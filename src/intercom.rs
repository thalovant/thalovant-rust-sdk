//! Intercom: say a short text aloud on a household hub's speakers.
//!
//! An announcement is a text of at most 280 characters, spoken by each chosen
//! speaker's own text-to-speech. It is announce-only: there is no two-way
//! audio and nothing listens. **It is not a certified safety system** and does
//! not replace smoke or CO alarms, medical alert devices or emergency
//! services.
//!
//! The platform never stores the text: the API keeps its SHA-256 and length,
//! the targets, each speaker's result and the actor. The calls live on
//! [`ControlPlane`](crate::ControlPlane), under `/v1/hubs/{hub_id}/intercom`:
//!
//! | Call | Route | Scope |
//! | :-- | :-- | :-- |
//! | [`get_intercom_settings`](crate::ControlPlane::get_intercom_settings) | `GET .../settings` | `intercom:read` |
//! | [`set_intercom_settings`](crate::ControlPlane::set_intercom_settings) | `PUT .../settings` | `hubs:write` |
//! | [`list_intercom_speakers`](crate::ControlPlane::list_intercom_speakers) | `GET .../speakers` | `intercom:read` |
//! | [`update_intercom_speaker`](crate::ControlPlane::update_intercom_speaker) | `PATCH .../speakers/{client_id}` | `hubs:write` |
//! | [`announce`](crate::ControlPlane::announce) | `POST .../announce` | `intercom:send` |
//! | [`get_announcement`](crate::ControlPlane::get_announcement) | `GET .../announcements/{id}` | `intercom:read` |
//! | [`list_announcements`](crate::ControlPlane::list_announcements) | `GET .../announcements` | `intercom:read` |
//!
//! The names say `announce` and `intercom`, never a bare `intercom` for the
//! HTTP calls: the HiveMind mesh already has an `intercom` frame, which is a
//! different thing.
//!
//! Refusals are [`ThalovantError`](crate::ThalovantError)s:
//! [`ApiRefusal::IntercomDisabled`](crate::ApiRefusal::IntercomDisabled) (the
//! hub's switch is off, or it is a kid or public hub),
//! [`ApiRefusal::HubUnreachable`](crate::ApiRefusal::HubUnreachable) (nothing
//! was spoken), [`is_rate_limited`](crate::ThalovantError::is_rate_limited)
//! and [`ApiRefusal::Plan`](crate::ApiRefusal::Plan).
//!
//! Every status is an open enum: a value this release does not know is kept
//! in `Other`.

use std::fmt;
use std::time::Duration;

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
    /// Why intercom cannot be enabled for a hub.
    pub enum IntercomUnavailableReason {
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
    /// Why it cannot be enabled; `None` when it can.
    #[serde(default)]
    pub unavailable_reason: Option<IntercomUnavailableReason>,
}

/// Quiet hours: `HH:MM` start and end in an IANA timezone (UTC when `None`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomQuietHours {
    /// `HH:MM`.
    pub start: String,
    /// `HH:MM`.
    pub end: String,
    /// An IANA timezone name; `None` for UTC.
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

/// A client of the hub that may be spoken to.
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
}

/// Every client of the hub, as speakers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IntercomSpeakerList {
    /// The speakers.
    pub items: Vec<IntercomSpeaker>,
}

/// What happened for one speaker.
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
}

/// An announcement and its deliveries. The text is never part of it: the
/// platform keeps none.
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnounceOptions {
    /// 1 to 280 characters, trimmed, with no control characters. It is spoken
    /// aloud in a home.
    pub text: String,
    /// Whom to speak to.
    pub targets: IntercomTargets,
    /// `normal` when `None`.
    pub priority: Option<IntercomPriority>,
    /// For urgent announcements only.
    pub repeat: Option<IntercomRepeat>,
    /// A language tag such as `en-US`; `None` leaves it to the hub.
    pub lang: Option<String>,
    /// Sent as `Idempotency-Key` when set; never generated.
    pub idempotency_key: Option<String>,
    /// Poll a `sending` announcement until it completes.
    pub wait: bool,
    /// The longest wait: `times * interval + 15 s` when `None`. On timeout
    /// the latest answer is returned, still `sending`.
    pub wait_timeout: Option<Duration>,
    /// Time between polls: 2 s when `None`.
    pub poll_interval: Option<Duration>,
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
/// one field is required.
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

/// The text as it is sent: trimmed, 1 to 280 characters, no control
/// characters.
pub fn announcement_text(text: &str) -> ApiResult<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(refuse("an announcement's text must not be empty"));
    }
    if trimmed.chars().count() > MAX_ANNOUNCEMENT_TEXT {
        return Err(refuse(format!(
            "an announcement's text is at most {MAX_ANNOUNCEMENT_TEXT} characters"
        )));
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
    {
        return Err(refuse(
            "an announcement's text must not contain control characters",
        ));
    }
    Ok(trimmed.to_string())
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
    if kind == "speakers" && items.len() > MAX_ANNOUNCEMENT_SPEAKERS {
        return Err(refuse(format!(
            "an announcement names at most {MAX_ANNOUNCEMENT_SPEAKERS} speakers"
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
        non_blank("lang", lang)?;
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
