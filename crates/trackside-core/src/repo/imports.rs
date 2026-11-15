//! Uploads and inbox imports.

use std::path::PathBuf;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::sql::{
    OrderKey, after_condition, bytes_path, clamp_limit, decode_keyset, encode_keyset, from_u64,
    order_by, path_bytes, read_keys, to_u64,
};
use crate::db::{DbError, WriteTx};
use crate::ids::{ImportId, LibraryKey, PublicId, SongId, UserKey, random_id};
use crate::records::{ImportSource, ImportState, Page};
use crate::time::Millis;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRow {
    pub id: ImportId,
    pub user: Option<UserKey>,
    pub source: ImportSource,
    pub original_name: String,
    pub staged_path: Option<PathBuf>,
    pub expected_bytes: Option<u64>,
    pub received_bytes: u64,
    pub state: ImportState,
    pub target_library: Option<LibraryKey>,
    pub song: Option<SongId>,
    pub error: Option<String>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewImport {
    pub user: Option<UserKey>,
    pub source: ImportSource,
    pub original_name: String,
    pub staged_path: Option<PathBuf>,
    pub expected_bytes: Option<u64>,
    pub target_library: Option<LibraryKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportFilter {
    pub user: Option<UserKey>,
    pub state: Option<ImportState>,
    pub cursor: Option<String>,
    pub limit: u32,
}

const COLUMNS: &str = "SELECT pid, user_id, source, original_name, staged_path, expected_bytes,
        received_bytes, state, target_library_id, song_pid, error, created_at, updated_at";
const SELECT: &str = "SELECT pid, user_id, source, original_name, staged_path, expected_bytes,
        received_bytes, state, target_library_id, song_pid, error, created_at, updated_at
    FROM imports";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRow> {
    let source: String = row.get(2)?;
    let staged: Option<Vec<u8>> = row.get(4)?;
    let state: String = row.get(7)?;
    let song: Option<String> = row.get(9)?;
    Ok(ImportRow {
        id: row.get(0)?,
        user: row.get(1)?,
        source: ImportSource::parse(&source).unwrap_or(ImportSource::Upload),
        original_name: row.get(3)?,
        staged_path: staged.as_deref().map(bytes_path),
        expected_bytes: row.get::<_, Option<i64>>(5)?.map(to_u64),
        received_bytes: to_u64(row.get(6)?),
        state: ImportState::parse(&state).unwrap_or(ImportState::Failed),
        target_library: row.get(8)?,
        song: song.and_then(|s| SongId::parse(&s).ok()),
        error: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

pub fn create(tx: &mut WriteTx<'_>, new: &NewImport) -> Result<ImportRow, DbError> {
    let id: ImportId = random_id();
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO imports (pid, user_id, source, original_name, staged_path, expected_bytes,
             state, target_library_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'receiving', ?7, ?8, ?8)",
    )?
    .execute(params![
        id,
        new.user,
        new.source.as_str(),
        new.original_name,
        new.staged_path.as_deref().map(path_bytes),
        new.expected_bytes.map(from_u64),
        new.target_library,
        now
    ])?;
    get(tx, &id)?.ok_or(DbError::NotFound)
}

pub fn get(c: &Connection, id: &ImportId) -> Result<Option<ImportRow>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} WHERE pid = ?1"))?
        .query_row([id], map_row)
        .optional()?)
}

/// Records received bytes of an upload in `receiving`.
pub fn progress(tx: &mut WriteTx<'_>, id: &ImportId, received_bytes: u64) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE imports SET received_bytes = ?2, updated_at = ?3
             WHERE pid = ?1 AND state = 'receiving'",
        )?
        .execute(params![id, from_u64(received_bytes), tx.now()])?;
    if changed == 0 {
        return Err(if get(tx, id)?.is_some() {
            DbError::Conflict(String::from("import is not receiving"))
        } else {
            DbError::NotFound
        });
    }
    Ok(())
}

/// Moves an import to `state` with its outcome.
pub fn finish(
    tx: &mut WriteTx<'_>,
    id: &ImportId,
    state: ImportState,
    song: Option<&SongId>,
    error: Option<&str>,
) -> Result<ImportRow, DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE imports SET state = ?2, song_pid = coalesce(?3, song_pid), error = ?4,
                 updated_at = ?5 WHERE pid = ?1",
        )?
        .execute(params![id, state.as_str(), song, error, tx.now()])?;
    if changed == 0 {
        return Err(DbError::NotFound);
    }
    get(tx, id)?.ok_or(DbError::NotFound)
}

/// Deletes uploads still receiving that nothing was sent to since `before`, and returns
/// their staging files for the caller to remove.
pub fn prune_abandoned(tx: &mut WriteTx<'_>, before: Millis) -> Result<Vec<PathBuf>, DbError> {
    let staged: Vec<Option<Vec<u8>>> = tx
        .prepare_cached(
            "DELETE FROM imports WHERE state = 'receiving' AND updated_at < ?1
             RETURNING staged_path",
        )?
        .query_map([before], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(staged
        .into_iter()
        .flatten()
        .map(|b| bytes_path(&b))
        .collect())
}

pub fn delete(tx: &mut WriteTx<'_>, id: &ImportId) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM imports WHERE pid = ?1")?
        .execute([id])?;
    Ok(())
}

/// Newest first.
pub fn list(c: &Connection, filter: &ImportFilter) -> Result<Page<ImportRow>, DbError> {
    let limit = clamp_limit(filter.limit, 100, 500);
    let after = decode_keyset(filter.cursor.as_deref(), 2)?;
    let keys = [OrderKey::desc("created_at"), OrderKey::desc("id")];
    let (after_sql, after_params) = after_condition(&keys, &after);
    let mut params = vec![
        filter.user.map_or(Value::Null, |u| Value::Integer(u.0)),
        filter
            .state
            .map_or(Value::Null, |s| Value::Text(s.as_str().to_owned())),
    ];
    params.extend(after_params);
    let sql = format!(
        "{COLUMNS}, created_at, id FROM imports
         WHERE (?1 IS NULL OR user_id = ?1) AND (?2 IS NULL OR state = ?2)
         AND {after_sql} {} LIMIT {}",
        order_by(&keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<(ImportRow, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            Ok((map_row(row)?, read_keys(row, 13, 2)?))
        })?
        .collect::<Result<_, _>>()?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|(_, keys)| encode_keyset(keys))
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(|(row, _)| row).collect(),
        next,
    })
}
