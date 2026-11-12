Last Edit: Claude Sonnet 5.5 - 2026-11-11 - Motive: Intercom amendment 2: a replayed flag, problem scrubbing, caps.

# Suggestions

## A `replayed` field on `Announcement`

- **Problem:** the API answers a reused `Idempotency-Key` with the stored announcement and `Idempotency-Replayed: true`; `Announcement` does not carry it, so a caller cannot tell a replay from a first send (security review H3).
- **Proposal:** read the header in `ControlPlane::request` for the intercom routes and expose `Announcement::replayed`; the same on every SDK, with the header in the loopback tests.
- **Impact:** low effort; touches the reference digest and every consumer.

## Scrub `input` and `ctx` from the problem, and cap lists and keys, on intercom calls

- **Problem:** the problem keeps the whole API body; a regression in the API's error middleware would put the announcement text there (review L5). Rooms, groups and the key length are unchecked client-side (L6).
- **Proposal:** in `announce`, drop `input` and `ctx` from the problem; check `idempotency_key` (128) and rooms/groups (50).
- **Impact:** low; defence in depth.

## Shared intercom vectors

- **Problem:** `intercom` has no conformance vectors; text rules (control characters, trimming, character count) are proved by each SDK's own tests.
- **Proposal:** an `intercom-vectors.json` in the Python reference (validation and error-kind cases), claimed by the capability.
- **Impact:** low effort; one more file for every consumer.

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
- **Proposed solution:** in the Python reference first, then in every port: accept `ws://` only when the address actually connected to is loopback (`127.0.0.0/8` or `::1`). A host name such as `localhost` counts only after it resolves to loopback, and the checked address is the one used for the connection. Add a shared vector for it. `validate_control_request_url` in `src/control.rs` allows `http://localhost` by name alone, so the same tightening fits there.
- **Impact:** limits the unencrypted access key to loopback connections, which never leave the machine, instead of any host. A client pointed at a `ws://` hub on another host fails to connect until its identity names a `wss://` endpoint, so this needs a minor release and a changelog note in each SDK.

## 7. Let test-only changes under `src/` merge without a release

- **Problem:** `auto-release.yml` treats any change under `src/` since the latest tag as release-relevant. A change to a `#[cfg(test)]` module therefore publishes a crate whose code did not change. The contract tests and full-request test servers asked for on #66 and #21 waited in #88 for that reason.
- **Proposed solution:** move unit-test modules into `src/**/*_tests.rs` files included with `#[cfg(test)] #[path = ...]`, as `src/transport_noise_tests.rs` already is, and have the planner in `auto-release.yml` (and `release_guard.py`) leave `*_tests.rs` files out when it looks for release-relevant changes since the latest tag. Every other file under `src/`, `Cargo.toml` and `LICENSE` stays release-relevant, so a push that changes a test file and the manifest still releases.
- **Impact:** test-only fixes merge when they are written. It costs a one-time move of the test modules and one more rule in the release planner.

## 8. Browser-neutral messages for the reused sign-in outcomes

- **Problem:** `browser_login` returns `DeviceAuthorizationExpired` for a browser that did not come back, and its message tells the person to call `login_with_browser` again for a new code (`AUDIT.md` item 7).
- **Proposed solution:** in the next minor release, reword both unit variants' messages so that they fit either flow ("the sign-in was not approved in time; start a new sign-in"), recorded in the changelog as a message change, with every SDK's wording checked at once.
- **Impact:** clearer errors for the person signing in; any caller that matched on the message text (not the variant) would notice.

## 9. One copy of the conformance recorder in the library's test binary

- **Problem:** `src/transport_noise_tests.rs` and `src/loopback_tests.rs` each load `tests/common/mod.rs` with `#[path]`, so the second needs `#[allow(clippy::duplicate_mod)]`.
- **Proposed solution:** declare it once, as a crate-level `#[cfg(test)] mod` in `src/lib.rs`, and `use` it from both suites; refresh the evidence of every capability whose files that touches, after re-running the suite.
- **Impact:** one recorder per binary, no lint exception; a hash refresh across several capabilities.

## 10. Drop the old reference digest after the Python merge

- **Problem:** `reference_digests` in `contracts/sdk-parity.json` carries both `57c7e71f6456…` and `6dfb3bb31ac7…` for the browser-login rollout.
- **Proposed solution:** once thalovant-python-sdk `main` carries `6dfb3bb31ac7…` (#113 merged), remove `57c7e71f6456…` in a follow-up, as the porting brief says.
- **Impact:** the acceptance names only references that are still current.

## 11. A timeout for every control-plane request

- **Problem:** only a browser sign-in's two requests are bounded (10 s); every other `ControlPlane` call waits as long as the API takes (`AUDIT.md` item 10), where the Python reference bounds every request at 10 s.
- **Proposed solution:** a client-wide default request timeout on the reqwest client built in `ControlPlane::new`, with a way to change it, noted in the changelog as a behaviour change, alongside the other SDKs.
- **Impact:** a stalled API cannot hold any call for good; a call that legitimately takes longer (a large inventory) would need the knob.
