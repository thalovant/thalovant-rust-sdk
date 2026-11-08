//! Browser sign-in on the control plane: the authorization code grant with
//! PKCE (S256 only, RFC 7636) on a loopback redirect (RFC 8252 section 7.3),
//! and the chooser between it and device login.
//!
//! A device code can be read off one screen and typed in anywhere. With a
//! loopback redirect the browser that approves hands the code to its own
//! computer's `127.0.0.1`, so a link or code sent to someone on another
//! computer delivers nothing to the sender. (A forwarded callback port, or
//! another user of the same computer, is another matter.) On a machine with a
//! browser a program signs in this way; device login stays the way in for a
//! machine with none.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::{
    access_token_value, api_response_error, open_url_in_browser, token_id_value, ControlPlane,
    DeviceLoginOptions, DEFAULT_DEVICE_LOGIN_TIMEOUT,
};

/// How long each request of a browser sign-in may take, headers and body:
/// the configuration and the exchange. What the Python reference gives every
/// request. Without it a stalled API would hold the sign-in, and the tab
/// waiting on it, for good.
pub(super) const BROWSER_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// What a secret is replaced with in an error.
const REDACTED_SECRET: &str = "[redacted]";
use crate::errors::{ApiProblem, Result, ThalovantError};
use crate::loopback::{prompt_text, CallbackOutcome, LoopbackListener};
use crate::native_auth::{
    browser_authorization_url, can_open_browser, challenge_for, default_device_name,
    loopback_redirect_uri, new_state, new_verifier, require_secure_token_exchange,
    NativeSignInConfig, DEFAULT_BROWSER_LOGIN_TIMEOUT,
};

/// Presents the authorization URL instead of the default line on stdout. It
/// does not stop the browser from opening.
pub type BrowserPrompt = Box<dyn Fn(&str) + Send + Sync>;

/// Opens a URL in a browser instead of the platform default (`open` on macOS,
/// `xdg-open` elsewhere, `rundll32 url.dll,FileProtocolHandler` on Windows,
/// never through a shell). It runs on a thread of its own, so an opener that
/// blocks holds nothing up, and whatever it does with a failure changes
/// nothing: the URL was already shown.
pub type BrowserOpener = Arc<dyn Fn(&str) + Send + Sync>;

/// How [`ControlPlane::interactive_login`] signs in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LoginMethod {
    /// The browser when this machine can show one and the API offers it,
    /// device login otherwise.
    #[default]
    Auto,
    /// Browser sign-in, whatever the machine; an API that does not offer it
    /// is an error, never a device login.
    Browser,
    /// Device login; the browser sign-in configuration is never asked for.
    Device,
}

/// Options for [`ControlPlane::browser_login`].
///
/// The default asks for no scopes (the API then grants `hubs:read
/// clients:write`), names the device [`default_device_name`]
/// (`thalovant-rust-sdk on <hostname>`), prints the URL and opens the
/// browser, and waits up to [`DEFAULT_BROWSER_LOGIN_TIMEOUT`] (300 seconds).
pub struct BrowserLoginOptions {
    /// What the token carries; empty strings are dropped. Any non-admin scope
    /// may be asked for; the API refuses admin or unknown ones when the
    /// person approves.
    pub scopes: Vec<String>,
    /// What the consent page and the token show, at most 120 code points.
    /// `None` is [`default_device_name`]; an empty name sends none, and keeps
    /// the hostname off the consent page.
    pub device_name: Option<String>,
    /// Open the browser; with `false` the URL is only shown.
    pub open_browser: bool,
    /// How long to wait for the browser to come back.
    pub timeout: Duration,
    /// Presents the URL instead of the line printed on stdout.
    pub prompt: Option<BrowserPrompt>,
    /// Opens the browser instead of the platform default.
    pub opener: Option<BrowserOpener>,
}

impl Default for BrowserLoginOptions {
    fn default() -> Self {
        Self {
            scopes: Vec::new(),
            device_name: None,
            open_browser: true,
            timeout: DEFAULT_BROWSER_LOGIN_TIMEOUT,
            prompt: None,
            opener: None,
        }
    }
}

impl fmt::Debug for BrowserLoginOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserLoginOptions")
            .field("scopes", &self.scopes)
            .field("device_name", &self.device_name)
            .field("open_browser", &self.open_browser)
            .field("timeout", &self.timeout)
            .field("prompt", &self.prompt.as_ref().map(|_| "<callback>"))
            .field("opener", &self.opener.as_ref().map(|_| "<callback>"))
            .finish()
    }
}

/// Options for [`ControlPlane::interactive_login`].
///
/// The default is [`LoginMethod::Auto`], no scopes, the
/// [`default_device_name`], the browser opened, and each method's own timeout.
pub struct InteractiveLoginOptions {
    /// Which way to sign in.
    pub method: LoginMethod,
    /// What the token carries, for either method.
    pub scopes: Vec<String>,
    /// Names this machine on the consent page: a browser sign-in's
    /// `device_name`, a device login's `client_name`. `None` is
    /// [`default_device_name`]; an empty name sends neither.
    pub device_name: Option<String>,
    /// Open the browser (the consent page, or the device login's
    /// verification page).
    pub open_browser: bool,
    /// `None` is each method's own: 300 seconds for the browser, 900 for a
    /// device login.
    pub timeout: Option<Duration>,
    /// Opens the browser instead of the platform default, for either method.
    pub opener: Option<BrowserOpener>,
    /// The variables [`can_open_browser`] reads (`CI`, `SSH_CONNECTION`,
    /// `SSH_TTY`, `DISPLAY`, `WAYLAND_DISPLAY`) instead of this process's:
    /// for tests, and for a host that knows better than its environment.
    pub environment: Option<HashMap<String, String>>,
    /// The platform [`can_open_browser`] decides for, named as
    /// `std::env::consts::OS` names it, instead of this machine's.
    pub platform: Option<String>,
}

impl Default for InteractiveLoginOptions {
    fn default() -> Self {
        Self {
            method: LoginMethod::Auto,
            scopes: Vec::new(),
            device_name: None,
            open_browser: true,
            timeout: None,
            opener: None,
            environment: None,
            platform: None,
        }
    }
}

impl fmt::Debug for InteractiveLoginOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InteractiveLoginOptions")
            .field("method", &self.method)
            .field("scopes", &self.scopes)
            .field("device_name", &self.device_name)
            .field("open_browser", &self.open_browser)
            .field("timeout", &self.timeout)
            .field("opener", &self.opener.as_ref().map(|_| "<callback>"))
            .field("environment", &self.environment)
            .field("platform", &self.platform)
            .finish()
    }
}

impl ControlPlane {
    /// Sign in through the browser and store the API token.
    ///
    /// The authorization code grant with PKCE on a loopback redirect (RFC
    /// 8252): the SDK reads `GET /v1/auth/native/config`, listens on
    /// `127.0.0.1` on a port the system picks, opens the browser at the
    /// dashboard's consent page, and waits for the browser to come back to
    /// `http://127.0.0.1:<port>/callback`. The browser that approves hands
    /// the code to its own computer, so a sign-in link sent to somebody
    /// on another computer delivers nothing to the sender -- which a device
    /// code, read off one screen and typed in anywhere, cannot promise.
    ///
    /// The URL is always shown as well as opened (`Opening your browser to
    /// sign in to Thalovant. If it does not open, visit:` and the URL on
    /// stdout); set `options.prompt` to present it yourself, and
    /// `options.open_browser = false` to only show it. Only `GET /callback`
    /// is answered; a tab that comes back with another sign-in's `state` is
    /// turned away (400) and the wait goes on. The tab that brought the
    /// answer is told how the sign-in ended once it has.
    ///
    /// Returns the token answer and keeps its `access_token` and `token_id`
    /// exactly as [`ControlPlane::login_with_browser`] does, so
    /// [`ControlPlane::revoke_api_token`] revokes it the same way.
    ///
    /// ```no_run
    /// # async fn run() -> thalovant::Result<()> {
    /// use thalovant::{BrowserLoginOptions, ControlPlane};
    ///
    /// let mut control = ControlPlane::default();
    /// control.browser_login(BrowserLoginOptions::default()).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Errors:
    ///
    /// - [`ThalovantError::DeviceAuthorizationDenied`]: the person denied it
    ///   in the browser;
    /// - [`ThalovantError::DeviceAuthorizationExpired`]: no answer within
    ///   `options.timeout`;
    /// - [`ThalovantError::BrowserLoginFailed`]: the browser came back with
    ///   another OAuth error ([`ThalovantError::api_code`] names it) or
    ///   without a code;
    /// - [`ThalovantError::ApiResponse`] for a code the API will not exchange
    ///   (HTTP 400, [`ThalovantError::api_detail`] `invalid_grant`), and for
    ///   an API that does not offer browser sign-in (its configuration
    ///   answers 404; [`ControlPlane::interactive_login`] uses a device login
    ///   then);
    /// - [`ThalovantError::Api`], with no status, for a configuration or a
    ///   token answer the SDK cannot use, and for an API URL that is neither
    ///   https nor loopback, refused before anything is asked or opened.
    ///
    /// The code, the verifier, the state and the token never appear in an
    /// error. Dropping the future cancels the sign-in and closes the port.
    pub async fn browser_login(&mut self, options: BrowserLoginOptions) -> Result<Value> {
        // Before anything is asked or opened: a code and its verifier never
        // cross the network in cleartext.
        require_secure_token_exchange(&self.api_url)?;
        let config = self.native_sign_in_config().await?;
        self.sign_in_through_browser(&config, options).await
    }

    /// Sign a person in the best way this machine allows, and store the API
    /// token.
    ///
    /// [`LoginMethod::Auto`] signs in through the browser
    /// ([`ControlPlane::browser_login`]) when this machine can show one
    /// ([`can_open_browser`]: never over SSH or in CI; on Linux only with a
    /// display) and the API answers `GET /v1/auth/native/config` with a
    /// configuration it can use; otherwise it uses a device login
    /// ([`ControlPlane::login_with_browser`]), whose code can be approved
    /// from any browser. A 404, another failure, an unusable answer and a
    /// cleartext API URL all mean device login: nothing has been opened yet,
    /// so that is not a downgrade. [`LoginMethod::Browser`] and
    /// [`LoginMethod::Device`] force one. Once the browser has been sent to
    /// the consent page, the browser sign-in's own outcome stands -- denied,
    /// expired or a refused exchange -- and it never turns into a device
    /// login.
    ///
    /// Returns the token answer, as both methods do, and keeps the token as
    /// both do. The errors are each method's own.
    pub async fn interactive_login(&mut self, options: InteractiveLoginOptions) -> Result<Value> {
        let InteractiveLoginOptions {
            method,
            scopes,
            device_name,
            open_browser,
            timeout,
            opener,
            environment,
            platform,
        } = options;
        // Only None asks for the default: an empty name is the caller's way
        // of sending none, and keeps the hostname out.
        let name = device_name.unwrap_or_else(default_device_name);
        let config = match method {
            LoginMethod::Device => None,
            LoginMethod::Browser => {
                require_secure_token_exchange(&self.api_url)?;
                Some(self.native_sign_in_config().await?)
            }
            LoginMethod::Auto => {
                if can_open_browser(environment.as_ref(), platform.as_deref())
                    && require_secure_token_exchange(&self.api_url).is_ok()
                {
                    // An older API (404), or one that cannot say: the browser
                    // has not been opened, so a device login is not a downgrade.
                    self.native_sign_in_config().await.ok()
                } else {
                    None
                }
            }
        };
        match config {
            Some(config) => {
                let options = BrowserLoginOptions {
                    scopes,
                    device_name: Some(name),
                    open_browser,
                    timeout: timeout.unwrap_or(DEFAULT_BROWSER_LOGIN_TIMEOUT),
                    prompt: None,
                    opener,
                };
                self.sign_in_through_browser(&config, options).await
            }
            None => {
                let options = DeviceLoginOptions {
                    scopes,
                    client_name: Some(name),
                    open_browser,
                    timeout: timeout.unwrap_or(DEFAULT_DEVICE_LOGIN_TIMEOUT),
                    prompt: None,
                };
                self.device_login(options, None, opener.as_ref()).await
            }
        }
    }

    /// `GET /v1/auth/native/config`: where to send the browser, and where to
    /// exchange the code.
    async fn native_sign_in_config(&self) -> Result<NativeSignInConfig> {
        let (status, body) = self
            .bounded_request("GET", "/v1/auth/native/config", None)
            .await?;
        if status == reqwest::StatusCode::NOT_FOUND {
            let problem = match serde_json::from_str::<Value>(&body) {
                Ok(Value::Object(map)) => Some(Box::new(ApiProblem::from(map))),
                _ => None,
            };
            return Err(ThalovantError::ApiResponse {
                status_code: status.as_u16(),
                detail: "this Thalovant API does not offer browser sign-in; sign in with a device login instead".into(),
                problem,
            });
        }
        if !status.is_success() {
            return Err(api_response_error(status, &body));
        }
        // A 2xx the SDK cannot use is an error with no status, as a token
        // answer's is: the API did not refuse.
        let payload = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
        NativeSignInConfig::from_value(&payload).map_err(|error| match error {
            ThalovantError::Api(reason) => ThalovantError::Api(format!(
                "Thalovant API answered an unusable browser sign-in configuration: {reason}"
            )),
            other => other,
        })
    }

    /// One browser sign-in against `config`: listen, show and open, wait,
    /// exchange, answer the tab.
    async fn sign_in_through_browser(
        &mut self,
        config: &NativeSignInConfig,
        options: BrowserLoginOptions,
    ) -> Result<Value> {
        require_secure_token_exchange(&self.api_url)?;
        let verifier = new_verifier();
        let state = new_state();
        // Bound before the URL is built: the redirect names the port.
        let mut listener = LoopbackListener::bind(state.clone()).await?;
        let signed_in = self
            .wait_and_exchange(&mut listener, config, options, &verifier, &state)
            .await;
        if signed_in.is_ok() {
            listener.finish(true);
        }
        listener.close().await;
        signed_in
    }

    async fn wait_and_exchange(
        &mut self,
        listener: &mut LoopbackListener,
        config: &NativeSignInConfig,
        options: BrowserLoginOptions,
        verifier: &str,
        state: &str,
    ) -> Result<Value> {
        let BrowserLoginOptions {
            scopes,
            device_name,
            open_browser,
            timeout,
            prompt,
            opener,
        } = options;
        let redirect_uri = loopback_redirect_uri(u32::from(listener.port()))?;
        let name = device_name.unwrap_or_else(default_device_name);
        let url = browser_authorization_url(
            &config.authorization_endpoint,
            &config.sdk_client_id,
            &redirect_uri,
            &challenge_for(verifier),
            state,
            &scopes,
            Some(&name),
        );
        match prompt.as_ref() {
            Some(prompt) => prompt(&url),
            None => println!("{}", prompt_text(&url, open_browser)),
        }
        if open_browser {
            open_in_browser(&url, opener.as_ref());
        }
        let Some(callback) = listener.wait(timeout).await else {
            return Err(ThalovantError::DeviceAuthorizationExpired);
        };
        let code = match (callback.outcome, callback.code) {
            (CallbackOutcome::Denied, _) => return Err(ThalovantError::DeviceAuthorizationDenied),
            (CallbackOutcome::Code, Some(code)) => code,
            _ => {
                // One of RFC 6749's codes or nothing (classify_request): a
                // value the page chose could be the state or a code.
                return Err(ThalovantError::BrowserLoginFailed {
                    code: callback.error,
                });
            }
        };
        // A code is single-use: retrying the exchange would only revoke the
        // token a first exchange minted (RFC 9700), so a failure is final.
        let hidden = [code.as_str(), verifier, state];
        self.exchange_browser_code(config, &code, verifier, &redirect_uri, &hidden)
            .await
            .map_err(|error| without_secrets(error, &hidden))
    }

    /// `POST {token_endpoint}` with exactly the four fields, and keep the
    /// token.
    ///
    /// Read as a device login's token answer is: a 2xx whose body is not a
    /// JSON object, or has no `access_token`, is an error with no status --
    /// the API did not refuse, its answer could not be used -- and any other
    /// failure carries the api-errors fields (`invalid_grant` for every way
    /// an exchange fails). A refusal has every value in `hidden` taken out of
    /// its body before the error is made of it, so a message cut to length
    /// cannot keep part of one.
    async fn exchange_browser_code(
        &mut self,
        config: &NativeSignInConfig,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        hidden: &[&str],
    ) -> Result<Value> {
        require_secure_token_exchange(&self.api_url)?;
        let body = json!({
            "code": code,
            "code_verifier": verifier,
            "client_id": config.sdk_client_id,
            "redirect_uri": redirect_uri,
        });
        // The configuration refuses a token endpoint that could leave the
        // API, and send() refuses any request off the API's origin: the code
        // and the verifier go nowhere else.
        let (status, text) = self
            .bounded_request("POST", &config.token_endpoint, Some(body))
            .await?;
        if !status.is_success() {
            return Err(api_response_error(status, &redacted_body(&text, hidden)));
        }
        let token = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| {
                ThalovantError::Api("Thalovant API returned an unexpected response shape".into())
            })?;
        let access_token = access_token_value(&token).ok_or_else(|| {
            ThalovantError::Api("token response did not include a usable access_token".into())
        })?;
        self.accept_token(access_token, token_id_value(&token));
        Ok(token)
    }

    /// One request of a browser sign-in, bounded whole -- headers and body --
    /// by [`BROWSER_REQUEST_TIMEOUT`]. Running out of time is the API out of
    /// reach, with no status, as a request that never got an answer is.
    async fn bounded_request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(reqwest::StatusCode, String)> {
        let request = self.send_request(method, path, body, None, false);
        match tokio::time::timeout(self.browser_request_timeout, request).await {
            Ok(answer) => answer,
            Err(_) => Err(ThalovantError::api_unreachable("the request timed out")),
        }
    }
}

/// Open `url` with the caller's opener, on a thread of its own, or with the
/// platform default.
pub(super) fn open_in_browser(url: &str, opener: Option<&BrowserOpener>) {
    match opener {
        Some(opener) => {
            let opener = Arc::clone(opener);
            let url = url.to_string();
            let _ = std::thread::Builder::new()
                .name("thalovant-browser".into())
                .spawn(move || opener(&url));
        }
        None => open_url_in_browser(url),
    }
}

/// `error`, rebuilt with every value in `hidden` taken out of its message
/// and its problem, keys included.
///
/// An API that echoes a request back (a 422 lists each field's `input`)
/// would otherwise hand the code and the verifier to whoever logs the error.
/// Always a new error, carrying nothing of the old one but its scrubbed text
/// and status: none of these variants keeps a cause.
fn without_secrets(error: ThalovantError, hidden: &[&str]) -> ThalovantError {
    match error {
        ThalovantError::ApiResponse {
            status_code,
            detail,
            problem,
        } => ThalovantError::ApiResponse {
            status_code,
            detail: scrub_text(&detail, hidden),
            problem: problem
                .map(|problem| Box::new(ApiProblem::from(scrub_map(problem.into_map(), hidden)))),
        },
        ThalovantError::Api(message) => ThalovantError::Api(scrub_text(&message, hidden)),
        other => other,
    }
}

/// A refusal's body with `hidden` taken out, before any message is made of
/// it: as JSON when it parses (an escaped value is still the value), else as
/// text.
fn redacted_body(text: &str, hidden: &[&str]) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(parsed) => scrub_value(parsed, hidden).to_string(),
        Err(_) => scrub_text(text, hidden),
    }
}

/// `text` with every value in `hidden` replaced by `[redacted]`.
///
/// One pass, the longest value first, so that a value which is part of
/// another cannot break the longer one up before it is matched; and never
/// inside a `[redacted]` already there, so scrubbing twice changes nothing.
fn scrub_text(text: &str, hidden: &[&str]) -> String {
    let mut secrets: Vec<&str> = hidden
        .iter()
        .copied()
        .filter(|value| !value.is_empty())
        .collect();
    if secrets.is_empty() {
        return text.to_string();
    }
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    text.split(REDACTED_SECRET)
        .map(|part| {
            let mut scrubbed = String::with_capacity(part.len());
            let mut rest = part;
            while let Some(next) = rest.chars().next() {
                match secrets.iter().find(|secret| rest.starts_with(**secret)) {
                    Some(secret) => {
                        scrubbed.push_str(REDACTED_SECRET);
                        rest = &rest[secret.len()..];
                    }
                    None => {
                        scrubbed.push(next);
                        rest = &rest[next.len_utf8()..];
                    }
                }
            }
            scrubbed
        })
        .collect::<Vec<_>>()
        .join(REDACTED_SECRET)
}

fn scrub_map(map: Map<String, Value>, hidden: &[&str]) -> Map<String, Value> {
    map.into_iter()
        .map(|(key, value)| (scrub_text(&key, hidden), scrub_value(value, hidden)))
        .collect()
}

fn scrub_value(value: Value, hidden: &[&str]) -> Value {
    match value {
        Value::String(text) => Value::String(scrub_text(&text, hidden)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| scrub_value(item, hidden))
                .collect(),
        ),
        Value::Object(map) => Value::Object(scrub_map(map, hidden)),
        other => other,
    }
}

#[cfg(test)]
#[path = "browser_login_tests.rs"]
mod tests;
