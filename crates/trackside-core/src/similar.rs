//! Similar songs and artists from local signals (shared credits, genres, decade, co-listening,
//! playlist co-occurrence) and stored enrichment data. Never calls out during a request.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension};

use crate::access::Access;
use crate::db::DbError;
use crate::ids::{ArtistId, SongId};
use crate::records::{Artist, Song};
use crate::repo;
use crate::repo::sql::{json_ints, visible};

/// A similar artist, which may not be in the library.
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarArtist {
    /// Present when the artist is in the caller's libraries.
    pub artist: Option<Artist>,
    pub name: String,
    pub identity: String,
    /// 0 to 1.
    pub score: f32,
    pub in_library: bool,
}

/// How much each local signal adds to a candidate song's score.
mod weight {
    pub const SAME_ARTIST: f64 = 3.0;
    pub const CO_CREDITED: f64 = 1.5;
    pub const SIMILAR_ARTIST: f64 = 2.0;
    pub const SIMILAR_SONG: f64 = 4.0;
    pub const GENRE: f64 = 1.0;
    pub const DECADE: f64 = 0.5;
    pub const PLAYLIST: f64 = 1.5;
    pub const LISTENED_NEAR: f64 = 1.0;
    /// On the seed's album. With [`SAME_ALBUM`] it nets below other albums by the same artist,
    /// and lets a compilation's other artists in as a last resort.
    pub const ALBUM: f64 = 1.0;
    pub const SAME_ALBUM: f64 = -1.5;
    /// The caller starred the candidate.
    pub const STARRED: f64 = 1.5;
    /// Per play of the candidate by the caller, up to [`OWN_PLAYS_MAX`] plays.
    pub const OWN_PLAY: f64 = 0.3;
    pub const OWN_PLAYS_MAX: i64 = 5;
}

/// How many candidates each signal may contribute, so a huge genre or a prolific artist costs a
/// bounded amount of work.
const PER_SIGNAL: i64 = 300;

/// Two listens this close together by one person count as listened together.
const LISTENED_NEAR_MS: i64 = 30 * 60 * 1000;

struct Seed {
    id: i64,
    pid: SongId,
    song_key: String,
    album: Option<i64>,
    year: Option<i32>,
    artists: Vec<(i64, String)>,
    genres: Vec<i64>,
}

fn add(scores: &mut HashMap<i64, f64>, rows: Vec<(i64, f64)>) {
    for (id, score) in rows {
        *scores.entry(id).or_default() += score;
    }
}

fn scored(
    c: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<(i64, f64)>, DbError> {
    let mut statement = c.prepare_cached(sql)?;
    let rows = statement.query_map(params, |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn load_seed(c: &Connection, vis: &str, seed: &SongId) -> Result<Seed, DbError> {
    type Row = (i64, SongId, String, Option<i64>, Option<i32>);

    let row: Option<Row> = c
        .prepare_cached(&format!(
            "SELECT s.id, s.pid, s.song_key, s.album_id, s.year FROM songs s
             WHERE s.pid = ?1 AND {vis}"
        ))?
        .query_row([seed], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .optional()?;
    let Some((id, pid, song_key, album, year)) = row else {
        return Err(DbError::NotFound);
    };
    let artists = c
        .prepare_cached(
            "SELECT r.id, r.identity FROM song_artists sa JOIN artists r ON r.id = sa.artist_id
             WHERE sa.song_id = ?1 AND sa.role IN ('main', 'featured')",
        )?
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let genres = c
        .prepare_cached("SELECT genre_id FROM song_genres WHERE song_id = ?1")?
        .query_map([id], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(Seed {
        id,
        pid,
        song_key,
        album,
        year,
        artists,
        genres,
    })
}

/// Every local signal's score for candidate songs, by internal id.
fn song_signals(c: &Connection, seed: &Seed) -> Result<HashMap<i64, f64>, DbError> {
    let mut scores: HashMap<i64, f64> = HashMap::new();
    for (artist, identity) in &seed.artists {
        add(
            &mut scores,
            scored(
                c,
                "SELECT song_id, ?2 FROM song_artists WHERE artist_id = ?1
                 AND role IN ('main', 'featured') LIMIT ?3",
                &[artist, &weight::SAME_ARTIST, &PER_SIGNAL],
            )?,
        );
        add(
            &mut scores,
            scored(
                c,
                "SELECT sa3.song_id, ?2 FROM song_artists sa1
                 JOIN song_artists sa2 ON sa2.song_id = sa1.song_id AND sa2.artist_id != sa1.artist_id
                 JOIN song_artists sa3 ON sa3.artist_id = sa2.artist_id
                 WHERE sa1.artist_id = ?1 AND sa3.role IN ('main', 'featured')
                 GROUP BY sa3.song_id LIMIT ?3",
                &[artist, &weight::CO_CREDITED, &PER_SIGNAL],
            )?,
        );
        add(
            &mut scores,
            scored(
                c,
                "SELECT sa.song_id, max(sim.score) * ?2 FROM similar_artists sim
                 JOIN artists r ON r.identity = sim.similar_identity
                 JOIN song_artists sa ON sa.artist_id = r.id AND sa.role IN ('main', 'featured')
                 WHERE sim.artist_identity = ?1 GROUP BY sa.song_id LIMIT ?3",
                &[identity, &weight::SIMILAR_ARTIST, &PER_SIGNAL],
            )?,
        );
    }
    if let Some(album) = seed.album {
        add(
            &mut scores,
            scored(
                c,
                "SELECT id, ?2 FROM songs WHERE album_id = ?1 LIMIT ?3",
                &[&album, &weight::ALBUM, &PER_SIGNAL],
            )?,
        );
    }
    add(
        &mut scores,
        scored(
            c,
            "SELECT s.id, max(sim.score) * ?2 FROM similar_songs sim
             JOIN songs s ON s.song_key = sim.similar_song_key
             WHERE sim.song_key = ?1 GROUP BY s.id LIMIT ?3",
            &[&seed.song_key, &weight::SIMILAR_SONG, &PER_SIGNAL],
        )?,
    );
    add(
        &mut scores,
        scored(
            c,
            "SELECT s.id, min(count(*), 3) * ?2 FROM playlist_entries e1
             JOIN playlist_entries e2 ON e2.playlist_id = e1.playlist_id AND e2.song_pid != e1.song_pid
             JOIN songs s ON s.pid = e2.song_pid
             WHERE e1.song_pid = ?1 GROUP BY s.id LIMIT ?3",
            &[&seed.pid, &weight::PLAYLIST, &PER_SIGNAL],
        )?,
    );
    add(
        &mut scores,
        scored(
            c,
            "SELECT s.id, min(count(*), 3) * ?2 FROM (
                 SELECT user_id, started_at FROM listens WHERE song_pid = ?1
                 ORDER BY started_at DESC LIMIT 50) l1
             JOIN listens l2 ON l2.user_id = l1.user_id
                 AND l2.started_at BETWEEN l1.started_at - ?4 AND l1.started_at + ?4
                 AND l2.song_pid IS NOT NULL AND l2.song_pid != ?1
             JOIN songs s ON s.pid = l2.song_pid
             GROUP BY s.id LIMIT ?3",
            &[
                &seed.pid,
                &weight::LISTENED_NEAR,
                &PER_SIGNAL,
                &LISTENED_NEAR_MS,
            ],
        )?,
    );
    for genre in &seed.genres {
        // A fixed pseudo-random slice of a large genre, so the same seed gives the same songs.
        add(
            &mut scores,
            scored(
                c,
                "SELECT song_id, ?2 FROM song_genres WHERE genre_id = ?1
                 ORDER BY (song_id * 2654435761 + ?4) % 4294967311 LIMIT ?3",
                &[genre, &weight::GENRE, &PER_SIGNAL, &seed.id],
            )?,
        );
    }
    Ok(scores)
}

/// Library songs only, best first: songs by the same and related artists, songs on the same
/// album, songs other users
/// listened to around it or put in the same playlists, songs sharing its genres and decade, and
/// stored enrichment data, each candidate raised by the caller's own stars and plays.
/// `NotFound` when the seed is not visible.
pub fn similar_songs(
    c: &Connection,
    a: &Access,
    seed: &SongId,
    count: u32,
) -> Result<Vec<Song>, DbError> {
    let count = count.clamp(1, 200);
    let vis = visible(a, "s.library_id", None);
    let seed = load_seed(c, &vis, seed)?;
    let mut scores = song_signals(c, &seed)?;
    scores.remove(&seed.id);
    if scores.is_empty() {
        return Ok(Vec::new());
    }
    let list = json_ints(scores.keys().copied());
    let tail = format!(
        "WHERE s.id IN (SELECT value FROM json_each(?1)) AND s.missing_since IS NULL AND {vis}"
    );
    let candidates = repo::songs::load_keyed(c, &tail, &[&list])?;
    let album_of: HashMap<i64, Option<i64>> = {
        let mut statement = c.prepare_cached(
            "SELECT id, album_id FROM songs WHERE id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = statement.query_map([&list], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let personal: HashMap<i64, f64> = scored(
        c,
        "SELECT s.id,
                ?2 * EXISTS (SELECT 1 FROM stars st WHERE st.user_id = ?4
                             AND st.item_kind = 'song' AND st.item_pid = s.pid)
                + ?3 * min(coalesce((SELECT ps.play_count FROM play_stats ps
                                     WHERE ps.user_id = ?4 AND ps.song_pid = s.pid), 0), ?5)
         FROM songs s WHERE s.id IN (SELECT value FROM json_each(?1))",
        &[
            &list,
            &weight::STARRED,
            &weight::OWN_PLAY,
            &a.user,
            &weight::OWN_PLAYS_MAX,
        ],
    )?
    .into_iter()
    .collect();
    let decade = seed.year.map(|y| y / 10);
    let mut ranked: Vec<(f64, i64, Song)> = candidates
        .into_iter()
        .filter(|(_, song)| song.song_key != seed.song_key)
        .map(|(id, song)| {
            let mut score = scores.get(&id).copied().unwrap_or_default()
                + personal.get(&id).copied().unwrap_or_default();
            if decade.is_some() && song.year.map(|y| y / 10) == decade {
                score += weight::DECADE;
            }
            if seed.album.is_some() && album_of.get(&id).copied().flatten() == seed.album {
                score += weight::SAME_ALBUM;
            }
            (score, id, song)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut keys = HashSet::new();
    Ok(ranked
        .into_iter()
        .filter(|(_, _, song)| keys.insert(song.song_key.clone()))
        .take(count as usize)
        .map(|(_, _, song)| song)
        .collect())
}

/// Similar artists' names and scores by identity, from stored data and local signals.
fn artist_signals(
    c: &Connection,
    seed_key: i64,
    identity: &str,
) -> Result<HashMap<String, (String, f64)>, DbError> {
    // identity -> (name, score)
    let mut found: HashMap<String, (String, f64)> = HashMap::new();
    let mut merge = |identity: String, name: String, score: f64| {
        let entry = found.entry(identity).or_insert((name, 0.0));
        entry.1 = (entry.1 + score).min(1.0);
    };
    {
        let mut statement = c.prepare_cached(
            "SELECT similar_identity, similar_name, max(score) FROM similar_artists
             WHERE artist_identity = ?1 GROUP BY similar_identity",
        )?;
        let mut rows = statement.query([identity])?;
        while let Some(row) = rows.next()? {
            merge(
                row.get(0)?,
                row.get(1)?,
                row.get::<_, f64>(2)?.clamp(0.0, 1.0),
            );
        }
    }
    {
        let mut statement = c.prepare_cached(
            "SELECT r.identity, r.name, count(DISTINCT sa1.song_id) FROM song_artists sa1
             JOIN song_artists sa2 ON sa2.song_id = sa1.song_id AND sa2.artist_id != sa1.artist_id
             JOIN artists r ON r.id = sa2.artist_id
             WHERE sa1.artist_id = ?1 GROUP BY r.id ORDER BY 3 DESC LIMIT ?2",
        )?;
        let mut rows = statement.query(rusqlite::params![seed_key, PER_SIGNAL])?;
        while let Some(row) = rows.next()? {
            let shared: i64 = row.get(2)?;
            merge(
                row.get(0)?,
                row.get(1)?,
                (shared as f64 / 5.0).min(1.0) * 0.7,
            );
        }
    }
    {
        // Artists people played within half an hour of the seed's songs, counted over everyone's
        // listens: an aggregate, never one person's history.
        let mut statement = c.prepare_cached(
            "SELECT r.identity, r.name, count(*) FROM (
                 SELECT l.user_id, l.started_at FROM songs s
                 JOIN listens l ON l.song_pid = s.pid
                 WHERE s.artist_id = ?1 ORDER BY l.started_at DESC LIMIT 200) l1
             JOIN listens l2 ON l2.user_id = l1.user_id
                 AND l2.started_at BETWEEN l1.started_at - ?2 AND l1.started_at + ?2
                 AND l2.song_pid IS NOT NULL
             JOIN songs s2 ON s2.pid = l2.song_pid
             JOIN artists r ON r.id = s2.artist_id
             WHERE r.id != ?1 GROUP BY r.id ORDER BY 3 DESC LIMIT 50",
        )?;
        let mut rows = statement.query(rusqlite::params![seed_key, LISTENED_NEAR_MS])?;
        while let Some(row) = rows.next()? {
            let together: i64 = row.get(2)?;
            merge(
                row.get(0)?,
                row.get(1)?,
                (together as f64 / 5.0).min(1.0) * 0.5,
            );
        }
    }
    {
        let genres: Vec<i64> = c
            .prepare_cached(
                "SELECT sg.genre_id FROM song_artists sa JOIN song_genres sg ON sg.song_id = sa.song_id
                 WHERE sa.artist_id = ?1 GROUP BY sg.genre_id ORDER BY count(*) DESC LIMIT 3",
            )?
            .query_map([seed_key], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for genre in genres {
            let mut statement = c.prepare_cached(
                "SELECT r.identity, r.name, count(*) FROM (
                     SELECT song_id FROM song_genres WHERE genre_id = ?1
                     ORDER BY (song_id * 2654435761 + ?3) % 4294967311 LIMIT 2000) g
                 JOIN song_artists sa ON sa.song_id = g.song_id AND sa.role = 'main'
                 JOIN artists r ON r.id = sa.artist_id
                 WHERE r.id != ?2 GROUP BY r.id ORDER BY 3 DESC LIMIT 50",
            )?;
            let mut rows = statement.query(rusqlite::params![genre, seed_key, seed_key])?;
            while let Some(row) = rows.next()? {
                let songs: i64 = row.get(2)?;
                merge(
                    row.get(0)?,
                    row.get(1)?,
                    (songs as f64 / 20.0).min(1.0) * 0.3,
                );
            }
        }
    }
    Ok(found)
}

/// How much the caller likes each in-library artist: a star, and plays of their songs.
fn artist_affinity(
    c: &Connection,
    a: &Access,
    artists: &HashMap<String, Artist>,
) -> Result<HashMap<String, f64>, DbError> {
    if artists.is_empty() {
        return Ok(HashMap::new());
    }
    let list = crate::repo::sql::json_strings(artists.keys().map(String::as_str));
    let mut statement = c.prepare_cached(
        "SELECT r.identity,
                0.1 * EXISTS (SELECT 1 FROM stars st WHERE st.user_id = ?2
                              AND st.item_kind = 'artist' AND st.item_pid = r.pid)
                + 0.02 * min(coalesce((SELECT sum(ps.play_count) FROM songs s
                                       JOIN play_stats ps ON ps.song_pid = s.pid
                                       WHERE s.artist_id = r.id AND ps.user_id = ?2), 0), 5)
         FROM artists r WHERE r.identity IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = statement.query_map(rusqlite::params![list, a.user], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Similar artists from stored data (enrichment and earlier local results) and from local
/// signals: artists credited together, artists played around the seed's songs, and artists
/// sharing the seed's main genres, raised a
/// little by the caller's stars and plays. Artists not in the caller's libraries are included
/// with `in_library: false`. `NotFound` when the seed is not visible.
pub fn similar_artists(
    c: &Connection,
    a: &Access,
    seed: &ArtistId,
    count: u32,
) -> Result<Vec<SimilarArtist>, DbError> {
    let mut out = ranked_artists(c, a, seed)?;
    out.truncate(count.clamp(1, 100) as usize);
    Ok(out)
}

/// [`similar_artists`] in the caller's libraries only, chosen before the list is cut to
/// `count`, so artists from elsewhere cannot crowd them out.
pub fn similar_artists_in_library(
    c: &Connection,
    a: &Access,
    seed: &ArtistId,
    count: u32,
) -> Result<Vec<SimilarArtist>, DbError> {
    let mut out = ranked_artists(c, a, seed)?;
    out.retain(|s| s.in_library);
    out.truncate(count.clamp(1, 100) as usize);
    Ok(out)
}

/// Every similar artist, best first.
fn ranked_artists(
    c: &Connection,
    a: &Access,
    seed: &ArtistId,
) -> Result<Vec<SimilarArtist>, DbError> {
    let Some((seed_key, seed_artist)) =
        repo::artists::load_keyed(c, a, None, "r.pid = ?1", &[seed])?
            .into_iter()
            .next()
    else {
        return Err(DbError::NotFound);
    };
    let mut found = artist_signals(c, seed_key, &seed_artist.identity)?;
    found.remove(&seed_artist.identity);
    let identities: Vec<&String> = found.keys().collect();
    let list = crate::repo::sql::json_strings(identities.iter().map(|s| s.as_str()));
    let in_library: HashMap<String, Artist> = repo::artists::load_keyed(
        c,
        a,
        None,
        "r.identity IN (SELECT value FROM json_each(?1))",
        &[&list],
    )?
    .into_iter()
    .map(|(_, artist)| (artist.identity.clone(), artist))
    .collect();
    let affinity = artist_affinity(c, a, &in_library)?;
    let mut out: Vec<SimilarArtist> = found
        .into_iter()
        .map(|(identity, (name, score))| {
            let artist = in_library.get(&identity).cloned();
            let score = (score + affinity.get(&identity).copied().unwrap_or_default()).min(1.0);
            SimilarArtist {
                in_library: artist.is_some(),
                name: artist.as_ref().map_or(name, |a| a.name.clone()),
                artist,
                identity,
                score: score as f32,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.in_library.cmp(&a.in_library))
            .then_with(|| a.identity.cmp(&b.identity))
    });
    Ok(out)
}
