//! The client handle, the supervisor that keeps one connection up, and the resync worker.

use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{Notify, Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::connection::{self, Command, Ended, Established, Link, Request};
use crate::de;
use crate::endpoint::Endpoint;
use crate::error::{AuthFailure, ConnectError, Incompatibility, RequestError};
use crate::event::{Snapshot, SnapshotReason, Update};
use crate::models::{Player, PlayerQueue, ServerInfo, User};
use crate::options::Options;
use crate::schema::Compatibility;
use crate::token::Token;

/// An authenticated connection's details.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Session {
    pub server: ServerInfo,
    /// The Music Assistant account the token belongs to.
    pub user: Option<User>,
    pub compatibility: Compatibility,
    /// Counts connections since the client started, from 1.
    pub generation: u64,
}

/// Where the connection stands. Watch it with [`Client::state`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ConnectionState {
    /// Waiting to try again, with the reason the last attempt or connection ended.
    Disconnected {
        error: Option<ConnectError>,
        retry_in: Option<Duration>,
    },
    Connecting {
        attempt: u32,
    },
    Connected {
        server_version: String,
        schema: u32,
        session: Arc<Session>,
    },
    /// The token was refused. Tried again after `retry_in`, or at once after
    /// [`Client::set_token`].
    AuthFailed {
        error: AuthFailure,
        retry_in: Duration,
    },
    /// The server speaks a schema this client cannot use. Tried again after `retry_in`, in case
    /// the server is upgraded or downgraded.
    Incompatible {
        error: Incompatibility,
        retry_in: Duration,
    },
    /// The client shut down.
    Stopped,
}

impl ConnectionState {
    pub fn session(&self) -> Option<&Arc<Session>> {
        match self {
            Self::Connected { session, .. } => Some(session),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.session().is_some()
    }
}

/// The channel side of the connection task, shared by the client and the resync worker.
#[derive(Clone)]
pub(crate) struct Requester {
    commands: mpsc::Sender<Command>,
    permits: Arc<Semaphore>,
    timeout: Duration,
}

impl Requester {
    pub(crate) fn default_timeout(&self) -> Duration {
        self.timeout
    }

    /// Sends a command and waits for its (assembled) result. Dropping the future at any point
    /// is safe: the connection task still answers or times out the request and frees its slot.
    pub(crate) async fn value(
        &self,
        command: &str,
        args: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let deadline = Instant::now() + timeout;
        let permit = match tokio::time::timeout_at(
            deadline,
            Arc::clone(&self.permits).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(RequestError::ShutDown),
            Err(_) => return Err(RequestError::Busy),
        };
        let (reply, answer) = oneshot::channel();
        let request = Request {
            command: command.to_owned(),
            args,
            deadline,
            timeout,
            reply,
            permit,
        };
        match tokio::time::timeout_at(deadline, self.commands.send(Command::Request(request))).await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(RequestError::ShutDown),
            Err(_) => return Err(RequestError::Busy),
        }
        match tokio::time::timeout_at(deadline, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RequestError::ConnectionLost),
            Err(_) => Err(RequestError::Timeout(timeout)),
        }
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &self,
        command: &str,
        args: Option<Value>,
        timeout: Duration,
    ) -> Result<T, RequestError> {
        let value = self.value(command, args, timeout).await?;
        serde_json::from_value(value).map_err(|source| RequestError::Decode {
            command: command.to_owned(),
            source,
        })
    }

    /// A list answer, dropping (and logging) entries that do not decode.
    pub(crate) async fn list<T: DeserializeOwned>(
        &self,
        command: &str,
        args: Option<Value>,
        timeout: Duration,
    ) -> Result<Vec<T>, RequestError> {
        let items = match self.value(command, args, timeout).await? {
            Value::Array(items) => items,
            Value::Null => Vec::new(),
            _ => {
                return Err(RequestError::Decode {
                    command: command.to_owned(),
                    source: serde::de::Error::custom("expected a list"),
                });
            }
        };
        let (decoded, dropped) = de::decode_entries(items);
        if dropped > 0 {
            tracing::warn!(
                command,
                dropped,
                "dropped Music Assistant entries that did not decode"
            );
        }
        Ok(decoded)
    }
}

struct Inner {
    requester: Requester,
    endpoint: Endpoint,
    state: watch::Receiver<ConnectionState>,
    updates: broadcast::Sender<Update>,
    resync: Arc<Notify>,
    token: watch::Sender<Token>,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A Music Assistant client: one supervised connection, typed commands and an event feed.
/// Cheap to clone. Dropping the last clone stops the connection; [`Client::shutdown`] also
/// waits for it to close.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.inner.endpoint)
            .field("state", &*self.inner.state.borrow())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Starts the connection in the background. Must be called inside a tokio runtime.
    pub fn start(options: Options) -> Self {
        Self::start_with(options, CancellationToken::new())
    }

    /// [`Client::start`] tied to a parent token: cancelling it shuts the client down.
    pub fn start_with(options: Options, cancel: CancellationToken) -> Self {
        let (commands_tx, commands_rx) = mpsc::channel(options.max_in_flight.max(1));
        let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting { attempt: 0 });
        let (updates, _) = broadcast::channel(options.event_capacity.max(1));
        let (token_tx, token_rx) = watch::channel(options.token.clone());
        let resync = Arc::new(Notify::new());
        let requester = Requester {
            commands: commands_tx,
            permits: Arc::new(Semaphore::new(options.max_in_flight.max(1))),
            timeout: options.request_timeout,
        };
        let tls = options.endpoint.is_secure().then(connection::tls_config);
        let tasks = TaskTracker::new();

        let supervisor = Supervisor {
            options: options.clone(),
            tls,
            commands: commands_rx,
            state: state_tx,
            updates: updates.clone(),
            resync: Arc::clone(&resync),
            token: token_rx,
            cancel: cancel.clone(),
            permits: Arc::clone(&requester.permits),
            generation: 0,
            rng: fastrand::Rng::new(),
        };
        tasks.spawn(supervisor.run());
        tasks.spawn(resync_worker(
            requester.clone(),
            updates.clone(),
            Arc::clone(&resync),
            state_rx.clone(),
            cancel.clone(),
        ));
        tasks.close();

        Self {
            inner: Arc::new(Inner {
                requester,
                endpoint: options.endpoint,
                state: state_rx,
                updates,
                resync,
                token: token_tx,
                cancel,
                tasks,
            }),
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    /// The connection state, as a watch channel.
    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.inner.state.clone()
    }

    pub fn current_state(&self) -> ConnectionState {
        self.inner.state.borrow().clone()
    }

    /// Waits until connected and returns the session, or `None` once the client stops.
    pub async fn connected(&self) -> Option<Arc<Session>> {
        let mut state = self.state();
        let result = state
            .wait_for(|s| s.is_connected() || matches!(s, ConnectionState::Stopped))
            .await;
        result.ok().and_then(|s| s.session().cloned())
    }

    /// A feed of events and snapshots. Each subscriber gets every update; one that falls
    /// behind gets [`Update::Lagged`] and then a fresh snapshot.
    pub fn subscribe(&self) -> Subscription {
        Subscription {
            updates: self.inner.updates.subscribe(),
            resync: Arc::clone(&self.inner.resync),
            cancel: self.inner.cancel.clone(),
        }
    }

    /// Asks for a fresh snapshot for all subscribers.
    pub fn resync(&self) {
        self.inner.resync.notify_one();
    }

    /// Replaces the token and reconnects with it.
    pub fn set_token(&self, token: Token) {
        self.inner.token.send_replace(token);
        self.reconnect();
    }

    /// Drops the connection (or the wait before the next attempt) and connects again now.
    pub fn reconnect(&self) {
        let _ = self.inner.requester.commands.try_send(Command::Reconnect);
    }

    /// Closes the connection and waits for the background tasks to end.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        self.inner.tasks.wait().await;
    }

    /// Sends any command and decodes its result. Prefer the typed methods.
    pub async fn call<T: DeserializeOwned>(
        &self,
        command: &str,
        args: Option<Value>,
    ) -> Result<T, RequestError> {
        self.call_with_timeout(command, args, self.inner.requester.default_timeout())
            .await
    }

    pub async fn call_with_timeout<T: DeserializeOwned>(
        &self,
        command: &str,
        args: Option<Value>,
        timeout: Duration,
    ) -> Result<T, RequestError> {
        self.inner.requester.call(command, args, timeout).await
    }

    /// Sends any command and returns its raw result.
    pub async fn call_raw(
        &self,
        command: &str,
        args: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        self.inner.requester.value(command, args, timeout).await
    }

    pub(crate) fn requester(&self) -> &Requester {
        &self.inner.requester
    }
}

/// Connects once, authenticates and closes: for a "test connection" button.
pub async fn probe(options: &Options) -> Result<Session, ConnectError> {
    let tls = options.endpoint.is_secure().then(connection::tls_config);
    let Established {
        mut ws,
        server,
        user,
        compatibility,
    } = connection::establish(options, &options.token, tls.as_ref()).await?;
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.close(None)).await;
    Ok(Session {
        server,
        user,
        compatibility,
        generation: 0,
    })
}

/// An event subscription.
#[derive(Debug)]
pub struct Subscription {
    updates: broadcast::Receiver<Update>,
    resync: Arc<Notify>,
    cancel: CancellationToken,
}

impl Subscription {
    /// The next update, or `None` once the client has shut down.
    pub async fn recv(&mut self) -> Option<Update> {
        let received = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return None,
            received = self.updates.recv() => received,
        };
        match received {
            Ok(update) => Some(update),
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "a Music Assistant subscriber lagged; resyncing");
                self.resync.notify_one();
                Some(Update::Lagged { missed })
            }
            Err(RecvError::Closed) => None,
        }
    }
}

struct Supervisor {
    options: Options,
    tls: Option<Result<Arc<ClientConfig>, String>>,
    commands: mpsc::Receiver<Command>,
    state: watch::Sender<ConnectionState>,
    updates: broadcast::Sender<Update>,
    resync: Arc<Notify>,
    token: watch::Receiver<Token>,
    cancel: CancellationToken,
    permits: Arc<Semaphore>,
    generation: u64,
    rng: fastrand::Rng,
}

enum Wake {
    Retry,
    Stop,
}

impl Supervisor {
    async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            self.state
                .send_replace(ConnectionState::Connecting { attempt });
            let token = self.token.borrow().clone();
            let established = tokio::select! {
                () = self.cancel.cancelled() => break,
                established = connection::establish(&self.options, &token, self.tls.as_ref()) => established,
            };
            let delay = match established {
                Ok(established) => {
                    let started = Instant::now();
                    let ended = self.connected(established).await;
                    if matches!(ended, Ended::Shutdown) {
                        break;
                    }
                    if started.elapsed() >= self.options.stable_after
                        || matches!(ended, Ended::Reconnect)
                    {
                        attempt = 0;
                    }
                    let delay = if matches!(ended, Ended::Reconnect) {
                        Duration::ZERO
                    } else {
                        self.options.backoff.delay(attempt, &mut self.rng)
                    };
                    attempt = attempt.saturating_add(1);
                    let error = match ended {
                        Ended::Lost(why) => Some(ConnectError::Dropped(why)),
                        Ended::Dead => Some(ConnectError::Unresponsive),
                        Ended::Reconnect | Ended::Shutdown => None,
                    };
                    if let Some(error) = &error {
                        tracing::warn!(%error, retry_in = ?delay, "lost the Music Assistant connection");
                    }
                    self.state.send_replace(ConnectionState::Disconnected {
                        error,
                        retry_in: Some(delay),
                    });
                    delay
                }
                Err(error) => {
                    let delay = self.failed(error, attempt);
                    attempt = attempt.saturating_add(1);
                    delay
                }
            };
            match self.wait(delay).await {
                Wake::Retry => {}
                Wake::Stop => break,
            }
        }
        self.stop();
    }

    async fn connected(&mut self, established: Established) -> Ended {
        self.generation += 1;
        let Established {
            ws,
            server,
            user,
            compatibility,
        } = established;
        tracing::info!(
            version = %server.server_version,
            schema = server.schema_version,
            user = user.as_ref().map_or("", |u| u.username.as_str()),
            "connected to Music Assistant"
        );
        if compatibility == Compatibility::Newer {
            tracing::info!(
                schema = server.schema_version,
                tested = crate::schema::TESTED_SCHEMA,
                "Music Assistant is newer than this client was tested with"
            );
        }
        let session = Arc::new(Session {
            server,
            user,
            compatibility,
            generation: self.generation,
        });
        self.state.send_replace(ConnectionState::Connected {
            server_version: session.server.server_version.clone(),
            schema: session.server.schema_version,
            session,
        });
        self.resync.notify_one();
        connection::run(
            ws,
            Link {
                commands: &mut self.commands,
                updates: &self.updates,
                resync: &self.resync,
                cancel: &self.cancel,
                options: &self.options,
            },
        )
        .await
    }

    fn failed(&mut self, error: ConnectError, attempt: u32) -> Duration {
        match error {
            ConnectError::Auth(error) => {
                let retry_in = self.options.auth_retry;
                tracing::warn!(%error, ?retry_in, "Music Assistant refused the token");
                self.state
                    .send_replace(ConnectionState::AuthFailed { error, retry_in });
                retry_in
            }
            ConnectError::Incompatible(error) => {
                let retry_in = self.options.incompatible_retry;
                tracing::warn!(%error, ?retry_in, "Music Assistant is not compatible");
                self.state
                    .send_replace(ConnectionState::Incompatible { error, retry_in });
                retry_in
            }
            error => {
                let retry_in = if matches!(error, ConnectError::NoToken) {
                    self.options.auth_retry
                } else {
                    self.options.backoff.delay(attempt, &mut self.rng)
                };
                if attempt == 0 {
                    tracing::warn!(%error, ?retry_in, "could not connect to Music Assistant");
                } else {
                    tracing::debug!(%error, attempt, ?retry_in, "could not connect to Music Assistant");
                }
                self.state.send_replace(ConnectionState::Disconnected {
                    error: Some(error),
                    retry_in: Some(retry_in),
                });
                retry_in
            }
        }
    }

    /// Waits before the next attempt, answering requests with `NotConnected` meanwhile.
    async fn wait(&mut self, delay: Duration) -> Wake {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Wake::Stop,
                () = &mut sleep => return Wake::Retry,
                command = self.commands.recv() => match command {
                    Some(Command::Request(request)) => {
                        let _ = request.reply.send(Err(RequestError::NotConnected));
                    }
                    Some(Command::Reconnect) => return Wake::Retry,
                    None => return Wake::Stop,
                },
            }
        }
    }

    fn stop(mut self) {
        self.permits.close();
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            if let Command::Request(request) = command {
                let _ = request.reply.send(Err(RequestError::ShutDown));
            }
        }
        self.state.send_replace(ConnectionState::Stopped);
        tracing::debug!("Music Assistant client stopped");
    }
}

const RESYNC_MIN_INTERVAL: Duration = Duration::from_secs(1);
const RESYNC_RETRY_MAX: Duration = Duration::from_secs(30);

/// Fetches and broadcasts snapshots: after each connect, and when a subscriber lagged or an
/// event could not be decoded. Notifications coalesce, so a burst of lags costs one fetch.
async fn resync_worker(
    requester: Requester,
    updates: broadcast::Sender<Update>,
    resync: Arc<Notify>,
    state: watch::Receiver<ConnectionState>,
    cancel: CancellationToken,
) {
    let mut last_generation = 0;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = resync.notified() => {}
        }
        let mut retry = RESYNC_MIN_INTERVAL;
        loop {
            let Some(generation) = state.borrow().session().map(|s| s.generation) else {
                break;
            };
            let reason = if generation == last_generation {
                SnapshotReason::Resync
            } else {
                SnapshotReason::Connected
            };
            match snapshot(&requester, &updates, reason).await {
                Ok(Some(snapshot)) => {
                    last_generation = generation;
                    let _ = updates.send(Update::Snapshot(Arc::new(snapshot)));
                    break;
                }
                Ok(None) => tracing::debug!("events overflowed while fetching a snapshot; again"),
                Err(error) => {
                    tracing::warn!(%error, "could not fetch a Music Assistant snapshot");
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(retry) => {}
            }
            retry = (retry * 2).min(RESYNC_RETRY_MAX);
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(RESYNC_MIN_INTERVAL) => {}
        }
    }
}

/// Every player and queue, with the events that arrived meanwhile applied so the snapshot is
/// never older than an event already delivered. `None` if too many events arrived to track.
async fn snapshot(
    requester: &Requester,
    updates: &broadcast::Sender<Update>,
    reason: SnapshotReason,
) -> Result<Option<Snapshot>, RequestError> {
    let mut window = updates.subscribe();
    let timeout = requester.default_timeout();
    let (players, queues) = tokio::try_join!(
        requester.list::<Player>(
            "players/all",
            Some(json!({ "return_unavailable": true })),
            timeout
        ),
        requester.list::<PlayerQueue>("player_queues/all", None, timeout),
    )?;
    let mut snapshot = Snapshot::new(players, queues, reason);
    loop {
        match window.try_recv() {
            Ok(Update::Event(event)) => snapshot.apply(&event),
            Ok(_) => {}
            Err(TryRecvError::Lagged(_)) => return Ok(None),
            Err(TryRecvError::Empty | TryRecvError::Closed) => return Ok(Some(snapshot)),
        }
    }
}
