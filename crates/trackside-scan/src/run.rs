//! One scan run of one library: a walker thread streams folders and files, reader threads read
//! tags and digests, and this thread batches results into write transactions. Memory stays
//! bounded: channels are bounded, one folder listing is held at a time, and results are
//! committed every 200 files or 250 ms.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use tokio_util::sync::CancellationToken;
use trackside_core::config::ScannerConfig;
use trackside_core::control::ScanKind;
use trackside_core::ingest::{Resolution, ScannedFile, SeenSet};
use trackside_core::records::{Library, ScanCounts, ScanState};
use trackside_core::repo::folders::FolderUpsert;
use trackside_core::repo::scan_runs::{QuarantinedFile, ScanErrorRow};
use trackside_core::{ArtworkId, Clock, DbError, FolderId, LibraryId, LibraryKey, ScanRunId};

use crate::artwork::{ArtworkCache, ArtworkSink, pick_artist, sidecar_in};
use crate::catalog::{Batch, Catalog, RootHealth};
use crate::listing_hash;
use crate::paths::{
    ExtensionSet, child_rel, display, has_ignore_marker, is_hidden, os_bytes, parent_rel,
};
use crate::reader::{FileReader, FileStat, Job, Outcome, SafeReader, dev_of, ino_of, mtime_ns};
use crate::tags::ReadHints;
use crate::{DirEntrySummary, SongOrigin};

/// Files per write transaction.
pub const BATCH_FILES: usize = 200;
/// Longest a result waits before it is committed.
pub const BATCH_WAIT: Duration = Duration::from_millis(250);
/// Capacity of the walker-to-reader channel.
pub const JOB_QUEUE: usize = 1024;
/// How long a stopped run waits for the walker before leaving it behind. A walker stuck in a
/// `stat` on a hung mount cannot be interrupted and would otherwise block every later scan.
pub const WALKER_GRACE: Duration = Duration::from_secs(10);

/// The scanner settings a run uses.
#[derive(Clone, Debug)]
pub struct RunSettings {
    pub threads: usize,
    pub follow_symlinks: bool,
    pub empty_guard: f64,
    pub file_timeout: Duration,
    pub ignore: Vec<String>,
    pub extensions: ExtensionSet,
    /// Folders the walker treats as unlistable, because tests run as root, where permissions
    /// cannot make a folder unreadable.
    #[cfg(test)]
    pub deny: Vec<Vec<u8>>,
}

impl RunSettings {
    pub fn from_config(config: &ScannerConfig) -> Self {
        Self {
            threads: config.threads.max(1),
            follow_symlinks: config.follow_symlinks,
            empty_guard: config.empty_guard,
            file_timeout: config.file_timeout,
            ignore: config.ignore.clone(),
            extensions: ExtensionSet::new(&config.extensions),
            #[cfg(test)]
            deny: Vec::new(),
        }
    }
}

/// The parts a run talks to.
#[derive(Clone)]
pub struct Services {
    pub catalog: Arc<dyn Catalog>,
    pub reader: Arc<dyn FileReader>,
    pub artwork: Arc<dyn ArtworkSink>,
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services").finish_non_exhaustive()
    }
}

/// What to scan.
pub struct RunInput<'a> {
    pub library: &'a Library,
    pub run: &'a ScanRunId,
    pub kind: ScanKind,
    pub force_empty: bool,
    /// Files modified more recently than this are deferred (`scanner.settle`, shorter for
    /// watch-triggered runs, whose folders were already quiet for the debounce).
    pub settle: Duration,
    /// Scan only this folder and everything under it (a watch event), with the missing pass
    /// limited to it. `None` scans the whole library.
    pub scope: Option<Vec<u8>>,
    pub cancel: &'a CancellationToken,
    pub progress: &'a (dyn Fn(&ScanCounts) + Sync),
}

impl std::fmt::Debug for RunInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunInput")
            .field("library", &self.library.id)
            .field("run", self.run)
            .field("kind", &self.kind)
            .field("force_empty", &self.force_empty)
            .finish_non_exhaustive()
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    pub state: ScanState,
    pub counts: ScanCounts,
    pub message: Option<String>,
    /// Files that changed while being read or were modified too recently.
    pub deferred: Vec<Vec<u8>>,
    /// Set when the run found the library unavailable; the reason is shown to admins.
    pub unavailable: Option<String>,
    /// Whether the root itself could not be read (unmounted, gone, hung).
    pub root_gone: bool,
}

/// A folder the walker read completely.
#[derive(Debug)]
struct WalkedFolder {
    upsert: FolderUpsert,
}

#[derive(Debug)]
struct Result1 {
    rel: Vec<u8>,
    known: bool,
    size: u64,
    mtime_ns: i64,
    outcome: Outcome,
}

#[derive(Debug)]
enum Msg {
    Folder(WalkedFolder),
    Seen(Vec<Vec<u8>>),
    Deferred(Vec<u8>),
    Error(ScanErrorRow),
    Unreadable(ScanErrorRow),
    Read(Box<Result1>),
    Done(WalkSummary),
}

#[derive(Debug, Default)]
struct WalkSummary {
    folders: u64,
    root_entries: u64,
    root_dev: Option<u64>,
    root_error: Option<String>,
    db_error: Option<String>,
    cancelled: bool,
}

/// Runs one scan to the end, or until cancelled or the library turns out to be unavailable.
/// Never marks songs missing unless the walk completed and the guards passed.
pub fn execute(
    services: &Services,
    settings: &RunSettings,
    input: &RunInput<'_>,
) -> Result<RunReport, DbError> {
    let library = input.library;
    let root = library.root.clone();
    let key = library.key;
    let (msg_tx, msg_rx) = mpsc::sync_channel::<Msg>(JOB_QUEUE);
    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(JOB_QUEUE);
    let job_rx = Arc::new(Mutex::new(job_rx));
    // Stops this run's threads: the caller's cancel, or the run itself when storage stalls.
    let stop = input.cancel.child_token();

    let walker = {
        let ctx = WalkCtx {
            root: root.clone(),
            library: key,
            library_id: library.id.clone(),
            library_name: library.name.clone(),
            kind: input.kind,
            scope: input.scope.clone().unwrap_or_default(),
            settle: input.settle,
            settings: settings.clone(),
            catalog: Arc::clone(&services.catalog),
            cancel: stop.clone(),
        };
        let tx = msg_tx.clone();
        std::thread::Builder::new()
            .name("trackside-walk".into())
            .spawn(move || {
                let summary = walk(&ctx, &tx, &job_tx);
                let _ = tx.send(Msg::Done(summary));
            })
            .map_err(DbError::Io)?
    };

    let art_cache = Arc::new(ArtworkCache::default());
    let mut readers = Vec::new();
    for _ in 0..settings.threads {
        readers.push(spawn_reader(
            services,
            settings.file_timeout,
            Arc::clone(&job_rx),
            msg_tx.clone(),
            Arc::clone(&art_cache),
            stop.clone(),
        )?);
    }
    drop(msg_tx);
    // Only the readers hold the queue now, so if they all stop (the coordinator failed) the
    // walker's next send fails instead of blocking forever on a full queue.
    drop(job_rx);

    let mut coordinator = Coordinator {
        services,
        input,
        key,
        guard_ratio: settings.empty_guard,
        art_cache: Arc::clone(&art_cache),
        artist_art: HashMap::new(),
        pending: Batch::default(),
        first_pending: None,
        last_progress: None,
        counts: ScanCounts::default(),
        seen: SeenSet::new(),
        unreadable: Vec::new(),
        deferred: Vec::new(),
        walked: HashMap::new(),
        dirty: HashSet::new(),
        upserted: HashSet::new(),
        summary: None,
        stop: stop.clone(),
        stalled: None,
    };
    let result = coordinator.drain(&msg_rx);
    if result.is_err() {
        stop.cancel();
    }
    drop(msg_rx);
    for reader in readers {
        let _ = reader.join();
    }
    join_walker(walker, &stop);
    result?;
    coordinator.finish(&root)
}

/// Starts one tag reader: it takes jobs from the shared queue until the queue closes or the
/// run stops, and sends each file's outcome to the coordinator.
fn spawn_reader(
    services: &Services,
    timeout: Duration,
    rx: Arc<Mutex<Receiver<Job>>>,
    tx: SyncSender<Msg>,
    cache: Arc<ArtworkCache>,
    cancel: CancellationToken,
) -> Result<std::thread::JoinHandle<()>, DbError> {
    let reader = Arc::clone(&services.reader);
    let artwork = Arc::clone(&services.artwork);
    std::thread::Builder::new()
        .name("trackside-read".into())
        .spawn(move || {
            let mut safe = SafeReader::new(reader, timeout).with_stop(cancel.clone());
            loop {
                // A timed wait, so a reader notices a stop even while the walker that feeds
                // it is stuck on a hung mount and never closes the queue.
                let job = {
                    let guard = rx.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.recv_timeout(Duration::from_millis(200))
                };
                let job = match job {
                    Ok(job) => job,
                    Err(RecvTimeoutError::Timeout) if cancel.is_cancelled() => break,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if cancel.is_cancelled() {
                    continue;
                }
                let outcome = crate::reader::process(&job, &mut safe, artwork.as_ref(), &cache);
                let result = Result1 {
                    rel: job.rel,
                    known: job.known,
                    size: job.stat.size,
                    mtime_ns: job.stat.mtime_ns,
                    outcome,
                };
                if tx.send(Msg::Read(Box::new(result))).is_err() {
                    break;
                }
            }
        })
        .map_err(DbError::Io)
}

/// Joins the walker, or after a stop gives it [`WALKER_GRACE`] and then leaves it: it ends on
/// its own once its blocked call returns, since every channel it sends on is closed by then.
fn join_walker(walker: std::thread::JoinHandle<()>, stop: &CancellationToken) {
    if stop.is_cancelled() {
        let deadline = Instant::now() + WALKER_GRACE;
        while !walker.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!("the folder walker is stuck on the library's storage; leaving it");
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = walker.join();
}

struct WalkCtx {
    root: PathBuf,
    library: LibraryKey,
    library_id: LibraryId,
    library_name: String,
    kind: ScanKind,
    scope: Vec<u8>,
    settle: Duration,
    settings: RunSettings,
    catalog: Arc<dyn Catalog>,
    cancel: CancellationToken,
}

struct AudioEntry {
    name: Vec<u8>,
    path: PathBuf,
    stat: FileStat,
}

fn walk(ctx: &WalkCtx, msgs: &SyncSender<Msg>, jobs: &SyncSender<Job>) -> WalkSummary {
    let mut summary = WalkSummary::default();
    match walk_inner(ctx, msgs, jobs, &mut summary) {
        Ok(()) => {}
        Err(WalkStop::Cancelled | WalkStop::Closed) => summary.cancelled = true,
        Err(WalkStop::Db(e)) => summary.db_error = Some(e.to_string()),
    }
    summary
}

enum WalkStop {
    Cancelled,
    Closed,
    Db(DbError),
}

impl From<DbError> for WalkStop {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

fn send<T>(tx: &SyncSender<T>, value: T) -> Result<(), WalkStop> {
    tx.send(value).map_err(|_| WalkStop::Closed)
}

fn walk_inner(
    ctx: &WalkCtx,
    msgs: &SyncSender<Msg>,
    jobs: &SyncSender<Job>,
    summary: &mut WalkSummary,
) -> Result<(), WalkStop> {
    let mut visited: HashSet<(u64, u64)> = HashSet::new();
    let mut files_seen: HashSet<(u64, u64)> = HashSet::new();
    let mut artist_pictures: HashMap<Vec<u8>, PathBuf> = HashMap::new();
    let mut stack: Vec<(Vec<u8>, PathBuf)> = vec![(
        ctx.scope.clone(),
        crate::paths::join_rel(&ctx.root, &ctx.scope),
    )];
    let now = SystemTime::now();
    summary.root_dev = std::fs::metadata(&ctx.root).ok().and_then(|m| dev_of(&m));

    while let Some((rel, abs)) = stack.pop() {
        if ctx.cancel.is_cancelled() {
            return Err(WalkStop::Cancelled);
        }
        let (dir_meta, mut names) = match open_folder(ctx, &rel, &abs, &mut visited, msgs)? {
            Opened::Listed(meta, names) => (meta, names),
            Opened::Skipped => continue,
            Opened::RootFailed(reason) => {
                summary.root_error = Some(reason);
                return Ok(());
            }
        };
        let is_root = rel.is_empty();
        if is_root {
            summary.root_dev = dev_of(&dir_meta);
        }
        summary.folders += 1;
        let raw: Vec<&[u8]> = names.iter().map(|(n, _)| n.as_slice()).collect();
        if has_ignore_marker(&raw, &ctx.settings.ignore) {
            continue;
        }
        names.retain(|(n, _)| !is_hidden(n));
        names.sort_by(|a, b| a.0.cmp(&b.0));
        if is_root {
            summary.root_entries = names.len() as u64;
        }

        let Classified {
            entries,
            audio,
            subdirs,
        } = classify(ctx, &rel, &names, msgs)?;
        stack.extend(subdirs.into_iter().rev());

        let hash = listing_hash(&entries);
        let mtime = mtime_ns(&dir_meta);
        let audio_rels: Vec<Vec<u8>> = audio.iter().map(|a| child_rel(&rel, &a.name)).collect();
        if !audio_rels.is_empty() {
            send(msgs, Msg::Seen(audio_rels.clone()))?;
        }

        if ctx.kind == ScanKind::Quick && folder_unchanged(ctx, &rel, mtime, hash)? {
            for a in &audio {
                if let (Some(dev), Some(ino)) = (a.stat.dev, a.stat.ino) {
                    files_seen.insert((dev, ino));
                }
            }
            continue;
        }

        let folder_name = if is_root {
            ctx.library_name.clone()
        } else {
            display(crate::paths::file_name(&rel))
        };
        send(
            msgs,
            Msg::Folder(WalkedFolder {
                upsert: FolderUpsert {
                    rel_path: rel.clone(),
                    name: folder_name,
                    mtime_ns: Some(mtime),
                    listing_hash: Some(hash),
                },
            }),
        )?;
        let listing: Vec<Vec<u8>> = names.iter().map(|(n, _)| n.clone()).collect();
        let artist_art = artist_picture(&mut artist_pictures, &rel, &abs, &listing);
        if !audio.is_empty() {
            let folder = FolderFiles {
                rel: &rel,
                abs: &abs,
                listing: &listing,
                artist_art,
            };
            queue_files(
                ctx,
                &folder,
                audio,
                audio_rels,
                &mut files_seen,
                now,
                msgs,
                jobs,
            )?;
        }
    }
    Ok(())
}

/// Whether a folder's mtime and listing hash match what the index stored.
fn folder_unchanged(
    ctx: &WalkCtx,
    rel: &[u8],
    mtime: i64,
    hash: [u8; 32],
) -> Result<bool, WalkStop> {
    Ok(ctx
        .catalog
        .folder_state(ctx.library, rel)?
        .is_some_and(|state| {
            state.mtime_ns == Some(mtime) && state.listing_hash == Some(hash) && !state.unreadable
        }))
}

/// The `artist.*` picture for files in `rel`: its own, else its parent's. Folders with one are
/// remembered for their subfolders, which the walk visits later.
fn artist_picture(
    known: &mut HashMap<Vec<u8>, PathBuf>,
    rel: &[u8],
    abs: &Path,
    listing: &[Vec<u8>],
) -> Option<PathBuf> {
    if let Some(name) = pick_artist(listing) {
        known.insert(rel.to_vec(), abs.join(crate::paths::os_from_bytes(name)));
    }
    known
        .get(rel)
        .or_else(|| {
            (!rel.is_empty())
                .then(|| known.get(parent_rel(rel)))
                .flatten()
        })
        .cloned()
}

enum Opened {
    Listed(std::fs::Metadata, Vec<(Vec<u8>, std::fs::DirEntry)>),
    Skipped,
    RootFailed(String),
}

/// Stats and lists one folder. A folder that cannot be read is reported unreadable; a folder
/// already visited (a symlink loop) is reported and skipped.
fn open_folder(
    ctx: &WalkCtx,
    rel: &[u8],
    abs: &Path,
    visited: &mut HashSet<(u64, u64)>,
    msgs: &SyncSender<Msg>,
) -> Result<Opened, WalkStop> {
    let is_root = rel.is_empty();
    let unreadable = |e: &dyn std::fmt::Display, what: &str| -> Result<Opened, WalkStop> {
        if is_root {
            return Ok(Opened::RootFailed(format!(
                "the library root cannot be {what}: {e}"
            )));
        }
        send(msgs, Msg::Unreadable(error_row(rel, &e.to_string())))?;
        Ok(Opened::Skipped)
    };
    let dir_meta = match std::fs::metadata(abs) {
        Ok(m) if m.is_dir() => m,
        Ok(_) if is_root => {
            return Ok(Opened::RootFailed(String::from(
                "the library root is not a folder",
            )));
        }
        Ok(_) => return Ok(Opened::Skipped),
        Err(e) => return unreadable(&e, "read"),
    };
    if let (Some(dev), Some(ino)) = (dev_of(&dir_meta), ino_of(&dir_meta))
        && !visited.insert((dev, ino))
    {
        send(
            msgs,
            Msg::Error(error_row(
                rel,
                "skipped: this folder was already visited (symlink loop or repeated link)",
            )),
        )?;
        return Ok(Opened::Skipped);
    }
    #[cfg(test)]
    let denied = ctx.settings.deny.iter().any(|d| d == rel);
    #[cfg(not(test))]
    let denied = {
        let _ = ctx;
        false
    };
    let listed = if denied {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    } else {
        std::fs::read_dir(abs)
    };
    let reading = match listed {
        Ok(r) => r,
        Err(e) => return unreadable(&e, "listed"),
    };
    let mut names = Vec::new();
    for entry in reading {
        match entry {
            Ok(entry) => names.push((os_bytes(&entry.file_name()), entry)),
            Err(e) => return unreadable(&e, "listed"),
        }
    }
    Ok(Opened::Listed(dir_meta, names))
}

struct Classified {
    entries: Vec<DirEntrySummary>,
    audio: Vec<AudioEntry>,
    subdirs: Vec<(Vec<u8>, PathBuf)>,
}

/// Stats a folder's entries, following symlinks when configured, into listing entries, audio
/// files and subfolders.
fn classify(
    ctx: &WalkCtx,
    rel: &[u8],
    names: &[(Vec<u8>, std::fs::DirEntry)],
    msgs: &SyncSender<Msg>,
) -> Result<Classified, WalkStop> {
    let mut out = Classified {
        entries: Vec::with_capacity(names.len()),
        audio: Vec::new(),
        subdirs: Vec::new(),
    };
    for (name, entry) in names {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                send(
                    msgs,
                    Msg::Error(error_row(&child_rel(rel, name), &e.to_string())),
                )?;
                continue;
            }
        };
        let stat = if file_type.is_symlink() {
            if !ctx.settings.follow_symlinks {
                continue;
            }
            std::fs::metadata(&path)
        } else {
            entry.metadata()
        };
        let meta = match stat {
            Ok(m) => m,
            Err(e) => {
                stat_failed(ctx, rel, name, file_type, &e, msgs)?;
                continue;
            }
        };
        out.entries.push(DirEntrySummary {
            name: name.clone(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            mtime_ns: mtime_ns(&meta),
            is_dir: meta.is_dir(),
        });
        if meta.is_dir() {
            out.subdirs.push((child_rel(rel, name), path));
        } else if meta.is_file() && ctx.settings.extensions.matches(name) {
            out.audio.push(AudioEntry {
                name: name.clone(),
                path,
                stat: FileStat::of(&meta),
            });
        }
    }
    Ok(out)
}

/// An entry that could not be stat'ed. Its songs must not be marked missing: an audio file
/// counts as seen, and a folder (or a followed link to one, such as a share mounted elsewhere
/// that went away) is unreadable. A link the index never had a folder for is just broken.
fn stat_failed(
    ctx: &WalkCtx,
    rel: &[u8],
    name: &[u8],
    file_type: std::fs::FileType,
    error: &std::io::Error,
    msgs: &SyncSender<Msg>,
) -> Result<(), WalkStop> {
    let child = child_rel(rel, name);
    if ctx.settings.extensions.matches(name) {
        send(msgs, Msg::Seen(vec![child.clone()]))?;
        return send(msgs, Msg::Error(error_row(&child, &error.to_string())));
    }
    let known_folder = file_type.is_dir()
        || (file_type.is_symlink() && ctx.catalog.folder_state(ctx.library, &child)?.is_some());
    if known_folder {
        return send(msgs, Msg::Unreadable(error_row(&child, &error.to_string())));
    }
    if file_type.is_symlink() {
        return send(
            msgs,
            Msg::Error(error_row(&child, &format!("broken link: {error}"))),
        );
    }
    Ok(())
}

struct FolderFiles<'a> {
    rel: &'a [u8],
    abs: &'a Path,
    listing: &'a [Vec<u8>],
    artist_art: Option<PathBuf>,
}

/// Sends a changed folder's files to the readers, leaving out files reachable by an earlier
/// path, files still being written, quarantined files and (in quick scans) unchanged files.
#[expect(
    clippy::too_many_arguments,
    reason = "the walk's state, passed down one level"
)]
fn queue_files(
    ctx: &WalkCtx,
    folder: &FolderFiles<'_>,
    audio: Vec<AudioEntry>,
    rels: Vec<Vec<u8>>,
    files_seen: &mut HashSet<(u64, u64)>,
    now: SystemTime,
    msgs: &SyncSender<Msg>,
    jobs: &SyncSender<Job>,
) -> Result<(), WalkStop> {
    let sidecar_art = sidecar_in(folder.abs, folder.listing);
    let folder_id = FolderId::derive(&ctx.library_id, folder.rel);
    for (a, rel_path) in audio.into_iter().zip(rels) {
        if ctx.cancel.is_cancelled() {
            return Err(WalkStop::Cancelled);
        }
        if let (Some(dev), Some(ino)) = (a.stat.dev, a.stat.ino)
            && !files_seen.insert((dev, ino))
        {
            continue;
        }
        if recently_modified(a.stat.mtime_ns, now, ctx.settle) {
            send(msgs, Msg::Deferred(rel_path))?;
            continue;
        }
        if ctx
            .catalog
            .is_quarantined(ctx.library, &rel_path, a.stat.size, a.stat.mtime_ns)?
        {
            continue;
        }
        let known = ctx.catalog.known_file(ctx.library, &rel_path)?;
        if ctx.kind == ScanKind::Quick
            && let Some(k) = &known
            && k.size == a.stat.size
            && k.mtime_ns == a.stat.mtime_ns
            && k.missing_since.is_none()
        {
            continue;
        }
        let hints = ReadHints {
            lyrics_sidecar: crate::lyrics::sidecar_path(&a.path, Some(folder.listing)),
            listing_known: true,
        };
        send(
            jobs,
            Job {
                path: a.path,
                rel: rel_path,
                folder: folder_id.clone(),
                stat: a.stat,
                hints,
                sidecar_art: sidecar_art.clone(),
                artist_art: folder.artist_art.clone(),
                known: known.is_some(),
            },
        )?;
    }
    Ok(())
}

/// Whether a file was modified within `settle` of `now`, so it may still be being written.
pub fn recently_modified(mtime_ns: i64, now: SystemTime, settle: Duration) -> bool {
    if settle.is_zero() {
        return false;
    }
    let now_ns = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    let settle_ns = i64::try_from(settle.as_nanos()).unwrap_or(i64::MAX);
    now_ns.saturating_sub(mtime_ns) < settle_ns
}

fn error_row(rel: &[u8], error: &str) -> ScanErrorRow {
    ScanErrorRow {
        rel_path: rel.to_vec(),
        path_display: display(rel),
        error: error.to_owned(),
    }
}

struct Coordinator<'a> {
    services: &'a Services,
    input: &'a RunInput<'a>,
    key: LibraryKey,
    guard_ratio: f64,
    art_cache: Arc<ArtworkCache>,
    /// Artist pictures found beside read files, by credited name, applied at the end.
    artist_art: HashMap<String, ArtworkId>,
    pending: Batch,
    first_pending: Option<Instant>,
    last_progress: Option<Instant>,
    counts: ScanCounts,
    seen: SeenSet,
    unreadable: Vec<Vec<u8>>,
    deferred: Vec<Vec<u8>>,
    walked: HashMap<Vec<u8>, FolderUpsert>,
    dirty: HashSet<Vec<u8>>,
    upserted: HashSet<Vec<u8>>,
    summary: Option<WalkSummary>,
    stop: CancellationToken,
    /// Why the run stopped itself because the library's storage stopped answering.
    stalled: Option<String>,
}

impl Coordinator<'_> {
    fn drain(&mut self, rx: &Receiver<Msg>) -> Result<(), DbError> {
        loop {
            let wait = self
                .first_pending
                .map_or(BATCH_WAIT, |t| BATCH_WAIT.saturating_sub(t.elapsed()));
            match rx.recv_timeout(wait) {
                Ok(msg) => self.handle(msg)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let due = self
                .first_pending
                .is_some_and(|t| t.elapsed() >= BATCH_WAIT);
            if self.pending.files.len() >= BATCH_FILES || due {
                self.flush()?;
            }
            if self.stop.is_cancelled() {
                break;
            }
        }
        self.flush()
    }

    fn handle(&mut self, msg: Msg) -> Result<(), DbError> {
        match msg {
            Msg::Folder(folder) => {
                self.walked
                    .insert(folder.upsert.rel_path.clone(), folder.upsert);
            }
            Msg::Seen(paths) => {
                self.counts.files_seen += paths.len() as u64;
                for p in paths {
                    self.seen.insert(p);
                }
            }
            Msg::Deferred(rel) => {
                self.dirty.insert(parent_rel(&rel).to_vec());
                self.deferred.push(rel);
            }
            Msg::Error(row) => {
                self.counts.errors += 1;
                self.dirty.insert(parent_rel(&row.rel_path).to_vec());
                self.pending_at();
                self.pending.errors.push(row);
            }
            Msg::Unreadable(row) => {
                self.counts.errors += 1;
                self.unreadable.push(row.rel_path.clone());
                self.pending_at();
                self.pending.unreadable.push(row.rel_path.clone());
                self.pending.errors.push(row);
            }
            Msg::Read(result) => return self.handle_read(*result),
            Msg::Done(summary) => {
                self.counts.folders_seen = summary.folders;
                self.summary = Some(summary);
            }
        }
        Ok(())
    }

    fn pending_at(&mut self) {
        if self.first_pending.is_none() {
            self.first_pending = Some(Instant::now());
        }
    }

    /// A file new at its path may be a song that moved: if the present song with the same
    /// audio has lost its file, that path is marked gone in the same transaction, so ingest
    /// matches the file to the song by digest and the id stays.
    fn handle_read(&mut self, result: Result1) -> Result<(), DbError> {
        let folder = parent_rel(&result.rel).to_vec();
        match result.outcome {
            Outcome::Read(read) => {
                let crate::reader::ReadFile { file, artist_art } = *read;
                if !result.known
                    && let Some(existing) =
                        self.services.catalog.present_with_digest(&file.digest)?
                    && crate::import::file_is_gone(&existing.path)
                {
                    self.pending.gone.push((existing.library, existing.rel));
                }
                if let Some(art) = artist_art {
                    let name = file
                        .tags
                        .album_artist
                        .clone()
                        .unwrap_or_else(|| file.tags.artist.clone());
                    self.artist_art.insert(name, art);
                }
                self.pending_at();
                self.pending.files.push(file);
            }
            Outcome::Deferred => {
                self.dirty.insert(folder);
                self.deferred.push(result.rel);
            }
            Outcome::Failed(error) => {
                self.counts.errors += 1;
                self.dirty.insert(folder);
                self.pending_at();
                self.pending.errors.push(error_row(&result.rel, &error));
            }
            Outcome::Stalled(reason) => {
                self.counts.errors += 1;
                self.dirty.insert(folder);
                self.pending_at();
                self.pending.errors.push(error_row(&result.rel, &reason));
                if self.stalled.is_none() {
                    tracing::warn!(library = %self.input.library.id, %reason, "stopping the scan");
                    self.stalled = Some(reason);
                }
                self.stop.cancel();
            }
            Outcome::Quarantine(reason) => {
                self.counts.errors += 1;
                self.dirty.insert(folder);
                self.pending_at();
                self.pending.errors.push(error_row(&result.rel, &reason));
                self.pending.quarantine.push(QuarantinedFile {
                    rel_path: result.rel,
                    file_size: result.size,
                    file_mtime_ns: result.mtime_ns,
                    reason,
                    at: self.services.clock.now(),
                });
            }
        }
        Ok(())
    }

    /// Adds placeholder rows for the folders of `files` not yet written in this run. The
    /// listing hash is written only at the end, so an interrupted run never makes a quick scan
    /// skip a folder whose files were not all indexed.
    fn folder_rows(&mut self, files: &[ScannedFile]) -> Vec<FolderUpsert> {
        let mut rows = Vec::new();
        for file in files {
            let rel = parent_rel(&file.rel_path);
            if self.upserted.contains(rel) {
                continue;
            }
            self.upserted.insert(rel.to_vec());
            let name = self
                .walked
                .get(rel)
                .map_or_else(|| display(crate::paths::file_name(rel)), |f| f.name.clone());
            rows.push(FolderUpsert {
                rel_path: rel.to_vec(),
                name,
                mtime_ns: None,
                listing_hash: None,
            });
        }
        rows
    }

    fn commit(&mut self, mut batch: Batch) -> Result<(), DbError> {
        batch.artwork.extend(self.art_cache.take_fresh());
        if batch.is_empty() {
            return Ok(());
        }
        let mut folders = self.folder_rows(&batch.files);
        folders.append(&mut batch.folders);
        batch.folders = folders;
        let progress_due = self
            .last_progress
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if progress_due {
            batch.progress = Some(self.counts);
            self.last_progress = Some(Instant::now());
        }
        let resolutions =
            self.services
                .catalog
                .commit(self.key, self.input.run, SongOrigin::Scan, batch)?;
        for resolution in &resolutions {
            match resolution {
                Resolution::Added(_) => self.counts.added += 1,
                Resolution::Updated(_) | Resolution::Replaced(_) => self.counts.updated += 1,
                Resolution::Moved(_) => self.counts.moved += 1,
                Resolution::Unchanged(_) => {}
            }
        }
        (self.input.progress)(&self.counts);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DbError> {
        self.first_pending = None;
        let batch = std::mem::take(&mut self.pending);
        self.commit(batch)
    }

    fn report(&self, state: ScanState, message: Option<String>) -> RunReport {
        RunReport {
            state,
            counts: self.counts,
            message,
            deferred: self.deferred.clone(),
            unavailable: None,
            root_gone: false,
        }
    }

    fn end(&self, report: &RunReport, root: Option<RootHealth>) -> Result<(), DbError> {
        self.services.catalog.end_run(
            self.key,
            self.input.run,
            report.state,
            &report.counts,
            report.message.as_deref(),
            root,
        )?;
        Ok(())
    }

    fn unavailable(&self, reason: String, root_gone: bool) -> Result<RunReport, DbError> {
        let mut report = self.report(ScanState::Unavailable, Some(reason.clone()));
        report.unavailable = Some(reason);
        report.root_gone = root_gone;
        self.end(&report, None)?;
        Ok(report)
    }

    fn finish(mut self, root: &Path) -> Result<RunReport, DbError> {
        if let Some(reason) = self.stalled.take() {
            return self.unavailable(reason, false);
        }
        // No summary means the walker never finished (it was left behind after a stop).
        let Some(summary) = self.summary.take() else {
            let report = self.report(ScanState::Aborted, Some(String::from("stopped")));
            self.end(&report, None)?;
            return Ok(report);
        };
        if let Some(error) = summary.db_error {
            let report = self.report(ScanState::Failed, Some(error));
            self.end(&report, None)?;
            return Ok(report);
        }
        if summary.cancelled || self.stop.is_cancelled() {
            let report = self.report(ScanState::Aborted, Some(String::from("stopped")));
            self.end(&report, None)?;
            return Ok(report);
        }
        if let Some(reason) = summary.root_error {
            return self.unavailable(reason, true);
        }
        if let Some(reason) = self.guard(&summary, root)? {
            return self.unavailable(reason, false);
        }

        // Every file is committed by now, so the listing hashes can be written. Folders that
        // became readable again are cleared by these upserts, before the missing pass that
        // spares folders still stored as unreadable.
        let mut folders: Vec<FolderUpsert> =
            std::mem::take(&mut self.walked).into_values().collect();
        folders.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        for folder in &mut folders {
            if self.dirty.contains(&folder.rel_path) {
                folder.listing_hash = None;
            }
        }
        for chunk in folders.chunks(500) {
            self.commit(Batch {
                folders: chunk.to_vec(),
                ..Batch::default()
            })?;
        }
        let artists: Vec<(String, ArtworkId)> =
            std::mem::take(&mut self.artist_art).into_iter().collect();
        self.commit(Batch {
            artist_artwork: artists,
            ..Batch::default()
        })?;
        let scope = self.input.scope.as_deref().filter(|s| !s.is_empty());
        let outcome = match scope {
            Some(folder) => {
                self.services
                    .catalog
                    .finish_folder(self.key, self.input.run, folder, &self.seen)?
            }
            None => self.services.catalog.mark_missing(
                self.key,
                self.input.run,
                &self.seen,
                &self.unreadable,
            )?,
        };
        self.counts.missing = outcome.marked_missing;
        let message = (!self.deferred.is_empty()).then(|| {
            format!(
                "{} files were still being written; a follow-up scan in {} s imports them.",
                self.deferred.len(),
                crate::scanner::DEFERRED_RETRY.as_secs()
            )
        });
        let report = self.report(ScanState::Done, message);
        let root = scope.is_none().then_some(RootHealth {
            dev: summary.root_dev,
            entry_count: summary.root_entries,
        });
        self.end(&report, root)?;
        Ok(report)
    }

    /// The empty-library guard and the device check: reasons not to trust this walk enough to
    /// mark anything missing.
    fn guard(&self, summary: &WalkSummary, root: &Path) -> Result<Option<String>, DbError> {
        if self.input.force_empty {
            return Ok(None);
        }
        let known = self.input.library.song_count;
        if let Some(health) = self.services.catalog.root_health(self.key)?
            && let (Some(before), Some(now)) = (health.dev, summary.root_dev)
            && before != now
        {
            return Ok(Some(String::from(
                "the library folder is on a different device than at the last scan; is the share mounted?",
            )));
        }
        let whole_library = self.input.scope.as_deref().is_none_or(<[u8]>::is_empty);
        if whole_library && known > 0 && summary.root_entries == 0 {
            return Ok(Some(String::from(
                "the library folder is empty; is the share mounted?",
            )));
        }
        // Songs under unreadable folders are neither found nor marked missing, and their
        // number is unknown here, so the share-of-known-files check only runs without them.
        let found = self.seen.len() as f64;
        if whole_library
            && known > 0
            && self.unreadable.is_empty()
            && found < known as f64 * self.guard_ratio
        {
            return Ok(Some(format!(
                "found {} of {known} known files; is the share fully mounted?",
                self.seen.len()
            )));
        }
        match std::fs::metadata(root) {
            Ok(meta) if meta.is_dir() && dev_of(&meta) == summary.root_dev => Ok(None),
            _ => Ok(Some(String::from(
                "the library folder went away during the scan",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_defers_only_recent_files() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let ns = |s: i64| s * 1_000_000_000;
        assert!(recently_modified(ns(995), now, Duration::from_secs(10)));
        assert!(!recently_modified(ns(980), now, Duration::from_secs(10)));
        assert!(!recently_modified(ns(999), now, Duration::ZERO));
    }
}
