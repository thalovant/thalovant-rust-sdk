//! What an ask does when the hub refuses it or cannot answer it.
//!
//! The hub sends `hive.policy.denied` the instant it refuses, built with
//! source and destination context only (hivemind-core `_send_policy_denied`),
//! so it carries no request id and names the type it refused instead. The
//! shared `refusal-vectors.json` pins every case here.

use std::time::Duration;

use serde_json::Value;

use crate::constants::{
    EVENT_INTENT_FAILURE, EVENT_INTENT_UNMATCHED, EVENT_POLICY_DENIED,
    EVENT_RECOGNIZER_LOOP_UTTERANCE,
};
use crate::errors::{Quota, ThalovantError};
use crate::events::Event;

/// How long a fire-and-forget utterance counts as possibly still being refused.
///
/// Denials come back as fast as the hub admits a message -- milliseconds -- so
/// this is generous on purpose: a wrong "in flight" only costs an ask the
/// deadline it always had, where a wrong "not in flight" ends a question the
/// hub never refused. The shared refusal vectors name it
/// (`untracked_grace_seconds`), so every SDK uses the same window.
pub(crate) const UNTRACKED_UTTERANCE_GRACE: Duration = Duration::from_secs(10);

/// Whether a `hive.policy.denied` is this ask's to fail with.
///
/// A denial carrying a request id is judged by it, like any reply. Without one
/// it is taken when it names the type this ask sent and this ask is the only
/// utterance the client has out: with a second ask, a query, or a
/// fire-and-forget utterance still inside the grace window, either could be
/// the one refused, and a wrong guess ends a question the hub never refused.
pub(crate) fn refusal_belongs_to_ask(
    request_id: Option<&str>,
    own_request_id: &str,
    denied_type: Option<&str>,
    asks_in_flight: usize,
    queries_in_flight: usize,
    sends_in_flight: usize,
) -> bool {
    match request_id {
        Some(id) if !id.is_empty() => id == own_request_id,
        _ => {
            denied_type == Some(EVENT_RECOGNIZER_LOOP_UTTERANCE)
                && asks_in_flight == 1
                && queries_in_flight == 0
                && sends_in_flight == 0
        }
    }
}

/// The largest count the wire can carry, being the largest whole number every
/// JSON decoder holds exactly. Above it a decoder backed by a double can no
/// longer tell one whole number from the next, so two SDKs would report
/// different allowances for the same denial -- and a count nobody can agree on
/// is worse than none.
const MAX_COUNT: i64 = (1 << 53) - 1;

/// A whole, non-negative count from the wire, or 0: never a bool, never a
/// guess. A negative limit, usage or reset time is not something a policy can
/// mean, and passing one through would have an app say "-1 of -5 questions
/// used".
fn whole_count(value: Option<&Value>) -> u64 {
    // Whole, non-negative, and no larger than MAX_COUNT: past that it is not a
    // count a policy can have meant, and not one two SDKs could agree on.
    let whole = match value {
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
            number.as_i64().unwrap_or(0)
        }
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    if (0..=MAX_COUNT).contains(&whole) {
        whole as u64
    } else {
        0
    }
}

/// [`ThalovantError::PolicyDenied`] from the hub's `hive.policy.denied`.
pub(crate) fn policy_denied(event: &Event) -> ThalovantError {
    // The policy's own detail rides nested under data.data
    // (hivemind-core _send_policy_denied: "data": verdict.data).
    let inner = event.data.get("data").and_then(Value::as_object);
    let allowed = inner
        .and_then(|inner| inner.get("allowed"))
        .and_then(Value::as_array)
        .map(|items| {
            // Only non-blank strings, trimmed: a number, a null or a blank in
            // the hub's list is not a message type, and rendering one would
            // put "3" or "null" in front of an operator reading what to allow.
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let code = string_field(&event.data, "code");
    let quota = (code == Quota::EXCEEDED_CODE).then(|| {
        Box::new(Quota {
            period: inner
                .and_then(|inner| inner.get("period"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            limit: whole_count(inner.and_then(|inner| inner.get("limit"))),
            used: whole_count(inner.and_then(|inner| inner.get("used"))),
            reset_after: whole_count(inner.and_then(|inner| inner.get("reset_after"))),
        })
    });
    ThalovantError::PolicyDenied {
        denied_type: string_field(&event.data, "denied_type"),
        code,
        reason: string_field(&event.data, "reason"),
        allowed,
        quota,
    }
}

/// The typed error an ask fails with for the failure event it ended on: a
/// refusal, a question the hub has nothing for, and a fault need three
/// different sentences, and a bare runtime error allowed only one.
pub(crate) fn failure_error(event: &Event) -> ThalovantError {
    match event.name.as_str() {
        EVENT_POLICY_DENIED => policy_denied(event),
        EVENT_INTENT_UNMATCHED | EVENT_INTENT_FAILURE => ThalovantError::Unanswered {
            // What the person said: both names carry the input, and that is
            // what a caller shows. `reason` is not on these events at all, so
            // reading it left `said` empty.
            said: event.text().trim().to_string(),
        },
        _ => ThalovantError::Runtime(event.name.clone()),
    }
}

fn string_field(data: &serde_json::Map<String, Value>, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    //! The shared refusal vectors, vendored from the Python reference and
    //! pinned by the parity contract.
    use super::*;
    use serde_json::json;

    fn vectors() -> Value {
        serde_json::from_str(include_str!("../tests/data/refusal-vectors.json")).unwrap()
    }

    fn event_of(case: &Value) -> Event {
        let wire = &case["event"];
        Event {
            name: wire["type"].as_str().unwrap().to_string(),
            data: wire["data"].as_object().cloned().unwrap_or_default(),
            context: wire["context"].as_object().cloned().unwrap_or_default(),
            raw: None,
        }
    }

    #[test]
    fn every_failure_event_becomes_the_error_its_vector_names() {
        for case in vectors()["classification"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expect = &case["expect"];
            let error = failure_error(&event_of(case));
            if expect["kind"] == "unanswered" {
                let ThalovantError::Unanswered { said } = &error else {
                    panic!("{name}: wanted an unanswered question, got {error}");
                };
                // What the person said, which is what a caller shows.
                assert_eq!(said.as_str(), expect["said"], "{name}");
                continue;
            }
            let ThalovantError::PolicyDenied {
                denied_type,
                code,
                reason,
                allowed,
                quota,
            } = error
            else {
                panic!("{name}: wanted a refusal, got {error}");
            };
            let produced = json!({
                "kind": "refused",
                "denied_type": denied_type,
                "code": code,
                "reason": reason,
                "allowed": allowed,
                "quota": quota.map(|quota| json!({
                    "period": quota.period,
                    "limit": quota.limit,
                    "used": quota.used,
                    "reset_after": quota.reset_after,
                })),
            });
            assert_eq!(&produced, expect, "{name}");
        }
    }

    #[test]
    fn a_denial_is_taken_only_by_the_ask_it_can_belong_to() {
        for case in vectors()["correlation"].as_array().unwrap() {
            let request_id = match case["request_id"].as_str() {
                Some("own") => Some("req-own"),
                Some("other") => Some("req-other"),
                _ => None,
            };
            let taken = refusal_belongs_to_ask(
                request_id,
                "req-own",
                case["denied_type"].as_str(),
                case["asks_in_flight"].as_u64().unwrap() as usize,
                case["queries_in_flight"].as_u64().unwrap() as usize,
                case["sends_in_flight"].as_u64().unwrap() as usize,
            );
            assert_eq!(
                taken,
                case["taken"].as_bool().unwrap(),
                "{}",
                case["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn the_grace_window_is_the_one_the_vectors_name() {
        let named = vectors()["untracked_grace_seconds"].as_u64().unwrap();
        assert_eq!(UNTRACKED_UTTERANCE_GRACE, Duration::from_secs(named));
    }

    #[test]
    fn the_vectors_cover_every_kind_of_refusal() {
        // A copy that quietly lost its quota or its unanswered case would still pass.
        let vectors = vectors();
        let classification = vectors["classification"].as_array().unwrap();
        for code in [
            "acl_disallowed_type",
            Quota::EXCEEDED_CODE,
            Quota::BACKEND_UNAVAILABLE_CODE,
        ] {
            assert!(
                classification
                    .iter()
                    .any(|case| case["expect"]["code"] == code),
                "no vector for {code}"
            );
        }
        assert!(classification
            .iter()
            .any(|case| case["expect"]["kind"] == "unanswered"));
        let correlation = vectors["correlation"].as_array().unwrap();
        assert!(correlation.iter().any(|case| case["taken"] == true));
        assert!(correlation.iter().any(|case| case["taken"] == false));
    }
}
