//! Native RTP depacketization and FFmpeg decoding.
//!
//! Remote tracks arrive as RTP packets. This module reassembles them into
//! decoder-ready frames and decodes them with FFmpeg.

pub mod ffmpeg;
pub mod jpeg;
pub mod rtp;

pub use ffmpeg::{OpusDecoder, VideoDecoder};
pub use jpeg::JpegEncoder;
pub use rtp::{RtpDepacketizer, RtpKind};
