//! Detects when noise suppression cannot keep up with real time on this computer.
//!
//! Each 10 ms frame must be processed well within 10 ms, or capture backs up and audio is lost.
//! Single slow frames (a game loading, a context switch) are tolerated; a sustained run is not.

use std::time::Duration;

/// Processing time per frame above which a frame counts as slow (most of the 10 ms budget).
const SLOW_FRAME: Duration = Duration::from_millis(7);
/// Share of recent frames that may be slow before suppression is considered overloaded.
const SLOW_SHARE: f32 = 0.3;
/// Smoothing of the slow-frame share: about one second of frames.
const WINDOW_FRAMES: f32 = 100.0;

#[derive(Debug, Clone, Default)]
pub struct OverloadDetector {
    slow_share: f32,
    frames: u32,
    /// Average processing time in microseconds (smoothed like the share).
    average_us: f32,
}

impl OverloadDetector {
    /// Records one frame's processing time; true once the computer is too slow for real time.
    pub fn record(&mut self, elapsed: Duration) -> bool {
        let slow = if elapsed > SLOW_FRAME { 1.0 } else { 0.0 };
        let alpha = 1.0 / WINDOW_FRAMES;
        self.slow_share += (slow - self.slow_share) * alpha;
        self.average_us += (elapsed.as_micros() as f32 - self.average_us) * alpha;
        self.frames = self.frames.saturating_add(1);
        // Judge only after a full window, so the first frames (caches warming up) do not count.
        self.frames as f32 >= WINDOW_FRAMES && self.slow_share > SLOW_SHARE
    }

    /// Smoothed processing time per frame.
    pub fn average(&self) -> Duration {
        Duration::from_micros(self.average_us as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = Duration::from_millis(2);
    const SLOW: Duration = Duration::from_millis(12);

    #[test]
    fn fast_processing_is_fine() {
        let mut detector = OverloadDetector::default();
        assert!((0..1000).all(|_| !detector.record(FAST)));
        assert!(detector.average() < Duration::from_millis(3));
    }

    #[test]
    fn occasional_slow_frames_are_tolerated() {
        let mut detector = OverloadDetector::default();
        assert!((0..2000).all(|i| !detector.record(if i % 10 == 0 { SLOW } else { FAST })));
    }

    #[test]
    fn sustained_slowness_is_detected_within_a_second_or_two() {
        let mut detector = OverloadDetector::default();
        for _ in 0..200 {
            detector.record(FAST);
        }
        let frames = (1..=300).find(|_| detector.record(SLOW));
        assert!(frames.is_some_and(|n| n <= 200), "{frames:?}");
    }

    #[test]
    fn a_slow_start_is_not_judged_before_a_full_window() {
        let mut detector = OverloadDetector::default();
        assert!((0..50).all(|_| !detector.record(SLOW)));
    }
}
