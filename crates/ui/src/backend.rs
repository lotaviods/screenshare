//! Backend actor: owns capture/audio/call state and communicates with the GPUI
//! thread through channels. The UI never blocks on media or network work.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use gpui::RenderImage;
use sharescreen_audio::AudioEngine;
use sharescreen_call::{CallConfig, CallSession};
use sharescreen_capture::CaptureEngine;
use sharescreen_config::NoiseSuppressionLevel;
use sharescreen_config::{Config, ConfigStore};
use sharescreen_core::{
    AudioCapture, CallEvent, CaptureBackend, CaptureOptions, CaptureSource, Device, DeviceKind,
    EncodedSample, ScreenCapture,
};
use sharescreen_network as network;
use tokio::sync::mpsc;

/// Which frame stream an update belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FrameKind {
    Remote(String),
    Pip,
    Local,
}

/// Identifies one screen share without confusing our preview with a peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareId {
    Local,
    Remote(String),
}

/// What the UI wants each poll cycle.
#[derive(Clone, Debug, Default)]
pub struct FrameRequest {
    /// Whether to update (or clear) the remote stage.
    pub remote: bool,
    /// Remote participants whose decoded frames to fetch.
    pub peers: Vec<String>,
    /// Fetch the best frame for the picture-in-picture window.
    pub pip: bool,
    /// The share shown in PiP, if one is selected.
    pub pip_target: Option<ShareId>,
    /// Fetch this peer's own self-view frame for the main stage while sharing.
    pub local: bool,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // channel messages, not stored in bulk
pub enum Command {
    Connect,
    Disconnect,
    CreateRoom,
    JoinRoom(String),
    LeaveRoom,
    DeleteRoom,
    ListDevices,
    ListSources,
    StartMic(Option<String>),
    StopMic,
    StartSystem(Option<String>),
    StopSystem,
    StartShare {
        source: CaptureSource,
        options: CaptureOptions,
        include_system_audio: bool,
    },
    StopShare,
    SetMicVolume(f32),
    SetPeerVolume {
        peer_id: String,
        volume: f64,
    },
    Poll(FrameRequest),
    ForgetRemote(String),
    PollLatency,
    PollHealth,
    SaveConfig(Config),
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // channel messages, not stored in bulk
pub enum UiEvent {
    Error(String),
    ConnectionFailed(String),
    Config(Config),
    Connected(bool),
    Call(CallEvent),
    Devices {
        microphones: Vec<Device>,
        outputs: Vec<Device>,
        monitors: Vec<Device>,
    },
    Sources(Vec<CaptureSource>),
    Mic(bool),
    System(bool),
    Sharing(bool),
    /// Latest RTT measurements for the active peer connections, in ms.
    Latency(Vec<(String, f64)>),
    EncoderNotice(String),
    Frame {
        kind: FrameKind,
        /// Already-decoded BGRA pixels; the GPUI thread only wraps them.
        frame: Option<Arc<RenderImage>>,
    },
}

/// Handle the UI uses to issue commands.
#[derive(Clone)]
pub struct Backend {
    tx: mpsc::UnboundedSender<Command>,
}

impl Backend {
    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

/// Start the backend on its own thread with its own Tokio runtime. Media
/// engines and the call session stay on that thread; only plain data crosses
/// the channel.
pub fn spawn(store: ConfigStore) -> (Backend, mpsc::UnboundedReceiver<UiEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("sharescreen-backend".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build backend runtime");
            runtime.block_on(async move {
                let mut actor = Actor::new(store, ui_tx);
                let mut rx = rx;
                while let Some(command) = rx.recv().await {
                    actor.handle(command).await;
                }
            });
        })
        .expect("failed to spawn backend thread");
    (Backend { tx }, ui_rx)
}

/// What is being shared, so monitor hotplug can restart capture.
#[derive(Clone)]
struct ShareTarget {
    source: CaptureSource,
    options: CaptureOptions,
}

struct Actor {
    store: Arc<StdMutex<ConfigStore>>,
    ui: mpsc::UnboundedSender<UiEvent>,
    capture: CaptureEngine,
    audio: AudioEngine,
    call: Option<CallSession>,
    share: Option<ShareTarget>,
    mic_running: bool,
    /// Last hash sent per frame stream. `None` means nothing sent yet.
    frames: HashMap<FrameKind, Option<u64>>,
    last_hotplug_check: std::time::Instant,
    device_enumeration: Arc<AtomicBool>,
    source_enumeration: Arc<AtomicBool>,
    latency_inflight: Arc<AtomicBool>,
    latency_generation: Arc<AtomicU64>,
    session_generation: Arc<AtomicU64>,
}

impl Actor {
    fn new(store: ConfigStore, ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        let config = store.config();
        let capture = CaptureEngine::new(config.capture.backend)
            .or_else(|_| CaptureEngine::new(CaptureBackend::Auto))
            .expect("no capture backend available");
        let mut audio = AudioEngine::default();
        audio.set_bitrate_kbps(config.audio.bitrate_kbps);
        audio.set_processing_options(
            config.audio.noise_suppression,
            noise_suppression_level_id(config.audio.noise_suppression_level),
            config.audio.echo_cancellation,
        );
        Self {
            store: Arc::new(StdMutex::new(store)),
            ui,
            capture,
            audio,
            call: None,
            share: None,
            mic_running: false,
            frames: HashMap::new(),
            last_hotplug_check: std::time::Instant::now(),
            device_enumeration: Arc::new(AtomicBool::new(false)),
            source_enumeration: Arc::new(AtomicBool::new(false)),
            latency_inflight: Arc::new(AtomicBool::new(false)),
            latency_generation: Arc::new(AtomicU64::new(0)),
            session_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    fn send(&self, event: UiEvent) {
        let _ = self.ui.send(event);
    }

    fn config(&self) -> Config {
        self.store
            .lock()
            .expect("config store poisoned")
            .config()
            .clone()
    }

    async fn handle(&mut self, command: Command) {
        tracing::debug!(?command, "backend command");
        let result = match command {
            Command::Connect => match self.connect().await {
                Ok(()) => Ok(()),
                Err(error) => {
                    self.send(UiEvent::ConnectionFailed(error));
                    Ok(())
                }
            },
            Command::Disconnect => self.disconnect().await,
            Command::CreateRoom => self.create_room(),
            Command::JoinRoom(room) => self.join_room(room),
            Command::LeaveRoom => self.leave_room().await,
            Command::DeleteRoom => self.delete_room().await,
            Command::ListDevices => self.list_devices(),
            Command::ListSources => self.list_sources(),
            Command::StartMic(device) => self.start_mic(device).await,
            Command::StopMic => self.stop_mic().await,
            Command::StartSystem(device) => self.start_system(device).await,
            Command::StopSystem => self.stop_system().await,
            Command::StartShare {
                source,
                options,
                include_system_audio,
            } => {
                self.start_share(source, options, include_system_audio)
                    .await
            }
            Command::StopShare => self.stop_share().await,
            Command::SetMicVolume(volume) => {
                self.audio.set_mic_volume(volume).map_err(|e| e.to_string())
            }
            Command::SetPeerVolume { peer_id, volume } => {
                if let Some(call) = &self.call {
                    call.set_peer_volume(&peer_id, volume);
                }
                Ok(())
            }
            Command::Poll(request) => {
                self.check_hotplug().await;
                self.poll_frames(request);
                Ok(())
            }
            Command::ForgetRemote(peer) => {
                self.frames.remove(&FrameKind::Remote(peer));
                Ok(())
            }
            Command::PollLatency => {
                self.poll_latency();
                Ok(())
            }
            Command::PollHealth => self.check_media_health().await,
            Command::SaveConfig(config) => self.save_config(config),
        };

        if let Err(error) = result {
            tracing::warn!("{error}");
            self.send(UiEvent::Error(error));
        }
    }

    fn poll_latency(&self) {
        let Some(call) = &self.call else { return };
        if self.latency_inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        let query = call.peer_round_trip_times();
        let flag = self.latency_inflight.clone();
        let generation = self.latency_generation.clone();
        let current = generation.load(Ordering::Acquire);
        let ui = self.ui.clone();
        tokio::spawn(async move {
            let _guard = EnumerationGuard(flag);
            let values = query.await;
            if generation.load(Ordering::Acquire) == current {
                let _ = ui.send(UiEvent::Latency(values));
            }
        });
    }

    async fn check_media_health(&mut self) -> Result<(), String> {
        if let Some(notice) = self.capture.take_notice() {
            self.send(UiEvent::EncoderNotice(notice));
        }
        for (kind, error) in self.audio.take_failures() {
            match kind {
                DeviceKind::Microphone => {
                    self.mic_running = false;
                    if let Some(call) = &self.call {
                        call.set_mic_on(false);
                    }
                    self.send(UiEvent::Mic(false));
                    self.send(UiEvent::Error(format!("Microphone stopped: {error}")));
                }
                DeviceKind::AudioOutput => {
                    self.send(UiEvent::System(false));
                    self.send(UiEvent::Error(format!("System audio stopped: {error}")));
                }
                _ => {}
            }
        }
        if self.share.is_some() && self.capture.finished() {
            let error = self
                .capture
                .failure()
                .unwrap_or_else(|| "worker stopped unexpectedly; see the log for details".into());
            self.stop_share().await?;
            self.send(UiEvent::Error(format!("Screen capture stopped: {error}")));
        }
        Ok(())
    }

    fn media_sink(&self, stream: Stream) -> mpsc::Sender<EncodedSample> {
        match &self.call {
            Some(session) => match stream {
                Stream::Mic => session.mic_sender(),
                Stream::System => session.system_sender(),
                Stream::Screen => session.screen_sender(),
            },
            None => {
                let (tx, mut rx) = mpsc::channel(64);
                tokio::spawn(async move { while rx.recv().await.is_some() {} });
                tx
            }
        }
    }

    async fn connect(&mut self) -> Result<(), String> {
        if self.call.is_some() {
            return Ok(());
        }
        let config = self.config();
        let peer_id = network::generate_peer_id();
        let call_config = CallConfig {
            self_id: peer_id.clone(),
            self_name: config.state.display_name.clone(),
            signaling_url: network::signaling_url_with_identity(
                &config.network,
                &peer_id,
                &config.state.display_name,
            ),
            peer_config: network::peer_config(&config.network),
            video_codec: config.video.codec,
        };

        let (tx, mut rx) = mpsc::unbounded_channel::<CallEvent>();
        let ui = self.ui.clone();
        let store = self.store.clone();
        let generation = self.session_generation.clone();
        let current = generation.fetch_add(1, Ordering::AcqRel) + 1;
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if generation.load(Ordering::Acquire) != current {
                    break;
                }
                if let CallEvent::RoomCreated { room_id } | CallEvent::RoomJoined { room_id } =
                    &event
                    && let Ok(mut store) = store.lock()
                {
                    store.config_mut().record_joined_room(room_id.clone());
                    let _ = store.save();
                    let _ = ui.send(UiEvent::Config(store.config().clone()));
                }
                let _ = ui.send(UiEvent::Call(event));
            }
        });

        tracing::info!(
            url = %call_config.signaling_url,
            "connecting to signaling server"
        );
        let session = CallSession::start(call_config, tx).await.map_err(|e| {
            tracing::warn!("signaling connect failed: {e}");
            e.to_string()
        })?;
        tracing::info!("connected to signaling server");
        self.call = Some(session);
        self.send(UiEvent::Connected(true));

        if let Ok(mut store) = self.store.lock() {
            store.config_mut().record_successful_server(
                config.network.signaling_host.clone(),
                config.network.signaling_port,
                config.network.signaling_secure,
            );
            let _ = store.save();
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), String> {
        self.frames.clear();
        self.session_generation.fetch_add(1, Ordering::AcqRel);
        self.latency_generation.fetch_add(1, Ordering::AcqRel);
        if self.share.is_some() {
            self.stop_share().await?;
        }
        self.stop_mic().await?;
        self.stop_system().await?;
        if let Some(session) = self.call.as_ref() {
            session.disconnect().await;
        }
        self.call = None;
        self.share = None;
        self.send(UiEvent::Connected(false));
        Ok(())
    }

    fn create_room(&self) -> Result<(), String> {
        let session = self
            .call
            .as_ref()
            .ok_or("not connected to a signaling server")?;
        tracing::info!("creating room");
        session.create_room().map_err(|e| e.to_string())
    }

    fn join_room(&self, room_id: String) -> Result<(), String> {
        let session = self
            .call
            .as_ref()
            .ok_or("not connected to a signaling server")?;
        tracing::info!(%room_id, "joining room");
        session.join_room(room_id).map_err(|e| e.to_string())
    }

    async fn leave_room(&mut self) -> Result<(), String> {
        self.frames.clear();
        self.latency_generation.fetch_add(1, Ordering::AcqRel);
        let session = self
            .call
            .as_ref()
            .ok_or("not connected to a signaling server")?;
        session.leave_room().await.map_err(|e| e.to_string())
    }

    async fn delete_room(&mut self) -> Result<(), String> {
        self.frames.clear();
        self.latency_generation.fetch_add(1, Ordering::AcqRel);
        let session = self
            .call
            .as_ref()
            .ok_or("not connected to a signaling server")?;
        session.delete_room().await.map_err(|e| e.to_string())
    }

    fn list_devices(&self) -> Result<(), String> {
        if self.device_enumeration.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let flag = self.device_enumeration.clone();
        let ui = self.ui.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = EnumerationGuard(flag);
            let audio = AudioEngine::default();
            let _ = ui.send(UiEvent::Devices {
                microphones: audio.list_microphones(),
                outputs: audio.list_outputs(),
                monitors: audio.list_system_monitors(),
            });
        });
        Ok(())
    }

    fn list_sources(&self) -> Result<(), String> {
        if self.source_enumeration.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let flag = self.source_enumeration.clone();
        let ui = self.ui.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = EnumerationGuard(flag);
            match CaptureEngine::new(CaptureBackend::Auto) {
                Ok(capture) => {
                    let _ = ui.send(UiEvent::Sources(capture.list_sources()));
                }
                Err(error) => {
                    let _ = ui.send(UiEvent::Error(error.to_string()));
                }
            }
        });
        Ok(())
    }

    async fn start_mic(&mut self, device: Option<String>) -> Result<(), String> {
        let settings = self.config().audio;
        self.audio.set_processing_options(
            settings.noise_suppression,
            noise_suppression_level_id(settings.noise_suppression_level),
            settings.echo_cancellation,
        );
        let sink = self.media_sink(Stream::Mic);
        self.audio
            .start_mic(device.as_deref(), sink)
            .await
            .map_err(|e| e.to_string())?;
        self.mic_running = true;
        let volume = self.config().audio.mic_volume;
        let _ = self.audio.set_mic_volume(volume);
        if let Some(call) = &self.call {
            call.set_mic_on(true);
        }
        self.send(UiEvent::Mic(true));
        Ok(())
    }

    async fn stop_mic(&mut self) -> Result<(), String> {
        self.audio.stop_mic().await.map_err(|e| e.to_string())?;
        self.mic_running = false;
        if let Some(call) = &self.call {
            call.set_mic_on(false);
        }
        self.send(UiEvent::Mic(false));
        Ok(())
    }

    async fn start_system(&mut self, device: Option<String>) -> Result<(), String> {
        let sink = self.media_sink(Stream::System);
        self.audio
            .start_system(device.as_deref(), sink)
            .await
            .map_err(|e| e.to_string())?;
        let volume = self.config().audio.system_volume;
        let _ = self.audio.set_system_volume(volume);
        self.send(UiEvent::System(true));
        Ok(())
    }

    async fn stop_system(&mut self) -> Result<(), String> {
        self.audio.stop_system().await.map_err(|e| e.to_string())?;
        self.send(UiEvent::System(false));
        Ok(())
    }

    async fn start_share(
        &mut self,
        source: CaptureSource,
        options: CaptureOptions,
        include_system_audio: bool,
    ) -> Result<(), String> {
        if !(64..=50_000).contains(&options.bitrate_kbps) {
            return Err("video bitrate must be between 64 and 50000 kbps".into());
        }
        let sink = self.media_sink(Stream::Screen);
        let mut options = options;
        if let Some(call) = &self.call {
            let session_codec = call.video_codec();
            if options.codec != session_codec {
                tracing::warn!(
                    requested = ?options.codec,
                    session = ?session_codec,
                    "screen share codec overridden by the negotiated session codec"
                );
            }
            options.codec = session_codec;
        }
        tracing::info!(
            "starting screen share: source={} resolution={}x{} fps={} bitrate={} kbps",
            source.id,
            options.width,
            options.height,
            options.fps,
            options.bitrate_kbps
        );
        self.capture
            .start(source.clone(), options.clone(), sink)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!("screen capture started: source={}", source.id);
        let mut config = self.config();
        config.video.width = options.width;
        config.video.height = options.height;
        config.video.max_fps = options.fps;
        config.video.max_bitrate_kbps = options.bitrate_kbps;
        config.video.hardware_encoding = options.hardware_encoding;
        config.capture.last_monitor_id = Some(source.id.clone());
        let _ = self.save_config(config);
        self.share = Some(ShareTarget { source, options });
        if let Some(call) = &self.call {
            call.set_screen_sharing(true);
        }
        // Force the next self-view frame through even if it matches the last one
        // sent before a previous share.
        self.frames.remove(&FrameKind::Local);
        self.send(UiEvent::Sharing(true));

        if include_system_audio {
            match self.audio.list_system_monitors().first() {
                Some(monitor) => {
                    let monitor = monitor.id.clone();
                    if let Err(error) = self.start_system(Some(monitor)).await {
                        tracing::warn!("failed to start system audio with the share: {error}");
                        self.send(UiEvent::Error(error));
                    }
                }
                None => tracing::warn!(
                    "system audio requested for the share, but no audio output device is available"
                ),
            }
        }
        Ok(())
    }

    async fn stop_share(&mut self) -> Result<(), String> {
        self.share = None;
        self.capture.stop().await.map_err(|e| e.to_string())?;
        if let Some(call) = &self.call {
            call.set_screen_sharing(false);
        }
        self.frames.remove(&FrameKind::Local);
        self.send(UiEvent::Sharing(false));
        Ok(())
    }

    /// Follow monitor hotplug: when the shared source disappears, restart
    /// capture on the first remaining source so the share does not go black.
    async fn check_hotplug(&mut self) {
        if self.share.is_none()
            || self.last_hotplug_check.elapsed() < std::time::Duration::from_secs(3)
        {
            return;
        }
        self.last_hotplug_check = std::time::Instant::now();
        let Some(target) = self.share.clone() else {
            return;
        };
        let sources = self.capture.list_sources();
        if sources.iter().any(|source| source.id == target.source.id) {
            return;
        }
        let Some(next) = sources.into_iter().next() else {
            return;
        };
        let sink = self.media_sink(Stream::Screen);
        if self
            .capture
            .start(next.clone(), target.options.clone(), sink)
            .await
            .is_ok()
        {
            self.share = Some(ShareTarget {
                source: next,
                options: target.options,
            });
            tracing::info!("shared source disappeared, moved capture to a remaining source");
        }
    }

    fn poll_frames(&mut self, request: FrameRequest) {
        if request.remote {
            self.frames.retain(|kind, _| match kind {
                FrameKind::Remote(id) => request.peers.contains(id),
                _ => true,
            });
            for peer in &request.peers {
                let frame = self.call.as_ref().and_then(|call| call.remote_frame(peer));
                self.emit_frame(FrameKind::Remote(peer.clone()), frame);
            }
        }

        if request.pip {
            let frame = match &request.pip_target {
                Some(ShareId::Remote(peer)) => {
                    self.call.as_ref().and_then(|call| call.remote_frame(peer))
                }
                Some(ShareId::Local) => self.local_frame(),
                None => None,
            };
            self.emit_frame(FrameKind::Pip, frame);
        }

        if request.local {
            let frame = self.local_frame();
            self.emit_frame(FrameKind::Local, frame);
        }
    }

    /// The frame for our own share. Inside a call this is the outgoing encoded
    /// stream decoded again, so the self-view matches what peers receive; the
    /// capture-side preview is only a fallback for a share with no call
    /// session.
    fn local_frame(&self) -> Option<Vec<u8>> {
        match &self.call {
            Some(call) => call.local_preview(),
            None => self.capture.latest_preview(),
        }
    }

    fn emit_frame(&mut self, kind: FrameKind, jpeg: Option<Vec<u8>>) {
        let hash = jpeg.as_ref().map(|bytes| {
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            hasher.finish()
        });
        if self.frames.get(&kind) == Some(&hash) {
            return;
        }
        self.frames.insert(kind.clone(), hash);
        // JPEG decoding and the RGBA->BGRA pass run on the backend thread, not
        // the GPUI thread. Only a cheap `RenderImage` wrapper crosses over.
        let frame = decode_frame(jpeg);
        self.send(UiEvent::Frame { kind, frame });
    }

    fn save_config(&mut self, config: Config) -> Result<(), String> {
        config.validate().map_err(|e| e.to_string())?;
        let mut store = self.store.lock().map_err(|_| "config store poisoned")?;
        *store.config_mut() = config;
        store.save().map_err(|e| e.to_string())?;
        let saved = store.config().clone();
        drop(store);
        let _ = self.audio.set_mic_volume(saved.audio.mic_volume);
        let _ = self.audio.set_system_volume(saved.audio.system_volume);
        self.audio.set_processing_options(
            saved.audio.noise_suppression,
            noise_suppression_level_id(saved.audio.noise_suppression_level),
            saved.audio.echo_cancellation,
        );
        self.send(UiEvent::Config(saved));
        Ok(())
    }
}

/// Local media stream selector.
#[derive(Clone, Copy, Debug)]
enum Stream {
    Mic,
    System,
    Screen,
}

fn noise_suppression_level_id(level: NoiseSuppressionLevel) -> u8 {
    match level {
        NoiseSuppressionLevel::Low => 0,
        NoiseSuppressionLevel::Moderate => 1,
        NoiseSuppressionLevel::High => 2,
        NoiseSuppressionLevel::VeryHigh => 3,
    }
}

struct EnumerationGuard(Arc<AtomicBool>);

impl Drop for EnumerationGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Decode a JPEG into the BGRA buffer GPUI renders.
///
/// This runs on the backend actor's Tokio thread so the UI thread never does
/// image decoding or a per-pixel channel swap.
fn decode_frame(jpeg: Option<Vec<u8>>) -> Option<Arc<RenderImage>> {
    let jpeg = jpeg?;
    let decoded = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .into_rgba8();
    let mut buffer = decoded;
    // GPUI expects BGRA.
    for pixel in buffer.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_guard_releases_after_panic() {
        let flag = Arc::new(AtomicBool::new(true));
        let child = flag.clone();
        let _ = std::panic::catch_unwind(|| {
            let _guard = EnumerationGuard(child);
            panic!("enumerator failure");
        });
        assert!(!flag.load(Ordering::Acquire));
    }

    #[test]
    fn frame_payload_is_decoded_off_the_ui_thread() {
        let rgba = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();

        let frame = decode_frame(Some(jpeg)).expect("decoded frame");
        let bytes = frame.as_bytes(0).expect("frame pixels");
        assert_eq!(bytes.len(), 2 * 2 * 4, "BGRA buffer size");
        // The channel swap ran in the backend: solid blue-ish input arrives as
        // BGRA, and the alpha channel is opaque.
        assert_eq!(bytes[3], 255);
        assert!(bytes[0] > bytes[2], "expected BGR order");
    }

    #[test]
    fn preview_poll_does_not_clear_remote_frame() {
        let path = std::env::temp_dir().join(format!(
            "sharescreen-preview-test-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = ConfigStore::load_from_path(path.clone()).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut actor = Actor::new(store, tx);
        let remote = FrameKind::Remote("peer".into());
        actor.frames.insert(remote.clone(), Some(42));
        actor.poll_frames(FrameRequest {
            remote: false,
            local: true,
            ..Default::default()
        });
        assert_eq!(actor.frames.get(&remote), Some(&Some(42)));
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(
                event,
                UiEvent::Frame {
                    kind: FrameKind::Remote(_),
                    ..
                }
            ));
        }
        actor.emit_frame(FrameKind::Remote("a".into()), Some(vec![1]));
        actor.emit_frame(FrameKind::Remote("b".into()), Some(vec![1]));
        let mut delivered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let UiEvent::Frame {
                kind: FrameKind::Remote(id),
                ..
            } = event
            {
                delivered.push(id);
            }
        }
        assert_eq!(delivered, ["a", "b"]);
        actor.poll_frames(FrameRequest {
            remote: true,
            ..Default::default()
        });
        assert!(
            !actor
                .frames
                .keys()
                .any(|key| matches!(key, FrameKind::Remote(_)))
        );
        std::fs::remove_file(path).unwrap();
    }
}
