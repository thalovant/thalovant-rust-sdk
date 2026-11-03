//! Chat apps: the `/v1/bridges` owner routes, their models, and the
//! credentials they carry.
//!
//! Every call is served by a real loopback HTTP peer that records the request
//! as it arrived, so what is asserted is what reached the wire: the method,
//! the escaped path, the query, the exact JSON body, and `If-Match` only when
//! the caller gave an etag. The answers are the API's own shapes, including
//! values this release does not know and members an older API leaves out.

use std::io::{Read, Write};
use std::marker::PhantomData;
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use serde_json::{json, Map, Value};
use thalovant::{
    ApiRefusal, Bridge, BridgeAllowedChat, BridgeChat, BridgeCreate, BridgeCredentials,
    BridgeHoldReason, BridgeListOptions, BridgePhase, BridgeSessionScope, BridgeState,
    BridgeStatusReason, BridgeStatusSource, BridgeUpdate, ChatAppConnector, ChatAppHelper,
    ChatAppStatus, ControlPlane, SecretValue, ThalovantError,
};

/// A fake credential. It must never appear in anything printed.
const FAKE_TOKEN: &str = "123456:FAKE-bot-token-never-printed";
const FAKE_ACCESS_TOKEN: &str = "syt_FAKE_matrix_access_token";
const FAKE_REFRESH_TOKEN: &str = "FAKE-twitch-refresh-token";
const FAKE_CLIENT_SECRET: &str = "FAKE-twitch-client-secret";

/// One scripted answer: a status line, a Content-Type and the body bytes,
/// with a `Content-Length` unless `sized` is false (then the body runs to
/// the end of the connection).
struct Answer {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    sized: bool,
}

impl Answer {
    fn json(status: &'static str, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
            sized: true,
        }
    }

    fn problem(status: &'static str, body: Value) -> Self {
        Self {
            status,
            content_type: "application/problem+json",
            body: body.to_string().into_bytes(),
            sized: true,
        }
    }

    fn image(body: Vec<u8>, sized: bool) -> Self {
        Self {
            status: "200 OK",
            content_type: "image/png",
            body,
            sized,
        }
    }

    fn empty(status: &'static str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: Vec::new(),
            sized: true,
        }
    }
}

/// Read one whole request: its head, then as many body bytes as it promised.
fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let size = stream.read(&mut chunk).expect("read request");
        assert!(size > 0, "request closed before its body completed");
        raw.extend_from_slice(&chunk[..size]);
        if let Some(end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let head = std::str::from_utf8(&raw[..end]).expect("UTF-8 head");
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, length)| length.trim().parse::<usize>().expect("length"))
                .unwrap_or(0);
            if raw.len() >= end + 4 + length {
                return String::from_utf8(raw).expect("UTF-8 request");
            }
        }
    }
}

/// A loopback API that answers `count` requests from `route` and hands back
/// every request it received, in order.
fn serve(
    count: usize,
    route: impl Fn(&str) -> Answer + Send + 'static,
) -> (ControlPlane, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback API");
    let address = listener.local_addr().expect("address");
    let peer = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_full_request(&mut stream);
            let answer = route(&request);
            let length = if answer.sized {
                format!("content-length: {}\r\n", answer.body.len())
            } else {
                String::new()
            };
            let head = format!(
                "HTTP/1.1 {}\r\ncontent-type: {}\r\n{length}connection: close\r\n\r\n",
                answer.status, answer.content_type,
            );
            stream.write_all(head.as_bytes()).expect("write head");
            // A client that refuses a body may close before it is all
            // written.
            let _ = stream.write_all(&answer.body);
            requests.push(request);
        }
        requests
    });
    (
        ControlPlane::new(format!("http://{address}"), Some("owner-token".into())),
        peer,
    )
}

/// The request line, e.g. `PATCH /v1/bridges/b%2F1 HTTP/1.1`.
fn request_line(request: &str) -> &str {
    request.lines().next().expect("request line")
}

/// Every value of one header, lower-cased name.
fn header_values<'a>(request: &'a str, name: &str) -> Vec<&'a str> {
    let head = request.split_once("\r\n\r\n").expect("head").0;
    head.lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .filter(|(header, _)| header.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .collect()
}

fn body(request: &str) -> Value {
    let raw = request.split_once("\r\n\r\n").expect("body").1;
    serde_json::from_str(raw).expect("JSON body")
}

fn has_body(request: &str) -> bool {
    !request.split_once("\r\n\r\n").expect("body").1.is_empty()
}

fn connectors() -> Value {
    json!({"data": [
        {
            "id": "telegram", "name": "Telegram", "mark": "telegram", "status": "offered",
            "description": {"en": "Answer in Telegram groups.", "fr": "Répondre dans les groupes Telegram."},
            "setup_steps": {"en": "1. Open @BotFather.", "fr": null},
            "config_schema": {"type": "object", "properties": {}},
            "secrets": [{"key": "token", "label": {"en": "Bot token"}, "description": null, "required": true}],
            "targets": {"label": {"en": "Chat"}, "pattern": "^-?[0-9]{1,20}$", "max": 50,
                        "lowercase": false, "strip_prefix": null},
            "options": {"require_mention_fixed": false, "session_scopes": ["person", "chat"]},
            "helpers": ["validate", "discover", "test_message", "avatar"],
            "offered": true,
            "version": {"version": "1.4.0", "released_at": "2026-10-30T12:00:00Z"}
        },
        {
            "id": "twitch", "name": "Twitch", "mark": "twitch", "status": "preview",
            "description": {"en": "Answer in a Twitch chat."},
            "setup_steps": {"en": "Register an application."},
            "config_schema": {
                "type": "object", "required": ["bot_login", "client_id"],
                "properties": {
                    "bot_login": {"type": "string", "title": "Bot login", "x-title-fr": "Identifiant du bot"},
                    "client_id": {"type": "string", "title": "Client ID"}
                }
            },
            "secrets": [
                {"key": "refresh_token", "label": {"en": "Refresh token"}},
                {"key": "client_secret", "label": {"en": "Client secret"}, "required": false}
            ],
            "targets": {"label": {"en": "Channel"}, "pattern": "^[a-z0-9_]{4,25}$", "max": 50,
                        "lowercase": true, "strip_prefix": "#"},
            "options": {"require_mention_fixed": false},
            "helpers": ["summarise"],
            "offered": false
        }
    ]})
}

fn networks() -> Value {
    json!({
        "networks": [
            {"network": "telegram", "offered": true, "name": "Telegram", "mark": "telegram", "status": "offered"},
            {"network": "discord", "offered": false}
        ],
        "allowance": {"owner_id": "u-1", "plan": "Home", "paid_plan": true, "limit": 2,
                      "used": 1, "remaining": 1, "can_create": true}
    })
}

/// A bridge as the current API answers it, with every member.
fn bridge() -> Value {
    json!({
        "id": "b/1", "hub_id": "h/1", "owner_id": "u-1", "network": "telegram",
        "connector": {"id": "telegram", "name": "Telegram", "version": "1.4.0"},
        "settings": {}, "secrets_set": ["token"], "state": "active", "resource_name": "bridge-b1",
        "bot": {"user_id": "777", "username": "family_hub_bot", "display_name": "Family hub",
                "application_id": null, "avatar_url": "/v1/bridges/b1/avatar"},
        "invite_links": {"add_to_group": "https://t.me/family_hub_bot?startgroup=thalovant",
                         "direct_chat": "https://t.me/family_hub_bot"},
        "allowed": [{"id": "-1001234567890", "title": "Family", "kind": "supergroup",
                     "guild_id": null, "guild_name": null}],
        "require_mention": true, "session_scope": "person", "paused": false, "hold": null,
        "status": {"phase": "Ready", "ready": true, "reason": "Connected", "message": null,
                   "operator_reason": null, "generation": 3, "observed_generation": 3,
                   "applied": true, "source": "operator"},
        "etag": "etag-1",
        "token_updated_at": "2026-11-01T10:00:00Z", "activated_at": "2026-11-01T10:05:00Z",
        "created_at": "2026-11-01T10:00:00Z", "updated_at": "2026-11-02T08:00:00Z",
        "operation": {"id": "op-1", "kind": "bridge.sync", "aggregate_type": "bridge",
                      "aggregate_id": "b1", "status": "requested", "details": {},
                      "created_at": "2026-11-02T08:00:00Z", "updated_at": "2026-11-02T08:00:00Z",
                      "links": {"self": "/v1/operations/op-1"}}
    })
}

/// A bridge from an API that sends fewer members and values this release
/// does not know.
fn bridge_from_elsewhere() -> Value {
    json!({
        "id": "b2", "hub_id": "h1", "owner_id": "u1", "network": "matrix",
        "state": "archived", "resource_name": "bridge-b2",
        "bot": {"user_id": "bridge:b2", "avatar_url": "/v1/bridges/b2/avatar"},
        "allowed": [{"id": "!room:example.org"}],
        "require_mention": true, "session_scope": "thread", "paused": true,
        "hold": {"reason": "billing_review", "since": "2026-11-02T00:00:00Z"},
        "status": {"phase": "Hibernating", "reason": "QuotaExceeded", "source": "satellite"},
        "etag": "e2", "token_updated_at": "2026-11-01T00:00:00Z",
        "created_at": "2026-11-01T00:00:00Z", "updated_at": "2026-11-02T00:00:00Z"
    })
}

fn chats() -> Value {
    json!({
        "network": "discord",
        "data": [
            {"id": "123", "title": "general", "kind": "text", "guild_id": "9",
             "guild_name": "Home", "guild_icon_url": "https://cdn.example/icon.png", "allowed": true},
            {"id": "456", "title": "news", "kind": "announcement", "encrypted": true, "selectable": false}
        ],
        "note": null
    })
}

/// What the loopback API answers for each route.
fn route(request: &str) -> Answer {
    let line = request_line(request);
    let (method, rest) = line.split_once(' ').expect("method");
    let path = rest.rsplit_once(' ').expect("path").0;
    let route = path.split('?').next().expect("route");
    match (method, route) {
        ("GET", "/v1/bridges/connectors") => Answer::json("200 OK", connectors()),
        ("GET", "/v1/bridges/networks") => Answer::json("200 OK", networks()),
        ("GET", "/v1/bridges") => Answer::json(
            "200 OK",
            json!({"data": [bridge(), bridge_from_elsewhere()]}),
        ),
        ("POST", "/v1/bridges") => Answer::json("201 Created", bridge()),
        ("GET" | "PATCH", "/v1/bridges/b%2F1")
        | ("PUT", "/v1/bridges/b%2F1/token" | "/v1/bridges/b%2F1/secrets") => {
            Answer::json("200 OK", bridge())
        }
        ("GET", "/v1/bridges/b%2F1/chats") => Answer::json("200 OK", chats()),
        ("POST", "/v1/bridges/b%2F1/test") => {
            Answer::json("200 OK", json!({"sent": true, "chat_id": "-1001234567890"}))
        }
        ("GET", "/v1/bridges/b%2F1/avatar") => Answer::image(AVATAR.to_vec(), true),
        ("DELETE", "/v1/bridges/b%2F1") => Answer::empty("204 No Content"),
        _ => panic!("unexpected request: {line}"),
    }
}

/// Not UTF-8, so a text read would change it.
const AVATAR: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff, 0xfe,
];

#[tokio::test]
async fn every_route_sends_its_method_path_query_and_body() {
    let (api, peer) = serve(16, route);

    let found = api.list_chat_app_connectors().await.expect("connectors");
    assert_eq!(found.len(), 2);
    api.get_bridge_networks(None).await.expect("networks");
    let networks = api
        .get_bridge_networks(Some("h/1"))
        .await
        .expect("networks");
    assert_eq!(networks.allowance.remaining, 1);
    assert!(networks.allowance.can_create && networks.allowance.paid_plan);
    assert_eq!(networks.networks[1].status, None);
    assert_eq!(networks.networks[0].status, Some(ChatAppStatus::Offered));

    let listed = api
        .list_bridges(&BridgeListOptions::default())
        .await
        .expect("list");
    assert_eq!(listed.len(), 2);
    api.list_bridges(&BridgeListOptions {
        hub_id: Some("h/1".into()),
        owner_id: Some("u 1".into()),
        limit: Some(500),
    })
    .await
    .expect("list one hub");

    let created = api
        .create_bridge(BridgeCreate::with_token("h/1", "telegram", FAKE_TOKEN))
        .await
        .expect("create with a token");
    assert_eq!(created.etag, "etag-1");
    api.create_bridge(
        BridgeCreate::with_secrets(
            "h/1",
            "matrix",
            [("access_token", SecretValue::from(FAKE_ACCESS_TOKEN))],
        )
        .with_settings([
            ("homeserver", "https://matrix.example.org"),
            ("user_id", "@hub:example.org"),
        ]),
    )
    .await
    .expect("create with secrets and settings");

    api.get_bridge("b/1").await.expect("get");
    api.update_bridge(
        "b/1",
        &BridgeUpdate {
            allowed: Some(vec![
                BridgeAllowedChat::from("-1001234567890"),
                BridgeAllowedChat::from(&listed_chat()),
            ]),
            session_scope: Some(BridgeSessionScope::Chat),
            ..Default::default()
        },
    )
    .await
    .expect("update without an etag");
    api.update_bridge(
        "b/1",
        &BridgeUpdate {
            paused: Some(true),
            etag: Some("etag-1".into()),
            ..Default::default()
        },
    )
    .await
    .expect("pause with an etag");
    api.replace_bridge_token("b/1", FAKE_TOKEN)
        .await
        .expect("replace token");
    api.replace_bridge_secrets("b/1", [("client_secret", FAKE_CLIENT_SECRET)])
        .await
        .expect("replace secrets");
    let chats = api.list_bridge_chats("b/1").await.expect("chats");
    assert_eq!(chats.data.len(), 2);
    let sent = api
        .send_bridge_test_message("b/1", "-1001234567890")
        .await
        .expect("test message");
    assert!(sent.sent);
    assert_eq!(sent.language, "en", "a missing language is English");
    let avatar = api.get_bridge_avatar("b/1").await.expect("avatar");
    assert_eq!(avatar.content, AVATAR);
    assert_eq!(avatar.content_type, "image/png");
    api.delete_bridge("b/1").await.expect("delete");

    let requests = peer.join().expect("loopback API");
    let lines: Vec<&str> = requests
        .iter()
        .map(|request| request_line(request))
        .collect();
    assert_eq!(
        lines,
        [
            "GET /v1/bridges/connectors HTTP/1.1",
            "GET /v1/bridges/networks HTTP/1.1",
            "GET /v1/bridges/networks?hub_id=h%2F1 HTTP/1.1",
            "GET /v1/bridges HTTP/1.1",
            "GET /v1/bridges?hub_id=h%2F1&owner_id=u%201&limit=500 HTTP/1.1",
            "POST /v1/bridges HTTP/1.1",
            "POST /v1/bridges HTTP/1.1",
            "GET /v1/bridges/b%2F1 HTTP/1.1",
            "PATCH /v1/bridges/b%2F1 HTTP/1.1",
            "PATCH /v1/bridges/b%2F1 HTTP/1.1",
            "PUT /v1/bridges/b%2F1/token HTTP/1.1",
            "PUT /v1/bridges/b%2F1/secrets HTTP/1.1",
            "GET /v1/bridges/b%2F1/chats HTTP/1.1",
            "POST /v1/bridges/b%2F1/test HTTP/1.1",
            "GET /v1/bridges/b%2F1/avatar HTTP/1.1",
            "DELETE /v1/bridges/b%2F1 HTTP/1.1",
        ]
    );
    for request in &requests {
        assert_eq!(
            header_values(request, "authorization"),
            ["Bearer owner-token"]
        );
    }
    for index in [0, 1, 2, 3, 4, 7, 12, 14, 15] {
        assert!(!has_body(&requests[index]), "{}", lines[index]);
    }

    assert_eq!(
        body(&requests[5]),
        json!({"hub_id": "h/1", "connector": "telegram", "token": FAKE_TOKEN})
    );
    assert_eq!(
        body(&requests[6]),
        json!({
            "hub_id": "h/1",
            "connector": "matrix",
            "secrets": {"access_token": FAKE_ACCESS_TOKEN},
            "settings": {"homeserver": "https://matrix.example.org", "user_id": "@hub:example.org"},
        })
    );
    // Only what the caller set, and no If-Match without an etag.
    assert_eq!(
        body(&requests[8]),
        json!({
            "allowed": [
                {"id": "-1001234567890"},
                {"id": "123", "title": "general", "kind": "text", "guild_id": "9", "guild_name": "Home"},
            ],
            "session_scope": "chat",
        })
    );
    assert!(header_values(&requests[8], "if-match").is_empty());
    assert_eq!(body(&requests[9]), json!({"paused": true}));
    assert_eq!(header_values(&requests[9], "if-match"), ["etag-1"]);
    assert_eq!(body(&requests[10]), json!({"token": FAKE_TOKEN}));
    assert_eq!(
        body(&requests[11]),
        json!({"secrets": {"client_secret": FAKE_CLIENT_SECRET}})
    );
    assert_eq!(body(&requests[13]), json!({"chat_id": "-1001234567890"}));
    // The picture is asked for as an image, and nothing else asks for one.
    assert_eq!(
        header_values(&requests[14], "accept"),
        ["image/*, application/problem+json"]
    );
    assert_eq!(header_values(&requests[0], "accept"), ["application/json"]);
}

/// The first chat [`chats`] lists, the way `list_bridge_chats` returns it.
fn listed_chat() -> BridgeChat {
    serde_json::from_value(chats()["data"][0].clone()).expect("a chat")
}

#[test]
fn a_bridge_parses_with_every_member() {
    let bridge: Bridge = serde_json::from_value(bridge()).expect("bridge");
    assert_eq!(bridge.state, BridgeState::Active);
    assert_eq!(bridge.session_scope, BridgeSessionScope::Person);
    assert_eq!(bridge.status.phase, BridgePhase::Ready);
    assert_eq!(bridge.status.reason, BridgeStatusReason::Connected);
    assert_eq!(bridge.status.source, BridgeStatusSource::Operator);
    assert!(bridge.status.ready);
    assert_eq!(bridge.status.applied, Some(true));
    assert_eq!(bridge.status.generation, Some(3));
    let connector = bridge.connector.as_ref().expect("connector");
    assert_eq!(connector.version.as_deref(), Some("1.4.0"));
    assert_eq!(bridge.secrets_set, ["token"]);
    assert_eq!(bridge.allowed[0].title.as_deref(), Some("Family"));
    let links = bridge.invite_links.as_ref().expect("invite links");
    assert_eq!(
        links.direct_chat.as_deref(),
        Some("https://t.me/family_hub_bot")
    );
    assert_eq!(bridge.bot.application_id, None);
    let operation = bridge.operation.as_ref().expect("operation");
    assert_eq!(operation.kind, "bridge.sync");
    // Serialising gives the API's shape back.
    let again: Bridge =
        serde_json::from_value(serde_json::to_value(&bridge).expect("serialise")).expect("parse");
    assert_eq!(again, bridge);
}

#[test]
fn unknown_values_and_missing_members_still_parse() {
    let bridge: Bridge = serde_json::from_value(bridge_from_elsewhere()).expect("bridge");
    assert_eq!(bridge.state, BridgeState::Other("archived".into()));
    assert_eq!(bridge.session_scope.as_str(), "thread");
    assert!(!bridge.session_scope.is_known());
    assert_eq!(
        bridge.status.phase,
        BridgePhase::Other("Hibernating".into())
    );
    assert_eq!(
        bridge.status.reason,
        BridgeStatusReason::Other("QuotaExceeded".into())
    );
    assert_eq!(
        bridge.status.source,
        BridgeStatusSource::Other("satellite".into())
    );
    let hold = bridge.hold.as_ref().expect("hold");
    assert_eq!(
        hold.reason,
        BridgeHoldReason::Other("billing_review".into())
    );
    assert!(!bridge.status.ready, "ready defaults to false");
    assert_eq!(bridge.status.applied, None);
    assert_eq!(bridge.connector, None);
    assert!(bridge.settings.is_empty() && bridge.secrets_set.is_empty());
    assert_eq!(bridge.invite_links, None);
    assert_eq!(bridge.activated_at, None);
    assert_eq!(bridge.operation, None);
    assert_eq!(bridge.allowed[0].title, None);
    // An unknown value goes back out exactly as it came in.
    let written = serde_json::to_value(&bridge).expect("serialise");
    assert_eq!(written["status"]["phase"], "Hibernating");
    assert_eq!(written["hold"]["reason"], "billing_review");
    assert_eq!(written["session_scope"], "thread");

    // A known value is never Other, however it was built.
    assert_eq!(BridgePhase::from("Ready"), BridgePhase::Ready);
    assert_eq!(
        BridgeStatusReason::from("TokenRejected".to_string()),
        BridgeStatusReason::TokenRejected
    );
    assert_eq!(BridgeHoldReason::PlanLimit.as_str(), "plan_limit");
    assert_eq!(
        BridgeHoldReason::AccountSuspended.to_string(),
        "account_suspended"
    );
}

#[test]
fn a_connector_parses_with_its_defaults_and_unknown_values() {
    let found: Vec<ChatAppConnector> =
        serde_json::from_value(connectors()["data"].clone()).expect("connectors");
    let telegram = &found[0];
    assert_eq!(telegram.status, ChatAppStatus::Offered);
    assert_eq!(
        telegram.helpers,
        [
            ChatAppHelper::Validate,
            ChatAppHelper::Discover,
            ChatAppHelper::TestMessage,
            ChatAppHelper::Avatar
        ]
    );
    assert_eq!(
        telegram.description.fr.as_deref(),
        Some("Répondre dans les groupes Telegram.")
    );
    assert_eq!(telegram.setup_steps.fr, None);
    assert!(telegram.secrets[0].required);
    assert_eq!(
        telegram
            .version
            .as_ref()
            .map(|release| release.version.as_str()),
        Some("1.4.0")
    );

    let twitch = &found[1];
    assert_eq!(twitch.status, ChatAppStatus::Other("preview".into()));
    assert_eq!(twitch.helpers, [ChatAppHelper::Other("summarise".into())]);
    assert!(twitch.secrets[0].required, "required defaults to true");
    assert!(!twitch.secrets[1].required);
    assert_eq!(twitch.secrets[0].description, None);
    assert!(twitch.targets.lowercase);
    assert_eq!(twitch.targets.strip_prefix.as_deref(), Some("#"));
    assert_eq!(
        twitch.options.session_scopes,
        [BridgeSessionScope::Person, BridgeSessionScope::Chat],
        "session scopes default to both"
    );
    assert_eq!(
        twitch.config_schema["properties"]["bot_login"]["x-title-fr"],
        "Identifiant du bot"
    );
    assert_eq!(twitch.version, None);
    assert!(!twitch.offered);
}

#[test]
fn chats_parse_with_their_defaults() {
    let listed: thalovant::BridgeChatList = serde_json::from_value(chats()).expect("chats");
    assert!(!listed.manual_entry, "manual_entry defaults to false");
    assert_eq!(listed.note, None);
    let (general, news) = (&listed.data[0], &listed.data[1]);
    assert!(general.allowed && general.selectable && !general.encrypted);
    assert!(!news.allowed && !news.selectable && news.encrypted);
    assert_eq!(news.guild_id, None);

    let manual: thalovant::BridgeChatList =
        serde_json::from_value(json!({"network": "matrix", "data": [], "manual_entry": true}))
            .expect("manual entry");
    assert!(manual.manual_entry && manual.data.is_empty());

    // A chosen chat keeps what the listing showed, and sends nothing unset.
    let chosen = BridgeAllowedChat::from(general);
    assert_eq!(chosen.title.as_deref(), Some("general"));
    assert_eq!(
        serde_json::to_value(BridgeAllowedChat::new("!room:example.org")).expect("serialise"),
        json!({"id": "!room:example.org"})
    );
}

#[test]
fn an_update_sends_only_what_was_set_and_never_its_etag() {
    assert_eq!(
        serde_json::to_value(BridgeUpdate::default()).expect("serialise"),
        json!({})
    );
    let resume = BridgeUpdate {
        paused: Some(false),
        require_mention: Some(false),
        settings: Some(Map::from_iter([(
            "homeserver".to_string(),
            json!("https://matrix.example.org"),
        )])),
        etag: Some("etag-9".into()),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&resume).expect("serialise"),
        json!({"paused": false, "require_mention": false,
               "settings": {"homeserver": "https://matrix.example.org"}})
    );
}

/// Whether a type implements a trait, decided at compile time: the inherent
/// constant exists only when the bound holds, and shadows the trait's.
struct Probe<T: ?Sized>(PhantomData<T>);
trait NotSerialize {
    const SERIALIZE: bool = false;
}
impl<T: ?Sized> NotSerialize for Probe<T> {}
impl<T: ?Sized + serde::Serialize> Probe<T> {
    const SERIALIZE: bool = true;
}
trait NotDisplay {
    const DISPLAY: bool = false;
}
impl<T: ?Sized> NotDisplay for Probe<T> {}
impl<T: ?Sized + std::fmt::Display> Probe<T> {
    const DISPLAY: bool = true;
}
trait NotDeserialize {
    const DESERIALIZE: bool = false;
}
impl<T: ?Sized> NotDeserialize for Probe<T> {}
impl<T: serde::de::DeserializeOwned> Probe<T> {
    const DESERIALIZE: bool = true;
}

/// Checked when this file compiles: a secret, and a request that carries
/// one, has no `Serialize`, no `Deserialize` and no `Display`. The first two
/// lines prove the probe can say yes.
const NO_WAY_TO_PRINT_A_SECRET: () = {
    assert!(<Probe<Bridge>>::SERIALIZE && <Probe<Bridge>>::DESERIALIZE);
    assert!(<Probe<BridgePhase>>::DISPLAY);
    assert!(!<Probe<SecretValue>>::SERIALIZE);
    assert!(!<Probe<SecretValue>>::DESERIALIZE);
    assert!(!<Probe<SecretValue>>::DISPLAY);
    assert!(!<Probe<BridgeCreate>>::SERIALIZE);
    assert!(!<Probe<BridgeCredentials>>::SERIALIZE);
};

#[test]
fn a_secret_cannot_be_printed_or_serialised() {
    #[allow(clippy::let_unit_value)]
    let () = NO_WAY_TO_PRINT_A_SECRET;
    // What the type system says is checked at compile time, by
    // `NO_WAY_TO_PRINT_A_SECRET` below; this is what the values do.
    let secret = SecretValue::from(FAKE_TOKEN);
    assert_eq!(format!("{secret:?}"), "<redacted>");
    assert_eq!(format!("{secret:#?}"), "<redacted>");
    assert_eq!(secret.expose(), FAKE_TOKEN);
    assert_eq!(
        SecretValue::from(FAKE_TOKEN.to_string()).expose(),
        FAKE_TOKEN
    );
    assert!(SecretValue::new("  ").is_blank());

    let with_token = BridgeCreate::with_token("h1", "telegram", FAKE_TOKEN);
    let with_secrets = BridgeCreate::with_secrets(
        "h1",
        "twitch",
        [
            ("refresh_token", FAKE_REFRESH_TOKEN),
            ("client_secret", FAKE_CLIENT_SECRET),
        ],
    )
    .with_settings([("bot_login", "myhubbot"), ("client_id", "app-1")]);
    // A failure names the form, never what it printed: that could be the
    // credential itself.
    let forms = [
        format!("{with_token:?}"),
        format!("{with_token:#?}"),
        format!("{with_secrets:?}"),
        format!("{with_secrets:#?}"),
        format!("{:?}", with_secrets.credentials),
    ];
    for (form, printed) in forms.iter().enumerate() {
        for fake in [FAKE_TOKEN, FAKE_REFRESH_TOKEN, FAKE_CLIENT_SECRET] {
            assert!(!printed.contains(fake), "form {form} printed a credential");
        }
        assert!(
            printed.contains("<redacted>"),
            "form {form} shows no marker"
        );
    }
    // The keys and the settings are not secret, and still show.
    let printed = format!("{with_secrets:?}");
    assert!(printed.contains("refresh_token") && printed.contains("myhubbot"));
}

#[tokio::test]
async fn a_refused_create_never_prints_the_credentials_it_sent() {
    // An API that echoed the request would put the token in its body; the
    // SDK's message, Debug and problem Debug must still not show it.
    let (api, peer) = serve(2, |request| {
        let sent = body(request);
        let echoed = sent["token"]
            .as_str()
            .or(sent["secrets"]["access_token"].as_str())
            .unwrap_or_default()
            .to_string();
        Answer::problem(
            "422 Unprocessable Entity",
            json!({
                "type": "about:blank", "title": "HTTPException", "status": 422,
                "detail": "The network refused this token.",
                "code": "bridge_token_invalid",
                "token": echoed,
                "errors": [{"loc": ["body", "token"], "input": echoed}],
            }),
        )
    });
    let token_error = api
        .create_bridge(BridgeCreate::with_token("h1", "telegram", FAKE_TOKEN))
        .await
        .expect_err("refused");
    let secrets_error = api
        .create_bridge(BridgeCreate::with_secrets(
            "h1",
            "matrix",
            [("access_token", FAKE_ACCESS_TOKEN)],
        ))
        .await
        .expect_err("refused");
    peer.join().expect("loopback API");
    for (error, fake) in [
        (&token_error, FAKE_TOKEN),
        (&secrets_error, FAKE_ACCESS_TOKEN),
    ] {
        assert_eq!(error.api_code(), Some("bridge_token_invalid"));
        let problem = error.api_problem().expect("problem");
        let forms = [
            error.to_string(),
            format!("{error:?}"),
            format!("{error:#?}"),
            format!("{problem:?}"),
        ];
        for (form, printed) in forms.iter().enumerate() {
            // Named, not printed: the form could hold the credential.
            assert!(!printed.contains(fake), "form {form} printed a credential");
        }
    }
}

/// What a refusal says: status, code, fields and kind.
type Refusal<'a> = (
    Option<u16>,
    Option<&'a str>,
    Vec<&'a str>,
    Option<ApiRefusal>,
);

#[tokio::test]
async fn refusals_carry_their_code_fields_and_kind() {
    let answers: Vec<Answer> = vec![
        // A settings refusal names the settings, never their values.
        Answer::problem(
            "422 Unprocessable Entity",
            json!({"detail": "The homeserver is not a public address.",
                   "code": "bridge_url_not_public", "fields": ["homeserver", 7, null, "user_id"]}),
        ),
        // The same inside an object detail, as FastAPI sends it unlifted.
        Answer::problem(
            "422 Unprocessable Entity",
            json!({"detail": {"detail": "Unknown secrets.", "code": "bridge_secrets_invalid",
                              "fields": ["client_secret"]}}),
        ),
        Answer::problem(
            "402 Payment Required",
            json!({"detail": "Chat apps need a paid plan."}),
        ),
        Answer::problem(
            "403 Forbidden",
            json!({"detail": "Your plan allows 2 chat bridges.", "code": "plan_limit",
                   "resource": "chat_bridge", "limit": 2, "used": 2}),
        ),
        Answer::problem("403 Forbidden", json!({"detail": "Insufficient scopes"})),
        Answer::problem("401 Unauthorized", json!({"detail": "Invalid token"})),
        Answer::problem(
            "412 Precondition Failed",
            json!({"detail": "ETag mismatch"}),
        ),
        Answer::problem(
            "429 Too Many Requests",
            json!({"detail": "Chats were listed many times just now.", "code": "bridge_rate_limited"}),
        ),
        Answer::problem(
            "409 Conflict",
            json!({"detail": "This bot is already connected to a hub.",
                   "code": "bridge_bot_in_use", "bridge_id": "b-other"}),
        ),
        Answer::problem(
            "404 Not Found",
            json!({"detail": "This bot has no picture"}),
        ),
    ];
    let count = answers.len();
    let answers = std::sync::Mutex::new(answers.into_iter());
    let (api, peer) = serve(count, move |_| {
        answers.lock().expect("answers").next().expect("an answer")
    });
    let mut errors = Vec::new();
    for _ in 0..9 {
        errors.push(api.get_bridge("b1").await.expect_err("refused"));
    }
    errors.push(api.get_bridge_avatar("b1").await.expect_err("no picture"));
    peer.join().expect("loopback API");

    let summary: Vec<Refusal<'_>> = errors
        .iter()
        .map(|error| {
            (
                error.status_code(),
                error.api_code(),
                error.api_fields(),
                error.api_refusal(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                Some(422),
                Some("bridge_url_not_public"),
                vec!["homeserver", "user_id"],
                None
            ),
            (
                Some(422),
                Some("bridge_secrets_invalid"),
                vec!["client_secret"],
                None
            ),
            (Some(402), None, vec![], Some(ApiRefusal::Plan)),
            (
                Some(403),
                Some("plan_limit"),
                vec![],
                Some(ApiRefusal::Plan)
            ),
            (Some(403), None, vec![], Some(ApiRefusal::Auth)),
            (Some(401), None, vec![], Some(ApiRefusal::Auth)),
            (Some(412), None, vec![], None),
            (Some(429), Some("bridge_rate_limited"), vec![], None),
            (Some(409), Some("bridge_bot_in_use"), vec![], None),
            (Some(404), None, vec![], None),
        ]
    );
    for error in &errors {
        assert!(
            matches!(error, ThalovantError::ApiResponse { .. }),
            "{error}"
        );
    }
    assert_eq!(errors[6].api_detail(), Some("ETag mismatch"));
    assert_eq!(
        errors[3]
            .api_problem()
            .and_then(|problem| problem.get("resource")),
        Some(&json!("chat_bridge"))
    );
    assert_eq!(
        errors[8]
            .api_problem()
            .and_then(|problem| problem.get("bridge_id")),
        Some(&json!("b-other"))
    );
    assert_eq!(errors[9].api_detail(), Some("This bot has no picture"));
    // Fields are read from the problem itself too, and are empty elsewhere.
    assert_eq!(
        errors[0].api_problem().expect("problem").fields(),
        ["homeserver", "user_id"]
    );
    assert!(ThalovantError::Api("offline".into())
        .api_fields()
        .is_empty());
}

#[tokio::test]
async fn bad_arguments_fail_before_any_request() {
    // Nothing listens here: a request that went out would be unreachable.
    let api = ControlPlane::new("http://127.0.0.1:1", Some("owner-token".into()));
    let mut failures = Vec::new();
    for limit in [0, 501] {
        failures.push(
            api.list_bridges(&BridgeListOptions {
                limit: Some(limit),
                ..Default::default()
            })
            .await
            .expect_err("limit out of range"),
        );
    }
    failures.push(
        api.get_bridge_networks(Some(" "))
            .await
            .expect_err("blank hub"),
    );
    for (hub_id, owner_id) in [(Some(""), None), (None, Some(" "))] {
        failures.push(
            api.list_bridges(&BridgeListOptions {
                hub_id: hub_id.map(str::to_string),
                owner_id: owner_id.map(str::to_string),
                limit: None,
            })
            .await
            .expect_err("blank filter"),
        );
    }
    let empty: [(&str, &str); 0] = [];
    failures.push(
        api.create_bridge(BridgeCreate::with_secrets("h1", "matrix", empty))
            .await
            .expect_err("no secrets"),
    );
    failures.push(
        api.create_bridge(BridgeCreate::with_token(" ", "telegram", FAKE_TOKEN))
            .await
            .expect_err("no hub"),
    );
    failures.push(
        api.create_bridge(BridgeCreate::with_token("h1", "", FAKE_TOKEN))
            .await
            .expect_err("no connector"),
    );
    failures.push(
        api.create_bridge(BridgeCreate::with_token("h1", "telegram", " "))
            .await
            .expect_err("blank token"),
    );
    failures.push(api.get_bridge("").await.expect_err("no id"));
    failures.push(api.delete_bridge(" ").await.expect_err("no id"));
    failures.push(
        api.update_bridge(
            "b1",
            &BridgeUpdate {
                paused: Some(true),
                etag: Some(" ".into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("blank etag"),
    );
    failures.push(
        api.replace_bridge_token("b1", "")
            .await
            .expect_err("blank token"),
    );
    failures.push(
        api.replace_bridge_secrets("b1", empty)
            .await
            .expect_err("no secrets"),
    );
    failures.push(
        api.send_bridge_test_message("b1", "")
            .await
            .expect_err("no chat"),
    );
    failures.push(api.get_bridge_avatar("").await.expect_err("no id"));
    // Named by position, not printed: some were built with a credential.
    for (case, failure) in failures.iter().enumerate() {
        assert!(matches!(failure, ThalovantError::Api(_)), "case {case}");
        assert!(
            !failure.is_api_unreachable(),
            "case {case}: a request went out"
        );
        assert_eq!(failure.status_code(), None, "case {case}");
    }
    assert_eq!(
        failures[0].to_string(),
        "api error: limit must be from 1 to 500"
    );
}

#[tokio::test]
async fn a_picture_is_read_whole_and_never_past_its_cap() {
    // The API serves at most 1 MiB; the SDK reads at most 4 MiB.
    const CAP: usize = 4 * 1024 * 1024;
    let big = vec![0x5a_u8; CAP + 1];
    let small = AVATAR.to_vec();
    let answers = std::sync::Mutex::new(
        vec![
            // Read to the end of the connection: no Content-Length.
            Answer::image(small.clone(), false),
            Answer::image(vec![0xa5_u8; CAP], true),
            // Refused by its Content-Length, before the body is read.
            Answer::image(big.clone(), true),
            // Refused while it is read.
            Answer::image(big, false),
        ]
        .into_iter(),
    );
    let (api, peer) = serve(4, move |_| {
        answers.lock().expect("answers").next().expect("an answer")
    });
    let unsized_picture = api.get_bridge_avatar("b1").await.expect("unsized picture");
    assert_eq!(unsized_picture.content, small);
    assert_eq!(unsized_picture.content_type, "image/png");
    let at_cap = api
        .get_bridge_avatar("b1")
        .await
        .expect("picture at the cap");
    assert_eq!(at_cap.content.len(), CAP);
    assert!(format!("{at_cap:?}").contains(&format!("[{CAP} bytes]")));
    for case in ["announced", "streamed"] {
        let error = api.get_bridge_avatar("b1").await.expect_err(case);
        assert!(matches!(error, ThalovantError::Api(_)), "{case}");
        assert!(!error.is_api_unreachable(), "{case}");
        assert_eq!(
            error.to_string(),
            format!("api error: the answer is larger than {CAP} bytes"),
            "{case}"
        );
    }
    peer.join().expect("loopback API");
}
