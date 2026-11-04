//! Windows capture via GStreamer's `d3d11screencapturesrc`.
//!
//! The element covers both paths in one place: DXGI desktop duplication for
//! monitors (`capture-api=dxgi`) and Windows.Graphics.Capture for single
//! windows (`capture-api=wgc` + `window-handle`). Monitors are enumerated from
//! the `d3d11screencapturedeviceprovider`, windows from `EnumWindows`.
//!
//! When the d3d11 element is missing (GStreamer older than 1.22), monitor
//! capture falls back to `gdiscreencapsrc`; window capture is unavailable.

use async_trait::async_trait;
use gstreamer::prelude::*;
use tokio::sync::mpsc;

use sharescreen_core::{
    CaptureError, CaptureOptions, CaptureSource, CursorMode, EncodedSample, ScreenCapture,
    SourceKind,
};

use crate::gst;

/// Windows capture backend.
pub struct WindowsCapture {
    stream: Option<gst::Stream>,
}

impl Default for WindowsCapture {
    fn default() -> Self {
        Self { stream: None }
    }
}

impl WindowsCapture {
    /// Latest JPEG self-view frame of the running capture.
    pub fn latest_preview(&self) -> Option<Vec<u8>> {
        self.stream.as_ref().and_then(|s| s.latest_preview())
    }
}

#[async_trait]
impl ScreenCapture for WindowsCapture {
    fn list_sources(&self) -> Vec<CaptureSource> {
        let mut sources = monitors();
        sources.extend(list_windows());
        if sources.is_empty() {
            sources.push(CaptureSource {
                id: "screen".into(),
                name: "Entire screen".into(),
                kind: SourceKind::Screen,
            });
        }
        sources
    }

    async fn start(
        &mut self,
        source: CaptureSource,
        options: CaptureOptions,
        sink: mpsc::Sender<EncodedSample>,
    ) -> Result<(), CaptureError> {
        self.stop().await.ok();

        gst::ensure_gst_init();
        let src = windows_source(&source, options.cursor)?;
        let launch = gst::build_launch(&src, &options);
        let stream = gst::spawn(launch, sink, options.codec, ())?;

        self.stream = Some(stream);
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), CaptureError> {
        if let Some(stream) = self.stream.take() {
            stream.stop();
        }
        Ok(())
    }
}

/// Whether the d3d11 capture element (GStreamer 1.22+) is available.
fn has_d3d11() -> bool {
    gst::element_exists("d3d11screencapturesrc")
}

/// Build the source element string for a capture source.
///
/// `window:<hwnd>` selects one window through WGC; `monitor:<n>` selects a
/// monitor through DXGI; anything else captures the primary monitor.
fn windows_source(source: &CaptureSource, cursor: CursorMode) -> Result<String, CaptureError> {
    let show_cursor = matches!(cursor, CursorMode::Embedded | CursorMode::Metadata);

    if let Some(handle) = source.id.strip_prefix("window:") {
        let handle = parse_handle(handle)
            .ok_or_else(|| CaptureError::Failed(format!("invalid window handle: {}", source.id)))?;
        if !has_d3d11() {
            return Err(CaptureError::Unavailable(
                "window capture needs d3d11screencapturesrc (GStreamer 1.22+)".into(),
            ));
        }
        return Ok(format!(
            "d3d11screencapturesrc capture-api=wgc window-handle={handle} \
             show-cursor={show_cursor}"
        ));
    }

    let index = source
        .id
        .strip_prefix("monitor:")
        .and_then(parse_handle)
        .unwrap_or(0);

    if has_d3d11() {
        Ok(format!(
            "d3d11screencapturesrc capture-api=dxgi monitor-index={index} \
             show-cursor={show_cursor}"
        ))
    } else {
        Ok(format!(
            "gdiscreencapsrc monitor={index} show-cursor={show_cursor}"
        ))
    }
}

fn parse_handle(text: &str) -> Option<i64> {
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Monitors reported by GStreamer's d3d11 capture device provider.
fn monitors() -> Vec<CaptureSource> {
    gst::ensure_gst_init();
    let Some(factory) = gstreamer::DeviceProviderFactory::find("d3d11screencapturedeviceprovider")
    else {
        return Vec::new();
    };
    let Some(provider) = factory.get() else {
        return Vec::new();
    };
    if provider.start().is_err() {
        return Vec::new();
    }

    let sources = provider
        .devices()
        .iter()
        .enumerate()
        .map(|(position, device)| {
            let index = device
                .properties()
                .and_then(|props| props.get::<i32>("monitor-index").ok())
                .unwrap_or(position as i32);
            CaptureSource {
                id: format!("monitor:{index}"),
                name: device.display_name().to_string(),
                kind: SourceKind::Monitor,
            }
        })
        .collect();

    provider.stop();
    sources
}

/// Top-level, visible, titled windows, for the window picker.
fn list_windows() -> Vec<CaptureSource> {
    windows::visible_windows()
        .into_iter()
        .map(|(handle, title)| CaptureSource {
            id: format!("window:{handle}"),
            name: title,
            kind: SourceKind::Window,
        })
        .collect()
}

#[cfg(windows)]
mod windows {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
    };

    struct Entry {
        handle: isize,
        title: String,
    }

    /// Collect visible, titled top-level windows as `(hwnd, title)`.
    pub fn visible_windows() -> Vec<(isize, String)> {
        let mut entries: Vec<Entry> = Vec::new();
        // SAFETY: the callback only writes into `entries`, which outlives the
        // call, and every Win32 handle it reads is owned by the OS.
        unsafe {
            EnumWindows(Some(collect), &mut entries as *mut Vec<Entry> as LPARAM);
        }
        entries.into_iter().map(|e| (e.handle, e.title)).collect()
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is the `&mut Vec<Entry>` passed to `EnumWindows`.
        let entries = unsafe { &mut *(lparam as *mut Vec<Entry>) };
        // SAFETY: plain Win32 queries on an OS-owned window handle.
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        // SAFETY: same as above.
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return 1;
        }

        let mut buf = vec![0u16; len as usize + 1];
        // SAFETY: `buf` has room for `len` UTF-16 units plus a terminator.
        let copied = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if copied <= 0 {
            return 1;
        }

        let title = String::from_utf16_lossy(&buf[..copied as usize]);
        if title.trim().is_empty() {
            return 1;
        }
        entries.push(Entry {
            handle: hwnd as isize,
            title,
        });
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> CaptureSource {
        CaptureSource {
            id: id.into(),
            name: "source".into(),
            kind: SourceKind::Screen,
        }
    }

    /// `has_d3d11` probes the element registry, which needs GStreamer up.
    fn init() {
        gstreamer::init().unwrap();
    }

    #[test]
    fn invalid_window_handle_is_rejected() {
        assert!(windows_source(&source("window:abc"), CursorMode::Embedded).is_err());
    }

    #[test]
    fn parse_handle_rejects_non_digits() {
        assert_eq!(parse_handle("42"), Some(42));
        assert_eq!(parse_handle(""), None);
        assert_eq!(parse_handle("4x"), None);
    }

    #[test]
    fn monitor_source_names_the_requested_monitor() {
        init();
        let src = windows_source(&source("monitor:2"), CursorMode::Hidden).unwrap();
        assert!(src.contains("monitor-index=2") || src.contains("monitor=2"));
        assert!(src.contains("show-cursor=false"));
    }

    #[test]
    fn unknown_source_uses_primary_monitor() {
        init();
        let src = windows_source(&source("screen"), CursorMode::Embedded).unwrap();
        assert!(src.contains("monitor-index=0") || src.contains("monitor=0"));
        assert!(src.contains("show-cursor=true"));
    }

    #[test]
    fn window_source_uses_wgc() {
        // Only reachable when d3d11screencapturesrc is installed.
        init();
        if !has_d3d11() {
            return;
        }
        let src = windows_source(&source("window:1234"), CursorMode::Hidden).unwrap();
        assert!(src.contains("capture-api=wgc"));
        assert!(src.contains("window-handle=1234"));
    }
}
