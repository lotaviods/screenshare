//! PulseAudio / PipeWire device enumeration and application-audio capture via
//! `libpulse-binding`.
//!
//! Enumerates sources (microphones and monitor/loopback sources) and sinks
//! (playback outputs), and records individual sink inputs with
//! `pa_stream_set_monitor_stream`. Recording per application stream, instead of
//! the mixed output monitor, keeps the call's own playback out of a screen
//! share. Talks to the same `pipewire-pulse` server GStreamer uses.
//!
//! These calls block the calling thread while the PulseAudio mainloop drains;
//! they should be run off the UI/Tokio worker thread when latency matters.

use std::cell::RefCell;
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::AppSrc;
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SourceInfo};
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, PeekResult, Stream as PulseStream};
use sharescreen_core::EncodedSample;
use tokio::sync::mpsc;

/// A source or sink with the fields the app needs.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub description: String,
    /// `Some(sink_index)` when this source is a monitor of that sink (i.e. a
    /// system-audio loopback); `None` for a real input such as a microphone.
    pub monitor_of_sink: Option<u32>,
}

/// Enumerate all sources and sinks. Returns `(sources, sinks)`.
pub fn enumerate() -> Result<(Vec<DeviceInfo>, Vec<DeviceInfo>), String> {
    let mut mainloop = Mainloop::new().ok_or("failed to create PulseAudio mainloop")?;
    let mut context = Context::new(&mainloop, "sharescreen-audio")
        .ok_or("failed to create PulseAudio context")?;
    context
        .connect(None, FlagSet::NOFLAGS, None)
        .map_err(|e| format!("PulseAudio connect failed: {e}"))?;

    wait_ready(&mut mainloop, &context)?;

    let sources = collect_sources(&mut mainloop, &mut context);
    let sinks = collect_sinks(&mut mainloop, &mut context);

    context.disconnect();

    Ok((sources, sinks))
}

fn wait_ready(mainloop: &mut Mainloop, context: &Context) -> Result<(), String> {
    for _ in 0..1000 {
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => {
                return Err("PulseAudio mainloop quit while connecting".into());
            }
            IterateResult::Success(_) => {}
        }
        match context.get_state() {
            State::Ready => return Ok(()),
            State::Failed | State::Terminated => return Err("PulseAudio context failed".into()),
            _ => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Err("timed out waiting for PulseAudio context".into())
}

fn collect_sources(mainloop: &mut Mainloop, context: &mut Context) -> Vec<DeviceInfo> {
    let out: Rc<RefCell<Vec<DeviceInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(RefCell::new(false));
    {
        let out_cb = out.clone();
        let done_cb = done.clone();
        let introspect = context.introspect();
        let _op = introspect.get_source_info_list(move |r: ListResult<&SourceInfo>| match r {
            ListResult::Item(info) => out_cb.borrow_mut().push(DeviceInfo {
                name: info
                    .name
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                description: info
                    .description
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                monitor_of_sink: info.monitor_of_sink,
            }),
            ListResult::End | ListResult::Error => *done_cb.borrow_mut() = true,
        });
        drain(mainloop, &done);
    }
    let result = out.borrow().clone();
    result
}

fn collect_sinks(mainloop: &mut Mainloop, context: &mut Context) -> Vec<DeviceInfo> {
    let out: Rc<RefCell<Vec<DeviceInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(RefCell::new(false));
    {
        let out_cb = out.clone();
        let done_cb = done.clone();
        let introspect = context.introspect();
        let _op = introspect.get_sink_info_list(move |r: ListResult<&SinkInfo>| match r {
            ListResult::Item(info) => out_cb.borrow_mut().push(DeviceInfo {
                name: info
                    .name
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                description: info
                    .description
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                monitor_of_sink: None,
            }),
            ListResult::End | ListResult::Error => *done_cb.borrow_mut() = true,
        });
        drain(mainloop, &done);
    }
    let result = out.borrow().clone();
    result
}

fn drain(mainloop: &mut Mainloop, done: &Rc<RefCell<bool>>) {
    for _ in 0..1000 {
        if *done.borrow() {
            return;
        }
        match mainloop.iterate(false) {
            IterateResult::Quit(_) | IterateResult::Err(_) => return,
            IterateResult::Success(_) => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Only streams with a known, non-local process may enter a screen share.
/// Unknown ownership is dropped rather than risking a call-audio feedback loop.
pub(crate) fn capture_policy(pid: Option<u32>, self_pid: u32) -> Option<u32> {
    pid.filter(|&id| id != self_pid)
}

/// Name of the server's default sink, whose monitor matches the default output.
fn default_sink_name(mainloop: &mut Mainloop, context: &mut Context) -> Option<String> {
    let out: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let done = Rc::new(RefCell::new(false));
    {
        let out_cb = out.clone();
        let done_cb = done.clone();
        let _op = context.introspect().get_server_info(move |info| {
            *out_cb.borrow_mut() = info.default_sink_name.as_ref().map(|name| name.to_string());
            *done_cb.borrow_mut() = true;
        });
        drain(mainloop, &done);
    }
    let result = out.borrow().clone();
    result
}

#[derive(Clone, Copy)]
struct Playback {
    index: u32,
    sink: u32,
}

fn playback_inputs(mainloop: &mut Mainloop, context: &mut Context, sink: u32) -> Vec<Playback> {
    let out = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(RefCell::new(false));
    {
        let out_cb = out.clone();
        let done_cb = done.clone();
        let _op = context.introspect().get_sink_input_info_list(move |r| match r {
            ListResult::Item(info) => {
                let pid = info.proplist.get_str("application.process.id")
                    .or_else(|| info.proplist.get_str("pipewire.sec.pid"))
                    .and_then(|pid| pid.parse::<u32>().ok());
                if info.sink == sink && capture_policy(pid, std::process::id()).is_some() {
                    out_cb.borrow_mut().push(Playback { index: info.index, sink: info.sink });
                }
            }
            ListResult::End | ListResult::Error => *done_cb.borrow_mut() = true,
        });
        drain(mainloop, &done);
    }
    let result = out.borrow().clone();
    result
}

struct Input {
    stream: Rc<RefCell<PulseStream>>,
    pending: Rc<RefCell<Vec<i16>>>,
}

const CHANNELS: u8 = 2;
const RATE: u32 = 48_000;
/// One mix chunk: 20 ms of stereo S16LE at 48 kHz.
const FRAMES_PER_CHUNK: usize = 960;
const SAMPLES_PER_CHUNK: usize = FRAMES_PER_CHUNK * CHANNELS as usize;

fn attach_input(context: &mut Context, monitor: &str, index: u32) -> Result<Input, String> {
    let spec = Spec { format: Format::S16le, channels: CHANNELS, rate: RATE };
    let mut map = pulse::channelmap::Map::default();
    map.init_stereo();
    let stream = Rc::new(RefCell::new(
        PulseStream::new(context, "sharescreen application audio", &spec, Some(&map))
            .ok_or("failed to create application monitor")?,
    ));

    let pending: Rc<RefCell<Vec<i16>>> = Rc::new(RefCell::new(Vec::new()));
    let weak = Rc::downgrade(&stream);
    let pending_cb = pending.clone();
    stream.borrow_mut().set_read_callback(Some(Box::new(move |_| {
        let Some(stream) = weak.upgrade() else { return };
        let mut stream = stream.borrow_mut();
        while let Ok(chunk) = stream.peek() {
            let data = match chunk {
                PeekResult::Data(bytes) => bytes.to_vec(),
                PeekResult::Hole(_) => Vec::new(),
                PeekResult::Empty => break,
            };
            let _ = stream.discard();
            if !data.is_empty() {
                let mut samples = pending_cb.borrow_mut();
                for pair in data.as_chunks::<2>().0 {
                    samples.push(i16::from_le_bytes([pair[0], pair[1]]));
                }
                // Bound latency: drop the oldest backlog beyond ~200 ms if a
                // burst arrives faster than the mix loop drains it.
                const MAX_PENDING: usize = SAMPLES_PER_CHUNK * 10;
                if samples.len() > MAX_PENDING {
                    let excess = samples.len() - MAX_PENDING;
                    samples.drain(..excess);
                }
            }
        }
    })));

    {
        let mut stream = stream.borrow_mut();
        if let Err(e) = stream.set_monitor_stream(index) {
            return Err(format!("monitor stream rejected: {e:?}"));
        }
        let attr = pulse::def::BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 3840, // ~20ms of 48kHz stereo S16LE
        };
        stream.connect_record(Some(monitor), Some(&attr), StreamFlags::ADJUST_LATENCY)
            .map_err(|e| format!("failed to monitor playback stream {index}: {e}"))?;
    }
    Ok(Input { stream, pending })
}

pub(crate) fn run_system(
    device: Option<String>, bitrate_kbps: u32, running: Arc<AtomicBool>,
    control: Arc<Mutex<Option<gst::Element>>>, sink: mpsc::Sender<EncodedSample>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    if let Err(error) = run_system_inner(device, bitrate_kbps, running, control, sink, ready.clone()) {
        tracing::error!("system audio capture: {error}");
        let _ = ready.send(Err(error));
    }
}

fn run_system_inner(
    device: Option<String>, bitrate_kbps: u32, running: Arc<AtomicBool>,
    control: Arc<Mutex<Option<gst::Element>>>, sink: mpsc::Sender<EncodedSample>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    gst::init().map_err(|e| e.to_string())?;
    let (sources, _) = enumerate()?;

    let mut mainloop = Mainloop::new().ok_or("failed to create PulseAudio mainloop")?;
    let mut context = Context::new(&mainloop, "sharescreen-system-audio")
        .ok_or("failed to create PulseAudio context")?;
    if let Err(e) = context.connect(None, FlagSet::NOFLAGS, None) {
        return Err(format!("PulseAudio connect failed: {e:?}"));
    }
    wait_ready(&mut mainloop, &context)?;

    // Without an explicit device, follow the default output like the old
    // `@DEFAULT_MONITOR@` source did.
    let wanted = device.or_else(|| {
        default_sink_name(&mut mainloop, &mut context).map(|name| format!("{name}.monitor"))
    });
    let monitor = sources.into_iter()
        .filter(|source| source.monitor_of_sink.is_some())
        .find(|source| wanted.as_deref().is_none_or(|id| id == source.name))
        .ok_or("system audio monitor not found")?;
    let sink_index = monitor.monitor_of_sink.unwrap();

    let launch = crate::pipeline::encode_chain("appsrc name=src", bitrate_kbps);
    let pipeline = gst::parse::launch(&launch).map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>().map_err(|_| "audio pipeline is not a pipeline")?;
    let source = pipeline.by_name("src").ok_or("system audio appsrc missing")?
        .downcast::<AppSrc>().map_err(|_| "system appsrc unavailable")?;
    source.set_format(gst::Format::Time);
    source.set_caps(Some(&gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE").field("layout", "interleaved")
        .field("rate", RATE as i32).field("channels", CHANNELS as i32).build()));
    let volume = pipeline.by_name("volume").ok_or("system volume missing")?;
    *control.lock().unwrap() = Some(volume);
    let appsink = pipeline.by_name("sink").ok_or("system audio sink missing")?
        .downcast::<gstreamer_app::AppSink>().map_err(|_| "system appsink unavailable")?;
    pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;
    let _ = ready.send(Ok(()));

    let mut inputs: HashMap<u32, Input> = HashMap::new();
    let mut last_refresh = Instant::now() - Duration::from_secs(1);
    let mut last_mix = Instant::now();
    let mut pts_ns: u64 = 0;
    while running.load(Ordering::SeqCst) {
        if last_refresh.elapsed() >= Duration::from_millis(500) {
            let current = playback_inputs(&mut mainloop, &mut context, sink_index);
            let ids: std::collections::HashSet<u32> = current.iter().map(|i| i.index).collect();
            for index in inputs.keys().copied().filter(|index| !ids.contains(index)).collect::<Vec<_>>() {
                if let Some(input) = inputs.remove(&index) {
                    let _ = input.stream.borrow_mut().disconnect();
                }
            }
            for item in current {
                if let std::collections::hash_map::Entry::Vacant(slot) = inputs.entry(item.index) {
                    match attach_input(&mut context, &monitor.name, item.index) {
                        Ok(input) => { slot.insert(input); }
                        Err(e) => tracing::warn!("failed to capture playback stream {} on sink {}: {e}", item.index, item.sink),
                    }
                }
            }
            last_refresh = Instant::now();
        }
        mainloop.iterate(false);

        if last_mix.elapsed() >= Duration::from_millis(20) {
            last_mix = Instant::now();
            let mut mixed = vec![0i32; SAMPLES_PER_CHUNK];
            let mut any = false;
            for input in inputs.values() {
                let mut pending = input.pending.borrow_mut();
                let n = pending.len().min(SAMPLES_PER_CHUNK);
                if n > 0 {
                    any = true;
                    for (i, sample) in pending.iter().take(n).enumerate() {
                        mixed[i] += *sample as i32;
                    }
                }
                pending.drain(..n);
            }
            if any {
                let mut out = Vec::with_capacity(SAMPLES_PER_CHUNK * 2);
                for sample in mixed {
                    let clamped = sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    out.extend_from_slice(&clamped.to_le_bytes());
                }
                let mut buffer = gst::Buffer::from_mut_slice(out);
                if let Some(raw) = buffer.get_mut() {
                    raw.set_pts(gst::ClockTime::from_nseconds(pts_ns));
                }
                pts_ns += FRAMES_PER_CHUNK as u64 * 1_000_000_000 / RATE as u64;
                let _ = source.push_buffer(buffer);
            }
        }

        if let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(10)) {
            if let Some(encoded) = crate::pipeline::sample_to_encoded(&sample) {
                if sink.blocking_send(encoded).is_err() { break; }
            }
        }
        if let Some(bus) = pipeline.bus() {
            while let Some(message) = bus.pop() {
                if let gst::MessageView::Error(error) = message.view() {
                    tracing::error!("system audio pipeline error: {}", error.error());
                    running.store(false, Ordering::SeqCst);
                }
            }
        }
    }
    for (_, input) in inputs.drain() {
        let _ = input.stream.borrow_mut().disconnect();
    }
    let _ = pipeline.set_state(gst::State::Null);
    *control.lock().unwrap() = None;
    context.disconnect();
    Ok(())
}
