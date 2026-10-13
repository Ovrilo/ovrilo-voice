//! The DSP thread: owns devices' engine-side streams and the audio processor.
//!
//! - render: remote frames → mixer → processor (AEC reference) → playout
//! - capture: microphone → processor → transport
//!
//! This is not a real-time callback (those live in the backend); it may block briefly and sleeps
//! when idle. Mixing is paced by the playout buffer so latency does not grow with participants.

use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::audio::{CaptureStream, FRAME_SAMPLES, Frame, PlayoutStream, ProcessorFactory};
use crate::contract::ParticipantId;
use crate::mixer::Mixer;

/// Playout fill the mixer keeps; frames are mixed whenever the buffer drops below it.
const PLAYOUT_TARGET_MS: u32 = 40;
/// Remote frames queued beyond this are dropped (oldest first) to bound latency.
const MAX_REMOTE_QUEUE_FRAMES: usize = 10;
/// Playout + capture device buffering guess added to the AEC delay hint.
const DEVICE_LATENCY_GUESS_MS: u32 = 30;
const IDLE_SLEEP: Duration = Duration::from_millis(2);

pub(crate) enum DspCommand {
    SetCapture(Option<Box<dyn CaptureStream>>),
    SetPlayout(Option<Box<dyn PlayoutStream>>),
    SetMicrophoneSink(Option<mpsc::Sender<Frame>>),
    AddRemote(ParticipantId, mpsc::Receiver<Frame>),
    RemoveRemote(ParticipantId),
    /// Clears devices, remotes and the microphone sink (leaving a room).
    Reset,
    SetMuted(bool),
    SetDeafened(bool),
    /// Replies once every earlier command has been applied (commands are processed in order).
    Barrier(tokio::sync::oneshot::Sender<()>),
}

pub(crate) struct Dsp {
    commands: std_mpsc::Sender<DspCommand>,
    handle: Option<JoinHandle<()>>,
}

impl Dsp {
    pub(crate) fn spawn(processor: ProcessorFactory) -> Self {
        let (commands, receiver) = std_mpsc::channel();
        let handle = thread::Builder::new()
            .name("ovrilo-voice-dsp".into())
            .spawn(move || DspLoop::new(processor()).run(&receiver))
            .expect("spawn DSP thread");
        Self {
            commands,
            handle: Some(handle),
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
    remotes: Vec<(ParticipantId, mpsc::Receiver<Frame>)>,
    mixer: Mixer,
    muted: bool,
    deafened: bool,
}

impl DspLoop {
    fn new(processor: Box<dyn crate::audio::AudioProcessor>) -> Self {
        Self {
            processor,
            capture: None,
            playout: None,
            microphone: None,
            remotes: Vec::new(),
            mixer: Mixer::default(),
            muted: false,
            deafened: false,
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
            DspCommand::AddRemote(id, frames) => {
                self.remotes.retain(|(existing, _)| *existing != id);
                self.remotes.push((id, frames));
            }
            DspCommand::RemoveRemote(id) => self.remotes.retain(|(existing, _)| *existing != id),
            DspCommand::Reset => {
                self.capture = None;
                self.playout = None;
                self.microphone = None;
                self.remotes.clear();
            }
            DspCommand::SetMuted(muted) => self.muted = muted,
            DspCommand::SetDeafened(deafened) => self.deafened = deafened,
            DspCommand::Barrier(reply) => {
                let _ = reply.send(());
            }
        }
    }

    /// Mixes one frame when playout needs audio. Returns whether work was done.
    fn render(&mut self, frame: &mut Frame) -> bool {
        // Bound per-remote latency and drop streams whose track ended.
        self.remotes.retain_mut(|(_, frames)| {
            while frames.len() > MAX_REMOTE_QUEUE_FRAMES {
                let _ = frames.try_recv();
            }
            !frames.is_closed() || !frames.is_empty()
        });

        let Some(playout) = self.playout.as_mut() else {
            // No speaker: keep remote queues drained so they do not grow.
            let mut drained = false;
            for (_, frames) in &mut self.remotes {
                while frames.try_recv().is_ok() {
                    drained = true;
                }
            }
            return drained;
        };
        if playout.buffered_ms() >= PLAYOUT_TARGET_MS {
            return false;
        }

        for (_, frames) in &mut self.remotes {
            if let Ok(remote) = frames.try_recv() {
                self.mixer.add(&remote);
            }
        }
        self.mixer.finish(frame);
        if self.deafened {
            frame.fill(0);
        } else {
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
            self.processor.process_capture(frame, delay);
            if !self.muted
                && !self.deafened
                && let Some(microphone) = &self.microphone
            {
                // A full channel means the transport is behind; dropping one frame is the least-bad option.
                let _ = microphone.try_send(*frame);
            }
        }
        worked
    }
}
