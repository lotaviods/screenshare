//! Wayland capture via XDG Desktop Portal `ScreenCast` + PipeWire + native FFmpeg.
//!
//! Flow: create a portal session, ask the compositor to show its source picker
//! (`select_sources` + `start`), receive the PipeWire node id and remote fd,
//! then hand the DMA-BUF frames to the native FFmpeg encoder.

use ashpd::desktop::screencast::{
    CursorMode as AshpdCursorMode, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use async_trait::async_trait;
use tokio::sync::mpsc;

use sharescreen_core::{
    CaptureError, CaptureOptions, CaptureSource, CursorMode, EncodedSample, ScreenCapture,
    SourceKind,
};

use crate::native;

/// Wayland/portal capture backend. `start` blocks (async) on the compositor's
/// source-selection dialog, then streams encoded video on a background thread.
#[derive(Default)]
pub struct WaylandCapture {
    active: Option<ActiveCapture>,
}

struct ActiveCapture {
    /// Keeps the portal session alive for the lifetime of the capture.
    session: Session<Screencast>,
    stream: native::Stream,
}

impl WaylandCapture {
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
        if !native::available(options.codec) {
            return Err(CaptureError::Unavailable(format!(
                "native capture does not support {:?} on this system",
                options.codec
            )));
        }

        // Release any in-flight capture first (session + thread + fd).
        self.stop().await.ok();

        let proxy = Screencast::new().await.map_err(map_ashpd)?;
        let session = proxy
            .create_session(Default::default())
            .await
            .map_err(map_ashpd)?;

        let select_opts = SelectSourcesOptions::default()
            .set_cursor_mode(map_cursor(options.cursor))
            .set_sources(SourceType::Monitor | SourceType::Window | SourceType::Virtual)
            .set_multiple(false)
            .set_persist_mode(PersistMode::DoNot);

        proxy
            .select_sources(&session, select_opts)
            .await
            .map_err(map_ashpd)?;

        // This shows the compositor's picker and resolves once the user picks
        // or cancels. `.response()` is synchronous (blocks a worker thread for
        // the duration of the dialog) — acceptable here, see ARCHITECTURE.md.
        let streams = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(map_ashpd)?
            .response()
            .map_err(map_ashpd)?;

        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| CaptureError::SourceNotFound("portal returned no stream".into()))?;
        let node_id = stream.pipe_wire_node_id();

        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(map_ashpd)?;

        tracing::info!("capture: native DMA-BUF -> VAAPI path");
        let stream = native::spawn(fd, node_id, &options, sink)?;
        self.active = Some(ActiveCapture { session, stream });
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

fn map_cursor(c: CursorMode) -> AshpdCursorMode {
    match c {
        CursorMode::Hidden => AshpdCursorMode::Hidden,
        CursorMode::Embedded => AshpdCursorMode::Embedded,
        CursorMode::Metadata => AshpdCursorMode::Metadata,
    }
}

fn map_ashpd(e: ashpd::Error) -> CaptureError {
    if matches!(&e, ashpd::Error::Response(ResponseError::Cancelled)) {
        return CaptureError::PermissionDenied;
    }
    CaptureError::Failed(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_maps_all_variants() {
        assert!(matches!(
            map_cursor(CursorMode::Hidden),
            AshpdCursorMode::Hidden
        ));
        assert!(matches!(
            map_cursor(CursorMode::Embedded),
            AshpdCursorMode::Embedded
        ));
        assert!(matches!(
            map_cursor(CursorMode::Metadata),
            AshpdCursorMode::Metadata
        ));
    }
}
