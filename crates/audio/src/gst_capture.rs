//! GStreamer audio capture: PulseAudio source → S16LE 48 kHz stereo → appsink.
//!
//! Capture only. The PCM goes through the same gain, WebRTC DSP (noise
//! suppression, echo cancellation) and Opus encoder as the native backend, so
//! switching the capture source never changes what peers receive.
//!
//! Device ids come from the PulseAudio device enumeration and are also valid
//! `pulsesrc` device names. System audio uses a monitor source name, which is a
//! regular PulseAudio source recording the output loopback.

use sharescreen_gst::{self as gst, PcmChunk, Runner};
use std::sync::mpsc::Receiver;
use tracing::{debug, warn};

use crate::native::Capture;

/// A capture source backed by a GStreamer pipeline.
struct GstAudio {
    runner: Runner,
    rx: Receiver<PcmChunk>,
    pending: Vec<i16>,
}

impl Capture for GstAudio {
    fn pump(&mut self) -> Result<(), String> {
        if let Some(error) = self.runner.failure() {
            return Err(error);
        }
        if self.runner.finished() {
            return Err("GStreamer audio pipeline stopped".into());
        }
        while let Ok(chunk) = self.rx.try_recv() {
            self.pending.extend_from_slice(&chunk);
        }
        // The encoder thread drains every 2 ms; a large backlog means the
        // consumer stalled. Bound it instead of growing without limit.
        const MAX_PENDING: usize = 48_000 * 2 * 2;
        if self.pending.len() > MAX_PENDING {
            let excess = self.pending.len() - MAX_PENDING;
            self.pending.drain(..excess);
            debug!("dropping backlogged GStreamer audio");
        }
        Ok(())
    }

    fn next(&mut self) -> Option<Vec<i16>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

impl Drop for GstAudio {
    fn drop(&mut self) {
        // The runner stops its pipeline on drop; audio capture is torn down
        // before the thread exits, so no buffers leak into a dead consumer.
    }
}

/// Microphone capture through GStreamer, or `None` when unavailable.
pub fn microphone(device: Option<String>) -> Result<Box<dyn Capture>, String> {
    start(device, "microphone")
}

fn start(device: Option<String>, label: &str) -> Result<Box<dyn Capture>, String> {
    if !gst::available() || !gst::has_element("pulsesrc") {
        return Err("GStreamer PulseAudio capture is not installed".into());
    }
    let device_clause = device
        .as_deref()
        .map(|id| format!(" device={}", gst::quote(id)))
        .unwrap_or_default();
    let description = format!(
        "pulsesrc{device_clause} ! audioconvert ! audioresample ! \
         audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved ! \
         appsink name=sink max-buffers=32 drop=true sync=false"
    );
    let (tx, rx) = std::sync::mpsc::sync_channel(32);
    let runner = Runner::start_audio(&description, "sink", move |pcm| {
        if tx.try_send(pcm).is_err() {
            debug!("GStreamer audio queue full; dropping a chunk");
        }
        Ok(())
    })
    .map_err(|error| {
        warn!(%error, "GStreamer {label} capture unavailable");
        error
    })?;
    tracing::info!("GStreamer {label} capture started");
    Ok(Box::new(GstAudio {
        runner,
        rx,
        pending: Vec::new(),
    }))
}
