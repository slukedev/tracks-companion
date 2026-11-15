//! What the admin library health views list: songs whose tags fell back to the file name,
//! folders the scanner could not read, and groups of exact duplicates.

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

use super::sql::{
    OrderKey, after_condition, clamp_limit, decode_keyset, encode_keyset, json_ints, order_by,
    read_keys, to_u64,
};
use crate::db::DbError;
use crate::ids::{AudioDigest, LibraryKey};
use crate::records::{Folder, Page, Song};

/// A song whose tags could not be read as they are.
#[derive(Clone, Debug, PartialEq)]
pub struct TagProblem {
    pub song: Song,
    /// `tag_problem`: why tags fell back to the file name, or `digest_whole_file`.
    pub problem: String,
    pub rel_path_display: String,
}

/// Songs with exactly the same audio.
#[derive(Clone, Debug, PartialEq)]
pub struct DuplicateGroup {
    pub digest: AudioDigest,
    /// By internal id, so the first is the one indexed first.
    pub songs: Vec<Song>,
}

fn library_condition(library: Option<LibraryKey>, column: &str) -> String {
    library.map_or_else(|| String::from("1"), |key| format!("{column} = {}", key.0))
}

/// Songs with a tag problem, by path, for one library or all.
pub fn tag_problems(
    c: &Connection,
    library: Option<LibraryKey>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<TagProblem>, DbError> {
    let limit = clamp_limit(limit, 100, 500);
    let keys = [
        OrderKey::asc("s.library_id"),
        OrderKey::asc("s.rel_path_display"),
        OrderKey::asc("s.id"),
    ];
    let after = decode_keyset(cursor, keys.len())?;
    let (after_sql, params) = after_condition(&keys, &after);
    let sql = format!(
        "SELECT s.id, s.tag_problem, s.rel_path_display, s.library_id, s.rel_path_display, s.id
         FROM songs s WHERE s.tag_problem IS NOT NULL AND {} AND {after_sql} {} LIMIT {}",
        library_condition(library, "s.library_id"),
        order_by(&keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<(i64, String, String, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, read_keys(row, 3, 3)?))
        })?
        .collect::<Result<_, _>>()?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|r| encode_keyset(&r.3))
    } else {
        None
    };
    let list = json_ints(rows.iter().map(|r| r.0));
    let mut songs: std::collections::HashMap<i64, Song> = super::songs::load_keyed(
        c,
        "WHERE s.id IN (SELECT value FROM json_each(?1))",
        &[&list],
    )?
    .into_iter()
    .collect();
    let items = rows
        .into_iter()
        .filter_map(|(id, problem, rel_path_display, _)| {
            songs.remove(&id).map(|song| TagProblem {
                song,
                problem,
                rel_path_display,
            })
        })
        .collect();
    Ok(Page { items, next })
}

/// How many songs have a tag problem.
pub fn tag_problem_count(c: &Connection, library: Option<LibraryKey>) -> Result<u64, DbError> {
    let n: i64 = c.query_row(
        &format!(
            "SELECT count(*) FROM songs s WHERE s.tag_problem IS NOT NULL AND {}",
            library_condition(library, "s.library_id")
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(to_u64(n))
}

/// Folders marked unreadable, by path, for one library or all.
pub fn unreadable_folders(
    c: &Connection,
    library: Option<LibraryKey>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<Folder>, DbError> {
    let limit = clamp_limit(limit, 100, 500);
    let keys = [OrderKey::asc("f.id")];
    let after = decode_keyset(cursor, keys.len())?;
    let (after_sql, params) = after_condition(&keys, &after);
    let sql = format!(
        "SELECT f.pid, f.library_id, p.pid, f.rel_path, f.name, f.mtime_ns, f.listing_hash,
                f.unreadable, f.last_scanned_at, f.missing_since, f.id
         FROM folders f LEFT JOIN folders p ON p.id = f.parent_id
         WHERE f.unreadable = 1 AND {} AND {after_sql} {} LIMIT {}",
        library_condition(library, "f.library_id"),
        order_by(&keys),
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows: Vec<(Folder, Vec<Value>)> = statement
        .query_map(params_from_iter(params), |row| {
            Ok((
                Folder {
                    id: row.get(0)?,
                    library: row.get(1)?,
                    parent: row.get(2)?,
                    rel_path: row.get(3)?,
                    name: row.get(4)?,
                    mtime_ns: row.get(5)?,
                    listing_hash: super::sql::hash32(row.get(6)?),
                    unreadable: row.get(7)?,
                    last_scanned_at: row.get(8)?,
                    missing_since: row.get(9)?,
                },
                read_keys(row, 10, 1)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|r| encode_keyset(&r.1))
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(|(folder, _)| folder).collect(),
        next,
    })
}

/// Groups of two or more songs with the same audio digest, for one library or all. Each page
/// holds up to `limit` groups.
pub fn duplicate_groups(
    c: &Connection,
    library: Option<LibraryKey>,
    cursor: Option<&str>,
    limit: u32,
) -> Result<Page<DuplicateGroup>, DbError> {
    let limit = clamp_limit(limit, 50, 200);
    let after = decode_keyset(cursor, 1)?;
    let after_id = match after.first() {
        Some(Value::Integer(i)) => *i,
        _ => 0,
    };
    let lib = library_condition(library, "library_id");
    // Groups are keyed by their first song's id, which is stable while the group exists.
    let sql = format!(
        "SELECT audio_digest, min(id) AS first FROM songs WHERE {lib}
         GROUP BY audio_digest HAVING count(*) > 1 AND min(id) > ?1
         ORDER BY first LIMIT {}",
        limit + 1
    );
    let mut statement = c.prepare_cached(&sql)?;
    let mut groups: Vec<(Vec<u8>, i64)> = statement
        .query_map([after_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let more = groups.len() > limit as usize;
    groups.truncate(limit as usize);
    let next = if more {
        groups
            .last()
            .map(|(_, first)| encode_keyset(&[Value::Integer(*first)]))
    } else {
        None
    };
    let mut items = Vec::with_capacity(groups.len());
    for (digest, _) in groups {
        let tail = format!(
            "WHERE s.audio_digest = ?1 AND {} ORDER BY s.id",
            lib.replace("library_id", "s.library_id")
        );
        let songs = super::songs::load(c, &tail, &[&digest])?;
        let digest = <[u8; 32]>::try_from(digest.as_slice()).unwrap_or([0; 32]);
        items.push(DuplicateGroup {
            digest: AudioDigest(digest),
            songs,
        });
    }
    Ok(Page { items, next })
}
