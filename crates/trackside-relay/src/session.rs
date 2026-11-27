//! One relay session: a Sendspin player for one phone, its epochs, its encoder and the
//! readers. The Sendspin handler only appends under a short lock and wakes the session task,
//! which publishes segments on time, feeds the encoder, enforces the limits and tears down an
//! idle session. Readers wait on a `watch` and never touch the socket.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use trackside_sendspin::{self as sendspin, Clock, Event, GoodbyeReason};

use crate::encoder::Encoder;
use crate::epoch::{Epoch, Shape, Timeline};
use crate::flac::{self, StreamInfo};
use crate::{ControlFuture, EndReason, PlayerControl, SessionInfo, SessionState, Tier};

/// The encoder may sit on fed audio this long before the tier falls back.
const MAX_ENCODER_LAG_US: i64 = 3_000_000;

/// The parts of the relay configuration a session needs.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub shape: Shape,
    pub pending_cap: usize,
    pub published_cap: usize,
    pub fetch_timeout: Duration,
    pub paused_timeout: Duration,
    pub activation_timeout: Duration,
    pub first_segment_wait: Duration,
    pub segment_wait: Duration,
    pub cellular_kbps: u32,
    pub ffmpeg: Option<PathBuf>,
    pub path_prefix: String,
}

/// Why a playlist or segment cannot be served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// No such epoch or segment, or it left the window.
    #[error("not found")]
    NotFound,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of the idle rules"
)]
struct Inner {
    state: SessionState,
    ended: Option<(EndReason, Option<String>)>,
    tier: Tier,
    stream: Option<StreamInfo>,
    current: Option<Epoch>,
    previous: Option<Epoch>,
    generation: u32,
    encoder: Option<Encoder>,
    encoder_failed: Option<u32>,
    last_fetch: Instant,
    since: Instant,
    activated: bool,
    player_active: bool,
    pause_requested: bool,
    resume_due: bool,
    violations: u64,
}

struct Shared {
    id: String,
    player_id: String,
    settings: Settings,
    clock: Clock,
    inner: Mutex<Inner>,
    /// Bumped whenever readers may find something new.
    changed: watch::Sender<u64>,
    info: watch::Sender<SessionInfo>,
    wake: Notify,
}

struct ClockTimeline<'a>(&'a Clock);

impl Timeline for ClockTimeline<'_> {
    fn to_local(&self, server_us: i64) -> Option<i64> {
        self.0.to_local_us(server_us)
    }

    fn now_local(&self) -> i64 {
        self.0.now_us()
    }

    fn wall_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn info(&self, inner: &Inner) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            player_id: self.player_id.clone(),
            state: inner.state,
            ended_reason: inner.ended.as_ref().map(|(r, _)| *r),
            detail: inner.ended.as_ref().and_then(|(_, d)| d.clone()),
            generation: inner.generation,
            tier: inner.tier,
            hls: Some(format!(
                "{}/{}/{}/master.m3u8",
                self.settings.path_prefix,
                self.id,
                Self::playable_generation(inner)
            )),
        }
    }

    /// Where the audio is: the live epoch, or the next one when none is live, whose URL works
    /// before it starts (fetches wait for it).
    fn playable_generation(inner: &Inner) -> u32 {
        if inner.current.as_ref().is_some_and(|e| !e.ended) {
            inner.generation
        } else {
            inner.generation + 1
        }
    }

    /// Publishes the session's info if it changed, and tells readers to look again.
    fn changed(&self, inner: &Inner) {
        let info = self.info(inner);
        self.info.send_if_modified(|old| {
            let differs = *old != info;
            if differs {
                *old = info;
            }
            differs
        });
        self.changed.send_modify(|n| *n += 1);
    }

    fn set_state(inner: &mut Inner, state: SessionState) {
        if inner.state != state {
            inner.state = state;
            inner.since = Instant::now();
        }
    }

    fn end_epoch(inner: &mut Inner) {
        if let Some(epoch) = inner.current.as_mut() {
            epoch.end();
        }
        inner.encoder = None;
    }

    /// Ends the current epoch and starts the next, with a fresh playlist URL.
    fn start_epoch(self: &Arc<Self>, inner: &mut Inner) {
        let Some(info) = inner.stream.clone() else {
            return;
        };
        Self::end_epoch(inner);
        inner.previous = inner.current.take();
        inner.generation += 1;
        let generation = inner.generation;
        if inner.tier == Tier::Cellular {
            match self.spawn_encoder(generation, &info) {
                Ok(encoder) => inner.encoder = Some(encoder),
                Err(error) => {
                    tracing::warn!(%error, "relay encoder unavailable, using lossless");
                    inner.tier = Tier::Lossless;
                }
            }
        }
        inner.current = Some(Epoch::new(
            generation,
            inner.tier,
            info,
            self.settings.shape,
            self.settings.cellular_kbps * 1000,
        ));
    }

    fn spawn_encoder(
        self: &Arc<Self>,
        generation: u32,
        info: &StreamInfo,
    ) -> Result<Encoder, String> {
        let ffmpeg = self
            .settings
            .ffmpeg
            .as_deref()
            .ok_or_else(|| "no ffmpeg".to_owned())?;
        let frames: Weak<Self> = Arc::downgrade(self);
        let exits = Arc::downgrade(self);
        Encoder::spawn(
            ffmpeg,
            self.settings.cellular_kbps,
            info,
            move |config, frame| {
                if let Some(shared) = frames.upgrade() {
                    let mut inner = shared.lock();
                    if let Some(epoch) = inner.current.as_mut().filter(|e| e.number == generation) {
                        epoch.push_aac(config, frame);
                        drop(inner);
                        shared.wake.notify_one();
                    }
                }
            },
            move |result| {
                if let Some(shared) = exits.upgrade() {
                    let mut inner = shared.lock();
                    if inner
                        .current
                        .as_ref()
                        .is_some_and(|e| e.number == generation && !e.ended)
                    {
                        tracing::warn!(?result, "relay encoder stopped");
                        inner.encoder_failed = Some(generation);
                        drop(inner);
                        shared.wake.notify_one();
                    }
                }
            },
        )
        .map_err(|e| e.to_string())
    }

    fn end_session(inner: &mut Inner, reason: EndReason, detail: Option<String>) {
        if inner.ended.is_none() {
            inner.ended = Some((reason, detail));
        }
        Self::end_epoch(inner);
        if let Some(previous) = inner.previous.as_mut() {
            previous.end();
        }
        Self::set_state(inner, SessionState::Ended);
    }

    fn on_event(self: &Arc<Self>, event: Event) {
        let mut inner = self.lock();
        if inner.ended.is_some() {
            return;
        }
        match event {
            Event::Activated { player, .. } => {
                inner.activated |= player;
                inner.player_active = player;
                if player && inner.state == SessionState::Starting {
                    Self::set_state(&mut inner, SessionState::Ready);
                }
            }
            Event::StreamStart { format, in_place } => {
                let info = (format.codec == "flac")
                    .then(|| format.header_bytes())
                    .flatten()
                    .and_then(|h| StreamInfo::from_codec_header(&h).ok());
                match info {
                    None => {
                        tracing::warn!(codec = %format.codec, "relay got a stream it cannot carry");
                        inner.violations += 1;
                        inner.stream = None;
                        Self::end_epoch(&mut inner);
                    }
                    Some(info) => {
                        let same = in_place
                            && inner.stream.as_ref() == Some(&info)
                            && inner.current.as_ref().is_some_and(|e| !e.ended);
                        if !same {
                            inner.stream = Some(info);
                            self.start_epoch(&mut inner);
                        }
                    }
                }
                Self::set_state(&mut inner, SessionState::Playing);
                inner.pause_requested = false;
                inner.last_fetch = inner.last_fetch.max(Instant::now());
            }
            Event::Audio(chunk) => {
                let cap = self.settings.pending_cap;
                if let Some(epoch) = inner.current.as_mut().filter(|e| !e.ended) {
                    match flac::split_frames(&chunk.data) {
                        Ok(frames) => {
                            epoch.push_flac(chunk.timestamp, frames);
                            let dropped = epoch.enforce_pending_cap(cap);
                            if dropped > 0 {
                                inner.violations += 1;
                                tracing::warn!(dropped, "relay pending audio over its cap");
                            }
                        }
                        Err(error) => {
                            inner.violations += 1;
                            tracing::warn!(%error, "relay dropped an unreadable chunk");
                        }
                    }
                }
            }
            Event::Clear {
                from_timestamp: None,
            } => {
                if inner.state == SessionState::Playing {
                    self.start_epoch(&mut inner);
                }
            }
            Event::Clear {
                from_timestamp: Some(from),
            } => {
                if let Some(epoch) = inner.current.as_mut() {
                    epoch.clear_from(from);
                }
            }
            Event::End => {
                Self::end_epoch(&mut inner);
                Self::set_state(&mut inner, SessionState::Paused);
            }
            Event::Disconnected { .. } => {
                inner.player_active = false;
                if inner.state == SessionState::Playing {
                    inner.resume_due = true;
                    Self::end_epoch(&mut inner);
                    Self::set_state(&mut inner, SessionState::Paused);
                }
            }
            Event::Stopped { error } => {
                let reason = match &error {
                    Some(sendspin::Error::Auth(_) | sendspin::Error::Rejected(_)) => {
                        EndReason::Failed
                    }
                    _ => EndReason::AssistantUnavailable,
                };
                Self::end_session(&mut inner, reason, error.map(|e| e.to_string()));
            }
            _ => {}
        }
        self.changed(&inner);
        drop(inner);
        self.wake.notify_one();
    }
}

struct RelayHandler(Arc<Shared>);

impl sendspin::Handler for RelayHandler {
    fn handle(&self, event: Event) {
        self.0.on_event(event);
    }
}

/// What the session task does after a tick, outside the lock.
#[derive(Default)]
struct Actions {
    pause: bool,
    resume: bool,
    next: Option<Instant>,
    ended: bool,
}

/// A running relay session. Cheap to share; readers call the fetch methods.
#[derive(Debug)]
pub struct RelaySession {
    shared: Arc<Shared>,
    pub(crate) user: String,
    pub(crate) device: String,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl RelaySession {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        id: String,
        user: String,
        device: String,
        tier: Tier,
        settings: Settings,
        sendspin_config: sendspin::Config,
        control: Arc<dyn PlayerControl>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let player_id = sendspin_config.identity.client_id();
        let now = Instant::now();
        let inner = Inner {
            state: SessionState::Starting,
            ended: None,
            tier,
            stream: None,
            current: None,
            previous: None,
            generation: 0,
            encoder: None,
            encoder_failed: None,
            last_fetch: now,
            since: now,
            activated: false,
            player_active: false,
            pause_requested: false,
            resume_due: false,
            violations: 0,
        };
        let (changed, _) = watch::channel(0);
        let placeholder = SessionInfo {
            id: id.clone(),
            player_id: player_id.clone(),
            state: SessionState::Starting,
            ended_reason: None,
            detail: None,
            generation: 0,
            tier,
            hls: Some(format!("{}/{id}/1/master.m3u8", settings.path_prefix)),
        };
        let (info, _) = watch::channel(placeholder);
        let clock = Clock::new();
        let shared = Arc::new(Shared {
            id,
            player_id,
            settings,
            clock: clock.clone(),
            inner: Mutex::new(inner),
            changed,
            info,
            wake: Notify::new(),
        });
        let client = Arc::new(sendspin::Client::start_with_clock(
            sendspin_config,
            Arc::new(RelayHandler(Arc::clone(&shared))),
            clock,
            cancel.child_token(),
        ));
        let task = tokio::spawn(run(
            Arc::clone(&shared),
            Arc::clone(&client),
            control,
            cancel,
        ));
        Arc::new(Self {
            shared,
            user,
            device,
            task: Mutex::new(Some(task)),
        })
    }

    pub fn id(&self) -> &str {
        &self.shared.id
    }

    /// Music Assistant's id for this player (the Sendspin `client_id`).
    pub fn player_id(&self) -> &str {
        &self.shared.player_id
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn info(&self) -> SessionInfo {
        self.shared.info.borrow().clone()
    }

    /// Every change of state, generation or tier, for the SSE `relay` event.
    pub fn subscribe(&self) -> watch::Receiver<SessionInfo> {
        self.shared.info.subscribe()
    }

    pub fn is_ended(&self) -> bool {
        self.shared.lock().ended.is_some()
    }

    /// Times Music Assistant broke the stream's rules (over the buffer, unreadable chunks).
    pub fn violations(&self) -> u64 {
        self.shared.lock().violations
    }

    /// Bytes of audio held now, pending and published, across epochs.
    pub fn held_bytes(&self) -> usize {
        let inner = self.shared.lock();
        [inner.current.as_ref(), inner.previous.as_ref()]
            .into_iter()
            .flatten()
            .map(|e| e.pending_bytes() + e.published_bytes())
            .sum()
    }

    /// Counts as a fetch for the idle rules (the app is listening).
    pub fn touch(&self) {
        self.shared.lock().last_fetch = Instant::now();
    }

    /// Switches tier. While a stream runs this starts a new epoch.
    pub fn set_tier(&self, tier: Tier) {
        let mut inner = self.shared.lock();
        if inner.tier == tier || inner.ended.is_some() {
            return;
        }
        inner.tier = tier;
        if inner.current.as_ref().is_some_and(|e| !e.ended) {
            self.shared.start_epoch(&mut inner);
        }
        self.shared.changed(&inner);
    }

    /// Ends the session: goodbye to Music Assistant, playlists end.
    pub async fn close(&self, reason: EndReason) {
        {
            let mut inner = self.shared.lock();
            Shared::end_session(&mut inner, reason, None);
            self.shared.changed(&inner);
        }
        self.shared.wake.notify_one();
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    /// Waits until `ready` finds what it wants in the epoch, or `wait` passes.
    async fn wait_for<T>(
        &self,
        epoch: u32,
        wait: Duration,
        ready: impl Fn(&Epoch, bool) -> Option<Result<T, FetchError>>,
    ) -> Result<T, FetchError> {
        self.touch();
        let deadline = Instant::now() + wait;
        loop {
            let mut changes = self.shared.changed.subscribe();
            {
                let inner = self.shared.lock();
                let late = Instant::now() >= deadline;
                let found = [inner.current.as_ref(), inner.previous.as_ref()]
                    .into_iter()
                    .flatten()
                    .find(|e| e.number == epoch);
                match found {
                    Some(found) => {
                        if let Some(result) = ready(found, late) {
                            return result;
                        }
                    }
                    // The next epoch: wait for it to start.
                    None if epoch == inner.generation + 1 && inner.ended.is_none() && !late => {}
                    None => return Err(FetchError::NotFound),
                }
            }
            // On a timeout the next look has `late` set and answers.
            let _ = tokio::time::timeout_at(deadline, changes.changed()).await;
        }
    }

    /// The media playlist as it is now, without waiting or counting as a fetch.
    pub fn playlist_snapshot(&self, epoch: u32, query: Option<&str>) -> Result<String, FetchError> {
        let inner = self.shared.lock();
        [inner.current.as_ref(), inner.previous.as_ref()]
            .into_iter()
            .flatten()
            .find(|e| e.number == epoch)
            .map(|e| e.playlist(query))
            .ok_or(FetchError::NotFound)
    }

    /// The multivariant playlist of an epoch. `query` (the media token) is appended to URLs.
    pub async fn master_playlist(
        &self,
        epoch: u32,
        query: Option<&str>,
    ) -> Result<String, FetchError> {
        self.wait_for(epoch, self.shared.settings.first_segment_wait, |e, _| {
            Some(Ok(crate::hls::multivariant(
                e.codecs(),
                e.bandwidth(),
                query,
            )))
        })
        .await
    }

    /// The live media playlist. Waits for the first segment of a new epoch, then answers
    /// with whatever is there (possibly nothing yet, which players re-poll).
    pub async fn media_playlist(
        &self,
        epoch: u32,
        query: Option<&str>,
    ) -> Result<String, FetchError> {
        self.wait_for(epoch, self.shared.settings.first_segment_wait, |e, late| {
            (late || e.has_segments() || e.ended).then(|| Ok(e.playlist(query)))
        })
        .await
    }

    /// The init segment. For the cellular tier it exists once the encoder has produced audio.
    pub async fn init_segment(&self, epoch: u32) -> Result<Bytes, FetchError> {
        self.wait_for(
            epoch,
            self.shared.settings.first_segment_wait,
            |e, late| match e.init() {
                Some(init) => Some(Ok(init)),
                None if late || e.ended => Some(Err(FetchError::NotFound)),
                None => None,
            },
        )
        .await
    }

    /// A media segment. One just past the live edge is waited for; anything else missing is
    /// not found (it left the window, or it will not exist).
    pub async fn segment(&self, epoch: u32, sequence: u64) -> Result<Bytes, FetchError> {
        self.wait_for(epoch, self.shared.settings.segment_wait, |e, late| {
            if let Some(bytes) = e.segment(sequence) {
                return Some(Ok(bytes));
            }
            let waitable =
                !e.ended && sequence >= e.next_sequence() && sequence < e.next_sequence() + 2;
            (late || !waitable).then_some(Err(FetchError::NotFound))
        })
        .await
    }
}

fn tick(shared: &Arc<Shared>) -> Actions {
    let settings = &shared.settings;
    let mut inner = shared.lock();
    let mut actions = Actions::default();
    let now = Instant::now();
    let timeline = ClockTimeline(&shared.clock);

    if inner.ended.is_none() {
        let mut fall_back = inner.encoder_failed.is_some_and(|g| g == inner.generation);
        let due = inner
            .current
            .as_mut()
            .filter(|e| e.tier == Tier::Cellular && !e.ended)
            .map(|e| e.take_due_input(&timeline))
            .unwrap_or_default();
        if let Some(encoder) = inner.encoder.as_mut() {
            for (frame, samples) in due {
                if let Err(error) = encoder.feed(&frame, samples) {
                    tracing::warn!(%error, "relay encoder input failed");
                    fall_back = true;
                    break;
                }
            }
        }
        let now_local = shared.clock.now_us();
        if inner.current.as_mut().is_some_and(|e| {
            e.tier == Tier::Cellular && !e.ended && e.encoder_stalled(now_local, MAX_ENCODER_LAG_US)
        }) {
            fall_back = true;
        }
        if fall_back {
            tracing::warn!("relay cellular tier failed, falling back to lossless");
            inner.encoder_failed = None;
            inner.tier = Tier::Lossless;
            shared.start_epoch(&mut inner);
        }

        let published = inner
            .current
            .as_mut()
            .map_or(0, |e| e.publish_due(&timeline));
        let mut total: usize = [inner.current.as_ref(), inner.previous.as_ref()]
            .into_iter()
            .flatten()
            .map(Epoch::published_bytes)
            .sum();
        while total > settings.published_cap {
            let freed = match inner.previous.as_mut().map(Epoch::trim_oldest) {
                Some(n) if n > 0 => n,
                _ => inner.current.as_mut().map_or(0, Epoch::trim_oldest),
            };
            if freed == 0 {
                break;
            }
            total -= freed;
        }
        if published > 0 || fall_back {
            shared.changed(&inner);
        }

        idle_rules(shared, &mut inner, now, &mut actions);
        if inner.resume_due && inner.player_active && inner.state != SessionState::Playing {
            if shared.clock.is_synced() {
                inner.resume_due = false;
                actions.resume = true;
            }
        } else if inner.state == SessionState::Playing {
            inner.resume_due = false;
        }
        if let Some(deadline) = inner
            .current
            .as_ref()
            .and_then(|e| e.next_deadline(&timeline))
        {
            let at = shared.clock.instant_at(deadline);
            actions.next = Some(actions.next.map_or(at, |n| n.min(at)));
        }
    }
    actions.ended = inner.ended.is_some();
    actions
}

/// Pause when nobody listens, end when paused or idle too long, give up on an activation that
/// never comes.
fn idle_rules(shared: &Shared, inner: &mut Inner, now: Instant, actions: &mut Actions) {
    let settings = &shared.settings;
    match inner.state {
        SessionState::Playing => {
            let idle_at = inner.last_fetch + settings.fetch_timeout;
            if !inner.pause_requested && now >= idle_at {
                inner.pause_requested = true;
                actions.pause = true;
            }
            if !inner.pause_requested {
                actions.next = Some(idle_at);
            }
        }
        SessionState::Paused | SessionState::Ready => {
            let end_at = inner.since + settings.paused_timeout;
            if now >= end_at {
                Shared::end_session(inner, EndReason::Idle, None);
                shared.changed(inner);
            } else {
                actions.next = Some(end_at);
            }
        }
        SessionState::Starting if !inner.activated => {
            let end_at = inner.since + settings.activation_timeout;
            if now >= end_at {
                Shared::end_session(
                    inner,
                    EndReason::Failed,
                    Some("Music Assistant did not activate the player".into()),
                );
                shared.changed(inner);
            } else {
                actions.next = Some(end_at);
            }
        }
        _ => {}
    }
}

async fn call(what: &str, future: ControlFuture<'_>) {
    if let Err(error) = future.await {
        tracing::warn!(%error, "relay could not {what} the player");
    }
}

async fn run(
    shared: Arc<Shared>,
    client: Arc<sendspin::Client>,
    control: Arc<dyn PlayerControl>,
    cancel: CancellationToken,
) {
    loop {
        let actions = tick(&shared);
        if actions.pause {
            tracing::info!("relay idle: nobody fetched for a while, pausing");
            call("pause", control.pause(&shared.player_id)).await;
        }
        if actions.resume {
            call("resume", control.resume(&shared.player_id)).await;
        }
        if actions.ended {
            break;
        }
        let next = actions
            .next
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            () = cancel.cancelled() => {
                let mut inner = shared.lock();
                Shared::end_session(&mut inner, EndReason::ServerRestart, None);
                shared.changed(&inner);
                break;
            }
            () = shared.wake.notified() => {}
            () = tokio::time::sleep_until(next) => {}
        }
    }
    let reason = shared.lock().ended.as_ref().map(|(r, _)| *r);
    let goodbye = match reason {
        Some(EndReason::ServerRestart) => GoodbyeReason::Shutdown,
        _ => GoodbyeReason::UserRequest,
    };
    client.close(goodbye).await;
    let mut inner = shared.lock();
    inner.encoder = None;
    shared.changed(&inner);
}
