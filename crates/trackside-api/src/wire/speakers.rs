//! Speakers (api.md section 20): Music Assistant players, their commands, queue and events.

use serde::{Deserialize, Serialize};

use super::Time;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerGroup {
    pub leader_id: String,
    pub member_ids: Vec<String>,
    /// `leader` or `member`.
    pub role: String,
    /// The members are fixed in Music Assistant.
    #[serde(default, rename = "static", skip_serializing_if = "std::ops::Not::not")]
    pub fixed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowPlaying {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// `queue` or `other`.
    pub origin: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_key: Option<String>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playback {
    pub position_at: Time,
    pub position_ms: u64,
    pub repeat: String,
    pub shuffle: bool,
    pub state: String,
}

/// A relay's virtual speaker, to its owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerRelay {
    pub device_id: String,
    pub session_id: String,
    pub this_device: bool,
}

/// **Speaker**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaker {
    pub availability: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub can_group_with: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PlayContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<SpeakerGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_volume: Option<u8>,
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now_playing: Option<NowPlaying>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<Playback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub powered: Option<bool>,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<SpeakerRelay>,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
}

/// `GET /speakers`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speakers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Time>,
    pub speakers: Vec<Speaker>,
    /// `ok`, `connecting` or `unavailable`.
    pub state: String,
}

/// The `202` of every command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub action: String,
    pub command_id: String,
    pub settle_by: Time,
    pub speaker: Speaker,
}

/// `POST /speakers/{id}/playback`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackRequest {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

/// `PATCH /speakers/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakerPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_volume: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub powered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
}

/// `PUT /speakers/{id}/members`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Members {
    pub member_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayItem {
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
}

/// `POST /speakers/{id}/play`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PlayContext>,
    pub items: Vec<PlayItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_index: Option<usize>,
}

/// `POST /speakers/{id}/transfer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub to: String,
    #[serde(default)]
    pub keep_playing_here: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub index: u32,
    pub item_id: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_key: Option<String>,
    pub title: String,
}

/// `GET /speakers/{id}/queue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerQueue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_index: Option<u32>,
    pub id: String,
    pub items: Vec<QueueItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub repeat: String,
    pub revision: u64,
    pub shuffle: bool,
    pub total: u32,
}

/// SSE `speaker_removed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerRemoved {
    pub id: String,
}

/// SSE `speaker_progress`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerProgress {
    pub id: String,
    pub position_at: Time,
    pub position_ms: u64,
    pub revision: u64,
    pub state: String,
}

/// SSE `speaker_queue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerQueueEvent {
    pub id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
}

/// SSE `speaker_command`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerCommandEvent {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CommandError>,
    /// `done` or `failed`.
    pub outcome: String,
    pub speaker_id: String,
}

/// SSE `speakers`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakersEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Time>,
    pub state: String,
}
