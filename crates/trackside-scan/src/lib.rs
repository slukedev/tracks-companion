//! The scanner: walking libraries, reading tags with lofty, audio digests, folder listing
//! hashes, watching, the availability monitor, and importing single files and uploads. It
//! writes only through `trackside_core::ingest` and the scan repositories.
//!
//! A run streams the library: a walker thread lists one folder at a time and skips folders
//! whose mtime and listing hash are unchanged (quick scans), reader threads read tags and
//! digests with a per-file deadline, and the run's own thread commits results in transactions of
//! at most 200 files or 250 ms. Songs are marked missing only after a complete walk that passed
//! the empty-library guard, so an unmounted share never empties the library.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use trackside_core::config::{ScannerConfig, UploadsConfig};
use trackside_core::control::{
    ImportOutcome, LibraryScanStatus, ScanControl, ScanControlError, ScanKind, ScanTrigger,
    SongOrigin,
};
use trackside_core::ingest::{ScannedLyrics, ScannedProperties, ScannedTags};
use trackside_core::records::Container;
use trackside_core::{
    AudioDigest, BoxFuture, Clock, Db, DbError, EventBus, ImportId, LibraryId, ScanRunId,
};
use trackside_media::ArtworkStore;

pub mod artwork;
pub mod catalog;
pub mod digest;
mod import;
pub mod lyrics;
mod monitor;
pub mod paths;
pub mod reader;
pub mod run;
mod scanner;
pub mod tags;
mod watch;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

pub use import::safe_rel;
pub use scanner::ScannerOptions;

/// Why the scanner could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScanError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

/// Why a file could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReadError {
    #[error("not an audio file the scanner reads")]
    Unsupported,
    #[error("tags could not be parsed: {0}")]
    Tags(String),
    #[error("no readable audio properties")]
    NoAudio,
    #[error("the tag reader panicked")]
    Panicked,
    #[error("timed out")]
    Timeout,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
}

impl From<lofty::error::FileParseError> for ReadError {
    fn from(e: lofty::error::FileParseError) -> Self {
        Self::Tags(e.to_string())
    }
}

/// What kind of picture a tag holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PictureKind {
    FrontCover,
    Other,
}

/// An embedded picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub kind: PictureKind,
    pub mime: Option<String>,
    pub bytes: Vec<u8>,
}

/// Everything read from one file except its digest.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedFile {
    pub tags: ScannedTags,
    pub properties: ScannedProperties,
    pub pictures: Vec<Picture>,
    pub lyrics: Vec<ScannedLyrics>,
    /// Why tags fell back to the file name.
    pub tag_problem: Option<String>,
}

/// Whether the digest covers the located audio payload or, as a fallback, the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DigestMode {
    Payload,
    WholeFile,
}

/// One directory entry, for the listing hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntrySummary {
    pub name: Vec<u8>,
    pub size: u64,
    pub mtime_ns: i64,
    pub is_dir: bool,
}

/// Reads tags (strict, then relaxed, then the file name), properties, pictures and lyrics,
/// including a sidecar `.lrc` beside the file. A panic inside the tag parser is reported as
/// [`ReadError::Panicked`].
pub fn read_file(path: &Path) -> Result<ParsedFile, ReadError> {
    let hints = tags::ReadHints::default();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tags::read_file_with(path, &hints)
    }))
    .unwrap_or(Err(ReadError::Panicked))
}

/// The audio digest of architecture section 6.
pub fn audio_digest(
    path: &Path,
    container: Container,
) -> std::io::Result<(AudioDigest, DigestMode)> {
    digest::audio_digest(path, container)
}

/// BLAKE3 over the entries sorted by name: each name's length and bytes, size, mtime and
/// whether it is a directory. A quick scan skips a folder whose hash and mtime are unchanged.
pub fn listing_hash(entries: &[DirEntrySummary]) -> [u8; 32] {
    let mut sorted: Vec<&DirEntrySummary> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"trackside listing v1");
    for entry in sorted {
        hasher.update(&(entry.name.len() as u64).to_le_bytes());
        hasher.update(&entry.name);
        hasher.update(&entry.size.to_le_bytes());
        hasher.update(&entry.mtime_ns.to_le_bytes());
        hasher.update(&[u8::from(entry.is_dir)]);
    }
    *hasher.finalize().as_bytes()
}

/// The running scanner.
pub struct Scanner {
    inner: Arc<scanner::Inner>,
}

impl std::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scanner")
            .field("threads", &self.inner.settings.threads)
            .finish_non_exhaustive()
    }
}

impl Scanner {
    /// Starts the availability monitor, watcher, schedules and reader threads. Call it inside
    /// a tokio runtime for watching, schedules and the inbox; without one, scans still run on
    /// request.
    pub fn start(
        db: Db,
        artwork: ArtworkStore,
        config: &ScannerConfig,
        uploads: &UploadsConfig,
        events: EventBus,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
    ) -> Result<Arc<Self>, ScanError> {
        let services = run::Services {
            catalog: Arc::new(catalog::DbCatalog::new(db)),
            reader: Arc::new(reader::LoftyReader),
            artwork: Arc::new(artwork),
            clock,
        };
        Self::with_services(
            services,
            config,
            uploads,
            events,
            shutdown,
            &ScannerOptions::default(),
        )
    }

    /// [`Scanner::start`] with every part supplied, for tests and tools.
    pub fn with_services(
        services: run::Services,
        config: &ScannerConfig,
        uploads: &UploadsConfig,
        events: EventBus,
        shutdown: CancellationToken,
        options: &ScannerOptions,
    ) -> Result<Arc<Self>, ScanError> {
        let inner = scanner::Inner::launch(services, config, uploads, events, shutdown, options)?;
        Ok(Arc::new(Self { inner }))
    }

    /// The inbox folder, when configured.
    pub fn inbox_dir(&self) -> Option<&PathBuf> {
        self.inner.uploads.inbox_dir.as_ref()
    }

    /// Probes every library now instead of waiting for the next round, and returns once done.
    pub fn probe_now(&self) {
        monitor::probe_all(&self.inner, false);
    }

    /// Imports every settled file in the inbox folder now.
    pub async fn poll_inbox(&self) {
        import::poll_inbox(Arc::clone(&self.inner)).await;
    }

    /// Queues a scan of one folder of a library (relative path) and everything under it; its
    /// missing pass covers only that folder. A folder that no longer exists is scanned from its
    /// nearest existing parent.
    pub fn request_folder_scan(
        &self,
        library: &LibraryId,
        folder: &Path,
    ) -> Result<ScanRunId, ScanControlError> {
        let rel = if folder.as_os_str().is_empty() {
            Vec::new()
        } else {
            import::safe_rel(folder)?
        };
        scanner::blocking(|| self.inner.request_folder_scan(library, rel))
    }

    /// Waits until no scan is queued or running. For tests and the CLI's `scan --wait`.
    pub fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        self.inner.wait_idle(timeout)
    }
}

impl ScanControl for Scanner {
    fn request_scan(
        &self,
        library: &LibraryId,
        kind: ScanKind,
        trigger: ScanTrigger,
        force_empty: bool,
    ) -> Result<ScanRunId, ScanControlError> {
        scanner::blocking(|| self.inner.request_scan(library, kind, trigger, force_empty))
    }

    fn request_folder_scan(
        &self,
        library: &LibraryId,
        folder: &Path,
    ) -> Result<ScanRunId, ScanControlError> {
        Scanner::request_folder_scan(self, library, folder)
    }

    fn import_file<'a>(
        &'a self,
        library: &'a LibraryId,
        rel_path: &'a Path,
        origin: SongOrigin,
    ) -> BoxFuture<'a, Result<ImportOutcome, ScanControlError>> {
        let inner = Arc::clone(&self.inner);
        let library = library.clone();
        let rel_path = rel_path.to_path_buf();
        Box::pin(async move { import::import_file(inner, library, rel_path, origin).await })
    }

    fn process_upload<'a>(
        &'a self,
        import: &'a ImportId,
    ) -> BoxFuture<'a, Result<(), ScanControlError>> {
        let inner = Arc::clone(&self.inner);
        let import = import.clone();
        Box::pin(async move { import::process_upload(inner, import).await })
    }

    fn status(&self) -> Vec<LibraryScanStatus> {
        self.inner.status()
    }

    fn is_available(&self, library: &LibraryId) -> bool {
        self.inner.is_available(library)
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    fn entry(name: &str, size: u64) -> DirEntrySummary {
        DirEntrySummary {
            name: name.as_bytes().to_vec(),
            size,
            mtime_ns: 1,
            is_dir: false,
        }
    }

    #[test]
    fn listing_hash_ignores_order_and_sees_changes() {
        let a = listing_hash(&[entry("01.flac", 10), entry("02.flac", 20)]);
        let b = listing_hash(&[entry("02.flac", 20), entry("01.flac", 10)]);
        let c = listing_hash(&[entry("01.flac", 10), entry("02.flac", 21)]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
