//! Trackside Cloud over a real database file: batches, cursors, settings, quotas, isolation,
//! pruning, deletion with the user, backups, and a 100k-play import beside another user.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use trackside_cloud::store::{self, Registration};
use trackside_cloud::{
    Body, CloudError, Entity, Limits, PushItem, PushOutcome, Reject, SettingOutcome, SettingWrite,
};
use trackside_core::db::FilesystemCheck;
use trackside_core::repo::users::{self, NewUser};
use trackside_core::testing::TEST_START;
use trackside_core::{
    Db, DbOptions, EventBus, Millis, Permissions, Role, ServerEvent, TestClock, UserKey,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    _dir: tempfile::TempDir,
    db: Db,
    clock: Arc<TestClock>,
    events: EventBus,
}

fn open() -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut options = DbOptions::new(dir.path().join("trackside.db"));
    options.filesystem = FilesystemCheck::AssumeLocal;
    options.readers = 4;
    let clock = Arc::new(TestClock::new(TEST_START));
    let events = EventBus::default();
    let db = Db::open(&options, events.clone(), clock.clone())?;
    Ok(Fixture {
        _dir: dir,
        db,
        clock,
        events,
    })
}

fn user(db: &Db, name: &str) -> Result<UserKey, Box<dyn std::error::Error>> {
    let new = NewUser {
        username: name.to_owned(),
        display_name: name.to_owned(),
        password_hash: String::from("$argon2id$test"),
        role: Role::Member,
        permissions: Permissions::default(),
        libraries: Vec::new(),
    };
    Ok(db.write_blocking(move |tx| users::create(tx, &new))?.key)
}

fn item(id: &str, fields: Value, at: i64, device: &str) -> PushItem {
    PushItem {
        id: id.to_owned(),
        base_version: None,
        fields: match fields {
            Value::Object(map) => Some(map),
            _ => None,
        },
        client_modified_at: Millis(at),
        device_id: device.to_owned(),
        deleted_at: None,
        merged_into: None,
        undelete: false,
    }
}

fn delete(id: &str, at: i64, device: &str) -> PushItem {
    PushItem {
        fields: None,
        deleted_at: Some(Millis(at)),
        ..item(id, json!({}), at, device)
    }
}

fn play(n: usize) -> PushItem {
    item(
        &format!("00000000-0000-4000-8000-{n:012}"),
        json!({"title": format!("Song {n}"), "artistName": format!("Artist {}", n % 500),
               "albumTitle": "Album", "playedAtMs": TEST_START.0 - 1_000 * n as i64,
               "kind": "onDemand", "source": "appleMusic", "recordedByDeviceID": "IPHONE",
               "platform": "ios", "durationMs": 200_000,
               "artworkURL": format!("https://is1.example/{n}.jpg")}),
        TEST_START.0 - 1_000,
        "IPHONE",
    )
}

fn statuses(results: &[trackside_cloud::PushResult]) -> Vec<&'static str> {
    results
        .iter()
        .map(|r| match r.outcome {
            PushOutcome::Accepted { .. } => "accepted",
            PushOutcome::Duplicate { .. } => "duplicate",
            PushOutcome::Conflict { .. } => "conflict",
            PushOutcome::Rejected { reason } => reason.as_str(),
        })
        .collect()
}

#[tokio::test]
async fn replaying_a_batch_is_idempotent() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let batch: Vec<PushItem> = (0..5).map(play).collect();
    let first = store::push(&f.db, u, Entity::Plays, batch.clone(), Limits::default()).await?;
    assert_eq!(statuses(&first), vec!["accepted"; 5]);
    let page = store::pull(&f.db, u, Entity::Plays, None, 100).await?;
    let again = store::push(&f.db, u, Entity::Plays, batch, Limits::default()).await?;
    assert_eq!(statuses(&again), vec!["duplicate"; 5]);
    let after = store::pull(&f.db, u, Entity::Plays, Some(page.cursor), 100).await?;
    assert!(after.upserts.is_empty(), "a replay writes nothing");
    assert_eq!(after.cursor, page.cursor);
    let versions: Vec<i64> = first
        .iter()
        .filter_map(|r| match r.outcome {
            PushOutcome::Accepted { version } => Some(version),
            _ => None,
        })
        .collect();
    assert_eq!(versions, vec![1, 2, 3, 4, 5]);
    Ok(())
}

#[tokio::test]
async fn compare_and_set_conflicts_return_the_current_copy() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    let create = item("pl-1", json!({"name": "Drive"}), 10, "MAC");
    let created = store::push(&f.db, u, Entity::Playlists, vec![create], limits).await?;
    let PushOutcome::Accepted { version } = created[0].outcome else {
        return Err("not accepted".into());
    };
    let rename = |name: &str, base: i64| PushItem {
        base_version: Some(base),
        ..item("pl-1", json!({"name": name}), 20, "IPHONE")
    };
    let ok = store::push(&f.db, u, Entity::Playlists, vec![rename("Night", version)], limits).await?;
    assert_eq!(statuses(&ok), vec!["accepted"]);
    let stale = store::push(&f.db, u, Entity::Playlists, vec![rename("Day", version)], limits).await?;
    let PushOutcome::Conflict {
        current: Some(current),
    } = &stale[0].outcome
    else {
        return Err(format!("{stale:?}").into());
    };
    assert_eq!(current.version, version + 1);
    assert_eq!(current.body, Body::Live(json!({"name": "Night"}).as_object().cloned().unwrap_or_default()));
    Ok(())
}

#[tokio::test]
async fn tombstones_pull_as_deletes_and_need_an_explicit_undelete() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    store::push(&f.db, u, Entity::Plays, vec![play(1), play(2)], limits).await?;
    let mut merged = delete(&play(2).id, 50, "MAC");
    merged.merged_into = Some(play(1).id);
    let gone = store::push(&f.db, u, Entity::Plays, vec![merged], limits).await?;
    assert_eq!(statuses(&gone), vec!["accepted"]);
    let page = store::pull(&f.db, u, Entity::Plays, None, 100).await?;
    assert_eq!(page.upserts.len(), 1);
    assert_eq!(page.deletes.len(), 1);
    assert_eq!(
        page.deletes[0].body,
        Body::Deleted {
            deleted_at: Millis(50),
            merged_into: Some(play(1).id)
        }
    );
    let back = store::push(&f.db, u, Entity::Plays, vec![play(2)], limits).await?;
    assert_eq!(statuses(&back), vec!["deleted"]);
    let undelete = PushItem {
        undelete: true,
        ..play(2)
    };
    let back = store::push(&f.db, u, Entity::Plays, vec![undelete], limits).await?;
    assert_eq!(statuses(&back), vec!["accepted"]);
    let delta = store::pull(&f.db, u, Entity::Plays, Some(page.cursor), 100).await?;
    assert_eq!(delta.upserts.len(), 1);
    assert!(delta.deletes.is_empty());
    Ok(())
}

#[tokio::test]
async fn paging_while_writes_land_loses_and_repeats_nothing() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    store::push(&f.db, u, Entity::Plays, (0..300).map(play).collect(), limits).await?;
    let mut seen: BTreeMap<String, i64> = BTreeMap::new();
    let mut cursor = None;
    let mut next_play = 300;
    let mut pages = 0;
    loop {
        let page = store::pull(&f.db, u, Entity::Plays, cursor, 37).await?;
        for record in page.upserts.iter().chain(&page.deletes) {
            let previous = seen.insert(record.id.clone(), record.version);
            assert!(
                previous.is_none_or(|v| v < record.version),
                "{} came twice at the same version",
                record.id
            );
        }
        cursor = Some(page.cursor);
        pages += 1;
        if pages < 6 {
            // New plays, an update to one already paged, and a delete, between pages.
            let mut writes: Vec<PushItem> = (next_play..next_play + 20).map(play).collect();
            next_play += 20;
            writes.push(item(&play(pages).id, json!({"catalogID": format!("c{pages}")}), 1, "MAC"));
            store::push(&f.db, u, Entity::Plays, writes, limits).await?;
            store::push(&f.db, u, Entity::Plays, vec![delete(&play(100 + pages).id, 2, "MAC")], limits)
                .await?;
        }
        if !page.has_more {
            break;
        }
    }
    let everything = store::pull(&f.db, u, Entity::Plays, None, 2000).await?;
    let truth: BTreeMap<String, i64> = everything
        .upserts
        .iter()
        .chain(&everything.deletes)
        .map(|r| (r.id.clone(), r.version))
        .collect();
    assert_eq!(seen, truth, "the client's copy ends up exactly the server's");
    assert_eq!(truth.len(), next_play);
    Ok(())
}

#[tokio::test]
async fn cursors_from_the_future_or_before_pruning_reset() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    let add = item("abba", json!({"name": "ABBA"}), 10, "MAC");
    store::push(&f.db, u, Entity::BlockedArtists, vec![add], limits).await?;
    store::push(&f.db, u, Entity::BlockedArtists, vec![delete("abba", 20, "MAC")], limits).await?;
    store::push(&f.db, u, Entity::Plays, vec![play(1)], limits).await?;
    store::push(&f.db, u, Entity::Plays, vec![delete(&play(1).id, 30, "MAC")], limits).await?;
    assert!(matches!(
        store::pull(&f.db, u, Entity::Plays, Some(999), 10).await,
        Err(CloudError::SyncReset)
    ));
    f.clock.advance(Duration::from_hours(24 * 91));
    assert_eq!(store::prune(&f.db, u, false).await?, 1, "the block, not the play");
    assert!(matches!(
        store::pull(&f.db, u, Entity::BlockedArtists, Some(1), 10).await,
        Err(CloudError::SyncReset)
    ));
    let fresh = store::pull(&f.db, u, Entity::BlockedArtists, None, 10).await?;
    assert!(fresh.upserts.is_empty() && fresh.deletes.is_empty());
    assert!(store::pull(&f.db, u, Entity::BlockedArtists, Some(fresh.cursor), 10).await.is_ok());
    let plays = store::pull(&f.db, u, Entity::Plays, None, 10).await?;
    assert_eq!(plays.deletes.len(), 1, "play tombstones are kept forever");
    Ok(())
}

#[tokio::test]
async fn stations_upsert_by_their_folded_name() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    let first = item("", json!({"name": "Café Radio", "firstSeenMs": 100, "lastSeenMs": 200}), 1, "IPHONE");
    let second = item("", json!({"name": "  CAFE radio", "firstSeenMs": 50, "lastSeenMs": 150, "stationID": "ra.9"}), 2, "MAC");
    let results = store::push(&f.db, u, Entity::Stations, vec![first, second], limits).await?;
    assert_eq!(statuses(&results), vec!["accepted", "accepted"]);
    assert_eq!(results[0].id, "cafe radio");
    let page = store::pull(&f.db, u, Entity::Stations, None, 10).await?;
    assert_eq!(page.upserts.len(), 1);
    assert_eq!(
        page.upserts[0].body,
        Body::Live(
            json!({"name": "Café Radio", "firstSeenMs": 50, "lastSeenMs": 200, "stationID": "ra.9"})
                .as_object()
                .cloned()
                .unwrap_or_default()
        )
    );
    Ok(())
}

fn setting(key: &str, value: Value, at: i64) -> SettingWrite {
    SettingWrite {
        key: key.to_owned(),
        value,
        modified_at: Millis(at),
        device_id: String::from("MAC"),
    }
}

#[tokio::test]
async fn settings_follow_the_decide_rules() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    let seeds = vec![
        setting("playlistName", json!("Tracks"), 0),
        setting("forgottenSongs", json!(["b"]), 0),
        setting("macOnly.menuBarStyle", json!({"compact": true}), 0),
    ];
    let results = store::patch_settings(&f.db, u, seeds, limits).await?;
    assert!(results.iter().all(|r| r.outcome == SettingOutcome::Accepted));
    let later = vec![
        setting("playlistName", json!("Mine"), 0),
        setting("playlistName", json!("Mine"), 5_000),
        setting("forgottenSongs", json!(["a"]), 1_000),
        setting("playlistID", Value::Null, 6_000),
        setting("playlistID", json!("pl.1"), 5_500),
    ];
    let results = store::patch_settings(&f.db, u, later, limits).await?;
    let outcomes: Vec<_> = results.iter().map(|r| r.outcome.clone()).collect();
    assert_eq!(
        outcomes,
        vec![
            SettingOutcome::Conflict,
            SettingOutcome::Accepted,
            SettingOutcome::Accepted,
            SettingOutcome::Accepted,
            SettingOutcome::Conflict,
        ]
    );
    assert_eq!(results[2].current.as_ref().map(|s| s.value.clone()), Some(json!(["a", "b"])));
    let all = store::settings(&f.db, u, None).await?;
    let values: BTreeMap<_, _> = all.settings.iter().map(|s| (s.key.as_str(), s.value.clone())).collect();
    assert_eq!(values["playlistName"], json!("Mine"));
    assert_eq!(values["playlistID"], Value::Null, "a cleared value is an explicit null");
    assert_eq!(values["macOnly.menuBarStyle"], json!({"compact": true}));
    let since = store::settings(&f.db, u, Some(all.cursor)).await?;
    assert!(since.settings.is_empty());
    Ok(())
}

#[tokio::test]
async fn nearby_key_is_stored_only_as_an_opaque_envelope() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    let plain = setting("nearbyKey", json!("q83vEjRWeJAq83vEjRWeJAq83vEjRWeJAq83vEjRWeJA="), 1);
    let results = store::patch_settings(&f.db, u, vec![plain], limits).await?;
    assert_eq!(results[0].outcome, SettingOutcome::Rejected(Reject::EncryptionRequired));
    let envelope = json!({"$encrypted": {"v": 1, "alg": "chachapoly", "kid": "k1",
        "nonce": "AAAAAAAAAAAAAAAA", "ct": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}});
    let sealed = setting("nearbyKey", envelope.clone(), 2);
    let results = store::patch_settings(&f.db, u, vec![sealed], limits).await?;
    assert_eq!(results[0].outcome, SettingOutcome::Accepted);
    let all = store::settings(&f.db, u, None).await?;
    assert_eq!(all.settings[0].value, envelope, "returned exactly as sent");
    Ok(())
}

#[tokio::test]
async fn one_user_never_sees_or_changes_anothers_data() -> TestResult {
    let f = open()?;
    let luke = user(&f.db, "luke")?;
    let nana = user(&f.db, "nana")?;
    let limits = Limits::default();
    store::push(&f.db, luke, Entity::Plays, vec![play(1)], limits).await?;
    store::patch_settings(&f.db, luke, vec![setting("playlistName", json!("Luke's"), 5)], limits)
        .await?;
    for entity in Entity::ALL.into_iter().filter(|e| *e != Entity::Settings) {
        let page = store::pull(&f.db, nana, entity, None, 100).await?;
        assert!(page.upserts.is_empty() && page.deletes.is_empty(), "{entity:?}");
    }
    assert!(store::settings(&f.db, nana, None).await?.settings.is_empty());
    // The same id under another user is a different record.
    let theirs = store::push(&f.db, nana, Entity::Plays, vec![delete(&play(1).id, 9, "X")], limits).await?;
    assert_eq!(statuses(&theirs), vec!["accepted"]);
    let mine = store::pull(&f.db, luke, Entity::Plays, None, 10).await?;
    assert_eq!(mine.upserts.len(), 1);
    assert!(mine.deletes.is_empty());
    assert_eq!(store::usage(&f.db, nana).await?.items, 1);
    Ok(())
}

#[tokio::test]
async fn quotas_reject_new_items_but_allow_deletes() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits {
        max_items: 3,
        max_bytes: 1 << 20,
    };
    let results = store::push(&f.db, u, Entity::Plays, (0..5).map(play).collect(), limits).await?;
    assert_eq!(
        statuses(&results),
        vec!["accepted", "accepted", "accepted", "quota_exceeded", "quota_exceeded"]
    );
    let gone = store::push(&f.db, u, Entity::Plays, vec![delete(&play(0).id, 5, "MAC")], limits).await?;
    assert_eq!(statuses(&gone), vec!["accepted"]);
    let tight = Limits {
        max_items: 100,
        max_bytes: 2_000,
    };
    let bytes = store::push(&f.db, u, Entity::Plays, (10..20).map(play).collect(), tight).await?;
    assert!(statuses(&bytes).contains(&"quota_exceeded"));
    let usage = store::usage(&f.db, u).await?;
    assert!(usage.bytes > 0 && usage.items >= 3);
    assert!(matches!(
        store::push(&f.db, u, Entity::Plays, (0..1001).map(play).collect(), limits).await,
        Err(CloudError::TooMany)
    ));
    Ok(())
}

#[tokio::test]
async fn deleting_a_playlist_deletes_its_entries() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    store::push(&f.db, u, Entity::Playlists, vec![item("pl", json!({"name": "A"}), 1, "MAC")], limits).await?;
    let entries = (0..3)
        .map(|n| {
            item(
                &format!("e{n}"),
                json!({"playlistID": "pl", "order": format!("a{n}"),
                       "track": {"title": "Halo", "artist": "Beyoncé", "ids": {"appleMusic": "1"}}}),
                1,
                "MAC",
            )
        })
        .collect();
    store::push(&f.db, u, Entity::PlaylistEntries, entries, limits).await?;
    let mut events = f.events.subscribe();
    store::push(&f.db, u, Entity::Playlists, vec![delete("pl", 9, "MAC")], limits).await?;
    let page = store::pull(&f.db, u, Entity::PlaylistEntries, None, 10).await?;
    assert_eq!(page.deletes.len(), 3);
    let mut entities = HashSet::new();
    while let Ok(event) = events.try_recv() {
        if let ServerEvent::CloudChanged { user, entity } = &*event {
            assert_eq!(*user, u);
            entities.insert(*entity);
        }
    }
    assert_eq!(entities, HashSet::from(["playlists", "playlist-entries"]));
    Ok(())
}

#[tokio::test]
async fn restored_devices_inherit_sessions() -> TestResult {
    let f = open()?;
    let u = user(&f.db, "luke")?;
    let limits = Limits::default();
    let session = item("s1", json!({"startedAtMs": 1, "recordedByDeviceID": "OLD"}), 1, "OLD");
    store::push(&f.db, u, Entity::Sessions, vec![session], limits).await?;
    let update = item("s1", json!({"endedAtMs": 2}), 2, "NEW");
    let refused = store::push(&f.db, u, Entity::Sessions, vec![update.clone()], limits).await?;
    assert_eq!(statuses(&refused), vec!["not_owner"]);
    let registration = Registration {
        device_id: String::from("NEW"),
        predecessors: vec![String::from("OLD")],
        name: Some(String::from("New iPhone")),
        platform: Some(String::from("ios")),
    };
    let device = store::register_device(&f.db, u, registration, None).await?;
    assert_eq!(device.predecessors, vec![String::from("OLD")]);
    let allowed = store::push(&f.db, u, Entity::Sessions, vec![update], limits).await?;
    assert_eq!(statuses(&allowed), vec!["accepted"]);
    Ok(())
}

#[tokio::test]
async fn cloud_data_goes_with_its_user_and_into_backups() -> TestResult {
    let f = open()?;
    let luke = user(&f.db, "luke")?;
    let nana = user(&f.db, "nana")?;
    let limits = Limits::default();
    for u in [luke, nana] {
        store::push(&f.db, u, Entity::Plays, (0..10).map(play).collect(), limits).await?;
        store::patch_settings(&f.db, u, vec![setting("playlistName", json!("x"), 1)], limits).await?;
    }
    let dir = tempfile::tempdir()?;
    let backup = dir.path().join("backup.db");
    f.db.backup_into(&backup)?;
    let copy = rusqlite::Connection::open(&backup)?;
    let rows: i64 = copy.query_row("SELECT count(*) FROM cloud_items", [], |row| row.get(0))?;
    assert_eq!(rows, 22);
    f.db.write(move |tx| users::delete(tx, luke)).await?;
    let left: Vec<i64> = f.db.read(|c| {
        let mut statement = c.prepare(
            "SELECT count(*) FROM cloud_items UNION ALL SELECT count(*) FROM cloud_entities
             UNION ALL SELECT count(*) FROM cloud_state",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<i64>, _>>()?)
    })
    .await?;
    assert_eq!(left, vec![11, 2, 1], "only nana's rows remain");
    Ok(())
}

/// 100k plays pushed in batches of 1000 while another user keeps pushing and pulling. The
/// import commits in short chunks, so the other user never waits long for the writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_100k_import_does_not_block_another_user() -> TestResult {
    const PLAYS: usize = 100_000;
    let f = open()?;
    let importer = user(&f.db, "luke")?;
    let other = user(&f.db, "nana")?;
    let limits = Limits::default();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let neighbour = {
        let db = f.db.clone();
        let done = Arc::clone(&done);
        tokio::spawn(async move {
            let mut waits = Vec::new();
            let mut cursor = None;
            let mut n = 0;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let started = Instant::now();
                store::push(&db, other, Entity::Plays, vec![play(n)], limits).await?;
                let page = store::pull(&db, other, Entity::Plays, cursor, 100).await?;
                cursor = Some(page.cursor);
                waits.push(started.elapsed());
                n += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            waits.sort();
            Ok::<_, CloudError>((waits, n))
        })
    };
    let started = Instant::now();
    for batch in 0..PLAYS / 1000 {
        let items = (batch * 1000..(batch + 1) * 1000).map(play).collect();
        let results = store::push(&f.db, importer, Entity::Plays, items, limits).await?;
        assert!(results.iter().all(|r| matches!(r.outcome, PushOutcome::Accepted { .. })));
    }
    let import = started.elapsed();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let (waits, rounds) = neighbour.await??;
    let worst = waits.last().copied().unwrap_or_default();
    let median = waits.get(waits.len() / 2).copied().unwrap_or_default();
    let p99 = waits.get(waits.len() * 99 / 100).copied().unwrap_or_default();
    let mut pulled = 0;
    let mut cursor = None;
    loop {
        let page = store::pull(&f.db, importer, Entity::Plays, cursor, 2000).await?;
        pulled += page.upserts.len();
        cursor = Some(page.cursor);
        if !page.has_more {
            break;
        }
    }
    println!(
        "imported {PLAYS} plays in {import:?} ({:.0}/s); the other user's push+pull: {rounds} rounds, median {median:?}, p99 {p99:?}, worst {worst:?}",
        PLAYS as f64 / import.as_secs_f64()
    );
    assert_eq!(pulled, PLAYS);
    assert!(rounds > 10, "the other user kept going during the import");
    assert!(worst < Duration::from_secs(2), "worst wait {worst:?}");
    Ok(())
}
