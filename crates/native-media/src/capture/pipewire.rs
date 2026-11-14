//! PipeWire screen capture fed by an XDG Desktop Portal session.
//!
//! Phase 0 probe: connect to the portal's node, negotiate raw video, and hand
//! DMA-BUF frames to a callback. No production wiring yet.

use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::spa;

use crate::frame::{DmaBufFrame, DmaBufLayer, DmaBufObject, DmaBufPlane, DrmFormat};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("pipewire error: {0}")]
    Pipewire(String),
    #[error("portal error: {0}")]
    Portal(String),
    #[error("portal picker cancelled")]
    Cancelled,
    #[error("portal returned no usable stream")]
    NoStream,
}

#[derive(Debug, Clone)]
pub struct CapturedFormat {
    pub width: u32,
    pub height: u32,
    pub spa_format: u32,
    pub drm_format: Option<DrmFormat>,
    pub modifier: u64,
    pub data_type: String,
    pub planes: usize,
    pub stride: i32,
    pub chunk_offset: u32,
    pub chunk_size: u32,
    pub frames: usize,
}

#[derive(Default)]
struct ProbeState {
    current: Option<VideoInfo>,
    formats: Vec<CapturedFormat>,
    frames: usize,
    error: Option<String>,
}

/// Requested buffer memory. DMA-BUF is required for zero-copy encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    DmaBuf,
    MemFd,
    Both,
}

impl Memory {
    fn mask(self) -> (i32, Vec<i32>) {
        use spa::sys::{SPA_DATA_DmaBuf, SPA_DATA_MemFd};
        let dmabuf = 1i32 << SPA_DATA_DmaBuf;
        let memfd = 1i32 << SPA_DATA_MemFd;
        match self {
            Self::DmaBuf => (dmabuf, vec![dmabuf]),
            Self::MemFd => (memfd, vec![memfd]),
            Self::Both => (dmabuf, vec![dmabuf, memfd]),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct VideoInfo {
    width: u32,
    height: u32,
    format: u32,
    modifier: u64,
}

/// Connect to a portal PipeWire node and report the negotiated buffer layout.
///
/// The callback receives every DMA-BUF frame. It runs on the PipeWire thread,
/// so keep it short (or hand the frame to an encoder thread).
pub fn probe<F>(
    fd: RawFd,
    node_id: u32,
    memory: Memory,
    timeout: Duration,
    on_frame: F,
) -> Result<Vec<CapturedFormat>, CaptureError>
where
    F: FnMut(DmaBufFrame) + 'static,
{
    run(
        fd,
        node_id,
        memory,
        Some(timeout),
        Arc::new(AtomicUsize::new(1)),
        on_frame,
    )
}

/// Run the capture stream until `running` is cleared or `timeout` elapses.
///
/// `running` holds a nonzero value while the stream should keep going, so the
/// owner can stop it from another thread.
pub fn run<F>(
    fd: RawFd,
    node_id: u32,
    memory: Memory,
    timeout: Option<Duration>,
    running: Arc<AtomicUsize>,
    on_frame: F,
) -> Result<Vec<CapturedFormat>, CaptureError>
where
    F: FnMut(DmaBufFrame) + 'static,
{
    run_configured(
        fd, node_id, memory, timeout, running, 1920, 1080, 30, on_frame,
    )
}

/// Run PipeWire with a preferred capture size and frame rate. The compositor
/// may negotiate a different supported mode; callers should use the actual
/// dimensions reported by captured frames as the effective output size.
pub fn run_configured<F>(
    fd: RawFd,
    node_id: u32,
    memory: Memory,
    timeout: Option<Duration>,
    running: Arc<AtomicUsize>,
    width: u32,
    height: u32,
    fps: u32,
    mut on_frame: F,
) -> Result<Vec<CapturedFormat>, CaptureError>
where
    F: FnMut(DmaBufFrame) + 'static,
{
    pw::init();

    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|e| CaptureError::Pipewire(e.to_string()))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| CaptureError::Pipewire(e.to_string()))?;
    // Keep ownership of the caller's portal fd; PipeWire takes the dup.
    let owned = unsafe {
        let dup = libc::dup(fd);
        if dup < 0 {
            return Err(CaptureError::Pipewire("dup(portal fd) failed".into()));
        }
        OwnedFd::from_raw_fd(dup)
    };
    let core = context
        .connect_fd_rc(owned, None)
        .map_err(|e| CaptureError::Pipewire(e.to_string()))?;

    let stream = pw::stream::StreamRc::new(
        core,
        "sharescreen-probe",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
            *pw::keys::NODE_NAME => "sharescreen-probe",
        },
    )
    .map_err(|e| CaptureError::Pipewire(e.to_string()))?;

    let state = Arc::new(Mutex::new(ProbeState::default()));
    let seen = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();

    let _listener = stream
        .add_local_listener_with_user_data(state.clone())
        .state_changed(|_, state, _, new| {
            if let pw::stream::StreamState::Error(message) = new {
                state.lock().unwrap().error = Some(message);
            }
        })
        .param_changed(|_, state, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            let (media_type, media_subtype) = match spa::param::format_utils::parse_format(param) {
                Ok(v) => v,
                Err(_) => return,
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let mut guard = state.lock().unwrap();
            guard.current = Some(VideoInfo {
                width: info.size().width,
                height: info.size().height,
                format: info.format().as_raw(),
                modifier: info.modifier(),
            });
        })
        .process(move |stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(current) = state.lock().unwrap().current else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }

            let frame_no = seen.fetch_add(1, Ordering::Relaxed);
            let drm = DrmFormat::from_spa(current.format);
            let pts = started.elapsed().as_micros() as u64;

            let first = &datas[0];
            let first_type = format!("{:?}", first.type_());
            let first_stride = first.chunk().stride();
            let first_offset = first.chunk().offset();
            let first_size = first.chunk().size();

            if frame_no < 4 {
                let mut guard = state.lock().unwrap();
                if !guard.formats.iter().any(|f| {
                    f.spa_format == current.format
                        && f.data_type == first_type
                        && f.planes == datas.len()
                }) {
                    guard.formats.push(CapturedFormat {
                        width: current.width,
                        height: current.height,
                        spa_format: current.format,
                        drm_format: drm,
                        modifier: current.modifier,
                        data_type: first_type.clone(),
                        planes: datas.len(),
                        stride: first_stride,
                        chunk_offset: first_offset,
                        chunk_size: first_size,
                        frames: 0,
                    });
                }
                if let Some(entry) = guard.formats.last_mut() {
                    entry.frames += 1;
                }
                guard.frames += 1;
            }

            if !matches!(first.type_(), spa::buffer::DataType::DmaBuf) {
                return;
            }
            let Some(layer_format) = drm else { return };

            // NV12 often arrives as two data entries backed by the same dmabuf.
            // FFmpeg's VAAPI import accepts exactly one DRM object, so dedupe by
            // raw fd and point every plane at its object.
            let mut objects: Vec<DmaBufObject> = Vec::new();
            let mut object_fds: Vec<RawFd> = Vec::new();
            let mut object_sizes: Vec<u64> = Vec::new();
            let mut planes = Vec::with_capacity(datas.len());
            for data in datas.iter() {
                let fd = data.fd();
                if fd < 0 {
                    return;
                }
                let object_index = match object_fds.iter().position(|&known| known == fd) {
                    Some(index) => index,
                    None => {
                        let dup = unsafe { libc::dup(fd) };
                        if dup < 0 {
                            return;
                        }
                        object_fds.push(fd);
                        objects.push(DmaBufObject {
                            fd: unsafe { OwnedFd::from_raw_fd(dup) },
                            size: 0,
                        });
                        object_sizes.push(0);
                        objects.len() - 1
                    }
                };
                let chunk = data.chunk();
                let plane_index = planes.len();
                let pitch = chunk.stride().max(0) as u64;
                // DMA-BUF chunks report a dummy size; derive the plane extent
                // from stride and plane height instead.
                let rows = plane_rows(layer_format, plane_index, current.height) as u64;
                let extent = pitch * rows;
                let end = chunk.offset() as u64 + u64::from(chunk.size()).max(extent);
                object_sizes[object_index] = object_sizes[object_index].max(end);
                planes.push(DmaBufPlane {
                    object_index: object_index as u8,
                    offset: chunk.offset(),
                    pitch: chunk.stride().max(0) as u32,
                });
            }
            for (object, size) in objects.iter_mut().zip(object_sizes) {
                object.size = size;
            }

            let frame = DmaBufFrame {
                width: current.width,
                height: current.height,
                modifier: current.modifier,
                objects,
                layers: vec![DmaBufLayer {
                    format: layer_format,
                    planes,
                }],
                pts_micros: pts,
            };
            on_frame(frame);
        })
        .register()
        .map_err(|e| CaptureError::Pipewire(e.to_string()))?;

    let values = video_params(width.max(1), height.max(1), fps.clamp(1, 120), memory);
    let pod = spa::pod::Pod::from_bytes(&values).expect("valid video pod");
    let buffers = buffer_params(memory);
    let buffers = spa::pod::Pod::from_bytes(&buffers).expect("valid buffer pod");
    let mut params = [pod, buffers];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| CaptureError::Pipewire(e.to_string()))?;

    let quit = mainloop.clone();
    if let Some(timeout) = timeout {
        let timer = mainloop.loop_().add_timer(move |_| quit.quit());
        timer
            .update_timer(Some(timeout), None)
            .into_result()
            .map_err(|e| CaptureError::Pipewire(e.to_string()))?;
    }

    let stop = mainloop.clone();
    let watchdog = mainloop.loop_().add_timer(move |_| {
        if running.load(Ordering::Relaxed) == 0 {
            stop.quit();
        }
    });
    watchdog
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .map_err(|e| CaptureError::Pipewire(e.to_string()))?;

    mainloop.run();

    let guard = state.lock().unwrap();
    if guard.formats.is_empty() {
        if let Some(error) = &guard.error {
            return Err(CaptureError::Pipewire(error.clone()));
        }
    }
    Ok(guard.formats.clone())
}

/// Ask the compositor for the requested buffer memory.
fn buffer_params(memory: Memory) -> Vec<u8> {
    use spa::pod::{ChoiceValue, Object, Property, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let (default, allowed) = memory.mask();
    let object = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default,
                    flags: allowed,
                },
            ))),
        )],
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .expect("serialize buffer params")
    .0
    .into_inner();
    values
}

/// Rows covered by a plane, used to size DMA-BUF objects.
fn plane_rows(format: DrmFormat, plane: usize, height: u32) -> u32 {
    match (format, plane) {
        (DrmFormat::Nv12, 1) => height.div_ceil(2),
        _ => height,
    }
}

fn video_params(width: u32, height: u32, fps: u32, memory: Memory) -> Vec<u8> {
    use spa::pod::{ChoiceValue, Object, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    const DRM_FORMAT_MOD_LINEAR: u64 = 0;
    const DRM_FORMAT_MOD_INVALID: u64 = (1 << 56) - 1;

    let mut properties = vec![
        existing(
            spa::param::format::FormatProperties::MediaType.as_raw(),
            Value::Id(spa::utils::Id(
                spa::param::format::MediaType::Video.as_raw(),
            )),
        ),
        existing(
            spa::param::format::FormatProperties::MediaSubtype.as_raw(),
            Value::Id(spa::utils::Id(
                spa::param::format::MediaSubtype::Raw.as_raw(),
            )),
        ),
        Property {
            key: spa::param::format::FormatProperties::VideoFormat.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Id(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: spa::utils::Id(spa::param::video::VideoFormat::NV12.as_raw()),
                    alternatives: vec![
                        spa::utils::Id(spa::param::video::VideoFormat::NV12.as_raw()),
                        spa::utils::Id(spa::param::video::VideoFormat::BGRx.as_raw()),
                        spa::utils::Id(spa::param::video::VideoFormat::BGRA.as_raw()),
                        spa::utils::Id(spa::param::video::VideoFormat::RGBx.as_raw()),
                        spa::utils::Id(spa::param::video::VideoFormat::RGBA.as_raw()),
                    ],
                },
            ))),
        },
        Property {
            key: spa::param::format::FormatProperties::VideoSize.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: spa::utils::Rectangle { width, height },
                    min: spa::utils::Rectangle {
                        width: 1,
                        height: 1,
                    },
                    max: spa::utils::Rectangle {
                        width: 7680,
                        height: 4320,
                    },
                },
            ))),
        },
        Property {
            key: spa::param::format::FormatProperties::VideoFramerate.as_raw(),
            flags: PropertyFlags::empty(),
            value: Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: spa::utils::Fraction { num: fps, denom: 1 },
                    min: spa::utils::Fraction { num: 0, denom: 1 },
                    max: spa::utils::Fraction { num: 120, denom: 1 },
                },
            ))),
        },
    ];

    // Hyprland (and other portals) only export DMA-BUF when the modifier
    // property is present, and fixate it only when DONT_FIXATE is set.
    if memory != Memory::MemFd {
        let mut flags = PropertyFlags::from_bits_truncate(spa::sys::SPA_POD_PROP_FLAG_MANDATORY);
        flags |= PropertyFlags::from_bits_truncate(spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE);
        properties.push(Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags,
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: DRM_FORMAT_MOD_INVALID as i64,
                    alternatives: vec![DRM_FORMAT_MOD_LINEAR as i64],
                },
            ))),
        });
    }

    let object = Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .expect("serialize video format")
    .0
    .into_inner();
    values
}

/// Plain (non-choice) property, as the `property!` macro would build.
fn existing(key: u32, value: spa::pod::Value) -> spa::pod::Property {
    spa::pod::Property {
        key,
        flags: spa::pod::PropertyFlags::empty(),
        value,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn has_prop(values: &[u8], key: u32) -> bool {
        let pod = spa::pod::Pod::from_bytes(values).expect("valid pod");
        let object = unsafe {
            spa::pod::PodObject::from_raw(pod.as_raw_ptr().cast::<spa::sys::spa_pod_object>())
        };
        object.find_prop(spa::utils::Id(key)).is_some()
    }

    #[test]
    fn dma_params_request_modifier() {
        let values = video_params(1920, 1080, 30, Memory::DmaBuf);
        let key = spa::param::format::FormatProperties::VideoModifier.as_raw();
        assert!(has_prop(&values, key));
    }

    #[test]
    fn memfd_params_omit_modifier() {
        let values = video_params(1920, 1080, 30, Memory::MemFd);
        let key = spa::param::format::FormatProperties::VideoModifier.as_raw();
        assert!(!has_prop(&values, key));
    }

    #[test]
    fn buffer_mask_matches_memory() {
        assert_eq!(
            Memory::DmaBuf.mask(),
            (
                1 << spa::sys::SPA_DATA_DmaBuf,
                vec![1 << spa::sys::SPA_DATA_DmaBuf]
            )
        );
        assert_eq!(
            Memory::MemFd.mask(),
            (
                1 << spa::sys::SPA_DATA_MemFd,
                vec![1 << spa::sys::SPA_DATA_MemFd]
            )
        );
        let (_, allowed) = Memory::Both.mask();
        assert_eq!(allowed.len(), 2);
    }
}
