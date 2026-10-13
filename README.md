# ovrilo-voice

UI-independent Rust native voice engine for [Ovrilo](https://github.com/Ovrilo). Consumed by the `ovrilo` desktop client as a pinned Git revision or version.

## Status

- `crates/ovrilo-voice`: engine contract (`VoiceCommand`/`VoiceEvent`) and connection state machine.
- `crates/voice-spike`: Phase 02.1 development tool (not shipped) that validated LiveKit transport and app-owned microphone capture via cpal. Findings: [docs/spikes/2026-10-livekit-windows-audio.md](docs/spikes/2026-10-livekit-windows-audio.md); decision: ADR-0016 (Accepted).

## Windows build requirements

- Visual Studio 2022 17.12+ (MSVC 14.42+) with Windows SDK 10.0.26100.
- Static CRT (`.cargo/config.toml`); binaries linking this crate must also use `+crt-static`.
- ~10 GB free disk: the build downloads LiveKit's prebuilt libwebrtc.
- Microphone tests must run in a normal terminal (some sandboxed shells block capture).

## Commands

```bash
cargo build
cargo test
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p voice-spike --release -- devices      # see the spike report for call/measure usage
```

## Rules

- No Tauri, React or UI types in this repository (ADR-0010).
- Real-time audio callbacks: no blocking I/O, blocking locks, heap allocation or IPC.
- Hosts interact via `VoiceCommand` / `VoiceEvent`; events must be throttled, never raw PCM.
- Keep platform-specific code out of the core; isolate behind adapters.
- Report CPU, memory, underruns, latency and jitter with test conditions for voice changes (ADR-0009).
