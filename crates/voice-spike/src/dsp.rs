//! Audio processing thread. Owns the WebRTC Audio Processing Module (not Send), so both directions
//! run here:
//!
//! - render: remote 10 ms frames → APM reverse stream (AEC reference) → playout ring buffer
//! - capture: microphone ring buffer → APM process stream → frames to the LiveKit sender task
//!
//! This is not a real-time callback thread; it may allocate and sleep. Real-time callbacks stay in
//! `cpal_capture` / `cpal_playback`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use livekit::webrtc::native::apm::AudioProcessingModule;

use crate::cpal_capture::CaptureFeed;
use crate::cpal_playback::PlaybackFeed;

/// Rough WASAPI shared-mode output + input buffering, used only as an AEC delay hint.
const DEVICE_LATENCY_GUESS_MS: u32 = 30;

#[derive(Debug, Clone, Copy)]
pub struct Processing {
    pub echo_cancellation: bool,
    pub gain_control: bool,
    pub high_pass: bool,
    pub noise_suppression: bool,
}

impl Processing {
    pub fn any(self) -> bool {
        self.echo_cancellation || self.gain_control || self.high_pass || self.noise_suppression
    }
}

pub struct Dsp {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<DspStats>>,
}

#[derive(Debug, Default)]
pub struct DspStats {
    pub capture_frames: u64,
    pub render_frames: u64,
    pub apm_errors: u64,
    pub playout_overflow_samples: u64,
    pub send_dropped_frames: u64,
    /// Mean playout ring-buffer fill while capturing, in ms (part of the echo path delay).
    pub mean_playout_buffer_ms: f64,
}

impl Dsp {
    /// Starts the thread. `remote_frames` delivers mono render frames at the playout rate;
    /// processed capture frames are sent on `processed_tx`.
    pub fn spawn(
        processing: Processing,
        mut capture: Option<CaptureFeed>,
        mut playback: Option<PlaybackFeed>,
        remote_frames: mpsc::Receiver<Vec<i16>>,
        processed_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let handle = thread::Builder::new()
            .name("ovrilo-dsp".into())
            .spawn(move || {
                let mut apm = processing.any().then(|| {
                    AudioProcessingModule::new(
                        processing.echo_cancellation,
                        processing.gain_control,
                        processing.high_pass,
                        processing.noise_suppression,
                    )
                });
                let mut stats = DspStats::default();
                let capture_frame_len = capture
                    .as_ref()
                    .map_or(0, |c| (c.sample_rate / 100) as usize);
                let mut capture_buffer = vec![0i16; capture_frame_len];
                let mut buffer_ms_sum = 0u64;

                while !stop_flag.load(Ordering::Relaxed) {
                    let mut worked = false;

                    // Render first, so the AEC sees far-end audio before the near-end that contains its echo.
                    while let Ok(mut frame) = remote_frames.try_recv() {
                        worked = true;
                        stats.render_frames += 1;
                        if let Some(playback) = playback.as_mut() {
                            let rate = playback.sample_rate;
                            if let Some(apm) = apm.as_mut()
                                && frame.len() % (rate as usize / 100) == 0
                                && apm
                                    .process_reverse_stream(&mut frame, rate as i32, 1)
                                    .is_err()
                            {
                                stats.apm_errors += 1;
                            }
                            playback.push(&frame);
                        }
                    }

                    if let Some(capture) = capture.as_mut() {
                        while capture.read_frame(&mut capture_buffer) {
                            worked = true;
                            stats.capture_frames += 1;
                            let buffered_ms =
                                playback.as_ref().map_or(0, PlaybackFeed::buffered_ms);
                            buffer_ms_sum += u64::from(buffered_ms);
                            if let Some(apm) = apm.as_mut()
                                && processing.echo_cancellation
                            {
                                // Delay hint for AEC3: queued playout plus a device-latency guess.
                                let _ = apm.set_stream_delay_ms(
                                    (buffered_ms + DEVICE_LATENCY_GUESS_MS) as i32,
                                );
                            }
                            if let Some(apm) = apm.as_mut()
                                && apm
                                    .process_stream(
                                        &mut capture_buffer,
                                        capture.sample_rate as i32,
                                        1,
                                    )
                                    .is_err()
                            {
                                stats.apm_errors += 1;
                            }
                            if processed_tx.try_send(capture_buffer.clone()).is_err() {
                                stats.send_dropped_frames += 1;
                            }
                        }
                    }

                    if !worked {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                stats.playout_overflow_samples = playback.map_or(0, |p| p.overflow_samples);
                if stats.capture_frames > 0 {
                    stats.mean_playout_buffer_ms =
                        buffer_ms_sum as f64 / stats.capture_frames as f64;
                }
                stats
            })
            .expect("spawn dsp thread");
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) -> DspStats {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}
