//! Native Windows capture: GDI desktop/window capture -> FFmpeg.
//!
//! Uses `BitBlt` for monitors and `PrintWindow` for windows. Frames are copied
//! to a BGRA DIB and encoded by FFmpeg's H.264/VP8/VP9 encoder. No GStreamer is
//! involved.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sharescreen_core::{
    CaptureError, CaptureOptions, CaptureSource, Codec, EncodedSample, MediaKind, SourceKind,
};
use sharescreen_native_media::encode::{EncoderConfig, FfmpegEncoder, VideoEncoder};

/// Whether the native path can encode this request.
pub fn available(codec: Codec) -> bool {
    matches!(codec, Codec::H264 | Codec::Vp8 | Codec::Vp9) && FfmpegEncoder::available(codec)
}

/// A running native capture: a GDI thread pushing encoded samples.
pub struct Stream {
    running: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    preview: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Stream {
    pub fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.thread.join();
    }

    /// Latest JPEG self-view frame of the running capture.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        self.preview.lock().unwrap().clone()
    }
}

pub fn spawn(
    source: CaptureSource,
    options: &CaptureOptions,
    sink: tokio::sync::mpsc::Sender<EncodedSample>,
) -> Result<Stream, CaptureError> {
    let options = options.clone();
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let preview = Arc::new(Mutex::new(None));
    let preview_sink = preview.clone();
    let preview_every = options.fps.max(10) / 10;

    let thread = std::thread::Builder::new()
        .name("sharescreen-native-windows".into())
        .spawn(move || {
            run(source, options, flag, sink, preview_sink, preview_every);
        })
        .map_err(|e| CaptureError::Failed(format!("failed to spawn native capture thread: {e}")))?;

    Ok(Stream {
        running,
        thread,
        preview,
    })
}

fn run(
    source: CaptureSource,
    options: CaptureOptions,
    running: Arc<AtomicBool>,
    sink: tokio::sync::mpsc::Sender<EncodedSample>,
    preview_sink: Arc<Mutex<Option<Vec<u8>>>>,
    preview_every: u32,
) {
    let fps = options.fps.max(1);
    let bitrate_kbps = options.bitrate_kbps.max(1000);
    let width = options.width;
    let height = options.height;
    let cursor = options.cursor;
    let target = match target::Target::new(&source) {
        Ok(target) => target,
        Err(err) => {
            tracing::error!("native Windows capture failed: {err}");
            return;
        }
    };
    let mut encoder: Option<FfmpegEncoder> = None;
    let frame_duration = Duration::from_secs_f64(1.0 / fps as f64);
    let mut next = Instant::now();
    let mut index: u64 = 0;
    let mut frame_count: u64 = 0;

    while running.load(Ordering::SeqCst) {
        let Some(frame) = target.capture(cursor, index, fps) else {
            tracing::warn!("native Windows capture returned no frame");
            std::thread::sleep(Duration::from_millis(16));
            continue;
        };
        if encoder.is_none() {
            let w = if width == 0 { frame.width } else { width };
            let h = if height == 0 { frame.height } else { height };
            let config = EncoderConfig {
                width: w,
                height: h,
                fps,
                bitrate_kbps,
                codec: options.codec,
            };
            match FfmpegEncoder::new(config) {
                Ok(created) => encoder = Some(created),
                Err(err) => {
                    tracing::error!("native Windows encoder unavailable: {err}");
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
                        codec: options.codec,
                        data: frame.data,
                        timestamp_micros: frame.pts_micros,
                        duration_micros: frame.duration_micros,
                        keyframe: frame.keyframe,
                    };
                    if sink.blocking_send(sample).is_err() {
                        return;
                    }
                }
                if frame_count.is_multiple_of(preview_every as u64) {
                    if let Ok(jpeg) = encoder.preview(&frame) {
                        *preview_sink.lock().unwrap() = Some(jpeg);
                    }
                }
                frame_count = frame_count.wrapping_add(1);
            }
            Err(err) => {
                tracing::warn!("native Windows encode failed: {err}");
                return;
            }
        }
        index += 1;
        next += frame_duration;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

/// Monitors and top-level windows, as the source picker sees them.
pub fn sources() -> Vec<CaptureSource> {
    let mut sources = target::monitors();
    sources.extend(target::windows());
    if sources.is_empty() {
        sources.push(CaptureSource {
            id: "screen".into(),
            name: "Entire screen".into(),
            kind: SourceKind::Screen,
        });
    }
    sources
}

#[cfg(windows)]
mod target {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors,
        GetDC, GetMonitorInfoW, GetWindowDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBRUSH, HDC, HGDIOBJ, HMONITOR, MONITORINFO,
        MONITORINFOEXW, ROP_CODE, SRCCOPY,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::UI::WindowsAndMessaging::{
        DrawIconEx, EnumWindows, GetCursorInfo, GetIconInfo, GetSystemMetrics, GetWindowRect,
        GetWindowTextLengthW, GetWindowTextW, IsWindowVisible, CURSORINFO, CURSOR_SHOWING,
        DI_NORMAL, HICON, ICONINFO, PW_RENDERFULLCONTENT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    use sharescreen_core::{CaptureError, CaptureSource, CursorMode, SourceKind};
    use sharescreen_native_media::frame::RawFrame;

    pub struct Target {
        kind: Kind,
    }

    enum Kind {
        Monitor(usize),
        Window(HWND),
        Screen,
    }

    impl Target {
        pub fn new(source: &CaptureSource) -> Result<Self, CaptureError> {
            if let Some(handle) = source.id.strip_prefix("window:") {
                let handle = handle.parse::<isize>().map_err(|_| {
                    CaptureError::Failed(format!("invalid window handle: {}", source.id))
                })?;
                let hwnd = HWND(handle as *mut core::ffi::c_void);
                let _ = window_rect(hwnd)?;
                return Ok(Self {
                    kind: Kind::Window(hwnd),
                });
            }
            if source.id == "screen" {
                return Ok(Self { kind: Kind::Screen });
            }
            let index = source
                .id
                .strip_prefix("monitor:")
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            monitor_rect(index).ok_or_else(|| {
                CaptureError::SourceNotFound(format!("monitor {index} not found"))
            })?;
            Ok(Self {
                kind: Kind::Monitor(index),
            })
        }

        pub fn capture(&self, cursor: CursorMode, index: u64, fps: u32) -> Option<RawFrame> {
            let rect = match self.kind {
                Kind::Monitor(index) => monitor_rect(index)?,
                Kind::Window(hwnd) => window_rect(hwnd).ok()?,
                Kind::Screen => virtual_screen_rect(),
            };
            let width = rect.width;
            let height = rect.height;
            if width == 0 || height == 0 {
                return None;
            }
            let stride = width * 4;

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default()],
            };

            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbitmap =
                unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()? };
            let hdc = unsafe { CreateCompatibleDC(None) };
            let old = unsafe { SelectObject(hdc, HGDIOBJ(hbitmap.0)) };

            let copied = match self.kind {
                Kind::Window(hwnd) => {
                    let ok =
                        unsafe { PrintWindow(hwnd, hdc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)) };
                    if !ok.as_bool() {
                        let dc = unsafe { GetWindowDC(Some(hwnd)) };
                        let result = unsafe {
                            BitBlt(
                                hdc,
                                0,
                                0,
                                width as i32,
                                height as i32,
                                Some(dc),
                                0,
                                0,
                                SRCCOPY | CAPTUREBLT,
                            )
                        };
                        let _ = unsafe { ReleaseDC(Some(hwnd), dc) };
                        result.is_ok()
                    } else {
                        true
                    }
                }
                _ => {
                    let screen = unsafe { GetDC(None) };
                    let result = unsafe {
                        BitBlt(
                            hdc,
                            0,
                            0,
                            width as i32,
                            height as i32,
                            Some(screen),
                            rect.left,
                            rect.top,
                            SRCCOPY | CAPTUREBLT,
                        )
                    };
                    let _ = unsafe { ReleaseDC(None, screen) };
                    result.is_ok()
                }
            };

            if !copied {
                let _ = unsafe { SelectObject(hdc, old) };
                let _ = unsafe { DeleteObject(HGDIOBJ(hbitmap.0)) };
                let _ = unsafe { DeleteDC(hdc) };
                return None;
            }

            if matches!(cursor, CursorMode::Embedded | CursorMode::Metadata) {
                draw_cursor(hdc, rect);
            }

            let data = unsafe {
                std::slice::from_raw_parts(bits as *const u8, stride as usize * height as usize)
                    .to_vec()
            };
            let _ = unsafe { SelectObject(hdc, old) };
            let _ = unsafe { DeleteObject(HGDIOBJ(hbitmap.0)) };
            let _ = unsafe { DeleteDC(hdc) };

            Some(RawFrame {
                width,
                height,
                stride,
                data,
                pts_micros: index.saturating_mul(1_000_000) / fps.max(1) as u64,
            })
        }
    }

    #[derive(Clone, Copy)]
    struct Rect {
        left: i32,
        top: i32,
        width: u32,
        height: u32,
    }

    fn window_rect(hwnd: HWND) -> Result<Rect, CaptureError> {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(hwnd, &mut rect) }
            .map_err(|e| CaptureError::Failed(format!("GetWindowRect failed: {e}")))?;
        Ok(Rect {
            left: rect.left,
            top: rect.top,
            width: (rect.right - rect.left).max(0) as u32,
            height: (rect.bottom - rect.top).max(0) as u32,
        })
    }

    fn virtual_screen_rect() -> Rect {
        let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        Rect {
            left: x,
            top: y,
            width: width.max(0) as u32,
            height: height.max(0) as u32,
        }
    }

    fn monitor_rect(index: usize) -> Option<Rect> {
        monitor_infos().get(index).map(|info| info.rect)
    }

    pub fn monitors() -> Vec<CaptureSource> {
        monitor_infos()
            .into_iter()
            .enumerate()
            .map(|(index, info)| CaptureSource {
                id: format!("monitor:{index}"),
                name: if info.name.is_empty() {
                    format!("Monitor {index}")
                } else {
                    info.name
                },
                kind: SourceKind::Monitor,
            })
            .collect()
    }

    pub fn windows() -> Vec<CaptureSource> {
        visible_windows()
            .into_iter()
            .map(|(handle, title)| CaptureSource {
                id: format!("window:{handle}"),
                name: title,
                kind: SourceKind::Window,
            })
            .collect()
    }

    struct MonitorInfo {
        rect: Rect,
        name: String,
    }

    fn monitor_infos() -> Vec<MonitorInfo> {
        let mut infos: Vec<MonitorInfo> = Vec::new();
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(monitor_callback),
                LPARAM(&mut infos as *mut Vec<MonitorInfo> as isize),
            );
        }
        infos
    }

    unsafe extern "system" fn monitor_callback(
        hmonitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec<MonitorInfo>` passed to `EnumDisplayMonitors`.
        let infos = unsafe { &mut *(lparam.0 as *mut Vec<MonitorInfo>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a valid `MONITORINFOEXW`; `cbSize` says so.
        if unsafe { GetMonitorInfoW(hmonitor, &mut info.monitorInfo as *mut MONITORINFO) }.as_bool()
        {
            let name = String::from_utf16_lossy(&info.szDevice)
                .trim_end_matches('\0')
                .to_string();
            let rc = info.monitorInfo.rcMonitor;
            infos.push(MonitorInfo {
                rect: Rect {
                    left: rc.left,
                    top: rc.top,
                    width: (rc.right - rc.left).max(0) as u32,
                    height: (rc.bottom - rc.top).max(0) as u32,
                },
                name,
            });
        }
        BOOL(1)
    }

    struct Entry {
        handle: isize,
        title: String,
    }

    fn visible_windows() -> Vec<(isize, String)> {
        let mut entries: Vec<Entry> = Vec::new();
        // SAFETY: the callback only writes into `entries`, which outlives the
        // call, and every Win32 handle it reads is owned by the OS.
        unsafe {
            let _ = EnumWindows(
                Some(collect),
                LPARAM(&mut entries as *mut Vec<Entry> as isize),
            );
        }
        entries.into_iter().map(|e| (e.handle, e.title)).collect()
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec<Entry>` passed to `EnumWindows`.
        let entries = unsafe { &mut *(lparam.0 as *mut Vec<Entry>) };
        // SAFETY: plain Win32 queries on an OS-owned window handle.
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return BOOL(1);
        }

        let mut buf = vec![0u16; len as usize + 1];
        // SAFETY: `buf` has room for `len` UTF-16 units plus a terminator.
        let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if copied <= 0 {
            return BOOL(1);
        }

        let title = String::from_utf16_lossy(&buf[..copied as usize]);
        if title.trim().is_empty() {
            return BOOL(1);
        }
        entries.push(Entry {
            handle: hwnd.0 as isize,
            title,
        });
        BOOL(1)
    }

    fn draw_cursor(hdc: HDC, rect: Rect) {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a valid `CURSORINFO` with `cbSize` set.
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            return;
        }
        if info.flags.0 & CURSOR_SHOWING.0 == 0 {
            return;
        }
        let x = info.ptScreenPos.x - rect.left;
        let y = info.ptScreenPos.y - rect.top;
        if x < 0 || y < 0 || x >= rect.width as i32 || y >= rect.height as i32 {
            return;
        }
        let mut icon = ICONINFO::default();
        // SAFETY: the cursor handle is owned by the OS.
        if unsafe { GetIconInfo(HICON(info.hCursor.0), &mut icon) }.is_err() {
            return;
        }
        let _ = unsafe { DrawIconEx(hdc, x, y, HICON(info.hCursor.0), 0, 0, 0, None, DI_NORMAL) };
        let _ = unsafe { DeleteObject(HGDIOBJ(icon.hbmMask.0)) };
        let _ = unsafe { DeleteObject(HGDIOBJ(icon.hbmColor.0)) };
    }

    #[allow(dead_code)]
    fn _assert_types(_: HBRUSH, _: ROP_CODE, _: RECT) {}
}
