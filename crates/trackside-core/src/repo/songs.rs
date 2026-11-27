//! Songs.

use std::collections::HashMap;

use rusqlite::types::ToSql;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::{join_rel, json_ints, json_strings, opt_u8, opt_u32, to_u32, to_u64, visible};
use crate::access::Access;
use crate::db::DbError;
use crate::ids::{AlbumId, ArtistId, FolderId, LibraryKey, PublicId, SongId, UserKey};
use crate::records::{
    AudioFormat, Codec, Container, Credit, CreditRole, Explicit, ReplayGain, Song, SongFile,
    SongMbz,
};

/// Narrows random and listed songs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SongFilter {
    pub library: Option<LibraryKey>,
    /// Folded genre name.
    pub genre: Option<String>,
    pub year_from: Option<i32>,
    pub year_to: Option<i32>,
}

/// The columns [`map_row`] reads, in order. Every song query selects these from `songs s`
/// joined as below. The `+` keeps SQLite from starting at `libraries` or `folders` and reading
/// songs through their `library_id` or `folder_id` index, which at 500k songs means reading
/// and sorting most of the table.
pub(crate) const SONG_SELECT: &str =
    "SELECT s.id, s.pid, l.pid, s.title, s.artist, ar.pid, al.pid, s.album,
        s.album_artist, s.track_number, s.disc_number, s.disc_subtitle, s.year, s.release_date,
        s.duration_ms, s.container, s.codec, s.lossless, s.bitrate_kbps, s.sample_rate,
        s.bit_depth, s.channels, s.file_size, s.artwork_id, s.song_key, s.rg_track_gain,
        s.rg_track_peak, s.rg_album_gain, s.rg_album_peak, s.mbz_recording_id, s.mbz_track_id,
        s.mbz_release_id, s.isrc, s.bpm, s.comment, s.explicit, s.has_lyrics, f.pid, s.added_at,
        s.updated_at, s.missing_since, s.content_version
    FROM songs s
    JOIN libraries l ON l.id = +s.library_id
    JOIN folders f ON f.id = +s.folder_id
    LEFT JOIN artists ar ON ar.id = s.artist_id
    LEFT JOIN albums al ON al.id = s.album_id";

fn stored<T>(value: Option<T>, what: &str) -> rusqlite::Result<T> {
    value.ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown stored {what}").into(),
        )
    })
}

fn map_row(row: &Row<'_>) -> rusqlite::Result<(i64, Song)> {
    let container = stored(Container::parse(&row.get::<_, String>(15)?), "container")?;
    let codec = stored(Codec::parse(&row.get::<_, String>(16)?), "codec")?;
    let gains: [Option<f64>; 4] = [row.get(25)?, row.get(26)?, row.get(27)?, row.get(28)?];
    let replay_gain = gains.iter().any(Option::is_some).then_some(ReplayGain {
        track_gain: gains[0],
        track_peak: gains[1],
        album_gain: gains[2],
        album_peak: gains[3],
    });
    let explicit: Option<String> = row.get(35)?;
    let missing_since: Option<i64> = row.get(40)?;
    let song = Song {
        id: row.get(1)?,
        library: row.get(2)?,
        title: row.get(3)?,
        artist: row.get(4)?,
        artist_id: row.get(5)?,
        credits: Vec::new(),
        album: row.get(7)?,
        album_id: row.get(6)?,
        album_artist: row.get(8)?,
        track_number: opt_u32(row.get(9)?),
        disc_number: opt_u32(row.get(10)?),
        disc_subtitle: row.get(11)?,
        year: row.get(12)?,
        release_date: row.get(13)?,
        genres: Vec::new(),
        duration_ms: to_u32(row.get(14)?),
        format: AudioFormat {
            container,
            codec,
            content_type: container.content_type(),
            lossless: row.get(17)?,
            bitrate_kbps: opt_u32(row.get(18)?),
            sample_rate: opt_u32(row.get(19)?),
            bit_depth: opt_u8(row.get(20)?),
            channels: opt_u8(row.get(21)?),
            size: to_u64(row.get(22)?),
        },
        artwork: row.get(23)?,
        song_key: row.get(24)?,
        replay_gain,
        mbz: SongMbz {
            recording: row.get(29)?,
            track: row.get(30)?,
            release: row.get(31)?,
        },
        isrc: row.get(32)?,
        bpm: opt_u32(row.get(33)?),
        comment: row.get(34)?,
        explicit: explicit.as_deref().and_then(Explicit::parse),
        has_lyrics: row.get(36)?,
        folder: row.get(37)?,
        added_at: row.get(38)?,
        updated_at: row.get(39)?,
        missing: missing_since.is_some(),
        content_version: row.get(41)?,
    };
    Ok((row.get(0)?, song))
}

/// Runs `SONG_SELECT <tail>` and fills in credits and genres. Rows come back in query order.
pub(crate) fn load(
    c: &Connection,
    tail: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<Song>, DbError> {
    Ok(load_keyed(c, tail, params)?
        .into_iter()
        .map(|(_, song)| song)
        .collect())
}

/// Like [`load`], keeping each song's internal id.
pub(crate) fn load_keyed(
    c: &Connection,
    tail: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<(i64, Song)>, DbError> {
    let mut statement = c.prepare_cached(&format!("{SONG_SELECT} {tail}"))?;
    let mut songs: Vec<(i64, Song)> = statement
        .query_map(params, map_row)?
        .collect::<Result<_, _>>()?;
    attach_details(c, &mut songs)?;
    Ok(songs)
}

fn role_rank(role: CreditRole) -> u8 {
    match role {
        CreditRole::Main => 0,
        CreditRole::Featured => 1,
        CreditRole::AlbumArtist => 2,
        CreditRole::Composer => 3,
    }
}

fn attach_details(c: &Connection, songs: &mut [(i64, Song)]) -> Result<(), DbError> {
    if songs.is_empty() {
        return Ok(());
    }
    let ids = json_ints(songs.iter().map(|(id, _)| *id));
    let mut credits: HashMap<i64, Vec<(u8, i64, Credit)>> = HashMap::new();
    let mut statement = c.prepare_cached(
        "SELECT sa.song_id, a.pid, a.name, sa.role, sa.position
         FROM song_artists sa JOIN artists a ON a.id = sa.artist_id
         WHERE sa.song_id IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = statement.query([&ids])?;
    while let Some(row) = rows.next()? {
        let Some(role) = CreditRole::parse(&row.get::<_, String>(3)?) else {
            continue;
        };
        let credit = Credit {
            artist: row.get(1)?,
            name: row.get(2)?,
            role,
        };
        credits
            .entry(row.get(0)?)
            .or_default()
            .push((role_rank(role), row.get(4)?, credit));
    }
    let mut genres: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    let mut statement = c.prepare_cached(
        "SELECT sg.song_id, g.name, sg.position
         FROM song_genres sg JOIN genres g ON g.id = sg.genre_id
         WHERE sg.song_id IN (SELECT value FROM json_each(?1))",
    )?;
    let mut rows = statement.query([&ids])?;
    while let Some(row) = rows.next()? {
        genres
            .entry(row.get(0)?)
            .or_default()
            .push((row.get(2)?, row.get(1)?));
    }
    for (id, song) in songs.iter_mut() {
        if let Some(mut list) = credits.remove(id) {
            list.sort_by_key(|(rank, position, _)| (*rank, *position));
            song.credits = list.into_iter().map(|(_, _, credit)| credit).collect();
        }
        if let Some(mut list) = genres.remove(id) {
            list.sort_by_key(|(position, _)| *position);
            song.genres = list.into_iter().map(|(_, name)| name).collect();
        }
    }
    Ok(())
}

pub fn get(c: &Connection, a: &Access, id: &SongId) -> Result<Option<Song>, DbError> {
    let tail = format!("WHERE s.pid = ?1 AND {}", visible(a, "s.library_id", None));
    Ok(load(c, &tail, &[id])?.into_iter().next())
}

/// Songs in request order; unknown or invisible ids are left out.
pub fn get_many(c: &Connection, a: &Access, ids: &[SongId]) -> Result<Vec<Song>, DbError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = json_strings(ids.iter().map(PublicId::as_str));
    let tail = format!(
        "WHERE s.pid IN (SELECT value FROM json_each(?1)) AND {}",
        visible(a, "s.library_id", None)
    );
    let mut found: HashMap<SongId, Song> = load(c, &tail, &[&list])?
        .into_iter()
        .map(|song| (song.id.clone(), song))
        .collect();
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

/// Where the song's bytes are. `None` for unknown, invisible or missing songs.
pub fn file(c: &Connection, a: &Access, id: &SongId) -> Result<Option<SongFile>, DbError> {
    let sql = format!(
        "SELECT s.pid, s.library_id, l.root, s.rel_path, s.file_size, s.file_mtime_ns,
                s.content_version, s.container, s.codec, s.lossless, s.bitrate_kbps,
                s.sample_rate, s.bit_depth, s.channels, s.duration_ms
         FROM songs s JOIN libraries l ON l.id = +s.library_id
         WHERE s.pid = ?1 AND s.missing_since IS NULL AND {}",
        visible(a, "s.library_id", None)
    );
    let found = c
        .prepare_cached(&sql)?
        .query_row([id], |row| {
            let root: Vec<u8> = row.get(2)?;
            let rel: Vec<u8> = row.get(3)?;
            let container: String = row.get(7)?;
            let codec: String = row.get(8)?;
            Ok((
                row.get::<_, SongId>(0)?,
                row.get::<_, LibraryKey>(1)?,
                join_rel(&super::sql::bytes_path(&root), &rel),
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                (container, codec),
                row.get::<_, bool>(9)?,
                (
                    opt_u32(row.get(10)?),
                    opt_u32(row.get(11)?),
                    opt_u8(row.get(12)?),
                    opt_u8(row.get(13)?),
                ),
                row.get::<_, i64>(14)?,
            ))
        })
        .optional()?;
    let Some((song, library, path, size, mtime_ns, content_version, kinds, lossless, props, dur)) =
        found
    else {
        return Ok(None);
    };
    let (Some(container), Some(codec)) = (Container::parse(&kinds.0), Codec::parse(&kinds.1))
    else {
        return Err(DbError::Invalid(format!(
            "song {song} has an unknown format"
        )));
    };
    Ok(Some(SongFile {
        song,
        library,
        path,
        size: to_u64(size),
        mtime_ns,
        content_version,
        format: AudioFormat {
            container,
            codec,
            content_type: container.content_type(),
            lossless,
            bitrate_kbps: props.0,
            sample_rate: props.1,
            bit_depth: props.2,
            channels: props.3,
            size: to_u64(size),
        },
        duration_ms: to_u32(dur),
    }))
}

/// An album's songs by disc and track.
pub fn by_album(c: &Connection, a: &Access, album: &AlbumId) -> Result<Vec<Song>, DbError> {
    let tail = format!(
        "WHERE s.album_id = (SELECT id FROM albums WHERE pid = ?1) AND {}
         ORDER BY coalesce(s.disc_number, 1), coalesce(s.track_number, 0), s.sort_title, s.id",
        visible(a, "s.library_id", None)
    );
    load(c, &tail, &[album])
}

/// A folder's songs by file name.
pub fn by_folder(c: &Connection, a: &Access, folder: &FolderId) -> Result<Vec<Song>, DbError> {
    let tail = format!(
        "WHERE s.folder_id = (SELECT id FROM folders WHERE pid = ?1) AND {}
         ORDER BY s.rel_path",
        visible(a, "s.library_id", None)
    );
    load(c, &tail, &[folder])
}

/// The genre and year conditions, bound as `?2` (folded genre), `?3` and `?4`.
const FILTER_SQL: &str = "AND (?2 IS NULL OR EXISTS (SELECT 1 FROM song_genres sg
        JOIN genres g ON g.id = sg.genre_id WHERE sg.song_id = s.id AND g.name_key = ?2))
    AND (?3 IS NULL OR s.year >= ?3) AND (?4 IS NULL OR s.year <= ?4)";

/// Present songs picked at random. Without a genre or year filter it samples internal ids,
/// which stays fast on large libraries; with one it lets SQLite shuffle the matches.
pub fn random(
    c: &Connection,
    a: &Access,
    filter: &SongFilter,
    count: u32,
) -> Result<Vec<Song>, DbError> {
    let count = count.clamp(1, 500);
    let lib = visible(a, "s.library_id", filter.library);
    let genre = filter.genre.as_deref().map(tracks_model::folded);
    let narrowed = filter.genre.is_some() || filter.year_from.is_some() || filter.year_to.is_some();
    let ids: Vec<i64> = if narrowed {
        let sql = format!(
            "SELECT s.id FROM songs s WHERE s.missing_since IS NULL AND {lib} AND ?1 = ?1
             {FILTER_SQL} ORDER BY random() LIMIT {count}"
        );
        c.prepare_cached(&sql)?
            .query_map(params![0, genre, filter.year_from, filter.year_to], |row| {
                row.get(0)
            })?
            .collect::<Result<_, _>>()?
    } else {
        sample_ids(c, &lib, count)?
    };
    let list = json_ints(ids.iter().copied());
    let mut found: HashMap<i64, Song> = load_keyed(
        c,
        "WHERE s.id IN (SELECT value FROM json_each(?1))",
        &[&list],
    )?
    .into_iter()
    .collect();
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

fn random_u64() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let mut first = [0u8; 8];
    first.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(first)
}

fn sample_ids(c: &Connection, lib: &str, count: u32) -> Result<Vec<i64>, DbError> {
    // Over the whole table, which SQLite answers from the ends of the rowid; probes outside the
    // caller's libraries move on to the next visible song.
    let (low, high): (Option<i64>, Option<i64>) =
        c.query_row("SELECT min(id), max(id) FROM songs", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let (Some(low), Some(high)) = (low, high) else {
        return Ok(Vec::new());
    };
    let span = u64::try_from(high - low + 1).unwrap_or(1).max(1);
    let mut next = c.prepare_cached(&format!(
        "SELECT s.id FROM songs s WHERE s.id >= ?1 AND s.missing_since IS NULL AND {lib}
         ORDER BY s.id LIMIT 1"
    ))?;
    let mut first = c.prepare_cached(&format!(
        "SELECT s.id FROM songs s WHERE s.missing_since IS NULL AND {lib} ORDER BY s.id LIMIT 1"
    ))?;
    let mut picked: Vec<i64> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Gaps make some ids likelier than others; that is fine for shuffling a library, and a
    // bounded number of tries keeps a tiny library from looping.
    for _ in 0..count.saturating_mul(4) {
        let start = low + i64::try_from(random_u64() % span).unwrap_or(0);
        let id: Option<i64> = match next.query_row([start], |row| row.get(0)).optional()? {
            Some(id) => Some(id),
            None => first.query_row([], |row| row.get(0)).optional()?,
        };
        let Some(id) = id else { break };
        if seen.insert(id) {
            picked.push(id);
            if picked.len() == count as usize {
                break;
            }
        }
    }
    Ok(picked)
}

/// Songs with this `tracks_model::song_key`.
pub fn by_key(c: &Connection, a: &Access, song_key: &str) -> Result<Vec<Song>, DbError> {
    let tail = format!(
        "WHERE s.song_key = ?1 AND {} ORDER BY s.missing_since IS NOT NULL, s.id",
        visible(a, "s.library_id", None)
    );
    load(c, &tail, &[&song_key])
}

/// By a score of plays on this server, the caller's own plays (counted twice), the caller's
/// star, and the song's rank on Last.fm when enrichment has one; then the caller's rating,
/// then album order.
pub fn top_for_artist(
    c: &Connection,
    a: &Access,
    artist: &ArtistId,
    count: u32,
) -> Result<Vec<Song>, DbError> {
    let count = count.clamp(1, 500);
    crate::memo::cached(
        "top_for_artist",
        || format!("{}/{artist}/{count}", crate::memo::access_key(a)),
        Vec::len,
        || compute_top_for_artist(c, a, artist, count),
    )
}

fn compute_top_for_artist(
    c: &Connection,
    a: &Access,
    artist: &ArtistId,
    count: u32,
) -> Result<Vec<Song>, DbError> {
    let tail = format!(
        "WHERE s.id IN (SELECT sa.song_id FROM song_artists sa
                        WHERE sa.artist_id = (SELECT id FROM artists WHERE pid = ?1)
                          AND sa.role IN ('main', 'featured'))
           AND s.missing_since IS NULL AND {}
         ORDER BY (SELECT coalesce(sum(ps.play_count), 0)
                          + coalesce(sum(ps.play_count) FILTER (WHERE ps.user_id = ?2), 0)
                   FROM play_stats ps WHERE ps.song_pid = s.pid)
                  + 5 * EXISTS (SELECT 1 FROM stars st WHERE st.user_id = ?2
                                AND st.item_kind = 'song' AND st.item_pid = s.pid)
                  + coalesce((SELECT 20.0 / (1 + min(t.rank)) FROM enrichment_top_songs t
                              WHERE t.artist_identity = (SELECT identity FROM artists WHERE pid = ?1)
                                AND t.song_key = s.song_key), 0) DESC,
                  (SELECT r.rating FROM ratings r WHERE r.user_id = ?2 AND r.item_kind = 'song'
                   AND r.item_pid = s.pid) DESC NULLS LAST,
                  al.year, al.sort_name, coalesce(s.disc_number, 1),
                  coalesce(s.track_number, 0), s.id
         LIMIT {count}",
        visible(a, "s.library_id", None)
    );
    load(c, &tail, &[artist, &a.user])
}

/// A song as [`top_for_artists`] ranks it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopSong {
    pub id: SongId,
    pub song_key: String,
    /// The display artist, as tagged.
    pub artist: String,
}

/// What ranks a song among an artist's songs apart from plays, stars and ratings.
#[derive(Clone, Debug)]
struct ArtistSong {
    id: i64,
    song: TopSong,
    year: Option<i64>,
    album_sort: Option<String>,
    disc: i64,
    track: i64,
}

/// An artist's visible songs, kept per library generation.
#[derive(Debug)]
struct ArtistSongs {
    identity: String,
    songs: Vec<ArtistSong>,
}

fn artist_songs(
    c: &Connection,
    a: &Access,
    artists: &[ArtistId],
) -> Result<HashMap<ArtistId, std::sync::Arc<ArtistSongs>>, DbError> {
    // What the caller may see is all that matters here, so callers who see the same
    // libraries share these.
    let access = format!("{:?}", a.libraries);
    let mut out = HashMap::new();
    let mut wanted = Vec::new();
    for artist in artists {
        match crate::memo::library_get::<ArtistSongs>(
            "lib:artist_songs",
            &format!("{access}/{artist}"),
        ) {
            Some(held) => {
                out.insert(artist.clone(), held);
            }
            None => wanted.push(artist),
        }
    }
    if wanted.is_empty() {
        return Ok(out);
    }
    let list = json_strings(wanted.iter().map(|id| id.as_str()));
    let sql = format!(
        "SELECT DISTINCT r.pid, r.identity, s.id, s.pid, s.song_key, s.artist, al.year,
                al.sort_name, coalesce(s.disc_number, 1), coalesce(s.track_number, 0)
         FROM artists r
         JOIN song_artists sa ON sa.artist_id = r.id AND sa.role IN ('main', 'featured')
         JOIN songs s ON s.id = sa.song_id
         LEFT JOIN albums al ON al.id = s.album_id
         WHERE r.pid IN (SELECT value FROM json_each(?1)) AND s.missing_since IS NULL AND {}",
        visible(a, "s.library_id", None)
    );
    let mut loaded: HashMap<ArtistId, ArtistSongs> = HashMap::new();
    let mut statement = c.prepare_cached(&sql)?;
    let mut rows = statement.query([&list])?;
    while let Some(row) = rows.next()? {
        let entry = loaded.entry(row.get(0)?).or_insert_with(|| ArtistSongs {
            identity: String::new(),
            songs: Vec::new(),
        });
        entry.identity = row.get(1)?;
        entry.songs.push(ArtistSong {
            id: row.get(2)?,
            song: TopSong {
                id: row.get(3)?,
                song_key: row.get(4)?,
                artist: row.get(5)?,
            },
            year: row.get(6)?,
            album_sort: row.get(7)?,
            disc: row.get(8)?,
            track: row.get(9)?,
        });
    }
    for artist in wanted {
        let found = std::sync::Arc::new(loaded.remove(artist).unwrap_or(ArtistSongs {
            identity: String::new(),
            songs: Vec::new(),
        }));
        crate::memo::library_put(
            "lib:artist_songs",
            format!("{access}/{artist}"),
            std::sync::Arc::clone(&found),
            found.songs.len(),
        );
        out.insert(artist.clone(), found);
    }
    Ok(out)
}

/// Plays of each song, from `play_stats`, kept between reads and brought up to date from the
/// listens added since: `repo::listens::submit` is the only writer of `play_stats` and adds
/// exactly one play per stored listen of a library song. Rows go only when a user is deleted,
/// which changes the users' fingerprint the totals are kept under, so they are built again.
#[derive(Debug, Default)]
struct Plays(HashMap<String, i64>);

fn add_new_plays(
    c: &Connection,
    plays: &mut HashMap<String, i64>,
    user: Option<UserKey>,
    after: i64,
) -> Result<(), DbError> {
    let mut statement = c.prepare_cached(
        "SELECT song_pid FROM listens
         WHERE id > ?1 AND song_pid IS NOT NULL AND (?2 IS NULL OR user_id = ?2)",
    )?;
    let mut rows = statement.query(params![after, user])?;
    while let Some(row) = rows.next()? {
        *plays.entry(row.get(0)?).or_default() += 1;
    }
    Ok(())
}

fn updated(
    c: &Connection,
    older: Option<(i64, std::sync::Arc<Plays>)>,
    user: Option<UserKey>,
    full: &str,
) -> Result<Plays, DbError> {
    if let Some((after, held)) = older {
        let mut plays =
            std::sync::Arc::try_unwrap(held).map_or_else(|shared| shared.0.clone(), |p| p.0);
        add_new_plays(c, &mut plays, user, after)?;
        return Ok(Plays(plays));
    }
    let mut statement = c.prepare_cached(full)?;
    let rows = statement.query_map(params![user], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(Plays(rows.collect::<Result<_, _>>()?))
}

/// Everyone's plays of each song.
fn play_totals(c: &Connection) -> Result<std::sync::Arc<Plays>, DbError> {
    let (users, last_user, last_listen): (i64, i64, i64) = c
        .prepare_cached(
            "SELECT (SELECT count(*) FROM users), (SELECT coalesce(max(id), 0) FROM users),
                    (SELECT coalesce(max(id), 0) FROM listens)",
        )?
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    // Who the users are: a deleted user takes their plays along, so the totals start over.
    let fingerprint = UserKey(users.wrapping_mul(1_000_003).wrapping_add(last_user));
    crate::memo::per_user("play_totals", fingerprint, last_listen, |older| {
        updated(
            c,
            older,
            None,
            "SELECT song_pid, sum(play_count) FROM play_stats
             WHERE ?1 IS NULL GROUP BY song_pid",
        )
    })
}

/// The caller's own plays of each song, kept until they listen again.
fn own_plays(c: &Connection, a: &Access) -> Result<std::sync::Arc<Plays>, DbError> {
    let last_listen: i64 = c
        .prepare_cached("SELECT coalesce(max(id), 0) FROM listens WHERE user_id = ?1")?
        .query_row([a.user], |row| row.get(0))?;
    crate::memo::per_user("own_plays", a.user, last_listen, |older| {
        updated(
            c,
            older,
            Some(a.user),
            "SELECT song_pid, play_count FROM play_stats WHERE user_id = ?1",
        )
    })
}

/// The caller's own stars and ratings of songs.
#[derive(Debug, Default)]
struct Mine {
    stars: std::collections::HashSet<String>,
    ratings: HashMap<String, i64>,
}

fn mine(c: &Connection, a: &Access) -> Result<std::sync::Arc<Mine>, DbError> {
    crate::memo::cached(
        "my_songs",
        || a.user.0.to_string(),
        |m: &std::sync::Arc<Mine>| (m.stars.len() + m.ratings.len()) / 4,
        || {
            let mut out = Mine::default();
            let mut statement = c.prepare_cached(
                "SELECT item_pid FROM stars WHERE user_id = ?1 AND item_kind = 'song'",
            )?;
            let mut rows = statement.query([a.user])?;
            while let Some(row) = rows.next()? {
                out.stars.insert(row.get(0)?);
            }
            let mut statement = c.prepare_cached(
                "SELECT item_pid, rating FROM ratings WHERE user_id = ?1 AND item_kind = 'song'",
            )?;
            let mut rows = statement.query([a.user])?;
            while let Some(row) = rows.next()? {
                out.ratings.insert(row.get(0)?, row.get(1)?);
            }
            Ok(std::sync::Arc::new(out))
        },
    )
}

/// [`top_for_artist`] for many artists at once, as brief songs: each list holds the same
/// songs in the same order that function gives. The ranking is computed here from the same
/// values its SQL reads, so the order is the same, ties included. Artists the caller cannot
/// see, or without songs, are left out.
pub fn top_for_artists(
    c: &Connection,
    a: &Access,
    artists: &[ArtistId],
    count: u32,
) -> Result<HashMap<ArtistId, Vec<TopSong>>, DbError> {
    let count = count.clamp(1, 500) as usize;
    if artists.is_empty() {
        return Ok(HashMap::new());
    }
    let catalog = artist_songs(c, a, artists)?;
    let totals = play_totals(c)?;
    let own = own_plays(c, a)?;
    let mine = mine(c, a)?;
    let identities = json_strings(catalog.values().map(|s| s.identity.as_str()));
    let mut ranks: HashMap<String, HashMap<String, i64>> = HashMap::new();
    {
        let mut statement = c.prepare_cached(
            "SELECT artist_identity, song_key, min(rank) FROM enrichment_top_songs
             WHERE artist_identity IN (SELECT value FROM json_each(?1))
             GROUP BY artist_identity, song_key",
        )?;
        let mut rows = statement.query([&identities])?;
        while let Some(row) = rows.next()? {
            ranks
                .entry(row.get(0)?)
                .or_default()
                .insert(row.get(1)?, row.get(2)?);
        }
    }
    let mut out = HashMap::new();
    for (artist, found) in catalog {
        if found.songs.is_empty() {
            continue;
        }
        let own_ranks = ranks.get(&found.identity);
        let mut scored: Vec<(f64, Option<i64>, &ArtistSong)> = found
            .songs
            .iter()
            .map(|s| {
                let pid = s.song.id.as_str();
                // As the SQL adds them: whole plays and stars, then Last.fm's rank as a real.
                let whole = totals.0.get(pid).copied().unwrap_or(0)
                    + own.0.get(pid).copied().unwrap_or(0)
                    + 5 * i64::from(mine.stars.contains(pid));
                let rank = own_ranks
                    .and_then(|r| r.get(&s.song.song_key))
                    .map_or(0.0, |r| 20.0 / (1 + r) as f64);
                (whole as f64 + rank, mine.ratings.get(pid).copied(), s)
            })
            .collect();
        scored.sort_by(|x, y| {
            y.0.total_cmp(&x.0)
                .then_with(|| match (x.1, y.1) {
                    (Some(p), Some(q)) => q.cmp(&p),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| x.2.year.cmp(&y.2.year))
                .then_with(|| x.2.album_sort.cmp(&y.2.album_sort))
                .then_with(|| x.2.disc.cmp(&y.2.disc))
                .then_with(|| x.2.track.cmp(&y.2.track))
                .then_with(|| x.2.id.cmp(&y.2.id))
        });
        out.insert(
            artist,
            scored
                .into_iter()
                .take(count)
                .map(|(_, _, s)| s.song.clone())
                .collect(),
        );
    }
    Ok(out)
}

/// Every visible song by internal id, for Subsonic's legacy empty `search3` paging. Returns the
/// page and the internal id to continue after.
pub fn all_paged(
    c: &Connection,
    a: &Access,
    after: Option<i64>,
    limit: u32,
) -> Result<(Vec<Song>, Option<i64>), DbError> {
    all_paged_in(c, a, None, after, limit)
}

/// [`all_paged`] narrowed to one library (Subsonic's `musicFolderId`).
pub fn all_paged_in(
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    after: Option<i64>,
    limit: u32,
) -> Result<(Vec<Song>, Option<i64>), DbError> {
    paged(c, a, library, "1", None, after, limit)
}

/// Present songs with a genre (matched folded), by internal id, for Subsonic's
/// `getSongsByGenre`. Returns the page and the internal id to continue after.
pub fn by_genre(
    c: &Connection,
    a: &Access,
    genre: &str,
    library: Option<LibraryKey>,
    after: Option<i64>,
    limit: u32,
) -> Result<(Vec<Song>, Option<i64>), DbError> {
    let key = tracks_model::folded(genre);
    paged(
        c,
        a,
        library,
        "s.missing_since IS NULL AND s.id IN (SELECT sg.song_id FROM song_genres sg
             JOIN genres g ON g.id = sg.genre_id WHERE g.name_key = ?2)",
        Some(&key),
        after,
        limit,
    )
}

fn paged(
    c: &Connection,
    a: &Access,
    library: Option<LibraryKey>,
    condition: &str,
    second: Option<&str>,
    after: Option<i64>,
    limit: u32,
) -> Result<(Vec<Song>, Option<i64>), DbError> {
    let limit = limit.clamp(1, 5000);
    let tail = format!(
        "WHERE s.id > ?1 AND {condition} AND {} ORDER BY s.id LIMIT {}",
        visible(a, "s.library_id", library),
        limit + 1
    );
    let after = after.unwrap_or(0);
    let mut rows = match second {
        Some(value) => load_keyed(c, &tail, &[&after, &value])?,
        None => load_keyed(c, &tail, &[&after])?,
    };
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|(id, _)| *id)
    } else {
        None
    };
    Ok((rows.into_iter().map(|(_, song)| song).collect(), next))
}

/// How many present songs the caller can see.
pub fn count(c: &Connection, a: &Access) -> Result<u64, DbError> {
    let total: i64 = c.query_row(
        &format!(
            "SELECT count(*) FROM songs s WHERE s.missing_since IS NULL AND {}",
            visible(a, "s.library_id", None)
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(to_u64(total))
}

/// The sum of visible present songs' file sizes.
pub fn total_bytes(c: &Connection, a: &Access) -> Result<u64, DbError> {
    let total: i64 = c.query_row(
        &format!(
            "SELECT coalesce(sum(s.file_size), 0) FROM songs s WHERE s.missing_since IS NULL AND {}",
            visible(a, "s.library_id", None)
        ),
        [],
        |row| row.get(0),
    )?;
    Ok(to_u64(total))
}

/// Where each present song lives and its artwork, ignoring who asks: for state shared
/// between callers (what a speaker plays), checked per caller later.
pub fn placement(
    c: &Connection,
    ids: &[SongId],
) -> Result<HashMap<SongId, (LibraryKey, Option<crate::ids::ArtworkId>)>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let list = json_strings(ids.iter().map(PublicId::as_str));
    let mut statement = c.prepare_cached(
        "SELECT pid, library_id, artwork_id FROM songs
         WHERE pid IN (SELECT value FROM json_each(?1)) AND missing_since IS NULL",
    )?;
    let rows = statement.query_map([&list], |row| {
        Ok((row.get::<_, SongId>(0)?, (row.get(1)?, row.get(2)?)))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}
