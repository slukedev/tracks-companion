//! Players and queues (models `player.py`, `player_queue.py`, `queue_item.py`).

use serde::{Deserialize, Serialize};

use super::enums::{MediaType, PlaybackState, PlayerFeature, PlayerType, RepeatMode};
use super::media::{Image, MediaItem};
use crate::de;

/// The state of a player, as `players/all`, `players/get` and `player_updated` send it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Player {
    pub player_id: String,
    /// The instance id of the player provider.
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider: String,
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub player_type: PlayerType,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    /// The name the server shows; the same as `name` on current servers.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub available: bool,
    #[serde(default, deserialize_with = "de::lenient")]
    pub device_info: Option<DeviceInfo>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub supported_features: Vec<PlayerFeature>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub playback_state: PlaybackState,
    /// The older name of `playback_state`, which servers still send alongside it.
    #[serde(default, rename = "state", skip_serializing)]
    pub legacy_state: Option<PlaybackState>,
    /// Seconds into the current media at `elapsed_time_last_updated`.
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub elapsed_time: Option<f64>,
    /// UTC epoch seconds.
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub elapsed_time_last_updated: Option<f64>,
    #[serde(default)]
    pub powered: Option<bool>,
    /// 0 to 100.
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub volume_level: Option<u32>,
    #[serde(default)]
    pub volume_muted: Option<bool>,
    /// For a sync leader, its own id comes first.
    #[serde(default, deserialize_with = "de::string_list")]
    pub group_members: Vec<String>,
    #[serde(default, deserialize_with = "de::string_list")]
    pub static_group_members: Vec<String>,
    /// Player ids, or a whole provider instance id.
    #[serde(default, deserialize_with = "de::string_list")]
    pub can_group_with: Vec<String>,
    /// The sync leader this player follows.
    #[serde(default)]
    pub synced_to: Option<String>,
    /// A native source id or a queue id.
    #[serde(default)]
    pub active_source: Option<String>,
    #[serde(default)]
    pub active_group: Option<String>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub current_media: Option<PlayerMedia>,
    #[serde(default = "de::yes", deserialize_with = "de::nullable_true")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub hide_in_ui: bool,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub group_volume: Option<u32>,
    #[serde(default)]
    pub group_volume_muted: Option<bool>,
    #[serde(default)]
    pub power_control: Option<String>,
    #[serde(default)]
    pub volume_control: Option<String>,
    #[serde(default)]
    pub mute_control: Option<String>,
}

impl Player {
    /// The playback state, falling back to the legacy `state` field.
    pub fn state(&self) -> PlaybackState {
        match (&self.playback_state, &self.legacy_state) {
            (PlaybackState::Unknown, Some(legacy)) => legacy.clone(),
            (state, _) => state.clone(),
        }
    }

    pub fn supports(&self, feature: &PlayerFeature) -> bool {
        self.supported_features.contains(feature)
    }

    /// The position in seconds at `now` (UTC epoch seconds), advancing while playing.
    pub fn position_at(&self, now: f64) -> Option<f64> {
        let elapsed = self.elapsed_time?;
        if self.state() != PlaybackState::Playing {
            return Some(elapsed);
        }
        let since = self.elapsed_time_last_updated?;
        Some(elapsed + (now - since).max(0.0))
    }
}

/// What a player reports about the device.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeviceInfo {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub software_version: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub manufacturer_id: Option<String>,
}

/// The media loaded in a player (`PlayerMedia`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlayerMedia {
    #[serde(default, deserialize_with = "de::nullable")]
    pub uri: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub media_type: MediaType,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub duration: Option<f64>,
    /// The source, which may be a queue id.
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub queue_item_id: Option<String>,
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub elapsed_time: Option<f64>,
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub elapsed_time_last_updated: Option<f64>,
}

/// A player's queue. The queue id is the player id unless the player is grouped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors Music Assistant's model"
)]
pub struct PlayerQueue {
    pub queue_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub active: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub display_name: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub available: bool,
    /// How many items the queue holds; fetch them with `player_queues/items`.
    #[serde(default, deserialize_with = "de::lenient_u32_or_zero")]
    pub items: u32,
    #[serde(default, deserialize_with = "de::nullable")]
    pub shuffle_enabled: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub repeat_mode: RepeatMode,
    #[serde(default, deserialize_with = "de::nullable")]
    pub crossfade_enabled: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub autoplay_enabled: bool,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub current_index: Option<u32>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub index_in_buffer: Option<u32>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub ended: bool,
    /// Seconds into the current item at `elapsed_time_last_updated`.
    #[serde(default, deserialize_with = "de::lenient_f64_or_zero")]
    pub elapsed_time: f64,
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub elapsed_time_last_updated: Option<f64>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub state: PlaybackState,
    #[serde(default, deserialize_with = "de::lenient")]
    pub current_item: Option<QueueItem>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub next_item: Option<QueueItem>,
}

/// One entry of a queue (`QueueItem`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QueueItem {
    #[serde(default, deserialize_with = "de::nullable")]
    pub queue_id: String,
    pub queue_item_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    /// Seconds.
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub duration: Option<f64>,
    #[serde(default, deserialize_with = "de::lenient_u32_or_zero")]
    pub sort_index: u32,
    #[serde(default, deserialize_with = "de::lenient")]
    pub media_item: Option<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub image: Option<Image>,
    #[serde(default, deserialize_with = "de::lenient_u32_or_zero")]
    pub index: u32,
    #[serde(default = "de::yes", deserialize_with = "de::nullable_true")]
    pub available: bool,
}

impl QueueItem {
    /// The media item's URI, or the queue item id for items without one.
    pub fn uri(&self) -> &str {
        self.media_item
            .as_ref()
            .and_then(|m| m.uri.as_deref())
            .unwrap_or(&self.queue_item_id)
    }
}
