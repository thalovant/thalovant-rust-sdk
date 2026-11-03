//! Chat apps: a hub answering in a chat network.
//!
//! A hub owner connects a chat network (Telegram and Discord are offered;
//! Matrix, Mattermost and Twitch are in beta) to a hub, and the hub answers
//! there. Each connection is a *bridge*. The calls live on
//! [`ControlPlane`](crate::ControlPlane), under the API's `/v1/bridges`
//! routes:
//!
//! | Call | Route |
//! | :-- | :-- |
//! | [`list_chat_app_connectors`](crate::ControlPlane::list_chat_app_connectors) | `GET /v1/bridges/connectors` |
//! | [`get_bridge_networks`](crate::ControlPlane::get_bridge_networks) | `GET /v1/bridges/networks` |
//! | [`list_bridges`](crate::ControlPlane::list_bridges) | `GET /v1/bridges` |
//! | [`create_bridge`](crate::ControlPlane::create_bridge) | `POST /v1/bridges` |
//! | [`get_bridge`](crate::ControlPlane::get_bridge) | `GET /v1/bridges/{id}` |
//! | [`update_bridge`](crate::ControlPlane::update_bridge) | `PATCH /v1/bridges/{id}` |
//! | [`replace_bridge_token`](crate::ControlPlane::replace_bridge_token) | `PUT /v1/bridges/{id}/token` |
//! | [`replace_bridge_secrets`](crate::ControlPlane::replace_bridge_secrets) | `PUT /v1/bridges/{id}/secrets` |
//! | [`list_bridge_chats`](crate::ControlPlane::list_bridge_chats) | `GET /v1/bridges/{id}/chats` |
//! | [`send_bridge_test_message`](crate::ControlPlane::send_bridge_test_message) | `POST /v1/bridges/{id}/test` |
//! | [`get_bridge_avatar`](crate::ControlPlane::get_bridge_avatar) | `GET /v1/bridges/{id}/avatar` |
//! | [`delete_bridge`](crate::ControlPlane::delete_bridge) | `DELETE /v1/bridges/{id}` |
//!
//! Every answer is typed. Timestamps stay the RFC 3339 strings the API
//! sent, and every enum keeps a value this release does not know in its
//! `Other` variant rather than failing, so a reason or a phase the API adds
//! later still parses.
//!
//! A bot's credentials go in as [`SecretValue`], whose `Debug` prints
//! `<redacted>` and which has no `Display` and no `Serialize`: the SDK reads
//! the value only to build the request body, and never puts that body in an
//! error, a log line or debug output.
//!
//! # Example
//!
//! Take up a Telegram bridge an earlier run left in setup, or read the
//! allowance (a Free plan allows no bridges) and connect a bot. Then answer
//! in the one chat the owner picked, which starts the bridge, and say hello.
//! Matrix and Twitch take named secrets and settings instead of one token.
//!
//! ```no_run
//! use thalovant::{
//!     BridgeAllowedChat, BridgeCreate, BridgeListOptions, BridgeState, BridgeStatusReason,
//!     BridgeUpdate, ControlPlane, SecretValue,
//! };
//!
//! # async fn run() -> thalovant::Result<()> {
//! let control = ControlPlane::with_access_token("api-token");
//! let hub_id = "hub-id";
//! let earlier = control
//!     .list_bridges(&BridgeListOptions {
//!         hub_id: Some(hub_id.into()),
//!         ..Default::default()
//!     })
//!     .await?
//!     .into_iter()
//!     .find(|bridge| bridge.network == "telegram");
//! let bridge = match earlier {
//!     Some(bridge) if bridge.state == BridgeState::Setup => bridge,
//!     // Already answering, or created elsewhere: nothing to set up.
//!     Some(_) => return Ok(()),
//!     None => {
//!         let networks = control.get_bridge_networks(Some(hub_id)).await?;
//!         if !networks.allowance.can_create {
//!             return Ok(());
//!         }
//!         let token =
//!             SecretValue::from(std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default());
//!         control
//!             .create_bridge(BridgeCreate::with_token(hub_id, "telegram", token))
//!             .await?
//!     }
//! };
//!
//! // Answer only where the owner said: never every chat the bot can see.
//! let chats = control.list_bridge_chats(&bridge.id).await?;
//! let wanted = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
//! let Some(chat) = chats
//!     .data
//!     .iter()
//!     .find(|chat| chat.selectable && chat.id == wanted)
//! else {
//!     // Mention the bot in that chat first, then run again.
//!     return Ok(());
//! };
//! let bridge = control
//!     .update_bridge(
//!         &bridge.id,
//!         &BridgeUpdate {
//!             allowed: Some(vec![BridgeAllowedChat::from(chat)]),
//!             etag: Some(bridge.etag.clone()),
//!             ..Default::default()
//!         },
//!     )
//!     .await?;
//! control.send_bridge_test_message(&bridge.id, &chat.id).await?;
//!
//! // Matrix: a homeserver and a user id, with an access token. Its chats
//! // are typed, not listed: room ids like `!abc:example.org`.
//! control
//!     .create_bridge(
//!         BridgeCreate::with_secrets(
//!             hub_id,
//!             "matrix",
//!             [("access_token", SecretValue::from("matrix-access-token"))],
//!         )
//!         .with_settings([
//!             ("homeserver", "https://matrix.example.org"),
//!             ("user_id", "@hub:example.org"),
//!         ]),
//!     )
//!     .await?;
//!
//! // Twitch: a bot login and a client id, with a refresh token and a client
//! // secret. Its chats are channel logins (`#Name` is stored as `name`).
//! control
//!     .create_bridge(
//!         BridgeCreate::with_secrets(
//!             hub_id,
//!             "twitch",
//!             [
//!                 ("refresh_token", SecretValue::from("twitch-refresh-token")),
//!                 ("client_secret", SecretValue::from("twitch-client-secret")),
//!             ],
//!         )
//!         .with_settings([("bot_login", "myhubbot"), ("client_id", "your-client-id")]),
//!     )
//!     .await?;
//!
//! // Pause, then read why it is not answering.
//! let paused = control
//!     .update_bridge(
//!         &bridge.id,
//!         &BridgeUpdate {
//!             paused: Some(true),
//!             ..Default::default()
//!         },
//!     )
//!     .await?;
//! if paused.status.reason == BridgeStatusReason::TokenRejected {
//!     control
//!         .replace_bridge_token(&paused.id, SecretValue::from("new-token"))
//!         .await?;
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::control::OperationResource;

/// A credential: a bot token, an access token, a client secret.
///
/// Its `Debug` prints `<redacted>`, never the value. It has no `Display`,
/// no `Serialize` and no `Deserialize`, so it cannot reach a log line, a
/// format string or a JSON document by accident. [`SecretValue::expose`]
/// reads the value; the SDK calls it only to build a request body.
///
/// The value is held in an ordinary `String`, which is not wiped from
/// memory when it is dropped.
///
/// ```
/// use thalovant::SecretValue;
///
/// let token = SecretValue::from("123456:example-token");
/// assert_eq!(format!("{token:?}"), "<redacted>");
/// assert_eq!(token.expose(), "123456:example-token");
/// ```
#[derive(Clone)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wrap a credential.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The credential itself. Never log or print what this returns.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the credential is empty or only whitespace.
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(crate::redact::REDACTED)
    }
}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretValue {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<&String> for SecretValue {
    fn from(value: &String) -> Self {
        Self(value.clone())
    }
}

/// A string-valued enum that keeps values this release does not know.
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $wire:literal, )+
        }
    ) => {
        $(#[$meta])*
        ///
        /// A value this release does not know is kept, as sent, in `Other`.
        /// `#[non_exhaustive]`: a later release may name more values, which
        /// then stop arriving as `Other`. Build one from a string with
        /// `From<&str>`, which never gives `Other` for a known value.
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )+
            /// A value this release does not know, exactly as the API sent it.
            Other(String),
        }

        impl $name {
            /// The value as the API spells it.
            pub fn as_str(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Other(value) => value,
                }
            }

            /// Whether this release knows the value (it is not `Other`).
            pub fn is_known(&self) -> bool {
                !matches!(self, Self::Other(_))
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                match value {
                    $( $wire => Self::$variant, )+
                    other => Self::Other(other.to_string()),
                }
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                match value.as_str() {
                    $( $wire => Self::$variant, )+
                    _ => Self::Other(value),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                String::deserialize(deserializer).map(Self::from)
            }
        }
    };
}

open_enum! {
    /// Whether a bridge runs: `setup` until its first chats are chosen,
    /// then `active`.
    pub enum BridgeState {
        /// Created; nothing runs until [`BridgeUpdate::allowed`] is first set.
        Setup = "setup",
        /// Chats were chosen and the bridge runs (or is paused).
        Active = "active",
    }
}

open_enum! {
    /// Who shares a conversation with the hub in a chat.
    pub enum BridgeSessionScope {
        /// One conversation per person, wherever they write.
        Person = "person",
        /// One conversation per chat, shared by everybody in it.
        Chat = "chat",
    }
}

open_enum! {
    /// Where a bridge is in its life, as the platform reports it.
    pub enum BridgePhase {
        /// No chats chosen yet.
        Setup = "Setup",
        /// Rolling out, or not reported yet.
        Pending = "Pending",
        /// Running.
        Ready = "Ready",
        /// Running, but not as asked.
        Degraded = "Degraded",
        /// Stopped by a pause.
        Paused = "Paused",
        /// Not running.
        Error = "Error",
    }
}

open_enum! {
    /// The stable, customer-safe reason behind a bridge's status: what to
    /// show the owner, and what they can do about it.
    pub enum BridgeStatusReason {
        /// No chats chosen yet: set [`BridgeUpdate::allowed`].
        Setup = "Setup",
        /// Rolling out, or not reported yet: wait.
        Starting = "Starting",
        /// Running and answering.
        Connected = "Connected",
        /// Paused, by the owner or by a [`BridgeHold`].
        Paused = "Paused",
        /// Rolled out but not running, or a platform-side error.
        Unavailable = "Unavailable",
        /// The stored credentials have not reached the bridge yet; replacing
        /// them ([`ControlPlane::replace_bridge_token`](crate::ControlPlane::replace_bridge_token))
        /// restores them.
        TokenMissing = "TokenMissing",
        /// The network refused the bridge's credentials: paste new ones.
        TokenRejected = "TokenRejected",
    }
}

open_enum! {
    /// Where a bridge's status came from.
    pub enum BridgeStatusSource {
        /// The bridge's own lifecycle (setup).
        Lifecycle = "lifecycle",
        /// The platform's running bridge.
        Operator = "operator",
        /// The platform's latest recorded conditions.
        Watcher = "watcher",
        /// Nothing reported yet.
        Unavailable = "unavailable",
    }
}

open_enum! {
    /// Why the platform paused a bridge.
    pub enum BridgeHoldReason {
        /// The owner's plan allows fewer running bridges. Resume it
        /// (`paused: Some(false)`) once the plan has room; holds also lift
        /// on their own, oldest first, when it does.
        PlanLimit = "plan_limit",
        /// The account was deactivated; the bridge resumes when it is
        /// reactivated, as far as the plan allows.
        AccountSuspended = "account_suspended",
    }
}

open_enum! {
    /// How far a chat app is offered.
    pub enum ChatAppStatus {
        /// Generally available.
        Offered = "offered",
        /// Offered as a preview.
        Beta = "beta",
        /// Not listed (platform staff only).
        Hidden = "hidden",
    }
}

open_enum! {
    /// The built-in help the API gives a chat app. A connector without a
    /// helper uses the generic flow instead.
    pub enum ChatAppHelper {
        /// The API checks the bot's token with the network when it is given.
        Validate = "validate",
        /// [`ControlPlane::list_bridge_chats`](crate::ControlPlane::list_bridge_chats)
        /// lists the chats the bot is in; without it the owner types the ids.
        Discover = "discover",
        /// [`ControlPlane::send_bridge_test_message`](crate::ControlPlane::send_bridge_test_message)
        /// works; without it the API answers 409 `bridge_helper_unavailable`.
        TestMessage = "test_message",
        /// [`ControlPlane::get_bridge_avatar`](crate::ControlPlane::get_bridge_avatar)
        /// can serve the bot's picture; without it the API answers 404.
        Avatar = "avatar",
    }
}

/// Text in English, and in French when there is a translation.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct LocalizedText {
    /// The English text.
    pub en: String,
    /// The French text, when there is one.
    #[serde(default)]
    pub fr: Option<String>,
}

/// One secret a chat app needs, such as a bot token or a client secret.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChatAppSecretField {
    /// The key to send it under in [`BridgeCredentials::Secrets`].
    pub key: String,
    /// The form label.
    pub label: LocalizedText,
    /// A longer explanation, when there is one.
    #[serde(default)]
    pub description: Option<LocalizedText>,
    /// Whether a bridge needs it (true unless the API says otherwise).
    #[serde(default = "default_true")]
    pub required: bool,
}

/// What a chat app's chat ids look like, for an owner who types them.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChatAppTargets {
    /// What the network calls a chat (a group, a room, a channel).
    pub label: LocalizedText,
    /// An anchored regular expression every chat id matches.
    pub pattern: String,
    /// How many chats one bridge answers in at most.
    pub max: u32,
    /// Whether ids are stored lower-case (Twitch logins): fold before
    /// matching.
    #[serde(default)]
    pub lowercase: bool,
    /// A prefix owners type that the id does not carry (Twitch's `#`):
    /// drop it before matching.
    #[serde(default)]
    pub strip_prefix: Option<String>,
}

/// The bridge options a chat app supports.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChatAppOptions {
    /// True when the network only lets the bot read what mentions it
    /// (Discord): `require_mention: false` is refused with 422
    /// `bridge_mention_required`.
    pub require_mention_fixed: bool,
    /// The session scopes the bridge supports.
    #[serde(default = "default_session_scopes")]
    pub session_scopes: Vec<BridgeSessionScope>,
}

/// The released version new bridges of a chat app run.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChatAppRelease {
    /// The version.
    pub version: String,
    /// When it was released, as the API sent it.
    #[serde(default)]
    pub released_at: Option<String>,
}

/// A chat app a bridge can connect to, as
/// [`ControlPlane::list_chat_app_connectors`](crate::ControlPlane::list_chat_app_connectors)
/// lists it: its copy, its settings form, its secrets, its chat ids and its
/// helpers.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct ChatAppConnector {
    /// The connector id to create a bridge with, such as `telegram`.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// The id of its brand mark in the console.
    pub mark: String,
    /// How far it is offered.
    pub status: ChatAppStatus,
    /// What it is.
    pub description: LocalizedText,
    /// How to set a bot up for it (Markdown).
    pub setup_steps: LocalizedText,
    /// The settings form, a JSON Schema object: each property's `title` is
    /// English and `x-title-fr` French.
    #[serde(default)]
    pub config_schema: Map<String, Value>,
    /// The secrets a bridge needs.
    #[serde(default)]
    pub secrets: Vec<ChatAppSecretField>,
    /// What its chat ids look like.
    pub targets: ChatAppTargets,
    /// The options it supports.
    pub options: ChatAppOptions,
    /// The built-in help the API gives it.
    #[serde(default)]
    pub helpers: Vec<ChatAppHelper>,
    /// Whether a bridge can be created for it now.
    pub offered: bool,
    /// The version new bridges run, when one is released.
    #[serde(default)]
    pub version: Option<ChatAppRelease>,
}

/// One network in [`BridgeNetworks`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeNetworkOffer {
    /// The connector id, such as `telegram`.
    pub network: String,
    /// Whether a bridge can be created for it now.
    pub offered: bool,
    /// Its display name.
    #[serde(default)]
    pub name: Option<String>,
    /// The id of its brand mark in the console.
    #[serde(default)]
    pub mark: Option<String>,
    /// `offered` or `beta`.
    #[serde(default)]
    pub status: Option<ChatAppStatus>,
}

/// How many bridges an account may have, and whether it may add one now.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeAllowance {
    /// The account the bridges count against: the hub's owner when a hub
    /// was named, else the caller.
    pub owner_id: String,
    /// The plan's name.
    pub plan: String,
    /// False on Free, where creating a bridge answers 402 whatever `limit`
    /// says.
    pub paid_plan: bool,
    /// How many bridges the plan allows.
    pub limit: u64,
    /// How many the account has, in setup or active.
    pub used: u64,
    /// How many more it may add.
    pub remaining: u64,
    /// Whether creating a bridge would pass the plan checks now. Says
    /// nothing about the token or the network.
    pub can_create: bool,
}

/// Which networks are offered, and the allowance.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeNetworks {
    /// Each network the platform lists.
    #[serde(default)]
    pub networks: Vec<BridgeNetworkOffer>,
    /// How many bridges the account may have.
    pub allowance: BridgeAllowance,
}

/// The connector a bridge runs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeConnectorRef {
    /// The connector id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// The release the bridge last ran with, if any.
    #[serde(default)]
    pub version: Option<String>,
}

/// The bot a bridge speaks as.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeBot {
    /// The bot's id on its network (`bridge:<id>` for a connector the API
    /// cannot ask).
    pub user_id: String,
    /// Its user name, when known.
    #[serde(default)]
    pub username: Option<String>,
    /// Its display name, when known.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Discord only: the application its invite link authorises.
    #[serde(default)]
    pub application_id: Option<String>,
    /// The API path of its picture; read it with
    /// [`ControlPlane::get_bridge_avatar`](crate::ControlPlane::get_bridge_avatar).
    pub avatar_url: String,
}

/// Links that add the bot to a chat (Telegram and Discord).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeInviteLinks {
    /// Add the bot to a group (Telegram) or a server (Discord).
    #[serde(default)]
    pub add_to_group: Option<String>,
    /// Open a direct chat with the bot (Telegram).
    #[serde(default)]
    pub direct_chat: Option<String>,
}

/// A chat a bridge may answer in.
///
/// Build one from an id (`BridgeAllowedChat::from("-1001234567890")`), or
/// from a chat [`ControlPlane::list_bridge_chats`](crate::ControlPlane::list_bridge_chats)
/// found (`BridgeAllowedChat::from(&chat)`), which keeps its title, kind and
/// server. Everything but `id` is for display.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeAllowedChat {
    /// The chat, channel or room id.
    pub id: String,
    /// Its title when it was picked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Its kind on the network (Telegram: private, group, supergroup,
    /// channel; Discord: text, announcement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Discord: the server the channel belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    /// Discord: that server's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_name: Option<String>,
}

impl BridgeAllowedChat {
    /// A chat by its id alone.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }
}

impl From<&str> for BridgeAllowedChat {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl From<String> for BridgeAllowedChat {
    fn from(id: String) -> Self {
        Self::new(id)
    }
}

impl From<&BridgeChat> for BridgeAllowedChat {
    fn from(chat: &BridgeChat) -> Self {
        Self {
            id: chat.id.clone(),
            title: Some(chat.title.clone()),
            kind: Some(chat.kind.clone()),
            guild_id: chat.guild_id.clone(),
            guild_name: chat.guild_name.clone(),
        }
    }
}

/// Why the platform paused a bridge, and since when.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeHold {
    /// Why.
    pub reason: BridgeHoldReason,
    /// Since when, as the API sent it.
    pub since: String,
}

/// How a bridge is doing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeStatus {
    /// Where it is in its life.
    pub phase: BridgePhase,
    /// Whether it runs and answers.
    #[serde(default)]
    pub ready: bool,
    /// The stable reason to show the owner; see [`BridgeStatusReason`].
    pub reason: BridgeStatusReason,
    /// The platform's own sentence: platform staff only, `None` for
    /// everybody else.
    #[serde(default)]
    pub message: Option<String>,
    /// The platform's own reason code: platform staff only.
    #[serde(default)]
    pub operator_reason: Option<String>,
    /// The generation of the running bridge's configuration.
    #[serde(default)]
    pub generation: Option<i64>,
    /// The generation the platform last applied.
    #[serde(default)]
    pub observed_generation: Option<i64>,
    /// True once what the bridge asks for (chats, mention rule, scope,
    /// pause, version) is applied; false while a change is on its way;
    /// `None` in setup or when that is not known.
    #[serde(default)]
    pub applied: Option<bool>,
    /// Where this status came from.
    pub source: BridgeStatusSource,
}

/// A bridge: a hub answering in one chat network through one bot.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct Bridge {
    /// The bridge id.
    pub id: String,
    /// The hub that answers.
    pub hub_id: String,
    /// The account it counts against.
    pub owner_id: String,
    /// The connector id, such as `telegram`.
    pub network: String,
    /// The connector it runs (`None` from an API that does not send it).
    #[serde(default)]
    pub connector: Option<BridgeConnectorRef>,
    /// Its non-secret settings, such as a Matrix homeserver.
    #[serde(default)]
    pub settings: Map<String, Value>,
    /// Which secrets are stored, by key; never their values.
    #[serde(default)]
    pub secrets_set: Vec<String>,
    /// Whether it runs.
    pub state: BridgeState,
    /// Its name on the platform.
    pub resource_name: String,
    /// The bot it speaks as.
    pub bot: BridgeBot,
    /// Links that add the bot to a chat; `None` for a connector whose setup
    /// steps say how instead.
    #[serde(default)]
    pub invite_links: Option<BridgeInviteLinks>,
    /// The chats it answers in.
    #[serde(default)]
    pub allowed: Vec<BridgeAllowedChat>,
    /// Whether it answers only what mentions the bot.
    pub require_mention: bool,
    /// Who shares a conversation.
    pub session_scope: BridgeSessionScope,
    /// Whether it is paused, by the owner or by a hold.
    pub paused: bool,
    /// Set when the platform paused it; `None` for the owner's own pause.
    #[serde(default)]
    pub hold: Option<BridgeHold>,
    /// How it is doing.
    pub status: BridgeStatus,
    /// Its current version, for [`BridgeUpdate::etag`].
    pub etag: String,
    /// When its credentials were last set.
    pub token_updated_at: String,
    /// When it first became active.
    #[serde(default)]
    pub activated_at: Option<String>,
    /// When it was created.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// The platform operation this answer queued, when it queued one.
    ///
    /// Also `None` when the API sent an operation this release cannot read,
    /// such as one with a status [`OperationStatus`](crate::OperationStatus)
    /// does not name: the bridge itself still parses.
    #[serde(default, deserialize_with = "readable_operation")]
    pub operation: Option<OperationResource>,
}

/// A chat the bot can answer in, from
/// [`ControlPlane::list_bridge_chats`](crate::ControlPlane::list_bridge_chats).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeChat {
    /// The chat, channel or room id.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its kind on the network.
    pub kind: String,
    /// Discord: the server the channel belongs to.
    #[serde(default)]
    pub guild_id: Option<String>,
    /// Discord: that server's name.
    #[serde(default)]
    pub guild_name: Option<String>,
    /// Discord: that server's icon.
    #[serde(default)]
    pub guild_icon_url: Option<String>,
    /// Whether the bridge already answers in it.
    #[serde(default)]
    pub allowed: bool,
    /// An end-to-end encrypted room the bridge cannot read.
    #[serde(default)]
    pub encrypted: bool,
    /// Whether it can be chosen.
    #[serde(default = "default_true")]
    pub selectable: bool,
}

/// The chats a bridge's bot can answer in.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeChatList {
    /// The connector id.
    pub network: String,
    /// The chats found.
    #[serde(default)]
    pub data: Vec<BridgeChat>,
    /// A sentence to show beside the list, when there is one.
    #[serde(default)]
    pub note: Option<String>,
    /// True for a connector the API cannot list chats for: `data` is empty
    /// and the owner types the ids, which must match the connector's
    /// [`ChatAppTargets::pattern`].
    #[serde(default)]
    pub manual_entry: bool,
}

/// What [`ControlPlane::send_bridge_test_message`](crate::ControlPlane::send_bridge_test_message)
/// posted.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[non_exhaustive]
pub struct BridgeTestResult {
    /// Whether it was sent.
    pub sent: bool,
    /// The chat it was sent to.
    pub chat_id: String,
    /// The language it was written in: the hub's, else English.
    #[serde(default = "default_language")]
    pub language: String,
}

/// A bot's picture.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BridgeAvatar {
    /// The image bytes.
    pub content: Vec<u8>,
    /// Their media type, such as `image/jpeg`.
    pub content_type: String,
}

impl fmt::Debug for BridgeAvatar {
    /// The size, not the bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BridgeAvatar")
            .field("content", &format_args!("[{} bytes]", self.content.len()))
            .field("content_type", &self.content_type)
            .finish()
    }
}

/// The credentials a new bridge's bot uses: exactly one form.
#[derive(Clone, Debug)]
pub enum BridgeCredentials {
    /// A connector's single `token` secret (Telegram, Discord, Mattermost).
    Token(SecretValue),
    /// Every secret the connector names in [`ChatAppConnector::secrets`],
    /// by key (Matrix: `access_token`; Twitch: `refresh_token` and
    /// `client_secret`).
    Secrets(BTreeMap<String, SecretValue>),
}

/// What [`ControlPlane::create_bridge`](crate::ControlPlane::create_bridge)
/// sends.
///
/// Built with [`BridgeCreate::with_token`] or [`BridgeCreate::with_secrets`],
/// so it always carries exactly one form of credentials, and
/// [`BridgeCreate::with_settings`] for a connector that has settings. Its
/// `Debug` prints every credential as `<redacted>`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BridgeCreate {
    /// The hub that will answer.
    pub hub_id: String,
    /// The connector id, such as `telegram`.
    pub connector: String,
    /// The bot's credentials.
    pub credentials: BridgeCredentials,
    /// Non-secret settings, checked against the connector's
    /// [`ChatAppConnector::config_schema`].
    pub settings: Option<Map<String, Value>>,
}

impl BridgeCreate {
    /// A bridge whose bot has one token (Telegram, Discord, Mattermost).
    pub fn with_token(
        hub_id: impl Into<String>,
        connector: impl Into<String>,
        token: impl Into<SecretValue>,
    ) -> Self {
        Self {
            hub_id: hub_id.into(),
            connector: connector.into(),
            credentials: BridgeCredentials::Token(token.into()),
            settings: None,
        }
    }

    /// A bridge whose bot has the named secrets (Matrix, Twitch).
    pub fn with_secrets<I, K, V>(
        hub_id: impl Into<String>,
        connector: impl Into<String>,
        secrets: I,
    ) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<SecretValue>,
    {
        Self {
            hub_id: hub_id.into(),
            connector: connector.into(),
            credentials: BridgeCredentials::Secrets(secret_map(secrets)),
            settings: None,
        }
    }

    /// Add non-secret settings, such as a Matrix homeserver. Each value is
    /// anything that converts to JSON: a string, a number, a bool. Calls add
    /// up: a later call keeps the settings an earlier one added, and replaces
    /// only a key it names again.
    pub fn with_settings<I, K, V>(mut self, settings: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Value>,
    {
        self.settings.get_or_insert_with(Map::new).extend(
            settings
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        self
    }
}

/// What [`ControlPlane::update_bridge`](crate::ControlPlane::update_bridge)
/// changes: only the fields that are `Some` are sent.
///
/// Pause with `paused: Some(true)` and resume with `paused: Some(false)`.
/// Its `Serialize` writes the request body, without `etag`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BridgeUpdate {
    /// The chats the bridge answers in. Setting it the first time starts the
    /// bridge. An empty list is sent as it is, and the API refuses it with
    /// 422 `bridge_allowed_required`, as every Thalovant SDK reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed: Option<Vec<BridgeAllowedChat>>,
    /// Answer only what mentions the bot (always true on Discord).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_mention: Option<bool>,
    /// Who shares a conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_scope: Option<BridgeSessionScope>,
    /// Pause (`true`) or resume (`false`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    /// Settings to change, merged into the stored ones and checked again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
    /// Only change the bridge if it is still at this version
    /// ([`Bridge::etag`]), sent as `If-Match`. A stale value fails with
    /// HTTP 412 and changes nothing. `None` sends no `If-Match`.
    #[serde(skip)]
    pub etag: Option<String>,
}

/// Which bridges [`ControlPlane::list_bridges`](crate::ControlPlane::list_bridges)
/// lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeListOptions {
    /// One hub's bridges. Without it, every bridge the caller may see.
    pub hub_id: Option<String>,
    /// Narrow that to one owner's bridges.
    pub owner_id: Option<String>,
    /// At most this many, from 1 to 500 (the API's default is 200).
    pub limit: Option<u32>,
}

/// A map of secrets from any `(key, secret)` pairs.
pub(crate) fn secret_map<I, K, V>(secrets: I) -> BTreeMap<String, SecretValue>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<SecretValue>,
{
    secrets
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
}

/// A JSON object of secrets for a request body. The only place a
/// [`SecretValue`] is read, and what it builds is sent and dropped.
pub(crate) fn exposed_secrets(secrets: &BTreeMap<String, SecretValue>) -> Value {
    Value::Object(
        secrets
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(value.expose())))
            .collect(),
    )
}

/// An operation, or `None` when it is absent, `null`, or a shape this
/// release cannot read: an unreadable operation must not make the bridge
/// that carries it unreadable.
fn readable_operation<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<OperationResource>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

fn default_true() -> bool {
    true
}

fn default_language() -> String {
    "en".to_string()
}

fn default_session_scopes() -> Vec<BridgeSessionScope> {
    vec![BridgeSessionScope::Person, BridgeSessionScope::Chat]
}
