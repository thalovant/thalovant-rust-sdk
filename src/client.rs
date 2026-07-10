use crate::{
    constants::{
        EVENT_INTENT_FAILURE, EVENT_OVOS_UTTERANCE_SPEAK, EVENT_POLICY_DENIED, EVENT_QUERY_TIMEOUT,
        EVENT_RECOGNIZER_LOOP_UTTERANCE, EVENT_SPEAK, EVENT_UTTERANCE_HANDLED,
    },
    errors::{Result, ThalovantError},
    events::{
        context_with_correlation, event_matches_context, merge_context, new_request_id,
        new_session_id, utterance_payload, Context, Data, Event, Reply,
    },
    identity::Identity,
    protocols::{HubProtocol, DEFAULT_PROTOCOL_PREFERENCE},
    transport::{HiveMessage, RuntimeTransport, TransportConnectionInfo, TransportHealth},
};
use serde_json::{Map, Value};
use std::{path::Path, time::Duration};
use tokio::time::timeout;

#[derive(Clone)]
pub struct Client {
    pub identity: Identity,
    pub transport: RuntimeTransport,
}

#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    pub lang: Option<String>,
    pub context: Option<Context>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    pub timeout: Option<Duration>,
    pub lang: Option<String>,
    pub context: Option<Context>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub query_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ActionOptions {
    pub title: Option<String>,
    pub lang: Option<String>,
    pub context: Option<Context>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct CodeOptions {
    pub kind: Option<String>,
    pub label: Option<String>,
    pub lang: Option<String>,
    pub context: Option<Context>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ConversationOptions {
    pub session_id: Option<String>,
    pub lang: Option<String>,
    pub context: Option<Context>,
}

#[derive(Clone)]
pub struct Conversation {
    client: Client,
    pub session_id: String,
    pub lang: String,
    pub context: Context,
}

impl Client {
    pub fn new(identity: Identity) -> Self {
        Self {
            transport: RuntimeTransport::Http(crate::transport::HttpTransport::new(
                identity.clone(),
            )),
            identity,
        }
    }

    pub fn with_protocol(identity: Identity, protocol: HubProtocol) -> Result<Self> {
        let transport = RuntimeTransport::for_protocol(identity.clone(), protocol)?;
        Ok(Self {
            identity,
            transport,
        })
    }

    pub fn auto(identity: Identity) -> Result<Self> {
        Self::with_protocol(identity.clone(), default_runtime_protocol(&identity)?)
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::auto(Identity::from_file(path)?)
    }

    pub fn from_config(profile: Option<&str>) -> Result<Self> {
        Self::auto(Identity::from_config(profile)?)
    }

    pub fn from_config_file(path: impl AsRef<Path>, profile: Option<&str>) -> Result<Self> {
        Self::auto(Identity::from_config_file(path, profile)?)
    }

    pub fn from_env() -> Result<Self> {
        Self::auto(Identity::from_env()?)
    }

    pub async fn connect(&self) -> Result<()> {
        self.connect_with_timeout(Duration::from_secs(6)).await
    }

    pub async fn connect_with_timeout(&self, timeout_duration: Duration) -> Result<()> {
        if self.healthcheck().await.connected {
            return Ok(());
        }
        match timeout(timeout_duration, self.transport.connect()).await {
            Ok(result) => result,
            Err(_) => {
                let _ = self.transport.disconnect().await;
                Err(ThalovantError::Timeout(format!(
                    "hub connection did not complete within {}ms",
                    timeout_duration.as_millis()
                )))
            }
        }
    }

    pub async fn connect_with_info(&self) -> Result<TransportConnectionInfo> {
        self.connect().await?;
        Ok(self.connection_info().await)
    }

    pub async fn connection_info(&self) -> TransportConnectionInfo {
        self.transport.connection_info().await
    }

    pub async fn close(&self) -> Result<()> {
        self.transport.disconnect().await
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        self.transport.healthcheck().await
    }

    pub async fn emit(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.connect().await?;
        self.transport
            .emit_bus(
                event_type,
                data,
                self.context_with_identity_metadata(context),
            )
            .await
    }

    fn context_with_identity_metadata(&self, context: Context) -> Context {
        if self.identity.metadata.is_empty() {
            return context;
        }
        let mut next = context;
        let mut metadata = next
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (key, value) in &self.identity.metadata {
            metadata.entry(key.clone()).or_insert_with(|| value.clone());
        }
        next.insert("metadata".to_string(), Value::Object(metadata));
        next
    }

    pub async fn send_utterance(&self, text: &str, opts: RequestOptions) -> Result<()> {
        let prompt = text.trim();
        if prompt.is_empty() {
            return Err(ThalovantError::Runtime(
                "send_utterance requires non-empty text".to_string(),
            ));
        }
        let lang = opts.lang.as_deref().unwrap_or("en-us");
        let request_id = opts.request_id.unwrap_or_else(new_request_id);
        let context = context_with_correlation(
            opts.context.as_ref(),
            opts.session_id.as_deref(),
            Some(&self.identity.site_id),
            Some(lang),
            Some(&request_id),
        );
        self.emit(
            EVENT_RECOGNIZER_LOOP_UTTERANCE,
            utterance_payload(prompt, lang),
            context,
        )
        .await
    }

    pub async fn send_action(&self, payload: &str, opts: ActionOptions) -> Result<()> {
        let prompt = payload.trim();
        if prompt.is_empty() {
            return Err(ThalovantError::Runtime(
                "send_action requires non-empty payload".to_string(),
            ));
        }
        let mut input = Map::new();
        input.insert("kind".to_string(), Value::String("action".to_string()));
        if let Some(title) = opts.title {
            input.insert("title".to_string(), Value::String(title));
        }
        input.insert("payload".to_string(), Value::String(prompt.to_string()));
        let mut extra = Context::new();
        extra.insert("input".to_string(), Value::Object(input));
        self.send_utterance(
            prompt,
            RequestOptions {
                lang: opts.lang,
                context: Some(merge_context(opts.context.as_ref(), Some(&extra))),
                session_id: opts.session_id,
                request_id: opts.request_id,
                timeout: None,
            },
        )
        .await
    }

    pub async fn send_code(&self, value: &str, opts: CodeOptions) -> Result<()> {
        let code = value.trim();
        if code.is_empty() {
            return Err(ThalovantError::Runtime(
                "send_code requires non-empty value".to_string(),
            ));
        }
        let lang = opts.lang.as_deref().unwrap_or("en-us");
        let request_id = opts.request_id.unwrap_or_else(new_request_id);
        let mut input = Map::new();
        input.insert(
            "kind".to_string(),
            Value::String(opts.kind.unwrap_or_else(|| "code".to_string())),
        );
        if let Some(label) = opts.label {
            input.insert("label".to_string(), Value::String(label));
        }
        input.insert("value".to_string(), Value::String(code.to_string()));
        input.insert("exact".to_string(), Value::Bool(true));
        let mut extra = Context::new();
        extra.insert("input".to_string(), Value::Object(input.clone()));
        let context = context_with_correlation(
            Some(&merge_context(opts.context.as_ref(), Some(&extra))),
            opts.session_id.as_deref(),
            Some(&self.identity.site_id),
            Some(lang),
            Some(&request_id),
        );
        let mut data = utterance_payload(code, lang);
        data.insert("input".to_string(), Value::Object(input));
        self.emit(EVENT_RECOGNIZER_LOOP_UTTERANCE, data, context)
            .await
    }

    pub async fn ask(&self, text: &str, opts: RequestOptions) -> Result<Reply> {
        let prompt = text.trim();
        if prompt.is_empty() {
            return Err(ThalovantError::Runtime(
                "ask requires non-empty text".to_string(),
            ));
        }
        self.connect().await?;
        let lang = opts.lang.as_deref().unwrap_or("en-us");
        let timeout_duration = opts.timeout.unwrap_or(Duration::from_secs(12));
        let request_id = opts.request_id.unwrap_or_else(new_request_id);
        let context = context_with_correlation(
            opts.context.as_ref(),
            opts.session_id.as_deref(),
            Some(&self.identity.site_id),
            Some(lang),
            Some(&request_id),
        );
        let mut receiver = self.transport.subscribe();
        self.transport
            .emit_bus(
                EVENT_RECOGNIZER_LOOP_UTTERANCE,
                utterance_payload(prompt, lang),
                context.clone(),
            )
            .await?;
        let mut events = Vec::new();
        let mut fragments = Vec::new();
        let mut failure_event = None;
        timeout(timeout_duration, async {
            loop {
                let event = receiver
                    .recv()
                    .await
                    .map_err(|err| ThalovantError::Runtime(err.to_string()))?;
                if !event_matches_context(&event, Some(&context)) {
                    continue;
                }
                events.push(event.clone());
                match event.name.as_str() {
                    EVENT_SPEAK | EVENT_OVOS_UTTERANCE_SPEAK => {
                        let text = event.text();
                        if !text.is_empty() {
                            fragments.push(text);
                        }
                    }
                    EVENT_INTENT_FAILURE | EVENT_POLICY_DENIED | EVENT_QUERY_TIMEOUT => {
                        failure_event = Some(event)
                    }
                    EVENT_UTTERANCE_HANDLED => break,
                    _ => {}
                }
            }
            Ok::<(), ThalovantError>(())
        })
        .await
        .map_err(|_| ThalovantError::Timeout("utterance handling timed out".to_string()))??;
        if failure_event.is_some() && fragments.is_empty() {
            return Err(ThalovantError::Runtime(
                failure_event
                    .as_ref()
                    .map(|event| event.name.clone())
                    .unwrap_or_default(),
            ));
        }
        Ok(Reply {
            text: fragments.join(" "),
            utterances: fragments,
            handled: failure_event.is_none(),
            ok: failure_event.is_none(),
            session_id: context
                .get("session")
                .and_then(|value| value.get("session_id"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            request_id: Some(request_id),
            events,
            failure_event,
        })
    }

    pub async fn query(&self, text: &str, opts: QueryOptions) -> Result<Reply> {
        let prompt = text.trim();
        if prompt.is_empty() {
            return Err(ThalovantError::Runtime(
                "query requires non-empty text".to_string(),
            ));
        }
        self.connect().await?;
        let lang = opts.lang.as_deref().unwrap_or("en-us");
        let timeout_duration = opts.timeout.unwrap_or(Duration::from_secs(12));
        let request_id = opts.request_id.unwrap_or_else(new_request_id);
        let query_id = opts.query_id.unwrap_or_else(|| request_id.clone());
        let session_id = opts.session_id.unwrap_or_else(new_session_id);
        let context = context_with_correlation(
            opts.context.as_ref(),
            Some(&session_id),
            Some(&self.identity.site_id),
            Some(lang),
            Some(&request_id),
        );
        let mut receiver = self.transport.subscribe_hive();
        let inner = HiveMessage {
            msg_type: "bus".to_string(),
            payload: Map::from_iter([
                (
                    "type".to_string(),
                    Value::String(EVENT_RECOGNIZER_LOOP_UTTERANCE.to_string()),
                ),
                (
                    "data".to_string(),
                    Value::Object(utterance_payload(prompt, lang)),
                ),
                ("context".to_string(), Value::Object(context.clone())),
            ]),
            metadata: Map::new(),
            route: vec![],
            node: None,
            target_site_id: None,
            target_pubkey: None,
            source_peer: None,
        };
        self.transport
            .send_hive_message(
                HiveMessage {
                    msg_type: "query".to_string(),
                    payload: hive_message_payload(&inner)?,
                    metadata: Map::from_iter([(
                        "query_id".to_string(),
                        Value::String(query_id.clone()),
                    )]),
                    route: vec![],
                    node: None,
                    target_site_id: None,
                    target_pubkey: None,
                    source_peer: None,
                },
                true,
            )
            .await?;
        let mut events = Vec::new();
        let mut fragments = Vec::new();
        let mut failure_event = None;
        timeout(timeout_duration, async {
            loop {
                let message = receiver
                    .recv()
                    .await
                    .map_err(|err| ThalovantError::Runtime(err.to_string()))?;
                if query_id_from_hive_message(&message).as_deref() != Some(&query_id) {
                    continue;
                }
                let Some(event) = event_from_query_hive_message(&message) else {
                    continue;
                };
                if event.name == "hive.query.complete" {
                    events.push(event);
                    break;
                }
                match event.name.as_str() {
                    EVENT_SPEAK | EVENT_OVOS_UTTERANCE_SPEAK => {
                        push_fragment(&mut fragments, &event.text());
                    }
                    EVENT_INTENT_FAILURE | EVENT_POLICY_DENIED | EVENT_QUERY_TIMEOUT => {
                        failure_event = Some(event.clone());
                    }
                    _ => {}
                }
                events.push(event);
            }
            Ok::<(), ThalovantError>(())
        })
        .await
        .map_err(|_| ThalovantError::Timeout("query timed out".to_string()))??;
        if failure_event.is_some() && fragments.is_empty() {
            return Err(ThalovantError::Runtime(
                failure_event
                    .as_ref()
                    .map(|event| event.name.clone())
                    .unwrap_or_default(),
            ));
        }
        if fragments.is_empty() {
            return Err(ThalovantError::Timeout(
                "hub finished the query without a speak reply".to_string(),
            ));
        }
        Ok(Reply {
            text: fragments.join(" "),
            utterances: fragments,
            handled: failure_event.is_none(),
            ok: failure_event.is_none(),
            session_id: Some(session_id),
            request_id: Some(request_id),
            events,
            failure_event,
        })
    }

    pub fn conversation(&self, opts: ConversationOptions) -> Conversation {
        Conversation {
            client: self.clone(),
            session_id: opts.session_id.unwrap_or_else(new_session_id),
            lang: opts.lang.unwrap_or_else(|| "en-us".to_string()),
            context: opts.context.unwrap_or_default(),
        }
    }
}

fn default_runtime_protocol(identity: &Identity) -> Result<HubProtocol> {
    for protocol in DEFAULT_PROTOCOL_PREFERENCE {
        match protocol {
            HubProtocol::Wss => {
                if identity.supports_protocol(HubProtocol::Wss)
                    && identity.endpoint_for(HubProtocol::Wss).is_some()
                {
                    return Ok(HubProtocol::Wss);
                }
            }
            HubProtocol::Https => {
                if identity.supports_protocol(HubProtocol::Https)
                    || identity.endpoint_for(HubProtocol::Https).is_some()
                {
                    return Ok(HubProtocol::Https);
                }
            }
            HubProtocol::Mqtt => {
                if identity.supports_protocol(HubProtocol::Mqtt) && identity.mqtt.is_some() {
                    return Ok(HubProtocol::Mqtt);
                }
            }
        }
    }
    Err(ThalovantError::UnsupportedProtocol(
        "identity does not include a usable WSS, HTTPS, or MQTT endpoint".to_string(),
    ))
}

impl Conversation {
    pub async fn ask(&self, text: &str, mut opts: RequestOptions) -> Result<Reply> {
        opts.session_id = Some(self.session_id.clone());
        if opts.lang.is_none() {
            opts.lang = Some(self.lang.clone());
        }
        opts.context = Some(merge_context(Some(&self.context), opts.context.as_ref()));
        self.client.ask(text, opts).await
    }

    pub async fn query(&self, text: &str, mut opts: QueryOptions) -> Result<Reply> {
        opts.session_id = Some(self.session_id.clone());
        if opts.lang.is_none() {
            opts.lang = Some(self.lang.clone());
        }
        opts.context = Some(merge_context(Some(&self.context), opts.context.as_ref()));
        self.client.query(text, opts).await
    }

    pub async fn send_utterance(&self, text: &str, mut opts: RequestOptions) -> Result<()> {
        opts.session_id = Some(self.session_id.clone());
        if opts.lang.is_none() {
            opts.lang = Some(self.lang.clone());
        }
        opts.context = Some(merge_context(Some(&self.context), opts.context.as_ref()));
        self.client.send_utterance(text, opts).await
    }

    pub async fn send_action(&self, payload: &str, mut opts: ActionOptions) -> Result<()> {
        opts.session_id = Some(self.session_id.clone());
        if opts.lang.is_none() {
            opts.lang = Some(self.lang.clone());
        }
        opts.context = Some(merge_context(Some(&self.context), opts.context.as_ref()));
        self.client.send_action(payload, opts).await
    }

    pub async fn send_code(&self, value: &str, mut opts: CodeOptions) -> Result<()> {
        opts.session_id = Some(self.session_id.clone());
        if opts.lang.is_none() {
            opts.lang = Some(self.lang.clone());
        }
        opts.context = Some(merge_context(Some(&self.context), opts.context.as_ref()));
        self.client.send_code(value, opts).await
    }
}

fn hive_message_payload(message: &HiveMessage) -> Result<Map<String, Value>> {
    match serde_json::to_value(message)? {
        Value::Object(object) => Ok(object),
        _ => Ok(Map::new()),
    }
}

fn query_id_from_hive_message(message: &HiveMessage) -> Option<String> {
    message
        .metadata
        .get("query_id")
        .or_else(|| message.metadata.get("queryId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn event_from_query_hive_message(message: &HiveMessage) -> Option<Event> {
    let (event_type, data, context) = bus_payload_from_hive_payload(&message.payload)?;
    Some(Event::new(
        event_type,
        data,
        context,
        serde_json::to_value(message).ok(),
    ))
}

fn bus_payload_from_hive_payload(payload: &Map<String, Value>) -> Option<(String, Data, Context)> {
    if let Some(event_type) = payload.get("type").and_then(Value::as_str) {
        return Some((
            event_type.to_string(),
            object_value(payload.get("data")),
            object_value(payload.get("context")),
        ));
    }
    payload
        .get("payload")
        .and_then(Value::as_object)
        .and_then(bus_payload_from_hive_payload)
}

fn object_value(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn push_fragment(fragments: &mut Vec<String>, text: &str) {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return;
    }
    if fragments.last() != Some(&normalized) {
        fragments.push(normalized);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_message_extracts_bus_event() {
        let message = HiveMessage {
            msg_type: "query".to_string(),
            payload: Map::from_iter([
                ("msg_type".to_string(), Value::String("bus".to_string())),
                (
                    "payload".to_string(),
                    json!({
                        "type": EVENT_SPEAK,
                        "data": {"utterance": "direct answer"},
                        "context": {
                            "session": {
                                "session_id": "session-1",
                                "request_id": "request-1"
                            },
                            "request_id": "request-1"
                        }
                    }),
                ),
            ]),
            metadata: Map::from_iter([(
                "query_id".to_string(),
                Value::String("query-1".to_string()),
            )]),
            route: vec![],
            node: None,
            target_site_id: None,
            target_pubkey: None,
            source_peer: None,
        };

        let event = event_from_query_hive_message(&message).expect("query event");

        assert_eq!(
            query_id_from_hive_message(&message).as_deref(),
            Some("query-1")
        );
        assert_eq!(event.name, EVENT_SPEAK);
        assert_eq!(event.text(), "direct answer");
        assert_eq!(event.session_id().as_deref(), Some("session-1"));
        assert_eq!(event.request_id().as_deref(), Some("request-1"));
    }
}
