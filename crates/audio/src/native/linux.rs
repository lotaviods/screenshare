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
    Recorder::open(device).map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}

pub(super) fn open_echo_reference() -> Result<Box<dyn Capture>, String> {
    Recorder::open_echo_reference().map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}

pub(super) fn open_system(device: Option<String>) -> Result<Box<dyn Capture>, String> {
    Mixer::open(device).map(|mixer| Box::new(mixer) as Box<dyn Capture>)
}
