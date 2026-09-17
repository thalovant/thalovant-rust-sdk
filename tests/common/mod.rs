//! Record what this SDK produced for each conformance case.
//!
//! The parity gate can check that a test *names* a vector file. It cannot
//! check that the test ran it: a name reaching a loader call is evidence of
//! intent, not of execution. So the gate stopped asking about the test and
//! started asking about its output -- this writes what we computed, and the
//! checker compares it against what the Python reference computed for the
//! same case.
//!
//! The digest has to agree across languages, so it is deliberately the same
//! recipe as the reference's `tests/conformance_record.py`: JSON with keys
//! sorted at every depth, no insignificant whitespace, non-ASCII left as
//! itself, SHA-256 of the UTF-8 bytes, and a whole number spelled without a
//! fractional part. `serde_json::Map` is a `BTreeMap` here (no
//! `preserve_order` feature), so the sorting is already what Python's
//! `sort_keys` gives.
//!
//! Set `THALOVANT_CONFORMANCE_OUT` to a path and run the suite.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Spell a whole number the way every language spells it.
///
/// Rust writes an f64 of 1.0 as `1.0`; the reference now writes it as `1`,
/// because that is what JavaScript, Go, C# and Swift all write and JSON has
/// one number type. `conversation-vectors.json` has `activated_at: 1.0`.
fn same_number_everywhere(value: &Value) -> Value {
    match value {
        Value::Number(number) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && number.as_i64().is_none() => {
                Value::from(float as i64)
            }
            _ => value.clone(),
        },
        Value::Array(items) => Value::Array(items.iter().map(same_number_everywhere).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), same_number_everywhere(item)))
                .collect::<Map<String, Value>>(),
        ),
        _ => value.clone(),
    }
}

/// A stable digest of a produced value, agreeing across languages.
pub fn canonical_digest(value: &Value) -> String {
    let canonical = serde_json::to_string(&same_number_everywhere(value)).expect("serialise");
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

fn target() -> Option<PathBuf> {
    std::env::var_os("THALOVANT_CONFORMANCE_OUT").map(PathBuf::from)
}

/// Record what this SDK produced for one case of one vector file.
///
/// Written through on every call rather than at exit: each file under
/// `tests/` is its own test binary, so the binary cases and the conversation
/// cases run in different processes and neither can see the other's results.
/// Each writes its own shard and then rebuilds the whole file from every
/// shard present, so whichever finishes last leaves it complete.
pub fn record(vector_file: &str, case: &str, produced: &Value) {
    let Some(target) = target() else { return };
    let parts = PathBuf::from(format!("{}.parts", target.display()));
    fs::create_dir_all(&parts).expect("shard directory");

    let shard = parts.join(format!("{}.json", std::process::id()));
    let mut mine: BTreeMap<String, BTreeMap<String, String>> = fs::read_to_string(&shard)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let digest = canonical_digest(produced);
    let entry = mine.entry(vector_file.to_string()).or_default();
    if let Some(previous) = entry.get(case) {
        assert_eq!(
            previous, &digest,
            "{vector_file}/{case}: recorded twice with different outputs"
        );
    }
    entry.insert(case.to_string(), digest);
    fs::write(&shard, serde_json::to_string(&mine).expect("shard")).expect("write shard");

    merge(&parts, &target);
}

fn merge(parts: &Path, target: &Path) {
    let mut merged: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut shards: Vec<_> = fs::read_dir(parts)
        .expect("read shards")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    shards.sort();
    for shard in shards {
        let Ok(raw) = fs::read_to_string(&shard) else {
            continue;
        };
        let Ok(cases) = serde_json::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&raw)
        else {
            continue;
        };
        for (vector_file, recorded) in cases {
            merged.entry(vector_file).or_default().extend(recorded);
        }
    }

    let mut results = Map::new();
    for (vector_file, cases) in merged {
        // The parsed JSON, not the bytes: a vendored copy is allowed to differ
        // in indentation and line endings, and the checker accepts it on the
        // same terms.
        let raw = fs::read_to_string(format!("tests/conformance/{vector_file}"))
            .unwrap_or_else(|error| panic!("read {vector_file}: {error}"));
        let parsed: Value = serde_json::from_str(&raw).expect("parse vectors");
        let mut entry = Map::new();
        entry.insert("digest".into(), Value::from(canonical_digest(&parsed)));
        entry.insert(
            "cases".into(),
            Value::Object(
                cases
                    .into_iter()
                    .map(|(name, digest)| (name, Value::from(digest)))
                    .collect::<Map<String, Value>>(),
            ),
        );
        results.insert(vector_file, Value::Object(entry));
    }

    let mut document = Map::new();
    document.insert("schema_version".into(), Value::from(1));
    document.insert("results".into(), Value::Object(results));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).expect("output directory");
    }
    fs::write(
        target,
        serde_json::to_string_pretty(&Value::Object(document)).expect("document") + "\n",
    )
    .expect("write results");
}
