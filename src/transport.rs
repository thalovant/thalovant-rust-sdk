use crate::{
    constants::DEFAULT_USER_AGENT,
    errors::{Result, ThalovantError},
    events::{event_from_bus_payload, Data, Event},
    identity::{Identity, MqttBrokerCredentials},
    noise::{
        build_prologue, canonical_json, derive_psk, noise_protocol_name, select_noise_options,
        NoiseFrame, NoiseHandshake, NoiseSession,
    },
    noise_store::{
        forget_cached_psk, load_cached_psk, load_noise_pin, load_or_create_noise_key, pin_hub_key,
        save_cached_psk,
    },
    protocols::HubProtocol,
    tls::ensure_rustls_provider,
    wire::decode_hive_binary_frame,
};
use base64::{engine::general_purpose, Engine as _};
use futures_util::{SinkExt, StreamExt};
use rumqttc::{
    AsyncClient, Event as MqttEvent, LastWill, MqttOptions, Packet, QoS, TlsConfiguration,
    Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tokio::{
    net::TcpStream,
    sync::{broadcast, Mutex, Notify},
    task::JoinHandle,
    time::{sleep, timeout, timeout_at, Instant},
};
use tokio_tungstenite::{
    connect_async, tungstenite::Message as WebSocketMessage, MaybeTlsStream, WebSocketStream,
};
use url::Url;

#[derive(Clone, Debug, Default)]
pub struct TransportHealth {
    pub connected: bool,
    pub handshake_complete: bool,
    pub transport_alive: bool,
    pub last_error: Option<String>,
    pub connection: TransportConnectionInfo,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TransportConnectionPhase {
    #[default]
    Idle,
    Connecting,
    Handshake,
    Ready,
    Closed,
    Error,
}

#[derive(Clone, Debug)]
pub struct TransportConnectionInfo {
    pub phase: TransportConnectionPhase,
    pub started_at: Option<SystemTime>,
    pub connected_at: Option<SystemTime>,
    pub transport_open_ms: Option<f64>,
    pub socket_open_ms: Option<f64>,
    pub handshake_ms: Option<f64>,
    pub connect_ms: Option<f64>,
    pub last_error: Option<String>,
}

impl Default for TransportConnectionInfo {
    fn default() -> Self {
        Self {
            phase: TransportConnectionPhase::Idle,
            started_at: None,
            connected_at: None,
            transport_open_ms: None,
            socket_open_ms: None,
            handshake_ms: None,
            connect_ms: None,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HiveMessage {
    pub msg_type: String,
    #[serde(default)]
    pub payload: Map<String, Value>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    #[serde(default)]
    pub route: Vec<Value>,
    #[serde(default)]
    pub node: Option<Value>,
    #[serde(default)]
    pub target_site_id: Option<Value>,
    #[serde(default)]
    pub target_pubkey: Option<Value>,
    #[serde(default)]
    pub source_peer: Option<Value>,
}

#[derive(Clone)]
pub enum RuntimeTransport {
    Http(HttpTransport),
    Wss(WssTransport),
    Mqtt(MqttTransport),
}

#[derive(Default)]
struct ConnectionControl {
    gate: Mutex<()>,
    generation: std::sync::atomic::AtomicU64,
    cancelled: AtomicBool,
    active_attempt: AtomicBool,
    retired: Notify,
}

impl ConnectionControl {
    async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }
}

/// A dropped initiating connection retires only its own generation. Callers
/// waiting to join do not own this guard and cannot cancel another attempt.
struct ConnectionAttempt {
    transport: RuntimeTransport,
    generation: u64,
    complete: bool,
}

impl Drop for ConnectionAttempt {
    fn drop(&mut self) {
        self.transport
            .control()
            .active_attempt
            .store(false, Ordering::Release);
        if self.complete {
            return;
        }
        let transport = self.transport.clone();
        let generation = self.generation;
        transport.control().cancelled.store(true, Ordering::Release);
        if let RuntimeTransport::Wss(wss) = &transport {
            wss.state.session_valid.store(false, Ordering::Release);
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let control = transport.control();
                let _guard = control.lock().await;
                if control.generation.load(Ordering::Acquire) != generation {
                    return;
                }
                // Remote cleanup is owned but cannot extend the failed caller's
                // deadline. HTTP retains unacknowledged admission for retry.
                let _ = timeout(Duration::from_secs(2), transport.disconnect_locked()).await;
            });
        }
    }
}

impl RuntimeTransport {
    pub fn for_protocol(identity: Identity, protocol: HubProtocol) -> Result<Self> {
        match protocol {
            HubProtocol::Https => Ok(Self::Http(HttpTransport::new(identity))),
            HubProtocol::Wss => {
                if identity.endpoint_for(HubProtocol::Wss).is_none() {
                    return Err(ThalovantError::UnsupportedProtocol(
                        "identity does not include a WSS endpoint".to_string(),
                    ));
                }
                Ok(Self::Wss(WssTransport::new(identity)))
            }
            HubProtocol::Mqtt => Ok(Self::Mqtt(MqttTransport::new(identity)?)),
        }
    }

    pub fn identity(&self) -> &Identity {
        match self {
            Self::Http(transport) => transport.identity(),
            Self::Wss(transport) => transport.identity(),
            Self::Mqtt(transport) => transport.identity(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        match self {
            Self::Http(transport) => transport.subscribe(),
            Self::Wss(transport) => transport.subscribe(),
            Self::Mqtt(transport) => transport.subscribe(),
        }
    }

    pub fn subscribe_hive(&self) -> broadcast::Receiver<HiveMessage> {
        match self {
            Self::Http(transport) => transport.subscribe_hive(),
            Self::Wss(transport) => transport.subscribe_hive(),
            Self::Mqtt(transport) => transport.subscribe_hive(),
        }
    }

    pub async fn connect(&self) -> Result<()> {
        self.connect_with_timeout(Duration::from_secs(20)).await
    }

    fn control(&self) -> &ConnectionControl {
        match self {
            Self::Http(transport) => &transport.state.lifecycle,
            Self::Wss(transport) => &transport.state.lifecycle,
            Self::Mqtt(transport) => &transport.state.lifecycle,
        }
    }

    /// Join an authenticated connection within one deadline. Only the caller
    /// that acquires an unfinished attempt owns its cancellation and cleanup.
    pub async fn connect_with_timeout(&self, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        let control = self.control();
        let (guard, joining) = match control.gate.try_lock() {
            Ok(guard) => (guard, false),
            Err(_) => {
                let joining = control.active_attempt.load(Ordering::Acquire);
                (
                    timeout_at(deadline, control.lock())
                        .await
                        .map_err(|_| connection_timeout())?,
                    joining,
                )
            }
        };
        let _guard = guard;
        let health = timeout_at(deadline, self.healthcheck())
            .await
            .map_err(|_| connection_timeout())?;
        if health.connected
            && health.handshake_complete
            && !control.cancelled.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if joining {
            return Err(ThalovantError::Connection(
                health.last_error.unwrap_or_else(|| {
                    "the shared connection attempt ended before authenticated readiness".into()
                }),
            ));
        }
        let generation = control.generation.fetch_add(1, Ordering::AcqRel) + 1;
        control.cancelled.store(false, Ordering::Release);
        control.active_attempt.store(true, Ordering::Release);
        let mut attempt = ConnectionAttempt {
            transport: self.clone(),
            generation,
            complete: false,
        };
        let retired = control.retired.notified();
        tokio::pin!(retired);
        retired.as_mut().enable();
        let result = timeout_at(deadline, async {
            if control.cancelled.load(Ordering::Acquire) {
                return Err(ThalovantError::Connection("connection was closed during authentication".into()));
            }
            tokio::select! {
            biased;
            _ = &mut retired => Err(ThalovantError::Connection("connection was closed during authentication".into())),
            result = async { match self {
                Self::Http(transport) => transport.connect_locked().await,
                Self::Wss(transport) => transport.connect_locked().await,
                Self::Mqtt(transport) => transport.connect_locked().await,
            } } => result,
            }
        })
        .await
        .unwrap_or_else(|_| Err(connection_timeout()));
        attempt.complete = result.is_ok();
        result
    }

    async fn disconnect_locked(&self) -> Result<()> {
        match self {
            Self::Http(transport) => transport.disconnect_inner().await,
            Self::Wss(transport) => transport.disconnect_inner().await,
            Self::Mqtt(transport) => transport.disconnect_inner().await,
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.control().cancelled.store(true, Ordering::Release);
        self.control().retired.notify_waiters();
        if let Self::Wss(wss) = self {
            wss.state.session_valid.store(false, Ordering::Release);
        }
        timeout(Duration::from_secs(2), async {
            let _guard = self.control().lock().await;
            self.disconnect_locked().await
        })
        .await
        .map_err(|_| {
            ThalovantError::Timeout(
                "transport cleanup timed out; reconnect retries owned cleanup".into(),
            )
        })?
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        match self {
            Self::Http(transport) => transport.healthcheck().await,
            Self::Wss(transport) => transport.healthcheck().await,
            Self::Mqtt(transport) => transport.healthcheck().await,
        }
    }

    pub async fn connection_info(&self) -> TransportConnectionInfo {
        self.healthcheck().await.connection
    }

    pub async fn emit_bus(
        &self,
        event_type: &str,
        data: Data,
        context: Map<String, Value>,
    ) -> Result<()> {
        match self {
            Self::Http(transport) => transport.emit_bus(event_type, data, context).await,
            Self::Wss(transport) => transport.emit_bus(event_type, data, context).await,
            Self::Mqtt(transport) => transport.emit_bus(event_type, data, context).await,
        }
    }

    pub async fn send_hive_message(&self, message: HiveMessage, encrypt: bool) -> Result<()> {
        match self {
            Self::Http(transport) => transport.send_hive_message(message, encrypt).await,
            Self::Wss(transport) => transport.send_hive_message(message, encrypt).await,
            Self::Mqtt(transport) => transport.send_hive_message(message, encrypt).await,
        }
    }
}

fn connection_timeout() -> ThalovantError {
    ThalovantError::Timeout("hub connection did not complete before its deadline".into())
}

#[derive(Clone)]
pub struct HttpTransport {
    state: Arc<HttpTransportState>,
}

struct HttpTransportState {
    identity: Identity,
    user_agent: String,
    poll_interval: Duration,
    http_client: reqwest::Client,
    bus_tx: broadcast::Sender<Event>,
    hive_tx: broadcast::Sender<HiveMessage>,
    health: Mutex<TransportHealth>,
    poll_task: Mutex<Option<JoinHandle<()>>>,
    admitted: AtomicBool,
    noise: Mutex<Option<NoiseChannel>>,
    noise_state_dir: Mutex<Option<PathBuf>>,
    lifecycle: ConnectionControl,
    poll: Mutex<()>,
}

impl HttpTransport {
    pub fn new(identity: Identity) -> Self {
        Self::with_options(identity, DEFAULT_USER_AGENT, Duration::from_secs(1))
    }

    pub fn with_options(
        identity: Identity,
        user_agent: impl Into<String>,
        poll_interval: Duration,
    ) -> Self {
        ensure_rustls_provider();
        Self::with_options_and_http_client_builder(
            identity,
            user_agent,
            poll_interval,
            reqwest::Client::builder().timeout(Duration::from_secs(20)),
        )
        .expect("valid HTTP client configuration")
    }

    /// Supply custom TLS roots or HTTP settings. Replica cookies are enabled
    /// and redirects refused regardless of the builder's previous settings.
    pub fn with_options_and_http_client_builder(
        identity: Identity,
        user_agent: impl Into<String>,
        poll_interval: Duration,
        builder: reqwest::ClientBuilder,
    ) -> Result<Self> {
        ensure_rustls_provider();
        let http_client = builder
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| ThalovantError::Connection(err.without_url().to_string()))?;
        let (bus_tx, _) = broadcast::channel(64);
        let (hive_tx, _) = broadcast::channel(64);
        Ok(Self {
            state: Arc::new(HttpTransportState {
                identity,
                user_agent: user_agent.into(),
                poll_interval,
                http_client,
                bus_tx,
                hive_tx,
                health: Mutex::new(TransportHealth::default()),
                poll_task: Mutex::new(None),
                admitted: AtomicBool::new(false),
                noise: Mutex::new(None),
                noise_state_dir: Mutex::new(None),
                lifecycle: ConnectionControl::default(),
                poll: Mutex::new(()),
            }),
        })
    }

    /// Select a persistent directory for the client static key and hub pins.
    pub async fn set_noise_state_dir(&self, dir: Option<PathBuf>) {
        *self.state.noise_state_dir.lock().await = dir;
    }

    /// Authenticated hub static key, or `None` outside a working session.
    pub async fn remote_static_key(&self) -> Option<String> {
        self.state
            .noise
            .lock()
            .await
            .as_ref()
            .and_then(NoiseChannel::remote_key)
    }

    pub fn identity(&self) -> &Identity {
        &self.state.identity
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.state.bus_tx.subscribe()
    }

    pub fn subscribe_hive(&self) -> broadcast::Receiver<HiveMessage> {
        self.state.hive_tx.subscribe()
    }

    pub fn base_url(&self) -> String {
        self.state.identity.base_url()
    }

    pub fn authorization(&self) -> String {
        general_purpose::STANDARD.encode(format!(
            "{}:{}",
            self.state.user_agent, self.state.identity.access_key
        ))
    }

    pub async fn connect(&self) -> Result<()> {
        RuntimeTransport::Http(self.clone()).connect().await
    }

    async fn connect_locked(&self) -> Result<()> {
        self.stop_polling().await;
        if self.state.admitted.load(Ordering::Acquire) {
            // /connect does not issue a new offer for a peer still registered
            // locally. Reset only this object's previously admitted session.
            timeout(
                Duration::from_secs(2),
                self.request(reqwest::Method::POST, "/disconnect", None),
            )
            .await
            .map_err(|_| connection_timeout())??;
            self.state.admitted.store(false, Ordering::Release);
        }
        *self.state.noise.lock().await = None;
        *self.state.health.lock().await = TransportHealth {
            connection: connecting_connection(),
            ..Default::default()
        };
        let result = timeout(Duration::from_secs(20), self.connect_inner())
            .await
            .unwrap_or_else(|_| {
                Err(ThalovantError::Timeout(
                    "HiveMind HTTP Noise handshake timed out".into(),
                ))
            });
        if let Err(error) = &result {
            self.mark_connection_error(error).await;
        }
        result
    }

    async fn connect_inner(&self) -> Result<()> {
        let started = Instant::now();
        require_tls_endpoint(&self.base_url())?;
        if self.state.identity.password.is_empty() {
            return Err(ThalovantError::MissingIdentityField(
                "password: v3 Noise requires the identity password",
            ));
        }
        let dir = self.state.noise_state_dir.lock().await.clone();
        *self.state.noise.lock().await = Some(NoiseChannel::new(self.state.identity.clone(), dir));
        self.request(reqwest::Method::POST, "/connect", None)
            .await?;
        self.state.admitted.store(true, Ordering::Release);
        let opened = Instant::now();
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
            health.connection.phase = TransportConnectionPhase::Handshake;
            health.connection.transport_open_ms = Some(elapsed_ms(started, opened));
        }
        while !self.is_handshake_complete().await {
            self.poll_current_session().await?;
            if !self.is_handshake_complete().await {
                sleep(Duration::from_millis(100)).await;
            }
        }
        self.mark_connection_ready(started, opened).await;
        self.start_polling().await;
        Ok(())
    }

    async fn stop_polling(&self) {
        if let Some(task) = self.state.poll_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        RuntimeTransport::Http(self.clone()).disconnect().await
    }

    async fn disconnect_inner(&self) -> Result<()> {
        self.stop_polling().await;
        let _poll = self.state.poll.lock().await;
        *self.state.noise.lock().await = None;
        {
            let mut health = self.state.health.lock().await;
            health.connected = false;
            health.handshake_complete = false;
            health.transport_alive = false;
            health.connection.phase = TransportConnectionPhase::Closed;
        }
        if !self.state.admitted.load(Ordering::Acquire) {
            return Ok(());
        }
        self.request(reqwest::Method::POST, "/disconnect", None)
            .await?;
        self.state.admitted.store(false, Ordering::Release);
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        let ready = self
            .state
            .noise
            .lock()
            .await
            .as_ref()
            .is_some_and(NoiseChannel::ready);
        noise_health(
            self.state.health.lock().await.clone(),
            ready,
            self.state.lifecycle.cancelled.load(Ordering::Acquire),
        )
    }

    pub async fn connection_info(&self) -> TransportConnectionInfo {
        self.healthcheck().await.connection
    }

    pub async fn emit_bus(
        &self,
        event_type: &str,
        data: Data,
        context: Map<String, Value>,
    ) -> Result<()> {
        self.send_hive_message(
            HiveMessage {
                msg_type: "bus".to_string(),
                payload: Map::from_iter([
                    ("type".to_string(), Value::String(event_type.to_string())),
                    ("data".to_string(), Value::Object(data)),
                    ("context".to_string(), Value::Object(context)),
                ]),
                metadata: Map::new(),
                route: vec![],
                node: None,
                target_site_id: None,
                target_pubkey: None,
                source_peer: None,
            },
            true,
        )
        .await
    }

    pub async fn poll_once(&self) -> Result<()> {
        let _lifecycle = self.state.lifecycle.lock().await;
        self.poll_current_session().await
    }

    async fn poll_current_session(&self) -> Result<()> {
        let _poll = self.state.poll.lock().await;
        let result = self.poll_inner().await;
        if let Err(error) = &result {
            self.mark_connection_error(error).await;
        }
        result
    }

    async fn poll_inner(&self) -> Result<()> {
        if !self.state.health.lock().await.connected {
            return Ok(());
        }
        let body = self
            .request(reqwest::Method::GET, "/get_messages", None)
            .await?;
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| ThalovantError::Connection("malformed HTTP message queue".into()))?;
        for raw in messages {
            self.handle_raw_message(raw.clone()).await?;
        }
        if !self.is_handshake_complete().await {
            return Ok(());
        }
        let body = self
            .request(reqwest::Method::GET, "/get_binary_messages", None)
            .await?;
        let frames = body
            .get("b64_messages")
            .and_then(Value::as_array)
            .ok_or_else(|| ThalovantError::Connection("malformed HTTP binary queue".into()))?;
        for frame in frames {
            let encoded = frame
                .as_str()
                .ok_or_else(|| ThalovantError::Connection("malformed HTTP binary frame".into()))?;
            let raw = general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| ThalovantError::Connection("malformed HTTP binary frame".into()))?;
            self.receive_frame(&raw, true).await?;
        }
        Ok(())
    }

    async fn start_polling(&self) {
        let mut existing = self.state.poll_task.lock().await;
        if existing.is_some() {
            return;
        }
        let transport = self.clone();
        *existing = Some(tokio::spawn(async move {
            loop {
                sleep(transport.state.poll_interval).await;
                if let Err(error) = transport.poll_once().await {
                    let mut health = transport.state.health.lock().await;
                    health.connected = false;
                    health.transport_alive = false;
                    health.last_error = Some(error.to_string());
                    health.connection.phase = TransportConnectionPhase::Error;
                    health.connection.last_error = Some(error.to_string());
                    break;
                }
            }
        }));
    }

    async fn is_handshake_complete(&self) -> bool {
        self.healthcheck().await.handshake_complete
    }

    async fn handle_raw_message(&self, raw: Value) -> Result<()> {
        let bytes = match raw {
            Value::String(raw) => raw.into_bytes(),
            other => serde_json::to_vec(&other)?,
        };
        self.receive_frame(&bytes, false).await
    }

    async fn receive_frame(&self, raw: &[u8], binary: bool) -> Result<()> {
        let mut slot = self.state.noise.lock().await;
        let channel = slot
            .as_mut()
            .ok_or_else(|| ThalovantError::Connection("HTTP transport is not connected".into()))?;
        let (message, writes) = channel.receive(raw, binary)?;
        channel.failed = true;
        for write in writes {
            self.write_frame(write).await?;
        }
        channel.failed = false;
        let ready = channel.ready();
        drop(slot);
        dispatch_noise_message(&self.state.bus_tx, &self.state.hive_tx, message);
        if ready {
            self.state.health.lock().await.handshake_complete = true;
        }
        Ok(())
    }

    pub async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        timeout(Duration::from_secs(20), self.send_message_inner(message))
            .await
            .map_err(|_| ThalovantError::Timeout("Noise message send timed out".into()))?
    }

    async fn send_message_inner(&self, message: HiveMessage) -> Result<()> {
        let _lifecycle = self.state.lifecycle.lock().await;
        if self.state.lifecycle.cancelled.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection(
                "transport retired; reconnect required".into(),
            ));
        }
        let result = self.send_encrypted(message).await;
        if let Err(error) = &result {
            self.mark_connection_error(error).await;
        }
        result
    }

    async fn send_encrypted(&self, message: HiveMessage) -> Result<()> {
        let mut slot = self.state.noise.lock().await;
        let channel = slot
            .as_mut()
            .ok_or_else(|| ThalovantError::Connection("HTTP transport is not connected".into()))?;
        let writes = channel.encode(&message)?;
        channel.failed = true;
        for write in writes {
            self.write_frame(write).await?;
        }
        channel.failed = false;
        Ok(())
    }

    async fn write_frame(&self, write: NoiseWrite) -> Result<()> {
        let form = if write.binary {
            vec![
                ("message", general_purpose::STANDARD.encode(write.payload)),
                ("binary", "1".into()),
            ]
        } else {
            vec![(
                "message",
                String::from_utf8(write.payload)
                    .map_err(|_| ThalovantError::Connection("invalid handshake JSON".into()))?,
            )]
        };
        self.request(reqwest::Method::POST, "/send_message", Some(&form))
            .await
            .map(|_| ())
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        form: Option<&Vec<(&str, String)>>,
    ) -> Result<Value> {
        let mut request = self.state.http_client.request(method, self.endpoint(path));
        if let Some(form) = form {
            request = request.form(form);
        }
        let response = request
            .send()
            .await
            .map_err(|err| ThalovantError::Connection(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(ThalovantError::Connection(format!(
                "HTTP {path} status {}",
                response.status()
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| ThalovantError::Connection(format!("malformed HTTP {path} response")))?;
        if !body.is_object() || body.get("error").is_some() {
            return Err(ThalovantError::Runtime(format!(
                "HTTP {path} rejected by the hub"
            )));
        }
        let status = body.get("status").and_then(Value::as_str);
        let acknowledged = match path {
            "/connect" => status == Some("Connected"),
            "/disconnect" => status == Some("Disconnected"),
            "/send_message" => matches!(status, Some("message sent" | "buffered")),
            _ => true,
        };
        if !acknowledged {
            return Err(ThalovantError::Connection(format!(
                "HTTP {path} was not acknowledged"
            )));
        }
        Ok(body)
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}{}?authorization={}",
            self.base_url(),
            path,
            urlencoding::encode(&self.authorization())
        )
    }

    async fn mark_connection_ready(&self, started: Instant, opened: Instant) {
        let now = Instant::now();
        let mut health = self.state.health.lock().await;
        health.connection.phase = TransportConnectionPhase::Ready;
        health.connection.connected_at = Some(SystemTime::now());
        health.connection.handshake_ms = Some(elapsed_ms(opened, now));
        health.connection.connect_ms = Some(elapsed_ms(started, now));
        health.connection.last_error = None;
    }

    async fn mark_connection_error(&self, error: &ThalovantError) {
        *self.state.noise.lock().await = None;
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
        health.connection.phase = TransportConnectionPhase::Error;
        health.connection.last_error = Some(error.to_string());
    }
}

type WssWriter =
    futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WebSocketMessage>;

#[derive(Clone)]
pub struct WssTransport {
    state: Arc<WssTransportState>,
}

struct WssTransportState {
    lifecycle: ConnectionControl,
    identity: Identity,
    session_valid: AtomicBool,
    user_agent: String,
    bus_tx: broadcast::Sender<Event>,
    hive_tx: broadcast::Sender<HiveMessage>,
    health: Mutex<TransportHealth>,
    writer: Mutex<Option<WssWriter>>,
    read_task: Mutex<Option<JoinHandle<()>>>,
    handshake_notify: Notify,

    /// Overrides where the static key and the pin file live. `None` uses the
    /// directory holding the SDK config file.
    noise_state_dir: Mutex<Option<PathBuf>>,
    /// Shared handshake, trust and framing state. The writer lock is always
    /// acquired first and held through delivery of every generated frame.
    noise: Mutex<Option<NoiseChannel>>,
}

impl WssTransport {
    pub fn new(identity: Identity) -> Self {
        ensure_rustls_provider();
        let (bus_tx, _) = broadcast::channel(64);
        let (hive_tx, _) = broadcast::channel(64);
        Self {
            state: Arc::new(WssTransportState {
                lifecycle: ConnectionControl::default(),
                session_valid: AtomicBool::new(false),
                identity,
                user_agent: DEFAULT_USER_AGENT.to_string(),
                bus_tx,
                hive_tx,
                health: Mutex::new(TransportHealth::default()),
                writer: Mutex::new(None),
                read_task: Mutex::new(None),
                handshake_notify: Notify::new(),
                noise_state_dir: Mutex::new(None),
                noise: Mutex::new(None),
            }),
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.state.identity
    }

    /// Put the Noise static key and pin file somewhere other than beside the
    /// SDK config file.
    pub async fn set_noise_state_dir(&self, dir: Option<PathBuf>) {
        *self.state.noise_state_dir.lock().await = dir;
    }

    /// The hub's Noise static public key for the current session, hex encoded.
    /// `None` before the handshake completes.
    pub async fn remote_static_key(&self) -> Option<String> {
        let channel = self.state.noise.lock().await;
        if !self.state.session_valid.load(Ordering::Acquire) {
            return None;
        }
        channel.as_ref().and_then(NoiseChannel::remote_key)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.state.bus_tx.subscribe()
    }

    pub fn subscribe_hive(&self) -> broadcast::Receiver<HiveMessage> {
        self.state.hive_tx.subscribe()
    }

    pub async fn connect(&self) -> Result<()> {
        RuntimeTransport::Wss(self.clone()).connect().await
    }

    async fn connect_locked(&self) -> Result<()> {
        self.disconnect_inner().await?;
        let dir = self.state.noise_state_dir.lock().await.clone();
        *self.state.noise.lock().await = Some(NoiseChannel::new(self.state.identity.clone(), dir));
        *self.state.health.lock().await = TransportHealth::default();
        let started = Instant::now();
        self.set_connection(connecting_connection()).await;
        let endpoint_value = self
            .state
            .identity
            .endpoint_for(HubProtocol::Wss)
            .ok_or_else(|| {
                ThalovantError::UnsupportedProtocol(
                    "identity does not include a WSS endpoint".to_string(),
                )
            });
        let endpoint_value = match endpoint_value {
            Ok(endpoint_value) => endpoint_value,
            Err(error) => {
                self.mark_error(&error).await;
                return Err(error);
            }
        };
        let endpoint = match authorized_wss_url(&endpoint_value, &self.authorization()) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.mark_error(&error).await;
                return Err(error);
            }
        };
        let stream = connect_async(endpoint)
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()));
        let (stream, _) = match stream {
            Ok(stream) => stream,
            Err(error) => {
                self.mark_error(&error).await;
                return Err(error);
            }
        };
        let opened = Instant::now();
        let (writer, mut reader) = stream.split();
        *self.state.writer.lock().await = Some(writer);
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
            health.connection.phase = TransportConnectionPhase::Handshake;
            health.connection.transport_open_ms = Some(elapsed_ms(started, opened));
            health.connection.socket_open_ms = Some(elapsed_ms(started, opened));
        }
        let notified = self.state.handshake_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let transport = self.clone();
        *self.state.read_task.lock().await = Some(tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                match message {
                    Ok(WebSocketMessage::Text(payload)) => {
                        if let Err(error) = transport
                            .handle_socket_message(payload.into_bytes(), false)
                            .await
                        {
                            transport.mark_error(&error).await;
                            break;
                        }
                    }
                    Ok(WebSocketMessage::Binary(payload)) => {
                        if let Err(error) = transport.handle_socket_message(payload, true).await {
                            transport.mark_error(&error).await;
                            break;
                        }
                    }
                    Ok(WebSocketMessage::Close(_)) => {
                        transport.mark_disconnected().await;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let error = ThalovantError::Connection(error.to_string());
                        transport.mark_error(&error).await;
                        break;
                    }
                }
            }
            if transport.state.health.lock().await.connected {
                transport.mark_disconnected().await;
            }
            // Release a connect() still waiting on the handshake. A hub that
            // refuses one closes the socket, and reporting that as a timeout
            // would hide a wrong password behind the whole wait.
            transport.state.handshake_notify.notify_waiters();
        }));
        if !self.is_handshake_complete().await {
            // The first handshake with a hub runs argon2id at 64 MiB, which
            // costs a few hundred milliseconds on top of the round trips.
            let _ = timeout(Duration::from_secs(20), &mut notified).await;
        }
        if !self.is_handshake_complete().await {
            let cause = self.state.health.lock().await.last_error.clone();
            self.disconnect_inner().await?;
            let error = match cause {
                Some(reason) => ThalovantError::Connection(format!(
                    "v3 Noise handshake did not complete: {reason}"
                )),
                None => ThalovantError::Timeout("HiveMind WSS handshake timed out".to_string()),
            };
            self.mark_error(&error).await;
            return Err(error);
        }
        self.mark_connection_ready(started, opened).await;
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        RuntimeTransport::Wss(self.clone()).disconnect().await
    }

    async fn disconnect_inner(&self) -> Result<()> {
        if let Some(task) = self.state.read_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        // Give a graceful close a short budget, then drop the socket even if
        // the peer stops accepting writes.
        if let Some(mut writer) = self.state.writer.lock().await.take() {
            let _ = timeout(
                Duration::from_millis(100),
                writer.send(WebSocketMessage::Close(None)),
            )
            .await;
        }
        self.mark_disconnected().await;
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        // Keep health inspection independent of a backpressured writer. The
        // send guard and receive/reset paths invalidate this flag atomically.
        noise_health(
            self.state.health.lock().await.clone(),
            self.state.session_valid.load(Ordering::Acquire),
            self.state.lifecycle.cancelled.load(Ordering::Acquire),
        )
    }

    pub async fn connection_info(&self) -> TransportConnectionInfo {
        self.healthcheck().await.connection
    }

    pub async fn emit_bus(
        &self,
        event_type: &str,
        data: Data,
        context: Map<String, Value>,
    ) -> Result<()> {
        self.send_hive_message(
            HiveMessage {
                msg_type: "bus".to_string(),
                payload: Map::from_iter([
                    ("type".to_string(), Value::String(event_type.to_string())),
                    ("data".to_string(), Value::Object(data)),
                    ("context".to_string(), Value::Object(context)),
                ]),
                metadata: Map::new(),
                route: vec![],
                node: None,
                target_site_id: None,
                target_pubkey: None,
                source_peer: None,
            },
            true,
        )
        .await
    }

    fn authorization(&self) -> String {
        general_purpose::STANDARD.encode(format!(
            "{}:{}",
            self.state.user_agent, self.state.identity.access_key
        ))
    }

    async fn is_handshake_complete(&self) -> bool {
        self.healthcheck().await.handshake_complete
    }

    /// Drive the common Noise handshake and framing policy while retaining
    /// WebSocket writer serialization and cancellation poisoning.
    async fn handle_socket_message(&self, data: Vec<u8>, binary: bool) -> Result<()> {
        let mut writer = self.state.writer.lock().await;
        let mut slot = self.state.noise.lock().await;
        let channel = slot.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".into())
        })?;
        if channel.session.is_some() && !self.state.session_valid.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection(
                "Noise session interrupted; reconnect required".into(),
            ));
        }
        let mut guard = NoiseSendGuard {
            valid: &self.state.session_valid,
            committed: false,
        };
        let (message, writes) = channel.receive(&data, binary)?;
        channel.failed = true;
        Self::write_noise_frames(&mut writer, writes).await?;
        channel.failed = false;
        let ready = channel.ready();
        self.state.session_valid.store(ready, Ordering::Release);
        guard.committed = true;
        drop(slot);
        drop(writer);
        dispatch_noise_message(&self.state.bus_tx, &self.state.hive_tx, message);
        if ready {
            let mut health = self.state.health.lock().await;
            health.handshake_complete = true;
            health.transport_alive = true;
            drop(health);
            self.state.handshake_notify.notify_waiters();
        }
        Ok(())
    }

    async fn write_noise_frames(
        writer: &mut Option<WssWriter>,
        writes: Vec<NoiseWrite>,
    ) -> Result<()> {
        let writer = writer.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".into())
        })?;
        for write in writes {
            let frame = if write.binary {
                WebSocketMessage::Binary(write.payload)
            } else {
                WebSocketMessage::Text(String::from_utf8(write.payload).map_err(|_| {
                    ThalovantError::Connection("invalid Noise handshake JSON".into())
                })?)
            };
            writer
                .send(frame)
                .await
                .map_err(|error| ThalovantError::Connection(error.to_string()))?;
        }
        Ok(())
    }

    pub async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        timeout(Duration::from_secs(20), self.send_message_inner(message))
            .await
            .map_err(|_| ThalovantError::Timeout("Noise message send timed out".into()))?
    }

    async fn send_message_inner(&self, message: HiveMessage) -> Result<()> {
        // Acquire the writer before cipher advancement. Holding both locks
        // through every chunk preserves counter order and connection ownership.
        let mut writer = self.state.writer.lock().await;
        let mut slot = self.state.noise.lock().await;
        let channel = slot.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".into())
        })?;
        if !self.state.session_valid.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection(
                "Noise session interrupted; reconnect required".into(),
            ));
        }
        let mut guard = NoiseSendGuard {
            valid: &self.state.session_valid,
            committed: false,
        };
        let writes = channel.encode(&message)?;
        channel.failed = true;
        Self::write_noise_frames(&mut writer, writes).await?;
        channel.failed = false;
        guard.committed = true;
        Ok(())
    }

    async fn reset_noise(&self) {
        self.state.session_valid.store(false, Ordering::Release);
        *self.state.noise.lock().await = None;
    }

    async fn mark_error(&self, error: &ThalovantError) {
        self.reset_noise().await;
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
        health.connection.phase = TransportConnectionPhase::Error;
        health.connection.last_error = Some(error.to_string());
    }

    async fn mark_disconnected(&self) {
        self.reset_noise().await;
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        health.connection.phase = TransportConnectionPhase::Closed;
    }

    async fn set_connection(&self, connection: TransportConnectionInfo) {
        self.state.health.lock().await.connection = connection;
    }

    async fn mark_connection_ready(&self, started: Instant, opened: Instant) {
        let now = Instant::now();
        let mut health = self.state.health.lock().await;
        health.connection.phase = TransportConnectionPhase::Ready;
        health.connection.connected_at = Some(SystemTime::now());
        health.connection.handshake_ms = Some(elapsed_ms(opened, now));
        health.connection.connect_ms = Some(elapsed_ms(started, now));
        health.connection.last_error = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttTopicSet {
    pub inbound: String,
    pub outbound: String,
    pub status: String,
}

#[derive(Clone)]
pub struct MqttTransport {
    state: Arc<MqttTransportState>,
}

struct MqttTransportState {
    identity: Identity,
    lifecycle: ConnectionControl,
    noise: Mutex<Option<NoiseChannel>>,
    noise_state_dir: Mutex<Option<PathBuf>>,
    tls_config: Mutex<Option<TlsConfiguration>>,
    topics: MqttTopicSet,
    client: Mutex<Option<AsyncClient>>,
    bus_tx: broadcast::Sender<Event>,
    hive_tx: broadcast::Sender<HiveMessage>,
    health: Mutex<TransportHealth>,
    event_task: Mutex<Option<JoinHandle<()>>>,
    incoming_task: Mutex<Option<JoinHandle<()>>>,
    handshake_notify: Notify,
}

impl MqttTransport {
    pub fn new(identity: Identity) -> Result<Self> {
        ensure_rustls_provider();
        let topics = mqtt_topics_for_identity(&identity)?;
        let (bus_tx, _) = broadcast::channel(64);
        let (hive_tx, _) = broadcast::channel(64);
        Ok(Self {
            state: Arc::new(MqttTransportState {
                lifecycle: ConnectionControl::default(),
                noise: Mutex::new(None),
                noise_state_dir: Mutex::new(None),
                tls_config: Mutex::new(None),
                identity,
                topics,
                client: Mutex::new(None),
                bus_tx,
                hive_tx,
                health: Mutex::new(TransportHealth::default()),
                event_task: Mutex::new(None),
                incoming_task: Mutex::new(None),
                handshake_notify: Notify::new(),
            }),
        })
    }

    /// Select a persistent directory for the client static key and hub pins.
    pub async fn set_noise_state_dir(&self, dir: Option<PathBuf>) {
        *self.state.noise_state_dir.lock().await = dir;
    }

    /// Configure broker trust roots or a TLS client certificate.
    pub async fn set_tls_configuration(&self, config: Option<TlsConfiguration>) {
        *self.state.tls_config.lock().await = config;
    }

    /// Authenticated hub static key, or `None` outside a working session.
    pub async fn remote_static_key(&self) -> Option<String> {
        self.state
            .noise
            .lock()
            .await
            .as_ref()
            .and_then(NoiseChannel::remote_key)
    }

    pub fn identity(&self) -> &Identity {
        &self.state.identity
    }

    pub fn topics(&self) -> &MqttTopicSet {
        &self.state.topics
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.state.bus_tx.subscribe()
    }

    pub fn subscribe_hive(&self) -> broadcast::Receiver<HiveMessage> {
        self.state.hive_tx.subscribe()
    }

    pub async fn connect(&self) -> Result<()> {
        RuntimeTransport::Mqtt(self.clone()).connect().await
    }

    async fn connect_locked(&self) -> Result<()> {
        self.disconnect_inner().await?;
        *self.state.health.lock().await = TransportHealth::default();
        let result = timeout(Duration::from_secs(20), self.connect_inner())
            .await
            .unwrap_or_else(|_| {
                Err(ThalovantError::Timeout(
                    "HiveMind MQTT Noise handshake timed out".into(),
                ))
            });
        if let Err(error) = &result {
            self.disconnect_inner().await?;
            self.mark_error(error).await;
        }
        result
    }

    async fn connect_inner(&self) -> Result<()> {
        if self.state.identity.password.is_empty() {
            return Err(ThalovantError::MissingIdentityField(
                "password: v3 Noise requires the identity password",
            ));
        }
        let dir = self.state.noise_state_dir.lock().await.clone();
        *self.state.noise.lock().await = Some(NoiseChannel::new(self.state.identity.clone(), dir));
        let started = Instant::now();
        self.set_connection(connecting_connection()).await;
        let credentials = self.state.identity.mqtt.as_ref().ok_or_else(|| {
            ThalovantError::UnsupportedProtocol(
                "identity does not include MQTT broker credentials".to_string(),
            )
        });
        let credentials = match credentials {
            Ok(credentials) => credentials,
            Err(error) => {
                self.mark_error(&error).await;
                return Err(error);
            }
        };
        let mut options = match mqtt_options_for_identity(&self.state.identity) {
            Ok(options) => options,
            Err(error) => {
                self.mark_error(&error).await;
                return Err(error);
            }
        };
        if let Some(config) = self.state.tls_config.lock().await.clone() {
            options.set_transport(Transport::tls_with_config(config));
        }
        // A Noise frame may be 65,535 bytes, plus MQTT topic/header overhead.
        // rumqttc's 10 KiB default otherwise rejects valid encrypted chunks.
        options.set_max_packet_size(128 * 1024, 128 * 1024);
        options.set_keep_alive(Duration::from_secs(60));
        options.set_clean_session(true);
        options.set_credentials(credentials.username.clone(), credentials.password.clone());
        options.set_last_will(LastWill::new(
            self.state.topics.status.clone(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));
        let (client, mut eventloop) = AsyncClient::new(options, 16);
        *self.state.client.lock().await = Some(client.clone());
        let notified = self.state.handshake_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Keep polling the broker while the protocol worker publishes Noise
        // replies. Waiting for a bounded publish queue from inside eventloop.poll
        // would deadlock large concurrent messages or handshake continuations.
        let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let transport = self.clone();
        *self.state.event_task.lock().await = Some(tokio::spawn(async move {
            let error = loop {
                match eventloop.poll().await {
                    Ok(MqttEvent::Incoming(Packet::Publish(publish))) => {
                        if incoming_tx.try_send(publish.payload.to_vec()).is_err() {
                            break ThalovantError::Connection(
                                "MQTT receive queue overflow; reconnect required".into(),
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(error) => break ThalovantError::Connection(error.to_string()),
                }
            };
            // Drop the request receiver before acquiring protocol state so
            // publishers awaiting queue space wake and release their lock.
            drop(eventloop);
            transport.mark_error(&error).await;
            transport.state.handshake_notify.notify_waiters();
        }));
        let transport = self.clone();
        *self.state.incoming_task.lock().await = Some(tokio::spawn(async move {
            while let Some(payload) = incoming_rx.recv().await {
                if let Err(error) = transport.handle_raw_mqtt_payload(payload).await {
                    if let Some(task) = transport.state.event_task.lock().await.take() {
                        task.abort();
                        let _ = task.await;
                    }
                    transport.mark_error(&error).await;
                    transport.state.handshake_notify.notify_waiters();
                    break;
                }
            }
        }));
        if let Err(error) = client
            .subscribe(self.state.topics.outbound.clone(), qos(credentials.qos))
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
        {
            self.mark_error(&error).await;
            return Err(error);
        }
        if let Err(error) = client
            .publish(
                self.state.topics.status.clone(),
                QoS::AtLeastOnce,
                true,
                "online",
            )
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
        {
            self.mark_error(&error).await;
            return Err(error);
        }
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
            health.connection.phase = TransportConnectionPhase::Handshake;
            health.connection.transport_open_ms = Some(elapsed_ms(started, Instant::now()));
        }
        let opened = Instant::now();
        // The first clear HELLO creates the MQTT peer and requests its offer.
        self.publish_frame(NoiseWrite {
            payload: serde_json::to_vec(&hello_hive_message(
                &self.state.identity,
                "thalovant-rust-mqtt-",
            ))?,
            binary: false,
        })
        .await?;
        if !self.is_handshake_complete().await && self.state.health.lock().await.connected {
            let _ = timeout(Duration::from_secs(20), &mut notified).await;
        }
        if !self.is_handshake_complete().await {
            let cause = self.state.health.lock().await.last_error.clone();
            return Err(ThalovantError::Connection(cause.unwrap_or_else(|| {
                "HiveMind MQTT Noise handshake did not complete".into()
            })));
        }
        self.mark_connection_ready(started, opened).await;
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        RuntimeTransport::Mqtt(self.clone()).disconnect().await
    }

    async fn disconnect_inner(&self) -> Result<()> {
        // Publish offline while the event loop still drives the connection.
        if let Some(client) = self.state.client.lock().await.take() {
            let _ = timeout(Duration::from_secs(1), async {
                client
                    .publish(
                        self.state.topics.status.clone(),
                        QoS::AtLeastOnce,
                        true,
                        "offline",
                    )
                    .await?;
                client.disconnect().await
            })
            .await;
        }
        if let Some(mut task) = self.state.event_task.lock().await.take() {
            // AsyncClient::disconnect only queues the packet. Let the event
            // loop flush it and the retained offline status before closing TCP.
            if timeout(Duration::from_secs(1), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(task) = self.state.incoming_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.mark_disconnected().await;
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        let ready = self
            .state
            .noise
            .lock()
            .await
            .as_ref()
            .is_some_and(NoiseChannel::ready);
        noise_health(
            self.state.health.lock().await.clone(),
            ready,
            self.state.lifecycle.cancelled.load(Ordering::Acquire),
        )
    }

    pub async fn connection_info(&self) -> TransportConnectionInfo {
        self.healthcheck().await.connection
    }

    pub async fn emit_bus(
        &self,
        event_type: &str,
        data: Data,
        context: Map<String, Value>,
    ) -> Result<()> {
        self.send_hive_message(
            HiveMessage {
                msg_type: "bus".to_string(),
                payload: Map::from_iter([
                    ("type".to_string(), Value::String(event_type.to_string())),
                    ("data".to_string(), Value::Object(data)),
                    ("context".to_string(), Value::Object(context)),
                ]),
                metadata: Map::new(),
                route: vec![],
                node: None,
                target_site_id: None,
                target_pubkey: None,
                source_peer: None,
            },
            true,
        )
        .await
    }

    async fn is_handshake_complete(&self) -> bool {
        self.healthcheck().await.handshake_complete
    }

    async fn handle_raw_mqtt_payload(&self, raw: Vec<u8>) -> Result<()> {
        let mut slot = self.state.noise.lock().await;
        let channel = slot
            .as_mut()
            .ok_or_else(|| ThalovantError::Connection("MQTT transport is not connected".into()))?;
        let binary = channel.ready();
        let (message, writes) = channel.receive(&raw, binary)?;
        channel.failed = true;
        for write in writes {
            self.publish_frame(write).await?;
        }
        channel.failed = false;
        let ready = channel.ready();
        drop(slot);
        dispatch_noise_message(&self.state.bus_tx, &self.state.hive_tx, message);
        if ready {
            let mut health = self.state.health.lock().await;
            health.handshake_complete = true;
            health.transport_alive = true;
            self.state.handshake_notify.notify_waiters();
        }
        Ok(())
    }

    pub async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        timeout(Duration::from_secs(20), self.send_message_inner(message))
            .await
            .map_err(|_| ThalovantError::Timeout("Noise message send timed out".into()))?
    }

    async fn send_message_inner(&self, message: HiveMessage) -> Result<()> {
        let _lifecycle = self.state.lifecycle.lock().await;
        if self.state.lifecycle.cancelled.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection(
                "transport retired; reconnect required".into(),
            ));
        }
        let result = self.send_encrypted(message).await;
        if let Err(error) = &result {
            self.mark_error(error).await;
        }
        result
    }

    async fn send_encrypted(&self, message: HiveMessage) -> Result<()> {
        let mut slot = self.state.noise.lock().await;
        let channel = slot
            .as_mut()
            .ok_or_else(|| ThalovantError::Connection("MQTT transport is not connected".into()))?;
        let writes = channel.encode(&message)?;
        channel.failed = true;
        for write in writes {
            self.publish_frame(write).await?;
        }
        channel.failed = false;
        Ok(())
    }

    async fn publish_frame(&self, write: NoiseWrite) -> Result<()> {
        let client =
            self.state.client.lock().await.clone().ok_or_else(|| {
                ThalovantError::Connection("MQTT transport is not connected".into())
            })?;
        let publish_qos = self
            .state
            .identity
            .mqtt
            .as_ref()
            .map(|credentials| credentials.qos)
            .unwrap_or(1);
        client
            .publish(
                self.state.topics.inbound.clone(),
                qos(publish_qos),
                false,
                write.payload,
            )
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
    }

    async fn mark_error(&self, error: &ThalovantError) {
        *self.state.noise.lock().await = None;
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
        health.connection.phase = TransportConnectionPhase::Error;
        health.connection.last_error = Some(error.to_string());
    }

    async fn mark_disconnected(&self) {
        *self.state.noise.lock().await = None;
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        health.connection.phase = TransportConnectionPhase::Closed;
    }

    async fn set_connection(&self, connection: TransportConnectionInfo) {
        self.state.health.lock().await.connection = connection;
    }

    async fn mark_connection_ready(&self, started: Instant, opened: Instant) {
        let now = Instant::now();
        let mut health = self.state.health.lock().await;
        health.connection.phase = TransportConnectionPhase::Ready;
        health.connection.connected_at = Some(SystemTime::now());
        health.connection.handshake_ms = Some(elapsed_ms(opened, now));
        health.connection.connect_ms = Some(elapsed_ms(started, now));
        health.connection.last_error = None;
    }
}

// Cancellation after cipher advancement leaves delivery uncertain. Preserve
// the static identity, but require a new session before any subsequent send.
struct NoiseSendGuard<'a> {
    valid: &'a AtomicBool,
    committed: bool,
}
impl Drop for NoiseSendGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.valid.store(false, Ordering::Release);
        }
    }
}

// Transport-independent v3 negotiation. Callers hold their channel mutex across
// encryption and delivery of all chunks so counter order matches wire order.
// Set failed before an awaited write, and clear it only after every frame was
// delivered: cancellation or an uncertain write must never reuse the session.
struct NoiseChannel {
    identity: Identity,
    state_dir: Option<PathBuf>,
    hello: Option<Map<String, Value>>,
    node_id: String,
    handshake: Option<NoiseHandshake>,
    session: Option<NoiseSession>,
    failed: bool,
}

struct NoiseWrite {
    payload: Vec<u8>,
    binary: bool,
}

impl Drop for NoiseChannel {
    fn drop(&mut self) {
        // KK authenticates the first message: a peer may reject a stale PSK by
        // closing before sending any response. An abandoned initial handshake
        // must not keep retrying that provisional credential indefinitely.
        if self.handshake.is_some() && self.session.is_none() {
            let _ = forget_cached_psk(self.state_dir.as_deref(), &self.node_id);
        }
    }
}

impl NoiseChannel {
    fn new(identity: Identity, state_dir: Option<PathBuf>) -> Self {
        Self {
            identity,
            state_dir,
            hello: None,
            node_id: String::new(),
            handshake: None,
            session: None,
            failed: false,
        }
    }

    fn ready(&self) -> bool {
        self.session.is_some() && !self.failed
    }

    fn remote_key(&self) -> Option<String> {
        self.session
            .as_ref()
            .filter(|_| !self.failed)
            .and_then(|session| session.remote_static_key().map(str::to_string))
    }

    fn encode(&self, message: &HiveMessage) -> Result<Vec<NoiseWrite>> {
        if !self.ready() {
            return Err(ThalovantError::Connection(
                "refusing to send before the v3 Noise session is established".into(),
            ));
        }
        let session = self.session.as_ref().expect("checked above");
        Ok(session
            .encrypt_message(&serde_json::to_vec(message)?, true)?
            .into_iter()
            .map(|payload| NoiseWrite {
                payload,
                binary: true,
            })
            .collect())
    }

    fn receive(
        &mut self,
        data: &[u8],
        binary: bool,
    ) -> Result<(Option<HiveMessage>, Vec<NoiseWrite>)> {
        if self.failed {
            return Err(ThalovantError::Connection(
                "Noise session failed; reconnect required".into(),
            ));
        }
        let result = self.receive_inner(data, binary);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn receive_inner(
        &mut self,
        data: &[u8],
        binary: bool,
    ) -> Result<(Option<HiveMessage>, Vec<NoiseWrite>)> {
        if let Some(session) = &self.session {
            if !binary {
                return Err(ThalovantError::Connection(
                    "plaintext received after Noise negotiation".into(),
                ));
            }
            let message = match session.decrypt_frame(data)? {
                NoiseFrame::Partial => return Ok((None, vec![])),
                NoiseFrame::Message {
                    payload,
                    is_json: true,
                } => serde_json::from_slice(&payload)?,
                NoiseFrame::Message {
                    payload,
                    is_json: false,
                } => decode_hive_binary_frame(&payload)?,
            };
            return Ok((Some(message), vec![]));
        }
        if binary {
            return Err(ThalovantError::Connection(
                "ciphertext arrived before Noise negotiation".into(),
            ));
        }
        let message: HiveMessage = serde_json::from_slice(data)?;
        match message.msg_type.as_str() {
            "hello" => {
                if self.hello.is_some() || self.handshake.is_some() {
                    return Err(ThalovantError::Connection("duplicate Noise HELLO".into()));
                }
                self.node_id = message
                    .payload
                    .get("node_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if self.node_id.is_empty() {
                    return Err(ThalovantError::Connection(
                        "Noise HELLO lacks node_id".into(),
                    ));
                }
                self.hello = Some(message.payload);
                Ok((None, vec![]))
            }
            "shake" | "handshake" => {
                let params = message
                    .payload
                    .get("noise")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ThalovantError::Connection("the hub did not offer v3 Noise".into())
                    })?;
                let writes = if params.contains_key("msg") {
                    self.continue_handshake(params)?
                } else {
                    self.start(&message.payload, params)?
                };
                Ok((None, writes))
            }
            _ => Err(ThalovantError::Connection(
                "application traffic received before Noise negotiation".into(),
            )),
        }
    }

    fn clear(params: Value) -> Result<NoiseWrite> {
        Ok(NoiseWrite {
            payload: serde_json::to_vec(&HiveMessage {
                msg_type: "shake".into(),
                payload: json!({"noise":params})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ..Default::default()
            })?,
            binary: false,
        })
    }

    fn start(
        &mut self,
        offer: &Map<String, Value>,
        params: &Map<String, Value>,
    ) -> Result<Vec<NoiseWrite>> {
        if self.node_id.is_empty() || self.handshake.is_some() {
            return Err(ThalovantError::Connection(
                "unexpected Noise capability offer".into(),
            ));
        }
        if self.identity.password.is_empty() {
            return Err(ThalovantError::MissingIdentityField(
                "password: v3 Noise requires the identity password",
            ));
        }
        let pin = load_noise_pin(self.state_dir.as_deref(), &self.node_id)?;
        let (pattern, suite) = select_noise_options(
            &string_list(params.get("patterns")),
            &string_list(params.get("suites")),
            pin.as_deref(),
        )
        .ok_or_else(|| ThalovantError::Connection("no supported Noise pattern and suite".into()))?;
        let prologue = build_prologue(
            self.hello.as_ref().expect("HELLO checked above"),
            offer,
            &noise_protocol_name(&pattern, &suite),
        );
        let key = load_or_create_noise_key(self.state_dir.as_deref())?;
        let psk = match load_cached_psk(self.state_dir.as_deref(), &self.node_id)? {
            Some(psk) => psk,
            None => {
                let derived = derive_psk(&self.identity.password, &self.node_id)?;
                // Persistence is an optimization; a failed cache write does not
                // prevent an otherwise valid handshake.
                let _ = save_cached_psk(self.state_dir.as_deref(), &self.node_id, &derived);
                derived
            }
        };
        let mut handshake =
            NoiseHandshake::new(&pattern, &suite, &psk, &prologue, &key, pin.as_deref())?;
        let preferences = canonical_json(&json!({"binarize":false,"encodings":[]}));
        let msg = handshake.write_message(preferences.as_bytes())?;
        self.handshake = Some(handshake);
        Ok(vec![Self::clear(
            json!({"pattern":pattern,"suite":suite,"msg":hex::encode(msg)}),
        )?])
    }

    fn continue_handshake(&mut self, params: &Map<String, Value>) -> Result<Vec<NoiseWrite>> {
        let encoded = params
            .get("msg")
            .and_then(Value::as_str)
            .filter(|msg| !msg.is_empty())
            .ok_or_else(|| ThalovantError::Connection("malformed Noise envelope".into()))?;
        let msg = hex::decode(encoded)
            .map_err(|_| ThalovantError::Connection("malformed Noise envelope".into()))?;
        let handshake = self.handshake.as_mut().ok_or_else(|| {
            ThalovantError::Connection("Noise response before negotiation".into())
        })?;
        if let Err(error) = handshake.read_message(&msg) {
            // Cache entries are derived credentials, not trust decisions. A
            // rejected stale PSK can be recomputed on the next connection, but
            // an authenticated hub pin must never be discarded on failure.
            let _ = forget_cached_psk(self.state_dir.as_deref(), &self.node_id);
            self.handshake = None;
            return Err(error);
        }
        let mut writes = vec![];
        if !handshake.is_finished() {
            writes.push(Self::clear(
                json!({"msg":hex::encode(handshake.write_message(&[])?) }),
            )?);
        }
        let session = self
            .handshake
            .take()
            .expect("checked above")
            .into_session()?;
        let remote = session.remote_static_key().ok_or_else(|| {
            ThalovantError::Connection("Noise peer supplied no static identity".into())
        })?;
        pin_hub_key(self.state_dir.as_deref(), &self.node_id, remote)?;
        self.session = Some(session);
        writes.extend(self.encode(&hello_hive_message(&self.identity, "thalovant-rust-"))?);
        Ok(writes)
    }
}

fn noise_health(mut health: TransportHealth, ready: bool, cancelled: bool) -> TransportHealth {
    if cancelled {
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        if health.connection.phase != TransportConnectionPhase::Closed {
            health.connection.phase = TransportConnectionPhase::Error;
        }
    }
    health.handshake_complete &= ready;
    if !ready && health.connection.phase == TransportConnectionPhase::Ready {
        health.connected = false;
        health.transport_alive = false;
        health.connection.phase = TransportConnectionPhase::Error;
        health.last_error = Some("Noise session interrupted; reconnect required".into());
        health.connection.last_error = health.last_error.clone();
    }
    health
}

fn dispatch_noise_message(
    bus: &broadcast::Sender<Event>,
    hive: &broadcast::Sender<HiveMessage>,
    message: Option<HiveMessage>,
) {
    if let Some(message) = message {
        match message.msg_type.as_str() {
            "bus" => {
                let raw = serde_json::to_value(&message).ok();
                let _ = bus.send(event_from_bus_payload(&message.payload, raw));
            }
            "query" | "cascade" => {
                let _ = hive.send(message);
            }
            _ => {}
        }
    }
}

fn connecting_connection() -> TransportConnectionInfo {
    TransportConnectionInfo {
        phase: TransportConnectionPhase::Connecting,
        started_at: Some(SystemTime::now()),
        ..Default::default()
    }
}

fn elapsed_ms(start: Instant, end: Instant) -> f64 {
    end.saturating_duration_since(start).as_micros() as f64 / 1000.0
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn hello_hive_message(identity: &Identity, prefix: &str) -> HiveMessage {
    HiveMessage {
        msg_type: "hello".to_string(),
        payload: Map::from_iter([
            (
                "pubkey".to_string(),
                Value::String(identity.public_key.clone().unwrap_or_default()),
            ),
            (
                "session".to_string(),
                json!({"session_id": format!("{}{}", prefix, uuid::Uuid::new_v4().simple())}),
            ),
            (
                "site_id".to_string(),
                Value::String(identity.site_id.clone()),
            ),
        ]),
        metadata: Map::new(),
        route: vec![],
        node: None,
        target_site_id: None,
        target_pubkey: None,
        source_peer: None,
    }
}

fn authorized_wss_url(endpoint: &str, authorization: &str) -> Result<String> {
    let mut parsed =
        Url::parse(endpoint).map_err(|err| ThalovantError::Connection(err.to_string()))?;
    if parsed.scheme() != "ws" && parsed.scheme() != "wss" {
        return Err(ThalovantError::Connection(
            "WSS endpoint must start with ws:// or wss://".to_string(),
        ));
    }
    let existing = parsed
        .query_pairs()
        .filter(|(key, _)| key != "authorization")
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    parsed.set_query(None);
    {
        let mut query = parsed.query_pairs_mut();
        for (key, value) in existing {
            query.append_pair(&key, &value);
        }
        query.append_pair("authorization", authorization);
    }
    Ok(parsed.to_string())
}

pub fn mqtt_topics_for_identity(identity: &Identity) -> Result<MqttTopicSet> {
    let credentials = identity.mqtt.as_ref().ok_or_else(|| {
        ThalovantError::UnsupportedProtocol(
            "identity does not include MQTT broker credentials".to_string(),
        )
    })?;
    let base = credentials
        .topic_prefix
        .as_deref()
        .map(|value| value.trim().trim_matches('/').trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ThalovantError::Connection("MQTT credentials must include topic_prefix.".to_string())
        })?;
    // Reject MQTT wildcards (`#`/`+`) and ASCII control chars (`< 0x20`, incl.
    // NUL) so a malformed prefix can't smuggle a wildcard subscription or a
    // control byte into the derived topic paths.
    if base
        .chars()
        .any(|character| matches!(character, '#' | '+') || (character as u32) < 0x20)
    {
        return Err(ThalovantError::Connection(
            "MQTT topic_prefix contains characters that are not valid in an MQTT topic."
                .to_string(),
        ));
    }
    Ok(MqttTopicSet {
        inbound: format!("{base}/in"),
        outbound: format!("{base}/out"),
        status: format!("{base}/status"),
    })
}

fn mqtt_options_for_identity(identity: &Identity) -> Result<MqttOptions> {
    let credentials = identity.mqtt.as_ref().ok_or_else(|| {
        ThalovantError::UnsupportedProtocol(
            "identity does not include MQTT broker credentials".to_string(),
        )
    })?;
    let parsed = Url::parse(&credentials.endpoint)
        .map_err(|err| ThalovantError::Connection(err.to_string()))?;
    let host = parsed.host_str().ok_or_else(|| {
        ThalovantError::Connection("MQTT endpoint must include a host".to_string())
    })?;
    let tls_enabled = mqtt_tls_enabled(credentials, parsed.scheme());
    // TLS protects broker credentials; Noise protects the hub session.
    if !tls_enabled {
        return Err(ThalovantError::Connection(
            "refusing to connect to an MQTT broker without TLS. Use an mqtts:// endpoint, or set tls: true on the identity's mqtt block"
                .to_string(),
        ));
    }
    let port = parsed.port().unwrap_or(mqtt_default_port(tls_enabled));
    let mut options = MqttOptions::new(
        format!("thalovant-{}", uuid::Uuid::new_v4().simple()),
        host,
        port,
    );
    if tls_enabled {
        options.set_transport(default_mqtt_tls_transport());
    }
    Ok(options)
}

/// Refuse a hub endpoint that is not https.
fn require_tls_endpoint(endpoint: &str) -> Result<()> {
    let parsed = Url::parse(endpoint).map_err(|_| {
        ThalovantError::Connection(format!(
            "the HTTP transport needs a valid https:// endpoint; got {endpoint}"
        ))
    })?;
    if parsed.scheme() != "https" {
        return Err(ThalovantError::Connection(format!(
            "refusing to use the HTTP transport over {}://. It needs an https:// endpoint: without TLS every message and the access key travel in the clear",
            parsed.scheme()
        )));
    }
    Ok(())
}

fn default_mqtt_tls_transport() -> Transport {
    Transport::tls_with_config(TlsConfiguration::Native)
}

fn mqtt_tls_enabled(credentials: &MqttBrokerCredentials, scheme: &str) -> bool {
    credentials.tls || matches!(scheme, "mqtts" | "ssl")
}

fn mqtt_default_port(tls_enabled: bool) -> u16 {
    if tls_enabled {
        8883
    } else {
        1883
    }
}

fn qos(value: u8) -> QoS {
    if value == 0 {
        QoS::AtMostOnce
    } else {
        QoS::AtLeastOnce
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mqtt_credentials(endpoint: &str, tls: bool) -> MqttBrokerCredentials {
        MqttBrokerCredentials {
            endpoint: endpoint.to_string(),
            username: "access".to_string(),
            password: "broker-password".to_string(),
            topic_prefix: Some("hivemind/hub".to_string()),
            qos: 1,
            tls,
        }
    }

    fn identity_with_topic_prefix(topic_prefix: &str) -> Identity {
        Identity::from_value(serde_json::json!({
            "access_key": "access",
            "password": "secret",
            "site_id": "site",
            "default_master": "https://hub.example.com",
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "access",
                "password": "broker-password",
                "topic_prefix": topic_prefix
            }
        }))
        .expect("identity builds")
    }

    #[test]
    fn mqtt_topics_trim_surrounding_whitespace_and_slashes() {
        // Whitespace hugging the slashes must be trimmed, not baked into the
        // derived topics.
        let identity = identity_with_topic_prefix("/ hivemind/hub /");
        let topics = mqtt_topics_for_identity(&identity).expect("valid prefix");

        assert_eq!(topics.inbound, "hivemind/hub/in");
        assert_eq!(topics.outbound, "hivemind/hub/out");
        assert_eq!(topics.status, "hivemind/hub/status");
    }

    #[test]
    fn mqtt_topics_reject_whitespace_only_prefix() {
        // `"/ \t/"` survives the identity parser's outer trim (it is slash-bounded)
        // but is empty once slashes and inner whitespace are stripped.
        let identity = identity_with_topic_prefix("/ \t/");
        let message = mqtt_topics_for_identity(&identity).unwrap_err().to_string();

        assert!(
            message.contains("must include topic_prefix"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn mqtt_topics_reject_wildcard_prefix() {
        for prefix in ["hivemind/hub/#", "hivemind/+/hub"] {
            let identity = identity_with_topic_prefix(prefix);
            let message = mqtt_topics_for_identity(&identity).unwrap_err().to_string();

            assert!(
                message.contains("not valid in an MQTT topic"),
                "prefix {prefix:?} should be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn mqtt_topics_reject_control_char_prefix() {
        for prefix in ["hivemind/hub/\u{0}", "hivemind/\u{1}hub"] {
            let identity = identity_with_topic_prefix(prefix);
            let message = mqtt_topics_for_identity(&identity).unwrap_err().to_string();

            assert!(
                message.contains("not valid in an MQTT topic"),
                "prefix {prefix:?} should be rejected, got: {message}"
            );
        }
    }

    #[test]
    fn mqtt_tls_flag_controls_default_port() {
        let credentials = mqtt_credentials("mqtt://mqtt.example.com", true);

        assert!(mqtt_tls_enabled(&credentials, "mqtt"));
        assert_eq!(mqtt_default_port(true), 8883);
        assert_eq!(mqtt_default_port(false), 1883);
    }

    #[test]
    fn mqtt_tls_uses_the_native_security_backend() {
        assert!(matches!(
            default_mqtt_tls_transport(),
            Transport::Tls(TlsConfiguration::Native)
        ));
    }

    #[tokio::test]
    async fn http_transport_last_error_omits_authorization_url() {
        // `default_master` points at a closed port; the data-plane request URL is
        // `http://127.0.0.1:1/connect?authorization=<base64(user_agent:access_key)>`.
        let identity = Identity::from_value(serde_json::json!({
            "access_key": "access",
            "password": "secret",
            "site_id": "site",
            "default_master": "http://127.0.0.1:1"
        }))
        .unwrap();
        let transport = HttpTransport::new(identity);
        assert!(transport.connect().await.is_err());

        let last_error = transport
            .healthcheck()
            .await
            .last_error
            .expect("a failed connect must record last_error");
        assert!(
            !last_error.contains("authorization"),
            "last_error leaked the authorization query: {last_error}"
        );
        assert!(
            !last_error.contains("127.0.0.1"),
            "last_error leaked the request URL: {last_error}"
        );
    }

    #[test]
    fn transport_health_defaults_to_idle_connection() {
        let health = TransportHealth::default();

        assert_eq!(health.connection.phase, TransportConnectionPhase::Idle);
        assert!(health.connection.connect_ms.is_none());
    }

    /// Pins the one thing standing between an MQTT message and the wire now
    /// that v3 removed the separate payload cipher.
    #[test]
    fn mqtt_options_refuse_a_cleartext_broker() {
        let identity = Identity::from_value(serde_json::json!({
            "access_key": "access",
            "password": "secret",
            "site_id": "site",
            "default_master": "https://hub.example.com",
            "mqtt": {
                "endpoint": "mqtt://broker.example.com:1883",
                "username": "access",
                "password": "broker-secret",
                "topic_prefix": "hubs/hub-1/client-1",
                "tls": false
            }
        }))
        .unwrap();

        let error = mqtt_options_for_identity(&identity)
            .expect_err("connected to a cleartext MQTT broker; every message and the broker password would go out in the clear")
            .to_string();
        assert!(
            error.contains("mqtts://"),
            "the refusal does not tell the caller how to proceed: {error}"
        );
    }

    #[test]
    fn mqtt_options_accept_a_tls_broker() {
        let identity = Identity::from_value(serde_json::json!({
            "access_key": "access",
            "password": "secret",
            "site_id": "site",
            "default_master": "https://hub.example.com",
            "mqtt": {
                "endpoint": "mqtts://broker.example.com:8883",
                "username": "access",
                "password": "broker-secret",
                "topic_prefix": "hubs/hub-1/client-1",
                "tls": true
            }
        }))
        .unwrap();

        assert!(mqtt_options_for_identity(&identity).is_ok());
    }

    /// Pins the one thing standing between an HTTPS-transport message and the
    /// wire now that v3 removed the payload cipher. The access key also travels
    /// in the authorization query.
    #[test]
    fn require_tls_endpoint_refuses_cleartext() {
        let error = require_tls_endpoint("http://hub.example.com")
            .expect_err("accepted a cleartext endpoint; every message and the access key would go out in the clear")
            .to_string();
        assert!(
            error.contains("https://"),
            "the refusal does not tell the caller how to proceed: {error}"
        );

        assert!(require_tls_endpoint("https://hub.example.com").is_ok());
        assert!(require_tls_endpoint("not a url").is_err());
    }
}

#[cfg(test)]
#[path = "transport_noise_tests.rs"]
mod noise_transport_tests;
