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
//!   intent individually.
//! - `ovos.intent.describe` `{"skill_id", "intent_name", "lang"}` ->
//!   `ovos.intent.describe.response` `{"ok", "definitions": [{method,
//!   definition}]}` or `{"ok": false, "error"}`.
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
        EVENT_ADAPT_MANIFEST, EVENT_ADAPT_MANIFEST_GET, EVENT_INTENT_DESCRIBE,
        EVENT_INTENT_DESCRIBE_RESPONSE, EVENT_INTENT_LIST, EVENT_INTENT_LIST_RESPONSE,
        EVENT_PADATIOUS_MANIFEST, EVENT_PADATIOUS_MANIFEST_GET, EVENT_POLICY_DENIED,
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
use tokio::{sync::broadcast, time::timeout};

/// How long each intent query waits for its reply unless an option says otherwise.
pub const DEFAULT_INTENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The language asked about when the caller names none.
pub(crate) const DEFAULT_LANG: &str = "en-us";

/// Options for [`Client::intents`](crate::Client::intents).
///
/// The default describes every template registration for its sentences and
/// falls back to the engines' manifests when the hub refuses `ovos.intent.list`.
#[derive(Clone, Debug)]
pub struct IntentInventoryOptions {
    /// Deadline for each listing, and for the whole batch of describes. Default 5s.
    pub timeout: Option<Duration>,
    /// Ask for the sentences behind every template intent. Off, the inventory
    /// carries the intents and their engines but no phrases.
    pub describe: bool,
    /// When the hub refuses `ovos.intent.list`, read the engines' own manifests
    /// instead and return names only, marked `engine-manifests`. Off, the
    /// refusal is returned as [`ThalovantError::PolicyDenied`].
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
    /// inventory's `denied` names the query the hub refused.
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
/// is the names-only fallback, and `denied` then names the query the hub refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubIntentInventory {
    /// The languages asked for, in the order they were asked.
    pub languages: Vec<String>,
    /// Sorted by skill id.
    pub skills: Vec<HubSkillIntents>,
    pub source: IntentInventorySource,
    /// The queries the hub refused on the way to this inventory.
    pub denied: Vec<String>,
}

impl HubIntentInventory {
    /// Every intent across skills.
    pub fn intents(&self) -> impl Iterator<Item = &HubIntent> + '_ {
        self.skills.iter().flat_map(|skill| skill.intents.iter())
    }

    /// Whether any intent carries sentences; false for the names-only fallback.
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

/// The languages an inventory call asks about: trimmed, deduplicated, and
/// `en-us` when the caller named none.
pub(crate) fn chosen_languages<I, S>(languages: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut chosen: Vec<String> = Vec::new();
    for language in languages {
        let tag = language.as_ref().trim();
        if !tag.is_empty() && !chosen.iter().any(|seen| seen == tag) {
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
    mut accept: impl FnMut(&Event) -> Option<T>,
) -> Result<T> {
    timeout(deadline, async {
        loop {
            let event = match receiver.recv().await {
                Ok(event) => event,
                // Replies we could not keep up with are gone; the ones still
                // queued may be the ones we want.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(ThalovantError::Connection(
                        "hub session closed while waiting for a reply".to_string(),
                    ))
                }
            };
            if denied_type_of(&event) == Some(query_type) {
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
    link.emit_bus(query_type, data, context.clone()).await?;
    await_reply(&mut receiver, deadline, query_type, |event| {
        (event.name == reply_type && event_matches_context(event, Some(&context)))
            .then(|| event.clone())
    })
    .await
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

/// Describe many registrations with the requests in flight together.
///
/// One subscription, one request id per registration, replies matched by that
/// id, repeats dropped. A hub that does not echo the id is matched by the
/// definition's own `skill_id`/`intent_name`/`lang`. The deadline covers the
/// whole batch, and a partial answer is still an answer: the intents the hub
/// did not describe in time are simply absent from the result.
pub(crate) async fn describe_many<L: HubLink>(
    link: &L,
    wanted: &[Wanted],
    deadline: Duration,
) -> Result<HashMap<Wanted, Vec<IntentDefinition>>> {
    let mut unique: Vec<Wanted> = Vec::new();
    for key in wanted {
        if !unique.contains(key) {
            unique.push(key.clone());
        }
    }
    let mut found: HashMap<Wanted, Vec<IntentDefinition>> = HashMap::new();
    if unique.is_empty() {
        return Ok(found);
    }

    let mut receiver = link.subscribe();
    let mut by_request: HashMap<String, Wanted> = HashMap::new();
    for key in &unique {
        let (skill_id, intent_name, lang) = key;
        let request_id = new_request_id();
        by_request.insert(request_id.clone(), key.clone());
        link.emit_bus(
            EVENT_INTENT_DESCRIBE,
            describe_payload(skill_id, intent_name, lang),
            query_context(link, lang, &request_id),
        )
        .await?;
    }

    let waited = await_reply(&mut receiver, deadline, EVENT_INTENT_DESCRIBE, |event| {
        if event.name != EVENT_INTENT_DESCRIBE_RESPONSE {
            return None;
        }
        let definitions = definitions_of(event);
        let key = event
            .request_id()
            .and_then(|request_id| by_request.get(&request_id).cloned())
            .or_else(|| {
                // No request id came back: the definition names what it describes.
                let first = definitions.first()?;
                unique
                    .iter()
                    .find(|(skill_id, intent_name, lang)| {
                        *skill_id == first.skill_id
                            && *intent_name == first.intent_name
                            && same_language(lang, &first.lang)
                    })
                    .cloned()
            })?;
        if found.contains_key(&key) {
            return None;
        }
        found.insert(key, definitions);
        (found.len() == unique.len()).then_some(())
    })
    .await;
    match waited {
        Ok(()) => Ok(found),
        Err(ThalovantError::Timeout(message)) => {
            if found.is_empty() {
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
            by_skill.entry(skill_id.clone()).or_default().insert(
                intent_name.clone(),
                HubIntent {
                    skill_id,
                    name: intent_name,
                    engine: engine.to_string(),
                    enabled: true,
                    phrases: BTreeMap::new(),
                },
            );
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
                if !(opts.fallback && listing_refused) {
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
        describe_many(link, &wanted, deadline).await?
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
            phrases
                .entry(key)
                .or_default()
                .insert(lang.clone(), sentences);
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

    type Registration = ((&'static str, &'static str), Vec<&'static str>);

    /// What the hub registered: per language, per intent, the sentences.
    /// Weather speaks both languages; the shadow skill only English.
    fn registrations(lang: &str) -> Vec<Registration> {
        match lang {
            "en-us" => vec![
                (
                    (WEATHER, "current.weather"),
                    vec![
                        "what is the weather",
                        "what is the weather in {location}",
                        "how is it outside",
                    ],
                ),
                (
                    (SHADOW, "custos.incidents"),
                    vec!["are there incidents", "any incidents"],
                ),
            ],
            "fr-fr" => vec![(
                (WEATHER, "current.weather"),
                vec![
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

    /// A hub session: answers the manifest, or refuses it, twice over.
    struct FakeHub {
        refuse: Vec<&'static str>,
        silent: Vec<&'static str>,
        definitions_in_list: bool,
        echo_request_id: bool,
        repeats: usize,
        /// A skill whose describes go unanswered.
        deaf_to_describes_for: Option<&'static str>,
        tx: broadcast::Sender<Event>,
        emitted: Mutex<Vec<Emitted>>,
    }

    impl Default for FakeHub {
        fn default() -> Self {
            Self {
                refuse: Vec::new(),
                silent: Vec::new(),
                definitions_in_list: false,
                echo_request_id: true,
                repeats: 2,
                deaf_to_describes_for: None,
                tx: broadcast::channel(256).0,
                emitted: Mutex::new(Vec::new()),
            }
        }
    }

    impl FakeHub {
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

        fn definition(lang: &str, skill_id: &str, intent_name: &str, samples: &[&str]) -> Value {
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
            self.tx.subscribe()
        }

        async fn emit_bus(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
            self.emitted.lock().unwrap().push((
                event_type.to_string(),
                data.clone(),
                context.clone(),
            ));
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
                EVENT_INTENT_LIST => {
                    let rows: Vec<Value> = registrations(&lang)
                        .into_iter()
                        .map(|((skill_id, intent_name), samples)| {
                            let mut row = json!({
                                "skill_id": skill_id,
                                "intent_name": intent_name,
                                // The runtime standardises what it stores.
                                "lang": if lang == "fr-fr" { "fr-FR".to_string() } else { lang.clone() },
                                "method": "template",
                                "enabled": true,
                                "session_id": "default",
                            });
                            if self.definitions_in_list
                                && data.get("include_definitions") == Some(&Value::Bool(true))
                            {
                                row["definition"] =
                                    Self::definition(&lang, skill_id, intent_name, &samples);
                            }
                            row
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
                    let samples = registrations(&lang)
                        .into_iter()
                        .find(|(key, _)| *key == (skill_id, intent_name))
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
                    self.deliver(EVENT_ADAPT_MANIFEST, json!({"intents": []}), &context);
                }
                EVENT_PADATIOUS_MANIFEST_GET => {
                    let names: BTreeSet<String> = ["en-us", "fr-fr"]
                        .into_iter()
                        .flat_map(registrations)
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
    async fn a_silent_hub_times_out_on_the_listing() {
        let hub = FakeHub {
            silent: vec![EVENT_INTENT_LIST],
            ..Default::default()
        };
        let error = inventory(&hub, ["en-us"], &options(Some(Duration::from_millis(200))))
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
        assert_send(client.list_intents("en-us", IntentListOptions::default()));
        assert_send(client.describe_intent(
            WEATHER,
            "current.weather",
            "en-us",
            IntentDescribeOptions::default(),
        ));
    }
}
