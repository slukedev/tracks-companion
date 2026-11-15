//! Listens, stars, ratings, playlists, the play queue, bookmarks and uploads (api.md sections
//! 12 and 14).

use serde::{Deserialize, Serialize};

use super::{Playlist, Song, Time};

/// One listen in `POST /listens`. Times are kept as text so that one bad item is rejected
/// on its own instead of failing the batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenItem {
    pub client_listen_id: String,
    #[serde(default)]
    pub song_id: Option<String>,
    pub started_at: String,
    #[serde(default)]
    pub played_ms: Option<u32>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

/// `POST /listens`, at most 1000 items.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBatch {
    #[serde(default)]
    pub client_time: Option<Time>,
    pub listens: Vec<ListenItem>,
}

/// `accepted`, `duplicate` or `rejected` with `reason` (`future`, `invalid`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenResult {
    pub client_listen_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenResults {
    pub results: Vec<ListenResult>,
}

/// `POST /now-playing`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowPlaying {
    pub song_id: String,
    #[serde(default)]
    pub position_ms: u64,
    pub state: String,
}

/// The optional body of `PUT /me/stars/{kind}/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarBody {
    #[serde(default)]
    pub at: Option<Time>,
}

/// `PUT /me/ratings/{kind}/{id}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatingBody {
    pub rating: u8,
}

/// `{"playlists": [Playlist]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlists {
    pub playlists: Vec<Playlist>,
}

/// `POST /playlists`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPlaylist {
    pub name: String,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub is_public: Option<bool>,
    #[serde(default)]
    pub entries: Option<Vec<String>>,
}

/// `PATCH /playlists/{id}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub is_public: Option<bool>,
}

/// One entry of `GET /playlists/{id}`; `song` is left out when the song is gone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub position: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song: Option<Song>,
    pub song_id: String,
    pub added_at: Time,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaylistPage {
    pub playlist: Playlist,
    pub entries: Vec<PlaylistEntry>,
}

/// `PUT /playlists/{id}/entries`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceEntries {
    pub entries: Vec<String>,
}

/// `POST /playlists/{id}/entries`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendEntries {
    pub append: Vec<String>,
}

/// The saved play queue (`GET /me/play-queue` and sync).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayQueue {
    pub entries: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<u32>,
    pub position_ms: u64,
    pub changed_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub updated_at: Time,
}

/// `PUT /me/play-queue`, at most 2000 entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayQueueUpdate {
    pub entries: Vec<String>,
    #[serde(default)]
    pub current: Option<u32>,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub changed_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub song_id: String,
    pub position_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub created_at: Time,
    pub updated_at: Time,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmarks {
    pub bookmarks: Vec<Bookmark>,
}

/// `PUT /me/bookmarks/{song_id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookmarkBody {
    pub position_ms: u64,
    #[serde(default)]
    pub comment: Option<String>,
}

/// `POST /uploads`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewUpload {
    pub file_name: String,
    pub size: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}

/// An upload's state. `received` and `chunk_max` while receiving.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upload {
    pub id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_max: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `PUT /uploads/{id}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadProgress {
    pub received: u64,
}
