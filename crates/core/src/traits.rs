use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::types::{CaptureSource, Codec, CursorMode, Device, EncodedSample};

// --- Screen capture --------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("capture backend unavailable: {0}")]
    Unavailable(String),
    #[error("permission denied by the user")]
    PermissionDenied,
    #[error("capture source not found: {0}")]
    SourceNotFound(String),
    #[error("capture failed: {0}")]
    Failed(String),
    #[error("capture already running")]
    AlreadyRunning,
    #[error("capture not running")]
    NotRunning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureOptions {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub cursor: CursorMode,
    /// Video codec the backend must encode to.
    pub codec: Codec,
    /// Target video bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Prefer a hardware encoder when one is available.
    pub hardware_encoding: bool,
}

/// Platform screen-capture backend. Implementations live in
/// `sharescreen-capture` and are selected at runtime by [`crate::CaptureBackend`].
///
/// The backend owns the capture + encode pipeline and pushes [`EncodedSample`]
/// frames into `sink`. No raw frame buffer crosses the UI boundary; the
/// UI only ever receives encoded media or metadata.
#[async_trait]
pub trait ScreenCapture: Send + Sync {
    /// List shareable sources (monitors, windows, or the whole desktop).
    fn list_sources(&self) -> Vec<CaptureSource>;

    /// Begin capturing `source` with `options`, pushing encoded frames to
    /// `sink`. Stops any in-flight capture first.
    async fn start(
        &mut self,
        source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError>;

    /// Stop the current capture and release the session/portal handle.
    async fn stop(&mut self) -> Result<(), CaptureError>;
}

// --- Audio capture ---------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio backend unavailable: {0}")]
    Unavailable(String),
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),
    #[error("audio capture failed: {0}")]
    Failed(String),
    #[error("audio device disconnected: {0}")]
    Disconnected(String),
}

/// Microphone and system-audio capture backend. Microphone and system audio
/// are independent sources, each with its own volume and mute state, and each
/// produces its own [`EncodedSample`] stream.
#[async_trait]
pub trait AudioCapture: Send + Sync {
    fn list_microphones(&self) -> Vec<Device>;
    fn list_outputs(&self) -> Vec<Device>;
    /// List system-audio monitor sources (the output looped back as input).
    fn list_system_monitors(&self) -> Vec<Device>;

    async fn start_mic(
        &mut self,
        device_id: Option<&str>,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), AudioError>;
    async fn stop_mic(&mut self) -> Result<(), AudioError>;

    async fn start_system(
        &mut self,
        device_id: Option<&str>,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), AudioError>;
    async fn stop_system(&mut self) -> Result<(), AudioError>;

    fn set_mic_volume(&mut self, volume: f32) -> Result<(), AudioError>;
    fn set_mic_muted(&mut self, muted: bool) -> Result<(), AudioError>;
    fn set_system_volume(&mut self, volume: f32) -> Result<(), AudioError>;
}
