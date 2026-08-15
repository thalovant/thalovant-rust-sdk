//! Shared secret-redaction helpers.
//!
//! These guard the two *human-facing* leak vectors — `Debug`/`{:?}` output and
//! the non-secret JSON serializers (`as_value(false)`). They must **never** be
//! wired into `Serialize`: the identity file persistence and the wire protocol
//! depend on Serialize emitting the real credential values.

use serde_json::{Map, Value};

/// Placeholder written in place of a redacted secret value.
pub(crate) const REDACTED: &str = "<redacted>";

/// Normalize a key for matching: keep ASCII alphanumerics only, lowercased, so
/// `crypto_key`, `cryptoKey`, and `CRYPTO-KEY` all compare equal.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Return `true` when a JSON object key names a credential that must not appear
/// in `Debug` output or a non-secret serializer.
///
/// Matching is case-insensitive and ignores `_`/`-`/other separators, so
/// `crypto_key`, `cryptoKey`, and `CRYPTO-KEY` all match. Deliberately excludes
/// identifiers that are safe to log, such as `public_key`, `user_code`, and
/// bare `key` (a common non-secret map label). Bare `key` is still redacted when
/// it appears inside an identity bundle, where it aliases the access key — see
/// [`redact_value`].
pub(crate) fn is_secret_key(key: &str) -> bool {
    matches!(
        normalize_key(key).as_str(),
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

/// Return `true` when a key's *value* is a serialized identity bundle in which a
/// bare `key` field aliases the access key (`Identity::from_value` accepts
/// `access_key`, `accessKey`, or `key`). Inside such an object `key` is a
/// secret; everywhere else it stays visible to avoid redacting unrelated fields.
fn is_identity_bundle(key: &str) -> bool {
    matches!(normalize_key(key).as_str(), "initialidentify" | "identity")
}

/// Recursively clone `value`, replacing every secret-keyed field with
/// [`REDACTED`]. Structure and non-secret values are preserved so the result
/// stays useful for logs and error detail. Inside an identity bundle (e.g. a
/// `/v1/clients` `initial_identify` object) a bare `key` is redacted too, since
/// it aliases the access key.
pub(crate) fn redact_value(value: &Value) -> Value {
    redact_value_scoped(value, false)
}

/// Recursively clone `map`, redacting secret-keyed fields at every depth.
pub(crate) fn redact_map(map: &Map<String, Value>) -> Map<String, Value> {
    redact_map_scoped(map, false)
}

fn redact_value_scoped(value: &Value, in_identity_bundle: bool) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_map_scoped(map, in_identity_bundle)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_value_scoped(item, in_identity_bundle))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn redact_map_scoped(map: &Map<String, Value>, in_identity_bundle: bool) -> Map<String, Value> {
    map.iter()
        .map(|(key, value)| {
            let key_aliases_access_key = in_identity_bundle && normalize_key(key) == "key";
            if is_secret_key(key) || key_aliases_access_key {
                (key.clone(), Value::String(REDACTED.to_string()))
            } else {
                // Descend, entering identity-bundle scope when this field's value
                // is one, so a nested bare `key` (the access-key alias) redacts.
                let child_scope = in_identity_bundle || is_identity_bundle(key);
                (key.clone(), redact_value_scoped(value, child_scope))
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

    #[test]
    fn redacts_bare_key_only_inside_an_identity_bundle() {
        let value = json!({
            "key": "top-level-not-secret",
            "spec": {"key": "spec-not-secret", "apiKey": "ak-secret"},
            "initial_identify": {
                "key": "ak-LIVE-ALIAS",
                "password": "pw-secret",
                "site_id": "site",
                "public_key": "pub-not-secret"
            }
        });

        let redacted = redact_value(&value);

        // A bare `key` outside an identity bundle stays visible (no false positives).
        assert_eq!(redacted["key"], "top-level-not-secret");
        assert_eq!(redacted["spec"]["key"], "spec-not-secret");
        assert_eq!(redacted["spec"]["apiKey"], REDACTED);
        // Inside `initial_identify`, `key` aliases the access key -> redacted.
        assert_eq!(redacted["initial_identify"]["key"], REDACTED);
        assert_eq!(redacted["initial_identify"]["password"], REDACTED);
        assert_eq!(redacted["initial_identify"]["site_id"], "site");
        assert_eq!(redacted["initial_identify"]["public_key"], "pub-not-secret");
    }
}
