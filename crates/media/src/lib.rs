//! `sharescreen-media`: ingests remote WebRTC tracks and decodes them.
//!
//! Remote tracks arrive as raw RTP (`TrackRemoteStaticRTP`), so the packets
//! are repackaged into full RTP buffers and fed into a GStreamer pipeline that
//! depayloads and decodes them:
//!
//! - audio → Opus → PCM → the system output (you hear the other side);
//! - video → codec → RGB → JPEG, kept as the latest frame for the UI.
//!
//! The video path deliberately stops at JPEG: the frames are small enough to
//! hand to the WebView, and a native video surface would need a Tauri plugin.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app;
use sharescreen_core::Codec;
use sharescreen_webrtc::{RemoteTrack, RemoteTrackKind};

mod audio_sink;

use audio_sink::AudioSink;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("gstreamer pipeline failed: {0}")]
    Pipeline(String),
    #[error("unsupported remote codec: {0}")]
    UnsupportedCodec(String),
}

/// A live decoder for one remote track.
pub struct Decoder {
    appsrc: Arc<Mutex<Option<gstreamer_app::AppSrc>>>,
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    frames: Option<Arc<Mutex<Option<Vec<u8>>>>>,
    volume: Arc<Mutex<Option<gst::Element>>>,
    clock_rate: u32,
}

/// Derive a presentation time from an RTP packet's header timestamp.
fn rtp_pts(buffer: &gst::Buffer, clock_rate: u32) -> Option<gst::ClockTime> {
    let map = buffer.map_readable().ok()?;
    let bytes = map.as_slice();
    if bytes.len() < 12 {
        return None;
    }
    let timestamp = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if clock_rate == 0 {
        return None;
    }
    Some(gst::ClockTime::from_nseconds(
        timestamp as u64 * 1_000_000_000 / clock_rate as u64,
    ))
}

impl Decoder {
    /// Create a decoder pipeline for a remote track of the given kind/codec.
    pub fn new(kind: RemoteTrackKind, codec: Codec) -> Result<Self, MediaError> {
        // `video_pipeline`/`audio_pipeline` build `gst::Caps`, which panics if
        // GStreamer has not been initialized yet. The first remote track can
        // arrive before any other GStreamer use in the process (e.g. a viewer
        // that is not sharing), so initialize here, before touching caps.
        ensure_gst_init();
        let (launch, caps) = match kind {
            RemoteTrackKind::Audio => audio_pipeline(codec)?,
            RemoteTrackKind::Video => video_pipeline(codec)?,
        };

        let frames = match kind {
            RemoteTrackKind::Video => Some(Arc::new(Mutex::new(None))),
            RemoteTrackKind::Audio => None,
        };
        let clock_rate = match kind {
            RemoteTrackKind::Video => 90_000,
            RemoteTrackKind::Audio => 48_000,
        };

        let running = Arc::new(AtomicBool::new(true));
        let appsrc: Arc<Mutex<Option<gstreamer_app::AppSrc>>> = Arc::new(Mutex::new(None));
        let volume: Arc<Mutex<Option<gst::Element>>> = Arc::new(Mutex::new(None));

        let thread = spawn_pipeline(
            launch,
            caps,
            running.clone(),
            appsrc.clone(),
            volume.clone(),
            frames.clone(),
        )?;

        Ok(Self {
            appsrc,
            running,
            thread,
            frames,
            volume,
            clock_rate,
        })
    }

    /// Push a full RTP packet into the decoder pipeline.
    pub fn push_rtp(&self, bytes: Vec<u8>) {
        // Wait briefly for the pipeline thread to publish its appsrc.
        for _ in 0..100 {
            if let Some(src) = self.appsrc.lock().unwrap().clone() {
                let mut buffer = gst::Buffer::from_mut_slice(bytes);
                // The depayloaders need a presentation time; derive it from
                // the RTP header timestamp and the stream's clock rate.
                if let Some(pts) = rtp_pts(&buffer, self.clock_rate) {
                    if let Some(raw) = buffer.get_mut() {
                        raw.set_pts(pts);
                    }
                }
                let _ = src.push_buffer(buffer);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The latest decoded video frame (JPEG), if any.
    pub fn latest_frame(&self) -> Option<Vec<u8>> {
        let slot = self.frames.as_ref()?;
        let guard = slot.lock().unwrap();
        guard.clone()
    }

    /// Set the playback volume (0.0..=2.0).
    pub fn set_volume(&self, volume: f64) {
        if let Some(element) = self.volume.lock().unwrap().as_ref() {
            element.set_property("volume", volume);
        }
    }

    /// Stop the pipeline and release its resources.
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.thread.join();
    }

    fn appsrc_handle(&self) -> Arc<Mutex<Option<gstreamer_app::AppSrc>>> {
        self.appsrc.clone()
    }
}

/// Decodes remote tracks and exposes the results to the UI.
pub struct RemoteMedia {
    tracks: RwLock<HashMap<String, TrackHandle>>,
    frames: RwLock<HashMap<String, Arc<Mutex<Option<Vec<u8>>>>>>,
    counter: AtomicU64,
}

struct TrackHandle {
    peer_id: String,
    kind: RemoteTrackKind,
    decoder: Decoder,
}

impl Default for RemoteMedia {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteMedia {
    pub fn new() -> Self {
        Self {
            tracks: RwLock::new(HashMap::new()),
            frames: RwLock::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// Attach a remote track: build a decoder pipeline and start feeding it
    /// with the track's RTP packets.
    pub async fn attach(&self, peer_id: &str, track: RemoteTrack) -> Result<(), MediaError> {
        let kind = track.kind().await;
        let codec = track.codec().await.unwrap_or(Codec::Opus);
        tracing::info!("remote track from {peer_id}: kind={kind:?} codec={codec:?}");
        let key = format!("{peer_id}#{}", self.counter.fetch_add(1, Ordering::Relaxed));

        let decoder = Decoder::new(kind, codec)?;
        if let Some(slot) = decoder.frames.clone() {
            self.frames
                .write()
                .unwrap()
                .insert(peer_id.to_string(), slot);
        }

        // Feed the track's RTP packets into the pipeline.
        let appsrc = decoder.appsrc_handle();
        let running = decoder.running.clone();
        tokio::spawn(async move {
            let clock_rate = match kind {
                RemoteTrackKind::Audio => 48_000,
                RemoteTrackKind::Video => 90_000,
            };
            feed(track, appsrc, running, clock_rate).await;
        });

        self.tracks.write().unwrap().insert(
            key,
            TrackHandle {
                peer_id: peer_id.to_string(),
                kind,
                decoder,
            },
        );
        Ok(())
    }

    /// Stop and drop every track belonging to a peer.
    pub fn detach(&self, peer_id: &str) {
        let keys: Vec<String> = self
            .tracks
            .read()
            .unwrap()
            .iter()
            .filter(|(_, h)| h.peer_id == peer_id)
            .map(|(k, _)| k.clone())
            .collect();

        for key in keys {
            if let Some(handle) = self.tracks.write().unwrap().remove(&key) {
                handle.decoder.stop();
            }
        }
        self.frames.write().unwrap().remove(peer_id);
    }

    /// The latest decoded video frame (JPEG) for a peer, if any.
    pub fn latest_frame(&self, peer_id: &str) -> Option<Vec<u8>> {
        let frames = self.frames.read().unwrap();
        let slot = frames.get(peer_id)?;
        let guard = slot.lock().unwrap();
        guard.clone()
    }

    /// Set the playback volume (0.0..=2.0) for a peer's audio.
    pub fn set_peer_volume(&self, peer_id: &str, volume: f64) {
        for handle in self.tracks.read().unwrap().values() {
            if handle.peer_id != peer_id || handle.kind != RemoteTrackKind::Audio {
                continue;
            }
            handle.decoder.set_volume(volume);
        }
    }

    /// Number of attached tracks.
    pub fn track_count(&self) -> usize {
        self.tracks.read().unwrap().len()
    }
}

/// The audio pipeline: RTP → Opus → PCM → system output.
fn audio_pipeline(codec: Codec) -> Result<(String, gst::Caps), MediaError> {
    match codec {
        Codec::Opus => Ok((
            format!(
                "appsrc name=src ! rtpopusdepay ! opusdec ! audioconvert ! audioresample ! \
                 audio/x-raw,rate=48000 ! volume name=vol ! {}",
                AudioSink::detect().launch()
            ),
            rtp_caps("audio", 48_000, "OPUS", 111),
        )),
        other => Err(MediaError::UnsupportedCodec(format!("{other:?}"))),
    }
}

/// The video pipeline: RTP → codec → RGB → JPEG.
///
/// `sync=false` on the appsink is essential: RTP timestamps are arbitrary, so
/// a syncing sink would wait for the clock to reach them before delivering a
/// frame.
fn video_pipeline(codec: Codec) -> Result<(String, gst::Caps), MediaError> {
    match codec {
        Codec::Vp8 => Ok((
            "appsrc name=src ! rtpvp8depay ! vp8dec ! videoconvert ! videoscale ! \
             jpegenc ! appsink name=sink sync=false"
                .to_string(),
            rtp_caps("video", 90_000, "VP8", 96),
        )),
        Codec::Vp9 => Ok((
            "appsrc name=src ! rtpvp9depay ! vp9dec ! videoconvert ! videoscale ! \
             jpegenc ! appsink name=sink sync=false"
                .to_string(),
            rtp_caps("video", 90_000, "VP9", 98),
        )),
        Codec::H264 => Ok((
            // The byte-stream output matters: the H.264 depayloader builds its
            // decoder config from `sprop-parameter-sets`, which WebRTC does
            // not send — the parameter sets arrive in-band instead. Decoding
            // the byte-stream directly lets the decoder pick them up.
            "appsrc name=src ! rtph264depay ! video/x-h264,stream-format=byte-stream,alignment=nal ! \
             avdec_h264 ! videoconvert ! videoscale ! jpegenc ! appsink name=sink sync=false"
                .to_string(),
            rtp_caps("video", 90_000, "H264", 102),
        )),
        other => Err(MediaError::UnsupportedCodec(format!("{other:?}"))),
    }
}

fn rtp_caps(media: &str, clock_rate: i32, encoding: &str, payload: i32) -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", media)
        .field("clock-rate", clock_rate)
        .field("encoding-name", encoding)
        .field("payload", payload)
        .build()
}

/// Build the pipeline and run it on a dedicated thread.
fn spawn_pipeline(
    launch: String,
    caps: gst::Caps,
    running: Arc<AtomicBool>,
    appsrc_slot: Arc<Mutex<Option<gstreamer_app::AppSrc>>>,
    volume_slot: Arc<Mutex<Option<gst::Element>>>,
    frames: Option<Arc<Mutex<Option<Vec<u8>>>>>,
) -> Result<JoinHandle<()>, MediaError> {
    std::thread::Builder::new()
        .name("sharescreen-media".into())
        .spawn(move || {
            ensure_gst_init();

            let element = match gst::parse::launch(&launch) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("failed to parse remote media pipeline: {e}");
                    return;
                }
            };
            let pipeline = match element.downcast::<gst::Pipeline>() {
                Ok(p) => p,
                Err(_) => {
                    tracing::error!("remote media pipeline did not produce a pipeline");
                    return;
                }
            };

            if let Some(src) = pipeline
                .by_name("src")
                .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            {
                src.set_caps(Some(&caps));
                // RTP depayloaders require a TIME-format segment; appsrc
                // defaults to BYTES.
                src.set_format(gst::Format::Time);
            }
            if let Some(vol) = pipeline.by_name("vol") {
                *volume_slot.lock().unwrap() = Some(vol);
            }

            if let Err(e) = pipeline.set_state(gst::State::Playing) {
                tracing::error!("failed to start remote media pipeline: {e}");
                let _ = pipeline.set_state(gst::State::Null);
                return;
            }

            // Publish the appsrc so the feeding task can push packets into it.
            if let Some(src) = pipeline
                .by_name("src")
                .and_then(|e| e.downcast::<gstreamer_app::AppSrc>().ok())
            {
                *appsrc_slot.lock().unwrap() = Some(src);
            }

            let sink = pipeline
                .by_name("sink")
                .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok());
            let bus = pipeline.bus();

            match (&frames, sink) {
                (Some(slot), Some(sink)) => {
                    let timeout = gst::ClockTime::from_mseconds(100);
                    let mut decoded: u64 = 0;
                    while running.load(Ordering::SeqCst) {
                        if let Some(sample) = sink.try_pull_sample(timeout) {
                            if let Some(buffer) = sample.buffer() {
                                if let Ok(map) = buffer.map_readable() {
                                    *slot.lock().unwrap() = Some(map.as_slice().to_vec());
                                    if decoded == 0 {
                                        tracing::info!(
                                            "decoded first video frame ({} bytes)",
                                            map.as_slice().len()
                                        );
                                    }
                                    decoded += 1;
                                }
                            }
                        }
                        report_bus_errors(&bus);
                    }
                }
                // Audio: the pipeline plays on its own; just keep it alive.
                _ => {
                    while running.load(Ordering::SeqCst) {
                        report_bus_errors(&bus);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }

            let _ = pipeline.set_state(gst::State::Null);
        })
        .map_err(|e| MediaError::Pipeline(format!("failed to spawn media thread: {e}")))
}

/// Log any pipeline errors so a failed decode is visible instead of silent.
fn report_bus_errors(bus: &Option<gst::Bus>) {
    use gst::MessageView;
    let Some(bus) = bus else { return };
    while let Some(msg) = bus.pop() {
        match msg.view() {
            MessageView::Error(err) => {
                tracing::error!("remote media pipeline error: {}", err.error());
            }
            _ => {}
        }
    }
}

/// Push the track's RTP packets into the pipeline's `appsrc`.
/// Presentation time in nanoseconds of an RTP timestamp, relative to the first
/// packet's timestamp `base`. Handles the 32-bit wraparound.
fn rtp_pts_ns(timestamp: u32, base: u32, clock_rate: u32) -> u64 {
    let ticks = timestamp.wrapping_sub(base) as u64;
    ticks * 1_000_000_000 / clock_rate as u64
}

async fn feed(
    track: RemoteTrack,
    appsrc_slot: Arc<Mutex<Option<gstreamer_app::AppSrc>>>,
    running: Arc<AtomicBool>,
    clock_rate: u32,
) {
    let mut base_ts: Option<u32> = None;
    // Wait until the pipeline thread publishes its appsrc.
    let src = loop {
        if let Some(src) = appsrc_slot.lock().unwrap().clone() {
            break src;
        }
        if !running.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    let mut packets: u64 = 0;
    let mut last_arrival: Option<std::time::Instant> = None;
    while running.load(Ordering::SeqCst) {
        let Some(packet) = track.next_packet().await else {
            tracing::info!("remote track ended after {packets} packets");
            return;
        };
        if packets == 0 {
            tracing::info!(
                "first RTP packet on remote track: pt={} len={}",
                packet.payload_type,
                packet.payload.len()
            );
        }
        let now = std::time::Instant::now();
        if let Some(prev) = last_arrival {
            let gap = now.duration_since(prev);
            if gap > Duration::from_secs(1) {
                tracing::info!(
                    "remote track resumed after {:.1}s without packets",
                    gap.as_secs_f32()
                );
            }
        }
        last_arrival = Some(now);
        packets += 1;
        if packets % 500 == 0 {
            tracing::info!("remote track: {packets} RTP packets received");
        }
        let mut buffer = gst::Buffer::from_mut_slice(packet.to_bytes());
        if let Some(raw) = buffer.get_mut() {
            // RTP timestamps count clock ticks from a random start. Convert the
            // ticks since the first packet to nanoseconds with the clock rate;
            // a sync'ing sink such as pulsesink would otherwise wait for a
            // presentation time billions of seconds away and play nothing.
            let base = *base_ts.get_or_insert(packet.timestamp);
            raw.set_pts(gst::ClockTime::from_nseconds(rtp_pts_ns(
                packet.timestamp,
                base,
                clock_rate,
            )));
        }
        // A full queue drops the packet; the next one follows immediately.
        let _ = src.push_buffer(buffer);
    }
}

fn ensure_gst_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(e) = gst::init() {
            tracing::error!("gst::init failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the first Opus packet of a call has a large random RTP
    /// timestamp. Its presentation time must start at 0, not billions of
    /// seconds ahead, or a syncing sink never plays it.
    #[test]
    fn pts_starts_at_zero_for_large_rtp_timestamps() {
        let base = 4_052_363_971;
        assert_eq!(rtp_pts_ns(base, base, 48_000), 0);
        // One 20 ms Opus packet later.
        assert_eq!(rtp_pts_ns(base + 960, base, 48_000), 20_000_000);
        // Still well under a second after 50 packets (1 s of audio).
        assert_eq!(rtp_pts_ns(base + 48_000, base, 48_000), 1_000_000_000);
    }

    #[test]
    fn pts_handles_rtp_timestamp_wraparound() {
        let base = u32::MAX - 100;
        assert_eq!(rtp_pts_ns(base.wrapping_add(960), base, 48_000), 20_000_000);
    }

    #[test]
    fn audio_pipeline_is_valid() {
        gst::init().unwrap();
        let (launch, caps) = audio_pipeline(Codec::Opus).unwrap();
        gst::parse::launch(&launch).expect("audio pipeline parses");
        let caps = caps.to_string();
        assert!(caps.contains("media=(string)audio"), "caps: {caps}");
        assert!(caps.contains("clock-rate=(int)48000"), "caps: {caps}");
    }

    #[test]
    fn video_pipelines_are_valid() {
        gst::init().unwrap();
        for codec in [Codec::Vp8, Codec::Vp9, Codec::H264] {
            let (launch, caps) = video_pipeline(codec).unwrap();
            gst::parse::launch(&launch)
                .unwrap_or_else(|e| panic!("{codec:?} pipeline failed to parse: {e}"));
            assert!(
                caps.to_string().contains("media=(string)video"),
                "caps: {caps}"
            );
        }
    }

    #[test]
    fn mismatched_codecs_are_rejected() {
        assert!(matches!(
            audio_pipeline(Codec::Vp8),
            Err(MediaError::UnsupportedCodec(_))
        ));
        assert!(matches!(
            video_pipeline(Codec::Opus),
            Err(MediaError::UnsupportedCodec(_))
        ));
    }

    #[test]
    fn decodes_vp8_rtp_into_a_jpeg_frame() {
        gst::init().unwrap();
        let decoder = Decoder::new(RemoteTrackKind::Video, Codec::Vp8).unwrap();

        // Produce real VP8 RTP packets with GStreamer. The payload type is
        // pinned to 96, which is what our SDP registration negotiates.
        let source = gst::parse::launch(
            "videotestsrc num-buffers=10 ! video/x-raw,width=320,height=240,framerate=10/1 ! \
             vp8enc ! rtpvp8pay pt=96 ! appsink name=rtpsink",
        )
        .expect("source pipeline parses");
        let source = source.downcast::<gst::Pipeline>().unwrap();
        let sink = source
            .by_name("rtpsink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        source.set_state(gst::State::Playing).unwrap();

        let timeout = gst::ClockTime::from_mseconds(500);
        let mut pushed = 0;
        // A VP8 keyframe spans many RTP packets, so push generously.
        while pushed < 400 {
            let Some(sample) = sink.try_pull_sample(timeout) else {
                break;
            };
            let Some(buffer) = sample.buffer() else {
                break;
            };
            let Ok(map) = buffer.map_readable() else {
                break;
            };
            let bytes = map.as_slice().to_vec();
            decoder.push_rtp(bytes);
            pushed += 1;
        }
        let _ = source.set_state(gst::State::Null);
        assert!(pushed > 0, "no RTP packets produced");

        let mut frame = None;
        for _ in 0..50 {
            if let Some(f) = decoder.latest_frame() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let frame = frame.expect("a decoded JPEG frame");
        assert!(frame.len() > 100, "frame too small: {} bytes", frame.len());
        assert_eq!(&frame[0..2], &[0xff, 0xd8], "not a JPEG");

        decoder.stop();
    }

    #[test]
    fn decodes_h264_rtp_into_a_jpeg_frame() {
        gst::init().unwrap();
        let decoder = Decoder::new(RemoteTrackKind::Video, Codec::H264).unwrap();

        // Real H.264 RTP packets. The source encodes constrained-baseline
        // Annex-B and aggregates the parameter sets into STAP-A, which is what
        // the WebRTC payloader emits.
        let source = gst::parse::launch(
            "videotestsrc num-buffers=10 ! video/x-raw,width=320,height=240,framerate=10/1 ! \
             x264enc tune=zerolatency speed-preset=veryfast bitrate=500 key-int-max=5 ! h264parse ! \
             video/x-h264,profile=constrained-baseline,stream-format=byte-stream,alignment=au ! \
             rtph264pay pt=102 aggregate-mode=zero-latency config-interval=-1 ! \
             appsink name=rtpsink",
        )
        .expect("source pipeline parses");
        let source = source.downcast::<gst::Pipeline>().unwrap();
        let sink = source
            .by_name("rtpsink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        source.set_state(gst::State::Playing).unwrap();

        let timeout = gst::ClockTime::from_mseconds(500);
        let mut pushed = 0;
        while pushed < 400 {
            let Some(sample) = sink.try_pull_sample(timeout) else {
                break;
            };
            let Some(buffer) = sample.buffer() else {
                break;
            };
            let Ok(map) = buffer.map_readable() else {
                break;
            };
            let bytes = map.as_slice().to_vec();
            decoder.push_rtp(bytes);
            pushed += 1;
        }
        let _ = source.set_state(gst::State::Null);
        assert!(pushed > 0, "no RTP packets produced");

        let mut frame = None;
        for _ in 0..50 {
            if let Some(f) = decoder.latest_frame() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let frame = frame.expect("a decoded JPEG frame");
        assert!(frame.len() > 100, "frame too small: {} bytes", frame.len());
        assert_eq!(&frame[0..2], &[0xff, 0xd8], "not a JPEG");

        decoder.stop();
    }
}
