use crate::constants::{is_failure_event, EVENT_RECOGNIZER_LOOP_UTTERANCE};
use crate::redact::{redact_map, redact_value};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use uuid::Uuid;

pub type Context = Map<String, Value>;
pub type Data = Map<String, Value>;

// `Debug` is hand-written (below) to redact secret keys (e.g. `auth_token`,
// `auth.token`) carried in `data`/`context`/`raw`; `Serialize`/`Deserialize`
// stay derived so the wire protocol keeps round-tripping real values.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub name: String,
    #[serde(default)]
    pub data: Data,
    #[serde(default)]
    pub context: Context,
    #[serde(default)]
    pub raw: Option<Value>,
}

// `Debug` is hand-written (below); a `Reply` nests `Event`s whose contexts carry
// the end-user bearer token, so `{:?}` must route through `Event`'s redaction.
#[derive(Clone, PartialEq)]
pub struct Reply {
    pub text: String,
    pub utterances: Vec<String>,
    pub handled: bool,
    pub ok: bool,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub events: Vec<Event>,
    pub failure_event: Option<Event>,
}

impl Event {
    pub fn new(name: impl Into<String>, data: Data, context: Context, raw: Option<Value>) -> Self {
        Self {
            name: name.into(),
            data,
            context,
            raw,
        }
    }

    pub fn text(&self) -> String {
        if let Some(value) = self.data.get("utterance").and_then(Value::as_str) {
            return value.to_string();
        }
        if let Some(value) = self.data.get("text").and_then(Value::as_str) {
            return value.to_string();
        }
        self.utterances().into_iter().next().unwrap_or_default()
    }

    pub fn utterances(&self) -> Vec<String> {
        match self.data.get("utterances") {
            Some(Value::String(value)) => vec![value.clone()],
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => self
                .data
                .get("utterance")
                .and_then(Value::as_str)
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
        }
    }

    pub fn session_id(&self) -> Option<String> {
        session_id_from_context(&self.context)
    }

    pub fn request_id(&self) -> Option<String> {
        request_id_from_context(&self.context).or_else(|| request_id_from_map(&self.data))
    }

    pub fn is_failure(&self) -> bool {
        is_failure_event(&self.name)
    }
}

impl fmt::Debug for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Event")
            .field("name", &self.name)
            .field("data", &redact_map(&self.data))
            .field("context", &redact_map(&self.context))
            .field("raw", &self.raw.as_ref().map(redact_value))
            .finish()
    }
}

impl fmt::Debug for Reply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Reply")
            .field("text", &self.text)
            .field("utterances", &self.utterances)
            .field("handled", &self.handled)
            .field("ok", &self.ok)
            .field("session_id", &self.session_id)
            .field("request_id", &self.request_id)
            // `events`/`failure_event` route through `Event`'s redacting Debug.
            .field("events", &self.events)
            .field("failure_event", &self.failure_event)
            .finish()
    }
}

pub fn new_session_id() -> String {
    format!("thalovant-session-{}", Uuid::new_v4().simple())
}

pub fn new_request_id() -> String {
    format!("thalovant-request-{}", Uuid::new_v4().simple())
}

pub fn utterance_payload(text: impl Into<String>, lang: impl Into<String>) -> Data {
    let mut data = Data::new();
    data.insert(
        "utterances".to_string(),
        Value::Array(vec![Value::String(text.into())]),
    );
    data.insert("lang".to_string(), Value::String(lang.into()));
    data
}

pub fn merge_context(base: Option<&Context>, extra: Option<&Context>) -> Context {
    let mut merged = base.cloned().unwrap_or_default();
    if let Some(extra) = extra {
        for (key, value) in extra {
            if key == "session" {
                if let Some(next_session) = value.as_object() {
                    let mut session = session_from_context(&merged);
                    for (session_key, session_value) in next_session {
                        session.insert(session_key.clone(), session_value.clone());
                    }
                    merged.insert("session".to_string(), Value::Object(session));
                    continue;
                }
            }
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

pub fn context_with_correlation(
    context: Option<&Context>,
    session_id: Option<&str>,
    site_id: Option<&str>,
    lang: Option<&str>,
    request_id: Option<&str>,
) -> Context {
    let mut next = merge_context(context, None);
    let mut session = session_from_context(&next);
    if let Some(value) = session_id.filter(|value| !value.is_empty()) {
        session.insert("session_id".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = site_id.filter(|value| !value.is_empty()) {
        session
            .entry("site_id".to_string())
            .or_insert_with(|| Value::String(value.to_string()));
    }
    if let Some(value) = lang.filter(|value| !value.is_empty()) {
        session
            .entry("lang".to_string())
            .or_insert_with(|| Value::String(value.to_string()));
    }
    if let Some(value) = request_id.filter(|value| !value.is_empty()) {
        next.insert("request_id".to_string(), Value::String(value.to_string()));
        next.insert(
            "thalovant_request_id".to_string(),
            Value::String(value.to_string()),
        );
        session.insert("request_id".to_string(), Value::String(value.to_string()));
    }
    if !session.is_empty() {
        next.insert("session".to_string(), Value::Object(session));
    }
    next
}

/// True when a reply's session id is the one we asked for.
///
/// A hub rewrites a client-declared session id before the orchestrator sees
/// it: hivemind-core derives a Layer-1 identity as `{conn_nonce}:{declared}`
/// so two clients cannot collide on the same declared name
/// (HIVEMIND-BRIDGE-1 §4), and only admin connections are exempt. Replies can
/// therefore carry either form, and comparing for equality rejected every one
/// of them -- `ask()` timed out while the hub had already answered and emitted
/// `ovos.utterance.handled`.
///
/// Matching the part after the first `:` mirrors what the hub does on the way
/// out. Deliberately not a bare `ends_with`: a declared id of `b` must not
/// match a reply for `a:xb`.
pub fn session_ids_match(expected: &str, actual: &str) -> bool {
    if actual == expected {
        return true;
    }
    match actual.split_once(':') {
        Some((_, declared)) => declared == expected,
        None => false,
    }
}

pub fn event_matches_context(event: &Event, expected: Option<&Context>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    if let (Some(expected_session), Some(event_session)) =
        (session_id_from_context(expected), event.session_id())
    {
        if !session_ids_match(&expected_session, &event_session) {
            return false;
        }
    }
    if let (Some(expected_request), Some(event_request)) =
        (request_id_from_context(expected), event.request_id())
    {
        if expected_request != event_request {
            return false;
        }
    }
    true
}

pub(crate) fn event_from_bus_payload(payload: &Map<String, Value>, raw: Option<Value>) -> Event {
    let name = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(EVENT_RECOGNIZER_LOOP_UTTERANCE)
        .to_string();
    let data = payload
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let context = payload
        .get("context")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Event::new(name, data, context, raw)
}

fn session_id_from_context(context: &Context) -> Option<String> {
    session_from_context(context)
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            context
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn request_id_from_context(context: &Context) -> Option<String> {
    request_id_from_map(context).or_else(|| request_id_from_map(&session_from_context(context)))
}

fn session_from_context(context: &Context) -> Context {
    context
        .get("session")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn request_id_from_map(values: &Map<String, Value>) -> Option<String> {
    ["request_id", "thalovant_request_id", "correlation_id"]
        .iter()
        .find_map(|key| values.get(*key).and_then(Value::as_str).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_text_and_context_matching_work() {
        let context = context_with_correlation(
            None,
            Some("session-1"),
            Some("site"),
            Some("en-us"),
            Some("request-1"),
        );
        let mut data = Data::new();
        data.insert("utterance".to_string(), Value::String("hi".to_string()));
        let event = Event::new("speak", data, context.clone(), None);
        assert_eq!(event.text(), "hi");
        assert_eq!(event.session_id().as_deref(), Some("session-1"));
        assert_eq!(event.request_id().as_deref(), Some("request-1"));
        assert!(event_matches_context(&event, Some(&context)));
    }

    #[test]
    fn event_and_reply_debug_redact_bearer_token_but_event_serialize_roundtrips() {
        // The bearer token is duplicated by `build_client_context` into both
        // `context.auth.token` and top-level `context.auth_token`.
        let mut auth = Map::new();
        auth.insert(
            "token".to_string(),
            Value::String("bearer-LIVE-SECRET".to_string()),
        );
        let mut context = Context::new();
        context.insert("auth".to_string(), Value::Object(auth));
        context.insert(
            "auth_token".to_string(),
            Value::String("bearer-LIVE-SECRET".to_string()),
        );
        let raw = serde_json::json!({"context": {"auth_token": "bearer-LIVE-SECRET"}});
        let event = Event::new("recognizer_loop:utterance", Data::new(), context, Some(raw));

        let debug = format!("{event:?}");
        assert!(
            !debug.contains("bearer-LIVE-SECRET"),
            "Event Debug leaked token: {debug}"
        );
        assert!(debug.contains("<redacted>"));

        // Serialize/Deserialize is the wire protocol and MUST keep the real token.
        let serialized = serde_json::to_value(&event).unwrap();
        assert_eq!(serialized["context"]["auth_token"], "bearer-LIVE-SECRET");
        assert_eq!(serialized["context"]["auth"]["token"], "bearer-LIVE-SECRET");
        let restored: Event = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored, event);

        // `{:?}` on a Reply is a central use case; it must redact too.
        let reply = Reply {
            text: "hello".to_string(),
            utterances: vec!["hello".to_string()],
            handled: true,
            ok: true,
            session_id: None,
            request_id: None,
            events: vec![event.clone()],
            failure_event: Some(event),
        };
        let reply_debug = format!("{reply:?}");
        assert!(
            !reply_debug.contains("bearer-LIVE-SECRET"),
            "Reply Debug leaked token: {reply_debug}"
        );
        assert!(reply_debug.contains("<redacted>"));
    }
}

#[cfg(test)]
mod session_nat_tests {
    //! A hub rewrites a declared session id; replies must still be recognised.
    //!
    //! hivemind-core derives a Layer-1 identity for every client-declared
    //! session as `{conn_nonce}:{declared}` (HIVEMIND-BRIDGE-1 §4). Comparing
    //! the returned id to the sent one for equality rejected every reply:
    //! `ask()` timed out while the hub had already answered. Reproduced
    //! against a live hub on 2026-09-03.
    use super::session_ids_match;

    #[test]
    fn a_nat_rewritten_reply_is_recognised() {
        assert!(session_ids_match("my-session", "d41d8cd98f00b204:my-session"));
    }

    #[test]
    fn an_unrewritten_reply_is_still_recognised() {
        assert!(session_ids_match("my-session", "my-session"));
    }

    #[test]
    fn a_reply_for_a_different_session_is_rejected() {
        assert!(!session_ids_match("my-session", "nonce:other"));
        assert!(!session_ids_match("my-session", "other"));
    }

    #[test]
    fn only_the_declared_half_after_the_first_colon_matches() {
        // a bare ends_with would wrongly accept these
        assert!(!session_ids_match("abc", "nonce:xabc"));
        assert!(!session_ids_match("abc", "nonce:abc:def"));
        // a declared id containing a colon still matches as a whole
        assert!(session_ids_match("a:b", "nonce:a:b"));
        assert!(!session_ids_match("abc", ""));
        assert!(!session_ids_match("abc", "nonce:"));
    }
}
