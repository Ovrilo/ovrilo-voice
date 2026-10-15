//! Sound of one application for screen sharing (ADR-0017): WASAPI process loopback of the shared
//! window's process tree (Windows 10 2004 / build 19041 or later). Ovrilo's own playout is a
//! different process, so voice chat never leaks into the stream. Delivers 10 ms frames of
//! interleaved stereo i16 at 48 kHz; a full channel drops frames instead of blocking capture.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use ovrilo_voice::VoiceError;
use tokio::sync::mpsc;

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const CHANNELS: u32 = 2;
/// Interleaved samples per 10 ms frame.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const FRAME_SAMPLES: usize = (SAMPLE_RATE / 100 * CHANNELS) as usize;
/// Frames buffered toward the network (200 ms).
const QUEUE_FRAMES: usize = 20;

/// Whether application sound can be captured on this platform.
pub(crate) const SUPPORTED: bool = cfg!(windows);

/// The process that owns a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Process {
    pub id: u32,
    /// Executable name, for example `chrome.exe`.
    pub name: String,
}

/// Owners of the top-level windows, by WebRTC window id (the window handle on Windows).
pub(crate) fn window_processes() -> HashMap<u64, Process> {
    platform::window_processes()
}

/// A running capture. Dropping it stops capturing.
pub(crate) struct AppAudio {
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

/// Starts capturing the sound of `process_id` and its child processes (browsers and many games
/// play sound from a child process).
pub(crate) fn start(process_id: u32) -> Result<(AppAudio, mpsc::Receiver<Vec<i16>>), VoiceError> {
    let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("ovrilo-app-audio".into())
            .spawn(move || platform::capture(process_id, tx, &stop, ready_tx))
            .map_err(|e| VoiceError::capture(format!("app audio thread: {e}")))?
    };
    // Surface activation errors (unsupported Windows version, process gone) to the caller.
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((
            AppAudio {
                stop,
                thread: Some(thread),
            },
            rx,
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(VoiceError::capture(
                "app audio thread exited before starting",
            ))
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicBool, Ordering};

    use ovrilo_voice::VoiceError;
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    use tokio::sync::mpsc;
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

    use super::{CHANNELS, FRAME_SAMPLES, Process, SAMPLE_RATE};

    pub fn window_processes() -> HashMap<u64, Process> {
        let mut owners = HashMap::new();
        let _ = winsafe::EnumWindows(|window: winsafe::HWND| {
            let (_, process_id) = window.GetWindowThreadProcessId();
            owners.insert(window.ptr() as usize as u64, process_id);
            true
        });
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        owners
            .into_iter()
            .map(|(window, id)| {
                let name = system
                    .process(sysinfo::Pid::from_u32(id))
                    .map(|p| p.name().to_string_lossy().into_owned())
                    .unwrap_or_default();
                (window, Process { id, name })
            })
            .collect()
    }

    pub fn capture(
        process_id: u32,
        tx: mpsc::Sender<Vec<i16>>,
        stop: &AtomicBool,
        ready: std::sync::mpsc::Sender<Result<(), VoiceError>>,
    ) {
        let started = (|| {
            initialize_mta()
                .ok()
                .map_err(|e| VoiceError::capture(format!("COM init: {e}")))?;
            let format = WaveFormat::new(
                32,
                32,
                &SampleType::Float,
                SAMPLE_RATE as usize,
                CHANNELS as usize,
                None,
            );
            let mut client = AudioClient::new_application_loopback_client(process_id, true)
                .map_err(|e| {
                    VoiceError::capture(format!(
                        "application sound needs Windows 10 version 2004 or later: {e}"
                    ))
                })?;
            client
                .initialize_client(
                    &format,
                    &Direction::Capture,
                    &StreamMode::EventsShared {
                        autoconvert: true,
                        buffer_duration_hns: 0,
                    },
                )
                .map_err(|e| VoiceError::capture(format!("application sound: {e}")))?;
            let event = client
                .set_get_eventhandle()
                .map_err(|e| VoiceError::capture(format!("application sound: {e}")))?;
            let capture = client
                .get_audiocaptureclient()
                .map_err(|e| VoiceError::capture(format!("application sound: {e}")))?;
            client
                .start_stream()
                .map_err(|e| VoiceError::capture(format!("application sound: {e}")))?;
            Ok::<_, VoiceError>((client, event, capture))
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
                        if capture.read_from_device_to_deque(&mut bytes).is_err() {
                            return;
                        }
                    }
                    Ok(_) => break,
                    Err(_) => return,
                }
            }
            while bytes.len() >= bytes_per_frame {
                let samples: Vec<i16> = bytes
                    .drain(..bytes_per_frame)
                    .collect::<Vec<u8>>()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| (f32::from_le_bytes(*b).clamp(-1.0, 1.0) * 32767.0) as i16)
                    .collect();
                // A full queue means the network side is behind: drop this frame.
                if tx.try_send(samples).is_err() && tx.is_closed() {
                    return;
                }
            }
        }
        let _ = client.stop_stream();
    }
}

#[cfg(not(windows))]
mod platform {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    use ovrilo_voice::VoiceError;
    use tokio::sync::mpsc;

    use super::Process;

    pub fn window_processes() -> HashMap<u64, Process> {
        HashMap::new()
    }

    pub fn capture(
        _process_id: u32,
        _tx: mpsc::Sender<Vec<i16>>,
        _stop: &AtomicBool,
        ready: std::sync::mpsc::Sender<Result<(), VoiceError>>,
    ) {
        let _ = ready.send(Err(VoiceError::capture(
            "application sound can be shared only on Windows 10 version 2004 or later",
        )));
    }
}
