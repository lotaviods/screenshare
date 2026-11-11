use serde::{Deserialize, Serialize};

// --- Devices ---------------------------------------------------------------

/// The kind of an OS device, used for the device pickers in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// A microphone input.
    Microphone,
    /// An audio output (speakers/headphones) for local playback.
    AudioOutput,
    /// A physical or virtual monitor.
    Monitor,
    /// A single application window.
    Window,
    /// The whole desktop (all monitors).
    Screen,
}

/// An OS device (microphone, output, monitor, or window) as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
}

// --- Capture sources -------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Monitor,
    Window,
    Screen,
}

/// A shareable capture source (a monitor, a window, or the whole desktop).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub kind: SourceKind,
}

/// How the cursor is handled in the captured stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    Hidden,
    Embedded,
    Metadata,
}

// --- Codecs ----------------------------------------------------------------

/// The single codec vocabulary for the whole app. Audio currently only uses
/// `Opus`; video uses the remaining variants. Validation lives in `config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    Opus,
    Vp8,
    Vp9,
    H264,
    H265,
    Av1,
}

/// Preferred capture backend. `Auto` lets the app pick per platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureBackend {
    Auto,
    /// XDG Desktop Portal + PipeWire (Wayland and X11 portal).
    Portal,
    /// Deprecated alias for the portal; kept so existing configs load.
    X11,
    /// Windows.Graphics.Capture / DXGI.
    Windows,
}

/// Preferred connection topology. `Mesh` is peer-to-peer for small rooms;
/// `Sfu` selects an SFU when one is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Mesh,
    Sfu,
}

// --- Encoded media ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Audio,
    Video,
}

/// One encoded media unit (a full video frame or an Opus audio chunk) handed
/// from a capture/encode backend to the WebRTC layer.
///
/// Kept codec-typed and byte-buffer based so no platform frame type or raw
/// pixel buffer crosses the UI boundary; only encoded bytes leave the
/// backend.
#[derive(Debug, Clone)]
pub struct EncodedSample {
    pub kind: MediaKind,
    pub codec: Codec,
    pub data: Vec<u8>,
    /// Presentation timestamp in microseconds, monotonic within a stream.
    pub timestamp_micros: u64,
    /// Duration of this sample in microseconds (e.g. 20000 for a 20ms Opus
    /// frame, 33333 for a 30fps video frame).
    pub duration_micros: u64,
    /// True for a video keyframe. Ignored for audio.
    pub keyframe: bool,
}
