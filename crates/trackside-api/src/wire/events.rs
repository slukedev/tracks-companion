//! The `data` of each server-sent event (api.md section 13).

use serde::{Deserialize, Serialize};

use super::LibraryState;

/// `hello`, sent first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub server_id: String,
    pub seq: i64,
}

/// `changed`: something visible to this caller changed; call `/sync`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changed {
    pub seq: i64,
}

/// `library`: a library became available or unavailable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryEvent {
    pub id: String,
    pub state: LibraryState,
}

/// `scan`, admins only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanEvent {
    pub library_id: String,
    pub state: String,
    pub files_seen: u64,
    pub added: u64,
    pub updated: u64,
    pub missing: u64,
}

/// `upload`: one of the caller's uploads moved on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadEvent {
    pub id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
}

/// `server_urls`: the server's addresses changed; fetch `GET /server/urls`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUrlsEvent {
    pub version: String,
}

/// `session`: the credential was revoked and the stream ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub reason: String,
}
