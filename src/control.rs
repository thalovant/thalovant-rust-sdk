use crate::{
    errors::{Result, ThalovantError},
    identity::Identity,
    protocols::{
        endpoint_from_domain, select_data_plane_endpoint, HubDataPlaneEndpoints, HubProtocol,
        HubProtocolSettings, SelectedHubEndpoint, DEFAULT_PROTOCOL_PREFERENCE,
    },
    tls::ensure_rustls_provider,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::OsRng, RngCore};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    future::Future,
    time::Duration,
};
use uuid::Uuid;

pub const DEFAULT_CONTROL_API_URL: &str = "https://api.thalovant.com";
pub const DEFAULT_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Source type [`SkillInstallOptions`] uses by default: install from the
/// marketplace catalog rather than from a git repository.
pub const DEFAULT_SKILL_SOURCE_TYPE: &str = "catalog";
const DEFAULT_CONTROL_USER_AGENT: &str = concat!("thalovant-rust-sdk/", env!("CARGO_PKG_VERSION"));
const DEFAULT_DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Requested,
    Committed,
    Applied,
    Ready,
    Failed,
    TimedOut,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OperationResource {
    pub id: String,
    pub kind: String,
    pub aggregate_type: String,
    pub aggregate_id: Option<String>,
    pub status: OperationStatus,
    pub details: Map<String, Value>,
    pub git_commit_sha: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub committed_at: Option<String>,
    pub applied_at: Option<String>,
    pub ready_at: Option<String>,
    pub terminal_at: Option<String>,
    pub links: HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub struct ControlPlane {
    pub api_url: String,
    pub access_token: Option<String>,
    pub user_agent: String,
    http_client: reqwest::Client,
}

#[derive(Clone, Debug, Default)]
pub struct BootstrapIdentityOptions {
    pub name: String,
    pub site_id: Option<String>,
    pub spec: Map<String, Value>,
    pub owner_id: Option<String>,
    pub active: Option<bool>,
    pub preferred_protocols: Vec<HubProtocol>,
    pub idempotency_key: Option<String>,
}

// `Debug` is hand-written (below) to redact the MFA `otp_code`/`recovery_code`.
#[derive(Clone, Default)]
pub struct LoginOptions {
    pub scope: Option<String>,
    pub otp_code: Option<String>,
    pub recovery_code: Option<String>,
}

impl fmt::Debug for LoginOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginOptions")
            .field("scope", &self.scope)
            .field(
                "otp_code",
                &self.otp_code.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field(
                "recovery_code",
                &self.recovery_code.as_ref().map(|_| crate::redact::REDACTED),
            )
            .finish()
    }
}

/// A pending device authorization grant returned by
/// `POST /v1/auth/device/authorize`.
///
/// The user completes the sign-in by visiting `verification_uri` and entering
/// `user_code` (or by opening `verification_uri_complete`, which has the code
/// pre-filled). `raw` keeps the full response payload for custom prompts.
// `Debug` is hand-written (below) to redact `device_code` (a bearer-grade
// secret) and any secret keys the raw response body carries.
#[derive(Clone)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: Option<u64>,
    pub interval: Option<u64>,
    pub raw: Map<String, Value>,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceAuthorization")
            .field("device_code", &crate::redact::REDACTED)
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .field("raw", &crate::redact::redact_map(&self.raw))
            .finish()
    }
}

impl DeviceAuthorization {
    fn from_value(value: Value) -> Result<Self> {
        let raw = value.as_object().cloned().ok_or_else(|| {
            ThalovantError::Api("device authorization response was not a JSON object".to_string())
        })?;
        Ok(Self {
            device_code: required_device_field(&raw, "device_code")?,
            user_code: required_device_field(&raw, "user_code")?,
            verification_uri: required_device_field(&raw, "verification_uri")?,
            verification_uri_complete: raw.get("verification_uri_complete").and_then(json_string),
            expires_in: raw.get("expires_in").and_then(Value::as_u64),
            interval: raw.get("interval").and_then(Value::as_u64),
            raw,
        })
    }

    /// The polling interval requested by the server, or the protocol default.
    pub fn poll_interval(&self) -> Duration {
        self.interval
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL)
    }
}

/// Callback used to present a [`DeviceAuthorization`] to the user instead of
/// the default plain-text prompt on stdout.
pub type DevicePrompt = Box<dyn Fn(&DeviceAuthorization) + Send + Sync>;

/// Options for [`ControlPlane::login_with_browser`].
///
/// The default requests no explicit scopes (the server applies its defaults),
/// opens the verification page in the local browser, prints the plain
/// verification URI and user code on stdout, and waits up to 15 minutes for
/// the sign-in to be approved.
pub struct DeviceLoginOptions {
    pub scopes: Vec<String>,
    pub client_name: Option<String>,
    pub open_browser: bool,
    pub timeout: Duration,
    pub prompt: Option<DevicePrompt>,
}

impl Default for DeviceLoginOptions {
    fn default() -> Self {
        Self {
            scopes: Vec::new(),
            client_name: None,
            open_browser: true,
            timeout: DEFAULT_DEVICE_LOGIN_TIMEOUT,
            prompt: None,
        }
    }
}

impl fmt::Debug for DeviceLoginOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceLoginOptions")
            .field("scopes", &self.scopes)
            .field("client_name", &self.client_name)
            .field("open_browser", &self.open_browser)
            .field("timeout", &self.timeout)
            .field("prompt", &self.prompt.as_ref().map(|_| "<callback>"))
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct AnalyticsOverviewOptions {
    pub range: Option<String>,
    pub bucket: Option<String>,
    pub owner_id: Option<String>,
    pub hub_id: Option<String>,
    pub client_id: Option<String>,
    pub country: Option<String>,
    pub message: Option<String>,
    pub utterance: Option<String>,
    pub intent: Option<String>,
    pub time_start: Option<String>,
    pub time_end: Option<String>,
    pub weekday: Option<u8>,
    pub hour: Option<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryListOptions {
    pub scope: Option<String>,
    pub kind: Option<String>,
    pub owner_id: Option<String>,
    pub hub_id: Option<String>,
    pub query: Option<String>,
    pub include_deleted: bool,
    pub include_expired: bool,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Options for [`ControlPlane::release_hub`] and
/// [`ControlPlane::release_runtime_group`].
///
/// Every field is optional and only the ones you set are sent; omitted fields
/// fall back to the workspace release policy. Setting `images` switches the
/// target to `custom` mode unless you also set `mode`.
#[derive(Clone, Debug, Default)]
pub struct ReleaseOptions {
    pub channel: Option<String>,
    pub mode: Option<String>,
    pub version: Option<String>,
    pub images: Option<BTreeMap<String, String>>,
    pub reason: Option<String>,
}

/// Options for [`ControlPlane::list_marketplace_skills`].
///
/// `owner_id` and `include_inactive` are honored for admin tokens only; the API
/// silently scopes a non-admin caller to their own tenant and to active entries
/// instead of failing.
#[derive(Clone, Debug, Default)]
pub struct MarketplaceSkillsOptions {
    pub owner_id: Option<String>,
    pub include_inactive: bool,
    pub force_refresh: bool,
}

/// Options for [`ControlPlane::install_runtime_group_skill`].
///
/// The default installs an active skill from the marketplace catalog
/// (`source_type` of `catalog`). A `git` install needs `source_ref` set to the
/// repository URL.
#[derive(Clone, Debug)]
pub struct SkillInstallOptions {
    pub marketplace_skill_id: Option<String>,
    pub source_type: String,
    pub source_ref: Option<String>,
    pub version_pin: Option<String>,
    pub active: bool,
}

impl Default for SkillInstallOptions {
    fn default() -> Self {
        Self {
            marketplace_skill_id: None,
            source_type: DEFAULT_SKILL_SOURCE_TYPE.to_string(),
            source_ref: None,
            version_pin: None,
            active: true,
        }
    }
}

// `Debug` is hand-written (below) to redact the client credentials carried by
// the `identity`, `hub`, and `client` fields (`client` holds the raw
// `POST /v1/clients` body with apiKey/password/cryptoKey).
#[derive(Clone)]
pub struct BootstrapIdentityResult {
    pub identity: Identity,
    pub hub: Value,
    pub client: Value,
    pub endpoint: Option<SelectedHubEndpoint>,
}

impl fmt::Debug for BootstrapIdentityResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapIdentityResult")
            // Identity has its own redacting Debug.
            .field("identity", &self.identity)
            .field("hub", &crate::redact::redact_value(&self.hub))
            .field("client", &crate::redact::redact_value(&self.client))
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl ControlPlane {
    pub fn new(api_url: impl Into<String>, access_token: Option<String>) -> Self {
        ensure_rustls_provider();
        Self {
            api_url: normalize_control_api_url(api_url.into()),
            access_token,
            user_agent: DEFAULT_CONTROL_USER_AGENT.to_string(),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn with_access_token(access_token: impl Into<String>) -> Self {
        Self::new(DEFAULT_CONTROL_API_URL, Some(access_token.into()))
    }

    pub async fn login(
        &mut self,
        email: impl Into<String>,
        password: impl Into<String>,
        scope: Option<String>,
    ) -> Result<Value> {
        self.login_with_options(
            email,
            password,
            LoginOptions {
                scope,
                ..Default::default()
            },
        )
        .await
    }

    pub async fn login_with_options(
        &mut self,
        email: impl Into<String>,
        password: impl Into<String>,
        opts: LoginOptions,
    ) -> Result<Value> {
        let mut body = Map::from_iter([
            ("email".to_string(), Value::String(email.into())),
            ("password".to_string(), Value::String(password.into())),
        ]);
        if let Some(scope) = opts.scope.filter(|value| !value.trim().is_empty()) {
            body.insert("scope".to_string(), Value::String(scope));
        }
        if let Some(otp_code) = opts.otp_code.filter(|value| !value.trim().is_empty()) {
            body.insert("otp_code".to_string(), Value::String(otp_code));
        }
        if let Some(recovery_code) = opts.recovery_code.filter(|value| !value.trim().is_empty()) {
            body.insert("recovery_code".to_string(), Value::String(recovery_code));
        }
        let token = self
            .request(
                "POST",
                "/v1/auth/token",
                Some(Value::Object(body)),
                None,
                false,
            )
            .await?;
        let access_token = token
            .get("access_token")
            .and_then(json_string)
            .ok_or_else(|| {
                ThalovantError::Api("token response did not include access_token".to_string())
            })?;
        self.access_token = Some(access_token);
        Ok(token)
    }

    /// Sign in through the browser device flow and store the API token.
    ///
    /// This is the sign-in path for accounts without a password (for example
    /// Google sign-in). It requests a device authorization, tells the user to
    /// visit `verification_uri` and enter the short `user_code` (set
    /// `options.prompt` to present the [`DeviceAuthorization`] yourself),
    /// optionally opens the browser at `verification_uri_complete`, and polls
    /// until the request is approved, denied, expired, or `options.timeout`
    /// elapses.
    ///
    /// On approval the returned `access_token` is a durable scoped API token
    /// and is stored on `self.access_token` exactly like [`ControlPlane::login`].
    ///
    /// Errors: [`ThalovantError::DeviceAuthorizationDenied`],
    /// [`ThalovantError::DeviceAuthorizationExpired`], and
    /// [`ThalovantError::Timeout`] when the wait exceeds `options.timeout`.
    pub async fn login_with_browser(&mut self, options: DeviceLoginOptions) -> Result<Value> {
        let DeviceLoginOptions {
            scopes,
            client_name,
            open_browser,
            timeout,
            prompt,
        } = options;
        let mut body = Map::new();
        if !scopes.is_empty() {
            body.insert(
                "scopes".to_string(),
                Value::Array(scopes.into_iter().map(Value::String).collect()),
            );
        }
        if let Some(client_name) = client_name.filter(|value| !value.trim().is_empty()) {
            body.insert("client_name".to_string(), Value::String(client_name));
        }
        let grant = self
            .request(
                "POST",
                "/v1/auth/device/authorize",
                Some(Value::Object(body)),
                None,
                false,
            )
            .await?;
        let grant = DeviceAuthorization::from_value(grant)?;
        match prompt.as_ref() {
            Some(prompt) => prompt(&grant),
            None => println!(
                "To sign in, visit {} and enter the code {}",
                grant.verification_uri, grant.user_code
            ),
        }
        if open_browser {
            if let Some(uri) = grant.verification_uri_complete.as_deref() {
                open_url_in_browser(uri);
            }
        }
        let token = self
            .poll_device_token(&grant.device_code, grant.poll_interval(), timeout)
            .await?;
        let access_token = token
            .get("access_token")
            .and_then(json_string)
            .ok_or_else(|| {
                ThalovantError::Api("token response did not include access_token".to_string())
            })?;
        self.access_token = Some(access_token);
        Ok(token)
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        interval: Duration,
        timeout: Duration,
    ) -> Result<Value> {
        let start = std::time::Instant::now();
        self.poll_device_token_with(
            device_code,
            interval,
            timeout,
            tokio::time::sleep,
            move || start.elapsed(),
        )
        .await
    }

    /// Poll the device token endpoint until approval or a terminal state.
    ///
    /// `sleep` and `elapsed` are injectable so tests can drive the loop
    /// without real waiting.
    async fn poll_device_token_with<SleepFut>(
        &self,
        device_code: &str,
        interval: Duration,
        timeout: Duration,
        mut sleep: impl FnMut(Duration) -> SleepFut,
        mut elapsed: impl FnMut() -> Duration,
    ) -> Result<Value>
    where
        SleepFut: Future<Output = ()>,
    {
        let body = json!({ "device_code": device_code });
        let mut wait = interval;
        loop {
            let (status, text) = self
                .send_request(
                    "POST",
                    "/v1/auth/device/token",
                    Some(body.clone()),
                    None,
                    false,
                )
                .await?;
            if status.is_success() {
                if text.trim().is_empty() {
                    return Err(ThalovantError::Api(
                        "device token response was empty".to_string(),
                    ));
                }
                return serde_json::from_str::<Value>(&text).map_err(ThalovantError::from);
            }
            let parsed = serde_json::from_str::<Value>(&text).ok();
            let error = (status == reqwest::StatusCode::BAD_REQUEST)
                .then(|| {
                    parsed
                        .as_ref()
                        .and_then(|value| value.get("error"))
                        .and_then(Value::as_str)
                })
                .flatten();
            match error {
                Some("authorization_pending") => {}
                Some("slow_down") => wait += Duration::from_secs(5),
                Some("access_denied") => return Err(ThalovantError::DeviceAuthorizationDenied),
                Some("expired_token") => return Err(ThalovantError::DeviceAuthorizationExpired),
                _ => {
                    return Err(ThalovantError::Api(format!(
                        "HTTP {status}: {}",
                        server_error_detail(&text)
                    )))
                }
            }
            let remaining = timeout.saturating_sub(elapsed());
            if remaining.is_zero() {
                return Err(ThalovantError::Timeout(
                    "timed out waiting for the device sign-in to be approved".to_string(),
                ));
            }
            sleep(wait.min(remaining)).await;
        }
    }

    pub async fn list_hubs(
        &self,
        limit: Option<u32>,
        cursor: Option<&str>,
        owner_id: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![format!("limit={}", limit.unwrap_or(100))];
        if let Some(cursor) = cursor {
            params.push(format!("cursor={}", urlencoding::encode(cursor)));
        }
        if let Some(owner_id) = owner_id {
            params.push(format!("owner_id={}", urlencoding::encode(owner_id)));
        }
        self.request(
            "GET",
            &format!("/v1/hubs?{}", params.join("&")),
            None,
            None,
            true,
        )
        .await
    }

    pub async fn list_public_hubs(
        &self,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![format!("limit={}", limit.unwrap_or(24))];
        if let Some(cursor) = cursor {
            params.push(format!("cursor={}", urlencoding::encode(cursor)));
        }
        self.request(
            "GET",
            &format!("/v1/public/hubs?{}", params.join("&")),
            None,
            None,
            false,
        )
        .await
    }

    pub async fn get_operation(&self, operation_id: &str) -> Result<OperationResource> {
        let value = self
            .request(
                "GET",
                &format!("/v1/operations/{}", urlencoding::encode(operation_id)),
                None,
                None,
                true,
            )
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn list_memory_items(&self, opts: MemoryListOptions) -> Result<Value> {
        let mut params = Vec::new();
        push_query_param(&mut params, "scope", opts.scope.as_deref());
        push_query_param(&mut params, "kind", opts.kind.as_deref());
        push_query_param(&mut params, "owner_id", opts.owner_id.as_deref());
        push_query_param(&mut params, "hub_id", opts.hub_id.as_deref());
        push_query_param(&mut params, "q", opts.query.as_deref());
        if opts.include_deleted {
            params.push("include_deleted=true".to_string());
        }
        if opts.include_expired {
            params.push("include_expired=true".to_string());
        }
        if let Some(limit) = opts.limit {
            params.push(format!("limit={limit}"));
        }
        if let Some(offset) = opts.offset {
            params.push(format!("offset={offset}"));
        }
        let path = if params.is_empty() {
            "/v1/memory".to_string()
        } else {
            format!("/v1/memory?{}", params.join("&"))
        };
        self.request("GET", &path, None, None, true).await
    }

    pub async fn get_memory_summary(&self, owner_id: Option<&str>) -> Result<Value> {
        let mut params = Vec::new();
        push_query_param(&mut params, "owner_id", owner_id);
        let path = if params.is_empty() {
            "/v1/memory/summary".to_string()
        } else {
            format!("/v1/memory/summary?{}", params.join("&"))
        };
        self.request("GET", &path, None, None, true).await
    }

    pub async fn create_memory_item(&self, payload: Value) -> Result<Value> {
        self.request("POST", "/v1/memory", Some(payload), None, true)
            .await
    }

    pub async fn get_memory_item(&self, memory_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/v1/memory/{}", urlencoding::encode(memory_id)),
            None,
            None,
            true,
        )
        .await
    }

    pub async fn update_memory_item(&self, memory_id: &str, payload: Value) -> Result<Value> {
        self.request(
            "PATCH",
            &format!("/v1/memory/{}", urlencoding::encode(memory_id)),
            Some(payload),
            None,
            true,
        )
        .await
    }

    pub async fn delete_memory_item(&self, memory_id: &str) -> Result<()> {
        let _ = self
            .request(
                "DELETE",
                &format!("/v1/memory/{}", urlencoding::encode(memory_id)),
                None,
                None,
                true,
            )
            .await?;
        Ok(())
    }

    pub async fn get_analytics_overview(&self, opts: AnalyticsOverviewOptions) -> Result<Value> {
        let mut params = Vec::new();
        push_query_param(&mut params, "range", opts.range.as_deref());
        push_query_param(&mut params, "bucket", opts.bucket.as_deref());
        // `owner_id` is honored only for admin tokens; the API silently scopes a
        // non-admin caller to their own tenant, so it is always safe to send.
        push_query_param(&mut params, "owner_id", opts.owner_id.as_deref());
        push_query_param(&mut params, "hub_id", opts.hub_id.as_deref());
        push_query_param(&mut params, "client_id", opts.client_id.as_deref());
        push_query_param(&mut params, "country", opts.country.as_deref());
        push_query_param(&mut params, "message", opts.message.as_deref());
        push_query_param(&mut params, "utterance", opts.utterance.as_deref());
        push_query_param(&mut params, "intent", opts.intent.as_deref());
        push_query_param(&mut params, "time_start", opts.time_start.as_deref());
        push_query_param(&mut params, "time_end", opts.time_end.as_deref());
        if let Some(weekday) = opts.weekday {
            params.push(format!("weekday={weekday}"));
        }
        if let Some(hour) = opts.hour {
            params.push(format!("hour={hour}"));
        }
        let path = if params.is_empty() {
            "/v1/analytics/overview".to_string()
        } else {
            format!("/v1/analytics/overview?{}", params.join("&"))
        };
        self.request("GET", &path, None, None, true).await
    }

    pub async fn get_hub(&self, hub_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/v1/hubs/{}", urlencoding::encode(hub_id)),
            None,
            None,
            true,
        )
        .await
    }

    pub async fn get_public_hub(&self, hub_ref: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/v1/public/hubs/{}", urlencoding::encode(hub_ref)),
            None,
            None,
            false,
        )
        .await
    }

    /// Create a hub.
    ///
    /// `payload` mirrors the API's hub create body: `name` and `spec` are
    /// required, and `slug`, `namespace`, `runtime_group_id`, `domain`,
    /// `active`, `visibility`, `capacity_profile`, and `owner_id` are optional.
    ///
    /// The request is idempotent: a generated `Idempotency-Key` is sent unless
    /// you pass your own, so a create retried after a timeout returns the first
    /// hub instead of making a second one.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope. A
    /// free-plan token fails with HTTP 402 and a token without the scope with
    /// HTTP 403, both surfaced as [`ThalovantError::Api`].
    pub async fn create_hub(
        &self,
        payload: Value,
        idempotency_key: Option<String>,
    ) -> Result<Value> {
        let key = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
        self.request(
            "POST",
            "/v1/hubs",
            Some(payload),
            Some(single_header("Idempotency-Key", &key)?),
            true,
        )
        .await
    }

    /// Partially update a hub.
    ///
    /// The API enforces optimistic locking on this route, so `etag` is
    /// required, not optional: pass the `etag` from the hub resource you read
    /// and the SDK sends it as `If-Match`. A stale or missing value fails the
    /// request with HTTP 412 and changes nothing; re-read the hub with
    /// [`ControlPlane::get_hub`] and retry with the new `etag`.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn update_hub(&self, hub_id: &str, payload: Value, etag: &str) -> Result<Value> {
        self.request(
            "PATCH",
            &format!("/v1/hubs/{}", urlencoding::encode(hub_id)),
            Some(payload),
            Some(single_header("If-Match", etag)?),
            true,
        )
        .await
    }

    /// Delete a hub along with its dependent clients and ACLs.
    ///
    /// Like [`ControlPlane::update_hub`] this route requires the hub's current
    /// `etag`, sent as `If-Match`; a stale or missing value fails with HTTP
    /// 412.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn delete_hub(&self, hub_id: &str, etag: &str) -> Result<()> {
        let _ = self
            .request(
                "DELETE",
                &format!("/v1/hubs/{}", urlencoding::encode(hub_id)),
                None,
                Some(single_header("If-Match", etag)?),
                true,
            )
            .await?;
        Ok(())
    }

    /// Apply a hub release policy and return the updated hub.
    ///
    /// Every option is optional; omitted fields fall back to the workspace
    /// release policy. Passing `images` switches the hub to `custom` mode
    /// unless you also pass `mode`.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn release_hub(&self, hub_id: &str, opts: ReleaseOptions) -> Result<Value> {
        self.request(
            "POST",
            &format!("/v1/hubs/{}/release", urlencoding::encode(hub_id)),
            Some(release_payload(opts)),
            None,
            true,
        )
        .await
    }

    /// Rate a public hub from 1 to 5 and return the updated hub.
    ///
    /// Only public hubs can be rated, and owners cannot rate their own hubs.
    /// Requires a token with the `hubs:write` scope; unlike the provisioning
    /// routes this one is **not** paid-gated, so a free-plan token works.
    pub async fn set_hub_rating(&self, hub_id: &str, rating: u8) -> Result<Value> {
        self.request(
            "PUT",
            &format!("/v1/hubs/{}/rating", urlencoding::encode(hub_id)),
            Some(json!({ "rating": rating })),
            None,
            true,
        )
        .await
    }

    /// Remove the caller's rating from a public hub and return the hub.
    ///
    /// Requires a token with the `hubs:write` scope; not paid-gated.
    pub async fn clear_hub_rating(&self, hub_id: &str) -> Result<Value> {
        self.request(
            "DELETE",
            &format!("/v1/hubs/{}/rating", urlencoding::encode(hub_id)),
            None,
            None,
            true,
        )
        .await
    }

    /// Read the live skill and intent inventory a hub runtime exposes.
    ///
    /// Requires a token with the `hubs:inspect` scope. This is the one
    /// discovery route that fails when nothing is reporting: the API answers
    /// HTTP 409 when the hub has no connected client that can report
    /// inventory. The runtime-group reads
    /// ([`ControlPlane::list_runtime_group_inventory`],
    /// [`ControlPlane::list_runtime_group_marketplace`]) return empty data with
    /// a pending `source` instead.
    pub async fn get_hub_runtime_capabilities(&self, hub_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!(
                "/v1/hubs/{}/runtime-capabilities",
                urlencoding::encode(hub_id)
            ),
            None,
            None,
            true,
        )
        .await
    }

    /// List runtime groups visible to the authenticated user.
    ///
    /// Requires a token with the `hubs:read` scope.
    pub async fn list_runtime_groups(&self, owner_id: Option<&str>) -> Result<Value> {
        let mut params = Vec::new();
        push_query_param(&mut params, "owner_id", owner_id);
        let path = if params.is_empty() {
            "/v1/runtime-groups".to_string()
        } else {
            format!("/v1/runtime-groups?{}", params.join("&"))
        };
        self.request("GET", &path, None, None, true).await
    }

    /// Fetch one runtime group.
    ///
    /// Requires a token with the `hubs:read` scope.
    pub async fn get_runtime_group(&self, runtime_group_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!(
                "/v1/runtime-groups/{}",
                urlencoding::encode(runtime_group_id)
            ),
            None,
            None,
            true,
        )
        .await
    }

    /// Create a runtime group.
    ///
    /// `payload` takes the API's create body: `name` is required, and
    /// `description`, `environment`, `owner_id`, and `clone_from_default` are
    /// optional. This route reads no `Idempotency-Key`.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn create_runtime_group(&self, payload: Value) -> Result<Value> {
        self.request("POST", "/v1/runtime-groups", Some(payload), None, true)
            .await
    }

    /// Update a runtime group's `name`, `description`, or `spec`.
    ///
    /// `spec` patches `replicas` and container `resources`. Unlike the hub
    /// routes this one does **not** use `If-Match`.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn update_runtime_group(
        &self,
        runtime_group_id: &str,
        payload: Value,
    ) -> Result<Value> {
        self.request(
            "PATCH",
            &format!(
                "/v1/runtime-groups/{}",
                urlencoding::encode(runtime_group_id)
            ),
            Some(payload),
            None,
            true,
        )
        .await
    }

    /// Read a runtime group's runtime configuration and personas.
    ///
    /// Requires a token with the `hubs:read` scope.
    pub async fn get_runtime_group_config(&self, runtime_group_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!(
                "/v1/runtime-groups/{}/config",
                urlencoding::encode(runtime_group_id)
            ),
            None,
            None,
            true,
        )
        .await
    }

    /// Merge runtime configuration into a runtime group.
    ///
    /// The API merges `config` into the stored configuration rather than
    /// replacing it, and marks the group pending so the runtime operator
    /// reconciles the change. `personas` is sent, and therefore replaced, only
    /// when you pass `Some(..)`.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn update_runtime_group_config(
        &self,
        runtime_group_id: &str,
        config: Value,
        personas: Option<Value>,
    ) -> Result<Value> {
        let mut body = Map::from_iter([("config".to_string(), config)]);
        if let Some(personas) = personas {
            body.insert("personas".to_string(), personas);
        }
        self.request(
            "PATCH",
            &format!(
                "/v1/runtime-groups/{}/config",
                urlencoding::encode(runtime_group_id)
            ),
            Some(Value::Object(body)),
            None,
            true,
        )
        .await
    }

    /// Apply a runtime image policy and return the updated runtime group.
    ///
    /// Options behave like [`ControlPlane::release_hub`].
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn release_runtime_group(
        &self,
        runtime_group_id: &str,
        opts: ReleaseOptions,
    ) -> Result<Value> {
        self.request(
            "POST",
            &format!(
                "/v1/runtime-groups/{}/release",
                urlencoding::encode(runtime_group_id)
            ),
            Some(release_payload(opts)),
            None,
            true,
        )
        .await
    }

    /// Delete a runtime group.
    ///
    /// The API answers HTTP 409 for the workspace default group and for a
    /// group that still has hubs attached.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn delete_runtime_group(&self, runtime_group_id: &str) -> Result<()> {
        let _ = self
            .request(
                "DELETE",
                &format!(
                    "/v1/runtime-groups/{}",
                    urlencoding::encode(runtime_group_id)
                ),
                None,
                None,
                true,
            )
            .await?;
        Ok(())
    }

    /// Install (or re-install) a skill in a runtime group.
    ///
    /// The default [`SkillInstallOptions::source_type`] of `catalog` installs a
    /// marketplace skill and requires the skill to exist in the catalog; `git`
    /// installs need a `source_ref` repository URL. Installing a skill that is
    /// already present updates the existing entry.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope. Paid
    /// marketplace skills also need marketplace access on the tenant plan.
    pub async fn install_runtime_group_skill(
        &self,
        runtime_group_id: &str,
        skill_id: &str,
        opts: SkillInstallOptions,
    ) -> Result<Value> {
        let mut body = Map::from_iter([
            ("skill_id".to_string(), Value::String(skill_id.to_string())),
            ("source_type".to_string(), Value::String(opts.source_type)),
            ("active".to_string(), Value::Bool(opts.active)),
        ]);
        if let Some(marketplace_skill_id) = opts.marketplace_skill_id {
            body.insert(
                "marketplace_skill_id".to_string(),
                Value::String(marketplace_skill_id),
            );
        }
        if let Some(source_ref) = opts.source_ref {
            body.insert("source_ref".to_string(), Value::String(source_ref));
        }
        if let Some(version_pin) = opts.version_pin {
            body.insert("version_pin".to_string(), Value::String(version_pin));
        }
        self.request(
            "POST",
            &format!(
                "/v1/runtime-groups/{}/skills",
                urlencoding::encode(runtime_group_id)
            ),
            Some(Value::Object(body)),
            None,
            true,
        )
        .await
    }

    /// Remove a skill from a runtime group.
    ///
    /// Requires a paid plan and a token with the `hubs:write` scope.
    pub async fn uninstall_runtime_group_skill(
        &self,
        runtime_group_id: &str,
        skill_id: &str,
    ) -> Result<()> {
        let _ = self
            .request(
                "DELETE",
                &format!(
                    "/v1/runtime-groups/{}/skills/{}",
                    urlencoding::encode(runtime_group_id),
                    urlencoding::encode(skill_id)
                ),
                None,
                None,
                true,
            )
            .await?;
        Ok(())
    }

    /// List the marketplace skill catalog visible to the authenticated user.
    ///
    /// Returns `{"data": [...]}` where each entry carries the catalog fields an
    /// install needs — `skill_id`, `source_type`, `source_ref`,
    /// `package_name`, `version` compatibility, `config_schema` and
    /// `secret_schema` — alongside presentation and access fields such as
    /// `category`, `tags`, `verified`, `access_tier` and `billing_sku`. Global
    /// catalog entries and the caller's own tenant entries are both included.
    ///
    /// [`MarketplaceSkillsOptions::owner_id`] and
    /// [`MarketplaceSkillsOptions::include_inactive`] are honored for admin
    /// tokens only; the API silently scopes a non-admin caller to their own
    /// tenant and to active entries rather than failing.
    /// [`MarketplaceSkillsOptions::force_refresh`] re-syncs the global catalog
    /// from its source before answering, which is slower.
    ///
    /// Requires a token with the `hubs:read` scope. Unlike the provisioning
    /// routes this catalog is **not** paid-gated, so free-plan callers can
    /// browse the marketplace before upgrading — only the install itself needs
    /// a paid plan.
    pub async fn list_marketplace_skills(&self, opts: MarketplaceSkillsOptions) -> Result<Value> {
        let mut params = Vec::new();
        push_query_param(&mut params, "owner_id", opts.owner_id.as_deref());
        if opts.include_inactive {
            params.push("include_inactive=true".to_string());
        }
        if opts.force_refresh {
            params.push("force_refresh=true".to_string());
        }
        let path = if params.is_empty() {
            "/v1/marketplace/skills".to_string()
        } else {
            format!("/v1/marketplace/skills?{}", params.join("&"))
        };
        self.request("GET", &path, None, None, true).await
    }

    /// List the marketplace catalog resolved against one runtime group.
    ///
    /// This is the discovery view to use before installing: every catalog entry
    /// is returned with the group's own state folded in — whether the skill is
    /// desired (`active`, `version_pin`, `source_type`), whether it was
    /// observed running (`observed_source`, `observed_at`, intent counts),
    /// operator status fields, and the access verdict for the tenant plan
    /// (`purchase_required`, `installable`, `access_message`). The envelope
    /// also carries `runtime_group_id`, `observed_at`, `source`,
    /// `operator_phase` and `operator_message`.
    ///
    /// `refresh_inventory` forces a live read from the runtime operator instead
    /// of answering from the cached inventory snapshot.
    ///
    /// Requires a token with the `hubs:inspect` scope; no paid plan is needed
    /// to browse. The API answers HTTP 404 for an unknown group and HTTP 403
    /// when the caller does not own it, and returns empty data with a pending
    /// `source` — never HTTP 409 — when no client is connected.
    pub async fn list_runtime_group_marketplace(
        &self,
        runtime_group_id: &str,
        refresh_inventory: bool,
    ) -> Result<Value> {
        let mut path = format!(
            "/v1/runtime-groups/{}/marketplace",
            urlencoding::encode(runtime_group_id)
        );
        if refresh_inventory {
            path.push_str("?refresh_inventory=true");
        }
        self.request("GET", &path, None, None, true).await
    }

    /// List the skills a runtime group is actually observed running.
    ///
    /// Where [`ControlPlane::list_runtime_group_marketplace`] answers "what
    /// could be installed here", this answers "what is loaded right now": each
    /// entry carries `skill_id`, `version`, `source`, `active`,
    /// `adapt_intents`, `padatious_intents`, `total_intents` and
    /// `observed_at`. The envelope reports `source` — the observation's
    /// provenance, one of `ovos-runtime-operator`, `runtime-group-cache` or
    /// `ovos-runtime-operator-pending` — plus `operator_phase` and
    /// `operator_message`.
    ///
    /// `refresh` forces a live operator read; the API also refreshes on its own
    /// when it holds no cached snapshot. Unlike
    /// [`ControlPlane::get_hub_runtime_capabilities`] this route does not answer
    /// HTTP 409 when nothing is reporting — it returns an empty `data` list
    /// with a pending `source` instead.
    ///
    /// Requires a token with the `hubs:inspect` scope; no paid plan is needed.
    pub async fn list_runtime_group_inventory(
        &self,
        runtime_group_id: &str,
        refresh: bool,
    ) -> Result<Value> {
        let mut path = format!(
            "/v1/runtime-groups/{}/inventory",
            urlencoding::encode(runtime_group_id)
        );
        if refresh {
            path.push_str("?refresh=true");
        }
        self.request("GET", &path, None, None, true).await
    }

    pub async fn create_client(
        &self,
        payload: Value,
        idempotency_key: Option<String>,
    ) -> Result<Value> {
        let key = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
        self.request(
            "POST",
            "/v1/clients",
            Some(payload),
            Some(single_header("Idempotency-Key", &key)?),
            true,
        )
        .await
    }

    pub async fn create_client_identity_for_hub_id(
        &self,
        hub_id: &str,
        opts: BootstrapIdentityOptions,
    ) -> Result<BootstrapIdentityResult> {
        let hub = self.get_hub(hub_id).await?;
        self.create_client_identity(hub, opts).await
    }

    pub async fn create_client_identity(
        &self,
        hub: Value,
        opts: BootstrapIdentityOptions,
    ) -> Result<BootstrapIdentityResult> {
        if opts.name.trim().is_empty() {
            return Err(ThalovantError::Api("client name is required".to_string()));
        }
        let hub_id = hub
            .get("id")
            .and_then(json_string)
            .ok_or_else(|| ThalovantError::Api("hub resource is missing id".to_string()))?;
        let site_id = clean_site_id(opts.site_id.as_deref().unwrap_or(&opts.name));
        let api_key = new_secret();
        let password = new_secret();
        let crypto_key = new_secret();

        let mut spec = opts.spec.clone();
        spec.entry("version".to_string())
            .or_insert_with(|| Value::String("1".to_string()));
        spec.insert("apiKey".to_string(), Value::String(api_key.clone()));
        spec.insert("password".to_string(), Value::String(password.clone()));
        spec.insert("cryptoKey".to_string(), Value::String(crypto_key.clone()));
        spec.insert("siteId".to_string(), Value::String(site_id.clone()));

        let mut payload = Map::from_iter([
            ("hub_id".to_string(), Value::String(hub_id)),
            ("name".to_string(), Value::String(opts.name.clone())),
            ("spec".to_string(), Value::Object(spec)),
            (
                "active".to_string(),
                Value::Bool(opts.active.unwrap_or(true)),
            ),
        ]);
        if let Some(owner_id) = opts.owner_id {
            payload.insert("owner_id".to_string(), Value::String(owner_id));
        }

        let client = self
            .create_client(Value::Object(payload), opts.idempotency_key)
            .await?;
        let protocols = HubProtocolSettings::from_value(&hub);
        let endpoints = HubDataPlaneEndpoints::from_hub(&hub);
        let preferred = if opts.preferred_protocols.is_empty() {
            DEFAULT_PROTOCOL_PREFERENCE
        } else {
            opts.preferred_protocols.as_slice()
        };
        let endpoint = select_data_plane_endpoint(&endpoints, &protocols, preferred);
        let default_master = default_master(&hub, &endpoints, endpoint.as_ref())?;
        let identity = if let Some(initial_identify) =
            client.get("initial_identify").and_then(Value::as_object)
        {
            let mut identity = initial_identify.clone();
            identity.insert(
                "data_plane_endpoints".to_string(),
                Value::Object(endpoints.as_map(false)),
            );
            identity.insert("protocols".to_string(), protocols.as_spec_value());
            Identity::from_value(Value::Object(identity))?
        } else {
            Identity {
                access_key: api_key,
                password,
                crypto_key: Some(crypto_key),
                site_id,
                default_master,
                default_port: 443,
                default_path: String::new(),
                public_key: None,
                metadata: Map::new(),
                name: Some(opts.name),
                data_plane_endpoints: endpoints,
                protocols,
                mqtt: None,
            }
        };
        Ok(BootstrapIdentityResult {
            identity,
            hub,
            client,
            endpoint,
        })
    }

    pub fn require_runtime_protocol(
        &self,
        result: &BootstrapIdentityResult,
        protocol: HubProtocol,
    ) -> Result<SelectedHubEndpoint> {
        if protocol == HubProtocol::Mqtt && result.identity.mqtt.is_none() {
            return Err(ThalovantError::UnsupportedProtocol(
                "MQTT is enabled, but the API did not return client-scoped MQTT broker credentials"
                    .to_string(),
            ));
        }
        let endpoint = result.identity.endpoint_for(protocol).ok_or_else(|| {
            ThalovantError::UnsupportedProtocol(format!(
                "this hub does not expose a {protocol:?} endpoint for the SDK runtime"
            ))
        })?;
        Ok(SelectedHubEndpoint { protocol, endpoint })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: Option<HeaderMap>,
        auth: bool,
    ) -> Result<Value> {
        let (status, body) = self.send_request(method, path, body, headers, auth).await?;
        if !status.is_success() {
            return Err(ThalovantError::Api(format!(
                "HTTP {status}: {}",
                server_error_detail(&body)
            )));
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str::<Value>(&body).map_err(ThalovantError::from)
    }

    async fn send_request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: Option<HeaderMap>,
        auth: bool,
    ) -> Result<(reqwest::StatusCode, String)> {
        let mut request_headers = HeaderMap::new();
        request_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        request_headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent)
                .map_err(|err| ThalovantError::Api(err.to_string()))?,
        );
        if body.is_some() {
            request_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if let Some(headers) = headers {
            request_headers.extend(headers);
        }
        if auth {
            let token = self
                .access_token
                .as_ref()
                .ok_or_else(|| ThalovantError::Api("missing access token".to_string()))?;
            request_headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|err| ThalovantError::Api(err.to_string()))?,
            );
        }
        let method = method
            .parse::<reqwest::Method>()
            .map_err(|err| ThalovantError::Api(err.to_string()))?;
        let url = format!("{}{}", self.api_url, path.trim_start_matches('/'));
        let mut request = self
            .http_client
            .request(method, url)
            .headers(request_headers);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|err| ThalovantError::Api(err.to_string()))?;
        let status = response.status();
        let body = if status.is_success() {
            response
                .text()
                .await
                .map_err(|err| ThalovantError::Api(err.to_string()))?
        } else {
            response.text().await.unwrap_or_default()
        };
        Ok((status, body))
    }
}

impl Default for ControlPlane {
    fn default() -> Self {
        Self::new(DEFAULT_CONTROL_API_URL, None)
    }
}

impl BootstrapIdentityResult {
    pub fn selected_protocol(&self) -> Option<HubProtocol> {
        self.endpoint.as_ref().map(|endpoint| endpoint.protocol)
    }

    pub fn as_value(&self, include_secrets: bool) -> Value {
        let mut identity = Map::from_iter([
            (
                "site_id".to_string(),
                Value::String(self.identity.site_id.clone()),
            ),
            (
                "default_master".to_string(),
                Value::String(self.identity.default_master.clone()),
            ),
            (
                "default_port".to_string(),
                Value::from(self.identity.default_port),
            ),
            (
                "default_path".to_string(),
                Value::String(self.identity.default_path.clone()),
            ),
        ]);
        let endpoints = self.identity.data_plane_endpoints.as_map(!include_secrets);
        if !endpoints.is_empty() {
            identity.insert("data_plane_endpoints".to_string(), Value::Object(endpoints));
        }
        if !self.identity.metadata.is_empty() {
            identity.insert(
                "metadata".to_string(),
                Value::Object(self.identity.metadata.clone()),
            );
        }
        if include_secrets {
            identity.insert(
                "access_key".to_string(),
                Value::String(self.identity.access_key.clone()),
            );
            identity.insert(
                "password".to_string(),
                Value::String(self.identity.password.clone()),
            );
            if let Some(crypto_key) = self.identity.crypto_key.clone() {
                identity.insert("crypto_key".to_string(), Value::String(crypto_key));
            }
        }
        if let Some(mqtt) = self.identity.mqtt.as_ref() {
            identity.insert("mqtt".to_string(), mqtt.as_value(include_secrets));
        }
        // The hub and (especially) client resources carry the credentials minted
        // by `POST /v1/clients` (apiKey/password/cryptoKey). Gate their secret
        // subkeys behind `include_secrets`, exactly like the identity block above.
        let (hub, client) = if include_secrets {
            (self.hub.clone(), self.client.clone())
        } else {
            (
                crate::redact::redact_value(&self.hub),
                crate::redact::redact_value(&self.client),
            )
        };
        json!({
            "identity": identity,
            "hub": hub,
            "client": client,
            "selected_protocol": self.selected_protocol(),
            "selected_endpoint": self.endpoint.as_ref().map(|endpoint| endpoint.endpoint.clone()),
        })
    }
}

/// Build a one-entry header map, failing on a value HTTP cannot carry.
fn single_header(name: &'static str, value: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        name,
        HeaderValue::from_str(value).map_err(|err| ThalovantError::Api(err.to_string()))?,
    );
    Ok(headers)
}

/// Build a release-apply body, omitting the options the caller left unset.
fn release_payload(opts: ReleaseOptions) -> Value {
    let mut body = Map::new();
    if let Some(channel) = opts.channel {
        body.insert("channel".to_string(), Value::String(channel));
    }
    if let Some(mode) = opts.mode {
        body.insert("mode".to_string(), Value::String(mode));
    }
    if let Some(version) = opts.version {
        body.insert("version".to_string(), Value::String(version));
    }
    if let Some(images) = opts.images {
        body.insert(
            "images".to_string(),
            Value::Object(
                images
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            ),
        );
    }
    if let Some(reason) = opts.reason {
        body.insert("reason".to_string(), Value::String(reason));
    }
    Value::Object(body)
}

fn required_device_field(raw: &Map<String, Value>, key: &str) -> Result<String> {
    raw.get(key).and_then(json_string).ok_or_else(|| {
        ThalovantError::Api(format!(
            "device authorization response did not include {key}"
        ))
    })
}

/// Best-effort attempt to open `url` in the local browser. Failure to launch a
/// browser is never fatal; the plain verification prompt already covers it.
fn open_url_in_browser(url: &str) {
    for command in ["xdg-open", "open"] {
        let launched = std::process::Command::new(command)
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if launched.is_ok() {
            return;
        }
    }
}

fn new_secret() -> String {
    let mut raw = [0_u8; 32];
    OsRng.fill_bytes(&mut raw);
    general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn clean_site_id(value: &str) -> String {
    let cleaned = value
        .trim()
        .replace('_', "-")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    if cleaned.is_empty() {
        "thalovant-client".to_string()
    } else {
        cleaned
    }
}

fn default_master(
    hub: &Value,
    endpoints: &HubDataPlaneEndpoints,
    selected: Option<&SelectedHubEndpoint>,
) -> Result<String> {
    if let Some(https) = endpoints.https.as_deref() {
        return Ok(strip_endpoint_path(https));
    }
    if let Some(domain) = hub.get("domain").and_then(json_string) {
        if let Some(endpoint) = endpoint_from_domain(&domain, HubProtocol::Https) {
            return Ok(endpoint);
        }
    }
    if let Some(selected) = selected {
        return Ok(strip_endpoint_path(&selected.endpoint));
    }
    Err(ThalovantError::Api(
        "hub resource does not expose a usable data-plane endpoint".to_string(),
    ))
}

fn strip_endpoint_path(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return endpoint.trim_end_matches('/').to_string();
    };
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').to_string()
}

fn normalize_control_api_url(api_url: String) -> String {
    let mut normalized = api_url.trim().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        normalized = DEFAULT_CONTROL_API_URL.to_string();
    }
    if normalized.ends_with("/v1") {
        normalized.truncate(normalized.len() - 3);
    }
    format!("{}/", normalized.trim_end_matches('/'))
}

fn push_query_param(params: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        params.push(format!("{}={}", key, urlencoding::encode(value)));
    }
}

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => {
            let normalized = raw.trim();
            (!normalized.is_empty()).then(|| normalized.to_string())
        }
        Value::Null => None,
        other => Some(other.to_string().trim_matches('"').to_string()),
    }
}

/// Reduce a raw HTTP response body to a short detail that is safe to embed in an
/// error message. When the body is JSON its secret-keyed fields are redacted
/// (the `POST /v1/clients`, `/v1/auth/token`, and `/v1/auth/device/token` routes
/// are *sent* credentials that an error response may echo); whitespace is then
/// collapsed and the result is length-bounded so a large or multi-line body
/// never lands verbatim in `last_error`, logs, or a bug report.
fn server_error_detail(body: &str) -> String {
    let rendered = match serde_json::from_str::<Value>(body) {
        Ok(value) => crate::redact::redact_value(&value).to_string(),
        Err(_) => body.to_string(),
    };
    let collapsed = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX_DETAIL: usize = 200;
    if collapsed.chars().count() > MAX_DETAIL {
        let truncated: String = collapsed.chars().take(MAX_DETAIL).collect();
        format!("{truncated}...")
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn default_api_url_and_v1_normalization() {
        assert_eq!(
            ControlPlane::default().api_url,
            "https://api.thalovant.com/"
        );
        assert_eq!(
            ControlPlane::new("", None).api_url,
            "https://api.thalovant.com/"
        );
        assert_eq!(
            ControlPlane::new("https://api.thalovant.com/v1", None).api_url,
            "https://api.thalovant.com/"
        );
        assert_eq!(
            ControlPlane::new("https://dash.example.com/api/v1", None).api_url,
            "https://dash.example.com/api/"
        );
    }

    #[test]
    fn user_agents_match_crate_version() {
        let expected = format!("thalovant-rust-sdk/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(DEFAULT_CONTROL_USER_AGENT, expected);
        assert_eq!(crate::constants::DEFAULT_USER_AGENT, expected);
        assert_eq!(ControlPlane::default().user_agent, expected);
    }

    #[tokio::test]
    async fn login_sends_mfa_fields_only_when_provided() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for turn in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    request.starts_with("POST /v1/auth/token "),
                    "unexpected request: {request}"
                );
                assert!(
                    request.to_ascii_lowercase().contains(&format!(
                        "\r\nuser-agent: thalovant-rust-sdk/{}\r\n",
                        env!("CARGO_PKG_VERSION")
                    )),
                    "login must send the crate user agent: {request}"
                );
                assert!(request.contains(r#""email":"you@example.com""#));
                assert!(request.contains(r#""password":"hunter2""#));
                if turn == 0 {
                    assert!(
                        !request.contains("otp_code") && !request.contains("recovery_code"),
                        "plain login must omit MFA fields: {request}"
                    );
                    assert!(
                        !request.contains("scope"),
                        "plain login must omit scope: {request}"
                    );
                } else {
                    assert!(
                        request.contains(r#""scope":"admin""#),
                        "missing scope: {request}"
                    );
                    assert!(
                        request.contains(r#""otp_code":"123456""#),
                        "missing otp_code: {request}"
                    );
                    assert!(
                        request.contains(r#""recovery_code":"rc-1""#),
                        "missing recovery_code: {request}"
                    );
                }
                let body = r#"{"access_token":"token-1","token_type":"bearer","expires_in":3600}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write response");
            }
        });

        let mut control = ControlPlane::new(format!("http://{address}"), None);
        let token = control
            .login("you@example.com", "hunter2", None)
            .await
            .expect("login without MFA");
        assert_eq!(token["access_token"].as_str(), Some("token-1"));
        assert_eq!(control.access_token.as_deref(), Some("token-1"));

        let token = control
            .login_with_options(
                "you@example.com",
                "hunter2",
                LoginOptions {
                    scope: Some("admin".to_string()),
                    otp_code: Some("123456".to_string()),
                    recovery_code: Some("rc-1".to_string()),
                },
            )
            .await
            .expect("login with MFA");
        assert_eq!(token["access_token"].as_str(), Some("token-1"));
        server.join().expect("test server finished");
    }

    const DEVICE_GRANT_BODY: &str = r#"{"device_code":"device-code-1","user_code":"WDJB-MJHT","verification_uri":"https://dash.thalovant.com/activate","verification_uri_complete":"https://dash.thalovant.com/activate?user_code=WDJB-MJHT","expires_in":900,"interval":0}"#;
    const DEVICE_TOKEN_BODY: &str = r#"{"access_token":"device-token","token_type":"bearer","scopes":["hubs:read","clients:write"],"expires_at":"2027-08-13T00:00:00Z","token_id":"token-1"}"#;

    fn write_json_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response");
    }

    /// Serves one device-authorize grant, then the scripted `(status, body)`
    /// responses for each `/v1/auth/device/token` poll.
    fn spawn_device_flow_server(
        listener: TcpListener,
        token_responses: Vec<(&'static str, &'static str)>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            for turn in 0..=token_responses.len() {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    !request.to_ascii_lowercase().contains("\r\nauthorization:"),
                    "device flow requests must not send Authorization: {request}"
                );
                if turn == 0 {
                    assert!(
                        request.starts_with("POST /v1/auth/device/authorize "),
                        "unexpected request: {request}"
                    );
                    write_json_response(&mut stream, "200 OK", DEVICE_GRANT_BODY);
                } else {
                    assert!(
                        request.starts_with("POST /v1/auth/device/token "),
                        "unexpected request: {request}"
                    );
                    assert!(
                        request.contains(r#""device_code":"device-code-1""#),
                        "missing device_code: {request}"
                    );
                    let (status, body) = token_responses[turn - 1];
                    write_json_response(&mut stream, status, body);
                }
            }
        })
    }

    #[tokio::test]
    async fn login_with_browser_polls_until_token_and_stores_it() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for turn in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    !request.to_ascii_lowercase().contains("\r\nauthorization:"),
                    "device flow requests must not send Authorization: {request}"
                );
                if turn == 0 {
                    assert!(
                        request.starts_with("POST /v1/auth/device/authorize "),
                        "unexpected request: {request}"
                    );
                    assert!(
                        request.contains(r#""scopes":["hubs:read"]"#),
                        "missing scopes: {request}"
                    );
                    assert!(
                        request.contains(r#""client_name":"rust-test""#),
                        "missing client_name: {request}"
                    );
                    write_json_response(&mut stream, "200 OK", DEVICE_GRANT_BODY);
                } else if turn < 3 {
                    assert!(
                        request.starts_with("POST /v1/auth/device/token "),
                        "unexpected request: {request}"
                    );
                    write_json_response(
                        &mut stream,
                        "400 Bad Request",
                        r#"{"error":"authorization_pending"}"#,
                    );
                } else {
                    write_json_response(&mut stream, "200 OK", DEVICE_TOKEN_BODY);
                }
            }
        });

        let mut control = ControlPlane::new(format!("http://{address}"), None);
        let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = prompts.clone();
        let token = control
            .login_with_browser(DeviceLoginOptions {
                scopes: vec!["hubs:read".to_string()],
                client_name: Some("rust-test".to_string()),
                open_browser: false,
                prompt: Some(Box::new(move |grant: &DeviceAuthorization| {
                    captured.lock().expect("record prompt").push(grant.clone());
                })),
                ..Default::default()
            })
            .await
            .expect("device login");

        assert_eq!(token["access_token"].as_str(), Some("device-token"));
        assert_eq!(token["token_id"].as_str(), Some("token-1"));
        assert_eq!(control.access_token.as_deref(), Some("device-token"));
        let prompts = prompts.lock().expect("read prompts");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].user_code, "WDJB-MJHT");
        assert_eq!(
            prompts[0].verification_uri,
            "https://dash.thalovant.com/activate"
        );
        assert_eq!(
            prompts[0].verification_uri_complete.as_deref(),
            Some("https://dash.thalovant.com/activate?user_code=WDJB-MJHT")
        );
        assert_eq!(prompts[0].raw["expires_in"].as_u64(), Some(900));
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn device_poll_slow_down_grows_interval() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let responses = [
                ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
                ("400 Bad Request", r#"{"error":"slow_down"}"#),
                ("400 Bad Request", r#"{"error":"authorization_pending"}"#),
                ("200 OK", DEVICE_TOKEN_BODY),
            ];
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    request.starts_with("POST /v1/auth/device/token "),
                    "unexpected request: {request}"
                );
                write_json_response(&mut stream, status, body);
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), None);
        let sleeps = std::sync::Mutex::new(Vec::new());
        let token = control
            .poll_device_token_with(
                "device-code-1",
                Duration::from_secs(5),
                Duration::from_secs(900),
                |wait| {
                    sleeps.lock().expect("record sleep").push(wait);
                    std::future::ready(())
                },
                || Duration::ZERO,
            )
            .await
            .expect("device token");

        assert_eq!(token["access_token"].as_str(), Some("device-token"));
        assert_eq!(
            *sleeps.lock().expect("read sleeps"),
            vec![
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(10),
            ]
        );
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn login_with_browser_fails_on_access_denied() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = spawn_device_flow_server(
            listener,
            vec![("400 Bad Request", r#"{"error":"access_denied"}"#)],
        );

        let mut control = ControlPlane::new(format!("http://{address}"), None);
        let error = control
            .login_with_browser(DeviceLoginOptions {
                open_browser: false,
                prompt: Some(Box::new(|_| {})),
                ..Default::default()
            })
            .await
            .expect_err("denied sign-in must fail");

        assert!(
            matches!(error, ThalovantError::DeviceAuthorizationDenied),
            "unexpected error: {error:?}"
        );
        assert_eq!(control.access_token, None);
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn login_with_browser_fails_on_expired_token() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = spawn_device_flow_server(
            listener,
            vec![("400 Bad Request", r#"{"error":"expired_token"}"#)],
        );

        let mut control = ControlPlane::new(format!("http://{address}"), None);
        let error = control
            .login_with_browser(DeviceLoginOptions {
                open_browser: false,
                prompt: Some(Box::new(|_| {})),
                ..Default::default()
            })
            .await
            .expect_err("expired sign-in must fail");

        assert!(
            matches!(error, ThalovantError::DeviceAuthorizationExpired),
            "unexpected error: {error:?}"
        );
        assert_eq!(control.access_token, None);
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn device_poll_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    request.starts_with("POST /v1/auth/device/token "),
                    "unexpected request: {request}"
                );
                write_json_response(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"authorization_pending"}"#,
                );
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), None);
        let now = std::cell::Cell::new(Duration::ZERO);
        let error = control
            .poll_device_token_with(
                "device-code-1",
                Duration::from_secs(5),
                Duration::from_secs(10),
                |wait| {
                    now.set(now.get() + wait);
                    std::future::ready(())
                },
                || now.get(),
            )
            .await
            .expect_err("poll must time out");

        assert!(
            matches!(error, ThalovantError::Timeout(_)),
            "unexpected error: {error:?}"
        );
        assert_eq!(now.get(), Duration::from_secs(10));
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn public_hub_discovery_does_not_send_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 4096];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    !request.to_ascii_lowercase().contains("\r\nauthorization:"),
                    "public hub requests must not send Authorization"
                );
                let body = if request.starts_with("GET /v1/public/hubs?limit=12 ") {
                    r#"{"data":[{"id":"hub-public","name":"joke-garden","slug":"joke-garden","title":"Joke Garden"}],"meta":{"count":1,"next":null},"links":{"next":null}}"#
                } else if request.starts_with("GET /v1/public/hubs/joke-garden ") {
                    r#"{"id":"hub-public","name":"joke-garden","slug":"joke-garden","title":"Joke Garden"}"#
                } else {
                    panic!("unexpected request: {request}");
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write response");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let page = control
            .list_public_hubs(Some(12), None)
            .await
            .expect("list public hubs");
        let hub = control
            .get_public_hub("joke-garden")
            .await
            .expect("get public hub");

        assert_eq!(page["data"][0]["slug"].as_str(), Some("joke-garden"));
        assert_eq!(hub["title"].as_str(), Some("Joke Garden"));
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn gets_typed_durable_operation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buffer = [0_u8; 4096];
            let size = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("GET /v1/operations/operation-1 "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("\r\nauthorization: bearer token\r\n"),
                "operation requests must send Authorization: {request}"
            );
            let body = r#"{"id":"operation-1","kind":"gitops.commit","aggregate_type":"gitops","aggregate_id":null,"status":"committed","details":{"git_commit_created":true},"git_commit_sha":"abc123","error_code":null,"error_message":null,"created_at":"2026-07-11T00:00:00Z","updated_at":"2026-07-11T00:00:01Z","committed_at":"2026-07-11T00:00:01Z","applied_at":null,"ready_at":null,"terminal_at":null,"links":{"self":"/v1/operations/operation-1"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let operation = control
            .get_operation("operation-1")
            .await
            .expect("get operation");

        assert_eq!(operation.status, OperationStatus::Committed);
        assert_eq!(operation.git_commit_sha.as_deref(), Some("abc123"));
        assert_eq!(operation.details["git_commit_created"], Value::Bool(true));
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn memory_crud_sends_filters_payloads_and_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("\r\nauthorization: bearer token\r\n"),
                    "memory requests must send Authorization: {request}"
                );
                let (status, body) = if request.starts_with("GET /v1/memory?") {
                    for fragment in [
                        "scope=workspace",
                        "kind=preference",
                        "owner_id=owner-1",
                        "hub_id=hub-1",
                        "q=timezone",
                        "include_deleted=true",
                        "include_expired=true",
                        "limit=25",
                        "offset=50",
                    ] {
                        assert!(request.contains(fragment), "missing {fragment}: {request}");
                    }
                    (
                        "200 OK",
                        r#"{"data":[{"id":"memory-1","content":"Use UTC."}],"meta":{"count":1,"next":null},"links":{"next":null}}"#,
                    )
                } else if request.starts_with("GET /v1/memory/summary?owner_id=owner-1 ") {
                    (
                        "200 OK",
                        r#"{"total":1,"by_scope":{"workspace":1},"by_kind":{"preference":1},"expired":0,"deleted":0}"#,
                    )
                } else if request.starts_with("POST /v1/memory ") {
                    assert!(
                        request.contains(r#""content":"Use UTC.""#),
                        "missing create body: {request}"
                    );
                    (
                        "201 Created",
                        r#"{"id":"memory-1","scope":"workspace","kind":"preference","content":"Use UTC."}"#,
                    )
                } else if request.starts_with("GET /v1/memory/memory-1 ") {
                    (
                        "200 OK",
                        r#"{"id":"memory-1","scope":"workspace","kind":"preference","content":"Use UTC."}"#,
                    )
                } else if request.starts_with("PATCH /v1/memory/memory-1 ") {
                    assert!(
                        request.contains(r#""content":"Use America/Toronto.""#),
                        "missing update body: {request}"
                    );
                    (
                        "200 OK",
                        r#"{"id":"memory-1","scope":"workspace","kind":"preference","content":"Use America/Toronto."}"#,
                    )
                } else if request.starts_with("DELETE /v1/memory/memory-1 ") {
                    ("204 No Content", "")
                } else {
                    panic!("unexpected request: {request}");
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write response");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let page = control
            .list_memory_items(MemoryListOptions {
                scope: Some("workspace".to_string()),
                kind: Some("preference".to_string()),
                owner_id: Some("owner-1".to_string()),
                hub_id: Some("hub-1".to_string()),
                query: Some("timezone".to_string()),
                include_deleted: true,
                include_expired: true,
                limit: Some(25),
                offset: Some(50),
            })
            .await
            .expect("list memory");
        let summary = control
            .get_memory_summary(Some("owner-1"))
            .await
            .expect("memory summary");
        let created = control
            .create_memory_item(json!({
                "scope": "workspace",
                "kind": "preference",
                "content": "Use UTC.",
            }))
            .await
            .expect("create memory");
        let item = control
            .get_memory_item("memory-1")
            .await
            .expect("get memory");
        let updated = control
            .update_memory_item(
                "memory-1",
                json!({
                    "content": "Use America/Toronto.",
                    "clear_expires_at": true,
                }),
            )
            .await
            .expect("update memory");
        control
            .delete_memory_item("memory-1")
            .await
            .expect("delete memory");

        assert_eq!(page["data"][0]["id"].as_str(), Some("memory-1"));
        assert_eq!(summary["total"].as_i64(), Some(1));
        assert_eq!(created["id"].as_str(), Some("memory-1"));
        assert_eq!(item["content"].as_str(), Some("Use UTC."));
        assert_eq!(updated["content"].as_str(), Some("Use America/Toronto."));
        server.join().expect("test server finished");
    }

    #[tokio::test]
    async fn analytics_overview_sends_filters() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut buffer = [0_u8; 4096];
            let size = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(
                request.starts_with("GET /v1/analytics/overview?"),
                "unexpected request: {request}"
            );
            assert!(
                request.contains("\r\nauthorization: Bearer token\r\n")
                    || request.contains("\r\nAuthorization: Bearer token\r\n"),
                "analytics requests must send Authorization"
            );
            for fragment in [
                "range=30d",
                "bucket=1d",
                "owner_id=owner-1",
                "hub_id=hub-1",
                "client_id=client-1",
                "country=CA",
                "message=speak",
                "utterance=hello",
                "intent=DailyDeskIntent",
                "time_start=2026-05-03T20%3A00%3A00Z",
                "time_end=2026-05-03T21%3A00%3A00Z",
                "weekday=6",
                "hour=0",
            ] {
                assert!(request.contains(fragment), "missing {fragment}: {request}");
            }
            let body = r#"{"meta":{"scope":"tenant"},"totals":{"utterances":7}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write response");
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let overview = control
            .get_analytics_overview(AnalyticsOverviewOptions {
                range: Some("30d".to_string()),
                bucket: Some("1d".to_string()),
                owner_id: Some("owner-1".to_string()),
                hub_id: Some("hub-1".to_string()),
                client_id: Some("client-1".to_string()),
                country: Some("CA".to_string()),
                message: Some("speak".to_string()),
                utterance: Some("hello".to_string()),
                intent: Some("DailyDeskIntent".to_string()),
                time_start: Some("2026-05-03T20:00:00Z".to_string()),
                time_end: Some("2026-05-03T21:00:00Z".to_string()),
                weekday: Some(6),
                hour: Some(0),
            })
            .await
            .expect("analytics overview");

        assert_eq!(overview["meta"]["scope"].as_str(), Some("tenant"));
        assert_eq!(overview["totals"]["utterances"].as_i64(), Some(7));
        server.join().expect("test server finished");
    }

    const HUB_BODY: &str = r#"{"id":"hub-1","name":"joke-garden","slug":"joke-garden","runtime_group_id":"rg-1","active":true,"etag":"etag-2"}"#;
    const RUNTIME_GROUP_BODY: &str =
        r#"{"id":"rg-1","name":"kiosks","description":"Lobby kiosks","status":"ready"}"#;
    const RUNTIME_GROUP_CONFIG_BODY: &str = r#"{"runtime_group_id":"rg-1","config":{"lang":"en-us"},"personas":{"default":"assistant"}}"#;
    const DESIRED_SKILL_BODY: &str = r#"{"id":"desired-1","runtime_group_id":"rg-1","skill_id":"skill-weather","source_type":"catalog","active":true}"#;

    /// Serve `count` requests, answering each from `route` and recording the
    /// raw request text so assertions can run after the server has stopped.
    fn spawn_recording_server(
        listener: TcpListener,
        count: usize,
        route: impl Fn(&str) -> (&'static str, &'static str) + Send + 'static,
    ) -> (
        thread::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handle = thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 16384];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]).to_string();
                let (status, body) = route(&request);
                write_json_response(&mut stream, status, body);
                recorded.lock().expect("record request").push(request);
            }
        });
        (handle, requests)
    }

    fn recorded(requests: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, index: usize) -> String {
        requests.lock().expect("read requests")[index].clone()
    }

    #[tokio::test]
    async fn hub_provisioning_sends_paths_bodies_and_conditional_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (server, requests) = spawn_recording_server(listener, 5, |request| {
            if request.starts_with("POST /v1/hubs ") {
                ("201 Created", HUB_BODY)
            } else if request.starts_with("PATCH /v1/hubs/hub-1 ") {
                ("200 OK", HUB_BODY)
            } else if request.starts_with("DELETE /v1/hubs/hub-1 ") {
                ("204 No Content", "")
            } else if request.starts_with("POST /v1/hubs/hub-1/release ") {
                ("200 OK", HUB_BODY)
            } else {
                panic!("unexpected request: {request}");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let created = control
            .create_hub(json!({"name": "joke-garden", "spec": {}}), None)
            .await
            .expect("create hub");
        control
            .create_hub(
                json!({"name": "joke-garden", "spec": {}}),
                Some("caller-key-1".to_string()),
            )
            .await
            .expect("create hub with caller key");
        let updated = control
            .update_hub("hub-1", json!({"active": false}), "etag-1")
            .await
            .expect("update hub");
        control
            .delete_hub("hub-1", "etag-2")
            .await
            .expect("delete hub");
        let released = control
            .release_hub(
                "hub-1",
                ReleaseOptions {
                    channel: Some("stable".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("release hub");
        server.join().expect("test server finished");

        assert_eq!(created["id"].as_str(), Some("hub-1"));
        assert_eq!(updated["id"].as_str(), Some("hub-1"));
        assert_eq!(released["id"].as_str(), Some("hub-1"));

        // create_hub generates an Idempotency-Key when the caller omits one.
        let generated = recorded(&requests, 0);
        let lowered = generated.to_ascii_lowercase();
        let key = lowered
            .split("\r\nidempotency-key: ")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .unwrap_or_default()
            .to_string();
        assert_eq!(key.len(), 36, "expected a generated UUID key: {generated}");
        assert!(
            generated.contains(r#""name":"joke-garden""#),
            "missing create body: {generated}"
        );
        assert!(
            !lowered.contains("\r\nif-match:"),
            "hub create must not send If-Match: {generated}"
        );

        // A caller-supplied Idempotency-Key is sent verbatim.
        assert!(
            recorded(&requests, 1)
                .to_ascii_lowercase()
                .contains("\r\nidempotency-key: caller-key-1\r\n"),
            "missing caller key: {}",
            recorded(&requests, 1)
        );

        // PATCH and DELETE both carry the required If-Match precondition.
        let update = recorded(&requests, 2);
        assert!(
            update
                .to_ascii_lowercase()
                .contains("\r\nif-match: etag-1\r\n"),
            "hub update must send If-Match: {update}"
        );
        assert!(
            update.contains(r#""active":false"#),
            "missing update body: {update}"
        );
        let delete = recorded(&requests, 3);
        assert!(
            delete
                .to_ascii_lowercase()
                .contains("\r\nif-match: etag-2\r\n"),
            "hub delete must send If-Match: {delete}"
        );

        // The release body carries only the options the caller set.
        let release = recorded(&requests, 4);
        assert!(
            release.ends_with(r#"{"channel":"stable"}"#),
            "release must omit unset options: {release}"
        );
    }

    #[tokio::test]
    async fn hub_rating_and_runtime_capabilities_use_their_own_verbs() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (server, requests) = spawn_recording_server(listener, 3, |request| {
            if request.starts_with("PUT /v1/hubs/hub-1/rating ")
                || request.starts_with("DELETE /v1/hubs/hub-1/rating ")
            {
                ("200 OK", HUB_BODY)
            } else if request.starts_with("GET /v1/hubs/hub-1/runtime-capabilities ") {
                (
                    "200 OK",
                    r#"{"hub_id":"hub-1","source":"ovos-runtime","skills":[],"intents":[],"counts":{"skills":0,"total_intents":3}}"#,
                )
            } else {
                panic!("unexpected request: {request}");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let rated = control
            .set_hub_rating("hub-1", 5)
            .await
            .expect("set rating");
        let cleared = control
            .clear_hub_rating("hub-1")
            .await
            .expect("clear rating");
        let capabilities = control
            .get_hub_runtime_capabilities("hub-1")
            .await
            .expect("runtime capabilities");
        server.join().expect("test server finished");

        assert_eq!(rated["id"].as_str(), Some("hub-1"));
        assert_eq!(cleared["id"].as_str(), Some("hub-1"));
        assert_eq!(capabilities["counts"]["total_intents"].as_i64(), Some(3));
        assert!(
            recorded(&requests, 0).ends_with(r#"{"rating":5}"#),
            "missing rating body: {}",
            recorded(&requests, 0)
        );
        // Rating is not an optimistic-locking route.
        assert!(
            !recorded(&requests, 1)
                .to_ascii_lowercase()
                .contains("\r\nif-match:"),
            "clear rating must not send If-Match"
        );
    }

    #[tokio::test]
    async fn runtime_group_crud_sends_paths_params_and_bodies() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (server, requests) = spawn_recording_server(listener, 10, |request| {
            if request.starts_with("GET /v1/runtime-groups ")
                || request.starts_with("GET /v1/runtime-groups?")
            {
                ("200 OK", r#"{"data":[{"id":"rg-1","name":"kiosks"}]}"#)
            } else if request.starts_with("GET /v1/runtime-groups/rg-1/config ")
                || request.starts_with("PATCH /v1/runtime-groups/rg-1/config ")
            {
                // Both config routes answer with the same document; the test
                // asserts the verb and body from the recorded request instead.
                ("200 OK", RUNTIME_GROUP_CONFIG_BODY)
            } else if request.starts_with("POST /v1/runtime-groups/rg-1/release ") {
                ("200 OK", RUNTIME_GROUP_BODY)
            } else if request.starts_with("DELETE /v1/runtime-groups/rg-1 ") {
                ("204 No Content", "")
            } else if request.starts_with("GET /v1/runtime-groups/rg-1 ")
                || request.starts_with("PATCH /v1/runtime-groups/rg-1 ")
            {
                ("200 OK", RUNTIME_GROUP_BODY)
            } else if request.starts_with("POST /v1/runtime-groups ") {
                ("201 Created", RUNTIME_GROUP_BODY)
            } else {
                panic!("unexpected request: {request}");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let page = control
            .list_runtime_groups(None)
            .await
            .expect("list runtime groups");
        control
            .list_runtime_groups(Some("owner-1"))
            .await
            .expect("list runtime groups for owner");
        let fetched = control
            .get_runtime_group("rg-1")
            .await
            .expect("get runtime group");
        let created = control
            .create_runtime_group(json!({"name": "kiosks", "clone_from_default": true}))
            .await
            .expect("create runtime group");
        let updated = control
            .update_runtime_group(
                "rg-1",
                json!({"name": "kiosks-eu", "spec": {"replicas": 2}}),
            )
            .await
            .expect("update runtime group");
        let config = control
            .get_runtime_group_config("rg-1")
            .await
            .expect("get config");
        control
            .update_runtime_group_config(
                "rg-1",
                json!({"lang": "en-us"}),
                Some(json!({"default": "assistant"})),
            )
            .await
            .expect("update config with personas");
        control
            .update_runtime_group_config("rg-1", json!({"lang": "fr-fr"}), None)
            .await
            .expect("update config without personas");
        control
            .release_runtime_group(
                "rg-1",
                ReleaseOptions {
                    channel: Some("beta".to_string()),
                    mode: Some("custom".to_string()),
                    version: Some("1.2.3".to_string()),
                    images: Some(BTreeMap::from([(
                        "core".to_string(),
                        "ghcr.io/thalovant/core:1".to_string(),
                    )])),
                    reason: Some("pin the core image".to_string()),
                },
            )
            .await
            .expect("release runtime group");
        control
            .delete_runtime_group("rg-1")
            .await
            .expect("delete runtime group");
        server.join().expect("test server finished");

        assert_eq!(page["data"][0]["id"].as_str(), Some("rg-1"));
        assert_eq!(fetched["id"].as_str(), Some("rg-1"));
        assert_eq!(created["name"].as_str(), Some("kiosks"));
        assert_eq!(updated["id"].as_str(), Some("rg-1"));
        assert_eq!(config["config"]["lang"].as_str(), Some("en-us"));

        // owner_id is omitted entirely when unset.
        assert!(
            recorded(&requests, 0).starts_with("GET /v1/runtime-groups "),
            "unfiltered list must not send a query string: {}",
            recorded(&requests, 0)
        );
        assert!(
            recorded(&requests, 1).starts_with("GET /v1/runtime-groups?owner_id=owner-1 "),
            "missing owner filter: {}",
            recorded(&requests, 1)
        );

        // Runtime-group writes carry neither If-Match nor an idempotency key.
        for index in [3, 4, 6, 8, 9] {
            let request = recorded(&requests, index).to_ascii_lowercase();
            assert!(
                !request.contains("\r\nif-match:") && !request.contains("\r\nidempotency-key:"),
                "runtime-group writes must send no precondition headers: {request}"
            );
        }

        // personas is sent only when provided; config is always wrapped.
        let with_personas = recorded(&requests, 6);
        assert!(
            with_personas
                .ends_with(r#"{"config":{"lang":"en-us"},"personas":{"default":"assistant"}}"#),
            "unexpected config body: {with_personas}"
        );
        let without_personas = recorded(&requests, 7);
        assert!(
            without_personas.ends_with(r#"{"config":{"lang":"fr-fr"}}"#),
            "personas must be omitted when unset: {without_personas}"
        );

        let release = recorded(&requests, 8);
        for fragment in [
            r#""channel":"beta""#,
            r#""mode":"custom""#,
            r#""version":"1.2.3""#,
            r#""images":{"core":"ghcr.io/thalovant/core:1"}"#,
            r#""reason":"pin the core image""#,
        ] {
            assert!(release.contains(fragment), "missing {fragment}: {release}");
        }
    }

    #[tokio::test]
    async fn runtime_group_skill_install_maps_options_to_the_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (server, requests) = spawn_recording_server(listener, 3, |request| {
            if request.starts_with("POST /v1/runtime-groups/rg-1/skills ") {
                ("200 OK", DESIRED_SKILL_BODY)
            } else if request.starts_with("DELETE /v1/runtime-groups/rg-1/skills/skill-weather ") {
                ("204 No Content", "")
            } else {
                panic!("unexpected request: {request}");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let installed = control
            .install_runtime_group_skill("rg-1", "skill-weather", SkillInstallOptions::default())
            .await
            .expect("install skill");
        control
            .install_runtime_group_skill(
                "rg-1",
                "skill-weather",
                SkillInstallOptions {
                    marketplace_skill_id: Some("marketplace-1".to_string()),
                    source_type: "git".to_string(),
                    source_ref: Some("https://github.com/example/skill".to_string()),
                    version_pin: Some("1.4.0".to_string()),
                    active: false,
                },
            )
            .await
            .expect("install git skill");
        control
            .uninstall_runtime_group_skill("rg-1", "skill-weather")
            .await
            .expect("uninstall skill");
        server.join().expect("test server finished");

        assert_eq!(installed["skill_id"].as_str(), Some("skill-weather"));

        // The default install is an active catalog install with no extra keys.
        let default_install = recorded(&requests, 0);
        assert!(
            default_install
                .ends_with(r#"{"active":true,"skill_id":"skill-weather","source_type":"catalog"}"#),
            "unexpected default install body: {default_install}"
        );

        let git_install = recorded(&requests, 1);
        for fragment in [
            r#""skill_id":"skill-weather""#,
            r#""source_type":"git""#,
            r#""active":false"#,
            r#""marketplace_skill_id":"marketplace-1""#,
            r#""source_ref":"https://github.com/example/skill""#,
            r#""version_pin":"1.4.0""#,
        ] {
            assert!(
                git_install.contains(fragment),
                "missing {fragment}: {git_install}"
            );
        }
    }

    #[tokio::test]
    async fn skill_discovery_reads_send_optional_params_only_when_set() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (server, requests) = spawn_recording_server(listener, 7, |request| {
            if request.starts_with("GET /v1/marketplace/skills") {
                (
                    "200 OK",
                    r#"{"data":[{"skill_id":"skill-weather","access_tier":"free","source_type":"catalog"}]}"#,
                )
            } else if request.starts_with("GET /v1/runtime-groups/rg-1/marketplace") {
                (
                    "200 OK",
                    r#"{"runtime_group_id":"rg-1","source":"runtime-group-cache","operator_phase":"Ready","data":[{"skill_id":"skill-weather","installable":true,"active":false}]}"#,
                )
            } else if request.starts_with("GET /v1/runtime-groups/rg-1/inventory") {
                (
                    "200 OK",
                    r#"{"runtime_group_id":"rg-1","source":"ovos-runtime-operator-pending","operator_phase":null,"data":[]}"#,
                )
            } else {
                panic!("unexpected request: {request}");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let catalog = control
            .list_marketplace_skills(MarketplaceSkillsOptions::default())
            .await
            .expect("list marketplace skills");
        control
            .list_marketplace_skills(MarketplaceSkillsOptions {
                owner_id: Some("owner-1".to_string()),
                include_inactive: true,
                force_refresh: true,
            })
            .await
            .expect("list marketplace skills with params");
        control
            .list_marketplace_skills(MarketplaceSkillsOptions {
                owner_id: Some("   ".to_string()),
                ..Default::default()
            })
            .await
            .expect("list marketplace skills with a blank owner");
        let view = control
            .list_runtime_group_marketplace("rg-1", false)
            .await
            .expect("group marketplace");
        control
            .list_runtime_group_marketplace("rg-1", true)
            .await
            .expect("group marketplace refreshed");
        let inventory = control
            .list_runtime_group_inventory("rg-1", false)
            .await
            .expect("group inventory");
        control
            .list_runtime_group_inventory("rg-1", true)
            .await
            .expect("group inventory refreshed");
        server.join().expect("test server finished");

        assert_eq!(
            catalog["data"][0]["skill_id"].as_str(),
            Some("skill-weather")
        );
        assert_eq!(view["data"][0]["installable"], Value::Bool(true));
        // Nothing reporting yet is an empty list with a pending source, not an error.
        assert_eq!(
            inventory["source"].as_str(),
            Some("ovos-runtime-operator-pending")
        );
        assert_eq!(inventory["data"].as_array().map(Vec::len), Some(0));

        assert!(
            recorded(&requests, 0).starts_with("GET /v1/marketplace/skills "),
            "defaults must send no query string: {}",
            recorded(&requests, 0)
        );
        let all_params = recorded(&requests, 1);
        for fragment in [
            "owner_id=owner-1",
            "include_inactive=true",
            "force_refresh=true",
        ] {
            assert!(
                all_params.contains(fragment),
                "missing {fragment}: {all_params}"
            );
        }
        assert!(
            recorded(&requests, 2).starts_with("GET /v1/marketplace/skills "),
            "a blank owner_id must be dropped: {}",
            recorded(&requests, 2)
        );
        assert!(
            recorded(&requests, 3).starts_with("GET /v1/runtime-groups/rg-1/marketplace "),
            "unexpected request: {}",
            recorded(&requests, 3)
        );
        assert!(
            recorded(&requests, 4)
                .starts_with("GET /v1/runtime-groups/rg-1/marketplace?refresh_inventory=true "),
            "unexpected request: {}",
            recorded(&requests, 4)
        );
        assert!(
            recorded(&requests, 5).starts_with("GET /v1/runtime-groups/rg-1/inventory "),
            "unexpected request: {}",
            recorded(&requests, 5)
        );
        assert!(
            recorded(&requests, 6)
                .starts_with("GET /v1/runtime-groups/rg-1/inventory?refresh=true "),
            "unexpected request: {}",
            recorded(&requests, 6)
        );
    }

    #[tokio::test]
    async fn provisioning_errors_surface_status_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let (server, _requests) = spawn_recording_server(listener, 4, |request| {
            if request.starts_with("POST /v1/hubs ") {
                (
                    "402 Payment Required",
                    r#"{"detail":"API access requires a paid plan."}"#,
                )
            } else if request.starts_with("POST /v1/runtime-groups/rg-1/skills ") {
                ("403 Forbidden", r#"{"detail":"Insufficient scopes"}"#)
            } else if request.starts_with("PATCH /v1/hubs/hub-1 ") {
                ("412 Precondition Failed", r#"{"detail":"ETag mismatch"}"#)
            } else if request.starts_with("GET /v1/hubs/hub-1/runtime-capabilities ") {
                (
                    "409 Conflict",
                    r#"{"detail":"Live skills and intents are not available for this hub yet."}"#,
                )
            } else {
                panic!("unexpected request: {request}");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let free_plan = control
            .create_hub(json!({"name": "joke-garden", "spec": {}}), None)
            .await
            .expect_err("free-plan create must fail");
        let missing_scope = control
            .install_runtime_group_skill("rg-1", "skill-weather", SkillInstallOptions::default())
            .await
            .expect_err("scopeless install must fail");
        let stale_etag = control
            .update_hub("hub-1", json!({"active": false}), "stale")
            .await
            .expect_err("stale etag must fail");
        let not_connected = control
            .get_hub_runtime_capabilities("hub-1")
            .await
            .expect_err("capabilities without a client must fail");
        server.join().expect("test server finished");

        for (error, status, detail) in [
            (free_plan, "402", "API access requires a paid plan."),
            (missing_scope, "403", "Insufficient scopes"),
            (stale_etag, "412", "ETag mismatch"),
            (not_connected, "409", "Live skills and intents"),
        ] {
            let ThalovantError::Api(message) = &error else {
                panic!("unexpected error variant: {error:?}");
            };
            assert!(
                message.contains(status) && message.contains(detail),
                "expected HTTP {status} and {detail:?}: {message}"
            );
        }
    }

    fn bootstrap_result_with_secrets() -> BootstrapIdentityResult {
        let identity = Identity::from_value(json!({
            "access_key": "ak-LIVE",
            "password": "pw-LIVE",
            "site_id": "site",
            "default_master": "https://hub.example.com"
        }))
        .unwrap();
        BootstrapIdentityResult {
            identity,
            hub: json!({"id": "hub-1", "spec": {"broker_password": "hub-LIVE-SECRET"}}),
            client: json!({
                "id": "client-1",
                "spec": {"apiKey": "api-LIVE", "password": "cpw-LIVE", "cryptoKey": "ck-LIVE"}
            }),
            endpoint: None,
        }
    }

    #[test]
    fn bootstrap_summary_redacts_hub_and_client_secrets_unless_requested() {
        let result = bootstrap_result_with_secrets();

        // Non-secret view keeps structure but redacts every credential subkey.
        let public = result.as_value(false);
        assert_eq!(public["client"]["id"], "client-1");
        assert_eq!(public["client"]["spec"]["apiKey"], "<redacted>");
        assert_eq!(public["client"]["spec"]["password"], "<redacted>");
        assert_eq!(public["client"]["spec"]["cryptoKey"], "<redacted>");
        assert_eq!(public["hub"]["spec"]["broker_password"], "<redacted>");

        // Explicit secret view keeps the real values (persistence path).
        let full = result.as_value(true);
        assert_eq!(full["client"]["spec"]["apiKey"], "api-LIVE");
        assert_eq!(full["client"]["spec"]["password"], "cpw-LIVE");
        assert_eq!(full["client"]["spec"]["cryptoKey"], "ck-LIVE");
        assert_eq!(full["hub"]["spec"]["broker_password"], "hub-LIVE-SECRET");
    }

    #[test]
    fn bootstrap_result_debug_never_leaks_client_credentials() {
        let debug = format!("{:?}", bootstrap_result_with_secrets());
        for secret in [
            "ak-LIVE",
            "pw-LIVE",
            "api-LIVE",
            "cpw-LIVE",
            "ck-LIVE",
            "hub-LIVE-SECRET",
        ] {
            assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn login_options_debug_redacts_mfa_codes() {
        let opts = LoginOptions {
            scope: Some("hubs:read".to_string()),
            otp_code: Some("123456".to_string()),
            recovery_code: Some("recovery-LIVE-SECRET".to_string()),
        };
        let debug = format!("{opts:?}");
        assert!(debug.contains("hubs:read"));
        assert!(!debug.contains("123456"), "Debug leaked otp: {debug}");
        assert!(
            !debug.contains("recovery-LIVE-SECRET"),
            "Debug leaked recovery: {debug}"
        );
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn device_authorization_debug_redacts_device_code_and_raw() {
        let auth = DeviceAuthorization::from_value(json!({
            "device_code": "dc-LIVE-SECRET",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://example.com/device",
            "expires_in": 900,
            "interval": 5
        }))
        .unwrap();
        let debug = format!("{auth:?}");
        assert!(!debug.contains("dc-LIVE-SECRET"), "Debug leaked code: {debug}");
        // The user code is meant to be shown to the end user.
        assert!(debug.contains("WDJB-MJHT"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn server_error_detail_redacts_secret_keys_and_bounds_length() {
        // Secret keys an error body might echo are redacted; other detail stays.
        let redacted =
            server_error_detail(r#"{"detail":"bad request","password":"pw-LIVE-SECRET"}"#);
        assert!(redacted.contains("bad request"));
        assert!(
            !redacted.contains("pw-LIVE-SECRET"),
            "detail leaked secret: {redacted}"
        );

        // Multi-line / oversized bodies are collapsed to one bounded line.
        let big = format!("line-one\nline-two\n{}", "x".repeat(500));
        let bounded = server_error_detail(&big);
        assert!(!bounded.contains('\n'), "detail kept newlines: {bounded}");
        assert!(bounded.chars().count() <= 203, "detail not bounded: {bounded}");
    }
}
