//! `sharescreen-audio`: microphone and system-audio capture.
//!
//! Capture is native on both platforms: Linux records through
//! `libpulse-binding` (PulseAudio/PipeWire), Windows records through WASAPI
//! (process loopback for system audio). FFmpeg's Opus encoder produces the
//! packets. Linux system audio records each
//! application playback stream and mixes them, so the call's own playback is
//! never captured. Linux device enumeration uses `libpulse-binding` directly
//! (license-clean), not the GPL-licensed `pulsectl-rs` fork; Windows uses
//! WASAPI endpoint enumeration.
#![cfg(any(target_os = "linux", windows))]

#[cfg(target_os = "linux")]
mod native;
#[cfg(windows)]
mod native;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(windows)]
mod wasapi;

pub mod playback;
pub use playback::Output;

use async_trait::async_trait;
#[cfg(target_os = "linux")]
use sharescreen_core::DeviceKind;
use sharescreen_core::{AudioCapture, AudioError, Device, EncodedSample};
use tokio::sync::mpsc;

/// Microphone and system-audio capture engine. The two sources are fully
/// independent: separate capture threads, and separate volume controls.
pub struct AudioEngine {
    bitrate_kbps: u32,
    mic: Option<native::Stream>,
    system: Option<native::Stream>,
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self {
            bitrate_kbps: 96,
            mic: None,
            system: None,
        }
    }
}

impl AudioEngine {
    /// Set the Opus bitrate (kbps) used by streams started after this call.
    pub fn set_bitrate_kbps(&mut self, kbps: u32) {
        self.bitrate_kbps = kbps;
    }
}

/// Microphones as shown in the device picker.
#[cfg(target_os = "linux")]
fn microphones() -> Vec<Device> {
    pulse::enumerate()
        .map(|(sources, _)| sources)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.monitor_of_sink.is_none())
        .map(|s| Device {
            id: s.name,
            name: s.description,
            kind: DeviceKind::Microphone,
        })
        .collect()
}

/// Audio outputs (speakers/headphones) as shown in the device picker.
#[cfg(target_os = "linux")]
fn outputs() -> Vec<Device> {
    pulse::enumerate()
        .map(|(_, sinks)| sinks)
        .unwrap_or_default()
        .into_iter()
        .map(|s| Device {
            id: s.name,
            name: s.description,
            kind: DeviceKind::AudioOutput,
        })
        .collect()
}

/// Monitor sources usable for system-audio loopback.
#[cfg(target_os = "linux")]
fn system_monitors() -> Vec<Device> {
    pulse::enumerate()
        .map(|(sources, _)| sources)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.monitor_of_sink.is_some())
        .map(|s| Device {
            id: s.name,
            name: s.description,
            kind: DeviceKind::AudioOutput,
        })
        .collect()
}

#[cfg(windows)]
fn microphones() -> Vec<Device> {
    wasapi::microphones()
}

#[cfg(windows)]
fn outputs() -> Vec<Device> {
    wasapi::outputs()
}

/// WASAPI loopback captures a render device, so every output doubles as a
/// system-audio source.
#[cfg(windows)]
fn system_monitors() -> Vec<Device> {
    wasapi::outputs()
}

#[async_trait]
impl AudioCapture for AudioEngine {
    fn list_microphones(&self) -> Vec<Device> {
        microphones()
    }

    fn list_outputs(&self) -> Vec<Device> {
        outputs()
    }

    fn list_system_monitors(&self) -> Vec<Device> {
        system_monitors()
    }

    async fn start_mic(
        &mut self,
        device_id: Option<&str>,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), AudioError> {
        self.stop_mic().await.ok();
        self.mic = Some(native::spawn_mic(
            device_id.map(str::to_owned),
            self.bitrate_kbps,
            sink,
        )?);
        Ok(())
    }

    async fn stop_mic(&mut self) -> Result<(), AudioError> {
        if let Some(stream) = self.mic.take() {
            stream.stop();
        }
        Ok(())
    }

    async fn start_system(
        &mut self,
        device_id: Option<&str>,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), AudioError> {
        self.stop_system().await.ok();
        self.system = Some(native::spawn_system(
            device_id.map(str::to_owned),
            self.bitrate_kbps,
            sink,
        )?);
        Ok(())
    }

    async fn stop_system(&mut self) -> Result<(), AudioError> {
        if let Some(stream) = self.system.take() {
            stream.stop();
        }
        Ok(())
    }

    fn set_mic_volume(&mut self, volume: f32) -> Result<(), AudioError> {
        if let Some(stream) = &self.mic {
            stream.set_volume(volume as f64);
        }
        Ok(())
    }

    fn set_mic_muted(&mut self, muted: bool) -> Result<(), AudioError> {
        if let Some(stream) = &self.mic {
            stream.set_muted(muted);
        }
        Ok(())
    }

    fn set_system_volume(&mut self, volume: f32) -> Result<(), AudioError> {
        if let Some(stream) = &self.system {
            stream.set_volume(volume as f64);
        }
        Ok(())
    }
}
