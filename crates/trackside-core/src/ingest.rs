//! Ingest: the id rules of architecture section 6, and the only code that creates or moves
//! songs. The scanner reads files and hands [`ScannedFile`]s here inside a write transaction.

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

pub use crate::control::SongOrigin;
use crate::db::{Change, ChangeKind, ChangeOp, DbError, WriteTx};
use crate::ids::{
    AlbumId, ArtistId, ArtworkId, AudioDigest, FolderId, LibraryId, LibraryKey, PublicId,
    ScanRunId, SongId,
};
use crate::records::{
    Codec, Container, CreditRole, Explicit, LyricsKind, LyricsSource, ReplayGain,
};
use crate::repo::sql::{bytes_path, from_u64, join_rel, rel_name, rel_parent, to_u64};
use crate::time::Millis;

/// Everything the scanner learned about one file.
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedFile {
    /// Exact OS bytes, relative to the library root.
    pub rel_path: Vec<u8>,
    pub rel_path_display: String,
    pub folder: FolderId,
    pub size: u64,
    pub mtime_ns: i64,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub digest: AudioDigest,
    pub tags: ScannedTags,
    pub properties: ScannedProperties,
    pub artwork: Option<ArtworkId>,
    pub lyrics: Vec<ScannedLyrics>,
    /// Why tags fell back to the file name, or `digest_whole_file`.
    pub tag_problem: Option<String>,
}

/// Tags as read, NFC-normalised. `title` and `artist` are never empty (the file name stands
/// in).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ScannedTags {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub disc_subtitle: Option<String>,
    pub year: Option<i32>,
    pub release_date: Option<String>,
    pub original_date: Option<String>,
    pub genres: Vec<String>,
    pub compilation: bool,
    pub release_type: Option<String>,
    pub mbz_recording: Option<String>,
    pub mbz_track: Option<String>,
    pub mbz_release: Option<String>,
    pub mbz_release_group: Option<String>,
    pub mbz_artist: Option<String>,
    pub mbz_album_artist: Option<String>,
    pub isrc: Option<String>,
    pub bpm: Option<u32>,
    pub comment: Option<String>,
    pub explicit: Option<Explicit>,
    pub replay_gain: Option<ReplayGain>,
    pub sort_title: Option<String>,
    pub sort_artist: Option<String>,
    pub sort_album: Option<String>,
}

/// Audio properties as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedProperties {
    pub duration_ms: u32,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub codec: Codec,
    pub container: Container,
}

/// Lyrics found in tags or a sidecar file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedLyrics {
    pub kind: LyricsKind,
    pub lang: String,
    pub source: LyricsSource,
    pub content: String,
}

/// How a scanned file was matched to a song.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Same path, nothing to write.
    Unchanged(SongId),
    /// Same path, tags or file changed.
    Updated(SongId),
    /// A missing song with the same digest found at a new path.
    Moved(SongId),
    /// A re-encode or replacement of a missing song at a new path; `content_version` bumped.
    Replaced(SongId),
    Added(SongId),
}

impl Resolution {
    /// The song's final id.
    pub fn song(&self) -> &SongId {
        match self {
            Self::Unchanged(id)
            | Self::Updated(id)
            | Self::Moved(id)
            | Self::Replaced(id)
            | Self::Added(id) => id,
        }
    }
}

/// What the index already has at a path, for a quick scan's size and mtime check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownFile {
    pub song: SongId,
    pub size: u64,
    pub mtime_ns: i64,
    pub content_version: i64,
    pub digest: AudioDigest,
    pub missing_since: Option<Millis>,
}

/// Relative paths a scan run found, for the end-of-run missing pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeenSet {
    paths: HashSet<Vec<u8>>,
}

impl SeenSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, rel_path: Vec<u8>) {
        self.paths.insert(rel_path);
    }

    pub fn contains(&self, rel_path: &[u8]) -> bool {
        self.paths.contains(rel_path)
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// What the end-of-run pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinishOutcome {
    pub marked_missing: u64,
    pub albums_removed: u64,
    pub artists_removed: u64,
}

/// Matches each file to a song by the rules of architecture section 6 and writes songs, albums,
/// artists, credits, genres, lyrics and change log rows.
pub fn apply_batch(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    run: &ScanRunId,
    origin: SongOrigin,
    files: &[ScannedFile],
) -> Result<Vec<Resolution>, DbError> {
    let _ = run;
    let mut batch = Batch::new(tx, library, origin)?;
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        out.push(batch.apply(tx, file)?);
    }
    batch.finish(tx)?;
    Ok(out)
}

/// What the index has at `rel_path` in `library`.
pub fn known_file(
    c: &Connection,
    library: LibraryKey,
    rel_path: &[u8],
) -> Result<Option<KnownFile>, DbError> {
    Ok(c.prepare_cached(
        "SELECT pid, file_size, file_mtime_ns, content_version, audio_digest, missing_since
             FROM songs WHERE library_id = ?1 AND rel_path = ?2",
    )?
    .query_row(params![library, rel_path], |row| {
        let digest: Vec<u8> = row.get(4)?;
        Ok(KnownFile {
            song: row.get(0)?,
            size: to_u64(row.get(1)?),
            mtime_ns: row.get(2)?,
            content_version: row.get(3)?,
            digest: AudioDigest(<[u8; 32]>::try_from(digest.as_slice()).unwrap_or([0; 32])),
            missing_since: row.get(5)?,
        })
    })
    .optional()?)
}

fn under(prefix: &[u8], rel_path: &[u8]) -> bool {
    prefix.is_empty()
        || (rel_path.len() > prefix.len()
            && rel_path.starts_with(prefix)
            && rel_path[prefix.len()] == b'/')
}

/// Marks songs not seen and not under an unreadable folder as missing. Never called by an
/// aborted run.
pub fn finish_run(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    run: &ScanRunId,
    seen: &SeenSet,
    unreadable: &[Vec<u8>],
) -> Result<FinishOutcome, DbError> {
    let _ = run;
    let mut skipped: Vec<Vec<u8>> = unreadable.to_vec();
    skipped.extend(crate::repo::folders::unreadable(tx, library)?);
    let gone: Vec<(i64, SongId)> = {
        let mut statement = tx.prepare_cached(
            "SELECT id, pid, rel_path FROM songs WHERE library_id = ?1 AND missing_since IS NULL",
        )?;
        let mut rows = statement.query([library])?;
        let mut gone = Vec::new();
        while let Some(row) = rows.next()? {
            let rel: Vec<u8> = row.get(2)?;
            if seen.contains(&rel) || skipped.iter().any(|prefix| under(prefix, &rel)) {
                continue;
            }
            gone.push((row.get(0)?, row.get(1)?));
        }
        gone
    };
    let now = tx.now();
    for (id, pid) in &gone {
        tx.prepare_cached("UPDATE songs SET missing_since = ?2, updated_at = ?2 WHERE id = ?1")?
            .execute(params![id, now])?;
        tx.record(Change::shared(
            ChangeKind::Song,
            pid.as_str(),
            Some(library),
            ChangeOp::Upsert,
        ))?;
    }
    let orphans: Vec<i64> = tx
        .prepare_cached(
            "SELECT a.id FROM albums a WHERE a.library_id = ?1
             AND NOT EXISTS (SELECT 1 FROM songs s WHERE s.album_id = a.id)",
        )?
        .query_map([library], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let albums_removed = refresh_albums(tx, orphans, &HashSet::new())?;
    let artists_removed = refresh_artists(tx, Vec::new())?;
    tx.prepare_cached("UPDATE libraries SET last_scan_at = ?2 WHERE id = ?1")?
        .execute(params![library, now])?;
    tx.prepare_cached(
        "UPDATE libraries SET song_count = (SELECT count(*) FROM songs
             WHERE library_id = ?1 AND missing_since IS NULL) WHERE id = ?1",
    )?
    .execute([library])?;
    record_library(tx, library)?;
    Ok(FinishOutcome {
        marked_missing: gone.len() as u64,
        albums_removed,
        artists_removed,
    })
}

/// Marks songs at these paths missing now, the moment the scanner learns their files are gone
/// (a watcher delete event, a folder that lost entries). A file showing up later with the same
/// audio is then a move and keeps its id, however long the reorganisation takes. Paths without
/// a present song are ignored. Returns how many were marked.
pub fn mark_gone(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    rel_paths: &[Vec<u8>],
) -> Result<u64, DbError> {
    let now = tx.now();
    let mut marked = 0;
    for rel in rel_paths {
        let pid: Option<SongId> = tx
            .prepare_cached(
                "UPDATE songs SET missing_since = ?3, updated_at = ?3
                 WHERE library_id = ?1 AND rel_path = ?2 AND missing_since IS NULL RETURNING pid",
            )?
            .query_row(params![library, rel, now], |row| row.get(0))
            .optional()?;
        if let Some(pid) = pid {
            tx.record(Change::shared(
                ChangeKind::Song,
                pid.as_str(),
                Some(library),
                ChangeOp::Upsert,
            ))?;
            marked += 1;
        }
    }
    if marked > 0 {
        recount_library(tx, library)?;
    }
    Ok(marked)
}

/// [`finish_run`] for a scan of one folder: marks songs under `folder` (directly in it, or at
/// any depth with `recursive`) that the run did not see. Unreadable folders are spared as in a
/// full run. Never called by an aborted run.
pub fn finish_folder(
    tx: &mut WriteTx<'_>,
    library: LibraryKey,
    run: &ScanRunId,
    folder: &[u8],
    recursive: bool,
    seen: &SeenSet,
) -> Result<FinishOutcome, DbError> {
    let _ = run;
    let skipped = crate::repo::folders::unreadable(tx, library)?;
    let candidates: Vec<Vec<u8>> = {
        let mut statement = tx.prepare_cached(
            "SELECT rel_path FROM songs WHERE library_id = ?1 AND missing_since IS NULL",
        )?;
        let rows = statement.query_map([library], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|rel| {
                let inside = if recursive {
                    under(folder, rel)
                } else {
                    rel_parent(rel) == folder
                };
                inside && !seen.contains(rel) && !skipped.iter().any(|prefix| under(prefix, rel))
            })
            .collect()
    };
    let marked_missing = mark_gone(tx, library, &candidates)?;
    Ok(FinishOutcome {
        marked_missing,
        albums_removed: 0,
        artists_removed: 0,
    })
}

/// Deletes songs missing since before `older_than` and writes their tombstones. Per-user rows
/// that name them by public id stay.
pub fn purge_missing(tx: &mut WriteTx<'_>, older_than: Millis) -> Result<u64, DbError> {
    let doomed: Vec<(i64, SongId, LibraryKey, Option<i64>)> = tx
        .prepare_cached(
            "SELECT id, pid, library_id, album_id FROM songs
             WHERE missing_since IS NOT NULL AND missing_since < ?1",
        )?
        .query_map([older_than], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    if doomed.is_empty() {
        return Ok(0);
    }
    let mut albums = Vec::new();
    let mut artists = Vec::new();
    let mut libraries = HashSet::new();
    for (id, pid, library, album) in &doomed {
        artists.extend(credited_artists(tx, *id)?);
        albums.extend(*album);
        libraries.insert(*library);
        tx.prepare_cached("DELETE FROM songs WHERE id = ?1")?
            .execute([id])?;
        tx.record(Change::shared(
            ChangeKind::Song,
            pid.as_str(),
            Some(*library),
            ChangeOp::Delete,
        ))?;
    }
    refresh_albums(tx, albums, &HashSet::new())?;
    refresh_artists(tx, artists)?;
    for library in libraries {
        recount_library(tx, library)?;
    }
    Ok(doomed.len() as u64)
}

/// Resets a library's song count to the present songs it has, recording the library when the
/// number moved.
fn recount_library(tx: &mut WriteTx<'_>, library: LibraryKey) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE libraries SET song_count = (SELECT count(*) FROM songs
                 WHERE library_id = ?1 AND missing_since IS NULL)
             WHERE id = ?1 AND song_count != (SELECT count(*) FROM songs
                 WHERE library_id = ?1 AND missing_since IS NULL)",
        )?
        .execute([library])?;
    if changed > 0 {
        record_library(tx, library)?;
    }
    Ok(())
}

fn record_library(tx: &mut WriteTx<'_>, library: LibraryKey) -> Result<(), DbError> {
    let pid: Option<String> = tx
        .prepare_cached("SELECT pid FROM libraries WHERE id = ?1")?
        .query_row([library], |row| row.get(0))
        .optional()?;
    if let Some(pid) = pid {
        tx.record(Change::shared(
            ChangeKind::Library,
            pid,
            Some(library),
            ChangeOp::Upsert,
        ))?;
    }
    Ok(())
}

fn credited_artists(c: &Connection, song: i64) -> Result<Vec<i64>, DbError> {
    let mut statement =
        c.prepare_cached("SELECT artist_id FROM song_artists WHERE song_id = ?1")?;
    let rows = statement.query_map([song], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Recomputes the counts of these albums from their songs. Albums left without songs are
/// deleted with a tombstone; albums whose counts moved, or that are in `dirty`, get an upsert.
/// Returns how many were deleted.
pub(crate) fn refresh_albums(
    tx: &mut WriteTx<'_>,
    albums: impl IntoIterator<Item = i64>,
    dirty: &HashSet<i64>,
) -> Result<u64, DbError> {
    let mut ids: Vec<i64> = albums.into_iter().collect();
    ids.extend(dirty.iter().copied());
    ids.sort_unstable();
    ids.dedup();
    let mut removed = 0;
    for id in ids {
        let row: Option<(AlbumId, LibraryKey)> = tx
            .prepare_cached("SELECT pid, library_id FROM albums WHERE id = ?1")?
            .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((pid, library)) = row else { continue };
        let (count, discs, duration, size, added): (i64, i64, i64, i64, Option<i64>) = tx
            .prepare_cached(
                "SELECT count(*), coalesce(max(coalesce(disc_number, 1)), 1),
                        coalesce(sum(duration_ms), 0), coalesce(sum(file_size), 0), min(added_at)
                 FROM songs WHERE album_id = ?1",
            )?
            .query_row([id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?;
        if count == 0 {
            tx.prepare_cached("DELETE FROM albums WHERE id = ?1")?
                .execute([id])?;
            tx.record(Change::shared(
                ChangeKind::Album,
                pid.as_str(),
                Some(library),
                ChangeOp::Delete,
            ))?;
            removed += 1;
            continue;
        }
        let changed = tx
            .prepare_cached(
                "UPDATE albums SET song_count = ?2, disc_count = ?3, duration_ms = ?4,
                     size_bytes = ?5, added_at = coalesce(?6, added_at), updated_at = ?7
                 WHERE id = ?1 AND NOT (song_count = ?2 AND disc_count = ?3 AND duration_ms = ?4
                     AND size_bytes = ?5 AND added_at IS coalesce(?6, added_at))",
            )?
            .execute(params![id, count, discs, duration, size, added, tx.now()])?;
        if changed > 0 || dirty.contains(&id) {
            tx.record(Change::shared(
                ChangeKind::Album,
                pid.as_str(),
                Some(library),
                ChangeOp::Upsert,
            ))?;
        }
    }
    Ok(removed)
}

/// Recomputes `artist_libraries` for these artists, and for any artist left without rows.
/// Artists with no songs and no albums are deleted with a tombstone; artists whose counts moved
/// get an upsert. Returns how many were deleted.
pub(crate) fn refresh_artists(
    tx: &mut WriteTx<'_>,
    artists: impl IntoIterator<Item = i64>,
) -> Result<u64, DbError> {
    refresh_artists_marking(tx, artists, &HashSet::new(), true)
}

/// [`refresh_artists`], also recording `dirty` artists. `sweep` looks for artists left without
/// any songs or albums anywhere, which a batch never needs: it touches every artist it moves
/// songs away from.
fn refresh_artists_marking(
    tx: &mut WriteTx<'_>,
    artists: impl IntoIterator<Item = i64>,
    dirty: &HashSet<i64>,
    sweep: bool,
) -> Result<u64, DbError> {
    let mut ids: Vec<i64> = artists.into_iter().collect();
    ids.extend(dirty.iter().copied());
    let stranded: Vec<i64> = if sweep {
        tx.prepare_cached(
            "SELECT r.id FROM artists r
             WHERE NOT EXISTS (SELECT 1 FROM artist_libraries x WHERE x.artist_id = r.id)
               AND NOT EXISTS (SELECT 1 FROM song_artists sa WHERE sa.artist_id = r.id)",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?
    } else {
        Vec::new()
    };
    ids.extend(stranded);
    ids.sort_unstable();
    ids.dedup();
    let mut removed = 0;
    for id in ids {
        let pid: Option<ArtistId> = tx
            .prepare_cached("SELECT pid FROM artists WHERE id = ?1")?
            .query_row([id], |row| row.get(0))
            .optional()?;
        let Some(pid) = pid else { continue };
        let before: Vec<(i64, i64, i64)> = tx
            .prepare_cached(
                "SELECT library_id, album_count, song_count FROM artist_libraries
                 WHERE artist_id = ?1 ORDER BY library_id",
            )?
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut after: std::collections::BTreeMap<i64, (i64, i64)> =
            std::collections::BTreeMap::new();
        {
            let mut songs = tx.prepare_cached(
                "SELECT s.library_id, count(DISTINCT sa.song_id) FROM song_artists sa
                 JOIN songs s ON s.id = sa.song_id WHERE sa.artist_id = ?1 GROUP BY s.library_id",
            )?;
            let mut rows = songs.query([id])?;
            while let Some(row) = rows.next()? {
                after.entry(row.get(0)?).or_default().1 = row.get(1)?;
            }
            let mut albums = tx.prepare_cached(
                "SELECT library_id, count(*) FROM albums WHERE artist_id = ?1 GROUP BY library_id",
            )?;
            let mut rows = albums.query([id])?;
            while let Some(row) = rows.next()? {
                after.entry(row.get(0)?).or_default().0 = row.get(1)?;
            }
        }
        let after_rows: Vec<(i64, i64, i64)> = after
            .iter()
            .map(|(library, (albums, songs))| (*library, *albums, *songs))
            .collect();
        if after_rows.is_empty() {
            tx.prepare_cached("DELETE FROM artists WHERE id = ?1")?
                .execute([id])?;
            tx.record(Change::shared(
                ChangeKind::Artist,
                pid.as_str(),
                None,
                ChangeOp::Delete,
            ))?;
            removed += 1;
            continue;
        }
        if after_rows != before {
            tx.prepare_cached("DELETE FROM artist_libraries WHERE artist_id = ?1")?
                .execute([id])?;
            for (library, albums, songs) in &after_rows {
                tx.prepare_cached(
                    "INSERT INTO artist_libraries (artist_id, library_id, album_count, song_count)
                     VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![id, library, albums, songs])?;
            }
        }
        if after_rows != before || dirty.contains(&id) {
            tx.record(Change::shared(
                ChangeKind::Artist,
                pid.as_str(),
                None,
                ChangeOp::Upsert,
            ))?;
        }
    }
    Ok(removed)
}

/// One credited artist resolved to its row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CreditRow {
    artist: i64,
    role: CreditRole,
    position: i64,
}

/// A song as stored before this batch touched it.
struct Existing {
    id: i64,
    pid: SongId,
    library: LibraryKey,
    missing: bool,
    album: Option<i64>,
}

/// The state one `apply_batch` call keeps across its files.
struct Batch {
    library: LibraryKey,
    library_pid: LibraryId,
    origin: SongOrigin,
    artists: HashMap<String, i64>,
    albums: HashMap<String, i64>,
    genres: HashMap<String, i64>,
    claimed: HashSet<i64>,
    touched_albums: HashSet<i64>,
    dirty_albums: HashSet<i64>,
    touched_artists: HashSet<i64>,
    dirty_artists: HashSet<i64>,
    present: HashMap<LibraryKey, i64>,
}

/// The columns every song write sets from the file, in this order.
const FILE_COLUMNS: [&str; 47] = [
    "library_id",
    "folder_id",
    "rel_path",
    "rel_path_display",
    "file_size",
    "file_mtime_ns",
    "file_dev",
    "file_ino",
    "audio_digest",
    "title",
    "sort_title",
    "artist",
    "artist_id",
    "album_id",
    "album",
    "album_artist",
    "track_number",
    "disc_number",
    "disc_subtitle",
    "year",
    "release_date",
    "genre",
    "duration_ms",
    "bitrate_kbps",
    "sample_rate",
    "bit_depth",
    "channels",
    "codec",
    "container",
    "content_type",
    "lossless",
    "song_key",
    "mbz_recording_id",
    "mbz_track_id",
    "mbz_release_id",
    "isrc",
    "bpm",
    "comment",
    "explicit",
    "rg_track_gain",
    "rg_track_peak",
    "rg_album_gain",
    "rg_album_peak",
    "has_lyrics",
    "artwork_id",
    "tag_problem",
    "search_text",
];

/// The index of `audio_digest` in [`FILE_COLUMNS`].
const DIGEST_COLUMN: usize = 8;

fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |v| Value::Text(v.to_owned()))
}

fn int<T: Into<i64>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, |v| Value::Integer(v.into()))
}

fn real(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Real)
}

fn trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

fn file_stem(rel_path: &[u8]) -> &[u8] {
    let name = rel_name(rel_path);
    match name.iter().rposition(|&b| b == b'.') {
        Some(0) | None => name,
        Some(i) => &name[..i],
    }
}

/// Whether a stored song's file is gone from `root/rel_path`, or is the scanned file itself
/// reached by another name (a case-only rename on a case-insensitive filesystem).
fn path_gone(root: &[u8], rel_path: &[u8], file: &ScannedFile) -> bool {
    let path = join_rel(&bytes_path(root), rel_path);
    match std::fs::symlink_metadata(&path) {
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        Ok(meta) => same_file(&meta, file),
    }
}

#[cfg(unix)]
fn same_file(meta: &std::fs::Metadata, file: &ScannedFile) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((file.dev, file.ino), (Some(dev), Some(ino)) if dev == meta.dev() && ino == meta.ino())
}

#[cfg(not(unix))]
fn same_file(_meta: &std::fs::Metadata, _file: &ScannedFile) -> bool {
    false
}

/// A song another file might be a move or replacement of.
struct Candidate {
    id: i64,
    rel_path: Vec<u8>,
    missing: bool,
    root: Vec<u8>,
}

impl Batch {
    fn new(tx: &WriteTx<'_>, library: LibraryKey, origin: SongOrigin) -> Result<Self, DbError> {
        let library_pid: LibraryId = tx
            .prepare_cached("SELECT pid FROM libraries WHERE id = ?1")?
            .query_row([library], |row| row.get(0))
            .optional()?
            .ok_or(DbError::NotFound)?;
        Ok(Self {
            library,
            library_pid,
            origin,
            artists: HashMap::new(),
            albums: HashMap::new(),
            genres: HashMap::new(),
            claimed: HashSet::new(),
            touched_albums: HashSet::new(),
            dirty_albums: HashSet::new(),
            touched_artists: HashSet::new(),
            dirty_artists: HashSet::new(),
            present: HashMap::new(),
        })
    }

    fn apply(&mut self, tx: &mut WriteTx<'_>, file: &ScannedFile) -> Result<Resolution, DbError> {
        let existing: Option<Existing> = tx
            .prepare_cached(
                "SELECT id, pid, library_id, missing_since, album_id FROM songs
                 WHERE library_id = ?1 AND rel_path = ?2",
            )?
            .query_row(params![self.library, file.rel_path], |row| {
                Ok(Existing {
                    id: row.get(0)?,
                    pid: row.get(1)?,
                    library: row.get(2)?,
                    missing: row.get::<_, Option<i64>>(3)?.is_some(),
                    album: row.get(4)?,
                })
            })
            .optional()?;
        if let Some(existing) = existing {
            self.claimed.insert(existing.id);
            return self.write_existing(tx, file, &existing, Kind::SamePath);
        }
        if let Some(id) = self.find_move(tx, file)? {
            let existing = Self::load_existing(tx, id)?;
            self.claimed.insert(id);
            return self.write_existing(tx, file, &existing, Kind::Moved);
        }
        if let Some(id) = self.find_replacement(tx, file)? {
            let existing = Self::load_existing(tx, id)?;
            self.claimed.insert(id);
            return self.write_existing(tx, file, &existing, Kind::Replaced);
        }
        self.insert_new(tx, file)
    }

    fn load_existing(tx: &WriteTx<'_>, id: i64) -> Result<Existing, DbError> {
        Ok(tx
            .prepare_cached(
                "SELECT id, pid, library_id, missing_since, album_id FROM songs WHERE id = ?1",
            )?
            .query_row([id], |row| {
                Ok(Existing {
                    id: row.get(0)?,
                    pid: row.get(1)?,
                    library: row.get(2)?,
                    missing: row.get::<_, Option<i64>>(3)?.is_some(),
                    album: row.get(4)?,
                })
            })?)
    }

    fn eligible(&self, candidate: &Candidate, file: &ScannedFile) -> bool {
        !self.claimed.contains(&candidate.id)
            && (candidate.missing || path_gone(&candidate.root, &candidate.rel_path, file))
    }

    /// Rule 2: a missing song with the same digest, or a present one whose file is gone.
    /// Prefers an unchanged file name, then the lowest internal id.
    fn find_move(&self, tx: &WriteTx<'_>, file: &ScannedFile) -> Result<Option<i64>, DbError> {
        let candidates: Vec<Candidate> = tx
            .prepare_cached(
                "SELECT s.id, s.rel_path, s.missing_since, l.root FROM songs s
                 JOIN libraries l ON l.id = +s.library_id
                 WHERE s.audio_digest = ?1 ORDER BY s.id",
            )?
            .query_map([file.digest.as_bytes().as_slice()], |row| {
                Ok(Candidate {
                    id: row.get(0)?,
                    rel_path: row.get(1)?,
                    missing: row.get::<_, Option<i64>>(2)?.is_some(),
                    root: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        let name = rel_name(&file.rel_path);
        let eligible: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| self.eligible(c, file))
            .collect();
        Ok(eligible
            .iter()
            .find(|c| rel_name(&c.rel_path) == name)
            .or_else(|| eligible.first())
            .map(|c| c.id))
    }

    /// Rule 3: a re-encode or replacement of a missing (or gone) song, by `MusicBrainz` track
    /// id, or by song key, album key, disc, track, file name and duration within a second.
    fn find_replacement(
        &self,
        tx: &WriteTx<'_>,
        file: &ScannedFile,
    ) -> Result<Option<i64>, DbError> {
        if let Some(mbz) = trimmed(file.tags.mbz_track.as_deref()) {
            let candidates: Vec<Candidate> = tx
                .prepare_cached(
                    "SELECT s.id, s.rel_path, s.missing_since, l.root FROM songs s
                     JOIN libraries l ON l.id = +s.library_id
                     WHERE s.mbz_track_id = ?1 ORDER BY s.id",
                )?
                .query_map([mbz], |row| {
                    Ok(Candidate {
                        id: row.get(0)?,
                        rel_path: row.get(1)?,
                        missing: row.get::<_, Option<i64>>(2)?.is_some(),
                        root: row.get(3)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            if let Some(found) = candidates.iter().find(|c| self.eligible(c, file)) {
                return Ok(Some(found.id));
            }
        }
        let names = Names::of(&file.tags);
        let song_key = tracks_model::song_key(&file.tags.title, &file.tags.artist);
        let album_key = trimmed(file.tags.album.as_deref())
            .and_then(|album| tracks_model::album_key(album, &names.album_lead_identity));
        let mut statement = tx.prepare_cached(
            "SELECT s.id, s.rel_path, s.missing_since, l.root, al.album_key, s.disc_number,
                    s.track_number, s.duration_ms
             FROM songs s JOIN libraries l ON l.id = +s.library_id
             LEFT JOIN albums al ON al.id = s.album_id
             WHERE s.song_key = ?1 ORDER BY s.id",
        )?;
        let mut rows = statement.query([&song_key])?;
        let stem = file_stem(&file.rel_path);
        while let Some(row) = rows.next()? {
            let candidate = Candidate {
                id: row.get(0)?,
                rel_path: row.get(1)?,
                missing: row.get::<_, Option<i64>>(2)?.is_some(),
                root: row.get(3)?,
            };
            let same = row.get::<_, Option<String>>(4)? == album_key
                && row.get::<_, Option<i64>>(5)? == file.tags.disc.map(i64::from)
                && row.get::<_, Option<i64>>(6)? == file.tags.track.map(i64::from)
                && file_stem(&candidate.rel_path) == stem
                && (row.get::<_, i64>(7)? - i64::from(file.properties.duration_ms)).abs() <= 1000;
            if same && self.eligible(&candidate, file) {
                return Ok(Some(candidate.id));
            }
        }
        Ok(None)
    }

    fn insert_new(
        &mut self,
        tx: &mut WriteTx<'_>,
        file: &ScannedFile,
    ) -> Result<Resolution, DbError> {
        let taken = |tx: &WriteTx<'_>, id: &SongId| -> Result<bool, DbError> {
            Ok(tx
                .prepare_cached("SELECT EXISTS (SELECT 1 FROM songs WHERE pid = ?1)")?
                .query_row([id], |row| row.get(0))?)
        };
        let mut pid = SongId::derive(&file.digest);
        if taken(tx, &pid)? {
            pid = SongId::derive_disambiguated(&file.digest, &self.library_pid, &file.rel_path);
            if taken(tx, &pid)? {
                return Err(DbError::Conflict(format!(
                    "song id {pid} for {} is already taken",
                    file.rel_path_display
                )));
            }
        }
        let resolved = self.resolve(tx, file)?;
        let mut columns: Vec<&str> = FILE_COLUMNS.to_vec();
        columns.extend(["pid", "origin", "added_at", "updated_at"]);
        let mut values = resolved.values.clone();
        let now = tx.now();
        values.extend([
            Value::Text(pid.as_str().to_owned()),
            Value::Text(self.origin.as_str().to_owned()),
            Value::Integer(now.0),
            Value::Integer(now.0),
        ]);
        let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
        tx.prepare_cached(&format!(
            "INSERT INTO songs ({}) VALUES ({})",
            columns.join(", "),
            placeholders.join(", ")
        ))?
        .execute(params_from_iter(values))?;
        let id = tx.last_insert_rowid();
        self.claimed.insert(id);
        Self::write_details(tx, id, &resolved, file)?;
        self.touch(&resolved);
        *self.present.entry(self.library).or_default() += 1;
        tx.record(Change::shared(
            ChangeKind::Song,
            pid.as_str(),
            Some(self.library),
            ChangeOp::Upsert,
        ))?;
        Ok(Resolution::Added(pid))
    }

    fn write_existing(
        &mut self,
        tx: &mut WriteTx<'_>,
        file: &ScannedFile,
        existing: &Existing,
        kind: Kind,
    ) -> Result<Resolution, DbError> {
        let old_artists = credited_artists(tx, existing.id)?;
        let resolved = self.resolve(tx, file)?;
        let mut sets: Vec<String> = FILE_COLUMNS
            .iter()
            .enumerate()
            .map(|(i, column)| format!("{column} = ?{}", i + 2))
            .collect();
        let digest = DIGEST_COLUMN + 2;
        let bump = match kind {
            Kind::SamePath => format!(
                "CASE WHEN audio_digest IS ?{digest} THEN content_version ELSE content_version + 1 END"
            ),
            Kind::Moved => String::from("content_version"),
            Kind::Replaced => String::from("content_version + 1"),
        };
        let now_index = FILE_COLUMNS.len() + 2;
        sets.push(format!("content_version = {bump}"));
        sets.push(String::from("missing_since = NULL"));
        sets.push(format!("updated_at = ?{now_index}"));
        let unchanged: Vec<String> = FILE_COLUMNS
            .iter()
            .enumerate()
            .map(|(i, column)| format!("{column} IS ?{}", i + 2))
            .chain(std::iter::once(String::from("missing_since IS NULL")))
            .collect();
        let mut values = vec![Value::Integer(existing.id)];
        values.extend(resolved.values.iter().cloned());
        values.push(Value::Integer(tx.now().0));
        let changed_row = tx
            .prepare_cached(&format!(
                "UPDATE songs SET {} WHERE id = ?1 AND NOT ({})",
                sets.join(", "),
                unchanged.join(" AND ")
            ))?
            .execute(params_from_iter(values))?
            > 0;
        let changed_details = Self::write_details(tx, existing.id, &resolved, file)?;
        let changed = changed_row || changed_details;
        if !existing.missing {
            *self.present.entry(existing.library).or_default() -= 1;
        }
        *self.present.entry(self.library).or_default() += 1;
        if changed {
            self.touched_albums.extend(existing.album);
            self.touched_artists.extend(old_artists);
            self.touch(&resolved);
            tx.record(Change::shared(
                ChangeKind::Song,
                existing.pid.as_str(),
                Some(self.library),
                ChangeOp::Upsert,
            ))?;
        }
        let pid = existing.pid.clone();
        Ok(match kind {
            Kind::SamePath if changed => Resolution::Updated(pid),
            Kind::SamePath => Resolution::Unchanged(pid),
            Kind::Moved => Resolution::Moved(pid),
            Kind::Replaced => Resolution::Replaced(pid),
        })
    }

    fn touch(&mut self, resolved: &Resolved) {
        self.touched_albums.extend(resolved.album);
        self.touched_artists
            .extend(resolved.credits.iter().map(|credit| credit.artist));
        self.touched_artists.extend(resolved.album_artist);
    }

    /// Replaces credits, genres and lyrics when they differ. Returns whether any did.
    fn write_details(
        tx: &WriteTx<'_>,
        song: i64,
        resolved: &Resolved,
        file: &ScannedFile,
    ) -> Result<bool, DbError> {
        let mut changed = false;
        let credits: Vec<CreditRow> = tx
            .prepare_cached(
                "SELECT artist_id, role, position FROM song_artists WHERE song_id = ?1
                 ORDER BY role, position",
            )?
            .query_map([song], |row| {
                let role: String = row.get(1)?;
                Ok(CreditRow {
                    artist: row.get(0)?,
                    role: CreditRole::parse(&role).unwrap_or(CreditRole::Main),
                    position: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut wanted = resolved.credits.clone();
        wanted.sort_by(|a, b| (a.role.as_str(), a.position).cmp(&(b.role.as_str(), b.position)));
        if credits != wanted {
            changed = true;
            tx.prepare_cached("DELETE FROM song_artists WHERE song_id = ?1")?
                .execute([song])?;
            for credit in &wanted {
                tx.prepare_cached(
                    "INSERT INTO song_artists (song_id, artist_id, role, position)
                     VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![
                    song,
                    credit.artist,
                    credit.role.as_str(),
                    credit.position
                ])?;
            }
        }
        let genres: Vec<i64> = tx
            .prepare_cached(
                "SELECT genre_id FROM song_genres WHERE song_id = ?1 ORDER BY position",
            )?
            .query_map([song], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        if genres != resolved.genres {
            changed = true;
            tx.prepare_cached("DELETE FROM song_genres WHERE song_id = ?1")?
                .execute([song])?;
            for (position, genre) in resolved.genres.iter().enumerate() {
                tx.prepare_cached(
                    "INSERT INTO song_genres (song_id, genre_id, position) VALUES (?1, ?2, ?3)",
                )?
                .execute(params![song, genre, position as i64])?;
            }
        }
        let stored: Vec<(String, String, String, String)> = tx
            .prepare_cached(
                "SELECT kind, lang, source, content FROM lyrics WHERE song_id = ?1
                 ORDER BY kind, lang, source",
            )?
            .query_map([song], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut lyrics: BTreeMap<(String, String, String), String> = BTreeMap::new();
        for found in &file.lyrics {
            let lang = if found.lang.trim().is_empty() {
                String::from("und")
            } else {
                found.lang.trim().to_owned()
            };
            lyrics
                .entry((
                    found.kind.as_str().to_owned(),
                    lang,
                    found.source.as_str().to_owned(),
                ))
                .or_insert_with(|| found.content.clone());
        }
        let wanted: Vec<(String, String, String, String)> = lyrics
            .into_iter()
            .map(|((kind, lang, source), content)| (kind, lang, source, content))
            .collect();
        if stored != wanted {
            changed = true;
            tx.prepare_cached("DELETE FROM lyrics WHERE song_id = ?1")?
                .execute([song])?;
            for (kind, lang, source, content) in &wanted {
                tx.prepare_cached(
                    "INSERT INTO lyrics (song_id, kind, lang, source, content)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![song, kind, lang, source, content])?;
            }
        }
        Ok(changed)
    }

    /// Resolves the file's artists, album, folder, artwork and genres to rows, creating them as
    /// needed, and builds the values of [`FILE_COLUMNS`].
    #[expect(clippy::too_many_lines, reason = "one value per song column")]
    fn resolve(&mut self, tx: &mut WriteTx<'_>, file: &ScannedFile) -> Result<Resolved, DbError> {
        let tags = &file.tags;
        let names = Names::of(tags);
        let (folder, folder_pid) = crate::repo::folders::ensure(
            tx,
            self.library,
            &self.library_pid,
            rel_parent(&file.rel_path),
        )?;
        let mut credits = Vec::new();
        let mut seen = HashSet::new();
        let mut main_position = 0;
        let mut featured_position = 0;
        for (index, (credit, role)) in names.credits.iter().enumerate() {
            let sort = if index == 0 {
                tags.sort_artist.as_deref()
            } else {
                None
            };
            let mbz = if index == 0 {
                tags.mbz_artist.as_deref()
            } else {
                None
            };
            let artist = self.artist(tx, &credit.name, &credit.identity, sort, mbz)?;
            let (role, position) = match role {
                tracks_model::credits::CreditRole::Primary => {
                    main_position += 1;
                    (CreditRole::Main, main_position - 1)
                }
                tracks_model::credits::CreditRole::Featured => {
                    featured_position += 1;
                    (CreditRole::Featured, featured_position - 1)
                }
            };
            if seen.insert((artist, role)) {
                credits.push(CreditRow {
                    artist,
                    role,
                    position,
                });
            }
        }
        let lead_artist = credits.first().map(|c| c.artist);
        let mut album_artist = None;
        for (position, credit) in names.album_artists.iter().enumerate() {
            let mbz = if position == 0 {
                tags.mbz_album_artist.as_deref()
            } else {
                None
            };
            let artist = self.artist(tx, &credit.name, &credit.identity, None, mbz)?;
            album_artist.get_or_insert(artist);
            if names.album_artist_tagged && seen.insert((artist, CreditRole::AlbumArtist)) {
                credits.push(CreditRow {
                    artist,
                    role: CreditRole::AlbumArtist,
                    position: position as i64,
                });
            }
        }
        let album_artist = album_artist.or(lead_artist);
        if let Some(artwork) = &file.artwork {
            let source = match self.origin {
                SongOrigin::Scan => "embedded",
                SongOrigin::Upload => "upload",
                SongOrigin::Acquired => "acquired",
            };
            tx.prepare_cached(
                "INSERT INTO artwork (id, mime, width, height, bytes, source_kind, created_at)
                 VALUES (?1, '', 0, 0, 0, ?2, ?3) ON CONFLICT (id) DO NOTHING",
            )?
            .execute(params![artwork, source, tx.now()])?;
        }
        let album = self.album(tx, file, &names, &folder_pid, album_artist)?;
        let mut genres = Vec::new();
        for name in &tags.genres {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let genre = self.genre(tx, name)?;
            if !genres.contains(&genre) {
                genres.push(genre);
            }
        }
        let first_genre = tags.genres.iter().map(|g| g.trim()).find(|g| !g.is_empty());
        let gain = tags.replay_gain.unwrap_or_default();
        let title = tags.title.trim();
        let artist = tags.artist.trim();
        let album_title = trimmed(tags.album.as_deref());
        let search = tracks_model::search_folded(&format!(
            "{title} {artist} {}",
            album_title.unwrap_or_default()
        ));
        let props = &file.properties;
        let values = vec![
            Value::Integer(self.library.0),
            Value::Integer(folder),
            Value::Blob(file.rel_path.clone()),
            Value::Text(file.rel_path_display.clone()),
            Value::Integer(from_u64(file.size)),
            Value::Integer(file.mtime_ns),
            file.dev
                .map_or(Value::Null, |v| Value::Integer(from_u64(v))),
            file.ino
                .map_or(Value::Null, |v| Value::Integer(from_u64(v))),
            Value::Blob(file.digest.as_bytes().to_vec()),
            Value::Text(title.to_owned()),
            Value::Text(tracks_model::folded(
                trimmed(tags.sort_title.as_deref()).unwrap_or(title),
            )),
            Value::Text(artist.to_owned()),
            int(lead_artist),
            Value::Integer(album),
            text(album_title),
            text(trimmed(tags.album_artist.as_deref())),
            int(tags.track),
            int(tags.disc),
            text(trimmed(tags.disc_subtitle.as_deref())),
            int(tags.year),
            text(trimmed(tags.release_date.as_deref())),
            text(first_genre),
            Value::Integer(i64::from(props.duration_ms)),
            int(props.bitrate_kbps),
            int(props.sample_rate),
            int(props.bit_depth),
            int(props.channels),
            Value::Text(props.codec.as_str().to_owned()),
            Value::Text(props.container.as_str().to_owned()),
            Value::Text(props.container.content_type().to_owned()),
            Value::Integer(i64::from(props.codec.is_lossless())),
            Value::Text(tracks_model::song_key(title, artist)),
            text(trimmed(tags.mbz_recording.as_deref())),
            text(trimmed(tags.mbz_track.as_deref())),
            text(trimmed(tags.mbz_release.as_deref())),
            text(trimmed(tags.isrc.as_deref())),
            int(tags.bpm),
            text(trimmed(tags.comment.as_deref())),
            text(tags.explicit.map(Explicit::as_str)),
            real(gain.track_gain),
            real(gain.track_peak),
            real(gain.album_gain),
            real(gain.album_peak),
            Value::Integer(i64::from(!file.lyrics.is_empty())),
            text(file.artwork.as_ref().map(PublicId::as_str)),
            text(file.tag_problem.as_deref()),
            Value::Text(search),
        ];
        debug_assert_eq!(values.len(), FILE_COLUMNS.len());
        Ok(Resolved {
            values,
            credits,
            genres,
            album: Some(album),
            album_artist,
        })
    }

    fn genre(&mut self, tx: &WriteTx<'_>, name: &str) -> Result<i64, DbError> {
        let key = tracks_model::folded(name);
        if let Some(id) = self.genres.get(&key) {
            return Ok(*id);
        }
        let found: Option<i64> = tx
            .prepare_cached("SELECT id FROM genres WHERE name_key = ?1")?
            .query_row([&key], |row| row.get(0))
            .optional()?;
        let id = match found {
            Some(id) => id,
            None => {
                tx.prepare_cached("INSERT INTO genres (name, name_key) VALUES (?1, ?2)")?
                    .execute(params![name, key])?;
                tx.last_insert_rowid()
            }
        };
        self.genres.insert(key, id);
        Ok(id)
    }

    /// The artist row for a credit. A different spelling of a known artist renames it, once per
    /// batch, and every song and album showing the name is recorded for sync.
    fn artist(
        &mut self,
        tx: &mut WriteTx<'_>,
        name: &str,
        identity: &str,
        sort: Option<&str>,
        mbz: Option<&str>,
    ) -> Result<i64, DbError> {
        if let Some(id) = self.artists.get(identity) {
            return Ok(*id);
        }
        let sort_name = tracks_model::folded(trimmed(sort).unwrap_or(name));
        let search = tracks_model::search_folded(name);
        let now = tx.now();
        let found: Option<(i64, String, Option<String>)> = tx
            .prepare_cached("SELECT id, name, mbz_artist_id FROM artists WHERE identity = ?1")?
            .query_row([identity], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let id = match found {
            None => {
                tx.prepare_cached(
                    "INSERT INTO artists (pid, identity, name, sort_name, mbz_artist_id, search_text,
                         created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                )?
                .execute(params![
                    ArtistId::derive(identity),
                    identity,
                    name,
                    sort_name,
                    trimmed(mbz),
                    search,
                    now
                ])?;
                let id = tx.last_insert_rowid();
                self.dirty_artists.insert(id);
                id
            }
            Some((id, stored, stored_mbz)) => {
                if stored != name {
                    tx.prepare_cached(
                        "UPDATE artists SET name = ?2, sort_name = ?3, search_text = ?4,
                             updated_at = ?5 WHERE id = ?1",
                    )?
                    .execute(params![id, name, sort_name, search, now])?;
                    self.dirty_artists.insert(id);
                    record_artist_fanout(tx, id)?;
                }
                if stored_mbz.is_none() && trimmed(mbz).is_some() {
                    tx.prepare_cached("UPDATE artists SET mbz_artist_id = ?2 WHERE id = ?1")?
                        .execute(params![id, trimmed(mbz)])?;
                    self.dirty_artists.insert(id);
                }
                id
            }
        };
        self.artists.insert(identity.to_owned(), id);
        Ok(id)
    }

    /// The album row for a file, created or updated from its tags once per batch.
    #[expect(clippy::too_many_lines, reason = "one value per album column")]
    fn album(
        &mut self,
        tx: &mut WriteTx<'_>,
        file: &ScannedFile,
        names: &Names,
        folder_pid: &FolderId,
        album_artist: Option<i64>,
    ) -> Result<i64, DbError> {
        let tags = &file.tags;
        let title = trimmed(tags.album.as_deref());
        let album_key =
            title.and_then(|title| tracks_model::album_key(title, &names.album_lead_identity));
        let identity = match (trimmed(tags.mbz_release.as_deref()), &album_key) {
            (Some(release), _) => format!("mb:{}", release.to_ascii_lowercase()),
            (None, Some(key)) => key.clone(),
            (None, None) => format!("folder:{folder_pid}"),
        };
        if let Some(id) = self.albums.get(&identity) {
            return Ok(*id);
        }
        let name = match title {
            Some(title) => title.to_owned(),
            None => {
                let folder =
                    String::from_utf8_lossy(rel_name(rel_parent(&file.rel_path))).into_owned();
                if folder.is_empty() {
                    String::from("Unknown Album")
                } else {
                    folder
                }
            }
        };
        let sort_name = tracks_model::folded(trimmed(tags.sort_album.as_deref()).unwrap_or(&name));
        let search = tracks_model::search_folded(&format!("{name} {}", names.album_artist));
        let now = tx.now();
        let release_date = trimmed(tags.release_date.as_deref());
        let original_date = trimmed(tags.original_date.as_deref());
        let genre = tags.genres.iter().map(|g| g.trim()).find(|g| !g.is_empty());
        let release_type = trimmed(tags.release_type.as_deref());
        let mbz_release = trimmed(tags.mbz_release.as_deref());
        let mbz_group = trimmed(tags.mbz_release_group.as_deref());
        let values: [&dyn rusqlite::ToSql; 15] = [
            &name,
            &sort_name,
            &names.album_artist,
            &album_artist,
            &tags.year,
            &release_date,
            &original_date,
            &genre,
            &tags.compilation,
            &release_type,
            &mbz_release,
            &mbz_group,
            &file.artwork,
            &album_key,
            &search,
        ];
        let found: Option<(i64, String, String)> = tx
            .prepare_cached(
                "SELECT id, name, album_artist FROM albums WHERE library_id = ?1 AND identity = ?2",
            )?
            .query_row(params![self.library, identity], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let id = match found {
            None => {
                let pid = AlbumId::derive(&self.library_pid, &identity);
                let mut all: Vec<&dyn rusqlite::ToSql> = values.to_vec();
                all.push(&pid);
                all.push(&self.library);
                all.push(&identity);
                all.push(&now);
                tx.prepare_cached(
                    "INSERT INTO albums (name, sort_name, album_artist, artist_id, year, release_date,
                         original_date, genre, is_compilation, release_type, mbz_release_id,
                         mbz_release_group_id, artwork_id, album_key, search_text, pid, library_id,
                         identity, added_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19, ?19)",
                )?
                .execute(all.as_slice())?;
                let id = tx.last_insert_rowid();
                self.dirty_albums.insert(id);
                id
            }
            Some((id, stored_name, stored_artist)) => {
                let mut all: Vec<&dyn rusqlite::ToSql> = values.to_vec();
                all.push(&id);
                all.push(&now);
                let changed = tx
                    .prepare_cached(
                        "UPDATE albums SET name = ?1, sort_name = ?2, album_artist = ?3,
                             artist_id = ?4, year = ?5, release_date = ?6, original_date = ?7,
                             genre = ?8, is_compilation = ?9, release_type = ?10,
                             mbz_release_id = ?11, mbz_release_group_id = ?12,
                             artwork_id = coalesce(?13, artwork_id), album_key = ?14,
                             search_text = ?15, updated_at = ?17
                         WHERE id = ?16 AND NOT (name IS ?1 AND sort_name IS ?2
                             AND album_artist IS ?3 AND artist_id IS ?4 AND year IS ?5
                             AND release_date IS ?6 AND original_date IS ?7 AND genre IS ?8
                             AND is_compilation IS ?9 AND release_type IS ?10
                             AND mbz_release_id IS ?11 AND mbz_release_group_id IS ?12
                             AND artwork_id IS coalesce(?13, artwork_id) AND album_key IS ?14
                             AND search_text IS ?15)",
                    )?
                    .execute(all.as_slice())?
                    > 0;
                if changed {
                    self.dirty_albums.insert(id);
                    if stored_name != name || stored_artist != names.album_artist {
                        record_album_fanout(tx, id)?;
                    }
                }
                id
            }
        };
        self.albums.insert(identity, id);
        Ok(id)
    }

    fn finish(self, tx: &mut WriteTx<'_>) -> Result<(), DbError> {
        refresh_albums(tx, self.touched_albums, &self.dirty_albums)?;
        refresh_artists_marking(tx, self.touched_artists, &self.dirty_artists, false)?;
        for (library, delta) in self.present {
            if delta != 0 {
                tx.prepare_cached(
                    "UPDATE libraries SET song_count = max(0, song_count + ?2) WHERE id = ?1",
                )?
                .execute(params![library, delta])?;
                record_library(tx, library)?;
            }
        }
        Ok(())
    }
}

/// Records every song crediting the artist, and every album it is the album artist of, so
/// clients see the new spelling of its name.
fn record_artist_fanout(tx: &mut WriteTx<'_>, artist: i64) -> Result<(), DbError> {
    let songs: Vec<(SongId, LibraryKey)> = tx
        .prepare_cached(
            "SELECT DISTINCT s.pid, s.library_id FROM song_artists sa
             JOIN songs s ON s.id = sa.song_id WHERE sa.artist_id = ?1",
        )?
        .query_map([artist], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (pid, library) in songs {
        tx.record(Change::shared(
            ChangeKind::Song,
            pid.as_str(),
            Some(library),
            ChangeOp::Upsert,
        ))?;
    }
    let albums: Vec<(AlbumId, LibraryKey)> = tx
        .prepare_cached("SELECT pid, library_id FROM albums WHERE artist_id = ?1")?
        .query_map([artist], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (pid, library) in albums {
        tx.record(Change::shared(
            ChangeKind::Album,
            pid.as_str(),
            Some(library),
            ChangeOp::Upsert,
        ))?;
    }
    Ok(())
}

/// Records every song of the album, so clients that show album names from their song mirror
/// see a rename.
fn record_album_fanout(tx: &mut WriteTx<'_>, album: i64) -> Result<(), DbError> {
    let songs: Vec<(SongId, LibraryKey)> = tx
        .prepare_cached("SELECT pid, library_id FROM songs WHERE album_id = ?1")?
        .query_map([album], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (pid, library) in songs {
        tx.record(Change::shared(
            ChangeKind::Song,
            pid.as_str(),
            Some(library),
            ChangeOp::Upsert,
        ))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    SamePath,
    Moved,
    Replaced,
}

/// A file's tags resolved to rows.
struct Resolved {
    values: Vec<Value>,
    credits: Vec<CreditRow>,
    genres: Vec<i64>,
    album: Option<i64>,
    album_artist: Option<i64>,
}

/// The names a file credits, parsed the way Tracks parses them.
struct Names {
    credits: Vec<(
        tracks_model::ArtistCredit,
        tracks_model::credits::CreditRole,
    )>,
    /// Display name of the album artist.
    album_artist: String,
    album_artists: Vec<tracks_model::ArtistCredit>,
    album_lead_identity: String,
    /// The file has an album artist tag (or is a compilation).
    album_artist_tagged: bool,
}

const UNKNOWN_ARTIST: &str = "Unknown Artist";
const VARIOUS_ARTISTS: &str = "Various Artists";

impl Names {
    fn of(tags: &ScannedTags) -> Self {
        let artist = trimmed(Some(tags.artist.as_str())).unwrap_or(UNKNOWN_ARTIST);
        let mut credits = tracks_model::credits::credits_by_name_with_roles(artist, &tags.title);
        if credits.is_empty() {
            credits = tracks_model::credits::credits_by_name_with_roles(UNKNOWN_ARTIST, "");
        }
        let lead_name = credits
            .iter()
            .find(|(_, role)| *role == tracks_model::credits::CreditRole::Primary)
            .or_else(|| credits.first())
            .map_or_else(|| artist.to_owned(), |(credit, _)| credit.name.clone());
        let tagged = trimmed(tags.album_artist.as_deref())
            .map(str::to_owned)
            .or_else(|| tags.compilation.then(|| String::from(VARIOUS_ARTISTS)));
        let album_artist_tagged = tagged.is_some();
        let album_artist = tagged.unwrap_or(lead_name);
        let mut album_artists = tracks_model::credits_by_name(&album_artist, "");
        if album_artists.is_empty() {
            album_artists = tracks_model::credits_by_name(UNKNOWN_ARTIST, "");
        }
        let album_lead_identity = album_artists
            .first()
            .map(|credit| credit.identity.clone())
            .unwrap_or_default();
        Self {
            credits,
            album_artist,
            album_artists,
            album_lead_identity,
            album_artist_tagged,
        }
    }
}
