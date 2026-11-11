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
                        ffi::SWS_BILINEAR,
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

/// Decodes Opus packets into interleaved 16-bit PCM.
pub struct OpusDecoder {
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
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
            Ok(Self { ctx, frame })
        }
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
                let channels = (*self.frame).ch_layout.nb_channels as usize;
                if samples == 0 || channels == 0 {
                    continue;
                }
                let data = (*self.frame).data[0] as *const i16;
                if data.is_null() {
                    continue;
                }
                out.reserve(samples * channels);
                for i in 0..samples * channels {
                    out.push(*data.add(i));
                }
            }
            Ok(out)
        }
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe {
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
        }
    }
}
