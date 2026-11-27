//! An in-process fake Music Assistant: a WebSocket server on loopback whose every message is
//! scripted by the test.

#![allow(dead_code, clippy::missing_panics_doc)]

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use trackside_mass::{Backoff, Client, ConnectionState, Endpoint, Options, Token};

pub const TOKEN: &str = "test-token-3f9a1c";

/// How long a test waits for something that should happen promptly.
pub const PATIENCE: Duration = Duration::from_secs(10);

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
                        let _ = tx.send(FakeConn { ws });
                    }
                });
            }
        });
        Self { addr, connections }
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint::parse(&format!("http://{}", self.addr)).expect("endpoint")
    }

    /// Options with short delays, so real-time tests stay quick.
    pub fn options(&self) -> Options {
        let mut options = Options::new(self.endpoint(), Token::new(TOKEN));
        options.backoff = Backoff {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(100),
        };
        options.request_timeout = Duration::from_secs(5);
        options.auth_retry = Duration::from_millis(200);
        options.incompatible_retry = Duration::from_millis(200);
        options
    }

    /// [`Self::options`] with retries far away, so a test can look at why an attempt failed.
    pub fn options_without_retry(&self) -> Options {
        let mut options = self.options();
        options.backoff = Backoff {
            initial: Duration::from_secs(3600),
            max: Duration::from_secs(3600),
        };
        options
    }

    /// The next connection, after the WebSocket upgrade and before the greeting.
    pub async fn accept(&mut self) -> FakeConn {
        tokio::time::timeout(PATIENCE, self.connections.recv())
            .await
            .expect("a connection in time")
            .expect("server running")
    }

    /// A connection that completes the handshake and answers the first snapshot with no
    /// players and no queues.
    pub async fn accept_ready(&mut self) -> FakeConn {
        let mut conn = self.accept().await;
        conn.handshake().await;
        conn.serve_snapshot(json!([]), json!([])).await;
        conn
    }
}

/// A command the client sent.
#[derive(Debug, Clone)]
pub struct Sent {
    pub id: String,
    pub command: String,
    pub args: Value,
}

pub struct FakeConn {
    ws: WebSocketStream<TcpStream>,
}

pub fn server_info(schema: u32, min_supported: u32) -> Value {
    json!({
        "server_id": "6a1c2f0e9b8d4c7a",
        "server_version": "2.10.13",
        "schema_version": schema,
        "min_supported_schema_version": min_supported,
        "base_url": "http://192.168.1.20:8095",
        "homeassistant_addon": false,
        "onboard_done": true,
        "name": "Living room MA",
        "status": "running",
        "internal_url": "http://192.168.1.20:8095",
        "external_url": null,
        "has_remote_access": false
    })
}

impl FakeConn {
    pub async fn send(&mut self, value: Value) {
        self.send_text(value.to_string()).await;
    }

    pub async fn send_text(&mut self, text: String) {
        self.ws.send(Message::text(text)).await.expect("send");
    }

    pub async fn greet(&mut self, schema: u32, min_supported: u32) {
        self.send(server_info(schema, min_supported)).await;
    }

    /// The next text message, skipping pings and pongs (tungstenite answers pings itself).
    pub async fn recv_json(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(PATIENCE, self.ws.next())
                .await
                .expect("a message in time")
                .expect("connection open")
                .expect("valid frame");
            if let Message::Text(text) = message {
                return serde_json::from_str(text.as_str()).expect("json");
            }
        }
    }

    pub async fn recv(&mut self) -> Sent {
        let value = self.recv_json().await;
        Sent {
            id: value["message_id"].as_str().expect("message id").to_owned(),
            command: value["command"].as_str().expect("command").to_owned(),
            args: value.get("args").cloned().unwrap_or(Value::Null),
        }
    }

    /// Reads the `auth` command and returns it.
    pub async fn expect_auth(&mut self) -> Sent {
        let sent = self.recv().await;
        assert_eq!(sent.command, "auth");
        sent
    }

    pub async fn accept_auth(&mut self, id: &str) {
        self.reply(
            id,
            json!({
                "authenticated": true,
                "user": {"user_id": "u1", "username": "trackside", "display_name": "Trackside", "role": "user"}
            }),
        )
        .await;
    }

    pub async fn handshake(&mut self) {
        self.greet(65, 28).await;
        let auth = self.expect_auth().await;
        assert_eq!(auth.args["token"], TOKEN);
        self.accept_auth(&auth.id).await;
    }

    pub async fn reply(&mut self, id: &str, result: Value) {
        self.send(json!({"message_id": id, "result": result})).await;
    }

    pub async fn reply_partial(&mut self, id: &str, result: Value) {
        self.send(json!({"message_id": id, "result": result, "partial": true}))
            .await;
    }

    pub async fn reply_error(&mut self, id: &str, code: i64, details: &str) {
        self.send(json!({"message_id": id, "error_code": code, "details": details}))
            .await;
    }

    pub async fn event(&mut self, event: &str, object_id: Option<&str>, data: Value) {
        self.send(json!({"event": event, "object_id": object_id, "data": data}))
            .await;
    }

    /// Answers the two snapshot commands, in whichever order they come.
    pub async fn serve_snapshot(&mut self, players: Value, queues: Value) {
        for _ in 0..2 {
            let sent = self.recv().await;
            match sent.command.as_str() {
                "players/all" => self.reply(&sent.id, players.clone()).await,
                "player_queues/all" => self.reply(&sent.id, queues.clone()).await,
                other => panic!("expected a snapshot command, got {other}"),
            }
        }
    }

    /// Reads and discards everything for `duration`, which also answers the client's pings.
    pub async fn pump(&mut self, duration: Duration) {
        let _ = tokio::time::timeout(duration, async {
            while let Some(Ok(_)) = self.ws.next().await {}
        })
        .await;
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
}

/// Waits for a state matching `pred` and returns it.
pub async fn wait_state(
    client: &Client,
    pred: impl Fn(&ConnectionState) -> bool,
) -> ConnectionState {
    let mut state = client.state();
    tokio::time::timeout(PATIENCE, state.wait_for(|s| pred(s)))
        .await
        .expect("state in time")
        .expect("client alive")
        .clone()
}

pub fn player(id: &str, name: &str, volume: u32) -> Value {
    json!({
        "player_id": id,
        "provider": "airplay--a1b2",
        "type": "player",
        "name": name,
        "display_name": name,
        "available": true,
        "device_info": {"model": "HomePod mini", "manufacturer": "Apple", "software_version": "18.2", "identifiers": {}},
        "supported_features": ["power", "volume_set", "volume_mute", "pause", "set_members", "seek"],
        "playback_state": "idle",
        "state": "idle",
        "elapsed_time": null,
        "elapsed_time_last_updated": null,
        "powered": true,
        "volume_level": volume,
        "volume_muted": false,
        "group_members": [],
        "can_group_with": ["airplay--a1b2"],
        "synced_to": null,
        "current_media": null,
        "enabled": true,
        "icon": "speaker"
    })
}

pub fn queue(id: &str, items: u32) -> Value {
    json!({
        "queue_id": id,
        "active": true,
        "display_name": "Kitchen",
        "available": true,
        "items": items,
        "shuffle_enabled": false,
        "repeat_mode": "off",
        "current_index": 0,
        "elapsed_time": 0,
        "elapsed_time_last_updated": 1_764_280_000.5,
        "state": "idle",
        "current_item": null,
        "next_item": null,
        "dont_stop_the_music_enabled": false,
        "radio_source": []
    })
}
