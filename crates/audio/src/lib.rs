//! `sharescreen-audio`: microphone and system-audio capture.
//!
//! Capture is done through GStreamer by default (`pulsesrc`/`autoaudiosrc` for
//! the mic, `wasapi2src` on Windows) → Opus → appsink. With
//! `SHARESCREEN_MEDIA=native` on Linux, PulseAudio records the source directly
//! and FFmpeg's libopus encoder produces the packets instead, skipping
//! GStreamer entirely. Linux system audio records each application playback
//! stream through `libpulse-binding` and mixes them, so the call's own playback
//! is never captured. Linux device enumeration uses `libpulse-binding` directly
//! (license-clean), not the GPL-licensed `pulsectl-rs` fork; Windows enumeration
//! uses GStreamer's WASAPI device provider.
#![cfg(any(target_os = "linux", windows))]

mod pipeline;

#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(windows)]
mod wasapi;

use async_trait::async_trait;
#[cfg(target_os = "linux")]
use sharescreen_core::DeviceKind;
use sharescreen_core::{AudioCapture, AudioError, Device, EncodedSample};
use tokio::sync::mpsc;

/// Either encode pipeline behind one handle.
enum ActiveStream {
    Gst(pipeline::Stream),
    #[cfg(target_os = "linux")]
    Native(native::Stream),
}

impl ActiveStream {
    fn stop(self) {
        match self {
            Self::Gst(stream) => stream.stop(),
            #[cfg(target_os = "linux")]
            Self::Native(stream) => stream.stop(),
        }
    }

    fn set_volume(&self, volume: f64) {
        match self {
            Self::Gst(stream) => stream.set_volume(volume),
            #[cfg(target_os = "linux")]
            Self::Native(stream) => stream.set_volume(volume),
        }
    }

    fn set_muted(&self, muted: bool) {
        match self {
            Self::Gst(stream) => stream.set_muted(muted),
            #[cfg(target_os = "linux")]
            Self::Native(stream) => stream.set_muted(muted),
        }
    }
}

/// Microphone capture: native when opted in and usable, GStreamer otherwise.
fn mic_stream(
    device: Option<&str>,
    bitrate_kbps: u32,
    sink: mpsc::Sender<EncodedSample>,
) -> Result<ActiveStream, AudioError> {
    #[cfg(target_os = "linux")]
    if native::available() {
        tracing::info!("audio: native PulseAudio -> Opus path");
        return Ok(ActiveStream::Native(native::spawn_mic(
            device.map(str::to_owned),
            bitrate_kbps,
            sink,
        )?));
    }
    Ok(ActiveStream::Gst(pipeline::spawn(
        pipeline::mic_launch(device, bitrate_kbps),
        sink,
    )?))
}

#[cfg(target_os = "linux")]
fn system_stream(
    device: Option<&str>,
    bitrate_kbps: u32,
    sink: mpsc::Sender<EncodedSample>,
) -> Result<ActiveStream, AudioError> {
    if native::available() {
        tracing::info!("audio: native PulseAudio -> Opus path");
        return Ok(ActiveStream::Native(native::spawn_system(
            device.map(str::to_owned),
            bitrate_kbps,
            sink,
        )?));
    }
    Ok(ActiveStream::Gst(pipeline::spawn_system(
        device.map(str::to_owned),
        bitrate_kbps,
        sink,
    )?))
}

#[cfg(windows)]
fn system_stream(
    _device: Option<&str>,
    bitrate_kbps: u32,
    sink: mpsc::Sender<EncodedSample>,
) -> Result<ActiveStream, AudioError> {
    if !wasapi::supports_process_loopback() {
        return Err(AudioError::Unavailable(
            "system audio without call echo requires Windows 10 build 20348 or later".into(),
        ));
    }
    Ok(ActiveStream::Gst(pipeline::spawn(
        pipeline::system_launch(None, bitrate_kbps),
        sink,
    )?))
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

/// Microphone and system-audio capture engine. The two sources are fully
/// independent: separate GStreamer pipelines, threads, and volume controls.
pub struct AudioEngine {
    bitrate_kbps: u32,
    mic: Option<ActiveStream>,
    system: Option<ActiveStream>,
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
        self.mic = Some(mic_stream(device_id, self.bitrate_kbps, sink)?);
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
        self.system = Some(system_stream(device_id, self.bitrate_kbps, sink)?);
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

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::pipeline;

    #[test]
    fn mic_launch_default_uses_autoaudiosrc() {
        let s = pipeline::mic_launch(None, 96);
        assert!(s.starts_with("autoaudiosrc !"));
        assert!(s.contains("opusenc bitrate=96000"));
        assert!(s.contains("volume name=volume"));
        assert!(s.contains("name=sink"));
    }

    #[test]
    fn mic_launch_with_device_uses_pulsesrc() {
        let s = pipeline::mic_launch(Some("alsa_input.usb-Fifine"), 128);
        assert!(s.starts_with("pulsesrc device=alsa_input.usb-Fifine"));
        assert!(s.contains("opusenc bitrate=128000"));
    }

    #[test]
    fn system_capture_uses_per_stream_monitoring() {
        // A mixed output monitor would replay remote participants to the call.
        let self_pid = 42;
        assert_eq!(super::pulse::capture_policy(Some(42), self_pid), None);
        assert_eq!(super::pulse::capture_policy(Some(43), self_pid), Some(43));
        assert_eq!(super::pulse::capture_policy(None, self_pid), None);
    }
}

#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use super::pipeline;

    #[test]
    fn mic_launch_default_uses_wasapi() {
        let s = pipeline::mic_launch(None, 96);
        assert!(s.starts_with("wasapi2src !"));
        assert!(s.contains("opusenc bitrate=96000"));
    }

    #[test]
    fn system_launch_enables_loopback() {
        let s = pipeline::system_launch(None, 96);
        assert!(s.contains(&format!("loopback-target-pid={}", std::process::id())));
        assert!(s.contains("loopback-mode=exclude-process-tree"));
        assert!(s.contains("opusenc bitrate=96000"));
    }

    #[test]
    fn process_loopback_does_not_capture_an_endpoint_mix() {
        let s = pipeline::system_launch(Some("Speakers (USB Audio)"), 128);
        assert!(!s.contains("device="));
        assert!(s.contains("loopback=true"));
    }
}
