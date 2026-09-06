use crate::{
    constants::DEFAULT_USER_AGENT,
    errors::{Result, ThalovantError},
    events::{event_from_bus_payload, Data, Event},
    identity::{Identity, MqttBrokerCredentials},
    noise::{
        build_prologue, canonical_json, derive_psk, noise_protocol_name, select_noise_options,
        NoiseFrame, NoiseHandshake, NoiseSession, NOISE_PATTERN_KK,
    },
    noise_store::{forget_noise_pin, load_noise_pin, load_or_create_noise_key, save_noise_pin},
    protocols::HubProtocol,
    tls::ensure_rustls_provider,
    wire::{decode_hive_binary_frame, encode_hive_binary_frame},
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
    path::{Path, PathBuf},
    sync::Arc,
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
        let (bus_tx, _) = broadcast::channel(64);
        let (hive_tx, _) = broadcast::channel(64);
        Self {
            state: Arc::new(HttpTransportState {
                identity,
                user_agent: user_agent.into(),
                poll_interval,
                http_client: reqwest::Client::new(),
                bus_tx,
                hive_tx,
                health: Mutex::new(TransportHealth::default()),
                poll_task: Mutex::new(None),
            }),
        }
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
        let started = Instant::now();
        self.set_connection(connecting_connection()).await;
        let response = self
            .state
            .http_client
            .post(self.endpoint("/connect"))
            .send()
            .await
            // `endpoint(..)` carries the access key in a `?authorization=`
            // query; strip the URL so it never reaches `last_error`.
            .map_err(|err| ThalovantError::Connection(err.without_url().to_string()));
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                self.mark_connection_error(&error).await;
                return Err(error);
            }
        };
        if !response.status().is_success() {
            let error = ThalovantError::Connection(format!(
                "HiveMind HTTP connect status {}",
                response.status()
            ));
            self.mark_connection_error(&error).await;
            return Err(error);
        }
        let opened = Instant::now();
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
            health.connection.phase = TransportConnectionPhase::Handshake;
            health.connection.transport_open_ms = Some(elapsed_ms(started, opened));
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while !self.is_handshake_complete().await && Instant::now() < deadline {
            if let Err(error) = self.poll_once().await {
                self.mark_connection_error(&error).await;
                return Err(error);
            }
            if !self.is_handshake_complete().await {
                sleep(Duration::from_millis(100)).await;
            }
        }
        if !self.is_handshake_complete().await {
            let error = ThalovantError::Timeout("HiveMind HTTP handshake timed out".to_string());
            self.mark_connection_error(&error).await;
            return Err(error);
        }
        self.mark_connection_ready(started, opened).await;
        self.start_polling().await;
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        if let Some(task) = self.state.poll_task.lock().await.take() {
            task.abort();
        }
        let _ = self
            .state
            .http_client
            .post(self.endpoint("/disconnect"))
            .send()
            .await;
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
        health.connection.phase = TransportConnectionPhase::Closed;
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        self.state.health.lock().await.clone()
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
        if !self.healthcheck().await.connected {
            return Ok(());
        }
        let response = self
            .state
            .http_client
            .get(self.endpoint("/get_messages"))
            .send()
            .await
            // Strip the `?authorization=` URL before it reaches `last_error`.
            .map_err(|err| ThalovantError::Connection(err.without_url().to_string()))?;
        let body: PollResponse = response.json().await?;
        if let Some(error) = body.error.filter(|value| !value.is_empty()) {
            return Err(ThalovantError::Runtime(error));
        }
        for raw in body.messages {
            self.handle_raw_message(raw).await?;
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
        self.state.health.lock().await.handshake_complete
    }

    async fn handle_raw_message(&self, raw: Value) -> Result<()> {
        let decoded = match raw {
            Value::String(raw) => serde_json::from_str(&raw)?,
            other => other,
        };
        let message: HiveMessage = serde_json::from_value(decoded.clone())?;
        match message.msg_type.as_str() {
            "handshake" | "shake" => self.handle_handshake(message.payload).await,
            "bus" => {
                let event = event_from_bus_payload(&message.payload, Some(decoded));
                let _ = self.state.bus_tx.send(event);
                Ok(())
            }
            "query" | "cascade" => {
                let _ = self.state.hive_tx.send(message);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Complete the HTTP handshake.
    ///
    /// HTTP runs no Noise session: it authenticates with the identity
    /// credentials and takes its confidentiality from TLS, so there is no key
    /// exchange here.
    async fn handle_handshake(&self, payload: Map<String, Value>) -> Result<()> {
        if !truthy(payload.get("handshake")) && payload.get("envelope").is_none() {
            self.send_hive_message(
                HiveMessage {
                    msg_type: "hello".to_string(),
                    payload: Map::from_iter([
                        (
                            "pubkey".to_string(),
                            Value::String(self.state.identity.public_key.clone().unwrap_or_default()),
                        ),
                        ("session".to_string(), json!({"session_id": format!("thalovant-rust-{}", uuid::Uuid::new_v4().simple())})),
                        ("site_id".to_string(), Value::String(self.state.identity.site_id.clone())),
                    ]),
                    metadata: Map::new(),
                    route: vec![],
                    node: None,
                    target_site_id: None,
                    target_pubkey: None,
                    source_peer: None,
                },
                false,
            )
            .await?;
            let mut health = self.state.health.lock().await;
            health.handshake_complete = true;
            health.transport_alive = true;
            return Ok(());
        }
        Err(ThalovantError::Connection(
            "unexpected HiveMind HTTP handshake envelope".to_string(),
        ))
    }

    pub async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        let payload = serde_json::to_string(&message)?;
        let response = self
            .state
            .http_client
            .post(self.endpoint("/send_message"))
            .form(&[("message", payload)])
            .send()
            .await
            // Strip the `?authorization=` URL before it reaches `last_error`.
            .map_err(|err| ThalovantError::Connection(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(ThalovantError::Connection(format!(
                "HiveMind HTTP send status {}",
                response.status()
            )));
        }
        Ok(())
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}{}?authorization={}",
            self.base_url(),
            path,
            urlencoding::encode(&self.authorization())
        )
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

    async fn mark_connection_error(&self, error: &ThalovantError) {
        let mut health = self.state.health.lock().await;
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
    identity: Identity,
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
    psk_cache: Mutex<Option<(String, [u8; 32])>>,
}

impl WssTransport {
    pub fn new(identity: Identity) -> Self {
        ensure_rustls_provider();
        let (bus_tx, _) = broadcast::channel(64);
        let (hive_tx, _) = broadcast::channel(64);
        Self {
            state: Arc::new(WssTransportState {
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
            // Release a connect() still waiting on the handshake. A hub that
            // refuses one closes the socket, and reporting that as a timeout
            // would hide a wrong password behind the whole wait.
            transport.state.handshake_notify.notify_waiters();
        }));
        let notified = self.state.handshake_notify.notified();
        tokio::pin!(notified);
        if !self.is_handshake_complete().await {
            // The first handshake with a hub runs argon2id at 64 MiB, which
            // costs a few hundred milliseconds on top of the round trips.
            let _ = timeout(Duration::from_secs(20), &mut notified).await;
        }
        if !self.is_handshake_complete().await {
            let cause = self.state.health.lock().await.last_error.clone();
            self.disconnect().await?;
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
        if let Some(task) = self.state.read_task.lock().await.take() {
            task.abort();
        }
        if let Some(mut writer) = self.state.writer.lock().await.take() {
            let _ = writer.send(WebSocketMessage::Close(None)).await;
        }
        self.mark_disconnected().await;
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        self.state.health.lock().await.clone()
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

        let raw = match session {
            Some(session) => match session.decrypt_frame(&data)? {
                NoiseFrame::Partial => return Ok(()),
                NoiseFrame::Message {
                    payload,
                    is_json: true,
                } => payload,
                // A HIVEMIND-WIRE-1 binary frame. The Rust SDK does not decode
                // binary bus payloads on this transport yet, so it is dropped
                // rather than mis-parsed as JSON.
                NoiseFrame::Message { is_json: false, .. } => return Ok(()),
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
            // KKpsk0 needs each side to hold the other's static key, but the
            // client chose it knowing only that it had pinned the hub's. The
            // failure is as likely to mean the hub no longer has this client's,
            // so drop the pin and let the next attempt fall back to XXpsk2.
            if handshake.pattern() == NOISE_PATTERN_KK {
                let _ = forget_noise_pin(state_dir.as_deref(), &node_id);
            }
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
            self.pin_hub_key(state_dir.as_deref(), &node_id, remote)?;
        }
        *self.state.session.lock().await = Some(Arc::new(session));

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
    fn pin_hub_key(&self, dir: Option<&Path>, node_id: &str, remote: &str) -> Result<()> {
        match load_noise_pin(dir, node_id)? {
            None => save_noise_pin(dir, node_id, remote),
            Some(pinned) if pinned == remote => Ok(()),
            Some(_) => Err(ThalovantError::Connection(
                "the hub's Noise static key changed. If the hub was not reinstalled or replaced, another machine may be answering at this address. If it was, drop the stale pin with forget_noise_pin and reconnect to trust the new key"
                    .to_string(),
            )),
        }
    }

    /// Derive, or reuse, the pre-shared key for a hub.
    async fn psk_for(&self, node_id: &str) -> Result<[u8; 32]> {
        {
            let cache = self.state.psk_cache.lock().await;
            if let Some((cached_node, psk)) = cache.as_ref() {
                if cached_node == node_id {
                    return Ok(*psk);
                }
            }
        }
        let password = &self.state.identity.password;
        if password.is_empty() {
            return Err(ThalovantError::MissingIdentityField(
                "password: the v3 Noise handshake derives its pre-shared key from it",
            ));
        }
        let psk = derive_psk(password, node_id)?;
        *self.state.psk_cache.lock().await = Some((node_id.to_string(), psk));
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
        let session = self.state.session.lock().await.clone().ok_or_else(|| {
            ThalovantError::Connection(
                "refusing to send before the v3 Noise session is established".to_string(),
            )
        })?;
        let raw = serde_json::to_vec(&message)?;
        let frames = session.encrypt_message(&raw, true)?;

        // Hold the writer across every chunk of one message: the cipher state
        // nonce counter is strictly sequential, so interleaving two messages
        // would break decryption at the hub.
        let mut writer = self.state.writer.lock().await;
        let writer = writer.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".to_string())
        })?;
        for frame in frames {
            writer
                .send(WebSocketMessage::Binary(frame))
                .await
                .map_err(|err| ThalovantError::Connection(err.to_string()))?;
        }
        Ok(())
    }

    async fn mark_error(&self, error: &ThalovantError) {
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
        health.connection.phase = TransportConnectionPhase::Error;
        health.connection.last_error = Some(error.to_string());
    }

    async fn mark_disconnected(&self) {
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
    topics: MqttTopicSet,
    client: Mutex<Option<AsyncClient>>,
    bus_tx: broadcast::Sender<Event>,
    hive_tx: broadcast::Sender<HiveMessage>,
    health: Mutex<TransportHealth>,
    event_task: Mutex<Option<JoinHandle<()>>>,
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
                identity,
                topics,
                client: Mutex::new(None),
                bus_tx,
                hive_tx,
                health: Mutex::new(TransportHealth::default()),
                event_task: Mutex::new(None),
                handshake_notify: Notify::new(),
            }),
        })
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
        let transport = self.clone();
        *self.state.event_task.lock().await = Some(tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(MqttEvent::Incoming(Packet::Publish(publish))) => {
                        if let Err(error) = transport
                            .handle_raw_mqtt_payload(publish.payload.to_vec())
                            .await
                        {
                            transport.mark_error(&error).await;
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let error = ThalovantError::Connection(error.to_string());
                        transport.mark_error(&error).await;
                        break;
                    }
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
        if let Err(error) = self
            .send_hive_message(
                hello_hive_message(&self.state.identity, "thalovant-rust-mqtt-"),
                true,
            )
            .await
        {
            self.mark_error(&error).await;
            return Err(error);
        }
        let notified = self.state.handshake_notify.notified();
        tokio::pin!(notified);
        if !self.is_handshake_complete().await {
            let _ = timeout(Duration::from_secs(6), &mut notified).await;
        }
        if !self.is_handshake_complete().await {
            self.disconnect().await?;
            let error = ThalovantError::Timeout("HiveMind MQTT handshake timed out".to_string());
            self.mark_error(&error).await;
            return Err(error);
        }
        self.mark_connection_ready(started, opened).await;
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<()> {
        if let Some(task) = self.state.event_task.lock().await.take() {
            task.abort();
        }
        if let Some(client) = self.state.client.lock().await.take() {
            let _ = client
                .publish(
                    self.state.topics.status.clone(),
                    QoS::AtLeastOnce,
                    true,
                    "offline",
                )
                .await;
            let _ = client.disconnect().await;
        }
        self.mark_disconnected().await;
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        self.state.health.lock().await.clone()
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
        self.state.health.lock().await.handshake_complete
    }

    async fn handle_raw_mqtt_payload(&self, raw: Vec<u8>) -> Result<()> {
        let (message, decoded) = decode_mqtt_hive_message(&raw)?;
        match message.msg_type.as_str() {
            "handshake" | "shake" => {
                if !truthy(message.payload.get("preshared_key"))
                    || truthy(message.payload.get("handshake"))
                    || message.payload.get("envelope").is_some()
                {
                    return Err(ThalovantError::Connection(
                        "unexpected HiveMind MQTT handshake envelope".to_string(),
                    ));
                }
            }
            "bus" => {
                let event = event_from_bus_payload(&message.payload, Some(decoded));
                let _ = self.state.bus_tx.send(event);
                return Ok(());
            }
            "query" | "cascade" => {
                let _ = self.state.hive_tx.send(message);
                return Ok(());
            }
            _ => return Ok(()),
        }
        {
            let mut health = self.state.health.lock().await;
            health.handshake_complete = true;
            health.transport_alive = true;
            self.state.handshake_notify.notify_waiters();
        }
        Ok(())
    }

    pub async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        let payload = encode_hive_binary_frame(&message)?;
        let client = self.state.client.lock().await.clone().ok_or_else(|| {
            ThalovantError::Connection("HiveMind MQTT transport is not connected".to_string())
        })?;
        let publish_qos = self
            .state
            .identity
            .mqtt
            .as_ref()
            .map(|mqtt| mqtt.qos)
            .unwrap_or(1);
        client
            .publish(
                self.state.topics.inbound.clone(),
                qos(publish_qos),
                false,
                payload,
            )
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
    }

    async fn mark_error(&self, error: &ThalovantError) {
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
        health.connection.phase = TransportConnectionPhase::Error;
        health.connection.last_error = Some(error.to_string());
    }

    async fn mark_disconnected(&self) {
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

#[derive(Debug, Deserialize)]
struct PollResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    messages: Vec<Value>,
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

fn truthy(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

fn decode_mqtt_hive_message(raw: &[u8]) -> Result<(HiveMessage, Value)> {
    if let Ok(text) = std::str::from_utf8(raw) {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if parsed.get("msg_type").is_some() {
                let message = serde_json::from_value(parsed.clone())?;
                return Ok((message, parsed));
            }
        }
    }
    let message = decode_hive_binary_frame(raw)?;
    let decoded = serde_json::to_value(&message)?;
    Ok((message, decoded))
}

/// The string entries of a JSON array, ignoring anything else in it.
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
    // TLS is the only confidentiality on this path. The identity crypto key
    // that once sealed MQTT payloads separately is gone with v3, so a broker
    // hop without TLS would put every message, and the broker password with
    // them, on the wire in the clear.
    if !tls_enabled {
        return Err(ThalovantError::Connection(
            "refusing to connect to an MQTT broker without TLS. Use an mqtts:// endpoint, or set tls: true on the identity's mqtt block"
                .to_string(),
        ));
    }
    let port = parsed.port().unwrap_or(mqtt_default_port(tls_enabled));
    let mut options = MqttOptions::new(
        format!("thalovant-{}", safe_mqtt_client_id(&identity.access_key)),
        host,
        port,
    );
    if tls_enabled {
        options.set_transport(default_mqtt_tls_transport());
    }
    Ok(options)
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

fn safe_mqtt_client_id(value: &str) -> String {
    let id = value
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || char == '_' || char == '-' {
                char
            } else {
                '-'
            }
        })
        .take(48)
        .collect::<String>();
    if id.is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        id
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
}
