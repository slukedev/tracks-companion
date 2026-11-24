//! HLS jobs: one ffmpeg run writes fMP4 segments while the server hands out a VOD playlist
//! written up front from the known duration. A request far ahead of the encoder starts a
//! second run at that point; a complete single run is remuxed into the cache.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Body;
use http::Response;
use http::request::Parts;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use trackside_core::records::SongFile;
use trackside_core::{PlaylistId, PublicId, random_id};

use crate::jobs::{estimate_bytes, feed_growing};
use crate::serve::{Entity, respond_file};
use crate::slots::SlotPermit;
use crate::{
    HlsInput, HlsJobId, HlsPart, INTERACTIVE_WAIT, MediaError, Priority, Profile, Shared,
    TranscodeInput, TranscodeOutput, TranscodeSpec,
};

/// How long a part request waits for the encoder.
pub(crate) const PART_WAIT: Duration = Duration::from_secs(10);
/// Jobs nobody asked about for this long are stopped and deleted.
pub(crate) const JOB_IDLE: Duration = Duration::from_mins(30);
/// A request further ahead of the encoder than this many segments starts a new run there.
const SEEK_AHEAD: u32 = 3;
const POLL: Duration = Duration::from_millis(100);
const MAX_FAILED_RUNS: usize = 3;
/// A last segment shorter than this is folded into the one before, since the encoder may
/// not produce it at all when the indexed duration is slightly long.
const SHORTEST_LAST: f64 = 1.0;

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RunExit {
    /// The encoder reached the end of the input.
    Finished,
    /// Stopped by us: overtaken, its slot given to a seek, or the job ended.
    Stopped,
    /// Never started, because no slot was free in time.
    Abandoned,
    Failed(String),
}

#[derive(Debug)]
struct Run {
    start: u32,
    next: u32,
    stop: u32,
    dir: PathBuf,
    alive: bool,
    exit: Option<RunExit>,
    scanned_after_exit: bool,
    cancel: CancellationToken,
    permit: Option<SlotPermit>,
}

#[derive(Debug)]
struct State {
    produced: Vec<Option<usize>>,
    runs: Vec<Run>,
    finalized: bool,
}

pub(crate) struct HlsJob {
    id: HlsJobId,
    dir: PathBuf,
    input: HlsInput,
    profile: Profile,
    segment: Duration,
    durations: Vec<f64>,
    state: Mutex<State>,
    progress: watch::Sender<u64>,
    last_access: Mutex<Instant>,
    cancel: CancellationToken,
    user: Option<trackside_core::UserKey>,
    hold: Mutex<Option<Box<dyn std::any::Any + Send + Sync>>>,
}

impl std::fmt::Debug for HlsJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HlsJob")
            .field("id", &self.id)
            .field("profile", &self.profile)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Segment lengths for a duration: full segments, then the remainder.
pub(crate) fn segment_durations(duration: Duration, segment: Duration) -> Vec<f64> {
    let total = duration.as_secs_f64().max(0.001);
    let seg = segment.as_secs_f64().max(0.5);
    let mut count = (total / seg).ceil().max(1.0) as usize;
    let mut last = total - (count - 1) as f64 * seg;
    if count > 1 && last < SHORTEST_LAST {
        count -= 1;
        last += seg;
    }
    let mut durations = vec![seg; count];
    durations[count - 1] = last;
    durations
}

impl HlsJob {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn count(&self) -> u32 {
        self.durations.len() as u32
    }

    fn touch(&self) {
        *self
            .last_access
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_access
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    fn bump(&self) {
        self.progress.send_modify(|v| *v += 1);
    }

    fn song_file(&self) -> Option<&SongFile> {
        match &self.input {
            HlsInput::File(file) => Some(file),
            HlsInput::Growing { .. } => None,
        }
    }
}

fn lookup(shared: &Shared, id: &HlsJobId) -> Result<Arc<HlsJob>, MediaError> {
    shared
        .hls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(id)
        .cloned()
        .ok_or(MediaError::JobExpired)
}

pub(crate) async fn start(
    shared: &Arc<Shared>,
    input: HlsInput,
    profile: Profile,
    priority: Priority,
    owner: crate::HlsOwner,
) -> Result<HlsJobId, MediaError> {
    if shared.cache.is_none() {
        return Err(MediaError::NoTranscoder);
    }
    if profile.output().1 != trackside_core::records::Codec::Aac
        || !shared.profiles.contains(&profile)
    {
        return Err(MediaError::Unsupported(format!(
            "hls profile {}",
            profile.as_str()
        )));
    }
    let duration_ms = match &input {
        HlsInput::File(file) => file.duration_ms,
        HlsInput::Growing { duration_ms, .. } => *duration_ms,
    };
    let permit = if priority == Priority::Interactive {
        tokio::time::timeout(INTERACTIVE_WAIT, shared.slots.acquire(priority))
            .await
            .map_err(|_| MediaError::TranscoderBusy)??
    } else {
        shared.slots.acquire(priority).await?
    };
    if let HlsInput::File(file) = &input {
        let need = estimate_bytes(file, profile);
        let room = {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                shared.cache.as_ref().is_some_and(|c| c.has_space_for(need))
            })
            .await
            .unwrap_or(false)
        };
        if !room {
            return Err(MediaError::InsufficientStorage);
        }
    }
    let raw = random_id::<PlaylistId>();
    let id = HlsJobId(format!("h{}", &raw.as_str()[1..]).into());
    let dir = shared.hls_dir.join(&*id.0);
    tokio::fs::create_dir_all(&dir).await?;
    let durations = segment_durations(
        Duration::from_millis(u64::from(duration_ms)),
        shared.config.hls_segment,
    );
    let count = durations.len();
    let job = Arc::new(HlsJob {
        id: id.clone(),
        dir,
        input,
        profile,
        segment: shared.config.hls_segment,
        durations,
        state: Mutex::new(State {
            produced: vec![None; count],
            runs: Vec::new(),
            finalized: false,
        }),
        progress: watch::Sender::new(0),
        last_access: Mutex::new(Instant::now()),
        cancel: shared.shutdown.child_token(),
        user: owner.user,
        hold: Mutex::new(owner.hold),
    });
    let index = add_run(&job, 0);
    if let Err(e) = launch_run(shared, &job, index, permit).await {
        job.cancel.cancel();
        let _ = tokio::fs::remove_dir_all(&job.dir).await;
        return Err(e);
    }
    shared
        .hls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.clone(), job.clone());
    tokio::spawn(monitor(shared.clone(), job));
    Ok(id)
}

/// Records a run starting at `start` and shortens runs that would overlap it.
fn add_run(job: &HlsJob, start: u32) -> usize {
    let mut state = job.lock();
    let stop = state
        .runs
        .iter()
        .filter(|r| r.alive && r.start > start)
        .map(|r| r.start)
        .min()
        .unwrap_or(job.count());
    for run in &mut state.runs {
        if run.alive && run.start < start && run.stop > start {
            run.stop = start;
        }
    }
    let index = state.runs.len();
    state.runs.push(Run {
        start,
        next: start,
        stop,
        dir: job.dir.join(format!("r{index}")),
        alive: true,
        exit: None,
        scanned_after_exit: false,
        cancel: job.cancel.child_token(),
        permit: None,
    });
    index
}

async fn launch_run(
    shared: &Arc<Shared>,
    job: &Arc<HlsJob>,
    index: usize,
    permit: SlotPermit,
) -> Result<(), MediaError> {
    let (start, dir, cancel) = {
        let mut state = job.lock();
        let run = &mut state.runs[index];
        run.permit = Some(permit);
        (run.start, run.dir.clone(), run.cancel.clone())
    };
    let started = async {
        tokio::fs::create_dir_all(&dir).await?;
        let offset = (start > 0).then(|| job.segment * start);
        let input = match &job.input {
            HlsInput::File(file) => TranscodeInput::Path(file.path.clone()),
            HlsInput::Growing { .. } => TranscodeInput::Stdin,
        };
        let mut process = shared.transcoder.spawn(TranscodeSpec {
            input,
            profile: job.profile,
            output: TranscodeOutput::Hls {
                dir: dir.clone(),
                segment: job.segment,
                start_number: start,
            },
            offset,
        })?;
        if let HlsInput::Growing { file, .. } = &job.input {
            let stdin = process
                .stdin()
                .ok_or_else(|| MediaError::Process("no stdin".into()))?;
            feed_growing(file, stdin);
        }
        Ok::<_, MediaError>(process)
    }
    .await;
    let process = match started {
        Ok(process) => process,
        Err(e) => {
            end_run(job, index, RunExit::Failed(e.to_string()));
            return Err(e);
        }
    };
    let job = job.clone();
    tokio::spawn(async move {
        let wait = process.wait();
        let result = tokio::select! {
            () = cancel.cancelled() => RunExit::Stopped,
            r = wait => match r {
                Ok(()) => RunExit::Finished,
                Err(e) => RunExit::Failed(e.to_string()),
            },
        };
        end_run(&job, index, result);
    });
    Ok(())
}

fn end_run(job: &HlsJob, index: usize, exit: RunExit) {
    {
        let mut state = job.lock();
        if exit == RunExit::Abandoned {
            let (start, stop) = (state.runs[index].start, state.runs[index].stop);
            for run in &mut state.runs {
                if run.alive && run.stop == start {
                    run.stop = stop;
                }
            }
        }
        let run = &mut state.runs[index];
        run.alive = false;
        run.permit = None;
        if exit == RunExit::Abandoned {
            run.scanned_after_exit = true;
        }
        if let RunExit::Failed(message) = &exit {
            tracing::warn!(job = &*job.id.0, %message, "hls encoder failed");
        }
        run.exit = Some(exit);
    }
    job.bump();
}

/// Learns which segments each run has finished. ffmpeg writes to a temporary name and
/// renames, so an existing segment file is complete.
async fn poll(job: &HlsJob) {
    let targets: Vec<(usize, PathBuf, u32, bool)> = {
        let state = job.lock();
        state
            .runs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.alive || !r.scanned_after_exit)
            .map(|(i, r)| (i, r.dir.clone(), r.next, r.alive))
            .collect()
    };
    let mut changed = false;
    for (index, dir, from, alive) in targets {
        let mut next = from;
        while next < job.count()
            && tokio::fs::try_exists(dir.join(format!("seg-{next}.m4s")))
                .await
                .unwrap_or(false)
        {
            next += 1;
        }
        let mut state = job.lock();
        for n in from..next {
            if state.produced[n as usize].is_none() {
                state.produced[n as usize] = Some(index);
            }
        }
        let run = &mut state.runs[index];
        changed |= next != run.next;
        run.next = next;
        if !alive {
            run.scanned_after_exit = true;
            changed = true;
        } else if next >= run.stop {
            run.cancel.cancel();
        }
    }
    if changed {
        job.bump();
    }
}

async fn monitor(shared: Arc<Shared>, job: Arc<HlsJob>) {
    loop {
        tokio::select! {
            () = job.cancel.cancelled() => break,
            () = tokio::time::sleep(POLL) => {}
        }
        poll(&job).await;
        maybe_finalize(&shared, &job);
        release_hold_when_idle(&job);
        if job.idle_for() >= JOB_IDLE {
            tracing::debug!(job = &*job.id.0, "hls job expired");
            break;
        }
    }
    job.cancel.cancel();
    shared
        .hls
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&job.id);
    let _ = tokio::fs::remove_dir_all(&job.dir).await;
}

/// When one run from the start produced every segment, the song is worth a cached file, so
/// the next play is direct. It is a fresh file transcode at prefetch priority rather than a
/// remux of the segments: fragmented MP4 carries no edit list, so a remux would keep the
/// encoder's 1024 priming samples and the last frame's padding and play with a gap. The file
/// transcode's edit list trims both, which is what Apple's players read for gapless playback.
fn maybe_finalize(shared: &Arc<Shared>, job: &Arc<HlsJob>) {
    let Some(file) = job.song_file() else { return };
    {
        let mut state = job.lock();
        if state.finalized {
            return;
        }
        let complete = state.runs.first().is_some_and(|r| {
            r.start == 0
                && r.scanned_after_exit
                && r.exit == Some(RunExit::Finished)
                && r.next >= job.count()
        });
        if !complete {
            return;
        }
        state.finalized = true;
    }
    if let Err(e) = crate::jobs::start_job(shared, file, job.profile, Priority::Prefetch) {
        tracing::debug!(job = &*job.id.0, error = %e, "no cache transcode after hls");
    }
}

/// The owner's hold (a transcode slot) is given back once no encoder is running for the job.
fn release_hold_when_idle(job: &HlsJob) {
    if job.lock().runs.iter().any(|r| r.alive) {
        return;
    }
    job.hold
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn run_count(shared: &Shared, id: &HlsJobId) -> Option<usize> {
    Some(lookup(shared, id).ok()?.lock().runs.len())
}

pub(crate) fn info(shared: &Shared, id: &HlsJobId) -> Option<crate::HlsJobInfo> {
    let job = lookup(shared, id).ok()?;
    Some(crate::HlsJobInfo {
        user: job.user,
        song: job.song_file().map(|f| f.song.clone()),
    })
}

pub(crate) fn playlist(shared: &Shared, id: &HlsJobId, suffix: &str) -> Result<String, MediaError> {
    let job = lookup(shared, id)?;
    job.touch();
    let target = job
        .durations
        .iter()
        .fold(0.0f64, |a, b| a.max(*b))
        .round()
        .max(1.0);
    let mut out = String::with_capacity(64 + job.durations.len() * 40);
    let _ = write!(
        out,
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"init.mp4{suffix}\"\n"
    );
    for (n, d) in job.durations.iter().enumerate() {
        let _ = write!(out, "#EXTINF:{d:.3},\nseg-{n}.m4s{suffix}\n");
    }
    out.push_str("#EXT-X-ENDLIST\n");
    Ok(out)
}

enum Next {
    Ready(PathBuf),
    Wait,
    StartRun,
    Gone,
    Failed(String),
}

fn check_segment(job: &HlsJob, n: u32) -> Next {
    let state = job.lock();
    if let Some(Some(run)) = state.produced.get(n as usize) {
        return Next::Ready(state.runs[*run].dir.join(format!("seg-{n}.m4s")));
    }
    let covered = state
        .runs
        .iter()
        .any(|r| r.alive && r.start <= n && n < r.stop && n <= r.next + SEEK_AHEAD);
    // A run that has just exited over this segment has written it, or will be known not to
    // have, once the monitor reads its folder; starting another encoder meanwhile only
    // repeats the work, and with a fast encoder over and over until that read.
    let unread = state
        .runs
        .iter()
        .any(|r| !r.alive && !r.scanned_after_exit && r.start <= n && n < r.stop);
    if covered || unread {
        return Next::Wait;
    }
    // A run that finished cleanly without reaching this segment has nothing more to give:
    // the encoder made fewer segments than the indexed duration suggested.
    let past_the_end = state.runs.iter().any(|r| {
        r.exit == Some(RunExit::Finished)
            && r.scanned_after_exit
            && r.start <= n
            && r.next <= n
            && n < r.stop
    });
    if past_the_end {
        return Next::Gone;
    }
    let failed: Vec<&str> = state.runs.iter().filter_map(failure).collect();
    if failed.len() >= MAX_FAILED_RUNS {
        return Next::Failed(failed.join("; "));
    }
    Next::StartRun
}

fn failure(run: &Run) -> Option<&str> {
    match &run.exit {
        Some(RunExit::Failed(message)) => Some(message.as_str()),
        _ => None,
    }
}

fn check_init(job: &HlsJob) -> Next {
    let state = job.lock();
    if let Some(run) = state.runs.iter().find(|r| r.next > r.start) {
        return Next::Ready(run.dir.join("init.mp4"));
    }
    if state.runs.iter().any(|r| r.alive || !r.scanned_after_exit) {
        return Next::Wait;
    }
    let failed: Vec<&str> = state.runs.iter().filter_map(failure).collect();
    Next::Failed(if failed.is_empty() {
        "the encoder produced nothing".into()
    } else {
        failed.join("; ")
    })
}

/// A slot for a seek-ahead run: a free one, else the slot of this job's run that the
/// listener has left behind, else one that frees up before the deadline.
async fn seek_permit(
    shared: &Shared,
    job: &HlsJob,
    n: u32,
    deadline: Instant,
) -> Result<SlotPermit, MediaError> {
    if let Some(permit) = shared.slots.try_acquire() {
        return Ok(permit);
    }
    {
        let mut state = job.lock();
        let victim = state
            .runs
            .iter_mut()
            .filter(|r| r.alive && r.permit.is_some() && r.next < n)
            .min_by_key(|r| r.next);
        if let Some(run) = victim {
            run.cancel.cancel();
            if let Some(permit) = run.permit.take() {
                return Ok(permit);
            }
        }
    }
    tokio::time::timeout_at(deadline, shared.slots.acquire(Priority::Interactive))
        .await
        .map_err(|_| MediaError::Timeout)?
}

pub(crate) async fn part(
    shared: &Arc<Shared>,
    id: &HlsJobId,
    part: HlsPart,
    request: &Parts,
) -> Result<Response<Body>, MediaError> {
    let job = lookup(shared, id)?;
    job.touch();
    if let HlsPart::Segment(n) = part
        && n >= job.count()
    {
        return Err(MediaError::NotFound);
    }
    let deadline = Instant::now() + PART_WAIT;
    let mut progress = job.progress.subscribe();
    let path = loop {
        progress.borrow_and_update();
        let next = match part {
            HlsPart::Init => check_init(&job),
            HlsPart::Segment(n) => check_segment(&job, n),
        };
        match next {
            Next::Ready(path) => break path,
            Next::Gone => return Err(MediaError::NotFound),
            Next::Failed(message) => return Err(MediaError::Process(message)),
            Next::StartRun => {
                if let HlsPart::Segment(n) = part {
                    tracing::debug!(
                        job = &*job.id.0,
                        segment = n,
                        "starting a run ahead of the encoder"
                    );
                    let index = add_run(&job, n);
                    let permit = match seek_permit(shared, &job, n, deadline).await {
                        Ok(permit) => permit,
                        Err(e) => {
                            end_run(&job, index, RunExit::Abandoned);
                            return Err(e);
                        }
                    };
                    launch_run(shared, &job, index, permit).await?;
                }
            }
            Next::Wait => {}
        }
        tokio::select! {
            _ = progress.changed() => {}
            () = tokio::time::sleep_until(deadline) => return Err(MediaError::Timeout),
            () = job.cancel.cancelled() => return Err(MediaError::JobExpired),
        }
        if Instant::now() >= deadline {
            return Err(MediaError::Timeout);
        }
    };
    let handle = tokio::fs::File::open(&path).await?;
    let meta = handle.metadata().await?;
    let etag = match part {
        HlsPart::Init => format!("\"{}.init\"", job.id.0),
        HlsPart::Segment(n) => format!("\"{}.{n}\"", job.id.0),
    };
    let entity = Entity {
        len: meta.len(),
        etag: &etag,
        last_modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
        content_type: "audio/mp4",
        cache_control: "private, max-age=3600",
        disposition: None,
    };
    respond_file(handle, &entity, request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_durations_fold_a_tiny_tail() {
        let six = Duration::from_secs(6);
        assert_eq!(
            segment_durations(Duration::from_millis(13_500), six),
            vec![6.0, 6.0, 1.5]
        );
        let folded = segment_durations(Duration::from_millis(12_400), six);
        assert_eq!(folded.len(), 2);
        assert!((folded[1] - 6.4).abs() < 1e-9);
        assert_eq!(segment_durations(Duration::from_millis(500), six).len(), 1);
        assert_eq!(segment_durations(Duration::from_secs(240), six).len(), 40);
    }
}
