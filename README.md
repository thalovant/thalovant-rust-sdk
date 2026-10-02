# Thalovant Rust SDK

[![crates.io](https://img.shields.io/crates/v/thalovant)](https://crates.io/crates/thalovant) [![CI](https://github.com/thalovant/thalovant-rust-sdk/actions/workflows/ci.yml/badge.svg)](https://github.com/thalovant/thalovant-rust-sdk/actions/workflows/ci.yml) [![Licence](https://img.shields.io/github/license/thalovant/thalovant-rust-sdk)](LICENSE) [![Docs](https://img.shields.io/badge/docs-docs.thalovant.com-5c6bc0)](https://docs.thalovant.com/developers/sdks/rust/)

Rust SDK for connecting services, CLIs, devices, and agents to Thalovant hubs.

The control API is used to discover hubs and provision a client identity. After
that, the SDK talks directly to the hub data plane over HTTPS, WSS, or MQTTS.

## Requirements

- Rust 1.88 or newer (tested in CI alongside the current stable compiler).
- A Thalovant account with API access for authenticated control-plane actions.
- A hub id or slug.

## Install

```bash
cargo add thalovant
```

## Quick start

```rust
use thalovant::{
    BootstrapIdentityOptions, Client, ControlPlane, HubProtocol, RequestOptions,
};

#[tokio::main]
async fn main() -> thalovant::Result<()> {
    let mut control = ControlPlane::default();

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

`ControlPlane::default()` uses `https://api.thalovant.com`.

Keep `result.identity` secret: it holds the client credentials the hub uses.
`result.as_value(true)` embeds the real credentials and the raw `hub` and
`client` bodies, so never log it or write it anywhere world-readable. For
diagnostics use `result.as_value(false)`, which redacts them.

## Documentation

The full guide is at <https://docs.thalovant.com/developers/sdks/rust/>. Topics are covered there and on related pages.

| Topic | Page |
| :-- | :-- |
| Install, sign-in (MFA, device login, API tokens), saved identities, events, common issues | [Rust SDK](https://docs.thalovant.com/developers/sdks/rust/) |
| Provisioning hubs | [Provisioning](https://docs.thalovant.com/developers/provisioning/) |
| Sessions and context | [Sessions and context](https://docs.thalovant.com/developers/sessions-context/) |
| Events and rich output | [Events and rich output](https://docs.thalovant.com/developers/events-rich-output/) |
| Actions and exact inputs | [Actions and exact inputs](https://docs.thalovant.com/developers/actions-and-exact-inputs/) |
| Identity files | [Identity files](https://docs.thalovant.com/developers/identity-files/) |
| Everything else | [docs.thalovant.com](https://docs.thalovant.com) |

Crate API reference: [docs.rs/thalovant](https://docs.rs/thalovant).

## Reading an API error

A refused control-plane request fails with `ThalovantError::ApiResponse`. Read
the details from the error with `status_code()`, `api_code()`, `api_detail()`,
and `api_problem()`.

## Home Assistant link

`answer_home_requests`, `home_response`, and `LinkSupervisor` are exported for
Home Assistant integrations; see the crate API reference.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## Security

See the [security policy](SECURITY.md).

## Licence

MIT. See [LICENSE](LICENSE).
