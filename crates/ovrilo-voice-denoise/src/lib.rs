//! Enhanced noise suppression for the Ovrilo voice engine (ADR-0018).
//!
//! [`Denoiser`] runs DeepFilterNet3 on the engine's 10 ms, 48 kHz mono frames and closes a
//! [`VoiceGate`] when the model hears no speech, so keyboard clicks and room noise between words
//! are not sent at all. It runs on the DSP thread, never on a device callback. The model cannot move
//! between threads, so it is built where it runs; building takes a moment (see [`Denoiser::new`]).

mod gate;
mod overload;

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::Array2;
use ovrilo_voice::audio::{FRAME_SAMPLES, Frame};

pub use gate::VoiceGate;
pub use overload::OverloadDetector;

/// Noise attenuation limit in dB. 100 means unlimited: noise is removed, not just lowered.
const ATTENUATION_LIMIT_DB: f32 = 100.0;

/// DeepFilterNet3 noise suppression followed by a voice gate.
pub struct Denoiser {
    model: DfTract,
    input: Array2<f32>,
    output: Array2<f32>,
    gate: VoiceGate,
}

impl Denoiser {
    /// Builds the model (embedded in the binary). Slow compared with a frame: do it when switching
    /// modes, not per frame.
    pub fn new() -> Result<Self, String> {
        // No post filter: it removes a little more residual noise but makes distant or quiet
        // voices sound muffled; the gate already silences everything between words.
        let params = RuntimeParams::default_with_ch(1).with_atten_lim(ATTENUATION_LIMIT_DB);
        let model = DfTract::new(DfParams::default(), &params).map_err(|e| format!("{e:#}"))?;
        if model.hop_size != FRAME_SAMPLES || model.sr != ovrilo_voice::audio::SAMPLE_RATE as usize
        {
            return Err(format!(
                "model expects {} samples at {} Hz per frame",
                model.hop_size, model.sr
            ));
        }
        Ok(Self {
            model,
            input: Array2::zeros((1, FRAME_SAMPLES)),
            output: Array2::zeros((1, FRAME_SAMPLES)),
            gate: VoiceGate::default(),
        })
    }

    /// Removes noise from one frame in place and silences it while nobody speaks.
    ///
    /// The model looks ahead (see [`Denoiser::latency`]), so its output trails the input; its
    /// speech estimate for the newest input therefore opens the gate just before that audio comes
    /// out, and word onsets are not clipped.
    pub fn process(&mut self, frame: &mut Frame) {
        for (sample, value) in self.input.iter_mut().zip(frame.iter()) {
            *sample = f32::from(*value) / 32768.0;
        }
        let lsnr = match self
            .model
            .process(self.input.view(), self.output.view_mut())
        {
            Ok(lsnr) => lsnr,
            Err(_) => {
                // A failed frame is sent as is rather than dropped.
                return;
            }
        };
        for (value, sample) in frame.iter_mut().zip(self.output.iter()) {
            *value = (sample * 32768.0).clamp(-32768.0, 32767.0) as i16;
        }
        self.gate.process(frame, lsnr);
    }

    /// Delay the model adds: its analysis window beyond one frame plus its look-ahead frames.
    pub fn latency(&self) -> std::time::Duration {
        let samples =
            self.model.fft_size - self.model.hop_size + self.model.lookahead * self.model.hop_size;
        std::time::Duration::from_micros((samples as u64 * 1_000_000) / self.model.sr as u64)
    }

    /// Whether the gate currently lets audio through (speech detected).
    pub fn is_open(&self) -> bool {
        self.gate.is_open()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_model_loads() {
        if let Err(error) = Denoiser::new() {
            panic!("{error}");
        }
    }
}
