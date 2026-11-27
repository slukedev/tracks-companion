//! The native API under `/api/v1` for Tracks and the dashboard, including `/api/v1/admin`,
//! its wire types, and server-sent events. It maps core records to the wire and holds no
//! domain logic of its own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use axum::Router;
use axum::extract::FromRef;
use tokio_util::sync::CancellationToken;
use trackside_auth::Auth;
use trackside_core::now_playing::{NowPlayingRegistry, PlayState};
use trackside_core::{
    AcquireControl, BoxFuture, Config, ConfigReport, Db, EnrichControl, EventBus, Millis,
    ScanControl, ServerEvent, SongId, UserKey,
};
use trackside_media::Media;

mod error;
mod extract;
pub mod map;
mod qr;
mod routes;
mod sse;
pub mod wire;

/// The pure response builders the handlers use, for the fixture test and other crates.
pub mod build {
    pub use crate::routes::account::{
        listen_line, me_body, pair_result_body, qr_link, spaced_code,
    };
    pub use crate::routes::acquisition::{external_play_body, search_body};
    pub use crate::routes::addresses::urls_version;
    pub use crate::routes::admin::{
        admin_library, admin_user, audit_summary, invite_state, library_problems_for,
        overview_library, scan_detail_body,
    };
    pub use crate::routes::apple_music::{
        MAX_LIFETIME as APPLE_MUSIC_MAX_LIFETIME, TOKEN_LIFETIME as APPLE_MUSIC_TOKEN_LIFETIME,
        admin_key_body as apple_music_admin_body, parse_private_key as parse_apple_music_key,
        sign_with as sign_developer_token,
    };
    pub use crate::routes::cloud::{
        cloud_info, device_body, devices_body, page_body as cloud_page_body, push_body,
        record_body, setting_results_body, settings_body, usage_body,
    };
    pub use crate::routes::enrichment::status_body as enrichment_status_body;
    pub use crate::routes::link::{link_info_body, link_start_body};
    pub use crate::routes::media::{decision_body, download_name, reason_text};
    pub use crate::routes::public::capabilities_body;
    pub use crate::routes::settings::{LASTFM_API_KEY, apply_stored_auth, stored_secret};
    pub use crate::routes::social::{check_listen, listen_result};
    pub use crate::routes::stats::{
        DayBuckets, day_buckets, listening_body, requests_body, storage_body, streams_body,
    };
    pub use crate::routes::sync::page_body;
    pub use crate::routes::uploads::CHUNK_MAX;
}

pub use error::{ApiError, ApiResult, ErrorCode, device_link_error};
pub use extract::{RequestId, X_REQUEST_ID};
pub use qr::qr_svg;
pub use routes::Https;
pub use sse::{EventFilter, visible_event};

/// Facts about this server shown in capabilities, `/me`, pairing and the admin pages.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub server_id: uuid::Uuid,
    /// `server.name` from the configuration; a dashboard setting overrides it unless the
    /// configuration locks it.
    pub name: String,
    pub version: &'static str,
    pub started_at: Millis,
    /// `server.base_path`, normalised to `""` or `/prefix` without a trailing slash.
    pub base_path: String,
    /// LAN URLs the server detected. Configured public URLs are added per request.
    pub urls: UrlBook,
    /// Where each configuration key came from, to mark settings read-only.
    pub report: Arc<ConfigReport>,
    /// The SHA-256 fingerprint of a self-signed certificate, for pinning.
    pub fingerprint: Option<String>,
}

impl ServerInfo {
    /// Server facts with no detected URLs and an empty configuration report.
    pub fn new(server_id: uuid::Uuid, name: impl Into<String>, started_at: Millis) -> Self {
        Self {
            server_id,
            name: name.into(),
            version: env!("CARGO_PKG_VERSION"),
            started_at,
            base_path: String::new(),
            urls: UrlBook::default(),
            report: Arc::new(ConfigReport::default()),
            fingerprint: None,
        }
    }
}

/// Normalises `server.base_path`: `""`, `"/"` and `"music/"` become `""`, `""` and
/// `"/music"`.
pub fn normalize_base_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// The detected candidate URLs, shared with the server, which refreshes them when interfaces
/// change.
#[derive(Clone, Debug, Default)]
pub struct UrlBook(Arc<RwLock<Vec<wire::auth::ServerUrl>>>);

impl UrlBook {
    pub fn set(&self, urls: Vec<wire::auth::ServerUrl>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = urls;
    }

    pub fn get(&self) -> Vec<wire::auth::ServerUrl> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Why a server operation failed.
#[derive(Debug)]
pub enum OpsError {
    Busy,
    NotFound,
    NotEnoughSpace,
    Io(std::io::Error),
}

impl std::fmt::Display for OpsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("a backup is already running"),
            Self::NotFound => f.write_str("no such backup"),
            Self::NotEnoughSpace => f.write_str("not enough free space for a backup"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpsError {}

/// What the admin endpoints need from the server process itself: backups and URL checks.
pub trait ServerOps: Send + Sync + 'static {
    /// Every backup, newest first.
    fn backups(&self) -> BoxFuture<'_, Result<Vec<wire::admin::Backup>, OpsError>>;
    /// When the next scheduled backup runs, if one is scheduled.
    fn next_backup(&self) -> Option<Millis>;
    /// Starts a manual backup in the background. `Busy` while one runs.
    fn start_backup(&self) -> BoxFuture<'_, Result<(), OpsError>>;
    /// The file of a backup by name, when it exists. Names never contain a separator.
    fn backup_path(&self, name: &str) -> Option<PathBuf>;
    fn delete_backup<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), OpsError>>;
    /// Whether the server can reach its own `/api/v1/ping` at this URL.
    fn check_url<'a>(&'a self, url: &'a str) -> BoxFuture<'a, bool>;
}

/// [`ServerOps`] that has no backups and checks nothing, for tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOps;

impl ServerOps for NoOps {
    fn backups(&self) -> BoxFuture<'_, Result<Vec<wire::admin::Backup>, OpsError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn next_backup(&self) -> Option<Millis> {
        None
    }

    fn start_backup(&self) -> BoxFuture<'_, Result<(), OpsError>> {
        Box::pin(async { Ok(()) })
    }

    fn backup_path(&self, _name: &str) -> Option<PathBuf> {
        None
    }

    fn delete_backup<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<(), OpsError>> {
        Box::pin(async { Err(OpsError::NotFound) })
    }

    fn check_url<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
}

/// Where `/readyz` and the admin overview get the server's health.
pub trait HealthSource: Send + Sync + 'static {
    fn overview(&self) -> BoxFuture<'_, wire::admin::Overview>;
    fn ready(&self) -> wire::Ready;
}

/// What a user is playing, for the dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NowPlaying {
    pub song: SongId,
    pub position_ms: u64,
    pub state: String,
    pub at: Millis,
}

/// Media responses in flight and failed media requests, counted by the server's middleware
/// and sampled by its scheduler for `GET /admin/stats/streams`.
#[derive(Debug, Default)]
pub struct PlaybackLoad {
    active: std::sync::atomic::AtomicU32,
    peak: std::sync::atomic::AtomicU32,
    failed: std::sync::atomic::AtomicU32,
}

impl PlaybackLoad {
    /// A media response started; it ends when the guard is dropped.
    pub fn start(self: &Arc<Self>) -> StreamGuard {
        let now = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(now, Ordering::Relaxed);
        StreamGuard(Arc::clone(self))
    }

    pub fn fail(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn active(&self) -> u32 {
        self.active.load(Ordering::Relaxed)
    }

    /// The highest concurrency and the failures since the last call, resetting both.
    pub fn take(&self) -> (u32, u32) {
        let peak = self.peak.swap(self.active(), Ordering::Relaxed);
        (peak, self.failed.swap(0, Ordering::Relaxed))
    }
}

/// Holds one slot of [`PlaybackLoad`] while a media body is being sent.
#[derive(Debug)]
pub struct StreamGuard(Arc<PlaybackLoad>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A rendered QR code kept for its `qr_svg_url`, since the secret inside is never stored.
#[derive(Clone, Debug)]
struct QrEntry {
    owner: Option<UserKey>,
    svg: String,
    expires_at: Millis,
}

/// In-memory state of the API: QR images, the latest change seq seen, now playing, and the
/// shutdown signal that ends event streams.
#[derive(Debug, Default)]
pub struct LocalState {
    qr: Mutex<HashMap<String, QrEntry>>,
    /// HLS jobs started for instant play when the acquirer had none, so every playlist and
    /// part request for one acquisition reads the same job.
    instant_hls: tokio::sync::Mutex<
        HashMap<trackside_core::AcquisitionId, trackside_core::control::HlsJobId>,
    >,
    /// The same for songs played without being kept, per user.
    external_hls: tokio::sync::Mutex<
        HashMap<(UserKey, trackside_core::CandidateToken), trackside_core::control::HlsJobId>,
    >,
    latest_seq: AtomicI64,
    /// The version of the address list last announced over events.
    urls_version: Mutex<Option<String>>,
    now_playing: NowPlayingRegistry,
    playback: Arc<PlaybackLoad>,
    /// Trackside Cloud pushes per user.
    cloud_limiter: trackside_cloud::PushLimiter,
    /// The opened Apple Music key and the token it signed last.
    developer_tokens: routes::apple_music::DeveloperTokens,
    shutdown: CancellationToken,
}

impl LocalState {
    /// State whose event streams end when `shutdown` is cancelled.
    pub fn new(shutdown: CancellationToken) -> Self {
        Self {
            shutdown,
            ..Self::default()
        }
    }

    /// State sharing `now_playing` with the Subsonic router, so each side sees the other's
    /// players.
    pub fn with_now_playing(shutdown: CancellationToken, now_playing: NowPlayingRegistry) -> Self {
        Self {
            now_playing,
            shutdown,
            ..Self::default()
        }
    }

    /// Live media responses and failures, sampled into the hourly stats.
    pub fn playback(&self) -> &Arc<PlaybackLoad> {
        &self.playback
    }

    /// The per-user budget of Trackside Cloud pushes.
    pub fn cloud_limiter(&self) -> &trackside_cloud::PushLimiter {
        &self.cloud_limiter
    }

    pub(crate) fn developer_tokens(&self) -> &routes::apple_music::DeveloperTokens {
        &self.developer_tokens
    }

    /// The registry `POST /now-playing` reports to.
    pub fn now_playing_registry(&self) -> &NowPlayingRegistry {
        &self.now_playing
    }

    /// The highest change seq this process has seen committed.
    pub fn latest_seq(&self) -> i64 {
        self.latest_seq.load(Ordering::Relaxed)
    }

    pub fn note_seq(&self, seq: i64) {
        self.latest_seq.fetch_max(seq, Ordering::Relaxed);
    }

    pub(crate) fn instant_hls(
        &self,
    ) -> &tokio::sync::Mutex<
        HashMap<trackside_core::AcquisitionId, trackside_core::control::HlsJobId>,
    > {
        &self.instant_hls
    }

    pub(crate) fn external_hls(
        &self,
    ) -> &tokio::sync::Mutex<
        HashMap<(UserKey, trackside_core::CandidateToken), trackside_core::control::HlsJobId>,
    > {
        &self.external_hls
    }

    pub fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub(crate) fn urls_version(&self) -> &Mutex<Option<String>> {
        &self.urls_version
    }

    /// Everyone's current now-playing report that has not expired at `now`, the most recent
    /// per user.
    pub fn now_playing(&self, now: Millis) -> Vec<(UserKey, NowPlaying)> {
        let mut seen = std::collections::HashSet::new();
        self.now_playing
            .list(now)
            .into_iter()
            .filter(|entry| seen.insert(entry.user))
            .map(|entry| {
                let state = match entry.state {
                    PlayState::Playing => "playing",
                    PlayState::Paused => "paused",
                };
                (
                    entry.user,
                    NowPlaying {
                        song: entry.song,
                        position_ms: entry.position_ms,
                        state: String::from(state),
                        at: entry.updated_at,
                    },
                )
            })
            .collect()
    }

    fn put_qr(&self, key: String, owner: Option<UserKey>, svg: String, expires_at: Millis) {
        let mut map = self.qr.lock().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, e| e.expires_at > expires_at.saturating_sub(QR_KEEP));
        map.insert(
            key,
            QrEntry {
                owner,
                svg,
                expires_at,
            },
        );
    }

    fn qr(&self, key: &str, owner: Option<UserKey>, now: Millis) -> Option<String> {
        let map = self.qr.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(key)
            .filter(|e| e.expires_at > now && (e.owner.is_none() || e.owner == owner))
            .map(|e| e.svg.clone())
    }

    fn drop_qr(&self, key: &str) {
        self.qr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

/// Expired QR images are dropped once they are this old.
const QR_KEEP: std::time::Duration = std::time::Duration::from_hours(8 * 24);

/// What the `/api/v1` handlers need.
#[derive(Clone)]
pub struct ApiState {
    pub db: Db,
    pub auth: Arc<Auth>,
    pub media: Arc<Media>,
    pub scan: Arc<dyn ScanControl>,
    pub acquire: Arc<dyn AcquireControl>,
    /// Last.fm lookups for suggestions.
    pub enrich: Arc<dyn EnrichControl>,
    pub events: EventBus,
    pub config: Arc<Config>,
    pub server: ServerInfo,
    pub health: Arc<dyn HealthSource>,
    /// Backups and URL checks, implemented by the server.
    pub ops: Arc<dyn ServerOps>,
    pub local: Arc<LocalState>,
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiState")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl FromRef<ApiState> for Arc<Auth> {
    fn from_ref(state: &ApiState) -> Self {
        Arc::clone(&state.auth)
    }
}

/// The native API routes with their full paths (`/api/v1/...`), merged into the server's
/// router. Also follows the event bus to know the latest change seq.
pub fn router(state: ApiState) -> Router {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(follow_seq(state.events.clone(), Arc::clone(&state.local)));
        runtime.spawn(routes::addresses::watch(state.clone()));
    }
    routes::router(state)
}

async fn follow_seq(events: EventBus, local: Arc<LocalState>) {
    let mut receiver = events.subscribe();
    loop {
        tokio::select! {
            () = local.shutdown.cancelled() => return,
            event = receiver.recv() => match event {
                Ok(event) => {
                    if let ServerEvent::Changed { seq, .. } = &*event {
                        local.note_seq(seq.0);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_paths_are_normalised() {
        assert_eq!(normalize_base_path(""), "");
        assert_eq!(normalize_base_path("/"), "");
        assert_eq!(normalize_base_path("music/"), "/music");
        assert_eq!(normalize_base_path("/a/b/"), "/a/b");
    }

    #[test]
    fn qr_images_are_only_for_their_owner_until_expiry() {
        let local = LocalState::default();
        local.put_qr(
            String::from("pair:g1"),
            Some(UserKey(1)),
            String::from("<svg/>"),
            Millis(1_000),
        );
        assert_eq!(
            local.qr("pair:g1", Some(UserKey(1)), Millis(10)),
            Some(String::from("<svg/>"))
        );
        assert_eq!(local.qr("pair:g1", Some(UserKey(2)), Millis(10)), None);
        assert_eq!(local.qr("pair:g1", Some(UserKey(1)), Millis(1_000)), None);
        local.drop_qr("pair:g1");
        assert_eq!(local.qr("pair:g1", Some(UserKey(1)), Millis(10)), None);
    }

    #[test]
    fn seqs_only_move_forward() {
        let local = LocalState::default();
        local.note_seq(7);
        local.note_seq(3);
        assert_eq!(local.latest_seq(), 7);
    }
}
