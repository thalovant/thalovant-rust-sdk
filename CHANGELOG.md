# Changelog

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
