use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use tracing::{info, warn};

use crate::error::Result;
use crate::types::Config;

/// Owns a [`Config`] plus the path it is persisted to. The single entry point
/// for reading and writing user preferences; writes the compiled-in defaults
/// on first launch so users have an editable file.
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    config: Config,
}

impl ConfigStore {
    /// Load from the platform config directory, writing defaults on first
    /// launch. Falls back to in-memory defaults when no config directory is
    /// available (rare).
    pub fn load_or_default() -> Result<Self> {
        match default_config_path() {
            Some(path) => Self::load_from_path(path),
            None => {
                warn!("no platform config directory; using in-memory defaults");
                Ok(Self {
                    path: PathBuf::from("config.toml"),
                    config: Config::default(),
                })
            }
        }
    }

    /// Load from an explicit path, writing defaults when the file is absent.
    pub fn load_from_path(path: PathBuf) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let config: Config = toml::from_str(&text)?;
            config.validate()?;
            info!(path = %path.display(), "loaded configuration");
            Ok(Self { path, config })
        } else {
            let store = Self {
                path,
                config: Config::default(),
            };
            store.save()?;
            info!(path = %store.path.display(), "wrote default configuration");
            Ok(store)
        }
    }

    /// Persist the current config, creating parent directories as needed.
    pub fn save(&self) -> Result<()> {
        self.config.validate()?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let text = toml::to_string_pretty(&self.config)?;
        std::fs::write(&self.path, text)?;
        info!(path = %self.path.display(), "saved configuration");
        Ok(())
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Platform config directory for `config.toml`, e.g.
/// `~/.config/sharescreen/config.toml` on Linux.
pub fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "sharescreen", "sharescreen")
        .map(|d| d.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ConfigError;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sharescreen-{name}-{nanos}.toml"))
    }

    #[test]
    fn first_launch_writes_defaults() {
        let path = temp_path("first");
        let store = ConfigStore::load_from_path(path.clone()).unwrap();
        assert!(path.exists());
        assert_eq!(store.config(), &Config::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_and_reload_preserves_prefs() {
        let path = temp_path("reload");
        let mut store = ConfigStore::load_from_path(path.clone()).unwrap();
        store.config_mut().record_joined_room("persisted-room");
        store.save().unwrap();

        let reloaded = ConfigStore::load_from_path(path.clone()).unwrap();
        assert_eq!(
            reloaded.config().state.last_room.as_deref(),
            Some("persisted-room")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_file_on_disk_is_rejected() {
        let path = temp_path("invalid");
        std::fs::write(&path, "[network]\nsignaling_port = 0\n").unwrap();
        let err = ConfigStore::load_from_path(path.clone()).unwrap_err();
        match err {
            ConfigError::Invalid(msg) => assert!(msg.contains("signaling_port")),
            other => panic!("expected Invalid, got {other}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
