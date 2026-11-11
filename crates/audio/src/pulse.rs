//! PulseAudio / PipeWire device enumeration and application-audio capture via
//! `libpulse-binding`.
//!
//! Enumerates sources (microphones and monitor/loopback sources) and sinks
//! (playback outputs), and records individual sink inputs with
//! `pa_stream_set_monitor_stream`. Recording per application stream, instead of
//! the mixed output monitor, keeps the call's own playback out of a screen
//! share. Talks to the same `pipewire-pulse` server.
//!
//! These calls block the calling thread while the PulseAudio mainloop drains;
//! they should be run off the UI/Tokio worker thread when latency matters.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SourceInfo};
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlags, PeekResult, Stream as PulseStream};

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
        let _op = context
            .introspect()
            .get_sink_input_info_list(move |r| match r {
                ListResult::Item(info) => {
                    let pid = info
                        .proplist
                        .get_str("application.process.id")
                        .or_else(|| info.proplist.get_str("pipewire.sec.pid"))
                        .and_then(|pid| pid.parse::<u32>().ok());
                    if info.sink == sink && capture_policy(pid, std::process::id()).is_some() {
                        out_cb.borrow_mut().push(Playback {
                            index: info.index,
                            sink: info.sink,
                        });
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

/// Record one input stream as 48 kHz stereo S16LE. `monitor` selects a single
/// sink input (application playback) instead of the input itself; `device`
/// picks the source and defaults to the server's default source.
fn record(
    context: &mut Context,
    device: Option<&str>,
    monitor: Option<u32>,
) -> Result<Input, String> {
    let spec = Spec {
        format: Format::S16le,
        channels: CHANNELS,
        rate: RATE,
    };
    let mut map = pulse::channelmap::Map::default();
    map.init_stereo();
    let stream = Rc::new(RefCell::new(
        PulseStream::new(context, "sharescreen audio capture", &spec, Some(&map))
            .ok_or("failed to create audio capture stream")?,
    ));

    let pending: Rc<RefCell<Vec<i16>>> = Rc::new(RefCell::new(Vec::new()));
    let weak = Rc::downgrade(&stream);
    let pending_cb = pending.clone();
    stream
        .borrow_mut()
        .set_read_callback(Some(Box::new(move |_| {
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
        if let Some(index) = monitor {
            if let Err(e) = stream.set_monitor_stream(index) {
                return Err(format!("monitor stream rejected: {e:?}"));
            }
        }
        let attr = pulse::def::BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: 3840, // ~20ms of 48kHz stereo S16LE
        };
        stream
            .connect_record(device, Some(&attr), StreamFlags::ADJUST_LATENCY)
            .map_err(|e| format!("failed to record input: {e}"))?;
    }
    Ok(Input { stream, pending })
}

/// A recording stream owned by one capture thread.
pub(crate) struct Recorder {
    mainloop: Mainloop,
    context: Context,
    input: Input,
}

impl Recorder {
    /// Connect to PulseAudio and record `device` (or the default source).
    pub(crate) fn open(device: Option<String>) -> Result<Self, String> {
        let mut mainloop = Mainloop::new().ok_or("failed to create PulseAudio mainloop")?;
        let mut context = Context::new(&mainloop, "sharescreen-microphone")
            .ok_or("failed to create PulseAudio context")?;
        if let Err(e) = context.connect(None, FlagSet::NOFLAGS, None) {
            return Err(format!("PulseAudio connect failed: {e:?}"));
        }
        wait_ready(&mut mainloop, &context)?;
        let input = record(&mut context, device.as_deref(), None)?;
        Ok(Self {
            mainloop,
            context,
            input,
        })
    }

    /// Drain the mainloop so read callbacks fire.
    pub(crate) fn pump(&mut self) {
        self.mainloop.iterate(false);
    }

    /// Take every pending interleaved stereo sample.
    pub(crate) fn take(&self) -> Vec<i16> {
        let mut pending = self.input.pending.borrow_mut();
        std::mem::take(&mut *pending)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.input.stream.borrow_mut().disconnect();
        self.context.disconnect();
    }
}

/// Plays remote audio on a PulseAudio output device.
pub(crate) struct PlaybackSink {
    mainloop: Mainloop,
    context: Context,
    stream: PulseStream,
}

impl PlaybackSink {
    /// Open a playback stream on `device` (or the default output).
    pub(crate) fn open(device: Option<String>) -> Result<Self, String> {
        let mut mainloop = Mainloop::new().ok_or("failed to create PulseAudio mainloop")?;
        let mut context = Context::new(&mainloop, "sharescreen-playback")
            .ok_or("failed to create PulseAudio context")?;
        if let Err(e) = context.connect(None, FlagSet::NOFLAGS, None) {
            return Err(format!("PulseAudio connect failed: {e:?}"));
        }
        wait_ready(&mut mainloop, &context)?;

        let spec = Spec {
            format: Format::S16le,
            channels: CHANNELS,
            rate: RATE,
        };
        let mut map = pulse::channelmap::Map::default();
        map.init_stereo();
        let mut stream = PulseStream::new(&mut context, "sharescreen playback", &spec, Some(&map))
            .ok_or("failed to create audio playback stream")?;
        stream
            .connect_playback(device.as_deref(), None, StreamFlags::empty(), None, None)
            .map_err(|e| format!("failed to connect playback: {e}"))?;
        Ok(Self {
            mainloop,
            context,
            stream,
        })
    }

    /// Write interleaved 16-bit stereo samples. The stream is drained on the
    /// next call, so a burst is bounded by the server's buffer.
    pub(crate) fn play(&mut self, samples: &[i16]) {
        let bytes = samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>();
        let _ = self
            .stream
            .write_copy(&bytes, 0, pulse::stream::SeekMode::Relative);
        self.mainloop.iterate(false);
    }

    pub(crate) fn stop(&mut self) {
        let _ = self.stream.disconnect();
        self.context.disconnect();
    }
}

/// A live mix of every non-call playback stream on one monitor sink.
pub(crate) struct Mixer {
    mainloop: Mainloop,
    context: Context,
    monitor: String,
    sink_index: u32,
    inputs: HashMap<u32, Input>,
    last_refresh: Instant,
    last_mix: Instant,
}

impl Mixer {
    /// Connect to PulseAudio and resolve the monitor for `device`, or the
    /// default output's monitor when `None`.
    pub(crate) fn open(device: Option<String>) -> Result<Self, String> {
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
        let monitor = sources
            .into_iter()
            .filter(|source| source.monitor_of_sink.is_some())
            .find(|source| wanted.as_deref().is_none_or(|id| id == source.name))
            .ok_or("system audio monitor not found")?;
        let sink_index = monitor.monitor_of_sink.unwrap();

        Ok(Self {
            mainloop,
            context,
            monitor: monitor.name,
            sink_index,
            inputs: HashMap::new(),
            last_refresh: Instant::now() - Duration::from_secs(1),
            last_mix: Instant::now(),
        })
    }

    /// Pump the mainloop and track playback streams as they come and go.
    pub(crate) fn pump(&mut self) {
        if self.last_refresh.elapsed() >= Duration::from_millis(500) {
            let current = playback_inputs(&mut self.mainloop, &mut self.context, self.sink_index);
            let ids: std::collections::HashSet<u32> = current.iter().map(|i| i.index).collect();
            for index in self
                .inputs
                .keys()
                .copied()
                .filter(|index| !ids.contains(index))
                .collect::<Vec<_>>()
            {
                if let Some(input) = self.inputs.remove(&index) {
                    let _ = input.stream.borrow_mut().disconnect();
                }
            }
            for item in current {
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    self.inputs.entry(item.index)
                {
                    match attach_input(&mut self.context, &self.monitor, item.index) {
                        Ok(input) => {
                            slot.insert(input);
                        }
                        Err(e) => tracing::warn!(
                            "failed to capture playback stream {} on sink {}: {e}",
                            item.index,
                            item.sink
                        ),
                    }
                }
            }
            self.last_refresh = Instant::now();
        }
        self.mainloop.iterate(false);
    }

    /// One 20 ms stereo chunk of the sum of every captured stream, when due.
    pub(crate) fn chunk(&mut self) -> Option<Vec<i16>> {
        if self.last_mix.elapsed() < Duration::from_millis(20) {
            return None;
        }
        self.last_mix = Instant::now();

        let mut mixed = vec![0i32; SAMPLES_PER_CHUNK];
        let mut any = false;
        for input in self.inputs.values() {
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
        if !any {
            return None;
        }

        let mut out = Vec::with_capacity(SAMPLES_PER_CHUNK);
        for sample in mixed {
            out.push(sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        }
        Some(out)
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        for (_, input) in self.inputs.drain() {
            let _ = input.stream.borrow_mut().disconnect();
        }
        self.context.disconnect();
    }
}

fn attach_input(context: &mut Context, monitor: &str, index: u32) -> Result<Input, String> {
    record(context, Some(monitor), Some(index))
}

#[cfg(test)]
mod tests {
    use super::capture_policy;

    #[test]
    fn system_capture_skips_own_process() {
        // A mixed output monitor would replay remote participants to the call.
        assert_eq!(capture_policy(Some(42), 42), None);
        assert_eq!(capture_policy(Some(43), 42), Some(43));
        assert_eq!(capture_policy(None, 42), None);
    }
}
