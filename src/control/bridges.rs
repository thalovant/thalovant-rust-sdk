//! The `/v1/bridges` owner routes: chat apps, a hub answering in a chat
//! network. The types are in [`crate::chat_apps`].

use super::*;
use crate::chat_apps::{
    exposed_secrets, secret_map, Bridge, BridgeAvatar, BridgeChatList, BridgeCreate,
    BridgeCredentials, BridgeListOptions, BridgeNetworks, BridgeTestResult, BridgeUpdate,
    ChatAppConnector, SecretValue,
};

/// The most bridges one list asks for.
const MAX_BRIDGE_LIST_LIMIT: u32 = 500;

/// A `{data: [...]}` answer.
#[derive(Deserialize)]
struct DataEnvelope<T> {
    data: Vec<T>,
}

/// The path of one bridge, or of one of its routes, with its id escaped.
/// A blank id is refused: `/v1/bridges/` is the list route.
fn bridge_path(bridge_id: &str, route: &str) -> Result<String> {
    let id = required("bridge_id", bridge_id)?;
    Ok(format!("/v1/bridges/{}{route}", urlencoding::encode(id)))
}

/// A required argument, refused locally when blank.
fn required<'a>(name: &str, value: &'a str) -> Result<&'a str> {
    if value.trim().is_empty() {
        return Err(ThalovantError::Api(format!("{name} is required")));
    }
    Ok(value)
}

/// A secrets map for a request, refused locally when it is empty.
fn secrets_body(secrets: &BTreeMap<String, SecretValue>) -> Result<Value> {
    if secrets.is_empty() {
        return Err(ThalovantError::Api(
            "secrets must name at least one secret".into(),
        ));
    }
    Ok(exposed_secrets(secrets))
}

impl ControlPlane {
    /// List the chat apps a bridge can connect to (`GET /v1/bridges/connectors`,
    /// `hubs:read`): each one's copy, settings form, secrets, chat ids and
    /// helpers. Hidden ones are left out; `offered` says whether a bridge can
    /// be created for it now.
    pub async fn list_chat_app_connectors(&self) -> Result<Vec<ChatAppConnector>> {
        let value = self
            .request("GET", "/v1/bridges/connectors", None, None, true)
            .await?;
        Ok(serde_json::from_value::<DataEnvelope<_>>(value)?.data)
    }

    /// Which networks are offered, and how many bridges the account may add
    /// (`GET /v1/bridges/networks`, `hubs:read`).
    ///
    /// Read the allowance before creating a bridge: a Free plan allows none
    /// (`paid_plan` false, and creating answers 402). With `hub_id` it is the
    /// hub owner's allowance, since bridges count against the owner whoever
    /// adds them; without it, the caller's own.
    pub async fn get_bridge_networks(&self, hub_id: Option<&str>) -> Result<BridgeNetworks> {
        let mut params = Vec::new();
        push_query_param(&mut params, "hub_id", hub_id);
        let path = if params.is_empty() {
            "/v1/bridges/networks".to_string()
        } else {
            format!("/v1/bridges/networks?{}", params.join("&"))
        };
        let value = self.request("GET", &path, None, None, true).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// List bridges (`GET /v1/bridges`, `hubs:read`): one hub's, or every
    /// bridge the caller may see, oldest first.
    ///
    /// A `limit` outside 1 to 500 fails locally with
    /// [`ThalovantError::Api`] before any request.
    pub async fn list_bridges(&self, opts: &BridgeListOptions) -> Result<Vec<Bridge>> {
        let mut params = Vec::new();
        push_query_param(&mut params, "hub_id", opts.hub_id.as_deref());
        push_query_param(&mut params, "owner_id", opts.owner_id.as_deref());
        if let Some(limit) = opts.limit {
            if !(1..=MAX_BRIDGE_LIST_LIMIT).contains(&limit) {
                return Err(ThalovantError::Api(format!(
                    "limit must be from 1 to {MAX_BRIDGE_LIST_LIMIT}"
                )));
            }
            params.push(format!("limit={limit}"));
        }
        let path = if params.is_empty() {
            "/v1/bridges".to_string()
        } else {
            format!("/v1/bridges?{}", params.join("&"))
        };
        let value = self.request("GET", &path, None, None, true).await?;
        Ok(serde_json::from_value::<DataEnvelope<_>>(value)?.data)
    }

    /// Connect a chat app to a hub (`POST /v1/bridges`, `hubs:write`, a paid
    /// plan with room in its allowance).
    ///
    /// The API checks the credentials (with the network, for a connector
    /// that has the `validate` helper) and stores them; the bridge starts in
    /// [`BridgeState::Setup`](crate::BridgeState::Setup) and runs once its
    /// chats are chosen with [`ControlPlane::update_bridge`]. The credentials
    /// are read only to build the request body, which is never put in an
    /// error. A blank hub, connector or token, or an empty secrets map,
    /// fails locally with [`ThalovantError::Api`] before any request.
    pub async fn create_bridge(&self, bridge: BridgeCreate) -> Result<Bridge> {
        let mut body = Map::new();
        body.insert(
            "hub_id".into(),
            Value::from(required("hub_id", &bridge.hub_id)?),
        );
        body.insert(
            "connector".into(),
            Value::from(required("connector", &bridge.connector)?),
        );
        match &bridge.credentials {
            BridgeCredentials::Token(token) => {
                if token.is_blank() {
                    return Err(ThalovantError::Api("token is required".into()));
                }
                body.insert("token".into(), Value::from(token.expose()));
            }
            BridgeCredentials::Secrets(secrets) => {
                body.insert("secrets".into(), secrets_body(secrets)?);
            }
        }
        if let Some(settings) = bridge.settings {
            body.insert("settings".into(), Value::Object(settings));
        }
        let value = self
            .request("POST", "/v1/bridges", Some(Value::Object(body)), None, true)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Read one bridge, with its status (`GET /v1/bridges/{id}`, `hubs:read`).
    pub async fn get_bridge(&self, bridge_id: &str) -> Result<Bridge> {
        let value = self
            .request("GET", &bridge_path(bridge_id, "")?, None, None, true)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Change a bridge (`PATCH /v1/bridges/{id}`, `hubs:write`): its chats,
    /// its mention rule, its session scope, its settings, or pause and
    /// resume it. Only the fields that are `Some` are sent.
    ///
    /// With [`BridgeUpdate::etag`] the change is sent with `If-Match`, and
    /// fails with HTTP 412 if the bridge changed since it was read; a blank
    /// etag fails locally with [`ThalovantError::Api`]. Pausing is always
    /// allowed; anything else needs a paid plan, and resuming needs room in
    /// the plan's allowance (403 `plan_limit`).
    pub async fn update_bridge(&self, bridge_id: &str, update: &BridgeUpdate) -> Result<Bridge> {
        let path = bridge_path(bridge_id, "")?;
        let headers = match update.etag.as_deref() {
            Some(etag) if etag.trim().is_empty() => {
                return Err(ThalovantError::Api("etag must not be blank".into()));
            }
            Some(etag) => Some(single_header("If-Match", etag)?),
            None => None,
        };
        let body = serde_json::to_value(update)?;
        let value = self
            .request("PATCH", &path, Some(body), headers, true)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Replace a bridge's bot token (`PUT /v1/bridges/{id}/token`,
    /// `hubs:write`), for a connector with one `token` secret. The new token
    /// must belong to the same bot (409 `bridge_token_other_bot` otherwise:
    /// make a new bridge). A blank token fails locally.
    pub async fn replace_bridge_token(
        &self,
        bridge_id: &str,
        token: impl Into<SecretValue>,
    ) -> Result<Bridge> {
        let path = bridge_path(bridge_id, "/token")?;
        let token = token.into();
        if token.is_blank() {
            return Err(ThalovantError::Api("token is required".into()));
        }
        let body = json!({ "token": token.expose() });
        let value = self.request("PUT", &path, Some(body), None, true).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Replace some of a bridge's secrets (`PUT /v1/bridges/{id}/secrets`,
    /// `hubs:write`); the keys not sent stay as stored. An empty map fails
    /// locally.
    pub async fn replace_bridge_secrets<I, K, V>(
        &self,
        bridge_id: &str,
        secrets: I,
    ) -> Result<Bridge>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<SecretValue>,
    {
        let path = bridge_path(bridge_id, "/secrets")?;
        let body = json!({ "secrets": secrets_body(&secret_map(secrets))? });
        let value = self.request("PUT", &path, Some(body), None, true).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// The chats a bridge's bot can answer in, to choose from
    /// (`GET /v1/bridges/{id}/chats`, `hubs:write`).
    ///
    /// For a connector without the `discover` helper the list is empty and
    /// `manual_entry` is true: the owner types the ids. Telegram lists chats
    /// only while its bridge is stopped (409 `bridge_running`: pause it
    /// first), and only chats with recent activity.
    pub async fn list_bridge_chats(&self, bridge_id: &str) -> Result<BridgeChatList> {
        let value = self
            .request("GET", &bridge_path(bridge_id, "/chats")?, None, None, true)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Post a short test message in one of the bridge's allowed chats
    /// (`POST /v1/bridges/{id}/test`, `hubs:write`), in the hub's language.
    /// A blank chat id fails locally.
    pub async fn send_bridge_test_message(
        &self,
        bridge_id: &str,
        chat_id: &str,
    ) -> Result<BridgeTestResult> {
        let path = bridge_path(bridge_id, "/test")?;
        let body = json!({ "chat_id": required("chat_id", chat_id)? });
        let value = self.request("POST", &path, Some(body), None, true).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// The bot's picture (`GET /v1/bridges/{id}/avatar`, `hubs:read`): the
    /// image bytes and their media type. A bot without one is the API's
    /// 404, returned as [`ThalovantError::ApiResponse`].
    pub async fn get_bridge_avatar(&self, bridge_id: &str) -> Result<BridgeAvatar> {
        let path = bridge_path(bridge_id, "/avatar")?;
        let (content, content_type) = self
            .request_bytes(&path, "image/*, application/problem+json")
            .await?;
        Ok(BridgeAvatar {
            content,
            content_type: content_type.unwrap_or_else(|| "application/octet-stream".into()),
        })
    }

    /// Stop and remove a bridge (`DELETE /v1/bridges/{id}`, `hubs:write`),
    /// and the credentials it stored.
    pub async fn delete_bridge(&self, bridge_id: &str) -> Result<()> {
        let _ = self
            .request("DELETE", &bridge_path(bridge_id, "")?, None, None, true)
            .await?;
        Ok(())
    }
}
