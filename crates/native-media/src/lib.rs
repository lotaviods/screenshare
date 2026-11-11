//! Native GPU media primitives.
//!
//! This crate replaces the GStreamer pipeline. It deliberately exposes encoded
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

pub mod decode;
pub mod encode;
pub mod frame;

#[cfg(target_os = "linux")]
pub mod capture;

pub use encode::FfmpegEncoder;
pub use encode::{Backend, EncoderConfig, OpusEncoder, VideoEncoder};

pub use frame::Frame;
#[cfg(windows)]
pub use frame::RawFrame;
#[cfg(target_os = "linux")]
pub use frame::{DmaBufFrame, DrmFormat};
