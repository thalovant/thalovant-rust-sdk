use crate::{
    constants::{
        EVENT_INTENT_FAILURE, EVENT_INTENT_UNMATCHED, EVENT_OVOS_UTTERANCE_SPEAK,
        EVENT_POLICY_DENIED, EVENT_QUERY_TIMEOUT, EVENT_RECOGNIZER_LOOP_UTTERANCE, EVENT_SPEAK,
        EVENT_UTTERANCE_HANDLED,
    },
    errors::{Result, ThalovantError},
    events::{
        context_with_correlation, merge_context, new_request_id, new_session_id, utterance_payload,
        Context, Data, Event, Reply,
    },
    identity::Identity,
    intents::{
        self, HubFallback, HubIntentCapabilities, HubIntentInventory, HubLink, IntentDefinition,
        IntentDescribeOptions, IntentInventoryOptions, IntentListOptions, IntentRegistration,
        DEFAULT_INTENT_TIMEOUT,
    },
    protocols::{HubProtocol, DEFAULT_PROTOCOL_PREFERENCE},
    transport::{HiveMessage, RuntimeTransport, TransportConnectionInfo, TransportHealth},
};
use serde_json::{Map, Value};
use std::{path::Path, time::Duration};
use tokio::{
    sync::broadcast,
    time::{timeout, timeout_at, Instant},
};

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

/// Reply collection controls without changing existing `RequestOptions` literals.
#[derive(Clone, Debug)]
pub struct AskOptions {
    pub request: RequestOptions,
    /// Wait for delayed speech after a handled event or a soft intent miss.
    pub empty_reply_wait: Duration,
    /// Collect adjacent speech fragments after the first answer.
    pub reply_settle: Duration,
}

impl Default for AskOptions {
    fn default() -> Self {
        Self {
            request: RequestOptions::default(),
            empty_reply_wait: Duration::from_secs(5),
            reply_settle: Duration::from_millis(250),
        }
    }
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
        self.transport.connect_with_timeout(timeout_duration).await
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

    /// Ask within one deadline covering connection, send, and reply collection.
    pub async fn ask(&self, text: &str, opts: RequestOptions) -> Result<Reply> {
        self.ask_with_options(
            text,
            AskOptions {
                request: opts,
                ..Default::default()
            },
        )
        .await
    }

    /// Configure bounded delayed-speech and fragment collection explicitly.
    pub async fn ask_with_options(&self, text: &str, options: AskOptions) -> Result<Reply> {
        self.ask_with_hints(text, options, crate::RequestContextOptions::default())
            .await
    }

    /// Apply request-level language, pipeline and location hints without mutating caller context.
    pub async fn ask_with_hints(
        &self,
        text: &str,
        mut options: AskOptions,
        hints: crate::RequestContextOptions,
    ) -> Result<Reply> {
        options.request.context = crate::request_context(options.request.context.as_ref(), &hints);
        let prompt = text.trim();
        if prompt.is_empty() {
            return Err(ThalovantError::Runtime(
                "ask requires non-empty text".into(),
            ));
        }
        let deadline = Instant::now()
            .checked_add(options.request.timeout.unwrap_or(Duration::from_secs(12)))
            .ok_or_else(|| ThalovantError::Runtime("ask timeout is too large".into()))?;
        let request_id = options
            .request
            .request_id
            .clone()
            .unwrap_or_else(new_request_id);
        let _reservation = self.transport.reserve_reply(false, &request_id)?;
        timeout_at(
            deadline,
            self.connect_with_timeout(deadline.saturating_duration_since(Instant::now())),
        )
        .await
        .map_err(|_| ThalovantError::Timeout("utterance handling timed out".into()))??;
        if Instant::now() >= deadline {
            return Err(ThalovantError::Timeout(
                "utterance handling timed out".into(),
            ));
        }
        let opts = &options.request;
        let lang = opts.lang.as_deref().unwrap_or("en-us");
        let context = context_with_correlation(
            opts.context.as_ref(),
            opts.session_id.as_deref(),
            Some(&self.identity.site_id),
            Some(lang),
            Some(&request_id),
        );
        let mut receiver = self.transport.subscribe();
        // The collector owns the remaining deadline so already received speech
        // is returned even when the transport write has not completed.
        send_and_collect(
            self.transport.emit_bus(
                EVENT_RECOGNIZER_LOOP_UTTERANCE,
                utterance_payload(prompt, lang),
                context.clone(),
            ),
            collect_ask_reply(&mut receiver, &context, &request_id, deadline, &options),
        )
        .await
    }

    pub async fn query(&self, text: &str, opts: QueryOptions) -> Result<Reply> {
        timeout(
            opts.timeout.unwrap_or(Duration::from_secs(12)),
            self.query_inner(text, opts),
        )
        .await
        .map_err(|_| ThalovantError::Timeout("query timed out".into()))?
    }

    async fn query_inner(&self, text: &str, opts: QueryOptions) -> Result<Reply> {
        let prompt = text.trim();
        if prompt.is_empty() {
            return Err(ThalovantError::Runtime(
                "query requires non-empty text".to_string(),
            ));
        }
        let lang = opts.lang.as_deref().unwrap_or("en-us");
        let timeout_duration = opts.timeout.unwrap_or(Duration::from_secs(12));
        let request_id = opts.request_id.unwrap_or_else(new_request_id);
        let query_id = opts.query_id.unwrap_or_else(|| request_id.clone());
        let _reservation = self.transport.reserve_reply(true, &query_id)?;
        self.connect().await?;
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
        send_and_collect(
            self.transport.send_hive_message(
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
            ),
            collect_query_reply(
                &mut receiver,
                &query_id,
                session_id,
                request_id,
                timeout_duration,
            ),
        )
        .await
    }

    pub fn conversation(&self, opts: ConversationOptions) -> Conversation {
        Conversation {
            client: self.clone(),
            session_id: opts.session_id.unwrap_or_else(new_session_id),
            lang: opts.lang.unwrap_or_else(|| "en-us".to_string()),
            context: opts.context.unwrap_or_default(),
        }
    }

    /// Everything the hub can be asked, per language, grouped by skill.
    ///
    /// Read from the runtime's intent manifest over this session, so no
    /// control-plane credential is involved. Each intent carries the sentences
    /// a person says to reach it, as the skill wrote them, `{slot}`
    /// placeholders included. `languages` defaults to `en-us` when empty.
    ///
    /// The preferred listing is `ovos.intent.list`; silence or denial may use engine manifests.
    /// `ovos.intent.describe` is needed only when the client has to ask for
    /// the definitions itself: `describe` is on (the default) *and* the
    /// runtime did not attach each row's `definition` to the listing. A
    /// runtime that honours `include_definitions` is never sent a describe,
    /// and `describe: false` never asks for the sentences at all.
    ///
    /// Fails with [`ThalovantError::PolicyDenied`] when the hub refuses the
    /// query and `fallback` is off; with it on (the default), a hub allowed for
    /// only the engines' manifests yields intent names with `source` set to
    /// [`IntentInventorySource::EngineManifests`](crate::IntentInventorySource::EngineManifests).
    /// A hub that answers the listing `ok: false` fails with
    /// [`ThalovantError::Runtime`] carrying the hub's wording: a query that
    /// failed is not an empty hub, and the fallback answers a refusal, not a
    /// failure. A silent listing also uses the fallback when enabled.
    pub async fn intents<I, S>(
        &self,
        languages: I,
        opts: IntentInventoryOptions,
    ) -> Result<HubIntentInventory>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.connect().await?;
        intents::inventory(self, languages, &opts).await
    }

    /// Inventory plus optional fallback handlers and conservative language availability.
    /// The fallback probe adds at most 1.5 seconds after the inventory query.
    pub async fn intents_with_capabilities<I, S>(
        &self,
        languages: I,
        opts: IntentInventoryOptions,
    ) -> Result<HubIntentCapabilities>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.connect().await?;
        intents::inventory_with_capabilities(self, languages, &opts).await
    }

    /// Registered fallback skills, or `None` when permission/support is unknown.
    pub async fn list_fallbacks(
        &self,
        timeout_duration: Option<Duration>,
    ) -> Result<Option<Vec<HubFallback>>> {
        let budget = timeout_duration
            .unwrap_or(intents::FALLBACK_PROBE_TIMEOUT)
            .min(intents::FALLBACK_PROBE_TIMEOUT);
        let deadline = Instant::now() + budget;
        match timeout_at(deadline, async {
            self.connect_with_timeout(budget).await?;
            intents::list_fallbacks(self, deadline.saturating_duration_since(Instant::now())).await
        })
        .await
        {
            Err(_) | Ok(Err(ThalovantError::Timeout(_))) => Ok(None),
            Ok(result) => result,
        }
    }

    /// The hub's intent manifest for one language, one row per registration.
    ///
    /// `lang` defaults to `en-us` when empty. A hub answering `ok: false`
    /// fails with [`ThalovantError::Runtime`] rather than reporting no intents.
    pub async fn list_intents(
        &self,
        lang: &str,
        opts: IntentListOptions,
    ) -> Result<Vec<IntentRegistration>> {
        self.connect().await?;
        intents::list_intents(self, lang_or_default(lang), &opts).await
    }

    /// The registrations behind one intent in one language, sentences included.
    ///
    /// Empty for a registration the hub does not know -- `ok: false` here is a
    /// real answer, not a failure. `lang` defaults to `en-us` when empty.
    pub async fn describe_intent(
        &self,
        skill_id: &str,
        intent_name: &str,
        lang: &str,
        opts: IntentDescribeOptions,
    ) -> Result<Vec<IntentDefinition>> {
        self.connect().await?;
        intents::describe_intent(
            self,
            skill_id,
            intent_name,
            lang_or_default(lang),
            opts.timeout.unwrap_or(DEFAULT_INTENT_TIMEOUT),
        )
        .await
    }
}

async fn collect_query_reply(
    receiver: &mut broadcast::Receiver<HiveMessage>,
    query_id: &str,
    session_id: String,
    request_id: String,
    timeout_duration: Duration,
) -> Result<Reply> {
    let mut events = Vec::new();
    let mut media_budget = crate::events::ReplyMediaBudget::default();
    let mut fragments = Vec::new();
    let mut failure_event = None;
    let mut soft_failure = None;
    timeout(timeout_duration, async {
        loop {
            let message = receiver
                .recv()
                .await
                .map_err(|err| ThalovantError::Runtime(err.to_string()))?;
            if query_id_from_hive_message(&message).as_deref() != Some(query_id) {
                continue;
            }
            let Some(event) = event_from_query_hive_message(&message) else {
                continue;
            };
            if !media_budget.accept(&event) {
                continue;
            }
            if event.name == "hive.query.complete" {
                events.push(event);
                break;
            }
            match event.name.as_str() {
                EVENT_SPEAK | EVENT_OVOS_UTTERANCE_SPEAK => {
                    push_fragment(&mut fragments, &event.text());
                }
                EVENT_INTENT_FAILURE | EVENT_INTENT_UNMATCHED => soft_failure = Some(event.clone()),
                EVENT_POLICY_DENIED | EVENT_QUERY_TIMEOUT => {
                    failure_event = Some(event.clone());
                }
                _ => {}
            }
            events.push(event);
            if failure_event.is_some() {
                break;
            }
        }
        Ok::<(), ThalovantError>(())
    })
    .await
    .map_err(|_| ThalovantError::Timeout("query timed out".to_string()))??;
    if fragments.is_empty() {
        failure_event = failure_event.or(soft_failure);
    }
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
        dropped_media: media_budget.dropped,
        text: fragments.join(" "),
        utterances: fragments,
        handled: failure_event.is_none(),
        ok: failure_event.is_none(),
        session_id: events
            .iter()
            .filter_map(Event::session_id)
            .find(|id| !id.trim().is_empty())
            .or(Some(session_id)),
        request_id: Some(request_id),
        events,
        failure_event,
    })
}

async fn send_and_collect(
    sending: impl std::future::Future<Output = Result<()>>,
    collecting: impl std::future::Future<Output = Result<Reply>>,
) -> Result<Reply> {
    tokio::pin!(sending, collecting);
    tokio::select! {
        // A ready terminal response wins over a subsequent write error.
        biased;
        result = &mut collecting => result,
        result = &mut sending => {
            result?;
            collecting.await
        }
    }
    // Dropping an unfinished write preserves the transport's cancellation guard:
    // uncertain encrypted delivery poisons only its captured Noise generation.
}

async fn collect_ask_reply(
    receiver: &mut broadcast::Receiver<Event>,
    context: &Context,
    request_id: &str,
    deadline: Instant,
    options: &AskOptions,
) -> Result<Reply> {
    let mut events = Vec::new();
    let mut media_budget = crate::events::ReplyMediaBudget::default();
    let mut fragments = Vec::new();
    let mut hard_failure = None;
    let mut soft_failure = None;
    let mut empty_deadline = None;
    let mut settle_deadline = None;
    loop {
        let wake = settle_deadline
            .or(empty_deadline)
            .unwrap_or(deadline)
            .min(deadline);
        if Instant::now() >= wake {
            break;
        }
        let event = match timeout_at(wake, receiver.recv()).await {
            Err(_) => break,
            Ok(Ok(event)) => event,
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                return Err(ThalovantError::Runtime(format!(
                    "reply event buffer overflow: skipped {skipped} events"
                )))
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Err(ThalovantError::Connection(
                    "hub session closed while collecting a reply".into(),
                ))
            }
        };
        // A runtime may replace the session ID. The request ID is required:
        // ambient or uncorrelated events must never satisfy a concurrent Ask.
        if event.request_id().as_deref() != Some(request_id) {
            continue;
        }
        if !media_budget.accept(&event) {
            continue;
        }
        match event.name.as_str() {
            EVENT_SPEAK | EVENT_OVOS_UTTERANCE_SPEAK => {
                push_fragment(&mut fragments, &event.text());
                if !fragments.is_empty() && settle_deadline.is_none() {
                    settle_deadline = Some(
                        Instant::now()
                            .checked_add(options.reply_settle)
                            .unwrap_or(deadline)
                            .min(deadline),
                    );
                }
            }
            EVENT_INTENT_FAILURE | EVENT_INTENT_UNMATCHED => {
                soft_failure = Some(event.clone());
                empty_deadline.get_or_insert_with(|| {
                    Instant::now()
                        .checked_add(options.empty_reply_wait)
                        .unwrap_or(deadline)
                        .min(deadline)
                });
            }
            EVENT_POLICY_DENIED | EVENT_QUERY_TIMEOUT => hard_failure = Some(event.clone()),
            EVENT_UTTERANCE_HANDLED => {
                empty_deadline.get_or_insert_with(|| {
                    Instant::now()
                        .checked_add(options.empty_reply_wait)
                        .unwrap_or(deadline)
                        .min(deadline)
                });
            }
            _ => {}
        }
        events.push(event);
        if hard_failure.is_some() {
            break;
        }
    }
    let failure_event =
        hard_failure.or_else(|| fragments.is_empty().then_some(soft_failure).flatten());
    if fragments.is_empty() {
        return Err(match failure_event {
            Some(event) => ThalovantError::Runtime(event.name),
            None => ThalovantError::Timeout("hub finished without a speak reply".into()),
        });
    }
    Ok(Reply {
        dropped_media: media_budget.dropped,
        text: fragments.join(" "),
        utterances: fragments,
        handled: failure_event.is_none(),
        ok: failure_event.is_none(),
        session_id: events
            .iter()
            .filter_map(Event::session_id)
            .find(|id| !id.trim().is_empty())
            .or_else(|| {
                context
                    .get("session")
                    .and_then(|value| value.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
        request_id: Some(request_id.to_string()),
        events,
        failure_event,
    })
}

fn lang_or_default(lang: &str) -> &str {
    let lang = lang.trim();
    if lang.is_empty() {
        intents::DEFAULT_LANG
    } else {
        lang
    }
}

impl HubLink for Client {
    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.transport.subscribe()
    }

    async fn emit_bus(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.transport.emit_bus(event_type, data, context).await
    }

    fn site_id(&self) -> Option<String> {
        Some(self.identity.site_id.clone()).filter(|value| !value.is_empty())
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
    fn reply_context(id: &str) -> Context {
        context_with_correlation(None, Some("shared-session"), None, None, Some(id))
    }

    #[tokio::test]
    async fn handled_and_soft_miss_wait_for_late_speech_without_losing_concurrent_replies() {
        let (tx, _) = broadcast::channel(32);
        let mut first = tx.subscribe();
        let mut second = tx.subscribe();
        let options = AskOptions {
            empty_reply_wait: Duration::from_millis(100),
            reply_settle: Duration::from_millis(20),
            ..Default::default()
        };
        let first_context = reply_context("first");
        let second_context = reply_context("second");
        let producer = tokio::spawn(async move {
            tx.send(Event::new(
                EVENT_SPEAK,
                json!({"utterance":"ambient"}).as_object().unwrap().clone(),
                Context::new(),
                None,
            ))
            .unwrap();
            for id in ["first", "second"] {
                for name in [EVENT_INTENT_UNMATCHED, EVENT_UTTERANCE_HANDLED] {
                    tx.send(Event::new(name, Data::new(), reply_context(id), None))
                        .unwrap();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            for id in ["second", "first"] {
                tx.send(Event::new(
                    EVENT_SPEAK,
                    json!({"utterance":id}).as_object().unwrap().clone(),
                    reply_context(id),
                    None,
                ))
                .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        let (a, b) = tokio::join!(
            collect_ask_reply(&mut first, &first_context, "first", deadline, &options),
            collect_ask_reply(&mut second, &second_context, "second", deadline, &options)
        );
        for (reply, expected) in [(a.unwrap(), "first"), (b.unwrap(), "second")] {
            assert_eq!(reply.text, expected);
            assert!(reply.ok);
            assert!(reply.failure_event.is_none());
            assert_eq!(reply.events.len(), 3);
        }
        producer.abort();
    }

    #[tokio::test]
    async fn empty_success_is_timeout_and_hard_failure_is_not_recovered_by_speech() {
        for hard in [false, true] {
            let (tx, mut receiver) = broadcast::channel(8);
            let context = reply_context("test");
            tx.send(Event::new(
                EVENT_UTTERANCE_HANDLED,
                Data::new(),
                context.clone(),
                None,
            ))
            .unwrap();
            if hard {
                tx.send(Event::new(
                    EVENT_SPEAK,
                    json!({"utterance":"partial"}).as_object().unwrap().clone(),
                    context.clone(),
                    None,
                ))
                .unwrap();
                tx.send(Event::new(
                    EVENT_POLICY_DENIED,
                    Data::new(),
                    context.clone(),
                    None,
                ))
                .unwrap();
            }
            let options = AskOptions {
                empty_reply_wait: Duration::from_secs(5),
                ..Default::default()
            };
            let result = collect_ask_reply(
                &mut receiver,
                &context,
                "test",
                Instant::now() + Duration::from_millis(20),
                &options,
            )
            .await;
            if hard {
                let reply = result.unwrap();
                assert!(!reply.ok);
                assert_eq!(reply.failure_event.unwrap().name, EVENT_POLICY_DENIED);
            } else {
                assert!(matches!(result, Err(ThalovantError::Timeout(_))));
            }
        }
    }
    fn fixture_query_event(name: &str, text: &str) -> HiveMessage {
        HiveMessage {
            msg_type: "cascade".into(),
            metadata: json!({"query_id":"fixture"}).as_object().unwrap().clone(),
            payload: json!({"type":name,"data":{"utterance":text},"context":{}})
                .as_object()
                .unwrap()
                .clone(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn query_reports_the_first_accepted_hub_session_or_requested_fallback() {
        for assigned in [None, Some("assigned-by-hub")] {
            for hard in [false, true] {
                let (tx, mut rx) = broadcast::channel(8);
                let mut foreign = fixture_query_event(EVENT_SPEAK, "foreign");
                foreign
                    .metadata
                    .insert("query_id".into(), "other-query".into());
                foreign.payload.insert(
                    "context".into(),
                    json!({"session":{"session_id":"foreign-session"}}),
                );
                tx.send(foreign).unwrap();
                let mut blank = fixture_query_event(EVENT_SPEAK, "first");
                blank
                    .payload
                    .insert("context".into(), json!({"session":{"session_id":"  "}}));
                tx.send(blank).unwrap();
                let mut accepted = fixture_query_event(EVENT_SPEAK, "second");
                if let Some(id) = assigned {
                    accepted
                        .payload
                        .insert("context".into(), json!({"session":{"session_id":id}}));
                }
                tx.send(accepted).unwrap();
                tx.send(fixture_query_event(
                    if hard {
                        EVENT_POLICY_DENIED
                    } else {
                        "hive.query.complete"
                    },
                    "",
                ))
                .unwrap();
                let reply = collect_query_reply(
                    &mut rx,
                    "fixture",
                    "requested".into(),
                    "request".into(),
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                assert_eq!(
                    reply.session_id.as_deref(),
                    Some(assigned.unwrap_or("requested"))
                );
                assert_eq!(reply.text, "first second");
                assert_eq!(reply.ok, !hard);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn query_hard_failure_freezes_partial_without_waiting_for_complete() {
        for hard in [EVENT_POLICY_DENIED, EVENT_QUERY_TIMEOUT] {
            for partial in [false, true] {
                let (tx, mut rx) = broadcast::channel(8);
                if partial {
                    tx.send(fixture_query_event(EVENT_SPEAK, "partial"))
                        .unwrap();
                }
                tx.send(fixture_query_event(hard, "denied")).unwrap();
                tx.send(fixture_query_event(EVENT_SPEAK, "too late"))
                    .unwrap();
                let result = timeout(
                    Duration::from_millis(100),
                    collect_query_reply(
                        &mut rx,
                        "fixture",
                        "session".into(),
                        "request".into(),
                        Duration::from_secs(10),
                    ),
                )
                .await
                .expect("hard failures must not wait for complete or timeout");
                if partial {
                    let reply = result.unwrap();
                    assert_eq!(reply.text, "partial");
                    assert!(!reply.ok);
                    assert_eq!(
                        reply
                            .events
                            .iter()
                            .map(|e| e.name.as_str())
                            .collect::<Vec<_>>(),
                        [EVENT_SPEAK, hard]
                    );
                } else {
                    assert!(matches!(result, Err(ThalovantError::Runtime(_))));
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ask_collects_partial_at_its_deadline_while_the_send_is_stalled() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct SendGuard(Arc<AtomicBool>);
        impl Drop for SendGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = broadcast::channel(4);
        let context = context_with_correlation(None, None, None, None, Some("request"));
        let sending = async {
            let _guard = SendGuard(dropped.clone());
            tx.send(Event::new(
                EVENT_SPEAK,
                json!({"utterance":"partial"}).as_object().unwrap().clone(),
                context.clone(),
                None,
            ))
            .unwrap();
            std::future::pending::<Result<()>>().await
        };
        let started = Instant::now();
        let options = AskOptions {
            reply_settle: Duration::from_secs(1),
            ..Default::default()
        };
        let reply = timeout(
            Duration::from_millis(200),
            send_and_collect(
                sending,
                collect_ask_reply(
                    &mut rx,
                    &context,
                    "request",
                    started + Duration::from_millis(100),
                    &options,
                ),
            ),
        )
        .await
        .expect("partial reply must complete independently of its pending write")
        .unwrap();
        assert_eq!(reply.text, "partial");
        assert!(started.elapsed() <= Duration::from_millis(101));
        assert!(
            dropped.load(Ordering::Acquire),
            "returning drops the owned send future and its cancellation guard"
        );
    }

    #[tokio::test]
    async fn ask_reports_broadcast_overflow_instead_of_silently_losing_events() {
        let (tx, mut rx) = broadcast::channel(2);
        let context = context_with_correlation(None, None, None, None, Some("request"));
        for text in ["lost", "second", "third"] {
            tx.send(Event::new(
                EVENT_SPEAK,
                json!({"utterance":text}).as_object().unwrap().clone(),
                context.clone(),
                None,
            ))
            .unwrap();
        }
        let result = collect_ask_reply(
            &mut rx,
            &context,
            "request",
            Instant::now() + Duration::from_millis(10),
            &AskOptions::default(),
        )
        .await;
        assert!(
            matches!(result, Err(ThalovantError::Runtime(ref message)) if message.contains("overflow")),
            "overflow must be explicit, got {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hard_reply_finishes_while_the_write_is_stalled_and_ignores_late_speech() {
        for hard in [EVENT_POLICY_DENIED, EVENT_QUERY_TIMEOUT] {
            let (tx, mut rx) = broadcast::channel(8);
            let context = reply_context("request");
            let sending = async {
                for (name, text) in [(EVENT_SPEAK, "partial"), (hard, ""), (EVENT_SPEAK, "late")] {
                    tx.send(Event::new(
                        name,
                        json!({"utterance":text}).as_object().unwrap().clone(),
                        context.clone(),
                        None,
                    ))
                    .unwrap();
                }
                std::future::pending::<Result<()>>().await
            };
            let reply = timeout(
                Duration::from_millis(1),
                send_and_collect(
                    sending,
                    collect_ask_reply(
                        &mut rx,
                        &context,
                        "request",
                        Instant::now() + Duration::from_secs(12),
                        &AskOptions::default(),
                    ),
                ),
            )
            .await
            .expect("hard response cannot wait for the write")
            .unwrap();
            assert_eq!(reply.text, "partial");
            assert_eq!(reply.events.len(), 2);
            assert!(!reply.ok);
            assert_eq!(reply.failure_event.unwrap().name, hard);

            let (tx, mut rx) = broadcast::channel(8);
            let sending = async {
                tx.send(fixture_query_event(EVENT_SPEAK, "partial"))
                    .unwrap();
                tx.send(fixture_query_event(hard, "")).unwrap();
                tx.send(fixture_query_event(EVENT_SPEAK, "late")).unwrap();
                std::future::pending::<Result<()>>().await
            };
            let reply = timeout(
                Duration::from_millis(1),
                send_and_collect(
                    sending,
                    collect_query_reply(
                        &mut rx,
                        "fixture",
                        "session".into(),
                        "request".into(),
                        Duration::from_secs(12),
                    ),
                ),
            )
            .await
            .expect("hard query response cannot wait for the write")
            .unwrap();
            assert_eq!(reply.text, "partial");
            assert_eq!(reply.events.len(), 2);
            assert!(!reply.ok);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_collection_drops_its_write_and_subscription() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = dropped.clone();
        let (tx, mut rx) = broadcast::channel(4);
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        let task = tokio::spawn(send_and_collect(
            async move {
                let _guard = Guard(guard);
                signal.notify_one();
                std::future::pending::<Result<()>>().await
            },
            async move {
                collect_ask_reply(
                    &mut rx,
                    &reply_context("request"),
                    "request",
                    Instant::now() + Duration::from_secs(12),
                    &AskOptions::default(),
                )
                .await
            },
        ));
        started.notified().await;
        assert_eq!(tx.receiver_count(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(tx.receiver_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ask_first_empty_and_first_speech_windows_never_restart() {
        for speak in [false, true] {
            let (tx, mut rx) = broadcast::channel(8);
            let producer = tokio::spawn(async move {
                let name = if speak {
                    EVENT_SPEAK
                } else {
                    EVENT_UTTERANCE_HANDLED
                };
                tx.send(Event::new(
                    name,
                    json!({"utterance":"first"}).as_object().unwrap().clone(),
                    reply_context("request"),
                    None,
                ))
                .unwrap();
                tokio::time::sleep(Duration::from_millis(80)).await;
                tx.send(Event::new(
                    name,
                    json!({"utterance":"second"}).as_object().unwrap().clone(),
                    reply_context("request"),
                    None,
                ))
                .unwrap();
                std::future::pending::<()>().await;
            });
            let start = Instant::now();
            let result = collect_ask_reply(
                &mut rx,
                &reply_context("request"),
                "request",
                start + Duration::from_secs(12),
                &AskOptions {
                    reply_settle: Duration::from_millis(100),
                    empty_reply_wait: Duration::from_millis(100),
                    ..Default::default()
                },
            )
            .await;
            assert!(
                start.elapsed() <= Duration::from_millis(101),
                "later events must not restart the first-event window"
            );
            if speak {
                assert_eq!(result.unwrap().text, "first second");
            } else {
                assert!(matches!(result, Err(ThalovantError::Timeout(_))));
            }
            producer.abort();
        }
    }

    #[tokio::test]
    async fn ask_reports_the_hub_session_from_correlated_events() {
        for requested in [None, Some("caller-session")] {
            let context = context_with_correlation(None, requested, None, None, Some("request"));
            let hub_context = context_with_correlation(
                None,
                Some("assigned-by-hub"),
                None,
                None,
                Some("request"),
            );
            let (tx, mut receiver) = broadcast::channel(4);
            tx.send(Event::new(
                EVENT_SPEAK,
                json!({"utterance":"reply"}).as_object().unwrap().clone(),
                hub_context,
                None,
            ))
            .unwrap();
            let reply = collect_ask_reply(
                &mut receiver,
                &context,
                "request",
                Instant::now() + Duration::from_secs(1),
                &AskOptions {
                    reply_settle: Duration::ZERO,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(reply.session_id.as_deref(), Some("assigned-by-hub"));
        }
    }
}
