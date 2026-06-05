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
  "default_port": 443
}
```

## Development

```bash
cargo test
```
