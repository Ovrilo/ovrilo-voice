//! Mixes remote participants' 10 ms frames into one playout frame.

use crate::audio::{FRAME_SAMPLES, Frame};

/// Sums frames with saturation. Each remote stream contributes at most one frame per mix.
#[derive(Debug)]
pub struct Mixer {
    accumulator: [i32; FRAME_SAMPLES],
    sources: usize,
}

impl Default for Mixer {
    fn default() -> Self {
        Self {
            accumulator: [0; FRAME_SAMPLES],
            sources: 0,
        }
    }
}

impl Mixer {
    pub fn add(&mut self, frame: &Frame) {
        self.add_scaled(frame, 100);
    }

    /// Adds a frame at `percent` volume (100 = unchanged). Integer math; clipping happens once
    /// in [`Mixer::finish`].
    pub fn add_scaled(&mut self, frame: &Frame, percent: u16) {
        if percent == 100 {
            for (acc, &sample) in self.accumulator.iter_mut().zip(frame) {
                *acc += i32::from(sample);
            }
        } else {
            let percent = i32::from(percent);
            for (acc, &sample) in self.accumulator.iter_mut().zip(frame) {
                *acc += i32::from(sample) * percent / 100;
            }
        }
        self.sources += 1;
    }

    /// Number of frames added since the last [`Mixer::finish`].
    pub fn sources(&self) -> usize {
        self.sources
    }

    /// Writes the clipped sum into `out` and resets the mixer.
    pub fn finish(&mut self, out: &mut Frame) {
        for (sample, acc) in out.iter_mut().zip(self.accumulator.iter_mut()) {
            *sample = (*acc).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            *acc = 0;
        }
        self.sources = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_and_resets() {
        let mut mixer = Mixer::default();
        mixer.add(&[100; FRAME_SAMPLES]);
        mixer.add(&[-30; FRAME_SAMPLES]);
        let mut out = [0; FRAME_SAMPLES];
        assert_eq!(mixer.sources(), 2);
        mixer.finish(&mut out);
        assert!(out.iter().all(|&s| s == 70));

        mixer.finish(&mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn scales_per_source() {
        let mut mixer = Mixer::default();
        mixer.add_scaled(&[1000; FRAME_SAMPLES], 50);
        mixer.add_scaled(&[1000; FRAME_SAMPLES], 200);
        mixer.add_scaled(&[1000; FRAME_SAMPLES], 0);
        let mut out = [0; FRAME_SAMPLES];
        mixer.finish(&mut out);
        assert!(out.iter().all(|&s| s == 2500));
    }

    #[test]
    fn saturates_instead_of_wrapping() {
        let mut mixer = Mixer::default();
        mixer.add(&[i16::MAX; FRAME_SAMPLES]);
        mixer.add(&[i16::MAX; FRAME_SAMPLES]);
        mixer.add(&[i16::MIN; FRAME_SAMPLES]);
        let mut out = [0; FRAME_SAMPLES];
        mixer.finish(&mut out);
        assert!(out.iter().all(|&s| s == i16::MAX - 1));

        mixer.add(&[i16::MIN; FRAME_SAMPLES]);
        mixer.add(&[i16::MIN; FRAME_SAMPLES]);
        mixer.finish(&mut out);
        assert!(out.iter().all(|&s| s == i16::MIN));
    }
}
