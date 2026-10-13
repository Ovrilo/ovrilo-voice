//! Measures the acoustic echo path delay: speaker click → microphone, including WASAPI buffering.
//!
//! Plays a short 2 kHz click at the start of every second through a cpal output device while
//! recording a cpal input device. Both streams report hardware timestamps on the same clock
//! (`StreamInstant`), so delay = mic capture time of the click onset − speaker playback time of the click.
//! Diagnostic tool: callbacks still avoid allocation; first timestamps are stored once in a `OnceLock`.

use std::f32::consts::TAU;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamInstant};

const CLICK_HZ: f32 = 2000.0;
const CLICK_MS: u32 = 5;
const SECONDS: u32 = 8;

pub fn measure(input: Option<usize>, output: Option<usize>) -> Result<(), String> {
    let host = cpal::default_host();
    let in_device = match input {
        Some(i) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .nth(i)
            .ok_or("no such input")?,
        None => host.default_input_device().ok_or("no default input")?,
    };
    let out_device = match output {
        Some(i) => host
            .output_devices()
            .map_err(|e| e.to_string())?
            .nth(i)
            .ok_or("no such output")?,
        None => host.default_output_device().ok_or("no default output")?,
    };
    let in_config = in_device
        .default_input_config()
        .map_err(|e| e.to_string())?;
    let out_config = out_device
        .default_output_config()
        .map_err(|e| e.to_string())?;
    if in_config.sample_format() != SampleFormat::F32
        || out_config.sample_format() != SampleFormat::F32
    {
        return Err("this diagnostic expects f32 devices".into());
    }
    let in_rate = in_config.sample_rate();
    let out_rate = out_config.sample_rate();
    let in_channels = usize::from(in_config.channels());
    let out_channels = usize::from(out_config.channels());
    println!("speaker: {out_device} ({out_rate} Hz) → mic: {in_device} ({in_rate} Hz)");

    let out_first: Arc<OnceLock<StreamInstant>> = Arc::default();
    let in_first: Arc<OnceLock<StreamInstant>> = Arc::default();

    let (mut producer, mut consumer) =
        rtrb::RingBuffer::<f32>::new((in_rate * (SECONDS + 2)) as usize);
    let in_first_cb = in_first.clone();
    let input_stream = in_device
        .build_input_stream::<f32, _, _>(
            in_config.config(),
            move |data, info| {
                let _ = in_first_cb.set(info.timestamp().capture);
                for frame in data.chunks_exact(in_channels.max(1)) {
                    let _ = producer.push(frame.iter().sum::<f32>() / frame.len() as f32);
                }
            },
            |e| eprintln!("input error: {e}"),
            None,
        )
        .map_err(|e| format!("input stream: {e}"))?;

    let out_first_cb = out_first.clone();
    let click_samples = u64::from(out_rate * CLICK_MS / 1000);
    let mut position = 0u64;
    let output_stream = out_device
        .build_output_stream::<f32, _, _>(
            out_config.config(),
            move |data: &mut [f32], info| {
                let _ = out_first_cb.set(info.timestamp().playback);
                for frame in data.chunks_exact_mut(out_channels.max(1)) {
                    let in_second = position % u64::from(out_rate);
                    let value = if in_second < click_samples {
                        (in_second as f32 * CLICK_HZ / out_rate as f32 * TAU).sin() * 0.5
                    } else {
                        0.0
                    };
                    frame.fill(value);
                    position += 1;
                }
            },
            |e| eprintln!("output error: {e}"),
            None,
        )
        .map_err(|e| format!("output stream: {e}"))?;

    input_stream.play().map_err(|e| e.to_string())?;
    output_stream.play().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(u64::from(SECONDS) * 1000 + 600));
    drop(output_stream);
    drop(input_stream);

    let mut recorded = Vec::with_capacity(consumer.slots());
    while let Ok(sample) = consumer.pop() {
        recorded.push(sample);
    }
    let (Some(&out_first), Some(&in_first)) = (out_first.get(), in_first.get()) else {
        return Err("streams did not start".into());
    };

    let delays = click_delays(&recorded, in_rate, in_first, out_first);
    if delays.is_empty() {
        println!(
            "no clicks detected in the microphone signal (raise speaker volume / move closer)"
        );
        return Ok(());
    }
    for (i, d) in delays.iter().enumerate() {
        println!("click {}: {d} ms", i + 1);
    }
    let mut sorted = delays.clone();
    sorted.sort_unstable();
    println!(
        "echo path delay: median {} ms (min {}, max {}, {} clicks)",
        sorted[sorted.len() / 2],
        sorted[0],
        sorted[sorted.len() - 1],
        sorted.len()
    );
    Ok(())
}

/// Finds click onsets in the recording and converts them to delays relative to the click playback times.
fn click_delays(
    recorded: &[f32],
    in_rate: u32,
    in_first: StreamInstant,
    out_first: StreamInstant,
) -> Vec<u64> {
    let noise = {
        let mut abs: Vec<f32> = recorded.iter().map(|s| s.abs()).collect();
        abs.sort_by(f32::total_cmp);
        abs.get(abs.len() / 2).copied().unwrap_or(0.0)
    };
    let threshold = (noise * 10.0).max(0.01);
    let min_gap = (in_rate as usize) * 3 / 10;

    let mut delays = Vec::new();
    let mut last_onset: Option<usize> = None;
    for (index, sample) in recorded.iter().enumerate() {
        if sample.abs() < threshold || last_onset.is_some_and(|last| index - last < min_gap) {
            continue;
        }
        last_onset = Some(index);
        let offset = Duration::from_secs_f64(index as f64 / f64::from(in_rate));
        let Some(captured_at) = in_first.checked_add(offset) else {
            continue;
        };
        if captured_at < out_first {
            continue;
        }
        // Clicks are played at out_first + k seconds; attribute the onset to the latest preceding click.
        let since_first_click = captured_at.duration_since(out_first);
        let delay_ms = (since_first_click.as_millis() % 1000) as u64;
        delays.push(delay_ms);
    }
    delays
}
