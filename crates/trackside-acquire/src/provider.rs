//! The provider interface: what every source of files implements.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use trackside_core::control::GrowingFile;
use trackside_core::records::{Codec, Container};
use trackside_core::{BoxFuture, Millis};

/// A source of files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderName {
    Ytdlp,
    /// A source an earlier version had. Rows that name one still decode, so their history
    /// lists and a file already downloaded from one still imports, but nothing fetches from it.
    Retired,
}

impl ProviderName {
    /// The providers this version has.
    pub const ALL: [Self; 1] = [Self::Ytdlp];

    /// Names earlier versions stored for sources that are gone.
    const RETIRED: &[&str] = &["soulseek"];

    /// The name in configuration, the database and admin views.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ytdlp => "ytdlp",
            Self::Retired => "retired",
        }
    }

    /// A provider this version has, by name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ytdlp" => Some(Self::Ytdlp),
            _ => None,
        }
    }

    /// A name read from a stored row: [`ProviderName::parse`], or [`ProviderName::Retired`]
    /// for a source an earlier version had.
    pub fn from_stored(s: &str) -> Option<Self> {
        Self::parse(s).or_else(|| Self::RETIRED.contains(&s).then_some(Self::Retired))
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

    /// [`Provider::fetch_streaming`] for a song that is played and not kept, so nothing is
    /// fetched for tagging.
    fn play_streaming<'a>(
        &'a self,
        candidate: &'a Candidate,
        dir: &'a Path,
        progress: ProgressSink,
        cancel: CancellationToken,
    ) -> Option<BoxFuture<'a, Result<StreamingFetch, ProviderError>>> {
        self.fetch_streaming(candidate, dir, progress, cancel)
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

    /// Settings shown in the dashboard. Never secrets.
    fn settings_view(&self) -> serde_json::Value {
        serde_json::Value::Object(serde_json::Map::new())
    }

    /// Whether the provider has what it needs to run, such as an executable.
    fn configured(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderName;

    #[test]
    fn every_provider_round_trips_through_its_name() {
        for name in ProviderName::ALL {
            assert_eq!(ProviderName::parse(name.as_str()), Some(name));
        }
    }

    #[test]
    fn a_retired_source_is_not_a_provider() {
        assert_eq!(ProviderName::parse("soulseek"), None);
    }

    #[test]
    fn a_stored_retired_source_decodes_as_retired() {
        assert_eq!(
            ProviderName::from_stored("soulseek"),
            Some(ProviderName::Retired)
        );
    }

    #[test]
    fn an_unknown_stored_name_does_not_decode() {
        assert_eq!(ProviderName::from_stored("napster"), None);
    }
}
