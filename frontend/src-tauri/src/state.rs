//! Application state shared across Tauri commands.

use std::sync::atomic::AtomicBool;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex as TokioMutex;

use sharescreen_audio::AudioEngine;
use sharescreen_call::CallSession;
use sharescreen_capture::CaptureEngine;
use sharescreen_config::ConfigStore;
use tauri::AppHandle;

/// Managed Tauri state. Config is sync (plain `Mutex`); capture/audio engines
/// have async methods and use `tokio::sync::Mutex` so commands can hold the
/// guard across `.await`.
pub struct AppState {
    pub config: StdMutex<ConfigStore>,
    pub audio: TokioMutex<AudioEngine>,
    pub capture: TokioMutex<CaptureEngine>,
    /// Active call session, present once connected to a signaling server.
    pub call: TokioMutex<Option<CallSession>>,
    /// Whether the microphone is running, so mute can be relayed correctly.
    pub mic_running: AtomicBool,
    /// Used to emit call events to the frontend.
    pub app: AppHandle,
}
