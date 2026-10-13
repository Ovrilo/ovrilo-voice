# Spike: LiveKit Rust SDK and Windows Audio (Phase 02.1)

**Date:** 2026-10-13/14
**Tool:** `crates/voice-spike` (development only, not shipped)
**Outcome:** LiveKit transport validated; WebRTC's Windows capture rejected; app-owned capture via cpal validated. Decision: ADR-0016 (Accepted).

## Environment

| Item | Value |
|---|---|
| CPU | Intel Core i5-12400F, 12 logical processors |
| OS | Windows 11 Pro 10.0.26200 |
| Toolchain | Rust 1.99 (MSVC), Visual Studio 17.14.42 (MSVC 14.44), Windows SDK 10.0.26100 |
| SDK | `livekit` 0.9.5 (prebuilt libwebrtc `webrtc-89d790b`), `livekit-api` 0.9.0, `cpal` 0.18.2 |
| SFU | `livekit/livekit-server:v1.13.9` via `ovrilo-infra/local` (Docker Desktop, same machine) |
| Audio devices | USB Audio CODEC mic/headphones; Voicemeeter and VB-Cable virtual devices installed |
| Topology | Two client processes on the same machine, same room, no network impairment |

## Build requirements found

- MSVC **14.42+** (tested 14.44): LiveKit's prebuilt libwebrtc links MSVC STL vectorized algorithms (`__std_search_1`, …) missing from 14.41.
- Windows SDK **10.0.26100**: `webrtc-sys` compiles with `NTDDI_WIN11_GE`.
- **Static CRT**: `.cargo/config.toml` sets `+crt-static` for `x86_64-pc-windows-msvc`. Every binary linking `ovrilo-voice` (including the Tauri host) must match.
- `livekit` must keep the `native` feature (network transport) when default features are disabled.
- Building downloads LiveKit's prebuilt libwebrtc from `github.com/livekit/rust-sdks` releases; debug + release targets need ~8–10 GB free disk.

## Results

### Transport (synthetic tone, no audio devices), release build, 70 s

| Metric | Value |
|---|---|
| Frames / gaps | 7000 frames, max inter-frame gap 12 ms |
| Level | −14.9 dBFS received for a −15 dBFS RMS tone (no level loss) |
| Beeps | 70/70 detected |
| One-way latency (encode + SFU + jitter buffer, localhost) | mean 48.9 ms, p95 49 ms, max 69 ms, stddev 2.5 ms |
| Publisher process | CPU avg 0.39 % (max 1.03 %), private 10.3 MB, working set 66 MB |
| Subscriber process | CPU avg 0.28 % (max 1.03 %), private 9.8 MB, working set 65 MB |

CPU is a share of the whole machine (12 threads). Working set is dominated by shared DLL pages. Release binary: 29 MB.

### PlatformAudio (WebRTC Windows ADM)

| Scenario | Result |
|---|---|
| Device enumeration/selection | OK (10 inputs, 13 outputs) |
| Playout to USB headphones | OK (user heard test tone) |
| Mic capture, built-in AEC (default) | Near-silence (−62 dBFS). Log: Voice Capture DSP opened "capture device index 5" at 16 kHz mono for selected index 4 |
| Mic capture, software processing | Publishing re-enables built-in AEC (`EnableBuiltInAEC(1)` from the voice engine) → same silence |
| Mic capture, built-in AEC hidden via local `webrtc-sys` patch | Software path active, recording initialized, still −92 to −109 dBFS |
| Reconfigure processing while recording | Deadlock |
| Shutdown | Some sessions did not exit within 15 s |
| With mic, release, 74 s | Publisher CPU avg 0.47 %, private 11.3 MB; subscriber CPU avg 0.15 %, private 10.0 MB |

### cpal capture → NativeAudioSource (user listening test, headphones)

| Item | Value |
|---|---|
| Capture | USB mic, 48 kHz stereo F32, downmixed to mono i16, 10 ms frames, APM AGC + HPF + NS |
| Received level | peak −3.1 dBFS while speaking |
| Continuity | 50.2 s of audio over 50.2 s, max inter-frame gap 13 ms, 0 ring-buffer overflow samples |
| User assessment | Own voice clearly audible; latency not disturbing; no robotic artifacts or dropouts; keyboard/mouse clicks still audible |

### cpal playout + APM echo cancellation (user terminal, headphones placed next to the mic)

Pipeline: remote `NativeAudioStream` → DSP thread (`ProcessReverseStream`) → playout ring (40 ms pre-buffer, trim above 100 ms) → cpal output; cpal input → ring → DSP thread (`ProcessStream`) → `NativeAudioSource`. A tone publisher's beep plays through bob's speaker into bob's mic; a third client measures bob's published audio. Metric: median of per-second peak 440 Hz amplitude.

| Bob processing | Echo (440 Hz) leaked | Beeps detected in bob's stream | Peak level |
|---|---|---|---|
| off | −29.8 dBFS | 19/24 | −12.7 dBFS |
| AEC only | −38.6 dBFS | 2/24 | −30.1 dBFS |
| AEC + AGC + NS + HPF | −31.3 dBFS | 10/24 | −14.8 dBFS |

- Playout: mean ring fill 30–38 ms, 0–1 underruns per 25 s, no trims, no overflow; DSP: 0 APM errors, 0 dropped frames.
- Echo path delay (`voice-spike echo-delay 4 5`): 116 ms from cpal playback timestamp to capture timestamp (USB codec internal latency); total reverse-to-capture ≈ 170–180 ms including the playout ring.
- Assessment: AEC is active but weak on this setup (~9 dB on a pure tone). AGC re-amplifies residual echo. Tonal test signals and headphones next to the mic are poor AEC stimuli; tuning and acceptance move to the two-PC test (02.2) and audio quality profiles (Phase 07).
- Found and fixed: the SFU reuses a receiver for the next participant; remote-audio sinks must be detached on `TrackUnsubscribed`, otherwise they keep reading the next participant's audio.

## Pitfalls

- **Claude Code's command sandbox blocks microphone capture** on this machine (`REGDB_E_CLASSNOTREG`, `0x800705AA`); tests needing a microphone must run in a normal terminal. Playout and networking work in the sandbox.
- Windows built-in AEC hides capture failures as silence; the engine must not use it and must surface device errors explicitly.
- WebRTC stationary noise suppression does not remove keyboard/mouse transients.
- The tone beep detector reports false "beeps" on speech; latency figures are valid only for the tone source.

## Not yet validated

- Two physical PCs over a real network (LAN/WAN, TURN).
- AEC quality in a real speaker scenario (needs two PCs); AGC/AEC interaction; multi-participant mixing (spike plays only the first remote).
- Device hot-plug, sample-rate conversion for non-48 kHz devices, long sessions, 4–8 participants.
- Reconnection and token expiry; API-issued room tokens (milestone 02.3).

## Reproduce

```bash
# from ovrilo-voice, with ovrilo-infra/local running
cargo build -p voice-spike --release
target/release/voice-spike devices
target/release/voice-spike call --identity alice --source tone --seconds 80
target/release/voice-spike call --identity bob --measure --seconds 70
pwsh scripts/measure-processes.ps1 -Name voice-spike -Seconds 60
# microphone (own terminal, headphones):
target/release/voice-spike call --identity bob --play --output <headphones index> --measure --seconds 60
target/release/voice-spike call --identity alice --source cpal-mic --input <cpal input index> --processing sw --seconds 50
# echo test: tone publisher + measuring client + bob with cpal mic and cpal speaker
target/release/voice-spike call --identity bob --source cpal-mic --input 4 --playback cpal --output 5 --processing aec --seconds 25
target/release/voice-spike echo-delay <cpal input index> <cpal output index>
```
