//! `sharescreen-core`: shared application domain model and the trait
//! boundaries between platform-specific capture/audio backends and the
//! WebRTC layer.
//!
//! This crate depends on nothing OS-specific, no Tauri, no GStreamer, and no
//! WebRTC implementation. Platform backends (`sharescreen-capture`,
//! `sharescreen-audio`) implement the traits here; `sharescreen-webrtc`
//! consumes the [`EncodedSample`] values those backends produce. Swapping a
//! platform never touches this crate.

mod call;
mod protocol;
mod traits;
mod types;

pub use call::*;
pub use protocol::*;
pub use traits::*;
pub use types::*;
