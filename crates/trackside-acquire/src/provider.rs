//! The provider interface: what every source of files implements.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use trackside_core::config::Secret;
use trackside_core::control::GrowingFile;
use trackside_core::records::{Codec, Container};
use trackside_core::{BoxFuture, Millis};

/// A source of files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderName {
    Ytdlp,
    Soulseek,
}

impl ProviderName {
    pub const ALL: [Self; 2] = [Self::Ytdlp, Self::Soulseek];

    /// The name in configuration, the database and admin views.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ytdlp => "ytdlp",
            Self::Soulseek => "soulseek",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ytdlp" => Some(Self::Ytdlp),
            "soulseek" => Some(Self::Soulseek),
            _ => None,
        }
    }
}

/// What to look for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    /// Typed text instead of fields; title and artist are then empty.
    pub free_text: Option<String>,
}

impl SearchQuery {
    /// The text sent to a source: the free text, or `<artist> <title>`.
    pub fn text(&self) -> String {
        match &self.free_text {
            Some(text) => text.trim().to_owned(),
            None => format!("{} {}", self.artist.trim(), self.title.trim())
                .trim()
                .to_owned(),
        }
    }
}

/// Source-specific facts that ranking uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceHints {
    Video {
        uploader: String,
        topic_channel: bool,
        official: bool,
        music_video: bool,
    },
    Peer {
        username: String,
        folder: String,
        free_slot: bool,
        queue_length: u32,
        upload_speed: u64,
    },
}

/// A search result from one provider.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub provider: ProviderName,
    /// What the provider needs to fetch this item again. Only identifying fields, since the
    /// candidate token is derived from it.
    pub provider_ref: serde_json::Value,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    pub container: Option<Container>,
    pub codec: Option<Codec>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub lossless: bool,
    pub size: Option<u64>,
    pub hints: SourceHints,
}

/// Bytes fetched so far and the total when known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Progress {
    pub bytes: u64,
    pub total: Option<u64>,
}

/// Where a fetch reports its progress.
#[derive(Clone, Debug)]
pub struct ProgressSink(Arc<watch::Sender<Progress>>);

impl ProgressSink {
    /// A sink and the receiver that watches it.
    pub fn new() -> (Self, watch::Receiver<Progress>) {
        let (sender, receiver) = watch::channel(Progress::default());
        (Self(Arc::new(sender)), receiver)
    }

    pub fn report(&self, bytes: u64, total: Option<u64>) {
        self.0.send_replace(Progress { bytes, total });
    }
}

/// A completed download in staging.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub thumbnail: Option<PathBuf>,
}

/// A download that can be played while it runs.
pub struct StreamingFetch {
    pub growing: GrowingFile,
    pub done: BoxFuture<'static, Result<FetchedFile, ProviderError>>,
}

impl std::fmt::Debug for StreamingFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingFetch")
            .field("growing", &self.growing)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderError {
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Missing configuration or tooling on this install, as opposed to a configured source
    /// that fails.
    #[error("not set up: {0}")]
    NotSetUp(String),
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("bot check")]
    BotCheck,
    #[error("not found")]
    NotFound,
    #[error("stalled")]
    Stalled,
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("cancelled")]
    Cancelled,
    #[error("too large")]
    TooLarge,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// Whether this failure should pause the provider (bot checks and throttling).
    pub fn wants_cooldown(&self) -> bool {
        matches!(self, Self::BotCheck | Self::RateLimited { .. })
    }
}

/// A provider's health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderHealth {
    Ok { version: Option<String> },
    Degraded(String),
    Cooldown { until: Millis },
    Outdated { version: String },
    Unavailable(String),
    NotSetUp(String),
    Disabled,
}

impl ProviderHealth {
    /// Whether searches and fetches should be tried.
    pub fn is_usable(&self) -> bool {
        matches!(
            self,
            Self::Ok { .. } | Self::Degraded(_) | Self::Outdated { .. }
        )
    }
}

/// A dashboard change to a provider's connection settings.
#[derive(Clone, Debug, Default)]
pub struct ProviderSettings {
    pub url: Option<String>,
    pub api_key: Option<Secret>,
}

/// A source of files. Implementations honour `cancel` promptly.
pub trait Provider: Send + Sync + std::fmt::Debug + 'static {
    fn name(&self) -> ProviderName;

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<Candidate>, ProviderError>>;

    fn fetch<'a>(
        &'a self,
        candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<FetchedFile, ProviderError>>;

    /// Providers that can play while downloading return a growing file and a completion
    /// future.
    fn fetch_streaming<'a>(
        &'a self,
        _candidate: &'a Candidate,
        _staging: &'a Path,
        _progress: ProgressSink,
        _cancel: CancellationToken,
    ) -> Option<BoxFuture<'a, Result<StreamingFetch, ProviderError>>> {
        None
    }

    /// Whether [`Provider::fetch_streaming`] is implemented, so candidates can offer instant
    /// play.
    fn streams(&self) -> bool {
        false
    }

    /// Continues a fetch that a restart interrupted, reusing what is in `staging`.
    fn resume<'a>(
        &'a self,
        candidate: &'a Candidate,
        staging: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<FetchedFile, ProviderError>>;

    fn health(&self) -> BoxFuture<'_, ProviderHealth>;

    /// Applies new connection settings from the dashboard.
    fn reconfigure(&self, _settings: &ProviderSettings) -> Result<(), ProviderError> {
        Err(ProviderError::Rejected(String::from(
            "this provider has no connection settings",
        )))
    }

    /// Settings shown in the dashboard. Never secrets.
    fn settings_view(&self) -> serde_json::Value {
        serde_json::Value::Object(serde_json::Map::new())
    }

    /// Whether the provider has what it needs to run (an executable, a URL).
    fn configured(&self) -> bool {
        true
    }
}
