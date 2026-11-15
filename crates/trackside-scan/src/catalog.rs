//! The scanner's view of the index. [`DbCatalog`] calls `trackside_core`'s repositories and
//! ingest; tests use an in-memory catalog with the same matching rules, so the pipeline can be
//! exercised without the database.

use std::path::{Path, PathBuf};

use trackside_core::control::ScanTrigger;
use trackside_core::ingest::{self, FinishOutcome, KnownFile, Resolution, ScannedFile, SeenSet};
use trackside_core::records::{ImportState, Library, ScanCounts, ScanRun, ScanRunKind, ScanState};
use trackside_core::repo::artwork::{self, NewArtwork};
use trackside_core::repo::folders::{self, FolderState, FolderUpsert};
use trackside_core::repo::imports::{self, ImportRow, NewImport};
use trackside_core::repo::scan_runs::{self, QuarantinedFile, ScanErrorRow};
use trackside_core::repo::{libraries, settings, songs};
use trackside_core::{
    Access, ArtworkId, AudioDigest, Db, DbError, ImportId, LibraryId, LibraryKey, PublicId,
    ScanRunId, SongId,
};

use crate::SongOrigin;
use crate::paths::rel_of;

/// What the root looked like after a healthy scan, for the empty-library guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootHealth {
    pub dev: Option<u64>,
    pub entry_count: u64,
}

/// A present song with some audio, and where its file is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentSong {
    pub song: SongId,
    pub library: LibraryKey,
    pub rel: Vec<u8>,
    pub path: PathBuf,
}

/// One write transaction's worth of scan results.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    /// Artwork rows with their real metadata, for images stored since the last batch.
    pub artwork: Vec<NewArtwork>,
    /// Paths whose files are gone, marked missing before the files are matched, so a file
    /// that moved there is matched to its song by digest.
    pub gone: Vec<(LibraryKey, Vec<u8>)>,
    /// Upserted before the files, so every file's folder exists.
    pub folders: Vec<FolderUpsert>,
    pub files: Vec<ScannedFile>,
    pub errors: Vec<ScanErrorRow>,
    pub quarantine: Vec<QuarantinedFile>,
    /// Folders found unreadable in this batch.
    pub unreadable: Vec<Vec<u8>>,
    /// Artist pictures, by credited name, applied after the files so the artists exist.
    pub artist_artwork: Vec<(String, ArtworkId)>,
    /// Counters to record on the run row.
    pub progress: Option<ScanCounts>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
            && self.artwork.is_empty()
            && self.gone.is_empty()
            && self.artist_artwork.is_empty()
            && self.files.is_empty()
            && self.errors.is_empty()
            && self.quarantine.is_empty()
            && self.unreadable.is_empty()
            && self.progress.is_none()
    }
}

/// Everything the scanner reads from and writes to the index. Methods block; the scanner calls
/// them from its own threads or the blocking pool.
pub trait Catalog: Send + Sync + 'static {
    fn libraries(&self) -> Result<Vec<Library>, DbError>;
    fn library(&self, id: &LibraryId) -> Result<Option<Library>, DbError>;
    fn root_health(&self, library: LibraryKey) -> Result<Option<RootHealth>, DbError>;
    fn set_available(
        &self,
        library: LibraryKey,
        available: bool,
        reason: Option<&str>,
    ) -> Result<bool, DbError>;

    fn folder_state(&self, library: LibraryKey, rel: &[u8])
    -> Result<Option<FolderState>, DbError>;
    fn known_file(&self, library: LibraryKey, rel: &[u8]) -> Result<Option<KnownFile>, DbError>;
    fn is_quarantined(
        &self,
        library: LibraryKey,
        rel: &[u8],
        size: u64,
        mtime_ns: i64,
    ) -> Result<bool, DbError>;
    /// A present song with exactly this audio, if any.
    fn present_with_digest(&self, digest: &AudioDigest) -> Result<Option<PresentSong>, DbError>;

    fn start_run(
        &self,
        library: LibraryKey,
        kind: ScanRunKind,
        trigger: ScanTrigger,
    ) -> Result<ScanRun, DbError>;
    fn commit(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        origin: SongOrigin,
        batch: Batch,
    ) -> Result<Vec<Resolution>, DbError>;
    fn mark_missing(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        seen: &SeenSet,
        unreadable: &[Vec<u8>],
    ) -> Result<FinishOutcome, DbError>;
    /// The missing pass of a scan of one folder and everything under it.
    fn finish_folder(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        folder: &[u8],
        seen: &SeenSet,
    ) -> Result<FinishOutcome, DbError>;
    fn end_run(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        state: ScanState,
        counts: &ScanCounts,
        message: Option<&str>,
        root: Option<RootHealth>,
    ) -> Result<ScanRun, DbError>;
    fn recent_runs(&self, library: LibraryKey, limit: u32) -> Result<Vec<ScanRun>, DbError>;

    fn import(&self, id: &ImportId) -> Result<Option<ImportRow>, DbError>;
    fn create_import(&self, new: &NewImport) -> Result<ImportRow, DbError>;
    fn finish_import(
        &self,
        id: &ImportId,
        state: ImportState,
        song: Option<&SongId>,
        error: Option<&str>,
    ) -> Result<ImportRow, DbError>;
    /// The writable library uploads and the inbox go to (setting `uploads.library`).
    fn uploads_library(&self) -> Result<Option<Library>, DbError>;
}

/// The real catalog over the database.
#[derive(Debug)]
pub struct DbCatalog {
    db: Db,
}

impl DbCatalog {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Catalog for DbCatalog {
    fn libraries(&self) -> Result<Vec<Library>, DbError> {
        self.db
            .read_blocking(|c| libraries::list(c, &Access::system()))
    }

    fn library(&self, id: &LibraryId) -> Result<Option<Library>, DbError> {
        self.db
            .read_blocking(|c| libraries::get(c, &Access::system(), id))
    }

    fn root_health(&self, library: LibraryKey) -> Result<Option<RootHealth>, DbError> {
        let stored = self
            .db
            .read_blocking(|c| libraries::root_health(c, library))?;
        Ok(stored.and_then(|h| {
            h.root_entry_count.map(|entry_count| RootHealth {
                dev: h.root_dev,
                entry_count,
            })
        }))
    }

    fn set_available(
        &self,
        library: LibraryKey,
        available: bool,
        reason: Option<&str>,
    ) -> Result<bool, DbError> {
        let reason = reason.map(str::to_owned);
        self.db.write_blocking(|tx| {
            libraries::set_available(tx, library, available, reason.as_deref())
        })
    }

    fn folder_state(
        &self,
        library: LibraryKey,
        rel: &[u8],
    ) -> Result<Option<FolderState>, DbError> {
        self.db
            .read_blocking(|c| folders::listing_hash(c, library, rel))
    }

    fn known_file(&self, library: LibraryKey, rel: &[u8]) -> Result<Option<KnownFile>, DbError> {
        self.db
            .read_blocking(|c| ingest::known_file(c, library, rel))
    }

    fn is_quarantined(
        &self,
        library: LibraryKey,
        rel: &[u8],
        size: u64,
        mtime_ns: i64,
    ) -> Result<bool, DbError> {
        self.db
            .read_blocking(|c| scan_runs::is_quarantined(c, library, rel, size, mtime_ns))
    }

    fn present_with_digest(&self, digest: &AudioDigest) -> Result<Option<PresentSong>, DbError> {
        // A song's id is derived from its digest when it is first seen, so the derived id
        // finds the original of an exact duplicate; its stored digest confirms the match.
        let candidate = SongId::derive(digest);
        self.db.read_blocking(|c| {
            let Some(file) = songs::file(c, &Access::system(), &candidate)? else {
                return Ok(None);
            };
            let Some(library) = libraries::by_key(c, file.library)? else {
                return Ok(None);
            };
            let Some(rel) = rel_of(&library.root, &file.path) else {
                return Ok(None);
            };
            let known = ingest::known_file(c, file.library, &rel)?;
            Ok(known
                .filter(|k| k.digest == *digest && k.missing_since.is_none())
                .map(|k| PresentSong {
                    song: k.song,
                    library: file.library,
                    rel,
                    path: file.path,
                }))
        })
    }

    fn start_run(
        &self,
        library: LibraryKey,
        kind: ScanRunKind,
        trigger: ScanTrigger,
    ) -> Result<ScanRun, DbError> {
        self.db
            .write_blocking(|tx| scan_runs::start(tx, library, kind, trigger))
    }

    fn commit(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        origin: SongOrigin,
        batch: Batch,
    ) -> Result<Vec<Resolution>, DbError> {
        self.db.write_blocking(|tx| {
            for art in &batch.artwork {
                artwork::put(tx, art)?;
            }
            let mut gone: Vec<&(LibraryKey, Vec<u8>)> = batch.gone.iter().collect();
            gone.sort();
            for group in gone.chunk_by(|a, b| a.0 == b.0) {
                let paths: Vec<Vec<u8>> = group.iter().map(|(_, rel)| rel.clone()).collect();
                ingest::mark_gone(tx, group[0].0, &paths)?;
            }
            for folder in &batch.folders {
                folders::upsert(tx, library, folder)?;
            }
            for rel in &batch.unreadable {
                folders::mark_unreadable(tx, library, rel, true)?;
            }
            let resolutions = if batch.files.is_empty() {
                Vec::new()
            } else {
                ingest::apply_batch(tx, library, run, origin, &batch.files)?
            };
            for (name, art) in &batch.artist_artwork {
                artwork::set_artist_artwork(tx, name, Some(art))?;
            }
            for error in &batch.errors {
                scan_runs::add_error(tx, run, error)?;
            }
            for file in &batch.quarantine {
                scan_runs::quarantine(tx, library, file)?;
            }
            if let Some(counts) = &batch.progress {
                scan_runs::progress(tx, run, counts)?;
            }
            Ok(resolutions)
        })
    }

    fn mark_missing(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        seen: &SeenSet,
        unreadable: &[Vec<u8>],
    ) -> Result<FinishOutcome, DbError> {
        self.db
            .write_blocking(|tx| ingest::finish_run(tx, library, run, seen, unreadable))
    }

    fn finish_folder(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        folder: &[u8],
        seen: &SeenSet,
    ) -> Result<FinishOutcome, DbError> {
        self.db
            .write_blocking(|tx| ingest::finish_folder(tx, library, run, folder, true, seen))
    }

    fn end_run(
        &self,
        library: LibraryKey,
        run: &ScanRunId,
        state: ScanState,
        counts: &ScanCounts,
        message: Option<&str>,
        root: Option<RootHealth>,
    ) -> Result<ScanRun, DbError> {
        self.db.write_blocking(|tx| {
            if let Some(root) = root {
                libraries::set_root_health(tx, library, root.dev, root.entry_count)?;
            }
            scan_runs::finish(tx, run, state, counts, message)
        })
    }

    fn recent_runs(&self, library: LibraryKey, limit: u32) -> Result<Vec<ScanRun>, DbError> {
        self.db
            .read_blocking(|c| scan_runs::list(c, library, limit))
    }

    fn import(&self, id: &ImportId) -> Result<Option<ImportRow>, DbError> {
        self.db.read_blocking(|c| imports::get(c, id))
    }

    fn create_import(&self, new: &NewImport) -> Result<ImportRow, DbError> {
        self.db.write_blocking(|tx| imports::create(tx, new))
    }

    fn finish_import(
        &self,
        id: &ImportId,
        state: ImportState,
        song: Option<&SongId>,
        error: Option<&str>,
    ) -> Result<ImportRow, DbError> {
        self.db
            .write_blocking(|tx| imports::finish(tx, id, state, song, error))
    }

    fn uploads_library(&self) -> Result<Option<Library>, DbError> {
        let target: Option<String> = self
            .db
            .read_blocking(|c| settings::get(c, "uploads.library"))?;
        let Some(target) = target else {
            return Ok(None);
        };
        let all = self.libraries()?;
        Ok(all
            .into_iter()
            .find(|l| l.id.as_str().eq_ignore_ascii_case(&target) || l.root == Path::new(&target)))
    }
}
