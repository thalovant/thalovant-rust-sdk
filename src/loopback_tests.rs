//! The loopback listener against the `callbacks` section of
//! `tests/conformance/browser-login-vectors.json`, and the listener's own
//! edges.
//!
//! Colocated, as this SDK's private suites are: `classify_request` and
//! `LoopbackListener` are private to the crate, so this file is the private
//! `tests` module of `src/loopback.rs` and is recorded as test evidence
//! beside it. Every `callbacks` case runs through `classify_request` -- what
//! it produced is recorded under `callbacks/<name>` through the recorder the
//! integration tests use -- and again through a real listener on a real
//! socket. `tests/browser_login.rs` runs the other seven sections through the
//! public API.

// The recorder the integration tests use. `src/transport_noise_tests.rs`
// loads it into this test binary too; a second copy, rather than a path
// into that private module, keeps this suite apart from the transport.
#[allow(clippy::duplicate_mod)]
#[path = "../tests/common/mod.rs"]
mod common;

use super::*;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The state every `callbacks` case is run with.
const STATE: &str = "Xx0-_yLq7m3Vw9ZtR2cKp4HdN8sFjB6a";

fn browser_vectors() -> Value {
    let raw = std::fs::read_to_string("tests/conformance/browser-login-vectors.json")
        .expect("read the browser-login vectors");
    serde_json::from_str(&raw).expect("parse the browser-login vectors")
}

fn section(spec: &Value, name: &str) -> Vec<Value> {
    spec[name].as_array().cloned().expect("a section")
}

fn case_name(case: &Value) -> String {
    case["name"].as_str().expect("name").to_string()
}

fn outcome(status: u16, callback: Option<&Callback>) -> Value {
    match callback {
        None => json!({"status": status, "outcome": "ignored"}),
        Some(callback) => match callback.outcome {
            CallbackOutcome::Code => {
                json!({"status": status, "outcome": "code", "code": callback.code})
            }
            CallbackOutcome::Denied => json!({"status": status, "outcome": "denied"}),
            CallbackOutcome::Failed => {
                json!({"status": status, "outcome": "failed", "error": callback.error})
            }
        },
    }
}

struct Answer {
    status: u16,
    headers: Map<String, Value>,
    body: String,
}

/// One request on its own connection, as a browser tab sends it.
async fn send(port: u16, request: Vec<u8>) -> Answer {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the listener");
    stream.write_all(&request).await.expect("send the request");
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
        .map(|(name, value)| (name.trim().to_string(), Value::from(value.trim())))
        .collect();
    Answer {
        status,
        headers,
        body: body.to_string(),
    }
}

async fn get(port: u16, method: &str, target: &str) -> Answer {
    send(
        port,
        format!(
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
    )
    .await
}

/// Every answer carries the vectors' headers and a page that loads nothing.
fn check_page(answer: &Answer, spec: &Value) {
    for (name, value) in spec["page_headers"].as_object().expect("page_headers") {
        assert_eq!(answer.headers.get(name), Some(value), "{name}");
    }
    for loads in ["src=", "href=", "http"] {
        assert!(!answer.body.contains(loads), "the page loads something");
    }
}

fn refused(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
}

#[test]
fn callbacks_run_their_vectors() {
    let spec = browser_vectors();
    for case in section(&spec, "callbacks") {
        let request = &case["request"];
        let target = request["target"]
            .as_str()
            .expect("target")
            .replace("{state}", STATE);
        let (status, callback) =
            classify_request(request["method"].as_str().expect("method"), &target, STATE);
        let produced = outcome(status, callback.as_ref());
        // Recorded before the assert: the record is what this SDK produced.
        common::record(
            "browser-login-vectors.json",
            &format!("callbacks/{}", case_name(&case)),
            &produced,
        );
        assert_eq!(produced, case["expect"], "{}", case_name(&case));
    }
}

#[tokio::test]
async fn callbacks_over_a_real_listener_answer_as_the_rule_says() {
    let spec = browser_vectors();
    for case in section(&spec, "callbacks") {
        let mut listener = LoopbackListener::bind(STATE.to_string())
            .await
            .expect("bind");
        let port = listener.port();
        let request = &case["request"];
        let method = request["method"].as_str().expect("method").to_string();
        let target = request["target"]
            .as_str()
            .expect("target")
            .replace("{state}", STATE);
        let mut tab = tokio::spawn(async move { get(port, &method, &target).await });
        let mut answered = None;
        let accepted = tokio::select! {
            callback = listener.wait(Duration::from_secs(5)) => callback,
            answer = &mut tab => {
                answered = Some(answer.expect("the tab"));
                None
            }
        };
        if accepted.is_some() {
            // An accepted tab waits for the sign-in to end.
            listener.finish(false);
        }
        let answer = match answered {
            Some(answer) => answer,
            None => tab.await.expect("the tab"),
        };
        check_page(&answer, &spec);
        listener.close().await;
        assert_eq!(
            outcome(answer.status, accepted.as_ref()),
            case["expect"],
            "{}",
            case_name(&case)
        );
        if accepted.is_some() {
            assert!(answer.body.contains(NOT_COMPLETED_TEXT));
        }
        assert!(
            refused(port),
            "{}: the port is still open",
            case_name(&case)
        );
    }
}

#[test]
fn the_pages_headers_and_prompts_are_the_vectors() {
    let spec = browser_vectors();
    assert_eq!(spec["pages"]["signed_in"], SIGNED_IN_TEXT);
    assert_eq!(spec["pages"]["not_completed"], NOT_COMPLETED_TEXT);
    let headers: Map<String, Value> = PAGE_HEADERS
        .iter()
        .map(|(name, value)| (name.to_string(), Value::from(*value)))
        .collect();
    assert_eq!(spec["page_headers"], Value::Object(headers));
    let url = "https://dash.thalovant.com/authorize?response_type=code";
    for (key, open_browser) in [("opening", true), ("not_opening", false)] {
        let wanted = spec["prompt"][key]
            .as_str()
            .expect("prompt")
            .replace("{url}", url);
        assert_eq!(prompt_text(url, open_browser), wanted, "{key}");
    }
    assert_eq!(spec["redirect_uri"], "http://127.0.0.1:{port}/callback");
    assert_eq!(
        spec["redirect_uri"]
            .as_str()
            .expect("redirect_uri")
            .trim_start_matches("http://127.0.0.1:{port}"),
        LOOPBACK_CALLBACK_PATH
    );
}

#[tokio::test]
async fn the_listener_binds_ipv4_loopback_only() {
    let listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let address = listener.address;
    assert_eq!(address.ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(address.port() >= 1024, "an ephemeral port");
    let port = address.port();
    drop(listener);
    // Dropped is closed: a cancelled sign-in closes its port at once.
    assert!(refused(port));
}

#[tokio::test]
async fn an_oversized_head_or_one_that_is_not_a_request_is_400_and_the_wait_goes_on() {
    let spec = browser_vectors();
    let mut listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let port = listener.port();
    let tabs = tokio::spawn(async move {
        let huge = format!(
            "GET /callback?state={STATE} HTTP/1.1\r\nX-Padding: {}\r\n\r\n",
            "a".repeat(17 * 1024)
        );
        let oversized = send(port, huge.into_bytes()).await;
        let garbled = send(port, b"hello\r\n\r\n".to_vec()).await;
        let no_version = send(port, b"GET /callback\r\n\r\n".to_vec()).await;
        (oversized, garbled, no_version)
    });
    assert!(listener.wait(Duration::from_millis(400)).await.is_none());
    let (oversized, garbled, no_version) = tabs.await.expect("the tabs");
    for answer in [&oversized, &garbled, &no_version] {
        assert_eq!(answer.status, 400);
        check_page(answer, &spec);
    }
    listener.close().await;
}

#[tokio::test]
async fn an_answer_is_read_whole_although_the_browser_sent_more_than_was_read() {
    // Unread bytes at close would reset the connection, and a reset can
    // throw away the answer before it is read: on macOS it did, for the
    // oversized head above, until the listener drained what was left. Linux
    // keeps an answer that arrived readable after a reset, so this case only
    // fails there (the CI macOS job runs it).
    let spec = browser_vectors();
    let mut listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let port = listener.port();
    let tabs = tokio::spawn(async move {
        let mut answers = Vec::new();
        for size in [1, 64 * 1024, 300 * 1024] {
            let body = "b".repeat(size);
            let request = format!(
                "POST /callback?code=c&state={STATE} HTTP/1.1\r\nContent-Length: {size}\r\n\r\n{body}"
            );
            answers.push(send(port, request.into_bytes()).await);
        }
        let mut huge = format!("GET /callback?state={STATE} HTTP/1.1\r\nX-Padding: ").into_bytes();
        huge.extend(std::iter::repeat_n(b'a', 200 * 1024));
        huge.extend_from_slice(b"\r\n\r\n");
        answers.push(send(port, huge).await);
        answers
    });
    let answers = tokio::select! {
        accepted = listener.wait(Duration::from_secs(10)) => {
            panic!("no request here may be accepted: {accepted:?}")
        }
        answers = tabs => answers.expect("the tabs"),
    };
    assert_eq!(
        answers
            .iter()
            .map(|answer| answer.status)
            .collect::<Vec<_>>(),
        vec![404, 404, 404, 400]
    );
    for answer in &answers {
        check_page(answer, &spec);
    }
    listener.close().await;
}

#[tokio::test]
async fn a_connection_that_sends_nothing_does_not_hold_the_close() {
    let mut listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let port = listener.port();
    let idle = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    // Accepted, and its task left reading a head that never comes.
    assert!(listener.wait(Duration::from_millis(200)).await.is_none());
    let started = std::time::Instant::now();
    listener.close().await;
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "closing waited on a connection that never sent anything"
    );
    drop(idle);
}

#[tokio::test]
async fn a_second_answer_after_the_first_is_turned_away_and_the_port_closes() {
    let spec = browser_vectors();
    let mut listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let port = listener.port();
    // A tab that connected early and answers late.
    let mut early = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let first = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        get(port, "GET", &format!("/callback?code=first&state={STATE}")).await
    });
    let callback = listener
        .wait(Duration::from_secs(5))
        .await
        .expect("the first answer is accepted");
    assert_eq!(callback.outcome, CallbackOutcome::Code);
    assert_eq!(callback.code.as_deref(), Some("first"));
    // No new connection once a callback is accepted.
    assert!(
        refused(port),
        "the port still listens after a callback was accepted"
    );
    // The early tab's answer, with the right state, is not this sign-in's any more.
    early
        .write_all(
            format!("GET /callback?code=second&state={STATE} HTTP/1.1\r\nHost: x\r\n\r\n")
                .as_bytes(),
        )
        .await
        .expect("send");
    let mut raw = Vec::new();
    early.read_to_end(&mut raw).await.expect("read");
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 400 "));
    listener.finish(true);
    let answer = first.await.expect("the first tab");
    assert_eq!(answer.status, 200);
    assert!(answer.body.contains(SIGNED_IN_TEXT));
    check_page(&answer, &spec);
    listener.close().await;
}

#[tokio::test]
async fn the_wait_is_never_cut_short() {
    let mut listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let started = std::time::Instant::now();
    assert!(listener.wait(Duration::from_millis(250)).await.is_none());
    assert!(started.elapsed() >= Duration::from_millis(250));
    // A wrong-state tab neither ends the wait nor restarts it.
    let port = listener.port();
    let stray = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        get(port, "GET", "/callback?code=x&state=an-old-attempt").await
    });
    let started = std::time::Instant::now();
    assert!(listener.wait(Duration::from_millis(400)).await.is_none());
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(400) && waited < Duration::from_millis(1500));
    assert_eq!(stray.await.expect("the stray tab").status, 400);
    listener.close().await;
}

#[tokio::test]
async fn a_tab_accepted_as_the_wait_ends_is_still_answered() {
    let mut listener = LoopbackListener::bind(STATE.to_string())
        .await
        .expect("bind");
    let port = listener.port();
    // Accepted by the listener, but its head arrives after the wait gave up.
    let mut late = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    assert!(listener.wait(Duration::from_millis(150)).await.is_none());
    late.write_all(format!("GET /callback?code=c&state={STATE} HTTP/1.1\r\n\r\n").as_bytes())
        .await
        .expect("send");
    // Until its connection has taken the one acceptance there is.
    let taken = async {
        while listener.shared.accept.lock().expect("lock").is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), taken)
        .await
        .expect("the late tab was read");
    listener.close().await;
    let mut raw = Vec::new();
    late.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200 "),
        "{}",
        text.lines().next().unwrap_or_default()
    );
    assert!(text.contains(NOT_COMPLETED_TEXT));
}

#[test]
fn a_callback_never_prints_its_code() {
    let (_, callback) = classify_request(
        "GET",
        &format!("/callback?code=secret-code&state={STATE}"),
        STATE,
    );
    let callback = callback.expect("accepted");
    let printed = format!("{callback:?} {callback:#?}");
    assert!(!printed.contains("secret-code"));
    assert!(printed.contains("Code"));
}

#[test]
fn every_answer_is_a_whole_http_answer() {
    for (status, reason) in [(200, "OK"), (400, "Bad Request"), (404, "Not Found")] {
        let raw = String::from_utf8(response_bytes(status, "x")).expect("ASCII");
        let (head, body) = raw.split_once("\r\n\r\n").expect("a head");
        assert!(head.starts_with(&format!("HTTP/1.1 {status} {reason}\r\n")));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert!(head.ends_with("Connection: close"));
        assert!(body.contains("<p>x</p>"));
    }
}
