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
//! The Opus encoder is also used on Windows by the native audio path.

#![cfg(any(target_os = "linux", windows))]

pub mod encode;

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod frame;

#[cfg(target_os = "linux")]
pub use encode::{Backend, EncoderConfig, VideoEncoder};
#[cfg(target_os = "linux")]
pub use frame::{DmaBufFrame, DrmFormat};
