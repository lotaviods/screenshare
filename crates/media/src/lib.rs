//! `sharescreen-media`: ingests remote WebRTC tracks and decodes them.
//!
//! Remote tracks arrive as raw RTP packets. The packets are depacketized and
//! decoded with FFmpeg:
//!
//! - audio → Opus → PCM → the system output (you hear the other side);
//! - video → H.264/VP8/VP9 → RGB → JPEG, kept as the latest frame for the UI.
//!
//! The video path deliberately stops at JPEG: the frames are small enough to
//! hand to the GPUI renderer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;

use sharescreen_audio::Output;
use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_native_media::decode::{
    JpegEncoder, OpusDecoder, RtpDepacketizer, RtpKind, VideoDecoder,
};
use sharescreen_native_media::preview::{preview_size, Demand, MAX_PREVIEW_WIDTH};
use sharescreen_webrtc::{RemoteTrack, RemoteTrackKind};

pub use sharescreen_native_media::linux_preview::LinuxPreviewer;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("native pipeline failed: {0}")]
    Pipeline(String),
    #[error("unsupported remote codec: {0}")]
    UnsupportedCodec(String),
}

/// A live decoder for one remote track.
pub struct Decoder {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    frames: Option<Arc<Mutex<Option<Vec<u8>>>>>,
    volume: Arc<Mutex<f64>>,
    tx: mpsc::Sender<Vec<u8>>,
    stop: watch::Sender<bool>,
}

/// Caps queued remote RTP at roughly 256 packets per track (about 5 seconds
/// for 20 ms audio, less for video). `attach` awaits capacity, keeping packet
/// memory bounded while allowing the WebRTC reader to apply backpressure.
const REMOTE_RTP_QUEUE: usize = 256;

impl Decoder {
    /// Create a decoder for a remote track of the given kind/codec.
    pub fn new(kind: RemoteTrackKind, codec: Codec) -> Result<Self, MediaError> {
        let rtp_kind = match codec {
            Codec::H264 => RtpKind::H264,
            Codec::Vp8 => RtpKind::Vp8,
            Codec::Vp9 => RtpKind::Vp9,
            Codec::Opus => RtpKind::Opus,
            other => return Err(MediaError::UnsupportedCodec(format!("{other:?}"))),
        };

        let frames = match kind {
            RemoteTrackKind::Video => Some(Arc::new(Mutex::new(None))),
            RemoteTrackKind::Audio => None,
        };
        let running = Arc::new(AtomicBool::new(true));
        let volume = Arc::new(Mutex::new(1.0));
        let (tx, mut rx) = mpsc::channel(REMOTE_RTP_QUEUE);
        let (stop, _) = watch::channel(false);

        let flag = running.clone();
        let volume_slot = volume.clone();
        let frames_slot = frames.clone();
        let thread_flag = flag.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("sharescreen-media".into())
            .spawn(move || {
                run(
                    kind,
                    codec,
                    rtp_kind,
                    &mut rx,
                    flag,
                    volume_slot,
                    frames_slot,
                );
                thread_flag.store(false, Ordering::SeqCst);
                thread_stop.send_replace(true);
            })
            .map_err(|e| MediaError::Pipeline(format!("failed to spawn media thread: {e}")))?;

        Ok(Self {
            running,
            thread,
            frames,
            volume,
            tx,
            stop,
        })
    }

    /// Push a full RTP packet into the decoder.
    pub fn push_rtp(&self, bytes: Vec<u8>) {
        if let Err(error) = self.tx.try_send(bytes) {
            tracing::debug!(
                "dropping RTP packet because the decoder queue is full or closed: {error}"
            );
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
        *self.volume.lock().unwrap() = volume;
    }

    /// Stop the pipeline and release its resources.
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        self.stop.send_replace(true);
        let _ = self.thread.join();
    }
}

fn run(
    kind: RemoteTrackKind,
    codec: Codec,
    rtp_kind: RtpKind,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    running: Arc<AtomicBool>,
    volume: Arc<Mutex<f64>>,
    frames: Option<Arc<Mutex<Option<Vec<u8>>>>>,
) {
    let mut depacketizer = RtpDepacketizer::new(rtp_kind);
    let mut video = if kind == RemoteTrackKind::Video {
        match VideoDecoder::new(codec_name(codec)) {
            Ok(decoder) => Some(decoder),
            Err(e) => {
                tracing::error!("failed to create video decoder: {e}");
                return;
            }
        }
    } else {
        None
    };
    let mut jpeg = match kind {
        RemoteTrackKind::Video => match JpegEncoder::new() {
            Ok(encoder) => Some(encoder),
            Err(e) => {
                tracing::error!("failed to create JPEG encoder: {e}");
                return;
            }
        },
        RemoteTrackKind::Audio => None,
    };
    let mut audio = match kind {
        RemoteTrackKind::Audio => match OpusDecoder::new() {
            Ok(decoder) => Some(decoder),
            Err(e) => {
                tracing::error!("failed to create Opus decoder: {e}");
                return;
            }
        },
        RemoteTrackKind::Video => None,
    };
    let mut output = match kind {
        RemoteTrackKind::Audio => match Output::open(None) {
            Ok(output) => Some(output),
            Err(e) => {
                tracing::error!("failed to open audio output: {e}");
                return;
            }
        },
        RemoteTrackKind::Video => None,
    };

    let mut decoded = 0u64;
    // `output` is passed in instead of captured so the receive timeout can
    // also pump the playback buffer while no packets arrive.
    let mut process = |frame: &[u8], output: &mut Option<Output>| match kind {
        RemoteTrackKind::Video => {
            let (Some(video), Some(jpeg)) = (video.as_mut(), jpeg.as_mut()) else {
                return;
            };
            match video.decode(frame) {
                Ok(rgb_frames) => {
                    for rgb in rgb_frames {
                        match jpeg.encode(&rgb, video.width(), video.height()) {
                            Ok(jpeg) => {
                                if decoded == 0 {
                                    tracing::info!(
                                        "decoded first video frame ({} bytes)",
                                        jpeg.len()
                                    );
                                }
                                decoded += 1;
                                if let Some(slot) = frames.as_ref() {
                                    *slot.lock().unwrap() = Some(jpeg);
                                }
                            }
                            Err(e) => tracing::warn!("JPEG encode failed: {e}"),
                        }
                    }
                }
                Err(e) => tracing::warn!("video decode failed: {e}"),
            }
        }
        RemoteTrackKind::Audio => {
            let (Some(audio), Some(output)) = (audio.as_mut(), output.as_mut()) else {
                return;
            };
            match audio.decode(frame) {
                Ok(pcm) => {
                    let gain = *volume.lock().unwrap();
                    let scaled: Vec<i16> = pcm
                        .iter()
                        .map(|sample| (*sample as f64 * gain).clamp(-32768.0, 32767.0) as i16)
                        .collect();
                    // Queue decoded audio; the jitter buffer releases it on the
                    // 20 ms playback cadence instead of writing every packet
                    // straight to the device.
                    output.push(&scaled);
                }
                Err(e) => tracing::warn!("Opus decode failed: {e}"),
            }
        }
    };

    while running.load(Ordering::SeqCst) {
        match rx.try_recv() {
            Ok(packet) => {
                for frame in depacketizer.push(&packet) {
                    process(&frame, &mut output);
                }
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {
                for frame in depacketizer.flush() {
                    process(&frame, &mut output);
                }
                if let Some(output) = output.as_mut() {
                    output.tick();
                }
                // Keep stop responsive while still servicing audio during
                // packet gaps. The media queue itself remains bounded.
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::Vp8 => "vp8",
        Codec::Vp9 => "vp9",
        _ => "h264",
    }
}

/// Renders the local screen share the way a peer receives it.
///
/// The outgoing access units are decoded again with the same decoder the
/// remote path uses, instead of re-encoding the raw captured frame. The
/// self-view therefore shows the real negotiated stream: codec, resolution,
/// frame rate and compression artifacts.
///
/// Nothing runs while the UI is not polling, the tap never blocks the peer
/// path, and the JPEG stage runs only as fast as the UI consumes frames.
/// Decoding itself is in order and cannot skip access units, so a sample that
/// cannot be queued resets the decoder and lets it resynchronize.
pub struct LocalPreview {
    running: Arc<AtomicBool>,
    tx: std_mpsc::SyncSender<EncodedSample>,
    frames: Arc<Mutex<Option<Vec<u8>>>>,
    /// Set by every poll, cleared when the worker produces a frame: keeps the
    /// JPEG stage from outrunning the UI.
    consumed: Arc<AtomicBool>,
    demand: Arc<Demand>,
    thread: Option<JoinHandle<()>>,
}

/// Samples the worker may have queued before it is considered behind.
const PREVIEW_QUEUE: usize = 4;

/// Access units to wait for a keyframe flag before decoding anyway.
///
/// A hardware encoder may not flag keyframes. Waiting forever would leave the
/// self-view blank, so after this many samples the decoder starts mid-GOP
/// (exactly what a peer joining late sees) and recovers at the next IDR.
const PREVIEW_KEYFRAME_WAIT: u32 = 90;

impl LocalPreview {
    /// Start the loopback decoder. Never fails: a preview that cannot start
    /// simply produces no frames.
    pub fn new() -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let frames = Arc::new(Mutex::new(None));
        let consumed = Arc::new(AtomicBool::new(true));
        let demand = Arc::new(Demand::new());
        let (tx, rx) = std_mpsc::sync_channel(PREVIEW_QUEUE);

        let flag = running.clone();
        let slot = frames.clone();
        let eaten = consumed.clone();
        let wanted = demand.clone();
        let thread = std::thread::Builder::new()
            .name("sharescreen-selfview".into())
            .spawn(move || preview_loop(rx, flag, slot, eaten, wanted))
            .map_err(|e| tracing::error!("failed to spawn self-view thread: {e}"))
            .ok();

        Self {
            running,
            tx,
            frames,
            consumed,
            demand,
            thread,
        }
    }

    /// Offer one outgoing sample to the loopback. Audio is ignored.
    pub fn push(&self, sample: &EncodedSample) {
        if sample.kind != MediaKind::Video || sample.data.is_empty() {
            return;
        }
        if !self.demand.wanted() {
            return;
        }
        // Full queue means the decoder is behind the capture rate.
        let _ = self.tx.try_send(sample.clone());
    }

    /// The latest decoded self-view frame (JPEG), if any.
    pub fn latest_frame(&self) -> Option<Vec<u8>> {
        self.demand.touch();
        self.consumed.store(true, Ordering::Release);
        self.frames.lock().unwrap().clone()
    }
}

impl Default for LocalPreview {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LocalPreview {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn preview_loop(
    rx: std_mpsc::Receiver<EncodedSample>,
    running: Arc<AtomicBool>,
    frames: Arc<Mutex<Option<Vec<u8>>>>,
    consumed: Arc<AtomicBool>,
    demand: Arc<Demand>,
) {
    let mut jpeg = match JpegEncoder::new() {
        Ok(encoder) => encoder,
        Err(e) => {
            tracing::error!("self-view JPEG encoder unavailable: {e}");
            return;
        }
    };
    let mut decoder: Option<VideoDecoder> = None;
    let mut codec: Option<Codec> = None;
    let mut waiting_keyframe = true;
    let mut waited = 0u32;

    while running.load(Ordering::SeqCst) {
        let sample = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(sample) => sample,
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if !demand.wanted() {
            // Nobody is looking: drop the decoder so a later request starts
            // from a keyframe instead of a stale reference frame.
            decoder = None;
            codec = None;
            waiting_keyframe = true;
            waited = 0;
            continue;
        }

        if codec != Some(sample.codec) {
            codec = Some(sample.codec);
            decoder = match VideoDecoder::new(codec_name(sample.codec)) {
                Ok(created) => Some(created),
                Err(e) => {
                    tracing::warn!("self-view cannot decode {:?}: {e}", sample.codec);
                    None
                }
            };
            waiting_keyframe = true;
        }
        let Some(video) = decoder.as_mut() else {
            continue;
        };
        if waiting_keyframe {
            if sample.keyframe {
                waiting_keyframe = false;
                waited = 0;
            } else {
                waited += 1;
                if waited <= PREVIEW_KEYFRAME_WAIT {
                    continue;
                }
                // Some hardware encoders never flag keyframes. Waiting forever
                // would leave the self-view blank, so start mid-GOP instead.
                waiting_keyframe = false;
                waited = 0;
            }
        }
        match video.decode(&sample.data) {
            Ok(pixels) => {
                // The decoder must run in order, but the JPEG stage only needs
                // to run as fast as the UI consumes frames. Skipping a frame
                // nobody read leaves the previous JPEG in place.
                if !consumed.swap(false, Ordering::AcqRel) {
                    continue;
                }
                for rgb in pixels {
                    let (width, height) = (video.width(), video.height());
                    let (dst_width, dst_height) = preview_size(width, height, MAX_PREVIEW_WIDTH);
                    if dst_width == 0 || dst_height == 0 {
                        continue;
                    }
                    match jpeg.encode_rgb_scaled(&rgb, width, height, dst_width, dst_height) {
                        Ok(bytes) => *frames.lock().unwrap() = Some(bytes),
                        Err(e) => tracing::warn!("self-view JPEG encode failed: {e}"),
                    }
                }
            }
            Err(e) => {
                tracing::warn!("self-view decode failed, waiting for a keyframe: {e}");
                decoder = None;
                codec = None;
                waiting_keyframe = true;
                waited = 0;
            }
        }
    }
}

/// Decodes remote tracks and exposes the results to the UI.
pub struct RemoteMedia {
    inner: Arc<RemoteMediaInner>,
}

struct RemoteMediaInner {
    tracks: RwLock<HashMap<String, TrackHandle>>,
    frames: RwLock<HashMap<String, SharedFrame>>,
    counter: std::sync::atomic::AtomicU64,
}

type SharedFrame = Arc<Mutex<Option<Vec<u8>>>>;

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
            inner: Arc::new(RemoteMediaInner {
                tracks: RwLock::new(HashMap::new()),
                frames: RwLock::new(HashMap::new()),
                counter: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// Attach a remote track and start feeding it with the track's RTP packets.
    pub async fn attach(&self, peer_id: &str, track: RemoteTrack) -> Result<(), MediaError> {
        let kind = track.kind().await;
        let codec = track.codec().await.unwrap_or(Codec::Opus);
        tracing::info!("remote track from {peer_id}: kind={kind:?} codec={codec:?}");
        let key = format!(
            "{peer_id}#{}",
            self.inner.counter.fetch_add(1, Ordering::Relaxed)
        );

        let decoder = Decoder::new(kind, codec)?;
        if let Some(slot) = decoder.frames.clone() {
            self.inner
                .frames
                .write()
                .unwrap()
                .insert(peer_id.to_string(), slot);
        }

        let running = decoder.running.clone();
        let tx = decoder.tx.clone();
        let mut stop = decoder.stop.subscribe();
        let inner = Arc::downgrade(&self.inner);
        let cleanup_key = key.clone();
        tokio::spawn(async move {
            loop {
                let packet = tokio::select! {
                    _ = stop.changed() => break,
                    packet = track.next_packet() => packet,
                };
                let Some(packet) = packet else { break };
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if tx.send(packet.to_bytes()).await.is_err() {
                    break;
                }
            }
            if let Some(inner) = inner.upgrade() {
                stop_track(&inner, &cleanup_key);
            }
        });

        self.inner.tracks.write().unwrap().insert(
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
            .inner
            .tracks
            .read()
            .unwrap()
            .iter()
            .filter(|(_, h)| h.peer_id == peer_id)
            .map(|(k, _)| k.clone())
            .collect();

        for key in keys {
            stop_track(&self.inner, &key);
        }
    }

    /// The latest decoded video frame (JPEG) for a peer, if any.
    pub fn latest_frame(&self, peer_id: &str) -> Option<Vec<u8>> {
        let frames = self.inner.frames.read().unwrap();
        let slot = frames.get(peer_id)?;
        let guard = slot.lock().unwrap();
        guard.clone()
    }

    /// Set the playback volume (0.0..=2.0) for a peer's audio.
    pub fn set_peer_volume(&self, peer_id: &str, volume: f64) {
        for handle in self.inner.tracks.read().unwrap().values() {
            if handle.peer_id != peer_id || handle.kind != RemoteTrackKind::Audio {
                continue;
            }
            handle.decoder.set_volume(volume);
        }
    }

    /// Number of attached tracks.
    pub fn track_count(&self) -> usize {
        self.inner.tracks.read().unwrap().len()
    }
}

fn stop_track(inner: &RemoteMediaInner, key: &str) {
    let handle = inner.tracks.write().unwrap().remove(key);
    let Some(handle) = handle else { return };
    let peer_id = handle.peer_id.clone();
    let removed_frame = handle.decoder.frames.clone();
    handle.decoder.stop();
    let has_video = inner
        .tracks
        .read()
        .unwrap()
        .values()
        .any(|track| track.peer_id == peer_id && track.kind == RemoteTrackKind::Video);
    if !has_video {
        let mut frames = inner.frames.write().unwrap();
        let is_removed_track_frame = frames
            .get(&peer_id)
            .zip(removed_frame.as_ref())
            .is_some_and(|(current, removed)| Arc::ptr_eq(current, removed));
        if is_removed_track_frame {
            frames.remove(&peer_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nals(sample: &[u8]) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 4 <= sample.len() {
            if sample[i..i + 4] == [0, 0, 0, 1] {
                let start = i + 4;
                let mut end = sample.len();
                let mut j = start;
                while j + 4 <= sample.len() {
                    if sample[j..j + 4] == [0, 0, 0, 1] {
                        end = j;
                        break;
                    }
                    j += 1;
                }
                out.push(&sample[start..end]);
                i = end;
            } else {
                i += 1;
            }
        }
        out
    }

    #[test]
    fn decodes_h264_sample_into_jpeg() {
        let sample = include_bytes!("../fixtures/sample.h264");
        let decoder = Decoder::new(RemoteTrackKind::Video, Codec::H264).unwrap();
        for nal in nals(sample) {
            let mut rtp = vec![0u8; 12];
            rtp[0] = 0x80;
            rtp[1] = 96;
            rtp.extend_from_slice(nal);
            decoder.push_rtp(rtp);
        }
        let mut frame = None;
        for _ in 0..100 {
            if let Some(f) = decoder.latest_frame() {
                frame = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let frame = frame.expect("a decoded JPEG frame");
        assert_eq!(&frame[0..2], &[0xff, 0xd8], "not a JPEG");
        decoder.stop();
    }
}
