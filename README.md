Last Edit: Claude Sonnet 5.5 - 2026-11-11 - Motive: Intercom (0.18.0), final API: intercom:urgent, speaker kind/can_play/policy_synced, new refusal kinds, one text rule, DND timezone, replayed.

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

## Intercom

Say a short text aloud on a hub's speakers. Off by default at every level, announce-only,
and **not a certified safety system**: it does not replace smoke or CO alarms, medical
alert devices or emergency services. The API does not store the text (it keeps a keyed hash
and the length), and intercom does not record or store speech: while an alert sounds, the hub
only checks whether a reply is an acknowledgement. (The names are `announce` and
`*_intercom_*`: the HiveMind mesh already has an `intercom` frame.) Scopes: `intercom:read`,
`intercom:send` (announce and cancel; it implies `intercom:read`, which the polls of `wait` use), `intercom:urgent` (an
**urgent** announcement, and so every `repeat`, needs it **in addition to** `intercom:send`; nothing
implies it) and `hubs:write` for settings and speakers (owner, manager or administrator of the hub).
The constants are `INTERCOM_READ_SCOPE`, `INTERCOM_SEND_SCOPE` and `INTERCOM_URGENT_SCOPE`.

```rust,no_run
use thalovant::{
    AnnounceOptions, ControlPlane, IntercomPriority, IntercomRepeat, IntercomSpeakerUpdate,
    IntercomTargets,
};

# async fn run(api: ControlPlane, hub_id: &str) -> thalovant::Result<()> {
api.set_intercom_settings(hub_id, true).await?; // hubs:write
// A speaker is addressed by its `client_id` (from the list), not by a room or a name.
let speakers = api.list_intercom_speakers(hub_id).await?; // intercom:read
// A connection with `can_play == false` is listed but cannot receive: pick one that can.
let Some(speaker) = speakers.items.iter().find(|speaker| speaker.can_play) else {
    return Err(thalovant::ThalovantError::Api(
        "this hub has no connection that can play speech".into(),
    ));
};
api.update_intercom_speaker(
    hub_id,
    &speaker.client_id,
    &IntercomSpeakerUpdate { receive: Some(true), room: Some(Some("kitchen".into())), ..Default::default() },
)
.await?;

// A fresh unique key for each announcement you mean to send (see "Idempotency keys" below):
// `new_request_id()` is 128 random bits; a plain UUID (the `uuid` crate) works as well.
let mut dinner = AnnounceOptions::new("Dinner is ready", IntercomTargets::Rooms(vec!["kitchen".into()]));
dinner.idempotency_key = Some(thalovant::new_request_id());
let said = api.announce(hub_id, &dinner).await?;
println!("{} {:?}", said.status, said.deliveries);

let mut urgent = AnnounceOptions::new("Please answer", IntercomTargets::All);
urgent.priority = Some(IntercomPriority::Urgent);
urgent.repeat = Some(IntercomRepeat { times: 3, interval_seconds: 20 });
urgent.idempotency_key = Some(thalovant::new_request_id());
urgent.wait = true; // polls while `sending` (intercom:read, implied by intercom:send); urgent needs intercom:urgent
let done = api.announce(hub_id, &urgent).await?;
println!("{} acked by {:?}", done.status, done.acked_by);

// Changed your mind, or sent it to the wrong room? Stop it while it is still repeating.
let mut wake = AnnounceOptions::new("Wake up", IntercomTargets::All);
wake.priority = Some(IntercomPriority::Urgent);
wake.repeat = Some(IntercomRepeat { times: 10, interval_seconds: 60 });
wake.idempotency_key = Some(thalovant::new_request_id());
let sent = api.announce(hub_id, &wake).await?;
let stopped = api.cancel_announcement(hub_id, &sent.id).await?; // intercom:send
println!("{} {:?}", stopped.status, stopped.hub_confirmed);     // cancelled, Some(true) when the hub confirmed
# Ok(())
# }
```

### Idempotency keys: one fresh key per announcement

Set `idempotency_key` to a fresh unique key (a UUID) for every announcement you mean to send.
The API scopes a key to `(hub, key)` and remembers it for 24 hours:

- **The same key replays the stored announcement** (the response header is
  `Idempotency-Replayed: true`) and **never sends again**, whatever its status. A replayed
  `failed` announcement stays `failed`; it is not retried.
- **A constant key silences you.** A literal key such as `"dinner"` sends once. For the next 24
  hours a later announcement with that key and the **same** request replays the first one's stored
  answer, with the status it stored (`completed`, `failed`, ...), and nothing is spoken; one with a
  **different** request is a 409. Never copy a literal key into real code, and never derive it
  from the text or the date.
- **Reuse a key only for the same attempt**, after a network drop or a timeout: the call then
  returns what the first attempt did (and its id) instead of speaking twice.
- **To send the same text again on purpose, use a new key.**
- **After a 502 `hub_unreachable`** (`ApiRefusal::HubUnreachable`) the hub may or may not have
  spoken. The API recorded the attempt as a `failed` announcement and puts its id in the problem
  body: `error.announcement_id()`. Read it with `get_announcement` and look at the deliveries
  before you send again. Resending with the **same** key will not retry (it replays the `failed`
  record). A `failed` record is not proof that nothing was spoken: weigh the risk of a repeated
  announcement, and use a **new** key only when you choose to send a second attempt.

```rust,no_run
# use thalovant::{AnnounceOptions, ApiRefusal, ControlPlane, IntercomTargets};
# async fn run(api: ControlPlane, hub_id: &str) -> thalovant::Result<()> {
let mut options = AnnounceOptions::new("Dinner is ready", IntercomTargets::All);
options.idempotency_key = Some(thalovant::new_request_id());
if let Err(error) = api.announce(hub_id, &options).await {
    if error.api_refusal() == Some(ApiRefusal::HubUnreachable) {
        if let Some(id) = error.announcement_id() {
            let recorded = api.get_announcement(hub_id, id).await?; // what did the hub do?
            println!("{} {:?}", recorded.status, recorded.deliveries);
        }
    }
}
# Ok(())
# }
```

### Stopping an announcement

An urgent announcement that repeats can be stopped. These are the real controls:

- **`cancel_announcement(hub_id, announcement_id)`** (`intercom:send`; the hub's owner, a
  manager, a plain member of the tenant or an administrator, and an impersonated session too:
  stopping speech is not restricted to the people who may announce) asks the hub to stop repeating and returns the announcement as `cancelled`. It works only
  while the status is `sending`; for one that already completed, failed or was cancelled the API
  answers 409 `not_cancellable` (`ApiRefusal::NotCancellable`). If the hub cannot confirm the
  cancel, the record is still `cancelled` and `hub_confirmed` is `Some(false)`: speech already
  queued on the hub may still play. `error.announcement_status()` of a `not_cancellable` refusal
  is the current status: refresh the result instead of showing an error. Stopping is never
  refused for platform, hub or plan reasons: it works while the platform switch is off (the answer
  then has `hub_confirmed: Some(false)` and no hub was called).
- **The hub's Intercom switch** (`set_intercom_settings(hub_id, false)`, `hubs:write`) takes
  effect when the hub's runtime updates, which can take as long as a rollout; the hub's repeat
  loop also stops by itself when it sees the switch off.
- **The platform kill switch** (operator side) and a speaker whose `receive` is turned off also
  stop the repeats.
- **Somebody answering at a speaker.** An urgent announcement stops being repeated once it is
  acknowledged by voice ("I'm okay", "got it", "stop").

Nothing here is instant for sound that is already playing, and none of it makes intercom a
safety system.

### Speakers: `kind`, `can_play` and `policy_synced`

`list_intercom_speakers` lists every connection of the hub (the platform's own Intercom client is
not listed). Each `IntercomSpeaker` has `kind` (`voice_satellite`, `web_chat`, `developer`,
`embedded`, `home_assistant`; anything else reads `voice_satellite`) and `can_play`: the
**effective** "can this connection play speech". True by default for a `voice_satellite` and
`web_chat`; false for `home_assistant`, `developer` and `embedded` unless its owner declared
`spec.intercom.can_play: true` on the **connection** (never on the speaker). A connection that
cannot play is listed (grey it out) but `receive: Some(true)` is refused with
`ApiRefusal::NotASpeaker` (422 `not_a_speaker`, `error.not_a_speaker_kind()`), it is excluded
from `all`, rooms and groups, and naming it in `IntercomTargets::Speakers` is the same refusal
with `error.not_speakers()`. `policy_synced` is set on the answer to `update_intercom_speaker`
(`None` on list items); the list has its own, and `unavailable_reason`, which can be `platform`.
`Some(false)` means the hub may hold an out-of-date consent map (a push failed and is retried in
the background), so a speaker whose `receive` was just turned off can still be spoken to by a
routine until it clears: show "Changes may take a moment to reach the home".

`IntercomSpeakerUpdate` has exactly `receive`, `room`, `groups` and `dnd`; send only what changed.
Do-not-disturb that is switched on needs `quiet_hours` with an IANA `timezone` (at most 64
characters, such as `Europe/Paris`) and a `start` that differs from the `end`; a timezone that
is given is checked even when it is off. The check is of the *shape* of the name (`Area/Location`);
this crate links no timezone database, so a well-formed name that does not exist is the API's 422.

### The text, and what an answer means

`announce` checks the text with the API's own rule before anything is sent, so a refusal never
costs a request and never echoes the text: NFC-normalised and trimmed, 1 to 280 characters (counted
after that), no `<` or `>` (a speaker's text-to-speech may read them as markup), no control
characters (a newline or a tab included), no bidirectional overrides or isolates and no other
invisible format characters (the zero-width joiner and non-joiner are allowed), no private-use
characters and no line or paragraph separators (a Rust string cannot hold a lone surrogate).
`lang`, when given, is a language tag (`en`, `en-US`). Target lists hold at most 50 names of 1
to 64 characters each; `all`, a room or a group that resolves to more than 50 speakers is
`ApiRefusal::TooManyTargets` (422, `error.matched_speakers()`); naming the same room or group
resolves to the same speakers, so choose a narrower target or name at most 50. An
`idempotency_key` is at most 128 printable ASCII characters.

`delivered` means the hub handed the speech to that speaker's connection. There is **no playback
receipt**: do not say "heard" or "played". Only `acked` (an urgent announcement somebody answered)
is a person's answer, and `failed` is not proof that nothing was spoken. A delivery's `detail` is a
fixed code from `DELIVERY_DETAIL_CODES` (`hub_no_response`, `rate_limited`, ...) or `None`; treat a
code you do not know as opaque. `Announcement::replayed` is `true` when the answer is the stored
one for your `idempotency_key` (the header `Idempotency-Replayed: true`, also after `wait`): nothing
new was sent, so do not clear what a person typed, or say "sent", without saying so.

At most two urgent announcements repeat on one hub at a time (the platform has a pool too): the
next is `ApiRefusal::UrgentBusy` (429, `error.urgent_busy_scope()`, `retry_after_seconds` in
`error.api_problem()`), with nothing stored and no rate token spent: cancel one that is running,
or wait. An urgent announcement on a hub whose runtime has no active Intercom skill is
`ApiRefusal::SkillNotInstalled` (409): nothing was sent.

### Dark by default: the platform switch

Until the platform operator turns intercom on, `get_intercom_settings` reads `enabled: false,
unavailable_reason: Some(Platform)` for every hub, and every write and every announcement is
refused with `ApiRefusal::IntercomDisabledPlatform` (503 `intercom_disabled_platform`; nothing is
stored or sent). Treat it as "show the platform message", not as an error toast; reads and
`cancel_announcement` keep working. A platform that is not configured yet is
`ApiRefusal::IntercomNotConfigured` (503). Writing needs the owner, a manager or an administrator
acting as themselves: a plain member can read but not enable, change a speaker or announce, an
impersonated session cannot (`ApiRefusal::ImpersonationNotAllowed`), and a locked hub is
`ApiRefusal::HubLocked`; switching off and cancelling stay open.

### Ids, timeouts and `Debug`

The hub id, a speaker's `client_id` and an announcement id are each one path segment. The SDK
refuses (`ThalovantError::Api`) and sends nothing when an id is empty or blank, is `.` or `..`
(dot segments), or holds `/`, `?`, `#`, a backslash or a control character. The API waits up to
ten seconds for the hub, and this SDK sets no request timeout of its own: if you wrap `announce`
in `tokio::time::timeout`, give it at least 20 seconds. `AnnounceOptions` redacts its text in
`Debug` (`text: <N chars>`), so `{:?}`, `{:#?}` and `tracing`'s `#[instrument]` never print what
is spoken aloud.

`wait` polls with `get_announcement` (`intercom:read`, which `intercom:send` implies, so no further
scope is needed).
`poll_interval` must be above zero and `wait_timeout` a time the clock can reach (both are
checked before anything is sent), and the deadline bounds each poll, not only the sleep between
polls. A poll can fail after the announcement was accepted (a 429; the network): the error is then
`ThalovantError::AnnouncementWaitFailed`, and `error.announcement()` is the accepted
announcement (the POST's own answer), whose id lets you poll it later. `status_code()`, `api_refusal()` and the other
accessors look through to the poll's own error. Always set an `idempotency_key` with `wait`,
created **before** the first call (a new key is a different attempt and can announce twice). To get
the announcement back, call `announce` again with that same key and `wait` false: the POST replays
the stored announcement (with its id, no status read) and does not announce a second time; with
`wait` true the repeat call polls again and can fail the same way.

Refusals: `error.api_refusal()` is `Some(ApiRefusal::IntercomDisabled)` (the hub's switch is off),
`Some(ApiRefusal::IntercomUnavailable)` (a kid or public hub; `error.unavailable_reason()`),
`Some(ApiRefusal::IntercomDisabledPlatform)` (these two and `IntercomDisabled` are what
`kind.is_intercom_disabled()` is true for), `Some(ApiRefusal::IntercomNotConfigured)`,
`Some(ApiRefusal::UrgentScopeRequired)` (403 `urgent_scope_required`; `kind.is_auth()`),
`Some(ApiRefusal::UrgentBusy)`, `SkillNotInstalled`, `NotASpeaker`, `ReservedName`, `HubLocked`,
`ImpersonationNotAllowed`, `TooManyTargets`, `ManagedClient` (403: the platform's own Intercom
client cannot be managed; `error.refused_client_refs()`), `Some(ApiRefusal::HubUnreachable)` (502: the text may or may not have been
spoken; `error.announcement_id()` names the recorded `failed` announcement: read it before
sending again, and use a new key, because the same key replays the failed record),
`Some(ApiRefusal::NotCancellable)` (409) or `Some(ApiRefusal::Plan)` (HTTP 402, or 403 with
`plan_limit`); `error.is_rate_limited()` is a 429 (its `retry_after_seconds` is in
`error.api_problem()`).

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
