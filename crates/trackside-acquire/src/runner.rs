//! The leased job runner (architecture sections 9 and 9.3): lease, search, rank, fetch,
//! verify, tag, import, with retries, cooldowns, staging cleanup and a janitor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use trackside_core::control::{
    AcquireError, AcquireFailure, AcquisitionState, ImportOutcome, InstantStream, SongOrigin,
};
use trackside_core::ids::PublicId;
use trackside_core::repo::acquisitions::{self, AcquisitionRow, AcquisitionUpdate, CandidateRow};
use trackside_core::{AcquisitionId, DbError, SongId, UserKey};

use crate::acquirer::{
    Inner, Running, Slot, candidate_of_row, candidate_row, lock, quality_of_row,
};
use crate::provider::{Candidate, FetchedFile, ProgressSink, ProviderError, SearchQuery};
use crate::rank::rank;
use crate::store::{self, TargetLibrary};
use crate::tag::write_tags;
use crate::verify::{Expectation, TagValues, safe_file_name, unique_path, verify_with};

/// The template for songs without an album (architecture section 9, step 8).
const SINGLES_TEMPLATE: &str = "{artist}/Singles/{title}.{ext}";
/// Where a user-picked candidate sits among the job's candidates: first.
const PICKED_SCORE: f32 = 1000.0;
/// ENOSPC.
const NO_SPACE: i32 = 28;

/// The runner: leases ready jobs while there is room, and sleeps until the next one is due
/// or something wakes it.
pub(crate) async fn run(inner: Arc<Inner>) {
    if let Err(e) = start(&inner).await {
        tracing::warn!(error = %e, "acquisition runner could not start cleanly");
    }
    let mut last_janitor = Instant::now();
    let mut last_health = Instant::now();
    let mut space_check: Option<tokio::task::JoinHandle<bool>> = None;
    loop {
        if inner.shutdown.is_cancelled() {
            break;
        }
        if last_janitor.elapsed() >= inner.timing.janitor_every {
            last_janitor = Instant::now();
            if let Err(e) = janitor(&inner).await {
                tracing::warn!(error = %e, "staging janitor failed");
            }
        }
        if last_health.elapsed() >= inner.timing.health_every {
            last_health = Instant::now();
            for slot in &inner.slots {
                inner.check_health(slot).await;
            }
        }
        let paused = !space_ok(&inner, &mut space_check).await;
        if paused != inner.disk_paused.swap(paused, Ordering::Relaxed) {
            if paused {
                tracing::warn!(
                    "acquisition queue paused: staging or the acquired library is nearly full"
                );
            } else {
                tracing::info!("acquisition queue resumed");
            }
        }
        let wait = if paused {
            inner.timing.idle_poll
        } else {
            match lease_ready(&inner).await {
                Ok(wait) => wait,
                Err(e) => {
                    tracing::warn!(error = %e, "could not lease acquisition jobs");
                    inner.timing.idle_poll
                }
            }
        };
        tokio::select! {
            () = inner.shutdown.cancelled() => break,
            () = inner.wake.notified() => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
}

async fn start(inner: &Arc<Inner>) -> Result<(), DbError> {
    inner.ensure_ready().await?;
    // One process owns a data directory, so every lease at start belongs to a process that
    // is gone; their jobs resume from their state now rather than after the lease runs out.
    let released = inner.db.write(acquisitions::release_leases).await?;
    if released > 0 {
        tracing::info!(jobs = released, "resuming interrupted acquisitions");
    }
    tokio::fs::create_dir_all(&inner.config.staging_dir).await?;
    janitor(inner).await?;
    for slot in &inner.slots {
        inner.check_health(slot).await;
    }
    Ok(())
}

/// Whether staging and the acquired library have room. `statfs` on a hung network mount
/// never returns, so it runs on the blocking pool with a deadline, and a check still stuck
/// from an earlier round is waited on again instead of starting another.
async fn space_ok(inner: &Arc<Inner>, pending: &mut Option<tokio::task::JoinHandle<bool>>) -> bool {
    let check = pending.get_or_insert_with(|| {
        let inner = Arc::clone(inner);
        tokio::task::spawn_blocking(move || space_ok_blocking(&inner))
    });
    match tokio::time::timeout(inner.timing.space_check_deadline, check).await {
        Ok(result) => {
            *pending = None;
            result.unwrap_or(true)
        }
        Err(_) => {
            tracing::warn!(
                "free space check did not answer; is the acquired library's share hung?"
            );
            false
        }
    }
}

fn space_ok_blocking(inner: &Inner) -> bool {
    let mut paths = vec![inner.config.staging_dir.clone()];
    if let Some(target) = inner.cached_target() {
        paths.push(target.root);
    }
    paths
        .iter()
        .all(|path| match inner.free_space.available(path) {
            Ok(free) => free >= inner.timing.min_free_bytes,
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "free space unknown");
                true
            }
        })
}

/// Leases jobs while there is room and returns how long to sleep.
async fn lease_ready(inner: &Arc<Inner>) -> Result<Duration, DbError> {
    while lock(&inner.jobs).len() < inner.timing.max_jobs {
        let now = inner.now();
        let lease = inner.timing.lease;
        let Some(row) = inner
            .db
            .write(move |tx| acquisitions::lease_next(tx, now, lease))
            .await?
        else {
            break;
        };
        let cancel = inner.shutdown.child_token();
        let cancelled_by_user = Arc::new(AtomicBool::new(false));
        lock(&inner.jobs).insert(
            row.id.clone(),
            Running {
                cancel: cancel.clone(),
                cancelled_by_user: Arc::clone(&cancelled_by_user),
                stream: None,
                quality: None,
            },
        );
        let job_inner = Arc::clone(inner);
        inner
            .tracker
            .spawn(job(job_inner, row, cancel, cancelled_by_user));
    }
    let next = inner.db.read(acquisitions::next_ready_at).await?;
    let now = inner.now();
    Ok(match next {
        Some(at) if at > now => at.since(now).min(inner.timing.idle_poll),
        _ => inner.timing.idle_poll,
    })
}

async fn job(
    inner: Arc<Inner>,
    row: AcquisitionRow,
    cancel: CancellationToken,
    cancelled_by_user: Arc<AtomicBool>,
) {
    let id = row.id.clone();
    let renew = {
        let inner = Arc::clone(&inner);
        let id = id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(inner.timing.renew_every).await;
                let until = inner.now().saturating_add(inner.timing.lease);
                let id = id.clone();
                if let Err(e) = inner
                    .db
                    .write(move |tx| acquisitions::renew_lease(tx, &id, until))
                    .await
                {
                    tracing::warn!(error = %e, "could not renew an acquisition lease");
                }
            }
        })
    };
    let job = Job {
        inner: &inner,
        id: id.clone(),
        cancel: &cancel,
        cancelled_by_user: &cancelled_by_user,
        staging: inner.config.staging_dir.join(id.as_str()),
    };
    // Ends the lease renewal and frees the job's place even if `execute` panics, which would
    // otherwise renew a dead job's lease forever and keep its slot.
    let _cleanup = JobCleanup {
        inner: Arc::clone(&inner),
        id: id.clone(),
        renew,
    };
    let result = job.execute(row).await;
    if let Err(e) = result {
        tracing::warn!(error = %e, acquisition = %id, "acquisition job failed; it will be retried");
        let retry_at = inner.now().saturating_add(inner.timing.idle_poll);
        let id = id.clone();
        let _ = inner
            .db
            .write(move |tx| {
                acquisitions::transition(
                    tx,
                    &id,
                    AcquisitionUpdate {
                        lease_until: Some(None),
                        next_attempt_at: Some(retry_at),
                        ..AcquisitionUpdate::default()
                    },
                )
            })
            .await;
    }
}

struct JobCleanup {
    inner: Arc<Inner>,
    id: AcquisitionId,
    renew: tokio::task::JoinHandle<()>,
}

impl Drop for JobCleanup {
    fn drop(&mut self) {
        self.renew.abort();
        lock(&self.inner.jobs).remove(&self.id);
        self.inner.wake.notify_one();
    }
}

/// Why a job stopped early.
enum Stop {
    /// The server is shutting down: leave the job as it is for the next start.
    Shutdown,
    /// The user or an admin cancelled it; the row is already `cancelled`.
    Cancelled,
}

/// What happened to one fetched file.
enum Imported {
    Done,
    Rejected(String),
    NoSpace(String),
}

/// Why the remaining candidates ran out.
#[derive(Default)]
struct Tally {
    tried: usize,
    skipped_unusable: usize,
    last_failure: Option<String>,
}

struct Job<'a> {
    inner: &'a Arc<Inner>,
    id: AcquisitionId,
    cancel: &'a CancellationToken,
    cancelled_by_user: &'a AtomicBool,
    staging: PathBuf,
}

impl Job<'_> {
    fn stopped(&self) -> Option<Stop> {
        if self.cancelled_by_user.load(Ordering::SeqCst) {
            Some(Stop::Cancelled)
        } else if self.inner.shutdown.is_cancelled() {
            Some(Stop::Shutdown)
        } else if self.cancel.is_cancelled() {
            Some(Stop::Cancelled)
        } else {
            None
        }
    }

    /// Applies a change unless the request already finished (a cancel won the race), mirrors
    /// it to joined requests, and publishes the views.
    async fn update(&self, change: AcquisitionUpdate) -> Result<Option<AcquisitionRow>, DbError> {
        let id = self.id.clone();
        let rows = self
            .inner
            .db
            .write(move |tx| {
                let Some(current) = acquisitions::get(tx, &id)? else {
                    return Ok(None);
                };
                if current.state.is_finished() {
                    return Ok(None);
                }
                let mirror = AcquisitionUpdate {
                    state: change.state,
                    failure: change.failure,
                    progress: change.progress,
                    song: change.song.clone(),
                    existing: change.existing,
                    ..AcquisitionUpdate::default()
                };
                let main = acquisitions::transition(tx, &id, change)?;
                let mut rows = vec![main];
                if mirror != AcquisitionUpdate::default() {
                    for joined in acquisitions::joined(tx, &id)? {
                        if !joined.state.is_finished() {
                            rows.push(acquisitions::transition(tx, &joined.id, mirror.clone())?);
                        }
                    }
                }
                Ok(Some(rows))
            })
            .await?;
        let Some(rows) = rows else {
            return Ok(None);
        };
        for row in &rows {
            self.inner.publish(row, None);
        }
        Ok(rows.into_iter().next())
    }

    async fn finish(
        &self,
        state: AcquisitionState,
        failure: Option<AcquireFailure>,
        detail: Option<String>,
    ) -> Result<(), DbError> {
        self.clean_staging().await;
        self.update(AcquisitionUpdate {
            state: Some(state),
            failure,
            failure_detail: detail,
            lease_until: Some(None),
            staging_dir: Some(None),
            ..AcquisitionUpdate::default()
        })
        .await?;
        Ok(())
    }

    async fn clean_staging(&self) {
        match tokio::fs::remove_dir_all(&self.staging).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %self.staging.display(), "could not remove staging");
            }
        }
    }

    async fn stop(&self, stop: Stop) -> Result<(), DbError> {
        if matches!(stop, Stop::Cancelled) {
            self.clean_staging().await;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the job lifecycle of architecture section 9 in order"
    )]
    async fn execute(&self, row: AcquisitionRow) -> Result<(), DbError> {
        let inner = self.inner;
        if !inner.live().enabled {
            return self
                .finish(
                    AcquisitionState::Failed,
                    Some(AcquireFailure::Disabled),
                    Some(String::from("acquisition is turned off")),
                )
                .await;
        }
        let Some(target) = inner.refresh_target().await? else {
            return self
                .finish(
                    AcquisitionState::Failed,
                    Some(AcquireFailure::Disabled),
                    Some(String::from("no acquired library is set")),
                )
                .await;
        };
        let mut row = row;
        let mut tally = Tally::default();

        if matches!(
            row.state,
            AcquisitionState::Verifying | AcquisitionState::Importing
        ) && let Some(candidate) = self.chosen(&row).await?
            && let Some(file) = find_staged(&self.staging).await
        {
            match self.import(&row, &target, &candidate.1, file).await? {
                Imported::Done => return Ok(()),
                Imported::NoSpace(why) => return self.no_space(why).await,
                Imported::Rejected(why) => self.mark_tried(&candidate.0.token, &why).await?,
            }
        }
        if row.state == AcquisitionState::Downloading
            && let Some((cand_row, candidate)) = self.chosen(&row).await?
        {
            if let Some(stop) = self.stopped() {
                return self.stop(stop).await;
            }
            match self
                .try_candidate(&row, &target, &cand_row, &candidate, true, &mut tally)
                .await?
            {
                Some(Ok(())) => return Ok(()),
                Some(Err(stop)) => return self.stop(stop).await,
                None => {}
            }
        }

        let mut list = if row.state == AcquisitionState::Queued {
            Vec::new()
        } else {
            self.untried().await?
        };
        if list.is_empty() {
            let Some(updated) = self
                .update(AcquisitionUpdate {
                    state: Some(AcquisitionState::Searching),
                    add_attempt: true,
                    ..AcquisitionUpdate::default()
                })
                .await?
            else {
                return self.stop(Stop::Cancelled).await;
            };
            row = updated;
            let (found, providers_answered) = self.search(&row).await?;
            if let Some(stop) = self.stopped() {
                return self.stop(stop).await;
            }
            if found.is_empty() {
                if providers_answered == 0 {
                    return self
                        .retry_or_fail(
                            &row,
                            AcquireFailure::ProviderUnavailable,
                            String::from("no source could be reached"),
                        )
                        .await;
                }
                return self
                    .finish(
                        AcquisitionState::Failed,
                        Some(AcquireFailure::NoMatch),
                        Some(String::from("nothing passed the filters")),
                    )
                    .await;
            }
            list = found;
        }

        for cand_row in list {
            if let Some(stop) = self.stopped() {
                return self.stop(stop).await;
            }
            let Some(candidate) = candidate_of_row(&cand_row) else {
                self.mark_tried(&cand_row.token, "unknown provider").await?;
                continue;
            };
            match self
                .try_candidate(&row, &target, &cand_row, &candidate, false, &mut tally)
                .await?
            {
                Some(Ok(())) => return Ok(()),
                Some(Err(stop)) => return self.stop(stop).await,
                None => {}
            }
        }
        if let Some(stop) = self.stopped() {
            return self.stop(stop).await;
        }
        let row = self.current().await?.unwrap_or(row);
        if tally.tried == 0 && tally.skipped_unusable > 0 {
            return self
                .retry_or_fail(
                    &row,
                    AcquireFailure::ProviderUnavailable,
                    String::from("no source could be reached"),
                )
                .await;
        }
        let detail = tally.last_failure.map_or_else(
            || String::from("all candidates failed"),
            |last| format!("all candidates failed; last: {last}"),
        );
        self.retry_or_fail(&row, AcquireFailure::Exhausted, detail)
            .await
    }

    async fn current(&self) -> Result<Option<AcquisitionRow>, DbError> {
        let id = self.id.clone();
        self.inner.db.read(move |c| acquisitions::get(c, &id)).await
    }

    /// The candidate a resumed job was working on.
    async fn chosen(
        &self,
        row: &AcquisitionRow,
    ) -> Result<Option<(CandidateRow, Candidate)>, DbError> {
        let Some(token) = row.candidate.clone() else {
            return Ok(None);
        };
        let found = self
            .inner
            .db
            .read(move |c| acquisitions::candidate_by_token(c, &token))
            .await?;
        Ok(found.and_then(|r| candidate_of_row(&r).map(|c| (r, c))))
    }

    async fn untried(&self) -> Result<Vec<CandidateRow>, DbError> {
        let id = self.id.clone();
        let all = self
            .inner
            .db
            .read(move |c| acquisitions::candidates(c, &id))
            .await?;
        Ok(all.into_iter().filter(|c| c.tried_at.is_none()).collect())
    }

    async fn mark_tried(
        &self,
        token: &trackside_core::CandidateToken,
        why: &str,
    ) -> Result<(), DbError> {
        let token = token.clone();
        let why = why.to_owned();
        self.inner
            .db
            .write(move |tx| acquisitions::mark_tried(tx, &token, Some(&why)))
            .await
    }

    /// Searches, ranks and stores this attempt's candidates, the user's pick first. Returns
    /// them and how many providers answered.
    async fn search(&self, row: &AcquisitionRow) -> Result<(Vec<CandidateRow>, usize), DbError> {
        let query = SearchQuery {
            title: row.title.clone(),
            artist: row.artist.clone(),
            album: row.album.clone(),
            duration_ms: row.duration_ms,
            free_text: None,
        };
        let (found, answered) = self.inner.search_for_job(&query).await;
        let ranked = rank(&query, found, &self.inner.rank_options(row.allow_versions));
        let now = self.inner.now();
        let ttl = self.inner.timing.candidate_ttl;
        let mut rows: Vec<CandidateRow> =
            ranked.iter().map(|r| candidate_row(r, now, ttl)).collect();
        if let Some(token) = row.candidate_token.clone() {
            let picked = self
                .inner
                .db
                .read(move |c| acquisitions::candidate_by_token(c, &token))
                .await?;
            if let Some(mut picked) = picked {
                rows.retain(|r| r.token != picked.token);
                picked.score = PICKED_SCORE;
                picked.expires_at = picked.expires_at.max(now.saturating_add(ttl));
                rows.insert(0, picked);
            }
        }
        let id = self.id.clone();
        let stored = rows.clone();
        self.inner
            .db
            .write(move |tx| acquisitions::put_candidates(tx, Some(&id), &stored))
            .await?;
        Ok((rows, answered))
    }

    /// Fetches, verifies and imports one candidate. `None`: it failed, try the next.
    async fn try_candidate(
        &self,
        row: &AcquisitionRow,
        target: &TargetLibrary,
        cand_row: &CandidateRow,
        candidate: &Candidate,
        resume: bool,
        tally: &mut Tally,
    ) -> Result<Option<Result<(), Stop>>, DbError> {
        let inner = self.inner;
        let Some(slot) = inner.slot(candidate.provider) else {
            tally.skipped_unusable += 1;
            return Ok(None);
        };
        if !inner.usable(slot).await {
            tally.skipped_unusable += 1;
            return Ok(None);
        }
        let permit = tokio::select! {
            permit = Arc::clone(&slot.fetches).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return Ok(Some(Err(Stop::Shutdown))),
            },
            () = self.cancel.cancelled() => return Ok(self.stopped().map(Err)),
        };
        tally.tried += 1;
        let quality = quality_of_row(cand_row);
        if let Some(running) = lock(&inner.jobs).get_mut(&self.id) {
            running.quality = Some(quality);
        }
        if !resume {
            self.clean_staging().await;
        }
        tokio::fs::create_dir_all(&self.staging).await?;
        if self
            .update(AcquisitionUpdate {
                state: Some(AcquisitionState::Downloading),
                provider: Some(candidate.provider.as_str().to_owned()),
                candidate: Some(cand_row.token.clone()),
                staging_dir: Some(Some(self.staging.clone())),
                progress: Some((0, candidate.size)),
                ..AcquisitionUpdate::default()
            })
            .await?
            .is_none()
        {
            return Ok(Some(Err(Stop::Cancelled)));
        }
        slot.count(inner.now(), |s| s.fetches_today += 1);
        let fetched = self.fetch(slot, row, candidate, resume).await;
        drop(permit);
        let fetched = match fetched {
            Ok(fetched) => {
                lock(&slot.cooldown).succeed();
                fetched
            }
            Err(e) => {
                if let Some(stop) = self.stopped() {
                    return Ok(Some(Err(stop)));
                }
                slot.count(inner.now(), |s| s.failures_today += 1);
                if e.wants_cooldown() {
                    let retry_after = match &e {
                        ProviderError::RateLimited { retry_after } => *retry_after,
                        _ => None,
                    };
                    inner.trip_cooldown(slot, retry_after);
                }
                tracing::info!(provider = slot.name().as_str(), error = %e, "fetch failed; trying the next candidate");
                tally.last_failure = Some(e.to_string());
                self.mark_tried(&cand_row.token, &e.to_string()).await?;
                return Ok(None);
            }
        };
        match self.import(row, target, candidate, fetched).await? {
            Imported::Done => Ok(Some(Ok(()))),
            Imported::NoSpace(why) => {
                self.no_space(why).await?;
                Ok(Some(Ok(())))
            }
            Imported::Rejected(why) => {
                if let Some(stop) = self.stopped() {
                    return Ok(Some(Err(stop)));
                }
                tracing::info!(provider = slot.name().as_str(), reason = %why, "file rejected; trying the next candidate");
                tally.last_failure = Some(why.clone());
                self.mark_tried(&cand_row.token, &why).await?;
                Ok(None)
            }
        }
    }

    async fn fetch(
        &self,
        slot: &Slot,
        row: &AcquisitionRow,
        candidate: &Candidate,
        resume: bool,
    ) -> Result<FetchedFile, ProviderError> {
        let (sink, receiver) = ProgressSink::new();
        let forward = tokio::spawn(forward_progress(
            Arc::clone(self.inner),
            self.id.clone(),
            receiver,
        ));
        let token = self.cancel.clone();
        let result = if resume {
            slot.provider
                .resume(candidate, &self.staging, sink, token)
                .await
        } else if row.instant
            && let Some(streaming) =
                slot.provider
                    .fetch_streaming(candidate, &self.staging, sink.clone(), token.clone())
        {
            match streaming.await {
                Ok(streaming) => {
                    let duration = row.duration_ms.or(candidate.duration_ms).unwrap_or(0);
                    let hls = match self
                        .inner
                        .media
                        .start_hls(streaming.growing.clone(), duration, row.user)
                        .await
                    {
                        Ok(job) => job,
                        Err(e) => {
                            tracing::info!(error = %e, "no HLS for instant play; progressive only");
                            None
                        }
                    };
                    if let Some(running) = lock(&self.inner.jobs).get_mut(&self.id) {
                        running.stream = Some(InstantStream {
                            progressive: streaming.growing.clone(),
                            hls_job: hls,
                            content_type: "audio/mp4",
                            duration_ms: (duration > 0).then_some(duration),
                        });
                    }
                    self.publish_current().await;
                    streaming.done.await
                }
                Err(e) => Err(e),
            }
        } else {
            slot.provider
                .fetch(candidate, &self.staging, sink, token)
                .await
        };
        forward.abort();
        result
    }

    async fn publish_current(&self) {
        let id = self.id.clone();
        let rows = self
            .inner
            .db
            .read(move |c| {
                let mut rows: Vec<AcquisitionRow> =
                    acquisitions::get(c, &id)?.into_iter().collect();
                rows.extend(acquisitions::joined(c, &id)?);
                Ok(rows)
            })
            .await;
        for row in rows.unwrap_or_default() {
            self.inner.publish(&row, None);
        }
    }

    /// Verifies, tags and imports a fetched file, then finishes the request.
    #[expect(
        clippy::too_many_lines,
        reason = "verify, tag, move and import read best as one sequence"
    )]
    async fn import(
        &self,
        row: &AcquisitionRow,
        target: &TargetLibrary,
        candidate: &Candidate,
        fetched: FetchedFile,
    ) -> Result<Imported, DbError> {
        if self
            .update(AcquisitionUpdate {
                state: Some(AcquisitionState::Verifying),
                ..AcquisitionUpdate::default()
            })
            .await?
            .is_none()
        {
            return Ok(Imported::Rejected(String::from("cancelled")));
        }
        let expected = Expectation {
            duration_ms: row.duration_ms.or(candidate.duration_ms),
            max_bytes: self.inner.config.max_file_bytes,
        };
        let verified = match verify_with(
            &fetched.path,
            &self.staging,
            &expected,
            self.inner.media.as_ref(),
        )
        .await
        {
            Ok(verified) => verified,
            Err(rejection) => {
                let _ = tokio::fs::remove_file(&fetched.path).await;
                return Ok(Imported::Rejected(rejection.to_string()));
            }
        };
        if self
            .update(AcquisitionUpdate {
                state: Some(AcquisitionState::Importing),
                ..AcquisitionUpdate::default()
            })
            .await?
            .is_none()
        {
            return Ok(Imported::Rejected(String::from("cancelled")));
        }
        let album = row.album.clone().or_else(|| candidate.album.clone());
        let tags = TagValues {
            artist: row.artist.clone(),
            album_artist: album
                .as_ref()
                .map(|_| tracks_model::credits::lead(&row.artist)),
            album: album.clone(),
            title: row.title.clone(),
            disc: None,
            track: None,
            year: None,
            ext: verified.probe.container.as_str().to_owned(),
        };
        {
            let path = verified.path.clone();
            let tags = tags.clone();
            let cover = fetched.thumbnail.clone();
            let written =
                tokio::task::spawn_blocking(move || write_tags(&path, &tags, cover.as_deref()))
                    .await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::info!(error = %e, "could not tag the staged file; importing it as it is");
                }
                Err(e) => tracing::warn!(error = %e, "tagging panicked"),
            }
        }
        let template = if album.is_some() {
            self.inner.config.path_template.as_str()
        } else {
            SINGLES_TEMPLATE
        };
        let rel = safe_file_name(template, &tags);
        let moved = {
            let source = verified.path.clone();
            let root = target.root.clone();
            tokio::task::spawn_blocking(move || put_in_library(&source, &root, &rel)).await
        };
        let (dest, rel) = match moved {
            Ok(Ok(moved)) => moved,
            Ok(Err(e)) if is_no_space(&e) => return Ok(Imported::NoSpace(e.to_string())),
            Ok(Err(e)) => {
                return Ok(Imported::Rejected(format!(
                    "could not move into the library: {e}"
                )));
            }
            Err(e) => {
                return Ok(Imported::Rejected(format!(
                    "could not move into the library: {e}"
                )));
            }
        };
        let outcome = self
            .inner
            .scan
            .import_file(&target.id, &rel, SongOrigin::Acquired)
            .await;
        let (song, existing) = match outcome {
            Ok(ImportOutcome::Added(song)) => (song, false),
            Ok(ImportOutcome::Existing(song)) => {
                self.drop_duplicate(&song, target, &dest, &rel).await;
                (song, true)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&dest).await;
                return Ok(Imported::Rejected(format!("import failed: {e}")));
            }
        };
        // Staging goes only now: an instant play serves the staged file until it can follow
        // the request to its song.
        self.update(AcquisitionUpdate {
            state: Some(AcquisitionState::Done),
            song: Some(song),
            existing: Some(existing),
            lease_until: Some(None),
            staging_dir: Some(None),
            ..AcquisitionUpdate::default()
        })
        .await?;
        self.clean_staging().await;
        Ok(Imported::Done)
    }

    /// The library already had this audio: remove the copy just moved in, unless the
    /// existing song is that very file (the watcher may have indexed it first).
    async fn drop_duplicate(&self, song: &SongId, target: &TargetLibrary, dest: &Path, rel: &Path) {
        let song = song.clone();
        let location = self
            .inner
            .db
            .read(move |c| store::song_location(c, &song))
            .await;
        let same_file = matches!(
            location,
            Ok(Some((library, ref path))) if library == target.key && path.as_slice() == rel_bytes(rel).as_slice()
        );
        if !same_file && let Err(e) = tokio::fs::remove_file(dest).await {
            tracing::warn!(error = %e, "could not remove a duplicate acquired file");
        }
    }

    async fn no_space(&self, why: String) -> Result<(), DbError> {
        self.inner.disk_paused.store(true, Ordering::Relaxed);
        self.finish(
            AcquisitionState::Failed,
            Some(AcquireFailure::InsufficientSpace),
            Some(why),
        )
        .await
    }

    /// Schedules the next attempt with backoff, or fails once the attempts are used up.
    async fn retry_or_fail(
        &self,
        row: &AcquisitionRow,
        failure: AcquireFailure,
        detail: String,
    ) -> Result<(), DbError> {
        let backoff = &self.inner.timing.backoff;
        let attempts = row.attempts.max(1) as usize;
        if attempts > backoff.len() {
            return self
                .finish(AcquisitionState::Failed, Some(failure), Some(detail))
                .await;
        }
        let next = self.inner.now().saturating_add(backoff[attempts - 1]);
        self.clean_staging().await;
        self.update(AcquisitionUpdate {
            state: Some(AcquisitionState::Queued),
            failure_detail: Some(detail),
            next_attempt_at: Some(next),
            lease_until: Some(None),
            staging_dir: Some(None),
            ..AcquisitionUpdate::default()
        })
        .await?;
        Ok(())
    }
}

/// Writes progress to the row (and joined rows) and publishes it, at most once per interval.
async fn forward_progress(
    inner: Arc<Inner>,
    id: AcquisitionId,
    mut receiver: tokio::sync::watch::Receiver<crate::Progress>,
) {
    while receiver.changed().await.is_ok() {
        let progress = *receiver.borrow_and_update();
        let job_id = id.clone();
        let rows = inner
            .db
            .write(move |tx| {
                let Some(current) = acquisitions::get(tx, &job_id)? else {
                    return Ok(Vec::new());
                };
                if current.state.is_finished() {
                    return Ok(Vec::new());
                }
                let mut rows = vec![store::set_progress(
                    tx,
                    &job_id,
                    progress.bytes,
                    progress.total,
                )?];
                for joined in acquisitions::joined(tx, &job_id)? {
                    if !joined.state.is_finished() {
                        rows.push(store::set_progress(
                            tx,
                            &joined.id,
                            progress.bytes,
                            progress.total,
                        )?);
                    }
                }
                Ok(rows)
            })
            .await;
        for row in rows.unwrap_or_default() {
            inner.publish(&row, None);
        }
        tokio::time::sleep(inner.timing.progress_every).await;
    }
}

fn is_no_space(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(NO_SPACE) || e.kind() == std::io::ErrorKind::StorageFull
}

fn rel_bytes(rel: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        rel.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        rel.to_string_lossy().as_bytes().to_vec()
    }
}

/// Puts a verified file at `root/rel`, adding ` (2)` on collision, and leaves the source for
/// staging cleanup. Returns the absolute and relative paths.
fn put_in_library(source: &Path, root: &Path, rel: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let wanted = root.join(rel);
    if let Some(parent) = wanted.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest = unique_path(&wanted, |p| p.symlink_metadata().is_ok());
    if std::fs::hard_link(source, &dest).is_err() {
        // Across filesystems: copy to a hidden temporary name first, so a scan never sees a
        // partial file under the real name.
        let temp = dest.with_file_name(format!(
            ".{}.partial",
            dest.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        if let Err(e) = std::fs::copy(source, &temp).and_then(|_| std::fs::rename(&temp, &dest)) {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
    }
    let rel = dest
        .strip_prefix(root)
        .map_or_else(|_| rel.to_path_buf(), Path::to_path_buf);
    Ok((dest, rel))
}

/// The staged audio file a resumed job left: the largest regular `audio.*` file.
async fn find_staged(staging: &Path) -> Option<FetchedFile> {
    let staging = staging.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut best: Option<FetchedFile> = None;
        for entry in std::fs::read_dir(&staging).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let partial = Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("part"));
            if !name.starts_with("audio.") || partial {
                continue;
            }
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_file() && best.as_ref().is_none_or(|b| meta.len() > b.bytes) {
                best = Some(FetchedFile {
                    path: entry.path(),
                    bytes: meta.len(),
                    thumbnail: None,
                });
            }
        }
        best
    })
    .await
    .ok()
    .flatten()
}

/// Removes staging directories without an active request and `.part` files older than a day,
/// and prunes expired search candidates.
pub(crate) async fn janitor(inner: &Arc<Inner>) -> Result<(), DbError> {
    inner.load_external().await;
    inner.sweep_external().await;
    let active = inner.db.read(acquisitions::active_ids).await?;
    let root = inner.config.staging_dir.clone();
    let max_age = inner.timing.part_max_age;
    let removed = tokio::task::spawn_blocking(move || sweep(&root, &active, max_age))
        .await
        .map_err(|e| DbError::Io(std::io::Error::other(e)))??;
    if removed > 0 {
        tracing::info!(removed, "staging janitor removed leftovers");
    }
    let now = inner.now();
    inner
        .db
        .write(move |tx| acquisitions::prune_candidates(tx, now))
        .await?;
    Ok(())
}

fn sweep(root: &Path, active: &[AcquisitionId], part_max_age: Duration) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !active.iter().any(|id| id.as_str() == name) {
            let result = if is_dir {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => removed += 1,
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "could not remove a staging leftover");
                }
            }
            continue;
        }
        if is_dir {
            for file in std::fs::read_dir(&path)?.flatten() {
                let old = file
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > part_max_age);
                if file.file_name().to_string_lossy().ends_with(".part")
                    && old
                    && std::fs::remove_file(file.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

/// Cancels a request that has not finished. A joined request is cancelled alone; a request
/// others joined hands them on to the first of them.
pub(crate) async fn cancel_job(
    inner: &Arc<Inner>,
    row: &AcquisitionRow,
    actor: Option<UserKey>,
) -> Result<(), AcquireError> {
    if row.state.is_finished() {
        return Err(AcquireError::InvalidState);
    }
    let id = row.id.clone();
    let joined = row.joined_to.is_some();
    let rows = inner
        .db
        .write(move |tx| {
            let Some(current) = acquisitions::get(tx, &id)? else {
                return Ok(None);
            };
            if current.state.is_finished() {
                return Ok(None);
            }
            let mut rows = Vec::new();
            if !joined {
                let followers: Vec<AcquisitionRow> = acquisitions::joined(tx, &id)?
                    .into_iter()
                    .filter(|r| !r.state.is_finished())
                    .collect();
                if let Some(first) = followers.first() {
                    store::detach(tx, &first.id)?;
                    store::rejoin(tx, &id, &first.id)?;
                    rows.extend(acquisitions::get(tx, &first.id)?);
                }
            }
            let cancelled = acquisitions::transition(
                tx,
                &id,
                AcquisitionUpdate {
                    state: Some(AcquisitionState::Cancelled),
                    lease_until: Some(None),
                    staging_dir: Some(None),
                    ..AcquisitionUpdate::default()
                },
            )?;
            rows.insert(0, cancelled);
            Ok(Some(rows))
        })
        .await?
        .ok_or(AcquireError::InvalidState)?;
    for row in &rows {
        inner.publish(row, None);
    }
    let running = lock(&inner.jobs)
        .get(&row.id)
        .map(|r| (r.cancel.clone(), Arc::clone(&r.cancelled_by_user)));
    match running {
        Some((cancel, by_user)) => {
            by_user.store(true, Ordering::SeqCst);
            cancel.cancel();
        }
        None => {
            if let Some(dir) = &row.staging_dir
                && dir.starts_with(&inner.config.staging_dir)
            {
                let _ = tokio::fs::remove_dir_all(dir).await;
            }
        }
    }
    inner
        .audit(
            actor,
            "acquire.cancel",
            Some(row.id.as_str().to_owned()),
            serde_json::json!({ "title": row.title, "artist": row.artist }),
        )
        .await;
    inner.wake.notify_one();
    Ok(())
}
