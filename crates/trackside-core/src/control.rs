//! Traits the HTTP crates use to reach the scanner and the acquirer without depending on them.
//! `trackside-scan` implements [`ScanControl`], `trackside-acquire` implements
//! [`AcquireControl`], and `trackside-server` wires them in.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::access::{Access, Permissions};
use crate::config::Secret;
use crate::db::DbError;
use crate::events::ScanProgress;
use crate::ids::{AcquisitionId, CandidateToken, ImportId, LibraryId, ScanRunId, SongId, UserId};
use crate::records::{Codec, Container, ScanRun, Song, text_enum};
use crate::time::Millis;

/// A boxed, sendable future, for the object-safe async traits.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// How thorough a scan is: quick skips unchanged folders and files, full rereads every tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanKind {
    Quick,
    Full,
}

text_enum!(
    /// What started a scan, as stored in `scan_runs.trigger`.
    ScanTrigger {
        Startup => "startup",
        Schedule => "schedule",
        Watch => "watch",
        Admin => "admin",
        Import => "import",
        Cli => "cli",
    }
);

/// How a file entered the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SongOrigin {
    Scan,
    Upload,
    Acquired,
}

impl SongOrigin {
    /// The value stored in `songs.origin`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Upload => "upload",
            Self::Acquired => "acquired",
        }
    }
}

/// The result of importing one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportOutcome {
    /// A new song with its final, stable id.
    Added(SongId),
    /// The library already had this audio; the existing song's id.
    Existing(SongId),
}

/// How a library notices changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchMode {
    Events,
    Polling,
    Off,
}

/// The scanner's view of one library, for the dashboard and `/readyz`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryScanStatus {
    pub library: LibraryId,
    pub available: bool,
    /// The running scan, if any.
    pub scanning: Option<ScanProgress>,
    pub last_run: Option<ScanRun>,
    pub watching: WatchMode,
}

/// Why the scanner refused or failed a request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanControlError {
    #[error("no such library")]
    UnknownLibrary,
    #[error("library is unavailable")]
    Unavailable,
    #[error("library is not writable")]
    NotWritable,
    #[error("no such import")]
    UnknownImport,
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("shutting down")]
    ShuttingDown,
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

/// The scanner as the HTTP crates and the acquirer see it.
pub trait ScanControl: Send + Sync + 'static {
    /// Queues a scan. `force_empty` overrides the empty-library guard.
    fn request_scan(
        &self,
        library: &LibraryId,
        kind: ScanKind,
        trigger: ScanTrigger,
        force_empty: bool,
    ) -> Result<ScanRunId, ScanControlError>;

    /// Queues a scan of one folder of `library` (relative path) and everything below it.
    /// Implementations without folder scans run a quick scan of the whole library.
    fn request_folder_scan(
        &self,
        library: &LibraryId,
        folder: &Path,
    ) -> Result<ScanRunId, ScanControlError> {
        let _ = folder;
        self.request_scan(library, ScanKind::Quick, ScanTrigger::Watch, false)
    }

    /// Indexes one file already inside `library` at `rel_path`, returning its final song id.
    fn import_file<'a>(
        &'a self,
        library: &'a LibraryId,
        rel_path: &'a Path,
        origin: SongOrigin,
    ) -> BoxFuture<'a, Result<ImportOutcome, ScanControlError>>;

    /// Verifies and imports a completed upload.
    fn process_upload<'a>(
        &'a self,
        import: &'a ImportId,
    ) -> BoxFuture<'a, Result<(), ScanControlError>>;

    /// Every library's scan state.
    fn status(&self) -> Vec<LibraryScanStatus>;

    /// Whether the availability monitor currently sees the library.
    fn is_available(&self, library: &LibraryId) -> bool;
}

text_enum!(
    /// Where an acquisition is (api.md section 15.1).
    AcquisitionState {
        Queued => "queued",
        Searching => "searching",
        Downloading => "downloading",
        Verifying => "verifying",
        Importing => "importing",
        Done => "done",
        Failed => "failed",
        Cancelled => "cancelled",
    }
);

impl AcquisitionState {
    /// Done, failed or cancelled.
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

text_enum!(
    /// Why an acquisition failed (api.md section 15.1).
    AcquireFailure {
        NoMatch => "no_match",
        AllCandidatesFailed => "all_candidates_failed",
        Exhausted => "exhausted",
        ProviderUnavailable => "provider_unavailable",
        InsufficientSpace => "insufficient_space",
        RejectedFile => "rejected_file",
        QuotaExceeded => "quota_exceeded",
        Disabled => "disabled",
    }
);

text_enum!(
    /// Quality class: lossless, lossy at 256 kbps or more, or below.
    Quality {
        Lossless => "lossless",
        High => "high",
        Standard => "standard",
    }
);

/// Relative URLs for playing an acquisition while it downloads. The client appends `mt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstantStreamUrls {
    pub hls: String,
    pub progressive: String,
}

/// The view Tracks sees: no provider names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcquisitionView {
    pub id: AcquisitionId,
    pub state: AcquisitionState,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// Bytes so far and the total when known.
    pub progress: Option<(u64, Option<u64>)>,
    pub quality: Option<Quality>,
    pub song: Option<SongId>,
    pub existing: bool,
    pub failure: Option<AcquireFailure>,
    pub stream: Option<InstantStreamUrls>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

/// `features.acquisition` in `GET /api/v1/me`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcquireFeature {
    pub instant_play: bool,
}

/// What to search sources for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalQuery {
    Text(String),
    Fields {
        title: String,
        artist: String,
        album: Option<String>,
        duration_ms: Option<u32>,
    },
}

/// A candidate's format, as far as the source says.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CandidateFormat {
    pub container: Option<Container>,
    pub codec: Option<Codec>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
}

/// A search result offered to Tracks: no provider names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateView {
    pub token: CandidateToken,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub quality: Quality,
    pub format: CandidateFormat,
    pub size: Option<u64>,
    pub instant: bool,
    /// `match: exact` when true, `close` otherwise.
    pub exact: bool,
}

/// A search across enabled sources.
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalSearch {
    pub search_id: String,
    pub complete: bool,
    /// Library songs with the same `song_key`.
    pub in_library: Vec<Song>,
    pub candidates: Vec<CandidateView>,
}

/// `POST /api/v1/acquisitions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcquireRequest {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub candidate: Option<CandidateToken>,
    pub allow_versions: bool,
    pub instant: bool,
}

/// Which requests to list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquisitionStateFilter {
    Active,
    Finished,
    Exactly(AcquisitionState),
}

/// A filter for listing acquisitions. `user: None` lists everyone's (admin views only).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AcquisitionFilter {
    pub user: Option<crate::ids::UserKey>,
    pub state: Option<AcquisitionStateFilter>,
    pub cursor: Option<String>,
    pub limit: u32,
}

/// A provider's health as the dashboard shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderHealthState {
    Ok,
    Degraded,
    Cooldown,
    Outdated,
    Unavailable,
    Disabled,
}

/// Today's counters for one provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ProviderStats {
    pub searches_today: u32,
    pub fetches_today: u32,
    pub failures_today: u32,
}

/// `ProviderStatus` of api.md section 16. Admin only: it names the provider.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderStatus {
    pub name: String,
    pub enabled: bool,
    pub configured: bool,
    pub health: ProviderHealthState,
    pub detail: Option<String>,
    pub cooldown_until: Option<Millis>,
    pub version: Option<String>,
    pub latest_known: Option<String>,
    pub settings: serde_json::Value,
    pub stats: ProviderStats,
}

/// A change from the dashboard's acquisition pages.
#[derive(Clone, Debug)]
pub enum ProviderChange {
    /// `PATCH /admin/acquire/providers/{name}`.
    Provider {
        name: String,
        enabled: Option<bool>,
        concurrency: Option<u32>,
        url: Option<String>,
        api_key: Option<Secret>,
    },
    /// `PATCH /admin/acquire`.
    Global {
        enabled: Option<bool>,
        instant_play: Option<bool>,
        member_daily: Option<u32>,
        member_active: Option<u32>,
        global_daily: Option<u32>,
        prefer: Option<Vec<String>>,
    },
}

/// The result of `POST /admin/acquire/providers/{name}/test`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderTest {
    pub ok: bool,
    pub elapsed: Duration,
    pub candidates: u32,
    pub detail: Option<String>,
}

/// A candidate as admins see it, with its source.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminCandidate {
    pub token: CandidateToken,
    pub provider: String,
    pub provider_ref: serde_json::Value,
    pub title: String,
    pub artist: String,
    pub score: f32,
    pub quality: Quality,
    pub format: CandidateFormat,
    pub size: Option<u64>,
    pub tried_at: Option<Millis>,
    pub failure: Option<String>,
}

/// An acquisition as admins see it.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminAcquisitionView {
    pub view: AcquisitionView,
    pub user: Option<UserId>,
    pub provider: Option<String>,
    pub chosen: Option<AdminCandidate>,
    pub attempts: u32,
    pub failure_detail: Option<String>,
    pub candidates: Vec<AdminCandidate>,
}

/// Why an acquisition call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AcquireError {
    /// Unknown id, or acquisition is not available to this caller (answered as `404`).
    #[error("not found")]
    NotFound,
    #[error("acquisition is disabled")]
    Disabled,
    #[error("quota exceeded until {resets_at:?}")]
    QuotaExceeded { resets_at: Millis },
    #[error("candidate expired")]
    CandidateExpired,
    #[error("invalid state")]
    InvalidState,
    #[error("rate limited")]
    RateLimited { retry_after: Duration },
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("busy")]
    Busy,
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

/// A file that is still being written, shared between a fetch and the HTTP layer.
#[derive(Clone, Debug)]
pub struct GrowingFile {
    pub path: PathBuf,
    pub state: tokio::sync::watch::Receiver<GrowState>,
}

/// How far a growing file has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowState {
    Growing { bytes: u64 },
    Complete { bytes: u64 },
    Failed,
}

/// The id of an HLS transcode job.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HlsJobId(pub Box<str>);

/// An acquisition that can be played while it downloads.
#[derive(Clone, Debug)]
pub struct InstantStream {
    pub progressive: GrowingFile,
    pub hls_job: Option<HlsJobId>,
    pub content_type: &'static str,
    pub duration_ms: Option<u32>,
}

/// The acquirer as the HTTP crates see it.
pub trait AcquireControl: Send + Sync + 'static {
    /// The acquired library may have changed (the target setting, or libraries were added or
    /// removed); re-read it.
    fn targets_changed(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Some when `features.acquisition` should be present for this caller.
    fn feature_for(
        &self,
        caller: &Access,
        permissions: &Permissions,
        scopes_include_request: bool,
    ) -> Option<AcquireFeature>;

    fn search<'a>(
        &'a self,
        caller: &'a Access,
        query: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>>;

    fn search_more<'a>(
        &'a self,
        caller: &'a Access,
        search: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>>;

    fn request<'a>(
        &'a self,
        caller: &'a Access,
        request: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>>;

    fn get<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>>;

    /// The caller's requests. The filter's `user` is replaced by the caller's.
    fn list<'a>(
        &'a self,
        caller: &'a Access,
        filter: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<crate::records::Page<AcquisitionView>, AcquireError>>;

    fn cancel<'a>(
        &'a self,
        caller: &'a Access,
        id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>>;

    fn instant_stream(
        &self,
        caller: &Access,
        id: &AcquisitionId,
    ) -> Result<InstantStream, AcquireError>;

    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>>;

    fn configure(&self, change: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>>;

    fn test_provider<'a>(
        &'a self,
        name: &'a str,
        query: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>>;

    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>>;

    fn retry<'a>(&'a self, id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>>;

    fn admin_list(
        &self,
        filter: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<crate::records::Page<AdminAcquisitionView>, AcquireError>>;
}
