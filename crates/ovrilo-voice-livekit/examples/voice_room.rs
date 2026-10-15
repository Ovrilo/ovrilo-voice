//! Joins a LiveKit room with the real engine: cpal devices + LiveKit transport + WebRTC processing.
//!
//! Development only: mints a token with the local development API key (ovrilo-infra/local). The
//! desktop app gets tokens from ovrilo-api instead (Phase 02.3).
//!
//! ```text
//! cargo run -p ovrilo-voice-livekit --example voice_room --release -- --list
//! cargo run -p ovrilo-voice-livekit --example voice_room --release -- --identity alice --deafened
//! cargo run -p ovrilo-voice-livekit --example voice_room --release -- --identity bob --muted --output 5
//! ```

use std::time::Duration;

use livekit_api::access_token::{AccessToken, VideoGrants};
use ovrilo_voice::{AudioBackend, VoiceCommand, VoiceEngine, VoiceEvent};
use ovrilo_voice_cpal::CpalBackend;
use ovrilo_voice_livekit::{LiveKitTransport, ProcessingConfig, webrtc_processing};

const DEV_API_KEY: &str = "ovrilo-dev-key";
const DEV_API_SECRET: &str = "ovrilo-dev-secret-change-me-0123456789abcdef";

#[derive(Debug)]
struct Args {
    identity: String,
    room: String,
    url: String,
    input: Option<usize>,
    output: Option<usize>,
    muted: bool,
    deafened: bool,
    processing: bool,
    seconds: Option<u64>,
    list: bool,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        identity: "dev".into(),
        room: "ovrilo-spike".into(),
        url: "ws://127.0.0.1:7880".into(),
        input: None,
        output: None,
        muted: false,
        deafened: false,
        processing: true,
        seconds: None,
        list: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--identity" => args.identity = value()?,
            "--room" => args.room = value()?,
            "--url" => args.url = value()?,
            "--input" => {
                args.input = Some(value()?.parse().map_err(|_| "--input must be an index")?)
            }
            "--output" => {
                args.output = Some(value()?.parse().map_err(|_| "--output must be an index")?)
            }
            "--seconds" => {
                args.seconds = Some(value()?.parse().map_err(|_| "--seconds must be a number")?)
            }
            "--muted" => args.muted = true,
            "--deafened" => args.deafened = true,
            "--no-processing" => args.processing = false,
            "--list" => args.list = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = parse()?;
    let backend = CpalBackend::default();
    let inputs = backend.input_devices().map_err(|e| e.to_string())?;
    let outputs = backend.output_devices().map_err(|e| e.to_string())?;
    if args.list {
        for (label, devices) in [("inputs", &inputs), ("outputs", &outputs)] {
            println!("{label}:");
            for (i, d) in devices.iter().enumerate() {
                println!(
                    "  [{i}] {}{}",
                    d.name,
                    if d.is_default { " (default)" } else { "" }
                );
            }
        }
        return Ok(());
    }

    let processing = if args.processing {
        ProcessingConfig::default()
    } else {
        ProcessingConfig {
            echo_cancellation: false,
            gain_control: false,
            high_pass: false,
            noise_suppression: ovrilo_voice::NoiseSuppression::Off,
        }
    };
    let (engine, mut events) =
        VoiceEngine::spawn(backend, LiveKitTransport, webrtc_processing(processing));

    let pick = |devices: &[ovrilo_voice::AudioDevice], index: Option<usize>| {
        index
            .map(|i| {
                devices
                    .get(i)
                    .map(|d| d.id.clone())
                    .ok_or(format!("no device {i}"))
            })
            .transpose()
    };
    engine.send(VoiceCommand::SetInputDevice(pick(&inputs, args.input)?));
    engine.send(VoiceCommand::SetOutputDevice(pick(&outputs, args.output)?));
    engine.send(VoiceCommand::SetMicrophoneMuted(args.muted));
    engine.send(VoiceCommand::SetDeafened(args.deafened));

    let token = AccessToken::with_api_key(DEV_API_KEY, DEV_API_SECRET)
        .with_identity(&args.identity)
        .with_ttl(Duration::from_secs(600))
        .with_grants(VideoGrants {
            room_join: true,
            room: args.room.clone(),
            can_publish: Some(true),
            can_subscribe: Some(true),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| e.to_string())?;
    engine.send(VoiceCommand::Join {
        server_url: args.url.clone(),
        token,
    });

    let deadline = async {
        match args.seconds {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => print_event(&args.identity, &event),
                None => break,
            },
            _ = &mut deadline => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    engine.send(VoiceCommand::Leave);
    // Let the engine report the disconnect before exiting.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = events.recv().await {
            print_event(&args.identity, &event);
            if matches!(
                event,
                VoiceEvent::ConnectionStateChanged(ovrilo_voice::ConnectionState::Disconnected)
            ) {
                break;
            }
        }
    })
    .await;
    Ok(())
}

fn print_event(identity: &str, event: &VoiceEvent) {
    println!("[{identity}] {event:?}");
}
