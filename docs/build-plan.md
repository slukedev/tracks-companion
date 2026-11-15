# Trackside build plan

> Historical: the plan the first build followed, kept as written. For what was built, read [architecture.md](architecture.md) and [api.md](api.md).

How the work in [architecture.md](architecture.md) and [api.md](api.md) is split among
implementation agents. Each work package (WP) owns an exact set of files; no two packages edit the
same file. Packages talk to each other only through the types and traits written out in section 3,
which WP1 lands first as a compiling skeleton so that everything else can start in parallel.

Contents:

1. Waves and dependencies
2. File ownership
3. Shared interfaces (Rust)
4. Work packages with acceptance tests
5. Rules for every package
6. Workspace dependencies

---

## 1. Waves and dependencies

```
Wave 0   WP1-S  skeleton: workspace, all crates compiling, shared types and traits, migration 1, Db
           |
Wave 1   WP1 core   WP2 scan   WP3 media   WP4 auth   WP10 acquire
         WP5 subsonic*   WP6 api + server*   WP7 dashboard   WP8 packaging   WP9a Tracks client core
           |
Wave 2   WP5 and WP6 finish integration against the real WP1 to WP4 and WP10 crates
         WP9b Tracks app integration against a running server
           |
Wave 3   WP11 end-to-end, performance and hardening
```

`*` WP5 and WP6 start in wave 1 against the skeleton's stub implementations and fakes, and finish
when the crates they call are real.

| Package | Needs from others before it can finish | Provides |
|---|---|---|
| WP1-S | nothing | every shared type and trait in section 3, migration 1, `Db` |
| WP1 | WP1-S | repositories, ingest, sync, search, similarity, config loading |
| WP2 | WP1 ingest and folder repositories, WP3 `ArtworkStore` | `Scanner` implementing `ScanControl` |
| WP3 | WP1-S | `Media` |
| WP4 | WP1 user, device, key and session repositories | `Auth`, extractors |
| WP5 | WP1, WP3, WP4 | `/rest` router |
| WP6 | WP1, WP3, WP4, fakes for `ScanControl` and `AcquireControl` | `/api/v1` router, the binary |
| WP7 | api.md only (fixture preview mode); WP6 to embed | dashboard files |
| WP8 | WP6 binary for images; nothing for docs | images, compose, unit, CI, ops docs |
| WP9a | api.md and WP6 fixtures | `TracksideClient`, endpoint racing, sync applier, listen outbox in TracksCore |
| WP9b | WP9a, a running server (WP6) | app integration |
| WP10 | WP1 acquisition repositories, WP3 probe and HLS | `Acquirer` implementing `AcquireControl` |
| WP11 | everything | e2e and performance suite, release checklist |

---

## 2. File ownership

Paths are relative to the repository root. "Owns" means creates and edits; nobody else touches
those files. WP1-S creates the initial version of every crate's `Cargo.toml` and `src/lib.rs`
(with the section 3 signatures and `todo!()` bodies); when WP1-S merges, ownership of each crate's
files passes to the package below, which may then restructure its crate freely as long as the
section 3 public signatures still hold (changes to them go through WP1, see section 5).

| Package | Owns |
|---|---|
| WP1-S then WP1 | `Server/Cargo.toml`, `Server/Cargo.lock`, `Server/rust-toolchain.toml`, `Server/rustfmt.toml`, `Server/clippy.toml`, `Server/crates/trackside-core/**` |
| WP2 | `Server/crates/trackside-scan/**` |
| WP3 | `Server/crates/trackside-media/**` |
| WP4 | `Server/crates/trackside-auth/**` |
| WP5 | `Server/crates/trackside-subsonic/**` |
| WP6 | `Server/crates/trackside-api/**` (including `tests/fixtures/`), `Server/crates/trackside-server/**` |
| WP7 | `Server/dashboard/**` |
| WP8 | `Server/packaging/**`, `Server/README.md`, `Server/CHANGELOG.md`, `Server/docs/ops/**`, `Server/deny.toml`, `.github/workflows/server.yml` |
| WP9a | `TracksCore/Sources/TracksCore/Library/Trackside/**` (new), `TracksCore/Tests/TracksCoreTests/Trackside/**` (new) |
| WP9b | edits to `TracksCore/Sources/TracksCore/Library/SubsonicClient.swift` (model fields only), `ServerKeeps.swift`, `AudioQuality.swift`; `Apps/Shared/YourMusic/{MusicServers,ServerDiscovery,YourMusic,Downloads,LocalPlayerEngine,MusicSource,Playlists}.swift`, `Apps/Shared/YourMusic/Views/ServerSettings.swift`, the server pages (`ServerArtistPage.swift`, `LocalAlbumPage.swift`, `LocalLibraryPages.swift`, `SeedArtistPicker.swift`, `ServerForYouSection.swift`, `YourMusicSearch.swift`, `Views/Availability.swift`), `Apps/Shared/Design/ArtworkImages.swift`, `MotionCoverView.swift`, `PlaylistMerge.swift`, `Apps/macOS/Play/{LibrarySongsTable,LibraryDownloads}.swift`, `Apps/iOS/Resources/Info.plist`, `Apps/macOS/Resources/Info.plist`, and existing test files `LibraryTests.swift`, `ServerKeepsTests.swift`, `AudioQualityTests.swift` |
| WP10 | `Server/crates/trackside-acquire/**` |
| WP11 | `Server/tests-e2e/**` |
| none (architect) | `Server/docs/architecture.md`, `Server/docs/api.md`, `Server/docs/adr/**`, `Server/docs/build-plan.md`; packages propose changes to these in their reports, not by editing |

`Server/Cargo.toml` lists the workspace members and every dependency in section 6 from the start,
so packages never need to edit it. A package that needs a new dependency asks WP1 in its report.

---

## 3. Shared interfaces (Rust)

These signatures are the contract. Bodies are omitted. All public items have doc comments in the
real code; they are left out here to keep the plan readable. `Result<T>` in each crate means that
crate's own error type.

The skeleton (WP1-S) wrote every signature below out in `Server/crates/*/src`. Where the code and
this section differ, the code is the contract; the main decisions made while writing it:

- Stubs return a `Unimplemented(&'static str)` variant of the crate's error (every error enum
  has one, and every error enum is `#[non_exhaustive]`); functions without a `Result` return a
  neutral value (`None`, empty `Vec`, `RangeOutcome::Full`, `Decision::Direct`).
- Every repository module named below has concrete signatures and record types in
  `trackside-core/src/repo/`. Added modules: `password_resets`, `annotations` (stars, ratings and
  play stats for the `me` object), `genres`, `lyrics`; added `songs::by_folder`,
  `albums::get_many` and `artists::get_many`. List functions that page
  return `records::Page<T> { items, next: Option<String> }`.
- `KeyKind` (`repo::keys`) and `NewDevice` (`repo::devices`) live in core; auth re-exports them.
- `repo::acquisitions::list`, `AcquireControl::list` and `AcquireControl::admin_list` return a
  `Page`, since the API pages them.
- `ingest` functions take `run: &ScanRunId`; `SongOrigin` lives in `control` and is re-exported
  from `ingest`.
- Shared enums stored as text (`Container`, `Codec`, `CreditRole`, `Explicit`, `ItemKind`,
  `ScanRunKind`, `ScanState`, `ImportState`, `ScanTrigger`, `AcquisitionState`, `AcquireFailure`,
  `Quality`, `KeyKind`, `PairingKind`, `ListenSource`) have `as_str`, `parse`, `ALL` and serde as
  their stored value.
- Migration 1 adds `pairings.qr_secret_hash BLOB UNIQUE`: an offer has two secrets (the typed
  code and the 128-bit QR secret) and each needs its own lookup hash.
- `Db::open` writes the first-start settings (`server.id`, `setup.state`,
  `sync.pruned_through`, `schema.tracks_model` = `TRACKS_MODEL_FOLDING`) with `ON CONFLICT DO
  NOTHING`. `repo::settings` is implemented (get, put, delete, all, `server_id`).
- `SyncCursor::encode`/`decode`, `listing_hash`, `Profile`, `Scopes` and the config parsers
  (`parse_duration`, `parse_size`, `Rate`, `BackupSchedule`, `Config::defaults`,
  `default_data_dir`) are implemented; their owners may refine them.

### 3.1 trackside-core

```rust
// ids.rs
pub trait PublicId: Sized + Clone + Eq + std::hash::Hash + std::fmt::Display + Send + Sync + 'static {
    const PREFIX: char;
    fn parse(s: &str) -> Result<Self, IdError>;
    fn as_str(&self) -> &str;
    fn from_hash(hash: &[u8; 32]) -> Self; // first 100 bits, Crockford base32, lowercase
}
// One newtype per kind, each `pub struct X(Box<str>)` implementing PublicId, FromStr, Ord, Debug,
// rusqlite ToSql/FromSql and serde (as a string). Internal keys also implement ToSql/FromSql.
// SongId 's', AlbumId 'a', ArtistId 'r', LibraryId 'l', FolderId 'f', ArtworkId 'c',
// PlaylistId 'p', UserId 'u', DeviceId 'd', KeyId 'k', AcquisitionId 'q', CandidateToken 'x',
// ScanRunId 'n', ImportId 'i', PairingId 'g', InviteId 'v'.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AudioDigest(pub [u8; 32]);

impl SongId {
    pub fn derive(digest: &AudioDigest) -> Self;
    pub fn derive_disambiguated(digest: &AudioDigest, library: &LibraryId, rel_path: &[u8]) -> Self;
}
impl AlbumId { pub fn derive(library: &LibraryId, identity: &str) -> Self; }
impl ArtistId { pub fn derive(identity: &str) -> Self; }
impl FolderId { pub fn derive(library: &LibraryId, rel_path: &[u8]) -> Self; }
impl ArtworkId { pub fn of_bytes(bytes: &[u8]) -> Self; }
impl CandidateToken { pub fn derive(provider: &str, provider_ref: &str) -> Self; }
pub fn random_id<T: PublicId>() -> T;

// Internal keys, never sent to clients.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct UserKey(pub i64);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct LibraryKey(pub i64);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct DeviceKey(pub i64);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)] pub struct Seq(pub i64);

// time.rs
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Millis(pub i64); // Unix milliseconds, UTC
pub trait Clock: Send + Sync + std::fmt::Debug + 'static { fn now(&self) -> Millis; }
#[derive(Debug, Default)] pub struct SystemClock;
#[derive(Debug)] pub struct TestClock { /* AtomicI64 */ }
impl TestClock { pub fn new(start: Millis) -> Self; pub fn advance(&self, by: std::time::Duration); pub fn set(&self, to: Millis); }
impl Millis { pub fn from_system(t: SystemTime) -> Self; pub fn to_rfc3339(self) -> String; pub fn parse_rfc3339(s: &str) -> Option<Self>;
    pub fn saturating_add(self, d: Duration) -> Self; pub fn saturating_sub(self, d: Duration) -> Self; pub fn since(self, earlier: Self) -> Duration; }

// access.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role { Admin, Member }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    pub can_download: bool,
    pub can_upload: bool,
    pub can_request: bool,
    pub legacy_subsonic: bool,
    pub max_bitrate_kbps: Option<u32>,
}

#[derive(Clone, Debug)]
pub enum LibrarySet { All, Only(std::sync::Arc<[LibraryKey]>) }

/// What a caller may see. Every repository read takes one.
#[derive(Clone, Debug)]
pub struct Access {
    pub user: UserKey,
    pub role: Role,
    pub libraries: LibrarySet,
    pub sync_epoch: i64,
}
impl Access { pub fn can_see(&self, library: LibraryKey) -> bool; pub fn system() -> Self; }

// db.rs
#[derive(Clone, Debug)]
pub struct DbOptions { pub path: std::path::PathBuf, pub readers: usize, pub cache_bytes: u64, pub mmap_bytes: u64,
    pub allow_network_fs: bool, pub filesystem: FilesystemCheck }
impl DbOptions { pub fn new(path: impl Into<PathBuf>) -> Self; } // section 11 defaults
pub enum FilesystemCheck { Detect, AssumeLocal, AssumeNetwork } // AssumeNetwork is the test seam for "fake statfs"
pub const SCHEMA_VERSION: u32 = 1;
pub fn is_network_filesystem(dir: &Path) -> std::io::Result<bool>; // /proc/self/mountinfo on Linux, false elsewhere

#[derive(Clone)]
pub struct Db { /* Arc<Inner>: Mutex<Connection> writer, Mutex<Vec<Connection>> readers, next_seq, EventBus, Clock */ }
impl Db {
    pub fn open(options: &DbOptions, events: EventBus, clock: std::sync::Arc<dyn Clock>) -> Result<Self, DbError>;
    pub fn open_in_memory(events: EventBus, clock: std::sync::Arc<dyn Clock>) -> Result<Self, DbError>;
    pub fn schema_version(&self) -> Result<u32, DbError>;
    pub fn inspect_schema_version(path: &Path) -> Result<Option<u32>, DbError>; // without migrating, for pre-migration backups
    pub fn path(&self) -> Option<&Path>; pub fn events(&self) -> &EventBus; pub fn clock(&self) -> &Arc<dyn Clock>;
    pub async fn read<T, F>(&self, f: F) -> Result<T, DbError>
        where T: Send + 'static, F: FnOnce(&rusqlite::Connection) -> Result<T, DbError> + Send + 'static;
    pub async fn write<T, F>(&self, f: F) -> Result<T, DbError>
        where T: Send + 'static, F: FnOnce(&mut WriteTx<'_>) -> Result<T, DbError> + Send + 'static;
    pub fn read_blocking<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> Result<T, DbError>) -> Result<T, DbError>;
    pub fn write_blocking<T>(&self, f: impl FnOnce(&mut WriteTx<'_>) -> Result<T, DbError>) -> Result<T, DbError>;
    pub fn backup_into(&self, target: &std::path::Path) -> Result<(), DbError>; // VACUUM INTO, .partial then rename
    pub fn checkpoint_and_optimize(&self) -> Result<(), DbError>;
}

/// A write transaction. Changes recorded here go to change_log in the same transaction and are
/// published on the EventBus after commit.
pub struct WriteTx<'c> { /* rusqlite::Transaction<'c>, Vec<Change>, now */ }
impl<'c> WriteTx<'c> {
    pub fn conn(&self) -> &rusqlite::Connection;
    pub fn now(&self) -> Millis;
    pub fn record(&mut self, change: Change) -> Result<Seq, DbError>;
    pub fn savepoint(&mut self) -> Result<rusqlite::Savepoint<'_>, DbError>;
}
impl Change { pub fn shared(kind, entity, library: Option<LibraryKey>, op) -> Self; pub fn for_user(kind, entity, user: UserKey, op) -> Self; }
impl std::ops::Deref for WriteTx<'_> { type Target = rusqlite::Connection; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind { Song, Album, Artist, Library, Playlist, Star, Rating, PlayStat, PlayQueue, Acquisition }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeOp { Upsert, Delete }
#[derive(Clone, Debug)]
pub struct Change { pub kind: ChangeKind, pub entity: String, pub user: Option<UserKey>, pub library: Option<LibraryKey>, pub op: ChangeOp }

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)] Sqlite(#[from] rusqlite::Error),
    #[error("database is newer than this server (schema {found}, known {known})")] TooNew { found: u32, known: u32 },
    #[error("database is on a network filesystem")] NetworkFilesystem,
    #[error("disk full")] Full,
    #[error("database closed")] Closed,
    #[error("not found")] NotFound,
    #[error("forbidden")] Forbidden,                 // e.g. changing someone else's public playlist
    #[error("precondition failed")] PreconditionFailed, // If-Match revision moved
    #[error("conflict: {0}")] Conflict(String),
    #[error("invalid: {0}")] Invalid(String),
    #[error("migration failed: {0}")] Migration(String),
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")] Unimplemented(&'static str),
}

// events.rs
#[derive(Clone, Debug)]
pub enum Audience { Everyone { library: Option<LibraryKey> }, User(UserKey), Admins }

#[derive(Clone, Debug)]
pub enum ServerEvent {
    Changed { seq: Seq, audiences: Vec<Audience> },
    LibraryState { library: LibraryId, available: bool },
    Scan(ScanProgress),
    Acquisition { user: UserKey, view: AcquisitionView },
    Upload { user: UserKey, import: ImportId, state: ImportState, song: Option<SongId> },
    CredentialRevoked { user: UserKey, device: Option<DeviceKey> },
}
#[derive(Clone, Debug)]
pub struct EventBus { /* broadcast::Sender<Arc<ServerEvent>> */ }
impl EventBus {
    pub fn new(capacity: usize) -> Self;
    pub fn publish(&self, event: ServerEvent);
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<std::sync::Arc<ServerEvent>>;
}

// records.rs (read models returned by repositories; wire mapping lives in api and subsonic)
pub struct Song { pub id: SongId, pub library: LibraryId, pub title: String, pub artist: String,
    pub artist_id: Option<ArtistId>, pub credits: Vec<Credit>, pub album: Option<String>,
    pub album_id: Option<AlbumId>, pub album_artist: Option<String>, pub track_number: Option<u32>,
    pub disc_number: Option<u32>, pub disc_subtitle: Option<String>, pub year: Option<i32>,
    pub release_date: Option<String>, pub genres: Vec<String>, pub duration_ms: u32,
    pub format: AudioFormat, pub artwork: Option<ArtworkId>, pub song_key: String,
    pub replay_gain: Option<ReplayGain>, pub mbz: SongMbz, pub isrc: Option<String>,
    pub bpm: Option<u32>, pub comment: Option<String>, pub explicit: Option<Explicit>,
    pub has_lyrics: bool, pub folder: FolderId, pub added_at: Millis, pub updated_at: Millis,
    pub missing: bool, pub content_version: i64 }
pub struct AudioFormat { pub container: Container, pub codec: Codec, pub content_type: &'static str,
    pub lossless: bool, pub bitrate_kbps: Option<u32>, pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>, pub channels: Option<u8>, pub size: u64 }
pub enum Container { Flac, M4a, Mp3, Aac, Ogg, Opus, Wav, Aiff, WavPack, Ape }
pub enum Codec { Flac, Alac, Aac, Mp3, Opus, Vorbis, Pcm, WavPack, Ape }
pub struct Credit { pub artist: ArtistId, pub name: String, pub role: CreditRole }
pub enum CreditRole { Main, Featured, AlbumArtist, Composer }
pub struct Album { /* fields of the api.md Album object, typed */ }
pub struct Artist { /* fields of the api.md Artist object, typed */ }
pub struct Library { pub id: LibraryId, pub key: LibraryKey, pub name: String, pub root: std::path::PathBuf,
    pub enabled: bool, pub writable: bool, pub watch: bool, pub available: bool, pub song_count: u64 }
pub struct UserAnnotations { pub starred_at: Option<Millis>, pub rating: Option<u8>,
    pub play_count: u32, pub last_played_at: Option<Millis> }
/// Where a song's bytes are, for media serving.
pub struct SongFile { pub song: SongId, pub library: LibraryKey, pub path: std::path::PathBuf,
    pub size: u64, pub mtime_ns: i64, pub content_version: i64, pub format: AudioFormat, pub duration_ms: u32 }

// repo/*.rs: plain functions over a connection; every read takes &Access and filters by it.
pub mod repo {
    pub mod songs {
        pub fn get(c: &Connection, a: &Access, id: &SongId) -> Result<Option<Song>, DbError>;
        pub fn get_many(c: &Connection, a: &Access, ids: &[SongId]) -> Result<Vec<Song>, DbError>;
        pub fn file(c: &Connection, a: &Access, id: &SongId) -> Result<Option<SongFile>, DbError>;
        pub fn by_album(c: &Connection, a: &Access, album: &AlbumId) -> Result<Vec<Song>, DbError>;
        pub fn random(c: &Connection, a: &Access, filter: &SongFilter, count: u32) -> Result<Vec<Song>, DbError>;
        pub fn by_key(c: &Connection, a: &Access, song_key: &str) -> Result<Vec<Song>, DbError>;
        pub fn top_for_artist(c: &Connection, a: &Access, artist: &ArtistId, count: u32) -> Result<Vec<Song>, DbError>;
        pub fn all_paged(c: &Connection, a: &Access, after: Option<i64>, limit: u32) -> Result<(Vec<Song>, Option<i64>), DbError>;
    }
    pub mod albums { /* get, list(sort: AlbumSort, cursor, limit, filter), by_artist, appears_on */ }
    pub mod artists { /* get, list, by_name */ }
    pub mod folders { /* get, children, upsert(tx, ..), mark_unreadable, listing_hash */ }
    pub mod libraries { /* list, get, create, update, delete, set_available */ }
    pub mod users { /* get, by_username, create, update, delete, set_libraries, count_admins */ }
    pub mod devices { /* get, upsert_for_login, revoke, list */ }
    pub mod keys { /* insert, by_hash, list, revoke, touch_last_used, app_passwords_for */ }
    pub mod sessions { /* insert, by_hash, touch, delete, delete_for_user, prune */ }
    pub mod invites { /* insert, by_hash, accept, revoke, list */ }
    pub mod pairings { /* insert, by_code_hash, approve, claim, fail_attempt, prune */ }
    pub mod listens {
        pub fn submit(tx: &mut WriteTx<'_>, user: UserKey, device: Option<DeviceKey>, items: &[NewListen])
            -> Result<Vec<ListenOutcome>, DbError>;
    }
    pub mod stars { /* set, clear, list */ }
    pub mod ratings { /* set, clear */ }
    pub mod playlists { /* list, get, create, update, replace_entries(if_revision), append, delete */ }
    pub mod play_queue { /* get, put */ }
    pub mod bookmarks { /* list, put, delete */ }
    pub mod scan_runs { /* start, progress, finish, list, errors, quarantine */ }
    pub mod imports { /* create, progress, finish */ }
    pub mod acquisitions {
        pub fn insert(tx: &mut WriteTx<'_>, new: &NewAcquisition) -> Result<AcquisitionRow, DbError>;
        pub fn active_by_key(c: &Connection, request_key: &str) -> Result<Option<AcquisitionRow>, DbError>;
        pub fn lease_next(tx: &mut WriteTx<'_>, now: Millis, lease: std::time::Duration) -> Result<Option<AcquisitionRow>, DbError>;
        pub fn renew_lease(tx: &mut WriteTx<'_>, id: &AcquisitionId, until: Millis) -> Result<(), DbError>;
        pub fn transition(tx: &mut WriteTx<'_>, id: &AcquisitionId, to: AcquisitionUpdate) -> Result<AcquisitionRow, DbError>;
        pub fn candidates(c: &Connection, id: &AcquisitionId) -> Result<Vec<CandidateRow>, DbError>;
        pub fn put_candidates(tx: &mut WriteTx<'_>, job: Option<&AcquisitionId>, rows: &[CandidateRow]) -> Result<(), DbError>;
        pub fn candidate_by_token(c: &Connection, token: &CandidateToken) -> Result<Option<CandidateRow>, DbError>;
        pub fn count_for_user_since(c: &Connection, user: UserKey, since: Millis) -> Result<u32, DbError>;
        pub fn list(c: &Connection, filter: &AcquisitionFilter) -> Result<Page<AcquisitionRow>, DbError>;
    }
    pub mod settings { /* get<T: DeserializeOwned>, put<T: Serialize>, all */ }
    pub mod audit { pub fn log(tx: &mut WriteTx<'_>, entry: &AuditEntry) -> Result<(), DbError>; /* list */ }
    pub mod idempotency { /* get, put, prune */ }
}

// ingest.rs: the id rules of architecture section 6, the only code that creates or moves songs.
pub struct ScannedFile {
    pub rel_path: Vec<u8>, pub rel_path_display: String, pub folder: FolderId,
    pub size: u64, pub mtime_ns: i64, pub dev: Option<u64>, pub ino: Option<u64>,
    pub digest: AudioDigest, pub tags: ScannedTags, pub properties: ScannedProperties,
    pub artwork: Option<ArtworkId>, pub lyrics: Vec<ScannedLyrics>, pub tag_problem: Option<String>,
}
pub struct ScannedTags { pub title: String, pub artist: String, pub album: Option<String>,
    pub album_artist: Option<String>, pub track: Option<u32>, pub disc: Option<u32>,
    pub disc_subtitle: Option<String>, pub year: Option<i32>, pub release_date: Option<String>,
    pub original_date: Option<String>, pub genres: Vec<String>, pub compilation: bool,
    pub release_type: Option<String>, pub mbz_recording: Option<String>, pub mbz_track: Option<String>,
    pub mbz_release: Option<String>, pub mbz_release_group: Option<String>, pub mbz_artist: Option<String>,
    pub mbz_album_artist: Option<String>, pub isrc: Option<String>, pub bpm: Option<u32>,
    pub comment: Option<String>, pub explicit: Option<Explicit>, pub replay_gain: Option<ReplayGain>,
    pub sort_title: Option<String>, pub sort_artist: Option<String>, pub sort_album: Option<String> }
pub struct ScannedProperties { pub duration_ms: u32, pub bitrate_kbps: Option<u32>, pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>, pub channels: Option<u8>, pub codec: Codec, pub container: Container }
pub enum Resolution { Unchanged(SongId), Updated(SongId), Moved(SongId), Replaced(SongId), Added(SongId) }
pub fn apply_batch(tx: &mut WriteTx<'_>, library: LibraryKey, run: ScanRunId, origin: SongOrigin,
    files: &[ScannedFile]) -> Result<Vec<Resolution>, DbError>;
pub fn known_file(c: &Connection, library: LibraryKey, rel_path: &[u8]) -> Result<Option<KnownFile>, DbError>;
pub fn finish_run(tx: &mut WriteTx<'_>, library: LibraryKey, run: ScanRunId, seen: &SeenSet,
    unreadable: &[Vec<u8>]) -> Result<FinishOutcome, DbError>; // marks missing; never called by an aborted run
pub fn purge_missing(tx: &mut WriteTx<'_>, older_than: Millis) -> Result<u64, DbError>;
#[derive(Clone, Copy, Debug)] pub enum SongOrigin { Scan, Upload, Acquired }

// sync.rs
pub struct SyncCursor { pub epoch: i64, pub seq: Seq, pub snapshot: Option<SnapshotPosition> }
impl SyncCursor { pub fn encode(&self) -> String; pub fn decode(s: &str) -> Result<Self, SyncError>; }
pub enum SyncPage { Snapshot(SnapshotPage), Delta(DeltaPage) }
pub fn page(c: &Connection, a: &Access, cursor: Option<&SyncCursor>, limit: u32) -> Result<SyncPage, SyncError>;
#[derive(Debug, thiserror::Error)]
pub enum SyncError { #[error("reset required")] Reset, #[error("bad cursor")] BadCursor, #[error(transparent)] Db(#[from] DbError) }

// search.rs
pub struct SearchQuery { pub text: String, pub kinds: SearchKinds, pub limit: u32, pub cursor: Option<String>, pub library: Option<LibraryKey> }
pub fn search(c: &Connection, a: &Access, q: &SearchQuery) -> Result<SearchResults, DbError>;

// similar.rs
pub fn similar_songs(c: &Connection, a: &Access, seed: &SongId, count: u32) -> Result<Vec<Song>, DbError>;
pub fn similar_artists(c: &Connection, a: &Access, seed: &ArtistId, count: u32) -> Result<Vec<SimilarArtist>, DbError>;

// config.rs: every key in architecture section 11.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Config { pub data_dir: std::path::PathBuf, pub music_dir: Option<std::path::PathBuf>,
    pub cache_dir: std::path::PathBuf, pub server: ServerConfig, pub tls: TlsConfig,
    pub database: DatabaseConfig, pub scanner: ScannerConfig, pub media: MediaConfig,
    pub auth: AuthConfig, pub sync: SyncConfig, pub discovery: DiscoveryConfig,
    pub backup: BackupConfig, pub log: LogConfig, pub metrics: MetricsConfig,
    pub uploads: UploadsConfig, pub acquire: AcquireConfig, pub enrichment: EnrichmentConfig }
pub struct ConfigSources { pub file: Option<std::path::PathBuf>, pub env: Vec<(String, String)>, pub overrides: Vec<(String, String)> }
pub fn load_config(sources: &ConfigSources) -> Result<(Config, ConfigReport), ConfigError>; // report: per-key source, warnings
pub fn locked_keys(report: &ConfigReport) -> std::collections::BTreeSet<&'static str>; // set by file or env

// control.rs: traits the HTTP crates use, implemented in scan and acquire.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug)] pub enum ScanKind { Quick, Full }
#[derive(Clone, Copy, Debug)] pub enum ScanTrigger { Startup, Schedule, Watch, Admin, Import, Cli }
pub enum ImportOutcome { Added(SongId), Existing(SongId) }

pub trait ScanControl: Send + Sync + 'static {
    fn request_scan(&self, library: &LibraryId, kind: ScanKind, trigger: ScanTrigger, force_empty: bool)
        -> Result<ScanRunId, ScanControlError>;
    fn import_file<'a>(&'a self, library: &'a LibraryId, rel_path: &'a std::path::Path, origin: SongOrigin)
        -> BoxFuture<'a, Result<ImportOutcome, ScanControlError>>;
    fn process_upload<'a>(&'a self, import: &'a ImportId) -> BoxFuture<'a, Result<(), ScanControlError>>;
    fn status(&self) -> Vec<LibraryScanStatus>;
    fn is_available(&self, library: &LibraryId) -> bool;
}

pub trait AcquireControl: Send + Sync + 'static {
    /// Some when `features.acquisition` should be present for this caller.
    fn feature_for(&self, caller: &Access, permissions: &Permissions, scopes_include_request: bool) -> Option<AcquireFeature>;
    fn search<'a>(&'a self, caller: &'a Access, query: ExternalQuery) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>>;
    fn search_more<'a>(&'a self, caller: &'a Access, search: &'a str) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>>;
    fn request<'a>(&'a self, caller: &'a Access, request: AcquireRequest) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>>;
    fn get<'a>(&'a self, caller: &'a Access, id: &'a AcquisitionId) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>>;
    fn list<'a>(&'a self, caller: &'a Access, filter: AcquisitionFilter) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>>;
    fn cancel<'a>(&'a self, caller: &'a Access, id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>>;
    fn instant_stream(&self, caller: &Access, id: &AcquisitionId) -> Result<InstantStream, AcquireError>;
    // Admin
    fn providers<'a>(&'a self) -> BoxFuture<'a, Vec<ProviderStatus>>;
    fn configure<'a>(&'a self, change: ProviderChange) -> BoxFuture<'a, Result<(), AcquireError>>;
    fn test_provider<'a>(&'a self, name: &'a str, query: &'a str) -> BoxFuture<'a, Result<ProviderTest, AcquireError>>;
    fn update_ytdlp<'a>(&'a self) -> BoxFuture<'a, Result<(), AcquireError>>;
    fn retry<'a>(&'a self, id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>>;
    fn admin_list<'a>(&'a self, filter: AcquisitionFilter) -> BoxFuture<'a, Result<Page<AdminAcquisitionView>, AcquireError>>;
}

/// The view Tracks sees: no provider names.
#[derive(Clone, Debug)]
pub struct AcquisitionView { pub id: AcquisitionId, pub state: AcquisitionState, pub title: String,
    pub artist: String, pub album: Option<String>, pub progress: Option<(u64, Option<u64>)>,
    pub quality: Option<Quality>, pub song: Option<SongId>, pub existing: bool,
    pub failure: Option<AcquireFailure>, pub stream: Option<InstantStreamUrls>,
    pub created_at: Millis, pub updated_at: Millis }
pub enum AcquisitionState { Queued, Searching, Downloading, Verifying, Importing, Done, Failed, Cancelled }
pub enum AcquireFailure { NoMatch, AllCandidatesFailed, Exhausted, ProviderUnavailable, InsufficientSpace, RejectedFile, QuotaExceeded, Disabled }
pub enum Quality { Lossless, High, Standard }

/// A file that is still being written, shared between a fetch and the HTTP layer.
#[derive(Clone, Debug)]
pub struct GrowingFile { pub path: std::path::PathBuf, pub state: tokio::sync::watch::Receiver<GrowState> }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowState { Growing { bytes: u64 }, Complete { bytes: u64 }, Failed }
pub struct InstantStream { pub progressive: GrowingFile, pub hls_job: Option<HlsJobId>, pub content_type: &'static str, pub duration_ms: Option<u32> }
#[derive(Clone, Debug, PartialEq, Eq, Hash)] pub struct HlsJobId(pub Box<str>);
```

### 3.2 trackside-media

```rust
pub struct Media { /* config, Arc<dyn Transcoder>, caches, job registry, Db, Clock */ }

impl Media {
    pub fn new(config: &MediaConfig, cache_dir: &std::path::Path, db: Db, transcoder: std::sync::Arc<dyn Transcoder>,
               clock: std::sync::Arc<dyn Clock>) -> Result<Self, MediaError>;
    pub fn capabilities(&self) -> MediaCapabilities; // ffmpeg found, version, profiles, hls

    pub async fn serve_file(&self, file: &SongFile, request: &http::request::Parts, disposition: Disposition)
        -> Result<http::Response<axum::body::Body>, MediaError>;
    pub async fn serve_growing(&self, file: &GrowingFile, request: &http::request::Parts, content_type: &'static str)
        -> Result<http::Response<axum::body::Body>, MediaError>;

    pub fn decide(&self, file: &SongFile, client: &ClientProfile, user_limit_kbps: Option<u32>) -> Decision;
    pub async fn cached(&self, file: &SongFile, profile: Profile) -> Option<CachedTranscode>;
    pub async fn ensure_cached(&self, file: &SongFile, profile: Profile, priority: Priority)
        -> Result<CachedTranscode, MediaError>;
    pub async fn prepare(&self, files: &[SongFile], profile: Profile) -> Vec<PrepareState>;
    pub async fn start_hls(&self, input: HlsInput, profile: Profile, priority: Priority) -> Result<HlsJobId, MediaError>;
    pub async fn hls_playlist(&self, job: &HlsJobId, query_suffix: &str) -> Result<String, MediaError>;
    pub async fn hls_part(&self, job: &HlsJobId, part: HlsPart, request: &http::request::Parts)
        -> Result<http::Response<axum::body::Body>, MediaError>;
    pub async fn progressive_transcode(&self, file: &SongFile, profile: Profile, offset: Option<std::time::Duration>,
        estimate_length: bool) -> Result<http::Response<axum::body::Body>, MediaError>;

    pub fn artwork_store(&self) -> &ArtworkStore;
    pub async fn artwork(&self, id: &ArtworkId, size: Option<u32>, request: &http::request::Parts)
        -> Result<http::Response<axum::body::Body>, MediaError>;

    pub async fn probe(&self, path: &std::path::Path) -> Result<Probe, MediaError>;          // lofty properties
    pub async fn decode_check(&self, path: &std::path::Path) -> Result<DecodeCheck, MediaError>; // ffmpeg -f null
}

pub enum Disposition { Inline, Attachment { file_name: String } }

pub struct ClientProfile { pub direct: Vec<DirectPlay>, pub hls: bool, pub max_bitrate_kbps: Option<u32>,
    pub preferred_profile: Option<Profile>, pub allow_original_fallback: bool }
pub struct DirectPlay { pub container: Container, pub codecs: Vec<Codec> }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile { Aac256, Aac128, Mp3_320, Mp3_128, Opus128, Flac }
impl Profile { pub fn parse(s: &str) -> Option<Self>; pub fn as_str(self) -> &'static str; pub fn bitrate_kbps(self) -> Option<u32>; }

pub enum Decision {
    Direct { reason: Option<DecisionReason> },
    File { transcode: CachedTranscode, reason: DecisionReason },  // reason is sent as `reason`
    Hls { profile: Profile, reason: DecisionReason },              // the handler calls start_hls
}
pub enum DecisionReason { OverBitrate, UnsupportedCodec, TranscoderBusy, NoTranscoder }
pub struct CachedTranscode { pub path: std::path::PathBuf, pub size: u64, pub etag: String, pub profile: Profile, pub format: AudioFormat }
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub enum Priority { Interactive, Prefetch, Download }
pub enum HlsInput { File(SongFile), Growing { file: GrowingFile, duration_ms: u32 } }
pub enum HlsPart { Init, Segment(u32) }

/// Content-addressed artwork originals and resized copies under <cache>/art.
pub struct ArtworkStore { /* dir, max bytes, single-flight map */ }
impl ArtworkStore {
    pub fn new(dir: impl Into<PathBuf>, max_bytes: u64) -> Self; // Media::new makes one at <cache>/art
    pub fn put_original(&self, bytes: &[u8], source: ArtworkSource) -> Result<ArtworkMeta, MediaError>;
    pub fn has(&self, id: &ArtworkId) -> bool;
}
pub struct ArtworkMeta { pub id: ArtworkId, pub mime: &'static str, pub width: u32, pub height: u32, pub bytes: u64 }

/// The seam for tests: the real one runs ffmpeg, the fake writes deterministic bytes.
pub trait Transcoder: Send + Sync + std::fmt::Debug + 'static {
    fn version(&self) -> Option<String>;
    fn spawn(&self, spec: TranscodeSpec) -> Result<Box<dyn TranscodeProcess>, MediaError>;
}
pub struct TranscodeSpec { pub input: TranscodeInput, pub profile: Profile, pub output: TranscodeOutput,
    pub offset: Option<std::time::Duration> }
pub enum TranscodeInput { Path(std::path::PathBuf), Stdin }
pub enum TranscodeOutput { File(std::path::PathBuf), Hls { dir: std::path::PathBuf, segment: std::time::Duration, start_number: u32 }, Stdout }
pub trait TranscodeProcess: Send {
    fn stdin(&mut self) -> Option<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>;
    fn stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;
    fn wait(self: Box<Self>) -> BoxFuture<'static, Result<(), MediaError>>;
    fn kill(&mut self);
}
pub struct FfmpegTranscoder { /* path, version */ }
impl FfmpegTranscoder { pub fn detect(path: Option<&std::path::Path>) -> Option<Self>; }

/// Pure range logic, unit-tested without I/O.
pub enum RangeOutcome { Full, Partial { start: u64, end_inclusive: u64 }, Unsatisfiable, NotModified, PreconditionFailed }
pub fn evaluate(headers: &http::HeaderMap, len: u64, etag: &str, last_modified: std::time::SystemTime) -> RangeOutcome;
```

### 3.3 trackside-auth

```rust
pub struct Auth { /* Db, SecretKey, AuthConfig, limiters, Clock, Argon2 semaphore, key cache */ }

impl Auth {
    pub fn new(db: Db, config: &AuthConfig, secret: SecretKey, trusted_proxies: Vec<ipnet::IpNet>,
               clock: std::sync::Arc<dyn Clock>) -> Self;

    pub fn client_ip(&self, parts: &http::request::Parts, peer: std::net::SocketAddr) -> std::net::IpAddr;

    pub async fn authenticate(&self, parts: &http::request::Parts, ip: std::net::IpAddr, class: RouteClass)
        -> Result<Principal, AuthError>;
    pub async fn authenticate_subsonic(&self, params: &SubsonicCredentials, ip: std::net::IpAddr)
        -> Result<Principal, SubsonicAuthError>;

    pub async fn hash_password(&self, password: &str) -> Result<String, AuthError>;
    pub async fn login(&self, username: &str, password: &str, ip: std::net::IpAddr) -> Result<UserKey, AuthError>;
    pub async fn create_session(&self, user: UserKey, ip: std::net::IpAddr, user_agent: Option<&str>)
        -> Result<SessionCookie, AuthError>;
    pub async fn end_session(&self, cookie_value: &str) -> Result<(), AuthError>;

    pub async fn issue_device_key(&self, user: UserKey, device: NewDevice) -> Result<IssuedDeviceKey, AuthError>;
    pub async fn create_key(&self, user: UserKey, kind: KeyKind, name: &str, scopes: Scopes) -> Result<IssuedKey, AuthError>;
    pub async fn revoke_key(&self, user: Option<UserKey>, key: &KeyId) -> Result<(), AuthError>;
    pub async fn revoke_device(&self, user: Option<UserKey>, device: &DeviceId) -> Result<(), AuthError>;

    pub fn mint_media_token(&self, principal: &Principal) -> Result<MediaToken, AuthError>;

    pub async fn setup_state(&self) -> Result<SetupState, AuthError>;
    pub async fn ensure_setup_token(&self, data_dir: &std::path::Path) -> Result<Option<String>, AuthError>;
    pub async fn complete_setup(&self, token: &str, admin: NewUser, ip: std::net::IpAddr) -> Result<UserKey, AuthError>;

    pub async fn create_invite(&self, by: UserKey, invite: NewInvite) -> Result<IssuedInvite, AuthError>;
    pub async fn accept_invite(&self, token: &str, user: NewUser, ip: std::net::IpAddr) -> Result<UserKey, AuthError>;
    pub async fn create_password_reset(&self, by: UserKey, user: UserKey) -> Result<IssuedReset, AuthError>;
    pub async fn use_password_reset(&self, token: &str, new_password: &str) -> Result<(), AuthError>;

    pub async fn create_pairing_offer(&self, user: UserKey, device_name: Option<&str>) -> Result<PairingOffer, AuthError>;
    pub async fn claim_pairing(&self, code: &str, device: NewDevice, ip: std::net::IpAddr) -> Result<IssuedDeviceKey, AuthError>;
    pub async fn start_pair_request(&self, device: NewDevice, ip: std::net::IpAddr) -> Result<PairRequest, AuthError>;
    pub async fn approve_pair_request(&self, by: UserKey, code: &str) -> Result<PairRequestApproved, AuthError>;
    pub async fn poll_pair_request(&self, id: &PairingId, poll_secret: &str) -> Result<PairPoll, AuthError>;
}

#[derive(Clone, Debug)]
pub struct Principal { pub access: Access, pub user: UserId, pub username: String, pub role: Role,
    pub permissions: Permissions, pub scopes: Scopes, pub via: Via, pub device: Option<(DeviceKey, DeviceId)> }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum Via { Session, DeviceKey, ApiKey, AppPassword, MediaToken }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct Scopes(u8);
impl Scopes { pub const LIBRARY: Self; pub const STREAM: Self; pub const DOWNLOAD: Self; pub const WRITE: Self;
    pub const REQUEST: Self; pub const ALL: Self; pub const fn bits(self) -> u8; pub fn contains(self, other: Self) -> bool;
    pub fn parse(s: &str) -> Option<Self>; }
#[derive(Clone, Copy, Debug)] pub enum RouteClass { Api, Media, Admin }
pub enum KeyKind { Device, ApiKey, AppPassword }

pub struct SecretKey(/* zeroized [u8; 32] */);
impl SecretKey { pub fn load_or_create(path: &std::path::Path) -> Result<(Self, bool), AuthError>; } // bool: created now
pub fn seal(key: &SecretKey, purpose: &str, plaintext: &[u8]) -> Result<Vec<u8>, AuthError>; // nonce generation can fail
pub fn open(key: &SecretKey, purpose: &str, sealed: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>, AuthError>;

pub struct SubsonicCredentials { pub username: Option<String>, pub token: Option<String>, pub salt: Option<String>,
    pub password: Option<String>, pub api_key: Option<String> }
pub enum SubsonicAuthError { Missing, WrongCredentials, TokenNotSupported, MechanismNotSupported, Conflicting,
    InvalidApiKey, RateLimited { retry_after: std::time::Duration }, Internal }

#[derive(Debug, thiserror::Error)]
pub enum AuthError { #[error("unauthenticated")] Unauthenticated, #[error("forbidden")] Forbidden,
    #[error("csrf")] Csrf, #[error("rate limited")] RateLimited { retry_after: std::time::Duration },
    #[error("media token expired")] MediaTokenExpired, #[error("setup required")] SetupRequired,
    #[error("conflict: {0}")] Conflict(String), #[error("invalid: {0}")] Invalid(String),
    #[error("credential in query string")] CredentialInQuery, #[error("not found")] NotFound,
    #[error(transparent)] Db(#[from] DbError), #[error(transparent)] Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")] Unimplemented(&'static str) }
// AuthError implements IntoResponse with the api.md error shape; it is the extractors' rejection.
// IssuedDeviceKey { secret, key: KeyId, device: DeviceId, device_name, principal: Principal } (for PairResult).
// PairPoll { Pending, Approved(IssuedDeviceKey), Expired }; a poll after the key was handed out is AuthError::NotFound.

// Axum extractors (module `extract`). The state must give access to Arc<Auth> via FromRef.
pub struct Authed<const SCOPE: u8>(pub Principal);   // Api routes; e.g. Authed<{ Scopes::WRITE.bits() }>
pub struct MediaAuthed(pub Principal);               // header, cookie or mt
pub struct AdminSession(pub Principal);              // admin role, session only, CSRF checked
pub struct SessionUser(pub Principal);               // any signed-in browser session, CSRF checked on changes
pub struct ClientIp(pub std::net::IpAddr);
```

### 3.4 trackside-scan

```rust
pub struct Scanner { /* Db, Media's ArtworkStore, ScannerConfig, EventBus, per-library state, threads */ }
impl Scanner {
    pub fn start(db: Db, artwork: ArtworkStore, config: &ScannerConfig, uploads: &UploadsConfig,
                 events: EventBus, clock: std::sync::Arc<dyn Clock>, shutdown: tokio_util::sync::CancellationToken)
        -> Result<std::sync::Arc<Self>, ScanError>;
}
impl ScanControl for Scanner { /* trait from core */ }

// Public for tests and for WP10's verification.
pub fn read_file(path: &std::path::Path) -> Result<ParsedFile, ReadError>;        // tags, properties, pictures, lyrics
pub fn audio_digest(path: &std::path::Path, container: Container) -> std::io::Result<(AudioDigest, DigestMode)>;
pub fn listing_hash(entries: &[DirEntrySummary]) -> [u8; 32];
pub enum DigestMode { Payload, WholeFile }
```

### 3.5 trackside-acquire

```rust
pub struct Acquirer { /* Db, ScanControl, Media, providers, limiters, config, EventBus, Clock */ }
impl Acquirer {
    pub fn start(db: Db, scan: std::sync::Arc<dyn ScanControl>, media: std::sync::Arc<Media>, config: &AcquireConfig,
                 providers: Vec<std::sync::Arc<dyn Provider>>, events: EventBus, clock: std::sync::Arc<dyn Clock>,
                 shutdown: tokio_util::sync::CancellationToken) -> std::sync::Arc<Self>;
}
impl AcquireControl for Acquirer { /* trait from core */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderName { Ytdlp, Soulseek }

pub trait Provider: Send + Sync + std::fmt::Debug + 'static {
    fn name(&self) -> ProviderName;
    fn search<'a>(&'a self, query: &'a SearchQuery, cancel: tokio_util::sync::CancellationToken)
        -> BoxFuture<'a, Result<Vec<Candidate>, ProviderError>>;
    fn fetch<'a>(&'a self, candidate: &'a Candidate, staging: &'a std::path::Path, progress: ProgressSink,
                 cancel: tokio_util::sync::CancellationToken) -> BoxFuture<'a, Result<FetchedFile, ProviderError>>;
    /// Providers that can play while downloading return a growing file and a completion future.
    fn fetch_streaming<'a>(&'a self, _candidate: &'a Candidate, _staging: &'a std::path::Path, _progress: ProgressSink,
                           _cancel: tokio_util::sync::CancellationToken)
        -> Option<BoxFuture<'a, Result<StreamingFetch, ProviderError>>> { None }
    fn resume<'a>(&'a self, candidate: &'a Candidate, staging: &'a std::path::Path, progress: ProgressSink,
                  cancel: tokio_util::sync::CancellationToken) -> BoxFuture<'a, Result<FetchedFile, ProviderError>>;
    fn health<'a>(&'a self) -> BoxFuture<'a, ProviderHealth>;
}

pub struct SearchQuery { pub title: String, pub artist: String, pub album: Option<String>,
    pub duration_ms: Option<u32>, pub free_text: Option<String> }
pub struct Candidate { pub provider: ProviderName, pub provider_ref: serde_json::Value, pub title: String,
    pub artist: String, pub album: Option<String>, pub duration_ms: Option<u32>, pub container: Option<Container>,
    pub codec: Option<Codec>, pub bitrate_kbps: Option<u32>, pub sample_rate: Option<u32>, pub bit_depth: Option<u8>,
    pub lossless: bool, pub size: Option<u64>, pub hints: SourceHints }
pub enum SourceHints {
    Video { uploader: String, topic_channel: bool, official: bool, music_video: bool },
    Peer { username: String, folder: String, free_slot: bool, queue_length: u32, upload_speed: u64 },
}
#[derive(Clone)] pub struct ProgressSink(/* Arc<watch::Sender<Progress>> */);
impl ProgressSink { pub fn new() -> (Self, watch::Receiver<Progress>); pub fn report(&self, bytes: u64, total: Option<u64>); }
pub struct FetchedFile { pub path: std::path::PathBuf, pub bytes: u64, pub thumbnail: Option<std::path::PathBuf> }
pub struct StreamingFetch { pub growing: GrowingFile, pub done: BoxFuture<'static, Result<FetchedFile, ProviderError>> }

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("unavailable: {0}")] Unavailable(String),
    #[error("rate limited")] RateLimited { retry_after: Option<std::time::Duration> },
    #[error("bot check")] BotCheck,
    #[error("not found")] NotFound,
    #[error("stalled")] Stalled,
    #[error("rejected: {0}")] Rejected(String),
    #[error("cancelled")] Cancelled,
    #[error("too large")] TooLarge,
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error("{0}")] Other(String),
}
pub enum ProviderHealth { Ok { version: Option<String> }, Degraded(String), Cooldown { until: Millis },
    Outdated { version: String }, Unavailable(String), Disabled }

// Pure functions, unit-tested with tables.
pub struct RankOptions { pub allow_versions: bool, pub reject_words: Vec<String>, pub prefer: Vec<ProviderName>,
    pub max_file_bytes: u64, pub min_lossy_kbps: u32 }
pub struct Ranked { pub candidate: Candidate, pub score: f32, pub exact: bool }
pub fn rank(query: &SearchQuery, candidates: Vec<Candidate>, options: &RankOptions) -> Vec<Ranked>;

pub struct Expectation { pub duration_ms: Option<u32>, pub max_bytes: u64 }
pub async fn verify(path: &std::path::Path, staging_root: &std::path::Path, expected: &Expectation, media: &Media)
    -> Result<VerifiedFile, Rejection>;
pub fn safe_file_name(template: &str, tags: &TagValues) -> std::path::PathBuf;

pub struct YtdlpProvider { /* path, js runtime, cookies, limiter, cooldown */ }
impl YtdlpProvider { pub fn new(config: &YtdlpConfig, data_dir: &std::path::Path, max_file_bytes: u64) -> Self; }
pub struct SoulseekProvider { /* reqwest client, base url, api key, downloads dir, limits */ }
impl SoulseekProvider { pub fn new(config: &SoulseekConfig) -> Result<Self, ProviderError>; }
```

### 3.6 trackside-subsonic and trackside-api

```rust
// trackside-subsonic
#[derive(Clone)]
pub struct SubsonicState { pub db: Db, pub auth: std::sync::Arc<Auth>, pub media: std::sync::Arc<Media>,
    pub scan: std::sync::Arc<dyn ScanControl>, pub config: std::sync::Arc<Config>, pub server_id: uuid::Uuid }
pub fn router(state: SubsonicState) -> axum::Router; // full paths (/rest/...), merged by the server

// trackside-api
#[derive(Clone)]
pub struct ApiState { pub db: Db, pub auth: std::sync::Arc<Auth>, pub media: std::sync::Arc<Media>,
    pub scan: std::sync::Arc<dyn ScanControl>, pub acquire: std::sync::Arc<dyn AcquireControl>,
    pub events: EventBus, pub config: std::sync::Arc<Config>, pub server: ServerInfo, pub health: std::sync::Arc<dyn HealthSource> }
pub fn router(state: ApiState) -> axum::Router;  // full paths: /api/v1/** including /admin/**
// impl FromRef<ApiState> for Arc<Auth> (and for SubsonicState), so the auth extractors work.
pub mod wire;  // every request and response type in api.md, serde, with fixtures in tests/fixtures
pub trait HealthSource: Send + Sync + 'static { fn overview(&self) -> BoxFuture<'_, wire::admin::Overview>; fn ready(&self) -> wire::Ready; }

// trackside-server
pub async fn run(config: Config, report: ConfigReport, shutdown: tokio_util::sync::CancellationToken) -> anyhow::Result<()>;
pub async fn run_with(config, report, shutdown, on_listening: impl FnOnce(&[SocketAddr]) + Send) -> anyhow::Result<()>; // tests learn the ports
pub fn health_router() -> axum::Router; // /healthz
```

---

## 4. Work packages

Each package lists its scope, key decisions it must follow, and acceptance tests. "Tests" means
`cargo test` in the package's crate unless stated. Every package also meets section 5.

### WP1-S: Skeleton (wave 0)

Scope:

- `Server/Cargo.toml` with members, `[workspace.package]` (edition 2024, rust-version 1.90,
  license GPL-3.0-only, publish false), every dependency in section 6, and the lint table copied
  from `Engine/Cargo.toml` (`unsafe_code = "forbid"`, clippy pedantic, `unwrap_used = "deny"`).
- `Server/rust-toolchain.toml`: channel `1.99.0`, components clippy and rustfmt, targets
  `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`.
- All eight crates with `Cargo.toml` and `src/lib.rs` (and `src/main.rs` for the server) exposing
  exactly the section 3 signatures with `todo!()` bodies, except in core, where ids, time, access,
  events, `Db`, `WriteTx`, `Change` and migration 1 are real.
- Fakes in core under a `testing` feature: `FakeScanControl`, `FakeAcquireControl` (feature
  absent), `TestClock`, `TestDb::new()` (in-memory with migration 1; public `db`, `clock`,
  `events`, derefs to `Db`; clocks start at `testing::TEST_START`, 2026-11-14T09:30:00Z).

Acceptance:

- `cargo check --workspace --all-targets` and `cargo clippy --workspace --all-targets -- -D warnings`
  pass on Linux and macOS.
- `migration_1_applies_to_an_empty_database`, `migration_1_is_idempotent_on_reopen`,
  `opening_a_newer_database_is_refused`.
- `change_log_keeps_one_row_per_entity_with_increasing_seq`.
- `write_publishes_changed_after_commit_and_not_after_rollback`.
- Id tests: `song_id_derivation_is_stable` (golden vectors), `ids_parse_case_insensitively`,
  `ids_reject_wrong_prefix`.
- `tracks_model_is_linked`: `song_key("Halo", "Beyoncé")` equals `"halo\u{1F}beyonce"`.

### WP1: Core (wave 1)

Scope: everything else in `trackside-core`: repositories, `ingest` (the id rules of architecture
section 6), sync, search, similarity (local signals), config loading with env and `_FILE`
handling and the per-key source report, settings, audit, idempotency storage, acquisition queue
storage, change-log pruning, missing-song purge, and the folding-version check
(`settings.schema.tracks_model`).

Acceptance:

- Ingest: `retagged_file_keeps_its_id`, `moved_file_keeps_its_id`,
  `case_only_rename_keeps_its_id`, `file_moved_between_libraries_keeps_its_id`,
  `replaced_file_with_same_mbz_track_id_keeps_its_id_and_bumps_content_version`,
  `exact_duplicate_gets_a_disambiguated_id`, `rebuilt_database_assigns_the_same_ids` (scan into a
  fresh database twice, compare),
  `returning_file_after_purge_reattaches_stars_and_playlists`.
- Access: a proptest generating users, libraries and grants asserting no repository read ever
  returns a song, album, artist or playlist outside the caller's access.
- Sync: `snapshot_then_delta_equals_a_fresh_snapshot` (proptest over random edits during and
  after a snapshot), `cursor_below_pruned_seq_requires_reset`, `library_grant_change_requires_reset`,
  `song_moving_to_hidden_library_arrives_as_delete`.
- Listens: `resubmitted_batch_is_all_duplicates`, `future_listen_is_rejected`,
  `play_stats_follow_listens`.
- Search: `search_matches_tracks_folding` (golden cases from Tracks' search tests: "dont" finds
  "Don't", "ac dc" finds "AC/DC", "&" as "and", diacritics), prefix matching, ranking order.
- Config: every key in architecture section 11 has a default, an env name that parses, and a
  `_FILE` variant for secrets; `config_show_redacts_secrets`.
- Bench (criterion, not in CI): `repo::albums::list` and `search` at 500k synthetic songs within
  the budgets of architecture section 15.

### WP2: Scanner (wave 1)

Scope: walking (raw byte paths, symlink loop detection by device and inode, ignore markers),
folder skipping by mtime and listing hash, tag reading with lofty (strict then relaxed, filename
fallback), payload locators and digests, embedded and sidecar artwork through `ArtworkStore`,
lyrics (embedded `USLT`/`SYLT`/`LYRICS`, sidecar `.lrc`), batching into `core::ingest`, settle
and change-during-read deferral, per-file timeout and quarantine with `catch_unwind`, the
availability monitor (dedicated thread, 5 s deadline, per-library semaphore), the empty-library
guard, the watcher (notify 8, per-folder debounce, inotify limit fallback to polling), periodic
quick and full scans, `import_file`, uploads processing and the inbox folder.

Acceptance (with temp directories and generated audio fixtures committed under
`tests/fixtures/`, small FLAC, MP3, M4A, Ogg Opus, WAV and AIFF files):

- `digest_ignores_tag_rewrites` for every container (rewrite tags with lofty, compare digests).
- `digest_differs_for_different_audio`.
- `quick_scan_with_no_changes_reads_no_files` (count opens through an instrumented reader).
- `file_written_during_scan_is_deferred_not_imported`.
- `unmounted_library_is_marked_unavailable_and_nothing_goes_missing` (replace the root with an
  empty directory and with a path on a different device via a bind mount when available; skip
  with a message otherwise).
- `unreadable_folder_does_not_mark_its_songs_missing`.
- `symlink_loop_terminates`.
- `corrupt_tags_fall_back_to_file_name` and `panicking_parser_quarantines_the_file` (inject a
  reader that panics).
- `watcher_event_makes_song_visible_within_eight_seconds` (paused tokio time where possible).
- `import_file_returns_the_final_id_and_a_later_rescan_changes_nothing`.
- Throughput bench: 10k generated files on tmpfs at ≥ 1000 files/s (sanity check for the budget).

### WP3: Media (wave 1)

Scope: range evaluation and file serving, growing-file serving, playback decisions, profiles and
ffmpeg argument vectors, `FfmpegTranscoder`, single-flight job registry with priorities and the
global semaphore, transcode cache with LRU and free-space floor, HLS job runner with the
synthesised VOD playlist, seek-ahead restart and final remux, progressive transcode for Subsonic
with tee to cache, artwork store and resizing (image crate, Lanczos3, snapped sizes, dimension cap),
`probe` and `decode_check`.

Acceptance:

- Range table tests for `evaluate`: no range, `bytes=0-1` (AVPlayer probe), open, suffix,
  multiple ranges as full, unsatisfiable, `If-Range` with matching and stale ETag and date,
  `If-None-Match`, `HEAD`.
- `serve_file_rejects_a_file_changed_since_indexing`.
- Decision table: direct within limits, over-bitrate to `aac-256`, unsupported codec to file or
  HLS, `max_bitrate_kbps` from the user overriding the client, busy transcoder with and without
  fallback, no ffmpeg.
- With the fake transcoder: `concurrent_requests_share_one_job`, `priority_order_is_respected`,
  `cache_evicts_least_recently_used`, `cache_stays_above_min_free` (fake free-space source),
  `hls_playlist_has_all_segments_before_encoding_finishes`, `segment_wait_times_out_with_503`,
  `seek_far_ahead_starts_a_second_job`.
- With real ffmpeg (tests marked `#[ignore]` unless `TRACKSIDE_TEST_FFMPEG=1`, run in CI):
  `aac_256_output_is_bit_exact_across_runs`, `hls_segments_play_back_to_the_full_duration`
  (ffprobe the concatenation), `m4a_has_an_edit_list_for_priming`.
- `missing_artwork_is_404`, `resized_artwork_is_cached_and_immutable`,
  `oversized_image_is_rejected_before_decode`.

### WP4: Auth (wave 1)

Scope: everything in section 3.3: Argon2id with a semaphore, sessions, CSRF and Origin checks,
device keys, API keys, app passwords (sealing with XChaCha20-Poly1305 and subkeys from
`blake3::derive_key`), media tokens, setup token file and banner text, invites, password resets,
pairing offers and requests, rate limiters (governor) and failure backoff, trusted-proxy client
IP, Subsonic credential verification with error mapping, extractors, key cache with invalidation
on `CredentialRevoked`, log redaction helper for query strings.

Acceptance:

- `login_failure_message_is_identical_for_unknown_and_wrong_password` and timing within 10 % (run
  Argon2 on a dummy hash for unknown users).
- `fifth_failure_triggers_backoff_for_that_user_and_ip_only`.
- `revoked_key_is_rejected_within_one_second` (cache invalidation).
- `media_token_is_rejected_after_device_revoked`, `media_token_rejected_on_api_routes`.
- `key_in_query_string_is_rejected_outside_media_routes`.
- `session_write_without_csrf_header_is_rejected`, `foreign_origin_is_rejected`.
- `subsonic_token_auth_works_only_with_app_passwords`, `api_key_with_username_is_error_43`,
  `login_password_is_never_accepted_by_subsonic`.
- `setup_token_works_once`, `setup_is_refused_after_an_admin_exists`.
- `last_admin_cannot_be_demoted` (with WP1 repositories).
- `pairing_offer_dies_after_five_wrong_codes`, `pair_request_returns_the_key_exactly_once`.
- `forwarded_headers_are_ignored_from_untrusted_peers`, IPv4 and IPv6 cases.
- `lost_secret_key_invalidates_app_passwords_only`.
- Proptest: `seal` then `open` round-trips; tampered ciphertext fails.

### WP5: Subsonic (waves 1 and 2)

Scope: `/rest` routing for GET and formPost, `.view` suffixes, XML (quick-xml with serde) and
JSON envelopes, every endpoint marked "yes" in api.md section 17 with its parameters, error codes,
non-200 statuses for failed media requests, mapping from core records to Subsonic shapes including
OpenSubsonic fields, `getOpenSubsonicExtensions` without auth, legacy empty `search3` paging in
stable order.

Acceptance:

- Snapshot tests (insta) for the XML and JSON of every implemented endpoint against a seeded test
  library.
- A conformance suite replaying the requests Tracks' `SubsonicClient` makes today (listed in the
  client map: ping, getArtists, getArtist, getAlbum, getAlbumList2 types, search3 with each
  parameter, getSimilarSongs2 with a song id, getTopSongs, getArtistInfo2, getRandomSongs,
  scrobble with ms time, star, unstar, stream raw and mp3, download, getCoverArt) and asserting the
  fields Tracks decodes are present and typed as it expects (string ids, `created` parseable by
  ISO8601 with fractional seconds).
- `cover_art_missing_is_404_with_error_70`.
- `star_has_no_side_effects` (no acquisition row, no change other than the star).
- `stream_does_not_count_a_play`, `scrobble_submission_counts_once_per_time`.
- `plus_sign_in_query_is_decoded` (Tracks encodes `+` as `%2B`).
- `member_cannot_see_ungranted_library_through_any_endpoint` (iterate all endpoints).
- `apiKey_and_u_together_is_43`; formPost body equals query behaviour.

### WP6: Native API and server (waves 1 and 2)

Scope: every `/api/v1` endpoint in api.md including admin, wire types and fixtures, SSE with
audience filtering and keep-alives, idempotency middleware, error mapping, compression, request
ids, security headers, dashboard asset serving (`rust-embed` from `Server/dashboard/`, CSP), and
the binary: CLI (`serve`, `admin reset-password|set-role|list-users|create-setup-token`,
`backup create|list|restore`, `check`, `healthcheck`, `config show`, `version`), config loading
via core, data dir lock, startup sequence (lock, secret key, open and migrate with pre-migration
backup, setup banner, first library from `music_dir`, start scanner, media, acquirer, listeners),
listeners with TLS modes (files with reload, ACME feature, self-signed with rcgen), `[::]` with
IPv4 fallback, trusted proxies, base path, mDNS advertising, candidate URL detection, scheduler
(backups, quick scans, change-log pruning, session and pairing cleanup, idempotency pruning,
transcode cache eviction, WAL checkpoint), graceful shutdown, systemd notify and watchdog,
`/healthz`, `/readyz`, `/metrics`.

Acceptance:

- `fixtures_match_wire_types` for every response in api.md (serialise seeded responses, compare to
  `tests/fixtures/*.json`; examples in api.md must deserialise).
- Endpoint tests on an ephemeral port with a real `Db` and fake scan and acquire controls:
  pairing flows end to end, login, sync snapshot then delta, listens idempotency, playlists with
  `If-Match` conflict, play queue last writer wins, stars, decisions, stream with ranges,
  downloads with `Authorization`, art 404, SSE receives `changed` after a write and `resync` when
  lagging, admin endpoints require a session and CSRF, `features.acquisition` absent unless every
  condition in api.md section 7 holds, acquisition endpoints 404 when absent.
- `idempotency_key_replays_and_detects_mismatch`.
- CLI: `reset_password_works_while_server_runs`, `restore_refuses_while_locked`,
  `second_server_on_same_data_dir_exits`, `healthcheck_exits_nonzero_when_down`.
- Startup: `zero_config_start_listens_and_prints_setup_token`, `music_dir_creates_first_library_once`,
  `crash_during_migration_leaves_previous_schema_and_a_backup` (kill between statements via a
  test hook), `database_on_network_fs_is_refused` (fake statfs).
- `shutdown_drains_and_checkpoints` within the grace period.
- `forwarded_proto_and_base_path_produce_correct_absolute_urls`.

### WP7: Dashboard (wave 1)

Scope: `Server/dashboard/` with `index.html`, `setup.html`, `invite.html`, `reset.html`,
`app.js` (ES module router), one module per screen under `pages/` (overview, users, invites,
libraries and scans, devices and keys, my account with app passwords and pairing, backups,
audit log, settings, acquisition providers and requests), `api.js` (fetch wrapper adding the CSRF
header and mapping error codes to messages), `style.css` (system fonts, light and dark via
`prefers-color-scheme`), and `dev/fixtures/` plus a `?preview` mode that answers from fixtures
so the pages can be built before the server is ready. No external URLs, no inline scripts or
styles (CSP), all dynamic text through `textContent`, keyboard accessible, works at 360 px wide.

Acceptance:

- `grep -R "http" Server/dashboard --include=*.{html,js,css}` finds no external URL (CI check).
- Every admin endpoint in api.md section 16 is called by at least one page (a script in
  `dev/check-coverage.mjs` run by CI with Node only at CI time, or a Rust test in WP6 that parses
  `api.js` route constants; WP7 picks one and documents it).
- Manual checklist in `Server/dashboard/README.md`: setup with token, create invite and accept,
  pair a device by QR, revoke it, add a library and watch the scan, trigger a backup, enable a
  provider, view a failed acquisition, all in light and dark mode and at 360 px.
- Provider names appear only on the acquisition pages.

### WP8: Packaging, CI and server docs (wave 1, images in wave 2)

Scope:

- `Server/packaging/docker/Dockerfile` multi-stage: build with `cargo-zigbuild` for musl amd64
  and arm64; three targets: `slim` (`gcr.io/distroless/static-debian12:nonroot`, no ffmpeg),
  `standard` (same base plus a pinned static ffmpeg build without libfdk_aac, the default `latest`
  tag), `acquire` (`gcr.io/distroless/cc-debian12:nonroot` for glibc, plus ffmpeg, a pinned
  yt-dlp standalone binary and Deno, tagged `-acquire`). `HEALTHCHECK` runs `trackside healthcheck`.
  `ENV TRACKSIDE_DATA_DIR=/data TRACKSIDE_MUSIC_DIR=/music`, `VOLUME /data`, `EXPOSE 4747`, user
  65532.
- `Server/packaging/compose.yaml`: one `trackside` service by default (`/data` volume, `/music`
  read-only, port 4747); a commented `network_mode: host` block for mDNS; profile `acquire` that
  switches to the `-acquire` image, mounts `./acquired:/acquired` read-write and
  `./slskd/downloads:/slskd-downloads`, and adds an `slskd` service (pinned image) with
  `./acquired` mounted read-only as its only share and `./slskd/downloads` as its downloads
  directory; profile `tailscale` with a sidecar example.
- `Server/packaging/systemd/trackside.service` (`Type=notify`, `WatchdogSec=30`,
  `DynamicUser=yes`, `StateDirectory=trackside`, `ReadOnlyPaths=/srv/music`,
  `ProtectSystem=strict`, `NoNewPrivileges=yes`, `Restart=on-failure`), plus `.deb` and `.rpm`
  packages. `cargo-deb` and `cargo-generate-rpm` read metadata from the server crate's manifest,
  which WP6 owns, so WP8 writes the metadata in `packaging/deb/metadata.toml` and asks WP6 to
  include it.
- Homebrew formula with `brew services`; Unraid, TrueNAS, CasaOS and Synology templates.
- `.github/workflows/server.yml`: path filter `Server/**` and `Engine/crates/tracks-model/**`;
  fmt, clippy `-D warnings`, tests on Linux and macOS, ffmpeg tests, `cargo deny`, musl builds,
  e2e at 50k with regression check (WP11), multi-arch image build and push with cosign signing and
  SBOM on tags.
- Docs in `Server/README.md` and `Server/docs/ops/`: install (Docker, compose, binary, systemd,
  Homebrew, NAS), first run and the setup token, libraries and permissions, remote access
  (Tailscale first, Caddy, built-in ACME, Cloudflare Tunnel for the dashboard only, mDNS and host
  networking), reverse proxy recipes (Caddy and nginx with SSE settings), backups and restore
  and Litestream, upgrading and downgrading, troubleshooting (inotify limits, NAS mounts,
  permissions), configuration reference (generated from WP1's config report), acquisition
  (enabling providers, slskd setup and API key, cookies, updating yt-dlp, sharing only the
  acquired folder, and one neutral paragraph: "You are responsible for what you download and
  share under the laws where you live.").

Acceptance:

- `docker run` of each image variant starts with no configuration, prints the setup token, and
  passes its healthcheck; `standard` reports ffmpeg found; `acquire` reports yt-dlp and Deno.
- Images run as non-root with a read-only root filesystem (`--read-only` with `/data` writable).
- `docker compose --profile acquire up` brings up both services and slskd's share list contains
  only the acquired folder.
- `systemd-analyze verify` passes on the unit; a VM test starts it and sees `READY=1`.
- CI workflow runs green on a branch.
- Tracks' README and app copy are not changed by this package (acquisition is documented only in
  server docs).

### WP9a: Tracks client core (wave 1)

Scope, in new files under `TracksCore/Sources/TracksCore/Library/Trackside/` (Swift 6, `Sendable`,
`nonisolated`, tested with Swift Testing):

- `TracksideWire.swift`: `Codable` types for every api.md shape Tracks uses.
- `TracksideClient.swift`: requests over a `TracksideTransport` protocol (like
  `SubsonicTransport`), bearer auth, media URL building with `mt`, error decoding.
- `MusicServerClient.swift`: the protocol both clients conform to (search, album, artist, top and
  similar songs, similar artists, random songs, album lists, stream and download URLs, artwork
  URL, star, listens), plus `SubsonicClient` conformance in an extension file in the same folder.
- `ServerEndpoints.swift`: actor racing candidate URLs (architecture section 10).
- `TracksideSync.swift`: applies snapshot and delta pages to `[LocalTrack]` and artist refs,
  stores the cursor, handles `sync_reset`.
- `ListenOutbox.swift`: durable batched listens with `client_listen_id`.
- `PairingPayload.swift`: parses `trackside://pair` links and QR payloads.
- `TracksideEvents.swift`: SSE parser over `URLSession.bytes`.
- `Acquisition.swift`: request and status types, gated by `features.acquisition`.

Acceptance (Swift Testing, `cd TracksCore && swift test`):

- Decodes every fixture in `Server/crates/trackside-api/tests/fixtures/` (read by path from the
  repository at test time).
- `song_key` in fixtures equals `LocalTrack.identity` for the same title and artist.
- Racing picks LAN within 300 ms of the first answer, rejects a wrong `server_id`, re-races on
  path change (stub transport with delays).
- Sync applier: snapshot then deltas produce the same mirror as a fresh snapshot; reset keeps the
  old mirror until the new snapshot completes.
- Outbox survives relaunch (temp directory), resends on failure, drops `rejected`, treats
  `duplicate` as done.
- SSE parser handles comments, multi-line data, reconnect with `Last-Event-ID`.
- With no `features.acquisition` in `/me`, the acquisition model reports unavailable and makes no
  requests.

### WP9b: Tracks app integration (wave 2)

Scope: the edits listed in section 2 for WP9b, following architecture section 17: server kind
detection, the Trackside `ServerForm` paths (QR, Bonjour, code, password), Keychain storage of the
device key under the existing service, `MusicServers` and `ServerDiscovery` through
`MusicServerClient`, delta sync for Trackside, playback decisions and prepare-next,
downloads with the header, artwork without stand-in probing, listens through the outbox,
`ServerKeeps` bypassed for Trackside, throttles relaxed for Trackside only, acquisition UI only
when `features.acquisition` is present and no provider names anywhere, the one-time "match my
downloads and playlists" step when an entry switches from another server to Trackside, and
`_trackside._tcp` in both `Info.plist` files. Navidrome and other Subsonic servers keep
today's behaviour.

Acceptance:

- Existing tests pass unchanged for Subsonic servers (`LibraryTests` SubsonicTests,
  `ServerKeepsTests`, `AudioQualityTests`).
- New tests: kind detection from capabilities, fallback to Subsonic on 404, keychain account is
  the entry id, acquisition UI hidden without the feature (view model tests), no string in the
  app target names YouTube, Soulseek, yt-dlp or slskd (a test that scans the app's localisable
  strings and source literals).
- Run on a device or simulator against a local server (from WP6): pair by QR, browse, play on
  Wi-Fi (direct) and with Network Link Conditioner on a cellular profile (AAC), download and
  resume after toggling airplane mode, scrobble offline then online, switch networks and observe
  re-racing. The pull request says what was checked and what could not be.

### WP10: Acquisition (wave 1)

Scope: everything in section 3.5 and architecture section 9: `Acquirer` with the leased job
runner, dedupe, quotas, per-provider limits, cooldowns, staging and janitor, ranking,
verification, tagging with lofty, import through `ScanControl`, instant play through
`fetch_streaming` and media HLS, SSE events, audit entries, the admin operations including the
hash-checked yt-dlp updater, and the two providers.

Acceptance (with fake providers implementing `Provider` and a fake `ScanControl`, unless stated):

- `request_for_owned_song_returns_existing_id_immediately`.
- `duplicate_request_joins_the_active_one_and_does_not_count_against_quota`.
- `restart_resumes_a_downloading_job_from_its_lease` (drop the `Acquirer`, start a new one on the
  same database, assert the provider's `resume` is called once).
- `failed_candidate_falls_back_to_the_next` and `all_candidates_failed_retries_with_backoff`
  (paused time).
- `quota_exceeded_is_429_with_reset_time`, admins exempt.
- `bot_check_puts_provider_in_cooldown` and `cooldown_doubles_up_to_six_hours`.
- `disk_full_pauses_and_resumes` (fake free-space source).
- `staging_is_removed_on_done_failed_and_cancelled` and `janitor_removes_orphans`.
- Ranking table: live, remix, karaoke rejected unless `allow_versions`; duration tolerance;
  `- Topic` preferred; lossless preferred; artist credit matching through tracks-model with
  "A feat. B" titles.
- Verification: rejects a ZIP, an ELF, a symlink, a path outside staging, an MP4 with a video
  track, a truncated FLAC (real ffmpeg, ignored without it), a duration mismatch; accepts each
  allowed container.
- `safe_file_name` proptest: never contains a separator, NUL, control character, leading dot or
  reserved name; never exceeds 180 bytes per component.
- `done_event_carries_the_song_id_and_the_song_is_in_sync` (with real WP1 and WP2 in wave 2).
- `instant_play_serves_hls_before_download_completes` (fake streaming provider, real media).
- `view_never_contains_provider_names` (serialise every `AcquisitionView` variant, scan for the
  names).
- Providers: ytdlp argument vectors built exactly as specified and URLs only from validated ids
  (unit tests over a fake executable script in a temp dir that records its argv); progress
  template parsing; slskd client against a local mock HTTP server (search, responses, enqueue,
  transfer states, stall, cancel, 401, connection refused).

### WP11: End to end, performance and hardening (wave 3)

Scope: `Server/tests-e2e/` package with a synthetic library generator (50k and 500k songs of tiny
valid audio files with realistic tag distributions, nested folders, duplicates, odd Unicode and
NFD names, unreadable folders), a black-box client that drives the binary over HTTP, and
scenarios.

Acceptance:

- Every budget in architecture section 15 measured and written to a JSON report; CI fails on a
  regression over 25 % against the stored baseline at 50k.
- Scenarios: NAS unmount and remount during a scan (bind mount where available), file moves and
  retags with id stability checks across a database rebuild, two devices for one user submitting
  overlapping listens, two users with different library grants never seeing each other's data,
  AVPlayer-style range probing, a slow client reading at 8 KiB/s while others stream, transcoder
  saturation with fallback, upgrade from the previous release's database with pre-migration
  backup, downgrade refusal, disk-full simulation on a small loop or tmpfs volume, kill -9 at
  random points during scans and acquisitions followed by restart and consistency checks.
- Fuzzing targets (cargo-fuzz, run nightly): payload locators, range header parsing, Subsonic
  query parsing, sync cursor decoding, LRC parsing, safe file name building.
- A release checklist in `Server/tests-e2e/RELEASE.md`.

---

## 5. Rules for every package

- Follow the architecture and API documents. If something in them is wrong or missing, implement
  the closest reasonable behaviour, note it in the package report, and do not edit the documents.
- Changing a section 3 signature needs WP1's agreement; WP1 makes the change in core and every
  dependent package adapts. Additive changes (new methods with default bodies, new enum variants
  behind `#[non_exhaustive]`) are preferred.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and the package's
  tests pass. No `unwrap()` or `expect()` outside tests (the workspace lint denies `unwrap`).
  `unsafe` is forbidden.
- Errors: `thiserror` enums in libraries, `anyhow` only in `trackside-server`'s binary code and in
  tests.
- No blocking calls on async workers: SQLite through `Db::read`/`write`, filesystem through
  `spawn_blocking` or the scanner's threads, CPU-heavy work (Argon2, image resizing) through the
  blocking pool with a semaphore.
- Never hold a `std::sync::Mutex` guard across `.await`.
- Subprocesses: argument vectors only, `kill_on_drop(true)`, timeouts, stderr captured and
  truncated into logs.
- Code explains itself through names and types. Comments only say why something is the way it is
  (a workaround, an external behaviour, a non-obvious invariant), as in CONTRIBUTING.md. Doc
  comments on public items say what and how in plain English.
- Tests: descriptive names (`what_happens_when_condition`), real SQLite (in memory or temp file)
  and real HTTP on ephemeral ports in preference to mocks, `tokio::time::pause` instead of sleeps,
  proptest for invariants, insta for large outputs.
- No network access in tests except to local mock servers.
- Logs never contain secrets; use the auth crate's redaction helper for any URL or query string.
- Each package report lists: what was built, deviations from these documents and why, what was
  tested, what could not be tested here (for example on-device AVPlayer behaviour), and any
  dependency or signature requests.

---

## 6. Workspace dependencies

Versions checked on crates.io with `cargo search` on 2026-11-14. WP1-S declares all of them in
`[workspace.dependencies]`; crates opt in with `.workspace = true`.

| Crate | Version | Features | Used by |
|---|---|---|---|
| tracks-model | path `../Engine/crates/tracks-model` | | core, acquire |
| tokio | 1.54 | rt-multi-thread, macros, fs, io-util, net, process, signal, sync, time | all |
| tokio-util | 0.7 | rt (CancellationToken), io | core, scan, media, acquire, server |
| futures-util | 0.3 | | core (BoxFuture), media, api |
| axum | 0.8 | http1, http2, json, query, form, tokio | media, auth, subsonic, api, server |
| axum-server | 0.9 | tls-rustls-no-provider | server |
| tower | 0.5 | | api, server |
| tower-http | 0.7 | compression-br, compression-gzip, trace, timeout, limit, request-id, set-header, sensitive-headers (check feature names against 0.7) | api, server |
| http | 1 | | media, auth |
| rustls | 0.23 | ring | server |
| rustls-acme | 0.15 | axum, ring, tls12, webpki-roots, no default features (optional feature `acme`) | server |
| rcgen | 0.14 | | server |
| rusqlite | 0.41 | bundled, functions, blob (0.42 is out; 0.41 is what rusqlite_migration 2.7 and Engine use) | core |
| rusqlite_migration | 2.7 | | core |
| serde | 1 | derive | all |
| serde_json | 1 | | all |
| quick-xml | 0.42 | serialize | subsonic |
| toml | 1.2 | | core |
| thiserror | 2 | | libraries |
| anyhow | 1 | | server, tests |
| tracing | 0.1 | | all |
| tracing-subscriber | 0.3 | env-filter, json, fmt | server |
| clap | 4.6 | derive, env | server |
| jiff | 0.2 | | core, server |
| uuid | 1.29 | v4, serde | core, server |
| blake3 | 1.8 | | core, auth, scan, media |
| sha2 | 0.11 | | auth |
| md-5 | 0.11 | | auth (Subsonic tokens) |
| argon2 | 0.6 | (defaults; 0.6 has no `std` feature) | auth |
| chacha20poly1305 | 0.11 | | auth |
| getrandom | 0.4 | | core, auth |
| base64 | 0.23 | | auth, core |
| subtle | 2.6 | | auth |
| zeroize | 1.9 | | auth |
| governor | 0.10 | | auth |
| ipnet | 2.12 | serde | auth, core |
| lofty | 0.25 | | scan, media, acquire |
| notify | 8.2 | | scan |
| image | 0.25 | jpeg, png, webp, gif | media |
| qrcode | 0.14 | svg | api |
| mdns-sd | 0.21 | | server |
| if-addrs | 0.15 | | server |
| rust-embed | 8.14 | | server |
| mime_guess | 2.0 | | server |
| httpdate | 1.0 | | media |
| percent-encoding | 2.3 | | auth, subsonic |
| sd-notify | 0.5 | | server |
| fs4 | 1.1 | | server (data dir lock) |
| reqwest | 0.13 | rustls-no-provider, json, stream (no default features; `rustls` pulls aws-lc-rs, which complicates musl builds, so the process installs ring as the rustls provider) | acquire (slskd, yt-dlp updater), enrichment |
| unicode-normalization | 0.1 | | core, scan |
| proptest | 1.12 | | dev |
| rstest | 0.28 | | dev |
| insta | 1.49 | json | dev |
| tempfile | 3.27 | | dev |
| criterion | 0.8 | | dev |

Not used, on purpose: `sqlx` (ADR 0002), `notify-debouncer-full` (WP2 debounces per folder
itself, which is what the rescan logic needs, without the debouncer's file-id tracking),
`metrics` and `metrics-exporter-prometheus` (a hand-written text endpoint covers the need),
`async-trait` (boxed futures in the two object-safe traits), `figment` (TOML plus a small env
overlay in core), `symphonia` (lofty gives properties; ffmpeg does the decode check), `tantivy`
(FTS5 is enough).
