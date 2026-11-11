//! RGB24 → JPEG encoder for the remote preview.

use std::ffi::CString;
use std::ptr;

use ffmpeg_sys_next as ffi;

/// Encodes RGB24 frames to JPEG.
pub struct JpegEncoder {
    codec: *const ffi::AVCodec,
    ctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
    sws: *mut ffi::SwsContext,
    sws_bgra: *mut ffi::SwsContext,
    width: u32,
    height: u32,
}

unsafe impl Send for JpegEncoder {}

impl JpegEncoder {
    pub fn new() -> Result<Self, String> {
        let name = CString::new("mjpeg").map_err(|e| e.to_string())?;
        let codec = unsafe { ffi::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err("mjpeg encoder not found".into());
        }
        unsafe {
            let frame = ffi::av_frame_alloc();
            if frame.is_null() {
                return Err("av_frame_alloc failed".into());
            }
            Ok(Self {
                codec,
                ctx: ptr::null_mut(),
                frame,
                sws: ptr::null_mut(),
                sws_bgra: ptr::null_mut(),
                width: 0,
                height: 0,
            })
        }
    }

    /// Open (or re-open) the codec for the current frame size.
    fn open(&mut self, width: u32, height: u32) -> Result<(), String> {
        unsafe {
            self.close();
            let ctx = ffi::avcodec_alloc_context3(self.codec);
            if ctx.is_null() {
                return Err("avcodec_alloc_context3 failed".into());
            }
            (*ctx).width = width as i32;
            (*ctx).height = height as i32;
            (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P;
            (*ctx).time_base = ffi::AVRational { num: 1, den: 30 };
            let ret = ffi::avcodec_open2(ctx, self.codec, ptr::null_mut());
            if ret < 0 {
                ffi::avcodec_free_context(&mut { ctx });
                return Err(format!("avcodec_open2(mjpeg) failed: {ret}"));
            }
            self.ctx = ctx;
            self.width = width;
            self.height = height;
            self.sws = ffi::sws_getContext(
                width as i32,
                height as i32,
                ffi::AVPixelFormat::AV_PIX_FMT_RGB24,
                width as i32,
                height as i32,
                ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P,
                ffi::SWS_BILINEAR,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if self.sws.is_null() {
                return Err("sws_getContext failed".into());
            }
            self.sws_bgra = ffi::sws_getContext(
                width as i32,
                height as i32,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                width as i32,
                height as i32,
                ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P,
                ffi::SWS_BILINEAR,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if self.sws_bgra.is_null() {
                return Err("sws_getContext(BGRA) failed".into());
            }
            (*self.frame).width = width as i32;
            (*self.frame).height = height as i32;
            (*self.frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
            let ret = ffi::av_frame_get_buffer(self.frame, 0);
            if ret < 0 {
                return Err(format!("av_frame_get_buffer failed: {ret}"));
            }
            Ok(())
        }
    }

    fn close(&mut self) {
        unsafe {
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
                self.sws = ptr::null_mut();
            }
            if !self.sws_bgra.is_null() {
                ffi::sws_freeContext(self.sws_bgra);
                self.sws_bgra = ptr::null_mut();
            }
            if !self.ctx.is_null() {
                ffi::avcodec_free_context(&mut self.ctx);
            }
        }
    }

    /// Encode one RGB24 frame. Returns a JPEG byte stream.
    pub fn encode(&mut self, rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
        if width == 0 || height == 0 || rgb.len() < width as usize * height as usize * 3 {
            return Err("empty RGB frame".into());
        }
        if width != self.width || height != self.height || self.ctx.is_null() {
            self.open(width, height)?;
        }
        unsafe {
            let ret = ffi::av_frame_make_writable(self.frame);
            if ret < 0 {
                return Err(format!("av_frame_make_writable failed: {ret}"));
            }
            let src_data = [rgb.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride = [width as i32 * 3, 0, 0, 0];
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
                height as i32,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if ret < 0 {
                return Err(format!("sws_scale failed: {ret}"));
            }

            let ret = ffi::avcodec_send_frame(self.ctx, self.frame);
            if ret < 0 && ret != ffi::AVERROR(ffi::EAGAIN) {
                return Err(format!("avcodec_send_frame failed: {ret}"));
            }
            let mut packet = ffi::av_packet_alloc();
            if packet.is_null() {
                return Err("av_packet_alloc failed".into());
            }
            let ret = ffi::avcodec_receive_packet(self.ctx, packet);
            if ret < 0 && ret != ffi::AVERROR(ffi::EAGAIN) && ret != ffi::AVERROR_EOF {
                ffi::av_packet_free(&mut packet);
                return Err(format!("avcodec_receive_packet failed: {ret}"));
            }
            let bytes = if ret >= 0 {
                let data = std::slice::from_raw_parts((*packet).data, (*packet).size as usize);
                data.to_vec()
            } else {
                Vec::new()
            };
            ffi::av_packet_free(&mut packet);
            Ok(bytes)
        }
    }

    /// Encode one BGRA frame. Returns a JPEG byte stream.
    pub fn encode_bgra(&mut self, bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
        if width == 0 || height == 0 || bgra.len() < width as usize * height as usize * 4 {
            return Err("empty BGRA frame".into());
        }
        if width != self.width || height != self.height || self.ctx.is_null() {
            self.open(width, height)?;
        }
        unsafe {
            let ret = ffi::av_frame_make_writable(self.frame);
            if ret < 0 {
                return Err(format!("av_frame_make_writable failed: {ret}"));
            }
            let src_data = [bgra.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride = [width as i32 * 4, 0, 0, 0];
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
                self.sws_bgra,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                height as i32,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if ret < 0 {
                return Err(format!("sws_scale(BGRA) failed: {ret}"));
            }

            let ret = ffi::avcodec_send_frame(self.ctx, self.frame);
            if ret < 0 && ret != ffi::AVERROR(ffi::EAGAIN) {
                return Err(format!("avcodec_send_frame failed: {ret}"));
            }
            let mut packet = ffi::av_packet_alloc();
            if packet.is_null() {
                return Err("av_packet_alloc failed".into());
            }
            let ret = ffi::avcodec_receive_packet(self.ctx, packet);
            if ret < 0 && ret != ffi::AVERROR(ffi::EAGAIN) && ret != ffi::AVERROR_EOF {
                ffi::av_packet_free(&mut packet);
                return Err(format!("avcodec_receive_packet failed: {ret}"));
            }
            let bytes = if ret >= 0 {
                let data = std::slice::from_raw_parts((*packet).data, (*packet).size as usize);
                data.to_vec()
            } else {
                Vec::new()
            };
            ffi::av_packet_free(&mut packet);
            Ok(bytes)
        }
    }
}

impl Drop for JpegEncoder {
    fn drop(&mut self) {
        self.close();
        unsafe {
            ffi::av_frame_free(&mut self.frame);
        }
    }
}
