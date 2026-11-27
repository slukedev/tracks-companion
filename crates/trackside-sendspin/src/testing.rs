//! An in-process fake Sendspin server: a WebSocket listener on loopback with `snow` as the
//! Noise initiator, scripted step by step by the test. It can also play Music Assistant's
//! `/sendspin` proxy auth step.

#![allow(clippy::missing_panics_doc, clippy::unwrap_used)]

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use snow::{HandshakeState, TransportState};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

use crate::b64;
use crate::frame::{self, AudioChunk};
use crate::identity::Identity;
use crate::noise::{self, SENTINEL_PSK};

/// How long a test waits for something that should happen promptly.
pub const PATIENCE: Duration = Duration::from_secs(10);
pub const TOKEN: &str = "test-token-7c1e";

#[derive(Debug)]
pub struct FakeServer {
    pub addr: SocketAddr,
    connections: mpsc::UnboundedReceiver<FakeConn>,
}

impl FakeServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, connections) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                        let _ = tx.send(FakeConn::new(ws));
                    }
                });
            }
        });
        Self { addr, connections }
    }

    pub fn url(&self) -> String {
        format!("ws://{}/sendspin", self.addr)
    }

    /// The next connection, after the WebSocket upgrade.
    pub async fn accept(&mut self) -> FakeConn {
        tokio::time::timeout(PATIENCE, self.connections.recv())
            .await
            .expect("a connection in time")
            .expect("server running")
    }

    /// Whether a connection arrives within `wait`.
    pub async fn try_accept(&mut self, wait: Duration) -> Option<FakeConn> {
        tokio::time::timeout(wait, self.connections.recv())
            .await
            .ok()
            .flatten()
    }
}

/// What the fake puts into the handshake, to test the client's checks.
#[derive(Clone, Debug)]
pub struct HandshakeScript {
    pub psk: [u8; 32],
    /// The plaintext payload of Noise message 1.
    pub message_1_payload: Vec<u8>,
    /// Flip a byte of `server/init` in the prologue the server uses (as if tampered in transit).
    pub tamper_prologue: bool,
    /// Use this static key instead of the one announced in `server/init`.
    pub wrong_static: bool,
}

impl Default for HandshakeScript {
    fn default() -> Self {
        Self {
            psk: SENTINEL_PSK,
            message_1_payload: sentinel_message_1(),
            tamper_prologue: false,
            wrong_static: false,
        }
    }
}

pub fn sentinel_message_1() -> Vec<u8> {
    json!({"psk_id": noise::psk_id_for(&SENTINEL_PSK), "psk_category": "sn"})
        .to_string()
        .into_bytes()
}

pub struct FakeConn {
    ws: WebSocketStream<TcpStream>,
    pub server: Identity,
    client_static: Option<[u8; 32]>,
    transport: Option<TransportState>,
    hash: Vec<u8>,
    started: Instant,
    /// JSON messages read past while looking for another.
    pub seen: Vec<Value>,
}

impl std::fmt::Debug for FakeConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeConn")
    }
}

/// Server clock: microseconds since the connection, plus a large offset so the client must
/// really map it.
const SERVER_EPOCH_US: i64 = 7_000_000_000;

impl FakeConn {
    fn new(ws: WebSocketStream<TcpStream>) -> Self {
        Self {
            ws,
            server: Identity::generate().expect("server key"),
            client_static: None,
            transport: None,
            hash: Vec::new(),
            started: Instant::now(),
            seen: Vec::new(),
        }
    }

    pub fn server_now_us(&self) -> i64 {
        SERVER_EPOCH_US + i64::try_from(self.started.elapsed().as_micros()).unwrap()
    }

    pub fn client_id(&self) -> String {
        b64::encode_url(&self.client_static.expect("client/init read"))
    }

    async fn next_raw(&mut self) -> Option<Message> {
        loop {
            match tokio::time::timeout(PATIENCE, self.ws.next())
                .await
                .expect("a message in time")
            {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(message)) => return Some(message),
                Some(Err(_)) | None => return None,
            }
        }
    }

    pub async fn recv_text(&mut self) -> String {
        match self.next_raw().await {
            Some(Message::Text(text)) => text.as_str().to_owned(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    pub async fn send_text(&mut self, text: &str) {
        self.ws.send(Message::text(text)).await.expect("send");
    }

    /// Reads the proxy `auth` message.
    pub async fn expect_auth(&mut self) -> Value {
        let auth: Value = serde_json::from_str(&self.recv_text().await).expect("json");
        assert_eq!(auth["type"], "auth");
        auth
    }

    pub async fn auth_ok(&mut self) {
        self.send_text(r#"{"type": "auth_ok"}"#).await;
    }

    pub async fn refuse_auth(&mut self, reason: &str) {
        let _ = self
            .ws
            .close(Some(CloseFrame {
                code: 4001.into(),
                reason: reason.to_owned().into(),
            }))
            .await;
    }

    /// Auth (when `token`), init and the Sentinel handshake.
    pub async fn handshake(&mut self) {
        self.handshake_with(&HandshakeScript::default())
            .await
            .expect("handshake");
    }

    pub async fn read_client_init(&mut self) -> String {
        let text = self.recv_text().await;
        let init: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(init["type"], "client/init");
        let id = init["payload"]["client_id"].as_str().expect("client_id");
        self.client_static = Some(b64::decode_id(id).expect("client id"));
        text
    }

    pub async fn read_client_init_value(&mut self) -> Value {
        serde_json::from_str(&self.read_client_init().await).expect("json")
    }

    /// Runs init and the Noise handshake as initiator. `Err` when the client did not answer
    /// message 1 with a message 2 that verifies.
    pub async fn handshake_with(&mut self, script: &HandshakeScript) -> Result<(), String> {
        let client_init = self.read_client_init().await;
        let server_init = json!({"type": "server/init",
                                 "payload": {"server_id": self.server.client_id(), "version": 1}})
        .to_string();
        self.send_text(&server_init).await;
        let mut prologue = client_init.into_bytes();
        prologue.extend_from_slice(server_init.as_bytes());
        if script.tamper_prologue {
            let last = prologue.len() - 3;
            prologue[last] ^= 1;
        }
        let local = if script.wrong_static {
            Identity::generate().expect("key")
        } else {
            self.server.clone()
        };
        let mut state = initiator(
            &local,
            &self.client_static.expect("init"),
            &prologue,
            &script.psk,
        );
        let mut message = vec![0u8; 1024];
        let len = state
            .write_message(&script.message_1_payload, &mut message)
            .expect("message 1");
        self.send_text(&handshake_text(&message[..len])).await;
        let Some(Message::Text(reply)) = self.next_raw().await else {
            return Err("no message 2".into());
        };
        let data = handshake_data(reply.as_str()).ok_or("not a handshake message")?;
        let mut payload = vec![0u8; data.len()];
        let len = state
            .read_message(&data, &mut payload)
            .map_err(|e| format!("message 2: {e:?}"))?;
        assert_eq!(&payload[..len], b"{}", "message 2 payload");
        self.hash = state.get_handshake_hash().to_vec();
        self.transport = Some(state.into_transport_mode().expect("transport"));
        Ok(())
    }

    /// Sends an encrypted plaintext (message id byte included).
    pub async fn send_plain(&mut self, plaintext: &[u8]) {
        let transport = self.transport.as_mut().expect("transport mode");
        let mut out = vec![0u8; plaintext.len() + 16];
        let len = transport
            .write_message(plaintext, &mut out)
            .expect("encrypt");
        out.truncate(len);
        self.ws.send(Message::binary(out)).await.expect("send");
    }

    /// Sends bytes as a binary message without encrypting them.
    pub async fn send_binary_raw(&mut self, bytes: Vec<u8>) {
        self.ws.send(Message::binary(bytes)).await.expect("send");
    }

    pub async fn send(&mut self, kind: &str, payload: Value) {
        let text = json!({"type": kind, "payload": payload}).to_string();
        let mut plain = vec![frame::JSON];
        plain.extend_from_slice(text.as_bytes());
        self.send_plain(&plain).await;
    }

    /// Sends a JSON message split into fragments of at most `piece` bytes of data.
    pub async fn send_fragmented(&mut self, kind: &str, payload: Value, piece: usize) {
        let text = json!({"type": kind, "payload": payload}).to_string();
        for plain in frame::split(frame::JSON, text.as_bytes(), piece + 3) {
            self.send_plain(&plain).await;
        }
    }

    pub async fn send_audio(&mut self, timestamp: i64, data: &[u8]) {
        let chunk = AudioChunk {
            timestamp,
            send_ahead: Some(1),
            data: bytes::Bytes::copy_from_slice(data),
        };
        self.send_plain(&chunk.encode()).await;
    }

    /// The next decrypted JSON message from the client, or `None` once it closed.
    pub async fn try_recv(&mut self) -> Option<Value> {
        loop {
            let message = self.next_raw().await?;
            let Message::Binary(data) = message else {
                return None;
            };
            let transport = self.transport.as_mut().expect("transport mode");
            let mut plain = vec![0u8; data.len()];
            let len = transport.read_message(&data, &mut plain).expect("decrypt");
            if plain[0] == frame::JSON {
                return Some(serde_json::from_slice(&plain[1..len]).expect("json"));
            }
        }
    }

    pub async fn recv(&mut self) -> Value {
        self.try_recv().await.expect("a message")
    }

    /// Reads until a message of `kind`, answering `client/time` on the way. Everything read
    /// past goes to `seen`.
    pub async fn expect(&mut self, kind: &str) -> Value {
        loop {
            let message = self.recv().await;
            if message["type"] == kind {
                return message["payload"].clone();
            }
            if message["type"] == "client/time" {
                self.answer_time(&message["payload"]).await;
            }
            self.seen.push(message);
        }
    }

    pub async fn answer_time(&mut self, request: &Value) {
        let received = self.server_now_us();
        self.send(
            "server/time",
            json!({
                "client_transmitted": request["client_transmitted"],
                "server_received": received,
                "server_transmitted": self.server_now_us(),
            }),
        )
        .await;
    }

    /// Handshake, hello, an activation with the player role, one time burst and the client's
    /// `client/state`. Returns `client/hello`'s payload.
    pub async fn ready(&mut self) -> Value {
        self.handshake().await;
        self.send("server/hello", json!({"name": "Music Assistant"}))
            .await;
        let hello = self.expect("client/hello").await;
        self.activate_player().await;
        self.expect("client/state").await;
        hello
    }

    pub async fn activate_player(&mut self) {
        self.send(
            "server/activate",
            json!({"activities": ["playback"], "active_roles": ["player@v1"]}),
        )
        .await;
    }

    /// Sends `stream/start` for FLAC with `header` as the codec header.
    pub async fn start_stream(&mut self, header: &[u8]) {
        self.send(
            "stream/start",
            json!({"server_transmitted": self.server_now_us(),
                   "player": {"codec": "flac", "sample_rate": 44100, "channels": 2, "bit_depth": 16,
                              "codec_header": b64::encode_standard(header)}}),
        )
        .await;
    }

    /// Starts an in-band re-handshake with the Sentinel, then reads message 2, which arrives
    /// under the old keys, and switches to the new keys.
    pub async fn rehandshake(&mut self) {
        let hash = self.hash.clone();
        let mut state = initiator(
            &self.server.clone(),
            &self.client_static.expect("init"),
            &hash,
            &SENTINEL_PSK,
        );
        let mut message = vec![0u8; 1024];
        let len = state
            .write_message(&sentinel_message_1(), &mut message)
            .expect("message 1");
        let data = b64::encode_url(&message[..len]);
        self.send("noise/handshake", json!({"data": data})).await;
        let reply = self.expect("noise/handshake").await;
        let data = b64::decode_url(reply["data"].as_str().expect("data")).expect("b64");
        let mut payload = vec![0u8; data.len()];
        let len = state.read_message(&data, &mut payload).expect("message 2");
        assert_eq!(&payload[..len], b"{}");
        self.hash = state.get_handshake_hash().to_vec();
        self.transport = Some(state.into_transport_mode().expect("transport"));
    }

    /// Waits until the client closes or drops the connection.
    pub async fn closed(&mut self) {
        loop {
            match tokio::time::timeout(PATIENCE, self.ws.next())
                .await
                .expect("closed in time")
            {
                None | Some(Err(_) | Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    /// Drops the TCP connection without a close handshake.
    pub fn drop_connection(self) {}
}

fn initiator(
    local: &Identity,
    remote: &[u8; 32],
    prologue: &[u8],
    psk: &[u8; 32],
) -> HandshakeState {
    snow::Builder::new(noise::PATTERN.parse().unwrap())
        .local_private_key(local.private())
        .unwrap()
        .remote_public_key(remote)
        .unwrap()
        .prologue(prologue)
        .unwrap()
        .psk(2, psk)
        .unwrap()
        .build_initiator()
        .unwrap()
}

fn handshake_text(message: &[u8]) -> String {
    json!({"type": "noise/handshake", "payload": {"data": b64::encode_url(message)}}).to_string()
}

fn handshake_data(text: &str) -> Option<Vec<u8>> {
    let value: Value = serde_json::from_str(text).ok()?;
    b64::decode_url(value["payload"]["data"].as_str()?)
}

/// A handler that collects events into a channel, for tests.
#[derive(Debug)]
pub struct Collect(pub mpsc::UnboundedSender<crate::Event>);

impl crate::Handler for Collect {
    fn handle(&self, event: crate::Event) {
        let _ = self.0.send(event);
    }
}

/// A [`Collect`] handler and its receiver.
pub fn collect() -> (
    std::sync::Arc<Collect>,
    mpsc::UnboundedReceiver<crate::Event>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (std::sync::Arc::new(Collect(tx)), rx)
}

/// The next event matching `pred`, skipping others.
pub async fn next_event(
    events: &mut mpsc::UnboundedReceiver<crate::Event>,
    pred: impl Fn(&crate::Event) -> bool,
) -> crate::Event {
    tokio::time::timeout(PATIENCE, async {
        loop {
            let event = events.recv().await.expect("client alive");
            if pred(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event in time")
}
