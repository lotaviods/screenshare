//! Live smoke test: `cargo test -p sharescreen-audio --test native_smoke -- --ignored --nocapture`

use std::time::Duration;

use sharescreen_audio::{AudioEngine, Output};
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

fn check_one(samples: Vec<EncodedSample>, label: &str) {
    let sample = samples
        .first()
        .unwrap_or_else(|| panic!("{label}: no packets"));
    assert_eq!(sample.codec, Codec::Opus);
    assert!(sample.duration_micros > 0);
    assert!(!sample.data.is_empty());
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

#[test]
#[ignore]
fn playback_device_open_play_close_100_times() {
    let pcm = vec![0i16; 960 * 2];
    for _ in 0..100 {
        let mut output = Output::open(None).expect("open playback device");
        output.play(&pcm);
        output.stop();
    }
}

#[test]
#[ignore]
fn capture_devices_open_use_close_100_times() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    for _ in 0..100 {
        let mut engine = AudioEngine::default();
        let (mic_tx, mut mic_rx) = mpsc::channel(8);
        rt.block_on(engine.start_mic(None, mic_tx))
            .expect("open microphone");
        check_one(collect(&mut mic_rx, 1, Duration::from_secs(2)), "mic");
        rt.block_on(engine.stop_mic()).expect("close microphone");

        let (system_tx, mut system_rx) = mpsc::channel(8);
        rt.block_on(engine.start_system(None, system_tx))
            .expect("open system loopback");
        check_one(
            collect(&mut system_rx, 1, Duration::from_secs(2)),
            "system loopback",
        );
        rt.block_on(engine.stop_system())
            .expect("close system loopback");
    }
}
