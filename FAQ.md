Last Edit: Claude Opus 5.5 - 2026-11-07 - Motive: Release-path, ws:// and hard-link questions from the CodeRabbit review-body close-out.

# FAQ

## Chat apps

**How do I connect a Telegram bot to a hub?**
Read `ControlPlane::get_bridge_networks(Some(hub_id))` first: `allowance.can_create` must be true (a Free plan allows none). Then `create_bridge(BridgeCreate::with_token(hub_id, "telegram", token))`, add the bot to a group with `bridge.invite_links`, list the chats with `list_bridge_chats`, and set them with `update_bridge(id, &BridgeUpdate { allowed: Some(...), ..Default::default() })`. The first chats start the bridge. See `src/chat_apps.rs` (module example) and the README.

**Which connectors take secrets instead of one token?**
Matrix (`access_token`, settings `homeserver` and optional `user_id`) and Twitch (`refresh_token` and `client_secret`, settings `bot_login` and `client_id`). Use `BridgeCreate::with_secrets(...).with_settings(...)`. `ChatAppConnector::secrets` and `config_schema` from `list_chat_app_connectors()` name what each needs.

**Why does `list_bridge_chats` return nothing for Matrix?**
Connectors without the `discover` helper answer `manual_entry: true` with an empty list: type the ids (Matrix room ids like `!abc:example.org`), which must match `ChatAppTargets::pattern`.

**Which token scope does `list_bridge_chats` need?**
`hubs:write`, unlike the other reads (`hubs:read`): the API asks the network for the chats with the bot's credentials.

**Why does `list_bridge_chats` fail with 409 `bridge_running` on Telegram?**
Telegram lists chats only while the bridge is stopped. Pause it (`paused: Some(true)`), wait a minute, list again.

**How do I pause or resume a bridge?**
`update_bridge(id, &BridgeUpdate { paused: Some(true), ..Default::default() })`, and `Some(false)` to resume. Resuming needs room in the plan (403 `plan_limit` otherwise).

**What is `bridge.hold`?**
Set when the platform paused the bridge: `PlanLimit` (the plan allows fewer running bridges) or `AccountSuspended`. `None` means the owner paused it.

**How do I avoid overwriting someone else's change?**
Set `BridgeUpdate::etag` to the `etag` of the bridge you read. The SDK sends it as `If-Match`; a stale one fails with HTTP 412 and changes nothing. Without it no `If-Match` is sent.

**Can a token end up in a log?**
Not through the SDK. `SecretValue`'s `Debug` prints `<redacted>`; it has no `Display` or `Serialize`; the request body is built only inside the SDK and never put in an error. `tests/chat_apps.rs` checks this with fake tokens, and checks the missing traits at compile time.

**What does a new status reason do to my code?**
Nothing breaks: every chat-apps enum keeps an unknown value in `Other(String)`. Match with a wildcard arm.

**Which settings did the API refuse?**
`error.api_fields()` lists their names (never values) for `bridge_settings_invalid`, `bridge_secrets_invalid` and the `bridge_url_*` codes; `error.api_code()` gives the code.

## Releases and transports

**Why does a test-only change wait for the next release?**
`auto-release.yml` treats every change under `src/` as release-relevant, and unit tests live under `src/` in `#[cfg(test)]` modules. Merging a test-only change there would bump the patch version and publish a crate with no code change, so such changes wait in a draft pull request until the next `src/` change. Changes to `tests/`, `CHANGELOG.md` and these notes files cut no release. `SUGGESTIONS.md` item 7 proposes a way out.

**Does the WSS transport accept `ws://`?**
Yes, as the Python reference does: `authorized_wss_url` in `src/transport.rs` accepts `ws://` and `wss://`. The access key then crosses the network in the clear, though it does not open a session on its own (the Noise pre-shared key comes from the password). The HTTP and MQTT transports refuse cleartext endpoints. Use `wss://` for any hub that is not on this machine; `SUGGESTIONS.md` item 6 proposes making that the rule in every SDK.

**Why does the first connect fail with "atomic Noise key publication requires a filesystem supporting hard links"?**
The state folder is on a filesystem without hard links (FAT32, exFAT, some network shares). The static key is published with a hard link so that two processes never replace each other's key (`publish_private` in `src/noise_store.rs`). Keep the identity file on a local disk, since `Client::from_file` keeps the key beside it, or point the transport's `set_noise_state_dir` at a local folder.
