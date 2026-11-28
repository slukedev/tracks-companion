//! Media items, as Music Assistant serializes `Track`, `Album`, `Artist`, `Playlist`, `Radio`
//! and `ItemMapping` (models `media_items/media_item.py`, `provider_mapping.py`, `metadata.py`).
//! One struct covers all of them: the fields a kind does not have are empty. Browse folders
//! and recommendation rows (`BrowseFolder`, `RecommendationFolder`) have their own.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::enums::{ImageType, MediaType, RecommendationFolderType};
use crate::de;

/// An artist, album, track, playlist, radio station or item mapping.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors Music Assistant's model"
)]
pub struct MediaItem {
    #[serde(default, deserialize_with = "de::nullable")]
    pub item_id: String,
    /// The provider instance id, `library` for library items, or a provider domain.
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub media_type: MediaType,
    /// For example `library://track/12` or `spotify://track/abc`.
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub version: String,
    #[serde(default)]
    pub sort_name: Option<String>,
    #[serde(default, deserialize_with = "external_ids")]
    pub external_ids: Vec<ExternalId>,
    #[serde(default = "de::yes", deserialize_with = "de::nullable_true")]
    pub is_playable: bool,
    /// Empty for an item mapping, which is a short reference to an item.
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub provider_mappings: Vec<ProviderMapping>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub metadata: Option<Metadata>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub favorite: bool,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub position: Option<u32>,
    /// Seconds.
    #[serde(default, deserialize_with = "de::lenient_f64")]
    pub duration: Option<f64>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub artists: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_box")]
    pub album: Option<Box<MediaItem>>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub disc_number: Option<u32>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub track_number: Option<u32>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub year: Option<u32>,
    #[serde(default)]
    pub album_type: Option<String>,
    /// Playlists only.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub is_editable: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub is_dynamic: bool,
    /// Item mappings only; full items derive it from their provider mappings.
    #[serde(default)]
    pub available: Option<bool>,
    /// Item mappings only; full items carry their images in `metadata`.
    #[serde(default, deserialize_with = "de::lenient")]
    pub image: Option<Image>,
}

impl MediaItem {
    /// An item mapping is a short reference without provider mappings or metadata.
    pub fn is_mapping(&self) -> bool {
        self.provider_mappings.is_empty() && self.metadata.is_none()
    }

    /// Whether some provider can play the item right now.
    pub fn is_available(&self) -> bool {
        self.available
            .unwrap_or_else(|| self.provider_mappings.iter().any(|m| m.available))
    }

    /// The item's thumbnail: its own, else its album's, as Music Assistant picks it.
    pub fn thumb(&self) -> Option<&Image> {
        let own = self
            .metadata
            .as_ref()
            .and_then(|m| m.images.iter().find(|i| i.kind == ImageType::Thumb))
            .or(self.image.as_ref().filter(|i| i.kind == ImageType::Thumb));
        own.or_else(|| self.album.as_deref().and_then(MediaItem::thumb))
    }

    /// The artists' names joined with `/`, as Music Assistant shows them.
    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// An external id such as an ISRC or a `MusicBrainz` id, sent as a `[kind, value]` pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExternalId {
    pub kind: String,
    pub value: String,
}

fn external_ids<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ExternalId>, D::Error> {
    let Value::Array(pairs) = Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(pairs
        .into_iter()
        .filter_map(|pair| match pair {
            Value::Array(mut parts) if parts.len() == 2 => {
                let value = parts.pop()?;
                let kind = parts.pop()?;
                match (kind, value) {
                    (Value::String(kind), Value::String(value)) => Some(ExternalId { kind, value }),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect())
}

/// Where a provider has an item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderMapping {
    #[serde(default, deserialize_with = "de::nullable")]
    pub item_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider_domain: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider_instance: String,
    #[serde(default = "de::yes", deserialize_with = "de::nullable_true")]
    pub available: bool,
    #[serde(default)]
    pub in_library: Option<bool>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub audio_format: Option<AudioFormat>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
}

/// The format a provider offers an item in.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AudioFormat {
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub sample_rate: Option<u32>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub bit_depth: Option<u32>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub bit_rate: Option<u32>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub channels: Option<u32>,
}

/// The parts of `MediaItemMetadata` Trackside uses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Metadata {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub explicit: Option<bool>,
    /// Sorted by preference.
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub images: Vec<Image>,
    #[serde(default, deserialize_with = "de::string_list")]
    pub genres: Vec<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default, deserialize_with = "de::lenient_u32")]
    pub popularity: Option<u32>,
}

/// An image of an item (`MediaItemImage`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Image {
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub kind: ImageType,
    /// A URL or a provider-specific path.
    #[serde(default, deserialize_with = "de::nullable")]
    pub path: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider: String,
    /// Whether `path` is a URL anyone can fetch.
    #[serde(default, deserialize_with = "de::nullable")]
    pub remotely_accessible: bool,
    /// The id for `<base>/imageproxy/<proxy_id>`, set by servers that resolve image ids.
    #[serde(default)]
    pub proxy_id: Option<String>,
}

/// The answer to `music/search` (`SearchResults`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SearchResults {
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub artists: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub albums: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub tracks: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub playlists: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub radio: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub genres: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub audiobooks: Vec<MediaItem>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub podcasts: Vec<MediaItem>,
}

/// A folder in a browse listing, or among a recommendation row's items (`BrowseFolder`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BrowseFolder {
    #[serde(default, deserialize_with = "de::nullable")]
    pub item_id: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    /// The folder's browse path in URI style, such as `spotify--x://playlists`.
    #[serde(default, deserialize_with = "de::nullable")]
    pub path: String,
    #[serde(default, deserialize_with = "de::lenient")]
    pub uri: Option<String>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub image: Option<Image>,
}

/// One entry of a recommendation row: a full media item or an item mapping of any kind (kinds
/// this client does not know keep their `media_type` as [`MediaType::Other`], for the caller
/// to skip), or a folder. Music Assistant tells them apart the same way: `folder` is a
/// folder, anything else a media item.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum RecommendationItem {
    Media(Box<MediaItem>),
    Folder(Box<BrowseFolder>),
}

impl<'de> Deserialize<'de> for RecommendationItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(D::Error::custom("a recommendation item is an object"));
        }
        if value.get("media_type").and_then(Value::as_str) == Some("folder") {
            serde_json::from_value(value)
                .map(|folder| Self::Folder(Box::new(folder)))
                .map_err(D::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(|media| Self::Media(Box::new(media)))
                .map_err(D::Error::custom)
        }
    }
}

/// A recommendation row (`RecommendationFolder`, a `BrowseFolder` with its items), as
/// `music/recommendations` lists them. Music Assistant 2.10.14 and the 2.11 betas send the
/// rows without items, which `music/recommendations/items` reads one row at a time; older
/// servers sent them inline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RecommendationFolder {
    #[serde(default, deserialize_with = "de::nullable")]
    pub item_id: String,
    /// The provider instance id (or domain) that owns the row, `recommendations` for Music
    /// Assistant's own library rows.
    #[serde(default, deserialize_with = "de::nullable")]
    pub provider: String,
    /// The English title.
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub path: String,
    #[serde(default, deserialize_with = "de::lenient")]
    pub uri: Option<String>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub image: Option<Image>,
    /// A Material Design icon name.
    #[serde(default, deserialize_with = "de::lenient")]
    pub icon: Option<String>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub translation_key: Option<String>,
    #[serde(default, deserialize_with = "de::lenient_vec")]
    pub items: Vec<RecommendationItem>,
    #[serde(default, deserialize_with = "de::lenient")]
    pub subtitle: Option<String>,
    #[serde(rename = "type", default, deserialize_with = "de::nullable")]
    pub kind: RecommendationFolderType,
    /// Rows off by default are noisier; Music Assistant's own client hides them until the
    /// user opts in.
    #[serde(default = "de::yes", deserialize_with = "de::nullable_true")]
    pub enabled_by_default: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub supports_provider_filter: bool,
    #[serde(default, deserialize_with = "de::nullable")]
    pub supports_hide_played: bool,
}
