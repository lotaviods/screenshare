//! Native GPU capture: PipeWire DMA-BUF straight into VAAPI H.264.
//!
//! Frames never touch the CPU: the compositor exports DMA-BUFs, VAAPI imports
//! them, `scale_vaapi` converts RGB to NV12 on the GPU when needed, and
//! `h264_vaapi` encodes.
//!
//! `shortcut: no JPEG self-view on this path yet; add one before it is needed.`

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use sharescreen_core::{CaptureError, CaptureOptions, Codec, EncodedSample, MediaKind};
use sharescreen_native_media::capture::{pipewire, pipewire::Memory};
use sharescreen_native_media::encode::{EncoderConfig, FfmpegEncoder, VideoEncoder};

/// Whether the native path can encode this request.
pub fn available(codec: Codec) -> bool {
    matches!(codec, Codec::H264 | Codec::Vp8 | Codec::Vp9)
        && sharescreen_native_media::encode::ffmpeg::vaapi_available(codec)
}

/// A running native capture: a PipeWire thread pushing encoded samples.
pub struct Stream {
    running: Arc<AtomicUsize>,
    thread: JoinHandle<()>,
}

impl Stream {
    pub fn stop(self) {
        self.running.store(0, Ordering::SeqCst);
        let _ = self.thread.join();
    }

    /// No preview on the native path yet; the UI falls back to no self-view.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
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
    let bitrate_kbps = options.bitrate_kbps.max(1000);
    let codec = options.codec;
    let running = Arc::new(AtomicUsize::new(1));
    let flag = running.clone();

    let thread = std::thread::Builder::new()
        .name("sharescreen-native".into())
        .spawn(move || {
            let raw = fd.as_raw_fd();
            let _keep_alive = fd;
            let mut encoder: Option<FfmpegEncoder> = None;
            let mut failed = false;

            let result = pipewire::run(raw, node_id, Memory::DmaBuf, None, flag, move |frame| {
                if failed || frame.format().is_none() {
                    return;
                }
                if encoder.is_none() {
                    let config = EncoderConfig {
                        width: frame.width,
                        height: frame.height,
                        fps,
                        bitrate_kbps,
                        codec,
                    };
                    match FfmpegEncoder::new(config) {
                        Ok(created) => encoder = Some(created),
                        Err(err) => {
                            tracing::error!("native encoder unavailable: {err}");
                            failed = true;
                            return;
                        }
                    }
                }
                let Some(encoder) = encoder.as_mut() else {
                    return;
                };
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
                            if sink.blocking_send(sample).is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!("native encode failed: {err}");
                        failed = true;
                    }
                }
            });

            if let Err(err) = result {
                tracing::error!("native capture stopped: {err}");
            }
        })
        .map_err(|e| CaptureError::Failed(format!("failed to spawn native capture thread: {e}")))?;

    Ok(Stream { running, thread })
}
