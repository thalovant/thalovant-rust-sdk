//! Browser sign-in on the control plane, beyond the shared vectors: what is
//! private to the crate and cannot be reached from `tests/`.
//!
//! The scrub that keeps the code, the verifier and the state out of an
//! error; the origin every request stays on; the bound on each request of a
//! sign-in, which a stalled API would otherwise hold for good; and what a
//! failed exchange carries. Colocated, as this SDK's private suites are: this
//! file is the private `tests` module of `src/control/browser_login.rs` and is
//! recorded as test evidence beside it. `tests/browser_login.rs` runs the
//! vectors.

use super::*;

use std::sync::Mutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const CONFIG: &str = r#"{"authorization_endpoint":"https://dash.thalovant.com/authorize","token_endpoint":"/v1/auth/native/token","sdk_client_id":"thalovant-sdk","code_challenge_methods_supported":["S256"]}"#;
const GRANT: &str = r#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://thalovant.com/activate","verification_uri_complete":"https://thalovant.com/activate?code=ABCD-EFGH","expires_in":900,"interval":0}"#;
const DEVICE_TOKEN: &str =
    r#"{"access_token":"tvpat_device","token_type":"bearer","scopes":[],"token_id":"d-1"}"#;

/// What the loopback API does with one request.
enum Reply {
    Answer(u16, String),
    /// Hold the connection and never answer.
    Stall,
    /// Close the connection without answering.
    HangUp,
}

type Route = Arc<dyn Fn(&str, &str, &str) -> Reply + Send + Sync>;

/// An API on loopback that answers each request as `route` says and keeps
/// the request lines it saw.
struct Api {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Api {
    async fn start(route: Route) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (log, kept) = (Arc::clone(&seen), Arc::clone(&bodies));
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle(
                    stream,
                    Arc::clone(&route),
                    Arc::clone(&log),
                    Arc::clone(&kept),
                ));
            }
        });
        Self {
            url,
            seen,
            bodies,
            server,
        }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("lock").clone()
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(
    mut stream: TcpStream,
    route: Route,
    seen: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
) {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut line = head.split("\r\n").next().unwrap_or_default().split(' ');
    let method = line.next().unwrap_or_default().to_string();
    let path = line.next().unwrap_or_default().to_string();
    let length: usize = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    while raw.len() < head_end + length {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => raw.extend_from_slice(&buffer[..read]),
        }
    }
    let body = String::from_utf8_lossy(&raw[head_end..head_end + length]).to_string();
    seen.lock().expect("lock").push(format!("{method} {path}"));
    bodies.lock().expect("lock").push(body.clone());
    match route(&method, &path, &body) {
        Reply::Answer(status, text) => {
            let answer = format!(
                "HTTP/1.1 {status} Answer\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = stream.write_all(answer.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
        Reply::Stall => tokio::time::sleep(Duration::from_secs(60)).await,
        Reply::HangUp => drop(stream),
    }
}

/// A control plane on `api` whose sign-in requests give up after 300 ms.
fn plane(api: &Api) -> ControlPlane {
    let mut control = ControlPlane::new(api.url.clone(), None);
    control.browser_request_timeout = Duration::from_millis(300);
    control
}

/// One tab, as a browser sends it, on its own connection; the answer's text.
async fn browse(port: u16, target: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the listener");
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await
        .expect("send");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    String::from_utf8_lossy(&raw).into_owned()
}

/// The port and the state the URL the SDK presented carries.
fn read_url(url: &str) -> (u16, String) {
    let query = url.split_once('?').expect("a query").1;
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let value = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .expect("a parameter")
    };
    let port = value("redirect_uri")
        .trim_start_matches("http://127.0.0.1:")
        .trim_end_matches("/callback")
        .parse()
        .expect("a port");
    (port, value("state"))
}

/// A browser sign-in on its own task, and the URL it presented.
async fn sign_in(
    control: ControlPlane,
) -> (
    String,
    tokio::task::JoinHandle<(ControlPlane, Result<Value>)>,
) {
    let (hand_over, mut opened) = mpsc::unbounded_channel();
    let mut control = control;
    let task = tokio::spawn(async move {
        let options = BrowserLoginOptions {
            open_browser: false,
            prompt: Some(Box::new(move |url: &str| {
                let _ = hand_over.send(url.to_string());
            })),
            ..Default::default()
        };
        let outcome = control.browser_login(options).await;
        (control, outcome)
    });
    let url = tokio::time::timeout(Duration::from_secs(5), opened.recv())
        .await
        .expect("a URL in time")
        .expect("a URL");
    (url, task)
}

fn shown(error: &ThalovantError) -> String {
    let mut shown = format!("{error}\n{error:?}\n{error:#?}");
    if let Some(problem) = error.api_problem() {
        shown.push_str(&Value::Object(problem.as_map().clone()).to_string());
    }
    shown
}

#[test]
fn scrubbing_is_one_pass_longest_first_and_leaves_its_marker_alone() {
    let hidden = ["abc", "abcdef", "c"];
    assert_eq!(
        scrub_text("xx-abc-abcdef-[redacted]", &hidden),
        "xx-[redacted]-[redacted]-[redacted]"
    );
    let once = scrub_value(json!({"seen:abcdef": ["c", {"deep": "abc"}]}), &hidden);
    assert_eq!(
        once,
        json!({"seen:[redacted]": ["[redacted]", {"deep": "[redacted]"}]})
    );
    assert_eq!(
        scrub_value(once.clone(), &hidden),
        once,
        "scrubbing twice changes nothing"
    );
    assert_eq!(scrub_text("nothing hidden", &[]), "nothing hidden");
    assert_eq!(scrub_text("été-abc", &["abc"]), "été-[redacted]");
}

#[test]
fn every_secret_reads_the_same_percent_encoded() {
    // The scrub matches a secret however a URL or form would encode it,
    // because nothing in one needs encoding: the verifier and the state are
    // base64url, all RFC 3986 unreserved.
    for _ in 0..200 {
        for secret in [
            crate::native_auth::new_verifier(),
            crate::native_auth::new_state(),
        ] {
            assert_eq!(urlencoding::encode(&secret), secret);
            let form: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
            assert_eq!(form, secret);
        }
    }
}

#[tokio::test]
async fn no_request_leaves_the_api_origin() {
    let collector = Api::start(Arc::new(|_: &str, _: &str, _: &str| {
        Reply::Answer(200, "{}".into())
    }))
    .await;
    let api = Api::start(Arc::new(|_: &str, _: &str, _: &str| {
        Reply::Answer(404, r#"{"detail":"Not Found"}"#.into())
    }))
    .await;
    let collector_host = collector.url.trim_start_matches("http://").to_string();
    let control = ControlPlane::new(api.url.clone(), Some("tvpat_must_stay_home".into()));
    for path in [
        "/https://elsewhere.example/v1/x".to_string(),
        "/elsewhere.example:443/v1/x".to_string(),
        format!("/{}/v1/x", collector.url),
        format!("/{collector_host}/v1/x"),
        "https://elsewhere.example/v1/x".to_string(),
    ] {
        // Appended to the API's address, every one of these is a path on it.
        let error = control
            .request("GET", &path, None, None, true)
            .await
            .expect_err("the API answers 404");
        assert_eq!(error.status_code(), Some(404), "{path}");
    }
    assert!(
        collector.seen().is_empty(),
        "a request left the API: {:?}",
        collector.seen()
    );
    // `api_url` is public: one set without its trailing slash is glued to a
    // path into another host. Refused before anything is sent.
    let mut glued = ControlPlane::new("http://localhost", Some("tvpat_must_stay_home".into()));
    glued.api_url = "http://localhost".to_string();
    let error = glued
        .request("GET", "host:9/v1/x", None, None, false)
        .await
        .expect_err("another origin");
    assert!(error.to_string().contains("origin"), "{error}");
    assert!(error.status_code().is_none() && !error.is_api_unreachable());
    assert!(!error.to_string().contains("tvpat_must_stay_home"));
    // An ordinary path still reaches the API.
    let ordinary = Api::start(Arc::new(|_: &str, _: &str, _: &str| {
        Reply::Answer(200, CONFIG.into())
    }))
    .await;
    let control = ControlPlane::new(ordinary.url.clone(), None);
    assert_eq!(
        control
            .native_sign_in_config()
            .await
            .expect("usable")
            .sdk_client_id,
        "thalovant-sdk"
    );
    assert_eq!(ordinary.seen(), vec!["GET /v1/auth/native/config"]);
}

#[tokio::test]
async fn a_stalled_configuration_is_bounded_and_auto_falls_back_to_device_login() {
    let api = Api::start(Arc::new(|method: &str, path: &str, _: &str| {
        match (method, path) {
            ("GET", "/v1/auth/native/config") => Reply::Stall,
            ("POST", "/v1/auth/device/authorize") => Reply::Answer(200, GRANT.into()),
            ("POST", "/v1/auth/device/token") => Reply::Answer(200, DEVICE_TOKEN.into()),
            _ => Reply::Answer(404, "{}".into()),
        }
    }))
    .await;
    let mut control = plane(&api);
    let started = std::time::Instant::now();
    let error = control
        .browser_login(BrowserLoginOptions {
            open_browser: false,
            prompt: Some(Box::new(|_: &str| {
                panic!("nothing is shown before a configuration")
            })),
            ..Default::default()
        })
        .await
        .expect_err("a stalled configuration");
    assert!(
        error.is_api_unreachable() && error.status_code().is_none(),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // Auto, on a machine with a browser: the stalled configuration is a
    // configuration it cannot use, and a device login stands in.
    let opener: BrowserOpener = Arc::new(|_: &str| {});
    let token = control
        .interactive_login(InteractiveLoginOptions {
            opener: Some(opener),
            environment: Some(HashMap::new()),
            platform: Some("macos".into()),
            ..Default::default()
        })
        .await
        .expect("a device login");
    assert_eq!(token["token_id"], "d-1");
    assert!(api
        .seen()
        .contains(&"POST /v1/auth/device/authorize".to_string()));
}

#[tokio::test]
async fn a_stalled_exchange_ends_the_sign_in_and_tells_the_tab() {
    let api = Api::start(Arc::new(|method: &str, _: &str, _: &str| match method {
        "GET" => Reply::Answer(200, CONFIG.into()),
        _ => Reply::Stall,
    }))
    .await;
    let (url, task) = sign_in(plane(&api)).await;
    let (port, state) = read_url(&url);
    let started = std::time::Instant::now();
    let page = browse(port, &format!("/callback?code=the-code&state={state}")).await;
    assert!(page.contains(crate::loopback::NOT_COMPLETED_TEXT));
    let (control, outcome) = task.await.expect("the sign-in task");
    let error = outcome.expect_err("the exchange timed out");
    assert!(
        error.is_api_unreachable() && error.status_code().is_none(),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(control.access_token.is_none());
    for secret in ["the-code", state.as_str()] {
        assert!(
            !shown(&error).contains(secret),
            "a secret reached the error"
        );
    }
    let posts = api
        .seen()
        .iter()
        .filter(|line| line.starts_with("POST"))
        .count();
    assert_eq!(posts, 1, "the exchange was sent once and never retried");
}

#[tokio::test]
async fn a_transport_failure_during_the_exchange_carries_nothing_of_the_request() {
    let api = Api::start(Arc::new(|method: &str, _: &str, _: &str| match method {
        "GET" => Reply::Answer(200, CONFIG.into()),
        _ => Reply::HangUp,
    }))
    .await;
    let (url, task) = sign_in(plane(&api)).await;
    let (port, state) = read_url(&url);
    browse(port, &format!("/callback?code=the-code&state={state}")).await;
    let (_, outcome) = task.await.expect("the sign-in task");
    let error = outcome.expect_err("the API hung up");
    let sent: Value =
        serde_json::from_str(api.bodies().last().expect("the exchange")).expect("JSON");
    let verifier = sent["code_verifier"]
        .as_str()
        .expect("a verifier")
        .to_string();
    for secret in ["the-code", verifier.as_str(), state.as_str()] {
        assert!(
            !shown(&error).contains(secret),
            "a secret reached the error"
        );
    }
    // Nothing rides along: the error has no cause to keep the request in.
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(
        api.seen()
            .iter()
            .filter(|line| line.starts_with("POST"))
            .count(),
        1,
        "never retried"
    );
}

#[tokio::test]
async fn a_message_cut_to_length_keeps_no_part_of_a_secret() {
    // The verifier starts ten characters before the display line's
    // 200-character cut, so a scrub after the cut could not find it whole.
    let api = Api::start(Arc::new(|method: &str, _: &str, body: &str| match method {
        "GET" => Reply::Answer(200, CONFIG.into()),
        _ => {
            let sent: Value = serde_json::from_str(body).expect("JSON");
            let verifier = sent["code_verifier"].as_str().unwrap_or_default();
            let detail = format!("{}{verifier}", "a".repeat(190));
            Reply::Answer(400, json!({ "detail": detail }).to_string())
        }
    }))
    .await;
    let (url, task) = sign_in(plane(&api)).await;
    let (port, state) = read_url(&url);
    browse(port, &format!("/callback?code=c&state={state}")).await;
    let (_, outcome) = task.await.expect("the sign-in task");
    let error = outcome.expect_err("refused");
    assert_eq!(error.status_code(), Some(400));
    let sent: Value =
        serde_json::from_str(api.bodies().last().expect("the exchange")).expect("JSON");
    let prefix: String = sent["code_verifier"]
        .as_str()
        .expect("a verifier")
        .chars()
        .take(10)
        .collect();
    assert!(
        !shown(&error).contains(&prefix),
        "part of the verifier reached the error"
    );
    assert_eq!(
        error.api_detail(),
        Some(format!("{}[redacted]", "a".repeat(190)).as_str())
    );
}

#[test]
fn a_secret_is_also_hidden_as_a_url_or_a_form_writes_it() {
    // A base64url secret -- every real verifier, state and code -- is only itself.
    assert_eq!(hidden_forms(&["Ab0-_"]), vec!["Ab0-_".to_string()]);
    // A form writes `~` as `%7E`, so that is hidden too.
    assert!(hidden_forms(&["a~b"]).contains(&"a%7Eb".to_string()));
    let forms = hidden_forms(&["a+b/c=", ""]);
    for written in ["a+b/c=", "a%2Bb%2Fc%3D", "a%2bb%2fc%3d"] {
        assert!(
            forms.contains(&written.to_string()),
            "{written} is not hidden: {forms:?}"
        );
    }
    assert_eq!(lowercase_escapes("Ab%2Bc%3D"), "Ab%2bc%3d");
}

#[tokio::test]
async fn an_echo_of_the_code_encoded_or_as_a_number_never_reaches_the_error() {
    for (code, query) in [("a+b/c=", "a%2Bb%2Fc%3D"), ("123456", "123456")] {
        let api = Api::start(Arc::new(
            move |method: &str, _: &str, body: &str| match method {
                "GET" => Reply::Answer(200, CONFIG.into()),
                _ => {
                    let sent: Value = serde_json::from_str(body).expect("JSON");
                    let code = sent["code"].as_str().unwrap_or_default().to_string();
                    let encoded = urlencoding::encode(&code).into_owned();
                    let mut problem = json!({
                        "detail": "invalid_grant",
                        "encoded": encoded,
                        "lower": lowercase_escapes(&encoded),
                        "unrelated": 7,
                    });
                    if let Ok(number) = code.parse::<u64>() {
                        problem["number"] = json!(number);
                    }
                    Reply::Answer(400, problem.to_string())
                }
            },
        ))
        .await;
        let (url, task) = sign_in(plane(&api)).await;
        let (port, state) = read_url(&url);
        browse(port, &format!("/callback?code={query}&state={state}")).await;
        let (_, outcome) = task.await.expect("the sign-in task");
        let error = outcome.expect_err("refused");
        assert_eq!(error.status_code(), Some(400));
        let shown = shown(&error);
        for written in hidden_forms(&[code]) {
            assert!(
                !shown.contains(&written),
                "an echo of the code reached the error"
            );
        }
        let problem = error.api_problem().expect("a problem");
        assert_eq!(
            problem.get("unrelated"),
            Some(&json!(7)),
            "other numbers stay"
        );
        if code == "123456" {
            assert_eq!(problem.get("number"), Some(&json!("[redacted]")));
        }
    }
}

#[tokio::test]
async fn an_oversized_answer_is_refused_without_reading_it_and_auto_falls_back() {
    let huge = format!("{{\"padding\":\"{}\"}}", "x".repeat(2 * 1024 * 1024));
    let api = Api::start(Arc::new(move |method: &str, path: &str, _: &str| {
        match (method, path) {
            ("GET", "/v1/auth/native/config") => Reply::Answer(200, huge.clone()),
            ("POST", "/v1/auth/device/authorize") => Reply::Answer(200, GRANT.into()),
            ("POST", "/v1/auth/device/token") => Reply::Answer(200, DEVICE_TOKEN.into()),
            _ => Reply::Answer(404, "{}".into()),
        }
    }))
    .await;
    let mut control = plane(&api);
    let error = control
        .browser_login(BrowserLoginOptions {
            open_browser: false,
            prompt: Some(Box::new(|_: &str| panic!("nothing is shown"))),
            ..Default::default()
        })
        .await
        .expect_err("an oversized configuration");
    assert!(matches!(&error, ThalovantError::Api(message) if message.contains("larger than")));
    assert!(error.status_code().is_none());
    let token = control
        .interactive_login(InteractiveLoginOptions {
            opener: Some(Arc::new(|_: &str| {})),
            environment: Some(HashMap::new()),
            platform: Some("windows".into()),
            ..Default::default()
        })
        .await
        .expect("a device login stands in");
    assert_eq!(token["token_id"], "d-1");
}

#[tokio::test]
async fn an_oversized_refusal_is_read_as_its_status_alone() {
    let huge = format!("{{\"detail\":\"{}\"}}", "x".repeat(2 * 1024 * 1024));
    let api = Api::start(Arc::new(
        move |method: &str, _: &str, _: &str| match method {
            "GET" => Reply::Answer(200, CONFIG.into()),
            _ => Reply::Answer(400, huge.clone()),
        },
    ))
    .await;
    let (url, task) = sign_in(plane(&api)).await;
    let (port, state) = read_url(&url);
    browse(port, &format!("/callback?code=c&state={state}")).await;
    let (_, outcome) = task.await.expect("the sign-in task");
    let error = outcome.expect_err("refused");
    assert_eq!(error.status_code(), Some(400));
    assert!(error.api_problem().is_none(), "the body was not read");
}
