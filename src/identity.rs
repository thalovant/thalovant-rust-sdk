use crate::errors::{Result, ThalovantError};
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
            ("DEFAULT_PORT", "default_port"),
            ("DEFAULT_PATH", "default_path"),
            ("HUB_HTTP_PATH", "default_path"),
            ("PUBLIC_KEY", "public_key"),
            ("NAME", "name"),
        ] {
            if let Ok(value) = env::var(format!("{prefix}{env_key}")) {
                object.insert(field.to_string(), Value::String(value));
            }
        }
        Self::from_value(Value::Object(object))
    }

    pub fn base_url(&self) -> String {
        let mut host = self.default_master.clone();
        if host.starts_with("wss://") {
            host = host.replacen("wss://", "https://", 1);
        } else if host.starts_with("ws://") {
            host = host.replacen("ws://", "http://", 1);
        }
        if let Ok(mut url) = reqwest::Url::parse(&host) {
            if url.port().is_none() {
                let _ = url.set_port(Some(self.default_port));
            }
            url.set_path(&join_url_path(url.path(), &self.default_path));
            url.set_query(None);
            url.set_fragment(None);
            return url.as_str().trim_end_matches('/').to_string();
        }
        format!(
            "{}:{}{}",
            host.trim_end_matches('/'),
            self.default_port,
            self.default_path
        )
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
}
