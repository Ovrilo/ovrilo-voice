//! Microphone capture with cpal (WASAPI shared mode) feeding a LiveKit `NativeAudioSource`.
//!
//! Spike finding (2026-10-14): WebRTC's built-in Windows audio device module delivered near-silence
//! from a USB microphone that cpal captures normally, so capture is done by us instead.
//!
//! Real-time rules (ADR-0009, AGENTS.md): the cpal callback only converts samples and pushes them into
//! a lock-free SPSC ring buffer. No locks, no allocation, no I/O. The DSP thread (`dsp.rs`) pulls 10 ms frames,
//! runs WebRTC audio processing and hands them to the LiveKit sender task.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};

/// A running capture stream; dropping it stops capture. Kept on the owning thread (cpal streams are not Send).
pub struct CpalCapture {
    _stream: Stream,
    pub device_name: String,
    pub sample_rate: u32,
    /// Samples dropped because the feeder fell behind (ring buffer full).
    pub overflow_samples: Arc<AtomicU64>,
}

/// Consumer side of the capture ring buffer; owned by the DSP thread.
pub struct CaptureFeed {
    consumer: rtrb::Consumer<i16>,
    pub sample_rate: u32,
}

/// Opens the input device (cpal index from `voice-spike mic-level`, or the system default) and starts
/// capturing mono 16-bit samples at the device's native rate.
pub fn start(index: Option<usize>) -> Result<(CpalCapture, CaptureFeed), String> {
    let host = cpal::default_host();
    let device = match index {
        Some(i) => host
            .input_devices()
            .map_err(|e| format!("input devices: {e}"))?
            .nth(i)
            .ok_or_else(|| format!("no input device {i}"))?,
        None => host
            .default_input_device()
            .ok_or("no default input device")?,
    };
    let supported = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    let sample_rate = supported.sample_rate();
    let channels = usize::from(supported.channels());
    let config = supported.config();

    // One second of mono audio absorbs scheduling hiccups of the feeder task.
    let (mut producer, consumer) = rtrb::RingBuffer::<i16>::new(sample_rate as usize);
    let overflow_samples = Arc::new(AtomicU64::new(0));
    let overflow = overflow_samples.clone();
    let on_error = |e| eprintln!("capture stream error: {e}");

    let stream = match supported.sample_format() {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |data, _| push_mono(&mut producer, &overflow, data, channels, |s| s),
            on_error,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |data, _| {
                push_mono(&mut producer, &overflow, data, channels, |s| {
                    f32::from(s) / f32::from(i16::MAX)
                })
            },
            on_error,
            None,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    }
    .map_err(|e| format!("build input stream: {e}"))?;
    stream.play().map_err(|e| format!("start capture: {e}"))?;

    Ok((
        CpalCapture {
            _stream: stream,
            device_name: device.to_string(),
            sample_rate,
            overflow_samples,
        },
        CaptureFeed {
            consumer,
            sample_rate,
        },
    ))
}

/// Real-time callback body: downmix interleaved frames to mono i16 and push without blocking.
fn push_mono<T: Copy>(
    producer: &mut rtrb::Producer<i16>,
    overflow: &AtomicU64,
    data: &[T],
    channels: usize,
    to_f32: impl Fn(T) -> f32,
) {
    for frame in data.chunks_exact(channels.max(1)) {
        let mono = frame.iter().map(|&s| to_f32(s)).sum::<f32>() / frame.len() as f32;
        let sample = (mono.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        if producer.push(sample).is_err() {
            overflow.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl CaptureFeed {
    /// Copies one full frame (`out.len()` samples) out of the ring buffer; false if not enough is buffered.
    pub fn read_frame(&mut self, out: &mut [i16]) -> bool {
        if out.is_empty() || self.consumer.slots() < out.len() {
            return false;
        }
        let chunk = self.consumer.read_chunk(out.len()).expect("checked slots");
        let (first, second) = chunk.as_slices();
        out[..first.len()].copy_from_slice(first);
        out[first.len()..].copy_from_slice(second);
        chunk.commit_all();
        true
    }
}
