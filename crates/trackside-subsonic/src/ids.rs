//! Subsonic passes every kind of id in the same parameters; the kind prefix tells them apart.

use trackside_core::records::ItemRef;
use trackside_core::{
    AlbumId, ArtistId, ArtworkId, FolderId, LibraryId, PlaylistId, PublicId, SongId,
};

use crate::error::ApiError;

/// Any id a Subsonic client may send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyId {
    Song(SongId),
    Album(AlbumId),
    Artist(ArtistId),
    Folder(FolderId),
    Library(LibraryId),
    Artwork(ArtworkId),
    Playlist(PlaylistId),
}

impl AnyId {
    /// `None` for strings that are no Trackside id, such as another server's ids or the
    /// probes Tracks sends (`tracks-no-such-cover`).
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        match s.chars().next()?.to_ascii_lowercase() {
            's' => SongId::parse(s).ok().map(Self::Song),
            'a' => AlbumId::parse(s).ok().map(Self::Album),
            'r' => ArtistId::parse(s).ok().map(Self::Artist),
            'f' => FolderId::parse(s).ok().map(Self::Folder),
            'l' => LibraryId::parse(s).ok().map(Self::Library),
            'c' => ArtworkId::parse(s).ok().map(Self::Artwork),
            'p' => PlaylistId::parse(s).ok().map(Self::Playlist),
            _ => None,
        }
    }

    /// The starrable or ratable item, if this is one.
    pub fn item(&self) -> Option<ItemRef> {
        match self {
            Self::Song(id) => Some(ItemRef::Song(id.clone())),
            Self::Album(id) => Some(ItemRef::Album(id.clone())),
            Self::Artist(id) => Some(ItemRef::Artist(id.clone())),
            _ => None,
        }
    }
}

/// Parses an id of one kind, answering error 70 for anything else: an unknown id is simply
/// not found.
pub fn parse<T: PublicId>(s: &str, what: &str) -> Result<T, ApiError> {
    T::parse(s.trim()).map_err(|_| ApiError::not_found(what))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::{AudioDigest, random_id};

    #[test]
    fn kinds_come_from_the_prefix_in_any_case() {
        let song = SongId::derive(&AudioDigest([3; 32]));
        let upper = song.as_str().to_ascii_uppercase();
        assert_eq!(AnyId::parse(&upper), Some(AnyId::Song(song.clone())));
        let playlist: PlaylistId = random_id();
        assert_eq!(
            AnyId::parse(playlist.as_str()),
            Some(AnyId::Playlist(playlist))
        );
        assert!(
            AnyId::parse(song.as_str())
                .and_then(|id| id.item())
                .is_some()
        );
    }

    #[test]
    fn foreign_ids_are_none() {
        assert_eq!(AnyId::parse("tracks-no-such-cover"), None);
        assert_eq!(AnyId::parse("ext-album-tracks-no-such-cover"), None);
        assert_eq!(AnyId::parse("42"), None);
        assert_eq!(AnyId::parse(""), None);
        assert_eq!(
            parse::<SongId>("al-123", "Song").map_err(|e| e.code),
            Err(70)
        );
    }
}
