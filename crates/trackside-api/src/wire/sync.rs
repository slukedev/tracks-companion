//! Delta sync pages (api.md section 10). Snapshot pages use the same keys as delta upserts.

use serde::{Deserialize, Serialize};

use super::social::PlayQueue;
use super::{Album, Artist, Library, Playlist, Song, Time};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Snapshot,
    Delta,
}

/// Where a snapshot is: its pinned seq, the kind on this page, and the next page's cursor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub seq: i64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistWithEntries {
    pub playlist: Playlist,
    pub entries: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Star {
    pub kind: String,
    pub id: String,
    pub starred_at: Time,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rating {
    pub kind: String,
    pub id: String,
    pub rating: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayStat {
    pub id: String,
    pub play_count: u32,
    pub last_played_at: Time,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemRef {
    pub kind: String,
    pub id: String,
}

/// The records of one kind of sync, as snapshot pages and delta upserts carry them. Empty
/// arrays are left out.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Records {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<Library>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<Artist>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub albums: Vec<Album>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub songs: Vec<Song>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlists: Vec<PlaylistWithEntries>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stars: Vec<Star>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ratings: Vec<Rating>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub play_stats: Vec<PlayStat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub play_queue: Option<PlayQueue>,
}

/// Ids removed since the cursor. Empty arrays are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deletes {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub songs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub albums: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlists: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stars: Vec<ItemRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ratings: Vec<ItemRef>,
}

/// One page of `GET /sync`. A snapshot page has `snapshot`, its records flattened in, and
/// `complete` on the last page; a delta page has `upserts`, `deletes` and `has_more`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncPage {
    pub mode: SyncMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete: Option<bool>,
    #[serde(flatten)]
    pub records: Records,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upserts: Option<Records>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletes: Option<Deletes>,
    pub next_cursor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
}
