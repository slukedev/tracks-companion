//! Library search over pre-folded text with FTS5. Queries are folded with
//! `tracks_model::search_folded` and every word matches as a prefix.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::access::Access;
use crate::db::DbError;
use crate::ids::LibraryKey;
use crate::records::{Album, Artist, Page, Song};
use crate::repo;
use crate::repo::sql::{decode_cursor, encode_cursor, json_ints, visible};

/// Which result kinds to search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchKinds {
    pub songs: bool,
    pub albums: bool,
    pub artists: bool,
}

impl SearchKinds {
    pub const ALL: Self = Self {
        songs: true,
        albums: true,
        artists: true,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub kinds: SearchKinds,
    pub limit: u32,
    /// Pages a single kind; pass it with only that kind enabled.
    pub cursor: Option<String>,
    pub library: Option<LibraryKey>,
}

/// Results by kind, ranked by bm25 with exact folded matches first. Kinds not searched are
/// empty.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchResults {
    pub songs: Page<Song>,
    pub albums: Page<Album>,
    pub artists: Page<Artist>,
}

/// The FTS5 query for folded text: every word as a quoted prefix, all required. `None` when
/// nothing searchable is left after folding.
pub fn match_expression(text: &str) -> Option<(String, String)> {
    let folded = tracks_model::search_folded(text);
    let words: Vec<String> = folded
        .split_whitespace()
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    if words.is_empty() {
        return None;
    }
    Some((words.join(" "), folded))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SearchCursor {
    o: u32,
}

/// The largest page a search returns.
pub const MAX_LIMIT: u32 = 500;

/// How many full-text matches per kind are ranked. A one- or two-letter prefix can match most
/// of a large library, and ranking all of it costs more than search as you type allows, so
/// such queries rank the first matches in index order; more specific queries have fewer
/// matches than this and are ranked in full (see `hits` for callers who see only some
/// libraries).
pub const RANKED_CANDIDATES: u32 = 2000;

/// Results by kind, ranked with exact folded matches first, then by bm25, then by id. A cursor
/// is an offset into one kind's ranking.
pub fn search(c: &Connection, a: &Access, q: &SearchQuery) -> Result<SearchResults, DbError> {
    let mut results = SearchResults::default();
    let Some((expression, folded)) = match_expression(&q.text) else {
        return Ok(results);
    };
    let limit = q.limit.clamp(1, MAX_LIMIT);
    let offset = match q.cursor.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(cursor) => decode_cursor::<SearchCursor>(cursor)?.o,
        None => 0,
    };
    let page = |ids: Vec<i64>| -> (Vec<i64>, Option<String>) {
        let mut ids = ids;
        let more = ids.len() > limit as usize;
        ids.truncate(limit as usize);
        let next = more.then(|| encode_cursor(&SearchCursor { o: offset + limit }));
        (ids, next)
    };
    let window = format!("LIMIT {} OFFSET {offset}", limit + 1);
    if q.kinds.songs {
        let vis = visible(a, "s.library_id", q.library);
        let filter = format!("s.missing_since IS NULL AND {vis}");
        let sql = format!(
            "WITH hits AS MATERIALIZED ({})
             SELECT s.id FROM hits JOIN songs s ON s.id = hits.id WHERE {filter}
             ORDER BY trackside_search_folded(s.title) = ?2 DESC, hits.score, s.id {window}",
            hits("songs_fts", "songs s", "s", &vis, &filter)
        );
        let (ids, next) = page(ids_for(c, &sql, &expression, &folded)?);
        let list = json_ints(ids.iter().copied());
        let mut found: HashMap<i64, Song> = repo::songs::load_keyed(
            c,
            "WHERE s.id IN (SELECT value FROM json_each(?1))",
            &[&list],
        )?
        .into_iter()
        .collect();
        results.songs = Page {
            items: ids.iter().filter_map(|id| found.remove(id)).collect(),
            next,
        };
    }
    if q.kinds.albums {
        let vis = visible(a, "a.library_id", q.library);
        let sql = format!(
            "WITH hits AS MATERIALIZED ({})
             SELECT a.id FROM hits JOIN albums a ON a.id = hits.id WHERE {vis}
             ORDER BY trackside_search_folded(a.name) = ?2 DESC, hits.score, a.id {window}",
            hits("albums_fts", "albums a", "a", &vis, &vis)
        );
        let (ids, next) = page(ids_for(c, &sql, &expression, &folded)?);
        let list = json_ints(ids.iter().copied());
        let mut found: HashMap<i64, Album> = repo::albums::load_keyed(
            c,
            "WHERE a.id IN (SELECT value FROM json_each(?1))",
            &[&list],
        )?
        .into_iter()
        .collect();
        results.albums = Page {
            items: ids.iter().filter_map(|id| found.remove(id)).collect(),
            next,
        };
    }
    if q.kinds.artists {
        let vis = visible(a, "x.library_id", q.library);
        let exists = repo::artists::exists(&vis);
        let sql = format!(
            "WITH hits AS MATERIALIZED ({})
             SELECT r.id FROM hits JOIN artists r ON r.id = hits.id WHERE {exists}
             ORDER BY r.search_text = ?2 DESC, hits.score, r.id {window}",
            hits("artists_fts", "artists r", "r", &vis, &exists)
        );
        let (ids, next) = page(ids_for(c, &sql, &expression, &folded)?);
        let list = json_ints(ids.iter().copied());
        let mut found: HashMap<i64, Artist> = repo::artists::load_keyed(
            c,
            a,
            q.library,
            "r.id IN (SELECT value FROM json_each(?1))",
            &[&list],
        )?
        .into_iter()
        .collect();
        results.artists = Page {
            items: ids.iter().filter_map(|id| found.remove(id)).collect(),
            next,
        };
    }
    Ok(results)
}

/// The full-text matches to rank. For a caller who sees every library they are the first
/// matches in index order; for one who sees only some, `filter` is applied before the cap, so
/// matches the caller cannot see do not crowd out those they can. The join that needs costs
/// about as much as the ranking, so it is only paid when it changes the result.
fn hits(fts: &str, table: &str, alias: &str, visible: &str, filter: &str) -> String {
    if visible == "1" {
        format!(
            "SELECT rowid AS id, rank AS score FROM {fts} WHERE {fts} MATCH ?1
             LIMIT {RANKED_CANDIDATES}"
        )
    } else {
        format!(
            "SELECT f.rowid AS id, f.rank AS score FROM {fts} f JOIN {table} ON {alias}.id = f.rowid
             WHERE {fts} MATCH ?1 AND {filter} LIMIT {RANKED_CANDIDATES}"
        )
    }
}

fn ids_for(c: &Connection, sql: &str, expression: &str, folded: &str) -> Result<Vec<i64>, DbError> {
    let mut statement = c.prepare_cached(sql)?;
    let rows = statement.query_map([expression, folded], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
