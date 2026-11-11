//! Native GPU media primitives.
//!
//! This crate replaces the GStreamer pipeline piece by piece. It deliberately
//! exposes encoded bytes only: no raw frame crosses into `webrtc`, `call`, or
//! the UI.
//!
//! Validated end to end on Hyprland + AMD: portal capture, DMA-BUF import into
//! VAAPI, GPU RGB -> NV12 via `scale_vaapi`, and H.264 encode. The capture
//! crate uses it when `SHARESCREEN_MEDIA=native`; GStreamer remains the default
//! until the A/B comparison passes.
//!
//! The Opus encoder is also used on Windows by the native audio path. The
//! Windows capture path uses a raw BGRA frame and a software H.264 encoder.

#![cfg(any(target_os = "linux", windows))]

pub mod encode;
pub mod frame;

#[cfg(target_os = "linux")]
pub mod capture;

#[cfg(target_os = "linux")]
pub use encode::FfmpegEncoder;
#[cfg(windows)]
pub use encode::H264Encoder;
pub use encode::{Backend, EncoderConfig, OpusEncoder, VideoEncoder};

pub use frame::Frame;
#[cfg(windows)]
pub use frame::RawFrame;
#[cfg(target_os = "linux")]
pub use frame::{DmaBufFrame, DrmFormat};
