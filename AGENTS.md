# ovrilo-voice — Agent Instructions

Read the workspace `../AGENTS.md` first. **Never commit, stage, push or publish without explicit user authorization.**

- Pure Rust library; no Tauri/React/UI dependencies (ADR-0003, ADR-0010).
- Public surface is `VoiceCommand` / `VoiceEvent` and engine lifecycle; breaking changes require a SemVer bump and coordination with `ovrilo`.
- Real-time audio callbacks: no blocking I/O, blocking locks, allocation or IPC.
- Use LiveKit Rust SDK/WebRTC/Opus; do not implement custom codecs, jitter buffers or NAT traversal.
- Voice changes report measured CPU, memory, underruns, latency and jitter with test conditions (ADR-0009). Mocked transports are not end-to-end evidence.
- Validate: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
