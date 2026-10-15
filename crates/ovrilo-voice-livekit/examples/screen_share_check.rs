//! Checks screen sharing end to end through a local LiveKit (ADR-0017): a sharer session publishes
//! a screen or window with the real transport, a second session must see the share without
//! downloading its video, and a plain viewer counts the frames that arrive.
//!
//! Development only: mints tokens with the local development API key (ovrilo-infra/local).
//!
//! ```text
//! cargo run -p ovrilo-voice-livekit --example screen_share_check --release -- --list
//! cargo run -p ovrilo-voice-livekit --example screen_share_check --release -- --source window:<id> --audio --seconds 20
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use livekit::prelude::*;
use livekit::webrtc::video_stream::native::NativeVideoStream;
use livekit_api::access_token::{AccessToken, VideoGrants};
use ovrilo_voice::{
    ParticipantId, ScreenSourceId, SharePreset, Transport, TransportEvent, TransportSession,
};
use ovrilo_voice_livekit::LiveKitTransport;
use tokio::sync::mpsc;

const DEV_API_KEY: &str = "ovrilo-dev-key";
const DEV_API_SECRET: &str = "ovrilo-dev-secret-change-me-0123456789abcdef";
const URL: &str = "ws://127.0.0.1:7880";
const ROOM: &str = "ovrilo-screen-check";

fn token(identity: &str, sources: &[&str]) -> Result<String, String> {
    AccessToken::with_api_key(DEV_API_KEY, DEV_API_SECRET)
        .with_identity(identity)
        .with_name(identity)
        .with_grants(VideoGrants {
            room_join: true,
            room: ROOM.into(),
            can_publish: Some(!sources.is_empty()),
            can_subscribe: Some(true),
            can_publish_sources: sources.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| format!("token: {e}"))
}

/// Waits for the next [`TransportEvent::ParticipantScreenShare`] about `who`.
async fn next_share_event(
    events: &mut mpsc::UnboundedReceiver<TransportEvent>,
    who: &str,
) -> Result<(bool, bool), String> {
    let wait = async {
        while let Some(event) = events.recv().await {
            match event {
                TransportEvent::ParticipantScreenShare { id, sharing, audio } if id.0 == who => {
                    return Ok((sharing, audio));
                }
                TransportEvent::RemoteScreenAudioStarted { .. } => {
                    return Err("screen audio was subscribed without watching".to_owned());
                }
                _ => {}
            }
        }
        Err("event stream ended".to_owned())
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .map_err(|_| "no screen-share event within 10 s".to_owned())?
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let transport = LiveKitTransport;
    let sources = transport.screen_sources().map_err(|e| e.to_string())?;
    if args.iter().any(|a| a == "--list") || sources.is_empty() {
        for source in &sources {
            println!(
                "{:<20} {:?} {} {:?} audio={}",
                source.id, source.kind, source.title, source.application, source.audio_available
            );
        }
        return Ok(());
    }
    let source = value("--source")
        .map(ScreenSourceId)
        .unwrap_or_else(|| sources[0].id.clone());
    let audio = args.iter().any(|a| a == "--audio");
    let seconds: u64 = value("--seconds")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    // A watcher with the real transport: must learn about the share, never download its video.
    let watcher = transport
        .connect(URL, &token("watcher", &["microphone"])?)
        .await
        .map_err(|e| e.to_string())?;
    let mut watcher_events = watcher.events;

    // A plain viewer standing in for the desktop's webview: subscribes and counts frames.
    let mut options = RoomOptions::default();
    options.auto_subscribe = true;
    let (viewer, mut viewer_events) = Room::connect(URL, &token("viewer", &[])?, options)
        .await
        .map_err(|e| format!("viewer: {e}"))?;
    let frames = Arc::new(AtomicU64::new(0));
    let size = Arc::new(std::sync::Mutex::new((0, 0)));
    {
        let (frames, size) = (frames.clone(), size.clone());
        tokio::spawn(async move {
            while let Some(event) = viewer_events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track: RemoteTrack::Video(track),
                    ..
                } = event
                {
                    let (frames, size) = (frames.clone(), size.clone());
                    tokio::spawn(async move {
                        let mut stream = NativeVideoStream::new(track.rtc_track());
                        while let Some(frame) = stream.next().await {
                            frames.fetch_add(1, Ordering::Relaxed);
                            *size.lock().expect("size") =
                                (frame.buffer.width(), frame.buffer.height());
                        }
                    });
                }
            }
        });
    }

    let sharer = transport
        .connect(
            URL,
            &token(
                "sharer",
                &["microphone", "screen_share", "screen_share_audio"],
            )?,
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut session = sharer.session;
    let share = session
        .start_screen_share(&source, SharePreset::Hd720p30, audio)
        .await
        .map_err(|e| e.to_string())?;
    println!("sharing {:?}", share);

    let (sharing, with_audio) = next_share_event(&mut watcher_events, "sharer").await?;
    println!("watcher sees: sharing={sharing} audio={with_audio}");
    if !sharing {
        return Err("watcher did not see the share".into());
    }

    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let received = frames.load(Ordering::Relaxed);
    let (width, height) = *size.lock().expect("size");
    println!(
        "viewer received {received} frames in {seconds} s ({:.1} fps), last {width}x{height}",
        received as f64 / seconds as f64
    );

    let mut watcher_session = watcher.session;
    watcher_session
        .set_screen_audio_subscribed(&ParticipantId("sharer".into()), true)
        .await;
    session.stop_screen_share().await;
    let (sharing, _) = next_share_event(&mut watcher_events, "sharer").await?;
    println!("after stop, watcher sees: sharing={sharing}");

    session.close().await;
    watcher_session.close().await;
    let _ = viewer.close().await;
    if received == 0 || sharing {
        return Err("screen share check failed".into());
    }
    println!("screen share check passed");
    Ok(())
}
