# ovrilo-voice

UI-independent Rust native voice engine for [Ovrilo](https://github.com/Ovrilo). Consumed by the `ovrilo` desktop client as a pinned Git revision or version.

## Status

Phase 00 skeleton: command/event contract and connection state machine only. LiveKit Rust SDK/WebRTC/Opus integration and Windows device I/O arrive in Phase 02.

## Commands

```bash
cargo build
cargo test
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

## Rules

- No Tauri, React or UI types in this repository (ADR-0010).
- Real-time audio callbacks: no blocking I/O, blocking locks, heap allocation or IPC.
- Hosts interact via `VoiceCommand` / `VoiceEvent`; events must be throttled, never raw PCM.
- Keep platform-specific code out of the core; isolate behind adapters.
- Report CPU, memory, underruns, latency and jitter with test conditions for voice changes (ADR-0009).
