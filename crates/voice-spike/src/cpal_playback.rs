//! Speaker playout with cpal (WASAPI shared mode), fed from a lock-free ring buffer.
//!
//! The output callback only pops samples and writes them to every channel: no locks, no allocation.
//! A small pre-buffer absorbs delivery jitter; when the buffer runs dry the callback outputs silence,
//! counts an underrun and re-buffers. If more than `MAX_FILL_MS` accumulates (bursty delivery),
//! the callback skips audio back to the pre-buffer level so latency cannot grow unbounded.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};

/// Pre-buffer before (re)starting playout, in milliseconds.
const PREBUFFER_MS: u32 = 40;
/// Fill above which buffered audio is skipped back to the pre-buffer level.
const MAX_FILL_MS: u32 = 100;

/// A running playout stream; dropping it stops playout. Kept on the owning thread (not Send).
pub struct CpalPlayback {
    _stream: Stream,
    pub device_name: String,
    pub sample_rate: u32,
    /// Callbacks that found the buffer empty after playout had started.
    pub underruns: Arc<AtomicU64>,
    /// Times buffered audio was skipped to bound latency.
    pub latency_trims: Arc<AtomicU64>,
}

/// Producer side of the playout ring buffer; owned by the DSP thread.
pub struct PlaybackFeed {
    producer: rtrb::Producer<i16>,
    capacity: usize,
    pub sample_rate: u32,
    /// Samples dropped because the buffer was full (playout slower than delivery).
    pub overflow_samples: u64,
}

impl PlaybackFeed {
    /// Queues mono samples for playout; drops what does not fit.
    pub fn push(&mut self, samples: &[i16]) {
        let (_, rest) = self.producer.push_partial_slice(samples);
        self.overflow_samples += rest.len() as u64;
    }

    /// Audio currently queued for playout, in milliseconds (excludes the device's own buffer).
    pub fn buffered_ms(&self) -> u32 {
        let buffered = self.capacity - self.producer.slots();
        (buffered as u64 * 1000 / u64::from(self.sample_rate)) as u32
    }
}

pub fn start(index: Option<usize>) -> Result<(CpalPlayback, PlaybackFeed), String> {
    let host = cpal::default_host();
    let device = match index {
        Some(i) => host
            .output_devices()
            .map_err(|e| format!("output devices: {e}"))?
            .nth(i)
            .ok_or_else(|| format!("no output device {i}"))?,
        None => host
            .default_output_device()
            .ok_or("no default output device")?,
    };
    let supported = device
        .default_output_config()
        .map_err(|e| format!("output config: {e}"))?;
    let sample_rate = supported.sample_rate();
    let channels = usize::from(supported.channels());
    let config = supported.config();

    // Half a second of mono audio; normal fill stays near the pre-buffer.
    let capacity = sample_rate as usize / 2;
    let (producer, mut consumer) = rtrb::RingBuffer::<i16>::new(capacity);
    let prebuffer = (sample_rate * PREBUFFER_MS / 1000) as usize;
    let max_fill = (sample_rate * MAX_FILL_MS / 1000) as usize;
    let underruns = Arc::new(AtomicU64::new(0));
    let underrun_counter = underruns.clone();
    let latency_trims = Arc::new(AtomicU64::new(0));
    let trim_counter = latency_trims.clone();
    let mut playing = false;
    let on_error = |e| eprintln!("playout stream error: {e}");

    let mut next_sample = move |callback_start: bool| -> Option<i16> {
        if callback_start {
            let fill = consumer.slots();
            if fill > max_fill
                && let Ok(chunk) = consumer.read_chunk(fill - prebuffer)
            {
                chunk.commit_all();
                trim_counter.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !playing {
            if consumer.slots() < prebuffer {
                return None;
            }
            playing = true;
        }
        match consumer.pop() {
            Ok(sample) => Some(sample),
            Err(_) => {
                playing = false;
                underrun_counter.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    };

    let stream = match supported.sample_format() {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config,
            move |data, _| {
                for (i, frame) in data.chunks_exact_mut(channels.max(1)).enumerate() {
                    let value =
                        next_sample(i == 0).map_or(0.0, |s| f32::from(s) / f32::from(i16::MAX));
                    frame.fill(value);
                }
            },
            on_error,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config,
            move |data, _| {
                for (i, frame) in data.chunks_exact_mut(channels.max(1)).enumerate() {
                    frame.fill(next_sample(i == 0).unwrap_or(0));
                }
            },
            on_error,
            None,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| format!("build output stream: {e}"))?;
    stream.play().map_err(|e| format!("start playout: {e}"))?;

    Ok((
        CpalPlayback {
            _stream: stream,
            device_name: device.to_string(),
            sample_rate,
            underruns,
            latency_trims,
        },
        PlaybackFeed {
            producer,
            capacity,
            sample_rate,
            overflow_samples: 0,
        },
    ))
}

/// Lists cpal input and output devices with the indexes used by `--input`/`--output` for cpal paths.
pub fn list_devices() -> Result<(), String> {
    let host = cpal::default_host();
    println!("cpal inputs (--input with --source cpal-mic):");
    for (i, device) in host.input_devices().map_err(|e| e.to_string())?.enumerate() {
        println!("  [{i}] {device}");
    }
    println!("cpal outputs (--output with --playback cpal):");
    for (i, device) in host
        .output_devices()
        .map_err(|e| e.to_string())?
        .enumerate()
    {
        println!("  [{i}] {device}");
    }
    Ok(())
}
