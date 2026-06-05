use thiserror::Error;

pub type Result<T> = std::result::Result<T, ThalovantError>;

#[derive(Debug, Error)]
pub enum ThalovantError {
    #[error("missing identity field: {0}")]
    MissingIdentityField(&'static str),
    #[error("invalid identity: {0}")]
    InvalidIdentity(String),
    #[error("connection error: {0}")]
    Connection(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
