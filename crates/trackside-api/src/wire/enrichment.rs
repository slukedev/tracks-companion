//! Last.fm enrichment in the dashboard (api.md section 16, Suggestions).

use serde::{Deserialize, Serialize};

use super::Time;

/// `GET /admin/enrichment`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrichmentStatus {
    /// An API key is set, from the dashboard or the configuration.
    pub configured: bool,
    /// Artists and songs Last.fm knew.
    pub artists: u64,
    pub songs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fetch_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<EnrichmentError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrichmentError {
    pub at: Time,
    pub message: String,
}

/// `POST /admin/enrichment/test`: a key to try before saving it, or none for the current one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrichmentTestRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrichmentTestResult {
    pub ok: bool,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
