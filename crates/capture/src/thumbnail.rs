//! Lightweight second-stream thumbnail encoder for Linux (VAAPI path).
//!
//! This module runs entirely on the capture thread (inside the PipeWire
//! callback). It reuses the same `Frame` that the primary encoder already
//! consumed from the DMA-BUF, encodes it at ≤854×480 / 5 fps / 300 kbps,
//! and ships the result via `try_send` — never blocking the capture loop.
//!
//! All failures are logged and silently discarded so a broken thumbnail path
//! never disrupts the primary video stream.

#[cfg(target_os = "linux")]
use sharescreen_core::{Codec, EncodedSample, MediaKind};
#[cfg(target_os = "linux")]
use sharescreen_native_media::encode::{EncoderConfig, FfmpegEncoder, VideoEncoder};
#[cfg(target_os = "linux")]
use sharescreen_native_media::frame::Frame;

/// Maximum width of the thumbnail stream. Height is scaled proportionally.
pub const THUMB_MAX_WIDTH: u32 = 854;
/// Maximum height of the thumbnail stream.
pub const THUMB_MAX_HEIGHT: u32 = 480;
/// Frame rate of the thumbnail stream.
pub const THUMB_FPS: u32 = 5;
/// Bitrate of the thumbnail stream in kbps.
pub const THUMB_BITRATE_KBPS: u32 = 300;

/// Compute thumbnail dimensions that preserve the aspect ratio of `(src_w,
/// src_h)` while fitting within `(THUMB_MAX_WIDTH, THUMB_MAX_HEIGHT)`.
///
/// Returns `(0, 0)` when either source dimension is zero.
pub fn thumb_size(src_w: u32, src_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    // Scale to fit width first, then clamp height.
    let w_by_width = THUMB_MAX_WIDTH;
    let h_by_width = (src_h as u64 * THUMB_MAX_WIDTH as u64 / src_w as u64) as u32;

    if h_by_width <= THUMB_MAX_HEIGHT {
        // Width-limited — make both dimensions even (codec requirement).
        (w_by_width & !1, h_by_width.max(1) & !1)
    } else {
        // Height-limited.
        let h = THUMB_MAX_HEIGHT;
        let w = (src_w as u64 * THUMB_MAX_HEIGHT as u64 / src_h as u64) as u32;
        (w.max(1) & !1, h & !1)
    }
}

/// Per-frame thumbnail state kept on the capture thread.
#[cfg(target_os = "linux")]
pub struct ThumbnailEncoder {
    encoder: Option<FfmpegEncoder>,
    codec: Codec,
    frame_count: u64,
    /// Encode every Nth primary frame to hit THUMB_FPS.
    every: u64,
    thumb_w: u32,
    thumb_h: u32,
    sender: tokio::sync::mpsc::Sender<EncodedSample>,
}

#[cfg(target_os = "linux")]
impl ThumbnailEncoder {
    /// Create the encoder state.  The actual FFmpeg encoder is deferred until
    /// the first frame so we know the negotiated frame size.
    pub fn new(
        codec: Codec,
        primary_fps: u32,
        sender: tokio::sync::mpsc::Sender<EncodedSample>,
    ) -> Self {
        let every = (primary_fps.max(THUMB_FPS) / THUMB_FPS).max(1) as u64;
        Self {
            encoder: None,
            codec,
            frame_count: 0,
            every,
            thumb_w: 0,
            thumb_h: 0,
            sender,
        }
    }

    /// Offer one primary frame. The frame is silently discarded when it is not
    /// a thumbnail sample tick, when the sender is full, or when the encoder
    /// fails.  The primary encode path is never interrupted.
    pub fn try_encode(&mut self, frame: &Frame) {
        let count = self.frame_count;
        self.frame_count = self.frame_count.wrapping_add(1);

        // Rate limiting: only encode every `self.every` frames.
        if count % self.every != 0 {
            return;
        }

        // Lazy encoder init on the first accepted frame.
        if self.encoder.is_none() {
            let (tw, th) = thumb_size(frame.width, frame.height);
            if tw == 0 || th == 0 {
                tracing::warn!(
                    frame_w = frame.width,
                    frame_h = frame.height,
                    "thumbnail: cannot compute thumbnail size; skipping"
                );
                return;
            }
            self.thumb_w = tw;
            self.thumb_h = th;
            let config = EncoderConfig {
                width: tw,
                height: th,
                fps: THUMB_FPS,
                bitrate_kbps: THUMB_BITRATE_KBPS,
                codec: self.codec,
                hardware_encoding: true,
            };
            match FfmpegEncoder::new(config) {
                Ok(enc) => {
                    tracing::info!(
                        thumb_w = tw,
                        thumb_h = th,
                        fps = THUMB_FPS,
                        bitrate_kbps = THUMB_BITRATE_KBPS,
                        "thumbnail encoder initialized"
                    );
                    self.encoder = Some(enc);
                }
                Err(err) => {
                    tracing::warn!(%err, "thumbnail encoder init failed; thumbnail stream disabled");
                    return;
                }
            }
        }

        // The VAAPI encoder requires that the input frame matches the
        // encoder's configured resolution.  When the compositor has
        // negotiated a different size than the thumbnail target, we need a
        // scaled frame.  The primary FfmpegEncoder operates on DMA-BUF
        // `Frame` objects at the full resolution; the thumbnail encoder is
        // also configured for the thumbnail resolution — so encoding the
        // full-resolution frame into it would fail with a size mismatch.
        //
        // Strategy: construct a synthetic scaled Frame from the primary one.
        // Because VAAPI encoding validates `frame.width/height` against the
        // encoder config, we fabricate a Frame with the thumbnail dimensions
        // while keeping the same DMA-BUF descriptors.  VAAPI's
        // `scale_vaapi` filter inside the encoder handles the actual
        // down-scaling on the GPU.
        //
        // If the frame is already at or below thumbnail resolution, pass
        // it through directly.
        let scaled = if frame.width == self.thumb_w && frame.height == self.thumb_h {
            None
        } else {
            // Build a Frame view at the thumbnail dimensions.  The
            // underlying DMA-BUF objects are shared (we borrow them via the
            // frame reference), so no copy is made.
            Some(frame.with_size(self.thumb_w, self.thumb_h))
        };

        let encode_frame = scaled.as_ref().unwrap_or(frame);

        let enc = match self.encoder.as_mut() {
            Some(e) => e,
            None => return,
        };

        match enc.encode(encode_frame, false) {
            Ok(frames) => {
                for f in frames {
                    let sample = EncodedSample {
                        kind: MediaKind::Video,
                        codec: self.codec,
                        data: f.data,
                        timestamp_micros: f.pts_micros,
                        duration_micros: f.duration_micros,
                        keyframe: f.keyframe,
                    };
                    // try_send: drop if full, never block the capture thread.
                    match self.sender.try_send(sample) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::debug!("thumbnail queue full; dropping frame");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!("thumbnail consumer closed; disabling thumbnail encoder");
                            self.encoder = None;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, "thumbnail encode failed; will retry next tick");
                // Don't disable the encoder on a transient failure.
            }
        }
    }
}
