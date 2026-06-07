use crate::{
    errors::{Result, ThalovantError},
    protocols::{HubDataPlaneEndpoints, HubProtocol, HubProtocolSettings},
};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{env, fs, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub access_key: String,
    pub password: String,
    pub crypto_key: Option<String>,
    pub site_id: String,
    pub default_master: String,
    pub default_port: u16,
    pub default_path: String,
    pub public_key: Option<String>,
    pub name: Option<String>,
    pub data_plane_endpoints: HubDataPlaneEndpoints,
    pub protocols: HubProtocolSettings,
}

impl Identity {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        Self::from_json(&raw)
    }

    pub fn from_json(raw: &str) -> Result<Self> {
        Self::from_value(serde_json::from_str(raw)?)
    }

    pub fn from_value(value: Value) -> Result<Self> {
        let object = value.as_object().ok_or_else(|| {
            ThalovantError::InvalidIdentity("identity must be a JSON object".to_string())
        })?;
        let access_key = required_string(object, "access_key", &["accessKey", "key"])?;
        let password = required_string(object, "password", &["secret"])?;
        let site_id = required_string(object, "site_id", &["siteId", "site"])?;
        let default_master = required_string(
            object,
            "default_master",
            &["defaultMaster", "host", "master"],
        )?
        .trim_end_matches('/')
        .to_string();
        let default_port =
            optional_port(object, "default_port", &["defaultPort", "port"])?.unwrap_or(443);
        let default_path = normalize_path(optional_string(
            object,
            "default_path",
            &["defaultPath", "hub_http_path", "path", "uri_path"],
        )?);
        Ok(Self {
            access_key,
            password,
            crypto_key: optional_string(object, "crypto_key", &["cryptoKey", "preshared_key"])?,
            site_id,
            default_master,
            default_port,
            default_path,
            public_key: optional_string(object, "public_key", &["publicKey"])?,
            name: optional_string(object, "name", &[])?,
            data_plane_endpoints: HubDataPlaneEndpoints::from_object(object),
            protocols: HubProtocolSettings::from_object(object),
        })
    }

    pub fn from_env() -> Result<Self> {
        Self::from_env_prefix("THALOVANT_")
    }

    pub fn from_env_prefix(prefix: &str) -> Result<Self> {
        let mut object = Map::new();
        for (env_key, field) in [
            ("ACCESS_KEY", "access_key"),
            ("PASSWORD", "password"),
            ("CRYPTO_KEY", "crypto_key"),
            ("SITE_ID", "site_id"),
            ("DEFAULT_MASTER", "default_master"),
            ("HUB_HTTP_HOST", "default_master"),
            ("DEFAULT_PORT", "default_port"),
            ("DEFAULT_PATH", "default_path"),
            ("HUB_HTTP_PATH", "default_path"),
            ("HUB_HTTPS_HOST", "hub_https_host"),
            ("HUB_WSS_HOST", "hub_wss_host"),
            ("HUB_WEBSOCKET_HOST", "hub_websocket_host"),
            ("HUB_MQTT_HOST", "hub_mqtt_host"),
            ("PUBLIC_KEY", "public_key"),
            ("NAME", "name"),
        ] {
            if let Ok(value) = env::var(format!("{prefix}{env_key}")) {
                object.insert(field.to_string(), Value::String(value));
            }
        }
        let mut endpoints = Map::new();
        if let Some(value) = object
            .remove("hub_https_host")
            .or_else(|| object.get("default_master").cloned())
        {
            endpoints.insert("https".to_string(), value);
        }
        if let Some(value) = object
            .remove("hub_wss_host")
            .or_else(|| object.remove("hub_websocket_host"))
        {
            endpoints.insert("wss".to_string(), value);
        }
        if let Some(value) = object.remove("hub_mqtt_host") {
            endpoints.insert("mqtt".to_string(), value);
        }
        if !endpoints.is_empty() {
            object.insert("data_plane_endpoints".to_string(), Value::Object(endpoints));
        }
        Self::from_value(Value::Object(object))
    }

    pub fn base_url(&self) -> String {
        self.data_plane_endpoints.http_base(
            &self.default_master,
            self.default_port,
            &self.default_path,
        )
    }

    pub fn endpoint_for(&self, protocol: HubProtocol) -> Option<String> {
        if protocol == HubProtocol::Https {
            return Some(self.base_url());
        }
        self.data_plane_endpoints
            .endpoint_for(protocol)
            .map(str::to_string)
    }

    pub fn enabled_protocols(&self) -> Vec<HubProtocol> {
        self.protocols.enabled_protocols()
    }

    pub fn supports_protocol(&self, protocol: HubProtocol) -> bool {
        self.protocols.is_enabled(protocol)
    }
}

fn required_string(
    object: &Map<String, Value>,
    key: &'static str,
    aliases: &[&str],
) -> Result<String> {
    optional_string(object, key, aliases)?.ok_or(ThalovantError::MissingIdentityField(key))
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    aliases: &[&str],
) -> Result<Option<String>> {
    for candidate in std::iter::once(key).chain(aliases.iter().copied()) {
        if let Some(value) = object.get(candidate) {
            return match value {
                Value::String(raw) => {
                    let normalized = raw.trim();
                    Ok((!normalized.is_empty()).then(|| normalized.to_string()))
                }
                Value::Null => Ok(None),
                other => Ok(Some(other.to_string().trim_matches('"').to_string())),
            };
        }
    }
    Ok(None)
}

fn optional_port(object: &Map<String, Value>, key: &str, aliases: &[&str]) -> Result<Option<u16>> {
    let Some(raw) = optional_string(object, key, aliases)? else {
        return Ok(None);
    };
    raw.parse::<u16>()
        .map(Some)
        .map_err(|_| ThalovantError::InvalidIdentity(format!("invalid default_port: {raw}")))
}

fn normalize_path(path: Option<String>) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let normalized = path.trim_matches('/');
    if normalized.is_empty() {
        String::new()
    } else {
        format!("/{normalized}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_identity_aliases() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "cryptoKey": "crypto",
            "site": "site",
            "host": "https://hub.example.com/",
            "port": "443",
            "path": "/hivemind/public"
        }))
        .unwrap();
        assert_eq!(identity.access_key, "access");
        assert_eq!(identity.default_master, "https://hub.example.com");
        assert_eq!(identity.default_port, 443);
        assert_eq!(identity.default_path, "/hivemind/public");
        assert_eq!(
            identity.base_url(),
            "https://hub.example.com/hivemind/public"
        );
    }

    #[test]
    fn identity_uses_protocol_aware_data_plane_endpoints() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "site": "site",
            "host": "wss://hub.example.com",
            "port": 443,
            "path": "/hivemind/public",
            "data_plane_endpoints": {
                "https": "https://api.example.com/hivemind/public",
                "wss": "wss://socket.example.com/hivemind/public",
                "mqtt": "mqtts://mqtt.example.com:8883"
            },
            "protocols": {
                "wss": {"enabled": true},
                "http": {"enabled": true},
                "mqtt": {"enabled": true}
            }
        }))
        .unwrap();

        assert_eq!(
            identity.base_url(),
            "https://api.example.com/hivemind/public"
        );
        assert_eq!(
            identity.endpoint_for(HubProtocol::Wss).as_deref(),
            Some("wss://socket.example.com/hivemind/public")
        );
        assert_eq!(
            identity.endpoint_for(HubProtocol::Mqtt).as_deref(),
            Some("mqtts://mqtt.example.com:8883")
        );
        assert_eq!(
            identity.enabled_protocols(),
            vec![HubProtocol::Wss, HubProtocol::Https, HubProtocol::Mqtt]
        );
        assert!(identity.supports_protocol(HubProtocol::Https));
    }

    #[test]
    fn data_plane_endpoints_can_be_derived_from_hub_resource() {
        let endpoints = HubDataPlaneEndpoints::from_hub(&json!({
            "domain": "jokes.thalovant.io",
            "spec": {
                "protocols": {
                    "wss": {"enabled": true},
                    "http": {"enabled": true},
                    "mqtt": {"enabled": false}
                }
            }
        }));

        assert_eq!(endpoints.wss.as_deref(), Some("wss://jokes.thalovant.io"));
        assert_eq!(
            endpoints.https.as_deref(),
            Some("https://jokes.thalovant.io")
        );
        assert_eq!(endpoints.mqtt, None);
    }
}
