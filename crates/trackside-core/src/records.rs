//! Read models returned by repositories. Mapping to the wire lives in the api and subsonic
//! crates; these types carry typed values, not wire strings.

use std::path::PathBuf;
use std::time::Duration;

use crate::ids::{
    AlbumId, ArtistId, ArtworkId, FolderId, LibraryId, LibraryKey, ScanRunId, SongId,
};
use crate::time::Millis;

/// One page of a cursor-paginated listing. `next` is opaque and absent on the last page.
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

impl<T> Page<T> {
    /// A page with nothing after it.
    pub fn last(items: Vec<T>) -> Self {
        Self { items, next: None }
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self::last(Vec::new())
    }
}

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The value as stored in the database and sent on the wire.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// Parses a stored or wire value.
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use text_enum;

text_enum!(
    /// A file's container, named by its usual suffix.
    Container {
        Flac => "flac",
        M4a => "m4a",
        Mp3 => "mp3",
        Aac => "aac",
        Ogg => "ogg",
        Opus => "opus",
        Wav => "wav",
        Aiff => "aiff",
        WavPack => "wv",
        Ape => "ape",
    }
);

impl Container {
    /// The MIME type the file is served with.
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Flac => "audio/flac",
            Self::M4a => "audio/mp4",
            Self::Mp3 => "audio/mpeg",
            Self::Aac => "audio/aac",
            Self::Ogg | Self::Opus => "audio/ogg",
            Self::Wav => "audio/wav",
            Self::Aiff => "audio/aiff",
            Self::WavPack => "audio/x-wavpack",
            Self::Ape => "audio/x-ape",
        }
    }
}

text_enum!(
    /// The audio codec inside a container.
    Codec {
        Flac => "flac",
        Alac => "alac",
        Aac => "aac",
        Mp3 => "mp3",
        Opus => "opus",
        Vorbis => "vorbis",
        Pcm => "pcm",
        WavPack => "wavpack",
        Ape => "ape",
    }
);

impl Codec {
    /// Whether the codec keeps every bit of the source.
    pub const fn is_lossless(self) -> bool {
        matches!(
            self,
            Self::Flac | Self::Alac | Self::Pcm | Self::WavPack | Self::Ape
        )
    }
}

/// A song's file format, as indexed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub container: Container,
    pub codec: Codec,
    pub content_type: &'static str,
    pub lossless: bool,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub size: u64,
}

text_enum!(
    /// How an artist is credited on a song.
    CreditRole {
        Main => "main",
        Featured => "featured",
        AlbumArtist => "album_artist",
        Composer => "composer",
    }
);

/// One credited artist on a song.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credit {
    pub artist: ArtistId,
    pub name: String,
    pub role: CreditRole,
}

/// `ReplayGain` values from tags, in dB and linear peak.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ReplayGain {
    pub track_gain: Option<f64>,
    pub track_peak: Option<f64>,
    pub album_gain: Option<f64>,
    pub album_peak: Option<f64>,
}

/// `MusicBrainz` ids on a song.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SongMbz {
    pub recording: Option<String>,
    pub track: Option<String>,
    pub release: Option<String>,
}

text_enum!(
    /// The explicit-content tag.
    Explicit {
        Explicit => "explicit",
        Clean => "clean",
    }
);

/// A song as clients see it.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub id: SongId,
    pub library: LibraryId,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<ArtistId>,
    pub credits: Vec<Credit>,
    pub album: Option<String>,
    pub album_id: Option<AlbumId>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_subtitle: Option<String>,
    pub year: Option<i32>,
    pub release_date: Option<String>,
    pub genres: Vec<String>,
    pub duration_ms: u32,
    pub format: AudioFormat,
    pub artwork: Option<ArtworkId>,
    /// `tracks_model::song_key(title, artist)`, byte for byte what Tracks computes.
    pub song_key: String,
    pub replay_gain: Option<ReplayGain>,
    pub mbz: SongMbz,
    pub isrc: Option<String>,
    pub bpm: Option<u32>,
    pub comment: Option<String>,
    pub explicit: Option<Explicit>,
    pub has_lyrics: bool,
    pub folder: FolderId,
    pub added_at: Millis,
    pub updated_at: Millis,
    /// The file is gone but the song is still within the grace period.
    pub missing: bool,
    pub content_version: i64,
}

/// `MusicBrainz` ids on an album.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AlbumMbz {
    pub release: Option<String>,
    pub release_group: Option<String>,
}

/// An album as clients see it (api.md section 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    pub id: AlbumId,
    pub library: LibraryId,
    pub name: String,
    pub sort_name: String,
    pub album_artist: String,
    pub artist_id: Option<ArtistId>,
    pub year: Option<i32>,
    pub release_date: Option<String>,
    pub original_date: Option<String>,
    pub genres: Vec<String>,
    pub is_compilation: bool,
    pub release_type: Option<String>,
    pub song_count: u32,
    pub disc_count: u32,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub artwork: Option<ArtworkId>,
    pub album_key: Option<String>,
    pub mbz: AlbumMbz,
    pub added_at: Millis,
    pub updated_at: Millis,
}

/// An artist as clients see it. Counts are over the caller's libraries only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artist {
    pub id: ArtistId,
    pub name: String,
    pub sort_name: String,
    /// `tracks_model::folded(name)`.
    pub identity: String,
    pub album_count: u32,
    pub song_count: u32,
    pub artwork: Option<ArtworkId>,
    pub mbz_artist: Option<String>,
}

/// A music library: a root folder on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags, one per libraries column"
)]
pub struct Library {
    pub id: LibraryId,
    pub key: LibraryKey,
    pub name: String,
    pub root: PathBuf,
    pub enabled: bool,
    pub writable: bool,
    pub watch: bool,
    pub available: bool,
    pub song_count: u64,
    /// `None` means `scanner.quick_interval`.
    pub quick_scan_interval: Option<Duration>,
    pub unavailable_reason: Option<String>,
    pub unavailable_since: Option<Millis>,
    pub last_scan_at: Option<Millis>,
    pub created_at: Millis,
}

/// One user's data about one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct UserAnnotations {
    pub starred_at: Option<Millis>,
    pub rating: Option<u8>,
    pub play_count: u32,
    pub last_played_at: Option<Millis>,
}

/// Where a song's bytes are, for media serving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SongFile {
    pub song: SongId,
    pub library: LibraryKey,
    /// Absolute: the library root joined with the stored relative path.
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub content_version: i64,
    pub format: AudioFormat,
    pub duration_ms: u32,
}

text_enum!(
    /// A kind of item that can be starred or rated.
    ItemKind {
        Song => "song",
        Album => "album",
        Artist => "artist",
    }
);

/// A reference to a starrable or ratable item.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ItemRef {
    Song(SongId),
    Album(AlbumId),
    Artist(ArtistId),
}

impl ItemRef {
    pub fn kind(&self) -> ItemKind {
        match self {
            Self::Song(_) => ItemKind::Song,
            Self::Album(_) => ItemKind::Album,
            Self::Artist(_) => ItemKind::Artist,
        }
    }

    /// The item's public id.
    pub fn pid(&self) -> &str {
        use crate::ids::PublicId;
        match self {
            Self::Song(id) => id.as_str(),
            Self::Album(id) => id.as_str(),
            Self::Artist(id) => id.as_str(),
        }
    }

    /// The `change_log.entity_pid` of a star or rating on this item: `<kind>:<pid>`.
    pub fn change_entity(&self) -> String {
        format!("{}:{}", self.kind().as_str(), self.pid())
    }
}

text_enum!(
    /// What a scan run covers.
    ScanRunKind {
        Full => "full",
        Quick => "quick",
        Folder => "folder",
        File => "file",
    }
);

text_enum!(
    /// Where a scan run is.
    ScanState {
        Running => "running",
        Done => "done",
        Failed => "failed",
        Aborted => "aborted",
        Unavailable => "unavailable",
    }
);

/// Counters of a scan run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ScanCounts {
    pub folders_seen: u64,
    pub files_seen: u64,
    pub added: u64,
    pub updated: u64,
    pub moved: u64,
    pub missing: u64,
    pub errors: u64,
}

/// A scan run as recorded in `scan_runs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRun {
    pub id: ScanRunId,
    pub library: LibraryId,
    pub kind: ScanRunKind,
    pub trigger: crate::control::ScanTrigger,
    pub state: ScanState,
    pub started_at: Millis,
    pub finished_at: Option<Millis>,
    pub counts: ScanCounts,
    pub message: Option<String>,
}

text_enum!(
    /// Where an upload or inbox import is.
    ImportState {
        Receiving => "receiving",
        Staged => "staged",
        Imported => "imported",
        Duplicate => "duplicate",
        Rejected => "rejected",
        Failed => "failed",
    }
);

text_enum!(
    /// How a file reached the importer.
    ImportSource {
        Upload => "upload",
        Inbox => "inbox",
    }
);

text_enum!(
    /// Synced (LRC) or plain lyrics.
    LyricsKind {
        Synced => "synced",
        Plain => "plain",
    }
);

text_enum!(
    /// Where lyrics were found.
    LyricsSource {
        Embedded => "embedded",
        Sidecar => "sidecar",
    }
);

/// Lyrics for a song: LRC text for synced, plain text otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lyrics {
    pub kind: LyricsKind,
    pub lang: String,
    pub source: LyricsSource,
    pub content: String,
}

/// A folder in a library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub library: LibraryKey,
    pub parent: Option<FolderId>,
    pub rel_path: Vec<u8>,
    pub name: String,
    pub mtime_ns: Option<i64>,
    pub listing_hash: Option<[u8; 32]>,
    pub unreadable: bool,
    pub last_scanned_at: Option<Millis>,
    pub missing_since: Option<Millis>,
}

/// A genre with counts over the caller's libraries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenreCount {
    pub name: String,
    pub song_count: u64,
    pub album_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enums_round_trip() {
        for container in Container::ALL {
            assert_eq!(Container::parse(container.as_str()), Some(*container));
        }
        for codec in Codec::ALL {
            assert_eq!(Codec::parse(codec.as_str()), Some(*codec));
        }
        assert_eq!(
            CreditRole::parse("album_artist"),
            Some(CreditRole::AlbumArtist)
        );
        assert_eq!(ScanState::parse("nope"), None);
    }

    #[test]
    fn text_enums_serialize_as_their_stored_value() -> Result<(), serde_json::Error> {
        assert_eq!(serde_json::to_string(&Container::WavPack)?, "\"wv\"");
        assert_eq!(serde_json::from_str::<Codec>("\"alac\"")?, Codec::Alac);
        Ok(())
    }

    #[test]
    fn star_entity_names_kind_and_id() {
        let song = crate::ids::SongId::derive(&crate::ids::AudioDigest([0; 32]));
        let entity = ItemRef::Song(song.clone()).change_entity();
        assert_eq!(entity, format!("song:{song}"));
    }
}
