//! Playback decisions and transcode preparation (api.md section 11).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectPlay {
    pub container: String,
    #[serde(default)]
    pub codecs: Vec<String>,
}

/// What the client can play.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientProfile {
    #[serde(default)]
    pub direct: Vec<DirectPlay>,
    #[serde(default)]
    pub hls: bool,
    #[serde(default)]
    pub max_bitrate_kbps: Option<u32>,
    #[serde(default)]
    pub preferred_profile: Option<String>,
    #[serde(default)]
    pub allow_original_fallback: bool,
}

/// `POST /playback/decisions`. `network` is `wifi`, `wired`, `cellular` or `unknown`, kept
/// only for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub song_id: String,
    #[serde(default)]
    pub network: Option<String>,
    pub client: ClientProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMode {
    Direct,
    File,
    Hls,
}

/// What the client will receive. `size` is absent for HLS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub container: String,
    pub codec: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    pub lossless: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub duration_ms: u32,
}

/// The decision. `url` is relative to the server and includes `server.base_path`; the client
/// appends `mt`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub mode: DecisionMode,
    pub url: String,
    pub delivered: Delivered,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// `POST /transcodes/prepare`, at most five songs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub songs: Vec<String>,
    pub profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedJob {
    pub song_id: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    pub jobs: Vec<PreparedJob>,
}
