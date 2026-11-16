//! XDG Desktop Portal `ScreenCast` handshake shared by the capture backends.
//!
//! The compositor always shows its own picker; the caller receives the chosen
//! PipeWire node id plus a remote fd bound to it, and must keep the session (and
//! the fd) alive for as long as frames are read.

#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;

use ashpd::desktop::screencast::{
    CursorMode as PortalCursorMode, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};

use sharescreen_core::{CaptureError, CursorMode};

/// A live portal session plus the PipeWire endpoint to read frames from.
pub struct PortalStream {
    /// Owned fd for the remote PipeWire connection.
    pub fd: OwnedFd,
    /// Portal node id to connect the capture stream to.
    pub node_id: u32,
    /// Keep alive: closes the compositor stream when dropped.
    pub session: Session<Screencast>,
}

/// Run the portal picker. Blocks until the user picks a source or cancels.
pub async fn open(cursor: CursorMode) -> Result<PortalStream, CaptureError> {
    let proxy = Screencast::new().await.map_err(map_portal)?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(map_portal)?;

    let select = SelectSourcesOptions::default()
        .set_cursor_mode(map_cursor(cursor))
        .set_sources(SourceType::Monitor | SourceType::Window | SourceType::Virtual)
        .set_multiple(false)
        .set_persist_mode(PersistMode::DoNot);
    proxy
        .select_sources(&session, select)
        .await
        .map_err(map_portal)?;

    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .map_err(map_portal)?
        .response()
        .map_err(map_portal)?;

    let node_id = streams
        .streams()
        .first()
        .ok_or_else(|| CaptureError::SourceNotFound("portal returned no stream".into()))?
        .pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(map_portal)?;

    Ok(PortalStream {
        fd,
        node_id,
        session,
    })
}

fn map_cursor(cursor: CursorMode) -> PortalCursorMode {
    match cursor {
        CursorMode::Hidden => PortalCursorMode::Hidden,
        CursorMode::Embedded => PortalCursorMode::Embedded,
        CursorMode::Metadata => PortalCursorMode::Metadata,
    }
}

fn map_portal(error: ashpd::Error) -> CaptureError {
    if matches!(&error, ashpd::Error::Response(ResponseError::Cancelled)) {
        return CaptureError::PermissionDenied;
    }
    CaptureError::Failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_maps_all_variants() {
        assert!(matches!(
            map_cursor(CursorMode::Hidden),
            PortalCursorMode::Hidden
        ));
        assert!(matches!(
            map_cursor(CursorMode::Embedded),
            PortalCursorMode::Embedded
        ));
        assert!(matches!(
            map_cursor(CursorMode::Metadata),
            PortalCursorMode::Metadata
        ));
    }
}
