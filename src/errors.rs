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
    #[error("api error: {0}")]
    Api(String),
    #[error("device authorization denied: the sign-in request was denied in the browser")]
    DeviceAuthorizationDenied,
    #[error("device authorization expired: the code expired before it was approved; call login_with_browser again to request a new code")]
    DeviceAuthorizationExpired,
    #[error("unsupported protocol: {0}")]
    UnsupportedProtocol(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}
