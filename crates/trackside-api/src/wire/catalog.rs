//! The streaming catalog (api.md section 21): services, search, items, saved libraries,
//! recommendations.

use serde::{Deserialize, Deserializer, Serialize};

use super::Time;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `ok`, `signed_out`, `error` or `unavailable`.
    pub state: String,
}

/// **`CatalogService`**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogService {
    pub account: Account,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    pub id: String,
    /// With `library_edit`: the kinds that can be saved to the service's library, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub library_edit_kinds: Vec<String>,
    pub logo_key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
}

/// `GET /catalog/services`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogServices {
    pub services: Vec<CatalogService>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Time>,
    pub state: String,
}

/// SSE `catalog_services`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogServicesEvent {
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistCredit {
    pub name: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// **`CatalogTrack`**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogTrack {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<ArtistCredit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// `explicit` or `clean`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    pub kind: String,
    /// Sent only as `false`, when Music Assistant says the item cannot play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(rename = "ref")]
    pub reference: String,
    /// In the service's library, when Music Assistant knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<bool>,
    pub service_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub song_key: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
}

/// **`CatalogAlbum`**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogAlbum {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<ArtistCredit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    pub kind: String,
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// In the service's library, when Music Assistant knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<bool>,
    pub service_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
}

/// **`CatalogArtist`**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogArtist {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    pub kind: String,
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    /// In the service's library, when Music Assistant knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<bool>,
    pub service_id: String,
}

/// **`CatalogPlaylist`**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogPlaylist {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(rename = "ref")]
    pub reference: String,
    /// In the service's library, when Music Assistant knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<bool>,
    pub service_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u32>,
}

/// **`CatalogRadio`**.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRadio {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork_id: Option<String>,
    pub kind: String,
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub service_id: String,
}

/// Any catalog item, told apart by its `kind`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum CatalogItem {
    Track(CatalogTrack),
    Album(CatalogAlbum),
    Artist(CatalogArtist),
    Playlist(CatalogPlaylist),
    Radio(CatalogRadio),
}

impl<'de> Deserialize<'de> for CatalogItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let kind = value["kind"].as_str().unwrap_or_default().to_owned();
        let item = match kind.as_str() {
            "track" => serde_json::from_value(value).map(Self::Track),
            "album" => serde_json::from_value(value).map(Self::Album),
            "artist" => serde_json::from_value(value).map(Self::Artist),
            "playlist" => serde_json::from_value(value).map(Self::Playlist),
            "radio" => serde_json::from_value(value).map(Self::Radio),
            other => return Err(D::Error::custom(format!("unknown catalog kind {other:?}"))),
        };
        item.map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Items<T> {
    pub items: Vec<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceState {
    pub id: String,
    /// `ok`, `timed_out`, `failed` or `signed_out`.
    pub state: String,
}

/// `GET /catalog/search`, and the `catalog` section of `GET /search`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSearch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub albums: Option<Items<CatalogAlbum>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artists: Option<Items<CatalogArtist>>,
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlists: Option<Items<CatalogPlaylist>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radio: Option<Items<CatalogRadio>>,
    pub services: Vec<ServiceState>,
    /// Inside `/search` only, when Music Assistant is not `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracks: Option<Items<CatalogTrack>>,
}

/// `GET /catalog/items/{ref}`, by the ref's kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CatalogDetail {
    Album {
        album: CatalogAlbum,
        tracks: Vec<CatalogTrack>,
    },
    Artist {
        albums: Vec<CatalogAlbum>,
        artist: CatalogArtist,
        top_tracks: Vec<CatalogTrack>,
    },
    Playlist {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
        playlist: CatalogPlaylist,
        tracks: Vec<CatalogTrack>,
    },
    Track {
        track: CatalogTrack,
    },
    Radio {
        radio: CatalogRadio,
    },
}

/// `GET /catalog/services/{id}/library`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogLibrary {
    pub items: Vec<CatalogItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<Time>,
}

/// `PUT` and `DELETE /catalog/items/{ref}/saved`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSaved {
    #[serde(rename = "ref")]
    pub reference: String,
    pub saved: bool,
    pub service_id: String,
}

/// One song identity of `POST /catalog/match`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The client's own id, echoed in the answer.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    pub title: String,
}

/// `POST /catalog/match`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchRequest {
    pub items: Vec<MatchItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceMatch {
    pub service_id: String,
    /// `found`, `none`, `timed_out`, `failed`, `signed_out` or `deferred`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<CatalogTrack>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchResult {
    pub id: String,
    pub matches: Vec<ServiceMatch>,
}

/// The answer to `POST /catalog/match`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogMatches {
    pub complete: bool,
    pub results: Vec<MatchResult>,
    /// With `deferred` lookups: seconds until the next is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u64>,
}

/// A shelf of `GET /catalog/recommendations`: one recommendation row of one service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogShelf {
    pub id: String,
    pub items: Vec<CatalogItem>,
    pub service_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub title: String,
    /// `timeline` for a row of what was played last; absent for a row of tiles.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// `GET /catalog/recommendations`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogRecommendations {
    pub complete: bool,
    pub services: Vec<ServiceState>,
    pub shelves: Vec<CatalogShelf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_told_apart_by_kind() -> Result<(), serde_json::Error> {
        let radio =
            serde_json::json!({"kind": "radio", "name": "R", "ref": "mc1.x", "service_id": "b"});
        let item: CatalogItem = serde_json::from_value(radio.clone())?;
        assert!(matches!(item, CatalogItem::Radio(_)));
        assert_eq!(serde_json::to_value(item)?, radio);
        let unknown = serde_json::json!({"kind": "podcast", "name": "P"});
        assert!(serde_json::from_value::<CatalogItem>(unknown).is_err());
        Ok(())
    }
}
