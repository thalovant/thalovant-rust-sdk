use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubProtocol {
    Wss,
    Https,
    Mqtt,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubProtocolSettings {
    pub wss: bool,
    pub http: bool,
    pub mqtt: bool,
}

impl Default for HubProtocolSettings {
    fn default() -> Self {
        Self {
            wss: true,
            http: false,
            mqtt: false,
        }
    }
}

impl HubProtocolSettings {
    pub fn from_value(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::default();
        };
        Self::from_object(object)
    }

    pub fn from_object(object: &Map<String, Value>) -> Self {
        let source = object
            .get("spec")
            .and_then(Value::as_object)
            .unwrap_or(object);
        let protocols = source.get("protocols").and_then(Value::as_object);
        let network = source.get("network").and_then(Value::as_object);
        Self {
            wss: enabled_value(first_value(protocols, network, &["wss", "websocket"]), true),
            http: enabled_value(first_value(protocols, network, &["http", "https"]), false),
            mqtt: enabled_value(first_value(protocols, network, &["mqtt"]), false),
        }
    }

    pub fn enabled_protocols(&self) -> Vec<HubProtocol> {
        let mut enabled = Vec::with_capacity(3);
        if self.wss {
            enabled.push(HubProtocol::Wss);
        }
        if self.http {
            enabled.push(HubProtocol::Https);
        }
        if self.mqtt {
            enabled.push(HubProtocol::Mqtt);
        }
        enabled
    }

    pub fn is_enabled(&self, protocol: HubProtocol) -> bool {
        match protocol {
            HubProtocol::Wss => self.wss,
            HubProtocol::Https => self.http,
            HubProtocol::Mqtt => self.mqtt,
        }
    }

    pub fn as_spec_value(&self) -> Value {
        json!({
            "wss": {"enabled": self.wss},
            "http": {"enabled": self.http},
            "mqtt": {"enabled": self.mqtt},
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubDataPlaneEndpoints {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub https: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wss: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<String>,
}

impl HubDataPlaneEndpoints {
    pub fn from_value(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::default();
        };
        Self::from_object(object)
    }

    pub fn from_object(object: &Map<String, Value>) -> Self {
        let source = object
            .get("data_plane_endpoints")
            .or_else(|| object.get("dataPlaneEndpoints"))
            .or_else(|| object.get("endpoints"))
            .and_then(Value::as_object)
            .unwrap_or(object);
        Self {
            https: normalize_endpoint(first_string(source, &["https", "http"]).as_deref()),
            wss: normalize_endpoint(first_string(source, &["wss", "ws"]).as_deref()),
            mqtt: normalize_endpoint(first_string(source, &["mqtt", "mqtts"]).as_deref()),
        }
    }

    pub fn from_hub(value: &Value) -> Self {
        let mut endpoints = Self::from_value(value);
        let protocols = HubProtocolSettings::from_value(value);
        let domain = value
            .as_object()
            .and_then(|object| object.get("domain"))
            .and_then(string_value);
        if let Some(domain) = domain {
            if endpoints.https.is_none() && protocols.http {
                endpoints.https = endpoint_from_domain(&domain, HubProtocol::Https);
            }
            if endpoints.wss.is_none() && protocols.wss {
                endpoints.wss = endpoint_from_domain(&domain, HubProtocol::Wss);
            }
        }
        endpoints
    }

    pub fn endpoint_for(&self, protocol: HubProtocol) -> Option<&str> {
        match protocol {
            HubProtocol::Https => self.https.as_deref(),
            HubProtocol::Wss => self.wss.as_deref(),
            HubProtocol::Mqtt => self.mqtt.as_deref(),
        }
    }

    pub fn http_base(
        &self,
        fallback_master: &str,
        fallback_port: u16,
        fallback_path: &str,
    ) -> String {
        if let Some(https) = self.https.as_deref() {
            return endpoint_base(https, fallback_port, "");
        }
        let master = fallback_master
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1);
        endpoint_base(&master, fallback_port, fallback_path)
    }

    pub fn as_map(&self, redact_credentials: bool) -> Map<String, Value> {
        let mut data = Map::new();
        for (key, endpoint) in [
            ("https", self.https.as_deref()),
            ("wss", self.wss.as_deref()),
            ("mqtt", self.mqtt.as_deref()),
        ] {
            if let Some(endpoint) = endpoint {
                let value = if redact_credentials {
                    redact_endpoint_credentials(endpoint)
                } else {
                    endpoint.to_string()
                };
                data.insert(key.to_string(), Value::String(value));
            }
        }
        data
    }
}

pub fn endpoint_from_domain(domain: &str, protocol: HubProtocol) -> Option<String> {
    let normalized = domain.trim().trim_end_matches('/');
    match protocol {
        HubProtocol::Wss => {
            if normalized.starts_with("wss://") || normalized.starts_with("ws://") {
                normalize_endpoint(Some(normalized))
            } else if normalized.starts_with("https://") || normalized.starts_with("http://") {
                normalize_endpoint(Some(&replace_any_prefix(
                    normalized,
                    &[("https://", "wss://"), ("http://", "wss://")],
                )))
            } else {
                normalize_endpoint(Some(&format!("wss://{normalized}")))
            }
        }
        HubProtocol::Https => {
            if normalized.starts_with("https://") || normalized.starts_with("http://") {
                normalize_endpoint(Some(&replace_any_prefix(
                    normalized,
                    &[("http://", "https://")],
                )))
            } else if normalized.starts_with("wss://") || normalized.starts_with("ws://") {
                normalize_endpoint(Some(&replace_any_prefix(
                    normalized,
                    &[("wss://", "https://"), ("ws://", "https://")],
                )))
            } else {
                normalize_endpoint(Some(&format!("https://{normalized}")))
            }
        }
        HubProtocol::Mqtt => None,
    }
}

pub fn endpoint_base(master: &str, default_port: u16, default_path: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(master) {
        if url.port().is_none() {
            let _ = url.set_port(Some(default_port));
        }
        url.set_path(&join_url_path(url.path(), default_path));
        url.set_query(None);
        url.set_fragment(None);
        return url.as_str().trim_end_matches('/').to_string();
    }
    format!(
        "{}:{}{}",
        master.trim_end_matches('/'),
        default_port,
        default_path
    )
}

fn first_value<'a>(
    primary: Option<&'a Map<String, Value>>,
    secondary: Option<&'a Map<String, Value>>,
    keys: &[&str],
) -> Option<&'a Value> {
    for source in [primary, secondary].into_iter().flatten() {
        for key in keys {
            if let Some(value) = source.get(*key) {
                return Some(value);
            }
        }
    }
    None
}

fn enabled_value(value: Option<&Value>, fallback: bool) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => fallback,
        },
        Some(Value::Object(object)) => enabled_value(object.get("enabled"), fallback),
        _ => fallback,
    }
}

fn first_string(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = object.get(*key).and_then(string_value) {
            return Some(value);
        }
    }
    None
}

fn string_value(value: &Value) -> Option<String> {
    match value {
        Value::String(raw) => {
            let normalized = raw.trim();
            (!normalized.is_empty()).then(|| normalized.to_string())
        }
        Value::Null => None,
        other => Some(other.to_string().trim_matches('"').to_string()),
    }
}

fn normalize_endpoint(value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(raw).ok()?;
    match parsed.scheme() {
        "http" | "https" | "ws" | "wss" | "mqtt" | "mqtts" => {
            Some(raw.trim_end_matches('/').to_string())
        }
        _ => None,
    }
}

fn replace_any_prefix(value: &str, replacements: &[(&str, &str)]) -> String {
    for (prefix, replacement) in replacements {
        if let Some(rest) = value.strip_prefix(prefix) {
            return format!("{replacement}{rest}");
        }
    }
    value.to_string()
}

fn redact_endpoint_credentials(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return endpoint.to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.as_str().trim_end_matches('/').to_string()
}

fn join_url_path(base: &str, extra: &str) -> String {
    let path = [base.trim_matches('/'), extra.trim_matches('/')]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    }
}
