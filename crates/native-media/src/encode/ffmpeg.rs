//! FFmpeg-backed VAAPI encoder.
//!
//! FFmpeg is used as a thin codec layer, not as a media framework: only
//! `libavcodec`/`libavutil` plus libavfilter's `scale_vaapi` VPP filter are
//! linked. DMA-BUF frames are imported straight into VAAPI surfaces, converted
//! to NV12 on the GPU when the compositor exports RGB, and encoded without a
//! CPU round trip.

use std::ffi::CString;
use std::ptr;

use ffmpeg_sys_next as ffi;
use sharescreen_core::Codec;

use super::{Backend, EncodeError, EncodedFrame, EncoderConfig, VideoEncoder};
use crate::frame::{DrmFormat, Frame};

const AV_HWFRAME_MAP_READ: i32 = 1;

/// Raise FFmpeg's log level. Useful while validating a new GPU path.
pub fn enable_debug_logging() {
    unsafe { ffi::av_log_set_level(ffi::AV_LOG_DEBUG) }
}

/// Probe whether FFmpeg exposes a VAAPI encoder for `codec`.
pub fn vaapi_available(codec: Codec) -> bool {
    let Some(name) = codec_name(codec) else {
        return false;
    };
    let name = CString::new(name).unwrap();
    unsafe { !ffi::avcodec_find_encoder_by_name(name.as_ptr()).is_null() }
}

/// VAAPI encoder name for a supported video codec.
fn codec_name(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264 => Some("h264_vaapi"),
        Codec::Vp8 => Some("vp8_vaapi"),
        Codec::Vp9 => Some("vp9_vaapi"),
        _ => None,
    }
}

/// FFmpeg codec id for a supported video codec.
fn codec_id(codec: Codec) -> Option<ffi::AVCodecID> {
    match codec {
        Codec::H264 => Some(ffi::AVCodecID::AV_CODEC_ID_H264),
        Codec::Vp8 => Some(ffi::AVCodecID::AV_CODEC_ID_VP8),
        Codec::Vp9 => Some(ffi::AVCodecID::AV_CODEC_ID_VP9),
        _ => None,
    }
}

/// VAAPI import format matching each DRM fourcc.
fn vaapi_sw_format(format: DrmFormat) -> Option<ffi::AVPixelFormat> {
    use ffi::AVPixelFormat as P;
    Some(match format {
        DrmFormat::Nv12 => P::AV_PIX_FMT_NV12,
        DrmFormat::Xrgb8888 => P::AV_PIX_FMT_BGR0,
        DrmFormat::Argb8888 => P::AV_PIX_FMT_BGRA,
        DrmFormat::Xbgr8888 => P::AV_PIX_FMT_RGB0,
        DrmFormat::Abgr8888 => P::AV_PIX_FMT_RGBA,
        DrmFormat::Rgba8888 => P::AV_PIX_FMT_ABGR,
        DrmFormat::Bgrx8888 => P::AV_PIX_FMT_0RGB,
        DrmFormat::Bgra8888 => P::AV_PIX_FMT_ARGB,
        DrmFormat::Rgbx8888 => P::AV_PIX_FMT_0BGR,
    })
}

/// Frees a DRM frame descriptor once every frame references are gone.
unsafe extern "C" fn free_drm_desc(_opaque: *mut libc::c_void, data: *mut u8) {
    let desc = data as *mut ffi::AVDRMFrameDescriptor;
    if desc.is_null() {
        return;
    }
    for i in 0..(*desc).nb_objects as usize {
        libc::close((*desc).objects[i].fd);
    }
    ffi::av_free(desc.cast());
}

/// RGB -> NV12 conversion done by VAAPI VPP.
struct Converter {
    sw_format: i32,
    input_frames: *mut ffi::AVBufferRef,
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
}

unsafe impl Send for Converter {}

impl Converter {
    unsafe fn new(
        device: *mut ffi::AVBufferRef,
        config: &EncoderConfig,
        sw_format: ffi::AVPixelFormat,
    ) -> Result<Self, EncodeError> {
        let input_frames = ffi::av_hwframe_ctx_alloc(device);
        if input_frames.is_null() {
            return Err(EncodeError::Ffmpeg(
                "av_hwframe_ctx_alloc(input) failed".into(),
            ));
        }
        {
            let hw = (*input_frames).data as *mut ffi::AVHWFramesContext;
            (*hw).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*hw).sw_format = sw_format;
            (*hw).width = config.width as i32;
            (*hw).height = config.height as i32;
            (*hw).initial_pool_size = 0;
        }
        let ret = ffi::av_hwframe_ctx_init(input_frames);
        if ret < 0 {
            ffi::av_buffer_unref(&mut { input_frames });
            return Err(EncodeError::Ffmpeg(format!(
                "av_hwframe_ctx_init(input) failed: {ret}"
            )));
        }

        let graph = ffi::avfilter_graph_alloc();
        if graph.is_null() {
            ffi::av_buffer_unref(&mut { input_frames });
            return Err(EncodeError::Ffmpeg("avfilter_graph_alloc failed".into()));
        }

        // The buffer source must be allocated, configured with its hw frames
        // context, and only then initialized; avfilter_graph_create_filter
        // would initialize it with no pixel format.
        let buffer = ffi::avfilter_get_by_name(c"buffer".as_ptr());
        let src = if buffer.is_null() {
            ptr::null_mut()
        } else {
            ffi::avfilter_graph_alloc_filter(graph, buffer, c"in".as_ptr())
        };
        if src.is_null() {
            ffi::avfilter_graph_free(&mut { graph });
            ffi::av_buffer_unref(&mut { input_frames });
            return Err(EncodeError::Ffmpeg("cannot create filter source".into()));
        }

        let ret = (|| {
            let mut params: ffi::AVBufferSrcParameters = std::mem::zeroed();
            params.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
            params.width = config.width as i32;
            params.height = config.height as i32;
            params.time_base = ffi::AVRational {
                num: 1,
                den: config.fps.max(1) as i32,
            };
            params.frame_rate = ffi::AVRational {
                num: config.fps.max(1) as i32,
                den: 1,
            };
            params.hw_frames_ctx = ffi::av_buffer_ref(input_frames);
            let ret = ffi::av_buffersrc_parameters_set(src, &mut params);
            if !params.hw_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut params.hw_frames_ctx);
            }
            if ret < 0 {
                return Err(ret);
            }
            let ret = ffi::avfilter_init_str(src, ptr::null());
            if ret < 0 {
                return Err(ret);
            }

            let mut scale: *mut ffi::AVFilterContext = ptr::null_mut();
            let mut sink: *mut ffi::AVFilterContext = ptr::null_mut();
            Self::create_filter(
                &mut scale,
                "scale_vaapi",
                "convert",
                c"w=iw:h=ih:format=nv12".as_ptr(),
                graph,
            )?;
            Self::create_filter(&mut sink, "buffersink", "out", ptr::null(), graph)?;

            let ret = ffi::avfilter_link(src, 0, scale, 0);
            if ret < 0 {
                return Err(ret);
            }
            let ret = ffi::avfilter_link(scale, 0, sink, 0);
            if ret < 0 {
                return Err(ret);
            }
            let ret = ffi::avfilter_graph_config(graph, ptr::null_mut());
            if ret < 0 {
                return Err(ret);
            }
            Ok((scale, sink))
        })();

        let (_, sink) = match ret {
            Ok((scale, sink)) => (scale, sink),
            Err(ret) => {
                ffi::avfilter_graph_free(&mut { graph });
                ffi::av_buffer_unref(&mut { input_frames });
                return Err(EncodeError::Ffmpeg(format!(
                    "VAAPI VPP graph setup failed: {ret}"
                )));
            }
        };

        if let Err(ret) = ret {
            ffi::avfilter_graph_free(&mut { graph });
            ffi::av_buffer_unref(&mut { input_frames });
            return Err(EncodeError::Ffmpeg(format!(
                "VAAPI VPP graph setup failed: {ret}"
            )));
        }

        Ok(Self {
            sw_format: sw_format as i32,
            input_frames,
            graph,
            src,
            sink,
        })
    }

    unsafe fn create_filter(
        out: &mut *mut ffi::AVFilterContext,
        name: &str,
        instance: &str,
        args: *const std::os::raw::c_char,
        graph: *mut ffi::AVFilterGraph,
    ) -> Result<(), i32> {
        let filter_name = CString::new(name).map_err(|_| ffi::AVERROR(libc::EINVAL))?;
        let instance = CString::new(instance).map_err(|_| ffi::AVERROR(libc::EINVAL))?;
        let filter = ffi::avfilter_get_by_name(filter_name.as_ptr());
        if filter.is_null() {
            return Err(ffi::AVERROR(libc::ENOENT));
        }
        let ret = ffi::avfilter_graph_create_filter(
            out,
            filter,
            instance.as_ptr(),
            args,
            ptr::null_mut(),
            graph,
        );
        if ret < 0 {
            return Err(ret);
        }
        Ok(())
    }
}

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe {
            ffi::avfilter_graph_free(&mut self.graph);
            ffi::av_buffer_unref(&mut self.input_frames);
        }
    }
}

pub struct FfmpegEncoder {
    ctx: *mut ffi::AVCodecContext,
    frames: *mut ffi::AVBufferRef,
    device: *mut ffi::AVBufferRef,
    converter: Option<Converter>,
    config: EncoderConfig,
    force_keyframe: bool,
    preview_sws: *mut ffi::SwsContext,
    preview_sw_format: ffi::AVPixelFormat,
    jpeg: Option<crate::decode::JpegEncoder>,
}

unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        unsafe { Self::new_vaapi(config) }
    }

    unsafe fn new_vaapi(config: EncoderConfig) -> Result<Self, EncodeError> {
        let name = codec_name(config.codec).ok_or_else(|| {
            EncodeError::Unavailable(format!("no VAAPI encoder for {:?}", config.codec))
        })?;
        let name = CString::new(name).unwrap();
        let codec = ffi::avcodec_find_encoder_by_name(name.as_ptr());
        if codec.is_null() {
            return Err(EncodeError::Unavailable(format!("{name:?} not built in")));
        }

        let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
        let ret = ffi::av_hwdevice_ctx_create(
            &mut device,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            ptr::null(),
            ptr::null_mut(),
            0,
        );
        if ret < 0 {
            return Err(EncodeError::Unavailable(format!(
                "av_hwdevice_ctx_create(vaapi) failed: {ret}"
            )));
        }

        let frames = ffi::av_hwframe_ctx_alloc(device);
        if frames.is_null() {
            ffi::av_buffer_unref(&mut { device });
            return Err(EncodeError::Ffmpeg("av_hwframe_ctx_alloc failed".into()));
        }
        {
            let hw = (*frames).data as *mut ffi::AVHWFramesContext;
            (*hw).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*hw).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*hw).width = config.width as i32;
            (*hw).height = config.height as i32;
            (*hw).initial_pool_size = 0;
        }
        let ret = ffi::av_hwframe_ctx_init(frames);
        if ret < 0 {
            ffi::av_buffer_unref(&mut { frames });
            ffi::av_buffer_unref(&mut { device });
            return Err(EncodeError::Ffmpeg(format!(
                "av_hwframe_ctx_init failed: {ret}"
            )));
        }

        let ctx = ffi::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            ffi::av_buffer_unref(&mut { frames });
            ffi::av_buffer_unref(&mut { device });
            return Err(EncodeError::Ffmpeg("avcodec_alloc_context3 failed".into()));
        }

        (*ctx).codec_id = codec_id(config.codec).ok_or_else(|| {
            EncodeError::Unavailable(format!("no VAAPI encoder for {:?}", config.codec))
        })?;
        (*ctx).width = config.width as i32;
        (*ctx).height = config.height as i32;
        (*ctx).time_base = ffi::AVRational {
            num: 1,
            den: config.fps.max(1) as i32,
        };
        (*ctx).framerate = ffi::AVRational {
            num: config.fps.max(1) as i32,
            den: 1,
        };
        (*ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*ctx).bit_rate = (config.bitrate_kbps as i64) * 1000;
        (*ctx).rc_max_rate = (config.bitrate_kbps as i64) * 1000;
        (*ctx).rc_buffer_size = (config.bitrate_kbps as i32) * 1000;
        (*ctx).gop_size = (config.fps.max(1) * 2) as i32;
        (*ctx).max_b_frames = 0;
        (*ctx).hw_frames_ctx = ffi::av_buffer_ref(frames);

        let mut opts: *mut ffi::AVDictionary = ptr::null_mut();
        let key = CString::new("rc_mode").unwrap();
        let value = CString::new("CBR").unwrap();
        ffi::av_dict_set(&mut opts, key.as_ptr(), value.as_ptr(), 0);

        let ret = ffi::avcodec_open2(ctx, codec, &mut opts);
        ffi::av_dict_free(&mut opts);
        if ret < 0 {
            ffi::avcodec_free_context(&mut { ctx });
            ffi::av_buffer_unref(&mut { frames });
            ffi::av_buffer_unref(&mut { device });
            return Err(EncodeError::Ffmpeg(format!("avcodec_open2 failed: {ret}")));
        }

        Ok(Self {
            ctx,
            frames,
            device,
            converter: None,
            config,
            force_keyframe: true,
            preview_sws: ptr::null_mut(),
            preview_sw_format: ffi::AVPixelFormat::AV_PIX_FMT_NONE,
            jpeg: Some(crate::decode::JpegEncoder::new().map_err(EncodeError::Ffmpeg)?),
        })
    }

    /// Import one DMA-BUF frame into a VAAPI surface from `frames`.
    unsafe fn import_drm(
        &self,
        frames: *mut ffi::AVBufferRef,
        frame: &Frame,
    ) -> Result<*mut ffi::AVFrame, EncodeError> {
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(EncodeError::Ffmpeg(format!(
                "frame size {}x{} does not match encoder {}x{}",
                frame.width, frame.height, self.config.width, self.config.height
            )));
        }
        if frame.objects.is_empty() || frame.layers.is_empty() {
            return Err(EncodeError::Ffmpeg("empty dmabuf frame".into()));
        }
        let layer = &frame.layers[0];
        if layer.planes.is_empty() || layer.planes.len() > 4 || frame.objects.len() > 4 {
            return Err(EncodeError::Ffmpeg("unsupported plane count".into()));
        }

        // The DRM frame must own its descriptor through `buf[0]`: av_hwframe_map
        // refs the source frame, and a frame without buffers sends
        // av_frame_ref down the CPU-allocation path, which fails with EINVAL.
        let desc_size = std::mem::size_of::<ffi::AVDRMFrameDescriptor>();
        let desc_ptr = ffi::av_mallocz(desc_size) as *mut ffi::AVDRMFrameDescriptor;
        if desc_ptr.is_null() {
            return Err(EncodeError::Ffmpeg(
                "av_mallocz(drm descriptor) failed".into(),
            ));
        }
        let desc = &mut *desc_ptr;
        desc.nb_objects = frame.objects.len() as i32;
        for (i, object) in frame.objects.iter().enumerate() {
            let fd = libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&object.fd),
                libc::F_DUPFD_CLOEXEC,
                0,
            );
            if fd < 0 {
                desc.nb_objects = i as i32;
                free_drm_desc(ptr::null_mut(), desc_ptr.cast());
                return Err(EncodeError::Ffmpeg("dup(dmabuf fd) failed".into()));
            }
            desc.objects[i].fd = fd;
            desc.objects[i].size = object.size as usize;
            desc.objects[i].format_modifier = frame.modifier;
        }
        desc.nb_layers = 1;
        desc.layers[0].format = layer.format.fourcc();
        desc.layers[0].nb_planes = layer.planes.len() as i32;
        for (i, plane) in layer.planes.iter().enumerate() {
            desc.layers[0].planes[i].object_index = plane.object_index as i32;
            desc.layers[0].planes[i].offset = plane.offset as isize;
            desc.layers[0].planes[i].pitch = plane.pitch as isize;
        }

        let mut drm = ffi::av_frame_alloc();
        if drm.is_null() {
            free_drm_desc(ptr::null_mut(), desc_ptr.cast());
            return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
        }
        (*drm).buf[0] = ffi::av_buffer_create(
            desc_ptr.cast(),
            desc_size,
            Some(free_drm_desc),
            ptr::null_mut(),
            0,
        );
        if (*drm).buf[0].is_null() {
            free_drm_desc(ptr::null_mut(), desc_ptr.cast());
            ffi::av_frame_free(&mut drm);
            return Err(EncodeError::Ffmpeg("av_buffer_create(drm) failed".into()));
        }
        (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*drm).width = frame.width as i32;
        (*drm).height = frame.height as i32;
        (*drm).data[0] = desc_ptr.cast();

        // av_hwframe_map fills a bare destination frame: allocating a surface
        // first makes the VAAPI import reject the mapping.
        let mut hw = ffi::av_frame_alloc();
        if hw.is_null() {
            ffi::av_frame_free(&mut drm);
            return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
        }
        (*hw).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*hw).width = frame.width as i32;
        (*hw).height = frame.height as i32;
        (*hw).hw_frames_ctx = ffi::av_buffer_ref(frames);
        if (*hw).hw_frames_ctx.is_null() {
            ffi::av_frame_free(&mut drm);
            ffi::av_frame_free(&mut hw);
            return Err(EncodeError::Ffmpeg("av_buffer_ref(frames) failed".into()));
        }
        let ret = ffi::av_hwframe_map(hw, drm, AV_HWFRAME_MAP_READ);
        ffi::av_frame_free(&mut drm);
        if ret < 0 {
            ffi::av_frame_free(&mut hw);
            return Err(EncodeError::Ffmpeg(format!(
                "av_hwframe_map(drm->vaapi) failed: {ret} (layer {:#x}, {} object(s), {} plane(s))",
                layer.format.fourcc(),
                frame.objects.len(),
                layer.planes.len()
            )));
        }

        (*hw).pts = (frame.pts_micros as i64) * (self.config.fps.max(1) as i64) / 1_000_000;
        Ok(hw)
    }

    /// Run RGB through VAAPI VPP so the encoder gets NV12 surfaces.
    fn encode_rgb(
        &mut self,
        frame: &Frame,
        format: DrmFormat,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        let Some(sw_format) = vaapi_sw_format(format) else {
            return Err(EncodeError::UnsupportedFormat(Some(format)));
        };

        if self.converter.as_ref().map(|c| c.sw_format) != Some(sw_format as i32) {
            self.converter = Some(unsafe { Converter::new(self.device, &self.config, sw_format)? });
        }
        let converter = self.converter.as_ref().expect("converter created above");
        let (src, sink, input_frames) = (converter.src, converter.sink, converter.input_frames);

        unsafe {
            let hw = self.import_drm(input_frames, frame)?;
            let ret = ffi::av_buffersrc_add_frame_flags(src, hw, 0);
            if ret < 0 {
                ffi::av_frame_free(&mut { hw });
                return Err(EncodeError::Ffmpeg(format!(
                    "av_buffersrc_add_frame failed: {ret}"
                )));
            }

            let mut converted = ffi::av_frame_alloc();
            if converted.is_null() {
                return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
            }
            let ret = ffi::av_buffersink_get_frame(sink, converted);
            if ret < 0 {
                ffi::av_frame_free(&mut converted);
                return Err(EncodeError::Ffmpeg(format!(
                    "VAAPI VPP produced no frame: {ret}"
                )));
            }
            self.receive_with_keyframe(converted, force_keyframe)
        }
    }

    fn encode_nv12(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        if !frame.is_nv12() {
            return Err(EncodeError::UnsupportedFormat(frame.format()));
        }
        let hw = unsafe { self.import_drm(self.frames, frame)? };
        unsafe { self.receive_with_keyframe(hw, force_keyframe) }
    }

    /// Encode one frame as a JPEG preview for the local self-view.
    pub fn preview(&mut self, frame: &Frame) -> Result<Vec<u8>, EncodeError> {
        let format = frame.format().ok_or(EncodeError::UnsupportedFormat(None))?;
        let (sw_format, input_frames) = if format.is_nv12() {
            (ffi::AVPixelFormat::AV_PIX_FMT_NV12, self.frames)
        } else {
            let sw = vaapi_sw_format(format).ok_or(EncodeError::UnsupportedFormat(Some(format)))?;
            if self.converter.as_ref().map(|c| c.sw_format) != Some(sw as i32) {
                self.converter = Some(unsafe { Converter::new(self.device, &self.config, sw)? });
            }
            (
                sw,
                self.converter
                    .as_ref()
                    .expect("converter created above")
                    .input_frames,
            )
        };

        unsafe {
            let hw = self.import_drm(input_frames, frame)?;
            let mut cpu = ffi::av_frame_alloc();
            if cpu.is_null() {
                ffi::av_frame_free(&mut { hw });
                return Err(EncodeError::Ffmpeg("av_frame_alloc failed".into()));
            }
            let ret = ffi::av_hwframe_transfer_data(cpu, hw, 0);
            ffi::av_frame_free(&mut { hw });
            if ret < 0 {
                ffi::av_frame_free(&mut cpu);
                return Err(EncodeError::Ffmpeg(format!(
                    "av_hwframe_transfer_data failed: {ret}"
                )));
            }

            if self.preview_sws.is_null() || self.preview_sw_format != sw_format {
                if !self.preview_sws.is_null() {
                    ffi::sws_freeContext(self.preview_sws);
                }
                self.preview_sws = ffi::sws_getContext(
                    self.config.width as i32,
                    self.config.height as i32,
                    sw_format,
                    self.config.width as i32,
                    self.config.height as i32,
                    ffi::AVPixelFormat::AV_PIX_FMT_RGB24,
                    ffi::SwsFlags::SWS_BILINEAR as i32,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                );
                if self.preview_sws.is_null() {
                    ffi::av_frame_free(&mut cpu);
                    return Err(EncodeError::Ffmpeg(
                        "sws_getContext(preview) failed".into(),
                    ));
                }
                self.preview_sw_format = sw_format;
            }

            let mut rgb = vec![0u8; self.config.width as usize * self.config.height as usize * 3];
            let src_data = [
                (*cpu).data[0],
                (*cpu).data[1],
                (*cpu).data[2],
                (*cpu).data[3],
            ];
            let src_stride = [
                (*cpu).linesize[0],
                (*cpu).linesize[1],
                (*cpu).linesize[2],
                (*cpu).linesize[3],
            ];
            let dst_data = [
                rgb.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let dst_stride = [self.config.width as i32 * 3, 0, 0, 0];
            let ret = ffi::sws_scale(
                self.preview_sws,
                src_data.as_ptr() as *const *const u8,
                src_stride.as_ptr(),
                0,
                self.config.height as i32,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            ffi::av_frame_free(&mut cpu);
            if ret < 0 {
                return Err(EncodeError::Ffmpeg(format!(
                    "sws_scale(preview) failed: {ret}"
                )));
            }
            let jpeg = self
                .jpeg
                .as_mut()
                .ok_or_else(|| EncodeError::Ffmpeg("no JPEG encoder".into()))?;
            jpeg.encode(&rgb, self.config.width, self.config.height)
                .map_err(EncodeError::Ffmpeg)
        }
    }

    unsafe fn receive_with_keyframe(
        &mut self,
        frame: *mut ffi::AVFrame,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        if force_keyframe || self.force_keyframe {
            (*frame).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
            self.force_keyframe = false;
        }
        self.receive(frame)
    }

    unsafe fn receive(
        &mut self,
        frame: *mut ffi::AVFrame,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        let ret = ffi::avcodec_send_frame(self.ctx, frame);
        ffi::av_frame_free(&mut { frame });
        if ret < 0 {
            return Err(EncodeError::Ffmpeg(format!(
                "avcodec_send_frame failed: {ret}"
            )));
        }

        let mut out = Vec::new();
        loop {
            let mut packet = ffi::av_packet_alloc();
            if packet.is_null() {
                return Err(EncodeError::Ffmpeg("av_packet_alloc failed".into()));
            }
            let ret = ffi::avcodec_receive_packet(self.ctx, packet);
            if ret == -ffi::EAGAIN || ret == ffi::AVERROR_EOF {
                ffi::av_packet_free(&mut packet);
                break;
            }
            if ret < 0 {
                ffi::av_packet_free(&mut packet);
                return Err(EncodeError::Ffmpeg(format!(
                    "avcodec_receive_packet failed: {ret}"
                )));
            }
            let bytes = std::slice::from_raw_parts((*packet).data, (*packet).size as usize);
            out.push(EncodedFrame {
                data: bytes.to_vec(),
                keyframe: ((*packet).flags & ffi::AV_PKT_FLAG_KEY) != 0,
                pts_micros: ((*packet).pts as u64) * 1_000_000 / self.config.fps.max(1) as u64,
                duration_micros: 1_000_000 / self.config.fps.max(1) as u64,
            });
            ffi::av_packet_free(&mut packet);
        }
        Ok(out)
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn backend(&self) -> Backend {
        Backend::Vaapi
    }

    fn encode(
        &mut self,
        frame: &Frame,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedFrame>, EncodeError> {
        match frame.format() {
            Some(DrmFormat::Nv12) => self.encode_nv12(frame, force_keyframe),
            Some(format) => self.encode_rgb(frame, format, force_keyframe),
            None => Err(EncodeError::UnsupportedFormat(None)),
        }
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<(), EncodeError> {
        self.config.bitrate_kbps = bitrate_kbps;
        unsafe {
            (*self.ctx).bit_rate = (bitrate_kbps as i64) * 1000;
            (*self.ctx).rc_max_rate = (bitrate_kbps as i64) * 1000;
        }
        Ok(())
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.preview_sws.is_null() {
                ffi::sws_freeContext(self.preview_sws);
            }
            self.jpeg = None;
            self.converter = None;
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.frames);
            ffi::av_buffer_unref(&mut self.device);
        }
    }
}
