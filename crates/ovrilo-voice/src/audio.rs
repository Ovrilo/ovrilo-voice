//! Audio device and processing boundaries (ADR-0016).
//!
//! The engine works in one internal format: 48 kHz, mono, signed 16-bit, 10 ms frames.
//! Backends convert device formats to and from it; transports convert network formats.

use crate::contract::{AudioDevice, DeviceId, NoiseSuppression, VoiceError};

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE / 100) as usize;

/// One 10 ms mono frame in the internal format.
pub type Frame = [i16; FRAME_SAMPLES];

/// Platform audio devices. Implementations keep real-time callbacks lock- and allocation-free and
/// exchange audio with the engine through the returned streams.
pub trait AudioBackend: Send + 'static {
    fn input_devices(&self) -> Result<Vec<AudioDevice>, VoiceError>;
    fn output_devices(&self) -> Result<Vec<AudioDevice>, VoiceError>;
    /// Opens a microphone (`None` = default) and starts capturing.
    fn open_input(
        &mut self,
        device: Option<&DeviceId>,
    ) -> Result<Box<dyn CaptureStream>, VoiceError>;
    /// Opens a speaker (`None` = default) and starts playout (silence until frames are written).
    fn open_output(
        &mut self,
        device: Option<&DeviceId>,
    ) -> Result<Box<dyn PlayoutStream>, VoiceError>;
}

/// A running capture. Dropping it stops the device.
pub trait CaptureStream: Send {
    /// Copies the next captured frame into `out`; false when a full frame is not available yet.
    fn read_frame(&mut self, out: &mut Frame) -> bool;
    /// Samples lost because the engine did not drain capture in time.
    fn overflow_samples(&self) -> u64 {
        0
    }
}

/// A running playout. Dropping it stops the device.
pub trait PlayoutStream: Send {
    /// Queues one frame for playout. Must not block.
    fn write_frame(&mut self, frame: &Frame);
    /// Audio queued but not yet handed to the device, in milliseconds.
    fn buffered_ms(&self) -> u32;
    /// Playout callbacks that ran out of audio after playout started.
    fn underruns(&self) -> u64 {
        0
    }
    /// Audio dropped by the device side to bound latency, in milliseconds.
    fn skipped_ms(&self) -> u64 {
        0
    }
}

/// Echo cancellation / gain control / noise suppression applied on the DSP thread.
pub trait AudioProcessor {
    /// Far-end audio about to be played (AEC reference). Called before the matching capture frames.
    fn process_render(&mut self, frame: &mut Frame);
    /// Near-end microphone audio to be sent.
    fn process_capture(&mut self, frame: &mut Frame, playout_delay_ms: u32);
    /// Requests a noise suppression mode. Processors without suppression ignore it.
    fn set_noise_suppression(&mut self, _mode: NoiseSuppression) {}
    /// The suppression actually running, which can differ from the request (unavailable, or
    /// fallen back because this computer cannot keep up).
    fn noise_suppression(&self) -> NoiseSuppression {
        NoiseSuppression::Off
    }
}

/// Creates the processor on the DSP thread (processors such as WebRTC's APM are not `Send`).
pub type ProcessorFactory = Box<dyn FnOnce() -> Box<dyn AudioProcessor> + Send>;

/// Pass-through processor used when processing is disabled and in tests.
#[derive(Debug, Default)]
pub struct NoProcessing;

impl AudioProcessor for NoProcessing {
    fn process_render(&mut self, _frame: &mut Frame) {}

    fn process_capture(&mut self, _frame: &mut Frame, _playout_delay_ms: u32) {}
}
