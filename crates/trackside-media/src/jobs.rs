//! Single-flight file transcodes into the cache, progressive streaming, growing files and
//! decode checks.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use axum::body::Body;
use http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use http::request::Parts;
use http::{Response, StatusCode};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use trackside_core::records::{Container, SongFile};

use crate::cache::{CacheKey, Route};
use crate::serve::{
    Entity, GrowSource, WatchSource, growing_body, http_error, is_head, pump_reader, reader_body,
    respond_file,
};
use crate::{
    CachedTranscode, DecodeCheck, GrowState, GrowingFile, INTERACTIVE_WAIT, MediaError,
    PrepareState, PrepareStateKind, Priority, Profile, Shared, TranscodeInput, TranscodeOutput,
    TranscodeSpec,
};

/// How often a reader of a file being written checks for more bytes.
const POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub(crate) enum JobError {
    NoTranscoder,
    Storage,
    Failed(Arc<str>),
}

impl From<JobError> for MediaError {
    fn from(e: JobError) -> Self {
        match e {
            JobError::NoTranscoder => Self::NoTranscoder,
            JobError::Storage => Self::InsufficientStorage,
            JobError::Failed(message) => Self::Process(message.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum JobState {
    Queued,
    Running,
    Done(Result<CachedTranscode, JobError>),
}

/// A transcode into the cache that any number of requests can wait on.
#[derive(Debug)]
pub(crate) struct FileJob {
    pub part: PathBuf,
    pub final_path: PathBuf,
    pub state: watch::Sender<JobState>,
    priority: watch::Sender<Priority>,
}

/// Bytes a transcode is expected to take, for space checks and length estimates.
pub(crate) fn estimate_bytes(file: &SongFile, profile: Profile) -> u64 {
    profile.bitrate_kbps().map_or(file.size, |kbps| {
        u64::from(kbps) * u64::from(file.duration_ms) / 8
    })
}

fn job_timeout(file: &SongFile) -> Duration {
    Duration::from_secs(120) + Duration::from_millis(u64::from(file.duration_ms)) * 2
}

/// Finds or starts the job for this transcode, raising its priority if needed.
pub(crate) fn start_job(
    shared: &Arc<Shared>,
    file: &SongFile,
    profile: Profile,
    priority: Priority,
) -> Result<Arc<FileJob>, MediaError> {
    let cache = shared.cache.as_ref().ok_or(MediaError::NoTranscoder)?;
    let key = CacheKey::of(file, profile);
    let mut jobs = shared.jobs.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(job) = jobs.get(&key) {
        job.priority.send_if_modified(|p| {
            let raise = priority < *p;
            if raise {
                *p = priority;
            }
            raise
        });
        return Ok(job.clone());
    }
    let (priority_tx, priority_rx) = watch::channel(priority);
    let job = Arc::new(FileJob {
        part: cache.part_path(&key, Route::Direct),
        final_path: cache.dir().join(cache.file_name(&key, Route::Direct)),
        state: watch::Sender::new(JobState::Queued),
        priority: priority_tx,
    });
    jobs.insert(key.clone(), job.clone());
    drop(jobs);
    tokio::spawn(run_job(
        shared.clone(),
        job.clone(),
        key,
        file.clone(),
        priority_rx,
    ));
    Ok(job)
}

async fn run_job(
    shared: Arc<Shared>,
    job: Arc<FileJob>,
    key: CacheKey,
    file: SongFile,
    priority: watch::Receiver<Priority>,
) {
    let result = transcode_to_cache(&shared, &job, &key, &file, priority).await;
    if let Err(JobError::Failed(message)) = &result {
        tracing::warn!(song = %file.song, profile = key.profile.as_str(), %message, "transcode failed");
        shared.record_failure(&file);
    }
    {
        let mut jobs = shared.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        if jobs.get(&key).is_some_and(|j| Arc::ptr_eq(j, &job)) {
            jobs.remove(&key);
        }
    }
    job.state.send_replace(JobState::Done(result));
}

async fn acquire_by_priority(
    shared: &Shared,
    mut priority: watch::Receiver<Priority>,
) -> Option<crate::slots::SlotPermit> {
    loop {
        let current = *priority.borrow_and_update();
        tokio::select! {
            biased;
            () = shared.shutdown.cancelled() => return None,
            permit = shared.slots.acquire(current) => return permit.ok(),
            Ok(()) = priority.changed() => {}
        }
    }
}

async fn transcode_to_cache(
    shared: &Arc<Shared>,
    job: &FileJob,
    key: &CacheKey,
    file: &SongFile,
    priority: watch::Receiver<Priority>,
) -> Result<CachedTranscode, JobError> {
    let Some(_permit) = acquire_by_priority(shared, priority).await else {
        return Err(JobError::Failed("shutting down".into()));
    };
    let cache = shared.cache.as_ref().ok_or(JobError::NoTranscoder)?;
    if let Some(done) = cache.get(file, key.profile) {
        return Ok(done);
    }
    let need = estimate_bytes(file, key.profile);
    let room = {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || {
            shared.cache.as_ref().is_some_and(|c| c.has_space_for(need))
        })
        .await
        .unwrap_or(false)
    };
    if !room {
        return Err(JobError::Storage);
    }
    let process = shared
        .transcoder
        .spawn(TranscodeSpec {
            input: TranscodeInput::Path(file.path.clone()),
            profile: key.profile,
            output: TranscodeOutput::File(job.part.clone()),
            offset: None,
        })
        .map_err(|e| match e {
            MediaError::NoTranscoder => JobError::NoTranscoder,
            other => JobError::Failed(other.to_string().into()),
        })?;
    // Running means bytes are on their way, so progressive readers may start reading.
    job.state.send_replace(JobState::Running);
    let outcome = tokio::select! {
        () = shared.shutdown.cancelled() => Err(MediaError::Process("shutting down".into())),
        r = tokio::time::timeout(job_timeout(file), process.wait()) => {
            r.unwrap_or_else(|_| Err(MediaError::Process("transcode timed out".into())))
        }
    };
    if let Err(e) = outcome {
        let _ = tokio::fs::remove_file(&job.part).await;
        let floor = cache.min_free();
        let low = {
            let shared = shared.clone();
            tokio::task::spawn_blocking(move || {
                shared.cache.as_ref().is_some_and(|c| c.free_now() < floor)
            })
            .await
            .unwrap_or(false)
        };
        return Err(if low {
            JobError::Storage
        } else {
            JobError::Failed(e.to_string().into())
        });
    }
    let committed = {
        let shared = shared.clone();
        let key = key.clone();
        let part = job.part.clone();
        tokio::task::spawn_blocking(move || {
            shared
                .cache
                .as_ref()
                .map(|c| c.commit(&key, Route::Direct, &part))
        })
        .await
    };
    match committed {
        Ok(Some(Ok(_))) => cache
            .get(file, key.profile)
            .ok_or_else(|| JobError::Failed("evicted at once".into())),
        Ok(Some(Err(e))) => {
            let _ = tokio::fs::remove_file(&job.part).await;
            Err(JobError::Failed(e.to_string().into()))
        }
        Ok(None) => Err(JobError::NoTranscoder),
        Err(e) => Err(JobError::Failed(e.to_string().into())),
    }
}

/// Waits for a job. Interactive callers give up after 2 s in the queue.
async fn wait_job(job: &FileJob, priority: Priority) -> Result<CachedTranscode, MediaError> {
    let mut rx = job.state.subscribe();
    if priority == Priority::Interactive {
        let started = tokio::time::timeout(
            INTERACTIVE_WAIT,
            rx.wait_for(|s| !matches!(s, JobState::Queued)),
        )
        .await;
        if started.is_err() {
            return Err(MediaError::TranscoderBusy);
        }
    }
    let state = rx
        .wait_for(|s| matches!(s, JobState::Done(_)))
        .await
        .map_err(|_| MediaError::Process("transcode job vanished".into()))?
        .clone();
    match state {
        JobState::Done(result) => result.map_err(MediaError::from),
        JobState::Queued | JobState::Running => {
            Err(MediaError::Process("transcode job vanished".into()))
        }
    }
}

pub(crate) async fn ensure_cached(
    shared: &Arc<Shared>,
    file: &SongFile,
    profile: Profile,
    priority: Priority,
) -> Result<CachedTranscode, MediaError> {
    let cache = shared.cache.as_ref().ok_or(MediaError::NoTranscoder)?;
    if !shared.profiles.contains(&profile) {
        return Err(MediaError::Unsupported(format!(
            "profile {}",
            profile.as_str()
        )));
    }
    if let Some(done) = cache.get(file, profile) {
        return Ok(done);
    }
    if shared.is_failing(file) {
        return Err(MediaError::Process(
            "this song failed to transcode recently".into(),
        ));
    }
    let job = start_job(shared, file, profile, priority)?;
    wait_job(&job, priority).await
}

pub(crate) fn prepare(
    shared: &Arc<Shared>,
    files: &[SongFile],
    profile: Profile,
) -> Vec<PrepareState> {
    let Some(cache) = shared.cache.as_ref() else {
        return Vec::new();
    };
    if !shared.profiles.contains(&profile) {
        return Vec::new();
    }
    files
        .iter()
        .take(5)
        .filter_map(|file| {
            let state = if cache.get(file, profile).is_some() {
                PrepareStateKind::Cached
            } else if shared.is_failing(file) {
                return None;
            } else {
                let job = start_job(shared, file, profile, Priority::Prefetch).ok()?;
                let running = matches!(*job.state.borrow(), JobState::Running);
                if running {
                    PrepareStateKind::Running
                } else {
                    PrepareStateKind::Queued
                }
            };
            Some(PrepareState {
                song: file.song.clone(),
                state,
            })
        })
        .collect()
}

/// A cache job seen as a growing file.
struct JobSource {
    _job: Arc<FileJob>,
    rx: watch::Receiver<JobState>,
    final_path: PathBuf,
}

impl GrowSource for JobSource {
    fn state(&mut self) -> Option<GrowState> {
        Some(match &*self.rx.borrow_and_update() {
            JobState::Queued | JobState::Running => GrowState::Growing { bytes: 0 },
            JobState::Done(Ok(done)) => GrowState::Complete { bytes: done.size },
            JobState::Done(Err(_)) => GrowState::Failed,
        })
    }

    async fn changed(&mut self) {
        tokio::select! {
            _ = self.rx.changed() => {}
            () = tokio::time::sleep(POLL) => {}
        }
    }

    fn final_path(&self) -> Option<PathBuf> {
        Some(self.final_path.clone())
    }
}

fn streaming_response(
    content_type: &'static str,
    length: Option<u64>,
    parts: &Parts,
    body: impl FnOnce() -> Body,
) -> Result<Response<Body>, MediaError> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(ACCEPT_RANGES, "none")
        .header(CACHE_CONTROL, "no-store");
    if let Some(length) = length {
        builder = builder.header(CONTENT_LENGTH, length);
    }
    let body = if is_head(parts) {
        Body::empty()
    } else {
        body()
    };
    builder.body(body).map_err(|e| http_error(&e))
}

fn get_parts() -> Parts {
    http::Request::new(()).into_parts().0
}

pub(crate) async fn progressive(
    shared: &Arc<Shared>,
    file: &SongFile,
    profile: Profile,
    offset: Option<Duration>,
    estimate_length: bool,
) -> Result<Response<Body>, MediaError> {
    let cache = shared.cache.as_ref().ok_or(MediaError::NoTranscoder)?;
    if !shared.profiles.contains(&profile) {
        return Err(MediaError::Unsupported(format!(
            "profile {}",
            profile.as_str()
        )));
    }
    let parts = get_parts();
    let estimate = estimate_length.then(|| estimate_bytes(file, profile));
    let (container, _) = profile.output();
    if offset.is_none() {
        if let Some(done) = cache.get(file, profile) {
            let handle = tokio::fs::File::open(&done.path).await?;
            let len = done.size;
            return streaming_response(done.format.content_type, Some(len), &parts, || {
                reader_body(handle, Some(len))
            });
        }
        // MP4 with faststart is rewritten at the end, so it cannot be read while written;
        // the other containers only patch their header in place.
        if container != Container::M4a && !shared.is_failing(file) {
            let job = start_job(shared, file, profile, Priority::Interactive)?;
            let mut rx = job.state.subscribe();
            let started = tokio::time::timeout(
                INTERACTIVE_WAIT,
                rx.wait_for(|s| !matches!(s, JobState::Queued)),
            )
            .await;
            let state = match started {
                Ok(Ok(state)) => state.clone(),
                _ => return Err(MediaError::TranscoderBusy),
            };
            match state {
                JobState::Done(Err(JobError::Storage)) => {}
                JobState::Done(Err(e)) => return Err(e.into()),
                _ => {
                    let source = JobSource {
                        final_path: job.final_path.clone(),
                        rx: job.state.subscribe(),
                        _job: job.clone(),
                    };
                    let part = job.part.clone();
                    return streaming_response(container.content_type(), estimate, &parts, || {
                        growing_body(part, source, estimate)
                    });
                }
            }
        }
    }
    stream_stdout(shared, file, profile, offset, estimate, &parts).await
}

/// Streams ffmpeg's stdout without caching; ffmpeg is killed when the client goes away.
async fn stream_stdout(
    shared: &Arc<Shared>,
    file: &SongFile,
    profile: Profile,
    offset: Option<Duration>,
    estimate: Option<u64>,
    parts: &Parts,
) -> Result<Response<Body>, MediaError> {
    let permit = tokio::time::timeout(
        INTERACTIVE_WAIT,
        shared.slots.acquire(Priority::Interactive),
    )
    .await
    .map_err(|_| MediaError::TranscoderBusy)??;
    let mut process = shared.transcoder.spawn(TranscodeSpec {
        input: TranscodeInput::Path(file.path.clone()),
        profile,
        output: TranscodeOutput::Stdout,
        offset,
    })?;
    let stdout = process
        .stdout()
        .ok_or_else(|| MediaError::Process("no stdout".into()))?;
    let (tx, mut rx) = mpsc::channel(2);
    let shutdown = shared.shutdown.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let wait = process.wait();
        tokio::select! {
            () = shutdown.cancelled() => {}
            completed = pump_reader(stdout, estimate, tx) => {
                if completed {
                    let _ = tokio::time::timeout(Duration::from_secs(10), wait).await;
                }
            }
        }
    });
    let content_type = profile.output().0.content_type();
    streaming_response(content_type, estimate, parts, move || {
        Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)))
    })
}

fn growing_etag(path: &Path, meta: &std::fs::Metadata) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_os_str().as_encoded_bytes());
    hasher.update(&meta.len().to_le_bytes());
    hasher.update(&crate::serve::mtime_ns(meta).to_le_bytes());
    format!("\"g{}\"", &hasher.finalize().to_hex()[..20])
}

pub(crate) async fn serve_growing(
    file: &GrowingFile,
    request: &Parts,
    content_type: &'static str,
) -> Result<Response<Body>, MediaError> {
    let state = *file.state.borrow();
    match state {
        GrowState::Failed => Err(MediaError::NotFound),
        GrowState::Complete { .. } => {
            let handle = match tokio::fs::File::open(&file.path).await {
                Ok(h) => h,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(MediaError::NotFound);
                }
                Err(e) => return Err(e.into()),
            };
            let meta = handle.metadata().await?;
            let etag = growing_etag(&file.path, &meta);
            let entity = Entity {
                len: meta.len(),
                etag: &etag,
                last_modified: meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                content_type,
                cache_control: "private, no-cache",
                disposition: None,
            };
            respond_file(handle, &entity, request).await
        }
        GrowState::Growing { .. } => {
            let path = file.path.clone();
            let source = WatchSource(file.state.clone());
            streaming_response(content_type, None, request, || {
                growing_body(path, source, None)
            })
        }
    }
}

/// Feeds a growing file into a process's stdin until it completes.
pub(crate) fn feed_growing(
    file: &GrowingFile,
    mut stdin: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
) {
    let (tx, mut rx) = mpsc::channel(2);
    tokio::spawn(crate::serve::pump_growing(
        file.path.clone(),
        WatchSource(file.state.clone()),
        None,
        tx,
    ));
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(Ok(chunk)) = rx.recv().await {
            if stdin.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = stdin.shutdown().await;
    });
}

const DECODE_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) async fn decode_check(
    shared: &Arc<Shared>,
    path: &Path,
) -> Result<DecodeCheck, MediaError> {
    if shared.cache.is_none() {
        return Err(MediaError::NoTranscoder);
    }
    let _permit = shared.slots.acquire(Priority::Prefetch).await?;
    let mut process = shared.transcoder.spawn(TranscodeSpec {
        input: TranscodeInput::Path(path.to_path_buf()),
        profile: Profile::Flac,
        output: TranscodeOutput::Null,
        offset: None,
    })?;
    let stdout = process.stdout();
    let read = async move {
        let mut text = String::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_string(&mut text).await;
        }
        text
    };
    let run = async { tokio::join!(read, process.wait()) };
    let (progress, result) = tokio::time::timeout(DECODE_TIMEOUT, run)
        .await
        .map_err(|_| MediaError::Timeout)?;
    let decoded_ms = progress
        .lines()
        .filter_map(|line| line.strip_prefix("out_time_us="))
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .next_back()
        .map(|us| u32::try_from(us / 1000).unwrap_or(u32::MAX));
    Ok(match result {
        Ok(()) => DecodeCheck {
            ok: true,
            decoded_ms,
            message: None,
        },
        Err(MediaError::Process(message)) => DecodeCheck {
            ok: false,
            decoded_ms,
            message: Some(message),
        },
        Err(e) => return Err(e),
    })
}
