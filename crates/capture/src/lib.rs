//! `sharescreen-capture`: screen capture backends.
//!
//! Linux uses the XDG Desktop Portal + PipeWire + GStreamer (or native X11 via
//! `ximagesrc`). Windows uses GStreamer's `gdiscreencapsrc`. Both feed the same
//! GStreamer encode pipeline, so the `ScreenCapture` trait in
//! `sharescreen-core` keeps every backend behind one interface.
#![cfg(any(target_os = "linux", windows))]

mod gst;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use wayland::WaylandCapture;
#[cfg(target_os = "linux")]
pub use x11::X11Capture;
#[cfg(windows)]
pub use windows::WindowsCapture;

use async_trait::async_trait;
use sharescreen_core::{
    CaptureBackend, CaptureError, CaptureOptions, CaptureSource, EncodedSample, ScreenCapture,
};
use tokio::sync::mpsc;

/// Selects and owns a concrete capture backend.
pub enum CaptureEngine {
    #[cfg(target_os = "linux")]
    Wayland(WaylandCapture),
    #[cfg(target_os = "linux")]
    X11(X11Capture),
    #[cfg(windows)]
    Windows(WindowsCapture),
}

impl CaptureEngine {
    /// Latest JPEG self-view frame of the running capture, for the local
    /// picture-in-picture preview.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(w) => w.latest_preview(),
            #[cfg(target_os = "linux")]
            Self::X11(x) => x.latest_preview(),
            #[cfg(windows)]
            Self::Windows(w) => w.latest_preview(),
        }
    }

    /// Build the engine for the requested backend. `Auto` and `Portal` select
    /// the desktop-portal path on Linux (which also covers X11 sessions via the
    /// portal) and `gdiscreencapsrc` on Windows; `X11` selects the native
    /// `ximagesrc` backend.
    pub fn new(backend: CaptureBackend) -> Result<Self, CaptureError> {
        match backend {
            #[cfg(target_os = "linux")]
            CaptureBackend::Auto | CaptureBackend::Portal => {
                Ok(CaptureEngine::Wayland(WaylandCapture::default()))
            }
            #[cfg(windows)]
            CaptureBackend::Auto | CaptureBackend::Portal => {
                Ok(CaptureEngine::Windows(WindowsCapture::default()))
            }
            #[cfg(target_os = "linux")]
            CaptureBackend::X11 => Ok(CaptureEngine::X11(X11Capture::default())),
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
            #[cfg(target_os = "linux")]
            Self::X11(x) => x.list_sources(),
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
            #[cfg(target_os = "linux")]
            Self::X11(x) => x.start(source, options, sink).await,
            #[cfg(windows)]
            Self::Windows(w) => w.start(source, options, sink).await,
        }
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Wayland(w) => w.stop().await,
            #[cfg(target_os = "linux")]
            Self::X11(x) => x.stop().await,
            #[cfg(windows)]
            Self::Windows(w) => w.stop().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn auto_and_portal_select_wayland() {
        assert!(CaptureEngine::new(CaptureBackend::Auto).is_ok());
        assert!(CaptureEngine::new(CaptureBackend::Portal).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_selects_x11_backend() {
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
