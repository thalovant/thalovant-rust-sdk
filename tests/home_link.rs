//! The Home Assistant link, against the four shared vector files.
//!
//! `device-login`, `connection-kinds` and `connection-admission` are HTTP
//! exchanges: each case's answers are served by a loopback API, in order, and
//! every request the SDK sends through the real `ControlPlane` path is checked
//! against the one the case names -- method, path, body or body subset,
//! `If-Match` and `Authorization`. `home-link` holds the reply routing and the
//! request/response rules, run through `answer_home_request` with a replier
//! that records what it was asked to send. What the SDK produced is recorded
//! for the conformance record before it is compared with the case, shaped
//! exactly as the Python reference's `tests/test_home_link_vectors.py` shapes
//! it. The vector files are vendored byte for byte from the reference.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use thalovant::{
    answer_home_request, reply_context, ApiRefusal, BootstrapIdentityOptions, ControlPlane, Data,
    DeviceAuthorization, Event, HomeAnswer, OperationResource, Replier, ThalovantError,
    ERROR_CODES, HOME_ASSISTANT_SCOPES, HOME_REQUEST, HOME_REQUEST_TIMEOUT, HOME_RESPONSE,
    RESPONSE_TYPES,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod common;

// No lifetimes or other lone apostrophes in this file: the parity checker
// reads string literals with a scanner that takes `'` for a quote, and a
// `&'static` here hides every vector name after it.

fn vectors(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("tests/conformance/{name}"))
        .unwrap_or_else(|error| panic!("read {name}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("parse {name}: {error}"))
}

fn cases(spec: &Value) -> &Vec<Value> {
    spec["cases"].as_array().expect("cases")
}

fn name(case: &Value) -> &str {
    case["name"].as_str().expect("name")
}

/// A duration the vectors give in seconds.
fn seconds(value: &Value) -> Duration {
    Duration::from_secs_f64(value.as_f64().expect("seconds"))
}

/// A duration as the reference records it: whole seconds as an integer.
fn recorded_seconds(duration: Duration) -> Value {
    if duration.subsec_nanos() == 0 {
        Value::from(duration.as_secs())
    } else {
        Value::from(duration.as_secs_f64())
    }
}

// -- the loopback API ---------------------------------------------------------

#[derive(Default)]
struct Script {
    exchanges: Vec<Value>,
    index: usize,
    served: Vec<usize>,
    sent: Vec<String>,
    mismatches: Vec<String>,
}

/// Serves a case's exchanges in order and checks each request against its own.
/// An exchange marked `repeat` answers every request after it.
struct ScriptedApi {
    url: String,
    script: Arc<Mutex<Script>>,
    server: tokio::task::JoinHandle<()>,
}

impl ScriptedApi {
    async fn start(exchanges: &Value) -> Self {
        let exchanges = exchanges.as_array().cloned().unwrap_or_default();
        let script = Arc::new(Mutex::new(Script {
            served: vec![0; exchanges.len()],
            exchanges,
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
        assert!(
            script.served.iter().all(|count| *count > 0),
            "{case}: not every exchange was used: {:?}",
            script.served
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
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_string();
    let target = request_line.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default().to_string();
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
    let body: Option<Value> = (!body_text.is_empty())
        .then(|| serde_json::from_str(&body_text).expect("a JSON request body"));
    let if_match = header("if-match");
    let authorization = header("authorization");

    let response = {
        let mut script = script.lock().unwrap();
        script.sent.push(match &if_match {
            Some(etag) => format!("{method} {path} If-Match={etag}"),
            None => format!("{method} {path}"),
        });
        if script.index >= script.exchanges.len() {
            script
                .mismatches
                .push(format!("unexpected {method} {path}"));
            None
        } else {
            let at = script.index;
            let exchange = script.exchanges[at].clone();
            script.served[at] += 1;
            if exchange["repeat"] != Value::Bool(true) {
                script.index += 1;
            }
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
            if let Some(json) = expected.get("json") {
                if body.as_ref() != Some(json) {
                    script
                        .mismatches
                        .push(format!("{method} {path}: body is not {json}"));
                }
            }
            if let Some(subset) = expected.get("json_subset") {
                if !body.as_ref().is_some_and(|body| contains(body, subset)) {
                    script
                        .mismatches
                        .push(format!("{method} {path}: body lacks {subset}"));
                }
            }
            if let Some(etag) = expected.get("if_match") {
                if if_match.as_deref() != etag.as_str() {
                    script
                        .mismatches
                        .push(format!("{method} {path}: If-Match {if_match:?} != {etag}"));
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
            Some(exchange["response"].clone())
        }
    };
    let (status, content_type, body) = match &response {
        Some(response) => (
            response["status"].as_u64().expect("status") as u16,
            response["content_type"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            response["body"].as_str().unwrap_or_default().to_string(),
        ),
        None => (599, "application/json".to_string(), "{}".to_string()),
    };
    let reason = reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Status");
    let mut reply = format!("HTTP/1.1 {status} {reason}\r\n");
    if !body.is_empty() {
        reply.push_str(&format!("content-type: {content_type}\r\n"));
    }
    reply.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    ));
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn contains(value: &Value, subset: &Value) -> bool {
    match subset {
        Value::Object(fields) => fields.iter().all(|(key, item)| {
            value
                .get(key)
                .is_some_and(|present| contains(present, item))
        }),
        other => value == other,
    }
}

/// Neither the device code, the token nor a generated credential is ever in
/// an error's message, its `Debug` or its alternate `Debug`.
fn excluded(error: &ThalovantError, spec: &Value, case: &str) {
    for secret in spec["message_excludes"].as_array().into_iter().flatten() {
        let secret = secret.as_str().expect("a string");
        for form in [
            error.to_string(),
            format!("{error:?}"),
            format!("{error:#?}"),
        ] {
            assert!(!form.contains(secret), "{case}: an error leaked a secret");
        }
    }
}

fn api_fields(error: &ThalovantError) -> Map<String, Value> {
    Map::from_iter([
        ("status".to_string(), json!(error.status_code())),
        ("code".to_string(), json!(error.api_code())),
        ("detail".to_string(), json!(error.api_detail())),
    ])
}

// -- device login -------------------------------------------------------------

async fn poll_once(
    control: &mut ControlPlane,
    authorization: &DeviceAuthorization,
    spec: &Value,
    case: &str,
) -> Value {
    match control.poll_device_login(authorization).await {
        Ok(token) => {
            assert_eq!(
                control.access_token.as_deref(),
                Some(token.access_token.as_str())
            );
            assert_eq!(control.token_id(), token.token_id.as_deref());
            assert!(!format!("{token:?}").contains(&token.access_token));
            json!({
                "outcome": "approved",
                "token_type": token.token_type,
                "scopes": token.scopes,
                "expires_at": token.expires_at,
                "token_id": token.token_id,
            })
        }
        Err(error) => {
            excluded(&error, spec, case);
            match &error {
                ThalovantError::DeviceLoginPending { interval, .. } => {
                    json!({"outcome": "pending", "interval": recorded_seconds(*interval)})
                }
                ThalovantError::DeviceLoginExpired { .. } => {
                    json!({"outcome": "expired", "status": error.status_code()})
                }
                ThalovantError::DeviceLoginDenied { .. } => {
                    json!({"outcome": "denied", "status": error.status_code()})
                }
                _ => {
                    let mut produced = Map::from_iter([
                        ("outcome".to_string(), json!("error")),
                        ("status".to_string(), json!(error.status_code())),
                    ]);
                    if error.status_code().is_some() {
                        produced.extend(api_fields(&error));
                    }
                    Value::Object(produced)
                }
            }
        }
    }
}

async fn device_case(case: &Value, spec: &Value) -> (Value, ScriptedApi) {
    let call = &case["call"];
    let api = ScriptedApi::start(&case["exchanges"]).await;
    let mut control = ControlPlane::new(api.url.clone(), None);
    let mut produced = Vec::new();
    let case_name = name(case);
    if call["op"] == "begin" {
        let scopes: Vec<&str> = call["scopes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        match control
            .begin_device_login(&scopes, call["client_name"].as_str())
            .await
        {
            Ok(grant) => produced.push(json!({
                "outcome": "started",
                "user_code": grant.user_code,
                "verification_uri": grant.verification_uri,
                "verification_uri_complete": grant.verification_uri_complete,
                "interval": recorded_seconds(grant.poll_interval()),
                "expires_in": grant.expires_in,
            })),
            Err(error) => {
                excluded(&error, spec, case_name);
                produced.push(json!({"outcome": "error", "status": error.status_code()}));
            }
        }
    } else {
        let authorization = DeviceAuthorization {
            device_code: call["authorization"]["device_code"]
                .as_str()
                .expect("device_code")
                .to_string(),
            user_code: String::new(),
            verification_uri: "https://x".to_string(),
            verification_uri_complete: None,
            expires_in: Some(900),
            interval: call["authorization"]["interval"].as_u64(),
            raw: Map::new(),
        };
        for _ in 0..call["times"].as_u64().unwrap_or(1) {
            produced.push(poll_once(&mut control, &authorization, spec, case_name).await);
        }
        if call["op"] == "revoke" {
            control
                .revoke_api_token(None)
                .await
                .expect("the token revokes itself");
            assert!(control.access_token.is_none() && control.token_id().is_none());
            produced = vec![json!({"outcome": "revoked"})];
        }
    }
    (Value::Array(produced), api)
}

#[tokio::test]
async fn device_login_runs_its_vectors() {
    let spec = vectors("device-login-vectors.json");
    for case in cases(&spec) {
        let (produced, api) = device_case(case, &spec).await;
        // Recorded before the assert: the record is what this SDK produced,
        // not a restatement of what the vector says it should have.
        common::record("device-login-vectors.json", name(case), &produced);
        api.finish(name(case));
        assert_eq!(produced, case["expect"], "{}", name(case));
    }
}

// -- connection kinds ---------------------------------------------------------

fn kind_outcome(error: &ThalovantError) -> &str {
    match error.api_refusal() {
        Some(ApiRefusal::Plan) => "plan",
        Some(ApiRefusal::AlreadyLinked) => "already_linked",
        Some(ApiRefusal::Auth) => "auth",
        _ => "error",
    }
}

async fn kinds_case(case: &Value, spec: &Value) -> (Value, ScriptedApi) {
    let call = &case["call"];
    let api = ScriptedApi::start(&case["exchanges"]).await;
    let control = ControlPlane::new(api.url.clone(), Some("synthetic-token".to_string()));
    let case_name = name(case);
    let mut produced = if call["op"] == "create" {
        let options = BootstrapIdentityOptions {
            name: call["name"].as_str().expect("name").to_string(),
            ..Default::default()
        };
        let created = control
            .create_client_identity_of_type(
                call["hub"].clone(),
                call["connection_type"].as_str().expect("connection_type"),
                options,
            )
            .await;
        match created {
            Ok(result) => {
                assert!(!format!("{result:?}").contains("synthetic-hub-password"));
                json!({
                    "outcome": "created",
                    "client_id": result.client_id(),
                    "connection_type": result.connection_type(),
                    "operation_id": result.operation().map(|operation| operation.id),
                })
            }
            Err(error @ ThalovantError::UnsupportedConnectionType { .. }) => {
                excluded(&error, spec, case_name);
                let mut produced = Map::from_iter([("outcome".to_string(), json!("unsupported"))]);
                if error.status_code().is_some() {
                    produced.extend(api_fields(&error));
                } else {
                    let deleted = api.sent().iter().any(|line| line.starts_with("DELETE "));
                    produced.insert("deleted".to_string(), json!(deleted));
                }
                Value::Object(produced)
            }
            Err(error) => {
                excluded(&error, spec, case_name);
                // A refusal is classified, never re-typed: what create
                // returns for a 402 is what every other call returns for one.
                assert!(
                    matches!(error, ThalovantError::ApiResponse { .. }),
                    "{case_name}: {error:?}"
                );
                let mut produced =
                    Map::from_iter([("outcome".to_string(), json!(kind_outcome(&error)))]);
                produced.extend(api_fields(&error));
                if error.api_refusal() == Some(ApiRefusal::AlreadyLinked) {
                    produced.insert("client_id".to_string(), json!(error.linked_client_id()));
                }
                Value::Object(produced)
            }
        }
    } else {
        let deleted = control
            .delete_client(
                call["client_id"].as_str().expect("client_id"),
                call["etag"].as_str(),
            )
            .await;
        match deleted {
            Ok(()) => json!({"outcome": "deleted"}),
            Err(error) => {
                let mut produced =
                    Map::from_iter([("outcome".to_string(), json!(kind_outcome(&error)))]);
                produced.extend(api_fields(&error));
                Value::Object(produced)
            }
        }
    };
    produced["requests"] = json!(api.sent());
    (produced, api)
}

#[tokio::test]
async fn connection_kinds_run_their_vectors() {
    let spec = vectors("connection-kinds-vectors.json");
    for case in cases(&spec) {
        let (produced, api) = kinds_case(case, &spec).await;
        common::record("connection-kinds-vectors.json", name(case), &produced);
        api.finish(name(case));
        assert_eq!(produced, case["expect"], "{}", name(case));
    }
}

// -- connection admission -----------------------------------------------------

async fn admission_case(case: &Value) -> (Value, ScriptedApi) {
    let call = &case["call"];
    let api = ScriptedApi::start(&case["exchanges"]).await;
    let control = ControlPlane::new(api.url.clone(), Some("synthetic-token".to_string()));
    let operation: Option<OperationResource> = (!call["operation"].is_null())
        .then(|| serde_json::from_value(call["operation"].clone()).expect("an operation"));
    let waited = control
        .wait_for_admission(
            operation.as_ref(),
            seconds(&call["timeout_seconds"]),
            seconds(&call["poll_interval_seconds"]),
        )
        .await;
    let polls = api.sent().len();
    let produced = match waited {
        Ok(()) => json!({"outcome": "admitted", "polls": polls}),
        Err(error @ ThalovantError::AdmissionTimeout { .. }) => {
            assert!(error.is_connection_error() && error.is_timeout());
            json!({"outcome": "timeout"})
        }
        Err(ThalovantError::AdmissionFailed { error_code, .. }) => {
            json!({"outcome": "failed", "error_code": error_code, "polls": polls})
        }
        Err(ThalovantError::Api(_) | ThalovantError::ApiResponse { .. }) => {
            json!({"outcome": "error", "polls": polls})
        }
        Err(other) => panic!("{}: unexpected {other:?}", name(case)),
    };
    (produced, api)
}

#[tokio::test]
async fn connection_admission_runs_its_vectors() {
    let spec = vectors("connection-admission-vectors.json");
    for case in cases(&spec) {
        let (produced, api) = admission_case(case).await;
        common::record("connection-admission-vectors.json", name(case), &produced);
        api.finish(name(case));
        assert_eq!(produced, case["expect"], "{}", name(case));
    }
}

// -- the home link --------------------------------------------------------------

/// Records every reply it is asked to send.
#[derive(Default)]
struct Replies(Mutex<Vec<(String, Data)>>);

impl Replier for Replies {
    fn reply(
        &self,
        _event: &Event,
        msg_type: &str,
        data: Data,
    ) -> impl Future<Output = thalovant::Result<()>> + Send {
        self.0.lock().unwrap().push((msg_type.to_string(), data));
        async { Ok(()) }
    }
}

async fn run_handler(spec: Value) -> Result<HomeAnswer, String> {
    if spec["raises"] == Value::Bool(true) {
        return Err("the conversation agent is gone".to_string());
    }
    if let Some(wait) = spec["sleep_seconds"].as_f64() {
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    }
    let mut answer = HomeAnswer::new(
        spec["response_type"].as_str().unwrap_or("action_done"),
        spec["speech"].as_str().unwrap_or_default(),
    )
    .with_continue_conversation(spec["continue_conversation"] == Value::Bool(true));
    if let Some(code) = spec["error_code"].as_str() {
        answer = answer.with_error_code(code);
    }
    Ok(answer)
}

#[tokio::test]
async fn home_link_runs_its_vectors() {
    let spec = vectors("home-link-vectors.json");
    for case in cases(&spec) {
        let produced = if case["kind"] == "reply_context" {
            let context = case["context"].as_object().expect("context");
            Value::Object(reply_context(context))
        } else {
            let replies = Replies::default();
            let event = Event::new(
                HOME_REQUEST,
                case["request"].as_object().expect("request").clone(),
                json!({"source": "skill"}).as_object().unwrap().clone(),
                None,
            );
            let timeout = case
                .get("timeout_seconds")
                .map(seconds)
                .unwrap_or(thalovant::DEFAULT_HOME_HANDLER_TIMEOUT);
            let handler = case["handler"].clone();
            let payload = answer_home_request(
                &replies,
                &event,
                move |_request| run_handler(handler),
                timeout,
            )
            .await
            .expect("the answer is sent");
            let sent = replies.0.into_inner().unwrap();
            assert_eq!(
                sent,
                vec![(HOME_RESPONSE.to_string(), payload.clone())],
                "{}: exactly one response",
                name(case)
            );
            Value::Object(payload)
        };
        common::record("home-link-vectors.json", name(case), &produced);
        assert_eq!(produced, case["expect"], "{}", name(case));
    }
}

#[test]
fn the_contract_lists_match_the_sdk() {
    let home = vectors("home-link-vectors.json");
    let device = vectors("device-login-vectors.json");
    assert_eq!(json!(RESPONSE_TYPES), home["response_types"]);
    assert_eq!(json!(ERROR_CODES), home["error_codes"]);
    assert_eq!(HOME_REQUEST, home["request_type"]);
    assert_eq!(HOME_RESPONSE, home["response_type"]);
    assert_eq!(
        json!(HOME_REQUEST_TIMEOUT.as_secs()),
        home["reply_timeout_seconds"]
    );
    assert_eq!(HOME_REQUEST_TIMEOUT.subsec_nanos(), 0);
    assert_eq!(
        json!(HOME_ASSISTANT_SCOPES),
        device["home_assistant_scopes"]
    );
}

// -- beyond the vectors -------------------------------------------------------

#[tokio::test]
async fn an_unexpected_device_poll_failure_carries_what_the_api_said() {
    // The fix the port asked for: this path used to drop the status and the
    // body for a one-line string, and login_with_browser rides it too.
    let api = ScriptedApi::start(&json!([{
        "request": {"method": "POST", "path": "/v1/auth/device/authorize"},
        "response": {"status": 200, "content_type": "application/json",
            "body": "{\"device_code\":\"dc-1\",\"user_code\":\"U\",\"verification_uri\":\"https://x.example/a\",\"interval\":0}"}
    }, {
        "request": {"method": "POST", "path": "/v1/auth/device/token", "json": {"device_code": "dc-1"}},
        "response": {"status": 429, "content_type": "application/problem+json",
            "body": "{\"detail\":\"Slow down, for real.\",\"code\":\"rate_limited\"}"}
    }]))
    .await;
    let mut control = ControlPlane::new(api.url.clone(), None);
    let error = control
        .login_with_browser(thalovant::DeviceLoginOptions {
            open_browser: false,
            prompt: Some(Box::new(|_| {})),
            ..Default::default()
        })
        .await
        .expect_err("a 429 is not a sign-in");
    assert_eq!(error.status_code(), Some(429));
    assert_eq!(error.api_code(), Some("rate_limited"));
    assert_eq!(error.api_detail(), Some("Slow down, for real."));
    assert!(!format!("{error:?}").contains("dc-1"));
    api.finish("login_with_browser");
}

#[tokio::test]
async fn slow_down_stays_with_its_device_code_and_a_refusal_keeps_its_kind() {
    let body = |error: &str| {
        json!({"request": {"method": "POST", "path": "/v1/auth/device/token"},
            "response": {"status": 400, "content_type": "application/json",
                "body": format!("{{\"error\":\"{error}\"}}")}})
    };
    let api = ScriptedApi::start(&json!([
        body("slow_down"),
        body("slow_down"),
        body("authorization_pending"),
        body("authorization_pending"),
    ]))
    .await;
    let mut control = ControlPlane::new(api.url.clone(), None);
    let grant = |code: &str| DeviceAuthorization {
        device_code: code.to_string(),
        user_code: String::new(),
        verification_uri: "https://x".to_string(),
        verification_uri_complete: None,
        expires_in: None,
        interval: Some(5),
        raw: Map::new(),
    };
    let mut intervals = Vec::new();
    for code in ["a", "a", "a", "b"] {
        match control.poll_device_login(&grant(code)).await {
            Err(ThalovantError::DeviceLoginPending { interval, .. }) => {
                intervals.push(interval.as_secs())
            }
            other => panic!("{other:?}"),
        }
    }
    // Two slow_downs add ten seconds to "a", for good; "b" starts afresh.
    assert_eq!(intervals, vec![10, 15, 15, 5]);
    api.finish("slow_down");

    let refused = ThalovantError::ApiResponse {
        status_code: 423,
        detail: "locked".into(),
        problem: None,
    };
    assert_eq!(refused.api_refusal(), Some(ApiRefusal::Auth));
    assert_eq!(refused.linked_client_id(), None);
    let nested: Map<String, Value> = serde_json::from_value(json!({
        "code": "home_assistant_already_linked",
        "detail": {"existing_client_id": "c-9"}
    }))
    .unwrap();
    let linked = ThalovantError::ApiResponse {
        status_code: 409,
        detail: "linked".into(),
        problem: Some(Box::new(nested.into())),
    };
    assert_eq!(linked.api_refusal(), Some(ApiRefusal::AlreadyLinked));
    assert_eq!(linked.linked_client_id(), Some("c-9"));
}
