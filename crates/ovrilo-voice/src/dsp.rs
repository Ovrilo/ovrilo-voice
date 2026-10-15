//! The DSP thread: owns devices' engine-side streams and the audio processor.
//!
//! - render: remote frames → mixer → processor (AEC reference) → playout
//! - capture: microphone → processor → transport
//!
//! This is not a real-time callback (those live in the backend); it may block briefly and sleeps
//! when idle. Mixing is paced by the playout buffer so latency does not grow with participants.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::audio::{
    CaptureStream, FRAME_SAMPLES, Frame, PlayoutStream, ProcessorFactory, SAMPLE_RATE,
};
use crate::contract::{NoiseSuppression, ParticipantId, VoiceEvent, VoiceStats};
use crate::mixer::Mixer;
use crate::speaking::SpeakingDetector;

/// Playout fill the mixer keeps; frames are mixed whenever the buffer drops below it.
const PLAYOUT_TARGET_MS: u32 = 40;
/// Remote frames queued beyond this are dropped (oldest first) to bound latency.
const MAX_REMOTE_QUEUE_FRAMES: usize = 10;
/// Push-to-talk keeps transmitting this long after the key is released (end of the last word).
const PTT_RELEASE_FRAMES: u32 = 20;
/// Playout + capture device buffering guess added to the AEC delay hint.
const DEVICE_LATENCY_GUESS_MS: u32 = 30;
const IDLE_SLEEP: Duration = Duration::from_millis(2);

pub(crate) enum DspCommand {
    SetCapture(Option<Box<dyn CaptureStream>>),
    SetPlayout(Option<Box<dyn PlayoutStream>>),
    SetMicrophoneSink(Option<mpsc::Sender<Frame>>),
    AddRemote(ParticipantId, mpsc::Receiver<Frame>),
    /// A watched screen share's audio (ADR-0017): mixed and echo-cancelled like voice, but never
    /// reported as speaking.
    AddStream(ParticipantId, mpsc::Receiver<Frame>),
    RemoveRemote(ParticipantId),
    /// Clears devices, remotes and the microphone sink (leaving a room).
    Reset,
    SetMuted(bool),
    SetDeafened(bool),
    SetPushToTalk(bool),
    SetPushToTalkPressed(bool),
    SetParticipantVolume(ParticipantId, u16),
    SetNoiseSuppression(NoiseSuppression),
    SetInputVolume(u16),
    SetOutputVolume(u16),
    /// Replies once every earlier command has been applied (commands are processed in order).
    Barrier(tokio::sync::oneshot::Sender<()>),
}

pub(crate) struct Dsp {
    commands: std_mpsc::Sender<DspCommand>,
    handle: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
}

/// Diagnostics written by the DSP thread and read by the control task (lock-free).
#[derive(Default)]
struct Counters {
    playout_buffered_ms: AtomicU32,
    playout_underruns: AtomicU64,
    playout_skipped_ms: AtomicU64,
    capture_overflow_samples: AtomicU64,
    remote_frames_dropped: AtomicU64,
    microphone_frames_dropped: AtomicU64,
    capture_processing_us: AtomicU32,
}

impl Counters {
    fn reset(&self) {
        for counter in [
            &self.playout_underruns,
            &self.playout_skipped_ms,
            &self.capture_overflow_samples,
            &self.remote_frames_dropped,
            &self.microphone_frames_dropped,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        self.playout_buffered_ms.store(0, Ordering::Relaxed);
    }
}

impl Dsp {
    pub(crate) fn spawn(
        processor: ProcessorFactory,
        events: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Self {
        let (commands, receiver) = std_mpsc::channel();
        let counters = Arc::new(Counters::default());
        let loop_counters = counters.clone();
        let handle = thread::Builder::new()
            .name("ovrilo-voice-dsp".into())
            .spawn(move || DspLoop::new(processor(), loop_counters, events).run(&receiver))
            .expect("spawn DSP thread");
        Self {
            commands,
            handle: Some(handle),
            counters,
        }
    }

    /// Engine-side diagnostics; network fields are filled in by the caller.
    pub(crate) fn stats(&self) -> VoiceStats {
        let c = &self.counters;
        VoiceStats {
            playout_buffered_ms: c.playout_buffered_ms.load(Ordering::Relaxed),
            playout_underruns: c.playout_underruns.load(Ordering::Relaxed),
            playout_skipped_ms: c.playout_skipped_ms.load(Ordering::Relaxed),
            capture_overflow_ms: c.capture_overflow_samples.load(Ordering::Relaxed) * 1000
                / u64::from(SAMPLE_RATE),
            remote_frames_dropped: c.remote_frames_dropped.load(Ordering::Relaxed),
            microphone_frames_dropped: c.microphone_frames_dropped.load(Ordering::Relaxed),
            capture_processing_us: c.capture_processing_us.load(Ordering::Relaxed),
            network: Default::default(),
        }
    }

    pub(crate) fn send(&self, command: DspCommand) {
        // The thread only exits when this handle is dropped, so a send failure means it panicked.
        let _ = self.commands.send(command);
    }

    /// Waits until the DSP thread has applied every command sent so far.
    pub(crate) async fn sync(&self) {
        let (reply, applied) = tokio::sync::oneshot::channel();
        self.send(DspCommand::Barrier(reply));
        let _ = applied.await;
    }
}

impl Drop for Dsp {
    fn drop(&mut self) {
        // Dropping the sender ends the loop; join so devices are released before returning.
        let (closed, _) = std_mpsc::channel();
        drop(std::mem::replace(&mut self.commands, closed));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct DspLoop {
    processor: Box<dyn crate::audio::AudioProcessor>,
    capture: Option<Box<dyn CaptureStream>>,
    playout: Option<Box<dyn PlayoutStream>>,
    microphone: Option<mpsc::Sender<Frame>>,
    remotes: Vec<Remote>,
    /// Per-participant playback volume (percent), applied when their audio starts.
    volumes: HashMap<ParticipantId, u16>,
    mixer: Mixer,
    muted: bool,
    deafened: bool,
    push_to_talk: bool,
    ptt_pressed: bool,
    /// Frames still transmitted after the push-to-talk key was released.
    ptt_tail: u32,
    counters: Arc<Counters>,
    /// Speaking indicators go straight to the host (unbounded send never blocks).
    events: mpsc::UnboundedSender<VoiceEvent>,
    local_speaking: SpeakingDetector,
    /// Last suppression mode reported to the host.
    noise_suppression: Option<NoiseSuppression>,
    /// Smoothed capture processing time per frame, in microseconds.
    processing_us: f32,
    /// Master volumes, percent (100 = unchanged).
    input_volume: u16,
    output_volume: u16,
}

struct Remote {
    id: ParticipantId,
    frames: mpsc::Receiver<Frame>,
    /// `None` for streams (screen-share audio), which are not speech.
    speaking: Option<SpeakingDetector>,
    volume: u16,
}

impl Remote {
    /// Updates speaking detection with a received frame; returns the transition, if any.
    fn detect_speaking(&mut self, audio: &Frame) -> Option<bool> {
        self.speaking
            .as_mut()
            .and_then(|detector| detector.update(audio))
    }
}

impl DspLoop {
    fn new(
        processor: Box<dyn crate::audio::AudioProcessor>,
        counters: Arc<Counters>,
        events: mpsc::UnboundedSender<VoiceEvent>,
    ) -> Self {
        // The starting mode is known to the host; only changes are reported.
        let noise_suppression = Some(processor.noise_suppression());
        Self {
            events,
            local_speaking: SpeakingDetector::default(),
            counters,
            capture: None,
            playout: None,
            microphone: None,
            remotes: Vec::new(),
            volumes: HashMap::new(),
            mixer: Mixer::default(),
            muted: false,
            deafened: false,
            push_to_talk: false,
            ptt_pressed: false,
            ptt_tail: 0,
            noise_suppression,
            processing_us: 0.0,
            input_volume: 100,
            output_volume: 100,
            processor,
        }
    }

    fn run(mut self, commands: &std_mpsc::Receiver<DspCommand>) {
        let mut frame: Frame = [0; FRAME_SAMPLES];
        loop {
            loop {
                match commands.try_recv() {
                    Ok(command) => self.apply(command),
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let rendered = self.render(&mut frame);
            let captured = self.capture(&mut frame);
            self.publish_device_counters();
            if !rendered && !captured {
                thread::sleep(IDLE_SLEEP);
            }
        }
    }

    fn apply(&mut self, command: DspCommand) {
        match command {
            DspCommand::SetCapture(capture) => self.capture = capture,
            DspCommand::SetPlayout(playout) => self.playout = playout,
            DspCommand::SetMicrophoneSink(sink) => self.microphone = sink,
            DspCommand::AddRemote(id, frames) => self.add_remote(id, frames, true),
            DspCommand::AddStream(id, frames) => self.add_remote(id, frames, false),
            DspCommand::RemoveRemote(id) => self.remove_remote(&id),
            DspCommand::Reset => {
                self.capture = None;
                self.playout = None;
                self.microphone = None;
                for remote in std::mem::take(&mut self.remotes) {
                    self.end_remote(remote);
                }
                self.silence_local();
                self.counters.reset();
            }
            DspCommand::SetMuted(muted) => {
                self.muted = muted;
                if muted {
                    self.silence_local();
                }
            }
            DspCommand::SetDeafened(deafened) => {
                self.deafened = deafened;
                if deafened {
                    self.silence_local();
                }
            }
            DspCommand::SetParticipantVolume(id, percent) => {
                if let Some(remote) = self.remotes.iter_mut().find(|r| r.id == id) {
                    remote.volume = percent;
                }
                if percent == 100 {
                    self.volumes.remove(&id);
                } else {
                    self.volumes.insert(id, percent);
                }
            }
            DspCommand::SetPushToTalk(enabled) => {
                self.push_to_talk = enabled;
                self.ptt_tail = 0;
            }
            DspCommand::SetPushToTalkPressed(pressed) => {
                if self.ptt_pressed && !pressed {
                    self.ptt_tail = PTT_RELEASE_FRAMES;
                }
                self.ptt_pressed = pressed;
            }
            DspCommand::SetInputVolume(percent) => self.input_volume = percent,
            DspCommand::SetOutputVolume(percent) => self.output_volume = percent,
            DspCommand::SetNoiseSuppression(mode) => {
                self.processor.set_noise_suppression(mode);
                // Always answer a request, even when the running mode did not change.
                self.noise_suppression = None;
                self.report_noise_suppression();
            }
            DspCommand::Barrier(reply) => {
                let _ = reply.send(());
            }
        }
    }

    /// Tells the host when the suppression actually running changes (requested or fallen back).
    fn report_noise_suppression(&mut self) {
        let active = self.processor.noise_suppression();
        if self.noise_suppression != Some(active) {
            self.noise_suppression = Some(active);
            let _ = self
                .events
                .send(VoiceEvent::NoiseSuppressionChanged(active));
        }
    }

    fn emit_speaking(&self, participant: Option<ParticipantId>, change: Option<bool>) {
        if let Some(speaking) = change {
            let _ = self.events.send(VoiceEvent::SpeakingChanged {
                participant,
                speaking,
            });
        }
    }

    fn silence_local(&mut self) {
        let change = self.local_speaking.silence();
        self.emit_speaking(None, change);
    }

    fn remove_remote(&mut self, id: &ParticipantId) {
        if let Some(index) = self.remotes.iter().position(|r| r.id == *id) {
            let remote = self.remotes.swap_remove(index);
            self.end_remote(remote);
        }
    }

    fn add_remote(&mut self, id: ParticipantId, frames: mpsc::Receiver<Frame>, speech: bool) {
        self.remove_remote(&id);
        let volume = self.volumes.get(&id).copied().unwrap_or(100);
        self.remotes.push(Remote {
            id,
            frames,
            speaking: speech.then(SpeakingDetector::default),
            volume,
        });
    }

    fn end_remote(&self, mut remote: Remote) {
        let change = remote.speaking.as_mut().and_then(SpeakingDetector::silence);
        self.emit_speaking(Some(remote.id), change);
    }

    /// Mixes one frame when playout needs audio. Returns whether work was done.
    fn render(&mut self, frame: &mut Frame) -> bool {
        // Bound per-remote latency and drop streams whose track ended.
        let mut index = 0;
        while index < self.remotes.len() {
            let remote = &mut self.remotes[index];
            while remote.frames.len() > MAX_REMOTE_QUEUE_FRAMES {
                if remote.frames.try_recv().is_ok() {
                    self.counters
                        .remote_frames_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            if remote.frames.is_closed() && remote.frames.is_empty() {
                let ended = self.remotes.swap_remove(index);
                self.end_remote(ended);
            } else {
                index += 1;
            }
        }

        let Some(playout) = self.playout.as_mut() else {
            // No speaker: keep remote queues drained so they do not grow (still tracking speech).
            let mut drained = false;
            for remote in &mut self.remotes {
                while let Ok(audio) = remote.frames.try_recv() {
                    drained = true;
                    if let Some(speaking) = remote.detect_speaking(&audio) {
                        let _ = self.events.send(VoiceEvent::SpeakingChanged {
                            participant: Some(remote.id.clone()),
                            speaking,
                        });
                    }
                }
            }
            return drained;
        };
        if playout.buffered_ms() >= PLAYOUT_TARGET_MS {
            return false;
        }

        for remote in &mut self.remotes {
            if let Ok(audio) = remote.frames.try_recv() {
                if let Some(speaking) = remote.detect_speaking(&audio) {
                    let _ = self.events.send(VoiceEvent::SpeakingChanged {
                        participant: Some(remote.id.clone()),
                        speaking,
                    });
                }
                // Speaking detection above used the received level; volume only changes playback.
                self.mixer.add_scaled(&audio, remote.volume);
            }
        }
        self.mixer.finish(frame);
        if self.deafened {
            frame.fill(0);
        } else {
            // Before the AEC reference, so the reference matches what the speaker plays.
            scale(frame, self.output_volume);
            self.processor.process_render(frame);
        }
        playout.write_frame(frame);
        true
    }

    /// Processes and forwards available microphone frames. Returns whether work was done.
    fn capture(&mut self, frame: &mut Frame) -> bool {
        let Some(capture) = self.capture.as_mut() else {
            return false;
        };
        let mut worked = false;
        while capture.read_frame(frame) {
            worked = true;
            let delay =
                self.playout.as_ref().map_or(0, |p| p.buffered_ms()) + DEVICE_LATENCY_GUESS_MS;
            let started = Instant::now();
            self.processor.process_capture(frame, delay);
            record_processing_time(&mut self.processing_us, &self.counters, started.elapsed());
            // After gain control, which would otherwise undo it.
            scale(frame, self.input_volume);
            // The processor still sees every frame (AEC/AGC state stays converged).
            let ptt_open = !self.push_to_talk || self.ptt_pressed || self.ptt_tail > 0;
            if !self.ptt_pressed {
                self.ptt_tail = self.ptt_tail.saturating_sub(1);
            }
            if !ptt_open {
                // Silence keeps the outgoing stream continuous (Opus DTX makes it nearly free).
                frame.fill(0);
            }
            let change = if self.muted || self.deafened || !ptt_open {
                self.local_speaking.silence()
            } else {
                self.local_speaking.update(frame)
            };
            if let Some(speaking) = change {
                let _ = self.events.send(VoiceEvent::SpeakingChanged {
                    participant: None,
                    speaking,
                });
            }
            if !self.muted
                && !self.deafened
                && let Some(microphone) = &self.microphone
            {
                // A full channel means the transport is behind; dropping one frame is the least-bad option.
                if microphone.try_send(*frame).is_err() {
                    self.counters
                        .microphone_frames_dropped
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if worked {
            self.report_noise_suppression();
        }
        worked
    }

    /// Copies the device streams' counters (cheap atomic reads) for the control task.
    fn publish_device_counters(&self) {
        let c = &self.counters;
        if let Some(playout) = &self.playout {
            c.playout_buffered_ms
                .store(playout.buffered_ms(), Ordering::Relaxed);
            c.playout_underruns
                .store(playout.underruns(), Ordering::Relaxed);
            c.playout_skipped_ms
                .store(playout.skipped_ms(), Ordering::Relaxed);
        }
        if let Some(capture) = &self.capture {
            c.capture_overflow_samples
                .store(capture.overflow_samples(), Ordering::Relaxed);
        }
    }
}

/// Smooths capture processing time over about a second of frames and publishes it.
fn record_processing_time(average_us: &mut f32, counters: &Counters, elapsed: Duration) {
    const SMOOTHING: f32 = 0.01;
    *average_us += (elapsed.as_micros() as f32 - *average_us) * SMOOTHING;
    counters
        .capture_processing_us
        .store(*average_us as u32, Ordering::Relaxed);
}

/// Applies a volume in percent (100 = unchanged), saturating instead of wrapping.
fn scale(frame: &mut Frame, percent: u16) {
    if percent == 100 {
        return;
    }
    let gain = i32::from(percent);
    for sample in frame.iter_mut() {
        *sample = (i32::from(*sample) * gain / 100).clamp(i32::from(i16::MIN), i32::from(i16::MAX))
            as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_scales_and_saturates() {
        let mut frame: Frame = [0; FRAME_SAMPLES];
        frame[0] = 1000;
        frame[1] = -20_000;
        frame[2] = 30_000;
        let original = frame;
        scale(&mut frame, 100);
        assert_eq!(frame, original);
        scale(&mut frame, 50);
        assert_eq!(&frame[..3], &[500, -10_000, 15_000]);
        scale(&mut frame, 200);
        scale(&mut frame, 200);
        assert_eq!(&frame[..3], &[2000, i16::MIN, i16::MAX]);
        scale(&mut frame, 0);
        assert!(frame.iter().all(|&s| s == 0));
    }
}
