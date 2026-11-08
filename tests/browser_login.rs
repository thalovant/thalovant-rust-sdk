//! Browser sign-in against `tests/conformance/browser-login-vectors.json`.
//!
//! The pure sections (PKCE, the redirect, the authorization URL, the
//! configuration, the browser rule) run against the public functions in
//! `thalovant::native_auth` that decide them. `sign_ins` and `chooser` run
//! for real: a loopback API serves each case's exchanges in order and checks
//! every request the SDK sends through the real `ControlPlane` path, and a
//! raw HTTP client plays the browser against the port the SDK bound.
//! `callbacks` runs in `src/loopback_tests.rs`, the private suite of the
//! listener whose classifier it checks. What the SDK produced is recorded
//! under `<section>/<name>` for the conformance record before it is compared
//! with the case, shaped as the Python reference's
//! `tests/test_browser_login_vectors.py` shapes it. The vector file is
//! vendored byte for byte from the reference.
//!
//! The SDK is async only, so each case runs in the one form there is.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use thalovant::native_auth::{
    browser_authorization_url, can_open_browser, challenge_for, default_device_name,
    loopback_redirect_uri, new_state, new_verifier, NativeSignInConfig,
    DEFAULT_BROWSER_LOGIN_TIMEOUT, LOOPBACK_CALLBACK_PATH, MAX_DEVICE_NAME_LENGTH, SDK_CLIENT_ID,
    SDK_DEVICE_LABEL,
};
use thalovant::{
    BrowserLoginOptions, BrowserOpener, ControlPlane, InteractiveLoginOptions, LoginMethod,
    ThalovantError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

fn vectors() -> Value {
    let raw = std::fs::read_to_string("tests/conformance/browser-login-vectors.json")
        .expect("read the browser-login vectors");
    serde_json::from_str(&raw).expect("parse the browser-login vectors")
}

fn section(spec: &Value, name: &str) -> Vec<Value> {
    spec[name].as_array().cloned().expect("a section")
}

fn name(case: &Value) -> String {
    case["name"].as_str().expect("name").to_string()
}

fn record(section: &str, case: &Value, produced: &Value) {
    common::record(
        "browser-login-vectors.json",
        &format!("{section}/{}", name(case)),
        produced,
    );
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item.as_str().expect("a string").to_string())
        .collect()
}

fn base64url_sha256(text: &str) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(text.as_bytes()))
}

fn unreserved(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

fn base64url(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

// -- the pure sections --------------------------------------------------------

#[test]
fn pkce_runs_its_vectors() {
    let spec = vectors();
    for case in section(&spec, "pkce") {
        let produced =
            json!({"challenge": challenge_for(case["verifier"].as_str().expect("verifier"))});
        record("pkce", &case, &produced);
        assert_eq!(produced, case["expect"], "{}", name(&case));
    }
}

#[test]
fn redirect_uris_run_their_vectors() {
    let spec = vectors();
    for case in section(&spec, "redirect_uris") {
        let port = u32::try_from(case["port"].as_u64().expect("port")).expect("a u32 port");
        let produced = match loopback_redirect_uri(port) {
            Ok(redirect_uri) => json!({"redirect_uri": redirect_uri}),
            Err(ThalovantError::Api(_)) => json!({"error": "invalid_port"}),
            Err(other) => panic!("{}: unexpected {other}", name(&case)),
        };
        record("redirect_uris", &case, &produced);
        assert_eq!(produced, case["expect"], "{}", name(&case));
    }
}

#[test]
fn authorization_urls_run_their_vectors() {
    let spec = vectors();
    for case in section(&spec, "authorization_urls") {
        let given = &case["input"];
        let port = u32::try_from(given["port"].as_u64().expect("port")).expect("a u32 port");
        let redirect_uri = loopback_redirect_uri(port).expect("a usable port");
        let url = browser_authorization_url(
            given["authorization_endpoint"].as_str().expect("endpoint"),
            given["client_id"].as_str().expect("client_id"),
            &redirect_uri,
            given["code_challenge"].as_str().expect("code_challenge"),
            given["state"].as_str().expect("state"),
            strings(&given["scopes"]),
            given["device_name"].as_str(),
        );
        let produced = json!({"url": url});
        record("authorization_urls", &case, &produced);
        assert_eq!(produced, case["expect"], "{}", name(&case));
    }
}

#[test]
fn config_runs_its_vectors() {
    let spec = vectors();
    for case in section(&spec, "config") {
        let produced = match NativeSignInConfig::from_value(&case["body"]) {
            Ok(config) => json!({
                "usable": true,
                "authorization_endpoint": config.authorization_endpoint,
                "token_endpoint": config.token_endpoint,
                "sdk_client_id": config.sdk_client_id,
            }),
            Err(error) => {
                assert!(
                    error.status_code().is_none(),
                    "an unusable answer has no status"
                );
                json!({"usable": false})
            }
        };
        record("config", &case, &produced);
        assert_eq!(produced, case["expect"], "{}", name(&case));
    }
}

fn environment_of(case: &Value) -> HashMap<String, String> {
    case["env"]
        .as_object()
        .expect("env")
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().expect("a value").to_string()))
        .collect()
}

#[test]
fn environment_runs_its_vectors() {
    let spec = vectors();
    for case in section(&spec, "environment") {
        let environment = environment_of(&case);
        let platform = case["platform"].as_str().expect("platform");
        let produced = json!({"browser": can_open_browser(Some(&environment), Some(platform))});
        record("environment", &case, &produced);
        assert_eq!(produced, case["expect"], "{}", name(&case));
    }
}

// -- an API, and a browser, on loopback ----------------------------------------

/// What the test learnt from the URL the SDK built.
#[derive(Clone)]
struct Signing {
    endpoint: String,
    pairs: Vec<(String, String)>,
    port: u16,
    state: String,
    challenge: String,
}

impl Signing {
    fn read(url: &str) -> Self {
        let (endpoint, query) = url.split_once('?').expect("a query");
        let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        let value = |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("no {key} in the URL"))
        };
        let redirect_uri = value("redirect_uri");
        let port: u16 = redirect_uri
            .strip_prefix("http://127.0.0.1:")
            .and_then(|rest| rest.strip_suffix(LOOPBACK_CALLBACK_PATH))
            .and_then(|port| port.parse().ok())
            .expect("redirect_uri is http://127.0.0.1:<port>/callback");
        assert!(port >= 1024, "an unprivileged port");
        let state = value("state");
        assert!(
            state.len() >= 22 && base64url(&state),
            "state must be at least 128 bits of base64url"
        );
        let challenge = value("code_challenge");
        assert!(challenge.len() == 43 && base64url(&challenge));
        Self {
            endpoint: endpoint.to_string(),
            pairs,
            port,
            state,
            challenge,
        }
    }

    fn normalized(&self, text: &str) -> String {
        text.replace(
            &format!("http://127.0.0.1:{}/callback", self.port),
            "http://127.0.0.1:{port}/callback",
        )
        .replace(&self.state, "{state}")
        .replace(&self.challenge, "{challenge}")
    }

    fn url_record(&self) -> Value {
        json!({
            "endpoint": self.endpoint,
            "query": self
                .pairs
                .iter()
                .map(|(key, value)| json!([key, self.normalized(value)]))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Default)]
struct Script {
    exchanges: Vec<Value>,
    index: usize,
    sent: Vec<String>,
    mismatches: Vec<String>,
    signing: Option<Signing>,
    exchange: Option<Value>,
    verifier: Option<String>,
}

impl Script {
    /// The exchange body with the generated values put back as placeholders,
    /// once proven: exactly four fields, and a verifier whose S256 challenge
    /// is the one the URL carried.
    fn token_body(&mut self, body: Value) -> Value {
        let (Some(signing), Value::Object(fields)) = (self.signing.clone(), &body) else {
            return body;
        };
        let keys: BTreeSet<&str> = fields.keys().map(String::as_str).collect();
        if keys != BTreeSet::from(["client_id", "code", "code_verifier", "redirect_uri"]) {
            self.mismatches.push(format!(
                "the exchange must carry exactly four fields, not {keys:?}"
            ));
        }
        let mut clean: Map<String, Value> = fields
            .iter()
            .map(|(key, value)| match value {
                Value::String(text) => (key.clone(), Value::from(signing.normalized(text))),
                other => (key.clone(), other.clone()),
            })
            .collect();
        match fields.get("code_verifier").and_then(Value::as_str) {
            Some(verifier)
                if (43..=128).contains(&verifier.len())
                    && unreserved(verifier)
                    && base64url_sha256(verifier) == signing.challenge =>
            {
                clean.insert("code_verifier".into(), Value::from("{verifier}"));
                self.verifier = Some(verifier.to_string());
            }
            _ => self
                .mismatches
                .push("the code_verifier is not the one the challenge was made from".into()),
        }
        let clean = Value::Object(clean);
        self.exchange = Some(clean.clone());
        clean
    }
}

/// Serves a case's exchanges in order and checks each request against its own.
struct ScriptedApi {
    url: String,
    script: Arc<Mutex<Script>>,
    server: tokio::task::JoinHandle<()>,
}

impl ScriptedApi {
    async fn start(exchanges: &Value) -> Self {
        let script = Arc::new(Mutex::new(Script {
            exchanges: exchanges.as_array().cloned().unwrap_or_default(),
            ..Default::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let shared = script.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(answer(stream, shared.clone()));
            }
        });
        Self {
            url,
            script,
            server,
        }
    }

    fn sent(&self) -> Vec<String> {
        self.script.lock().unwrap().sent.clone()
    }

    fn exchange(&self) -> Option<Value> {
        self.script.lock().unwrap().exchange.clone()
    }

    fn verifier(&self) -> Option<String> {
        self.script.lock().unwrap().verifier.clone()
    }

    /// Stop serving, then check that nothing was wrong and every exchange was
    /// used.
    fn finish(self, case: &str) {
        self.server.abort();
        let script = self.script.lock().unwrap();
        assert!(
            script.mismatches.is_empty(),
            "{case}: {:?}",
            script.mismatches
        );
        assert_eq!(
            script.index,
            script.exchanges.len(),
            "{case}: not every exchange was used: {:?}",
            script.sent
        );
    }
}

async fn answer(mut stream: TcpStream, script: Arc<Mutex<Script>>) {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(size) => raw.extend_from_slice(&buffer[..size]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut request_line = head.split("\r\n").next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    let header = |wanted: &str| {
        head.split("\r\n").skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(wanted)
                .then(|| value.trim().to_string())
        })
    };
    let length: usize = header("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    while raw.len() < head_end + length {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(size) => raw.extend_from_slice(&buffer[..size]),
        }
    }
    let body_text = String::from_utf8_lossy(&raw[head_end..head_end + length]).to_string();
    let body: Option<Value> =
        (!body_text.is_empty()).then(|| serde_json::from_str(&body_text).expect("a JSON body"));
    let authorization = header("authorization");

    let (status, content_type, text) = {
        let mut script = script.lock().unwrap();
        script.sent.push(format!("{method} {path}"));
        if script.index >= script.exchanges.len() {
            script
                .mismatches
                .push(format!("unexpected {method} {path}"));
            (599, "application/json".to_string(), "{}".to_string())
        } else {
            let exchange = script.exchanges[script.index].clone();
            script.index += 1;
            let expected = &exchange["request"];
            if (method.as_str(), path.as_str())
                != (
                    expected["method"].as_str().unwrap_or_default(),
                    expected["path"].as_str().unwrap_or_default(),
                )
            {
                script.mismatches.push(format!(
                    "{method} {path} != {} {}",
                    expected["method"], expected["path"]
                ));
            }
            let token_request = path == "/v1/auth/native/token"
                || body
                    .as_ref()
                    .is_some_and(|body| body.get("code_verifier").is_some());
            let checked = match (&body, token_request) {
                (Some(body), true) => Some(script.token_body(body.clone())),
                (body, _) => body.clone(),
            };
            if let Some(json) = expected.get("json") {
                if checked.as_ref() != Some(json) {
                    script.mismatches.push(format!(
                        "{method} {path}: the body is not the one the case names"
                    ));
                }
            }
            if let Some(wanted) = expected.get("authorization") {
                if authorization.as_deref() != wanted.as_str() {
                    // Never the header itself: it is the token.
                    script
                        .mismatches
                        .push(format!("{method} {path}: wrong Authorization header"));
                }
            }
            let response = &exchange["response"];
            let status = response["status"].as_u64().expect("status") as u16;
            let text = if response["echo"] == Value::Bool(true) {
                echo(body.as_ref())
            } else {
                response["body"].as_str().unwrap_or_default().to_string()
            };
            let content_type = response["content_type"]
                .as_str()
                .unwrap_or("application/json")
                .to_string();
            (status, content_type, text)
        }
    };
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Status");
    let mut reply = format!("HTTP/1.1 {status} {reason}\r\n");
    if !text.is_empty() {
        reply.push_str(&format!("content-type: {content_type}\r\n"));
    }
    reply.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    ));
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A validation error that lists every field it was sent, as FastAPI's 422
/// does, and the code in a key too.
fn echo(body: Option<&Value>) -> String {
    let fields = body.and_then(Value::as_object).cloned().unwrap_or_default();
    let detail: Vec<Value> = fields
        .iter()
        .map(|(key, value)| json!({"loc": ["body", key], "msg": "bad", "input": value}))
        .collect();
    let code = fields
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({"detail": detail, format!("seen:{code}"): "x"}).to_string()
}

struct Tab {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

impl Tab {
    fn page(&self, spec: &Value) -> Value {
        spec["pages"]
            .as_object()
            .expect("pages")
            .iter()
            .find(|(_, text)| self.body.contains(text.as_str().expect("a page text")))
            .map(|(key, _)| Value::from(key.as_str()))
            .unwrap_or(Value::Null)
    }
}

/// One request on its own connection, as a browser tab sends it.
async fn browse(port: u16, method: &str, target: &str) -> Tab {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the listener");
    stream
        .write_all(
            format!(
                "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read the answer");
    let text = String::from_utf8(raw).expect("a UTF-8 answer");
    let (head, body) = text.split_once("\r\n\r\n").expect("a head and a body");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|status| status.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    Tab {
        status,
        headers,
        body: body.to_string(),
    }
}

/// The vectors' headers on every answer, and a page that loads nothing.
fn check_page(tab: &Tab, spec: &Value) {
    for (name, value) in spec["page_headers"].as_object().expect("page_headers") {
        assert_eq!(
            tab.headers.get(name).map(String::as_str),
            value.as_str(),
            "{name}"
        );
    }
    for loads in ["src=", "href=", "http"] {
        assert!(!tab.body.contains(loads), "the page loads something");
    }
}

async fn refused(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).await.is_err()
}

/// Neither a synthetic secret, the verifier nor the state reaches an error:
/// its message, both `Debug` forms, or its problem.
fn no_secrets(error: &ThalovantError, spec: &Value, generated: &[Option<String>]) {
    let mut hidden = strings(&spec["message_excludes"]);
    hidden.extend(generated.iter().flatten().cloned());
    let mut shown = vec![
        error.to_string(),
        format!("{error:?}"),
        format!("{error:#?}"),
    ];
    if let Some(problem) = error.api_problem() {
        shown.push(Value::Object(problem.as_map().clone()).to_string());
    }
    for secret in &hidden {
        assert!(
            !shown.iter().any(|text| text.contains(secret.as_str())),
            "a secret reached the error"
        );
    }
}

/// What a sign-in came to, shaped as the vectors' `results` say.
fn result_of(control: &ControlPlane, outcome: &thalovant::Result<Value>) -> Value {
    match outcome {
        Ok(token) => {
            assert_eq!(
                control.access_token.as_deref(),
                token["access_token"].as_str()
            );
            assert_eq!(control.token_id(), token["token_id"].as_str());
            json!({
                "outcome": "approved",
                "token_type": token["token_type"],
                "scopes": token["scopes"],
                "expires_at": token["expires_at"],
                "token_id": token["token_id"],
            })
        }
        Err(error @ ThalovantError::DeviceAuthorizationDenied) => {
            json!({"outcome": "denied", "status": error.status_code()})
        }
        Err(error @ ThalovantError::DeviceAuthorizationExpired) => {
            json!({"outcome": "expired", "status": error.status_code()})
        }
        Err(error) => {
            let mut produced = json!({
                "outcome": "error",
                "status": error.status_code(),
                "code": error.api_code(),
            });
            if error.status_code().is_some() {
                produced["detail"] = json!(error.api_detail());
            }
            produced
        }
    }
}

/// Run one sign-in for real: serve the case's API, take the URL the SDK
/// presents, play the case's browser requests, and record what happened.
async fn sign_in<Start, Running>(case: &Value, spec: &Value, start: Start) -> (Value, ScriptedApi)
where
    Start: FnOnce(ControlPlane, mpsc::UnboundedSender<String>) -> Running,
    Running: Future<Output = (ControlPlane, thalovant::Result<Value>)> + Send + 'static,
{
    let api = ScriptedApi::start(&case["exchanges"]).await;
    let (hand_over, mut opened) = mpsc::unbounded_channel();
    let mut task = tokio::spawn(start(ControlPlane::new(api.url.clone(), None), hand_over));
    let mut finished = None;
    let url = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            url = opened.recv() => url,
            joined = &mut task => {
                finished = Some(joined.expect("the sign-in task"));
                None
            }
        }
    })
    .await
    .expect("the sign-in neither opened a URL nor ended");

    let mut produced = Map::new();
    let mut signing = None;
    if let Some(url) = url {
        let read = Signing::read(&url);
        api.script.lock().unwrap().signing = Some(read.clone());
        produced.insert("authorization_url".into(), read.url_record());
        let mut browser = Vec::new();
        for request in case["requests"].as_array().into_iter().flatten() {
            let target = request["target"]
                .as_str()
                .expect("target")
                .replace("{state}", &read.state);
            let tab = browse(
                read.port,
                request["method"].as_str().expect("method"),
                &target,
            )
            .await;
            check_page(&tab, spec);
            let mut step = json!({"status": tab.status});
            if tab.status == 200 {
                step["page"] = tab.page(spec);
            }
            browser.push(step);
        }
        produced.insert("browser".into(), Value::Array(browser));
        signing = Some(read);
    }
    let (mut control, outcome) = match finished {
        Some(finished) => finished,
        None => tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the sign-in ended")
            .expect("the sign-in task"),
    };
    if let Err(error) = &outcome {
        let state = signing.as_ref().map(|signing| signing.state.clone());
        no_secrets(error, spec, &[api.verifier(), state]);
    }
    if let Some(signing) = &signing {
        produced.insert("closed".into(), Value::Bool(refused(signing.port).await));
    }
    if let Some(exchange) = api.exchange() {
        produced.insert("exchange".into(), exchange);
    }
    let mut result = result_of(&control, &outcome);
    if case["call"]["then"] == "revoke" && result["outcome"] == "approved" {
        control
            .revoke_api_token(None)
            .await
            .expect("the token revokes itself");
        assert!(control.access_token.is_none() && control.token_id().is_none());
        // Idempotent: revoking again sends nothing and succeeds.
        control
            .revoke_api_token(None)
            .await
            .expect("revoking again changes nothing");
        result = json!({"outcome": "revoked"});
    }
    produced.insert("result".into(), result);
    (Value::Object(produced), api)
}

fn timeout_of(call: &Value) -> Option<Duration> {
    call.get("timeout_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
}

#[tokio::test]
async fn sign_ins_run_their_vectors() {
    let spec = vectors();
    for case in section(&spec, "sign_ins") {
        let call = case["call"].clone();
        let (produced, api) = sign_in(&case, &spec, move |mut control, hand_over| async move {
            let options = BrowserLoginOptions {
                scopes: strings(&call["scopes"]),
                device_name: call["device_name"].as_str().map(str::to_string),
                open_browser: false,
                timeout: timeout_of(&call).unwrap_or(DEFAULT_BROWSER_LOGIN_TIMEOUT),
                prompt: Some(Box::new(move |url: &str| {
                    let _ = hand_over.send(url.to_string());
                })),
                opener: None,
            };
            let outcome = control.browser_login(options).await;
            (control, outcome)
        })
        .await;
        // Recorded before the assert: the record is what this SDK produced,
        // not a restatement of what the vector says it should have.
        record("sign_ins", &case, &produced);
        api.finish(&name(&case));
        assert_eq!(produced, case["expect"], "{}", name(&case));
    }
}

#[tokio::test]
async fn the_chooser_runs_its_vectors() {
    let spec = vectors();
    for case in section(&spec, "chooser") {
        let call = case["call"].clone();
        let method = match call["method"].as_str().expect("method") {
            "auto" => LoginMethod::Auto,
            "browser" => LoginMethod::Browser,
            "device" => LoginMethod::Device,
            other => panic!("no such method: {other}"),
        };
        let environment = environment_of(&case);
        let platform = case["platform"].as_str().expect("platform").to_string();
        let (produced, api) = sign_in(&case, &spec, move |mut control, hand_over| async move {
            // The browser the SDK opens. Device login opens its verification
            // page too; only the consent page is this test's to answer.
            let opener: BrowserOpener = Arc::new(move |url: &str| {
                if url.contains("response_type=code") {
                    let _ = hand_over.send(url.to_string());
                }
            });
            let options = InteractiveLoginOptions {
                method,
                device_name: call["device_name"].as_str().map(str::to_string),
                timeout: timeout_of(&call),
                opener: Some(opener),
                environment: Some(environment),
                platform: Some(platform),
                ..Default::default()
            };
            let outcome = control.interactive_login(options).await;
            (control, outcome)
        })
        .await;
        let used = if api
            .sent()
            .iter()
            .any(|sent| sent == "POST /v1/auth/device/authorize")
        {
            "device"
        } else {
            "browser"
        };
        let result = json!({"method": used, "result": produced["result"]});
        record("chooser", &case, &result);
        if used == "browser" && produced.get("browser").is_some() {
            assert_eq!(
                produced["closed"],
                true,
                "{}: the port is still open",
                name(&case)
            );
        }
        api.finish(&name(&case));
        assert_eq!(result, case["expect"], "{}", name(&case));
    }
}

#[test]
fn every_section_is_run() {
    // `callbacks` is run by src/loopback_tests.rs; the other seven here.
    let spec = vectors();
    let sections: BTreeSet<&str> = spec
        .as_object()
        .expect("an object")
        .iter()
        .filter(|(_, value)| {
            value
                .as_array()
                .and_then(|cases| cases.first())
                .is_some_and(|first| first.get("name").is_some())
        })
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(
        sections,
        BTreeSet::from([
            "authorization_urls",
            "callbacks",
            "chooser",
            "config",
            "environment",
            "pkce",
            "redirect_uris",
            "sign_ins",
        ])
    );
}

#[test]
fn the_rfc_7636_vector_is_the_first_pkce_case() {
    let first = &vectors()["pkce"][0];
    assert_eq!(
        first["verifier"],
        "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
    );
    assert_eq!(
        first["expect"]["challenge"],
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn the_constants_are_the_contract() {
    let spec = vectors();
    assert_eq!(spec["client_id"], SDK_CLIENT_ID);
    assert_eq!(
        spec["default_timeout_ms"].as_u64(),
        Some(DEFAULT_BROWSER_LOGIN_TIMEOUT.as_millis() as u64)
    );
    assert_eq!(
        spec["device_name_max_code_points"].as_u64(),
        Some(MAX_DEVICE_NAME_LENGTH as u64)
    );
    let defaults = BrowserLoginOptions::default();
    assert!(defaults.open_browser && defaults.scopes.is_empty() && defaults.device_name.is_none());
    assert_eq!(defaults.timeout, DEFAULT_BROWSER_LOGIN_TIMEOUT);
    let interactive = InteractiveLoginOptions::default();
    assert_eq!(interactive.method, LoginMethod::Auto);
    assert!(interactive.open_browser && interactive.timeout.is_none());
    // Debug shows no callback.
    let printed = format!(
        "{:?}",
        BrowserLoginOptions {
            prompt: Some(Box::new(|_: &str| {})),
            ..Default::default()
        }
    );
    assert!(printed.contains("<callback>"));
}

#[test]
fn every_verifier_and_state_is_fresh_and_well_formed() {
    let verifiers: BTreeSet<String> = (0..50).map(|_| new_verifier()).collect();
    let states: BTreeSet<String> = (0..50).map(|_| new_state()).collect();
    assert_eq!((verifiers.len(), states.len()), (50, 50));
    assert!(verifiers
        .iter()
        .all(|verifier| (43..=128).contains(&verifier.len()) && unreserved(verifier)));
    // 24 random bytes: 192 bits, well over the 128 the contract asks for.
    assert!(states
        .iter()
        .all(|state| state.len() == 32 && base64url(state)));
}

#[test]
fn the_device_name_defaults_to_this_sdk_on_this_host() {
    let name = default_device_name();
    assert!(name.starts_with(SDK_DEVICE_LABEL), "{name}");
    assert_eq!(SDK_DEVICE_LABEL, "thalovant-rust-sdk");
    assert!(name == SDK_DEVICE_LABEL || name.starts_with("thalovant-rust-sdk on "));
    assert!(name.chars().count() <= MAX_DEVICE_NAME_LENGTH);
}

#[test]
fn the_browser_rule_reads_this_process_by_default() {
    let here: HashMap<String, String> = [
        "CI",
        "SSH_CONNECTION",
        "SSH_TTY",
        "DISPLAY",
        "WAYLAND_DISPLAY",
    ]
    .into_iter()
    .filter_map(|name| {
        // Lossy, never dropped: a value that is not Unicode is still set.
        std::env::var_os(name).map(|value| (name.to_string(), value.to_string_lossy().into_owned()))
    })
    .collect();
    assert_eq!(
        can_open_browser(None, None),
        can_open_browser(Some(&here), Some(std::env::consts::OS))
    );
    // Android and iOS are neither macOS nor Windows: no display, no browser.
    for platform in ["android", "ios"] {
        assert!(!can_open_browser(Some(&HashMap::new()), Some(platform)));
    }
}

#[test]
fn an_authorization_endpoint_no_browser_can_open_is_unusable() {
    let config = |endpoint: &str| {
        json!({
            "authorization_endpoint": endpoint,
            "token_endpoint": "/v1/auth/native/token",
            "sdk_client_id": "thalovant-sdk",
            "code_challenge_methods_supported": ["S256"],
        })
    };
    for endpoint in [
        "https://dash.thalovant.com/author\u{7}ize",
        "https://dash.thalovant.com /authorize",
        "https://dash.thalovant.com:99999/authorize",
        "https://[::1/authorize",
        "https://dash.thalovant.com:8x/authorize",
        "https:///authorize",
        "https://dash.thalovant.com?",
        "javascript://dash.thalovant.com/authorize",
        "https://bad|host/authorize",
        "https://dash.thalovant.com:0/authorize",
        "https://dash.thalovant.com./authorize",
        "https://-dash.thalovant.com/authorize",
        "https://1.2.3/authorize",
        "https://[127.0.0.1]/authorize",
    ] {
        assert!(
            NativeSignInConfig::from_value(&config(endpoint)).is_err(),
            "{endpoint:?} was usable"
        );
    }
    for endpoint in [
        "https://dash.thalovant.com:8443/authorize",
        "HTTPS://Dash.Thalovant.com/authorize",
        "http://[::1]:5173/authorize",
        "http://127.0.0.1/authorize",
    ] {
        assert!(
            NativeSignInConfig::from_value(&config(endpoint)).is_ok(),
            "{endpoint:?} was unusable"
        );
    }
    let usable = NativeSignInConfig::from_value(&json!({
        "authorization_endpoint": "https://dash.thalovant.com/authorize",
        "token_endpoint": "/v1/auth/native/token",
        "sdk_client_id": "thalovant-sdk",
        "code_challenge_methods_supported": ["S256"],
        "device_authorization_endpoint": "/v1/auth/device/authorize",
    }))
    .expect("usable");
    assert_eq!(
        usable.device_authorization_endpoint.as_deref(),
        Some("/v1/auth/device/authorize")
    );
    assert!(NativeSignInConfig::from_value(&json!(["not", "an", "object"])).is_err());
}

// -- the sign-in beyond the vectors ------------------------------------------------

const CONFIG: &str = r#"{"authorization_endpoint":"https://dash.thalovant.com/authorize","token_endpoint":"/v1/auth/native/token","sdk_client_id":"thalovant-sdk","code_challenge_methods_supported":["S256"]}"#;

fn exchange(method: &str, path: &str, status: u16, body: &str) -> Value {
    json!({
        "request": {"method": method, "path": path},
        "response": {"status": status, "content_type": "application/json", "body": body},
    })
}

/// A sign-in on its own task, the URL it presents, and the task.
async fn started(
    api: &ScriptedApi,
    options: BrowserLoginOptions,
) -> (
    Option<String>,
    tokio::task::JoinHandle<(ControlPlane, thalovant::Result<Value>)>,
) {
    let (hand_over, mut opened) = mpsc::unbounded_channel();
    let options = BrowserLoginOptions {
        prompt: Some(Box::new(move |url: &str| {
            let _ = hand_over.send(url.to_string());
        })),
        open_browser: false,
        ..options
    };
    let mut control = ControlPlane::new(api.url.clone(), None);
    let task = tokio::spawn(async move {
        let outcome = control.browser_login(options).await;
        (control, outcome)
    });
    let url = tokio::time::timeout(Duration::from_secs(10), opened.recv())
        .await
        .expect("a URL in time");
    (url, task)
}

#[tokio::test]
async fn cancelling_the_sign_in_closes_the_port_and_the_default_name_reaches_the_page() {
    let api = ScriptedApi::start(&json!([exchange(
        "GET",
        "/v1/auth/native/config",
        200,
        CONFIG
    )]))
    .await;
    let (url, task) = started(&api, BrowserLoginOptions::default()).await;
    let signing = Signing::read(&url.expect("a URL"));
    let device_name = signing
        .pairs
        .iter()
        .find(|(key, _)| key == "device_name")
        .map(|(_, value)| value.clone());
    assert_eq!(device_name, Some(default_device_name()));
    assert!(
        !signing.pairs.iter().any(|(key, _)| key == "scope"),
        "no scope asked for"
    );
    assert!(!refused(signing.port).await, "the listener is waiting");
    task.abort();
    assert!(task.await.err().is_some_and(|error| error.is_cancelled()));
    assert!(
        refused(signing.port).await,
        "a cancelled sign-in left its port open"
    );
    api.finish("cancelled");
}

#[tokio::test]
async fn a_connection_that_sends_nothing_does_not_hold_the_sign_in() {
    let spec = vectors();
    let token = r#"{"access_token":"tvpat_synthetic_browser_token_do_not_log","token_type":"bearer","scopes":[],"token_id":"t-1"}"#;
    let api = ScriptedApi::start(&json!([
        exchange("GET", "/v1/auth/native/config", 200, CONFIG),
        exchange("POST", "/v1/auth/native/token", 200, token),
    ]))
    .await;
    let (url, task) = started(&api, BrowserLoginOptions::default()).await;
    let signing = Signing::read(&url.expect("a URL"));
    api.script.lock().unwrap().signing = Some(signing.clone());
    // A browser opens connections it never sends anything on.
    let idle = TcpStream::connect(("127.0.0.1", signing.port))
        .await
        .expect("connect");
    let begun = std::time::Instant::now();
    let tab = browse(
        signing.port,
        "GET",
        &format!("/callback?code=c&state={}", signing.state),
    )
    .await;
    assert_eq!((tab.status, tab.page(&spec)), (200, json!("signed_in")));
    let (control, outcome) = task.await.expect("the sign-in task");
    assert!(outcome.is_ok());
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "the idle connection held the sign-in"
    );
    assert_eq!(control.token_id(), Some("t-1"));
    drop(idle);
    api.finish("idle");
}

#[tokio::test]
async fn an_echoed_code_and_verifier_never_reach_the_error() {
    let spec = vectors();
    let api = ScriptedApi::start(&json!([
        exchange("GET", "/v1/auth/native/config", 200, CONFIG),
        {
            "request": {"method": "POST", "path": "/v1/auth/native/token"},
            "response": {"status": 422, "content_type": "application/json", "echo": true},
        },
    ]))
    .await;
    let (url, task) = started(&api, BrowserLoginOptions::default()).await;
    let signing = Signing::read(&url.expect("a URL"));
    api.script.lock().unwrap().signing = Some(signing.clone());
    let tab = browse(
        signing.port,
        "GET",
        &format!("/callback?code=secret-code-value&state={}", signing.state),
    )
    .await;
    assert_eq!((tab.status, tab.page(&spec)), (200, json!("not_completed")));
    let (_, outcome) = task.await.expect("the sign-in task");
    let error = outcome.expect_err("a refused exchange");
    assert_eq!(error.status_code(), Some(422));
    let verifier = api.verifier().expect("the exchange was sent");
    let problem =
        Value::Object(error.api_problem().expect("a problem").as_map().clone()).to_string();
    let shown = [
        error.to_string(),
        format!("{error:?}"),
        format!("{error:#?}"),
        problem.clone(),
    ];
    for secret in [
        "secret-code-value",
        verifier.as_str(),
        signing.state.as_str(),
    ] {
        assert!(
            !shown.iter().any(|text| text.contains(secret)),
            "a secret reached the error"
        );
    }
    assert!(
        problem.contains("[redacted]"),
        "the echo was scrubbed, not dropped"
    );
    api.finish("echo");
}

#[tokio::test]
async fn an_api_url_that_cannot_be_read_is_never_repeated_in_the_error() {
    // It can still carry a credential, and a caller reports the error.
    let opened: BrowserOpener = Arc::new(|_: &str| panic!("the browser was opened"));
    for broken in [
        "https://user:hunter2-secret@[bad",
        "http://user:hunter2-secret@[::1",
    ] {
        let mut control = ControlPlane::new(broken, None);
        let browser = control
            .browser_login(BrowserLoginOptions {
                opener: Some(opened.clone()),
                ..Default::default()
            })
            .await
            .expect_err("an API URL that cannot be read");
        let forced = control
            .interactive_login(InteractiveLoginOptions {
                method: LoginMethod::Browser,
                opener: Some(opened.clone()),
                ..Default::default()
            })
            .await
            .expect_err("an API URL that cannot be read");
        for error in [browser, forced] {
            assert!(error.status_code().is_none());
            for form in [
                error.to_string(),
                format!("{error:?}"),
                format!("{error:#?}"),
            ] {
                assert!(
                    !form.contains("hunter2-secret"),
                    "the API URL reached the error"
                );
            }
        }
    }
}

#[tokio::test]
async fn a_cleartext_api_is_refused_before_anything_is_asked_or_opened() {
    let opened: BrowserOpener = Arc::new(|_: &str| panic!("the browser was opened"));
    let mut control = ControlPlane::new("http://control.example.test", None);
    let error = control
        .browser_login(BrowserLoginOptions {
            opener: Some(opened.clone()),
            ..Default::default()
        })
        .await
        .expect_err("cleartext");
    assert!(matches!(&error, ThalovantError::Api(message) if message.contains("cleartext")));
    let error = control
        .interactive_login(InteractiveLoginOptions {
            method: LoginMethod::Browser,
            opener: Some(opened),
            ..Default::default()
        })
        .await
        .expect_err("cleartext");
    assert!(matches!(&error, ThalovantError::Api(message) if message.contains("cleartext")));
}

#[tokio::test]
async fn an_answer_that_is_not_json_is_an_error_with_no_status() {
    for route in ["config", "token"] {
        let garbled = json!({
            "request": {"method": if route == "config" { "GET" } else { "POST" }, "path": format!("/v1/auth/native/{route}")},
            "response": {"status": 200, "content_type": "text/html", "body": "<html>not json</html>"},
        });
        let exchanges = if route == "config" {
            json!([garbled])
        } else {
            json!([
                exchange("GET", "/v1/auth/native/config", 200, CONFIG),
                garbled
            ])
        };
        let api = ScriptedApi::start(&exchanges).await;
        let (url, task) = started(&api, BrowserLoginOptions::default()).await;
        if let Some(url) = url {
            let signing = Signing::read(&url);
            api.script.lock().unwrap().signing = Some(signing.clone());
            browse(
                signing.port,
                "GET",
                &format!("/callback?code=c&state={}", signing.state),
            )
            .await;
        }
        let (_, outcome) = task.await.expect("the sign-in task");
        let error = outcome.expect_err("not JSON");
        assert!(matches!(error, ThalovantError::Api(_)), "{route}: {error}");
        assert_eq!(error.status_code(), None, "{route}");
        api.finish(route);
    }
}

#[tokio::test]
async fn the_prompt_does_not_stop_the_browser_and_without_open_browser_nothing_opens() {
    for open_browser in [true, false] {
        let api = ScriptedApi::start(&json!([exchange(
            "GET",
            "/v1/auth/native/config",
            200,
            CONFIG
        )]))
        .await;
        let (shown, mut prompted) = mpsc::unbounded_channel();
        let (opened, mut browsed) = mpsc::unbounded_channel();
        let options = BrowserLoginOptions {
            open_browser,
            timeout: Duration::from_millis(300),
            prompt: Some(Box::new(move |url: &str| {
                let _ = shown.send(url.to_string());
            })),
            opener: Some(Arc::new(move |url: &str| {
                let _ = opened.send(url.to_string());
            })),
            ..Default::default()
        };
        let mut control = ControlPlane::new(api.url.clone(), None);
        let outcome = control.browser_login(options).await;
        assert!(matches!(
            outcome,
            Err(ThalovantError::DeviceAuthorizationExpired)
        ));
        let shown = prompted.recv().await.expect("the prompt was given the URL");
        assert!(shown.contains("response_type=code"));
        match browsed.recv().await {
            Some(url) => assert!(open_browser && url == shown, "opened without open_browser"),
            None => assert!(!open_browser, "the browser was not opened"),
        }
        api.finish("prompt");
    }
}

#[tokio::test]
async fn auto_without_a_browser_never_asks_for_the_configuration() {
    let token = r#"{"access_token":"tvpat_synthetic_device_token_do_not_log","token_type":"bearer","scopes":[],"token_id":"d-1"}"#;
    let authorization = r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://thalovant.com/activate","verification_uri_complete":"https://thalovant.com/activate?code=ABCD-EFGH","expires_in":900,"interval":0}"#;
    let api = ScriptedApi::start(&json!([
        {
            "request": {"method": "POST", "path": "/v1/auth/device/authorize", "json": {"client_name": default_device_name(), "scopes": ["hubs:read"]}},
            "response": {"status": 200, "content_type": "application/json", "body": authorization},
        },
        exchange("POST", "/v1/auth/device/token", 200, token),
    ]))
    .await;
    let (opened, mut browsed) = mpsc::unbounded_channel();
    let mut control = ControlPlane::new(api.url.clone(), None);
    let token = control
        .interactive_login(InteractiveLoginOptions {
            scopes: vec!["hubs:read".into()],
            opener: Some(Arc::new(move |url: &str| {
                let _ = opened.send(url.to_string());
            })),
            environment: Some(HashMap::from([(
                "SSH_TTY".to_string(),
                "/dev/pts/1".to_string(),
            )])),
            platform: Some("macos".into()),
            ..Default::default()
        })
        .await
        .expect("a device login");
    assert_eq!(token["token_id"], "d-1");
    assert_eq!(control.token_id(), Some("d-1"));
    // The device login opened its own page through the same opener.
    assert_eq!(
        browsed.recv().await.as_deref(),
        Some("https://thalovant.com/activate?code=ABCD-EFGH")
    );
    api.finish("device");
}
