//! Sound of one application (and its child processes) via WASAPI process loopback (ADR-0017).
//!
//! Windows 10 2004 (build 19041) or later. Ovrilo's own playout is a different process, so voice
//! chat never leaks into the shared stream. Delivers 10 ms frames of interleaved stereo i16 at
//! 48 kHz on a channel; a full channel drops frames (counted) instead of blocking capture.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// Interleaved samples per 10 ms frame.
#[cfg_attr(not(windows), allow(dead_code))]
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE / 100 * CHANNELS) as usize;

pub struct AppAudio {
    pub process_id: u32,
    pub dropped_frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for AppAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Finds the root process of `exe_name` (for example `chrome.exe`) and starts capturing its sound.
pub fn start(exe_name: &str) -> Result<(AppAudio, mpsc::Receiver<Vec<i16>>), String> {
    let process_id = root_process_id(exe_name)?;
    let (tx, rx) = mpsc::channel(50);
    let stop = Arc::new(AtomicBool::new(false));
    let dropped_frames = Arc::new(AtomicU64::new(0));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = {
        let stop = stop.clone();
        let dropped = dropped_frames.clone();
        std::thread::Builder::new()
            .name("app-audio".into())
            .spawn(move || platform::capture(process_id, tx, stop, dropped, ready_tx))
            .map_err(|e| format!("app audio thread: {e}"))?
    };
    // Surface activation errors (unsupported Windows version, process gone) to the caller.
    ready_rx
        .recv()
        .map_err(|_| "app audio thread exited before starting".to_owned())??;
    Ok((
        AppAudio {
            process_id,
            dropped_frames,
            stop,
            thread: Some(thread),
        },
        rx,
    ))
}

/// The process with this executable name whose parent is not the same executable: capturing its
/// tree includes helper processes (browsers and many games play sound from a child process).
fn root_process_id(exe_name: &str) -> Result<u32, String> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let matches: Vec<_> = system
        .processes()
        .values()
        .filter(|p| p.name().eq_ignore_ascii_case(exe_name))
        .collect();
    let is_match = |pid| matches.iter().any(|p| p.pid() == pid);
    matches
        .iter()
        .find(|p| p.parent().is_none_or(|parent| !is_match(parent)))
        .map(|p| p.pid().as_u32())
        .ok_or_else(|| format!("no running process named '{exe_name}'"))
}

#[cfg(windows)]
mod platform {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use tokio::sync::mpsc;
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

    use super::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};

    pub fn capture(
        process_id: u32,
        tx: mpsc::Sender<Vec<i16>>,
        stop: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
        ready: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let started = (|| {
            initialize_mta()
                .ok()
                .map_err(|e| format!("COM init: {e}"))?;
            let format = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                SAMPLE_RATE as usize,
                CHANNELS as usize,
                None,
            );
            let mut client = AudioClient::new_application_loopback_client(process_id, true)
                .map_err(|e| format!("process loopback (needs Windows 10 2004+): {e}"))?;
            client
                .initialize_client(
                    &format,
                    &Direction::Capture,
                    &StreamMode::EventsShared {
                        autoconvert: true,
                        buffer_duration_hns: 0,
                    },
                )
                .map_err(|e| format!("initialize loopback client: {e}"))?;
            let event = client
                .set_get_eventhandle()
                .map_err(|e| format!("event handle: {e}"))?;
            let capture = client
                .get_audiocaptureclient()
                .map_err(|e| format!("capture client: {e}"))?;
            client
                .start_stream()
                .map_err(|e| format!("start loopback: {e}"))?;
            Ok::<_, String>((client, event, capture))
        })();
        let (client, event, capture) = match started {
            Ok(parts) => {
                let _ = ready.send(Ok(()));
                parts
            }
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };

        let bytes_per_frame = FRAME_SAMPLES * 4;
        let mut bytes = VecDeque::<u8>::new();
        while !stop.load(Ordering::Relaxed) {
            // No events arrive while the application is silent; just check `stop` again.
            if event.wait_for_event(200).is_err() {
                continue;
            }
            loop {
                match capture.get_next_packet_size() {
                    Ok(Some(frames)) if frames > 0 => {
                        if let Err(error) = capture.read_from_device_to_deque(&mut bytes) {
                            eprintln!("app audio read failed: {error}");
                            return;
                        }
                    }
                    Ok(_) => break,
                    Err(error) => {
                        eprintln!("app audio packet size failed: {error}");
                        return;
                    }
                }
            }
            while bytes.len() >= bytes_per_frame {
                let samples: Vec<i16> = bytes
                    .drain(..bytes_per_frame)
                    .collect::<Vec<u8>>()
                    .chunks_exact(4)
                    .map(|b| {
                        (f32::from_le_bytes([b[0], b[1], b[2], b[3]]).clamp(-1.0, 1.0) * 32767.0)
                            as i16
                    })
                    .collect();
                if tx.try_send(samples).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let _ = client.stop_stream();
    }
}

#[cfg(not(windows))]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use tokio::sync::mpsc;

    pub fn capture(
        _process_id: u32,
        _tx: mpsc::Sender<Vec<i16>>,
        _stop: Arc<AtomicBool>,
        _dropped: Arc<AtomicU64>,
        ready: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let _ = ready.send(Err(
            "application audio capture needs Windows 10 2004 or later".into(),
        ));
    }
}
