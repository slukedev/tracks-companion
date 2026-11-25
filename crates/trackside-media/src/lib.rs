//! Media: range serving of original files and growing files, playback decisions, ffmpeg
//! transcoding with a single-flight job registry and an LRU cache, HLS, and artwork.
//!
//! Media receives already authorised requests; it never decides who may hear what.

mod artwork;
mod cache;
mod decide;
mod ffmpeg;
mod gate;
mod hls;
mod jobs;
mod probe;
mod range;
mod serve;
mod slots;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Body;
use http::Response;
use http::request::Parts;
use tokio_util::sync::CancellationToken;
use trackside_core::config::MediaConfig;
pub use trackside_core::control::{GrowState, GrowingFile, HlsJobId};
use trackside_core::records::{AudioFormat, Codec, Container, SongFile};
use trackside_core::{ArtworkId, BoxFuture, Clock, Db, DbError, Millis, SongId, UserKey};

pub use artwork::{
    ART_SIZES, ArtworkMeta, ArtworkSource, ArtworkSourceKind, ArtworkStore, MAX_DIMENSION,
    MAX_ORIGINAL_BYTES, snap_size,
};
pub use cache::{FreeSpace, FsFreeSpace};
pub use ffmpeg::{FfmpegTranscoder, ffmpeg_args};
pub use range::{RangeOutcome, evaluate};

use crate::cache::{CacheKey, TranscodeCache};
use crate::gate::LibraryGate;
use crate::serve::{Entity, respond_file, system_time_from_ns};
use crate::slots::Slots;

/// Why a media operation failed. The HTTP layer maps these to api.md error codes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MediaError {
    #[error("not found")]
    NotFound,
    /// The file changed on disk since it was indexed (`409 content_changed`).
    #[error("content changed since indexing")]
    ContentChanged,
    #[error("range not satisfiable")]
    RangeNotSatisfiable { len: u64 },
    /// No transcode slot in time (`503 transcoder_busy` with `Retry-After`).
    #[error("transcoder busy")]
    TranscoderBusy,
    #[error("no transcoder available")]
    NoTranscoder,
    /// An HLS segment was not produced in time (`503` with `Retry-After: 1`).
    #[error("timed out waiting for the transcoder")]
    Timeout,
    /// The HLS job is unknown or expired (`404`; the client asks for a new decision).
    #[error("transcode job expired")]
    JobExpired,
    #[error("too large")]
    TooLarge,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("transcoder failed: {0}")]
    Process(String),
    /// The cache volume is too full to write a transcode (`507 insufficient_storage`).
    #[error("not enough free space for the transcode cache")]
    InsufficientStorage,
    /// The library's filesystem is not answering: every permit for blocking calls on its
    /// paths is held by a call that has not returned, as on a hung NFS mount
    /// (`503 library_unavailable` with `Retry-After`).
    #[error("library unavailable")]
    LibraryUnavailable,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

/// A transcode profile (api.md section 5 `media.profiles`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    Aac256,
    Aac128,
    Mp3_320,
    Mp3_128,
    Opus128,
    Flac,
}

impl Profile {
    pub const ALL: [Self; 6] = [
        Self::Aac256,
        Self::Aac128,
        Self::Mp3_320,
        Self::Mp3_128,
        Self::Opus128,
        Self::Flac,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str().eq_ignore_ascii_case(s))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aac256 => "aac-256",
            Self::Aac128 => "aac-128",
            Self::Mp3_320 => "mp3-320",
            Self::Mp3_128 => "mp3-128",
            Self::Opus128 => "opus-128",
            Self::Flac => "flac",
        }
    }

    /// Target bitrate, or `None` for lossless.
    pub fn bitrate_kbps(self) -> Option<u32> {
        match self {
            Self::Aac256 => Some(256),
            Self::Aac128 | Self::Mp3_128 | Self::Opus128 => Some(128),
            Self::Mp3_320 => Some(320),
            Self::Flac => None,
        }
    }

    /// The container and codec the profile produces as a file.
    pub fn output(self) -> (Container, Codec) {
        match self {
            Self::Aac256 | Self::Aac128 => (Container::M4a, Codec::Aac),
            Self::Mp3_320 | Self::Mp3_128 => (Container::Mp3, Codec::Mp3),
            Self::Opus128 => (Container::Opus, Codec::Opus),
            Self::Flac => (Container::Flac, Codec::Flac),
        }
    }
}

/// The sample rate the AAC and MP3 profiles resample to (ffmpeg's `aformat` picks one of
/// these), as reported to clients.
fn lossy_rate(source: Option<u32>) -> u32 {
    match source {
        Some(rate) if rate > 44_100 => 48_000,
        _ => 44_100,
    }
}

/// The format a profile delivers for a source file.
pub(crate) fn profile_format(profile: Profile, file: &SongFile, size: u64) -> AudioFormat {
    let (container, codec) = profile.output();
    let source = &file.format;
    let sample_rate = match profile {
        Profile::Opus128 => Some(48_000),
        Profile::Flac => source.sample_rate,
        _ => Some(lossy_rate(source.sample_rate)),
    };
    AudioFormat {
        container,
        codec,
        content_type: container.content_type(),
        lossless: profile == Profile::Flac,
        bitrate_kbps: profile.bitrate_kbps(),
        sample_rate,
        bit_depth: (profile == Profile::Flac)
            .then_some(source.bit_depth)
            .flatten(),
        channels: source.channels,
        size,
    }
}

/// Whether a response is played inline or saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    Inline,
    Attachment { file_name: String },
}

/// One container a client plays directly, with the codecs it accepts in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectPlay {
    pub container: Container,
    pub codecs: Vec<Codec>,
}

/// What a client can play (api.md section 11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientProfile {
    pub direct: Vec<DirectPlay>,
    pub hls: bool,
    pub max_bitrate_kbps: Option<u32>,
    pub preferred_profile: Option<Profile>,
    pub allow_original_fallback: bool,
}

/// Why a decision is not plain direct play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionReason {
    OverBitrate,
    UnsupportedCodec,
    TranscoderBusy,
    NoTranscoder,
}

/// A complete transcode in the cache, range-servable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedTranscode {
    pub path: PathBuf,
    pub size: u64,
    /// Strong and quoted: `"<pid>.<content_version>.<profile>.<encoder id>"`.
    pub etag: String,
    pub profile: Profile,
    pub format: AudioFormat,
}

/// How to deliver a song to a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The original file; `reason` is `TranscoderBusy` or `NoTranscoder` when falling back.
    Direct { reason: Option<DecisionReason> },
    /// A complete cached transcode.
    File {
        transcode: CachedTranscode,
        reason: DecisionReason,
    },
    /// Transcode with HLS; the handler calls [`Media::start_hls`]. A client without HLS
    /// support gets this too; its handler awaits [`Media::ensure_cached`] at interactive
    /// priority and answers with a file instead.
    Hls {
        profile: Profile,
        reason: DecisionReason,
    },
}

/// Transcode job priority, highest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Interactive,
    Prefetch,
    Download,
}

/// What `POST /api/v1/transcodes/prepare` reports per song.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareStateKind {
    Cached,
    Queued,
    Running,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareState {
    pub song: SongId,
    pub state: PrepareStateKind,
}

/// Who an HLS job is for. Only that user (and admins) may fetch its parts, and `hold` (a
/// per-user transcode slot) is kept until the encoder stops.
#[derive(Default)]
pub struct HlsOwner {
    pub user: Option<UserKey>,
    pub hold: Option<Box<dyn std::any::Any + Send + Sync>>,
}

impl std::fmt::Debug for HlsOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlsOwner")
            .field("user", &self.user)
            .field("hold", &self.hold.is_some())
            .finish()
    }
}

/// Whose an HLS job is and what it plays, for the HTTP layer's access check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsJobInfo {
    pub user: Option<UserKey>,
    /// The song, for an indexed file; `None` while the file is still downloading.
    pub song: Option<SongId>,
}

/// Keeps `guard` alive until the response body has been sent or dropped, such as a stream
/// slot that should be held for as long as the client is reading.
pub fn hold<G: Send + Sync + Unpin + 'static>(
    response: Response<Body>,
    guard: G,
) -> Response<Body> {
    response.map(|body| Body::new(serve::Held::new(body, guard)))
}

/// What can be HLS-transcoded: an indexed file or one still downloading.
#[derive(Clone, Debug)]
pub enum HlsInput {
    File(SongFile),
    Growing { file: GrowingFile, duration_ms: u32 },
}

/// A part of an HLS stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlsPart {
    Init,
    Segment(u32),
}

/// What media can do on this host, for capabilities and `/readyz`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaCapabilities {
    /// ffmpeg's version when found.
    pub ffmpeg: Option<String>,
    pub profiles: Vec<Profile>,
    pub hls: bool,
    pub suggested_cellular_profile: Option<Profile>,
}

/// Audio properties of a file, from lofty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub container: Container,
    pub codec: Codec,
    pub duration_ms: u32,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub has_video: bool,
}

/// The result of decoding a whole file with `ffmpeg -f null`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeCheck {
    pub ok: bool,
    pub decoded_ms: Option<u32>,
    pub message: Option<String>,
}

/// The input of a transcode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscodeInput {
    Path(PathBuf),
    /// Bytes written by the caller, for growing files.
    Stdin,
}

/// Where a transcode writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TranscodeOutput {
    File(PathBuf),
    Hls {
        dir: PathBuf,
        segment: Duration,
        start_number: u32,
    },
    Stdout,
    /// Copy the input's audio into an M4A without re-encoding (finishing an HLS run).
    Remux(PathBuf),
    /// Decode and discard, for checks; progress goes to stdout.
    Null,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscodeSpec {
    pub input: TranscodeInput,
    pub profile: Profile,
    pub output: TranscodeOutput,
    pub offset: Option<Duration>,
}

/// A running transcode. Take stdin and stdout before calling `wait`; dropping the future
/// `wait` returns must stop the process.
pub trait TranscodeProcess: Send {
    fn stdin(&mut self) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>;
    fn stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;
    fn wait(self: Box<Self>) -> BoxFuture<'static, Result<(), MediaError>>;
    fn kill(&mut self);
}

/// The seam for tests: the real one runs ffmpeg, a fake writes deterministic bytes.
pub trait Transcoder: Send + Sync + std::fmt::Debug + 'static {
    fn version(&self) -> Option<String>;
    fn spawn(&self, spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError>;

    /// A short id for the encoder build, part of cache file names and `ETags`.
    fn encoder_id(&self) -> Option<String> {
        self.version()
            .map(|v| blake3::hash(v.as_bytes()).to_hex()[..6].to_owned())
    }

    /// The profiles this transcoder can produce.
    fn profiles(&self) -> Vec<Profile> {
        if self.version().is_some() {
            Profile::ALL.to_vec()
        } else {
            Vec::new()
        }
    }
}

/// Failures within this window count toward holding a song back.
const FAILURE_HOLD: Duration = Duration::from_secs(3600);
const FAILURES_TO_HOLD: u32 = 3;
/// An interactive request waits this long for a slot before it is "busy".
pub(crate) const INTERACTIVE_WAIT: Duration = Duration::from_secs(2);

/// State shared by `Media` and its background tasks.
#[derive(Debug)]
pub(crate) struct Shared {
    pub config: MediaConfig,
    pub transcoder: Arc<dyn Transcoder>,
    pub clock: Arc<dyn Clock>,
    pub profiles: Vec<Profile>,
    pub slots: Slots,
    transcode_limit: usize,
    pub cache: Option<TranscodeCache>,
    pub hls_dir: PathBuf,
    pub jobs: Mutex<HashMap<CacheKey, Arc<jobs::FileJob>>>,
    pub hls: Mutex<HashMap<HlsJobId, Arc<hls::HlsJob>>>,
    pub failures: Mutex<HashMap<(SongId, i64), (u32, Millis)>>,
    pub shutdown: CancellationToken,
}

impl Shared {
    pub(crate) fn record_failure(&self, file: &SongFile) {
        let now = self.clock.now();
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if failures.len() > 1024 {
            failures.retain(|_, (_, last)| now.since(*last) <= FAILURE_HOLD);
        }
        let entry = failures
            .entry((file.song.clone(), file.content_version))
            .or_insert((0, now));
        if now.since(entry.1) > FAILURE_HOLD {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.1 = now;
    }

    pub(crate) fn is_failing(&self, file: &SongFile) -> bool {
        let now = self.clock.now();
        let failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        failures
            .get(&(file.song.clone(), file.content_version))
            .is_some_and(|(count, last)| {
                *count >= FAILURES_TO_HOLD && now.since(*last) <= FAILURE_HOLD
            })
    }
}

/// Range serving, transcoding, HLS and artwork.
pub struct Media {
    shared: Arc<Shared>,
    db: Db,
    artwork: ArtworkStore,
    libraries: LibraryGate,
}

impl std::fmt::Debug for Media {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Media")
            .field("transcoder", &self.shared.transcoder)
            .field("hls_dir", &self.shared.hls_dir)
            .finish_non_exhaustive()
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.shared.shutdown.cancel();
    }
}

fn file_changed(meta: &std::fs::Metadata, file: &SongFile) -> bool {
    meta.len() != file.size || serve::mtime_ns(meta) != file.mtime_ns
}

impl Media {
    /// Media with the cache under `cache_dir` (`transcodes/`, `hls/`, `art/`).
    pub fn new(
        config: &MediaConfig,
        cache_dir: &Path,
        db: Db,
        transcoder: Arc<dyn Transcoder>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, MediaError> {
        Self::with_free_space(
            config,
            cache_dir,
            db,
            transcoder,
            clock,
            Arc::new(FsFreeSpace),
        )
    }

    /// As [`Media::new`], with the free-space source used for the cache floor.
    pub fn with_free_space(
        config: &MediaConfig,
        cache_dir: &Path,
        db: Db,
        transcoder: Arc<dyn Transcoder>,
        clock: Arc<dyn Clock>,
        free_space: Arc<dyn FreeSpace>,
    ) -> Result<Self, MediaError> {
        let encoder = transcoder.encoder_id();
        let profiles = if encoder.is_some() {
            transcoder.profiles()
        } else {
            Vec::new()
        };
        let cache = match &encoder {
            Some(encoder) => Some(TranscodeCache::open(
                cache_dir.join("transcodes"),
                encoder,
                config.transcode_cache_max,
                config.cache_min_free,
                free_space,
            )?),
            None => {
                tracing::warn!("ffmpeg not found: direct play only, no transcoding");
                None
            }
        };
        let hls_dir = cache_dir.join("hls");
        if hls_dir.exists() {
            std::fs::remove_dir_all(&hls_dir)?;
        }
        std::fs::create_dir_all(&hls_dir)?;
        let shared = Arc::new(Shared {
            config: config.clone(),
            slots: Slots::new(config.transcode_max_concurrent),
            transcode_limit: config.transcode_max_concurrent.max(1),
            transcoder,
            clock,
            profiles,
            cache,
            hls_dir,
            jobs: Mutex::default(),
            hls: Mutex::default(),
            failures: Mutex::default(),
            shutdown: CancellationToken::new(),
        });
        Ok(Self {
            shared,
            db,
            artwork: ArtworkStore::new(cache_dir.join("art"), config.art_cache_max),
            libraries: LibraryGate::new(gate::LIBRARY_PERMITS, gate::LIBRARY_WAIT),
        })
    }

    /// ffmpeg found, its version, the profiles and HLS.
    pub fn capabilities(&self) -> MediaCapabilities {
        let ffmpeg = self
            .shared
            .cache
            .as_ref()
            .and_then(|_| self.shared.transcoder.version());
        let profiles = self.shared.profiles.clone();
        let hls = profiles.contains(&Profile::Aac256);
        MediaCapabilities {
            ffmpeg,
            suggested_cellular_profile: Profile::parse(&self.shared.config.cellular_profile)
                .filter(|_| hls),
            profiles,
            hls,
        }
    }

    /// Stops every transcode and HLS job. Called on shutdown; dropping `Media` does the same.
    pub fn shutdown(&self) {
        self.shared.shutdown.cancel();
    }

    /// Serves an original file with ranges and conditional requests, after checking it still
    /// matches the index. A file that changed or vanished is [`MediaError::ContentChanged`];
    /// the caller queues a quick scan of its folder.
    pub async fn serve_file(
        &self,
        file: &SongFile,
        request: &Parts,
        disposition: Disposition,
    ) -> Result<Response<Body>, MediaError> {
        // Open and stat behind the library's semaphore, so a hung mount cannot take the
        // blocking pool; the reads that follow are bounded by the stream limits.
        let path = file.path.clone();
        let opened = self
            .libraries
            .run(file.library, move || {
                let handle = std::fs::File::open(&path)?;
                let meta = handle.metadata()?;
                Ok((handle, meta))
            })
            .await;
        let (handle, meta) = match opened {
            Ok(opened) => opened,
            Err(MediaError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MediaError::ContentChanged);
            }
            Err(e) => return Err(e),
        };
        let handle = tokio::fs::File::from_std(handle);
        if file_changed(&meta, file) {
            return Err(MediaError::ContentChanged);
        }
        let etag = format!("\"{}.{}\"", file.song, file.content_version);
        let entity = Entity {
            len: meta.len(),
            etag: &etag,
            last_modified: system_time_from_ns(file.mtime_ns),
            content_type: file.format.content_type,
            cache_control: "private, no-cache",
            disposition: Some(&disposition),
        };
        respond_file(handle, &entity, request).await
    }

    /// Serves a complete cached transcode exactly like an original: ranges, `If-Range`,
    /// strong `ETag`, `HEAD`. A cache file that disappeared is [`MediaError::ContentChanged`]
    /// and is forgotten, so the next decision transcodes again.
    pub async fn serve_transcode(
        &self,
        transcode: &CachedTranscode,
        request: &Parts,
        disposition: Disposition,
    ) -> Result<Response<Body>, MediaError> {
        let handle = match tokio::fs::File::open(&transcode.path).await {
            Ok(handle) => handle,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let (Some(cache), Some(key)) =
                    (&self.shared.cache, cache::key_from_path(&transcode.path))
                {
                    cache.forget(&key);
                }
                return Err(MediaError::ContentChanged);
            }
            Err(e) => return Err(e.into()),
        };
        let meta = handle.metadata().await?;
        let entity = Entity {
            len: meta.len(),
            etag: &transcode.etag,
            last_modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            content_type: transcode.format.content_type,
            cache_control: "private, no-cache",
            disposition: Some(&disposition),
        };
        respond_file(handle, &entity, request).await
    }

    /// Serves a file that is still being written: `200` without length, no ranges, until
    /// complete; then the whole file with ranges and a strong `ETag`.
    pub async fn serve_growing(
        &self,
        file: &GrowingFile,
        request: &Parts,
        content_type: &'static str,
    ) -> Result<Response<Body>, MediaError> {
        jobs::serve_growing(file, request, content_type).await
    }

    /// Chooses direct play, a cached file or HLS for this client and user limit.
    pub fn decide(
        &self,
        file: &SongFile,
        client: &ClientProfile,
        user_limit_kbps: Option<u32>,
    ) -> Decision {
        let shared = &self.shared;
        let cached = |profile: Profile| shared.cache.as_ref().and_then(|c| c.get(file, profile));
        let ctx = decide::DecideContext {
            profiles: &shared.profiles,
            default_profile: Profile::parse(&shared.config.cellular_profile)
                .unwrap_or(Profile::Aac256),
            cached: &cached,
            busy: shared.slots.free() == 0,
            failing: shared.is_failing(file),
        };
        decide::decide(file, client, user_limit_kbps, &ctx)
    }

    /// The complete cached transcode, if there is one.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async in the shared signature so callers need not change if this starts to wait"
    )]
    pub async fn cached(&self, file: &SongFile, profile: Profile) -> Option<CachedTranscode> {
        self.shared.cache.as_ref()?.get(file, profile)
    }

    /// Transcodes into the cache (joining a running job) and waits for it. Interactive
    /// callers get [`MediaError::TranscoderBusy`] when no slot frees up within 2 s.
    pub async fn ensure_cached(
        &self,
        file: &SongFile,
        profile: Profile,
        priority: Priority,
    ) -> Result<CachedTranscode, MediaError> {
        jobs::ensure_cached(&self.shared, file, profile, priority).await
    }

    /// [`Self::ensure_cached`], calling `admit` (to take the caller's transcode slot, say)
    /// only when this request starts the encoder, and holding what it returns until the
    /// transcode is done. Callers who join a cached or running transcode need no slot, and of
    /// several asking at once exactly one is admitted.
    pub async fn ensure_cached_admitting<G: Default, E: From<MediaError>>(
        &self,
        file: &SongFile,
        profile: Profile,
        priority: Priority,
        admit: impl FnOnce() -> Result<G, E>,
    ) -> Result<CachedTranscode, E> {
        jobs::ensure_cached_admitting(&self.shared, file, profile, priority, admit).await
    }

    /// Starts prefetch-priority transcodes for at most 5 songs. Songs that cannot be
    /// transcoded at all are left out.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async in the shared signature so callers need not change if this starts to wait"
    )]
    pub async fn prepare(&self, files: &[SongFile], profile: Profile) -> Vec<PrepareState> {
        jobs::prepare(&self.shared, files, profile)
    }

    /// Starts an HLS job. Interactive jobs that get no slot within 2 s fail with
    /// [`MediaError::TranscoderBusy`].
    pub async fn start_hls(
        &self,
        input: HlsInput,
        profile: Profile,
        priority: Priority,
        owner: HlsOwner,
    ) -> Result<HlsJobId, MediaError> {
        hls::start(&self.shared, input, profile, priority, owner).await
    }

    /// How many encoder runs an HLS job has started, for tests.
    #[cfg(any(test, feature = "testing"))]
    pub fn hls_run_count(&self, job: &HlsJobId) -> Option<usize> {
        hls::run_count(&self.shared, job)
    }

    /// Whose a live HLS job is, or `None` once it has expired.
    pub fn hls_job(&self, job: &HlsJobId) -> Option<HlsJobInfo> {
        hls::info(&self.shared, job)
    }

    /// The VOD playlist with every segment listed. `query_suffix` (such as `?mt=...`) is
    /// appended to each segment URL.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async in the shared signature so callers need not change if this starts to wait"
    )]
    pub async fn hls_playlist(
        &self,
        job: &HlsJobId,
        query_suffix: &str,
    ) -> Result<String, MediaError> {
        hls::playlist(&self.shared, job, query_suffix)
    }

    /// The init section or a segment, waiting up to 10 s for it
    /// ([`MediaError::Timeout`] after that).
    pub async fn hls_part(
        &self,
        job: &HlsJobId,
        part: HlsPart,
        request: &Parts,
    ) -> Result<Response<Body>, MediaError> {
        hls::part(&self.shared, job, part, request).await
    }

    /// Subsonic-style streaming transcode, written into the cache as it streams where the
    /// format allows. With `offset` nothing is cached and ffmpeg stops when the client goes.
    pub async fn progressive_transcode(
        &self,
        file: &SongFile,
        profile: Profile,
        offset: Option<Duration>,
        estimate_length: bool,
    ) -> Result<Response<Body>, MediaError> {
        jobs::progressive(&self.shared, file, profile, offset, estimate_length).await
    }

    pub fn artwork_store(&self) -> &ArtworkStore {
        &self.artwork
    }

    /// Artwork at a snapped size (64, 128, 256, 300, 600, 1200) or the original.
    pub async fn artwork(
        &self,
        id: &ArtworkId,
        size: Option<u32>,
        request: &Parts,
    ) -> Result<Response<Body>, MediaError> {
        match self.artwork.respond(id, size, request).await {
            Err(MediaError::NotFound) if self.restore_artwork(id).await => {
                self.artwork.respond(id, size, request).await
            }
            other => other,
        }
    }

    /// A cover fetched for a suggestion, at a snapped size or the original.
    /// [`MediaError::NotFound`] when it is not (or no longer) in the cache.
    pub async fn external_artwork(
        &self,
        id: &trackside_core::ExternalArtId,
        size: Option<u32>,
        request: &Parts,
    ) -> Result<Response<Body>, MediaError> {
        self.artwork.respond_external(id, size, request).await
    }

    /// Puts a lost original back into the cache from the file the database says it came
    /// from. True when it is back.
    async fn restore_artwork(&self, id: &ArtworkId) -> bool {
        let lookup = id.clone();
        let row = match self
            .db
            .read(move |c| trackside_core::repo::artwork::get(c, &lookup))
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => return false,
            Err(e) => {
                tracing::debug!(%id, error = %e, "artwork row not read");
                return false;
            }
        };
        let Some(source) = row.art.source_path.clone() else {
            return false;
        };
        let kind = match row.art.source {
            trackside_core::repo::artwork::ArtworkSourceKind::Embedded => {
                ArtworkSourceKind::Embedded
            }
            trackside_core::repo::artwork::ArtworkSourceKind::Sidecar => ArtworkSourceKind::Sidecar,
            trackside_core::repo::artwork::ArtworkSourceKind::Upload => ArtworkSourceKind::Upload,
            trackside_core::repo::artwork::ArtworkSourceKind::Acquired => {
                ArtworkSourceKind::Acquired
            }
        };
        let store = self.artwork.clone();
        let wanted = id.clone();
        let library = self.library_of(&source).await;
        let restore = move || {
            let embedded = kind == ArtworkSourceKind::Embedded;
            let Some(bytes) = artwork::reextract_blocking(&wanted, embedded, &source)? else {
                return Ok(false);
            };
            store.put_original(
                &bytes,
                ArtworkSource {
                    kind,
                    path: Some(source),
                },
            )?;
            Ok::<_, MediaError>(true)
        };
        // A source inside a library is read behind that library's semaphore, like a stream.
        let restored = match library {
            Some(library) => self.libraries.run_media(library, restore).await,
            None => tokio::task::spawn_blocking(restore)
                .await
                .unwrap_or_else(|e| Err(MediaError::Process(format!("restore task: {e}")))),
        };
        match restored {
            Ok(done) => {
                if done {
                    tracing::info!(%id, "artwork restored from its source file");
                }
                done
            }
            Err(e) => {
                tracing::debug!(%id, error = %e, "artwork not restored");
                false
            }
        }
    }

    /// The library whose root holds `path` (the deepest, if roots nest), for its semaphore.
    async fn library_of(&self, path: &Path) -> Option<trackside_core::LibraryKey> {
        let path = path.to_path_buf();
        self.db
            .read(move |c| {
                Ok(trackside_core::repo::libraries::all(c)?
                    .into_iter()
                    .filter(|l| path.starts_with(&l.root))
                    .max_by_key(|l| l.root.components().count())
                    .map(|l| l.key))
            })
            .await
            .ok()
            .flatten()
    }

    /// Audio properties read with lofty.
    pub async fn probe(&self, path: &Path) -> Result<Probe, MediaError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || probe::probe_blocking(&path))
            .await
            .map_err(|e| MediaError::Process(format!("probe task: {e}")))?
    }

    /// Decodes the whole file with `ffmpeg -f null`. A file that decodes with errors is
    /// `ok: false` with ffmpeg's message.
    pub async fn decode_check(&self, path: &Path) -> Result<DecodeCheck, MediaError> {
        jobs::decode_check(&self.shared, path).await
    }

    /// Evicts transcodes over the cap or below the free-space floor. For the scheduler.
    pub async fn evict_caches(&self) {
        if self.shared.cache.is_none() {
            return;
        }
        let shared = self.shared.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(cache) = &shared.cache {
                cache.make_room(0, None);
            }
        })
        .await;
    }

    /// Bytes held by complete cached transcodes.
    pub fn transcode_cache_bytes(&self) -> u64 {
        self.shared.cache.as_ref().map_or(0, TranscodeCache::total)
    }

    /// ffmpeg jobs running now and requests waiting for a slot.
    pub fn transcode_load(&self) -> (usize, usize) {
        let running = self
            .shared
            .transcode_limit
            .saturating_sub(self.shared.slots.free());
        (running, self.shared.slots.waiting())
    }

    /// How many ffmpeg jobs run at once, or `None` without ffmpeg.
    pub fn transcode_limit(&self) -> Option<usize> {
        self.shared
            .cache
            .as_ref()
            .map(|_| self.shared.transcode_limit)
    }
}

#[cfg(test)]
mod tests;
