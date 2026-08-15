# Changelog

## Unreleased

- Security: redact secrets from every `Debug`/`{:?}` rendering. `Identity`, `MqttBrokerCredentials`, `control::LoginOptions`, `control::DeviceAuthorization`, `control::BootstrapIdentityResult`, `events::Event`, and `events::Reply` now use hand-written `Debug` implementations that redact credentials — access key, password, crypto key, MQTT username/password, MFA `otp_code`/`recovery_code`, device code, and the end-user bearer token duplicated into `context.auth_token` and `context.auth.token`. `Serialize`/`Deserialize` are **unchanged**, so identity-file persistence and the wire protocol still emit the real values.
- Security: `BootstrapIdentityResult::as_value(false)` now redacts the credential subkeys in the `hub` and `client` resources (for example the `apiKey`, `password`, and `cryptoKey` minted by `POST /v1/clients`), matching how it already redacted the identity block. `as_value(true)` still returns the real values for persistence.
- Security: strip the request URL from stored `reqwest` errors. Data-plane request URLs carry the caller's access key in a `?authorization=` query, which reqwest's `Display` appended as " for url (...)"; that URL is no longer copied into `TransportHealth::last_error` or any rendered `ThalovantError::Http`.
- Security: bound and redact the server response body interpolated into `ThalovantError::Api` messages for `/v1/auth/token`, `/v1/auth/device/token`, and `/v1/clients`. Errors now carry the HTTP status plus a short, single-line, secret-redacted detail instead of the raw body.
- **BREAKING:** remove the admin analytics path. `AnalyticsOverviewOptions::admin` is deleted and `get_analytics_overview` no longer targets `GET /v1/admin/analytics/overview`; it always calls `GET /v1/analytics/overview`. This SDK serves non-admin customers, who never had access to the admin route. Callers that set `admin: true` must drop the field; `owner_id` is still sent and is scoped to the caller's own tenant by the API.

## 0.2.22

- Add the hub-provisioning surface: `create_hub`, `update_hub`, `delete_hub`, `release_hub`, `set_hub_rating`, `clear_hub_rating`, and `get_hub_runtime_capabilities`. `create_hub` sends a generated `Idempotency-Key` unless the caller supplies one. `update_hub` and `delete_hub` take `etag` as a **required** argument, not an option, because the API enforces optimistic locking on both routes and rejects a stale *or missing* `If-Match` with HTTP 412; the etag is read from the hub resource's `etag` body field, as the API emits no `ETag` response header.
- Add runtime-group management: `list_runtime_groups`, `get_runtime_group`, `create_runtime_group`, `update_runtime_group`, `get_runtime_group_config`, `update_runtime_group_config`, `release_runtime_group`, and `delete_runtime_group`. These routes read no `If-Match` and no idempotency header, so concurrent writes are last-write-wins. `update_runtime_group_config` merges rather than replaces, and sends `personas` only when it is `Some(..)`.
- Add skill discovery and installation: `list_marketplace_skills`, `list_runtime_group_marketplace`, `list_runtime_group_inventory`, `install_runtime_group_skill`, and `uninstall_runtime_group_skill`.
- New public types `ReleaseOptions`, `MarketplaceSkillsOptions`, `SkillInstallOptions`, and constant `DEFAULT_SKILL_SOURCE_TYPE`. No existing signature changed and no `ThalovantError` variant was added: HTTP failures on these routes keep the crate's existing mapping to `ThalovantError::Api` carrying the status and response body.
- Scope and plan notes now documented on each method: the provisioning writes require a paid plan and `hubs:write` (HTTP 402 on the free plan, HTTP 403 without the scope); the rating routes require `hubs:write` but are **not** paid-gated; `list_marketplace_skills` needs only `hubs:read` and is not paid-gated, with `owner_id` and `include_inactive` silently ignored for non-admin callers; and `get_hub_runtime_capabilities`, `list_runtime_group_marketplace`, and `list_runtime_group_inventory` require `hubs:inspect`. Only `get_hub_runtime_capabilities` answers HTTP 409 when no client is connected — the two runtime-group reads return an empty `data` list with a pending `source` of `ovos-runtime-operator-pending`.
- Derive both user-agent constants from `CARGO_PKG_VERSION` instead of repeating the version literal, so a release bump can no longer leave them stale.

## 0.2.21

- Document the two HTTP 429 responses the control plane returns for token-authenticated calls: `token_rate_limited` (the plan's per-minute request rate, 60 requests per minute on the free plan) and `token_quota_exceeded` (the plan's daily or monthly call quota, reported in `quota`, `limit`, and `used`). Both carry a `Retry-After` header and a matching `retry_after_seconds`, both surface as `ThalovantError::Api`, `Retry-After` is authoritative, and the SDK does not retry automatically.

## 0.2.20

- Add browser device-flow sign-in: `ControlPlane::login_with_browser(DeviceLoginOptions)` requests a device authorization from `/v1/auth/device/authorize`, shows the verification URI and user code (override with `DeviceLoginOptions::prompt`), best-effort opens the browser at `verification_uri_complete` (`xdg-open`/`open`, never fatal), and polls `/v1/auth/device/token` honoring the server `interval` and `slow_down` back-pressure until approval. On approval the durable scoped API token is stored on `access_token` exactly like `login`.
- New public types `DeviceLoginOptions`, `DeviceAuthorization`, `DevicePrompt`, constant `DEFAULT_DEVICE_POLL_INTERVAL`, and error variants `ThalovantError::DeviceAuthorizationDenied` and `ThalovantError::DeviceAuthorizationExpired` (a wait past `DeviceLoginOptions::timeout` fails with the existing `ThalovantError::Timeout`).
- Document direct API-token auth for CI (`ControlPlane::with_access_token` / `ControlPlane::new` with a pre-provisioned token such as `THALOVANT_API_TOKEN`); no code change, the constructors already accepted a token.

## 0.2.19

- Add MFA login support: `ControlPlane::login_with_options` and `LoginOptions` send optional `otp_code` and `recovery_code` fields to `/v1/auth/token` for accounts that require multi-factor authentication. `ControlPlane::login` is unchanged.
- Realign the control-plane user agent with the crate version (it was stuck at `thalovant-rust-sdk/0.2.17`) and add a regression test that pins both user-agent constants to `CARGO_PKG_VERSION`.

## 0.2.18

- Update the `base64` dependency from 0.22 to 0.23.
- CI and release-process hardening only, no API changes: pin GitHub Actions by full SHA, attest crate releases, add repository security ownership (CODEOWNERS and SECURITY.md), and schedule Dependabot dependency updates limited to minor and patch versions.

## 0.2.17

- Use the native TLS backend for MQTT so the published dependency graph no longer includes vulnerable `rustls-webpki 0.102.8`.
- Add an explicit regression assertion for the MQTT TLS backend and align runtime user-agent versions.
- Give CI and release-guard workflows explicit read-only repository permissions.

## 0.2.16

- Add typed `OperationResource` and `ControlPlane::get_operation` support.
