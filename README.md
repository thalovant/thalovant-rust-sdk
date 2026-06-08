# Thalovant Rust SDK

Rust SDK for direct Thalovant hub data-plane clients and agents.

Full documentation: <https://docs.thalovant.com/developers/sdks/rust/>

```bash
cargo add thalovant
```

```rust
use thalovant::{Client, Identity, RequestOptions};

#[tokio::main]
async fn main() -> thalovant::Result<()> {
    let identity = Identity::from_file("_identity.json")?;
    let client = Client::new(identity);
    let reply = client.ask("Tell me a short clean joke.", RequestOptions::default()).await?;
    println!("{}", reply.text);
    client.close().await?;
    Ok(())
}
```

## Status

This is an alpha SDK scaffold with identity, event, session, conversation,
AES-GCM preshared-key helpers, protocol endpoint helpers, and an async HTTP
transport compatible with the Thalovant SDK contract. The live transport
targets the preshared-key HTTPS HTTP-protocol path used by Thalovant public
hubs.

## Identity

```json
{
  "access_key": "client-access-key",
  "password": "client-password",
  "crypto_key": "optional-preshared-key",
  "site_id": "my-client-site",
  "default_master": "https://hub.example.com",
  "default_port": 443,
  "default_path": "/public",
  "data_plane_endpoints": {
    "https": "https://hub.example.com/public",
    "wss": "wss://hub.example.com/public",
    "mqtt": "mqtts://mqtt.example.com:8883"
  },
  "protocols": {
    "wss": {"enabled": true},
    "http": {"enabled": true},
    "mqtt": {"enabled": false}
  },
  "mqtt": {
    "endpoint": "mqtts://mqtt.example.com:8883",
    "username": "client-access-key",
    "password": "mqtt-broker-password",
    "topic_prefix": "hivemind/hub-id/client-access-key"
  }
}
```

```rust
use thalovant::HubProtocol;

let identity = Identity::from_file("_identity.json")?;

println!("{:?}", identity.enabled_protocols());
println!("{:?}", identity.endpoint_for(HubProtocol::Https));
println!("{:?}", identity.endpoint_for(HubProtocol::Wss));
println!("{:?}", identity.endpoint_for(HubProtocol::Mqtt));
println!("{:?}", identity.mqtt.as_ref().map(|mqtt| &mqtt.endpoint));
```

You can also create a hub client through the Thalovant API:

```rust
use thalovant::{BootstrapIdentityOptions, Client, ControlPlane};

let mut control = ControlPlane::new("https://dash.thalovant.com/api", None);
control.login("you@example.com", "password", None).await?;

let result = control
    .create_client_identity_for_hub_id("hub-id", BootstrapIdentityOptions {
        name: "kiosk-1".into(),
        ..Default::default()
    })
    .await?;

let client = Client::new(result.identity);
```

The SDK generates `apiKey`, `password`, and `cryptoKey` locally and sends them
to the API once. The API can store them in Vault and return only secret
references. When MQTT is enabled, `result.identity.mqtt` contains the broker
credentials returned by the API. Do not log `result.as_value(true)`.

## Generic Client Context

```rust
use thalovant::{build_client_context, ClientContextOptions};

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
```

## Actions, Codes, And Rich Output

```rust
use thalovant::{ActionOptions, CodeOptions};

let conversation = client.conversation(Default::default());

conversation.send_action(r#"/choose{"id":"42"}"#, ActionOptions {
    title: Some("Choose item".into()),
    ..Default::default()
}).await?;

conversation.send_code("SN-001-XYZ", CodeOptions {
    kind: Some("qr".into()),
    label: Some("serial".into()),
    ..Default::default()
}).await?;

let items = reply.display_items(Some(600));
```

## Development

```bash
cargo test
```
