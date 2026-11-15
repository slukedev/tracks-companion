//! The queue operations core's `repo::acquisitions` does not have: handing joined requests
//! on, requeueing for a retry, progress without a sync change, and the two library lookups
//! the acquirer needs by key and by path. These could move into core as they are.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};
use trackside_core::ids::PublicId;
use trackside_core::repo::acquisitions::{self, AcquisitionRow};
use trackside_core::{
    AcquisitionId, Change, ChangeKind, ChangeOp, DbError, LibraryId, LibraryKey, SongId, WriteTx,
};

fn path_from_blob(bytes: Vec<u8>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn path_to_blob(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

fn reread(tx: &WriteTx<'_>, id: &AcquisitionId) -> Result<AcquisitionRow, DbError> {
    acquisitions::get(tx, id)?.ok_or(DbError::NotFound)
}

/// Writes progress without a sync change: progress reaches clients over SSE only, at most
/// once a second, and a change-log row per tick would churn delta sync.
pub fn set_progress(
    tx: &mut WriteTx<'_>,
    id: &AcquisitionId,
    bytes: u64,
    total: Option<u64>,
) -> Result<AcquisitionRow, DbError> {
    let now = tx.now();
    tx.prepare_cached(
        "UPDATE acquisitions SET progress_bytes = ?2, total_bytes = ?3, updated_at = ?4
         WHERE pid = ?1",
    )?
    .execute(params![id, bytes as i64, total.map(|t| t as i64), now])?;
    reread(tx, id)
}

/// Makes a joined request run on its own (when the one it joined was cancelled).
pub fn detach(tx: &mut WriteTx<'_>, id: &AcquisitionId) -> Result<(), DbError> {
    let now = tx.now();
    tx.prepare_cached(
        "UPDATE acquisitions SET joined_to = NULL, state = 'queued', next_attempt_at = ?2,
             updated_at = ?2 WHERE pid = ?1",
    )?
    .execute(params![id, now])?;
    Ok(())
}

/// Moves requests joined to `from` over to `to`.
pub fn rejoin(
    tx: &mut WriteTx<'_>,
    from: &AcquisitionId,
    to: &AcquisitionId,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE acquisitions SET joined_to = (SELECT id FROM acquisitions WHERE pid = ?2)
         WHERE joined_to = (SELECT id FROM acquisitions WHERE pid = ?1) AND pid != ?2",
    )?
    .execute(params![from, to])?;
    Ok(())
}

/// Puts a failed or cancelled request back in the queue for a fresh run: attempts, failure
/// and progress cleared, which `acquisitions::transition` (it only sets fields) cannot do.
pub fn requeue(tx: &mut WriteTx<'_>, id: &AcquisitionId) -> Result<AcquisitionRow, DbError> {
    let now = tx.now();
    let changed = tx
        .prepare_cached(
            "UPDATE acquisitions SET state = 'queued', failure = NULL, failure_detail = NULL,
                 attempts = 0, next_attempt_at = ?2, lease_until = NULL, finished_at = NULL,
                 progress_bytes = NULL, total_bytes = NULL, updated_at = ?2
             WHERE pid = ?1",
        )?
        .execute(params![id, now])?;
    if changed == 0 {
        return Err(DbError::NotFound);
    }
    let row = reread(tx, id)?;
    if let Some(user) = row.user {
        tx.record(Change::for_user(
            ChangeKind::Acquisition,
            row.id.as_str(),
            user,
            ChangeOp::Upsert,
        ))?;
    }
    Ok(row)
}

/// Where a song's file is: its library and relative path bytes, missing or not, to tell
/// whether a digest duplicate is the very file just imported.
pub fn song_location(
    c: &Connection,
    song: &SongId,
) -> Result<Option<(LibraryKey, Vec<u8>)>, DbError> {
    Ok(
        c.prepare_cached("SELECT library_id, rel_path FROM songs WHERE pid = ?1")?
            .query_row([song], |r| Ok((LibraryKey(r.get(0)?), r.get(1)?)))
            .optional()?,
    )
}

/// The acquired library: key, pid and root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetLibrary {
    pub key: LibraryKey,
    pub id: LibraryId,
    pub root: PathBuf,
    pub writable: bool,
}

/// The library named by `acquire.library`, which may be a pid or a root path. Core's
/// `libraries::get` takes an `Access` and a pid only; this runs as the server, by either.
pub fn target_library(c: &Connection, pid_or_root: &str) -> Result<Option<TargetLibrary>, DbError> {
    let read = |r: &Row<'_>| {
        Ok(TargetLibrary {
            key: LibraryKey(r.get(0)?),
            id: r.get(1)?,
            root: path_from_blob(r.get(2)?),
            writable: r.get(3)?,
        })
    };
    let by_pid = c
        .prepare_cached("SELECT id, pid, root, writable FROM libraries WHERE pid = ?1")?
        .query_row([pid_or_root], read)
        .optional()?;
    if by_pid.is_some() {
        return Ok(by_pid);
    }
    Ok(
        c.prepare_cached("SELECT id, pid, root, writable FROM libraries WHERE root = ?1")?
            .query_row([path_to_blob(Path::new(pid_or_root))], read)
            .optional()?,
    )
}
