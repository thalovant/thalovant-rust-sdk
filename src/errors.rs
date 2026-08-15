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
    Http(reqwest::Error),
}

impl From<reqwest::Error> for ThalovantError {
    /// Strip the request URL before storing a reqwest error. The data-plane URLs
    /// carry the caller's access key in a `?authorization=` query, and reqwest's
    /// `Display` would otherwise append it as " for url (...)" wherever the error
    /// is rendered (notably `TransportHealth::last_error`).
    fn from(error: reqwest::Error) -> Self {
        ThalovantError::Http(error.without_url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_error_conversion_strips_authorization_url() {
        // Nothing listens on port 1, so the request fails to connect and reqwest
        // attaches the request URL — including the secret `?authorization=` query.
        let secret = "dXNlcjphY2Nlc3Mta2V5";
        let url = format!("http://127.0.0.1:1/connect?authorization={secret}");
        let raw = reqwest::Client::new().get(&url).send().await.unwrap_err();
        assert!(
            raw.url()
                .is_some_and(|value| value.as_str().contains(secret)),
            "precondition: the raw reqwest error must carry the secret URL"
        );

        let converted: ThalovantError = raw.into();
        let rendered = converted.to_string();
        assert!(
            !rendered.contains(secret),
            "converted error leaked secret: {rendered}"
        );
        assert!(
            !rendered.contains("authorization"),
            "converted error leaked query: {rendered}"
        );
    }
}
