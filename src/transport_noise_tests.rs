use super::*;
use std::collections::{HashSet, VecDeque};
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
    pause_send: Option<(Arc<Notify>, Arc<Notify>)>,
    pause_poll: Option<(Arc<Notify>, Arc<Notify>)>,
    pause_disconnect: Option<(Arc<Notify>, Arc<Notify>)>,
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
            pause_send: None,
            pause_poll: None,
            pause_disconnect: None,
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
                    let body = state
                        .lock()
                        .await
                        .request(path, &bytes[header_end..], cookie);
                    if pause.is_some() {
                        state.lock().await.reject = None;
                    }
                    let encoded = serde_json::to_vec(&body).unwrap();
                    let cookie_header = if path == "/connect" {
                        "Set-Cookie: hivemind_http_replica=test-replica; Path=/; Secure\r\n"
                    } else {
                        ""
                    };
                    let headers=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",encoded.len(),cookie_header);
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
        let identity=Identity::from_value(json!({"site_id":"test-site","key":"test-access","password":test_password(),"default_master":self.endpoint,"data_plane_endpoints":{"https":self.endpoint}})).unwrap();
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
async fn client_event_stream_uses_authenticated_http_and_reports_disconnect() {
    let fixture = HttpFixture::new().await;
    let transport = fixture.transport();
    let dir = FixtureDir::new();
    transport.set_noise_state_dir(Some(dir.0.clone())).await;
    let client = crate::Client {
        identity: transport.identity().clone(),
        transport: RuntimeTransport::Http(transport.clone()),
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
    let mut channel = NoiseChannel::new(transport.identity().clone(), Some(dir.0.clone()));
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
    let mut channel = NoiseChannel::new(identity.clone(), Some(dir.0.clone()));
    exchange_with_channel(&mut channel, &mut responder).unwrap();
    assert!(channel.ready());
    assert_eq!(responder.hellos, 1);
    let pin = load_noise_pin(Some(&dir.0), "test-hub").unwrap();
    assert!(pin.is_some());

    // The password rotates back to the identity's current value. Force XX so
    // the peer can return the response that authenticates and rejects our PSK.
    responder.psk = derive_psk(test_password(), "test-hub").unwrap();
    responder.peer = None;
    let mut stale = NoiseChannel::new(identity.clone(), Some(dir.0.clone()));
    assert!(exchange_with_channel(&mut stale, &mut responder).is_err());
    assert!(!stale.ready());
    assert_eq!(load_cached_psk(Some(&dir.0), "test-hub").unwrap(), None);
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);

    let mut recovered = NoiseChannel::new(identity, Some(dir.0.clone()));
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
    let mut channel = NoiseChannel::new(identity.clone(), Some(dir.0.clone()));
    channel.receive(&hello.payload, false).unwrap();
    assert!(channel.receive(&hello.payload, false).is_err());
    assert!(!channel.ready());

    let mut first = NoiseChannel::new(identity.clone(), Some(dir.0.clone()));
    exchange_with_channel(&mut first, &mut responder).unwrap();
    let pin = load_noise_pin(Some(&dir.0), "test-hub").unwrap();
    let mut replacement = Responder::new();
    let mut reconnect = NoiseChannel::new(identity, Some(dir.0.clone()));
    assert!(exchange_with_channel(&mut reconnect, &mut replacement).is_err());
    assert!(!reconnect.ready());
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wss_cancelled_chunked_send_poisons_session_and_fresh_reconnect_recovers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
                if attempt == 0 && responder.hellos == 1 {
                    server_paused.notify_one();
                    server_release.notified().await;
                    break;
                }
            }
        }
        responder.patterns
    });
    transport.connect().await.unwrap();
    timeout(Duration::from_secs(5), paused.notified())
        .await
        .unwrap();
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
    timeout(Duration::from_secs(5), async {
        loop {
            if transport.state.writer.try_lock().is_err()
                && transport.state.noise.try_lock().is_err()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!send.is_finished());
    send.abort();
    assert!(send.await.unwrap_err().is_cancelled());
    assert!(!transport.healthcheck().await.handshake_complete);
    assert!(transport.remote_static_key().await.is_none());
    assert!(transport
        .emit_bus("rejected", Map::new(), Map::new())
        .await
        .is_err());
    release.notify_one();
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
    let mut initial = NoiseChannel::new(identity.clone(), Some(dir.0.clone()));
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
    let mut stale = NoiseChannel::new(identity.clone(), Some(dir.0.clone()));
    // KK fails at the peer while reading message 1, so our receive path never
    // gets a message on which to report an authentication failure.
    assert!(exchange_with_channel(&mut stale, &mut responder).is_err());
    assert!(stale.handshake.is_some());
    assert!(stale.session.is_none());
    drop(stale); // Transport failure/timeout cleanup abandons this channel.
    assert_eq!(load_cached_psk(Some(&dir.0), "test-hub").unwrap(), None);
    assert_eq!(load_noise_pin(Some(&dir.0), "test-hub").unwrap(), pin);
    assert_eq!(load_or_create_noise_key(Some(&dir.0)).unwrap(), client_key);

    let mut recovered = NoiseChannel::new(identity, Some(dir.0.clone()));
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
