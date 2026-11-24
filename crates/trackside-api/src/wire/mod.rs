//! Every request and response type in api.md, with serde. Fixtures live in `tests/fixtures`;
//! `tests/fixtures.rs` builds each response with the handlers' own builders and compares.
//!
//! Absent optional fields are left out, never sent as `null` (api.md section 1).

pub mod acquisition;
pub mod admin;
pub mod auth;
pub mod cloud;
pub mod enrichment;
pub mod events;
pub mod library;
pub mod playback;
pub mod social;
pub mod sync;

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use trackside_core::Millis;

/// A time on the wire: RFC 3339 UTC with milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub Millis);

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Millis::parse_rfc3339(&text)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom(format!("not an RFC 3339 time: {text}")))
    }
}

impl From<Millis> for Time {
    fn from(value: Millis) -> Self {
        Self(value)
    }
}

/// The error body of every non-2xx response (api.md section 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// A song's file format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Format {
    pub container: String,
    pub codec: String,
    pub content_type: String,
    pub lossless: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
    pub size: u64,
}

/// One credited artist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    pub id: String,
    pub name: String,
    pub role: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReplayGain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_peak: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_peak: Option<f64>,
}

/// `MusicBrainz` ids, keyed by what they identify (`recording`, `track`, `release`,
/// `release_group`, `artist`). Left out when there are none.
pub type Mbz = BTreeMap<String, String>;

/// The caller's own data about an item, in browse responses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred_at: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default)]
    pub play_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_at: Option<Time>,
}

/// api.md section 4 Song.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub library_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<Credit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    pub duration_ms: u32,
    pub format: Format,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    pub song_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_gain: Option<ReplayGain>,
    #[serde(default, skip_serializing_if = "Mbz::is_empty")]
    pub mbz: Mbz,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<String>,
    pub has_lyrics: bool,
    pub added_at: Time,
    pub updated_at: Time,
    pub missing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub me: Option<Me>,
}

/// api.md section 4 Album.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub library_id: String,
    pub name: String,
    pub album_artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    pub is_compilation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_type: Option<String>,
    pub song_count: u32,
    pub disc_count: u32,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_key: Option<String>,
    #[serde(default, skip_serializing_if = "Mbz::is_empty")]
    pub mbz: Mbz,
    pub added_at: Time,
    pub updated_at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub me: Option<Me>,
}

/// api.md section 4 Artist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub identity: String,
    pub album_count: u32,
    pub song_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Mbz::is_empty")]
    pub mbz: Mbz,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub me: Option<Me>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub id: String,
    pub display_name: String,
}

/// api.md section 4 Playlist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: String,
    pub owner: Owner,
    pub is_public: bool,
    pub song_count: u32,
    pub duration_ms: u64,
    pub revision: i64,
    pub created_at: Time,
    pub updated_at: Time,
}

/// `ok` or `unavailable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryState {
    Ok,
    Unavailable,
}

/// api.md section 4 Library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub id: String,
    pub name: String,
    pub state: LibraryState,
    pub song_count: u64,
}

/// `{"libraries": [Library]}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Libraries {
    pub libraries: Vec<Library>,
}

/// `GET /api/v1/ping`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub server_id: String,
    pub time: Time,
    /// The endpoint proof for a `challenge` (ADR 0011); absent when none was asked or the
    /// server cannot give one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
}

/// A deprecated endpoint or field (api.md section 18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deprecation {
    pub what: String,
    pub since: String,
    pub sunset: String,
    pub r#use: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiVersions {
    pub versions: Vec<String>,
    pub current: String,
    pub deprecated: Vec<Deprecation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthFeatures {
    pub password_login: bool,
    pub pairing: bool,
    pub pair_requests: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFeatures {
    pub transcoding: bool,
    pub profiles: Vec<String>,
    pub hls: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_cellular_profile: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags on the wire"
)]
pub struct Features {
    pub lyrics: bool,
    pub uploads: bool,
    pub events: bool,
    pub animated_art: bool,
    /// Trackside Cloud is on (section 19).
    #[serde(default)]
    pub cloud: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsonicInfo {
    pub enabled: bool,
    pub path: String,
}

/// `GET /api/v1/capabilities`. Never mentions acquisition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub product: String,
    pub server_id: String,
    pub name: String,
    pub version: String,
    pub api: ApiVersions,
    pub setup_required: bool,
    pub auth: AuthFeatures,
    pub media: MediaFeatures,
    pub features: Features,
    pub subsonic: SubsonicInfo,
    /// Present while Trackside Cloud is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud: Option<cloud::CloudInfo>,
}

/// `GET /readyz` status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyStatus {
    Ready,
    Starting,
    Migrating,
}

/// The `GET /readyz` body. `503` unless `status` is `ready`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub status: ReadyStatus,
    /// `database`, `migrations`, `libraries`, `ffmpeg`: `ok`, `degraded` or `failed`.
    pub checks: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A cursor-paged list section: `{"items": [...], "next_cursor"?}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Section<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> Section<T> {
    pub fn new(items: Vec<T>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }
}
