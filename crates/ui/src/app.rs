//! The GPUI application: screens, state and rendering.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Div, ElementId, Entity, FontWeight,
    ObjectFit, Render, RenderImage, SharedString, Stateful, StatefulInteractiveElement, WeakEntity,
    Window, WindowHandle, WindowKind, WindowOptions, div, img, prelude::*, px, rgb, size,
};
use sharescreen_config::{Config, ConfigStore};
use sharescreen_core::{
    CallEvent, CaptureBackend, CaptureOptions, CaptureSource, Codec, ConnectionMode, CursorMode,
    Device, Participant,
};

use crate::backend::{self, Backend, Command, FrameKind, FrameRequest, UiEvent};
use crate::text_input::TextInput;

const BG: u32 = 0x0d0d12;
const PANEL: u32 = 0x17171f;
const PANEL_ALT: u32 = 0x1f1f2b;
const BORDER: u32 = 0x2d2d3d;
const TEXT: u32 = 0xe8e8f0;
const MUTED: u32 = 0x9a9aac;
const ACCENT: u32 = 0x6c6cf0;
const DANGER: u32 = 0xe05561;
const OK: u32 = 0x46c18a;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Call,
    Settings,
}

/// Share dialog choices, seeded from the saved video settings.
#[derive(Clone)]
struct ShareDraft {
    source: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    hardware: bool,
    system_audio: bool,
}

impl ShareDraft {
    fn from_config(config: &Config) -> Self {
        Self {
            source: None,
            width: config.video.width,
            height: config.video.height,
            fps: config.video.max_fps,
            bitrate: config.video.max_bitrate_kbps,
            hardware: config.video.hardware_encoding,
            system_audio: config.capture.system_audio,
        }
    }
}

pub struct AppView {
    config: Config,
    backend: Backend,
    screen: Screen,
    connected: bool,
    room_id: Option<String>,
    participants: Vec<Participant>,
    error: Option<String>,
    mic_on: bool,
    system_on: bool,
    sharing: bool,
    peer_volumes: HashMap<String, f64>,

    name_input: Entity<TextInput>,
    host_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    room_input: Entity<TextInput>,

    settings: Config,
    s_name: Entity<TextInput>,
    s_host: Entity<TextInput>,
    s_port: Entity<TextInput>,
    s_stun: Entity<TextInput>,
    s_turn: Entity<TextInput>,
    s_width: Entity<TextInput>,
    s_height: Entity<TextInput>,
    s_fps: Entity<TextInput>,
    s_bitrate: Entity<TextInput>,

    share_open: bool,
    share: ShareDraft,
    sources: Vec<CaptureSource>,
    microphones: Vec<Device>,
    outputs: Vec<Device>,
    monitors: Vec<Device>,

    remote_frame: Option<Arc<RenderImage>>,
    pip_window: Option<WindowHandle<PipView>>,
    pip_entity: Option<Entity<PipView>>,
}

impl AppView {
    pub fn new(store: ConfigStore, cx: &mut Context<Self>) -> Self {
        let config = store.config().clone();
        let name = config.state.display_name.clone();
        let host = config.network.signaling_host.clone();
        let port = config.network.signaling_port.to_string();
        let (backend, mut events) = backend::spawn(store);

        let view = Self {
            settings: config.clone(),
            share: ShareDraft::from_config(&config),
            config,
            backend,
            screen: Screen::Home,
            connected: false,
            room_id: None,
            participants: Vec::new(),
            error: None,
            mic_on: false,
            system_on: false,
            sharing: false,
            peer_volumes: HashMap::new(),
            name_input: cx.new(|cx| TextInput::new(name.clone(), "Name others will see", cx)),
            host_input: cx.new(|cx| TextInput::new(host.clone(), "127.0.0.1", cx)),
            port_input: cx.new(|cx| TextInput::new(port.clone(), "8080", cx)),
            room_input: cx.new(|cx| TextInput::new("", "paste a room id", cx)),
            s_name: cx.new(|cx| TextInput::new(name, "", cx)),
            s_host: cx.new(|cx| TextInput::new(host, "", cx)),
            s_port: cx.new(|cx| TextInput::new(port, "", cx)),
            s_stun: cx.new(|cx| TextInput::new("", "", cx)),
            s_turn: cx.new(|cx| TextInput::new("", "", cx)),
            s_width: cx.new(|cx| TextInput::new("", "", cx)),
            s_height: cx.new(|cx| TextInput::new("", "", cx)),
            s_fps: cx.new(|cx| TextInput::new("", "", cx)),
            s_bitrate: cx.new(|cx| TextInput::new("", "", cx)),
            share_open: false,
            sources: Vec::new(),
            microphones: Vec::new(),
            outputs: Vec::new(),
            monitors: Vec::new(),
            remote_frame: None,
            pip_window: None,
            pip_entity: None,
        };

        view.backend.send(Command::ListDevices);

        cx.spawn(async move |_this, cx| {
            while let Some(event) = events.recv().await {
                if _this
                    .update(cx, |view, cx| view.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        view.start_frame_poll(cx);
        view
    }

    fn start_frame_poll(&self, cx: &mut Context<Self>) {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            loop {
                if let Ok(Some(request)) = this.update(cx, |view, cx| view.frame_request(cx)) {
                    backend.send(Command::Poll(request));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    fn frame_request(&mut self, cx: &mut Context<Self>) -> Option<FrameRequest> {
        if let Some(handle) = self.pip_window
            && handle.update(cx, |_, _, _| ()).is_err()
        {
            self.pip_window = None;
            self.pip_entity = None;
        }
        let peer = if self.screen == Screen::Call {
            self.participants
                .iter()
                .find(|participant| participant.screen_on)
                .map(|participant| participant.id.clone())
        } else {
            None
        };
        if peer.is_none() && self.pip_window.is_none() {
            return None;
        }
        Some(FrameRequest {
            peer,
            pip: self.pip_window.is_some(),
        })
    }

    fn handle_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Error(message) => self.error = Some(message),
            UiEvent::Config(config) => self.config = config,
            UiEvent::Connected(connected) => {
                self.connected = connected;
                if !connected {
                    self.screen = Screen::Home;
                    self.room_id = None;
                    self.participants.clear();
                    self.remote_frame = None;
                }
            }
            UiEvent::Call(event) => self.handle_call_event(event),
            UiEvent::Devices {
                microphones,
                outputs,
                monitors,
            } => {
                self.microphones = microphones;
                self.outputs = outputs;
                self.monitors = monitors;
            }
            UiEvent::Sources(sources) => {
                self.sources = sources;
                if self.share.source.is_none() {
                    self.share.source = self.sources.first().map(|source| source.id.clone());
                }
            }
            UiEvent::Mic(on) => self.mic_on = on,
            UiEvent::System(on) => self.system_on = on,
            UiEvent::Sharing(on) => self.sharing = on,
            UiEvent::Frame { kind, jpeg } => match kind {
                FrameKind::Remote => self.remote_frame = decode_frame(jpeg),
                FrameKind::Pip => {
                    let frame = decode_frame(jpeg);
                    if let Some(pip) = &self.pip_entity {
                        pip.update(cx, |pip, cx| {
                            pip.frame = frame;
                            cx.notify();
                        });
                    }
                }
            },
        }
        cx.notify();
    }

    fn handle_call_event(&mut self, event: CallEvent) {
        match event {
            CallEvent::RoomCreated { room_id } | CallEvent::RoomJoined { room_id } => {
                self.room_id = Some(room_id);
                self.screen = Screen::Call;
            }
            CallEvent::RoomDeleted { .. } => {
                self.room_id = None;
                self.participants.clear();
                self.remote_frame = None;
                self.screen = Screen::Home;
            }
            CallEvent::ParticipantJoined { participant }
            | CallEvent::ParticipantUpdated { participant } => {
                match self
                    .participants
                    .iter()
                    .position(|existing| existing.id == participant.id)
                {
                    Some(index) => self.participants[index] = participant,
                    None => self.participants.push(participant),
                }
            }
            CallEvent::ParticipantLeft { id } => {
                self.participants.retain(|participant| participant.id != id);
                self.peer_volumes.remove(&id);
            }
            CallEvent::ConnectionStateChanged { .. } => {}
            CallEvent::Error { message } => self.error = Some(message),
        }
    }

    // --- Home actions ------------------------------------------------------

    fn collect_home_config(&self, cx: &Context<Self>) -> Config {
        let mut config = self.config.clone();
        config.state.display_name = self.name_input.read(cx).value().trim().to_string();
        config.network.signaling_host = self.host_input.read(cx).value().trim().to_string();
        config.network.signaling_port =
            self.port_input.read(cx).value().trim().parse().unwrap_or(0);
        config
    }

    fn prepare_home_config(&mut self, cx: &mut Context<Self>) -> Option<Config> {
        let config = self.collect_home_config(cx);
        if config.state.display_name.is_empty() {
            self.error = Some("display name must not be empty".into());
            return None;
        }
        if let Err(error) = config.validate() {
            self.error = Some(error.to_string());
            return None;
        }
        self.config = config.clone();
        self.backend.send(Command::SaveConfig(config.clone()));
        Some(config)
    }

    fn on_connect(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.prepare_home_config(cx).is_some() {
            self.error = None;
            self.backend.send(Command::Connect);
        }
    }

    fn on_disconnect(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Command::Disconnect);
        self.connected = false;
        self.room_id = None;
        self.participants.clear();
        self.remote_frame = None;
        self.screen = Screen::Home;
        cx.notify();
    }

    fn on_save_name(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(config) = self.prepare_home_config(cx) {
            let _ = config;
            self.error = None;
        }
    }

    fn on_create_room(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.prepare_home_config(cx).is_some() {
            self.backend.send(Command::CreateRoom);
        }
    }

    fn on_join_room(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let room = self.room_input.read(cx).value().trim().to_string();
        if room.is_empty() {
            self.error = Some("enter a room id first".into());
            cx.notify();
            return;
        }
        if self.prepare_home_config(cx).is_some() {
            self.backend.send(Command::JoinRoom(room));
        }
    }

    fn join_recent(&mut self, room: String, cx: &mut Context<Self>) {
        self.room_input
            .update(cx, |input, cx| input.set_value(room.clone(), cx));
        if self.prepare_home_config(cx).is_some() {
            self.backend.send(Command::JoinRoom(room));
        }
    }

    fn on_open_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(cx);
    }

    fn on_back_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.screen = if self.room_id.is_some() {
            Screen::Call
        } else {
            Screen::Home
        };
        cx.notify();
    }

    fn on_save_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let mut config = self.settings.clone();
        config.state.display_name = self.s_name.read(cx).value().trim().to_string();
        config.network.signaling_host = self.s_host.read(cx).value().trim().to_string();
        config.network.signaling_port = self.s_port.read(cx).value().trim().parse().unwrap_or(0);
        config.network.stun_servers = parse_stun(self.s_stun.read(cx).value());
        config.network.turn_servers = parse_turn(self.s_turn.read(cx).value());
        config.video.width = parse_u32(self.s_width.read(cx).value(), config.video.width);
        config.video.height = parse_u32(self.s_height.read(cx).value(), config.video.height);
        config.video.max_fps = parse_u32(self.s_fps.read(cx).value(), config.video.max_fps);
        config.video.max_bitrate_kbps = parse_u32(
            self.s_bitrate.read(cx).value(),
            config.video.max_bitrate_kbps,
        );
        if let Err(error) = config.validate() {
            self.error = Some(error.to_string());
            cx.notify();
            return;
        }
        self.settings = config.clone();
        self.config = config.clone();
        self.error = None;
        self.backend.send(Command::SaveConfig(config));
        self.screen = if self.room_id.is_some() {
            Screen::Call
        } else {
            Screen::Home
        };
        cx.notify();
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings = self.config.clone();
        let config = self.settings.clone();
        self.s_name.update(cx, |i, cx| {
            i.set_value(config.state.display_name.clone(), cx)
        });
        self.s_host.update(cx, |i, cx| {
            i.set_value(config.network.signaling_host.clone(), cx)
        });
        self.s_port.update(cx, |i, cx| {
            i.set_value(config.network.signaling_port.to_string(), cx)
        });
        let stun = config
            .network
            .stun_servers
            .iter()
            .map(|server| server.url.clone())
            .collect::<Vec<_>>()
            .join(", ");
        self.s_stun.update(cx, |i, cx| i.set_value(stun, cx));
        let turn = config
            .network
            .turn_servers
            .iter()
            .map(|server| match (&server.username, &server.credential) {
                (Some(user), Some(credential)) => {
                    format!("{}|{}|{}", server.url, user, credential)
                }
                (Some(user), None) => format!("{}|{}", server.url, user),
                _ => server.url.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.s_turn.update(cx, |i, cx| i.set_value(turn, cx));
        self.s_width
            .update(cx, |i, cx| i.set_value(config.video.width.to_string(), cx));
        self.s_height
            .update(cx, |i, cx| i.set_value(config.video.height.to_string(), cx));
        self.s_fps.update(cx, |i, cx| {
            i.set_value(config.video.max_fps.to_string(), cx)
        });
        self.s_bitrate.update(cx, |i, cx| {
            i.set_value(config.video.max_bitrate_kbps.to_string(), cx)
        });
        self.screen = Screen::Settings;
        cx.notify();
    }

    // --- Call actions ------------------------------------------------------

    fn on_toggle_mic(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.mic_on {
            self.backend.send(Command::StopMic);
        } else {
            self.backend
                .send(Command::StartMic(self.config.audio.mic_device_id.clone()));
        }
        cx.notify();
    }

    fn on_toggle_system(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.system_on {
            self.backend.send(Command::StopSystem);
        } else {
            let device = self.monitors.first().map(|device| device.id.clone());
            self.backend.send(Command::StartSystem(device));
        }
        cx.notify();
    }

    fn change_mic_volume(&mut self, delta: f32, cx: &mut Context<Self>) {
        let volume = (self.settings.audio.mic_volume + delta).clamp(0.0, 2.0);
        self.settings.audio.mic_volume = volume;
        self.config.audio.mic_volume = volume;
        self.backend.send(Command::SetMicVolume(volume));
        cx.notify();
    }

    fn change_peer_volume(&mut self, peer_id: String, delta: f64, cx: &mut Context<Self>) {
        let volume = self.peer_volumes.entry(peer_id.clone()).or_insert(1.0);
        *volume = (*volume + delta).clamp(0.0, 2.0);
        self.backend.send(Command::SetPeerVolume {
            peer_id,
            volume: *volume,
        });
        cx.notify();
    }

    fn on_leave(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Command::LeaveRoom);
        self.room_id = None;
        self.participants.clear();
        self.remote_frame = None;
        self.screen = Screen::Home;
        cx.notify();
    }

    fn on_delete_room(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Command::DeleteRoom);
        self.room_id = None;
        self.participants.clear();
        self.remote_frame = None;
        self.screen = Screen::Home;
        cx.notify();
    }

    fn on_copy_room(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(room) = &self.room_id {
            cx.write_to_clipboard(ClipboardItem::new_string(room.clone()));
        }
    }

    fn open_share(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.share = ShareDraft::from_config(&self.config);
        self.share_open = true;
        self.backend.send(Command::ListSources);
        self.backend.send(Command::ListDevices);
        cx.notify();
    }

    fn close_share(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.share_open = false;
        cx.notify();
    }

    fn on_start_share(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(source_id) = self.share.source.clone() else {
            self.error = Some("no capture source available".into());
            cx.notify();
            return;
        };
        let Some(source) = self
            .sources
            .iter()
            .find(|source| source.id == source_id)
            .cloned()
        else {
            self.error = Some("capture source disappeared".into());
            cx.notify();
            return;
        };
        let options = CaptureOptions {
            width: self.share.width,
            height: self.share.height,
            fps: self.share.fps,
            cursor: CursorMode::Embedded,
            codec: self.config.video.codec,
            bitrate_kbps: self.share.bitrate,
            hardware_encoding: self.share.hardware,
        };
        self.backend.send(Command::StartShare {
            source,
            options,
            include_system_audio: self.share.system_audio,
        });
        self.share_open = false;
        cx.notify();
    }

    fn on_stop_share(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.backend.send(Command::StopShare);
        cx.notify();
    }

    fn open_pip(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.pip_window.is_some() {
            return;
        }
        let app = cx.entity().downgrade();
        let pip = cx.new(|_| PipView { frame: None, app });
        let bounds = Bounds::centered(None, size(px(480.), px(270.)), cx);
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                    kind: WindowKind::PopUp,
                    is_resizable: true,
                    window_min_size: Some(size(px(320.), px(180.))),
                    ..Default::default()
                },
                |_, _| pip.clone(),
            )
            .ok();
        self.pip_entity = Some(pip);
        self.pip_window = handle;
        cx.notify();
    }

    // --- rendering ---------------------------------------------------------

    fn error_banner(&self, message: SharedString, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .bg(rgb(0x3a1a20))
            .border_b_1()
            .border_color(rgb(DANGER))
            .text_sm()
            .text_color(rgb(0xffb3bb))
            .child(div().flex_1().child(message))
            .child(
                div()
                    .id("dismiss-error")
                    .px_2()
                    .cursor(CursorStyle::PointingHand)
                    .text_color(rgb(MUTED))
                    .child("dismiss")
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.error = None;
                        cx.notify();
                    })),
            )
    }

    fn home(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = div()
            .id("home-scroll")
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .size_full()
            .overflow_y_scroll();

        page = page.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .size_10()
                                .rounded_lg()
                                .bg(rgb(ACCENT))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_lg()
                                .font_weight(FontWeight::BOLD)
                                .child("S"),
                        )
                        .child(
                            div().flex().flex_col().child(label("SHARESCREEN")).child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Share your screen, together."),
                            ),
                        ),
                )
                .child(
                    button("open-settings", "Settings")
                        .on_click(cx.listener(Self::on_open_settings)),
                ),
        );

        page = page.child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_5()
                .rounded_lg()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(label("YOUR DISPLAY NAME"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .items_center()
                        .child(div().flex_1().child(self.name_input.clone()))
                        .child(
                            button("save-name", "Save name")
                                .on_click(cx.listener(Self::on_save_name)),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("Used when you connect next time."),
                ),
        );

        let status = if self.connected {
            "Connected"
        } else {
            "Not connected"
        };
        page = page.child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_5()
                .rounded_lg()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div().flex().flex_col().child(label("CONNECTION")).child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Signaling server"),
                            ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(if self.connected { OK } else { MUTED }))
                                .child(status),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label("ADDRESS"))
                                .child(self.host_input.clone()),
                        )
                        .child(
                            div()
                                .w(px(130.))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label("PORT"))
                                .child(self.port_input.clone()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .child(
                            toggle(
                                "home-secure",
                                "Use secure WebSocket (wss://)",
                                self.config.network.signaling_secure,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.config.network.signaling_secure =
                                    !view.config.network.signaling_secure;
                                let config = view.config.clone();
                                view.backend.send(Command::SaveConfig(config));
                                cx.notify();
                            })),
                        )
                        .child(div().flex_1())
                        .child(if self.connected {
                            danger_button("disconnect", "Disconnect")
                                .on_click(cx.listener(Self::on_disconnect))
                        } else {
                            primary_button("connect", "Connect")
                                .on_click(cx.listener(Self::on_connect))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child(if self.connected {
                            format!(
                                "Connected to {}://{}:{}",
                                if self.config.network.signaling_secure {
                                    "wss"
                                } else {
                                    "ws"
                                },
                                self.config.network.signaling_host,
                                self.config.network.signaling_port
                            )
                        } else {
                            "Not connected to a signaling server.".to_string()
                        }),
                ),
        );

        let mut rooms = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .rounded_lg()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .child(label("GET TOGETHER"))
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child("Join or create a room"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .items_end()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label("ROOM ID"))
                            .child(self.room_input.clone()),
                    )
                    .child(primary_button("join", "Join").on_click(cx.listener(Self::on_join_room)))
                    .child(
                        button("create", "Create room").on_click(cx.listener(Self::on_create_room)),
                    ),
            );
        if !self.config.state.recent_rooms.is_empty() {
            rooms = rooms.child(label("RECENT ROOMS"));
            for room in self.config.state.recent_rooms.clone() {
                let id = room.clone();
                rooms = rooms.child(
                    div()
                        .id(SharedString::from(format!("recent-{room}")))
                        .cursor(CursorStyle::PointingHand)
                        .text_sm()
                        .text_color(rgb(0x9fa4ff))
                        .child(SharedString::from(room))
                        .on_click(
                            cx.listener(move |view, _, _, cx| view.join_recent(id.clone(), cx)),
                        ),
                );
            }
        }
        page = page.child(rooms);

        if !self.participants.is_empty() {
            page = page.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_5()
                    .rounded_lg()
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .child(div().text_lg().child("Participants"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(format!("{} online", self.participants.len())),
                            ),
                    )
                    .children(self.participants.iter().map(participant_row)),
            );
        }

        page
    }

    fn call(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let room = self.room_id.clone().unwrap_or_default();
        let mut sidebar = div()
            .flex()
            .flex_col()
            .gap_3()
            .w(px(300.))
            .h_full()
            .p_4()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(label("CURRENT ROOM"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(SharedString::from(room.clone())),
                    )
                    .child(button("copy-room", "Copy").on_click(cx.listener(Self::on_copy_room))),
            )
            .child(label("YOU"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(avatar(&self.config.state.display_name))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .child(self.config.state.display_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(if self.mic_on { "mic on" } else { "mic off" }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .child(div().text_sm().child("Participants"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(self.participants.len().to_string()),
                    ),
            );

        if self.participants.is_empty() {
            sidebar = sidebar.child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Waiting for others to join…"),
            );
        }
        for participant in &self.participants {
            let peer_id = participant.id.clone();
            let volume = self
                .peer_volumes
                .get(&participant.id)
                .copied()
                .unwrap_or(1.0);
            sidebar = sidebar.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(avatar(&participant.name))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .child(participant.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(format!("{:?}", participant.connection)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if participant.mic_on { OK } else { MUTED }))
                            .child(if participant.mic_on { "mic" } else { "muted" }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(mini_button("peer-down", "−").on_click(cx.listener({
                                let peer_id = peer_id.clone();
                                move |view, _, _, cx| {
                                    view.change_peer_volume(peer_id.clone(), -0.1, cx)
                                }
                            })))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(format!("{volume:.1}")),
                            )
                            .child(mini_button("peer-up", "+").on_click(cx.listener({
                                let peer_id = peer_id.clone();
                                move |view, _, _, cx| {
                                    view.change_peer_volume(peer_id.clone(), 0.1, cx)
                                }
                            }))),
                    ),
            );
        }

        sidebar = sidebar.child(div().flex_1()).child(
            button("call-settings", "Settings").on_click(cx.listener(Self::on_open_settings)),
        );

        let sharer = self
            .participants
            .iter()
            .find(|participant| participant.screen_on);
        let stage = match (&self.remote_frame, sharer) {
            (Some(frame), Some(participant)) => div()
                .relative()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(frame.clone())
                        .object_fit(ObjectFit::Contain)
                        .size_full(),
                )
                .child(
                    div()
                        .absolute()
                        .top_2()
                        .right_2()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xffffff))
                                .bg(rgb(0x000000))
                                .px_2()
                                .rounded_md()
                                .child(SharedString::from(participant.name.clone())),
                        )
                        .child(button("open-pip", "PiP").on_click(cx.listener(Self::open_pip))),
                ),
            _ => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(MUTED))
                .child(if self.sharing {
                    "You are sharing your screen."
                } else {
                    "No one is sharing a screen."
                }),
        };

        div().flex().flex_row().size_full().child(sidebar).child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .h_full()
                .child(div().flex_1().min_h_0().bg(rgb(0x09090d)).child(stage))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .p_3()
                        .bg(rgb(PANEL_ALT))
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .child(
                            button("toggle-mic", if self.mic_on { "Mic on" } else { "Mic off" })
                                .on_click(cx.listener(Self::on_toggle_mic)),
                        )
                        .child(div().text_xs().text_color(rgb(MUTED)).child("Mic volume"))
                        .child(mini_button("mic-down", "−").on_click(
                            cx.listener(|view, _, _, cx| view.change_mic_volume(-0.05, cx)),
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!("{:.2}", self.settings.audio.mic_volume)),
                        )
                        .child(mini_button("mic-up", "+").on_click(
                            cx.listener(|view, _, _, cx| view.change_mic_volume(0.05, cx)),
                        ))
                        .child(
                            button(
                                "toggle-system",
                                if self.system_on {
                                    "System on"
                                } else {
                                    "System off"
                                },
                            )
                            .on_click(cx.listener(Self::on_toggle_system)),
                        )
                        .child(if self.sharing {
                            danger_button("stop-share", "Stop share")
                                .on_click(cx.listener(Self::on_stop_share))
                        } else {
                            primary_button("share-screen", "Share screen")
                                .on_click(cx.listener(Self::open_share))
                        })
                        .child(div().flex_1())
                        .child(
                            button("delete-room", "Delete room")
                                .text_color(rgb(DANGER))
                                .on_click(cx.listener(Self::on_delete_room)),
                        )
                        .child(
                            danger_button("leave", "Leave").on_click(cx.listener(Self::on_leave)),
                        ),
                ),
        )
    }

    fn settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut page = div()
            .id("settings-scroll")
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::BOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                button("settings-back", "Back")
                                    .on_click(cx.listener(Self::on_back_settings)),
                            )
                            .child(
                                primary_button("settings-save", "Save")
                                    .on_click(cx.listener(Self::on_save_settings)),
                            ),
                    ),
            );

        page = page.child(
            card()
                .child(section_title("Identity"))
                .child(field("Display name", self.s_name.clone())),
        );

        page = page.child(
            card()
                .child(section_title("Network"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .child(field("Signaling host", self.s_host.clone())),
                        )
                        .child(div().w(px(130.)).child(field("Port", self.s_port.clone()))),
                )
                .child(
                    toggle(
                        "settings-secure",
                        "Use secure WebSocket (wss://)",
                        self.settings.network.signaling_secure,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.settings.network.signaling_secure =
                            !view.settings.network.signaling_secure;
                        cx.notify();
                    })),
                )
                .child(field("STUN servers (comma-separated)", self.s_stun.clone()))
                .child(field(
                    "TURN servers (url|user|credential, comma-separated)",
                    self.s_turn.clone(),
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label("CONNECTION MODE"))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(
                                    chip(
                                        "mode-mesh",
                                        "Mesh (peer to peer)",
                                        self.settings.network.connection_mode
                                            == ConnectionMode::Mesh,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.settings.network.connection_mode =
                                                ConnectionMode::Mesh;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    chip(
                                        "mode-sfu",
                                        "SFU",
                                        self.settings.network.connection_mode
                                            == ConnectionMode::Sfu,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.settings.network.connection_mode =
                                                ConnectionMode::Sfu;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                ),
        );

        let mic_label = self
            .settings
            .audio
            .mic_device_id
            .as_ref()
            .and_then(|id| {
                self.microphones
                    .iter()
                    .find(|device| &device.id == id)
                    .map(|device| device.name.clone())
            })
            .unwrap_or_else(|| "System default".into());
        let output_label = self
            .settings
            .audio
            .output_device_id
            .as_ref()
            .and_then(|id| {
                self.outputs
                    .iter()
                    .find(|device| &device.id == id)
                    .map(|device| device.name.clone())
            })
            .unwrap_or_else(|| "System default".into());

        page = page.child(
            card()
                .child(section_title("Audio"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label("MICROPHONE"))
                                .child(
                                    button("cycle-mic", SharedString::from(mic_label))
                                        .on_click(cx.listener(Self::cycle_mic_device)),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label("AUDIO OUTPUT"))
                                .child(
                                    button("cycle-output", SharedString::from(output_label))
                                        .on_click(cx.listener(Self::cycle_output_device)),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label(format!(
                                    "MIC VOLUME ({:.2})",
                                    self.settings.audio.mic_volume
                                )))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_1()
                                        .child(mini_button("settings-mic-down", "−").on_click(
                                            cx.listener(|view, _, _, cx| {
                                                view.settings.audio.mic_volume =
                                                    (view.settings.audio.mic_volume - 0.05)
                                                        .clamp(0.0, 2.0);
                                                cx.notify();
                                            }),
                                        ))
                                        .child(mini_button("settings-mic-up", "+").on_click(
                                            cx.listener(|view, _, _, cx| {
                                                view.settings.audio.mic_volume =
                                                    (view.settings.audio.mic_volume + 0.05)
                                                        .clamp(0.0, 2.0);
                                                cx.notify();
                                            }),
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label(format!(
                                    "SYSTEM AUDIO VOLUME ({:.2})",
                                    self.settings.audio.system_volume
                                )))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_1()
                                        .child(mini_button("settings-sys-down", "−").on_click(
                                            cx.listener(|view, _, _, cx| {
                                                view.settings.audio.system_volume =
                                                    (view.settings.audio.system_volume - 0.05)
                                                        .clamp(0.0, 2.0);
                                                cx.notify();
                                            }),
                                        ))
                                        .child(mini_button("settings-sys-up", "+").on_click(
                                            cx.listener(|view, _, _, cx| {
                                                view.settings.audio.system_volume =
                                                    (view.settings.audio.system_volume + 0.05)
                                                        .clamp(0.0, 2.0);
                                                cx.notify();
                                            }),
                                        )),
                                ),
                        ),
                )
                .child(
                    toggle(
                        "settings-echo",
                        "Echo cancellation",
                        self.settings.audio.echo_cancellation,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.settings.audio.echo_cancellation =
                            !view.settings.audio.echo_cancellation;
                        cx.notify();
                    })),
                )
                .child(
                    toggle(
                        "settings-noise",
                        "Noise suppression",
                        self.settings.audio.noise_suppression,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.settings.audio.noise_suppression =
                            !view.settings.audio.noise_suppression;
                        cx.notify();
                    })),
                ),
        );

        page = page.child(
            card()
                .child(section_title("Video"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(div().flex_1().child(field("Width", self.s_width.clone())))
                        .child(div().flex_1().child(field("Height", self.s_height.clone())))
                        .child(div().flex_1().child(field("Max FPS", self.s_fps.clone()))),
                )
                .child(
                    div().flex().flex_col().gap_1().child(label("CODEC")).child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                chip("codec-vp8", "VP8", self.settings.video.codec == Codec::Vp8)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.settings.video.codec = Codec::Vp8;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                chip("codec-vp9", "VP9", self.settings.video.codec == Codec::Vp9)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.settings.video.codec = Codec::Vp9;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                chip(
                                    "codec-h264",
                                    "H.264",
                                    self.settings.video.codec == Codec::H264,
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.settings.video.codec = Codec::H264;
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .child(field("Max bitrate (kbps)", self.s_bitrate.clone())),
                        )
                        .child(div().flex_1()),
                )
                .child(
                    toggle(
                        "settings-hardware",
                        "Prefer hardware encoding",
                        self.settings.video.hardware_encoding,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.settings.video.hardware_encoding =
                            !view.settings.video.hardware_encoding;
                        cx.notify();
                    })),
                ),
        );

        page = page.child(
            card()
                .child(section_title("Capture"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(label("BACKEND"))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(
                                    chip(
                                        "backend-auto",
                                        "Automatic",
                                        self.settings.capture.backend == CaptureBackend::Auto,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.settings.capture.backend = CaptureBackend::Auto;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    chip(
                                        "backend-portal",
                                        "XDG portal",
                                        self.settings.capture.backend == CaptureBackend::Portal,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.settings.capture.backend = CaptureBackend::Portal;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    chip(
                                        "backend-windows",
                                        "Windows",
                                        self.settings.capture.backend == CaptureBackend::Windows,
                                    )
                                    .on_click(cx.listener(
                                        |view, _, _, cx| {
                                            view.settings.capture.backend = CaptureBackend::Windows;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        ),
                )
                .child(
                    toggle(
                        "settings-system-audio",
                        "Share system audio with the screen by default",
                        self.settings.capture.system_audio,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.settings.capture.system_audio = !view.settings.capture.system_audio;
                        cx.notify();
                    })),
                ),
        );

        page
    }

    fn share_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut dialog = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .w(px(560.))
            .rounded_lg()
            .bg(rgb(PANEL_ALT))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child("Share your screen"),
            );

        dialog = dialog.child(label("SOURCE"));
        if self.sources.is_empty() {
            dialog = dialog.child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("No sources available"),
            );
        } else {
            let mut sources = div().flex().flex_row().gap_2();
            for source in &self.sources {
                let id = source.id.clone();
                sources = sources.child(
                    chip(
                        SharedString::from(format!("source-{}", source.id)),
                        SharedString::from(source.name.clone()),
                        self.share.source.as_deref() == Some(source.id.as_str()),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.share.source = Some(id.clone());
                        cx.notify();
                    })),
                );
            }
            dialog = dialog.child(sources);
        }

        dialog = dialog
            .child(label("RESOLUTION"))
            .child(
                div().flex().flex_row().gap_2().children(
                    [(1920, 1080), (1280, 720), (2560, 1440), (3840, 2160)]
                        .into_iter()
                        .map(|(width, height)| {
                            chip(
                                SharedString::from(format!("res-{width}x{height}")),
                                SharedString::from(format!("{width}×{height}")),
                                self.share.width == width && self.share.height == height,
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    view.share.width = width;
                                    view.share.height = height;
                                    cx.notify();
                                },
                            ))
                        }),
                ),
            )
            .child(label("FRAME RATE"))
            .child(div().flex().flex_row().gap_2().children(
                [15u32, 24, 30, 60, 90, 120].into_iter().map(|fps| {
                    chip(
                        SharedString::from(format!("fps-{fps}")),
                        SharedString::from(format!("{fps} fps")),
                        self.share.fps == fps,
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.share.fps = fps;
                        cx.notify();
                    }))
                }),
            ))
            .child(label(format!(
                "VIDEO BITRATE — {} kbps",
                self.share.bitrate
            )))
            .child(
                div().flex().flex_row().gap_2().children(
                    [1500u32, 2500, 4000, 8000, 16000]
                        .into_iter()
                        .map(|bitrate| {
                            chip(
                                SharedString::from(format!("rate-{bitrate}")),
                                SharedString::from(format!("{bitrate} kbps")),
                                self.share.bitrate == bitrate,
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    view.share.bitrate = bitrate;
                                    cx.notify();
                                },
                            ))
                        }),
                ),
            )
            .child(
                toggle(
                    "share-hardware",
                    "Prefer hardware encoding",
                    self.share.hardware,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.share.hardware = !view.share.hardware;
                    cx.notify();
                })),
            )
            .child(
                toggle(
                    "share-system-audio",
                    "Share system audio",
                    self.share.system_audio,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.share.system_audio = !view.share.system_audio;
                    cx.notify();
                })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("share-cancel", "Cancel").on_click(cx.listener(Self::close_share)),
                    )
                    .child(
                        primary_button("share-start", "Start sharing")
                            .on_click(cx.listener(Self::on_start_share)),
                    ),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000099))
            .child(dialog)
    }

    fn cycle_mic_device(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let options = self.microphones.clone();
        self.settings.audio.mic_device_id =
            next_device(&options, self.settings.audio.mic_device_id.clone());
        cx.notify();
    }

    fn cycle_output_device(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let options = self.outputs.clone();
        self.settings.audio.output_device_id =
            next_device(&options, self.settings.audio.output_device_id.clone());
        cx.notify();
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT));

        if let Some(error) = self.error.clone() {
            root = root.child(self.error_banner(SharedString::from(error), cx));
        }

        root = root.child(match self.screen {
            Screen::Home => self.home(cx).into_any_element(),
            Screen::Call => self.call(cx).into_any_element(),
            Screen::Settings => self.settings(cx).into_any_element(),
        });

        if self.share_open {
            root = root.child(self.share_dialog(cx));
        }

        root
    }
}

// --- Pip window ------------------------------------------------------------

pub struct PipView {
    frame: Option<Arc<RenderImage>>,
    app: WeakEntity<AppView>,
}

impl Render for PipView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.frame {
            Some(frame) => img(frame.clone())
                .object_fit(ObjectFit::Contain)
                .size_full()
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(MUTED))
                .child("Waiting for video…")
                .into_any_element(),
        };
        let app = self.app.clone();
        div()
            .size_full()
            .relative()
            .bg(rgb(0x000000))
            .child(content)
            .child(
                div()
                    .id("pip-close")
                    .absolute()
                    .top_1()
                    .right_1()
                    .size_6()
                    .rounded_full()
                    .bg(gpui::rgba(0x000000aa))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(CursorStyle::PointingHand)
                    .child("×")
                    .on_click(move |_, window, cx| {
                        let _ = app.update(cx, |view, cx| {
                            view.pip_window = None;
                            view.pip_entity = None;
                            cx.notify();
                        });
                        window.remove_window();
                    }),
            )
    }
}

// --- small helpers ---------------------------------------------------------

fn label(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(MUTED)).child(text.into())
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .p_5()
        .rounded_lg()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
}

fn section_title(text: &'static str) -> Div {
    div().text_lg().font_weight(FontWeight::BOLD).child(text)
}

fn field(caption: impl Into<SharedString>, input: Entity<TextInput>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(label(caption))
        .child(input)
}

fn button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(PANEL_ALT))
        .border_1()
        .border_color(rgb(BORDER))
        .text_sm()
        .text_color(rgb(TEXT))
        .cursor(CursorStyle::PointingHand)
        .hover(|style| style.bg(rgb(0x2a2a3a)))
        .child(text.into())
}

fn primary_button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(ACCENT))
        .text_sm()
        .text_color(rgb(0xffffff))
        .cursor(CursorStyle::PointingHand)
        .hover(|style| style.bg(rgb(0x7d7dff)))
        .child(text.into())
}

fn danger_button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(0x3a1a20))
        .border_1()
        .border_color(rgb(DANGER))
        .text_sm()
        .text_color(rgb(0xffb3bb))
        .cursor(CursorStyle::PointingHand)
        .hover(|style| style.bg(rgb(0x4a2028)))
        .child(text.into())
}

fn mini_button(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size_6()
        .rounded_md()
        .bg(rgb(PANEL_ALT))
        .border_1()
        .border_color(rgb(BORDER))
        .text_sm()
        .cursor(CursorStyle::PointingHand)
        .hover(|style| style.bg(rgb(0x2a2a3a)))
        .child(text.into())
}

fn chip(id: impl Into<ElementId>, text: impl Into<SharedString>, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_full()
        .border_1()
        .border_color(rgb(if selected { ACCENT } else { BORDER }))
        .bg(rgb(if selected { 0x2a2a55 } else { PANEL }))
        .text_xs()
        .text_color(rgb(if selected { 0xd7d7ff } else { TEXT }))
        .cursor(CursorStyle::PointingHand)
        .hover(|style| style.bg(rgb(0x26263a)))
        .child(text.into())
}

fn toggle(id: &'static str, text: &'static str, checked: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor(CursorStyle::PointingHand)
        .child(
            div()
                .size_4()
                .rounded_sm()
                .border_1()
                .border_color(rgb(if checked { ACCENT } else { BORDER }))
                .bg(rgb(if checked { ACCENT } else { PANEL }))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .child(if checked { "✓" } else { "" }),
        )
        .child(div().text_sm().text_color(rgb(TEXT)).child(text))
}

fn avatar(name: &str) -> Div {
    div()
        .size_8()
        .rounded_full()
        .bg(rgb(0x2a2a44))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .child(SharedString::from(initials(name)))
}

fn participant_row(participant: &Participant) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(avatar(&participant.name))
        .child(div().flex_1().child(participant.name.clone()))
        .child(
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .child(format!("{:?}", participant.connection)),
        )
}

pub fn initials(name: &str) -> String {
    let mut parts = name.split_whitespace().filter(|part| !part.is_empty());
    match (parts.next(), parts.next()) {
        (None, _) => "?".into(),
        (Some(only), None) => only.chars().take(2).collect::<String>().to_uppercase(),
        (Some(first), Some(second)) => {
            let mut initials = String::new();
            if let Some(character) = first.chars().next() {
                initials.push(character);
            }
            if let Some(character) = second.chars().next() {
                initials.push(character);
            }
            initials.to_uppercase()
        }
    }
}

fn parse_u32(value: &str, fallback: u32) -> u32 {
    value.trim().parse().unwrap_or(fallback)
}

fn parse_stun(text: &str) -> Vec<sharescreen_config::StunServer> {
    text.split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| sharescreen_config::StunServer {
            url: url.to_string(),
        })
        .collect()
}

fn parse_turn(text: &str) -> Vec<sharescreen_config::TurnServer> {
    text.split(',')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.split('|');
            let url = parts.next().unwrap_or_default().trim().to_string();
            let username = parts.next().map(str::trim).filter(|s| !s.is_empty());
            let credential = parts.next().map(str::trim).filter(|s| !s.is_empty());
            sharescreen_config::TurnServer {
                url,
                username: username.map(str::to_string),
                credential: credential.map(str::to_string),
            }
        })
        .collect()
}

fn next_device(devices: &[Device], current: Option<String>) -> Option<String> {
    if devices.is_empty() {
        return None;
    }
    let current = current.as_deref();
    let index = devices
        .iter()
        .position(|device| Some(device.id.as_str()) == current)
        .map(|index| (index + 1) % (devices.len() + 1))
        .unwrap_or(0);
    if index == devices.len() {
        None
    } else {
        Some(devices[index].id.clone())
    }
}

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
    fn initials_handle_one_and_two_words() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("zed"), "ZE");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn parse_turn_accepts_optional_credentials() {
        let servers = parse_turn("turn:a|user|pass, turn:b");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].username.as_deref(), Some("user"));
        assert_eq!(servers[0].credential.as_deref(), Some("pass"));
        assert!(servers[1].username.is_none());
    }

    #[test]
    fn device_cycle_wraps_to_default() {
        let devices = vec![Device {
            id: "mic1".into(),
            name: "Mic".into(),
            kind: sharescreen_core::DeviceKind::Microphone,
        }];
        assert_eq!(next_device(&devices, None).as_deref(), Some("mic1"));
        assert_eq!(next_device(&devices, Some("mic1".into())), None);
    }
}
