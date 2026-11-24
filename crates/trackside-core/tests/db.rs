//! The database layer: migration 1, the change log, event publishing, readers, backups.

use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::broadcast::error::TryRecvError;
use trackside_core::db::{FilesystemCheck, SCHEMA_VERSION};
use trackside_core::repo::settings;
use trackside_core::testing::{TEST_START, TestDb};
use trackside_core::{
    Audience, Change, ChangeKind, ChangeOp, Db, DbError, DbOptions, EventBus, LibraryKey, Seq,
    ServerEvent, TestClock, UserKey,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn options(dir: &tempfile::TempDir) -> DbOptions {
    let mut options = DbOptions::new(dir.path().join("data").join("trackside.db"));
    options.filesystem = FilesystemCheck::AssumeLocal;
    options.readers = 2;
    options
}

fn open(options: &DbOptions) -> Result<Db, DbError> {
    Db::open(
        options,
        EventBus::default(),
        Arc::new(TestClock::new(TEST_START)),
    )
}

fn table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
    statement.query_map([], |row| row.get(0))?.collect()
}

#[test]
fn migrations_apply_to_an_empty_database() -> TestResult {
    let dir = tempfile::tempdir()?;
    let db = open(&options(&dir))?;
    assert_eq!(db.schema_version()?, trackside_core::db::SCHEMA_VERSION);
    let tables = db.read_blocking(|c| Ok(table_names(c)?))?;
    for expected in [
        "users",
        "sessions",
        "devices",
        "api_keys",
        "invites",
        "invite_libraries",
        "password_resets",
        "pairings",
        "audit_log",
        "idempotency_keys",
        "libraries",
        "library_access",
        "folders",
        "artwork",
        "artists",
        "artist_libraries",
        "albums",
        "genres",
        "songs",
        "song_artists",
        "song_genres",
        "lyrics",
        "songs_fts",
        "albums_fts",
        "artists_fts",
        "similar_artists",
        "similar_songs",
        "listens",
        "play_stats",
        "stats_hourly",
        "stats_daily",
        "stars",
        "ratings",
        "bookmarks",
        "playlists",
        "playlist_entries",
        "play_queues",
        "play_queue_entries",
        "change_log",
        "scan_runs",
        "scan_errors",
        "scan_quarantine",
        "imports",
        "acquisitions",
        "acquisition_candidates",
        "settings",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}"
        );
    }
    let setup: Option<String> =
        db.read_blocking(|c| settings::get(c, settings::keys::SETUP_STATE))?;
    assert_eq!(setup.as_deref(), Some("needs_admin"));
    let pruned: Option<i64> =
        db.read_blocking(|c| settings::get(c, settings::keys::SYNC_PRUNED_THROUGH))?;
    assert_eq!(pruned, Some(0));
    let folding: Option<u32> =
        db.read_blocking(|c| settings::get(c, settings::keys::SCHEMA_TRACKS_MODEL))?;
    assert_eq!(folding, Some(trackside_core::TRACKS_MODEL_FOLDING));
    db.read_blocking(settings::server_id)?;
    Ok(())
}

#[test]
fn migration_1_is_idempotent_on_reopen() -> TestResult {
    let dir = tempfile::tempdir()?;
    let options = options(&dir);
    let first = open(&options)?;
    let server_id = first.read_blocking(settings::server_id)?;
    first.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Library,
            "l1",
            Some(LibraryKey(1)),
            ChangeOp::Upsert,
        ))?;
        Ok(())
    })?;
    drop(first);

    let second = open(&options)?;
    assert_eq!(second.schema_version()?, SCHEMA_VERSION);
    assert_eq!(second.read_blocking(settings::server_id)?, server_id);
    let rows: i64 = second
        .read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM change_log", [], |r| r.get(0))?))?;
    assert_eq!(rows, 1);
    let seq = second.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Library,
            "l2",
            None,
            ChangeOp::Upsert,
        ))
    })?;
    assert_eq!(seq, Seq(2), "next_seq continues from the stored maximum");
    Ok(())
}

#[test]
fn opening_a_newer_database_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let options = options(&dir);
    drop(open(&options)?);
    Connection::open(&options.path)?.execute_batch("PRAGMA user_version = 7")?;
    assert_eq!(Db::inspect_schema_version(&options.path)?, Some(7));
    let refused = open(&options);
    assert!(
        matches!(
            refused,
            Err(DbError::TooNew {
                found: 7,
                known: SCHEMA_VERSION
            })
        ),
        "{refused:?}"
    );
    Ok(())
}

#[test]
fn inspecting_a_missing_file_finds_no_version() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert_eq!(
        Db::inspect_schema_version(&dir.path().join("none.db"))?,
        None
    );
    Ok(())
}

#[test]
fn database_on_a_network_filesystem_is_refused_unless_allowed() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut options = options(&dir);
    options.filesystem = FilesystemCheck::AssumeNetwork;
    assert!(matches!(open(&options), Err(DbError::NetworkFilesystem)));
    assert!(!options.path.exists(), "nothing is created on refusal");
    options.allow_network_fs = true;
    open(&options)?;
    Ok(())
}

#[test]
fn change_log_keeps_one_row_per_entity_with_increasing_seq() -> TestResult {
    let test = TestDb::new()?;
    let user = UserKey(5);
    let seqs = test.write_blocking(move |tx| {
        let song = tx.record(Change::shared(
            ChangeKind::Song,
            "s1",
            Some(LibraryKey(1)),
            ChangeOp::Upsert,
        ))?;
        let star = tx.record(Change::for_user(
            ChangeKind::Star,
            "song:s1",
            user,
            ChangeOp::Upsert,
        ))?;
        let song_again = tx.record(Change::shared(
            ChangeKind::Song,
            "s1",
            Some(LibraryKey(1)),
            ChangeOp::Delete,
        ))?;
        Ok(vec![song, star, song_again])
    })?;
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    let later = test.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Song,
            "s2",
            Some(LibraryKey(1)),
            ChangeOp::Upsert,
        ))
    })?;
    assert!(later > seqs[2]);

    let rows: Vec<(i64, String, String, i64, String)> = test.read_blocking(|c| {
        let mut statement =
            c.prepare("SELECT seq, kind, entity_pid, user_id, op FROM change_log ORDER BY seq")?;
        let rows = statement
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })?;
    assert_eq!(
        rows,
        vec![
            (
                seqs[1].0,
                "star".into(),
                "song:s1".into(),
                5,
                "upsert".into()
            ),
            (seqs[2].0, "song".into(), "s1".into(), 0, "delete".into()),
            (later.0, "song".into(), "s2".into(), 0, "upsert".into()),
        ]
    );
    Ok(())
}

#[test]
fn the_same_entity_for_two_users_keeps_two_rows() -> TestResult {
    let test = TestDb::new()?;
    test.write_blocking(|tx| {
        tx.record(Change::for_user(
            ChangeKind::Playlist,
            "p1",
            UserKey(1),
            ChangeOp::Upsert,
        ))?;
        tx.record(Change::shared(
            ChangeKind::Playlist,
            "p1",
            None,
            ChangeOp::Delete,
        ))?;
        Ok(())
    })?;
    let rows: i64 = test
        .read_blocking(|c| Ok(c.query_row("SELECT count(*) FROM change_log", [], |r| r.get(0))?))?;
    assert_eq!(rows, 2);
    Ok(())
}

#[tokio::test]
async fn write_publishes_changed_after_commit_and_not_after_rollback() -> TestResult {
    let test = TestDb::new()?;
    let mut events = test.events.subscribe();

    let seq = test
        .write(|tx| {
            tx.record(Change::shared(
                ChangeKind::Song,
                "s1",
                Some(LibraryKey(2)),
                ChangeOp::Upsert,
            ))?;
            tx.record(Change::for_user(
                ChangeKind::Star,
                "song:s1",
                UserKey(9),
                ChangeOp::Upsert,
            ))
        })
        .await?;
    let event = events.try_recv()?;
    match &*event {
        ServerEvent::Changed {
            seq: published,
            audiences,
        } => {
            assert_eq!(*published, seq);
            assert_eq!(
                audiences,
                &vec![
                    Audience::Everyone {
                        library: Some(LibraryKey(2))
                    },
                    Audience::User(UserKey(9))
                ]
            );
        }
        other => panic!("unexpected event {other:?}"),
    }

    let failed: Result<(), DbError> = test
        .write(|tx| {
            tx.record(Change::shared(
                ChangeKind::Song,
                "s2",
                None,
                ChangeOp::Upsert,
            ))?;
            Err(DbError::Conflict(String::from("changed my mind")))
        })
        .await;
    assert!(matches!(failed, Err(DbError::Conflict(_))));
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    let rows: i64 = test
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM change_log WHERE entity_pid = 's2'",
                [],
                |r| r.get(0),
            )?)
        })
        .await?;
    assert_eq!(rows, 0, "the rolled back change is not stored");

    test.write(|tx| {
        tx.execute("UPDATE settings SET updated_at = updated_at", [])?;
        Ok(())
    })
    .await?;
    assert!(
        matches!(events.try_recv(), Err(TryRecvError::Empty)),
        "a write without changes publishes nothing"
    );
    Ok(())
}

#[test]
fn rolled_back_seqs_are_reused_but_never_seen() -> TestResult {
    let test = TestDb::new()?;
    let _ = test.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Album,
            "a1",
            None,
            ChangeOp::Upsert,
        ))?;
        Err::<(), _>(DbError::Invalid(String::from("no")))
    });
    let seq = test.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Album,
            "a2",
            None,
            ChangeOp::Upsert,
        ))
    })?;
    assert_eq!(seq, Seq(1));
    Ok(())
}

#[test]
fn write_tx_time_comes_from_the_clock() -> TestResult {
    let test = TestDb::new()?;
    test.clock.advance(std::time::Duration::from_secs(60));
    let now = test.write_blocking(|tx| Ok(tx.now()))?;
    assert_eq!(now.0, TEST_START.0 + 60_000);
    Ok(())
}

#[tokio::test]
async fn readers_see_committed_writes_on_a_file_database() -> TestResult {
    let dir = tempfile::tempdir()?;
    let db = open(&options(&dir))?;
    let mut handles = Vec::new();
    for i in 0..8 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            db.write(move |tx| {
                tx.record(Change::shared(
                    ChangeKind::Song,
                    format!("s{i}"),
                    None,
                    ChangeOp::Upsert,
                ))?;
                Ok(())
            })
            .await?;
            db.read(|c| {
                Ok(c.query_row("SELECT count(*) FROM change_log", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
        }));
    }
    for handle in handles {
        assert!(handle.await?? >= 1);
    }
    let total: i64 = db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM change_log", [], |r| r.get(0))?))
        .await?;
    assert_eq!(total, 8);
    Ok(())
}

#[test]
fn readers_cannot_write() -> TestResult {
    let dir = tempfile::tempdir()?;
    let db = open(&options(&dir))?;
    let attempt = db.read_blocking(|c| Ok(c.execute("DELETE FROM settings", [])?));
    assert!(attempt.is_err());
    Ok(())
}

#[test]
fn backup_into_writes_a_complete_copy_and_no_partial_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let db = open(&options(&dir))?;
    db.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Song,
            "s1",
            None,
            ChangeOp::Upsert,
        ))?;
        Ok(())
    })?;
    let target = dir.path().join("backup.db");
    db.backup_into(&target)?;
    assert!(!dir.path().join("backup.db.partial").exists());
    let copy = Connection::open(&target)?;
    let version: i64 = copy.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let rows: i64 = copy.query_row("SELECT count(*) FROM change_log", [], |r| r.get(0))?;
    assert_eq!(
        (version, rows),
        (i64::from(trackside_core::db::SCHEMA_VERSION), 1)
    );
    db.checkpoint_and_optimize()?;
    Ok(())
}

#[test]
fn foreign_keys_are_enforced() -> TestResult {
    let test = TestDb::new()?;
    let result = test.write_blocking(|tx| {
        tx.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, last_seen_at, idle_expires_at, expires_at)
             VALUES (x'00', 42, 0, 0, 0, 0)",
            [],
        )?;
        Ok(())
    });
    assert!(result.is_err());
    Ok(())
}

#[test]
fn full_text_search_tables_follow_their_rows() -> TestResult {
    let test = TestDb::new()?;
    let hits: i64 = test.write_blocking(|tx| {
        tx.execute(
            "INSERT INTO artists (pid, identity, name, sort_name, search_text, created_at, updated_at)
             VALUES ('r0000000000000000000a', 'beyonce', 'Beyoncé', 'Beyoncé', 'beyonce', 0, 0)",
            [],
        )?;
        Ok(tx.query_row("SELECT count(*) FROM artists_fts WHERE artists_fts MATCH 'bey*'", [], |r| r.get(0))?)
    })?;
    assert_eq!(hits, 1);
    Ok(())
}

#[test]
fn tracks_model_is_linked() {
    assert_eq!(
        tracks_model::song_key("Halo", "Beyoncé"),
        "halo\u{1F}beyonce"
    );
}

#[test]
fn stored_keys_use_the_current_tracks_model_folding() {
    // A change in any of these means stored keys no longer match Tracks: bump
    // TRACKS_MODEL_FOLDING and add a migration that recomputes them.
    assert_eq!(trackside_core::TRACKS_MODEL_FOLDING, 1);
    assert_eq!(
        tracks_model::song_key("Halo", "Beyoncé"),
        "halo\u{1F}beyonce"
    );
    assert_eq!(tracks_model::folded(" Straße "), "strasse");
    assert_eq!(
        tracks_model::album_key("I Am... Sasha Fierce", "beyonce").as_deref(),
        Some("i am... sasha fierce\u{1F}beyonce")
    );
    assert_eq!(
        tracks_model::search_folded("Don't Stop (AC/DC) & Co"),
        "dont stop ac dc and co"
    );
}

#[test]
fn database_built_with_other_folding_is_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let options = options(&dir);
    let db = open(&options)?;
    db.write_blocking(|tx| settings::put(tx, "schema.tracks_model", &99))?;
    drop(db);
    assert!(matches!(open(&options), Err(DbError::Migration(_))));
    Ok(())
}

#[test]
fn simulated_crash_during_migration_leaves_the_previous_schema() -> TestResult {
    let dir = tempfile::tempdir()?;
    let options = options(&dir);
    trackside_core::testing::crash_migration_of(&options.path);
    assert!(matches!(open(&options), Err(DbError::Migration(_))));
    assert_eq!(Db::inspect_schema_version(&options.path)?, Some(0));
    trackside_core::testing::clear_hooks(dir.path());
    assert_eq!(open(&options)?.schema_version()?, SCHEMA_VERSION);
    Ok(())
}

#[test]
fn assumed_network_filesystem_is_refused_on_detect() -> TestResult {
    let dir = tempfile::tempdir()?;
    let mut options = options(&dir);
    options.filesystem = FilesystemCheck::Detect;
    trackside_core::testing::assume_network_filesystem(dir.path());
    assert!(matches!(open(&options), Err(DbError::NetworkFilesystem)));
    trackside_core::testing::clear_hooks(dir.path());
    Ok(())
}

#[test]
fn integrity_check_and_current_seq() -> TestResult {
    let dir = tempfile::tempdir()?;
    let options = options(&dir);
    let db = open(&options)?;
    assert_eq!(db.current_seq(), Seq(0));
    let seq = db.write_blocking(|tx| {
        tx.record(Change::shared(
            ChangeKind::Library,
            "l1",
            None,
            ChangeOp::Upsert,
        ))
    })?;
    assert_eq!(db.current_seq(), seq);
    assert_eq!(db.integrity_check(false)?, Vec::<String>::new());
    assert_eq!(db.integrity_check(true)?, Vec::<String>::new());
    let copy = dir.path().join("copy.db");
    db.backup_into(&copy)?;
    assert_eq!(Db::check_file(&copy, true)?, Vec::<String>::new());
    std::fs::write(
        dir.path().join("junk.db"),
        b"not a database, not at all, no",
    )?;
    assert!(Db::check_file(&dir.path().join("junk.db"), false).is_err());
    Ok(())
}

/// Writes queued behind a long one wait without a blocking thread each, so reads still get one.
#[test]
fn queued_writes_do_not_take_the_blocking_pool() -> TestResult {
    let dir = tempfile::tempdir()?;
    let db = open(&options(&dir))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?;
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let long = db.clone();
    let holder = std::thread::spawn(move || {
        long.write_blocking(|_| {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Ok(())
        })
    });
    started_rx.recv()?;
    let read = runtime.block_on(async {
        let mut queued = Vec::new();
        for _ in 0..16 {
            let db = db.clone();
            queued.push(tokio::spawn(async move { db.write(|_| Ok(())).await }));
        }
        tokio::task::yield_now().await;
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            db.read(|c| Ok(settings::server_id(c)?)),
        )
        .await;
        let _ = release_tx.send(());
        for write in queued {
            let _ = write.await;
        }
        read
    });
    holder.join().map_err(|_| "writer panicked")??;
    assert!(read.is_ok(), "a read waited behind queued writes");
    Ok(())
}
