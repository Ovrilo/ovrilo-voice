//! Media transport boundary (ADR-0003). The LiveKit adapter implements it; tests use a fake.
//!
//! Audio crosses this boundary in the engine's internal format (see [`crate::audio`]).

use std::future::Future;

use tokio::sync::mpsc;

use crate::audio::Frame;
use crate::contract::{ParticipantId, VoiceError};

/// Connects to rooms. One transport can create many sessions over the engine's lifetime.
pub trait Transport: Send + Sync + 'static {
    type Session: TransportSession;

    /// Connects with a short-lived token and publishes a microphone track fed by the returned sender.
    fn connect(
        &self,
        server_url: &str,
        token: &str,
    ) -> impl Future<Output = Result<Connected<Self::Session>, VoiceError>> + Send;
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
    /// A remote participant's audio became available. Frames stop when the track ends.
    RemoteAudioStarted {
        id: ParticipantId,
        frames: mpsc::Receiver<Frame>,
    },
    RemoteAudioEnded {
        id: ParticipantId,
    },
    Reconnecting,
    Reconnected,
    /// The session ended without the engine asking (kicked, server shutdown, network loss).
    Disconnected {
        reason: String,
    },
}
