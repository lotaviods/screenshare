//! Native audio playback for remote call audio.

#[cfg(target_os = "linux")]
pub(crate) use crate::pulse::PlaybackSink as Playback;
#[cfg(windows)]
pub(crate) use crate::wasapi::Playback;

/// A playback sink on the default output device.
pub struct Output {
    inner: Playback,
}

impl Output {
    /// Open the default output. `device` is reserved for the settings picker.
    pub fn open(_device: Option<String>) -> Result<Self, String> {
        Ok(Self {
            inner: Playback::open(None)?,
        })
    }

    /// Play interleaved 16-bit stereo PCM.
    pub fn play(&mut self, samples: &[i16]) {
        self.inner.play(samples);
    }

    pub fn stop(&mut self) {
        self.inner.stop();
    }
}
