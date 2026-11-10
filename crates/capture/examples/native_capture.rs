//! Runtime check of the production native capture path.
//!
//! Starts `WaylandCapture` exactly as the app does, receives encoded samples
//! from the sink for a few seconds, and writes them as Annex B H.264.
//!
//! ```sh
//! SHARESCREEN_MEDIA=native cargo run -p sharescreen-capture --example native_capture -- /tmp/native.h264
//! ```

use std::time::Duration;

use sharescreen_capture::WaylandCapture;
use sharescreen_core::{
    CaptureOptions, CaptureSource, Codec, CursorMode, ScreenCapture, SourceKind,
};

#[tokio::main]
async fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/sharescreen-native.h264".to_string());
    let mut capture = WaylandCapture::default();
    let (sink, mut samples) = tokio::sync::mpsc::channel(64);

    let options = CaptureOptions {
        width: 1920,
        height: 1080,
        fps: 30,
        cursor: CursorMode::Embedded,
        codec: Codec::H264,
        bitrate_kbps: 6000,
        hardware_encoding: true,
    };
    let source = CaptureSource {
        id: "screen".into(),
        name: "screen".into(),
        kind: SourceKind::Screen,
    };

    println!("opening portal picker...");
    capture.start(source, options, sink).await.expect("start");

    let mut out = std::fs::File::create(&path).expect("create output");
    let mut bytes = 0usize;
    let mut packets = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while let Ok(Some(sample)) = tokio::time::timeout_at(deadline, samples.recv()).await {
        use std::io::Write;
        out.write_all(&sample.data).expect("write");
        bytes += sample.data.len();
        packets += 1;
    }

    capture.stop().await.expect("stop");
    println!("wrote {packets} packets ({bytes} bytes) to {path}");
}
