//! Delta sync: snapshots, deltas, resets and visibility.

use std::collections::BTreeMap;
use std::time::Duration;

use proptest::prelude::*;
use trackside_core::access::Role;
use trackside_core::ids::{LibraryKey, PublicId, SongId};
use trackside_core::records::{Album, Artist, ItemRef, Library, Song};
use trackside_core::repo::listens::{ListenSource, NewListen};
use trackside_core::repo::play_queue::{PlayQueue, PlayQueueUpdate};
use trackside_core::repo::{listens, play_queue, playlists, ratings, stars};
use trackside_core::sync::{
    self, PlaylistWithEntries, SnapshotItems, SyncCursor, SyncError, SyncPage,
};
use trackside_core::testing::{TestDb, scanned};
use trackside_core::{Access, Clock, DbError, Millis};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// What a client keeps from sync.
#[derive(Debug, Default, PartialEq)]
struct Mirror {
    libraries: BTreeMap<String, Library>,
    artists: BTreeMap<String, Artist>,
    albums: BTreeMap<String, Album>,
    songs: BTreeMap<String, Song>,
    playlists: BTreeMap<String, PlaylistWithEntries>,
    stars: BTreeMap<String, Millis>,
    ratings: BTreeMap<String, u8>,
    play_stats: BTreeMap<String, (u32, Millis)>,
    play_queue: Option<Vec<SongId>>,
}

fn key(item: &ItemRef) -> String {
    item.change_entity()
}

impl Mirror {
    fn apply_snapshot(&mut self, items: SnapshotItems, libraries: Vec<Library>) {
        self.libraries = libraries
            .into_iter()
            .map(|l| (l.id.to_string(), l))
            .collect();
        match items {
            SnapshotItems::Libraries(_) => {}
            SnapshotItems::Artists(list) => self
                .artists
                .extend(list.into_iter().map(|a| (a.id.to_string(), a))),
            SnapshotItems::Albums(list) => self
                .albums
                .extend(list.into_iter().map(|a| (a.id.to_string(), a))),
            SnapshotItems::Songs(list) => self
                .songs
                .extend(list.into_iter().map(|s| (s.id.to_string(), s))),
            SnapshotItems::Playlists(list) => self
                .playlists
                .extend(list.into_iter().map(|p| (p.playlist.id.to_string(), p))),
            SnapshotItems::Stars(list) => self
                .stars
                .extend(list.into_iter().map(|s| (key(&s.item), s.starred_at))),
            SnapshotItems::Ratings(list) => self
                .ratings
                .extend(list.into_iter().map(|r| (key(&r.item), r.rating))),
            SnapshotItems::PlayStats(list) => self.play_stats.extend(
                list.into_iter()
                    .map(|p| (p.song.to_string(), (p.play_count, p.last_played_at))),
            ),
            SnapshotItems::PlayQueue(queue) => self.set_queue(queue),
        }
    }

    fn set_queue(&mut self, queue: Option<PlayQueue>) {
        self.play_queue = queue.map(|q| q.entries).filter(|e| !e.is_empty());
    }

    fn apply_delta(&mut self, page: sync::DeltaPage) {
        let up = page.upserts;
        self.libraries
            .extend(up.libraries.into_iter().map(|l| (l.id.to_string(), l)));
        self.artists
            .extend(up.artists.into_iter().map(|a| (a.id.to_string(), a)));
        self.albums
            .extend(up.albums.into_iter().map(|a| (a.id.to_string(), a)));
        self.songs
            .extend(up.songs.into_iter().map(|s| (s.id.to_string(), s)));
        self.playlists.extend(
            up.playlists
                .into_iter()
                .map(|p| (p.playlist.id.to_string(), p)),
        );
        self.stars
            .extend(up.stars.into_iter().map(|s| (key(&s.item), s.starred_at)));
        self.ratings
            .extend(up.ratings.into_iter().map(|r| (key(&r.item), r.rating)));
        self.play_stats.extend(
            up.play_stats
                .into_iter()
                .map(|p| (p.song.to_string(), (p.play_count, p.last_played_at))),
        );
        if let Some(queue) = up.play_queue {
            self.set_queue(Some(queue));
        }
        let del = page.deletes;
        for id in del.libraries {
            self.libraries.remove(id.as_str());
        }
        for id in del.artists {
            self.artists.remove(id.as_str());
        }
        for id in del.albums {
            self.albums.remove(id.as_str());
        }
        for id in del.songs {
            self.songs.remove(id.as_str());
        }
        for id in del.playlists {
            self.playlists.remove(id.as_str());
        }
        for item in del.stars {
            self.stars.remove(&key(&item));
        }
        for item in del.ratings {
            self.ratings.remove(&key(&item));
        }
    }
}

fn page(
    test: &TestDb,
    access: &Access,
    cursor: Option<&SyncCursor>,
    limit: u32,
) -> Result<SyncPage, SyncError> {
    let access = access.clone();
    let cursor = cursor.cloned();
    test.read_blocking(move |c| Ok(sync::page(c, &access, cursor.as_ref(), limit)))
        .map_err(SyncError::Db)?
}

/// A full snapshot in one go; returns the mirror and the delta cursor.
fn fresh(test: &TestDb, access: &Access) -> Result<(Mirror, SyncCursor), SyncError> {
    let mut mirror = Mirror::default();
    let mut cursor = None;
    loop {
        match page(test, access, cursor.as_ref(), 5000)? {
            SyncPage::Snapshot(snapshot) => {
                mirror.apply_snapshot(snapshot.items, snapshot.libraries);
                let complete = snapshot.complete;
                cursor = Some(snapshot.next_cursor);
                if complete {
                    break;
                }
            }
            SyncPage::Delta(_) => return Err(SyncError::BadCursor),
        }
    }
    Ok((mirror, cursor.ok_or(SyncError::BadCursor)?))
}

fn catch_up(
    test: &TestDb,
    access: &Access,
    mirror: &mut Mirror,
    mut cursor: SyncCursor,
    limit: u32,
) -> Result<SyncCursor, SyncError> {
    loop {
        match page(test, access, Some(&cursor), limit)? {
            SyncPage::Delta(delta) => {
                let more = delta.has_more;
                cursor = delta.next_cursor.clone();
                mirror.apply_delta(delta);
                if !more {
                    return Ok(cursor);
                }
            }
            SyncPage::Snapshot(_) => return Err(SyncError::BadCursor),
        }
    }
}

struct World {
    test: TestDb,
    libraries: Vec<LibraryKey>,
    user: trackside_core::ids::UserKey,
    access: Access,
    next: usize,
    _dirs: Vec<tempfile::TempDir>,
}

impl World {
    fn new(visible: usize, hidden: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let test = TestDb::new()?;
        let mut libraries = Vec::new();
        let mut dirs = Vec::new();
        for i in 0..visible + hidden {
            let dir = tempfile::tempdir()?;
            libraries.push(test.add_library(&format!("L{i}"), dir.path())?.key);
            dirs.push(dir);
        }
        let user = test.add_user("member", Role::Member, &libraries[..visible])?;
        let access = test.access(user.key)?;
        Ok(Self {
            test,
            libraries,
            user: user.key,
            access,
            next: 0,
            _dirs: dirs,
        })
    }

    fn song_ids(&self) -> Result<Vec<(SongId, String)>, DbError> {
        self.test.read_blocking(|c| {
            let mut statement = c.prepare("SELECT pid, rel_path_display FROM songs ORDER BY id")?;
            let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    #[expect(clippy::too_many_lines, reason = "one arm per kind of edit")]
    fn edit(&mut self, edit: &Edit) -> Result<(), Box<dyn std::error::Error>> {
        let songs = self.song_ids()?;
        let pick = |n: usize| songs.get(n % songs.len().max(1)).cloned();
        let user = self.user;
        match edit {
            Edit::Add { library, artist } => {
                self.next += 1;
                let lib = self.libraries[library % self.libraries.len()];
                let file = scanned(
                    &format!("{}.flac", self.next),
                    &format!("Song {}", self.next),
                    ARTISTS[artist % ARTISTS.len()],
                    Some(ARTISTS[(artist + 1) % ARTISTS.len()]),
                );
                self.test.ingest(lib, &[file])?;
            }
            Edit::Retag { song, artist } => {
                if let Some((_, path)) = pick(*song) {
                    let lib = self.library_of(&path)?;
                    let mut file = scanned(
                        &path,
                        &format!("Retagged {song}"),
                        ARTISTS[artist % ARTISTS.len()],
                        Some("Other"),
                    );
                    file.mtime_ns += 1;
                    self.test.ingest(lib, &[file])?;
                }
            }
            Edit::Move { song, library } => {
                if let Some((_, path)) = pick(*song) {
                    let from = self.library_of(&path)?;
                    let to = self.libraries[library % self.libraries.len()];
                    let mut file = scanned(&format!("moved/{path}"), "Moved", "Mover", None);
                    file.digest = trackside_core::testing::digest(&path);
                    self.test.ingest(to, &[file])?;
                    let _ = from;
                }
            }
            Edit::MarkMissing { library } => {
                let lib = self.libraries[library % self.libraries.len()];
                self.test.finish_scan(lib, &[])?;
            }
            Edit::Purge => {
                self.test.clock.advance(Duration::from_secs(1));
                let now = Clock::now(&*self.test.clock);
                self.test
                    .write_blocking(move |tx| trackside_core::ingest::purge_missing(tx, now))?;
            }
            Edit::Star { song, on } => {
                if let Some((id, _)) = pick(*song) {
                    let item = ItemRef::Song(id);
                    let on = *on;
                    self.test.write_blocking(move |tx| {
                        if on {
                            stars::set(tx, user, &item, Millis(5))
                        } else {
                            stars::clear(tx, user, &item)
                        }
                    })?;
                }
            }
            Edit::Rate { song, rating } => {
                if let Some((id, _)) = pick(*song) {
                    let item = ItemRef::Song(id);
                    let rating = *rating;
                    self.test.write_blocking(move |tx| {
                        if rating == 0 {
                            ratings::clear(tx, user, &item)
                        } else {
                            ratings::set(tx, user, &item, rating)
                        }
                    })?;
                }
            }
            Edit::Listen { song } => {
                if let Some((id, _)) = pick(*song) {
                    self.next += 1;
                    let listen = NewListen {
                        client_listen_id: format!("play-{}", self.next),
                        song: Some(id),
                        title: String::from("t"),
                        artist: String::from("a"),
                        album: None,
                        started_at: Millis(1_000 + i64::try_from(self.next)?),
                        played_ms: None,
                        duration_ms: None,
                        source: ListenSource::Native,
                    };
                    self.test
                        .write_blocking(move |tx| listens::submit(tx, user, None, &[listen]))?;
                }
            }
            Edit::Playlist { song, public } => {
                if let Some((id, _)) = pick(*song) {
                    let public = *public;
                    self.test.write_blocking(move |tx| {
                        let list = playlists::list(tx, &Access::system())?;
                        match list.into_iter().find(|p| p.owner == user) {
                            Some(existing) => {
                                playlists::append(tx, user, &existing.id, &[id])?;
                                playlists::update(
                                    tx,
                                    user,
                                    &existing.id,
                                    &playlists::PlaylistPatch {
                                        is_public: Some(public),
                                        ..playlists::PlaylistPatch::default()
                                    },
                                )?;
                            }
                            None => {
                                playlists::create(
                                    tx,
                                    user,
                                    &playlists::NewPlaylist {
                                        name: String::from("Mine"),
                                        is_public: public,
                                        entries: vec![id],
                                        ..playlists::NewPlaylist::default()
                                    },
                                )?;
                            }
                        }
                        Ok(())
                    })?;
                }
            }
            Edit::Queue { song } => {
                let entries: Vec<SongId> = pick(*song).map(|(id, _)| id).into_iter().collect();
                self.test.write_blocking(move |tx| {
                    play_queue::put(
                        tx,
                        user,
                        &PlayQueueUpdate {
                            entries,
                            current: Some(0),
                            position_ms: 0,
                            changed_by: String::from("test"),
                            device: None,
                        },
                    )
                })?;
            }
        }
        Ok(())
    }

    fn library_of(&self, path: &str) -> Result<LibraryKey, DbError> {
        let path = path.to_owned();
        self.test.read_blocking(move |c| {
            Ok(c.query_row(
                "SELECT library_id FROM songs WHERE rel_path_display = ?1",
                [path],
                |row| row.get(0),
            )?)
        })
    }
}

const ARTISTS: [&str; 3] = ["Beyoncé", "AC/DC", "Beyoncé feat. JAY-Z"];

#[derive(Clone, Debug)]
enum Edit {
    Add { library: usize, artist: usize },
    Retag { song: usize, artist: usize },
    Move { song: usize, library: usize },
    MarkMissing { library: usize },
    Purge,
    Star { song: usize, on: bool },
    Rate { song: usize, rating: u8 },
    Listen { song: usize },
    Playlist { song: usize, public: bool },
    Queue { song: usize },
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        3 => (0usize..3, 0usize..3).prop_map(|(library, artist)| Edit::Add { library, artist }),
        2 => (0usize..20, 0usize..3).prop_map(|(song, artist)| Edit::Retag { song, artist }),
        1 => (0usize..20, 0usize..3).prop_map(|(song, library)| Edit::Move { song, library }),
        1 => (0usize..3).prop_map(|library| Edit::MarkMissing { library }),
        1 => Just(Edit::Purge),
        2 => (0usize..20, any::<bool>()).prop_map(|(song, on)| Edit::Star { song, on }),
        1 => (0usize..20, 0u8..6).prop_map(|(song, rating)| Edit::Rate { song, rating }),
        2 => (0usize..20).prop_map(|song| Edit::Listen { song }),
        1 => (0usize..20, any::<bool>()).prop_map(|(song, public)| Edit::Playlist { song, public }),
        1 => (0usize..20).prop_map(|song| Edit::Queue { song }),
    ]
}

fn run_scenario(
    setup: &[Edit],
    during: &[Edit],
    after: &[Edit],
    limit: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut world = World::new(2, 1)?;
    for e in setup {
        world.edit(e)?;
    }
    let access = world.access.clone();
    let mut mirror = Mirror::default();
    let mut cursor: Option<SyncCursor> = None;
    let mut during = during.iter();
    let delta_cursor = loop {
        match page(&world.test, &access, cursor.as_ref(), limit)? {
            SyncPage::Snapshot(snapshot) => {
                mirror.apply_snapshot(snapshot.items, snapshot.libraries);
                if snapshot.complete {
                    break snapshot.next_cursor;
                }
                cursor = Some(snapshot.next_cursor);
                if let Some(e) = during.next() {
                    world.edit(e)?;
                }
            }
            SyncPage::Delta(_) => return Err("delta during a snapshot".into()),
        }
    };
    for e in during {
        world.edit(e)?;
    }
    let mid = catch_up(&world.test, &access, &mut mirror, delta_cursor, limit)?;
    for e in after {
        world.edit(e)?;
    }
    catch_up(&world.test, &access, &mut mirror, mid, limit)?;
    let (expected, _) = fresh(&world.test, &access)?;
    if mirror != expected {
        return Err(format!("mirror differs:\n{mirror:#?}\nexpected:\n{expected:#?}").into());
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn snapshot_then_delta_equals_a_fresh_snapshot(
        setup in prop::collection::vec(edit(), 0..12),
        during in prop::collection::vec(edit(), 0..8),
        after in prop::collection::vec(edit(), 0..12),
        limit in 1u32..4,
    ) {
        run_scenario(&setup, &during, &after, limit).map_err(|e| TestCaseError::fail(e.to_string()))?;
    }
}

#[test]
fn cursor_below_pruned_seq_requires_reset() -> TestResult {
    let mut world = World::new(1, 0)?;
    world.edit(&Edit::Add {
        library: 0,
        artist: 0,
    })?;
    let (_, cursor) = fresh(&world.test, &world.access)?;
    world.edit(&Edit::MarkMissing { library: 0 })?;
    world.edit(&Edit::Purge)?;
    world.test.clock.advance(Duration::from_secs(91 * 86_400));
    let cutoff = Clock::now(&*world.test.clock).saturating_sub(Duration::from_secs(90 * 86_400));
    let pruned = world
        .test
        .write_blocking(move |tx| sync::prune_tombstones(tx, cutoff))?;
    assert!(pruned > 0);
    assert!(matches!(
        page(&world.test, &world.access, Some(&cursor), 100),
        Err(SyncError::Reset)
    ));
    let (_, fresh_cursor) = fresh(&world.test, &world.access)?;
    assert!(page(&world.test, &world.access, Some(&fresh_cursor), 100).is_ok());
    Ok(())
}

#[test]
fn library_grant_change_requires_reset() -> TestResult {
    let world = World::new(1, 1)?;
    let (_, cursor) = fresh(&world.test, &world.access)?;
    let user = world.user;
    let all = world.libraries.clone();
    world.test.write_blocking(move |tx| {
        trackside_core::repo::users::set_libraries(tx, user, &all, None)
    })?;
    let access = world.test.access(user)?;
    assert!(matches!(
        page(&world.test, &access, Some(&cursor), 100),
        Err(SyncError::Reset)
    ));
    Ok(())
}

#[test]
fn song_moving_to_hidden_library_arrives_as_delete() -> TestResult {
    let mut world = World::new(1, 1)?;
    world.edit(&Edit::Add {
        library: 0,
        artist: 0,
    })?;
    let (mut mirror, cursor) = fresh(&world.test, &world.access)?;
    assert_eq!(mirror.songs.len(), 1);
    let id = world.song_ids()?[0].0.clone();
    world.edit(&Edit::Move {
        song: 0,
        library: 1,
    })?;
    // A later change to the song, now only in the hidden library, must not hide the move.
    world.edit(&Edit::Retag { song: 0, artist: 1 })?;
    let delta = match page(&world.test, &world.access, Some(&cursor), 100)? {
        SyncPage::Delta(delta) => delta,
        SyncPage::Snapshot(_) => return Err("expected a delta".into()),
    };
    assert!(delta.deletes.songs.contains(&id), "{:?}", delta.deletes);
    mirror.apply_delta(delta);
    assert!(mirror.songs.is_empty());
    Ok(())
}

#[test]
fn delta_with_no_changes_is_empty_and_keeps_the_cursor() -> TestResult {
    let mut world = World::new(1, 0)?;
    world.edit(&Edit::Add {
        library: 0,
        artist: 0,
    })?;
    let (_, cursor) = fresh(&world.test, &world.access)?;
    match page(&world.test, &world.access, Some(&cursor), 100)? {
        SyncPage::Delta(delta) => {
            assert!(delta.upserts.songs.is_empty() && !delta.has_more);
            assert_eq!(delta.next_cursor, cursor);
        }
        SyncPage::Snapshot(_) => return Err("expected a delta".into()),
    }
    Ok(())
}

#[test]
fn bad_cursor_is_rejected() {
    assert!(matches!(
        SyncCursor::decode("AAAA"),
        Err(SyncError::BadCursor)
    ));
}

mod cursor_properties {
    use base64::Engine;
    use proptest::prelude::*;
    use trackside_core::sync::SyncCursor;

    proptest! {
        #[test]
        fn arbitrary_cursors_are_refused_not_panicked_on(raw in any::<String>()) {
            let _ = SyncCursor::decode(&raw);
        }

        #[test]
        fn arbitrary_json_in_base64_is_refused_not_panicked_on(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
            let _ = SyncCursor::decode(&encoded);
        }

        #[test]
        fn cursors_round_trip(epoch in any::<i64>(), seq in 0i64..i64::MAX) {
            let cursor = SyncCursor { epoch, seq: trackside_core::Seq(seq), snapshot: None };
            prop_assert_eq!(SyncCursor::decode(&cursor.encode()).ok(), Some(cursor));
        }
    }
}
