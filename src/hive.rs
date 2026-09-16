//! The hive's own frame kinds, and the binary frames a hub sends back.
//!
//! A hub relays more than this client's conversation: `broadcast` is aimed down
//! at every child, `propagate` walks the whole hive, `escalate` goes up to the
//! parent, `intercom` is addressed node to node, and `rendezvous` is the mailbox
//! peers use to find each other through NAT. A BINARY frame is how a hub answers
//! `speak:synth` -- the rendered audio itself, so a client with no synthesiser
//! of its own can still speak -- and how a file arrives.

use crate::{
    client::Client,
    errors::{Result, ThalovantError},
    events::{Context, Data, ThalovantBinary, HIVE_KINDS},
    transport::HiveMessage,
};
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

/// One hive frame kind, filtered off the transport's hive stream.
pub struct HiveStream {
    receiver: broadcast::Receiver<HiveMessage>,
    kind: String,
}

impl HiveStream {
    /// The next frame of this kind. `None` once the transport stream ends.
    ///
    /// A lagged receiver is an error rather than a silent gap: a dropped
    /// broadcast frame is not something to discover later.
    pub async fn recv(&mut self) -> Result<Option<HiveMessage>> {
        loop {
            match self.receiver.recv().await {
                Ok(message) if message.msg_type == self.kind => return Ok(Some(message)),
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    return Err(ThalovantError::Runtime(format!(
                        "missed {count} hive frames; subscribe sooner or read faster"
                    )))
                }
            }
        }
    }
}

/// Binary frames: rendered speech, and files.
pub struct BinaryStream {
    receiver: broadcast::Receiver<HiveMessage>,
}

impl BinaryStream {
    /// The next binary frame. `None` once the transport stream ends.
    pub async fn recv(&mut self) -> Result<Option<ThalovantBinary>> {
        loop {
            match self.receiver.recv().await {
                Ok(message) => match message.binary {
                    Some(binary) => return Ok(Some(binary)),
                    None => continue,
                },
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    return Err(ThalovantError::Runtime(format!(
                        "missed {count} hive frames; subscribe sooner or read faster"
                    )))
                }
            }
        }
    }
}

impl Client {
    /// Listen to one of the hive's own frame kinds. See [`HIVE_KINDS`].
    pub async fn subscribe_hive(&self, kind: &str) -> Result<HiveStream> {
        if !HIVE_KINDS.contains(&kind) {
            // Named rather than silently never firing: subscribing to "bus" or
            // to a typo is the kind of mistake that looks like a quiet hub.
            return Err(ThalovantError::Runtime(format!(
                "{kind:?} is not a hive frame kind; expected one of {}",
                HIVE_KINDS.join(", ")
            )));
        }
        let receiver = self.transport.subscribe_hive();
        self.connect().await?;
        Ok(HiveStream {
            receiver,
            kind: kind.to_string(),
        })
    }

    /// Listen for binary frames: rendered speech, and files.
    ///
    /// Delivered as a stream and not on a reply, because a binary frame carries
    /// no request id: it cannot be attributed to one `ask`. Its `utterance` is
    /// the only thread back to a turn.
    pub async fn subscribe_binary(&self) -> Result<BinaryStream> {
        let receiver = self.transport.subscribe_hive();
        self.connect().await?;
        Ok(BinaryStream { receiver })
    }

    /// Send an event across the hive; every node sees it once.
    pub async fn propagate(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.send_hive("propagate", event_type, data, context).await
    }

    /// Send an event up to the parent node.
    pub async fn escalate(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.send_hive("escalate", event_type, data, context).await
    }

    /// Send an event down to every child of this hub. **Admin only.**
    ///
    /// A hub requires admin standing and the `can_broadcast` grant, and a client
    /// that sends one without them is not answered with an error -- it is
    /// disconnected for misbehaviour. Nothing here can check first: a hub's
    /// HELLO carries its public key, peer name and node id, and nothing about
    /// what this client may do, so a refusal arrives as a closed connection on
    /// the next read.
    pub async fn broadcast(&self, event_type: &str, data: Data, context: Context) -> Result<()> {
        self.send_hive("broadcast", event_type, data, context).await
    }

    async fn send_hive(
        &self,
        kind: &str,
        event_type: &str,
        data: Data,
        context: Context,
    ) -> Result<()> {
        self.connect().await?;
        // Nested on purpose: a hub reads message.payload as a HiveMessage of its
        // own and rewrites the route on it, so a flat frame loses the route.
        let mut payload = Map::new();
        payload.insert("msg_type".into(), Value::String("bus".into()));
        payload.insert(
            "payload".into(),
            json!({
                "type": event_type,
                "data": Value::Object(data),
                "context": Value::Object(context),
            }),
        );
        self.transport
            .send_hive_message(
                HiveMessage {
                    msg_type: kind.to_string(),
                    binary: None,
                    payload,
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
}
