//! Reading one file safely: tags and digest on a helper thread with a deadline and
//! `catch_unwind`, a stat before and after so a file being written is deferred, and the result
//! assembled into a [`ScannedFile`].

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

use trackside_core::ingest::ScannedFile;
use trackside_core::records::Container;
use trackside_core::{ArtworkId, AudioDigest, FolderId};

use crate::artwork::{ArtworkCache, ArtworkSink};
use crate::tags::ReadHints;
use crate::{DigestMode, ParsedFile, ReadError};

/// Reads tags and digests. The real one uses lofty; tests count calls or inject failures.
pub trait FileReader: Send + Sync + 'static {
    fn read(&self, path: &Path, hints: &ReadHints) -> Result<ParsedFile, ReadError>;
    fn digest(&self, path: &Path, container: Container) -> io::Result<(AudioDigest, DigestMode)>;
}

/// lofty for tags, the payload locators for digests.
#[derive(Debug, Default, Clone, Copy)]
pub struct LoftyReader;

impl FileReader for LoftyReader {
    fn read(&self, path: &Path, hints: &ReadHints) -> Result<ParsedFile, ReadError> {
        crate::tags::read_file_with(path, hints)
    }

    fn digest(&self, path: &Path, container: Container) -> io::Result<(AudioDigest, DigestMode)> {
        crate::digest::audio_digest(path, container)
    }
}

/// What the scanner knows about a file before reading it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    pub mtime_ns: i64,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
}

impl FileStat {
    pub fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            mtime_ns: mtime_ns(meta),
            dev: dev_of(meta),
            ino: ino_of(meta),
        }
    }

    /// Whether the file is the same as at `other`, by size and modification time.
    pub fn same_content(&self, other: &Self) -> bool {
        self.size == other.size && self.mtime_ns == other.mtime_ns
    }
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

#[cfg(unix)]
pub fn dev_of(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.dev())
}

#[cfg(not(unix))]
pub fn dev_of(_: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
pub fn ino_of(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
pub fn ino_of(_: &std::fs::Metadata) -> Option<u64> {
    None
}

/// One file to read.
#[derive(Clone, Debug)]
pub struct Job {
    pub path: PathBuf,
    pub rel: Vec<u8>,
    pub folder: FolderId,
    pub stat: FileStat,
    pub hints: ReadHints,
    pub sidecar_art: Option<PathBuf>,
    /// An `artist.*` picture in the file's folder or the one above.
    pub artist_art: Option<PathBuf>,
    /// Whether the index already has a song at this path.
    pub known: bool,
}

/// A file that was read, with the artist picture found beside it.
#[derive(Debug)]
pub struct ReadFile {
    pub file: ScannedFile,
    pub artist_art: Option<ArtworkId>,
}

/// What reading a file came to.
#[derive(Debug)]
pub enum Outcome {
    Read(Box<ReadFile>),
    /// The file changed while it was read; try again later.
    Deferred,
    /// Recorded in the run's errors; the song, if any, is kept.
    Failed(String),
    /// The reader hung or crashed on this file; skip it until it changes.
    Quarantine(String),
}

type Task = (PathBuf, ReadHints);
type TaskResult = std::thread::Result<Result<(ParsedFile, AudioDigest, DigestMode), ReadError>>;

/// A thread that runs reads, so a hung read can be abandoned.
struct Helper {
    tasks: SyncSender<Task>,
    results: Receiver<TaskResult>,
}

impl Helper {
    fn spawn(reader: Arc<dyn FileReader>) -> io::Result<Self> {
        let (task_tx, task_rx) = mpsc::sync_channel::<Task>(1);
        let (result_tx, result_rx) = mpsc::sync_channel::<TaskResult>(1);
        std::thread::Builder::new()
            .name("trackside-tag-read".into())
            .spawn(move || {
                while let Ok((path, hints)) = task_rx.recv() {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        let parsed = reader.read(&path, &hints)?;
                        let (digest, mode) = reader.digest(&path, parsed.properties.container)?;
                        Ok((parsed, digest, mode))
                    }));
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tasks: task_tx,
            results: result_rx,
        })
    }
}

/// Reads files one at a time with a deadline per file.
pub struct SafeReader {
    reader: Arc<dyn FileReader>,
    timeout: Duration,
    helper: Option<Helper>,
}

impl std::fmt::Debug for SafeReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafeReader")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// The result of [`SafeReader::read`].
#[derive(Debug)]
pub enum Guarded {
    Done(Box<Result<(ParsedFile, AudioDigest, DigestMode), ReadError>>),
    Panicked,
    TimedOut,
}

impl SafeReader {
    pub fn new(reader: Arc<dyn FileReader>, timeout: Duration) -> Self {
        Self {
            reader,
            timeout,
            helper: None,
        }
    }

    /// Reads tags and digest of `path`. A read that misses the deadline leaves its helper thread
    /// behind (it cannot be interrupted) and a fresh helper takes over.
    pub fn read(&mut self, path: &Path, hints: &ReadHints) -> Guarded {
        let helper = match self.helper.take() {
            Some(h) => h,
            None => match Helper::spawn(Arc::clone(&self.reader)) {
                Ok(h) => h,
                Err(e) => return Guarded::Done(Box::new(Err(ReadError::Io(e)))),
            },
        };
        if helper
            .tasks
            .send((path.to_path_buf(), hints.clone()))
            .is_err()
        {
            return Guarded::Panicked;
        }
        match helper.results.recv_timeout(self.timeout) {
            Ok(Ok(result)) => {
                self.helper = Some(helper);
                Guarded::Done(Box::new(result))
            }
            Ok(Err(_)) => {
                self.helper = Some(helper);
                Guarded::Panicked
            }
            Err(RecvTimeoutError::Timeout) => Guarded::TimedOut,
            Err(RecvTimeoutError::Disconnected) => Guarded::Panicked,
        }
    }
}

/// Reads one job: stat, guarded read, stat again, artwork.
pub fn process(
    job: &Job,
    reader: &mut SafeReader,
    artwork: &dyn ArtworkSink,
    art_cache: &ArtworkCache,
) -> Outcome {
    let (parsed, digest, mode) = match reader.read(&job.path, &job.hints) {
        Guarded::Done(done) => match *done {
            Ok(done) => done,
            Err(ReadError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                return Outcome::Deferred;
            }
            Err(e) => return Outcome::Failed(e.to_string()),
        },
        Guarded::Panicked => return Outcome::Quarantine(String::from("the tag reader panicked")),
        Guarded::TimedOut => {
            return Outcome::Quarantine(format!(
                "the tag reader took longer than {} s",
                reader.timeout.as_secs()
            ));
        }
    };
    let after = match std::fs::metadata(&job.path) {
        Ok(meta) => FileStat::of(&meta),
        Err(_) => return Outcome::Deferred,
    };
    if !after.same_content(&job.stat) {
        return Outcome::Deferred;
    }
    let art = art_cache.resolve(
        artwork,
        job.sidecar_art.as_deref(),
        &job.path,
        &parsed.pictures,
    );
    let tag_problem = match (parsed.tag_problem, mode) {
        (Some(p), DigestMode::WholeFile) => Some(format!("{p}, digest_whole_file")),
        (None, DigestMode::WholeFile) => Some(String::from("digest_whole_file")),
        (p, DigestMode::Payload) => p,
    };
    let artist_art = job
        .artist_art
        .as_deref()
        .and_then(|path| art_cache.file(artwork, path));
    let file = ScannedFile {
        rel_path: job.rel.clone(),
        rel_path_display: crate::paths::display(&job.rel),
        folder: job.folder.clone(),
        size: job.stat.size,
        mtime_ns: job.stat.mtime_ns,
        dev: job.stat.dev,
        ino: job.stat.ino,
        digest,
        tags: parsed.tags,
        properties: parsed.properties,
        artwork: art,
        lyrics: parsed.lyrics,
        tag_problem,
    };
    Outcome::Read(Box::new(ReadFile { file, artist_art }))
}
