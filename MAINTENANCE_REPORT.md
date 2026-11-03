Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Record the CodeRabbit review-body items checked on older PRs.

# Maintenance Report

## 2026-11-03 — CodeRabbit review-body items on older PRs

CodeRabbit's review bodies hold findings that are not review threads: nitpicks, and comments outside the diff. Fourteen of them, on #21, #23, #30, #37, #40 and #66, were checked against `main` (a81ec4a). Each old PR has a comment that lists them and says how each was handled.

- **Already fixed:** eleven.
  - In `8a27279` (#42): empty-etag guard, full request reads in the provisioning tests, order-free install body.
  - In `2939de5` (#37): `Notified` registered before the WSS read task.
  - In `922d140` (#39): one `NoiseChannel` for every transport, and the PSK cache with it.
  - In `5156879` (#40): Argon2 off the async workers, staging collisions, bounded readiness loops, one describe deadline.
  - In `3a9bed5` (#70): close verdict before the disconnect wakes `connect()`.
- **Fixed here:** CHANGELOG.md had no entry for the breaking MQTT change in #23, which shipped in 0.2.24. It now has one, with migration steps. Its section, still headed "Unreleased" although 0.2.23 to 0.2.27 shipped it, now says so.
- **Not changed:** two.
  - The `ws://` scheme (#30), which every SDK accepts alike.
  - The retry test asked for on #66, a test-only addition that would cut a release.
  - Also left: older single-`read()` test servers, outside #21's fixed scope. See AUDIT.md for all three.

Verification: documentation only. No file `auto-release.yml` treats as a release path changes, and the parity contract hashes none of these files.

## Transparency Report

- **AI Model**: Claude Opus 5.5.
- **Actions Taken**: Read every CodeRabbit review-body item on this repo's merged PRs and checked each against `main` and its history. Ran an uncommitted test of the usual-form retry. Wrote the CHANGELOG entry and the AUDIT.md notes, and posted the old-PR comments.
- **Oversight**: Pull-request CI. No human reviewed this before the pull request was opened.

## 2026-11-03 — CodeRabbit follow-ups (#64, #66)

| Old PR | Finding | Outcome |
| --- | --- | --- |
| #64 | `tests/common/mod.rs`: a reused `THALOVANT_CONFORMANCE_OUT` merges shards an earlier run left in `<target>.parts`. | Still true on `main` (9394578), and fixed. The recorder cleared old shards only when the target was missing, but the documented command, `THALOVANT_CONFORMANCE_OUT=contracts/conformance-results.json`, writes over a committed file that is always there. Shards now live in one directory per run. The run is identified by the parent process under `cargo test`/`cargo nextest` (its id, plus its start time on Linux), or by `THALOVANT_CONFORMANCE_RUN`, percent-encoded. Only the current run's shards are merged, and the first process of a run clears every other run's. `*.parts/` is git-ignored. |
| #66 | `usual_form("en-u")` should be `None`. | Not changed. The Python reference returns `en-us` for `en-u` (and for `en-x`, `en-x-foo`, `en-u-ca-gregory`), and a port must answer as the reference does. See AUDIT.md for the reference differences this check did find. |

Verification:

- CodeRabbit's review of #85 asked for three more things, now done: a failed removal of an earlier run's shard panics instead of being ignored; run names are percent-encoded rather than filtered, so `feature/a` and `featurea` stay two runs (`two_run_names_stay_two_runs` fails with the old filter); and a reused parent id is told apart by the parent's start time.
- New `tests/conformance_record.rs`. With the old clearing rule restored, `an_earlier_run_is_not_counted_although_the_target_is_there` fails: the earlier run's shard is merged into the new run's record, and the recorder panics "recorded twice with different outputs". With the fix it passes.
- `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` are clean.
- `THALOVANT_CONFORMANCE_OUT=contracts/conformance-results.json cargo test --all-features -- --test-threads=2`: 361 passed, 0 failed. `contracts/conformance-results.json` is byte-identical to the committed file.
- `check-sdk-parity.py --consumer thalovant-rust-sdk` against a clean reference `main` verifies after refreshing the `tests/common/mod.rs` evidence hash.

## Transparency Report

- **AI Model**: Claude Opus 5.5.
- **Actions Taken**: Verified both unresolved CodeRabbit threads against current `main` and the Python reference. Changed shard scoping in `tests/common/mod.rs` and added `record_into`. Added `tests/conformance_record.rs` and the `.gitignore` entry. Refreshed parity evidence hashes. Probed `usual_form` against the reference on edge tags. Ran the checks above, including the revert check.
- **Oversight**: Pull-request CI (Linux, macOS, Windows, minimum Rust, parity). No human reviewed this change before the pull request was opened. `tests/` is not a release path for `auto-release.yml`, so merging does not cut a release.
