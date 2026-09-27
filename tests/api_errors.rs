//! What a control-plane error carries, against the shared vectors.
//!
//! `tests/conformance/api-error-vectors.json` is vendored byte for byte from
//! the Python reference, so every SDK runs the same cases. The API answers a
//! refusal with a Problem+JSON body whose structured fields say what to do
//! next -- the images a caller may pin instead, the plan's numbers -- and a
//! display line cut at 200 characters is not where anybody can read them.
//! Each case is served by a real loopback HTTP peer and read back through
//! `ControlPlane::get_hub`, so what is recorded is what a caller gets.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use serde_json::{json, Map, Value};
use thalovant::{ApiProblem, ControlPlane, ThalovantError};

mod common;

fn vectors(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("tests/conformance/{name}"))
        .unwrap_or_else(|error| panic!("read {name}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("parse {name}: {error}"))
}

/// A loopback API that answers one `GET /v1/hubs/hub-1` with `response`:
/// its status, its Content-Type and its body, byte for byte.
fn answering(response: &Value) -> (String, thread::JoinHandle<()>) {
    let status = response["status"].as_u64().expect("status") as u16;
    let content_type = response["content_type"]
        .as_str()
        .expect("content_type")
        .to_string();
    let body = response["body"].as_str().expect("body").as_bytes().to_vec();
    let reason = reqwest::StatusCode::from_u16(status)
        .expect("a valid status")
        .canonical_reason()
        .unwrap_or("Status");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback peer");
    let address = listener.local_addr().expect("peer address");
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let size = stream.read(&mut buffer).expect("read request");
            assert!(size > 0, "the client closed before sending its request");
            request.extend_from_slice(&buffer[..size]);
        }
        let request = String::from_utf8_lossy(&request);
        assert!(
            request.starts_with("GET /v1/hubs/hub-1 "),
            "unexpected request: {request}"
        );
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("write head");
        stream.write_all(&body).expect("write body");
    });
    (format!("http://{address}"), peer)
}

async fn refusal(response: &Value) -> ThalovantError {
    let (url, peer) = answering(response);
    let error = ControlPlane::new(url, Some("synthetic-token".to_string()))
        .get_hub("hub-1")
        .await
        .expect_err("the API refused, so the call fails");
    peer.join().expect("loopback peer");
    error
}

fn produced(error: &ThalovantError) -> Value {
    json!({
        "status": error.status_code(),
        "code": error.api_code(),
        "detail": error.api_detail(),
        "problem": error
            .api_problem()
            .map(|problem| Value::Object(problem.as_map().clone())),
    })
}

#[tokio::test]
async fn an_api_error_carries_what_its_vector_names() {
    let spec = vectors("api-error-vectors.json");
    for case in spec["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let error = refusal(&case["response"]).await;
        let produced = produced(&error);
        // Recorded before the assert: the record is what this SDK produced,
        // not a restatement of what the vector says it should have.
        common::record("api-error-vectors.json", name, &produced);
        assert_eq!(produced, case["expect"], "{name}");
        assert!(
            matches!(error, ThalovantError::ApiResponse { .. }),
            "{name}"
        );
        for echoed in case["message_excludes"].as_array().into_iter().flatten() {
            let echoed = echoed.as_str().expect("a string");
            // The display line, and what `{:?}` and an `unwrap()` panic print.
            for form in [
                error.to_string(),
                format!("{error:?}"),
                format!("{error:#?}"),
            ] {
                assert!(!form.contains(echoed), "{name}: {echoed} leaked: {form}");
            }
        }
    }
}

#[tokio::test]
async fn the_message_may_be_shortened_but_the_detail_never_is() {
    let spec = vectors("api-error-vectors.json");
    let case = spec["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["expect"]["code"] == "platform_image_required")
        .expect("an image refusal case");
    let error = refusal(&case["response"]).await;
    let sentence = case["expect"]["detail"].as_str().expect("detail");

    // The display line is what it always was: one line, cut at 200.
    let message = error.to_string();
    let shown = message
        .strip_prefix("api error: HTTP 403: ")
        .unwrap_or_else(|| panic!("unexpected message: {message}"));
    assert!(shown.ends_with("..."), "{message}");
    assert_eq!(shown.chars().count(), 203, "{message}");
    assert!(!message.contains(sentence), "{message}");

    // The sentence the API wrote is whole, and every list it sent is there.
    assert_eq!(error.api_detail(), Some(sentence));
    assert!(sentence.chars().count() > 200);
    assert_eq!(error.api_code(), Some("platform_image_required"));
    let problem = error.api_problem().expect("the body was a JSON object");
    assert_eq!(
        problem["allowed_images"]["core"],
        json!([
            "ghcr.io/thalovant/ovos-core:2026.09.2",
            "ghcr.io/thalovant/ovos-core:2026.09.3-alpha.1",
        ])
    );
    assert_eq!(
        problem.get("allowed_repositories"),
        Some(&json!({"core": "ghcr.io/thalovant/ovos-core"}))
    );
    assert_eq!(
        problem["refused_images"]["bus"],
        "docker.io/example/ovos-messagebus:custom"
    );
}

#[test]
fn an_error_built_the_old_way_still_reads_the_old_way() {
    let error = ThalovantError::ApiResponse {
        status_code: 412,
        detail: "ETag mismatch".into(),
        problem: None,
    };
    assert_eq!(error.to_string(), "api error: HTTP 412: ETag mismatch");
    assert_eq!(error.status_code(), Some(412));
    assert!(error.api_problem().is_none());
    assert_eq!((error.api_code(), error.api_detail()), (None, None));

    let error = ThalovantError::Api("missing access token".into());
    assert_eq!(error.to_string(), "api error: missing access token");
    assert_eq!(error.status_code(), None);
    assert!(error.api_problem().is_none());
    assert_eq!((error.api_code(), error.api_detail()), (None, None));
}

#[test]
fn a_problem_reads_as_a_map_and_debug_never_prints_a_secret() {
    let body = json!({
        "detail": {"detail": "Free plan allows up to 1 connection.", "code": "plan_limit"},
        "limit": 1,
        "errors": [{"input": {"password": "pw-DEBUG-SECRET"}}],
    });
    let map: Map<String, Value> = body.as_object().expect("object").clone();
    let problem = ApiProblem::from(map.clone());
    assert_eq!(problem.code(), Some("plan_limit"));
    assert_eq!(
        problem.detail(),
        Some("Free plan allows up to 1 connection.")
    );
    assert_eq!(problem.get("limit"), Some(&json!(1)));
    assert_eq!(problem.len(), 3, "Deref reaches the map");
    assert_eq!(problem.as_map(), &map);
    // The body is kept as sent: a caller reading the map gets what the API said.
    assert_eq!(
        problem["errors"][0]["input"]["password"],
        json!("pw-DEBUG-SECRET")
    );

    let error = ThalovantError::ApiResponse {
        status_code: 403,
        detail: "refused".into(),
        problem: Some(Box::new(problem.clone())),
    };
    assert_eq!(error.api_code(), Some("plan_limit"));
    assert_eq!(error.api_problem(), Some(&problem));
    for form in [
        format!("{error:?}"),
        format!("{error:#?}"),
        error.to_string(),
    ] {
        assert!(!form.contains("pw-DEBUG-SECRET"), "leaked: {form}");
    }
    assert!(format!("{problem:?}").contains("plan_limit"));
    assert_eq!(problem.into_map(), map);
}

#[test]
fn the_vectors_cover_every_shape_the_rules_name() {
    // A vector set that quietly lost its non-JSON or its nested case would
    // still pass.
    let spec = vectors("api-error-vectors.json");
    let cases = spec["cases"].as_array().expect("cases");
    let expects: Vec<&Value> = cases.iter().map(|case| &case["expect"]).collect();
    let detail = |expect: &Value| expect["detail"].as_str().unwrap_or("").to_string();
    assert!(expects.iter().any(|e| e["problem"].is_null()));
    assert!(expects
        .iter()
        .any(|e| e["code"].is_string() && e["detail"].is_null()));
    assert!(expects
        .iter()
        .any(|e| e["detail"].is_string() && e["code"].is_null()));
    assert!(expects.iter().any(|e| e["problem"]["detail"].is_object()));
    assert!(expects.iter().any(|e| e["problem"]["detail"].is_array()));
    assert!(
        expects.iter().any(|e| detail(e).chars().count() > 256),
        "no detail longer than any SDK's message limit"
    );
    assert!(expects.iter().any(|e| detail(e).contains('\n')));
    assert!(cases.iter().any(|case| case["message_excludes"].is_array()));
}
