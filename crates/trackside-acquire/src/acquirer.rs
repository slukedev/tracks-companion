//! The acquirer: request handling, dedupe, quotas, search sessions and the admin operations.
//! The leased job runner is in [`crate::runner`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use tokio::sync::{Notify, OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use trackside_core::config::{AcquireConfig, Rate};
use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionState, AcquisitionView, AdminAcquisitionView, AdminCandidate, CandidateFormat,
    CandidateView, ExternalPlay, ExternalPlayRequest, ExternalQuery, ExternalSearch, FetchedUsage,
    GrowingFile, HlsJobId, InstantStream, InstantStreamUrls, ProviderChange, ProviderHealthState,
    ProviderStats, ProviderStatus, ProviderTest, Quality, ScanControl,
};
use trackside_core::ids::PublicId;
use trackside_core::records::Page;
use trackside_core::repo::acquisitions::{self, AcquisitionRow, CandidateRow, NewAcquisition};
use trackside_core::repo::audit::{self, AuditEntry};
use trackside_core::repo::{settings, songs, users};
use trackside_core::{
    Access, AcquisitionId, BoxFuture, CandidateToken, Clock, Db, DbError, EventBus, Millis,
    Permissions, ServerEvent, UserKey, random_id,
};
use trackside_media::{HlsInput, Media, MediaError, Priority, Profile};

use crate::limits::{Cooldown, TokenBucket};
use crate::provider::{
    Candidate, Provider, ProviderHealth, ProviderName, ProviderSettings, SearchQuery, SourceHints,
};
use crate::rank::{RankOptions, Ranked, quality_of, rank, tolerance_ms};
use crate::store::{self, TargetLibrary};
use crate::update;
use crate::verify::AudioInspector;
use crate::ytdlp;

/// What the acquirer needs from media: verification and HLS for instant play.
pub trait MediaOps: AudioInspector {
    /// Starts an HLS job that reads a growing file, for `user` only. `None` when there is no
    /// transcoder.
    fn start_hls(
        &self,
        file: GrowingFile,
        duration_ms: u32,
        user: Option<UserKey>,
    ) -> BoxFuture<'_, Result<Option<HlsJobId>, MediaError>>;
}

impl MediaOps for Media {
    fn start_hls(
        &self,
        file: GrowingFile,
        duration_ms: u32,
        user: Option<UserKey>,
    ) -> BoxFuture<'_, Result<Option<HlsJobId>, MediaError>> {
        Box::pin(async move {
            let input = HlsInput::Growing { file, duration_ms };
            let owner = trackside_media::HlsOwner { user, hold: None };
            match Media::start_hls(self, input, Profile::Aac256, Priority::Interactive, owner).await
            {
                Ok(job) => Ok(Some(job)),
                Err(MediaError::NoTranscoder) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }
}

/// Free space on a volume. The real one asks the filesystem; tests set it.
pub trait FreeSpace: Send + Sync + std::fmt::Debug + 'static {
    fn available(&self, path: &Path) -> std::io::Result<u64>;
}

/// [`FreeSpace`] from the filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskSpace;

impl FreeSpace for DiskSpace {
    fn available(&self, path: &Path) -> std::io::Result<u64> {
        fs4::available_space(path)
    }
}

/// Intervals and limits of the runner. The defaults are architecture section 9.3.
#[derive(Clone, Debug)]
pub struct Timing {
    pub lease: Duration,
    pub renew_every: Duration,
    /// Waits between whole-job attempts; after the last one the job fails `exhausted`.
    pub backoff: Vec<Duration>,
    /// The longest the runner sleeps without being woken.
    pub idle_poll: Duration,
    pub janitor_every: Duration,
    /// `.part` files older than this are removed.
    pub part_max_age: Duration,
    pub progress_every: Duration,
    pub health_every: Duration,
    /// How soon an unhealthy provider is checked again before use.
    pub unhealthy_recheck: Duration,
    /// The queue pauses while staging or the library has less than this free.
    pub min_free_bytes: u64,
    /// How long the free-space check may take before the queue pauses as if the volume were
    /// full: `statfs` on a hung network mount never returns.
    pub space_check_deadline: Duration,
    pub max_jobs: usize,
    pub search_session: Duration,
    /// How long provider searches of a session may keep running after the response.
    pub search_cap: Duration,
    pub candidate_ttl: Duration,
    pub user_search_rate: Rate,
    pub ytdlp_release_base: String,
    /// Songs played without being kept: how long one stays protected from eviction after it
    /// was last played, and how many one user may be fetching at once.
    pub external_in_use: Duration,
    pub external_per_user: usize,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            lease: Duration::from_mins(5),
            renew_every: Duration::from_secs(60),
            backoff: vec![
                Duration::from_secs(60),
                Duration::from_mins(5),
                Duration::from_mins(30),
                Duration::from_hours(2),
            ],
            idle_poll: Duration::from_secs(30),
            janitor_every: Duration::from_hours(1),
            part_max_age: Duration::from_hours(24),
            progress_every: Duration::from_secs(1),
            health_every: Duration::from_hours(24),
            unhealthy_recheck: Duration::from_secs(60),
            min_free_bytes: 256 * 1024 * 1024,
            space_check_deadline: Duration::from_secs(5),
            max_jobs: 8,
            search_session: Duration::from_mins(10),
            search_cap: Duration::from_secs(30),
            candidate_ttl: Duration::from_hours(24),
            user_search_rate: Rate {
                count: 20,
                per: Duration::from_secs(60),
            },
            ytdlp_release_base: String::from(update::DEFAULT_RELEASE_BASE),
            external_in_use: Duration::from_mins(30),
            external_per_user: 2,
        }
    }
}

/// Everything [`Acquirer::start_with`] wires together.
pub struct AcquirerDeps {
    pub db: Db,
    pub scan: Arc<dyn ScanControl>,
    pub media: Arc<dyn MediaOps>,
    pub config: AcquireConfig,
    pub providers: Vec<Arc<dyn Provider>>,
    pub events: EventBus,
    pub clock: Arc<dyn Clock>,
    pub shutdown: CancellationToken,
    pub free_space: Arc<dyn FreeSpace>,
    pub timing: Timing,
    /// The data directory, where the yt-dlp updater installs `bin/yt-dlp`.
    pub data_dir: Option<PathBuf>,
}

impl std::fmt::Debug for AcquirerDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcquirerDeps")
            .field("providers", &self.providers)
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

/// Settings the dashboard can change at run time.
#[derive(Clone, Debug)]
pub(crate) struct Live {
    pub enabled: bool,
    pub instant_play: bool,
    pub member_daily: u32,
    pub member_active: u32,
    pub global_daily: u32,
    pub prefer: Vec<ProviderName>,
    pub fetched_keep: u32,
    pub fetched_max_bytes: u64,
}

/// Today's counters.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DayStats {
    pub day: i64,
    pub stats: ProviderStats,
}

/// One provider with its limits and state.
#[derive(Debug)]
pub(crate) struct Slot {
    pub provider: Arc<dyn Provider>,
    pub enabled: AtomicBool,
    pub concurrency: AtomicU32,
    pub fetches: Arc<Semaphore>,
    pub searches: TokenBucket,
    pub cooldown: Mutex<Cooldown>,
    pub health: Mutex<(ProviderHealth, Option<Millis>)>,
    pub stats: Mutex<DayStats>,
}

impl Slot {
    pub fn name(&self) -> ProviderName {
        self.provider.name()
    }

    pub fn cooldown_until(&self, now: Millis) -> Option<Millis> {
        lock(&self.cooldown).active(now)
    }

    pub fn count(&self, now: Millis, f: impl FnOnce(&mut ProviderStats)) {
        let mut stats = lock(&self.stats);
        let day = now.0.div_euclid(86_400_000);
        if stats.day != day {
            *stats = DayStats {
                day,
                stats: ProviderStats::default(),
            };
        }
        f(&mut stats.stats);
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A job that is running in this process.
#[derive(Debug)]
pub(crate) struct Running {
    pub cancel: CancellationToken,
    /// Set before `cancel` when a user or admin cancelled, as opposed to a shutdown.
    pub cancelled_by_user: Arc<AtomicBool>,
    pub stream: Option<InstantStream>,
    pub quality: Option<Quality>,
}

/// A search whose provider tasks may still be adding candidates.
#[derive(Debug)]
struct SearchSession {
    user: UserKey,
    query: SearchQuery,
    expires: Millis,
    state: Arc<Mutex<SessionState>>,
}

#[derive(Debug, Default)]
pub(crate) struct SessionState {
    pub candidates: Vec<Candidate>,
    pending: usize,
    answered: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct UpdateStatus {
    pub running: bool,
    pub detail: Option<String>,
    pub installed: Option<String>,
}

/// State shared by the request handlers, the runner and its jobs.
pub(crate) struct Inner {
    pub db: Db,
    pub scan: Arc<dyn ScanControl>,
    pub media: Arc<dyn MediaOps>,
    pub config: AcquireConfig,
    pub events: EventBus,
    pub clock: Arc<dyn Clock>,
    pub shutdown: CancellationToken,
    pub free_space: Arc<dyn FreeSpace>,
    pub timing: Timing,
    pub data_dir: Option<PathBuf>,
    pub live: RwLock<Live>,
    pub slots: Vec<Slot>,
    pub jobs: Mutex<HashMap<AcquisitionId, Running>>,
    searches: Mutex<HashMap<String, SearchSession>>,
    user_searches: Mutex<HashMap<UserKey, Arc<TokenBucket>>>,
    pub target: RwLock<Option<TargetLibrary>>,
    pub wake: Notify,
    pub ready: OnceCell<()>,
    pub tracker: TaskTracker,
    pub disk_paused: AtomicBool,
    pub update: Mutex<UpdateStatus>,
    pub external: Mutex<crate::external::ExternalCache>,
    /// Held while a play without keeping starts, so two plays of one song start one fetch.
    pub external_start: Arc<tokio::sync::Mutex<()>>,
    pub external_dir: PathBuf,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}

/// The acquirer: request handling and the leased job runner.
pub struct Acquirer {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Acquirer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Acquirer")
            .field("providers", &self.inner.slots.len())
            .finish_non_exhaustive()
    }
}

impl Acquirer {
    /// Starts the job runner and the staging janitor.
    #[expect(clippy::too_many_arguments, reason = "the wiring the server does once")]
    pub fn start(
        db: Db,
        scan: Arc<dyn ScanControl>,
        media: Arc<Media>,
        config: &AcquireConfig,
        providers: Vec<Arc<dyn Provider>>,
        events: EventBus,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let data_dir = config
            .staging_dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        Self::start_with(AcquirerDeps {
            db,
            scan,
            media,
            config: config.clone(),
            providers,
            events,
            clock,
            shutdown,
            free_space: Arc::new(DiskSpace),
            timing: Timing::default(),
            data_dir,
        })
    }

    /// [`Acquirer::start`] with every seam open, for tests and custom wiring.
    pub fn start_with(deps: AcquirerDeps) -> Arc<Self> {
        let config = deps.config;
        let prefer = parse_prefer(&config.prefer);
        let slots = deps
            .providers
            .into_iter()
            .map(|provider| {
                let (enabled, concurrency, rate) = match provider.name() {
                    ProviderName::Ytdlp => (
                        config.ytdlp.enabled,
                        config.ytdlp.concurrency,
                        config.ytdlp.search_rate,
                    ),
                    ProviderName::Soulseek => (
                        config.soulseek.enabled,
                        config.soulseek.concurrency,
                        Rate {
                            count: 1,
                            per: Duration::from_secs(3),
                        },
                    ),
                };
                let concurrency = concurrency.max(1);
                Slot {
                    provider,
                    enabled: AtomicBool::new(enabled),
                    concurrency: AtomicU32::new(concurrency),
                    fetches: Arc::new(Semaphore::new(concurrency as usize)),
                    searches: TokenBucket::new(rate),
                    cooldown: Mutex::new(Cooldown::default()),
                    health: Mutex::new((ProviderHealth::Ok { version: None }, None)),
                    stats: Mutex::new(DayStats::default()),
                }
            })
            .collect();
        let external_dir = deps.data_dir.as_deref().map_or_else(
            || config.staging_dir.with_file_name("external"),
            crate::external::dir_in,
        );
        let inner = Arc::new(Inner {
            db: deps.db,
            scan: deps.scan,
            media: deps.media,
            live: RwLock::new(Live {
                enabled: config.enabled,
                instant_play: config.instant_play,
                member_daily: config.member_daily,
                member_active: config.member_active,
                global_daily: config.global_daily,
                prefer,
                fetched_keep: config.fetched_keep,
                fetched_max_bytes: config.fetched_max_bytes,
            }),
            config,
            events: deps.events,
            clock: deps.clock,
            shutdown: deps.shutdown,
            free_space: deps.free_space,
            timing: deps.timing,
            data_dir: deps.data_dir,
            slots,
            jobs: Mutex::new(HashMap::new()),
            searches: Mutex::new(HashMap::new()),
            user_searches: Mutex::new(HashMap::new()),
            target: RwLock::new(None),
            wake: Notify::new(),
            ready: OnceCell::new(),
            tracker: TaskTracker::new(),
            disk_paused: AtomicBool::new(false),
            update: Mutex::new(UpdateStatus::default()),
            external: Mutex::default(),
            external_start: Arc::new(tokio::sync::Mutex::new(())),
            external_dir,
        });
        let runner = Arc::clone(&inner);
        inner.tracker.spawn(crate::runner::run(runner));
        Arc::new(Self { inner })
    }

    /// Stops the runner and waits for it and every job to end. Jobs keep their state and
    /// leases, so the next start resumes them.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        self.inner.tracker.wait().await;
    }

    /// Runs the staging janitor now.
    pub async fn run_janitor(&self) -> Result<(), DbError> {
        crate::runner::janitor(&self.inner).await
    }

    /// Whether the queue is paused for lack of disk space.
    pub fn paused_for_space(&self) -> bool {
        self.inner.disk_paused.load(Ordering::Relaxed)
    }
}

pub(crate) fn parse_prefer(names: &[String]) -> Vec<ProviderName> {
    names
        .iter()
        .filter_map(|n| ProviderName::parse(n))
        .collect()
}

/// Setting keys for dashboard changes.
mod keys {
    pub const ENABLED: &str = "acquire.enabled";
    pub const INSTANT_PLAY: &str = "acquire.instant_play";
    pub const MEMBER_DAILY: &str = "acquire.member_daily";
    pub const MEMBER_ACTIVE: &str = "acquire.member_active";
    pub const GLOBAL_DAILY: &str = "acquire.global_daily";
    pub const PREFER: &str = "acquire.prefer";
    pub const FETCHED_KEEP: &str = "acquire.fetched_keep";
    pub const FETCHED_MAX_BYTES: &str = "acquire.fetched_max_bytes";
}

fn provider_key(name: ProviderName, field: &str) -> String {
    format!("acquire.{}.{field}", name.as_str())
}

const DAY: Duration = Duration::from_hours(24);

impl Inner {
    pub fn now(&self) -> Millis {
        self.clock.now()
    }

    pub fn live(&self) -> Live {
        self.live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn slot(&self, name: ProviderName) -> Option<&Slot> {
        self.slots.iter().find(|s| s.name() == name)
    }

    pub fn slot_named(&self, name: &str) -> Option<&Slot> {
        ProviderName::parse(name).and_then(|n| self.slot(n))
    }

    pub fn rank_options(&self, allow_versions: bool) -> RankOptions {
        RankOptions {
            allow_versions,
            reject_words: self.config.reject_words.clone(),
            prefer: self.live().prefer,
            max_file_bytes: self.config.max_file_bytes,
            min_lossy_kbps: self.config.soulseek.min_bitrate,
        }
    }

    /// Loads dashboard overrides and the acquired library, once.
    pub async fn ensure_ready(&self) -> Result<(), DbError> {
        self.ready
            .get_or_try_init(|| async {
                let stored = self.db.read(settings::all).await?;
                self.apply_stored(&stored);
                self.refresh_target().await?;
                self.load_external().await;
                Ok::<(), DbError>(())
            })
            .await?;
        Ok(())
    }

    fn apply_stored(&self, stored: &[(String, serde_json::Value)]) {
        let get = |key: &str| stored.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        {
            let mut live = self.live.write().unwrap_or_else(PoisonError::into_inner);
            if let Some(v) = get(keys::ENABLED).and_then(serde_json::Value::as_bool) {
                live.enabled = v;
            }
            if let Some(v) = get(keys::INSTANT_PLAY).and_then(serde_json::Value::as_bool) {
                live.instant_play = v;
            }
            let number = |key| {
                get(key)
                    .and_then(serde_json::Value::as_u64)
                    .map(|n| n as u32)
            };
            if let Some(v) = number(keys::MEMBER_DAILY) {
                live.member_daily = v;
            }
            if let Some(v) = number(keys::MEMBER_ACTIVE) {
                live.member_active = v;
            }
            if let Some(v) = number(keys::GLOBAL_DAILY) {
                live.global_daily = v;
            }
            if let Some(v) = number(keys::FETCHED_KEEP) {
                live.fetched_keep = v;
            }
            if let Some(v) = get(keys::FETCHED_MAX_BYTES)
                .and_then(serde_json::Value::as_str)
                .and_then(trackside_core::config::parse_size)
            {
                live.fetched_max_bytes = v;
            }
            if let Some(v) = get(keys::PREFER)
                .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            {
                live.prefer = parse_prefer(&v);
            }
        }
        for slot in &self.slots {
            if let Some(v) =
                get(&provider_key(slot.name(), "enabled")).and_then(serde_json::Value::as_bool)
            {
                slot.enabled.store(v, Ordering::Relaxed);
            }
            if let Some(v) =
                get(&provider_key(slot.name(), "concurrency")).and_then(serde_json::Value::as_u64)
            {
                resize(slot, v as u32);
            }
            if let Some(url) =
                get(&provider_key(slot.name(), "url")).and_then(serde_json::Value::as_str)
            {
                let _ = slot.provider.reconfigure(&ProviderSettings {
                    url: Some(url.to_owned()),
                    api_key: None,
                });
            }
        }
    }

    /// Re-reads which library acquisitions go to.
    pub async fn refresh_target(&self) -> Result<Option<TargetLibrary>, DbError> {
        let configured = self.config.library.clone();
        let target = self
            .db
            .read(move |c| {
                let chosen: Option<String> =
                    settings::get(c, settings::keys::ACQUIRE_LIBRARY)?.or(configured);
                match chosen {
                    Some(id) => store::target_library(c, &id),
                    None => Ok(None),
                }
            })
            .await?;
        self.target
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clone_from(&target);
        Ok(target)
    }

    pub fn cached_target(&self) -> Option<TargetLibrary> {
        self.target
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether a provider may be used now: enabled, not cooling down, and healthy enough. An
    /// unhealthy provider is checked again at most once a minute.
    pub async fn usable(&self, slot: &Slot) -> bool {
        if !slot.enabled.load(Ordering::Relaxed) || !slot.provider.configured() {
            return false;
        }
        let now = self.now();
        if slot.cooldown_until(now).is_some() {
            return false;
        }
        let (health, checked) = lock(&slot.health).clone();
        if health.is_usable() {
            return true;
        }
        if checked.is_some_and(|at| now.since(at) < self.timing.unhealthy_recheck) {
            return false;
        }
        self.check_health(slot).await.is_usable()
    }

    pub async fn check_health(&self, slot: &Slot) -> ProviderHealth {
        let health = slot.provider.health().await;
        *lock(&slot.health) = (health.clone(), Some(self.now()));
        health
    }

    /// Records a bot check or throttling.
    pub fn trip_cooldown(&self, slot: &Slot, retry_after: Option<Duration>) -> Millis {
        let until = lock(&slot.cooldown).trip(self.now(), retry_after);
        tracing::warn!(
            provider = slot.name().as_str(),
            until = %until.to_rfc3339(),
            "provider paused after a bot check or throttling; adding a cookies file may help"
        );
        until
    }

    pub fn instant_capable(&self, provider: ProviderName) -> bool {
        self.live().instant_play && self.slot(provider).is_some_and(|s| s.provider.streams())
    }

    /// The view Tracks sees.
    pub fn view(&self, row: &AcquisitionRow, quality: Option<Quality>) -> AcquisitionView {
        let jobs = lock(&self.jobs);
        let running = jobs
            .get(&row.id)
            .or_else(|| row.joined_to.as_ref().and_then(|j| jobs.get(j)));
        let stream = (row.instant && !row.state.is_finished())
            .then(|| running.and_then(|r| r.stream.as_ref()))
            .flatten()
            .map(|_| InstantStreamUrls {
                hls: format!("/api/v1/acquisitions/{}/stream.m3u8", row.id),
                progressive: format!("/api/v1/acquisitions/{}/stream", row.id),
            });
        let quality = quality.or_else(|| running.and_then(|r| r.quality));
        AcquisitionView {
            id: row.id.clone(),
            state: row.state,
            title: row.title.clone(),
            artist: row.artist.clone(),
            album: row.album.clone(),
            progress: row.progress_bytes.map(|b| (b, row.total_bytes)),
            quality,
            song: row.song.clone(),
            existing: row.existing,
            failure: row.failure,
            stream,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }

    pub fn publish(&self, row: &AcquisitionRow, quality: Option<Quality>) {
        if let Some(user) = row.user {
            self.events.publish(ServerEvent::Acquisition {
                user,
                view: self.view(row, quality),
            });
        }
    }

    pub async fn audit(
        &self,
        actor: Option<UserKey>,
        action: &'static str,
        target: Option<String>,
        detail: serde_json::Value,
    ) {
        let result = self
            .db
            .write(move |tx| {
                let actor_name = match actor {
                    Some(user) => users::get(tx, user)?
                        .map_or_else(|| String::from("unknown"), |u| u.username),
                    None => String::from("system"),
                };
                audit::log(
                    tx,
                    &AuditEntry {
                        actor,
                        actor_name,
                        ip: None,
                        action: action.to_owned(),
                        target,
                        detail: Some(detail),
                    },
                )
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, action, "could not write the audit log");
        }
    }

    pub(crate) fn user_limiter(&self, user: UserKey) -> Arc<TokenBucket> {
        Arc::clone(
            lock(&self.user_searches)
                .entry(user)
                .or_insert_with(|| Arc::new(TokenBucket::new(self.timing.user_search_rate))),
        )
    }
}

/// Grows or shrinks a provider's fetch semaphore.
fn resize(slot: &Slot, to: u32) {
    let to = to.max(1);
    let from = slot.concurrency.swap(to, Ordering::Relaxed);
    if to > from {
        slot.fetches.add_permits((to - from) as usize);
    } else if from > to {
        let fetches = Arc::clone(&slot.fetches);
        let shrink = from - to;
        tokio::spawn(async move {
            if let Ok(permits) = fetches.acquire_many_owned(shrink).await {
                permits.forget();
            }
        });
    }
}

pub(crate) fn quality_of_row(c: &CandidateRow) -> Quality {
    if c.lossless {
        Quality::Lossless
    } else if c.bitrate_kbps.is_some_and(|k| k >= 256) {
        Quality::High
    } else {
        Quality::Standard
    }
}

/// A stored candidate as the fetch code needs it. Hints are only for ranking, so they are
/// rebuilt from the reference.
pub(crate) fn candidate_of_row(row: &CandidateRow) -> Option<Candidate> {
    let provider = ProviderName::parse(&row.provider)?;
    let text = |key: &str| {
        row.provider_ref
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let hints = match provider {
        ProviderName::Ytdlp => SourceHints::Video {
            uploader: String::new(),
            topic_channel: false,
            official: false,
            music_video: false,
        },
        ProviderName::Soulseek => SourceHints::Peer {
            username: text("username"),
            folder: String::new(),
            free_slot: false,
            queue_length: 0,
            upload_speed: 0,
        },
    };
    Some(Candidate {
        provider,
        provider_ref: row.provider_ref.clone(),
        title: row.title.clone(),
        artist: row.artist.clone(),
        album: row.album.clone(),
        duration_ms: row.duration_ms,
        container: row.container,
        codec: row.codec,
        bitrate_kbps: row.bitrate_kbps,
        sample_rate: None,
        bit_depth: None,
        lossless: row.lossless,
        size: row.size_bytes,
        hints,
    })
}

/// The stored form of a ranked candidate.
pub(crate) fn candidate_row(ranked: &Ranked, now: Millis, ttl: Duration) -> CandidateRow {
    let c = &ranked.candidate;
    CandidateRow {
        token: token_of(c),
        acquisition: None,
        provider: c.provider.as_str().to_owned(),
        provider_ref: c.provider_ref.clone(),
        title: c.title.clone(),
        artist: c.artist.clone(),
        album: c.album.clone(),
        duration_ms: c.duration_ms,
        container: c.container,
        codec: c.codec,
        bitrate_kbps: c.bitrate_kbps,
        lossless: c.lossless,
        size_bytes: c.size,
        score: ranked.score,
        tried_at: None,
        failure: None,
        created_at: now,
        expires_at: now.saturating_add(ttl),
    }
}

pub(crate) fn token_of(c: &Candidate) -> CandidateToken {
    CandidateToken::derive(c.provider.as_str(), &c.provider_ref.to_string())
}

/// Searches every usable provider in parallel. Each result is added to `state` as it comes;
/// the returned future ends when all are done or `wait` passes, while the tasks themselves
/// may run on until `cap`.
pub(crate) async fn search_providers(
    inner: &Arc<Inner>,
    query: &SearchQuery,
    wait: Duration,
    cap: Duration,
    state: Arc<Mutex<SessionState>>,
    only_streaming: bool,
) -> bool {
    let mut usable = Vec::new();
    for (i, slot) in inner.slots.iter().enumerate() {
        if (!only_streaming || slot.provider.streams()) && inner.usable(slot).await {
            usable.push(i);
        }
    }
    let done = Arc::new(Notify::new());
    lock(&state).pending += usable.len();
    for index in usable {
        let inner = Arc::clone(inner);
        let query = query.clone();
        let state = Arc::clone(&state);
        let done = Arc::clone(&done);
        let cancel = inner.shutdown.child_token();
        inner.tracker.clone().spawn(async move {
            let slot = &inner.slots[index];
            let found = tokio::time::timeout(cap, async {
                slot.searches.take().await;
                slot.count(inner.now(), |s| s.searches_today += 1);
                slot.provider.search(&query, cancel.clone()).await
            })
            .await;
            cancel.cancel();
            match found {
                Ok(Ok(candidates)) => {
                    lock(&slot.cooldown).succeed();
                    let mut state = lock(&state);
                    state.candidates.extend(candidates);
                    state.answered += 1;
                }
                Ok(Err(e)) => {
                    slot.count(inner.now(), |s| s.failures_today += 1);
                    if e.wants_cooldown() {
                        let retry = match &e {
                            crate::ProviderError::RateLimited { retry_after } => *retry_after,
                            _ => None,
                        };
                        inner.trip_cooldown(slot, retry);
                    }
                    let health = match &e {
                        crate::ProviderError::Unavailable(why) => {
                            Some(ProviderHealth::Unavailable(why.clone()))
                        }
                        crate::ProviderError::NotSetUp(why) => {
                            Some(ProviderHealth::NotSetUp(why.clone()))
                        }
                        _ => None,
                    };
                    if let Some(health) = health {
                        *lock(&slot.health) = (health, Some(inner.now()));
                    }
                    tracing::info!(provider = slot.name().as_str(), error = %e, "search failed");
                }
                Err(_) => {
                    tracing::info!(provider = slot.name().as_str(), "search timed out");
                }
            }
            lock(&state).pending -= 1;
            done.notify_waiters();
        });
    }
    let all_done = async {
        loop {
            let notified = done.notified();
            if lock(&state).pending == 0 {
                return;
            }
            notified.await;
        }
    };
    tokio::time::timeout(wait, all_done).await.is_ok()
}

impl Inner {
    /// Searches for a job: all usable providers, waiting up to the search timeout. Returns
    /// the candidates and how many providers answered.
    pub(crate) async fn search_for_job(
        self: &Arc<Self>,
        query: &SearchQuery,
    ) -> (Vec<Candidate>, usize) {
        let state = Arc::new(Mutex::new(SessionState::default()));
        search_providers(
            self,
            query,
            self.config.search_timeout,
            self.config.search_timeout,
            Arc::clone(&state),
            false,
        )
        .await;
        let mut state = lock(&state);
        (std::mem::take(&mut state.candidates), state.answered)
    }

    async fn search_result(
        &self,
        caller: &Access,
        search_id: String,
        query: &SearchQuery,
        state: &Arc<Mutex<SessionState>>,
    ) -> Result<ExternalSearch, AcquireError> {
        let (candidates, complete) = {
            let state = lock(state);
            (state.candidates.clone(), state.pending == 0)
        };
        let ranked = rank(query, candidates, &self.rank_options(false));
        let now = self.now();
        let rows: Vec<CandidateRow> = ranked
            .iter()
            .map(|r| candidate_row(r, now, self.timing.candidate_ttl))
            .collect();
        let views = ranked
            .iter()
            .zip(&rows)
            .map(|(r, row)| CandidateView {
                token: row.token.clone(),
                title: r.candidate.title.clone(),
                artist: r.candidate.artist.clone(),
                album: r.candidate.album.clone(),
                duration_ms: r.candidate.duration_ms,
                quality: quality_of(&r.candidate),
                format: CandidateFormat {
                    container: r.candidate.container,
                    codec: r.candidate.codec,
                    bitrate_kbps: r.candidate.bitrate_kbps,
                    sample_rate: r.candidate.sample_rate,
                    bit_depth: r.candidate.bit_depth,
                },
                size: r.candidate.size,
                instant: self.instant_capable(r.candidate.provider),
                exact: r.exact,
            })
            .collect();
        let access = caller.clone();
        let key = (query.free_text.is_none())
            .then(|| tracks_model::song_key(&query.title, &query.artist));
        let in_library = self
            .db
            .write(move |tx| {
                acquisitions::put_candidates(tx, None, &rows)?;
                let Some(key) = key else {
                    return Ok(Vec::new());
                };
                Ok(songs::by_key(tx, &access, &key)?
                    .into_iter()
                    .filter(|song| !song.missing)
                    .collect())
            })
            .await?;
        Ok(ExternalSearch {
            search_id,
            complete,
            in_library,
            candidates: views,
        })
    }
}

fn query_of(query: ExternalQuery) -> Result<SearchQuery, AcquireError> {
    let q = match query {
        ExternalQuery::Text(text) => SearchQuery {
            title: String::new(),
            artist: String::new(),
            album: None,
            duration_ms: None,
            free_text: Some(text.trim().to_owned()),
        },
        ExternalQuery::Fields {
            title,
            artist,
            album,
            duration_ms,
        } => SearchQuery {
            title: title.trim().to_owned(),
            artist: artist.trim().to_owned(),
            album: album.map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()),
            duration_ms,
            free_text: None,
        },
    };
    if q.text().is_empty() {
        return Err(AcquireError::Invalid(String::from("the search is empty")));
    }
    Ok(q)
}

/// What a request turned into, decided in one write transaction.
enum Decided {
    Created(AcquisitionRow),
    Existing(AcquisitionRow),
    Joined(AcquisitionRow),
    Quota(Millis),
    CandidateExpired,
    NotInstant,
}

impl AcquireControl for Acquirer {
    fn feature_for(
        &self,
        caller: &Access,
        permissions: &Permissions,
        scopes_include_request: bool,
    ) -> Option<AcquireFeature> {
        let inner = &self.inner;
        let live = inner.live();
        let target = inner.cached_target()?;
        let provider_on = inner
            .slots
            .iter()
            .any(|s| s.enabled.load(Ordering::Relaxed) && s.provider.configured());
        let allowed = permissions.can_request || caller.is_admin();
        (live.enabled
            && scopes_include_request
            && allowed
            && provider_on
            && caller.can_see(target.key))
        .then(|| {
            let streaming = inner
                .slots
                .iter()
                .any(|s| s.enabled.load(Ordering::Relaxed) && s.provider.streams());
            AcquireFeature {
                instant_play: live.instant_play && streaming,
                external_play: streaming,
            }
        })
    }

    fn search<'a>(
        &'a self,
        caller: &'a Access,
        query: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        Box::pin(async move {
            let inner = &self.inner;
            inner.ensure_ready().await?;
            if !inner.live().enabled {
                return Err(AcquireError::Disabled);
            }
            if let Err(wait) = inner.user_limiter(caller.user).try_take() {
                return Err(AcquireError::RateLimited { retry_after: wait });
            }
            let query = query_of(query)?;
            let state = Arc::new(Mutex::new(SessionState::default()));
            search_providers(
                inner,
                &query,
                inner.config.search_timeout,
                inner.timing.search_cap,
                Arc::clone(&state),
                false,
            )
            .await;
            let search_id = random_id::<AcquisitionId>().as_str()[1..].to_owned();
            let now = inner.now();
            {
                let mut searches = lock(&inner.searches);
                searches.retain(|_, s| s.expires > now);
                searches.insert(
                    search_id.clone(),
                    SearchSession {
                        user: caller.user,
                        query: query.clone(),
                        expires: now.saturating_add(inner.timing.search_session),
                        state: Arc::clone(&state),
                    },
                );
            }
            inner.search_result(caller, search_id, &query, &state).await
        })
    }

    fn search_more<'a>(
        &'a self,
        caller: &'a Access,
        search: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        Box::pin(async move {
            let inner = &self.inner;
            let now = inner.now();
            let (query, state) = {
                let searches = lock(&inner.searches);
                let session = searches
                    .get(search)
                    .filter(|s| s.expires > now && s.user == caller.user)
                    .ok_or(AcquireError::NotFound)?;
                (session.query.clone(), Arc::clone(&session.state))
            };
            inner
                .search_result(caller, search.to_owned(), &query, &state)
                .await
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one transaction decides dedupe, joining, quotas and the candidate"
    )]
    fn request<'a>(
        &'a self,
        caller: &'a Access,
        request: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        Box::pin(async move {
            let inner = &self.inner;
            inner.ensure_ready().await?;
            let live = inner.live();
            if !live.enabled {
                return Err(AcquireError::Disabled);
            }
            let title = request.title.trim().to_owned();
            let artist = request.artist.trim().to_owned();
            if title.is_empty() || artist.is_empty() {
                return Err(AcquireError::Invalid(String::from(
                    "title and artist are required",
                )));
            }
            let target = inner
                .refresh_target()
                .await?
                .ok_or(AcquireError::Disabled)?;
            if !caller.can_see(target.key) {
                return Err(AcquireError::NotFound);
            }
            let streaming: Vec<String> = inner
                .slots
                .iter()
                .filter(|s| s.provider.streams())
                .map(|s| s.name().as_str().to_owned())
                .collect();
            if request.instant && !live.instant_play {
                return Err(AcquireError::Invalid(String::from("instant play is off")));
            }
            let request_key = tracks_model::song_key(&title, &artist);
            let access = caller.clone();
            let admin = caller.is_admin();
            let new = NewAcquisition {
                user: Some(caller.user),
                joined_to: None,
                request_key: request_key.clone(),
                title,
                artist,
                album: request
                    .album
                    .map(|a| a.trim().to_owned())
                    .filter(|a| !a.is_empty()),
                duration_ms: request.duration_ms,
                candidate_token: request.candidate.clone(),
                allow_versions: request.allow_versions,
                instant: request.instant,
                priority: u8::from(!request.instant),
                state: AcquisitionState::Queued,
                song: None,
                existing: false,
            };
            let decided = inner
                .db
                .write(move |tx| {
                    let now = tx.now();
                    let present = songs::by_key(tx, &access, &new.request_key)?
                        .into_iter()
                        .filter(|song| !song.missing)
                        .find(|song| {
                            new.duration_ms.is_none_or(|want| {
                                song.duration_ms.abs_diff(want) <= tolerance_ms(want)
                            })
                        });
                    if let Some(song) = present.map(|song| song.id) {
                        let row = acquisitions::insert(
                            tx,
                            &NewAcquisition {
                                state: AcquisitionState::Done,
                                song: Some(song),
                                existing: true,
                                candidate_token: None,
                                ..new
                            },
                        )?;
                        return Ok(Decided::Existing(row));
                    }
                    if let Some(active) = acquisitions::active_by_key(tx, &new.request_key)? {
                        let row = acquisitions::insert(
                            tx,
                            &NewAcquisition {
                                joined_to: Some(active.id.clone()),
                                state: active.state,
                                candidate_token: None,
                                ..new
                            },
                        )?;
                        let row = match active.progress_bytes {
                            Some(bytes) => {
                                store::set_progress(tx, &row.id, bytes, active.total_bytes)?
                            }
                            None => row,
                        };
                        return Ok(Decided::Joined(row));
                    }
                    let since = now.saturating_sub(DAY);
                    if !admin {
                        let (today, oldest) =
                            acquisitions::quota_for_user_since(tx, access.user, since)?;
                        if today >= live.member_daily {
                            return Ok(Decided::Quota(oldest.unwrap_or(now).saturating_add(DAY)));
                        }
                        if acquisitions::count_active_for_user(tx, access.user)?
                            >= live.member_active
                        {
                            return Ok(Decided::Quota(now.saturating_add(Duration::from_secs(60))));
                        }
                    }
                    let (all, oldest) = acquisitions::count_all_since(tx, since)?;
                    if all >= live.global_daily {
                        return Ok(Decided::Quota(oldest.unwrap_or(now).saturating_add(DAY)));
                    }
                    if let Some(token) = &new.candidate_token {
                        let Some(candidate) = acquisitions::candidate_by_token_at(tx, token, now)?
                        else {
                            return Ok(Decided::CandidateExpired);
                        };
                        if new.instant && !streaming.contains(&candidate.provider) {
                            return Ok(Decided::NotInstant);
                        }
                    } else if new.instant {
                        return Ok(Decided::NotInstant);
                    }
                    Ok(Decided::Created(acquisitions::insert(tx, &new)?))
                })
                .await?;
            let row = match decided {
                Decided::Quota(resets_at) => return Err(AcquireError::QuotaExceeded { resets_at }),
                Decided::CandidateExpired => return Err(AcquireError::CandidateExpired),
                Decided::NotInstant => {
                    return Err(AcquireError::Invalid(String::from(
                        "instant play needs a candidate that offers it",
                    )));
                }
                Decided::Existing(row) | Decided::Joined(row) | Decided::Created(row) => row,
            };
            inner
                .audit(
                    Some(caller.user),
                    "acquire.request",
                    Some(row.id.as_str().to_owned()),
                    serde_json::json!({
                        "title": row.title,
                        "artist": row.artist,
                        "album": row.album,
                        "existing": row.existing,
                        "joined": row.joined_to.is_some(),
                    }),
                )
                .await;
            inner.publish(&row, None);
            inner.wake.notify_one();
            Ok(inner.view(&row, None))
        })
    }

    fn get<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        Box::pin(async move {
            let id = id.clone();
            let found = self
                .inner
                .db
                .read(move |c| {
                    let Some(row) = acquisitions::get(c, &id)? else {
                        return Ok(None);
                    };
                    let quality = quality_for(c, &row)?;
                    Ok(Some((row, quality)))
                })
                .await?;
            match found {
                Some((row, quality)) if caller.is_admin() || row.user == Some(caller.user) => {
                    Ok(self.inner.view(&row, quality))
                }
                _ => Err(AcquireError::NotFound),
            }
        })
    }

    fn list<'a>(
        &'a self,
        caller: &'a Access,
        filter: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        Box::pin(async move {
            let filter = AcquisitionFilter {
                user: Some(caller.user),
                ..filter
            };
            let page = self
                .inner
                .db
                .read(move |c| list_with_quality(c, &filter))
                .await?;
            Ok(Page {
                items: page
                    .items
                    .iter()
                    .map(|(row, q)| self.inner.view(row, *q))
                    .collect(),
                next: page.next,
            })
        })
    }

    fn cancel<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        Box::pin(async move {
            let row = self
                .inner
                .db
                .read({
                    let id = id.clone();
                    move |c| acquisitions::get(c, &id)
                })
                .await?;
            let row = match row {
                Some(row) if caller.is_admin() || row.user == Some(caller.user) => row,
                _ => return Err(AcquireError::NotFound),
            };
            crate::runner::cancel_job(&self.inner, &row, Some(caller.user)).await
        })
    }

    fn instant_stream<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<InstantStream, AcquireError>> {
        Box::pin(async move {
            let id = id.clone();
            let row = self
                .inner
                .db
                .read(move |c| acquisitions::get(c, &id))
                .await?
                .filter(|row| caller.is_admin() || row.user == Some(caller.user))
                .ok_or(AcquireError::NotFound)?;
            let job = row.joined_to.unwrap_or(row.id);
            lock(&self.inner.jobs)
                .get(&job)
                .and_then(|r| r.stream.clone())
                .ok_or(AcquireError::NotFound)
        })
    }

    fn play_external<'a>(
        &'a self,
        caller: &'a Access,
        request: ExternalPlayRequest,
    ) -> BoxFuture<'a, Result<ExternalPlay, AcquireError>> {
        Box::pin(self.inner.play_external(caller, request))
    }

    fn external_stream(
        &self,
        _caller: &Access,
        token: &CandidateToken,
    ) -> Result<InstantStream, AcquireError> {
        self.inner
            .external_stream(token)
            .ok_or(AcquireError::NotFound)
    }

    fn fetched_usage(&self) -> Option<FetchedUsage> {
        Some(self.inner.fetched_usage())
    }

    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        Box::pin(async move {
            let inner = &self.inner;
            let now = inner.now();
            let update = lock(&inner.update).clone();
            inner
                .slots
                .iter()
                .map(|slot| {
                    let enabled = slot.enabled.load(Ordering::Relaxed);
                    let (health, _) = lock(&slot.health).clone();
                    let cooldown_until = slot.cooldown_until(now);
                    let (state, detail, version) = if !enabled {
                        (ProviderHealthState::Disabled, None, None)
                    } else if let Some(until) = cooldown_until {
                        (
                            ProviderHealthState::Cooldown,
                            Some(format!(
                                "paused until {} after a bot check or throttling; a cookies file may help",
                                until.to_rfc3339()
                            )),
                            None,
                        )
                    } else {
                        match health {
                            ProviderHealth::Ok { version } => (
                                ProviderHealthState::Ok,
                                version.as_ref().map(|v| format!("{} {v}", tool_name(slot.name()))),
                                version,
                            ),
                            ProviderHealth::Degraded(why) => (ProviderHealthState::Degraded, Some(why), None),
                            ProviderHealth::Cooldown { until } => (
                                ProviderHealthState::Cooldown,
                                Some(format!("paused until {}", until.to_rfc3339())),
                                None,
                            ),
                            ProviderHealth::Outdated { version } => (
                                ProviderHealthState::Outdated,
                                Some(format!("{} {version} is out of date", tool_name(slot.name()))),
                                Some(version),
                            ),
                            ProviderHealth::Unavailable(why) => (ProviderHealthState::Unavailable, Some(why), None),
                            ProviderHealth::NotSetUp(why) => (ProviderHealthState::NotSetUp, Some(why), None),
                            ProviderHealth::Disabled => (ProviderHealthState::Disabled, None, None),
                        }
                    };
                    let mut settings = slot.provider.settings_view();
                    if let Some(map) = settings.as_object_mut() {
                        map.insert(
                            String::from("concurrency"),
                            serde_json::json!(slot.concurrency.load(Ordering::Relaxed)),
                        );
                    }
                    let is_ytdlp = slot.name() == ProviderName::Ytdlp;
                    ProviderStatus {
                        name: slot.name().as_str().to_owned(),
                        enabled,
                        configured: slot.provider.configured(),
                        health: state,
                        detail: if is_ytdlp && update.running {
                            Some(String::from("updating yt-dlp"))
                        } else if is_ytdlp && update.detail.is_some() {
                            update.detail.clone()
                        } else {
                            detail
                        },
                        cooldown_until,
                        version,
                        latest_known: if is_ytdlp { update.installed.clone() } else { None },
                        settings,
                        stats: lock(&slot.stats).stats,
                    }
                })
                .collect()
        })
    }

    fn targets_changed(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Err(e) = self.inner.refresh_target().await {
                tracing::warn!(error = %e, "could not read the acquired library");
            }
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one match over every dashboard setting"
    )]
    fn configure(&self, change: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async move {
            let inner = &self.inner;
            inner.ensure_ready().await?;
            let mut stored: Vec<(String, serde_json::Value)> = Vec::new();
            let detail = match change {
                ProviderChange::Fetched { keep, max_bytes } => {
                    {
                        let mut live = inner.live.write().unwrap_or_else(PoisonError::into_inner);
                        live.fetched_keep = keep;
                        live.fetched_max_bytes = max_bytes;
                    }
                    let sweeper = Arc::clone(inner);
                    inner
                        .tracker
                        .spawn(async move { sweeper.sweep_external().await });
                    return Ok(());
                }
                ProviderChange::Provider {
                    name,
                    enabled,
                    concurrency,
                    url,
                    api_key,
                } => {
                    let slot = inner.slot_named(&name).ok_or(AcquireError::NotFound)?;
                    let key_changed = api_key.is_some();
                    if url.is_some() || api_key.is_some() {
                        slot.provider
                            .reconfigure(&ProviderSettings {
                                url: url.clone(),
                                api_key,
                            })
                            .map_err(|e| AcquireError::Invalid(e.to_string()))?;
                        *lock(&slot.health) = (ProviderHealth::Ok { version: None }, None);
                        inner.check_health(slot).await;
                    }
                    if let Some(url) = &url {
                        stored.push((provider_key(slot.name(), "url"), serde_json::json!(url)));
                    }
                    if let Some(enabled) = enabled {
                        slot.enabled.store(enabled, Ordering::Relaxed);
                        stored.push((
                            provider_key(slot.name(), "enabled"),
                            serde_json::json!(enabled),
                        ));
                        if enabled {
                            lock(&slot.cooldown).clear();
                            inner.check_health(slot).await;
                        }
                    }
                    if let Some(concurrency) = concurrency {
                        if !(1..=16).contains(&concurrency) {
                            return Err(AcquireError::Invalid(String::from(
                                "concurrency must be 1 to 16",
                            )));
                        }
                        resize(slot, concurrency);
                        stored.push((
                            provider_key(slot.name(), "concurrency"),
                            serde_json::json!(concurrency),
                        ));
                    }
                    serde_json::json!({
                        "provider": name, "enabled": enabled, "concurrency": concurrency,
                        "url": url, "api_key_changed": key_changed,
                    })
                }
                ProviderChange::Global {
                    enabled,
                    instant_play,
                    member_daily,
                    member_active,
                    global_daily,
                    prefer,
                } => {
                    let prefer = prefer
                        .map(|names| {
                            names
                                .iter()
                                .map(|n| {
                                    ProviderName::parse(n).ok_or_else(|| {
                                        AcquireError::Invalid(format!("unknown provider {n}"))
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .transpose()?;
                    let mut live = inner.live.write().unwrap_or_else(PoisonError::into_inner);
                    if let Some(v) = enabled {
                        live.enabled = v;
                        stored.push((keys::ENABLED.to_owned(), serde_json::json!(v)));
                    }
                    if let Some(v) = instant_play {
                        live.instant_play = v;
                        stored.push((keys::INSTANT_PLAY.to_owned(), serde_json::json!(v)));
                    }
                    if let Some(v) = member_daily {
                        live.member_daily = v;
                        stored.push((keys::MEMBER_DAILY.to_owned(), serde_json::json!(v)));
                    }
                    if let Some(v) = member_active {
                        live.member_active = v;
                        stored.push((keys::MEMBER_ACTIVE.to_owned(), serde_json::json!(v)));
                    }
                    if let Some(v) = global_daily {
                        live.global_daily = v;
                        stored.push((keys::GLOBAL_DAILY.to_owned(), serde_json::json!(v)));
                    }
                    if let Some(v) = prefer {
                        let names: Vec<&str> = v.iter().map(|n| n.as_str()).collect();
                        stored.push((keys::PREFER.to_owned(), serde_json::json!(names)));
                        live.prefer = v;
                    }
                    drop(live);
                    serde_json::json!({
                        "enabled": enabled, "instant_play": instant_play, "member_daily": member_daily,
                        "member_active": member_active, "global_daily": global_daily,
                    })
                }
            };
            let to_store = stored.clone();
            inner
                .db
                .write(move |tx| {
                    for (key, value) in &to_store {
                        settings::put(tx, key, value)?;
                    }
                    Ok(())
                })
                .await?;
            inner
                .audit(None, "acquire.provider_update", None, detail)
                .await;
            inner.wake.notify_one();
            Ok(())
        })
    }

    fn test_provider<'a>(
        &'a self,
        name: &'a str,
        query: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        Box::pin(async move {
            let inner = &self.inner;
            let slot = inner.slot_named(name).ok_or(AcquireError::NotFound)?;
            let started = tokio::time::Instant::now();
            let health = inner.check_health(slot).await;
            if !health.is_usable() {
                return Ok(ProviderTest {
                    ok: false,
                    elapsed: started.elapsed(),
                    candidates: 0,
                    detail: Some(format!("{health:?}")),
                });
            }
            let q = SearchQuery {
                title: String::new(),
                artist: String::new(),
                album: None,
                duration_ms: None,
                free_text: Some(query.to_owned()),
            };
            let cancel = inner.shutdown.child_token();
            let result = tokio::time::timeout(
                inner.timing.search_cap,
                slot.provider.search(&q, cancel.clone()),
            )
            .await;
            cancel.cancel();
            let (ok, candidates, detail) = match result {
                Ok(Ok(found)) => (true, found.len() as u32, None),
                Ok(Err(e)) => (false, 0, Some(e.to_string())),
                Err(_) => (false, 0, Some(String::from("the search timed out"))),
            };
            Ok(ProviderTest {
                ok,
                elapsed: started.elapsed(),
                candidates,
                detail,
            })
        })
    }

    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async move {
            let inner = Arc::clone(&self.inner);
            let data_dir = inner.data_dir.clone().ok_or_else(|| {
                AcquireError::Invalid(String::from("no data directory to install into"))
            })?;
            {
                let mut status = lock(&inner.update);
                if status.running {
                    return Err(AcquireError::Busy);
                }
                status.running = true;
                status.detail = None;
            }
            inner
                .audit(
                    None,
                    "acquire.provider_update",
                    Some(String::from("ytdlp")),
                    serde_json::json!({"update": true}),
                )
                .await;
            let task = Arc::clone(&inner);
            inner.tracker.spawn(async move {
                let target = ytdlp::updated_binary(&data_dir);
                let result = match crate::slskd::http_client() {
                    Ok(client) => update::install(
                        &client,
                        &task.timing.ytdlp_release_base,
                        update::asset_name(),
                        &target,
                        &ytdlp::tmp_dir(&data_dir),
                    )
                    .await
                    .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                {
                    let mut status = lock(&task.update);
                    status.running = false;
                    match result {
                        Ok(hash) => {
                            status.detail = None;
                            status.installed =
                                Some(format!("sha256 {}", &hash[..12.min(hash.len())]));
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "yt-dlp update failed");
                            status.detail = Some(format!("update failed: {e}"));
                        }
                    }
                }
                if let Some(slot) = task.slot(ProviderName::Ytdlp) {
                    task.check_health(slot).await;
                }
            });
            Ok(())
        })
    }

    fn retry<'a>(&'a self, id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        Box::pin(async move {
            let inner = &self.inner;
            let id = id.clone();
            let row = inner
                .db
                .write(move |tx| {
                    let Some(row) = acquisitions::get(tx, &id)? else {
                        return Ok(None);
                    };
                    if !matches!(
                        row.state,
                        AcquisitionState::Failed | AcquisitionState::Cancelled
                    ) {
                        return Ok(Some(Err(row)));
                    }
                    Ok(Some(Ok(store::requeue(tx, &id)?)))
                })
                .await?
                .ok_or(AcquireError::NotFound)?
                .map_err(|_| AcquireError::InvalidState)?;
            inner
                .audit(
                    None,
                    "acquire.retry",
                    Some(row.id.as_str().to_owned()),
                    serde_json::json!({}),
                )
                .await;
            inner.publish(&row, None);
            inner.wake.notify_one();
            Ok(())
        })
    }

    fn admin_list(
        &self,
        filter: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        Box::pin(async move {
            let rows = self
                .inner
                .db
                .read(move |c| {
                    let page = list_with_quality(c, &filter)?;
                    let mut items = Vec::with_capacity(page.items.len());
                    for (row, quality) in page.items {
                        let candidates = acquisitions::candidates(c, &row.id)?;
                        let user = row
                            .user
                            .map(|u| users::get(c, u))
                            .transpose()?
                            .flatten()
                            .map(|u| u.id);
                        items.push((row, quality, candidates, user));
                    }
                    Ok(Page {
                        items,
                        next: page.next,
                    })
                })
                .await?;
            let items = rows
                .items
                .into_iter()
                .map(|(row, quality, candidates, user)| {
                    let admin: Vec<AdminCandidate> = candidates
                        .iter()
                        .map(|c| AdminCandidate {
                            token: c.token.clone(),
                            provider: c.provider.clone(),
                            provider_ref: c.provider_ref.clone(),
                            title: c.title.clone(),
                            artist: c.artist.clone(),
                            score: c.score,
                            quality: quality_of_row(c),
                            format: CandidateFormat {
                                container: c.container,
                                codec: c.codec,
                                bitrate_kbps: c.bitrate_kbps,
                                sample_rate: None,
                                bit_depth: None,
                            },
                            size: c.size_bytes,
                            tried_at: c.tried_at,
                            failure: c.failure.clone(),
                        })
                        .collect();
                    let chosen = row
                        .candidate
                        .as_ref()
                        .and_then(|t| admin.iter().find(|c| &c.token == t).cloned());
                    AdminAcquisitionView {
                        view: self.inner.view(&row, quality),
                        user,
                        provider: row.provider.clone(),
                        chosen,
                        attempts: row.attempts,
                        failure_detail: row.failure_detail.clone(),
                        candidates: admin,
                    }
                })
                .collect();
            Ok(Page {
                items,
                next: rows.next,
            })
        })
    }
}

fn tool_name(name: ProviderName) -> &'static str {
    match name {
        ProviderName::Ytdlp => "yt-dlp",
        ProviderName::Soulseek => "slskd",
    }
}

fn quality_for(c: &rusqlite::Connection, row: &AcquisitionRow) -> Result<Option<Quality>, DbError> {
    let Some(token) = &row.candidate else {
        return Ok(None);
    };
    Ok(acquisitions::candidate_by_token(c, token)?.map(|c| quality_of_row(&c)))
}

fn list_with_quality(
    c: &rusqlite::Connection,
    filter: &AcquisitionFilter,
) -> Result<Page<(AcquisitionRow, Option<Quality>)>, DbError> {
    let page = acquisitions::list(c, filter)?;
    let mut items = Vec::with_capacity(page.items.len());
    for row in page.items {
        let quality = quality_for(c, &row)?;
        items.push((row, quality));
    }
    Ok(Page {
        items,
        next: page.next,
    })
}
