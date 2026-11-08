Last Edit: Claude Opus 5.5 - 2026-11-07 - Motive: CodeRabbit review-body close-out: ws:// for loopback only, and test-only changes without a release.

# Suggestions

## 1. Read a 429's wait from the error

- **Problem:** chat-app rate limits (`bridge_rate_limited`, `bridge_network_rate_limited`) put the wait only in `Retry-After`, which no public accessor exposes (`AUDIT.md` item 1).
- **Proposed solution:** keep the header's wait beside the body inside `ApiProblem` (a private member, so no public type changes) and add `ThalovantError::retry_after() -> Option<Duration>`: the body's `retry_after_seconds`, else the header.
- **Impact:** callers back off for the time the API asks instead of guessing; additive.

## 2. Put `fields` in the shared api-error vectors

- **Problem:** `fields` is read by every SDK but proved only by each SDK's own tests (`AUDIT.md` item 3).
- **Proposed solution:** add a `bridge_settings_invalid` case with `fields` (and a nested-`detail` one) to the Python reference's `api-error-vectors.json`, and add `fields` to what each SDK records.
- **Impact:** one behaviour, one set of vectors across the SDKs.

## 3. Type the other control-plane reads

- **Problem:** hubs, memory and runtime groups return raw JSON (`AUDIT.md` item 4), so each caller picks fields by hand.
- **Proposed solution:** typed models in the style of `src/chat_apps.rs` (`#[non_exhaustive]` structs, open enums), behind new methods so the `Value` ones keep working.
- **Impact:** fewer field-name mistakes in callers; larger API surface to maintain.

## 4. Optional zeroize for `SecretValue`

- **Problem:** credentials stay in freed memory until reused (`AUDIT.md` item 2).
- **Proposed solution:** an optional `zeroize` feature that wipes `SecretValue` on drop.
- **Impact:** defence in depth for long-running processes; one optional dependency.

## 5. Gate releases on the whole-fleet parity check

- **Problem:** the release paths (`auto-release.yml`, `publish.yml`) reuse `sdk-parity.yml`, which checks this SDK as one consumer without `--release`, so a "not yet on par" state (such as `chat-apps` before the Python reference declares it) reports but does not stop a release.
- **Proposed solution:** on release paths, check out every sibling SDK and run the checker with `--workspace ... --release`, keeping the consumer check for pull requests; decide it fleet-wide, since every SDK reuses the same workflow shape.
- **Impact:** a release cannot ship while the fleet is behind; a release then waits on every sibling SDK's acknowledgement.

## 6. Accept `ws://` only for a loopback hub, in every SDK at once

- **Problem:** `authorized_wss_url` in `src/transport.rs` accepts `ws://` and puts `authorization` (base64 of `user-agent:access_key`) in the upgrade URL before Noise starts, so on a `ws://` endpoint the access key crosses the network in the clear (CodeRabbit on #30). The HTTP transport (`require_tls_endpoint`) and MQTT (`mqtt_options_for_identity`) already refuse cleartext for that reason. The Python reference accepts `ws://` the same way (`_hive.py`), so this port matches it and cannot change alone (`AUDIT.md`, Open).
- **Proposed solution:** in the Python reference first, then in every port: accept `ws://` only for `localhost`, `127.0.0.1` and `[::1]`, the rule `validate_control_request_url` in `src/control.rs` already applies to `http://`, and add a shared vector for it.
- **Impact:** removes the last path where the SDK sends the access key unencrypted. A client pointed at a `ws://` hub on another host fails to connect until its identity names a `wss://` endpoint, so this needs a minor release and a changelog note in each SDK.

## 7. Let test-only changes under `src/` merge without a release

- **Problem:** `auto-release.yml` treats any change under `src/` as release-relevant. A change to a `#[cfg(test)]` module therefore bumps the patch version and publishes a crate whose code did not change. The contract tests and full-request test servers asked for on #66 and #21 wait in draft #88 for that reason (`AUDIT.md`, Open).
- **Proposed solution:** move unit-test modules into `src/**/*_tests.rs` files included with `#[cfg(test)] #[path = ...]`, as `src/transport_noise_tests.rs` already is, and have the planner in `auto-release.yml` skip a push whose `src/` changes are only `*_tests.rs` files.
- **Impact:** test-only fixes merge when they are written. It costs a one-time move of the test modules and one more rule in the release planner.
