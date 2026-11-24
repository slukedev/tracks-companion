//! Shelves made from listens and the library alone: On Repeat (the caller's own recent plays),
//! top songs (plays on this server) and picks (songs like the caller's favourites that they have
//! not played lately). Library songs the caller can see only. Never calls out during a request.
//!
//! Privacy: On Repeat and the picks' seeds read only the caller's own listens. Other people's
//! plays count toward top songs only for a song that at least [`TOP_MIN_LISTENERS`] other
//! people played, so no answer shows one person's listening.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, params};

use crate::access::Access;
use crate::db::DbError;
use crate::ids::{PublicId, SongId};
use crate::records::Song;
use crate::repo::songs::{self, SongFilter};
use crate::repo::sql::{json_strings, visible};
use crate::similar;
use crate::time::Millis;

const DAY_MS: i64 = 86_400_000;

/// On Repeat looks back this many days.
pub const ON_REPEAT_DAYS: i64 = 30;
/// Plays of a song within [`ON_REPEAT_DAYS`] that put it On Repeat.
pub const ON_REPEAT_MIN_PLAYS: u32 = 2;
/// A play this many days old counts half as much toward On Repeat.
const ON_REPEAT_HALF_LIFE_DAYS: f64 = 10.0;
/// Other people's plays of a song count toward top songs only when at least this many other
/// people played it.
pub const TOP_MIN_LISTENERS: i64 = 2;
/// Picks start from at most this many songs, each by a different artist.
const PICKS_SEEDS: usize = 5;
/// Picks start from the caller's plays in this many days.
const PICKS_SEED_DAYS: i64 = 90;
/// Picks leave out songs the caller played in this many days.
const PICKS_FRESH_DAYS: i64 = 30;
/// The most listens one shelf reads.
const LISTENS_READ: i64 = 5000;

/// One song key in the caller's own listens.
#[derive(Debug)]
struct Played {
    key: String,
    /// The library song the latest listen named, if any.
    pid: Option<String>,
    plays: u32,
    score: f64,
}

/// The caller's own listens since `days` before `now`, by song key, best first: each play
/// counts by its age, halving every `half_life` days.
fn own_played(
    c: &Connection,
    a: &Access,
    now: Millis,
    days: i64,
    half_life: f64,
) -> Result<Vec<Played>, DbError> {
    let since = now.0 - days * DAY_MS;
    let mut statement = c.prepare_cached(
        "SELECT song_key, song_pid, started_at FROM listens
         WHERE user_id = ?1 AND started_at >= ?2 ORDER BY started_at DESC LIMIT ?3",
    )?;
    let mut rows = statement.query(params![a.user, since, LISTENS_READ])?;
    let mut by_key: HashMap<String, Played> = HashMap::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let pid: Option<String> = row.get(1)?;
        let at: i64 = row.get(2)?;
        let age_days = (now.0 - at).max(0) as f64 / DAY_MS as f64;
        let entry = by_key.entry(key.clone()).or_insert(Played {
            key,
            pid: None,
            plays: 0,
            score: 0.0,
        });
        entry.plays += 1;
        entry.score += 0.5_f64.powf(age_days / half_life);
        if entry.pid.is_none() {
            entry.pid = pid;
        }
    }
    let mut out: Vec<Played> = by_key.into_values().collect();
    out.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then(y.plays.cmp(&x.plays))
            .then_with(|| x.key.cmp(&y.key))
    });
    Ok(out)
}

/// The visible, present library song for each played key: the one the listen named when the
/// caller can see it, otherwise the first with the same song key. Listens sent by title and
/// artist only find their song this way.
fn resolve(c: &Connection, a: &Access, played: &[Played]) -> Result<Vec<Song>, DbError> {
    if played.is_empty() {
        return Ok(Vec::new());
    }
    let pids = json_strings(played.iter().filter_map(|p| p.pid.as_deref()));
    let keys = json_strings(played.iter().map(|p| p.key.as_str()));
    let tail = format!(
        "WHERE (s.pid IN (SELECT value FROM json_each(?1))
                OR s.song_key IN (SELECT value FROM json_each(?2)))
           AND s.missing_since IS NULL AND {} ORDER BY s.id",
        visible(a, "s.library_id", None)
    );
    let found = songs::load(c, &tail, &[&pids, &keys])?;
    let mut by_pid: HashMap<&str, &Song> = HashMap::new();
    let mut by_key: HashMap<&str, &Song> = HashMap::new();
    for song in &found {
        by_pid.insert(song.id.as_str(), song);
        by_key.entry(song.song_key.as_str()).or_insert(song);
    }
    Ok(played
        .iter()
        .filter_map(|p| {
            p.pid
                .as_deref()
                .and_then(|pid| by_pid.get(pid))
                .or_else(|| by_key.get(p.key.as_str()))
                .map(|song| (*song).clone())
        })
        .collect())
}

/// On Repeat: songs the caller played at least [`ON_REPEAT_MIN_PLAYS`] times in the last
/// [`ON_REPEAT_DAYS`] days, recent plays counting most. Only the caller's own listens.
pub fn on_repeat(
    c: &Connection,
    a: &Access,
    now: Millis,
    count: u32,
) -> Result<Vec<Song>, DbError> {
    let mut played = own_played(c, a, now, ON_REPEAT_DAYS, ON_REPEAT_HALF_LIFE_DAYS)?;
    played.retain(|p| p.plays >= ON_REPEAT_MIN_PLAYS);
    let mut songs = resolve(c, a, &played)?;
    songs.truncate(count.clamp(1, 200) as usize);
    Ok(songs)
}

/// The most played songs on this server: the caller's plays counted twice, and other
/// people's plays of songs that at least [`TOP_MIN_LISTENERS`] other people played. One song
/// per song key. Empty while nobody has played anything.
pub fn top_songs(c: &Connection, a: &Access, count: u32) -> Result<Vec<Song>, DbError> {
    let count = count.clamp(1, 200);
    let vis = visible(a, "s.library_id", None);
    let ranked: Vec<String> = c
        .prepare_cached(&format!(
            "SELECT ps.song_pid,
                    2 * coalesce(sum(ps.play_count) FILTER (WHERE ps.user_id = ?1), 0)
                    + CASE WHEN count(*) FILTER (WHERE ps.user_id != ?1) >= ?2
                           THEN coalesce(sum(ps.play_count) FILTER (WHERE ps.user_id != ?1), 0)
                           ELSE 0 END AS score
             FROM play_stats ps JOIN songs s ON s.pid = ps.song_pid
             WHERE s.missing_since IS NULL AND {vis}
             GROUP BY ps.song_pid HAVING score > 0
             ORDER BY score DESC, max(ps.last_played_at) DESC, ps.song_pid LIMIT ?3"
        ))?
        .query_map(
            params![a.user, TOP_MIN_LISTENERS, i64::from(count) * 3],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    let ids: Vec<SongId> = ranked
        .iter()
        .filter_map(|pid| SongId::parse(pid).ok())
        .collect();
    let mut keys = HashSet::new();
    Ok(songs::get_many(c, a, &ids)?
        .into_iter()
        .filter(|song| keys.insert(song.song_key.clone()))
        .take(count as usize)
        .collect())
}

/// What picks were made from, and the picks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picks {
    /// The songs the picks start from, by different artists.
    pub seeds: Vec<Song>,
    pub songs: Vec<Song>,
}

/// Up to [`PICKS_SEEDS`] songs by different artists, in order.
fn add_seeds(seeds: &mut Vec<Song>, artists: &mut HashSet<String>, from: Vec<Song>) {
    for song in from {
        if seeds.len() >= PICKS_SEEDS {
            return;
        }
        let artist = song.artist_id.as_ref().map_or_else(
            || tracks_model::folded(&song.artist),
            |id| id.as_str().to_owned(),
        );
        if artists.insert(artist) {
            seeds.push(song);
        }
    }
}

/// Picks for the caller: songs like the ones they play most (the last
/// [`PICKS_SEED_DAYS`] days), then like the songs they starred, taken from each in turn,
/// leaving out what they played in the last [`PICKS_FRESH_DAYS`] days. With no plays or stars
/// of their own it starts from this server's top songs, then from songs picked at random, so a
/// new server and a new listener get picks too. Similar songs bring in stored Last.fm data
/// when there is some.
pub fn picks(c: &Connection, a: &Access, now: Millis, count: u32) -> Result<Picks, DbError> {
    let count = count.clamp(1, 200) as usize;
    let mut seeds = Vec::new();
    let mut artists = HashSet::new();
    let played = own_played(c, a, now, PICKS_SEED_DAYS, ON_REPEAT_HALF_LIFE_DAYS)?;
    add_seeds(&mut seeds, &mut artists, resolve(c, a, &played)?);
    if seeds.len() < PICKS_SEEDS {
        let starred: Vec<SongId> = c
            .prepare_cached(
                "SELECT item_pid FROM stars WHERE user_id = ?1 AND item_kind = 'song'
                 ORDER BY starred_at DESC LIMIT 50",
            )?
            .query_map([a.user], |row| row.get::<_, String>(0))?
            .filter_map(|pid| pid.ok().and_then(|p| SongId::parse(&p).ok()))
            .collect();
        add_seeds(&mut seeds, &mut artists, songs::get_many(c, a, &starred)?);
    }
    if seeds.is_empty() {
        add_seeds(&mut seeds, &mut artists, top_songs(c, a, 20)?);
    }
    if seeds.is_empty() {
        let random = songs::random(c, a, &SongFilter::default(), 20)?
            .into_iter()
            .filter(|s| !s.missing)
            .collect();
        add_seeds(&mut seeds, &mut artists, random);
    }
    if seeds.is_empty() {
        return Ok(Picks::default());
    }
    let since = now.0 - PICKS_FRESH_DAYS * DAY_MS;
    let mut taken: HashSet<String> = c
        .prepare_cached(
            "SELECT DISTINCT song_key FROM listens WHERE user_id = ?1 AND started_at >= ?2",
        )?
        .query_map(params![a.user, since], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    taken.extend(seeds.iter().map(|s| s.song_key.clone()));
    let per = u32::try_from((count / seeds.len() + 10).min(100)).unwrap_or(100);
    let mut lists = Vec::with_capacity(seeds.len());
    for seed in &seeds {
        lists.push(similar::similar_songs(c, a, &seed.id, per)?.into_iter());
    }
    let mut out = Vec::new();
    while out.len() < count && !lists.is_empty() {
        lists.retain_mut(|list| match list.next() {
            Some(song) => {
                if out.len() < count && taken.insert(song.song_key.clone()) {
                    out.push(song);
                }
                true
            }
            None => false,
        });
    }
    Ok(Picks { seeds, songs: out })
}
