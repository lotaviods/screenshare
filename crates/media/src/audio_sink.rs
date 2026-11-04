//! Audio output selection for the remote-audio decode pipeline.
//!
//! The sink element differs per host: `pulsesink` only exists in Linux
//! GStreamer builds, `wasapi2sink` in Windows builds, and `autoaudiosink`
//! adapts to whatever the host offers. Probing at runtime keeps the decode
//! pipeline identical on every platform and still plays audio when the
//! preferred element is absent.
//!
//! A sink is picked once per decoder; [`AudioSink::detect_with`] exists so the
//! preference order can be tested without a real GStreamer installation or
//! sound device.

use gstreamer as gst;

/// Remote-audio latency requested from the platform sinks, in microseconds.
///
/// Small enough for conversational audio; `autoaudiosink` (a `GstAutoDetect`
/// bin) has no such property and uses the element's own default.
const LATENCY_US: u32 = 60_000;

/// Audio sink used to play decoded remote audio.
///
/// The variants are host-specific, so a given build only constructs some of
/// them (`Pulse` on Linux, `Wasapi` on Windows, `Auto` as fallback).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSink {
    /// PulseAudio/PipeWire sink: Linux.
    Pulse,
    /// WASAPI render sink: Windows.
    Wasapi,
    /// Sink that picks a backend at runtime; last resort on any host.
    Auto,
}

impl AudioSink {
    /// Pick the sink for this host, preferring the platform-native element.
    pub fn detect() -> Self {
        Self::detect_with(|name| gst::ElementFactory::find(name).is_some())
    }

    /// Pick the first candidate accepted by `exists`.
    ///
    /// Fallible probing matters: a distro can ship GStreamer without the
    /// PulseAudio plugin, and `autoaudiosink` still routes audio then.
    pub fn detect_with(exists: impl Fn(&str) -> bool) -> Self {
        Self::candidates()
            .iter()
            .copied()
            .find(|sink| exists(sink.element()))
            .unwrap_or(Self::Auto)
    }

    /// Candidates in preference order for this build.
    fn candidates() -> &'static [AudioSink] {
        #[cfg(target_os = "linux")]
        {
            &[AudioSink::Pulse, AudioSink::Auto]
        }
        #[cfg(windows)]
        {
            &[AudioSink::Wasapi, AudioSink::Auto]
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            &[AudioSink::Auto]
        }
    }

    /// GStreamer factory name of the chosen sink.
    pub fn element(self) -> &'static str {
        match self {
            Self::Pulse => "pulsesink",
            Self::Wasapi => "wasapi2sink",
            Self::Auto => "autoaudiosink",
        }
    }

    /// Sink chain for a `gst::parse::launch` string.
    ///
    /// `sync=false` everywhere: the sender's RTP timestamps stop advancing
    /// while muted but the pipeline clock does not, so a syncing sink would
    /// consider every packet late and drop them after an unmute. Only the
    /// platform sinks extend `GstAudioBaseSink`, which is what exposes
    /// `buffer-time`; setting it on `autoaudiosink` would fail to parse.
    pub fn launch(self) -> String {
        match self {
            Self::Pulse | Self::Wasapi => {
                format!("{} sync=false buffer-time={LATENCY_US}", self.element())
            }
            Self::Auto => format!("{} sync=false", self.element()),
        }
    }
}

impl Default for AudioSink {
    fn default() -> Self {
        Self::detect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_the_platform_sink() {
        let best = AudioSink::detect_with(|_| true);
        #[cfg(target_os = "linux")]
        assert_eq!(best, AudioSink::Pulse);
        #[cfg(windows)]
        assert_eq!(best, AudioSink::Wasapi);
        #[cfg(not(any(target_os = "linux", windows)))]
        assert_eq!(best, AudioSink::Auto);
    }

    #[test]
    fn skips_missing_platform_sink() {
        let portable = AudioSink::detect_with(|name| name == "autoaudiosink");
        assert_eq!(portable, AudioSink::Auto);
    }

    #[test]
    fn platform_sink_gets_latency_but_auto_does_not() {
        assert_eq!(
            AudioSink::Pulse.launch(),
            "pulsesink sync=false buffer-time=60000"
        );
        assert_eq!(
            AudioSink::Wasapi.launch(),
            "wasapi2sink sync=false buffer-time=60000"
        );
        assert_eq!(AudioSink::Auto.launch(), "autoaudiosink sync=false");
    }
}
