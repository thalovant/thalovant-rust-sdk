Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Chat apps audit notes (0.16.0), merged with the conformance-record follow-up from main.

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
- **Conformance shards on platforms without a parent process id**. Outside Unix, a run is told apart only by `THALOVANT_CONFORMANCE_RUN` or a deleted target, as before. CI records conformance on Linux.
