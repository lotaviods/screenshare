//! XDG Desktop Portal screencast handshake.
//!
//! The compositor's picker is unavoidable: the user selects a monitor or
//! window, and the portal returns the PipeWire node plus a remote fd bound to
//! that node.

use std::os::fd::OwnedFd;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};

use super::CaptureError;

/// A live portal session plus the PipeWire endpoint to read frames from.
pub struct PortalStream {
    /// Owned fd for the remote PipeWire connection.
    pub fd: OwnedFd,
    /// Portal node id to connect the capture stream to.
    pub node_id: u32,
    /// Keep alive: closes the compositor stream when dropped.
    pub session: Session<Screencast>,
}

/// Run the portal dialogue. Blocks until the user picks a source or cancels.
pub async fn open_stream() -> Result<PortalStream, CaptureError> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| CaptureError::Portal(e.to_string()))?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(map_portal)?;

    let select = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Embedded)
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
        .ok_or(CaptureError::NoStream)?
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

fn map_portal(e: ashpd::Error) -> CaptureError {
    if matches!(&e, ashpd::Error::Response(ResponseError::Cancelled)) {
        return CaptureError::Cancelled;
    }
    CaptureError::Portal(e.to_string())
}
