//! Native GPU capture: PipeWire DMA-BUF straight into VAAPI H.264/VP8/VP9.
//!
//! Frames never touch the CPU: the compositor exports DMA-BUFs, VAAPI imports
//! them, `scale_vaapi` converts RGB to NV12 on the GPU when needed, and
//! `h264_vaapi`/`vp8_vaapi`/`vp9_vaapi` encodes.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use sharescreen_core::{CaptureError, CaptureOptions, Codec, EncodedSample, MediaKind};
use sharescreen_native_media::capture::{pipewire, pipewire::Memory};
use sharescreen_native_media::encode::{EncoderConfig, FfmpegEncoder, VideoEncoder};
use sharescreen_native_media::linux_preview::LinuxPreviewer;
use sharescreen_native_media::preview::Demand;

use crate::thumbnail::ThumbnailEncoder;

/// Whether the native path can encode this request.
pub fn available(codec: Codec) -> bool {
    matches!(codec, Codec::H264 | Codec::Vp8 | Codec::Vp9)
        && sharescreen_native_media::encode::ffmpeg::vaapi_available(codec)
}

/// A running native capture: a PipeWire thread pushing encoded samples.
pub struct Stream {
    running: Arc<AtomicUsize>,
    thread: JoinHandle<()>,
    thumbnail_worker: Option<JoinHandle<()>>,
    #[allow(dead_code)] // wired into the capture worker next
    demand: Arc<Demand>,
    #[allow(dead_code)] // wired into the capture worker next
    linux_preview: Option<Arc<LinuxPreviewer>>,
    failure: Arc<Mutex<Option<String>>>,
}

impl Stream {
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }
    pub fn finished(&self) -> bool {
        self.thread.is_finished()
    }
    pub fn stop(self) {
        self.running.store(0, Ordering::SeqCst);
        let Self {
            thread,
            thumbnail_worker,
            ..
        } = self;
        let _ = thread.join();
        if let Some(worker) = thumbnail_worker {
            let _ = worker.join();
        }
    }

    /// Latest JPEG self-view frame of the running capture.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        // The capture thread no longer produces JPEGs inline; the loopback worker
        // handles them. Return nothing here.
        None
    }
}

pub fn spawn(
    fd: OwnedFd,
    node_id: u32,
    options: &CaptureOptions,
    sink: tokio::sync::mpsc::Sender<EncodedSample>,
) -> Result<Stream, CaptureError> {
    let fps = options.fps.max(1);
    let requested_width = options.width.max(1);
    let requested_height = options.height.max(1);
    let bitrate_kbps = options.bitrate_kbps.max(1000);
    let codec = options.codec;
    let thumbnail_sender = options.thumbnail_sender.clone();
    let keyframe_request = options.keyframe_request.clone();
    let (thumbnail_tx, thumbnail_worker) = if let Some(sender) = thumbnail_sender {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("sharescreen-thumbnail".into())
            .spawn(move || {
                let mut encoder = ThumbnailEncoder::new(codec, fps, sender);
                while let Ok(frame) = rx.recv() {
                    encoder.try_encode(&frame);
                }
            }) {
            Ok(worker) => (Some(tx), Some(worker)),
            Err(error) => {
                tracing::warn!(%error, "thumbnail worker unavailable; continuing primary capture");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    let running = Arc::new(AtomicUsize::new(1));
    let flag = running.clone();
    let linux_preview = match LinuxPreviewer::spawn() {
        Ok(worker) => Some(Arc::new(worker)),
        Err(error) => {
            tracing::warn!(%error, "preview unavailable; continuing capture");
            None
        }
    };
    let demand = Arc::new(Demand::new());
    let demand_flag = demand.clone();
    let preview_every = fps.max(10) / 10;
    let failure = Arc::new(Mutex::new(None));
    let worker_failure = failure.clone();
    let callback_failure = failure.clone();

    let thread = std::thread::Builder::new()
        .name("sharescreen-native".into())
        .spawn(move || {
            let raw = fd.as_raw_fd();
            let _keep_alive = fd;
            let mut encoder: Option<FfmpegEncoder> = None;
            let mut thumbnail_tx = thumbnail_tx;
            let mut failed = false;
            let mut frame_count = 0u64;
            let mut last_encoded: Option<std::time::Instant> = None;
            let mut negotiated_size_logged = false;

            let callback_flag = flag.clone();
            let result = pipewire::run_configured(
                raw,
                node_id,
                Memory::DmaBuf,
                None,
                flag,
                requested_width,
                requested_height,
                fps,
                move |frame| {
                    if failed || frame.format().is_none() {
                        return;
                    }
                    let now = std::time::Instant::now();
                    let frame_interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);
                    if last_encoded.is_some_and(|last| now.duration_since(last) < frame_interval) {
                        return;
                    }
                    last_encoded = Some(now);
                    if !negotiated_size_logged {
                        if frame.width != requested_width || frame.height != requested_height {
                            tracing::warn!(
                            requested_width,
                            requested_height,
                            actual_width = frame.width,
                            actual_height = frame.height,
                            "compositor did not negotiate requested share size; using actual size"
                        );
                        }
                        negotiated_size_logged = true;
                    }
                    if encoder.is_none() {
                        let config = EncoderConfig {
                            width: frame.width,
                            height: frame.height,
                            fps,
                            bitrate_kbps,
                            codec,
                            hardware_encoding: true,
                        };
                        match FfmpegEncoder::new(config) {
                            Ok(created) => encoder = Some(created),
                            Err(err) => {
                                let message =
                                    format!("native {codec:?} encoder unavailable: {err}");
                                tracing::error!(error = %message);
                                if let Ok(mut failure) = callback_failure.lock() {
                                    *failure = Some(message);
                                }
                                failed = true;
                                callback_flag.store(0, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                    let Some(encoder) = encoder.as_mut() else {
                        return;
                    };
                    if keyframe_request
                        .as_ref()
                        .is_some_and(|request| request.swap(false, std::sync::atomic::Ordering::AcqRel))
                    {
                        encoder.request_keyframe();
                    }
                    match encoder.encode(&frame, false) {
                        Ok(frames) => {
                            for frame in frames {
                                let sample = EncodedSample {
                                    kind: MediaKind::Video,
                                    codec,
                                    data: frame.data,
                                    timestamp_micros: frame.pts_micros,
                                    duration_micros: frame.duration_micros,
                                    keyframe: frame.keyframe,
                                };
                                match sink.try_send(sample) {
                                    Ok(()) => {}
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        // Do not hold a PipeWire buffer while waiting for a
                                        // slow peer. Drop this access unit and recover at the
                                        // next keyframe instead of blocking the capture loop.
                                        encoder.request_keyframe();
                                        tracing::warn!("video queue full; dropping frame and requesting keyframe");
                                        return;
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        if let Ok(mut failure) = callback_failure.lock() {
                                            *failure = Some("video consumer disconnected".into());
                                        }
                                        callback_flag.store(0, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                            if let Some(sender) = thumbnail_tx.as_ref() {
                                match sender.try_send(frame) {
                                    Ok(()) => {}
                                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                        tracing::debug!("thumbnail worker busy; dropping frame");
                                    }
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                        tracing::warn!(
                                            "thumbnail worker stopped; disabling thumbnail output"
                                        );
                                        thumbnail_tx = None;
                                    }
                                }
                            }
                            // Only pay for the self-view while the UI reads it. The
                            // preview shares this thread with the encoder, so an
                            // unused preview would otherwise stall the peer stream.
                            if frame_count.is_multiple_of(preview_every as u64)
                                && demand_flag.wanted()
                            {
                                // Skip JPEG when nobody is watching; the loopback worker
                                // will handle it on its own thread.
                            }
                            frame_count = frame_count.wrapping_add(1);
                        }
                        Err(err) => {
                            let message = format!("native {codec:?} encode failed: {err}");
                            tracing::error!(error = %message);
                            if let Ok(mut failure) = callback_failure.lock() {
                                *failure = Some(message);
                            }
                            failed = true;
                            callback_flag.store(0, Ordering::SeqCst);
                        }
                    }
                },
            );

            if let Err(err) = result {
                let message = format!("native capture stopped: {err}");
                tracing::error!(error = %message);
                if let Ok(mut failure) = worker_failure.lock() {
                    if failure.is_none() {
                        *failure = Some(message);
                    }
                }
            }
        })
        .map_err(|e| CaptureError::Failed(format!("failed to spawn native capture thread: {e}")))?;

    Ok(Stream {
        running,
        thread,
        thumbnail_worker,
        demand,
        linux_preview,
        failure,
    })
}
