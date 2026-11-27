//! Timings at 500k synthetic songs against the budgets of architecture section 15. Not run by
//! default: `cargo test --release -p trackside-core --test large -- --ignored --nocapture`.
//! `TRACKSIDE_LARGE_SONGS` changes the size.

use std::sync::Arc;
use std::time::{Duration, Instant};

use trackside_core::access::Role;
use trackside_core::db::FilesystemCheck;
use trackside_core::ids::{LibraryKey, SongId, random_id};
use trackside_core::repo::albums::{self, AlbumFilter, AlbumSort};
use trackside_core::repo::listens::{self, ListenSource, NewListen};
use trackside_core::repo::{artists, songs};
use trackside_core::search::{SearchKinds, SearchQuery, search};
use trackside_core::sync::{self, SyncPage};
use trackside_core::testing::{TEST_START, scanned};
use trackside_core::{Access, Db, DbOptions, EventBus, Millis, TestClock};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const WORDS: [&str; 24] = [
    "love", "night", "heart", "dance", "fire", "blue", "dream", "rain", "city", "light", "home",
    "wild", "gold", "summer", "river", "ghost", "angel", "storm", "shadow", "sweet", "electric",
    "midnight", "paradise", "forever",
];

fn word(n: usize) -> &'static str {
    WORDS[n % WORDS.len()]
}

/// p50 and p99 of `runs` timings of `f`, in milliseconds.
fn measure(
    runs: usize,
    mut f: impl FnMut(usize) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(f64, f64), Box<dyn std::error::Error>> {
    let mut times: Vec<Duration> = Vec::with_capacity(runs);
    for i in 0..runs {
        let start = Instant::now();
        f(i)?;
        times.push(start.elapsed());
    }
    times.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    Ok((
        ms(times[runs / 2]),
        ms(times[(runs * 99 / 100).min(runs - 1)]),
    ))
}

fn report(name: &str, budget_ms: f64, (p50, p99): (f64, f64)) {
    let verdict = if p99 <= budget_ms { "ok" } else { "OVER" };
    println!(
        "{name:<44} p50 {p50:>8.2} ms   p99 {p99:>8.2} ms   budget {budget_ms:>5} ms  {verdict}"
    );
}

fn seed(db: &Db, library: LibraryKey, total: usize) -> Result<(), Box<dyn std::error::Error>> {
    let batch = 1000;
    let start = Instant::now();
    for chunk in 0..total.div_ceil(batch) {
        let files: Vec<_> = (chunk * batch..((chunk + 1) * batch).min(total))
            .map(|n| {
                let artist = format!("{} {} {}", word(n / 13), word(n / 7 + 3), n / 50);
                let album = format!("{} {}", word(n / 11 + 5), n / 12);
                let title = format!("{} {} {}", word(n), word(n / 3 + 1), n);
                let mut file = scanned(
                    &format!("{artist}/{album}/{n}.flac"),
                    &title,
                    &artist,
                    Some(&album),
                );
                file.tags.track = Some(u32::try_from(n % 12)? + 1);
                file.tags.year = Some(1960 + i32::try_from(n % 60)?);
                file.tags.genres = vec![String::from(word(n / 101 + 7))];
                Ok(file)
            })
            .collect::<Result<_, std::num::TryFromIntError>>()?;
        db.write_blocking(move |tx| {
            let run = random_id();
            trackside_core::ingest::apply_batch(
                tx,
                library,
                &run,
                trackside_core::ingest::SongOrigin::Scan,
                &files,
            )
        })?;
    }
    println!(
        "seeded {total} songs in {:.1} s ({:.0} files/s through ingest)",
        start.elapsed().as_secs_f64(),
        total as f64 / start.elapsed().as_secs_f64()
    );
    Ok(())
}

#[test]
#[expect(clippy::too_many_lines, reason = "one measurement after another")]
#[ignore = "seeds a large library; run in release with --ignored"]
fn large_library_stays_within_budgets() -> TestResult {
    let total: usize = std::env::var("TRACKSIDE_LARGE_SONGS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500_000);
    let dir = tempfile::tempdir()?;
    let mut options = DbOptions::new(dir.path().join("trackside.db"));
    options.filesystem = FilesystemCheck::AssumeLocal;
    let db = Db::open(
        &options,
        EventBus::default(),
        Arc::new(TestClock::new(TEST_START)),
    )?;
    let library = db.write_blocking(|tx| {
        trackside_core::repo::libraries::create(
            tx,
            &trackside_core::repo::libraries::NewLibrary {
                name: String::from("Music"),
                root: dir.path().join("music"),
                writable: false,
                watch: false,
                grant_all_users: false,
                granted_by: None,
            },
        )
    })?;
    seed(&db, library.key, total)?;
    db.checkpoint_and_optimize()?;
    let member = db.write_blocking(move |tx| {
        trackside_core::repo::users::create(
            tx,
            &trackside_core::repo::users::NewUser {
                username: String::from("member"),
                display_name: String::from("member"),
                password_hash: String::from("x"),
                role: Role::Member,
                permissions: trackside_core::Permissions::default(),
                libraries: vec![library.key],
                service: None,
            },
        )
    })?;
    let member = db
        .read_blocking(|c| trackside_core::repo::users::access(c, member.key))?
        .ok_or("no access")?;
    let admin = Access::system();
    if std::env::var_os("TRACKSIDE_LARGE_PLANS").is_some() {
        db.read_blocking(|c| {
            for sql in [
                "SELECT a.id FROM albums a JOIN libraries l ON l.id = a.library_id LEFT JOIN artists ar ON ar.id = a.artist_id WHERE +a.library_id IN (1) AND (a.added_at < 5 OR (a.added_at = 5 AND a.id < 3)) ORDER BY a.added_at DESC, a.id DESC LIMIT 101",
                "SELECT a.id FROM albums a JOIN libraries l ON l.id = a.library_id LEFT JOIN artists ar ON ar.id = a.artist_id WHERE +a.library_id IN (1) ORDER BY a.sort_name, a.id LIMIT 101",
            ] {
                let mut statement = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                let rows: Vec<String> = statement.query_map([], |row| row.get(3))?.collect::<Result<_, _>>()?;
                println!("{rows:?}");
            }
            Ok(())
        })?;
    }
    println!("--- {total} songs, member access (one granted library) unless noted");

    for (name, sort) in [
        ("albums newest", AlbumSort::Newest),
        ("albums alphabetical", AlbumSort::Alphabetical),
        (
            "albums alphabetical_by_artist",
            AlbumSort::AlphabeticalByArtist,
        ),
        ("albums year", AlbumSort::Year),
        ("albums random", AlbumSort::Random { seed: 7 }),
    ] {
        let a = member.clone();
        let mut cursor: Option<String> = None;
        let timing = measure(50, |_| {
            let a = a.clone();
            let c = cursor.clone();
            let page = db.read_blocking(move |conn| {
                albums::list(conn, &a, sort, c.as_deref(), 100, &AlbumFilter::default())
            })?;
            cursor = page.next;
            Ok(())
        })?;
        report(&format!("{name} (100, paging on)"), 40.0, timing);
    }
    let a = member.clone();
    let mut cursor: Option<String> = None;
    report(
        "artists list (100, paging on)",
        40.0,
        measure(50, |_| {
            let a = a.clone();
            let c = cursor.clone();
            cursor = db
                .read_blocking(move |conn| artists::list(conn, &a, None, c.as_deref(), 100))?
                .next;
            Ok(())
        })?,
    );
    let album_ids: Vec<_> = db
        .read_blocking(|c| {
            albums::list(
                c,
                &Access::system(),
                AlbumSort::Random { seed: 1 },
                None,
                100,
                &AlbumFilter::default(),
            )
        })?
        .items
        .into_iter()
        .map(|a| a.id)
        .collect();
    let a = member.clone();
    report(
        "album with songs",
        40.0,
        measure(100, |i| {
            let a = a.clone();
            let id = album_ids[i % album_ids.len()].clone();
            db.read_blocking(move |c| {
                albums::get(c, &a, &id)?;
                songs::by_album(c, &a, &id)
            })?;
            Ok(())
        })?,
    );
    let artist_ids: Vec<_> = db
        .read_blocking(|c| artists::list(c, &Access::system(), None, None, 100))?
        .items
        .into_iter()
        .map(|a| a.id)
        .collect();
    let a = member.clone();
    report(
        "artist page (albums, appears on, top songs)",
        40.0,
        measure(100, |i| {
            let a = a.clone();
            let id = artist_ids[i % artist_ids.len()].clone();
            db.read_blocking(move |c| {
                artists::get(c, &a, &id)?;
                albums::by_artist(c, &a, &id)?;
                albums::appears_on(c, &a, &id)?;
                songs::top_for_artist(c, &a, &id, 20)
            })?;
            Ok(())
        })?,
    );
    for (who, access) in [("member", member.clone()), ("admin", admin.clone())] {
        for text in [
            "l",
            "lo",
            "love",
            "love night",
            "midnight paradise 12",
            "zzz",
        ] {
            let a = access.clone();
            report(
                &format!("search {text:?} ({who})"),
                25.0,
                measure(40, |_| {
                    let a = a.clone();
                    let q = SearchQuery {
                        text: text.to_owned(),
                        kinds: SearchKinds::ALL,
                        limit: 20,
                        cursor: None,
                        library: None,
                    };
                    db.read_blocking(move |c| search(c, &a, &q))?;
                    Ok(())
                })?,
            );
        }
    }
    let a = member.clone();
    let ids: Vec<SongId> = db.read_blocking(move |c| {
        Ok(songs::all_paged(c, &a, None, 500)?
            .0
            .into_iter()
            .map(|s| s.id)
            .collect())
    })?;
    let a = member.clone();
    report(
        "songs get_many 500",
        40.0,
        measure(20, |_| {
            let a = a.clone();
            let ids = ids.clone();
            db.read_blocking(move |c| songs::get_many(c, &a, &ids))?;
            Ok(())
        })?,
    );
    let a = member.clone();
    report(
        "random songs 40",
        40.0,
        measure(50, |_| {
            let a = a.clone();
            db.read_blocking(move |c| songs::random(c, &a, &songs::SongFilter::default(), 40))?;
            Ok(())
        })?,
    );
    let a = member.clone();
    let first_song = ids[0].clone();
    report(
        "similar songs 20",
        40.0,
        measure(30, |_| {
            let a = a.clone();
            let id = first_song.clone();
            db.read_blocking(move |c| trackside_core::similar::similar_songs(c, &a, &id, 20))?;
            Ok(())
        })?,
    );

    // Snapshot pages of 5000 songs, from the songs kind onward.
    let a = member.clone();
    let start = db.read_blocking(move |c| {
        Ok(sync::SyncCursor {
            epoch: a.sync_epoch,
            seq: sync::latest_seq(c)?,
            snapshot: Some(sync::SnapshotPosition {
                kind: sync::SnapshotKind::Songs,
                after: 0,
                after_key: None,
            }),
        })
    })?;
    let a = member.clone();
    let mut cursor = start;
    report(
        "sync snapshot page of 5000 songs",
        120.0,
        measure(10, |_| {
            let a = a.clone();
            let c = cursor.clone();
            let page = db.read_blocking(move |conn| {
                sync::page(conn, &a, Some(&c), 5000).map_err(|e| match e {
                    sync::SyncError::Db(e) => e,
                    other => trackside_core::DbError::Invalid(other.to_string()),
                })
            })?;
            if let SyncPage::Snapshot(page) = page {
                cursor = page.next_cursor;
            }
            Ok(())
        })?,
    );

    // A delta of 100 changes.
    let a = member.clone();
    let before = db.read_blocking(sync::latest_seq)?;
    let user = member.user;
    let changed = ids[..100].to_vec();
    db.write_blocking(move |tx| {
        for id in &changed {
            trackside_core::repo::stars::set(
                tx,
                user,
                &trackside_core::records::ItemRef::Song(id.clone()),
                Millis(1),
            )?;
        }
        Ok(())
    })?;
    let cursor = sync::SyncCursor {
        epoch: a.sync_epoch,
        seq: before,
        snapshot: None,
    };
    report(
        "sync delta with 100 changes",
        10.0,
        measure(30, |_| {
            let a = a.clone();
            let c = cursor.clone();
            db.read_blocking(move |conn| {
                sync::page(conn, &a, Some(&c), 2000)
                    .map_err(|e| trackside_core::DbError::Invalid(e.to_string()))
            })?;
            Ok(())
        })?,
    );

    let mut batch_no = 0;
    report(
        "listens submit 500",
        80.0,
        measure(10, |_| {
            batch_no += 1;
            let items: Vec<NewListen> = ids
                .iter()
                .enumerate()
                .map(|(i, id)| NewListen {
                    client_listen_id: format!("{batch_no}-{i}"),
                    song: Some(id.clone()),
                    title: String::from("t"),
                    artist: String::from("a"),
                    album: None,
                    started_at: Millis(TEST_START.0 - i64::try_from(i).unwrap_or(0) * 1000),
                    played_ms: None,
                    duration_ms: None,
                    source: ListenSource::Native,
                })
                .collect();
            db.write_blocking(move |tx| listens::submit(tx, user, None, &items))?;
            Ok(())
        })?,
    );
    let a = member.clone();
    report(
        "albums frequent after listens",
        40.0,
        measure(20, |_| {
            let a = a.clone();
            db.read_blocking(move |c| {
                albums::list(
                    c,
                    &a,
                    AlbumSort::Frequent,
                    None,
                    100,
                    &AlbumFilter::default(),
                )
            })?;
            Ok(())
        })?,
    );
    Ok(())
}
