use thiserror::Error;

pub type Result<T> = std::result::Result<T, RemoteSteerError>;

#[derive(Debug, Error)]
pub enum RemoteSteerError {
    #[error("backend is unavailable on this platform: {0}")]
    BackendUnavailable(&'static str),

    #[error("unsupported profile: {0}")]
    UnsupportedProfile(String),

    #[error("unsupported operation: {0}")]
    UnsupportedOperation(&'static str),

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    #[error("authentication failed")]
    AuthenticationFailed,

    #[error("transport error: {0}")]
    Transport(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("backend error: {0}")]
    Backend(String),
}
