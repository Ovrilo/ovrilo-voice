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

/// Identifies a screen or window that can be shared, as listed by [`VoiceEvent::ScreenSourcesListed`].
/// Opaque to hosts; valid until the window closes or the display changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ScreenSourceId(pub String);

impl fmt::Display for ScreenSourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScreenSourceKind {
    Screen,
    Window,
}

/// Something that can be shared (ADR-0017). Ovrilo's own windows are never listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSource {
    pub id: ScreenSourceId,
    pub kind: ScreenSourceKind,
    pub title: String,
    /// Executable of the window's application (for example `chrome.exe`); `None` for screens.
    pub application: Option<String>,
    /// The application's sound can be shared with it (windows on Windows 10 2004 or later).
    pub audio_available: bool,
}

/// Resolution and frame-rate cap of a screen share. The source is scaled down to fit, never up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SharePreset {
    #[default]
    Hd720p30,
    FullHd1080p30,
    FullHd1080p60,
}

/// What a screen share keeps when the network or the computer cannot keep up, and the codec
/// chosen for that kind of picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ShareOptimization {
    /// Text, code and applications: stays sharp and lowers the frame rate first.
    #[default]
    Detail,
    /// Games and video: stays smooth and lowers the resolution first.
    Motion,
}

/// How the microphone is cleaned before it is sent (ADR-0018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum NoiseSuppression {
    /// Microphone audio is sent as captured (echo cancellation and gain control still run).
    Off,
    /// WebRTC's built-in suppressor: steady noise such as fans and hum.
    #[default]
    Standard,
    /// A neural suppressor plus a voice gate: also keyboard and mouse clicks and room noise, and
    /// silence between words.
    Enhanced,
}

/// The local user's screen share while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenShare {
    pub source: ScreenSourceId,
    pub title: String,
    pub preset: SharePreset,
    pub optimization: ShareOptimization,
    /// The application's sound is shared too.
    pub audio: bool,
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
    /// Push-to-talk mode: the microphone transmits only while the key is held (plus a short
    /// release tail). Off means open microphone (voice activity).
    SetPushToTalk(bool),
    /// State of the push-to-talk key, from the host's key watcher. Ignored when push-to-talk is off.
    SetPushToTalkPressed(bool),
    /// Local playback volume for one participant, 0–200 % (100 = unchanged; values above 200
    /// are clamped). Kept for the engine's lifetime, so it also applies when they join later.
    SetParticipantVolume {
        participant: ParticipantId,
        percent: u16,
    },
    /// Volume of the local microphone as sent, 0–200 % (100 = unchanged; values above 200 are
    /// clamped). Applied after echo cancellation, noise suppression and gain control, so gain
    /// control does not undo it. Kept for the engine's lifetime.
    SetInputVolume(u16),
    /// Volume of everything played (all participants and streams), 0–200 % (clamped). Kept for
    /// the engine's lifetime.
    SetOutputVolume(u16),
    /// Chooses microphone noise suppression. Kept for the engine's lifetime. The mode actually
    /// running is reported by [`VoiceEvent::NoiseSuppressionChanged`]: Enhanced can fall back to
    /// Standard when it is unavailable or this computer cannot keep up.
    SetNoiseSuppression(NoiseSuppression),
    /// Lists screens and windows that can be shared ([`VoiceEvent::ScreenSourcesListed`]).
    ListScreenSources,
    /// Shares a screen or window in the current room, optionally with the window's application
    /// sound. Replaces a running share.
    StartScreenShare {
        source: ScreenSourceId,
        preset: SharePreset,
        optimization: ShareOptimization,
        audio: bool,
    },
    /// Stops sharing. Idempotent.
    StopScreenShare,
    /// Plays (or stops playing) a participant's screen-share sound. Their video is watched by the
    /// host itself; the engine never downloads screen-share video.
    WatchScreenShare {
        participant: ParticipantId,
        watching: bool,
    },
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
    /// A remote participant's own microphone mute (as published to the room).
    ParticipantMutedChanged {
        participant: ParticipantId,
        muted: bool,
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
    PushToTalkChanged(bool),
    ParticipantVolumeChanged {
        participant: ParticipantId,
        percent: u16,
    },
    /// Whether microphone audio is currently allowed out (not muted, not deafened, and the
    /// push-to-talk key held when push-to-talk is on). Reported on changes.
    TransmittingChanged(bool),
    /// Someone started or stopped talking (`participant: None` is the local user). Reported on
    /// transitions only; muted or deafened local audio never counts as speaking.
    SpeakingChanged {
        participant: Option<ParticipantId>,
        speaking: bool,
    },
    ScreenSourcesListed(Vec<ScreenSource>),
    InputVolumeChanged(u16),
    OutputVolumeChanged(u16),
    /// The noise suppression now running. Sent after [`VoiceCommand::SetNoiseSuppression`] and when
    /// Enhanced falls back to Standard on its own.
    NoiseSuppressionChanged(NoiseSuppression),
    /// The local screen share started, changed or stopped (`None`), including when the shared
    /// window closed or capture failed (an [`VoiceEvent::Error`] says why).
    ScreenShareChanged(Option<ScreenShare>),
    /// A remote participant started or stopped sharing their screen (also sent right after joining).
    ParticipantScreenShareChanged {
        participant: ParticipantId,
        sharing: bool,
        /// Their share includes application sound.
        audio: bool,
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
    /// Time spent cleaning each 10 ms microphone frame (echo cancellation, noise suppression),
    /// smoothed over about a second.
    pub capture_processing_us: u32,
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
    /// Screen sharing failed: unsupported platform, source gone, capture or encoder error.
    CaptureUnavailable,
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

    pub fn capture(message: impl Into<String>) -> Self {
        Self::new(VoiceErrorCode::CaptureUnavailable, message)
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
