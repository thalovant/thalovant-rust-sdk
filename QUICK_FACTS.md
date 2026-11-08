Last Edit: Claude Opus 5.5 - 2026-11-07 - Motive: Release row names the paths that cut a release.

# Quick facts: thalovant (Rust SDK)

| Fact | Value |
| :-- | :-- |
| Crate | `thalovant` on crates.io |
| Version | 0.16.0 (`Cargo.toml`; the user agent reads it through `CARGO_PKG_VERSION`) |
| Rust | 1.88 or newer (`rust-version`), edition 2021 |
| Licence | MIT |
| Binaries | none: a library crate |
| Default control API | `https://api.thalovant.com` (`DEFAULT_CONTROL_API_URL`) |
| Release | `auto-release.yml`, on every push to `main` that changes `src/` (tests included), `Cargo.toml` or `LICENSE` since the last tag: publishes the version in `Cargo.toml` when its tag does not exist yet, else bumps the patch and publishes that |
| Parity contract | `contracts/sdk-parity.json`, checked by the Python SDK's `scripts/check-sdk-parity.py` |

## Entry points

| Item | Where |
| :-- | :-- |
| `ControlPlane` (control API client) | `src/control.rs`; chat apps in `src/control/bridges.rs`, hub skills in `src/control/hub_skills.rs` |
| `Client` (hub data plane over HTTPS, WSS, MQTTS) | `src/client.rs` |
| `ThalovantError`, `ApiProblem`, `ApiRefusal` | `src/errors.rs` |
| Chat apps types and `SecretValue` | `src/chat_apps.rs` (module `thalovant::chat_apps`, re-exported at the crate root) |

## Chat apps (`/v1/bridges`)

| `ControlPlane` method | Route | Returns |
| :-- | :-- | :-- |
| `list_chat_app_connectors()` | `GET /v1/bridges/connectors` | `Vec<ChatAppConnector>` |
| `get_bridge_networks(Option<&str>)` | `GET /v1/bridges/networks?hub_id=` | `BridgeNetworks` |
| `list_bridges(&BridgeListOptions)` | `GET /v1/bridges?hub_id=&owner_id=&limit=` | `Vec<Bridge>` |
| `create_bridge(BridgeCreate)` | `POST /v1/bridges` | `Bridge` |
| `get_bridge(&str)` | `GET /v1/bridges/{id}` | `Bridge` |
| `update_bridge(&str, &BridgeUpdate)` | `PATCH /v1/bridges/{id}` (`If-Match` from `BridgeUpdate::etag`) | `Bridge` |
| `replace_bridge_token(&str, impl Into<SecretValue>)` | `PUT /v1/bridges/{id}/token` | `Bridge` |
| `replace_bridge_secrets(&str, pairs)` | `PUT /v1/bridges/{id}/secrets` | `Bridge` |
| `list_bridge_chats(&str)` | `GET /v1/bridges/{id}/chats` | `BridgeChatList` |
| `send_bridge_test_message(&str, &str)` | `POST /v1/bridges/{id}/test` | `BridgeTestResult` |
| `get_bridge_avatar(&str)` | `GET /v1/bridges/{id}/avatar` | `BridgeAvatar` (bytes, content type) |
| `delete_bridge(&str)` | `DELETE /v1/bridges/{id}` | `()` |

- Open enums (`Other(String)` keeps unknown values): `BridgeState`, `BridgeSessionScope`, `BridgePhase`, `BridgeStatusReason`, `BridgeStatusSource`, `BridgeHoldReason`, `ChatAppStatus`, `ChatAppHelper`.
- Request types: `BridgeCreate` (`with_token`, `with_secrets`, `with_settings`), `BridgeCredentials`, `BridgeUpdate`, `BridgeListOptions`, `BridgeAllowedChat`.
- Errors: `ThalovantError::api_fields()` and `ApiProblem::fields()` name refused settings or secrets.

## Validate

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features -- --test-threads=2
cargo package --allow-dirty --list
```
