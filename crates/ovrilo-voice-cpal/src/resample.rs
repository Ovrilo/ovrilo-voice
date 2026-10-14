//! Sample-rate conversion between a device's rate and the engine's 48 kHz.
//!
//! Runs on the engine's DSP thread (inside `read_frame`/`write_frame`), never in cpal callbacks.
//! All buffers are allocated up front; processing does not allocate.

use ovrilo_voice::VoiceError;
use ovrilo_voice::audio::FRAME_SAMPLES;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Streaming mono converter: accepts any number of samples and emits converted blocks.
pub(crate) struct RateConverter {
    resampler: Fft<f32>,
    input: Vec<f32>,
    filled: usize,
    output: Vec<f32>,
    output_rate: u32,
}

impl RateConverter {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self, VoiceError> {
        let resampler = Fft::<f32>::new(
            input_rate as usize,
            output_rate as usize,
            FRAME_SAMPLES,
            1,
            FixedSync::Input,
        )
        .map_err(|e| {
            VoiceError::device(format!(
                "cannot convert {input_rate} Hz to {output_rate} Hz: {e}"
            ))
        })?;
        Ok(Self {
            input: vec![0.0; resampler.input_frames_max()],
            output: vec![0.0; resampler.output_frames_max()],
            filled: 0,
            resampler,
            output_rate,
        })
    }

    /// Most samples one input block can produce.
    pub fn max_block_output(&self) -> usize {
        self.output.len()
    }

    /// Input samples still needed before the next block is converted.
    pub fn needed(&self) -> usize {
        self.resampler.input_frames_next() - self.filled
    }

    /// Converter latency in milliseconds.
    pub fn delay_ms(&self) -> u32 {
        (self.resampler.output_delay() as u64 * 1000 / u64::from(self.output_rate)) as u32
    }

    /// Feeds samples; `sink` receives each converted block as soon as one is complete.
    pub fn push(&mut self, samples: impl IntoIterator<Item = f32>, mut sink: impl FnMut(&[f32])) {
        for sample in samples {
            self.input[self.filled] = sample;
            self.filled += 1;
            let block = self.resampler.input_frames_next();
            if self.filled < block {
                continue;
            }
            self.filled = 0;
            let produced = {
                let out_len = self.resampler.output_frames_next();
                let (Ok(input), Ok(mut output)) = (
                    InterleavedSlice::new(&self.input[..block], 1, block),
                    InterleavedSlice::new_mut(&mut self.output[..out_len], 1, out_len),
                ) else {
                    continue;
                };
                match self
                    .resampler
                    .process_into_buffer(&input, &mut output, None)
                {
                    Ok((_, produced)) => produced,
                    Err(_) => continue,
                }
            };
            sink(&self.output[..produced]);
        }
    }
}

pub(crate) fn to_f32(sample: i16) -> f32 {
    f32::from(sample) / f32::from(i16::MAX)
}

pub(crate) fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, seconds: f32) -> Vec<f32> {
        (0..(rate as f32 * seconds) as usize)
            .map(|i| (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Frequency estimate from zero crossings, skipping the converter's start-up delay.
    fn frequency(samples: &[f32], rate: u32) -> f32 {
        let tail = &samples[samples.len() / 4..];
        let crossings = tail
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        crossings as f32 * rate as f32 / tail.len() as f32
    }

    fn convert(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut converter = RateConverter::new(from, to).unwrap();
        let mut out = Vec::new();
        // Uneven pushes, as from a device ring buffer.
        for chunk in input.chunks(333) {
            converter.push(chunk.iter().copied(), |block| out.extend_from_slice(block));
        }
        out
    }

    #[test]
    fn converts_common_device_rates_preserving_pitch_and_duration() {
        for rate in [44_100, 16_000, 96_000] {
            for (from, to) in [(rate, 48_000), (48_000, rate)] {
                let input = tone(from, 440.0, 1.0);
                let output = convert(from, to, &input);

                let expected = input.len() as f32 * to as f32 / from as f32;
                assert!(
                    (output.len() as f32 - expected).abs() < 2.0 * FRAME_SAMPLES as f32 * 2.0,
                    "{from}->{to}: {} samples, expected ~{expected}",
                    output.len()
                );
                let hz = frequency(&output, to);
                assert!((hz - 440.0).abs() < 5.0, "{from}->{to}: {hz} Hz");
                let peak = output.iter().fold(0f32, |m, s| m.max(s.abs()));
                assert!((0.4..0.6).contains(&peak), "{from}->{to}: peak {peak}");
            }
        }
    }

    #[test]
    fn delay_is_small() {
        let converter = RateConverter::new(44_100, 48_000).unwrap();
        assert!(converter.delay_ms() <= 20, "{} ms", converter.delay_ms());
    }

    #[test]
    fn sample_conversion_round_trips_and_clamps() {
        assert_eq!(to_i16(to_f32(1234)), 1234);
        assert_eq!(to_i16(2.0), i16::MAX);
        assert_eq!(to_i16(-2.0), -i16::MAX);
    }
}
