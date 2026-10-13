//! LiveKit implementation of [`ovrilo_voice::Transport`] plus WebRTC audio processing (ADR-0003, ADR-0016).
//!
//! Audio crosses the boundary as 48 kHz mono 10 ms frames. The microphone is published from a
//! `NativeAudioSource` fed by the engine; remote tracks are read with `NativeAudioStream`. WebRTC's
//! platform audio device module is not used (see ADR-0016).

use std::collections::HashMap;

use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::native::apm::AudioProcessingModule;
use ovrilo_voice::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use ovrilo_voice::{
    AudioProcessor, Connected, Frame, ParticipantId, ProcessorFactory, Transport, TransportEvent,
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
}

impl Transport for LiveKitTransport {
    type Session = LiveKitSession;

    async fn connect(
        &self,
        server_url: &str,
        token: &str,
    ) -> Result<Connected<LiveKitSession>, VoiceError> {
        let (room, room_events) = Room::connect(server_url, token, RoomOptions::default())
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
        }

        let tasks = vec![
            tokio::spawn(send_microphone(source, microphone_rx)),
            tokio::spawn(forward_room_events(room_events, event_tx)),
        ];
        Ok(Connected {
            session: LiveKitSession {
                room,
                microphone: microphone_publication,
                tasks,
            },
            events,
            microphone,
        })
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

    async fn close(self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = self.room.close().await;
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
) {
    // Remote-audio readers per participant. Aborted on unsubscribe: the SFU may reuse the receiver
    // for the next participant and a stale reader would keep consuming their audio.
    let mut readers: HashMap<String, JoinHandle<()>> = HashMap::new();
    while let Some(event) = room_events.recv().await {
        let forwarded = match event {
            RoomEvent::ParticipantConnected(participant) => Some(joined(&participant)),
            RoomEvent::ParticipantDisconnected(participant) => {
                let id = participant.identity().to_string();
                if let Some(reader) = readers.remove(&id) {
                    reader.abort();
                }
                Some(TransportEvent::ParticipantLeft {
                    id: ParticipantId(id),
                })
            }
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                participant,
                ..
            } => {
                let id = participant.identity().to_string();
                let (frames_tx, frames) = mpsc::channel(REMOTE_QUEUE_FRAMES);
                if let Some(previous) =
                    readers.insert(id.clone(), tokio::spawn(read_remote(track, frames_tx)))
                {
                    previous.abort();
                }
                Some(TransportEvent::RemoteAudioStarted {
                    id: ParticipantId(id),
                    frames,
                })
            }
            RoomEvent::TrackUnsubscribed {
                track: RemoteTrack::Audio(_),
                participant,
                ..
            } => {
                let id = participant.identity().to_string();
                if let Some(reader) = readers.remove(&id) {
                    reader.abort();
                }
                Some(TransportEvent::RemoteAudioEnded {
                    id: ParticipantId(id),
                })
            }
            RoomEvent::Reconnecting => Some(TransportEvent::Reconnecting),
            RoomEvent::Reconnected => Some(TransportEvent::Reconnected),
            RoomEvent::Disconnected { reason } => Some(TransportEvent::Disconnected {
                reason: format!("{reason:?}"),
            }),
            _ => None,
        };
        if let Some(event) = forwarded
            && events.send(event).is_err()
        {
            break;
        }
    }
    for reader in readers.into_values() {
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
