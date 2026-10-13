//! Captures from a microphone with cpal (WASAPI shared mode) and prints the peak level each second.
//! Diagnostic for devices that WebRTC's built-in Windows audio layer fails to open.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};

pub fn mic_level(index: Option<usize>, seconds: u64) -> Result<(), String> {
    let host = cpal::default_host();
    let devices: Vec<_> = host
        .input_devices()
        .map_err(|e| format!("input devices: {e}"))?
        .collect();
    for (i, device) in devices.iter().enumerate() {
        println!("  [{i}] {device}");
    }
    let device = match index {
        Some(i) => devices
            .get(i)
            .cloned()
            .ok_or_else(|| format!("no input device {i}"))?,
        None => host
            .default_input_device()
            .ok_or("no default input device")?,
    };
    let supported = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    println!(
        "capturing from: {device} ({} Hz, {} ch, {:?})",
        supported.sample_rate(),
        supported.channels(),
        supported.sample_format()
    );

    // Peak amplitude stored as f32 bits; the audio callback only does atomic ops (no locks, no allocation).
    let peak = Arc::new(AtomicU32::new(0));
    let config: StreamConfig = supported.config();
    let on_error = |e| eprintln!("stream error: {e}");
    let stream = match supported.sample_format() {
        SampleFormat::F32 => {
            let peak = peak.clone();
            device.build_input_stream::<f32, _, _>(
                config,
                move |data, _| record_peak(&peak, data.iter().map(|s| s.abs())),
                on_error,
                None,
            )
        }
        SampleFormat::I16 => {
            let peak = peak.clone();
            device.build_input_stream::<i16, _, _>(
                config,
                move |data, _| {
                    record_peak(
                        &peak,
                        data.iter()
                            .map(|&s| (f32::from(s) / f32::from(i16::MAX)).abs()),
                    )
                },
                on_error,
                None,
            )
        }
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| format!("build input stream: {e}"))?;
    stream.play().map_err(|e| format!("start stream: {e}"))?;

    for second in 1..=seconds {
        std::thread::sleep(Duration::from_secs(1));
        let value = f32::from_bits(peak.swap(0, Ordering::Relaxed));
        println!(
            "t={second:>2}s peak {:.1} dBFS",
            20.0 * value.max(1e-6).log10()
        );
    }
    Ok(())
}

fn record_peak(peak: &AtomicU32, samples: impl Iterator<Item = f32>) {
    let frame_peak = samples.fold(0.0f32, f32::max);
    let _ = peak.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        (frame_peak > f32::from_bits(current)).then(|| frame_peak.to_bits())
    });
}
