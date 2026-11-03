Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Chat apps audit notes (0.16.0), merged with the CodeRabbit follow-ups from main.

# Audit

Written after `cargo test --all-features -- --test-threads=2` passed in full on 2026-11-03 (see `MAINTENANCE_REPORT.md`), and from reading the code.

## Known issues and technical debt

1. **A 429's wait is not readable from the error.** The chat-app rate limits (`bridge_rate_limited`, `bridge_network_rate_limited`) send the wait only in the `Retry-After` header. `send_request_full` in `src/control.rs` reads that header, but only `get_with_retry_after` (used by `wait_for_admission`) keeps it; `ThalovantError::ApiResponse` (`src/errors.rs`, the `ApiResponse` variant) has no member for it, so a caller of any other method cannot know how long to wait. `tests/chat_apps.rs` (`refusals_carry_their_code_fields_and_kind`) checks the 429's status and code only.
2. **Credentials are not wiped from memory.** `SecretValue` (`src/chat_apps.rs`) holds an ordinary `String`, and the request body built by `exposed_secrets` and `create_bridge` (`src/control/bridges.rs`) is a `serde_json::Value` dropped after sending. The crate has no `zeroize` dependency; this is documented on `SecretValue`.
3. **The shared api-error vectors do not cover `fields` yet.** `produced` in `tests/api_errors.rs` records status, code, detail and problem only, as the Python reference's vectors define them. `ApiProblem::fields` and `ThalovantError::api_fields` (`src/errors.rs`) are covered by `tests/chat_apps.rs` instead.
4. **Most control-plane reads are untyped.** `list_hubs`, `get_hub`, the memory and runtime-group calls in `src/control.rs` return `serde_json::Value`; chat apps (`src/chat_apps.rs`), `OperationResource` and `DeviceLoginRequest` are the typed exceptions.
5. **Releases do not wait for the whole fleet's parity.** The consumer branch of `.github/workflows/sdk-parity.yml` runs `check-sdk-parity.py --consumer thalovant-rust-sdk` without `--release`, and `auto-release.yml` (`planned-parity`) and `publish.yml` reuse it, so a "not yet on par" state is reported but does not stop a release. See `SUGGESTIONS.md` item 5.
6. **The minimum Rust version was not built locally.** Only the current compiler was available; the `minimum-rust` CI job builds and tests on 1.88. The new code uses no feature newer than 1.88 (const `assert!` is 1.57).

## Security notes

- No credential reaches an error, `Debug` or JSON output: `SecretValue` has no `Display`, `Serialize` or `Deserialize` (checked at compile time by `NO_WAY_TO_PRINT_A_SECRET` in `tests/chat_apps.rs`), its `Debug` prints `<redacted>`, and `a_refused_create_never_prints_the_credentials_it_sent` checks an API that echoes the token.
- `get_bridge_avatar` reads the image as bytes (`request_bytes` in `src/control.rs`), with the same HTTPS-only rule and redirect refusal as every control-plane request, and refuses a body over 4 MiB by its `Content-Length` or while streaming (`a_picture_is_read_whole_and_never_past_its_cap`).
- A refusal of the avatar route is read at most 4 MiB too; past that it is the `ApiResponse` of its status with no problem (`read_at_most` in `src/control.rs`, covered by `a_picture_is_read_whole_and_never_past_its_cap`).
- `OperationStatus` (`src/control.rs`) is a closed public enum: an operation status the API adds later fails `ControlPlane::get_operation`. `Bridge::operation` reads such an operation as `None` (`readable_operation` in `src/chat_apps.rs`, `an_unreadable_operation_leaves_the_bridge_readable`) so the bridge stays readable; opening `OperationStatus` would break exhaustive matches and waits for a breaking release.
- Test assertions about credentials name the printed form by position rather than printing it, so a failing test cannot write a credential to the CI log (code scanning flagged the first version).

Known issues, technical debt and their status, from test runs and code reading. Each entry names the code it concerns.

## Fixed

- **Conformance record could carry an earlier run's cases** (CodeRabbit on #64). `record()` in `tests/common/mod.rs` cleared stale shards only when `THALOVANT_CONFORMANCE_OUT` did not exist yet. The documented target, `contracts/conformance-results.json`, is committed, so it always exists. Shards are now scoped to a run, and `tests/conformance_record.rs` fails if an earlier run's shard reaches the record.

## Open

- **`usual_form` differs from the Python reference on two kinds of tag** (found while checking CodeRabbit on #66, by running both on the same inputs; not changed here):
  - A tag with an empty subtag: `"en-"`, `"en--US"`, `"en-US-"`. The reference returns `None` because any empty subtag ends it (`thalovant/_language.py`, `usual_form`). `src/language_matching.rs` `usual_form` returns `Some("en-us")`.
  - A language CLDR knows only through its macrolanguage: `"arb"`, `"cmn"`. The reference returns `arb-eg` / `cmn-cn`, checks the macrolanguage's likely subtags and keeps the language's own code. Rust returns `None` because `DATA.likely` has no direct entry.
  - The Go port gives the same answers as Rust on these inputs. No shared vector covers `usual_form`, so the parity gate does not see either difference.
  - Dangling singletons (`"en-u"`, `"en-x"`) already match the reference (`en-us`).
- **A `ws://` endpoint carries the access key in cleartext** (CodeRabbit on #30, outside the diff). `authorized_wss_url` in `src/transport.rs` accepts `ws://` as well as `wss://` and puts `authorization`, which is base64 of `user-agent:access_key`, in the upgrade URL's query, before Noise starts. The Python reference (`_hive.py`, "WSS endpoint must start with ws:// or wss://") and the Go port (`transport_wss.go`) accept `ws://` the same way, so refusing it only here would break parity. The access key alone does not authenticate: the Noise pre-shared key comes from the password (`derive_psk`, Argon2id), and the client key is pinned. Platform identities name `wss://` endpoints. Requiring `wss://` would be a cross-SDK decision, for every port at once.
- **Tests not yet written, because unit tests live under `src/`, a release path.** Each would publish a crate with no code change, so they wait for the next change to `src/`:
  - The usual-form retry after an empty first listing (CodeRabbit on #66). `en-CA` against a hub that loaded `en-us` keeps `languages == ["en-CA"]` and reports `listed_in == ["en-us"]`; with `nearest: false` it lists once and reports `["en-CA"]`. Checked on 2026-11-03 with a test that was run and not committed, against `FakeHub` in `src/intents.rs`; it passes on `main` (a81ec4a). The tests in place cover the keyword-only retry (`a_tag_with_only_keyword_rows_is_listed_under_its_usual_form`) and the nearest-off listing (`the_hubs_own_listing_is_not_asked_when_nearest_is_off`).
  - Single `read()` test servers (CodeRabbit on #21). The provisioning tests read whole requests through `read_full_request` (since 8a27279). About ten older test servers in `src/control.rs` still take one `read()` and then assert on the body, for example `login_sends_mfa_fields_only_when_provided`. They can fail if a request's body arrives in a later TCP segment. No CI failure has been seen from it.
- **Conformance shards on platforms without a parent process id**. Outside Unix, a run is told apart only by `THALOVANT_CONFORMANCE_RUN` or a deleted target, as before. CI records conformance on Linux.
