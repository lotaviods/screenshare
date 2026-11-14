//! Native audio capture: platform capture → FFmpeg/Opus.
//!
//! Linux records the microphone or the application playback streams through
//! `libpulse-binding`; Windows records the capture endpoint or the process
//! loopback through WASAPI. Both are converted to 48 kHz stereo S16LE and
//! encoded straight to Opus packets. Volume and mute apply as software gain, so
//! no `volume` element is involved.
//!
//! This module owns the platform-independent pipeline (threading, framing,
//! gain, Opus encode). Each platform backend lives in its own submodule and
//! only has to provide a [`Capture`] source.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use sharescreen_core::{AudioError, Codec, EncodedSample, MediaKind};
use sharescreen_native_media::encode::OpusEncoder;
use sonora::config::{EchoCanceller, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel};
use sonora::{AudioProcessing, Config as DspConfig, StreamConfig};
use tokio::sync::mpsc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux::{open_echo_reference, open_mic, open_system};
#[cfg(windows)]
use windows::{open_echo_reference, open_mic, open_system};

/// A running native capture: a capture thread pushing Opus samples.
pub struct Stream {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl Stream {
    pub fn finished(&self) -> bool {
        self.thread.is_finished()
    }

    pub fn finish_error(self) -> String {
        let result = self.thread.join();
        if result.is_err() {
            return "capture thread panicked".into();
        }
        self.error
            .lock()
            .ok()
            .and_then(|mut e| e.take())
            .unwrap_or_else(|| "capture thread stopped unexpectedly".into())
    }
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
    dsp_buf: Vec<i16>,
    render_buf: Vec<i16>,
    dsp: Option<AudioProcessing>,
    gain: Gain,
}

impl Framer {
    fn new(gain: Gain, process_voice: bool, noise_suppression: bool, echo_reference: bool) -> Self {
        let dsp = process_voice.then(|| {
            let mut dsp = AudioProcessing::builder()
                .config(DspConfig {
                    high_pass_filter: Some(HighPassFilter::default()),
                    echo_canceller: echo_reference.then(EchoCanceller::default),
                    noise_suppression: noise_suppression.then(|| NoiseSuppression {
                        level: NoiseSuppressionLevel::High,
                        ..NoiseSuppression::default()
                    }),
                    ..DspConfig::default()
                })
                .capture_config(StreamConfig::new(48_000, 2))
                .render_config(StreamConfig::new(48_000, 2))
                .build();
            if echo_reference {
                // Approximate the receiver prebuffer. The AEC can track the
                // residual path, but needs a realistic initial render delay.
                let _ = dsp.set_stream_delay_ms(60);
            }
            dsp
        });
        Self {
            buf: Vec::new(),
            dsp_buf: Vec::new(),
            render_buf: Vec::new(),
            dsp,
            gain,
        }
    }

    fn push_render(&mut self, pcm: &[i16]) -> Result<(), String> {
        let Some(dsp) = &mut self.dsp else {
            return Ok(());
        };
        self.render_buf.extend_from_slice(pcm);
        while self.render_buf.len() >= 960 {
            let input: Vec<i16> = self.render_buf.drain(..960).collect();
            let mut output = vec![0i16; input.len()];
            dsp.process_render_i16(&input, &mut output)
                .map_err(|e| format!("echo reference DSP failed: {e}"))?;
        }
        Ok(())
    }

    /// Drop packets when the consumer is overloaded, keeping capture responsive.
    fn push(
        &mut self,
        encoder: &mut OpusEncoder,
        sink: &mpsc::Sender<EncodedSample>,
        pcm: &[i16],
    ) -> Result<(), String> {
        // Sonora's WebRTC processing operates on 10 ms frames. Buffer capture
        // chunks to that boundary before processing; then the Opus framer
        // collects the processed PCM into its encoder frame size.
        if let Some(dsp) = &mut self.dsp {
            self.dsp_buf.extend_from_slice(pcm);
            while self.dsp_buf.len() >= 960 {
                let input: Vec<i16> = self.dsp_buf.drain(..960).collect();
                let mut output = vec![0i16; input.len()];
                dsp.process_capture_i16(&input, &mut output)
                    .map_err(|e| format!("audio DSP failed: {e}"))?;
                self.buf.extend_from_slice(&output);
            }
        } else {
            self.buf.extend_from_slice(pcm);
        }
        let frame = encoder.frame_len();
        while self.buf.len() >= frame {
            let mut chunk: Vec<i16> = self.buf.drain(..frame).collect();
            self.gain.apply(&mut chunk);
            let encoded = encoder
                .encode(&chunk)
                .map_err(|e| format!("Opus encode failed: {e}"))?;
            if let Some(frame) = encoded {
                let sample = EncodedSample {
                    kind: MediaKind::Audio,
                    codec: Codec::Opus,
                    data: frame.data,
                    timestamp_micros: frame.pts_micros,
                    duration_micros: frame.duration_micros,
                    keyframe: false,
                };
                match sink.try_send(sample) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return Err("audio consumer disconnected".into())
                    }
                }
            }
        }
        Ok(())
    }
}

/// Start recording `device` (or the default source) as Opus.
pub fn spawn_mic(
    device: Option<String>,
    bitrate_kbps: u32,
    noise_suppression: bool,
    echo_cancellation: bool,
    sink: mpsc::Sender<EncodedSample>,
) -> Result<Stream, AudioError> {
    spawn("sharescreen-native-mic", move |running, gain, ready| {
        run_mic(
            device,
            bitrate_kbps,
            noise_suppression,
            echo_cancellation,
            running,
            gain,
            sink,
            ready,
        )
    })
}

/// Start capturing system audio as Opus.
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
    body: impl FnOnce(
            Arc<AtomicBool>,
            Gain,
            std::sync::mpsc::SyncSender<Result<(), String>>,
        ) -> Result<(), String>
        + Send
        + 'static,
) -> Result<Stream, AudioError> {
    let running = Arc::new(AtomicBool::new(true));
    let gain = Gain::new(1.0);
    let volume = gain.volume.clone();
    let muted = gain.muted.clone();
    let flag = running.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let error = Arc::new(Mutex::new(None));
    let thread_error = error.clone();

    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if let Err(e) = body(flag, gain, ready_tx) {
                if let Ok(mut error) = thread_error.lock() {
                    *error = Some(e);
                }
            }
        })
        .map_err(|e| AudioError::Failed(format!("failed to spawn {name} thread: {e}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Stream {
            running,
            thread,
            volume,
            muted,
            error,
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

/// A platform capture that yields interleaved 48 kHz stereo S16LE samples.
///
/// Implemented per platform in the [`linux`] and [`windows`] submodules, where
/// `pump` drains whatever the backend has ready and `next` returns the pending
/// samples (or `None` when there are none).
trait Capture {
    fn pump(&mut self) -> Result<(), String>;
    fn next(&mut self) -> Option<Vec<i16>>;
}

fn run_mic(
    device: Option<String>,
    bitrate_kbps: u32,
    noise_suppression: bool,
    echo_cancellation: bool,
    running: Arc<AtomicBool>,
    gain: Gain,
    sink: mpsc::Sender<EncodedSample>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let mut capture = match open_mic(device) {
        Ok(capture) => capture,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let mut encoder = match OpusEncoder::new(bitrate_kbps) {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(()));

    let mut reference = match echo_cancellation.then(open_echo_reference) {
        None => None,
        Some(Ok(reference)) => Some(reference),
        Some(Err(error)) => {
            tracing::warn!(%error, noise_suppression, "echo reference unavailable; echo cancellation disabled");
            None
        }
    };
    let mut framer = Framer::new(gain, true, noise_suppression, reference.is_some());
    tracing::info!("native microphone capture started");
    while running.load(Ordering::SeqCst) {
        capture.pump()?;
        if let Some(reference) = &mut reference {
            reference.pump()?;
            if let Some(render) = reference.next() {
                framer.push_render(&render)?;
            }
        }
        if let Some(pcm) = capture.next() {
            framer.push(&mut encoder, &sink, &pcm)?;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn run_system(
    device: Option<String>,
    bitrate_kbps: u32,
    running: Arc<AtomicBool>,
    gain: Gain,
    sink: mpsc::Sender<EncodedSample>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let mut capture = match open_system(device) {
        Ok(capture) => capture,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let mut encoder = match OpusEncoder::new(bitrate_kbps) {
        Ok(encoder) => encoder,
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            return Ok(());
        }
    };
    let _ = ready.send(Ok(()));

    let mut framer = Framer::new(gain, false, false, false);
    tracing::info!("native system audio capture started");
    while running.load(Ordering::SeqCst) {
        capture.pump()?;
        if let Some(chunk) = capture.next() {
            framer.push(&mut encoder, &sink, &chunk)?;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
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

    #[test]
    fn unexpected_worker_error_is_retained() {
        let stream = spawn("audio-test", |_, _, ready| {
            ready.send(Ok(())).unwrap();
            Err("device disconnected".into())
        })
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !stream.finished() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(stream.finished());
        assert_eq!(stream.finish_error(), "device disconnected");
    }
}
