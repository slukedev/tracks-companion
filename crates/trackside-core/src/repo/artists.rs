//! Artists. Artists are global and visible when they have songs in one of the caller's
//! libraries.

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{Connection, Row, params_from_iter};

use super::sql::{
    OrderKey, after_condition, clamp_limit, decode_keyset, encode_keyset, json_strings,
    key_columns, order_by, read_keys, to_u32, visible,
};
use crate::access::Access;
use crate::db::DbError;
use crate::ids::{ArtistId, LibraryKey, PublicId};
use crate::records::{Artist, Page};

const ARTIST_COLUMNS: usize = 9;

/// The select list for artists `r`, with counts and visibility over `vis`, a condition on
/// `x.library_id`.
pub(crate) fn select(vis: &str) -> String {
    format!(
        "SELECT r.id, r.pid, r.name, r.sort_name, r.identity,
            (SELECT coalesce(sum(x.album_count), 0) FROM artist_libraries x
             WHERE x.artist_id = r.id AND {vis}),
            (SELECT coalesce(sum(x.song_count), 0) FROM artist_libraries x
             WHERE x.artist_id = r.id AND {vis}),
            coalesce(r.artwork_id, (SELECT a.artwork_id FROM albums a
                WHERE a.artist_id = r.id AND a.artwork_id IS NOT NULL
                  AND {} ORDER BY a.added_at LIMIT 1)),
            r.mbz_artist_id",
        vis.replace("x.library_id", "a.library_id")
    )
}

/// Whether artist `r` has songs in a library matching `vis`.
pub(crate) fn exists(vis: &str) -> String {
    format!("EXISTS (SELECT 1 FROM artist_libraries x WHERE x.artist_id = r.id AND {vis})")
}

pub(crate) fn map_row(row: &Row<'_>) -> rusqlite::Result<Artist> {
    Ok(Artist {
        id: row.get(1)?,
        name: row.get(2)?,
        sort_name: row.get(3)?,
        identity: row.get(4)?,
        album_count: to_u32(row.get(5)?),
        song_count: to_u32(row.get(6)?),
        artwork: row.get(7)?,
        mbz_artist: row.get(8)?,
    })
}

/// Visible artists matching `condition` on `r`, in `order`.
pub(crate) fn load(
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    condition: &str,
    order: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Artist>, DbError> {
    let vis = visible(a, "x.library_id", library);
    let sql = format!(
        "{} FROM artists r WHERE {condition} AND {} {order}",
        select(&vis),
        exists(&vis)
    );
    let mut statement = c.prepare_cached(&sql)?;
    let rows = statement.query_map(params, map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Visible artists after internal id `after`, by id, with their ids. For sync snapshots.
pub(crate) fn after_id(
    c: &Connection,
    a: &Access,
    after: i64,
    limit: u32,
) -> Result<Vec<(i64, Artist)>, DbError> {
    let vis = visible(a, "x.library_id", None);
    let sql = format!(
        "{} FROM artists r WHERE r.id > ?1 AND {} ORDER BY r.id LIMIT {limit}",
        select(&vis),
        exists(&vis)
    );
    let mut statement = c.prepare_cached(&sql)?;
    let rows = statement.query_map([after], |row| Ok((row.get(0)?, map_row(row)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Visible artists matching `condition` on `r`, with their internal ids.
pub(crate) fn load_keyed(
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    condition: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(i64, Artist)>, DbError> {
    let vis = visible(a, "x.library_id", library);
    let sql = format!(
        "{} FROM artists r WHERE {condition} AND {}",
        select(&vis),
        exists(&vis)
    );
    let mut statement = c.prepare_cached(&sql)?;
    let rows = statement.query_map(params, |row| Ok((row.get(0)?, map_row(row)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn get(c: &Connection, a: &Access, id: &ArtistId) -> Result<Option<Artist>, DbError> {
    Ok(load(c, a, None, "r.pid = ?1", "", &[id])?
        .into_iter()
        .next())
}

/// Artists in request order; unknown or invisible ids are left out.
pub fn get_many(c: &Connection, a: &Access, ids: &[ArtistId]) -> Result<Vec<Artist>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = json_strings(ids.iter().map(PublicId::as_str));
    let mut found: HashMap<ArtistId, Artist> = load(
        c,
        a,
        None,
        "r.pid IN (SELECT value FROM json_each(?1))",
        "",
        &[&list],
    )?
    .into_iter()
    .map(|artist| (artist.id.clone(), artist))
    .collect();
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

/// By sort name.
pub fn list(
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<Artist>, DbError> {
    let limit = clamp_limit(limit, 100, 500);
    let keys = [OrderKey::asc("r.sort_name"), OrderKey::asc("r.id")];
    let after = decode_keyset(cursor, keys.len())?;
    let (after_sql, params) = after_condition(&keys, &after);
    let vis = visible(a, "x.library_id", library);
    let sql = format!(
        "{}{} FROM artists r WHERE {} AND {after_sql} {} LIMIT {}",
        select(&vis),
        key_columns(&keys),
        exists(&vis),
        order_by(&keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<(Artist, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            Ok((map_row(row)?, read_keys(row, ARTIST_COLUMNS, keys.len())?))
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
        items: rows.into_iter().map(|(artist, _)| artist).collect(),
        next,
    })
}

/// The artist whose identity is `tracks_model::folded(name)`.
pub fn by_name(c: &Connection, a: &Access, name: &str) -> Result<Option<Artist>, DbError> {
    let identity = tracks_model::folded(name);
    Ok(load(c, a, None, "r.identity = ?1", "", &[&identity])?
        .into_iter()
        .next())
}

/// How many artists the caller can see.
pub fn count(c: &Connection, a: &Access) -> Result<u64, DbError> {
    let vis = visible(a, "x.library_id", None);
    let total: i64 = c.query_row(
        &format!("SELECT count(*) FROM artists r WHERE {}", exists(&vis)),
        [],
        |row| row.get(0),
    )?;
    Ok(super::sql::to_u64(total))
}
