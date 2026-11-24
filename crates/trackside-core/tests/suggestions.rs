//! Suggestions with no Last.fm key: similar songs and artists, top songs, picks, Get More and
//! the mixes (On Repeat, Rediscover) from the library and this server's listens alone, filtered
//! by the caller's libraries, never showing one person's listening to another, and with the
//! mixes exactly what Tracks' own engine builds from the same plays.

use std::collections::HashSet;
use std::path::Path;

use tracks_mixes::MixPlay;
use trackside_core::access::Role;
use trackside_core::ids::SongId;
use trackside_core::ingest::ScannedFile;
use trackside_core::recommend::TimeZone;
use trackside_core::records::Song;
use trackside_core::repo::enrichment::{self as store, ArtistFacts, Related};
use trackside_core::repo::listens::{self, ListenSource, NewListen};
use trackside_core::repo::{artists, songs};
use trackside_core::testing::{TestDb, scanned};
use trackside_core::{Access, ArtistId, Clock, LibraryKey, Millis, UserKey, recommend, similar};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const DAY: i64 = 86_400_000;
const MINUTE: i64 = 60_000;

/// Four artists of two genres in library A, one song in library B that only the admin sees.
const CATALOG: [(&str, &str, i32, &str, [&str; 3]); 4] = [
    (
        "Radiohead",
        "OK Computer",
        1997,
        "Rock",
        ["Airbag", "Paranoid Android", "Karma Police"],
    ),
    (
        "Blur",
        "Blur",
        1997,
        "Rock",
        ["Beetlebum", "Song 2", "M.O.R."],
    ),
    (
        "Portishead",
        "Dummy",
        1994,
        "Trip Hop",
        ["Roads", "Glory Box", "Sour Times"],
    ),
    (
        "Massive Attack",
        "Mezzanine",
        1998,
        "Trip Hop",
        ["Teardrop", "Angel", "Exchange"],
    ),
];

struct World {
    test: TestDb,
    ana: Access,
    bob: Access,
    carl: Access,
    admin: Access,
    /// Library A's songs by title.
    songs: Vec<(String, SongId)>,
    hidden: SongId,
}

fn files(genres: bool) -> Vec<ScannedFile> {
    let mut out = Vec::new();
    for (artist, album, year, genre, titles) in CATALOG {
        for (track, title) in (1..).zip(titles) {
            let mut file = scanned(
                &format!("{artist}/{title}.flac"),
                title,
                artist,
                Some(album),
            );
            file.tags.year = Some(year);
            file.tags.track = Some(track);
            if genres {
                file.tags.genres = vec![genre.to_owned()];
            }
            out.push(file);
        }
    }
    out
}

fn world(genres: bool) -> Result<World, Box<dyn std::error::Error>> {
    let test = TestDb::new()?;
    let a = test.add_library("A", Path::new("/a"))?;
    let b = test.add_library("B", Path::new("/b"))?;
    let library = files(genres);
    let found = test.ingest(a.key, &library)?;
    let mut hidden = scanned(
        "x.flac",
        "Unfinished Sympathy",
        "Massive Attack",
        Some("Blue Lines"),
    );
    hidden.tags.year = Some(1991);
    if genres {
        hidden.tags.genres = vec![String::from("Trip Hop")];
    }
    let hidden = test.ingest(b.key, &[hidden])?[0].song().clone();
    let user = |name: &str,
                role: Role,
                libs: &[LibraryKey]|
     -> Result<Access, Box<dyn std::error::Error>> {
        let user = test.add_user(name, role, libs)?;
        Ok(test.access(user.key)?)
    };
    Ok(World {
        ana: user("ana", Role::Member, &[a.key])?,
        bob: user("bob", Role::Member, &[a.key])?,
        carl: user("carl", Role::Member, &[a.key])?,
        admin: user("luke", Role::Admin, &[])?,
        songs: library
            .iter()
            .zip(&found)
            .map(|(file, r)| (file.tags.title.clone(), r.song().clone()))
            .collect(),
        hidden,
        test,
    })
}

impl World {
    fn id(&self, title: &str) -> SongId {
        self.songs
            .iter()
            .find(|(t, _)| t == title)
            .map_or_else(|| panic!("no song {title}"), |(_, id)| id.clone())
    }

    fn now(&self) -> Millis {
        self.test.clock.now()
    }

    /// Listens by `who`, each `(title, minutes ago)`, naming the library song unless
    /// `by_name`.
    fn listen(&self, who: &Access, plays: &[(&str, i64)], by_name: bool) -> TestResult {
        let items = self.listens(who, plays, by_name);
        let user: UserKey = who.user;
        self.test
            .db
            .write_blocking(move |tx| listens::submit(tx, user, None, &items))?;
        Ok(())
    }

    fn listens(&self, who: &Access, plays: &[(&str, i64)], by_name: bool) -> Vec<NewListen> {
        let now = self.now();
        let user: UserKey = who.user;
        plays
            .iter()
            .enumerate()
            .map(|(i, (title, ago))| {
                let artist = CATALOG
                    .iter()
                    .find(|c| c.4.contains(title))
                    .map_or("Massive Attack", |c| c.0);
                NewListen {
                    client_listen_id: format!("{user:?}-{}-{i}-{title}", now.0 - ago * MINUTE),
                    song: (!by_name).then(|| self.id(title)),
                    title: (*title).to_owned(),
                    artist: artist.to_owned(),
                    album: None,
                    started_at: Millis(now.0 - ago * MINUTE),
                    played_ms: None,
                    duration_ms: None,
                    source: ListenSource::Native,
                }
            })
            .collect()
    }

    fn artist(&self, name: &str) -> Result<ArtistId, Box<dyn std::error::Error>> {
        let (admin, name) = (self.admin.clone(), name.to_owned());
        Ok(self
            .test
            .db
            .read_blocking(move |c| artists::by_name(c, &admin, &name))?
            .ok_or("no artist")?
            .id)
    }

    fn similar_artist_names(
        &self,
        who: &Access,
        artist: &str,
        in_library: bool,
        count: u32,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let (who, id) = (who.clone(), self.artist(artist)?);
        let found = self.test.db.read_blocking(move |c| {
            if in_library {
                similar::similar_artists_in_library(c, &who, &id, count)
            } else {
                similar::similar_artists(c, &who, &id, count)
            }
        })?;
        Ok(found.into_iter().map(|s| s.name).collect())
    }
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|s| s.title.as_str()).collect()
}

#[test]
fn a_new_server_without_last_fm_suggests_from_the_library() -> TestResult {
    let w = world(true)?;
    let (ana, seed) = (w.ana.clone(), w.id("Paranoid Android"));
    let similar = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &ana, &seed, 20))?;
    let found = titles(&similar);
    assert_eq!(&found[..2], ["Airbag", "Karma Police"], "{found:?}");
    assert!(
        found.contains(&"Song 2"),
        "same genre and decade: {found:?}"
    );
    assert!(!found.contains(&"Unfinished Sympathy"));

    assert_eq!(
        w.similar_artist_names(&w.ana, "Portishead", false, 15)?,
        ["Massive Attack"]
    );
    let (ana, radiohead) = (w.ana.clone(), w.artist("Radiohead")?);
    let top = w
        .test
        .db
        .read_blocking(move |c| songs::top_for_artist(c, &ana, &radiohead, 20))?;
    assert_eq!(titles(&top), ["Airbag", "Paranoid Android", "Karma Police"]);

    let (ana, now) = (w.ana.clone(), w.now());
    let (on_repeat, top, picks) = w.test.db.read_blocking(move |c| {
        Ok((
            recommend::mix(c, &ana, now, &TimeZone::UTC, "onRepeat")?,
            recommend::top_songs(c, &ana, 40)?,
            recommend::picks(c, &ana, now, &TimeZone::UTC, 40)?,
        ))
    })?;
    assert!(on_repeat.is_empty(), "nothing played yet");
    assert!(top.is_empty(), "nothing played yet");
    assert!(!picks.seeds.is_empty(), "picks start from the library");
    assert!(!picks.songs.is_empty(), "picks on a new server");
    assert!(!titles(&picks.songs).contains(&"Unfinished Sympathy"));
    Ok(())
}

#[test]
fn similar_artists_come_from_listening_together_without_genres() -> TestResult {
    let w = world(false)?;
    assert!(
        w.similar_artist_names(&w.ana, "Radiohead", false, 15)?
            .is_empty(),
        "no genres, credits or listens"
    );
    w.listen(
        &w.bob,
        &[("Airbag", 60), ("Roads", 56), ("Karma Police", 52)],
        false,
    )?;
    w.listen(
        &w.carl,
        &[("Teardrop", 300), ("Paranoid Android", 296)],
        false,
    )?;
    let found = w.similar_artist_names(&w.ana, "Radiohead", false, 15)?;
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0], "Portishead", "listened together most");
    assert!(found.contains(&String::from("Massive Attack")));
    Ok(())
}

#[test]
fn in_library_artists_are_not_crowded_out_by_last_fm() -> TestResult {
    let w = world(true)?;
    let elsewhere: Vec<Related> = (0..10)
        .map(|i| Related {
            title: String::new(),
            artist: format!("Nobody Here {i}"),
            score: 0.9,
        })
        .collect();
    w.test.db.write_blocking(move |tx| {
        store::store_artist(
            tx,
            "portishead",
            "Portishead",
            &ArtistFacts {
                similar: elsewhere,
                top: Vec::new(),
                bio: None,
            },
            30 * DAY,
        )
    })?;
    let all = w.similar_artist_names(&w.ana, "Portishead", false, 8)?;
    assert!(!all.contains(&String::from("Massive Attack")), "{all:?}");
    assert_eq!(
        w.similar_artist_names(&w.ana, "Portishead", true, 8)?,
        ["Massive Attack"]
    );
    Ok(())
}

/// What `tracks_mixes` builds from these listens on its own, as the apps call it: the song keys
/// of the mix with this id, in order.
fn engine_mix(listened: &[NewListen], now: Millis, id: &str) -> Vec<String> {
    let plays: Vec<MixPlay> = listened
        .iter()
        .map(|l| MixPlay {
            identity: tracks_model::song_key(&l.title, &l.artist),
            catalog_id: String::new(),
            title: l.title.clone(),
            artist_name: l.artist.clone(),
            album_title: None,
            artwork_url: None,
            played_at_ms: l.started_at.0,
            is_radio: false,
        })
        .collect();
    let output =
        tracks_mixes::build_mixes(&plays, &HashSet::<String>::new(), now.0, &TimeZone::UTC);
    output
        .mixes
        .iter()
        .find(|m| m.kind.id() == id)
        .map(|m| m.songs.iter().map(|s| s.identity.clone()).collect())
        .unwrap_or_default()
}

fn keys(songs: &[Song]) -> Vec<String> {
    songs.iter().map(|s| s.song_key.clone()).collect()
}

const ON_REPEAT: [&str; 7] = [
    "Roads",
    "Glory Box",
    "Song 2",
    "Airbag",
    "Teardrop",
    "Beetlebum",
    "Angel",
];

#[test]
fn on_repeat_is_the_engines_mix_of_the_callers_own_listens() -> TestResult {
    let w = world(true)?;
    let mut plays = Vec::new();
    for (i, title) in (0_i64..).zip(ON_REPEAT) {
        for day in 0..3 {
            plays.push((title, (i + day * 5) * 24 * 60 + 30));
        }
    }
    let (named, by_name) = plays.split_at(plays.len() - 3);
    let mut listened = w.listens(&w.ana, named, false);
    // Angel sent by name only, as a play of a copy on the phone would be.
    listened.extend(w.listens(&w.ana, by_name, true));
    w.listen(&w.ana, named, false)?;
    w.listen(&w.ana, by_name, true)?;
    w.listen(
        &w.bob,
        &[("Airbag", 5), ("Airbag", 6), ("Airbag", 7)],
        false,
    )?;

    let (ana, bob, now) = (w.ana.clone(), w.bob.clone(), w.now());
    let (mine, theirs) = w.test.db.read_blocking(move |c| {
        Ok((
            recommend::mix(c, &ana, now, &TimeZone::UTC, "onRepeat")?,
            recommend::mix(c, &bob, now, &TimeZone::UTC, "onRepeat")?,
        ))
    })?;
    let expected = engine_mix(&listened, now, "onRepeat");
    assert_eq!(expected.len(), ON_REPEAT.len());
    assert_eq!(
        keys(&mine),
        expected,
        "the engine's songs in the engine's order"
    );
    let angel = mine.iter().find(|s| s.title == "Angel").ok_or("no Angel")?;
    assert_eq!(angel.id, w.id("Angel"), "found by song key");
    assert!(
        theirs.is_empty(),
        "one song is too few for the engine's mix"
    );
    Ok(())
}

#[test]
fn a_mix_leaves_out_songs_the_caller_cannot_see() -> TestResult {
    let w = world(true)?;
    let mut plays = Vec::new();
    for title in ON_REPEAT.iter().take(5).chain(&["Unfinished Sympathy"]) {
        for day in 0..3 {
            plays.push((*title, day * 24 * 60 + 30));
        }
    }
    w.listen(&w.ana, &plays, true)?;
    let (ana, now) = (w.ana.clone(), w.now());
    let mine = w
        .test
        .db
        .read_blocking(move |c| recommend::mix(c, &ana, now, &TimeZone::UTC, "onRepeat"))?;
    assert_eq!(mine.len(), 5, "{:?}", titles(&mine));
    assert!(!titles(&mine).contains(&"Unfinished Sympathy"));
    Ok(())
}

#[test]
fn rediscover_is_the_engines_old_favourites() -> TestResult {
    let w = world(true)?;
    let mut plays = Vec::new();
    for (i, title) in (0_i64..).zip(ON_REPEAT.iter().take(6)) {
        for n in 0..4 {
            plays.push((*title, (100 + i * 3 + n) * 24 * 60));
        }
    }
    plays.push(("Exchange", 60));
    let listened = w.listens(&w.ana, &plays, false);
    w.listen(&w.ana, &plays, false)?;
    let (ana, now) = (w.ana.clone(), w.now());
    let found = w
        .test
        .db
        .read_blocking(move |c| recommend::mix(c, &ana, now, &TimeZone::UTC, "rediscover"))?;
    let expected = engine_mix(&listened, now, "rediscover");
    assert_eq!(expected.len(), 6);
    assert_eq!(keys(&found), expected);
    assert!(!titles(&found).contains(&"Exchange"));
    Ok(())
}

#[test]
fn get_more_ranks_unheard_songs_by_what_you_play() -> TestResult {
    let w = world(true)?;
    w.listen(
        &w.ana,
        &[("Airbag", 10), ("Airbag", 20), ("Paranoid Android", 30)],
        false,
    )?;
    w.listen(&w.bob, &[("Song 2", 5)], false)?;
    let (ana, now) = (w.ana.clone(), w.now());
    let more = w
        .test
        .db
        .read_blocking(move |c| recommend::get_more(c, &ana, now, 24, true))?;
    let found = titles(&more.songs);
    assert_eq!(found.first(), Some(&"Karma Police"), "{found:?}");
    assert!(
        found.contains(&"Song 2"),
        "an artist like Radiohead: {found:?}"
    );
    assert!(!found.contains(&"Airbag") && !found.contains(&"Paranoid Android"));
    assert!(more.external.is_empty(), "no Last.fm data stored");

    let (bob, now) = (w.carl.clone(), w.now());
    let none = w
        .test
        .db
        .read_blocking(move |c| recommend::get_more(c, &bob, now, 24, true))?;
    assert!(none.songs.is_empty(), "nothing played, nothing to go on");
    Ok(())
}

#[test]
fn top_songs_show_other_people_only_in_aggregate() -> TestResult {
    let w = world(true)?;
    w.listen(
        &w.bob,
        &[("Airbag", 5), ("Airbag", 6), ("Airbag", 7)],
        false,
    )?;
    w.listen(&w.ana, &[("Roads", 5)], false)?;
    let top = |who: &Access| -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let who = who.clone();
        let found = w
            .test
            .db
            .read_blocking(move |c| recommend::top_songs(c, &who, 40))?;
        Ok(found.into_iter().map(|s| s.title).collect())
    };
    assert_eq!(top(&w.ana)?, ["Roads"], "one other listener stays private");
    assert_eq!(top(&w.bob)?, ["Airbag"]);
    w.listen(&w.carl, &[("Airbag", 3)], false)?;
    assert_eq!(
        top(&w.ana)?,
        ["Airbag", "Roads"],
        "two others are an aggregate"
    );
    Ok(())
}

#[test]
fn picks_are_like_what_you_play_and_leave_it_out() -> TestResult {
    let w = world(true)?;
    w.listen(
        &w.ana,
        &[("Roads", 10), ("Roads", 20), ("Glory Box", 40 * 24 * 60)],
        false,
    )?;
    let (ana, now) = (w.ana.clone(), w.now());
    let picks = w
        .test
        .db
        .read_blocking(move |c| recommend::picks(c, &ana, now, &TimeZone::UTC, 40))?;
    assert_eq!(titles(&picks.seeds), ["Roads"]);
    let found = titles(&picks.songs);
    assert!(!found.contains(&"Roads"), "played lately: {found:?}");
    assert!(found.contains(&"Glory Box"), "played long ago: {found:?}");
    assert!(found.contains(&"Teardrop"), "same genre: {found:?}");
    assert!(!found.contains(&"Unfinished Sympathy"), "another library");

    let (admin, now) = (w.admin.clone(), w.now());
    w.listen(&w.admin, &[("Teardrop", 10)], false)?;
    let picks = w
        .test
        .db
        .read_blocking(move |c| recommend::picks(c, &admin, now, &TimeZone::UTC, 40))?;
    assert!(titles(&picks.songs).contains(&"Unfinished Sympathy"));
    assert_ne!(w.hidden, w.id("Teardrop"));
    Ok(())
}
