//! yt-dlp as a subprocess (architecture section 9.4).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use trackside_core::config::YtdlpConfig;
use trackside_core::control::{GrowState, GrowingFile};
use trackside_core::{BoxFuture, Clock, Millis, SystemClock};

use crate::process::{self, RunError, push_tail};
use crate::provider::{
    Candidate, FetchedFile, Progress, ProgressSink, Provider, ProviderError, ProviderHealth,
    ProviderName, SearchQuery, SourceHints, StreamingFetch,
};

const SEARCH_RESULTS: u32 = 8;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(60);
/// A fetch with no output for this long is stalled.
const STALL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_JSON: usize = 4 * 1024 * 1024;
/// The output name inside staging; yt-dlp fills in the extension.
pub const OUTPUT_STEM: &str = "audio";
/// Where a stream-through fetch writes, since the format is known only from the bytes.
pub const STREAM_FILE: &str = "audio.stream";
/// The health detail when no yt-dlp can run on this install.
pub const NEEDS_ACQUIRE_IMAGE: &str = "Needs the acquire image";

/// A file's modification time and size, to notice when it is replaced.
type FileStamp = (Option<SystemTime>, u64);

/// yt-dlp as a subprocess.
#[derive(Clone, Debug)]
pub struct YtdlpProvider {
    config: YtdlpConfig,
    data_dir: PathBuf,
    max_file_bytes: u64,
    stall_timeout: Duration,
    clock: Arc<dyn Clock>,
    updated_runs: Arc<Mutex<Option<(FileStamp, bool)>>>,
}

impl YtdlpProvider {
    pub fn new(config: &YtdlpConfig, data_dir: &Path, max_file_bytes: u64) -> Self {
        Self {
            config: config.clone(),
            data_dir: data_dir.to_path_buf(),
            max_file_bytes,
            stall_timeout: STALL_TIMEOUT,
            clock: Arc::new(SystemClock),
            updated_runs: Arc::default(),
        }
    }

    /// Uses `clock` for the version age check.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Treats a fetch as stalled after `timeout` without output.
    #[must_use]
    pub fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = timeout;
        self
    }

    /// The executable: the configured path, else `<data>/bin/yt-dlp` when the updater put one
    /// there that runs, else `yt-dlp` on `PATH`.
    pub fn executable(&self) -> PathBuf {
        if let Some(path) = &self.config.path {
            return path.clone();
        }
        let updated = updated_binary(&self.data_dir);
        if self.updated_runs(&updated) {
            return updated;
        }
        PathBuf::from("yt-dlp")
    }

    fn updated_runs(&self, path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        let stamp = (meta.modified().ok(), meta.len());
        let mut cached = self
            .updated_runs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match *cached {
            Some((seen, runs)) if seen == stamp => runs,
            _ => {
                let runs = process::can_exec(path);
                *cached = Some((stamp, runs));
                runs
            }
        }
    }

    /// What a failed spawn means. With no configured path, nothing on this install can run
    /// yt-dlp, which is a setup problem rather than an outage.
    fn spawn_failure(&self, e: std::io::Error, program: &Path) -> ProviderError {
        let missing = e.kind() == std::io::ErrorKind::NotFound;
        let cannot_run = process::cannot_exec(&e);
        if self.config.path.is_none() && (missing || cannot_run) {
            ProviderError::NotSetUp(String::from(NEEDS_ACQUIRE_IMAGE))
        } else if missing {
            ProviderError::Unavailable(format!("yt-dlp not found at {}", program.display()))
        } else if cannot_run {
            ProviderError::Unavailable(format!("{} cannot run on this system", program.display()))
        } else {
            ProviderError::Io(e)
        }
    }

    /// `<data>/tmp`, passed as `TMPDIR`: the container's root filesystem is read-only, and a
    /// one-file yt-dlp from the updater unpacks itself into the temporary directory.
    pub fn tmp_dir(&self) -> PathBuf {
        tmp_dir(&self.data_dir)
    }

    fn common_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["--ignore-config".into(), "--no-plugin-dirs".into()];
        if let Some(cookies) = &self.config.cookies_file {
            args.push("--cookies".into());
            args.push(cookies.into());
        }
        if let Some(runtime) = &self.config.js_runtime {
            let kind = runtime_kind(runtime);
            let mut value = OsString::from(format!("{kind}:"));
            value.push(runtime);
            args.push("--js-runtimes".into());
            args.push(value);
        }
        args
    }

    /// `--flat-playlist -J "ytsearch8:<text>"`.
    pub fn search_args(&self, text: &str) -> Vec<OsString> {
        let mut args = self.common_args();
        args.extend([
            "--flat-playlist".into(),
            "-J".into(),
            format!("ytsearch{SEARCH_RESULTS}:{text}").into(),
        ]);
        args
    }

    /// The download: best audio, preferring m4a, extracted without re-encoding.
    pub fn fetch_args(&self, url: &str, staging: &Path, resume: bool) -> Vec<OsString> {
        let mut args = self.common_args();
        args.extend([
            "--no-playlist".into(),
            "-f".into(),
            "bestaudio[ext=m4a]/bestaudio".into(),
            "-x".into(),
            "--embed-metadata".into(),
            "--embed-thumbnail".into(),
            "--max-filesize".into(),
            self.max_file_bytes.to_string().into(),
            "--newline".into(),
            "--progress-template".into(),
            "%(progress)j".into(),
        ]);
        if resume {
            args.push("--continue".into());
        }
        args.extend([
            "-P".into(),
            staging.into(),
            "-o".into(),
            format!("{OUTPUT_STEM}.%(ext)s").into(),
            url.into(),
        ]);
        args
    }

    /// The stream-through download to stdout.
    pub fn stream_args(&self, url: &str) -> Vec<OsString> {
        let mut args = self.common_args();
        args.extend([
            "--no-playlist".into(),
            "-f".into(),
            "bestaudio[ext=m4a]/bestaudio".into(),
            "--max-filesize".into(),
            self.max_file_bytes.to_string().into(),
            "--newline".into(),
            "--progress-template".into(),
            "%(progress)j".into(),
            "-o".into(),
            "-".into(),
            url.into(),
        ]);
        args
    }

    /// The cover for a stream-through download, fetched afterwards.
    pub fn thumbnail_args(&self, url: &str, staging: &Path) -> Vec<OsString> {
        let mut args = self.common_args();
        args.extend([
            "--no-playlist".into(),
            "--skip-download".into(),
            "--write-thumbnail".into(),
            "--convert-thumbnails".into(),
            "jpg".into(),
            "-P".into(),
            staging.into(),
            "-o".into(),
            "cover.%(ext)s".into(),
            url.into(),
        ]);
        args
    }

    async fn fetch_inner(
        &self,
        candidate: &Candidate,
        staging: &Path,
        progress: ProgressSink,
        cancel: CancellationToken,
        resume: bool,
    ) -> Result<FetchedFile, ProviderError> {
        let url = candidate_url(candidate)?;
        tokio::fs::create_dir_all(staging).await?;
        let args = self.fetch_args(&url, staging, resume);
        let program = self.executable();
        let mut child = process::spawn(&program, &args, true, Some(&self.tmp_dir()))
            .await
            .map_err(|e| self.spawn_failure(e, &program))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stderr_task = tokio::spawn(process::keep_tail(stderr));
        let mut said_too_large = false;
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = tokio::select! {
                    () = cancel.cancelled() => {
                        let _ = child.kill().await;
                        return Err(ProviderError::Cancelled);
                    }
                    line = tokio::time::timeout(self.stall_timeout, lines.next_line()) => line,
                };
                match line {
                    Err(_) => {
                        let _ = child.kill().await;
                        return Err(ProviderError::Stalled);
                    }
                    Ok(Ok(Some(line))) => {
                        if let Some(p) = parse_progress(&line) {
                            progress.report(p.bytes, p.total);
                        } else if line.contains("larger than max-filesize") {
                            said_too_large = true;
                        }
                    }
                    Ok(Ok(None) | Err(_)) => break,
                }
            }
        }
        let status = tokio::select! {
            () = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(ProviderError::Cancelled);
            }
            status = child.wait() => status?,
        };
        let stderr = stderr_task.await.unwrap_or_default();
        if !status.success() {
            return Err(classify(&stderr));
        }
        if said_too_large || stderr.contains("larger than max-filesize") {
            return Err(ProviderError::TooLarge);
        }
        let staging = staging.to_path_buf();
        let found = tokio::task::spawn_blocking(move || find_output(&staging))
            .await
            .map_err(|e| ProviderError::Other(e.to_string()))??;
        let Some((path, bytes)) = found else {
            return Err(ProviderError::Other(String::from(
                "yt-dlp finished without writing a file",
            )));
        };
        progress.report(bytes, Some(bytes));
        Ok(FetchedFile {
            path,
            bytes,
            thumbnail: None,
        })
    }

    async fn stream_inner(
        &self,
        candidate: &Candidate,
        staging: &Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> Result<StreamingFetch, ProviderError> {
        let url = candidate_url(candidate)?;
        tokio::fs::create_dir_all(staging).await?;
        let path = staging.join(STREAM_FILE);
        let mut file = tokio::fs::File::create(&path).await?;
        let args = self.stream_args(&url);
        let program = self.executable();
        let mut child = process::spawn(&program, &args, true, Some(&self.tmp_dir()))
            .await
            .map_err(|e| self.spawn_failure(e, &program))?;
        let (state_tx, state_rx) = watch::channel(GrowState::Growing { bytes: 0 });
        let growing = GrowingFile {
            path: path.clone(),
            state: state_rx,
        };
        let stall = self.stall_timeout;
        let total = candidate.size;
        let thumbnail = self.thumbnail_args(&url, staging);
        let tmp_dir = self.tmp_dir();
        let staging = staging.to_path_buf();
        let done = async move {
            use tokio::io::AsyncReadExt;
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| ProviderError::Other(String::from("no stdout")))?;
            let stderr = child.stderr.take();
            let stderr_task = tokio::spawn(async move {
                let mut tail = String::new();
                if let Some(stderr) = stderr {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if parse_progress(&line).is_none() {
                            push_tail(&mut tail, &line);
                        }
                    }
                }
                tail
            });
            let mut buf = vec![0u8; 64 * 1024];
            let mut written: u64 = 0;
            let outcome: Result<(), ProviderError> = loop {
                let read = tokio::select! {
                    () = cancel.cancelled() => break Err(ProviderError::Cancelled),
                    read = tokio::time::timeout(stall, stdout.read(&mut buf)) => read,
                };
                match read {
                    Err(_) => break Err(ProviderError::Stalled),
                    Ok(Err(e)) => break Err(e.into()),
                    Ok(Ok(0)) => break Ok(()),
                    Ok(Ok(n)) => {
                        if let Err(e) = file.write_all(&buf[..n]).await {
                            break Err(e.into());
                        }
                        if let Err(e) = file.flush().await {
                            break Err(e.into());
                        }
                        written += n as u64;
                        state_tx.send_replace(GrowState::Growing { bytes: written });
                        progress.report(written, total);
                    }
                }
            };
            if let Err(e) = outcome {
                let _ = child.kill().await;
                state_tx.send_replace(GrowState::Failed);
                return Err(e);
            }
            let status = child.wait().await?;
            let stderr = stderr_task.await.unwrap_or_default();
            if !status.success() {
                state_tx.send_replace(GrowState::Failed);
                return Err(classify(&stderr));
            }
            if written == 0 {
                state_tx.send_replace(GrowState::Failed);
                return Err(ProviderError::Other(String::from("yt-dlp wrote nothing")));
            }
            file.sync_all().await?;
            state_tx.send_replace(GrowState::Complete { bytes: written });
            progress.report(written, Some(written));
            let cover = fetch_cover(&program, &thumbnail, &staging, &tmp_dir).await;
            Ok(FetchedFile {
                path,
                bytes: written,
                thumbnail: cover,
            })
        };
        Ok(StreamingFetch {
            growing,
            done: Box::pin(done),
        })
    }

    async fn health_inner(&self) -> ProviderHealth {
        let program = self.executable();
        let args = vec![OsString::from("--version")];
        let output = match process::run(
            &program,
            &args,
            VERSION_TIMEOUT,
            4096,
            &CancellationToken::new(),
            Some(&self.tmp_dir()),
        )
        .await
        {
            Ok(output) => output,
            Err(RunError::Io(e)) => {
                return match self.spawn_failure(e, &program) {
                    ProviderError::NotSetUp(why) => ProviderHealth::NotSetUp(why),
                    ProviderError::Unavailable(why) => ProviderHealth::Unavailable(why),
                    e => ProviderHealth::Unavailable(format!("yt-dlp --version failed: {e}")),
                };
            }
            Err(e) => return ProviderHealth::Unavailable(format!("yt-dlp --version failed: {e}")),
        };
        if !output.status.success() {
            return ProviderHealth::Unavailable(format!(
                "yt-dlp --version failed: {}",
                output.stderr.trim()
            ));
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        match version_date(&version) {
            Some(released) => {
                let age_days = (self.clock.now().0 - released.0) / 86_400_000;
                if age_days > i64::from(self.config.max_age_days) {
                    ProviderHealth::Outdated { version }
                } else {
                    ProviderHealth::Ok {
                        version: Some(version),
                    }
                }
            }
            None => ProviderHealth::Ok {
                version: Some(version),
            },
        }
    }
}

impl Provider for YtdlpProvider {
    fn name(&self) -> ProviderName {
        ProviderName::Ytdlp
    }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Candidate>, ProviderError>> {
        Box::pin(async move {
            let args = self.search_args(&query.text());
            let program = self.executable();
            let output = process::run(
                &program,
                &args,
                SEARCH_TIMEOUT,
                MAX_JSON,
                &cancel,
                Some(&self.tmp_dir()),
            )
            .await
            .map_err(|e| match e {
                RunError::Io(e) => self.spawn_failure(e, &program),
                RunError::TimedOut => ProviderError::Stalled,
                RunError::Cancelled => ProviderError::Cancelled,
            })?;
            if !output.status.success() {
                return Err(classify(&output.stderr));
            }
            parse_search(&output.stdout)
        })
    }

    fn fetch<'a>(
        &'a self,
        candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<FetchedFile, ProviderError>> {
        Box::pin(self.fetch_inner(candidate, staging, progress, cancel, false))
    }

    fn fetch_streaming<'a>(
        &'a self,
        candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> Option<BoxFuture<'a, Result<StreamingFetch, ProviderError>>> {
        Some(Box::pin(
            self.stream_inner(candidate, staging, progress, cancel),
        ))
    }

    fn streams(&self) -> bool {
        true
    }

    fn resume<'a>(
        &'a self,
        candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<FetchedFile, ProviderError>> {
        Box::pin(self.fetch_inner(candidate, staging, progress, cancel, true))
    }

    fn health(&self) -> BoxFuture<'_, ProviderHealth> {
        Box::pin(self.health_inner())
    }

    fn settings_view(&self) -> serde_json::Value {
        serde_json::json!({
            "concurrency": self.config.concurrency,
            "search_rate": format!("{}/{}s", self.config.search_rate.count, self.config.search_rate.per.as_secs()),
            "cookies_file": self.config.cookies_file,
            "path": self.executable(),
        })
    }
}

/// The `TMPDIR` for every yt-dlp run under `data_dir`.
pub fn tmp_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tmp")
}

/// Where the updater installs yt-dlp.
pub fn updated_binary(data_dir: &Path) -> PathBuf {
    data_dir.join("bin").join("yt-dlp")
}

fn runtime_kind(path: &Path) -> &'static str {
    let name = path
        .file_stem()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    ["node", "bun", "quickjs"]
        .into_iter()
        .find(|kind| name.contains(kind))
        .unwrap_or("deno")
}

/// Whether `id` is a video id: 11 characters of `[A-Za-z0-9_-]`.
pub fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The watch URL for a validated id. Clients never pass URLs; they come only from stored
/// candidates.
pub fn video_url(id: &str) -> Option<String> {
    valid_video_id(id).then(|| format!("https://www.youtube.com/watch?v={id}"))
}

fn candidate_url(candidate: &Candidate) -> Result<String, ProviderError> {
    candidate
        .provider_ref
        .get("id")
        .and_then(serde_json::Value::as_str)
        .and_then(video_url)
        .ok_or_else(|| ProviderError::Rejected(String::from("not a valid video id")))
}

#[derive(Deserialize)]
struct ProgressLine {
    downloaded_bytes: Option<f64>,
    total_bytes: Option<f64>,
    total_bytes_estimate: Option<f64>,
}

/// One line of `--progress-template "%(progress)j"`.
pub fn parse_progress(line: &str) -> Option<Progress> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let parsed: ProgressLine = serde_json::from_str(line).ok()?;
    let bytes = parsed.downloaded_bytes?;
    Some(Progress {
        bytes: bytes.max(0.0) as u64,
        total: parsed
            .total_bytes
            .or(parsed.total_bytes_estimate)
            .map(|t| t.max(0.0) as u64),
    })
}

/// Maps yt-dlp's error output to a provider error. Bot checks and throttling put the
/// provider in cooldown.
pub fn classify(stderr: &str) -> ProviderError {
    let lower = stderr.to_lowercase();
    if lower.contains("not a bot") || lower.contains("sign in to confirm") {
        return ProviderError::BotCheck;
    }
    if lower.contains("http error 429") || lower.contains("too many requests") {
        return ProviderError::RateLimited { retry_after: None };
    }
    if lower.contains("http error 403") {
        return ProviderError::RateLimited { retry_after: None };
    }
    if lower.contains("video unavailable")
        || lower.contains("private video")
        || lower.contains("has been removed")
        || lower.contains("http error 404")
    {
        return ProviderError::NotFound;
    }
    if lower.contains("larger than max-filesize") {
        return ProviderError::TooLarge;
    }
    let last = stderr
        .lines()
        .rev()
        .find(|line| line.contains("ERROR"))
        .or_else(|| stderr.lines().last())
        .unwrap_or("yt-dlp failed")
        .trim();
    ProviderError::Other(last.chars().take(300).collect())
}

#[derive(Deserialize)]
struct SearchOutput {
    #[serde(default)]
    entries: Vec<Option<SearchEntry>>,
}

#[derive(Deserialize)]
struct SearchEntry {
    id: Option<String>,
    title: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    duration: Option<f64>,
}

/// Candidates from `-J` output. Entries without a valid id are dropped.
pub fn parse_search(json: &[u8]) -> Result<Vec<Candidate>, ProviderError> {
    let output: SearchOutput = serde_json::from_slice(json)
        .map_err(|e| ProviderError::Other(format!("unreadable yt-dlp output: {e}")))?;
    Ok(output
        .entries
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.id.filter(|id| valid_video_id(id))?;
            let raw_title = entry.title.unwrap_or_default();
            let uploader = entry.uploader.or(entry.channel).unwrap_or_default();
            let topic_channel = uploader.ends_with(" - Topic");
            let lower = raw_title.to_lowercase();
            let official = lower.contains("official audio");
            let music_video = lower.contains("official video")
                || lower.contains("music video")
                || lower.contains("official music video");
            let (artist, title) = split_artist_title(&raw_title, &uploader, topic_channel);
            Some(Candidate {
                provider: ProviderName::Ytdlp,
                provider_ref: serde_json::json!({ "id": id }),
                title,
                artist,
                album: None,
                duration_ms: entry
                    .duration
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .map(|d| (d * 1000.0).round() as u32),
                container: None,
                codec: None,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                lossless: false,
                size: None,
                hints: SourceHints::Video {
                    uploader,
                    topic_channel,
                    official,
                    music_video,
                },
            })
        })
        .collect())
}

/// Videos are titled "Artist - Title" or carry the title alone on a `- Topic` channel.
fn split_artist_title(raw: &str, uploader: &str, topic: bool) -> (String, String) {
    if topic {
        let artist = uploader.trim_end_matches(" - Topic").trim().to_owned();
        return (artist, raw.trim().to_owned());
    }
    for separator in [" - ", " – ", " — "] {
        if let Some((artist, title)) = raw.split_once(separator) {
            return (artist.trim().to_owned(), title.trim().to_owned());
        }
    }
    (uploader.trim().to_owned(), raw.trim().to_owned())
}

/// The release date of a version such as `2026.10.22` or `2026.10.22.123456`, at midnight UTC.
pub fn version_date(version: &str) -> Option<Millis> {
    let mut parts = version.trim().split('.');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(2000..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date, after Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(Millis(days * 86_400_000))
}

/// The largest finished `audio.*` file in staging (not a `.part`, thumbnail or temporary).
fn find_output(staging: &Path) -> std::io::Result<Option<(PathBuf, u64)>> {
    let mut best: Option<(PathBuf, u64)> = None;
    for entry in std::fs::read_dir(staging)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(ext) = name.strip_prefix(&format!("{OUTPUT_STEM}.")) else {
            continue;
        };
        if ext.ends_with("part")
            || ext.ends_with("ytdl")
            || ext.contains(".temp")
            || matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "stream")
        {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        if best.as_ref().is_none_or(|(_, size)| meta.len() > *size) {
            best = Some((entry.path(), meta.len()));
        }
    }
    Ok(best)
}

async fn fetch_cover(
    program: &Path,
    args: &[OsString],
    staging: &Path,
    tmp_dir: &Path,
) -> Option<PathBuf> {
    let ran = process::run(
        program,
        args,
        THUMBNAIL_TIMEOUT,
        4096,
        &CancellationToken::new(),
        Some(tmp_dir),
    )
    .await;
    if !ran.is_ok_and(|output| output.status.success()) {
        return None;
    }
    let cover = staging.join("cover.jpg");
    tokio::fs::try_exists(&cover)
        .await
        .ok()
        .filter(|exists| *exists)
        .map(|_| cover)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use trackside_core::config::Rate;

    fn config(path: &Path) -> YtdlpConfig {
        YtdlpConfig {
            enabled: true,
            path: Some(path.to_path_buf()),
            js_runtime: None,
            cookies_file: None,
            concurrency: 2,
            search_rate: Rate {
                count: 10,
                per: Duration::from_secs(60),
            },
            max_age_days: 60,
        }
    }

    /// Writes a fake yt-dlp that records its arguments, one per line, to `argv`, then runs
    /// `body`.
    fn fake(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("yt-dlp");
        let script = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{argv}'; done\necho \"TMPDIR=$TMPDIR\" >> '{argv}'\necho END >> '{argv}'\n{body}\n",
            argv = dir.join("argv").display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn recorded(dir: &Path) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(dir.join("argv")).unwrap_or_default();
        text.split("END\n")
            .filter(|run| !run.is_empty())
            .map(|run| {
                run.lines()
                    .filter(|l| !l.starts_with("TMPDIR="))
                    .map(str::to_owned)
                    .collect()
            })
            .collect()
    }

    fn recorded_tmpdirs(dir: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(dir.join("argv")).unwrap_or_default();
        text.lines()
            .filter_map(|l| l.strip_prefix("TMPDIR="))
            .map(str::to_owned)
            .collect()
    }

    fn candidate(id: &str) -> Candidate {
        Candidate {
            provider: ProviderName::Ytdlp,
            provider_ref: serde_json::json!({ "id": id }),
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            album: None,
            duration_ms: Some(261_000),
            container: None,
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            lossless: false,
            size: None,
            hints: SourceHints::Video {
                uploader: String::from("Beyoncé - Topic"),
                topic_channel: true,
                official: false,
                music_video: false,
            },
        }
    }

    const SEARCH_JSON: &str = r#"{"_type":"playlist","entries":[
        {"id":"bnVUHWCynig","title":"Halo","uploader":"Beyoncé - Topic","duration":261.0},
        {"id":"bad id","title":"Nope","uploader":"x","duration":1.0},
        {"id":"abcdefghijk","title":"Beyoncé - Halo (Official Video)","channel":"Beyoncé","duration":224.5},
        null
    ]}"#;

    #[tokio::test]
    async fn search_runs_the_exact_arguments_and_parses_entries() {
        let dir = tempfile::tempdir().unwrap();
        let json_file = dir.path().join("out.json");
        std::fs::write(&json_file, SEARCH_JSON).unwrap();
        let program = fake(dir.path(), &format!("cat '{}'", json_file.display()));
        let mut cfg = config(&program);
        cfg.cookies_file = Some(PathBuf::from("/data/cookies.txt"));
        let provider = YtdlpProvider::new(&cfg, dir.path(), 1 << 30);
        let query = SearchQuery {
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            album: None,
            duration_ms: None,
            free_text: None,
        };
        let found = provider
            .search(&query, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            recorded(dir.path()),
            vec![vec![
                "--ignore-config",
                "--no-plugin-dirs",
                "--cookies",
                "/data/cookies.txt",
                "--flat-playlist",
                "-J",
                "ytsearch8:Beyoncé Halo",
            ]]
        );
        let tmp = dir.path().join("tmp");
        assert_eq!(recorded_tmpdirs(dir.path()), [tmp.display().to_string()]);
        assert!(tmp.is_dir(), "TMPDIR is created under the data directory");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].artist, "Beyoncé");
        assert_eq!(found[0].duration_ms, Some(261_000));
        assert_eq!(found[1].artist, "Beyoncé");
        assert_eq!(found[1].title, "Halo (Official Video)");
        assert!(matches!(
            found[1].hints,
            SourceHints::Video {
                music_video: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn fetch_runs_the_exact_arguments_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let program = fake(
            dir.path(),
            r#"
dir=""; prev=""
for a in "$@"; do [ "$prev" = "-P" ] && dir="$a"; prev="$a"; done
echo '{"status":"downloading","downloaded_bytes":512,"total_bytes":1024}'
echo '[download] some text line'
echo '{"status":"finished","downloaded_bytes":1024,"total_bytes":1024}'
printf 'fLaCdata' > "$dir/audio.m4a"
touch "$dir/audio.m4a.part"
"#,
        );
        let mut cfg = config(&program);
        cfg.js_runtime = Some(PathBuf::from("/usr/bin/deno"));
        let provider = YtdlpProvider::new(&cfg, dir.path(), 5000);
        let (sink, rx) = ProgressSink::new();
        let fetched = provider
            .fetch(
                &candidate("bnVUHWCynig"),
                &staging,
                sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(fetched.path, staging.join("audio.m4a"));
        assert_eq!(fetched.bytes, 8);
        assert_eq!(rx.borrow().bytes, 8);
        let staging_text = staging.display().to_string();
        assert_eq!(
            recorded(dir.path()),
            vec![vec![
                "--ignore-config",
                "--no-plugin-dirs",
                "--js-runtimes",
                "deno:/usr/bin/deno",
                "--no-playlist",
                "-f",
                "bestaudio[ext=m4a]/bestaudio",
                "-x",
                "--embed-metadata",
                "--embed-thumbnail",
                "--max-filesize",
                "5000",
                "--newline",
                "--progress-template",
                "%(progress)j",
                "-P",
                staging_text.as_str(),
                "-o",
                "audio.%(ext)s",
                "https://www.youtube.com/watch?v=bnVUHWCynig",
            ]]
        );
    }

    #[tokio::test]
    async fn resume_passes_continue() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let program = fake(
            dir.path(),
            r#"dir=""; prev=""; for a in "$@"; do [ "$prev" = "-P" ] && dir="$a"; prev="$a"; done; printf 'x' > "$dir/audio.opus""#,
        );
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        let (sink, _rx) = ProgressSink::new();
        provider
            .resume(
                &candidate("bnVUHWCynig"),
                &staging,
                sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(recorded(dir.path())[0].contains(&String::from("--continue")));
    }

    #[tokio::test]
    async fn an_invalid_video_id_never_reaches_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "exit 0");
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        for id in [
            "https://evil.example/x",
            "--exec=rm",
            "short",
            "abcdefghij;",
        ] {
            let (sink, _rx) = ProgressSink::new();
            let result = provider
                .fetch(&candidate(id), dir.path(), sink, CancellationToken::new())
                .await;
            assert!(matches!(result, Err(ProviderError::Rejected(_))), "{id}");
        }
        assert_eq!(recorded(dir.path()).len(), 0);
    }

    #[tokio::test]
    async fn bot_checks_and_throttling_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(
            dir.path(),
            "echo \"ERROR: [youtube] bnVUHWCynig: Sign in to confirm you're not a bot.\" >&2; exit 1",
        );
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        let (sink, _rx) = ProgressSink::new();
        let result = provider
            .fetch(
                &candidate("bnVUHWCynig"),
                dir.path(),
                sink,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(ProviderError::BotCheck)));
        assert!(matches!(
            classify("ERROR: unable to download video data: HTTP Error 429: Too Many Requests"),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify("ERROR: HTTP Error 403: Forbidden"),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify("ERROR: Video unavailable"),
            ProviderError::NotFound
        ));
    }

    #[tokio::test]
    async fn an_oversized_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(
            dir.path(),
            "echo '[download] File is larger than max-filesize (9000 bytes > 5000 bytes). Aborting.'",
        );
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        let (sink, _rx) = ProgressSink::new();
        let result = provider
            .fetch(
                &candidate("bnVUHWCynig"),
                &dir.path().join("s"),
                sink,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(ProviderError::TooLarge)));
    }

    #[tokio::test]
    async fn cancelling_kills_the_process() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "exec sleep 30");
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        let (sink, _rx) = ProgressSink::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = provider
            .fetch(
                &candidate("bnVUHWCynig"),
                &dir.path().join("s"),
                sink,
                cancel,
            )
            .await;
        assert!(matches!(result, Err(ProviderError::Cancelled)));
    }

    #[tokio::test]
    async fn streaming_tees_stdout_into_a_growing_file() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging");
        let program = fake(
            dir.path(),
            r#"
case "$*" in *--skip-download*) exit 0;; esac
printf 'fLaCpart1'
echo '{"downloaded_bytes":9}' >&2
printf 'part2'
"#,
        );
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        let (sink, _rx) = ProgressSink::new();
        let stream = provider
            .fetch_streaming(
                &candidate("bnVUHWCynig"),
                &staging,
                sink,
                CancellationToken::new(),
            )
            .unwrap()
            .await
            .unwrap();
        let mut state = stream.growing.state.clone();
        let fetched = stream.done.await.unwrap();
        assert_eq!(
            *state.borrow_and_update(),
            GrowState::Complete { bytes: 14 }
        );
        assert_eq!(std::fs::read(&fetched.path).unwrap(), b"fLaCpart1part2");
        let runs = recorded(dir.path());
        assert!(runs[0].ends_with(&[
            String::from("-o"),
            String::from("-"),
            String::from("https://www.youtube.com/watch?v=bnVUHWCynig")
        ]));
        assert!(runs[1].contains(&String::from("--skip-download")));
    }

    #[tokio::test]
    async fn health_reads_the_version_and_flags_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo 2026.01.02");
        let clock = Arc::new(trackside_core::TestClock::new(
            trackside_core::testing::TEST_START,
        ));
        let provider =
            YtdlpProvider::new(&config(&program), dir.path(), 5000).with_clock(clock.clone());
        assert_eq!(
            provider.health().await,
            ProviderHealth::Outdated {
                version: String::from("2026.01.02")
            }
        );
        clock.set(version_date("2026.02.01").unwrap());
        assert_eq!(
            provider.health().await,
            ProviderHealth::Ok {
                version: Some(String::from("2026.01.02"))
            }
        );
    }

    #[tokio::test]
    async fn health_ignores_the_startup_setting_so_the_dashboard_switch_wins() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake(dir.path(), "echo 2026.02.01");
        let mut off = config(&program);
        off.enabled = false;
        let clock = Arc::new(trackside_core::TestClock::new(
            version_date("2026.02.01").unwrap(),
        ));
        let provider = YtdlpProvider::new(&off, dir.path(), 5000).with_clock(clock);
        assert_eq!(
            provider.health().await,
            ProviderHealth::Ok {
                version: Some(String::from("2026.02.01"))
            }
        );
    }

    #[tokio::test]
    async fn health_without_the_executable_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let provider = YtdlpProvider::new(&config(&dir.path().join("missing")), dir.path(), 5000);
        assert!(matches!(
            provider.health().await,
            ProviderHealth::Unavailable(_)
        ));
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn unconfigured() -> YtdlpConfig {
        let mut config = config(Path::new("unused"));
        config.path = None;
        config
    }

    #[test]
    fn executable_prefers_an_updated_binary_that_runs() {
        let dir = tempfile::tempdir().unwrap();
        let updated = updated_binary(dir.path());
        write_script(&updated, "#!/bin/sh\necho 2026.11.03\n");
        let provider = YtdlpProvider::new(&unconfigured(), dir.path(), 5000);
        assert_eq!(provider.executable(), updated);
    }

    #[test]
    fn executable_falls_back_past_an_updated_binary_that_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        write_script(&updated_binary(dir.path()), "#!/nonexistent/sh\n");
        let provider = YtdlpProvider::new(&unconfigured(), dir.path(), 5000);
        assert_eq!(provider.executable(), PathBuf::from("yt-dlp"));
    }

    #[test]
    fn executable_notices_a_replaced_binary() {
        let dir = tempfile::tempdir().unwrap();
        let updated = updated_binary(dir.path());
        write_script(&updated, "#!/nonexistent/sh\n");
        let provider = YtdlpProvider::new(&unconfigured(), dir.path(), 5000);
        assert_eq!(provider.executable(), PathBuf::from("yt-dlp"));
        write_script(&updated, "#!/bin/sh\necho 2026.11.03\n");
        assert_eq!(provider.executable(), updated);
    }

    #[test]
    fn no_runnable_yt_dlp_is_not_set_up() {
        let dir = tempfile::tempdir().unwrap();
        let provider = YtdlpProvider::new(&unconfigured(), dir.path(), 5000);
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(matches!(
            provider.spawn_failure(e, Path::new("yt-dlp")),
            ProviderError::NotSetUp(why) if why == NEEDS_ACQUIRE_IMAGE
        ));
    }

    #[tokio::test]
    async fn a_configured_path_that_cannot_run_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("yt-dlp");
        write_script(&program, "#!/nonexistent/sh\n");
        let provider = YtdlpProvider::new(&config(&program), dir.path(), 5000);
        assert_eq!(
            provider.health().await,
            ProviderHealth::Unavailable(format!("{} cannot run on this system", program.display()))
        );
    }

    #[test]
    fn progress_lines_parse() {
        assert_eq!(
            parse_progress(
                r#"{"status":"downloading","downloaded_bytes":10,"total_bytes_estimate":99.5}"#
            ),
            Some(Progress {
                bytes: 10,
                total: Some(99)
            })
        );
        assert_eq!(parse_progress("[download] 10%"), None);
        assert_eq!(parse_progress(r#"{"status":"downloading"}"#), None);
    }

    #[test]
    fn version_dates_are_days_since_the_epoch() {
        assert_eq!(version_date("1970.01.02"), None);
        assert_eq!(version_date("2026.11.14"), Some(Millis(1_794_614_400_000)));
        assert_eq!(version_date("nightly"), None);
    }

    #[test]
    fn video_ids_are_validated() {
        assert!(valid_video_id("bnVUHWCynig"));
        assert!(valid_video_id("a-b_c-d_e-f"));
        assert!(!valid_video_id("bnVUHWCyni"));
        assert!(!valid_video_id("bnVUHWCyni/"));
        assert_eq!(video_url("x"), None);
    }
}
