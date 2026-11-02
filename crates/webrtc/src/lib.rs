//! `sharescreen-webrtc`: WebRTC media transport.
//!
//! Wraps the `webrtc` crate (0.21) so the rest of the app never touches the
//! WebRTC API directly. A [`WebRtcPeer`] owns one peer connection; local
//! media (mic / system / screen) is written as [`EncodedSample`] values
//! through [`TrackWriter`] handles returned by [`WebRtcPeer::add_track`].
//!
//! Signaling (offer/answer/ICE) stays outside this crate: the caller passes
//! SDP and candidates in and out through the `WebRtcPeer` methods and the
//! [`PeerEvent`] callback. See the `sharescreen-signaling` crate for the
//! message plumbing.

mod codec;
mod peer;

pub use codec::{codec_mime, codec_to_rtp, register_default_codecs};
pub use peer::{
    IceCandidate, IceServer, PeerConfig, PeerEvent, RemoteTrack, RemoteTrackKind, RtpPacket,
    SessionDescription, TrackId, TrackWriter, WebRtcPeer,
};

/// Crate error type.
#[derive(Debug, thiserror::Error)]
pub enum WebRtcError {
    #[error("{0}")]
    Webrtc(#[from] webrtc::error::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no runtime available; enable the runtime-tokio or runtime-smol feature")]
    NoRuntime,
    #[error("the negotiated codec list does not contain {0:?}")]
    NoMatchingCodec(sharescreen_core::Codec),
}

pub type Result<T> = std::result::Result<T, WebRtcError>;
