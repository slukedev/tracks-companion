//! The scanner against the real database and ingest. While a core function is still a stub
//! (`DbError::Unimplemented`), a test says so on stderr and returns early.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;
use trackside_core::ArtworkId;
use trackside_core::config::Config;
use trackside_core::control::{ImportOutcome, ScanControl, ScanKind, ScanTrigger, SongOrigin};
use trackside_core::records::{Library, ScanState};
use trackside_core::repo::artists;
use trackside_core::repo::artwork::{self, NewArtwork};
use trackside_core::repo::libraries::{self, NewLibrary};
use trackside_core::repo::{folders, scan_runs, songs};
use trackside_core::testing::TestDb;
use trackside_core::{Access, DbError, FolderId, LibraryId, PublicId, SongId};
use trackside_media::ArtworkSourceKind;
use trackside_scan::artwork::{ArtworkSink, row_kind};
use trackside_scan::catalog::DbCatalog;
use trackside_scan::reader::LoftyReader;
use trackside_scan::run::Services;
use trackside_scan::{Scanner, ScannerOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn stub(error: &DbError) -> bool {
    if matches!(error, DbError::Unimplemented(_)) {
        eprintln!("skipped: core is still a stub here ({error})");
        return true;
    }
    false
}

fn is_stub(error: &dyn std::error::Error) -> bool {
    let stubbed = error.to_string().contains("not implemented yet");
    if stubbed {
        eprintln!("skipped: core is still a stub here ({error})");
    }
    stubbed
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn put(root: &Path, rel: &str, name: &str) -> std::io::Result<()> {
    let dest = root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(fixture(name), &dest)?;
    std::fs::File::options()
        .write(true)
        .open(&dest)?
        .set_modified(SystemTime::now() - Duration::from_secs(3600))
}

/// Stands in for the media crate's store, which is not implemented yet, with the metadata a
/// real store would report.
struct FakeArt;

impl ArtworkSink for FakeArt {
    fn put(
        &self,
        bytes: &[u8],
        kind: ArtworkSourceKind,
        path: Option<&Path>,
    ) -> Option<NewArtwork> {
        Some(NewArtwork {
            id: ArtworkId::of_bytes(bytes),
            mime: String::from("image/jpeg"),
            width: 500,
            height: 500,
            bytes: bytes.len() as u64,
            source: row_kind(kind),
            source_path: path.map(Path::to_path_buf),
        })
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    db: TestDb,
    library: Library,
    scanner: Arc<Scanner>,
    shutdown: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn setup() -> Result<Option<Setup>, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("music");
    std::fs::create_dir_all(&root)?;
    let db = TestDb::new()?;
    let new = NewLibrary {
        name: "Music".into(),
        root: root.clone(),
        writable: true,
        watch: false,
        grant_all_users: false,
        granted_by: None,
    };
    let library = match db.write_blocking(|tx| libraries::create(tx, &new)) {
        Ok(l) => l,
        Err(e) if stub(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut config = Config::defaults(dir.path()).scanner;
    config.threads = 2;
    let uploads = Config::defaults(dir.path()).uploads;
    let shutdown = CancellationToken::new();
    let services = Services {
        catalog: Arc::new(DbCatalog::new(db.db.clone())),
        reader: Arc::new(LoftyReader),
        artwork: Arc::new(FakeArt),
        clock: db.clock.clone(),
    };
    let scanner = Scanner::with_services(
        services,
        &config,
        &uploads,
        db.events.clone(),
        shutdown.clone(),
        &ScannerOptions::manual(),
    )?;
    Ok(Some(Setup {
        _dir: dir,
        root,
        db,
        library,
        scanner,
        shutdown,
    }))
}

fn scan(
    s: &Setup,
    kind: ScanKind,
) -> Result<trackside_core::records::ScanRun, Box<dyn std::error::Error>> {
    s.db.clock.advance(Duration::from_secs(1));
    s.scanner
        .request_scan(&s.library.id, kind, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(30)));
    let key = s.library.key;
    let runs = s.db.read_blocking(move |c| scan_runs::list(c, key, 1))?;
    Ok(runs.into_iter().next().ok_or("no run")?)
}

fn songs_in(s: &Setup, folder: &[u8]) -> Result<Vec<trackside_core::records::Song>, DbError> {
    let id = FolderId::derive(&s.library.id, folder);
    s.db.read_blocking(move |c| songs::by_folder(c, &Access::system(), &id))
}

#[test]
fn a_scan_indexes_files_and_a_quick_rescan_changes_nothing() -> TestResult {
    let Some(s) = setup()? else { return Ok(()) };
    put(&s.root, "Artist/Album/01 a.flac", "tone-440.flac")?;
    put(&s.root, "Artist/Album/02 b.mp3", "tone-440.mp3")?;
    put(&s.root, "Artist/Album/03 c.m4a", "tone-440.m4a")?;
    let first = match scan(&s, ScanKind::Full) {
        Ok(run) => run,
        Err(e) if is_stub(e.as_ref()) => return Ok(()),
        Err(e) => return Err(e),
    };
    assert_eq!(first.state, ScanState::Done, "{first:?}");
    assert_eq!(first.counts.added, 3);
    let found = songs_in(&s, b"Artist/Album")?;
    assert_eq!(found.len(), 3);
    let flac = found
        .iter()
        .find(|song| song.title == "Tone 440" && song.format.container.as_str() == "flac");
    let digest = trackside_scan::audio_digest(
        &fixture("tone-440.flac"),
        trackside_core::records::Container::Flac,
    )?
    .0;
    assert_eq!(flac.map(|f| f.id.clone()), Some(SongId::derive(&digest)));

    let quick = scan(&s, ScanKind::Quick)?;
    assert_eq!(quick.state, ScanState::Done);
    assert_eq!(
        (
            quick.counts.added,
            quick.counts.updated,
            quick.counts.missing
        ),
        (0, 0, 0)
    );
    let key = s.library.key;
    let state =
        s.db.read_blocking(move |c| folders::listing_hash(c, key, b"Artist/Album"))?;
    assert!(state.is_some_and(|f| f.listing_hash.is_some()));
    Ok(())
}

#[test]
fn an_emptied_root_marks_nothing_missing_and_a_move_keeps_the_id() -> TestResult {
    let Some(s) = setup()? else { return Ok(()) };
    put(&s.root, "A/1.flac", "tone-440.flac")?;
    put(&s.root, "A/2.mp3", "tone-660.mp3")?;
    match scan(&s, ScanKind::Full) {
        Ok(_) => {}
        Err(e) if is_stub(e.as_ref()) => return Ok(()),
        Err(e) => return Err(e),
    }
    let before = songs_in(&s, b"A")?;
    let parked = s.root.with_file_name("parked");
    std::fs::rename(&s.root, &parked)?;
    std::fs::create_dir(&s.root)?;
    let run = scan(&s, ScanKind::Quick)?;
    assert_eq!(run.state, ScanState::Unavailable);
    let after = songs_in(&s, b"A")?;
    assert!(after.iter().all(|song| !song.missing));
    std::fs::remove_dir(&s.root)?;
    std::fs::rename(&parked, &s.root)?;
    s.scanner.probe_now();
    assert!(s.scanner.wait_idle(Duration::from_secs(30)));

    std::fs::create_dir_all(s.root.join("B"))?;
    std::fs::rename(s.root.join("A/1.flac"), s.root.join("B/1.flac"))?;
    let run = scan(&s, ScanKind::Quick)?;
    assert_eq!(run.state, ScanState::Done, "{run:?}");
    assert_eq!(run.counts.moved, 1, "{run:?}");
    let moved = songs_in(&s, b"B")?;
    let original = before
        .iter()
        .find(|song| song.format.container.as_str() == "flac")
        .map(|song| song.id.clone());
    assert_eq!(moved.first().map(|song| song.id.clone()), original);
    Ok(())
}

#[test]
fn import_file_returns_the_final_id() -> TestResult {
    let Some(s) = setup()? else { return Ok(()) };
    put(&s.root, "Acquired/x.opus", "tone-440.opus")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let id: LibraryId = s.library.id.clone();
    let outcome = runtime.block_on(s.scanner.import_file(
        &id,
        Path::new("Acquired/x.opus"),
        SongOrigin::Acquired,
    ));
    let outcome = match outcome {
        Ok(o) => o,
        Err(trackside_core::control::ScanControlError::Db(e)) if stub(&e) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let ImportOutcome::Added(song) = outcome else {
        return Err(format!("expected a new song, got {outcome:?}").into());
    };
    let stored = s.db.read_blocking({
        let song = song.clone();
        move |c| songs::get(c, &Access::system(), &song)
    })?;
    assert_eq!(
        stored.map(|x| x.id.as_str().to_owned()),
        Some(song.as_str().to_owned())
    );
    let run = scan(&s, ScanKind::Quick)?;
    assert_eq!((run.counts.added, run.counts.updated), (0, 0));
    Ok(())
}

#[test]
fn artwork_rows_carry_real_metadata_and_artist_pictures_reach_the_artist() -> TestResult {
    let Some(s) = setup()? else { return Ok(()) };
    put(
        &s.root,
        "Test Artist/Fixtures/03 Tone.flac",
        "tone-440.flac",
    )?;
    std::fs::write(
        s.root.join("Test Artist/Fixtures/cover.jpg"),
        b"cover bytes",
    )?;
    std::fs::write(s.root.join("Test Artist/artist.jpg"), b"artist bytes")?;
    let run = scan(&s, ScanKind::Full)?;
    assert_eq!(run.state, ScanState::Done, "{run:?}");
    let cover = ArtworkId::of_bytes(b"cover bytes");
    let face = ArtworkId::of_bytes(b"artist bytes");
    let row = s.db.read_blocking({
        let cover = cover.clone();
        move |c| artwork::get(c, &cover)
    })?;
    let row = row.ok_or("no artwork row")?;
    assert_eq!(
        (row.art.mime.as_str(), row.art.width, row.art.bytes),
        ("image/jpeg", 500, 11)
    );
    let found = songs_in(&s, b"Test Artist/Fixtures")?;
    assert_eq!(
        found.first().and_then(|song| song.artwork.clone()),
        Some(cover)
    );
    let artist =
        s.db.read_blocking(|c| artists::by_name(c, &Access::system(), "Test Artist"))?
            .ok_or("no artist")?;
    assert_eq!(artist.artwork, Some(face));
    Ok(())
}

#[test]
fn a_folder_scan_finds_a_move_and_marks_only_its_folder() -> TestResult {
    let Some(s) = setup()? else { return Ok(()) };
    put(&s.root, "Z/1.flac", "tone-440.flac")?;
    put(&s.root, "Y/2.mp3", "tone-660.mp3")?;
    let first = scan(&s, ScanKind::Full)?;
    assert_eq!(first.state, ScanState::Done, "{first:?}");
    let original = songs_in(&s, b"Z")?
        .first()
        .map(|song| song.id.clone())
        .ok_or("not indexed")?;
    std::fs::create_dir_all(s.root.join("A"))?;
    std::fs::rename(s.root.join("Z/1.flac"), s.root.join("A/1.flac"))?;
    std::fs::remove_file(s.root.join("Y/2.mp3"))?;
    s.db.clock.advance(Duration::from_secs(1));
    s.scanner
        .request_folder_scan(&s.library.id, Path::new("A"))?;
    assert!(s.scanner.wait_idle(Duration::from_secs(30)));
    let moved = songs_in(&s, b"A")?;
    assert_eq!(moved.first().map(|song| song.id.clone()), Some(original));
    let y = songs_in(&s, b"Y")?;
    assert!(
        y.iter().all(|song| !song.missing),
        "a folder scan of A touched Y"
    );
    Ok(())
}
