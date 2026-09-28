//! Keeping a hub link up, against `link-keeping-vectors.json`.
//!
//! `close` cases hold the transport's reading of a close to the vectors
//! through the pure rule it uses, `close_refuses`. `supervise` cases drive the
//! `LinkSupervisor` that `HubSession::run` asks after every attempt.
//! `handshake` cases run a real Noise handshake against a loopback hub, so
//! they live beside the transport's own Noise fixtures, in
//! `src/transport_noise_tests.rs`, and record into the same file. The vector
//! file is vendored byte for byte from the Python reference, and what is
//! recorded has exactly the shape the reference's
//! `tests/test_link_keeping_vectors.py` records.

use std::time::Duration;

use serde_json::{json, Value};
use thalovant::{
    close_refuses, HubSessionPolicy, LinkDecision, LinkOutcome, LinkSupervisor, CLOSE_CODE_GRACE,
    DEFAULT_REFUSAL_GRACE, DEFAULT_SETTLE_WINDOW, REFUSAL_CLOSE_CODES, REFUSAL_SETTLE,
};

mod common;

fn vectors(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("tests/conformance/{name}"))
        .unwrap_or_else(|error| panic!("read {name}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("parse {name}: {error}"))
}

fn millis(value: &Value) -> Duration {
    Duration::from_millis(value.as_u64().expect("whole milliseconds"))
}

fn cases<'a>(spec: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    spec["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(move |case| case["kind"] == kind)
}

fn name(case: &Value) -> &str {
    case["name"].as_str().expect("name")
}

#[test]
fn the_policy_is_the_sdks() {
    let spec = vectors("link-keeping-vectors.json");
    let policy = &spec["policy"];
    let defaults = HubSessionPolicy::default();
    assert_eq!(defaults.retry, millis(&policy["retry_ms"]));
    assert_eq!(defaults.retry_ceiling, millis(&policy["retry_ceiling_ms"]));
    assert_eq!(defaults.probe, millis(&policy["probe_ms"]));
    assert_eq!(defaults.probe_down, millis(&policy["probe_down_ms"]));
    assert_eq!(DEFAULT_REFUSAL_GRACE, millis(&policy["refusal_grace_ms"]));
    assert_eq!(REFUSAL_SETTLE, millis(&policy["settle_ms"]));
    assert_eq!(DEFAULT_SETTLE_WINDOW, millis(&policy["settle_ms"]));
    assert_eq!(CLOSE_CODE_GRACE, millis(&policy["close_code_grace_ms"]));
    assert_eq!(json!(REFUSAL_CLOSE_CODES), policy["refusal_close_codes"]);
}

#[test]
fn a_close_is_read_as_its_vector_says() {
    let spec = vectors("link-keeping-vectors.json");
    for case in cases(&spec, "close") {
        let after = (case["when"] == "after_handshake").then(|| millis(&case["after_ms"]));
        let late = case.get("code_late_ms").map(millis).unwrap_or_default();
        let code = case["code"].as_u64().map(|code| code as u16);
        let refused = close_refuses(code, after, late);
        let produced = json!({"outcome": if refused { "refused" } else { "dropped" }});
        // Recorded before the assert: the record is what this SDK produced.
        common::record("link-keeping-vectors.json", name(case), &produced);
        assert_eq!(produced, case["expect"], "{}", name(case));
    }
}

fn outcome(name: &str) -> LinkOutcome {
    match name {
        "up" => LinkOutcome::Up,
        "dropped" => LinkOutcome::Dropped,
        "failed" => LinkOutcome::Failed,
        "refused" => LinkOutcome::Refused,
        "key_changed" => LinkOutcome::KeyChanged,
        other => panic!("unknown outcome {other}"),
    }
}

#[test]
fn the_supervisor_decides_as_its_vector_says() {
    let spec = vectors("link-keeping-vectors.json");
    let policy = &spec["policy"];
    for case in cases(&spec, "supervise") {
        let mut supervisor = LinkSupervisor::new(
            HubSessionPolicy {
                retry: millis(&policy["retry_ms"]),
                retry_ceiling: millis(&policy["retry_ceiling_ms"]),
                probe: millis(&policy["probe_ms"]),
                probe_down: millis(&policy["probe_down_ms"]),
            },
            millis(&policy["refusal_grace_ms"]),
        );
        let produced: Vec<Value> = case["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|event| {
                let decision = supervisor.after(
                    outcome(event["outcome"].as_str().expect("outcome")),
                    millis(&event["at_ms"]),
                );
                match decision {
                    LinkDecision::Retry { wait } => {
                        json!({"action": "retry", "wait_ms": wait.as_millis() as u64})
                    }
                    LinkDecision::GiveUp { reason } => json!({
                        "action": "give_up",
                        "reason": match reason {
                            LinkOutcome::Refused => "refused",
                            LinkOutcome::KeyChanged => "key_changed",
                            other => panic!("gave up for {other:?}"),
                        },
                    }),
                    LinkDecision::Hold => json!({"action": "hold"}),
                    other => panic!("unexpected {other:?}"),
                }
            })
            .collect();
        let produced = Value::Array(produced);
        common::record("link-keeping-vectors.json", name(case), &produced);
        assert_eq!(produced, case["expect"], "{}", name(case));
    }
}
