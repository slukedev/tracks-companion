//! Albums.

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};

use super::sql::{
    OrderKey, after_condition, clamp_limit, decode_keyset, encode_keyset, json_strings,
    key_columns, order_by, read_keys, to_u32, to_u64, visible,
};
use crate::access::Access;
use crate::db::DbError;
use crate::ids::{AlbumId, ArtistId, LibraryKey, PublicId};
use crate::records::{Album, AlbumMbz, Page};

/// Album list orders of `GET /api/v1/albums` (and Subsonic `getAlbumList2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlbumSort {
    /// Most recently added first.
    Newest,
    /// The caller's most recently played first.
    Recent,
    /// The caller's most played first.
    Frequent,
    /// Random; the same seed gives the same order. Cursors are ignored.
    Random {
        seed: u64,
    },
    Alphabetical,
    AlphabeticalByArtist,
    /// The caller's starred albums, most recent star first.
    Starred,
    Year,
    /// The caller's highest rated first.
    Highest,
}

/// Narrows album lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlbumFilter {
    pub library: Option<LibraryKey>,
    pub genre: Option<String>,
    pub year_from: Option<i32>,
    pub year_to: Option<i32>,
}

/// The columns [`map_row`] reads, from `albums a` joined as below.
pub(crate) const ALBUM_SELECT: &str =
    "SELECT a.id, a.pid, l.pid, a.name, a.sort_name, a.album_artist, ar.pid,
        a.year, a.release_date, a.original_date, a.genre, a.is_compilation, a.release_type,
        a.song_count, a.disc_count, a.duration_ms, a.size_bytes, a.artwork_id, a.album_key,
        a.mbz_release_id, a.mbz_release_group_id, a.added_at, a.updated_at";

const ALBUM_FROM: &str = "FROM albums a
    JOIN libraries l ON l.id = +a.library_id
    LEFT JOIN artists ar ON ar.id = a.artist_id";

const ALBUM_COLUMNS: usize = 23;

pub(crate) fn map_row(row: &Row<'_>) -> rusqlite::Result<Album> {
    let genre: Option<String> = row.get(10)?;
    Ok(Album {
        id: row.get(1)?,
        library: row.get(2)?,
        name: row.get(3)?,
        sort_name: row.get(4)?,
        album_artist: row.get(5)?,
        artist_id: row.get(6)?,
        year: row.get(7)?,
        release_date: row.get(8)?,
        original_date: row.get(9)?,
        genres: genre.into_iter().collect(),
        is_compilation: row.get(11)?,
        release_type: row.get(12)?,
        song_count: to_u32(row.get(13)?),
        disc_count: to_u32(row.get(14)?),
        duration_ms: to_u64(row.get(15)?),
        size_bytes: to_u64(row.get(16)?),
        artwork: row.get(17)?,
        album_key: row.get(18)?,
        mbz: AlbumMbz {
            release: row.get(19)?,
            release_group: row.get(20)?,
        },
        added_at: row.get(21)?,
        updated_at: row.get(22)?,
    })
}

/// Runs `ALBUM_SELECT ALBUM_FROM <tail>`.
pub(crate) fn load(
    c: &Connection,
    tail: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Album>, DbError> {
    let mut statement = c.prepare_cached(&format!("{ALBUM_SELECT} {ALBUM_FROM} {tail}"))?;
    let rows = statement.query_map(params, map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Like [`load`], keeping each album's internal id.
pub(crate) fn load_keyed(
    c: &Connection,
    tail: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(i64, Album)>, DbError> {
    let mut statement = c.prepare_cached(&format!("{ALBUM_SELECT} {ALBUM_FROM} {tail}"))?;
    let rows = statement.query_map(params, |row| Ok((row.get(0)?, map_row(row)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn get(c: &Connection, a: &Access, id: &AlbumId) -> Result<Option<Album>, DbError> {
    let tail = format!("WHERE a.pid = ?1 AND {}", visible(a, "a.library_id", None));
    Ok(load(c, &tail, &[id])?.into_iter().next())
}

/// Albums in request order; unknown or invisible ids are left out.
pub fn get_many(c: &Connection, a: &Access, ids: &[AlbumId]) -> Result<Vec<Album>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = json_strings(ids.iter().map(PublicId::as_str));
    let tail = format!(
        "WHERE a.pid IN (SELECT value FROM json_each(?1)) AND {}",
        visible(a, "a.library_id", None)
    );
    let mut found: HashMap<AlbumId, Album> = load(c, &tail, &[&list])?
        .into_iter()
        .map(|album| (album.id.clone(), album))
        .collect();
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

/// The extra joins and ordering of one sort. `user` is bound once per `?` in `from`.
struct SortPlan {
    from: String,
    user_params: usize,
    keys: Vec<OrderKey>,
}

const RANDOM_MODULUS: i64 = 4_294_967_311;

fn plan(sort: AlbumSort, filter: &AlbumFilter) -> SortPlan {
    let base = String::from(ALBUM_FROM);
    let id_desc = OrderKey::desc("a.id");
    let id_asc = OrderKey::asc("a.id");
    match sort {
        AlbumSort::Newest => SortPlan {
            from: base,
            user_params: 0,
            // Ties on `added_at` (a whole batch shares it) run in rowid order, as the index has it.
            keys: vec![OrderKey::desc("a.added_at"), id_asc.clone()],
        },
        AlbumSort::Alphabetical => SortPlan {
            from: base,
            user_params: 0,
            keys: vec![OrderKey::asc("a.sort_name"), id_asc],
        },
        AlbumSort::AlphabeticalByArtist => SortPlan {
            from: base,
            user_params: 0,
            keys: vec![
                OrderKey::asc("lower(a.album_artist)"),
                OrderKey::asc("a.sort_name"),
                id_asc,
            ],
        },
        AlbumSort::Year => {
            let reversed = matches!((filter.year_from, filter.year_to), (Some(f), Some(t)) if f > t);
            let year = "coalesce(a.year, 0)";
            SortPlan {
                from: base,
                user_params: 0,
                keys: if reversed {
                    vec![OrderKey::desc(year), OrderKey::desc("a.sort_name"), id_desc]
                } else {
                    vec![OrderKey::asc(year), OrderKey::asc("a.sort_name"), id_asc]
                },
            }
        }
        AlbumSort::Random { seed } => {
            let seed = i64::try_from(seed % 2_147_483_647).unwrap_or(0);
            SortPlan {
                from: base,
                user_params: 0,
                keys: vec![
                    OrderKey::asc(format!(
                        "((a.id * 2654435761 + {seed}) % {RANDOM_MODULUS})"
                    )),
                    id_asc,
                ],
            }
        }
        AlbumSort::Recent | AlbumSort::Frequent => {
            let from = format!(
                "{base}
                 JOIN (SELECT so.album_id AS album_id, max(ps.last_played_at) AS last_at,
                              sum(ps.play_count) AS plays
                       FROM play_stats ps JOIN songs so ON so.pid = ps.song_pid
                       WHERE ps.user_id = ? AND so.album_id IS NOT NULL
                       GROUP BY so.album_id) ua ON ua.album_id = a.id"
            );
            let first = if sort == AlbumSort::Recent {
                OrderKey::desc("ua.last_at")
            } else {
                OrderKey::desc("ua.plays")
            };
            SortPlan {
                from,
                user_params: 1,
                keys: vec![first, id_desc],
            }
        }
        AlbumSort::Starred => SortPlan {
            from: format!(
                "{base}
                 JOIN stars st ON st.user_id = ? AND st.item_kind = 'album' AND st.item_pid = a.pid"
            ),
            user_params: 1,
            keys: vec![OrderKey::desc("st.starred_at"), id_desc],
        },
        AlbumSort::Highest => SortPlan {
            from: format!(
                "{base}
                 JOIN ratings rt ON rt.user_id = ? AND rt.item_kind = 'album' AND rt.item_pid = a.pid"
            ),
            user_params: 1,
            keys: vec![
                OrderKey::desc("rt.rating"),
                OrderKey::desc("rt.rated_at"),
                id_desc,
            ],
        },
    }
}

pub fn list(
    c: &Connection,
    a: &Access,
    sort: AlbumSort,
    cursor: Option<&str>,
    limit: u32,
    filter: &AlbumFilter,
) -> Result<Page<Album>, DbError> {
    let limit = clamp_limit(limit, 100, 500);
    if let AlbumSort::Random { seed } = sort
        && filter.genre.is_none()
        && filter.year_from.is_none()
        && filter.year_to.is_none()
    {
        return Ok(Page::last(sample(c, a, filter.library, seed, limit)?));
    }
    let plan = plan(sort, filter);
    let random = matches!(sort, AlbumSort::Random { .. });
    let after = if random {
        Vec::new()
    } else {
        decode_keyset(cursor, plan.keys.len())?
    };
    let mut params: Vec<Value> = vec![Value::Integer(a.user.0); plan.user_params];
    let mut conditions = vec![visible(a, "a.library_id", filter.library)];
    if let Some(genre) = &filter.genre {
        conditions.push(String::from(
            "EXISTS (SELECT 1 FROM songs gs JOIN song_genres sg ON sg.song_id = gs.id
                     JOIN genres g ON g.id = sg.genre_id
                     WHERE gs.album_id = a.id AND g.name_key = ?)",
        ));
        params.push(Value::Text(tracks_model::folded(genre)));
    }
    let (low, high) = match (filter.year_from, filter.year_to) {
        (Some(f), Some(t)) if f > t => (Some(t), Some(f)),
        other => other,
    };
    if let Some(low) = low {
        conditions.push(String::from("a.year >= ?"));
        params.push(Value::Integer(i64::from(low)));
    }
    if let Some(high) = high {
        conditions.push(String::from("a.year <= ?"));
        params.push(Value::Integer(i64::from(high)));
    }
    let (after_sql, after_params) = after_condition(&plan.keys, &after);
    conditions.push(after_sql);
    params.extend(after_params);
    let sql = format!(
        "{ALBUM_SELECT}{} {} WHERE {} {} LIMIT {}",
        key_columns(&plan.keys),
        plan.from,
        conditions.join(" AND "),
        order_by(&plan.keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let key_count = plan.keys.len();
    let mut rows: Vec<(Album, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            Ok((map_row(row)?, read_keys(row, ALBUM_COLUMNS, key_count)?))
        })?
        .collect::<Result<_, _>>()?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more && !random {
        rows.last().map(|(_, keys)| encode_keyset(keys))
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(|(album, _)| album).collect(),
        next,
    })
}

/// `SplitMix64`: a small, well-mixed sequence from a seed, so a seed always picks the same albums.
fn split_mix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Random albums by probing internal ids, which stays fast however many albums there are.
fn sample(
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    seed: u64,
    limit: u32,
) -> Result<Vec<Album>, DbError> {
    let (low, high): (Option<i64>, Option<i64>) =
        c.query_row("SELECT min(id), max(id) FROM albums", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let (Some(low), Some(high)) = (low, high) else {
        return Ok(Vec::new());
    };
    let vis = visible(a, "library_id", library);
    let mut next = c.prepare_cached(&format!(
        "SELECT id FROM albums WHERE id >= ?1 AND {vis} ORDER BY id LIMIT 1"
    ))?;
    let span = u64::try_from(high - low + 1).unwrap_or(1).max(1);
    let mut state = seed;
    let mut picked: Vec<i64> = Vec::new();
    let mut taken = std::collections::HashSet::new();
    for _ in 0..limit.saturating_mul(4) {
        let start = low + i64::try_from(split_mix(&mut state) % span).unwrap_or(0);
        let found: Option<i64> = next
            .query_row([start], |row| row.get(0))
            .optional()?
            .or(next.query_row([low], |row| row.get(0)).optional()?);
        let Some(id) = found else { break };
        if taken.insert(id) {
            picked.push(id);
            if picked.len() == limit as usize {
                break;
            }
        }
    }
    let list = super::sql::json_ints(picked.iter().copied());
    let mut found: HashMap<i64, Album> = load_keyed(
        c,
        "WHERE a.id IN (SELECT value FROM json_each(?1))",
        &[&list],
    )?
    .into_iter()
    .collect();
    Ok(picked.iter().filter_map(|id| found.remove(id)).collect())
}

/// Albums whose album artist is `artist`, oldest first.
pub fn by_artist(c: &Connection, a: &Access, artist: &ArtistId) -> Result<Vec<Album>, DbError> {
    let tail = format!(
        "WHERE a.artist_id = (SELECT id FROM artists WHERE pid = ?1) AND {}
         ORDER BY coalesce(a.year, 0), a.sort_name, a.id",
        visible(a, "a.library_id", None)
    );
    load(c, &tail, &[artist])
}

/// Albums by others that credit `artist` on at least one song, oldest first.
pub fn appears_on(c: &Connection, a: &Access, artist: &ArtistId) -> Result<Vec<Album>, DbError> {
    let tail = format!(
        "WHERE a.id IN (SELECT s.album_id FROM song_artists sa JOIN songs s ON s.id = sa.song_id
                        WHERE sa.artist_id = (SELECT id FROM artists WHERE pid = ?1)
                          AND sa.role IN ('main', 'featured') AND s.album_id IS NOT NULL)
           AND a.artist_id IS NOT (SELECT id FROM artists WHERE pid = ?1) AND {}
         ORDER BY coalesce(a.year, 0), a.sort_name, a.id",
        visible(a, "a.library_id", None)
    );
    load(c, &tail, &[artist])
}

/// How many albums the caller can see.
pub fn count(c: &Connection, a: &Access) -> Result<u64, DbError> {
    let total: i64 = c.query_row(
        &format!(
            "SELECT count(*) FROM albums a WHERE {}",
            visible(a, "a.library_id", None)
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(to_u64(total))
}
