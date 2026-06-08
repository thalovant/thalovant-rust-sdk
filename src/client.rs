use crate::{
    constants::{
        EVENT_INTENT_FAILURE, EVENT_POLICY_DENIED, EVENT_RECOGNIZER_LOOP_UTTERANCE, EVENT_SPEAK,
        EVENT_UTTERANCE_HANDLED,
    },
    errors::{Result, ThalovantError},
    events::{
        context_with_correlation, event_matches_context, merge_context, new_request_id,
        new_session_id, utterance_payload, Context, Data, Reply,
    },
    identity::Identity,
    protocols::HubProtocol,
    transport::{HttpTransport, TransportHealth},
};
use serde_json::{Map, Value};
use std::{path::Path, time::Duration};
use tokio::time::timeout;

#[derive(Clone)]
pub struct Client {
    pub identity: Identity,
    pub transport: HttpTransport,
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
            transport: HttpTransport::new(identity.clone()),
            identity,
        }
    }

    pub fn with_protocol(identity: Identity, protocol: HubProtocol) -> Result<Self> {
        if protocol != HubProtocol::Https {
            let detail = identity
                .endpoint_for(protocol)
                .map(|endpoint| format!(" at {endpoint}"))
                .unwrap_or_default();
            return Err(ThalovantError::UnsupportedProtocol(format!(
                "{protocol:?} is enabled{detail}, but this SDK runtime currently connects through the HTTPS HiveMind HTTP protocol transport"
            )));
        }
        Ok(Self::new(identity))
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(Identity::from_file(path)?))
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self::new(Identity::from_env()?))
    }

    pub async fn connect(&self) -> Result<()> {
        if self.healthcheck().await.connected {
            return Ok(());
        }
        self.transport.connect().await
    }

    pub async fn close(&self) -> Result<()> {
        self.transport.disconnect().await
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        self.transport.healthcheck().await
    }

    pub async fn emit(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.connect().await?;
        self.transport.emit_bus(event_type, data, context).await
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
                    EVENT_SPEAK => {
                        let text = event.text();
                        if !text.is_empty() {
                            fragments.push(text);
                        }
                    }
                    EVENT_INTENT_FAILURE | EVENT_POLICY_DENIED => failure_event = Some(event),
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

    pub fn conversation(&self, opts: ConversationOptions) -> Conversation {
        Conversation {
            client: self.clone(),
            session_id: opts.session_id.unwrap_or_else(new_session_id),
            lang: opts.lang.unwrap_or_else(|| "en-us".to_string()),
            context: opts.context.unwrap_or_default(),
        }
    }
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
