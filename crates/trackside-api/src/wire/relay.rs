//! The phone playback relay (api.md section 22): sessions and their HLS stream.

use serde::{Deserialize, Serialize};

use super::Time;
use super::speakers::PlayRequest;

/// `stream.format`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    pub channels: u32,
    pub codec: String,
    pub container: String,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStream {
    pub format: RelayFormat,
    /// The multivariant playlist, an absolute path; append `mt`.
    pub hls: String,
}

/// **RelaySession**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySession {
    pub created_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    pub fetch_timeout_s: u64,
    pub generation: u32,
    pub id: String,
    pub idle_timeout_s: u64,
    pub latency_ms: u32,
    pub speaker_id: String,
    /// `starting`, `ready`, `playing`, `paused` or `ended`.
    pub state: String,
    /// Absent once ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<RelayStream>,
    /// `lossless` or `cellular`.
    pub tier: String,
}

/// `GET /relay/sessions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySessions {
    pub sessions: Vec<RelaySession>,
}

/// `POST /relay/sessions`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRelay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play: Option<PlayRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// `PATCH /relay/sessions/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayPatch {
    pub tier: String,
}
