//! The acquirer end to end with fake providers, a fake scanner and fake media, on real
//! SQLite. Time is paused, and the acquirer's clock follows tokio's, so backoffs and leases
//! pass instantly.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use trackside_core::config::AcquireConfig;
use trackside_core::control::{
    AcquireControl, AcquireError, AcquireFailure, AcquireRequest, AcquisitionFilter,
    AcquisitionState, AcquisitionView, ExternalPlayRequest, ExternalQuery, GrowState, GrowingFile,
    HlsJobId, ImportOutcome, ProviderChange, ProviderHealthState, ScanControl,
};
use trackside_core::db::{DbOptions, FilesystemCheck};
use trackside_core::ids::PublicId;
use trackside_core::records::{Codec, Container};
use trackside_core::repo::settings;
use trackside_core::testing::{FakeScanControl, TEST_START};
use trackside_core::{
    Access, AcquisitionId, BoxFuture, Clock, Db, EventBus, LibrarySet, Millis, Permissions, Role,
    ServerEvent, SongId, UserKey, random_id,
};
use trackside_media::{DecodeCheck, MediaError, Probe};

use crate::{
    Acquirer, AcquirerDeps, AudioInspector, Candidate, FetchedFile, FreeSpace, MediaOps,
    ProgressSink, Provider, ProviderError, ProviderHealth, ProviderName, SearchQuery, SourceHints,
    StreamingFetch, Timing,
};

/// Milliseconds since the epoch that move with tokio's (pausable) clock.
#[derive(Debug)]
struct TokioClock {
    start: Millis,
    origin: tokio::time::Instant,
}

impl TokioClock {
    fn new() -> Self {
        Self {
            start: TEST_START,
            origin: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Millis {
        self.start.saturating_add(self.origin.elapsed())
    }
}

/// What a fake fetch does.
#[derive(Clone, Debug)]
enum Plan {
    /// Writes `audio.flac` with these bytes.
    Write(&'static [u8]),
    Stall,
    BotCheck,
    /// Waits until cancelled.
    Block,
}

#[derive(Debug)]
struct FakeProvider {
    name: ProviderName,
    candidates: Mutex<Vec<Candidate>>,
    plans: Mutex<VecDeque<Plan>>,
    default_plan: Mutex<Plan>,
    searches: AtomicUsize,
    fetches: AtomicUsize,
    resumes: AtomicUsize,
    search_error: Mutex<Option<fn() -> ProviderError>>,
    /// For streaming: the test sends on this to let the download finish.
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// What a streaming fetch delivers; a fake FLAC header unless set.
    stream_bytes: Mutex<Vec<u8>>,
    streams: bool,
}

impl FakeProvider {
    fn new(name: ProviderName, candidates: Vec<Candidate>) -> Arc<Self> {
        Arc::new(Self {
            name,
            candidates: Mutex::new(candidates),
            plans: Mutex::new(VecDeque::new()),
            default_plan: Mutex::new(Plan::Write(GOOD_FLAC)),
            searches: AtomicUsize::new(0),
            fetches: AtomicUsize::new(0),
            resumes: AtomicUsize::new(0),
            search_error: Mutex::new(None),
            release: Mutex::new(None),
            stream_bytes: Mutex::new(GOOD_FLAC.to_vec()),
            streams: false,
        })
    }

    fn plan(&self, plans: &[Plan]) {
        self.plans.lock().unwrap().extend(plans.iter().cloned());
    }

    fn always(&self, plan: Plan) {
        *self.default_plan.lock().unwrap() = plan;
    }

    fn next_plan(&self) -> Plan {
        let next = self.plans.lock().unwrap().pop_front();
        next.unwrap_or_else(|| self.default_plan.lock().unwrap().clone())
    }

    async fn run(
        &self,
        staging: &Path,
        progress: &ProgressSink,
        cancel: &CancellationToken,
    ) -> Result<FetchedFile, ProviderError> {
        match self.next_plan() {
            Plan::Write(bytes) => {
                tokio::fs::create_dir_all(staging).await?;
                let path = staging.join("audio.flac");
                tokio::fs::write(&path, bytes).await?;
                progress.report(bytes.len() as u64, Some(bytes.len() as u64));
                Ok(FetchedFile {
                    path,
                    bytes: bytes.len() as u64,
                    thumbnail: None,
                })
            }
            Plan::Stall => Err(ProviderError::Stalled),
            Plan::BotCheck => Err(ProviderError::BotCheck),
            Plan::Block => {
                tokio::fs::create_dir_all(staging).await?;
                tokio::fs::write(staging.join("audio.flac.part"), b"half").await?;
                progress.report(4, Some(8));
                cancel.cancelled().await;
                Err(ProviderError::Cancelled)
            }
        }
    }
}

impl Provider for FakeProvider {
    fn name(&self) -> ProviderName {
        self.name
    }

    fn search<'a>(
        &'a self,
        _query: &'a SearchQuery,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Candidate>, ProviderError>> {
        Box::pin(async move {
            self.searches.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = *self.search_error.lock().unwrap() {
                return Err(error());
            }
            Ok(self.candidates.lock().unwrap().clone())
        })
    }

    fn fetch<'a>(
        &'a self,
        _candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<FetchedFile, ProviderError>> {
        Box::pin(async move {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.run(staging, &progress, &cancel).await
        })
    }

    fn fetch_streaming<'a>(
        &'a self,
        _candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        _cancel: CancellationToken,
    ) -> Option<BoxFuture<'a, Result<StreamingFetch, ProviderError>>> {
        if !self.streams {
            return None;
        }
        Some(Box::pin(async move {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            tokio::fs::create_dir_all(staging).await?;
            let path = staging.join("audio.stream");
            let bytes = self.stream_bytes.lock().unwrap().clone();
            tokio::fs::write(&path, &bytes[..4]).await?;
            let (tx, rx) = watch::channel(GrowState::Growing { bytes: 4 });
            let release = self.release.lock().unwrap().take();
            let done_path = path.clone();
            let done = async move {
                if let Some(release) = release {
                    let _ = release.await;
                }
                tokio::fs::write(&done_path, &bytes).await?;
                tx.send_replace(GrowState::Complete {
                    bytes: bytes.len() as u64,
                });
                progress.report(bytes.len() as u64, None);
                Ok(FetchedFile {
                    path: done_path,
                    bytes: bytes.len() as u64,
                    thumbnail: None,
                })
            };
            Ok(StreamingFetch {
                growing: GrowingFile { path, state: rx },
                done: Box::pin(done),
            })
        }))
    }

    fn streams(&self) -> bool {
        self.streams
    }

    fn resume<'a>(
        &'a self,
        candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<FetchedFile, ProviderError>> {
        let _ = candidate;
        Box::pin(async move {
            self.resumes.fetch_add(1, Ordering::SeqCst);
            self.run(staging, &progress, &cancel).await
        })
    }

    fn health(&self) -> BoxFuture<'_, ProviderHealth> {
        Box::pin(async { ProviderHealth::Ok { version: None } })
    }
}

const GOOD_FLAC: &[u8] = b"fLaC\0\0\0\x22good audio bytes";
const ZIP: &[u8] = b"PK\x03\x04not audio at all";

#[derive(Debug, Default)]
struct FakeMedia {
    duration_ms: AtomicU64,
    hls_started: AtomicUsize,
}

impl AudioInspector for FakeMedia {
    fn probe<'a>(&'a self, _path: &'a Path) -> BoxFuture<'a, Result<Probe, MediaError>> {
        Box::pin(async move {
            Ok(Probe {
                container: Container::Flac,
                codec: Codec::Flac,
                duration_ms: self.duration_ms.load(Ordering::SeqCst) as u32,
                bitrate_kbps: Some(900),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                has_video: false,
            })
        })
    }

    fn decode_check<'a>(
        &'a self,
        _path: &'a Path,
    ) -> BoxFuture<'a, Result<Option<DecodeCheck>, MediaError>> {
        Box::pin(async { Ok(None) })
    }
}

impl MediaOps for FakeMedia {
    fn start_hls(
        &self,
        _file: GrowingFile,
        _duration_ms: u32,
        _user: Option<UserKey>,
    ) -> BoxFuture<'_, Result<Option<HlsJobId>, MediaError>> {
        Box::pin(async move {
            self.hls_started.fetch_add(1, Ordering::SeqCst);
            Ok(Some(HlsJobId(Box::from("hls-1"))))
        })
    }
}

#[derive(Debug)]
/// Free bytes, and whether `statfs` hangs as it does on a hung network mount.
struct FakeSpace(AtomicU64, AtomicBool);

impl FreeSpace for FakeSpace {
    fn available(&self, _path: &Path) -> std::io::Result<u64> {
        while self.1.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(self.0.load(Ordering::SeqCst))
    }
}

fn candidate(provider: ProviderName, key: &str, lossless: bool) -> Candidate {
    Candidate {
        provider,
        provider_ref: match provider {
            ProviderName::Ytdlp => serde_json::json!({ "id": key }),
            ProviderName::Soulseek => {
                serde_json::json!({ "username": "peer", "filename": key, "size": 20 })
            }
        },
        title: String::from("Halo"),
        artist: String::from("Beyoncé"),
        album: None,
        duration_ms: Some(200_000),
        container: lossless.then_some(Container::Flac),
        codec: lossless.then_some(Codec::Flac),
        bitrate_kbps: if lossless { Some(900) } else { None },
        sample_rate: None,
        bit_depth: None,
        lossless,
        size: Some(20),
        hints: match provider {
            ProviderName::Ytdlp => SourceHints::Video {
                uploader: String::from("Beyoncé - Topic"),
                topic_channel: true,
                official: false,
                music_video: false,
            },
            ProviderName::Soulseek => SourceHints::Peer {
                username: String::from("peer"),
                folder: String::from("Beyoncé\\Album"),
                free_slot: true,
                queue_length: 0,
                upload_speed: 0,
            },
        },
    }
}

struct World {
    dir: tempfile::TempDir,
    db: Db,
    events: EventBus,
    clock: Arc<TokioClock>,
    scan: Arc<FakeScanControl>,
    media: Arc<FakeMedia>,
    space: Arc<FakeSpace>,
    library: PathBuf,
    library_key: i64,
    config: AcquireConfig,
    admin: Access,
    member: Access,
    other: Access,
    shutdown: CancellationToken,
}

impl World {
    async fn new() -> Self {
        Self::with_db(None).await
    }

    async fn with_db(path: Option<&Path>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let events = EventBus::default();
        let clock = Arc::new(TokioClock::new());
        let db = match path {
            Some(path) => {
                let mut options = DbOptions::new(path);
                options.filesystem = FilesystemCheck::AssumeLocal;
                Db::open(&options, events.clone(), clock.clone()).unwrap()
            }
            None => Db::open_in_memory(events.clone(), clock.clone()).unwrap(),
        };
        let library = dir.path().join("library");
        std::fs::create_dir_all(&library).unwrap();
        let root = library.clone();
        let library_key = db
            .write(move |tx| {
                let now = tx.now();
                for (id, name, role) in [(1, "admin", "admin"), (2, "member", "member"), (3, "other", "member")] {
                    let user: trackside_core::UserId = random_id();
                    tx.execute(
                        "INSERT OR IGNORE INTO users (id, pid, username, username_key, display_name, password_hash, role,
                             created_at, updated_at, password_changed_at)
                         VALUES (?1, ?2, ?3, ?3, ?3, 'x', ?4, ?5, ?5, ?5)",
                        rusqlite::params![id, user, name, role, now],
                    )?;
                }
                let library: trackside_core::LibraryId = random_id();
                let pid = library.as_str().to_owned();
                tx.execute(
                    "INSERT OR IGNORE INTO libraries (pid, name, root, root_display, writable, created_at, updated_at)
                     VALUES (?1, 'Acquired', ?2, ?3, 1, ?4, ?4)",
                    rusqlite::params![pid, root.to_string_lossy().as_bytes(), root.to_string_lossy(), now],
                )?;
                settings::put(tx, settings::keys::ACQUIRE_LIBRARY, &pid)?;
                Ok(tx.query_row("SELECT id FROM libraries WHERE pid = ?1", [&pid], |r| r.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        let mut config = trackside_core::Config::defaults(dir.path()).acquire;
        config.enabled = true;
        config.ytdlp.enabled = true;
        config.soulseek.enabled = true;
        config.staging_dir = dir.path().join("staging");
        let access = |user, role| Access {
            user: UserKey(user),
            role,
            libraries: LibrarySet::All,
            sync_epoch: 1,
        };
        let media = Arc::new(FakeMedia::default());
        media.duration_ms.store(200_000, Ordering::SeqCst);
        Self {
            dir,
            db,
            events,
            clock,
            scan: Arc::new(FakeScanControl::new()),
            media,
            space: Arc::new(FakeSpace(AtomicU64::new(u64::MAX), AtomicBool::new(false))),
            library,
            library_key,
            config,
            admin: access(1, Role::Admin),
            member: access(2, Role::Member),
            other: access(3, Role::Member),
            shutdown: CancellationToken::new(),
        }
    }

    fn timing() -> Timing {
        Timing {
            min_free_bytes: 1024,
            ..Timing::default()
        }
    }

    fn start(&self, providers: Vec<Arc<FakeProvider>>) -> Arc<Acquirer> {
        self.start_timed(providers, Self::timing())
    }

    fn start_timed(&self, providers: Vec<Arc<FakeProvider>>, timing: Timing) -> Arc<Acquirer> {
        Acquirer::start_with(AcquirerDeps {
            db: self.db.clone(),
            scan: self.scan.clone() as Arc<dyn ScanControl>,
            media: self.media.clone() as Arc<dyn MediaOps>,
            config: self.config.clone(),
            providers: providers
                .into_iter()
                .map(|p| p as Arc<dyn Provider>)
                .collect(),
            events: self.events.clone(),
            clock: self.clock.clone(),
            shutdown: self.shutdown.child_token(),
            free_space: self.space.clone(),
            timing,
            data_dir: Some(self.dir.path().to_path_buf()),
        })
    }

    fn song_added(&self) -> SongId {
        let song: SongId = random_id();
        self.scan
            .push_import_outcome(ImportOutcome::Added(song.clone()));
        song
    }

    async fn insert_song(&self, title: &str, artist: &str, duration_ms: u32) -> SongId {
        let song: SongId = random_id();
        let pid = song.clone();
        let key = tracks_model::song_key(title, artist);
        let (title, artist) = (title.to_owned(), artist.to_owned());
        let library = self.library_key;
        self.db
            .write(move |tx| {
                let now = tx.now();
                tx.execute(
                    "INSERT OR IGNORE INTO folders (pid, library_id, rel_path, name) VALUES (?2, ?1, x'', '')",
                    rusqlite::params![library, random_id::<trackside_core::FolderId>()],
                )?;
                tx.execute(
                    "INSERT INTO songs (pid, library_id, folder_id, rel_path, rel_path_display, file_size, file_mtime_ns,
                         audio_digest, title, sort_title, artist, duration_ms, codec, container, content_type, lossless,
                         song_key, search_text, added_at, updated_at)
                     VALUES (?1, ?2, (SELECT id FROM folders WHERE library_id = ?2 AND rel_path = x''), ?3, ?4, 1, 1, zeroblob(32), ?5, ?5, ?6, ?7,
                         'flac', 'flac', 'audio/flac', 1, ?8, ?5, ?9, ?9)",
                    rusqlite::params![pid, library, title.as_bytes(), title, title, artist, duration_ms, key, now],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        song
    }

    async fn audit_actions(&self) -> Vec<String> {
        self.db
            .read(|c| {
                let mut s = c.prepare("SELECT action FROM audit_log ORDER BY id")?;
                let rows = s.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<Result<Vec<String>, _>>()?)
            })
            .await
            .unwrap()
    }
}

fn halo() -> AcquireRequest {
    AcquireRequest {
        title: String::from("Halo"),
        artist: String::from("Beyoncé"),
        album: None,
        duration_ms: Some(200_000),
        candidate: None,
        allow_versions: false,
        instant: false,
    }
}

async fn wait_for(
    events: &mut broadcast::Receiver<Arc<ServerEvent>>,
    id: &AcquisitionId,
    done: impl Fn(&AcquisitionView) -> bool,
) -> AcquisitionView {
    let wait = async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let ServerEvent::Acquisition { view, .. } = &*event
                        && &view.id == id
                        && done(view)
                    {
                        return view.clone();
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("event bus closed"),
            }
        }
    };
    tokio::time::timeout(Duration::from_hours(7 * 24), wait)
        .await
        .expect("the acquisition never reached the expected state")
}

fn is(state: AcquisitionState) -> impl Fn(&AcquisitionView) -> bool {
    move |v| v.state == state
}

#[tokio::test(start_paused = true)]
async fn request_for_owned_song_returns_existing_id_immediately() {
    let world = World::new().await;
    let song = world.insert_song("Halo", "Beyoncé", 201_000).await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let acquirer = world.start(vec![provider.clone()]);
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    assert_eq!(view.state, AcquisitionState::Done);
    assert!(view.existing);
    assert_eq!(view.song, Some(song));
    assert_eq!(provider.searches.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn an_owned_song_with_a_different_duration_is_acquired() {
    let world = World::new().await;
    world.insert_song("Halo", "Beyoncé", 300_000).await;
    let acquirer = world.start(vec![FakeProvider::new(ProviderName::Soulseek, Vec::new())]);
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    assert_eq!(view.state, AcquisitionState::Queued);
}

#[tokio::test(start_paused = true)]
async fn duplicate_request_joins_the_active_one_and_does_not_count_against_quota() {
    let mut world = World::new().await;
    world.config.member_daily = 1;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.plan(&[Plan::Block]);
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let first = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &first.id, is(AcquisitionState::Downloading)).await;

    let joined = acquirer.request(&world.other, halo()).await.unwrap();
    assert_ne!(joined.id, first.id);
    assert_eq!(joined.state, AcquisitionState::Downloading);
    let mut different = halo();
    different.title = String::from("Single Ladies");
    assert!(
        acquirer.request(&world.other, different).await.is_ok(),
        "joining must not use the quota"
    );

    // The joined request follows the first: cancel the blocked fetch so the next run
    // succeeds, and both finish with the same song.
    let song = world.song_added();
    acquirer.cancel(&world.member, &first.id).await.unwrap();
    let done = wait_for(&mut events, &joined.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert!(provider.searches.load(Ordering::SeqCst) >= 2);
}

#[tokio::test(start_paused = true)]
async fn joined_requests_finish_with_the_one_they_joined() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.plan(&[Plan::Block]);
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let first = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &first.id, is(AcquisitionState::Downloading)).await;
    let joined = acquirer.request(&world.other, halo()).await.unwrap();
    // Cancelling a joined request leaves the first running.
    acquirer.cancel(&world.other, &joined.id).await.unwrap();
    let first_now = acquirer.get(&world.member, &first.id).await.unwrap();
    assert_eq!(first_now.state, AcquisitionState::Downloading);
    let other_now = acquirer.get(&world.other, &joined.id).await.unwrap();
    assert_eq!(other_now.state, AcquisitionState::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn restart_resumes_a_downloading_job_from_its_lease() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("trackside.db");
    let world = World::with_db(Some(&db_path)).await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.plan(&[Plan::Block]);
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &view.id, is(AcquisitionState::Downloading)).await;
    acquirer.shutdown().await;
    drop(acquirer);
    let row = acquirer_row(&world.db, &view.id).await;
    assert_eq!(row.state, AcquisitionState::Downloading);
    assert!(row.lease_until.is_some(), "the lease stays with the row");

    let song = world.song_added();
    let again = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let acquirer = world.start(vec![again.clone()]);
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert_eq!(again.resumes.load(Ordering::SeqCst), 1);
    assert_eq!(again.fetches.load(Ordering::SeqCst), 0);
    assert_eq!(again.searches.load(Ordering::SeqCst), 0);
    drop(acquirer);
}

async fn acquirer_row(
    db: &Db,
    id: &AcquisitionId,
) -> trackside_core::repo::acquisitions::AcquisitionRow {
    let id = id.clone();
    db.read(move |c| trackside_core::repo::acquisitions::get(c, &id))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn failed_candidate_falls_back_to_the_next() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![
            candidate(ProviderName::Soulseek, "best", true),
            candidate(ProviderName::Soulseek, "second", false),
        ],
    );
    provider.plan(&[Plan::Stall]);
    let song = world.song_added();
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 2);
    let admin = acquirer
        .admin_list(AcquisitionFilter {
            limit: 10,
            ..AcquisitionFilter::default()
        })
        .await
        .unwrap();
    let job = &admin.items[0];
    assert_eq!(job.candidates.len(), 2);
    assert_eq!(job.candidates[0].failure.as_deref(), Some("stalled"));
    assert_eq!(job.provider.as_deref(), Some("soulseek"));
}

#[tokio::test(start_paused = true)]
async fn a_rejected_file_falls_back_to_the_next_candidate() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![
            candidate(ProviderName::Soulseek, "zip", true),
            candidate(ProviderName::Soulseek, "good", false),
        ],
    );
    provider.plan(&[Plan::Write(ZIP)]);
    world.song_added();
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    let admin = acquirer
        .admin_list(AcquisitionFilter {
            limit: 10,
            ..AcquisitionFilter::default()
        })
        .await
        .unwrap();
    assert!(
        admin.items[0].candidates[0]
            .failure
            .as_deref()
            .unwrap()
            .contains("not an allowed audio file")
    );
}

#[tokio::test(start_paused = true)]
async fn all_candidates_failed_retries_with_backoff() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.always(Plan::Stall);
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let started = tokio::time::Instant::now();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let failed = wait_for(&mut events, &view.id, is(AcquisitionState::Failed)).await;
    assert_eq!(failed.failure, Some(AcquireFailure::Exhausted));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_mins(1 + 5 + 30 + 120),
        "{waited:?}"
    );
    assert!(
        waited < Duration::from_mins(1 + 5 + 30 + 120 + 5),
        "{waited:?}"
    );
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 5);
    assert_eq!(acquirer_row(&world.db, &view.id).await.attempts, 5);
}

#[tokio::test(start_paused = true)]
async fn nothing_found_fails_no_match_at_once() {
    let world = World::new().await;
    let provider = FakeProvider::new(ProviderName::Soulseek, vec![]);
    let acquirer = world.start(vec![provider]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let failed = wait_for(&mut events, &view.id, is(AcquisitionState::Failed)).await;
    assert_eq!(failed.failure, Some(AcquireFailure::NoMatch));
}

#[tokio::test(start_paused = true)]
async fn an_unreachable_only_provider_ends_provider_unavailable_after_retries() {
    let world = World::new().await;
    let provider = FakeProvider::new(ProviderName::Soulseek, vec![]);
    *provider.search_error.lock().unwrap() =
        Some(|| ProviderError::Unavailable(String::from("cannot reach slskd")));
    let acquirer = world.start_timed(
        vec![provider],
        Timing {
            backoff: vec![Duration::from_secs(60)],
            ..World::timing()
        },
    );
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let failed = wait_for(&mut events, &view.id, is(AcquisitionState::Failed)).await;
    assert_eq!(failed.failure, Some(AcquireFailure::ProviderUnavailable));
}

#[tokio::test(start_paused = true)]
async fn quota_exceeded_is_429_with_reset_time() {
    let mut world = World::new().await;
    world.config.member_daily = 2;
    let provider = FakeProvider::new(ProviderName::Soulseek, vec![]);
    let acquirer = world.start(vec![provider]);
    let titles = ["One", "Two", "Three"];
    let mut first_at = None;
    for title in &titles[..2] {
        let mut request = halo();
        request.title = (*title).to_owned();
        let view = acquirer.request(&world.member, request).await.unwrap();
        first_at.get_or_insert(view.created_at);
        tokio::time::advance(Duration::from_secs(60)).await;
    }
    let mut third = halo();
    third.title = String::from("Three");
    match acquirer.request(&world.member, third.clone()).await {
        Err(AcquireError::QuotaExceeded { resets_at }) => {
            assert_eq!(
                resets_at,
                first_at.unwrap().saturating_add(Duration::from_hours(24))
            );
        }
        other => panic!("expected a quota error, got {other:?}"),
    }
    assert!(
        acquirer.request(&world.admin, third).await.is_ok(),
        "admins are exempt"
    );
}

#[tokio::test(start_paused = true)]
async fn the_active_quota_limits_requests_in_flight() {
    let mut world = World::new().await;
    world.config.member_active = 1;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.always(Plan::Block);
    let acquirer = world.start(vec![provider]);
    acquirer.request(&world.member, halo()).await.unwrap();
    let mut second = halo();
    second.title = String::from("Other");
    assert!(matches!(
        acquirer.request(&world.member, second).await,
        Err(AcquireError::QuotaExceeded { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn bot_check_puts_provider_in_cooldown() {
    let world = World::new().await;
    let video = FakeProvider::new(
        ProviderName::Ytdlp,
        vec![candidate(ProviderName::Ytdlp, "bnVUHWCynig", false)],
    );
    video.always(Plan::BotCheck);
    let peer = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "peer-file", false)],
    );
    world.song_added();
    let mut config_world = world;
    config_world.config.prefer = vec![String::from("ytdlp"), String::from("soulseek")];
    let world = config_world;
    let acquirer = world.start(vec![video.clone(), peer.clone()]);
    let mut events = world.events.subscribe();
    let started = world.clock.now();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(video.fetches.load(Ordering::SeqCst), 1);
    assert_eq!(
        peer.fetches.load(Ordering::SeqCst),
        1,
        "the next candidate came from the other provider"
    );
    let status = acquirer.providers().await;
    let ytdlp = status.iter().find(|s| s.name == "ytdlp").unwrap();
    assert_eq!(ytdlp.health, ProviderHealthState::Cooldown);
    let until = ytdlp.cooldown_until.unwrap();
    assert!(until.0 - started.0 >= 15 * 60_000 && until.0 - started.0 < 16 * 60_000);
    assert!(ytdlp.detail.as_deref().unwrap().contains("cookies"));

    // While cooling down, searches skip the provider.
    let before = video.searches.load(Ordering::SeqCst);
    acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    assert_eq!(video.searches.load(Ordering::SeqCst), before);
}

#[tokio::test(start_paused = true)]
async fn disk_full_pauses_and_resumes() {
    let world = World::new().await;
    world.space.0.store(0, Ordering::SeqCst);
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let song = world.song_added();
    let acquirer = world.start(vec![provider.clone()]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    tokio::time::sleep(Duration::from_mins(10)).await;
    assert!(acquirer.paused_for_space());
    assert_eq!(provider.searches.load(Ordering::SeqCst), 0);
    assert_eq!(
        acquirer.get(&world.member, &view.id).await.unwrap().state,
        AcquisitionState::Queued
    );

    world.space.0.store(u64::MAX, Ordering::SeqCst);
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert!(!acquirer.paused_for_space());
}

// Real time: paused time does not advance while a blocking task runs, and this one hangs.
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_free_space_check_pauses_the_queue_instead_of_the_runtime() {
    let world = World::new().await;
    world.space.1.store(true, Ordering::SeqCst);
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let song = world.song_added();
    let acquirer = world.start_timed(
        vec![provider.clone()],
        Timing {
            idle_poll: Duration::from_millis(50),
            space_check_deadline: Duration::from_millis(100),
            ..World::timing()
        },
    );
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !acquirer.paused_for_space() {
        assert!(tokio::time::Instant::now() < deadline, "never paused");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(acquirer.paused_for_space());
    assert_eq!(provider.searches.load(Ordering::SeqCst), 0);

    world.space.1.store(false, Ordering::SeqCst);
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert!(!acquirer.paused_for_space());
}

fn staging_of(world: &World, id: &AcquisitionId) -> PathBuf {
    world.config.staging_dir.join(id.as_str())
}

#[tokio::test(start_paused = true)]
async fn staging_is_removed_on_done_failed_and_cancelled() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.plan(&[Plan::Write(GOOD_FLAC), Plan::Stall, Plan::Block]);
    world.song_added();
    let acquirer = world.start_timed(
        vec![provider.clone()],
        Timing {
            backoff: Vec::new(),
            ..World::timing()
        },
    );
    let mut events = world.events.subscribe();

    let done = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &done.id, is(AcquisitionState::Done)).await;
    assert!(!staging_of(&world, &done.id).exists());
    let imported = world.scan.requested_imports();
    assert_eq!(
        imported[0].rel_path,
        PathBuf::from("Beyoncé/Singles/Halo.flac")
    );
    assert!(world.library.join("Beyoncé/Singles/Halo.flac").is_file());

    let mut failing = halo();
    failing.title = String::from("Halo");
    failing.artist = String::from("Beyoncé");
    failing.duration_ms = None;
    let failed = acquirer.request(&world.admin, failing).await.unwrap();
    let failed_view = wait_for(&mut events, &failed.id, is(AcquisitionState::Failed)).await;
    assert_eq!(failed_view.failure, Some(AcquireFailure::Exhausted));
    assert!(!staging_of(&world, &failed.id).exists());

    let mut blocked = halo();
    blocked.album = Some(String::from("Blocked"));
    blocked.title = String::from("Halo");
    let cancelled = acquirer.request(&world.other, blocked).await.unwrap();
    wait_for(
        &mut events,
        &cancelled.id,
        is(AcquisitionState::Downloading),
    )
    .await;
    assert!(staging_of(&world, &cancelled.id).exists());
    acquirer.cancel(&world.other, &cancelled.id).await.unwrap();
    acquirer.shutdown().await;
    assert!(!staging_of(&world, &cancelled.id).exists());
    assert_eq!(
        acquirer
            .get(&world.other, &cancelled.id)
            .await
            .unwrap()
            .state,
        AcquisitionState::Cancelled
    );
    assert!(matches!(
        acquirer.cancel(&world.other, &cancelled.id).await,
        Err(AcquireError::InvalidState)
    ));
}

#[tokio::test(start_paused = true)]
async fn janitor_removes_orphans() {
    let world = World::new().await;
    let orphan = world.config.staging_dir.join("qorphan0000000000000");
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("audio.flac.part"), b"x").unwrap();
    std::fs::write(world.config.staging_dir.join("stray.part"), b"x").unwrap();
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.always(Plan::Block);
    let acquirer = world.start(vec![provider]);
    let mut events = world.events.subscribe();
    let active = acquirer.request(&world.member, halo()).await.unwrap();
    wait_for(&mut events, &active.id, is(AcquisitionState::Downloading)).await;
    acquirer.run_janitor().await.unwrap();
    assert!(!orphan.exists());
    assert!(!world.config.staging_dir.join("stray.part").exists());
    assert!(
        staging_of(&world, &active.id)
            .join("audio.flac.part")
            .exists(),
        "a fresh .part of an active job stays"
    );
}

#[tokio::test(start_paused = true)]
async fn view_never_contains_provider_names() {
    let world = World::new().await;
    let video = FakeProvider::new(
        ProviderName::Ytdlp,
        vec![candidate(ProviderName::Ytdlp, "bnVUHWCynig", false)],
    );
    video.plan(&[Plan::Stall]);
    let peer = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "file.flac", true)],
    );
    peer.plan(&[Plan::Stall]);
    world.song_added();
    let acquirer = world.start(vec![video, peer]);
    let mut events = world.events.subscribe();
    let mut seen: Vec<String> = Vec::new();

    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    assert_eq!(search.candidates.len(), 2);
    seen.push(format!("{search:?}"));

    let done = acquirer.request(&world.member, halo()).await.unwrap();
    let mut states = Vec::new();
    loop {
        let view = wait_for(&mut events, &done.id, |_| true).await;
        seen.push(format!("{view:?}"));
        states.push(view.state);
        if view.state.is_finished() {
            break;
        }
    }
    for state in [
        AcquisitionState::Queued,
        AcquisitionState::Searching,
        AcquisitionState::Downloading,
        AcquisitionState::Verifying,
        AcquisitionState::Importing,
        AcquisitionState::Done,
    ] {
        assert!(states.contains(&state), "{state:?} missing from {states:?}");
    }
    world.space.0.store(0, Ordering::SeqCst);
    let cancelled = acquirer
        .request(
            &world.other,
            AcquireRequest {
                title: String::from("Never"),
                ..halo()
            },
        )
        .await
        .unwrap();
    acquirer.cancel(&world.other, &cancelled.id).await.unwrap();
    seen.push(format!(
        "{:?}",
        acquirer.get(&world.other, &cancelled.id).await.unwrap()
    ));
    let list = acquirer
        .list(
            &world.member,
            AcquisitionFilter {
                limit: 50,
                ..AcquisitionFilter::default()
            },
        )
        .await
        .unwrap();
    seen.push(format!("{list:?}"));

    for text in &seen {
        let lower = text.to_lowercase();
        for name in ["ytdlp", "yt-dlp", "soulseek", "slskd", "youtube"] {
            assert!(!lower.contains(name), "{name} leaked in {text}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn instant_play_serves_hls_before_download_completes() {
    let mut world = World::new().await;
    world.config.instant_play = true;
    let mut video = FakeProvider::new(
        ProviderName::Ytdlp,
        vec![candidate(ProviderName::Ytdlp, "bnVUHWCynig", false)],
    );
    Arc::get_mut(&mut video).unwrap().streams = true;
    let (release, gate) = tokio::sync::oneshot::channel();
    *video.release.lock().unwrap() = Some(gate);
    let song = world.song_added();
    let acquirer = world.start(vec![video.clone()]);
    let mut events = world.events.subscribe();

    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let offered = &search.candidates[0];
    assert!(offered.instant);
    let mut request = halo();
    request.candidate = Some(offered.token.clone());
    request.instant = true;
    let view = acquirer.request(&world.member, request).await.unwrap();

    let streaming = wait_for(&mut events, &view.id, |v| v.stream.is_some()).await;
    assert_eq!(streaming.state, AcquisitionState::Downloading);
    let urls = streaming.stream.unwrap();
    assert_eq!(
        urls.hls,
        format!("/api/v1/acquisitions/{}/stream.m3u8", view.id)
    );
    let stream = acquirer.instant_stream(&world.member, &view.id).unwrap();
    assert!(matches!(
        acquirer.instant_stream(&world.other, &view.id),
        Err(AcquireError::NotFound)
    ));
    assert!(acquirer.instant_stream(&world.admin, &view.id).is_ok());
    assert_eq!(stream.hls_job, Some(HlsJobId(Box::from("hls-1"))));
    assert_eq!(
        *stream.progressive.state.borrow(),
        GrowState::Growing { bytes: 4 }
    );
    assert_eq!(world.media.hls_started.load(Ordering::SeqCst), 1);

    release.send(()).unwrap();
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert!(done.stream.is_none());
}

#[tokio::test(start_paused = true)]
async fn instant_needs_a_streaming_candidate_and_the_setting() {
    let mut world = World::new().await;
    let peer = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "f", true)],
    );
    world.config.instant_play = false;
    let acquirer = world.start(vec![peer]);
    let mut request = halo();
    request.instant = true;
    assert!(matches!(
        acquirer.request(&world.member, request).await,
        Err(AcquireError::Invalid(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn done_event_carries_the_song_id_and_the_change_is_in_sync() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let song = world.song_added();
    let acquirer = world.start(vec![provider]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    assert!(!done.existing);
    let pid = view.id.as_str().to_owned();
    let logged: i64 = world
        .db
        .read(move |c| Ok(c.query_row("SELECT COUNT(*) FROM change_log WHERE kind = 'acquisition' AND entity_pid = ?1 AND user_id = 2", [pid], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(logged, 1);
}

#[tokio::test(start_paused = true)]
async fn a_digest_duplicate_ends_done_existing_and_removes_the_copy() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let existing = world
        .insert_song("Some Other Name", "Somebody", 200_000)
        .await;
    world
        .scan
        .push_import_outcome(ImportOutcome::Existing(existing.clone()));
    let acquirer = world.start(vec![provider]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert!(done.existing);
    assert_eq!(done.song, Some(existing));
    assert!(!world.library.join("Beyoncé/Singles/Halo.flac").exists());
}

#[tokio::test(start_paused = true)]
async fn a_picked_candidate_is_tried_first_and_an_unknown_token_has_expired() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![
            candidate(ProviderName::Soulseek, "best", true),
            candidate(ProviderName::Soulseek, "picked", false),
        ],
    );
    world.song_added();
    let acquirer = world.start(vec![provider]);
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Fields {
                title: String::from("Halo"),
                artist: String::from("Beyoncé"),
                album: None,
                duration_ms: None,
            },
        )
        .await
        .unwrap();
    assert!(search.complete);
    assert_eq!(search.candidates.len(), 2);
    let picked = search
        .candidates
        .iter()
        .find(|c| c.format.container.is_none())
        .unwrap()
        .token
        .clone();
    let mut events = world.events.subscribe();
    let mut request = halo();
    request.candidate = Some(picked.clone());
    let view = acquirer.request(&world.member, request).await.unwrap();
    wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    let admin = acquirer
        .admin_list(AcquisitionFilter {
            limit: 10,
            ..AcquisitionFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(
        admin.items[0].chosen.as_ref().map(|c| c.token.clone()),
        Some(picked)
    );

    let mut stale = halo();
    stale.title = String::from("Other");
    stale.candidate = Some(trackside_core::CandidateToken::derive(
        "soulseek",
        "{\"gone\":1}",
    ));
    assert!(matches!(
        acquirer.request(&world.member, stale).await,
        Err(AcquireError::CandidateExpired)
    ));
}

#[tokio::test(start_paused = true)]
async fn requests_and_cancellations_are_audited() {
    let world = World::new().await;
    world.space.0.store(0, Ordering::SeqCst);
    let provider = FakeProvider::new(ProviderName::Soulseek, vec![]);
    let acquirer = world.start(vec![provider]);
    let view = acquirer
        .request(
            &world.member,
            AcquireRequest {
                title: String::from("Queued"),
                ..halo()
            },
        )
        .await
        .unwrap();
    acquirer.cancel(&world.member, &view.id).await.unwrap();
    acquirer.retry(&view.id).await.unwrap();
    acquirer
        .configure(ProviderChange::Global {
            enabled: None,
            instant_play: Some(true),
            member_daily: None,
            member_active: None,
            global_daily: None,
            prefer: None,
        })
        .await
        .unwrap();
    assert_eq!(
        world.audit_actions().await,
        [
            "acquire.request",
            "acquire.cancel",
            "acquire.retry",
            "acquire.provider_update"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn others_cannot_see_or_cancel_a_request() {
    let world = World::new().await;
    let acquirer = world.start(vec![FakeProvider::new(ProviderName::Soulseek, vec![])]);
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    assert!(matches!(
        acquirer.get(&world.other, &view.id).await,
        Err(AcquireError::NotFound)
    ));
    assert!(matches!(
        acquirer.cancel(&world.other, &view.id).await,
        Err(AcquireError::NotFound)
    ));
    assert!(acquirer.get(&world.admin, &view.id).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn the_feature_is_offered_only_when_enabled_and_allowed() {
    let mut world = World::new().await;
    let permissions = Permissions {
        can_download: true,
        can_upload: false,
        can_request: true,
        legacy_subsonic: false,
        max_bitrate_kbps: None,
    };
    let acquirer = world.start(vec![FakeProvider::new(ProviderName::Soulseek, vec![])]);
    acquirer
        .search(&world.member, ExternalQuery::Text(String::from("x")))
        .await
        .unwrap();
    assert!(
        acquirer
            .feature_for(&world.member, &permissions, true)
            .is_some()
    );
    assert!(
        acquirer
            .feature_for(&world.member, &permissions, false)
            .is_none()
    );
    let denied = Permissions {
        can_request: false,
        ..permissions.clone()
    };
    assert!(acquirer.feature_for(&world.member, &denied, true).is_none());
    assert!(acquirer.feature_for(&world.admin, &denied, true).is_some());
    let blind = Access {
        libraries: LibrarySet::Only(Arc::from(Vec::new())),
        ..world.member.clone()
    };
    assert!(acquirer.feature_for(&blind, &permissions, true).is_none());

    world.config.enabled = false;
    let off = world.start(vec![FakeProvider::new(ProviderName::Soulseek, vec![])]);
    assert!(matches!(
        off.request(&world.member, halo()).await,
        Err(AcquireError::Disabled)
    ));
    assert!(off.feature_for(&world.member, &permissions, true).is_none());
}

#[tokio::test(start_paused = true)]
async fn turning_acquisition_off_fails_queued_requests_as_disabled() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    world.space.0.store(0, Ordering::SeqCst);
    let acquirer = world.start(vec![provider]);
    let mut events = world.events.subscribe();
    let view = acquirer.request(&world.member, halo()).await.unwrap();
    acquirer
        .configure(ProviderChange::Global {
            enabled: Some(false),
            instant_play: None,
            member_daily: None,
            member_active: None,
            global_daily: None,
            prefer: None,
        })
        .await
        .unwrap();
    world.space.0.store(u64::MAX, Ordering::SeqCst);
    let failed = wait_for(&mut events, &view.id, is(AcquisitionState::Failed)).await;
    assert_eq!(failed.failure, Some(AcquireFailure::Disabled));
}

#[tokio::test(start_paused = true)]
async fn per_provider_concurrency_limits_parallel_fetches() {
    let mut world = World::new().await;
    world.config.soulseek.concurrency = 1;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    provider.always(Plan::Block);
    let acquirer = world.start(vec![provider.clone()]);
    for title in ["Ha", "Lo", "Al"] {
        acquirer
            .request(
                &world.admin,
                AcquireRequest {
                    title: title.to_owned(),
                    ..halo()
                },
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(provider.fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn user_searches_are_rate_limited() {
    let world = World::new().await;
    let acquirer = world.start_timed(
        vec![FakeProvider::new(ProviderName::Soulseek, vec![])],
        Timing {
            user_search_rate: trackside_core::config::Rate {
                count: 2,
                per: Duration::from_secs(60),
            },
            ..World::timing()
        },
    );
    for _ in 0..2 {
        acquirer
            .search(&world.member, ExternalQuery::Text(String::from("x")))
            .await
            .unwrap();
    }
    assert!(matches!(
        acquirer
            .search(&world.member, ExternalQuery::Text(String::from("x")))
            .await,
        Err(AcquireError::RateLimited { .. })
    ));
    assert!(
        acquirer
            .search(&world.other, ExternalQuery::Text(String::from("x")))
            .await
            .is_ok(),
        "limits are per user"
    );
}

#[tokio::test(start_paused = true)]
async fn search_more_returns_the_session_and_only_to_its_owner() {
    let world = World::new().await;
    let provider = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "a", true)],
    );
    let acquirer = world.start(vec![provider]);
    let first = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let more = acquirer
        .search_more(&world.member, &first.search_id)
        .await
        .unwrap();
    assert_eq!(more.candidates, first.candidates);
    assert!(matches!(
        acquirer.search_more(&world.other, &first.search_id).await,
        Err(AcquireError::NotFound)
    ));
}

#[tokio::test]
async fn instant_play_serves_hls_before_download_completes_with_real_media() {
    let mut world = World::new().await;
    world.config.instant_play = true;
    let Some(ffmpeg) = trackside_media::FfmpegTranscoder::detect(None) else {
        eprintln!("skipped: ffmpeg not found");
        return;
    };
    let audio = world.dir.path().join("source.m4a");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=4",
        ])
        .args(["-ac", "2", "-c:a", "aac", "-b:a", "128k"])
        .arg(&audio)
        .status()
        .unwrap();
    assert!(made.success());
    let media_config = trackside_core::Config::defaults(world.dir.path()).media;
    let media = Arc::new(
        trackside_media::Media::new(
            &media_config,
            &world.dir.path().join("cache"),
            world.db.clone(),
            Arc::new(ffmpeg),
            world.clock.clone(),
        )
        .unwrap(),
    );
    let mut offered = candidate(ProviderName::Ytdlp, "bnVUHWCynig", false);
    offered.duration_ms = Some(4_000);
    let mut video = FakeProvider::new(ProviderName::Ytdlp, vec![offered]);
    Arc::get_mut(&mut video).unwrap().streams = true;
    *video.stream_bytes.lock().unwrap() = std::fs::read(&audio).unwrap();
    let (release, gate) = tokio::sync::oneshot::channel();
    *video.release.lock().unwrap() = Some(gate);
    let song = world.song_added();
    let acquirer = Acquirer::start_with(AcquirerDeps {
        db: world.db.clone(),
        scan: world.scan.clone() as Arc<dyn ScanControl>,
        media: media.clone() as Arc<dyn MediaOps>,
        config: world.config.clone(),
        providers: vec![video as Arc<dyn Provider>],
        events: world.events.clone(),
        clock: world.clock.clone(),
        shutdown: world.shutdown.child_token(),
        free_space: world.space.clone(),
        timing: World::timing(),
        data_dir: Some(world.dir.path().to_path_buf()),
    });
    let mut events = world.events.subscribe();
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let mut request = halo();
    request.duration_ms = Some(4_000);
    request.candidate = Some(search.candidates[0].token.clone());
    request.instant = true;
    let view = acquirer.request(&world.member, request).await.unwrap();
    let streaming = wait_for(&mut events, &view.id, |v| v.stream.is_some()).await;
    assert_eq!(streaming.state, AcquisitionState::Downloading);
    let stream = acquirer.instant_stream(&world.member, &view.id).unwrap();
    assert!(matches!(
        *stream.progressive.state.borrow(),
        GrowState::Growing { .. }
    ));
    let job = stream.hls_job.expect("an HLS job while downloading");
    let playlist = media.hls_playlist(&job, "").await.unwrap();
    assert!(playlist.contains("#EXTM3U"), "{playlist}");
    assert!(playlist.contains("#EXTINF"), "{playlist}");

    // Finishing the download runs real verification (probe and full decode) and imports.
    release.send(()).unwrap();
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
    let imported = world.scan.requested_imports();
    assert_eq!(
        imported[0].rel_path,
        PathBuf::from("Beyoncé/Singles/Halo.m4a")
    );
}

fn streaming_provider(ids: &[&str]) -> Arc<FakeProvider> {
    let mut video = FakeProvider::new(
        ProviderName::Ytdlp,
        ids.iter()
            .map(|id| candidate(ProviderName::Ytdlp, id, false))
            .collect(),
    );
    Arc::get_mut(&mut video).unwrap().streams = true;
    video
}

async fn count(db: &Db, table: &'static str) -> i64 {
    db.read(move |c| Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?))
        .await
        .unwrap()
}

async fn wait_complete(acquirer: &Acquirer, token: &trackside_core::CandidateToken) {
    let stream = acquirer.external_stream(&Access::system(), token).unwrap();
    let mut state = stream.progressive.state.clone();
    state
        .wait_for(|s| matches!(s, GrowState::Complete { .. }))
        .await
        .unwrap();
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn playing_without_keeping_keeps_nothing_and_replays_from_the_cache() {
    let world = World::new().await;
    let video = streaming_provider(&["bnVUHWCynig"]);
    let (release, gate) = tokio::sync::oneshot::channel();
    *video.release.lock().unwrap() = Some(gate);
    let acquirer = world.start(vec![video.clone()]);
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let token = search.candidates[0].token.clone();
    let changes = count(&world.db, "change_log").await;

    let play = acquirer
        .play_external(&world.member, ExternalPlayRequest::Candidate(token.clone()))
        .await
        .unwrap();
    assert_eq!(play.token.as_ref(), Some(&token));
    assert_eq!(play.song, None);
    let stream = acquirer.external_stream(&world.member, &token).unwrap();
    assert_eq!(
        *stream.progressive.state.borrow(),
        GrowState::Growing { bytes: 4 }
    );
    assert!(
        stream
            .progressive
            .path
            .starts_with(world.dir.path().join("cache").join("external"))
    );
    release.send(()).unwrap();
    wait_complete(&acquirer, &token).await;
    assert!(stream.progressive.path.exists());

    let again = acquirer
        .play_external(&world.member, ExternalPlayRequest::Candidate(token.clone()))
        .await
        .unwrap();
    assert_eq!(again.token.as_ref(), Some(&token));
    let by_name = acquirer
        .play_external(
            &world.other,
            ExternalPlayRequest::Song {
                title: String::from("Halo"),
                artist: String::from("Beyoncé"),
                album: None,
                duration_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(by_name.token.as_ref(), Some(&token));
    assert_eq!(
        video.fetches.load(Ordering::SeqCst),
        1,
        "replays come from the cache"
    );

    assert_eq!(count(&world.db, "acquisitions").await, 0);
    assert_eq!(count(&world.db, "songs").await, 0);
    assert_eq!(count(&world.db, "change_log").await, changes);
    assert_eq!(world.scan.requested_imports().len(), 0);
    assert_eq!(world.audit_actions().await, Vec::<String>::new());

    // A Get afterwards still downloads and keeps it.
    let song = world.song_added();
    let mut events = world.events.subscribe();
    let mut get = halo();
    get.candidate = Some(token.clone());
    let view = acquirer.request(&world.member, get).await.unwrap();
    let done = wait_for(&mut events, &view.id, is(AcquisitionState::Done)).await;
    assert_eq!(done.song, Some(song));
}

/// Waits until no song is being fetched and the fetch's own sweep has run.
async fn settle(acquirer: &Acquirer) {
    while acquirer.fetched_usage().is_some_and(|u| u.fetching > 0) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    acquirer.run_janitor().await.unwrap();
}

fn external_dir(world: &World) -> PathBuf {
    world.dir.path().join("cache").join("external")
}

/// The song folders in the play cache, without the marker and the trash.
fn cached_dirs(world: &World) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(external_dir(world))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Plays the first `n` candidates of a search, one after the other, each to the end.
async fn play_all(
    world: &World,
    acquirer: &Acquirer,
    n: usize,
) -> Vec<trackside_core::CandidateToken> {
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let mut tokens = Vec::new();
    for candidate in &search.candidates[..n] {
        let token = candidate.token.clone();
        acquirer
            .play_external(&world.member, ExternalPlayRequest::Candidate(token.clone()))
            .await
            .unwrap();
        settle(acquirer).await;
        tokens.push(token);
    }
    tokens
}

const PAST_IN_USE: Duration = Duration::from_mins(31);

#[tokio::test(start_paused = true)]
async fn fetched_songs_outlive_a_restart_and_replay_without_fetching() {
    let world = World::new().await;
    let video = streaming_provider(&["bnVUHWCynig"]);
    let acquirer = world.start(vec![video.clone()]);
    let halo = || ExternalPlayRequest::Song {
        title: String::from("Halo"),
        artist: String::from("Beyoncé"),
        album: None,
        duration_ms: Some(200_000),
    };
    let token = acquirer
        .play_external(&world.member, halo())
        .await
        .unwrap()
        .token
        .unwrap();
    settle(&acquirer).await;
    acquirer.shutdown().await;

    let again = world.start(vec![video.clone()]);
    let play = again.play_external(&world.other, halo()).await.unwrap();
    assert_eq!(
        play.token.as_ref(),
        Some(&token),
        "asked by name, found by key"
    );
    let stream = again.external_stream(&world.other, &token).unwrap();
    assert_eq!(
        *stream.progressive.state.borrow(),
        GrowState::Complete {
            bytes: GOOD_FLAC.len() as u64
        }
    );
    assert_eq!(std::fs::read(&stream.progressive.path).unwrap(), GOOD_FLAC);
    assert_eq!(video.fetches.load(Ordering::SeqCst), 1);
    let usage = again.fetched_usage().unwrap();
    assert_eq!((usage.songs, usage.bytes), (1, GOOD_FLAC.len() as u64));
    assert_eq!((usage.keep, usage.max_bytes), (500, 20 << 30));
    assert_eq!(count(&world.db, "change_log").await, 0);
}

#[tokio::test(start_paused = true)]
async fn the_most_recently_played_songs_stay_within_the_count() {
    let mut world = World::new().await;
    world.config.fetched_keep = 2;
    let video = streaming_provider(&["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]);
    let acquirer = world.start(vec![video.clone()]);
    let tokens = play_all(&world, &acquirer, 2).await;
    tokio::time::advance(PAST_IN_USE).await;
    // The first is played again, so the second is now the least recently played.
    acquirer
        .play_external(
            &world.member,
            ExternalPlayRequest::Candidate(tokens[0].clone()),
        )
        .await
        .unwrap();
    let third = {
        let search = acquirer
            .search(
                &world.member,
                ExternalQuery::Text(String::from("beyonce halo")),
            )
            .await
            .unwrap();
        search.candidates[2].token.clone()
    };
    acquirer
        .play_external(&world.member, ExternalPlayRequest::Candidate(third.clone()))
        .await
        .unwrap();
    settle(&acquirer).await;
    assert!(acquirer.external_stream(&world.member, &tokens[1]).is_err());
    assert!(acquirer.external_stream(&world.member, &tokens[0]).is_ok());
    assert!(acquirer.external_stream(&world.member, &third).is_ok());
    let mut want = vec![tokens[0].to_string(), third.to_string()];
    want.sort();
    assert_eq!(cached_dirs(&world), want);
    assert!(!external_dir(&world).join(".trash").exists());
    assert_eq!(acquirer.fetched_usage().unwrap().songs, 2);
}

#[tokio::test(start_paused = true)]
async fn the_play_cache_stays_under_its_size() {
    let mut world = World::new().await;
    world.config.fetched_max_bytes = GOOD_FLAC.len() as u64;
    let video = streaming_provider(&["aaaaaaaaaaa", "bbbbbbbbbbb"]);
    let acquirer = world.start(vec![video.clone()]);
    let first = play_all(&world, &acquirer, 1).await.remove(0);
    tokio::time::advance(PAST_IN_USE).await;
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let second = search.candidates[1].token.clone();
    acquirer
        .play_external(
            &world.member,
            ExternalPlayRequest::Candidate(second.clone()),
        )
        .await
        .unwrap();
    settle(&acquirer).await;
    assert!(acquirer.external_stream(&world.member, &first).is_err());
    assert!(acquirer.external_stream(&world.member, &second).is_ok());
    assert_eq!(cached_dirs(&world), vec![second.to_string()]);
}

#[tokio::test(start_paused = true)]
async fn a_song_being_streamed_or_played_lately_is_never_evicted() {
    let mut world = World::new().await;
    world.config.fetched_keep = 0;
    let video = streaming_provider(&["aaaaaaaaaaa"]);
    let acquirer = world.start(vec![video.clone()]);
    let token = play_all(&world, &acquirer, 1).await.remove(0);
    assert!(
        acquirer.external_stream(&world.member, &token).is_ok(),
        "played moments ago"
    );
    let held = acquirer.external_stream(&world.member, &token).unwrap();
    tokio::time::advance(PAST_IN_USE).await;
    acquirer.run_janitor().await.unwrap();
    assert!(held.progressive.path.exists(), "a stream still reads it");
    assert_eq!(
        acquirer.fetched_usage().unwrap().songs,
        1,
        "over the limit while held"
    );
    drop(held);
    acquirer.run_janitor().await.unwrap();
    assert_eq!(acquirer.fetched_usage().unwrap().songs, 0);
    assert_eq!(cached_dirs(&world), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn a_song_still_downloading_is_never_evicted() {
    let mut world = World::new().await;
    world.config.fetched_keep = 0;
    world.config.fetched_max_bytes = 0;
    let video = streaming_provider(&["aaaaaaaaaaa"]);
    let (release, gate) = tokio::sync::oneshot::channel();
    *video.release.lock().unwrap() = Some(gate);
    let acquirer = world.start(vec![video.clone()]);
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let token = search.candidates[0].token.clone();
    acquirer
        .play_external(&world.member, ExternalPlayRequest::Candidate(token.clone()))
        .await
        .unwrap();
    tokio::time::advance(PAST_IN_USE).await;
    acquirer.run_janitor().await.unwrap();
    let usage = acquirer.fetched_usage().unwrap();
    assert_eq!((usage.songs, usage.fetching), (0, 1));
    assert_eq!(cached_dirs(&world), vec![token.to_string()]);
    release.send(()).unwrap();
    settle(&acquirer).await;
    assert!(
        acquirer.external_stream(&world.member, &token).is_ok(),
        "just finished, so still in use"
    );
}

#[tokio::test(start_paused = true)]
async fn two_fetches_finishing_together_leave_exactly_the_limit() {
    let mut world = World::new().await;
    world.config.fetched_keep = 1;
    let video = streaming_provider(&["aaaaaaaaaaa", "bbbbbbbbbbb"]);
    let timing = Timing {
        external_in_use: Duration::ZERO,
        ..World::timing()
    };
    let acquirer = world.start_timed(vec![video.clone()], timing);
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let plays = search.candidates.iter().map(|c| {
        acquirer.play_external(
            &world.member,
            ExternalPlayRequest::Candidate(c.token.clone()),
        )
    });
    for play in futures_util::future::join_all(plays).await {
        play.unwrap();
    }
    settle(&acquirer).await;
    assert_eq!(acquirer.fetched_usage().unwrap().songs, 1);
    assert_eq!(cached_dirs(&world).len(), 1);
    assert!(!external_dir(&world).join(".trash").exists());
}

#[tokio::test(start_paused = true)]
async fn an_evicted_song_played_again_is_fetched_again() {
    let mut world = World::new().await;
    world.config.fetched_keep = 0;
    let video = streaming_provider(&["aaaaaaaaaaa"]);
    let acquirer = world.start(vec![video.clone()]);
    let token = play_all(&world, &acquirer, 1).await.remove(0);
    tokio::time::advance(PAST_IN_USE).await;
    acquirer.run_janitor().await.unwrap();
    assert!(acquirer.external_stream(&world.member, &token).is_err());
    let play = acquirer
        .play_external(&world.member, ExternalPlayRequest::Candidate(token.clone()))
        .await
        .unwrap();
    assert_eq!(play.token.as_ref(), Some(&token));
    settle(&acquirer).await;
    let stream = acquirer.external_stream(&world.member, &token).unwrap();
    assert_eq!(std::fs::read(&stream.progressive.path).unwrap(), GOOD_FLAC);
    assert_eq!(video.fetches.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_crash_midway_is_cleaned_up_at_start() {
    let world = World::new().await;
    let video = streaming_provider(&["aaaaaaaaaaa", "bbbbbbbbbbb"]);
    let acquirer = world.start(vec![video.clone()]);
    let tokens = play_all(&world, &acquirer, 2).await;
    acquirer.shutdown().await;
    let root = external_dir(&world);
    let file_of =
        |token: &trackside_core::CandidateToken| root.join(token.as_str()).join("audio.stream");
    // The first lost its file after its record was written; the second has its file but no
    // record. A fetch was cut off, an eviction was moved to the trash but not deleted, and a
    // link points into the library.
    std::fs::remove_file(file_of(&tokens[0])).unwrap();
    std::fs::remove_file(root.join(tokens[1].as_str()).join("entry.json")).unwrap();
    let cut: trackside_core::CandidateToken = random_id();
    std::fs::create_dir_all(root.join(cut.as_str())).unwrap();
    std::fs::write(root.join(cut.as_str()).join("audio.stream"), b"fLa").unwrap();
    std::fs::create_dir_all(root.join(".trash").join("x.0")).unwrap();
    std::fs::write(root.join(".trash").join("x.0").join("a"), b"a").unwrap();
    let kept = world.library.join("Kept.flac");
    std::fs::write(&kept, GOOD_FLAC).unwrap();
    std::os::unix::fs::symlink(&world.library, root.join("library-link")).unwrap();
    let link: trackside_core::CandidateToken = random_id();
    std::os::unix::fs::symlink(&world.library, root.join(link.as_str())).unwrap();

    let again = world.start(vec![video.clone()]);
    again.run_janitor().await.unwrap();
    assert_eq!(again.fetched_usage().unwrap().songs, 0);
    assert_eq!(cached_dirs(&world), Vec::<String>::new());
    assert!(!root.join(".trash").exists());
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        GOOD_FLAC,
        "the library is never touched"
    );
    assert!(world.library.exists());
}

#[tokio::test(start_paused = true)]
async fn nothing_is_evicted_while_the_cache_volume_is_missing() {
    let world = World::new().await;
    let video = streaming_provider(&["aaaaaaaaaaa"]);
    let acquirer = world.start(vec![video.clone()]);
    let token = play_all(&world, &acquirer, 1).await.remove(0);
    tokio::time::advance(PAST_IN_USE).await;
    // The share is unmounted: the folder is there but empty.
    let root = external_dir(&world);
    let away = world.dir.path().join("away");
    std::fs::rename(&root, &away).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    acquirer
        .configure(ProviderChange::Fetched {
            keep: 0,
            max_bytes: 0,
        })
        .await
        .unwrap();
    settle(&acquirer).await;
    assert_eq!(
        acquirer.fetched_usage().unwrap().songs,
        1,
        "the index is left alone"
    );
    std::fs::remove_dir(&root).unwrap();
    std::fs::rename(&away, &root).unwrap();
    acquirer.run_janitor().await.unwrap();
    assert_eq!(acquirer.fetched_usage().unwrap().songs, 0);
    assert!(acquirer.external_stream(&world.member, &token).is_err());
    assert_eq!(cached_dirs(&world), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn the_limits_come_from_the_dashboard_and_apply_at_once() {
    let world = World::new().await;
    world
        .db
        .write(|tx| {
            settings::put(tx, "acquire.fetched_keep", &serde_json::json!(7))?;
            settings::put(tx, "acquire.fetched_max_bytes", &serde_json::json!("1GiB"))
        })
        .await
        .unwrap();
    let video = streaming_provider(&["aaaaaaaaaaa"]);
    let acquirer = world.start(vec![video.clone()]);
    play_all(&world, &acquirer, 1).await;
    let usage = acquirer.fetched_usage().unwrap();
    assert_eq!((usage.keep, usage.max_bytes), (7, 1 << 30));
    tokio::time::advance(PAST_IN_USE).await;
    acquirer
        .configure(ProviderChange::Fetched {
            keep: 0,
            max_bytes: 1 << 30,
        })
        .await
        .unwrap();
    settle(&acquirer).await;
    let usage = acquirer.fetched_usage().unwrap();
    assert_eq!((usage.songs, usage.keep), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn playing_without_keeping_respects_limits_and_cooldowns() {
    let world = World::new().await;
    let video = streaming_provider(&["aaaaaaaaaaa", "bbbbbbbbbbb"]);
    let (_release, gate) = tokio::sync::oneshot::channel::<()>();
    *video.release.lock().unwrap() = Some(gate);
    let timing = Timing {
        external_per_user: 1,
        ..World::timing()
    };
    let acquirer = world.start_timed(vec![video.clone()], timing);
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let tokens: Vec<_> = search.candidates.iter().map(|c| c.token.clone()).collect();
    acquirer
        .play_external(
            &world.member,
            ExternalPlayRequest::Candidate(tokens[0].clone()),
        )
        .await
        .unwrap();
    assert!(matches!(
        acquirer
            .play_external(
                &world.member,
                ExternalPlayRequest::Candidate(tokens[1].clone())
            )
            .await,
        Err(AcquireError::RateLimited { .. })
    ));
    assert!(matches!(
        acquirer
            .play_external(&world.member, ExternalPlayRequest::Candidate(random_id()))
            .await,
        Err(AcquireError::CandidateExpired)
    ));

    *video.search_error.lock().unwrap() = Some(|| ProviderError::BotCheck);
    let song = |title: &str| ExternalPlayRequest::Song {
        title: title.to_owned(),
        artist: String::from("Beyoncé"),
        album: None,
        duration_ms: None,
    };
    assert!(matches!(
        acquirer
            .play_external(&world.other, song("Single Ladies"))
            .await,
        Err(AcquireError::NotFound)
    ));
    assert!(matches!(
        acquirer.play_external(&world.other, song("Irreplaceable")).await,
        Err(AcquireError::Unavailable { retry_after }) if retry_after > Duration::ZERO
    ));
    assert_eq!(count(&world.db, "acquisitions").await, 0);
}

#[tokio::test(start_paused = true)]
async fn playing_without_keeping_needs_a_source_that_streams() {
    let mut world = World::new().await;
    let peer = FakeProvider::new(
        ProviderName::Soulseek,
        vec![candidate(ProviderName::Soulseek, "f", true)],
    );
    let permissions = Permissions {
        can_download: true,
        can_upload: false,
        can_request: true,
        legacy_subsonic: false,
        max_bitrate_kbps: None,
    };
    let acquirer = world.start(vec![peer]);
    let search = acquirer
        .search(
            &world.member,
            ExternalQuery::Text(String::from("beyonce halo")),
        )
        .await
        .unwrap();
    let feature = acquirer
        .feature_for(&world.member, &permissions, true)
        .unwrap();
    assert!(!feature.external_play);
    assert!(matches!(
        acquirer
            .play_external(
                &world.member,
                ExternalPlayRequest::Candidate(search.candidates[0].token.clone())
            )
            .await,
        Err(AcquireError::Unavailable { .. })
    ));

    let owned = world.insert_song("Halo", "Beyoncé", 200_000).await;
    let video = streaming_provider(&["bnVUHWCynig"]);
    let acquirer = world.start(vec![video.clone()]);
    let play = acquirer
        .play_external(
            &world.member,
            ExternalPlayRequest::Song {
                title: String::from("Halo"),
                artist: String::from("Beyoncé"),
                album: None,
                duration_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(play.song, Some(owned));
    assert_eq!(play.token, None);
    assert_eq!(video.fetches.load(Ordering::SeqCst), 0);

    world.config.enabled = false;
    let off = world.start(vec![streaming_provider(&["bnVUHWCynig"])]);
    assert!(matches!(
        off.play_external(&world.member, ExternalPlayRequest::Candidate(random_id()))
            .await,
        Err(AcquireError::Disabled)
    ));
}
