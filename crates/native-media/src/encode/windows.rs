//! FFmpeg-backed software encoder for Windows raw frames.
//!
//! FFmpeg is used as a codec layer only: libx264/libvpx converts the BGRA
//! capture frame to YUV420P with `swscale` and emits Annex-B H.264 or VP8/VP9.

use std::ffi::CString;
use std::ptr;

use ffmpeg_sys_next as ffi;
use sharescreen_core::Codec;

use super::{Backend, EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::frame::Frame;

/// Whether FFmpeg exposes an encoder for `codec`.
pub fn codec_available(codec: Codec) -> bool {
    codec_names(codec, false)
        .iter()
        .any(|name| find_codec(name).is_some())
        || codec_names(codec, true)
            .iter()
            .any(|name| find_codec(name).is_some())
}

/// Candidate encoders for `codec`. libx264 is preferred for H.264
/// because it repeats SPS/PPS in-band for RTP; the Media Foundation encoder is
/// a fallback. libvpx is preferred for VP8/VP9.
fn codec_names(codec: Codec, prefer_hardware: bool) -> &'static [&'static str] {
    match (codec, prefer_hardware) {
        (Codec::H264, true) => &["h264_mf", "libx264", "h264"],
        (Codec::H264, false) => &["libx264", "h264", "h264_mf"],
        (Codec::Vp8, true) => &["vp8_mf", "libvpx", "libvpx-vp8"],
        (Codec::Vp8, false) => &["libvpx", "libvpx-vp8", "vp8_mf"],
        (Codec::Vp9, true) => &["vp9_mf", "libvpx-vp9"],
        (Codec::Vp9, false) => &["libvpx-vp9", "vp9_mf"],
        _ => &[],
    }
}

fn find_codec(name: &str) -> Option<*const ffi::AVCodec> {
    let name = CString::new(name).ok()?;
    let codec = unsafe { ffi::avcodec_find_encoder_by_name(name.as_ptr()) };
    (!codec.is_null()).then_some(codec)
}

/// A software encoder over BGRA CPU frames.
pub struct FfmpegEncoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    sws: *mut ffi::SwsContext,
    src_w: u32,
    src_h: u32,
    config: EncoderConfig,
    force_keyframe: bool,
    fallback: Option<String>,
}

// The context is only touched from the owning capture thread.
unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    /// Whether FFmpeg exposes an encoder for `codec`.
    pub fn available(codec: Codec) -> bool {
        codec_available(codec)
    }

    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        let mut errors = Vec::new();
        for name in codec_names(config.codec, config.hardware_encoding) {
            let Some(codec) = find_codec(name) else {
                errors.push(format!("{name}: encoder not available"));
                continue;
            };
            match Self::open(config.clone(), codec) {
                Ok(mut encoder) => {
                    tracing::info!(encoder = name, codec = ?config.codec, "using FFmpeg encoder");
                    if !errors.is_empty() {
                        tracing::warn!(encoder = name, "using fallback video encoder");
                        encoder.fallback = Some(format!(
                            "Video encoder fallback: using {name} for {:?}",
                            config.codec
                        ));
                    }
                    return Ok(encoder);
                }
                Err(error) => {
                    tracing::warn!(encoder = name, %error, "video encoder initialization failed");
                    errors.push(format!("{name}: {error}"));
                }
            }
        }
        Err(EncodeError::Unavailable(format!(
            "no usable {:?} encoder in FFmpeg{}",
            config.codec,
            if errors.is_empty() {
                String::new()
            } else {
                format!(": {}", errors.join("; "))
            }
        )))
    }

    /// Notice when an earlier available encoder failed to initialize.
    pub fn fallback_notice(&self) -> Option<&str> {
        self.fallback.as_deref()
    }

    fn open(config: EncoderConfig, codec: *const ffi::AVCodec) -> Result<Self, EncodeError> {
        let width = config.width.max(1);
        let height = config.height.max(1);
        let fps = config.fps.max(1);

        unsafe {
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(EncodeError::Ffmpeg("avcodec_alloc_context3 failed".into()));
            }
            (*ctx).width = width as i32;
            (*ctx).height = height as i32;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: fps as i32,
            };
            (*ctx).framerate = ffi::AVRational {
                num: fps as i32,
                den: 1,
            };
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*ctx).bit_rate = (config.bitrate_kbps.max(1) as i64) * 1000;
            (*ctx).gop_size = (fps * 2) as i32;
            (*ctx).max_b_frames = 0;

            // Repeat SPS/PPS in-band so the RTP payloader can decode without
            // out-of-band extradata.
            if config.codec == Codec::H264 && !(*ctx).priv_data.is_null() {
                let key = CString::new("x264-params").unwrap();
                let value = CString::new("repeat-headers=1").unwrap();
                let _ = ffi::av_opt_set((*ctx).priv_data, key.as_ptr(), value.as_ptr(), 0);
            }

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(EncodeError::Ffmpeg(format!(
                    "avcodec_open2({:?}) failed: {ret}",
                    config.codec
                )));
            }

            let frame = ffi::av_frame_alloc();
            if frame.is_null() {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
            }
            (*frame).width = width as i32;
            (*frame).height = height as i32;
            (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
            let ret = ffi::av_frame_get_buffer(frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { frame });
                ffi::avcodec_free_context(&mut { ctx });
                return Err(EncodeError::Ffmpeg(format!(
                    "{:?} frame buffer failed: {ret}",
                    config.codec
                )));
            }

            Ok(Self {
                ctx,
                frame,
                sws: ptr::null_mut(),
                src_w: width,
                src_h: height,
                config,
                force_keyframe: false,
                fallback: None,
            })
        }
    }

    /// Encode one BGRA frame. Returns zero packets when the encoder buffers.
    pub fn encode_frame(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        if frame.width == 0 || frame.height == 0 || frame.data.is_empty() {
            return Err(EncodeError::Ffmpeg("empty frame".into()));
        }
        if frame.stride < frame.width.saturating_mul(4) {
            return Err(EncodeError::Ffmpeg(format!(
                "frame stride {} is too small for {} pixels",
                frame.stride, frame.width
            )));
        }

        if frame.width != self.src_w || frame.height != self.src_h {
            self.src_w = frame.width;
            self.src_h = frame.height;
            unsafe {
                if !self.sws.is_null() {
                    ffi::sws_freeContext(self.sws);
                }
                self.sws = ptr::null_mut();
            }
        }

        let force = force_keyframe || self.force_keyframe;
        self.force_keyframe = false;

        unsafe {
            if self.sws.is_null() {
                self.sws = ffi::sws_getContext(
                    frame.width as i32,
                    frame.height as i32,
                    ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                    self.config.width as i32,
                    self.config.height as i32,
                    ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
                    crate::SWS_BILINEAR,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                );
                if self.sws.is_null() {
                    return Err(EncodeError::Ffmpeg("sws_getContext failed".into()));
                }
            }

            let ret = ffi::av_frame_make_writable(self.frame);
            if ret < 0 {
                return Err(EncodeError::Ffmpeg(format!(
                    "av_frame_make_writable failed: {ret}"
                )));
            }

            let src_data = [frame.data.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride = [frame.stride as i32, 0, 0, 0];
            let dst_data = [
                (*self.frame).data[0],
                (*self.frame).data[1],
                (*self.frame).data[2],
                (*self.frame).data[3],
            ];
            let dst_stride = [
                (*self.frame).linesize[0],
                (*self.frame).linesize[1],
                (*self.frame).linesize[2],
                (*self.frame).linesize[3],
            ];
            let ret = ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                frame.height as i32,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if ret < 0 {
                return Err(EncodeError::Ffmpeg(format!("sws_scale failed: {ret}")));
            }

            let fps = self.config.fps.max(1) as i64;
            (*self.frame).pts = frame.pts_micros as i64 * fps / 1_000_000;
            (*self.frame).pict_type = if force {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let ret = ffi::avcodec_send_frame(self.ctx, self.frame);
            if ret < 0 && ret != ffi::AVERROR(ffi::EAGAIN) {
                return Err(EncodeError::Ffmpeg(format!(
                    "avcodec_send_frame({:?}) failed: {ret}",
                    self.config.codec
                )));
            }

            let mut out = Vec::new();
            loop {
                let mut packet = ffi::av_packet_alloc();
                if packet.is_null() {
                    return Err(EncodeError::Ffmpeg("av_packet_alloc failed".into()));
                }
                let ret = ffi::avcodec_receive_packet(self.ctx, packet);
                if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                    ffi::av_packet_free(&mut packet);
                    break;
                }
                if ret < 0 {
                    ffi::av_packet_free(&mut packet);
                    return Err(EncodeError::Ffmpeg(format!(
                        "avcodec_receive_packet({:?}) failed: {ret}",
                        self.config.codec
                    )));
                }

                let bytes = std::slice::from_raw_parts((*packet).data, (*packet).size as usize);
                let data = bytes.to_vec();
                let keyframe = (*packet).flags & ffi::AV_PKT_FLAG_KEY != 0;
                let pts = (*packet).pts;
                let duration = (*packet).duration;
                ffi::av_packet_free(&mut packet);

                out.push(EncodedFrame {
                    data,
                    keyframe,
                    pts_micros: if pts >= 0 {
                        pts as u64 * 1_000_000 / fps as u64
                    } else {
                        frame.pts_micros
                    },
                    duration_micros: if duration > 0 {
                        duration as u64 * 1_000_000 / fps as u64
                    } else {
                        1_000_000 / fps as u64
                    },
                });
            }
            Ok(out)
        }
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn backend(&self) -> Backend {
        Backend::Software
    }

    fn encode(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        self.encode_frame(frame, force_keyframe)
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), EncodeError> {
        self.config.bitrate_kbps = bitrate_kbps;
        unsafe {
            (*self.ctx).bit_rate = (bitrate_kbps.max(1) as i64) * 1000;
        }
        Ok(())
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}
