//! Platform-independent self-view preview worker.
//!
//! The capture loop hands each frame to the worker and moves on: the worker
//! downscales and JPEG-encodes on its own thread, so neither the capture path
//! nor the video encoder pays for the preview. Frames overwrite each other
//! ("latest wins"), so a slow encode never builds a backlog.
//!
//! The worker is platform-agnostic. A capture backend supplies CPU pixels by
//! implementing [`PreviewSource`] for its frame type (BGRA from GDI/Screen
//! Capture on Windows, a downloaded CPU frame from the DMA-BUF on Linux).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::decode::JpegEncoder;

/// Widest preview we ever encode. The UI shows the self-view at most in the
/// main stage, so this bounds JPEG cost without visible loss.
pub const MAX_PREVIEW_WIDTH: u32 = 1280;

/// Slowest interval between preview encodes (~30 fps ceiling).
const MIN_INTERVAL: Duration = Duration::from_millis(33);

/// A captured frame the preview worker can turn into a downscaled JPEG.
///
/// Implemented per platform for the frame type the capture backend produces.
pub trait PreviewSource: Send + Sync {
    /// Encode this frame as JPEG, scaled to fit `max_width`, preserving aspect.
    fn to_jpeg(&self, encoder: &mut JpegEncoder, max_width: u32) -> Result<Vec<u8>, String>;
}

/// Produces the local self-view JPEG away from the capture thread.
pub struct Previewer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    pending: Mutex<Option<Arc<dyn PreviewSource>>>,
    ready: Condvar,
    latest: Mutex<Option<Vec<u8>>>,
    requested: AtomicBool,
    running: AtomicBool,
}

impl Previewer {
    /// Spawn the worker thread.
    pub fn spawn() -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::new(None),
            ready: Condvar::new(),
            latest: Mutex::new(None),
            requested: AtomicBool::new(false),
            running: AtomicBool::new(true),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("sharescreen-preview".into())
            .spawn(move || run(worker))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Hand the newest frame to the worker without waiting for a contended lock.
    pub fn publish(&self, frame: Arc<dyn PreviewSource>) {
        if !self.shared.requested.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut pending) = self.shared.pending.try_lock() {
            *pending = Some(frame);
            self.shared.ready.notify_one();
        } else {
            self.shared.requested.store(true, Ordering::Release);
        }
    }

    /// Latest encoded preview, if any.
    pub fn latest(&self) -> Option<Vec<u8>> {
        self.shared.requested.store(true, Ordering::Release);
        self.shared.latest.lock().unwrap().clone()
    }
}

impl Drop for Previewer {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        self.shared.ready.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: Arc<Shared>) {
    let mut encoder = match JpegEncoder::new() {
        Ok(encoder) => encoder,
        Err(err) => {
            tracing::error!("preview JPEG encoder unavailable: {err}");
            return;
        }
    };
    let mut last = Instant::now() - MIN_INTERVAL;

    while shared.running.load(Ordering::SeqCst) {
        let Some(mut frame) = take_pending(&shared) else {
            continue;
        };
        // Coalesce: frames published during the last encode were overwritten,
        // so we already hold the newest.
        let now = Instant::now();
        if now < last + MIN_INTERVAL {
            std::thread::sleep(last + MIN_INTERVAL - now);
        }
        if !shared.running.load(Ordering::SeqCst) {
            break;
        }
        // Frames can arrive during the throttle interval: encode the newest.
        if let Ok(mut pending) = shared.pending.lock() {
            if let Some(newer) = pending.take() {
                frame = newer;
            }
        }
        last = Instant::now();

        match frame.to_jpeg(&mut encoder, MAX_PREVIEW_WIDTH) {
            Ok(jpeg) => *shared.latest.lock().unwrap() = Some(jpeg),
            Err(err) => tracing::warn!("preview JPEG encode failed: {err}"),
        }
    }
}

/// Block until a frame is published or the worker is stopped.
fn take_pending(shared: &Shared) -> Option<Arc<dyn PreviewSource>> {
    let mut pending = shared.pending.lock().unwrap();
    while pending.is_none() && shared.running.load(Ordering::SeqCst) {
        let (guard, _) = shared
            .ready
            .wait_timeout(pending, Duration::from_millis(100))
            .unwrap();
        pending = guard;
    }
    pending.take()
}

/// Preview size: `width`×`height` scaled to fit within `max_width`, preserving
/// aspect ratio. Returns `(0, 0)` for an empty frame.
pub fn preview_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_width == 0 {
        return (0, 0);
    }
    if width <= max_width {
        return (width, height);
    }
    let height = (height as u64 * max_width as u64 / width as u64).max(1) as u32;
    (max_width, height)
}

#[cfg(windows)]
impl PreviewSource for crate::frame::RawFrame {
    fn to_jpeg(&self, encoder: &mut JpegEncoder, max_width: u32) -> Result<Vec<u8>, String> {
        let (dst_width, dst_height) = preview_size(self.width, self.height, max_width);
        if dst_width == 0 || dst_height == 0 {
            return Err("empty frame".into());
        }
        encoder.encode_bgra_scaled(&self.data, self.width, self.height, dst_width, dst_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_frames_pass_through() {
        assert_eq!(preview_size(640, 480, MAX_PREVIEW_WIDTH), (640, 480));
        assert_eq!(preview_size(1280, 720, MAX_PREVIEW_WIDTH), (1280, 720));
    }

    #[test]
    fn large_frames_scale_to_max_width_keeping_aspect() {
        assert_eq!(preview_size(2560, 1440, MAX_PREVIEW_WIDTH), (1280, 720));
        assert_eq!(preview_size(3840, 2160, MAX_PREVIEW_WIDTH), (1280, 720));
        // Odd aspect ratios round but never collapse to zero.
        assert_eq!(preview_size(2560, 1081, MAX_PREVIEW_WIDTH), (1280, 540));
    }

    #[test]
    fn empty_frames_are_rejected() {
        assert_eq!(preview_size(0, 0, MAX_PREVIEW_WIDTH), (0, 0));
        assert_eq!(preview_size(1920, 0, MAX_PREVIEW_WIDTH), (0, 0));
    }

    #[test]
    fn publishing_does_not_wait_for_pending_lock() {
        struct Frame;
        impl PreviewSource for Frame {
            fn to_jpeg(&self, _: &mut JpegEncoder, _: u32) -> Result<Vec<u8>, String> {
                Ok(Vec::new())
            }
        }
        let previewer = Previewer::spawn().expect("preview thread");
        previewer.shared.requested.store(true, Ordering::Release);
        let guard = previewer.shared.pending.lock().unwrap();
        // Would deadlock if publish acquired the same mutex in blocking mode.
        previewer.publish(Arc::new(Frame));
        assert!(guard.is_none());
        drop(guard);
    }

    #[cfg(windows)]
    #[test]
    fn worker_encodes_a_jpeg_off_thread() {
        use crate::frame::RawFrame;
        use std::time::Instant;

        let (width, height) = (64u32, 48u32);
        let data = (0..(width * height * 4) as usize)
            .map(|i| (i % 251) as u8)
            .collect();
        let frame = RawFrame {
            width,
            height,
            stride: width * 4,
            data,
            pts_micros: 0,
        };

        let previewer = Previewer::spawn().expect("preview thread");
        assert!(previewer.latest().is_none());
        previewer.publish(Arc::new(frame));

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut jpeg = None;
        while Instant::now() < deadline {
            if let Some(bytes) = previewer.latest() {
                jpeg = Some(bytes);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let jpeg = jpeg.expect("worker produced a preview");
        assert!(jpeg.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
        assert!(jpeg.ends_with(&[0xFF, 0xD9]), "JPEG EOI marker");
    }
}
