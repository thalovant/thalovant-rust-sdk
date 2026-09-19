use thiserror::Error;

pub type Result<T> = std::result::Result<T, ThalovantError>;

/// The numbers behind a refusal that is a spent allowance, not a policy.
///
/// The intent-quota policy denies with `intent_quota_exceeded` and sends which
/// counter ran out (`daily`, `monthly`), what it allows, how much was used,
/// and how many seconds until it resets. Without them a caller can only say
/// "refused", which is what an app showed somebody who had used up the day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quota {
    /// The counter that ran out, as the hub names it.
    pub period: String,
    /// What that counter allows in its period.
    pub limit: u64,
    /// How much of it was used.
    pub used: u64,
    /// Seconds until the counter resets, or 0 when the hub did not say.
    pub reset_after: u64,
}

impl Quota {
    /// The hub's code for a refusal that is a spent quota, not a policy.
    pub const EXCEEDED_CODE: &'static str = "intent_quota_exceeded";
    /// The hub's code for a refusal because its own agent bus is down.
    pub const BACKEND_UNAVAILABLE_CODE: &'static str = "backend_unavailable";
}

/// Advice follows the kind of refusal. Telling somebody who used up their day
/// to "allow this connection to publish recognizer_loop:utterance" sent them
/// to a settings page that could not help.
fn refusal_message(
    denied_type: &str,
    code: &str,
    reason: &str,
    quota: &Option<Box<Quota>>,
) -> String {
    if let Some(quota) = quota {
        let used = if quota.limit > 0 {
            format!("{} of {}", quota.used, quota.limit)
        } else {
            "all".to_string()
        };
        let period = if quota.period.is_empty() {
            String::new()
        } else {
            format!(" {}", quota.period)
        };
        let resets = if quota.reset_after > 0 {
            format!("; it resets in {}s", quota.reset_after)
        } else {
            String::new()
        };
        return format!(
            "policy denied: the hub refused `{denied_type}`: {used}{period} questions used{resets}."
        );
    }
    if code == Quota::BACKEND_UNAVAILABLE_CODE {
        let detail = if reason.is_empty() {
            String::new()
        } else {
            format!(": {reason}")
        };
        return format!(
            "policy denied: the hub could not reach its assistant{detail}. Try again shortly."
        );
    }
    format!(
        "policy denied: the hub refused `{denied_type}`: {}. Allow this connection to publish `{denied_type}` in the dashboard's connection settings.",
        policy_denied_detail(code, reason)
    )
}

/// Every way an SDK call fails.
///
/// `#[non_exhaustive]`: a caller matching on this enum needs a wildcard arm,
/// so a later release can name a new failure without breaking them.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ThalovantError {
    #[error("missing identity field: {0}")]
    MissingIdentityField(&'static str),
    #[error("invalid identity: {0}")]
    InvalidIdentity(String),
    #[error("connection error: {0}")]
    Connection(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("listing error: {0}")]
    Listing(String),
    #[error("runtime error: {0}")]
    Runtime(String),
    /// The hub refused a message type this connection may not publish.
    ///
    /// The hub answers `hive.policy.denied` at once, naming the type and the
    /// list it does allow; failing here saves the caller a timeout and tells
    /// the operator exactly what to add to the connection's allow-list.
    #[error("{}", refusal_message(.denied_type, .code, .reason, .quota))]
    PolicyDenied {
        /// The message type the hub refused, for example `recognizer_loop:utterance`.
        denied_type: String,
        /// The hub's code: `acl_disallowed_type`, `intent_quota_exceeded` or
        /// `backend_unavailable`.
        code: String,
        /// The hub's own wording, when it gave one.
        reason: String,
        /// The types this connection may publish, as the hub listed them.
        allowed: Vec<String>,
        /// The numbers behind a spent allowance; `None` for any other refusal.
        ///
        /// Boxed to keep `ThalovantError` small: every `Result` in this crate
        /// carries this enum, and inline the quota pushed it past the size
        /// clippy's `result_large_err` allows. `&*quota` reads it.
        quota: Option<Box<Quota>>,
    },
    /// The hub understood a question and has nothing for it.
    ///
    /// `ovos.intent.unmatched` (`complete_intent_failure` from older hubs) is
    /// neither a refusal nor a fault: nothing went wrong, the question is
    /// outside what this hub can do. As a bare runtime error a caller could
    /// only report that something failed.
    #[error("unanswered: {}", if .said.is_empty() { "the hub has no skill that answers this" } else { .said })]
    Unanswered {
        /// The hub's own words, when it sent any.
        said: String,
    },
    #[error("api error: {0}")]
    Api(String),
    #[error("api error: HTTP {status_code}: {detail}")]
    ApiResponse { status_code: u16, detail: String },
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

fn policy_denied_detail<'a>(code: &'a str, reason: &'a str) -> &'a str {
    [reason, code]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or("refused by the hub's policy")
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

impl ThalovantError {
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::ApiResponse { status_code, .. } => Some(*status_code),
            Self::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
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
