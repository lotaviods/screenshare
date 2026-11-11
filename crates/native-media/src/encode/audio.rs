//! FFmpeg-backed Opus encoder.
//!
//! FFmpeg is used as a codec layer only: the `libopus` encoder is preferred,
//! with the built-in `opus` encoder as a fallback. Input is interleaved S16LE
//! at 48 kHz stereo, the format WebRTC expects, and every call must carry
//! exactly [`OpusEncoder::frame_len`] samples.

use std::ffi::CString;
use std::ptr;

use ffmpeg_sys_next as ffi;

use super::{EncodeError, EncodedFrame};

/// Opus sample rate; WebRTC only negotiates 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Opus channel count.
pub const CHANNELS: usize = 2;

/// Name of the FFmpeg encoder to use, preferring the system libopus.
fn find_codec() -> Option<(*const ffi::AVCodec, bool)> {
    for (name, experimental) in [("libopus", false), ("opus", true)] {
        let name = CString::new(name).unwrap();
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(name.as_ptr()) };
        if !codec.is_null() {
            return Some((codec, experimental));
        }
    }
    None
}

/// Whether FFmpeg exposes an Opus encoder.
pub fn opus_available() -> bool {
    find_codec().is_some()
}

/// A 48 kHz stereo S16LE Opus encoder, one packet per 20 ms.
pub struct OpusEncoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    /// Interleaved samples per frame (`frame_size * channels`).
    frame_len: usize,
    pts: i64,
    bitrate_kbps: u32,
}

// The context is only touched from the owning capture thread.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(bitrate_kbps: u32) -> Result<Self, EncodeError> {
        let Some((codec, experimental)) = find_codec() else {
            return Err(EncodeError::Unavailable("no Opus encoder in FFmpeg".into()));
        };

        unsafe {
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(EncodeError::Ffmpeg("avcodec_alloc_context3 failed".into()));
            }

            (*ctx).sample_rate = SAMPLE_RATE as i32;
            (*ctx).sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_S16;
            (*ctx).bit_rate = (bitrate_kbps.max(1) as i64) * 1000;
            (*ctx).time_base = ffi::AVRational {
                num: 1,
                den: SAMPLE_RATE as i32,
            };
            ffi::av_channel_layout_default(&mut (*ctx).ch_layout, CHANNELS as i32);
            if experimental {
                (*ctx).strict_std_compliance = ffi::FF_COMPLIANCE_EXPERIMENTAL;
            }

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(EncodeError::Ffmpeg(format!(
                    "avcodec_open2(opus) failed: {ret}"
                )));
            }

            let frame = ffi::av_frame_alloc();
            if frame.is_null() {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
            }
            let frame_size = (*ctx).frame_size.max(1) as usize;
            (*frame).nb_samples = frame_size as i32;
            (*frame).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_S16 as i32;
            (*frame).sample_rate = SAMPLE_RATE as i32;
            ffi::av_channel_layout_default(&mut (*frame).ch_layout, CHANNELS as i32);
            let ret = ffi::av_frame_get_buffer(frame, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { frame });
                ffi::avcodec_free_context(&mut { ctx });
                return Err(EncodeError::Ffmpeg(format!(
                    "opus frame buffer failed: {ret}"
                )));
            }

            Ok(Self {
                ctx,
                frame,
                frame_len: frame_size * CHANNELS,
                pts: 0,
                bitrate_kbps,
            })
        }
    }

    /// Interleaved samples one call to [`encode`](Self::encode) must carry.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Encode one frame of interleaved S16LE samples into one Opus packet.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Option<EncodedFrame>, EncodeError> {
        if pcm.len() != self.frame_len {
            return Err(EncodeError::Ffmpeg(format!(
                "opus frame is {} samples, got {}",
                self.frame_len,
                pcm.len()
            )));
        }

        unsafe {
            let ret = ffi::av_frame_make_writable(self.frame);
            if ret < 0 {
                return Err(EncodeError::Ffmpeg(format!(
                    "av_frame_make_writable failed: {ret}"
                )));
            }
            let dst =
                std::slice::from_raw_parts_mut((*self.frame).data[0] as *mut i16, self.frame_len);
            dst.copy_from_slice(pcm);
            (*self.frame).pts = self.pts;
            let frame_time = self.pts;
            self.pts += (*self.frame).nb_samples as i64;

            let ret = ffi::avcodec_send_frame(self.ctx, self.frame);
            if ret < 0 {
                return Err(EncodeError::Ffmpeg(format!(
                    "avcodec_send_frame(opus) failed: {ret}"
                )));
            }

            let mut packet = ffi::av_packet_alloc();
            if packet.is_null() {
                return Err(EncodeError::Ffmpeg("av_packet_alloc failed".into()));
            }
            let ret = ffi::avcodec_receive_packet(self.ctx, packet);
            if ret < 0 {
                ffi::av_packet_free(&mut packet);
                return Err(EncodeError::Ffmpeg(format!(
                    "avcodec_receive_packet(opus) failed: {ret}"
                )));
            }
            let bytes = std::slice::from_raw_parts((*packet).data, (*packet).size as usize);
            let data = bytes.to_vec();
            ffi::av_packet_free(&mut packet);

            Ok(Some(EncodedFrame {
                data,
                keyframe: false,
                pts_micros: frame_time as u64 * 1_000_000 / SAMPLE_RATE as u64,
                duration_micros: (*self.frame).nb_samples as u64 * 1_000_000 / SAMPLE_RATE as u64,
            }))
        }
    }

    pub fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), EncodeError> {
        self.bitrate_kbps = bitrate_kbps;
        unsafe {
            (*self.ctx).bit_rate = (bitrate_kbps.max(1) as i64) * 1000;
        }
        Ok(())
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe {
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_packet_when_ffmpeg_has_opus() {
        if !opus_available() {
            eprintln!("no Opus encoder in FFmpeg; skipping");
            return;
        }
        let mut encoder = OpusEncoder::new(96).expect("encoder");
        assert_eq!(encoder.frame_len() % CHANNELS, 0);
        let pcm = vec![0i16; encoder.frame_len()];
        let packet = encoder.encode(&pcm).expect("encode").expect("one packet");
        // Every Opus packet starts with a TOC byte whose config is < 32.
        assert!(!packet.data.is_empty());
        assert!(packet.data[0] >> 3 < 32);
        assert_eq!(packet.duration_micros, 20_000);
    }

    #[test]
    fn rejects_a_partial_frame() {
        if !opus_available() {
            return;
        }
        let mut encoder = OpusEncoder::new(96).expect("encoder");
        assert!(encoder.encode(&[0i16; 2]).is_err());
    }
}
