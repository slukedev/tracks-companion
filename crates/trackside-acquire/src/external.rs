//! Playing a song the library does not have without keeping it (architecture section 9.7).
//!
//! The bytes go to `<data>/cache/external/<token>/`. A finished song gets `entry.json` beside
//! its file; those are the cache's index on disk, read back at start. Retention keeps the most
//! recently played songs within a count and a size (section 9.8). Nothing here writes an
//! acquisition, a library song or the change log.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use trackside_core::control::{
    AcquireError, ExternalPlay, ExternalPlayRequest, FetchedUsage, GrowState, GrowingFile,
    InstantStream,
};
use trackside_core::ids::PublicId;
use trackside_core::repo::{acquisitions, songs};
use trackside_core::{Access, CandidateToken, Millis, UserKey};

use crate::acquirer::{Inner, Slot, candidate_of_row, lock, search_providers, token_of};
use crate::provider::{Candidate, ProgressSink, ProviderError, SearchQuery};
use crate::rank::{rank, tolerance_ms};

/// How long to wait for a free fetch before answering that the source is busy.
const PERMIT_WAIT: Duration = Duration::from_secs(5);

/// A finished song's record beside its file.
const MANIFEST: &str = "entry.json";
/// Where evicted songs go before they are deleted: a rename is the moment a song leaves.
const TRASH: &str = ".trash";
/// Written when the folder is loaded. Its absence means the volume went away (an unmounted
/// network share), and nothing is evicted until it is back.
const MARKER: &str = ".trackside-play-cache";

/// The limits on finished songs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub keep: u32,
    pub max_bytes: u64,
}

/// Songs being fetched, and fetched songs kept for replays.
#[derive(Debug, Default)]
pub(crate) struct ExternalCache {
    entries: HashMap<CandidateToken, Entry>,
    /// The candidate a song asked for by name resolved to, by `song_key`.
    by_key: HashMap<String, CandidateToken>,
    /// Whether the folder's songs were read back. Until then nothing is evicted.
    loaded: bool,
}

#[derive(Debug)]
struct Entry {
    dir: PathBuf,
    file: GrowingFile,
    /// For a finished song, the sender every handed-out `file` listens to: more than one
    /// receiver means a stream or a transcode still holds the file.
    holder: Option<watch::Sender<GrowState>>,
    title: String,
    artist: String,
    duration_ms: Option<u32>,
    started_by: UserKey,
    /// When the fetch started, then when it finished.
    fetched_at: Millis,
    /// The last time a stream of it was asked for.
    played_at: Option<Millis>,
    /// Set once the whole file is in.
    size: Option<u64>,
    /// Chosen for eviction: no longer handed out.
    evicting: bool,
    /// `played_at` changed since `entry.json` was written, or it was never written.
    dirty: bool,
}

/// `entry.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    v: u8,
    file: String,
    bytes: u64,
    title: String,
    artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u32>,
    started_by: i64,
    fetched_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    played_at: Option<i64>,
    /// The `song_key`s that resolved to this song when asked for by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    keys: Vec<String>,
}

impl Entry {
    fn play(&self, token: &CandidateToken) -> ExternalPlay {
        ExternalPlay {
            token: Some(token.clone()),
            title: self.title.clone(),
            artist: self.artist.clone(),
            duration_ms: self.duration_ms,
            song: None,
        }
    }

    fn failed(&self) -> bool {
        matches!(*self.file.state.borrow(), GrowState::Failed)
    }

    fn usable(&self) -> bool {
        !self.evicting && !self.failed()
    }

    fn played(&mut self, now: Millis) {
        self.played_at = Some(now);
        self.dirty = true;
    }

    /// When it was last played, or fetched when it never was.
    fn recency(&self) -> Millis {
        self.played_at
            .map_or(self.fetched_at, |p| p.max(self.fetched_at))
    }

    /// Still being fetched, read by a stream or a transcode, or played within `grace`.
    fn in_use(&self, now: Millis, grace: Duration) -> bool {
        self.size.is_none()
            || self.holder.as_ref().is_some_and(|h| h.receiver_count() > 1)
            || now.since(self.recency()) < grace
    }

    fn manifest(&self, keys: Vec<String>) -> Option<Manifest> {
        Some(Manifest {
            v: 1,
            file: self.file.path.file_name()?.to_str()?.to_owned(),
            bytes: self.size?,
            title: self.title.clone(),
            artist: self.artist.clone(),
            duration_ms: self.duration_ms,
            started_by: self.started_by.0,
            fetched_at: self.fetched_at.0,
            played_at: self.played_at.map(|p| p.0),
            keys,
        })
    }

    fn finished(dir: PathBuf, manifest: Manifest) -> Self {
        let path = dir.join(&manifest.file);
        let (holder, state) = watch::channel(GrowState::Complete {
            bytes: manifest.bytes,
        });
        Self {
            dir,
            file: GrowingFile { path, state },
            holder: Some(holder),
            title: manifest.title,
            artist: manifest.artist,
            duration_ms: manifest.duration_ms,
            started_by: UserKey(manifest.started_by),
            fetched_at: Millis(manifest.fetched_at),
            played_at: manifest.played_at.map(Millis),
            size: Some(manifest.bytes),
            evicting: false,
            dirty: false,
        }
    }
}

impl ExternalCache {
    fn touch(&mut self, token: &CandidateToken, now: Millis) -> Option<ExternalPlay> {
        let entry = self.entries.get_mut(token).filter(|e| e.usable())?;
        entry.played(now);
        Some(entry.play(token))
    }

    fn active_for(&self, user: UserKey) -> usize {
        self.entries
            .values()
            .filter(|e| e.started_by == user && e.size.is_none() && !e.failed())
            .count()
    }

    fn keys_of(&self, token: &CandidateToken) -> Vec<String> {
        let mut keys: Vec<String> = self
            .by_key
            .iter()
            .filter(|(_, t)| *t == token)
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    fn usage(&self) -> (u64, u64, u64) {
        let (mut songs, mut bytes, mut fetching) = (0, 0, 0);
        for entry in self.entries.values().filter(|e| !e.failed()) {
            match entry.size {
                Some(size) => {
                    songs += 1;
                    bytes += size;
                }
                None => fetching += 1,
            }
        }
        (songs, bytes, fetching)
    }

    /// Drops failed entries, then picks the finished songs that go so the rest fit `limits`:
    /// the least recently played first, and never one in use, though those still count.
    /// Picked songs are marked so nobody is handed them while they go.
    fn pick(&mut self, now: Millis, limits: Limits, grace: Duration) -> Picked {
        let failed: Vec<CandidateToken> = self
            .entries
            .iter()
            .filter(|(_, e)| e.failed())
            .map(|(t, _)| t.clone())
            .collect();
        let failed = failed
            .iter()
            .filter_map(|t| self.entries.remove(t).map(|e| e.dir))
            .collect();
        let mut finished: Vec<(Millis, Millis, &CandidateToken, u64, bool)> = self
            .entries
            .iter()
            .filter(|(_, e)| !e.evicting)
            .filter_map(|(t, e)| {
                e.size
                    .map(|size| (e.recency(), e.fetched_at, t, size, e.in_use(now, grace)))
            })
            .collect();
        finished.sort();
        let mut count = finished.len() as u64;
        let mut total: u64 = finished.iter().map(|(.., size, _)| size).sum();
        let mut victims = Vec::new();
        for (_, _, token, size, in_use) in finished {
            if count <= u64::from(limits.keep) && total <= limits.max_bytes {
                break;
            }
            if in_use {
                continue;
            }
            count -= 1;
            total -= size;
            victims.push(token.clone());
        }
        let victims = victims
            .into_iter()
            .filter_map(|t| {
                let entry = self.entries.get_mut(&t)?;
                entry.evicting = true;
                Some((t, entry.dir.clone()))
            })
            .collect();
        self.by_key.retain(|_, t| self.entries.contains_key(t));
        Picked { failed, victims }
    }

    /// Takes a song off the index once its folder is gone, or hands it back.
    fn evicted(&mut self, token: &CandidateToken, gone: bool) {
        if gone {
            self.entries.remove(token);
            self.by_key.retain(|_, t| t != token);
        } else if let Some(entry) = self.entries.get_mut(token) {
            entry.evicting = false;
        }
    }
}

#[derive(Debug, Default)]
struct Picked {
    failed: Vec<PathBuf>,
    victims: Vec<(CandidateToken, PathBuf)>,
}

impl Inner {
    pub(crate) async fn play_external(
        self: &Arc<Self>,
        caller: &Access,
        request: ExternalPlayRequest,
    ) -> Result<ExternalPlay, AcquireError> {
        self.ensure_ready().await?;
        if !self.live().enabled {
            return Err(AcquireError::Disabled);
        }
        let now = self.now();
        let candidate = match request {
            ExternalPlayRequest::Candidate(token) => {
                if let Some(play) = lock(&self.external).touch(&token, now) {
                    return Ok(play);
                }
                let row = self
                    .db
                    .read(move |c| acquisitions::candidate_by_token_at(c, &token, now))
                    .await?
                    .ok_or(AcquireError::CandidateExpired)?;
                match candidate_of_row(&row).filter(|c| self.streams(c)) {
                    Some(candidate) => candidate,
                    // A copy that cannot be played while it is fetched: the same song from a
                    // source that can.
                    None => {
                        let query = song_query(&row.title, &row.artist, None, row.duration_ms);
                        match self.resolve(caller, query).await? {
                            Resolved::Library(play) => return Ok(play),
                            Resolved::Candidate(candidate) => candidate,
                        }
                    }
                }
            }
            ExternalPlayRequest::Song {
                title,
                artist,
                album,
                duration_ms,
            } => {
                let query = song_query(&title, &artist, album, duration_ms);
                if query.title.is_empty() || query.artist.is_empty() {
                    return Err(AcquireError::Invalid(String::from(
                        "title and artist are required",
                    )));
                }
                let key = tracks_model::song_key(&query.title, &query.artist);
                let known = lock(&self.external).by_key.get(&key).cloned();
                if let Some(play) = known.and_then(|t| lock(&self.external).touch(&t, now)) {
                    return Ok(play);
                }
                match self.resolve(caller, query).await? {
                    Resolved::Library(play) => return Ok(play),
                    Resolved::Candidate(candidate) => {
                        lock(&self.external)
                            .by_key
                            .insert(key, token_of(&candidate));
                        candidate
                    }
                }
            }
        };
        self.start_external(caller, candidate).await
    }

    pub(crate) fn external_stream(&self, token: &CandidateToken) -> Option<InstantStream> {
        let mut cache = lock(&self.external);
        let entry = cache.entries.get_mut(token).filter(|e| e.usable())?;
        entry.played(self.now());
        Some(InstantStream {
            progressive: entry.file.clone(),
            hls_job: None,
            content_type: "audio/mp4",
            duration_ms: entry.duration_ms,
        })
    }

    fn streams(&self, candidate: &Candidate) -> bool {
        self.slot(candidate.provider)
            .is_some_and(|s| s.enabled.load(Ordering::Relaxed) && s.provider.streams())
    }

    /// The library's copy, or the best candidate from the sources that can stream.
    async fn resolve(
        self: &Arc<Self>,
        caller: &Access,
        query: SearchQuery,
    ) -> Result<Resolved, AcquireError> {
        let key = tracks_model::song_key(&query.title, &query.artist);
        let access = caller.clone();
        let wanted = query.duration_ms;
        let present = self
            .db
            .read(move |c| songs::by_key(c, &access, &key))
            .await?
            .into_iter()
            .filter(|song| !song.missing)
            .find(|song| {
                wanted.is_none_or(|want| song.duration_ms.abs_diff(want) <= tolerance_ms(want))
            });
        if let Some(song) = present {
            return Ok(Resolved::Library(ExternalPlay {
                token: None,
                title: song.title,
                artist: song.artist,
                duration_ms: Some(song.duration_ms),
                song: Some(song.id),
            }));
        }
        let mut streaming = false;
        for slot in self.slots.iter().filter(|s| s.provider.streams()) {
            streaming |= self.usable(slot).await;
        }
        if !streaming {
            return Err(self.unavailable());
        }
        if let Err(wait) = self.user_limiter(caller.user).try_take() {
            return Err(AcquireError::RateLimited { retry_after: wait });
        }
        let state = Arc::default();
        let answered = search_providers(
            self,
            &query,
            self.config.search_timeout,
            self.config.search_timeout,
            Arc::clone(&state),
            true,
        )
        .await;
        let candidates = std::mem::take(&mut lock(&state).candidates);
        if candidates.is_empty() && !answered {
            return Err(self.unavailable());
        }
        rank(&query, candidates, &self.rank_options(false))
            .into_iter()
            .map(|r| r.candidate)
            .find(|c| self.streams(c))
            .map(Resolved::Candidate)
            .ok_or(AcquireError::NotFound)
    }

    fn unavailable(&self) -> AcquireError {
        let now = self.now();
        let retry_after = self
            .slots
            .iter()
            .filter(|s| s.provider.streams())
            .filter_map(|s| s.cooldown_until(now))
            .min()
            .map_or(Duration::from_secs(60), |until| until.since(now));
        AcquireError::Unavailable { retry_after }
    }

    async fn start_external(
        self: &Arc<Self>,
        caller: &Access,
        candidate: Candidate,
    ) -> Result<ExternalPlay, AcquireError> {
        let token = token_of(&candidate);
        let _starting = self.external_start.lock().await;
        let now = self.now();
        {
            let cache = lock(&self.external);
            if let Some(entry) = cache.entries.get(&token).filter(|e| e.usable()) {
                return Ok(entry.play(&token));
            }
            if cache.active_for(caller.user) >= self.timing.external_per_user {
                return Err(AcquireError::RateLimited {
                    retry_after: Duration::from_secs(5),
                });
            }
        }
        let slot = self
            .slot(candidate.provider)
            .ok_or(AcquireError::NotFound)?;
        if !self.usable(slot).await {
            return Err(self.unavailable());
        }
        let permit = tokio::time::timeout(PERMIT_WAIT, Arc::clone(&slot.fetches).acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(AcquireError::Unavailable {
                retry_after: PERMIT_WAIT,
            })?;
        let dir = self.external_dir.join(token.as_str());
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| AcquireError::Invalid(e.to_string()))?;
        let (sink, _) = ProgressSink::new();
        let cancel = self.shutdown.child_token();
        slot.count(now, |s| s.fetches_today += 1);
        let started = match slot
            .provider
            .play_streaming(&candidate, &dir, sink, cancel.clone())
        {
            Some(started) => started.await,
            None => Err(ProviderError::NotFound),
        };
        let streaming = match started {
            Ok(streaming) => streaming,
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                self.external_failed(slot, &e);
                return Err(self.unavailable());
            }
        };
        let entry = Entry {
            dir,
            file: streaming.growing,
            holder: None,
            title: candidate.title.clone(),
            artist: candidate.artist.clone(),
            duration_ms: candidate.duration_ms,
            started_by: caller.user,
            fetched_at: now,
            played_at: None,
            size: None,
            evicting: false,
            dirty: true,
        };
        let play = entry.play(&token);
        // A failed entry of the same song may still be there; it is replaced.
        lock(&self.external).entries.insert(token.clone(), entry);
        let inner = Arc::clone(self);
        let provider = candidate.provider;
        self.tracker.spawn(async move {
            let _permit = permit;
            let done = streaming.done.await;
            let Some(slot) = inner.slot(provider) else {
                return;
            };
            match done {
                Ok(fetched) => {
                    lock(&slot.cooldown).succeed();
                    inner.external_finished(&token, fetched.bytes).await;
                }
                Err(e) => {
                    if !cancel.is_cancelled() {
                        inner.external_failed(slot, &e);
                    }
                    let _starting = inner.external_start.lock().await;
                    let dir = lock(&inner.external).entries.remove(&token).map(|e| e.dir);
                    if let Some(dir) = dir {
                        let _ = inner.within(tokio::fs::remove_dir_all(dir)).await;
                    }
                }
            }
            inner.sweep_external().await;
        });
        Ok(play)
    }

    /// Marks a fetch complete and writes its `entry.json`, so it survives a restart.
    async fn external_finished(&self, token: &CandidateToken, bytes: u64) {
        let _starting = self.external_start.lock().await;
        let now = self.now();
        let written = {
            let mut cache = lock(&self.external);
            let keys = cache.keys_of(token);
            let Some(entry) = cache.entries.get_mut(token) else {
                return;
            };
            let (holder, state) = watch::channel(GrowState::Complete { bytes });
            entry.file = GrowingFile {
                path: entry.file.path.clone(),
                state,
            };
            entry.holder = Some(holder);
            entry.size = Some(bytes);
            entry.fetched_at = now;
            entry.dirty = true;
            entry.manifest(keys).map(|m| (entry.dir.clone(), m))
        };
        if let Some((dir, manifest)) = written {
            self.write_manifest(token, &dir, &manifest).await;
        }
    }

    /// Writes `entry.json` through a temporary file and a rename, so it is whole or absent.
    async fn write_manifest(&self, token: &CandidateToken, dir: &Path, manifest: &Manifest) {
        let Ok(text) = serde_json::to_vec(manifest) else {
            return;
        };
        let temp = dir.join(format!("{MANIFEST}.tmp"));
        let path = dir.join(MANIFEST);
        let written = self
            .within(async {
                tokio::fs::write(&temp, &text).await?;
                tokio::fs::rename(&temp, &path).await
            })
            .await;
        match written {
            Ok(()) => {
                if let Some(entry) = lock(&self.external).entries.get_mut(token)
                    && entry.played_at.map(|p| p.0) == manifest.played_at
                {
                    entry.dirty = false;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "could not record a fetched song; it goes at the next restart");
            }
        }
    }

    fn external_failed(&self, slot: &Slot, error: &ProviderError) {
        slot.count(self.now(), |s| s.failures_today += 1);
        if error.wants_cooldown() {
            let retry_after = match error {
                ProviderError::RateLimited { retry_after } => *retry_after,
                _ => None,
            };
            self.trip_cooldown(slot, retry_after);
        }
        tracing::info!(provider = slot.name().as_str(), error = %error, "playing a song without keeping it failed");
    }

    /// The limits the dashboard or configuration set.
    pub(crate) fn fetched_limits(&self) -> Limits {
        let live = self.live();
        Limits {
            keep: live.fetched_keep,
            max_bytes: live.fetched_max_bytes,
        }
    }

    pub(crate) fn fetched_usage(&self) -> FetchedUsage {
        let (songs, bytes, fetching) = lock(&self.external).usage();
        let limits = self.fetched_limits();
        FetchedUsage {
            songs,
            bytes,
            fetching,
            keep: limits.keep,
            max_bytes: limits.max_bytes,
        }
    }

    /// Runs a filesystem step with a deadline: on a hung network share it never returns.
    async fn within<T>(
        &self,
        step: impl Future<Output = std::io::Result<T>>,
    ) -> std::io::Result<T> {
        tokio::time::timeout(self.timing.space_check_deadline, step)
            .await
            .unwrap_or_else(|_| Err(std::io::Error::from(std::io::ErrorKind::TimedOut)))
    }

    /// Brings fetched songs back within the limits, after each fetch and on the janitor's
    /// round. A song leaves the index only once its folder has been moved out of the way, and
    /// nothing is evicted while the folder's volume is missing.
    pub(crate) async fn sweep_external(&self) {
        let starting = self.external_start.lock().await;
        if !lock(&self.external).loaded {
            return;
        }
        let marker = self.external_dir.join(MARKER);
        if self.within(tokio::fs::metadata(&marker)).await.is_err() {
            tracing::warn!(dir = %self.external_dir.display(), "the play cache folder is not reachable; nothing is evicted until it is");
            return;
        }
        let dirty: Vec<(CandidateToken, PathBuf, Manifest)> = {
            let cache = lock(&self.external);
            cache
                .entries
                .iter()
                .filter(|(_, e)| e.dirty && !e.evicting)
                .filter_map(|(t, e)| {
                    let manifest = e.manifest(cache.keys_of(t))?;
                    Some((t.clone(), e.dir.clone(), manifest))
                })
                .collect()
        };
        for (token, dir, manifest) in dirty {
            self.write_manifest(&token, &dir, &manifest).await;
        }
        let picked = lock(&self.external).pick(
            self.now(),
            self.fetched_limits(),
            self.timing.external_in_use,
        );
        let trash = self.external_dir.join(TRASH);
        if !picked.victims.is_empty() || !picked.failed.is_empty() {
            let _ = self.within(tokio::fs::create_dir_all(&trash)).await;
        }
        for dir in picked.failed {
            let _ = self
                .within(tokio::fs::rename(&dir, trash_name(&trash, &dir)))
                .await;
        }
        let mut evicted = 0_u32;
        for (token, dir) in picked.victims {
            let moved = self
                .within(tokio::fs::rename(&dir, trash_name(&trash, &dir)))
                .await;
            let gone = match moved {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                Err(e) => {
                    tracing::warn!(error = %e, dir = %dir.display(), "could not evict a fetched song; trying again later");
                    false
                }
            };
            lock(&self.external).evicted(&token, gone);
            evicted += u32::from(gone);
        }
        drop(starting);
        if evicted > 0 {
            tracing::info!(evicted, "fetched songs evicted to stay within the limits");
        }
        self.empty_trash().await;
    }

    async fn empty_trash(&self) {
        let trash = self.external_dir.join(TRASH);
        match self.within(tokio::fs::remove_dir_all(&trash)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, dir = %trash.display(), "could not delete evicted songs; trying again later");
            }
        }
    }

    /// Reads back the songs an earlier run fetched, and removes what does not check out: a
    /// folder without a whole `entry.json`, or whose file is missing or the wrong size. At
    /// start, and on the janitor's round in case the folder was unreachable before.
    pub(crate) async fn load_external(&self) {
        let starting = self.external_start.lock().await;
        let (known, first) = {
            let cache = lock(&self.external);
            let known: HashSet<CandidateToken> = cache.entries.keys().cloned().collect();
            (known, !cache.loaded)
        };
        let dir = self.external_dir.clone();
        let deadline = self.timing.space_check_deadline * 10;
        let scan = tokio::task::spawn_blocking(move || reconcile(&dir, &known, first));
        let found = match tokio::time::timeout(deadline, scan).await {
            Ok(Ok(Ok(found))) => found,
            Ok(Ok(Err(e))) => {
                tracing::warn!(error = %e, dir = %self.external_dir.display(), "could not read the play cache; trying again later");
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "reading the play cache failed");
                return;
            }
            Err(_) => {
                tracing::warn!(dir = %self.external_dir.display(), "reading the play cache timed out; trying again later");
                return;
            }
        };
        {
            let mut cache = lock(&self.external);
            for (token, entry_dir, manifest) in found {
                for key in &manifest.keys {
                    cache
                        .by_key
                        .entry(key.clone())
                        .or_insert_with(|| token.clone());
                }
                cache
                    .entries
                    .entry(token)
                    .or_insert_with(|| Entry::finished(entry_dir, manifest));
            }
            cache.loaded = true;
        }
        drop(starting);
        self.empty_trash().await;
    }
}

/// A unique place in the trash for `dir`.
fn trash_name(trash: &Path, dir: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = dir
        .file_name()
        .map_or_else(Default::default, |n| n.to_string_lossy().into_owned());
    trash.join(format!("{name}.{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// The folder's finished songs, after removing everything else in it apart from `known`
/// songs and the trash. Never follows a link. The first time it marks the folder; after that
/// a folder without the mark is a volume that went away, and is left alone.
fn reconcile(
    root: &Path,
    known: &HashSet<CandidateToken>,
    first: bool,
) -> std::io::Result<Vec<(CandidateToken, PathBuf, Manifest)>> {
    if first {
        std::fs::create_dir_all(root)?;
        std::fs::write(root.join(MARKER), b"")?;
    } else {
        std::fs::metadata(root.join(MARKER))?;
    }
    let mut found = Vec::new();
    for item in std::fs::read_dir(root)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        if name == MARKER || name == TRASH {
            continue;
        }
        let token = CandidateToken::parse(&name).ok();
        if token.as_ref().is_some_and(|t| known.contains(t)) {
            continue;
        }
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        let entry = token
            .filter(|_| is_dir)
            .and_then(|token| read_manifest(&path).map(|m| (token, path.clone(), m)));
        match entry {
            Some(entry) => found.push(entry),
            None if is_dir => std::fs::remove_dir_all(&path)?,
            None => std::fs::remove_file(&path)?,
        }
    }
    Ok(found)
}

fn read_manifest(dir: &Path) -> Option<Manifest> {
    let text = std::fs::read(dir.join(MANIFEST)).ok()?;
    let manifest: Manifest = serde_json::from_slice(&text).ok()?;
    let plain = Path::new(&manifest.file)
        .file_name()
        .is_some_and(|n| n == manifest.file.as_str())
        && manifest.file != MANIFEST
        && !manifest.file.starts_with('.');
    if manifest.v != 1 || !plain {
        return None;
    }
    let meta = std::fs::symlink_metadata(dir.join(&manifest.file)).ok()?;
    (meta.is_file() && meta.len() == manifest.bytes).then_some(manifest)
}

enum Resolved {
    Library(ExternalPlay),
    Candidate(Candidate),
}

fn song_query(
    title: &str,
    artist: &str,
    album: Option<String>,
    duration_ms: Option<u32>,
) -> SearchQuery {
    SearchQuery {
        title: title.trim().to_owned(),
        artist: artist.trim().to_owned(),
        album: album.map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()),
        duration_ms,
        free_text: None,
    }
}

/// Where the play cache lives under a data directory.
pub(crate) fn dir_in(data_dir: &Path) -> PathBuf {
    data_dir.join("cache").join("external")
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::random_id;

    const GRACE: Duration = Duration::from_mins(30);
    const NOW: Millis = Millis(1_000_000_000);

    fn at(minutes_ago: i64) -> Millis {
        Millis(NOW.0 - minutes_ago * 60_000)
    }

    fn song(
        cache: &mut ExternalCache,
        bytes: u64,
        fetched: Millis,
        played: Option<Millis>,
    ) -> CandidateToken {
        let token: CandidateToken = random_id();
        let manifest = Manifest {
            v: 1,
            file: String::from("audio.m4a"),
            bytes,
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            duration_ms: None,
            started_by: 2,
            fetched_at: fetched.0,
            played_at: played.map(|p| p.0),
            keys: Vec::new(),
        };
        let entry = Entry::finished(PathBuf::from(token.as_str()), manifest);
        cache.entries.insert(token.clone(), entry);
        token
    }

    fn limits(keep: u32, max_bytes: u64) -> Limits {
        Limits { keep, max_bytes }
    }

    fn victims(cache: &mut ExternalCache, limits: Limits) -> Vec<CandidateToken> {
        cache
            .pick(NOW, limits, GRACE)
            .victims
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn nothing_goes_while_both_limits_hold_exactly() {
        let mut cache = ExternalCache::default();
        song(&mut cache, 10, at(120), None);
        song(&mut cache, 10, at(110), None);
        assert_eq!(victims(&mut cache, limits(2, 20)), Vec::new());
    }

    #[test]
    fn one_song_over_the_count_evicts_the_least_recently_played() {
        let mut cache = ExternalCache::default();
        let old = song(&mut cache, 10, at(300), Some(at(200)));
        let newer = song(&mut cache, 10, at(400), Some(at(100)));
        let fresh = song(&mut cache, 10, at(90), None);
        assert_eq!(victims(&mut cache, limits(2, u64::MAX)), vec![old]);
        assert!(cache.entries[&newer].usable());
        assert!(cache.entries[&fresh].usable());
    }

    #[test]
    fn a_song_never_played_is_ordered_by_when_it_was_fetched() {
        let mut cache = ExternalCache::default();
        let played = song(&mut cache, 10, at(500), Some(at(300)));
        let fetched_earlier = song(&mut cache, 10, at(400), None);
        let fetched_later = song(&mut cache, 10, at(200), None);
        assert_eq!(
            victims(&mut cache, limits(1, u64::MAX)),
            vec![fetched_earlier, played]
        );
        assert!(cache.entries[&fetched_later].usable());
    }

    #[test]
    fn the_size_limit_evicts_until_the_rest_fits() {
        for (max_bytes, gone) in [(80, 1), (79, 2)] {
            let mut cache = ExternalCache::default();
            let oldest = [
                song(&mut cache, 40, at(300), None),
                song(&mut cache, 40, at(200), None),
            ];
            song(&mut cache, 40, at(100), None);
            assert_eq!(
                victims(&mut cache, limits(500, max_bytes)),
                oldest[..gone].to_vec()
            );
        }
    }

    #[test]
    fn zero_keeps_nothing_but_what_is_in_use() {
        let mut cache = ExternalCache::default();
        let idle = song(&mut cache, 1, at(300), None);
        let playing = song(&mut cache, 1, at(300), Some(at(5)));
        assert_eq!(victims(&mut cache, limits(0, u64::MAX)), vec![idle]);
        assert!(cache.entries[&playing].usable());
        let mut cache = ExternalCache::default();
        let huge = song(&mut cache, 1_000, at(300), None);
        assert_eq!(victims(&mut cache, limits(10, 0)), vec![huge]);
    }

    #[test]
    fn songs_in_use_are_skipped_but_still_count() {
        let mut cache = ExternalCache::default();
        let held = song(&mut cache, 10, at(300), None);
        let _stream = cache.entries[&held]
            .holder
            .as_ref()
            .map(watch::Sender::subscribe);
        let recent = song(&mut cache, 10, at(300), Some(at(29)));
        let idle = song(&mut cache, 10, at(200), None);
        let fetching: CandidateToken = random_id();
        let (_tx, state) = watch::channel(GrowState::Growing { bytes: 4 });
        cache.entries.insert(
            fetching.clone(),
            Entry {
                size: None,
                holder: None,
                file: GrowingFile {
                    path: PathBuf::from("x"),
                    state,
                },
                ..Entry::finished(
                    PathBuf::from("x"),
                    Manifest {
                        v: 1,
                        file: String::from("a"),
                        bytes: 0,
                        title: String::new(),
                        artist: String::new(),
                        duration_ms: None,
                        started_by: 2,
                        fetched_at: at(500).0,
                        played_at: None,
                        keys: Vec::new(),
                    },
                )
            },
        );
        assert_eq!(victims(&mut cache, limits(1, u64::MAX)), vec![idle]);
        assert!(cache.entries[&held].usable());
        assert!(cache.entries[&recent].usable());
        assert!(cache.entries[&fetching].usable());
        assert_eq!(cache.usage(), (3, 30, 1));
        assert!(
            victims(&mut cache, limits(0, 0)).is_empty(),
            "only songs in use are left"
        );
    }

    #[test]
    fn a_song_being_evicted_is_not_handed_out_and_comes_back_if_it_stays() {
        let mut cache = ExternalCache::default();
        let token = song(&mut cache, 10, at(300), None);
        cache.by_key.insert(String::from("halo"), token.clone());
        assert_eq!(victims(&mut cache, limits(0, 0)), vec![token.clone()]);
        assert!(cache.touch(&token, NOW).is_none());
        assert!(victims(&mut cache, limits(0, 0)).is_empty(), "picked once");
        cache.evicted(&token, false);
        assert!(cache.touch(&token, NOW).is_some());
        assert_eq!(
            cache.pick(NOW, limits(0, 0), Duration::ZERO).victims.len(),
            1
        );
        cache.evicted(&token, true);
        assert!(cache.entries.is_empty());
        assert!(cache.by_key.is_empty());
    }

    #[test]
    fn failed_fetches_are_dropped() {
        let mut cache = ExternalCache::default();
        let token = song(&mut cache, 10, at(300), None);
        if let Some(entry) = cache.entries.get_mut(&token) {
            let (_tx, state) = watch::channel(GrowState::Failed);
            entry.file.state = state;
        }
        let picked = cache.pick(NOW, limits(10, 100), GRACE);
        assert_eq!(picked.failed.len(), 1);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn manifests_must_name_a_whole_plain_file_beside_them() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let write = |file: &str, bytes: u64| -> std::io::Result<()> {
            let manifest = serde_json::json!({
                "v": 1, "file": file, "bytes": bytes, "title": "Halo", "artist": "Beyoncé",
                "started_by": 2, "fetched_at": 1,
            });
            std::fs::write(dir.path().join(MANIFEST), manifest.to_string())
        };
        std::fs::write(dir.path().join("audio.m4a"), b"12345")?;
        write("audio.m4a", 5)?;
        assert!(read_manifest(dir.path()).is_some());
        write("audio.m4a", 6)?;
        assert!(read_manifest(dir.path()).is_none(), "wrong size");
        write("../audio.m4a", 5)?;
        assert!(read_manifest(dir.path()).is_none(), "outside its folder");
        write(MANIFEST, 5)?;
        assert!(read_manifest(dir.path()).is_none());
        std::fs::write(dir.path().join(MANIFEST), b"{\"v\":1,")?;
        assert!(read_manifest(dir.path()).is_none(), "cut short");
        Ok(())
    }
}
