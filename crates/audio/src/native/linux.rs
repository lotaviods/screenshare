//! Linux capture backend: PulseAudio/PipeWire through `libpulse-binding`.
//!
//! The microphone is a PulseAudio source; system audio records each
//! application playback stream and mixes them (see [`crate::pulse::Mixer`]), so
//! the call's own playback is never captured.

use std::time::{Duration, Instant};

use crate::pulse::{Mixer, Recorder};

use super::Capture;

/// How long a GStreamer source gets to prove it delivers audio. `pulsesrc` can
/// start on a busy or half-broken device and then never produce a buffer, which
/// used to leave the call silent because the worker only checks for errors.
const GSTREAMER_PROBE: Duration = Duration::from_millis(300);

impl Capture for Recorder {
    fn pump(&mut self) -> Result<(), String> {
        Recorder::pump(self);
        Ok(())
    }

    fn next(&mut self) -> Option<Vec<i16>> {
        let pcm = self.take();
        if pcm.is_empty() {
            None
        } else {
            Some(pcm)
        }
    }
}

impl Capture for Mixer {
    fn pump(&mut self) -> Result<(), String> {
        Mixer::pump(self);
        Ok(())
    }

    fn next(&mut self) -> Option<Vec<i16>> {
        self.chunk()
    }
}

pub(super) fn open_mic(device: Option<String>) -> Result<Box<dyn Capture>, String> {
    // Prefer the GStreamer capture source; the shared gain/DSP/Opus path is the
    // same either way. A source that starts but delivers nothing is treated as
    // a failure, so the native backend takes over instead of sharing silence.
    match crate::gst_capture::microphone(device.clone()) {
        Ok(mut capture) => {
            if delivers_audio(capture.as_mut()) {
                return Ok(capture);
            }
            tracing::warn!("GStreamer microphone produced no audio; falling back to libpulse");
        }
        Err(error) => tracing::warn!(%error, "falling back to native microphone capture"),
    }
    Recorder::open(device).map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}

/// Pump a capture for a short window and report whether any PCM arrived.
fn delivers_audio(capture: &mut dyn Capture) -> bool {
    let deadline = Instant::now() + GSTREAMER_PROBE;
    while Instant::now() < deadline {
        if capture.pump().is_err() {
            return false;
        }
        if capture.next().is_some_and(|pcm| !pcm.is_empty()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

pub(super) fn open_echo_reference() -> Result<Box<dyn Capture>, String> {
    Recorder::open_echo_reference().map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}

pub(super) fn open_system(device: Option<String>) -> Result<Box<dyn Capture>, String> {
    // System audio stays on the native mixer: it records each application
    // playback stream and excludes the call's own output. A plain monitor
    // source would feed remote voices back to peers.
    Mixer::open(device).map(|mixer| Box::new(mixer) as Box<dyn Capture>)
}
