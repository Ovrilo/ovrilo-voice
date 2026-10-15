//! Measures model build time and per-frame processing time on this computer.
//! `cargo run -p ovrilo-voice-denoise --example denoise_timing --release`
use std::time::Instant;

use ovrilo_voice::audio::{FRAME_SAMPLES, Frame};
use ovrilo_voice_denoise::Denoiser;

fn main() {
    let started = Instant::now();
    let mut denoiser = Denoiser::new().expect("model");
    println!(
        "build: {:?}, added latency: {:?}",
        started.elapsed(),
        denoiser.latency()
    );

    // Pseudo-random noise with a speech-like tone burst every other second.
    let mut seed = 1u32;
    let mut worst = std::time::Duration::ZERO;
    let started = Instant::now();
    let frames = 1000;
    for n in 0..frames {
        let mut frame: Frame = [0; FRAME_SAMPLES];
        for (i, sample) in frame.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((seed >> 16) as i16) / 16;
            let t = (n * FRAME_SAMPLES + i) as f32 / 48_000.0;
            let voice = if (n / 100) % 2 == 0 {
                (t * 220.0 * std::f32::consts::TAU).sin() * 6000.0
            } else {
                0.0
            };
            *sample = noise.saturating_add(voice as i16);
        }
        let frame_started = Instant::now();
        denoiser.process(&mut frame);
        worst = worst.max(frame_started.elapsed());
    }
    let total = started.elapsed();
    println!(
        "{frames} frames: {:?} per frame on average, worst {:?}",
        total / frames as u32,
        worst
    );
}
