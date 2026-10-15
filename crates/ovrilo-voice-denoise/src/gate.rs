//! Voice gate: passes audio while the model hears speech and silences it otherwise.
//!
//! Driven by DeepFilterNet's per-frame local SNR estimate (dB, about −15 for pure noise to 35 for
//! clean speech), not by loudness, so a loud keyboard does not open it and quiet speech does.
//! Gain changes are ramped within a frame so opening and closing never click.

use ovrilo_voice::audio::{FRAME_SAMPLES, Frame};

/// Local SNR at or above which a frame counts as speech and opens the gate.
const OPEN_DB: f32 = 2.0;
/// While open, frames above this keep it open (hysteresis: speech tails sit lower than onsets,
/// and a distant voice sits lower still).
const STAY_OPEN_DB: f32 = -7.0;
/// Frames the gate stays open after the last speech frame, so pauses between words and soft word
/// endings are kept.
const HOLD_FRAMES: u32 = 25;
/// Frames over which the gate fades out once the hold has passed.
const FADE_OUT_FRAMES: u32 = 4;

#[derive(Debug, Clone)]
pub struct VoiceGate {
    open: bool,
    /// Frames left before the gate starts closing.
    hold: u32,
    /// Gain applied at the end of the previous frame (0..=1).
    gain: f32,
}

impl Default for VoiceGate {
    fn default() -> Self {
        Self {
            open: false,
            hold: 0,
            gain: 0.0,
        }
    }
}

impl VoiceGate {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Applies the gate to one (already denoised) frame given the model's local SNR for it.
    pub fn process(&mut self, frame: &mut Frame, lsnr_db: f32) {
        let speech = if self.open {
            lsnr_db >= STAY_OPEN_DB
        } else {
            lsnr_db >= OPEN_DB
        };
        if speech {
            self.open = true;
            self.hold = HOLD_FRAMES;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            self.open = false;
        }

        // Opening is immediate (ramped over this frame); closing fades over a few frames.
        let target = if self.open {
            1.0
        } else {
            (self.gain - 1.0 / FADE_OUT_FRAMES as f32).max(0.0)
        };
        let start = self.gain;
        if start == target && target == 1.0 {
            return;
        }
        if start == target && target == 0.0 {
            frame.fill(0);
            return;
        }
        let step = (target - start) / FRAME_SAMPLES as f32;
        for (i, sample) in frame.iter_mut().enumerate() {
            let gain = start + step * (i + 1) as f32;
            *sample = (f32::from(*sample) * gain) as i16;
        }
        self.gain = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOISE_DB: f32 = -12.0;
    const SPEECH_DB: f32 = 15.0;

    fn tone() -> Frame {
        [1000; FRAME_SAMPLES]
    }

    fn run(gate: &mut VoiceGate, lsnr: f32) -> Frame {
        let mut frame = tone();
        gate.process(&mut frame, lsnr);
        frame
    }

    #[test]
    fn noise_alone_never_opens_it() {
        let mut gate = VoiceGate::default();
        for _ in 0..100 {
            assert!(run(&mut gate, NOISE_DB).iter().all(|&s| s == 0));
        }
        assert!(!gate.is_open());
    }

    #[test]
    fn speech_opens_it_within_one_frame_without_a_click() {
        let mut gate = VoiceGate::default();
        let first = run(&mut gate, SPEECH_DB);
        assert!(gate.is_open());
        // Ramps up across the frame instead of jumping to full level.
        assert!(first[0] < 50 && first[FRAME_SAMPLES - 1] == 1000);
        assert!(first.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(run(&mut gate, SPEECH_DB), tone());
    }

    #[test]
    fn pauses_between_words_are_kept() {
        let mut gate = VoiceGate::default();
        run(&mut gate, SPEECH_DB);
        for _ in 0..HOLD_FRAMES {
            assert_eq!(run(&mut gate, NOISE_DB), tone());
        }
        // Speech resumes inside the hold: still fully open.
        assert_eq!(run(&mut gate, SPEECH_DB), tone());
    }

    #[test]
    fn soft_speech_keeps_it_open_but_does_not_open_it() {
        let soft = (OPEN_DB + STAY_OPEN_DB) / 2.0;
        let mut closed = VoiceGate::default();
        run(&mut closed, soft);
        assert!(!closed.is_open());

        let mut open = VoiceGate::default();
        run(&mut open, SPEECH_DB);
        for _ in 0..HOLD_FRAMES * 3 {
            assert_eq!(run(&mut open, soft), tone());
        }
    }

    #[test]
    fn closes_with_a_fade_after_the_hold() {
        let mut gate = VoiceGate::default();
        run(&mut gate, SPEECH_DB);
        for _ in 0..=HOLD_FRAMES {
            run(&mut gate, NOISE_DB);
        }
        assert!(!gate.is_open());
        let mut last_peak = 1000;
        for _ in 0..FADE_OUT_FRAMES {
            let frame = run(&mut gate, NOISE_DB);
            let peak = frame[0];
            assert!(peak <= last_peak && frame[FRAME_SAMPLES - 1] < peak.max(1));
            last_peak = frame[FRAME_SAMPLES - 1];
        }
        assert!(run(&mut gate, NOISE_DB).iter().all(|&s| s == 0));
    }
}
