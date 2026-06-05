use crate::events::Context;
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default)]
pub struct ClientContextOptions {
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub auth_token: Option<String>,
    pub auth_provider: Option<String>,
    pub auth_claims: Option<Map<String, Value>>,
    pub roles: Vec<String>,
    pub platform: Option<String>,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub channel: Option<String>,
    pub device_id: Option<String>,
    pub locale: Option<String>,
    pub metadata: Option<Map<String, Value>>,
    pub session_id: Option<String>,
}

pub fn build_client_context(base: Option<&Context>, opts: ClientContextOptions) -> Context {
    let mut context = base.cloned().unwrap_or_default();
    if opts.user_id.is_some() || opts.user_name.is_some() || !opts.roles.is_empty() {
        let mut user = map_from_value(context.get("user"));
        if let Some(value) = opts.user_id {
            user.insert("id".to_string(), Value::String(value.clone()));
            context
                .entry("user_id".to_string())
                .or_insert(Value::String(value));
        }
        if let Some(value) = opts.user_name {
            user.insert("name".to_string(), Value::String(value.clone()));
            context
                .entry("user_name".to_string())
                .or_insert(Value::String(value));
        }
        if !opts.roles.is_empty() {
            let roles = Value::Array(opts.roles.iter().cloned().map(Value::String).collect());
            user.insert("roles".to_string(), roles.clone());
            context.entry("roles".to_string()).or_insert(roles);
        }
        context.insert("user".to_string(), Value::Object(user));
    }
    if opts.auth_token.is_some() || opts.auth_provider.is_some() || opts.auth_claims.is_some() {
        let mut auth = map_from_value(context.get("auth"));
        if let Some(value) = opts.auth_token {
            auth.insert("token".to_string(), Value::String(value.clone()));
            context
                .entry("auth_token".to_string())
                .or_insert(Value::String(value));
        }
        if let Some(value) = opts.auth_provider {
            auth.insert("provider".to_string(), Value::String(value));
        }
        if let Some(value) = opts.auth_claims {
            auth.insert("claims".to_string(), Value::Object(value));
        }
        context.insert("auth".to_string(), Value::Object(auth));
    }
    set_default(&mut context, "platform", opts.platform);
    set_default(&mut context, "source", opts.source);
    set_default(&mut context, "destination", opts.destination);
    set_default(&mut context, "channel", opts.channel);
    set_default(&mut context, "locale", opts.locale);
    if let Some(device_id) = opts.device_id {
        let mut device = map_from_value(context.get("device"));
        device.insert("id".to_string(), Value::String(device_id));
        if let Some(Value::String(platform)) = context.get("platform") {
            device
                .entry("platform".to_string())
                .or_insert(Value::String(platform.clone()));
        }
        context.insert("device".to_string(), Value::Object(device));
    }
    if let Some(metadata) = opts.metadata {
        let mut existing = map_from_value(context.get("metadata"));
        existing.extend(metadata);
        context.insert("metadata".to_string(), Value::Object(existing));
    }
    if let Some(session_id) = opts.session_id {
        let mut session = map_from_value(context.get("session"));
        session.insert("session_id".to_string(), Value::String(session_id.clone()));
        context
            .entry("session_id".to_string())
            .or_insert(Value::String(session_id));
        context.insert("session".to_string(), Value::Object(session));
    }
    context
}

fn set_default(context: &mut Context, key: &str, value: Option<String>) {
    if let Some(value) = value {
        context
            .entry(key.to_string())
            .or_insert(Value::String(value));
    }
}

fn map_from_value(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_generic_client_context() {
        let context = build_client_context(
            None,
            ClientContextOptions {
                user_id: Some("u-1".to_string()),
                user_name: Some("Ada".to_string()),
                auth_token: Some("token".to_string()),
                auth_provider: Some("oidc".to_string()),
                roles: vec!["operator".to_string()],
                platform: Some("mobile".to_string()),
                source: Some("device-1".to_string()),
                channel: Some("chat".to_string()),
                device_id: Some("phone-1".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(context["user"]["name"], Value::String("Ada".to_string()));
        assert_eq!(
            context["auth"]["provider"],
            Value::String("oidc".to_string())
        );
        assert_eq!(
            context["device"]["platform"],
            Value::String("mobile".to_string())
        );
    }
}
