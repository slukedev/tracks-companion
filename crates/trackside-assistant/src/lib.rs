//! Music Assistant for Trackside (ADR 0013): the [`AssistantControl`] the HTTP crates use.
//!
//! One [`trackside_mass::Client`] keeps the connection; it is replaced whenever the admin
//! changes the address, token or certificate, so nothing needs a restart. A pump task per
//! connection turns Music Assistant's events into speakers ([`hub`]) and publishes
//! coalesced changes on Trackside's event bus, whose subscribers lag on their own and never
//! hold up the connection. Commands answer `202` at once with the expected state and run in
//! their own tasks; their outcome is an event.
//!
//! Trackside also registers itself in Music Assistant as an `OpenSubsonic` source, with a
//! service account and a key scoped `library stream`, so the library plays on every speaker
//! ([`registration`]).
//!
//! The streaming catalog ([`catalog`]) reads the services connected in Music Assistant over
//! the same connection.
//!
//! No audio passes through here: speakers play streaming services in Music Assistant
//! itself, and nothing from a streaming service is downloaded, stored or exported.

mod catalog;
mod commands;
mod convert;
mod hub;
mod identities;
mod registration;
mod relay;
mod settings;

pub use relay::RelayTuning;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use trackside_auth::Auth;
use trackside_core::assistant::{
    AssistantChange, AssistantControl, AssistantError, AssistantOverview, AssistantStatus,
    AssistantTest, AssistantUser, CatalogControl, CommandAccepted, ConnectionState, LanUrl,
    MusicProvider, PlayerProvider, ProviderLists, QueuePage, RegistrationView, STABLE_SCHEMA,
    ServerFacts, Speaker, SpeakerCommand, SpeakerError, SpeakerEvent, SpeakerList, SpeakerQueue,
    SpeakersState, TESTED_SCHEMA,
};
use trackside_core::catalog::{
    CatalogDetail, CatalogError, CatalogKind, CatalogPage, CatalogResults, CatalogSearch,
    LibraryPage, ServiceList,
};
use trackside_core::config::Secret;
use trackside_core::relay::{
    RelayBody, RelayCaller, RelayControl, RelayError, RelayOpen, RelayOpened, RelayPart, RelayTier,
    RelayView,
};
use trackside_core::{
    Access, BoxFuture, CatalogArtId, Config, ConfigReport, Db, DeviceKey, EventBus, Millis,
    RelaySessionId, ServerEvent, ServiceId, SongId, SpeakerId, UserKey,
};
use trackside_mass::models::{ProviderType, ServerInfo};
use trackside_mass::{Client, ConnectError, Endpoint, Options, Session, Token, Update};

use crate::hub::{Hub, ListState};
use crate::registration::RegState;
use crate::settings::Effective;

/// The integration's name on service accounts and managed keys.
pub const SERVICE: &str = "music_assistant";
/// The username and display name of the service account, and the name of its key.
pub const SERVICE_USERNAME: &str = "music-assistant";
pub const SERVICE_DISPLAY_NAME: &str = "Music Assistant";

/// Waits a test may shorten.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// How long a lost connection reads `connecting` before `unavailable`.
    pub unavailable_after: Duration,
    /// How long one Music Assistant command may take.
    pub command_timeout: Duration,
    /// Reconnect backoff and the wait after a refused token.
    pub retry: Duration,
    pub relay: RelayTuning,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            unavailable_after: Duration::from_secs(30),
            command_timeout: Duration::from_secs(10),
            retry: Duration::from_secs(5),
            relay: RelayTuning::default(),
        }
    }
}

/// What the integration needs from the server.
pub struct Deps {
    pub db: Db,
    pub auth: Arc<Auth>,
    pub events: EventBus,
    pub config: Arc<Config>,
    pub report: Arc<ConfigReport>,
    /// The name Trackside shows itself under in Music Assistant.
    pub server_name: String,
    /// The first LAN address Trackside detected for itself (`/server/urls`), with the base
    /// path.
    pub lan_url: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub shutdown: CancellationToken,
    pub tuning: Tuning,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The connection as the dashboard sees it.
#[derive(Clone, Debug)]
struct Conn {
    state: ConnectionState,
    since: Option<Millis>,
    last_error: Option<(Millis, String)>,
    session: Option<Arc<Session>>,
    list: ListState,
}

impl Default for Conn {
    fn default() -> Self {
        Self {
            state: ConnectionState::NotSetUp,
            since: None,
            last_error: None,
            session: None,
            list: ListState {
                state: SpeakersState::Unavailable,
                since: None,
            },
        }
    }
}

struct Link {
    client: Client,
    endpoint: Endpoint,
    cancel: CancellationToken,
}

#[derive(Clone, Debug)]
struct Pending {
    user: trackside_core::UserKey,
    speaker: SpeakerId,
}

pub(crate) struct Inner {
    db: Db,
    auth: Arc<Auth>,
    events: EventBus,
    config: Arc<Config>,
    report: Arc<ConfigReport>,
    server_name: String,
    lan_url: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    tuning: Tuning,
    effective: RwLock<Effective>,
    link: Mutex<Option<Link>>,
    hub: Mutex<Hub>,
    conn: Mutex<Conn>,
    pending: Mutex<HashMap<trackside_core::SpeakerCommandId, Pending>>,
    slots: Mutex<HashMap<(SpeakerId, commands::Slot), commands::SlotState>>,
    reg: Mutex<RegState>,
    catalog: catalog::State,
    reg_lock: tokio::sync::Mutex<()>,
    reconfigure_lock: tokio::sync::Mutex<()>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    relay: relay::Relays,
    /// Stops the listeners that live as long as the integration; cancelled by
    /// [`Assistant::shutdown`].
    listeners: CancellationToken,
    /// Told whenever the speakers' state may have changed.
    hub_changed: tokio::sync::Notify,
}

/// Music Assistant for Trackside. Cheap to clone.
#[derive(Clone)]
pub struct Assistant {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Assistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assistant").finish_non_exhaustive()
    }
}

impl Assistant {
    /// Reads the settings and connects when Music Assistant is set up.
    pub async fn start(deps: Deps) -> Self {
        let stored = settings::load_stored(&deps.db).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot read the Music Assistant settings");
            settings::Stored::default()
        });
        let effective = settings::effective(&deps.config, &deps.report, &deps.auth, &stored);
        if let Some(error) = &effective.ca_error {
            tracing::warn!(%error, "the Music Assistant certificate authority cannot be used");
        }
        let identities = Arc::new(identities::SealedIdentities::new(
            deps.db.clone(),
            Arc::clone(&deps.auth),
        ));
        if let Err(error) = identities.load_all().await {
            tracing::warn!(%error, "cannot read the relay player keys");
        }
        let ffmpeg = trackside_relay::find_ffmpeg(deps.config.media.ffmpeg_path.as_deref());
        let relay = relay::Relays::new(identities, ffmpeg, deps.tuning.relay.clone());
        let inner = Arc::new(Inner {
            db: deps.db,
            auth: deps.auth,
            events: deps.events,
            config: deps.config,
            report: deps.report,
            server_name: deps.server_name,
            lan_url: deps.lan_url,
            tuning: deps.tuning,
            effective: RwLock::new(effective),
            link: Mutex::new(None),
            hub: Mutex::new(Hub::default()),
            conn: Mutex::new(Conn::default()),
            pending: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
            reg: Mutex::new(RegState::default()),
            catalog: catalog::State::default(),
            reg_lock: tokio::sync::Mutex::new(()),
            reconfigure_lock: tokio::sync::Mutex::new(()),
            shutdown: deps.shutdown.clone(),
            tasks: TaskTracker::new(),
            relay,
            listeners: deps.shutdown.child_token(),
            hub_changed: tokio::sync::Notify::new(),
        });
        lock(&inner.hub)
            .relay_players
            .extend(inner.relay.identities.player_ids());
        inner.sweep_identities().await;
        inner.tasks.spawn(Arc::clone(&inner).follow_revocations());
        inner.reconfigure().await;
        Self { inner }
    }

    /// The live client while one is configured, for the streaming catalog to build on.
    pub fn client(&self) -> Option<Client> {
        lock(&self.inner.link).as_ref().map(|l| l.client.clone())
    }

    /// Ends every relay with a goodbye to Music Assistant, disconnects and waits for every
    /// task.
    pub async fn shutdown(&self) {
        self.inner.shutdown_relays().await;
        self.inner.listeners.cancel();
        let link = lock(&self.inner.link).take();
        if let Some(link) = link {
            link.cancel.cancel();
            link.client.shutdown().await;
        }
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
    }
}

impl Inner {
    fn effective(&self) -> Effective {
        self.effective
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn now(&self) -> Millis {
        self.db.clock().now()
    }

    fn publish(&self, events: Vec<SpeakerEvent>) {
        self.hub_changed.notify_waiters();
        for event in events {
            if let SpeakerEvent::State { state, .. } = &event {
                self.events
                    .publish(ServerEvent::CatalogServices { state: *state });
            }
            self.events.publish(ServerEvent::Speaker(self.owned(event)));
        }
    }

    /// Ends relays of revoked credentials, and drops the keys of revoked devices.
    async fn follow_revocations(self: Arc<Self>) {
        let mut events = self.events.subscribe();
        loop {
            let event = tokio::select! {
                () = self.listeners.cancelled() => return,
                event = events.recv() => event,
            };
            match event {
                Ok(event) => {
                    if let ServerEvent::CredentialRevoked { user, device } = &*event {
                        self.on_revoked(*user, *device).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    self.sweep_identities().await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// The connected client, or `Unavailable`.
    pub(crate) fn connected(&self) -> Result<Client, SpeakerError> {
        let conn = lock(&self.conn);
        if conn.state != ConnectionState::Connected {
            return Err(SpeakerError::Unavailable);
        }
        drop(conn);
        lock(&self.link)
            .as_ref()
            .map(|l| l.client.clone())
            .ok_or(SpeakerError::Unavailable)
    }

    fn session(&self) -> Option<Arc<Session>> {
        lock(&self.conn).session.clone()
    }

    /// Replaces the connection to match the settings in force.
    async fn reconfigure(self: &Arc<Self>) {
        let _guard = self.reconfigure_lock.lock().await;
        let old = lock(&self.link).take();
        if let Some(old) = old {
            old.cancel.cancel();
            let client = old.client;
            self.tasks.spawn(async move { client.shutdown().await });
        }
        self.fail_pending(
            "speakers_unavailable",
            "Speakers aren't reachable right now.",
        );
        lock(&self.hub).reset(None, None);
        self.catalog.reset();
        let effective = self.effective();
        let now = self.now();
        let (state, endpoint) = match (
            effective.configured(),
            effective.enabled,
            effective.endpoint(),
        ) {
            (false, _, _) => (ConnectionState::NotSetUp, None),
            (true, false, _) => (ConnectionState::Disabled, None),
            (true, true, None) => (ConnectionState::Unreachable, None),
            (true, true, Some(endpoint)) => (ConnectionState::Connecting, Some(endpoint)),
        };
        {
            let mut conn = lock(&self.conn);
            let previous = conn.list;
            *conn = Conn {
                state,
                since: Some(now),
                last_error: None,
                session: None,
                list: ListState {
                    state: if state == ConnectionState::Connecting {
                        SpeakersState::Connecting
                    } else {
                        SpeakersState::Unavailable
                    },
                    since: (state != ConnectionState::Connecting).then_some(now),
                },
            };
            if state == ConnectionState::Unreachable {
                conn.last_error = Some((
                    now,
                    String::from("The Music Assistant address is not valid."),
                ));
            }
            if previous.state != conn.list.state {
                let list = conn.list;
                drop(conn);
                self.publish(vec![SpeakerEvent::State {
                    state: list.state,
                    since: list.since,
                }]);
            }
        }
        let (Some(endpoint), Some(token)) = (endpoint, effective.token.clone()) else {
            self.rebuild_relay(None);
            return;
        };
        self.rebuild_relay(Some((endpoint.ws_url(), token.expose().to_owned())));
        let mut options = Options::new(endpoint.clone(), token);
        options.custom_ca.clone_from(&effective.ca);
        options.request_timeout = self.tuning.command_timeout;
        options.auth_retry = self.tuning.retry.max(Duration::from_millis(100)) * 12;
        options.backoff = trackside_mass::Backoff {
            initial: self.tuning.retry.min(Duration::from_secs(1)),
            max: self.tuning.retry.max(Duration::from_secs(1)) * 12,
        };
        let cancel = self.shutdown.child_token();
        let client = Client::start_with(options, cancel.clone());
        *lock(&self.link) = Some(Link {
            client: client.clone(),
            endpoint: endpoint.clone(),
            cancel: cancel.clone(),
        });
        self.tasks
            .spawn(Arc::clone(self).watch(client.clone(), cancel.clone()));
        self.tasks
            .spawn(Arc::clone(self).pump(client, endpoint, cancel));
    }

    /// Follows the connection state: the dashboard's view, the speakers list state and its
    /// event, and failing commands that can no longer finish.
    #[expect(
        clippy::too_many_lines,
        reason = "one state machine, read in one place"
    )]
    async fn watch(self: Arc<Self>, client: Client, cancel: CancellationToken) {
        let mut states = client.state();
        let mut unavailable_at = Some(Instant::now() + self.tuning.unavailable_after);
        loop {
            let state = states.borrow_and_update().clone();
            let now = self.now();
            let mut events = Vec::new();
            {
                let mut conn = lock(&self.conn);
                let before = conn.list;
                match &state {
                    trackside_mass::ConnectionState::Connected { session, .. } => {
                        if conn.state != ConnectionState::Connected {
                            conn.since = Some(now);
                        }
                        conn.state = ConnectionState::Connected;
                        conn.last_error = None;
                        conn.session = Some(Arc::clone(session));
                        conn.list = ListState {
                            state: SpeakersState::Ok,
                            since: None,
                        };
                        unavailable_at = None;
                    }
                    trackside_mass::ConnectionState::Connecting { .. } => {
                        if conn.state == ConnectionState::Connected {
                            conn.state = ConnectionState::Connecting;
                            conn.since = Some(now);
                        }
                    }
                    trackside_mass::ConnectionState::Disconnected { error, .. } => {
                        let (code, message) = describe(error.as_ref());
                        if conn.state != code {
                            conn.since = Some(now);
                        }
                        conn.state = code;
                        conn.session = None;
                        if let Some(message) = message {
                            conn.last_error = Some((now, message));
                        }
                    }
                    trackside_mass::ConnectionState::AuthFailed { error, .. } => {
                        conn.state = ConnectionState::AuthFailed;
                        conn.since = Some(now);
                        conn.session = None;
                        conn.last_error = Some((
                            now,
                            format!(
                                "Music Assistant refused the token (error {}: {}).",
                                error.code,
                                error.details.as_deref().unwrap_or("no details")
                            ),
                        ));
                        conn.list = ListState {
                            state: SpeakersState::Unavailable,
                            since: Some(now),
                        };
                    }
                    trackside_mass::ConnectionState::Incompatible { error, .. } => {
                        conn.state = ConnectionState::Incompatible;
                        conn.since = Some(now);
                        conn.session = None;
                        conn.last_error = Some((now, error.to_string()));
                        conn.list = ListState {
                            state: SpeakersState::Unavailable,
                            since: Some(now),
                        };
                    }
                    trackside_mass::ConnectionState::Stopped => return,
                    _ => {}
                }
                if !state.is_connected() && conn.list.state == SpeakersState::Ok {
                    conn.list = ListState {
                        state: SpeakersState::Connecting,
                        since: None,
                    };
                    unavailable_at = Some(Instant::now() + self.tuning.unavailable_after);
                }
                if conn.list != before {
                    events.push(SpeakerEvent::State {
                        state: conn.list.state,
                        since: conn.list.since,
                    });
                    if conn.list.state == SpeakersState::Unavailable {
                        self.end_relays(trackside_relay::EndReason::AssistantUnavailable);
                    }
                }
            }
            if !state.is_connected() {
                self.fail_pending(
                    "speakers_unavailable",
                    "Speakers aren't reachable right now.",
                );
            }
            self.publish(events);
            let deadline = unavailable_at;
            tokio::select! {
                () = cancel.cancelled() => return,
                changed = states.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = sleep_until(deadline) => {
                    unavailable_at = None;
                    let mut conn = lock(&self.conn);
                    if conn.list.state == SpeakersState::Connecting {
                        let now = self.now();
                        conn.list = ListState { state: SpeakersState::Unavailable, since: Some(now) };
                        drop(conn);
                        self.end_relays(trackside_relay::EndReason::AssistantUnavailable);
                        self.publish(vec![SpeakerEvent::State {
                            state: SpeakersState::Unavailable,
                            since: Some(now),
                        }]);
                    }
                }
            }
        }
    }

    /// Turns Music Assistant's events into speakers and announces the changes.
    #[expect(clippy::too_many_lines, reason = "the event loop, read in one place")]
    async fn pump(self: Arc<Self>, client: Client, endpoint: Endpoint, cancel: CancellationToken) {
        let mut feed = client.subscribe();
        let mut unknown: HashSet<SongId> = HashSet::new();
        loop {
            let due = lock(&self.hub).next_due();
            let update = tokio::select! {
                () = cancel.cancelled() => return,
                () = sleep_until(due) => None,
                update = feed.recv() => match update {
                    Some(update) => Some(update),
                    None => return,
                },
            };
            let mut queue_changed = None;
            let mut connected = false;
            let mut catalog_changed = false;
            match update {
                None => {
                    let now = self.now();
                    let mut hub = lock(&self.hub);
                    let mut events = hub.flush(Instant::now());
                    events.extend(hub.heartbeats(now, Instant::now()));
                    drop(hub);
                    self.publish(events);
                    continue;
                }
                Some(Update::Snapshot(snapshot)) => {
                    let server_id = client
                        .current_state()
                        .session()
                        .map(|s| s.server.server_id.clone());
                    let providers = match client.providers(None).await {
                        Ok(providers) => Some(providers),
                        Err(e) => {
                            tracing::warn!(error = %e, "cannot list the Music Assistant providers");
                            None
                        }
                    };
                    let ours = settings::load_registration(&self.db)
                        .await
                        .ok()
                        .flatten()
                        .filter(|r| Some(&r.server_id) == server_id.as_ref())
                        .map(|r| r.instance_id);
                    let mut hub = lock(&self.hub);
                    connected =
                        snapshot.reason == trackside_mass::SnapshotReason::Connected || !hub.loaded;
                    // A new connection announces every speaker afresh (contract 1.10).
                    if connected {
                        hub.reset(server_id, Some(endpoint.clone()));
                        unknown.clear();
                    }
                    hub.load(&snapshot);
                    if let Some(providers) = providers {
                        hub.providers = providers;
                        catalog_changed = true;
                    }
                    if ours.is_some() {
                        hub.ours = ours;
                    }
                }
                Some(Update::Event(event)) => {
                    let epoch = self.now().0 as f64 / 1000.0;
                    queue_changed = lock(&self.hub).apply(&event, epoch);
                    if matches!(*event, trackside_mass::Event::ProvidersUpdated(_)) {
                        catalog_changed = true;
                        let me = Arc::clone(&self);
                        self.tasks
                            .spawn(async move { me.check_registration(false).await });
                    }
                }
                Some(_) => continue,
            }
            let wanted: Vec<SongId> = lock(&self.hub)
                .view()
                .map(|v| v.unresolved_songs())
                .unwrap_or_default()
                .into_iter()
                .filter(|id| !unknown.contains(id))
                .collect();
            if !wanted.is_empty() {
                let ids = wanted.clone();
                match self
                    .db
                    .read(move |c| trackside_core::repo::songs::placement(c, &ids))
                    .await
                {
                    Ok(found) => {
                        for id in &wanted {
                            if !found.contains_key(id) {
                                unknown.insert(id.clone());
                            }
                        }
                        lock(&self.hub).remember_songs(found, &wanted);
                    }
                    Err(e) => tracing::warn!(error = %e, "cannot look up the songs speakers play"),
                }
            }
            let now = self.now();
            let mut hub = lock(&self.hub);
            let mut events = hub.refresh(now, Instant::now());
            if let Some(queue) = queue_changed {
                events.extend(hub.touch_queue(&queue));
            }
            drop(hub);
            self.publish(events);
            if catalog_changed {
                let state = lock(&self.conn).list.state;
                self.events.publish(ServerEvent::CatalogServices { state });
            }
            if connected {
                let me = Arc::clone(&self);
                self.tasks
                    .spawn(async move { me.check_registration(false).await });
            }
        }
    }

    fn fail_pending(&self, code: &'static str, message: &str) {
        let drained: Vec<_> = lock(&self.pending).drain().collect();
        let events = drained
            .into_iter()
            .map(|(command, pending)| SpeakerEvent::Command {
                user: pending.user,
                command,
                speaker: pending.speaker,
                outcome: Err(trackside_core::assistant::CommandFailure {
                    code,
                    message: message.to_owned(),
                }),
            })
            .collect();
        self.publish(events);
    }

    fn server_facts(info: &ServerInfo) -> ServerFacts {
        ServerFacts {
            server_id: info.server_id.clone(),
            name: info.name.clone(),
            version: info.server_version.clone(),
            schema_version: info.schema_version,
            min_supported_schema_version: info.min_supported_schema_version,
            compatible: info.schema_version >= STABLE_SCHEMA
                && info.min_supported_schema_version <= STABLE_SCHEMA,
            tested: info.schema_version <= TESTED_SCHEMA,
            relay_supported: false,
        }
    }

    fn ui_url(&self) -> Option<String> {
        lock(&self.link).as_ref().map(|l| l.endpoint.http_base())
    }

    /// The address Music Assistant reaches Trackside at.
    fn lan_url(&self) -> Option<LanUrl> {
        let effective = self.effective();
        let (url, configured) = match effective.lan_url {
            Some(url) => (url, true),
            None => ((self.lan_url)()?, false),
        };
        let url = url.trim().trim_end_matches('/').to_owned();
        let warning = settings::valid_http_url(&url).ok().and_then(|uri| {
            let ma_remote = effective
                .url
                .as_deref()
                .and_then(|u| settings::valid_http_url(u).ok())
                .is_some_and(|ma| !settings::is_loopback(&ma));
            (settings::is_loopback(&uri) && ma_remote).then(|| {
                String::from(
                    "This address only works on this machine, and Music Assistant runs elsewhere.",
                )
            })
        });
        let reg = lock(&self.reg);
        Some(LanUrl {
            url,
            configured,
            reachable: reg.reachable,
            checked_at: reg.checked_at,
            warning,
        })
    }

    fn counts(&self) -> (Option<u32>, Option<u32>) {
        if lock(&self.conn).state != ConnectionState::Connected {
            return (None, None);
        }
        let hub = lock(&self.hub);
        let hidden = self.effective().hidden_services;
        let services = hub
            .providers
            .iter()
            .filter(|p| {
                p.provider_type == ProviderType::Music
                    && p.is_streaming_provider == Some(true)
                    && !hidden.contains(&p.instance_id)
            })
            .count();
        let speakers = hub.speakers().iter().filter(|s| s.relay.is_none()).count();
        (
            Some(u32::try_from(services).unwrap_or(u32::MAX)),
            Some(u32::try_from(speakers).unwrap_or(u32::MAX)),
        )
    }

    fn status(&self) -> AssistantStatus {
        let effective = self.effective();
        let conn = lock(&self.conn).clone();
        let (services, speakers) = self.counts();
        let configured = effective.configured();
        if !configured {
            let mut status = AssistantStatus::not_set_up();
            status.url.clone_from(&effective.url);
            status.url_source = effective.url_source;
            status.token_set = effective.token.is_some();
            status.token_source = effective.token_source;
            status.token_set_at = effective.token_set_at;
            status.custom_ca = effective.ca.is_some();
            return status;
        }
        let session = conn.session.as_ref();
        AssistantStatus {
            configured,
            enabled: effective.enabled,
            auto_register: effective.auto_register,
            state: conn.state,
            since: conn.since,
            last_error: conn.last_error.clone().or_else(|| {
                effective.ca_error.clone().map(|e| {
                    (
                        self.now(),
                        format!("The certificate authority cannot be used: {e}"),
                    )
                })
            }),
            hidden_services: effective.hidden_services.clone(),
            lan_url: self.lan_url(),
            relays_active: self.relay.active(),
            relays_max: effective.relay_max_sessions,
            server: session.map(|s| {
                let mut facts = Self::server_facts(&s.server);
                facts.relay_supported = self.relay_supported();
                facts
            }),
            services,
            speakers,
            url_source: effective.url_source,
            token_source: effective.token_source,
            token_set: effective.token.is_some(),
            token_set_at: effective.token_set_at,
            custom_ca: effective.ca.is_some(),
            ui_url: effective.endpoint().map(|e| e.http_base()),
            url: effective.url.clone(),
            user: session
                .and_then(|s| s.user.as_ref())
                .map(|u| AssistantUser {
                    username: u.username.clone(),
                    display_name: u.display_name.clone(),
                    role: u.role.clone(),
                }),
        }
    }

    #[expect(clippy::too_many_lines, reason = "one check per field of the change")]
    async fn update(
        self: &Arc<Self>,
        change: AssistantChange,
    ) -> Result<AssistantStatus, AssistantError> {
        let effective = self.effective();
        let locked = |field: &'static str, touched: bool| {
            if touched && effective.locked.contains(&field) {
                Err(AssistantError::SetByConfig { field })
            } else {
                Ok(())
            }
        };
        locked("url", change.url.is_some())?;
        locked("token", change.token.is_some())?;
        locked("enabled", change.enabled.is_some())?;
        locked("auto_register", change.auto_register.is_some())?;
        locked("lan_url", change.lan_url.is_some())?;
        locked("ca_pem", change.ca_pem.is_some())?;
        locked("hidden_services", change.hidden_services.is_some())?;
        locked("relay_max_sessions", change.relay_max_sessions.is_some())?;
        let invalid =
            |field: &'static str, message: String| AssistantError::Invalid { field, message };
        let mut row = settings::load_stored(&self.db).await?;
        if let Some(url) = &change.url {
            match url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                Some(url) => {
                    settings::valid_http_url(url)
                        .map_err(|e| invalid("url", format!("url {e}.")))?;
                    Endpoint::parse(url).map_err(|e| invalid("url", format!("url: {e}.")))?;
                    row.url = Some(url.trim_end_matches('/').to_owned());
                }
                None => row.url = None,
            }
        }
        if let Some(lan) = &change.lan_url {
            match lan.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                Some(lan) => {
                    settings::valid_http_url(lan)
                        .map_err(|e| invalid("lan_url", format!("lan_url {e}.")))?;
                    row.lan_url = Some(lan.trim_end_matches('/').to_owned());
                }
                None => row.lan_url = None,
            }
        }
        if let Some(pem) = &change.ca_pem {
            match pem.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                Some(pem) => {
                    if pem.len() > 256 * 1024 {
                        return Err(invalid("ca_pem", String::from("ca_pem is too long.")));
                    }
                    trackside_mass::CustomCa::from_pem(pem.as_bytes())
                        .map_err(|e| invalid("ca_pem", format!("ca_pem: {e}.")))?;
                    row.ca_pem = Some(pem.to_owned());
                }
                None => row.ca_pem = None,
            }
        }
        if let Some(token) = &change.token {
            match token.as_ref().filter(|t| !t.expose().trim().is_empty()) {
                Some(token) => {
                    if token.expose().len() > 8192 {
                        return Err(invalid("token", String::from("token is too long.")));
                    }
                    let trimmed = Secret::new(token.expose().trim());
                    row.token = Some(
                        settings::seal_token(&self.auth, &trimmed)
                            .map_err(|e| AssistantError::Failed(e.to_string()))?,
                    );
                    row.token_set_at = Some(self.now().0);
                }
                None => {
                    row.token = None;
                    row.token_set_at = None;
                }
            }
        }
        if let Some(hidden) = &change.hidden_services {
            if hidden.len() > 500 || hidden.iter().any(|h| h.is_empty() || h.len() > 200) {
                return Err(invalid(
                    "hidden_services",
                    String::from("hidden_services must hold provider instance ids."),
                ));
            }
            row.hidden_services = Some(hidden.clone());
        }
        if let Some(max) = change.relay_max_sessions {
            if !(1..=64).contains(&max) {
                return Err(invalid(
                    "relay_max_sessions",
                    String::from("relay_max_sessions is 1 to 64."),
                ));
            }
            row.relay_max_sessions = Some(max);
        }
        if let Some(enabled) = change.enabled {
            row.enabled = Some(enabled);
        }
        if let Some(auto) = change.auto_register {
            row.auto_register = Some(auto);
        }
        settings::save_stored(&self.db, &row).await?;
        let fresh = settings::effective(&self.config, &self.report, &self.auth, &row);
        let reconnect = fresh.url != effective.url
            || fresh.token != effective.token
            || fresh.enabled != effective.enabled
            || fresh.ca != effective.ca;
        let reregister = fresh.lan_url != effective.lan_url;
        let fresh_max = fresh.relay_max_sessions;
        *self
            .effective
            .write()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
        self.relay
            .set_limits(fresh_max, self.config.music_assistant.relay_max_per_user);
        if change.hidden_services.is_some() {
            self.catalog.reset();
            let state = lock(&self.conn).list.state;
            self.events.publish(ServerEvent::CatalogServices { state });
        }
        if reconnect {
            self.reconfigure().await;
        } else if reregister || (change.auto_register == Some(true)) {
            let me = Arc::clone(self);
            self.tasks
                .spawn(async move { me.check_registration(reregister).await });
        }
        Ok(self.status())
    }

    async fn remove(self: &Arc<Self>) -> Result<(), AssistantError> {
        if let Err(e) = self.unregister().await {
            tracing::warn!(error = %e, "could not unregister from Music Assistant; forgetting it anyway");
        }
        settings::delete_stored(&self.db).await?;
        let fresh = settings::effective(
            &self.config,
            &self.report,
            &self.auth,
            &settings::Stored::default(),
        );
        *self
            .effective
            .write()
            .unwrap_or_else(PoisonError::into_inner) = fresh;
        self.reconfigure().await;
        Ok(())
    }

    async fn test(&self, url: Option<String>, token: Option<Secret>) -> AssistantTest {
        let started = std::time::Instant::now();
        let effective = self.effective();
        let fail = |detail: String, server: Option<ServerFacts>| AssistantTest {
            ok: false,
            detail,
            elapsed: started.elapsed(),
            players: None,
            providers: None,
            server,
        };
        let Some(url) = url.or(effective.url.clone()) else {
            return fail(String::from("No Music Assistant address is set."), None);
        };
        let endpoint = match settings::valid_http_url(&url)
            .map_err(str::to_owned)
            .and_then(|_| Endpoint::parse(&url).map_err(|e| e.to_string()))
        {
            Ok(endpoint) => endpoint,
            Err(e) => return fail(format!("The address {e}."), None),
        };
        let token = match token {
            Some(token) => Token::new(token.expose()),
            None => match effective.token.clone() {
                Some(token) => token,
                None => return fail(String::from("No Music Assistant token is set."), None),
            },
        };
        let mut options = Options::new(endpoint.clone(), token.clone());
        options.custom_ca.clone_from(&effective.ca);
        options.connect_timeout = Duration::from_secs(5);
        options.handshake_timeout = Duration::from_secs(5);
        let session = match trackside_mass::probe(&options).await {
            Ok(session) => session,
            Err(error) => {
                let (_, message) = describe(Some(&error));
                return fail(message.unwrap_or_else(|| error.to_string()), None);
            }
        };
        let facts = Self::server_facts(&session.server);
        if !facts.compatible {
            return fail(
                format!(
                    "Music Assistant {} is not supported; Trackside needs 2.10 or newer.",
                    facts.version
                ),
                Some(facts),
            );
        }
        // Counting players and providers needs a full connection: a short-lived client.
        let mut probe_options = Options::new(endpoint, token);
        probe_options.custom_ca.clone_from(&effective.ca);
        probe_options.request_timeout = Duration::from_secs(5);
        let cancel = CancellationToken::new();
        let client = Client::start_with(probe_options, cancel.clone());
        let counted = tokio::time::timeout(Duration::from_secs(8), async {
            client.connected().await?;
            let players = client.players().await.ok()?;
            let providers = client.providers(None).await.ok()?;
            Some((players.len(), providers.len()))
        })
        .await
        .ok()
        .flatten();
        client.shutdown().await;
        let user = session.user.as_ref().map_or_else(String::new, |u| {
            format!(
                " as {}{}",
                u.username,
                u.role
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            )
        });
        AssistantTest {
            ok: true,
            detail: format!("Connected to Music Assistant {}{user}.", facts.version),
            elapsed: started.elapsed(),
            players: counted.map(|(p, _)| u32::try_from(p).unwrap_or(u32::MAX)),
            providers: counted.map(|(_, p)| u32::try_from(p).unwrap_or(u32::MAX)),
            server: Some(facts),
        }
    }

    async fn providers(&self) -> Result<ProviderLists, AssistantError> {
        let client = self.connected().map_err(|_| AssistantError::Unavailable)?;
        let providers = client
            .providers(None)
            .await
            .map_err(|_| AssistantError::Unavailable)?;
        let base = self.ui_url();
        let edit = |instance: &str| {
            base.as_ref()
                .map(|b| format!("{b}/#/settings/editprovider/{instance}"))
        };
        let hidden = self.effective().hidden_services;
        let (server_id, ours, counts) = {
            let hub = lock(&self.hub);
            let mut counts: HashMap<String, u32> = HashMap::new();
            for player in hub
                .players
                .values()
                .filter(|p| convert::visible(p) && !hub.is_relay(&p.player_id))
            {
                *counts.entry(player.provider.clone()).or_default() += 1;
            }
            (
                hub.server_id.clone().unwrap_or_default(),
                hub.ours.clone(),
                counts,
            )
        };
        let mut music = Vec::new();
        let mut players = Vec::new();
        for p in providers {
            match p.provider_type {
                ProviderType::Music => {
                    let streaming = p.is_streaming_provider == Some(true);
                    music.push(MusicProvider {
                        ui_url: edit(&p.instance_id),
                        service_id: streaming
                            .then(|| ServiceId::derive(&server_id, &p.instance_id)),
                        hidden: hidden.contains(&p.instance_id),
                        trackside: ours.as_deref() == Some(p.instance_id.as_str()),
                        instance_id: p.instance_id,
                        domain: p.domain,
                        name: p.name,
                        available: p.available,
                        streaming,
                    });
                }
                ProviderType::Player => players.push(PlayerProvider {
                    ui_url: edit(&p.instance_id),
                    speakers: counts.get(&p.instance_id).copied().unwrap_or(0),
                    instance_id: p.instance_id,
                    domain: p.domain,
                    name: p.name,
                    available: p.available,
                }),
                _ => {}
            }
        }
        Ok(ProviderLists {
            music,
            players,
            ui_url: base.map(|b| format!("{b}/#/settings/providers")),
        })
    }

    fn list_for(&self, caller: &Access) -> SpeakerList {
        let conn = lock(&self.conn).clone();
        let speakers = match conn.list.state {
            SpeakersState::Unavailable => Vec::new(),
            _ => lock(&self.hub)
                .speakers()
                .into_iter()
                .filter(|s| s.relay.as_ref().is_none_or(|r| r.user == caller.user))
                .collect(),
        };
        SpeakerList {
            speakers,
            state: Some(conn.list.state),
            since: conn.list.since,
        }
    }

    fn speaker(&self, caller: &Access, id: &SpeakerId) -> Result<Arc<Speaker>, SpeakerError> {
        if self.relay.owner_of(id).is_some_and(|u| u != caller.user) {
            return Err(SpeakerError::NotFound);
        }
        if lock(&self.conn).list.state == SpeakersState::Unavailable {
            return Err(SpeakerError::Unavailable);
        }
        if let Some(speaker) = lock(&self.hub).speaker(id) {
            return Ok(speaker);
        }
        let server_id = lock(&self.hub).server_id.clone();
        match (self.relay.live(id), server_id) {
            (Some(live), Some(_)) => Ok(Arc::new(convert::relay_placeholder(
                id.clone(),
                &live.name,
                &live.tag,
            ))),
            _ => Err(SpeakerError::NotFound),
        }
    }
}

/// A disconnect as the dashboard shows it.
fn describe(error: Option<&ConnectError>) -> (ConnectionState, Option<String>) {
    match error {
        None => (ConnectionState::Connecting, None),
        Some(ConnectError::SetupRequired) => (
            ConnectionState::SetupRequired,
            Some(String::from(
                "Music Assistant has no users yet; finish its setup first.",
            )),
        ),
        Some(ConnectError::Auth(e)) => (
            ConnectionState::AuthFailed,
            Some(format!(
                "Music Assistant refused the token (error {}: {}).",
                e.code,
                e.details.as_deref().unwrap_or("no details")
            )),
        ),
        Some(ConnectError::Incompatible(e)) => (ConnectionState::Incompatible, Some(e.to_string())),
        Some(ConnectError::NoToken) => (
            ConnectionState::AuthFailed,
            Some(String::from("No Music Assistant token is set.")),
        ),
        Some(other) => (
            ConnectionState::Unreachable,
            Some(format!("Could not reach Music Assistant: {other}.")),
        ),
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

impl CatalogControl for Inner {
    fn services<'a>(&'a self, _caller: &'a Access) -> BoxFuture<'a, ServiceList> {
        Box::pin(self.service_list())
    }

    fn search<'a>(
        &'a self,
        caller: &'a Access,
        query: CatalogSearch,
    ) -> BoxFuture<'a, Result<CatalogResults, CatalogError>> {
        Box::pin(self.catalog_search(caller, query))
    }

    fn item<'a>(
        &'a self,
        _caller: &'a Access,
        reference: &'a str,
        page: CatalogPage,
    ) -> BoxFuture<'a, Result<CatalogDetail, CatalogError>> {
        Box::pin(self.catalog_item(reference, page))
    }

    fn library<'a>(
        &'a self,
        _caller: &'a Access,
        service: &'a ServiceId,
        kind: CatalogKind,
        page: CatalogPage,
    ) -> BoxFuture<'a, Result<LibraryPage, CatalogError>> {
        Box::pin(self.catalog_library(service, kind, page))
    }

    fn art_source(&self, id: &CatalogArtId) -> Option<String> {
        self.catalog_art_source(id)
    }

    fn fetch_art<'a>(
        &'a self,
        id: &'a CatalogArtId,
    ) -> BoxFuture<'a, Result<Vec<u8>, CatalogError>> {
        Box::pin(self.catalog_fetch_art(id))
    }
}

impl AssistantControl for Assistant {
    fn enabled(&self) -> bool {
        self.inner.effective().active()
    }

    fn relay_supported(&self) -> bool {
        self.inner.relay_supported()
    }

    fn speakers<'a>(&'a self, caller: &'a Access) -> BoxFuture<'a, SpeakerList> {
        Box::pin(async move { self.inner.list_for(caller) })
    }

    fn speaker<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a SpeakerId,
    ) -> BoxFuture<'a, Result<Arc<Speaker>, SpeakerError>> {
        Box::pin(async move { self.inner.speaker(caller, id) })
    }

    fn queue<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a SpeakerId,
        page: QueuePage,
    ) -> BoxFuture<'a, Result<SpeakerQueue, SpeakerError>> {
        Box::pin(async move { self.inner.queue(caller, id, page).await })
    }

    fn command<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a SpeakerId,
        command: SpeakerCommand,
    ) -> BoxFuture<'a, Result<CommandAccepted, SpeakerError>> {
        Box::pin(async move { self.inner.command(caller, id, command).await })
    }

    fn status(&self) -> BoxFuture<'_, AssistantStatus> {
        Box::pin(async move { self.inner.status() })
    }

    fn update(
        &self,
        change: AssistantChange,
    ) -> BoxFuture<'_, Result<AssistantStatus, AssistantError>> {
        Box::pin(async move { self.inner.update(change).await })
    }

    fn remove(&self) -> BoxFuture<'_, Result<(), AssistantError>> {
        Box::pin(async move { self.inner.remove().await })
    }

    fn test(&self, url: Option<String>, token: Option<Secret>) -> BoxFuture<'_, AssistantTest> {
        Box::pin(async move { self.inner.test(url, token).await })
    }

    fn registration(&self) -> BoxFuture<'_, RegistrationView> {
        Box::pin(async move { self.inner.registration_view().await })
    }

    fn register(&self) -> BoxFuture<'_, Result<RegistrationView, AssistantError>> {
        Box::pin(async move { self.inner.register_now().await })
    }

    fn unregister(&self) -> BoxFuture<'_, Result<(), AssistantError>> {
        Box::pin(async move { self.inner.unregister().await })
    }

    fn providers(&self) -> BoxFuture<'_, Result<ProviderLists, AssistantError>> {
        Box::pin(async move { self.inner.providers().await })
    }

    fn overview(&self) -> BoxFuture<'_, AssistantOverview> {
        Box::pin(async move {
            let status = self.inner.status();
            let registration = status.configured.then(|| lock(&self.inner.reg).state);
            AssistantOverview {
                state: status.state,
                registration,
                relays: status.configured.then_some(status.relays_active),
                services: status.services,
                speakers: status.speakers,
            }
        })
    }

    fn catalog(&self) -> Option<&dyn CatalogControl> {
        Some(&*self.inner)
    }

    fn relay(&self) -> Option<&dyn RelayControl> {
        Some(self)
    }
}

impl RelayControl for Assistant {
    fn supported(&self) -> bool {
        self.inner.relay_supported()
    }

    fn cellular(&self) -> bool {
        self.inner.relay.cellular()
    }

    fn open<'a>(
        &'a self,
        caller: &'a RelayCaller,
        open: RelayOpen,
    ) -> BoxFuture<'a, Result<RelayOpened, RelayError>> {
        Box::pin(async move { self.inner.open_relay(caller, open).await })
    }

    fn sessions(&self, user: UserKey) -> Vec<RelayView> {
        self.inner.relay.sessions(user)
    }

    fn session(&self, user: UserKey, id: &RelaySessionId) -> Option<RelayView> {
        self.inner.relay.session(user, id)
    }

    fn set_tier(
        &self,
        caller: &RelayCaller,
        id: &RelaySessionId,
        tier: RelayTier,
    ) -> Result<RelayView, RelayError> {
        self.inner.relay.set_tier(caller, id, tier)
    }

    fn close<'a>(
        &'a self,
        user: UserKey,
        id: &'a RelaySessionId,
    ) -> BoxFuture<'a, Result<(), RelayError>> {
        Box::pin(async move { self.inner.relay.close(user, id).await })
    }

    fn fetch<'a>(
        &'a self,
        device: DeviceKey,
        id: &'a RelaySessionId,
        generation: u32,
        part: RelayPart,
        query: &'a str,
    ) -> BoxFuture<'a, Result<RelayBody, RelayError>> {
        Box::pin(async move {
            self.inner
                .relay
                .fetch(device, id, generation, part, query)
                .await
        })
    }
}
