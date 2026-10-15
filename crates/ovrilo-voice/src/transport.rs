//! Media transport boundary (ADR-0003). The LiveKit adapter implements it; tests use a fake.
//!
//! Audio crosses this boundary in the engine's internal format (see [`crate::audio`]).

use std::future::Future;

use tokio::sync::mpsc;

use crate::audio::Frame;
use crate::contract::{
    NetworkStats, ParticipantId, ScreenShare, ScreenSource, ScreenSourceId, SharePreset, VoiceError,
};

/// Connects to rooms. One transport can create many sessions over the engine's lifetime.
pub trait Transport: Send + Sync + 'static {
    type Session: TransportSession;

    /// Connects with a short-lived token and publishes a microphone track fed by the returned sender.
    fn connect(
        &self,
        server_url: &str,
        token: &str,
    ) -> impl Future<Output = Result<Connected<Self::Session>, VoiceError>> + Send;

    /// Screens and windows that can be shared. Transports without screen sharing list none.
    fn screen_sources(&self) -> Result<Vec<ScreenSource>, VoiceError> {
        Ok(Vec::new())
    }
}

/// A successful connection.
pub struct Connected<S> {
    pub session: S,
    pub events: mpsc::UnboundedReceiver<TransportEvent>,
    /// Processed microphone frames to publish. Dropping frames on a full channel is acceptable.
    pub microphone: mpsc::Sender<Frame>,
}

/// A connected room session.
pub trait TransportSession: Send + 'static {
    /// Signals mute state to other participants (the engine also stops sending audio).
    fn set_microphone_muted(&mut self, muted: bool) -> impl Future<Output = ()> + Send;
    /// Leaves the room and releases network resources.
    fn close(self) -> impl Future<Output = ()> + Send;
    /// Current network diagnostics; transports without statistics report defaults.
    fn network_stats(&self) -> impl Future<Output = NetworkStats> + Send {
        std::future::ready(NetworkStats::default())
    }

    /// Starts sharing `source` (replacing a running share). The share ends by itself with
    /// [`TransportEvent::ScreenShareEnded`] when the source goes away or capture fails.
    fn start_screen_share(
        &mut self,
        source: &ScreenSourceId,
        preset: SharePreset,
        audio: bool,
    ) -> impl Future<Output = Result<ScreenShare, VoiceError>> + Send {
        let _ = (source, preset, audio);
        std::future::ready(Err(VoiceError::capture(
            "screen sharing is not supported by this transport",
        )))
    }

    /// Stops the local screen share, if any.
    fn stop_screen_share(&mut self) -> impl Future<Output = ()> + Send {
        std::future::ready(())
    }

    /// Subscribes to (or drops) a participant's screen-share audio; frames arrive with
    /// [`TransportEvent::RemoteScreenAudioStarted`].
    fn set_screen_audio_subscribed(
        &mut self,
        participant: &ParticipantId,
        subscribed: bool,
    ) -> impl Future<Output = ()> + Send {
        let _ = (participant, subscribed);
        std::future::ready(())
    }
}

/// Room events delivered to the engine.
#[derive(Debug)]
pub enum TransportEvent {
    ParticipantJoined {
        id: ParticipantId,
        name: String,
    },
    ParticipantLeft {
        id: ParticipantId,
    },
    /// A remote participant muted or unmuted their microphone (also sent right after joining).
    ParticipantMuted {
        id: ParticipantId,
        muted: bool,
    },
    /// A remote participant's audio became available. Frames stop when the track ends.
    RemoteAudioStarted {
        id: ParticipantId,
        frames: mpsc::Receiver<Frame>,
    },
    RemoteAudioEnded {
        id: ParticipantId,
    },
    /// A remote participant started or stopped sharing their screen.
    ParticipantScreenShare {
        id: ParticipantId,
        sharing: bool,
        audio: bool,
    },
    /// Audio of a subscribed screen share (mono 10 ms frames, like voice).
    RemoteScreenAudioStarted {
        id: ParticipantId,
        frames: mpsc::Receiver<Frame>,
    },
    RemoteScreenAudioEnded {
        id: ParticipantId,
    },
    /// The local screen share ended without the engine asking (window closed, capture failed).
    ScreenShareEnded {
        reason: String,
    },
    Reconnecting,
    Reconnected,
    /// The session ended without the engine asking (kicked, server shutdown, network loss).
    Disconnected {
        reason: String,
    },
}
