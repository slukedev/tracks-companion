//! Libraries.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::{bytes_path, conflict_on_unique, path_bytes, to_u64, visible};
use crate::access::Access;
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{LibraryId, LibraryKey, PublicId, UserKey, random_id};
use crate::records::Library;

/// `POST /admin/libraries`. Path checks (exists, readable, no overlap) are the caller's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewLibrary {
    pub name: String,
    pub root: PathBuf,
    pub writable: bool,
    pub watch: bool,
    /// Grant every existing member access.
    pub grant_all_users: bool,
    pub granted_by: Option<UserKey>,
}

/// `PATCH /admin/libraries/{id}`. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryPatch {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub writable: Option<bool>,
    pub watch: Option<bool>,
    /// `Some(None)` returns to `scanner.quick_interval`.
    pub quick_scan_interval: Option<Option<Duration>>,
}

/// The empty-library guard's memory of the last healthy scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootHealth {
    pub root_dev: Option<u64>,
    pub root_entry_count: Option<u64>,
}

const SELECT: &str = "SELECT l.id, l.pid, l.name, l.root, l.enabled, l.writable, l.watch, l.state,
        l.song_count, l.quick_scan_interval_s, l.unavailable_reason, l.unavailable_since,
        l.last_scan_at, l.created_at
    FROM libraries l";

fn map_row(row: &Row<'_>) -> rusqlite::Result<Library> {
    let root: Vec<u8> = row.get(3)?;
    let state: String = row.get(7)?;
    let interval: Option<i64> = row.get(9)?;
    Ok(Library {
        key: row.get(0)?,
        id: row.get(1)?,
        name: row.get(2)?,
        root: bytes_path(&root),
        enabled: row.get(4)?,
        writable: row.get(5)?,
        watch: row.get(6)?,
        available: state == "ok",
        song_count: to_u64(row.get(8)?),
        quick_scan_interval: interval.map(|s| Duration::from_secs(to_u64(s))),
        unavailable_reason: row.get(10)?,
        unavailable_since: row.get(11)?,
        last_scan_at: row.get(12)?,
        created_at: row.get(13)?,
    })
}

fn load(
    c: &Connection,
    tail: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Library>, DbError> {
    let mut statement = c.prepare_cached(&format!("{SELECT} {tail}"))?;
    let rows = statement.query_map(params, map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Libraries the caller can see, by name.
pub fn list(c: &Connection, a: &Access) -> Result<Vec<Library>, DbError> {
    load(
        c,
        &format!(
            "WHERE {} ORDER BY l.name COLLATE NOCASE, l.id",
            visible(a, "l.id", None)
        ),
        &[],
    )
}

pub fn get(c: &Connection, a: &Access, id: &LibraryId) -> Result<Option<Library>, DbError> {
    Ok(load(
        c,
        &format!("WHERE l.pid = ?1 AND {}", visible(a, "l.id", None)),
        &[id],
    )?
    .into_iter()
    .next())
}

/// By internal key, ignoring access. For the scanner and the server.
pub fn by_key(c: &Connection, key: LibraryKey) -> Result<Option<Library>, DbError> {
    Ok(load(c, "WHERE l.id = ?1", &[&key])?.into_iter().next())
}

/// Every library, ignoring access. For the scanner and the server.
pub fn all(c: &Connection) -> Result<Vec<Library>, DbError> {
    load(c, "ORDER BY l.id", &[])
}

/// The library at exactly this root, ignoring access.
pub fn by_root(c: &Connection, root: &std::path::Path) -> Result<Option<Library>, DbError> {
    let bytes = path_bytes(root);
    Ok(load(c, "WHERE l.root = ?1", &[&bytes])?.into_iter().next())
}

/// The internal key of a library by public id, ignoring access.
pub fn key_of(c: &Connection, id: &LibraryId) -> Result<Option<LibraryKey>, DbError> {
    Ok(c.prepare_cached("SELECT id FROM libraries WHERE pid = ?1")?
        .query_row([id], |row| row.get(0))
        .optional()?)
}

fn record(tx: &mut WriteTx<'_>, key: LibraryKey, op: ChangeOp) -> Result<(), DbError> {
    let pid: String = tx
        .prepare_cached("SELECT pid FROM libraries WHERE id = ?1")?
        .query_row([key], |row| row.get(0))?;
    tx.record(Change::shared(ChangeKind::Library, pid, Some(key), op))?;
    Ok(())
}

pub fn create(tx: &mut WriteTx<'_>, new: &NewLibrary) -> Result<Library, DbError> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(DbError::Invalid(String::from("name")));
    }
    let id: LibraryId = random_id();
    let now = tx.now();
    let root = path_bytes(&new.root);
    tx.prepare_cached(
        "INSERT INTO libraries (pid, name, root, root_display, writable, watch, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
    )?
    .execute(params![
        id,
        name,
        root,
        new.root.to_string_lossy(),
        new.writable,
        new.watch,
        now
    ])
    .map_err(|e| conflict_on_unique(e, "a library already uses this folder"))?;
    let key = LibraryKey(tx.last_insert_rowid());
    if new.grant_all_users {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO library_access (user_id, library_id, granted_at, granted_by)
             SELECT id, ?1, ?2, ?3 FROM users WHERE role = 'member'",
        )?
        .execute(params![key, now, new.granted_by])?;
    }
    record(tx, key, ChangeOp::Upsert)?;
    by_key(tx, key)?.ok_or(DbError::NotFound)
}

pub fn update(
    tx: &mut WriteTx<'_>,
    key: LibraryKey,
    patch: &LibraryPatch,
) -> Result<Library, DbError> {
    let current = by_key(tx, key)?.ok_or(DbError::NotFound)?;
    let name = match &patch.name {
        Some(name) if name.trim().is_empty() => return Err(DbError::Invalid(String::from("name"))),
        Some(name) => name.trim().to_owned(),
        None => current.name.clone(),
    };
    let interval = match patch.quick_scan_interval {
        Some(value) => value,
        None => current.quick_scan_interval,
    };
    tx.prepare_cached(
        "UPDATE libraries SET name = ?2, enabled = ?3, writable = ?4, watch = ?5,
             quick_scan_interval_s = ?6, updated_at = ?7
         WHERE id = ?1",
    )?
    .execute(params![
        key,
        name,
        patch.enabled.unwrap_or(current.enabled),
        patch.writable.unwrap_or(current.writable),
        patch.watch.unwrap_or(current.watch),
        interval.map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
        tx.now()
    ])?;
    let updated = by_key(tx, key)?.ok_or(DbError::NotFound)?;
    if updated != current {
        record(tx, key, ChangeOp::Upsert)?;
    }
    Ok(updated)
}

/// Removes the library and its songs from the index (files are never touched) and bumps every
/// affected user's sync epoch.
pub fn delete(tx: &mut WriteTx<'_>, key: LibraryKey) -> Result<(), DbError> {
    let current = by_key(tx, key)?.ok_or(DbError::NotFound)?;
    let now = tx.now();
    tx.prepare_cached(
        "UPDATE users SET sync_epoch = sync_epoch + 1, updated_at = ?2
         WHERE role = 'admin' OR id IN (SELECT user_id FROM library_access WHERE library_id = ?1)",
    )?
    .execute(params![key, now])?;
    let artists: Vec<i64> = tx
        .prepare_cached("SELECT artist_id FROM artist_libraries WHERE library_id = ?1")?
        .query_map([key], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    tx.prepare_cached("DELETE FROM change_log WHERE library_id = ?1")?
        .execute([key])?;
    // Songs reference folders, so they go first; cascades remove credits, genres and lyrics.
    tx.prepare_cached("DELETE FROM songs WHERE library_id = ?1")?
        .execute([key])?;
    tx.prepare_cached("DELETE FROM libraries WHERE id = ?1")?
        .execute([key])?;
    tx.record(Change::shared(
        ChangeKind::Library,
        current.id.as_str(),
        Some(key),
        ChangeOp::Delete,
    ))?;
    crate::ingest::refresh_artists(tx, artists)?;
    Ok(())
}

/// Records the availability monitor's verdict. Returns whether the state changed.
pub fn set_available(
    tx: &mut WriteTx<'_>,
    key: LibraryKey,
    available: bool,
    reason: Option<&str>,
) -> Result<bool, DbError> {
    let now = tx.now();
    let changed = if available {
        tx.prepare_cached(
            "UPDATE libraries SET state = 'ok', unavailable_reason = NULL, unavailable_since = NULL,
                 updated_at = ?2
             WHERE id = ?1 AND state != 'ok'",
        )?
        .execute(params![key, now])?
    } else {
        tx.prepare_cached(
            "UPDATE libraries SET state = 'unavailable', unavailable_reason = ?2,
                 unavailable_since = ?3, updated_at = ?3
             WHERE id = ?1 AND state != 'unavailable'",
        )?
        .execute(params![key, reason, now])?
    };
    let reason_changed = changed == 0
        && !available
        && tx
            .prepare_cached(
                "UPDATE libraries SET unavailable_reason = ?2
                 WHERE id = ?1 AND unavailable_reason IS NOT ?2",
            )?
            .execute(params![key, reason])?
            > 0;
    if changed > 0 || reason_changed {
        record(tx, key, ChangeOp::Upsert)?;
    }
    Ok(changed > 0)
}

/// Remembers the root's device and entry count after a healthy scan, for the empty-library
/// guard.
pub fn set_root_health(
    tx: &mut WriteTx<'_>,
    key: LibraryKey,
    root_dev: Option<u64>,
    root_entry_count: u64,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE libraries SET root_dev = ?2, root_entry_count = ?3, last_scan_at = ?4 WHERE id = ?1",
    )?
    .execute(params![
        key,
        root_dev.map(super::sql::from_u64),
        super::sql::from_u64(root_entry_count),
        tx.now()
    ])?;
    record(tx, key, ChangeOp::Upsert)
}

/// What [`set_root_health`] last stored.
pub fn root_health(c: &Connection, key: LibraryKey) -> Result<Option<RootHealth>, DbError> {
    Ok(
        c.prepare_cached("SELECT root_dev, root_entry_count FROM libraries WHERE id = ?1")?
            .query_row([key], |row| {
                Ok(RootHealth {
                    root_dev: row.get::<_, Option<i64>>(0)?.map(to_u64),
                    root_entry_count: row.get::<_, Option<i64>>(1)?.map(to_u64),
                })
            })
            .optional()?,
    )
}

/// How many present songs the library has, as known to the index.
pub fn present_songs(c: &Connection, key: LibraryKey) -> Result<u64, DbError> {
    let count: i64 = c
        .prepare_cached(
            "SELECT count(*) FROM songs WHERE library_id = ?1 AND missing_since IS NULL",
        )?
        .query_row([key], |row| row.get(0))?;
    Ok(to_u64(count))
}
