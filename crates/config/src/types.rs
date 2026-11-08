use serde::{Deserialize, Serialize};
use sharescreen_core::{CaptureBackend, Codec, ConnectionMode};

/// Root application configuration.
///
/// `#[serde(default)]` on the struct and every nested struct means a missing
/// section or field falls back to the compiled-in default, so adding a field
/// never breaks an existing user file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub network: NetworkConfig,
    pub audio: AudioConfig,
    pub video: VideoConfig,
    pub capture: CaptureConfig,
    pub state: AppState,
}

// --- Network ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Signaling server hostname or IP. No scheme, no trailing slash.
    pub signaling_host: String,
    pub signaling_port: u16,
    /// `true` = `wss://`, `false` = `ws://`.
    pub signaling_secure: bool,
    pub stun_servers: Vec<StunServer>,
    pub turn_servers: Vec<TurnServer>,
    pub connection_mode: ConnectionMode,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            signaling_host: "127.0.0.1".to_string(),
            signaling_port: 8080,
            signaling_secure: false,
            stun_servers: vec![StunServer {
                url: "stun:stun.l.google.com:19302".to_string(),
            }],
            turn_servers: Vec::new(),
            connection_mode: ConnectionMode::Mesh,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StunServer {
    /// e.g. `stun:host:port`
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnServer {
    /// e.g. `turn:host:port` or `turns:host:port`
    pub url: String,
    pub username: Option<String>,
    /// Static credential for self-hosted TURN. Prefer ephemeral credentials
    /// provisioned by the signaling server; do not commit real ones here.
    pub credential: Option<String>,
}

// --- Audio -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub mic_device_id: Option<String>,
    pub output_device_id: Option<String>,
    /// `0.0 ..= 2.0`; values above `1.0` are a software boost.
    pub mic_volume: f32,
    pub system_volume: f32,
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    /// Opus bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Audio codec. Only [`Codec::Opus`] is supported for audio.
    pub codec: Codec,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            mic_device_id: None,
            output_device_id: None,
            mic_volume: 1.0,
            system_volume: 1.0,
            echo_cancellation: true,
            noise_suppression: true,
            bitrate_kbps: 96,
            codec: Codec::Opus,
        }
    }
}

// --- Video -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub max_bitrate_kbps: u32,
    /// Video codec. [`Codec::Opus`] is invalid for video.
    pub codec: Codec,
    pub hardware_encoding: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            max_fps: 30,
            max_bitrate_kbps: 4000,
            codec: Codec::H264,
            hardware_encoding: true,
        }
    }
}

// --- Capture ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub backend: CaptureBackend,
    pub last_monitor_id: Option<String>,
    pub last_window_id: Option<String>,
    /// Share system audio together with the screen by default.
    pub system_audio: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            backend: CaptureBackend::Auto,
            last_monitor_id: None,
            last_window_id: None,
            system_audio: false,
        }
    }
}

// --- Session state ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    /// Name shown to other participants in a room.
    pub display_name: String,
    pub last_server: Option<ServerAddress>,
    pub last_room: Option<String>,
    /// Most-recent first, capped at [`MAX_RECENT_ROOMS`].
    pub recent_rooms: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerAddress {
    pub host: String,
    pub port: u16,
    pub secure: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            display_name: "User".to_string(),
            last_server: None,
            last_room: None,
            recent_rooms: Vec::new(),
        }
    }
}

/// How many recent rooms to remember across launches.
pub const MAX_RECENT_ROOMS: usize = 10;

// --- Remember-last-* helpers ----------------------------------------------

impl Config {
    /// Record a server that connected successfully, for the "last used server"
    /// restore on next launch.
    pub fn record_successful_server(&mut self, host: impl Into<String>, port: u16, secure: bool) {
        self.state.last_server = Some(ServerAddress {
            host: host.into(),
            port,
            secure,
        });
    }

    /// Record a joined room, moving it to the front of recent rooms and
    /// de-duplicating. The list is capped at [`MAX_RECENT_ROOMS`].
    pub fn record_joined_room(&mut self, room: impl Into<String>) {
        let room = room.into();
        self.state.last_room = Some(room.clone());
        self.state.recent_rooms.retain(|r| r != &room);
        self.state.recent_rooms.insert(0, room);
        self.state.recent_rooms.truncate(MAX_RECENT_ROOMS);
    }

    pub fn set_last_mic(&mut self, id: Option<String>) {
        self.audio.mic_device_id = id;
    }

    pub fn set_last_output(&mut self, id: Option<String>) {
        self.audio.output_device_id = id;
    }

    pub fn set_last_monitor(&mut self, id: Option<String>) {
        self.capture.last_monitor_id = id;
    }

    pub fn set_last_capture_quality(&mut self, fps: u32, width: u32, height: u32) {
        self.video.max_fps = fps;
        self.video.width = width;
        self.video.height = height;
    }
}

// --- Validation ------------------------------------------------------------

impl Config {
    /// Validate the whole config, returning a single error that lists every
    /// invalid value with a human-readable message.
    pub fn validate(&self) -> crate::Result<()> {
        let mut problems: Vec<String> = Vec::new();

        let n = &self.network;
        if n.signaling_host.trim().is_empty() {
            problems.push("network.signaling_host must not be empty".into());
        }
        if n.signaling_port == 0 {
            problems.push("network.signaling_port must be between 1 and 65535".into());
        }
        for (i, s) in n.stun_servers.iter().enumerate() {
            validate_ice_url(&s.url, "stun", i, &mut problems);
        }
        for (i, t) in n.turn_servers.iter().enumerate() {
            validate_ice_url(&t.url, "turn", i, &mut problems);
        }

        let a = &self.audio;
        if !(0.0..=2.0).contains(&a.mic_volume) {
            problems.push("audio.mic_volume must be between 0.0 and 2.0".into());
        }
        if !(0.0..=2.0).contains(&a.system_volume) {
            problems.push("audio.system_volume must be between 0.0 and 2.0".into());
        }
        if !(6..=510).contains(&a.bitrate_kbps) {
            problems.push("audio.bitrate_kbps must be between 6 and 510".into());
        }
        if a.codec != Codec::Opus {
            problems.push("audio.codec must be opus".into());
        }

        let v = &self.video;
        if !(1..=240).contains(&v.max_fps) {
            problems.push("video.max_fps must be between 1 and 240".into());
        }
        if !(320..=7680).contains(&v.width) || !(240..=4320).contains(&v.height) {
            problems
                .push("video resolution out of range (width 320..=7680, height 240..=4320)".into());
        }
        if !(64..=50000).contains(&v.max_bitrate_kbps) {
            problems.push("video.max_bitrate_kbps must be between 64 and 50000".into());
        }
        if v.codec == Codec::Opus {
            problems.push("video.codec cannot be opus".into());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(crate::ConfigError::Invalid(problems.join("\n")))
        }
    }
}

/// Validate a `stun:`/`turn:`/`turns:` URL, pushing a descriptive problem for
/// the given list index when it is malformed.
fn validate_ice_url(url: &str, kind: &str, index: usize, problems: &mut Vec<String>) {
    let (scheme, rest) = match url.split_once(':') {
        Some(parts) => parts,
        None => {
            problems.push(format!(
                "{kind}_servers[{index}]: missing scheme (expected {kind}:host:port)"
            ));
            return;
        }
    };
    let valid_scheme = scheme == kind || (kind == "turn" && scheme == "turns");
    if !valid_scheme {
        problems.push(format!(
            "{kind}_servers[{index}]: unsupported scheme '{scheme}'"
        ));
        return;
    }
    let host_port = rest.trim_start_matches('/');
    if host_port.is_empty() || host_port.starts_with(':') {
        problems.push(format!("{kind}_servers[{index}]: missing host"));
    }
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn record_room_dedups_and_caps() {
        let mut c = Config::default();
        for i in 0..12 {
            c.record_joined_room(format!("room{i}"));
        }
        assert_eq!(c.state.last_room.as_deref(), Some("room11"));
        assert_eq!(c.state.recent_rooms.len(), MAX_RECENT_ROOMS);
        assert_eq!(c.state.recent_rooms[0], "room11");

        // Re-recording an existing room moves it to the front without duplicating.
        c.record_joined_room("room5");
        assert_eq!(c.state.recent_rooms[0], "room5");
        assert_eq!(
            c.state
                .recent_rooms
                .iter()
                .filter(|r| *r == "room5")
                .count(),
            1
        );
    }

    #[test]
    fn invalid_values_are_reported() {
        let mut c = Config::default();
        c.network.signaling_port = 0;
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("signaling_port"));

        let mut c = Config::default();
        c.video.max_fps = 0;
        assert!(c.validate().unwrap_err().to_string().contains("max_fps"));

        let mut c = Config::default();
        c.video.codec = Codec::Opus;
        assert!(c
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cannot be opus"));

        let mut c = Config::default();
        c.network.stun_servers[0].url = "http://not-an-ice-scheme".to_string();
        assert!(c.validate().unwrap_err().to_string().contains("scheme"));
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.record_successful_server("example.com", 443, true);
        c.record_joined_room("test-room");
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn partial_file_falls_back_to_defaults() {
        let text = "[network]\nsignaling_host = \"example.com\"\n";
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.network.signaling_host, "example.com");
        assert_eq!(c.network.signaling_port, 8080);
        assert_eq!(c.video, VideoConfig::default());
    }
}
