//! The phone playback relay (contract section 3): one [`RelayManager`] per Music Assistant
//! connection, sessions bound to their user and app device, relay players shown as
//! `kind: phone` speakers to their owner only, and the SSE `relay` event.
//!
//! The manager is replaced when the address or token changes, and its sessions end when Music
//! Assistant goes away for good (`assistant_unavailable`), the server stops
//! (`server_restart`) or the device is revoked (`revoked`). Player keys are sealed per device
//! ([`SealedIdentities`]) and deleted with the device.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use tokio::time::Instant;
use trackside_core::assistant::{SpeakerEvent, SpeakerRelay};
use trackside_core::relay::{
    RelayBody, RelayCaller, RelayError, RelayFormat, RelayOpen, RelayOpened, RelayPart, RelayState,
    RelayTier, RelayView,
};
use trackside_core::{DeviceKey, Millis, RelaySessionId, SpeakerId, UserKey, random_id};
use trackside_relay::{
    ControlFuture, EndReason, FetchError, OpenRequest, PlayerControl, RelayConfig, RelayManager,
    RelaySession, SessionInfo, SessionState, Tier,
};

use crate::identities::SealedIdentities;
use crate::{Inner, lock};

/// How long an ended session can still be read with `GET /relay/sessions/{id}`.
const ENDED_KEPT: Duration = Duration::from_mins(10);

/// Waits and limits of the relay a test may shorten.
#[derive(Clone, Debug)]
pub struct RelayTuning {
    /// The Sendspin server, instead of `/sendspin` on Music Assistant's web port. For tests
    /// and unusual proxies; the Music Assistant token is sent either way.
    pub sendspin_url: Option<String>,
    /// Playing with no fetch for this long pauses Music Assistant.
    pub fetch_timeout: Duration,
    /// Paused or ready for this long ends the session.
    pub paused_timeout: Duration,
    /// Music Assistant has this long to activate the player.
    pub activation_timeout: Duration,
    /// A new epoch's playlist and init segment wait this long for audio.
    pub first_segment_wait: Duration,
    /// A segment just past the live edge waits this long.
    pub segment_wait: Duration,
    /// A command for a phone that is still joining waits this long for Music Assistant to
    /// list its player.
    pub player_wait: Duration,
}

impl Default for RelayTuning {
    fn default() -> Self {
        Self {
            sendspin_url: None,
            fetch_timeout: Duration::from_secs(30),
            paused_timeout: Duration::from_mins(10),
            activation_timeout: Duration::from_secs(30),
            first_segment_wait: Duration::from_secs(8),
            segment_wait: Duration::from_secs(3),
            player_wait: Duration::from_secs(10),
        }
    }
}

/// What is known of a session besides its audio.
#[derive(Clone, Debug)]
struct Meta {
    id: RelaySessionId,
    user: UserKey,
    device: DeviceKey,
    device_id: trackside_core::DeviceId,
    speaker: SpeakerId,
    player: String,
    name: String,
    created_at: Millis,
    latency_ms: u32,
    idle_timeout_s: u64,
    fetch_timeout_s: u64,
    session: Arc<RelaySession>,
    ended_at: Option<Instant>,
}

impl Meta {
    fn view(&self, info: &SessionInfo) -> RelayView {
        RelayView {
            id: self.id.clone(),
            user: self.user,
            device: self.device,
            device_id: self.device_id.clone(),
            speaker_id: self.speaker.clone(),
            created_at: self.created_at,
            state: match info.state {
                SessionState::Starting => RelayState::Starting,
                SessionState::Ready => RelayState::Ready,
                SessionState::Playing => RelayState::Playing,
                SessionState::Paused => RelayState::Paused,
                SessionState::Ended => RelayState::Ended,
            },
            ended_reason: info.ended_reason.map(EndReason::as_str),
            detail: info.detail.clone(),
            generation: info.generation,
            tier: tier_out(info.tier),
            latency_ms: self.latency_ms,
            idle_timeout_s: self.idle_timeout_s,
            fetch_timeout_s: self.fetch_timeout_s,
            hls: info.hls.clone(),
            format: RelayFormat {
                codec: info.format.codec,
                container: info.format.container,
                sample_rate: info.format.sample_rate,
                channels: info.format.channels,
                bit_depth: info.format.bit_depth,
                bitrate_kbps: info.format.bitrate_kbps,
            },
        }
    }

    fn tag(&self) -> SpeakerRelay {
        SpeakerRelay {
            session: self.id.clone(),
            user: self.user,
            device: self.device,
            device_id: self.device_id.clone(),
        }
    }
}

fn tier_in(tier: RelayTier) -> Tier {
    match tier {
        RelayTier::Lossless => Tier::Lossless,
        RelayTier::Cellular => Tier::Cellular,
    }
}

fn tier_out(tier: Tier) -> RelayTier {
    match tier {
        Tier::Lossless => RelayTier::Lossless,
        Tier::Cellular => RelayTier::Cellular,
    }
}

/// A relay speaker that has a live session but may not be in Music Assistant's list yet.
#[derive(Clone, Debug)]
pub(crate) struct LiveRelay {
    pub player: String,
    pub name: String,
    pub tag: SpeakerRelay,
}

/// Pauses and resumes a relay player's queue through the Music Assistant client.
struct Control(Weak<Inner>);

impl Control {
    fn queue<'a>(&'a self, player_id: &'a str, play: bool) -> ControlFuture<'a> {
        Box::pin(async move {
            let inner = self.0.upgrade().ok_or("shutting down")?;
            let client = inner.connected().map_err(|e| e.to_string())?;
            let sent = if play {
                client.queue_play(player_id).await
            } else {
                client.queue_pause(player_id).await
            };
            sent.map_err(|e| e.to_string())
        })
    }
}

impl PlayerControl for Control {
    fn pause<'a>(&'a self, player_id: &'a str) -> ControlFuture<'a> {
        self.queue(player_id, false)
    }

    fn resume<'a>(&'a self, player_id: &'a str) -> ControlFuture<'a> {
        self.queue(player_id, true)
    }
}

/// The relay side of the integration.
pub(crate) struct Relays {
    pub identities: Arc<SealedIdentities>,
    manager: RwLock<Option<Arc<RelayManager>>>,
    sessions: Mutex<HashMap<RelaySessionId, Meta>>,
    /// Every relay speaker seen this run and whose it is, so its events reach only them.
    owners: Mutex<HashMap<SpeakerId, UserKey>>,
    ffmpeg: Option<PathBuf>,
    tuning: RelayTuning,
}

impl std::fmt::Debug for Relays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relays").finish_non_exhaustive()
    }
}

impl Relays {
    pub fn new(
        identities: Arc<SealedIdentities>,
        ffmpeg: Option<PathBuf>,
        tuning: RelayTuning,
    ) -> Self {
        Self {
            identities,
            manager: RwLock::new(None),
            sessions: Mutex::new(HashMap::new()),
            owners: Mutex::new(HashMap::new()),
            ffmpeg,
            tuning,
        }
    }

    pub fn tuning(&self) -> &RelayTuning {
        &self.tuning
    }

    fn manager(&self) -> Option<Arc<RelayManager>> {
        self.manager
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn cellular(&self) -> bool {
        self.ffmpeg.is_some()
    }

    pub fn active(&self) -> u32 {
        self.manager()
            .map_or(0, |m| u32::try_from(m.active()).unwrap_or(u32::MAX))
    }

    pub fn owner_of(&self, speaker: &SpeakerId) -> Option<UserKey> {
        lock(&self.owners).get(speaker).copied()
    }

    /// The live session behind a relay speaker.
    pub fn live(&self, speaker: &SpeakerId) -> Option<LiveRelay> {
        lock(&self.sessions)
            .values()
            .find(|m| m.speaker == *speaker && !m.session.is_ended())
            .map(|m| LiveRelay {
                player: m.player.clone(),
                name: m.name.clone(),
                tag: m.tag(),
            })
    }

    pub fn set_limits(&self, max_sessions: u32, max_per_user: u32) {
        if let Some(manager) = self.manager() {
            manager.set_limits(max_sessions as usize, max_per_user as usize);
        }
    }

    /// Replaces the manager, returning the old one for the caller to shut down.
    pub fn replace(&self, fresh: Option<Arc<RelayManager>>) -> Option<Arc<RelayManager>> {
        std::mem::replace(
            &mut *self
                .manager
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            fresh,
        )
    }

    /// The relay manager for a Music Assistant at `ws_url` (its `/ws` API address).
    pub fn build(
        &self,
        inner: &Arc<Inner>,
        ws_url: &str,
        token: &str,
        max_sessions: u32,
        max_per_user: u32,
    ) -> Arc<RelayManager> {
        let url = self.tuning.sendspin_url.clone().unwrap_or_else(|| {
            format!("{}/sendspin", ws_url.strip_suffix("/ws").unwrap_or(ws_url))
        });
        let mut config = RelayConfig::new(url, Some(trackside_sendspin::Token::new(token)));
        config.ffmpeg.clone_from(&self.ffmpeg);
        config.fetch_timeout = self.tuning.fetch_timeout;
        config.paused_timeout = self.tuning.paused_timeout;
        config.activation_timeout = self.tuning.activation_timeout;
        config.first_segment_wait = self.tuning.first_segment_wait;
        config.segment_wait = self.tuning.segment_wait;
        config.max_sessions = (max_sessions as usize).max(1);
        config.max_per_user = (max_per_user as usize).max(1);
        config.path_prefix = String::from("/relay");
        config.backoff = trackside_sendspin::Backoff {
            initial: inner.tuning.retry.min(Duration::from_secs(1)),
            max: inner.tuning.retry.max(Duration::from_secs(1)) * 12,
        };
        Arc::new(RelayManager::new(
            config,
            Arc::clone(&self.identities) as Arc<dyn trackside_sendspin::IdentityStore>,
            Arc::new(Control(Arc::downgrade(inner))),
            inner.shutdown.child_token(),
        ))
    }

    fn meta(&self, id: &RelaySessionId) -> Option<Meta> {
        lock(&self.sessions).get(id).cloned()
    }

    fn prune(&self) {
        let now = Instant::now();
        lock(&self.sessions).retain(|_, m| m.ended_at.is_none_or(|at| now < at + ENDED_KEPT));
    }

    pub fn sessions(&self, user: UserKey) -> Vec<RelayView> {
        self.prune();
        let mut views: Vec<RelayView> = lock(&self.sessions)
            .values()
            .filter(|m| m.user == user && !m.session.is_ended())
            .map(|m| m.view(&m.session.info()))
            .collect();
        views.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        views
    }

    pub fn session(&self, user: UserKey, id: &RelaySessionId) -> Option<RelayView> {
        self.meta(id)
            .filter(|m| m.user == user)
            .map(|m| m.view(&m.session.info()))
    }

    pub fn set_tier(
        &self,
        caller: &RelayCaller,
        id: &RelaySessionId,
        tier: RelayTier,
    ) -> Result<RelayView, RelayError> {
        let meta = self
            .meta(id)
            .filter(|m| m.user == caller.user)
            .ok_or(RelayError::NotFound)?;
        if meta.device != caller.device {
            return Err(RelayError::NotFound);
        }
        if meta.session.is_ended() {
            return Err(RelayError::Ended);
        }
        if tier == RelayTier::Cellular && !self.cellular() {
            return Err(RelayError::Unsupported("relay_cellular"));
        }
        meta.session.set_tier(tier_in(tier));
        Ok(meta.view(&meta.session.info()))
    }

    pub async fn close(&self, user: UserKey, id: &RelaySessionId) -> Result<(), RelayError> {
        let meta = self
            .meta(id)
            .filter(|m| m.user == user)
            .ok_or(RelayError::NotFound)?;
        meta.session.close(EndReason::Closed).await;
        Ok(())
    }

    /// Ends every session matching `which`.
    async fn close_where(&self, which: impl Fn(&Meta) -> bool, reason: EndReason) {
        let sessions: Vec<Arc<RelaySession>> = lock(&self.sessions)
            .values()
            .filter(|m| !m.session.is_ended() && which(m))
            .map(|m| Arc::clone(&m.session))
            .collect();
        for session in sessions {
            session.close(reason).await;
        }
    }

    pub async fn close_device(&self, device: DeviceKey, reason: EndReason) {
        self.close_where(|m| m.device == device, reason).await;
    }

    pub async fn close_user(&self, user: UserKey, reason: EndReason) {
        self.close_where(|m| m.user == user, reason).await;
    }

    pub async fn fetch(
        &self,
        device: DeviceKey,
        id: &RelaySessionId,
        generation: u32,
        part: RelayPart,
        query: &str,
    ) -> Result<RelayBody, RelayError> {
        let meta = self
            .meta(id)
            .filter(|m| m.device == device)
            .ok_or(RelayError::NotFound)?;
        let session = meta.session;
        let query = (!query.is_empty()).then_some(query);
        let found = match part {
            RelayPart::Master => session
                .master_playlist(generation, query)
                .await
                .map(RelayBody::Playlist),
            RelayPart::Media => session
                .media_playlist(generation, query)
                .await
                .map(RelayBody::Playlist),
            RelayPart::Init => session.init_segment(generation).await.map(RelayBody::Media),
            RelayPart::Segment(n) => session.segment(generation, n).await.map(RelayBody::Media),
        };
        found.map_err(|FetchError::NotFound| RelayError::NotFound)
    }
}

impl Inner {
    /// Whether Music Assistant is set up and its Sendspin server is loaded.
    pub(crate) fn relay_supported(&self) -> bool {
        self.effective().active()
            && lock(&self.hub)
                .providers
                .iter()
                .any(|p| p.domain == "sendspin" && p.available)
    }

    /// Wraps events about relay speakers so only their owner hears them.
    pub(crate) fn owned(&self, event: SpeakerEvent) -> SpeakerEvent {
        let id = match &event {
            SpeakerEvent::Added(s) | SpeakerEvent::Updated(s) => Some(&s.id),
            SpeakerEvent::Removed(id)
            | SpeakerEvent::Progress { id, .. }
            | SpeakerEvent::Queue { id, .. } => Some(id),
            _ => None,
        };
        match id.and_then(|id| self.relay.owner_of(id)) {
            Some(user) => SpeakerEvent::Owned {
                user,
                event: Box::new(event),
            },
            None => event,
        }
    }

    /// Recomputes the speakers now (a relay started or ended) and announces the changes.
    pub(crate) fn refresh_speakers(&self) {
        let now = self.now();
        let events = lock(&self.hub).refresh(now, Instant::now());
        self.publish(events);
    }

    /// Ends every relay session in the background, keeping the manager.
    pub(crate) fn end_relays(self: &Arc<Self>, reason: EndReason) {
        if let Some(manager) = self.relay.manager() {
            self.tasks
                .spawn(async move { manager.shutdown(reason).await });
        }
    }

    /// Ends every relay session and waits for each goodbye to Music Assistant.
    pub(crate) async fn shutdown_relays(&self) {
        if let Some(manager) = self.relay.replace(None) {
            manager.shutdown(EndReason::ServerRestart).await;
        }
    }

    /// Replaces the relay manager to match the connection settings.
    pub(crate) fn rebuild_relay(self: &Arc<Self>, connection: Option<(String, String)>) {
        let effective = self.effective();
        let per_user = self.config.music_assistant.relay_max_per_user;
        let fresh = connection.map(|(ws_url, token)| {
            self.relay.build(
                self,
                &ws_url,
                &token,
                effective.relay_max_sessions,
                per_user,
            )
        });
        if let Some(old) = self.relay.replace(fresh) {
            self.tasks
                .spawn(async move { old.shutdown(EndReason::AssistantUnavailable).await });
        }
    }

    pub(crate) async fn open_relay(
        self: &Arc<Self>,
        caller: &RelayCaller,
        open: RelayOpen,
    ) -> Result<RelayOpened, RelayError> {
        self.connected().map_err(|_| RelayError::Unavailable)?;
        if !self.relay_supported() {
            return Err(RelayError::Unsupported("relay"));
        }
        if open.tier == RelayTier::Cellular && !self.relay.cellular() {
            return Err(RelayError::Unsupported("relay_cellular"));
        }
        let manager = self.relay.manager().ok_or(RelayError::Unavailable)?;
        let server_id = lock(&self.hub)
            .server_id
            .clone()
            .ok_or(RelayError::Unavailable)?;
        let id: RelaySessionId = random_id();
        let opened = manager
            .open(OpenRequest {
                id: id.to_string(),
                user: caller.user.0.to_string(),
                device: caller.device_id.to_string(),
                name: caller.name.clone(),
                tier: tier_in(open.tier),
            })
            .await
            .map_err(|e| match e {
                trackside_relay::RelayError::Limit { limit } => RelayError::Limit(limit),
                trackside_relay::RelayError::Unsupported(_) => {
                    RelayError::Unsupported("relay_cellular")
                }
                other => {
                    tracing::warn!(error = %other, "cannot open a relay session");
                    RelayError::Failed(String::from("The relay could not start."))
                }
            })?;
        let session = opened.session;
        let session_id = trackside_core::PublicId::parse(session.id())
            .map_err(|_| RelayError::Failed(String::from("The relay could not start.")))?;
        if !opened.created {
            let meta = self.relay.meta(&session_id).ok_or(RelayError::NotFound)?;
            return Ok(RelayOpened {
                session: meta.view(&session.info()),
                created: false,
            });
        }
        let player = session.player_id().to_owned();
        let speaker = SpeakerId::derive(&server_id, &player);
        let config = manager.config();
        let meta = Meta {
            id: session_id.clone(),
            user: caller.user,
            device: caller.device,
            device_id: caller.device_id.clone(),
            speaker: speaker.clone(),
            player: player.clone(),
            name: caller.name.clone(),
            created_at: self.now(),
            latency_ms: manager.latency_ms(),
            idle_timeout_s: config.paused_timeout.as_secs(),
            fetch_timeout_s: config.fetch_timeout.as_secs(),
            session: Arc::clone(&session),
            ended_at: None,
        };
        lock(&self.relay.owners).insert(speaker, caller.user);
        lock(&self.relay.sessions).insert(session_id.clone(), meta.clone());
        {
            let mut hub = lock(&self.hub);
            hub.relay_players.insert(player.clone());
            hub.relays.insert(player, meta.tag());
        }
        self.refresh_speakers();
        let me = Arc::clone(self);
        let watched = meta.clone();
        self.tasks
            .spawn(async move { me.follow_relay(watched).await });
        Ok(RelayOpened {
            session: meta.view(&session.info()),
            created: true,
        })
    }

    /// Announces every change of a session to its owner until it ends, then takes its
    /// speaker away.
    ///
    /// While playing, the session holds one of its user's stream slots (contract 3.7). With
    /// none free it asks Music Assistant to pause, as a stream request would get `429`.
    async fn follow_relay(self: Arc<Self>, meta: Meta) {
        let mut changes = meta.session.subscribe();
        let mut slot = None;
        loop {
            let info = changes.borrow_and_update().clone();
            if info.state == SessionState::Playing {
                if slot.is_none() {
                    slot = self.auth.user_stream_slot(meta.user);
                    if slot.is_none() {
                        tracing::info!(
                            "relay over the user's stream limit, pausing its player"
                        );
                        self.pause_relay(&meta.player).await;
                    }
                }
            } else {
                slot = None;
            }
            self.publish(vec![SpeakerEvent::Relay(Box::new(meta.view(&info)))]);
            if info.state == SessionState::Ended {
                break;
            }
            if changes.changed().await.is_err() {
                break;
            }
        }
        drop(slot);
        if let Some(stored) = lock(&self.relay.sessions).get_mut(&meta.id) {
            stored.ended_at = Some(Instant::now());
        }
        let mut hub = lock(&self.hub);
        if hub
            .relays
            .get(&meta.player)
            .is_some_and(|r| r.session == meta.id)
        {
            hub.relays.remove(&meta.player);
        }
        drop(hub);
        self.refresh_speakers();
    }

    async fn pause_relay(&self, player: &str) {
        let paused = match self.connected() {
            Ok(client) => client.queue_pause(player).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(error) = paused {
            tracing::warn!(%error, "relay could not pause the player");
        }
    }

    /// Ends the relays of revoked credentials and deletes the keys of devices that are gone.
    pub(crate) async fn on_revoked(self: &Arc<Self>, user: UserKey, device: Option<DeviceKey>) {
        match device {
            Some(device) => self.relay.close_device(device, EndReason::Revoked).await,
            None => self.relay.close_user(user, EndReason::Revoked).await,
        }
        self.sweep_identities().await;
    }

    pub(crate) async fn sweep_identities(&self) {
        match self.relay.identities.sweep().await {
            Ok(gone) if !gone.is_empty() => {
                let mut hub = lock(&self.hub);
                for player in &gone {
                    hub.relay_players.remove(player);
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "cannot sweep the relay player keys"),
        }
    }

    /// Waits until Music Assistant lists `player`, or `deadline` passes.
    pub(crate) async fn wait_for_player(&self, player: &str, deadline: Instant) -> bool {
        loop {
            let changed = self.hub_changed.notified();
            if lock(&self.hub).players.contains_key(player) {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return lock(&self.hub).players.contains_key(player);
            }
        }
    }
}
