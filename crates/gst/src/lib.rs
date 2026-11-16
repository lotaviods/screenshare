//! GStreamer pipeline runner shared by the capture and audio backends.
//!
//! A [`Runner`] owns one `gst-launch`-style pipeline on its own thread and
//! delivers the buffers pulled from a named `appsink` as plain Rust data. The
//! callers (screen capture, microphone, system audio) never touch GStreamer
//! types, so the encoders stay replaceable and platform pipelines stay strings.
//!
//! The crate is empty unless the `gst-runtime` feature is enabled. Only the
//! Linux backends enable it, so a Windows workspace build needs no GStreamer
//! SDK.
#![cfg(feature = "gst-runtime")]

use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use tracing::{debug, error, warn};

/// One encoded video access unit pulled from an `appsink`.
#[derive(Debug, Clone)]
pub struct VideoPacket {
    /// Encoded bytes as produced by the encoder (Annex-B for H.264, a whole
    /// frame for VP8/VP9), ready for RTP packetization.
    pub data: Vec<u8>,
    /// False when the encoder marked the buffer as a delta unit.
    pub keyframe: bool,
    pub pts_ns: Option<u64>,
    pub duration_ns: Option<u64>,
}

/// Interleaved 48 kHz stereo S16LE samples pulled from an `appsink`.
pub type PcmChunk = Vec<i16>;

/// Initialize GStreamer once per process.
pub fn init() -> Result<(), String> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| gst::init().map_err(|e| e.to_string()))
        .clone()
}

/// Whether this host can run the pipelines built by the backends.
pub fn available() -> bool {
    init().is_ok() && has_element("appsink")
}

/// Whether a named GStreamer element is installed.
pub fn has_element(name: &str) -> bool {
    // `ElementFactory::find` panics before `gst::init`, so every caller gets
    // the one-time initialization for free instead of having to remember it.
    if init().is_err() {
        return false;
    }
    gst::ElementFactory::find(name).is_some()
}

/// Quote a value for a `gst-launch` description string.
pub fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A running pipeline delivering buffers from one named `appsink`.
pub struct Runner {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Runner {
    /// Start a pipeline that delivers encoded video from `appsink_name`.
    pub fn start_video(
        description: &str,
        appsink_name: &str,
        on_packet: impl FnMut(VideoPacket) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        let mut on_packet = on_packet;
        Self::start(description, appsink_name, move |sample| {
            if let Some(packet) = video_packet(sample) {
                on_packet(packet)?;
            }
            Ok(())
        })
    }

    /// Start a pipeline that delivers S16LE 48 kHz stereo audio from
    /// `appsink_name`.
    pub fn start_audio(
        description: &str,
        appsink_name: &str,
        on_pcm: impl FnMut(PcmChunk) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        let mut on_pcm = on_pcm;
        Self::start(description, appsink_name, move |sample| {
            if let Some(pcm) = pcm_chunk(sample) {
                on_pcm(pcm)?;
            }
            Ok(())
        })
    }

    fn start(
        description: &str,
        appsink_name: &str,
        on_sample: impl FnMut(&gst::Sample) -> Result<(), String> + Send + 'static,
    ) -> Result<Self, String> {
        init()?;
        debug!(pipeline = description, "starting GStreamer pipeline");
        let element = gst::parse::launch(description).map_err(|e| e.to_string())?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| "pipeline description is not a pipeline".to_string())?;
        let appsink = pipeline
            .by_name(appsink_name)
            .ok_or_else(|| format!("pipeline has no element named {appsink_name}"))?
            .downcast::<AppSink>()
            .map_err(|_| format!("{appsink_name} is not an appsink"))?;
        appsink.set_max_buffers(4);
        appsink.set_drop(true);

        let failure = Arc::new(Mutex::new(None));
        let handler = Arc::new(Mutex::new(on_sample));
        let callback_failure = failure.clone();
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = match appsink.pull_sample() {
                        Ok(sample) => sample,
                        Err(_) => return Ok(gst::FlowSuccess::Ok),
                    };
                    match handler.lock() {
                        Ok(mut handler) => match handler(&sample) {
                            Ok(()) => Ok(gst::FlowSuccess::Ok),
                            Err(error) => {
                                record_failure(&callback_failure, error);
                                Err(gst::FlowError::Error)
                            }
                        },
                        Err(_) => Err(gst::FlowError::Error),
                    }
                })
                .build(),
        );

        let bus = pipeline
            .bus()
            .ok_or_else(|| "pipeline has no bus".to_string())?;
        let thread_pipeline = pipeline.clone();
        let thread_bus = bus.clone();
        let thread_failure = failure.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        let thread = std::thread::Builder::new()
            .name("sharescreen-gst".into())
            .spawn(move || {
                let main_loop = gst::glib::MainLoop::new(None, false);
                let watch_loop = main_loop.clone();
                let watch_failure = thread_failure.clone();
                let watch = thread_bus.add_watch(move |_, message| {
                    match message.view() {
                        gst::MessageView::Eos(..) => watch_loop.quit(),
                        gst::MessageView::Error(error) => {
                            let text = match error.debug() {
                                Some(debug) => format!("{} ({debug})", error.error()),
                                None => error.error().to_string(),
                            };
                            record_failure(&watch_failure, text);
                            watch_loop.quit();
                        }
                        gst::MessageView::Application(_) => watch_loop.quit(),
                        _ => {}
                    }
                    gst::glib::ControlFlow::Continue
                });
                if let Err(error) = watch {
                    let _ = ready_tx.send(Err(format!("cannot watch pipeline bus: {error}")));
                    return;
                }
                match thread_pipeline.set_state(gst::State::Playing) {
                    Ok(_) => {
                        let _ = ready_tx.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("pipeline failed to start: {error}")));
                        let _ = thread_pipeline.set_state(gst::State::Null);
                        return;
                    }
                }
                main_loop.run();
                let _ = thread_pipeline.set_state(gst::State::Null);
            })
            .map_err(|e| format!("failed to spawn GStreamer thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                pipeline,
                bus,
                failure,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err("GStreamer thread exited before starting".into())
            }
        }
    }

    /// First error reported by the pipeline, if any.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }

    /// Whether the pipeline thread has stopped.
    pub fn finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
    }

    /// Ask the pipeline thread to stop and wait for it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            let stop = gst::message::Application::builder(
                gst::Structure::builder("sharescreen-stop").build(),
            )
            .build();
            if let Err(error) = self.bus.post(stop) {
                warn!(%error, "failed to post the pipeline stop message");
            }
            let _ = self.pipeline.set_state(gst::State::Null);
            if thread.join().is_err() {
                error!("GStreamer pipeline thread panicked");
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn record_failure(failure: &Arc<Mutex<Option<String>>>, message: String) {
    error!(%message, "GStreamer pipeline failure");
    if let Ok(mut failure) = failure.lock() {
        if failure.is_none() {
            *failure = Some(message);
        }
    }
}

fn video_packet(sample: &gst::Sample) -> Option<VideoPacket> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
    let pts_ns = buffer.pts().map(|pts| pts.nseconds());
    let duration_ns = buffer
        .duration()
        .map(|duration| duration.nseconds())
        .or_else(|| {
            let structure = sample.caps()?.structure(0)?;
            let framerate: gst::Fraction = structure.get("framerate").ok()?;
            (framerate.numer() > 0 && framerate.denom() > 0)
                .then(|| 1_000_000_000u64 * framerate.denom() as u64 / framerate.numer() as u64)
        });
    Some(VideoPacket {
        data: map.as_slice().to_vec(),
        keyframe,
        pts_ns,
        duration_ns,
    })
}

fn pcm_chunk(sample: &gst::Sample) -> Option<PcmChunk> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    let (pairs, _) = map.as_slice().as_chunks::<2>();
    Some(
        pairs
            .iter()
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_pipeline_values() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a \"b\" c"), "\"a \\\"b\\\" c\"");
        assert_eq!(quote("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn missing_elements_are_reported() {
        init().expect("GStreamer initializes");
        assert!(!has_element("no-such-gstreamer-element"));
    }

    /// Exercises the runner end to end. Skips when the host lacks any of the
    /// plugins, so a minimal GStreamer install does not fail the suite.
    #[test]
    fn video_pipeline_delivers_encoded_packets() {
        if !available() || !has_element("videotestsrc") || !has_element("x264enc") {
            return;
        }
        let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = delivered.clone();
        let runner = Runner::start_video(
            "videotestsrc num-buffers=6 ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! \
             x264enc tune=zerolatency speed-preset=ultrafast ! \
             video/x-h264,stream-format=byte-stream,alignment=au ! \
             appsink name=sink sync=false",
            "sink",
            move |packet| {
                assert!(!packet.data.is_empty());
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("test pipeline starts");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while delivered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "no encoded packet");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        runner.stop();
    }

    #[test]
    fn audio_pipeline_delivers_pcm() {
        if !available() || !has_element("audiotestsrc") {
            return;
        }
        let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = delivered.clone();
        let runner = Runner::start_audio(
            "audiotestsrc num-buffers=4 ! audioconvert ! audioresample ! \
             audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved ! \
             appsink name=sink sync=false",
            "sink",
            move |pcm| {
                assert!(!pcm.is_empty());
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )
        .expect("test pipeline starts");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while delivered.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "no PCM chunk");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        runner.stop();
    }
}
