//! Screen sharing (ADR-0017): WebRTC desktop capture (Windows Graphics Capture on Windows) → I420
//! → `NativeVideoSource`, published as a `Screenshare` track. Capture runs on its own thread and
//! never touches the voice path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use livekit::options::{DegradationPreference, VideoCodec, VideoEncoding};
use livekit::webrtc::desktop_capturer::{
    CaptureError, CaptureSource, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions,
    DesktopFrame,
};
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoResolution, VideoRotation};
use livekit::webrtc::video_source::native::NativeVideoSource;
use ovrilo_voice::{
    ScreenSource, ScreenSourceId, ScreenSourceKind, ShareOptimization, SharePreset, TransportEvent,
    VoiceError,
};
use tokio::sync::mpsc;

use crate::app_audio;

/// Consecutive permanent capture errors after which the share ends (the window closed).
const PERMANENT_ERRORS_TO_END: u32 = 5;

/// How a share is encoded: size and frame-rate caps from the preset, codec and degradation from
/// the optimization.
///
/// This SDK has no hardware video encoder on Windows (NVENC is Linux-only), so codecs are picked
/// from software encoders measured on the same 1080p30 content (4-core Linux VM, local SFU,
/// not an authoritative baseline): on scrolling code AV1 needed ~600 kbps against ~1.3 Mbps for
/// VP9 at similar CPU, while OpenH264 looked worst; on fast motion only VP8 kept 30 fps (H.264
/// 10, VP9 13, AV1 18 fps, all CPU-limited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Encoding {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_bitrate: u64,
    pub codec: VideoCodec,
    pub degradation: DegradationPreference,
}

impl Encoding {
    pub fn of(preset: SharePreset, optimization: ShareOptimization) -> Self {
        // Upper bounds per viewer for VP8; the encoder uses less when the picture allows.
        let (width, height, fps, max_bitrate) = match preset {
            SharePreset::FullHd1080p30 => (1920, 1080, 30, 6_000_000),
            SharePreset::FullHd1080p60 => (1920, 1080, 60, 9_000_000),
            _ => (1280, 720, 30, 3_000_000),
        };
        let (codec, degradation, max_bitrate) = match optimization {
            ShareOptimization::Motion => (
                VideoCodec::VP8,
                DegradationPreference::MaintainFramerate,
                max_bitrate,
            ),
            // AV1 needs about 30 % fewer bits for the same picture (LiveKit applies the same factor).
            _ => (
                VideoCodec::AV1,
                DegradationPreference::MaintainResolution,
                max_bitrate * 7 / 10,
            ),
        };
        Self {
            width,
            height,
            fps,
            max_bitrate,
            codec,
            degradation,
        }
    }

    pub fn video_encoding(&self) -> VideoEncoding {
        VideoEncoding {
            max_bitrate: self.max_bitrate,
            max_framerate: f64::from(self.fps),
        }
    }

    pub fn resolution(&self) -> VideoResolution {
        VideoResolution {
            width: self.width,
            height: self.height,
        }
    }

    /// Largest even size within the preset that keeps the source's aspect ratio (never upscales).
    pub fn fit(&self, width: u32, height: u32) -> (u32, u32) {
        let scale = (f64::from(self.width) / f64::from(width))
            .min(f64::from(self.height) / f64::from(height))
            .min(1.0);
        let even = |v: f64| ((v as u32) & !1).max(2);
        (
            even(f64::from(width) * scale),
            even(f64::from(height) * scale),
        )
    }
}

/// A parsed [`ScreenSourceId`]: `screen:<id>` or `window:<id>` (WebRTC source ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Screen(u64),
    Window(u64),
}

impl Target {
    pub fn parse(id: &ScreenSourceId) -> Result<Self, VoiceError> {
        let parse = |rest: &str| {
            rest.parse::<u64>()
                .map_err(|_| VoiceError::capture(format!("unknown screen source '{id}'")))
        };
        match id.0.split_once(':') {
            Some(("screen", rest)) => Ok(Self::Screen(parse(rest)?)),
            Some(("window", rest)) => Ok(Self::Window(parse(rest)?)),
            _ => Err(VoiceError::capture(format!("unknown screen source '{id}'"))),
        }
    }

    fn kind(self) -> DesktopCaptureSourceType {
        match self {
            Self::Screen(_) => DesktopCaptureSourceType::Screen,
            Self::Window(_) => DesktopCaptureSourceType::Window,
        }
    }

    fn id(self) -> u64 {
        match self {
            Self::Screen(id) | Self::Window(id) => id,
        }
    }
}

/// Screens, then windows of other applications (Ovrilo's own windows are left out).
pub(crate) fn list_sources() -> Result<Vec<ScreenSource>, VoiceError> {
    let mut sources = Vec::new();
    let processes = app_audio::window_processes();
    for kind in [
        DesktopCaptureSourceType::Screen,
        DesktopCaptureSourceType::Window,
    ] {
        let Some(capturer) = DesktopCapturer::new(DesktopCapturerOptions::new(kind)) else {
            return Err(VoiceError::capture(
                "screen capture is not available on this system",
            ));
        };
        let windows = matches!(kind, DesktopCaptureSourceType::Window);
        let screens = capturer.get_source_list();
        let count = screens.len();
        for (index, source) in screens.into_iter().enumerate() {
            if windows {
                let process = processes.get(&source.id());
                if process.is_some_and(|p| p.id == std::process::id()) {
                    continue;
                }
                let title = source.title();
                if title.trim().is_empty() {
                    continue;
                }
                sources.push(ScreenSource {
                    id: ScreenSourceId(format!("window:{}", source.id())),
                    kind: ScreenSourceKind::Window,
                    title,
                    application: process.map(|p| p.name.clone()).filter(|n| !n.is_empty()),
                    audio_available: process.is_some() && app_audio::SUPPORTED,
                });
            } else {
                // WebRTC titles screens by device name or not at all; number them instead.
                sources.push(ScreenSource {
                    id: ScreenSourceId(format!("screen:{}", source.id())),
                    kind: ScreenSourceKind::Screen,
                    title: if count > 1 {
                        format!("Screen {}", index + 1)
                    } else {
                        "Entire screen".to_owned()
                    },
                    application: None,
                    audio_available: false,
                });
            }
        }
    }
    Ok(sources)
}

/// A running capture. Dropping it stops the capture thread.
pub(crate) struct Capture {
    pub title: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts capturing `target` at the preset's frame rate into `video`. `events` receives
/// [`TransportEvent::ScreenShareEnded`] once when capture stops by itself (the window closed).
pub(crate) fn start(
    target: Target,
    encoding: Encoding,
    video: NativeVideoSource,
    events: mpsc::UnboundedSender<TransportEvent>,
) -> Result<Capture, VoiceError> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("ovrilo-screen-capture".into())
            .spawn(move || run(target, encoding, video, &stop, events, ready_tx))
            .map_err(|e| VoiceError::capture(format!("capture thread: {e}")))?
    };
    // Desktop capturers must be created and driven on the same thread; wait until it found the
    // source so a bad id is reported to the caller instead of later.
    match ready_rx.recv() {
        Ok(Ok(title)) => Ok(Capture {
            title,
            stop,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(VoiceError::capture("capture thread exited before starting"))
        }
    }
}

fn run(
    target: Target,
    encoding: Encoding,
    video: NativeVideoSource,
    stop: &AtomicBool,
    events: mpsc::UnboundedSender<TransportEvent>,
    ready: std::sync::mpsc::Sender<Result<String, VoiceError>>,
) {
    let Some(mut capturer) = DesktopCapturer::new(DesktopCapturerOptions::new(target.kind()))
    else {
        let _ = ready.send(Err(VoiceError::capture(
            "screen capture is not available on this system",
        )));
        return;
    };
    let Some(source) = find(&capturer, target) else {
        let _ = ready.send(Err(VoiceError::capture(
            "the window or screen is no longer available",
        )));
        return;
    };
    let title = match target {
        Target::Window(_) => source.title(),
        Target::Screen(_) => "Entire screen".to_owned(),
    };
    let _ = ready.send(Ok(title));

    let permanent_errors = Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let permanent_errors = permanent_errors.clone();
        capturer.start_capture(
            Some(source),
            move |result: Result<DesktopFrame, CaptureError>| match result {
                Ok(frame) => {
                    permanent_errors.store(0, Ordering::Relaxed);
                    deliver(&frame, encoding, &video);
                }
                // Minimized windows and similar: no frame this time.
                Err(CaptureError::Temporary) => {}
                Err(CaptureError::Permanent) => {
                    permanent_errors.fetch_add(1, Ordering::Relaxed);
                }
            },
        );
    }

    let interval = Duration::from_secs_f64(1.0 / f64::from(encoding.fps));
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        capturer.capture_frame();
        if permanent_errors.load(Ordering::Relaxed) >= PERMANENT_ERRORS_TO_END {
            let reason = match target {
                Target::Window(_) => "the shared window was closed",
                Target::Screen(_) => "the shared screen is no longer available",
            };
            let _ = events.send(TransportEvent::ScreenShareEnded {
                reason: reason.to_owned(),
            });
            return;
        }
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Fell behind (slow capture): skip ahead instead of capturing in a burst.
            next = now;
        }
    }
}

fn find(capturer: &DesktopCapturer, target: Target) -> Option<CaptureSource> {
    capturer
        .get_source_list()
        .into_iter()
        .find(|s| s.id() == target.id())
}

fn deliver(frame: &DesktopFrame, encoding: Encoding, video: &NativeVideoSource) {
    let (width, height) = (frame.width().max(0) as u32, frame.height().max(0) as u32);
    if width < 2 || height < 2 {
        return;
    }
    // WebRTC desktop frames are 32-bit BGRA in memory, which libyuv calls ARGB.
    let mut full = I420Buffer::new(width, height);
    let (stride_y, stride_u, stride_v) = full.strides();
    let (y, u, v) = full.data_mut();
    yuv_helper::argb_to_i420(
        frame.data(),
        frame.stride(),
        y,
        stride_y,
        u,
        stride_u,
        v,
        stride_v,
        width as i32,
        height as i32,
    );
    let (out_width, out_height) = encoding.fit(width, height);
    let buffer = if (out_width, out_height) == (width, height) {
        full
    } else {
        full.scale(out_width as i32, out_height as i32)
    };
    let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
    frame.timestamp_us = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64);
    video.capture_frame(&frame);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_fit_without_upscaling_and_keep_even_sizes() {
        let hd = Encoding::of(SharePreset::Hd720p30, ShareOptimization::Detail);
        assert_eq!(hd.fit(2560, 1440), (1280, 720));
        assert_eq!(hd.fit(3440, 1440), (1280, 534));
        assert_eq!(hd.fit(1001, 601), (1000, 600), "never upscaled, even");
        let full = Encoding::of(SharePreset::FullHd1080p60, ShareOptimization::Motion);
        assert_eq!((full.fps, full.max_bitrate), (60, 9_000_000));
    }

    #[test]
    fn optimization_picks_codec_and_what_gives_way() {
        let detail = Encoding::of(SharePreset::FullHd1080p30, ShareOptimization::Detail);
        assert_eq!(detail.codec, VideoCodec::AV1);
        assert_eq!(
            detail.degradation,
            DegradationPreference::MaintainResolution
        );
        assert_eq!(detail.max_bitrate, 4_200_000);
        let motion = Encoding::of(SharePreset::FullHd1080p30, ShareOptimization::Motion);
        assert_eq!(motion.codec, VideoCodec::VP8);
        assert_eq!(motion.degradation, DegradationPreference::MaintainFramerate);
        assert_eq!(motion.max_bitrate, 6_000_000);
    }

    #[test]
    fn source_ids_round_trip() {
        assert_eq!(
            Target::parse(&ScreenSourceId("window:42".into())).unwrap(),
            Target::Window(42)
        );
        assert_eq!(
            Target::parse(&ScreenSourceId("screen:0".into())).unwrap(),
            Target::Screen(0)
        );
        assert!(Target::parse(&ScreenSourceId("tab:1".into())).is_err());
        assert!(Target::parse(&ScreenSourceId("window:x".into())).is_err());
    }
}
