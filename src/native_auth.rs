//! The authorization-code grant with PKCE (RFC 7636), for a client that can
//! open a browser.
//!
//! [`ControlPlane::login_with_browser`](crate::control::ControlPlane::login_with_browser)
//! is the device grant, and it exists for something that *cannot* open one:
//! somebody reads a code off one screen and types it into another. A desktop
//! tool can open the browser itself and be handed the answer back, and asking
//! its user to copy a code between two windows on the same machine is a worse
//! experience than the one every other tool on that machine offers.
//!
//! ```no_run
//! # use thalovant::native_auth::{begin_native_sign_in, NativeSignInOptions};
//! let begun = begin_native_sign_in(NativeSignInOptions {
//!     client_id: "my-app".into(),
//!     redirect_uri: "http://127.0.0.1:8765/".into(),
//!     ..Default::default()
//! })?;
//! // open begun.authorization_url, then when the browser comes back:
//! let redirect = format!("http://127.0.0.1:8765/?code=abc&state={}", begun.state);
//! let code = begun.code_from(&redirect);   // None when it is not ours
//! # Ok::<(), thalovant::errors::ThalovantError>(())
//! ```
//!
//! `begun.verifier` never leaves the process and never enters the browser.
//! That is what PKCE is for: a code intercepted by whatever else claimed the
//! redirect is useless without it.
//!
//! The same grant signs a *program* in -- a command line, a service someone
//! runs at their desk -- through
//! [`ControlPlane::browser_login`](crate::control::ControlPlane::browser_login),
//! which listens on a loopback port for the answer (RFC 8252 section 7.3). The
//! pieces it is made of live here, so that each can be checked against the
//! shared vectors on its own: [`browser_authorization_url`] builds the address
//! the browser opens, [`loopback_redirect_uri`] the redirect it comes back to,
//! [`new_state`] the value that ties the two together, [`NativeSignInConfig`]
//! reads where the API says to send the browser, and [`can_open_browser`] says
//! whether this machine has a browser to send. The browser that approves hands
//! the code to its own computer's `127.0.0.1`, so a link or code sent to
//! someone on another computer delivers nothing to the sender. That is all it
//! promises: a forwarded callback port (`ssh -L`, a port-forwarding tool) or
//! another user of the same computer can still complete a sign-in.

use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::SysRng, TryRng};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use url::Url;

use crate::errors::{Result, ThalovantError};

/// Where a person approves the request.
pub const DEFAULT_DASHBOARD_URL: &str = "https://dash.thalovant.com";

/// The three a phone needs; also the three a free plan may mint.
pub const DEFAULT_NATIVE_SCOPES: [&str; 3] = ["hubs:read", "clients:read", "clients:write"];

/// Inputs for [`begin_native_sign_in`].
///
/// `redirect_uri` must be one the API has registered for `client_id`; the
/// authorization endpoint matches it exactly and refuses anything else, so it
/// cannot be turned into an open redirect.
#[derive(Debug, Clone, Default)]
pub struct NativeSignInOptions {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Option<Vec<String>>,
    pub dashboard_url: Option<String>,
}

/// One sign-in attempt in progress. Keep it until the browser comes back; it
/// holds the two secrets that make the round trip safe.
///
/// `Debug` is written by hand rather than derived: the derived one prints the
/// PKCE verifier, and an intercepted authorization code is redeemable by
/// anyone who also has that. A log line is enough to leak it.
#[derive(Clone)]
pub struct NativeSignIn {
    /// Open this in a browser.
    pub authorization_url: String,
    /// Proves the redirect answers *this* attempt and not a replayed one.
    pub state: String,
    /// Never send this to the browser. Exchanged with the code, once.
    pub verifier: String,
    /// What this attempt asked the callback to arrive at. One that lands
    /// anywhere else is not this attempt's, however good its state looks.
    pub redirect_uri: String,
}

impl fmt::Debug for NativeSignIn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSignIn")
            // The query string carries `state` (and the challenge, and the
            // redirect), so printing the URL whole put the state back in the
            // log that redacting the field below was meant to keep it out of.
            // The endpoint is what is worth seeing.
            .field("authorization_url", &redacted_url(&self.authorization_url))
            .field("state", &"<redacted>")
            .field("verifier", &"<redacted>")
            // Only emptiness is rejected when this is supplied, so it can
            // carry userinfo, a query or a fragment. An exact-match OAuth
            // contract does not make any of those safe to log.
            .field("redirect_uri", &redacted_url(&self.redirect_uri))
            .finish()
    }
}

/// An endpoint without the parts of a URL that carry secrets.
///
/// The query holds `state` and the PKCE challenge; a fragment holds whatever
/// an implicit flow put there; userinfo holds credentials outright. What is
/// worth seeing in a log is the endpoint.
fn redacted_url(raw: &str) -> String {
    let (before_fragment, fragment) = match raw.split_once('#') {
        Some((head, _)) => (head, "#<redacted>"),
        None => (raw, ""),
    };
    let (endpoint, query) = match before_fragment.split_once('?') {
        Some((head, _)) => (head, "?<redacted>"),
        None => (before_fragment, ""),
    };
    // `scheme://user:password@host/path` -- the authority is everything up to
    // the first `/` after `://`, and userinfo is whatever precedes its last
    // `@`. Anything without an authority has no userinfo to remove.
    // ...and the network-path form `//user:password@host/path` that RFC 3986
    // §4.2 allows: an authority is introduced by either marker. Matching only
    // `://` let the second reach Debug with its credentials intact.
    let split = endpoint
        .find("://")
        .map(|at| (&endpoint[..at + 3], &endpoint[at + 3..]))
        .or_else(|| endpoint.strip_prefix("//").map(|rest| ("//", rest)));
    let endpoint = match split {
        Some((prefix, rest)) => {
            let (authority, path) = match rest.find('/') {
                Some(cut) => (&rest[..cut], &rest[cut..]),
                None => (rest, ""),
            };
            match authority.rsplit_once('@') {
                Some((_, host)) => format!("{prefix}<redacted>@{host}{path}"),
                None => format!("{prefix}{authority}{path}"),
            }
        }
        None => endpoint.to_string(),
    };
    format!("{endpoint}{query}{fragment}")
}

fn base64_url(raw: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// `size` bytes from the operating system's random source, base64url.
fn random_url_safe(size: usize) -> String {
    let mut raw = vec![0u8; size];
    SysRng
        .try_fill_bytes(&mut raw)
        .expect("the operating system random source failed");
    base64_url(&raw)
}

/// A PKCE verifier: 64 random bytes, base64url, no padding.
pub fn new_verifier() -> String {
    random_url_safe(64)
}

/// The S256 challenge for a verifier.
pub fn challenge_for(verifier: &str) -> String {
    base64_url(&Sha256::digest(verifier.as_bytes()))
}

/// Whether a URL belongs to Thalovant, for a caller that wants to show where
/// it is about to send somebody. Scheme and host only: a display check, not an
/// authorization one.
pub fn is_thalovant_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    // Reject embedded credentials: https://evil.test@dash.thalovant.com/ has a
    // host that passes, and a URL somebody is about to be sent to should not
    // read as one host and resolve to another.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    match parsed.host_str() {
        Some(host) => {
            let host = host.to_ascii_lowercase();
            host == "thalovant.com" || host.ends_with(".thalovant.com")
        }
        None => false,
    }
}

/// Start a sign-in, returning the URL to open and the secrets to keep.
pub fn begin_native_sign_in(options: NativeSignInOptions) -> Result<NativeSignIn> {
    let client_id = options.client_id.trim().to_string();
    let redirect_uri = options.redirect_uri.trim().to_string();
    if client_id.is_empty() {
        return Err(ThalovantError::Api(
            "client_id is required to start a sign-in".to_string(),
        ));
    }
    if redirect_uri.is_empty() {
        return Err(ThalovantError::Api(
            "redirect_uri is required to start a sign-in".to_string(),
        ));
    }
    require_safe_dashboard(options.dashboard_url.as_deref())?;
    let verifier = new_verifier();
    let state = random_url_safe(24);
    let scopes = options.scopes.unwrap_or_else(|| {
        DEFAULT_NATIVE_SCOPES
            .iter()
            .map(|scope| (*scope).to_string())
            .collect()
    });
    let dashboard = options
        .dashboard_url
        .unwrap_or_else(|| DEFAULT_DASHBOARD_URL.to_string());
    let dashboard = dashboard.trim_end_matches('/');
    let mut url = Url::parse(&format!("{dashboard}/authorize"))
        .map_err(|_| ThalovantError::Api("could not build the sign-in address".to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("code_challenge", &challenge_for(&verifier))
        // S256 only. `plain` is refused by the API, and offering it here would
        // only give a caller a way to ask for the weaker one.
        .append_pair("code_challenge_method", "S256")
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", &state);
    Ok(NativeSignIn {
        authorization_url: url.to_string(),
        state,
        verifier,
        redirect_uri,
    })
}

impl NativeSignIn {
    /// The authorization code out of the redirect the browser came back with,
    /// or `None` when it is not an answer to this attempt.
    ///
    /// `None` rather than an error on a state mismatch, a missing code, or an
    /// `error=` response -- including one that also carries a code: all of
    /// those mean "do not continue", and a caller that
    /// handles them alike cannot accidentally treat one of them as success.
    pub fn code_from(&self, redirect: &str) -> Option<String> {
        let parsed = Url::parse(redirect).ok()?;
        // The callback has to arrive where this attempt asked it to. State
        // proves the answer belongs to this request; the address proves it
        // came back to the app that made it.
        if !same_target(&parsed, &self.redirect_uri) {
            return None;
        }
        let mut state = None;
        let mut code = None;
        let mut refused = false;
        for (key, value) in parsed.query_pairs() {
            match key.as_ref() {
                "state" => state = Some(value.into_owned()),
                "code" => code = Some(value.into_owned()),
                // A refusal that also carries a code is still a refusal.
                // Checking only for a missing code accepted that pair and
                // would have started an exchange on a code the server had just
                // declined to issue.
                "error" => refused = true,
                _ => {}
            }
        }
        if refused || state.as_deref() != Some(self.state.as_str()) {
            return None;
        }
        code.filter(|value| !value.is_empty())
    }
}

fn same_target(got: &Url, expected: &str) -> bool {
    let Ok(want) = Url::parse(expected) else {
        return false;
    };
    got.scheme().eq_ignore_ascii_case(want.scheme())
        && got.host_str().map(str::to_ascii_lowercase)
            == want.host_str().map(str::to_ascii_lowercase)
        && got.path().trim_end_matches('/') == want.path().trim_end_matches('/')
}

/// Refuse to hand the authorization request to a dashboard that cannot be
/// trusted with it.
///
/// The request carries the challenge, the scopes and the state. A caller may
/// point this at their own dashboard -- a self-hosted control plane is a real
/// thing -- but not at a cleartext one, and not at one whose address reads as
/// a different host than it resolves to. Loopback is allowed: it never leaves
/// the machine.
fn require_safe_dashboard(url: Option<&str>) -> Result<()> {
    let Some(raw) = url.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(()); // the default is used instead
    };
    let parsed = Url::parse(raw)
        .map_err(|_| ThalovantError::Api(format!("dashboard_url is not a URL: {raw}")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ThalovantError::Api(
            "dashboard_url must not carry credentials".to_string(),
        ));
    }
    // A query or a fragment breaks the address this builds: "<dash>#x" becomes
    // "<dash>#x/authorize?client_id=..." and every parameter lands in the
    // fragment, which a browser never sends. A query mangles the path likewise.
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ThalovantError::Api(
            "dashboard_url must not carry a query or a fragment".to_string(),
        ));
    }
    if parsed.scheme() == "https" {
        return Ok(());
    }
    if parsed.scheme() == "http"
        && matches!(
            parsed.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        )
    {
        return Ok(());
    }
    Err(ThalovantError::Api(format!(
        "dashboard_url must be https (or a loopback address while developing), not {raw}"
    )))
}

/// Refuse to put an authorization code and its PKCE verifier on the wire in
/// cleartext.
///
/// The control-plane URL accepts an `http` scheme -- a self-hosted or local
/// deployment may legitimately be served that way -- and `request` hands
/// whatever it is given to the HTTP client without looking. Every other call
/// that would leak over http leaks a bearer token the caller already holds;
/// this one leaks the two secrets that are about to become one, and a code is
/// exchangeable by whoever sees it first.
///
/// Loopback is allowed: a request that never leaves the machine has no
/// cleartext to observe, and that is how the control plane is run while
/// somebody is working on it.
pub(crate) fn require_secure_token_exchange(api_url: &str) -> Result<()> {
    // Never the URL itself in an error: one that does not parse can still
    // carry a credential (`https://user:secret@[bad`).
    let parsed = Url::parse(api_url)
        .map_err(|_| ThalovantError::Api("the Thalovant API URL could not be read".into()))?;
    if parsed.scheme() == "https" {
        return Ok(());
    }
    if matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    ) {
        return Ok(());
    }
    Err(ThalovantError::Api(format!(
        "refusing to send an authorization code and PKCE verifier in cleartext to {}; use https, or a loopback address while developing",
        parsed.host_str().unwrap_or("the Thalovant API")
    )))
}

// -- browser sign-in on a loopback redirect (RFC 8252 section 7.3) -------------

/// The SDKs' own registered app, which a program signs in as through the
/// browser. `GET /v1/auth/native/config` names it as `sdk_client_id`.
pub const SDK_CLIENT_ID: &str = "thalovant-sdk";
/// The one path the loopback listener answers.
pub const LOOPBACK_CALLBACK_PATH: &str = "/callback";
/// How long a browser sign-in waits for the browser to come back.
pub const DEFAULT_BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest `device_name` the consent page shows and the token is named
/// by, in Unicode code points (the API refuses a longer one).
pub const MAX_DEVICE_NAME_LENGTH: usize = 120;
/// The name this SDK gives itself in [`default_device_name`].
pub const SDK_DEVICE_LABEL: &str = "thalovant-rust-sdk";

/// An OAuth `state`: 24 random bytes (192 bits) from the operating system,
/// base64url, no padding (32 characters).
///
/// It ties the answer the browser brings back to the request that sent it.
/// Compare it in constant time.
pub fn new_state() -> String {
    random_url_safe(24)
}

/// `http://127.0.0.1:<port>/callback`, the one redirect a program is answered on.
///
/// The IP literal, never `localhost`, which a hosts file or a resolver can
/// send elsewhere; that path; no query, no trailing slash. The API matches it
/// exactly and only for an unprivileged port, which is what a port bound as
/// port 0 always is. A port outside 1024 to 65535 is refused with
/// [`ThalovantError::Api`] rather than sent.
pub fn loopback_redirect_uri(port: u32) -> Result<String> {
    if !(1024..=65535).contains(&port) {
        return Err(ThalovantError::Api(format!(
            "a loopback redirect needs a port from 1024 to 65535, not {port}"
        )));
    }
    Ok(format!("http://127.0.0.1:{port}{LOOPBACK_CALLBACK_PATH}"))
}

/// The address a browser sign-in opens.
///
/// `{authorization_endpoint}?response_type=code&client_id=...&redirect_uri=...
/// &code_challenge=...&code_challenge_method=S256&state=...&scope=...
/// &device_name=...`, in that order, every value percent-encoded as RFC 3986
/// has it: `A-Z a-z 0-9 - . _ ~` as they are, every other byte of the UTF-8
/// form as `%XX` in uppercase hex, a space as `%20`. Empty strings among
/// `scopes` are dropped and the rest joined by one space; `scope` is left out
/// when nothing is left, and the API then grants its default (`hubs:read
/// clients:write`). `device_name` is cut to [`MAX_DEVICE_NAME_LENGTH`] code
/// points, and left out when it is empty or `None`. (`browser_login` gives a
/// `None` [`default_device_name`]; an empty name is the caller's way of
/// sending none.)
pub fn browser_authorization_url<I, S>(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
    scopes: I,
    device_name: Option<&str>,
) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut parameters = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_challenge", code_challenge.to_string()),
        // S256 only: `plain` puts the verifier in the browser.
        ("code_challenge_method", "S256".to_string()),
        ("state", state.to_string()),
    ];
    let scope = scopes
        .into_iter()
        .filter(|scope| !scope.as_ref().is_empty())
        .map(|scope| scope.as_ref().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    if !scope.is_empty() {
        parameters.push(("scope", scope));
    }
    // Code points, never bytes or grapheme clusters: the API's limit.
    let name: String = device_name
        .unwrap_or_default()
        .chars()
        .take(MAX_DEVICE_NAME_LENGTH)
        .collect();
    if !name.is_empty() {
        parameters.push(("device_name", name));
    }
    // `urlencoding::encode` keeps exactly the unreserved characters and
    // writes uppercase hex; a form encoder would write a space as `+`.
    let query = parameters
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{authorization_endpoint}?{query}")
}

/// `thalovant-rust-sdk on <hostname>`: what the consent page and the token
/// show when the caller names nothing, cut to [`MAX_DEVICE_NAME_LENGTH`] code
/// points. `thalovant-rust-sdk` alone when the host has no name to read.
pub fn default_device_name() -> String {
    let name = match hostname() {
        Some(host) => format!("{SDK_DEVICE_LABEL} on {host}"),
        None => SDK_DEVICE_LABEL.to_string(),
    };
    name.chars().take(MAX_DEVICE_NAME_LENGTH).collect()
}

fn hostname() -> Option<String> {
    let raw = read_hostname()?;
    let host = raw.trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// The kernel's node name, which is what `gethostname(2)` answers.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn read_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
}

/// The NetBIOS name, which is what `gethostname` answers on Windows.
#[cfg(windows)]
fn read_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// macOS and the BSDs: what `hostname(1)` prints, without unsafe code.
#[cfg(not(any(target_os = "linux", target_os = "android", windows)))]
fn read_hostname() -> Option<String> {
    let output = std::process::Command::new("hostname")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a machine can show a browser to the person signing in.
///
/// Never in CI (`CI` set) or over SSH (`SSH_CONNECTION` or `SSH_TTY` set):
/// the browser would open on another machine, or nowhere. Otherwise yes on
/// `macos` and `windows`; anywhere else -- Linux, the BSDs, Android -- only
/// with a display (`DISPLAY` or `WAYLAND_DISPLAY`). A variable counts as set
/// when it has a value: `CI=false` is set, an empty `DISPLAY` is not.
///
/// `environment` and `platform` (named as `std::env::consts::OS` names it:
/// `linux`, `macos`, `windows`, `freebsd`, ...) default to this process's.
pub fn can_open_browser(
    environment: Option<&HashMap<String, String>>,
    platform: Option<&str>,
) -> bool {
    let present = |name: &str| match environment {
        Some(variables) => variables.get(name).is_some_and(|value| !value.is_empty()),
        None => std::env::var_os(name).is_some_and(|value| !value.is_empty()),
    };
    if present("CI") || present("SSH_CONNECTION") || present("SSH_TTY") {
        return false;
    }
    if matches!(
        platform.unwrap_or(std::env::consts::OS),
        "macos" | "windows"
    ) {
        return true;
    }
    present("DISPLAY") || present("WAYLAND_DISPLAY")
}

/// Where a program signs in through the browser, from `GET
/// /v1/auth/native/config`.
///
/// `authorization_endpoint` is the dashboard page the browser opens;
/// `token_endpoint` the path on the API the code is exchanged at;
/// `sdk_client_id` the registered app the program signs in as. The API reads
/// them from its own settings, so a staging or self-hosted API sends the
/// browser to its own dashboard. The answer's `loopback_redirect_uri` is never
/// read: the redirect is fixed ([`loopback_redirect_uri`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeSignInConfig {
    /// The consent page, opened with the authorization request.
    pub authorization_endpoint: String,
    /// The path on the API the code is exchanged at.
    pub token_endpoint: String,
    /// The registered app a program signs in as (`thalovant-sdk`).
    pub sdk_client_id: String,
    /// Where a device login starts, when the API names it.
    pub device_authorization_endpoint: Option<String>,
}

impl NativeSignInConfig {
    /// Read the configuration; refuse one a browser sign-in cannot safely use.
    ///
    /// Unusable ([`ThalovantError::Api`], with no status): a body that is not
    /// a JSON object; an `authorization_endpoint` that is not https (or http
    /// on `localhost`, `127.0.0.1` or `::1`), has a host that is not an IP
    /// address or a DNS name of ASCII labels (`%`, `_`, an empty label, a
    /// non-ASCII name not written as `xn--`, a numeric last label that is not
    /// IPv4), or carries credentials, a query, a fragment, whitespace, control
    /// characters, port 0 or a port above 65535; a `token_endpoint` that is
    /// not a path on the API (one leading `/`, not `//`, every `%` followed by
    /// two hex digits, no `:`, `?`, `#`, `\` or whitespace); an
    /// `sdk_client_id` that is not 1 to 64 of `A-Z a-z 0-9 . _ -`; or a
    /// `code_challenge_methods_supported` that is not a list naming `S256`.
    /// Other members are ignored.
    pub fn from_value(payload: &Value) -> Result<Self> {
        let unusable = |reason: &str| {
            Err(ThalovantError::Api(format!(
                "the browser sign-in configuration {reason}"
            )))
        };
        let Some(payload) = payload.as_object() else {
            return unusable("is not an object");
        };
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let Some(authorization) =
            text("authorization_endpoint").filter(|value| usable_authorization_endpoint(value))
        else {
            return unusable("has no usable authorization_endpoint");
        };
        let Some(token) = text("token_endpoint").filter(|value| usable_token_endpoint(value))
        else {
            return unusable("has no usable token_endpoint");
        };
        let Some(client_id) = text("sdk_client_id").filter(|value| usable_client_id(value)) else {
            return unusable("has no usable sdk_client_id");
        };
        let offers_s256 = payload
            .get("code_challenge_methods_supported")
            .and_then(Value::as_array)
            .is_some_and(|methods| methods.iter().any(|method| method == "S256"));
        if !offers_s256 {
            return unusable("does not offer S256");
        }
        Ok(Self {
            authorization_endpoint: authorization.to_string(),
            token_endpoint: token.to_string(),
            sdk_client_id: client_id.to_string(),
            device_authorization_endpoint: text("device_authorization_endpoint")
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        })
    }
}

/// C0, DEL and C1: never part of an address a browser is sent to.
fn is_control(character: char) -> bool {
    let point = u32::from(character);
    point < 32 || (127..=159).contains(&point)
}

/// https, or http on a loopback host while developing; a host; no
/// credentials, query, fragment, whitespace, control characters, or a port no
/// browser can open.
fn usable_authorization_endpoint(value: &str) -> bool {
    if value.is_empty()
        || value
            .chars()
            .any(|character| character.is_whitespace() || is_control(character))
        || value.contains(['?', '#'])
    {
        return false;
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    // No query or fragment is left, so the authority ends at the first `/`.
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let Some(host) = authority_host(authority) else {
        return false;
    };
    // And one the browser's own URL parser takes: it reads a last label such
    // as `0x10` as a number too, and `example.0x10` is then no address, so
    // the default opener would refuse it and the sign-in would wait for a
    // callback that cannot come.
    if Url::parse(value).is_err() {
        return false;
    }
    match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"),
        _ => false,
    }
}

/// The lowercased host of an authority without userinfo, or `None` when it
/// has none, has a host a browser cannot reach ([`real_host`]), or a port
/// that is not a number from 1 to 65535 (an empty port is no port).
fn authority_host(authority: &str) -> Option<String> {
    let (host, bracketed, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (inside, after) = bracketed.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (inside, true, port)
    } else {
        if authority.contains(['[', ']']) {
            return None;
        }
        match authority.split_once(':') {
            Some((host, port)) => (host, false, Some(port)),
            None => (authority, false, None),
        }
    };
    if let Some(port) = port.filter(|port| !port.is_empty()) {
        let numeric = port.bytes().all(|byte| byte.is_ascii_digit());
        // 0 parses, and no browser can open it either.
        if !numeric
            || !port
                .parse::<u32>()
                .is_ok_and(|number| (1..=65535).contains(&number))
        {
            return None;
        }
    }
    let host = host.to_ascii_lowercase();
    real_host(&host, bracketed).then_some(host)
}

/// Whether `host` (lowercased, brackets off), as written, names something a
/// browser can reach.
///
/// An IPv6 address between brackets; an IPv4 address, which it must be
/// whenever its last label is a number, as a browser reads it; or a DNS name
/// of ASCII labels, each 1 to 63 letters, digits and inner hyphens, at most
/// 253 characters in all. So `%`, `_`, an empty label, a trailing dot and a
/// non-ASCII host (an international name is written in its `xn--` form) are
/// not hosts.
fn real_host(host: &str, bracketed: bool) -> bool {
    if bracketed {
        return host.parse::<Ipv6Addr>().is_ok();
    }
    if host.parse::<Ipv4Addr>().is_ok() {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let last = labels.last().copied().unwrap_or_default();
    if !last.is_empty() && last.bytes().all(|byte| byte.is_ascii_digit()) {
        return false; // numeric like an IPv4 address, and not one
    }
    let label = |label: &str| {
        let bytes = label.as_bytes();
        (1..=63).contains(&bytes.len())
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            && bytes.first() != Some(&b'-')
            && bytes.last() != Some(&b'-')
    };
    host.len() <= 253 && labels.into_iter().all(label)
}

/// A path on the API: one leading slash, every `%` followed by two hex
/// digits, and no colon, query, fragment, backslash, whitespace or control
/// character.
///
/// No colon, because a URL joiner can read what follows the slash as a URL of
/// its own (`/https://collector.example/token`, `/collector.example:443/token`),
/// and the exchange carries the code and the verifier. A broken escape
/// (`/v1/%zz`) is found here, while a device login can still stand in, rather
/// than after somebody approved.
fn usable_token_endpoint(value: &str) -> bool {
    let bytes = value.as_bytes();
    let whole_escapes = bytes.iter().enumerate().all(|(at, byte)| {
        *byte != b'%'
            || (bytes.get(at + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(at + 2).is_some_and(u8::is_ascii_hexdigit))
    });
    whole_escapes
        && value.starts_with('/')
        && !value.starts_with("//")
        && !value.chars().any(|character| {
            matches!(character, ':' | '?' | '#' | '\\')
                || character.is_whitespace()
                || is_control(character)
        })
}

/// 1 to 64 of `A-Z a-z 0-9 . _ -`.
fn usable_client_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn debug_redacts_every_part_of_a_url_that_can_carry_a_secret() {
        // begin_native_sign_in rejects only an empty redirect_uri, so it can
        // arrive with userinfo, a query or a fragment. None of those are safe
        // to put in a log.
        let redacted = super::redacted_url("https://user:pw@app.example/cb?code=abc#tok=xyz");
        assert!(!redacted.contains("pw"), "{redacted}");
        assert!(!redacted.contains("abc"), "{redacted}");
        assert!(!redacted.contains("xyz"), "{redacted}");
        assert!(redacted.contains("app.example/cb"), "{redacted}");
        // A plain endpoint survives intact -- redaction must not make a log useless.
        assert_eq!(
            super::redacted_url("https://app.example/cb"),
            "https://app.example/cb"
        );
        // RFC 3986 §4.2 network-path: an authority with no scheme. Matching
        // only "://" let this through with its credentials intact.
        let network_path = super::redacted_url("//user:pw@app.example/cb?code=abc");
        assert!(!network_path.contains("pw"), "{network_path}");
        assert!(!network_path.contains("abc"), "{network_path}");
        assert!(network_path.contains("app.example/cb"), "{network_path}");
    }

    use super::*;

    // The authorization-code grant, which every client that needed it wrote
    // for itself until this existed. What is tested is what is a security bug
    // when wrong and looks fine when wrong.

    fn begin(client_id: &str, redirect_uri: &str) -> NativeSignIn {
        begin_native_sign_in(NativeSignInOptions {
            client_id: client_id.to_string(),
            redirect_uri: redirect_uri.to_string(),
            ..Default::default()
        })
        .expect("a sign-in starts")
    }

    fn query(url: &str, name: &str) -> Option<String> {
        Url::parse(url)
            .ok()?
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    #[test]
    fn the_challenge_is_the_s256_of_the_verifier() {
        let begun = begin("thalovant-cli", "http://127.0.0.1:8765/");
        let expected = base64_url(&Sha256::digest(begun.verifier.as_bytes()));
        assert_eq!(
            query(&begun.authorization_url, "code_challenge"),
            Some(expected.clone())
        );
        assert_eq!(challenge_for(&begun.verifier), expected);
    }

    #[test]
    fn the_verifier_never_reaches_the_browser() {
        let begun = begin("app", "app://auth");
        assert!(!begun.authorization_url.contains(&begun.verifier));
    }

    #[test]
    fn only_s256_is_offered() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            query(&begun.authorization_url, "code_challenge_method"),
            Some("S256".to_string())
        );
    }

    #[test]
    fn every_attempt_gets_its_own_verifier_and_state() {
        let first = begin("app", "app://auth");
        let second = begin("app", "app://auth");
        assert_ne!(first.verifier, second.verifier);
        assert_ne!(first.state, second.state);
        assert_ne!(new_verifier(), new_verifier());
    }

    #[test]
    fn the_request_carries_what_the_authorize_endpoint_matches_on() {
        let begun = begin_native_sign_in(NativeSignInOptions {
            client_id: "thalovant-cli".to_string(),
            redirect_uri: "http://127.0.0.1:8765/".to_string(),
            scopes: Some(vec!["hubs:read".to_string(), "clients:write".to_string()]),
            ..Default::default()
        })
        .expect("a sign-in starts");
        let url = &begun.authorization_url;
        assert_eq!(query(url, "client_id"), Some("thalovant-cli".to_string()));
        assert_eq!(
            query(url, "redirect_uri"),
            Some("http://127.0.0.1:8765/".to_string())
        );
        assert_eq!(query(url, "response_type"), Some("code".to_string()));
        assert_eq!(
            query(url, "scope"),
            Some("hubs:read clients:write".to_string())
        );
        assert_eq!(query(url, "state"), Some(begun.state.clone()));
        assert!(url.starts_with("https://dash.thalovant.com/authorize?"));
    }

    #[test]
    fn the_default_scopes_are_the_three_a_free_plan_may_mint() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            query(&begun.authorization_url, "scope"),
            Some(DEFAULT_NATIVE_SCOPES.join(" "))
        );
    }

    #[test]
    fn a_redirect_answering_a_different_attempt_is_refused() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            begun.code_from("app://auth?code=abc&state=somebody-elses"),
            None
        );
        assert_eq!(
            begun.code_from(&format!("app://auth?code=abc&state={}", begun.state)),
            Some("abc".to_string())
        );
    }

    #[test]
    fn no_code_or_an_error_instead_is_not_success() {
        let begun = begin("app", "app://auth");
        for redirect in [
            format!("app://auth?state={}", begun.state),
            format!("app://auth?code=&state={}", begun.state),
            format!("app://auth?error=access_denied&state={}", begun.state),
            "app://auth".to_string(),
        ] {
            assert_eq!(
                begun.code_from(&redirect),
                None,
                "{redirect} was treated as success"
            );
        }
    }

    #[test]
    fn a_code_with_escaped_characters_survives_the_round_trip() {
        let begun = begin("app", "app://auth");
        assert_eq!(
            begun.code_from(&format!(
                "app://auth?code=a%2Bb%2Fc%3D&state={}",
                begun.state
            )),
            Some("a+b/c=".to_string())
        );
    }

    #[test]
    fn an_empty_client_or_redirect_is_refused_here_rather_than_at_the_api() {
        assert!(begin_native_sign_in(NativeSignInOptions {
            client_id: String::new(),
            redirect_uri: "app://auth".to_string(),
            ..Default::default()
        })
        .is_err());
        assert!(begin_native_sign_in(NativeSignInOptions {
            client_id: "app".to_string(),
            redirect_uri: "   ".to_string(),
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn a_refusal_that_also_carries_a_code_is_still_a_refusal() {
        // CodeRabbit caught this: checking only for a missing code accepted
        // error=access_denied&code=... and would have started an exchange on a
        // code the authorization server had just declined to issue.
        let begun = begin("app", "app://auth");
        for redirect in [
            format!(
                "app://auth?error=access_denied&code=abc&state={}",
                begun.state
            ),
            format!(
                "app://auth?code=abc&error=server_error&state={}",
                begun.state
            ),
        ] {
            assert_eq!(
                begun.code_from(&redirect),
                None,
                "{redirect} was treated as success"
            );
        }
    }

    #[test]
    fn the_token_exchange_refuses_cleartext_and_allows_loopback() {
        assert!(require_secure_token_exchange("http://control.example.test").is_err());
        // Loopback has no cleartext to observe, and is how the API is run locally.
        for allowed in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "https://api.thalovant.com",
        ] {
            assert!(
                require_secure_token_exchange(allowed).is_ok(),
                "{allowed} was refused"
            );
        }
    }

    #[test]
    fn a_callback_arriving_somewhere_else_is_refused() {
        // CodeRabbit: state proves the answer belongs to this request; it does
        // not prove it came back to the app that made it.
        let begun = begin("app", "app://auth");
        assert_eq!(
            begun.code_from(&format!("app://auth?code=abc&state={}", begun.state)),
            Some("abc".to_string())
        );
        for elsewhere in [
            format!("app://elsewhere?code=abc&state={}", begun.state),
            format!("https://evil.test/auth?code=abc&state={}", begun.state),
        ] {
            assert_eq!(
                begun.code_from(&elsewhere),
                None,
                "{elsewhere} was accepted"
            );
        }
    }

    #[test]
    fn a_dashboard_that_is_not_safe_is_refused() {
        for bad in [
            "http://dash.example.test",
            "https://evil.test@dash.thalovant.com",
            "ftp://dash.thalovant.com",
            // A fragment puts every parameter somewhere a browser never sends.
            "https://dash.example.test#section",
            "https://dash.example.test?next=/x",
        ] {
            assert!(
                begin_native_sign_in(NativeSignInOptions {
                    client_id: "app".to_string(),
                    redirect_uri: "app://auth".to_string(),
                    dashboard_url: Some(bad.to_string()),
                    ..Default::default()
                })
                .is_err(),
                "{bad} was accepted"
            );
        }
        // Self-hosted https is real, and loopback never leaves the machine.
        for good in [
            "https://dash.example.test",
            "http://localhost:9000",
            "http://127.0.0.1:9000",
        ] {
            assert!(
                begin_native_sign_in(NativeSignInOptions {
                    client_id: "app".to_string(),
                    redirect_uri: "app://auth".to_string(),
                    dashboard_url: Some(good.to_string()),
                    ..Default::default()
                })
                .is_ok(),
                "{good} was refused"
            );
        }
    }

    #[test]
    fn a_thalovant_url_is_recognised_by_scheme_and_host() {
        assert!(is_thalovant_url("https://dash.thalovant.com/authorize?x=1"));
        assert!(is_thalovant_url("https://thalovant.com"));
        assert!(!is_thalovant_url("http://dash.thalovant.com"));
        // The one that matters: a lookalike host ending in the same letters.
        assert!(!is_thalovant_url("https://dash.thalovant.com.evil.test"));
        // A host that passes, reached through credentials reading as another.
        assert!(!is_thalovant_url("https://evil.test@dash.thalovant.com"));
        assert!(!is_thalovant_url("https://notthalovant.com"));
        assert!(!is_thalovant_url("nonsense"));
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_verifier_or_the_state() {
        // An intercepted authorization code is redeemable by anyone who also
        // has the verifier, so a log line carrying it is enough to lose the
        // exchange. The state is redacted with it: it is the other half of what
        // proves a redirect answers this attempt.
        // Built the way begin_native_sign_in builds it, with the state in the
        // query string: a hand-made URL without one let the first version of
        // this test pass while Debug still printed the state.
        let sign_in = NativeSignIn {
            authorization_url: "https://hub.example/authorize?client_id=app&state=state-secret\
                 &code_challenge=abc&response_type=code"
                .into(),
            state: "state-secret".into(),
            verifier: "verifier-secret".into(),
            redirect_uri: "http://127.0.0.1:0/callback".into(),
        };
        let printed = format!("{sign_in:?}");
        assert!(!printed.contains("verifier-secret"), "{printed}");
        assert!(!printed.contains("state-secret"), "{printed}");
        assert!(
            printed.contains("https://hub.example/authorize"),
            "{printed}"
        );
    }
}
