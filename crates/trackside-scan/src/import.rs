//! Importing single files: `import_file` for the acquirer and the importer, and the importer
//! itself for uploads and the inbox folder (verify, move into the uploads library with
//! `uploads.path_template`, index).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use trackside_core::control::{ImportOutcome, ScanControlError, ScanTrigger, SongOrigin};
use trackside_core::ingest::{Resolution, ScannedFile};
use trackside_core::records::{
    ImportSource, ImportState, Library, ScanCounts, ScanRunKind, ScanState,
};
use trackside_core::repo::artwork::NewArtwork;
use trackside_core::repo::folders::FolderUpsert;
use trackside_core::repo::imports::{ImportRow, NewImport};
use trackside_core::{ArtworkId, FolderId, ImportId, LibraryId, ServerEvent, SongId};

use crate::artwork::{ArtworkCache, pick_artist, sidecar_in};
use crate::catalog::Batch;
use crate::paths::{
    TemplateValues, display, file_name, is_hidden, os_bytes, parent_rel, render_template,
    with_collision_suffix,
};
use crate::reader::{FileStat, Job, Outcome, SafeReader};
use crate::run::recently_modified;
use crate::scanner::Inner;
use crate::tags::ReadHints;

/// How often the inbox folder is checked when no event arrives.
pub const INBOX_INTERVAL: Duration = Duration::from_secs(30);
/// Where rejected inbox files are moved, inside the inbox.
pub const REJECTED_DIR: &str = ".rejected";

/// The relative path as raw bytes, refusing anything that could leave the library.
pub fn safe_rel(rel: &Path) -> Result<Vec<u8>, ScanControlError> {
    let mut out = Vec::new();
    for part in rel.components() {
        match part {
            Component::Normal(name) => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(&os_bytes(name));
            }
            Component::CurDir => {}
            _ => {
                return Err(ScanControlError::Rejected(String::from(
                    "the path must be relative and stay inside the library",
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(ScanControlError::Rejected(String::from(
            "the path names no file",
        )));
    }
    Ok(out)
}

pub async fn import_file(
    inner: Arc<Inner>,
    library: LibraryId,
    rel_path: PathBuf,
    origin: SongOrigin,
) -> Result<ImportOutcome, ScanControlError> {
    let rel = safe_rel(&rel_path)?;
    tokio::task::spawn_blocking(move || {
        let library = inner.resolve(&library)?;
        if !inner.is_available(&library.id) {
            return Err(ScanControlError::Unavailable);
        }
        import_blocking(&inner, &library, &rel, origin)
    })
    .await
    .map_err(|e| ScanControlError::Io(io::Error::other(e)))?
}

/// Reads one file inside `library` and indexes it in its own run, returning its final id.
pub fn import_blocking(
    inner: &Inner,
    library: &Library,
    rel: &[u8],
    origin: SongOrigin,
) -> Result<ImportOutcome, ScanControlError> {
    let catalog = &inner.services.catalog;
    let path = crate::paths::join_rel(&library.root, rel);
    let meta = std::fs::symlink_metadata(&path)?;
    if !meta.is_file() {
        return Err(ScanControlError::Rejected(String::from(
            "not a regular file",
        )));
    }
    let stat = FileStat::of(&meta);
    let known = catalog.known_file(library.key, rel)?;
    if let Some(k) = &known
        && k.size == stat.size
        && k.mtime_ns == stat.mtime_ns
        && k.missing_since.is_none()
    {
        return Ok(ImportOutcome::Existing(k.song.clone()));
    }

    let read = read_one(inner, library, rel, stat, known.is_some())?;
    let mut gone = Vec::new();
    if known.is_none()
        && let Some(existing) = catalog.present_with_digest(&read.file.digest)?
    {
        if !file_is_gone(&existing.path) {
            return Ok(ImportOutcome::Existing(existing.song));
        }
        gone.push((existing.library, existing.rel));
    }
    commit_one(inner, library, read, gone, origin)
}

/// Whether a file the index has as present is really gone from disk. Any doubt (a permission
/// error, a hung share) counts as present.
pub fn file_is_gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

/// A read file with the artwork rows stored while reading it.
struct ReadOne {
    file: ScannedFile,
    artwork: Vec<NewArtwork>,
    artist_art: Option<(String, ArtworkId)>,
}

/// Reads one file of a library into a [`ScannedFile`], with the same safety as a scan.
fn read_one(
    inner: &Inner,
    library: &Library,
    rel: &[u8],
    stat: FileStat,
    known: bool,
) -> Result<ReadOne, ScanControlError> {
    let path = crate::paths::join_rel(&library.root, rel);
    let folder_rel = parent_rel(rel).to_vec();
    let folder_abs = crate::paths::join_rel(&library.root, &folder_rel);
    let listing: Vec<Vec<u8>> = std::fs::read_dir(&folder_abs)?
        .filter_map(Result::ok)
        .map(|e| os_bytes(&e.file_name()))
        .collect();
    let parent_listing: Vec<Vec<u8>> = if folder_rel.is_empty() {
        Vec::new()
    } else {
        let parent = crate::paths::join_rel(&library.root, parent_rel(&folder_rel));
        std::fs::read_dir(&parent)
            .map(|d| {
                d.filter_map(Result::ok)
                    .map(|e| os_bytes(&e.file_name()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let artist_art = pick_artist(&listing)
        .map(|name| folder_abs.join(crate::paths::os_from_bytes(name)))
        .or_else(|| {
            pick_artist(&parent_listing).map(|name| {
                crate::paths::join_rel(&library.root, parent_rel(&folder_rel))
                    .join(crate::paths::os_from_bytes(name))
            })
        });
    let job = Job {
        artist_art,
        hints: ReadHints {
            lyrics_sidecar: crate::lyrics::sidecar_path(&path, Some(&listing)),
            listing_known: true,
        },
        sidecar_art: sidecar_in(&folder_abs, &listing),
        path,
        rel: rel.to_vec(),
        folder: FolderId::derive(&library.id, &folder_rel),
        stat,
        known,
    };
    let mut reader = SafeReader::new(
        Arc::clone(&inner.services.reader),
        inner.settings.file_timeout,
    );
    let cache = ArtworkCache::default();
    match crate::reader::process(&job, &mut reader, inner.services.artwork.as_ref(), &cache) {
        Outcome::Read(read) => {
            let artist_art = read.artist_art.map(|id| {
                let tags = &read.file.tags;
                (
                    tags.album_artist
                        .clone()
                        .unwrap_or_else(|| tags.artist.clone()),
                    id,
                )
            });
            Ok(ReadOne {
                file: read.file,
                artwork: cache.take_fresh(),
                artist_art,
            })
        }
        Outcome::Deferred => Err(ScanControlError::Rejected(String::from(
            "the file changed while it was read",
        ))),
        Outcome::Failed(error) | Outcome::Quarantine(error) => {
            Err(ScanControlError::Rejected(error))
        }
    }
}

/// Indexes one read file in its own `file` run.
fn commit_one(
    inner: &Inner,
    library: &Library,
    read: ReadOne,
    gone: Vec<(trackside_core::LibraryKey, Vec<u8>)>,
    origin: SongOrigin,
) -> Result<ImportOutcome, ScanControlError> {
    let catalog = &inner.services.catalog;
    let folder_rel = parent_rel(&read.file.rel_path).to_vec();
    let run = catalog.start_run(library.key, ScanRunKind::File, ScanTrigger::Import)?;
    let folder_name = if folder_rel.is_empty() {
        library.name.clone()
    } else {
        display(file_name(&folder_rel))
    };
    let batch = Batch {
        folders: vec![FolderUpsert {
            rel_path: folder_rel,
            name: folder_name,
            mtime_ns: None,
            listing_hash: None,
        }],
        files: vec![read.file],
        artwork: read.artwork,
        gone,
        artist_artwork: read.artist_art.into_iter().collect(),
        ..Batch::default()
    };
    let resolutions = match catalog.commit(library.key, &run.id, origin, batch) {
        Ok(r) => r,
        Err(error) => {
            let _ = catalog.end_run(
                library.key,
                &run.id,
                ScanState::Failed,
                &ScanCounts::default(),
                Some(&error.to_string()),
                None,
            );
            return Err(error.into());
        }
    };
    let mut counts = ScanCounts {
        files_seen: 1,
        ..ScanCounts::default()
    };
    let outcome = match resolutions.into_iter().next() {
        Some(Resolution::Unchanged(id)) => ImportOutcome::Existing(id),
        Some(resolution) => {
            match &resolution {
                Resolution::Added(_) => counts.added = 1,
                Resolution::Moved(_) => counts.moved = 1,
                _ => counts.updated = 1,
            }
            ImportOutcome::Added(resolution.song().clone())
        }
        None => {
            return Err(ScanControlError::Rejected(String::from(
                "the file was not indexed",
            )));
        }
    };
    catalog.end_run(library.key, &run.id, ScanState::Done, &counts, None, None)?;
    Ok(outcome)
}

/// Containers the importer accepts, by their first bytes.
pub fn sniff(path: &Path) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut head = [0u8; 12];
    let n = read_up_to(&mut file, &mut head)?;
    let head = &head[..n];
    let mut start = 0u64;
    if head.starts_with(b"ID3") && n >= 10 {
        let size = head[6..10]
            .iter()
            .fold(0u64, |acc, b| (acc << 7) | u64::from(b & 0x7f));
        start = 10 + size;
        if head[5] & 0x10 != 0 {
            start += 10;
        }
    }
    let mut after = [0u8; 4];
    file.seek(SeekFrom::Start(start))?;
    let m = read_up_to(&mut file, &mut after)?;
    let after = &after[..m];
    if after.starts_with(b"fLaC") {
        return Ok(true);
    }
    if after.len() >= 2 && after[0] == 0xFF && after[1] & 0xE0 == 0xE0 {
        return Ok(true);
    }
    if start > 0 {
        return Ok(false);
    }
    let accepted = head.starts_with(b"OggS")
        || (head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE"))
        || (head.starts_with(b"FORM") && matches!(head.get(8..12), Some(b"AIFF" | b"AIFC")))
        || head.starts_with(b"wvpk")
        || head.starts_with(b"MAC ");
    if accepted {
        return Ok(true);
    }
    if head.get(4..8) == Some(b"ftyp") {
        return Ok(!mp4_has_video(&mut file)?);
    }
    Ok(false)
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Whether any track's handler is `vide`. Walks moov/trak/mdia/hdlr only.
fn mp4_has_video(file: &mut File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    let mut stack = vec![(0u64, len, 0u8)];
    while let Some((mut pos, end, depth)) = stack.pop() {
        while pos + 8 <= end {
            let mut header = [0u8; 8];
            file.seek(SeekFrom::Start(pos))?;
            if read_up_to(file, &mut header)? < 8 {
                break;
            }
            let size = u64::from(u32::from_be_bytes([
                header[0], header[1], header[2], header[3],
            ]));
            let kind = &header[4..8];
            if size < 8 || pos + size > end {
                break;
            }
            match kind {
                b"moov" | b"trak" | b"mdia" if depth < 4 => {
                    stack.push((pos + 8, pos + size, depth + 1));
                }
                b"hdlr" => {
                    let mut body = [0u8; 12];
                    file.seek(SeekFrom::Start(pos + 8))?;
                    if read_up_to(file, &mut body)? == 12 && &body[8..12] == b"vide" {
                        return Ok(true);
                    }
                }
                _ => {}
            }
            pos += size;
        }
    }
    Ok(false)
}

/// Why a staged file is refused, or `None` when it may be imported.
pub fn verify(path: &Path, max_bytes: u64) -> io::Result<Option<String>> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Ok(Some(String::from("not a regular file")));
    }
    if meta.len() == 0 {
        return Ok(Some(String::from("the file is empty")));
    }
    if meta.len() > max_bytes {
        return Ok(Some(format!("larger than the {max_bytes} byte limit")));
    }
    if !sniff(path)? {
        return Ok(Some(String::from("not an audio format the server accepts")));
    }
    Ok(None)
}

/// Moves a file, copying when the target is on another filesystem.
pub fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            std::fs::copy(from, to)?;
            File::open(to)?.sync_all()?;
            std::fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

pub async fn process_upload(inner: Arc<Inner>, import: ImportId) -> Result<(), ScanControlError> {
    tokio::task::spawn_blocking(move || {
        let row = inner
            .services
            .catalog
            .import(&import)?
            .ok_or(ScanControlError::UnknownImport)?;
        if row.state != ImportState::Staged {
            return Err(ScanControlError::Rejected(format!(
                "the upload is {}, not staged",
                row.state
            )));
        }
        process_row(&inner, &row)
    })
    .await
    .map_err(|e| ScanControlError::Io(io::Error::other(e)))?
}

/// The importer: verify, refuse duplicates, move into the target library and index.
fn process_row(inner: &Inner, row: &ImportRow) -> Result<(), ScanControlError> {
    let catalog = &inner.services.catalog;
    let staged = row
        .staged_path
        .clone()
        .ok_or_else(|| ScanControlError::Rejected(String::from("the upload has no file")))?;
    let target = match row.target_library {
        Some(key) => catalog.libraries()?.into_iter().find(|l| l.key == key),
        None => catalog.uploads_library()?,
    };
    let Some(target) = target else {
        finish(
            inner,
            row,
            ImportState::Failed,
            None,
            Some("no uploads library is set"),
        )?;
        return Err(ScanControlError::NotWritable);
    };
    if !target.writable {
        finish(
            inner,
            row,
            ImportState::Failed,
            None,
            Some("the uploads library is not writable"),
        )?;
        return Err(ScanControlError::NotWritable);
    }
    if !inner.is_available(&target.id) {
        return Err(ScanControlError::Unavailable);
    }

    if let Some(reason) = verify(&staged, inner.uploads.max_file)? {
        discard(row, &staged);
        return finish(inner, row, ImportState::Rejected, None, Some(&reason));
    }
    let parsed = match crate::read_file(&staged) {
        Ok(p) => p,
        Err(e) => {
            discard(row, &staged);
            return finish(
                inner,
                row,
                ImportState::Rejected,
                None,
                Some(&e.to_string()),
            );
        }
    };
    let digest = inner
        .services
        .reader
        .digest(&staged, parsed.properties.container)?
        .0;
    if let Some(existing) = catalog.present_with_digest(&digest)?
        && !file_is_gone(&existing.path)
    {
        discard(row, &staged);
        return finish(
            inner,
            row,
            ImportState::Duplicate,
            Some(&existing.song),
            None,
        );
    }

    let dest = destination(inner, &target.root, row, &staged, &parsed);
    if let Err(e) = move_file(&staged, &dest) {
        let message = if e.kind() == io::ErrorKind::StorageFull {
            String::from("insufficient_space")
        } else {
            e.to_string()
        };
        finish(inner, row, ImportState::Failed, None, Some(&message))?;
        return Err(e.into());
    }
    let rel = crate::paths::rel_of(&target.root, &dest).ok_or_else(|| {
        ScanControlError::Rejected(String::from("the destination left the library"))
    })?;
    match import_blocking(inner, &target, &rel, SongOrigin::Upload) {
        Ok(ImportOutcome::Added(id)) => finish(inner, row, ImportState::Imported, Some(&id), None),
        Ok(ImportOutcome::Existing(id)) => {
            finish(inner, row, ImportState::Duplicate, Some(&id), None)
        }
        Err(error) => {
            finish(
                inner,
                row,
                ImportState::Failed,
                None,
                Some(&error.to_string()),
            )?;
            Err(error)
        }
    }
}

/// Where an accepted file goes in the target library: the path template filled from its tags,
/// with a numbered suffix when the name is taken. The extension is the one the file was sent
/// or found with, since an upload is staged under a `.part` name the walker skips.
fn destination(
    inner: &Inner,
    root: &Path,
    row: &ImportRow,
    staged: &Path,
    parsed: &crate::ParsedFile,
) -> PathBuf {
    let container = parsed.properties.container.as_str();
    let ext = [Path::new(row.original_name.trim()), staged]
        .into_iter()
        .filter_map(|p| p.extension())
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .find(|e| crate::paths::container_for_extension(e).is_some())
        .unwrap_or_else(|| container.to_owned());
    let values = TemplateValues {
        artist: parsed.tags.artist.clone(),
        album_artist: parsed.tags.album_artist.clone(),
        album: parsed.tags.album.clone(),
        title: parsed.tags.title.clone(),
        track: parsed.tags.track,
        disc: parsed.tags.disc,
        year: parsed.tags.year,
        ext,
    };
    let relative = render_template(&inner.uploads.path_template, &values);
    with_collision_suffix(&root.join(relative), |p| {
        std::fs::symlink_metadata(p).is_ok()
    })
}

/// A refused upload's staged copy is deleted; a refused inbox file is the user's own file and
/// is moved aside by the inbox poller instead.
fn discard(row: &ImportRow, staged: &Path) {
    if row.source == ImportSource::Upload {
        let _ = std::fs::remove_file(staged);
    }
}

fn finish(
    inner: &Inner,
    row: &ImportRow,
    state: ImportState,
    song: Option<&SongId>,
    error: Option<&str>,
) -> Result<(), ScanControlError> {
    inner
        .services
        .catalog
        .finish_import(&row.id, state, song, error)?;
    if let Some(user) = row.user {
        inner.events.publish(ServerEvent::Upload {
            user,
            import: row.id.clone(),
            state,
            song: song.cloned(),
        });
    }
    Ok(())
}

/// Inbox files that failed for a reason that may pass (the target was unavailable), by path,
/// with the size and mtime they had, so they are retried only once they change or the target
/// returns.
fn failed_inbox() -> &'static Mutex<HashMap<PathBuf, (u64, i64)>> {
    static FAILED: OnceLock<Mutex<HashMap<PathBuf, (u64, i64)>>> = OnceLock::new();
    FAILED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Settled audio files in the inbox, oldest folders first.
pub fn inbox_candidates(inbox: &Path, inner: &Inner) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![inbox.to_path_buf()];
    let now = SystemTime::now();
    let failed = failed_inbox()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = os_bytes(&entry.file_name());
            if is_hidden(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let path = entry.path();
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && inner.settings.extensions.matches(&name) {
                let stat = FileStat::of(&meta);
                if recently_modified(
                    stat.mtime_ns,
                    now,
                    inner.config.settle.max(Duration::from_secs(2)),
                ) {
                    continue;
                }
                if failed.get(&path) == Some(&(stat.size, stat.mtime_ns)) {
                    continue;
                }
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Imports every settled file in the inbox.
pub async fn poll_inbox(inner: Arc<Inner>) {
    let Some(inbox) = inner.uploads.inbox_dir.clone() else {
        return;
    };
    let worker = Arc::clone(&inner);
    let result = tokio::task::spawn_blocking(move || {
        for path in inbox_candidates(&inbox, &worker) {
            if worker.shutdown.is_cancelled() {
                break;
            }
            import_inbox_file(&worker, &inbox, &path);
        }
    })
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "inbox import stopped");
    }
}

fn import_inbox_file(inner: &Inner, inbox: &Path, path: &Path) {
    let catalog = &inner.services.catalog;
    let target = catalog.uploads_library().ok().flatten();
    let new = NewImport {
        user: None,
        source: ImportSource::Inbox,
        original_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        staged_path: Some(path.to_path_buf()),
        expected_bytes: std::fs::metadata(path).ok().map(|m| m.len()),
        target_library: target.map(|l| l.key),
    };
    let row = match catalog.create_import(&new) {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "inbox file not imported");
            remember_failure(path);
            return;
        }
    };
    let result = process_row(inner, &row);
    if path.exists() {
        match result {
            Ok(()) => {
                if let Ok(Some(done)) = catalog.import(&row.id)
                    && matches!(done.state, ImportState::Rejected | ImportState::Duplicate)
                {
                    move_rejected(inbox, path);
                }
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "inbox file not imported");
                remember_failure(path);
            }
        }
    }
}

fn remember_failure(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let stat = FileStat::of(&meta);
        failed_inbox()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (stat.size, stat.mtime_ns));
    }
}

fn move_rejected(inbox: &Path, path: &Path) {
    let Some(name) = path.file_name() else { return };
    let dest = with_collision_suffix(&inbox.join(REJECTED_DIR).join(name), Path::exists);
    if let Err(error) = move_file(path, &dest) {
        tracing::warn!(%error, path = %path.display(), "rejected inbox file not moved aside");
        remember_failure(path);
    }
}

/// Polls the inbox every [`INBOX_INTERVAL`] until shutdown.
pub async fn inbox_task(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(INBOX_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = inner.shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        poll_inbox(Arc::clone(&inner)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_cannot_escape() {
        assert_eq!(
            safe_rel(Path::new("a/b.flac")).ok(),
            Some(b"a/b.flac".to_vec())
        );
        assert!(safe_rel(Path::new("../x.flac")).is_err());
        assert!(safe_rel(Path::new("/etc/passwd")).is_err());
        assert!(safe_rel(Path::new("")).is_err());
        assert_eq!(
            safe_rel(Path::new("./a.flac")).ok(),
            Some(b"a.flac".to_vec())
        );
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn every_fixture_container_is_accepted() -> io::Result<()> {
        for name in [
            "tone-440.flac",
            "tone-440.mp3",
            "tone-440.m4a",
            "tone-440-alac.m4a",
            "tone-440.ogg",
            "tone-440.opus",
            "tone-440.wav",
            "tone-440.aiff",
        ] {
            assert!(sniff(&fixture(name))?, "{name}");
        }
        Ok(())
    }

    #[test]
    fn scripts_archives_and_symlinks_are_refused() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let script = dir.path().join("x.mp3");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n")?;
        assert!(verify(&script, 1 << 20)?.is_some());
        let zip = dir.path().join("y.flac");
        std::fs::write(&zip, b"PK\x03\x04rest")?;
        assert!(verify(&zip, 1 << 20)?.is_some());
        #[cfg(unix)]
        {
            let link = dir.path().join("z.flac");
            std::os::unix::fs::symlink(fixture("tone-440.flac"), &link)?;
            assert_eq!(
                verify(&link, 1 << 20)?.as_deref(),
                Some("not a regular file")
            );
        }
        let big = fixture("tone-440.flac");
        assert!(verify(&big, 10)?.is_some_and(|r| r.contains("limit")));
        assert_eq!(verify(&big, 1 << 20)?, None);
        Ok(())
    }

    #[test]
    fn an_mp4_with_a_video_track_is_refused() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("v.m4a");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&16u32.to_be_bytes());
        bytes.extend_from_slice(b"ftypisom\0\0\0\0");
        let hdlr_len = 8 + 12 + 13;
        let mdia_len = 8 + hdlr_len;
        let trak_len = 8 + mdia_len;
        let moov_len = 8 + trak_len;
        bytes.extend_from_slice(&(moov_len as u32).to_be_bytes());
        bytes.extend_from_slice(b"moov");
        bytes.extend_from_slice(&(trak_len as u32).to_be_bytes());
        bytes.extend_from_slice(b"trak");
        bytes.extend_from_slice(&(mdia_len as u32).to_be_bytes());
        bytes.extend_from_slice(b"mdia");
        bytes.extend_from_slice(&(hdlr_len as u32).to_be_bytes());
        bytes.extend_from_slice(b"hdlr");
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(b"vide");
        bytes.extend_from_slice(&[0u8; 13]);
        std::fs::write(&path, &bytes)?;
        assert!(!sniff(&path)?);
        assert!(sniff(&fixture("tone-440.m4a"))?);
        Ok(())
    }

    #[test]
    fn moving_creates_parent_folders() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let from = dir.path().join("a.flac");
        std::fs::write(&from, b"x")?;
        let to = dir.path().join("x/y/a.flac");
        move_file(&from, &to)?;
        assert!(to.is_file());
        assert!(!from.exists());
        Ok(())
    }
}
