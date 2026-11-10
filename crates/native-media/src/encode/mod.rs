//! Video encoders.

use crate::frame::DmaBufFrame;

pub mod ffmpeg;

pub use ffmpeg::FfmpegEncoder;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("no usable hardware encoder: {0}")]
    Unavailable(String),
    #[error("unsupported input format: {0:?}")]
    UnsupportedFormat(Option<crate::frame::DrmFormat>),
    #[error("ffmpeg error: {0}")]
    Ffmpeg(String),
}

/// One encoded access unit, ready for RTP packetization.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_micros: u64,
    pub duration_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vaapi,
    Nvenc,
    Software,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// Encode captured frames. Implementations run on a dedicated thread.
pub trait VideoEncoder: Send {
    fn backend(&self) -> Backend;
    fn encode(
        &mut self,
        frame: &DmaBufFrame,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError>;
    fn request_keyframe(&mut self);
    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), EncodeError>;
}
