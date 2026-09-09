# Changelog

## 0.5.0

- Share one absolute timeout across every intent-description window, retain earlier answers, and stop publishing after the budget expires.

- Add `listen`, `wait_for_event`, `ListenOptions`, and `EventStream::recv` with scoped correlation, total deadlines, predicate filters, limits, cancellation-safe subscription ownership, and explicit overflow or disconnect errors.
- Refuse control-plane redirects, including 307/308 login-body replay, require HTTPS for authenticated requests and request bodies outside explicit loopback development endpoints, and remove request URLs from transport errors.
- Validate device verification URLs before prompting or launching a browser; allow only HTTP(S) without embedded credentials and use direct platform commands without a shell.
- Flush complete MQTT broker-fixture packets before waiting for more input, with a buffered-writer regression and native TLS coverage on Windows.

- Raise the minimum Rust version from 1.85 to 1.88 and upgrade `time` to 0.3.55 to fix RUSTSEC-2026-0009 (RFC2822 parser stack exhaustion). HTTP cookie affinity requires this dependency; upgrade the compiler before upgrading the crate.

- Preserve complete Noise identities with atomic publication and OS locks across processes. Interrupted writers cannot publish partial keys, pin transactions cannot lose another process's changes, and malformed or exposed trust files remain errors without automatic reset. Bound OS lock waits and move handshake/store work off Tokio workers so caller deadlines remain responsive.
- Join concurrent connections only after authenticated Noise readiness. A joining caller owns its own deadline; cancellation of the initiating caller retires only that connection generation. Bound transport writes and the caller's cleanup wait, retain cleanup workers after cancellation, abort taken MQTT tasks safely, and retain unacknowledged HTTP admission for a later cleanup attempt.
- Include connection, send, and response collection in Ask/Query deadlines. Add `AskOptions` and `ask_with_options` for delayed speech and fragment collection, recover soft intent misses when speech arrives, and report empty completion as a timeout. Existing options struct literals remain compatible.
- Fall back to engine manifests when intent listing is denied or silent. Add `intents_with_capabilities`, `HubIntentCapabilities`, `HubFallback`, and `list_fallbacks`; distinguish unknown fallback support from a confirmed empty list and expose conservative `may_answer(language)`. The legacy `denied` field names an unavailable query and is not proof of an ACL denial.
- Exercise stable Rust on Linux, macOS, and Windows, test the declared Rust 1.88 minimum, and audit freshly resolved dependencies in CI. Add failure, cancellation, concurrency, interrupted-write, and fallback-discovery regressions.

## 0.4.8

- Drive WSS, HTTP, and MQTT through one Noise handshake, authenticated framing, and hub trust implementation. Preserve WSS writer ordering and cancellation poisoning, and keep HTTP/MQTT lifecycle ownership unchanged.
- Reuse the protected persisted PSK cache across all runtime transports. A rejected or abandoned unfinished handshake discards the derived cache entry while preserving the authenticated hub pin, so the next connection can derive from the current password.
- Share transport health reporting and validate cached-credential recovery, changed-peer rejection, and a canceled chunked WSS send followed by a fresh authenticated reconnect.

## 0.4.7

- Correct the declared minimum Rust version to 1.85, already required by the
  Noise cryptography dependencies. Rust 1.75 was an inaccurate compatibility
  claim in earlier package metadata.
- Keep HTTP cookie and URL dependencies on releases compatible with Rust 1.85,
  so a fresh dependency resolution does not unexpectedly require Rust 1.88.
- Test all targets and features on Rust 1.85.0 in CI as well as current stable.

## 0.4.6

- Implement the deployed HiveMind v3 Noise handshake for HTTP and MQTT. Offers
  alone no longer mark a transport ready, and all post-handshake HELLO/bus
  traffic is encrypted, including chunked messages and `encrypt=false` calls.
- Reset a previously admitted HTTP peer before reconnecting after failure;
  first connections never evict an unknown peer.
- Preserve HTTP replica cookies, use binary form/poll endpoints, reject redirects
  and JSON error responses, and expose a builder for custom HTTP trust roots.
- Add persistent Noise state directory and remote static key methods to HTTP
  and MQTT; preserve hub pins after authentication failures on every transport.
- Reset WSS cipher, handshake, HELLO and node state before same-object reconnects,
  join old readers, reject unauthenticated bus events, and decode authenticated
  binary bus frames. Interrupted cipher sends invalidate session readiness.
- Serialize MQTT ciphertext delivery, handle full-size Noise frames, and keep
  polling the broker while the ordered protocol worker exchanges messages.
  Broker failures require an explicit reconnect with a new session. Add TLS
  trust configuration and random broker connection IDs.
- Exercise real local TLS HTTP/MQTT and WebSocket responders, XX-to-KK reconnects,
  correlated encrypted replies, concurrent chunked messages, wrong credentials,
  unsupported offers, tampering, plaintext rejection, and changed-key refusal.

## 0.4.0

- **Breaking.** `wss` connections now perform the HiveMind v3 Noise handshake,
  and only that. A HiveMind-core 5.x hub accepts no other key exchange, so this
  release requires one; against an older hub the connection is refused rather
  than downgraded. `Noise_XXpsk2_25519_ChaChaPoly_SHA256` on first contact and
  `Noise_KKpsk0_...` once the hub's static key is pinned, with
  `25519_AESGCM_SHA256` supported where a hub prefers it.
- **Breaking.** `Identity::crypto_key` is gone, along with the whole `crypto`
  module (`encrypt_as_json`, `decrypt_from_json`, `encrypt_as_binary`,
  `decrypt_binary`, `runtime_crypto_key`) and the pre-shared handshake on all
  three transports. Hubs no longer issue a crypto key and v3 derives its
  pre-shared key from the `password`, so the field named a credential that no
  longer exists. `crypto_key` is still accepted and ignored when parsing an
  older identity file, and stays in the bootstrap redaction list so an older
  payload carrying one does not leak it. `https` and `mqtt` now rely on TLS for
  confidentiality, as they already did for everything the crypto key did not
  cover.
- The Noise pre-shared key is derived from `password` with argon2id
  (`t_cost=3`, 64 MiB, `p_cost=1`), salted with SHA-256 of the hub's node id. A
  transport caches it per hub, so only the first connection pays the few
  hundred milliseconds.
- New `noise` and `noise_store` modules: `derive_psk`, `canonical_json`,
  `select_noise_options`, `NoiseHandshake`, `NoiseSession`, and the on-disk
  state helpers `load_or_create_noise_key`, `load_noise_pin`, `save_noise_pin`
  and `forget_noise_pin`.
- Three files persist beside the SDK config file, all `0600`: `noise_key` (this
  client's static X25519 key), `noise_pins.json` (the hub keys it has pinned),
  and `noise_psks.json` (cached derived credentials indexed by hub node ID).
  `WssTransport::set_noise_state_dir` overrides the location; `forget_cached_psk`
  removes one derived credential without removing keys or pins.
- Trust on first use: the first hub key seen for a node id is pinned, and a
  later connection presenting a different key is refused with an error naming
  `forget_noise_pin`, rather than silently re-pinned. A failed `KKpsk0`
  handshake drops the stale pin, because `KK` needs each side to hold the
  other's key and the failure is as likely to mean the hub no longer has this
  client's.
- `WssTransport::remote_static_key` reports the hub's static key for the
  current session.
- A `wss` connection the hub refuses now fails with the close reason instead of
  running out the handshake clock: a wrong password reported as a timeout hid
  what had actually happened.
- The WSS transport takes the writer lock *before* encrypting. `encrypt_message`
  advances the cipher state nonce counter and the hub decrypts strictly in
  counter order, so encrypting outside the lock let two concurrent senders
  consume nonces in one order and reach the wire in the other -- which the hub
  treats as tampering and drops the session for.
- The MQTT handshake no longer requires the legacy `preshared_key` capability
  flag, which a v3 hub does not set. Requiring it rejected the handshake and
  left `connect()` to time out.
- Trust on first use moved into `noise_store::pin_hub_key`, which holds the pin
  lock across the read and the write. Checking for a pin and then writing it as
  separate calls was the race itself.
- The static key is created with `create_new` straight at its final path. The
  pin lock only covers one process, so two processes could each generate a key
  and the rename would leave one of them holding a key the file does not
  contain -- and the hub pins what it was shown, so that client would be
  refused for good. Losing the create now reloads the winner's key. Pin-map
  updates remain process-local; see the note in `noise_store`.
- Noise state files are written to a uniquely named temporary file created
  `0600` with `create_new` and renamed into place. A truncating write left the
  pin file empty on failure, which reads back as "no pins" and would silently
  re-pin whatever key the next connection was offered.
- **Breaking.** `HttpTransport::connect` now refuses a hub endpoint that is not
  `https://`, for the same reason as the MQTT change below: TLS is the only
  confidentiality left on that hop, and the access key travels in the
  `authorization` query.
- `create_client_identity` drops `cryptoKey` and `crypto_key` from a
  caller-supplied `opts.spec` rather than passing them through. The error
  redaction covers only what the SDK mints, so a legacy value left in by a
  caller could otherwise be echoed back inside a `ThalovantError::Api`.
- **Breaking.** The MQTT transport now refuses a broker whose identity does not
  enable TLS. Removing the crypto key took the separate payload cipher with it,
  so TLS is the only confidentiality left on that hop; without it every message
  and the broker password would travel in the clear. Use an `mqtts://`
  endpoint, or set `tls: true` on the identity's `mqtt` block.
- Messages larger than one Noise transport message are chunked at 65000 bytes
  and reassembled by the peer, with reassembly capped at 32 MiB. Any transport
  message that fails to decrypt, and any malformed chunk sequence, drops the
  session rather than the frame.
- `HiveMessage` derives `Default`.

## 0.3.1

- `list_intents` (and so `intents`) returns `ThalovantError::Runtime` carrying the hub's `error` text when the hub answers `ovos.intent.list` with `ok: false`, instead of reading the missing `intents` key as an empty list. A refused listing is not an empty hub, and reporting it as no intents showed a person a device that can do nothing; the engine-manifest fallback still answers a `hive.policy.denied` refusal only, since a failed query is not evidence the connection lacks the type. `describe_intent` keeps returning an empty list for `ok: false`, which is a real answer: the hub does not know that registration, so the intent simply has no sentences. Reported by the Kotlin port's review.
- Documentation: `ovos.intent.list` is always needed; `ovos.intent.describe` only when the client has to ask for the definitions itself -- `describe` is on (the default) *and* the runtime did not attach each row's `definition` to the listing. A runtime that honours `include_definitions` is sent no describe at all. `ThalovantError::PolicyDenied`'s `allowed` already kept string entries only -- the other half of the Kotlin port's report -- and a test now pins it.

## 0.3.0

- Add the intent inventory: `Client::intents(languages, IntentInventoryOptions)` reads the hub runtime's intent manifest (OVOS-INTENT-4 §10) over the client's own session and returns a `HubIntentInventory` — every intent each skill registered, per language, with the sentences a person says to reach it as the skill's locale files wrote them, `{slot}` placeholders included. No control-plane credential is involved. `Client::list_intents(lang, IntentListOptions)` and `Client::describe_intent(skill_id, intent_name, lang, IntentDescribeOptions)` expose the two underlying queries (`ovos.intent.list` / `ovos.intent.describe`).
- New public types `HubIntentInventory`, `HubSkillIntents`, `HubIntent`, `IntentRegistration`, `IntentDefinition`, `IntentInventorySource`, `IntentInventoryOptions`, `IntentListOptions`, `IntentDescribeOptions`, constants `DEFAULT_INTENT_TIMEOUT` and `DESCRIBE_BATCH`, `intents::same_language`, and the event-name constants `EVENT_INTENT_LIST`, `EVENT_INTENT_LIST_RESPONSE`, `EVENT_INTENT_DESCRIBE`, `EVENT_INTENT_DESCRIBE_RESPONSE`, `EVENT_ADAPT_MANIFEST_GET`, `EVENT_ADAPT_MANIFEST`, `EVENT_PADATIOUS_MANIFEST_GET`, and `EVENT_PADATIOUS_MANIFEST`. `HubIntentInventory`, `HubSkillIntents`, and `HubIntent` serialise (and `as_value()`) to the same JSON the other SDKs print.
- Queries are correlated by `context.request_id` like every other request, and a reply delivered more than once is taken once. Describes are sent together and matched by request id, or by the definition's own `skill_id`/`intent_name`/`lang` for a hub that does not echo the id. A describe that never comes leaves that intent without sentences instead of failing the inventory. Language tags compare case-insensitively with `_` and `-` folded: `intents(languages)` trims each tag and asks the hub once per language whatever its spelling (`en-us`, `en-US`, `en_us` are one), keeping the first spelling given in `languages`. `has_phrases()` is true only when at least one intent carries at least one sentence. An intent registered under both engines in one language keeps the template row's sentences whichever order the rows arrive in, and the first row seen names its `engine`; on the names-only fallback the first engine to name an intent decides likewise (adapt is asked before padatious). These are the four points the reference settled in Python SDK 0.4.37, so every SDK reads the same.
- **BREAKING:** `ThalovantError` is now `#[non_exhaustive]`, so a caller matching on it needs a wildcard arm and a later release can add a failure without breaking anyone. This release adds one such variant, which is why it is a minor bump and not a patch: code that matched `ThalovantError` exhaustively must add `_ => ...`.
- Add `ThalovantError::PolicyDenied { denied_type, code, reason, allowed }`, returned at once from the hub's `hive.policy.denied` instead of waiting for a timeout. `IntentInventoryOptions::fallback` (on by default) falls back to the engines' own manifests (`intent.service.adapt.manifest.get` / `intent.service.padatious.manifest.get`) when `ovos.intent.list` is refused; the result then carries names only, `source` of `IntentInventorySource::EngineManifests`, and `denied` naming `ovos.intent.list`.
- A runtime that attaches each row's `definition` to `ovos.intent.list` when asked with `include_definitions` is used as such; one that does not is described row by row, at most `DESCRIBE_BATCH` (32) describes in flight at a time. Each batch is its own subscription window with its own deadline, so a large inventory cannot outrun the transports' 64-slot bus channel and lose replies, the hub is never sent the whole burst at once, and a hub that answers nothing fails after one batch instead of holding every request open. A partial answer is an answer across windows as within one: a window nothing answered contributes nothing rather than discarding the windows that did answer, and the describe fails only when no window produced anything, so a hub silent from the start still fails at the first window.

## Unreleased

- Security: redact secrets from every `Debug`/`{:?}` rendering. `Identity`, `MqttBrokerCredentials`, `control::LoginOptions`, `control::DeviceAuthorization`, `control::BootstrapIdentityResult`, `events::Event`, and `events::Reply` now use hand-written `Debug` implementations that redact credentials — access key, password, crypto key, MQTT username/password, MFA `otp_code`/`recovery_code`, device code, secret-keyed `Identity::metadata` entries, and the end-user bearer token duplicated into `context.auth_token` and `context.auth.token`. `Serialize`/`Deserialize` are **unchanged**, so identity-file persistence and the wire protocol still emit the real values.
- Security: `BootstrapIdentityResult::as_value(false)` now redacts the credential subkeys in the `hub` and `client` resources (for example the `apiKey`, `password`, and `cryptoKey` minted by `POST /v1/clients`, and the `initial_identify.key` access-key alias) as well as secret-keyed `Identity::metadata`, matching how it already redacted the identity block. `as_value(true)` still returns the real values for persistence.
- Security: strip the request URL from stored `reqwest` errors. Data-plane request URLs carry the caller's access key in a `?authorization=` query, which reqwest's `Display` appended as " for url (...)"; that URL is no longer copied into `TransportHealth::last_error` or any rendered `ThalovantError::Http`.
- Security: bound and redact the server response body interpolated into `ThalovantError::Api` messages for `/v1/auth/token`, `/v1/auth/device/token`, and `/v1/clients`. Errors now carry the HTTP status plus a short, single-line, secret-redacted detail instead of the raw body.
- Security: redact `MqttBrokerCredentials::topic_prefix` from `Debug`/`{:?}`. Since the MQTT migration the prefix is `hivemind/<hub-id>/<access-key>`, so it embedded the account access key; its hand-written `Debug` now prints `<redacted>` for the field. `Serialize` still emits the real prefix for persistence and the wire protocol.
- Harden `mqtt_topics_for_identity` topic_prefix validation: trim surrounding whitespace as well as slashes (whitespace-only prefixes like `"/ \t/"` now report the existing "must include topic_prefix" error instead of generating whitespace-only topics), and reject prefixes containing an MQTT wildcard (`#`/`+`) or an ASCII control char (`< 0x20`, incl. NUL) with a new "MQTT topic_prefix contains characters that are not valid in an MQTT topic." error.
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
