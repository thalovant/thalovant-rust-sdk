use crate::{
    constants::DEFAULT_USER_AGENT,
    crypto::{decrypt_from_json, encrypt_as_json, runtime_crypto_key},
    errors::{Result, ThalovantError},
    events::{event_from_bus_payload, Data, Event},
    identity::Identity,
};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, Mutex},
    task::JoinHandle,
    time::{sleep, Instant},
};

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
            "handshake" => self.handle_handshake(message.payload).await,
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
