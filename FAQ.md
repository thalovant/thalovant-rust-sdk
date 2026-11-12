Last Edit: Claude Sonnet 5.5 - 2026-11-11 - Motive: Intercom (0.18.0): wait scope and retry questions after the PR 91 review.

# FAQ

## Is intercom a safety system, and why is there no `intercom()` method?
It is announce-only text spoken by each speaker's own text-to-speech, and **not a certified safety system**: it does not replace smoke or CO alarms, medical alert devices or emergency services. The mesh already has an `intercom` frame, so the HTTP calls are `announce`, `get_announcement`, `list_announcements` and `*_intercom_*`. The platform never stores the text.

## Why did `announce` with `wait` fail after the announcement was accepted?
`wait` polls `get_announcement`, which needs `intercom:read` as well as `intercom:send`; a poll can also fail on a 429 or the network. The error is `ThalovantError::AnnouncementWaitFailed`: `error.announcement()` is the accepted announcement (its id), and `status_code()` and `api_refusal()` read the poll's own error. Set an `idempotency_key` and reuse it to retry, so the text is not said twice.

## Why was nobody spoken to?
A speaker is skipped unless its `receive` is true (`skipped_not_receiving`, even for urgent); a normal announcement is skipped inside quiet hours (`skipped_dnd`; urgent bypasses them); an offline speaker is `offline`. `ApiRefusal::IntercomDisabled` means the hub's switch is off or it is a kid or public hub.

## Sign-in

**How does a program sign somebody in?**
`ControlPlane::interactive_login(InteractiveLoginOptions::default())`. It opens the browser at the consent page and listens on `127.0.0.1:<port>/callback` for the answer (OAuth authorization code with PKCE, RFC 8252), or prints a device code where the machine has no browser. Both paths keep the token on the control plane (`access_token`, `token_id()`), and `revoke_api_token(None)` revokes it. See `src/control/browser_login.rs`.

**Why did `interactive_login` print a code instead of opening my browser?**
`native_auth::can_open_browser` said this machine cannot show one: `CI`, `SSH_CONNECTION` or `SSH_TTY` is set to a non-empty value (`CI=false` included; an empty one does not count), or it is Linux or a BSD with neither `DISPLAY` nor `WAYLAND_DISPLAY`. Or the API answered `GET /v1/auth/native/config` with a 404 (an API before thalovant-api#501), another failure or an answer the SDK cannot use, or the API URL is cleartext. Force the browser with `method: LoginMethod::Browser`; that never falls back to a device login.

**Why browser sign-in at all, when device login works?**
A device code can be read off one screen and typed in anywhere (device-code phishing). With a loopback redirect the browser that approves delivers the code to its own computer's `127.0.0.1`, so a link or code sent to someone on another computer delivers nothing to the sender. That is all it promises: a forwarded callback port (`ssh -L`) or another user of the same computer can still complete a sign-in.

**What happens if somebody else's tab, or an old attempt's, hits the port?**
A request to any other path is a 404, and a callback with a missing, repeated or wrong `state` is a 400. Neither ends the sign-in, and neither restarts its timeout. Only the first callback with the right state is accepted (`src/loopback.rs`, `classify_request`).

**What does the person see after approving?**
The tab shows `Signed in to Thalovant. You can close this tab.` once the token is stored, or `Sign-in was not completed.` for a denial, another error, a refused exchange or a timeout. The page loads nothing and is sent `no-store` and `no-referrer`.

**The person clicked Deny and nothing happened until the timeout. Why?**
The dashboard sends `error=access_denied` back only from the same release train as thalovant-api#501. Before it ships, a deny never calls back, so the 300-second timeout applies and the result is `DeviceAuthorizationExpired`.

**Which errors can `browser_login` return?**
`DeviceAuthorizationDenied` (denied), `DeviceAuthorizationExpired` (timeout), `BrowserLoginFailed { code }` (another OAuth error, or no code), `ApiResponse` 400 `invalid_grant` (a refused exchange; never retried, because a code presented twice revokes the token), `ApiResponse` 404 (no browser sign-in on this API), and `Api` with no status for an unusable configuration or token answer, or a cleartext API URL.

**How do I test code that calls `browser_login` without a real browser?**
Set `prompt` (it receives the URL) and `open_browser: false`, or set `opener` (it receives every URL the SDK would open, on its own thread). For `interactive_login`, `environment` and `platform` decide the browser rule for another machine. `tests/browser_login.rs` plays the browser with raw HTTP requests to the port in the URL's `redirect_uri`.

**Can I cancel a sign-in?**
Drop its future (or abort its task). The port closes at once and every connection the listener was serving is closed.

**Why does the tab say nothing for a while after I approve?**
It is answered once the sign-in has ended: the page says whether the token was stored, so it waits for the exchange (at most 10 seconds) rather than guess. Closing the sign-in answers it at once.

**How do I keep my hostname off the consent page?**
Pass `device_name: Some(String::new())`. Only `None` gets `thalovant-rust-sdk on <hostname>`; an empty name is left out of the URL, and the chooser's device login then sends no `client_name`.

**Why was my dashboard address refused as unusable?**
`NativeSignInConfig::from_value` takes only an address a browser can open: https (or loopback http), port 1 to 65535, and a host that is an IPv6 address in brackets, an IPv4 address or a DNS name of ASCII labels. `_`, `%`, an empty label, a trailing dot and a non-ASCII name are refused; write an international name in its `xn--` form. A `token_endpoint` with a colon or a broken `%`-escape is refused too, so `auto` uses a device login before anybody approves.

**Why is there a new error variant, `BrowserLoginFailed`?**
The contract reports a consent-page error such as `server_error` as an error with no status that still carries the code (only RFC 6749's codes are kept; any other value is `None`, since it could be the state or a code). `ThalovantError::Api(String)` cannot carry a code, and `ApiResponse` always has a status, so a variant of the existing (`#[non_exhaustive]`) enum carries it; `api_code()` reads it.

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
`auto-release.yml` treats every change under `src/` as release-relevant, and unit tests live under `src/` in `#[cfg(test)]` modules. Merging a test-only change there would publish a release with no code change (the version in `Cargo.toml` if its tag does not exist yet, else the next patch), so such changes wait in a draft pull request until the next `src/` change. Changes to `tests/`, `CHANGELOG.md` and these notes files cut no release, once a version tag exists. `SUGGESTIONS.md` item 7 proposes a way out.

**Does the WSS transport accept `ws://`?**
Yes, as the Python reference does: `authorized_wss_url` in `src/transport.rs` accepts `ws://` and `wss://`. The access key then crosses the network in the clear, though it does not open a session on its own (the Noise pre-shared key comes from the password). The HTTP and MQTT transports refuse cleartext endpoints. Use `wss://` unless the endpoint itself is a loopback address. A `ws://` URL to another host sends the key in the clear even when that host forwards to a hub on this machine. `SUGGESTIONS.md` item 6 proposes making that the rule in every SDK.

**Why does the first connect fail with "atomic Noise key publication requires a filesystem supporting hard links"?**
The state folder is on a filesystem without hard links (FAT32, exFAT, some network shares). The static key is published with a hard link so that two processes never replace each other's key (`publish_private` in `src/noise_store.rs`). Keep the state on a filesystem that supports hard links (NTFS, ext4, APFS; not FAT32 or exFAT, even on a local disk). That is the identity file's folder, since `Client::from_file` keeps the key beside it, or the folder you give the transport's `set_noise_state_dir`.
