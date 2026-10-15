# Spike: Screen Sharing with Application Audio (ADR-0017, S0)

**Date:** 2026-10-14 (in progress)
**Tool:** `crates/voice-spike` (development only, not shipped): `screen-sources`, `encoders`, `screen-share`, `screen-watch`
**Outcome:** pending Windows measurements. Decides whether the native video path in ADR-0017 stands or video falls back to the webview.

## Questions

1. Does WebRTC's desktop capture (Windows Graphics Capture, DXGI fallback) capture the windows and games we care about, including borderless/fullscreen games?
2. Which encoder does LiveKit's bundled libwebrtc use on our Windows PCs (`encoder_implementation`), is a hardware encoder available (`encoders`, `--encoder hardware|nvenc`), and what does encoding cost in CPU and per-frame time?
3. Does sharing while playing cost game FPS, and does voice stay clean (no audio glitches in a parallel voice call)?
4. Does WASAPI process loopback capture only the chosen application's sound (no Ovrilo voice, no notifications), on our Windows versions?
5. End-to-end latency and quality at 720p30, 1080p30, 1080p60.

## How the tool works

- **Capture:** `libwebrtc::desktop_capturer` (LiveKit's libwebrtc), which on 64-bit Windows enables the WGC window/screen capturers with DXGI allowed. One thread drives it at the preset frame rate; frames (BGRA) are converted to I420 with libyuv and scaled down to the preset (never up), then given to a `NativeVideoSource` (`is_screencast = true`) published as `Screenshare` with simulcast off.
- **Test pattern** (`--source pattern`): synthetic frames that carry their send time in a row of 16 px blocks; `screen-watch` reads it back and reports latency (capture-to-decode, excluding display). Platform-independent, so the transport path is also tested on Linux.
- **Application audio** (`--app-audio <exe>`): WASAPI process loopback (crate `wasapi`), including the process tree of the oldest process with that executable name, 48 kHz stereo, published as `ScreenshareAudio` (no AEC/NS/AGC, no DTX/RED, 128 kbps cap).
- **Reports every 5 s:** capture fps and conversion time; encoder implementation, sent resolution/fps/bitrate, encode ms/frame, quality limitation reason; viewer fps, decoder, decode time, jitter buffer delay, drops/freezes; process CPU and memory (sysinfo; "of one core" like `top`, also divided by core count like Task Manager).

## Results on Linux (cloud container, synthetic pattern only)

Not the reference machine: a 4-vCPU cloud container without GPU or display, LiveKit `v1.13.9` from `ovrilo-infra/local` on the same host, release build. Shows the transport path works and what **software** encoding costs; it says nothing about Windows capture or hardware encoders.

| Run (20–40 s) | Encoder | Sent | Encode ms/frame | Sharer CPU | Viewer | Decode ms | Latency median / p95 |
|---|---|---|---|---|---|---|---|
| 720p30 VP8 | libvpx (software) | 1280×720 @ 30–31 fps, ~930 kbps | 11–13 | ~70 % of one core (~18 % of 4) | 30 fps, 0 drops/freezes/loss | 6.8 | 40 / 46 ms |
| 1080p60 VP8 | libvpx (software) | 1920×1080 @ **36 fps** (target 60), ~2.5 Mbps | 27 | ~210 % of one core (53 % of 4) | 35 fps | 16 | 84 / 109 ms |
| 720p30 H.264 | OpenH264 (software) | 1280×720 @ 30 fps | 15 | ~52 % of one core | 30 fps | 1.5 (FFmpeg) | 30 / 32 ms |

Findings:

- Publish → SFU → subscribe works with `TrackSource::Screenshare`, `is_screencast` and simulcast off; frames arrive as I420 and the pattern decodes on every frame.
- **Software encoding does not keep up at 1080p60** on this machine (36 fps at half the CPU); 720p30 is affordable. The Windows run must show whether LiveKit's libwebrtc offers a hardware encoder there (`encoders`): available backends on Linux were `Auto, Software, PreEncoded`.
- The pattern bitrate is not representative (it compresses far better than a game); compare bitrates only on real windows.
- Linux build notes: the prebuilt libwebrtc needs clang ≥ 21 (`CC`/`CXX`), and its desktop-capture code needs GLib/GIO at link time (`cargo rustc -p voice-spike --release -- -l gio-2.0 -l gobject-2.0 -l glib-2.0`). Not relevant on Windows.

### Codec choice on real screen content (2026-10-15)

First Windows feedback: 720p over H.264 looked bad and 1080p60 did not open. `webrtc-sys` 0.3.49 compiles NVENC only on Linux (it needs CUDA headers and `dlopen`) and has no Windows hardware encoder at all, so on Windows every codec is a software encoder. Same container as above, Xvfb 1920×1080 captured with `screen:<id>` while Chromium rendered the page (which itself used about one core), 1080p30, 4 Mbps cap, one viewer, 30 s per run:

| Content | Codec | Sent | Encode ms/frame | Sharer CPU | Notes |
|---|---|---|---|---|---|
| Scrolling code (static between steps) | OpenH264 | 30 fps, ~100 kbps | 13 | ~50 % of one core | very low bitrate for text, jitter buffer ~320 ms |
| | VP9 | 30 fps, ~1.3 Mbps | 11 | ~66 % | |
| | **AV1** (libaom) | 30 fps, **~600 kbps** | 9 | ~52 % | |
| Full-screen motion (canvas animation) | OpenH264 | **10 fps**, 3.7 Mbps | 64 | ~75 % | limited by CPU |
| | VP9 | **13 fps**, 2.5 Mbps | 39 | ~104 % | limited by CPU |
| | AV1 | **18 fps**, 3.6 Mbps | 56 | ~183 % | limited by CPU |
| | **VP8** | **30 fps**, 4.0 Mbps (at cap) | 17 | ~109 % | |

Decision (engine `ShareOptimization`): **Detail** (text, code, apps) publishes AV1 with `MaintainResolution`; **Motion** (games, video) publishes VP8 with `MaintainFramerate`. Caps: 720p30 3 Mbps, 1080p30 6 Mbps, 1080p60 9 Mbps for VP8, AV1 at 70 % of that. Chromium decodes both (checked in the desktop UI in a browser: AV1 1920×1080 ~29 fps, VP8 1920×1080 30 fps). Open: CPU cost and game FPS on the reference Windows PC, AV1 decoding in WebView2 on the testers' machines, and revisiting hardware H.264 once the SDK offers it on Windows.

## Measurement protocol (Windows, reference PC)

Prerequisites: `ovrilo-infra/local` running (`docker compose up -d --wait`), a release build of the spike (`cargo build -p voice-spike --release`), and a game with an in-game FPS counter. Run each line in its own terminal from `ovrilo-voice`.

```powershell
# 0. What can be captured and which encoders exist
target\release\voice-spike encoders
target\release\voice-spike screen-sources

# 1. Transport baseline (no capture): pattern, 720p30, then 1080p60
target\release\voice-spike screen-watch --identity viewer --seconds 70
target\release\voice-spike screen-share --identity sharer --source pattern --preset 720p30 --seconds 60
target\release\voice-spike screen-share --identity sharer --source pattern --preset 1080p60 --seconds 60

# 2. A real window, default encoder, then forced hardware
target\release\voice-spike screen-share --identity sharer --source window:<id> --preset 1080p30 --seconds 60
target\release\voice-spike screen-share --identity sharer --source window:<id> --preset 1080p30 --encoder hardware --codec h264 --seconds 60

# 3. Gaming: note the game's FPS without sharing, then while sharing (1080p30 and 1080p60), with
#    the voice spike in a parallel call (cpal mic) to listen for glitches
target\release\voice-spike screen-share --identity sharer --source window:<game id> --preset 1080p60 --app-audio <game>.exe

# 4. Application audio isolation: share a browser window playing music with --app-audio chrome.exe
#    while a voice call and a Windows notification sound play; the viewer should hear only the music
target\release\voice-spike screen-share --identity sharer --source window:<browser id> --app-audio chrome.exe
```

Record for each run: the last few report blocks of both processes, the game FPS before/during, Task Manager GPU "Video Encode" usage, and what was heard.

| Run | Encoder | Sent res/fps | Encode ms | Sharer CPU | Viewer fps | Latency p95 | Game FPS before/during | Notes |
|---|---|---|---|---|---|---|---|---|
| | | | | | | | | |

## Known limitations of the tool

- The viewer reads the pattern only from I420 frames; a hardware decoder that returns native buffers reports the buffer type instead of latency.
- Latency needs both processes on one machine (same clock).
- `--app-audio` picks the process by executable name; the product will resolve the shared window's process instead (S2).
- One frame buffer is allocated per captured frame; the product should reuse buffers.
