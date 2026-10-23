# ovrilo-voice

UI-independent Rust native voice engine for [Ovrilo](https://github.com/Ovrilo). Consumed by the `ovrilo` desktop client as a pinned Git revision or version.

## Status

- `crates/ovrilo-voice`: portable engine core: contract (`VoiceCommand`/`VoiceEvent`), state machine, control task, DSP thread, mixer, and the `AudioBackend`/`Transport`/`AudioProcessor` boundaries. Tested with fakes (no devices or network).
- `crates/ovrilo-voice-cpal`: cpal (WASAPI) audio backend; lock-free ring buffers, per-device threads; non-48 kHz devices are resampled (rubato FFT) on the DSP thread.
- `crates/ovrilo-voice-livekit`: LiveKit transport and WebRTC audio processing; example `voice_room` joins a room with the real engine.
- `crates/voice-spike`: Phase 02.1 development tool (not shipped) that validated LiveKit transport and app-owned microphone capture via cpal. Findings: [docs/spikes/2026-10-livekit-windows-audio.md](docs/spikes/2026-10-livekit-windows-audio.md); decision: ADR-0016 (Accepted).

## Windows build requirements

- Visual Studio 2022 17.12+ (MSVC 14.42+) with Windows SDK 10.0.26100.
- Static CRT (`.cargo/config.toml`); binaries linking this crate must also use `+crt-static`.
- ~10 GB free disk: the build downloads LiveKit's prebuilt libwebrtc.
- If microphone capture fails from an automated/sandboxed shell, re-check with `voice-spike mic-level <index>` in a normal terminal.

## Commands

```bash
cargo build
cargo test
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p voice-spike --release -- devices      # see the spike report for call/measure usage
cargo run -p ovrilo-voice-livekit --example voice_room --release -- --list
cargo run -p ovrilo-voice-livekit --example voice_room --release -- --identity alice --deafened
```

## Rules

- No Tauri, React or UI types in this repository (ADR-0010).
- Real-time audio callbacks: no blocking I/O, blocking locks, heap allocation or IPC.
- Hosts interact via `VoiceCommand` / `VoiceEvent`; events must be throttled, never raw PCM.
- Keep platform-specific code out of the core; isolate behind adapters.
- Report CPU, memory, underruns, latency and jitter with test conditions for voice changes (ADR-0009).

## License

Copyright 2026 Barış Taner. Licensed under the Apache License, Version 2.0 (see [LICENSE](LICENSE) and [NOTICE](NOTICE)).
