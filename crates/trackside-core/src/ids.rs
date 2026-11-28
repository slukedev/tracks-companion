//! Public ids (architecture section 6) and internal keys.
//!
//! A public id is a one-letter kind prefix and 20 lowercase Crockford base32 characters, the
//! first 100 bits of a BLAKE3 hash. Derived ids are stable across database rebuilds; the others
//! are random. Ids are case-insensitive on input and always emitted lowercase.

use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// Characters after the prefix.
pub const ID_BODY_LEN: usize = 20;

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Why a string is not a valid id of the expected kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdError {
    #[error("id has the wrong length")]
    Length,
    #[error("id should start with '{expected}'")]
    Prefix { expected: char },
    #[error("id contains a character outside the id alphabet")]
    Character,
}

/// A public id of one kind. Implemented by every id newtype in this module.
pub trait PublicId:
    Sized + Clone + Eq + std::hash::Hash + fmt::Display + Send + Sync + 'static
{
    /// The kind letter every id of this type starts with.
    const PREFIX: char;

    /// Parses an id, accepting any letter case.
    fn parse(s: &str) -> Result<Self, IdError>;

    /// The id as sent to clients, always lowercase.
    fn as_str(&self) -> &str;

    /// Builds an id from the first 100 bits of a hash.
    fn from_hash(hash: &[u8; 32]) -> Self;
}

/// The audio payload digest of architecture section 6. Tag edits do not change it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AudioDigest(pub [u8; 32]);

impl AudioDigest {
    /// The raw bytes, as stored in `songs.audio_digest`.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn encode_body(hash: &[u8; 32], prefix: char) -> Box<str> {
    let mut out = String::with_capacity(ID_BODY_LEN + 1);
    out.push(prefix);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    let mut bytes = hash.iter();
    while out.len() < ID_BODY_LEN + 1 {
        if bits < 5 {
            let next = bytes.next().copied().unwrap_or(0);
            buffer = (buffer << 8) | u32::from(next);
            bits += 8;
        }
        let index = (buffer >> (bits - 5)) & 0x1f;
        bits -= 5;
        out.push(char::from(ALPHABET[index as usize]));
    }
    out.into_boxed_str()
}

fn parse_body(s: &str, prefix: char) -> Result<Box<str>, IdError> {
    if s.len() != ID_BODY_LEN + 1 {
        return Err(IdError::Length);
    }
    let lower = s.to_ascii_lowercase();
    let mut chars = lower.chars();
    if chars.next() != Some(prefix) {
        return Err(IdError::Prefix { expected: prefix });
    }
    if !chars.all(|c| c.is_ascii() && ALPHABET.contains(&(c as u8))) {
        return Err(IdError::Character);
    }
    Ok(lower.into_boxed_str())
}

fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
}

macro_rules! public_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Box<str>);

        impl PublicId for $name {
            const PREFIX: char = $prefix;

            fn parse(s: &str) -> Result<Self, IdError> {
                parse_body(s, $prefix).map(Self)
            }

            fn as_str(&self) -> &str {
                &self.0
            }

            fn from_hash(hash: &[u8; 32]) -> Self {
                Self(encode_body(hash, $prefix))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                <Self as PublicId>::parse(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
                <Self as PublicId>::parse(&raw).map_err(serde::de::Error::custom)
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0.as_bytes())))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                <Self as PublicId>::parse(text).map_err(|e| FromSqlError::Other(Box::new(e)))
            }
        }
    };
}

public_id!(
    /// A song. Derived from the audio digest when the song is first seen, then stored.
    SongId,
    's'
);
public_id!(
    /// An album, derived from its library and identity.
    AlbumId,
    'a'
);
public_id!(
    /// An artist, derived from its folded name.
    ArtistId,
    'r'
);
public_id!(
    /// A library (random).
    LibraryId,
    'l'
);
public_id!(
    /// A folder, derived from its library and relative path.
    FolderId,
    'f'
);
public_id!(
    /// Artwork, content-addressed by its image bytes.
    ArtworkId,
    'c'
);
public_id!(
    /// Cover art for a song the server does not have, looked up for a suggestion. Derived
    /// from the image's own name at its source, so it is the same for every user and every
    /// song on the same album.
    ExternalArtId,
    'e'
);
public_id!(
    /// A playlist (random).
    PlaylistId,
    'p'
);
public_id!(
    /// A user (random).
    UserId,
    'u'
);
public_id!(
    /// A device (random).
    DeviceId,
    'd'
);
public_id!(
    /// A device key, API key or app password (random).
    KeyId,
    'k'
);
public_id!(
    /// An acquisition request (random).
    AcquisitionId,
    'q'
);
public_id!(
    /// An acquisition candidate, derived from its provider and provider reference.
    CandidateToken,
    'x'
);
public_id!(
    /// A scan run (random).
    ScanRunId,
    'n'
);
public_id!(
    /// An upload or inbox import (random).
    ImportId,
    'i'
);
public_id!(
    /// A pairing offer or request (random).
    PairingId,
    'g'
);
public_id!(
    /// An invite (random).
    InviteId,
    'v'
);
public_id!(
    /// A speaker: a Music Assistant player, derived from the Music Assistant server and the
    /// player's id there, so it stays the same while Music Assistant keeps the player.
    SpeakerId,
    'o'
);
public_id!(
    /// A streaming service: a Music Assistant music provider instance, derived from the
    /// server and the instance id.
    ServiceId,
    'b'
);
public_id!(
    /// A speaker command (random), named in its `202` and its outcome event.
    SpeakerCommandId,
    'j'
);
public_id!(
    /// Artwork of a catalog item, derived from the image's address as Music Assistant gave it.
    CatalogArtId,
    'm'
);
public_id!(
    /// A phone playback relay session (random); sessions end when the server stops.
    RelaySessionId,
    'h'
);
public_id!(
    /// A recommendation shelf: a Music Assistant recommendation row, derived from the server,
    /// the provider instance that owns the row and the row's id there.
    ShelfId,
    'w'
);

impl SongId {
    /// The id a song gets when it is first seen: `BLAKE3("song" ‖ digest)`.
    pub fn derive(digest: &AudioDigest) -> Self {
        Self::from_hash(&hash_parts(&[b"song", &digest.0]))
    }

    /// The id for an exact duplicate of a present song, with its location mixed in:
    /// `BLAKE3("song" ‖ digest ‖ library ‖ rel_path)`.
    pub fn derive_disambiguated(
        digest: &AudioDigest,
        library: &LibraryId,
        rel_path: &[u8],
    ) -> Self {
        Self::from_hash(&hash_parts(&[
            b"song",
            &digest.0,
            library.as_str().as_bytes(),
            rel_path,
        ]))
    }
}

impl AlbumId {
    /// `BLAKE3("album" ‖ library ‖ identity)`.
    pub fn derive(library: &LibraryId, identity: &str) -> Self {
        Self::from_hash(&hash_parts(&[
            b"album",
            library.as_str().as_bytes(),
            identity.as_bytes(),
        ]))
    }
}

impl ArtistId {
    /// `BLAKE3("artist" ‖ identity)`, where identity is `tracks_model::folded(name)`.
    pub fn derive(identity: &str) -> Self {
        Self::from_hash(&hash_parts(&[b"artist", identity.as_bytes()]))
    }
}

impl FolderId {
    /// `BLAKE3("folder" ‖ library ‖ rel_path)`.
    pub fn derive(library: &LibraryId, rel_path: &[u8]) -> Self {
        Self::from_hash(&hash_parts(&[
            b"folder",
            library.as_str().as_bytes(),
            rel_path,
        ]))
    }
}

impl ArtworkId {
    /// `BLAKE3(image bytes)`: the same image always has the same id.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self::from_hash(blake3::hash(bytes).as_bytes())
    }
}

impl ExternalArtId {
    /// `BLAKE3("external-art" ‖ image)`, where `image` names the image at its source (the
    /// file name Last.fm gives it).
    pub fn derive(image: &str) -> Self {
        Self::from_hash(&hash_parts(&[b"external-art", image.as_bytes()]))
    }
}

impl CandidateToken {
    /// `BLAKE3("cand" ‖ provider ‖ provider_ref)`, stable for the same source item.
    pub fn derive(provider: &str, provider_ref: &str) -> Self {
        Self::from_hash(&hash_parts(&[
            b"cand",
            provider.as_bytes(),
            provider_ref.as_bytes(),
        ]))
    }
}

impl SpeakerId {
    /// `BLAKE3("speaker" ‖ ma_server_id ‖ ma_player_id)`.
    pub fn derive(server_id: &str, player_id: &str) -> Self {
        Self::from_hash(&hash_parts(&[
            b"speaker",
            server_id.as_bytes(),
            player_id.as_bytes(),
        ]))
    }
}

impl ServiceId {
    /// `BLAKE3("service" ‖ ma_server_id ‖ provider_instance_id)`.
    pub fn derive(server_id: &str, instance_id: &str) -> Self {
        Self::from_hash(&hash_parts(&[
            b"service",
            server_id.as_bytes(),
            instance_id.as_bytes(),
        ]))
    }
}

impl ShelfId {
    /// `BLAKE3("shelf" ‖ ma_server_id ‖ 0x1F ‖ provider_instance_id ‖ 0x1F ‖ row_item_id)`.
    pub fn derive(server_id: &str, instance_id: &str, row_id: &str) -> Self {
        Self::from_hash(&hash_parts(&[
            b"shelf",
            server_id.as_bytes(),
            b"\x1f",
            instance_id.as_bytes(),
            b"\x1f",
            row_id.as_bytes(),
        ]))
    }
}

impl CatalogArtId {
    /// `BLAKE3("catalog-art" ‖ source)`, where `source` is the image address exactly as Music
    /// Assistant sent it.
    pub fn derive(source: &str) -> Self {
        Self::from_hash(&hash_parts(&[b"catalog-art", source.as_bytes()]))
    }
}

/// A new random id of any kind.
pub fn random_id<T: PublicId>() -> T {
    // A v4 UUID has 122 random bits; hashing it spreads them over the 100 the id keeps.
    let seed = uuid::Uuid::new_v4();
    T::from_hash(blake3::hash(seed.as_bytes()).as_bytes())
}

macro_rules! internal_key {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
        pub struct $name(pub i64);

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.0))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value.as_i64().map(Self)
            }
        }
    };
}

internal_key!(
    /// `users.id`. Never sent to clients.
    UserKey
);
internal_key!(
    /// `libraries.id`. Never sent to clients.
    LibraryKey
);
internal_key!(
    /// `devices.id`. Never sent to clients.
    DeviceKey
);
internal_key!(
    /// A `change_log` sequence number, assigned by the single writer and never reused.
    Seq
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_and_service_ids_depend_on_the_server_and_the_object() {
        let kitchen = SpeakerId::derive("8f2e", "ap8c1f2e3d4b5a");
        assert_eq!(kitchen, SpeakerId::derive("8f2e", "ap8c1f2e3d4b5a"));
        assert_ne!(kitchen, SpeakerId::derive("9f2e", "ap8c1f2e3d4b5a"));
        assert!(kitchen.as_str().starts_with('o'));
        let spotify = ServiceId::derive("8f2e", "spotify--Xb4LkP2q");
        assert!(spotify.as_str().starts_with('b'));
        assert_ne!(
            spotify.as_str()[1..],
            ServiceId::derive("8f2e", "ap8c1f2e3d4b5a").as_str()[1..]
        );
        assert!(
            CatalogArtId::derive("https://i.scdn.co/image/ab67")
                .as_str()
                .starts_with('m')
        );
    }

    const DIGEST: AudioDigest = AudioDigest([7; 32]);

    #[test]
    // Golden vectors computed independently with Python's `blake3` and `base64.b32encode`.
    fn song_id_derivation_is_stable() -> Result<(), IdError> {
        let library = LibraryId::parse("l0000000000000000000a")?;
        assert_eq!(SongId::derive(&DIGEST).as_str(), "skj82k53wq05b5jb8zqpq");
        assert_eq!(
            SongId::derive_disambiguated(&DIGEST, &library, b"Artist/Album/01 Song.flac").as_str(),
            "stz5ccydwy3d3w1hndrpg"
        );
        assert_eq!(
            AlbumId::derive(&library, "mb:1234").as_str(),
            "a7zxb8zs68f2ch4k6ntea"
        );
        assert_eq!(
            ArtistId::derive("beyonce").as_str(),
            "rgtm6gw8kzkjyq611mzzp"
        );
        assert_eq!(
            FolderId::derive(&library, b"Artist").as_str(),
            "fn14wrb840nan1a4x4tfx"
        );
        assert_eq!(
            ArtworkId::of_bytes(b"png").as_str(),
            "cqbbwk8sdchegxd2vwxjh"
        );
        assert_eq!(
            CandidateToken::derive("ytdlp", "{\"id\":\"abc\"}").as_str(),
            "xpazd3k0b9k58vh2k4zf4"
        );
        Ok(())
    }

    #[test]
    fn ids_are_twenty_characters_after_the_prefix() {
        let id = SongId::derive(&DIGEST);
        assert_eq!(id.as_str().len(), ID_BODY_LEN + 1);
        assert!(id.as_str().starts_with('s'));
    }

    #[test]
    fn ids_parse_case_insensitively() -> Result<(), IdError> {
        let id = SongId::derive(&DIGEST);
        let upper = id.as_str().to_ascii_uppercase();
        assert_eq!(SongId::parse(&upper)?, id);
        assert_eq!(SongId::parse(&upper)?.as_str(), id.as_str());
        Ok(())
    }

    #[test]
    fn ids_reject_wrong_prefix() {
        let album = AlbumId::derive(&LibraryId::from_hash(&[1; 32]), "x");
        assert_eq!(
            SongId::parse(album.as_str()),
            Err(IdError::Prefix { expected: 's' })
        );
    }

    #[test]
    fn ids_reject_wrong_length_and_characters() {
        assert_eq!(SongId::parse("s123"), Err(IdError::Length));
        assert_eq!(
            SongId::parse("s0000000000000000000u"),
            Err(IdError::Character)
        );
        let accented = format!("s{}é", "0".repeat(ID_BODY_LEN - 2));
        assert_eq!(accented.len(), ID_BODY_LEN + 1);
        assert_eq!(SongId::parse(&accented), Err(IdError::Character));
    }

    #[test]
    fn random_ids_differ_and_parse() -> Result<(), IdError> {
        let a: PlaylistId = random_id();
        let b: PlaylistId = random_id();
        assert_ne!(a, b);
        assert_eq!(PlaylistId::parse(a.as_str())?, a);
        Ok(())
    }

    #[test]
    fn ids_round_trip_through_json() -> Result<(), serde_json::Error> {
        let id = SongId::derive(&DIGEST);
        let json = serde_json::to_string(&id)?;
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<SongId>(&json)?, id);
        assert!(serde_json::from_str::<AlbumId>(&json).is_err());
        Ok(())
    }
}
