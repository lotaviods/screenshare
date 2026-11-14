//! Windows capture backend: WASAPI.
//!
//! The microphone is a capture endpoint; system audio uses process loopback
//! (Windows 10 build 20348+) with `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`,
//! so the call's own playback is never captured. See [`crate::wasapi`].

use crate::wasapi::Recorder;

use super::Capture;

impl Capture for Recorder {
    fn pump(&mut self) -> Result<(), String> {
        Recorder::pump(self)
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

pub(super) fn open_mic(device: Option<String>) -> Result<Box<dyn Capture>, String> {
    Recorder::open_mic(device).map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}

pub(super) fn open_echo_reference() -> Result<Box<dyn Capture>, String> {
    Recorder::open_echo_reference().map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}

pub(super) fn open_system(device: Option<String>) -> Result<Box<dyn Capture>, String> {
    Recorder::open_system(device).map(|recorder| Box::new(recorder) as Box<dyn Capture>)
}
