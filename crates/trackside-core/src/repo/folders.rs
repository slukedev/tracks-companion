//! Folders, as the scanner records them and Subsonic's folder browsing shows them.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::{hash32, rel_name, rel_parent, visible};
use crate::access::Access;
use crate::db::{DbError, WriteTx};
use crate::ids::{FolderId, LibraryId, LibraryKey};
use crate::records::Folder;

/// What the scanner knows about a folder it walked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderUpsert {
    /// Empty for the library root.
    pub rel_path: Vec<u8>,
    pub name: String,
    pub mtime_ns: Option<i64>,
    pub listing_hash: Option<[u8; 32]>,
}

/// What a quick scan compares to decide whether to skip a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderState {
    pub mtime_ns: Option<i64>,
    pub listing_hash: Option<[u8; 32]>,
    pub unreadable: bool,
}

const SELECT: &str = "SELECT f.pid, f.library_id, p.pid, f.rel_path, f.name, f.mtime_ns,
        f.listing_hash, f.unreadable, f.last_scanned_at, f.missing_since
    FROM folders f LEFT JOIN folders p ON p.id = f.parent_id";

fn map_row(row: &Row<'_>) -> rusqlite::Result<Folder> {
    Ok(Folder {
        id: row.get(0)?,
        library: row.get(1)?,
        parent: row.get(2)?,
        rel_path: row.get(3)?,
        name: row.get(4)?,
        mtime_ns: row.get(5)?,
        listing_hash: hash32(row.get(6)?),
        unreadable: row.get(7)?,
        last_scanned_at: row.get(8)?,
        missing_since: row.get(9)?,
    })
}

fn load(
    c: &Connection,
    tail: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Folder>, DbError> {
    let mut statement = c.prepare_cached(&format!("{SELECT} {tail}"))?;
    let rows = statement.query_map(params, map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn get(c: &Connection, a: &Access, id: &FolderId) -> Result<Option<Folder>, DbError> {
    Ok(load(
        c,
        &format!("WHERE f.pid = ?1 AND {}", visible(a, "f.library_id", None)),
        &[id],
    )?
    .into_iter()
    .next())
}

/// A folder's path relative to its library root, as the scanner's folder scan takes it.
pub fn rel_path(
    c: &Connection,
    a: &Access,
    id: &FolderId,
) -> Result<Option<std::path::PathBuf>, DbError> {
    Ok(get(c, a, id)?.map(|f| super::sql::bytes_path(&f.rel_path)))
}

/// The root folder of a library.
pub fn root(c: &Connection, a: &Access, library: LibraryKey) -> Result<Option<Folder>, DbError> {
    if !a.can_see(library) {
        return Ok(None);
    }
    Ok(load(
        c,
        "WHERE f.library_id = ?1 AND f.rel_path = x''",
        &[&library],
    )?
    .into_iter()
    .next())
}

/// Subfolders by name.
pub fn children(c: &Connection, a: &Access, id: &FolderId) -> Result<Vec<Folder>, DbError> {
    load(
        c,
        &format!(
            "WHERE f.parent_id = (SELECT id FROM folders WHERE pid = ?1) AND {}
             ORDER BY f.name COLLATE NOCASE, f.id",
            visible(a, "f.library_id", None)
        ),
        &[id],
    )
}

fn library_pid(c: &Connection, library: LibraryKey) -> Result<LibraryId, DbError> {
    c.prepare_cached("SELECT pid FROM libraries WHERE id = ?1")?
        .query_row([library], |row| row.get(0))
        .optional()?
        .ok_or(DbError::NotFound)
}

/// The internal id of the folder at `rel_path`, creating it and its parents as needed. Parents
/// made here get their name from the path; the scanner fills in the rest when it walks them.
pub(crate) fn ensure(
    tx: &WriteTx<'_>,
    library: LibraryKey,
    library_pid: &LibraryId,
    rel_path: &[u8],
) -> Result<(i64, FolderId), DbError> {
    let existing: Option<(i64, FolderId)> = tx
        .prepare_cached("SELECT id, pid FROM folders WHERE library_id = ?1 AND rel_path = ?2")?
        .query_row(params![library, rel_path], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    if let Some(found) = existing {
        return Ok(found);
    }
    let parent = if rel_path.is_empty() {
        None
    } else {
        Some(ensure(tx, library, library_pid, rel_parent(rel_path))?.0)
    };
    let id = FolderId::derive(library_pid, rel_path);
    let name = String::from_utf8_lossy(rel_name(rel_path)).into_owned();
    tx.prepare_cached(
        "INSERT INTO folders (pid, library_id, parent_id, rel_path, name) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![id, library, parent, rel_path, name])?;
    Ok((tx.last_insert_rowid(), id))
}

/// Creates or updates a folder (and any missing parents), clearing `missing_since` and
/// `unreadable`.
pub fn upsert(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    folder: &FolderUpsert,
) -> Result<FolderId, DbError> {
    let library_pid = library_pid(tx, library)?;
    let (key, id) = ensure(tx, library, &library_pid, &folder.rel_path)?;
    tx.prepare_cached(
        "UPDATE folders SET name = ?2, mtime_ns = ?3, listing_hash = ?4, unreadable = 0,
             missing_since = NULL, last_scanned_at = ?5
         WHERE id = ?1",
    )?
    .execute(params![
        key,
        folder.name,
        folder.mtime_ns,
        folder.listing_hash.map(|h| h.to_vec()),
        tx.now()
    ])?;
    Ok(id)
}

/// Marks a folder unreadable (or readable again). Songs under an unreadable folder are never
/// marked missing.
pub fn mark_unreadable(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    rel_path: &[u8],
    unreadable: bool,
) -> Result<(), DbError> {
    let library_pid = library_pid(tx, library)?;
    let (key, _) = ensure(tx, library, &library_pid, rel_path)?;
    tx.prepare_cached("UPDATE folders SET unreadable = ?2 WHERE id = ?1")?
        .execute(params![key, unreadable])?;
    Ok(())
}

/// The stored mtime and listing hash of a folder, if known.
pub fn listing_hash(
    c: &Connection,
    library: LibraryKey,
    rel_path: &[u8],
) -> Result<Option<FolderState>, DbError> {
    Ok(c.prepare_cached(
        "SELECT mtime_ns, listing_hash, unreadable FROM folders
             WHERE library_id = ?1 AND rel_path = ?2",
    )?
    .query_row(params![library, rel_path], |row| {
        Ok(FolderState {
            mtime_ns: row.get(0)?,
            listing_hash: hash32(row.get(1)?),
            unreadable: row.get(2)?,
        })
    })
    .optional()?)
}

/// Relative paths of a library's folders marked unreadable.
pub fn unreadable(c: &Connection, library: LibraryKey) -> Result<Vec<Vec<u8>>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT rel_path FROM folders WHERE library_id = ?1 AND unreadable = 1 ORDER BY rel_path",
    )?;
    let rows = statement.query_map([library], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
