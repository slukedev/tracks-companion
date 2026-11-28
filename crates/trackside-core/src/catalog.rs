//! The streaming catalog as the HTTP crates see it (contract section 2): the services
//! connected in Music Assistant, search across them, item detail, each service's saved
//! library, the services' recommendations, and the artwork Music Assistant names for them.
//!
//! Nothing here downloads, stores or exports audio. Items play on speakers inside Music
//! Assistant, or on a phone through the relay; the only bytes Trackside fetches are artwork
//! images, which the artwork cache keeps re-encoded.

use std::time::Duration;

use crate::access::Access;
use crate::assistant::SpeakersState;
use crate::control::BoxFuture;
use crate::ids::{CatalogArtId, ServiceId, ShelfId};
use crate::time::Millis;

/// How long a search waits for the slowest service.
pub const SEARCH_WAIT: Duration = Duration::from_secs(5);
/// The default and the largest `limit` of a search, per type and service.
pub const SEARCH_LIMIT: u32 = 20;
pub const SEARCH_LIMIT_MAX: u32 = 50;
/// The default and the largest page of a playlist or a saved library.
pub const PAGE_LIMIT: u32 = 100;
pub const PAGE_LIMIT_MAX: u32 = 500;
/// The most top tracks and albums an artist shows.
pub const ARTIST_TOP_TRACKS: usize = 20;
pub const ARTIST_ALBUMS: usize = 100;
/// The longest search text accepted.
pub const MAX_QUERY_LEN: usize = 200;
/// How long recommendations wait for Music Assistant, rows and their items together.
pub const RECOMMENDATIONS_WAIT: Duration = Duration::from_secs(5);
/// The default and the largest `limit` of recommendations: items per shelf.
pub const RECOMMENDATIONS_LIMIT: u32 = 20;
pub const RECOMMENDATIONS_LIMIT_MAX: u32 = 50;
/// The most shelves an answer holds.
pub const MAX_SHELVES: usize = 30;
/// The most services a request may name.
pub const MAX_SERVICES: usize = 50;

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

wire_enum!(
    /// What a service offers (contract 2.1). Sorted by name, as on the wire.
    ServiceCapability {
        Library => "library",
        Playlists => "playlists",
        Radio => "radio",
        Recommendations => "recommendations",
        Search => "search",
        Similar => "similar",
        TopTracks => "top_tracks",
    }
);

wire_enum!(
    /// Whether the owner's account at a service works.
    AccountState {
        Ok => "ok",
        SignedOut => "signed_out",
        Error => "error",
        Unavailable => "unavailable",
    }
);

wire_enum!(
    /// The best a service or a track delivers through Music Assistant (api.md 15.1's scale).
    Quality {
        Lossless => "lossless",
        High => "high",
        Standard => "standard",
    }
);

wire_enum!(
    /// How one service did in a search.
    ServiceSearchState {
        Ok => "ok",
        TimedOut => "timed_out",
        Failed => "failed",
        SignedOut => "signed_out",
    }
);

wire_enum!(
    /// The kinds of catalog item.
    CatalogKind {
        Track => "track",
        Album => "album",
        Artist => "artist",
        Playlist => "playlist",
        Radio => "radio",
    }
);

impl CatalogKind {
    /// A `types` entry: the kind or its plural.
    pub fn parse_plural(text: &str) -> Option<Self> {
        Self::parse(text.strip_suffix('s').unwrap_or(text)).or_else(|| Self::parse(text))
    }
}

/// A streaming service connected in Music Assistant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogService {
    pub id: ServiceId,
    /// The instance's name in Music Assistant.
    pub name: String,
    /// Music Assistant's provider domain, for the app's glyph.
    pub logo_key: String,
    /// Sorted, no duplicates.
    pub capabilities: Vec<ServiceCapability>,
    pub account: AccountState,
    /// English, for logs.
    pub account_detail: Option<String>,
    pub quality: Option<Quality>,
}

impl CatalogService {
    pub fn has(&self, capability: ServiceCapability) -> bool {
        self.capabilities.binary_search(&capability).is_ok()
    }
}

/// `GET /catalog/services`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceList {
    pub services: Vec<CatalogService>,
    pub state: SpeakersState,
    /// When the state became `unavailable`.
    pub since: Option<Millis>,
}

/// An artist named on a track or album, with a ref when the service has a page for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistCredit {
    pub name: String,
    pub reference: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTrack {
    pub reference: String,
    pub service: ServiceId,
    pub title: String,
    /// The artists' names as shown, which [`Self::song_key`] is computed from.
    pub artist: Option<String>,
    pub artists: Vec<ArtistCredit>,
    pub album: Option<String>,
    pub album_ref: Option<String>,
    pub artwork: Option<CatalogArtId>,
    pub duration_ms: Option<u64>,
    pub explicit: Option<bool>,
    pub isrc: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub quality: Option<Quality>,
    /// `tracks_model::song_key(title, artist)`, which owned songs are matched by.
    pub song_key: Option<String>,
    /// False only when Music Assistant says the item cannot play.
    pub playable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogAlbum {
    pub reference: String,
    pub service: ServiceId,
    pub name: String,
    pub artist: Option<String>,
    pub artists: Vec<ArtistCredit>,
    pub year: Option<u32>,
    pub artwork: Option<CatalogArtId>,
    pub track_count: Option<u32>,
    pub album_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogArtist {
    pub reference: String,
    pub service: ServiceId,
    pub name: String,
    /// `tracks_model`'s identity of the name.
    pub identity: Option<String>,
    pub artwork: Option<CatalogArtId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPlaylist {
    pub reference: String,
    pub service: ServiceId,
    pub name: String,
    pub owner: Option<String>,
    pub track_count: Option<u32>,
    pub artwork: Option<CatalogArtId>,
    pub editable: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRadio {
    pub reference: String,
    pub service: ServiceId,
    pub name: String,
    pub artwork: Option<CatalogArtId>,
}

/// Any catalog item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogEntry {
    Track(CatalogTrack),
    Album(CatalogAlbum),
    Artist(CatalogArtist),
    Playlist(CatalogPlaylist),
    Radio(CatalogRadio),
}

impl CatalogEntry {
    pub fn reference(&self) -> &str {
        match self {
            Self::Track(t) => &t.reference,
            Self::Album(a) => &a.reference,
            Self::Artist(a) => &a.reference,
            Self::Playlist(p) => &p.reference,
            Self::Radio(r) => &r.reference,
        }
    }
}

/// Which kinds a search asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[expect(clippy::struct_excessive_bools, reason = "one flag per kind")]
pub struct CatalogKinds {
    pub tracks: bool,
    pub albums: bool,
    pub artists: bool,
    pub playlists: bool,
    pub radio: bool,
}

impl CatalogKinds {
    /// The default: everything but radio.
    pub const DEFAULT: Self = Self {
        tracks: true,
        albums: true,
        artists: true,
        playlists: true,
        radio: false,
    };
    pub const NONE: Self = Self {
        tracks: false,
        albums: false,
        artists: false,
        playlists: false,
        radio: false,
    };

    #[must_use]
    pub fn with(mut self, kind: CatalogKind) -> Self {
        match kind {
            CatalogKind::Track => self.tracks = true,
            CatalogKind::Album => self.albums = true,
            CatalogKind::Artist => self.artists = true,
            CatalogKind::Playlist => self.playlists = true,
            CatalogKind::Radio => self.radio = true,
        }
        self
    }

    pub fn kinds(self) -> Vec<CatalogKind> {
        [
            (self.tracks, CatalogKind::Track),
            (self.albums, CatalogKind::Album),
            (self.artists, CatalogKind::Artist),
            (self.playlists, CatalogKind::Playlist),
            (self.radio, CatalogKind::Radio),
        ]
        .into_iter()
        .filter_map(|(on, kind)| on.then_some(kind))
        .collect()
    }

    pub fn is_empty(self) -> bool {
        self == Self::NONE
    }
}

/// `GET /catalog/search`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CatalogSearch {
    pub text: String,
    pub kinds: CatalogKinds,
    /// `None`: every service that searches and is signed in.
    pub services: Option<Vec<ServiceId>>,
    /// Per kind and service.
    pub limit: u32,
}

/// How one service answered a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceOutcome {
    pub service: ServiceId,
    pub state: ServiceSearchState,
}

/// The sections of a search, each in Music Assistant's relevance order with the services
/// interleaved best first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResults {
    pub kinds: CatalogKinds,
    pub tracks: Vec<CatalogTrack>,
    pub albums: Vec<CatalogAlbum>,
    pub artists: Vec<CatalogArtist>,
    pub playlists: Vec<CatalogPlaylist>,
    pub radio: Vec<CatalogRadio>,
    pub services: Vec<ServiceOutcome>,
    /// False when some service did not answer in time.
    pub complete: bool,
}

impl CatalogResults {
    pub fn empty(kinds: CatalogKinds) -> Self {
        Self {
            kinds,
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
            radio: Vec::new(),
            services: Vec::new(),
            complete: true,
        }
    }
}

wire_enum!(
    /// How a shelf is meant to be shown: a row of tiles, or a timeline of what was played
    /// last (Music Assistant's `RecommendationFolderType`).
    ShelfKind {
        Default => "default",
        Timeline => "timeline",
    }
);

/// `GET /catalog/recommendations`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecommendationsQuery {
    /// `None`: every signed-in service that recommends.
    pub services: Option<Vec<ServiceId>>,
    /// Items per shelf.
    pub limit: u32,
}

/// One recommendation row of one service, with the items the catalog shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shelf {
    pub id: ShelfId,
    pub service: ServiceId,
    /// As Music Assistant names the row, usually in English.
    pub title: String,
    pub subtitle: Option<String>,
    pub kind: ShelfKind,
    /// Never empty.
    pub items: Vec<CatalogEntry>,
}

/// The services' recommendations, in Music Assistant's order (one row of each service in
/// turn).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recommendations {
    pub shelves: Vec<Shelf>,
    pub services: Vec<ServiceOutcome>,
    /// False when Music Assistant did not answer for some row in time.
    pub complete: bool,
}

impl Recommendations {
    /// Every track on every shelf, for naming owned songs.
    pub fn tracks(&self) -> impl Iterator<Item = &CatalogTrack> {
        self.shelves
            .iter()
            .flat_map(|s| s.items.iter())
            .filter_map(|item| match item {
                CatalogEntry::Track(track) => Some(track),
                _ => None,
            })
    }
}

/// A cursor page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPage {
    pub cursor: Option<String>,
    pub limit: u32,
}

/// `GET /catalog/items/{ref}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogDetail {
    Track(CatalogTrack),
    /// Tracks in disc and track order.
    Album {
        album: CatalogAlbum,
        tracks: Vec<CatalogTrack>,
    },
    Artist {
        artist: CatalogArtist,
        top_tracks: Vec<CatalogTrack>,
        albums: Vec<CatalogAlbum>,
    },
    Playlist {
        playlist: CatalogPlaylist,
        tracks: Vec<CatalogTrack>,
        next_cursor: Option<String>,
    },
    Radio(CatalogRadio),
}

/// `GET /catalog/services/{id}/library`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryPage {
    pub items: Vec<CatalogEntry>,
    pub next_cursor: Option<String>,
    pub synced_at: Option<Millis>,
}

/// Why a catalog request was refused or failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    #[error("no such item")]
    NotFound,
    #[error("Music Assistant is not reachable")]
    Unavailable,
    #[error("the service is signed out in Music Assistant")]
    SignedOut { service: Option<ServiceId> },
    #[error("the service has no {capability}")]
    Unsupported { capability: &'static str },
    #[error("{message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("{0}")]
    Failed(String),
}

/// The streaming catalog, implemented by `trackside-assistant`.
pub trait CatalogControl: Send + Sync + 'static {
    /// The services listed to callers, without hidden ones.
    fn services<'a>(&'a self, caller: &'a Access) -> BoxFuture<'a, ServiceList>;

    /// Searches the services, each with the time left until [`SEARCH_WAIT`].
    fn search<'a>(
        &'a self,
        caller: &'a Access,
        query: CatalogSearch,
    ) -> BoxFuture<'a, Result<CatalogResults, CatalogError>>;

    fn item<'a>(
        &'a self,
        caller: &'a Access,
        reference: &'a str,
        page: CatalogPage,
    ) -> BoxFuture<'a, Result<CatalogDetail, CatalogError>>;

    fn library<'a>(
        &'a self,
        caller: &'a Access,
        service: &'a ServiceId,
        kind: CatalogKind,
        page: CatalogPage,
    ) -> BoxFuture<'a, Result<LibraryPage, CatalogError>>;

    /// The services' recommendation rows with their items, waiting at most
    /// [`RECOMMENDATIONS_WAIT`].
    fn recommendations<'a>(
        &'a self,
        caller: &'a Access,
        query: RecommendationsQuery,
    ) -> BoxFuture<'a, Result<Recommendations, CatalogError>>;

    /// The image address an `m` id was derived from, while it is known.
    fn art_source(&self, id: &CatalogArtId) -> Option<String>;

    /// Fetches the image an `m` id names, from an address Music Assistant gave: at most
    /// [`MAX_ART_BYTES`], with the fetch limits of contract 2.6. The bytes are not yet
    /// checked; the artwork cache does that.
    fn fetch_art<'a>(
        &'a self,
        id: &'a CatalogArtId,
    ) -> BoxFuture<'a, Result<Vec<u8>, CatalogError>>;
}

/// The largest catalog image fetched.
pub const MAX_ART_BYTES: usize = 10 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_sort_by_their_wire_names() {
        let names: Vec<&str> = ServiceCapability::ALL.iter().map(|c| c.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn kinds_take_plurals() {
        assert_eq!(
            CatalogKind::parse_plural("tracks"),
            Some(CatalogKind::Track)
        );
        assert_eq!(CatalogKind::parse_plural("radio"), Some(CatalogKind::Radio));
        assert_eq!(
            CatalogKind::parse_plural("radios"),
            Some(CatalogKind::Radio)
        );
        assert_eq!(CatalogKind::parse_plural("song"), None);
        assert_eq!(
            CatalogKinds::DEFAULT.kinds(),
            vec![
                CatalogKind::Track,
                CatalogKind::Album,
                CatalogKind::Artist,
                CatalogKind::Playlist
            ]
        );
    }
}
