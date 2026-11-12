Last Edit: Claude Sonnet 5.5 - 2026-11-11 - Motive: CodeRabbit review of PR 91: wait validation and deadline, accepted announcement kept on a poll error; digest 66bfb1ac5879.

# Maintenance report

## 2026-11-11: intercom review fixes (CodeRabbit, PR 91; Python digest `66bfb1ac5879`)

`announce` refuses a zero `poll_interval` and a `wait_timeout` the clock cannot reach (`Instant::checked_add`) before the POST; the deadline bounds each poll (`tokio::time::timeout_at`, the latest `sending` answer on expiry) and no poll starts once the sleep used the deadline. A poll that fails after the POST is the new `ThalovantError::AnnouncementWaitFailed { announcement, source }` instead of the bare poll error, so the accepted announcement's id is not lost; `status_code`, `api_answer` (hence `api_problem`, `api_refusal`, `api_code`, `is_rate_limited`), `is_timeout`, `is_connection_error` and `is_api_unreachable` look through to `source`. README: the example lists the speakers and addresses one by its `client_id`. Docs: `wait` needs `intercom:read` too, 402 is listed with 403 `plan_limit` (`ApiRefusal::Plan`), and `ApiRefusal::HubUnreachable` no longer says a retry is safe. `tests/intercom.rs` asserts the status of every refusal on its own and adds a 402 case. Verification: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`, parity check against the Python reference.

Transparency Report: AI model Claude Sonnet 5.5; actions: fixed the review findings, added tests, refreshed the parity record; oversight: none yet, review pending.

## 2026-11-11: intercom (0.18.0)

Ported the `intercom` capability from thalovant-python-sdk 0.12.0 (digest `690784dbb9af`, since moved to `66bfb1ac5879`): `src/intercom.rs`, `src/control/intercom.rs` (seven `ControlPlane` methods), two `ApiRefusal` kinds and `is_rate_limited` in `src/errors.rs`, `tests/intercom.rs` (loopback peer), the parity record, README, QUICK_FACTS, FAQ, AUDIT and CHANGELOG. Verified with `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings` and `cargo test --all-features`; the other capabilities' evidence hashes for the files this change touched were refreshed after the suite passed.

Transparency Report: AI model Claude Sonnet 5.5; actions: wrote the module, methods, tests, docs and parity record from the contract and the Python reference; oversight: none yet, review pending.

## 2026-11-08 (later): CodeRabbit on #90, and the final browser-login reference

**CodeRabbit.** Eleven threads; ten fixed with tests, one answered.
- Fixed: the FAQ now says a variable counts only when non-empty. A malformed API URL is no longer repeated in an error (`require_secure_token_exchange`). The configuration and the exchange are bounded at 10 s each, headers and body (`bounded_request`), and a stalled configuration is a fallback in `auto`. A refused exchange's body is scrubbed before its message is cut to length (`redacted_body`). A callback `error` keeps only RFC 6749's codes, so it cannot carry a code or the state. The tab no longer has a 60 s deadline of its own. `wait` reaps finished connection tasks and serves at most 32 connections at once. A callback is claimed under the lock `close` checks under, so a claimed tab always gets its grace. An `authorization_endpoint` whose host no browser can open is unusable. The browser-rule test keeps a non-Unicode variable.
- Answered, not changed: a flow-neutral message for `DeviceAuthorizationExpired`. The porting brief has every SDK reuse its device-login error types and leave device login exactly as it is, and the unit variant is shared with `login_with_browser`, so rewording it rewrites device login's message. It stays `SUGGESTIONS.md` item 8, for a change across the SDKs at once; reported to the lead.

**CodeRabbit's re-review of 7e4d76f.** Seven more threads; six fixed with tests, one answered.
- Fixed: the README states the guarantee's limit (a forwarded callback port, or another user of the same computer). A browser sign-in reads its answers to at most 64 KiB. A secret is also scrubbed as a URL or a form writes it, and as a JSON number. An oversized `HEAD` gets its 400 with no body. An `authorization_endpoint` the URL parser cannot read (`example.0x10`) is unusable.
- Answered, not changed: matching a secret across an existing `[redacted]`. The brief fixes the scrub as "one pass, longest first, never inside a `[redacted]` already there", and a value that contains `[redacted]` cannot be a real code, verifier or state, which are base64url. A code a browser made up is no credential, since the API refuses it. Reported to the lead.

**CodeRabbit's follow-ups.** `a_message_cut_to_length_keeps_no_part_of_a_secret` did not straddle the cut: Rust's display line is the problem as JSON, so `{"detail":"` came first and pushed the verifier past 200 characters. The padding is now 179, and with the pre-format scrub taken out the test fails, which it did not before. A mixed-case echo of an encoded code (`a%2bb%2Fc%3D`) is scrubbed too: an escape's two hex digits match in either case (`written_at`). Answered, not changed: keeping a callback's `error` when it equals the callback's code (`code=server_error&error=server_error`). The kept value can only be one of RFC 6749's seven public words, so nothing secret reaches the error, and the API's codes are random base64url, never one of them.

**Final reference.** thalovant-python-sdk#113 at 39e31d8 (digest `6dfb3bb31ac7590591febef1c4d8372984ca6cdad4700381f869db30c38452df`, which replaces the interim `49744ee385bf…`; `57c7e71f6456…` stays). The vectors are re-vendored byte for byte (sha256 `b0c73f808c4f26328c27dd4d04eebfbbc3e7c0c81b8d63a666f63737d0259e73`, 119 cases), and the brief's ten changes are ported: every request stays on the API's origin (`send`); `token_endpoint` with a colon or a broken `%`-escape is unusable; `authorization_endpoint` with port 0, a port above 65535 or a host that is not an address or ASCII DNS labels is unusable (`real_host`); only RFC 6749's error codes are kept; only a missing `device_name` gets the default, and an empty one is left out (and no `client_name` is sent); the tab's 60 s bound is gone; a `HEAD` answer has no body; a refusal is redacted before its message is built, in one pass, longest first, with `[redacted]`, into a new error; draining before close stays (2 s / 1 MiB); and the wording is "a link or code sent to someone on another computer delivers nothing to the sender". The API (#501) is live: `GET https://api.thalovant.com/v1/auth/native/config` answers 200 with a usable configuration, and a real `browser_login` against it, with no browser opened and a 1 s timeout, built the consent URL with the contract's parameters and expired as it should (a temporary test, not committed).

**Verification.** `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings`: clean. `THALOVANT_CONFORMANCE_OUT=contracts/conformance-results.json cargo test --all-features -- --test-threads=2`: 431 passed, 0 failed after the re-review fixes (337 library unit tests, among them 20 in `src/loopback_tests.rs` and 11 in the new `src/control/browser_login_tests.rs`; 90 in the integration binaries, 22 in `tests/browser_login.rs`; 4 doc tests; 426 before them). All 119 browser-login cases recorded, every digest equal to the reference's at 39e31d8, the original 98 unchanged, and the nine other vector files' records byte-identical. `check-sdk-parity.py --workspace <ws> --consumer thalovant-rust-sdk`: verified against 39e31d8 (snapshot `6dfb3bb31ac7`) and against `main` 44b6e2b (`57c7e71f6456`, with the rollout note that the reference does not know `browser-login` yet). Evidence hashes were refreshed only for files this round changed: `src/control.rs`, `src/errors.rs` and `src/native_auth.rs` in the capabilities that name them, and the browser-login entry, which now lists `src/control/browser_login_tests.rs` as test evidence.

### Transparency report

- **AI model:** Claude Opus 5.5 (`claude-opus-5-5`).
- **Actions taken:** read each CodeRabbit thread against the code and the final brief; fixed ten with tests and answered one; ported the final reference's changes and re-vendored its vectors; updated the parity and conformance records from a full run; updated the README, CHANGELOG and these notes; ran the checks above; replied on and resolved every thread.
- **Oversight:** automated tests, the parity checker and CI. No human reviewed this round before it was pushed; nothing was merged, tagged or released by the agent.

## 2026-11-08: browser sign-in (0.17.0)

**Change.** Ported browser sign-in from the Python reference (thalovant-python-sdk#113, `e3999cb`, porting brief `docs/browser-login-porting.md`): `ControlPlane::browser_login` and `ControlPlane::interactive_login` with `BrowserLoginOptions`, `InteractiveLoginOptions`, `LoginMethod`, `BrowserPrompt` and `BrowserOpener` (`src/control/browser_login.rs`, re-exported from `src/control.rs` and `src/lib.rs`); the loopback listener and callback classifier (`src/loopback.rs`, private, with its suite in `src/loopback_tests.rs`); the URL builder, redirect, state, device name, browser rule and configuration parser (`src/native_auth.rs`); and `ThalovantError::BrowserLoginFailed` (`src/errors.rs`). `login_with_browser_as` now delegates to a private `device_login`, which the chooser calls with its opener; device login's behaviour is unchanged. `random_url_safe` reads the OS random source directly (`SysRng`). Dependencies: `subtle` 2, `windows-sys` 0.61 on Windows, tokio `net` and `io-util`, all already in the tree. Version 0.17.0. README "Sign in" section (and the quick start now uses `interactive_login`), CHANGELOG, `AGENTS.md`, `QUICK_FACTS.md`, `FAQ.md`, `AUDIT.md` and `SUGGESTIONS.md`.

**Parity.** `tests/conformance/browser-login-vectors.json` vendored byte for byte (sha256 `9058adf2ad30fd8b7f255882ce718865170d0d4b84d36f8fbfa23bde51d1651e`, 98 cases). `contracts/sdk-parity.json`: `reference_digests` keeps `57c7e71f6456…` and adds `49744ee385bf…` (both recomputed here with `check-sdk-parity.py --snapshot` on Python `main` 44b6e2b and on `e3999cb`); a new `browser-login` entry binds the six implementation files and the four test files (the colocated `src/loopback_tests.rs` as test evidence, as `contracts/README.md` in the reference asks); the hashes of `src/control.rs`, `src/errors.rs`, `src/lib.rs` and `src/native_auth.rs` were refreshed in the 12 other capabilities that name them (api-errors, chat-apps, connection-admission, connection-kinds, control, device-login, home-link, identity, link-carriers, refusal, runtime, session), because this change edits those files, after the full suite re-ran. `contracts/conformance-results.json` was rewritten by that run: the nine existing vector files' results are byte-identical to before, and all 98 browser-login case digests equal the reference's.

**Rationale.** A device code can be relayed to somebody else; a loopback redirect cannot, so a machine with a browser signs in through it and device login stays the fallback. Every SDK the reference marks `required` ports the same contract.

**Verification** (current stable compiler, rustc 1.99.1, Linux):
- `cargo fmt --check`: clean.
- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `THALOVANT_CONFORMANCE_OUT=contracts/conformance-results.json cargo test --all-features -- --test-threads=2`: 412 passed, 0 failed after the CI round below (319 library unit tests including the 13 in `src/loopback_tests.rs`, 89 in the integration binaries including the 21 in `tests/browser_login.rs`, and 4 doc tests; 411 before it). The two new suites were run three more times: stable.
- `cargo package --allow-dirty --list`: succeeds and lists the new files. `cargo doc --no-deps --all-features`: no warnings.
- `check-sdk-parity.py --workspace <ws> --consumer thalovant-rust-sdk` against the Python PR head (`e3999cb`): verified, conformance results compared. Against Python `main` (44b6e2b): verified, with the expected rollout note that the reference does not know `browser-login` yet.
- Not run here: the Windows build (no Windows target installed; `exclusive_address_use` is `#[cfg(windows)]`), macOS, and the Rust 1.88 job. CI's `windows-latest`, `macos-latest` and `minimum-rust` jobs cover them.

**CI round (same day).** `test (macos-latest)` failed one listener test: a request head over 16 KiB got a connection reset instead of its 400, because the listener closed with the rest of the head unread and macOS discards an answer once the reset arrives. Each answered connection now drains what is left (at most 2 s and 1 MiB) before it closes, and a new test sends large bodies and heads. Linux keeps the answer readable after a reset, so the new test passes there with or without the fix (checked by disabling the drain); the macOS job is what proves it. The full suite was re-run with the recorder after the fix (see the counts above, updated), and the evidence hashes of `src/loopback.rs` and `src/loopback_tests.rs` in `browser-login` were recomputed from the new files.

### Transparency report

- **AI model:** Claude Opus 5.5 (`claude-opus-5-5`).
- **Actions taken:** read the contract, the Python reference and its brief, vectors and parity tooling; wrote the browser sign-in, the listener, the chooser, the new error variant, both test suites and the vendored vectors; updated the parity record (new entry, both digests, refreshed hashes for the files changed) and the conformance record from a full run; wrote the README, CHANGELOG and these notes; ran the checks above.
- **Oversight:** automated tests, the parity checker and CI. No human reviewed this change before the pull request was opened; it is not merged, tagged or released by the agent.

## 2026-11-07: CodeRabbit review-body close-out

CodeRabbit's review bodies on merged PRs hold 16 items that are not review threads: nitpicks, and comments outside the diff. Every one was checked against `main` (072a2d8). Every CodeRabbit review thread on these PRs was already resolved.

| PR | Item | Outcome |
| --- | --- | --- |
| #21 | Reject an empty `etag` in `update_hub` / `delete_hub` | Fixed since 8a27279 (#42): both return `ThalovantError::Api` before any request; `empty_hub_etags_fail_before_any_request`. |
| #21 | Read the whole request in `spawn_recording_server` | Fixed since 8a27279 (#42): `read_full_request` reads up to `Content-Length`. Nine older servers in `src/control.rs` that still read once are moved to it in #88, held (see below). |
| #21 | Compare the install body as JSON, not as text | Fixed since 8a27279 (#42): `request_body` parses it and compares it as a `Value`. |
| #23 | Document the breaking MQTT change and bump the version | Fixed since 706908c (#86): the 0.2.24 **BREAKING** entry names each change and how to migrate. Published versions are never replaced, and 0.3.0 and later shipped after it. |
| #30 | Run Argon2id off the async workers | Fixed since 5156879 (#40): `receive_noise` runs handshake frames in `spawn_blocking`, and the PSK is cached since 922d140 (#39). |
| #30 | Register the `Notified` waiter before spawning the read task | Fixed since 2939de5 (#37): created, pinned and `enable()`d before `tokio::spawn`, in the WSS transport and in the MQTT one too. |
| #30 | Require `wss://` before adding `authorization` | Not changed. The Python reference accepts `ws://` the same way (`_hive.py`), so this is a decision for every SDK at once; the access key does not open a session without the password-derived PSK. Recorded in `AUDIT.md`; `SUGGESTIONS.md` item 6 proposes loopback-only `ws://`. |
| #30 | Record why the WebSocket closed | Fixed since 8a27279 (#42), which records the close code as `last_error` (the peer's reason text is left out on purpose), and 3a9bed5 (#70), which stores the refusal verdict before `connect()` wakes. |
| #37 | Reuse the PSK cache over HTTP and MQTT | Fixed since 922d140 (#39): `NoiseChannel::start` loads and saves the cache, and `continue_handshake` forgets it when the answer does not authenticate. |
| #37 | Move WSS onto `NoiseChannel`; share the health check | Fixed since 922d140 (#39): all three transports hold a `NoiseChannel` and report health through `noise_health`. |
| #40 | Tell staging collisions from publication races; hard-link portability | The staging part was fixed before merge (5156879): staging names carry a UUID, and a collision is its own error. Hard links stay, as in the reference: neither `fs::rename` nor `create_new` is a safe fallback. Recorded in `AUDIT.md` and `FAQ.md`. |
| #40 | Bound the readiness spin loops | Fixed before merge (5156879): 5 s timeouts that fail with a message. |
| #40 | One deadline for all describe windows | Fixed before merge (5156879): `describe_many` computes the deadline once; `describe_windows_share_one_deadline_and_keep_earlier_answers`. |
| #65 | The 0.11.0 note gives the wrong quota-count ceiling | Still valid, fixed here: the entry now says 2^53 − 1, as `whole_count` enforces. |
| #66 | Serialise `listed_in` in `as_value` | Fixed since 6ef9f57 (#80): `as_value` writes `listed_in`, the key the Python reference uses; `the_serialised_inventory_says_which_language_it_listed_in`. |
| #66 | Retry contract tests | Still valid: written and passing in #88, held because `src/` is a release path. |

Also fixed here: the `## 0.15.0 — 2026-10-11` heading, replaced by mistake when the 0.16.0 entry was added (13d9640).

**Held, not merged: #88.** It adds `an_empty_listing_is_answered_by_its_usual_form` and `an_empty_listing_is_not_retried_when_nearest_is_off` to `src/intents.rs`, and moves the nine single-`read()` test servers in `src/control.rs` to `read_full_request`. It changes only test code, but `auto-release.yml` cuts a release for any change under `src/` since the latest tag, so merging it alone would ship a crate with no code change. It waits for the next `src/` change (`SUGGESTIONS.md` item 7).

**Verification.** This change touches only `CHANGELOG.md` and the notes files, none of them a release path. On the branch of #88, on the current stable compiler: `cargo fmt --check` and `cargo clippy --all-targets --all-features -- -D warnings` are clean, and `cargo test --all-features -- --test-threads=2` passes (378 passed, 0 failed, doc tests included); `cargo package --allow-dirty --list` succeeds; `check-sdk-parity.py --consumer thalovant-rust-sdk` against the Python reference verifies.

**Review round (#89).** CodeRabbit's ten comments on this change were all about wording, and nine are taken. The release rule now names both cases: the untagged `Cargo.toml` version, or the next patch, plus a release when no tag exists. The hard-link workaround names a filesystem with hard links, since a local exFAT disk has none. 2^53 − 1 is the binary64 limit, not every decoder's. The `ws://` advice is about the endpoint, not where the hub runs. The `ws://` suggestion checks the address actually connected to, not the name `localhost`, and only limits cleartext to loopback. The proposed release rule leaves `*_tests.rs` files out rather than skipping a push. The tenth asks for the cross-SDK `ws://` change itself, which stays `SUGGESTIONS.md` item 6.

### Transparency report

- **AI model:** Claude Opus 5.5 (`claude-opus-5-5`).
- **Actions taken:** read every CodeRabbit review body on the seven PRs and checked each item against `main`, its history and the Python reference; corrected the changelog; wrote the held tests; updated `AUDIT.md`, `SUGGESTIONS.md`, `FAQ.md` and `QUICK_FACTS.md`; ran the checks above; replied on each original PR.
- **Oversight:** pull-request CI. No human reviewed this change before the pull request was opened.

## 2026-11-03: chat apps (0.16.0)

**Change.** Added the owner routes under `/v1/bridges` as `ControlPlane` methods (`src/control/bridges.rs`), their typed models, open enums, request types and `SecretValue` (`src/chat_apps.rs`, re-exported from `src/lib.rs`), a bytes-reading request path for the bot's picture (`request_bytes` in `src/control.rs`, sharing the existing HTTPS rule and redirect refusal), and `ApiProblem::fields` / `ThalovantError::api_fields` (`src/errors.rs`). `OperationResource` derives `PartialEq`. Version 0.16.0 in `Cargo.toml`. README "Chat apps" section, a compiled module example, CHANGELOG entry, a `chat-apps` entry in `contracts/sdk-parity.json` and refreshed hashes for the capabilities whose evidence files changed (`src/control.rs`, `src/errors.rs`, `src/lib.rs`). These notes files are excluded from the published crate.

**Rationale.** Hub owners can connect chat networks to their hubs; every SDK and the MCP server expose the same owner routes with parallel names. The Python SDK is the parity reference.

**Verification.**
- `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features -- --test-threads=2` (371 passed, 0 failed, including 3 doc tests, after the review round), `cargo package --allow-dirty --list`, and `cargo doc --no-deps --all-features` with warnings denied, on the current stable compiler; CI also builds on Rust 1.88 and on macOS and Windows.
- `tests/chat_apps.rs`: 11 tests covering every route (method, escaped path, query, exact body, `If-Match` only with an etag), parsing with unknown enum values and absent members, refusals with code and fields and the unchanged plan/auth classification, local refusals before any request, the avatar's bytes, and fake credentials absent from every printed form; the missing traits on `SecretValue` are checked at compile time.
- The shared api-error vectors (`tests/api_errors.rs`) still pass unchanged.
- Parity checker run as a consumer: verified, with the expected rollout note that the reference does not know `chat-apps` yet.

**Review round (same day).** Code scanning flagged three test assertions that printed a formatted request or error in their failure message; they now name the form by position. From the CodeRabbit review: a blank `Some` hub or owner filter is refused locally instead of being dropped (`get_bridge_networks`, `list_bridges`); `get_bridge_avatar` refuses a body over 4 MiB without reading the rest; the README states that `list_bridge_chats` needs `hubs:write` (as the API's route requires) and its example no longer sends an empty `allowed`. Declined: running the whole-fleet `--release` parity check on this repository's release paths, a fleet-wide gate change recorded in `SUGGESTIONS.md` item 5.

**Second review round (same day).** From CodeRabbit's review of the fix commits: `Bridge::operation` reads as `None` when the operation cannot be parsed (an unknown `OperationStatus` failed the whole bridge; `OperationStatus` itself is unchanged, since a new variant would break exhaustive matches on a public enum); `BridgeCreate::with_settings` adds to the settings instead of replacing them; `get_bridge_avatar` caps a refusal's body like a picture's (`read_at_most` in `src/control.rs`); the README and module examples take up a bridge an earlier run left in setup and answer only in the chat the owner names, never every chat the bot can see. Declined: refusing an empty `allowed` locally, since the API's 422 `bridge_allowed_required` is the contract the other SDKs report too; the field's doc now says so. `tests/chat_apps.rs` has 13 tests; the three new ones fail against the previous code. Full suite: 376 passed, 0 failed (`cargo test --all-features -- --test-threads=2`), with fmt, clippy, rustdoc and the package check clean.

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
