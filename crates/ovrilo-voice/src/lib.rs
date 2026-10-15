//! Ovrilo native voice engine.
//!
//! This crate is independent of Tauri, React and any UI framework (ADR-0003, ADR-0010) and of
//! concrete audio/network stacks: devices come from an [`AudioBackend`] (cpal adapter) and media
//! from a [`Transport`] (LiveKit adapter), per ADR-0016. Hosts drive the engine through
//! [`VoiceCommand`]s and observe it through [`VoiceEvent`]s.

pub mod audio;
mod contract;
mod dsp;
mod engine;
mod mixer;
mod speaking;
mod state;
pub mod transport;

pub use audio::{
    AudioBackend, AudioProcessor, CaptureStream, Frame, NoProcessing, PlayoutStream,
    ProcessorFactory,
};
pub use contract::{
    AudioDevice, DeviceId, NetworkStats, ParticipantId, ScreenShare, ScreenSource, ScreenSourceId,
    ScreenSourceKind, ShareOptimization, SharePreset, VoiceCommand, VoiceError, VoiceErrorCode,
    VoiceEvent, VoiceStats,
};
pub use engine::{VoiceEngine, VoiceEngineHandle};
pub use mixer::Mixer;
pub use state::{ConnectionState, InvalidTransition};
pub use transport::{Connected, Transport, TransportEvent, TransportSession};

/// Version of this crate, reported to hosts for compatibility diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_crate_version() {
        assert_eq!(VERSION, "0.1.0");
    }
}
