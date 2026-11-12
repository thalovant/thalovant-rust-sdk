//! Intercom: say a short text aloud on a household hub's speakers.
//!
//! An announcement is a text of at most 280 characters, spoken by each chosen
//! speaker's own text-to-speech. It is announce-only: there is no two-way
//! audio. Intercom does not record or store speech; while an urgent alert
//! sounds, the hub only checks whether a reply is an acknowledgement. **It is
//! not a certified safety system** and does not replace smoke or CO alarms,
//! medical alert devices or emergency services.
//!
//! The API does not store the text: it keeps a keyed hash of it (HMAC-SHA256,
//! server-side secret) and its length, the targets, each speaker's result and
//! the actor. (The hub's own message bus and each speaker's text-to-speech are
//! outside the platform's control.) The calls live on
//! [`ControlPlane`](crate::ControlPlane), under `/v1/hubs/{hub_id}/intercom`:
//!
//! | Call | Route | Scope |
//! | :-- | :-- | :-- |
//! | [`get_intercom_settings`](crate::ControlPlane::get_intercom_settings) | `GET .../settings` | `intercom:read` |
//! | [`set_intercom_settings`](crate::ControlPlane::set_intercom_settings) | `PUT .../settings` | `hubs:write` |
//! | [`list_intercom_speakers`](crate::ControlPlane::list_intercom_speakers) | `GET .../speakers` | `intercom:read` |
//! | [`update_intercom_speaker`](crate::ControlPlane::update_intercom_speaker) | `PATCH .../speakers/{client_id}` | `hubs:write` |
//! | [`announce`](crate::ControlPlane::announce) | `POST .../announce` | `intercom:send` (and `intercom:read` too with `wait`; `intercom:urgent` too for an urgent one) |
//! | [`cancel_announcement`](crate::ControlPlane::cancel_announcement) | `POST .../announcements/{id}/cancel` | `intercom:send` |
//! | [`get_announcement`](crate::ControlPlane::get_announcement) | `GET .../announcements/{id}` | `intercom:read` |
//! | [`list_announcements`](crate::ControlPlane::list_announcements) | `GET .../announcements` | `intercom:read` |
//!
//! **Scopes.** [`INTERCOM_READ_SCOPE`], [`INTERCOM_SEND_SCOPE`], and
//! [`INTERCOM_URGENT_SCOPE`], which an urgent announcement (`priority` urgent,
//! and so every `repeat`) needs **in addition to** `intercom:send`: nothing
//! implies it and it implies nothing. A token without it is refused before
//! anything is spent
//! ([`ApiRefusal::UrgentScopeRequired`](crate::ApiRefusal::UrgentScopeRequired)).
//! Changing settings or a speaker needs `hubs:write` (owner, manager or
//! administrator of the hub, acting as themselves).
//!
//! The names say `announce` and `intercom`, never a bare `intercom` for the
//! HTTP calls: the HiveMind mesh already has an `intercom` frame, which is a
//! different thing.
//!
//! Refusals are [`ThalovantError`](crate::ThalovantError)s, each named by an
//! [`ApiRefusal`](crate::ApiRefusal) kind (the codes are constants in
//! [`problem_code`]):
//! [`ApiRefusal::IntercomDisabled`](crate::ApiRefusal::IntercomDisabled) (the
//! hub's switch is off; `IntercomUnavailable` for a kid or public hub and
//! `IntercomDisabledPlatform` for the platform's own switch are kinds of it,
//! see [`ApiRefusal::is_intercom_disabled`](crate::ApiRefusal::is_intercom_disabled)),
//! [`ApiRefusal::HubUnreachable`](crate::ApiRefusal::HubUnreachable) (whether
//! the text was spoken is not guaranteed: read the announcement named by
//! [`announcement_id`](crate::ThalovantError::announcement_id) before sending
//! again, and use a new key),
//! [`ApiRefusal::NotCancellable`](crate::ApiRefusal::NotCancellable) (409),
//! [`is_rate_limited`](crate::ThalovantError::is_rate_limited) and
//! [`ApiRefusal::Plan`](crate::ApiRefusal::Plan) (HTTP 402, or 403
//! `plan_limit`), plus `UrgentScopeRequired`, `UrgentBusy`,
//! `SkillNotInstalled`, `NotASpeaker`, `ReservedName`, `HubLocked`,
//! `ImpersonationNotAllowed`, `TooManyTargets`, `IntercomUnavailable`,
//! `IntercomDisabledPlatform`, `IntercomNotConfigured` and `ManagedClient`.
//! With `wait`, a poll that fails after the announcement was
//! accepted is [`ThalovantError::AnnouncementWaitFailed`](crate::ThalovantError::AnnouncementWaitFailed),
//! which carries the accepted announcement.
//!
//! **Idempotency.** Set `idempotency_key` to a fresh unique key (a UUID) for
//! each announcement you mean to send. The same key replays the stored answer
//! (`Idempotency-Replayed: true`) and **never sends again**, whatever its
//! status; a replayed `failed` announcement stays `failed`. To send the same
//! text again on purpose, use a new key.
//!
//! **What an answer means.** `delivered` means the hub handed the speech to
//! that speaker's connection. There is no playback receipt: do not say "heard"
//! or "played". Only `acked` (an urgent announcement somebody answered) is a
//! person's answer, and `failed` is not proof that nothing was spoken. The
//! platform switch ships off: until the operator turns it on every write and
//! announcement is refused with `intercom_disabled_platform`.
//!
//! **One text rule, the API's own,** checked before anything is sent and never
//! echoed: NFC-normalised and trimmed, 1 to 280 characters (counted after
//! that), no `<` or `>`, no control, bidirectional or other invisible format
//! character (the zero-width joiner and non-joiner are allowed), no private-use
//! character and no line or paragraph separator. (A Rust `&str` cannot hold a
//! lone surrogate.)
//!
//! Every status is an open enum: a value this release does not know is kept
//! in `Other`.

use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};

use crate::chat_apps::open_enum;
use crate::errors::ThalovantError;

// `open_enum!` names the standard `Result`, so the crate's alias is spelled out.
type ApiResult<T> = std::result::Result<T, ThalovantError>;

/// The longest announcement, in characters.
pub const MAX_ANNOUNCEMENT_TEXT: usize = 280;
/// The most speakers one announcement may name.
pub const MAX_ANNOUNCEMENT_SPEAKERS: usize = 50;
/// The most repeats of an urgent announcement.
pub const MAX_REPEAT_TIMES: u32 = 10;
/// The shortest repeat interval, in seconds.
pub const MIN_REPEAT_INTERVAL_SECONDS: u32 = 5;
/// The longest repeat interval, in seconds.
pub const MAX_REPEAT_INTERVAL_SECONDS: u32 = 120;
/// The longest room name of a speaker's policy, in characters.
pub const MAX_ROOM_LENGTH: usize = 64;
/// The longest group name of a speaker's policy, in characters.
pub const MAX_GROUP_LENGTH: usize = 32;
/// The most groups of a speaker's policy.
pub const MAX_GROUPS: usize = 8;
/// The longest name in a `speakers`, `rooms` or `groups` target, in characters.
pub const MAX_TARGET_NAME_LENGTH: usize = 64;
/// The most names in one `speakers`, `rooms` or `groups` target list.
pub const MAX_TARGET_NAMES: usize = 50;
/// The longest `Idempotency-Key` the API takes, in characters.
pub const MAX_IDEMPOTENCY_KEY_LENGTH: usize = 128;
/// The longest IANA timezone name the API takes, in characters.
pub const MAX_TIMEZONE_LENGTH: usize = 64;

/// The scope of the intercom reads (settings, speakers, announcements).
pub const INTERCOM_READ_SCOPE: &str = "intercom:read";
/// The scope of `announce` and `cancel_announcement`.
pub const INTERCOM_SEND_SCOPE: &str = "intercom:send";
/// The scope an urgent announcement needs IN ADDITION to `intercom:send`
/// (every `repeat` is urgent). Not implied by anything; implies nothing.
pub const INTERCOM_URGENT_SCOPE: &str = "intercom:urgent";

/// What a delivery's `detail` can be: a fixed code, never free text. Treat a
/// value that is not in this list as opaque: it is kept as sent.
pub const DELIVERY_DETAIL_CODES: &[&str] = &[
    "disabled",
    "sender_not_allowed",
    "rate_limited",
    "invalid_text",
    "text_too_long",
    "invalid_priority",
    "too_many_targets",
    "no_targets",
    "internal_error",
    "duplicate_id",
    "busy",
    "not_running",
    "invalid_id",
    "policy_changed",
    "dnd_unreadable",
    "cancelled",
    "intercom_disabled",
    "intercom_disabled_platform",
    "no_result",
    "unknown_status",
    "no_matching_speakers",
    "hub_no_response",
    "hub_unreachable",
    "hub_not_ready",
    "hub_transport_error",
    "skill_not_installed",
    "stopped",
];

/// The `code` of every refusal the intercom routes (and the connection routes
/// beside them) add. The API's answers are `application/problem+json`: read
/// `code` first. Each has an [`ApiRefusal`](crate::ApiRefusal) kind; a code not
/// listed here arrives as a plain error with
/// [`api_code`](crate::ThalovantError::api_code) set.
pub mod problem_code {
    /// 403: `priority` urgent (and so every `repeat`) needs `intercom:urgent` as well as `intercom:send`.
    pub const URGENT_SCOPE_REQUIRED: &str = "urgent_scope_required";
    /// 429: the hub (or the platform) already has its urgent announcements repeating; `scope` says which.
    pub const URGENT_BUSY: &str = "intercom_urgent_busy";
    /// 409: an urgent announcement on a hub whose runtime has no active Intercom skill.
    pub const SKILL_NOT_INSTALLED: &str = "skill_not_installed";
    /// 422: `receive: true`, or a `speakers` target, on a connection that cannot play speech.
    pub const NOT_A_SPEAKER: &str = "not_a_speaker";
    /// 422: a connection named like the platform's own Intercom client.
    pub const RESERVED_NAME: &str = "reserved_name";
    /// 403: the hub is locked; nothing was changed.
    pub const HUB_LOCKED: &str = "hub_locked";
    /// 403: an impersonated session cannot send announcements.
    pub const IMPERSONATION_NOT_ALLOWED: &str = "impersonation_not_allowed";
    /// 422: `all`, a room or a group resolved to more than 50 speakers.
    pub const TOO_MANY_TARGETS: &str = "too_many_targets";
    /// 422: intercom is never available on this hub (a kid hub or a public hub); `reason` says which.
    pub const UNAVAILABLE: &str = "intercom_unavailable";
    /// 503: intercom is switched off for the whole platform; nothing was sent.
    pub const DISABLED_PLATFORM: &str = "intercom_disabled_platform";
    /// 503: the operator has not configured intercom yet; nothing was sent.
    pub const NOT_CONFIGURED: &str = "intercom_not_configured";
    /// 403: the platform-managed Intercom client cannot be managed or given credentials.
    pub const MANAGED_CLIENT: &str = "managed_client";
    /// 409: only an announcement that is still `sending` can be cancelled; `announcement_status` is its current status.
    pub const NOT_CANCELLABLE: &str = "not_cancellable";
    /// 502: the hub did not take the announcement; `announcement_id` names the failed record.
    pub const HUB_UNREACHABLE: &str = "hub_unreachable";
    /// 403 or 422 (any status): the hub's switch is off.
    pub const DISABLED: &str = "intercom_disabled";
}

open_enum! {
    /// How urgent an announcement is.
    pub enum IntercomPriority {
        /// Waits up to ten seconds for the hub's answer.
        Normal = "normal",
        /// May repeat until somebody acknowledges.
        Urgent = "urgent",
    }
}

open_enum! {
    /// Where an announcement is.
    pub enum AnnouncementStatus {
        /// Every eligible speaker was spoken to.
        Completed = "completed",
        /// Some were, some were not.
        Partial = "partial",
        /// Nobody was spoken to.
        Failed = "failed",
        /// An urgent announcement is still repeating: poll it.
        Sending = "sending",
        /// Stopped with
        /// [`cancel_announcement`](crate::ControlPlane::cancel_announcement).
        Cancelled = "cancelled",
    }
}

open_enum! {
    /// What happened for one speaker.
    pub enum DeliveryStatus {
        /// Spoken.
        Delivered = "delivered",
        /// Spoken, and acknowledged.
        Acked = "acked",
        /// The speaker had no live connection.
        Offline = "offline",
        /// Inside the speaker's quiet hours (normal announcements only).
        SkippedDnd = "skipped_dnd",
        /// The speaker's `receive` is off.
        SkippedNotReceiving = "skipped_not_receiving",
        /// Something failed.
        Failed = "failed",
    }
}

open_enum! {
    /// Why intercom cannot run on a hub.
    pub enum IntercomUnavailableReason {
        /// The platform's own switch is off. It wins over every other reason.
        Platform = "platform",
        /// The plan has no intercom.
        Plan = "plan",
        /// A kid hub.
        KidHub = "kid_hub",
        /// A public or showcase hub.
        PublicHub = "public_hub",
    }
}

/// A hub's intercom switch (`GET .../intercom/settings`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomSettings {
    /// Whether intercom is on for the hub.
    #[serde(default)]
    pub enabled: bool,
    /// Whether the plan has intercom.
    #[serde(default)]
    pub plan_allowed: bool,
    /// Why it cannot run (`platform`: the platform's own switch is off, which
    /// wins over the others; `plan`; `kid_hub`; `public_hub`); `None` when it
    /// can. `enabled` is true only when the hub's switch is on AND this is
    /// `None`.
    #[serde(default)]
    pub unavailable_reason: Option<IntercomUnavailableReason>,
}

/// Quiet hours: `HH:MM` start and end in an IANA timezone.
///
/// Quiet hours that are switched on (`IntercomDnd::enabled`) need a real
/// `timezone` and a `start` that differs from the `end`: the API refuses a
/// window it would have to guess the zone of (422), so the SDK refuses it
/// first. A `timezone` that is given is checked even when do-not-disturb is
/// off.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomQuietHours {
    /// `HH:MM`.
    pub start: String,
    /// `HH:MM`.
    pub end: String,
    /// An IANA timezone name such as `Europe/Paris` (at most 64 characters).
    /// `None` only while do-not-disturb is off.
    #[serde(default)]
    pub timezone: Option<String>,
}

/// Do not disturb: a normal announcement is skipped inside quiet hours; an
/// urgent one is not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomDnd {
    /// Whether do-not-disturb is on.
    #[serde(default)]
    pub enabled: bool,
    /// The quiet hours, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<IntercomQuietHours>,
}

/// A speaker's intercom policy. `receive` is false until somebody turns it on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomPolicy {
    /// A speaker is spoken to only when this is true.
    #[serde(default)]
    pub receive: bool,
    /// A free-text room, such as `kitchen`.
    #[serde(default)]
    pub room: Option<String>,
    /// Groups, such as `downstairs`.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Do not disturb.
    #[serde(default)]
    pub dnd: IntercomDnd,
}

/// A connection of the hub that may be spoken to.
///
/// `can_play` is the EFFECTIVE answer to "can this connection play speech at
/// all": true by default for a `voice_satellite` and `web_chat`, false for
/// `home_assistant`, `developer` and `embedded` unless its owner declared
/// `spec.intercom.can_play: true` on the **connection** (never on the
/// speaker). One that cannot is listed (grey it out) but cannot be given
/// `receive: true` (422 `not_a_speaker`), is never part of `all`, a room or a
/// group, and naming it in an announcement is refused.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomSpeaker {
    /// The client's id.
    pub client_id: String,
    /// The client's slug.
    #[serde(default)]
    pub slug: Option<String>,
    /// The client's name.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether the client is online now.
    #[serde(default)]
    pub online: bool,
    /// When the client was last seen, as the API wrote it.
    #[serde(default)]
    pub last_seen_at: Option<String>,
    /// The speaker's policy.
    #[serde(default)]
    pub intercom: IntercomPolicy,
    /// The connection kind (`voice_satellite`, `web_chat`, `developer`,
    /// `embedded`, `home_assistant`; anything else reads `voice_satellite`,
    /// the API's own rule). Kept as sent: a value this release does not know
    /// is not an error.
    #[serde(default = "default_kind", deserialize_with = "kind_or_default")]
    pub kind: String,
    /// Whether the connection can play speech at all (see the type). Only an
    /// explicit `false` reads as false.
    #[serde(default = "yes", deserialize_with = "can_play_or_default")]
    pub can_play: bool,
    /// Set on the answer to
    /// [`update_intercom_speaker`](crate::ControlPlane::update_intercom_speaker):
    /// `Some(false)` while the hub may hold an out-of-date consent map (the
    /// push failed and is retried in the background), so show "Changes may
    /// take a moment to reach the home". `None` when the API did not say (a
    /// list item: the list carries its own, see
    /// [`IntercomSpeakerList::policy_synced`]).
    #[serde(default, deserialize_with = "boolean_or_none")]
    pub policy_synced: Option<bool>,
}

fn default_kind() -> String {
    "voice_satellite".into()
}

fn yes() -> bool {
    true
}

/// A non-empty string kind, else the API's own default.
fn kind_or_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::String(kind) if !kind.is_empty() => kind,
        _ => default_kind(),
    })
}

/// The API's own default is true: only an explicit `false` says "cannot play".
fn can_play_or_default<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?.as_bool() != Some(false))
}

/// A string, or `None` for anything else.
fn string_or_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::String(text) => Some(text),
        _ => None,
    })
}

/// Every connection of the hub, as speakers (the platform's own Intercom
/// client is not listed).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomSpeakerList {
    /// The speakers.
    pub items: Vec<IntercomSpeaker>,
    /// `Some(false)` while the hub may hold an out-of-date consent map (a push
    /// failed and is retried in the background): a speaker whose `receive` was
    /// just turned off can still be spoken to by a routine until it clears.
    /// Show a quiet "Changes may take a moment to reach the home". `Some(true)`
    /// when nothing is pending, and while intercom is off; `None` when the API
    /// did not say.
    #[serde(default, deserialize_with = "boolean_or_none")]
    pub policy_synced: Option<bool>,
    /// The same value as [`IntercomSettings::unavailable_reason`]: a list read
    /// while intercom is off platform-wide says `platform`.
    #[serde(default)]
    pub unavailable_reason: Option<IntercomUnavailableReason>,
}

/// What happened for one speaker.
///
/// `delivered` means the hub handed the speech to that speaker's connection;
/// there is no playback receipt, so it does not prove the sound came out or
/// that anybody heard it. Only `acked` (an urgent announcement somebody
/// answered) is a person's answer, and `failed` is not proof that nothing was
/// spoken.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomDelivery {
    /// The speaker's client id.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The speaker's slug.
    #[serde(default)]
    pub slug: Option<String>,
    /// The speaker's name.
    #[serde(default)]
    pub name: Option<String>,
    /// What happened.
    pub status: DeliveryStatus,
    /// When, as the API wrote it.
    #[serde(default)]
    pub at: Option<String>,
    /// One of [`DELIVERY_DETAIL_CODES`] or `None`: a fixed code, never free
    /// text; a value not in the list is kept as sent.
    #[serde(default, deserialize_with = "string_or_none")]
    pub detail: Option<String>,
}

/// An announcement and its deliveries. The text is never part of it: the
/// platform keeps none. `acked_by` is the display name of a client of the hub,
/// or the literal `"unknown"`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Announcement {
    /// The announcement's id.
    pub id: String,
    /// Where it is.
    pub status: AnnouncementStatus,
    /// `normal` when the API says nothing.
    #[serde(default = "normal_priority")]
    pub priority: IntercomPriority,
    /// When it was created, as the API wrote it.
    #[serde(default)]
    pub created_at: Option<String>,
    /// One delivery per speaker.
    #[serde(default)]
    pub deliveries: Vec<IntercomDelivery>,
    /// The speaker that acknowledged an urgent announcement.
    #[serde(default)]
    pub acked_by: Option<String>,
    /// Only on the answer to
    /// [`cancel_announcement`](crate::ControlPlane::cancel_announcement):
    /// `Some(false)` means the hub could not confirm the cancel (the record is
    /// `cancelled` anyway), so speech already queued on the hub may still
    /// play. `None` on every other answer, and when the API sent something
    /// that is not a boolean.
    #[serde(
        default,
        deserialize_with = "boolean_or_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub hub_confirmed: Option<bool>,
    /// `true` when this answer is the STORED announcement for an
    /// `idempotency_key` the hub had seen before (the response header
    /// `Idempotency-Replayed: true`): nothing new was sent, whatever its
    /// status. `false` on a first answer and on every read; it is set only
    /// from the header, never from the body. Do not clear the text a person
    /// typed, or say "sent", on a replayed answer without saying nothing new
    /// was sent. Kept after `wait`.
    #[serde(default, skip_deserializing)]
    pub replayed: bool,
}

/// A real boolean, or `None` for anything else: a malformed `hub_confirmed`
/// must not fail the whole answer.
fn boolean_or_none<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Value::deserialize(deserializer)?.as_bool())
}

fn normal_priority() -> IntercomPriority {
    IntercomPriority::Normal
}

impl Announcement {
    /// Whether an urgent announcement is still repeating.
    pub fn is_sending(&self) -> bool {
        self.status == AnnouncementStatus::Sending
    }
}

/// A page of announcements, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AnnouncementList {
    /// The announcements (read from `items`, or `data`).
    #[serde(alias = "data")]
    pub items: Vec<Announcement>,
    /// Continue from here; `None` on the last page (read from `next_cursor`,
    /// or `cursor`).
    #[serde(default, alias = "cursor")]
    pub next_cursor: Option<String>,
}

/// Whom to speak to: exactly one of the four.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntercomTargets {
    /// Every speaker that receives.
    All,
    /// Speakers by client id or slug (at most 50).
    Speakers(Vec<String>),
    /// Speakers by room.
    Rooms(Vec<String>),
    /// Speakers by group.
    Groups(Vec<String>),
}

/// How an urgent announcement repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct IntercomRepeat {
    /// 1 to 10.
    pub times: u32,
    /// 5 to 120 seconds.
    pub interval_seconds: u32,
}

/// One announcement, for [`ControlPlane::announce`](crate::ControlPlane::announce).
///
/// `Debug` redacts the text: it prints `<N chars>`, never the characters, so
/// `{:?}`, `{:#?}`, a `tracing` `#[instrument]` or a logged error chain does
/// not carry what is spoken aloud in a home.
#[derive(Clone, PartialEq, Eq)]
pub struct AnnounceOptions {
    /// 1 to 280 characters, NFC-normalised and trimmed, with no `<` or `>`, no
    /// control, bidirectional or other invisible format characters (see
    /// [`announcement_text`]). It is spoken aloud in a home.
    pub text: String,
    /// Whom to speak to.
    pub targets: IntercomTargets,
    /// `normal` when `None`.
    pub priority: Option<IntercomPriority>,
    /// For urgent announcements only. Urgent (and so every `repeat`) needs the
    /// scope `intercom:urgent` as well as `intercom:send`.
    pub repeat: Option<IntercomRepeat>,
    /// A language tag such as `en` or `en-US`; `None` leaves it to the hub.
    pub lang: Option<String>,
    /// Sent as `Idempotency-Key` when set (at most 128 printable ASCII
    /// characters); never generated. Use a fresh unique key (a UUID) per
    /// announcement you mean to send: the same key replays the stored result
    /// for 24 hours and never sends again (the answer's `replayed` is `true`),
    /// a replayed `failed` announcement stays `failed`, and to send again on
    /// purpose you need a new key.
    pub idempotency_key: Option<String>,
    /// Poll a `sending` announcement until it completes. The polls need
    /// `intercom:read` as well as `intercom:send`.
    pub wait: bool,
    /// The longest wait: `times * interval + 15 s` when `None`; a time the
    /// clock cannot reach is refused before the POST. The deadline bounds each
    /// poll, not only the sleep between polls. On timeout the latest answer is
    /// returned, still `sending`.
    pub wait_timeout: Option<Duration>,
    /// Time between polls: 2 s when `None`; zero is refused before the POST.
    pub poll_interval: Option<Duration>,
}

impl fmt::Debug for AnnounceOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Redacted(usize);
        impl fmt::Debug for Redacted {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "<{} chars>", self.0)
            }
        }
        f.debug_struct("AnnounceOptions")
            .field("text", &Redacted(self.text.chars().count()))
            .field("targets", &self.targets)
            .field("priority", &self.priority)
            .field("repeat", &self.repeat)
            .field("lang", &self.lang)
            .field("idempotency_key", &self.idempotency_key)
            .field("wait", &self.wait)
            .field("wait_timeout", &self.wait_timeout)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

impl AnnounceOptions {
    /// A normal announcement of `text` to `targets`.
    pub fn new(text: impl Into<String>, targets: IntercomTargets) -> Self {
        Self {
            text: text.into(),
            targets,
            priority: None,
            repeat: None,
            lang: None,
            idempotency_key: None,
            wait: false,
            wait_timeout: None,
            poll_interval: None,
        }
    }
}

/// A change to a speaker's policy: only what is `Some` is sent, and at least
/// one field is required. These four fields are all there is (send only what
/// changed): the API refuses any other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntercomSpeakerUpdate {
    /// Turn receiving on or off.
    pub receive: Option<bool>,
    /// `Some(Some(room))` sets the room, `Some(None)` clears it.
    pub room: Option<Option<String>>,
    /// Replaces the groups (an empty list clears them).
    pub groups: Option<Vec<String>>,
    /// Replaces do-not-disturb.
    pub dnd: Option<IntercomDnd>,
}

/// Which page of announcements
/// [`ControlPlane::list_announcements`](crate::ControlPlane::list_announcements) reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnnouncementListOptions {
    /// At most this many (a positive number); `None` leaves it to the API.
    pub limit: Option<u32>,
    /// Continue from a page's `next_cursor`.
    pub cursor: Option<String>,
}

fn refuse(message: impl Into<String>) -> ThalovantError {
    ThalovantError::Api(message.into())
}

fn non_blank(name: &str, value: &str) -> ApiResult<()> {
    if value.trim().is_empty() {
        return Err(refuse(format!("{name} is required")));
    }
    Ok(())
}

/// An id (hub, client or announcement) fit to be one path segment, or an error
/// before any request. Refused: an empty or blank one, `.` and `..` (dot
/// segments, which a URL resolver collapses into another path), and any id
/// holding `/`, `?`, `#`, a backslash or a control character.
pub(crate) fn valid_id(name: &str, value: &str) -> ApiResult<()> {
    non_blank(name, value)?;
    if value == "."
        || value == ".."
        || value
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '\\') || c.is_control())
    {
        return Err(refuse(format!(
            "{name} is not a valid id: it is a dot segment or holds '/', '?', '#', a backslash or a control character"
        )));
    }
    Ok(())
}
/// The Unicode categories an announcement may not hold: controls (Cc), format
/// characters (Cf: bidirectional overrides and isolates, zero-width spaces,
/// tag characters), private use (Co), and line and paragraph separators (Zl,
/// Zp). Lone surrogates (Cs) cannot occur in a Rust `&str`.
static REFUSED_CATEGORIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\p{Cc}\p{Cf}\p{Co}\p{Zl}\p{Zp}]").expect("a valid class"));

/// A language tag such as `en` or `en-US`, the API's own pattern.
static LANGUAGE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})?$").expect("a valid pattern"));

/// The shape of an IANA timezone name: `UTC`, `Europe/Paris`,
/// `America/Argentina/Buenos_Aires`, `Etc/GMT+5`.
static TIMEZONE_SHAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_+-]+(/[A-Za-z0-9_+-]+)*$").expect("a valid pattern"));

/// The text as it is sent: NFC-normalised, trimmed, 1 to 280 characters, or
/// [`ThalovantError::Api`] before any request.
///
/// The API's own rule, applied before anything is sent (one rule everywhere):
/// no `<` or `>` (a speaker's text-to-speech may read them as markup), no
/// control characters (a newline or a tab included), no bidirectional
/// overrides or isolates or other invisible format characters (the zero-width
/// joiner and non-joiner, which scripts and emoji need, are allowed), no
/// private-use characters and no line or paragraph separators. The text is
/// refused rather than rewritten, the length is counted after normalising, and
/// an error never echoes the text.
pub fn announcement_text(text: &str) -> ApiResult<String> {
    const NFC: icu_normalizer::ComposingNormalizerBorrowed<'static> =
        icu_normalizer::ComposingNormalizerBorrowed::new_nfc();
    let normalised = NFC.normalize(text);
    let trimmed = normalised.trim();
    if trimmed.is_empty() {
        return Err(refuse("an announcement's text must not be empty"));
    }
    if trimmed.chars().count() > MAX_ANNOUNCEMENT_TEXT {
        return Err(refuse(format!(
            "an announcement's text is at most {MAX_ANNOUNCEMENT_TEXT} characters"
        )));
    }
    if trimmed.contains(['<', '>']) {
        return Err(refuse(
            "an announcement's text must not contain markup characters (< or >)",
        ));
    }
    if REFUSED_CATEGORIES
        .find_iter(trimmed)
        .any(|found| !matches!(found.as_str(), "\u{200c}" | "\u{200d}"))
    {
        return Err(refuse(
            "an announcement's text must not contain control, bidirectional, format or private-use characters",
        ));
    }
    Ok(trimmed.to_string())
}

/// An `Idempotency-Key` fit to send: 1 to 128 printable ASCII characters, or
/// [`ThalovantError::Api`] before any request. Use a fresh unique key (a UUID)
/// for each announcement you mean to send: the same key replays the stored
/// answer and never sends again.
pub(crate) fn idempotency_key(key: &str) -> ApiResult<()> {
    if key.trim().is_empty() {
        return Err(refuse("idempotency_key must not be blank"));
    }
    if key.chars().count() > MAX_IDEMPOTENCY_KEY_LENGTH {
        return Err(refuse(format!(
            "idempotency_key is at most {MAX_IDEMPOTENCY_KEY_LENGTH} characters"
        )));
    }
    if !key.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err(refuse("idempotency_key must be printable ASCII"));
    }
    Ok(())
}

/// An IANA timezone name the API can accept: at most 64 characters, not
/// padded, and shaped like one (`Area/Location`).
///
/// This checks the shape, not the tz database: no timezone database is linked
/// into this crate (it would add crates and go stale against the API's own),
/// so a well-formed name that does not exist is the API's 422.
fn valid_timezone(name: &str) -> ApiResult<()> {
    if name.is_empty()
        || name.chars().count() > MAX_TIMEZONE_LENGTH
        || name != name.trim()
        || !TIMEZONE_SHAPE.is_match(name)
    {
        return Err(refuse("quiet_hours timezone must be an IANA timezone name"));
    }
    Ok(())
}

fn targets_body(targets: &IntercomTargets) -> ApiResult<Value> {
    let (kind, items) = match targets {
        IntercomTargets::All => return Ok(json!({ "all": true })),
        IntercomTargets::Speakers(items) => ("speakers", items),
        IntercomTargets::Rooms(items) => ("rooms", items),
        IntercomTargets::Groups(items) => ("groups", items),
    };
    if items.is_empty() {
        return Err(refuse(format!("targets: {kind} must not be empty")));
    }
    if items.iter().any(|item| item.trim().is_empty()) {
        return Err(refuse(format!("a target in {kind} must not be blank")));
    }
    if items
        .iter()
        .any(|item| item.chars().count() > MAX_TARGET_NAME_LENGTH)
    {
        return Err(refuse(format!(
            "a target in {kind} is at most {MAX_TARGET_NAME_LENGTH} characters"
        )));
    }
    // 50 names of any kind; "at most 50 speakers" is the API's rule for
    // explicit speakers.
    if items.len() > MAX_TARGET_NAMES {
        return Err(refuse(format!(
            "an announcement names at most {MAX_TARGET_NAMES} {kind}"
        )));
    }
    Ok(json!({ kind: items }))
}

/// The `POST .../intercom/announce` body. It holds the text: never log it.
pub(crate) fn announce_body(options: &AnnounceOptions) -> ApiResult<Value> {
    let priority = options.priority.clone().unwrap_or(IntercomPriority::Normal);
    if !matches!(
        priority,
        IntercomPriority::Normal | IntercomPriority::Urgent
    ) {
        return Err(refuse("priority must be \"normal\" or \"urgent\""));
    }
    let mut body = Map::new();
    body.insert(
        "text".into(),
        Value::from(announcement_text(&options.text)?),
    );
    body.insert("targets".into(), targets_body(&options.targets)?);
    body.insert("priority".into(), Value::from(priority.as_str()));
    if let Some(repeat) = options.repeat {
        if priority != IntercomPriority::Urgent {
            return Err(refuse("repeat is only for urgent announcements"));
        }
        if !(1..=MAX_REPEAT_TIMES).contains(&repeat.times) {
            return Err(refuse(format!(
                "repeat times must be an integer from 1 to {MAX_REPEAT_TIMES}"
            )));
        }
        if !(MIN_REPEAT_INTERVAL_SECONDS..=MAX_REPEAT_INTERVAL_SECONDS)
            .contains(&repeat.interval_seconds)
        {
            return Err(refuse(format!(
                "repeat interval_seconds must be an integer from {MIN_REPEAT_INTERVAL_SECONDS} to {MAX_REPEAT_INTERVAL_SECONDS}"
            )));
        }
        body.insert("repeat".into(), serde_json::to_value(repeat)?);
    }
    if let Some(lang) = &options.lang {
        if !LANGUAGE_TAG.is_match(lang) {
            return Err(refuse(
                "lang must be a language tag such as \"en-US\" or \"fr\"",
            ));
        }
        body.insert("lang".into(), Value::from(lang.as_str()));
    }
    Ok(Value::Object(body))
}

fn valid_clock(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[3..].iter().all(u8::is_ascii_digit)
        && value[..2].parse::<u8>().is_ok_and(|h| h < 24)
        && value[3..].parse::<u8>().is_ok_and(|m| m < 60)
}

/// The `PATCH .../intercom/speakers/{id}` body: only what is `Some`.
pub(crate) fn speaker_update_body(update: &IntercomSpeakerUpdate) -> ApiResult<Value> {
    let mut body = Map::new();
    if let Some(receive) = update.receive {
        body.insert("receive".into(), Value::from(receive));
    }
    if let Some(room) = &update.room {
        match room {
            Some(room) => {
                non_blank("room", room)?;
                if room.chars().count() > MAX_ROOM_LENGTH {
                    return Err(refuse(format!(
                        "room is at most {MAX_ROOM_LENGTH} characters"
                    )));
                }
                body.insert("room".into(), Value::from(room.as_str()));
            }
            None => {
                body.insert("room".into(), Value::Null);
            }
        }
    }
    if let Some(groups) = &update.groups {
        if groups.len() > MAX_GROUPS {
            return Err(refuse(format!(
                "groups is a list of at most {MAX_GROUPS} names"
            )));
        }
        for group in groups {
            non_blank("a group", group)?;
            if group.chars().count() > MAX_GROUP_LENGTH {
                return Err(refuse(format!(
                    "a group name is at most {MAX_GROUP_LENGTH} characters"
                )));
            }
        }
        body.insert("groups".into(), json!(groups));
    }
    if let Some(dnd) = &update.dnd {
        if let Some(hours) = &dnd.quiet_hours {
            if !valid_clock(&hours.start) || !valid_clock(&hours.end) {
                return Err(refuse("quiet_hours start and end must be HH:MM"));
            }
            if let Some(zone) = &hours.timezone {
                valid_timezone(zone)?;
            }
        }
        if dnd.enabled {
            // The API refuses an enabled window with no zone (UTC would put a
            // Toronto bedroom's night in the afternoon) or with start == end.
            match &dnd.quiet_hours {
                Some(hours) if hours.timezone.is_some() => {
                    if hours.start == hours.end {
                        return Err(refuse(
                            "dnd quiet_hours start and end must differ when dnd is enabled",
                        ));
                    }
                }
                _ => {
                    return Err(refuse(
                        "dnd quiet_hours with a timezone is required when dnd is enabled",
                    ));
                }
            }
        }
        let mut value = json!({ "enabled": dnd.enabled });
        if let Some(hours) = &dnd.quiet_hours {
            value["quiet_hours"] = json!({
                "start": hours.start, "end": hours.end, "timezone": hours.timezone,
            });
        }
        body.insert("dnd".into(), value);
    }
    if body.is_empty() {
        return Err(refuse("pass at least one of receive, room, groups or dnd"));
    }
    Ok(Value::Object(body))
}
