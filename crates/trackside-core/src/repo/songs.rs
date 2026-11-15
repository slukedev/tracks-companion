//! Songs.

use std::collections::HashMap;

use rusqlite::types::ToSql;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::{join_rel, json_ints, json_strings, opt_u8, opt_u32, to_u32, to_u64, visible};
use crate::access::Access;
use crate::db::DbError;
use crate::ids::{AlbumId, ArtistId, FolderId, LibraryKey, PublicId, SongId};
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
