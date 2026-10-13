//! Phase 02 voice spike: LiveKit Rust SDK + Windows platform audio.
//!
//! Development-only tool (not shipped). It validates that the SDK can enumerate devices,
//! capture/play audio through WebRTC's Audio Device Module (WASAPI), and carry audio through a
//! self-hosted LiveKit SFU, and it measures what arrives. Findings feed the `ovrilo-voice` engine
//! design; nothing here is a stable API.
//!
//! ```text
//! voice-spike devices
//! voice-spike call --identity alice --source tone            # publish the test tone
//! voice-spike call --identity bob --measure --seconds 60     # receive and measure it
//! voice-spike call --identity alice --source mic             # listening test (use headphones!)
//! voice-spike call --identity bob --play
//! ```

mod cpal_capture;
mod cpal_probe;
mod signal;

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::{AudioProcessingOptions, PlatformAudio};
use livekit_api::access_token::{AccessToken, VideoGrants};

use signal::{BeepDetector, SAMPLE_RATE, SAMPLES_PER_FRAME};

/// Development credentials from ovrilo-infra/local/.env.example. Never valid outside local development.
const DEV_API_KEY: &str = "ovrilo-dev-key";
const DEV_API_SECRET: &str = "ovrilo-dev-secret-change-me-0123456789abcdef";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    None,
    Tone,
    Mic,
    /// Microphone captured by us via cpal (bypasses WebRTC's Windows device module).
    CpalMic,
}

#[derive(Debug)]
struct CallArgs {
    identity: String,
    room: String,
    url: String,
    source: Source,
    play: bool,
    measure: bool,
    seconds: Option<u64>,
    input: Option<usize>,
    output: Option<usize>,
    processing: Processing,
}

/// Audio processing mode for platform audio: hardware-preferred (default), software APM, or none.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Processing {
    Hardware,
    Software,
    Off,
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("devices") => list_devices(),
        Some("mic-level") => cpal_probe::mic_level(args.get(1).and_then(|v| v.parse().ok()), 8),
        Some("call") => match parse_call(&args[1..]) {
            Ok(call_args) => call(call_args).await,
            Err(error) => Err(error),
        },
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "usage:
  voice-spike devices
  voice-spike mic-level [cpal input index]
  voice-spike call --identity <name> [--room ovrilo-spike] [--url ws://127.0.0.1:7880]
                   [--source none|tone|mic|cpal-mic] [--play] [--measure] [--seconds N]
                   [--input <recording device index>] [--output <playout device index>]
                   [--processing hw|sw|off]";

fn parse_call(args: &[String]) -> Result<CallArgs, String> {
    let mut call = CallArgs {
        identity: String::new(),
        room: "ovrilo-spike".into(),
        url: "ws://127.0.0.1:7880".into(),
        source: Source::None,
        play: false,
        measure: false,
        seconds: None,
        input: None,
        output: None,
        processing: Processing::Hardware,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--identity" => call.identity = value()?,
            "--room" => call.room = value()?,
            "--url" => call.url = value()?,
            "--source" => {
                call.source = match value()?.as_str() {
                    "none" => Source::None,
                    "tone" => Source::Tone,
                    "mic" => Source::Mic,
                    "cpal-mic" => Source::CpalMic,
                    other => return Err(format!("unknown source '{other}'")),
                }
            }
            "--play" => call.play = true,
            "--measure" => call.measure = true,
            "--seconds" => {
                call.seconds = Some(value()?.parse().map_err(|_| "--seconds must be a number")?)
            }
            "--processing" => {
                call.processing = match value()?.as_str() {
                    "hw" => Processing::Hardware,
                    "sw" => Processing::Software,
                    "off" => Processing::Off,
                    other => return Err(format!("unknown processing mode '{other}'")),
                }
            }
            "--input" => {
                call.input = Some(
                    value()?
                        .parse()
                        .map_err(|_| "--input must be a device index")?,
                )
            }
            "--output" => {
                call.output = Some(
                    value()?
                        .parse()
                        .map_err(|_| "--output must be a device index")?,
                )
            }
            other => return Err(format!("unknown argument '{other}'\n{USAGE}")),
        }
    }
    if call.identity.is_empty() {
        return Err(format!("--identity is required\n{USAGE}"));
    }
    Ok(call)
}

fn list_devices() -> Result<(), String> {
    let audio = PlatformAudio::new().map_err(|e| format!("platform audio unavailable: {e}"))?;
    println!("recording devices:");
    for device in audio.recording_devices() {
        println!(
            "  [{}] {} ({})",
            device.index,
            device.name,
            device.id.as_str()
        );
    }
    println!("playout devices:");
    for device in audio.playout_devices() {
        println!(
            "  [{}] {} ({})",
            device.index,
            device.name,
            device.id.as_str()
        );
    }
    println!(
        "processing (aec/agc/ns): {:?}/{:?}/{:?}",
        audio.active_aec_type(),
        audio.active_agc_type(),
        audio.active_ns_type()
    );
    Ok(())
}

/// Selects devices by index from `voice-spike devices`; without an index the system default is used.
fn select_devices(
    audio: &PlatformAudio,
    input: Option<usize>,
    output: Option<usize>,
) -> Result<(), String> {
    if let Some(index) = input {
        let device = audio
            .recording_devices()
            .find(|d| d.index == index)
            .ok_or_else(|| format!("no recording device with index {index}"))?;
        audio
            .set_recording_device(&device.id)
            .map_err(|e| format!("select microphone: {e}"))?;
        println!("microphone: {}", device.name);
    }
    if let Some(index) = output {
        let device = audio
            .playout_devices()
            .find(|d| d.index == index)
            .ok_or_else(|| format!("no playout device with index {index}"))?;
        audio
            .set_playout_device(&device.id)
            .map_err(|e| format!("select speaker: {e}"))?;
        println!("speaker: {}", device.name);
    }
    Ok(())
}

fn configure_processing(audio: &PlatformAudio, mode: Processing) -> Result<(), String> {
    let enabled = mode != Processing::Off;
    audio
        .configure_audio_processing(AudioProcessingOptions {
            echo_cancellation: enabled,
            noise_suppression: enabled,
            auto_gain_control: enabled,
            prefer_hardware_processing: mode == Processing::Hardware,
        })
        .map_err(|e| format!("audio processing: {e}"))?;
    println!(
        "processing {mode:?}: aec/agc/ns active {:?}/{:?}/{:?}",
        audio.active_aec_type(),
        audio.active_agc_type(),
        audio.active_ns_type()
    );
    Ok(())
}

fn dev_token(identity: &str, room: &str) -> Result<String, String> {
    let key = std::env::var("LIVEKIT_API_KEY").unwrap_or_else(|_| DEV_API_KEY.into());
    let secret = std::env::var("LIVEKIT_API_SECRET").unwrap_or_else(|_| DEV_API_SECRET.into());
    AccessToken::with_api_key(&key, &secret)
        .with_identity(identity)
        .with_ttl(Duration::from_secs(10 * 60))
        .with_grants(VideoGrants {
            room_join: true,
            room: room.into(),
            can_publish: Some(true),
            can_subscribe: Some(true),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| format!("token: {e}"))
}

async fn call(args: CallArgs) -> Result<(), String> {
    // Platform audio (WASAPI via WebRTC ADM) is only needed for the microphone or speaker playout.
    // Without it, remote audio is consumed in "synthetic" mode, which keeps measurements device-independent.
    let platform = if args.source == Source::Mic || args.play {
        Some(PlatformAudio::new().map_err(|e| format!("platform audio unavailable: {e}"))?)
    } else {
        None
    };
    if let Some(audio) = &platform {
        select_devices(audio, args.input, args.output)?;
        configure_processing(audio, args.processing)?;
    }

    // Open the microphone before connecting so device errors surface immediately.
    let cpal_capture = if args.source == Source::CpalMic {
        let (capture, feed) = cpal_capture::start(args.input)?;
        println!(
            "cpal microphone: {} ({} Hz)",
            capture.device_name, capture.sample_rate
        );
        Some((capture, feed))
    } else {
        None
    };
    let mut capture_guard = None;

    let token = dev_token(&args.identity, &args.room)?;
    let connect_started = Instant::now();
    let (room, mut events) = Room::connect(&args.url, &token, RoomOptions::default())
        .await
        .map_err(|e| format!("connect to {}: {e}", args.url))?;
    println!(
        "[{}] connected to room '{}' in {} ms",
        args.identity,
        args.room,
        connect_started.elapsed().as_millis()
    );

    let mut tone_task = None;
    match args.source {
        Source::None => {}
        Source::Tone => {
            let source = NativeAudioSource::new(
                AudioSourceOptions {
                    echo_cancellation: false,
                    noise_suppression: false,
                    auto_gain_control: false,
                },
                SAMPLE_RATE,
                1,
                100,
            );
            publish(&room, "tone", RtcAudioSource::Native(source.clone())).await?;
            tone_task = Some(tokio::spawn(run_tone(source)));
        }
        Source::CpalMic => {
            let (capture, feed) = cpal_capture.expect("cpal capture opened above");
            let source = NativeAudioSource::new(
                AudioSourceOptions {
                    echo_cancellation: false,
                    noise_suppression: false,
                    auto_gain_control: false,
                },
                capture.sample_rate,
                1,
                100,
            );
            publish(&room, "microphone", RtcAudioSource::Native(source.clone())).await?;
            // Software processing only; AEC needs the playout reference, which arrives with our own playout.
            let apm = (args.processing != Processing::Off).then(|| {
                livekit::webrtc::native::apm::AudioProcessingModule::new(false, true, true, true)
            });
            tone_task = Some(tokio::spawn(feed.feed(source, apm)));
            capture_guard = Some(capture);
        }
        Source::Mic => {
            let audio = platform.as_ref().expect("platform audio for mic");
            publish(&room, "microphone", audio.rtc_source()).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            println!(
                "microphone recording initialized: {}",
                audio.is_recording_initialized()
            );
        }
    }

    let reports: Arc<Mutex<HashMap<String, TrackReport>>> = Arc::default();
    let deadline = args
        .seconds
        .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let started = Instant::now();

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
            _ = sleep_until(deadline) => break,
        };
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            } => {
                let who = participant.identity().to_string();
                println!("[{}] subscribed to audio from {who}", args.identity);
                if args.measure {
                    tokio::spawn(measure(track, who, reports.clone()));
                }
            }
            RoomEvent::ParticipantConnected(p) => {
                println!("[{}] {} joined", args.identity, p.identity())
            }
            RoomEvent::ParticipantDisconnected(p) => {
                println!("[{}] {} left", args.identity, p.identity())
            }
            RoomEvent::ConnectionStateChanged(state) => {
                println!("[{}] connection: {state:?}", args.identity)
            }
            RoomEvent::Reconnecting => println!("[{}] reconnecting…", args.identity),
            RoomEvent::Reconnected => println!("[{}] reconnected", args.identity),
            RoomEvent::Disconnected { reason } => {
                println!("[{}] disconnected: {reason:?}", args.identity);
                break;
            }
            _ => {}
        }
    }

    if let Some(task) = tone_task {
        task.abort();
    }
    if let Some(capture) = capture_guard {
        println!(
            "capture overflow: {} samples dropped",
            capture
                .overflow_samples
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }
    let elapsed = started.elapsed();
    room.close().await.map_err(|e| format!("close: {e}"))?;
    drop(platform);

    if args.measure {
        print_reports(&args.identity, &reports.lock().expect("reports"), elapsed);
    }
    Ok(())
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn publish(room: &Room, name: &str, source: RtcAudioSource) -> Result<(), String> {
    let track = LocalAudioTrack::create_audio_track(name, source);
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(track),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                dtx: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish {name}: {e}"))?;
    println!("published '{name}'");
    Ok(())
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

/// Pushes 10 ms tone frames whose content is derived from wall-clock time, so beeps start at
/// each second boundary. `capture_frame` back-pressure paces the loop; drift beyond 200 ms is
/// corrected by resynchronizing to the clock (reported as a source stall).
async fn run_tone(source: NativeAudioSource) {
    let samples_per_ms = u64::from(SAMPLE_RATE) / 1000;
    let mut position = epoch_ms() * samples_per_ms;
    let mut buffer = [0i16; SAMPLES_PER_FRAME];
    let mut stalls = 0u32;
    loop {
        let now = epoch_ms() * samples_per_ms;
        if now > position + 200 * samples_per_ms {
            stalls += 1;
            eprintln!(
                "tone source stall #{stalls}: {} ms behind, resyncing",
                (now - position) / samples_per_ms
            );
            position = now;
        } else if position > now + 200 * samples_per_ms {
            tokio::time::sleep(Duration::from_millis(
                (position - now) / samples_per_ms - 100,
            ))
            .await;
        }
        signal::tone_frame(position, &mut buffer);
        let frame = AudioFrame {
            data: buffer.as_slice().into(),
            sample_rate: SAMPLE_RATE,
            num_channels: 1,
            samples_per_channel: SAMPLES_PER_FRAME as u32,
        };
        if let Err(error) = source.capture_frame(&frame).await {
            eprintln!("capture_frame failed: {error}");
            return;
        }
        position += SAMPLES_PER_FRAME as u64;
    }
}

#[derive(Default)]
struct TrackReport {
    frames: u64,
    samples: u64,
    first_frame_at: Option<Instant>,
    max_gap_ms: u128,
    peak_dbfs: f32,
    last_frame_at: Option<Instant>,
    detector: BeepDetector,
}

/// Consumes a remote audio track off the real-time thread and records arrival statistics.
async fn measure(
    track: RemoteAudioTrack,
    who: String,
    reports: Arc<Mutex<HashMap<String, TrackReport>>>,
) {
    let mut stream = NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, 1);
    while let Some(frame) = stream.next().await {
        let now = Instant::now();
        let arrival = epoch_ms();
        let mut reports = reports.lock().expect("reports");
        let report = reports.entry(who.clone()).or_default();
        report.frames += 1;
        report.peak_dbfs = if report.frames == 1 {
            signal::rms_dbfs(&frame.data)
        } else {
            report.peak_dbfs.max(signal::rms_dbfs(&frame.data))
        };
        report.samples += u64::from(frame.samples_per_channel);
        report.first_frame_at.get_or_insert(now);
        if let Some(last) = report.last_frame_at {
            report.max_gap_ms = report.max_gap_ms.max(now.duration_since(last).as_millis());
        }
        report.last_frame_at = Some(now);
        if let Some(latency) = report
            .detector
            .push(&frame.data, frame.sample_rate, arrival)
        {
            println!("beep from {who}: {latency} ms");
        }
    }
}

fn print_reports(identity: &str, reports: &HashMap<String, TrackReport>, elapsed: Duration) {
    if reports.is_empty() {
        println!("[{identity}] no remote audio received");
        return;
    }
    for (who, report) in reports {
        let received_seconds = report.samples as f64 / f64::from(SAMPLE_RATE);
        let listened_seconds = report
            .first_frame_at
            .zip(report.last_frame_at)
            .map(|(first, last)| last.duration_since(first).as_secs_f64())
            .unwrap_or_default();
        println!(
            "== {identity} <- {who} ({:.1} s session)",
            elapsed.as_secs_f64()
        );
        println!(
            "audio: {} frames, {:.1} s of samples over {:.1} s, max inter-frame gap {} ms, peak frame level {:.1} dBFS",
            report.frames, received_seconds, listened_seconds, report.max_gap_ms, report.peak_dbfs
        );
        match signal::latency_stats(&report.detector.latencies_ms) {
            Some(s) => {
                let expected = listened_seconds.floor() as u64;
                println!(
                    "beeps: {}/{} detected; one-way latency ms min {} / mean {:.1} / p95 {} / max {}; stddev {:.1}",
                    s.count, expected, s.min_ms, s.mean_ms, s.p95_ms, s.max_ms, s.stddev_ms
                );
            }
            None => println!("beeps: none detected"),
        }
    }
}
