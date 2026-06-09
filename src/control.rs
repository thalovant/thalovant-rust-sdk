use crate::{
    errors::{Result, ThalovantError},
    identity::Identity,
    protocols::{
        endpoint_from_domain, select_data_plane_endpoint, HubDataPlaneEndpoints, HubProtocol,
        HubProtocolSettings, SelectedHubEndpoint, DEFAULT_PROTOCOL_PREFERENCE,
    },
    tls::ensure_rustls_provider,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{rngs::OsRng, RngCore};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde_json::{json, Map, Value};
use uuid::Uuid;

const DEFAULT_CONTROL_USER_AGENT: &str = "thalovant-rust-sdk/0.2.6";

#[derive(Clone)]
pub struct ControlPlane {
    pub api_url: String,
    pub access_token: Option<String>,
    pub user_agent: String,
    http_client: reqwest::Client,
}

#[derive(Clone, Debug, Default)]
pub struct BootstrapIdentityOptions {
    pub name: String,
    pub site_id: Option<String>,
    pub spec: Map<String, Value>,
    pub owner_id: Option<String>,
    pub active: Option<bool>,
    pub preferred_protocols: Vec<HubProtocol>,
    pub idempotency_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BootstrapIdentityResult {
    pub identity: Identity,
    pub hub: Value,
    pub client: Value,
    pub endpoint: Option<SelectedHubEndpoint>,
}

impl ControlPlane {
    pub fn new(api_url: impl Into<String>, access_token: Option<String>) -> Self {
        ensure_rustls_provider();
        Self {
            api_url: format!("{}/", api_url.into().trim_end_matches('/')),
            access_token,
            user_agent: DEFAULT_CONTROL_USER_AGENT.to_string(),
            http_client: reqwest::Client::new(),
        }
    }

    pub async fn login(
        &mut self,
        email: impl Into<String>,
        password: impl Into<String>,
        scope: Option<String>,
    ) -> Result<Value> {
        let mut body = Map::from_iter([
            ("email".to_string(), Value::String(email.into())),
            ("password".to_string(), Value::String(password.into())),
        ]);
        if let Some(scope) = scope.filter(|value| !value.trim().is_empty()) {
            body.insert("scope".to_string(), Value::String(scope));
        }
        let token = self
            .request(
                "POST",
                "/v1/auth/token",
                Some(Value::Object(body)),
                None,
                false,
            )
            .await?;
        let access_token = token
            .get("access_token")
            .and_then(json_string)
            .ok_or_else(|| {
                ThalovantError::Api("token response did not include access_token".to_string())
            })?;
        self.access_token = Some(access_token);
        Ok(token)
    }

    pub async fn list_hubs(
        &self,
        limit: Option<u32>,
        cursor: Option<&str>,
        owner_id: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![format!("limit={}", limit.unwrap_or(100))];
        if let Some(cursor) = cursor {
            params.push(format!("cursor={}", urlencoding::encode(cursor)));
        }
        if let Some(owner_id) = owner_id {
            params.push(format!("owner_id={}", urlencoding::encode(owner_id)));
        }
        self.request(
            "GET",
            &format!("/v1/hubs?{}", params.join("&")),
            None,
            None,
            true,
        )
        .await
    }

    pub async fn list_public_hubs(
        &self,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![format!("limit={}", limit.unwrap_or(24))];
        if let Some(cursor) = cursor {
            params.push(format!("cursor={}", urlencoding::encode(cursor)));
        }
        self.request(
            "GET",
            &format!("/v1/public/hubs?{}", params.join("&")),
            None,
            None,
            false,
        )
        .await
    }

    pub async fn get_hub(&self, hub_id: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/v1/hubs/{}", urlencoding::encode(hub_id)),
            None,
            None,
            true,
        )
        .await
    }

    pub async fn get_public_hub(&self, hub_ref: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/v1/public/hubs/{}", urlencoding::encode(hub_ref)),
            None,
            None,
            false,
        )
        .await
    }

    pub async fn create_client(
        &self,
        payload: Value,
        idempotency_key: Option<String>,
    ) -> Result<Value> {
        let mut headers = HeaderMap::new();
        let key = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
        headers.insert(
            "Idempotency-Key",
            HeaderValue::from_str(&key).map_err(|err| ThalovantError::Api(err.to_string()))?,
        );
        self.request("POST", "/v1/clients", Some(payload), Some(headers), true)
            .await
    }

    pub async fn create_client_identity_for_hub_id(
        &self,
        hub_id: &str,
        opts: BootstrapIdentityOptions,
    ) -> Result<BootstrapIdentityResult> {
        let hub = self.get_hub(hub_id).await?;
        self.create_client_identity(hub, opts).await
    }

    pub async fn create_client_identity(
        &self,
        hub: Value,
        opts: BootstrapIdentityOptions,
    ) -> Result<BootstrapIdentityResult> {
        if opts.name.trim().is_empty() {
            return Err(ThalovantError::Api("client name is required".to_string()));
        }
        let hub_id = hub
            .get("id")
            .and_then(json_string)
            .ok_or_else(|| ThalovantError::Api("hub resource is missing id".to_string()))?;
        let site_id = clean_site_id(opts.site_id.as_deref().unwrap_or(&opts.name));
        let api_key = new_secret();
        let password = new_secret();
        let crypto_key = new_secret();

        let mut spec = opts.spec.clone();
        spec.entry("version".to_string())
            .or_insert_with(|| Value::String("1".to_string()));
        spec.insert("apiKey".to_string(), Value::String(api_key.clone()));
        spec.insert("password".to_string(), Value::String(password.clone()));
        spec.insert("cryptoKey".to_string(), Value::String(crypto_key.clone()));
        spec.insert("siteId".to_string(), Value::String(site_id.clone()));

        let mut payload = Map::from_iter([
            ("hub_id".to_string(), Value::String(hub_id)),
            ("name".to_string(), Value::String(opts.name.clone())),
            ("spec".to_string(), Value::Object(spec)),
            (
                "active".to_string(),
                Value::Bool(opts.active.unwrap_or(true)),
            ),
        ]);
        if let Some(owner_id) = opts.owner_id {
            payload.insert("owner_id".to_string(), Value::String(owner_id));
        }

        let client = self
            .create_client(Value::Object(payload), opts.idempotency_key)
            .await?;
        let protocols = HubProtocolSettings::from_value(&hub);
        let endpoints = HubDataPlaneEndpoints::from_hub(&hub);
        let preferred = if opts.preferred_protocols.is_empty() {
            DEFAULT_PROTOCOL_PREFERENCE
        } else {
            opts.preferred_protocols.as_slice()
        };
        let endpoint = select_data_plane_endpoint(&endpoints, &protocols, preferred);
        let default_master = default_master(&hub, &endpoints, endpoint.as_ref())?;
        let identity = if let Some(initial_identify) =
            client.get("initial_identify").and_then(Value::as_object)
        {
            let mut identity = initial_identify.clone();
            identity.insert(
                "data_plane_endpoints".to_string(),
                Value::Object(endpoints.as_map(false)),
            );
            identity.insert("protocols".to_string(), protocols.as_spec_value());
            Identity::from_value(Value::Object(identity))?
        } else {
            Identity {
                access_key: api_key,
                password,
                crypto_key: Some(crypto_key),
                site_id,
                default_master,
                default_port: 443,
                default_path: String::new(),
                public_key: None,
                metadata: Map::new(),
                name: Some(opts.name),
                data_plane_endpoints: endpoints,
                protocols,
                mqtt: None,
            }
        };
        Ok(BootstrapIdentityResult {
            identity,
            hub,
            client,
            endpoint,
        })
    }

    pub fn require_runtime_protocol(
        &self,
        result: &BootstrapIdentityResult,
        protocol: HubProtocol,
    ) -> Result<SelectedHubEndpoint> {
        if protocol == HubProtocol::Mqtt && result.identity.mqtt.is_none() {
            return Err(ThalovantError::UnsupportedProtocol(
                "MQTT is enabled, but the API did not return client-scoped MQTT broker credentials"
                    .to_string(),
            ));
        }
        let endpoint = result.identity.endpoint_for(protocol).ok_or_else(|| {
            ThalovantError::UnsupportedProtocol(format!(
                "this hub does not expose a {protocol:?} endpoint for the SDK runtime"
            ))
        })?;
        Ok(SelectedHubEndpoint { protocol, endpoint })
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: Option<HeaderMap>,
        auth: bool,
    ) -> Result<Value> {
        let mut request_headers = HeaderMap::new();
        request_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        request_headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent)
                .map_err(|err| ThalovantError::Api(err.to_string()))?,
        );
        if body.is_some() {
            request_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        if let Some(headers) = headers {
            request_headers.extend(headers);
        }
        if auth {
            let token = self
                .access_token
                .as_ref()
                .ok_or_else(|| ThalovantError::Api("missing access token".to_string()))?;
            request_headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|err| ThalovantError::Api(err.to_string()))?,
            );
        }
        let method = method
            .parse::<reqwest::Method>()
            .map_err(|err| ThalovantError::Api(err.to_string()))?;
        let url = format!("{}{}", self.api_url, path.trim_start_matches('/'));
        let mut request = self
            .http_client
            .request(method, url)
            .headers(request_headers);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|err| ThalovantError::Api(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ThalovantError::Api(format!("HTTP {status}: {body}")));
        }
        response.json::<Value>().await.map_err(ThalovantError::from)
    }
}

impl BootstrapIdentityResult {
    pub fn selected_protocol(&self) -> Option<HubProtocol> {
        self.endpoint.as_ref().map(|endpoint| endpoint.protocol)
    }

    pub fn as_value(&self, include_secrets: bool) -> Value {
        let mut identity = Map::from_iter([
            (
                "site_id".to_string(),
                Value::String(self.identity.site_id.clone()),
            ),
            (
                "default_master".to_string(),
                Value::String(self.identity.default_master.clone()),
            ),
            (
                "default_port".to_string(),
                Value::from(self.identity.default_port),
            ),
            (
                "default_path".to_string(),
                Value::String(self.identity.default_path.clone()),
            ),
        ]);
        let endpoints = self.identity.data_plane_endpoints.as_map(!include_secrets);
        if !endpoints.is_empty() {
            identity.insert("data_plane_endpoints".to_string(), Value::Object(endpoints));
        }
        if !self.identity.metadata.is_empty() {
            identity.insert(
                "metadata".to_string(),
                Value::Object(self.identity.metadata.clone()),
            );
        }
        if include_secrets {
            identity.insert(
                "access_key".to_string(),
                Value::String(self.identity.access_key.clone()),
            );
            identity.insert(
                "password".to_string(),
                Value::String(self.identity.password.clone()),
            );
            if let Some(crypto_key) = self.identity.crypto_key.clone() {
                identity.insert("crypto_key".to_string(), Value::String(crypto_key));
            }
        }
        if let Some(mqtt) = self.identity.mqtt.as_ref() {
            identity.insert("mqtt".to_string(), mqtt.as_value(include_secrets));
        }
        json!({
            "identity": identity,
            "hub": self.hub,
            "client": self.client,
            "selected_protocol": self.selected_protocol(),
            "selected_endpoint": self.endpoint.as_ref().map(|endpoint| endpoint.endpoint.clone()),
        })
    }
}

fn new_secret() -> String {
    let mut raw = [0_u8; 32];
    OsRng.fill_bytes(&mut raw);
    general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn clean_site_id(value: &str) -> String {
    let cleaned = value
        .trim()
        .replace('_', "-")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-");
    if cleaned.is_empty() {
        "thalovant-client".to_string()
    } else {
        cleaned
    }
}

fn default_master(
    hub: &Value,
    endpoints: &HubDataPlaneEndpoints,
    selected: Option<&SelectedHubEndpoint>,
) -> Result<String> {
    if let Some(https) = endpoints.https.as_deref() {
        return Ok(strip_endpoint_path(https));
    }
    if let Some(domain) = hub.get("domain").and_then(json_string) {
        if let Some(endpoint) = endpoint_from_domain(&domain, HubProtocol::Https) {
            return Ok(endpoint);
        }
    }
    if let Some(selected) = selected {
        return Ok(strip_endpoint_path(&selected.endpoint));
    }
    Err(ThalovantError::Api(
        "hub resource does not expose a usable data-plane endpoint".to_string(),
    ))
}

fn strip_endpoint_path(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return endpoint.trim_end_matches('/').to_string();
    };
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').to_string()
}

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => {
            let normalized = raw.trim();
            (!normalized.is_empty()).then(|| normalized.to_string())
        }
        Value::Null => None,
        other => Some(other.to_string().trim_matches('"').to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[tokio::test]
    async fn public_hub_discovery_does_not_send_auth() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0_u8; 4096];
                let size = stream.read(&mut buffer).expect("read request");
                let request = String::from_utf8_lossy(&buffer[..size]);
                assert!(
                    !request.to_ascii_lowercase().contains("\r\nauthorization:"),
                    "public hub requests must not send Authorization"
                );
                let body = if request.starts_with("GET /v1/public/hubs?limit=12 ") {
                    r#"{"data":[{"id":"hub-public","name":"joke-garden","slug":"joke-garden","title":"Joke Garden"}],"meta":{"count":1,"next":null},"links":{"next":null}}"#
                } else if request.starts_with("GET /v1/public/hubs/joke-garden ") {
                    r#"{"id":"hub-public","name":"joke-garden","slug":"joke-garden","title":"Joke Garden"}"#
                } else {
                    panic!("unexpected request: {request}");
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write response");
            }
        });

        let control = ControlPlane::new(format!("http://{address}"), Some("token".to_string()));
        let page = control
            .list_public_hubs(Some(12), None)
            .await
            .expect("list public hubs");
        let hub = control
            .get_public_hub("joke-garden")
            .await
            .expect("get public hub");

        assert_eq!(page["data"][0]["slug"].as_str(), Some("joke-garden"));
        assert_eq!(hub["title"].as_str(), Some("Joke Garden"));
        server.join().expect("test server finished");
    }
}
