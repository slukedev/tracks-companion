//! Stored Last.fm enrichment: when each artist and song was looked up, and what came back.
//! Similar artists and songs go to `similar_artists` and `similar_songs` with source
//! `lastfm`, where [`crate::similar`] merges them with local signals.

use rusqlite::{Connection, OptionalExtension};

use crate::access::Access;
use crate::db::{DbError, WriteTx};
use crate::ids::ExternalArtId;
use crate::repo::sql::visible;
use crate::time::Millis;

/// What a lookup was for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Keyed by artist identity.
    Artist,
    /// Keyed by song key.
    Song,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Song => "song",
        }
    }
}

/// An artist or song Last.fm relates to the one looked up. `score` is 0 to 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Related {
    pub title: String,
    pub artist: String,
    pub score: f64,
}

/// What Last.fm said about an artist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtistFacts {
    /// Similar artists by name; `title` is unused.
    pub similar: Vec<Related>,
    /// Most played song titles, most played first.
    pub top: Vec<String>,
    pub bio: Option<String>,
}

/// Lookups that were never made or have expired, most played artists first.
pub fn due_artists(
    c: &Connection,
    now: Millis,
    limit: u32,
) -> Result<Vec<(String, String)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT r.identity, r.name FROM artists r
         LEFT JOIN enrichment_fetches f ON f.kind = 'artist' AND f.key = r.identity
         WHERE f.key IS NULL OR f.expires_at <= ?1
         ORDER BY (SELECT coalesce(sum(ps.play_count), 0) FROM songs s
                   JOIN play_stats ps ON ps.song_pid = s.pid WHERE s.artist_id = r.id) DESC,
                  r.id
         LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![now, limit], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Songs someone has played whose lookup was never made or has expired, most played first,
/// as `(song_key, title, artist)`.
pub fn due_songs(
    c: &Connection,
    now: Millis,
    limit: u32,
) -> Result<Vec<(String, String, String)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT s.song_key, min(s.title), min(s.artist) FROM play_stats ps
         JOIN songs s ON s.pid = ps.song_pid
         LEFT JOIN enrichment_fetches f ON f.kind = 'song' AND f.key = s.song_key
         WHERE f.key IS NULL OR f.expires_at <= ?1
         GROUP BY s.song_key ORDER BY sum(ps.play_count) DESC, s.song_key
         LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![now, limit], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Whether a lookup is stored and has not expired, found or not.
pub fn is_fresh(c: &Connection, kind: Kind, key: &str, now: Millis) -> Result<bool, DbError> {
    Ok(c.prepare_cached(
        "SELECT 1 FROM enrichment_fetches WHERE kind = ?1 AND key = ?2 AND expires_at > ?3",
    )?
    .query_row(rusqlite::params![kind.as_str(), key, now], |_| Ok(()))
    .optional()?
    .is_some())
}

fn mark(tx: &WriteTx<'_>, kind: Kind, key: &str, found: bool, ttl_ms: i64) -> Result<(), DbError> {
    tx.touch();
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO enrichment_fetches (kind, key, found, fetched_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (kind, key) DO UPDATE SET found = excluded.found,
             fetched_at = excluded.fetched_at, expires_at = excluded.expires_at",
    )?
    .execute(rusqlite::params![
        kind.as_str(),
        key,
        found,
        now,
        Millis(now.0.saturating_add(ttl_ms))
    ])?;
    Ok(())
}

/// Records that Last.fm does not know `key`, and drops what it said before.
pub fn store_unknown(tx: &WriteTx<'_>, kind: Kind, key: &str, ttl_ms: i64) -> Result<(), DbError> {
    match kind {
        Kind::Artist => clear_artist(tx, key)?,
        Kind::Song => clear_song(tx, key)?,
    }
    mark(tx, kind, key, false, ttl_ms)
}

fn clear_artist(tx: &WriteTx<'_>, identity: &str) -> Result<(), DbError> {
    tx.prepare_cached(
        "DELETE FROM similar_artists WHERE artist_identity = ?1 AND source = 'lastfm'",
    )?
    .execute([identity])?;
    tx.prepare_cached("DELETE FROM enrichment_top_songs WHERE artist_identity = ?1")?
        .execute([identity])?;
    tx.prepare_cached("DELETE FROM enrichment_artists WHERE identity = ?1")?
        .execute([identity])?;
    Ok(())
}

fn clear_song(tx: &WriteTx<'_>, song_key: &str) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM similar_songs WHERE song_key = ?1 AND source = 'lastfm'")?
        .execute([song_key])?;
    Ok(())
}

/// Replaces what is stored about the artist with `identity`.
pub fn store_artist(
    tx: &WriteTx<'_>,
    identity: &str,
    artist_name: &str,
    facts: &ArtistFacts,
    ttl_ms: i64,
) -> Result<(), DbError> {
    clear_artist(tx, identity)?;
    let now = tx.now();
    {
        let mut insert = tx.prepare_cached(
            "INSERT OR IGNORE INTO similar_artists
                 (artist_identity, similar_identity, similar_name, score, source, fetched_at)
             VALUES (?1, ?2, ?3, ?4, 'lastfm', ?5)",
        )?;
        for related in &facts.similar {
            let similar = tracks_model::folded(&related.artist);
            if similar.is_empty() || similar == identity {
                continue;
            }
            insert.execute(rusqlite::params![
                identity,
                similar,
                related.artist,
                related.score.clamp(0.0, 1.0),
                now
            ])?;
        }
    }
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO enrichment_top_songs (artist_identity, rank, song_key, title)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (rank, title) in facts.top.iter().enumerate() {
            insert.execute(rusqlite::params![
                identity,
                rank as i64,
                tracks_model::song_key(title, artist_name),
                title
            ])?;
        }
    }
    if let Some(bio) = facts.bio.as_deref().filter(|b| !b.is_empty()) {
        tx.prepare_cached("INSERT INTO enrichment_artists (identity, bio) VALUES (?1, ?2)")?
            .execute([identity, bio])?;
    }
    mark(tx, Kind::Artist, identity, true, ttl_ms)
}

/// Replaces the songs Last.fm relates to `song_key`.
pub fn store_song(
    tx: &WriteTx<'_>,
    song_key: &str,
    similar: &[Related],
    ttl_ms: i64,
) -> Result<(), DbError> {
    clear_song(tx, song_key)?;
    let now = tx.now();
    let mut insert = tx.prepare_cached(
        "INSERT OR IGNORE INTO similar_songs (song_key, similar_song_key, score, source, fetched_at)
         VALUES (?1, ?2, ?3, 'lastfm', ?4)",
    )?;
    let mut name = tx.prepare_cached(
        "INSERT INTO enrichment_song_names (song_key, title, artist) VALUES (?1, ?2, ?3)
         ON CONFLICT (song_key) DO UPDATE SET title = excluded.title, artist = excluded.artist",
    )?;
    for related in similar {
        let key = tracks_model::song_key(&related.title, &related.artist);
        if key == song_key {
            continue;
        }
        insert.execute(rusqlite::params![
            song_key,
            key,
            related.score.clamp(0.0, 1.0),
            now
        ])?;
        name.execute([&key, &related.title, &related.artist])?;
    }
    drop((insert, name));
    mark(tx, Kind::Song, song_key, true, ttl_ms)
}

/// Drops names no Last.fm `similar_songs` row points at any more.
pub fn prune_names(tx: &WriteTx<'_>) -> Result<usize, DbError> {
    Ok(tx
        .prepare_cached(
            "DELETE FROM enrichment_song_names WHERE song_key NOT IN
                 (SELECT similar_song_key FROM similar_songs WHERE source = 'lastfm')",
        )?
        .execute([])?)
}

/// Artists and songs Last.fm knew, and when the latest lookup was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub artists: u64,
    pub songs: u64,
    pub last_fetch: Option<Millis>,
}

pub fn counts(c: &Connection) -> Result<Counts, DbError> {
    Ok(c.prepare_cached(
        "SELECT coalesce(sum(kind = 'artist' AND found), 0), coalesce(sum(kind = 'song' AND found), 0),
                max(fetched_at)
         FROM enrichment_fetches",
    )?
    .query_row([], |row| {
        Ok(Counts {
            artists: row.get::<_, i64>(0)?.max(0) as u64,
            songs: row.get::<_, i64>(1)?.max(0) as u64,
            last_fetch: row.get(2)?,
        })
    })?)
}

/// The artist's biography summary, when Last.fm has one.
pub fn bio(c: &Connection, identity: &str) -> Result<Option<String>, DbError> {
    Ok(
        c.prepare_cached("SELECT bio FROM enrichment_artists WHERE identity = ?1")?
            .query_row([identity], |row| row.get(0))
            .optional()?,
    )
}

/// Songs Last.fm relates to `song_key` that the caller cannot play here, best first, as
/// `(title, artist)`.
pub fn missing_similar(
    c: &Connection,
    a: &Access,
    song_key: &str,
    count: u32,
) -> Result<Vec<(String, String)>, DbError> {
    let vis = visible(a, "s.library_id", None);
    let mut statement = c.prepare_cached(&format!(
        "SELECT n.title, n.artist FROM similar_songs sim
         JOIN enrichment_song_names n ON n.song_key = sim.similar_song_key
         WHERE sim.song_key = ?1 AND sim.source = 'lastfm'
           AND NOT EXISTS (SELECT 1 FROM songs s WHERE s.song_key = sim.similar_song_key
                           AND s.missing_since IS NULL AND {vis})
         ORDER BY sim.score DESC, n.song_key LIMIT ?2"
    ))?;
    let rows = statement.query_map(rusqlite::params![song_key, count], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// An artist's most played songs on Last.fm that the caller cannot play here, as titles.
pub fn missing_top(
    c: &Connection,
    a: &Access,
    identity: &str,
    count: u32,
) -> Result<Vec<String>, DbError> {
    let vis = visible(a, "s.library_id", None);
    let mut statement = c.prepare_cached(&format!(
        "SELECT t.title FROM enrichment_top_songs t
         WHERE t.artist_identity = ?1
           AND NOT EXISTS (SELECT 1 FROM songs s WHERE s.song_key = t.song_key
                           AND s.missing_since IS NULL AND {vis})
         ORDER BY t.rank LIMIT ?2"
    ))?;
    let rows = statement.query_map(rusqlite::params![identity, count], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// [`missing_top`] for many artist identities at once.
pub fn missing_top_many(
    c: &Connection,
    a: &Access,
    identities: &[String],
    count: u32,
) -> Result<std::collections::HashMap<String, Vec<String>>, DbError> {
    let vis = visible(a, "s.library_id", None);
    let list = crate::repo::sql::json_strings(identities.iter().map(String::as_str));
    let mut statement = c.prepare_cached(&format!(
        "SELECT artist_identity, title FROM (
             SELECT t.artist_identity, t.title, t.rank, row_number() OVER (
                 PARTITION BY t.artist_identity ORDER BY t.rank) AS n
             FROM enrichment_top_songs t
             WHERE t.artist_identity IN (SELECT value FROM json_each(?1))
               AND NOT EXISTS (SELECT 1 FROM songs s WHERE s.song_key = t.song_key
                               AND s.missing_since IS NULL AND {vis}))
         WHERE n <= ?2 ORDER BY artist_identity, rank"
    ))?;
    let mut out: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let mut rows = statement.query(rusqlite::params![list, count])?;
    while let Some(row) = rows.next()? {
        out.entry(row.get(0)?).or_default().push(row.get(1)?);
    }
    Ok(out)
}

/// Cover art stored for each of these song keys: `Some(id)` with an image, `None` when Last.fm
/// had none. Keys never looked up are left out.
pub fn external_art(
    c: &Connection,
    keys: &[String],
) -> Result<std::collections::HashMap<String, Option<ExternalArtId>>, DbError> {
    let list = crate::repo::sql::json_strings(keys.iter().map(String::as_str));
    let mut statement = c.prepare_cached(
        "SELECT song_key, art_id FROM external_art
         WHERE song_key IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = statement.query_map([list], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Song keys among `keys` whose art was never looked up or has expired.
pub fn art_due(c: &Connection, keys: &[String], now: Millis) -> Result<Vec<String>, DbError> {
    let list = crate::repo::sql::json_strings(keys.iter().map(String::as_str));
    let mut statement = c.prepare_cached(
        "SELECT k.value FROM json_each(?1) k
         WHERE NOT EXISTS (SELECT 1 FROM external_art a WHERE a.song_key = k.value
                           AND a.expires_at > ?2)",
    )?;
    let rows = statement.query_map(rusqlite::params![list, now], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Whether the art for `key` is stored and has not expired.
pub fn art_is_fresh(c: &Connection, key: &str, now: Millis) -> Result<bool, DbError> {
    Ok(
        c.prepare_cached("SELECT 1 FROM external_art WHERE song_key = ?1 AND expires_at > ?2")?
            .query_row(rusqlite::params![key, now], |_| Ok(()))
            .optional()?
            .is_some(),
    )
}

/// Records the art found for `key`, or that there is none.
pub fn store_art(
    tx: &WriteTx<'_>,
    key: &str,
    art: Option<&ExternalArtId>,
    ttl_ms: i64,
) -> Result<(), DbError> {
    tx.touch();
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO external_art (song_key, art_id, fetched_at, expires_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (song_key) DO UPDATE SET art_id = excluded.art_id,
             fetched_at = excluded.fetched_at, expires_at = excluded.expires_at",
    )?
    .execute(rusqlite::params![
        key,
        art,
        now,
        Millis(now.0.saturating_add(ttl_ms))
    ])?;
    Ok(())
}

/// Whether some suggestion shows this image.
pub fn art_in_use(c: &Connection, id: &ExternalArtId) -> Result<bool, DbError> {
    Ok(
        c.prepare_cached("SELECT EXISTS (SELECT 1 FROM external_art WHERE art_id = ?1)")?
            .query_row([id], |row| row.get(0))?,
    )
}

/// The image is no longer in the cache: look it up again the next time it is suggested.
pub fn art_lost(tx: &WriteTx<'_>, id: &ExternalArtId) -> Result<(), DbError> {
    tx.touch();
    tx.prepare_cached("UPDATE external_art SET expires_at = 0 WHERE art_id = ?1")?
        .execute([id])?;
    Ok(())
}
