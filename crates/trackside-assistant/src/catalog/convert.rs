//! Music Assistant media items as catalog items (contract 2.1), for one service.

use trackside_core::catalog::{
    ArtistCredit, CatalogAlbum, CatalogArtist, CatalogEntry, CatalogKind, CatalogPlaylist,
    CatalogRadio, CatalogTrack, Quality,
};
use trackside_core::catalog_ref::{CatalogRef, RefKind};
use trackside_core::{CatalogArtId, ServiceId};
use trackside_mass::models::{AudioFormat, MediaItem, MediaType};
use trackside_mass::{Endpoint, ImageSize, image_url};

use crate::convert::ArtSeen;

/// The service a listing came from, and where its images are.
pub(crate) struct Source<'a> {
    pub instance_id: &'a str,
    pub domain: &'a str,
    pub service: &'a ServiceId,
    pub endpoint: Option<&'a Endpoint>,
}

/// What one conversion found, besides the items: the art it named.
pub(crate) struct Converter<'a> {
    pub source: Source<'a>,
    pub art: ArtSeen,
}

pub(crate) fn ref_kind(kind: CatalogKind) -> RefKind {
    match kind {
        CatalogKind::Track => RefKind::Track,
        CatalogKind::Album => RefKind::Album,
        CatalogKind::Artist => RefKind::Artist,
        CatalogKind::Playlist => RefKind::Playlist,
        CatalogKind::Radio => RefKind::Radio,
    }
}

pub(crate) fn catalog_kind(kind: RefKind) -> CatalogKind {
    match kind {
        RefKind::Track => CatalogKind::Track,
        RefKind::Album => CatalogKind::Album,
        RefKind::Artist => CatalogKind::Artist,
        RefKind::Playlist => CatalogKind::Playlist,
        RefKind::Radio => CatalogKind::Radio,
    }
}

pub(crate) fn media_type(kind: CatalogKind) -> MediaType {
    match kind {
        CatalogKind::Track => MediaType::Track,
        CatalogKind::Album => MediaType::Album,
        CatalogKind::Artist => MediaType::Artist,
        CatalogKind::Playlist => MediaType::Playlist,
        CatalogKind::Radio => MediaType::Radio,
    }
}

fn kind_of(media: &MediaType) -> Option<CatalogKind> {
    Some(match media {
        MediaType::Track => CatalogKind::Track,
        MediaType::Album => CatalogKind::Album,
        MediaType::Artist => CatalogKind::Artist,
        MediaType::Playlist => CatalogKind::Playlist,
        MediaType::Radio => CatalogKind::Radio,
        _ => return None,
    })
}

/// The quality of a format: lossless codecs, else by bit rate.
fn quality_of(format: &AudioFormat) -> Option<Quality> {
    let codec = format.content_type.as_deref()?.to_ascii_lowercase();
    if ["flac", "alac", "wav", "aiff", "dsf", "pcm"]
        .iter()
        .any(|c| codec.starts_with(c))
    {
        return Some(Quality::Lossless);
    }
    match format.bit_rate {
        Some(rate) if rate >= 256 => Some(Quality::High),
        Some(rate) if rate > 0 => Some(Quality::Standard),
        _ => None,
    }
}

/// The best a service delivers, by its domain, where that is known.
pub(crate) fn service_quality(domain: &str) -> Option<Quality> {
    Some(match domain {
        "tidal" | "qobuz" => Quality::Lossless,
        "spotify" | "apple_music" | "deezer" => Quality::High,
        "soundcloud" | "ytmusic" | "tunein" | "radiobrowser" => Quality::Standard,
        _ => return None,
    })
}

/// The identity `tracks_model` gives an artist's name: its first credit's.
fn identity(name: &str) -> Option<String> {
    tracks_model::credits_by_name(name, "")
        .into_iter()
        .next()
        .map(|c| c.identity)
        .filter(|i| !i.is_empty())
}

impl Converter<'_> {
    pub(crate) fn new(source: Source<'_>) -> Converter<'_> {
        Converter {
            source,
            art: Vec::new(),
        }
    }

    /// The item's id at this service: its own when it came from the service, else its
    /// mapping to it (a library item the service also has).
    fn item_id<'m>(&self, item: &'m MediaItem) -> Option<&'m str> {
        let instance = self.source.instance_id;
        if (item.provider == instance || item.provider == self.source.domain)
            && !item.item_id.is_empty()
        {
            return Some(&item.item_id);
        }
        item.provider_mappings
            .iter()
            .find(|m| m.provider_instance == instance && !m.item_id.is_empty())
            .map(|m| m.item_id.as_str())
    }

    fn reference(&self, kind: CatalogKind, item: &MediaItem) -> Option<String> {
        let id = self.item_id(item)?;
        CatalogRef::new(
            ref_kind(kind),
            self.source.domain,
            id,
            Some(self.source.instance_id),
        )
        .encode()
    }

    fn artwork(&mut self, item: &MediaItem) -> Option<CatalogArtId> {
        let url = item.thumb().and_then(|image| {
            self.source
                .endpoint
                .and_then(|e| image_url(e, image, ImageSize::Original, None))
        })?;
        let id = CatalogArtId::derive(&url);
        self.art.push((id.clone(), url));
        Some(id)
    }

    fn credits(&self, item: &MediaItem) -> Vec<ArtistCredit> {
        item.artists
            .iter()
            .filter(|a| !a.name.is_empty())
            .map(|a| ArtistCredit {
                name: a.name.clone(),
                reference: self.reference(CatalogKind::Artist, a),
            })
            .collect()
    }

    pub(crate) fn track(&mut self, item: &MediaItem) -> Option<CatalogTrack> {
        let reference = self.reference(CatalogKind::Track, item)?;
        let title = item.name.clone();
        if title.is_empty() {
            return None;
        }
        let artist = Some(item.artist_names()).filter(|a| !a.is_empty());
        let mapping = item
            .provider_mappings
            .iter()
            .find(|m| m.provider_instance == self.source.instance_id);
        let album = item.album.as_deref();
        Some(CatalogTrack {
            reference,
            service: self.source.service.clone(),
            song_key: artist
                .as_deref()
                .map(|artist| tracks_model::song_key(&title, artist)),
            title,
            artist,
            artists: self.credits(item),
            album: album.map(|a| a.name.clone()).filter(|a| !a.is_empty()),
            album_ref: album.and_then(|a| self.reference(CatalogKind::Album, a)),
            artwork: self.artwork(item),
            duration_ms: item
                .duration
                .filter(|d| d.is_finite() && *d > 0.0)
                .map(|d| (d * 1000.0).round() as u64),
            explicit: item.metadata.as_ref().and_then(|m| m.explicit),
            isrc: item
                .external_ids
                .iter()
                .find(|e| e.kind.eq_ignore_ascii_case("isrc"))
                .map(|e| e.value.to_ascii_uppercase())
                .filter(|v| !v.is_empty()),
            track_number: item.track_number.filter(|n| *n > 0),
            disc_number: item.disc_number.filter(|n| *n > 0),
            quality: mapping.and_then(|m| m.audio_format.as_ref().and_then(quality_of)),
            playable: item.is_playable && mapping.is_none_or(|m| m.available),
        })
    }

    pub(crate) fn album(&mut self, item: &MediaItem) -> Option<CatalogAlbum> {
        let reference = self.reference(CatalogKind::Album, item)?;
        let artist = Some(item.artist_names()).filter(|a| !a.is_empty());
        let lead = item
            .artists
            .first()
            .and_then(|a| identity(&a.name))
            .unwrap_or_default();
        Some(CatalogAlbum {
            reference,
            service: self.source.service.clone(),
            album_key: (!lead.is_empty())
                .then(|| tracks_model::album_key(&item.name, &lead))
                .flatten(),
            name: item.name.clone(),
            artist,
            artists: self.credits(item),
            year: item.year.filter(|y| *y > 0),
            artwork: self.artwork(item),
            track_count: None,
        })
    }

    pub(crate) fn artist(&mut self, item: &MediaItem) -> Option<CatalogArtist> {
        Some(CatalogArtist {
            reference: self.reference(CatalogKind::Artist, item)?,
            service: self.source.service.clone(),
            identity: identity(&item.name),
            name: item.name.clone(),
            artwork: self.artwork(item),
        })
    }

    pub(crate) fn playlist(&mut self, item: &MediaItem) -> Option<CatalogPlaylist> {
        Some(CatalogPlaylist {
            reference: self.reference(CatalogKind::Playlist, item)?,
            service: self.source.service.clone(),
            name: item.name.clone(),
            owner: item.owner.clone().filter(|o| !o.is_empty()),
            track_count: None,
            artwork: self.artwork(item),
            editable: Some(item.is_editable),
        })
    }

    pub(crate) fn radio(&mut self, item: &MediaItem) -> Option<CatalogRadio> {
        Some(CatalogRadio {
            reference: self.reference(CatalogKind::Radio, item)?,
            service: self.source.service.clone(),
            name: item.name.clone(),
            artwork: self.artwork(item),
        })
    }

    /// Any item, by its own media type; `None` for kinds the catalog does not show.
    pub(crate) fn entry(&mut self, item: &MediaItem) -> Option<CatalogEntry> {
        if item.name.is_empty() {
            return None;
        }
        Some(match kind_of(&item.media_type)? {
            CatalogKind::Track => CatalogEntry::Track(self.track(item)?),
            CatalogKind::Album => CatalogEntry::Album(self.album(item)?),
            CatalogKind::Artist => CatalogEntry::Artist(self.artist(item)?),
            CatalogKind::Playlist => CatalogEntry::Playlist(self.playlist(item)?),
            CatalogKind::Radio => CatalogEntry::Radio(self.radio(item)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(value: serde_json::Value) -> MediaItem {
        serde_json::from_value(value).unwrap_or_default()
    }

    fn convert<T>(f: impl FnOnce(&mut Converter<'_>) -> T) -> (T, ArtSeen) {
        let service = ServiceId::derive("s", "tidal--x");
        let endpoint = Endpoint::parse("http://ma.local:8095").ok();
        let mut converter = Converter::new(Source {
            instance_id: "tidal--x",
            domain: "tidal",
            service: &service,
            endpoint: endpoint.as_ref(),
        });
        let out = f(&mut converter);
        (out, converter.art)
    }

    #[test]
    fn a_track_carries_the_keys_tracks_computes() {
        let halo = item(json!({
            "item_id": "1781887", "provider": "tidal--x", "name": "Halo", "media_type": "track",
            "duration": 261.64, "track_number": 3, "disc_number": 0,
            "external_ids": [["isrc", "ussm10804555"]],
            "artists": [{"item_id": "1", "provider": "tidal--x", "name": "Beyoncé", "media_type": "artist"}],
            "album": {"item_id": "1781880", "provider": "tidal--x", "name": "I Am... Sasha Fierce", "media_type": "album",
                      "image": {"type": "thumb", "path": "https://resources.tidal.com/a.jpg", "provider": "tidal", "remotely_accessible": true}},
            "metadata": {"explicit": false},
            "provider_mappings": [{"item_id": "1781887", "provider_domain": "tidal", "provider_instance": "tidal--x",
                                   "available": true, "audio_format": {"content_type": "flac", "bit_depth": 16}}],
        }));
        let (track, art) = convert(|c| c.track(&halo));
        let track = track.unwrap_or_else(|| panic!("converts"));
        assert_eq!(
            CatalogRef::decode(&track.reference),
            Ok(CatalogRef::new(
                RefKind::Track,
                "tidal",
                "1781887",
                Some("tidal--x")
            ))
        );
        assert_eq!(track.song_key.as_deref(), Some("halo\u{1f}beyonce"));
        assert_eq!(track.duration_ms, Some(261_640));
        assert_eq!(track.isrc.as_deref(), Some("USSM10804555"));
        assert_eq!(track.disc_number, None);
        assert_eq!(track.quality, Some(Quality::Lossless));
        assert_eq!(track.explicit, Some(false));
        assert!(track.playable);
        assert_eq!(
            CatalogRef::decode(track.album_ref.as_deref().unwrap_or_default())
                .map(|r| (r.kind, r.item_id)),
            Ok((RefKind::Album, String::from("1781880")))
        );
        assert_eq!(art.len(), 1, "the album's image stands in");
        assert_eq!(art[0].1, "https://resources.tidal.com/a.jpg");
    }

    #[test]
    fn library_items_use_their_mapping_and_others_are_skipped() {
        let saved = item(json!({
            "item_id": "12", "provider": "library", "name": "Saved", "media_type": "track",
            "artists": [{"item_id": "3", "provider": "library", "name": "Someone", "media_type": "artist"}],
            "provider_mappings": [
                {"item_id": "s1", "provider_domain": "opensubsonic", "provider_instance": "opensubsonic--t"},
                {"item_id": "t9", "provider_domain": "tidal", "provider_instance": "tidal--x", "available": false}
            ],
        }));
        let (track, _) = convert(|c| c.track(&saved));
        let track = track.unwrap_or_else(|| panic!("converts"));
        assert_eq!(
            CatalogRef::decode(&track.reference).map(|r| r.item_id),
            Ok(String::from("t9"))
        );
        assert!(!track.playable, "the mapping is unavailable");
        assert_eq!(
            track.artists[0].reference, None,
            "a library artist has no page here"
        );

        let elsewhere = item(json!({
            "item_id": "s1", "provider": "opensubsonic--t", "name": "Owned", "media_type": "track",
        }));
        assert!(convert(|c| c.track(&elsewhere)).0.is_none());
    }

    #[test]
    fn albums_and_artists_get_tracks_model_keys() {
        let album = item(json!({
            "item_id": "1781880", "provider": "tidal--x", "name": "I Am... Sasha Fierce",
            "media_type": "album", "year": 2008,
            "artists": [{"item_id": "1", "provider": "tidal--x", "name": "Beyoncé", "media_type": "artist"}],
        }));
        let (album, _) = convert(|c| c.album(&album));
        assert_eq!(
            album.and_then(|a| a.album_key).as_deref(),
            Some("i am... sasha fierce\u{1f}beyonce")
        );
        let artist = item(
            json!({"item_id": "1", "provider": "tidal--x", "name": "Beyoncé", "media_type": "artist"}),
        );
        let (artist, _) = convert(|c| c.artist(&artist));
        assert_eq!(artist.and_then(|a| a.identity).as_deref(), Some("beyonce"));
    }

    #[test]
    fn qualities_follow_the_format() {
        let format = |codec: &str, rate: Option<u32>| -> AudioFormat {
            serde_json::from_value(json!({"content_type": codec, "bit_rate": rate}))
                .unwrap_or_default()
        };
        assert_eq!(quality_of(&format("FLAC", None)), Some(Quality::Lossless));
        assert_eq!(quality_of(&format("ogg", Some(320))), Some(Quality::High));
        assert_eq!(
            quality_of(&format("mp3", Some(128))),
            Some(Quality::Standard)
        );
        assert_eq!(quality_of(&format("aac", None)), None);
    }
}
