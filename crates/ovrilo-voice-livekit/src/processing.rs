//! Microphone and speaker processing on the DSP thread: WebRTC's Audio Processing Module for echo
//! cancellation, gain control and Standard noise suppression, and DeepFilterNet3 with a voice gate
//! for Enhanced suppression (ADR-0018).
//!
//! Capture order:
//! - Standard / Off: echo cancellation → (WebRTC suppression) → gain control.
//! - Enhanced: echo cancellation → DeepFilterNet3 → gate → gain control. Gain control runs after
//!   the noise is gone, so a quiet or distant voice is raised instead of the room around it.
//!
//! Only one suppressor runs at a time.

use std::time::Instant;

use livekit::webrtc::native::apm::AudioProcessingModule;
use ovrilo_voice::audio::SAMPLE_RATE;
use ovrilo_voice::{AudioProcessor, Frame, NoiseSuppression, ProcessorFactory};
use ovrilo_voice_denoise::{Denoiser, OverloadDetector};

/// Which processing stages to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessingConfig {
    pub echo_cancellation: bool,
    pub gain_control: bool,
    pub high_pass: bool,
    /// Starting mode; hosts change it with `VoiceCommand::SetNoiseSuppression`.
    pub noise_suppression: NoiseSuppression,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            gain_control: true,
            high_pass: true,
            noise_suppression: NoiseSuppression::Standard,
        }
    }
}

/// Factory for the engine: creates the processors on the DSP thread (neither moves between threads).
pub fn webrtc_processing(config: ProcessingConfig) -> ProcessorFactory {
    Box::new(move || {
        let mut processor = WebRtcProcessor {
            config,
            layout: NoiseSuppression::Standard,
            apm: build_apm(&config, NoiseSuppression::Standard),
            gain: None,
            requested: NoiseSuppression::Standard,
            enhanced: Enhanced::Unloaded,
        };
        processor.set_noise_suppression(config.noise_suppression);
        Box::new(processor)
    })
}

/// The APM for a layout: in Enhanced it does echo cancellation (and high-pass) only, because noise
/// removal and gain control follow DeepFilterNet3.
fn build_apm(config: &ProcessingConfig, layout: NoiseSuppression) -> AudioProcessingModule {
    let enhanced = layout == NoiseSuppression::Enhanced;
    AudioProcessingModule::new(
        config.echo_cancellation,
        config.gain_control && !enhanced,
        config.high_pass,
        layout == NoiseSuppression::Standard,
    )
}

enum Enhanced {
    /// Not built yet (built on first request; kept afterwards so switching back is instant).
    Unloaded,
    Ready(Box<Denoiser>, OverloadDetector),
    /// Could not be built, or too slow on this computer. Not retried until requested again.
    Unavailable,
}

struct WebRtcProcessor {
    config: ProcessingConfig,
    /// What the processing chain is currently built for.
    layout: NoiseSuppression,
    apm: AudioProcessingModule,
    /// Gain control after DeepFilterNet3 (Enhanced layout only).
    gain: Option<AudioProcessingModule>,
    requested: NoiseSuppression,
    enhanced: Enhanced,
}

impl WebRtcProcessor {
    fn enhanced_ready(&self) -> bool {
        matches!(self.enhanced, Enhanced::Ready(..))
    }

    /// Rebuilds the chain for a layout. Echo cancellation re-converges within a second; this only
    /// happens when the mode changes.
    fn build(&mut self, layout: NoiseSuppression) {
        if layout == self.layout {
            return;
        }
        self.apm = build_apm(&self.config, layout);
        self.gain = (layout == NoiseSuppression::Enhanced && self.config.gain_control)
            .then(|| AudioProcessingModule::new(false, true, false, false));
        self.layout = layout;
    }
}

impl AudioProcessor for WebRtcProcessor {
    fn process_render(&mut self, frame: &mut Frame) {
        let _ = self
            .apm
            .process_reverse_stream(frame, SAMPLE_RATE as i32, 1);
    }

    fn process_capture(&mut self, frame: &mut Frame, playout_delay_ms: u32) {
        if self.config.echo_cancellation {
            let _ = self.apm.set_stream_delay_ms(playout_delay_ms as i32);
        }
        let _ = self.apm.process_stream(frame, SAMPLE_RATE as i32, 1);

        if self.layout != NoiseSuppression::Enhanced {
            return;
        }
        let Enhanced::Ready(denoiser, load) = &mut self.enhanced else {
            return;
        };
        let started = Instant::now();
        denoiser.process(frame);
        let overloaded = load.record(started.elapsed());
        if let Some(gain) = &mut self.gain {
            let _ = gain.process_stream(frame, SAMPLE_RATE as i32, 1);
        }
        if overloaded {
            // This computer cannot run it in real time: fall back to WebRTC suppression for the
            // rest of the session (the host is told through NoiseSuppressionChanged).
            self.enhanced = Enhanced::Unavailable;
            self.build(NoiseSuppression::Standard);
        }
    }

    fn set_noise_suppression(&mut self, mode: NoiseSuppression) {
        self.requested = mode;
        if mode == NoiseSuppression::Enhanced {
            if matches!(self.enhanced, Enhanced::Unavailable) {
                // Asked again: give it another chance (for example after closing a heavy game).
                self.enhanced = Enhanced::Unloaded;
            }
            if matches!(self.enhanced, Enhanced::Unloaded) {
                // Takes a few hundred milliseconds once; hosts choose the mode before joining.
                self.enhanced = match Denoiser::new() {
                    Ok(denoiser) => {
                        Enhanced::Ready(Box::new(denoiser), OverloadDetector::default())
                    }
                    Err(_) => Enhanced::Unavailable,
                };
            }
        }
        let layout = match mode {
            NoiseSuppression::Enhanced if self.enhanced_ready() => NoiseSuppression::Enhanced,
            NoiseSuppression::Off => NoiseSuppression::Off,
            _ => NoiseSuppression::Standard,
        };
        self.build(layout);
    }

    fn noise_suppression(&self) -> NoiseSuppression {
        self.layout
    }
}

#[cfg(test)]
mod tests {
    use ovrilo_voice::audio::FRAME_SAMPLES;

    use super::*;

    fn processor(mode: NoiseSuppression) -> Box<dyn AudioProcessor> {
        webrtc_processing(ProcessingConfig {
            noise_suppression: mode,
            ..ProcessingConfig::default()
        })()
    }

    #[test]
    fn each_mode_runs_exactly_one_suppressor() {
        let mut p = processor(NoiseSuppression::Standard);
        assert_eq!(p.noise_suppression(), NoiseSuppression::Standard);
        p.set_noise_suppression(NoiseSuppression::Enhanced);
        assert_eq!(p.noise_suppression(), NoiseSuppression::Enhanced);
        p.set_noise_suppression(NoiseSuppression::Off);
        assert_eq!(p.noise_suppression(), NoiseSuppression::Off);
        // The model stays loaded, so switching back is immediate.
        p.set_noise_suppression(NoiseSuppression::Enhanced);
        assert_eq!(p.noise_suppression(), NoiseSuppression::Enhanced);
    }

    #[test]
    fn enhanced_sends_silence_when_there_is_only_noise() {
        let mut p = processor(NoiseSuppression::Enhanced);
        let mut seed = 7u32;
        let mut sent_energy = 0u64;
        for n in 0..300 {
            let mut frame: Frame = [0; FRAME_SAMPLES];
            for sample in frame.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *sample = ((seed >> 16) as i16) / 8;
            }
            p.process_capture(&mut frame, 40);
            // Judge after the first second (filters and gate settle).
            if n >= 100 {
                sent_energy += frame
                    .iter()
                    .map(|&s| (s as i64).unsigned_abs())
                    .sum::<u64>();
            }
        }
        assert_eq!(sent_energy, 0, "noise leaked through");
    }
}
