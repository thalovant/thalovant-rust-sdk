# Thalovant Rust SDK

Rust SDK for connecting services, CLIs, devices, and agents to Thalovant hubs.

The control API is used to discover hubs and provision a client identity. After
that, the SDK talks directly to the hub data plane over HTTPS, WSS, or MQTTS.

Full docs: <https://docs.thalovant.com/developers/sdks/rust/>

## What You Need

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
`password`, and `cryptoKey` minted by `POST /v1/clients`), so never log it or
write it anywhere world-readable. For diagnostics use `result.as_value(false)`,
which redacts every credential in both the identity and the hub/client bodies,
or `{:?}`, which redacts the same fields; neither exposes a secret.

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
`ThalovantError::Api` carrying the status and body.

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

Creating a hub is idempotent. `create_hub` sends a generated `Idempotency-Key`
header when you pass `None`, so a create retried after a timeout returns the
hub that was already created instead of making a second one. Pass
`Some(key)` to control the key yourself. No other provisioning route reads that
header.

Updating and deleting a hub use optimistic locking, so `etag` is a required
argument rather than an option. Pass the `etag` from the hub resource you read
— it lives in the JSON body, not in an `ETag` response header — and the SDK
sends it as `If-Match`. The API rejects a stale *or missing* value with HTTP
412 without changing anything:

```rust
let hub = control.get_hub(&hub_id).await?;
let etag = hub["etag"].as_str().unwrap_or_default();
let hub = control
    .update_hub(&hub_id, json!({"active": false}), etag)
    .await?;
control
    .delete_hub(&hub_id, hub["etag"].as_str().unwrap_or_default())
    .await?;
```

Deleting a hub also deletes its clients and ACLs. Runtime groups have no
`If-Match` requirement and read no idempotency header, but the API refuses to
delete the workspace default group or a group that still has hubs attached
(HTTP 409).

Runtime configuration is merged, not replaced, and `personas` is sent only when
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
- A request times out: set `RequestOptions { timeout: Some(...), .. }`.
- `HTTP 429` with `"code": "token_rate_limited"`: the API token exceeded its
  plan's per-minute request rate (60 requests per minute on the free plan).
  The response carries a `Retry-After` header and a matching
  `retry_after_seconds`; wait that long and resend.
- `HTTP 429` with `"code": "token_quota_exceeded"`: the API token exhausted
  its plan's daily or monthly call quota. The body names which in `quota`
  (`daily` or `monthly`) alongside `limit` and `used`, and `Retry-After`
  points at the next UTC day or month boundary.

Both 429s apply to token-authenticated control-plane calls and surface as
`ThalovantError::Api`, carrying the status and response body. The SDK does not
retry automatically: `Retry-After` is authoritative, so honor it before
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
- `client.send_utterance(text, options)`
- `client.send_action(payload, options)`
- `client.send_code(value, options)`
- `client.conversation(options)`

## Development

```bash
cargo test
```
