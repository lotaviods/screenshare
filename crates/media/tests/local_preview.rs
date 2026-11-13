//! The self-view loopback decodes the outgoing stream instead of re-encoding
//! the raw captured frame, and stops working when nobody watches it.

use std::time::{Duration, Instant};

use sharescreen_core::{Codec, EncodedSample, MediaKind};
use sharescreen_media::LocalPreview;

const H264_SAMPLE: &[u8] = include_bytes!("../fixtures/sample.h264");

fn sample(keyframe: bool) -> EncodedSample {
    EncodedSample {
        kind: MediaKind::Video,
        codec: Codec::H264,
        data: H264_SAMPLE.to_vec(),
        timestamp_micros: 0,
        duration_micros: 33_333,
        keyframe,
    }
}

#[test]
fn decodes_the_outgoing_stream_while_watched() {
    let preview = LocalPreview::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut jpeg = None;
    while Instant::now() < deadline {
        // Poll first: the loopback only decodes a watched self-view.
        preview.latest_frame();
        preview.push(&sample(true));
        if let Some(bytes) = preview.latest_frame() {
            jpeg = Some(bytes);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let jpeg = jpeg.expect("loopback produced a self-view frame");
    assert!(jpeg.starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
    assert!(jpeg.ends_with(&[0xFF, 0xD9]), "JPEG EOI marker");
}

#[test]
fn stays_idle_while_nobody_polls() {
    let preview = LocalPreview::new();
    for _ in 0..10 {
        preview.push(&sample(true));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        preview.latest_frame().is_none(),
        "decoded a self-view frame with no viewer"
    );
}

#[test]
fn non_keyframes_do_not_produce_a_frame() {
    let preview = LocalPreview::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut inter = sample(false);
    inter.data = b"not a decodable access unit".to_vec();
    while Instant::now() < deadline {
        preview.latest_frame();
        preview.push(&inter);
        assert!(
            preview.latest_frame().is_none(),
            "decoded an access unit with no keyframe and no reference frame"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
