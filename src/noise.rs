//! HiveMind protocol v3: the Noise handshake and its transport framing.
//!
//! A HiveMind-core 5.x hub accepts one transport key exchange and closes
//! anything else with WebSocket `1008`. There is no pre-shared `crypto_key`
//! any more and no cleartext path.
//!
//! The pieces here are the interop contract with the reference implementation
//! (`poorman-handshake` / `hivemind-bus-client`). Each of them has to produce
//! byte-identical output or the handshake fails in exactly the same way as a
//! wrong password, so they are pinned against reference vectors in the tests
//! rather than checked by round-tripping against themselves.

use std::collections::BTreeMap;
use std::sync::Mutex;

use argon2::{Algorithm, Argon2, Params, Version};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use snow::{HandshakeState, TransportState};

use crate::errors::{Result, ThalovantError};

/// HiveMind protocol version that switches the handshake to Noise.
pub const PROTOCOL_V3: u64 = 3;

/// Registered handshake pattern for a peer whose static key is already pinned.
pub const NOISE_PATTERN_KK: &str = "KKpsk0";
/// Registered handshake pattern for first contact, trust on first use.
pub const NOISE_PATTERN_XX: &str = "XXpsk2";

/// Registered cipher suites in *our* preference order. The selection walks this
/// list rather than the hub's, so ChaChaPoly wins whenever both peers have it
/// however the hub ordered its advertisement.
pub const NOISE_SUITES: [&str; 2] = ["25519_ChaChaPoly_SHA256", "25519_AESGCM_SHA256"];

/// argon2id parameters for the password to pre-shared-key derivation. These are
/// part of the wire contract: a peer deriving with different parameters
/// produces a different key, and the handshake then fails as it would on a
/// wrong password.
const PSK_TIME_COST: u32 = 3;
const PSK_MEMORY_KIB: u32 = 64 * 1024;
const PSK_LANES: u32 = 1;
const PSK_LEN: usize = 32;

/// Transport frame markers. The first plaintext byte of every Noise transport
/// message says how to parse the rest and where it sits in a chunked message.
const FRAME_JSON: u8 = 0x00;
const FRAME_BINARY: u8 = 0x01;
const FRAME_FIRST_JSON: u8 = 0x02;
const FRAME_FIRST_BINARY: u8 = 0x03;
const FRAME_MORE: u8 = 0x04;
const FRAME_LAST: u8 = 0x05;

/// A Noise transport message caps at 65535 bytes. Chunking well below that
/// leaves room for the AEAD tag, the marker, and implementation overhead.
pub const NOISE_CHUNK_SIZE: usize = 65_000;

/// Bounded reassembly budget, so one peer cannot make us allocate without
/// limit from a single message.
pub const NOISE_MAX_REASSEMBLY: usize = 32 * 1024 * 1024;

/// Stretch the shared site password into the 32-byte Noise pre-shared key,
/// salted with SHA-256 of the *hub's* node id.
///
/// This costs 64 MiB and a few hundred milliseconds, and the result is fixed
/// for a `(password, node_id)` pair, so a caller that reconnects should derive
/// once and keep it.
pub fn derive_psk(password: &str, node_id: &str) -> Result<[u8; PSK_LEN]> {
    let salt = Sha256::digest(node_id.as_bytes());
    let params = Params::new(PSK_MEMORY_KIB, PSK_TIME_COST, PSK_LANES, Some(PSK_LEN))
        .map_err(|err| ThalovantError::Crypto(format!("invalid argon2id parameters: {err}")))?;
    let mut psk = [0_u8; PSK_LEN];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), &salt, &mut psk)
        .map_err(|err| {
            ThalovantError::Crypto(format!("could not derive the Noise pre-shared key: {err}"))
        })?;
    Ok(psk)
}

/// The full Noise protocol name for a pattern and suite selection.
pub fn noise_protocol_name(pattern: &str, suite: &str) -> String {
    format!("Noise_{pattern}_{suite}")
}

/// Pick the handshake pattern and suite from the hub's advertised lists.
///
/// `KKpsk0` is chosen only when a static key for this hub is already pinned and
/// the hub offers it; otherwise `XXpsk2`. `None` means there is no mutual
/// option and the connection cannot proceed.
pub fn select_noise_options(
    hub_patterns: &[String],
    hub_suites: &[String],
    pinned_remote_key: Option<&str>,
) -> Option<(String, String)> {
    let suite = NOISE_SUITES
        .iter()
        .find(|candidate| hub_suites.iter().any(|offered| offered == *candidate))?;

    let offers = |pattern: &str| hub_patterns.iter().any(|offered| offered == pattern);

    if pinned_remote_key.is_some_and(|key| !key.is_empty()) && offers(NOISE_PATTERN_KK) {
        return Some((NOISE_PATTERN_KK.to_string(), (*suite).to_string()));
    }
    if offers(NOISE_PATTERN_XX) {
        return Some((NOISE_PATTERN_XX.to_string(), (*suite).to_string()));
    }
    None
}

/// Serialize a JSON value the way the reference implementation does: sorted
/// keys, no whitespace, and no ASCII escaping of non-ASCII characters.
///
/// `serde_json::to_string` agrees with this today, because `serde_json::Map` is
/// a `BTreeMap` by default and its string escaping already matches Python's
/// `json.dumps(ensure_ascii=False)`. It is written out anyway because that
/// agreement is not ours to rely on: any crate anywhere in the dependency graph
/// enabling `serde_json/preserve_order` turns that map into an `IndexMap` for
/// the whole build, and key order would then follow insertion. That would
/// change the prologue bytes and make every handshake fail like a wrong
/// password, in a build we did not change. The reference vectors in the tests
/// are the real protection; this keeps the encoder out of reach of feature
/// unification.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_canonical_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(entries) => {
            // BTreeMap sorts by the same byte order as Python's sort_keys.
            let sorted: BTreeMap<&String, &Value> = entries.iter().collect();
            out.push('{');
            for (index, (key, item)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_string(out, key);
                out.push(':');
                write_canonical(out, item);
            }
            out.push('}');
        }
    }
}

/// Escape exactly what `json.dumps(ensure_ascii=False)` escapes: the quote, the
/// backslash, and control characters below 0x20. Everything else, non-ASCII
/// included, is written literally.
fn write_canonical_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Prologue bytes: the hub's cleartext HELLO payload, its cleartext parameter
/// HANDSHAKE payload, and the selected protocol name, concatenated in that
/// order with no separators.
///
/// This is the downgrade and tampering protection. If either peer computed the
/// negotiation differently the handshake aborts rather than quietly proceeding
/// on weaker terms.
pub fn build_prologue(
    hello_payload: &Map<String, Value>,
    handshake_payload: &Map<String, Value>,
    protocol_name: &str,
) -> Vec<u8> {
    let mut prologue = canonical_json(&Value::Object(hello_payload.clone())).into_bytes();
    prologue
        .extend_from_slice(canonical_json(&Value::Object(handshake_payload.clone())).as_bytes());
    prologue.extend_from_slice(protocol_name.as_bytes());
    prologue
}

/// The initiator side of a v3 handshake.
pub struct NoiseHandshake {
    state: Option<HandshakeState>,
    transport: Option<TransportState>,
    pattern: String,
    remote_static_key: Option<String>,
}

impl NoiseHandshake {
    /// Start the handshake.
    ///
    /// `static_key` is this client's persistent X25519 private key. It has to
    /// persist: a hub pins it on first contact, so regenerating it makes every
    /// connection look like a new peer. `pinned_remote_key` is the hex-encoded
    /// hub static key, required for `KKpsk0` and ignored otherwise.
    pub fn new(
        pattern: &str,
        suite: &str,
        psk: &[u8; PSK_LEN],
        prologue: &[u8],
        static_key: &[u8],
        pinned_remote_key: Option<&str>,
    ) -> Result<Self> {
        let name = noise_protocol_name(pattern, suite);
        let params = name.parse().map_err(|err| {
            ThalovantError::Connection(format!("unsupported Noise protocol {name}: {err}"))
        })?;

        let psk_index = match pattern {
            NOISE_PATTERN_XX => 2,
            NOISE_PATTERN_KK => 0,
            other => {
                return Err(ThalovantError::Connection(format!(
                    "unsupported Noise pattern {other}"
                )))
            }
        };

        // snow 0.10 made these builder steps fallible: they validate the key
        // and prologue up front instead of at build time.
        let setup_failed =
            |err| ThalovantError::Connection(format!("could not configure {name}: {err}"));
        let mut builder = snow::Builder::new(params)
            .local_private_key(static_key)
            .map_err(setup_failed)?
            .prologue(prologue)
            .map_err(setup_failed)?
            .psk(psk_index, psk)
            .map_err(setup_failed)?;

        let remote_bytes;
        if pattern == NOISE_PATTERN_KK {
            let hex_key = pinned_remote_key.unwrap_or_default();
            remote_bytes = hex::decode(hex_key).map_err(|_| {
                ThalovantError::Connection(
                    "KKpsk0 needs a pinned 32-byte hub static key".to_string(),
                )
            })?;
            if remote_bytes.len() != 32 {
                return Err(ThalovantError::Connection(
                    "KKpsk0 needs a pinned 32-byte hub static key".to_string(),
                ));
            }
            builder = builder.remote_public_key(&remote_bytes).map_err(setup_failed)?;
        }

        let state = builder
            .build_initiator()
            .map_err(|err| ThalovantError::Connection(format!("could not start {name}: {err}")))?;

        Ok(Self {
            state: Some(state),
            transport: None,
            pattern: pattern.to_string(),
            remote_static_key: None,
        })
    }

    /// Produce the next outgoing handshake message.
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let state = self.state.as_mut().ok_or_else(|| {
            ThalovantError::Connection("the Noise handshake has already finished".to_string())
        })?;
        let mut buffer = vec![0_u8; 65_535];
        let written = state.write_message(payload, &mut buffer).map_err(|err| {
            ThalovantError::Connection(format!("Noise handshake write failed: {err}"))
        })?;
        buffer.truncate(written);
        self.maybe_finish()?;
        Ok(buffer)
    }

    /// Consume an incoming handshake message and return its payload.
    ///
    /// A failure here is authentication failing: a wrong password, a tampered
    /// negotiation, or a static key contradicting the pinned one. It is fatal,
    /// and the connection must be rejected rather than retried on weaker terms.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        let state = self.state.as_mut().ok_or_else(|| {
            ThalovantError::Connection("the Noise handshake has already finished".to_string())
        })?;
        let mut buffer = vec![0_u8; 65_535];
        let read = state.read_message(message, &mut buffer).map_err(|err| {
            ThalovantError::Connection(format!(
                "Noise handshake authentication failed (wrong password or tampered negotiation): {err}"
            ))
        })?;
        buffer.truncate(read);
        self.maybe_finish()?;
        Ok(buffer)
    }

    /// Move to transport mode once the pattern has run its course, keeping the
    /// learned hub static key first: `snow` drops the handshake state on the
    /// transition, and under `XXpsk2` that key is what gets pinned.
    fn maybe_finish(&mut self) -> Result<()> {
        let finished = self
            .state
            .as_ref()
            .is_some_and(|state| state.is_handshake_finished());
        if !finished {
            return Ok(());
        }
        let state = self.state.take().expect("checked above");
        self.remote_static_key = state.get_remote_static().map(hex::encode);
        self.transport = Some(state.into_transport_mode().map_err(|err| {
            ThalovantError::Connection(format!("could not enter Noise transport mode: {err}"))
        })?);
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self.transport.is_some()
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The hub's static public key, hex encoded. Under `XXpsk2` it is learned
    /// during the handshake and is what the client pins.
    pub fn remote_static_key(&self) -> Option<&str> {
        self.remote_static_key.as_deref()
    }

    /// Take the completed session. Fails while the handshake is still running.
    pub fn into_session(self) -> Result<NoiseSession> {
        let transport = self.transport.ok_or_else(|| {
            ThalovantError::Connection("the Noise handshake is not finished".to_string())
        })?;
        Ok(NoiseSession {
            transport: Mutex::new(transport),
            reassembly: Mutex::new(None),
            remote_static_key: self.remote_static_key,
        })
    }
}

/// One decrypted incoming message, and whether it is JSON or a binary frame.
#[derive(Debug, PartialEq, Eq)]
pub enum NoiseFrame {
    /// A complete message.
    Message { payload: Vec<u8>, is_json: bool },
    /// This frame only advanced a chunked message; keep receiving.
    Partial,
}

struct Reassembly {
    buffer: Vec<u8>,
    is_json: bool,
}

/// A completed v3 session: the transport cipher states plus the HiveMind frame
/// markers that separate JSON from binary after decryption.
pub struct NoiseSession {
    transport: Mutex<TransportState>,
    reassembly: Mutex<Option<Reassembly>>,
    remote_static_key: Option<String>,
}

impl NoiseSession {
    pub fn remote_static_key(&self) -> Option<&str> {
        self.remote_static_key.as_deref()
    }

    /// Encrypt one message into the Noise transport messages that carry it,
    /// chunking when it does not fit in one.
    ///
    /// The whole message is encrypted under one lock so its chunks stay
    /// contiguous and ordered: the cipher state nonce counter is strictly
    /// sequential, and interleaving two messages would break the receiver.
    pub fn encrypt_message(&self, payload: &[u8], is_json: bool) -> Result<Vec<Vec<u8>>> {
        let (single, first) = if is_json {
            (FRAME_JSON, FRAME_FIRST_JSON)
        } else {
            (FRAME_BINARY, FRAME_FIRST_BINARY)
        };

        let mut transport = self.transport.lock().map_err(poisoned)?;

        if payload.len() <= NOISE_CHUNK_SIZE {
            return Ok(vec![encrypt_one(&mut transport, single, payload)?]);
        }

        let last_offset = payload.len() - NOISE_CHUNK_SIZE;
        let mut frames = Vec::new();
        let mut offset = 0;
        while offset < payload.len() {
            let end = (offset + NOISE_CHUNK_SIZE).min(payload.len());
            let marker = if offset == 0 {
                first
            } else if offset >= last_offset {
                FRAME_LAST
            } else {
                FRAME_MORE
            };
            frames.push(encrypt_one(&mut transport, marker, &payload[offset..end])?);
            offset += NOISE_CHUNK_SIZE;
        }
        Ok(frames)
    }

    /// Decrypt one incoming Noise transport message.
    ///
    /// Every error here is fatal for the session. A message that fails to
    /// decrypt at the current counter means tampering, replay or reordering,
    /// and so does a malformed chunk sequence; the connection must be dropped
    /// rather than the frame skipped.
    pub fn decrypt_frame(&self, data: &[u8]) -> Result<NoiseFrame> {
        let plaintext = {
            let mut transport = self.transport.lock().map_err(poisoned)?;
            let mut buffer = vec![0_u8; data.len().max(1)];
            let read = transport.read_message(data, &mut buffer).map_err(|err| {
                ThalovantError::Connection(format!(
                    "Noise transport message rejected (tampered, replayed or out-of-order): {err}"
                ))
            })?;
            buffer.truncate(read);
            buffer
        };

        let (marker, body) = plaintext.split_first().ok_or_else(|| {
            ThalovantError::Connection("empty Noise transport message".to_string())
        })?;

        let mut open = self.reassembly.lock().map_err(poisoned)?;

        match *marker {
            FRAME_JSON | FRAME_BINARY => {
                if let Some(stale) = open.take() {
                    return Err(ThalovantError::Connection(format!(
                        "a complete frame arrived while {} bytes of a chunked message were still buffered",
                        stale.buffer.len()
                    )));
                }
                Ok(NoiseFrame::Message {
                    payload: body.to_vec(),
                    is_json: *marker == FRAME_JSON,
                })
            }
            FRAME_FIRST_JSON | FRAME_FIRST_BINARY => {
                if let Some(stale) = open.take() {
                    return Err(ThalovantError::Connection(format!(
                        "a new chunked message started while {} bytes of a previous one were still buffered",
                        stale.buffer.len()
                    )));
                }
                *open = Some(Reassembly {
                    buffer: body.to_vec(),
                    is_json: *marker == FRAME_FIRST_JSON,
                });
                guard_reassembly_cap(&mut open)?;
                Ok(NoiseFrame::Partial)
            }
            FRAME_MORE => {
                let current = open.as_mut().ok_or_else(|| {
                    ThalovantError::Connection(
                        "a middle chunk arrived with no chunked message open".to_string(),
                    )
                })?;
                current.buffer.extend_from_slice(body);
                guard_reassembly_cap(&mut open)?;
                Ok(NoiseFrame::Partial)
            }
            FRAME_LAST => {
                {
                    let current = open.as_mut().ok_or_else(|| {
                        ThalovantError::Connection(
                            "a final chunk arrived with no chunked message open".to_string(),
                        )
                    })?;
                    current.buffer.extend_from_slice(body);
                }
                guard_reassembly_cap(&mut open)?;
                let finished = open.take().expect("checked above");
                Ok(NoiseFrame::Message {
                    payload: finished.buffer,
                    is_json: finished.is_json,
                })
            }
            unknown => Err(ThalovantError::Connection(format!(
                "unknown v3 frame marker 0x{unknown:02x}"
            ))),
        }
    }
}

fn encrypt_one(transport: &mut TransportState, marker: u8, body: &[u8]) -> Result<Vec<u8>> {
    let mut plaintext = Vec::with_capacity(body.len() + 1);
    plaintext.push(marker);
    plaintext.extend_from_slice(body);
    let mut buffer = vec![0_u8; plaintext.len() + 32];
    let written = transport
        .write_message(&plaintext, &mut buffer)
        .map_err(|err| {
            ThalovantError::Connection(format!("Noise transport encryption failed: {err}"))
        })?;
    buffer.truncate(written);
    Ok(buffer)
}

/// Drop the whole buffer once it passes the cap, so a peer cannot grow it
/// without limit.
fn guard_reassembly_cap(open: &mut Option<Reassembly>) -> Result<()> {
    let buffered = open.as_ref().map_or(0, |current| current.buffer.len());
    if buffered <= NOISE_MAX_REASSEMBLY {
        return Ok(());
    }
    *open = None;
    Err(ThalovantError::Connection(format!(
        "chunked reassembly exceeded the {NOISE_MAX_REASSEMBLY} byte cap ({buffered} buffered); dropping the message"
    )))
}

fn poisoned<T>(_: T) -> ThalovantError {
    ThalovantError::Connection("the Noise session lock was poisoned by a panic".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The vectors below come from the reference implementation
    // (poorman-handshake 2.0.0a3 / hivemind-bus-client 1.1.1a1). They are the
    // interop contract: a peer whose pre-shared key, canonical JSON or prologue
    // differs by one byte fails the handshake in exactly the same way as a
    // wrong password, so round-tripping against ourselves would prove nothing.

    #[test]
    fn derive_psk_matches_the_reference_vectors() {
        for (index, case) in [
            (
                "Tr0ub4dor-Horse-Battery-91x",
                "node-alpha",
                "ce6825b343771aed1833233c8d1af4ce4e470cee89b625065402def527f900ce",
            ),
            (
                "",
                "",
                "38bedc40c2ce3b79cd5ccf53745e029363f5ba1948cb21f018bcce6eb0869876",
            ),
            (
                "pass\u{e9}-w\u{f6}rd",
                "hub-\u{fc}ml\u{e4}ut",
                "988418601dbad183fbd6116e7981e9ab8ffe93be3f3f45c27eb0b70c325f9cd8",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let (password, node_id, expected) = case;
            let psk = derive_psk(password, node_id).unwrap();
            // Identify the failing case by index rather than by echoing the
            // password: these are public reference vectors, but a test that
            // prints a secret teaches the wrong habit and trips secret scanners.
            assert_eq!(
                hex::encode(psk),
                expected,
                "vector {index} (node id {node_id:?}) does not match the reference"
            );
        }
    }

    #[test]
    fn derive_psk_is_salted_by_the_node_id() {
        let first = derive_psk("same-password", "hub-one").unwrap();
        let second = derive_psk("same-password", "hub-two").unwrap();
        assert_ne!(
            first, second,
            "the same password produced the same key for two hubs; the node id salt is not applied"
        );
    }

    #[test]
    fn canonical_json_matches_the_reference_vectors() {
        for (input, expected) in [
            (
                r#"{"b": 1, "a": [1, 2], "c": {"z": true, "y": null}}"#,
                r#"{"a":[1,2],"b":1,"c":{"y":null,"z":true}}"#,
            ),
            (
                r#"{"binarize": false, "encodings": []}"#,
                r#"{"binarize":false,"encodings":[]}"#,
            ),
            (
                // serde_json would keep insertion order rather than sorting.
                r#"{"unicode": "café", "amp": "a<b>c&d"}"#,
                "{\"amp\":\"a<b>c&d\",\"unicode\":\"caf\u{e9}\"}",
            ),
            (
                r#"{"nested": {"deep": [{"k": "v"}, 2, null]}, "num": 3}"#,
                r#"{"nested":{"deep":[{"k":"v"},2,null]},"num":3}"#,
            ),
        ] {
            let value: Value = serde_json::from_str(input).unwrap();
            assert_eq!(canonical_json(&value), expected);
        }
    }

    #[test]
    fn canonical_json_sorts_keys_whatever_order_they_arrived_in() {
        // The prologue must not depend on how the payload was parsed or built.
        let mut reversed = Map::new();
        reversed.insert("zebra".into(), json!(1));
        reversed.insert("apple".into(), json!(2));
        reversed.insert("Mango".into(), json!(3));

        assert_eq!(
            canonical_json(&Value::Object(reversed)),
            r#"{"Mango":3,"apple":2,"zebra":1}"#,
            "keys must sort by byte order, uppercase first, however they were inserted"
        );
    }

    #[test]
    fn canonical_json_escapes_quotes_backslashes_and_control_characters() {
        // Built with Rust escapes so the source carries no literal control bytes.
        let value = json!({
            "s": "quote\" back\\slash\nnewline\ttab",
            "ctrl": "\u{1}\u{1f}",
        });
        assert_eq!(
            canonical_json(&value),
            "{\"ctrl\":\"\\u0001\\u001f\",\"s\":\"quote\\\" back\\\\slash\\nnewline\\ttab\"}"
        );
    }

    #[test]
    fn build_prologue_matches_the_reference_vector() {
        let expected = "7b226e6f64655f6964223a226e6f64652d616c706861222c227075626b6579223a222d2d2d2d2d424547494e205055424c4943204b45592d2d2d2d2d5c6e6162635c6e2d2d2d2d2d454e44205055424c4943204b45592d2d2d2d2d227d7b226d61785f70726f746f636f6c5f76657273696f6e223a332c226e6f697365223a7b227061747465726e73223a5b22585870736b32225d2c22737569746573223a5b2232353531395f436861436861506f6c795f534841323536225d7d7d4e6f6973655f585870736b325f32353531395f436861436861506f6c795f534841323536";

        let hello = json!({
            "node_id": "node-alpha",
            "pubkey": "-----BEGIN PUBLIC KEY-----\nabc\n-----END PUBLIC KEY-----"
        });
        let handshake = json!({
            "max_protocol_version": 3,
            "noise": {"patterns": ["XXpsk2"], "suites": ["25519_ChaChaPoly_SHA256"]}
        });
        let prologue = build_prologue(
            hello.as_object().unwrap(),
            handshake.as_object().unwrap(),
            &noise_protocol_name(NOISE_PATTERN_XX, "25519_ChaChaPoly_SHA256"),
        );
        assert_eq!(hex::encode(prologue), expected);
    }

    #[test]
    fn noise_protocol_name_joins_the_selection() {
        assert_eq!(
            noise_protocol_name(NOISE_PATTERN_XX, "25519_ChaChaPoly_SHA256"),
            "Noise_XXpsk2_25519_ChaChaPoly_SHA256"
        );
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn select_noise_options_prefers_a_pinned_hub() {
        let both = names(&[NOISE_PATTERN_KK, NOISE_PATTERN_XX]);
        let chacha = names(&["25519_ChaChaPoly_SHA256"]);
        let pin = "ab".repeat(32);

        assert_eq!(
            select_noise_options(&both, &chacha, None),
            Some((NOISE_PATTERN_XX.into(), "25519_ChaChaPoly_SHA256".into())),
            "no pin means first contact"
        );
        assert_eq!(
            select_noise_options(&both, &chacha, Some(&pin)),
            Some((NOISE_PATTERN_KK.into(), "25519_ChaChaPoly_SHA256".into())),
            "a pinned key upgrades to KK when the hub offers it"
        );
        assert_eq!(
            select_noise_options(&names(&[NOISE_PATTERN_XX]), &chacha, Some(&pin)),
            Some((NOISE_PATTERN_XX.into(), "25519_ChaChaPoly_SHA256".into())),
            "a pinned key still falls back when KK is not offered"
        );
    }

    #[test]
    fn select_noise_options_walks_our_own_suite_preference() {
        // The hub lists AESGCM first; ChaChaPoly must still win because both
        // peers have it.
        assert_eq!(
            select_noise_options(
                &names(&[NOISE_PATTERN_XX]),
                &names(&["25519_AESGCM_SHA256", "25519_ChaChaPoly_SHA256"]),
                None
            ),
            Some((NOISE_PATTERN_XX.into(), "25519_ChaChaPoly_SHA256".into()))
        );
        assert_eq!(
            select_noise_options(
                &names(&[NOISE_PATTERN_XX]),
                &names(&["25519_AESGCM_SHA256"]),
                None
            ),
            Some((NOISE_PATTERN_XX.into(), "25519_AESGCM_SHA256".into())),
            "AESGCM is taken when it is all the hub has"
        );
    }

    #[test]
    fn select_noise_options_reports_no_mutual_option() {
        assert_eq!(
            select_noise_options(
                &names(&[NOISE_PATTERN_XX]),
                &names(&["448_ChaChaPoly_BLAKE2b"]),
                None
            ),
            None,
            "no mutual suite"
        );
        assert_eq!(
            select_noise_options(
                &names(&["NNpsk0"]),
                &names(&["25519_ChaChaPoly_SHA256"]),
                None
            ),
            None,
            "no mutual pattern"
        );
        assert_eq!(select_noise_options(&[], &[], None), None);
    }

    /// Complete a handshake between the SDK's initiator and a responder built
    /// straight from `snow`, so the tests exercise the real client side against
    /// an independent peer rather than against itself.
    fn session_pair() -> (NoiseSession, NoiseSession) {
        let psk = derive_psk("shared", "hub").unwrap();
        let prologue = b"prologue";
        let params: snow::params::NoiseParams =
            "Noise_XXpsk2_25519_ChaChaPoly_SHA256".parse().unwrap();

        let client_key = snow::Builder::new(params.clone())
            .generate_keypair()
            .unwrap();
        let server_key = snow::Builder::new(params.clone())
            .generate_keypair()
            .unwrap();

        let mut initiator = NoiseHandshake::new(
            NOISE_PATTERN_XX,
            "25519_ChaChaPoly_SHA256",
            &psk,
            prologue,
            &client_key.private,
            None,
        )
        .unwrap();
        let mut responder = snow::Builder::new(params)
            .local_private_key(&server_key.private)
            .unwrap()
            .prologue(prologue)
            .unwrap()
            .psk(2, &psk)
            .unwrap()
            .build_responder()
            .unwrap();

        let mut scratch = vec![0_u8; 65_535];
        let message1 = initiator.write_message(b"payload-1").unwrap();
        responder.read_message(&message1, &mut scratch).unwrap();
        let written = responder.write_message(&[], &mut scratch).unwrap();
        initiator.read_message(&scratch[..written]).unwrap();
        let message3 = initiator.write_message(&[]).unwrap();
        responder.read_message(&message3, &mut scratch).unwrap();

        assert!(initiator.is_finished(), "the handshake did not finish");
        assert!(
            initiator.remote_static_key().is_some(),
            "XXpsk2 finished without learning the peer static key; there would be nothing to pin"
        );

        let hub = NoiseSession {
            transport: Mutex::new(responder.into_transport_mode().unwrap()),
            reassembly: Mutex::new(None),
            remote_static_key: None,
        };
        (initiator.into_session().unwrap(), hub)
    }

    #[test]
    fn a_small_message_travels_as_one_frame() {
        let (client, hub) = session_pair();
        let frames = client
            .encrypt_message(br#"{"msg_type":"bus"}"#, true)
            .unwrap();
        assert_eq!(frames.len(), 1, "a small message should not be chunked");

        assert_eq!(
            hub.decrypt_frame(&frames[0]).unwrap(),
            NoiseFrame::Message {
                payload: br#"{"msg_type":"bus"}"#.to_vec(),
                is_json: true
            }
        );
    }

    #[test]
    fn an_oversize_message_is_chunked_and_reassembled() {
        let (client, hub) = session_pair();
        // Two and a bit chunks, so the sequence is FIRST, MORE, LAST.
        let original = vec![b'x'; NOISE_CHUNK_SIZE * 2 + 1024];

        let frames = client.encrypt_message(&original, true).unwrap();
        assert_eq!(frames.len(), 3);

        assert_eq!(hub.decrypt_frame(&frames[0]).unwrap(), NoiseFrame::Partial);
        assert_eq!(hub.decrypt_frame(&frames[1]).unwrap(), NoiseFrame::Partial);
        assert_eq!(
            hub.decrypt_frame(&frames[2]).unwrap(),
            NoiseFrame::Message {
                payload: original,
                is_json: true
            }
        );
    }

    #[test]
    fn binary_frames_keep_their_marker() {
        let (client, hub) = session_pair();
        let frames = client.encrypt_message(&[0x0c, 0xff, 0x00], false).unwrap();
        assert_eq!(
            hub.decrypt_frame(&frames[0]).unwrap(),
            NoiseFrame::Message {
                payload: vec![0x0c, 0xff, 0x00],
                is_json: false
            }
        );
    }

    #[test]
    fn a_tampered_frame_is_rejected() {
        let (client, hub) = session_pair();
        let mut frames = client.encrypt_message(br#"{"a":1}"#, true).unwrap();
        let last = frames[0].len() - 1;
        frames[0][last] ^= 0xff;

        assert!(
            hub.decrypt_frame(&frames[0]).is_err(),
            "a tampered frame decrypted; the AEAD tag is not being enforced"
        );
    }

    #[test]
    fn a_replayed_frame_is_rejected() {
        let (client, hub) = session_pair();
        let first = client.encrypt_message(br#"{"n":1}"#, true).unwrap();
        let _second = client.encrypt_message(br#"{"n":2}"#, true).unwrap();

        hub.decrypt_frame(&first[0]).unwrap();
        assert!(
            hub.decrypt_frame(&first[0]).is_err(),
            "a replayed frame decrypted; the nonce counter is not advancing"
        );
    }

    #[test]
    fn a_middle_chunk_with_nothing_open_is_rejected() {
        let (client, hub) = session_pair();
        // Reach past encrypt_message to forge a marker it would never send.
        let orphan = {
            let mut transport = client.transport.lock().unwrap();
            encrypt_one(&mut transport, FRAME_MORE, b"body").unwrap()
        };
        let error = hub.decrypt_frame(&orphan).unwrap_err().to_string();
        assert!(
            error.contains("no chunked message open"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_unknown_marker_is_rejected() {
        let (client, hub) = session_pair();
        let frame = {
            let mut transport = client.transport.lock().unwrap();
            encrypt_one(&mut transport, 0x7f, b"x").unwrap()
        };
        let error = hub.decrypt_frame(&frame).unwrap_err().to_string();
        assert!(
            error.contains("unknown v3 frame marker"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn reassembly_is_capped_and_the_buffer_released() {
        let (client, hub) = session_pair();
        *hub.reassembly.lock().unwrap() = Some(Reassembly {
            buffer: vec![0_u8; NOISE_MAX_REASSEMBLY],
            is_json: true,
        });
        let frame = {
            let mut transport = client.transport.lock().unwrap();
            encrypt_one(&mut transport, FRAME_MORE, &[b'y'; 64]).unwrap()
        };

        assert!(
            hub.decrypt_frame(&frame).is_err(),
            "reassembly grew past the cap"
        );
        assert!(
            hub.reassembly.lock().unwrap().is_none(),
            "the buffer was kept after the cap was hit; the memory is not released"
        );
    }
}
