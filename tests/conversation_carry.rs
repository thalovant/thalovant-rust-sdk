//! Carrying a conversation between the turns of a named session.
//!
//! A hub keeps nothing for one: OVOS-SESSION-2 §2.2 makes the orchestrator
//! stateless, so the carrier a client sends is the whole snapshot and whatever
//! the last turn activated is discarded the moment it ends. Without
//! `converse_handlers` the converse pipeline has no skill to poll and every
//! follow-up reaches the fallback instead of the skill that just answered.
//!
//! The cases are `contracts/conformance/conversation-vectors.json` and
//! `mesh-vectors.json`, shared with every other SDK so that being on par is
//! something a machine checks rather than something a digest asserts.

use serde_json::Value;
use thalovant::events::{carry_conversation, CONVERSATION_SESSION_FIELDS, HIVE_KINDS};

fn vectors(name: &str) -> Value {
    let raw = std::fs::read_to_string(format!("tests/conformance/{name}"))
        .unwrap_or_else(|error| panic!("read {name}: {error}"));
    serde_json::from_str(&raw).unwrap_or_else(|error| panic!("parse {name}: {error}"))
}

#[test]
fn the_carry_matches_conversation_vectors() {
    let spec = vectors("conversation-vectors.json");
    for case in spec["cases"].as_array().expect("cases") {
        let previous = case["previous"].as_object().cloned().unwrap_or_default();
        let session = case["session"].as_object().cloned().unwrap_or_default();
        let expected = case["expected"].as_object().cloned().unwrap_or_default();
        assert_eq!(
            carry_conversation(Some(&previous), &session),
            expected,
            "{}",
            case["name"],
        );
    }
}

#[test]
fn the_carried_fields_are_the_ones_the_vectors_name() {
    let spec = vectors("conversation-vectors.json");
    let mut want: Vec<String> = spec["carried_fields"]
        .as_array()
        .expect("carried_fields")
        .iter()
        .map(|field| field.as_str().expect("string").to_string())
        .collect();
    let mut got: Vec<String> = CONVERSATION_SESSION_FIELDS
        .iter()
        .map(|f| f.to_string())
        .collect();
    want.sort();
    got.sort();
    assert_eq!(got, want);
}

#[test]
fn the_fields_the_vectors_forbid_never_travel() {
    // A remembered `lang` would pin a bilingual conversation to whichever
    // language it opened in, which is the failure this list prevents.
    let spec = vectors("conversation-vectors.json");
    for field in spec["never_carried"].as_array().expect("never_carried") {
        let field = field.as_str().expect("string");
        assert!(!CONVERSATION_SESSION_FIELDS.contains(&field), "{field}");
    }
}

#[test]
fn the_hive_kinds_are_the_ones_the_vectors_name() {
    let spec = vectors("mesh-vectors.json");
    let mut want: Vec<String> = spec["kinds"]
        .as_array()
        .expect("kinds")
        .iter()
        .map(|kind| kind.as_str().expect("string").to_string())
        .collect();
    let mut got: Vec<String> = HIVE_KINDS.iter().map(|k| k.to_string()).collect();
    want.sort();
    got.sort();
    assert_eq!(got, want);
}

#[test]
fn this_clients_own_traffic_is_not_a_hive_kind() {
    // `query` and `cascade` belong to ask; subscribing to one here would
    // quietly compete for the same replies.
    let spec = vectors("mesh-vectors.json");
    for refused in spec["refused_kinds"].as_array().expect("refused_kinds") {
        let refused = refused.as_str().expect("string");
        assert!(!HIVE_KINDS.contains(&refused), "{refused}");
    }
}
