//! Home bridges (`connection_type: connector`) and the refusals that came with
//! them, against a real loopback HTTP peer.
//!
//! Written against the API's `Idempotent-Replayed` header, `replay` marker,
//! `serves` block and the problem codes of `connection_problem_code`. Each test
//! also holds for an API that does not have them yet. There are no shared
//! vectors for this capability: these cases mirror the Python reference's
//! `tests/test_connector_connections.py`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Map, Value};
use thalovant::connection_problem_code as code;
use thalovant::{
    ApiRefusal, BootstrapIdentityOptions, ControlPlane, ThalovantError, CONNECTION_TYPE_CONNECTOR,
    CONNECTION_TYPE_HOME_ASSISTANT,
};

fn hub() -> Value {
    json!({"id": "hub-1", "name": "Home", "domain": "home.example.com", "wss_enabled": true, "http_enabled": true})
}

struct Answer {
    status: &'static str,
    body: Value,
    headers: Vec<(&'static str, &'static str)>,
}

fn ok(body: Value) -> Answer {
    Answer {
        status: "201 Created",
        body,
        headers: vec![],
    }
}

fn refusal(status: &'static str, body: Value) -> Answer {
    Answer {
        status,
        body,
        headers: vec![],
    }
}

fn problem(status: &'static str, code: &str, extra: Value) -> Answer {
    let mut body = json!({"detail": "refused", "code": code});
    for (key, value) in extra.as_object().expect("object") {
        body[key] = value.clone();
    }
    refusal(status, body)
}

fn created(connection_type: &str, extra: Value) -> Value {
    let mut body = json!({
        "id": "client-1",
        "name": "Bridge",
        "hub_id": "hub-1",
        "spec": {"version": "1", "connection_type": connection_type},
        "etag": "e1",
        "initial_identify": {
            "password": "platform-password",
            "access_key": "platform-key",
            "site_id": "bridge",
            "default_port": 443,
            "default_master": "wss://home.example.com"
        }
    });
    for (key, value) in extra.as_object().expect("object") {
        body[key] = value.clone();
    }
    body
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let size = stream.read(&mut chunk).expect("read request");
        assert!(size > 0, "request closed early");
        raw.extend_from_slice(&chunk[..size]);
        if let Some(end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let head = std::str::from_utf8(&raw[..end]).expect("UTF-8 head");
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, length)| length.trim().parse::<usize>().expect("length"))
                .unwrap_or(0);
            if raw.len() >= end + 4 + length {
                return String::from_utf8(raw).expect("UTF-8 request");
            }
        }
    }
}

/// A loopback API that answers every request with `answer`, recording them.
fn serve(answer: Answer) -> (ControlPlane, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let request = read_request(&mut stream);
            log.lock().unwrap().push(request);
            let body = answer.body.to_string();
            let extra: String = answer
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let head = format!(
                "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n",
                answer.status,
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (
        ControlPlane::new(format!("http://{address}"), Some("tok".into())),
        seen,
    )
}

fn options(name: &str) -> BootstrapIdentityOptions {
    BootstrapIdentityOptions {
        name: name.to_string(),
        site_id: None,
        spec: Map::new(),
        owner_id: None,
        active: None,
        preferred_protocols: Vec::new(),
        idempotency_key: None,
    }
}

fn sent_body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1).expect("JSON")
}

#[test]
fn the_kind_constants_are_the_wire_values() {
    assert_eq!(CONNECTION_TYPE_CONNECTOR, "connector");
    assert_eq!(CONNECTION_TYPE_HOME_ASSISTANT, "home_assistant");
}

#[tokio::test]
async fn a_bridge_is_created_with_its_connector_list_and_the_platforms_credentials() {
    let serves = json!({
        "devices": [{"id": "d1", "name": "Kitchen", "connection_type": "voice_satellite", "active": true}],
        "count": 1,
        "warning": null
    });
    let (api, seen) = serve(ok(created("connector", json!({"serves": serves}))));
    let mut opts = options("Bridge");
    opts.spec
        .insert("connectors".into(), json!(["music_assistant"]));
    let result = api
        .create_client_identity_of_type(hub(), CONNECTION_TYPE_CONNECTOR, opts)
        .await
        .expect("created");
    let sent = sent_body(&seen.lock().unwrap()[0]);
    assert_eq!(sent["spec"]["connection_type"], "connector");
    assert_eq!(sent["spec"]["connectors"], json!(["music_assistant"]));
    // The answer's credentials are the connection's; the ones minted locally are not.
    assert_eq!(result.identity.access_key, "platform-key");
    assert_eq!(result.identity.password, "platform-password");
    assert_eq!(result.connection_type(), Some("connector"));
    assert!(!result.replayed());
    assert_eq!(result.serves(), serves.as_object());
    assert_eq!(result.serves_warning(), None);
}

#[tokio::test]
async fn a_link_that_serves_nobody_says_so() {
    let serves =
        json!({"devices": [], "count": 0, "warning": "No device of your account is on this hub."});
    let (api, _) = serve(ok(created("home_assistant", json!({"serves": serves}))));
    let result = api
        .create_client_identity_of_type(hub(), "home_assistant", options("HA"))
        .await
        .expect("created");
    assert_eq!(
        result.serves_warning(),
        Some("No device of your account is on this hub.")
    );
}

#[tokio::test]
async fn an_api_without_serves_still_works() {
    let (api, _) = serve(ok(created("connector", json!({}))));
    let result = api
        .create_client_identity_of_type(hub(), "connector", options("Bridge"))
        .await
        .expect("created");
    assert!(result.serves().is_none());
    assert!(result.serves_warning().is_none());
}

fn replayed_answers() -> Vec<(&'static str, Answer)> {
    let mut body = created("connector", json!({}));
    body.as_object_mut().unwrap().remove("initial_identify");
    let mut marked = body.clone();
    marked["replay"] =
        json!({"code": "idempotent_replay", "credentials_included": false, "detail": "x"});
    vec![
        (
            "header",
            Answer {
                status: "201 Created",
                body,
                headers: vec![("Idempotent-Replayed", "true")],
            },
        ),
        ("marker", ok(marked)),
    ]
}

#[tokio::test]
async fn a_replayed_create_never_hands_back_an_identity_the_sdk_made_up() {
    for (name, answer) in replayed_answers() {
        let (api, _) = serve(answer);
        let mut opts = options("Bridge");
        opts.idempotency_key = Some("k1".into());
        let error = api
            .create_client_identity_of_type(hub(), "connector", opts)
            .await
            .expect_err(name);
        assert!(
            matches!(error, ThalovantError::CredentialsNotReplayed { .. }),
            "{name}: {error:?}"
        );
        assert_eq!(error.replayed_client_id(), Some("client-1"), "{name}");
        let client = error.replayed_client().expect("client");
        assert!(client.get("initial_identify").is_none(), "{name}");
    }
}

#[tokio::test]
async fn a_replay_scrubs_every_credential_the_resource_carries() {
    let mut body = created(
        "connector",
        json!({"spec": {"connection_type": "connector", "apiKey": "k", "password": "p", "apiKeyRef": "vault/x"}}),
    );
    body["initial_identify_token"] = json!("t");
    let (api, _) = serve(Answer {
        status: "201 Created",
        body,
        headers: vec![("idempotent-replayed", "TRUE")],
    });
    let error = api
        .create_client_identity(hub(), options("Bridge"))
        .await
        .expect_err("replay");
    let client = error.replayed_client().expect("client");
    assert!(client.get("initial_identify").is_none());
    assert!(client.get("initial_identify_token").is_none());
    assert!(client["spec"].get("apiKey").is_none());
    assert!(client["spec"].get("password").is_none());
    assert_eq!(client["spec"]["apiKeyRef"], "vault/x");
}

#[tokio::test]
async fn an_answer_without_credentials_that_is_not_a_replay_keeps_the_old_behaviour() {
    let mut body = created("connector", json!({}));
    body.as_object_mut().unwrap().remove("initial_identify");
    let (api, _) = serve(ok(body));
    let result = api
        .create_client_identity_of_type(hub(), "connector", options("Bridge"))
        .await
        .expect("created");
    assert!(!result.identity.access_key.is_empty());
}

#[tokio::test]
async fn the_intercom_spelling_of_the_replay_header_is_not_a_replay() {
    let (api, _) = serve(Answer {
        status: "201 Created",
        body: created("connector", json!({})),
        headers: vec![("Idempotency-Replayed", "true")],
    });
    api.create_client_identity_of_type(hub(), "connector", options("Bridge"))
        .await
        .expect("created");
}

#[tokio::test]
async fn a_link_on_another_accounts_hub_names_the_reason() {
    let (api, seen) = serve(problem(
        "422 Unprocessable Entity",
        code::CONNECTION_TYPE_UNAVAILABLE,
        json!({"reason": "hub_operated_by_another_account"}),
    ));
    let error = api
        .create_client_identity_of_type(hub(), "home_assistant", options("HA"))
        .await
        .expect_err("refused");
    // Still a kind of the unsupported-kind error, so existing handlers keep working.
    assert!(
        matches!(error, ThalovantError::UnsupportedConnectionType { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.api_refusal(),
        Some(ApiRefusal::ConnectionTypeUnavailable)
    );
    assert_eq!(
        error.connection_unavailable_reason(),
        Some("hub_operated_by_another_account")
    );
    assert_eq!(error.api_code(), Some(code::CONNECTION_TYPE_UNAVAILABLE));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "nothing to delete: the API made nothing"
    );
}

#[tokio::test]
async fn an_unknown_or_ambiguous_kind_is_unsupported() {
    for refused in [
        code::CONNECTION_TYPE_UNKNOWN,
        code::CONNECTION_TYPE_AMBIGUOUS,
    ] {
        let (api, _) = serve(problem(
            "422 Unprocessable Entity",
            refused,
            json!({"known": ["voice_satellite", "connector"]}),
        ));
        let error = api
            .create_client_identity_of_type(hub(), "plex", options("X"))
            .await
            .expect_err(refused);
        assert!(
            matches!(error, ThalovantError::UnsupportedConnectionType { .. }),
            "{refused}: {error:?}"
        );
        assert_eq!(error.api_code(), Some(refused));
        // `known` stays in the problem.
        assert!(error.api_problem().unwrap().get("known").is_some());
    }
}

#[tokio::test]
async fn a_refused_connector_list_says_why_and_what_is_known() {
    for reason in ["duplicate", "own_kind", "unknown_or_malformed"] {
        let (api, _) = serve(problem(
            "422 Unprocessable Entity",
            code::CONNECTORS_INVALID,
            json!({"reason": reason, "known": ["music_assistant"]}),
        ));
        let mut opts = options("Bridge");
        opts.spec.insert(
            "connectors".into(),
            json!(["music_assistant", "music_assistant"]),
        );
        let error = api
            .create_client_identity_of_type(hub(), "connector", opts)
            .await
            .expect_err(reason);
        assert_eq!(error.api_refusal(), Some(ApiRefusal::ConnectorsInvalid));
        assert_eq!(error.connectors_invalid_reason(), Some(reason));
        assert_eq!(error.connectors_known(), vec!["music_assistant"]);
    }
}

#[tokio::test]
async fn a_connector_refusal_that_mentions_the_kind_is_not_retyped() {
    let (api, _) = serve(refusal(
        "422 Unprocessable Entity",
        json!({"detail": "connectors invalid for connection_type connector", "code": "connectors_invalid", "reason": "duplicate"}),
    ));
    let error = api
        .create_client_identity_of_type(hub(), "connector", options("Bridge"))
        .await
        .expect_err("refused");
    assert!(
        matches!(error, ThalovantError::ApiResponse { .. }),
        "{error:?}"
    );
    assert_eq!(error.connectors_invalid_reason(), Some("duplicate"));
}

#[tokio::test]
async fn an_owner_only_refusal_names_the_link() {
    for refused in [
        code::DEVICE_OWNER_ONLY,
        code::SETUP_SESSION_OWNER_ONLY,
        code::DEVICE_IDENTITY_OWNER_ONLY,
    ] {
        let (api, _) = serve(problem(
            "403 Forbidden",
            refused,
            json!({"link_client_id": "client-9"}),
        ));
        let mut opts = options("Speaker");
        opts.owner_id = Some("someone".into());
        let error = api
            .create_client_identity(hub(), opts)
            .await
            .expect_err(refused);
        assert_eq!(error.api_refusal(), Some(ApiRefusal::OwnerOnlyBesideLink));
        assert_eq!(error.link_client_id(), Some("client-9"));
        assert_eq!(error.status_code(), Some(403));
    }
}

#[tokio::test]
async fn the_other_new_codes_are_typed() {
    let (api, _) = serve(problem(
        "409 Conflict",
        code::HUB_SCALE_OUT_BREAKS_LINKS,
        json!({"links": 2}),
    ));
    let error = api
        .create_client_identity(hub(), options("X"))
        .await
        .expect_err("scale");
    assert_eq!(
        error.api_refusal(),
        Some(ApiRefusal::HubScaleOutBreaksLinks)
    );
    assert_eq!(error.hub_links(), Some(2));

    let (api, _) = serve(problem(
        "409 Conflict",
        code::IDEMPOTENCY_KEY_SPENT,
        json!({}),
    ));
    let error = api
        .create_client_identity(hub(), options("X"))
        .await
        .expect_err("spent");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::IdempotencyKeySpent));

    let (api, _) = serve(problem(
        "422 Unprocessable Entity",
        code::SKILL_CONFIG_PRIVATE_HUB_ONLY,
        json!({"settings": ["url", "token"]}),
    ));
    let error = api
        .create_client_identity(hub(), options("X"))
        .await
        .expect_err("private");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::PrivateHubOnly));
    assert_eq!(error.private_hub_settings(), vec!["url", "token"]);
}

#[tokio::test]
async fn a_structured_refusal_inside_detail_is_read_too() {
    let (api, _) = serve(refusal(
        "422 Unprocessable Entity",
        json!({"detail": {"code": "connectors_invalid", "reason": "duplicate", "known": ["music_assistant"]}}),
    ));
    let error = api
        .create_client_identity_of_type(hub(), "connector", options("Bridge"))
        .await
        .expect_err("refused");
    assert_eq!(error.api_refusal(), Some(ApiRefusal::ConnectorsInvalid));
    assert_eq!(error.connectors_invalid_reason(), Some("duplicate"));
}

#[tokio::test]
async fn an_unlisted_code_is_an_ordinary_api_error() {
    let (api, _) = serve(problem("409 Conflict", "something_new", json!({})));
    let error = api
        .create_client_identity(hub(), options("X"))
        .await
        .expect_err("refused");
    assert!(matches!(error, ThalovantError::ApiResponse { .. }));
    assert_eq!(error.api_refusal(), None);
}
