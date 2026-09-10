//! What a hub can be asked: the intent inventory, over the client's own session.
//!
//! The hub runtime keeps an intent manifest (OVOS-INTENT-4 section 10): every
//! intent a skill registered, per language, and on request the registration
//! itself, which for a template intent carries the sentences from the skill's
//! locale files, slots and all -- `what is the weather in {location}`. This
//! module asks that manifest and shapes the answer, so a satellite, an
//! installer or an agent shows a person what they can say without a
//! control-plane token.
//!
//! Two queries, correlated by `context.request_id` like every other request:
//!
//! - `ovos.intent.list` `{"lang": <tag>}` -> `ovos.intent.list.response`
//!   `{"ok", "intents": [{skill_id, intent_name, lang, method, enabled,
//!   session_id}]}`. `method` is `template` (sample sentences) or `keyword`
//!   (keyword sets). A runtime may attach each entry's `definition` when asked
//!   with `include_definitions`; when it does not, the client describes each
//!   intent individually. `{"ok": false, "error"}` is a failed query, not an
//!   empty hub: it becomes [`ThalovantError::Runtime`] carrying the hub's
//!   wording.
//! - `ovos.intent.describe` `{"skill_id", "intent_name", "lang"}` ->
//!   `ovos.intent.describe.response` `{"ok", "definitions": [{method,
//!   definition}]}` or `{"ok": false, "error"}`. Here `ok: false` is a real
//!   answer -- the hub does not know that registration -- and leaves the
//!   intent without sentences rather than failing.
//!
//! A hub whose connection may not publish a type answers `hive.policy.denied`
//! naming it; that becomes [`ThalovantError::PolicyDenied`] at once rather
//! than a timeout. The engines' own manifests (`intent.service.adapt.manifest.get`
//! and `intent.service.padatious.manifest.get`, names only, no language) are
//! the fallback for a hub allowed for those alone.
//!
//! The public surface is [`Client::intents`](crate::Client::intents),
//! [`Client::list_intents`](crate::Client::list_intents) and
//! [`Client::describe_intent`](crate::Client::describe_intent); the types they
//! return live here.

use crate::{
    constants::{
        EVENT_ADAPT_MANIFEST, EVENT_ADAPT_MANIFEST_GET, EVENT_FALLBACK_LIST,
        EVENT_FALLBACK_LIST_RESPONSE, EVENT_INTENT_DESCRIBE, EVENT_INTENT_DESCRIBE_RESPONSE,
        EVENT_INTENT_LIST, EVENT_INTENT_LIST_RESPONSE, EVENT_PADATIOUS_MANIFEST,
        EVENT_PADATIOUS_MANIFEST_GET, EVENT_POLICY_DENIED,
    },
    errors::{Result, ThalovantError},
    events::{
        context_with_correlation, event_matches_context, new_request_id, Context, Data, Event,
    },
};
use serde::{Serialize, Serializer};
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    time::Duration,
};
use tokio::{
    sync::broadcast,
    time::{timeout, timeout_at, Instant},
};

/// How long each intent query waits for its reply unless an option says otherwise.
pub const DEFAULT_INTENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How many describes may be in flight at once.
///
/// A hub with 69 intents in two languages is 138 requests and, with every
/// reply delivered twice, 276 inbound events, while the transports' bus
/// channel holds 64: past its capacity the broadcast receiver lags, replies
/// are dropped, and the inventory comes back missing sentences. A batch of 32
/// is 64 events at the observed duplication, so a window fits what the channel
/// holds even if nothing is read until the last request is out. Batching also
/// spares the hub a burst it never asked for.
pub const DESCRIBE_BATCH: usize = 32;

/// The language asked about when the caller names none.
pub(crate) const DEFAULT_LANG: &str = "en-us";

/// Options for [`Client::intents`](crate::Client::intents).
///
/// The default describes every template registration for its sentences and
/// falls back to the engines' manifests when `ovos.intent.list` is denied or silent.
#[derive(Clone, Debug)]
pub struct IntentInventoryOptions {
    /// Deadline for each listing and one shared deadline across all describe
    /// windows, including their sends and replies. Default 5s. Initial connection
    /// and other inventory phases have separate budgets.
    pub timeout: Option<Duration>,
    /// Ask for the sentences behind every template intent. Off, the inventory
    /// carries the intents and their engines but no phrases.
    pub describe: bool,
    /// When `ovos.intent.list` is denied or silent, read the engines' own manifests
    /// instead and return names only, marked `engine-manifests`. Off, the
    /// refusal or silence is returned as [`ThalovantError::PolicyDenied`] or [`ThalovantError::Timeout`].
    pub fallback: bool,
}

impl Default for IntentInventoryOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            describe: true,
            fallback: true,
        }
    }
}

/// Options for [`Client::list_intents`](crate::Client::list_intents).
#[derive(Clone, Debug, Default)]
pub struct IntentListOptions {
    /// Deadline for the reply. Default 5s.
    pub timeout: Option<Duration>,
    /// Ask the runtime to attach each row's `definition`. A runtime that honours
    /// it saves a describe per intent; one that does not answers rows as usual.
    pub include_definitions: bool,
}

/// Options for [`Client::describe_intent`](crate::Client::describe_intent).
#[derive(Clone, Debug, Default)]
pub struct IntentDescribeOptions {
    /// Deadline for the reply. Default 5s.
    pub timeout: Option<Duration>,
}

/// Where an inventory was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentInventorySource {
    /// `intent-manifest`: the runtime's intent manifest, sentences per language.
    IntentManifest,
    /// `engine-manifests`: the engines' own manifests, names only; the
    /// inventory's legacy `denied` names a query unavailable through denial or silence.
    EngineManifests,
}

impl IntentInventorySource {
    /// The wire spelling shared with the other SDKs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IntentManifest => "intent-manifest",
            Self::EngineManifests => "engine-manifests",
        }
    }
}

impl fmt::Display for IntentInventorySource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for IntentInventorySource {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// `fr-fr` and `fr_FR` are the same language tag.
pub fn same_language(a: &str, b: &str) -> bool {
    fold_language(a) == fold_language(b)
}

fn fold_language(tag: &str) -> String {
    tag.trim().to_lowercase().replace('_', "-")
}

/// The engine behind a manifest `method`: `template` is padatious, `keyword` is adapt.
fn engine_for_method(method: &str) -> &str {
    match method {
        "template" => "padatious",
        "keyword" => "adapt",
        "" => "unknown",
        other => other,
    }
}

/// One row of the hub's intent manifest, from `ovos.intent.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IntentRegistration {
    pub skill_id: String,
    pub intent_name: String,
    /// The tag as the runtime stores it (`fr-FR` for a `fr-fr` listing).
    pub lang: String,
    /// `template` or `keyword`; see [`IntentRegistration::engine`].
    pub method: String,
    pub enabled: bool,
    pub session_id: String,
    /// The registration itself, when the runtime attached it to the listing.
    pub definition: Option<Map<String, Value>>,
}

impl IntentRegistration {
    /// `padatious` for a template intent, `adapt` for a keyword one.
    pub fn engine(&self) -> &str {
        engine_for_method(&self.method)
    }

    /// Read one listing row; `None` when it names no skill or intent.
    pub fn from_value(raw: &Value) -> Option<Self> {
        let row = raw.as_object()?;
        let skill_id = string_field(row, "skill_id");
        let intent_name = string_field(row, "intent_name");
        if skill_id.is_empty() || intent_name.is_empty() {
            return None;
        }
        Some(Self {
            skill_id,
            intent_name,
            lang: string_field(row, "lang"),
            method: string_field(row, "method"),
            enabled: !matches!(row.get("enabled"), Some(Value::Bool(false))),
            session_id: Some(string_field(row, "session_id"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "default".to_string()),
            definition: row.get("definition").and_then(Value::as_object).cloned(),
        })
    }
}

/// A registration as the skill made it, from `ovos.intent.describe`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IntentDefinition {
    pub skill_id: String,
    pub intent_name: String,
    pub lang: String,
    /// `template` or `keyword`; see [`IntentDefinition::engine`].
    pub method: String,
    /// The sentences as the skill's locale files wrote them, slots in braces.
    /// Empty for a keyword intent.
    pub samples: Vec<String>,
    /// The whole definition, for what the SDK does not model (`blacklist`,
    /// `slot_blacklist`, keyword sets).
    pub raw: Map<String, Value>,
}

impl IntentDefinition {
    /// `padatious` for a template intent, `adapt` for a keyword one.
    pub fn engine(&self) -> &str {
        engine_for_method(&self.method)
    }

    /// Read one `{method, definition}` item; `None` when it names no skill or intent.
    pub fn from_value(item: &Value) -> Option<Self> {
        let item = item.as_object()?;
        let definition = item.get("definition").and_then(Value::as_object)?;
        let skill_id = string_field(definition, "skill_id");
        let intent_name = string_field(definition, "intent_name");
        if skill_id.is_empty() || intent_name.is_empty() {
            return None;
        }
        let method = Some(string_field(item, "method"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| string_field(definition, "method"));
        Some(Self {
            skill_id,
            intent_name,
            lang: string_field(definition, "lang"),
            method,
            samples: samples_of(definition),
            raw: definition.clone(),
        })
    }
}

/// One thing a hub can be asked, with the sentences that ask it, per language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubIntent {
    pub skill_id: String,
    pub name: String,
    /// `padatious` or `adapt`.
    pub engine: String,
    pub enabled: bool,
    /// The sentences per language tag as it was asked for; a language the hub
    /// listed the intent in but gave no sentences for maps to an empty list.
    pub phrases: BTreeMap<String, Vec<String>>,
}

impl HubIntent {
    /// `skill_id:name`, the intent's name in the engines' manifests.
    pub fn id(&self) -> String {
        format!("{}:{}", self.skill_id, self.name)
    }

    /// The languages this intent was listed in, sorted.
    pub fn languages(&self) -> Vec<&str> {
        self.phrases.keys().map(String::as_str).collect()
    }

    /// The sentences for one language, matched case-insensitively with `_`/`-` folded.
    pub fn phrases_for(&self, lang: &str) -> &[String] {
        self.phrases
            .iter()
            .find(|(candidate, _)| same_language(candidate, lang))
            .map(|(_, sentences)| sentences.as_slice())
            .unwrap_or(&[])
    }

    /// A few sentences worth showing: whole ones before ones with a slot,
    /// shorter ones first. `lang` `None` takes the first language; `limit` 0
    /// returns every sentence as the skill wrote them.
    pub fn examples(&self, lang: Option<&str>, limit: usize) -> Vec<String> {
        let pool: Vec<String> = match lang {
            Some(lang) => self.phrases_for(lang).to_vec(),
            None => self.phrases.values().next().cloned().unwrap_or_default(),
        };
        if limit == 0 {
            return pool;
        }
        let mut chosen = pool;
        chosen.sort_by_key(|text| (text.contains('{'), text.len()));
        chosen.truncate(limit);
        chosen
    }

    /// The JSON the other SDKs and the CLI print for an intent.
    pub fn as_value(&self) -> Value {
        json!({
            "id": self.id(),
            "skill_id": self.skill_id,
            "name": self.name,
            "engine": self.engine,
            "enabled": self.enabled,
            "phrases": self.phrases,
        })
    }
}

impl Serialize for HubIntent {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.as_value().serialize(serializer)
    }
}

/// One skill's intents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubSkillIntents {
    pub skill_id: String,
    /// Sorted by name.
    pub intents: Vec<HubIntent>,
}

impl HubSkillIntents {
    /// Every language one of this skill's intents was listed in, sorted.
    pub fn languages(&self) -> Vec<&str> {
        self.intents
            .iter()
            .flat_map(|intent| intent.phrases.keys())
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// The JSON the other SDKs and the CLI print for a skill.
    pub fn as_value(&self) -> Value {
        json!({
            "skill_id": self.skill_id,
            "languages": self.languages(),
            "intents": self.intents.iter().map(HubIntent::as_value).collect::<Vec<_>>(),
        })
    }
}

impl Serialize for HubSkillIntents {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.as_value().serialize(serializer)
    }
}

/// Everything a hub can be asked, grouped by skill.
///
/// `source` says how it was read: [`IntentInventorySource::IntentManifest`]
/// carries sentences per language; [`IntentInventorySource::EngineManifests`]
/// is the names-only fallback. The legacy `denied` field names queries unavailable
/// through denial or silence; it does not prove an ACL refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubIntentInventory {
    /// The languages asked for, one per language in the order given, spelt
    /// as first given (`en-US` after `en-us` is the same language, asked once).
    pub languages: Vec<String>,
    /// Sorted by skill id.
    pub skills: Vec<HubSkillIntents>,
    pub source: IntentInventorySource,
    /// Legacy name: queries unavailable through denial or silence, not proof of an ACL denial.
    pub denied: Vec<String>,
}

impl HubIntentInventory {
    /// Every intent across skills.
    pub fn intents(&self) -> impl Iterator<Item = &HubIntent> + '_ {
        self.skills.iter().flat_map(|skill| skill.intents.iter())
    }

    /// True when at least one intent carries at least one sentence: false for
    /// the names-only fallback, and for a manifest whose describes all came
    /// back empty.
    pub fn has_phrases(&self) -> bool {
        self.intents().any(|intent| {
            intent
                .phrases
                .values()
                .any(|sentences| !sentences.is_empty())
        })
    }

    /// The JSON the other SDKs and the CLI print for an inventory.
    pub fn as_value(&self) -> Value {
        json!({
            "languages": self.languages,
            "source": self.source.as_str(),
            "denied": self.denied,
            "skills": self.skills.iter().map(HubSkillIntents::as_value).collect::<Vec<_>>(),
        })
    }
}

impl Serialize for HubIntentInventory {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.as_value().serialize(serializer)
    }
}

/// A skill that may answer after the ordinary intent engines do not match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HubFallback {
    pub skill_id: String,
    pub priority: i64,
}

/// An intent inventory with separately discovered fallback capabilities.
///
/// This additive wrapper keeps existing `HubIntentInventory` struct literals
/// source compatible. Unknown fallback support must not be treated as absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubIntentCapabilities {
    pub inventory: HubIntentInventory,
    pub fallbacks: Vec<HubFallback>,
    pub fallbacks_known: bool,
}

impl HubIntentCapabilities {
    /// Conservative language availability, not a guarantee of an answer.
    pub fn may_answer(&self, lang: &str) -> bool {
        !self.fallbacks_known
            || !self.fallbacks.is_empty()
            || self
                .inventory
                .intents()
                .any(|intent| intent.enabled && !intent.phrases_for(lang).is_empty())
    }

    pub fn as_value(&self) -> Value {
        let mut value = self.inventory.as_value();
        value["fallbacks"] = json!(self.fallbacks);
        value["fallbacks_known"] = json!(self.fallbacks_known);
        value
    }
}

impl Serialize for HubIntentCapabilities {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.as_value().serialize(serializer)
    }
}

/// Optional fallback discovery has its own small budget after the inventory.
pub const FALLBACK_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

pub(crate) async fn inventory_with_capabilities<L, I, S>(
    link: &L,
    languages: I,
    opts: &IntentInventoryOptions,
) -> Result<HubIntentCapabilities>
where
    L: HubLink,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let inventory = inventory(link, languages, opts).await?;
    let deadline = opts
        .timeout
        .unwrap_or(DEFAULT_INTENT_TIMEOUT)
        .min(FALLBACK_PROBE_TIMEOUT);
    let fallbacks = list_fallbacks(link, deadline).await?;
    Ok(HubIntentCapabilities {
        inventory,
        fallbacks_known: fallbacks.is_some(),
        fallbacks: fallbacks.unwrap_or_default(),
    })
}

pub(crate) async fn list_fallbacks<L: HubLink>(
    link: &L,
    deadline: Duration,
) -> Result<Option<Vec<HubFallback>>> {
    let event = match request_reply(
        link,
        EVENT_FALLBACK_LIST,
        EVENT_FALLBACK_LIST_RESPONSE,
        Data::new(),
        DEFAULT_LANG,
        deadline,
    )
    .await
    {
        Ok(event) => event,
        Err(ThalovantError::PolicyDenied { .. } | ThalovantError::Timeout(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if event.data.get("ok") == Some(&Value::Bool(false)) {
        return Ok(None);
    }
    let Some(rows) = event.data.get("fallbacks").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut fallbacks: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            let row = row.as_object()?;
            let skill_id = string_field(row, "skill_id");
            if skill_id.is_empty() {
                return None;
            }
            let priority = match row.get("priority") {
                Some(Value::Bool(value)) => i64::from(*value),
                Some(Value::Number(value)) => match value.as_i64() {
                    Some(value) => value,
                    None => {
                        let value = value.as_f64()?;
                        // Skip ranks that cannot be represented instead of
                        // silently saturating and changing handler order.
                        if !value.is_finite()
                            || !(-9223372036854775808.0..9223372036854775808.0).contains(&value)
                        {
                            return None;
                        }
                        value as i64
                    }
                },
                _ => 0,
            };
            Some(HubFallback { skill_id, priority })
        })
        .collect();
    fallbacks.sort_by(|a, b| (a.priority, &a.skill_id).cmp(&(b.priority, &b.skill_id)));
    Ok(Some(fallbacks))
}

fn string_field(map: &Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn samples_of(definition: &Map<String, Value>) -> Vec<String> {
    definition
        .get("samples")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

// ----------------------------------------------------------------------------
// the wire
// ----------------------------------------------------------------------------

/// The two things the inventory needs from a session: bus events as they
/// arrive, and a way to publish. `Client` is the real one; the tests script a hub.
pub(crate) trait HubLink {
    /// Subscribe before publishing, so no reply is missed.
    fn subscribe(&self) -> broadcast::Receiver<Event>;
    /// Publish one bus message over the session.
    async fn emit_bus(&self, event_type: &str, data: Data, context: Context) -> Result<()>;
    /// The identity's site id, stamped into each query's context like `ask` does.
    fn site_id(&self) -> Option<String>;
}

/// The languages an inventory call asks about: trimmed, one entry per
/// language whatever its spelling (`en-us`, `en-US` and `en_us` are one, the
/// first spelling seen is kept, in the order given), and `en-us` when the
/// caller named none.
pub(crate) fn chosen_languages<I, S>(languages: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut chosen: Vec<String> = Vec::new();
    for language in languages {
        let tag = language.as_ref().trim();
        if !tag.is_empty() && !chosen.iter().any(|seen| same_language(seen, tag)) {
            chosen.push(tag.to_string());
        }
    }
    if chosen.is_empty() {
        chosen.push(DEFAULT_LANG.to_string());
    }
    chosen
}

/// [`ThalovantError::PolicyDenied`] from the hub's `hive.policy.denied`.
fn policy_denied(event: &Event) -> ThalovantError {
    let allowed = event
        .data
        .get("data")
        .and_then(Value::as_object)
        .and_then(|inner| inner.get("allowed"))
        .and_then(Value::as_array)
        .map(|items| {
            // Only strings: a number or a null in the hub's list is not a
            // message type, and rendering one would put "3" or "null" in
            // front of an operator reading which types to allow.
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    ThalovantError::PolicyDenied {
        denied_type: string_field(&event.data, "denied_type"),
        code: string_field(&event.data, "code"),
        reason: string_field(&event.data, "reason"),
        allowed,
    }
}

fn denied_type_of(event: &Event) -> Option<&str> {
    if event.name != EVENT_POLICY_DENIED {
        return None;
    }
    event.data.get("denied_type").and_then(Value::as_str)
}

fn query_context<L: HubLink>(link: &L, lang: &str, request_id: &str) -> Context {
    let mut base = Context::new();
    base.insert("lang".to_string(), Value::String(lang.to_string()));
    context_with_correlation(
        Some(&base),
        None,
        link.site_id().as_deref(),
        Some(lang),
        Some(request_id),
    )
}

/// Read bus events until `accept` is satisfied, a `hive.policy.denied` names
/// `query_type`, or `deadline` passes.
///
/// A reply may be delivered more than once; `accept` sees each copy and
/// decides. A denial of the query in flight fails at once: the hub will not
/// answer, so waiting for the deadline would only hide the cause.
async fn await_reply<T>(
    receiver: &mut broadcast::Receiver<Event>,
    deadline: Duration,
    query_type: &str,
    correlates: impl Fn(&Event) -> bool,
    mut accept: impl FnMut(&Event) -> Option<T>,
) -> Result<T> {
    timeout(deadline, async {
        loop {
            let event = match receiver.recv().await {
                Ok(event) => event,
                // Replies we could not keep up with are gone; the ones still
                // queued may be the ones we want. `DESCRIBE_BATCH` is what
                // keeps a describe window inside the channel's capacity.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(ThalovantError::Connection(
                        "hub session closed while waiting for a reply".to_string(),
                    ))
                }
            };
            if denied_type_of(&event) == Some(query_type) && correlates(&event) {
                return Err(policy_denied(&event));
            }
            if let Some(found) = accept(&event) {
                return Ok(found);
            }
        }
    })
    .await
    .map_err(|_| {
        ThalovantError::Timeout(format!(
            "hub did not answer {query_type} within {}ms",
            deadline.as_millis()
        ))
    })?
}

/// Send one bus query and return its reply, matched by request id.
///
/// A reply may arrive more than once; the first one wins and repeats are
/// dropped. A `hive.policy.denied` naming the query fails at once.
async fn request_reply<L: HubLink>(
    link: &L,
    query_type: &str,
    reply_type: &str,
    data: Data,
    lang: &str,
    deadline: Duration,
) -> Result<Event> {
    let request_id = new_request_id();
    let context = query_context(link, lang, &request_id);
    let mut receiver = link.subscribe();
    timeout(deadline, async {
        link.emit_bus(query_type, data, context.clone()).await?;
        await_reply(
            &mut receiver,
            deadline,
            query_type,
            |event| event_matches_context(event, Some(&context)),
            |event| {
                (event.name == reply_type && event_matches_context(event, Some(&context)))
                    .then(|| event.clone())
            },
        )
        .await
    })
    .await
    .map_err(|_| ThalovantError::Timeout(format!("hub request {query_type} timed out")))?
}

fn registrations_of(event: &Event) -> Vec<IntentRegistration> {
    event
        .data
        .get("intents")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(IntentRegistration::from_value)
                .collect()
        })
        .unwrap_or_default()
}

fn definitions_of(event: &Event) -> Vec<IntentDefinition> {
    if matches!(event.data.get("ok"), Some(Value::Bool(false))) {
        return Vec::new();
    }
    event
        .data
        .get("definitions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(IntentDefinition::from_value)
                .collect()
        })
        .unwrap_or_default()
}

/// The hub's wording for a listing that failed, or a sentence of our own.
fn listing_error(event: &Event) -> String {
    let detail = match event.data.get("error") {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    };
    let detail = if detail.is_empty() {
        "the hub refused the listing"
    } else {
        detail.as_str()
    };
    format!("{EVENT_INTENT_LIST} failed: {detail}")
}

/// The hub's intent manifest for one language.
pub(crate) async fn list_intents<L: HubLink>(
    link: &L,
    lang: &str,
    opts: &IntentListOptions,
) -> Result<Vec<IntentRegistration>> {
    let mut data = Data::new();
    data.insert("lang".to_string(), Value::String(lang.to_string()));
    if opts.include_definitions {
        data.insert("include_definitions".to_string(), Value::Bool(true));
    }
    let event = request_reply(
        link,
        EVENT_INTENT_LIST,
        EVENT_INTENT_LIST_RESPONSE,
        data,
        lang,
        opts.timeout.unwrap_or(DEFAULT_INTENT_TIMEOUT),
    )
    .await?;
    if matches!(event.data.get("ok"), Some(Value::Bool(false))) {
        // A refused listing is not an empty hub. A describe answering
        // `ok: false` is a real answer -- the hub does not know that
        // registration -- but a listing that failed has told us nothing, and
        // reading the missing `intents` key as no intents would show a person
        // a device that can do nothing.
        return Err(ThalovantError::Runtime(listing_error(&event)));
    }
    Ok(registrations_of(&event))
}

fn describe_payload(skill_id: &str, intent_name: &str, lang: &str) -> Data {
    let mut data = Data::new();
    data.insert("skill_id".to_string(), Value::String(skill_id.to_string()));
    data.insert(
        "intent_name".to_string(),
        Value::String(intent_name.to_string()),
    );
    data.insert("lang".to_string(), Value::String(lang.to_string()));
    data
}

/// Every registration behind one intent in one language, keyword ones first.
pub(crate) async fn describe_intent<L: HubLink>(
    link: &L,
    skill_id: &str,
    intent_name: &str,
    lang: &str,
    deadline: Duration,
) -> Result<Vec<IntentDefinition>> {
    let event = request_reply(
        link,
        EVENT_INTENT_DESCRIBE,
        EVENT_INTENT_DESCRIBE_RESPONSE,
        describe_payload(skill_id, intent_name, lang),
        lang,
        deadline,
    )
    .await?;
    Ok(definitions_of(&event))
}

/// `(skill_id, intent_name, lang)`: one registration to describe.
type Wanted = (String, String, String);

/// Describe many registrations, at most `batch` of them in flight.
///
/// One subscription per batch, one request id per registration, replies
/// matched by that id, repeats dropped. A hub that does not echo the id is
/// matched by the definition's own `skill_id`/`intent_name`/`lang`. The
/// deadline covers all windows, including sends and reply collection. Once it
/// expires, no later window is sent. Partial answers from completed windows
/// survive; intents not described in time are absent from the result. Only a
/// call with unanswered registrations and no usable definition is a timeout.
/// Explicit empty answers to every request remain a successful empty result. `batch` 0 sends them all
/// at once.
pub(crate) async fn describe_many<L: HubLink>(
    link: &L,
    wanted: &[Wanted],
    deadline: Duration,
    batch: usize,
) -> Result<HashMap<Wanted, Vec<IntentDefinition>>> {
    let end = Instant::now()
        .checked_add(deadline)
        .ok_or_else(|| ThalovantError::Runtime("intent describe timeout is too large".into()))?;
    let mut unique: Vec<Wanted> = Vec::new();
    for key in wanted {
        if !unique.contains(key) {
            unique.push(key.clone());
        }
    }
    if batch > 0 && unique.len() > batch {
        let mut found: HashMap<Wanted, Vec<IntentDefinition>> = HashMap::new();
        for window in unique.chunks(batch) {
            match describe_one_batch(link, window, end).await {
                Ok(described) => found.extend(described),
                // A partial answer is an answer across windows as within one.
                // Windows are contiguous slices of the work, so a skill that
                // stops answering can own a whole one: without this, an
                // unresponsive skill with more than `batch` intents would fail
                // the whole inventory while the same skill with fewer intents
                // only loses its sentences. A hub silent from the start still
                // fails at the first window, having found nothing.
                Err(ThalovantError::Timeout(message)) => {
                    if !found.values().any(|definitions| !definitions.is_empty()) {
                        return Err(ThalovantError::Timeout(message));
                    }
                }
                Err(error) => return Err(error),
            }
            if Instant::now() >= end {
                break;
            }
        }
        return Ok(found);
    }
    describe_one_batch(link, &unique, end).await
}

/// One describe window: every request out, then the replies, then done.
async fn describe_one_batch<L: HubLink>(
    link: &L,
    unique: &[Wanted],
    end: Instant,
) -> Result<HashMap<Wanted, Vec<IntentDefinition>>> {
    let mut found: HashMap<Wanted, Vec<IntentDefinition>> = HashMap::new();
    if unique.is_empty() {
        return Ok(found);
    }

    let mut receiver = link.subscribe();
    let mut by_request: HashMap<String, Wanted> = HashMap::new();
    for key in unique {
        if Instant::now() >= end {
            return Err(ThalovantError::Timeout(
                "intent describe send timed out".into(),
            ));
        }
        let (skill_id, intent_name, lang) = key;
        let request_id = new_request_id();
        by_request.insert(request_id.clone(), key.clone());
        timeout_at(
            end,
            link.emit_bus(
                EVENT_INTENT_DESCRIBE,
                describe_payload(skill_id, intent_name, lang),
                query_context(link, lang, &request_id),
            ),
        )
        .await
        .map_err(|_| ThalovantError::Timeout("intent describe send timed out".into()))??;
    }

    let waited = await_reply(
        &mut receiver,
        end.saturating_duration_since(Instant::now()),
        EVENT_INTENT_DESCRIBE,
        |event| {
            event
                .request_id()
                .map(|id| by_request.contains_key(&id))
                .unwrap_or(true)
        },
        |event| {
            if event.name != EVENT_INTENT_DESCRIBE_RESPONSE {
                return None;
            }
            let definitions = definitions_of(event);
            let key = match event.request_id() {
                Some(request_id) => by_request.get(&request_id).cloned(),
                None => {
                    // Content fallback is only for a response with no request ID.
                    // A foreign ID remains foreign even if its definition matches.
                    let first = definitions.first()?;
                    unique
                        .iter()
                        .find(|(skill_id, intent_name, lang)| {
                            *skill_id == first.skill_id
                                && *intent_name == first.intent_name
                                && same_language(lang, &first.lang)
                        })
                        .cloned()
                }
            }?;
            if found.contains_key(&key) {
                return None;
            }
            found.insert(key, definitions);
            (found.len() == unique.len()).then_some(())
        },
    )
    .await;
    match waited {
        Ok(()) => Ok(found),
        Err(ThalovantError::Timeout(message)) => {
            if !found.values().any(|definitions| !definitions.is_empty()) {
                Err(ThalovantError::Timeout(message))
            } else {
                Ok(found)
            }
        }
        Err(error) => Err(error),
    }
}

/// The engines' own manifests: `[("adapt", names), ("padatious", names)]`.
///
/// Names only, and the same names whatever the language asked, because an
/// intent's name is the same in every language. The fallback for a hub
/// allowed for these queries but not the intent manifest.
pub(crate) async fn intent_names<L: HubLink>(
    link: &L,
    lang: &str,
    deadline: Duration,
) -> Result<Vec<(&'static str, Vec<String>)>> {
    let mut names = Vec::new();
    for (engine, query_type, reply_type) in [
        ("adapt", EVENT_ADAPT_MANIFEST_GET, EVENT_ADAPT_MANIFEST),
        (
            "padatious",
            EVENT_PADATIOUS_MANIFEST_GET,
            EVENT_PADATIOUS_MANIFEST,
        ),
    ] {
        let mut data = Data::new();
        data.insert("lang".to_string(), Value::String(lang.to_string()));
        let event = request_reply(link, query_type, reply_type, data, lang, deadline).await?;
        let listed: Vec<String> = event
            .data
            .get("intents")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        names.push((engine, listed));
    }
    Ok(names)
}

fn inventory_from_names(
    names: Vec<(&'static str, Vec<String>)>,
    languages: Vec<String>,
    denied: String,
) -> HubIntentInventory {
    let mut by_skill: BTreeMap<String, BTreeMap<String, HubIntent>> = BTreeMap::new();
    for (engine, entries) in names {
        for raw in entries {
            let (skill_id, intent_name) = match raw.split_once(':') {
                Some((skill_id, intent_name)) if !intent_name.is_empty() => {
                    (skill_id.to_string(), intent_name.to_string())
                }
                _ => (String::new(), raw.clone()),
            };
            // First engine to name it wins, as on the manifest path.
            by_skill
                .entry(skill_id.clone())
                .or_default()
                .entry(intent_name.clone())
                .or_insert_with(|| HubIntent {
                    skill_id,
                    name: intent_name,
                    engine: engine.to_string(),
                    enabled: true,
                    phrases: BTreeMap::new(),
                });
        }
    }
    HubIntentInventory {
        languages,
        skills: by_skill
            .into_iter()
            .map(|(skill_id, intents)| HubSkillIntents {
                skill_id,
                intents: intents.into_values().collect(),
            })
            .collect(),
        source: IntentInventorySource::EngineManifests,
        denied: vec![denied],
    }
}

/// Everything the hub can be asked, in each language, grouped by skill.
///
/// Asks the intent manifest per language and, unless the runtime attached
/// definitions to the listing, describes every registration at once. When the
/// hub refuses `ovos.intent.list` and `fallback` is on, the engines' manifests
/// give the names and the result says so.
pub(crate) async fn inventory<L, I, S>(
    link: &L,
    languages: I,
    opts: &IntentInventoryOptions,
) -> Result<HubIntentInventory>
where
    L: HubLink,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let asked = chosen_languages(languages);
    let deadline = opts.timeout.unwrap_or(DEFAULT_INTENT_TIMEOUT);
    let list_opts = IntentListOptions {
        timeout: Some(deadline),
        include_definitions: opts.describe,
    };

    let mut listed: Vec<(String, Vec<IntentRegistration>)> = Vec::new();
    for lang in &asked {
        match list_intents(link, lang, &list_opts).await {
            Ok(rows) => listed.push((lang.clone(), rows)),
            Err(error) => {
                let listing_refused = matches!(
                    &error,
                    ThalovantError::PolicyDenied { denied_type, .. }
                        if denied_type == EVENT_INTENT_LIST
                );
                if !(opts.fallback
                    && (listing_refused || matches!(error, ThalovantError::Timeout(_))))
                {
                    return Err(error);
                }
                // Names carry no language, so the engines are asked once.
                let names = intent_names(link, &asked[0], deadline).await?;
                return Ok(inventory_from_names(
                    names,
                    asked,
                    EVENT_INTENT_LIST.to_string(),
                ));
            }
        }
    }

    let wanted: Vec<Wanted> = listed
        .iter()
        .flat_map(|(lang, rows)| {
            rows.iter()
                .filter(|row| row.enabled && row.definition.is_none() && row.method == "template")
                .map(|row| (row.skill_id.clone(), row.intent_name.clone(), lang.clone()))
        })
        .collect();
    let described = if opts.describe && !wanted.is_empty() {
        describe_many(link, &wanted, deadline, DESCRIBE_BATCH).await?
    } else {
        HashMap::new()
    };

    // Per (skill, intent): the engine as first listed, enabled if any language
    // says so, and the sentences per language asked.
    let mut engines: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut enabled: BTreeMap<(String, String), bool> = BTreeMap::new();
    let mut phrases: BTreeMap<(String, String), BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for (lang, rows) in &listed {
        for row in rows {
            let key = (row.skill_id.clone(), row.intent_name.clone());
            engines
                .entry(key.clone())
                .or_insert_with(|| row.engine().to_string());
            let flag = enabled.entry(key.clone()).or_insert(false);
            *flag = *flag || row.enabled;
            let sentences = match &row.definition {
                Some(definition) => samples_of(definition),
                None => described
                    .get(&(row.skill_id.clone(), row.intent_name.clone(), lang.clone()))
                    .and_then(|definitions| {
                        definitions
                            .iter()
                            .find(|definition| !definition.samples.is_empty())
                    })
                    .map(|definition| definition.samples.clone())
                    .unwrap_or_default(),
            };
            // An intent registered under both engines has two rows for the
            // language; the keyword row carries no sentences and must not
            // erase the template row's, whichever order they arrive in.
            let per_language = phrases.entry(key).or_default();
            if !sentences.is_empty() || !per_language.contains_key(lang) {
                per_language.insert(lang.clone(), sentences);
            }
        }
    }

    let mut by_skill: BTreeMap<String, Vec<HubIntent>> = BTreeMap::new();
    for ((skill_id, intent_name), per_language) in phrases {
        let key = (skill_id.clone(), intent_name.clone());
        by_skill
            .entry(skill_id.clone())
            .or_default()
            .push(HubIntent {
                skill_id,
                name: intent_name,
                engine: engines.remove(&key).unwrap_or_default(),
                enabled: enabled.remove(&key).unwrap_or(true),
                phrases: per_language,
            });
    }
    Ok(HubIntentInventory {
        languages: asked,
        skills: by_skill
            .into_iter()
            .map(|(skill_id, mut intents)| {
                intents.sort_by(|a, b| a.name.cmp(&b.name));
                HubSkillIntents { skill_id, intents }
            })
            .collect(),
        source: IntentInventorySource::IntentManifest,
        denied: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    //! The intent inventory, against a hub that behaves like the one observed.
    //!
    //! Shapes copied from a live runtime on 2026-09-05: `ovos.intent.list.response`
    //! rows, `ovos.intent.describe.response` definitions carrying `samples` as
    //! the skill's locale files wrote them, `hive.policy.denied` for a type the
    //! connection may not publish, and every reply delivered twice.
    use super::*;
    use crate::{client::Client, identity::Identity};
    use std::sync::Mutex;

    const WEATHER: &str = "thalovant-skill-weather.thalovant";
    const SHADOW: &str = "thalovant-skill-custos-shadow.thalovant";
    const ALLOWED: [&str; 2] = ["recognizer_loop:utterance", "speak"];

    /// One intent the hub registered: `((skill_id, intent_name), samples)`.
    type Registration = ((String, String), Vec<String>);

    /// The transports all build their bus channel with this capacity, so the
    /// fake does too: a describe window that outgrows it loses replies.
    const BUS_CAPACITY: usize = 64;

    fn registration(skill_id: &str, intent_name: &str, samples: &[&str]) -> Registration {
        (
            (skill_id.to_string(), intent_name.to_string()),
            samples.iter().map(|text| text.to_string()).collect(),
        )
    }

    /// What the hub registered: per language, per intent, the sentences.
    /// Weather speaks both languages; the shadow skill only English.
    fn registrations(lang: &str) -> Vec<Registration> {
        match lang {
            "en-us" => vec![
                registration(
                    WEATHER,
                    "current.weather",
                    &[
                        "what is the weather",
                        "what is the weather in {location}",
                        "how is it outside",
                    ],
                ),
                registration(
                    SHADOW,
                    "custos.incidents",
                    &["are there incidents", "any incidents"],
                ),
            ],
            "fr-fr" => vec![registration(
                WEATHER,
                "current.weather",
                &[
                    "quel temps fait-il",
                    "quelle est la météo à {location}",
                    "quelle est la météo",
                ],
            )],
            _ => Vec::new(),
        }
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    type Emitted = (String, Data, Context);

    /// Whether the hub also registered each intent under adapt, and where
    /// that keyword row sits in the listing relative to the template row.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum KeywordRows {
        None,
        AfterTemplate,
        BeforeTemplate,
    }

    /// A hub session: answers the manifest, or refuses it, twice over.
    struct FakeHub {
        /// Per language, what the hub registered; [`registrations`] by default.
        registrations: Vec<(&'static str, Vec<Registration>)>,
        refuse: Vec<&'static str>,
        silent: Vec<&'static str>,
        /// Answer the listing `{"ok": false, "error": ...}` instead of rows.
        listing_fails_with: Option<Value>,
        fallback_response: Value,
        blocked_send: bool,
        foreign_denial: bool,
        foreign_describes_only: bool,
        definitions_in_list: bool,
        echo_request_id: bool,
        repeats: usize,
        /// A skill whose describes go unanswered.
        deaf_to_describes_for: Option<&'static str>,
        keyword_rows: KeywordRows,
        /// What the adapt engine's manifest lists.
        adapt_names: Vec<String>,
        tx: broadcast::Sender<Event>,
        emitted: Mutex<Vec<Emitted>>,
        /// One entry per subscription window, counting the describes sent in it.
        windows: Mutex<Vec<usize>>,
        /// Answer at most this many describes, then go quiet.
        describe_answer_limit: Option<usize>,
        answered_describes: Mutex<usize>,
    }

    impl Default for FakeHub {
        fn default() -> Self {
            Self {
                registrations: ["en-us", "fr-fr"]
                    .into_iter()
                    .map(|lang| (lang, registrations(lang)))
                    .collect(),
                refuse: Vec::new(),
                silent: Vec::new(),
                listing_fails_with: None,
                fallback_response: json!({"fallbacks": []}),
                blocked_send: false,
                foreign_denial: false,
                foreign_describes_only: false,
                definitions_in_list: false,
                echo_request_id: true,
                repeats: 2,
                deaf_to_describes_for: None,
                keyword_rows: KeywordRows::None,
                adapt_names: Vec::new(),
                tx: broadcast::channel(BUS_CAPACITY).0,
                emitted: Mutex::new(Vec::new()),
                windows: Mutex::new(Vec::new()),
                describe_answer_limit: None,
                answered_describes: Mutex::new(0),
            }
        }
    }

    impl FakeHub {
        fn registered(&self, lang: &str) -> Vec<Registration> {
            self.registrations
                .iter()
                .find(|(candidate, _)| *candidate == lang)
                .map(|(_, rows)| rows.clone())
                .unwrap_or_default()
        }

        fn emitted(&self, event_type: &str) -> Vec<Emitted> {
            self.emitted
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _, _)| name == event_type)
                .cloned()
                .collect()
        }

        fn deliver(&self, name: &str, data: Value, context: &Context) {
            let mut reply_context = context.clone();
            if !self.echo_request_id {
                reply_context.remove("request_id");
                reply_context.remove("thalovant_request_id");
                if let Some(Value::Object(session)) = reply_context.get_mut("session") {
                    session.remove("request_id");
                }
            }
            for _ in 0..self.repeats {
                let _ = self.tx.send(Event::new(
                    name,
                    object(data.clone()),
                    reply_context.clone(),
                    None,
                ));
            }
        }

        fn definition(lang: &str, skill_id: &str, intent_name: &str, samples: &[String]) -> Value {
            json!({
                "skill_id": skill_id,
                "intent_name": intent_name,
                "lang": lang,
                "samples": samples,
                "blacklist": [],
                "slot_blacklist": {},
            })
        }
    }

    impl HubLink for FakeHub {
        fn subscribe(&self) -> broadcast::Receiver<Event> {
            self.windows.lock().unwrap().push(0);
            self.tx.subscribe()
        }

        async fn emit_bus(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
            if self.blocked_send {
                std::future::pending::<()>().await;
            }
            self.emitted.lock().unwrap().push((
                event_type.to_string(),
                data.clone(),
                context.clone(),
            ));
            if event_type == EVENT_INTENT_DESCRIBE {
                if let Some(window) = self.windows.lock().unwrap().last_mut() {
                    *window += 1;
                }
            }
            if self.foreign_denial {
                let foreign = query_context(self, DEFAULT_LANG, "another-request");
                self.deliver(
                    EVENT_POLICY_DENIED,
                    json!({"denied_type":event_type}),
                    &foreign,
                );
            }
            if self.foreign_describes_only && event_type == EVENT_INTENT_DESCRIBE {
                let foreign = query_context(self, DEFAULT_LANG, "another-request");
                self.deliver(EVENT_INTENT_DESCRIBE_RESPONSE, json!({"ok":true,"definitions":[{"method":"template","definition":Self::definition(
                    data["lang"].as_str().unwrap(), data["skill_id"].as_str().unwrap(), data["intent_name"].as_str().unwrap(), &["foreign speech".into()])}]}), &foreign);
                return Ok(());
            }
            if self.refuse.contains(&event_type) {
                self.deliver(
                    EVENT_POLICY_DENIED,
                    json!({
                        "denied_type": event_type,
                        "code": "acl_disallowed_type",
                        "reason": format!("{event_type} not in allowed_types"),
                        "data": {"msg_type": event_type, "allowed": ALLOWED},
                    }),
                    &context,
                );
                return Ok(());
            }
            if self.silent.contains(&event_type) {
                return Ok(());
            }
            let lang = data
                .get("lang")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match event_type {
                EVENT_FALLBACK_LIST => self.deliver(
                    EVENT_FALLBACK_LIST_RESPONSE,
                    self.fallback_response.clone(),
                    &context,
                ),
                EVENT_INTENT_LIST => {
                    if let Some(error) = &self.listing_fails_with {
                        self.deliver(
                            EVENT_INTENT_LIST_RESPONSE,
                            json!({"ok": false, "error": error}),
                            &context,
                        );
                        return Ok(());
                    }
                    let attach = self.definitions_in_list
                        && data.get("include_definitions") == Some(&Value::Bool(true));
                    let rows: Vec<Value> = self
                        .registered(&lang)
                        .into_iter()
                        .flat_map(|((skill_id, intent_name), samples)| {
                            let (skill_id, intent_name) = (skill_id.as_str(), intent_name.as_str());
                            let mut template = json!({
                                "skill_id": skill_id,
                                "intent_name": intent_name,
                                // The runtime standardises what it stores.
                                "lang": if lang == "fr-fr" { "fr-FR".to_string() } else { lang.clone() },
                                "method": "template",
                                "enabled": true,
                                "session_id": "default",
                            });
                            let mut keyword = template.clone();
                            keyword["method"] = json!("keyword");
                            if attach {
                                template["definition"] =
                                    Self::definition(&lang, skill_id, intent_name, &samples);
                                keyword["definition"] = json!({
                                    "skill_id": skill_id,
                                    "intent_name": intent_name,
                                    "lang": lang,
                                    "required": [["WeatherKeyword"]],
                                });
                            }
                            match self.keyword_rows {
                                KeywordRows::None => vec![template],
                                KeywordRows::AfterTemplate => vec![template, keyword],
                                KeywordRows::BeforeTemplate => vec![keyword, template],
                            }
                        })
                        .collect();
                    self.deliver(
                        EVENT_INTENT_LIST_RESPONSE,
                        json!({"ok": true, "intents": rows}),
                        &context,
                    );
                }
                EVENT_INTENT_DESCRIBE => {
                    let skill_id = data["skill_id"].as_str().unwrap_or_default();
                    let intent_name = data["intent_name"].as_str().unwrap_or_default();
                    if self.deaf_to_describes_for == Some(skill_id) {
                        return Ok(());
                    }
                    if let Some(limit) = self.describe_answer_limit {
                        let mut answered = self.answered_describes.lock().unwrap();
                        if *answered >= limit {
                            return Ok(());
                        }
                        *answered += 1;
                    }
                    let samples = self
                        .registered(&lang)
                        .into_iter()
                        .find(|((registered_skill, registered_intent), _)| {
                            registered_skill == skill_id && registered_intent == intent_name
                        })
                        .map(|(_, samples)| samples);
                    let payload = match samples {
                        None => json!({"ok": false, "error": "unknown intent"}),
                        Some(samples) => json!({"ok": true, "definitions": [{
                            "method": "template",
                            "definition": Self::definition(&lang, skill_id, intent_name, &samples),
                        }]}),
                    };
                    self.deliver(EVENT_INTENT_DESCRIBE_RESPONSE, payload, &context);
                }
                EVENT_ADAPT_MANIFEST_GET => {
                    self.deliver(
                        EVENT_ADAPT_MANIFEST,
                        json!({"intents": self.adapt_names}),
                        &context,
                    );
                }
                EVENT_PADATIOUS_MANIFEST_GET => {
                    let names: BTreeSet<String> = self
                        .registrations
                        .iter()
                        .flat_map(|(_, rows)| rows.iter())
                        .map(|((skill_id, intent_name), _)| format!("{skill_id}:{intent_name}"))
                        .collect();
                    self.deliver(
                        EVENT_PADATIOUS_MANIFEST,
                        json!({"intents": names}),
                        &context,
                    );
                }
                _ => {}
            }
            Ok(())
        }

        fn site_id(&self) -> Option<String> {
            Some("site".to_string())
        }
    }

    fn options(timeout: Option<Duration>) -> IntentInventoryOptions {
        IntentInventoryOptions {
            timeout,
            ..Default::default()
        }
    }

    fn ids(inventory: &HubIntentInventory) -> Vec<String> {
        inventory.intents().map(HubIntent::id).collect()
    }

    #[tokio::test]
    async fn inventory_carries_the_sentences_per_language() {
        let hub = FakeHub::default();
        let inventory = inventory(&hub, ["en-us", "fr-fr"], &options(None))
            .await
            .unwrap();

        assert_eq!(inventory.source, IntentInventorySource::IntentManifest);
        assert!(inventory.denied.is_empty());
        assert_eq!(inventory.languages, ["en-us", "fr-fr"]);
        assert_eq!(
            inventory
                .skills
                .iter()
                .map(|skill| skill.skill_id.as_str())
                .collect::<Vec<_>>(),
            [SHADOW, WEATHER]
        );
        let weather = &inventory.skills[1].intents[0];
        assert_eq!(weather.id(), format!("{WEATHER}:current.weather"));
        assert_eq!(weather.engine, "padatious");
        assert!(weather.enabled);
        assert_eq!(
            weather.phrases_for("fr-FR"),
            [
                "quel temps fait-il",
                "quelle est la météo à {location}",
                "quelle est la météo",
            ]
        );
        assert_eq!(inventory.skills[1].languages(), ["en-us", "fr-fr"]);
        let shadow = &inventory.skills[0];
        assert_eq!(
            shadow.languages(),
            ["en-us"],
            "the hub said the skill has no French"
        );
        assert!(shadow.intents[0].phrases_for("fr-fr").is_empty());
        assert!(inventory.has_phrases());
    }

    #[tokio::test]
    async fn examples_prefer_whole_sentences_and_respect_the_limit() {
        let hub = FakeHub::default();
        let inventory = inventory(&hub, ["en-us"], &options(None)).await.unwrap();
        let weather = &inventory.skills[1].intents[0];

        assert_eq!(
            weather.examples(Some("en-us"), 2),
            ["how is it outside", "what is the weather"]
        );
        assert_eq!(
            weather.examples(Some("en-us"), 0),
            weather.phrases_for("en-us")
        );
        assert_eq!(weather.examples(None, 1), ["how is it outside"]);
    }

    #[tokio::test]
    async fn every_registration_is_described_at_once_and_repeats_are_dropped() {
        let hub = FakeHub {
            repeats: 3,
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us", "fr-fr"], &options(None))
            .await
            .unwrap();

        let describes: Vec<(String, String, String)> = hub
            .emitted(EVENT_INTENT_DESCRIBE)
            .iter()
            .map(|(_, data, _)| {
                (
                    data["skill_id"].as_str().unwrap().to_string(),
                    data["intent_name"].as_str().unwrap().to_string(),
                    data["lang"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(describes.len(), 3);
        assert_eq!(describes.iter().collect::<BTreeSet<_>>().len(), 3);
        assert_eq!(inventory.intents().count(), 2);
        for (name, _, context) in hub.emitted.lock().unwrap().iter() {
            assert!(
                context.get("request_id").is_some(),
                "{name} must be correlated by request id"
            );
            assert_eq!(
                context.get("lang").and_then(Value::as_str).map(str::len),
                Some(5),
                "{name} must carry the language in its context"
            );
        }
    }

    #[tokio::test]
    async fn definitions_attached_to_the_listing_skip_the_describes() {
        let hub = FakeHub {
            definitions_in_list: true,
            ..Default::default()
        };
        let inventory = inventory(&hub, ["fr-fr"], &options(None)).await.unwrap();

        assert!(hub.emitted(EVENT_INTENT_DESCRIBE).is_empty());
        assert_eq!(
            inventory.intents().next().unwrap().phrases_for("fr-fr")[0],
            "quel temps fait-il"
        );
        let (_, data, _) = &hub.emitted(EVENT_INTENT_LIST)[0];
        assert_eq!(
            Value::Object(data.clone()),
            json!({"lang": "fr-fr", "include_definitions": true})
        );
    }

    #[tokio::test]
    async fn a_refusal_is_an_error_naming_the_type_not_a_timeout() {
        let hub = FakeHub {
            refuse: vec![EVENT_INTENT_LIST],
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let error = inventory(
            &hub,
            ["en-us"],
            &IntentInventoryOptions {
                fallback: false,
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a denial must not wait for the deadline"
        );
        match &error {
            ThalovantError::PolicyDenied {
                denied_type,
                code,
                reason,
                allowed,
            } => {
                assert_eq!(denied_type, EVENT_INTENT_LIST);
                assert_eq!(code, "acl_disallowed_type");
                assert_eq!(reason, "ovos.intent.list not in allowed_types");
                assert_eq!(allowed, &ALLOWED);
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
        let message = error.to_string();
        assert!(message.contains("ovos.intent.list"), "{message}");
        assert!(message.contains("connection"), "{message}");
    }

    #[test]
    fn a_denial_without_wording_still_explains_itself() {
        let error = ThalovantError::PolicyDenied {
            denied_type: "ovos.intent.describe".to_string(),
            code: String::new(),
            reason: String::new(),
            allowed: Vec::new(),
        };
        assert_eq!(
            error.to_string(),
            "policy denied: the hub refused `ovos.intent.describe`: refused by the hub's \
             policy. Allow this connection to publish `ovos.intent.describe` in the \
             dashboard's connection settings."
        );
        let coded = ThalovantError::PolicyDenied {
            denied_type: "ovos.intent.describe".to_string(),
            code: "acl_disallowed_type".to_string(),
            reason: String::new(),
            allowed: Vec::new(),
        };
        assert!(coded.to_string().contains(": acl_disallowed_type."));
    }

    #[tokio::test]
    async fn the_fallback_lists_names_and_says_what_was_refused() {
        let hub = FakeHub {
            refuse: vec![EVENT_INTENT_LIST],
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us", "fr-fr"], &options(None))
            .await
            .unwrap();

        assert_eq!(inventory.source, IntentInventorySource::EngineManifests);
        assert_eq!(inventory.denied, [EVENT_INTENT_LIST]);
        assert!(!inventory.has_phrases());
        assert_eq!(
            ids(&inventory),
            [
                format!("{SHADOW}:custos.incidents"),
                format!("{WEATHER}:current.weather"),
            ]
        );
        assert_eq!(inventory.intents().next().unwrap().engine, "padatious");
        assert_eq!(inventory.languages, ["en-us", "fr-fr"]);
        // Names carry no language, so the engines are asked once, not per language.
        assert_eq!(hub.emitted(EVENT_PADATIOUS_MANIFEST_GET).len(), 1);
        assert_eq!(hub.emitted(EVENT_ADAPT_MANIFEST_GET).len(), 1);
    }

    #[tokio::test]
    async fn a_hub_refusing_everything_fails_even_with_the_fallback() {
        let hub = FakeHub {
            refuse: vec![EVENT_INTENT_LIST, EVENT_ADAPT_MANIFEST_GET],
            ..Default::default()
        };
        let error = inventory(&hub, ["en-us"], &options(None))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ThalovantError::PolicyDenied { ref denied_type, .. }
                if denied_type == EVENT_ADAPT_MANIFEST_GET
        ));
    }

    #[tokio::test]
    async fn a_refused_describe_fails_the_inventory() {
        let hub = FakeHub {
            refuse: vec![EVENT_INTENT_DESCRIBE],
            ..Default::default()
        };
        let error = inventory(&hub, ["en-us"], &options(None))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ThalovantError::PolicyDenied { ref denied_type, .. }
                if denied_type == EVENT_INTENT_DESCRIBE
        ));
    }

    #[tokio::test]
    async fn a_silent_hub_times_out_when_fallback_is_disabled() {
        let hub = FakeHub {
            silent: vec![EVENT_INTENT_LIST],
            ..Default::default()
        };
        let error = inventory(
            &hub,
            ["en-us"],
            &IntentInventoryOptions {
                fallback: false,
                ..options(Some(Duration::from_millis(20)))
            },
        )
        .await
        .unwrap_err();
        match error {
            ThalovantError::Timeout(message) => {
                assert!(message.contains(EVENT_INTENT_LIST), "{message}")
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_refused_listing_is_an_error_not_an_empty_hub() {
        // `ok: false` means the query failed. Reading the missing `intents`
        // key as no intents would show a person an empty hub, and the
        // engine-manifest fallback answers a denial, not a failed query.
        let hub = FakeHub {
            listing_fails_with: Some(json!("manifest unavailable")),
            ..Default::default()
        };
        let error = inventory(&hub, ["en-us"], &options(None))
            .await
            .unwrap_err();

        match &error {
            ThalovantError::Runtime(message) => {
                assert!(message.contains(EVENT_INTENT_LIST), "{message}");
                assert!(message.contains("manifest unavailable"), "{message}");
            }
            other => panic!("expected Runtime, got {other:?}"),
        }
        assert!(
            hub.emitted(EVENT_ADAPT_MANIFEST_GET).is_empty(),
            "a failed listing is not a refused one; it must not fall back"
        );
    }

    #[tokio::test]
    async fn a_listing_that_fails_without_wording_still_names_the_query() {
        for error in [json!(null), json!("   ")] {
            let hub = FakeHub {
                listing_fails_with: Some(error.clone()),
                ..Default::default()
            };
            let message = list_intents(&hub, "en-us", &IntentListOptions::default())
                .await
                .unwrap_err()
                .to_string();
            assert!(message.contains(EVENT_INTENT_LIST), "{error}: {message}");
            assert!(
                message.contains("refused the listing"),
                "{error}: {message}"
            );
        }
    }

    #[test]
    fn only_string_entries_survive_in_the_allowed_list() {
        // A number or a null in `allowed` is not a message type; stringifying
        // one would put "3" in front of an operator reading which types to
        // allow.
        let error = policy_denied(&Event::new(
            EVENT_POLICY_DENIED,
            object(json!({
                "denied_type": EVENT_INTENT_LIST,
                "code": "acl_disallowed_type",
                "data": {"allowed": ["speak", 3, null, "recognizer_loop:utterance"]},
            })),
            Context::new(),
            None,
        ));
        match &error {
            ThalovantError::PolicyDenied { allowed, .. } => {
                assert_eq!(allowed, &["speak", "recognizer_loop:utterance"]);
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_describe_that_never_comes_leaves_that_intent_without_sentences() {
        let hub = FakeHub {
            deaf_to_describes_for: Some(SHADOW),
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us"], &options(Some(Duration::from_millis(300))))
            .await
            .unwrap();

        let by_id: HashMap<String, &HubIntent> = inventory
            .intents()
            .map(|intent| (intent.id(), intent))
            .collect();
        assert!(!by_id[&format!("{WEATHER}:current.weather")]
            .phrases_for("en-us")
            .is_empty());
        assert!(by_id[&format!("{SHADOW}:custos.incidents")]
            .phrases_for("en-us")
            .is_empty());
        assert_eq!(inventory.source, IntentInventorySource::IntentManifest);
    }

    #[tokio::test]
    async fn a_reply_without_a_request_id_is_still_taken() {
        // A hub that does not echo the request id is not evidence of anything.
        let hub = FakeHub {
            echo_request_id: false,
            repeats: 1,
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us", "fr-fr"], &options(None))
            .await
            .unwrap();
        assert!(inventory.has_phrases());
        assert_eq!(
            inventory.skills[1].intents[0].phrases_for("fr-fr")[0],
            "quel temps fait-il"
        );
        assert_eq!(hub.emitted(EVENT_INTENT_DESCRIBE).len(), 3);
    }

    #[tokio::test]
    async fn low_level_calls_expose_the_manifest_rows_and_definitions() {
        let hub = FakeHub::default();
        let rows = list_intents(&hub, "fr-fr", &IntentListOptions::default())
            .await
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (
                    row.skill_id.as_str(),
                    row.intent_name.as_str(),
                    row.engine(),
                    row.enabled
                ))
                .collect::<Vec<_>>(),
            [(WEATHER, "current.weather", "padatious", true)]
        );
        assert_eq!(rows[0].lang, "fr-FR");
        assert!(same_language(&rows[0].lang, "fr-fr"));
        assert_eq!(rows[0].session_id, "default");
        assert!(rows[0].definition.is_none());

        let definitions = describe_intent(
            &hub,
            WEATHER,
            "current.weather",
            "fr-fr",
            DEFAULT_INTENT_TIMEOUT,
        )
        .await
        .unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].samples[0], "quel temps fait-il");
        assert_eq!(definitions[0].engine(), "padatious");
        assert_eq!(definitions[0].raw["blacklist"], json!([]));

        let unknown = describe_intent(
            &hub,
            SHADOW,
            "custos.incidents",
            "fr-fr",
            DEFAULT_INTENT_TIMEOUT,
        )
        .await
        .unwrap();
        assert!(unknown.is_empty());
    }

    #[tokio::test]
    async fn as_value_is_json_ready_and_complete() {
        let hub = FakeHub::default();
        let inventory = inventory(&hub, ["en-us", "fr-fr"], &options(None))
            .await
            .unwrap();
        let payload = inventory.as_value();

        assert_eq!(payload["source"], "intent-manifest");
        assert_eq!(payload["languages"], json!(["en-us", "fr-fr"]));
        assert_eq!(payload["denied"], json!([]));
        let weather = payload["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|skill| skill["skill_id"] == WEATHER)
            .unwrap();
        assert_eq!(weather["languages"], json!(["en-us", "fr-fr"]));
        assert_eq!(
            weather["intents"][0]["id"],
            format!("{WEATHER}:current.weather")
        );
        assert_eq!(
            weather["intents"][0]["phrases"]["fr-fr"][0],
            "quel temps fait-il"
        );
        assert_eq!(serde_json::to_value(&inventory).unwrap(), payload);
    }

    #[tokio::test]
    async fn languages_default_to_english() {
        let hub = FakeHub::default();
        inventory(&hub, Vec::<String>::new(), &options(None))
            .await
            .unwrap();
        assert_eq!(hub.emitted(EVENT_INTENT_LIST)[0].1["lang"], "en-us");

        assert_eq!(chosen_languages(["", "  "]), ["en-us"]);
        assert_eq!(
            chosen_languages([" fr-fr ", "en-us", "fr-fr"]),
            ["fr-fr", "en-us"]
        );
    }

    #[tokio::test]
    async fn languages_are_folded_and_deduplicated_before_asking() {
        let hub = FakeHub::default();
        let inventory = inventory(&hub, [" en-us ", "en-US", "en_us", "fr-fr"], &options(None))
            .await
            .unwrap();

        assert_eq!(inventory.languages, ["en-us", "fr-fr"]);
        assert_eq!(
            hub.emitted(EVENT_INTENT_LIST)
                .iter()
                .map(|(_, data, _)| data["lang"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            ["en-us", "fr-fr"]
        );
        assert_eq!(
            chosen_languages(["en-US", "en_us", "fr-fr", "FR_fr"]),
            ["en-US", "fr-fr"],
            "the first spelling seen is kept"
        );
    }

    #[tokio::test]
    async fn has_phrases_means_at_least_one_sentence() {
        let hub = FakeHub {
            registrations: vec![("en-us", vec![registration(SHADOW, "custos.incidents", &[])])],
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us"], &options(None)).await.unwrap();

        assert_eq!(inventory.intents().count(), 1);
        assert_eq!(inventory.intents().next().unwrap().languages(), ["en-us"]);
        assert!(
            !inventory.has_phrases(),
            "a describe that came back empty is not a phrase"
        );
    }

    #[tokio::test]
    async fn a_keyword_row_does_not_erase_the_template_rows_sentences() {
        // One intent, two registrations in one language: the keyword row has
        // no samples. Whether it arrives after or before the template row,
        // and whether the definitions ride on the listing or come from the
        // describes, the sentences survive and the first row names the engine.
        for (keyword_rows, definitions_in_list, engine) in [
            (KeywordRows::AfterTemplate, true, "padatious"),
            (KeywordRows::BeforeTemplate, true, "adapt"),
            (KeywordRows::AfterTemplate, false, "padatious"),
            (KeywordRows::BeforeTemplate, false, "adapt"),
        ] {
            let hub = FakeHub {
                registrations: vec![(
                    "en-us",
                    vec![registration(
                        WEATHER,
                        "current.weather",
                        &["what is the weather"],
                    )],
                )],
                keyword_rows,
                definitions_in_list,
                ..Default::default()
            };
            let inventory = inventory(&hub, ["en-us"], &options(None)).await.unwrap();

            assert_eq!(inventory.intents().count(), 1, "one intent, not two");
            let weather = inventory.intents().next().unwrap();
            assert_eq!(weather.phrases_for("en-us"), ["what is the weather"]);
            assert_eq!(weather.engine, engine, "the first row names the engine");
            assert!(inventory.has_phrases());
            // Only the template row is described; the keyword row never is.
            let describes = hub.emitted(EVENT_INTENT_DESCRIBE).len();
            assert_eq!(describes, usize::from(!definitions_in_list));
        }
    }

    /// A hub with `count` intents in one skill, each with one sentence.
    fn many_registrations(
        lang: &'static str,
        count: usize,
    ) -> Vec<(&'static str, Vec<Registration>)> {
        vec![(
            lang,
            (0..count)
                .map(|index| {
                    registration(
                        WEATHER,
                        &format!("intent.{index:03}"),
                        &[&format!("sentence number {index}")],
                    )
                })
                .collect(),
        )]
    }

    #[tokio::test]
    async fn a_large_inventory_is_described_in_bounded_batches() {
        // 69 intents is 69 describes and, at two copies a reply, 138 inbound
        // events against a 64-slot bus channel: sent as one window they would
        // outrun the receiver and the sentences would go missing. Three
        // windows of at most DESCRIBE_BATCH keep every reply.
        let hub = FakeHub {
            registrations: many_registrations("en-us", 69),
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us"], &options(None)).await.unwrap();

        assert_eq!(hub.emitted(EVENT_INTENT_DESCRIBE).len(), 69);
        let windows: Vec<usize> = hub
            .windows
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|sent| *sent > 0)
            .collect();
        assert_eq!(
            windows,
            [DESCRIBE_BATCH, DESCRIBE_BATCH, 69 - 2 * DESCRIBE_BATCH],
            "three windows, none over DESCRIBE_BATCH"
        );

        assert_eq!(inventory.intents().count(), 69);
        assert!(inventory.has_phrases());
        for (index, intent) in inventory.intents().enumerate() {
            assert_eq!(
                intent.phrases_for("en-us"),
                [format!("sentence number {index}")],
                "{} lost its sentence",
                intent.id()
            );
        }
    }

    #[tokio::test]
    async fn a_silent_hub_gives_up_after_one_describe_batch() {
        // Nothing is answered, so the first window's deadline ends it; the
        // remaining requests are never sent.
        let hub = FakeHub {
            registrations: many_registrations("en-us", 69),
            silent: vec![EVENT_INTENT_DESCRIBE],
            ..Default::default()
        };
        let error = inventory(&hub, ["en-us"], &options(Some(Duration::from_millis(200))))
            .await
            .unwrap_err();

        assert!(matches!(error, ThalovantError::Timeout(_)), "{error:?}");
        assert_eq!(
            hub.emitted(EVENT_INTENT_DESCRIBE).len(),
            DESCRIBE_BATCH,
            "one batch, not all 69 requests"
        );
    }

    #[tokio::test]
    async fn a_hub_that_goes_quiet_after_one_window_keeps_what_it_answered() {
        // Windows are contiguous slices, so a skill that stops answering can
        // own whole ones. The windows it did answer must survive: the intents
        // behind the silent windows simply carry no sentences.
        let hub = FakeHub {
            registrations: many_registrations("en-us", 69),
            describe_answer_limit: Some(DESCRIBE_BATCH),
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us"], &options(Some(Duration::from_millis(200))))
            .await
            .expect("a silent later window must not fail the inventory");

        assert_eq!(inventory.intents().count(), 69, "every intent is listed");
        assert!(inventory.has_phrases());
        for (index, intent) in inventory.intents().enumerate() {
            let sentences = intent.phrases_for("en-us");
            if index < DESCRIBE_BATCH {
                assert_eq!(
                    sentences,
                    [format!("sentence number {index}")],
                    "{} was answered and must keep its sentence",
                    intent.id()
                );
            } else {
                assert!(
                    sentences.is_empty(),
                    "{} was never described, so it has no sentences",
                    intent.id()
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn empty_describe_answers_do_not_hide_unanswered_registrations() {
        struct EmptyAnswers {
            hub: FakeHub,
            refused: bool,
            silence: bool,
        }
        impl HubLink for EmptyAnswers {
            fn subscribe(&self) -> broadcast::Receiver<Event> {
                self.hub.subscribe()
            }
            fn site_id(&self) -> Option<String> {
                self.hub.site_id()
            }
            async fn emit_bus(&self, name: &str, data: Data, context: Context) -> Result<()> {
                if self.silence && data["intent_name"] == "second" {
                    return Ok(());
                }
                let response = if self.refused {
                    json!({"ok": false, "error": "unknown intent"})
                } else {
                    json!({"ok": true, "definitions": []})
                };
                self.hub
                    .deliver(EVENT_INTENT_DESCRIBE_RESPONSE, response, &context);
                assert_eq!(name, EVENT_INTENT_DESCRIBE);
                Ok(())
            }
        }
        let wanted = vec![
            ("skill".into(), "first".into(), "en-us".into()),
            ("skill".into(), "second".into(), "en-us".into()),
        ];
        for refused in [false, true] {
            for batch in [1, 2] {
                let mut hub = EmptyAnswers {
                    hub: FakeHub::default(),
                    refused,
                    silence: true,
                };
                assert!(
                    matches!(
                        describe_many(&hub, &wanted, Duration::from_millis(20), batch).await,
                        Err(ThalovantError::Timeout(_))
                    ),
                    "refused={refused}, batch={batch}: no usable partial definition"
                );
                hub.silence = false;
                let found = describe_many(&hub, &wanted, Duration::from_secs(1), batch)
                    .await
                    .unwrap();
                assert_eq!(
                    found.len(),
                    2,
                    "explicit answers for every registration remain valid"
                );
            }
        }
    }

    #[tokio::test]
    async fn an_expired_describe_budget_never_publishes() {
        let hub = FakeHub::default();
        let wanted = vec![(WEATHER.into(), "current.weather".into(), "en-us".into())];
        assert!(matches!(
            describe_many(&hub, &wanted, Duration::ZERO, 1).await,
            Err(ThalovantError::Timeout(_))
        ));
        assert!(hub.emitted(EVENT_INTENT_DESCRIBE).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn describe_windows_share_one_deadline_and_keep_earlier_answers() {
        struct SlowFirstDescribe(FakeHub);
        impl HubLink for SlowFirstDescribe {
            fn subscribe(&self) -> broadcast::Receiver<Event> {
                self.0.subscribe()
            }
            fn site_id(&self) -> Option<String> {
                self.0.site_id()
            }
            async fn emit_bus(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
                if event_type == EVENT_INTENT_DESCRIBE
                    && self.0.emitted(EVENT_INTENT_DESCRIBE).is_empty()
                {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
                self.0.emit_bus(event_type, data, context).await
            }
        }
        let hub = SlowFirstDescribe(FakeHub {
            registrations: many_registrations("en-us", 3),
            describe_answer_limit: Some(1),
            ..Default::default()
        });
        let wanted: Vec<Wanted> = (0..3)
            .map(|index| (WEATHER.into(), format!("intent.{index:03}"), "en-us".into()))
            .collect();
        let budget = Duration::from_millis(100);
        let started = Instant::now();
        let found = describe_many(&hub, &wanted, budget, 1).await.unwrap();
        assert_eq!(
            found.len(),
            1,
            "the answered first window remains available"
        );
        assert_eq!(found[&wanted[0]][0].samples, vec!["sentence number 0"]);
        assert!(
            started.elapsed() <= budget + Duration::from_millis(1),
            "the slow first window consumed the shared budget: elapsed {:?}",
            started.elapsed()
        );
        assert_eq!(
            hub.0.emitted(EVENT_INTENT_DESCRIBE).len(),
            2,
            "no third window may publish after the shared deadline"
        );
    }

    #[tokio::test]
    async fn batching_is_what_keeps_a_window_inside_the_bus_channel() {
        // Why DESCRIBE_BATCH exists. 40 describes, each answered twice, is 80
        // events against the transports' 64 slots, and every request goes out
        // before the first reply is read: sent as one window (`batch` 0) the
        // oldest replies are overwritten before the receiver sees them and
        // those intents come back with no sentences. Bounded, the same 40 all
        // arrive.
        async fn describe_all(batch: usize) -> (usize, usize) {
            let hub = FakeHub {
                registrations: many_registrations("en-us", 40),
                ..Default::default()
            };
            let rows = list_intents(&hub, "en-us", &IntentListOptions::default())
                .await
                .unwrap();
            let wanted: Vec<Wanted> = rows
                .iter()
                .map(|row| {
                    (
                        row.skill_id.clone(),
                        row.intent_name.clone(),
                        "en-us".to_string(),
                    )
                })
                .collect();
            // A short deadline: the fake answers at once, so only the window
            // that lost replies ever waits it out.
            let found = describe_many(&hub, &wanted, Duration::from_millis(300), batch)
                .await
                .unwrap();
            let windows = hub
                .windows
                .lock()
                .unwrap()
                .iter()
                .filter(|sent| **sent > 0)
                .count();
            (found.len(), windows)
        }

        assert_eq!(
            describe_all(DESCRIBE_BATCH).await,
            (40, 2),
            "bounded: two windows, every definition kept"
        );
        let (found, windows) = describe_all(0).await;
        assert_eq!(windows, 1, "`batch` 0 sends them all at once");
        assert!(
            found < 40,
            "precondition: one window of 40 overruns the channel, found {found}"
        );
    }

    #[tokio::test]
    async fn the_fallback_keeps_the_first_engine_that_names_an_intent() {
        let hub = FakeHub {
            refuse: vec![EVENT_INTENT_LIST],
            adapt_names: vec![format!("{WEATHER}:current.weather")],
            ..Default::default()
        };
        let inventory = inventory(&hub, ["en-us"], &options(None)).await.unwrap();

        assert_eq!(inventory.source, IntentInventorySource::EngineManifests);
        let weather = inventory
            .intents()
            .find(|intent| intent.name == "current.weather")
            .unwrap();
        assert_eq!(weather.engine, "adapt", "adapt is asked first and named it");
        let shadow = inventory
            .intents()
            .find(|intent| intent.name == "custos.incidents")
            .unwrap();
        assert_eq!(shadow.engine, "padatious");
        assert_eq!(
            inventory.intents().count(),
            2,
            "a name both engines list is one intent"
        );
    }

    #[tokio::test]
    async fn describe_is_skipped_when_not_asked_for() {
        let hub = FakeHub::default();
        let inventory = inventory(
            &hub,
            ["en-us"],
            &IntentInventoryOptions {
                describe: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(hub.emitted(EVENT_INTENT_DESCRIBE).is_empty());
        assert_eq!(inventory.intents().count(), 2);
        assert!(!inventory.has_phrases());
        assert_eq!(
            hub.emitted(EVENT_INTENT_LIST)[0]
                .1
                .get("include_definitions"),
            None
        );
    }

    #[test]
    fn manifest_rows_need_a_skill_and_an_intent() {
        assert!(IntentRegistration::from_value(&json!({"intent_name": "x"})).is_none());
        assert!(
            IntentRegistration::from_value(&json!({"skill_id": " ", "intent_name": "x"})).is_none()
        );
        let row = IntentRegistration::from_value(&json!({
            "skill_id": "skill", "intent_name": "name", "enabled": false, "method": "keyword"
        }))
        .unwrap();
        assert!(!row.enabled);
        assert_eq!(row.engine(), "adapt");
        assert_eq!(row.session_id, "default");
        assert_eq!(row.lang, "");
        assert!(IntentDefinition::from_value(&json!({"method": "template"})).is_none());
        let definition = IntentDefinition::from_value(&json!({
            "definition": {"skill_id": "skill", "intent_name": "name", "method": "template",
                           "samples": ["  hello ", "", 3]}
        }))
        .unwrap();
        assert_eq!(definition.method, "template");
        assert_eq!(definition.samples, ["hello"]);
    }

    #[test]
    fn language_tags_fold_case_and_separators() {
        assert!(same_language("fr-fr", "fr_FR"));
        assert!(same_language(" en-US", "en-us "));
        assert!(!same_language("en-us", "en-gb"));
    }

    #[test]
    fn client_intent_futures_are_send() {
        fn assert_send<T: Send>(_: T) {}
        let identity = Identity::from_value(json!({
            "access_key": "access",
            "password": "secret",
            "site_id": "site",
            "default_master": "http://127.0.0.1:1"
        }))
        .unwrap();
        let client = Client::new(identity);
        assert_send(client.intents(["en-us"], IntentInventoryOptions::default()));
        assert_send(client.intents_with_capabilities(["en-us"], IntentInventoryOptions::default()));
        assert_send(client.list_fallbacks(None));
        assert_send(client.ask_with_options("hello", crate::AskOptions::default()));
        assert_send(client.list_intents("en-us", IntentListOptions::default()));
        assert_send(client.describe_intent(
            WEATHER,
            "current.weather",
            "en-us",
            IntentDescribeOptions::default(),
        ));
    }
    #[tokio::test]
    async fn silent_listing_uses_engine_names_and_records_the_unavailable_query() {
        let hub = FakeHub {
            silent: vec![EVENT_INTENT_LIST],
            ..Default::default()
        };
        let result = inventory(&hub, ["en-us"], &options(Some(Duration::from_millis(20))))
            .await
            .unwrap();
        assert_eq!(result.source, IntentInventorySource::EngineManifests);
        assert!(!result.skills.is_empty());
        assert_eq!(result.denied, vec![EVENT_INTENT_LIST]);
        let hub = FakeHub {
            silent: vec![
                EVENT_INTENT_LIST,
                EVENT_ADAPT_MANIFEST_GET,
                EVENT_PADATIOUS_MANIFEST_GET,
            ],
            ..Default::default()
        };
        assert!(matches!(
            inventory(&hub, ["en-us"], &options(Some(Duration::from_millis(20)))).await,
            Err(ThalovantError::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn capability_probe_distinguishes_unknown_empty_and_registered_fallbacks() {
        let opts = options(Some(Duration::from_millis(20)));
        for response in [
            json!({}),
            json!({"fallbacks":null}),
            json!({"fallbacks":{}}),
            json!({"ok":false,"fallbacks":[]}),
        ] {
            let hub = FakeHub {
                fallback_response: response,
                ..Default::default()
            };
            let result = inventory_with_capabilities(&hub, ["de-de"], &opts)
                .await
                .unwrap();
            assert!(!result.fallbacks_known);
            assert!(result.may_answer("de-de"));
        }
        for (refuse, silent) in [
            (vec![EVENT_FALLBACK_LIST], vec![]),
            (vec![], vec![EVENT_FALLBACK_LIST]),
        ] {
            let hub = FakeHub {
                refuse,
                silent,
                ..Default::default()
            };
            let result = inventory_with_capabilities(&hub, ["de-de"], &opts)
                .await
                .unwrap();
            assert!(!result.fallbacks_known);
            assert!(result.may_answer("de-de"));
        }
        let mut result = inventory_with_capabilities(&FakeHub::default(), ["en-us"], &opts)
            .await
            .unwrap();
        assert!(result.fallbacks_known);
        assert!(result.may_answer("en_US"));
        assert!(!result.may_answer("de-de"));
        for skill in &mut result.inventory.skills {
            for intent in &mut skill.intents {
                intent.enabled = false;
            }
        }
        assert!(!result.may_answer("en-us"));
        let hub = FakeHub {
            fallback_response: json!({"fallbacks":[null, {}, {"skill_id":""}, {"skill_id":"z", "priority":12}, {"skill_id":"b", "priority":1.9}, {"skill_id":"a", "priority":1}, {"skill_id":"c", "priority":true}, {"skill_id":"d", "priority":false}, {"skill_id":"out-of-range", "priority":u64::MAX}]}),
            ..Default::default()
        };
        let result = inventory_with_capabilities(&hub, ["de-de"], &opts)
            .await
            .unwrap();
        assert_eq!(
            result
                .fallbacks
                .iter()
                .map(|row| row.skill_id.as_str())
                .collect::<Vec<_>>(),
            vec!["d", "a", "b", "c", "z"]
        );
        assert_eq!(
            result
                .fallbacks
                .iter()
                .map(|row| row.priority)
                .collect::<Vec<_>>(),
            vec![0, 1, 1, 1, 12]
        );
        assert!(result.may_answer("de-de"));
        assert_eq!(result.as_value()["fallbacks_known"], true);
    }

    #[tokio::test]
    async fn request_timeout_also_bounds_a_blocked_send() {
        let hub = FakeHub {
            blocked_send: true,
            ..Default::default()
        };
        assert!(list_fallbacks(&hub, Duration::from_millis(20))
            .await
            .unwrap()
            .is_none());
    }
    #[tokio::test]
    async fn foreign_policy_denial_does_not_cancel_a_correlated_request() {
        let hub = FakeHub {
            foreign_denial: true,
            ..Default::default()
        };
        let result = inventory_with_capabilities(
            &hub,
            ["en-us"],
            &options(Some(Duration::from_millis(100))),
        )
        .await
        .unwrap();
        assert!(result.inventory.has_phrases());
        assert!(result.fallbacks_known);
    }

    #[tokio::test]
    async fn foreign_describe_id_never_falls_back_to_matching_definition_content() {
        let hub = FakeHub {
            foreign_describes_only: true,
            ..Default::default()
        };
        let wanted = vec![(
            WEATHER.to_string(),
            "current.weather".into(),
            "en-us".into(),
        )];
        assert!(matches!(
            describe_many(&hub, &wanted, Duration::from_millis(20), 32).await,
            Err(ThalovantError::Timeout(_))
        ));
        let blocked = FakeHub {
            blocked_send: true,
            ..Default::default()
        };
        assert!(matches!(
            describe_many(&blocked, &wanted, Duration::from_millis(20), 32).await,
            Err(ThalovantError::Timeout(_))
        ));
    }
}
