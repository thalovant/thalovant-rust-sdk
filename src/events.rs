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
    pub dropped_media: usize,
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

pub fn event_matches_context(event: &Event, expected: Option<&Context>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    // The request id decides, when both sides carry one. A hub does not echo a
    // client-declared session id: it substitutes its own. Observed against a
    // live hub on 2026-09-03 -- sent "observe-me", every reply came back as
    // "71048b7f-e7b0-4360-8fb5-a03816f78617" -- so comparing session ids
    // rejected replies the request id had already identified as ours.
    if let (Some(expected_request), Some(event_request)) =
        (request_id_from_context(expected), event.request_id())
    {
        return expected_request == event_request;
    }
    // No request id on one side or the other: fall back to the session, which
    // is all a caller had before request ids existed.
    if let (Some(expected_session), Some(event_session)) =
        (session_id_from_context(expected), event.session_id())
    {
        if expected_session != event_session {
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
            dropped_media: 0,
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
    //! A hub substitutes its own session id; the request id is what correlates.
    //!
    //! Observed against a live hub on 2026-09-03: a client declaring
    //! `session_id="observe-me"` gets every reply back carrying the hub's own
    //! uuid. Comparing session ids rejected replies the request id had already
    //! identified as ours, so `ask()` timed out while the hub had answered.
    use super::*;
    use serde_json::json;

    fn ctx(session: Option<&str>, request: Option<&str>) -> Context {
        let mut m = serde_json::Map::new();
        if let Some(s) = session {
            m.insert("session".into(), json!({ "session_id": s }));
        }
        if let Some(r) = request {
            m.insert("request_id".into(), json!(r));
        }
        m
    }

    fn event(session: Option<&str>, request: Option<&str>) -> Event {
        Event::new(
            "ovos.utterance.handled",
            serde_json::Map::new(),
            ctx(session, request),
            None,
        )
    }

    #[test]
    fn a_matching_request_id_wins_over_a_substituted_session() {
        let e = event(Some("71048b7f-e7b0-4360-8fb5-a03816f78617"), Some("req-1"));
        assert!(event_matches_context(
            &e,
            Some(&ctx(Some("observe-me"), Some("req-1")))
        ));
    }

    #[test]
    fn a_wrong_request_id_is_rejected_even_if_sessions_agree() {
        let e = event(Some("same"), Some("req-2"));
        assert!(!event_matches_context(
            &e,
            Some(&ctx(Some("same"), Some("req-1")))
        ));
    }

    #[test]
    fn without_request_ids_the_session_still_decides() {
        assert!(event_matches_context(
            &event(Some("s1"), None),
            Some(&ctx(Some("s1"), None))
        ));
        assert!(!event_matches_context(
            &event(Some("s2"), None),
            Some(&ctx(Some("s1"), None))
        ));
    }
}

impl Event {
    pub fn lang(&self) -> Option<String> {
        [
            self.data.get("lang"),
            self.context.get("lang"),
            self.context.get("session").and_then(|s| s.get("lang")),
        ]
        .into_iter()
        .flatten()
        .find(|v| !v.is_null() && v.as_str() != Some(""))
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
    }
    pub fn is_audio(&self) -> bool {
        self.name == crate::EVENT_AUDIO_QUEUE
    }
    pub fn has_audio(&self) -> bool {
        self.is_audio()
            && self
                .data
                .get("binary_data")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
    }
    pub fn audio_bytes(&self) -> crate::Result<Vec<u8>> {
        self.audio_bytes_with_limit(crate::MAX_AUDIO_CLIP_BYTES)
    }
    /// Decode embedded hex only. Never fetch a path or URL returned by a skill.
    pub fn audio_bytes_with_limit(&self, max_bytes: usize) -> crate::Result<Vec<u8>> {
        let fail = || {
            crate::ThalovantError::Runtime("missing, invalid or oversized embedded audio".into())
        };
        if !self.is_audio() {
            return Err(fail());
        }
        let encoded = self
            .data
            .get("binary_data")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(fail)?;
        if encoded.len().div_ceil(2) > max_bytes {
            return Err(fail());
        }
        let mut compact = Vec::with_capacity(encoded.len());
        for byte in encoded.bytes() {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 11 | 12) {
                if compact.len() % 2 != 0 {
                    return Err(fail());
                }
            } else {
                compact.push(byte);
            }
        }
        hex::decode(compact).map_err(|_| fail())
    }
}
impl Reply {
    pub fn lang(&self) -> Option<String> {
        self.events.iter().find_map(Event::lang)
    }
    pub fn has_audio(&self) -> bool {
        self.events.iter().any(Event::is_audio)
    }
    pub fn media_events(&self) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|e| {
                e.is_audio()
                    || e.name == crate::EVENT_SPEAK
                    || e.name == crate::EVENT_OVOS_UTTERANCE_SPEAK
            })
            .collect()
    }
}
#[derive(Default)]
pub(crate) struct ReplyMediaBudget {
    chars: usize,
    pub dropped: usize,
}
impl ReplyMediaBudget {
    pub fn accept(&mut self, event: &Event) -> bool {
        if !event.is_audio() {
            return true;
        }
        let Some(encoded) = event.data.get("binary_data").and_then(Value::as_str) else {
            self.dropped += 1;
            return false;
        };
        if encoded.len() > crate::MAX_AUDIO_CLIP_BYTES * 2
            || self.chars + encoded.len() > crate::MAX_REPLY_MEDIA_BYTES * 2
        {
            self.dropped += 1;
            return false;
        }
        self.chars += encoded.len();
        true
    }
}
