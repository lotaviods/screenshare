//! Linux capture backend: PulseAudio/PipeWire through `libpulse-binding`.
//!
//! The microphone is a PulseAudio source; system audio records each
//! application playback stream and mixes them (see [`crate::pulse::Mixer`]), so
//! the call's own playback is never captured.

use crate::pulse::{Mixer, Recorder};

use super::Capture;

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
    // same either way. Fall back to libpulse when GStreamer is not installed.
    match crate::gst_capture::microphone(device.clone()) {
        Ok(capture) => return Ok(capture),
        Err(error) => tracing::warn!(%error, "falling back to native microphone capture"),
    }
    Recorder::open(device).map(|recorder| Box::new(recorder) as Box<dyn Capture>)
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
