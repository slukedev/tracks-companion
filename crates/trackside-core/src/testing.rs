//! Test helpers for every crate: an in-memory database, and fakes for the scan and acquire
//! controls. Enabled by the `testing` feature.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

pub use crate::time::TestClock;

use crate::access::{Access, Permissions, Role};
use crate::control::{
    AcquireControl, AcquireError, AcquireFeature, AcquireRequest, AcquisitionFilter,
    AcquisitionView, AdminAcquisitionView, BoxFuture, ExternalQuery, ExternalSearch, ImportOutcome,
    InstantStream, LibraryScanStatus, ProviderChange, ProviderStatus, ProviderTest, ScanControl,
    ScanControlError, ScanKind, ScanTrigger, SongOrigin,
};
use crate::db::{Db, DbError};
use crate::events::EventBus;
use crate::ids::{
    AcquisitionId, AudioDigest, FolderId, ImportId, LibraryId, LibraryKey, PublicId, ScanRunId,
    UserKey, random_id,
};
use crate::ingest::{
    FinishOutcome, Resolution, ScannedFile, ScannedProperties, ScannedTags, SeenSet,
};
use crate::records::{Codec, Container, Library, Page};
use crate::repo::libraries::NewLibrary;
use crate::repo::users::{NewUser, User};
use crate::time::Millis;

/// 2026-11-14T09:30:00Z, where test clocks start.
pub const TEST_START: Millis = Millis(1_794_648_600_000);

/// An in-memory database with migration 1, a [`TestClock`] and its own [`EventBus`].
#[derive(Clone, Debug)]
pub struct TestDb {
    pub db: Db,
    pub clock: Arc<TestClock>,
    pub events: EventBus,
}

impl TestDb {
    pub fn new() -> Result<Self, DbError> {
        let clock = Arc::new(TestClock::new(TEST_START));
        let events = EventBus::default();
        let db = Db::open_in_memory(events.clone(), clock.clone())?;
        Ok(Self { db, clock, events })
    }
}

impl TestDb {
    /// A library at `root` (which need not exist), granted to no member.
    pub fn add_library(&self, name: &str, root: &Path) -> Result<Library, DbError> {
        let new = NewLibrary {
            name: name.to_owned(),
            root: root.to_path_buf(),
            writable: false,
            watch: false,
            grant_all_users: false,
            granted_by: None,
        };
        self.db
            .write_blocking(move |tx| crate::repo::libraries::create(tx, &new))
    }

    /// A user with a placeholder password hash and grants to `libraries`.
    pub fn add_user(
        &self,
        username: &str,
        role: Role,
        libraries: &[LibraryKey],
    ) -> Result<User, DbError> {
        let new = NewUser {
            username: username.to_owned(),
            display_name: username.to_owned(),
            password_hash: String::from("$argon2id$test"),
            role,
            permissions: Permissions::default(),
            libraries: libraries.to_vec(),
            service: None,
        };
        self.db
            .write_blocking(move |tx| crate::repo::users::create(tx, &new))
    }

    /// What `user` may see.
    pub fn access(&self, user: UserKey) -> Result<Access, DbError> {
        self.db
            .read_blocking(|c| crate::repo::users::access(c, user))?
            .ok_or(DbError::NotFound)
    }

    /// Ingests files into a library as one scan batch.
    pub fn ingest(
        &self,
        library: LibraryKey,
        files: &[ScannedFile],
    ) -> Result<Vec<Resolution>, DbError> {
        let files = files.to_vec();
        self.db.write_blocking(move |tx| {
            let run: ScanRunId = random_id();
            crate::ingest::apply_batch(tx, library, &run, SongOrigin::Scan, &files)
        })
    }

    /// Ends a complete scan that saw exactly `seen`.
    pub fn finish_scan(
        &self,
        library: LibraryKey,
        seen: &[&str],
    ) -> Result<FinishOutcome, DbError> {
        let mut set = SeenSet::new();
        for path in seen {
            set.insert(path.as_bytes().to_vec());
        }
        self.db.write_blocking(move |tx| {
            let run: ScanRunId = random_id();
            crate::ingest::finish_run(tx, library, &run, &set, &[])
        })
    }
}

/// Makes [`Db::open`](crate::Db::open) with `FilesystemCheck::Detect` treat `dir` (and
/// everything under it) as a network filesystem, as a fake `statfs` would. For the
/// `database_on_network_fs_is_refused` test.
pub fn assume_network_filesystem(dir: &Path) {
    crate::db::hooks::NETWORK_DIRS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(dir.to_path_buf());
}

/// Makes the next migrations of the database at `path` fail halfway, inside their
/// transaction, as a crash between statements would. Cleared by [`clear_hooks`].
pub fn crash_migration_of(path: &Path) {
    crate::db::hooks::CRASH_MIGRATION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(path.to_path_buf());
}

/// Removes every hook set for paths under `dir`.
pub fn clear_hooks(dir: &Path) {
    for list in [
        &crate::db::hooks::NETWORK_DIRS,
        &crate::db::hooks::CRASH_MIGRATION,
    ] {
        list.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|p| !p.starts_with(dir));
    }
}

/// A stable stand-in audio digest: the same seed gives the same digest.
pub fn digest(seed: &str) -> AudioDigest {
    AudioDigest(*blake3::hash(seed.as_bytes()).as_bytes())
}

/// A scanned FLAC file at `rel_path` with the given tags, a digest from `rel_path`, three
/// minutes long. Tests change fields as they need.
pub fn scanned(rel_path: &str, title: &str, artist: &str, album: Option<&str>) -> ScannedFile {
    ScannedFile {
        rel_path: rel_path.as_bytes().to_vec(),
        rel_path_display: rel_path.to_owned(),
        folder: FolderId::derive(&LibraryId::from_hash(&[0; 32]), b""),
        size: 30_000_000,
        mtime_ns: 1_700_000_000_000_000_000,
        dev: None,
        ino: None,
        digest: digest(rel_path),
        tags: ScannedTags {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.map(str::to_owned),
            ..ScannedTags::default()
        },
        properties: ScannedProperties {
            duration_ms: 180_000,
            bitrate_kbps: Some(900),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            codec: Codec::Flac,
            container: Container::Flac,
        },
        artwork: None,
        lyrics: Vec::new(),
        tag_problem: None,
    }
}

impl std::ops::Deref for TestDb {
    type Target = Db;

    fn deref(&self) -> &Db {
        &self.db
    }
}

/// A scan the fake was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestedScan {
    pub library: LibraryId,
    pub kind: ScanKind,
    pub trigger: ScanTrigger,
    pub force_empty: bool,
}

/// A file the fake was asked to import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestedImport {
    pub library: LibraryId,
    pub rel_path: std::path::PathBuf,
    pub origin: SongOrigin,
}

#[derive(Debug, Default)]
struct FakeScanState {
    scans: Vec<RequestedScan>,
    imports: Vec<RequestedImport>,
    uploads: Vec<ImportId>,
    import_outcomes: Vec<ImportOutcome>,
    import_hold: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    status: Vec<LibraryScanStatus>,
    unavailable: Vec<LibraryId>,
}

/// Records what it is asked and answers from what the test set.
#[derive(Debug, Default)]
pub struct FakeScanControl {
    state: Mutex<FakeScanState>,
}

impl FakeScanControl {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeScanState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues the outcome of the next `import_file` call. Without one it fails with
    /// `Rejected`.
    pub fn push_import_outcome(&self, outcome: ImportOutcome) {
        self.state().import_outcomes.push(outcome);
    }

    /// Makes the next `import_file` call, once recorded, wait until the returned sender fires
    /// or drops. The returned receiver fires when the call starts waiting.
    pub fn hold_next_import(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, hold) = tokio::sync::oneshot::channel();
        self.state().import_hold = Some((entered_tx, hold));
        (entered, release)
    }

    pub fn set_status(&self, status: Vec<LibraryScanStatus>) {
        self.state().status = status;
    }

    pub fn set_available(&self, library: &LibraryId, available: bool) {
        let mut state = self.state();
        state.unavailable.retain(|l| l != library);
        if !available {
            state.unavailable.push(library.clone());
        }
    }

    pub fn requested_scans(&self) -> Vec<RequestedScan> {
        self.state().scans.clone()
    }

    pub fn requested_imports(&self) -> Vec<RequestedImport> {
        self.state().imports.clone()
    }

    pub fn processed_uploads(&self) -> Vec<ImportId> {
        self.state().uploads.clone()
    }
}

impl ScanControl for FakeScanControl {
    fn request_scan(
        &self,
        library: &LibraryId,
        kind: ScanKind,
        trigger: ScanTrigger,
        force_empty: bool,
    ) -> Result<ScanRunId, ScanControlError> {
        self.state().scans.push(RequestedScan {
            library: library.clone(),
            kind,
            trigger,
            force_empty,
        });
        Ok(random_id())
    }

    fn import_file<'a>(
        &'a self,
        library: &'a LibraryId,
        rel_path: &'a Path,
        origin: SongOrigin,
    ) -> BoxFuture<'a, Result<ImportOutcome, ScanControlError>> {
        Box::pin(async move {
            let (outcome, hold) = {
                let mut state = self.state();
                state.imports.push(RequestedImport {
                    library: library.clone(),
                    rel_path: rel_path.to_path_buf(),
                    origin,
                });
                let outcome = if state.import_outcomes.is_empty() {
                    Err(ScanControlError::Rejected(String::from(
                        "no outcome queued in FakeScanControl",
                    )))
                } else {
                    Ok(state.import_outcomes.remove(0))
                };
                (outcome, state.import_hold.take())
            };
            if let Some((entered, hold)) = hold {
                let _ = entered.send(());
                let _ = hold.await;
            }
            outcome
        })
    }

    fn process_upload<'a>(
        &'a self,
        import: &'a ImportId,
    ) -> BoxFuture<'a, Result<(), ScanControlError>> {
        Box::pin(async move {
            self.state().uploads.push(import.clone());
            Ok(())
        })
    }

    fn status(&self) -> Vec<LibraryScanStatus> {
        self.state().status.clone()
    }

    fn is_available(&self, library: &LibraryId) -> bool {
        !self.state().unavailable.contains(library)
    }
}

/// Acquisition switched off: no feature for anyone, and every call answers `NotFound`, as the
/// API does when the feature is absent.
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeAcquireControl;

impl FakeAcquireControl {
    pub fn new() -> Self {
        Self
    }
}

fn absent<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T, AcquireError>> {
    Box::pin(async { Err(AcquireError::NotFound) })
}

impl AcquireControl for FakeAcquireControl {
    fn feature_for(
        &self,
        _caller: &Access,
        _permissions: &Permissions,
        _scopes_include_request: bool,
    ) -> Option<AcquireFeature> {
        None
    }

    fn search<'a>(
        &'a self,
        _caller: &'a Access,
        _query: ExternalQuery,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }

    fn search_more<'a>(
        &'a self,
        _caller: &'a Access,
        _search: &'a str,
    ) -> BoxFuture<'a, Result<ExternalSearch, AcquireError>> {
        absent()
    }

    fn request<'a>(
        &'a self,
        _caller: &'a Access,
        _request: AcquireRequest,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }

    fn get<'a>(
        &'a self,
        _caller: &'a Access,
        _id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<AcquisitionView, AcquireError>> {
        absent()
    }

    fn list<'a>(
        &'a self,
        _caller: &'a Access,
        _filter: AcquisitionFilter,
    ) -> BoxFuture<'a, Result<Page<AcquisitionView>, AcquireError>> {
        absent()
    }

    fn cancel<'a>(
        &'a self,
        _caller: &'a Access,
        _id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }

    fn instant_stream<'a>(
        &'a self,
        _caller: &'a Access,
        _id: &'a AcquisitionId,
    ) -> BoxFuture<'a, Result<InstantStream, AcquireError>> {
        Box::pin(async { Err(AcquireError::NotFound) })
    }

    fn providers(&self) -> BoxFuture<'_, Vec<ProviderStatus>> {
        Box::pin(async { Vec::new() })
    }

    fn configure(&self, _change: ProviderChange) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Err(AcquireError::Disabled) })
    }

    fn test_provider<'a>(
        &'a self,
        _name: &'a str,
        _query: &'a str,
    ) -> BoxFuture<'a, Result<ProviderTest, AcquireError>> {
        absent()
    }

    fn update_ytdlp(&self) -> BoxFuture<'_, Result<(), AcquireError>> {
        Box::pin(async { Err(AcquireError::Disabled) })
    }

    fn retry<'a>(&'a self, _id: &'a AcquisitionId) -> BoxFuture<'a, Result<(), AcquireError>> {
        absent()
    }

    fn admin_list(
        &self,
        _filter: AcquisitionFilter,
    ) -> BoxFuture<'_, Result<Page<AdminAcquisitionView>, AcquireError>> {
        Box::pin(async { Ok(Page::default()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_starts_at_the_test_time() -> Result<(), DbError> {
        let test = TestDb::new()?;
        assert_eq!(test.schema_version()?, crate::db::SCHEMA_VERSION);
        assert_eq!(crate::time::Clock::now(&*test.clock), TEST_START);
        assert_eq!(TEST_START.to_rfc3339(), "2026-11-14T09:30:00.000Z");
        Ok(())
    }

    #[tokio::test]
    async fn fake_scan_control_records_imports() {
        let fake = FakeScanControl::new();
        let library: LibraryId = random_id();
        let song = crate::ids::SongId::derive(&crate::ids::AudioDigest([1; 32]));
        fake.push_import_outcome(ImportOutcome::Added(song.clone()));
        let outcome = fake
            .import_file(&library, Path::new("a/b.flac"), SongOrigin::Acquired)
            .await;
        assert!(matches!(outcome, Ok(ImportOutcome::Added(id)) if id == song));
        assert_eq!(fake.requested_imports().len(), 1);
        let second = fake
            .import_file(&library, Path::new("c.flac"), SongOrigin::Upload)
            .await;
        assert!(matches!(second, Err(ScanControlError::Rejected(_))));
    }

    #[test]
    fn fake_acquire_control_offers_no_feature() {
        let fake = FakeAcquireControl::new();
        assert_eq!(
            fake.feature_for(&Access::system(), &Permissions::default(), true),
            None
        );
    }
}
