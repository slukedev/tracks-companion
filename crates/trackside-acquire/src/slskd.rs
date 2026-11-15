//! Soulseek through an slskd instance's REST API (architecture section 9.4). Trackside never
//! speaks the Soulseek protocol itself.
//!
//! Endpoints, from slskd's controllers (API v0): `POST /api/v0/searches`,
//! `GET /api/v0/searches/{id}`, `GET /api/v0/searches/{id}/responses`,
//! `DELETE /api/v0/searches/{id}`, `POST /api/v0/transfers/downloads/{username}` with
//! `[{filename, size}]`, `GET /api/v0/transfers/downloads/{username}`,
//! `DELETE /api/v0/transfers/downloads/{username}/{id}?remove=true` and
//! `GET /api/v0/application`. JSON is camelCase; transfer states are Soulseek.NET's flags
//! enum written as text, such as `"Completed, Succeeded"` or `"Queued, Remotely"`.

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use trackside_core::BoxFuture;
use trackside_core::config::{Secret, SoulseekConfig};
use trackside_core::records::{Codec, Container};

use crate::provider::{
    Candidate, FetchedFile, ProgressSink, Provider, ProviderError, ProviderHealth, ProviderName,
    ProviderSettings, SearchQuery, SourceHints,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long past slskd's own search timeout we keep waiting for it to finish.
const SEARCH_GRACE: Duration = Duration::from_secs(3);
const RESPONSE_LIMIT: u32 = 100;
const FILE_LIMIT: u32 = 10_000;
/// Consecutive polls that may miss the transfer before it counts as gone.
const MISSING_POLLS: u32 = 3;
const LOCATE_DEPTH: usize = 4;

/// Installs ring as the process's rustls provider if none is, then builds a client. reqwest
/// is built without a provider of its own (build-plan section 6).
pub fn http_client() -> Result<reqwest::Client, ProviderError> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("trackside/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ProviderError::Other(format!("cannot build an HTTP client: {e}")))
}

#[derive(Clone, Debug, Default)]
struct Endpoint {
    url: Option<Url>,
    api_key: Option<Secret>,
}

/// slskd over its HTTP API.
#[derive(Debug)]
pub struct SoulseekProvider {
    config: SoulseekConfig,
    endpoint: RwLock<Endpoint>,
    client: reqwest::Client,
    poll_interval: Duration,
}

impl SoulseekProvider {
    pub fn new(config: &SoulseekConfig) -> Result<Self, ProviderError> {
        let url = config.url.as_deref().map(parse_base).transpose()?;
        Ok(Self {
            config: config.clone(),
            endpoint: RwLock::new(Endpoint {
                url,
                api_key: config.api_key.clone(),
            }),
            client: http_client()?,
            poll_interval: POLL_INTERVAL,
        })
    }

    /// Polls searches and transfers this often (tests use a short interval).
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    fn endpoint(&self) -> Endpoint {
        self.endpoint
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let endpoint = self.endpoint();
        let base = endpoint
            .url
            .ok_or_else(|| ProviderError::Unavailable(String::from("slskd is not configured")))?;
        let url = base
            .join(path.trim_start_matches('/'))
            .map_err(|e| ProviderError::Other(format!("bad slskd path: {e}")))?;
        let mut builder = self.client.request(method, url);
        if let Some(key) = &endpoint.api_key {
            builder = builder.header("X-API-Key", key.expose());
        }
        Ok(builder)
    }

    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = builder.send().await.map_err(|e| {
            if e.is_connect() || e.is_timeout() {
                ProviderError::Unavailable(String::from("cannot reach slskd"))
            } else {
                ProviderError::Other(format!("slskd request failed: {}", e.without_url()))
            }
        })?;
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ProviderError::Unavailable(
                String::from("slskd rejected the API key"),
            )),
            StatusCode::TOO_MANY_REQUESTS => Err(ProviderError::RateLimited {
                retry_after: response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .map(Duration::from_secs),
            }),
            _ => Ok(response),
        }
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<T, ProviderError> {
        let response = self.send(builder).await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ProviderError::NotFound);
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!("slskd answered {status}")));
        }
        response.json().await.map_err(|e| {
            ProviderError::Other(format!("unreadable slskd answer: {}", e.without_url()))
        })
    }

    async fn search_inner(
        &self,
        query: &SearchQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<Candidate>, ProviderError> {
        let id = uuid::Uuid::new_v4();
        let body = serde_json::json!({
            "id": id,
            "searchText": query.text(),
            "searchTimeout": self.config.search_timeout.as_secs().max(1),
            "filterResponses": true,
            "responseLimit": RESPONSE_LIMIT,
            "fileLimit": FILE_LIMIT,
        });
        let _: serde_json::Value = self
            .json(self.request(Method::POST, "api/v0/searches")?.json(&body))
            .await?;
        let result = self.wait_for_search(id, &cancel).await;
        let cleanup = self.request(Method::DELETE, &format!("api/v0/searches/{id}"));
        if let Ok(cleanup) = cleanup {
            let _ = self.send(cleanup).await;
        }
        let responses = result?;
        Ok(responses.into_iter().flat_map(candidates_from).collect())
    }

    async fn wait_for_search(
        &self,
        id: uuid::Uuid,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchResponse>, ProviderError> {
        let deadline = Instant::now() + self.config.search_timeout + SEARCH_GRACE;
        loop {
            let search: SearchState = self
                .json(self.request(Method::GET, &format!("api/v0/searches/{id}"))?)
                .await?;
            if search.is_complete || Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                () = cancel.cancelled() => return Err(ProviderError::Cancelled),
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
        self.json(self.request(Method::GET, &format!("api/v0/searches/{id}/responses"))?)
            .await
    }

    async fn enqueue(&self, file: &PeerFile) -> Result<(), ProviderError> {
        let body = serde_json::json!([{ "filename": file.filename, "size": file.size }]);
        let response = self
            .send(
                self.request(Method::POST, &downloads_path(&file.username))?
                    .json(&body),
            )
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(
            if status == StatusCode::NOT_FOUND || status.is_server_error() {
                ProviderError::Unavailable(format!(
                    "the peer is offline or refused the request ({})",
                    first_line(&text, status)
                ))
            } else {
                ProviderError::Rejected(first_line(&text, status))
            },
        )
    }

    /// The newest transfer of this file, or `None` when slskd does not list it.
    async fn transfer(&self, file: &PeerFile) -> Result<Option<Transfer>, ProviderError> {
        let user: Result<UserTransfers, _> = self
            .json(self.request(Method::GET, &downloads_path(&file.username))?)
            .await;
        let user = match user {
            Ok(user) => user,
            Err(ProviderError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(user
            .directories
            .into_iter()
            .flat_map(|d| d.files)
            .filter(|t| t.filename == file.filename)
            .max_by(|a, b| a.requested_at.cmp(&b.requested_at)))
    }

    async fn cancel_transfer(&self, file: &PeerFile, id: &str) {
        let path = format!(
            "{}/{}?remove=true",
            downloads_path(&file.username),
            percent(id)
        );
        if let Ok(request) = self.request(Method::DELETE, &path) {
            let _ = self.send(request).await;
        }
    }

    /// Polls the transfer until it succeeds, fails, stalls or is cancelled.
    async fn follow(
        &self,
        file: &PeerFile,
        progress: &ProgressSink,
        cancel: &CancellationToken,
    ) -> Result<(), ProviderError> {
        let mut last_bytes: Option<u64> = None;
        let mut last_change = Instant::now();
        let mut missing = 0;
        let mut transfer_id: Option<String> = None;
        loop {
            match self.transfer(file).await? {
                None => {
                    missing += 1;
                    if missing >= MISSING_POLLS {
                        return Err(ProviderError::Unavailable(String::from(
                            "slskd no longer lists the transfer",
                        )));
                    }
                }
                Some(transfer) => {
                    missing = 0;
                    transfer_id = Some(transfer.id.clone());
                    let state = TransferState::parse(&transfer.state);
                    progress.report(
                        transfer.bytes_transferred,
                        Some(transfer.size.max(file.size)),
                    );
                    match state {
                        TransferState::Succeeded => return Ok(()),
                        TransferState::Failed(reason) => {
                            return Err(match reason {
                                FailReason::Rejected => ProviderError::Rejected(String::from(
                                    "the peer rejected the transfer",
                                )),
                                FailReason::TimedOut => ProviderError::Stalled,
                                FailReason::Other => ProviderError::Unavailable(format!(
                                    "the transfer failed: {}",
                                    transfer.exception.as_deref().unwrap_or(&transfer.state)
                                )),
                            });
                        }
                        TransferState::Pending => {
                            if last_bytes != Some(transfer.bytes_transferred) {
                                last_bytes = Some(transfer.bytes_transferred);
                                last_change = Instant::now();
                            }
                        }
                    }
                }
            }
            if last_change.elapsed() >= self.config.stall_timeout {
                if let Some(id) = &transfer_id {
                    self.cancel_transfer(file, id).await;
                }
                return Err(ProviderError::Stalled);
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    if let Some(id) = &transfer_id {
                        self.cancel_transfer(file, id).await;
                    }
                    return Err(ProviderError::Cancelled);
                }
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }

    async fn collect(&self, file: &PeerFile, staging: &Path) -> Result<FetchedFile, ProviderError> {
        let downloads = self.config.downloads_dir.clone().ok_or_else(|| {
            ProviderError::Unavailable(String::from("acquire.soulseek.downloads_dir is not set"))
        })?;
        let remote = file.filename.clone();
        let size = file.size;
        let staging = staging.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let found = locate(&downloads, &remote, size)?;
            move_into_staging(&found, &staging, &remote)
        })
        .await
        .map_err(|e| ProviderError::Other(e.to_string()))?
    }

    async fn fetch_inner(
        &self,
        candidate: &Candidate,
        staging: &Path,
        progress: ProgressSink,
        cancel: CancellationToken,
        resume: bool,
    ) -> Result<FetchedFile, ProviderError> {
        let file = PeerFile::of(candidate)?;
        let existing = if resume {
            self.transfer(&file).await?
        } else {
            None
        };
        match existing.map(|t| TransferState::parse(&t.state)) {
            Some(TransferState::Succeeded) => {}
            Some(TransferState::Pending) => self.follow(&file, &progress, &cancel).await?,
            Some(TransferState::Failed(_)) | None => {
                self.enqueue(&file).await?;
                self.follow(&file, &progress, &cancel).await?;
            }
        }
        let fetched = self.collect(&file, staging).await?;
        progress.report(fetched.bytes, Some(fetched.bytes));
        Ok(fetched)
    }

    async fn health_inner(&self) -> ProviderHealth {
        if !self.config.enabled {
            return ProviderHealth::Disabled;
        }
        let request = match self.request(Method::GET, "api/v0/application") {
            Ok(request) => request,
            Err(ProviderError::Unavailable(why)) => return ProviderHealth::Unavailable(why),
            Err(e) => return ProviderHealth::Unavailable(e.to_string()),
        };
        match self.json::<Application>(request).await {
            Ok(app) => {
                let version = app.version.and_then(|v| v.full.or(v.current));
                if app.server.is_some_and(|s| s.is_logged_in) {
                    ProviderHealth::Ok { version }
                } else {
                    ProviderHealth::Degraded(String::from("slskd is not logged in to Soulseek"))
                }
            }
            Err(ProviderError::Unavailable(why)) => ProviderHealth::Unavailable(why),
            Err(e) => ProviderHealth::Unavailable(e.to_string()),
        }
    }
}

impl Provider for SoulseekProvider {
    fn name(&self) -> ProviderName {
        ProviderName::Soulseek
    }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Candidate>, ProviderError>> {
        Box::pin(self.search_inner(query, cancel))
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

    fn reconfigure(&self, settings: &ProviderSettings) -> Result<(), ProviderError> {
        let url = settings.url.as_deref().map(parse_base).transpose()?;
        let mut endpoint = self
            .endpoint
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(url) = url {
            endpoint.url = Some(url);
        }
        if let Some(key) = &settings.api_key {
            endpoint.api_key = Some(key.clone());
        }
        Ok(())
    }

    fn settings_view(&self) -> serde_json::Value {
        let endpoint = self.endpoint();
        serde_json::json!({
            "concurrency": self.config.concurrency,
            "search_rate": "1/3s",
            "url": endpoint.url.map(|u| u.to_string()),
            "api_key_set": endpoint.api_key.is_some(),
            "downloads_dir": self.config.downloads_dir,
        })
    }

    fn configured(&self) -> bool {
        self.endpoint().url.is_some() && self.config.downloads_dir.is_some()
    }
}

fn parse_base(url: &str) -> Result<Url, ProviderError> {
    let mut url = Url::parse(url.trim())
        .map_err(|e| ProviderError::Rejected(format!("invalid slskd URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ProviderError::Rejected(String::from(
            "slskd URL must be http or https",
        )));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn downloads_path(username: &str) -> String {
    format!("api/v0/transfers/downloads/{}", percent(username))
}

/// Percent-encodes a path segment.
fn percent(segment: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn first_line(text: &str, status: StatusCode) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        status.to_string()
    } else {
        line.chars().take(200).collect()
    }
}

/// What identifies a peer's file; the candidate's `provider_ref`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PeerFile {
    username: String,
    filename: String,
    size: u64,
}

impl PeerFile {
    fn of(candidate: &Candidate) -> Result<Self, ProviderError> {
        let r = &candidate.provider_ref;
        let text = |key: &str| {
            r.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        match (text("username"), text("filename")) {
            (Some(username), Some(filename)) if !username.is_empty() && !filename.is_empty() => {
                Ok(Self {
                    username,
                    filename,
                    size: r
                        .get("size")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                })
            }
            _ => Err(ProviderError::Rejected(String::from(
                "not a Soulseek candidate",
            ))),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchState {
    #[serde(default)]
    is_complete: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    username: String,
    #[serde(default)]
    files: Vec<SearchFile>,
    #[serde(default)]
    has_free_upload_slot: bool,
    #[serde(default)]
    queue_length: u64,
    #[serde(default)]
    upload_speed: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchFile {
    filename: String,
    #[serde(default)]
    size: u64,
    bit_rate: Option<u32>,
    sample_rate: Option<u32>,
    bit_depth: Option<u32>,
    /// Seconds.
    length: Option<u32>,
    #[serde(default)]
    is_locked: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserTransfers {
    #[serde(default)]
    directories: Vec<TransferDirectory>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransferDirectory {
    #[serde(default)]
    files: Vec<Transfer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Transfer {
    id: String,
    filename: String,
    #[serde(default)]
    size: u64,
    state: String,
    #[serde(default)]
    bytes_transferred: u64,
    #[serde(default)]
    requested_at: Option<String>,
    exception: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Application {
    version: Option<VersionState>,
    server: Option<ServerState>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionState {
    full: Option<String>,
    current: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerState {
    #[serde(default)]
    is_logged_in: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum FailReason {
    Rejected,
    TimedOut,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
enum TransferState {
    Pending,
    Succeeded,
    Failed(FailReason),
}

impl TransferState {
    fn parse(text: &str) -> Self {
        let has = |flag: &str| {
            text.split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(flag))
        };
        if !has("Completed") {
            return Self::Pending;
        }
        if has("Succeeded") {
            Self::Succeeded
        } else if has("Rejected") || has("Cancelled") {
            Self::Failed(FailReason::Rejected)
        } else if has("TimedOut") {
            Self::Failed(FailReason::TimedOut)
        } else {
            Self::Failed(FailReason::Other)
        }
    }
}

fn audio_format(ext: &str, bit_depth: Option<u32>) -> Option<(Container, Option<Codec>, bool)> {
    Some(match ext {
        "flac" => (Container::Flac, Some(Codec::Flac), true),
        "mp3" => (Container::Mp3, Some(Codec::Mp3), false),
        "m4a" | "mp4" => match bit_depth {
            Some(_) => (Container::M4a, Some(Codec::Alac), true),
            None => (Container::M4a, Some(Codec::Aac), false),
        },
        "ogg" => (Container::Ogg, Some(Codec::Vorbis), false),
        "opus" => (Container::Opus, Some(Codec::Opus), false),
        "wav" => (Container::Wav, Some(Codec::Pcm), true),
        "aif" | "aiff" => (Container::Aiff, Some(Codec::Pcm), true),
        _ => return None,
    })
}

fn candidates_from(response: SearchResponse) -> Vec<Candidate> {
    let SearchResponse {
        username,
        files,
        has_free_upload_slot,
        queue_length,
        upload_speed,
    } = response;
    files
        .into_iter()
        .filter(|f| !f.is_locked)
        .filter_map(|file| {
            let parts: Vec<&str> = file
                .filename
                .split(['\\', '/'])
                .filter(|p| !p.is_empty())
                .collect();
            let name = *parts.last()?;
            let (stem, ext) = name.rsplit_once('.')?;
            let (container, codec, lossless) =
                audio_format(&ext.to_ascii_lowercase(), file.bit_depth)?;
            let folder = parts[..parts.len() - 1].join("\\");
            let album_dir = parts.len().checked_sub(2).map(|i| parts[i]);
            let artist_dir = parts.len().checked_sub(3).map(|i| parts[i]);
            let stem = strip_track_number(stem);
            let (artist, title) = match stem.split_once(" - ") {
                Some((artist, title)) => (artist.trim().to_owned(), title.trim().to_owned()),
                None => (artist_dir.unwrap_or("").to_owned(), stem.trim().to_owned()),
            };
            Some(Candidate {
                provider: ProviderName::Soulseek,
                provider_ref: serde_json::json!({
                    "username": username,
                    "filename": file.filename,
                    "size": file.size,
                }),
                title,
                artist,
                album: album_dir.map(str::to_owned),
                duration_ms: file.length.map(|s| s.saturating_mul(1000)),
                container: Some(container),
                codec,
                bitrate_kbps: file.bit_rate,
                sample_rate: file.sample_rate,
                bit_depth: file.bit_depth.and_then(|d| u8::try_from(d).ok()),
                lossless,
                size: Some(file.size),
                hints: SourceHints::Peer {
                    username: username.clone(),
                    folder,
                    free_slot: has_free_upload_slot,
                    queue_length: u32::try_from(queue_length).unwrap_or(u32::MAX),
                    upload_speed,
                },
            })
        })
        .collect()
}

/// "03 - Halo", "03. Halo", "03 Halo" and "1-03 Halo" become "Halo".
fn strip_track_number(stem: &str) -> &str {
    let trimmed = stem.trim_start();
    let digits = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(trimmed.len());
    if digits == 0 || digits > 5 || !trimmed[..digits].chars().any(|c| c.is_ascii_digit()) {
        return stem;
    }
    let rest = trimmed[digits..].trim_start_matches([' ', '.', '-', '_']);
    if rest.is_empty() { stem } else { rest }
}

/// Finds slskd's copy of `remote` under `downloads`: `<downloads>/<parent>/<name>` as slskd
/// writes it, then a bounded search by name and size. The result is inside `downloads`.
fn locate(downloads: &Path, remote: &str, size: u64) -> Result<PathBuf, ProviderError> {
    let root = downloads.canonicalize().map_err(|e| {
        ProviderError::Unavailable(format!("slskd's downloads directory is not readable: {e}"))
    })?;
    let parts: Vec<&str> = remote
        .split(['\\', '/'])
        .filter(|p| !p.is_empty())
        .collect();
    let Some(name) = parts.last().copied() else {
        return Err(ProviderError::Other(String::from("empty remote file name")));
    };
    if name == "." || name == ".." {
        return Err(ProviderError::Rejected(String::from(
            "bad remote file name",
        )));
    }
    let mut guesses = Vec::new();
    if let Some(parent) = parts.len().checked_sub(2).map(|i| parts[i])
        && parent != "."
        && parent != ".."
    {
        guesses.push(root.join(parent).join(name));
    }
    guesses.push(root.join(name));
    let found = guesses
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| find_by_name(&root, name, size, LOCATE_DEPTH))
        .ok_or_else(|| {
            ProviderError::Other(format!("slskd finished but {name} is not in its downloads"))
        })?;
    let meta = std::fs::symlink_metadata(&found)?;
    if meta.file_type().is_symlink() {
        return Err(ProviderError::Rejected(String::from(
            "the downloaded file is a symlink",
        )));
    }
    let canonical = found.canonicalize()?;
    if !canonical.starts_with(&root) {
        return Err(ProviderError::Rejected(String::from(
            "the downloaded file is outside slskd's downloads",
        )));
    }
    Ok(canonical)
}

fn find_by_name(dir: &Path, name: &str, size: u64, depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            dirs.push(entry.path());
        } else if kind.is_file()
            && entry.file_name().to_string_lossy() == name
            && (size == 0 || entry.metadata().is_ok_and(|m| m.len() == size))
        {
            return Some(entry.path());
        }
    }
    if depth == 0 {
        return None;
    }
    dirs.into_iter()
        .find_map(|d| find_by_name(&d, name, size, depth - 1))
}

fn move_into_staging(
    found: &Path,
    staging: &Path,
    remote: &str,
) -> Result<FetchedFile, ProviderError> {
    std::fs::create_dir_all(staging)?;
    let ext: String = remote
        .rsplit_once('.')
        .map_or("", |(_, e)| e)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let target = staging.join(if ext.is_empty() {
        String::from("audio")
    } else {
        format!("audio.{ext}")
    });
    if std::fs::rename(found, &target).is_err() {
        std::fs::copy(found, &target)?;
        std::fs::remove_file(found)?;
    }
    let bytes = std::fs::metadata(&target)?.len();
    Ok(FetchedFile {
        path: target,
        bytes,
        thumbnail: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::extract::{Path as UrlPath, State};
    use axum::http::{HeaderMap, StatusCode as Status};
    use axum::routing::{delete, get, post};
    use std::sync::{Arc, Mutex};

    /// What the fake slskd does with a download.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Plan {
        /// Queued, then in progress, then succeeded, writing the file.
        Succeed,
        /// Stays queued remotely with no bytes.
        QueuedForever,
        /// Completes as rejected.
        Reject,
        /// The peer is offline: enqueue fails.
        Offline,
    }

    #[derive(Debug)]
    struct Fake {
        plan: Plan,
        key: &'static str,
        downloads: PathBuf,
        polls: u32,
        log: Vec<String>,
        search_polls: u32,
    }

    type Shared = Arc<Mutex<Fake>>;

    fn authorised(headers: &HeaderMap, fake: &Fake) -> bool {
        headers.get("x-api-key").and_then(|v| v.to_str().ok()) == Some(fake.key)
    }

    fn log(state: &Shared, line: String) {
        state.lock().unwrap().log.push(line);
    }

    #[expect(clippy::too_many_lines, reason = "the whole fake slskd in one place")]
    async fn serve(plan: Plan, downloads: PathBuf) -> (String, Shared) {
        let state: Shared = Arc::new(Mutex::new(Fake {
            plan,
            key: "secret",
            downloads,
            polls: 0,
            log: Vec::new(),
            search_polls: 0,
        }));
        let app = Router::new()
            .route(
                "/api/v0/searches",
                post(|State(s): State<Shared>, headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    if !authorised(&headers, &s.lock().unwrap()) {
                        return Err(Status::UNAUTHORIZED);
                    }
                    log(&s, format!("search {}", body["searchText"].as_str().unwrap_or("")));
                    Ok(Json(serde_json::json!({ "id": body["id"], "isComplete": false })))
                }),
            )
            .route(
                "/api/v0/searches/{id}",
                get(|State(s): State<Shared>| async move {
                    let mut fake = s.lock().unwrap();
                    fake.search_polls += 1;
                    Json(serde_json::json!({ "isComplete": fake.search_polls >= 2, "responseCount": 1 }))
                })
                .delete(|State(s): State<Shared>, UrlPath(id): UrlPath<String>| async move {
                    log(&s, format!("delete search {id}"));
                    Status::NO_CONTENT
                }),
            )
            .route(
                "/api/v0/searches/{id}/responses",
                get(|| async {
                    Json(serde_json::json!([{
                        "username": "peer one",
                        "hasFreeUploadSlot": true,
                        "queueLength": 2,
                        "uploadSpeed": 900_000,
                        "fileCount": 3,
                        "files": [
                            {"filename": "@@music\\Beyoncé\\I Am... Sasha Fierce\\03 - Halo.flac", "size": 30_000_000, "bitRate": 1000, "sampleRate": 44100, "bitDepth": 16, "length": 261, "isLocked": false},
                            {"filename": "@@music\\Beyoncé\\cover.jpg", "size": 1000},
                            {"filename": "@@music\\Beyoncé\\Halo.mp3", "size": 9_000_000, "bitRate": 320, "length": 261, "isLocked": true}
                        ]
                    }]))
                }),
            )
            .route(
                "/api/v0/transfers/downloads/{username}",
                post(|State(s): State<Shared>, UrlPath(user): UrlPath<String>, Json(body): Json<serde_json::Value>| async move {
                    log(&s, format!("enqueue {user} {}", body[0]["filename"].as_str().unwrap_or("")));
                    if s.lock().unwrap().plan == Plan::Offline {
                        return Status::INTERNAL_SERVER_ERROR;
                    }
                    Status::CREATED
                })
                .get(|State(s): State<Shared>, UrlPath(user): UrlPath<String>| async move {
                    let mut fake = s.lock().unwrap();
                    fake.polls += 1;
                    let filename = "@@music\\Beyoncé\\I Am... Sasha Fierce\\03 - Halo.flac";
                    let (state, bytes) = match (fake.plan, fake.polls) {
                        (Plan::QueuedForever, _) | (_, 1) => ("Queued, Remotely", 0),
                        (Plan::Reject, _) => ("Completed, Rejected", 0),
                        (_, 2) => ("InProgress", 4),
                        _ => {
                            let dir = fake.downloads.join("I Am... Sasha Fierce");
                            std::fs::create_dir_all(&dir).unwrap();
                            std::fs::write(dir.join("03 - Halo.flac"), b"fLaCaudio").unwrap();
                            ("Completed, Succeeded", 9)
                        }
                    };
                    Json(serde_json::json!({
                        "username": user,
                        "directories": [{"directory": "x", "fileCount": 1, "files": [{
                            "id": "1b2c3d4e-0000-0000-0000-000000000000",
                            "username": user,
                            "filename": filename,
                            "size": 9,
                            "state": state,
                            "bytesTransferred": bytes,
                            "requestedAt": "2026-11-14T09:30:00"
                        }]}]
                    }))
                }),
            )
            .route(
                "/api/v0/transfers/downloads/{username}/{id}",
                delete(|State(s): State<Shared>, UrlPath((user, id)): UrlPath<(String, String)>| async move {
                    log(&s, format!("cancel {user} {id}"));
                    Status::NO_CONTENT
                }),
            )
            .route(
                "/api/v0/application",
                get(|State(s): State<Shared>, headers: HeaderMap| async move {
                    if !authorised(&headers, &s.lock().unwrap()) {
                        return Err(Status::UNAUTHORIZED);
                    }
                    Ok(Json(serde_json::json!({
                        "version": {"full": "0.24.1.0", "current": "0.24.1"},
                        "server": {"address": "x", "state": "Connected, LoggedIn", "isConnected": true, "isLoggedIn": true}
                    })))
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}"), state)
    }

    fn config(url: &str, downloads: &Path) -> SoulseekConfig {
        SoulseekConfig {
            enabled: true,
            url: Some(url.to_owned()),
            api_key: Some(Secret::new("secret")),
            downloads_dir: Some(downloads.to_path_buf()),
            concurrency: 3,
            search_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_millis(300),
            min_bitrate: 256,
        }
    }

    fn provider(config: &SoulseekConfig) -> SoulseekProvider {
        SoulseekProvider::new(config)
            .unwrap()
            .with_poll_interval(Duration::from_millis(20))
    }

    fn query() -> SearchQuery {
        SearchQuery {
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            album: None,
            duration_ms: None,
            free_text: None,
        }
    }

    fn halo() -> Candidate {
        Candidate {
            provider: ProviderName::Soulseek,
            provider_ref: serde_json::json!({
                "username": "peer one",
                "filename": "@@music\\Beyoncé\\I Am... Sasha Fierce\\03 - Halo.flac",
                "size": 9
            }),
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            album: None,
            duration_ms: None,
            container: Some(Container::Flac),
            codec: Some(Codec::Flac),
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            lossless: true,
            size: Some(9),
            hints: SourceHints::Peer {
                username: String::from("peer one"),
                folder: String::new(),
                free_slot: true,
                queue_length: 0,
                upload_speed: 0,
            },
        }
    }

    #[tokio::test]
    async fn search_waits_for_responses_and_keeps_unlocked_audio() {
        let dir = tempfile::tempdir().unwrap();
        let (url, state) = serve(Plan::Succeed, dir.path().to_path_buf()).await;
        let found = provider(&config(&url, dir.path()))
            .search(&query(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        let halo = &found[0];
        assert_eq!(halo.title, "Halo");
        assert_eq!(halo.artist, "Beyoncé");
        assert_eq!(halo.album.as_deref(), Some("I Am... Sasha Fierce"));
        assert_eq!(halo.duration_ms, Some(261_000));
        assert!(halo.lossless);
        assert!(matches!(
            &halo.hints,
            SourceHints::Peer { free_slot: true, queue_length: 2, folder, .. } if folder == "@@music\\Beyoncé\\I Am... Sasha Fierce"
        ));
        let fake = state.lock().unwrap();
        assert_eq!(fake.log[0], "search Beyoncé Halo");
        assert!(fake.log[1].starts_with("delete search "));
        assert!(fake.search_polls >= 2);
    }

    #[tokio::test]
    async fn fetch_enqueues_follows_progress_and_moves_the_file_into_staging() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        let staging = dir.path().join("staging");
        let (url, state) = serve(Plan::Succeed, downloads.clone()).await;
        let (sink, rx) = ProgressSink::new();
        let fetched = provider(&config(&url, &downloads))
            .fetch(&halo(), &staging, sink, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fetched.path, staging.join("audio.flac"));
        assert_eq!(std::fs::read(&fetched.path).unwrap(), b"fLaCaudio");
        assert!(
            !downloads
                .join("I Am... Sasha Fierce")
                .join("03 - Halo.flac")
                .exists()
        );
        assert_eq!(rx.borrow().bytes, 9);
        let fake = state.lock().unwrap();
        assert_eq!(
            fake.log,
            ["enqueue peer one @@music\\Beyoncé\\I Am... Sasha Fierce\\03 - Halo.flac"]
        );
    }

    #[tokio::test]
    async fn a_transfer_queued_forever_is_cancelled_as_stalled() {
        let dir = tempfile::tempdir().unwrap();
        let (url, state) = serve(Plan::QueuedForever, dir.path().to_path_buf()).await;
        let (sink, _rx) = ProgressSink::new();
        let result = provider(&config(&url, dir.path()))
            .fetch(
                &halo(),
                &dir.path().join("s"),
                sink,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(ProviderError::Stalled)), "{result:?}");
        let fake = state.lock().unwrap();
        assert_eq!(
            fake.log.last().map(String::as_str),
            Some("cancel peer one 1b2c3d4e-0000-0000-0000-000000000000")
        );
    }

    #[tokio::test]
    async fn a_rejected_transfer_fails_so_the_next_candidate_is_tried() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _state) = serve(Plan::Reject, dir.path().to_path_buf()).await;
        let (sink, _rx) = ProgressSink::new();
        let result = provider(&config(&url, dir.path()))
            .fetch(
                &halo(),
                &dir.path().join("s"),
                sink,
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(ProviderError::Rejected(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_offline_peer_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _state) = serve(Plan::Offline, dir.path().to_path_buf()).await;
        let (sink, _rx) = ProgressSink::new();
        let result = provider(&config(&url, dir.path()))
            .fetch(
                &halo(),
                &dir.path().join("s"),
                sink,
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(result, Err(ProviderError::Unavailable(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn cancelling_a_fetch_cancels_the_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let (url, state) = serve(Plan::QueuedForever, dir.path().to_path_buf()).await;
        let mut cfg = config(&url, dir.path());
        cfg.stall_timeout = Duration::from_secs(60);
        let provider = provider(&cfg);
        let cancel = CancellationToken::new();
        let (sink, mut rx) = ProgressSink::new();
        let candidate = halo();
        let staging = dir.path().join("s");
        let fetch = provider.fetch(&candidate, &staging, sink, cancel.clone());
        let canceller = async {
            rx.changed().await.unwrap();
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(fetch, canceller);
        assert!(
            matches!(result, Err(ProviderError::Cancelled)),
            "{result:?}"
        );
        assert!(
            state
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|l| l.starts_with("cancel "))
        );
    }

    #[tokio::test]
    async fn resume_finds_a_transfer_that_finished_while_we_were_away() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        let (url, state) = serve(Plan::Succeed, downloads.clone()).await;
        state.lock().unwrap().polls = 5;
        let (sink, _rx) = ProgressSink::new();
        let provider = provider(&config(&url, &downloads));
        // The first poll writes the file and reports success; resume then only collects it.
        let fetched = provider
            .resume(
                &halo(),
                &dir.path().join("s"),
                sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(fetched.bytes, 9);
        assert!(
            state.lock().unwrap().log.is_empty(),
            "resume must not enqueue again"
        );
    }

    #[tokio::test]
    async fn a_wrong_api_key_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _state) = serve(Plan::Succeed, dir.path().to_path_buf()).await;
        let mut cfg = config(&url, dir.path());
        cfg.api_key = Some(Secret::new("wrong"));
        let provider = provider(&cfg);
        assert_eq!(
            provider.health().await,
            ProviderHealth::Unavailable(String::from("slskd rejected the API key"))
        );
        let result = provider.search(&query(), CancellationToken::new()).await;
        assert!(matches!(result, Err(ProviderError::Unavailable(_))));
    }

    #[tokio::test]
    async fn health_reports_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _state) = serve(Plan::Succeed, dir.path().to_path_buf()).await;
        assert_eq!(
            provider(&config(&url, dir.path())).health().await,
            ProviderHealth::Ok {
                version: Some(String::from("0.24.1.0"))
            }
        );
    }

    #[tokio::test]
    async fn connection_refused_is_unavailable() {
        // Port 1 (tcpmux) is reserved and unused here. A freed ephemeral port could be
        // taken by another test's fake server before we connect.
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(&config("http://127.0.0.1:1", dir.path()));
        assert_eq!(
            provider.health().await,
            ProviderHealth::Unavailable(String::from("cannot reach slskd"))
        );
        let result = provider.search(&query(), CancellationToken::new()).await;
        assert!(matches!(result, Err(ProviderError::Unavailable(_))));
    }

    #[tokio::test]
    async fn not_configured_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config("http://127.0.0.1:1", dir.path());
        cfg.url = None;
        let provider = provider(&cfg);
        assert!(!provider.configured());
        assert!(matches!(
            provider.health().await,
            ProviderHealth::Unavailable(_)
        ));
    }

    #[test]
    fn transfer_states_parse_flags() {
        assert_eq!(
            TransferState::parse("Completed, Succeeded"),
            TransferState::Succeeded
        );
        assert_eq!(
            TransferState::parse("Queued, Remotely"),
            TransferState::Pending
        );
        assert_eq!(TransferState::parse("InProgress"), TransferState::Pending);
        assert_eq!(
            TransferState::parse("Completed, Rejected"),
            TransferState::Failed(FailReason::Rejected)
        );
        assert_eq!(
            TransferState::parse("Completed, TimedOut"),
            TransferState::Failed(FailReason::TimedOut)
        );
        assert_eq!(
            TransferState::parse("Completed, Errored"),
            TransferState::Failed(FailReason::Other)
        );
    }

    #[test]
    fn track_numbers_are_stripped_from_file_names() {
        assert_eq!(strip_track_number("03 - Halo"), "Halo");
        assert_eq!(strip_track_number("03. Halo"), "Halo");
        assert_eq!(strip_track_number("1-03 Halo"), "Halo");
        assert_eq!(strip_track_number("1999"), "1999");
        assert_eq!(strip_track_number("Halo"), "Halo");
    }

    #[test]
    fn locate_refuses_files_outside_the_downloads_directory() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        std::fs::create_dir(&downloads).unwrap();
        std::fs::write(dir.path().join("secret.flac"), b"x").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("secret.flac"),
            downloads.join("secret.flac"),
        )
        .unwrap();
        assert!(locate(&downloads, "x\\..\\secret.flac", 1).is_err());
        assert!(locate(&downloads, "a\\secret.flac", 1).is_err());
    }
}
