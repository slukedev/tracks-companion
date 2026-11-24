//! Scan runs, their errors, and the quarantine of files that hung or crashed the tag reader.

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::{from_u64, to_u64};
use crate::control::ScanTrigger;
use crate::db::{DbError, WriteTx};
use crate::ids::{LibraryKey, ScanRunId, random_id};
use crate::records::{ScanCounts, ScanRun, ScanRunKind, ScanState};
use crate::time::Millis;

/// One file that failed in a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanErrorRow {
    pub rel_path: Vec<u8>,
    pub path_display: String,
    pub error: String,
}

/// A file skipped until its size or mtime changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantinedFile {
    pub rel_path: Vec<u8>,
    pub file_size: u64,
    pub file_mtime_ns: i64,
    pub reason: String,
    pub at: Millis,
}

const SELECT: &str = "SELECT r.pid, l.pid, r.kind, r.trigger, r.state, r.started_at, r.finished_at,
        r.folders_seen, r.files_seen, r.added, r.updated, r.moved, r.missing, r.errors, r.message
    FROM scan_runs r JOIN libraries l ON l.id = +r.library_id";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScanRun> {
    let kind: String = row.get(2)?;
    let trigger: String = row.get(3)?;
    let state: String = row.get(4)?;
    Ok(ScanRun {
        id: row.get(0)?,
        library: row.get(1)?,
        kind: ScanRunKind::parse(&kind).unwrap_or(ScanRunKind::Quick),
        trigger: ScanTrigger::parse(&trigger).unwrap_or(ScanTrigger::Admin),
        state: ScanState::parse(&state).unwrap_or(ScanState::Failed),
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        counts: ScanCounts {
            folders_seen: to_u64(row.get(7)?),
            files_seen: to_u64(row.get(8)?),
            added: to_u64(row.get(9)?),
            updated: to_u64(row.get(10)?),
            moved: to_u64(row.get(11)?),
            missing: to_u64(row.get(12)?),
            errors: to_u64(row.get(13)?),
        },
        message: row.get(14)?,
    })
}

fn run_key(c: &Connection, id: &ScanRunId) -> Result<i64, DbError> {
    c.prepare_cached("SELECT id FROM scan_runs WHERE pid = ?1")?
        .query_row([id], |row| row.get(0))
        .optional()?
        .ok_or(DbError::NotFound)
}

pub fn start(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    kind: ScanRunKind,
    trigger: ScanTrigger,
) -> Result<ScanRun, DbError> {
    let id: ScanRunId = random_id();
    tx.prepare_cached(
        "INSERT INTO scan_runs (pid, library_id, kind, trigger, state, started_at)
         VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
    )?
    .execute(params![
        id,
        library,
        kind.as_str(),
        trigger.as_str(),
        tx.now()
    ])?;
    get(tx, &id)?.ok_or(DbError::NotFound)
}

fn write_counts(tx: &WriteTx<'_>, id: &ScanRunId, counts: &ScanCounts) -> Result<usize, DbError> {
    Ok(tx
        .prepare_cached(
            "UPDATE scan_runs SET folders_seen = ?2, files_seen = ?3, added = ?4, updated = ?5,
                 moved = ?6, missing = ?7, errors = ?8
             WHERE pid = ?1",
        )?
        .execute(params![
            id,
            from_u64(counts.folders_seen),
            from_u64(counts.files_seen),
            from_u64(counts.added),
            from_u64(counts.updated),
            from_u64(counts.moved),
            from_u64(counts.missing),
            from_u64(counts.errors)
        ])?)
}

pub fn progress(tx: &mut WriteTx<'_>, id: &ScanRunId, counts: &ScanCounts) -> Result<(), DbError> {
    if write_counts(tx, id, counts)? == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Finished runs kept per library. Quick scans every 30 minutes and a folder scan per watch
/// event would otherwise grow `scan_runs` and `scan_errors` without bound.
pub const KEEP_RUNS: u32 = 200;

pub fn finish(
    tx: &mut WriteTx<'_>,
    id: &ScanRunId,
    state: ScanState,
    counts: &ScanCounts,
    message: Option<&str>,
) -> Result<ScanRun, DbError> {
    if write_counts(tx, id, counts)? == 0 {
        return Err(DbError::NotFound);
    }
    tx.prepare_cached(
        "UPDATE scan_runs SET state = ?2, finished_at = ?3, message = ?4 WHERE pid = ?1",
    )?
    .execute(params![id, state.as_str(), tx.now(), message])?;
    prune(tx, run_key(tx, id)?)?;
    get(tx, id)?.ok_or(DbError::NotFound)
}

/// Deletes the finished runs (and with them their errors) of the library `run` belongs to,
/// beyond the newest [`KEEP_RUNS`].
fn prune(tx: &WriteTx<'_>, run: i64) -> Result<(), DbError> {
    tx.prepare_cached(
        "DELETE FROM scan_runs WHERE state != 'running'
           AND library_id = (SELECT library_id FROM scan_runs WHERE id = ?1)
           AND id NOT IN (SELECT id FROM scan_runs
                          WHERE library_id = (SELECT library_id FROM scan_runs WHERE id = ?1)
                          ORDER BY started_at DESC, id DESC LIMIT ?2)",
    )?
    .execute(params![run, i64::from(KEEP_RUNS)])?;
    Ok(())
}

/// Marks runs left `running` by a crash as `aborted`. Returns how many there were.
pub fn abort_interrupted(tx: &mut WriteTx<'_>) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached(
            "UPDATE scan_runs SET state = 'aborted', finished_at = ?1,
                 message = coalesce(message, 'interrupted by a restart')
             WHERE state = 'running'",
        )?
        .execute([tx.now()])?;
    Ok(n as u64)
}

pub fn get(c: &Connection, id: &ScanRunId) -> Result<Option<ScanRun>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} WHERE r.pid = ?1"))?
        .query_row([id], map_row)
        .optional()?)
}

/// A library's runs, newest first.
pub fn list(c: &Connection, library: LibraryKey, limit: u32) -> Result<Vec<ScanRun>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "{SELECT} WHERE r.library_id = ?1 ORDER BY r.started_at DESC, r.id DESC LIMIT ?2"
    ))?;
    let rows = statement.query_map(params![library, i64::from(limit.max(1))], map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn add_error(
    tx: &mut WriteTx<'_>,
    id: &ScanRunId,
    error: &ScanErrorRow,
) -> Result<(), DbError> {
    let run = run_key(tx, id)?;
    tx.prepare_cached(
        "INSERT INTO scan_errors (scan_run_id, rel_path, path_display, error) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (scan_run_id, rel_path) DO UPDATE SET error = excluded.error",
    )?
    .execute(params![run, error.rel_path, error.path_display, error.error])?;
    Ok(())
}

/// The first `limit` errors of a run.
pub fn errors(c: &Connection, id: &ScanRunId, limit: u32) -> Result<Vec<ScanErrorRow>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT e.rel_path, e.path_display, e.error FROM scan_errors e
         JOIN scan_runs r ON r.id = e.scan_run_id WHERE r.pid = ?1 ORDER BY e.rel_path LIMIT ?2",
    )?;
    let rows = statement.query_map(params![id, i64::from(limit.max(1))], |row| {
        Ok(ScanErrorRow {
            rel_path: row.get(0)?,
            path_display: row.get(1)?,
            error: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn quarantine(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    file: &QuarantinedFile,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO scan_quarantine (library_id, rel_path, file_size, file_mtime_ns, reason, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (library_id, rel_path) DO UPDATE SET file_size = excluded.file_size,
             file_mtime_ns = excluded.file_mtime_ns, reason = excluded.reason, at = excluded.at",
    )?
    .execute(params![
        library,
        file.rel_path,
        from_u64(file.file_size),
        file.file_mtime_ns,
        file.reason,
        file.at
    ])?;
    Ok(())
}

/// Whether this exact file (path, size and mtime) is quarantined.
pub fn is_quarantined(
    c: &Connection,
    library: LibraryKey,
    rel_path: &[u8],
    file_size: u64,
    file_mtime_ns: i64,
) -> Result<bool, DbError> {
    Ok(c.prepare_cached(
        "SELECT EXISTS (SELECT 1 FROM scan_quarantine WHERE library_id = ?1 AND rel_path = ?2
             AND file_size = ?3 AND file_mtime_ns = ?4)",
    )?
    .query_row(
        params![library, rel_path, from_u64(file_size), file_mtime_ns],
        |row| row.get(0),
    )?)
}

/// Lifts a file's quarantine, after it changed or an admin asked.
pub fn release(tx: &mut WriteTx<'_>, library: LibraryKey, rel_path: &[u8]) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM scan_quarantine WHERE library_id = ?1 AND rel_path = ?2")?
        .execute(params![library, rel_path])?;
    Ok(())
}

pub fn quarantined(c: &Connection, library: LibraryKey) -> Result<Vec<QuarantinedFile>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT rel_path, file_size, file_mtime_ns, reason, at FROM scan_quarantine
         WHERE library_id = ?1 ORDER BY rel_path",
    )?;
    let rows = statement.query_map([library], |row| {
        Ok(QuarantinedFile {
            rel_path: row.get(0)?,
            file_size: to_u64(row.get(1)?),
            file_mtime_ns: row.get(2)?,
            reason: row.get(3)?,
            at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}
