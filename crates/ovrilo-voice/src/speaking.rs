//! Speaking detection for UI indicators: a level gate with a short attack and a longer release.
//!
//! Runs on the DSP thread per 10 ms frame; reports only transitions, so hosts get a few events per
//! utterance instead of a level stream (CONTRACTS.md §3).

use crate::audio::Frame;

/// RMS (i16 scale) above which a frame counts as voice, ≈ −42 dBFS. Processed microphone audio
/// (noise suppression on) and decoded remote audio sit well below this when nobody talks.
const SPEAKING_RMS: f32 = 260.0;
/// Consecutive loud frames before reporting speech, so clicks do not flash the indicator.
const ATTACK_FRAMES: u32 = 2;
/// Quiet frames before reporting silence, so pauses between words do not flicker it.
const RELEASE_FRAMES: u32 = 25;

#[derive(Debug, Default)]
pub(crate) struct SpeakingDetector {
    speaking: bool,
    loud: u32,
    quiet: u32,
}

impl SpeakingDetector {
    /// Feeds one frame; returns the new state when it changed.
    pub(crate) fn update(&mut self, frame: &Frame) -> Option<bool> {
        self.update_level(rms(frame))
    }

    /// Forces silence (muted, deafened source removed); returns `Some(false)` if it was speaking.
    pub(crate) fn silence(&mut self) -> Option<bool> {
        self.loud = 0;
        self.quiet = 0;
        std::mem::take(&mut self.speaking).then_some(false)
    }

    fn update_level(&mut self, level: f32) -> Option<bool> {
        if level >= SPEAKING_RMS {
            self.quiet = 0;
            self.loud += 1;
            if !self.speaking && self.loud >= ATTACK_FRAMES {
                self.speaking = true;
                return Some(true);
            }
        } else {
            self.loud = 0;
            self.quiet += 1;
            if self.speaking && self.quiet >= RELEASE_FRAMES {
                self.speaking = false;
                return Some(false);
            }
        }
        None
    }
}

fn rms(frame: &Frame) -> f32 {
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::FRAME_SAMPLES;

    const LOUD: Frame = [2000; FRAME_SAMPLES];
    const QUIET: Frame = [20; FRAME_SAMPLES];

    #[test]
    fn needs_two_loud_frames_to_start_and_a_pause_to_stop() {
        let mut d = SpeakingDetector::default();
        assert_eq!(d.update(&LOUD), None);
        assert_eq!(d.update(&LOUD), Some(true));
        assert_eq!(d.update(&LOUD), None);
        for _ in 0..RELEASE_FRAMES - 1 {
            assert_eq!(d.update(&QUIET), None, "short pauses keep the indicator on");
        }
        assert_eq!(d.update(&QUIET), Some(false));
    }

    #[test]
    fn a_single_click_does_not_count() {
        let mut d = SpeakingDetector::default();
        assert_eq!(d.update(&LOUD), None);
        assert_eq!(d.update(&QUIET), None);
        assert_eq!(d.update(&LOUD), None);
    }

    #[test]
    fn silence_reports_only_when_speaking() {
        let mut d = SpeakingDetector::default();
        assert_eq!(d.silence(), None);
        d.update(&LOUD);
        d.update(&LOUD);
        assert_eq!(d.silence(), Some(false));
        assert_eq!(d.silence(), None);
    }
}
