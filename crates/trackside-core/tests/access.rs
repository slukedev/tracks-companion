//! No repository read returns a song, album, artist or playlist outside the caller's access.

use std::collections::HashSet;

use proptest::prelude::*;
use trackside_core::access::Role;
use trackside_core::ids::{LibraryId, LibraryKey, PublicId, SongId};
use trackside_core::records::Song;
use trackside_core::repo::albums::{AlbumFilter, AlbumSort};
use trackside_core::repo::songs::SongFilter;
use trackside_core::repo::{albums, artists, folders, genres, lyrics, playlists, songs};
use trackside_core::search::{SearchKinds, SearchQuery};
use trackside_core::testing::{TestDb, scanned};
use trackside_core::{Access, DbError};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct World {
    test: TestDb,
    libraries: Vec<(LibraryKey, LibraryId)>,
    /// Every song and the library it is in.
    songs: Vec<(SongId, LibraryId)>,
    _dirs: Vec<tempfile::TempDir>,
}

const ARTISTS: [&str; 4] = ["Beyoncé", "AC/DC", "Mara Solís", "Beyoncé feat. Mara Solís"];

fn build(
    placement: &[(usize, usize, usize)],
    library_count: usize,
) -> Result<World, Box<dyn std::error::Error>> {
    let test = TestDb::new()?;
    let mut libraries = Vec::new();
    let mut dirs = Vec::new();
    for i in 0..library_count {
        let dir = tempfile::tempdir()?;
        let library = test.add_library(&format!("Library {i}"), dir.path())?;
        libraries.push((library.key, library.id));
        dirs.push(dir);
    }
    let mut songs = Vec::new();
    for (n, (lib, artist, album)) in placement.iter().enumerate() {
        let (key, id) = &libraries[lib % library_count];
        let artist = ARTISTS[artist % ARTISTS.len()];
        let album = format!("Album {}", album % 3);
        let mut file = scanned(
            &format!("{album}/{n}.flac"),
            &format!("Song {n}"),
            artist,
            Some(&album),
        );
        file.tags.genres = vec![format!("Genre {}", n % 2)];
        let result = test.ingest(*key, &[file])?;
        songs.push((result[0].song().clone(), id.clone()));
    }
    Ok(World {
        test,
        libraries,
        songs,
        _dirs: dirs,
    })
}

fn check_songs(found: &[Song], allowed: &HashSet<LibraryId>) -> Result<(), String> {
    for song in found {
        if !allowed.contains(&song.library) {
            return Err(format!(
                "song {} from hidden library {}",
                song.id, song.library
            ));
        }
    }
    Ok(())
}

fn push(problems: &mut Vec<String>, result: Result<(), String>) {
    if let Err(e) = result {
        problems.push(e);
    }
}

#[expect(clippy::too_many_lines, reason = "one check per repository read")]
fn check_member(
    world: &World,
    access: &Access,
    allowed: &HashSet<LibraryId>,
) -> Result<(), Box<dyn std::error::Error>> {
    let test = &world.test;
    let all_ids: Vec<SongId> = world.songs.iter().map(|(id, _)| id.clone()).collect();
    let a = access.clone();
    let allowed_set = allowed.clone();
    let songs_in_allowed: HashSet<SongId> = world
        .songs
        .iter()
        .filter(|(_, lib)| allowed.contains(lib))
        .map(|(id, _)| id.clone())
        .collect();
    let report = test.read_blocking(move |c| {
        let mut problems: Vec<String> = Vec::new();
        for id in &all_ids {
            let one = songs::get(c, &a, id)?;
            push(&mut problems, check_songs(one.as_slice(), &allowed_set));
            if let Some(file) = songs::file(c, &a, id)? {
                push(&mut problems, check_songs(&[], &allowed_set));
                let _ = file;
            }
            for lyric in lyrics::for_song(c, &a, id)? {
                let _ = lyric;
            }
        }
        let many = songs::get_many(c, &a, &all_ids)?;
        push(&mut problems, check_songs(&many, &allowed_set));
        let visible: HashSet<SongId> = many.iter().map(|s| s.id.clone()).collect();
        if visible.len() != songs_in_allowed.len() {
            problems.push(format!(
                "get_many saw {} of {} allowed songs",
                visible.len(),
                songs_in_allowed.len()
            ));
        }
        push(
            &mut problems,
            check_songs(
                &songs::random(c, &a, &SongFilter::default(), 50)?,
                &allowed_set,
            ),
        );
        push(
            &mut problems,
            check_songs(&songs::all_paged(c, &a, None, 500)?.0, &allowed_set),
        );
        for sort in [
            AlbumSort::Newest,
            AlbumSort::Alphabetical,
            AlbumSort::AlphabeticalByArtist,
            AlbumSort::Year,
            AlbumSort::Random { seed: 3 },
        ] {
            for album in albums::list(c, &a, sort, None, 500, &AlbumFilter::default())?.items {
                if !allowed_set.contains(&album.library) {
                    problems.push(format!("album {} from a hidden library", album.id));
                }
                push(
                    &mut problems,
                    check_songs(&songs::by_album(c, &a, &album.id)?, &allowed_set),
                );
            }
        }
        for artist in artists::list(c, &Access::system(), None, None, 500)?.items {
            let seen = artists::get(c, &a, &artist.id)?;
            let albums_of = albums::by_artist(c, &a, &artist.id)?;
            let appears = albums::appears_on(c, &a, &artist.id)?;
            for album in albums_of.iter().chain(&appears) {
                if !allowed_set.contains(&album.library) {
                    problems.push(format!("album {} via artist", album.id));
                }
            }
            push(
                &mut problems,
                check_songs(&songs::top_for_artist(c, &a, &artist.id, 50)?, &allowed_set),
            );
            if let Some(seen) = seen
                && seen.song_count == 0
            {
                problems.push(format!("artist {} visible with no visible songs", seen.id));
            }
            if let Ok(similar) = trackside_core::similar::similar_artists(c, &a, &artist.id, 20) {
                for found in similar {
                    if let Some(found) = found.artist
                        && found.song_count == 0
                    {
                        problems.push(format!("similar artist {} from a hidden library", found.id));
                    }
                }
            }
        }
        for artist in artists::list(c, &a, None, None, 500)?.items {
            if artist.song_count == 0 {
                problems.push(format!("listed artist {} has no visible songs", artist.id));
            }
        }
        for id in &all_ids {
            if let Ok(similar) = trackside_core::similar::similar_songs(c, &a, id, 50) {
                push(&mut problems, check_songs(&similar, &allowed_set));
            }
        }
        let results = trackside_core::search::search(
            c,
            &a,
            &SearchQuery {
                text: String::from("song"),
                kinds: SearchKinds::ALL,
                limit: 500,
                cursor: None,
                library: None,
            },
        )?;
        push(
            &mut problems,
            check_songs(&results.songs.items, &allowed_set),
        );
        for album in &results.albums.items {
            if !allowed_set.contains(&album.library) {
                problems.push(format!("search album {} from a hidden library", album.id));
            }
        }
        for song in songs::by_key(c, &a, "song 0\u{1F}beyonce")? {
            push(&mut problems, check_songs(&[song], &allowed_set));
        }
        let _ = genres::list(c, &a)?;
        for playlist in playlists::list(c, &a)? {
            if playlist.owner != a.user && !playlist.is_public {
                problems.push(format!("private playlist {} of another user", playlist.id));
            }
        }
        let _ = folders::root(c, &a, LibraryKey(1))?;
        Ok::<_, DbError>(problems)
    })?;
    if report.is_empty() {
        Ok(())
    } else {
        Err(report.join("; ").into())
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    #[test]
    fn no_read_returns_anything_outside_the_callers_access(
        library_count in 1usize..4,
        placement in prop::collection::vec((0usize..4, 0usize..4, 0usize..3), 1..14),
        grants in prop::collection::vec(prop::collection::vec(any::<bool>(), 3), 1..4),
    ) {
        let world = build(&placement, library_count).map_err(|e| TestCaseError::fail(e.to_string()))?;
        for (n, grant) in grants.iter().enumerate() {
            let granted: Vec<LibraryKey> = world
                .libraries
                .iter()
                .zip(grant)
                .filter(|(_, g)| **g)
                .map(|((key, _), _)| *key)
                .collect();
            let user = world
                .test
                .add_user(&format!("member{n}"), Role::Member, &granted)
                .map_err(|e| TestCaseError::fail(e.to_string()))?;
            let owner = user.key;
            let first = world.songs[0].0.clone();
            world.test.write_blocking(move |tx| {
                playlists::create(tx, owner, &playlists::NewPlaylist {
                    name: format!("private {n}"),
                    entries: vec![first],
                    ..playlists::NewPlaylist::default()
                })
            }).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let access = world.test.access(user.key).map_err(|e| TestCaseError::fail(e.to_string()))?;
            let allowed: HashSet<LibraryId> = world
                .libraries
                .iter()
                .filter(|(key, _)| granted.contains(key))
                .map(|(_, id)| id.clone())
                .collect();
            check_member(&world, &access, &allowed).map_err(|e| TestCaseError::fail(e.to_string()))?;
        }
    }
}

#[test]
fn member_without_grants_sees_nothing() -> TestResult {
    let world = build(&[(0, 0, 0), (1, 1, 1)], 2)?;
    let user = world.test.add_user("nobody", Role::Member, &[])?;
    let access = world.test.access(user.key)?;
    let (count, libraries) = world.test.read_blocking(move |c| {
        Ok((
            songs::count(c, &access)?,
            trackside_core::repo::libraries::list(c, &access)?,
        ))
    })?;
    assert_eq!(count, 0);
    assert_eq!(libraries.len(), 0);
    let _ = world.songs[0].0.as_str();
    Ok(())
}

#[test]
fn artwork_is_visible_only_through_something_the_caller_can_see() -> TestResult {
    use trackside_core::ArtworkId;
    use trackside_core::repo::artwork;
    let test = TestDb::new()?;
    let (kids_dir, music_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let kids = test.add_library("Kids", kids_dir.path())?.key;
    let music = test.add_library("Music", music_dir.path())?.key;
    let cover = ArtworkId::of_bytes(b"music cover");
    let mut song = scanned("a.flac", "Halo", "Beyoncé", Some("Sasha"));
    song.artwork = Some(cover.clone());
    test.ingest(music, &[song])?;
    let mia = test.add_user("mia", Role::Member, &[kids])?.key;
    let luke = test.add_user("luke", Role::Member, &[music])?.key;
    let seen = |user| -> Result<bool, DbError> {
        let access = test.access(user)?;
        let cover = cover.clone();
        test.read_blocking(move |c| artwork::visible_to(c, &access, &cover))
    };
    assert!(!seen(mia)?);
    assert!(seen(luke)?);
    Ok(())
}

#[test]
fn a_public_playlist_shows_others_only_the_songs_they_can_see() -> TestResult {
    use trackside_core::repo::playlists::NewPlaylist;
    let test = TestDb::new()?;
    let (kids_dir, music_dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let kids = test.add_library("Kids", kids_dir.path())?.key;
    let music = test.add_library("Music", music_dir.path())?.key;
    let halo = test.ingest(music, &[scanned("a.flac", "Halo", "Beyoncé", None)])?[0]
        .song()
        .clone();
    let lullaby = test.ingest(kids, &[scanned("b.flac", "Lullaby", "Nursery", None)])?[0]
        .song()
        .clone();
    let luke = test.add_user("luke", Role::Member, &[music, kids])?.key;
    let mia = test.add_user("mia", Role::Member, &[kids])?.key;
    let entries = vec![halo.clone(), lullaby.clone()];
    let playlist = test.write_blocking(move |tx| {
        playlists::create(
            tx,
            luke,
            &NewPlaylist {
                name: String::from("Mixed"),
                is_public: true,
                entries,
                ..NewPlaylist::default()
            },
        )
    })?;
    let songs_for = |user| -> Result<Vec<SongId>, DbError> {
        let access = test.access(user)?;
        let id = playlist.id.clone();
        let (_, entries) = test
            .read_blocking(move |c| playlists::get(c, &access, &id))?
            .ok_or(DbError::NotFound)?;
        Ok(entries.into_iter().map(|e| e.song).collect())
    };
    assert_eq!(songs_for(mia)?, std::slice::from_ref(&lullaby));
    assert_eq!(songs_for(luke)?, [halo, lullaby]);
    Ok(())
}
