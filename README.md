# Thalovant Rust SDK

Rust SDK for direct Thalovant HiveMind HTTPS clients and agents.

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
AES-GCM preshared-key helpers, and an async HTTP transport compatible with the
Thalovant SDK contract. The live transport targets the preshared-key HiveMind
HTTP path used by Thalovant public hubs.

## Identity

```json
{
  "access_key": "client-access-key",
  "password": "client-password",
  "crypto_key": "optional-preshared-key",
  "site_id": "my-client-site",
  "default_master": "https://hub.example.com",
  "default_port": 443,
  "default_path": "/hivemind/public"
}
```

## Generic Client Context

```rust
use thalovant::{build_client_context, ClientContextOptions};

let context = build_client_context(None, ClientContextOptions {
    user_id: Some("operator-42".into()),
    user_name: Some("Ada".into()),
    auth_token: Some("access-token".into()),
    auth_provider: Some("oidc".into()),
    roles: vec!["operator".into()],
    platform: Some("mobile".into()),
    source: Some("line-a-tablet-3".into()),
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
