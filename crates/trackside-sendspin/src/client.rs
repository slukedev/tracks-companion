//! The client handle and its supervisor, which keeps one connection up, reconnecting with
//! backoff, and the connection task that owns the socket. Nothing holds a lock across an await,
//! and events go to the [`Handler`] synchronously, so the socket loop never waits on anyone.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use rustls::pki_types::CertificateDer;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::b64;
use crate::clock::{Clock, Exchange};
use crate::config::{Config, Token};
use crate::event::{Error, Event, GoodbyeReason, Handler, Stage};
use crate::frame::{self, AudioChunk, Reassembler};
use crate::noise::{self, NoiseError, PskMatch, Responder, Transport};
use crate::wire::{self, Envelope};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Music Assistant's proxy closes with this code when it refuses the `auth` message.
const AUTH_CLOSE: u16 = 4001;
/// Before transport mode only small text messages travel; afterwards one Noise message each.
const MAX_WS_MESSAGE: usize = 128 << 10;

enum Command {
    Goodbye(GoodbyeReason, oneshot::Sender<()>),
}

/// A running Sendspin player connection. Dropping it leaves the task running until its
/// cancellation token fires; call [`Client::close`] to say goodbye and wait.
#[derive(Debug)]
pub struct Client {
    commands: mpsc::Sender<Command>,
    clock: Clock,
    id: String,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Client {
    /// Starts connecting at once. `cancel` stops the client with goodbye `shutdown`.
    pub fn start(config: Config, handler: Arc<dyn Handler>, cancel: CancellationToken) -> Self {
        Self::start_with_clock(config, handler, Clock::new(), cancel)
    }

    /// [`Client::start`] with a clock the caller already shares (for example with the handler).
    pub fn start_with_clock(
        config: Config,
        handler: Arc<dyn Handler>,
        clock: Clock,
        cancel: CancellationToken,
    ) -> Self {
        let client_id = config.identity.client_id();
        let (commands, receiver) = mpsc::channel(4);
        let task = tokio::spawn(supervise(config, handler, clock.clone(), receiver, cancel));
        Self {
            commands,
            clock,
            id: client_id,
            task: Mutex::new(Some(task)),
        }
    }

    /// The local clock and its mapping from server timestamps. Shared, and reset on reconnect.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// The Sendspin `client_id`, which Music Assistant uses as the player id.
    pub fn client_id(&self) -> &str {
        &self.id
    }

    /// Says goodbye with `reason`, closes, and waits for the task to end.
    pub async fn close(&self, reason: GoodbyeReason) {
        let (done, wait) = oneshot::channel();
        if self
            .commands
            .send(Command::Goodbye(reason, done))
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// The TLS setup for `wss://`: the workspace's rustls with ring, verified by the platform's
/// certificate store, or by the custom CA alone when one is given.
fn tls_config(custom: Option<&[CertificateDer<'static>]>) -> Result<Arc<ClientConfig>, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    let config = match custom {
        Some(certificates) => {
            let mut roots = rustls::RootCertStore::empty();
            for certificate in certificates {
                roots.add(certificate.clone()).map_err(|e| e.to_string())?;
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

/// How a connection ended.
enum Ended {
    Goodbye,
    Lost(Error),
}

async fn supervise(
    config: Config,
    handler: Arc<dyn Handler>,
    clock: Clock,
    mut commands: mpsc::Receiver<Command>,
    cancel: CancellationToken,
) {
    let tls = config
        .url
        .starts_with("wss:")
        .then(|| tls_config(config.custom_ca.as_deref()));
    let mut rng = fastrand::Rng::new();
    let mut attempt = 0u32;
    let mut generation = 0u64;
    let mut last_up = Instant::now();
    let mut dropped_at: Option<Instant> = None;
    let stop = |error: Option<Error>| handler.handle(Event::Stopped { error });

    loop {
        let connected = tokio::select! {
            biased;
            () = cancel.cancelled() => return stop(None),
            command = commands.recv() => {
                if let Some(Command::Goodbye(_, done)) = command {
                    let _ = done.send(());
                }
                return stop(None);
            }
            result = establish(&config, tls.as_ref(), &clock) => result,
        };
        let error = match connected {
            Ok(established) => {
                generation += 1;
                let warm = dropped_at.is_some_and(|at| at.elapsed() < config.retention);
                handler.handle(Event::Connected {
                    server_id: established.server_id.clone(),
                    server_name: established.server_name.clone(),
                    generation,
                    warm,
                });
                let started = Instant::now();
                let ended = Connection::new(&config, established, &clock, handler.as_ref())
                    .run(&mut commands, &cancel)
                    .await;
                last_up = Instant::now();
                dropped_at = Some(last_up);
                if started.elapsed() >= config.stable_after {
                    attempt = 0;
                }
                match ended {
                    Ended::Goodbye => return stop(None),
                    Ended::Lost(error) => error,
                }
            }
            Err(error) => error,
        };
        if !error.is_retryable() {
            tracing::warn!(%error, "sendspin connection refused, not retrying");
            return stop(Some(error));
        }
        if last_up.elapsed() >= config.give_up_after {
            tracing::warn!(%error, "sendspin server unreachable, giving up");
            return stop(Some(error));
        }
        let retry_in = config.backoff.delay(attempt, &mut rng);
        attempt = attempt.saturating_add(1);
        tracing::debug!(%error, ?retry_in, "sendspin connection lost");
        handler.handle(Event::Disconnected { error, retry_in });
        tokio::select! {
            biased;
            () = cancel.cancelled() => return stop(None),
            command = commands.recv() => {
                if let Some(Command::Goodbye(_, done)) = command {
                    let _ = done.send(());
                }
                return stop(None);
            }
            () = tokio::time::sleep(retry_in) => {}
        }
    }
}

struct Established {
    ws: Ws,
    transport: Transport,
    hash: [u8; 32],
    server_static: [u8; 32],
    server_id: String,
    server_name: String,
}

/// Connects, authenticates with the proxy, runs the init exchange, the Noise handshake and the
/// hello exchange.
async fn establish(
    config: &Config,
    tls: Option<&Result<Arc<ClientConfig>, String>>,
    clock: &Clock,
) -> Result<Established, Error> {
    let connector = match tls {
        None => Connector::Plain,
        Some(Ok(config)) => Connector::Rustls(Arc::clone(config)),
        Some(Err(e)) => return Err(Error::Tls(e.clone())),
    };
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE))
        .max_frame_size(Some(MAX_WS_MESSAGE));
    let connect = tokio_tungstenite::connect_async_tls_with_config(
        config.url.as_str(),
        Some(ws_config),
        true,
        Some(connector),
    );
    let (mut ws, _) = timeout(config.connect_timeout, connect)
        .await
        .map_err(|_| Error::Timeout(Stage::Connect))?
        .map_err(|e| Error::Connect(e.to_string()))?;
    let client_id = config.identity.client_id();

    if let Some(token) = &config.token {
        proxy_auth(&mut ws, config, token, &client_id).await?;
    }

    let client_init = wire::client_init(&client_id, noise::SUITE);
    send(
        &mut ws,
        Message::text(client_init.clone()),
        config.write_timeout,
    )
    .await?;
    let server_init = timeout(config.handshake_timeout, next_text(&mut ws, Stage::Init))
        .await
        .map_err(|_| Error::Timeout(Stage::Init))??;
    let envelope = wire::parse_envelope(server_init.as_bytes())
        .ok_or_else(|| Error::Protocol("server/init is not a message".into()))?;
    let server_static = match envelope.kind.as_str() {
        "server/init" => {
            let init: wire::ServerInit = wire::payload(envelope.payload)
                .ok_or_else(|| Error::Protocol("malformed server/init".into()))?;
            if init.version.as_u64() != Some(1) {
                return Err(Error::Rejected(format!(
                    "protocol version {}",
                    init.version
                )));
            }
            b64::decode_id(&init.server_id)
                .ok_or_else(|| Error::Protocol("malformed server_id".into()))?
        }
        "server/error" => {
            let error: Option<wire::ServerError> = wire::payload(envelope.payload);
            let reason = error.and_then(|e| e.reason).unwrap_or_default();
            return Err(Error::Rejected(wire::clip(&reason, 64)));
        }
        _ => return Err(Error::Protocol("expected server/init".into())),
    };
    let server_id = b64::encode_url(&server_static);

    let mut prologue = client_init.into_bytes();
    prologue.extend_from_slice(server_init.as_bytes());
    let mut responder = Responder::new(&config.identity, &server_static, &prologue)?;
    let message_1 = timeout(
        config.handshake_timeout,
        next_text(&mut ws, Stage::Handshake),
    )
    .await
    .map_err(|_| Error::Timeout(Stage::Handshake))??;
    let message_1 = handshake_bytes(message_1.as_bytes())?;
    if responder.read_message_1(&message_1)? == PskMatch::Miss {
        tracing::info!(
            "the sendspin server referenced an unknown PSK; answering with the Sentinel"
        );
    }
    let (message_2, established) = responder.write_message_2()?;
    send(
        &mut ws,
        Message::text(wire::noise_handshake(&message_2)),
        config.write_timeout,
    )
    .await?;
    let mut transport = established.transport;

    let server_name = hello_exchange(&mut ws, &mut transport, config).await?;
    clock.reset();
    Ok(Established {
        ws,
        transport,
        hash: established.hash,
        server_static,
        server_id,
        server_name: wire::clip(&server_name, 64),
    })
}

/// Waits for `server/hello` and answers with `client/hello`. Returns the server's name.
async fn hello_exchange(
    ws: &mut Ws,
    transport: &mut Transport,
    config: &Config,
) -> Result<String, Error> {
    let mut reassembler = Reassembler::new(config.max_message_bytes);
    let name = timeout(config.handshake_timeout, async {
        loop {
            let message = next_binary(ws, transport, &mut reassembler).await?;
            if message.id != frame::JSON {
                continue;
            }
            let envelope = wire::parse_envelope(&message.body)
                .ok_or_else(|| Error::Protocol("a JSON message that is not one".into()))?;
            if envelope.kind == "server/hello" {
                let hello: wire::ServerHello = wire::payload(envelope.payload).unwrap_or_default();
                return Ok::<_, Error>(hello.name);
            }
        }
    })
    .await
    .map_err(|_| Error::Timeout(Stage::Hello))??;
    let hello = wire::client_hello(&config.hello);
    for piece in frame::split(frame::JSON, hello.as_bytes(), noise::MAX_PLAINTEXT) {
        let sealed = transport.encrypt(&piece)?;
        send(ws, Message::binary(sealed), config.write_timeout).await?;
    }
    Ok(name)
}

/// Music Assistant's `/sendspin` proxy wants an `auth` message first; `client_id` binds the
/// player to this token's user.
async fn proxy_auth(
    ws: &mut Ws,
    config: &Config,
    token: &Token,
    client_id: &str,
) -> Result<(), Error> {
    let auth = serde_json::json!({
        "type": "auth",
        "token": token.expose(),
        "client_id": client_id,
    })
    .to_string();
    send(ws, Message::text(auth), config.write_timeout).await?;
    let reply = timeout(config.auth_timeout, next_text(ws, Stage::Auth))
        .await
        .map_err(|_| Error::Timeout(Stage::Auth))??;
    match wire::parse_envelope_kind(reply.as_bytes()).as_deref() {
        Some("auth_ok") => Ok(()),
        _ => Err(Error::Protocol("the proxy did not answer auth_ok".into())),
    }
}

fn handshake_bytes(text: &[u8]) -> Result<Vec<u8>, Error> {
    let envelope = wire::parse_envelope(text)
        .filter(|e| e.kind == "noise/handshake")
        .ok_or_else(|| Error::Protocol("expected noise/handshake".into()))?;
    let handshake: wire::NoiseHandshake = wire::payload(envelope.payload)
        .ok_or_else(|| Error::Protocol("malformed noise/handshake".into()))?;
    b64::decode_url(&handshake.data).ok_or(Error::Noise(NoiseError::Payload))
}

async fn send(ws: &mut Ws, message: Message, limit: Duration) -> Result<(), Error> {
    timeout(limit, ws.send(message))
        .await
        .map_err(|_| Error::Dead)?
        .map_err(|_| Error::Closed)
}

fn close_error(frame: Option<&CloseFrame>, stage: Stage) -> Error {
    match frame {
        Some(frame) if u16::from(frame.code) == AUTH_CLOSE && stage == Stage::Auth => {
            Error::Auth(wire::clip(frame.reason.as_str(), 64))
        }
        _ => Error::Closed,
    }
}

/// The next text message during the cleartext phase.
async fn next_text(ws: &mut Ws, stage: Stage) -> Result<String, Error> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => return Ok(text.as_str().to_owned()),
            Some(Ok(Message::Binary(_))) => {
                return Err(Error::Protocol(
                    "a binary message before the handshake".into(),
                ));
            }
            Some(Ok(Message::Close(frame))) => return Err(close_error(frame.as_ref(), stage)),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Err(Error::Closed),
        }
    }
}

/// The next complete message in transport mode.
async fn next_binary(
    ws: &mut Ws,
    transport: &mut Transport,
    reassembler: &mut Reassembler,
) -> Result<frame::Message, Error> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(data))) => {
                let plaintext = transport.decrypt(&data)?;
                if let Some(message) = reassembler.push(plaintext)? {
                    return Ok(message);
                }
            }
            Some(Ok(Message::Text(_))) => {
                return Err(Error::Protocol("cleartext after the handshake".into()));
            }
            Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(Error::Closed),
            Some(Ok(_)) => {}
        }
    }
}

/// A time-sync burst in progress.
#[derive(Debug, Default)]
struct Burst {
    sent: u32,
    waiting: Option<(i64, Instant)>,
    best: Option<Exchange>,
}

#[expect(clippy::struct_excessive_bools, reason = "one flag per spec state")]
struct Connection<'a> {
    config: &'a Config,
    clock: &'a Clock,
    handler: &'a dyn Handler,
    ws: Ws,
    transport: Transport,
    hash: [u8; 32],
    server_static: [u8; 32],
    reassembler: Reassembler,
    activated: bool,
    active_roles: Vec<String>,
    playback: bool,
    player: bool,
    /// The player role became active and its `client/state` has not gone out yet.
    state_due: bool,
    streaming: bool,
    /// Between answering a re-handshake and the `server/activate` under the new keys.
    rehandshaking: bool,
    burst: Option<Burst>,
    next_burst: Option<Instant>,
    synced_reported: bool,
    last_seen: Instant,
    next_ping: Instant,
}

enum Flow {
    Continue,
    Stop(Ended),
}

fn to_flow(result: Result<(), Error>) -> Flow {
    match result {
        Ok(()) => Flow::Continue,
        Err(error) => Flow::Stop(Ended::Lost(error)),
    }
}

impl<'a> Connection<'a> {
    fn new(
        config: &'a Config,
        established: Established,
        clock: &'a Clock,
        handler: &'a dyn Handler,
    ) -> Self {
        let now = Instant::now();
        Self {
            config,
            clock,
            handler,
            ws: established.ws,
            transport: established.transport,
            hash: established.hash,
            server_static: established.server_static,
            reassembler: Reassembler::new(config.max_message_bytes),
            activated: false,
            active_roles: Vec::new(),
            playback: false,
            player: false,
            state_due: false,
            streaming: false,
            rehandshaking: false,
            burst: None,
            next_burst: None,
            synced_reported: false,
            last_seen: now,
            next_ping: now + config.ping_interval,
        }
    }

    async fn run(
        mut self,
        commands: &mut mpsc::Receiver<Command>,
        cancel: &CancellationToken,
    ) -> Ended {
        loop {
            let wake = self.next_wake();
            let flow = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.goodbye(GoodbyeReason::Shutdown).await;
                    Flow::Stop(Ended::Goodbye)
                }
                command = commands.recv() => match command {
                    Some(Command::Goodbye(reason, done)) => {
                        self.goodbye(reason).await;
                        let _ = done.send(());
                        Flow::Stop(Ended::Goodbye)
                    }
                    None => {
                        self.goodbye(GoodbyeReason::Shutdown).await;
                        Flow::Stop(Ended::Goodbye)
                    }
                },
                message = self.ws.next() => self.on_ws(message).await,
                () = sleep_until(wake) => self.on_timer().await,
            };
            match flow {
                Flow::Continue => {}
                Flow::Stop(ended) => break ended,
            }
        }
    }

    fn next_wake(&self) -> Instant {
        let mut wake = self.next_ping.min(self.last_seen + self.config.dead_after);
        if !self.rehandshaking {
            if let Some(burst) = &self.burst {
                wake = wake.min(burst.waiting.map_or_else(Instant::now, |(_, at)| at));
            } else if let Some(at) = self.next_burst {
                wake = wake.min(at);
            }
        }
        wake
    }

    async fn goodbye(&mut self, reason: GoodbyeReason) {
        let _ = self.send_json(&wire::client_goodbye(reason.wire())).await;
        let _ = timeout(self.config.write_timeout, self.ws.close(None)).await;
    }

    async fn send_json(&mut self, text: &str) -> Result<(), Error> {
        for piece in frame::split(frame::JSON, text.as_bytes(), noise::MAX_PLAINTEXT) {
            let sealed = self.transport.encrypt(&piece)?;
            send(
                &mut self.ws,
                Message::binary(sealed),
                self.config.write_timeout,
            )
            .await?;
        }
        Ok(())
    }

    async fn on_timer(&mut self) -> Flow {
        let now = Instant::now();
        if now >= self.last_seen + self.config.dead_after {
            return Flow::Stop(Ended::Lost(Error::Dead));
        }
        if now >= self.next_ping {
            self.next_ping = now + self.config.ping_interval;
            if let Err(error) = send(
                &mut self.ws,
                Message::Ping(Bytes::new()),
                self.config.write_timeout,
            )
            .await
            {
                return Flow::Stop(Ended::Lost(error));
            }
        }
        to_flow(self.advance_time_sync().await)
    }

    /// Sends the next `client/time` of a burst, starts bursts and finishes them.
    async fn advance_time_sync(&mut self) -> Result<(), Error> {
        if !self.activated || self.rehandshaking {
            return Ok(());
        }
        let now = Instant::now();
        if self.burst.is_none() && self.next_burst.is_some_and(|at| now >= at) {
            self.burst = Some(Burst::default());
            self.next_burst = Some(now + self.config.burst_interval);
        }
        let Some(burst) = self.burst.as_mut() else {
            return Ok(());
        };
        if burst.waiting.is_some_and(|(_, deadline)| now >= deadline) {
            burst.waiting = None;
        }
        if burst.waiting.is_some() {
            return Ok(());
        }
        if burst.sent < self.config.burst_size {
            let t1 = self.clock.now_us();
            burst.sent += 1;
            burst.waiting = Some((t1, now + self.config.time_reply_timeout));
            return self.send_json(&wire::client_time(t1)).await;
        }
        let best = burst.best;
        self.burst = None;
        if let Some(best) = best {
            self.clock.update(&best);
            if !self.synced_reported {
                self.synced_reported = true;
                self.handler.handle(Event::Synced);
            }
        }
        self.send_state_if_due().await
    }

    async fn send_state_if_due(&mut self) -> Result<(), Error> {
        if self.state_due && self.player && self.clock.is_synced() && !self.rehandshaking {
            self.state_due = false;
            return self
                .send_json(&wire::client_state(self.config.timing))
                .await;
        }
        Ok(())
    }

    async fn on_ws(
        &mut self,
        message: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Flow {
        self.last_seen = Instant::now();
        let data = match message {
            Some(Ok(Message::Binary(data))) => data,
            Some(Ok(Message::Text(_))) => {
                return Flow::Stop(Ended::Lost(Error::Protocol(
                    "cleartext after the handshake".into(),
                )));
            }
            Some(Ok(Message::Close(_)) | Err(_)) | None => {
                return Flow::Stop(Ended::Lost(Error::Closed));
            }
            Some(Ok(_)) => return Flow::Continue,
        };
        let result = async {
            let plaintext = self.transport.decrypt(&data)?;
            match self.reassembler.push(plaintext)? {
                Some(message) => self.on_message(message).await,
                None => Ok(()),
            }
        }
        .await;
        to_flow(result)
    }

    async fn on_message(&mut self, message: frame::Message) -> Result<(), Error> {
        match message.id {
            frame::JSON => {
                let envelope = wire::parse_envelope(&message.body)
                    .ok_or_else(|| Error::Protocol("a JSON message that is not one".into()))?;
                self.on_json(envelope).await
            }
            frame::AUDIO => {
                let chunk = AudioChunk::parse(&message.body)?;
                if self.streaming && self.player && self.playback && !self.rehandshaking {
                    self.handler.handle(Event::Audio(chunk));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_json(&mut self, envelope: Envelope) -> Result<(), Error> {
        if envelope.kind == "noise/handshake" {
            return self.rehandshake(envelope).await;
        }
        if envelope.kind == "server/activate" {
            return self.on_activate(envelope).await;
        }
        if !self.activated || self.rehandshaking {
            return Ok(());
        }
        match envelope.kind.as_str() {
            "server/time" => {
                let Some(reply) = wire::payload::<wire::ServerTime>(envelope.payload) else {
                    return Err(Error::Protocol("malformed server/time".into()));
                };
                let received = self.clock.now_us();
                if let Some(burst) = self.burst.as_mut()
                    && burst
                        .waiting
                        .is_some_and(|(t1, _)| t1 == reply.client_transmitted)
                {
                    let exchange = Exchange {
                        client_transmitted: reply.client_transmitted,
                        server_received: reply.server_received,
                        server_transmitted: reply.server_transmitted,
                        client_received: received,
                    };
                    burst.waiting = None;
                    if exchange.max_error() > 0
                        && burst
                            .best
                            .is_none_or(|best| exchange.max_error() < best.max_error())
                    {
                        burst.best = Some(exchange);
                    }
                    return self.advance_time_sync().await;
                }
                Ok(())
            }
            "stream/start" => {
                let start: wire::StreamStart = wire::payload(envelope.payload)
                    .ok_or_else(|| Error::Protocol("malformed stream/start".into()))?;
                if let Some(format) = start.player
                    && self.player
                    && self.playback
                {
                    let in_place = self.streaming;
                    self.streaming = true;
                    self.handler.handle(Event::StreamStart { format, in_place });
                }
                Ok(())
            }
            "stream/clear" => {
                let clear: wire::StreamClear = wire::payload(envelope.payload)
                    .ok_or_else(|| Error::Protocol("malformed stream/clear".into()))?;
                if self.streaming && wire::targets_player(clear.roles.as_deref()) {
                    self.handler.handle(Event::Clear {
                        from_timestamp: clear.from_timestamp,
                    });
                }
                Ok(())
            }
            "stream/end" => {
                let end: wire::StreamEnd = wire::payload(envelope.payload)
                    .ok_or_else(|| Error::Protocol("malformed stream/end".into()))?;
                if self.streaming && wire::targets_player(end.roles.as_deref()) {
                    self.streaming = false;
                    self.handler.handle(Event::End);
                }
                Ok(())
            }
            "group/update" => {
                let group: wire::GroupUpdate = wire::payload(envelope.payload)
                    .ok_or_else(|| Error::Protocol("malformed group/update".into()))?;
                if b64::decode_id(&group.group_id).is_none() {
                    return Err(Error::Protocol(
                        "a group_id that is not 43 base64url characters".into(),
                    ));
                }
                self.handler.handle(Event::Group(group));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_activate(&mut self, envelope: Envelope) -> Result<(), Error> {
        let activate: wire::Activate = wire::payload(envelope.payload)
            .ok_or_else(|| Error::Protocol("malformed server/activate".into()))?;
        let first = !self.activated;
        self.activated = true;
        self.rehandshaking = false;
        if let Some(roles) = activate.active_roles {
            self.active_roles = roles;
        } else if first {
            self.active_roles.clear();
        }
        let playback = activate.activities.iter().any(|a| a == "playback");
        let pairing = activate.activities.iter().any(|a| a == "pairing");
        let player = self.active_roles.iter().any(|r| r == "player@v1");
        if self.streaming && (!player || !playback) {
            self.streaming = false;
            self.handler.handle(Event::End);
        }
        if player && !self.player {
            self.state_due = true;
        }
        self.player = player;
        self.playback = playback;
        self.handler.handle(Event::Activated { player, playback });
        if pairing && activate.pairing.is_some() {
            // The session is keyed by the Sentinel, which no method this client offers may
            // pair under.
            self.send_json(&wire::pair_abort("method_not_supported"))
                .await?;
        }
        if first {
            self.next_burst = Some(Instant::now());
        }
        self.advance_time_sync().await?;
        self.send_state_if_due().await
    }

    /// An in-band re-handshake (connection.md, Re-handshake): the prologue is the previous
    /// handshake hash, message 2 goes out under the old keys, and the server's next message is
    /// `server/activate` under the new ones.
    async fn rehandshake(&mut self, envelope: Envelope) -> Result<(), Error> {
        let handshake: wire::NoiseHandshake = wire::payload(envelope.payload)
            .ok_or_else(|| Error::Protocol("malformed noise/handshake".into()))?;
        let message_1 =
            b64::decode_url(&handshake.data).ok_or(Error::Noise(NoiseError::Payload))?;
        let mut responder = Responder::new(&self.config.identity, &self.server_static, &self.hash)?;
        if responder.read_message_1(&message_1)? == PskMatch::Miss {
            return Err(Error::Noise(NoiseError::UnknownPsk));
        }
        let (message_2, established) = responder.write_message_2()?;
        self.send_json(&wire::noise_handshake(&message_2)).await?;
        self.transport = established.transport;
        self.hash = established.hash;
        self.reassembler.clear();
        self.rehandshaking = true;
        if let Some(burst) = self.burst.as_mut() {
            burst.waiting = None;
        }
        Ok(())
    }
}
