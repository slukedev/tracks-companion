//! The long-running parts: the scan queue and its worker thread, per-library state, the
//! schedules, and the glue to the monitor, the watcher and the importer.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use trackside_core::config::{ScannerConfig, UploadsConfig};
use trackside_core::control::{
    LibraryScanStatus, ScanControlError, ScanKind, ScanTrigger, WatchMode,
};
use trackside_core::events::ScanProgress;
use trackside_core::records::{Library, ScanCounts, ScanRun, ScanRunKind, ScanState};
use trackside_core::{EventBus, LibraryId, LibraryKey, ScanRunId, ServerEvent};

use crate::ScanError;
use crate::run::{self, RunInput, RunReport, RunSettings, Services};

/// A follow-up pass for files that were still changing.
pub const DEFERRED_RETRY: Duration = Duration::from_secs(15);
/// How often the availability monitor probes every library.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Which background parts to start. Tests turn them off to drive the scanner by hand.
#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent switches, one per background part"
)]
pub struct ScannerOptions {
    /// The availability monitor thread, which also queues the startup scans.
    pub monitor: bool,
    /// Filesystem watching (needs a tokio runtime).
    pub watch: bool,
    /// Periodic quick and full scans (needs a tokio runtime).
    pub schedule: bool,
    /// The inbox poller (needs a tokio runtime and `uploads.inbox_dir`).
    pub inbox: bool,
}

impl Default for ScannerOptions {
    fn default() -> Self {
        Self {
            monitor: true,
            watch: true,
            schedule: true,
            inbox: true,
        }
    }
}

impl ScannerOptions {
    /// Only the scan worker; everything else is driven by the caller.
    pub fn manual() -> Self {
        Self {
            monitor: false,
            watch: false,
            schedule: false,
            inbox: false,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LibState {
    pub library: Library,
    pub available: bool,
    pub scanning: Option<ScanProgress>,
    pub last_run: Option<ScanRun>,
    pub watching: WatchMode,
    pub probing: bool,
}

#[derive(Debug)]
struct Queued {
    key: LibraryKey,
    run: ScanRunId,
    kind: ScanKind,
    trigger: ScanTrigger,
    force_empty: bool,
    /// A folder scan (a watch event): this folder and everything under it.
    scope: Option<Vec<u8>>,
}

/// Whether a queued scan already covers `scope` (`None` is the whole library).
fn covers(queued: Option<&[u8]>, scope: Option<&[u8]>) -> bool {
    match (queued, scope) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(q), Some(s)) => {
            q.is_empty() || s == q || (s.len() > q.len() && s.starts_with(q) && s[q.len()] == b'/')
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct State {
    pub libs: HashMap<LibraryKey, LibState>,
    queue: VecDeque<Queued>,
    delayed: Vec<(Instant, LibraryKey)>,
    running: Option<(LibraryKey, CancellationToken)>,
}

/// Shared by the scanner's threads and tasks.
pub(crate) struct Inner {
    pub services: Services,
    pub settings: RunSettings,
    pub config: ScannerConfig,
    pub uploads: UploadsConfig,
    pub events: EventBus,
    pub shutdown: CancellationToken,
    state: Mutex<State>,
    wake: Condvar,
    requesting: Mutex<()>,
    pub watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").finish_non_exhaustive()
    }
}

/// Runs a blocking call from synchronous code that may be on a tokio worker.
pub(crate) fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

impl Inner {
    pub fn launch(
        services: Services,
        config: &ScannerConfig,
        uploads: &UploadsConfig,
        events: EventBus,
        shutdown: CancellationToken,
        options: &ScannerOptions,
    ) -> Result<Arc<Self>, ScanError> {
        let inner = Arc::new(Self {
            settings: RunSettings::from_config(config),
            services,
            config: config.clone(),
            uploads: uploads.clone(),
            events,
            shutdown,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            requesting: Mutex::new(()),
            watcher: Mutex::new(None),
        });
        inner.refresh_libraries();
        inner.recover_interrupted_runs();

        let worker = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("trackside-scan".into())
            .spawn(move || worker.work())?;

        if options.monitor {
            crate::monitor::spawn(Arc::clone(&inner))?;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if options.watch && config.watch {
                crate::watch::start(&inner, &handle);
            }
            if options.schedule {
                handle.spawn(schedule(Arc::clone(&inner)));
            }
            if options.inbox && uploads.inbox_dir.is_some() {
                handle.spawn(crate::import::inbox_task(Arc::clone(&inner)));
            }
        }
        Ok(inner)
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits on the scanner's condition variable for at most `timeout`.
    pub fn wait<'a>(
        &self,
        state: MutexGuard<'a, State>,
        timeout: Duration,
    ) -> MutexGuard<'a, State> {
        self.wake
            .wait_timeout(state, timeout)
            .map_or_else(|e| e.into_inner().0, |r| r.0)
    }

    pub fn notify(&self) {
        self.wake.notify_all();
    }

    /// Loads or reloads the library list, keeping what the monitor and runs know. Libraries
    /// that are gone, no longer want watching or moved to another root are unwatched; callers
    /// watch the new roots with `watch::add_library`.
    pub fn refresh_libraries(&self) -> Vec<Library> {
        let libraries = match self.services.catalog.libraries() {
            Ok(l) => l,
            Err(error) => {
                tracing::warn!(%error, "libraries could not be loaded");
                return Vec::new();
            }
        };
        let mut unwatch = Vec::new();
        let mut state = self.lock();
        for library in &libraries {
            match state.libs.get_mut(&library.key) {
                Some(lib) => {
                    if lib.watching == WatchMode::Events
                        && (lib.library.root != library.root || !crate::watch::wants(self, library))
                    {
                        unwatch.push(lib.library.root.clone());
                        lib.watching = WatchMode::Off;
                    }
                    lib.library = library.clone();
                }
                None => {
                    state.libs.insert(
                        library.key,
                        LibState {
                            available: library.available,
                            library: library.clone(),
                            scanning: None,
                            last_run: None,
                            watching: WatchMode::Off,
                            probing: false,
                        },
                    );
                }
            }
        }
        state.libs.retain(|key, lib| {
            let keep = libraries.iter().any(|l| l.key == *key);
            if !keep && lib.watching == WatchMode::Events {
                unwatch.push(lib.library.root.clone());
            }
            keep
        });
        drop(state);
        crate::watch::remove_roots(self, &unwatch);
        libraries
    }

    fn recover_interrupted_runs(&self) {
        let keys: Vec<LibraryKey> = self.lock().libs.keys().copied().collect();
        for key in keys {
            let runs = match self.services.catalog.recent_runs(key, 10) {
                Ok(runs) => runs,
                Err(error) => {
                    tracing::debug!(%error, "scan history unavailable");
                    continue;
                }
            };
            let mut last = None;
            for run in runs {
                if run.state == ScanState::Running {
                    match self.services.catalog.end_run(
                        key,
                        &run.id,
                        ScanState::Aborted,
                        &run.counts,
                        Some("the server stopped during this scan"),
                        None,
                    ) {
                        Ok(ended) => last = last.or(Some(ended)),
                        Err(error) => tracing::warn!(%error, "could not close an interrupted scan"),
                    }
                } else {
                    last = last.or(Some(run));
                }
            }
            if let Some(lib) = self.lock().libs.get_mut(&key) {
                lib.last_run = last;
            }
        }
    }

    /// The library's key, loading it if the scanner has not seen it yet.
    pub fn resolve(&self, id: &LibraryId) -> Result<Library, ScanControlError> {
        if let Some(lib) = self.lock().libs.values().find(|l| l.library.id == *id) {
            return Ok(lib.library.clone());
        }
        let library = self
            .services
            .catalog
            .library(id)?
            .ok_or(ScanControlError::UnknownLibrary)?;
        self.refresh_libraries();
        crate::watch::add_library(self, &library);
        Ok(library)
    }

    pub fn request_scan(
        &self,
        library: &LibraryId,
        kind: ScanKind,
        trigger: ScanTrigger,
        force_empty: bool,
    ) -> Result<ScanRunId, ScanControlError> {
        self.request(library, kind, trigger, force_empty, None)
    }

    /// Queues a scan of one folder and everything under it, for a watch event. An empty folder
    /// is the whole library.
    pub fn request_folder_scan(
        &self,
        library: &LibraryId,
        folder: Vec<u8>,
    ) -> Result<ScanRunId, ScanControlError> {
        let scope = (!folder.is_empty()).then_some(folder);
        self.request(library, ScanKind::Quick, ScanTrigger::Watch, false, scope)
    }

    fn request(
        &self,
        library: &LibraryId,
        kind: ScanKind,
        trigger: ScanTrigger,
        force_empty: bool,
        scope: Option<Vec<u8>>,
    ) -> Result<ScanRunId, ScanControlError> {
        if self.shutdown.is_cancelled() {
            return Err(ScanControlError::ShuttingDown);
        }
        let _serial = self
            .requesting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let library = self.resolve(library)?;
        if !library.enabled {
            return Err(ScanControlError::Rejected(String::from(
                "the library is disabled",
            )));
        }
        let superseded: Vec<Queued> = {
            let mut state = self.lock();
            let available = state.libs.get(&library.key).is_none_or(|l| l.available);
            if !available && !force_empty {
                return Err(ScanControlError::Unavailable);
            }
            if let Some(queued) = state.queue.iter_mut().find(|q| {
                q.key == library.key
                    && covers(q.scope.as_deref(), scope.as_deref())
                    && (q.kind == ScanKind::Full || kind == ScanKind::Quick)
            }) {
                queued.force_empty |= force_empty;
                return Ok(queued.run.clone());
            }
            let (gone, kept): (Vec<Queued>, Vec<Queued>) =
                std::mem::take(&mut state.queue).into_iter().partition(|q| {
                    q.key == library.key && covers(scope.as_deref(), q.scope.as_deref())
                });
            state.queue = kept.into();
            gone
        };
        for replaced in superseded {
            self.services.catalog.end_run(
                library.key,
                &replaced.run,
                ScanState::Aborted,
                &ScanCounts::default(),
                Some("replaced by a wider scan"),
                None,
            )?;
        }
        let run_kind = match (kind, &scope) {
            (_, Some(_)) => ScanRunKind::Folder,
            (ScanKind::Quick, None) => ScanRunKind::Quick,
            (ScanKind::Full, None) => ScanRunKind::Full,
        };
        let run = self
            .services
            .catalog
            .start_run(library.key, run_kind, trigger)?;
        self.lock().queue.push_back(Queued {
            key: library.key,
            run: run.id.clone(),
            kind,
            trigger,
            force_empty,
            scope,
        });
        self.notify();
        Ok(run.id)
    }

    pub fn status(&self) -> Vec<LibraryScanStatus> {
        let state = self.lock();
        let mut out: Vec<LibraryScanStatus> = state
            .libs
            .values()
            .map(|l| LibraryScanStatus {
                library: l.library.id.clone(),
                available: l.available,
                scanning: l.scanning.clone(),
                last_run: l.last_run.clone(),
                watching: l.watching,
            })
            .collect();
        out.sort_by(|a, b| a.library.cmp(&b.library));
        out
    }

    pub fn is_available(&self, id: &LibraryId) -> bool {
        self.lock()
            .libs
            .values()
            .find(|l| l.library.id == *id)
            .is_none_or(|l| l.available)
    }

    /// Records the monitor's or a run's verdict, tells clients, and queues a quick scan when a
    /// library comes back.
    pub fn set_available(&self, key: LibraryKey, available: bool, reason: Option<&str>) {
        let changed = {
            let mut state = self.lock();
            let Some(lib) = state.libs.get_mut(&key) else {
                return;
            };
            let changed = lib.available != available;
            lib.available = available;
            let id = changed.then(|| lib.library.id.clone());
            // A scan of a library whose storage went away would only wait on it (a hung
            // mount blocks the walker for good) and must not judge what is missing.
            if !available
                && let Some((running, token)) = &state.running
                && *running == key
            {
                token.cancel();
            }
            id
        };
        let Some(id) = changed else {
            return;
        };
        if let Err(error) = self.services.catalog.set_available(key, available, reason) {
            tracing::warn!(%error, "library state not saved");
        }
        if available {
            tracing::info!(library = %id, "library is available again");
        } else {
            tracing::warn!(library = %id, reason = reason.unwrap_or_default(), "library is unavailable");
        }
        self.events.publish(ServerEvent::LibraryState {
            library: id.clone(),
            available,
        });
        if available
            && let Err(error) =
                self.request_scan(&id, ScanKind::Quick, ScanTrigger::Schedule, false)
        {
            tracing::warn!(%error, "could not queue a scan after the library returned");
        }
    }

    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            if state.queue.is_empty() && state.running.is_none() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .wake
                .wait_timeout(state, (deadline - now).min(Duration::from_millis(50)))
                .map_or_else(|e| e.into_inner().0, |r| r.0);
        }
    }

    fn next(&self) -> Option<(Queued, CancellationToken)> {
        let mut state = self.lock();
        loop {
            if self.shutdown.is_cancelled() {
                return None;
            }
            let now = Instant::now();
            let due: Vec<LibraryKey> = state
                .delayed
                .iter()
                .filter(|(at, _)| *at <= now)
                .map(|(_, key)| *key)
                .collect();
            if !due.is_empty() {
                state.delayed.retain(|(at, _)| *at > now);
                let ids: Vec<LibraryId> = due
                    .iter()
                    .filter_map(|k| state.libs.get(k).map(|l| l.library.id.clone()))
                    .collect();
                drop(state);
                for id in ids {
                    if let Err(error) =
                        self.request_scan(&id, ScanKind::Quick, ScanTrigger::Schedule, false)
                    {
                        tracing::debug!(%error, "follow-up scan not queued");
                    }
                }
                state = self.lock();
                continue;
            }
            if let Some(queued) = state.queue.pop_front() {
                let token = self.shutdown.child_token();
                state.running = Some((queued.key, token.clone()));
                return Some((queued, token));
            }
            let wait = state
                .delayed
                .iter()
                .map(|(at, _)| at.saturating_duration_since(now))
                .min()
                .unwrap_or(Duration::from_secs(1))
                .min(Duration::from_secs(1));
            state = self
                .wake
                .wait_timeout(state, wait)
                .map_or_else(|e| e.into_inner().0, |r| r.0);
        }
    }

    fn work(self: Arc<Self>) {
        while let Some((queued, cancel)) = self.next() {
            let report = self.run_one(&queued, &cancel);
            let mut state = self.lock();
            state.running = None;
            if let Some(lib) = state.libs.get_mut(&queued.key) {
                lib.scanning = None;
            }
            if let Some(report) = &report
                && !report.deferred.is_empty()
            {
                state
                    .delayed
                    .push((Instant::now() + DEFERRED_RETRY, queued.key));
            }
            drop(state);
            self.notify();
        }
    }

    fn run_one(&self, queued: &Queued, cancel: &CancellationToken) -> Option<RunReport> {
        let library = {
            let state = self.lock();
            state.libs.get(&queued.key).map(|l| l.library.id.clone())
        };
        let fresh = library
            .as_ref()
            .and_then(|id| self.services.catalog.library(id).ok().flatten());
        let library =
            fresh.or_else(|| self.lock().libs.get(&queued.key).map(|l| l.library.clone()))?;
        if let Some(lib) = self.lock().libs.get_mut(&library.key) {
            lib.library = library.clone();
        }
        let last_sent = Mutex::new(None::<Instant>);
        let progress = |counts: &ScanCounts| {
            let mut last = last_sent.lock().unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
                return;
            }
            *last = Some(Instant::now());
            drop(last);
            self.publish_progress(&library, &queued.run, ScanState::Running, *counts);
        };
        let input = RunInput {
            library: &library,
            run: &queued.run,
            kind: queued.kind,
            force_empty: queued.force_empty,
            settle: if queued.trigger == ScanTrigger::Watch {
                self.config.settle.min(self.config.watch_debounce / 2)
            } else {
                self.config.settle
            },
            scope: queued
                .scope
                .as_deref()
                .map(|folder| existing_folder(&library.root, folder)),
            cancel,
            progress: &progress,
        };
        self.publish_progress(
            &library,
            &queued.run,
            ScanState::Running,
            ScanCounts::default(),
        );
        let started = Instant::now();
        match run::execute(&self.services, &self.settings, &input) {
            Ok(report) => {
                tracing::info!(
                    library = %library.id,
                    state = %report.state,
                    files = report.counts.files_seen,
                    added = report.counts.added,
                    updated = report.counts.updated,
                    moved = report.counts.moved,
                    missing = report.counts.missing,
                    errors = report.counts.errors,
                    seconds = started.elapsed().as_secs_f32(),
                    "scan finished"
                );
                self.publish_progress(&library, &queued.run, report.state, report.counts);
                if let Some(reason) = &report.unavailable
                    && (report.root_gone || report.counts.files_seen == 0)
                {
                    self.set_available(library.key, false, Some(reason));
                }
                let last = self
                    .services
                    .catalog
                    .recent_runs(library.key, 1)
                    .ok()
                    .and_then(|mut r| r.pop());
                if let Some(lib) = self.lock().libs.get_mut(&library.key) {
                    lib.last_run = last;
                }
                Some(report)
            }
            Err(error) => {
                tracing::error!(library = %library.id, %error, "scan failed");
                let _ = self.services.catalog.end_run(
                    library.key,
                    &queued.run,
                    ScanState::Failed,
                    &ScanCounts::default(),
                    Some(&error.to_string()),
                    None,
                );
                self.publish_progress(
                    &library,
                    &queued.run,
                    ScanState::Failed,
                    ScanCounts::default(),
                );
                None
            }
        }
    }

    fn publish_progress(
        &self,
        library: &Library,
        run: &ScanRunId,
        state: ScanState,
        counts: ScanCounts,
    ) {
        let progress = ScanProgress {
            library: library.id.clone(),
            run: run.clone(),
            state,
            counts,
        };
        if let Some(lib) = self.lock().libs.get_mut(&library.key) {
            lib.scanning = (state == ScanState::Running).then(|| progress.clone());
        }
        self.events.publish(ServerEvent::Scan(progress));
    }
}

/// `folder`, or its nearest ancestor that still exists: a deleted folder is rescanned from its
/// parent, so the songs that were in it are found missing.
pub fn existing_folder(root: &std::path::Path, folder: &[u8]) -> Vec<u8> {
    let mut rel = folder;
    while !rel.is_empty() && !crate::paths::join_rel(root, rel).is_dir() {
        rel = crate::paths::parent_rel(rel);
    }
    rel.to_vec()
}

/// Whether a periodic scan is due.
pub fn due(last: Instant, interval: Option<Duration>, now: Instant) -> bool {
    interval.is_some_and(|i| !i.is_zero() && now.saturating_duration_since(last) >= i)
}

/// Queues periodic quick and full scans. Network shares often deliver no filesystem events, so
/// quick scans run whether or not the library is watched.
async fn schedule(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let start = tokio::time::Instant::now().into_std();
    let mut last: HashMap<LibraryKey, (Instant, Instant)> = HashMap::new();
    loop {
        tokio::select! {
            () = inner.shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let now = tokio::time::Instant::now().into_std();
        let libs: Vec<(LibraryKey, LibraryId, Option<Duration>, bool)> = inner
            .lock()
            .libs
            .values()
            .map(|l| {
                (
                    l.library.key,
                    l.library.id.clone(),
                    l.library.quick_scan_interval,
                    l.available && l.library.enabled,
                )
            })
            .collect();
        for (key, id, quick_override, ready) in libs {
            let entry = last.entry(key).or_insert((start, start));
            if !ready {
                continue;
            }
            let quick = quick_override.or(inner.config.quick_interval);
            let kind = if due(entry.1, inner.config.full_interval, now) {
                entry.1 = now;
                entry.0 = now;
                Some(ScanKind::Full)
            } else if due(entry.0, quick, now) {
                entry.0 = now;
                Some(ScanKind::Quick)
            } else {
                None
            };
            if let Some(kind) = kind {
                let inner = Arc::clone(&inner);
                let result = tokio::task::spawn_blocking(move || {
                    inner.request_scan(&id, kind, ScanTrigger::Schedule, false)
                })
                .await;
                if let Ok(Err(error)) = result {
                    tracing::debug!(%error, "scheduled scan not queued");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wider_queued_scan_covers_a_narrower_one() {
        assert!(covers(None, Some(b"a/b")));
        assert!(covers(Some(b"a"), Some(b"a/b")));
        assert!(covers(Some(b"a"), Some(b"a")));
        assert!(!covers(Some(b"a"), Some(b"ab")));
        assert!(!covers(Some(b"a/b"), Some(b"a")));
        assert!(!covers(Some(b"a"), None));
    }

    #[test]
    fn a_deleted_folder_is_scanned_from_its_nearest_parent() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("a/b"))?;
        assert_eq!(existing_folder(dir.path(), b"a/b"), b"a/b".to_vec());
        assert_eq!(existing_folder(dir.path(), b"a/gone/deeper"), b"a".to_vec());
        assert_eq!(existing_folder(dir.path(), b"x"), Vec::<u8>::new());
        Ok(())
    }

    #[test]
    fn schedules_fire_after_their_interval_and_never_when_off() {
        let start = Instant::now();
        let later = start + Duration::from_mins(30);
        assert!(due(start, Some(Duration::from_mins(30)), later));
        assert!(!due(start, Some(Duration::from_secs(1801)), later));
        assert!(!due(start, None, later));
        assert!(!due(start, Some(Duration::ZERO), later));
    }
}
