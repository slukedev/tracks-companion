//! A fake Music Assistant for other crates' tests (feature `testing`): a WebSocket server on
//! loopback that answers the commands Trackside sends from a small in-memory model, records
//! every command, and pushes events when the model changes or a test asks.
//!
//! It covers what the integration needs, not all of Music Assistant: players and their
//! queues, player and queue commands with their visible effect, providers, provider configs
//! and the `OpenSubsonic` setup flow, and a token with a role.

#![allow(
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::manual_assert,
    missing_docs,
    reason = "a test double"
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

/// A command the fake received after `auth`.
#[derive(Clone, Debug, PartialEq)]
pub struct Received {
    pub command: String,
    pub args: Value,
}

/// What the fake knows. Tests change it through [`FakeAssistant::with`].
#[derive(Debug)]
pub struct Model {
    pub server_id: String,
    pub version: String,
    pub schema: u32,
    pub min_schema: u32,
    pub token: String,
    /// `admin` lets the token add providers; anything else is refused with error 22.
    pub role: String,
    pub username: String,
    pub players: Vec<Value>,
    pub queues: Vec<Value>,
    pub queue_items: HashMap<String, Vec<Value>>,
    pub providers: Vec<Value>,
    pub configs: Vec<Value>,
    /// Commands answered with this error code and details instead.
    pub failures: HashMap<String, (i64, String)>,
    /// New connections are closed at once while false.
    pub reachable: bool,
    next_instance: u32,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            server_id: String::from("8f2e4b6a1c3d5e7f9a0b2c4d6e8f0a1b"),
            version: String::from("2.10.13"),
            schema: 65,
            min_schema: 28,
            token: String::from("fake-ma-token"),
            role: String::from("admin"),
            username: String::from("trackside"),
            players: Vec::new(),
            queues: Vec::new(),
            queue_items: HashMap::new(),
            providers: Vec::new(),
            configs: Vec::new(),
            failures: HashMap::new(),
            reachable: true,
            next_instance: 1,
        }
    }
}

struct Shared {
    model: Mutex<Model>,
    received: Mutex<Vec<Received>>,
    arrived: Notify,
    events: broadcast::Sender<String>,
    kick: broadcast::Sender<()>,
}

impl Shared {
    fn model(&self) -> MutexGuard<'_, Model> {
        self.model.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The running fake. Dropping it stops the server.
pub struct FakeAssistant {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
    stop: CancellationToken,
}

impl Drop for FakeAssistant {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl std::fmt::Debug for FakeAssistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeAssistant")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl FakeAssistant {
    pub async fn start(model: Model) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (events, _) = broadcast::channel(1024);
        let (kick, _) = broadcast::channel(4);
        let shared = Arc::new(Shared {
            model: Mutex::new(model),
            received: Mutex::new(Vec::new()),
            arrived: Notify::new(),
            events,
            kick,
        });
        let stop = CancellationToken::new();
        let accept = {
            let shared = Arc::clone(&shared);
            let stop = stop.clone();
            async move {
                loop {
                    let stream = tokio::select! {
                        () = stop.cancelled() => return,
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _)) => stream,
                            Err(_) => continue,
                        },
                    };
                    if !shared.model().reachable {
                        drop(stream);
                        continue;
                    }
                    let shared = Arc::clone(&shared);
                    let stop = stop.clone();
                    tokio::spawn(async move {
                        if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                            serve(ws, shared, stop).await;
                        }
                    });
                }
            }
        };
        tokio::spawn(accept);
        Self { addr, shared, stop }
    }

    /// `http://127.0.0.1:<port>`, as an admin would type it.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn token(&self) -> String {
        self.shared.model().token.clone()
    }

    /// Changes the model.
    pub fn with<T>(&self, change: impl FnOnce(&mut Model) -> T) -> T {
        change(&mut self.shared.model())
    }

    /// Sends an event to every authenticated connection.
    pub fn emit(&self, event: &str, object_id: Option<&str>, data: Value) {
        let text = json!({"event": event, "object_id": object_id, "data": data}).to_string();
        let _ = self.shared.events.send(text);
    }

    /// Replaces a player in the model and announces it with `player_updated`.
    pub fn update_player(&self, player_id: &str, change: impl FnOnce(&mut Value)) {
        let updated = {
            let mut model = self.shared.model();
            let Some(player) = model
                .players
                .iter_mut()
                .find(|p| p["player_id"] == player_id)
            else {
                return;
            };
            change(player);
            player.clone()
        };
        self.emit("player_updated", Some(player_id), updated);
    }

    /// Closes every open connection; with `reachable` false, new ones too.
    pub fn disconnect_all(&self) {
        let _ = self.shared.kick.send(());
    }

    pub fn received(&self) -> Vec<Received> {
        self.shared
            .received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn received_named(&self, command: &str) -> Vec<Received> {
        self.received()
            .into_iter()
            .filter(|r| r.command == command)
            .collect()
    }

    /// Waits until `command` has been received `count` times in all.
    pub async fn wait_for(&self, command: &str, count: usize) -> Vec<Received> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let arrived = self.shared.arrived.notified();
            let found = self.received_named(command);
            if found.len() >= count {
                return found;
            }
            if tokio::time::timeout_at(deadline, arrived).await.is_err() {
                panic!("{command} was received {} times, not {count}", found.len());
            }
        }
    }
}

/// A player in Music Assistant 2.10's shape.
pub fn player(id: &str, name: &str, provider: &str) -> Value {
    json!({
        "player_id": id,
        "provider": provider,
        "type": "player",
        "name": name,
        "display_name": name,
        "available": true,
        "device_info": {"model": "Era 100", "manufacturer": "Sonos"},
        "supported_features": ["power", "volume_set", "volume_mute", "pause", "set_members",
                               "seek", "next_previous", "enqueue", "play_media"],
        "playback_state": "idle",
        "state": "idle",
        "elapsed_time": null,
        "elapsed_time_last_updated": null,
        "powered": true,
        "volume_level": 30,
        "volume_muted": false,
        "group_members": [],
        "static_group_members": [],
        "can_group_with": [provider],
        "synced_to": null,
        "active_source": id,
        "current_media": null,
        "enabled": true,
        "hide_in_ui": false,
    })
}

/// An empty queue for a player.
pub fn queue(id: &str) -> Value {
    json!({
        "queue_id": id,
        "active": true,
        "display_name": id,
        "available": true,
        "items": 0,
        "shuffle_enabled": false,
        "repeat_mode": "off",
        "current_index": null,
        "elapsed_time": 0,
        "elapsed_time_last_updated": 0,
        "state": "idle",
        "current_item": null,
        "next_item": null,
    })
}

/// A provider instance.
pub fn provider(instance_id: &str, domain: &str, kind: &str, name: &str, streaming: bool) -> Value {
    json!({
        "type": kind,
        "domain": domain,
        "name": name,
        "instance_id": instance_id,
        "supported_features": ["search", "library_tracks", "library_playlists"],
        "available": true,
        "is_streaming_provider": if kind == "music" { Value::Bool(streaming) } else { Value::Null },
        "lookup_key": instance_id,
    })
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn serve(mut ws: Ws, shared: Arc<Shared>, stop: CancellationToken) {
    let greeting = {
        let model = shared.model();
        json!({
            "server_id": model.server_id,
            "server_version": model.version,
            "schema_version": model.schema,
            "min_supported_schema_version": model.min_schema,
            "base_url": "http://192.168.1.20:8095",
            "onboard_done": true,
            "name": "Music Assistant",
            "status": "running",
        })
    };
    if ws.send(Message::text(greeting.to_string())).await.is_err() {
        return;
    }
    let mut kick = shared.kick.subscribe();
    let mut events = shared.events.subscribe();
    let mut authed = false;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = kick.recv() => {
                let _ = ws.close(None).await;
                return;
            }
            event = events.recv(), if authed => match event {
                Ok(text) => {
                    if ws.send(Message::text(text)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            message = ws.next() => {
                let Some(Ok(message)) = message else { return };
                let Message::Text(text) = message else { continue };
                let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                let id = value["message_id"].as_str().unwrap_or_default().to_owned();
                let command = value["command"].as_str().unwrap_or_default().to_owned();
                let args = value.get("args").cloned().unwrap_or(Value::Null);
                let (answer, follow) = if command == "auth" {
                    let (answer, ok) = auth(&shared, &id, &args);
                    authed = ok;
                    (answer, Vec::new())
                } else if !authed {
                    (error(&id, 20, "Authentication required"), Vec::new())
                } else {
                    shared
                        .received
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(Received { command: command.clone(), args: args.clone() });
                    shared.arrived.notify_waiters();
                    answer(&shared, &id, &command, &args)
                };
                if ws.send(Message::text(answer.to_string())).await.is_err() {
                    return;
                }
                for event in follow {
                    let _ = shared.events.send(event.to_string());
                }
            }
        }
    }
}

fn error(id: &str, code: i64, details: &str) -> Value {
    json!({"message_id": id, "error_code": code, "details": details})
}

fn ok(id: &str, result: Value) -> Value {
    json!({"message_id": id, "result": result})
}

fn auth(shared: &Shared, id: &str, args: &Value) -> (Value, bool) {
    let model = shared.model();
    if args["token"].as_str() != Some(model.token.as_str()) {
        return (error(id, 23, "Invalid or expired token"), false);
    }
    let user = json!({"user_id": "u1", "username": model.username,
                      "display_name": "Trackside", "role": model.role});
    (ok(id, json!({"authenticated": true, "user": user})), true)
}

fn event(name: &str, object_id: &str, data: Value) -> Value {
    json!({"event": name, "object_id": object_id, "data": data})
}

#[expect(clippy::too_many_lines, reason = "one table of commands")]
fn answer(shared: &Shared, id: &str, command: &str, args: &Value) -> (Value, Vec<Value>) {
    let mut model = shared.model();
    if let Some((code, details)) = model.failures.get(command) {
        return (error(id, *code, details), Vec::new());
    }
    let admin = model.role == "admin";
    let player_id = args["player_id"]
        .as_str()
        .or_else(|| args["queue_id"].as_str())
        .unwrap_or_default()
        .to_owned();
    let mut follow = Vec::new();
    let mut touch_player = |model: &mut Model, change: &dyn Fn(&mut Map<String, Value>)| {
        if let Some(Value::Object(player)) = model
            .players
            .iter_mut()
            .find(|p| p["player_id"] == player_id.as_str())
        {
            change(player);
            follow.push(event(
                "player_updated",
                &player_id,
                Value::Object(player.clone()),
            ));
        }
    };
    let result = match command {
        "info" => json!({"server_id": model.server_id, "server_version": model.version,
                         "schema_version": model.schema,
                         "min_supported_schema_version": model.min_schema}),
        "players/all" => Value::Array(model.players.clone()),
        "players/get" => model
            .players
            .iter()
            .find(|p| p["player_id"] == player_id.as_str())
            .cloned()
            .unwrap_or(Value::Null),
        "player_queues/all" => Value::Array(model.queues.clone()),
        "player_queues/get" => model
            .queues
            .iter()
            .find(|q| q["queue_id"] == player_id.as_str())
            .cloned()
            .unwrap_or(Value::Null),
        "player_queues/items" => {
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().unwrap_or(500) as usize;
            Value::Array(
                model
                    .queue_items
                    .get(&player_id)
                    .map(|items| items.iter().skip(offset).take(limit).cloned().collect())
                    .unwrap_or_default(),
            )
        }
        "providers" => {
            let kind = args["provider_type"].as_str();
            Value::Array(
                model
                    .providers
                    .iter()
                    .filter(|p| kind.is_none_or(|k| p["type"] == k))
                    .cloned()
                    .collect(),
            )
        }
        "players/cmd/volume_set" => {
            let level = args["volume_level"].clone();
            touch_player(&mut model, &|p| {
                p.insert("volume_level".into(), level.clone());
            });
            Value::Null
        }
        "players/cmd/volume_mute" => {
            let muted = args["muted"].clone();
            touch_player(&mut model, &|p| {
                p.insert("volume_muted".into(), muted.clone());
            });
            Value::Null
        }
        "players/cmd/power" => {
            let powered = args["powered"].clone();
            touch_player(&mut model, &|p| {
                p.insert("powered".into(), powered.clone());
            });
            Value::Null
        }
        "players/cmd/pause" | "players/cmd/stop" => {
            let state = if command.ends_with("pause") {
                "paused"
            } else {
                "idle"
            };
            touch_player(&mut model, &|p| {
                p.insert("playback_state".into(), json!(state));
                p.insert("state".into(), json!(state));
            });
            Value::Null
        }
        "players/cmd/play" => {
            touch_player(&mut model, &|p| {
                p.insert("playback_state".into(), json!("playing"));
                p.insert("state".into(), json!("playing"));
            });
            Value::Null
        }
        "config/providers" => {
            if !admin {
                return (error(id, 22, "Insufficient permissions"), Vec::new());
            }
            let domain = args["provider_domain"].as_str();
            Value::Array(
                model
                    .configs
                    .iter()
                    .filter(|c| domain.is_none_or(|d| c["domain"] == d))
                    .cloned()
                    .collect(),
            )
        }
        "config/providers/setup" => {
            if !admin {
                return (error(id, 22, "Insufficient permissions"), Vec::new());
            }
            json!({"flow_id": "flow-1", "step_id": "user", "type": "form",
                   "entries": [{"key": "baseURL"}, {"key": "api_key"}], "errors": {}})
        }
        "config/flows/submit" => {
            let values = args["values"].clone();
            let instance_id = format!("opensubsonic--Fake{}", model.next_instance);
            model.next_instance += 1;
            model.configs.push(json!({
                "type": "music", "domain": "opensubsonic", "instance_id": instance_id,
                "enabled": true, "name": null, "default_name": "Open Subsonic",
                "status": "loaded", "values": values_of(&values),
            }));
            model.providers.push(provider(
                &instance_id,
                "opensubsonic",
                "music",
                "Open Subsonic",
                false,
            ));
            follow.push(json!({"event": "providers_updated", "object_id": null,
                               "data": model.providers.clone()}));
            json!({"flow_id": "flow-1", "step_id": "done", "type": "finish",
                   "result": {"instance_id": instance_id}})
        }
        "config/providers/save" => {
            let instance = args["instance_id"].as_str().unwrap_or_default();
            let values = args["values"].as_object().cloned().unwrap_or_default();
            let mut saved = Value::Null;
            if let Some(Value::Object(config)) = model
                .configs
                .iter_mut()
                .find(|c| c["instance_id"] == instance)
            {
                for (key, value) in values {
                    if key == "name" {
                        config.insert(key, value);
                    } else if let Some(Value::Object(entries)) = config.get_mut("values") {
                        entries.insert(key.clone(), json!({"key": key, "value": value}));
                    }
                }
                saved = Value::Object(config.clone());
            }
            if let Some(Value::Object(p)) = model
                .providers
                .iter_mut()
                .find(|p| p["instance_id"] == instance)
                && let Some(name) = args["values"]["name"].as_str()
            {
                p.insert("name".into(), json!(name));
            }
            if saved.is_null() {
                return (error(id, 15, "Invalid provider id"), Vec::new());
            }
            saved
        }
        "config/providers/remove" => {
            if !admin {
                return (error(id, 22, "Insufficient permissions"), Vec::new());
            }
            let instance = args["instance_id"].as_str().unwrap_or_default().to_owned();
            model
                .configs
                .retain(|c| c["instance_id"] != instance.as_str());
            model
                .providers
                .retain(|p| p["instance_id"] != instance.as_str());
            Value::Null
        }
        _ => Value::Null,
    };
    (ok(id, result), follow)
}

fn values_of(submitted: &Value) -> Value {
    let mut out = Map::new();
    if let Value::Object(values) = submitted {
        // Music Assistant keeps secrets in the setup data and never sends them back.
        for (key, value) in values.iter().filter(|(k, _)| k.as_str() != "api_key") {
            out.insert(key.clone(), json!({"key": key, "value": value}));
        }
    }
    Value::Object(out)
}
