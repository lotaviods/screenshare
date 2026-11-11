//! Backend actor: owns capture/audio/call state and communicates with the GPUI
//! thread through channels. The UI never blocks on media or network work.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex as StdMutex};

use sharescreen_audio::AudioEngine;
use sharescreen_call::{CallConfig, CallSession};
use sharescreen_capture::CaptureEngine;
use sharescreen_config::{Config, ConfigStore};
use sharescreen_core::{
    AudioCapture, CallEvent, CaptureBackend, CaptureOptions, CaptureSource, Device, EncodedSample,
    ScreenCapture,
};
use sharescreen_network as network;
use tokio::sync::mpsc;

/// Which frame stream an update belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Remote,
    Pip,
}

impl FrameKind {
    fn index(self) -> usize {
        match self {
            Self::Remote => 0,
            Self::Pip => 1,
        }
    }
}

/// What the UI wants each poll cycle.
#[derive(Clone, Debug, Default)]
pub struct FrameRequest {
    /// Remote participant whose decoded frame to fetch.
    pub peer: Option<String>,
    /// Fetch the best frame for the picture-in-picture window.
    pub pip: bool,
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
    SaveConfig(Config),
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // channel messages, not stored in bulk
pub enum UiEvent {
    Error(String),
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
    Frame {
        kind: FrameKind,
        jpeg: Option<Vec<u8>>,
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
    frames: [Option<Option<u64>>; 2],
    last_hotplug_check: std::time::Instant,
}

impl Actor {
    fn new(store: ConfigStore, ui: mpsc::UnboundedSender<UiEvent>) -> Self {
        let config = store.config();
        let capture = CaptureEngine::new(config.capture.backend)
            .or_else(|_| CaptureEngine::new(CaptureBackend::Auto))
            .expect("no capture backend available");
        let mut audio = AudioEngine::default();
        audio.set_bitrate_kbps(config.audio.bitrate_kbps);
        Self {
            store: Arc::new(StdMutex::new(store)),
            ui,
            capture,
            audio,
            call: None,
            share: None,
            mic_running: false,
            frames: [None; 2],
            last_hotplug_check: std::time::Instant::now(),
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
            Command::Connect => self.connect().await,
            Command::Disconnect => self.disconnect().await,
            Command::CreateRoom => self.create_room(),
            Command::JoinRoom(room) => self.join_room(room),
            Command::LeaveRoom => self.leave_room().await,
            Command::DeleteRoom => self.delete_room().await,
            Command::ListDevices => self.list_devices(),
            Command::ListSources => {
                self.send(UiEvent::Sources(self.capture.list_sources()));
                Ok(())
            }
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
            Command::SaveConfig(config) => self.save_config(config),
        };

        if let Err(error) = result {
            tracing::warn!("{error}");
            self.send(UiEvent::Error(error));
        }
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
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
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
        let session = self
            .call
            .as_ref()
            .ok_or("not connected to a signaling server")?;
        session.leave_room().await.map_err(|e| e.to_string())
    }

    async fn delete_room(&mut self) -> Result<(), String> {
        let session = self
            .call
            .as_ref()
            .ok_or("not connected to a signaling server")?;
        session.delete_room().await.map_err(|e| e.to_string())
    }

    fn list_devices(&self) -> Result<(), String> {
        self.send(UiEvent::Devices {
            microphones: self.audio.list_microphones(),
            outputs: self.audio.list_outputs(),
            monitors: self.audio.list_system_monitors(),
        });
        Ok(())
    }

    async fn start_mic(&mut self, device: Option<String>) -> Result<(), String> {
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
            options.codec = call.video_codec();
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
        self.send(UiEvent::Sharing(true));

        if include_system_audio {
            let monitor = self
                .audio
                .list_system_monitors()
                .first()
                .map(|d| d.id.clone());
            if let Err(error) = self.start_system(monitor).await {
                tracing::warn!("failed to start system audio with the share: {error}");
                self.send(UiEvent::Error(error));
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
        let remote = request
            .peer
            .as_ref()
            .and_then(|peer| self.call.as_ref()?.remote_frame(peer));
        self.emit_frame(FrameKind::Remote, remote);

        if request.pip {
            let frame = self
                .call
                .as_ref()
                .and_then(|call| call.shared_screen_frame())
                .or_else(|| self.capture.latest_preview());
            self.emit_frame(FrameKind::Pip, frame);
        }
    }

    fn emit_frame(&mut self, kind: FrameKind, jpeg: Option<Vec<u8>>) {
        let hash = jpeg.as_ref().map(|bytes| {
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            hasher.finish()
        });
        let slot = &mut self.frames[kind.index()];
        if slot.as_ref() == Some(&hash) {
            return;
        }
        *slot = Some(hash);
        self.send(UiEvent::Frame { kind, jpeg });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_kind_indexes_are_distinct() {
        assert_eq!(FrameKind::Remote.index(), 0);
        assert_eq!(FrameKind::Pip.index(), 1);
    }
}
