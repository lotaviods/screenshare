//! Phase 0 probe: portal picker -> PipeWire DMA-BUF -> VAAPI H.264 file.
//!
//! Run on a graphical session:
//!
//! ```sh
//! cargo run -p sharescreen-native-media --example probe -- /tmp/sharescreen-probe.h264
//! ```
//!
//! The compositor picker appears once. The probe prints the negotiated buffer
//! layout, encodes NV12 frames for a few seconds, and writes Annex B H.264.
//!
//! Linux-only: it drives the XDG portal, PipeWire, and VAAPI directly.

#[cfg(target_os = "linux")]
use std::{
    fs::File,
    io::{BufWriter, Write},
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(target_os = "linux")]
use sharescreen_native_media::{
    capture::{pipewire, pipewire::Memory, portal},
    encode::{EncoderConfig, FfmpegEncoder, VideoEncoder},
    frame::DmaBufFrame,
};

#[cfg(target_os = "linux")]
#[derive(Default)]
struct Sink {
    encoder: Option<FfmpegEncoder>,
    out: Option<BufWriter<File>>,
    encoded: usize,
    keyframes: usize,
    skipped: usize,
}

#[cfg(target_os = "linux")]
impl Sink {
    fn frame(&mut self, frame: &DmaBufFrame) {
        if frame.format().is_none() {
            self.skipped += 1;
            return;
        }
        if self.encoder.is_none() {
            let config = EncoderConfig {
                width: frame.width,
                height: frame.height,
                fps: 30,
                bitrate_kbps: 6000,
                codec: sharescreen_core::Codec::H264,
                hardware_encoding: true,
            };
            match FfmpegEncoder::new(config) {
                Ok(enc) => {
                    println!(
                        "encoder up: {}x{} {:?}",
                        frame.width,
                        frame.height,
                        enc.backend()
                    );
                    self.encoder = Some(enc);
                    self.out = Some(BufWriter::new(
                        File::create(
                            std::env::args()
                                .nth(1)
                                .unwrap_or_else(|| "/tmp/sharescreen-probe.h264".to_string()),
                        )
                        .expect("create output file"),
                    ));
                }
                Err(err) => {
                    eprintln!("encoder init failed: {err}");
                    self.skipped += 1;
                    return;
                }
            }
        }
        let enc = self.encoder.as_mut().expect("encoder initialised above");
        match enc.encode(frame, false) {
            Ok(packets) => {
                let file = self.out.as_mut().expect("output opened above");
                for packet in packets {
                    file.write_all(&packet.data).expect("write packet");
                    self.encoded += 1;
                    if packet.keyframe {
                        self.keyframes += 1;
                    }
                }
            }
            Err(err) => eprintln!("encode failed: {err}"),
        }
    }

    fn finish(&mut self) {
        if let Some(mut file) = self.out.take() {
            let _ = file.flush();
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("SS_PROBE_DEBUG").is_ok() {
        sharescreen_native_media::encode::ffmpeg::enable_debug_logging();
    }
    println!("opening portal picker...");
    let stream = portal::open_stream().await?;
    println!("portal node {} ready, capturing 8s", stream.node_id);

    let memory = match std::env::var("SS_PROBE_MEMORY").as_deref() {
        Ok("memfd") => Memory::MemFd,
        Ok("both") => Memory::Both,
        _ => Memory::DmaBuf,
    };
    println!("requesting {memory:?} buffers");

    let sink = Arc::new(Mutex::new(Sink::default()));
    let callback_sink = sink.clone();
    let formats = pipewire::probe(
        stream.fd.as_raw_fd(),
        stream.node_id,
        memory,
        Duration::from_secs(8),
        move |frame| callback_sink.lock().unwrap().frame(&frame),
    )?;

    let mut sink = sink.lock().unwrap();
    sink.finish();

    println!("\nnegotiated formats:");
    for format in &formats {
        println!("{format:#?}");
    }
    println!(
        "\nwrote {} packets ({} keyframes); skipped {} unsupported frames",
        sink.encoded, sink.keyframes, sink.skipped
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("probe is Linux-only (portal + PipeWire + VAAPI)");
}
