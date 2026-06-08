use crate::{
    constants::DEFAULT_USER_AGENT,
    crypto::{
        decrypt_binary, decrypt_from_json, encrypt_as_binary, encrypt_as_json, runtime_crypto_key,
    },
    errors::{Result, ThalovantError},
    events::{event_from_bus_payload, Data, Event},
    identity::Identity,
    protocols::HubProtocol,
    tls::ensure_rustls_provider,
    wire::{decode_hive_binary_frame, encode_hive_binary_frame},
};
use base64::{engine::general_purpose, Engine as _};
use futures_util::{SinkExt, StreamExt};
use rumqttc::{AsyncClient, Event as MqttEvent, LastWill, MqttOptions, Packet, QoS, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{broadcast, Mutex},
    task::JoinHandle,
    time::{sleep, Instant},
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
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
        Self {
            state: Arc::new(HttpTransportState {
                identity,
                user_agent: user_agent.into(),
                poll_interval,
                http_client: reqwest::Client::new(),
                bus_tx,
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
        let response = self
            .state
            .http_client
            .post(self.endpoint("/connect"))
            .send()
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))?;
        if !response.status().is_success() {
            return Err(ThalovantError::Connection(format!(
                "HiveMind HTTP connect status {}",
                response.status()
            )));
        }
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while !self.is_handshake_complete().await && Instant::now() < deadline {
            self.poll_once().await?;
            if !self.is_handshake_complete().await {
                sleep(Duration::from_millis(100)).await;
            }
        }
        if !self.is_handshake_complete().await {
            return Err(ThalovantError::Timeout(
                "HiveMind HTTP handshake timed out".to_string(),
            ));
        }
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
        Ok(())
    }

    pub async fn healthcheck(&self) -> TransportHealth {
        self.state.health.lock().await.clone()
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
            .map_err(|err| ThalovantError::Connection(err.to_string()))?;
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
            Value::String(raw) => {
                let parsed: Value = serde_json::from_str(&raw)?;
                if parsed.get("ciphertext").is_some() {
                    self.decrypt_message_value(parsed)?
                } else {
                    parsed
                }
            }
            Value::Object(object) if object.get("ciphertext").is_some() => {
                self.decrypt_message_value(Value::Object(object))?
            }
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
            _ => Ok(()),
        }
    }

    fn decrypt_message_value(&self, value: Value) -> Result<Value> {
        let key = self.state.identity.crypto_key.as_deref().ok_or_else(|| {
            ThalovantError::Crypto(
                "encrypted message received without identity.crypto_key".to_string(),
            )
        })?;
        let decrypted = decrypt_from_json(key, &value.to_string())?;
        Ok(serde_json::from_str(&decrypted)?)
    }

    async fn handle_handshake(&self, payload: Map<String, Value>) -> Result<()> {
        if truthy(payload.get("preshared_key"))
            && !truthy(payload.get("handshake"))
            && payload.get("envelope").is_none()
        {
            let crypto_key = self.state.identity.crypto_key.as_deref();
            if runtime_crypto_key(crypto_key).is_none() {
                return Err(ThalovantError::Connection(
                    "HiveMind requested a preshared key, but identity.crypto_key is missing"
                        .to_string(),
                ));
            }
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
            "Only HiveMind preshared-key HTTP handshakes are supported in this alpha".to_string(),
        ))
    }

    async fn send_hive_message(&self, message: HiveMessage, encrypt: bool) -> Result<()> {
        let serialized = serde_json::to_string(&message)?;
        let payload = if encrypt && self.is_handshake_complete().await {
            if let Some(key) = self.state.identity.crypto_key.as_deref() {
                encrypt_as_json(key, &serialized)?
            } else {
                serialized
            }
        } else {
            serialized
        };
        let response = self
            .state
            .http_client
            .post(self.endpoint("/send_message"))
            .form(&[("message", payload)])
            .send()
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))?;
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
    health: Mutex<TransportHealth>,
    writer: Mutex<Option<WssWriter>>,
    read_task: Mutex<Option<JoinHandle<()>>>,
}

impl WssTransport {
    pub fn new(identity: Identity) -> Self {
        ensure_rustls_provider();
        let (bus_tx, _) = broadcast::channel(64);
        Self {
            state: Arc::new(WssTransportState {
                identity,
                user_agent: DEFAULT_USER_AGENT.to_string(),
                bus_tx,
                health: Mutex::new(TransportHealth::default()),
                writer: Mutex::new(None),
                read_task: Mutex::new(None),
            }),
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.state.identity
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.state.bus_tx.subscribe()
    }

    pub async fn connect(&self) -> Result<()> {
        let endpoint_value = self
            .state
            .identity
            .endpoint_for(HubProtocol::Wss)
            .ok_or_else(|| {
                ThalovantError::UnsupportedProtocol(
                    "identity does not include a WSS endpoint".to_string(),
                )
            })?;
        let endpoint = authorized_wss_url(&endpoint_value, &self.authorization())?;
        let (stream, _) = connect_async(endpoint)
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))?;
        let (writer, mut reader) = stream.split();
        *self.state.writer.lock().await = Some(writer);
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
        }
        let transport = self.clone();
        *self.state.read_task.lock().await = Some(tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                match message {
                    Ok(WebSocketMessage::Text(payload)) => {
                        if let Err(error) =
                            transport.handle_raw_message(Value::String(payload)).await
                        {
                            transport.mark_error(error).await;
                            break;
                        }
                    }
                    Ok(WebSocketMessage::Binary(payload)) => {
                        let text = String::from_utf8_lossy(&payload).to_string();
                        if let Err(error) = transport.handle_raw_message(Value::String(text)).await
                        {
                            transport.mark_error(error).await;
                            break;
                        }
                    }
                    Ok(WebSocketMessage::Close(_)) => {
                        transport.mark_disconnected().await;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        transport
                            .mark_error(ThalovantError::Connection(error.to_string()))
                            .await;
                        break;
                    }
                }
            }
        }));
        let deadline = Instant::now() + Duration::from_secs(6);
        while !self.is_handshake_complete().await && Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
        }
        if !self.is_handshake_complete().await {
            self.disconnect().await?;
            return Err(ThalovantError::Timeout(
                "HiveMind WSS handshake timed out".to_string(),
            ));
        }
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

    async fn handle_raw_message(&self, raw: Value) -> Result<()> {
        let completed = handle_runtime_message(
            &self.state.identity,
            &self.state.bus_tx,
            raw,
            |message, encrypt| {
                let transport = self.clone();
                async move { transport.send_hive_message(message, encrypt).await }
            },
        )
        .await?;
        if completed {
            let mut health = self.state.health.lock().await;
            health.handshake_complete = true;
            health.transport_alive = true;
        }
        Ok(())
    }

    async fn send_hive_message(&self, message: HiveMessage, encrypt: bool) -> Result<()> {
        let payload = serialize_hive_message(
            &self.state.identity,
            self.is_handshake_complete().await,
            message,
            encrypt,
        )?;
        let mut writer = self.state.writer.lock().await;
        let writer = writer.as_mut().ok_or_else(|| {
            ThalovantError::Connection("HiveMind WSS transport is not connected".to_string())
        })?;
        writer
            .send(WebSocketMessage::Text(payload))
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
    }

    async fn mark_error(&self, error: ThalovantError) {
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
    }

    async fn mark_disconnected(&self) {
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MqttTopicSet {
    pub c2s: String,
    pub s2c: String,
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
    health: Mutex<TransportHealth>,
    event_task: Mutex<Option<JoinHandle<()>>>,
}

impl MqttTransport {
    pub fn new(identity: Identity) -> Result<Self> {
        ensure_rustls_provider();
        let topics = mqtt_topics_for_identity(&identity)?;
        let (bus_tx, _) = broadcast::channel(64);
        Ok(Self {
            state: Arc::new(MqttTransportState {
                identity,
                topics,
                client: Mutex::new(None),
                bus_tx,
                health: Mutex::new(TransportHealth::default()),
                event_task: Mutex::new(None),
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

    pub async fn connect(&self) -> Result<()> {
        let credentials = self.state.identity.mqtt.as_ref().ok_or_else(|| {
            ThalovantError::UnsupportedProtocol(
                "identity does not include MQTT broker credentials".to_string(),
            )
        })?;
        let mut options = mqtt_options_for_identity(&self.state.identity)?;
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
                            transport.mark_error(error).await;
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        transport
                            .mark_error(ThalovantError::Connection(error.to_string()))
                            .await;
                        break;
                    }
                }
            }
        }));
        client
            .subscribe(self.state.topics.s2c.clone(), qos(credentials.qos))
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))?;
        client
            .publish(
                self.state.topics.status.clone(),
                QoS::AtLeastOnce,
                true,
                "online",
            )
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))?;
        {
            let mut health = self.state.health.lock().await;
            health.connected = true;
            health.transport_alive = true;
        }
        self.send_hive_message(
            hello_hive_message(&self.state.identity, "thalovant-rust-mqtt-"),
            true,
        )
        .await?;
        let deadline = Instant::now() + Duration::from_secs(6);
        while !self.is_handshake_complete().await && Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
        }
        if !self.is_handshake_complete().await {
            self.disconnect().await?;
            return Err(ThalovantError::Timeout(
                "HiveMind MQTT handshake timed out".to_string(),
            ));
        }
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
        let (message, decoded) = decode_mqtt_hive_message(&self.state.identity, &raw)?;
        match message.msg_type.as_str() {
            "handshake" | "shake" => {
                if !truthy(message.payload.get("preshared_key"))
                    || truthy(message.payload.get("handshake"))
                    || message.payload.get("envelope").is_some()
                {
                    return Err(ThalovantError::Connection(
                        "Only HiveMind preshared-key MQTT handshakes are supported".to_string(),
                    ));
                }
                if runtime_crypto_key(self.state.identity.crypto_key.as_deref()).is_none() {
                    return Err(ThalovantError::Connection(
                        "HiveMind requested a preshared key, but identity.crypto_key is missing"
                            .to_string(),
                    ));
                }
            }
            "bus" => {
                let event = event_from_bus_payload(&message.payload, Some(decoded));
                let _ = self.state.bus_tx.send(event);
                return Ok(());
            }
            _ => return Ok(()),
        }
        {
            let mut health = self.state.health.lock().await;
            health.handshake_complete = true;
            health.transport_alive = true;
        }
        Ok(())
    }

    async fn send_hive_message(&self, message: HiveMessage, _encrypt: bool) -> Result<()> {
        let mut payload = encode_hive_binary_frame(&message)?;
        if let Some(key) = self.state.identity.crypto_key.as_deref() {
            if !key.trim().is_empty() {
                payload = encrypt_as_binary(key, &payload)?;
            }
        }
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
                self.state.topics.c2s.clone(),
                qos(publish_qos),
                false,
                payload,
            )
            .await
            .map_err(|err| ThalovantError::Connection(err.to_string()))
    }

    async fn mark_error(&self, error: ThalovantError) {
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.transport_alive = false;
        health.last_error = Some(error.to_string());
    }

    async fn mark_disconnected(&self) {
        let mut health = self.state.health.lock().await;
        health.connected = false;
        health.handshake_complete = false;
        health.transport_alive = false;
    }
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    messages: Vec<Value>,
}

fn truthy(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

async fn handle_runtime_message<F, Fut>(
    identity: &Identity,
    bus_tx: &broadcast::Sender<Event>,
    raw: Value,
    send: F,
) -> Result<bool>
where
    F: Fn(HiveMessage, bool) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let decoded = match raw {
        Value::String(raw) => {
            let parsed: Value = serde_json::from_str(&raw)?;
            if parsed.get("ciphertext").is_some() {
                decrypt_message_value(identity, parsed)?
            } else {
                parsed
            }
        }
        Value::Object(object) if object.get("ciphertext").is_some() => {
            decrypt_message_value(identity, Value::Object(object))?
        }
        other => other,
    };
    let message: HiveMessage = serde_json::from_value(decoded.clone())?;
    match message.msg_type.as_str() {
        "handshake" | "shake" => {
            if truthy(message.payload.get("preshared_key"))
                && !truthy(message.payload.get("handshake"))
                && message.payload.get("envelope").is_none()
            {
                if runtime_crypto_key(identity.crypto_key.as_deref()).is_none() {
                    return Err(ThalovantError::Connection(
                        "HiveMind requested a preshared key, but identity.crypto_key is missing"
                            .to_string(),
                    ));
                }
                send(hello_hive_message(identity, "thalovant-rust-"), false).await?;
                Ok(true)
            } else {
                Err(ThalovantError::Connection(
                    "Only HiveMind preshared-key handshakes are supported".to_string(),
                ))
            }
        }
        "bus" => {
            let event = event_from_bus_payload(&message.payload, Some(decoded));
            let _ = bus_tx.send(event);
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn decrypt_message_value(identity: &Identity, value: Value) -> Result<Value> {
    let key = identity.crypto_key.as_deref().ok_or_else(|| {
        ThalovantError::Crypto("encrypted message received without identity.crypto_key".to_string())
    })?;
    let decrypted = decrypt_from_json(key, &value.to_string())?;
    Ok(serde_json::from_str(&decrypted)?)
}

fn decode_mqtt_hive_message(identity: &Identity, raw: &[u8]) -> Result<(HiveMessage, Value)> {
    if let Ok(text) = std::str::from_utf8(raw) {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            if parsed.get("ciphertext").is_some() {
                let decoded = decrypt_message_value(identity, parsed)?;
                let message = serde_json::from_value(decoded.clone())?;
                return Ok((message, decoded));
            }
            if parsed.get("msg_type").is_some() {
                let message = serde_json::from_value(parsed.clone())?;
                return Ok((message, parsed));
            }
        }
    }
    if let Some(key) = identity
        .crypto_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        if let Ok(decrypted) = decrypt_binary(key, raw) {
            let message = decode_hive_binary_frame(&decrypted)?;
            let decoded = serde_json::to_value(&message)?;
            return Ok((message, decoded));
        }
    }
    let message = decode_hive_binary_frame(raw)?;
    let decoded = serde_json::to_value(&message)?;
    Ok((message, decoded))
}

fn serialize_hive_message(
    identity: &Identity,
    handshake_complete: bool,
    message: HiveMessage,
    encrypt: bool,
) -> Result<String> {
    let serialized = serde_json::to_string(&message)?;
    if encrypt && handshake_complete {
        if let Some(key) = identity.crypto_key.as_deref() {
            return encrypt_as_json(key, &serialized);
        }
    }
    Ok(serialized)
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
    let satellite_id = if credentials.hash_topics {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(identity.access_key.as_bytes());
        hex::encode(digest)[..16].to_string()
    } else {
        identity.access_key.clone()
    };
    if let (Some(c2s), Some(s2c)) = (&credentials.c2s_topic, &credentials.s2c_topic) {
        return Ok(MqttTopicSet {
            c2s: c2s.clone(),
            s2c: s2c.clone(),
            status: credentials
                .status_topic
                .clone()
                .unwrap_or_else(|| sibling_mqtt_topic(c2s, "status")),
        });
    }
    let raw = credentials
        .topic_prefix
        .as_deref()
        .map(|value| value.trim_matches('/').to_string())
        .filter(|value| !value.is_empty());
    let base = if let Some(raw) = raw {
        if raw.contains("/c2s/") {
            return Ok(MqttTopicSet {
                c2s: raw.clone(),
                s2c: sibling_mqtt_topic(&raw, "s2c"),
                status: sibling_mqtt_topic(&raw, "status"),
            });
        }
        if raw.contains("/s2c/") {
            return Ok(MqttTopicSet {
                c2s: sibling_mqtt_topic(&raw, "c2s"),
                s2c: raw.clone(),
                status: sibling_mqtt_topic(&raw, "status"),
            });
        }
        if raw.contains("/status/") {
            return Ok(MqttTopicSet {
                c2s: sibling_mqtt_topic(&raw, "c2s"),
                s2c: sibling_mqtt_topic(&raw, "s2c"),
                status: raw.clone(),
            });
        }
        let mut parts = raw.split('/').collect::<Vec<_>>();
        let last = parts.last().copied().unwrap_or_default();
        let mut base = if last == identity.access_key
            || last == credentials.username
            || last == satellite_id
        {
            parts.pop();
            parts.join("/")
        } else {
            raw
        };
        if let Some(hub_id) = credentials
            .hub_id
            .as_deref()
            .map(|value| value.trim_matches('/'))
            .filter(|value| !value.is_empty())
        {
            if !base.split('/').any(|part| part == hub_id) {
                base = format!("{base}/{hub_id}");
            }
        }
        base
    } else if let Some(hub_id) = credentials.hub_id.as_deref() {
        format!("hivemind/{}", hub_id.trim_matches('/'))
    } else {
        return Err(ThalovantError::Connection(
            "MQTT credentials must include topic_prefix, hub_id, or explicit c2s/s2c topics"
                .to_string(),
        ));
    };
    Ok(MqttTopicSet {
        c2s: format!("{base}/c2s/{satellite_id}"),
        s2c: format!("{base}/s2c/{satellite_id}"),
        status: format!("{base}/status/{satellite_id}"),
    })
}

fn sibling_mqtt_topic(topic: &str, segment: &str) -> String {
    topic
        .replacen("/c2s/", &format!("/{segment}/"), 1)
        .replacen("/s2c/", &format!("/{segment}/"), 1)
        .replacen("/status/", &format!("/{segment}/"), 1)
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
    let port = parsed.port().unwrap_or(match parsed.scheme() {
        "mqtts" | "ssl" => 8883,
        _ => 1883,
    });
    let mut options = MqttOptions::new(
        format!("thalovant-{}", safe_mqtt_client_id(&identity.access_key)),
        host,
        port,
    );
    if parsed.scheme() == "mqtts" || parsed.scheme() == "ssl" {
        options.set_transport(Transport::tls_with_default_config());
    }
    Ok(options)
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
