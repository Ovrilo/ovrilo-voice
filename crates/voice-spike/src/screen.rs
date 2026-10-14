//! Phase S0 screen-share spike (ADR-0017): WebRTC desktop capture (Windows Graphics Capture on
//! Windows) → I420 → LiveKit `NativeVideoSource`, optional application audio, and a viewer that
//! reports what arrives. Measures what the ADR's gate asks for: encoder in use, encode time, CPU
//! and memory, achieved frame rate and (with the test pattern) end-to-end latency.
//!
//! ```text
//! voice-spike screen-sources
//! voice-spike encoders
//! voice-spike screen-share --identity alice --source window:<id> --preset 1080p30 --app-audio game.exe
//! voice-spike screen-watch --identity bob
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use livekit::options::{
    AudioEncoding, DegradationPreference, TrackPublishOptions, VideoCodec, VideoEncoderBackend,
    VideoEncoding,
};
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::desktop_capturer::{
    CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame,
};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{
    I420Buffer, RtcVideoSource, VideoFrame, VideoResolution, VideoRotation,
};
use livekit::webrtc::stats::RtcStats;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_stream::native::NativeVideoStream;

use crate::{app_audio, dev_token, epoch_ms, pattern, signal};

const REPORT_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate: u64,
}

impl Preset {
    fn parse(name: &str) -> Result<Self, String> {
        // Bitrate caps follow LiveKit's screen-share presets; a game moves more than a document.
        match name {
            "720p30" => Ok(Self {
                width: 1280,
                height: 720,
                fps: 30,
                max_bitrate: 2_000_000,
            }),
            "1080p30" => Ok(Self {
                width: 1920,
                height: 1080,
                fps: 30,
                max_bitrate: 4_000_000,
            }),
            "1080p60" => Ok(Self {
                width: 1920,
                height: 1080,
                fps: 60,
                max_bitrate: 6_000_000,
            }),
            other => Err(format!(
                "unknown preset '{other}' (720p30, 1080p30, 1080p60)"
            )),
        }
    }

    /// Largest even size within the preset that keeps the source's aspect ratio (never upscales).
    fn fit(&self, width: u32, height: u32) -> (u32, u32) {
        let scale = (f64::from(self.width) / f64::from(width))
            .min(f64::from(self.height) / f64::from(height))
            .min(1.0);
        let even = |v: f64| ((v as u32) & !1).max(2);
        (
            even(f64::from(width) * scale),
            even(f64::from(height) * scale),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Capture {
    /// Synthetic frames carrying their capture time (platform-independent, measures latency).
    Pattern,
    Screen(u64),
    Window(u64),
}

#[derive(Debug)]
pub struct ShareArgs {
    identity: String,
    room: String,
    url: String,
    capture: Capture,
    preset: Preset,
    encoder: VideoEncoderBackend,
    codec: VideoCodec,
    degradation: Option<DegradationPreference>,
    app_audio: Option<String>,
    cursor: bool,
    seconds: Option<u64>,
}

#[derive(Debug)]
pub struct WatchArgs {
    identity: String,
    room: String,
    url: String,
    seconds: Option<u64>,
}

pub const USAGE: &str = "  voice-spike screen-sources
  voice-spike encoders
  voice-spike screen-share --identity <name> [--source pattern|screen:<id>|window:<id>]
                   [--preset 720p30|1080p30|1080p60] [--encoder auto|software|hardware|nvenc]
                   [--codec vp8|vp9|h264|av1] [--degradation resolution|framerate|balanced]
                   [--app-audio <exe name>] [--cursor] [--seconds N] [--room R] [--url U]
  voice-spike screen-watch --identity <name> [--seconds N] [--room R] [--url U]";

fn take_value<'a>(it: &mut impl Iterator<Item = &'a String>, arg: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{arg} needs a value"))
}

pub fn parse_share(args: &[String]) -> Result<ShareArgs, String> {
    let mut share = ShareArgs {
        identity: String::new(),
        room: "ovrilo-spike".into(),
        url: "ws://127.0.0.1:7880".into(),
        capture: Capture::Pattern,
        preset: Preset::parse("720p30")?,
        encoder: VideoEncoderBackend::Auto,
        codec: VideoCodec::VP8,
        degradation: None,
        app_audio: None,
        cursor: false,
        seconds: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--identity" => share.identity = take_value(&mut it, arg)?,
            "--room" => share.room = take_value(&mut it, arg)?,
            "--url" => share.url = take_value(&mut it, arg)?,
            "--source" => {
                let value = take_value(&mut it, arg)?;
                let id = |rest: &str| {
                    rest.parse::<u64>()
                        .map_err(|_| format!("bad source id in '{value}'"))
                };
                share.capture = match value.split_once(':') {
                    None if value == "pattern" => Capture::Pattern,
                    Some(("screen", rest)) => Capture::Screen(id(rest)?),
                    Some(("window", rest)) => Capture::Window(id(rest)?),
                    _ => return Err(format!("unknown source '{value}'")),
                };
            }
            "--preset" => share.preset = Preset::parse(&take_value(&mut it, arg)?)?,
            "--encoder" => {
                share.encoder = match take_value(&mut it, arg)?.as_str() {
                    "auto" => VideoEncoderBackend::Auto,
                    "software" => VideoEncoderBackend::Software,
                    "hardware" => VideoEncoderBackend::Hardware,
                    "nvenc" => VideoEncoderBackend::Nvenc,
                    other => return Err(format!("unknown encoder '{other}'")),
                }
            }
            "--codec" => {
                share.codec = match take_value(&mut it, arg)?.as_str() {
                    "vp8" => VideoCodec::VP8,
                    "vp9" => VideoCodec::VP9,
                    "h264" => VideoCodec::H264,
                    "av1" => VideoCodec::AV1,
                    other => return Err(format!("unknown codec '{other}'")),
                }
            }
            "--degradation" => {
                share.degradation = Some(match take_value(&mut it, arg)?.as_str() {
                    "resolution" => DegradationPreference::MaintainResolution,
                    "framerate" => DegradationPreference::MaintainFramerate,
                    "balanced" => DegradationPreference::Balanced,
                    other => return Err(format!("unknown degradation preference '{other}'")),
                })
            }
            "--app-audio" => share.app_audio = Some(take_value(&mut it, arg)?),
            "--cursor" => share.cursor = true,
            "--seconds" => {
                share.seconds = Some(
                    take_value(&mut it, arg)?
                        .parse()
                        .map_err(|_| "--seconds must be a number")?,
                )
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if share.identity.is_empty() {
        return Err("--identity is required".into());
    }
    Ok(share)
}

pub fn parse_watch(args: &[String]) -> Result<WatchArgs, String> {
    let mut watch = WatchArgs {
        identity: String::new(),
        room: "ovrilo-spike".into(),
        url: "ws://127.0.0.1:7880".into(),
        seconds: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--identity" => watch.identity = take_value(&mut it, arg)?,
            "--room" => watch.room = take_value(&mut it, arg)?,
            "--url" => watch.url = take_value(&mut it, arg)?,
            "--seconds" => {
                watch.seconds = Some(
                    take_value(&mut it, arg)?
                        .parse()
                        .map_err(|_| "--seconds must be a number")?,
                )
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if watch.identity.is_empty() {
        return Err("--identity is required".into());
    }
    Ok(watch)
}

/// Screens and windows WebRTC can capture, with the ids `--source` takes.
pub fn list_sources() -> Result<(), String> {
    for (label, kind) in [
        ("screen", DesktopCaptureSourceType::Screen),
        ("window", DesktopCaptureSourceType::Window),
    ] {
        let capturer = DesktopCapturer::new(DesktopCapturerOptions::new(kind))
            .ok_or_else(|| format!("no {label} capturer on this platform"))?;
        let sources = capturer.get_source_list();
        println!("{label}s ({}):", sources.len());
        for source in sources {
            println!("  {label}:{:<12} {}", source.id(), source.title());
        }
    }
    Ok(())
}

pub fn list_encoders() -> Result<(), String> {
    let backends: Vec<_> = VideoEncoderBackend::list_available().into_iter().collect();
    println!("video encoder backends available: {backends:?}");
    Ok(())
}

/// Counters shared by the capture thread and the report loop.
#[derive(Default)]
struct CaptureStats {
    frames: AtomicU64,
    temporary_errors: AtomicU64,
    permanent_errors: AtomicU64,
    /// Total microseconds spent converting/scaling to I420 (the part of the pipeline we own).
    convert_us: AtomicU64,
    last_size: Mutex<(u32, u32, u32, u32)>,
}

pub async fn share(args: ShareArgs) -> Result<(), String> {
    let token = dev_token(&args.identity, &args.room)?;
    let (room, mut events) = Room::connect(&args.url, &token, RoomOptions::default())
        .await
        .map_err(|e| format!("connect to {}: {e}", args.url))?;
    println!("[{}] connected to '{}'", args.identity, args.room);

    let preset = args.preset;
    let source = NativeVideoSource::new(
        VideoResolution {
            width: preset.width,
            height: preset.height,
        },
        true,
    );
    let track =
        LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source.clone()));
    room.local_participant()
        .publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                video_codec: args.codec,
                video_encoder: args.encoder,
                video_encoding: Some(VideoEncoding {
                    max_bitrate: preset.max_bitrate,
                    max_framerate: f64::from(preset.fps),
                }),
                simulcast: false,
                degradation_preference: args.degradation,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish screen: {e}"))?;
    println!(
        "published screen: {:?}, {}x{}@{} max {} kbps, codec {:?}, encoder {:?}",
        args.capture,
        preset.width,
        preset.height,
        preset.fps,
        preset.max_bitrate / 1000,
        args.codec,
        args.encoder
    );

    let stats = Arc::new(CaptureStats::default());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let capture_thread = {
        let (stats, stop, source, capture, cursor) = (
            stats.clone(),
            stop.clone(),
            source.clone(),
            args.capture,
            args.cursor,
        );
        std::thread::Builder::new()
            .name("screen-capture".into())
            .spawn(move || run_capture(capture, cursor, preset, source, &stats, &stop))
            .map_err(|e| format!("capture thread: {e}"))?
    };

    let mut app_audio_guard = None;
    let audio_level = Arc::new(Mutex::new(f32::NEG_INFINITY));
    if let Some(exe) = &args.app_audio {
        let (audio, mut frames) = app_audio::start(exe)?;
        println!(
            "capturing audio of '{exe}' (pid {}, with child processes)",
            audio.process_id
        );
        let audio_source = NativeAudioSource::new(
            // Stream audio is not speech: no echo cancellation, noise suppression or gain control.
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            app_audio::SAMPLE_RATE,
            app_audio::CHANNELS,
            100,
        );
        let audio_track = LocalAudioTrack::create_audio_track(
            "screen-audio",
            RtcAudioSource::Native(audio_source.clone()),
        );
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(audio_track),
                TrackPublishOptions {
                    source: TrackSource::ScreenshareAudio,
                    dtx: false,
                    red: false,
                    audio_encoding: Some(AudioEncoding {
                        max_bitrate: 128_000,
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| format!("publish screen audio: {e}"))?;
        let level = audio_level.clone();
        tokio::spawn(async move {
            while let Some(samples) = frames.recv().await {
                {
                    let mut peak = level.lock().expect("level");
                    *peak = peak.max(signal::rms_dbfs(&samples));
                }
                let frame = AudioFrame {
                    samples_per_channel: (samples.len() / app_audio::CHANNELS as usize) as u32,
                    data: samples.into(),
                    sample_rate: app_audio::SAMPLE_RATE,
                    num_channels: app_audio::CHANNELS,
                };
                if let Err(error) = audio_source.capture_frame(&frame).await {
                    eprintln!("screen audio capture_frame failed: {error}");
                    return;
                }
            }
        });
        app_audio_guard = Some(audio);
    }

    let mut usage = ProcessUsage::new();
    let mut previous = Snapshot::default();
    let deadline = args
        .seconds
        .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut ticker = tokio::time::interval(REPORT_EVERY);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let rtc = track.get_stats().await.unwrap_or_default();
                previous = report_share(&stats, &rtc, &previous, &mut usage);
                if let Some(audio) = &app_audio_guard {
                    let mut peak = audio_level.lock().expect("level");
                    println!(
                        "  app audio: peak {:.1} dBFS, {} frames dropped",
                        *peak,
                        audio.dropped_frames.load(Ordering::Relaxed)
                    );
                    *peak = f32::NEG_INFINITY;
                }
            }
            event = events.recv() => if event.is_none() { break },
            _ = tokio::signal::ctrl_c() => break,
            _ = crate::sleep_until(deadline) => break,
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = capture_thread.join();
    drop(app_audio_guard);
    room.close().await.map_err(|e| format!("close: {e}"))?;
    Ok(())
}

/// Captures at the preset frame rate on one thread: WebRTC desktop capturers must be driven from
/// the thread that created them. The callback converts to I420 and hands the frame to WebRTC.
fn run_capture(
    capture: Capture,
    cursor: bool,
    preset: Preset,
    source: NativeVideoSource,
    stats: &Arc<CaptureStats>,
    stop: &std::sync::atomic::AtomicBool,
) {
    let interval = Duration::from_secs_f64(1.0 / f64::from(preset.fps));
    let mut desktop = match capture {
        Capture::Pattern => None,
        Capture::Screen(id) | Capture::Window(id) => {
            let kind = if matches!(capture, Capture::Screen(_)) {
                DesktopCaptureSourceType::Screen
            } else {
                DesktopCaptureSourceType::Window
            };
            let mut options = DesktopCapturerOptions::new(kind);
            options.set_include_cursor(cursor);
            let Some(mut capturer) = DesktopCapturer::new(options) else {
                eprintln!("could not create a desktop capturer");
                return;
            };
            let Some(target) = capturer
                .get_source_list()
                .into_iter()
                .find(|s| s.id() == id)
            else {
                eprintln!("source {id} not found; run `voice-spike screen-sources`");
                return;
            };
            println!("capturing '{}'", target.title());
            let (stats, source) = (stats.clone(), source.clone());
            capturer.start_capture(
                Some(target),
                move |result: Result<DesktopFrame, CaptureError>| match result {
                    Ok(frame) => deliver_desktop_frame(&frame, preset, &source, &stats),
                    Err(CaptureError::Temporary) => {
                        stats.temporary_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(CaptureError::Permanent) => {
                        stats.permanent_errors.fetch_add(1, Ordering::Relaxed);
                    }
                },
            );
            Some(capturer)
        }
    };

    let mut next = Instant::now();
    let mut index = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match desktop.as_mut() {
            Some(capturer) => capturer.capture_frame(),
            None => deliver_pattern_frame(preset, index, &source, stats),
        }
        index += 1;
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Fell behind (slow capture): do not try to catch up with a burst.
            next = now;
        }
    }
}

fn deliver_desktop_frame(
    frame: &DesktopFrame,
    preset: Preset,
    source: &NativeVideoSource,
    stats: &CaptureStats,
) {
    let started = Instant::now();
    let (width, height) = (frame.width().max(0) as u32, frame.height().max(0) as u32);
    if width < 2 || height < 2 {
        return;
    }
    // WebRTC desktop frames are 32-bit BGRA in memory, which libyuv calls ARGB.
    let mut full = I420Buffer::new(width, height);
    let (stride_y, stride_u, stride_v) = full.strides();
    let (y, u, v) = full.data_mut();
    yuv_helper::argb_to_i420(
        frame.data(),
        frame.stride(),
        y,
        stride_y,
        u,
        stride_u,
        v,
        stride_v,
        width as i32,
        height as i32,
    );
    let (out_width, out_height) = preset.fit(width, height);
    let buffer = if (out_width, out_height) == (width, height) {
        full
    } else {
        full.scale(out_width as i32, out_height as i32)
    };
    push(
        source,
        buffer,
        stats,
        started,
        (width, height, out_width, out_height),
    );
}

fn deliver_pattern_frame(
    preset: Preset,
    index: u64,
    source: &NativeVideoSource,
    stats: &CaptureStats,
) {
    let started = Instant::now();
    let (width, height) = (preset.width, preset.height);
    let mut buffer = I420Buffer::new(width, height);
    let (stride_y, _, _) = buffer.strides();
    let (y, u, v) = buffer.data_mut();
    pattern::draw(y, stride_y, width, height, epoch_ms(), index);
    u.fill(128);
    v.fill(128);
    push(
        source,
        buffer,
        stats,
        started,
        (width, height, width, height),
    );
}

fn push(
    source: &NativeVideoSource,
    buffer: I420Buffer,
    stats: &CaptureStats,
    started: Instant,
    size: (u32, u32, u32, u32),
) {
    let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
    frame.timestamp_us = epoch_ms() as i64 * 1000;
    source.capture_frame(&frame);
    stats.frames.fetch_add(1, Ordering::Relaxed);
    stats
        .convert_us
        .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    *stats.last_size.lock().expect("size") = size;
}

/// Previous counter values, to turn cumulative stats into per-interval rates.
#[derive(Default)]
struct Snapshot {
    at: Option<Instant>,
    captured: u64,
    convert_us: u64,
    bytes: u64,
    frames_encoded: u32,
    encode_time: f64,
}

fn report_share(
    stats: &CaptureStats,
    rtc: &[RtcStats],
    previous: &Snapshot,
    usage: &mut ProcessUsage,
) -> Snapshot {
    let now = Instant::now();
    let seconds = previous.at.map_or(REPORT_EVERY.as_secs_f64(), |at| {
        now.duration_since(at).as_secs_f64()
    });
    let captured = stats.frames.load(Ordering::Relaxed);
    let convert_us = stats.convert_us.load(Ordering::Relaxed);
    let frames = captured - previous.captured;
    let (src_w, src_h, out_w, out_h) = *stats.last_size.lock().expect("size");
    println!(
        "capture: {:.1} fps, {src_w}x{src_h} -> {out_w}x{out_h}, convert {:.2} ms/frame, errors temporary {} permanent {}",
        frames as f64 / seconds,
        if frames > 0 {
            (convert_us - previous.convert_us) as f64 / frames as f64 / 1000.0
        } else {
            0.0
        },
        stats.temporary_errors.load(Ordering::Relaxed),
        stats.permanent_errors.load(Ordering::Relaxed),
    );

    let mut next = Snapshot {
        at: Some(now),
        captured,
        convert_us,
        ..Snapshot::default()
    };
    let outbound = rtc.iter().find_map(|s| match s {
        RtcStats::OutboundRtp(o) if o.stream.kind == "video" => Some(o),
        _ => None,
    });
    match outbound {
        Some(o) => {
            let encoded =
                o.outbound.frames_encoded - previous.frames_encoded.min(o.outbound.frames_encoded);
            let encode_ms = if encoded > 0 {
                (o.outbound.total_encode_time - previous.encode_time) * 1000.0 / f64::from(encoded)
            } else {
                0.0
            };
            println!(
                "encoder: {} (power efficient {}), {}x{} @ {:.1} fps, {:.0} kbps (target {:.0}), encode {:.2} ms/frame, keyframes {}, limited by {:?}",
                o.outbound.encoder_implementation,
                o.outbound.power_efficient_encoder,
                o.outbound.frame_width,
                o.outbound.frame_height,
                o.outbound.frames_per_second,
                (o.sent.bytes_sent - previous.bytes.min(o.sent.bytes_sent)) as f64 * 8.0
                    / seconds
                    / 1000.0,
                o.outbound.target_bitrate / 1000.0,
                encode_ms,
                o.outbound.key_frames_encoded,
                o.outbound.quality_limitation_reason,
            );
            next.bytes = o.sent.bytes_sent;
            next.frames_encoded = o.outbound.frames_encoded;
            next.encode_time = o.outbound.total_encode_time;
        }
        None => println!("encoder: no outbound video stats yet"),
    }
    usage.report();
    next
}

#[derive(Default)]
struct ViewStats {
    frames: u64,
    width: u32,
    height: u32,
    buffer_type: String,
    latencies_ms: Vec<i64>,
}

pub async fn watch(args: WatchArgs) -> Result<(), String> {
    let token = dev_token(&args.identity, &args.room)?;
    let (room, mut events) = Room::connect(&args.url, &token, RoomOptions::default())
        .await
        .map_err(|e| format!("connect to {}: {e}", args.url))?;
    println!(
        "[{}] connected to '{}', waiting for a screen share",
        args.identity, args.room
    );

    let view = Arc::new(Mutex::new(ViewStats::default()));
    let audio_peak = Arc::new(Mutex::new(f32::NEG_INFINITY));
    let mut video_track: Option<RemoteVideoTrack> = None;
    let mut usage = ProcessUsage::new();
    let mut previous_frames = 0u64;
    let mut previous_bytes = 0u64;
    let deadline = args
        .seconds
        .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut ticker = tokio::time::interval(REPORT_EVERY);
    ticker.tick().await;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(RoomEvent::TrackSubscribed { track: RemoteTrack::Video(track), publication, participant }) => {
                    println!("watching {:?} from {}", publication.source(), participant.identity());
                    let view = view.clone();
                    let stream = NativeVideoStream::new(track.rtc_track());
                    tokio::spawn(consume_video(stream, view));
                    video_track = Some(track);
                }
                Some(RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(track), publication, participant })
                    if publication.source() == TrackSource::ScreenshareAudio =>
                {
                    println!("hearing screen audio from {}", participant.identity());
                    let peak = audio_peak.clone();
                    let mut stream = NativeAudioStream::new(track.rtc_track(), app_audio::SAMPLE_RATE as i32, app_audio::CHANNELS as i32);
                    tokio::spawn(async move {
                        while let Some(frame) = stream.next().await {
                            let mut level = peak.lock().expect("peak");
                            *level = level.max(signal::rms_dbfs(&frame.data));
                        }
                    });
                }
                Some(_) => {}
                None => break,
            },
            _ = ticker.tick() => {
                let (frames, latencies, size, buffer_type) = {
                    let mut view = view.lock().expect("view");
                    (view.frames, std::mem::take(&mut view.latencies_ms), (view.width, view.height), view.buffer_type.clone())
                };
                println!(
                    "received: {:.1} fps, {}x{} ({buffer_type}){}",
                    (frames - previous_frames) as f64 / REPORT_EVERY.as_secs_f64(),
                    size.0,
                    size.1,
                    latency_summary(latencies),
                );
                previous_frames = frames;
                if let Some(track) = &video_track {
                    let rtc = track.get_stats().await.unwrap_or_default();
                    if let Some(i) = rtc.iter().find_map(|s| match s { RtcStats::InboundRtp(i) if i.stream.kind == "video" => Some(i), _ => None }) {
                        let bytes = i.inbound.bytes_received;
                        let decoded = i.inbound.frames_decoded.max(1);
                        let emitted = i.inbound.jitter_buffer_emitted_count.max(1) as f64;
                        println!(
                            "decoder: {} (power efficient {}), {:.0} kbps, decode {:.2} ms/frame (avg), jitter buffer {:.0} ms (avg), dropped {}, freezes {}, packets lost {}",
                            i.inbound.decoder_implementation,
                            i.inbound.power_efficient_decoder,
                            (bytes - previous_bytes.min(bytes)) as f64 * 8.0 / REPORT_EVERY.as_secs_f64() / 1000.0,
                            i.inbound.total_decode_time * 1000.0 / f64::from(decoded),
                            i.inbound.jitter_buffer_delay * 1000.0 / emitted,
                            i.inbound.frames_dropped,
                            i.inbound.freeze_count,
                            i.received.packets_lost,
                        );
                        previous_bytes = bytes;
                    }
                }
                let mut peak = audio_peak.lock().expect("peak");
                if peak.is_finite() {
                    println!("  screen audio: peak {:.1} dBFS", *peak);
                    *peak = f32::NEG_INFINITY;
                }
                drop(peak);
                usage.report();
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = crate::sleep_until(deadline) => break,
        }
    }
    room.close().await.map_err(|e| format!("close: {e}"))?;
    Ok(())
}

/// Counts decoded frames and reads the test pattern's timestamp (I420 frames only).
async fn consume_video(mut stream: NativeVideoStream, view: Arc<Mutex<ViewStats>>) {
    while let Some(frame) = stream.next().await {
        let arrived = epoch_ms();
        let mut view = view.lock().expect("view");
        view.frames += 1;
        view.width = frame.buffer.width();
        view.height = frame.buffer.height();
        match frame.buffer.as_i420() {
            Some(i420) => {
                view.buffer_type = "i420".into();
                let (stride_y, _, _) = i420.strides();
                let (y, _, _) = i420.data();
                if let Some(sent) = pattern::read(y, stride_y, view.width, view.height) {
                    view.latencies_ms
                        .push(pattern::truncate(arrived) as i64 - sent as i64);
                }
            }
            None => view.buffer_type = format!("{:?}", frame.buffer.buffer_type()),
        }
    }
}

fn latency_summary(mut latencies: Vec<i64>) -> String {
    if latencies.is_empty() {
        return String::new();
    }
    latencies.sort_unstable();
    let pick = |q: f64| latencies[((latencies.len() - 1) as f64 * q).round() as usize];
    format!(
        ", pattern latency ms min {} / median {} / p95 {} / max {}",
        latencies[0],
        pick(0.5),
        pick(0.95),
        latencies[latencies.len() - 1]
    )
}

/// CPU (all cores = 100 % per core, like Task Manager's per-process column × cores) and memory of this process.
struct ProcessUsage {
    system: sysinfo::System,
    pid: sysinfo::Pid,
}

impl ProcessUsage {
    fn new() -> Self {
        let mut usage = Self {
            system: sysinfo::System::new(),
            pid: sysinfo::Pid::from_u32(std::process::id()),
        };
        usage.refresh();
        usage
    }

    fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[self.pid]),
            false,
            sysinfo::ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory(),
        );
    }

    fn report(&mut self) {
        self.refresh();
        if let Some(process) = self.system.process(self.pid) {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            println!(
                "process: cpu {:.1} % of one core ({:.1} % of {cores} cores), memory {:.0} MB",
                process.cpu_usage(),
                process.cpu_usage() / cores as f32,
                process.memory() as f64 / 1_048_576.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_fit_without_upscaling_and_keep_even_sizes() {
        let p = Preset::parse("720p30").unwrap();
        assert_eq!(p.fit(2560, 1440), (1280, 720));
        assert_eq!(p.fit(3440, 1440), (1280, 534));
        assert_eq!(p.fit(800, 601), (800, 600));
        assert!(Preset::parse("4k").is_err());
    }

    #[test]
    fn parses_share_arguments() {
        let args: Vec<String> = [
            "--identity",
            "a",
            "--source",
            "window:42",
            "--preset",
            "1080p60",
            "--encoder",
            "nvenc",
            "--codec",
            "h264",
            "--app-audio",
            "game.exe",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let share = parse_share(&args).unwrap();

        assert_eq!(share.capture, Capture::Window(42));
        assert_eq!(share.preset.fps, 60);
        assert_eq!(share.encoder, VideoEncoderBackend::Nvenc);
        assert_eq!(share.app_audio.as_deref(), Some("game.exe"));
        assert!(
            parse_share(&[
                "--source".into(),
                "tab:1".into(),
                "--identity".into(),
                "a".into()
            ])
            .is_err()
        );
    }
}
