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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::watch;

use sharescreen_audio::Output;
use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_native_media::decode::{
    rtp_payload, vp8_payload, vp9_payload, JpegEncoder, OpusDecoder, RtpDepacketizer, RtpKind,
    RtpReorderBuffer, VideoDecoder,
};
use sharescreen_native_media::preview::Demand;
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
    keyframe_tx: mpsc::Sender<Vec<Vec<u8>>>,
    video_demand: Arc<AtomicU64>,
    stop: watch::Sender<bool>,
    video_recovery: Arc<AtomicBool>,
    /// True until a picture decodes after startup or stream recovery.
    needs_keyframe: Arc<AtomicBool>,
}

/// Caps queued remote RTP at roughly 256 packets per track (about 5 seconds
/// for 20 ms audio, less for video). `attach` awaits capacity, keeping packet
/// memory bounded while allowing the WebRTC reader to apply backpressure.
const REMOTE_RTP_QUEUE: usize = 256;
const MAX_KEYFRAME_PRIORITY_BYTES: usize = 2 * 1024 * 1024;
const KEYFRAME_PRIORITY_QUEUE: usize = 1;
// Seven remote participants can publish full and thumbnail video on Linux.
const MAX_VIDEO_DECODERS: usize = 14;
const VIDEO_DEMAND_TIMEOUT: Duration = Duration::from_millis(500);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
/// Widest frame decoded for a remote share. The UI shows a focused share at
/// (nearly) full window width, so 1280 was visibly soft on large monitors.
const MAX_REMOTE_VIDEO_WIDTH: u32 = 1920;
/// Self-view loopback: the peer picture is small, so keep the extra pixels off
/// the JPEG path.
const MAX_LOOPBACK_PREVIEW_WIDTH: u32 = 1280;
static VIDEO_RTP_QUEUE_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OPUS_PACKET_GAPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OPUS_FEC_RECOVERIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OPUS_PLC_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OPUS_SILENCE_FALLBACK_FRAMES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaMetrics {
    pub video_rtp_queue_drops: u64,
    pub opus_packet_gaps: u64,
    pub opus_fec_recoveries: u64,
    pub opus_plc_frames: u64,
    pub opus_silence_fallback_frames: u64,
}

pub fn media_metrics() -> MediaMetrics {
    MediaMetrics {
        video_rtp_queue_drops: VIDEO_RTP_QUEUE_DROPS.load(Ordering::Relaxed),
        opus_packet_gaps: OPUS_PACKET_GAPS.load(Ordering::Relaxed),
        opus_fec_recoveries: OPUS_FEC_RECOVERIES.load(Ordering::Relaxed),
        opus_plc_frames: OPUS_PLC_FRAMES.load(Ordering::Relaxed),
        opus_silence_fallback_frames: OPUS_SILENCE_FALLBACK_FRAMES.load(Ordering::Relaxed),
    }
}

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
        let (keyframe_tx, mut keyframe_rx) = mpsc::channel(KEYFRAME_PRIORITY_QUEUE);
        let video_demand = Arc::new(AtomicU64::new(0));
        let (stop, _) = watch::channel(false);
        let video_recovery = Arc::new(AtomicBool::new(false));
        let needs_keyframe = Arc::new(AtomicBool::new(kind == RemoteTrackKind::Video));

        let flag = running.clone();
        let volume_slot = volume.clone();
        let frames_slot = frames.clone();
        let thread_flag = flag.clone();
        let thread_stop = stop.clone();
        let thread_recovery = video_recovery.clone();
        let thread_video_demand = video_demand.clone();
        let thread_needs_keyframe = needs_keyframe.clone();
        let thread = std::thread::Builder::new()
            .name("sharescreen-media".into())
            .spawn(move || {
                run(
                    kind,
                    codec,
                    rtp_kind,
                    &mut rx,
                    &mut keyframe_rx,
                    flag,
                    volume_slot,
                    frames_slot,
                    thread_recovery,
                    thread_video_demand,
                    thread_needs_keyframe,
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
            keyframe_tx,
            video_demand,
            stop,
            video_recovery,
            needs_keyframe,
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
        self.video_demand
            .store(monotonic_micros(), Ordering::Relaxed);
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

#[allow(clippy::too_many_arguments)]
fn run(
    kind: RemoteTrackKind,
    codec: Codec,
    rtp_kind: RtpKind,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    keyframe_rx: &mut mpsc::Receiver<Vec<Vec<u8>>>,
    running: Arc<AtomicBool>,
    volume: Arc<Mutex<f64>>,
    frames: Option<Arc<Mutex<Option<Vec<u8>>>>>,
    video_recovery: Arc<AtomicBool>,
    video_demand: Arc<AtomicU64>,
    needs_keyframe: Arc<AtomicBool>,
) {
    let mut reorder = RtpReorderBuffer::new();
    let mut depacketizer = RtpDepacketizer::new(rtp_kind);
    // Do not feed mid-GOP slices into a fresh native decoder. The RTP reader
    // asks for an IDR until the first picture arrives.
    let mut video: Option<VideoDecoder> = None;
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
    let mut consecutive_decode_errors = 0u32;
    let mut waiting_for_keyframe = kind == RemoteTrackKind::Video;
    // Set after a decoder flush: the next access unit may need the cached
    // SPS/PPS replayed if it does not carry its own.
    let mut replay_parameter_sets = false;
    // A sender may emit SPS/PPS once before the IDR that follows. Keep the
    // latest ones so an IDR without repeated parameter sets can still start a
    // fresh decoder. Bounded because senders may repeat them per keyframe.
    let mut parameter_sets = Vec::new();
    // `output` is passed in instead of captured so the receive timeout can
    // also pump the playback buffer while no packets arrive.
    let mut process =
        |frame: &[u8], output: &mut Option<Output>, missing_audio_packets: u16| match kind {
            RemoteTrackKind::Video => {
                // Keep the latest parameter sets; an encoder may emit them only
                // once, before later IDRs. Bounded because senders may repeat
                // them per keyframe.
                if codec == Codec::H264 {
                    let sets = extract_h264_parameter_sets(frame);
                    if !sets.is_empty() && sets.len() <= MAX_KEYFRAME_PRIORITY_BYTES {
                        parameter_sets = sets;
                    }
                }
                // A recovery keeps the same decoder: flushing it drops broken
                // reference frames while the parsed SPS/PPS survive, so the
                // next IDR decodes without a fresh context.
                if (waiting_for_keyframe || video_recovery.load(Ordering::Acquire))
                    && !contains_video_keyframe(codec, frame)
                {
                    return;
                }
                if video_recovery.swap(false, Ordering::AcqRel) {
                    if let Some(decoder) = video.as_mut() {
                        decoder.flush();
                        // A flush may drop the parsed parameter sets; replay
                        // them on an IDR that does not carry its own.
                        replay_parameter_sets = true;
                    }
                }
                let mut fresh = false;
                if video.is_none() {
                    match VideoDecoder::new(codec_name(codec)) {
                        Ok(decoder) => {
                            video = Some(decoder);
                            waiting_for_keyframe = false;
                            consecutive_decode_errors = 0;
                            fresh = true;
                            tracing::info!("created video decoder");
                        }
                        Err(error) => {
                            tracing::error!("video decoder recreation failed: {error}");
                            return;
                        }
                    }
                } else {
                    waiting_for_keyframe = false;
                }
                let Some(jpeg) = jpeg.as_mut() else {
                    return;
                };
                let Some(decoder) = video.as_mut() else {
                    return;
                };
                let mut joined;
                let frame = match parameter_sets.is_empty() {
                    false
                        if codec == Codec::H264
                            && (fresh || replay_parameter_sets)
                            && !au_has_h264_parameter_sets(frame) =>
                    {
                        joined = Vec::with_capacity(parameter_sets.len() + frame.len());
                        joined.extend_from_slice(&parameter_sets);
                        joined.extend_from_slice(frame);
                        &joined[..]
                    }
                    _ => frame,
                };
                replay_parameter_sets = false;
                let result = decoder.decode_scaled(frame, MAX_REMOTE_VIDEO_WIDTH);
                match result {
                    Ok(rgb_frames) => {
                        if rgb_frames.is_empty() {
                            return;
                        }
                        consecutive_decode_errors = 0;
                        // A recovery armed while this packet was in flight must
                        // keep asking for an IDR.
                        if !video_recovery.load(Ordering::Acquire) {
                            needs_keyframe.store(false, Ordering::Release);
                        }
                        let (width, height) = video
                            .as_ref()
                            .map(|decoder| (decoder.width(), decoder.height()))
                            .unwrap_or_default();
                        for rgb in rgb_frames {
                            match jpeg.encode(&rgb, width, height) {
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
                    Err(error) => {
                        consecutive_decode_errors += 1;
                        needs_keyframe.store(true, Ordering::Release);
                        tracing::warn!(consecutive_decode_errors, "video decode failed: {error}");
                        if consecutive_decode_errors >= 3 {
                            // Keep the decoder: its SPS/PPS stay valid, only
                            // the broken reference frames need to go. Replay
                            // the parameter sets with the next IDR in case the
                            // flush also discards them.
                            if let Some(decoder) = video.as_mut() {
                                decoder.flush();
                                replay_parameter_sets = true;
                            }
                            waiting_for_keyframe = true;
                            consecutive_decode_errors = 0;
                        }
                    }
                }
            }
            RemoteTrackKind::Audio => {
                let (Some(audio), Some(output)) = (audio.as_mut(), output.as_mut()) else {
                    return;
                };
                if missing_audio_packets > 0 {
                    let missing = missing_audio_packets.min(5);
                    OPUS_PACKET_GAPS.fetch_add(missing_audio_packets as u64, Ordering::Relaxed);
                    let fec_available = audio.supports_loss_recovery();
                    let plc_count = if fec_available {
                        missing.saturating_sub(1)
                    } else {
                        missing
                    };
                    for _ in 0..plc_count {
                        match audio.conceal(960) {
                            Ok(pcm) => {
                                if fec_available {
                                    OPUS_PLC_FRAMES.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    OPUS_SILENCE_FALLBACK_FRAMES.fetch_add(1, Ordering::Relaxed);
                                }
                                output.push(&pcm);
                            }
                            Err(error) => tracing::warn!(%error, "Opus PLC failed"),
                        }
                    }
                    if fec_available {
                        match audio.decode_fec(frame, 960) {
                            Ok(pcm) => {
                                OPUS_FEC_RECOVERIES.fetch_add(1, Ordering::Relaxed);
                                output.push(&pcm);
                            }
                            Err(error) => tracing::warn!(%error, "Opus FEC recovery failed"),
                        }
                    }
                }
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

    let mut last_audio_sequence = None;
    while running.load(Ordering::SeqCst) {
        if kind == RemoteTrackKind::Video
            && !video_demand_recent(video_demand.load(Ordering::Relaxed), monotonic_micros())
        {
            let mut discarded = 0u64;
            while rx.try_recv().is_ok() {
                discarded += 1;
            }
            while keyframe_rx.try_recv().is_ok() {
                discarded += 1;
            }
            VIDEO_RTP_QUEUE_DROPS.fetch_add(discarded, Ordering::Relaxed);
            // Only re-arm on the transition into idle: doing this every 20 ms
            // would spam keyframe requests and throw away the parsed
            // parameter sets.
            if !video_recovery.swap(true, Ordering::AcqRel) {
                depacketizer = RtpDepacketizer::new(rtp_kind);
                reorder.reset();
                needs_keyframe.store(true, Ordering::Release);
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let priority_keyframe = match keyframe_rx.try_recv() {
            Ok(packets) => {
                let mut discarded = 0u64;
                while rx.try_recv().is_ok() {
                    discarded += 1;
                }
                VIDEO_RTP_QUEUE_DROPS.fetch_add(discarded, Ordering::Relaxed);
                depacketizer = RtpDepacketizer::new(rtp_kind);
                reorder.reset();
                Some(packets)
            }
            Err(mpsc::error::TryRecvError::Disconnected) => None,
            Err(mpsc::error::TryRecvError::Empty) => None,
        };
        if let Some(packets) = priority_keyframe {
            for packet in packets {
                for frame in depacketizer.push(&packet) {
                    process(&frame, &mut output, 0);
                }
            }
            continue;
        }
        match rx.try_recv() {
            Ok(packet) => {
                if kind == RemoteTrackKind::Video {
                    let ready = reorder.push(packet, Instant::now());
                    for packet in ready {
                        for frame in depacketizer.push(&packet) {
                            process(&frame, &mut output, 0);
                        }
                    }
                } else {
                    let missing = if packet.len() >= 4 {
                        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
                        let lost = last_audio_sequence
                            .map(|previous| rtp_sequence_gap(previous, sequence))
                            .unwrap_or(0);
                        last_audio_sequence = Some(sequence);
                        lost
                    } else {
                        0
                    };
                    for (index, frame) in depacketizer.push(&packet).into_iter().enumerate() {
                        process(&frame, &mut output, if index == 0 { missing } else { 0 });
                    }
                }
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {
                if kind == RemoteTrackKind::Video {
                    let now = Instant::now();
                    let mut ready = Vec::new();
                    reorder.release(now, &mut ready);
                    for packet in ready {
                        for frame in depacketizer.push(&packet) {
                            process(&frame, &mut output, 0);
                        }
                    }
                    // Leave a partially assembled access unit buffered: its
                    // remaining packets (or the marker that closes it) may
                    // still be in flight. It is discarded on the next
                    // sequence gap or when the stream idles.
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

fn monotonic_micros() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn video_demand_recent(last_requested: u64, now: u64) -> bool {
    now.saturating_sub(last_requested) <= VIDEO_DEMAND_TIMEOUT.as_micros() as u64
}

fn rtp_sequence_gap(previous: u16, current: u16) -> u16 {
    let distance = current.wrapping_sub(previous);
    if (2..=32).contains(&distance) {
        distance - 1
    } else {
        // Ignore duplicates, reordering and large discontinuities. Those need
        // a stream reset rather than speculative PLC on an arbitrary duration.
        0
    }
}

fn h264_rtp_idr_start(packet: &[u8]) -> bool {
    let Some(payload) = rtp_payload(packet) else {
        return false;
    };
    let Some(&indicator) = payload.first() else {
        return false;
    };
    match indicator & 0x1f {
        5 => true,
        24 => {
            let mut cursor = 1;
            while cursor + 2 <= payload.len() {
                let nal_len =
                    usize::from(u16::from_be_bytes([payload[cursor], payload[cursor + 1]]));
                cursor += 2;
                let Some(nal) = payload.get(cursor..cursor + nal_len) else {
                    return false;
                };
                if nal.first().is_some_and(|header| header & 0x1f == 5) {
                    return true;
                }
                cursor += nal_len;
            }
            false
        }
        28 => payload.len() >= 2 && payload[1] & 0x80 != 0 && payload[1] & 0x1f == 5,
        _ => false,
    }
}

fn rtp_marker(packet: &[u8]) -> bool {
    packet.get(1).is_some_and(|header| header & 0x80 != 0)
}

fn vp8_rtp_keyframe_start(packet: &[u8]) -> bool {
    let Some(payload) = rtp_payload(packet) else {
        return false;
    };
    let Some(&descriptor) = payload.first() else {
        return false;
    };
    if descriptor & 0x10 == 0 || descriptor & 0x07 != 0 {
        return false;
    }
    vp8_payload(payload).is_some_and(|frame| frame.first().is_some_and(|tag| tag & 1 == 0))
}

fn vp9_rtp_keyframe_start(packet: &[u8]) -> bool {
    let Some(payload) = rtp_payload(packet) else {
        return false;
    };
    let Some(&descriptor) = payload.first() else {
        return false;
    };
    if descriptor & 0x08 == 0 || descriptor & 0x40 != 0 {
        return false;
    }
    vp9_payload(payload).is_some_and(vp9_frame_is_keyframe)
}

fn packet_starts_keyframe(codec: Codec, packet: &[u8]) -> bool {
    match codec {
        Codec::H264 => h264_rtp_idr_start(packet),
        Codec::Vp8 => vp8_rtp_keyframe_start(packet),
        Codec::Vp9 => vp9_rtp_keyframe_start(packet),
        _ => false,
    }
}

fn contains_video_keyframe(codec: Codec, access_unit: &[u8]) -> bool {
    match codec {
        Codec::H264 => contains_h264_idr(access_unit),
        Codec::Vp8 => access_unit.first().is_some_and(|tag| tag & 1 == 0),
        Codec::Vp9 => vp9_frame_is_keyframe(access_unit),
        _ => false,
    }
}

fn vp9_frame_is_keyframe(frame: &[u8]) -> bool {
    let Some(&header) = frame.first() else {
        return false;
    };
    if header & 0x03 != 0x02 {
        return false;
    }
    let profile = (header >> 2) & 0x03;
    let mut next_bit = 4;
    if profile == 3 {
        next_bit += 1;
    }
    let show_existing_frame = (header >> next_bit) & 1;
    next_bit += 1;
    show_existing_frame == 0 && (header >> next_bit) & 1 == 0
}

fn h264_nal_types(access_unit: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i < access_unit.len() {
        let start = if access_unit[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if access_unit[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if let Some(header) = access_unit.get(i + start) {
            types.push(header & 0x1f);
        }
        i += start;
    }
    types
}

/// True when an access unit carries an SPS or PPS NAL.
fn au_has_h264_parameter_sets(access_unit: &[u8]) -> bool {
    h264_nal_types(access_unit)
        .iter()
        .any(|kind| *kind == 7 || *kind == 8)
}

/// Extract the SPS/PPS NAL units from an access unit, in Annex-B form.
///
/// Encoders may bundle them with a slice, so this looks at every NAL instead
/// of requiring a parameter-set-only access unit.
fn extract_h264_parameter_sets(access_unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < access_unit.len() {
        let start = if access_unit[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if access_unit[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let body = i + start;
        if body >= access_unit.len() {
            break;
        }
        // The next start code ends this NAL. Emulation prevention guarantees
        // `00 00 01` cannot occur inside the payload. A leading zero before it
        // belongs to a four-byte start code, not to the NAL.
        let mut end = body + 1;
        while end + 3 <= access_unit.len() && !access_unit[end..].starts_with(&[0, 0, 1]) {
            end += 1;
        }
        if end + 3 > access_unit.len() {
            end = access_unit.len();
        } else if end > body + 1 && access_unit[end - 1] == 0 {
            end -= 1;
        }
        let kind = access_unit[body] & 0x1f;
        if (kind == 7 || kind == 8)
            && out.len().saturating_add(end - body + 4) <= MAX_KEYFRAME_PRIORITY_BYTES
        {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&access_unit[body..end]);
        }
        i = end.max(body + 1);
    }
    out
}

fn contains_h264_idr(access_unit: &[u8]) -> bool {
    let mut i = 0;
    while i < access_unit.len() {
        let start_code = if i + 4 < access_unit.len() && access_unit[i..].starts_with(&[0, 0, 0, 1])
        {
            Some(4)
        } else if i + 3 < access_unit.len() && access_unit[i..].starts_with(&[0, 0, 1]) {
            Some(3)
        } else {
            None
        };
        if let Some(len) = start_code {
            if access_unit[i + len] & 0x1f == 5 {
                return true;
            }
            i += len;
        } else {
            i += 1;
        }
    }
    false
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
    let mut last_jpeg = Instant::now() - Duration::from_millis(66);

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
        match video.decode_scaled(&sample.data, MAX_LOOPBACK_PREVIEW_WIDTH) {
            Ok(pixels) => {
                // The decoder must run in order, but the JPEG stage only needs
                // to run as fast as the UI consumes frames. Skipping a frame
                // nobody read leaves the previous JPEG in place.
                if !consumed.swap(false, Ordering::AcqRel) {
                    continue;
                }
                if last_jpeg.elapsed() < Duration::from_millis(66) {
                    continue;
                }
                last_jpeg = Instant::now();
                for rgb in pixels {
                    let (width, height) = (video.width(), video.height());
                    match jpeg.encode(&rgb, width, height) {
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
    frame_requests: RwLock<HashMap<String, Arc<AtomicU64>>>,
    keyframe_tracks: RwLock<HashMap<String, RemoteTrack>>,
    video_slots: Arc<tokio::sync::Semaphore>,
    counter: std::sync::atomic::AtomicU64,
}

type SharedFrame = Arc<Mutex<Option<Vec<u8>>>>;

struct TrackHandle {
    peer_id: String,
    kind: RemoteTrackKind,
    frame_key: String,
    _video_permit: Option<tokio::sync::OwnedSemaphorePermit>,
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
                frame_requests: RwLock::new(HashMap::new()),
                keyframe_tracks: RwLock::new(HashMap::new()),
                video_slots: Arc::new(tokio::sync::Semaphore::new(MAX_VIDEO_DECODERS)),
                counter: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// Attach a remote track and start feeding it with the track's RTP packets.
    pub async fn attach(&self, peer_id: &str, track: RemoteTrack) -> Result<(), MediaError> {
        self.attach_named(peer_id, peer_id, track).await
    }

    /// Attach a track while keeping its publication id as the frame key.
    pub async fn attach_named(
        &self,
        peer_id: &str,
        frame_key: &str,
        track: RemoteTrack,
    ) -> Result<(), MediaError> {
        let kind = track.kind().await;
        let codec = track.codec().await.unwrap_or(Codec::Opus);
        tracing::info!("remote track from {peer_id}: kind={kind:?} codec={codec:?}");
        let video_permit = if kind == RemoteTrackKind::Video {
            Some(
                self.inner
                    .video_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| {
                        MediaError::Pipeline(format!(
                            "remote video decode budget exhausted ({MAX_VIDEO_DECODERS} tracks)"
                        ))
                    })?,
            )
        } else {
            None
        };
        let key = format!(
            "{peer_id}#{}",
            self.inner.counter.fetch_add(1, Ordering::Relaxed)
        );

        let decoder = Decoder::new(kind, codec)?;
        let frame_request = decoder.video_demand.clone();
        if let Some(slot) = decoder.frames.clone() {
            self.inner
                .frames
                .write()
                .unwrap()
                .insert(frame_key.to_string(), slot);
            self.inner
                .frame_requests
                .write()
                .unwrap()
                .insert(frame_key.to_string(), frame_request.clone());
            self.inner
                .keyframe_tracks
                .write()
                .unwrap()
                .insert(frame_key.to_string(), track.clone());
        }

        let running = decoder.running.clone();
        let tx = decoder.tx.clone();
        let keyframe_tx = decoder.keyframe_tx.clone();
        let recovery = decoder.video_recovery.clone();
        let needs_keyframe = decoder.needs_keyframe.clone();
        let thread_frame_request = frame_request.clone();
        let mut stop = decoder.stop.subscribe();
        let inner = Arc::downgrade(&self.inner);
        let cleanup_key = key.clone();
        tokio::spawn(async move {
            let mut keyframe_packets: Option<Vec<Vec<u8>>> = None;
            let mut keyframe_bytes = 0usize;
            let mut media_ssrc = None;
            let mut retry = tokio::time::interval_at(
                tokio::time::Instant::now() + KEYFRAME_REQUEST_INTERVAL,
                KEYFRAME_REQUEST_INTERVAL,
            );
            retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let packet = tokio::select! {
                    _ = stop.changed() => break,
                    _ = retry.tick(), if kind == RemoteTrackKind::Video => {
                        let watched = video_demand_recent(
                            thread_frame_request.load(Ordering::Relaxed),
                            monotonic_micros(),
                        );
                        if watched && needs_keyframe.load(Ordering::Acquire) {
                            if let Err(error) = track.request_keyframe_for_ssrc(media_ssrc).await {
                                tracing::debug!(%error, "could not request video keyframe");
                            }
                        }
                        continue;
                    }
                    packet = track.next_packet() => packet,
                };
                let Some(packet) = packet else { break };
                media_ssrc = Some(packet.ssrc);
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if kind == RemoteTrackKind::Video
                    && !video_demand_recent(
                        thread_frame_request.load(Ordering::Relaxed),
                        monotonic_micros(),
                    )
                {
                    keyframe_packets = None;
                    keyframe_bytes = 0;
                    recovery.store(true, Ordering::Release);
                    needs_keyframe.store(true, Ordering::Release);
                    continue;
                }
                let bytes = packet.to_bytes();
                if kind == RemoteTrackKind::Video {
                    if let Some(packets) = keyframe_packets.as_mut() {
                        keyframe_bytes = keyframe_bytes.saturating_add(bytes.len());
                        packets.push(bytes);
                        if packets.len() > REMOTE_RTP_QUEUE
                            || keyframe_bytes > MAX_KEYFRAME_PRIORITY_BYTES
                        {
                            VIDEO_RTP_QUEUE_DROPS
                                .fetch_add(packets.len() as u64, Ordering::Relaxed);
                            keyframe_packets = None;
                            keyframe_bytes = 0;
                        } else if packets.last().is_some_and(|packet| rtp_marker(packet)) {
                            let complete = keyframe_packets.take().unwrap();
                            keyframe_bytes = 0;
                            if let Err(error) = keyframe_tx.try_send(complete) {
                                tracing::debug!(
                                    "dropping prioritized keyframe because its queue is full: {error}"
                                );
                            }
                        }
                        continue;
                    }
                    if let Err(error) = tx.try_send(bytes) {
                        match error {
                            mpsc::error::TrySendError::Closed(_) => break,
                            mpsc::error::TrySendError::Full(bytes) => {
                                let drops =
                                    VIDEO_RTP_QUEUE_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
                                recovery.store(true, Ordering::Release);
                                needs_keyframe.store(true, Ordering::Release);
                                if packet_starts_keyframe(codec, &bytes) {
                                    if rtp_marker(&bytes) {
                                        if let Err(error) = keyframe_tx.try_send(vec![bytes]) {
                                            tracing::debug!(
                                                "dropping prioritized keyframe because its queue is full: {error}"
                                            );
                                        }
                                    } else {
                                        keyframe_bytes = bytes.len();
                                        keyframe_packets = Some(vec![bytes]);
                                    }
                                }
                                if drops == 1 || drops.is_multiple_of(100) {
                                    tracing::warn!(
                                        drops,
                                        "dropping video RTP packet: decoder queue full"
                                    );
                                }
                            }
                        }
                    }
                } else if tx.send(bytes).await.is_err() {
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
                frame_key: frame_key.to_string(),
                _video_permit: video_permit,
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

    /// Stop the decoder for one SFU publication and release its decode slot.
    pub fn detach_frame_key(&self, frame_key: &str) {
        let keys: Vec<String> = self
            .inner
            .tracks
            .read()
            .unwrap()
            .iter()
            .filter(|(_, track)| track.frame_key == frame_key)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            stop_track(&self.inner, &key);
        }
    }

    /// The latest decoded video frame (JPEG) for a track key, if any.
    pub fn latest_frame(&self, frame_key: &str) -> Option<Vec<u8>> {
        let frames = self.inner.frames.read().unwrap();
        let slot = frames.get(frame_key)?;
        if let Some(request) = self.inner.frame_requests.read().unwrap().get(frame_key) {
            let now = monotonic_micros();
            let was_inactive = !video_demand_recent(request.load(Ordering::Relaxed), now);
            request.store(now, Ordering::Relaxed);
            if was_inactive {
                let track = self
                    .inner
                    .keyframe_tracks
                    .read()
                    .unwrap()
                    .get(frame_key)
                    .cloned();
                if let Some(track) = track {
                    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                        runtime.spawn(async move {
                            if let Err(error) = track.request_keyframe().await {
                                tracing::debug!(%error, "could not request video keyframe after decode resumed");
                            }
                        });
                    }
                }
            }
        }
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
    let frame_key = handle.frame_key.clone();
    let removed_frame = handle.decoder.frames.clone();
    handle.decoder.stop();
    let mut frames = inner.frames.write().unwrap();
    let is_removed_track_frame = frames
        .get(&frame_key)
        .zip(removed_frame.as_ref())
        .is_some_and(|(current, removed)| Arc::ptr_eq(current, removed));
    if is_removed_track_frame {
        frames.remove(&frame_key);
        inner.frame_requests.write().unwrap().remove(&frame_key);
        inner.keyframe_tracks.write().unwrap().remove(&frame_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_sequence_gaps_handle_wrap_and_ignore_reordering() {
        assert_eq!(rtp_sequence_gap(10, 11), 0);
        assert_eq!(rtp_sequence_gap(10, 13), 2);
        assert_eq!(rtp_sequence_gap(u16::MAX, 1), 1);
        assert_eq!(rtp_sequence_gap(10, 9), 0);
        assert_eq!(rtp_sequence_gap(1, 100), 0);
    }

    #[test]
    fn h264_recovery_waits_for_an_idr_access_unit() {
        assert!(contains_h264_idr(&[0, 0, 0, 1, 0x65, 0x88]));
        assert!(contains_h264_idr(&[0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65]));
        assert!(!contains_h264_idr(&[0, 0, 0, 1, 0x41, 0x88]));
    }

    #[test]
    fn recognizes_keyframe_starts_for_all_negotiated_video_codecs() {
        let packet = |payload: &[u8]| {
            let mut rtp = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
            rtp.extend_from_slice(payload);
            rtp
        };
        assert!(packet_starts_keyframe(Codec::H264, &packet(&[0x65, 0x88])));
        assert!(packet_starts_keyframe(
            Codec::H264,
            &packet(&[0x7c, 0x85, 0x88])
        ));
        assert!(!packet_starts_keyframe(
            Codec::H264,
            &packet(&[0x7c, 0x45, 0x88])
        ));
        assert!(!packet_starts_keyframe(Codec::H264, &packet(&[0x41, 0x88])));
        assert!(packet_starts_keyframe(Codec::Vp8, &packet(&[0x10, 0x00])));
        assert!(!packet_starts_keyframe(Codec::Vp8, &packet(&[0x10, 0x01])));
        assert!(packet_starts_keyframe(Codec::Vp9, &packet(&[0x08, 0x02])));
        assert!(!packet_starts_keyframe(Codec::Vp9, &packet(&[0x08, 0x22])));
        assert!(!packet_starts_keyframe(Codec::Vp9, &packet(&[0x48, 0x02])));
    }

    #[test]
    fn recovery_keyframe_detection_matches_h264_vp8_and_vp9_frames() {
        assert!(contains_video_keyframe(Codec::H264, &[0, 0, 0, 1, 0x65]));
        assert!(!contains_video_keyframe(Codec::H264, &[0, 0, 0, 1, 0x41]));
        assert!(contains_video_keyframe(
            Codec::Vp8,
            &[0x00, 0x9d, 0x01, 0x2a]
        ));
        assert!(!contains_video_keyframe(Codec::Vp8, &[0x01, 0x00]));
        assert!(contains_video_keyframe(Codec::Vp9, &[0x02, 0x00]));
        assert!(!contains_video_keyframe(Codec::Vp9, &[0x22, 0x00]));
        assert!(!contains_video_keyframe(Codec::Vp9, &[0x12, 0x00]));
    }

    #[test]
    fn inactive_video_tracks_stop_decoding_after_demand_expires() {
        assert!(video_demand_recent(1_000_000, 1_499_999));
        assert!(!video_demand_recent(1_000_000, 1_500_001));
        assert!(video_demand_recent(2_000_000, 1_000_000));
    }

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

    fn push_nal_marked(
        decoder: &Decoder,
        sequence: &mut u16,
        timestamp: u32,
        nal: &[u8],
        marker: bool,
    ) {
        let mut rtp = vec![0u8; 12];
        rtp[0] = 0x80;
        rtp[1] = 96 | if marker { 0x80 } else { 0 };
        rtp[2..4].copy_from_slice(&sequence.to_be_bytes());
        rtp[4..8].copy_from_slice(&timestamp.to_be_bytes());
        rtp.extend_from_slice(nal);
        decoder.push_rtp(rtp);
        *sequence = sequence.wrapping_add(1);
    }

    fn push_nal(decoder: &Decoder, sequence: &mut u16, timestamp: u32, nal: &[u8]) {
        push_nal_marked(decoder, sequence, timestamp, nal, false);
    }

    /// Push every NAL of one access unit, marking the last RTP packet the way
    /// the real packetizer does.
    fn push_sample(decoder: &Decoder, sequence: &mut u16, timestamp: u32) {
        let sample = include_bytes!("../fixtures/sample.h264");
        let nals = nals(sample);
        let Some((last, rest)) = nals.split_last() else {
            return;
        };
        for nal in rest {
            push_nal(decoder, sequence, timestamp, nal);
        }
        push_nal_marked(decoder, sequence, timestamp, last, true);
    }

    fn wait_for_frame(decoder: &Decoder) -> Vec<u8> {
        for _ in 0..100 {
            if let Some(frame) = decoder.latest_frame() {
                return frame;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("a decoded JPEG frame");
    }

    #[test]
    fn decodes_h264_sample_into_jpeg() {
        let decoder = Decoder::new(RemoteTrackKind::Video, Codec::H264).unwrap();
        decoder.latest_frame();
        push_sample(&decoder, &mut 0, 1);
        let frame = wait_for_frame(&decoder);
        assert_eq!(&frame[0..2], &[0xff, 0xd8], "not a JPEG");
        assert!(!decoder.needs_keyframe.load(Ordering::Acquire));
        decoder.stop();
    }

    #[test]
    fn android_style_mid_gop_join_recovers_after_a_keyframe() {
        let decoder = Decoder::new(RemoteTrackKind::Video, Codec::H264).unwrap();
        decoder.latest_frame();
        let mut sequence = 0;
        assert!(decoder.needs_keyframe.load(Ordering::Acquire));
        push_nal_marked(&decoder, &mut sequence, 1, &[0x41, 0x88, 0x84], true);
        std::thread::sleep(Duration::from_millis(30));
        assert!(decoder.latest_frame().is_none());
        assert!(decoder.needs_keyframe.load(Ordering::Acquire));

        push_sample(&decoder, &mut sequence, 2);
        assert_eq!(&wait_for_frame(&decoder)[0..2], &[0xff, 0xd8]);
        assert!(!decoder.needs_keyframe.load(Ordering::Acquire));

        // Invalid IDRs force native recovery; empty decode results between
        // errors must not cancel the recovery sequence.
        for step in 0..3 {
            let timestamp = 3 + step * 2;
            push_nal_marked(&decoder, &mut sequence, timestamp, &[0x65, 0xff], true);
            push_nal_marked(&decoder, &mut sequence, timestamp + 1, &[0x41, 0x88], true);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(decoder.needs_keyframe.load(Ordering::Acquire));
        push_sample(&decoder, &mut sequence, 20);
        for _ in 0..100 {
            if !decoder.needs_keyframe.load(Ordering::Acquire) {
                break;
            }
            decoder.latest_frame();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!decoder.needs_keyframe.load(Ordering::Acquire));
        decoder.stop();
    }

    #[test]
    fn h264_parameter_set_detection_ignores_slices() {
        assert!(au_has_h264_parameter_sets(&[0, 0, 0, 1, 0x67, 1, 2]));
        assert!(au_has_h264_parameter_sets(&[0, 0, 0, 1, 0x68, 3]));
        assert!(au_has_h264_parameter_sets(&[
            0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 3
        ]));
        assert!(!au_has_h264_parameter_sets(&[0, 0, 0, 1, 0x65, 3]));
    }

    #[test]
    fn parameter_sets_are_extracted_even_when_bundled_with_a_slice() {
        let access_unit = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 1, 0x68, 3, // PPS
            0, 0, 0, 1, 0x65, 9, 9, // IDR slice
        ];
        assert_eq!(
            extract_h264_parameter_sets(&access_unit),
            vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]
        );
        assert!(extract_h264_parameter_sets(&[0, 0, 0, 1, 0x65, 9]).is_empty());
    }

    #[test]
    fn idr_without_repeated_parameter_sets_uses_the_cached_sps_pps() {
        let decoder = Decoder::new(RemoteTrackKind::Video, Codec::H264).unwrap();
        decoder.latest_frame();
        let sample = include_bytes!("../fixtures/sample.h264");
        let nals = nals(sample);
        assert_eq!(
            nals[..4]
                .iter()
                .map(|nal| nal[0] & 0x1f)
                .collect::<Vec<_>>(),
            vec![7, 8, 6, 5],
            "fixture must start with SPS, PPS, SEI and an IDR"
        );
        let mut sequence = 0;
        // Config arrives as separate access units, then an IDR slice without
        // any repeated parameter sets. The marker bit closes each unit.
        let Some((last, rest)) = nals[..3].split_last() else {
            unreachable!("the fixture has SPS, PPS and SEI");
        };
        for nal in rest {
            push_nal(&decoder, &mut sequence, 1, nal);
        }
        push_nal_marked(&decoder, &mut sequence, 1, last, true);
        std::thread::sleep(Duration::from_millis(30));
        assert!(decoder.latest_frame().is_none());
        let Some((last, rest)) = nals[3..].split_last() else {
            unreachable!("the fixture has an IDR");
        };
        for nal in rest {
            push_nal(&decoder, &mut sequence, 2, nal);
        }
        push_nal_marked(&decoder, &mut sequence, 2, last, true);
        assert_eq!(&wait_for_frame(&decoder)[0..2], &[0xff, 0xd8]);
        decoder.stop();
    }

    #[test]
    fn simultaneous_android_shares_decode_and_recover_independently() {
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                std::thread::spawn(move || {
                    let decoder = Decoder::new(RemoteTrackKind::Video, Codec::H264).unwrap();
                    decoder.latest_frame();
                    let mut sequence = worker * 1000;
                    push_nal_marked(&decoder, &mut sequence, 1, &[0x65, 0xff], true);
                    push_sample(&decoder, &mut sequence, 2);
                    assert_eq!(&wait_for_frame(&decoder)[0..2], &[0xff, 0xd8]);
                    decoder.stop();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
