//! Runtime probe: enumerate audio devices, then capture ~1.5s each of the
//! default microphone and default system monitor, counting Opus packets.
//!
//! Run with: `cargo run -p sharescreen-audio --example probe`

use sharescreen_audio::AudioEngine;
use sharescreen_core::AudioCapture;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let mut engine = AudioEngine::default();

    println!("microphones:");
    for d in engine.list_microphones() {
        println!("  {}  ({})", d.name, d.id);
    }
    println!("outputs:");
    for d in engine.list_outputs() {
        println!("  {}  ({})", d.name, d.id);
    }
    println!("system monitors:");
    for d in engine.list_system_monitors() {
        println!("  {}  ({})", d.name, d.id);
    }

    // Capture default microphone.
    let (tx, mut rx) = mpsc::channel(100);
    engine.start_mic(None, tx).await.expect("start mic");
    let (n, bytes) = count_for(&mut rx, 1500).await;
    println!("mic: {n} opus packets, {bytes} bytes in ~1.5s");
    engine.stop_mic().await.expect("stop mic");

    // Capture default system monitor.
    let (tx, mut rx) = mpsc::channel(100);
    engine.start_system(None, tx).await.expect("start system");
    let (n, bytes) = count_for(&mut rx, 1500).await;
    println!("system: {n} opus packets, {bytes} bytes in ~1.5s");
    engine.stop_system().await.expect("stop system");
}

async fn count_for(rx: &mut mpsc::Receiver<sharescreen_core::EncodedSample>, ms: u64) -> (u64, u64) {
    let start = std::time::Instant::now();
    let mut n = 0u64;
    let mut bytes = 0u64;
    while start.elapsed().as_millis() < ms as u128 {
        while let Ok(s) = rx.try_recv() {
            n += 1;
            bytes += s.data.len() as u64;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    (n, bytes)
}
