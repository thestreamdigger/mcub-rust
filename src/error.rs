use thiserror::Error;

#[derive(Debug, Error)]
pub enum McubError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("nix: {0}")]
    Nix(#[from] nix::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("config: {0}")]
    Config(String),

    #[error("serial: {0}")]
    Serial(String),

    #[error("mpd: {0}")]
    Mpd(String),

    #[error("device not found")]
    DeviceNotFound,

    #[error("timeout")]
    Timeout,

    #[error("disconnected")]
    Disconnected,
}

pub type Result<T> = std::result::Result<T, McubError>;
