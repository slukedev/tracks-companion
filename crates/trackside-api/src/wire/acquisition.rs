//! Acquisition as Tracks sees it (api.md section 15). Provider names never appear here.

use serde::{Deserialize, Serialize};

use super::{Song, Time};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
}

/// A source item the server could get.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub token: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    pub quality: String,
    pub format: CandidateFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub instant: bool,
    pub r#match: String,
    /// The song's cover when it was looked up for a suggestion, served by
    /// `GET /art/{artwork_id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// Relative URLs for instant play; the client appends `mt`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    pub hls: String,
    pub progressive: String,
}

/// An acquisition request and how it is going.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acquisition {
    pub id: String,
    pub state: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    pub existing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<Stream>,
    pub created_at: Time,
    pub updated_at: Time,
}

/// `POST /acquisitions/search`: free text, or title and artist.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

/// A search across sources, with the library songs that already match.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Search {
    pub search_id: String,
    pub complete: bool,
    pub in_library: Vec<Song>,
    pub candidates: Vec<Candidate>,
}

/// `POST /acquisitions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireRequest {
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u32>,
    #[serde(default)]
    pub candidate: Option<String>,
    #[serde(default)]
    pub allow_versions: bool,
    #[serde(default)]
    pub instant: bool,
}

/// `GET /acquisitions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acquisitions {
    pub acquisitions: Vec<Acquisition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `POST /external/play`: a candidate, or title and artist.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPlayRequest {
    #[serde(default)]
    pub candidate: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

/// A song played without being kept: `stream` to play it, or `song_id` when the library has
/// it after all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalPlay {
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<Stream>,
}
