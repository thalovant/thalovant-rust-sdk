Last Edit: Claude Opus 5.5 - 2026-11-08 - Motive: Sign-in section for browser sign-in (0.17.0), with the final reference's wording; the quick start signs in with interactive_login().

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/thalovant/.github/main/profile/assets/thalovant-logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://raw.githubusercontent.com/thalovant/.github/main/profile/assets/thalovant-logo-light.png">
    <img src="https://raw.githubusercontent.com/thalovant/.github/main/profile/assets/thalovant-logo-light.png" alt="Thalovant" width="320">
  </picture>
</p>

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
    BootstrapIdentityOptions, Client, ControlPlane, HubProtocol, InteractiveLoginOptions,
    RequestOptions,
};

#[tokio::main]
async fn main() -> thalovant::Result<()> {
    let mut control = ControlPlane::default();

    // Auth is required when creating a client identity. This opens the browser
    // on this machine, or prints a code to approve from any browser where there
    // is none.
    control.interactive_login(InteractiveLoginOptions::default()).await?;

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

## Sign in

```rust
use thalovant::{ControlPlane, InteractiveLoginOptions, LoginMethod};

let mut control = ControlPlane::default();
// The browser on this machine, or a device code where there is none.
control.interactive_login(InteractiveLoginOptions::default()).await?;

// LoginMethod::Browser or LoginMethod::Device forces one.
let forced = InteractiveLoginOptions {
    method: LoginMethod::Device,
    ..Default::default()
};
control.interactive_login(forced).await?;
```

`interactive_login` opens your browser at the Thalovant consent page and
listens on `127.0.0.1` for the answer (OAuth with PKCE on a loopback redirect,
RFC 8252). It always prints the address too, in case the browser does not
open. The browser that approves hands the code to this computer's
`127.0.0.1`, so a link or code sent to someone on another computer delivers
nothing to the sender. That is all it promises: a forwarded callback port
(`ssh -L`, a port-forwarding tool) or another user of the same computer can
still complete a sign-in.

On a machine that cannot show a browser, it prints a code to enter at
`thalovant.com/activate` from any browser instead (device login): over SSH
(`SSH_CONNECTION` or `SSH_TTY` set to a non-empty value), in CI (`CI`, any
non-empty value), or on Linux and the BSDs with
neither `DISPLAY` nor `WAYLAND_DISPLAY`. It does the same against an API that
does not offer browser sign-in yet. Once the browser has been opened, its
outcome stands: a denial or a timeout is returned, never turned into a device
login.

`browser_login(BrowserLoginOptions)` is the browser sign-in on its own. Its
options are `scopes` (none asks for the API's default, `hubs:read` and
`clients:write`), `device_name` (what the consent page and the token show;
`thalovant-rust-sdk on <hostname>` when `None`, nothing when empty, at most
120 characters),
`open_browser`, `timeout` (300 seconds), `prompt` (present the URL yourself)
and `opener` (open it yourself). Both calls return the token answer and keep
the token exactly as a device login does, so `revoke_api_token(None)` revokes
it. Dropping the future cancels the sign-in and closes the port.

| Outcome | Error |
| :-- | :-- |
| Denied in the browser | `ThalovantError::DeviceAuthorizationDenied` |
| No answer within the timeout | `ThalovantError::DeviceAuthorizationExpired` |
| Another error from the consent page, or no code | `ThalovantError::BrowserLoginFailed` (`api_code()` names it) |
| A code the API will not exchange | `ThalovantError::ApiResponse`, HTTP 400, `api_detail()` `invalid_grant` |
| An API without browser sign-in | `ThalovantError::ApiResponse`, HTTP 404 (`interactive_login` uses device login) |

The code, the PKCE verifier, the state and the token never appear in an
error. Device login (`login_with_browser`, `begin_device_login`,
`poll_device_login`) is unchanged. The building blocks (`browser_authorization_url`,
`loopback_redirect_uri`, `can_open_browser`, `NativeSignInConfig`, ...) are in
`thalovant::native_auth`.

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

## Chat apps

A hub owner can connect a chat network to a hub, and the hub answers there.
Telegram and Discord are offered; Matrix, Mattermost and Twitch are in beta.
Each connection is a *bridge*, managed with `ControlPlane` methods (types in
`thalovant::chat_apps`). Reads need `hubs:read` and changes `hubs:write`, with
one exception: `list_bridge_chats` needs `hubs:write`, since the API asks the
network for the chats with the bot's own credentials. Creating a bridge needs a
paid plan with room in its allowance.

| Method | What it does |
| :-- | :-- |
| `list_chat_app_connectors()` | The chat apps: copy, settings form, secrets, chat id pattern, helpers |
| `get_bridge_networks(hub_id)` | Which networks are offered, and the owner's allowance |
| `list_bridges(&BridgeListOptions)` | One hub's bridges, or every bridge you may see |
| `create_bridge(BridgeCreate)` | Connect a bot; the bridge starts in `setup` |
| `get_bridge(id)` | One bridge with its status |
| `update_bridge(id, &BridgeUpdate)` | Choose chats, mention rule, session scope, settings; pause or resume |
| `replace_bridge_token(id, token)` / `replace_bridge_secrets(id, secrets)` | New credentials |
| `list_bridge_chats(id)` | The chats the bot can answer in (`hubs:write`) |
| `send_bridge_test_message(id, chat_id)` | Post a test message in an allowed chat |
| `get_bridge_avatar(id)` | The bot's picture: bytes and media type |
| `delete_bridge(id)` | Stop and remove a bridge and its stored credentials |

Credentials go in as `SecretValue`: its `Debug` prints `<redacted>`, it has no
`Display` and no `Serialize`, and the SDK reads it only to build the request
body, which never appears in an error.

### Telegram (and Discord): one bot token

A run takes up the bridge an earlier run left in setup, so running it again
neither adds a bridge nor trips over the first one. Otherwise it reads the
allowance first (a Free plan allows no bridges) and connects the bot. The bridge
then answers only in the chat the owner names in `TELEGRAM_CHAT_ID`, never in
every chat the bot can see.

```rust
use thalovant::{
    BridgeAllowedChat, BridgeCreate, BridgeListOptions, BridgeState, BridgeUpdate,
    ControlPlane, SecretValue,
};

#[tokio::main]
async fn main() -> thalovant::Result<()> {
    let control = ControlPlane::with_access_token(std::env::var("THALOVANT_API_TOKEN").unwrap());
    let hub_id = "your-hub-id";

    let earlier = control
        .list_bridges(&BridgeListOptions {
            hub_id: Some(hub_id.into()),
            ..Default::default()
        })
        .await?
        .into_iter()
        .find(|bridge| bridge.network == "telegram");
    let bridge = match earlier {
        Some(bridge) if bridge.state == BridgeState::Setup => bridge,
        Some(bridge) => {
            println!("bridge {} is already set up", bridge.id);
            return Ok(());
        }
        None => {
            let networks = control.get_bridge_networks(Some(hub_id)).await?;
            if !networks.allowance.can_create {
                println!("no room: {} of {} bridges used", networks.allowance.used, networks.allowance.limit);
                return Ok(());
            }
            let token = SecretValue::from(std::env::var("TELEGRAM_BOT_TOKEN").unwrap());
            control
                .create_bridge(BridgeCreate::with_token(hub_id, "telegram", token))
                .await?
        }
    };

    // Add the bot to the group with bridge.invite_links and mention it there:
    // the chat list shows only chats with recent activity.
    let chats = control.list_bridge_chats(&bridge.id).await?;
    let Ok(wanted) = std::env::var("TELEGRAM_CHAT_ID") else {
        for chat in chats.data.iter().filter(|chat| chat.selectable) {
            println!("{}  {}", chat.id, chat.title);
        }
        println!("set TELEGRAM_CHAT_ID to the chat the hub should answer in, then run again");
        return Ok(());
    };
    let Some(chat) = chats.data.iter().find(|chat| chat.selectable && chat.id == wanted) else {
        println!("the bot cannot see chat {wanted} yet: mention it there, then run again");
        return Ok(());
    };

    // Choosing its first chat starts the bridge.
    let bridge = control
        .update_bridge(
            &bridge.id,
            &BridgeUpdate {
                allowed: Some(vec![BridgeAllowedChat::from(chat)]),
                etag: Some(bridge.etag.clone()),
                ..Default::default()
            },
        )
        .await?;
    control.send_bridge_test_message(&bridge.id, &chat.id).await?;
    Ok(())
}
```

Discord is the same with `"discord"`; a Discord bot always answers only what
mentions it.

### Matrix: homeserver and user id, with an access token

```rust
let bridge = control
    .create_bridge(
        BridgeCreate::with_secrets(
            hub_id,
            "matrix",
            [("access_token", SecretValue::from(std::env::var("MATRIX_ACCESS_TOKEN").unwrap()))],
        )
        .with_settings([
            ("homeserver", "https://matrix.example.org"),
            ("user_id", "@hub:example.org"),
        ]),
    )
    .await?;
// Matrix chats are typed, not listed (`list_bridge_chats` answers
// `manual_entry: true`): room ids look like `!abc:example.org`.
control
    .update_bridge(
        &bridge.id,
        &BridgeUpdate {
            allowed: Some(vec!["!abc:example.org".into()]),
            ..Default::default()
        },
    )
    .await?;
```

### Twitch: bot login and client id, with a refresh token and client secret

```rust
let bridge = control
    .create_bridge(
        BridgeCreate::with_secrets(
            hub_id,
            "twitch",
            [
                ("refresh_token", SecretValue::from(std::env::var("TWITCH_REFRESH_TOKEN").unwrap())),
                ("client_secret", SecretValue::from(std::env::var("TWITCH_CLIENT_SECRET").unwrap())),
            ],
        )
        .with_settings([("bot_login", "myhubbot"), ("client_id", "your-client-id")]),
    )
    .await?;
// Chats are channel logins; `#Name` is stored as `name`.
```

### Pause, resume, status and holds

Pause with `BridgeUpdate { paused: Some(true), ..Default::default() }` and
resume with `Some(false)`. Pausing is always allowed; resuming needs room in the
plan's allowance.

`bridge.status.reason` is the stable reason to show the owner:

| `BridgeStatusReason` | Meaning |
| :-- | :-- |
| `Setup` | No chats chosen yet: set `allowed`. |
| `Starting` | Rolling out, or not reported yet. |
| `Connected` | Running and answering. |
| `Paused` | Paused, by the owner or by a hold. |
| `Unavailable` | Rolled out but not running, or a platform-side error. |
| `TokenMissing` | The stored credentials have not reached the bridge; replace them. |
| `TokenRejected` | The network refused the bot's credentials: paste new ones. |

`bridge.hold` is set when the platform paused the bridge: `PlanLimit` (the plan
allows fewer running bridges; resume it once it has room, and holds also lift
on their own) or `AccountSuspended` (it resumes when the account is
reactivated). It is `None` for the owner's own pause. Every enum keeps a value
this release does not know in its `Other` variant.

A refusal is a `ThalovantError::ApiResponse` with an `api_code()` such as
`bridge_token_invalid`, `bridge_bot_in_use` or `bridge_url_not_public`;
`api_fields()` names the settings or secrets it refused, never their values. A
Free plan is HTTP 402 and a full allowance 403 `plan_limit`
(`api_refusal()` is `ApiRefusal::Plan` for both); a stale `etag` is HTTP 412.

## Reading an API error

A refused control-plane request fails with `ThalovantError::ApiResponse`. Read
the details from the error with `status_code()`, `api_code()`, `api_detail()`,
`api_fields()` and `api_problem()`.

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
