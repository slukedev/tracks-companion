//! Listens, per-user data, accounts and the smaller repositories.

use std::time::Duration;

use trackside_core::access::{Permissions, Role};
use trackside_core::ids::{PublicId, SongId, UserKey};
use trackside_core::records::{ItemKind, ItemRef};
use trackside_core::repo::listens::{ListenSource, ListenStatus, NewListen, RejectReason};
use trackside_core::repo::playlists::{NewPlaylist, PlaylistPatch};
use trackside_core::repo::{
    annotations, device_links, devices, idempotency, invites, keys, listens, pairings,
    password_resets, playlists, sessions, users,
};
use trackside_core::testing::{TEST_START, TestDb, scanned};
use trackside_core::{Access, Clock, DbError, Millis};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn listen(id: &str, song: Option<&SongId>, started_at: Millis) -> NewListen {
    NewListen {
        client_listen_id: id.to_owned(),
        song: song.cloned(),
        title: String::from("Halo"),
        artist: String::from("Beyoncé"),
        album: None,
        started_at,
        played_ms: Some(200_000),
        duration_ms: Some(261_640),
        source: ListenSource::Native,
    }
}

fn seeded() -> Result<(TestDb, UserKey, SongId, tempfile::TempDir), Box<dyn std::error::Error>> {
    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = test.add_library("Music", dir.path())?.key;
    let user = test.add_user("luke", Role::Member, &[lib])?.key;
    let song = test.ingest(lib, &[scanned("a.flac", "Halo", "Beyoncé", None)])?[0]
        .song()
        .clone();
    Ok((test, user, song, dir))
}

#[test]
fn resubmitted_batch_is_all_duplicates() -> TestResult {
    let (test, user, song, _dir) = seeded()?;
    let batch: Vec<NewListen> = (0..5)
        .map(|i| {
            listen(
                &format!("play-{i}"),
                Some(&song),
                Millis(TEST_START.0 - 60_000 * i),
            )
        })
        .collect();
    let again = batch.clone();
    let first = test.write_blocking(move |tx| listens::submit(tx, user, None, &batch))?;
    assert!(first.iter().all(|o| o.status == ListenStatus::Accepted));
    let second = test.write_blocking(move |tx| listens::submit(tx, user, None, &again))?;
    assert!(second.iter().all(|o| o.status == ListenStatus::Duplicate));
    assert_eq!(test.read_blocking(move |c| listens::count(c, user))?, 5);
    Ok(())
}

#[test]
fn future_listen_is_rejected() -> TestResult {
    let (test, user, song, _dir) = seeded()?;
    let far = TEST_START.saturating_add(Duration::from_hours(25));
    let near = TEST_START.saturating_add(Duration::from_secs(3600));
    let items = vec![
        listen("far", Some(&song), far),
        listen("near", Some(&song), near),
        NewListen {
            title: String::new(),
            ..listen("blank", None, TEST_START)
        },
    ];
    let outcome = test.write_blocking(move |tx| listens::submit(tx, user, None, &items))?;
    assert_eq!(
        outcome[0].status,
        ListenStatus::Rejected(RejectReason::Future)
    );
    assert_eq!(outcome[1].status, ListenStatus::Accepted);
    assert_eq!(
        outcome[2].status,
        ListenStatus::Rejected(RejectReason::Invalid)
    );
    Ok(())
}

#[test]
fn play_stats_follow_listens() -> TestResult {
    let (test, user, song, _dir) = seeded()?;
    let items = vec![
        listen("1", Some(&song), Millis(1_000)),
        listen("2", Some(&song), Millis(3_000)),
        listen("3", Some(&song), Millis(2_000)),
        listen("off-server", None, Millis(4_000)),
    ];
    test.write_blocking(move |tx| listens::submit(tx, user, None, &items))?;
    let id = song.clone();
    let stat = test.read_blocking(move |c| listens::play_stat(c, user, &id))?;
    assert_eq!(stat, Some((3, Millis(3_000))));
    let item = ItemRef::Song(song);
    let notes = test.read_blocking(move |c| annotations::for_items(c, user, &[item]))?;
    assert_eq!(notes.values().next().map(|n| n.play_count), Some(3));
    let (rows, next) = test.read_blocking(move |c| listens::export(c, user, None, 2))?;
    assert_eq!(rows.len(), 2);
    assert!(next.is_some());
    assert_eq!(rows[0].song_key, "halo\u{1F}beyonce");
    Ok(())
}

#[test]
fn playlists_check_owner_and_revision() -> TestResult {
    let (test, user, song, _dir) = seeded()?;
    let other = test.add_user("other", Role::Member, &[])?.key;
    let entries = vec![song.clone()];
    let playlist = test.write_blocking(move |tx| {
        playlists::create(
            tx,
            user,
            &NewPlaylist {
                name: String::from("Late Drive"),
                is_public: true,
                entries,
                ..NewPlaylist::default()
            },
        )
    })?;
    assert_eq!(playlist.song_count, 1);
    assert_eq!(playlist.duration_ms, 180_000);
    let id = playlist.id.clone();
    let stale = playlist.revision;
    let moved = test.write_blocking({
        let id = id.clone();
        let song = song.clone();
        move |tx| playlists::append(tx, user, &id, &[song])
    })?;
    assert_eq!(moved.song_count, 2);
    let refused = test.write_blocking({
        let id = id.clone();
        move |tx| playlists::replace_entries(tx, user, &id, Some(stale), &[])
    });
    assert!(matches!(refused, Err(DbError::PreconditionFailed)));
    let foreign = test.write_blocking({
        let id = id.clone();
        move |tx| playlists::update(tx, other, &id, &PlaylistPatch::default())
    });
    assert!(matches!(foreign, Err(DbError::Forbidden)));
    test.write_blocking({
        let id = id.clone();
        move |tx| {
            playlists::update(
                tx,
                user,
                &id,
                &PlaylistPatch {
                    is_public: Some(false),
                    ..PlaylistPatch::default()
                },
            )
        }
    })?;
    let hidden = test.write_blocking(move |tx| playlists::delete(tx, other, &id));
    assert!(matches!(hidden, Err(DbError::NotFound)));
    Ok(())
}

#[test]
fn last_admin_cannot_be_demoted_or_deleted() -> TestResult {
    let test = TestDb::new()?;
    let admin = test.add_user("admin", Role::Admin, &[])?.key;
    let demote = users::UserPatch {
        role: Some(Role::Member),
        ..users::UserPatch::default()
    };
    let refused = test.write_blocking({
        let demote = demote.clone();
        move |tx| users::update(tx, admin, &demote)
    });
    assert!(matches!(refused, Err(DbError::Conflict(_))));
    assert!(matches!(
        test.write_blocking(move |tx| users::delete(tx, admin)),
        Err(DbError::Conflict(_))
    ));
    test.add_user("second", Role::Admin, &[])?;
    let demoted = test.write_blocking(move |tx| users::update(tx, admin, &demote))?;
    assert_eq!(demoted.role, Role::Member);
    assert!(matches!(
        test.add_user("ADMIN", Role::Member, &[]),
        Err(DbError::Conflict(_))
    ));
    let found = test.read_blocking(|c| users::by_username(c, "Admin"))?;
    assert_eq!(found.map(|u| u.key), Some(admin));
    Ok(())
}

#[test]
fn must_change_password_is_cleared_by_a_new_password() -> TestResult {
    let test = TestDb::new()?;
    let user = test.add_user("u", Role::Member, &[])?.key;
    test.write_blocking(move |tx| users::set_must_change_password(tx, user, true))?;
    assert!(
        test.read_blocking(move |c| users::get(c, user))?
            .is_some_and(|u| u.must_change_password)
    );
    test.write_blocking(move |tx| users::set_password(tx, user, "$argon2id$new"))?;
    assert!(
        test.read_blocking(move |c| users::get(c, user))?
            .is_some_and(|u| !u.must_change_password)
    );
    Ok(())
}

#[test]
fn device_login_again_revokes_old_keys_and_bumps_media_epoch() -> TestResult {
    let test = TestDb::new()?;
    let user = test.add_user("u", Role::Member, &[])?.key;
    let new = devices::NewDevice {
        client_device_id: String::from("install-1"),
        name: String::from("iPhone"),
        platform: String::from("iOS"),
        model: None,
        app_name: Some(String::from("Tracks")),
        app_version: None,
    };
    let again = new.clone();
    let device = test.write_blocking(move |tx| devices::upsert_for_login(tx, user, &new, None))?;
    assert_eq!(device.platform, "ios");
    let key = test.write_blocking(move |tx| {
        keys::insert(
            tx,
            &keys::NewKey {
                user,
                device: Some(device.key),
                kind: keys::KeyKind::Device,
                name: String::from("iPhone"),
                prefix: String::from("tsk_abcdef"),
                secret_hash: [7; 32],
                secret_sealed: None,
                scopes: String::from("library stream"),
                expires_at: None,
            },
        )
    })?;
    assert!(
        test.read_blocking(|c| keys::by_hash(c, &[7; 32]))?
            .is_some()
    );
    let renewed =
        test.write_blocking(move |tx| devices::upsert_for_login(tx, user, &again, None))?;
    assert_eq!(renewed.key, device.key);
    assert_eq!(renewed.media_epoch, device.media_epoch + 1);
    assert!(
        test.read_blocking(|c| keys::by_hash(c, &[7; 32]))?
            .is_none()
    );
    let other = test.add_user("v", Role::Member, &[])?.key;
    let id = key.id.clone();
    assert!(matches!(
        test.write_blocking(move |tx| keys::revoke(tx, Some(other), &id)),
        Err(DbError::NotFound)
    ));
    Ok(())
}

#[test]
fn sessions_invites_resets_and_pairings_round_trip() -> TestResult {
    let test = TestDb::new()?;
    let user = test.add_user("u", Role::Admin, &[])?.key;
    let now = Clock::now(&*test.clock);
    let later = now.saturating_add(Duration::from_secs(3600));
    let session = test.write_blocking(move |tx| {
        sessions::insert(
            tx,
            &sessions::NewSession {
                token_hash: [1; 32],
                user,
                idle_expires_at: later,
                expires_at: later,
                ip: None,
                user_agent: None,
            },
        )
    })?;
    assert_eq!(
        test.read_blocking(|c| sessions::by_hash(c, &[1; 32]))?
            .map(|s| s.key),
        Some(session.key)
    );
    let invite = test.write_blocking(move |tx| {
        invites::insert(
            tx,
            &invites::NewInvite {
                token_hash: [2; 32],
                created_by: Some(user),
                role: Role::Member,
                permissions: trackside_core::Permissions::default(),
                note: None,
                libraries: Vec::new(),
                expires_at: later,
            },
        )
    })?;
    let id = invite.id.clone();
    test.write_blocking(move |tx| invites::accept(tx, &id, user))?;
    let id = invite.id.clone();
    assert!(matches!(
        test.write_blocking(move |tx| invites::accept(tx, &id, user)),
        Err(DbError::Conflict(_))
    ));
    let reset =
        test.write_blocking(move |tx| password_resets::insert(tx, &[3; 32], user, None, later))?;
    test.write_blocking(move |tx| password_resets::mark_used(tx, reset.id))?;
    assert!(matches!(
        test.write_blocking(move |tx| password_resets::mark_used(tx, reset.id)),
        Err(DbError::Conflict(_))
    ));
    let device = devices::NewDevice {
        client_device_id: String::from("install-9"),
        name: String::from("Mac"),
        platform: String::from("macos"),
        model: Some(String::from("Mac15,3")),
        app_name: None,
        app_version: None,
    };
    let expected = device.clone();
    let pairing = test.write_blocking(move |tx| {
        pairings::insert(
            tx,
            &pairings::NewPairing {
                kind: pairings::PairingKind::Request,
                user: None,
                code_hash: [4; 32],
                qr_secret_hash: None,
                poll_secret_hash: Some([5; 32]),
                device_name: None,
                platform: None,
                device: Some(device),
                expires_at: later,
            },
        )
    })?;
    assert_eq!(pairing.device, Some(expected));
    assert_eq!(pairing.device_name.as_deref(), Some("Mac"));
    let id = pairing.id.clone();
    assert_eq!(
        test.write_blocking(move |tx| pairings::fail_attempt(tx, &id))?,
        1
    );
    let id = pairing.id.clone();
    test.write_blocking(move |tx| pairings::claim(tx, &id))?;
    let id = pairing.id.clone();
    assert!(matches!(
        test.write_blocking(move |tx| pairings::claim(tx, &id)),
        Err(DbError::Conflict(_))
    ));
    test.clock.advance(Duration::from_secs(7200));
    let now = Clock::now(&*test.clock);
    assert_eq!(test.write_blocking(move |tx| pairings::prune(tx, now))?, 1);
    assert_eq!(test.write_blocking(move |tx| sessions::prune(tx, now))?, 1);
    Ok(())
}

#[test]
fn idempotency_keeps_the_first_response() -> TestResult {
    let test = TestDb::new()?;
    let user = test.add_user("u", Role::Member, &[])?.key;
    let stored = |status: u16| idempotency::StoredResponse {
        request_hash: [9; 32],
        status,
        body: b"{}".to_vec(),
        created_at: TEST_START,
    };
    let (first, second) = (stored(201), stored(500));
    test.write_blocking(move |tx| {
        idempotency::put(tx, user, "k", &first)?;
        idempotency::put(tx, user, "k", &second)
    })?;
    let found = test.read_blocking(move |c| idempotency::get(c, user, "k"))?;
    assert_eq!(found.map(|r| r.status), Some(201));
    let cutoff = TEST_START.saturating_add(Duration::from_secs(1));
    assert_eq!(
        test.write_blocking(move |tx| idempotency::prune(tx, cutoff))?,
        1
    );
    Ok(())
}

#[test]
fn stars_list_pages_most_recent_first() -> TestResult {
    let (test, user, song, dir) = seeded()?;
    let lib = test.read_blocking(|c| Ok(trackside_core::repo::libraries::all(c)?[0].key))?;
    let more = test.ingest(
        lib,
        &[
            scanned("b.flac", "B", "X", None),
            scanned("c.flac", "C", "X", None),
        ],
    )?;
    let mut ids = vec![song];
    ids.extend(more.iter().map(|r| r.song().clone()));
    let star_ids = ids.clone();
    test.write_blocking(move |tx| {
        for (i, id) in star_ids.iter().enumerate() {
            trackside_core::repo::stars::set(
                tx,
                user,
                &ItemRef::Song(id.clone()),
                Millis(i as i64 + 1),
            )?;
        }
        Ok(())
    })?;
    let access = test.access(user)?;
    let first = {
        let access = access.clone();
        test.read_blocking(move |c| {
            trackside_core::repo::stars::list(c, &access, ItemKind::Song, None, 2)
        })?
    };
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.items[0].item, ItemRef::Song(ids[2].clone()));
    let cursor = first.next.ok_or(DbError::NotFound)?;
    let second = test.read_blocking(move |c| {
        trackside_core::repo::stars::list(c, &access, ItemKind::Song, Some(&cursor), 2)
    })?;
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].item, ItemRef::Song(ids[0].clone()));
    assert!(second.next.is_none());
    drop(dir);
    let _ = (Permissions::default(), Access::system(), ids[0].as_str());
    Ok(())
}

#[test]
fn abandoned_uploads_are_pruned_with_their_staging_files() -> TestResult {
    use trackside_core::records::{ImportSource, ImportState};
    use trackside_core::repo::imports::{self, NewImport};
    let test = TestDb::new()?;
    let user = test.add_user("luke", Role::Member, &[])?.key;
    let new = |name: &str| NewImport {
        user: Some(user),
        source: ImportSource::Upload,
        original_name: format!("{name}.flac"),
        staged_path: Some(std::path::PathBuf::from(format!("/staging/{name}.part"))),
        expected_bytes: Some(10),
        target_library: None,
    };
    let old = test
        .db
        .write_blocking(move |tx| imports::create(tx, &new("old")))?;
    let done = test.db.write_blocking({
        let row = new("done");
        move |tx| {
            let row = imports::create(tx, &row)?;
            imports::finish(tx, &row.id, ImportState::Imported, None, None)
        }
    })?;
    test.clock.advance(Duration::from_hours(2 * 24));
    let fresh = test
        .db
        .write_blocking(move |tx| imports::create(tx, &new("fresh")))?;
    let cutoff = test.clock.now().saturating_sub(Duration::from_hours(24));
    let pruned = test
        .db
        .write_blocking(move |tx| imports::prune_abandoned(tx, cutoff))?;
    assert_eq!(pruned, [std::path::PathBuf::from("/staging/old.part")]);
    let left = |id| test.db.read_blocking(move |c| imports::get(c, &id));
    assert!(left(old.id)?.is_none());
    assert!(left(done.id)?.is_some());
    assert!(left(fresh.id)?.is_some());
    Ok(())
}

#[test]
fn finished_scan_runs_are_kept_to_a_bounded_history() -> TestResult {
    use trackside_core::control::ScanTrigger;
    use trackside_core::records::{ScanCounts, ScanRunKind, ScanState};
    use trackside_core::repo::scan_runs::{self, KEEP_RUNS, ScanErrorRow};

    let test = TestDb::new()?;
    let dir = tempfile::tempdir()?;
    let lib = test.add_library("Music", dir.path())?.key;
    let running = test.write_blocking(move |tx| {
        scan_runs::start(tx, lib, ScanRunKind::Full, ScanTrigger::Admin)
    })?;
    for i in 0..KEEP_RUNS + 20 {
        test.write_blocking(move |tx| {
            let run = scan_runs::start(tx, lib, ScanRunKind::Quick, ScanTrigger::Schedule)?;
            scan_runs::add_error(
                tx,
                &run.id,
                &ScanErrorRow {
                    rel_path: format!("e{i}").into_bytes(),
                    path_display: format!("e{i}"),
                    error: String::from("unreadable"),
                },
            )?;
            scan_runs::finish(tx, &run.id, ScanState::Done, &ScanCounts::default(), None)
        })?;
    }
    let (runs, errors, still_running) = test.read_blocking(move |c| {
        let count = |sql: &str| c.query_row(sql, [], |row| row.get::<_, i64>(0));
        Ok((
            count("SELECT count(*) FROM scan_runs")?,
            count("SELECT count(*) FROM scan_errors")?,
            scan_runs::get(c, &running.id)?.map(|r| r.state),
        ))
    })?;
    assert_eq!(runs, i64::from(KEEP_RUNS) + 1);
    assert_eq!(errors, i64::from(KEEP_RUNS));
    assert_eq!(still_running, Some(ScanState::Running));
    Ok(())
}

fn link(n: u8, expires_at: Millis) -> device_links::NewLink {
    device_links::NewLink {
        device_code_hash: [n; 32],
        user_code_locator: [n; 32],
        user_code_hash: [n.wrapping_add(100); 32],
        device: devices::NewDevice {
            client_device_id: format!("tv-{n}"),
            name: String::from("Living Room TV"),
            platform: String::from("android_tv"),
            model: Some(String::from("Chromecast with Google TV")),
            app_name: None,
            app_version: None,
        },
        nearby: true,
        interval_ms: 5000,
        expires_at,
    }
}

#[test]
fn device_links_lock_decide_once_and_complete_once() -> TestResult {
    let test = TestDb::new()?;
    let user = test.add_user("luke", Role::Member, &[])?.key;
    let expires = test.clock.now().saturating_add(Duration::from_mins(10));
    let new = link(1, expires);
    let row = test.write_blocking(move |tx| device_links::insert(tx, &new))?;
    assert_eq!(row.state, device_links::LinkState::Pending);
    assert_eq!(row.device.platform, "android_tv");
    assert!(row.nearby);
    let clash = link(1, expires);
    assert!(matches!(
        test.write_blocking(move |tx| device_links::insert(tx, &clash)),
        Err(DbError::Conflict(_))
    ));
    let found = test.read_blocking(|c| device_links::by_locator(c, &[1; 32]))?;
    assert_eq!(found.map(|r| r.key), Some(row.key));
    let by_code = test.read_blocking(|c| device_links::by_device_code(c, &[1; 32]))?;
    assert_eq!(by_code.map(|r| r.key), Some(row.key));
    let now = test.clock.now();
    assert_eq!(
        test.read_blocking(move |c| device_links::count_pending(c, now))?,
        1
    );

    let key = row.key;
    test.write_blocking(move |tx| device_links::decide(tx, key, user, Some("library stream")))?;
    assert!(matches!(
        test.write_blocking(move |tx| device_links::decide(tx, key, user, None)),
        Err(DbError::Conflict(_))
    ));
    let decided = test
        .read_blocking(move |c| device_links::get(c, key))?
        .ok_or("gone")?;
    assert_eq!(decided.state, device_links::LinkState::Approved);
    assert_eq!(decided.scopes.as_deref(), Some("library stream"));
    test.write_blocking(move |tx| device_links::complete(tx, key))?;
    assert!(matches!(
        test.write_blocking(move |tx| device_links::complete(tx, key)),
        Err(DbError::Conflict(_))
    ));

    let other = link(2, expires);
    let other = test
        .write_blocking(move |tx| device_links::insert(tx, &other))?
        .key;
    for n in 1..=5 {
        assert_eq!(
            test.write_blocking(move |tx| device_links::fail_attempt(tx, other, 5))?,
            n
        );
    }
    let locked = test
        .read_blocking(move |c| device_links::get(c, other))?
        .ok_or("gone")?;
    assert_eq!(locked.state, device_links::LinkState::Locked);
    assert_eq!(
        test.write_blocking(move |tx| device_links::fail_attempt(tx, other, 5))?,
        5,
        "a locked link counts no further"
    );
    Ok(())
}

#[test]
fn an_expired_device_link_frees_its_code() -> TestResult {
    let test = TestDb::new()?;
    let expires = test.clock.now().saturating_add(Duration::from_mins(10));
    let first = link(1, expires);
    test.write_blocking(move |tx| device_links::insert(tx, &first))?;
    test.clock.advance(Duration::from_mins(10));
    let mut again = link(1, test.clock.now().saturating_add(Duration::from_mins(10)));
    again.device_code_hash = [9; 32];
    test.write_blocking(move |tx| device_links::insert(tx, &again))?;
    let now = test.clock.now();
    assert_eq!(
        test.read_blocking(move |c| device_links::count_pending(c, now))?,
        1
    );
    assert_eq!(
        test.write_blocking(move |tx| device_links::prune(
            tx,
            now.saturating_add(Duration::from_mins(11))
        ))?,
        1
    );
    Ok(())
}
