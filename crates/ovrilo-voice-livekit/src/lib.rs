//! LiveKit implementation of [`ovrilo_voice::Transport`] plus WebRTC audio processing (ADR-0003, ADR-0016).
//!
//! Audio crosses the boundary as 48 kHz mono 10 ms frames. The microphone is published from a
//! `NativeAudioSource` fed by the engine; remote tracks are read with `NativeAudioStream`. WebRTC's
//! platform audio device module is not used (see ADR-0016).
//!
//! Subscriptions are explicit: remote microphones always, a screen share's audio only while the
//! user watches it, and screen-share video never (the desktop's webview watches video, ADR-0017).

mod app_audio;
mod screen;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use livekit::options::{AudioEncoding, TrackPublishOptions};
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::native::apm::AudioProcessingModule;
use livekit::webrtc::prelude::RtcVideoSource;
use livekit::webrtc::stats::{IceCandidatePairState, RtcStats};
use livekit::webrtc::video_source::native::NativeVideoSource;
use ovrilo_voice::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use ovrilo_voice::{
    AudioProcessor, Connected, Frame, NetworkStats, ParticipantId, ProcessorFactory, ScreenShare,
    ScreenSource, ScreenSourceId, ShareOptimization, SharePreset, Transport, TransportEvent,
    TransportSession, VoiceError,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Remote frames buffered per participant before the engine's mixer drops the oldest.
const REMOTE_QUEUE_FRAMES: usize = 20;
/// Microphone frames buffered toward the network (200 ms).
const MICROPHONE_QUEUE_FRAMES: usize = 20;

#[derive(Debug, Default, Clone)]
pub struct LiveKitTransport;

pub struct LiveKitSession {
    room: Room,
    microphone: LocalTrackPublication,
    tasks: Vec<JoinHandle<()>>,
    events: mpsc::UnboundedSender<TransportEvent>,
    /// Identities whose screen-share audio the user watches (kept for shares published later).
    watched: Arc<Mutex<HashSet<String>>>,
    share: Option<LocalShare>,
}

/// The local screen share: published tracks plus the capture threads feeding them.
struct LocalShare {
    video: LocalTrackPublication,
    audio: Option<LocalTrackPublication>,
    // Dropped (stopped) before the tracks are unpublished.
    capture: Option<screen::Capture>,
    app_audio: Option<app_audio::AppAudio>,
    sender: Option<JoinHandle<()>>,
}

impl Transport for LiveKitTransport {
    type Session = LiveKitSession;

    async fn connect(
        &self,
        server_url: &str,
        token: &str,
    ) -> Result<Connected<LiveKitSession>, VoiceError> {
        let mut options = RoomOptions::default();
        options.auto_subscribe = false;
        let (room, room_events) = Room::connect(server_url, token, options)
            .await
            .map_err(|e| VoiceError::transport(format!("connect: {e}")))?;

        let source = NativeAudioSource::new(
            // Processing happens in the engine's DSP thread, not in the source.
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            SAMPLE_RATE,
            1,
            100,
        );
        let track = LocalAudioTrack::create_audio_track(
            "microphone",
            RtcAudioSource::Native(source.clone()),
        );
        let microphone_publication = room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| VoiceError::transport(format!("publish microphone: {e}")))?;

        let (microphone, microphone_rx) = mpsc::channel(MICROPHONE_QUEUE_FRAMES);
        let (event_tx, events) = mpsc::unbounded_channel();

        // Participants already in the room.
        for participant in room.remote_participants().values() {
            let _ = event_tx.send(joined(participant));
            let _ = event_tx.send(microphone_muted(participant));
            for publication in participant.track_publications().values() {
                if publication.source() == TrackSource::Microphone {
                    publication.set_subscribed(true);
                }
            }
            let (sharing, audio) = screen_share_state(participant, None);
            if sharing {
                let _ = event_tx.send(TransportEvent::ParticipantScreenShare {
                    id: ParticipantId(participant.identity().to_string()),
                    sharing,
                    audio,
                });
            }
        }

        let watched = Arc::new(Mutex::new(HashSet::new()));
        let tasks = vec![
            tokio::spawn(send_microphone(source, microphone_rx)),
            tokio::spawn(forward_room_events(
                room_events,
                event_tx.clone(),
                watched.clone(),
            )),
        ];
        Ok(Connected {
            session: LiveKitSession {
                room,
                microphone: microphone_publication,
                tasks,
                events: event_tx,
                watched,
                share: None,
            },
            events,
            microphone,
        })
    }

    fn screen_sources(&self) -> Result<Vec<ScreenSource>, VoiceError> {
        screen::list_sources()
    }
}

impl LiveKitSession {
    async fn publish_share(
        &mut self,
        source: &ScreenSourceId,
        preset: SharePreset,
        optimization: ShareOptimization,
        audio: bool,
    ) -> Result<ScreenShare, VoiceError> {
        let target = screen::Target::parse(source)?;
        let encoding = screen::Encoding::of(preset, optimization);
        let video_source = NativeVideoSource::new(encoding.resolution(), true);
        let capture = screen::start(target, encoding, video_source.clone(), self.events.clone())?;
        let title = capture.title.clone();
        let track =
            LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(video_source));
        let video = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: encoding.codec,
                    video_encoding: Some(encoding.video_encoding()),
                    degradation_preference: Some(encoding.degradation),
                    simulcast: false,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| VoiceError::capture(format!("publish screen: {e}")))?;
        let mut share = LocalShare {
            video,
            audio: None,
            capture: Some(capture),
            app_audio: None,
            sender: None,
        };

        // Sound is best effort: the share goes on without it (reported as `audio: false`).
        if audio && let screen::Target::Window(window) = target {
            let _ = self.publish_app_audio(window, &mut share).await;
        }
        let shared_audio = share.audio.is_some();
        self.share = Some(share);
        Ok(ScreenShare {
            source: source.clone(),
            title,
            preset,
            optimization,
            audio: shared_audio,
        })
    }

    async fn publish_app_audio(
        &mut self,
        window: u64,
        share: &mut LocalShare,
    ) -> Result<(), VoiceError> {
        let process = app_audio::window_processes()
            .remove(&window)
            .ok_or_else(|| VoiceError::capture("the shared window's application was not found"))?;
        let (capture, frames) = app_audio::start(process.id)?;
        let source = NativeAudioSource::new(
            // Stream sound is not speech: no echo cancellation, noise suppression or gain control.
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            app_audio::SAMPLE_RATE,
            app_audio::CHANNELS,
            100,
        );
        let track = LocalAudioTrack::create_audio_track(
            "screen-audio",
            RtcAudioSource::Native(source.clone()),
        );
        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track),
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
            .map_err(|e| VoiceError::capture(format!("publish screen audio: {e}")))?;
        share.audio = Some(publication);
        share.app_audio = Some(capture);
        share.sender = Some(tokio::spawn(send_app_audio(source, frames)));
        Ok(())
    }

    async fn unpublish_share(&mut self) {
        let Some(mut share) = self.share.take() else {
            return;
        };
        // Stop feeding the tracks first, then remove them from the room.
        share.capture = None;
        share.app_audio = None;
        if let Some(sender) = share.sender.take() {
            sender.abort();
        }
        let participant = self.room.local_participant();
        if let Some(audio) = share.audio.take() {
            let _ = participant.unpublish_track(&audio.sid()).await;
        }
        let _ = participant.unpublish_track(&share.video.sid()).await;
    }
}

impl TransportSession for LiveKitSession {
    async fn set_microphone_muted(&mut self, muted: bool) {
        if muted {
            self.microphone.mute();
        } else {
            self.microphone.unmute();
        }
    }

    async fn close(mut self) {
        self.unpublish_share().await;
        for task in &self.tasks {
            task.abort();
        }
        let _ = self.room.close().await;
    }

    async fn network_stats(&self) -> NetworkStats {
        match self.room.get_stats().await {
            Ok(session) => network_stats(&session.publisher_stats, &session.subscriber_stats),
            Err(_) => NetworkStats::default(),
        }
    }

    async fn start_screen_share(
        &mut self,
        source: &ScreenSourceId,
        preset: SharePreset,
        optimization: ShareOptimization,
        audio: bool,
    ) -> Result<ScreenShare, VoiceError> {
        self.unpublish_share().await;
        let result = self
            .publish_share(source, preset, optimization, audio)
            .await;
        if result.is_err() {
            self.unpublish_share().await;
        }
        result
    }

    async fn stop_screen_share(&mut self) {
        self.unpublish_share().await;
    }

    async fn set_screen_audio_subscribed(&mut self, participant: &ParticipantId, subscribed: bool) {
        {
            let mut watched = self.watched.lock().expect("watched");
            if subscribed {
                watched.insert(participant.0.clone());
            } else {
                watched.remove(&participant.0);
            }
        }
        let remote = self.room.remote_participants();
        let Some(remote) = remote
            .values()
            .find(|p| p.identity().as_str() == participant.0)
        else {
            return;
        };
        for publication in remote.track_publications().values() {
            if publication.source() == TrackSource::ScreenshareAudio {
                publication.set_subscribed(subscribed);
            }
        }
    }
}

/// Whether a participant shares their screen, and with sound. `ignoring` leaves out a publication
/// that is being removed.
fn screen_share_state(
    participant: &RemoteParticipant,
    ignoring: Option<&TrackSid>,
) -> (bool, bool) {
    let publications = participant.track_publications();
    let has = |source: TrackSource| {
        publications
            .values()
            .any(|p| p.source() == source && Some(&p.sid()) != ignoring)
    };
    let sharing = has(TrackSource::Screenshare);
    (sharing, sharing && has(TrackSource::ScreenshareAudio))
}

fn screen_share_event(
    participant: &RemoteParticipant,
    ignoring: Option<&TrackSid>,
) -> TransportEvent {
    let (sharing, audio) = screen_share_state(participant, ignoring);
    TransportEvent::ParticipantScreenShare {
        id: ParticipantId(participant.identity().to_string()),
        sharing,
        audio,
    }
}

async fn send_app_audio(source: NativeAudioSource, mut frames: mpsc::Receiver<Vec<i16>>) {
    while let Some(samples) = frames.recv().await {
        let audio = AudioFrame {
            samples_per_channel: (samples.len() / app_audio::CHANNELS as usize) as u32,
            data: samples.into(),
            sample_rate: app_audio::SAMPLE_RATE,
            num_channels: app_audio::CHANNELS,
        };
        if source.capture_frame(&audio).await.is_err() {
            return;
        }
    }
}

/// Summarizes WebRTC statistics: inbound audio from remote tracks, outbound loss as reported back
/// by the server, and the round-trip time of the active candidate pair. Depending on the SDK's
/// peer-connection mode, entries can appear in either list, so both are scanned (once per id).
fn network_stats(publisher: &[RtcStats], subscriber: &[RtcStats]) -> NetworkStats {
    let mut stats = NetworkStats::default();
    let mut jitter_s: Option<f64> = None;
    let mut pair_rtt: Option<(bool, f64)> = None;
    let mut remote_rtt: Option<f64> = None;
    let mut seen = HashSet::new();
    for entry in publisher.iter().chain(subscriber) {
        match entry {
            RtcStats::InboundRtp(rtp)
                if rtp.stream.kind == "audio" && seen.insert(rtp.rtc.id.as_str()) =>
            {
                stats.inbound_packets_received += rtp.received.packets_received;
                stats.inbound_packets_lost += rtp.received.packets_lost.max(0) as u64;
                jitter_s = Some(jitter_s.unwrap_or(0.0).max(rtp.received.jitter));
                // Concealment during DTX silence is inaudible; count only audible concealment.
                let audible = rtp
                    .inbound
                    .concealed_samples
                    .saturating_sub(rtp.inbound.silent_concealed_samples);
                stats.inbound_concealed_ms += audible * 1000 / u64::from(SAMPLE_RATE);
            }
            RtcStats::RemoteInboundRtp(remote)
                if remote.stream.kind == "audio" && seen.insert(remote.rtc.id.as_str()) =>
            {
                stats.outbound_packets_lost += remote.received.packets_lost.max(0) as u64;
                if remote.remote_inbound.round_trip_time > 0.0 {
                    remote_rtt = Some(remote.remote_inbound.round_trip_time);
                }
            }
            // The SDK does not always flag the active pair as nominated; a succeeded pair is
            // in use too. Prefer nominated pairs.
            RtcStats::CandidatePair(pair)
                if pair.candidate_pair.current_round_trip_time > 0.0
                    && (pair.candidate_pair.nominated
                        || pair.candidate_pair.state == Some(IceCandidatePairState::Succeeded)) =>
            {
                let candidate = (
                    pair.candidate_pair.nominated,
                    pair.candidate_pair.current_round_trip_time,
                );
                if pair_rtt.is_none_or(|(nominated, _)| candidate.0 && !nominated) {
                    pair_rtt = Some(candidate);
                }
            }
            _ => {}
        }
    }
    // ICE round trip is the network RTT; RTCP-based RTT also includes server processing.
    stats.round_trip_ms = pair_rtt
        .map(|(_, rtt)| rtt)
        .or(remote_rtt)
        .map(|rtt| (rtt * 1000.0) as u32);
    stats.inbound_jitter_ms = jitter_s.map(|s| (s * 1000.0) as u32);
    stats
}

/// A participant without a published microphone counts as not muted (still connecting).
fn microphone_muted(participant: &RemoteParticipant) -> TransportEvent {
    let muted = participant
        .track_publications()
        .values()
        .any(|p| p.source() == TrackSource::Microphone && p.is_muted());
    TransportEvent::ParticipantMuted {
        id: ParticipantId(participant.identity().to_string()),
        muted,
    }
}

fn mute_event(
    participant: &Participant,
    publication: &TrackPublication,
    muted: bool,
) -> Option<TransportEvent> {
    match participant {
        Participant::Remote(remote) if publication.source() == TrackSource::Microphone => {
            Some(TransportEvent::ParticipantMuted {
                id: ParticipantId(remote.identity().to_string()),
                muted,
            })
        }
        _ => None,
    }
}

fn joined(participant: &RemoteParticipant) -> TransportEvent {
    let id = participant.identity().to_string();
    let name = participant.name();
    TransportEvent::ParticipantJoined {
        name: if name.is_empty() { id.clone() } else { name },
        id: ParticipantId(id),
    }
}

async fn send_microphone(source: NativeAudioSource, mut frames: mpsc::Receiver<Frame>) {
    while let Some(frame) = frames.recv().await {
        let audio = AudioFrame {
            data: frame.as_slice().into(),
            sample_rate: SAMPLE_RATE,
            num_channels: 1,
            samples_per_channel: FRAME_SAMPLES as u32,
        };
        if source.capture_frame(&audio).await.is_err() {
            return;
        }
    }
}

async fn forward_room_events(
    mut room_events: mpsc::UnboundedReceiver<RoomEvent>,
    events: mpsc::UnboundedSender<TransportEvent>,
    watched: Arc<Mutex<HashSet<String>>>,
) {
    // Remote-audio readers per participant: voice and screen-share sound apart. Aborted on
    // unsubscribe: the SFU may reuse the receiver for the next participant and a stale reader
    // would keep consuming their audio.
    let mut voices: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut streams: HashMap<String, JoinHandle<()>> = HashMap::new();
    let forward = |event: TransportEvent| events.send(event).is_ok();
    while let Some(event) = room_events.recv().await {
        let open = match event {
            RoomEvent::ParticipantConnected(participant) => forward(joined(&participant)),
            RoomEvent::ParticipantDisconnected(participant) => {
                let id = participant.identity().to_string();
                for readers in [&mut voices, &mut streams] {
                    if let Some(reader) = readers.remove(&id) {
                        reader.abort();
                    }
                }
                forward(TransportEvent::ParticipantLeft {
                    id: ParticipantId(id),
                })
            }
            RoomEvent::TrackPublished {
                publication,
                participant,
            } => match publication.source() {
                TrackSource::Microphone => {
                    publication.set_subscribed(true);
                    true
                }
                TrackSource::Screenshare => forward(screen_share_event(&participant, None)),
                TrackSource::ScreenshareAudio => {
                    let id = participant.identity().to_string();
                    if watched.lock().expect("watched").contains(&id) {
                        publication.set_subscribed(true);
                    }
                    forward(screen_share_event(&participant, None))
                }
                _ => true,
            },
            RoomEvent::TrackUnpublished {
                publication,
                participant,
            } => match publication.source() {
                TrackSource::Screenshare | TrackSource::ScreenshareAudio => {
                    forward(screen_share_event(&participant, Some(&publication.sid())))
                }
                _ => true,
            },
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                publication,
                participant,
            } => {
                let id = participant.identity().to_string();
                let (frames_tx, frames) = mpsc::channel(REMOTE_QUEUE_FRAMES);
                let reader = tokio::spawn(read_remote(track, frames_tx));
                let (readers, started) = match publication.source() {
                    TrackSource::ScreenshareAudio => (
                        &mut streams,
                        TransportEvent::RemoteScreenAudioStarted {
                            id: ParticipantId(id.clone()),
                            frames,
                        },
                    ),
                    _ => (
                        &mut voices,
                        TransportEvent::RemoteAudioStarted {
                            id: ParticipantId(id.clone()),
                            frames,
                        },
                    ),
                };
                if let Some(previous) = readers.insert(id, reader) {
                    previous.abort();
                }
                forward(started)
            }
            RoomEvent::TrackUnsubscribed {
                track: RemoteTrack::Audio(_),
                publication,
                participant,
            } => {
                let id = participant.identity().to_string();
                let screen = publication.source() == TrackSource::ScreenshareAudio;
                let readers = if screen { &mut streams } else { &mut voices };
                if let Some(reader) = readers.remove(&id) {
                    reader.abort();
                }
                let id = ParticipantId(id);
                forward(if screen {
                    TransportEvent::RemoteScreenAudioEnded { id }
                } else {
                    TransportEvent::RemoteAudioEnded { id }
                })
            }
            RoomEvent::TrackMuted {
                participant,
                publication,
            } => mute_event(&participant, &publication, true).is_none_or(forward),
            RoomEvent::TrackUnmuted {
                participant,
                publication,
            } => mute_event(&participant, &publication, false).is_none_or(forward),
            RoomEvent::Reconnecting => forward(TransportEvent::Reconnecting),
            RoomEvent::Reconnected => forward(TransportEvent::Reconnected),
            RoomEvent::Disconnected { reason } => forward(TransportEvent::Disconnected {
                reason: format!("{reason:?}"),
            }),
            _ => true,
        };
        if !open {
            break;
        }
    }
    for reader in voices.into_values().chain(streams.into_values()) {
        reader.abort();
    }
}

/// Re-chunks the remote stream into exact 10 ms frames for the engine.
async fn read_remote(track: RemoteAudioTrack, frames: mpsc::Sender<Frame>) {
    let mut stream = NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, 1);
    let mut frame: Frame = [0; FRAME_SAMPLES];
    let mut filled = 0;
    while let Some(audio) = stream.next().await {
        for &sample in audio.data.iter() {
            frame[filled] = sample;
            filled += 1;
            if filled == FRAME_SAMPLES {
                filled = 0;
                // A full queue means the engine is behind; it drops the oldest frames itself.
                if frames.try_send(frame).is_err() && frames.is_closed() {
                    return;
                }
            }
        }
    }
}

/// Which WebRTC audio processing stages to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessingConfig {
    pub echo_cancellation: bool,
    pub gain_control: bool,
    pub high_pass: bool,
    pub noise_suppression: bool,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            gain_control: true,
            high_pass: true,
            noise_suppression: true,
        }
    }
}

/// Factory for the engine: creates WebRTC's Audio Processing Module on the DSP thread.
pub fn webrtc_processing(config: ProcessingConfig) -> ProcessorFactory {
    Box::new(move || {
        Box::new(WebRtcProcessor {
            apm: AudioProcessingModule::new(
                config.echo_cancellation,
                config.gain_control,
                config.high_pass,
                config.noise_suppression,
            ),
            echo_cancellation: config.echo_cancellation,
        })
    })
}

struct WebRtcProcessor {
    apm: AudioProcessingModule,
    echo_cancellation: bool,
}

impl AudioProcessor for WebRtcProcessor {
    fn process_render(&mut self, frame: &mut Frame) {
        let _ = self
            .apm
            .process_reverse_stream(frame, SAMPLE_RATE as i32, 1);
    }

    fn process_capture(&mut self, frame: &mut Frame, playout_delay_ms: u32) {
        if self.echo_cancellation {
            let _ = self.apm.set_stream_delay_ms(playout_delay_ms as i32);
        }
        let _ = self.apm.process_stream(frame, SAMPLE_RATE as i32, 1);
    }
}

#[cfg(test)]
mod tests {
    use livekit::webrtc::stats::{CandidatePairStats, InboundRtpStats, RemoteInboundRtpStats};

    use super::*;

    fn inbound(
        kind: &str,
        received: u64,
        lost: i64,
        jitter: f64,
        concealed: u64,
        silent: u64,
    ) -> RtcStats {
        let mut rtp = InboundRtpStats::default();
        rtp.rtc.id = format!("in-{kind}-{received}");
        rtp.stream.kind = kind.into();
        rtp.received.packets_received = received;
        rtp.received.packets_lost = lost;
        rtp.received.jitter = jitter;
        rtp.inbound.concealed_samples = concealed;
        rtp.inbound.silent_concealed_samples = silent;
        RtcStats::InboundRtp(rtp)
    }

    #[test]
    fn summarizes_audio_streams_and_round_trip() {
        let mut pair = CandidatePairStats::default();
        pair.candidate_pair.nominated = true;
        pair.candidate_pair.current_round_trip_time = 0.045;
        let subscriber = [
            inbound("audio", 1000, 10, 0.004, 4800 + 960, 960),
            inbound("audio", 500, -1, 0.012, 0, 0),
            inbound("video", 9999, 99, 0.5, 0, 0),
            RtcStats::CandidatePair(pair),
        ];
        let mut remote = RemoteInboundRtpStats::default();
        remote.stream.kind = "audio".into();
        remote.received.packets_lost = 3;

        let stats = network_stats(&[RtcStats::RemoteInboundRtp(remote)], &subscriber);
        let single_pc = network_stats(&[subscriber.as_slice(), &[]].concat(), &[]);
        assert_eq!(
            single_pc.inbound_packets_received, 1500,
            "entries in the publisher list count"
        );

        assert_eq!(stats.inbound_packets_received, 1500);
        assert_eq!(
            stats.inbound_packets_lost, 10,
            "negative loss (duplicates) counts as 0"
        );
        assert_eq!(stats.inbound_jitter_ms, Some(12), "worst participant");
        assert_eq!(
            stats.inbound_concealed_ms, 100,
            "silent concealment excluded"
        );
        assert_eq!(stats.outbound_packets_lost, 3);
        assert_eq!(stats.round_trip_ms, Some(45));
    }

    #[test]
    fn empty_stats_report_nothing() {
        let stats = network_stats(&[], &[]);
        assert_eq!(stats, NetworkStats::default());
    }
}
