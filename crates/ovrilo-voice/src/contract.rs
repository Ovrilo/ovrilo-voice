//! Host-facing contract: commands in, events out (docs/development/CONTRACTS.md §3).
//!
//! Enums are `#[non_exhaustive]` so variants can be added without a breaking change; hosts must
//! handle unknown variants gracefully.

use std::fmt;

use crate::ConnectionState;

/// Opaque, stable identifier of an audio device (platform GUID or backend-specific ID).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceId(pub String);

/// An input or output device as reported by the audio backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    pub id: DeviceId,
    pub name: String,
    pub is_default: bool,
}

/// Identity of a remote participant in the current room (the SFU participant identity).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParticipantId(pub String);

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Coarse control requests sent by the host to the engine. Never carries audio frames.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VoiceCommand {
    /// Join a room using a short-lived grant issued by the API after authorization.
    Join {
        server_url: String,
        token: String,
    },
    /// Leave the current room. Idempotent.
    Leave,
    SetMicrophoneMuted(bool),
    /// Deafened stops playout and also silences the microphone while active.
    SetDeafened(bool),
    /// Selects the microphone (`None` = system default). Applied immediately when in a room.
    SetInputDevice(Option<DeviceId>),
    /// Selects the speaker/headphones (`None` = system default). Applied immediately when in a room.
    SetOutputDevice(Option<DeviceId>),
    /// Re-enumerates devices and emits [`VoiceEvent::DevicesChanged`].
    RefreshDevices,
}

/// Notifications emitted by the engine. The engine is authoritative for this state (ADR-0011).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum VoiceEvent {
    ConnectionStateChanged(ConnectionState),
    MicrophoneMutedChanged(bool),
    DeafenedChanged(bool),
    /// Whether a microphone is open and feeding the room (false when listen-only).
    MicrophoneActiveChanged(bool),
    ParticipantJoined {
        id: ParticipantId,
        name: String,
    },
    ParticipantLeft {
        id: ParticipantId,
    },
    DevicesChanged {
        inputs: Vec<AudioDevice>,
        outputs: Vec<AudioDevice>,
    },
    Error {
        code: VoiceErrorCode,
        message: String,
    },
    /// Voice quality diagnostics, about once per second while in a room.
    StatsUpdated(VoiceStats),
    /// Someone started or stopped talking (`participant: None` is the local user). Reported on
    /// transitions only; muted or deafened local audio never counts as speaking.
    SpeakingChanged {
        participant: Option<ParticipantId>,
        speaking: bool,
    },
}

/// Diagnostics for the current room. Counters are cumulative since joining.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct VoiceStats {
    /// Audio queued for the speaker right now.
    pub playout_buffered_ms: u32,
    /// Times the speaker ran out of audio (heard as gaps).
    pub playout_underruns: u64,
    /// Audio skipped to bound latency when playout fell behind (heard as jumps).
    pub playout_skipped_ms: u64,
    /// Microphone audio lost because the engine did not drain capture in time.
    pub capture_overflow_ms: u64,
    /// Remote 10 ms frames dropped because a participant's queue was full.
    pub remote_frames_dropped: u64,
    /// Microphone 10 ms frames dropped because the network side was behind.
    pub microphone_frames_dropped: u64,
    pub network: NetworkStats,
}

/// Network-side diagnostics reported by the transport. Counters are cumulative.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct NetworkStats {
    /// Round-trip time to the media server.
    pub round_trip_ms: Option<u32>,
    pub inbound_packets_received: u64,
    pub inbound_packets_lost: u64,
    /// Largest inbound jitter across remote participants.
    pub inbound_jitter_ms: Option<u32>,
    /// Remote audio synthesized to hide loss or late packets (heard as robotic voice).
    pub inbound_concealed_ms: u64,
    /// Our packets the server reports as lost.
    pub outbound_packets_lost: u64,
}

/// Stable, host-facing error categories. Hosts branch on the code, never on the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VoiceErrorCode {
    InvalidState,
    PermissionDenied,
    DeviceUnavailable,
    TransportUnavailable,
    Internal,
}

/// Error returned by backends and transports, surfaced to hosts as [`VoiceEvent::Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceError {
    pub code: VoiceErrorCode,
    pub message: String,
}

impl VoiceError {
    pub fn new(code: VoiceErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn device(message: impl Into<String>) -> Self {
        Self::new(VoiceErrorCode::DeviceUnavailable, message)
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(VoiceErrorCode::TransportUnavailable, message)
    }
}

impl fmt::Display for VoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for VoiceError {}

impl From<VoiceError> for VoiceEvent {
    fn from(error: VoiceError) -> Self {
        VoiceEvent::Error {
            code: error.code,
            message: error.message,
        }
    }
}
