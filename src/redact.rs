//! Shared secret-redaction helpers.
//!
//! These guard the two *human-facing* leak vectors — `Debug`/`{:?}` output and
//! the non-secret JSON serializers (`as_value(false)`). They must **never** be
//! wired into `Serialize`: the identity file persistence and the wire protocol
//! depend on Serialize emitting the real credential values.

use serde_json::{Map, Value};

/// Placeholder written in place of a redacted secret value.
pub(crate) const REDACTED: &str = "<redacted>";

/// Return `true` when a JSON object key names a credential that must not appear
/// in `Debug` output or a non-secret serializer.
///
/// Matching is case-insensitive and ignores `_`/`-`/other separators, so
/// `crypto_key`, `cryptoKey`, and `CRYPTO-KEY` all match. Deliberately excludes
/// identifiers that are safe to log, such as `public_key`, `user_code`, and
/// bare `key` (a common non-secret map label).
pub(crate) fn is_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "password"
            | "apikey"
            | "cryptokey"
            | "accesskey"
            | "secret"
            | "presharedkey"
            | "brokerpassword"
            | "token"
            | "accesstoken"
            | "authtoken"
            | "refreshtoken"
            | "devicecode"
            | "otpcode"
            | "recoverycode"
    )
}

/// Recursively clone `value`, replacing every secret-keyed field with
/// [`REDACTED`]. Structure and non-secret values are preserved so the result
/// stays useful for logs and error detail.
pub(crate) fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_map(map)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        other => other.clone(),
    }
}

/// Recursively clone `map`, redacting secret-keyed fields at every depth.
pub(crate) fn redact_map(map: &Map<String, Value>) -> Map<String, Value> {
    map.iter()
        .map(|(key, value)| {
            if is_secret_key(key) {
                (key.clone(), Value::String(REDACTED.to_string()))
            } else {
                (key.clone(), redact_value(value))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_known_credential_keys_at_every_depth() {
        let value = json!({
            "id": "client-1",
            "spec": {
                "apiKey": "ak-secret",
                "password": "pw-secret",
                "cryptoKey": "ck-secret",
                "siteId": "site",
            },
            "initial_identify": {
                "access_key": "ak-secret",
                "password": "pw-secret",
                "public_key": "pub-not-secret",
            },
            "mqtt": {"broker_password": "bp-secret", "endpoint": "mqtts://host"},
            "device_code": "dc-secret",
        });

        let redacted = redact_value(&value);

        assert_eq!(redacted["id"], "client-1");
        assert_eq!(redacted["spec"]["siteId"], "site");
        assert_eq!(redacted["initial_identify"]["public_key"], "pub-not-secret");
        assert_eq!(redacted["mqtt"]["endpoint"], "mqtts://host");
        for pointer in [
            "/spec/apiKey",
            "/spec/password",
            "/spec/cryptoKey",
            "/initial_identify/access_key",
            "/initial_identify/password",
            "/mqtt/broker_password",
            "/device_code",
        ] {
            assert_eq!(
                redacted.pointer(pointer).and_then(Value::as_str),
                Some(REDACTED),
                "expected {pointer} to be redacted"
            );
        }
    }

    #[test]
    fn keeps_safe_identifiers_visible() {
        assert!(!is_secret_key("public_key"));
        assert!(!is_secret_key("publicKey"));
        assert!(!is_secret_key("key"));
        assert!(!is_secret_key("user_code"));
        assert!(!is_secret_key("site_id"));
        assert!(is_secret_key("password"));
        assert!(is_secret_key("crypto_key"));
        assert!(is_secret_key("cryptoKey"));
        assert!(is_secret_key("auth_token"));
        assert!(is_secret_key("device_code"));
    }
}
