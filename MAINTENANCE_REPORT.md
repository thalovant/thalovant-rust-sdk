Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Chat apps (0.16.0), merged with the CodeRabbit follow-up entries from main.

# Maintenance report

## 2026-11-03: chat apps (0.16.0)

**Change.** Added the owner routes under `/v1/bridges` as `ControlPlane` methods (`src/control/bridges.rs`), their typed models, open enums, request types and `SecretValue` (`src/chat_apps.rs`, re-exported from `src/lib.rs`), a bytes-reading request path for the bot's picture (`request_bytes` in `src/control.rs`, sharing the existing HTTPS rule and redirect refusal), and `ApiProblem::fields` / `ThalovantError::api_fields` (`src/errors.rs`). `OperationResource` derives `PartialEq`. Version 0.16.0 in `Cargo.toml`. README "Chat apps" section, a compiled module example, CHANGELOG entry, a `chat-apps` entry in `contracts/sdk-parity.json` and refreshed hashes for the capabilities whose evidence files changed (`src/control.rs`, `src/errors.rs`, `src/lib.rs`). These notes files are excluded from the published crate.

**Rationale.** Hub owners can connect chat networks to their hubs; every SDK and the MCP server expose the same owner routes with parallel names. The Python SDK is the parity reference.

**Verification.**
- `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features -- --test-threads=2` (371 passed, 0 failed, including 3 doc tests, after the review round), `cargo package --allow-dirty --list`, and `cargo doc --no-deps --all-features` with warnings denied, on the current stable compiler; CI also builds on Rust 1.88 and on macOS and Windows.
- `tests/chat_apps.rs`: 11 tests covering every route (method, escaped path, query, exact body, `If-Match` only with an etag), parsing with unknown enum values and absent members, refusals with code and fields and the unchanged plan/auth classification, local refusals before any request, the avatar's bytes, and fake credentials absent from every printed form; the missing traits on `SecretValue` are checked at compile time.
- The shared api-error vectors (`tests/api_errors.rs`) still pass unchanged.
- Parity checker run as a consumer: verified, with the expected rollout note that the reference does not know `chat-apps` yet.

**Review round (same day).** Code scanning flagged three test assertions that printed a formatted request or error in their failure message; they now name the form by position. From the CodeRabbit review: a blank `Some` hub or owner filter is refused locally instead of being dropped (`get_bridge_networks`, `list_bridges`); `get_bridge_avatar` refuses a body over 4 MiB without reading the rest; the README states that `list_bridge_chats` needs `hubs:write` (as the API's route requires) and its example no longer sends an empty `allowed`. Declined: running the whole-fleet `--release` parity check on this repository's release paths, a fleet-wide gate change recorded in `SUGGESTIONS.md` item 5.

### Transparency report

- **AI model:** Claude Opus 5.5 (`claude-opus-5-5`).
- **Actions taken:** wrote the chat apps module, the control-plane methods, the bytes request path, the error `fields` accessors, the tests, README, CHANGELOG, parity entry and these repository notes; ran the full test suite, clippy, rustdoc, the package check and the parity checker.
- **Oversight:** automated tests and CI, CodeRabbit review on the pull request, merged by the repository owner's delegated agent.

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
- **Recorded in AUDIT.md** with both of those: about ten older single-`read()` test servers. They are outside the provisioning tests that #21's item named, which are fixed.

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
