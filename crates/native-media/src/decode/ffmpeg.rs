//! FFmpeg decoders for remote video and audio.

use std::ffi::CString;
use std::ptr;

use ffmpeg_sys_next as ffi;

/// Wrap a byte slice in a borrowed `AVPacket` and feed it to the decoder.
fn send_packet(ctx: *mut ffi::AVCodecContext, packet: &[u8]) -> Result<(), String> {
    unsafe {
        let mut pkt = ffi::av_packet_alloc();
        if pkt.is_null() {
            return Err("av_packet_alloc failed".into());
        }
        (*pkt).data = packet.as_ptr() as *mut u8;
        (*pkt).size = packet.len() as i32;
        let ret = ffi::avcodec_send_packet(ctx, pkt);
        ffi::av_packet_free(&mut pkt);
        if ret < 0 && ret != ffi::AVERROR(ffi::EAGAIN) {
            return Err(format!("avcodec_send_packet failed: {ret}"));
        }
        Ok(())
    }
}

/// Decodes H.264/VP8/VP9 access units into RGB24 frames.
pub struct VideoDecoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    sws: *mut ffi::SwsContext,
    width: u32,
    height: u32,
}

unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    pub fn new(codec: &str) -> Result<Self, String> {
        let name = CString::new(codec).map_err(|e| e.to_string())?;
        let codec = unsafe { ffi::avcodec_find_decoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(format!("decoder not found: {codec:?}"));
        }
        unsafe {
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err("avcodec_alloc_context3 failed".into());
            }
            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(format!("avcodec_open2({codec:?}) failed: {ret}"));
            }
            let frame = ffi::av_frame_alloc();
            if frame.is_null() {
                ffi::avcodec_free_context(&mut { ctx });
                return Err("av_frame_alloc failed".into());
            }
            Ok(Self {
                ctx,
                frame,
                sws: ptr::null_mut(),
                width: 0,
                height: 0,
            })
        }
    }

    /// Decode one packet. Returns RGB24 frames, one per decoded picture.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        if packet.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            send_packet(self.ctx, packet)?;
            let mut out = Vec::new();
            loop {
                let ret = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    return Err(format!("avcodec_receive_frame failed: {ret}"));
                }
                let width = (*self.frame).width as u32;
                let height = (*self.frame).height as u32;
                if width == 0 || height == 0 {
                    continue;
                }
                if width != self.width || height != self.height {
                    self.width = width;
                    self.height = height;
                    if !self.sws.is_null() {
                        ffi::sws_freeContext(self.sws);
                    }
                    self.sws = ffi::sws_getContext(
                        width as i32,
                        height as i32,
                        std::mem::transmute::<i32, ffi::AVPixelFormat>((*self.frame).format),
                        width as i32,
                        height as i32,
                        ffi::AVPixelFormat::AV_PIX_FMT_RGB24,
                        crate::SWS_BILINEAR,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                    );
                    if self.sws.is_null() {
                        return Err("sws_getContext failed".into());
                    }
                }
                let mut rgb = vec![0u8; width as usize * height as usize * 3];
                let src_data = [
                    (*self.frame).data[0] as *const u8,
                    (*self.frame).data[1] as *const u8,
                    (*self.frame).data[2] as *const u8,
                    (*self.frame).data[3] as *const u8,
                ];
                let src_stride = [
                    (*self.frame).linesize[0],
                    (*self.frame).linesize[1],
                    (*self.frame).linesize[2],
                    (*self.frame).linesize[3],
                ];
                let dst_data = [
                    rgb.as_mut_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ];
                let dst_stride = [width as i32 * 3, 0, 0, 0];
                let ret = ffi::sws_scale(
                    self.sws,
                    src_data.as_ptr(),
                    src_stride.as_ptr(),
                    0,
                    height as i32,
                    dst_data.as_ptr(),
                    dst_stride.as_ptr(),
                );
                if ret < 0 {
                    return Err(format!("sws_scale failed: {ret}"));
                }
                out.push(rgb);
            }
            Ok(out)
        }
    }

    /// Decoded width of the last frame.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Decoded height of the last frame.
    pub fn height(&self) -> u32 {
        self.height
    }
}

impl Drop for VideoDecoder {
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

/// Decoded Opus output format: interleaved S16 at 48 kHz stereo.
const OPUS_OUT_RATE: i32 = 48_000;
const OPUS_OUT_CHANNELS: i32 = 2;

/// Decodes Opus packets into interleaved 16-bit PCM.
///
/// FFmpeg's native `opus` decoder emits planar float (`FLTP`), not S16.
/// `swresample` converts whatever the decoder actually produces (format, rate
/// and channel layout included) into the interleaved S16 the playback path
/// expects.
pub struct OpusDecoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    swr: *mut ffi::SwrContext,
    /// Input parameters the current `swr` was configured for.
    swr_format: ffi::AVSampleFormat,
    swr_rate: i32,
    swr_layout: ffi::AVChannelLayout,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, String> {
        let name = CString::new("opus").map_err(|e| e.to_string())?;
        let codec = unsafe { ffi::avcodec_find_decoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err("opus decoder not found".into());
        }
        unsafe {
            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err("avcodec_alloc_context3 failed".into());
            }
            // The native decoder may still choose FLTP; the resampler below is
            // the real format boundary and handles either case.
            (*ctx).request_sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_S16;
            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(format!("avcodec_open2(opus) failed: {ret}"));
            }
            let frame = ffi::av_frame_alloc();
            if frame.is_null() {
                ffi::avcodec_free_context(&mut { ctx });
                return Err("av_frame_alloc failed".into());
            }
            Ok(Self {
                ctx,
                frame,
                swr: ptr::null_mut(),
                swr_format: ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE,
                swr_rate: 0,
                swr_layout: std::mem::zeroed(),
            })
        }
    }

    /// (Re)build the resampler when the decoder's real output parameters change.
    unsafe fn ensure_resampler(&mut self, frame: *mut ffi::AVFrame) -> Result<(), String> {
        let format = std::mem::transmute::<i32, ffi::AVSampleFormat>((*frame).format);
        let rate = if (*frame).sample_rate > 0 {
            (*frame).sample_rate
        } else {
            OPUS_OUT_RATE
        };

        // A malformed layout is unusual, but fall back to the negotiated stereo
        // layout rather than handing an invalid descriptor to swresample.
        let frame_layout = &(*frame).ch_layout;
        let mut fallback_layout: ffi::AVChannelLayout = std::mem::zeroed();
        let in_layout =
            if frame_layout.nb_channels > 0 && ffi::av_channel_layout_check(frame_layout) != 0 {
                frame_layout as *const ffi::AVChannelLayout
            } else {
                ffi::av_channel_layout_default(&mut fallback_layout, OPUS_OUT_CHANNELS);
                &fallback_layout
            };

        let unchanged = !self.swr.is_null()
            && self.swr_format == format
            && self.swr_rate == rate
            && ffi::av_channel_layout_compare(&self.swr_layout, in_layout) == 0;
        if unchanged {
            return Ok(());
        }

        if !self.swr.is_null() {
            ffi::swr_free(&mut self.swr);
        }
        ffi::av_channel_layout_uninit(&mut self.swr_layout);

        let mut out_layout: ffi::AVChannelLayout = std::mem::zeroed();
        ffi::av_channel_layout_default(&mut out_layout, OPUS_OUT_CHANNELS);
        let mut swr: *mut ffi::SwrContext = ptr::null_mut();
        let ret = ffi::swr_alloc_set_opts2(
            &mut swr,
            &out_layout,
            ffi::AVSampleFormat::AV_SAMPLE_FMT_S16,
            OPUS_OUT_RATE,
            in_layout,
            format,
            rate,
            0,
            ptr::null_mut(),
        );
        ffi::av_channel_layout_uninit(&mut out_layout);
        if ret < 0 || swr.is_null() {
            if !swr.is_null() {
                ffi::swr_free(&mut swr);
            }
            return Err(format!("swr_alloc_set_opts2 failed: {ret}"));
        }
        let ret = ffi::swr_init(swr);
        if ret < 0 {
            ffi::swr_free(&mut swr);
            return Err(format!("swr_init failed: {ret}"));
        }

        let mut layout: ffi::AVChannelLayout = std::mem::zeroed();
        let ret = ffi::av_channel_layout_copy(&mut layout, in_layout);
        if ret < 0 {
            ffi::swr_free(&mut swr);
            return Err(format!("av_channel_layout_copy failed: {ret}"));
        }

        self.swr = swr;
        self.swr_format = format;
        self.swr_rate = rate;
        self.swr_layout = layout;
        Ok(())
    }

    /// Decode one Opus packet. Returns interleaved 16-bit PCM.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, String> {
        if packet.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            send_packet(self.ctx, packet)?;
            let mut out = Vec::new();
            loop {
                let ret = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if ret == ffi::AVERROR(ffi::EAGAIN) || ret == ffi::AVERROR_EOF {
                    break;
                }
                if ret < 0 {
                    return Err(format!("avcodec_receive_frame failed: {ret}"));
                }
                let samples = (*self.frame).nb_samples as usize;
                if samples == 0 {
                    continue;
                }
                self.ensure_resampler(self.frame)?;

                let max_out = ffi::swr_get_out_samples(self.swr, samples as i32);
                let max_out = if max_out > 0 {
                    max_out as usize
                } else {
                    samples.saturating_add(64)
                };
                let channels = OPUS_OUT_CHANNELS as usize;
                let mut pcm = vec![0i16; max_out.saturating_mul(channels)];
                let out_data = [
                    pcm.as_mut_ptr() as *mut u8,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ];
                let in_data = if (*self.frame).extended_data.is_null() {
                    (*self.frame).data.as_ptr() as *const *const u8
                } else {
                    (*self.frame).extended_data as *const *const u8
                };
                let converted = ffi::swr_convert(
                    self.swr,
                    out_data.as_ptr(),
                    max_out as i32,
                    in_data,
                    samples as i32,
                );
                if converted < 0 {
                    return Err(format!("swr_convert failed: {converted}"));
                }
                let converted = converted as usize;
                out.extend_from_slice(&pcm[..converted.saturating_mul(channels)]);
            }
            Ok(out)
        }
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.swr.is_null() {
                ffi::swr_free(&mut self.swr);
            }
            ffi::av_channel_layout_uninit(&mut self.swr_layout);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}
