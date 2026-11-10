//! Native audio capture: PulseAudio → FFmpeg/Opus, no GStreamer.
//!
//! Opt-in via `SHARESCREEN_MEDIA=native` while the GStreamer pipeline stays the
//! default, so the two paths can be compared A/B. The microphone records the
//! selected source; system audio mixes every non-call playback stream, exactly
//! like the GStreamer path, then encodes straight to Opus packets. Volume and
//! mute apply as software gain, so no `volume` element is involved.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use sharescreen_core::{AudioError, Codec, EncodedSample, MediaKind};
use sharescreen_native_media::encode::OpusEncoder;
use tokio::sync::mpsc;

use crate::pulse::{Mixer, Recorder};

/// Whether the native audio path is usable.
pub fn available() -> bool {
    std::env::var("SHARESCREEN_MEDIA").is_ok_and(|value| value == "native")
        && sharescreen_native_media::encode::audio::opus_available()
}

/// A running native capture: a capture thread pushing Opus samples.
pub struct Stream {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
}

impl Stream {
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.thread.join();
    }

    pub fn set_volume(&self, volume: f64) {
        *self.volume.lock().unwrap() = volume;
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }
}

/// Software gain and mute, applied before encoding.
struct Gain {
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
}

impl Gain {
    fn new(volume: f64) -> Self {
        Self {
            volume: Arc::new(Mutex::new(volume)),
            muted: Arc::new(AtomicBool::new(false)),
        }
    }

    fn apply(&self, pcm: &mut [i16]) {
        if self.muted.load(Ordering::SeqCst) {
            pcm.fill(0);
            return;
        }
        let volume = *self.volume.lock().unwrap();
        if (volume - 1.0).abs() < f64::EPSILON {
            return;
        }
        for sample in pcm {
            *sample = (*sample as f64 * volume).clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        }
    }
}

/// Accumulates interleaved PCM into fixed Opus frames and pushes them to `sink`.
struct Framer {
    buf: Vec<i16>,
    gain: Gain,
}

impl Framer {
    fn new(gain: Gain) -> Self {
        Self {
            buf: Vec::new(),
            gain,
        }
    }

    /// Returns false when the consumer is gone.
    fn push(
        &mut self,
        encoder: &mut OpusEncoder,
        sink: &mpsc::Sender<EncodedSample>,
        pcm: &[i16],
    ) -> bool {
        self.buf.extend_from_slice(pcm);
        let frame = encoder.frame_len();
        while self.buf.len() >= frame {
            let mut chunk: Vec<i16> = self.buf.drain(..frame).collect();
            self.gain.apply(&mut chunk);
            let encoded = match encoder.encode(&chunk) {
                Ok(frames) => frames,
                Err(err) => {
                    tracing::warn!("native opus encode failed: {err}");
                    return false;
                }
            };
            if let Some(frame) = encoded {
                let sample = EncodedSample {
                    kind: MediaKind::Audio,
                    codec: Codec::Opus,
                    data: frame.data,
                    timestamp_micros: frame.pts_micros,
                    duration_micros: frame.duration_micros,
                    keyframe: false,
                };
                if sink.blocking_send(sample).is_err() {
                    return false;
                }
            }
        }
        true
    }
}

/// Start recording `device` (or the default source) as Opus.
pub fn spawn_mic(
    device: Option<String>,
    bitrate_kbps: u32,
    sink: mpsc::Sender<EncodedSample>,
) -> Result<Stream, AudioError> {
    spawn("sharescreen-native-mic", move |running, gain, ready| {
        run_mic(device, bitrate_kbps, running, gain, sink, ready)
    })
}

/// Start mixing the system output as Opus.
pub fn spawn_system(
    device: Option<String>,
    bitrate_kbps: u32,
    sink: mpsc::Sender<EncodedSample>,
) -> Result<Stream, AudioError> {
    spawn("sharescreen-native-system", move |running, gain, ready| {
        run_system(device, bitrate_kbps, running, gain, sink, ready)
    })
}

/// Spawn a capture thread and wait until it reports success or failure.
fn spawn(
    name: &str,
    body: impl FnOnce(Arc<AtomicBool>, Gain, std::sync::mpsc::SyncSender<Result<(), String>>)
        + Send
        + 'static,
) -> Result<Stream, AudioError> {
    let running = Arc::new(AtomicBool::new(true));
    let gain = Gain::new(1.0);
    let volume = gain.volume.clone();
    let muted = gain.muted.clone();
    let flag = running.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || body(flag, gain, ready_tx))
        .map_err(|e| AudioError::Failed(format!("failed to spawn {name} thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Stream {
            running,
            thread,
            volume,
            muted,
        }),
        Ok(Err(e)) => {
            running.store(false, Ordering::SeqCst);
            let _ = thread.join();
            Err(AudioError::Unavailable(e))
        }
        Err(_) => {
            let _ = thread.join();
            Err(AudioError::Failed(format!(
                "{name} thread exited before starting"
            )))
        }
    }
}

fn run_mic(
    device: Option<String>,
    bitrate_kbps: u32,
    running: Arc<AtomicBool>,
    gain: Gain,
    sink: mpsc::Sender<EncodedSample>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let mut recorder = match Recorder::open(device) {
        Ok(recorder) => recorder,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut encoder = match OpusEncoder::new(bitrate_kbps) {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let mut framer = Framer::new(gain);
    tracing::info!("native microphone capture started");
    while running.load(Ordering::SeqCst) {
        recorder.pump();
        let pcm = recorder.take();
        if !pcm.is_empty() && !framer.push(&mut encoder, &sink, &pcm) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn run_system(
    device: Option<String>,
    bitrate_kbps: u32,
    running: Arc<AtomicBool>,
    gain: Gain,
    sink: mpsc::Sender<EncodedSample>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let mut mixer = match Mixer::open(device) {
        Ok(mixer) => mixer,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut encoder = match OpusEncoder::new(bitrate_kbps) {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let mut framer = Framer::new(gain);
    tracing::info!("native system audio capture started");
    while running.load(Ordering::SeqCst) {
        mixer.pump();
        if let Some(chunk) = mixer.chunk() {
            if !framer.push(&mut encoder, &sink, &chunk) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muted_gain_silences_audio() {
        let gain = Gain::new(0.5);
        gain.muted.store(true, Ordering::SeqCst);
        let mut pcm = [1000i16, -2000];
        gain.apply(&mut pcm);
        assert_eq!(pcm, [0, 0]);
    }

    #[test]
    fn volume_scales_and_clamps() {
        let gain = Gain::new(2.0);
        let mut pcm = [20_000i16, -30_000];
        gain.apply(&mut pcm);
        assert_eq!(pcm, [i16::MAX, i16::MIN]);
    }
}
