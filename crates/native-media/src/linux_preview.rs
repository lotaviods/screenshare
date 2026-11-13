//! Linux-native self-view preview worker.
//!
//! Mirrors `Previewer::run` but uses VAAPI import on Linux instead of GDI.
//! Decodes captured frames, downscales to 1280 wide, and emits JPEGs via
//! `FfmpegEncoder` + `VideoDecoder`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::decode::JpegEncoder;
use crate::encode::EncoderConfig;
use crate::preview::PreviewSource;
use sharescreen_core::Codec;

const MAX_PREVIEW_WIDTH: u32 = 1280;
const MIN_INTERVAL: Duration = Duration::from_millis(33);
pub struct LinuxPreviewer {
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

impl LinuxPreviewer {
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
            .name("sharescreen-linux-preview".into())
            .spawn(move || run(worker))
            .map_err(std::io::Error::other)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

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

    pub fn latest(&self) -> Option<Vec<u8>> {
        self.shared.requested.store(true, Ordering::Release);
        self.shared.latest.lock().unwrap().clone()
    }
}

impl Drop for LinuxPreviewer {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

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

fn run(shared: Arc<Shared>) {
    let _config = EncoderConfig {
        width: 0,
        height: 0,
        fps: 30,
        bitrate_kbps: 2000,
        codec: Codec::H264,
    };
    let mut jpeg = match JpegEncoder::new() {
        Ok(e) => e,
        Err(err) => {
            tracing::error!("JpegEncoder unavailable: {err}");
            return;
        }
    };

    let mut last = Instant::now() - MIN_INTERVAL;

    while shared.running.load(Ordering::SeqCst) {
        let Some(mut frame) = take_pending(&shared) else {
            continue;
        };
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

        match frame.to_jpeg(&mut jpeg, MAX_PREVIEW_WIDTH) {
            Ok(jpeg) => {
                *shared.latest.lock().unwrap() = Some(jpeg);
            }
            Err(err) => tracing::warn!("preview JPEG encode failed: {err}"),
        }
    }
}
