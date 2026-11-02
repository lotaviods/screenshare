use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("failed to parse configuration: {0}")]
    Deserialize(#[from] toml::de::Error),
    #[error("invalid configuration:\n{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;
