use crate::{
    errors::{Result, ThalovantError},
    protocols::{HubDataPlaneEndpoints, HubProtocol, HubProtocolSettings},
};
use serde::Serialize;
use serde_json::{Map, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG_FILENAME: &str = "config.yaml";

pub fn default_config_path() -> Result<PathBuf> {
    if let Ok(config_home) = env::var("XDG_CONFIG_HOME") {
        if !config_home.trim().is_empty() {
            return Ok(PathBuf::from(config_home)
                .join("thalovant")
                .join(DEFAULT_CONFIG_FILENAME));
        }
    }
    if cfg!(windows) {
        if let Ok(appdata) = env::var("APPDATA") {
            if !appdata.trim().is_empty() {
                return Ok(PathBuf::from(appdata)
                    .join("Thalovant")
                    .join(DEFAULT_CONFIG_FILENAME));
            }
        }
    }
    let home = env::var("HOME").map_err(|_| {
        ThalovantError::InvalidIdentity("unable to resolve home directory".to_string())
    })?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("thalovant")
        .join(DEFAULT_CONFIG_FILENAME))
}

// `Debug` is hand-written (below) to redact `password` and `username` (the
// account access key); `Serialize` stays derived so the identity file and wire
// protocol keep the real values.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct MqttBrokerCredentials {
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub topic_prefix: Option<String>,
    pub hub_id: Option<String>,
    pub c2s_topic: Option<String>,
    pub s2c_topic: Option<String>,
    pub status_topic: Option<String>,
    pub hash_topics: bool,
    pub qos: u8,
    pub tls: bool,
}

impl MqttBrokerCredentials {
    pub fn from_value(value: Option<&Value>) -> Option<Self> {
        let object = value?.as_object()?;
        let endpoint = optional_string(object, "endpoint", &["broker_url", "brokerUrl"])
            .ok()
            .flatten()?;
        let username = optional_string(object, "username", &["broker_username", "brokerUsername"])
            .ok()
            .flatten()?;
        let password = optional_string(object, "password", &["broker_password", "brokerPassword"])
            .ok()
            .flatten()?;
        let topic_prefix = optional_string(object, "topic_prefix", &["topicPrefix"])
            .ok()
            .flatten();
        let hub_id = optional_string(object, "hub_id", &["hubId"]).ok().flatten();
        let c2s_topic = optional_string(object, "c2s_topic", &["c2sTopic"])
            .ok()
            .flatten();
        let s2c_topic = optional_string(object, "s2c_topic", &["s2cTopic"])
            .ok()
            .flatten();
        let status_topic = optional_string(object, "status_topic", &["statusTopic"])
            .ok()
            .flatten();
        let hash_topics = optional_bool(object, "hash_topics")
            .or_else(|| optional_bool(object, "hashTopics"))
            .unwrap_or(false);
        let qos = optional_u8(object, "qos").unwrap_or(1).min(1);
        let tls = optional_bool(object, "tls").unwrap_or_else(|| endpoint.starts_with("mqtts://"));
        Some(Self {
            endpoint,
            username,
            password,
            topic_prefix,
            hub_id,
            c2s_topic,
            s2c_topic,
            status_topic,
            hash_topics,
            qos,
            tls,
        })
    }

    pub fn as_value(&self, include_secrets: bool) -> Value {
        let mut data = Map::from_iter([
            ("endpoint".to_string(), Value::String(self.endpoint.clone())),
            ("tls".to_string(), Value::Bool(self.tls)),
        ]);
        if include_secrets {
            data.insert("username".to_string(), Value::String(self.username.clone()));
            data.insert("password".to_string(), Value::String(self.password.clone()));
            if let Some(topic_prefix) = self.topic_prefix.clone() {
                data.insert("topic_prefix".to_string(), Value::String(topic_prefix));
            }
            if let Some(hub_id) = self.hub_id.clone() {
                data.insert("hub_id".to_string(), Value::String(hub_id));
            }
            if let Some(topic) = self.c2s_topic.clone() {
                data.insert("c2s_topic".to_string(), Value::String(topic));
            }
            if let Some(topic) = self.s2c_topic.clone() {
                data.insert("s2c_topic".to_string(), Value::String(topic));
            }
            if let Some(topic) = self.status_topic.clone() {
                data.insert("status_topic".to_string(), Value::String(topic));
            }
            if self.hash_topics {
                data.insert("hash_topics".to_string(), Value::Bool(true));
            }
            if self.qos != 1 {
                data.insert(
                    "qos".to_string(),
                    Value::Number(serde_json::Number::from(self.qos)),
                );
            }
        }
        Value::Object(data)
    }
}

impl fmt::Debug for MqttBrokerCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MqttBrokerCredentials")
            .field("endpoint", &self.endpoint)
            // The broker username is the account access key, which the identity's
            // own Debug redacts; keep them consistent so `{:?}` never exposes it.
            .field("username", &crate::redact::REDACTED)
            .field("password", &crate::redact::REDACTED)
            .field("topic_prefix", &self.topic_prefix)
            .field("hub_id", &self.hub_id)
            .field("c2s_topic", &self.c2s_topic)
            .field("s2c_topic", &self.s2c_topic)
            .field("status_topic", &self.status_topic)
            .field("hash_topics", &self.hash_topics)
            .field("qos", &self.qos)
            .field("tls", &self.tls)
            .finish()
    }
}

// `Debug` is hand-written (below) to redact `access_key`, `password`, and
// `crypto_key`; `Serialize` stays derived so identity-file persistence and the
// wire protocol keep emitting the real credential values.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub access_key: String,
    pub password: String,
    pub crypto_key: Option<String>,
    pub site_id: String,
    pub default_master: String,
    pub default_port: u16,
    pub default_path: String,
    pub public_key: Option<String>,
    pub metadata: Map<String, Value>,
    pub name: Option<String>,
    pub data_plane_endpoints: HubDataPlaneEndpoints,
    pub protocols: HubProtocolSettings,
    pub mqtt: Option<MqttBrokerCredentials>,
}

impl Identity {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        assert_secure_identity_file(path)?;
        let raw = fs::read_to_string(path)?;
        Self::from_json(&raw)
    }

    pub fn from_config(profile: Option<&str>) -> Result<Self> {
        Self::from_config_file(default_config_path()?, profile)
    }

    pub fn from_config_file(path: impl AsRef<Path>, profile: Option<&str>) -> Result<Self> {
        let path = path.as_ref();
        assert_secure_config_file(path)?;
        let raw = fs::read_to_string(path)?;
        let value: Value = serde_yaml::from_str(&raw).map_err(|err| {
            ThalovantError::InvalidIdentity(format!(
                "Thalovant config file is not valid YAML: {err}"
            ))
        })?;
        let object = value.as_object().ok_or_else(|| {
            ThalovantError::InvalidIdentity(
                "Thalovant config file must contain a YAML object".to_string(),
            )
        })?;
        Self::from_value(Value::Object(
            identity_config_object(object, profile)?.clone(),
        ))
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
            metadata: object
                .get("metadata")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            name: optional_string(object, "name", &[])?,
            data_plane_endpoints: HubDataPlaneEndpoints::from_object(object),
            protocols: HubProtocolSettings::from_object(object),
            mqtt: MqttBrokerCredentials::from_value(object.get("mqtt")),
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
            ("MQTT_ENDPOINT", "mqtt_endpoint"),
            ("MQTT_USERNAME", "mqtt_username"),
            ("MQTT_PASSWORD", "mqtt_password"),
            ("MQTT_TOPIC_PREFIX", "mqtt_topic_prefix"),
            ("MQTT_HUB_ID", "mqtt_hub_id"),
            ("MQTT_C2S_TOPIC", "mqtt_c2s_topic"),
            ("MQTT_S2C_TOPIC", "mqtt_s2c_topic"),
            ("MQTT_STATUS_TOPIC", "mqtt_status_topic"),
            ("MQTT_HASH_TOPICS", "mqtt_hash_topics"),
            ("MQTT_QOS", "mqtt_qos"),
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
        let mqtt_endpoint = object.remove("mqtt_endpoint").or_else(|| {
            object
                .get("data_plane_endpoints")
                .and_then(Value::as_object)
                .and_then(|endpoints| endpoints.get("mqtt"))
                .cloned()
        });
        let mqtt_username = object.remove("mqtt_username");
        let mqtt_password = object.remove("mqtt_password");
        let mqtt_topic_prefix = object.remove("mqtt_topic_prefix");
        let mqtt_hub_id = object.remove("mqtt_hub_id");
        let mqtt_c2s_topic = object.remove("mqtt_c2s_topic");
        let mqtt_s2c_topic = object.remove("mqtt_s2c_topic");
        let mqtt_status_topic = object.remove("mqtt_status_topic");
        let mqtt_hash_topics = object.remove("mqtt_hash_topics");
        let mqtt_qos = object.remove("mqtt_qos");
        let mut mqtt = Map::new();
        if let Some(value) = mqtt_endpoint {
            mqtt.insert("endpoint".to_string(), value);
        }
        if let Some(value) = mqtt_username {
            mqtt.insert("username".to_string(), value);
        }
        if let Some(value) = mqtt_password {
            mqtt.insert("password".to_string(), value);
        }
        if let Some(value) = mqtt_topic_prefix {
            mqtt.insert("topic_prefix".to_string(), value);
        }
        if let Some(value) = mqtt_hub_id {
            mqtt.insert("hub_id".to_string(), value);
        }
        if let Some(value) = mqtt_c2s_topic {
            mqtt.insert("c2s_topic".to_string(), value);
        }
        if let Some(value) = mqtt_s2c_topic {
            mqtt.insert("s2c_topic".to_string(), value);
        }
        if let Some(value) = mqtt_status_topic {
            mqtt.insert("status_topic".to_string(), value);
        }
        if let Some(value) = mqtt_hash_topics {
            mqtt.insert("hash_topics".to_string(), value);
        }
        if let Some(value) = mqtt_qos {
            mqtt.insert("qos".to_string(), value);
        }
        if !mqtt.is_empty() {
            object.insert("mqtt".to_string(), Value::Object(mqtt));
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

impl fmt::Debug for Identity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Identity")
            .field("access_key", &crate::redact::REDACTED)
            .field("password", &crate::redact::REDACTED)
            .field(
                "crypto_key",
                &self.crypto_key.as_ref().map(|_| crate::redact::REDACTED),
            )
            .field("site_id", &self.site_id)
            .field("default_master", &self.default_master)
            .field("default_port", &self.default_port)
            .field("default_path", &self.default_path)
            .field("public_key", &self.public_key)
            .field("metadata", &self.metadata)
            .field("name", &self.name)
            .field("data_plane_endpoints", &self.data_plane_endpoints)
            .field("protocols", &self.protocols)
            // MqttBrokerCredentials' own Debug redacts its password.
            .field("mqtt", &self.mqtt)
            .finish()
    }
}

fn required_string(
    object: &Map<String, Value>,
    key: &'static str,
    aliases: &[&str],
) -> Result<String> {
    optional_string(object, key, aliases)?.ok_or(ThalovantError::MissingIdentityField(key))
}

fn identity_config_object<'a>(
    object: &'a Map<String, Value>,
    profile: Option<&str>,
) -> Result<&'a Map<String, Value>> {
    if let Some(profiles) = object.get("profiles").and_then(Value::as_object) {
        let profile_name = profile
            .map(str::to_string)
            .or(optional_string(
                object,
                "profile",
                &["default_profile", "defaultProfile"],
            )?)
            .unwrap_or_else(|| "default".to_string());
        let selected = profiles
            .get(&profile_name)
            .and_then(Value::as_object)
            .ok_or_else(|| {
                ThalovantError::InvalidIdentity(format!(
                    "missing Thalovant config profile: {profile_name}"
                ))
            })?;
        return profile_identity_object(selected);
    }
    profile_identity_object(object)
}

fn profile_identity_object(object: &Map<String, Value>) -> Result<&Map<String, Value>> {
    if let Some(identity) = object.get("identity").and_then(Value::as_object) {
        return Ok(identity);
    }
    Ok(object)
}

fn assert_secure_config_file(path: &Path) -> Result<()> {
    assert_secure_secret_file(path, "Thalovant config file")
}

fn assert_secure_identity_file(path: &Path) -> Result<()> {
    assert_secure_secret_file(path, "identity file")
}

fn assert_secure_secret_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::metadata(path)?;
    #[cfg(unix)]
    {
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ThalovantError::InvalidIdentity(format!(
                "{} is too permissive: {}. Run `chmod 600 {}`.",
                capitalize(description),
                path.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
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

fn optional_bool(object: &Map<String, Value>, key: &str) -> Option<bool> {
    match object.get(key)? {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => Some(value.as_i64().unwrap_or(0) != 0),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn optional_u8(object: &Map<String, Value>, key: &str) -> Option<u8> {
    match object.get(key)? {
        Value::Number(value) => value.as_u64().and_then(|value| u8::try_from(value).ok()),
        Value::String(value) => value.trim().parse::<u8>().ok(),
        _ => None,
    }
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
    use crate::{
        client::Client,
        control::BootstrapIdentityResult,
        protocols::{select_data_plane_endpoint, SelectedHubEndpoint},
        transport::{mqtt_topics_for_identity, RuntimeTransport},
    };
    use serde_json::{json, Value};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn temp_config_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "thalovant-{name}-{}.yaml",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn temp_identity_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "thalovant-{name}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[test]
    fn normalizes_identity_aliases() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "cryptoKey": "crypto",
            "site": "site",
            "host": "https://hub.example.com/",
            "port": "443",
            "path": "/hivemind/public",
            "metadata": {"thalovant_owner_id": "owner-1"}
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
        assert_eq!(identity.metadata["thalovant_owner_id"], "owner-1");
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
    fn identity_loads_mqtt_credentials_and_redacts_by_default() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "site": "site",
            "host": "wss://hub.example.com",
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "access",
                "password": "broker-password",
                "topic_prefix": "hivemind/hub/access"
            }
        }))
        .unwrap();

        let mqtt = identity.mqtt.as_ref().expect("mqtt credentials");
        assert_eq!(mqtt.endpoint, "mqtts://mqtt.example.com:8883");
        assert_eq!(mqtt.username, "access");
        assert_eq!(
            mqtt.as_value(false),
            json!({"endpoint": "mqtts://mqtt.example.com:8883", "tls": true})
        );
        assert_eq!(
            mqtt.as_value(true),
            json!({
                "endpoint": "mqtts://mqtt.example.com:8883",
                "tls": true,
                "username": "access",
                "password": "broker-password",
                "topic_prefix": "hivemind/hub/access"
            })
        );
    }

    #[test]
    fn identity_loads_private_json_file() {
        let path = temp_identity_path("identity");
        fs::write(
            &path,
            r#"{
                "access_key": "access",
                "password": "secret",
                "site_id": "site",
                "default_master": "https://hub.example.com",
                "default_port": 443
            }"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&path, permissions).unwrap();
        }

        let identity = Identity::from_file(&path).unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(identity.access_key, "access");
        assert_eq!(identity.default_master, "https://hub.example.com");
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_permissive_json_file() {
        let path = temp_identity_path("insecure");
        fs::write(
            &path,
            r#"{"access_key":"access","password":"secret","site_id":"site","default_master":"https://hub.example.com"}"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();

        let error = Identity::from_file(&path).unwrap_err();
        let _ = fs::remove_file(&path);

        assert!(error.to_string().contains("too permissive"));
    }

    #[test]
    fn identity_loads_yaml_config_profile() {
        let path = temp_config_path("config");
        fs::write(
            &path,
            r#"
version: 1
profile: prod
profiles:
  prod:
    identity:
      access_key: access
      password: secret
      site_id: site
      default_master: https://hub.example.com
      default_port: 443
      mqtt:
        endpoint: mqtts://mqtt.example.com:8883
        username: access
        password: broker-password
        topic_prefix: hivemind/hub/access
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&path, permissions).unwrap();
        }

        let identity = Identity::from_config_file(&path, None).unwrap();
        let _ = fs::remove_file(&path);

        assert_eq!(identity.access_key, "access");
        assert_eq!(
            identity.mqtt.as_ref().map(|mqtt| mqtt.password.as_str()),
            Some("broker-password")
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_rejects_permissive_config_file() {
        let path = temp_config_path("insecure");
        fs::write(&path, "identity: {}\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&path, permissions).unwrap();

        let error = Identity::from_config_file(&path, None).unwrap_err();
        let _ = fs::remove_file(&path);

        assert!(error.to_string().contains("too permissive"));
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

    #[test]
    fn selects_first_enabled_endpoint_from_preference() {
        let endpoints = HubDataPlaneEndpoints {
            https: Some("https://hub.example.com/public".to_string()),
            wss: Some("wss://hub.example.com/public".to_string()),
            mqtt: None,
        };
        let selected = select_data_plane_endpoint(
            &endpoints,
            &HubProtocolSettings {
                wss: true,
                http: true,
                mqtt: false,
            },
            &[HubProtocol::Mqtt, HubProtocol::Wss, HubProtocol::Https],
        )
        .unwrap();

        assert_eq!(selected.protocol, HubProtocol::Wss);
        assert_eq!(selected.endpoint, "wss://hub.example.com/public");
    }

    #[test]
    fn client_requires_mqtt_credentials_for_mqtt_runtime() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "site": "site",
            "host": "https://hub.example.com"
        }))
        .unwrap();

        assert!(Client::with_protocol(identity, HubProtocol::Mqtt).is_err());
    }

    #[test]
    fn client_selects_wss_and_mqtt_runtime_transports() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "crypto_key": "0123456789abcdef",
            "site": "site",
            "host": "https://hub.example.com",
            "data_plane_endpoints": {
                "https": "https://hub.example.com",
                "wss": "wss://hub.example.com",
                "mqtt": "mqtts://mqtt.example.com:8883"
            },
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "access",
                "password": "broker-password",
                "topic_prefix": "hivemind/hub/access"
            }
        }))
        .unwrap();

        assert!(Client::with_protocol(identity.clone(), HubProtocol::Wss).is_ok());
        assert!(Client::with_protocol(identity.clone(), HubProtocol::Mqtt).is_ok());
        let auto_client = Client::auto(identity.clone()).unwrap();
        assert!(matches!(auto_client.transport, RuntimeTransport::Wss(_)));
        assert_eq!(
            mqtt_topics_for_identity(&identity).unwrap(),
            crate::transport::MqttTopicSet {
                c2s: "hivemind/hub/c2s/access".to_string(),
                s2c: "hivemind/hub/s2c/access".to_string(),
                status: "hivemind/hub/status/access".to_string(),
            }
        );
    }

    #[test]
    fn client_falls_back_to_https_when_wss_endpoint_is_missing() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "site": "site",
            "host": "https://hub.example.com"
        }))
        .unwrap();

        let auto_client = Client::auto(identity).unwrap();
        assert!(matches!(auto_client.transport, RuntimeTransport::Http(_)));
    }

    #[test]
    fn mqtt_topics_append_hub_id_for_scoped_acls() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "site": "site",
            "host": "https://hub.example.com",
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "access",
                "password": "broker-password",
                "topic_prefix": "hivemind",
                "hub_id": "hub-1"
            }
        }))
        .unwrap();

        assert_eq!(
            mqtt_topics_for_identity(&identity).unwrap(),
            crate::transport::MqttTopicSet {
                c2s: "hivemind/hub-1/c2s/access".to_string(),
                s2c: "hivemind/hub-1/s2c/access".to_string(),
                status: "hivemind/hub-1/status/access".to_string(),
            }
        );
    }

    #[test]
    fn bootstrap_summary_redacts_secrets_by_default() {
        let identity = Identity::from_value(json!({
            "key": "access",
            "password": "secret",
            "cryptoKey": "crypto",
            "site": "site",
            "host": "https://hub.example.com",
            "protocols": {"http": {"enabled": true}},
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "access",
                "password": "broker-password"
            }
        }))
        .unwrap();
        let result = BootstrapIdentityResult {
            identity,
            hub: json!({"id": "hub-1"}),
            client: json!({"id": "client-1"}),
            endpoint: Some(SelectedHubEndpoint {
                protocol: HubProtocol::Https,
                endpoint: "https://hub.example.com".to_string(),
            }),
        };

        assert!(result.as_value(false)["identity"]
            .get("access_key")
            .is_none());
        assert!(result.as_value(false)["identity"]["mqtt"]
            .get("password")
            .is_none());
        assert_eq!(
            result.as_value(true)["identity"]["access_key"],
            Value::String("access".to_string())
        );
        assert_eq!(
            result.as_value(true)["identity"]["mqtt"]["password"],
            Value::String("broker-password".to_string())
        );
    }

    #[test]
    fn identity_debug_redacts_secrets_but_serialize_roundtrips() {
        let identity = Identity::from_value(json!({
            "access_key": "ak-LIVE-SECRET",
            "password": "pw-LIVE-SECRET",
            "crypto_key": "ck-LIVE-SECRET",
            "site_id": "site",
            "default_master": "https://hub.example.com",
            "default_port": 443,
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "ak-LIVE-SECRET",
                "password": "broker-LIVE-SECRET"
            }
        }))
        .unwrap();

        // `{:?}` must never leak a secret, but stays useful for non-secret fields.
        let debug = format!("{identity:?}");
        for secret in [
            "ak-LIVE-SECRET",
            "pw-LIVE-SECRET",
            "ck-LIVE-SECRET",
            "broker-LIVE-SECRET",
        ] {
            assert!(!debug.contains(secret), "Debug leaked {secret}: {debug}");
        }
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("site"));

        // Serialize MUST keep the real values: identity-file persistence and the
        // wire protocol depend on it, so redaction must never touch Serialize.
        let serialized = serde_json::to_value(&identity).unwrap();
        assert_eq!(serialized["access_key"], "ak-LIVE-SECRET");
        assert_eq!(serialized["password"], "pw-LIVE-SECRET");
        assert_eq!(serialized["crypto_key"], "ck-LIVE-SECRET");
        assert_eq!(serialized["mqtt"]["password"], "broker-LIVE-SECRET");

        // ...and a Serialize -> parse round-trip recovers the same secrets.
        let restored = Identity::from_value(serialized).unwrap();
        assert_eq!(restored.access_key, "ak-LIVE-SECRET");
        assert_eq!(restored.password, "pw-LIVE-SECRET");
        assert_eq!(restored.crypto_key.as_deref(), Some("ck-LIVE-SECRET"));
        assert_eq!(
            restored.mqtt.as_ref().map(|mqtt| mqtt.password.as_str()),
            Some("broker-LIVE-SECRET")
        );
    }

    #[test]
    fn mqtt_credentials_debug_redacts_password_but_serialize_keeps_it() {
        let mqtt = Identity::from_value(json!({
            "access_key": "access",
            "password": "secret",
            "site_id": "site",
            "default_master": "https://hub.example.com",
            "mqtt": {
                "endpoint": "mqtts://mqtt.example.com:8883",
                "username": "access",
                "password": "broker-LIVE-SECRET"
            }
        }))
        .unwrap()
        .mqtt
        .expect("mqtt credentials");

        let debug = format!("{mqtt:?}");
        assert!(
            !debug.contains("broker-LIVE-SECRET"),
            "Debug leaked: {debug}"
        );
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("mqtts://mqtt.example.com:8883"));
        assert_eq!(
            serde_json::to_value(&mqtt).unwrap()["password"],
            "broker-LIVE-SECRET"
        );
    }
}
