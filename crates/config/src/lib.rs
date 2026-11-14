//! `sharescreen-config`: user configuration and persistent preferences.
//!
//! The [`Config`] type is the full application configuration, split into
//! network, audio, video, capture, and session-state sections. It has two
//! layers:
//!
//! - compiled-in defaults ([`Config::default`]), written to disk on first
//!   launch so users have an editable file;
//! - user overrides, read back from that file on subsequent launches.
//!
//! Every field and struct is `#[serde(default)]`, so a partially-written or
//! older config file falls back to defaults instead of failing to parse.

mod error;
mod store;
mod types;

pub use error::{ConfigError, Result};
pub use store::{default_config_path, default_log_path, ConfigStore};
pub use types::{
    AppState, AudioConfig, CaptureConfig, Config, NetworkConfig, NoiseSuppressionLevel,
    ServerAddress, StunServer, TurnServer, VideoConfig,
};
