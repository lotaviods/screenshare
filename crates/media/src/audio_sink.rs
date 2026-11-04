use gstreamer as gst;

const LATENCY_US: u32 = 60_000;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSink {
    Pulse,
    Wasapi,
    Auto,
}

impl AudioSink {
    pub fn detect() -> Self {
        Self::detect_with(|name| gst::ElementFactory::find(name).is_some())
    }

    pub fn detect_with(exists: impl Fn(&str) -> bool) -> Self {
        Self::candidates()
            .iter()
            .copied()
            .find(|sink| exists(sink.element()))
            .unwrap_or(Self::Auto)
    }

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

    pub fn element(self) -> &'static str {
        match self {
            Self::Pulse => "pulsesink",
            Self::Wasapi => "wasapi2sink",
            Self::Auto => "autoaudiosink",
        }
    }

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
