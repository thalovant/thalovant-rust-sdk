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
    time::{sleep, timeout, Instant},
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
        match self {
            Self::Http(transport) => transport.connect().await,
            Self::Wss(transport) => transport.connect().await,
            Self::Mqtt(transport) => transport.connect().await,
        }
    }

    pub async fn disconnect(&self) -> Result<()> {
        match self {
            Self::Http(transport) => transport.disconnect().await,
            Self::Wss(transport) => transport.disconnect().await,
            Self::Mqtt(transport) => transport.disconnect().await,
        }
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
    lifecycle: Mutex<()>,
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
                lifecycle: Mutex::new(()),
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
        let _lifecycle = self.state.lifecycle.lock().await;
        self.stop_polling().await;
        if self.state.admitted.swap(false, Ordering::AcqRel) {
            // /connect does not issue a new offer for a peer still registered
            // locally. Reset only this object's previously admitted session.
            let _ = timeout(
                Duration::from_secs(2),
                self.request(reqwest::Method::POST, "/disconnect", None),
            )
            .await;
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
            if self.state.admitted.swap(false, Ordering::AcqRel) {
                let _ = timeout(
                    Duration::from_secs(2),
                    self.request(reqwest::Method::POST, "/disconnect", None),
                )
                .await;
            }
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
        let _lifecycle = self.state.lifecycle.lock().await;
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
        if !self.state.admitted.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.request(reqwest::Method::POST, "/disconnect", None)
            .await
            .map(|_| ())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        let ready = self
            .state
            .noise
            .lock()
            .await
            .as_ref()
            .is_some_and(NoiseChannel::ready);
        let mut health = self.state.health.lock().await.clone();
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
        let _lifecycle = self.state.lifecycle.lock().await;
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
    lifecycle: Mutex<()>,
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
    /// The hub's cleartext HELLO payload, kept verbatim because it is bound
    /// into the Noise prologue rather than read for the node id alone.
    server_hello: Mutex<Option<Map<String, Value>>>,
    node_id: Mutex<String>,
    noise_handshake: Mutex<Option<NoiseHandshake>>,
    session: Mutex<Option<Arc<NoiseSession>>>,
    /// Deriving the pre-shared key costs 64 MiB and a few hundred
    /// milliseconds, and the result is fixed for a (password, node id) pair,
    /// so a reconnect to the same hub reuses it.
    /// (node id, password) -> PSK. The password is part of the key: a caller
    /// that swaps it and reconnects on this same transport would otherwise be
    /// handed the previous one's key, which the hub refuses exactly as it
    /// refuses a wrong password. It is held in memory only -- the identity
    /// already carries it there -- and never written beside the key.
    psk_cache: Mutex<Option<(String, String, [u8; 32])>>,
}

impl WssTransport {
    pub fn new(identity: Identity) -> Self {
        ensure_rustls_provider();
        let (bus_tx, _) = broadcast::channel(64);
        let (hive_tx, _) = broadcast::channel(64);
        Self {
            state: Arc::new(WssTransportState {
                lifecycle: Mutex::new(()),
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
                server_hello: Mutex::new(None),
                node_id: Mutex::new(String::new()),
                noise_handshake: Mutex::new(None),
                session: Mutex::new(None),
                psk_cache: Mutex::new(None),
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
        if !self.state.session_valid.load(Ordering::Acquire) {
            return None;
        }
        self.state
            .session
            .lock()
            .await
            .as_ref()
            .and_then(|session| session.remote_static_key().map(str::to_string))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.state.bus_tx.subscribe()
    }

    pub fn subscribe_hive(&self) -> broadcast::Receiver<HiveMessage> {
        self.state.hive_tx.subscribe()
    }

    pub async fn connect(&self) -> Result<()> {
        let _lifecycle = self.state.lifecycle.lock().await;
        self.disconnect_inner().await?;
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
                        if let Err(error) =
                            transport.handle_socket_message(payload.into_bytes()).await
                        {
                            transport.mark_error(&error).await;
                            break;
                        }
                    }
                    Ok(WebSocketMessage::Binary(payload)) => {
                        if let Err(error) = transport.handle_socket_message(payload).await {
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
        let _lifecycle = self.state.lifecycle.lock().await;
        self.disconnect_inner().await
    }

    async fn disconnect_inner(&self) -> Result<()> {
        if let Some(task) = self.state.read_task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(mut writer) = self.state.writer.lock().await.take() {
            let _ = writer.send(WebSocketMessage::Close(None)).await;
        }
        self.mark_disconnected().await;
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        let mut health = self.state.health.lock().await.clone();
        health.handshake_complete &= self.state.session_valid.load(Ordering::Acquire);
        if !health.handshake_complete && health.connection.phase == TransportConnectionPhase::Ready
        {
            health.connected = false;
            health.transport_alive = false;
            health.connection.phase = TransportConnectionPhase::Error;
            health.last_error = Some("Noise session interrupted; reconnect required".into());
            health.connection.last_error = health.last_error.clone();
        }
        health
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
        self.state.health.lock().await.handshake_complete
    }

    /// Handle one websocket message.
    ///
    /// Before the Noise session exists the frames are cleartext JSON handshake
    /// traffic. After it they are Noise transport messages, and the plaintext
    /// underneath is what gets parsed.
    async fn handle_socket_message(&self, data: Vec<u8>) -> Result<()> {
        let session = self.state.session.lock().await.clone();

        let authenticated = session.is_some();
        if authenticated && !self.state.session_valid.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection(
                "Noise session interrupted; reconnect required".into(),
            ));
        }
        let raw = match session {
            Some(session) => match session.decrypt_frame(&data)? {
                NoiseFrame::Partial => return Ok(()),
                NoiseFrame::Message {
                    payload,
                    is_json: true,
                } => payload,
                NoiseFrame::Message {
                    payload,
                    is_json: false,
                } => serde_json::to_vec(&decode_hive_binary_frame(&payload)?)?,
            },
            None => data,
        };

        let decoded: Value = serde_json::from_slice(&raw)?;
        let msg_type = decoded
            .get("msg_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let payload = decoded
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        if !authenticated && !matches!(msg_type.as_str(), "hello" | "shake" | "handshake") {
            return Err(ThalovantError::Connection(
                "application traffic received before Noise negotiation".into(),
            ));
        }
        match msg_type.as_str() {
            "hello" => self.handle_hello(payload).await,
            "handshake" | "shake" => self.handle_handshake(payload).await,
            "bus" => {
                let event = event_from_bus_payload(&payload, Some(decoded));
                let _ = self.state.bus_tx.send(event);
                Ok(())
            }
            "query" | "cascade" => {
                let message: HiveMessage = serde_json::from_value(decoded)?;
                let _ = self.state.hive_tx.send(message);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Record the hub's cleartext HELLO. Both its payload and the parameter
    /// HANDSHAKE payload are bound into the Noise prologue, so it is kept
    /// whole rather than reduced to the node id.
    async fn handle_hello(&self, payload: Map<String, Value>) -> Result<()> {
        if self.state.session.lock().await.is_some() {
            return Ok(());
        }
        let mut hello = self.state.server_hello.lock().await;
        if hello.is_none() {
            *self.state.node_id.lock().await = payload
                .get("node_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            *hello = Some(payload);
        }
        Ok(())
    }

    async fn handle_handshake(&self, payload: Map<String, Value>) -> Result<()> {
        let noise_params = payload.get("noise").and_then(Value::as_object).cloned().ok_or_else(|| {
            ThalovantError::Connection(
                "this hub did not offer the v3 Noise handshake; the SDK requires a hub running HiveMind-core 5.x or newer".to_string(),
            )
        })?;

        if noise_params.contains_key("msg") {
            self.continue_noise_handshake(&noise_params).await
        } else {
            self.start_noise_handshake(&payload, &noise_params).await
        }
    }

    /// Select a pattern and suite, bind the negotiation into the prologue, and
    /// send Noise message 1.
    async fn start_noise_handshake(
        &self,
        handshake_payload: &Map<String, Value>,
        noise_params: &Map<String, Value>,
    ) -> Result<()> {
        let node_id = self.state.node_id.lock().await.clone();
        if node_id.is_empty() {
            return Err(ThalovantError::Connection(
                "the hub sent its HANDSHAKE parameters before a HELLO carrying node_id".to_string(),
            ));
        }
        let server_hello = self
            .state
            .server_hello
            .lock()
            .await
            .clone()
            .unwrap_or_default();

        let state_dir = self.state.noise_state_dir.lock().await.clone();
        let pinned = load_noise_pin(state_dir.as_deref(), &node_id)?;

        let (pattern, suite) = select_noise_options(
            &string_list(noise_params.get("patterns")),
            &string_list(noise_params.get("suites")),
            pinned.as_deref(),
        )
        .ok_or_else(|| {
            ThalovantError::Connection(
                "no Noise pattern and suite this SDK supports are on offer from the hub"
                    .to_string(),
            )
        })?;

        let protocol_name = noise_protocol_name(&pattern, &suite);
        let prologue = build_prologue(&server_hello, handshake_payload, &protocol_name);
        let static_key = load_or_create_noise_key(state_dir.as_deref())?;
        let psk = self.psk_for(&node_id).await?;

        let mut handshake = NoiseHandshake::new(
            &pattern,
            &suite,
            &psk,
            &prologue,
            &static_key,
            pinned.as_deref(),
        )?;

        // Message 1 carries this node's binarize capability and its
        // preference-ordered encodings, canonicalized so both peers hash the
        // same bytes.
        let noise_payload = canonical_json(&json!({"binarize": false, "encodings": []}));
        let message = handshake.write_message(noise_payload.as_bytes())?;
        *self.state.noise_handshake.lock().await = Some(handshake);

        self.send_cleartext(HiveMessage {
            msg_type: "shake".to_string(),
            payload: json!({"noise": {
                "pattern": pattern,
                "suite": suite,
                "msg": hex::encode(&message),
            }})
            .as_object()
            .cloned()
            .unwrap_or_default(),
            ..Default::default()
        })
        .await
    }

    /// Consume the hub's Noise message, send the final one where the pattern
    /// needs it, and bring the transport up.
    async fn continue_noise_handshake(&self, noise_params: &Map<String, Value>) -> Result<()> {
        let node_id = self.state.node_id.lock().await.clone();
        let state_dir = self.state.noise_state_dir.lock().await.clone();

        let encoded = noise_params
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = hex::decode(encoded).map_err(|_| {
            ThalovantError::Connection("malformed Noise handshake envelope".to_string())
        })?;

        let mut slot = self.state.noise_handshake.lock().await;
        let handshake = slot.as_mut().ok_or_else(|| {
            ThalovantError::Connection(
                "the hub sent a Noise handshake message before its parameters".to_string(),
            )
        })?;

        if let Err(error) = handshake.read_message(&message) {
            // A failed authentication must not remove trust. A key rotation
            // requires an explicit forget_noise_pin after verifying the hub.
            // The PSK is the other thing this message authenticates, so a
            // rejection may mean the stored key came from a password that has
            // since been rotated. Drop it; the next attempt derives again.
            let _ = forget_cached_psk(state_dir.as_deref(), &node_id);
            *self.state.psk_cache.lock().await = None;
            return Err(error);
        }

        let mut pending_final = None;
        if !handshake.is_finished() {
            // XXpsk2 message 3: our encrypted static key and the final DH mix.
            // The pattern and suite are named only on message 1.
            pending_final = Some(handshake.write_message(&[])?);
        }
        let handshake = slot.take().expect("checked above");
        drop(slot);

        if let Some(final_message) = pending_final {
            self.send_cleartext(HiveMessage {
                msg_type: "shake".to_string(),
                payload: json!({"noise": {"msg": hex::encode(&final_message)}})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ..Default::default()
            })
            .await?;
        }

        let session = handshake.into_session()?;
        if let Some(remote) = session.remote_static_key() {
            pin_hub_key(state_dir.as_deref(), &node_id, remote)?;
        }
        *self.state.session.lock().await = Some(Arc::new(session));
        self.state.session_valid.store(true, Ordering::Release);

        // The first Noise transport message is the encrypted HELLO.
        self.send_hive_message(
            hello_hive_message(&self.state.identity, "thalovant-rust-"),
            false,
        )
        .await?;

        let mut health = self.state.health.lock().await;
        health.handshake_complete = true;
        health.transport_alive = true;
        drop(health);
        self.state.handshake_notify.notify_waiters();
        Ok(())
    }

    /// Enforce trust on first use: the first key seen for a node id is
    /// recorded, and a later key that does not match it is refused.
    ///
    /// A changed key means the hub was reinstalled or another machine is
    /// answering at the address. The SDK cannot tell those apart, so it refuses
    /// and leaves clearing the pin as a deliberate act.
    /// Derive, or reuse, the pre-shared key for a hub.
    async fn psk_for(&self, node_id: &str) -> Result<[u8; 32]> {
        let password = &self.state.identity.password;
        if password.is_empty() {
            return Err(ThalovantError::MissingIdentityField(
                "password: the v3 Noise handshake derives its pre-shared key from it",
            ));
        }
        let derived_here_for_this_hub = {
            let cache = self.state.psk_cache.lock().await;
            match cache.as_ref() {
                Some((cached_node, cached_password, psk)) if cached_node == node_id => {
                    if cached_password == password {
                        return Ok(*psk);
                    }
                    true
                }
                _ => false,
            }
        };

        let state_dir = self.state.noise_state_dir.lock().await.clone();
        // Having already derived for this hub under a different password means
        // the stored key belongs to that one, so the disk read would only
        // return something known to be stale.
        let stored = if derived_here_for_this_hub {
            None
        } else {
            // On disk before deriving: argon2id at 64 MiB gives the same answer
            // for a password and hub every time, so a restart should not pay it
            // again. A key left from a password rotated elsewhere is caught by
            // the handshake, which forgets it.
            load_cached_psk(state_dir.as_deref(), node_id)?
        };
        let psk = match stored {
            Some(psk) => psk,
            None => {
                let derived = derive_psk(password, node_id)?;
                // Persisting is an optimisation, never a reason to fail.
                let _ = save_cached_psk(state_dir.as_deref(), node_id, &derived);
                derived
            }
        };
        *self.state.psk_cache.lock().await = Some((node_id.to_string(), password.clone(), psk));
        Ok(psk)
    }

    /// Write a handshake message as a cleartext JSON text frame. Only the
    /// handshake exchange travels this way; everything after it goes through
    /// the Noise session.
    async fn send_cleartext(&self, message: HiveMessage) -> Result<()> {
        let payload = serde_json::to_string(&message)?;
        let mut writer = self.state.writer.lock().await;
        let writer = writer.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".to_string())
        })?;
        writer
            .send(WebSocketMessage::Text(payload))
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
    }

    pub async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        let mut writer = self.state.writer.lock().await;
        let session = self.state.session.lock().await.clone().ok_or_else(|| {
            ThalovantError::Connection(
                "refusing to send before the v3 Noise session is established".to_string(),
            )
        })?;
        let raw = serde_json::to_vec(&message)?;

        // Take the writer *before* encrypting. encrypt_message advances the
        // cipher state nonce counter, and the hub decrypts strictly in counter
        // order -- so encrypting outside this lock lets two concurrent senders
        // consume their nonces in one order and reach the wire in the other,
        // which the hub treats as tampering and drops the session for.
        if !self.state.session_valid.load(Ordering::Acquire) {
            return Err(ThalovantError::Connection(
                "Noise session interrupted; reconnect required".into(),
            ));
        }
        let mut guard = NoiseSendGuard {
            valid: &self.state.session_valid,
            committed: false,
        };
        let frames = session.encrypt_message(&raw, true)?;
        let writer = writer.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".to_string())
        })?;
        for frame in frames {
            writer
                .send(WebSocketMessage::Binary(frame))
                .await
                .map_err(|err| ThalovantError::Connection(err.to_string()))?;
        }
        guard.committed = true;
        Ok(())
    }

    async fn reset_noise(&self) {
        self.state.session_valid.store(false, Ordering::Release);
        *self.state.session.lock().await = None;
        *self.state.noise_handshake.lock().await = None;
        *self.state.server_hello.lock().await = None;
        self.state.node_id.lock().await.clear();
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
    lifecycle: Mutex<()>,
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
                lifecycle: Mutex::new(()),
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
        let _lifecycle = self.state.lifecycle.lock().await;
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
        let _lifecycle = self.state.lifecycle.lock().await;
        self.disconnect_inner().await
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
        let mut health = self.state.health.lock().await.clone();
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
        let _lifecycle = self.state.lifecycle.lock().await;
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
        let psk = derive_psk(&self.identity.password, &self.node_id)?;
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
        handshake.read_message(&msg)?;
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
