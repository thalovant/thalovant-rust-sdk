use std::fmt;
use std::ops::Deref;
use std::time::Duration;

use serde_json::{Map, Value};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ThalovantError>;

/// What the API said when it refused a control-plane request: the whole error
/// body, parsed.
///
/// Every Thalovant API refusal is a Problem+JSON document, and many carry
/// structured fields beside their sentence: a `platform_image_required` 403
/// names `refused_images`, `allowed_images` and `allowed_repositories`; a
/// `plan_limit` 403 names `resource`, `limit`, `used` and `plan`. They are all
/// here, including fields the API adds after this crate was released.
///
/// [`code`](Self::code) and [`detail`](Self::detail) read the body's
/// machine-readable code and its sentence. The rest is a
/// [`serde_json::Map`]: [`get`](Self::get), or any `Map` method through
/// `Deref`.
///
/// The body is kept as sent, so it can hold a value it echoed back from the
/// request (a validation error repeats what it was given). `Debug` redacts
/// every secret-named key, so `{:?}` and an `unwrap()` panic never print one;
/// reading the map directly gives the values as they are.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ApiProblem(Map<String, Value>);

impl ApiProblem {
    /// The body's machine-readable code, such as `platform_image_required` or
    /// `plan_limit`.
    ///
    /// `code` when it is a string with at least one non-whitespace character;
    /// else, when `detail` is itself an object (FastAPI's own envelope, which
    /// the API's Problem+JSON handler normally lifts), that object's `code`
    /// under the same rule; else `None`. Returned exactly as sent.
    pub fn code(&self) -> Option<&str> {
        problem_text(self.0.get("code")).or_else(|| problem_text(self.nested()?.get("code")))
    }

    /// The API's own sentence, whole and exactly as sent: never trimmed,
    /// collapsed or shortened.
    ///
    /// `detail` when it is a string with at least one non-whitespace
    /// character; else, when `detail` is itself an object, that object's
    /// `detail` under the same rule; else `None`.
    pub fn detail(&self) -> Option<&str> {
        problem_text(self.0.get("detail")).or_else(|| problem_text(self.nested()?.get("detail")))
    }

    /// One member of the body, for example `allowed_images`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// The whole body.
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    /// The whole body, owned.
    pub fn into_map(self) -> Map<String, Value> {
        self.0
    }

    fn nested(&self) -> Option<&Map<String, Value>> {
        self.0.get("detail").and_then(Value::as_object)
    }
}

/// A string with something in it, exactly as sent; anything else is absent.
fn problem_text(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

impl Deref for ApiProblem {
    type Target = Map<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Map<String, Value>> for ApiProblem {
    fn from(map: Map<String, Value>) -> Self {
        Self(map)
    }
}

impl fmt::Debug for ApiProblem {
    /// Written by hand: `ThalovantError` derives `Debug`, so whatever this
    /// prints is what `{:?}` and an `unwrap()` panic print, and the body can
    /// echo a password the request sent.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApiProblem")
            .field(&crate::redact::redact_map(&self.0))
            .finish()
    }
}

/// What kind of refusal an API answer is, for a caller that branches on it;
/// see [`ThalovantError::api_refusal`].
///
/// The error itself stays what it always was -- usually
/// [`ThalovantError::ApiResponse`] -- and this names the refusals a caller
/// can do something specific about. `#[non_exhaustive]`: match with a
/// wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApiRefusal {
    /// The token itself was refused: HTTP 401 (unknown, expired or revoked),
    /// 423 (the account is locked), or 403 whose detail is exactly
    /// `Insufficient scopes`. Signing in again is the way out of each.
    Auth,
    /// The account's plan does not allow it: HTTP 402, or 403 with code
    /// `plan_limit`, whose problem carries `resource`, `limit` and `used`.
    Plan,
    /// The hub already holds the one connection of this kind it allows: HTTP
    /// 409 `home_assistant_already_linked`.
    /// [`ThalovantError::linked_client_id`] names the connection holding it.
    AlreadyLinked,
}

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
        if quota.limit == 0 && quota.used == 0 && quota.reset_after == 0 && quota.period.is_empty()
        {
            // Refused on a quota, with none of the numbers. "All questions
            // used" would be inventing one.
            return format!("policy denied: the hub refused `{denied_type}`: a quota has run out.");
        }
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
    /// The control-plane API answered with an error status.
    ///
    /// The message is one bounded line for display and can be shortened, so
    /// it is never where to read what the API said: use
    /// [`ThalovantError::api_code`], [`ThalovantError::api_detail`] and
    /// [`ThalovantError::api_problem`].
    #[error("api error: HTTP {status_code}: {detail}")]
    ApiResponse {
        /// The HTTP status.
        status_code: u16,
        /// The display line: the body as redacted JSON, whitespace collapsed
        /// and cut at 200 characters, or a fixed sentence when the body is
        /// not a JSON object. Not the API's `detail` member; that is
        /// [`ThalovantError::api_detail`].
        detail: String,
        /// The whole error body, parsed, when it is a JSON object; `None`
        /// for an empty body, one that is not JSON, or JSON that is not an
        /// object, and for a refused redirect.
        ///
        /// Boxed for the same reason as `PolicyDenied::quota`: every
        /// `Result` in this crate carries this enum.
        problem: Option<Box<ApiProblem>>,
    },
    #[error("device authorization denied: the sign-in request was denied in the browser")]
    DeviceAuthorizationDenied,
    #[error("device authorization expired: the code expired before it was approved; call login_with_browser again to request a new code")]
    DeviceAuthorizationExpired,
    /// One [`ControlPlane::poll_device_login`](crate::ControlPlane::poll_device_login)
    /// while nobody has approved the sign-in yet.
    ///
    /// Poll again after `interval`. A `slow_down` from the API has already
    /// lengthened it by five seconds, and it stays lengthened for every later
    /// poll of the same device code (RFC 8628 §3.5).
    #[error("device sign-in pending: nobody has approved it yet; poll again in {interval:?}")]
    DeviceLoginPending {
        /// How long to wait before the next poll.
        interval: Duration,
        /// The HTTP status the API answered with (400).
        status_code: u16,
        /// The body the API answered with, when it was a JSON object.
        problem: Option<Box<ApiProblem>>,
    },
    /// The device code expired before anybody approved it; begin again for a
    /// new code. [`ControlPlane::login_with_browser`](crate::ControlPlane::login_with_browser)
    /// reports the same outcome as [`ThalovantError::DeviceAuthorizationExpired`].
    #[error("device sign-in expired: the code expired before it was approved; call begin_device_login again for a new code")]
    DeviceLoginExpired {
        /// The HTTP status the API answered with (400).
        status_code: u16,
        /// The body the API answered with, when it was a JSON object.
        problem: Option<Box<ApiProblem>>,
    },
    /// The person declined the sign-in.
    /// [`ControlPlane::login_with_browser`](crate::ControlPlane::login_with_browser)
    /// reports the same outcome as [`ThalovantError::DeviceAuthorizationDenied`].
    #[error("device sign-in denied: the sign-in request was denied in the browser")]
    DeviceLoginDenied {
        /// The HTTP status the API answered with (400).
        status_code: u16,
        /// The body the API answered with, when it was a JSON object.
        problem: Option<Box<ApiProblem>>,
    },
    /// The API cannot make a connection of the kind asked for.
    ///
    /// Either it refused the kind (HTTP 422 naming `connection_type`, with
    /// `status_code` and `problem` set), or it made a connection whose kind
    /// did not come back as asked -- an ordinary connection nobody asked for,
    /// which the SDK deletes before failing (`status_code` is `None`; `reason`
    /// says if the delete failed and which connection is left).
    #[error("unsupported connection type `{connection_type}`: {reason}")]
    UnsupportedConnectionType {
        /// The kind asked for, such as `home_assistant`.
        connection_type: String,
        /// What happened, for display.
        reason: String,
        /// The HTTP status of a refusal; `None` when the API made the wrong kind.
        status_code: Option<u16>,
        /// The body of a refusal, when it was a JSON object.
        problem: Option<Box<ApiProblem>>,
    },
    /// A hub did not admit a new connection within the wait.
    ///
    /// Both a connection error and a timeout ([`ThalovantError::is_connection_error`]
    /// and [`ThalovantError::is_timeout`] are both true): the connection
    /// exists and may still be admitted, so waiting longer or connecting later
    /// can succeed.
    #[error(
        "admission timeout: the hub did not admit the connection within {timeout:?}; it may still admit it later"
    )]
    AdmissionTimeout {
        /// How long the wait was.
        timeout: Duration,
    },
    /// The operation that admits a new connection failed or timed out on the
    /// platform, or the API refused the wait itself.
    ///
    /// For a platform failure `error_code` is the operation's own code and
    /// `status_code` is `None`. For a refusal of the wait `error_code` is
    /// `None` and the refusal is `status_code` and `problem`, read as any API
    /// answer is ([`ThalovantError::api_code`], [`ThalovantError::api_detail`]).
    /// A 401 or 403 is never this: it is returned as the
    /// [`ThalovantError::ApiResponse`] it is. A connection error
    /// ([`ThalovantError::is_connection_error`]).
    #[error("admission failed: {reason}")]
    AdmissionFailed {
        /// The operation's own code, such as `gitops_push_rejected`; `None`
        /// when the API refused the wait instead.
        error_code: Option<String>,
        /// What happened, for display.
        reason: String,
        /// The HTTP status when the API refused the wait; `None` when the
        /// operation itself failed.
        status_code: Option<u16>,
        /// The body of that refusal, when it was a JSON object.
        problem: Option<Box<ApiProblem>>,
    },
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

/// How a [`ThalovantError::Connection`] that is a refusal of the credentials
/// begins. The finer kinds of connection and API failure ride in the message
/// of the variant they have always been reported as, rather than in variants
/// of their own, so that a caller matching `Connection(_)` or `Api(_)` keeps
/// catching them; the `is_*` methods read them back. Every one is built by
/// the constructors below, and nothing else in the crate begins a message
/// with these words.
const HUB_REFUSED: &str = "the hub refused this connection's credentials";
/// How a [`ThalovantError::Connection`] for a changed hub key begins.
const HUB_KEY_CHANGED: &str = "the hub's Noise key is not the one pinned for it";
/// How a [`ThalovantError::Api`] for an API out of reach begins.
const API_UNREACHABLE: &str = "could not reach the Thalovant API";

impl ThalovantError {
    /// The HTTP status of an error the API answered with.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Http(e) => e.status().map(|s| s.as_u16()),
            _ => self.api_answer().map(|(status_code, _)| status_code),
        }
    }

    /// The status and body of an error that is an answer from the API.
    fn api_answer(&self) -> Option<(u16, Option<&ApiProblem>)> {
        match self {
            Self::ApiResponse {
                status_code,
                problem,
                ..
            }
            | Self::DeviceLoginPending {
                status_code,
                problem,
                ..
            }
            | Self::DeviceLoginExpired {
                status_code,
                problem,
            }
            | Self::DeviceLoginDenied {
                status_code,
                problem,
            } => Some((*status_code, problem.as_deref())),
            Self::UnsupportedConnectionType {
                status_code: Some(status_code),
                problem,
                ..
            }
            | Self::AdmissionFailed {
                status_code: Some(status_code),
                problem,
                ..
            } => Some((*status_code, problem.as_deref())),
            _ => None,
        }
    }

    /// The error body the API sent, parsed, when it is a JSON object.
    ///
    /// Read from every error that is an answer from the API: `ApiResponse`,
    /// the device-login outcomes, a refused connection type and a refused
    /// admission wait. `None` for every other error, and for an API error
    /// whose body was empty, not JSON, or JSON that is not an object.
    pub fn api_problem(&self) -> Option<&ApiProblem> {
        self.api_answer().and_then(|(_, problem)| problem)
    }

    /// What kind of refusal this API answer is, when it is one a caller can
    /// act on; see [`ApiRefusal`].
    ///
    /// - [`ApiRefusal::Auth`]: HTTP 401, 423, or 403 whose detail is exactly
    ///   `Insufficient scopes`;
    /// - [`ApiRefusal::Plan`]: HTTP 402, or 403 with code `plan_limit`;
    /// - [`ApiRefusal::AlreadyLinked`]: HTTP 409 with code
    ///   `home_assistant_already_linked`.
    ///
    /// `None` for anything else. The error is unchanged: a 402 from
    /// [`ControlPlane::get_hub`](crate::ControlPlane::get_hub) is still
    /// [`ThalovantError::ApiResponse`], and this is how to tell it is a plan
    /// refusal without matching the status yourself.
    pub fn api_refusal(&self) -> Option<ApiRefusal> {
        let (status_code, problem) = self.api_answer()?;
        let code = problem.and_then(ApiProblem::code);
        let detail = problem.and_then(ApiProblem::detail);
        match status_code {
            401 | 423 => Some(ApiRefusal::Auth),
            403 if detail == Some("Insufficient scopes") => Some(ApiRefusal::Auth),
            402 => Some(ApiRefusal::Plan),
            403 if code == Some("plan_limit") => Some(ApiRefusal::Plan),
            409 if code == Some("home_assistant_already_linked") => Some(ApiRefusal::AlreadyLinked),
            _ => None,
        }
    }

    /// The connection that already holds a hub's link, on an
    /// [`ApiRefusal::AlreadyLinked`] refusal, when the API named it.
    ///
    /// Read from `client_id`, `existing_client_id` or `connection_id`, at the
    /// top of the problem first and then inside a `detail` that is itself an
    /// object. `None` for every other error.
    pub fn linked_client_id(&self) -> Option<&str> {
        if self.api_refusal() != Some(ApiRefusal::AlreadyLinked) {
            return None;
        }
        let problem = self.api_problem()?;
        let nested = problem.get("detail").and_then(Value::as_object);
        [Some(problem.as_map()), nested]
            .into_iter()
            .flatten()
            .flat_map(|source| {
                ["client_id", "existing_client_id", "connection_id"]
                    .into_iter()
                    .map(move |key| source.get(key))
            })
            .find_map(|value| value.and_then(Value::as_str).filter(|id| !id.is_empty()))
    }

    /// Whether this is a timeout: [`ThalovantError::Timeout`], or
    /// [`ThalovantError::AdmissionTimeout`].
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_) | Self::AdmissionTimeout { .. })
    }

    /// Whether this is a connection error: [`ThalovantError::Connection`]
    /// (a hub that refused the credentials or whose key changed among them),
    /// a control-plane API out of reach ([`ThalovantError::is_api_unreachable`]),
    /// [`ThalovantError::AdmissionTimeout`] or
    /// [`ThalovantError::AdmissionFailed`].
    pub fn is_connection_error(&self) -> bool {
        matches!(
            self,
            Self::Connection(_) | Self::AdmissionTimeout { .. } | Self::AdmissionFailed { .. }
        ) || self.is_api_unreachable()
    }

    /// Whether a hub turned this connection's credentials away.
    ///
    /// A hub closes the socket during the handshake or within 750 ms after
    /// it, without a status or with 1000, 1005 or 1008, when it does not know
    /// the connection's key -- or does not know it yet: a connection just
    /// created is refused until its hub has admitted it. A handshake message
    /// that does not authenticate under the key the password derives (a wrong
    /// password), and a WebSocket upgrade or an HTTP request answered 401 or
    /// 403, are refusals too; see
    /// [`close_refuses`](crate::transport::close_refuses).
    ///
    /// The error is the [`ThalovantError::Connection`] a failed connect has
    /// always been, so a match on that variant still catches it; this says
    /// which kind of connection error it is.
    pub fn is_hub_refused(&self) -> bool {
        matches!(self, Self::Connection(message) if message.starts_with(HUB_REFUSED))
    }

    /// Whether the hub's Noise static key is not the one pinned for it.
    ///
    /// The hub was reinstalled or replaced -- or another machine is answering
    /// at its address. Retrying cannot change that, so
    /// [`HubSession::run`](crate::HubSession::run) stops at once. The SDK
    /// never replaces a pin itself: if the hub really was replaced, drop the
    /// stale pin with [`forget_noise_pin`](crate::forget_noise_pin) and
    /// connect again. Not a refusal.
    ///
    /// Like [`ThalovantError::is_hub_refused`], the error is the
    /// [`ThalovantError::Connection`] it has always been.
    pub fn is_hub_key_changed(&self) -> bool {
        matches!(self, Self::Connection(message) if message.starts_with(HUB_KEY_CHANGED))
    }

    /// Whether a control-plane request never got an answer: DNS, the
    /// connection, TLS, a proxy, or the request's own timeout. There is no
    /// status and no problem, and trying again later can work, which is not
    /// true of an answer from the API.
    ///
    /// The error is the [`ThalovantError::Api`] such a failure has always
    /// been, so a match on that variant still catches it.
    pub fn is_api_unreachable(&self) -> bool {
        matches!(self, Self::Api(message) if message.starts_with(API_UNREACHABLE))
    }

    /// A refusal of the credentials by the hub: a
    /// [`ThalovantError::Connection`] that
    /// [`ThalovantError::is_hub_refused`] answers true for.
    pub(crate) fn hub_refused(detail: impl fmt::Display) -> Self {
        Self::Connection(format!("{HUB_REFUSED}: {detail}"))
    }

    /// A hub whose key is not the pinned one: a
    /// [`ThalovantError::Connection`] that
    /// [`ThalovantError::is_hub_key_changed`] answers true for.
    pub(crate) fn hub_key_changed(detail: impl fmt::Display) -> Self {
        Self::Connection(format!("{HUB_KEY_CHANGED}: {detail}"))
    }

    /// The message of a [`ThalovantError::Connection`], for a verdict kept
    /// to rebuild the same error later.
    pub(crate) fn into_connection_message(self) -> String {
        match self {
            Self::Connection(message) => message,
            other => other.to_string(),
        }
    }

    /// A control-plane request that never got an answer: a
    /// [`ThalovantError::Api`] that [`ThalovantError::is_api_unreachable`]
    /// answers true for.
    pub(crate) fn api_unreachable(detail: impl fmt::Display) -> Self {
        Self::Api(format!("{API_UNREACHABLE}: {detail}"))
    }

    /// The API's machine-readable code, such as `platform_image_required` or
    /// `plan_limit`; see [`ApiProblem::code`].
    pub fn api_code(&self) -> Option<&str> {
        self.api_problem().and_then(ApiProblem::code)
    }

    /// The API's own sentence, whole and exactly as sent; see
    /// [`ApiProblem::detail`].
    pub fn api_detail(&self) -> Option<&str> {
        self.api_problem().and_then(ApiProblem::detail)
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
