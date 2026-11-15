//! Filesystem watching with notify. Events are debounced per folder: a folder that has been
//! quiet for `scanner.watch_debounce` triggers a quick scan of its library, which skips every
//! unchanged folder by its listing hash. When the inotify watch limit is reached the library
//! falls back to the periodic quick scans.

use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::event::{AccessKind, AccessMode, MetadataKind, ModifyKind};
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::Instant;
use trackside_core::LibraryKey;
use trackside_core::control::WatchMode;
use trackside_core::records::Library;

use crate::paths::{parent_rel, rel_of};
use crate::scanner::Inner;

/// Waits for keys to go quiet: each touch pushes the key's deadline back.
#[derive(Debug)]
pub struct Debouncer<K> {
    wait: std::time::Duration,
    deadlines: HashMap<K, Instant>,
}

impl<K: Eq + Hash + Clone> Debouncer<K> {
    pub fn new(wait: std::time::Duration) -> Self {
        Self {
            wait,
            deadlines: HashMap::new(),
        }
    }

    pub fn touch(&mut self, key: K, now: Instant) {
        self.deadlines.insert(key, now + self.wait);
    }

    /// The earliest deadline, if anything is waiting.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.values().min().copied()
    }

    /// Removes and returns every key whose deadline has passed.
    pub fn take_due(&mut self, now: Instant) -> Vec<K> {
        let due: Vec<K> = self
            .deadlines
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &due {
            self.deadlines.remove(key);
        }
        due
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.deadlines.is_empty()
    }
}

/// Whether an event can mean a changed library. Opening and reading files, which the scanner
/// itself does, and access-time updates are ignored.
pub fn is_relevant(kind: EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_)
        | EventKind::Other
        | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => false,
        _ => true,
    }
}

const EVENT_QUEUE: usize = 4096;

/// A watched folder that went quiet.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Quiet {
    Folder(LibraryKey, Vec<u8>),
    Inbox,
}

/// Creates the watcher, watches every library that wants it, and starts the debounce task.
pub fn start(inner: &Arc<Inner>, handle: &tokio::runtime::Handle) {
    let (tx, rx) = mpsc::channel::<PathBuf>(EVENT_QUEUE);
    let overflow = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&overflow);
    let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            flag.store(true, Ordering::Relaxed);
            return;
        };
        if !is_relevant(event.kind) {
            return;
        }
        for path in event.paths {
            if tx.try_send(path).is_err() {
                flag.store(true, Ordering::Relaxed);
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(error) => {
            tracing::warn!(%error, "filesystem events are unavailable; relying on periodic scans");
            return;
        }
    };
    if let Some(inbox) = &inner.uploads.inbox_dir
        && let Err(error) = watcher.watch(inbox, RecursiveMode::Recursive)
    {
        tracing::warn!(%error, inbox = %inbox.display(), "the inbox folder is not watched");
    }
    *inner
        .watcher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(watcher);
    let libraries: Vec<Library> = inner
        .lock()
        .libs
        .values()
        .map(|l| l.library.clone())
        .collect();
    for library in &libraries {
        add_library(inner, library);
    }
    handle.spawn(debounce(Arc::clone(inner), rx, overflow));
}

/// Watches a library's root if it wants watching and is not watched yet.
pub fn add_library(inner: &Inner, library: &Library) {
    let current = inner.lock().libs.get(&library.key).map(|l| l.watching);
    if !matches!(current, Some(WatchMode::Off)) {
        return;
    }
    if !library.watch || !library.enabled || !inner.config.watch {
        return;
    }
    let mut guard = inner
        .watcher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(watcher) = guard.as_mut() else {
        return;
    };
    let mode = match watcher.watch(&library.root, RecursiveMode::Recursive) {
        Ok(()) => WatchMode::Events,
        Err(error) if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) => {
            tracing::warn!(
                library = %library.id,
                "the inotify watch limit is reached; this library is checked by periodic quick scans instead. Raise fs.inotify.max_user_watches to watch it"
            );
            WatchMode::Polling
        }
        Err(error) => {
            tracing::warn!(library = %library.id, %error, "library not watched; relying on periodic scans");
            WatchMode::Polling
        }
    };
    drop(guard);
    if let Some(lib) = inner.lock().libs.get_mut(&library.key) {
        lib.watching = mode;
    }
}

fn classify(inner: &Inner, path: &Path) -> Option<Quiet> {
    if let Some(inbox) = &inner.uploads.inbox_dir
        && path.starts_with(inbox)
    {
        return Some(Quiet::Inbox);
    }
    let state = inner.lock();
    state.libs.values().find_map(|l| {
        let rel = rel_of(&l.library.root, path)?;
        Some(Quiet::Folder(l.library.key, parent_rel(&rel).to_vec()))
    })
}

async fn debounce(inner: Arc<Inner>, mut rx: mpsc::Receiver<PathBuf>, overflow: Arc<AtomicBool>) {
    let mut waiting: Debouncer<Quiet> = Debouncer::new(inner.config.watch_debounce);
    loop {
        let sleep = waiting.next_deadline();
        tokio::select! {
            () = inner.shutdown.cancelled() => return,
            received = rx.recv() => {
                let Some(path) = received else { return };
                if let Some(quiet) = classify(&inner, &path) {
                    waiting.touch(quiet, Instant::now());
                }
                if overflow.swap(false, Ordering::Relaxed) {
                    let keys: Vec<LibraryKey> = inner.lock().libs.keys().copied().collect();
                    for key in keys {
                        waiting.touch(Quiet::Folder(key, Vec::new()), Instant::now());
                    }
                }
            }
            () = async {
                match sleep {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
        let due = waiting.take_due(Instant::now());
        let mut folders: Vec<(LibraryKey, Vec<u8>)> = Vec::new();
        let mut inbox = false;
        for quiet in due {
            match quiet {
                Quiet::Folder(key, folder) => folders.push((key, folder)),
                Quiet::Inbox => inbox = true,
            }
        }
        for (key, folder) in folders {
            let id = inner.lock().libs.get(&key).map(|l| l.library.id.clone());
            let Some(id) = id else { continue };
            let worker = Arc::clone(&inner);
            let result =
                tokio::task::spawn_blocking(move || worker.request_folder_scan(&id, folder)).await;
            if let Ok(Err(error)) = result {
                tracing::debug!(%error, "watch scan not queued");
            }
        }
        if inbox {
            tokio::spawn(crate::import::poll_inbox(Arc::clone(&inner)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_folder_fires_once_it_has_been_quiet_for_the_debounce() {
        let mut d: Debouncer<&str> = Debouncer::new(Duration::from_secs(5));
        let start = Instant::now();
        d.touch("a", start);
        tokio::time::advance(Duration::from_secs(3)).await;
        d.touch("a", Instant::now());
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(d.take_due(Instant::now()), Vec::<&str>::new());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(d.take_due(Instant::now()), vec!["a"]);
        assert!(d.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn each_folder_has_its_own_deadline() {
        let mut d: Debouncer<&str> = Debouncer::new(Duration::from_secs(5));
        d.touch("a", Instant::now());
        tokio::time::advance(Duration::from_secs(4)).await;
        d.touch("b", Instant::now());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(d.take_due(Instant::now()), vec!["a"]);
        assert_eq!(
            d.next_deadline(),
            Some(Instant::now() + Duration::from_secs(4))
        );
    }

    #[test]
    fn opening_and_reading_files_is_not_a_change() {
        assert!(!is_relevant(EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_relevant(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(is_relevant(EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(is_relevant(EventKind::Create(
            notify::event::CreateKind::File
        )));
        assert!(!is_relevant(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));
    }
}
