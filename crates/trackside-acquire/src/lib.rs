//! Acquisition: the `Provider` trait and its yt-dlp implementation, ranking, verification,
//! tagging, and the durable job runner behind `AcquireControl`.
//!
//! Everything here is off unless an admin enables it. Provider names never leave this crate
//! except through the admin views.

mod acquirer;
mod external;
mod limits;
mod net;
mod process;
mod provider;
mod rank;
mod runner;
mod store;
mod tag;
mod update;
mod verify;
mod ytdlp;

pub use acquirer::{Acquirer, AcquirerDeps, DiskSpace, FreeSpace, MediaOps, Timing};
pub use limits::{Cooldown, FIRST_COOLDOWN, MAX_COOLDOWN, TokenBucket};
pub use provider::{
    Candidate, FetchedFile, Progress, ProgressSink, Provider, ProviderError, ProviderHealth,
    ProviderName, SearchQuery, SourceHints, StreamingFetch,
};
pub use rank::{
    ALLOWED_CONTAINERS, RankOptions, Ranked, clean_title, duration_fits, quality_of, rank,
    tolerance_ms,
};
pub use tag::{TagError, write_tags};
pub use update::{DEFAULT_RELEASE_BASE, UpdateError, asset_name, install as install_ytdlp};
pub use verify::{
    AudioInspector, Expectation, MAX_COMPONENT_BYTES, Rejection, TagValues, VerifiedFile,
    safe_file_name, sniff, unique_path, verify, verify_with,
};
pub use ytdlp::{
    YtdlpProvider, classify as classify_ytdlp_error, parse_progress, parse_search, valid_video_id,
    version_date, video_url,
};

#[cfg(test)]
mod tests;
