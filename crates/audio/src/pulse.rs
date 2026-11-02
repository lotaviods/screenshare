//! PulseAudio / PipeWire device enumeration via `libpulse-binding`.
//!
//! Enumerates sources (microphones and monitor/loopback sources) and sinks
//! (playback outputs). Talks to the same `pipewire-pulse` server GStreamer's
//! `pulsesrc` uses, so device ids returned here are valid `pulsesrc device=`
//! values.
//!
//! These calls block the calling thread while the PulseAudio mainloop drains;
//! they should be run off the UI/Tokio worker thread when latency matters.

use std::cell::RefCell;
use std::rc::Rc;

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SourceInfo};
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::standard::{IterateResult, Mainloop};

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
    let mut context =
        Context::new(&mainloop, "sharescreen-audio").ok_or("failed to create PulseAudio context")?;
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
                name: info.name.as_ref().map(|s| s.to_string()).unwrap_or_default(),
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
                name: info.name.as_ref().map(|s| s.to_string()).unwrap_or_default(),
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
