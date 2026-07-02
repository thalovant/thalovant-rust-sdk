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

Keep `result.identity` secret. It contains the client credentials used by the
hub. Do not log `result.as_value(true)`.

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

## API Shape

- `ControlPlane::default()`
- `ControlPlane::new(api_url, access_token)` for local or self-hosted control planes
- `control.login(email, password, scope)`
- `control.list_public_hubs(limit, cursor)`
- `control.get_public_hub(hub_ref)`
- `control.list_hubs(limit, cursor, owner_id)`
- `control.get_hub(hub_id)`
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
- `client.ask(text, options)`
- `client.send_utterance(text, options)`
- `client.send_action(payload, options)`
- `client.send_code(value, options)`
- `client.conversation(options)`

## Development

```bash
cargo test
```
