//! Catalog refs (contract 0.2): the opaque, stateless name of a streaming-service item.
//!
//! `mc1.` then base64url without padding of `media_type 0x1F provider_domain 0x1F item_id`,
//! with `0x1F instance_id` appended when the instance is known. The ref names the service by
//! its domain so it survives the owner signing out of a service and in again; the instance,
//! when present, is only a preference. Refs are case-sensitive and at most
//! [`MAX_REF_LEN`] characters, and they are URL path safe.
//!
//! This module is shared: speakers decode refs to play them, the catalog encodes them for the
//! items it lists. Clients never parse refs.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The longest ref accepted.
pub const MAX_REF_LEN: usize = 1024;

const PREFIX: &str = "mc1.";
const SEPARATOR: char = '\u{1f}';

/// What kind of item a ref names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefKind {
    Track,
    Album,
    Artist,
    Playlist,
    Radio,
}

impl RefKind {
    /// The Music Assistant media type, as it appears in refs and URIs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Radio => "radio",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "track" => Self::Track,
            "album" => Self::Album,
            "artist" => Self::Artist,
            "playlist" => Self::Playlist,
            "radio" => Self::Radio,
            _ => return None,
        })
    }
}

/// A decoded ref.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CatalogRef {
    pub kind: RefKind,
    /// Music Assistant's provider domain, such as `spotify`.
    pub domain: String,
    /// The item's id at the provider.
    pub item_id: String,
    /// The provider instance the item was seen on, such as `spotify--Xb4LkP2q`.
    pub instance_id: Option<String>,
}

/// Why a string is not a ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RefError {
    #[error("the ref is longer than {MAX_REF_LEN} characters")]
    TooLong,
    #[error("the ref does not start with mc1.")]
    Version,
    #[error("the ref is not valid base64url")]
    Encoding,
    #[error("the ref's fields are not valid")]
    Fields,
}

impl CatalogRef {
    pub fn new(kind: RefKind, domain: &str, item_id: &str, instance_id: Option<&str>) -> Self {
        Self {
            kind,
            domain: domain.to_owned(),
            item_id: item_id.to_owned(),
            instance_id: instance_id.map(str::to_owned),
        }
    }

    /// The ref as clients see it. `None` when a field holds the separator or the result
    /// would be too long, so a ref is never minted that would not decode.
    pub fn encode(&self) -> Option<String> {
        let fields = [
            Some(self.kind.as_str()),
            Some(self.domain.as_str()),
            Some(self.item_id.as_str()),
            self.instance_id.as_deref(),
        ];
        let mut plain = String::new();
        for (i, field) in fields.into_iter().flatten().enumerate() {
            if field.is_empty() || field.contains(SEPARATOR) {
                return None;
            }
            if i > 0 {
                plain.push(SEPARATOR);
            }
            plain.push_str(field);
        }
        let text = format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(plain));
        (text.len() <= MAX_REF_LEN).then_some(text)
    }

    pub fn decode(text: &str) -> Result<Self, RefError> {
        if text.len() > MAX_REF_LEN {
            return Err(RefError::TooLong);
        }
        let body = text.strip_prefix(PREFIX).ok_or(RefError::Version)?;
        let bytes = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| RefError::Encoding)?;
        let plain = String::from_utf8(bytes).map_err(|_| RefError::Encoding)?;
        let fields: Vec<&str> = plain.split(SEPARATOR).collect();
        if !(3..=4).contains(&fields.len()) || fields.iter().any(|f| f.is_empty()) {
            return Err(RefError::Fields);
        }
        let kind = RefKind::parse(fields[0]).ok_or(RefError::Fields)?;
        if fields[1]
            .chars()
            .any(|c| c.is_control() || c == ':' || c == '/')
        {
            return Err(RefError::Fields);
        }
        Ok(Self {
            kind,
            domain: fields[1].to_owned(),
            item_id: fields[2].to_owned(),
            instance_id: fields.get(3).map(|s| (*s).to_owned()),
        })
    }

    /// The Music Assistant URI on `provider` (an instance id or the domain), such as
    /// `spotify--Xb4LkP2q://track/0Wqd0zA5wNpQnQBGEUH0f4`.
    pub fn uri_on(&self, provider: &str) -> String {
        format!("{provider}://{}/{}", self.kind.as_str(), self.item_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contract_examples_decode() -> Result<(), RefError> {
        let halo = CatalogRef::decode("mc1.dHJhY2sfc3BvdGlmeR8wV3FkMHpBNXdOcFFuUUJHRVVIMGY0")?;
        assert_eq!(
            halo,
            CatalogRef::new(RefKind::Track, "spotify", "0Wqd0zA5wNpQnQBGEUH0f4", None)
        );
        let album = CatalogRef::decode("mc1.YWxidW0fdGlkYWwfMTc4MTg4MA")?;
        assert_eq!(album.kind, RefKind::Album);
        assert_eq!(album.item_id, "1781880");
        Ok(())
    }

    #[test]
    fn refs_round_trip_with_and_without_an_instance() -> Result<(), RefError> {
        for instance in [None, Some("spotify--Xb4LkP2q")] {
            let item = CatalogRef::new(
                RefKind::Playlist,
                "spotify",
                "37i9dQZF1DX4sWSpwq3LiO",
                instance,
            );
            let text = item.encode().ok_or(RefError::Fields)?;
            assert!(text.starts_with("mc1."));
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            );
            assert_eq!(CatalogRef::decode(&text)?, item);
        }
        Ok(())
    }

    #[test]
    fn refs_are_case_sensitive() {
        let text = CatalogRef::new(RefKind::Track, "spotify", "AbC", None)
            .encode()
            .unwrap_or_default();
        assert_ne!(
            CatalogRef::decode(&text.to_ascii_lowercase()).ok(),
            CatalogRef::decode(&text).ok()
        );
    }

    #[test]
    fn malformed_refs_are_refused() {
        let too_long = format!("mc1.{}", "A".repeat(MAX_REF_LEN));
        let cases = [
            ("mc2.dHJhY2sfc3BvdGlmeR8x", RefError::Version),
            ("dHJhY2sfc3BvdGlmeR8x", RefError::Version),
            ("mc1.!!!", RefError::Encoding),
            (too_long.as_str(), RefError::TooLong),
            // "song\x1fspotify\x1fx": an unknown kind.
            ("mc1.c29uZx9zcG90aWZ5H3g", RefError::Fields),
            // "track\x1fspotify": too few fields.
            ("mc1.dHJhY2sfc3BvdGlmeQ", RefError::Fields),
        ];
        for (text, error) in cases {
            assert_eq!(CatalogRef::decode(text), Err(error), "{text}");
        }
    }

    #[test]
    fn a_field_holding_the_separator_is_never_encoded() {
        let item = CatalogRef::new(RefKind::Track, "spotify", "a\u{1f}b", None);
        assert_eq!(item.encode(), None);
    }

    #[test]
    fn uris_name_the_provider_given() {
        let item = CatalogRef::new(RefKind::Album, "tidal", "1781880", None);
        assert_eq!(item.uri_on("tidal--x"), "tidal--x://album/1781880");
    }
}
