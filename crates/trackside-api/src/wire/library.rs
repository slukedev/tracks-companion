//! Library browsing, search and lyrics (api.md sections 8, 9 and 11).

use serde::{Deserialize, Serialize};

use super::acquisition::Candidate;
use super::{Album, Artist, Section, Song};

/// `{"songs": [Song]}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Songs {
    pub songs: Vec<Song>,
}

/// `GET /songs/{id}/similar` and `GET /artists/{id}/top-songs`: library songs, and songs the
/// server does not have, by name. `external` is present only for callers who can request
/// songs (api.md section 8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuggestedSongs {
    pub songs: Vec<Song>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<Vec<Suggestion>>,
}

/// A song to request, not yet on the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    pub title: String,
    pub artist: String,
    /// Its cover, served by `GET /art/{artwork_id}` like library art, once it was looked up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
}

/// `GET /albums/{id}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlbumPage {
    pub album: Album,
    pub songs: Vec<Song>,
}

/// `GET /albums`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Albums {
    pub albums: Vec<Album>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `GET /artists`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artists {
    pub artists: Vec<Artist>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `GET /artists/{id}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtistPage {
    pub artist: Artist,
    pub albums: Vec<Album>,
    pub appears_on: Vec<Album>,
    pub top_songs: Vec<Song>,
    /// A short plain-text biography, when enrichment has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
}

/// One similar artist, which may not be in the library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimilarArtist {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<Artist>,
    pub name: String,
    pub identity: String,
    pub score: f32,
    pub in_library: bool,
}

/// `GET /artists/{id}/similar`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimilarArtists {
    pub artists: Vec<SimilarArtist>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genre {
    pub name: String,
    pub song_count: u64,
    pub album_count: u64,
}

/// `GET /genres`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genres {
    pub genres: Vec<Genre>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsEntry {
    pub kind: String,
    pub lang: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<i64>,
    pub lines: Vec<LyricsLine>,
}

/// `GET /songs/{id}/lyrics`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    pub lyrics: Vec<LyricsEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// `GET /folders/{id}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FolderPage {
    pub folder: Folder,
    pub folders: Vec<Folder>,
    pub songs: Vec<Song>,
}

/// Not-yet-owned results in `GET /search`, present only when `types` asks for `external` and
/// acquisition is available to the caller. Always a section of its own, never mixed into the
/// library results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSection {
    pub search_id: String,
    pub complete: bool,
    pub candidates: Vec<Candidate>,
}

/// `GET /search`. Sections that were not asked for are left out.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub songs: Option<Section<Song>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub albums: Option<Section<Album>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artists: Option<Section<Artist>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<ExternalSection>,
    /// Streaming-service results (contract 2.3), never mixed into the library sections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<super::catalog::CatalogSearch>,
}

/// An item in `GET /me/stars`: a song, album or artist.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StarredItem {
    Song(Box<Song>),
    Album(Box<Album>),
    Artist(Box<Artist>),
}

/// `GET /me/stars`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Starred {
    pub items: Vec<StarredItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
