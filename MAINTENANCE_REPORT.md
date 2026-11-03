Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Chat apps change (0.16.0) and its review round.

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
