//! Live smoke test: `cargo test -p sharescreen-audio --test native_smoke -- --ignored --nocapture`

use std::time::Duration;

use sharescreen_audio::AudioEngine;
use sharescreen_core::{AudioCapture, Codec, EncodedSample};
use tokio::sync::mpsc;

fn collect(
    rx: &mut mpsc::Receiver<EncodedSample>,
    want: usize,
    limit: Duration,
) -> Vec<EncodedSample> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + limit;
    while out.len() < want && std::time::Instant::now() < deadline {
        match rx.blocking_recv() {
            Some(sample) => out.push(sample),
            None => break,
        }
    }
    out
}

fn check(samples: &[EncodedSample], label: &str) {
    assert!(
        samples.len() >= 10,
        "{label}: only {} packets",
        samples.len()
    );
    for sample in samples {
        assert_eq!(sample.codec, Codec::Opus);
        assert!(sample.duration_micros > 0);
        assert!(!sample.data.is_empty());
    }
    eprintln!(
        "{label}: {} packets, {} bytes",
        samples.len(),
        samples.iter().map(|s| s.data.len()).sum::<usize>()
    );
}

#[test]
#[ignore]
fn mic_produces_opus_packets() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut engine = AudioEngine::default();
    let (tx, mut rx) = mpsc::channel(1000);
    rt.block_on(engine.start_mic(None, tx)).expect("start mic");
    let samples = collect(&mut rx, 20, Duration::from_secs(2));
    rt.block_on(engine.stop_mic()).unwrap();
    check(&samples, "mic");
}

#[test]
#[ignore]
fn system_produces_opus_packets() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut engine = AudioEngine::default();
    let (tx, mut rx) = mpsc::channel(1000);
    rt.block_on(engine.start_system(None, tx))
        .expect("start system");
    let samples = collect(&mut rx, 20, Duration::from_secs(3));
    rt.block_on(engine.stop_system()).unwrap();
    check(&samples, "system");
}
