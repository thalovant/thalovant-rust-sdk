Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Record the CodeRabbit follow-ups to #64 and #66.

# Maintenance Report

## 2026-11-03 — CodeRabbit follow-ups (#64, #66)

| Old PR | Finding | Outcome |
| --- | --- | --- |
| #64 | `tests/common/mod.rs`: a reused `THALOVANT_CONFORMANCE_OUT` merges shards an earlier run left in `<target>.parts`. | Still true on `main` (9394578), and fixed. The recorder cleared old shards only when the target was missing, but the documented command, `THALOVANT_CONFORMANCE_OUT=contracts/conformance-results.json`, writes over a committed file that is always there. Shards now live in one directory per run. The run is identified by the parent process id under `cargo test`/`cargo nextest`, or by `THALOVANT_CONFORMANCE_RUN`. Only the current run's shards are merged, and the first process of a run clears every other run's. `*.parts/` is git-ignored. |
| #66 | `usual_form("en-u")` should be `None`. | Not changed. The Python reference returns `en-us` for `en-u` (and for `en-x`, `en-x-foo`, `en-u-ca-gregory`), and a port must answer as the reference does. See AUDIT.md for the reference differences this check did find. |

Verification:

- New `tests/conformance_record.rs`. With the old clearing rule restored, `an_earlier_run_is_not_counted_although_the_target_is_there` fails: the earlier run's shard is merged into the new run's record, and the recorder panics "recorded twice with different outputs". With the fix it passes.
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` are clean.
- `THALOVANT_CONFORMANCE_OUT=contracts/conformance-results.json cargo test --all-features -- --test-threads=2`: 360 passed, 0 failed. `contracts/conformance-results.json` is byte-identical to the committed file.
- `check-sdk-parity.py --consumer thalovant-rust-sdk` against a clean reference `main` verifies after refreshing the `tests/common/mod.rs` evidence hash.

## Transparency Report

- **AI Model**: Claude Opus 5.5.
- **Actions Taken**: Verified both unresolved CodeRabbit threads against current `main` and the Python reference. Changed shard scoping in `tests/common/mod.rs` and added `record_into`. Added `tests/conformance_record.rs` and the `.gitignore` entry. Refreshed parity evidence hashes. Probed `usual_form` against the reference on edge tags. Ran the checks above, including the revert check.
- **Oversight**: Pull-request CI (Linux, macOS, Windows, minimum Rust, parity). No human reviewed this change before the pull request was opened. `tests/` is not a release path for `auto-release.yml`, so merging does not cut a release.
