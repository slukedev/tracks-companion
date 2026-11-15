//! Playlists. Entries name songs by public id, so they survive a song being purged and
//! returning.

use rusqlite::{Connection, Row, params};

use super::sql::{json_strings, to_u32, to_u64, visible};
use crate::access::Access;
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{PlaylistId, PublicId, SongId, UserId, UserKey, random_id};
use crate::time::Millis;

/// A playlist as clients see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playlist {
    pub id: PlaylistId,
    pub owner: UserKey,
    pub owner_id: UserId,
    pub owner_display_name: String,
    /// The owner's username, for Subsonic's `owner`.
    pub owner_username: String,
    pub name: String,
    pub comment: Option<String>,
    pub is_public: bool,
    pub song_count: u32,
    pub duration_ms: u64,
    /// The `ETag` and `If-Match` value; bumped by every change.
    pub revision: i64,
    pub created_at: Millis,
    pub updated_at: Millis,
}

/// One entry. The song may be gone; the id stays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub position: u32,
    pub song: SongId,
    pub added_at: Millis,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewPlaylist {
    pub name: String,
    pub comment: Option<String>,
    pub is_public: bool,
    pub entries: Vec<SongId>,
}

/// `PATCH /api/v1/playlists/{id}`. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaylistPatch {
    pub name: Option<String>,
    pub comment: Option<String>,
    pub is_public: Option<bool>,
}

const SELECT: &str = "SELECT p.id, p.pid, p.owner_id, u.pid, u.display_name, p.name, p.comment,
        p.is_public, p.song_count, p.duration_ms, p.revision, p.created_at, p.updated_at,
        u.username
    FROM playlists p JOIN users u ON u.id = p.owner_id";

fn map_row(row: &Row<'_>) -> rusqlite::Result<(i64, Playlist)> {
    Ok((
        row.get(0)?,
        Playlist {
            id: row.get(1)?,
            owner: row.get(2)?,
            owner_id: row.get(3)?,
            owner_display_name: row.get(4)?,
            owner_username: row.get(13)?,
            name: row.get(5)?,
            comment: row.get(6)?,
            is_public: row.get(7)?,
            song_count: to_u32(row.get(8)?),
            duration_ms: to_u64(row.get(9)?),
            revision: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        },
    ))
}

/// Whether playlist `p` is the caller's or public. The server itself sees every playlist.
fn visible_to(a: &Access) -> String {
    if a.user.0 == 0 {
        String::from("1")
    } else {
        format!("(p.owner_id = {} OR p.is_public = 1)", a.user.0)
    }
}

pub(crate) fn load(
    c: &Connection,
    tail: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(i64, Playlist)>, DbError> {
    let mut statement = c.prepare_cached(&format!("{SELECT} {tail}"))?;
    let rows = statement.query_map(params, map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn entries_of(c: &Connection, playlist: i64) -> Result<Vec<PlaylistEntry>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT position, song_pid, added_at FROM playlist_entries WHERE playlist_id = ?1
         ORDER BY position",
    )?;
    let mut rows = statement.query([playlist])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let song: String = row.get(1)?;
        if let Ok(song) = SongId::parse(&song) {
            out.push(PlaylistEntry {
                position: to_u32(row.get(0)?),
                song,
                added_at: row.get(2)?,
            });
        }
    }
    Ok(out)
}

/// The entries the caller may see: all of them for the owner (and the server), and for
/// anyone else only the songs in libraries they can see, so a public playlist never names
/// a song from a library it shouldn't.
pub(crate) fn entries_for(
    c: &Connection,
    a: &Access,
    playlist: i64,
    owner: UserKey,
) -> Result<Vec<PlaylistEntry>, DbError> {
    let entries = entries_of(c, playlist)?;
    if a.user == owner || a.user.0 == 0 || entries.is_empty() {
        return Ok(entries);
    }
    let list = json_strings(entries.iter().map(|e| e.song.as_str()));
    let sql = format!(
        "SELECT s.pid FROM songs s WHERE s.pid IN (SELECT value FROM json_each(?1)) AND {}",
        visible(a, "s.library_id", None)
    );
    let seen: std::collections::HashSet<String> = c
        .prepare_cached(&sql)?
        .query_map([&list], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(entries
        .into_iter()
        .filter(|e| seen.contains(e.song.as_str()))
        .collect())
}

/// The caller's own playlists and everyone's public ones, by name.
pub fn list(c: &Connection, a: &Access) -> Result<Vec<Playlist>, DbError> {
    Ok(load(
        c,
        &format!(
            "WHERE {} ORDER BY p.name COLLATE NOCASE, p.id",
            visible_to(a)
        ),
        &[],
    )?
    .into_iter()
    .map(|(_, playlist)| playlist)
    .collect())
}

/// A playlist the caller owns or that is public, with its entries in order.
pub fn get(
    c: &Connection,
    a: &Access,
    id: &PlaylistId,
) -> Result<Option<(Playlist, Vec<PlaylistEntry>)>, DbError> {
    let Some((key, playlist)) = load(c, &format!("WHERE p.pid = ?1 AND {}", visible_to(a)), &[id])?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let entries = entries_for(c, a, key, playlist.owner)?;
    Ok(Some((playlist, entries)))
}

fn record(
    tx: &mut WriteTx<'_>,
    pid: &PlaylistId,
    owner: UserKey,
    public: bool,
    op: ChangeOp,
) -> Result<(), DbError> {
    let change = if public {
        Change::shared(ChangeKind::Playlist, pid.as_str(), None, op)
    } else {
        Change::for_user(ChangeKind::Playlist, pid.as_str(), owner, op)
    };
    tx.record(change)?;
    Ok(())
}

/// Recounts entries and the duration of the songs this server has.
fn recount(tx: &WriteTx<'_>, playlist: i64) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE playlists SET
             song_count = (SELECT count(*) FROM playlist_entries WHERE playlist_id = ?1),
             duration_ms = (SELECT coalesce(sum(s.duration_ms), 0) FROM playlist_entries e
                            JOIN songs s ON s.pid = e.song_pid WHERE e.playlist_id = ?1)
         WHERE id = ?1",
    )?
    .execute([playlist])?;
    Ok(())
}

const MAX_ENTRIES: usize = 10_000;

fn write_entries(
    tx: &WriteTx<'_>,
    playlist: i64,
    start: u32,
    songs: &[SongId],
) -> Result<(), DbError> {
    let now = tx.now();
    for (offset, song) in songs.iter().enumerate() {
        tx.prepare_cached(
            "INSERT INTO playlist_entries (playlist_id, position, song_pid, added_at)
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            playlist,
            i64::from(start) + offset as i64,
            song,
            now
        ])?;
    }
    Ok(())
}

pub fn create(
    tx: &mut WriteTx<'_>,
    owner: UserKey,
    new: &NewPlaylist,
) -> Result<Playlist, DbError> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(DbError::Invalid(String::from("name")));
    }
    if new.entries.len() > MAX_ENTRIES {
        return Err(DbError::Invalid(String::from("entries")));
    }
    let pid: PlaylistId = random_id();
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO playlists (pid, owner_id, name, comment, is_public, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
    )?
    .execute(params![pid, owner, name, new.comment, new.is_public, now])?;
    let key = tx.last_insert_rowid();
    write_entries(tx, key, 0, &new.entries)?;
    recount(tx, key)?;
    record(tx, &pid, owner, new.is_public, ChangeOp::Upsert)?;
    one(tx, key)
}

fn one(c: &Connection, key: i64) -> Result<Playlist, DbError> {
    load(c, "WHERE p.id = ?1", &[&key])?
        .into_iter()
        .next()
        .map(|(_, playlist)| playlist)
        .ok_or(DbError::NotFound)
}

/// The playlist's key and row for a change by `owner`: `NotFound` when the caller cannot see
/// it, `Forbidden` when it is someone else's public playlist.
fn owned(tx: &WriteTx<'_>, owner: UserKey, id: &PlaylistId) -> Result<(i64, Playlist), DbError> {
    let found = load(tx, "WHERE p.pid = ?1", &[id])?.into_iter().next();
    match found {
        Some((key, playlist)) if playlist.owner == owner => Ok((key, playlist)),
        Some((_, playlist)) if playlist.is_public => Err(DbError::Forbidden),
        _ => Err(DbError::NotFound),
    }
}

/// Owner only: `NotFound` when invisible, `Forbidden` when public but not the caller's.
pub fn update(
    tx: &mut WriteTx<'_>,
    owner: UserKey,
    id: &PlaylistId,
    patch: &PlaylistPatch,
) -> Result<Playlist, DbError> {
    let (key, current) = owned(tx, owner, id)?;
    let name = match &patch.name {
        Some(name) if name.trim().is_empty() => return Err(DbError::Invalid(String::from("name"))),
        Some(name) => name.trim().to_owned(),
        None => current.name.clone(),
    };
    let comment = patch.comment.clone().or_else(|| current.comment.clone());
    let public = patch.is_public.unwrap_or(current.is_public);
    tx.prepare_cached(
        "UPDATE playlists SET name = ?2, comment = ?3, is_public = ?4, revision = revision + 1,
             updated_at = ?5 WHERE id = ?1",
    )?
    .execute(params![key, name, comment, public, tx.now()])?;
    match (current.is_public, public) {
        (true, false) => {
            record(tx, id, owner, true, ChangeOp::Delete)?;
            record(tx, id, owner, false, ChangeOp::Upsert)?;
        }
        (false, true) => {
            // The owner sees the shared row from now on; a stale private row would repeat it.
            tx.prepare_cached(
                "DELETE FROM change_log WHERE kind = 'playlist' AND entity_pid = ?1 AND user_id = ?2",
            )?
            .execute(params![id, owner])?;
            record(tx, id, owner, true, ChangeOp::Upsert)?;
        }
        _ => record(tx, id, owner, public, ChangeOp::Upsert)?,
    }
    one(tx, key)
}

/// Replaces the entries. `PreconditionFailed` when `if_revision` is given and has moved.
pub fn replace_entries(
    tx: &mut WriteTx<'_>,
    owner: UserKey,
    id: &PlaylistId,
    if_revision: Option<i64>,
    entries: &[SongId],
) -> Result<Playlist, DbError> {
    let (key, current) = owned(tx, owner, id)?;
    if if_revision.is_some_and(|revision| revision != current.revision) {
        return Err(DbError::PreconditionFailed);
    }
    if entries.len() > MAX_ENTRIES {
        return Err(DbError::Invalid(String::from("entries")));
    }
    tx.prepare_cached("DELETE FROM playlist_entries WHERE playlist_id = ?1")?
        .execute([key])?;
    write_entries(tx, key, 0, entries)?;
    touch(tx, key)?;
    record(tx, id, owner, current.is_public, ChangeOp::Upsert)?;
    one(tx, key)
}

fn touch(tx: &WriteTx<'_>, key: i64) -> Result<(), DbError> {
    recount(tx, key)?;
    tx.prepare_cached(
        "UPDATE playlists SET revision = revision + 1, updated_at = ?2 WHERE id = ?1",
    )?
    .execute(params![key, tx.now()])?;
    Ok(())
}

pub fn append(
    tx: &mut WriteTx<'_>,
    owner: UserKey,
    id: &PlaylistId,
    songs: &[SongId],
) -> Result<Playlist, DbError> {
    let (key, current) = owned(tx, owner, id)?;
    if current.song_count as usize + songs.len() > MAX_ENTRIES {
        return Err(DbError::Invalid(String::from("entries")));
    }
    let next: i64 = tx
        .prepare_cached(
            "SELECT coalesce(max(position) + 1, 0) FROM playlist_entries WHERE playlist_id = ?1",
        )?
        .query_row([key], |row| row.get(0))?;
    write_entries(tx, key, to_u32(next), songs)?;
    touch(tx, key)?;
    record(tx, id, owner, current.is_public, ChangeOp::Upsert)?;
    one(tx, key)
}

pub fn delete(tx: &mut WriteTx<'_>, owner: UserKey, id: &PlaylistId) -> Result<(), DbError> {
    let (key, current) = owned(tx, owner, id)?;
    tx.prepare_cached("DELETE FROM playlists WHERE id = ?1")?
        .execute([key])?;
    record(tx, id, owner, current.is_public, ChangeOp::Delete)?;
    Ok(())
}

/// The internal key of a visible playlist, for sync.
pub(crate) fn visible_after(
    c: &Connection,
    a: &Access,
    after: i64,
    limit: u32,
) -> Result<Vec<(i64, Playlist)>, DbError> {
    load(
        c,
        &format!(
            "WHERE p.id > ?1 AND {} ORDER BY p.id LIMIT {}",
            visible_to(a),
            limit
        ),
        &[&after],
    )
}

/// Deletes every playlist of a user, recording tombstones for the public ones.
pub(crate) fn delete_all_for(tx: &mut WriteTx<'_>, owner: UserKey) -> Result<(), DbError> {
    let public: Vec<PlaylistId> = tx
        .prepare_cached("SELECT pid FROM playlists WHERE owner_id = ?1 AND is_public = 1")?
        .query_map([owner], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    tx.prepare_cached("DELETE FROM playlists WHERE owner_id = ?1")?
        .execute([owner])?;
    for pid in public {
        record(tx, &pid, owner, true, ChangeOp::Delete)?;
    }
    Ok(())
}
