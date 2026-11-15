//! Playing a song the library does not have without keeping it (architecture section 9.7).
//!
//! The bytes go to `<data>/cache/external/<token>/`, which is bounded by size and age. Nothing
//! here writes an acquisition, a library song or the change log.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use trackside_core::control::{
    AcquireError, ExternalPlay, ExternalPlayRequest, GrowState, GrowingFile, InstantStream,
};
use trackside_core::ids::PublicId;
use trackside_core::repo::{acquisitions, songs};
use trackside_core::{Access, CandidateToken, Millis, UserKey};

use crate::acquirer::{Inner, Slot, candidate_of_row, lock, search_providers, token_of};
use crate::provider::{Candidate, ProgressSink, ProviderError, SearchQuery};
use crate::rank::{rank, tolerance_ms};

/// How long to wait for a free fetch before answering that the source is busy.
const PERMIT_WAIT: Duration = Duration::from_secs(5);

/// Songs being streamed or streamed recently.
#[derive(Debug, Default)]
pub(crate) struct ExternalCache {
    entries: HashMap<CandidateToken, Entry>,
    /// The candidate a song asked for by name resolved to, by `song_key`.
    by_key: HashMap<String, CandidateToken>,
}

#[derive(Debug)]
struct Entry {
    dir: PathBuf,
    file: GrowingFile,
    title: String,
    artist: String,
    duration_ms: Option<u32>,
    started_by: UserKey,
    used: Millis,
    /// Set once the whole file is in.
    size: Option<u64>,
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
}

impl ExternalCache {
    fn touch(&mut self, token: &CandidateToken, now: Millis) -> Option<ExternalPlay> {
        let entry = self.entries.get_mut(token).filter(|e| !e.failed())?;
        entry.used = now;
        Some(entry.play(token))
    }

    fn active_for(&self, user: UserKey) -> usize {
        self.entries
            .values()
            .filter(|e| e.started_by == user && e.size.is_none() && !e.failed())
            .count()
    }

    /// Drops finished entries unused for longer than `ttl`, then the least recently used
    /// finished ones until the total fits `max_bytes`. Returns the directories to delete.
    fn sweep(&mut self, now: Millis, ttl: Duration, max_bytes: u64) -> Vec<PathBuf> {
        let mut gone: Vec<CandidateToken> = self
            .entries
            .iter()
            .filter(|(_, e)| e.failed() || (e.size.is_some() && now.since(e.used) > ttl))
            .map(|(t, _)| t.clone())
            .collect();
        let mut finished: Vec<(Millis, CandidateToken, u64)> = self
            .entries
            .iter()
            .filter(|(t, _)| !gone.contains(t))
            .filter_map(|(t, e)| e.size.map(|size| (e.used, t.clone(), size)))
            .collect();
        finished.sort_by_key(|(used, ..)| *used);
        let mut total: u64 = finished.iter().map(|(.., size)| size).sum();
        for (_, token, size) in finished {
            if total <= max_bytes {
                break;
            }
            total -= size;
            gone.push(token);
        }
        let dirs = gone
            .iter()
            .filter_map(|t| self.entries.remove(t).map(|e| e.dir))
            .collect();
        self.by_key.retain(|_, t| self.entries.contains_key(t));
        dirs
    }
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
        self.sweep_external().await;
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
        let entry = cache.entries.get_mut(token).filter(|e| !e.failed())?;
        entry.used = self.now();
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
            if let Some(entry) = cache.entries.get(&token).filter(|e| !e.failed()) {
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
            title: candidate.title.clone(),
            artist: candidate.artist.clone(),
            duration_ms: candidate.duration_ms,
            started_by: caller.user,
            used: now,
            size: None,
        };
        let play = entry.play(&token);
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
                    if let Some(entry) = lock(&inner.external).entries.get_mut(&token) {
                        entry.size = Some(fetched.bytes);
                    }
                }
                Err(e) => {
                    if !cancel.is_cancelled() {
                        inner.external_failed(slot, &e);
                    }
                    let dir = lock(&inner.external).entries.remove(&token).map(|e| e.dir);
                    if let Some(dir) = dir {
                        let _ = tokio::fs::remove_dir_all(dir).await;
                    }
                }
            }
            inner.sweep_external().await;
        });
        Ok(play)
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

    pub(crate) async fn sweep_external(&self) {
        let dirs = lock(&self.external).sweep(
            self.now(),
            self.timing.external_ttl,
            self.timing.external_max_bytes,
        );
        for dir in dirs {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
    }

    /// Empties what an earlier run left: the index of it did not survive.
    pub(crate) async fn clear_external(&self) {
        let _ = tokio::fs::remove_dir_all(&self.external_dir).await;
        if let Err(e) = tokio::fs::create_dir_all(&self.external_dir).await {
            tracing::warn!(error = %e, dir = %self.external_dir.display(), "could not create the play cache");
        }
    }
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
