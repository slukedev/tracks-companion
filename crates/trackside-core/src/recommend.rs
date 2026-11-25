//! Shelves from listens, through Tracks' own engine, so the server and the apps agree on the
//! same plays: the mixes (On Repeat, Rediscover and the rest) from `tracks_mixes`, Get More
//! from `tracks_library::rank`, and the daily artist-spread order from `tracks_mixes`. What the
//! engine has no rule for stays here: this server's top songs, and which songs picks start
//! from. Similarity itself is `crate::similar`. Library songs the caller can see only; never
//! calls out during a request.
//!
//! Privacy: the mixes, Get More's taste and the picks' seeds read only the caller's own
//! listens. Other people's plays count toward top songs only for a song that at least
//! [`TOP_MIN_LISTENERS`] other people played, so no answer shows one person's listening.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{Connection, params};
pub use tracks_jiff::tz::TimeZone;
use tracks_library::{ArtistTaste, Candidate, CandidateKind, Rules};
use tracks_mixes::{Mix, MixOutput, MixPlay};

use crate::access::Access;
use crate::db::DbError;
use crate::ids::{ArtistId, PublicId, SongId};
use crate::memo;
use crate::records::Song;
use crate::repo::artists;
use crate::repo::enrichment as stored;
use crate::repo::songs::{self, SongFilter};
use crate::repo::sql::{json_strings, visible};
use crate::similar;
use crate::time::Millis;

const DAY_MS: i64 = 86_400_000;

/// The most of the caller's own listens the mixes read, newest first.
const MIX_LISTENS: i64 = 50_000;
/// Other people's plays of a song count toward top songs only when at least this many other
/// people played it.
pub const TOP_MIN_LISTENERS: i64 = 2;
/// Picks start from at most this many songs, each by a different artist.
const PICKS_SEEDS: usize = 5;
/// Picks leave out songs the caller played in this many days.
const PICKS_FRESH_DAYS: i64 = 30;
/// Get More weighs the artists in this many of the caller's listens, newest first.
const TASTE_LISTENS: i64 = 5000;
/// Get More looks for artists like this many of the caller's top artists, as the apps do.
const TASTE_LEADING: usize = 5;

/// The listener's time zone by IANA name, UTC when none is given. `None` for a name the
/// system's time zone database does not know.
pub fn zone(name: Option<&str>) -> Option<TimeZone> {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        None => Some(TimeZone::UTC),
        Some(name) => TimeZone::get(name).ok(),
    }
}

/// The caller's newest [`MIX_LISTENS`] listens as the engine's plays, newest first
/// (`started_at`, then listen id), with each listen's id.
#[derive(Debug, Default)]
struct Recent {
    ids: Vec<i64>,
    plays: Vec<MixPlay>,
}

impl Recent {
    /// The newest `limit` listens, as `ORDER BY started_at DESC, id DESC LIMIT limit` gives.
    fn newest(&self, limit: usize) -> &[MixPlay] {
        &self.plays[..limit.min(self.plays.len())]
    }

    /// Whether every listen at or after `since` is here.
    fn covers(&self, since: i64) -> bool {
        self.plays.len() < MIX_LISTENS as usize
            || self.plays.last().is_some_and(|p| p.played_at_ms < since)
    }
}

/// The listen's library song id, when it named one.
fn song_id(play: &MixPlay) -> Option<&str> {
    Some(play.catalog_id.as_str()).filter(|id| !id.is_empty())
}

fn read_listens(
    c: &Connection,
    a: &Access,
    after_id: i64,
    limit: i64,
) -> Result<Vec<(i64, MixPlay)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT id, song_key, song_pid, title, artist, album, started_at FROM listens
         WHERE user_id = ?1 AND id > ?2 ORDER BY started_at DESC, id DESC LIMIT ?3",
    )?;
    let rows = statement.query_map(params![a.user, after_id, limit], |row| {
        Ok((
            row.get(0)?,
            MixPlay {
                identity: row.get(1)?,
                catalog_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                title: row.get(3)?,
                artist_name: row.get(4)?,
                album_title: row.get(5)?,
                artwork_url: None,
                played_at_ms: row.get(6)?,
                is_radio: false,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The caller's recent listens, kept per user until they listen again; then only the new
/// listens are read and merged in. Listens are never deleted while their user exists, so the
/// highest listen id tells whether anything was added.
fn recent(c: &Connection, a: &Access) -> Result<Arc<Recent>, DbError> {
    let marker: i64 = c
        .prepare_cached("SELECT coalesce(max(id), 0) FROM listens WHERE user_id = ?1")?
        .query_row([a.user], |row| row.get(0))?;
    memo::per_user(
        "recent_listens",
        a.user,
        marker,
        |older: Option<(i64, Arc<Recent>)>| {
            let Some((after, held)) = older else {
                let (ids, plays) = read_listens(c, a, 0, MIX_LISTENS)?.into_iter().unzip();
                return Ok(Recent { ids, plays });
            };
            let mut recent = Arc::try_unwrap(held).unwrap_or_else(|shared| Recent {
                ids: shared.ids.clone(),
                plays: shared.plays.clone(),
            });
            // Usually a few new listens near the front: put each where the order wants it.
            for (id, play) in read_listens(c, a, after, MIX_LISTENS)? {
                let at = recent
                    .plays
                    .iter()
                    .zip(&recent.ids)
                    .take_while(|(p, i)| (p.played_at_ms, **i) > (play.played_at_ms, id))
                    .count();
                if at >= MIX_LISTENS as usize {
                    continue;
                }
                recent.ids.insert(at, id);
                recent.plays.insert(at, play);
            }
            recent.ids.truncate(MIX_LISTENS as usize);
            recent.plays.truncate(MIX_LISTENS as usize);
            Ok(recent)
        },
    )
}

/// The visible, present library song for each `(song key, song id)`, in order: the song the
/// listen named when the caller can see it, otherwise the first with the same key, so listens
/// sent by title and artist find their song too. Keys with neither are dropped.
fn resolve(
    c: &Connection,
    a: &Access,
    wanted: &[(&str, Option<&str>)],
) -> Result<Vec<Song>, DbError> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let pids = json_strings(wanted.iter().filter_map(|(_, pid)| *pid));
    let keys = json_strings(wanted.iter().map(|(key, _)| *key));
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
    let mut seen = HashSet::new();
    Ok(wanted
        .iter()
        .filter_map(|(key, pid)| {
            pid.and_then(|pid| by_pid.get(pid))
                .or_else(|| by_key.get(key))
                .map(|song| (*song).clone())
        })
        .filter(|song| seen.insert(song.song_key.clone()))
        .collect())
}

/// Every mix `tracks_mixes` makes from the caller's own listens, at `now` in `zone` (the
/// listener's, for the time-of-day mixes and the daily order). A song's identity is its
/// `song_key`, as in the apps, and it carries the library song id the listen named.
pub fn mixes(
    c: &Connection,
    a: &Access,
    now: Millis,
    zone: &TimeZone,
) -> Result<MixOutput, DbError> {
    memo::cached(
        "mixes",
        || format!("{}/{}", a.user.0, shelf_time(now, zone)),
        |m: &Arc<MixOutput>| m.mixes.iter().map(|m| m.songs.len()).sum(),
        || {
            let recent = recent(c, a)?;
            Ok(Arc::new(tracks_mixes::build_mixes(
                &recent.plays,
                &HashSet::<String>::new(),
                now.0,
                zone,
            )))
        },
    )
    .map(|m| (*m).clone())
}

/// How long a shelf that depends on the time of day is reused while nothing it reads has
/// changed: the engine's answer for a moment in this minute.
const SHELF_REUSE_MS: i64 = 60_000;

/// The minute `now` falls in, and the zone: what a time-of-day shelf is kept for.
fn shelf_time(now: Millis, zone: &TimeZone) -> String {
    format!(
        "{}/{}",
        now.0.div_euclid(SHELF_REUSE_MS),
        zone.iana_name().unwrap_or("fixed")
    )
}

/// The engine's mix with this id (`onRepeat`, `rediscover`, `newFinds`, `allTimeFavorites`,
/// `deepCuts`, `radioFinds`, `throwback.<months>`, `rightNow.<part>.<weekend>`) as library
/// songs in the engine's order. Empty when the engine does not offer that mix for these
/// listens. Songs not on the server, or in libraries the caller cannot see, are left out.
pub fn mix(
    c: &Connection,
    a: &Access,
    now: Millis,
    zone: &TimeZone,
    id: &str,
) -> Result<Vec<Song>, DbError> {
    let output = mixes(c, a, now, zone)?;
    let found = output
        .right_now
        .iter()
        .chain(&output.other_times)
        .chain(&output.mixes)
        .find(|m| m.kind.id() == id);
    mix_songs(c, a, found)
}

fn mix_songs(c: &Connection, a: &Access, found: Option<&Mix>) -> Result<Vec<Song>, DbError> {
    let Some(found) = found else {
        return Ok(Vec::new());
    };
    let wanted: Vec<(&str, Option<&str>)> = found
        .songs
        .iter()
        .map(|s| {
            (
                s.identity.as_str(),
                Some(s.catalog_id.as_str()).filter(|id| !id.is_empty()),
            )
        })
        .collect();
    resolve(c, a, &wanted)
}

/// The most played songs on this server: the caller's plays counted twice, and other
/// people's plays of songs that at least [`TOP_MIN_LISTENERS`] other people played. One song
/// per song key. Empty while nobody has played anything. The engine has no server-wide rule.
pub fn top_songs(c: &Connection, a: &Access, count: u32) -> Result<Vec<Song>, DbError> {
    let count = count.clamp(1, 200);
    memo::cached(
        "top_songs",
        || format!("{}/{count}", memo::access_key(a)),
        Vec::len,
        || compute_top_songs(c, a, count),
    )
}

fn compute_top_songs(c: &Connection, a: &Access, count: u32) -> Result<Vec<Song>, DbError> {
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

fn artist_key(song: &Song) -> String {
    tracks_model::folded(&tracks_model::credits::lead(&song.artist))
}

/// Up to [`PICKS_SEEDS`] songs by different artists, in order.
fn add_seeds(seeds: &mut Vec<Song>, artists: &mut HashSet<String>, from: Vec<Song>) {
    for song in from {
        if seeds.len() >= PICKS_SEEDS {
            return;
        }
        if artists.insert(artist_key(&song)) {
            seeds.push(song);
        }
    }
}

/// The engine's seed for a day in `zone`, as the apps make it.
fn day_seed(now: Millis, zone: &TimeZone, salt: &str) -> u64 {
    let day = tracks_jiff::Timestamp::from_millisecond(now.0)
        .unwrap_or(tracks_jiff::Timestamp::UNIX_EPOCH)
        .to_zoned(zone.clone());
    tracks_mixes::daily_seed(day.year(), day.month(), day.day(), salt)
}

/// Picks for the caller: songs like the ones they play, by way of similar songs. It starts
/// from the engine's On Repeat, New Finds and All-Time Favorites mixes, then the songs they
/// played most recently, then their starred songs; with none of those, this server's top
/// songs, then songs picked at random, so a new server and a new listener get picks too. It
/// leaves out the seeds and what the caller played in the last [`PICKS_FRESH_DAYS`] days, and
/// orders the result with the engine's daily artist-spread shuffle.
pub fn picks(
    c: &Connection,
    a: &Access,
    now: Millis,
    zone: &TimeZone,
    count: u32,
) -> Result<Picks, DbError> {
    let count = count.clamp(1, 200);
    memo::cached(
        "picks",
        || format!("{}/{count}/{}", memo::access_key(a), shelf_time(now, zone)),
        |p: &Picks| p.songs.len() + p.seeds.len(),
        || compute_picks(c, a, now, zone, count as usize),
    )
}

fn compute_picks(
    c: &Connection,
    a: &Access,
    now: Millis,
    zone: &TimeZone,
    count: usize,
) -> Result<Picks, DbError> {
    let mut seeds = Vec::new();
    let mut artists = HashSet::new();
    let output = mixes(c, a, now, zone)?;
    for id in ["onRepeat", "newFinds", "allTimeFavorites"] {
        let found = output.mixes.iter().find(|m| m.kind.id() == id);
        add_seeds(&mut seeds, &mut artists, mix_songs(c, a, found)?);
    }
    let recent = recent(c, a)?;
    if seeds.len() < PICKS_SEEDS {
        let wanted: Vec<(&str, Option<&str>)> = recent
            .newest(200)
            .iter()
            .map(|p| (p.identity.as_str(), song_id(p)))
            .collect();
        add_seeds(&mut seeds, &mut artists, resolve(c, a, &wanted)?);
    }
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
        add_seeds(
            &mut seeds,
            &mut artists,
            songs::random(c, a, &SongFilter::default(), 20)?,
        );
    }
    if seeds.is_empty() {
        return Ok(Picks::default());
    }
    let since = now.0 - PICKS_FRESH_DAYS * DAY_MS;
    let mut taken: HashSet<String> = if recent.covers(since) {
        recent
            .plays
            .iter()
            .take_while(|p| p.played_at_ms >= since)
            .map(|p| p.identity.clone())
            .collect()
    } else {
        c.prepare_cached(
            "SELECT DISTINCT song_key FROM listens WHERE user_id = ?1 AND started_at >= ?2",
        )?
        .query_map(params![a.user, since], |row| row.get(0))?
        .collect::<Result<_, _>>()?
    };
    taken.extend(seeds.iter().map(|s| s.song_key.clone()));
    let per = u32::try_from((count / seeds.len() + 10).min(100)).unwrap_or(100);
    let mut lists = Vec::with_capacity(seeds.len());
    for seed in &seeds {
        lists.push(similar::similar_songs(c, a, &seed.id, per)?.into_iter());
    }
    let mut found = Vec::new();
    while found.len() < count && !lists.is_empty() {
        lists.retain_mut(|list| match list.next() {
            Some(song) => {
                if found.len() < count && taken.insert(song.song_key.clone()) {
                    found.push(song);
                }
                true
            }
            None => false,
        });
    }
    let songs =
        tracks_mixes::fresh_order(&found, artist_key, |_| false, day_seed(now, zone, "picks"));
    Ok(Picks { seeds, songs })
}

/// Get More: what `tracks_library::rank` puts first, as the apps rank what to get next.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GetMore {
    /// Library songs the caller has never played.
    pub songs: Vec<Song>,
    /// `(title, artist)` of songs the server does not have, from stored Last.fm data. Empty
    /// unless asked for.
    pub external: Vec<(String, String)>,
}

/// Get More for the caller. The engine's taste is the caller's own listens by artist (plays,
/// last heard); its similar artists are this server's (local signals and Last.fm) for the
/// caller's top [`TASTE_LEADING`] artists, closest first. Candidates are library songs by
/// those artists that the caller has never played and, with `external`, Last.fm's top songs
/// by them that the caller cannot play here. Empty until the caller has listened.
pub fn get_more(
    c: &Connection,
    a: &Access,
    now: Millis,
    count: u32,
    external: bool,
) -> Result<GetMore, DbError> {
    let count = count.clamp(1, 100);
    memo::cached(
        "get_more_shelf",
        || {
            format!(
                "{}/{count}/{external}/{}",
                memo::access_key(a),
                now.0.div_euclid(SHELF_REUSE_MS)
            )
        },
        |g: &GetMore| g.songs.len() + g.external.len(),
        || compute_get_more(c, a, now, count as usize, external),
    )
}

fn compute_get_more(
    c: &Connection,
    a: &Access,
    now: Millis,
    count: usize,
    external: bool,
) -> Result<GetMore, DbError> {
    let inputs = memo::cached(
        "get_more",
        || format!("{}/{external}", memo::access_key(a)),
        |i: &Arc<MoreInputs>| i.library.len() + i.outside.len(),
        || more_inputs(c, a, external).map(Arc::new),
    )?;
    if inputs.taste.is_empty() {
        return Ok(GetMore::default());
    }
    let rules = Rules {
        limit: count * 2,
        now_ms: now.0,
        ..Rules::default()
    };
    let mut out = GetMore::default();
    let mut chosen: Vec<SongId> = Vec::new();
    let picks = tracks_library::rank(&inputs.candidates, &inputs.taste, &inputs.links, &rules);
    let mut used: HashSet<&str> = HashSet::new();
    for pick in &picks {
        if !used.insert(pick.id.as_str()) {
            continue;
        }
        if let Some(song) = inputs.library.get(&pick.id) {
            if chosen.len() < count {
                chosen.push(song.id.clone());
            }
        } else if let Some(pair) = inputs.outside.get(&pick.id)
            && out.external.len() < count
        {
            out.external.push(pair.clone());
        }
    }
    out.songs = songs::get_many(c, a, &chosen)?;
    Ok(out)
}

/// What Get More ranks: everything but the time, which the engine takes per call.
#[derive(Debug, Default)]
struct MoreInputs {
    taste: Vec<ArtistTaste>,
    links: Vec<tracks_library::SimilarArtist>,
    candidates: Vec<Candidate>,
    library: HashMap<String, songs::TopSong>,
    outside: HashMap<String, (String, String)>,
}

/// The engine's taste: the listens' artists by plays, with when each was last heard.
fn taste_of(listened: &[MixPlay]) -> Vec<ArtistTaste> {
    let mut by_artist: HashMap<String, ArtistTaste> = HashMap::new();
    for l in listened {
        let entry = by_artist
            .entry(tracks_model::folded(&l.artist_name))
            .or_insert_with(|| ArtistTaste {
                name: l.artist_name.clone(),
                plays: 0,
                last_played_ms: Some(l.played_at_ms),
            });
        entry.plays += 1;
    }
    let mut taste: Vec<ArtistTaste> = by_artist.into_values().collect();
    taste.sort_by(|x, y| y.plays.cmp(&x.plays).then_with(|| x.name.cmp(&y.name)));
    taste
}

fn more_inputs(c: &Connection, a: &Access, external: bool) -> Result<MoreInputs, DbError> {
    let recent = recent(c, a)?;
    let listened = recent.newest(TASTE_LISTENS as usize);
    let heard: HashSet<&str> = listened.iter().map(|l| l.identity.as_str()).collect();
    let taste = taste_of(listened);
    if taste.is_empty() {
        return Ok(MoreInputs::default());
    }
    let found = artists::by_names(c, a, taste.iter().map(|t| t.name.as_str()))?;
    let mut names: Vec<(String, Option<ArtistId>)> = taste
        .iter()
        .map(|t| {
            let id = found
                .get(&tracks_model::folded(&t.name))
                .map(|f| f.id.clone());
            (t.name.clone(), id)
        })
        .collect();
    let mut links = Vec::new();
    let leading: Vec<(String, ArtistId)> = names
        .iter()
        .take(TASTE_LEADING)
        .filter_map(|(name, id)| id.clone().map(|id| (name.clone(), id)))
        .collect();
    for (name, id) in leading {
        for like in similar::similar_artists(c, a, &id, 10)? {
            links.push(tracks_library::SimilarArtist {
                of: name.clone(),
                name: like.name.clone(),
            });
            if !names
                .iter()
                .any(|(n, _)| tracks_model::folded(n) == like.identity)
            {
                names.push((like.name, like.artist.map(|ar| ar.id)));
            }
        }
    }
    let ids: Vec<ArtistId> = names.iter().filter_map(|(_, id)| id.clone()).collect();
    let mut tops = songs::top_for_artists(c, a, &ids, 20)?;
    let missing = if external {
        stored::missing_top_many(
            c,
            a,
            &names
                .iter()
                .map(|(name, _)| tracks_model::folded(name))
                .collect::<Vec<_>>(),
            10,
        )?
    } else {
        HashMap::new()
    };
    let mut candidates = Vec::new();
    let mut library: HashMap<String, songs::TopSong> = HashMap::new();
    let mut outside: HashMap<String, (String, String)> = HashMap::new();
    for (name, id) in &names {
        if let Some(id) = id {
            for song in tops.remove(id).unwrap_or_default() {
                let key = song.id.as_str().to_owned();
                if heard.contains(song.song_key.as_str()) || library.contains_key(&key) {
                    continue;
                }
                candidates.push(Candidate {
                    id: key.clone(),
                    kind: CandidateKind::Song,
                    artist: song.artist.clone(),
                    owned_share: 0.0,
                    is_requested: false,
                });
                library.insert(key, song);
            }
        }
        if external {
            for title in missing
                .get(&tracks_model::folded(name))
                .into_iter()
                .flatten()
            {
                let key = format!("x\u{1f}{}", tracks_model::song_key(title, name));
                if outside.contains_key(&key) {
                    continue;
                }
                candidates.push(Candidate {
                    id: key.clone(),
                    kind: CandidateKind::Song,
                    artist: name.clone(),
                    owned_share: 0.0,
                    is_requested: false,
                });
                outside.insert(key, (title.clone(), name.clone()));
            }
        }
    }
    Ok(MoreInputs {
        taste,
        links,
        candidates,
        library,
        outside,
    })
}
