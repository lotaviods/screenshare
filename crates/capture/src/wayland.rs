//! Wayland capture via XDG Desktop Portal `ScreenCast` + PipeWire + native FFmpeg.
//!
//! Flow: create a portal session, ask the compositor to show its source picker,
//! receive the PipeWire node id and remote fd, then hand the DMA-BUF frames to
//! the native FFmpeg encoder. [`crate::gst::GstCapture`] reuses the same portal
//! handshake and encodes with GStreamer instead.

use async_trait::async_trait;
use tokio::sync::mpsc;

use sharescreen_core::{
    CaptureError, CaptureOptions, CaptureSource, EncodedSample, ScreenCapture, SourceKind,
};

use crate::native;
use crate::portal;

/// Wayland/portal capture backend. `start` blocks (async) on the compositor's
/// source-selection dialog, then streams encoded video on a background thread.
#[derive(Default)]
pub struct WaylandCapture {
    active: Option<ActiveCapture>,
}

struct ActiveCapture {
    /// Keeps the portal session alive for the lifetime of the capture.
    session: ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
    stream: native::Stream,
}

impl WaylandCapture {
    pub fn failure(&self) -> Option<String> {
        self.active
            .as_ref()
            .and_then(|active| active.stream.failure())
    }
    pub fn finished(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.stream.finished())
    }
    /// Latest JPEG self-view frame of the running capture.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        self.active.as_ref().and_then(|a| a.stream.latest_preview())
    }
}

#[async_trait]
impl ScreenCapture for WaylandCapture {
    fn list_sources(&self) -> Vec<CaptureSource> {
        // Wayland offers no programmatic monitor/window enumeration: the user
        // selects via the compositor's portal dialog during `start`. Expose a
        // single synthetic source the UI can present.
        vec![CaptureSource {
            id: "screen".into(),
            name: "Screen (portal picker)".into(),
            kind: SourceKind::Screen,
        }]
    }

    async fn start(
        &mut self,
        _source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError> {
        if !options.hardware_encoding {
            return Err(CaptureError::Unavailable(
                "software encoding is not supported by the Linux PipeWire backend; enable VAAPI hardware encoding".into(),
            ));
        }
        if !native::available(options.codec) {
            return Err(CaptureError::Unavailable(format!(
                "native capture does not support {:?} on this system",
                options.codec
            )));
        }

        // Release any in-flight capture first (session + thread + fd).
        self.stop().await.ok();

        let portal = portal::open(options.cursor).await?;

        tracing::info!("capture: native DMA-BUF -> VAAPI path");
        let stream = native::spawn(portal.fd, portal.node_id, &options, sink)?;
        self.active = Some(ActiveCapture {
            session: portal.session,
            stream,
        });
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        // Close the portal session so the compositor ends the stream, then stop
        // the native capture thread.
        let _ = active.session.close().await;
        active.stream.stop();
        Ok(())
    }
}
