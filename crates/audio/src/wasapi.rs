//! WASAPI device enumeration through GStreamer's `wasapi2deviceprovider`.
//!
//! Enumeration only; capture goes through the shared Opus pipeline in
//! [`crate::pipeline`] with `wasapi2src` as the source element.

use gst::prelude::*;
use gstreamer as gst;

use sharescreen_core::{Device, DeviceKind};

/// Capture devices of one GStreamer device class.
fn devices(class: &str) -> Vec<gst::Device> {
    if gst::init().is_err() {
        return Vec::new();
    }
    let Some(factory) = gst::DeviceProviderFactory::find("wasapi2deviceprovider") else {
        return Vec::new();
    };
    let Some(provider) = factory.get() else {
        return Vec::new();
    };
    if provider.start().is_err() {
        return Vec::new();
    }

    let found = provider
        .devices()
        .iter()
        .filter(|device| device.device_class() == class)
        .cloned()
        .collect();

    provider.stop();
    found
}

/// Microphone inputs.
pub fn microphones() -> Vec<Device> {
    list("Audio/Source", DeviceKind::Microphone)
}

/// Render endpoints, also used as system-audio loopback sources.
pub fn outputs() -> Vec<Device> {
    list("Audio/Sink", DeviceKind::AudioOutput)
}

fn list(class: &str, kind: DeviceKind) -> Vec<Device> {
    devices(class)
        .into_iter()
        .map(|device| {
            // `wasapi2src device=` takes the provider's device name; fall back
            // to the friendly name when the property is absent.
            let id = device
                .properties()
                .and_then(|props| props.get::<String>("device.name").ok())
                .unwrap_or_else(|| device.display_name().to_string());
            Device {
                id,
                name: device.display_name().to_string(),
                kind,
            }
        })
        .collect()
}
