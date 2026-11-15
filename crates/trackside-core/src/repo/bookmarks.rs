//! Bookmarks: a saved position in a song.

use rusqlite::{Connection, params};

use super::sql::{from_u64, to_u64};
use crate::db::{DbError, WriteTx};
use crate::ids::{PublicId, SongId, UserKey};
use crate::time::Millis;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub song: SongId,
    pub position_ms: u64,
    pub comment: Option<String>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

/// A user's bookmarks, most recently changed first.
pub fn list(c: &Connection, user: UserKey) -> Result<Vec<Bookmark>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT song_pid, position_ms, comment, created_at, updated_at FROM bookmarks
         WHERE user_id = ?1 ORDER BY updated_at DESC, song_pid",
    )?;
    let mut rows = statement.query([user])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let song: String = row.get(0)?;
        let Ok(song) = SongId::parse(&song) else {
            continue;
        };
        out.push(Bookmark {
            song,
            position_ms: to_u64(row.get(1)?),
            comment: row.get(2)?,
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
        });
    }
    Ok(out)
}

pub fn put(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    song: &SongId,
    position_ms: u64,
    comment: Option<&str>,
) -> Result<Bookmark, DbError> {
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO bookmarks (user_id, song_pid, position_ms, comment, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT (user_id, song_pid) DO UPDATE SET position_ms = excluded.position_ms,
             comment = excluded.comment, updated_at = excluded.updated_at",
    )?
    .execute(params![user, song, from_u64(position_ms), comment, now])?;
    list(tx, user)?
        .into_iter()
        .find(|b| &b.song == song)
        .ok_or(DbError::NotFound)
}

pub fn delete(tx: &mut WriteTx<'_>, user: UserKey, song: &SongId) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM bookmarks WHERE user_id = ?1 AND song_pid = ?2")?
        .execute(params![user, song])?;
    Ok(())
}
