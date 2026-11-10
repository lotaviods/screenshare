//! DMA-BUF frame description shared by capture and encode.

use std::os::fd::OwnedFd;

/// DRM format fourcc values used by capture and VAAPI import.
///
/// Variant names follow `drm_fourcc.h`; the byte order in memory is little
/// endian, so `Argb8888` is the usual `B,G,R,A` screencast buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrmFormat {
    Nv12,
    Argb8888,
    Xrgb8888,
    Abgr8888,
    Xbgr8888,
    Bgra8888,
    Bgrx8888,
    Rgba8888,
    Rgbx8888,
}

impl DrmFormat {
    pub fn fourcc(self) -> u32 {
        fourcc(match self {
            Self::Nv12 => *b"NV12",
            Self::Argb8888 => *b"AR24",
            Self::Xrgb8888 => *b"XR24",
            Self::Abgr8888 => *b"AB24",
            Self::Xbgr8888 => *b"XB24",
            Self::Bgra8888 => *b"BA24",
            Self::Bgrx8888 => *b"BX24",
            Self::Rgba8888 => *b"RA24",
            Self::Rgbx8888 => *b"RX24",
        })
    }

    /// Map `SPA_VIDEO_FORMAT_*` to the DRM fourcc the compositor exports.
    pub fn from_spa(format: u32) -> Option<Self> {
        match format {
            // SPA_VIDEO_FORMAT_RGBx / BGRx / xRGB / xBGR
            7 => Some(Self::Xbgr8888),
            8 => Some(Self::Xrgb8888),
            9 => Some(Self::Bgrx8888),
            10 => Some(Self::Rgbx8888),
            // SPA_VIDEO_FORMAT_RGBA / BGRA / ARGB / ABGR
            11 => Some(Self::Abgr8888),
            12 => Some(Self::Argb8888),
            13 => Some(Self::Bgra8888),
            14 => Some(Self::Rgba8888),
            // SPA_VIDEO_FORMAT_NV12
            23 => Some(Self::Nv12),
            _ => None,
        }
    }

    pub fn is_nv12(self) -> bool {
        matches!(self, Self::Nv12)
    }
}

const fn fourcc(s: [u8; 4]) -> u32 {
    (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
}

/// One DRM object backing a frame. PipeWire screencast normally sends one.
#[derive(Debug)]
pub struct DmaBufObject {
    pub fd: OwnedFd,
    pub size: u64,
}

/// One plane inside a DRM layer.
#[derive(Debug, Clone, Copy)]
pub struct DmaBufPlane {
    pub object_index: u8,
    pub offset: u32,
    pub pitch: u32,
}

/// One DRM layer. Most captures have exactly one.
#[derive(Debug)]
pub struct DmaBufLayer {
    pub format: DrmFormat,
    pub planes: Vec<DmaBufPlane>,
}

/// A captured frame still resident in GPU-accessible memory.
#[derive(Debug)]
pub struct DmaBufFrame {
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub objects: Vec<DmaBufObject>,
    pub layers: Vec<DmaBufLayer>,
    pub pts_micros: u64,
}

impl DmaBufFrame {
    pub fn format(&self) -> Option<DrmFormat> {
        self.layers.first().map(|layer| layer.format)
    }

    pub fn is_nv12(&self) -> bool {
        self.format().is_some_and(DrmFormat::is_nv12)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_matches_drm_values() {
        assert_eq!(DrmFormat::Nv12.fourcc(), 0x3231_564e);
        assert_eq!(DrmFormat::Xrgb8888.fourcc(), u32::from_le_bytes(*b"XR24"));
        assert_eq!(DrmFormat::Argb8888.fourcc(), u32::from_le_bytes(*b"AR24"));
    }

    #[test]
    fn spa_format_maps_to_drm() {
        assert_eq!(DrmFormat::from_spa(23), Some(DrmFormat::Nv12));
        assert_eq!(DrmFormat::from_spa(12), Some(DrmFormat::Argb8888));
        assert_eq!(DrmFormat::from_spa(0xffff), None);
    }
}
