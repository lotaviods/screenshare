//! X11 capture via GStreamer `ximagesrc`.
//!
//! The portal/PipeWire backend already covers X11 sessions with a compositor
//! picker; this backend exists for setups without a working portal. It captures
//! the whole X display, or a specific window when the source id is `xid:<id>`.
//!
//! Per-monitor and per-window *enumeration* needs XCB and is a follow-up; the
//! source list therefore only offers whole-screen capture for now.

use async_trait::async_trait;
use tokio::sync::mpsc;

use sharescreen_core::{
    CaptureError, CaptureOptions, CaptureSource, EncodedSample, ScreenCapture, SourceKind,
};

use crate::gst;

/// Native X11 capture backend.
pub struct X11Capture {
    stream: Option<gst::Stream>,
}

impl Default for X11Capture {
    fn default() -> Self {
        Self { stream: None }
    }
}

impl X11Capture {
    /// Latest JPEG self-view frame of the running capture.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        self.stream.as_ref().and_then(|s| s.latest_preview())
    }
}

#[async_trait]
impl ScreenCapture for X11Capture {
    fn list_sources(&self) -> Vec<CaptureSource> {
        vec![CaptureSource {
            id: "screen".into(),
            name: "Entire screen".into(),
            kind: SourceKind::Screen,
        }]
    }

    async fn start(
        &mut self,
        source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError> {
        self.stop().await.ok();

        gst::ensure_gst_init();
        let src = x11_source(&source);
        let launch = gst::build_launch(&src, &options);
        let stream = gst::spawn(launch, sink, options.codec, None)?;

        self.stream = Some(stream);
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        if let Some(stream) = self.stream.take() {
            stream.stop();
        }
        Ok(())
    }
}

/// Build the `ximagesrc` source string. A source id of `xid:<decimal>` selects
/// that X11 window; anything else captures the whole display.
fn x11_source(source: &CaptureSource) -> String {
    if let Some(xid) = source.id.strip_prefix("xid:") {
        if !xid.is_empty() && xid.chars().all(|c| c.is_ascii_digit()) {
            return format!("ximagesrc xid={xid} use-damage=false");
        }
    }
    "ximagesrc use-damage=false".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_source_is_whole_display() {
        let src = CaptureSource {
            id: "screen".into(),
            name: "screen".into(),
            kind: SourceKind::Screen,
        };
        assert_eq!(x11_source(&src), "ximagesrc use-damage=false");
    }

    #[test]
    fn xid_source_selects_window() {
        let src = CaptureSource {
            id: "xid:123456".into(),
            name: "window".into(),
            kind: SourceKind::Window,
        };
        assert_eq!(x11_source(&src), "ximagesrc xid=123456 use-damage=false");
    }

    #[test]
    fn malformed_xid_falls_back_to_display() {
        let src = CaptureSource {
            id: "xid:notanumber".into(),
            name: "window".into(),
            kind: SourceKind::Window,
        };
        assert_eq!(x11_source(&src), "ximagesrc use-damage=false");
    }
}
