//! Raw CPU frame for Windows capture.

/// A captured frame in BGRA byte order (32 bits per pixel).
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, including padding.
    pub stride: u32,
    pub data: Vec<u8>,
    pub pts_micros: u64,
}

/// The frame type used by the platform encoder.
pub type Frame = RawFrame;
