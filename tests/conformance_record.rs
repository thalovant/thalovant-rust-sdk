//! The conformance record counts only the run that wrote it.
//!
//! `contracts/conformance-results.json` is the evidence the parity gate
//! compares with the reference, so a case this run never executed must not
//! reach it. The documented command writes over the committed file, which is
//! always there, so "no target yet" cannot be what tells one run from the
//! next.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "thalovant-conformance-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The recorded cases of one vector file, name to digest.
fn recorded(target: &Path) -> Vec<(String, String)> {
    let document: Value =
        serde_json::from_str(&fs::read_to_string(target).expect("read target")).expect("json");
    document["results"]["binary-frames.json"]["cases"]
        .as_object()
        .expect("cases")
        .iter()
        .map(|(name, digest)| (name.clone(), digest.as_str().expect("digest").to_string()))
        .collect()
}

#[test]
fn an_earlier_run_is_not_counted_although_the_target_is_there() {
    let dir = scratch("earlier-run");
    let target = dir.join("conformance-results.json");
    common::record_into(
        &target,
        "earlier",
        "binary-frames.json",
        "earlier-only",
        &json!(1),
    );
    common::record_into(
        &target,
        "earlier",
        "binary-frames.json",
        "both",
        &json!("old"),
    );
    assert!(
        target.exists(),
        "the earlier run left its record, as a committed one is"
    );

    common::record_into(&target, "this", "binary-frames.json", "both", &json!("new"));

    assert_eq!(
        recorded(&target),
        vec![("both".to_string(), common::canonical_digest(&json!("new")))],
        "only what this run produced"
    );
    fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn one_run_keeps_every_case_it_recorded() {
    let dir = scratch("one-run");
    let target = dir.join("conformance-results.json");
    common::record_into(&target, "this", "binary-frames.json", "first", &json!(1));
    common::record_into(&target, "this", "binary-frames.json", "second", &json!(2));

    assert_eq!(
        recorded(&target),
        vec![
            ("first".to_string(), common::canonical_digest(&json!(1))),
            ("second".to_string(), common::canonical_digest(&json!(2))),
        ]
    );
    fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn two_run_names_stay_two_runs() {
    assert_ne!(common::run_name("feature/a"), common::run_name("featurea"));
    assert_eq!(common::run_name("feature/a"), "feature%2Fa");
    assert_eq!(common::run_name("ci-42_b"), "ci-42_b");
    assert!(!common::run_name("../up").contains('/'));
}
