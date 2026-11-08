//! GStreamer audio pipeline: `pulsesrc`/`autoaudiosrc` → Opus → appsink.
//!
//! Each stream runs on its own thread and exposes a handle to its `volume`
//! element so the engine can adjust volume/mute without touching the thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;

use gst::prelude::*;
use gstreamer as gst;
use tokio::sync::mpsc;

use sharescreen_core::{AudioError, Codec, EncodedSample, MediaKind};

/// A running audio capture stream.
pub struct Stream {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    control: Arc<Mutex<Option<gst::Element>>>,
}

impl Stream {
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.thread.join();
    }

    pub fn set_volume(&self, volume: f64) {
        if let Some(el) = self.control.lock().unwrap().as_ref() {
            el.set_property("volume", volume);
        }
    }

    pub fn set_muted(&self, muted: bool) {
        if let Some(el) = self.control.lock().unwrap().as_ref() {
            el.set_property("mute", muted);
        }
    }
}

/// Build and spawn an audio pipeline, returning its `Stream` handle.
pub fn spawn(launch: String, sink: mpsc::Sender<EncodedSample>) -> Result<Stream, AudioError> {
    let running = Arc::new(AtomicBool::new(true));
    let control: Arc<Mutex<Option<gst::Element>>> = Arc::new(Mutex::new(None));

    let thread = std::thread::Builder::new()
        .name("sharescreen-audio".into())
        .spawn({
            let running = running.clone();
            let control = control.clone();
            move || run(launch, running, control, sink)
        })
        .map_err(|e| AudioError::Failed(format!("failed to spawn audio thread: {e}")))?;

    Ok(Stream {
        running,
        thread,
        control,
    })
}

/// Microphone capture: default source, or a named PulseAudio/PipeWire source.
#[cfg(target_os = "linux")]
pub fn mic_launch(device: Option<&str>, bitrate_kbps: u32) -> String {
    let src = match device {
        Some(id) => format!("pulsesrc device={id}"),
        None => "autoaudiosrc".to_string(),
    };
    encode_chain(&src, bitrate_kbps)
}

/// System-audio capture: a monitor source, or `@DEFAULT_MONITOR@`.
#[cfg(target_os = "linux")]
pub fn system_launch(device: Option<&str>, bitrate_kbps: u32) -> String {
    let src = match device {
        Some(id) => format!("pulsesrc device={id}"),
        None => "pulsesrc device=@DEFAULT_MONITOR@".to_string(),
    };
    encode_chain(&src, bitrate_kbps)
}

/// Microphone capture through WASAPI: the default capture endpoint, or a
/// named one.
#[cfg(windows)]
pub fn mic_launch(device: Option<&str>, bitrate_kbps: u32) -> String {
    let src = match device {
        Some(id) => format!("wasapi2src device=\"{id}\""),
        None => "wasapi2src".to_string(),
    };
    encode_chain(&src, bitrate_kbps)
}

/// System-audio capture: WASAPI loopback on a render endpoint.
#[cfg(windows)]
pub fn system_launch(device: Option<&str>, bitrate_kbps: u32) -> String {
    let src = match device {
        Some(id) => format!("wasapi2src device=\"{id}\" loopback=true"),
        None => "wasapi2src loopback=true".to_string(),
    };
    encode_chain(&src, bitrate_kbps)
}

fn encode_chain(src: &str, bitrate_kbps: u32) -> String {
    format!(
        "{src} ! audioconvert ! audioresample ! \
         audio/x-raw,rate=48000,channels=2 ! \
         volume name=volume ! \
         opusenc bitrate={} ! \
         appsink name=sink sync=false max-buffers=2 drop=true",
        bitrate_kbps * 1000
    )
}

fn run(
    launch: String,
    running: Arc<AtomicBool>,
    control: Arc<Mutex<Option<gst::Element>>>,
    sink: mpsc::Sender<EncodedSample>,
) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(e) = gst::init() {
            tracing::error!("gst::init failed: {e}");
        }
    });

    let pipeline = match gst::parse::launch(&launch) {
        Ok(el) => match el.downcast::<gst::Pipeline>() {
            Ok(p) => p,
            Err(_) => {
                tracing::error!("audio launch produced a non-pipeline element");
                return;
            }
        },
        Err(e) => {
            tracing::error!("failed to build audio pipeline: {e}");
            return;
        }
    };

    // Publish the volume element for live volume/mute control.
    if let Some(vol) = pipeline.by_name("volume") {
        *control.lock().unwrap() = Some(vol);
    }

    let appsink = match pipeline
        .by_name("sink")
        .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok())
    {
        Some(s) => s,
        None => {
            tracing::error!("audio pipeline has no 'sink' appsink");
            let _ = pipeline.set_state(gst::State::Null);
            return;
        }
    };

    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        tracing::error!("failed to start audio pipeline: {e}");
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    let Some(bus) = pipeline.bus() else {
        tracing::error!("audio pipeline has no bus");
        let _ = pipeline.set_state(gst::State::Null);
        return;
    };

    let timeout = gst::ClockTime::from_mseconds(100);
    tracing::info!("audio capture started");
    let mut samples: u64 = 0;

    'outer: while running.load(Ordering::SeqCst) {
        if let Some(sample) = appsink.try_pull_sample(timeout) {
            if let Some(enc) = sample_to_encoded(&sample) {
                samples += 1;
                if samples == 1 || samples.is_multiple_of(250) {
                    tracing::info!("audio capture: {samples} encoded sample(s)");
                }
                if sink.blocking_send(enc).is_err() {
                    break 'outer; // consumer dropped
                }
            }
        }

        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    tracing::error!("audio pipeline error: {}", err.error());
                    break 'outer;
                }
                MessageView::Eos(_) => break 'outer,
                _ => {}
            }
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
}

fn sample_to_encoded(sample: &gst::Sample) -> Option<EncodedSample> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    let data = map.as_slice().to_vec();
    let timestamp_micros = buffer.pts().map(|p| p.useconds()).unwrap_or(0);
    let duration_micros = buffer.duration().map(|d| d.useconds()).unwrap_or(20_000);
    Some(EncodedSample {
        kind: MediaKind::Audio,
        codec: Codec::Opus,
        data,
        timestamp_micros,
        duration_micros,
        keyframe: false,
    })
}
