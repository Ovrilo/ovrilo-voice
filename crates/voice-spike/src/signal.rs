//! Test signal generation and analysis. Pure functions, no I/O, unit-testable.
//!
//! The tone source emits a short 440 Hz beep at the start of every wall-clock second.
//! Two processes on the same machine share the clock, so a receiver can measure one-way
//! latency as "beep onset arrival time − start of that second".

use std::f32::consts::TAU;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_MS: u32 = 10;
pub const SAMPLES_PER_FRAME: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
pub const TONE_HZ: f32 = 440.0;
pub const BEEP_MS: u64 = 100;
/// Beep amplitude: -12 dBFS.
const BEEP_AMPLITUDE: f32 = 0.25;

/// Fills one 10 ms mono frame whose first sample corresponds to `start_epoch_samples`
/// (samples since the Unix epoch at 48 kHz). Beeps occupy the first `BEEP_MS` of each second.
pub fn tone_frame(start_epoch_samples: u64, out: &mut [i16; SAMPLES_PER_FRAME]) {
    let samples_per_second = u64::from(SAMPLE_RATE);
    let beep_samples = samples_per_second * BEEP_MS / 1000;
    for (i, sample) in out.iter_mut().enumerate() {
        let n = start_epoch_samples + i as u64;
        let in_beep = n % samples_per_second < beep_samples;
        *sample = if in_beep {
            let phase = (n % samples_per_second) as f32 * TONE_HZ / SAMPLE_RATE as f32;
            ((phase * TAU).sin() * BEEP_AMPLITUDE * f32::from(i16::MAX)) as i16
        } else {
            0
        };
    }
}

/// RMS level of a frame in dBFS (−inf for silence is clamped to −120).
pub fn rms_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let rms = (sum / samples.len() as f64).sqrt() / f64::from(i16::MAX);
    (20.0 * rms.max(1e-6).log10()) as f32
}

/// Amplitude (0..1) of `freq_hz` in the frame, via the Goertzel algorithm.
pub fn tone_amplitude(samples: &[i16], sample_rate: u32, freq_hz: f32) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len() as f32;
    let coefficient = 2.0 * (TAU * freq_hz / sample_rate as f32).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &sample in samples {
        let s0 = f32::from(sample) / f32::from(i16::MAX) + coefficient * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coefficient * s1 * s2;
    2.0 * power.max(0.0).sqrt() / n
}

/// Detects beep onsets in a stream of frames and reports one-way latency per beep.
#[derive(Debug, Default)]
pub struct BeepDetector {
    in_beep: bool,
    quiet_ms: u64,
    pub latencies_ms: Vec<u64>,
}

impl BeepDetector {
    /// Amplitude above which a frame counts as "beep present" (~ -30 dBFS of a sine).
    const ONSET_AMPLITUDE: f32 = 0.03;
    /// Required silence before a new onset, so one beep is counted once.
    const MIN_QUIET_MS: u64 = 300;

    /// Feeds one received frame. `arrival_epoch_ms` is when the frame was delivered to us.
    pub fn push(
        &mut self,
        samples: &[i16],
        sample_rate: u32,
        arrival_epoch_ms: u64,
    ) -> Option<u64> {
        let frame_ms = (samples.len() as u64 * 1000) / u64::from(sample_rate.max(1));
        let present = tone_amplitude(samples, sample_rate, TONE_HZ) > Self::ONSET_AMPLITUDE;
        if present && !self.in_beep && self.quiet_ms >= Self::MIN_QUIET_MS {
            self.in_beep = true;
            self.quiet_ms = 0;
            let latency = arrival_epoch_ms % 1000;
            self.latencies_ms.push(latency);
            return Some(latency);
        }
        if present {
            self.in_beep = true;
            self.quiet_ms = 0;
        } else {
            self.in_beep = false;
            self.quiet_ms += frame_ms;
        }
        None
    }
}

/// Summary statistics of a latency sample set.
#[derive(Debug, PartialEq)]
pub struct LatencyStats {
    pub count: usize,
    pub min_ms: u64,
    pub mean_ms: f64,
    pub p95_ms: u64,
    pub max_ms: u64,
    /// Standard deviation of latency, a simple jitter indicator.
    pub stddev_ms: f64,
}

pub fn latency_stats(samples: &[u64]) -> Option<LatencyStats> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let count = sorted.len();
    let mean = sorted.iter().sum::<u64>() as f64 / count as f64;
    let variance = sorted
        .iter()
        .map(|&v| (v as f64 - mean).powi(2))
        .sum::<f64>()
        / count as f64;
    let p95_index = ((count as f64 * 0.95).ceil() as usize).clamp(1, count) - 1;
    Some(LatencyStats {
        count,
        min_ms: sorted[0],
        mean_ms: mean,
        p95_ms: sorted[p95_index],
        max_ms: sorted[count - 1],
        stddev_ms: variance.sqrt(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_at(ms: u64) -> [i16; SAMPLES_PER_FRAME] {
        let mut frame = [0; SAMPLES_PER_FRAME];
        tone_frame(ms * u64::from(SAMPLE_RATE) / 1000, &mut frame);
        frame
    }

    #[test]
    fn tone_is_present_only_at_the_start_of_each_second() {
        assert!(tone_amplitude(&frame_at(5_000), SAMPLE_RATE, TONE_HZ) > 0.2);
        assert!(tone_amplitude(&frame_at(5_090), SAMPLE_RATE, TONE_HZ) > 0.2);
        assert_eq!(rms_dbfs(&frame_at(5_100)), -120.0);
        assert_eq!(rms_dbfs(&frame_at(5_500)), -120.0);
    }

    #[test]
    fn beep_level_is_about_minus_15_dbfs_rms() {
        let level = rms_dbfs(&frame_at(7_000));
        assert!((-16.0..-14.0).contains(&level), "level {level}");
    }

    #[test]
    fn goertzel_rejects_other_frequencies() {
        let other: Vec<i16> = (0..SAMPLES_PER_FRAME)
            .map(|n| ((n as f32 * 1000.0 / SAMPLE_RATE as f32 * TAU).sin() * 8000.0) as i16)
            .collect();
        assert!(tone_amplitude(&other, SAMPLE_RATE, TONE_HZ) < 0.03);
    }

    #[test]
    fn detector_reports_latency_once_per_beep() {
        let mut detector = BeepDetector::default();
        let latency_ms = 85;
        // Simulate three seconds of frames arriving `latency_ms` after they were generated.
        for generated in (10_000..13_000).step_by(FRAME_MS as usize) {
            detector.push(&frame_at(generated), SAMPLE_RATE, generated + latency_ms);
        }
        // The first beep (at 10_000) is not counted: no preceding silence was observed.
        assert_eq!(detector.latencies_ms, vec![latency_ms, latency_ms]);
    }

    #[test]
    fn latency_stats_summarize_samples() {
        let stats = latency_stats(&[40, 50, 60, 70, 80]).unwrap();
        assert_eq!(
            (stats.count, stats.min_ms, stats.p95_ms, stats.max_ms),
            (5, 40, 80, 80)
        );
        assert!((stats.mean_ms - 60.0).abs() < f64::EPSILON);
        assert!(latency_stats(&[]).is_none());
    }
}
