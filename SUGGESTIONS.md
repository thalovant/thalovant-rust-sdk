Last Edit: Claude Opus 5.5 - 2026-11-03 - Motive: Created with suggestions from the chat apps change (0.16.0).

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
