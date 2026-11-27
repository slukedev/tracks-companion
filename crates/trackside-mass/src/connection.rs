//! One WebSocket connection: the handshake, then a single task that owns the socket. Callers
//! reach it only through channels, so nothing holds a lock across an await.

use std::collections::HashMap;
use std::future::poll_fn;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Notify, OwnedSemaphorePermit, broadcast, mpsc, oneshot};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tokio_util::time::DelayQueue;
use tokio_util::time::delay_queue::Key;

use crate::error::{AuthFailure, ConnectError, ErrorCode, RequestError, Stage};
use crate::event::{Event, Update};
use crate::models::{ServerInfo, User};
use crate::options::Options;
use crate::protocol::{self, CommandFrame, Incoming};
use crate::schema::{self, Compatibility};
use crate::token::Token;

pub(crate) type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A connection that is authenticated and ready.
pub(crate) struct Established {
    pub ws: Ws,
    pub server: ServerInfo,
    pub user: Option<User>,
    pub compatibility: Compatibility,
}

/// The TLS setup for `wss://`, or `None` for `ws://`: the workspace's rustls with ring,
/// verified by the platform's certificate store as reqwest does, or by the custom CA alone
/// when one is given.
pub(crate) fn tls_config(options: &Options) -> Option<Result<Arc<ClientConfig>, String>> {
    options
        .endpoint
        .is_secure()
        .then(|| build_tls(options.custom_ca.as_ref()))
}

fn build_tls(custom: Option<&crate::options::CustomCa>) -> Result<Arc<ClientConfig>, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = match custom {
        Some(ca) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in ca.0.iter() {
                roots.add(cert.clone()).map_err(|e| e.to_string())?;
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        None => builder
            .with_platform_verifier()
            .map_err(|e| e.to_string())?
            .with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// Connects, reads the greeting, checks the schema and authenticates.
pub(crate) async fn establish(
    options: &Options,
    token: &Token,
    tls: Option<&Result<Arc<ClientConfig>, String>>,
) -> Result<Established, ConnectError> {
    if token.is_empty() {
        return Err(ConnectError::NoToken);
    }
    let connector = match tls {
        None => Connector::Plain,
        Some(Ok(config)) => Connector::Rustls(Arc::clone(config)),
        Some(Err(e)) => return Err(ConnectError::Tls(e.clone())),
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(options.max_message_bytes))
        .max_frame_size(Some(options.max_message_bytes));
    let url = options.endpoint.ws_url();
    let connect = tokio_tungstenite::connect_async_tls_with_config(
        url.as_str(),
        Some(config),
        true,
        Some(connector),
    );
    let (mut ws, _) = timeout(options.connect_timeout, connect)
        .await
        .map_err(|_| ConnectError::Timeout(Stage::Connect))?
        .map_err(ConnectError::from_ws)?;

    let greeting = timeout(options.handshake_timeout, next_incoming(&mut ws))
        .await
        .map_err(|_| ConnectError::Timeout(Stage::Greeting))??;
    let server = match greeting {
        Incoming::ServerInfo(info) => *info,
        Incoming::Error { error, .. } if error.code == ErrorCode::SetupRequired => {
            return Err(ConnectError::SetupRequired);
        }
        _ => {
            return Err(ConnectError::Protocol(
                "the first message was not the server info".into(),
            ));
        }
    };
    let compatibility = schema::check(&server).map_err(ConnectError::Incompatible)?;

    let auth = json!({
        "message_id": "auth",
        "command": "auth",
        "args": { "token": token.expose() },
    })
    .to_string();
    timeout(options.write_timeout, ws.send(Message::text(auth)))
        .await
        .map_err(|_| ConnectError::Timeout(Stage::Auth))?
        .map_err(ConnectError::from_ws)?;

    let user = timeout(options.handshake_timeout, async {
        loop {
            match next_incoming(&mut ws).await? {
                Incoming::Result {
                    message_id, result, ..
                } if message_id == "auth" => {
                    return auth_result(&result);
                }
                Incoming::Error { error, .. } if error.code == ErrorCode::SetupRequired => {
                    return Err(ConnectError::SetupRequired);
                }
                Incoming::Error { message_id, error } if message_id == "auth" => {
                    return Err(ConnectError::Auth(AuthFailure {
                        code: error.code,
                        details: error.details,
                    }));
                }
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| ConnectError::Timeout(Stage::Auth))??;

    Ok(Established {
        ws,
        server,
        user,
        compatibility,
    })
}

fn auth_result(result: &Value) -> Result<Option<User>, ConnectError> {
    if result.get("authenticated").and_then(Value::as_bool) != Some(true) {
        return Err(ConnectError::Auth(AuthFailure {
            code: ErrorCode::AuthenticationFailed,
            details: None,
        }));
    }
    Ok(result
        .get("user")
        .and_then(|user| serde_json::from_value(user.clone()).ok()))
}

/// The next text message, parsed. Pings are answered by tungstenite while reading.
async fn next_incoming(ws: &mut Ws) -> Result<Incoming, ConnectError> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => {
                return protocol::parse(text.as_str())
                    .map_err(|e| ConnectError::Protocol(format!("unreadable message: {e}")));
            }
            Some(Ok(Message::Close(_))) | None => return Err(ConnectError::Closed),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(ConnectError::from_ws(e)),
        }
    }
}

pub(crate) enum Command {
    Request(Request),
    Reconnect,
}

pub(crate) struct Request {
    pub command: String,
    pub args: Option<Value>,
    pub deadline: Instant,
    pub timeout: Duration,
    pub reply: oneshot::Sender<Result<Value, RequestError>>,
    pub permit: OwnedSemaphorePermit,
}

/// Why a connection ended.
#[derive(Debug)]
pub(crate) enum Ended {
    Shutdown,
    Reconnect,
    Lost(String),
    Dead,
}

pub(crate) struct Link<'a> {
    pub commands: &'a mut mpsc::Receiver<Command>,
    pub updates: &'a broadcast::Sender<Update>,
    pub resync: &'a Notify,
    pub cancel: &'a CancellationToken,
    pub options: &'a Options,
}

struct Pending {
    command: String,
    reply: oneshot::Sender<Result<Value, RequestError>>,
    timeout: Duration,
    key: Key,
    chunks: Option<Vec<Value>>,
    _permit: OwnedSemaphorePermit,
}

struct Actor<'a> {
    link: Link<'a>,
    pending: HashMap<String, Pending>,
    deadlines: DelayQueue<String>,
    next_id: u64,
}

/// Runs an established connection until it ends, answering every pending request on the way
/// out.
pub(crate) async fn run(ws: Ws, link: Link<'_>) -> Ended {
    let mut actor = Actor {
        link,
        pending: HashMap::new(),
        deadlines: DelayQueue::new(),
        next_id: 0,
    };
    let mut ws = ws;
    let ended = actor.drive(&mut ws).await;
    let shutdown = matches!(ended, Ended::Shutdown);
    for (_, pending) in actor.pending.drain() {
        let error = if shutdown {
            RequestError::ShutDown
        } else {
            RequestError::ConnectionLost
        };
        let _ = pending.reply.send(Err(error));
    }
    if matches!(ended, Ended::Shutdown | Ended::Reconnect) {
        let _ = timeout(Duration::from_secs(2), ws.close(None)).await;
    }
    ended
}

impl Actor<'_> {
    async fn drive(&mut self, ws: &mut Ws) -> Ended {
        let options = self.link.options;
        let mut ping = tokio::time::interval_at(
            Instant::now() + options.ping_interval,
            options.ping_interval,
        );
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let dead = tokio::time::sleep(options.dead_after);
        tokio::pin!(dead);

        loop {
            tokio::select! {
                biased;
                () = self.link.cancel.cancelled() => return Ended::Shutdown,
                frame = ws.next() => {
                    dead.as_mut().reset(Instant::now() + options.dead_after);
                    match frame {
                        Some(Ok(Message::Text(text))) => self.on_text(text.as_str()),
                        Some(Ok(Message::Close(frame))) => {
                            let why = frame.map_or_else(String::new, |f| f.reason.to_string());
                            return Ended::Lost(format!("closed by Music Assistant {why}").trim().to_owned());
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Ended::Lost(e.to_string()),
                        None => return Ended::Lost("closed".to_owned()),
                    }
                }
                Some(expired) = poll_fn(|cx| self.deadlines.poll_expired(cx)) => {
                    let id = expired.into_inner();
                    if let Some(pending) = self.pending.remove(&id) {
                        tracing::debug!(command = %pending.command, "Music Assistant request timed out");
                        let _ = pending.reply.send(Err(RequestError::Timeout(pending.timeout)));
                    }
                }
                command = self.link.commands.recv() => match command {
                    Some(Command::Request(request)) => {
                        if let Err(e) = self.send_request(ws, request).await {
                            return Ended::Lost(e);
                        }
                    }
                    Some(Command::Reconnect) => return Ended::Reconnect,
                    None => return Ended::Shutdown,
                },
                _ = ping.tick() => {
                    if let Err(e) = send(ws, Message::Ping(Vec::new().into()), options.write_timeout).await {
                        return Ended::Lost(e);
                    }
                }
                () = &mut dead => return Ended::Dead,
            }
        }
    }

    async fn send_request(&mut self, ws: &mut Ws, request: Request) -> Result<(), String> {
        if request.reply.is_closed() {
            return Ok(());
        }
        if Instant::now() >= request.deadline {
            let _ = request
                .reply
                .send(Err(RequestError::Timeout(request.timeout)));
            return Ok(());
        }
        self.next_id += 1;
        let id = self.next_id.to_string();
        let frame = CommandFrame {
            message_id: &id,
            command: &request.command,
            args: request.args.as_ref(),
        };
        let text = match serde_json::to_string(&frame) {
            Ok(text) => text,
            Err(e) => {
                let _ = request.reply.send(Err(RequestError::Decode {
                    command: request.command,
                    source: e,
                }));
                return Ok(());
            }
        };
        tracing::trace!(command = %request.command, id = %id, "Music Assistant request");
        if let Err(e) = send(ws, Message::text(text), self.link.options.write_timeout).await {
            let _ = request.reply.send(Err(RequestError::ConnectionLost));
            return Err(e);
        }
        let key = self.deadlines.insert_at(id.clone(), request.deadline);
        self.pending.insert(
            id,
            Pending {
                command: request.command,
                reply: request.reply,
                timeout: request.timeout,
                key,
                chunks: None,
                _permit: request.permit,
            },
        );
        Ok(())
    }

    fn on_text(&mut self, text: &str) {
        match protocol::parse(text) {
            Ok(Incoming::Result {
                message_id,
                result,
                partial,
            }) => self.on_result(&message_id, result, partial),
            Ok(Incoming::Error { message_id, error }) => match self.finish(&message_id) {
                Some(pending) => {
                    let _ = pending.reply.send(Err(RequestError::Server(error)));
                }
                None => tracing::debug!(code = %error.code, "Music Assistant error for no request"),
            },
            Ok(Incoming::Event(raw)) => {
                let event = match Event::decode(raw) {
                    Ok(event) => event,
                    Err(undecoded) => {
                        tracing::warn!(
                            event = %undecoded.raw.event,
                            error = %undecoded.error,
                            "could not decode a Music Assistant event"
                        );
                        if undecoded.state_lost {
                            self.link.resync.notify_one();
                        }
                        Event::Other(undecoded.raw)
                    }
                };
                let _ = self.link.updates.send(Update::Event(Arc::new(event)));
            }
            Ok(Incoming::ServerInfo(_) | Incoming::Unknown) => {}
            Err(e) => tracing::warn!(error = %e, "unreadable message from Music Assistant"),
        }
    }

    fn on_result(&mut self, id: &str, result: Value, partial: bool) {
        let limit = self.link.options.max_result_items;
        let Some(pending) = self.pending.get_mut(id) else {
            tracing::trace!(id, "answer for a request nobody waits for");
            return;
        };
        if !partial && pending.chunks.is_none() {
            if let Some(pending) = self.finish(id) {
                let _ = pending.reply.send(Ok(result));
            }
            return;
        }
        let chunks = pending.chunks.get_or_insert_with(Vec::new);
        match result {
            Value::Array(items) => chunks.extend(items),
            Value::Null => {}
            other => chunks.push(other),
        }
        if chunks.len() > limit {
            if let Some(pending) = self.finish(id) {
                let _ = pending.reply.send(Err(RequestError::TooLarge {
                    command: pending.command,
                    limit,
                }));
            }
            return;
        }
        if !partial && let Some(mut pending) = self.finish(id) {
            let items = pending.chunks.take().unwrap_or_default();
            let _ = pending.reply.send(Ok(Value::Array(items)));
        }
    }

    fn finish(&mut self, id: &str) -> Option<Pending> {
        let pending = self.pending.remove(id)?;
        self.deadlines.try_remove(&pending.key);
        Some(pending)
    }
}

async fn send(ws: &mut Ws, message: Message, limit: Duration) -> Result<(), String> {
    match timeout(limit, ws.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("a write to Music Assistant timed out".to_owned()),
    }
}
