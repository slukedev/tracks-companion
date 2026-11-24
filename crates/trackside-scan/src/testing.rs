//! Test doubles: an in-memory catalog that follows the matching rules of architecture section
//! 6 closely enough to exercise the scanner, readers that count or misbehave, and audio
//! fixtures.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use trackside_core::control::ScanTrigger;
use trackside_core::ingest::{FinishOutcome, KnownFile, Resolution, SeenSet};
use trackside_core::records::{
    Container, ImportState, Library, ScanCounts, ScanRun, ScanRunKind, ScanState,
};
use trackside_core::repo::folders::FolderState;
use trackside_core::repo::imports::{ImportRow, NewImport};
use trackside_core::repo::scan_runs::{QuarantinedFile, ScanErrorRow};
use trackside_core::{
    AudioDigest, DbError, ImportId, LibraryId, LibraryKey, Millis, ScanRunId, SongId, random_id,
};

use crate::catalog::{Batch, Catalog, PresentSong, RootHealth};
use crate::reader::{FileReader, LoftyReader};
use crate::tags::ReadHints;
use crate::{DigestMode, ParsedFile, ReadError, SongOrigin};
use trackside_core::ArtworkId;
use trackside_core::repo::artwork::NewArtwork;

/// A song as the in-memory catalog keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct MemSong {
    pub id: SongId,
    pub size: u64,
    pub mtime_ns: i64,
    pub digest: AudioDigest,
    pub missing_since: Option<Millis>,
    pub content_version: i64,
    pub title: String,
    pub artist: String,
    pub origin: SongOrigin,
    pub tag_problem: Option<String>,
    pub has_artwork: bool,
    pub lyrics: usize,
}

#[derive(Debug, Default)]
pub struct Mem {
    pub libraries: Vec<Library>,
    pub songs: HashMap<(LibraryKey, Vec<u8>), MemSong>,
    pub folders: HashMap<(LibraryKey, Vec<u8>), FolderState>,
    pub quarantine: HashMap<(LibraryKey, Vec<u8>), QuarantinedFile>,
    pub runs: Vec<(LibraryKey, ScanRun)>,
    pub errors: Vec<ScanErrorRow>,
    pub root_health: HashMap<LibraryKey, RootHealth>,
    pub imports: Vec<ImportRow>,
    pub uploads_library: Option<LibraryKey>,
    pub largest_batch: usize,
    pub mark_missing_calls: usize,
    pub availability: Vec<(LibraryKey, bool)>,
    pub artwork: HashMap<ArtworkId, NewArtwork>,
    pub artist_artwork: HashMap<String, ArtworkId>,
    pub gone_marked: usize,
    pub folder_finishes: Vec<Vec<u8>>,
    /// Every commit fails, as on a full disk.
    pub fail_commits: bool,
}

/// The catalog tests use.
#[derive(Debug, Default)]
pub struct MemCatalog {
    pub mem: Mutex<Mem>,
    now: Mutex<i64>,
}

impl MemCatalog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mem: Mutex::new(Mem::default()),
            now: Mutex::new(1_794_648_600_000),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Mem> {
        self.mem.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> Millis {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now += 1;
        Millis(*now)
    }

    pub fn add_library(&self, root: &Path, name: &str) -> Library {
        let mut mem = self.lock();
        let key = LibraryKey(mem.libraries.len() as i64 + 1);
        let library = Library {
            id: random_id(),
            key,
            name: name.to_owned(),
            root: root.to_path_buf(),
            enabled: true,
            writable: true,
            watch: true,
            available: true,
            song_count: 0,
            quick_scan_interval: None,
            unavailable_reason: None,
            unavailable_since: None,
            last_scan_at: None,
            created_at: Millis(0),
        };
        mem.libraries.push(library.clone());
        library
    }

    /// The song at a path.
    pub fn song(&self, library: LibraryKey, rel: &[u8]) -> Option<MemSong> {
        self.lock().songs.get(&(library, rel.to_vec())).cloned()
    }

    pub fn present(&self, library: LibraryKey) -> Vec<(Vec<u8>, MemSong)> {
        let mut out: Vec<_> = self
            .lock()
            .songs
            .iter()
            .filter(|((k, _), s)| *k == library && s.missing_since.is_none())
            .map(|((_, rel), s)| (rel.clone(), s.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn missing(&self, library: LibraryKey) -> usize {
        self.lock()
            .songs
            .iter()
            .filter(|((k, _), s)| *k == library && s.missing_since.is_some())
            .count()
    }

    /// Everything in a batch except the files: artwork, gone paths, folders, errors.
    fn apply_rows(mem: &mut Mem, library: LibraryKey, batch: Batch) {
        for art in batch.artwork {
            mem.artwork.insert(art.id.clone(), art);
        }
        for (key, rel) in batch.gone {
            if let Some(song) = mem.songs.get_mut(&(key, rel))
                && song.missing_since.is_none()
            {
                song.missing_since = Some(Millis(1));
                mem.gone_marked += 1;
            }
        }
        for folder in batch.folders {
            mem.folders.insert(
                (library, folder.rel_path),
                FolderState {
                    mtime_ns: folder.mtime_ns,
                    listing_hash: folder.listing_hash,
                    unreadable: false,
                },
            );
        }
        for rel in batch.unreadable {
            mem.folders
                .entry((library, rel))
                .or_insert(FolderState {
                    mtime_ns: None,
                    listing_hash: None,
                    unreadable: true,
                })
                .unreadable = true;
        }
        mem.errors.extend(batch.errors);
        for q in batch.quarantine {
            mem.quarantine.insert((library, q.rel_path.clone()), q);
        }
    }

    fn with_count(mem: &Mem, mut library: Library) -> Library {
        library.song_count = mem
            .songs
            .iter()
            .filter(|((k, _), s)| *k == library.key && s.missing_since.is_none())
            .count() as u64;
        library
    }
}

fn under(rel: &[u8], folder: &[u8]) -> bool {
    folder.is_empty()
        || rel == folder
        || (rel.len() > folder.len() && rel.starts_with(folder) && rel[folder.len()] == b'/')
}

impl Catalog for MemCatalog {
    fn libraries(&self) -> Result<Vec<Library>, DbError> {
        let mem = self.lock();
        Ok(mem
            .libraries
            .iter()
            .cloned()
            .map(|l| Self::with_count(&mem, l))
            .collect())
    }

    fn library(&self, id: &LibraryId) -> Result<Option<Library>, DbError> {
        let mem = self.lock();
        Ok(mem
            .libraries
            .iter()
            .find(|l| l.id == *id)
            .cloned()
            .map(|l| Self::with_count(&mem, l)))
    }

    fn root_health(&self, library: LibraryKey) -> Result<Option<RootHealth>, DbError> {
        Ok(self.lock().root_health.get(&library).copied())
    }

    fn set_available(
        &self,
        library: LibraryKey,
        available: bool,
        _: Option<&str>,
    ) -> Result<bool, DbError> {
        let mut mem = self.lock();
        mem.availability.push((library, available));
        let Some(lib) = mem.libraries.iter_mut().find(|l| l.key == library) else {
            return Err(DbError::NotFound);
        };
        let changed = lib.available != available;
        lib.available = available;
        Ok(changed)
    }

    fn folder_state(
        &self,
        library: LibraryKey,
        rel: &[u8],
    ) -> Result<Option<FolderState>, DbError> {
        Ok(self.lock().folders.get(&(library, rel.to_vec())).copied())
    }

    fn known_file(&self, library: LibraryKey, rel: &[u8]) -> Result<Option<KnownFile>, DbError> {
        Ok(self.song(library, rel).map(|s| KnownFile {
            song: s.id,
            size: s.size,
            mtime_ns: s.mtime_ns,
            content_version: s.content_version,
            digest: s.digest,
            missing_since: s.missing_since,
        }))
    }

    fn is_quarantined(
        &self,
        library: LibraryKey,
        rel: &[u8],
        size: u64,
        mtime_ns: i64,
    ) -> Result<bool, DbError> {
        Ok(self
            .lock()
            .quarantine
            .get(&(library, rel.to_vec()))
            .is_some_and(|q| q.file_size == size && q.file_mtime_ns == mtime_ns))
    }

    fn present_with_digest(&self, digest: &AudioDigest) -> Result<Option<PresentSong>, DbError> {
        let mem = self.lock();
        let mut found: Vec<_> = mem
            .songs
            .iter()
            .filter(|(_, s)| s.digest == *digest && s.missing_since.is_none())
            .collect();
        found.sort_by(|a, b| a.0.cmp(b.0));
        Ok(found.first().and_then(|((key, rel), song)| {
            let root = &mem.libraries.iter().find(|l| l.key == *key)?.root;
            Some(PresentSong {
                song: song.id.clone(),
                library: *key,
                rel: rel.clone(),
                path: crate::paths::join_rel(root, rel),
            })
        }))
    }

    fn start_run(
        &self,
        library: LibraryKey,
        kind: ScanRunKind,
        trigger: ScanTrigger,
    ) -> Result<ScanRun, DbError> {
        let mut mem = self.lock();
        let lib = mem
            .libraries
            .iter()
            .find(|l| l.key == library)
            .ok_or(DbError::NotFound)?
            .id
            .clone();
        let run = ScanRun {
            id: random_id(),
            library: lib,
            kind,
            trigger,
            state: ScanState::Running,
            started_at: Millis(0),
            finished_at: None,
            counts: ScanCounts::default(),
            message: None,
        };
        mem.runs.push((library, run.clone()));
        Ok(run)
    }

    fn commit(
        &self,
        library: LibraryKey,
        _: &ScanRunId,
        origin: SongOrigin,
        mut batch: Batch,
    ) -> Result<Vec<Resolution>, DbError> {
        let mut mem = self.lock();
        if mem.fail_commits {
            return Err(DbError::Full);
        }
        mem.largest_batch = mem.largest_batch.max(batch.files.len());
        let lib_id = mem
            .libraries
            .iter()
            .find(|l| l.key == library)
            .ok_or(DbError::NotFound)?
            .id
            .clone();
        let files = std::mem::take(&mut batch.files);
        let artists = std::mem::take(&mut batch.artist_artwork);
        Self::apply_rows(&mut mem, library, batch);
        let mut out = Vec::new();
        for file in files {
            let mut song = MemSong {
                id: SongId::derive(&file.digest),
                size: file.size,
                mtime_ns: file.mtime_ns,
                digest: file.digest,
                missing_since: None,
                content_version: 1,
                title: file.tags.title.clone(),
                artist: file.tags.artist.clone(),
                origin,
                tag_problem: file.tag_problem.clone(),
                has_artwork: file.artwork.is_some(),
                lyrics: file.lyrics.len(),
            };
            let key = (library, file.rel_path.clone());
            if let Some(existing) = mem.songs.get(&key).cloned() {
                let unchanged = existing.size == song.size
                    && existing.mtime_ns == song.mtime_ns
                    && existing.digest == song.digest
                    && existing.missing_since.is_none();
                song.id = existing.id.clone();
                song.origin = existing.origin;
                song.content_version =
                    existing.content_version + i64::from(existing.digest != song.digest);
                mem.songs.insert(key, song.clone());
                out.push(if unchanged {
                    Resolution::Unchanged(song.id)
                } else {
                    Resolution::Updated(song.id)
                });
                continue;
            }
            let name = crate::paths::file_name(&file.rel_path).to_vec();
            let mut moved: Vec<(LibraryKey, Vec<u8>)> = mem
                .songs
                .iter()
                .filter(|(_, s)| s.digest == song.digest && s.missing_since.is_some())
                .map(|(k, _)| k.clone())
                .collect();
            moved.sort_by_key(|(_, rel)| crate::paths::file_name(rel) != name.as_slice());
            if let Some(from) = moved.first() {
                if let Some(old) = mem.songs.remove(from) {
                    song.id = old.id;
                    song.origin = old.origin;
                    song.content_version = old.content_version;
                }
                let id = song.id.clone();
                mem.songs.insert(key, song);
                out.push(Resolution::Moved(id));
                continue;
            }
            let taken = mem
                .songs
                .values()
                .any(|s| s.id == song.id && s.missing_since.is_none());
            if taken {
                song.id = SongId::derive_disambiguated(&song.digest, &lib_id, &file.rel_path);
            }
            let id = song.id.clone();
            mem.songs.insert(key, song);
            out.push(Resolution::Added(id));
        }
        for (name, art) in artists {
            mem.artist_artwork.insert(name, art);
        }
        Ok(out)
    }

    fn mark_missing(
        &self,
        library: LibraryKey,
        _: &ScanRunId,
        seen: &SeenSet,
        unreadable: &[Vec<u8>],
    ) -> Result<FinishOutcome, DbError> {
        let now = self.now();
        let mut mem = self.lock();
        mem.mark_missing_calls += 1;
        let mut marked = 0;
        for ((key, rel), song) in &mut mem.songs {
            if *key != library || song.missing_since.is_some() || seen.contains(rel) {
                continue;
            }
            if unreadable.iter().any(|u| under(rel, u)) {
                continue;
            }
            song.missing_since = Some(now);
            marked += 1;
        }
        Ok(FinishOutcome {
            marked_missing: marked,
            ..FinishOutcome::default()
        })
    }

    fn finish_folder(
        &self,
        library: LibraryKey,
        _: &ScanRunId,
        folder: &[u8],
        seen: &SeenSet,
    ) -> Result<FinishOutcome, DbError> {
        let now = self.now();
        let mut mem = self.lock();
        mem.folder_finishes.push(folder.to_vec());
        let mut marked = 0;
        for ((key, rel), song) in &mut mem.songs {
            if *key == library
                && song.missing_since.is_none()
                && !seen.contains(rel)
                && under(rel, folder)
            {
                song.missing_since = Some(now);
                marked += 1;
            }
        }
        Ok(FinishOutcome {
            marked_missing: marked,
            ..FinishOutcome::default()
        })
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
        let mut mem = self.lock();
        if let Some(root) = root {
            mem.root_health.insert(library, root);
        }
        let entry = mem
            .runs
            .iter_mut()
            .find(|(_, r)| r.id == *run)
            .ok_or(DbError::NotFound)?;
        entry.1.state = state;
        entry.1.counts = *counts;
        entry.1.message = message.map(str::to_owned);
        entry.1.finished_at = Some(Millis(1));
        Ok(entry.1.clone())
    }

    fn recent_runs(&self, library: LibraryKey, limit: u32) -> Result<Vec<ScanRun>, DbError> {
        Ok(self
            .lock()
            .runs
            .iter()
            .rev()
            .filter(|(k, _)| *k == library)
            .take(limit as usize)
            .map(|(_, r)| r.clone())
            .collect())
    }

    fn import(&self, id: &ImportId) -> Result<Option<ImportRow>, DbError> {
        Ok(self.lock().imports.iter().find(|r| r.id == *id).cloned())
    }

    fn create_import(&self, new: &NewImport) -> Result<ImportRow, DbError> {
        let row = ImportRow {
            id: random_id(),
            user: new.user,
            source: new.source,
            original_name: new.original_name.clone(),
            staged_path: new.staged_path.clone(),
            expected_bytes: new.expected_bytes,
            received_bytes: 0,
            state: ImportState::Staged,
            target_library: new.target_library,
            song: None,
            error: None,
            created_at: Millis(0),
            updated_at: Millis(0),
        };
        self.lock().imports.push(row.clone());
        Ok(row)
    }

    fn finish_import(
        &self,
        id: &ImportId,
        state: ImportState,
        song: Option<&SongId>,
        error: Option<&str>,
    ) -> Result<ImportRow, DbError> {
        let mut mem = self.lock();
        let row = mem
            .imports
            .iter_mut()
            .find(|r| r.id == *id)
            .ok_or(DbError::NotFound)?;
        row.state = state;
        row.song = song.cloned();
        row.error = error.map(str::to_owned);
        Ok(row.clone())
    }

    fn uploads_library(&self) -> Result<Option<Library>, DbError> {
        let key = self.lock().uploads_library;
        Ok(self.libraries()?.into_iter().find(|l| Some(l.key) == key))
    }
}

/// What a [`TestReader`] does for one file name.
#[derive(Clone)]
pub enum Behaviour {
    Panic,
    /// Blocks until the sender is dropped or sends.
    Hang(Arc<Mutex<std::sync::mpsc::Receiver<()>>>),
    /// Appends a byte to the file while "reading" it.
    Grow,
    /// Runs a callback, then reads normally.
    Before(Arc<dyn Fn() + Send + Sync>),
    /// A hung network share: the read blocks like [`Behaviour::Hang`] and so does a stat of
    /// the file afterwards.
    Stall(Arc<Mutex<std::sync::mpsc::Receiver<()>>>),
}

/// lofty underneath, counting reads and misbehaving for chosen file names.
#[derive(Default)]
pub struct TestReader {
    pub reads: AtomicUsize,
    pub behaviours: Mutex<HashMap<String, Behaviour>>,
}

impl std::fmt::Debug for TestReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestReader").finish_non_exhaustive()
    }
}

impl TestReader {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.reads.store(0, Ordering::SeqCst);
    }

    pub fn on(&self, file_name: &str, behaviour: Behaviour) {
        self.behaviours
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(file_name.to_owned(), behaviour);
    }
}

impl FileReader for TestReader {
    fn read(&self, path: &Path, hints: &ReadHints) -> Result<ParsedFile, ReadError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let behaviour = self
            .behaviours
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&name)
            .cloned();
        match behaviour {
            Some(Behaviour::Panic) => panic!("injected parser panic"),
            Some(Behaviour::Hang(rx) | Behaviour::Stall(rx)) => {
                let _ = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
            }
            Some(Behaviour::Grow) => {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
                f.write_all(b"\0")?;
                f.sync_all()?;
            }
            Some(Behaviour::Before(f)) => f(),
            None => {}
        }
        LoftyReader.read(path, hints)
    }

    fn digest(&self, path: &Path, container: Container) -> io::Result<(AudioDigest, DigestMode)> {
        LoftyReader.digest(path, container)
    }

    fn probe(&self, path: &Path) -> io::Result<()> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let behaviour = self
            .behaviours
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&name)
            .cloned();
        if let Some(Behaviour::Stall(rx)) = behaviour {
            let _ = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::fs::metadata(path).map(|_| ())
    }
}

/// The committed audio fixtures.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A small valid 16-bit mono PCM WAV whose samples depend on `seed`.
pub fn wav_bytes(seed: u32, samples: usize) -> Vec<u8> {
    let data_len = (samples * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    for _ in 0..samples {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        out.extend_from_slice(&((x >> 16) as i16).to_le_bytes());
    }
    out
}

/// Sets a file's mtime well in the past, so `scanner.settle` does not defer it.
pub fn age(path: &Path) -> io::Result<()> {
    let past = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(past)
}

/// Copies a fixture into place and ages it.
pub fn put_fixture(root: &Path, rel: &str, fixture_name: &str) -> io::Result<PathBuf> {
    let dest = root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(fixture(fixture_name), &dest)?;
    age(&dest)?;
    Ok(dest)
}

/// Writes a generated WAV into place and ages it.
pub fn put_wav(root: &Path, rel: &str, seed: u32) -> io::Result<PathBuf> {
    let dest = root.join(rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, wav_bytes(seed, 800))?;
    age(&dest)?;
    Ok(dest)
}
