//! `sharescreen-media`: ingests remote WebRTC tracks and decodes them.
//!
//! Remote tracks arrive as raw RTP packets. The packets are depacketized and
//! decoded with FFmpeg, without GStreamer:
//!
//! - audio → Opus → PCM → the system output (you hear the other side);
//! - video → H.264/VP8/VP9 → RGB → JPEG, kept as the latest frame for the UI.
//!
//! The video path deliberately stops at JPEG: the frames are small enough to
//! hand to the GPUI renderer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use sharescreen_audio::Output;
use sharescreen_core::Codec;
use sharescreen_native_media::decode::{
    JpegEncoder, OpusDecoder, RtpDepacketizer, RtpKind, VideoDecoder,
};
use sharescreen_webrtc::{RemoteTrack, RemoteTrackKind};

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
        let (tx, rx) = mpsc::channel();

        let flag = running.clone();
        let volume_slot = volume.clone();
        let frames_slot = frames.clone();
        let thread = std::thread::Builder::new()
            .name("sharescreen-media".into())
            .spawn(move || {
                run(kind, codec, rtp_kind, rx, flag, volume_slot, frames_slot);
            })
            .map_err(|e| MediaError::Pipeline(format!("failed to spawn media thread: {e}")))?;

        Ok(Self {
            running,
            thread,
            frames,
            volume,
            tx,
        })
    }

    /// Push a full RTP packet into the decoder.
    pub fn push_rtp(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(bytes);
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
        let _ = self.thread.join();
    }
}

fn run(
    kind: RemoteTrackKind,
    codec: Codec,
    rtp_kind: RtpKind,
    rx: Receiver<Vec<u8>>,
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
    let mut process = |frame: &[u8]| match kind {
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
                    output.play(&scaled);
                }
                Err(e) => tracing::warn!("Opus decode failed: {e}"),
            }
        }
    };

    while running.load(Ordering::SeqCst) {
        let packet = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => {
                for frame in depacketizer.flush() {
                    process(&frame);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        for frame in depacketizer.push(&packet) {
            process(&frame);
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

/// Decodes remote tracks and exposes the results to the UI.
pub struct RemoteMedia {
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
            tracks: RwLock::new(HashMap::new()),
            frames: RwLock::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Attach a remote track and start feeding it with the track's RTP packets.
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

        let running = decoder.running.clone();
        let tx = decoder.tx.clone();
        tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                let Some(packet) = track.next_packet().await else {
                    break;
                };
                let _ = tx.send(packet.to_bytes());
            }
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
