//! Stored Last.fm data merged into similar songs, similar artists and top songs, filtered by
//! what the caller may see and ranked by the caller's own stars and plays.

use std::path::Path;

use trackside_core::access::Role;
use trackside_core::ids::SongId;
use trackside_core::records::Song;
use trackside_core::repo::enrichment::{self as store, ArtistFacts, Related};
use trackside_core::repo::{artists, songs};
use trackside_core::similar;
use trackside_core::testing::{TestDb, scanned};
use trackside_core::{Access, ArtistId, Clock, UserKey};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const DAY: i64 = 86_400_000;

struct World {
    test: TestDb,
    member: Access,
    admin: Access,
    member_key: UserKey,
    creep: SongId,
}

fn related(title: &str, artist: &str, score: f64) -> Related {
    Related {
        title: title.to_owned(),
        artist: artist.to_owned(),
        score,
    }
}

/// Library A (the member's): Creep, Roads and Karma Police. Library B (admin only): Teardrop.
/// Last.fm relates Creep to Roads, Teardrop and a song nobody has.
fn world() -> Result<World, Box<dyn std::error::Error>> {
    let test = TestDb::new()?;
    let a = test.add_library("A", Path::new("/a"))?;
    let b = test.add_library("B", Path::new("/b"))?;
    let found = test.ingest(
        a.key,
        &[
            scanned("1.flac", "Creep", "Radiohead", Some("Pablo Honey")),
            scanned("2.flac", "Roads", "Portishead", Some("Dummy")),
            scanned("3.flac", "Karma Police", "Radiohead", Some("OK Computer")),
            scanned("4.flac", "Lucky", "Radiohead", Some("OK Computer")),
        ],
    )?;
    test.ingest(
        b.key,
        &[scanned(
            "5.flac",
            "Teardrop",
            "Massive Attack",
            Some("Mezzanine"),
        )],
    )?;
    let member = test.add_user("nana", Role::Member, &[a.key])?;
    let admin = test.add_user("luke", Role::Admin, &[])?;
    let creep = found[0].song().clone();
    let key = tracks_model::song_key("Creep", "Radiohead");
    test.db.write_blocking(move |tx| {
        store::store_song(
            tx,
            &key,
            &[
                related("Roads", "Portishead", 0.9),
                related("Teardrop", "Massive Attack", 0.8),
                related("Glory Box", "Nobody Here", 0.7),
            ],
            30 * DAY,
        )?;
        store::store_artist(
            tx,
            "radiohead",
            "Radiohead",
            &ArtistFacts {
                similar: vec![
                    related("", "Portishead", 0.9),
                    related("", "Massive Attack", 0.6),
                ],
                top: vec![String::from("Lucky"), String::from("No Surprises")],
                bio: Some(String::from("An English rock band.")),
            },
            30 * DAY,
        )
    })?;
    Ok(World {
        member: test.access(member.key)?,
        admin: test.access(admin.key)?,
        member_key: member.key,
        test,
        creep,
    })
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|s| s.title.as_str()).collect()
}

fn radiohead(w: &World) -> Result<ArtistId, Box<dyn std::error::Error>> {
    let a = w.admin.clone();
    Ok(w.test
        .db
        .read_blocking(move |c| artists::by_name(c, &a, "Radiohead"))?
        .ok_or("no artist")?
        .id)
}

#[test]
fn a_member_never_gets_songs_from_libraries_they_cannot_see() -> TestResult {
    let w = world()?;
    let (member, creep) = (w.member.clone(), w.creep.clone());
    let found = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &member, &creep, 20))?;
    assert!(titles(&found).contains(&"Roads"));
    assert!(
        !titles(&found).contains(&"Teardrop"),
        "{:?}",
        titles(&found)
    );

    let (admin, creep) = (w.admin.clone(), w.creep.clone());
    let found = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &admin, &creep, 20))?;
    assert!(titles(&found).contains(&"Teardrop"));
    Ok(())
}

#[test]
fn last_fm_songs_outrank_weaker_local_signals() -> TestResult {
    let w = world()?;
    let (member, creep) = (w.member.clone(), w.creep.clone());
    let found = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &member, &creep, 20))?;
    assert_eq!(titles(&found).first(), Some(&"Roads"));
    Ok(())
}

#[test]
fn songs_the_caller_cannot_play_are_suggested_by_name_only() -> TestResult {
    let w = world()?;
    let key = tracks_model::song_key("Creep", "Radiohead");
    let (member, admin) = (w.member.clone(), w.admin.clone());
    let (for_member, for_admin) = w.test.db.read_blocking(move |c| {
        Ok((
            store::missing_similar(c, &member, &key, 10)?,
            store::missing_similar(c, &admin, &key, 10)?,
        ))
    })?;
    let names = |v: &[(String, String)]| v.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>();
    assert_eq!(names(&for_member), ["Teardrop", "Glory Box"]);
    assert_eq!(names(&for_admin), ["Glory Box"]);
    Ok(())
}

#[test]
fn top_songs_follow_last_fm_until_people_play_them() -> TestResult {
    let w = world()?;
    let id = radiohead(&w)?;
    let (member, artist) = (w.member.clone(), id.clone());
    let top = w
        .test
        .db
        .read_blocking(move |c| songs::top_for_artist(c, &member, &artist, 10))?;
    assert_eq!(titles(&top).first(), Some(&"Lucky"));

    let user = w.member_key;
    w.test.db.write_blocking(move |tx| {
        tx.execute(
            "INSERT INTO play_stats (user_id, song_pid, play_count, last_played_at)
             SELECT ?1, pid, 30, 0 FROM songs WHERE title = 'Karma Police'",
            [user],
        )?;
        Ok(())
    })?;
    let (member, artist) = (w.member.clone(), id);
    let top = w
        .test
        .db
        .read_blocking(move |c| songs::top_for_artist(c, &member, &artist, 10))?;
    assert_eq!(titles(&top).first(), Some(&"Karma Police"));
    let missing = w
        .test
        .db
        .read_blocking(|c| store::missing_top(c, &Access::system(), "radiohead", 10))?;
    assert_eq!(missing, ["No Surprises"]);
    Ok(())
}

#[test]
fn a_starred_song_rises_for_the_one_who_starred_it() -> TestResult {
    let w = world()?;
    let user = w.member_key;
    let (member, creep) = (w.member.clone(), w.creep.clone());
    let before = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &member, &creep, 20))?;
    let last = before.last().ok_or("no songs")?.clone();
    assert_ne!(titles(&before).first(), Some(&last.title.as_str()));
    let pid = last.id.clone();
    w.test.db.write_blocking(move |tx| {
        tx.execute(
            "INSERT INTO stars (user_id, item_kind, item_pid, starred_at) VALUES (?1, 'song', ?2, 0)",
            rusqlite::params![user, pid],
        )?;
        Ok(())
    })?;
    let (member, creep) = (w.member.clone(), w.creep.clone());
    let after = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &member, &creep, 20))?;
    let rank = |list: &[Song]| list.iter().position(|s| s.id == last.id);
    assert!(
        rank(&after) < rank(&before),
        "{:?} then {:?}",
        titles(&before),
        titles(&after)
    );
    let (admin, creep) = (w.admin.clone(), w.creep.clone());
    let for_admin = w
        .test
        .db
        .read_blocking(move |c| similar::similar_songs(c, &admin, &creep, 20))?;
    assert_ne!(titles(&for_admin).first(), Some(&last.title.as_str()));
    Ok(())
}

#[test]
fn similar_artists_merge_last_fm_and_keep_artists_outside_the_library() -> TestResult {
    let w = world()?;
    let id = radiohead(&w)?;
    let member = w.member.clone();
    let found = w
        .test
        .db
        .read_blocking(move |c| similar::similar_artists(c, &member, &id, 10))?;
    let portishead = found
        .iter()
        .find(|s| s.name == "Portishead")
        .ok_or("no Portishead")?;
    assert!(portishead.in_library);
    let massive = found
        .iter()
        .find(|s| s.identity == tracks_model::folded("Massive Attack"))
        .ok_or("no Massive Attack")?;
    assert!(!massive.in_library && massive.artist.is_none());
    Ok(())
}

#[test]
fn lookups_expire_and_unknowns_are_remembered() -> TestResult {
    let w = world()?;
    let now = Clock::now(&*w.test.clock);
    let fresh = w.test.db.read_blocking(move |c| {
        Ok((
            store::is_fresh(c, store::Kind::Artist, "radiohead", now)?,
            store::due_artists(c, now, 10)?,
        ))
    })?;
    assert!(fresh.0);
    assert!(!fresh.1.iter().any(|(identity, _)| identity == "radiohead"));
    w.test
        .db
        .write_blocking(|tx| store::store_unknown(tx, store::Kind::Artist, "radiohead", DAY))?;
    let gone = w.test.db.read_blocking(|c| {
        Ok((
            store::bio(c, "radiohead")?,
            store::counts(c)?.artists,
            store::missing_top(c, &Access::system(), "radiohead", 10)?,
        ))
    })?;
    assert_eq!(gone, (None, 0, Vec::<String>::new()));
    w.test.clock.advance(std::time::Duration::from_secs(86_401));
    let now = Clock::now(&*w.test.clock);
    let due = w
        .test
        .db
        .read_blocking(move |c| store::due_artists(c, now, 10))?;
    assert!(due.iter().any(|(identity, _)| identity == "radiohead"));
    Ok(())
}
