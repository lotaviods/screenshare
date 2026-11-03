//! Shared GStreamer video pipeline: source → scale → rate → encode → appsink.
//!
//! Both the Wayland (`pipewiresrc`) and X11 (`ximagesrc`) backends share this
//! code; they differ only in the source element and how they obtain it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;

#[cfg(unix)]
use std::os::fd::OwnedFd;

use gstreamer as gst;
use gstreamer_app;
use gst::prelude::*;
use tokio::sync::mpsc;

use sharescreen_core::{CaptureError, CaptureOptions, Codec, EncodedSample, MediaKind};

/// Handle the capture thread holds for its lifetime. Linux keeps the PipeWire
/// fd alive; other hosts have nothing to keep.
#[cfg(unix)]
pub type KeepAlive = Option<OwnedFd>;
#[cfg(not(unix))]
pub type KeepAlive = ();

/// A running capture stream: a GStreamer thread pushing encoded samples.
pub struct Stream {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    /// Latest JPEG preview of the captured frames, for the local self-view.
    preview: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Stream {
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.thread.join();
    }

    /// Latest JPEG frame of what this stream is capturing, if any.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        self.preview.lock().unwrap().clone()
    }
}

/// Call `gst::init()` exactly once per process.
pub fn ensure_gst_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(e) = gst::init() {
            tracing::error!("gst::init failed: {e}");
        }
    });
}

/// Whether an element with the given factory name is available.
pub fn element_exists(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Hardware encoder candidates for a codec, in preference order.
#[cfg(target_os = "linux")]
pub fn hw_candidates(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &["vah264enc", "vaapih264enc", "nvh264enc", "qsvh264enc"],
        Codec::Vp9 => &["vavp9enc", "vaapivp9enc"],
        _ => &["vavp8enc", "vaapivp8enc"],
    }
}

/// Hardware encoder candidates for a codec, in preference order.
#[cfg(windows)]
pub fn hw_candidates(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &["mfh264enc", "nvh264enc", "qsvh264enc"],
        Codec::Vp9 => &["nvvp9enc"],
        _ => &["nvvp8enc"],
    }
}

/// Encoder `bitrate` unit: x264enc and the vpx encoders take kbit/s, Media
/// Foundation and openh264 take bit/s.
#[cfg(target_os = "linux")]
fn bitrate_prop(kbps: u32) -> u32 {
    kbps
}

#[cfg(windows)]
fn bitrate_prop(kbps: u32) -> u32 {
    kbps * 1000
}

/// Software encoder chain for a codec. Returns `(encoder elements, appsink caps)`.
///
/// The H.264 caps pin the format the WebRTC payloader needs: Annex-B
/// start codes (`stream-format=byte-stream`) and the constrained-baseline
/// profile the SDP advertises (`profile-level-id=42e01f`).
pub fn software_chain(codec: Codec, bitrate_kbps: u32) -> (String, &'static str) {
    match codec {
        #[cfg(target_os = "linux")]
        Codec::H264 => (
            format!(
                "x264enc tune=zerolatency speed-preset=veryfast key-int-max=60 bitrate={bitrate_kbps} ! h264parse"
            ),
            "video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au",
        ),
        // openh264enc is the always-available H.264 encoder in the Windows
        // GStreamer distribution.
        #[cfg(windows)]
        Codec::H264 => (
            format!("openh264enc bitrate={} ! h264parse", bitrate_prop(bitrate_kbps)),
            "video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au",
        ),
        Codec::Vp9 => (
            format!(
                "vp9enc deadline=1 cpu-used=6 keyframe-max-dist=60 target-bitrate={}",
                bitrate_kbps * 1000
            ),
            "video/x-vp9",
        ),
        _ => (
            format!(
                "vp8enc deadline=1 cpu-used=6 keyframe-max-dist=60 target-bitrate={}",
                bitrate_kbps * 1000
            ),
            "video/x-vp8",
        ),
    }
}

/// Pick the encoder chain for a codec. When `hardware` is true, prefer the first
/// available hardware encoder (per `exists`); otherwise fall back to software.
pub fn encoder_chain(
    codec: Codec,
    bitrate_kbps: u32,
    hardware: bool,
    exists: &dyn Fn(&str) -> bool,
) -> (String, &'static str) {
    if hardware {
        for enc in hw_candidates(codec) {
            if exists(enc) {
                let chain = format!("{enc} bitrate={}", bitrate_prop(bitrate_kbps));
                return match codec {
                    Codec::H264 => (
                        format!("{chain} ! h264parse"),
                        "video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au",
                    ),
                    _ => (
                        chain,
                        if codec == Codec::Vp9 {
                            "video/x-vp9"
                        } else {
                            "video/x-vp8"
                        },
                    ),
                };
            }
        }
    }
    software_chain(codec, bitrate_kbps)
}

/// Build a full capture pipeline: `source` → scale → rate → tee, where one
/// branch encodes for the call and the other produces JPEG self-view frames.
pub fn build_launch(source: &str, opts: &CaptureOptions) -> String {
    let (enc, caps) =
        encoder_chain(opts.codec, opts.bitrate_kbps, opts.hardware_encoding, &element_exists);
    format!(
        "{source} ! videoconvert ! videoscale ! \
         video/x-raw,width={w},height={h} ! \
         videorate ! video/x-raw,framerate={fps}/1 ! \
         tee name=t \
         t. ! queue ! {enc} ! \
         appsink name=sink sync=false max-buffers=2 drop=true caps={caps} \
         t. ! queue ! videorate ! video/x-raw,framerate=10/1 ! \
         jpegenc quality=70 ! \
         appsink name=preview sync=false max-buffers=1 drop=true",
        w = opts.width,
        h = opts.height,
        fps = opts.fps,
    )
}

/// Spawn the pipeline on a dedicated thread, pushing encoded samples to `sink`.
/// `keep_alive` is held for the thread's lifetime (used for the PipeWire fd on
/// the Wayland backend).
pub fn spawn(
    launch: String,
    sink: mpsc::Sender<EncodedSample>,
    codec: Codec,
    keep_alive: KeepAlive,
) -> Result<Stream, CaptureError> {
    let running = Arc::new(AtomicBool::new(true));
    let preview = Arc::new(Mutex::new(None));
    let thread = std::thread::Builder::new()
        .name("sharescreen-capture".into())
        .spawn({
            let running = running.clone();
            let preview = preview.clone();
            move || {
                let _keep_alive = keep_alive;
                run_pipeline(launch, running, preview, sink, codec);
            }
        })
        .map_err(|e| CaptureError::Failed(format!("failed to spawn capture thread: {e}")))?;

    Ok(Stream {
        running,
        thread,
        preview,
    })
}

fn run_pipeline(
    launch: String,
    running: Arc<AtomicBool>,
    preview: Arc<Mutex<Option<Vec<u8>>>>,
    sink: mpsc::Sender<EncodedSample>,
    codec: Codec,
) {
    ensure_gst_init();
    tracing::info!("starting capture pipeline: {launch}");

    let pipeline = match gst::parse::launch(&launch) {
        Ok(el) => match el.downcast::<gst::Pipeline>() {
            Ok(p) => p,
            Err(_) => {
                tracing::error!("capture pipeline description did not produce a pipeline");
                return;
            }
        },
        Err(e) => {
            tracing::error!("failed to build capture pipeline: {e}");
            return;
        }
    };

    let appsink = match pipeline
        .by_name("sink")
        .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok())
    {
        Some(s) => s,
        None => {
            tracing::error!("capture pipeline has no 'sink' appsink");
            let _ = pipeline.set_state(gst::State::Null);
            return;
        }
    };

    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        tracing::error!("failed to start capture pipeline: {e}");
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    let Some(bus) = pipeline.bus() else {
        tracing::error!("capture pipeline has no bus");
        let _ = pipeline.set_state(gst::State::Null);
        return;
    };

    let preview_sink = pipeline
        .by_name("preview")
        .and_then(|el| el.downcast::<gstreamer_app::AppSink>().ok());

    let timeout = gst::ClockTime::from_mseconds(100);

    'outer: while running.load(Ordering::SeqCst) {
        if let Some(sample) = appsink.try_pull_sample(timeout) {
            if let Some(enc) = sample_to_encoded(&sample, codec) {
                if enc.timestamp_micros == 0 || enc.keyframe {
                    tracing::debug!(
                        "capture sample: {} bytes keyframe={}",
                        enc.data.len(),
                        enc.keyframe
                    );
                }
                if sink.blocking_send(enc).is_err() {
                    break 'outer; // consumer dropped
                }
            }
        }

        // Keep the self-view branch drained even when the UI is not polling.
        if let Some(preview_sink) = &preview_sink {
            while let Some(sample) = preview_sink.try_pull_sample(gst::ClockTime::ZERO) {
                if let Some(buffer) = sample.buffer() {
                    if let Ok(map) = buffer.map_readable() {
                        *preview.lock().unwrap() = Some(map.as_slice().to_vec());
                    }
                }
            }
        }

        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    tracing::error!("capture pipeline error: {}", err.error());
                    break 'outer;
                }
                MessageView::Eos(_) => break 'outer,
                _ => {}
            }
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
}

fn sample_to_encoded(sample: &gst::Sample, codec: Codec) -> Option<EncodedSample> {
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    let data = map.as_slice().to_vec();
    let timestamp_micros = buffer.pts().map(|p| p.useconds()).unwrap_or(0);
    let duration_micros = buffer.duration().map(|d| d.useconds()).unwrap_or(33_333);
    let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
    Some(EncodedSample {
        kind: MediaKind::Video,
        codec,
        data,
        timestamp_micros,
        duration_micros,
        keyframe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sharescreen_core::CursorMode;

    fn opts(codec: Codec) -> CaptureOptions {
        CaptureOptions {
            width: 1920,
            height: 1080,
            fps: 30,
            cursor: CursorMode::Embedded,
            codec,
            bitrate_kbps: 2500,
            hardware_encoding: false,
        }
    }

    #[test]
    fn vp8_software_chain() {
        let (chain, caps) = software_chain(Codec::Vp8, 2500);
        assert_eq!(
            chain,
            "vp8enc deadline=1 cpu-used=6 keyframe-max-dist=60 target-bitrate=2500000"
        );
        assert_eq!(caps, "video/x-vp8");
    }

    #[test]
    fn h264_software_chain_includes_parser() {
        let (chain, caps) = software_chain(Codec::H264, 2500);
        assert_eq!(chain, "x264enc tune=zerolatency speed-preset=veryfast key-int-max=60 bitrate=2500 ! h264parse");
        // Annex-B start codes and the profile the SDP advertises.
        assert_eq!(
            caps,
            "video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au"
        );
    }

    #[test]
    fn h264_caps_carry_annex_b_start_codes() {
        // The WebRTC H.264 payloader splits frames on Annex-B start codes, so
        // the encoder must produce them.
        gst::init().unwrap();
        let (chain, caps) = software_chain(Codec::H264, 500);
        let launch = format!(
            "videotestsrc num-buffers=1 ! video/x-raw,width=320,height=240 ! \
             {chain} ! {caps} ! appsink name=sink"
        );
        let pipeline = gst::parse::launch(&launch)
            .expect("pipeline parses")
            .downcast::<gst::Pipeline>()
            .unwrap();
        let sink = pipeline
            .by_name("sink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let sample = sink
            .try_pull_sample(gst::ClockTime::from_mseconds(2000))
            .expect("a sample");
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        let bytes = map.as_slice();
        assert!(
            bytes.windows(4).any(|w| w == [0, 0, 0, 1] || w[..3] == [0, 0, 1]),
            "no Annex-B start code in the encoded frame"
        );
        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn hardware_prefers_first_available_candidate() {
        // Simulate a machine with vah264enc available.
        let (chain, caps) = encoder_chain(Codec::H264, 2500, true, &|n| n == "vah264enc");
        assert_eq!(chain, "vah264enc bitrate=2500 ! h264parse");
        assert_eq!(
            caps,
            "video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au"
        );
    }

    #[test]
    fn hardware_falls_back_when_none_available() {
        let (chain, caps) = encoder_chain(Codec::Vp8, 2500, true, &|_| false);
        assert_eq!(
            chain,
            "vp8enc deadline=1 cpu-used=6 keyframe-max-dist=60 target-bitrate=2500000"
        );
        assert_eq!(caps, "video/x-vp8");
    }

    #[test]
    fn launch_has_expected_shape() {
        let s = build_launch("pipewiresrc fd=42 path=7 do-timestamp=true", &opts(Codec::Vp8));
        assert!(s.starts_with("pipewiresrc fd=42 path=7"));
        assert!(s.contains("video/x-raw,width=1920,height=1080"));
        assert!(s.contains("framerate=30/1"));
        assert!(s.contains("vp8enc deadline=1 cpu-used=6 keyframe-max-dist=60 target-bitrate=2500000"));
        assert!(s.contains("name=sink"));
        assert!(s.contains("name=preview"));
    }

    #[test]
    fn preview_branch_emits_jpeg() {
        // The self-view branch must actually produce decodable JPEG frames.
        gst::init().unwrap();
        let launch = build_launch("videotestsrc num-buffers=10", &opts(Codec::Vp8));
        let pipeline = gst::parse::launch(&launch)
            .expect("pipeline parses")
            .downcast::<gst::Pipeline>()
            .unwrap();
        let preview = pipeline
            .by_name("preview")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let sample = preview
            .try_pull_sample(gst::ClockTime::from_mseconds(3000))
            .expect("a preview frame");
        let buffer = sample.buffer().unwrap();
        let map = buffer.map_readable().unwrap();
        assert!(
            map.as_slice().starts_with(&[0xff, 0xd8]),
            "preview frame is not a JPEG"
        );
        pipeline.set_state(gst::State::Null).unwrap();
    }
}
