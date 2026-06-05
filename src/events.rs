use crate::constants::{is_failure_event, EVENT_RECOGNIZER_LOOP_UTTERANCE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

pub type Context = Map<String, Value>;
pub type Data = Map<String, Value>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub name: String,
    #[serde(default)]
    pub data: Data,
    #[serde(default)]
    pub context: Context,
    #[serde(default)]
    pub raw: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
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
            Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).map(str::to_string).collect(),
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
        session.entry("site_id".to_string()).or_insert_with(|| Value::String(value.to_string()));
    }
    if let Some(value) = lang.filter(|value| !value.is_empty()) {
        session.entry("lang".to_string()).or_insert_with(|| Value::String(value.to_string()));
    }
    if let Some(value) = request_id.filter(|value| !value.is_empty()) {
        next.insert("request_id".to_string(), Value::String(value.to_string()));
        next.insert("thalovant_request_id".to_string(), Value::String(value.to_string()));
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
    if let (Some(expected_session), Some(event_session)) =
        (session_id_from_context(expected), event.session_id())
    {
        if expected_session != event_session {
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
    let data = payload.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
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
        .or_else(|| context.get("session_id").and_then(Value::as_str).map(str::to_string))
}

fn request_id_from_context(context: &Context) -> Option<String> {
    request_id_from_map(context).or_else(|| request_id_from_map(&session_from_context(context)))
}

fn session_from_context(context: &Context) -> Context {
    context.get("session").and_then(Value::as_object).cloned().unwrap_or_default()
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
        let context = context_with_correlation(None, Some("session-1"), Some("site"), Some("en-us"), Some("request-1"));
        let mut data = Data::new();
        data.insert("utterance".to_string(), Value::String("hi".to_string()));
        let event = Event::new("speak", data, context.clone(), None);
        assert_eq!(event.text(), "hi");
        assert_eq!(event.session_id().as_deref(), Some("session-1"));
        assert_eq!(event.request_id().as_deref(), Some("request-1"));
        assert!(event_matches_context(&event, Some(&context)));
    }
}
