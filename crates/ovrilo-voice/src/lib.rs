//! Ovrilo native voice engine.
//!
//! This crate is independent of Tauri, React and any UI framework (ADR-0003, ADR-0010).
//! Hosts drive the engine through [`VoiceCommand`]s and observe it through [`VoiceEvent`]s.
//! Media transport (LiveKit/WebRTC) is added in Phase 02 behind this interface.

mod state;

pub use state::{ConnectionState, InvalidTransition};

/// Version of this crate, reported to hosts for compatibility diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Coarse control requests sent by the host to the engine. Never carries audio frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceCommand {
    /// Join a room using a short-lived grant issued by the API after authorization.
    Join {
        server_url: String,
        token: String,
    },
    /// Leave the current room. Idempotent.
    Leave,
    SetMicrophoneMuted(bool),
    SetDeafened(bool),
}

/// Throttled notifications emitted by the engine to the host.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceEvent {
    ConnectionStateChanged(ConnectionState),
    MicrophoneMutedChanged(bool),
    DeafenedChanged(bool),
    Error {
        code: VoiceErrorCode,
        message: String,
    },
}

/// Stable, host-facing error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceErrorCode {
    InvalidState,
    PermissionDenied,
    DeviceUnavailable,
    TransportUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_crate_version() {
        assert_eq!(VERSION, "0.1.0");
    }
}
