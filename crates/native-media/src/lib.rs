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
pub mod frame;
pub mod preview;

#[cfg(target_os = "linux")]
pub mod capture;

pub use encode::FfmpegEncoder;
pub use encode::{Backend, EncoderConfig, OpusEncoder, VideoEncoder};
pub use preview::{PreviewSource, Previewer};

pub use frame::Frame;
#[cfg(windows)]
pub use frame::RawFrame;
#[cfg(target_os = "linux")]
pub use frame::{DmaBufFrame, DrmFormat};
