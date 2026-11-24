//! Ingest and the id rules of architecture section 6.

use std::time::Duration;

use trackside_core::access::Role;
use trackside_core::ids::{AudioDigest, LibraryKey, PublicId, SongId};
use trackside_core::ingest::{Resolution, ScannedFile};
use trackside_core::records::{ItemKind, ItemRef, Song};
use trackside_core::repo::{albums, artists, playlists, songs, stars};
use trackside_core::testing::{TestDb, digest, scanned};
use trackside_core::{Access, Change, ChangeKind, Clock, DbError, Millis};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn song(test: &TestDb, id: &SongId) -> Result<Song, DbError> {
    let id = id.clone();
    test.read_blocking(move |c| songs::get(c, &Access::system(), &id))?
        .ok_or(DbError::NotFound)
}

fn only(resolutions: &[Resolution]) -> &Resolution {
    &resolutions[0]
}

/// Changes recorded since `seq`, as (kind, entity).
fn changes_since(test: &TestDb, seq: i64) -> Result<Vec<(String, String)>, DbError> {
    test.read_blocking(move |c| {
        let mut statement =
            c.prepare("SELECT kind, entity_pid FROM change_log WHERE seq > ?1 ORDER BY seq")?;
        let rows = statement.query_map([seq], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    })
}

fn latest_seq(test: &TestDb) -> Result<i64, DbError> {
    test.read_blocking(|c| Ok(trackside_core::sync::latest_seq(c)?.0))
}

fn library(test: &TestDb, dir: &tempfile::TempDir) -> Result<LibraryKey, DbError> {
    Ok(test.add_library("Music", dir.path())?.key)
}

#[test]
fn new_file_gets_the_id_derived_from_its_digest() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let file = scanned(
        "Beyoncé/Halo.flac",
        "Halo",
        "Beyoncé",
        Some("I Am... Sasha Fierce"),
    );
    let added = test.ingest(lib, std::slice::from_ref(&file))?;
    assert_eq!(
        only(&added),
        &Resolution::Added(SongId::derive(&file.digest))
    );
    let stored = song(&test, added[0].song())?;
    assert_eq!(stored.song_key, "halo\u{1F}beyonce");
    assert_eq!(stored.credits.len(), 1);
    Ok(())
}

#[test]
fn rescan_of_an_unchanged_file_writes_nothing() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let file = scanned("a.flac", "A", "X", Some("Album"));
    test.ingest(lib, std::slice::from_ref(&file))?;
    let before = latest_seq(&test)?;
    let again = test.ingest(lib, std::slice::from_ref(&file))?;
    assert!(matches!(only(&again), Resolution::Unchanged(_)));
    assert_eq!(latest_seq(&test)?, before);
    Ok(())
}

#[test]
fn retagged_file_keeps_its_id() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let mut file = scanned("a.flac", "Helo", "X", Some("Album"));
    let id = test.ingest(lib, std::slice::from_ref(&file))?[0]
        .song()
        .clone();
    file.tags.title = String::from("Hello");
    file.mtime_ns += 1;
    let again = test.ingest(lib, &[file])?;
    assert_eq!(only(&again), &Resolution::Updated(id.clone()));
    let stored = song(&test, &id)?;
    assert_eq!(stored.title, "Hello");
    assert_eq!(stored.content_version, 1);
    Ok(())
}

#[test]
fn moved_file_keeps_its_id() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let original = scanned("Old/x.flac", "X", "Y", Some("Z"));
    let id = test.ingest(lib, std::slice::from_ref(&original))?[0]
        .song()
        .clone();
    let mut moved = scanned("New/x.flac", "X", "Y", Some("Z"));
    moved.digest = original.digest;
    let again = test.ingest(lib, &[moved])?;
    assert_eq!(only(&again), &Resolution::Moved(id.clone()));
    let outcome = test.finish_scan(lib, &["New/x.flac"])?;
    assert_eq!(outcome.marked_missing, 0);
    let stored = song(&test, &id)?;
    assert!(!stored.missing);
    let file = test
        .read_blocking(move |c| songs::file(c, &Access::system(), &id))?
        .ok_or(DbError::NotFound)?;
    assert!(file.path.ends_with("New/x.flac"));
    Ok(())
}

#[test]
fn missing_song_found_again_at_a_new_path_is_a_move() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let original = scanned("a.flac", "A", "B", None);
    let id = test.ingest(lib, std::slice::from_ref(&original))?[0]
        .song()
        .clone();
    test.finish_scan(lib, &[])?;
    assert!(song(&test, &id)?.missing);
    let mut back = scanned("elsewhere/a.flac", "A", "B", None);
    back.digest = original.digest;
    assert_eq!(
        only(&test.ingest(lib, &[back])?),
        &Resolution::Moved(id.clone())
    );
    assert!(!song(&test, &id)?.missing);
    Ok(())
}

#[test]
fn case_only_rename_keeps_its_id() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let original = scanned("Album/Song.flac", "Song", "Artist", Some("Album"));
    let id = test.ingest(lib, std::slice::from_ref(&original))?[0]
        .song()
        .clone();
    let mut renamed = scanned("Album/song.flac", "Song", "Artist", Some("Album"));
    renamed.digest = original.digest;
    assert_eq!(
        only(&test.ingest(lib, &[renamed])?),
        &Resolution::Moved(id.clone())
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn file_reachable_under_its_old_name_is_the_same_file_not_a_duplicate() -> TestResult {
    use std::os::unix::fs::MetadataExt;
    // A case-insensitive filesystem answers for the old spelling with the same inode; a hard
    // link reproduces that here.
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    std::fs::write(dir.path().join("Song.flac"), b"audio")?;
    std::fs::hard_link(dir.path().join("Song.flac"), dir.path().join("song.flac"))?;
    let meta = std::fs::metadata(dir.path().join("song.flac"))?;
    let original = scanned("Song.flac", "Song", "Artist", None);
    let id = test.ingest(lib, std::slice::from_ref(&original))?[0]
        .song()
        .clone();
    let mut renamed = scanned("song.flac", "Song", "Artist", None);
    renamed.digest = original.digest;
    renamed.dev = Some(meta.dev());
    renamed.ino = Some(meta.ino());
    assert_eq!(only(&test.ingest(lib, &[renamed])?), &Resolution::Moved(id));
    Ok(())
}

#[test]
fn file_moved_between_libraries_keeps_its_id() -> TestResult {
    let test = TestDb::new()?;
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    let one = library(&test, &first)?;
    let two = test.add_library("Other", second.path())?;
    let original = scanned("a.flac", "A", "B", Some("C"));
    let id = test.ingest(one, std::slice::from_ref(&original))?[0]
        .song()
        .clone();
    let mut moved = scanned("sub/a.flac", "A", "B", Some("C"));
    moved.digest = original.digest;
    assert_eq!(
        only(&test.ingest(two.key, &[moved])?),
        &Resolution::Moved(id.clone())
    );
    let stored = song(&test, &id)?;
    assert_eq!(stored.library, two.id);
    let member = test.add_user("m", Role::Member, &[one])?;
    let access = test.access(member.key)?;
    let hidden = test.read_blocking(move |c| songs::get(c, &access, &id))?;
    assert!(hidden.is_none());
    Ok(())
}

#[test]
fn replaced_file_with_same_mbz_track_id_keeps_its_id_and_bumps_content_version() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let mut lossy = scanned("Album/01 Song.mp3", "Song", "Artist", Some("Album"));
    lossy.tags.mbz_track = Some(String::from("1f2b-track"));
    let id = test.ingest(lib, std::slice::from_ref(&lossy))?[0]
        .song()
        .clone();
    let mut lossless = scanned("Album/01 Song.flac", "Song", "Artist", Some("Album"));
    lossless.tags.mbz_track = Some(String::from("1f2b-track"));
    assert_ne!(lossless.digest, lossy.digest);
    assert_eq!(
        only(&test.ingest(lib, &[lossless])?),
        &Resolution::Replaced(id.clone())
    );
    assert_eq!(song(&test, &id)?.content_version, 2);
    Ok(())
}

#[test]
fn replacement_without_mbz_matches_on_key_track_name_and_duration() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let mut old = scanned("Album/03 Halo.mp3", "Halo", "Beyoncé", Some("Fierce"));
    old.tags.track = Some(3);
    let id = test.ingest(lib, std::slice::from_ref(&old))?[0]
        .song()
        .clone();
    let mut new = scanned("Album/03 Halo.m4a", "Halo", "Beyonce", Some("Fierce"));
    new.tags.track = Some(3);
    new.properties.duration_ms += 600;
    assert_eq!(only(&test.ingest(lib, &[new])?), &Resolution::Replaced(id));
    let mut other = scanned(
        "Album/03 Halo (live).m4a",
        "Halo",
        "Beyonce",
        Some("Fierce"),
    );
    other.tags.track = Some(3);
    assert!(matches!(
        only(&test.ingest(lib, &[other])?),
        Resolution::Added(_)
    ));
    Ok(())
}

#[test]
fn changed_audio_at_the_same_path_bumps_content_version() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let mut file = scanned("a.flac", "A", "B", None);
    let id = test.ingest(lib, std::slice::from_ref(&file))?[0]
        .song()
        .clone();
    file.digest = digest("re-encoded");
    file.size += 10;
    assert_eq!(
        only(&test.ingest(lib, &[file])?),
        &Resolution::Updated(id.clone())
    );
    assert_eq!(song(&test, &id)?.content_version, 2);
    Ok(())
}

#[test]
fn exact_duplicate_gets_a_disambiguated_id() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let library = test.add_library("Music", dir.path())?;
    std::fs::write(dir.path().join("a.flac"), b"audio")?;
    let first = scanned("a.flac", "A", "B", None);
    let one = test.ingest(library.key, std::slice::from_ref(&first))?;
    let mut copy = scanned("copy of a.flac", "A", "B", None);
    copy.digest = first.digest;
    let two = test.ingest(library.key, &[copy])?;
    let expected = SongId::derive_disambiguated(&first.digest, &library.id, b"copy of a.flac");
    assert_eq!(only(&two), &Resolution::Added(expected));
    assert_ne!(one[0].song(), two[0].song());
    Ok(())
}

#[test]
fn two_copies_in_one_batch_get_two_ids() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let first = scanned("a.flac", "A", "B", None);
    let mut copy = scanned("b.flac", "A", "B", None);
    copy.digest = first.digest;
    let both = test.ingest(lib, &[first, copy])?;
    assert!(matches!(both[0], Resolution::Added(_)));
    assert!(matches!(both[1], Resolution::Added(_)));
    assert_ne!(both[0].song(), both[1].song());
    Ok(())
}

fn library_files() -> Vec<ScannedFile> {
    let mut files = Vec::new();
    for (i, (title, artist, album)) in [
        ("Halo", "Beyoncé", "I Am... Sasha Fierce"),
        ("Single Ladies", "Beyoncé", "I Am... Sasha Fierce"),
        (
            "Crazy in Love",
            "Beyoncé feat. JAY-Z",
            "Dangerously in Love",
        ),
        ("Don't Stop", "AC/DC", "Live"),
        ("Untitled", "Unknown", ""),
    ]
    .into_iter()
    .enumerate()
    {
        let folder = if album.is_empty() {
            artist.to_owned()
        } else {
            format!("{artist}/{album}")
        };
        let mut file = scanned(
            &format!("{folder}/{i:02} {title}.flac"),
            title,
            artist,
            Some(album).filter(|a| !a.is_empty()),
        );
        file.tags.track = Some(u32::try_from(i).unwrap_or(0) + 1);
        file.tags.genres = vec![String::from("Pop")];
        files.push(file);
    }
    files
}

#[test]
fn rebuilt_database_assigns_the_same_ids() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut runs = Vec::new();
    for _ in 0..2 {
        let test = TestDb::new()?;
        let lib = library(&test, &dir)?;
        let ids: Vec<SongId> = test
            .ingest(lib, &library_files())?
            .iter()
            .map(|r| r.song().clone())
            .collect();
        let artists = test.read_blocking(|c| {
            Ok(artists::list(c, &Access::system(), None, None, 100)?
                .items
                .into_iter()
                .map(|a| a.id)
                .collect::<Vec<_>>())
        })?;
        runs.push((ids, artists));
    }
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[0].0.len(), 5);
    Ok(())
}

#[test]
fn returning_file_after_purge_reattaches_stars_and_playlists() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let user = test.add_user("luke", Role::Member, &[lib])?;
    let file = scanned("a.flac", "A", "B", Some("C"));
    let id = test.ingest(lib, std::slice::from_ref(&file))?[0]
        .song()
        .clone();
    let item = ItemRef::Song(id.clone());
    let starred = item.clone();
    let entries = vec![id.clone()];
    let playlist = test.write_blocking(move |tx| {
        stars::set(tx, user.key, &starred, Millis(1))?;
        playlists::create(
            tx,
            user.key,
            &playlists::NewPlaylist {
                name: String::from("Mix"),
                entries,
                ..playlists::NewPlaylist::default()
            },
        )
    })?;
    test.finish_scan(lib, &[])?;
    test.clock.advance(Duration::from_hours(31 * 24));
    let cutoff = Clock::now(&*test.clock).saturating_sub(Duration::from_hours(30 * 24));
    let purged =
        test.write_blocking(move |tx| trackside_core::ingest::purge_missing(tx, cutoff))?;
    assert_eq!(purged, 1);
    let access = test.access(user.key)?;
    let visible_stars = |test: &TestDb, access: &Access| -> Result<usize, DbError> {
        let access = access.clone();
        test.read_blocking(move |c| {
            Ok(stars::list(c, &access, ItemKind::Song, None, 10)?
                .items
                .len())
        })
    };
    assert_eq!(visible_stars(&test, &access)?, 0);
    assert!(
        test.read_blocking({
            let id = id.clone();
            move |c| songs::get(c, &Access::system(), &id)
        })?
        .is_none()
    );

    let back = test.ingest(lib, &[file])?;
    assert_eq!(only(&back), &Resolution::Added(id.clone()));
    assert_eq!(visible_stars(&test, &access)?, 1);
    let pid = playlist.id.clone();
    let (_, entries) = test
        .read_blocking(move |c| playlists::get(c, &access, &pid))?
        .ok_or(DbError::NotFound)?;
    assert_eq!(entries[0].song, id);
    Ok(())
}

#[test]
fn missing_marking_spares_unreadable_folders() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let ids: Vec<SongId> = test
        .ingest(
            lib,
            &[
                scanned("locked/a.flac", "A", "X", None),
                scanned("open/b.flac", "B", "X", None),
                scanned("lockedness/c.flac", "C", "X", None),
            ],
        )?
        .iter()
        .map(|r| r.song().clone())
        .collect();
    let outcome = test.write_blocking(move |tx| {
        let run = trackside_core::ids::random_id();
        trackside_core::ingest::finish_run(
            tx,
            lib,
            &run,
            &trackside_core::ingest::SeenSet::new(),
            &[b"locked".to_vec()],
        )
    })?;
    assert_eq!(outcome.marked_missing, 2);
    assert!(!song(&test, &ids[0])?.missing);
    assert!(song(&test, &ids[1])?.missing);
    assert!(song(&test, &ids[2])?.missing);
    Ok(())
}

#[test]
fn albums_and_artists_follow_their_songs() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let results = test.ingest(lib, &library_files())?;
    let (album_list, artist_list) = test.read_blocking(|c| {
        Ok((
            albums::list(
                c,
                &Access::system(),
                albums::AlbumSort::Alphabetical,
                None,
                50,
                &albums::AlbumFilter::default(),
            )?,
            artists::list(c, &Access::system(), None, None, 50)?,
        ))
    })?;
    let fierce = album_list
        .items
        .iter()
        .find(|a| a.name == "I Am... Sasha Fierce")
        .ok_or(DbError::NotFound)?;
    assert_eq!(fierce.song_count, 2);
    assert_eq!(fierce.duration_ms, 360_000);
    assert_eq!(
        fierce.album_key.as_deref(),
        Some("i am... sasha fierce\u{1F}beyonce")
    );
    let names: Vec<&str> = artist_list.items.iter().map(|a| a.name.as_str()).collect();
    assert!(names.contains(&"JAY-Z"), "{names:?}");
    let beyonce = artist_list
        .items
        .iter()
        .find(|a| a.identity == "beyonce")
        .ok_or(DbError::NotFound)?;
    assert_eq!(beyonce.song_count, 3);
    assert_eq!(beyonce.album_count, 2);
    let blank = album_list
        .items
        .iter()
        .find(|a| a.album_key.is_none())
        .ok_or(DbError::NotFound)?;
    assert_eq!(blank.name, "Unknown");

    // Moving the only song of an album away deletes the album.
    let crazy = results[2].song().clone();
    let library_pid = test.read_blocking(move |c| {
        Ok(trackside_core::repo::libraries::by_key(c, lib)?
            .ok_or(DbError::NotFound)?
            .id)
    })?;
    let _ = library_pid;
    test.finish_scan(lib, &[])?;
    test.clock.advance(Duration::from_secs(1));
    let cutoff = Clock::now(&*test.clock);
    test.write_blocking(move |tx| trackside_core::ingest::purge_missing(tx, cutoff))?;
    let gone = test.read_blocking(move |c| songs::get(c, &Access::system(), &crazy))?;
    assert!(gone.is_none());
    let left = test.read_blocking(|c| albums::count(c, &Access::system()))?;
    assert_eq!(left, 0);
    let artists_left = test.read_blocking(|c| artists::count(c, &Access::system()))?;
    assert_eq!(artists_left, 0);
    Ok(())
}

#[test]
fn renaming_an_artist_records_upserts_for_its_songs() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let results = test.ingest(
        lib,
        &[
            scanned("1.flac", "One", "Beyonce", Some("A")),
            scanned("2.flac", "Two", "Beyonce", Some("A")),
            scanned("3.flac", "Three", "Someone feat. Beyonce", Some("B")),
        ],
    )?;
    let before = latest_seq(&test)?;
    let mut respelled = scanned("1.flac", "One", "Beyoncé", Some("A"));
    respelled.mtime_ns += 1;
    test.ingest(lib, &[respelled])?;
    let changed = changes_since(&test, before)?;
    for result in &results {
        assert!(
            changed.contains(&(String::from("song"), result.song().as_str().to_owned())),
            "{changed:?}"
        );
    }
    let artist = test
        .read_blocking(|c| artists::by_name(c, &Access::system(), "beyonce"))?
        .ok_or(DbError::NotFound)?;
    assert_eq!(artist.name, "Beyoncé");
    let third = song(&test, results[2].song())?;
    assert!(third.credits.iter().any(|c| c.name == "Beyoncé"));
    Ok(())
}

#[test]
fn renaming_an_album_records_upserts_for_its_songs() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let mut files = vec![
        scanned("1.flac", "One", "X", Some("Old Name")),
        scanned("2.flac", "Two", "X", Some("Old Name")),
    ];
    for file in &mut files {
        file.tags.mbz_release = Some(String::from("9c8d-release"));
    }
    let results = test.ingest(lib, &files)?;
    let before = latest_seq(&test)?;
    let mut renamed = files[0].clone();
    renamed.tags.album = Some(String::from("New Name"));
    renamed.mtime_ns += 1;
    test.ingest(lib, &[renamed])?;
    let changed = changes_since(&test, before)?;
    let untouched = results[1].song().as_str().to_owned();
    assert!(
        changed.contains(&(String::from("song"), untouched)),
        "{changed:?}"
    );
    assert!(changed.iter().any(|(kind, _)| kind == "album"));
    Ok(())
}

#[test]
fn library_song_count_follows_present_songs() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    test.ingest(lib, &library_files())?;
    let count = |test: &TestDb| -> Result<u64, DbError> {
        test.read_blocking(move |c| {
            Ok(trackside_core::repo::libraries::by_key(c, lib)?
                .ok_or(DbError::NotFound)?
                .song_count)
        })
    };
    assert_eq!(count(&test)?, 5);
    test.finish_scan(lib, &["Beyoncé/I Am... Sasha Fierce/00 Halo.flac"])?;
    assert_eq!(count(&test)?, 1);
    Ok(())
}

#[test]
fn known_file_reports_size_mtime_and_digest() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let file = scanned("a.flac", "A", "B", None);
    test.ingest(lib, std::slice::from_ref(&file))?;
    let known = test
        .read_blocking(move |c| trackside_core::ingest::known_file(c, lib, b"a.flac"))?
        .ok_or(DbError::NotFound)?;
    assert_eq!(known.size, file.size);
    assert_eq!(known.mtime_ns, file.mtime_ns);
    assert_eq!(known.digest, file.digest);
    let none =
        test.read_blocking(move |c| trackside_core::ingest::known_file(c, lib, b"b.flac"))?;
    assert!(none.is_none());
    Ok(())
}

#[test]
fn changes_are_recorded_per_song_with_its_library() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let id = test.ingest(lib, &[scanned("a.flac", "A", "B", None)])?[0]
        .song()
        .clone();
    let library_id: Option<i64> = test.read_blocking(move |c| {
        Ok(c.query_row(
            "SELECT library_id FROM change_log WHERE kind = 'song' AND entity_pid = ?1",
            [id.as_str()],
            |row| row.get(0),
        )?)
    })?;
    assert_eq!(library_id, Some(lib.0));
    let _ = Change::shared(
        ChangeKind::Song,
        "x",
        None,
        trackside_core::ChangeOp::Upsert,
    );
    let _ = AudioDigest([0; 32]);
    Ok(())
}

#[test]
fn paths_marked_gone_are_found_again_as_moves() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    std::fs::write(dir.path().join("a.flac"), b"still here")?;
    let original = scanned("a.flac", "A", "B", None);
    let id = test.ingest(lib, std::slice::from_ref(&original))?[0]
        .song()
        .clone();
    let marked = test.write_blocking(move |tx| {
        trackside_core::ingest::mark_gone(tx, lib, &[b"a.flac".to_vec()])
    })?;
    assert_eq!(marked, 1);
    let mut moved = scanned("z/a.flac", "A", "B", None);
    moved.digest = original.digest;
    assert_eq!(only(&test.ingest(lib, &[moved])?), &Resolution::Moved(id));
    Ok(())
}

#[test]
fn folder_scan_marks_only_that_folder() -> TestResult {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let ids: Vec<SongId> = test
        .ingest(
            lib,
            &[
                scanned("x/1.flac", "1", "A", None),
                scanned("x/sub/2.flac", "2", "A", None),
                scanned("y/3.flac", "3", "A", None),
            ],
        )?
        .iter()
        .map(|r| r.song().clone())
        .collect();
    let outcome = test.write_blocking(move |tx| {
        let run = trackside_core::ids::random_id();
        trackside_core::ingest::finish_folder(
            tx,
            lib,
            &run,
            b"x",
            false,
            &trackside_core::ingest::SeenSet::new(),
        )
    })?;
    assert_eq!(outcome.marked_missing, 1);
    assert!(song(&test, &ids[0])?.missing);
    assert!(!song(&test, &ids[1])?.missing);
    assert!(!song(&test, &ids[2])?.missing);
    Ok(())
}

#[test]
fn artwork_metadata_replaces_the_placeholder() -> TestResult {
    use trackside_core::repo::artwork::{self, ArtworkSourceKind, NewArtwork};
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    let art = trackside_core::ids::ArtworkId::of_bytes(b"jpeg");
    let mut file = scanned("a.flac", "A", "Beyoncé", Some("Album"));
    file.artwork = Some(art.clone());
    test.ingest(lib, &[file])?;
    let meta = NewArtwork {
        id: art.clone(),
        mime: String::from("image/jpeg"),
        width: 600,
        height: 600,
        bytes: 4,
        source: ArtworkSourceKind::Embedded,
        source_path: None,
    };
    let picture = art.clone();
    test.write_blocking(move |tx| {
        artwork::put(tx, &meta)?;
        artwork::set_artist_artwork(tx, "beyonce", Some(&picture))
    })?;
    let id = art.clone();
    let stored = test
        .read_blocking(move |c| artwork::get(c, &id))?
        .ok_or(DbError::NotFound)?;
    assert_eq!(
        (stored.art.width, stored.art.mime.as_str()),
        (600, "image/jpeg")
    );
    let artist = test
        .read_blocking(|c| artists::by_name(c, &Access::system(), "Beyoncé"))?
        .ok_or(DbError::NotFound)?;
    assert_eq!(artist.artwork, Some(art));
    Ok(())
}

#[test]
fn health_views_list_problems_unreadable_folders_and_duplicates() -> TestResult {
    use trackside_core::repo::health;
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = library(&test, &dir)?;
    std::fs::write(dir.path().join("a.flac"), b"x")?;
    let first = scanned("a.flac", "A", "B", None);
    let mut copy = scanned("b.flac", "A", "B", None);
    copy.digest = first.digest;
    let mut broken = scanned("c.flac", "c", "Unknown Artist", None);
    broken.tag_problem = Some(String::from("no tags"));
    test.ingest(lib, &[first])?;
    test.ingest(lib, &[copy, broken])?;
    test.write_blocking(move |tx| {
        trackside_core::repo::folders::mark_unreadable(tx, lib, b"locked", true)
    })?;
    let (problems, folders, duplicates) = test.read_blocking(move |c| {
        Ok((
            health::tag_problems(c, Some(lib), None, 10)?,
            health::unreadable_folders(c, None, None, 10)?,
            health::duplicate_groups(c, None, None, 10)?,
        ))
    })?;
    assert_eq!(problems.items.len(), 1);
    assert_eq!(problems.items[0].problem, "no tags");
    assert_eq!(folders.items.len(), 1);
    assert_eq!(duplicates.items.len(), 1);
    assert_eq!(duplicates.items[0].songs.len(), 2);
    Ok(())
}

#[test]
fn artwork_source_round_trips_for_regeneration() -> TestResult {
    use trackside_core::repo::artwork::{self, ArtworkSourceKind, NewArtwork};
    let test = TestDb::new()?;
    let art = NewArtwork {
        id: trackside_core::ids::ArtworkId::of_bytes(b"png"),
        mime: String::from("image/png"),
        width: 1200,
        height: 1000,
        bytes: 3,
        source: ArtworkSourceKind::Sidecar,
        source_path: Some(std::path::PathBuf::from("/music/Album/cover.png")),
    };
    let stored = art.clone();
    test.write_blocking(move |tx| artwork::put(tx, &stored))?;
    let id = art.id.clone();
    let found = test
        .read_blocking(move |c| artwork::get(c, &id))?
        .ok_or(DbError::NotFound)?;
    assert_eq!(found.art, art);
    let unknown = trackside_core::ids::ArtworkId::of_bytes(b"none");
    assert!(
        test.read_blocking(move |c| artwork::get(c, &unknown))?
            .is_none()
    );
    Ok(())
}
