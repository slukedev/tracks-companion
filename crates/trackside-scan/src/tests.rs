//! The WP2 acceptance tests, run against the in-memory catalog so they exercise the scanner's
//! own logic (walking, skipping, guards, deferral, quarantine) independently of core.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture as LoftyPicture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagExt};
use tokio_util::sync::CancellationToken;
use trackside_core::config::{Config, ScannerConfig, UploadsConfig};
use trackside_core::control::{
    ImportOutcome, ScanControl, ScanControlError, ScanKind, ScanTrigger, SongOrigin,
};
use trackside_core::records::{
    Codec, Container, ImportSource, ImportState, Library, ScanCounts, ScanRunKind, ScanState,
};
use trackside_core::repo::artwork::NewArtwork;
use trackside_core::repo::imports::NewImport;
use trackside_core::{ArtworkId, EventBus, ServerEvent, SystemClock};
use trackside_media::ArtworkSourceKind;

use crate::artwork::ArtworkSink;
use crate::catalog::{Catalog, RootHealth};
use crate::reader::FileReader;
use crate::run::{self, RunInput, RunReport, RunSettings, Services};
use crate::testing::{
    Behaviour, MemCatalog, TestReader, age, fixture, put_fixture, put_wav, wav_bytes,
};
use crate::{DigestMode, Scanner, ScannerOptions, audio_digest, read_file};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Default)]
struct ArtRecorder {
    puts: AtomicUsize,
}

impl ArtworkSink for ArtRecorder {
    fn put(
        &self,
        bytes: &[u8],
        kind: ArtworkSourceKind,
        path: Option<&Path>,
    ) -> Option<NewArtwork> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        Some(NewArtwork {
            id: ArtworkId::of_bytes(bytes),
            mime: String::from("image/jpeg"),
            width: 600,
            height: 600,
            bytes: bytes.len() as u64,
            source: crate::artwork::row_kind(kind),
            source_path: path.map(Path::to_path_buf),
        })
    }
}

fn scanner_config() -> ScannerConfig {
    let mut config = Config::defaults("/nonexistent").scanner;
    config.threads = 2;
    config
}

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<MemCatalog>,
    reader: Arc<TestReader>,
    services: Services,
    settings: RunSettings,
    settle: Duration,
    library: Library,
}

impl Harness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root)?;
        let catalog = MemCatalog::new();
        let library = catalog.add_library(&root, "Music");
        let reader = TestReader::new();
        let services = Services {
            catalog: catalog.clone(),
            reader: reader.clone(),
            artwork: Arc::new(ArtRecorder::default()),
            clock: Arc::new(SystemClock),
        };
        let config = scanner_config();
        Ok(Self {
            _dir: dir,
            root,
            catalog,
            reader,
            services,
            settings: RunSettings::from_config(&config),
            settle: config.settle,
            library,
        })
    }

    fn scan_with(
        &self,
        kind: ScanKind,
        force_empty: bool,
        cancel: &CancellationToken,
    ) -> Result<RunReport, Box<dyn std::error::Error>> {
        self.run_scoped(kind, force_empty, cancel, None)
    }

    fn run_scoped(
        &self,
        kind: ScanKind,
        force_empty: bool,
        cancel: &CancellationToken,
        scope: Option<Vec<u8>>,
    ) -> Result<RunReport, Box<dyn std::error::Error>> {
        let library = self
            .catalog
            .library(&self.library.id)?
            .ok_or("library vanished")?;
        let run_kind = match (kind, &scope) {
            (_, Some(_)) => ScanRunKind::Folder,
            (ScanKind::Quick, None) => ScanRunKind::Quick,
            (ScanKind::Full, None) => ScanRunKind::Full,
        };
        let run = self
            .catalog
            .start_run(library.key, run_kind, ScanTrigger::Admin)?;
        let progress = |_: &ScanCounts| {};
        let input = RunInput {
            library: &library,
            run: &run.id,
            kind,
            force_empty,
            settle: self.settle,
            scope,
            cancel,
            progress: &progress,
        };
        Ok(run::execute(&self.services, &self.settings, &input)?)
    }

    fn scan_folder(&self, folder: &str) -> Result<RunReport, Box<dyn std::error::Error>> {
        self.run_scoped(
            ScanKind::Quick,
            false,
            &CancellationToken::new(),
            Some(folder.as_bytes().to_vec()),
        )
    }

    fn scan(&self, kind: ScanKind) -> Result<RunReport, Box<dyn std::error::Error>> {
        self.scan_with(kind, false, &CancellationToken::new())
    }

    fn key(&self) -> trackside_core::LibraryKey {
        self.library.key
    }
}

const CONTAINERS: &[(&str, Container, Codec)] = &[
    ("tone-440.flac", Container::Flac, Codec::Flac),
    ("tone-440.mp3", Container::Mp3, Codec::Mp3),
    ("tone-440.m4a", Container::M4a, Codec::Aac),
    ("tone-440-alac.m4a", Container::M4a, Codec::Alac),
    ("tone-440.ogg", Container::Ogg, Codec::Vorbis),
    ("tone-440.opus", Container::Opus, Codec::Opus),
    ("tone-440.wav", Container::Wav, Codec::Pcm),
    ("tone-440.aiff", Container::Aiff, Codec::Pcm),
];

#[test]
fn every_container_reads_tags_and_properties() -> TestResult {
    for (name, container, codec) in CONTAINERS {
        let parsed = read_file(&fixture(name))?;
        assert_eq!(parsed.properties.container, *container, "{name}");
        assert_eq!(parsed.properties.codec, *codec, "{name}");
        assert!(
            parsed.properties.duration_ms >= 900,
            "{name}: {:?}",
            parsed.properties
        );
        assert_eq!(parsed.tags.title, "Tone 440", "{name}");
        assert_eq!(parsed.tags.artist, "Test Artist", "{name}");
        assert_eq!(parsed.tags.album.as_deref(), Some("Fixtures"), "{name}");
        assert_eq!(parsed.tags.track, Some(3), "{name}");
        assert_eq!(parsed.tag_problem, None, "{name}");
    }
    Ok(())
}

fn rewrite_tags(path: &Path) -> TestResult {
    let mut tagged = lofty::read_from_path(path)?;
    let tag_type = tagged.primary_tag_type();
    if tagged.primary_tag().is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged.primary_tag_mut().ok_or("no tag")?;
    tag.set_title(String::from("A Completely Different Title"));
    tag.set_artist(String::from("Someone Else"));
    tag.insert_text(ItemKey::Comment, "x".repeat(20_000));
    tag.push_picture(
        LoftyPicture::unchecked(vec![0xAB; 30_000])
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default())?;
    Ok(())
}

#[test]
fn digest_ignores_tag_rewrites() -> TestResult {
    let dir = tempfile::tempdir()?;
    for (name, container, _) in CONTAINERS {
        let path = dir.path().join(name);
        std::fs::copy(fixture(name), &path)?;
        let before = audio_digest(&path, *container)?;
        let size_before = std::fs::metadata(&path)?.len();
        rewrite_tags(&path)?;
        assert_ne!(
            std::fs::metadata(&path)?.len(),
            size_before,
            "{name} was not rewritten"
        );
        let after = audio_digest(&path, *container)?;
        assert_eq!(before.1, DigestMode::Payload, "{name}");
        assert_eq!(before, after, "{name}");
        let parsed = read_file(&path)?;
        assert_eq!(parsed.tags.title, "A Completely Different Title", "{name}");
    }
    Ok(())
}

#[test]
fn digest_differs_for_different_audio() -> TestResult {
    for (a, b, container) in [
        ("tone-440.flac", "tone-660.flac", Container::Flac),
        ("tone-440.mp3", "tone-660.mp3", Container::Mp3),
    ] {
        assert_ne!(
            audio_digest(&fixture(a), container)?,
            audio_digest(&fixture(b), container)?
        );
    }
    let dir = tempfile::tempdir()?;
    let one = dir.path().join("1.wav");
    let two = dir.path().join("2.wav");
    std::fs::write(&one, wav_bytes(1, 200_000))?;
    std::fs::write(&two, wav_bytes(2, 200_000))?;
    assert_ne!(
        audio_digest(&one, Container::Wav)?,
        audio_digest(&two, Container::Wav)?
    );
    Ok(())
}

#[test]
fn embedded_pictures_and_lyrics_are_found() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("01 Song.flac");
    std::fs::copy(fixture("tone-440.flac"), &path)?;
    let mut tagged = lofty::read_from_path(&path)?;
    let tag = tagged.primary_tag_mut().ok_or("no tag")?;
    tag.push_picture(
        LoftyPicture::unchecked(vec![1, 2, 3])
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Jpeg)
            .build(),
    );
    tag.insert_text(ItemKey::Lyrics, String::from("[00:01.00]Hello"));
    tag.save_to_path(&path, WriteOptions::default())?;
    std::fs::write(dir.path().join("01 Song.lrc"), "plain sidecar words")?;
    let parsed = read_file(&path)?;
    assert_eq!(parsed.pictures.len(), 1);
    assert_eq!(parsed.pictures[0].kind, crate::PictureKind::FrontCover);
    assert_eq!(parsed.lyrics.len(), 2, "{:?}", parsed.lyrics);
    Ok(())
}

#[test]
fn full_scan_indexes_every_container_with_stable_ids() -> TestResult {
    let h = Harness::new()?;
    for (i, (name, _, _)) in CONTAINERS.iter().enumerate() {
        put_fixture(&h.root, &format!("Album/{i:02} {name}"), name)?;
    }
    let report = h.scan(ScanKind::Full)?;
    assert_eq!(report.state, ScanState::Done, "{report:?}");
    assert_eq!(report.counts.added, CONTAINERS.len() as u64);
    let first: Vec<_> = h
        .catalog
        .present(h.key())
        .into_iter()
        .map(|(_, s)| s.id)
        .collect();
    let rescan = h.scan(ScanKind::Full)?;
    assert_eq!(rescan.counts.added, 0);
    assert_eq!(rescan.counts.updated, 0);
    let second: Vec<_> = h
        .catalog
        .present(h.key())
        .into_iter()
        .map(|(_, s)| s.id)
        .collect();
    assert_eq!(first, second);
    Ok(())
}

#[test]
fn quick_scan_with_no_changes_reads_no_files() -> TestResult {
    let h = Harness::new()?;
    for i in 0..20 {
        put_wav(&h.root, &format!("A{}/{i}.wav", i % 4), i)?;
    }
    h.scan(ScanKind::Full)?;
    assert_eq!(h.reader.reads(), 20);
    h.reader.reset();
    let report = h.scan(ScanKind::Quick)?;
    assert_eq!(report.state, ScanState::Done);
    assert_eq!(h.reader.reads(), 0);
    assert_eq!(report.counts.files_seen, 20);
    assert_eq!(h.catalog.missing(h.key()), 0);

    put_wav(&h.root, "A1/new.wav", 99)?;
    let report = h.scan(ScanKind::Quick)?;
    assert_eq!(h.reader.reads(), 1);
    assert_eq!(report.counts.added, 1);
    Ok(())
}

#[test]
fn file_written_during_scan_is_deferred_not_imported() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "a.wav", 1)?;
    put_wav(&h.root, "growing.wav", 2)?;
    h.reader.on("growing.wav", Behaviour::Grow);
    let report = h.scan(ScanKind::Full)?;
    assert_eq!(report.deferred, vec![b"growing.wav".to_vec()]);
    assert!(h.catalog.song(h.key(), b"growing.wav").is_none());
    assert!(h.catalog.song(h.key(), b"a.wav").is_some());

    std::fs::write(h.root.join("fresh.wav"), wav_bytes(3, 800))?;
    let report = h.scan(ScanKind::Quick)?;
    assert!(report.deferred.contains(&b"fresh.wav".to_vec()));
    assert!(h.catalog.song(h.key(), b"fresh.wav").is_none());
    Ok(())
}

#[test]
fn unmounted_library_is_marked_unavailable_and_nothing_goes_missing() -> TestResult {
    let h = Harness::new()?;
    for i in 0..6 {
        put_wav(&h.root, &format!("Artist/{i}.wav"), i)?;
    }
    assert_eq!(h.scan(ScanKind::Full)?.state, ScanState::Done);

    let parked = h.root.with_file_name("parked");
    std::fs::rename(&h.root, &parked)?;
    std::fs::create_dir(&h.root)?;
    let empty = h.scan(ScanKind::Quick)?;
    assert_eq!(empty.state, ScanState::Unavailable);
    assert!(empty.unavailable.is_some());
    assert_eq!(h.catalog.missing(h.key()), 0);

    std::fs::remove_dir(&h.root)?;
    let gone = h.scan(ScanKind::Quick)?;
    assert_eq!(gone.state, ScanState::Unavailable);
    assert!(gone.root_gone);
    assert_eq!(h.catalog.missing(h.key()), 0);

    std::fs::rename(&parked, &h.root)?;
    let health = h.catalog.root_health(h.key())?.ok_or("no root health")?;
    h.catalog.lock().root_health.insert(
        h.key(),
        RootHealth {
            dev: health.dev.map(|d| d + 1),
            ..health
        },
    );
    let other_device = h.scan(ScanKind::Quick)?;
    assert_eq!(other_device.state, ScanState::Unavailable);
    assert!(
        other_device
            .unavailable
            .as_deref()
            .is_some_and(|r| r.contains("different device"))
    );
    assert_eq!(h.catalog.missing(h.key()), 0);
    h.catalog.lock().root_health.insert(h.key(), health);

    assert_eq!(h.scan(ScanKind::Quick)?.state, ScanState::Done);
    assert_eq!(h.catalog.missing(h.key()), 0);
    assert_eq!(h.catalog.lock().mark_missing_calls, 2);
    Ok(())
}

#[test]
fn losing_most_files_trips_the_guard_until_forced() -> TestResult {
    let h = Harness::new()?;
    for i in 0..6 {
        put_wav(&h.root, &format!("{i}.wav"), i)?;
    }
    h.scan(ScanKind::Full)?;
    for i in 0..5 {
        std::fs::remove_file(h.root.join(format!("{i}.wav")))?;
    }
    let guarded = h.scan(ScanKind::Quick)?;
    assert_eq!(guarded.state, ScanState::Unavailable);
    assert_eq!(h.catalog.missing(h.key()), 0);
    let forced = h.scan_with(ScanKind::Quick, true, &CancellationToken::new())?;
    assert_eq!(forced.state, ScanState::Done);
    assert_eq!(forced.counts.missing, 5);
    Ok(())
}

#[test]
fn unreadable_folder_does_not_mark_its_songs_missing() -> TestResult {
    let mut h = Harness::new()?;
    put_wav(&h.root, "Locked/1.wav", 1)?;
    put_wav(&h.root, "Locked/Sub/2.wav", 2)?;
    put_wav(&h.root, "Open/3.wav", 3)?;
    put_wav(&h.root, "Open/4.wav", 4)?;
    h.scan(ScanKind::Full)?;
    std::fs::remove_file(h.root.join("Open/4.wav"))?;
    h.settings.deny = vec![b"Locked".to_vec()];
    let report = h.scan(ScanKind::Quick)?;
    assert_eq!(report.state, ScanState::Done);
    assert_eq!(report.counts.missing, 1);
    assert!(
        h.catalog
            .song(h.key(), b"Locked/1.wav")
            .is_some_and(|s| s.missing_since.is_none())
    );
    assert!(
        h.catalog
            .song(h.key(), b"Locked/Sub/2.wav")
            .is_some_and(|s| s.missing_since.is_none())
    );
    assert!(
        h.catalog
            .song(h.key(), b"Open/4.wav")
            .is_some_and(|s| s.missing_since.is_some())
    );
    let mem = h.catalog.lock();
    assert!(mem.errors.iter().any(|e| e.rel_path == b"Locked"));
    assert!(
        mem.folders
            .get(&(h.key(), b"Locked".to_vec()))
            .is_some_and(|f| f.unreadable)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_loop_terminates() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "A/1.wav", 1)?;
    std::os::unix::fs::symlink("..", h.root.join("A/loop"))?;
    std::os::unix::fs::symlink(h.root.join("A"), h.root.join("B"))?;
    std::os::unix::fs::symlink(h.root.join("A/1.wav"), h.root.join("link.wav"))?;
    let report = h.scan(ScanKind::Full)?;
    assert_eq!(report.state, ScanState::Done);
    assert_eq!(report.counts.added, 1);
    assert_eq!(h.catalog.present(h.key()).len(), 1);
    assert!(
        h.catalog
            .lock()
            .errors
            .iter()
            .any(|e| e.error.contains("already visited"))
    );
    Ok(())
}

#[test]
fn corrupt_tags_fall_back_to_file_name() -> TestResult {
    let h = Harness::new()?;
    let path = put_fixture(&h.root, "07 - My Song.mp3", "tone-440.mp3")?;
    let mut bytes = std::fs::read(&path)?;
    let size = bytes[6..10]
        .iter()
        .fold(0usize, |acc, b| (acc << 7) | usize::from(*b));
    for b in &mut bytes[10..10 + size] {
        *b = 0xFE;
    }
    std::fs::write(&path, &bytes)?;
    age(&path)?;
    put_wav(&h.root, "03 Untagged.wav", 5)?;
    h.scan(ScanKind::Full)?;
    let song = h
        .catalog
        .song(h.key(), b"07 - My Song.mp3")
        .ok_or("not indexed")?;
    assert_eq!(song.title, "My Song");
    assert_eq!(song.artist, crate::tags::UNKNOWN_ARTIST);
    assert!(song.tag_problem.is_some());
    let untagged = h
        .catalog
        .song(h.key(), b"03 Untagged.wav")
        .ok_or("not indexed")?;
    assert_eq!(untagged.title, "Untagged");
    assert_eq!(untagged.tag_problem.as_deref(), Some("no_tags"));
    Ok(())
}

#[test]
fn files_without_audio_are_recorded_and_skipped() -> TestResult {
    let h = Harness::new()?;
    std::fs::write(h.root.join("junk.flac"), b"definitely not audio")?;
    age(&h.root.join("junk.flac"))?;
    put_wav(&h.root, "ok.wav", 1)?;
    let report = h.scan(ScanKind::Full)?;
    assert_eq!(report.state, ScanState::Done);
    assert_eq!(report.counts.errors, 1);
    assert!(h.catalog.song(h.key(), b"junk.flac").is_none());
    assert!(
        h.catalog
            .lock()
            .errors
            .iter()
            .any(|e| e.rel_path == b"junk.flac")
    );
    Ok(())
}

#[test]
fn panicking_parser_quarantines_the_file() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "bad.wav", 1)?;
    put_wav(&h.root, "good.wav", 2)?;
    h.reader.on("bad.wav", Behaviour::Panic);
    let report = h.scan(ScanKind::Full)?;
    assert_eq!(report.state, ScanState::Done);
    assert!(h.catalog.song(h.key(), b"good.wav").is_some());
    assert!(h.catalog.song(h.key(), b"bad.wav").is_none());
    assert!(
        h.catalog
            .lock()
            .quarantine
            .contains_key(&(h.key(), b"bad.wav".to_vec()))
    );

    h.reader.reset();
    h.scan(ScanKind::Full)?;
    assert_eq!(h.reader.reads(), 1, "the quarantined file was read again");

    h.reader.behaviours.lock().map_err(|_| "poisoned")?.clear();
    std::fs::write(h.root.join("bad.wav"), wav_bytes(7, 900))?;
    age(&h.root.join("bad.wav"))?;
    h.scan(ScanKind::Quick)?;
    assert!(h.catalog.song(h.key(), b"bad.wav").is_some());
    Ok(())
}

#[test]
fn hanging_parser_times_out_and_quarantines_the_file() -> TestResult {
    let mut h = Harness::new()?;
    h.settings.file_timeout = Duration::from_millis(200);
    put_wav(&h.root, "hang.wav", 1)?;
    put_wav(&h.root, "fine.wav", 2)?;
    let (release, rx) = std::sync::mpsc::channel::<()>();
    h.reader
        .on("hang.wav", Behaviour::Hang(Arc::new(Mutex::new(rx))));
    let report = h.scan(ScanKind::Full)?;
    drop(release);
    assert_eq!(report.state, ScanState::Done);
    assert!(h.catalog.song(h.key(), b"fine.wav").is_some());
    let quarantined = h
        .catalog
        .lock()
        .quarantine
        .get(&(h.key(), b"hang.wav".to_vec()))
        .cloned();
    assert!(quarantined.is_some_and(|q| q.reason.contains("longer than")));
    Ok(())
}

#[test]
fn a_stalled_share_stops_the_scan_without_quarantining_or_marking_missing() -> TestResult {
    let mut h = Harness::new()?;
    for i in 0..20 {
        put_wav(&h.root, &format!("{i:02}.wav"), i)?;
    }
    h.scan(ScanKind::Full)?;
    std::fs::remove_file(h.root.join("19.wav"))?;
    h.settings.file_timeout = Duration::from_millis(200);
    let (release, rx) = std::sync::mpsc::channel::<()>();
    h.reader
        .on("05.wav", Behaviour::Stall(Arc::new(Mutex::new(rx))));
    let report = h.scan(ScanKind::Full)?;
    drop(release);
    assert_eq!(report.state, ScanState::Unavailable);
    assert!(
        report
            .unavailable
            .as_deref()
            .is_some_and(|r| r.contains("stopped answering"))
    );
    assert!(h.catalog.lock().quarantine.is_empty());
    assert_eq!(h.catalog.missing(h.key()), 0);
    Ok(())
}

#[test]
fn a_failing_commit_ends_the_scan_instead_of_hanging() -> TestResult {
    let h = Harness::new()?;
    for i in 0..(run::JOB_QUEUE as u32 * 2) {
        put_wav(&h.root, &format!("Singles/{i:04}.wav"), i)?;
    }
    h.catalog.lock().fail_commits = true;
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(h.scan(ScanKind::Full).is_err());
    });
    let failed = finished.recv_timeout(Duration::from_secs(60))?;
    assert!(failed);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_followed_link_to_a_vanished_folder_keeps_its_songs() -> TestResult {
    let mut h = Harness::new()?;
    h.settings.follow_symlinks = true;
    let share = h.root.with_file_name("share");
    put_wav(&share, "Album/1.wav", 1)?;
    put_wav(&share, "Album/2.wav", 2)?;
    // Enough local songs that the share-of-known-files guard does not catch the loss.
    for i in 3..8 {
        put_wav(&h.root, &format!("Local/{i}.wav"), i)?;
    }
    std::os::unix::fs::symlink(share.join("Album"), h.root.join("Linked"))?;
    std::os::unix::fs::symlink(share.join("never.wav"), h.root.join("broken.wav"))?;
    h.scan(ScanKind::Full)?;
    assert_eq!(h.catalog.present(h.key()).len(), 7);
    std::fs::rename(&share, h.root.with_file_name("share-offline"))?;
    let report = h.scan(ScanKind::Quick)?;
    assert_eq!(report.state, ScanState::Done);
    assert_eq!(h.catalog.missing(h.key()), 0);
    assert!(
        h.catalog
            .lock()
            .folders
            .get(&(h.key(), b"Linked".to_vec()))
            .is_some_and(|f| f.unreadable)
    );
    Ok(())
}

#[test]
fn a_library_going_unavailable_stops_its_running_scan() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    for i in 0..3 {
        put_wav(&s.root, &format!("{i}.wav"), i)?;
    }
    let (release, rx) = std::sync::mpsc::channel::<()>();
    s.reader
        .on("1.wav", Behaviour::Hang(Arc::new(Mutex::new(rx))));
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    s.reader.on(
        "0.wav",
        Behaviour::Before(Arc::new(move || {
            let _ = started_tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(());
        })),
    );
    s.scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    started.recv_timeout(Duration::from_secs(10))?;
    let parked = s.root.with_file_name("parked");
    std::fs::rename(&s.root, &parked)?;
    let stopped_at = Instant::now();
    s.scanner.probe_now();
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    assert!(stopped_at.elapsed() < Duration::from_secs(10));
    drop(release);
    let last = s
        .catalog
        .recent_runs(s.library.key, 1)?
        .pop()
        .ok_or("no run")?;
    assert_eq!(last.state, ScanState::Aborted);
    assert_eq!(s.catalog.missing(s.library.key), 0);
    Ok(())
}

#[test]
fn moved_and_case_renamed_files_keep_their_ids() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "Old/Song.wav", 1)?;
    put_wav(&h.root, "Old/Other.wav", 2)?;
    h.scan(ScanKind::Full)?;
    let id = h
        .catalog
        .song(h.key(), b"Old/Song.wav")
        .ok_or("missing")?
        .id;
    let other = h
        .catalog
        .song(h.key(), b"Old/Other.wav")
        .ok_or("missing")?
        .id;
    std::fs::create_dir_all(h.root.join("New"))?;
    std::fs::rename(h.root.join("Old/Song.wav"), h.root.join("New/Song.wav"))?;
    std::fs::rename(h.root.join("Old/Other.wav"), h.root.join("Old/other.wav"))?;
    let report = h.scan(ScanKind::Quick)?;
    assert_eq!(report.counts.moved, 2, "{report:?}");
    assert_eq!(report.counts.added, 0);
    assert_eq!(
        h.catalog.song(h.key(), b"New/Song.wav").map(|s| s.id),
        Some(id)
    );
    assert_eq!(
        h.catalog.song(h.key(), b"Old/other.wav").map(|s| s.id),
        Some(other)
    );
    assert_eq!(h.catalog.missing(h.key()), 0);
    Ok(())
}

#[test]
fn exact_duplicates_get_distinct_ids() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "a/x.wav", 1)?;
    put_wav(&h.root, "b/x.wav", 1)?;
    h.scan(ScanKind::Full)?;
    let a = h.catalog.song(h.key(), b"a/x.wav").ok_or("missing")?;
    let b = h.catalog.song(h.key(), b"b/x.wav").ok_or("missing")?;
    assert_eq!(a.digest, b.digest);
    assert_ne!(a.id, b.id);
    Ok(())
}

#[cfg(unix)]
#[test]
fn nfd_and_non_utf8_names_are_kept_as_raw_bytes() -> TestResult {
    use std::os::unix::ffi::OsStrExt;
    let h = Harness::new()?;
    let nfd = "Beyonce\u{301}.wav";
    put_wav(&h.root, nfd, 1)?;
    let raw = std::ffi::OsStr::from_bytes(b"caf\xe9.wav");
    std::fs::write(h.root.join(raw), wav_bytes(2, 800))?;
    age(&h.root.join(raw))?;
    h.scan(ScanKind::Full)?;
    assert!(h.catalog.song(h.key(), nfd.as_bytes()).is_some());
    assert!(h.catalog.song(h.key(), b"caf\xe9.wav").is_some());
    let report = h.scan(ScanKind::Full)?;
    assert_eq!((report.counts.added, report.counts.updated), (0, 0));
    Ok(())
}

#[test]
fn ignore_markers_and_hidden_files_are_skipped() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "Skip/1.wav", 1)?;
    std::fs::write(h.root.join("Skip/.nomedia"), b"")?;
    put_wav(&h.root, ".hidden/2.wav", 2)?;
    put_wav(&h.root, "Keep/3.wav", 3)?;
    h.scan(ScanKind::Full)?;
    let present: Vec<Vec<u8>> = h
        .catalog
        .present(h.key())
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(present, vec![b"Keep/3.wav".to_vec()]);
    Ok(())
}

#[test]
fn sidecar_artwork_is_stored_once_per_folder() -> TestResult {
    let h = Harness::new()?;
    for i in 0..3 {
        put_wav(&h.root, &format!("Album/{i}.wav"), i)?;
    }
    std::fs::write(h.root.join("Album/cover.jpg"), b"jpeg")?;
    h.scan(ScanKind::Full)?;
    assert!(
        h.catalog
            .present(h.key())
            .iter()
            .all(|(_, s)| s.has_artwork)
    );
    let mem = h.catalog.lock();
    let row = mem
        .artwork
        .get(&ArtworkId::of_bytes(b"jpeg"))
        .ok_or("no artwork row")?;
    assert_eq!(
        (row.mime.as_str(), row.width, row.height, row.bytes),
        ("image/jpeg", 600, 600, 4)
    );
    assert_eq!(row.source_path, Some(h.root.join("Album/cover.jpg")));
    assert_eq!(mem.artwork.len(), 1);
    Ok(())
}

#[test]
fn artist_pictures_one_level_up_are_set_on_the_artist() -> TestResult {
    let h = Harness::new()?;
    put_fixture(
        &h.root,
        "Test Artist/Fixtures/03 Tone.flac",
        "tone-440.flac",
    )?;
    std::fs::write(h.root.join("Test Artist/artist.jpg"), b"face")?;
    h.scan(ScanKind::Full)?;
    let mem = h.catalog.lock();
    assert_eq!(
        mem.artist_artwork.get("Test Artist"),
        Some(&ArtworkId::of_bytes(b"face"))
    );
    assert!(mem.artwork.contains_key(&ArtworkId::of_bytes(b"face")));
    Ok(())
}

#[test]
fn a_folder_scan_marks_only_that_folder_missing() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "A/1.wav", 1)?;
    put_wav(&h.root, "A/Sub/2.wav", 2)?;
    put_wav(&h.root, "B/3.wav", 3)?;
    h.scan(ScanKind::Full)?;
    std::fs::remove_file(h.root.join("A/Sub/2.wav"))?;
    std::fs::remove_file(h.root.join("B/3.wav"))?;
    put_wav(&h.root, "A/4.wav", 4)?;
    h.reader.reset();
    let report = h.scan_folder("A")?;
    assert_eq!(report.state, ScanState::Done);
    assert_eq!((report.counts.added, report.counts.missing), (1, 1));
    assert_eq!(h.reader.reads(), 1);
    assert!(
        h.catalog
            .song(h.key(), b"A/Sub/2.wav")
            .is_some_and(|s| s.missing_since.is_some())
    );
    assert!(
        h.catalog
            .song(h.key(), b"B/3.wav")
            .is_some_and(|s| s.missing_since.is_none())
    );
    assert_eq!(h.catalog.lock().folder_finishes, vec![b"A".to_vec()]);
    assert_eq!(h.catalog.lock().mark_missing_calls, 1);
    Ok(())
}

#[test]
fn a_move_into_a_folder_scanned_first_keeps_its_id() -> TestResult {
    let h = Harness::new()?;
    put_wav(&h.root, "Z/song.wav", 1)?;
    h.scan(ScanKind::Full)?;
    let id = h.catalog.song(h.key(), b"Z/song.wav").ok_or("missing")?.id;
    std::fs::create_dir_all(h.root.join("A"))?;
    std::fs::rename(h.root.join("Z/song.wav"), h.root.join("A/song.wav"))?;
    let report = h.scan_folder("A")?;
    assert_eq!(report.counts.moved, 1, "{report:?}");
    assert_eq!(
        h.catalog.song(h.key(), b"A/song.wav").map(|s| s.id),
        Some(id)
    );
    assert_eq!(h.catalog.lock().gone_marked, 1);
    assert!(h.catalog.song(h.key(), b"Z/song.wav").is_none());
    Ok(())
}

#[test]
fn large_library_streams_in_bounded_batches() -> TestResult {
    let h = Harness::new()?;
    for i in 0..1500u32 {
        put_wav(&h.root, &format!("F{}/{i}.wav", i % 37), i)?;
    }
    let report = h.scan(ScanKind::Full)?;
    assert_eq!(report.counts.added, 1500);
    assert!(h.catalog.lock().largest_batch <= run::BATCH_FILES);
    for i in 0..20u32 {
        std::fs::rename(
            h.root.join(format!("F{}/{i}.wav", i % 37)),
            h.root.join(format!("F{}/m{i}.wav", (i + 1) % 37)),
        )?;
    }
    let report = h.scan(ScanKind::Quick)?;
    assert_eq!(report.counts.moved, 20);
    assert!(h.catalog.lock().largest_batch <= run::BATCH_FILES);
    Ok(())
}

#[test]
fn cancelled_scan_stops_and_marks_nothing_missing() -> TestResult {
    let h = Harness::new()?;
    for i in 0..30 {
        put_wav(&h.root, &format!("{i:02}.wav"), i)?;
    }
    h.scan(ScanKind::Full)?;
    for i in 0..10 {
        std::fs::remove_file(h.root.join(format!("{i:02}.wav")))?;
    }
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    h.reader.on(
        "15.wav",
        Behaviour::Before(Arc::new(move || trigger.cancel())),
    );
    for i in 10..30 {
        std::fs::write(h.root.join(format!("{i:02}.wav")), wav_bytes(100 + i, 800))?;
        age(&h.root.join(format!("{i:02}.wav")))?;
    }
    let report = h.scan_with(ScanKind::Full, false, &cancel)?;
    assert_eq!(report.state, ScanState::Aborted);
    assert_eq!(h.catalog.missing(h.key()), 0);
    assert_eq!(h.catalog.lock().mark_missing_calls, 1);
    Ok(())
}

fn uploads_config(inbox: Option<PathBuf>) -> UploadsConfig {
    let mut uploads = Config::defaults("/nonexistent").uploads;
    uploads.inbox_dir = inbox;
    uploads
}

struct Service {
    _dir: tempfile::TempDir,
    root: PathBuf,
    staging: PathBuf,
    catalog: Arc<MemCatalog>,
    reader: Arc<TestReader>,
    library: Library,
    events: EventBus,
    shutdown: CancellationToken,
    scanner: Arc<Scanner>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn service(
    config: &ScannerConfig,
    options: &ScannerOptions,
    inbox: bool,
) -> Result<Service, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("music");
    let staging = dir.path().join("staging");
    std::fs::create_dir_all(&root)?;
    std::fs::create_dir_all(&staging)?;
    let catalog = MemCatalog::new();
    let library = catalog.add_library(&root, "Music");
    catalog.lock().uploads_library = Some(library.key);
    let reader = TestReader::new();
    let services = Services {
        catalog: catalog.clone(),
        reader: reader.clone(),
        artwork: Arc::new(ArtRecorder::default()),
        clock: Arc::new(SystemClock),
    };
    let events = EventBus::default();
    let shutdown = CancellationToken::new();
    let inbox_dir = inbox.then(|| dir.path().join("inbox"));
    if let Some(inbox) = &inbox_dir {
        std::fs::create_dir_all(inbox)?;
    }
    let scanner = Scanner::with_services(
        services,
        config,
        &uploads_config(inbox_dir),
        events.clone(),
        shutdown.clone(),
        options,
    )?;
    Ok(Service {
        _dir: dir,
        root,
        staging,
        catalog,
        reader,
        library,
        events,
        shutdown,
        scanner,
    })
}

#[test]
fn import_file_returns_the_final_id_and_a_later_rescan_changes_nothing() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    put_wav(&s.root, "Artist/Album/01 Song.wav", 1)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(s.scanner.import_file(
        &s.library.id,
        Path::new("Artist/Album/01 Song.wav"),
        SongOrigin::Acquired,
    ))?;
    let ImportOutcome::Added(id) = outcome else {
        return Err(format!("expected a new song, got {outcome:?}").into());
    };
    let song = s
        .catalog
        .song(s.library.key, b"Artist/Album/01 Song.wav")
        .ok_or("not indexed")?;
    assert_eq!(song.id, id);
    assert_eq!(song.origin, SongOrigin::Acquired);

    let again = runtime.block_on(s.scanner.import_file(
        &s.library.id,
        Path::new("Artist/Album/01 Song.wav"),
        SongOrigin::Acquired,
    ))?;
    assert_eq!(again, ImportOutcome::Existing(id.clone()));

    s.reader.reset();
    s.scanner
        .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    assert_eq!(s.reader.reads(), 0);
    s.scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    let last = s
        .catalog
        .recent_runs(s.library.key, 1)?
        .pop()
        .ok_or("no run")?;
    assert_eq!(last.state, ScanState::Done);
    assert_eq!(
        (
            last.counts.added,
            last.counts.updated,
            last.counts.moved,
            last.counts.missing
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        s.catalog
            .song(s.library.key, b"Artist/Album/01 Song.wav")
            .map(|s| s.id),
        Some(id)
    );
    Ok(())
}

#[test]
fn import_file_refuses_paths_outside_the_library_and_reports_duplicates() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    put_wav(&s.root, "a.wav", 1)?;
    put_wav(&s.root, "copy.wav", 1)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let escape = runtime.block_on(s.scanner.import_file(
        &s.library.id,
        Path::new("../etc/passwd"),
        SongOrigin::Acquired,
    ));
    assert!(matches!(escape, Err(ScanControlError::Rejected(_))));
    let ImportOutcome::Added(first) = runtime.block_on(s.scanner.import_file(
        &s.library.id,
        Path::new("a.wav"),
        SongOrigin::Acquired,
    ))?
    else {
        return Err("expected added".into());
    };
    let dup = runtime.block_on(s.scanner.import_file(
        &s.library.id,
        Path::new("copy.wav"),
        SongOrigin::Acquired,
    ))?;
    assert_eq!(dup, ImportOutcome::Existing(first));
    Ok(())
}

#[test]
fn uploads_are_verified_moved_with_the_template_and_indexed() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    let staged = s.staging.join("upload.flac");
    std::fs::copy(fixture("tone-440.flac"), &staged)?;
    let row = s.catalog.create_import(&NewImport {
        user: Some(trackside_core::UserKey(7)),
        source: ImportSource::Upload,
        original_name: "upload.flac".into(),
        staged_path: Some(staged.clone()),
        expected_bytes: None,
        target_library: None,
    })?;
    let mut events = s.events.subscribe();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(s.scanner.process_upload(&row.id))?;
    let done = s.catalog.import(&row.id)?.ok_or("row gone")?;
    assert_eq!(done.state, ImportState::Imported, "{done:?}");
    assert!(!staged.exists());
    let rel = b"Test Artist/Fixtures/1-03 Tone 440.flac";
    let song = s
        .catalog
        .song(s.library.key, rel)
        .ok_or("not at the template path")?;
    assert_eq!(Some(song.id), done.song);
    assert_eq!(song.origin, SongOrigin::Upload);
    let mut saw_upload = false;
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::Upload {
            state: ImportState::Imported,
            ..
        } = &*event
        {
            saw_upload = true;
        }
    }
    assert!(saw_upload);

    let again = s.staging.join("again.flac");
    std::fs::copy(fixture("tone-440.flac"), &again)?;
    let dup = s.catalog.create_import(&NewImport {
        user: None,
        source: ImportSource::Upload,
        original_name: "again.flac".into(),
        staged_path: Some(again.clone()),
        expected_bytes: None,
        target_library: None,
    })?;
    runtime.block_on(s.scanner.process_upload(&dup.id))?;
    let dup = s.catalog.import(&dup.id)?.ok_or("row gone")?;
    assert_eq!(dup.state, ImportState::Duplicate);
    assert_eq!(dup.song, done.song);
    assert!(!again.exists());

    let script = s.staging.join("evil.mp3");
    std::fs::write(&script, b"#!/bin/sh\nrm -rf /\n")?;
    let bad = s.catalog.create_import(&NewImport {
        user: None,
        source: ImportSource::Upload,
        original_name: "evil.mp3".into(),
        staged_path: Some(script.clone()),
        expected_bytes: None,
        target_library: None,
    })?;
    runtime.block_on(s.scanner.process_upload(&bad.id))?;
    assert_eq!(
        s.catalog.import(&bad.id)?.map(|r| r.state),
        Some(ImportState::Rejected)
    );
    assert!(!script.exists());
    Ok(())
}

#[test]
fn an_upload_staged_as_part_keeps_its_audio_extension_through_a_full_scan() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    let staged = s.staging.join("0f4c1c2a9e3b4d5f8a7b6c5d4e3f2a1b.part");
    std::fs::copy(fixture("tone-440.flac"), &staged)?;
    let row = s.catalog.create_import(&NewImport {
        user: Some(trackside_core::UserKey(7)),
        source: ImportSource::Upload,
        original_name: "My Song.FLAC".into(),
        staged_path: Some(staged),
        expected_bytes: None,
        target_library: None,
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(s.scanner.process_upload(&row.id))?;
    let done = s.catalog.import(&row.id)?.ok_or("row gone")?;
    assert_eq!(done.state, ImportState::Imported, "{done:?}");
    let rel = b"Test Artist/Fixtures/1-03 Tone 440.flac";
    assert!(s.catalog.song(s.library.key, rel).is_some());

    s.scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    assert_eq!(s.catalog.missing(s.library.key), 0);
    Ok(())
}

#[test]
fn inbox_files_are_imported_into_the_uploads_library() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), true)?;
    let inbox = s.scanner.inbox_dir().ok_or("no inbox")?.clone();
    put_fixture(&inbox, "drop/tone.mp3", "tone-440.mp3")?;
    std::fs::write(inbox.join("notes.txt"), b"ignored")?;
    std::fs::write(inbox.join("fake.flac"), b"not audio")?;
    age(&inbox.join("fake.flac"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(s.scanner.poll_inbox());
    assert!(!inbox.join("drop/tone.mp3").exists());
    assert!(
        s.catalog
            .song(s.library.key, b"Test Artist/Fixtures/1-03 Tone 440.mp3")
            .is_some()
    );
    assert!(
        inbox
            .join(crate::import::REJECTED_DIR)
            .join("fake.flac")
            .exists()
    );
    assert!(inbox.join("notes.txt").exists());
    Ok(())
}

#[test]
fn overlapping_inbox_polls_import_each_file_once() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), true)?;
    let inbox = s.scanner.inbox_dir().ok_or("no inbox")?.clone();
    for i in 0..30 {
        put_wav(&inbox, &format!("drop/{i:02}.wav"), i)?;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async { tokio::join!(s.scanner.poll_inbox(), s.scanner.poll_inbox()) });
    let states: Vec<ImportState> = s.catalog.lock().imports.iter().map(|i| i.state).collect();
    assert_eq!(states.len(), 30, "{states:?}");
    assert!(
        states.iter().all(|state| *state == ImportState::Imported),
        "{states:?}"
    );
    assert_eq!(s.catalog.present(s.library.key).len(), 30);
    Ok(())
}

#[test]
fn an_unavailable_library_refuses_scans_and_recovers() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    for i in 0..3 {
        put_wav(&s.root, &format!("{i}.wav"), i)?;
    }
    s.scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    assert_eq!(s.catalog.present(s.library.key).len(), 3);

    let parked = s.root.with_file_name("parked");
    std::fs::rename(&s.root, &parked)?;
    let mut events = s.events.subscribe();
    s.scanner.probe_now();
    assert!(!s.scanner.is_available(&s.library.id));
    let refused = s
        .scanner
        .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Admin, false);
    assert!(matches!(refused, Err(ScanControlError::Unavailable)));
    assert!(matches!(
        &*events.try_recv()?,
        ServerEvent::LibraryState {
            available: false,
            ..
        }
    ));
    assert_eq!(s.catalog.missing(s.library.key), 0);

    std::fs::rename(&parked, &s.root)?;
    s.scanner.probe_now();
    assert!(s.scanner.is_available(&s.library.id));
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    let last = s
        .catalog
        .recent_runs(s.library.key, 1)?
        .pop()
        .ok_or("no run")?;
    assert_eq!(
        (last.trigger, last.state),
        (ScanTrigger::Schedule, ScanState::Done)
    );
    assert_eq!(s.catalog.missing(s.library.key), 0);
    let status = s.scanner.status();
    assert_eq!(status.len(), 1);
    assert!(status[0].available);
    Ok(())
}

#[test]
fn a_scan_that_finds_the_root_gone_marks_the_library_unavailable() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    put_wav(&s.root, "1.wav", 1)?;
    s.scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    std::fs::remove_dir_all(&s.root)?;
    s.scanner
        .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    assert!(!s.scanner.is_available(&s.library.id));
    assert_eq!(s.catalog.missing(s.library.key), 0);
    Ok(())
}

#[test]
fn scans_report_progress_and_requests_coalesce() -> TestResult {
    let s = service(&scanner_config(), &ScannerOptions::manual(), false)?;
    for i in 0..5 {
        put_wav(&s.root, &format!("{i}.wav"), i)?;
    }
    let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    s.reader.on(
        "0.wav",
        Behaviour::Before(Arc::new(move || {
            if let Ok(tx) = entered_tx.lock() {
                let _ = tx.send(());
            }
            if let Ok(rx) = release_rx.lock() {
                let _ = rx.recv();
            }
        })),
    );
    let mut events = s.events.subscribe();
    let first =
        s.scanner
            .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Admin, false)?;
    entered.recv_timeout(Duration::from_secs(10))?;
    assert!(s.scanner.status()[0].scanning.is_some());
    let queued =
        s.scanner
            .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Watch, false)?;
    let coalesced =
        s.scanner
            .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Watch, false)?;
    let full = s
        .scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    let full_again =
        s.scanner
            .request_scan(&s.library.id, ScanKind::Quick, ScanTrigger::Watch, false)?;
    assert_ne!(first, queued);
    assert_eq!(queued, coalesced);
    assert_ne!(queued, full);
    assert_eq!(full, full_again);
    drop(release);
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    let mut states = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::Scan(progress) = &*event {
            states.push(progress.state);
        }
    }
    assert!(states.contains(&ScanState::Running));
    assert!(states.contains(&ScanState::Done));
    let runs: Vec<_> = s
        .catalog
        .recent_runs(s.library.key, 3)?
        .into_iter()
        .map(|r| (r.kind, r.state))
        .collect();
    assert_eq!(
        runs,
        vec![
            (ScanRunKind::Full, ScanState::Done),
            (ScanRunKind::Quick, ScanState::Aborted),
            (ScanRunKind::Quick, ScanState::Done),
        ]
    );
    Ok(())
}

#[test]
fn watcher_event_makes_song_visible_within_eight_seconds() -> TestResult {
    let mut config = scanner_config();
    config.watch_debounce = Duration::from_millis(500);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let options = ScannerOptions {
        monitor: false,
        watch: true,
        schedule: false,
        inbox: false,
    };
    let s = runtime.block_on(async { service(&config, &options, false) })?;
    assert_eq!(
        s.scanner.status()[0].watching,
        trackside_core::control::WatchMode::Events
    );
    let mut events = s.events.subscribe();
    std::fs::create_dir_all(s.root.join("New Album"))?;
    std::fs::write(s.root.join("New Album/01 Song.wav"), wav_bytes(1, 800))?;
    let catalog = Arc::clone(&s.catalog);
    let key = s.library.key;
    let appeared = runtime.block_on(async move {
        tokio::time::timeout(Duration::from_secs(8), async move {
            loop {
                let Ok(event) = events.recv().await else {
                    continue;
                };
                if let ServerEvent::Scan(progress) = &*event
                    && progress.state == ScanState::Done
                    && catalog.song(key, b"New Album/01 Song.wav").is_some()
                {
                    return;
                }
            }
        })
        .await
    });
    assert!(
        appeared.is_ok(),
        "the song did not appear within eight seconds"
    );
    let last = s
        .catalog
        .recent_runs(s.library.key, 1)?
        .pop()
        .ok_or("no run")?;
    assert_eq!(last.trigger, ScanTrigger::Watch);
    drop(s);
    drop(runtime);
    Ok(())
}

/// A scanner that watches its library, with a short debounce and an initial full scan done.
fn watching(
    runtime: &tokio::runtime::Runtime,
    files: &[(&str, u32)],
) -> Result<Service, Box<dyn std::error::Error>> {
    let mut config = scanner_config();
    config.watch_debounce = Duration::from_millis(300);
    let options = ScannerOptions {
        monitor: false,
        watch: true,
        schedule: false,
        inbox: false,
    };
    let s = runtime.block_on(async { service(&config, &options, false) })?;
    for (rel, seed) in files {
        put_wav(&s.root, rel, *seed)?;
    }
    s.scanner
        .request_scan(&s.library.id, ScanKind::Full, ScanTrigger::Admin, false)?;
    assert!(s.scanner.wait_idle(Duration::from_secs(10)));
    assert_eq!(s.catalog.present(s.library.key).len(), files.len());
    Ok(s)
}

/// Waits up to eight seconds for the library's present songs to be exactly `want`.
fn settles_to(s: &Service, want: &[&str]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let present: Vec<Vec<u8>> = s
            .catalog
            .present(s.library.key)
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        let want: Vec<Vec<u8>> = want.iter().map(|r| r.as_bytes().to_vec()).collect();
        if present == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn watched_renames_move_songs_within_into_and_out_of_the_library() -> TestResult {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let s = watching(
        &runtime,
        &[("A/01 Song.wav", 1), ("B/keep.wav", 2), ("C/02 Song.wav", 3)],
    )?;
    let outside = s.root.with_file_name("outside");
    put_wav(&outside, "D/03 Song.wav", 4)?;
    std::fs::create_dir_all(s.root.join("E"))?;

    // A file moved between folders, a folder moved out, a folder moved in, and a folder renamed
    // in place. notify 9 reports each rename under the path the library root was watched as.
    std::fs::rename(s.root.join("A/01 Song.wav"), s.root.join("B/01 Song.wav"))?;
    std::fs::rename(s.root.join("C"), outside.join("C"))?;
    std::fs::rename(outside.join("D"), s.root.join("D"))?;
    std::fs::rename(s.root.join("E"), s.root.join("F"))?;
    assert!(
        settles_to(&s, &["B/01 Song.wav", "B/keep.wav", "D/03 Song.wav"]),
        "renames were not picked up: {:?}",
        s.catalog.present(s.library.key)
    );

    // The folders that moved in or were renamed are watched too.
    put_wav(&s.root, "D/04 Song.wav", 5)?;
    put_wav(&s.root, "F/05 Song.wav", 6)?;
    assert!(
        settles_to(
            &s,
            &[
                "B/01 Song.wav",
                "B/keep.wav",
                "D/03 Song.wav",
                "D/04 Song.wav",
                "F/05 Song.wav"
            ]
        ),
        "new files in moved folders were not picked up: {:?}",
        s.catalog.present(s.library.key)
    );
    let last = s
        .catalog
        .recent_runs(s.library.key, 1)?
        .pop()
        .ok_or("no run")?;
    assert_eq!(last.trigger, ScanTrigger::Watch);
    drop(s);
    drop(runtime);
    Ok(())
}

#[test]
fn watched_deletes_mark_songs_missing() -> TestResult {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let s = watching(
        &runtime,
        &[
            ("A/01 Song.wav", 1),
            ("A/02 Song.wav", 2),
            ("B/C/03 Song.wav", 3),
            ("D/04 Song.wav", 4),
        ],
    )?;
    std::fs::remove_file(s.root.join("A/02 Song.wav"))?;
    std::fs::remove_dir_all(s.root.join("B"))?;
    assert!(
        settles_to(&s, &["A/01 Song.wav", "D/04 Song.wav"]),
        "deletes were not picked up: {:?}",
        s.catalog.present(s.library.key)
    );
    assert_eq!(s.catalog.missing(s.library.key), 2);
    drop(s);
    drop(runtime);
    Ok(())
}

#[test]
fn libraries_are_unwatched_when_removed_disabled_or_moved() -> TestResult {
    use trackside_core::control::WatchMode;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let s = watching(&runtime, &[("A/01 Song.wav", 1)])?;
    let watched = || crate::watch::watched_roots(&s.scanner.inner);
    assert_eq!(watched(), vec![s.root.clone()]);

    // Turning watching off releases the watch; turning it on watches the root again.
    let set_watch = |on: bool| {
        if let Some(l) = s.catalog.lock().libraries.first_mut() {
            l.watch = on;
        }
    };
    set_watch(false);
    s.scanner.probe_now();
    assert_eq!(watched(), Vec::<PathBuf>::new());
    assert_eq!(s.scanner.status()[0].watching, WatchMode::Off);
    set_watch(true);
    s.scanner.probe_now();
    assert_eq!(watched(), vec![s.root.clone()]);
    assert_eq!(s.scanner.status()[0].watching, WatchMode::Events);

    // A library moved to another root watches the new root only.
    let moved = s.root.with_file_name("moved");
    put_wav(&moved, "B/02 Song.wav", 2)?;
    if let Some(l) = s.catalog.lock().libraries.first_mut() {
        l.root.clone_from(&moved);
    }
    s.scanner.probe_now();
    assert_eq!(watched(), vec![moved.clone()]);

    // A removed library is unwatched, and a change under its old root queues nothing.
    s.catalog.lock().libraries.clear();
    s.scanner.probe_now();
    assert_eq!(watched(), Vec::<PathBuf>::new());
    assert!(s.scanner.status().is_empty());
    let runs = s.catalog.lock().runs.len();
    put_wav(&moved, "B/03 Song.wav", 3)?;
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(s.catalog.lock().runs.len(), runs);
    drop(s);
    drop(runtime);
    Ok(())
}

#[test]
#[ignore = "throughput check; run with --release --ignored"]
fn throughput_10k_files() -> TestResult {
    let mut h = Harness::new()?;
    h.settings.threads = 4;
    for i in 0..10_000u32 {
        put_wav(&h.root, &format!("A{}/B{}/{i}.wav", i % 50, i % 7), i)?;
    }
    let reader: Arc<dyn FileReader> = Arc::new(crate::reader::LoftyReader);
    h.services.reader = reader;
    let started = Instant::now();
    let report = h.scan(ScanKind::Full)?;
    let rate = report.counts.added as f64 / started.elapsed().as_secs_f64();
    eprintln!("{rate:.0} files/s");
    assert_eq!(report.counts.added, 10_000);
    assert!(rate >= 1000.0, "{rate:.0} files/s");
    Ok(())
}
