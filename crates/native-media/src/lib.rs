//! Native GPU media primitives.
//!
//! This crate is the native media pipeline. It deliberately exposes encoded
//! bytes only: no raw frame crosses into `webrtc`, `call`, or the UI.
//!
//! Validated end to end on Hyprland + AMD: portal capture, DMA-BUF import into
//! VAAPI, GPU RGB -> NV12 via `scale_vaapi`, and H.264 encode. The capture
//! crate uses this path exclusively on Linux.
//!
//! The Opus encoder is also used on Windows by the native audio path. The
//! Windows capture path uses a raw BGRA frame and a software H.264/VP8/VP9
//! encoder.

#![cfg(any(target_os = "linux", windows))]

/// FFmpeg's `SWS_BILINEAR` scaler flag.
///
/// The value is part of FFmpeg's stable public API, but bindgen exposes it
/// differently depending on the installed headers: older headers define it as
/// a macro (`ffi::SWS_BILINEAR`), while newer ones define it as an enum member
/// (`ffi::SwsFlags::SWS_BILINEAR`). Using the value directly keeps the same
/// source compiling against both.
pub(crate) const SWS_BILINEAR: i32 = 2;

pub mod decode;
pub mod encode;

use std::ptr;

use ffmpeg_sys_next as ffi;

/// Scale one picture into a tightly packed RGB24 buffer.
///
/// libswscale writes rows in SIMD-width blocks and may store past the last
/// pixel of a row. It therefore needs a destination stride wide enough for
/// that tail; handing it `width * 3` writes past the end of the buffer and
/// corrupts the heap. Scale into a padded stride, then compact the rows.
pub(crate) unsafe fn scale_rgb24(
    sws: *mut ffi::SwsContext,
    src_data: &[*const u8; 4],
    src_stride: &[i32; 4],
    src_height: i32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let row = (width as usize)
        .checked_mul(3)
        .ok_or_else(|| "scaled RGB row size overflow".to_string())?;
    // 64 bytes covers the widest store the installed libswscale can emit.
    let stride = (row + 63) & !63;
    let len = stride
        .checked_mul(height as usize)
        .ok_or_else(|| "scaled RGB frame size overflow".to_string())?;
    let mut buffer = vec![0u8; len];
    let dst_data = [
        buffer.as_mut_ptr(),
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
    ];
    let dst_stride = [stride as i32, 0, 0, 0];
    let ret = ffi::sws_scale(
        sws,
        src_data.as_ptr(),
        src_stride.as_ptr(),
        0,
        src_height,
        dst_data.as_ptr(),
        dst_stride.as_ptr(),
    );
    if ret != height as i32 {
        return Err(format!(
            "sws_scale returned {ret} rows for {height}-row output frame"
        ));
    }
    if stride == row {
        return Ok(buffer);
    }
    let mut rgb = Vec::with_capacity(row * height as usize);
    for line in buffer.chunks_exact(stride) {
        rgb.extend_from_slice(&line[..row]);
    }
    Ok(rgb)
}
pub mod frame;
pub mod preview;
pub use preview::{PreviewSource, Previewer};
pub mod linux_preview;

#[cfg(target_os = "linux")]
pub mod capture;

pub use encode::FfmpegEncoder;
pub use encode::{Backend, EncoderConfig, OpusEncoder, VideoEncoder};
pub use linux_preview::LinuxPreviewer;

pub use frame::Frame;
#[cfg(windows)]
pub use frame::RawFrame;
#[cfg(target_os = "linux")]
pub use frame::{DmaBufFrame, DrmFormat};
