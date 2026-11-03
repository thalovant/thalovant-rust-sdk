Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Audit notes from adding chat apps (0.16.0), after a green full test run.

# Audit

Written after `cargo test --all-features -- --test-threads=2` passed in full on 2026-11-03 (see `MAINTENANCE_REPORT.md`), and from reading the code.

## Known issues and technical debt

1. **A 429's wait is not readable from the error.** The chat-app rate limits (`bridge_rate_limited`, `bridge_network_rate_limited`) send the wait only in the `Retry-After` header. `send_request_full` in `src/control.rs` reads that header, but only `get_with_retry_after` (used by `wait_for_admission`) keeps it; `ThalovantError::ApiResponse` (`src/errors.rs`, the `ApiResponse` variant) has no member for it, so a caller of any other method cannot know how long to wait. `tests/chat_apps.rs` (`refusals_carry_their_code_fields_and_kind`) checks the 429's status and code only.
2. **Credentials are not wiped from memory.** `SecretValue` (`src/chat_apps.rs`) holds an ordinary `String`, and the request body built by `exposed_secrets` and `create_bridge` (`src/control/bridges.rs`) is a `serde_json::Value` dropped after sending. The crate has no `zeroize` dependency; this is documented on `SecretValue`.
3. **The shared api-error vectors do not cover `fields` yet.** `produced` in `tests/api_errors.rs` records status, code, detail and problem only, as the Python reference's vectors define them. `ApiProblem::fields` and `ThalovantError::api_fields` (`src/errors.rs`) are covered by `tests/chat_apps.rs` instead.
4. **Most control-plane reads are untyped.** `list_hubs`, `get_hub`, the memory and runtime-group calls in `src/control.rs` return `serde_json::Value`; chat apps (`src/chat_apps.rs`), `OperationResource` and `DeviceLoginRequest` are the typed exceptions.
5. **The minimum Rust version was not built locally.** Only the current compiler was available; the `minimum-rust` CI job builds and tests on 1.88. The new code uses no feature newer than 1.88 (const `assert!` is 1.57).

## Security notes

- No credential reaches an error, `Debug` or JSON output: `SecretValue` has no `Display`, `Serialize` or `Deserialize` (checked at compile time by `NO_WAY_TO_PRINT_A_SECRET` in `tests/chat_apps.rs`), its `Debug` prints `<redacted>`, and `a_refused_create_never_prints_the_credentials_it_sent` checks an API that echoes the token.
- `get_bridge_avatar` reads the image as bytes (`request_bytes` in `src/control.rs`), with the same HTTPS-only rule and redirect refusal as every control-plane request.
