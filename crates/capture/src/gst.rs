//! GStreamer capture: XDG portal PipeWire source → encoder → `appsink`.
//!
//! This is the default Linux backend. The portal picker is shared with the
//! native FFmpeg backend ([`crate::portal`]); only the frames differ: GStreamer
//! negotiates scaling/framerate and encodes with VAAPI when installed, falling
//! back to the software x264/VP8/VP9 encoders.
//!
//! The encoder runs on GStreamer's threads and delivers whole access units;
//! the backend only copies bytes into [`EncodedSample`] and never blocks the
//! streaming thread on a slow peer.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::Ordering;

use ashpd::desktop::screencast::Screencast;
use ashpd::desktop::Session;
use async_trait::async_trait;
use sharescreen_core::{
    CaptureError, CaptureOptions, CaptureSource, Codec, EncodedSample, MediaKind, ScreenCapture,
    SourceKind,
};
use sharescreen_gst::{self as gst, Runner, VideoPacket};
use tokio::sync::mpsc;

use crate::portal;

/// GStreamer-backed screen capture. `start` blocks on the compositor picker,
/// then streams encoded video from a pipeline thread.
#[derive(Default)]
pub struct GstCapture {
    active: Option<ActiveCapture>,
}

struct ActiveCapture {
    runner: Runner,
    /// Keeps the portal session and its fd alive for the stream lifetime.
    _session: Session<Screencast>,
    _fd: OwnedFd,
}

impl GstCapture {
    pub fn failure(&self) -> Option<String> {
        self.active
            .as_ref()
            .and_then(|active| active.runner.failure())
    }

    pub fn finished(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.runner.finished())
    }

    /// The pipeline does not publish JPEG previews; the call path decodes the
    /// outgoing stream instead.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        None
    }
}

pub fn available() -> bool {
    sharescreen_gst::available() && sharescreen_gst::has_element("pipewiresrc")
}

#[async_trait]
impl ScreenCapture for GstCapture {
    fn list_sources(&self) -> Vec<CaptureSource> {
        // The compositor's portal picker owns source selection.
        vec![CaptureSource {
            id: "screen".into(),
            name: "Screen (portal picker)".into(),
            kind: SourceKind::Screen,
        }]
    }

    async fn start(
        &mut self,
        _source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError> {
        let plans = encoder_plans(&options).ok_or_else(|| {
            CaptureError::Unavailable(format!("GStreamer has no encoder for {:?}", options.codec))
        })?;

        self.stop().await.ok();
        let portal = portal::open(options.cursor).await?;
        let fd = portal.fd.as_raw_fd();
        let mut last_error = None;
        for plan in plans {
            let description = pipeline_description(portal.node_id, fd, &options, &plan);
            let codec = options.codec;
            let keyframe_request = options.keyframe_request.clone();
            let mut base_pts: Option<u64> = None;
            let fallback = sink.clone();
            let started = Runner::start_video(&description, "sink", move |packet| {
                if keyframe_request
                    .as_ref()
                    .is_some_and(|request| request.swap(false, Ordering::AcqRel))
                {
                    // The GStreamer encoders expose no portable force-keyframe
                    // call; the periodic GOP bounds recovery instead.
                    tracing::debug!("keyframe request ignored; waiting for the next GOP keyframe");
                }
                let Some(sample) = sample_from(packet, &mut base_pts, options.fps.max(1), codec)
                else {
                    return Ok(());
                };
                match fallback.try_send(sample) {
                    Ok(()) => Ok(()),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!("video queue full; dropping encoded frame");
                        Ok(())
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        Err("video consumer disconnected".into())
                    }
                }
            });
            match started {
                Ok(runner) => {
                    tracing::info!(
                        element = plan.element,
                        codec = ?options.codec,
                        "GStreamer screen capture started"
                    );
                    self.active = Some(ActiveCapture {
                        runner,
                        _session: portal.session,
                        _fd: portal.fd,
                    });
                    return Ok(());
                }
                Err(error) => {
                    tracing::warn!(element = plan.element, %error, "GStreamer encoder pipeline failed");
                    last_error = Some(error);
                }
            }
        }
        Err(CaptureError::Failed(last_error.unwrap_or_else(|| {
            "no GStreamer encoder pipeline could be started".into()
        })))
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        if let Some(active) = self.active.take() {
            // Close the portal session first so the compositor stops producing.
            let _ = active._session.close().await;
            active.runner.stop();
        }
        Ok(())
    }
}

/// One candidate encoder pipeline. The first one that starts wins.
struct EncoderPlan {
    element: &'static str,
    props: String,
    /// Pixel format and output caps the encoder needs.
    input_format: &'static str,
    output_caps: &'static str,
}

fn encoder_plans(options: &CaptureOptions) -> Option<Vec<EncoderPlan>> {
    let gop = options.fps.max(1) * 2;
    let kbps = options.bitrate_kbps;
    let bps = kbps.saturating_mul(1000);
    let mut plans = Vec::new();

    let hardware = match options.codec {
        Codec::H264 => [("vah264enc", "NV12"), ("vaapih264enc", "NV12")]
            .into_iter()
            .filter(|(element, _)| gst::has_element(element))
            .map(|(element, format)| EncoderPlan {
                element,
                props: format!("bitrate={kbps} key-int-max={gop} rate-control=cbr"),
                input_format: format,
                output_caps: "video/x-h264,stream-format=byte-stream,alignment=au",
            })
            .collect::<Vec<_>>(),
        Codec::Vp8 => ["vavp8enc", "vaapivp8enc"]
            .into_iter()
            .filter(|element| gst::has_element(element))
            .map(|element| EncoderPlan {
                element,
                props: format!("bitrate={kbps} keyframe-period={gop}"),
                input_format: "NV12",
                output_caps: "video/x-vp8",
            })
            .collect(),
        Codec::Vp9 => ["vavp9enc", "vaapivp9enc"]
            .into_iter()
            .filter(|element| gst::has_element(element))
            .map(|element| EncoderPlan {
                element,
                props: format!("bitrate={kbps} keyframe-period={gop}"),
                input_format: "NV12",
                output_caps: "video/x-vp9",
            })
            .collect(),
        _ => Vec::new(),
    };
    if options.hardware_encoding {
        plans.extend(hardware);
    }

    let software = match options.codec {
        Codec::H264 => gst::has_element("x264enc").then(|| EncoderPlan {
            element: "x264enc",
            props: format!(
                "tune=zerolatency speed-preset=ultrafast bitrate={kbps} key-int-max={gop} bframes=0"
            ),
            input_format: "I420",
            output_caps: "video/x-h264,stream-format=byte-stream,alignment=au",
        }),
        Codec::Vp8 => gst::has_element("vp8enc").then(|| EncoderPlan {
            element: "vp8enc",
            props: format!(
                "deadline=1 cpu-used=8 target-bitrate={bps} keyframe-max-dist={gop} lag-in-frames=0"
            ),
            input_format: "I420",
            output_caps: "video/x-vp8",
        }),
        Codec::Vp9 => gst::has_element("vp9enc").then(|| EncoderPlan {
            element: "vp9enc",
            props: format!(
                "deadline=1 cpu-used=8 target-bitrate={bps} keyframe-max-dist={gop} lag-in-frames=0"
            ),
            input_format: "I420",
            output_caps: "video/x-vp9",
        }),
        _ => None,
    };
    plans.extend(software);

    (!plans.is_empty()).then_some(plans)
}

fn pipeline_description(
    node_id: u32,
    fd: i32,
    options: &CaptureOptions,
    plan: &EncoderPlan,
) -> String {
    format!(
        "pipewiresrc path={node_id} fd={fd} ! \
         videoconvert ! videoscale ! videorate ! \
         video/x-raw,format={},width={},height={},framerate={}/1 ! \
         {} {} ! {} ! \
         appsink name=sink max-buffers=4 drop=true sync=false",
        plan.input_format,
        options.width.max(1),
        options.height.max(1),
        options.fps.max(1),
        plan.element,
        plan.props,
        plan.output_caps,
    )
}

/// Convert one encoder buffer to a transport sample, normalizing timestamps to
/// the first frame of this capture.
fn sample_from(
    packet: VideoPacket,
    base_pts: &mut Option<u64>,
    fps: u32,
    codec: Codec,
) -> Option<EncodedSample> {
    let frame_ns = 1_000_000_000u64 / fps as u64;
    let pts_ns = packet
        .pts_ns
        .unwrap_or_else(|| base_pts.unwrap_or(0) + frame_ns);
    let base = *base_pts.get_or_insert(pts_ns);
    let relative_ns = pts_ns.saturating_sub(base);
    Some(EncodedSample {
        kind: MediaKind::Video,
        codec,
        data: packet.data,
        timestamp_micros: relative_ns / 1_000,
        duration_micros: packet.duration_ns.unwrap_or(frame_ns) / 1_000,
        keyframe: packet.keyframe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sharescreen_core::CursorMode;

    fn options(codec: Codec, hardware: bool) -> CaptureOptions {
        CaptureOptions {
            width: 1280,
            height: 720,
            fps: 30,
            cursor: CursorMode::Embedded,
            codec,
            bitrate_kbps: 2500,
            hardware_encoding: hardware,
            thumbnail_sender: None,
            keyframe_request: None,
        }
    }

    #[test]
    fn unsupported_codecs_have_no_plan() {
        assert!(encoder_plans(&options(Codec::Av1, false)).is_none());
        assert!(encoder_plans(&options(Codec::Opus, false)).is_none());
    }

    #[test]
    fn always_offers_a_software_encoder() {
        let plans = encoder_plans(&options(Codec::H264, false)).expect("software H.264");
        assert!(plans.iter().any(|plan| plan.element == "x264enc"));
        assert_eq!(plans.last().unwrap().input_format, "I420");
    }

    #[test]
    fn pipeline_scales_and_caps_the_source() {
        let plans = encoder_plans(&options(Codec::Vp8, false)).expect("software VP8");
        let description = pipeline_description(42, 7, &options(Codec::Vp8, false), &plans[0]);
        assert!(description.contains("pipewiresrc path=42 fd=7"));
        assert!(description.contains("width=1280,height=720,framerate=30/1"));
        assert!(description.contains("appsink name=sink"));
    }

    #[test]
    fn timestamps_are_relative_to_the_first_frame() {
        let packet = |pts_ns| VideoPacket {
            data: vec![1],
            keyframe: true,
            pts_ns: Some(pts_ns),
            duration_ns: None,
        };
        let mut base = None;
        let first = sample_from(packet(5_000_000_000), &mut base, 30, Codec::H264).unwrap();
        let second = sample_from(packet(5_033_000_000), &mut base, 30, Codec::H264).unwrap();
        assert_eq!(first.timestamp_micros, 0);
        assert_eq!(second.timestamp_micros, 33_000);
        assert_eq!(first.duration_micros, 33_333);
    }
}
