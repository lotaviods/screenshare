//! Tauri commands exposed to the frontend.
//!
//! Media samples produced by capture/audio are routed to the active call
//! session, which fans them out to every peer's WebRTC track. Without a call
//! session the samples are discarded.

use sharescreen_call::{CallConfig, CallSession};
use sharescreen_core::{
    AudioCapture, CallEvent, CaptureOptions, CaptureSource, Device, EncodedSample, ScreenCapture,
};
use sharescreen_config::Config;
use sharescreen_network as network;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::mpsc;

use crate::state::{AppState, ShareTarget};

/// Event name the frontend listens on for call state changes.
pub const CALL_EVENT: &str = "call-event";

/// Which local media stream a command feeds.
#[derive(Clone, Copy, Debug)]
enum LocalStream {
    Mic,
    System,
    Screen,
}

/// Spawn a task that drains (discards) media samples.
fn drain(channel: usize) -> mpsc::Sender<EncodedSample> {
    let (tx, mut rx) = mpsc::channel(channel);
    tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });
    tx
}

/// The sink for a local media stream: the call session's fan-out when
/// connected, otherwise a discard sink.
async fn media_sink(state: &State<'_, AppState>, stream: LocalStream) -> mpsc::Sender<EncodedSample> {
    let call = state.call.lock().await;
    match call.as_ref() {
        Some(session) => {
            tracing::info!("routing {stream:?} samples into the call session");
            match stream {
                LocalStream::Mic => session.mic_sender(),
                LocalStream::System => session.system_sender(),
                LocalStream::Screen => session.screen_sender(),
            }
        }
        None => {
            tracing::warn!("no call session; discarding {stream:?} samples");
            drain(64)
        }
    }
}

// --- configuration ---------------------------------------------------------

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Result<Config, String> {
    let store = state.config.lock().map_err(|e| e.to_string())?;
    Ok(store.config().clone())
}

#[tauri::command]
pub fn update_config(state: State<'_, AppState>, config: Config) -> Result<(), String> {
    config.validate().map_err(|e| e.to_string())?;
    let mut store = state.config.lock().map_err(|e| e.to_string())?;
    *store.config_mut() = config;
    store.save().map_err(|e| e.to_string())
}

// --- devices and sources ---------------------------------------------------

#[tauri::command]
pub async fn list_microphones(state: State<'_, AppState>) -> Result<Vec<Device>, String> {
    let engine = state.audio.lock().await;
    Ok(engine.list_microphones())
}

#[tauri::command]
pub async fn list_outputs(state: State<'_, AppState>) -> Result<Vec<Device>, String> {
    let engine = state.audio.lock().await;
    Ok(engine.list_outputs())
}

#[tauri::command]
pub async fn list_system_monitors(state: State<'_, AppState>) -> Result<Vec<Device>, String> {
    let engine = state.audio.lock().await;
    Ok(engine.list_system_monitors())
}

#[tauri::command]
pub async fn list_capture_sources(state: State<'_, AppState>) -> Result<Vec<CaptureSource>, String> {
    let engine = state.capture.lock().await;
    Ok(engine.list_sources())
}

// --- local media -----------------------------------------------------------

#[tauri::command]
pub async fn start_screen_share(
    state: State<'_, AppState>,
    source: CaptureSource,
    options: CaptureOptions,
) -> Result<(), String> {
    let tx = media_sink(&state, LocalStream::Screen).await;
    // The screen track's codec is fixed at call setup, so capture must encode
    // with the same codec or the samples cannot be decoded.
    let mut options = options;
    {
        let call = state.call.lock().await;
        if let Some(session) = call.as_ref() {
            options.codec = session.video_codec();
        }
    }
    let mut engine = state.capture.lock().await;
    engine
        .start(source.clone(), options.clone(), tx)
        .await
        .map_err(|e| e.to_string())?;
    drop(engine);
    *state.share.lock().unwrap() = Some(ShareTarget { source, options });
    // Tell the other participants that a screen is being shared, so they know
    // whose video to render.
    let call = state.call.lock().await;
    if let Some(session) = call.as_ref() {
        session.set_screen_sharing(true);
    }
    Ok(())
}

#[tauri::command]
pub async fn stop_screen_share(state: State<'_, AppState>) -> Result<(), String> {
    *state.share.lock().unwrap() = None;
    let mut engine = state.capture.lock().await;
    engine.stop().await.map_err(|e| e.to_string())?;
    drop(engine);
    let call = state.call.lock().await;
    if let Some(session) = call.as_ref() {
        session.set_screen_sharing(false);
    }
    Ok(())
}

#[tauri::command]
pub async fn start_microphone(
    state: State<'_, AppState>,
    device: Option<String>,
) -> Result<(), String> {
    let tx = media_sink(&state, LocalStream::Mic).await;
    let mut engine = state.audio.lock().await;
    engine
        .start_mic(device.as_deref(), tx)
        .await
        .map_err(|e| e.to_string())?;
    drop(engine);
    state.mic_running.store(true, Ordering::SeqCst);
    let call = state.call.lock().await;
    if let Some(session) = call.as_ref() {
        session.set_mic_on(true);
    }
    Ok(())
}

#[tauri::command]
pub async fn stop_microphone(state: State<'_, AppState>) -> Result<(), String> {
    let mut engine = state.audio.lock().await;
    engine.stop_mic().await.map_err(|e| e.to_string())?;
    drop(engine);
    state.mic_running.store(false, Ordering::SeqCst);
    let call = state.call.lock().await;
    if let Some(session) = call.as_ref() {
        session.set_mic_on(false);
    }
    Ok(())
}

#[tauri::command]
pub async fn start_system_audio(
    state: State<'_, AppState>,
    device: Option<String>,
) -> Result<(), String> {
    let tx = media_sink(&state, LocalStream::System).await;
    let mut engine = state.audio.lock().await;
    engine
        .start_system(device.as_deref(), tx)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_system_audio(state: State<'_, AppState>) -> Result<(), String> {
    let mut engine = state.audio.lock().await;
    engine.stop_system().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_mic_volume(state: State<'_, AppState>, volume: f32) -> Result<(), String> {
    let mut engine = state.audio.lock().await;
    engine.set_mic_volume(volume).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn set_mic_muted(state: State<'_, AppState>, muted: bool) -> Result<(), String> {
    let mut engine = state.audio.lock().await;
    engine.set_mic_muted(muted).map_err(|e| e.to_string())?;
    drop(engine);
    // A muted microphone is off as far as the other participants are concerned.
    let live = state.mic_running.load(Ordering::SeqCst) && !muted;
    let call = state.call.lock().await;
    if let Some(session) = call.as_ref() {
        session.set_mic_on(live);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_system_volume(state: State<'_, AppState>, volume: f32) -> Result<(), String> {
    let mut engine = state.audio.lock().await;
    engine.set_system_volume(volume).map_err(|e| e.to_string())
}

// --- call ------------------------------------------------------------------

/// Connect to the configured signaling server and start a call session.
#[tauri::command]
pub async fn connect_server(state: State<'_, AppState>) -> Result<(), String> {
    let mut call = state.call.lock().await;
    if call.is_some() {
        return Ok(()); // idempotent
    }

    let config = {
        let store = state.config.lock().map_err(|e| e.to_string())?;
        store.config().clone()
    };

    let peer_id = network::generate_peer_id();
    let display_name = config.state.display_name.clone();
    let call_config = CallConfig {
        self_id: peer_id.clone(),
        self_name: display_name.clone(),
        signaling_url: network::signaling_url_with_identity(
            &config.network,
            &peer_id,
            &display_name,
        ),
        peer_config: network::peer_config(&config.network),
        video_codec: config.video.codec,
    };

    // Bridge call events to the frontend.
    let (tx, mut rx) = mpsc::unbounded_channel::<CallEvent>();
    let handle = state.app.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let CallEvent::RoomCreated { room_id } | CallEvent::RoomJoined { room_id } = &event {
                remember_room(&handle, room_id);
            }
            if let Err(e) = handle.emit(CALL_EVENT, &event) {
                tracing::warn!("failed to emit call event: {e}");
            }
        }
    });

    let session = CallSession::start(call_config, tx)
        .await
        .map_err(|e| e.to_string())?;
    *call = Some(session);

    // Remember the server that connected successfully.
    {
        let mut store = state.config.lock().map_err(|e| e.to_string())?;
        store.config_mut().record_successful_server(
            config.network.signaling_host.clone(),
            config.network.signaling_port,
            config.network.signaling_secure,
        );
        let _ = store.save();
    }
    Ok(())
}

/// Record a joined room in the persistent preferences.
fn remember_room(app: &tauri::AppHandle, room_id: &str) {
    let state = app.state::<AppState>();
    let mut store = match state.config.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    store.config_mut().record_joined_room(room_id.to_string());
    let _ = store.save();
}

/// Whether a signaling server session is active.
#[tauri::command]
pub async fn is_connected(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.call.lock().await.is_some())
}

/// The current room id, if any.
#[tauri::command]
pub async fn get_current_room(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let call = state.call.lock().await;
    Ok(call.as_ref().and_then(|s| s.room_id()))
}

/// Disconnect from the signaling server and tear down the call session.
#[tauri::command]
pub async fn disconnect_server(state: State<'_, AppState>) -> Result<(), String> {
    *state.share.lock().unwrap() = None;
    let mut call = state.call.lock().await;
    if let Some(session) = call.as_ref() {
        // Close the peer connections and stop the decoders before dropping
        // the session, so their threads and pipelines are released.
        session.disconnect().await;
    }
    *call = None;
    Ok(())
}

#[tauri::command]
pub async fn create_room(state: State<'_, AppState>) -> Result<(), String> {
    let call = state.call.lock().await;
    let session = call.as_ref().ok_or("not connected to a signaling server")?;
    session.create_room().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn join_room(state: State<'_, AppState>, room_id: String) -> Result<(), String> {
    let call = state.call.lock().await;
    let session = call.as_ref().ok_or("not connected to a signaling server")?;
    session.join_room(room_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn leave_room(state: State<'_, AppState>) -> Result<(), String> {
    let call = state.call.lock().await;
    let session = call.as_ref().ok_or("not connected to a signaling server")?;
    session.leave_room().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_room(state: State<'_, AppState>) -> Result<(), String> {
    let call = state.call.lock().await;
    let session = call.as_ref().ok_or("not connected to a signaling server")?;
    session.delete_room().await.map_err(|e| e.to_string())
}

/// The latest decoded video frame (JPEG) from a peer. Returns raw bytes; an
/// empty body means no frame is available yet.
#[tauri::command]
pub async fn remote_frame(
    state: State<'_, AppState>,
    peer_id: String,
) -> Result<tauri::ipc::Response, String> {
    let frame = {
        let call = state.call.lock().await;
        call.as_ref().and_then(|s| s.remote_frame(&peer_id))
    };
    Ok(match frame {
        Some(bytes) => tauri::ipc::Response::new(bytes),
        None => tauri::ipc::Response::new(Vec::new()),
    })
}

/// The latest JPEG self-view frame of the local screen share. Empty when not
/// sharing or when the capture backend has not produced a frame yet.
#[tauri::command]
pub async fn local_frame(state: State<'_, AppState>) -> Result<tauri::ipc::Response, String> {
    let frame = {
        let engine = state.capture.lock().await;
        engine.latest_preview()
    };
    Ok(match frame {
        Some(bytes) => tauri::ipc::Response::new(bytes),
        None => tauri::ipc::Response::new(Vec::new()),
    })
}

/// The latest decoded video frame (JPEG) of whoever is sharing their screen,
/// for the picture-in-picture window. Falls back to the local capture preview
/// when nobody else is sharing.
#[tauri::command]
pub async fn pip_frame(state: State<'_, AppState>) -> Result<tauri::ipc::Response, String> {
    let frame = {
        let call = state.call.lock().await;
        call.as_ref().and_then(|s| s.shared_screen_frame())
    };
    if frame.is_some() {
        return Ok(tauri::ipc::Response::new(frame.unwrap()));
    }
    // Fall back to local preview when no remote sharer.
    let preview = {
        let engine = state.capture.lock().await;
        engine.latest_preview()
    };
    Ok(match preview {
        Some(bytes) => tauri::ipc::Response::new(bytes),
        None => tauri::ipc::Response::new(Vec::new()),
    })
}

/// Open a separate always-on-top PiP window showing the shared screen.
/// Uses the same frontend bundle with `?pip=1`. Idempotent: returns Ok
/// if the window already exists.
#[tauri::command]
pub async fn open_pip_window(app: tauri::AppHandle) -> Result<(), String> {
    // If the window already exists, focus it.
    if app.get_webview_window("pip").is_some() {
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "pip", WebviewUrl::App(PathBuf::from("index.html?pip=1")))
        .title("Sharescreen PiP")
        .always_on_top(true)
        .decorations(false)
        .resizable(false)
        .inner_size(480.0, 270.0)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Close the PiP window if it is open.
#[tauri::command]
pub async fn close_pip_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("pip") {
        window.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Follow monitor hotplug: when the shared source disappears, restart capture
/// on the first remaining source so the share does not silently go black.
pub fn spawn_hotplug_watcher(app: tauri::AppHandle) {
    // `setup` runs outside a Tokio runtime, so spawn on Tauri's.
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            let state = app.state::<AppState>();
            let Some(target) = state.share.lock().unwrap().clone() else {
                continue;
            };

            let sources = state.capture.lock().await.list_sources();
            if sources.iter().any(|s| s.id == target.source.id) {
                continue;
            }
            let Some(next) = sources.into_iter().next() else {
                continue;
            };

            let tx = media_sink(&state, LocalStream::Screen).await;
            let mut engine = state.capture.lock().await;
            if engine
                .start(next.clone(), target.options.clone(), tx)
                .await
                .is_ok()
            {
                drop(engine);
                *state.share.lock().unwrap() = Some(ShareTarget {
                    source: next,
                    options: target.options,
                });
                tracing::info!("shared source disappeared, moved capture to a remaining source");
            }
        }
    });
}

/// Set the playback volume for one peer's audio.
#[tauri::command]
pub async fn set_peer_volume(
    state: State<'_, AppState>,
    peer_id: String,
    volume: f32,
) -> Result<(), String> {
    let call = state.call.lock().await;
    let session = call.as_ref().ok_or("not connected to a signaling server")?;
    session.set_peer_volume(&peer_id, volume as f64);
    Ok(())
}
