//! `sharescreen-capture`: screen capture backends.
//!
//! Linux uses the XDG Desktop Portal + PipeWire + native FFmpeg encode. Windows
//! uses GDI + FFmpeg H.264/VP8/VP9. The
//! `ScreenCapture` trait in `sharescreen-core` keeps every backend behind one
//! interface.
#![cfg(any(target_os = "linux", windows))]

#[cfg(target_os = "linux")]
mod native;

#[cfg(windows)]
mod native_windows;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use wayland::WaylandCapture;
#[cfg(windows)]
pub use windows::WindowsCapture;

use async_trait::async_trait;
use sharescreen_core::{
    CaptureBackend, CaptureError, CaptureOptions, CaptureSource, EncodedSample, ScreenCapture,
};
use tokio::sync::mpsc;

/// Deliver ordered video packets without trapping the capture thread during stop.
fn send_while_running(
    sink: &mpsc::Sender<EncodedSample>,
    mut sample: EncodedSample,
    running: impl Fn() -> bool,
) -> bool {
    while running() {
        match sink.try_send(sample) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(returned)) => {
                sample = returned;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
    false
}

/// Selects and owns a concrete capture backend.
pub enum CaptureEngine {
    #[cfg(target_os = "linux")]
    Wayland(WaylandCapture),
    #[cfg(windows)]
    Windows(WindowsCapture),
}

impl CaptureEngine {
    pub fn failure(&self) -> Option<String> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(_) => None,
            #[cfg(windows)]
            Self::Windows(capture) => capture.failure(),
        }
    }
    /// Take a nonfatal encoder fallback notice, if available.
    pub fn take_notice(&self) -> Option<String> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(_) => None,
            #[cfg(windows)]
            Self::Windows(capture) => capture.take_notice(),
        }
    }
    /// Whether the current native capture worker has stopped unexpectedly.
    pub fn finished(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(capture) => capture.finished(),
            #[cfg(windows)]
            Self::Windows(capture) => capture.finished(),
        }
    }
    /// Latest JPEG self-view frame of the running capture, for the local
    /// picture-in-picture preview.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(w) => w.latest_preview(),
            #[cfg(windows)]
            Self::Windows(w) => w.latest_preview(),
        }
    }

    /// Build the engine for the requested backend. `Auto` and `Portal` select
    /// the desktop-portal path on Linux (which also covers X11 sessions via the
    /// portal) and the native GDI path on Windows.
    pub fn new(backend: CaptureBackend) -> Result<Self, CaptureError> {
        match backend {
            #[cfg(target_os = "linux")]
            CaptureBackend::Auto | CaptureBackend::Portal | CaptureBackend::X11 => {
                Ok(CaptureEngine::Wayland(WaylandCapture::default()))
            }
            #[cfg(windows)]
            CaptureBackend::Auto | CaptureBackend::Portal => {
                Ok(CaptureEngine::Windows(WindowsCapture::default()))
            }
            #[cfg(windows)]
            CaptureBackend::Windows => Ok(CaptureEngine::Windows(WindowsCapture::default())),
            other => Err(CaptureError::Unavailable(format!(
                "capture backend {other:?} requires a different host OS"
            ))),
        }
    }
}

#[async_trait]
impl ScreenCapture for CaptureEngine {
    fn list_sources(&self) -> Vec<CaptureSource> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(w) => w.list_sources(),
            #[cfg(windows)]
            Self::Windows(w) => w.list_sources(),
        }
    }

    async fn start(
        &mut self,
        source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(w) => w.start(source, options, sink).await,
            #[cfg(windows)]
            Self::Windows(w) => w.start(source, options, sink).await,
        }
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(w) => w.stop().await,
            #[cfg(windows)]
            Self::Windows(w) => w.stop().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_video_channel_does_not_prevent_shutdown() {
        use sharescreen_core::{Codec, MediaKind};
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        use std::time::Duration;
        let sample = EncodedSample {
            kind: MediaKind::Video,
            codec: Codec::H264,
            data: vec![1],
            timestamp_micros: 0,
            duration_micros: 1,
            keyframe: false,
        };
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(sample.clone()).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let sent = send_while_running(&tx, sample, || worker_running.load(Ordering::SeqCst));
            done_tx.send(sent).unwrap();
        });
        running.store(false, Ordering::SeqCst);
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        drop(rx);
        worker.join().unwrap();
        assert!(!result.unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn auto_and_portal_select_wayland() {
        assert!(CaptureEngine::new(CaptureBackend::Auto).is_ok());
        assert!(CaptureEngine::new(CaptureBackend::Portal).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_selects_portal() {
        assert!(CaptureEngine::new(CaptureBackend::X11).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn windows_backend_is_unavailable_on_linux() {
        assert!(matches!(
            CaptureEngine::new(CaptureBackend::Windows),
            Err(CaptureError::Unavailable(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_backend_is_available_on_windows() {
        assert!(CaptureEngine::new(CaptureBackend::Windows).is_ok());
        assert!(CaptureEngine::new(CaptureBackend::Auto).is_ok());
    }
}
