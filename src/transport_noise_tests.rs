use super::*;
use std::collections::{HashSet, VecDeque};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::{rustls::pki_types::PrivatePkcs8KeyDer, TlsAcceptor};

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
}
impl Responder {
    fn new() -> Self {
        let key = snow::Builder::new("Noise_XXpsk2_25519_ChaChaPoly_SHA256".parse().unwrap())
            .generate_keypair()
            .unwrap();
        Self {
            key,
            peer: None,
            psk: derive_psk("test-password", "test-hub").unwrap(),
            hello: Map::new(),
            offer: Map::new(),
            handshake: None,
            session: None,
            buffer: vec![],
            patterns: vec![],
            hellos: 0,
        }
    }
    fn reset(&mut self) -> Vec<NoiseWrite> {
        self.handshake = None;
        self.session = None;
        self.buffer.clear();
        self.hello = json!({"node_id":"test-hub","peer":"test-peer","pubkey":"test-public"})
            .as_object()
            .unwrap()
            .clone();
        let patterns = if self.peer.is_some() {
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
            if message.msg_type != "bus" {
                return Err(ThalovantError::Connection("unexpected message".into()));
            }
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
            replies.push(Self::plain(
                "shake",
                json!({"noise":{"msg":hex::encode(out)}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ));
            self.patterns.push(pattern.into());
            self.handshake = Some(handshake);
        }
        if self.handshake.as_ref().unwrap().is_handshake_finished() {
            let handshake = self.handshake.take().unwrap();
            self.peer = handshake.get_remote_static().map(Vec::from);
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
    tamper: bool,
    plaintext: bool,
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
        if self.reject.as_deref() == Some(path) {
            return json!({"error":"synthetic error"});
        }
        if path != "/connect" && !cookie {
            return json!({"error":"missing replica cookie"});
        }
        match path {
            "/connect" => {
                if self.connected {
                    return json!({"status":"Connected"});
                }
                self.connected = true;
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
                match self.responder.receive(&raw, binary) {
                    Ok(writes) => {
                        self.enqueue(writes);
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
            tamper: false,
            plaintext: false,
        }));
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let state = server_state.clone();
                tokio::spawn(async move {
                    let mut stream = acceptor.accept(stream).await.unwrap();
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
                    let body = state
                        .lock()
                        .await
                        .request(path, &bytes[header_end..], cookie);
                    let encoded = serde_json::to_vec(&body).unwrap();
                    let cookie_header = if path == "/connect" {
                        "Set-Cookie: hivemind_http_replica=test-replica; Path=/; Secure\r\n"
                    } else {
                        ""
                    };
                    let headers=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",encoded.len(),cookie_header);
                    stream.write_all(headers.as_bytes()).await.unwrap();
                    stream.write_all(&encoded).await.unwrap();
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
        let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":"test-password","default_master":self.endpoint,"data_plane_endpoints":{"https":self.endpoint}})).unwrap();
        HttpTransport::with_options_and_http_client_builder(
            identity,
            DEFAULT_USER_AGENT,
            Duration::from_secs(3600),
            reqwest::Client::builder()
                .add_root_certificate(reqwest::Certificate::from_der(&self.cert).unwrap())
                .timeout(Duration::from_secs(3)),
        )
        .unwrap()
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
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
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":"test-password","default_master":"https://example.invalid"})).unwrap();
    let mut channel = NoiseChannel::new(identity, Some(dir.0.clone()));
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
    stream.write_all(&packet).await
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
        let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":if wrong_password{"wrong"}else{"test-password"},"default_master":"https://example.invalid","mqtt":{"endpoint":format!("mqtts://{}",listener.local_addr().unwrap()),"username":"broker-user","password":"broker-password","topic_prefix":"test","tls":true,"qos":1}})).unwrap();
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
            let event = timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
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
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":"test-password","default_master":endpoint,"data_plane_endpoints":{"wss":endpoint}})).unwrap();
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
        assert!(transport.state.noise_handshake.lock().await.is_none());
        assert!(transport.state.server_hello.lock().await.is_none());
        assert!(transport.state.node_id.lock().await.is_empty());
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
        let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":"test-password","default_master":endpoint,"data_plane_endpoints":{"https":endpoint}})).unwrap();
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
    let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":"test-password","default_master":"https://example.invalid"})).unwrap();
    let transport = WssTransport::new(identity);
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let peer = Responder::new().key;
    let pin = hex::encode(peer.public);
    pin_hub_key(Some(&dir.0), "test-hub", &pin).unwrap();
    let key = load_or_create_noise_key(Some(&dir.0)).unwrap();
    let psk = derive_psk("test-password", "test-hub").unwrap();
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
    *transport.state.noise_handshake.lock().await = Some(handshake);
    *transport.state.node_id.lock().await = "test-hub".into();
    assert!(transport
        .continue_noise_handshake(json!({"msg":"00"}).as_object().unwrap())
        .await
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
