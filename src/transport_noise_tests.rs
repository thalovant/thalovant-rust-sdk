use super::*;
use std::collections::{HashSet, VecDeque};

// The conformance recorder the integration tests use: the link-keeping
// `handshake` cases run here, beside the Noise fixtures they need.
#[path = "../tests/common/mod.rs"]
mod common;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::{rustls::pki_types::PrivatePkcs8KeyDer, TlsAcceptor};

fn test_password() -> &'static str {
    static PASSWORD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PASSWORD.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

struct FixtureDir(PathBuf);
impl FixtureDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("thalovant-noise-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Responder {
    key: snow::Keypair,
    peer: Option<Vec<u8>>,
    psk: [u8; 32],
    hello: Map<String, Value>,
    offer: Map<String, Value>,
    handshake: Option<snow::HandshakeState>,
    session: Option<snow::TransportState>,
    buffer: Vec<u8>,
    patterns: Vec<String>,
    hellos: usize,
    /// Every bus message the client sent after the handshake.
    received: Vec<HiveMessage>,
    /// Whether to offer KK to a peer it knows; `None` offers it whenever it
    /// can.
    offer_kk: Option<bool>,
    /// The patterns whose handshake answer it spoils, so that the client
    /// cannot authenticate it.
    corrupt: HashSet<String>,
    /// Pin the first client key and abort on any other, as hivemind-core
    /// does ("client Noise static key contradicts pinned key"): a handshake
    /// that shows another key fails right after its last message is read,
    /// before the hub sends anything.
    pins_client: bool,
}
impl Responder {
    fn new() -> Self {
        let key = snow::Builder::new("Noise_XXpsk2_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        Self {
            key,
            peer: None,
            psk: derive_psk(test_password(), "test-hub").unwrap(),
            hello: Map::new(),
            offer: Map::new(),
            handshake: None,
            session: None,
            buffer: vec![],
            patterns: vec![],
            hellos: 0,
            received: vec![],
            offer_kk: None,
            corrupt: HashSet::new(),
            pins_client: false,
        }
    }
    /// The hub's record of this connection's password changed.
    fn set_password(&mut self, password: &str) {
        self.psk = derive_psk(password, "test-hub").unwrap();
    }
    /// The hub was replaced: a new static key.
    fn replace_key(&mut self) {
        self.key = snow::Builder::new("Noise_XXpsk2_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
    }
    fn reset(&mut self) -> Vec<NoiseWrite> {
        self.handshake = None;
        self.session = None;
        self.buffer.clear();
        self.hello = json!({"node_id":"test-hub","peer":"test-peer","pubkey":"test-public"})
            .as_object()
            .unwrap()
            .clone();
        let patterns = if self.peer.is_some() && self.offer_kk.unwrap_or(true) {
            vec!["XXpsk2", "KKpsk0"]
        } else {
            vec!["XXpsk2"]
        };
        self.offer=json!({"max_protocol_version":3,"binarize":true,"encodings":["JSON-HEX"],"ciphers":["AES-GCM"],"noise":{"patterns":patterns,"suites":["25519_ChaChaPoly_SHA256","25519_AESGCM_SHA256"]}}).as_object().unwrap().clone();
        vec![
            Self::plain("hello", self.hello.clone()),
            Self::plain("shake", self.offer.clone()),
        ]
    }
    /// One message from the hub, encrypted, in a single frame.
    fn encrypt(&mut self, message: &HiveMessage) -> NoiseWrite {
        let session = self.session.as_mut().expect("the handshake is complete");
        let plain = [&[0_u8][..], &serde_json::to_vec(message).unwrap()].concat();
        let mut encrypted = vec![0; 65535];
        let length = session.write_message(&plain, &mut encrypted).unwrap();
        encrypted.truncate(length);
        NoiseWrite {
            payload: encrypted,
            binary: true,
        }
    }
    fn plain(kind: &str, payload: Map<String, Value>) -> NoiseWrite {
        NoiseWrite {
            payload: serde_json::to_vec(&HiveMessage {
                msg_type: kind.into(),
                payload,
                ..Default::default()
            })
            .unwrap(),
            binary: false,
        }
    }
    fn receive(&mut self, raw: &[u8], binary: bool) -> Result<Vec<NoiseWrite>> {
        if binary {
            let session = self
                .session
                .as_mut()
                .ok_or_else(|| ThalovantError::Connection("binary before split".into()))?;
            let mut decoded = vec![0; 65535];
            let count = session
                .read_message(raw, &mut decoded)
                .map_err(|_| ThalovantError::Connection("invalid ciphertext".into()))?;
            decoded.truncate(count);
            let (&marker, body) = decoded.split_first().unwrap();
            let payload = match marker {
                0 => body.to_vec(),
                2 => {
                    self.buffer = body.to_vec();
                    return Ok(vec![]);
                }
                4 => {
                    self.buffer.extend_from_slice(body);
                    return Ok(vec![]);
                }
                5 => {
                    self.buffer.extend_from_slice(body);
                    std::mem::take(&mut self.buffer)
                }
                _ => return Err(ThalovantError::Connection("invalid marker".into())),
            };
            let message: HiveMessage = serde_json::from_slice(&payload)?;
            if message.msg_type == "hello" {
                self.hellos += 1;
                return Ok(vec![]);
            }
            if message.msg_type == "query" {
                // Admission tests keep cascade collectors pending without a reply.
                return Ok(vec![]);
            }
            if message.msg_type != "bus" {
                return Err(ThalovantError::Connection("unexpected message".into()));
            }
            self.received.push(message);
            let chunks: Vec<_> = payload.chunks(65000).collect();
            let mut replies = vec![];
            for (i, chunk) in chunks.iter().enumerate() {
                let marker = if chunks.len() == 1 {
                    0
                } else if i == 0 {
                    2
                } else if i + 1 == chunks.len() {
                    5
                } else {
                    4
                };
                let plain = [&[marker], *chunk].concat();
                let mut encrypted = vec![0; 65535];
                let len = session.write_message(&plain, &mut encrypted).unwrap();
                encrypted.truncate(len);
                replies.push(NoiseWrite {
                    payload: encrypted,
                    binary: true,
                });
            }
            return Ok(replies);
        }
        if self.session.is_some() {
            return Err(ThalovantError::Connection("plaintext after split".into()));
        }
        let message: HiveMessage = serde_json::from_slice(raw)?;
        if message.msg_type != "shake" {
            return Err(ThalovantError::Connection(
                "plaintext application message".into(),
            ));
        }
        let params = message.payload["noise"].as_object().unwrap();
        let msg = hex::decode(params["msg"].as_str().unwrap()).unwrap();
        let mut replies = vec![];
        if let Some(handshake) = &mut self.handshake {
            handshake
                .read_message(&msg, &mut vec![0; 65535])
                .map_err(|_| ThalovantError::Connection("authentication failed".into()))?;
        } else {
            let pattern = params["pattern"].as_str().unwrap();
            // The pattern the client chose, whether or not it authenticates.
            self.patterns.push(pattern.into());
            let suite = params["suite"].as_str().unwrap();
            let name = noise_protocol_name(pattern, suite);
            let prologue = build_prologue(&self.hello, &self.offer, &name);
            let mut builder = snow::Builder::new(name.parse().unwrap())
                .local_private_key(&self.key.private)
                .unwrap()
                .prologue(&prologue)
                .unwrap()
                .psk(if pattern == "XXpsk2" { 2 } else { 0 }, &self.psk)
                .unwrap();
            if pattern == "KKpsk0" {
                builder = builder
                    .remote_public_key(self.peer.as_deref().unwrap())
                    .unwrap()
            }
            let mut handshake = builder.build_responder().unwrap();
            let mut out = vec![0; 65535];
            handshake
                .read_message(&msg, &mut out)
                .map_err(|_| ThalovantError::Connection("authentication failed".into()))?;
            let len = handshake.write_message(&[], &mut out).unwrap();
            out.truncate(len);
            if self.corrupt.contains(pattern) {
                *out.last_mut().unwrap() ^= 1;
            }
            replies.push(Self::plain(
                "shake",
                json!({"noise":{"msg":hex::encode(out)}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ));
            self.handshake = Some(handshake);
        }
        if self.handshake.as_ref().unwrap().is_handshake_finished() {
            let handshake = self.handshake.take().unwrap();
            let client = handshake.get_remote_static().map(Vec::from);
            if self.pins_client && self.peer.is_some() && client != self.peer {
                return Err(ThalovantError::Connection(
                    "client Noise static key contradicts pinned key".into(),
                ));
            }
            self.peer = client;
            self.session = Some(handshake.into_transport_mode().unwrap());
        }
        Ok(replies)
    }
}

fn tls_fixture() -> (TlsAcceptor, Vec<u8>, Vec<u8>) {
    ensure_rustls_provider();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let cert = certified.cert.der().clone();
    let pem = certified.cert.pem().into_bytes();
    let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
    (TlsAcceptor::from(Arc::new(config)), cert.to_vec(), pem)
}

struct HttpFixtureState {
    responder: Responder,
    plain: VecDeque<String>,
    binary: VecDeque<String>,
    reject: Option<String>,
    unsupported: bool,
    connected: bool,
    pause_send: Option<(Arc<Notify>, Arc<Notify>)>,
    pause_poll: Option<(Arc<Notify>, Arc<Notify>)>,
    pause_disconnect: Option<(Arc<Notify>, Arc<Notify>)>,
    lose_disconnect_response: bool,
    disconnect_requests: usize,
    response_override: Option<(String, u16, Value)>,
    tamper: bool,
    plaintext: bool,
    /// The handshake patterns whose first message is answered 401.
    refuse: HashSet<String>,
    /// The patterns answered 401 so far.
    refused: Vec<String>,
    /// The status of the answer being built, when not 200.
    status_once: Option<u16>,
    /// Behave as hivemind-http-protocol does: a message the hub cannot read
    /// -- or a client key that contradicts its pin -- aborts the session,
    /// the request that carried it is answered as usual, and every later
    /// request of the session is answered 401 until the next `/connect`.
    carrier: bool,
    /// The session was aborted.
    aborted: bool,
    /// Refuse, with 401, the first binary frame after a completed handshake.
    refuse_after_handshake: bool,
}
impl HttpFixtureState {
    fn enqueue(&mut self, writes: Vec<NoiseWrite>) {
        for write in writes {
            if write.binary {
                self.binary
                    .push_back(general_purpose::STANDARD.encode(write.payload))
            } else {
                self.plain
                    .push_back(String::from_utf8(write.payload).unwrap())
            }
        }
    }
    fn request(&mut self, path: &str, body: &[u8], cookie: bool) -> Value {
        if path == "/disconnect" {
            self.disconnect_requests += 1;
        }
        if let Some((override_path, _, value)) = &self.response_override {
            if override_path == path {
                return value.clone();
            }
        }
        if self.reject.as_deref() == Some(path) {
            return json!({"error":"synthetic error"});
        }
        if path != "/connect" && !cookie {
            return json!({"error":"missing replica cookie"});
        }
        if self.carrier && self.aborted && !matches!(path, "/connect" | "/disconnect") {
            // The hub's listener refuses a session it no longer holds.
            self.status_once = Some(401);
            return json!({"error":"Unauthorized"});
        }
        match path {
            "/connect" => {
                if self.connected {
                    return json!({"status":"Connected"});
                }
                self.connected = true;
                self.aborted = false;
                self.plain.clear();
                self.binary.clear();
                let writes = self.responder.reset();
                self.enqueue(writes);
                if self.unsupported {
                    self.plain.pop_back();
                    self.plain.push_back(serde_json::to_string(&json!({"msg_type":"shake","payload":{"noise":{"patterns":[],"suites":[]}}})).unwrap());
                }
                json!({"status":"Connected"})
            }
            "/disconnect" => {
                if !self.connected {
                    return json!({"error":"Already Disconnected"});
                }
                self.connected = false;
                json!({"status":"Disconnected"})
            }
            "/get_messages" => {
                let mut messages = self.plain.drain(..).collect::<Vec<_>>();
                if self.plaintext {
                    messages.push(
                        serde_json::to_string(
                            &json!({"msg_type":"bus","payload":{"type":"untrusted"}}),
                        )
                        .unwrap(),
                    );
                }
                json!({"messages":messages})
            }
            "/get_binary_messages" => {
                let mut frames = self.binary.drain(..).collect::<Vec<_>>();
                if self.tamper && !frames.is_empty() {
                    let mut raw = general_purpose::STANDARD.decode(&frames[0]).unwrap();
                    raw[0] ^= 1;
                    frames[0] = general_purpose::STANDARD.encode(raw);
                }
                json!({"b64_messages":frames})
            }
            "/send_message" => {
                let form = url::form_urlencoded::parse(body)
                    .into_owned()
                    .collect::<std::collections::HashMap<_, _>>();
                let binary = form.get("binary").is_some_and(|value| value == "1");
                let raw = if binary {
                    general_purpose::STANDARD.decode(&form["message"]).unwrap()
                } else {
                    form["message"].as_bytes().to_vec()
                };
                let pattern = (!binary)
                    .then(|| serde_json::from_slice::<HiveMessage>(&raw).ok())
                    .flatten()
                    .and_then(|message| {
                        message
                            .payload
                            .get("noise")?
                            .get("pattern")?
                            .as_str()
                            .map(str::to_string)
                    });
                if binary && self.responder.session.is_some() && self.refuse_after_handshake {
                    self.refuse_after_handshake = false;
                    self.aborted = true;
                    self.status_once = Some(401);
                    return json!({"error":"unauthorized"});
                }
                if let Some(pattern) = pattern.filter(|pattern| self.refuse.contains(pattern)) {
                    self.refused.push(pattern);
                    self.status_once = Some(401);
                    return json!({"error":"unauthorized"});
                }
                match self.responder.receive(&raw, binary) {
                    Ok(writes) => {
                        self.enqueue(writes);
                        json!({"status":"message sent"})
                    }
                    Err(_) if self.carrier => {
                        self.aborted = true;
                        self.responder.handshake = None;
                        self.responder.session = None;
                        json!({"status":"message sent"})
                    }
                    Err(_) => json!({"error":"rejected message"}),
                }
            }
            _ => json!({"error":"unknown path"}),
        }
    }
}

struct HttpFixture {
    state: Arc<Mutex<HttpFixtureState>>,
    endpoint: String,
    cert: Vec<u8>,
    task: JoinHandle<()>,
}
impl HttpFixture {
    async fn new() -> Self {
        let (acceptor, cert, _) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(HttpFixtureState {
            responder: Responder::new(),
            plain: VecDeque::new(),
            binary: VecDeque::new(),
            reject: None,
            unsupported: false,
            connected: false,
            pause_send: None,
            pause_poll: None,
            pause_disconnect: None,
            lose_disconnect_response: false,
            disconnect_requests: 0,
            response_override: None,
            tamper: false,
            plaintext: false,
            refuse: HashSet::new(),
            refused: vec![],
            status_once: None,
            carrier: false,
            aborted: false,
            refuse_after_handshake: false,
        }));
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let state = server_state.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return; // A cancelled client can close during the TLS handshake.
                    };
                    let mut bytes = vec![];
                    let mut chunk = [0; 4096];
                    let header_end;
                    loop {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            header_end = pos + 4;
                            break;
                        }
                    }
                    let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    let lower = header.to_lowercase();
                    let length = lower
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map(|value| value.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    while bytes.len() < header_end + length {
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                    }
                    let uri = header.split_whitespace().nth(1).unwrap();
                    let path = uri.split('?').next().unwrap();
                    let cookie = lower.contains("hivemind_http_replica=test-replica");
                    let pause = if path == "/send_message" {
                        state.lock().await.pause_send.take()
                    } else if path == "/get_messages" {
                        state.lock().await.pause_poll.take()
                    } else if path == "/disconnect" {
                        state.lock().await.pause_disconnect.take()
                    } else {
                        None
                    };
                    if let Some((entered, resume)) = &pause {
                        entered.notify_one();
                        resume.notified().await;
                    }
                    let (body, status, lose_response) = {
                        let mut state = state.lock().await;
                        let body = state.request(path, &bytes[header_end..], cookie);
                        let status = state.status_once.take().unwrap_or_else(|| {
                            state
                                .response_override
                                .as_ref()
                                .filter(|(override_path, _, _)| override_path == path)
                                .map_or(200, |(_, status, _)| *status)
                        });
                        let lose_response = path == "/disconnect"
                            && std::mem::take(&mut state.lose_disconnect_response);
                        (body, status, lose_response)
                    };
                    if lose_response {
                        // Remote cleanup completed, but its acknowledgment never reached the client.
                        return;
                    }
                    if pause.is_some() {
                        state.lock().await.reject = None;
                    }
                    let encoded = serde_json::to_vec(&body).unwrap();
                    let cookie_header = if path == "/connect" {
                        "Set-Cookie: hivemind_http_replica=test-replica; Path=/; Secure\r\n"
                    } else {
                        ""
                    };
                    let headers=format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",encoded.len(),cookie_header);
                    if stream.write_all(headers.as_bytes()).await.is_err() {
                        return;
                    }
                    if stream.write_all(&encoded).await.is_err() {
                        return;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            state,
            endpoint,
            cert,
            task,
        }
    }
    fn transport(&self) -> HttpTransport {
        http_transport(&self.endpoint, &self.cert)
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn http_identity(endpoint: &str) -> Identity {
    Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"https":endpoint}})).unwrap()
}

/// An HTTPS polling transport that trusts the fixture's certificate.
fn http_transport(endpoint: &str, cert: &[u8]) -> HttpTransport {
    HttpTransport::with_options_and_http_client_builder(
        http_identity(endpoint),
        DEFAULT_USER_AGENT,
        Duration::from_secs(3600),
        reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_der(cert).unwrap())
            .timeout(Duration::from_secs(3)),
    )
    .unwrap()
}

#[tokio::test]
async fn http_disconnect_lost_acknowledgment_retries_without_resetting_trust() {
    for reconnect_directly in [false, true] {
        let fixture = HttpFixture::new().await;
        let transport = fixture.transport();
        let dir = FixtureDir::new();
        transport.set_noise_state_dir(Some(dir.0.clone())).await;
        transport.connect().await.unwrap();
        let pin = load_noise_pin(Some(&dir.0), "test-hub").unwrap();
        fixture.state.lock().await.lose_disconnect_response = true;
        assert!(transport.disconnect().await.is_err());
        assert!(transport.state.admitted.load(Ordering::Acquire));
        assert!(!fixture.state.lock().await.connected);
        if !reconnect_directly {
            transport.disconnect().await.unwrap();
            assert!(!transport.state.admitted.load(Ordering::Acquire));
        }
        transport.connect().await.unwrap();
        assert!(transport.healthcheck().await.handshake_complete);
        assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
        {
            let state = fixture.state.lock().await;
            assert_eq!(state.disconnect_requests, 2);
            assert_eq!(state.responder.patterns, vec!["XXpsk2", "KKpsk0"]);
        }
        transport.disconnect().await.unwrap();
    }
}

#[tokio::test]
async fn http_disconnect_idempotent_acknowledgments_are_narrowly_scoped() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    // Use a real admission/Noise session to establish affinity and owned state.
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    for (status, body) in [
        (200, json!({"error":"synthetic refusal"})),
        (200, json!({"error":"Already Disconnected", "ok":false})),
        (
            200,
            json!({"error":"Already Disconnected", "status":"Connected"}),
        ),
        (200, json!({"error":"Already Disconnected", "ok":true})),
        (200, json!({"status":"Disconnected", "ok":false})),
        (503, json!({"error":"Already Disconnected"})),
        (200, json!({"error":"already disconnected"})),
    ] {
        fixture.state.lock().await.response_override = Some(("/disconnect".into(), status, body));
        assert!(transport.disconnect().await.is_err());
        assert!(transport.state.admitted.load(Ordering::Acquire));
        assert!(fixture.state.lock().await.connected);
    }
    for path in ["/connect", "/send_message", "/get_messages"] {
        fixture.state.lock().await.response_override =
            Some((path.into(), 200, json!({"error":"Already Disconnected"})));
        assert!(transport
            .request(reqwest::Method::POST, path, None)
            .await
            .is_err());
    }
    fixture.state.lock().await.connected = false;
    for message in ["Already Disconnected", "Client is not connected"] {
        transport.state.admitted.store(true, Ordering::Release);
        fixture.state.lock().await.response_override =
            Some(("/disconnect".into(), 200, json!({"error":message})));
        transport.disconnect().await.unwrap();
        assert!(!transport.state.admitted.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn http_noise_tls_cookie_reconnect_and_concurrent_chunks() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let mut events = transport.subscribe();
    for _ in 0..2 {
        transport.connect().await.unwrap();
        assert!(transport.healthcheck().await.handshake_complete);
        assert!(transport.remote_static_key().await.is_some());
        let mut tasks = vec![];
        for i in 0..4 {
            let transport = transport.clone();
            tasks.push(tokio::spawn(async move {
                transport
                    .emit_bus(
                        "echo",
                        json!({"text":"x".repeat(140000)})
                            .as_object()
                            .unwrap()
                            .clone(),
                        json!({"request_id":i.to_string()})
                            .as_object()
                            .unwrap()
                            .clone(),
                    )
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap()
        }
        transport.poll_once().await.unwrap();
        let mut ids = HashSet::new();
        for _ in 0..4 {
            let event = timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap();
            ids.insert(event.context["request_id"].as_str().unwrap().to_owned());
            assert_eq!(event.data["text"].as_str().unwrap().len(), 140000);
        }
        assert_eq!(ids.len(), 4);
        transport.disconnect().await.unwrap();
        assert!(transport.remote_static_key().await.is_none());
        assert!(!transport.healthcheck().await.handshake_complete);
    }
    let state = fixture.state.lock().await;
    assert_eq!(state.responder.patterns, vec!["XXpsk2", "KKpsk0"]);
    assert_eq!(state.responder.hellos, 2);
}

#[tokio::test]
async fn client_event_stream_uses_authenticated_http_and_reports_disconnect() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let client = crate::Client {
        identity: transport.identity().clone(),
        transport: RuntimeTransport::Http(transport.clone()),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    let mut events = client
        .listen(
            "echo",
            crate::ListenOptions {
                timeout: Some(Duration::from_secs(12)),
                request_id: Some("ours".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(client.healthcheck().await.handshake_complete);
    let waiting_client = client.clone();
    let waiting = tokio::spawn(async move {
        waiting_client
            .wait_for_event("never", crate::ListenOptions::default())
            .await
    });
    timeout(Duration::from_secs(2), async {
        while transport.state.bus_tx.receiver_count() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    assert_eq!(transport.state.bus_tx.receiver_count(), 1);
    assert!(client.healthcheck().await.handshake_complete);
    for id in ["foreign", "ours"] {
        client
            .emit(
                "echo",
                Map::new(),
                json!({"request_id": id}).as_object().unwrap().clone(),
            )
            .await
            .unwrap();
    }
    transport.poll_once().await.unwrap();
    assert_eq!(
        events
            .recv()
            .await
            .unwrap()
            .unwrap()
            .request_id()
            .as_deref(),
        Some("ours")
    );
    let mut deadline_stream = client
        .listen(
            "never",
            crate::ListenOptions {
                timeout: Some(Duration::from_millis(250)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let held_noise = transport.state.noise.lock().await;
    assert!(matches!(
        timeout(Duration::from_secs(2), deadline_stream.recv())
            .await
            .unwrap(),
        Err(ThalovantError::Timeout(_))
    ));
    drop(held_noise);
    assert_eq!(transport.state.bus_tx.receiver_count(), 1);
    client.close().await.unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap(),
        Err(ThalovantError::Connection(_))
    ));
    assert!(events.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn http_noise_rejects_errors_plaintext_tampering_and_changed_pin() {
    for failure in [
        "wrong-password",
        "unsupported",
        "connect-error",
        "send-error",
        "plaintext",
        "tamper",
        "changed-pin",
    ] {
        let fixture = HttpFixture::new().await;
        let mut transport = fixture.transport();
        let dir = FixtureDir::new();
        if failure == "wrong-password" {
            let mut identity = transport.identity().clone();
            identity.password = "wrong".into();
            transport = HttpTransport::with_options_and_http_client_builder(
                identity,
                DEFAULT_USER_AGENT,
                Duration::from_secs(3600),
                reqwest::Client::builder()
                    .add_root_certificate(reqwest::Certificate::from_der(&fixture.cert).unwrap())
                    .timeout(Duration::from_secs(3)),
            )
            .unwrap();
        }
        transport.set_noise_state_dir(Some(dir.0.clone())).await;
        if failure == "unsupported" {
            fixture.state.lock().await.unsupported = true
        }
        if failure == "connect-error" {
            fixture.state.lock().await.reject = Some("/connect".into())
        }
        let result = transport.connect().await;
        if matches!(failure, "wrong-password" | "unsupported" | "connect-error") {
            assert!(result.is_err(), "{failure}");
            assert!(!transport.healthcheck().await.handshake_complete);
            continue;
        }
        result.unwrap();
        if failure == "changed-pin" {
            let pin = transport.remote_static_key().await.unwrap();
            transport.disconnect().await.unwrap();
            {
                let mut state = fixture.state.lock().await;
                state.responder.key = Responder::new().key;
                state.responder.peer = None;
            }
            assert!(transport.connect().await.is_err());
            assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), Some(pin));
            continue;
        }
        {
            let mut state = fixture.state.lock().await;
            state.plaintext = failure == "plaintext";
            state.tamper = failure == "tamper";
            if failure == "send-error" {
                state.reject = Some("/send_message".into())
            }
        }
        let result = transport.emit_bus("echo", Map::new(), Map::new()).await;
        if failure == "send-error" {
            assert!(result.is_err())
        } else {
            result.unwrap();
            assert!(transport.poll_once().await.is_err(), "{failure}")
        }
        assert!(!transport.healthcheck().await.handshake_complete);
        assert!(transport.remote_static_key().await.is_none());
    }
}

#[tokio::test]
async fn noise_offer_alone_never_marks_transport_ready() {
    let mut responder = Responder::new();
    let writes = responder.reset();
    let dir = FixtureDir::new();
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid"})).unwrap();
    let mut channel = NoiseChannel::new(identity, Some(dir.0.clone()), false);
    let mut output = vec![];
    for write in writes {
        output.extend(channel.receive(&write.payload, false).unwrap().1)
    }
    assert_eq!(output.len(), 1);
    assert!(!channel.ready());
    assert!(channel
        .encode(&HiveMessage {
            msg_type: "bus".into(),
            ..Default::default()
        })
        .is_err());
}

async fn mqtt_packet<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<(u8, Vec<u8>)> {
    let header = stream.read_u8().await?;
    let mut length = 0usize;
    let mut scale = 1usize;
    loop {
        let byte = stream.read_u8().await?;
        length += (byte as usize & 127) * scale;
        if byte & 128 == 0 {
            break;
        }
        scale *= 128;
        if scale > 128 * 128 * 128 {
            return Err(std::io::Error::other("invalid packet length"));
        }
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    Ok((header, payload))
}
async fn mqtt_write<S: AsyncWrite + Unpin>(
    stream: &mut S,
    header: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut packet = vec![header];
    let mut length = payload.len();
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 128
        }
        packet.push(byte);
        if length == 0 {
            break;
        }
    }
    packet.extend_from_slice(payload);
    stream.write_all(&packet).await?;
    // A TLS writer may accept the final plaintext bytes while retaining the
    // last encrypted record. The broker must flush before waiting for input.
    stream.flush().await
}

#[tokio::test]
async fn mqtt_fixture_flushes_the_complete_packet_before_reading_again() {
    let (writer, mut reader) = tokio::io::duplex(64);
    let mut writer = tokio::io::BufWriter::new(writer);
    mqtt_write(&mut writer, 0x20, &[0, 0]).await.unwrap();
    let packet = timeout(Duration::from_secs(1), mqtt_packet(&mut reader))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(packet, (0x20, vec![0, 0]));
}
async fn serve_mqtt<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    responder: &mut Responder,
    topics: &MqttTopicSet,
) -> Result<()> {
    let mut started = false;
    loop {
        let (header, body) = match mqtt_packet(stream).await {
            Ok(packet) => packet,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        match header >> 4 {
            1 => {
                mqtt_write(stream, 0x20, &[0, 0]).await?;
            }
            8 => {
                mqtt_write(stream, 0x90, &[body[0], body[1], 1]).await?;
            }
            3 => {
                let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                let topic = std::str::from_utf8(&body[2..2 + topic_len]).unwrap();
                let mut offset = topic_len + 2;
                if (header >> 1) & 3 > 0 {
                    mqtt_write(stream, 0x40, &body[offset..offset + 2]).await?;
                    offset += 2
                }
                if topic != topics.inbound {
                    continue;
                }
                let writes = if !started {
                    let hello: HiveMessage = serde_json::from_slice(&body[offset..])?;
                    assert_eq!(hello.msg_type, "hello");
                    started = true;
                    responder.reset()
                } else {
                    responder.receive(&body[offset..], responder.session.is_some())?
                };
                for write in writes {
                    let mut payload = (topics.outbound.len() as u16).to_be_bytes().to_vec();
                    payload.extend_from_slice(topics.outbound.as_bytes());
                    payload.extend_from_slice(&write.payload);
                    mqtt_write(stream, 0x30, &payload).await?;
                }
            }
            12 => {
                mqtt_write(stream, 0xd0, &[]).await?;
            }
            14 => return Ok(()),
            _ => {}
        }
    }
}

#[tokio::test]
async fn mqtt_noise_tls_broker_reconnect_and_wrong_password() {
    for wrong_password in [false, true] {
        let (acceptor, _, ca) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":if wrong_password{"wrong"}else{test_password()},"default_master":"https://example.invalid","mqtt":{"endpoint":format!("mqtts://{}",listener.local_addr().unwrap()),"username":"broker-user","password":"broker-password","topic_prefix":"test","tls":true,"qos":1}})).unwrap();
        let transport = MqttTransport::new(identity).unwrap();
        let dir = FixtureDir::new();
        transport.set_noise_state_dir(Some(dir.0.clone())).await;
        transport
            .set_tls_configuration(Some(TlsConfiguration::SimpleNative {
                ca,
                client_auth: None,
            }))
            .await;
        let mut events = transport.subscribe();
        let topics = transport.topics().clone();
        let attempts = if wrong_password { 1 } else { 2 };
        let broker = tokio::spawn(async move {
            let mut responder = Responder::new();
            for _ in 0..attempts {
                let (stream, _) = listener.accept().await.unwrap();
                stream.set_nodelay(true).unwrap();
                let mut stream = acceptor.accept(stream).await.unwrap();
                let result = serve_mqtt(&mut stream, &mut responder, &topics).await;
                if !wrong_password {
                    result.unwrap()
                }
            }
            responder.patterns
        });
        for _ in 0..attempts {
            let result = timeout(Duration::from_secs(12), transport.connect())
                .await
                .unwrap();
            if wrong_password {
                assert!(result.is_err());
                assert!(!transport.healthcheck().await.handshake_complete);
                break;
            }
            result.unwrap();
            assert!(transport.remote_static_key().await.is_some());
            transport
                .emit_bus(
                    "echo",
                    json!({"text":"x".repeat(1200000)})
                        .as_object()
                        .unwrap()
                        .clone(),
                    json!({"request_id":"mqtt"}).as_object().unwrap().clone(),
                )
                .await
                .unwrap();
            // This exchanges 1.2 MiB through native TLS and many Noise chunks;
            // Windows SChannel runners need a transfer budget, not a 2s echo budget.
            let event = match timeout(Duration::from_secs(10), events.recv()).await {
                Ok(result) => result.unwrap(),
                Err(error) => panic!(
                    "MQTT echo deadline: {error}; health={:?}; broker_finished={}",
                    transport.healthcheck().await,
                    broker.is_finished()
                ),
            };
            assert_eq!(event.context["request_id"], "mqtt");
            transport.disconnect().await.unwrap();
            assert!(transport.remote_static_key().await.is_none());
            assert!(!transport.healthcheck().await.handshake_complete);
        }
        let patterns = timeout(Duration::from_secs(2), broker)
            .await
            .unwrap()
            .unwrap();
        if !wrong_password {
            assert_eq!(patterns, vec!["XXpsk2", "KKpsk0"])
        }
    }
}

#[tokio::test]
async fn wss_noise_same_object_reconnect_after_encrypted_reply() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap();
    let transport = WssTransport::new(identity);
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let mut events = transport.subscribe();
    let server = tokio::spawn(async move {
        let mut responder = Responder::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio_tungstenite::accept_async(stream).await.unwrap();
            for write in responder.reset() {
                stream
                    .send(WebSocketMessage::Text(
                        String::from_utf8(write.payload).unwrap(),
                    ))
                    .await
                    .unwrap()
            }
            while let Some(message) = stream.next().await {
                let (raw, binary) = match message.unwrap() {
                    WebSocketMessage::Text(text) => (text.into_bytes(), false),
                    WebSocketMessage::Binary(bytes) => (bytes, true),
                    WebSocketMessage::Close(_) => break,
                    _ => continue,
                };
                for write in responder.receive(&raw, binary).unwrap() {
                    stream
                        .send(if write.binary {
                            WebSocketMessage::Binary(write.payload)
                        } else {
                            WebSocketMessage::Text(String::from_utf8(write.payload).unwrap())
                        })
                        .await
                        .unwrap()
                }
            }
        }
        responder.patterns
    });
    let mut pin = None;
    for _ in 0..2 {
        timeout(Duration::from_secs(12), transport.connect())
            .await
            .unwrap()
            .unwrap();
        let key = transport.remote_static_key().await.unwrap();
        if let Some(pin) = &pin {
            assert_eq!(pin, &key)
        } else {
            pin = Some(key)
        }
        transport
            .emit_bus(
                "echo",
                Map::new(),
                json!({"request_id":"wss"}).as_object().unwrap().clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap()
                .context["request_id"],
            "wss"
        );
        transport.disconnect().await.unwrap();
        assert!(transport.remote_static_key().await.is_none());
        assert!(transport.state.noise.lock().await.is_none());
    }
    assert_eq!(
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap(),
        vec!["XXpsk2", "KKpsk0"]
    );
}

#[tokio::test]
async fn http_noise_refuses_redirect_even_with_permissive_custom_builder() {
    for scheme in ["http", "https"] {
        let (acceptor, cert, _) = tls_fixture();
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", source.local_addr().unwrap());
        let location = format!("{scheme}://{}", target.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = source.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"https":endpoint}})).unwrap();
        let transport = HttpTransport::with_options_and_http_client_builder(
            identity,
            DEFAULT_USER_AGENT,
            Duration::from_secs(1),
            reqwest::Client::builder()
                .add_root_certificate(reqwest::Certificate::from_der(&cert).unwrap())
                .redirect(reqwest::redirect::Policy::limited(10)),
        )
        .unwrap();
        assert!(transport.connect().await.is_err());
        assert!(!transport.healthcheck().await.handshake_complete);
        assert!(
            timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err(),
            "redirect target received a connection"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn wss_failed_kk_keeps_the_authenticated_hub_pin() {
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid"})).unwrap();
    let transport = WssTransport::new(identity);
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let peer = Responder::new().key;
    let pin = hex::encode(peer.public);
    pin_hub_key(Some(&dir.0), "test-hub", &pin).unwrap();
    let key = load_or_create_noise_key(Some(&dir.0)).unwrap();
    let psk = derive_psk(test_password(), "test-hub").unwrap();
    let mut handshake = NoiseHandshake::new(
        "KKpsk0",
        "25519_ChaChaPoly_SHA256",
        &psk,
        &[],
        &key,
        Some(&pin),
    )
    .unwrap();
    handshake.write_message(&[]).unwrap();
    let mut channel = NoiseChannel::new(transport.identity().clone(), Some(dir.0.clone()), false);
    channel.handshake = Some(handshake);
    channel.node_id = "test-hub".into();
    assert!(channel
        .continue_handshake(json!({"msg":"00"}).as_object().unwrap())
        .is_err());
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), Some(pin));
}

#[tokio::test]
async fn http_noise_reconnect_resets_previously_admitted_server_session() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    fixture.state.lock().await.tamper = true;
    transport
        .emit_bus("echo", Map::new(), Map::new())
        .await
        .unwrap();
    assert!(transport.poll_once().await.is_err());
    fixture.state.lock().await.tamper = false;
    timeout(Duration::from_secs(10), transport.connect())
        .await
        .unwrap()
        .unwrap();
    assert!(transport.healthcheck().await.handshake_complete);
    assert_eq!(
        fixture.state.lock().await.responder.patterns,
        vec!["XXpsk2", "KKpsk0"]
    );
    transport.disconnect().await.unwrap();
}

#[tokio::test]
async fn http_reconnect_waits_for_failed_inflight_send_cleanup() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    {
        let mut state = fixture.state.lock().await;
        state.pause_send = Some((entered.clone(), resume.clone()));
        state.reject = Some("/send_message".into());
    }
    let sender = transport.clone();
    let send = tokio::spawn(async move { sender.emit_bus("old", Map::new(), Map::new()).await });
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let connector = transport.clone();
    let reconnect = tokio::spawn(async move { connector.connect().await });
    sleep(Duration::from_millis(50)).await;
    assert!(!reconnect.is_finished());
    resume.notify_one();
    assert!(send.await.unwrap().is_err());
    timeout(Duration::from_secs(10), reconnect)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(transport.healthcheck().await.handshake_complete);
    let mut events = transport.subscribe();
    transport
        .emit_bus("new", Map::new(), Map::new())
        .await
        .unwrap();
    transport.poll_once().await.unwrap();
    assert_eq!(events.recv().await.unwrap().name, "new");
    transport.disconnect().await.unwrap();
}

#[tokio::test]
async fn http_reconnect_waits_for_failed_caller_poll_cleanup() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    transport.stop_polling().await;
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    {
        let mut state = fixture.state.lock().await;
        state.pause_poll = Some((entered.clone(), resume.clone()));
        state.reject = Some("/get_messages".into());
    }
    let poller = transport.clone();
    let poll = tokio::spawn(async move { poller.poll_once().await });
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let connector = transport.clone();
    let reconnect = tokio::spawn(async move { connector.connect().await });
    sleep(Duration::from_millis(50)).await;
    assert!(!reconnect.is_finished());
    assert_eq!(
        fixture.state.lock().await.responder.patterns,
        vec!["XXpsk2"]
    );
    resume.notify_one();
    assert!(poll.await.unwrap().is_err());
    timeout(Duration::from_secs(10), reconnect)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(transport.healthcheck().await.handshake_complete);
    assert_eq!(
        fixture.state.lock().await.responder.patterns,
        vec!["XXpsk2", "KKpsk0"]
    );
    transport.disconnect().await.unwrap();
}

fn exchange_with_channel(channel: &mut NoiseChannel, responder: &mut Responder) -> Result<()> {
    let mut incoming = VecDeque::from(responder.reset());
    while let Some(write) = incoming.pop_front() {
        let (_, outgoing) = channel.receive(&write.payload, write.binary)?;
        for write in outgoing {
            incoming.extend(responder.receive(&write.payload, write.binary)?);
        }
    }
    Ok(())
}

#[test]
fn shared_noise_channel_reuses_persisted_psk_and_recovers_after_rejection() {
    let dir = FixtureDir::new();
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid"})).unwrap();
    let mut responder = Responder::new();
    // A deliberately different cached credential makes reuse observable without
    // timing or counting calls to the implementation's derivation function.
    let cached = [0x43; 32];
    save_cached_psk(Some(&dir.0), "test-hub", &cached).unwrap();
    responder.psk = cached;
    let mut channel = NoiseChannel::new(identity.clone(), Some(dir.0.clone()), false);
    exchange_with_channel(&mut channel, &mut responder).unwrap();
    assert!(channel.ready());
    assert_eq!(responder.hellos, 1);
    let pin = load_noise_pin(Some(&dir.0), "test-hub").unwrap();
    assert!(pin.is_some());

    // The password rotates back to the identity's current value. Force XX so
    // the peer can return the response that authenticates and rejects our PSK.
    responder.psk = derive_psk(test_password(), "test-hub").unwrap();
    responder.peer = None;
    let mut stale = NoiseChannel::new(identity.clone(), Some(dir.0.clone()), false);
    assert!(exchange_with_channel(&mut stale, &mut responder).is_err());
    assert!(!stale.ready());
    assert_eq!(load_cached_psk(Some(&dir.0), "test-hub").unwrap(), None);
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);

    let mut recovered = NoiseChannel::new(identity, Some(dir.0.clone()), false);
    exchange_with_channel(&mut recovered, &mut responder).unwrap();
    assert!(recovered.ready());
    assert_eq!(
        load_cached_psk(Some(&dir.0), "test-hub").unwrap(),
        Some(responder.psk)
    );
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
}

#[test]
fn shared_noise_rejects_duplicate_hello_and_a_changed_pinned_peer() {
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid"})).unwrap();
    let dir = FixtureDir::new();
    let mut responder = Responder::new();
    let hello = responder.reset().remove(0);
    let mut channel = NoiseChannel::new(identity.clone(), Some(dir.0.clone()), false);
    channel.receive(&hello.payload, false).unwrap();
    assert!(channel.receive(&hello.payload, false).is_err());
    assert!(!channel.ready());

    let mut first = NoiseChannel::new(identity.clone(), Some(dir.0.clone()), false);
    exchange_with_channel(&mut first, &mut responder).unwrap();
    let pin = load_noise_pin(Some(&dir.0), "test-hub").unwrap();
    let mut replacement = Responder::new();
    let mut reconnect = NoiseChannel::new(identity, Some(dir.0.clone()), false);
    assert!(exchange_with_channel(&mut reconnect, &mut replacement).is_err());
    assert!(!reconnect.ready());
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_a_send_left_mid_write_is_finished_and_only_a_failed_write_poisons() {
    // One encrypted chunk fits in the receive window on every platform, while
    // the 16 MiB message still exceeds loopback buffers after the peer stops.
    // The peer signals an actual application chunk before cancellation.
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(128 * 1024).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = socket.listen(128).unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap();
    let transport = WssTransport::new(identity);
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let paused = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (server_paused, server_release) = (paused.clone(), release.clone());
    let server = tokio::spawn(async move {
        let mut responder = Responder::new();
        for attempt in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio_tungstenite::accept_async(stream).await.unwrap();
            for write in responder.reset() {
                stream
                    .send(WebSocketMessage::Text(
                        String::from_utf8(write.payload).unwrap(),
                    ))
                    .await
                    .unwrap();
            }
            while let Some(message) = stream.next().await {
                let (raw, binary) = match message.unwrap() {
                    WebSocketMessage::Text(text) => (text.into_bytes(), false),
                    WebSocketMessage::Binary(bytes) => (bytes, true),
                    WebSocketMessage::Close(_) => break,
                    _ => continue,
                };
                let application_chunk = attempt == 0 && responder.hellos == 1;
                for write in responder.receive(&raw, binary).unwrap() {
                    stream
                        .send(if write.binary {
                            WebSocketMessage::Binary(write.payload)
                        } else {
                            WebSocketMessage::Text(String::from_utf8(write.payload).unwrap())
                        })
                        .await
                        .unwrap();
                }
                if application_chunk {
                    server_paused.notify_one();
                    server_release.notified().await;
                    break;
                }
            }
        }
        responder.patterns
    });
    transport.connect().await.unwrap();
    let sender = transport.clone();
    let send = tokio::spawn(async move {
        sender
            .emit_bus(
                "large",
                json!({"body":"x".repeat(16*1024*1024)})
                    .as_object()
                    .unwrap()
                    .clone(),
                Map::new(),
            )
            .await
    });
    timeout(Duration::from_secs(15), paused.notified())
        .await
        .expect("peer must receive the first encrypted application chunk");
    assert!(!send.is_finished());
    send.abort();
    assert!(send.await.unwrap_err().is_cancelled());
    // The caller left mid-write, but half a message would break the Noise
    // stream: the write goes on, and the session is not spoilt by it.
    assert!(transport.state.session_valid.load(Ordering::Acquire));
    assert!(transport.healthcheck().await.handshake_complete);
    // The peer goes away before reading the rest: the write fails, and that
    // is what spoils the session.
    release.notify_one();
    timeout(Duration::from_secs(15), async {
        while transport.healthcheck().await.handshake_complete {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a failed write spoils the session");
    assert!(transport.remote_static_key().await.is_none());
    assert!(transport
        .emit_bus("rejected", Map::new(), Map::new())
        .await
        .is_err());
    transport.connect().await.unwrap();
    let mut events = transport.subscribe();
    transport
        .emit_bus("echo", Map::new(), Map::new())
        .await
        .unwrap();
    timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    transport.disconnect().await.unwrap();
    assert_eq!(server.await.unwrap(), vec!["XXpsk2", "KKpsk0"]);
}

#[test]
fn shared_noise_rederives_after_peer_rejects_kk_before_responding() {
    let dir = FixtureDir::new();
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid"})).unwrap();
    let mut responder = Responder::new();
    let current = responder.psk;
    responder.psk = [0x52; 32];
    save_cached_psk(Some(&dir.0), "test-hub", &responder.psk).unwrap();
    let mut initial = NoiseChannel::new(identity.clone(), Some(dir.0.clone()), false);
    exchange_with_channel(&mut initial, &mut responder).unwrap();
    assert!(initial.ready());
    let pin = load_noise_pin(Some(&dir.0), "test-hub").unwrap();
    let client_key = load_or_create_noise_key(Some(&dir.0)).unwrap();
    drop(initial);
    assert_eq!(
        load_cached_psk(Some(&dir.0), "test-hub").unwrap(),
        Some(responder.psk)
    );

    responder.psk = current;
    let mut stale = NoiseChannel::new(identity.clone(), Some(dir.0.clone()), false);
    // KK fails at the peer while reading message 1, so our receive path never
    // gets a message on which to report an authentication failure.
    assert!(exchange_with_channel(&mut stale, &mut responder).is_err());
    assert!(stale.handshake.is_some());
    assert!(stale.session.is_none());
    drop(stale); // Transport failure/timeout cleanup abandons this channel.
    assert_eq!(load_cached_psk(Some(&dir.0), "test-hub").unwrap(), None);
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
    assert_eq!(load_or_create_noise_key(Some(&dir.0)).unwrap(), client_key);

    let mut recovered = NoiseChannel::new(identity, Some(dir.0.clone()), false);
    exchange_with_channel(&mut recovered, &mut responder).unwrap();
    assert!(recovered.ready());
    assert_eq!(
        load_cached_psk(Some(&dir.0), "test-hub").unwrap(),
        Some(current)
    );
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
}

#[tokio::test]
async fn concurrent_connect_waits_for_authentication_and_joiner_timeout_is_local() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let identity = Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap();
    let wss = WssTransport::new(identity.clone());
    let dir = FixtureDir::new();
    wss.set_noise_state_dir(Some(dir.0.clone())).await;
    let client = crate::Client {
        identity,
        transport: RuntimeTransport::Wss(wss.clone()),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let server_entered = entered.clone();
    let server_resume = resume.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = tokio_tungstenite::accept_async(stream).await.unwrap();
        server_entered.notify_one();
        server_resume.notified().await;
        let mut responder = Responder::new();
        for write in responder.reset() {
            stream
                .send(WebSocketMessage::Text(
                    String::from_utf8(write.payload).unwrap(),
                ))
                .await
                .unwrap();
        }
        while let Some(message) = stream.next().await {
            let (raw, binary) = match message.unwrap() {
                WebSocketMessage::Text(text) => (text.into_bytes(), false),
                WebSocketMessage::Binary(bytes) => (bytes, true),
                WebSocketMessage::Close(_) => break,
                _ => continue,
            };
            for write in responder.receive(&raw, binary).unwrap() {
                stream
                    .send(if write.binary {
                        WebSocketMessage::Binary(write.payload)
                    } else {
                        WebSocketMessage::Text(String::from_utf8(write.payload).unwrap())
                    })
                    .await
                    .unwrap();
            }
        }
        assert!(
            timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "a joining caller created another socket"
        );
        responder.patterns
    });
    let initiator = {
        let client = client.clone();
        tokio::spawn(async move { client.connect_with_timeout(Duration::from_secs(12)).await })
    };
    entered.notified().await;
    timeout(Duration::from_secs(5), async {
        while !wss.state.health.lock().await.connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the initiator never opened the socket");
    assert!(!client.healthcheck().await.handshake_complete);
    assert!(matches!(
        client.connect_with_timeout(Duration::from_millis(30)).await,
        Err(ThalovantError::Timeout(_))
    ));
    assert!(!initiator.is_finished());
    assert!(client
        .list_fallbacks(Some(Duration::from_millis(30)))
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        client
            .wait_for_event(
                "echo",
                crate::ListenOptions {
                    timeout: Some(Duration::from_millis(30)),
                    ..Default::default()
                }
            )
            .await,
        Err(ThalovantError::Timeout(_))
    ));
    assert!(
        !initiator.is_finished(),
        "queued event wait cancelled the connection owner"
    );
    assert!(matches!(
        client
            .ask(
                "hello",
                crate::RequestOptions {
                    timeout: Some(Duration::from_millis(30)),
                    ..Default::default()
                }
            )
            .await,
        Err(ThalovantError::Timeout(_))
    ));
    assert!(!initiator.is_finished());
    let joiner = {
        let client = client.clone();
        tokio::spawn(async move { client.connect_with_timeout(Duration::from_secs(12)).await })
    };
    tokio::task::yield_now().await;
    assert!(!joiner.is_finished());
    resume.notify_one();
    initiator.await.unwrap().unwrap();
    joiner.await.unwrap().unwrap();
    assert!(client.healthcheck().await.handshake_complete);
    client.connect().await.unwrap();
    client.close().await.unwrap();
    assert_eq!(server.await.unwrap(), vec!["XXpsk2"]);
}

#[tokio::test]
async fn cancelling_initiator_retires_its_generation_and_next_connect_recovers() {
    for explicit_close in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let identity = Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap();
        let wss = WssTransport::new(identity);
        let dir = FixtureDir::new();
        wss.set_noise_state_dir(Some(dir.0.clone())).await;
        let transport = RuntimeTransport::Wss(wss.clone());
        let opened = Arc::new(Notify::new());
        let server_opened = opened.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first = tokio_tungstenite::accept_async(stream).await.unwrap();
            server_opened.notify_one();
            // Deliberately never offer Noise on the first socket.
            while let Some(message) = first.next().await {
                if matches!(message, Ok(WebSocketMessage::Close(_)) | Err(_)) {
                    break;
                }
            }
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut responder = Responder::new();
            for write in responder.reset() {
                stream
                    .send(WebSocketMessage::Text(
                        String::from_utf8(write.payload).unwrap(),
                    ))
                    .await
                    .unwrap();
            }
            while let Some(message) = stream.next().await {
                let (raw, binary) = match message.unwrap() {
                    WebSocketMessage::Text(text) => (text.into_bytes(), false),
                    WebSocketMessage::Binary(bytes) => (bytes, true),
                    WebSocketMessage::Close(_) => break,
                    _ => continue,
                };
                for write in responder.receive(&raw, binary).unwrap() {
                    stream
                        .send(if write.binary {
                            WebSocketMessage::Binary(write.payload)
                        } else {
                            WebSocketMessage::Text(String::from_utf8(write.payload).unwrap())
                        })
                        .await
                        .unwrap();
                }
            }
        });
        let initiator = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.connect().await })
        };
        opened.notified().await;
        timeout(Duration::from_secs(5), async {
            while !wss.state.health.lock().await.connected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the initiator never opened the socket");
        let joiner = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.connect().await })
        };
        tokio::task::yield_now().await;
        if explicit_close {
            transport.disconnect().await.unwrap();
            assert!(initiator.await.unwrap().is_err());
        } else {
            initiator.abort();
            assert!(initiator.await.unwrap_err().is_cancelled());
        }
        assert!(joiner.await.unwrap().is_err());
        assert!(!transport.healthcheck().await.connected);
        transport.connect().await.unwrap();
        assert!(transport.healthcheck().await.handshake_complete);
        transport.disconnect().await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn http_cleanup_deadline_preserves_admission_until_acknowledged_retry() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    let entered = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    fixture.state.lock().await.pause_disconnect = Some((entered.clone(), resume.clone()));
    let closing = {
        let transport = transport.clone();
        tokio::spawn(async move { transport.disconnect().await })
    };
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(3), closing)
            .await
            .unwrap()
            .unwrap(),
        Err(ThalovantError::Timeout(_))
    ));
    assert!(transport.state.admitted.load(Ordering::Acquire));
    assert!(!transport.healthcheck().await.connected);
    assert!(fixture.state.lock().await.connected);
    resume.notify_one();
    timeout(Duration::from_secs(2), async {
        while fixture.state.lock().await.connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    transport.connect().await.unwrap();
    assert!(transport.healthcheck().await.handshake_complete);
    assert_eq!(
        fixture.state.lock().await.responder.patterns,
        vec!["XXpsk2", "KKpsk0"]
    );
    transport.disconnect().await.unwrap();
}

#[tokio::test]
async fn an_external_store_lock_does_not_block_the_async_connection_deadline() {
    struct Holder(std::process::Child);
    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let mut holder = Holder(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "noise_store::tests::store_process_worker",
                "--nocapture",
            ])
            .env("THALOVANT_STORE_TEST_DIR", &dir.0)
            .env("THALOVANT_STORE_TEST_OPERATION", "crash-static")
            .env("THALOVANT_STORE_TEST_ID", "0")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    timeout(Duration::from_secs(10), async {
        while !dir.0.join("staged").exists() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let runtime = RuntimeTransport::Http(transport.clone());
    let started = Instant::now();
    let result = runtime
        .connect_with_timeout(Duration::from_millis(50))
        .await;
    assert!(matches!(result, Err(ThalovantError::Timeout(_))));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a blocking file lock stalled the Tokio worker"
    );
    assert!(!transport.healthcheck().await.connected);
    holder.0.kill().unwrap();
    holder.0.wait().unwrap();
    // A fresh attempt owns a new channel; the abandoned worker cannot install its result.
    runtime
        .connect_with_timeout(Duration::from_secs(12))
        .await
        .unwrap();
    assert!(transport.healthcheck().await.handshake_complete);
    transport.disconnect().await.unwrap();
}

#[tokio::test]
async fn close_keeps_cleanup_owned_when_its_caller_stops_waiting() {
    let fixture = HttpFixture::new().await;
    let http = fixture.transport();
    let transport = RuntimeTransport::Http(http.clone());
    let gate = http.state.lifecycle.lock().await;
    http.state.health.lock().await.connection.phase = TransportConnectionPhase::Ready;
    assert!(timeout(Duration::from_millis(30), transport.disconnect())
        .await
        .is_err());
    assert!(http.state.lifecycle.cancelled.load(Ordering::Acquire));
    drop(gate);
    timeout(Duration::from_secs(1), async {
        while http.state.health.lock().await.connection.phase != TransportConnectionPhase::Closed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_mqtt_cleanup_aborts_the_taken_event_loop_task() {
    struct Signal(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Signal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let identity = Identity::from_value(json!({"site_id":"test","key":"test","password":test_password(),"default_master":"https://example.invalid","mqtt":{"endpoint":"mqtts://example.invalid:8883","username":"test","password":"test","topic_prefix":"test","tls":true}})).unwrap();
    let transport = MqttTransport::new(identity).unwrap();
    let (stopped, receiver) = tokio::sync::oneshot::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    *transport.state.event_task.lock().await = Some(tokio::spawn(async move {
        let _signal = Signal(Some(stopped));
        let _ = started.send(());
        std::future::pending::<()>().await;
    }));
    ready.await.unwrap();
    assert!(
        timeout(Duration::from_millis(30), transport.disconnect_inner())
            .await
            .is_err()
    );
    timeout(Duration::from_secs(1), receiver)
        .await
        .unwrap()
        .unwrap();
    assert!(transport.state.event_task.lock().await.is_none());
}

#[tokio::test]
async fn failed_http_admission_reset_refreshes_both_health_errors() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    transport.state.admitted.store(true, Ordering::Release);
    fixture.state.lock().await.reject = Some("/disconnect".into());
    let error = transport.connect_locked().await.unwrap_err();
    let health = transport.healthcheck().await;
    assert_eq!(
        health.last_error.as_deref(),
        Some(error.to_string().as_str())
    );
    assert_eq!(health.connection.last_error, health.last_error);
    assert!(transport.state.admitted.load(Ordering::Acquire));
}

#[tokio::test]
async fn active_reply_ids_reject_duplicates_on_a_shared_authenticated_transport() {
    async fn invoke(
        client: crate::Client,
        query: bool,
        request_id: &str,
        budget: Duration,
    ) -> Result<crate::Reply> {
        if query {
            client
                .query(
                    "question",
                    crate::QueryOptions {
                        request_id: Some(request_id.into()),
                        query_id: Some("shared".into()),
                        timeout: Some(budget),
                        ..Default::default()
                    },
                )
                .await
        } else {
            client
                .ask(
                    "question",
                    crate::RequestOptions {
                        request_id: Some("shared".into()),
                        timeout: Some(budget),
                        ..Default::default()
                    },
                )
                .await
        }
    }
    for query in [false, true] {
        let fixture = HttpFixture::new().await;
        let transport = fixture.transport();
        let dir = FixtureDir::new();
        transport.set_noise_state_dir(Some(dir.0.clone())).await;
        let client = crate::Client {
            identity: transport.identity().clone(),
            transport: RuntimeTransport::Http(transport.clone()),
            conversations: Default::default(),
            conversation_sequence: Default::default(),
        };
        client.connect().await.unwrap();
        let first = tokio::spawn(invoke(
            client.clone(),
            query,
            "first",
            Duration::from_secs(10),
        ));
        timeout(Duration::from_secs(2), async {
            while if query {
                transport.state.hive_tx.receiver_count()
            } else {
                transport.state.bus_tx.receiver_count()
            } == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let duplicate = invoke(
            client.clone(),
            query,
            "different-request-same-query",
            Duration::from_millis(50),
        )
        .await;
        // Equal strings in different matching namespaces must remain independent.
        let independent = invoke(
            client.clone(),
            !query,
            "other-namespace",
            Duration::from_millis(50),
        )
        .await;
        assert!(
            !matches!(independent, Err(ThalovantError::Runtime(_))),
            "Ask and Query namespaces collided: {independent:?}"
        );
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(
            matches!(duplicate, Err(ThalovantError::Runtime(_))),
            "query={query}: duplicate must fail before publication: {duplicate:?}"
        );
        // The dropped collector released its reservation, even through a Client clone.
        let repeated = invoke(client.clone(), query, "again", Duration::from_millis(30)).await;
        assert!(
            !matches!(repeated, Err(ThalovantError::Runtime(_))),
            "cancelled collector leaked its reservation: {repeated:?}"
        );
        let _ = client.close().await;
    }
}

#[tokio::test]
async fn a_peer_close_during_handshake_is_not_reported_as_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let identity = Identity::from_value(json!({"site_id":"test","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap();
    let transport = WssTransport::new(identity);
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(WebSocketMessage::Close(Some(
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code:
                        tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy,
                    reason: "untrusted-password=RAW-SECRET".into(),
                },
            )))
            .await
            .unwrap();
    });
    let error = timeout(Duration::from_secs(2), transport.connect())
        .await
        .unwrap()
        .unwrap_err();
    server.await.unwrap();
    // A 1008 during the handshake is the hub refusing the credentials: said
    // as such, not as a timeout nor as a bare connection failure.
    // Still the Connection it has always been, so an old match catches it.
    assert!(
        matches!(error, ThalovantError::Connection(_)) && error.is_hub_refused(),
        "peer refusal was mislabeled: {error:?}"
    );
    assert!(error.is_connection_error());
    assert!(error.to_string().contains("1008"));
    assert!(!error.to_string().contains("RAW-SECRET"));
    assert!(!transport
        .healthcheck()
        .await
        .last_error
        .unwrap()
        .contains("RAW-SECRET"));
}

/// A fake hub that completes the handshake, then does `after` with the socket.
async fn handshake_then<F, Fut, T>(listener: TcpListener, after: F) -> T
where
    F: FnOnce(WebSocketStream<TcpStream>, Responder) -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let (stream, _) = listener.accept().await.unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    let mut responder = Responder::new();
    for write in responder.reset() {
        socket
            .send(WebSocketMessage::Text(
                String::from_utf8(write.payload).unwrap(),
            ))
            .await
            .unwrap();
    }
    while responder.session.is_none() {
        let (raw, binary) = match socket.next().await.unwrap().unwrap() {
            WebSocketMessage::Text(text) => (text.into_bytes(), false),
            WebSocketMessage::Binary(bytes) => (bytes, true),
            _ => continue,
        };
        for write in responder.receive(&raw, binary).unwrap() {
            socket
                .send(if write.binary {
                    WebSocketMessage::Binary(write.payload)
                } else {
                    WebSocketMessage::Text(String::from_utf8(write.payload).unwrap())
                })
                .await
                .unwrap();
        }
    }
    after(socket, responder).await
}

fn wss_identity(endpoint: &str) -> Identity {
    Identity::from_value(json!({"site_id":"test","key":"test-access","password":test_password(),"default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap()
}

#[tokio::test]
async fn a_close_right_after_the_handshake_says_whether_it_was_a_refusal() {
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
    for (close, refused) in [
        (Some(CloseCode::Policy), true),
        (Some(CloseCode::Normal), true),
        (None, true),
        (Some(CloseCode::Error), false),
        (Some(CloseCode::Again), false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let transport = WssTransport::new(wss_identity(&endpoint));
        let dir = FixtureDir::new();
        transport.set_noise_state_dir(Some(dir.0.clone())).await;
        let server = tokio::spawn(handshake_then(listener, move |mut socket, _| async move {
            socket
                .send(WebSocketMessage::Close(close.map(|code| CloseFrame {
                    code,
                    reason: "".into(),
                })))
                .await
                .unwrap();
        }));
        timeout(Duration::from_secs(12), transport.connect())
            .await
            .unwrap()
            .unwrap();
        let runtime = RuntimeTransport::Wss(transport.clone());
        timeout(Duration::from_secs(2), runtime.stopped())
            .await
            .expect("the close is heard as it happens");
        server.await.unwrap();
        assert_eq!(transport.closed_refused(), refused, "{close:?}");
        assert_eq!(runtime.closed_refused(), refused, "{close:?}");
    }

    // A socket that simply drops is the network's trouble, not a verdict.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let transport = WssTransport::new(wss_identity(&endpoint));
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let server = tokio::spawn(handshake_then(listener, |socket, _| async move {
        drop(socket);
    }));
    timeout(Duration::from_secs(12), transport.connect())
        .await
        .unwrap()
        .unwrap();
    server.await.unwrap();
    timeout(
        Duration::from_secs(2),
        RuntimeTransport::Wss(transport.clone()).stopped(),
    )
    .await
    .unwrap();
    assert!(!transport.closed_refused());
}

#[tokio::test]
async fn a_hub_session_hears_a_refusal_inside_the_settle_window() {
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
    for (code, refused) in [
        (Some(CloseCode::Policy), true),
        (Some(CloseCode::Normal), true),
        (None, true),
        (Some(CloseCode::Error), false),
        (Some(CloseCode::Away), false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let identity = wss_identity(&endpoint);
        let dir = FixtureDir::new();
        let state = dir.0.clone();
        let server = tokio::spawn(handshake_then(listener, move |mut socket, _| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = socket
                .send(WebSocketMessage::Close(code.map(|code| CloseFrame {
                    code,
                    reason: "".into(),
                })))
                .await;
        }));
        let session = crate::HubSession::new(
            move || {
                let identity = identity.clone();
                let state = state.clone();
                async move {
                    let client = crate::Client::with_protocol(identity, HubProtocol::Wss)?;
                    if let RuntimeTransport::Wss(wss) = &client.transport {
                        wss.set_noise_state_dir(Some(state)).await;
                    }
                    // The budget the other WSS fixtures use: the first
                    // handshake runs argon2id at 64 MiB, which a debug build
                    // under a busy runner does not finish in the default 6 s.
                    client.connect_with_timeout(Duration::from_secs(20)).await?;
                    Ok(client)
                }
            },
            crate::HubSessionPolicy::default(),
        )
        .unwrap();
        let result = timeout(Duration::from_secs(30), session.connect())
            .await
            .unwrap();
        server.await.unwrap();
        if refused {
            assert!(
                result.as_ref().is_err_and(ThalovantError::is_hub_refused),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(ThalovantError::Connection(_))),
                "{result:?}"
            );
        }
        assert!(!session.held());
        session.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_hub_session_answers_a_home_request_back_along_its_route() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let identity = wss_identity(&endpoint);
    let dir = FixtureDir::new();
    let state = dir.0.clone();
    let request = HiveMessage {
        msg_type: "bus".into(),
        payload: json!({
            "type": crate::HOME_REQUEST,
            "data": {"request_id": "r-1", "utterance": "turn off the kitchen light",
                "lang": "en-US", "conversation_id": "conv-1"},
            "context": {"source": "thalovant-skill-home", "destination": ["ha-peer", "other"],
                "session": {"session_id": "kitchen"}, "request_id": "r-1"},
        })
        .as_object()
        .unwrap()
        .clone(),
        ..Default::default()
    };
    let server = tokio::spawn(handshake_then(
        listener,
        move |mut socket, mut responder| async move {
            // Well after the link counts, as a real request would be.
            tokio::time::sleep(Duration::from_millis(900)).await;
            let write = responder.encrypt(&request);
            socket
                .send(WebSocketMessage::Binary(write.payload))
                .await
                .unwrap();
            loop {
                let raw = match socket.next().await.unwrap().unwrap() {
                    WebSocketMessage::Binary(bytes) => bytes,
                    _ => continue,
                };
                for write in responder.receive(&raw, true).unwrap() {
                    let _ = socket.send(WebSocketMessage::Binary(write.payload)).await;
                }
                if let Some(reply) = responder
                    .received
                    .iter()
                    .find(|message| message.payload["type"] == crate::HOME_RESPONSE)
                {
                    return reply.payload.clone();
                }
            }
        },
    ));
    let session = crate::HubSession::new(
        move || {
            let identity = identity.clone();
            let state = state.clone();
            async move {
                let client = crate::Client::with_protocol(identity, HubProtocol::Wss)?;
                if let RuntimeTransport::Wss(wss) = &client.transport {
                    wss.set_noise_state_dir(Some(state)).await;
                }
                client.connect_with_timeout(Duration::from_secs(20)).await?;
                Ok(client)
            }
        },
        crate::HubSessionPolicy::default(),
    )
    .unwrap();
    let answering = crate::answer_home_requests(
        &session,
        |request| async move {
            assert_eq!(request.utterance, "turn off the kitchen light");
            Ok::<_, std::convert::Infallible>(crate::HomeAnswer::action_done(
                "<speak>Turned off the kitchen light.</speak>",
            ))
        },
        crate::DEFAULT_HOME_HANDLER_TIMEOUT,
    )
    .unwrap();
    timeout(Duration::from_secs(30), session.connect())
        .await
        .unwrap()
        .unwrap();
    let reply = timeout(Duration::from_secs(30), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reply["data"],
        json!({"request_id": "r-1", "speech": "Turned off the kitchen light.",
            "response_type": "action_done", "continue_conversation": false,
            "conversation_id": "conv-1"})
    );
    // The route turned round, from the context as the hub sent it.
    assert_eq!(reply["context"]["source"], "ha-peer");
    assert_eq!(reply["context"]["destination"], "thalovant-skill-home");
    assert_eq!(
        reply["context"]["session"],
        json!({"session_id": "kitchen"})
    );
    assert_eq!(reply["context"]["request_id"], "r-1");
    answering.stop();
    session.close().await.unwrap();
}

/// A loopback hub that serves one connection at a time with a shared
/// [`Responder`], and counts what it saw: the connections, and the pattern
/// each handshake chose.
struct FakeHub {
    endpoint: String,
    responder: Arc<std::sync::Mutex<Responder>>,
    upgrade_status: Arc<std::sync::atomic::AtomicU16>,
    /// Send one encrypted frame as the handshake completes, then close with
    /// no status: a hub that has spoken has accepted the client's key.
    speaks_then_closes: Arc<AtomicBool>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
    server: JoinHandle<()>,
}

impl FakeHub {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let mut hub = Responder::new();
        // As hivemind-core does: the first key a connection presents is the
        // only one it accepts.
        hub.pins_client = true;
        let responder = Arc::new(std::sync::Mutex::new(hub));
        let upgrade_status = Arc::new(std::sync::atomic::AtomicU16::new(0));
        let speaks_then_closes = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = tokio::spawn({
            let (responder, upgrade_status, speaks, attempts) = (
                responder.clone(),
                upgrade_status.clone(),
                speaks_then_closes.clone(),
                attempts.clone(),
            );
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    let status = upgrade_status.load(Ordering::SeqCst);
                    if status != 0 {
                        refuse_upgrade(stream, status).await;
                    } else {
                        // One at a time: a connect's KK and XX attempts come
                        // in that order.
                        serve_hub_socket(stream, &responder, speaks.load(Ordering::SeqCst)).await;
                    }
                }
            }
        });
        Self {
            endpoint,
            responder,
            upgrade_status,
            speaks_then_closes,
            attempts,
            server,
        }
    }

    fn patterns(&self) -> Vec<String> {
        self.responder.lock().unwrap().patterns.clone()
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn refuse_upgrade(mut stream: TcpStream, status: u16) {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(size) => request.extend_from_slice(&buffer[..size]),
        }
    }
    let reply =
        format!("HTTP/1.1 {status} Refused\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Close with no status, as hivemind-core does after a Noise abort, then
/// read what is still in flight until the client closes too: a socket
/// dropped with unread data resets, and a reset can overtake the close.
async fn close_without_status(socket: &mut WebSocketStream<TcpStream>) {
    let _ = socket.send(WebSocketMessage::Close(None)).await;
    let _ = timeout(Duration::from_secs(5), async {
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, WebSocketMessage::Close(_)) {
                break;
            }
        }
    })
    .await;
}

/// One connection: HELLO and the offer, then the handshake. A handshake
/// message the hub cannot authenticate -- or one that shows a client key
/// other than the one it pinned -- ends it with a close frame with no
/// status, before the hub sends anything. With `speaks_then_closes`, the hub
/// sends one encrypted frame as the handshake completes, then closes the same
/// way.
async fn serve_hub_socket(
    stream: TcpStream,
    responder: &std::sync::Mutex<Responder>,
    speaks_then_closes: bool,
) {
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let hello = responder.lock().unwrap().reset();
    for write in hello {
        let text = String::from_utf8(write.payload).unwrap();
        if socket.send(WebSocketMessage::Text(text)).await.is_err() {
            return;
        }
    }
    while let Some(Ok(message)) = socket.next().await {
        let (raw, binary) = match message {
            WebSocketMessage::Text(text) => (text.into_bytes(), false),
            WebSocketMessage::Binary(bytes) => (bytes, true),
            WebSocketMessage::Close(_) => break,
            _ => continue,
        };
        let (answered, spoke) = {
            let mut responder = responder.lock().unwrap();
            let before = responder.session.is_some();
            let answered = responder.receive(&raw, binary);
            let spoke = (speaks_then_closes && !before && responder.session.is_some()).then(|| {
                responder.encrypt(&HiveMessage {
                    msg_type: "bus".into(),
                    payload: json!({"type": "hub.ready", "data": {}, "context": {}})
                        .as_object()
                        .unwrap()
                        .clone(),
                    ..Default::default()
                })
            });
            (answered, spoke)
        };
        let Ok(writes) = answered else {
            close_without_status(&mut socket).await;
            break;
        };
        for write in writes {
            let frame = if write.binary {
                WebSocketMessage::Binary(write.payload)
            } else {
                WebSocketMessage::Text(String::from_utf8(write.payload).unwrap())
            };
            if socket.send(frame).await.is_err() {
                return;
            }
        }
        if let Some(frame) = spoke {
            if socket
                .send(WebSocketMessage::Binary(frame.payload))
                .await
                .is_ok()
            {
                close_without_status(&mut socket).await;
            }
            break;
        }
    }
}

/// A session whose clients connect with `identity` over `transport`, keeping
/// their Noise state in `state`, as [`crate::HubSession::for_identity`]
/// connects them.
fn kept_link(
    build: impl Fn() -> Result<RuntimeTransport> + Send + Sync + 'static,
    identity: &Identity,
    state: &std::path::Path,
) -> crate::HubSession {
    let (identity, state) = (identity.clone(), state.to_path_buf());
    let build = Arc::new(build);
    crate::HubSession::new(
        move || {
            let (identity, state, build) = (identity.clone(), state.clone(), build.clone());
            async move {
                let transport = build()?;
                match &transport {
                    RuntimeTransport::Http(http) => http.set_noise_state_dir(Some(state)).await,
                    RuntimeTransport::Wss(wss) => wss.set_noise_state_dir(Some(state)).await,
                    RuntimeTransport::Mqtt(mqtt) => mqtt.set_noise_state_dir(Some(state)).await,
                }
                let client = crate::Client {
                    identity,
                    transport,
                    conversations: Default::default(),
                    conversation_sequence: Default::default(),
                };
                // The first handshake runs argon2id at 64 MiB, which a debug
                // build on a busy runner does not finish in the default 6 s.
                crate::session::connect_for_session(client, Duration::from_secs(20)).await
            }
        },
        crate::HubSessionPolicy::default(),
    )
    .unwrap()
    .with_settle_window(REFUSAL_SETTLE)
}

/// What a connect came to, as the vectors name it.
fn outcome_name(result: &Result<()>) -> &'static str {
    match result {
        Ok(()) => "connected",
        Err(error) if error.is_hub_key_changed() => {
            assert!(!error.is_hub_refused(), "a changed key read as a refusal");
            "key_changed"
        }
        Err(error) if error.is_client_key_rejected() => {
            assert!(error.is_hub_refused(), "a rejected key is a refusal too");
            assert!(
                error.client_key_folders().is_some(),
                "a rejected key names its folder: {error}"
            );
            "client_key_rejected"
        }
        Err(error) if error.is_hub_refused() => "refused",
        Err(error) if error.is_connection_error() || error.is_timeout() => "failed",
        Err(error) => panic!("unexpected {error:?}"),
    }
}

/// One connect as a kept link makes it -- the handshake, then the settle
/// window -- read as the vectors read it.
async fn handshake_outcome(identity: &Identity, state: &std::path::Path) -> &'static str {
    let wss = identity.clone();
    let session = kept_link(
        move || Ok(RuntimeTransport::Wss(WssTransport::new(wss.clone()))),
        identity,
        state,
    );
    let result = timeout(Duration::from_secs(60), session.connect())
        .await
        .expect("a connect ends");
    let _ = session.close().await;
    outcome_name(&result)
}

/// Give the client a new static key in the same folder, keeping its pins.
fn replace_client_key(state: &std::path::Path) {
    let mut key = [0_u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut key);
    std::fs::write(
        state.join(crate::noise_store::NOISE_KEY_FILENAME),
        hex::encode(key),
    )
    .unwrap();
}

async fn handshake_case(case: &Value) -> Value {
    let hub = FakeHub::start().await;
    let state = FixtureDir::new();
    let another = FixtureDir::new();
    let mut identity = wss_identity(&hub.endpoint);
    let mut folder = state.0.clone();
    let situation = case["situation"].as_str().expect("situation");
    if matches!(
        situation,
        "pinned"
            | "password_changed_since_pinning"
            | "hub_key_changed"
            | "client_key_changed"
            | "client_key_changed_pinned_here"
    ) {
        // First contact pins both ways.
        assert_eq!(handshake_outcome(&identity, &state.0).await, "connected");
    }
    match situation {
        // Another program's folder, with its own key.
        "client_key_changed" => folder = another.0.clone(),
        "client_key_changed_pinned_here" => replace_client_key(&state.0),
        "closed_after_first_frame" => hub.speaks_then_closes.store(true, Ordering::SeqCst),
        "wrong_password" => identity.password = uuid::Uuid::new_v4().to_string(),
        "password_changed_since_pinning" => hub
            .responder
            .lock()
            .unwrap()
            .set_password(&uuid::Uuid::new_v4().to_string()),
        "hub_key_changed" => {
            let mut responder = hub.responder.lock().unwrap();
            responder.replace_key();
            responder.offer_kk = case["hub_offers_kk"].as_bool();
        }
        "upgrade_status" => hub.upgrade_status.store(
            case["status"].as_u64().expect("status") as u16,
            Ordering::SeqCst,
        ),
        _ => {}
    }
    let before = hub.patterns().len();
    let outcome = handshake_outcome(&identity, &folder).await;
    let patterns: Vec<String> = hub.patterns()[before..]
        .iter()
        .map(|pattern| pattern[..2].to_string())
        .collect();
    json!({"outcome": outcome, "patterns": patterns})
}

#[tokio::test]
async fn link_keeping_handshakes_run_their_vectors() {
    let raw = std::fs::read_to_string("tests/conformance/link-keeping-vectors.json").unwrap();
    let spec: Value = serde_json::from_str(&raw).unwrap();
    for case in spec["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["kind"] == "handshake")
    {
        let name = case["name"].as_str().unwrap();
        let produced = handshake_case(case).await;
        // Recorded before the assert: the record is what this SDK produced.
        common::record("link-keeping-vectors.json", name, &produced);
        assert_eq!(produced, case["expect"], "{name}");
    }
}

#[tokio::test]
async fn run_stops_at_once_when_the_hub_key_changed() {
    let hub = FakeHub::start().await;
    let state = FixtureDir::new();
    let identity = wss_identity(&hub.endpoint);
    assert_eq!(handshake_outcome(&identity, &state.0).await, "connected");
    hub.responder.lock().unwrap().replace_key();
    let dir = state.0.clone();
    let session = crate::HubSession::new(
        move || {
            let (identity, dir) = (identity.clone(), dir.clone());
            async move {
                let client = crate::Client::with_protocol(identity, HubProtocol::Wss)?;
                if let RuntimeTransport::Wss(wss) = &client.transport {
                    wss.set_noise_state_dir(Some(dir)).await;
                }
                client.connect_with_timeout(Duration::from_secs(30)).await?;
                Ok(client)
            }
        },
        crate::HubSessionPolicy {
            retry: Duration::from_millis(50),
            retry_ceiling: Duration::from_millis(100),
            probe: Duration::from_millis(50),
            probe_down: Duration::from_millis(50),
        },
    )
    .unwrap()
    .with_settle_window(Duration::from_millis(50));
    // KK against the old key fails, XX follows at once and meets the pin:
    // run() ends there rather than retrying for ever.
    let ended = timeout(Duration::from_secs(60), session.run())
        .await
        .expect("run ends");
    assert!(
        matches!(&ended, Err(error @ ThalovantError::Connection(_)) if error.is_hub_key_changed()),
        "{ended:?}"
    );
    let patterns = hub.patterns();
    assert_eq!(patterns[patterns.len() - 2..], ["KKpsk0", "XXpsk2"]);
    // The pinning connect, then KK and XX: nothing after.
    assert_eq!(hub.attempts.load(Ordering::SeqCst), 3);
    session.close().await.unwrap();
}

// A KK answer that does not authenticate is followed at once by one XX
// attempt on every transport, not just WSS. Over HTTP polling:
#[tokio::test]
async fn http_retries_a_failed_kk_with_xx_once() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    // First contact pins over XX.
    transport.connect().await.unwrap();
    transport.disconnect().await.unwrap();
    fixture
        .state
        .lock()
        .await
        .responder
        .corrupt
        .insert("KKpsk0".into());
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .expect("a spoiled KK answer was not followed by XX");
    transport.disconnect().await.unwrap();
    fixture
        .state
        .lock()
        .await
        .responder
        .corrupt
        .insert("XXpsk2".into());
    let error = timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .expect_err("an XX answer that does not authenticate");
    assert!(
        matches!(error, ThalovantError::Connection(_)) && error.is_hub_refused(),
        "{error:?}"
    );
    // XX once after each failed KK, and never twice.
    assert_eq!(
        fixture.state.lock().await.responder.patterns,
        ["XXpsk2", "KKpsk0", "XXpsk2", "KKpsk0", "XXpsk2"]
    );
}

// Over HTTP polling, a request answered 401 or 403 is a refusal, as an
// upgrade answered so is over WSS; during a KK exchange it is followed by XX.
#[tokio::test]
async fn http_a_kk_answered_unauthorized_is_followed_by_xx() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    transport.disconnect().await.unwrap();
    fixture.state.lock().await.refuse.insert("KKpsk0".into());
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .expect("a KK answered 401 was not followed by XX");
    transport.disconnect().await.unwrap();
    {
        let state = fixture.state.lock().await;
        assert_eq!(state.refused, ["KKpsk0"]);
        assert_eq!(state.responder.patterns, ["XXpsk2", "XXpsk2"]);
    }
    // XX answered 401 too: the connect's outcome is the refusal.
    fixture.state.lock().await.refuse.insert("XXpsk2".into());
    let error = timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .expect_err("both patterns refused");
    assert!(
        matches!(error, ThalovantError::Connection(_)) && error.is_hub_refused(),
        "{error:?}"
    );
    assert_eq!(
        fixture.state.lock().await.refused,
        ["KKpsk0", "KKpsk0", "XXpsk2"]
    );
}

// Over MQTT, against a loopback TLS broker. The broker spoils every KK answer;
// the first connect has no pin and offers XX anyway.
#[tokio::test]
async fn mqtt_retries_a_failed_kk_with_xx_once() {
    let (acceptor, _, ca) = tls_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid","mqtt":{"endpoint":format!("mqtts://{}",listener.local_addr().unwrap()),"username":"broker-user","password":"broker-password","topic_prefix":"test","tls":true,"qos":1}})).unwrap();
    let transport = MqttTransport::new(identity).unwrap();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport
        .set_tls_configuration(Some(TlsConfiguration::SimpleNative {
            ca,
            client_auth: None,
        }))
        .await;
    let topics = transport.topics().clone();
    let broker = tokio::spawn(async move {
        let mut responder = Responder::new();
        responder.corrupt.insert("KKpsk0".into());
        // The pinning connect, then the KK attempt and the XX one.
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            stream.set_nodelay(true).unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            // The spoiled KK ends with the client going away.
            let _ = serve_mqtt(&mut stream, &mut responder, &topics).await;
        }
        responder.patterns
    });
    for _ in 0..2 {
        timeout(Duration::from_secs(30), transport.connect())
            .await
            .unwrap()
            .expect("a spoiled KK answer was not followed by XX");
        transport.disconnect().await.unwrap();
    }
    assert_eq!(
        timeout(Duration::from_secs(10), broker)
            .await
            .unwrap()
            .unwrap(),
        ["XXpsk2", "KKpsk0", "XXpsk2"]
    );
}

// A reply the hub's bound withdraws while it still waits for the link's
// writer was never sent, so it leaves the link as it was: only a send that
// began writing frames and stopped half way spoils the session.
#[tokio::test]
async fn a_reply_withdrawn_before_it_was_sent_keeps_the_link() {
    let hub = FakeHub::start().await;
    let state = FixtureDir::new();
    let identity = wss_identity(&hub.endpoint);
    let transport = WssTransport::new(identity.clone());
    transport.set_noise_state_dir(Some(state.0.clone())).await;
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .unwrap();
    let client = crate::Client {
        identity,
        transport: RuntimeTransport::Wss(transport.clone()),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    let event = crate::Event {
        name: crate::home::HOME_REQUEST.into(),
        data: json!({"request_id": "r-1", "utterance": "turn off the light"})
            .as_object()
            .unwrap()
            .clone(),
        context: json!({"source": "thalovant-skill-home", "destination": "ha-peer"})
            .as_object()
            .unwrap()
            .clone(),
        raw: None,
    };
    // Another send holds the writer, so this reply can only queue.
    let writer = transport.state.writer.lock().await;
    let sent = crate::home::answer_home_request_within(
        &client,
        &event,
        |_| async {
            Ok::<_, std::convert::Infallible>(crate::HomeAnswer::new("action_done", "Done."))
        },
        Duration::from_millis(100),
        Duration::from_millis(300),
    )
    .await;
    assert!(matches!(sent, Ok(None)), "{sent:?}");
    drop(writer);
    assert!(transport.state.session_valid.load(Ordering::Acquire));
    assert!(transport.healthcheck().await.handshake_complete);
    // The link still carries the next message.
    client
        .reply(&event, "withdrawn.check", Map::new())
        .await
        .expect("the link is still up");
    timeout(Duration::from_secs(10), async {
        loop {
            let received = hub.responder.lock().unwrap().received.clone();
            if !received.is_empty() {
                break received;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map(|received| {
        let types: Vec<_> = received
            .iter()
            .map(|message| {
                message.payload["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert_eq!(types, ["withdrawn.check"], "the withdrawn reply went out");
    })
    .expect("the hub got the next message");
    transport.disconnect().await.unwrap();
}

// -- home-link-vectors.json: a reply queued behind another frame -------------

/// One `queued` case over a real link: another frame holds the WebSocket
/// writer for `busy_ms`, the reply waits behind it, and the link must then
/// carry another message as if nothing had happened.
async fn queued_case(case: &Value) -> Value {
    let hub = FakeHub::start().await;
    let state = FixtureDir::new();
    let identity = wss_identity(&hub.endpoint);
    let transport = WssTransport::new(identity.clone());
    transport.set_noise_state_dir(Some(state.0.clone())).await;
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .unwrap();
    let client = crate::Client {
        identity,
        transport: RuntimeTransport::Wss(transport.clone()),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    let millis = |key: &str| Duration::from_millis(case[key].as_u64().expect("whole ms"));
    let event = crate::Event {
        name: crate::home::HOME_REQUEST.into(),
        data: case["request"].as_object().expect("request").clone(),
        context: json!({"source": "skill", "destination": "ha"})
            .as_object()
            .unwrap()
            .clone(),
        raw: None,
    };
    // Another frame is being written ... for busy_ms.
    let (held, holding) = oneshot::channel();
    let busy = tokio::spawn({
        let state = transport.state.clone();
        let busy = millis("busy_ms");
        async move {
            let _writer = state.writer.lock().await;
            let _ = held.send(());
            sleep(busy).await;
        }
    });
    holding.await.unwrap();
    let handler = case["handler"].clone();
    let sent = crate::home::answer_home_request_within(
        &client,
        &event,
        move |_| async move {
            Ok::<_, std::convert::Infallible>(crate::HomeAnswer::new(
                handler["response_type"].as_str().unwrap_or("action_done"),
                handler["speech"].as_str().unwrap_or_default(),
            ))
        },
        crate::DEFAULT_HOME_HANDLER_TIMEOUT,
        millis("hub_timeout_ms"),
    )
    .await
    .expect("no send failed");
    busy.await.unwrap();
    // Time enough for a withdrawn reply to go out late, if it would.
    sleep(Duration::from_millis(200)).await;
    let responses: Vec<Value> = hub
        .responder
        .lock()
        .unwrap()
        .received
        .iter()
        .filter(|message| message.payload["type"] == crate::HOME_RESPONSE)
        .map(|message| message.payload["data"].clone())
        .collect();
    let expected: Vec<Value> = sent.iter().cloned().map(Value::Object).collect();
    assert_eq!(responses, expected, "never sent late, never twice");
    // The same link carries another message: the hub echoes it back.
    let mut events = transport.subscribe();
    let kept = client
        .emit("still.there", Map::new(), Map::new())
        .await
        .is_ok()
        && timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Ok(event) if event.name == "still.there" => break true,
                    Ok(_) => continue,
                    Err(_) => break false,
                }
            }
        })
        .await
        .unwrap_or(false)
        && hub.attempts.load(Ordering::SeqCst) == 1;
    let _ = transport.disconnect().await;
    let mut produced = json!({"replied": sent.is_some(), "link_kept": kept});
    if let Some(sent) = sent {
        produced["response"] = Value::Object(sent);
    }
    produced
}

#[tokio::test]
async fn home_link_queued_replies_run_their_vectors() {
    let raw = std::fs::read_to_string("tests/conformance/home-link-vectors.json").unwrap();
    let spec: Value = serde_json::from_str(&raw).unwrap();
    for case in spec["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["kind"] == "queued")
    {
        let name = case["name"].as_str().unwrap();
        let produced = queued_case(case).await;
        // Recorded before the assert: the record is what this SDK produced.
        common::record("home-link-vectors.json", name, &produced);
        assert_eq!(produced, case["expect"], "{name}");
    }
}

// A reply whose bound lands while its frames are being written is finished:
// half a message would break the Noise stream, and the link stays as it was.
#[tokio::test]
async fn a_reply_withdrawn_mid_write_is_finished_and_keeps_the_link() {
    let hub = FakeHub::start().await;
    let state = FixtureDir::new();
    let identity = wss_identity(&hub.endpoint);
    let transport = WssTransport::new(identity.clone());
    transport.set_noise_state_dir(Some(state.0.clone())).await;
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .unwrap();
    let client = crate::Client {
        identity,
        transport: RuntimeTransport::Wss(transport.clone()),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    let event = crate::Event {
        name: crate::home::HOME_REQUEST.into(),
        data: json!({"request_id": "r-mid", "utterance": "read me the news"})
            .as_object()
            .unwrap()
            .clone(),
        context: json!({"source": "thalovant-skill-home", "destination": "ha-peer"})
            .as_object()
            .unwrap()
            .clone(),
        raw: None,
    };
    let (entered, resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *transport.state.write_pause.lock().unwrap() = Some((entered.clone(), resume.clone()));
    // Long enough to go out in several Noise frames.
    let speech = "news ".repeat(40_000);
    let replying = tokio::spawn({
        let (client, event, speech) = (client.clone(), event.clone(), speech.clone());
        async move {
            crate::home::answer_home_request_within(
                &client,
                &event,
                move |_| async move {
                    Ok::<_, std::convert::Infallible>(crate::HomeAnswer::new(
                        "query_answer",
                        speech,
                    ))
                },
                Duration::from_millis(100),
                Duration::from_millis(300),
            )
            .await
        }
    });
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the first frame went out");
    // The bound lands while the rest is still to be written.
    let answered = timeout(Duration::from_secs(10), replying)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(answered, Ok(None)), "{answered:?}");
    resume.notify_one();
    let received = timeout(Duration::from_secs(10), async {
        loop {
            let received = hub.responder.lock().unwrap().received.clone();
            if let Some(reply) = received
                .iter()
                .find(|message| message.payload["type"] == crate::HOME_RESPONSE)
            {
                break reply.payload["data"]["speech"].clone();
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the reply was finished");
    assert_eq!(received, json!(speech.trim_end()));
    assert!(transport.state.session_valid.load(Ordering::Acquire));
    assert!(transport.healthcheck().await.handshake_complete);
    client
        .emit("after.the.reply", Map::new(), Map::new())
        .await
        .expect("the link is still up");
    timeout(Duration::from_secs(10), async {
        while !hub
            .responder
            .lock()
            .unwrap()
            .received
            .iter()
            .any(|message| message.payload["type"] == "after.the.reply")
        {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the hub got the next message on the same link");
    assert_eq!(hub.attempts.load(Ordering::SeqCst), 1);
    transport.disconnect().await.unwrap();
}

// A write stalled on a hub that stopped reading holds the writer; a
// disconnect must not wait behind it for the whole send timeout.
#[tokio::test]
async fn a_disconnect_stops_a_write_that_stalled_mid_message() {
    let hub = FakeHub::start().await;
    let state = FixtureDir::new();
    let identity = wss_identity(&hub.endpoint);
    let transport = WssTransport::new(identity.clone());
    transport.set_noise_state_dir(Some(state.0.clone())).await;
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .unwrap();
    let client = crate::Client {
        identity,
        transport: RuntimeTransport::Wss(transport.clone()),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    // Paused after its first frame and never let go: a hub that stopped
    // reading.
    let (entered, _resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *transport.state.write_pause.lock().unwrap() = Some((entered.clone(), _resume.clone()));
    let sending = tokio::spawn({
        let client = client.clone();
        async move {
            // Long enough to go out in several Noise frames.
            let data = json!({"blob": "stalled ".repeat(40_000)})
                .as_object()
                .unwrap()
                .clone();
            client.emit("a.large.message", data, Map::new()).await
        }
    });
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the first frame went out");
    let started = Instant::now();
    timeout(Duration::from_secs(3), client.transport.disconnect())
        .await
        .expect("the disconnect ends")
        .expect("inside its cleanup budget, not behind the stalled write");
    assert!(started.elapsed() < Duration::from_secs(2));
    let sent = timeout(Duration::from_secs(5), sending)
        .await
        .expect("the send ends with the link")
        .unwrap();
    assert!(sent.is_err(), "{sent:?}");
    // Half a message went out: the session is spoilt, as it is torn down.
    assert!(!transport.state.session_valid.load(Ordering::Acquire));
}

// -- link-carrier-vectors.json: KK then XX over HTTPS polling and MQTT ------

/// A TLS MQTT broker in front of one hub [`Responder`], serving one
/// connection at a time; `responder` is free between connections.
struct CarrierBroker {
    identity: Identity,
    ca: Vec<u8>,
    responder: Arc<Mutex<Responder>>,
    task: JoinHandle<()>,
}

impl CarrierBroker {
    async fn start() -> Self {
        let (acceptor, _, ca) = tls_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let identity = Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":"https://example.invalid","mqtt":{"endpoint":format!("mqtts://{}",listener.local_addr().unwrap()),"username":"broker-user","password":"broker-password","topic_prefix":"test","tls":true,"qos":1}})).unwrap();
        let topics = mqtt_topics_for_identity(&identity).unwrap();
        let mut hub = Responder::new();
        hub.pins_client = true;
        let responder = Arc::new(Mutex::new(hub));
        let task = tokio::spawn({
            let responder = responder.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    stream.set_nodelay(true).unwrap();
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        continue;
                    };
                    let mut responder = responder.lock().await;
                    // A hub that aborts just stops answering.
                    let _ = serve_mqtt(&mut stream, &mut responder, &topics).await;
                }
            }
        });
        Self {
            identity,
            ca,
            responder,
            task,
        }
    }

    fn transport(&self) -> impl Fn() -> Result<RuntimeTransport> + Send + Sync + 'static {
        let (identity, ca) = (self.identity.clone(), self.ca.clone());
        move || {
            let transport = MqttTransport::new(identity.clone())?;
            *transport.state.tls_config.try_lock().unwrap() =
                Some(TlsConfiguration::SimpleNative {
                    ca: ca.clone(),
                    client_auth: None,
                });
            Ok(RuntimeTransport::Mqtt(transport))
        }
    }
}

impl Drop for CarrierBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One connect over `build`'s carrier as a kept link makes it.
async fn carrier_attempt(
    build: impl Fn() -> Result<RuntimeTransport> + Send + Sync + 'static,
    identity: &Identity,
    state: &std::path::Path,
) -> &'static str {
    let session = kept_link(build, identity, state);
    let result = timeout(Duration::from_secs(90), session.connect())
        .await
        .expect("a connect ends");
    let _ = session.close().await;
    outcome_name(&result)
}

async fn carrier_case(case: &Value) -> Value {
    let situation = case["situation"].as_str().expect("situation");
    let state = FixtureDir::new();
    let another = FixtureDir::new();
    let mut folder = state.0.clone();
    let pins_first = matches!(
        situation,
        "pinned"
            | "password_changed_since_pinning"
            | "hub_key_changed"
            | "client_key_changed"
            | "kk_answer_unauthenticated"
    );
    if case["carrier"] == "https" {
        let fixture = HttpFixture::new().await;
        {
            let mut hub = fixture.state.lock().await;
            hub.carrier = true;
            hub.responder.pins_client = true;
        }
        let (endpoint, cert) = (fixture.endpoint.clone(), fixture.cert.clone());
        let build = move || Ok(RuntimeTransport::Http(http_transport(&endpoint, &cert)));
        let identity = http_identity(&fixture.endpoint);
        if pins_first {
            // First contact pins both ways.
            assert_eq!(
                carrier_attempt(build.clone(), &identity, &state.0).await,
                "connected"
            );
        }
        {
            let mut hub = fixture.state.lock().await;
            match situation {
                "wrong_password" | "password_changed_since_pinning" => hub
                    .responder
                    .set_password(&uuid::Uuid::new_v4().to_string()),
                "hub_key_changed" => {
                    hub.responder.replace_key();
                    hub.responder.offer_kk = case["hub_offers_kk"].as_bool();
                }
                "client_key_changed" => folder = another.0.clone(),
                "kk_answer_unauthenticated" => {
                    hub.responder.corrupt.insert("KKpsk0".into());
                }
                _ => {}
            }
        }
        let before = fixture.state.lock().await.responder.patterns.len();
        let outcome = carrier_attempt(build, &identity, &folder).await;
        let patterns = fixture.state.lock().await.responder.patterns[before..]
            .iter()
            .map(|pattern| pattern[..2].to_string())
            .collect::<Vec<_>>();
        return json!({"outcome": outcome, "patterns": patterns});
    }
    let broker = CarrierBroker::start().await;
    let identity = broker.identity.clone();
    if pins_first {
        assert_eq!(
            carrier_attempt(broker.transport(), &identity, &state.0).await,
            "connected"
        );
    }
    {
        let mut hub = broker.responder.lock().await;
        match situation {
            "wrong_password" | "password_changed_since_pinning" => {
                hub.set_password(&uuid::Uuid::new_v4().to_string())
            }
            "hub_key_changed" => {
                hub.replace_key();
                hub.offer_kk = case["hub_offers_kk"].as_bool();
            }
            "client_key_changed" => folder = another.0.clone(),
            "kk_answer_unauthenticated" => {
                hub.corrupt.insert("KKpsk0".into());
            }
            _ => {}
        }
    }
    let before = broker.responder.lock().await.patterns.len();
    let outcome = carrier_attempt(broker.transport(), &identity, &folder).await;
    let patterns = broker.responder.lock().await.patterns[before..]
        .iter()
        .map(|pattern| pattern[..2].to_string())
        .collect::<Vec<_>>();
    json!({"outcome": outcome, "patterns": patterns})
}

#[tokio::test]
async fn link_carriers_run_their_vectors() {
    let raw = std::fs::read_to_string("tests/conformance/link-carrier-vectors.json").unwrap();
    let spec: Value = serde_json::from_str(&raw).unwrap();
    for case in spec["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let produced = carrier_case(case).await;
        // Recorded before the assert: the record is what this SDK produced.
        common::record("link-carrier-vectors.json", name, &produced);
        assert_eq!(produced, case["expect"], "{name}");
    }
}

// A refusal after KK is a plain refusal: only XX can show the hub a key
// other than the one it pinned.
#[tokio::test]
async fn an_https_refusal_right_after_kk_is_not_a_rejected_key() {
    let fixture = HttpFixture::new().await;
    fixture.state.lock().await.carrier = true;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    transport.connect().await.unwrap();
    transport.disconnect().await.unwrap();
    // The hub completes KK, then refuses the encrypted HELLO.
    fixture.state.lock().await.refuse_after_handshake = true;
    let error = timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .expect_err("refused");
    assert!(error.is_hub_refused(), "{error:?}");
    assert!(!error.is_client_key_rejected(), "{error:?}");
    let runtime = RuntimeTransport::Http(transport.clone());
    assert!(runtime.closed_refused());
    assert!(!runtime.closed_key_rejected());
    assert_eq!(
        fixture.state.lock().await.responder.patterns,
        ["XXpsk2", "KKpsk0"]
    );
}

// -- where the key lives ------------------------------------------------------

#[test]
fn an_identity_file_keeps_its_key_beside_it() {
    let default = FixtureDir::new();
    let elsewhere = FixtureDir::new();
    let file = elsewhere.0.join("identity.json");
    assert_eq!(
        identity_key_folder(&file, Some(&default.0)),
        Some(IdentityKeyFolder {
            dir: elsewhere.0.clone(),
            keep_here: true,
            adopt_from: Some(default.0.clone()),
        })
    );
    // The default folder's own identity file: nothing changes.
    assert_eq!(
        identity_key_folder(&default.0.join("identity.json"), Some(&default.0)),
        Some(IdentityKeyFolder {
            dir: default.0.clone(),
            keep_here: false,
            adopt_from: None,
        })
    );
    // A folder that cannot be written keeps using the default.
    let missing = elsewhere.0.join("gone");
    assert_eq!(
        identity_key_folder(&missing.join("identity.json"), Some(&default.0)),
        Some(IdentityKeyFolder {
            dir: missing,
            keep_here: false,
            adopt_from: None,
        })
    );
    assert!(
        std::fs::read_dir(&elsewhere.0).unwrap().next().is_none(),
        "the probe is removed"
    );
}

#[tokio::test]
async fn a_client_read_from_a_file_uses_the_folder_it_is_in() {
    let folder = FixtureDir::new();
    let file = folder.0.join("identity.json");
    std::fs::write(
        &file,
        json!({"site_id":"test","key":"test-access","password":"p","default_master":"https://hub.example"})
            .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let client = crate::Client::from_file(&file).unwrap();
    let RuntimeTransport::Http(http) = &client.transport else {
        panic!("an https identity");
    };
    let named = http.state.noise_state_dir.lock().await.clone();
    let default = noise_state_dir().ok();
    if default
        .as_deref()
        .is_some_and(|default| same_dir(default, &folder.0))
    {
        assert_eq!(named, None);
    } else {
        assert_eq!(named.as_deref(), Some(folder.0.as_path()));
        assert_eq!(http.state.lifecycle.key_folder().adopt_from, default);
        // Naming a folder turns the copy off.
        http.set_noise_state_dir(Some(folder.0.clone())).await;
        assert_eq!(http.state.lifecycle.key_folder().adopt_from, None);
    }
}

#[test]
fn a_key_that_met_this_hub_is_copied_into_the_new_folder_never_moved() {
    let legacy = FixtureDir::new();
    let target = FixtureDir::new();
    let key = load_or_create_noise_key(Some(&legacy.0)).unwrap();
    let hub_key = "ab".repeat(32);
    pin_hub_key(Some(&legacy.0), "test-hub", &hub_key).unwrap();

    // Another hub's identity: this key never met it, so nothing is copied.
    assert!(!adopt_legacy_key(&target.0, &legacy.0, "another-hub").unwrap());
    assert!(!target
        .0
        .join(crate::noise_store::NOISE_KEY_FILENAME)
        .exists());

    assert!(adopt_legacy_key(&target.0, &legacy.0, "test-hub").unwrap());
    assert_eq!(load_or_create_noise_key(Some(&target.0)).unwrap(), key);
    assert_eq!(
        load_noise_pin(Some(&target.0), "test-hub").unwrap(),
        Some(hub_key.clone())
    );
    // Copied, not moved: the old folder is as it was.
    assert_eq!(load_or_create_noise_key(Some(&legacy.0)).unwrap(), key);
    assert_eq!(
        load_noise_pin(Some(&legacy.0), "test-hub").unwrap(),
        Some(hub_key)
    );
    // Once only: a folder that holds a key keeps it.
    assert!(!adopt_legacy_key(&target.0, &legacy.0, "test-hub").unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target.0.join(crate::noise_store::NOISE_KEY_FILENAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // A folder with no key of its own anywhere copies nothing.
    let empty = FixtureDir::new();
    let fresh = FixtureDir::new();
    assert!(!adopt_legacy_key(&fresh.0, &empty.0, "test-hub").unwrap());
}

// The first handshake from a new folder takes the key the hub already knows,
// so a device whose identity file is not in the default folder keeps its link.
#[tokio::test]
async fn the_first_handshake_from_a_new_folder_takes_the_key_the_hub_pinned() {
    let hub = FakeHub::start().await;
    let identity = wss_identity(&hub.endpoint);
    let legacy = FixtureDir::new();
    // An older client kept its key in the old default and met this hub.
    assert_eq!(handshake_outcome(&identity, &legacy.0).await, "connected");
    let beside = FixtureDir::new();
    let transport = WssTransport::new(identity.clone());
    transport.set_noise_state_dir(Some(beside.0.clone())).await;
    transport
        .state
        .lifecycle
        .key_folder
        .lock()
        .unwrap()
        .adopt_from = Some(legacy.0.clone());
    timeout(Duration::from_secs(30), transport.connect())
        .await
        .unwrap()
        .expect("the hub knows the copied key");
    transport.disconnect().await.unwrap();
    assert_eq!(
        load_or_create_noise_key(Some(&beside.0)).unwrap(),
        load_or_create_noise_key(Some(&legacy.0)).unwrap()
    );
    // KK straight away: the hub's pin came along too.
    let patterns = hub.patterns();
    assert_eq!(patterns[patterns.len() - 1], "KKpsk0");
}

// The rejected key says where the key is, and where the other program's is.
#[tokio::test]
async fn a_rejected_key_names_both_folders() {
    let hub = FakeHub::start().await;
    let identity = wss_identity(&hub.endpoint);
    let first = FixtureDir::new();
    assert_eq!(handshake_outcome(&identity, &first.0).await, "connected");
    let second = FixtureDir::new();
    let transport = WssTransport::new(identity.clone());
    transport.set_noise_state_dir(Some(second.0.clone())).await;
    transport
        .state
        .lifecycle
        .key_folder
        .lock()
        .unwrap()
        .identity_dir = Some(first.0.clone());
    let client = crate::Client {
        identity,
        transport: RuntimeTransport::Wss(transport),
        conversations: Default::default(),
        conversation_sequence: Default::default(),
    };
    let session = crate::HubSession::new(
        move || {
            let client = client.clone();
            async move { crate::session::connect_for_session(client, Duration::from_secs(20)).await }
        },
        crate::HubSessionPolicy::default(),
    )
    .unwrap();
    let error = timeout(Duration::from_secs(60), session.run())
        .await
        .expect("run gives up at once")
        .expect_err("the hub pinned the other key");
    assert!(error.is_client_key_rejected(), "{error:?}");
    let (folder, other) = error.client_key_folders().unwrap();
    assert_eq!(std::path::Path::new(&folder), second.0.as_path());
    assert_eq!(
        other.as_deref().map(std::path::Path::new),
        Some(first.0.as_path())
    );
    assert!(error
        .to_string()
        .contains("Re-pair, or share the key folder"));
    // Given up at once: the pinning connect, then this one attempt.
    assert_eq!(hub.attempts.load(Ordering::SeqCst), 2);
    session.close().await.unwrap();
}
