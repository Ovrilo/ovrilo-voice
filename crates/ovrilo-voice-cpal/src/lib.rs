//! cpal (WASAPI on Windows) implementation of [`ovrilo_voice::AudioBackend`] (ADR-0016).
//!
//! Each opened device runs its cpal stream on a dedicated thread (cpal streams are not `Send`);
//! the engine gets only the `Send` ends of lock-free SPSC ring buffers. Real-time callbacks convert
//! samples and push/pop the ring buffer: no locks, no allocation, no I/O.
//!
//! Devices run at their own rate (the Windows shared-mode format). When it is not 48 kHz, the
//! capture/playout streams convert on the engine's DSP thread (`resample`), not in callbacks.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

mod resample;

use resample::{RateConverter, to_f32, to_i16};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Host, SampleFormat, Stream, SupportedStreamConfig};
use ovrilo_voice::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use ovrilo_voice::{
    AudioBackend, AudioDevice, CaptureStream, DeviceId, Frame, PlayoutStream, VoiceError,
};

/// Pre-buffer before (re)starting playout.
const PREBUFFER_MS: u32 = 40;
/// Buffered playout above this is skipped back to the pre-buffer level to bound latency.
const MAX_PLAYOUT_FILL_MS: u32 = 120;

pub struct CpalBackend {
    host: Host,
}

impl Default for CpalBackend {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

impl CpalBackend {
    fn list(&self, input: bool) -> Result<Vec<AudioDevice>, VoiceError> {
        let default = if input {
            self.host.default_input_device()
        } else {
            self.host.default_output_device()
        };
        let default_id = default.and_then(|d| d.id().ok());
        let devices = if input {
            self.host.input_devices()
        } else {
            self.host.output_devices()
        }
        .map_err(|e| VoiceError::device(format!("enumerate devices: {e}")))?;
        Ok(devices
            .filter_map(|device| {
                let id = device.id().ok()?;
                Some(AudioDevice {
                    is_default: default_id.as_ref() == Some(&id),
                    id: DeviceId(id.to_string()),
                    name: device.to_string(),
                })
            })
            .collect())
    }

    fn resolve(&self, id: Option<&DeviceId>, input: bool) -> Result<Device, VoiceError> {
        match id {
            Some(id) => {
                let parsed = cpal::DeviceId::from_str(&id.0)
                    .map_err(|_| VoiceError::device("invalid device id"))?;
                self.host
                    .device_by_id(&parsed)
                    .ok_or_else(|| VoiceError::device("the selected audio device is not available"))
            }
            None if input => self
                .host
                .default_input_device()
                .ok_or_else(|| VoiceError::device("no microphone found")),
            None => self
                .host
                .default_output_device()
                .ok_or_else(|| VoiceError::device("no speaker found")),
        }
    }
}

impl AudioBackend for CpalBackend {
    fn input_devices(&self) -> Result<Vec<AudioDevice>, VoiceError> {
        self.list(true)
    }

    fn output_devices(&self) -> Result<Vec<AudioDevice>, VoiceError> {
        self.list(false)
    }

    fn open_input(&mut self, id: Option<&DeviceId>) -> Result<Box<dyn CaptureStream>, VoiceError> {
        let device = self.resolve(id, true)?;
        let config = device
            .default_input_config()
            .map_err(|e| VoiceError::device(format!("microphone format: {e}")))?;
        let device_rate = config.sample_rate();
        let converter = (device_rate != SAMPLE_RATE)
            .then(|| RateConverter::new(device_rate, SAMPLE_RATE))
            .transpose()?;
        let pending = Vec::with_capacity(
            FRAME_SAMPLES
                + converter
                    .as_ref()
                    .map_or(0, RateConverter::max_block_output),
        );
        let (producer, consumer) = rtrb::RingBuffer::<i16>::new(device_rate as usize / 2);
        let overflow = Arc::new(AtomicU64::new(0));
        let overflow_cb = overflow.clone();
        let guard = DeviceThread::spawn("ovrilo-voice-capture", move || {
            build_input(&device, &config, producer, overflow_cb)
        })?;
        Ok(Box::new(CpalCapture {
            consumer,
            converter,
            pending,
            overflow,
            _guard: guard,
        }))
    }

    fn open_output(&mut self, id: Option<&DeviceId>) -> Result<Box<dyn PlayoutStream>, VoiceError> {
        let device = self.resolve(id, false)?;
        let config = device
            .default_output_config()
            .map_err(|e| VoiceError::device(format!("speaker format: {e}")))?;
        let device_rate = config.sample_rate();
        let converter = (device_rate != SAMPLE_RATE)
            .then(|| RateConverter::new(SAMPLE_RATE, device_rate))
            .transpose()?;
        let scratch = vec![
            0;
            converter
                .as_ref()
                .map_or(0, RateConverter::max_block_output)
        ];
        let capacity = device_rate as usize / 2;
        let (producer, consumer) = rtrb::RingBuffer::<i16>::new(capacity);
        let underruns = Arc::new(AtomicU64::new(0));
        let skipped_samples = Arc::new(AtomicU64::new(0));
        let counters = PlayoutCounters {
            underruns: underruns.clone(),
            skipped_samples: skipped_samples.clone(),
        };
        let guard = DeviceThread::spawn("ovrilo-voice-playout", move || {
            build_output(&device, &config, consumer, counters)
        })?;
        Ok(Box::new(CpalPlayout {
            producer,
            converter,
            scratch,
            capacity,
            device_rate,
            underruns,
            skipped_samples,
            _guard: guard,
        }))
    }
}

/// Keeps a cpal stream alive on its own thread until dropped.
struct DeviceThread {
    stop: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl DeviceThread {
    fn spawn(
        name: &str,
        build: impl FnOnce() -> Result<Stream, VoiceError> + Send + 'static,
    ) -> Result<Self, VoiceError> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop, stop_rx) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let stream = build().and_then(|stream| {
                    stream
                        .play()
                        .map_err(|e| VoiceError::device(format!("start device: {e}")))?;
                    Ok(stream)
                });
                match stream {
                    Ok(stream) => {
                        let _ = ready_tx.send(Ok(()));
                        // Blocks until the guard is dropped (sender closed), then stops the stream.
                        let _ = stop_rx.recv();
                        drop(stream);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|e| VoiceError::device(format!("device thread: {e}")))?;
        let guard = Self {
            stop: Some(stop),
            handle: Some(handle),
        };
        ready_rx
            .recv()
            .map_err(|_| VoiceError::device("device thread exited"))??;
        Ok(guard)
    }
}

impl Drop for DeviceThread {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct CpalCapture {
    consumer: rtrb::Consumer<i16>,
    /// Device rate → 48 kHz, when the device is not at 48 kHz.
    converter: Option<RateConverter>,
    /// Converted 48 kHz samples not yet returned as a frame.
    pending: Vec<f32>,
    overflow: Arc<AtomicU64>,
    _guard: DeviceThread,
}

impl CaptureStream for CpalCapture {
    fn read_frame(&mut self, out: &mut Frame) -> bool {
        let Some(converter) = &mut self.converter else {
            let Ok(chunk) = self.consumer.read_chunk(FRAME_SAMPLES) else {
                return false;
            };
            let (first, second) = chunk.as_slices();
            out[..first.len()].copy_from_slice(first);
            out[first.len()..].copy_from_slice(second);
            chunk.commit_all();
            return true;
        };
        let pending = &mut self.pending;
        while pending.len() < FRAME_SAMPLES {
            // Pull exactly one converter block at a time so `pending` never outgrows its capacity.
            let Ok(chunk) = self.consumer.read_chunk(converter.needed()) else {
                return false;
            };
            let (first, second) = chunk.as_slices();
            converter.push(first.iter().chain(second).map(|&s| to_f32(s)), |block| {
                pending.extend_from_slice(block)
            });
            chunk.commit_all();
        }
        for (sample, converted) in out.iter_mut().zip(pending.drain(..FRAME_SAMPLES)) {
            *sample = to_i16(converted);
        }
        true
    }

    fn overflow_samples(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

struct CpalPlayout {
    producer: rtrb::Producer<i16>,
    /// 48 kHz → device rate, when the device is not at 48 kHz.
    converter: Option<RateConverter>,
    scratch: Vec<i16>,
    capacity: usize,
    device_rate: u32,
    underruns: Arc<AtomicU64>,
    skipped_samples: Arc<AtomicU64>,
    _guard: DeviceThread,
}

/// Counters updated by the playout callback (atomic adds only).
struct PlayoutCounters {
    underruns: Arc<AtomicU64>,
    skipped_samples: Arc<AtomicU64>,
}

impl PlayoutStream for CpalPlayout {
    fn write_frame(&mut self, frame: &Frame) {
        // Drops what does not fit; the consumer trims latency, so this only happens if playout stalls.
        let Some(converter) = &mut self.converter else {
            let _ = self.producer.push_partial_slice(frame);
            return;
        };
        let (producer, scratch) = (&mut self.producer, &mut self.scratch);
        converter.push(frame.iter().map(|&s| to_f32(s)), |block| {
            for (out, &sample) in scratch.iter_mut().zip(block) {
                *out = to_i16(sample);
            }
            let _ = producer.push_partial_slice(&scratch[..block.len()]);
        });
    }

    fn buffered_ms(&self) -> u32 {
        let buffered = self.capacity - self.producer.slots();
        let converter_ms = self.converter.as_ref().map_or(0, RateConverter::delay_ms);
        (buffered as u64 * 1000 / u64::from(self.device_rate)) as u32 + converter_ms
    }

    fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    fn skipped_ms(&self) -> u64 {
        self.skipped_samples.load(Ordering::Relaxed) * 1000 / u64::from(self.device_rate)
    }
}

fn build_input(
    device: &Device,
    config: &SupportedStreamConfig,
    mut producer: rtrb::Producer<i16>,
    overflow: Arc<AtomicU64>,
) -> Result<Stream, VoiceError> {
    let channels = usize::from(config.channels()).max(1);
    let on_error = |e| eprintln!("ovrilo-voice capture error: {e}");
    let mut push = move |mono: f32| {
        if producer
            .push((mono.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
            .is_err()
        {
            overflow.fetch_add(1, Ordering::Relaxed);
        }
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config.config(),
            move |data, _| {
                for frame in data.chunks_exact(channels) {
                    push(frame.iter().sum::<f32>() / channels as f32);
                }
            },
            on_error,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config.config(),
            move |data, _| {
                for frame in data.chunks_exact(channels) {
                    push(
                        frame.iter().map(|&s| f32::from(s)).sum::<f32>()
                            / channels as f32
                            / f32::from(i16::MAX),
                    );
                }
            },
            on_error,
            None,
        ),
        other => {
            return Err(VoiceError::device(format!(
                "unsupported microphone sample format {other:?}"
            )));
        }
    };
    stream.map_err(|e| VoiceError::device(format!("open microphone: {e}")))
}

fn build_output(
    device: &Device,
    config: &SupportedStreamConfig,
    mut consumer: rtrb::Consumer<i16>,
    counters: PlayoutCounters,
) -> Result<Stream, VoiceError> {
    let channels = usize::from(config.channels()).max(1);
    let device_rate = config.sample_rate();
    let prebuffer = (device_rate * PREBUFFER_MS / 1000) as usize;
    let max_fill = (device_rate * MAX_PLAYOUT_FILL_MS / 1000) as usize;
    let mut playing = false;
    let on_error = |e| eprintln!("ovrilo-voice playout error: {e}");

    // Called once per callback, then per output frame.
    let mut next = move |callback_start: bool| -> i16 {
        if callback_start {
            let fill = consumer.slots();
            if fill > max_fill
                && let Ok(chunk) = consumer.read_chunk(fill - prebuffer)
            {
                counters
                    .skipped_samples
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                chunk.commit_all();
            }
        }
        if !playing {
            if consumer.slots() < prebuffer {
                return 0;
            }
            playing = true;
        }
        consumer.pop().unwrap_or_else(|_| {
            playing = false;
            counters.underruns.fetch_add(1, Ordering::Relaxed);
            0
        })
    };

    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_output_stream::<f32, _, _>(
            config.config(),
            move |data: &mut [f32], _| {
                for (i, frame) in data.chunks_exact_mut(channels).enumerate() {
                    frame.fill(f32::from(next(i == 0)) / f32::from(i16::MAX));
                }
            },
            on_error,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
            config.config(),
            move |data: &mut [i16], _| {
                for (i, frame) in data.chunks_exact_mut(channels).enumerate() {
                    frame.fill(next(i == 0));
                }
            },
            on_error,
            None,
        ),
        other => {
            return Err(VoiceError::device(format!(
                "unsupported speaker sample format {other:?}"
            )));
        }
    };
    stream.map_err(|e| VoiceError::device(format!("open speaker: {e}")))
}
