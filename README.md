# Thalovant Rust SDK

Rust SDK for connecting services, CLIs, devices, and agents to Thalovant hubs.

The control API is used to discover hubs and provision a client identity. After
that, the SDK talks directly to the hub data plane over HTTPS, WSS, or MQTTS.

Full docs: <https://docs.thalovant.com/developers/sdks/rust/>

## What You Need

- Rust 1.88 or newer (tested in CI alongside the current stable compiler).
  Version 0.5.0 raises this minimum from 1.85 because the patched `time`
  dependency required for HTTP replica cookies needs Rust 1.88. Upgrade your
  compiler before upgrading the crate.
- A Thalovant account with API access for authenticated control-plane actions.
- A hub id or slug.
- A client identity for that hub. You can create one through the API or use one
  downloaded from the dashboard.

## Install

```bash
cargo add thalovant
```

## Quick Start

```rust
use thalovant::{
    BootstrapIdentityOptions, Client, ControlPlane, HubProtocol, RequestOptions,
};

#[tokio::main]
async fn main() -> thalovant::Result<()> {
    let mut control = ControlPlane::default();

    // Public hub discovery does not require auth.
    let public_hubs = control.list_public_hubs(Some(12), None).await?;
    if let Some(items) = public_hubs.get("data").and_then(|value| value.as_array()) {
        for hub in items {
            println!(
                "{} {} {}",
                hub.get("id").and_then(|value| value.as_str()).unwrap_or(""),
                hub.get("slug").and_then(|value| value.as_str()).unwrap_or(""),
                hub.get("title").and_then(|value| value.as_str()).unwrap_or("")
            );
        }
    }

    // Auth is required when creating a client identity.
    control.login("you@example.com", "password", None).await?;

    let result = control
        .create_client_identity_for_hub_id(
            "hub-id",
            BootstrapIdentityOptions {
                name: "rust-demo-client".into(),
                preferred_protocols: vec![HubProtocol::Wss, HubProtocol::Https, HubProtocol::Mqtt],
                ..Default::default()
            },
        )
        .await?;

    let client = Client::with_protocol(result.identity, HubProtocol::Wss)?;
    let info = client.connect_with_info().await?;
    println!("connected in {:?} ms", info.connect_ms);

    let reply = client
        .ask("Tell me a short clean joke.", RequestOptions::default())
        .await?;
    println!("{}", reply.text);
    client.close().await?;

    Ok(())
}
```

`ControlPlane::default()` uses `https://api.thalovant.com`. Use
`ControlPlane::new(...)` only for local development or a self-hosted control plane.
Control-plane requests never follow redirects: configure the final API URL.
Authenticated requests and requests with a body require HTTPS. HTTP is allowed
only for explicit `localhost`, `127.0.0.1`, and `[::1]` development endpoints.
Credentials embedded in the API URL are refused, and HTTP transport errors omit
the request URL.

## MFA Login

Accounts with multi-factor authentication enabled reject a plain
`control.login(...)` with HTTP 401 and `"code": "mfa_required"`. Pass the
current TOTP code, or a recovery code, with `login_with_options`:

```rust
use thalovant::LoginOptions;

control
    .login_with_options(
        "you@example.com",
        "password",
        LoginOptions {
            otp_code: Some("123456".into()),
            ..Default::default()
        },
    )
    .await?;
```

Use `recovery_code: Some(...)` instead of `otp_code` when the authenticator
device is unavailable. Both fields are only sent when set, so
`login_with_options` with a default `LoginOptions` behaves exactly like
`login` without a scope.

## Browser (Device) Login

Accounts without a password (for example Google sign-in) sign in through the
browser device flow. `login_with_browser` prints a verification URI and a
short user code, opens the browser (best effort, override with
`open_browser: false`), and waits until the request is approved in the
browser:

```rust
use thalovant::DeviceLoginOptions;

let token = control
    .login_with_browser(DeviceLoginOptions {
        scopes: vec!["hubs:read".into(), "clients:write".into()],
        client_name: Some("my-tool".into()),
        ..Default::default()
    })
    .await?;
println!("signed in, token scopes: {}", token["scopes"]);
```

On approval the returned `access_token` is a durable scoped API token stored
on `control.access_token`, exactly like `login`. Leave `scopes` empty to let
the server apply its defaults; the server may normalize or expand the scopes
it echoes back. Pass `prompt: Some(Box::new(|grant| ...))` to present the
`DeviceAuthorization` (verification URI, user code) yourself instead of the
stdout print, and `timeout: Duration::from_secs(...)` to change the default
15-minute wait.

Failures are typed: `ThalovantError::DeviceAuthorizationDenied` when the
request is rejected in the browser, `ThalovantError::DeviceAuthorizationExpired`
when the code expires first (call `login_with_browser` again for a new code),
and `ThalovantError::Timeout` when `timeout` elapses.

## CI: Direct API Token Auth

Non-interactive environments should skip login entirely and construct the
control plane with a pre-provisioned API token (for example one issued through
the device flow or the dashboard) kept in a secret such as
`THALOVANT_API_TOKEN`:

```rust
use thalovant::ControlPlane;

let token = std::env::var("THALOVANT_API_TOKEN").expect("THALOVANT_API_TOKEN is set");
let control = ControlPlane::with_access_token(token);
// Ready for authenticated calls, no login step needed.
let hubs = control.list_hubs(Some(50), None, None).await?;
```

Use `ControlPlane::new(api_url, Some(token))` instead when targeting a local
or self-hosted control plane.

Keep `result.identity` secret: it holds the client credentials the hub uses.
`result.as_value(true)` embeds those real credentials (access key, password,
crypto key) *and* the raw `hub`/`client` bodies (which include the `apiKey`,
`password` minted by `POST /v1/clients`), so never log it or
write it anywhere world-readable. For diagnostics use `result.as_value(false)`,
which redacts every credential in the identity (including secret-keyed
`metadata` entries) and in the hub/client bodies (including the
`initial_identify.key` access-key alias), or `{:?}`, which redacts the same
fields; neither exposes a secret.

## List Your Hubs

Authenticated accounts can list owned or visible hubs:

```rust
let mut control = ControlPlane::default();
control.login("you@example.com", "password", None).await?;

let page = control.list_hubs(Some(50), None, None).await?;
if let Some(items) = page.get("data").and_then(|value| value.as_array()) {
    for hub in items {
        println!(
            "{} {} {}",
            hub.get("id").and_then(|value| value.as_str()).unwrap_or(""),
            hub.get("slug").and_then(|value| value.as_str()).unwrap_or(""),
            hub.get("title").and_then(|value| value.as_str()).unwrap_or("")
        );
    }
}
```

## Provision Hubs

Hubs, runtime groups, and skills can be created and managed from code. These
routes need a **paid plan** and a token with the **`hubs:write`** scope
("Create and update your hubs" on the dashboard's API Tokens page). A free-plan
token fails with HTTP 402 `API access requires a paid plan.`, and a token
without the scope fails with HTTP 403 `Insufficient scopes`; both surface as
`ThalovantError::ApiResponse` carrying the status and redacted detail.

```rust
use serde_json::json;
use thalovant::{ControlPlane, MarketplaceSkillsOptions, ReleaseOptions, SkillInstallOptions};

let control = ControlPlane::with_access_token(std::env::var("THALOVANT_API_TOKEN")?);

// 1. Discover what is installable before provisioning anything. Browsing the
//    catalog only needs `hubs:read` and is not paid-gated.
let catalog = control
    .list_marketplace_skills(MarketplaceSkillsOptions::default())
    .await?;
if let Some(skills) = catalog["data"].as_array() {
    for skill in skills {
        println!("{} {}", skill["skill_id"], skill["access_tier"]);
    }
}

// 2. Create a runtime group to run the skills.
let group = control
    .create_runtime_group(json!({"name": "kiosks", "description": "Lobby kiosks"}))
    .await?;
let group_id = group["id"].as_str().unwrap_or_default().to_string();

// 3. Create a hub attached to it.
let hub = control
    .create_hub(
        json!({
            "name": "joke-garden",
            "runtime_group_id": group_id,
            "spec": {"protocols": {"wss": {"enabled": true}}},
        }),
        None,
    )
    .await?;
let hub_id = hub["id"].as_str().unwrap_or_default().to_string();

// 4. Install a skill from the marketplace catalog.
control
    .install_runtime_group_skill(&group_id, "skill-weather", SkillInstallOptions::default())
    .await?;

// 5. Release: roll the runtime and the hub onto a release channel.
let channel = || ReleaseOptions {
    channel: Some("stable".into()),
    ..Default::default()
};
control.release_runtime_group(&group_id, channel()).await?;
control.release_hub(&hub_id, channel()).await?;
```

For safe `create_hub` retries, choose one idempotency key before the first
attempt and pass the same `Some(key)` and payload for retries of that operation.
`None` generates a fresh
`Idempotency-Key` per call; repeating it after a timeout can create a second
hub. No other hub provisioning route reads that header.

Updating and deleting a hub use optimistic locking, so `etag` is a required
argument rather than an option. Pass the `etag` from the hub resource you read
— it lives in the JSON body, not in an `ETag` response header — and the SDK
sends it as `If-Match`. The API rejects a stale value with HTTP 412 without
changing anything. Empty or whitespace-only values fail locally with
`ThalovantError::Api` before any request is sent:

```rust
let hub = control.get_hub(&hub_id).await?;
let etag = hub["etag"].as_str().ok_or("hub response has no etag")?;
let hub = control
    .update_hub(&hub_id, json!({"active": false}), etag)
    .await?;
control
    .delete_hub(&hub_id, hub["etag"].as_str().ok_or("hub response has no etag")?)
    .await?;
```

Deleting a hub also deletes its clients and ACLs. Runtime groups have no
`If-Match` requirement and read no idempotency header, but the API refuses to
delete the workspace default group or a group that still has hubs attached
(HTTP 409).

Runtime configuration is deep-merged using a revision precondition, and `personas` is sent only when
you pass `Some(..)`:

```rust
control
    .update_runtime_group_config(&group_id, json!({"lang": "en-us"}), None)
    .await?;
println!("{}", control.get_runtime_group_config(&group_id).await?["config"]);
```

Rating a public hub is the exception to the paid gate: `set_hub_rating` and
`clear_hub_rating` need `hubs:write` but **no paid plan**. Only public hubs can
be rated, and owners cannot rate their own.

Reading what a hub is actually running needs the **`hubs:inspect`** scope
instead:

```rust
let capabilities = control.get_hub_runtime_capabilities(&hub_id).await?;
println!("{}", capabilities["counts"]["total_intents"]);
```

## Discover Skills

The marketplace catalog is readable with the **`hubs:read`** scope and, unlike
the provisioning routes above, is **not paid-gated** — a free-plan token can
browse the whole catalog before upgrading, and only the install needs a paid
plan.

Each entry carries what an install needs (`skill_id`, `source_type`,
`source_ref`, `config_schema`, `secret_schema`) next to presentation fields
(`title`, `summary`, `tags`, `verified`). Admin tokens can additionally set
`owner_id` to read another tenant's catalog and `include_inactive` to see
retired entries; both are silently ignored for non-admin callers rather than
rejected. `force_refresh` re-syncs the global catalog from source first, which
is slower and is open to every caller.

Two group-scoped reads need the **`hubs:inspect`** scope and are likewise not
paid-gated. The first resolves the catalog against one runtime group, so each
entry reports whether it is already desired, whether it was observed running,
and whether the tenant plan allows installing it:

```rust
let view = control.list_runtime_group_marketplace(&group_id, false).await?;
if let Some(entries) = view["data"].as_array() {
    for entry in entries {
        if entry["installable"] == json!(true) && entry["active"] != json!(true) {
            println!("available: {}", entry["skill_id"]);
        }
    }
}
```

The second answers what the group is actually running right now, rather than
what could be installed:

```rust
let inventory = control.list_runtime_group_inventory(&group_id, true).await?;
println!("{} {}", inventory["source"], inventory["data"].as_array().map_or(0, Vec::len));
```

Both answer from a cached inventory snapshot by default; pass `true` for
`refresh_inventory` / `refresh` to force a live read from the runtime operator.
When nothing is reporting yet these two return an empty `data` list with a
pending `source` (`ovos-runtime-operator-pending`) rather than failing —
`get_hub_runtime_capabilities` is the one that answers HTTP 409 in that case.

## Workspace Analytics

Authenticated accounts can read the same overview used by the dashboard:

```rust
let overview = control
    .get_analytics_overview(thalovant::AnalyticsOverviewOptions {
        range: Some("7d".into()),
        hub_id: Some("hub-id".into()),
        ..Default::default()
    })
    .await?;
println!("{}", overview["totals"]);
```

## Durable Memory

Private Daily Desk and workspace assistants can manage explicit opt-in memory:

```rust
let memory = control
    .create_memory_item(serde_json::json!({
        "scope": "workspace",
        "kind": "preference",
        "content": "Prefer America/Toronto for scheduling.",
        "tags": ["timezone"],
    }))
    .await?;
println!("{}", memory["id"]);

let items = control
    .list_memory_items(thalovant::MemoryListOptions {
        scope: Some("workspace".into()),
        query: Some("timezone".into()),
        ..Default::default()
    })
    .await?;
println!("{}", items["data"]);
```

## What Can I Ask?

A connected client can ask its hub what it can be asked, over its own session,
with no control-plane token:

```rust
use thalovant::{Client, IntentInventoryOptions};

let client = Client::from_file("_identity.json")?;
let inventory = client
    .intents_with_capabilities(["en-us", "fr-fr"], IntentInventoryOptions::default())
    .await?;
for skill in &inventory.inventory.skills {
    for intent in &skill.intents {
        println!("{} {:?}", intent.id(), intent.examples(Some("fr-fr"), 2));
    }
}
println!("French may be answered: {}", inventory.may_answer("fr-fr"));
println!("{}", serde_json::to_string_pretty(&inventory)?);
```

Each intent carries the sentences a person says to reach it, per language, as
the skill wrote them (`{location}` marks a slot); `examples` shows whole
sentences before ones with a slot. The preferred listing query is
`ovos.intent.list`. `ovos.intent.describe` is needed only when the
client has to ask for the definitions itself -- that is, `describe: true` (the
default) *and* a runtime that did not attach each row's `definition` to the
listing; a runtime that honours `include_definitions` is sent no describe at
all. A hub that refuses answers `hive.policy.denied`, which the
SDK returns at once as `ThalovantError::PolicyDenied` naming the type and the
types the connection may publish. With the default `fallback: true`, a denied
or silent `ovos.intent.list` falls back to the engines' own manifests: the inventory then
lists intent names only, with `source` set to
`IntentInventorySource::EngineManifests` and `denied` naming the unavailable query.
That legacy field is not proof of an ACL denial: a silent listing also records it.
A listing the hub answers `ok: false` is a different thing -- the query failed,
which is not an empty hub and not a refusal to fall back from -- and returns
`ThalovantError::Runtime` carrying the hub's own wording.

`intents_with_capabilities` additionally asks `ovos.skills.fallback.list` with a
separate budget of at most 1.5 seconds. `fallbacks_known: false` means the hub
could not report its fallback skills; `true` with an empty list means it reported
none. `may_answer(lang)` is conservative: enabled phrases for that language,
registered fallbacks, or unknown fallback support allow a request. It does not
guarantee an answer. `list_fallbacks` exposes that optional query directly.
The existing `intents` method still returns `HubIntentInventory` unchanged in
shape; use the enriched method when deciding language availability.

`list_intents(lang, IntentListOptions)` returns the manifest rows for one
language and `describe_intent(skill_id, intent_name, lang, IntentDescribeOptions)`
the registrations behind one intent, sentences included, for callers that want
the two underlying queries. A large inventory is described in batches of
`DESCRIBE_BATCH` (32) requests, so a hub with hundreds of intents neither
outruns the bus channel nor receives the whole burst at once. All describe
windows share one `IntentInventoryOptions::timeout` budget, including sends and
reply collection. When that budget expires, earlier definitions remain and no
later window is sent. Listing queries, initial connection, and the optional
fallback probe have separate budgets.

## Use An Existing Identity

For local development, store one or more identities in the protected SDK config:

```bash
mkdir -p ~/.config/thalovant
chmod 700 ~/.config/thalovant
$EDITOR ~/.config/thalovant/config.yaml
chmod 600 ~/.config/thalovant/config.yaml
```

```yaml
profile: prod
profiles:
  prod:
    identity:
      access_key: ...
      password: ...
      site_id: demo-agent
      default_master: https://jokes.thalovant.io
      data_plane_endpoints:
        wss: wss://jokes.thalovant.io/public
        https: https://jokes.thalovant.io/public
        mqtt: mqtts://mqtt.thalovant.com:8883
      mqtt:
        endpoint: mqtts://mqtt.thalovant.com:8883
        username: ...
        password: ...
        topic_prefix: hubs/hub-id/clients/client-id
        tls: true
```

```rust
use thalovant::{Client, RequestOptions};

let client = Client::from_config(Some("prod"))?;
let reply = client
    .ask("What can this hub do?", RequestOptions::default())
    .await?;
println!("{}", reply.text);
client.close().await?;
```

SDKs reject config files that are readable or writable by other users on Linux
and macOS. Keep this file out of git.

Raw identity files are supported too:

```rust
let client = Client::from_file("_identity.json")?;
```

Environment variables are supported too:

```rust
let client = Client::from_env()?;
```

## Protocols

Hubs may expose one or more public data-plane protocols:

- `wss`: secure realtime WebSocket, the default public path and SDK preference.
- `https`: request/response HTTP protocol exposed as HTTPS.
- `mqtt`: broker-mediated MQTT over TLS. Requires per-client broker credentials.

### Transport Security

`wss`, `https`, and `mqtt` connections perform the HiveMind **v3 Noise handshake**
(`Noise_XXpsk2_25519_ChaChaPoly_SHA256`, or `KKpsk0` once the hub's static key
is pinned). It is the only key exchange a HiveMind-core 5.x hub accepts: there
is no pre-shared `crypto_key` any more, no cleartext path, and a connection
that cannot complete the handshake never becomes ready. A WebSocket refusal
may close with code `1008`.

Nothing extra has to be provisioned. The Noise pre-shared key is derived from
the identity `password` with argon2id, salted with the hub's node id, so an
identity that can authenticate can already handshake.

Three files persist beside the SDK config file (`~/.config/thalovant` unless
`XDG_CONFIG_HOME` or `%APPDATA%` says otherwise), all `0600`:

- `noise_key` — this client's static X25519 key. It has to persist: a hub pins
  it on first contact, so regenerating it makes the client look like a
  different peer and the hub refuses it.
- `noise_pins.json` — the hub static keys this client has pinned.
- `noise_psks.json` — cached derived Noise credentials, indexed by hub node ID.
  Protect it like a password file; `forget_cached_psk` removes one entry.

Saved pins and pin-writing helpers require a nonempty node ID and exactly 64
hexadecimal characters (32 bytes). Malformed trust is rejected before any
rewrite. `save_noise_pin` now enforces its first-contact contract: an identical
key is idempotent; a conflicting key requires verified rotation through
`forget_noise_pin`. It cannot silently replace a saved decision. Hexadecimal case does not change
key identity; idempotent checks preserve the existing saved bytes.

Use `set_noise_state_dir` on `WssTransport`, `HttpTransport`, or `MqttTransport`
to select another persistent directory. Keep the same directory when switching
transports with one identity; regenerating the key breaks the hub's client pin.

The first connection to a hub trusts the key it presents and records it. A
later connection presenting a different key is **refused**, because the SDK
cannot tell a reinstalled hub from another machine answering at the same
address. If the hub really was replaced, clear the pin deliberately:

```rust
use thalovant::forget_noise_pin;

forget_noise_pin(None, &node_id)?;
```

Argon2id uses 64 MiB for derivation. WSS, HTTP, and MQTT share the protected
persisted PSK cache, indexed by hub node ID. A cached PSK is itself a credential:
changing only the identity password does not replace a cached key the hub still
accepts. Failed handshake authentication or an abandoned unfinished handshake
(including peer closure or timeout) evicts that derived entry, so the next
connection derives from the current password. Client static keys and trusted
hub pins are retained.

To deliberately derive again, remove only the PSK cache entry before reconnecting:

```rust
use thalovant::forget_cached_psk;

forget_cached_psk(None, &node_id)?; // Or Some(state_dir.as_path()).
```

Noise state operations use an OS lock shared across processes. A new static key
is published only after complete bytes have been flushed; pin/cache updates are
atomic transactions. Use a state directory on a filesystem supporting hard links
and atomic replacement (such as NTFS or native Unix filesystems); unsupported
filesystems fail safely without publishing partial keys. Lock acquisition is
bounded, and async handshake work runs away from Tokio workers so another
process's lock cannot stall a caller deadline. Interrupted or malformed trust files are never silently
replaced. Existing state files must be regular files and private on Unix.

Concurrent `connect_with_timeout` calls join authenticated readiness rather than
returning when a socket merely opens. A joining caller's timeout or cancellation
leaves the initiating connection alone; cancelling the initiator invalidates its
own generation. Transport sends default to a 20-second bound and cleanup to two
seconds for the caller; an owned worker continues cleanup after that deadline.
MQTT allows its worker three seconds for offline publication and event-loop
retirement. An unacknowledged HTTP cleanup retains this object's admission marker,
so the next connection retries cleanup before admitting a fresh session.
If the hub cleaned up but its response was lost, the retry also accepts its
exact one-field JSON object: `{"error":"Already Disconnected"}` or
`{"error":"Client is not connected"}`. These acknowledgments apply only to
successful HTTP disconnect responses; any additional field, including `ok` or
`status`, invalidates them. Other refusals and `ok: false` still fail and retain
admission ownership.

All three transports use the same Noise negotiation, authenticated framing, and
peer pinning implementation; transport-specific connection and send ownership
remain separate.

HTTP reconnect resets this object's prior admission before asking for a new
Noise offer, so it can recover when a failed poll leaves the old peer registered.
An initial connection does not evict a peer admitted by another process.

HTTP retains the listener's replica affinity cookie, sends encrypted frames as
Base64 form data with `binary=1`, and decrypts `/get_binary_messages` replies.
HTTP errors and JSON `error` responses fail the session, except the documented
already-disconnected acknowledgments during cleanup. Redirects are
refused so identity credentials cannot move to another endpoint. For custom
trust roots, use `HttpTransport::with_options_and_http_client_builder`; it
always enables cookies and disables redirects.

MQTT carries raw Noise ciphertext after the initial HELLO and handshake. Its
broker connection ID is random; identity credentials still belong in the
protocol's topic paths. Use one identity per simultaneous client. A broker
failure invalidates readiness; call `connect()` to resubscribe and negotiate a
fresh session. `set_tls_configuration` configures private CA roots or a TLS
client certificate. TLS remains mandatory to protect the access key and broker
credentials in addition to Noise's end-to-end message encryption.

All transports serialize encryption and complete chunk delivery. An interrupted
or failed send requires a new session, and `encrypt=false` cannot bypass Noise.
Each transport exposes `remote_static_key()` only for a working session.

```rust
let transport = thalovant::HttpTransport::new(identity);
transport.set_noise_state_dir(Some("/var/lib/my-agent/thalovant".into())).await;
transport.connect().await?;
// Readiness follows the Noise exchange and encrypted HELLO.
transport.emit_bus(
    "ovos.intent.list",
    serde_json::Map::new(),
    serde_json::json!({"request_id": thalovant::new_request_id()})
        .as_object().unwrap().clone(),
).await?;
transport.disconnect().await?;
```

Inspect what an identity supports:

```rust
let identity = result.identity.clone();

println!("{:?}", identity.enabled_protocols());
println!("{:?}", identity.endpoint_for(HubProtocol::Wss));
println!("{:?}", identity.endpoint_for(HubProtocol::Https));
println!("{:?}", identity.endpoint_for(HubProtocol::Mqtt));
println!("{:?}", identity.mqtt.as_ref().map(|mqtt| &mqtt.endpoint));
```

Connect with a specific protocol:

```rust
for protocol in [HubProtocol::Wss, HubProtocol::Https, HubProtocol::Mqtt] {
    if !identity.supports_protocol(protocol) {
        continue;
    }
    if protocol == HubProtocol::Mqtt && identity.mqtt.is_none() {
        continue;
    }

    let client = Client::with_protocol(identity.clone(), protocol)?;
    let reply = client
        .ask(&format!("Reply over {protocol:?}."), RequestOptions::default())
        .await?;
    println!("{protocol:?}: {}", reply.text);
    client.close().await?;
}
```

Use `client.connect_with_info().await` when you need connection telemetry for
benchmarks or health dashboards. The returned snapshot includes phase,
socket/open time, handshake time, total connect time, and last error.

Use `client.query(...).await` for the direct HiveMind query frame path when the
hub supports it. It avoids broad bus fanout and is the preferred request/reply
API for low-latency app integrations.

```rust
let reply = client.query("What time is it in Toronto?", QueryOptions::default()).await?;
```

When migrating from the old MQTT topic API, replace `MqttTopicSet.c2s` and
`.s2c` with `.inbound` and `.outbound`. `MqttBrokerCredentials.hub_id`,
`c2s_topic`, `s2c_topic`, `status_topic`, and `hash_topics` were removed: use
the API-provided full `topic_prefix`, from which `/in`, `/out`, and `/status`
are derived. These historical breaking changes are included in the 0.3 and
later release lines; no additional field is removed by this patch.

MQTT identities include a broker endpoint, username, password, TLS flag, and
topic prefix. The broker credentials are scoped to that client and should be
treated like a password. Public identities should use `mqtts://`; the SDK also
honors an explicit `tls: true` flag from the identity.

## Conversations

Use a conversation when related turns should share one session.

```rust
use thalovant::{ConversationOptions, RequestOptions};

let conversation = client.conversation(ConversationOptions {
    lang: Some("en-us".into()),
    ..Default::default()
});

let first = conversation
    .ask("Remember that my favorite color is blue.", RequestOptions::default())
    .await?;
let second = conversation
    .ask("What color did I mention?", RequestOptions::default())
    .await?;

println!("{}", first.text);
println!("{}", second.text);
```

## Client Context

Context lets skills know which app, device, user, or channel made the request.

```rust
use thalovant::{build_client_context, ClientContextOptions, RequestOptions};

let context = build_client_context(None, ClientContextOptions {
    user_id: Some("user-42".into()),
    user_name: Some("Ada".into()),
    auth_provider: Some("oidc".into()),
    roles: vec!["member".into()],
    platform: Some("kiosk".into()),
    source: Some("checkout-kiosk".into()),
    channel: Some("chat".into()),
    ..Default::default()
});

let reply = client
    .ask(
        "Show the next instruction.",
        RequestOptions {
            context: Some(context),
            ..Default::default()
        },
    )
    .await?;
```

## Actions And Exact Inputs

Use actions for button payloads and codes for exact typed or scanned values.

```rust
use thalovant::{ActionOptions, CodeOptions, ConversationOptions};

let conversation = client.conversation(ConversationOptions {
    session_id: Some("work-session".into()),
    ..Default::default()
});

conversation
    .send_action(
        r#"/choose{"id":"42"}"#,
        ActionOptions {
            title: Some("Choose item".into()),
            ..Default::default()
        },
    )
    .await?;

conversation
    .send_code(
        "SN-001-XYZ",
        CodeOptions {
            kind: Some("qr".into()),
            label: Some("serial".into()),
            ..Default::default()
        },
    )
    .await?;
```

## Rich Responses

Replies can include text, choices, tables, images, or attachments.

```rust
let items = reply.display_items(Some(600));
for item in items {
    if item.kind == "text" {
        println!("{}", item.text.unwrap_or_default());
    }
}
```

## Common Issues

- `missing access token`: call `control.login(...)` before private
  control-plane actions, or pass an access token to `ControlPlane::new`.
- `API access requires a paid plan`: upgrade the workspace before using the SDK
  control-plane API to provision private resources.
- `UnsupportedProtocol`: the hub does not expose that protocol, or the identity
  was created before that protocol was enabled.
- MQTT fails immediately: create or download a fresh client identity after MQTT
  is enabled. MQTT needs the per-client `identity.mqtt` credentials.
- A request times out: set `RequestOptions { timeout: Some(...), .. }`. Ask and
  Query use one deadline for connecting, writing, and collecting the answer.
  `ask` waits briefly for delayed speech after a handled event or an intent miss;
  recovered soft misses can succeed, while a policy denial remains a failure.
  Empty completion is `ThalovantError::Timeout`. `ask_with_options(AskOptions)`
  exposes `empty_reply_wait` (default five seconds) and `reply_settle` (default
  250 milliseconds). Each window starts once, at the first qualifying event,
  and stays within the request deadline. Ask collects while the write is pending
  and returns available speech when that deadline expires. Hard policy denials or
  query timeouts immediately freeze Ask/Query replies; later speech cannot change
  the failed result. Ask reports a receive-buffer overflow as an error. Dropping
  either request cancels its owned write and subscription; an uncertain Noise
  write poisons only the captured session and is never replayed automatically.
- `ThalovantError::PolicyDenied`: the hub's policy does not let this connection
  publish that message type (`ovos.intent.list`, say). Allow the type in the
  connection's settings in the dashboard; `allowed` lists what it may publish
  today.
- `HTTP 429` with `"code": "token_rate_limited"`: the API token exceeded its
  plan's per-minute request rate (60 requests per minute on the free plan).
  The response carries a `Retry-After` header and a matching
  `retry_after_seconds`; wait that long and resend.
- `HTTP 429` with `"code": "token_quota_exceeded"`: the API token exhausted
  its plan's daily or monthly call quota. The body names which in `quota`
  (`daily` or `monthly`) alongside `limit`, `used`, and `retry_after_seconds`;
  `Retry-After`
  points at the next UTC day or month boundary.

Both 429s apply to token-authenticated control-plane calls and surface as
`ThalovantError::ApiResponse`, carrying the status and a bounded, redacted JSON error
object. Unstructured response bodies are omitted. The SDK does not expose
HTTP headers or structured retry metadata and does not retry automatically.
When inspecting a direct API response, `Retry-After` is authoritative; honor it before
resending. Per-plan limits are listed in the dashboard and at
<https://docs.thalovant.com/developers/sdks/rust/>.

## API Shape

- `ControlPlane::default()`
- `ControlPlane::new(api_url, access_token)` for local or self-hosted control planes
- `ControlPlane::with_access_token(token)` for CI and other pre-provisioned-token environments
- `control.login(email, password, scope)`
- `control.login_with_options(email, password, options)` for MFA (`otp_code`, `recovery_code`)
- `control.login_with_browser(options)` for the browser device flow (`DeviceLoginOptions`)
- `control.list_public_hubs(limit, cursor)`
- `control.get_public_hub(hub_ref)`
- `control.list_hubs(limit, cursor, owner_id)`
- `control.get_hub(hub_id)`
- `control.create_hub(payload, idempotency_key)`
- `control.update_hub(hub_id, payload, etag)` (sends `If-Match`; `etag` required)
- `control.delete_hub(hub_id, etag)` (sends `If-Match`; `etag` required)
- `control.release_hub(hub_id, options)` (`ReleaseOptions`)
- `control.set_hub_rating(hub_id, rating)` (not paid-gated)
- `control.clear_hub_rating(hub_id)` (not paid-gated)
- `control.get_hub_runtime_capabilities(hub_id)` (`hubs:inspect`; 409 with no connected client)
- `control.list_runtime_groups(owner_id)`
- `control.get_runtime_group(runtime_group_id)`
- `control.create_runtime_group(payload)`
- `control.update_runtime_group(runtime_group_id, payload)`
- `control.get_runtime_group_config(runtime_group_id)`
- `control.update_runtime_group_config(runtime_group_id, config, personas)`
- `control.release_runtime_group(runtime_group_id, options)` (`ReleaseOptions`)
- `control.delete_runtime_group(runtime_group_id)`
- `control.install_runtime_group_skill(runtime_group_id, skill_id, options)` (`SkillInstallOptions`)
- `control.uninstall_runtime_group_skill(runtime_group_id, skill_id)`
- `control.list_marketplace_skills(options)` (`MarketplaceSkillsOptions`; `hubs:read`, not paid-gated)
- `control.list_runtime_group_marketplace(runtime_group_id, refresh_inventory)` (`hubs:inspect`)
- `control.list_runtime_group_inventory(runtime_group_id, refresh)` (`hubs:inspect`)
- `control.get_operation(operation_id)`
- `control.get_analytics_overview(options)`
- `control.list_memory_items(options)`
- `control.get_memory_summary(owner_id)`
- `control.create_memory_item(payload)`
- `control.get_memory_item(memory_id)`
- `control.update_memory_item(memory_id, payload)`
- `control.delete_memory_item(memory_id)`
- `control.create_client_identity_for_hub_id(hub_id, options)`
- `Identity::from_config(profile)`
- `Client::from_config(profile)`
- `Identity::from_file(path)`
- `Client::from_file(path)`
- `Client::from_env()`
- `Client::with_protocol(identity, protocol)`
- `client.connect_with_info()`
- `client.connection_info()`
- `client.query(text, options)`
- `client.ask(text, options)`
- `client.ask_with_options(text, options)` (`AskOptions`; delayed speech and fragment collection)
- `client.send_utterance(text, options)`
- `client.send_action(payload, options)`
- `client.send_code(value, options)`
- `client.conversation(options)`
- `client.intents_with_capabilities(languages, options)` (`HubIntentCapabilities`; fallback handlers and `may_answer`)
- `client.list_fallbacks(timeout)` (optional fallback skill discovery)
- `client.intents(languages, options)` (`IntentInventoryOptions`; the hub's intent manifest, sentences per language)
- `client.list_intents(lang, options)` (`IntentListOptions`)
- `client.describe_intent(skill_id, intent_name, lang, options)` (`IntentDescribeOptions`)

## Development

```bash
cargo test
```

### Scoped event waits and streams

`wait_for_event(name, ListenOptions)` waits for one event with a twelve-second
budget by default, including connection. `listen(name, ListenOptions)` returns
an `EventStream` whose `recv().await` yields `Result<Option<Event>>`. Its optional
timeout covers the stream's whole lifetime; without one it listens until closed.
Set `max_events`, `request_id`, `session_id`, or a fast nonblocking `predicate`
when needed. Matching request IDs take precedence over a rewritten hub session.

```rust
use thalovant::ListenOptions;
let mut events = client.listen("speak", ListenOptions {
    max_events: Some(2),
    timeout: Some(std::time::Duration::from_secs(20)),
    ..Default::default()
}).await?;
while let Some(event) = events.recv().await? { println!("{}", event.name); }
```

Each stream owns a bounded transport subscription. Overflow and connection loss
are explicit errors. Drop or `close()` the stream to unsubscribe without closing
the shared client. Cancelling an individual `recv` future leaves the stream
usable; dropping a `wait_for_event` future releases its subscription.

Concurrent `ask` calls sharing a transport (including cloned clients) must use
distinct request IDs; concurrent `query` calls must use distinct query IDs.
An active duplicate returns `ThalovantError::Runtime` before publication. Ask
and Query have separate namespaces. Dropping or completing a collector frees
its reservation; existing transport ownership still controls retiring writes.
Use fresh IDs for every later logical operation, including after cancellation;
delayed remote replies can outlive the collector that originally requested them.


### Shared-runtime skill management

Hub-addressed skill methods select the runtime group attached to the hub UUID.
Every hub sharing that group sees the same skill changes and history. The API
requires a restricted token to cover all served hubs. Reads need `hubs:inspect`
(`hubs:read` implies it); writes need `hubs:write`, an eligible paid plan and ownership.

The history response contains newest-first `event` and `operation` entries,
including nullable actor/version fields. Callers must supply a limit from 1 to 200; pass 50 for the server default.
An accepted mutation is not proof the skill is ready. Optional waiting polls the
operation, with a 120-second default timeout and two-second interval. Polling
never repeats an accepted mutation and starts no new read after its deadline;
The Rust wait helper also cancels an in-flight status request at this deadline.

Methods: `list_hub_skills / list_hub_skill_history / install_hub_skill / update_hub_skill / remove_hub_skill / wait_for_hub_skill_operation`. Responses preserve API JSON fields. Use
`HubSkillWaitOptions` to opt into waiting. For cancellation-sensitive work, submit
without waiting, retain the complete accepted response (including `operation_id`
and `state`), then pass that response to the wait helper separately. Cancelling waiting does not undo the server operation. After a polling
failure, inspect/resume that operation instead of submitting the write again.

## Request helpers and safe configuration updates

Request hints carry a recognized language, ordered intent pipeline, and caller
location without changing the caller's context. Empty hints are omitted. The
location helper requires a city and omits invalid or zero/zero coordinates.
The hub validates language hints against its configured languages.

Replies expose their reported language, ordered speech/audio events, and a
count of dropped media. Embedded skill clips are limited to 4 MiB each and
16 MiB per reply, checked before retention and decoding. Audio does not extend
the reply settlement window. Decoding accepts hexadecimal bytes with ASCII
whitespace between bytes; it never fetches a skill-supplied URL or file path.
The application owns playback (the `play`/`Play` function in this example).

```rust
let location = thalovant::build_location(&thalovant::LocationOptions {
    city: "Montréal".into(), country: "CA".into(), ..Default::default()
});
let reply = client.ask_with_hints("Quel temps fait-il ?", Default::default(), thalovant::RequestContextOptions {
    stt_lang: Some("fr-ca".into()), location, ..Default::default()
}).await?;
for event in reply.media_events() { if event.is_audio() { let bytes = event.audio_bytes()?; /* play bytes */ } }
let examples = intent.examples_with_options(Some("en-us"), 2, true, &Default::default());
api.update_runtime_group_config(group_id, delta, None).await?;
// Explicit full replacement:
api.replace_runtime_group_config(group_id, full_config, None).await?;
```

Guarded merging requires the `hubs:read` and `hubs:write` scopes and a paid plan.
Safe merging requires an API whose configuration GET returns a valid `revision`
and whose configuration PUT checks `expected_revision`. The SDK rereads and
reapplies the original delta only after HTTP 412, with at most three attempts.
Arrays and scalar values replace; objects merge recursively. Personas replace
only when explicitly supplied. Connection failures, redirects, other statuses,
and ambiguous write results are never retried. No unsafe PATCH fallback is used.
Unconditional replacements must still be coordinated with other writers.

Use the explicit replacement operation shown above when a complete replacement
is intended, including when working with an older API. Existing code relying on
replacement must opt into it when upgrading. Raw intent patterns remain the
default; speakable examples remove optional parts, choose alternatives, and
substitute caller-supplied slots while retaining complete-phrase priority.

The audio limits use encoded-length upper bounds before decoding, so formatting
whitespace consumes budget too. Like Python's `bytes.fromhex`, ASCII whitespace
alone decodes to zero bytes. Bounded malformed clips remain available as event
metadata and fail when decoded; they are never fetched or played automatically.
Distinct audio events may intentionally repeat identical sound content. Only
repeated delivery of the same event object is suppressed where object identity
is available, without counting it as a dropped clip. Rendered example ranking
uses the original pattern's slot presence even when sample values are supplied.

Guarded merges in 0.7.1 preserve native signed/unsigned 64-bit integers but
reject floating-point configuration/persona values outside the exact integer
range (±9,007,199,254,740,991), including stored integers that overflow native
JSON integer storage. Use string identifiers for larger integers.

## Locale-aware intent listings

`as_sentence("quelle heure est-il", Some("fr-CA"))` returns
`"Quelle heure est-il?"`. `speakable_with_language(pattern, &slots, Some(lang))`
uses the bundled thalovant-languages 0.1.1 slot examples before explicit caller
overrides. The original `speakable` function remains available without locale defaults.

Use `intent.examples_with_listing(lang, limit, &IntentExampleOptions {
sentence: true, ..Default::default() })` for capitalized sentences. Sentence mode
also renders patterns. The existing `examples_with_options` remains available.
Complete phrases rank before prefixes and slot patterns, then fuller wording
up to eight words. Empty and duplicate rendered phrases do not consume limits.
Raw unlimited examples keep their registered order. With no language, the
first sorted phrase-map key supplies the locale.

Regional matching follows the OVOS distance policy using langcodes 3.5.1 CLDR
data, including Portuguese norm-region behavior. Distances above ten do not
match; ties preserve candidate order.

`ListingRules::new(Some(data))` owns a complete custom data tree. Pass a reference
in `IntentExampleOptions.listing` or call its methods. `ListingRules::new(None)`
selects bare rendering with slot names. Unknown languages also stay bare. Invalid
patterns return `ThalovantError::Listing`. Regex backtracking is limited to
100,000 steps; `asks` reports matching failures, while sentence rendering leaves
failed rules unpunctuated. The rules are safe to share between threads and
perform no runtime file or network access.

Generated source licenses are included in `LICENSE-languages` and
`LICENSE-langcodes`. Regenerate data and reference cases with
`python scripts/sync-listing-data.py --test-dir tests/data` using the public
Python environment pinned in that script.
