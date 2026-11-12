Last Edit: Claude Sonnet 5.5 - 2026-11-11 - Motive: Intercom final API: intercom:urgent, speaker kind/can_play/policy_synced, refusal kinds, text rule, DND zone, replayed.

# Quick facts: thalovant (Rust SDK)

| Fact | Value |
| :-- | :-- |
| Crate | `thalovant` on crates.io |
| Version | 0.18.0 (`Cargo.toml`; the user agent reads it through `CARGO_PKG_VERSION`) |
| Rust | 1.88 or newer (`rust-version`), edition 2021 |
| Licence | MIT |
| Binaries | none: a library crate |
| Default control API | `https://api.thalovant.com` (`DEFAULT_CONTROL_API_URL`) |
| Release | `auto-release.yml`, on a push to `main`, when `src/` (tests included), `Cargo.toml` or `LICENSE` changed since the latest `v*` tag, or when no such tag exists: publishes the version in `Cargo.toml` when its tag does not exist yet, else bumps the patch and publishes that |
| Parity contract | `contracts/sdk-parity.json`, checked by the Python SDK's `scripts/check-sdk-parity.py` |
| Reference digests accepted | `57c7e71f6456…` (Python `main`, 0.10.0) and `6dfb3bb31ac7…` (thalovant-python-sdk#113 at 39e31d8, browser sign-in) |

## Entry points

| Item | Where |
| :-- | :-- |
| `ControlPlane` (control API client) | `src/control.rs`; chat apps in `src/control/bridges.rs`, hub skills in `src/control/hub_skills.rs` |
| `Client` (hub data plane over HTTPS, WSS, MQTTS) | `src/client.rs` |
| Browser sign-in and the chooser (`browser_login`, `interactive_login`) | `src/control/browser_login.rs` |
| Loopback listener (private: `LoopbackListener`, `classify_request`) | `src/loopback.rs`, its suite in `src/loopback_tests.rs` |
| PKCE, URL, redirect, browser rule, configuration | `src/native_auth.rs` (module `thalovant::native_auth`) |
| `ThalovantError`, `ApiProblem`, `ApiRefusal` | `src/errors.rs` |
| Chat apps types and `SecretValue` | `src/chat_apps.rs` (module `thalovant::chat_apps`, re-exported at the crate root) |

## Sign-in

| `ControlPlane` method | What it does | Returns |
| :-- | :-- | :-- |
| `interactive_login(InteractiveLoginOptions)` | `LoginMethod::Auto` (default): browser when `can_open_browser` and the configuration is usable, else device login; `Browser` / `Device` force one | the token answer (`Value`) |
| `browser_login(BrowserLoginOptions)` | `GET /v1/auth/native/config`, listen on `127.0.0.1:0`, open `{authorization_endpoint}?response_type=code&client_id&redirect_uri&code_challenge&code_challenge_method=S256&state&scope&device_name`, wait for `GET /callback`, `POST {token_endpoint}` | the token answer (`Value`) |
| `login_with_browser(DeviceLoginOptions)` | device login, unchanged | the token answer (`Value`) |

| Fact | Value |
| :-- | :-- |
| Client id | `thalovant-sdk` (`native_auth::SDK_CLIENT_ID`, or the configuration's `sdk_client_id`) |
| Redirect | `http://127.0.0.1:<port>/callback` (`native_auth::loopback_redirect_uri`), port 1024-65535 |
| PKCE | S256 only; verifier 64 random bytes (86 characters), state 24 random bytes (32 characters), both from the OS |
| Timeout | 300 s browser (`DEFAULT_BROWSER_LOGIN_TIMEOUT`), 900 s device login |
| Device name | `thalovant-rust-sdk on <hostname>` when `None`, left out when empty, at most 120 code points |
| Errors | denied `DeviceAuthorizationDenied`, timeout `DeviceAuthorizationExpired`, other callback error `BrowserLoginFailed`, refused exchange `ApiResponse` 400 `invalid_grant` |
| Request bound | configuration and exchange: 10 s each (`BROWSER_REQUEST_TIMEOUT`); the tab waits for the sign-in to end |
| Vectors | `tests/conformance/browser-login-vectors.json` (119 cases): `tests/browser_login.rs` and `src/loopback_tests.rs`; private edges in `src/control/browser_login_tests.rs` |

## Intercom (`src/intercom.rs`, `src/control/intercom.rs`; 0.18.0; scopes `intercom:read`, `intercom:send`, `intercom:urgent` (urgent needs it as well as `intercom:send`), `hubs:write`)

Announce-only text spoken aloud on a hub's speakers. Not a certified safety system; the API does not store the text (keyed hash and length only). Named `announce` / `*_intercom_*`: the mesh already has an `intercom` frame.

| Call | Route | Returns |
| :-- | :-- | :-- |
| `get_intercom_settings(hub_id)` | `GET /v1/hubs/{id}/intercom/settings` | `IntercomSettings` |
| `set_intercom_settings(hub_id, enabled)` | `PUT /v1/hubs/{id}/intercom/settings` | `IntercomSettings` |
| `list_intercom_speakers(hub_id)` | `GET /v1/hubs/{id}/intercom/speakers` | `IntercomSpeakerList` (`policy_synced`, `unavailable_reason`; speakers have `kind`, `can_play`) |
| `update_intercom_speaker(hub_id, client_id, &IntercomSpeakerUpdate)` | `PATCH /v1/hubs/{id}/intercom/speakers/{client_id}` | `IntercomSpeaker` |
| `announce(hub_id, &AnnounceOptions)` | `POST /v1/hubs/{id}/intercom/announce` (urgent needs `intercom:urgent` too) | `Announcement` (`replayed`; deliveries have `detail`) |
| `get_announcement(hub_id, id)` | `GET /v1/hubs/{id}/intercom/announcements/{id}` | `Announcement` |
| `cancel_announcement(hub_id, id)` | `POST /v1/hubs/{id}/intercom/announcements/{id}/cancel` (`intercom:send`; 409 `not_cancellable`, `announcement_status()`) | `Announcement` (`status` `Cancelled`, optional `hub_confirmed: Option<bool>`) |
| `list_announcements(hub_id, &AnnouncementListOptions)` | `GET /v1/hubs/{id}/intercom/announcements` | `AnnouncementList` |

- Refusal kinds for the final API's codes (constants in `thalovant::intercom::problem_code`): `UrgentScopeRequired` (403 `urgent_scope_required`), `UrgentBusy` (429, `urgent_busy_scope()`), `SkillNotInstalled` (409), `NotASpeaker` (422; `not_a_speaker_kind()`, `not_speakers()`), `ReservedName`, `HubLocked`, `ImpersonationNotAllowed`, `TooManyTargets` (`matched_speakers()`), `IntercomUnavailable` (`unavailable_reason()`), `IntercomDisabledPlatform`, `IntercomNotConfigured`, `ManagedClient` (`refused_client_refs()`); `is_intercom_disabled()` and `is_auth()` on `ApiRefusal`. Text rule: NFC, 1-280, no `<` `>`, control/bidi/format/private-use characters; DND on needs a timezone (shape-checked, no tz database linked) and start != end; target lists at most 50 names of 1-64 characters; `idempotency_key` at most 128 printable ASCII.
- Errors (`src/errors.rs`): `ApiRefusal::IntercomDisabled`, `ApiRefusal::HubUnreachable` (502; the text may or may not have been spoken; `ThalovantError::announcement_id()` names the `failed` record: read it before sending again, and use a new key), `ApiRefusal::NotCancellable` (409), `ThalovantError::is_rate_limited()`; `ApiRefusal::Plan` is HTTP 402 or 403 `plan_limit`.
- `announce` with `wait` polls `get_announcement` (`intercom:read`, which `intercom:send` implies); a poll that fails after the POST is `ThalovantError::AnnouncementWaitFailed` (`announcement()` is the accepted announcement).
- Idempotency: one fresh key per announcement (`new_request_id()` or a UUID); the same key replays the stored result (`Idempotency-Replayed: true`) and never re-sends (a replayed `failed` stays `failed`). Ids (hub, client, announcement) that are `.`, `..`, empty or hold `/ ? #`, a backslash or a control character are refused before any request (`valid_id`, `src/intercom.rs`). `AnnounceOptions` `Debug` redacts the text (`<N chars>`). No request timeout is set by the SDK.

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
