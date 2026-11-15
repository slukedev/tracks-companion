//! Core records as Subsonic shapes, with the `OpenSubsonic` fields api.md section 17 lists.
//!
//! `OpenSubsonic` asks a server to send every added field it supports even when empty, so
//! clients can tell "unknown" from "unsupported"; the fields here follow that.

use std::collections::HashMap;

use trackside_core::records::{Album, Artist, CreditRole, ItemRef, Song, UserAnnotations};
use trackside_core::repo::playlists::Playlist;
use trackside_core::{Millis, PublicId};

use crate::response::{Node, Value};

/// The caller's stars, ratings and plays for the items in one response.
pub type Notes = HashMap<ItemRef, UserAnnotations>;

fn note(notes: &Notes, item: &ItemRef) -> UserAnnotations {
    notes.get(item).copied().unwrap_or_default()
}

/// Whole seconds, rounded, as Subsonic counts durations.
pub fn seconds(ms: u64) -> u64 {
    (ms + 500) / 1000
}

pub fn time(at: Millis) -> String {
    at.to_rfc3339()
}

fn path_part(text: &str) -> String {
    text.replace(['/', '\\'], "_")
}

/// The path Subsonic clients show and use to name offline copies. It is virtual: real paths
/// never leave the server.
fn virtual_path(song: &Song) -> String {
    let artist = song.album_artist.as_deref().unwrap_or(&song.artist);
    let album = song.album.as_deref().unwrap_or("Unknown Album");
    let track = song
        .track_number
        .map(|n| format!("{n:02} - "))
        .unwrap_or_default();
    format!(
        "{}/{}/{track}{}.{}",
        path_part(artist),
        path_part(album),
        path_part(&song.title),
        song.format.container.as_str()
    )
}

fn artist_ref(id: &str, name: &str) -> Node {
    Node::new().attr("id", id).attr("name", name)
}

fn genre_nodes(genres: &[String]) -> Vec<Node> {
    genres
        .iter()
        .map(|g| Node::new().attr("name", g.as_str()))
        .collect()
}

fn annotate(mut node: Node, n: &UserAnnotations, play_count_always: bool) -> Node {
    if let Some(at) = n.starred_at {
        node.set("starred", time(at));
    }
    if let Some(rating) = n.rating {
        node.set("userRating", rating);
    }
    if play_count_always || n.play_count > 0 {
        node.set("playCount", n.play_count);
    }
    if let Some(at) = n.last_played_at {
        node.set("played", time(at));
    }
    node
}

/// A song as a Subsonic `Child`.
pub fn song(song: &Song, notes: &Notes) -> Node {
    let format = &song.format;
    let parent = song
        .album_id
        .as_ref()
        .map_or_else(|| song.folder.as_str(), |a| a.as_str());
    let mut node = Node::new()
        .attr("id", song.id.as_str())
        .attr("parent", parent)
        .attr("isDir", false)
        .attr("title", song.title.as_str())
        .attr_opt("album", song.album.as_deref())
        .attr("artist", song.artist.as_str())
        .attr_opt("track", song.track_number)
        .attr_opt("year", song.year)
        .attr_opt("genre", song.genres.first())
        .attr_opt("coverArt", song.artwork.as_ref().map(PublicId::as_str))
        .attr("size", format.size)
        .attr("contentType", format.content_type)
        .attr("suffix", format.container.as_str())
        .attr("duration", seconds(u64::from(song.duration_ms)))
        .attr_opt("bitRate", format.bitrate_kbps)
        .attr_opt("bitDepth", format.bit_depth)
        .attr_opt("samplingRate", format.sample_rate)
        .attr_opt("channelCount", format.channels)
        .attr("path", virtual_path(song))
        .attr("isVideo", false)
        .attr_opt("discNumber", song.disc_number)
        .attr("created", time(song.added_at))
        .attr_opt("albumId", song.album_id.as_ref().map(PublicId::as_str))
        .attr_opt("artistId", song.artist_id.as_ref().map(PublicId::as_str))
        .attr("type", "music")
        .attr("mediaType", "song")
        .attr("bpm", song.bpm.unwrap_or(0))
        .attr("comment", song.comment.as_deref().unwrap_or(""))
        .attr("sortName", song.title.as_str())
        .attr("musicBrainzId", song.mbz.recording.as_deref().unwrap_or(""))
        .attr("displayArtist", song.artist.as_str())
        .attr(
            "displayAlbumArtist",
            song.album_artist.as_deref().unwrap_or(""),
        )
        .attr("explicitStatus", song.explicit.map_or("", |e| e.as_str()));
    node = annotate(node, &note(notes, &ItemRef::Song(song.id.clone())), true);

    let mut artists: Vec<Node> = song
        .credits
        .iter()
        .filter(|c| matches!(c.role, CreditRole::Main | CreditRole::Featured))
        .map(|c| artist_ref(c.artist.as_str(), &c.name))
        .collect();
    if artists.is_empty()
        && let Some(id) = &song.artist_id
    {
        artists.push(artist_ref(id.as_str(), &song.artist));
    }
    let album_artists: Vec<Node> = song
        .credits
        .iter()
        .filter(|c| c.role == CreditRole::AlbumArtist)
        .map(|c| artist_ref(c.artist.as_str(), &c.name))
        .collect();
    let mut gain = Node::new();
    if let Some(rg) = song.replay_gain {
        gain = gain
            .attr_opt("trackGain", rg.track_gain)
            .attr_opt("trackPeak", rg.track_peak)
            .attr_opt("albumGain", rg.album_gain)
            .attr_opt("albumPeak", rg.album_peak);
    }
    node.texts(
        "isrc",
        song.isrc.iter().map(|i| Value::from(i.as_str())).collect(),
    )
    .many("genres", genre_nodes(&song.genres))
    .many("artists", artists)
    .many("albumArtists", album_artists)
    .one("replayGain", gain)
}

/// `{year, month, day}` from `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
pub fn item_date(date: &str) -> Option<Node> {
    let mut parts = date.trim().splitn(3, '-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: Option<u32> = parts.next().and_then(|m| m.parse().ok());
    let day: Option<u32> = parts
        .next()
        .and_then(|d| d.get(..2).unwrap_or(d).parse().ok());
    Some(
        Node::new()
            .attr("year", year)
            .attr_opt("month", month)
            .attr_opt("day", month.and(day)),
    )
}

/// An album as a Subsonic `AlbumID3`.
pub fn album_id3(album: &Album, notes: &Notes) -> Node {
    let mut node = Node::new()
        .attr("id", album.id.as_str())
        .attr("name", album.name.as_str())
        .attr("artist", album.album_artist.as_str())
        .attr_opt("artistId", album.artist_id.as_ref().map(PublicId::as_str))
        .attr_opt("coverArt", album.artwork.as_ref().map(PublicId::as_str))
        .attr("songCount", album.song_count)
        .attr("duration", seconds(album.duration_ms))
        .attr("created", time(album.added_at))
        .attr_opt("year", album.year)
        .attr_opt("genre", album.genres.first())
        .attr("musicBrainzId", album.mbz.release.as_deref().unwrap_or(""))
        .attr("displayArtist", album.album_artist.as_str())
        .attr("sortName", album.sort_name.as_str())
        .attr("isCompilation", album.is_compilation);
    node = annotate(node, &note(notes, &ItemRef::Album(album.id.clone())), false);
    let artists = album
        .artist_id
        .iter()
        .map(|id| artist_ref(id.as_str(), &album.album_artist))
        .collect();
    let release_types = album
        .release_type
        .iter()
        .map(|t| Value::from(t.as_str()))
        .collect();
    node = node
        .many("genres", genre_nodes(&album.genres))
        .many("artists", artists)
        .texts("releaseTypes", release_types);
    if let Some(date) = album.original_date.as_deref().and_then(item_date) {
        node = node.one("originalReleaseDate", date);
    }
    if let Some(date) = album.release_date.as_deref().and_then(item_date) {
        node = node.one("releaseDate", date);
    }
    node
}

/// An album as a directory `Child`, for `getAlbumList`, `search2` and folder browsing.
pub fn album_child(album: &Album, notes: &Notes) -> Node {
    let mut node = Node::new()
        .attr("id", album.id.as_str())
        .attr_opt("parent", album.artist_id.as_ref().map(PublicId::as_str))
        .attr("isDir", true)
        .attr("title", album.name.as_str())
        .attr("name", album.name.as_str())
        .attr("album", album.name.as_str())
        .attr("artist", album.album_artist.as_str())
        .attr_opt("year", album.year)
        .attr_opt("genre", album.genres.first())
        .attr_opt("coverArt", album.artwork.as_ref().map(PublicId::as_str))
        .attr("duration", seconds(album.duration_ms))
        .attr("songCount", album.song_count)
        .attr("created", time(album.added_at))
        .attr_opt("artistId", album.artist_id.as_ref().map(PublicId::as_str))
        .attr("mediaType", "album");
    node = annotate(node, &note(notes, &ItemRef::Album(album.id.clone())), false);
    node
}

/// An artist as a Subsonic `ArtistID3` (also used where `Artist` is asked for; the extra
/// fields are harmless there).
pub fn artist(artist: &Artist, notes: &Notes) -> Node {
    let node = Node::new()
        .attr("id", artist.id.as_str())
        .attr("name", artist.name.as_str())
        .attr_opt("coverArt", artist.artwork.as_ref().map(PublicId::as_str))
        .attr("albumCount", artist.album_count)
        .attr("musicBrainzId", artist.mbz_artist.as_deref().unwrap_or(""))
        .attr("sortName", artist.sort_name.as_str());
    let n = note(notes, &ItemRef::Artist(artist.id.clone()));
    node.attr_opt("starred", n.starred_at.map(time))
        .attr_opt("userRating", n.rating)
}

/// A playlist. `owner` is the owner's username, which clients compare to their own to allow
/// editing.
pub fn playlist(playlist: &Playlist, owner: &str, cover: Option<&str>) -> Node {
    Node::new()
        .attr("id", playlist.id.as_str())
        .attr("name", playlist.name.as_str())
        .attr("comment", playlist.comment.as_deref().unwrap_or(""))
        .attr("owner", owner)
        .attr("public", playlist.is_public)
        .attr("songCount", playlist.song_count)
        .attr("duration", seconds(playlist.duration_ms))
        .attr("created", time(playlist.created_at))
        .attr("changed", time(playlist.updated_at))
        .attr_opt("coverArt", cover)
}

/// The index letter of an artist: the first letter of its sort name, `#` for anything else.
pub fn index_letter(sort_name: &str, name: &str) -> String {
    let source = if sort_name.trim().is_empty() {
        name
    } else {
        sort_name
    };
    match tracks_model::folded(source).chars().next() {
        Some(c) if c.is_alphabetic() => c.to_uppercase().collect(),
        _ => String::from("#"),
    }
}

/// Artists grouped into `index` nodes by letter, `#` last.
pub fn indexes(artists: &[Artist], notes: &Notes) -> Vec<Node> {
    let mut groups: Vec<(String, Vec<Node>)> = Vec::new();
    for a in artists {
        let letter = index_letter(&a.sort_name, &a.name);
        match groups.iter_mut().find(|(l, _)| *l == letter) {
            Some((_, group)) => group.push(artist(a, notes)),
            None => groups.push((letter, vec![artist(a, notes)])),
        }
    }
    groups.sort_by(|(a, _), (b, _)| match (a.as_str(), b.as_str()) {
        ("#", "#") => std::cmp::Ordering::Equal,
        ("#", _) => std::cmp::Ordering::Greater,
        (_, "#") => std::cmp::Ordering::Less,
        _ => a.cmp(b),
    });
    groups
        .into_iter()
        .map(|(letter, nodes)| Node::new().attr("name", letter).many("artist", nodes))
        .collect()
}

#[cfg(test)]
pub(crate) mod fixtures {
    use trackside_core::records::{
        AlbumMbz, AudioFormat, Codec, Container, Credit, Explicit, ReplayGain, SongMbz,
    };
    use trackside_core::{AlbumId, ArtistId, ArtworkId, AudioDigest, FolderId, LibraryId, SongId};

    use super::*;

    pub fn library() -> LibraryId {
        LibraryId::from_hash(&[9; 32])
    }

    pub fn beyonce() -> ArtistId {
        ArtistId::derive("beyonce")
    }

    pub fn album() -> Album {
        Album {
            id: AlbumId::derive(&library(), "i am... sasha fierce\u{1f}beyonce"),
            library: library(),
            name: String::from("I Am... Sasha Fierce"),
            sort_name: String::from("i am sasha fierce"),
            album_artist: String::from("Beyoncé"),
            artist_id: Some(beyonce()),
            year: Some(2008),
            release_date: Some(String::from("2008-11-12")),
            original_date: Some(String::from("2008")),
            genres: vec![String::from("R&B")],
            is_compilation: false,
            release_type: Some(String::from("album")),
            song_count: 11,
            disc_count: 1,
            duration_ms: 2_486_000,
            size_bytes: 300_000_000,
            artwork: Some(ArtworkId::of_bytes(b"cover")),
            album_key: Some(String::from("i am... sasha fierce\u{1f}beyonce")),
            mbz: AlbumMbz {
                release: Some(String::from("9c8d")),
                release_group: None,
            },
            added_at: Millis(1_772_474_651_512),
            updated_at: Millis(1_772_474_651_512),
        }
    }

    pub fn song() -> Song {
        let album = album();
        Song {
            id: SongId::derive(&AudioDigest([1; 32])),
            library: library(),
            title: String::from("Halo"),
            artist: String::from("Beyoncé"),
            artist_id: Some(beyonce()),
            credits: vec![
                Credit {
                    artist: beyonce(),
                    name: String::from("Beyoncé"),
                    role: CreditRole::Main,
                },
                Credit {
                    artist: beyonce(),
                    name: String::from("Beyoncé"),
                    role: CreditRole::AlbumArtist,
                },
            ],
            album: Some(album.name.clone()),
            album_id: Some(album.id.clone()),
            album_artist: Some(String::from("Beyoncé")),
            track_number: Some(3),
            disc_number: Some(1),
            disc_subtitle: None,
            year: Some(2008),
            release_date: Some(String::from("2008-11-12")),
            genres: vec![String::from("R&B"), String::from("Pop")],
            duration_ms: 261_640,
            format: AudioFormat {
                container: Container::Flac,
                codec: Codec::Flac,
                content_type: "audio/flac",
                lossless: true,
                bitrate_kbps: Some(1012),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                size: 33_102_456,
            },
            artwork: album.artwork.clone(),
            song_key: String::from("halo\u{1f}beyonce"),
            replay_gain: Some(ReplayGain {
                track_gain: Some(-8.21),
                track_peak: Some(0.988),
                album_gain: Some(-8.9),
                album_peak: Some(1.0),
            }),
            mbz: SongMbz {
                recording: Some(String::from("e6a7")),
                track: None,
                release: None,
            },
            isrc: Some(String::from("USSM10804555")),
            bpm: None,
            comment: None,
            explicit: Some(Explicit::Clean),
            has_lyrics: true,
            folder: FolderId::derive(&library(), b"Beyonce/I Am"),
            added_at: Millis(1_772_474_651_512),
            updated_at: Millis(1_772_474_651_512),
            missing: false,
            content_version: 1,
        }
    }

    pub fn artist() -> Artist {
        Artist {
            id: beyonce(),
            name: String::from("Beyoncé"),
            sort_name: String::from("beyonce"),
            identity: String::from("beyonce"),
            album_count: 7,
            song_count: 112,
            artwork: None,
            mbz_artist: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures;
    use super::*;
    use crate::response::{Body, Format, render};

    fn starred_notes() -> Notes {
        let mut notes = Notes::new();
        notes.insert(
            ItemRef::Song(fixtures::song().id),
            UserAnnotations {
                starred_at: Some(Millis(1_777_629_600_000)),
                rating: Some(4),
                play_count: 17,
                last_played_at: Some(Millis(1_794_604_449_000)),
            },
        );
        notes
    }

    #[test]
    fn song_json_matches_golden() {
        let body = Body::one("song", song(&fixtures::song(), &starred_notes()));
        insta::assert_snapshot!("song_json", render(&body, Format::Json));
    }

    #[test]
    fn song_xml_matches_golden() {
        let body = Body::one("song", song(&fixtures::song(), &starred_notes()));
        insta::assert_snapshot!("song_xml", render(&body, Format::Xml));
    }

    #[test]
    fn album_and_artist_match_golden() {
        let notes = Notes::new();
        let album = album_id3(&fixtures::album(), &notes)
            .many("song", vec![song(&fixtures::song(), &notes)]);
        insta::assert_snapshot!(
            "album_json",
            render(&Body::one("album", album.clone()), Format::Json)
        );
        insta::assert_snapshot!("album_xml", render(&Body::one("album", album), Format::Xml));
        let artist = artist(&fixtures::artist(), &notes);
        insta::assert_snapshot!(
            "artist_json",
            render(&Body::one("artist", artist), Format::Json)
        );
    }

    /// The fields `SubsonicClient.swift` decodes, with the JSON types it decodes them as.
    #[test]
    fn song_has_every_field_tracks_decodes_with_its_type() -> Result<(), serde_json::Error> {
        let body = Body::one("song", song(&fixtures::song(), &Notes::new()));
        let json: serde_json::Value = serde_json::from_str(&render(&body, Format::Json))?;
        let s = &json["subsonic-response"]["song"];
        for key in [
            "id",
            "title",
            "artist",
            "album",
            "albumId",
            "artistId",
            "genre",
            "suffix",
            "contentType",
            "coverArt",
            "displayAlbumArtist",
            "created",
        ] {
            assert!(s[key].is_string(), "{key} should be a string");
        }
        for key in [
            "track",
            "discNumber",
            "year",
            "duration",
            "bitRate",
            "size",
            "samplingRate",
            "bitDepth",
        ] {
            assert!(s[key].is_i64(), "{key} should be an integer");
        }
        assert!(s.get("isExternal").is_none());
        assert_eq!(s["duration"], 262);
        let created = s["created"].as_str().unwrap_or_default();
        assert!(Millis::parse_rfc3339(created).is_some());
        assert_eq!(
            created.get(19..),
            Some(".512Z"),
            "{created} has milliseconds and Z"
        );
        Ok(())
    }

    #[test]
    fn index_letters_group_and_put_symbols_last() {
        let mut a = fixtures::artist();
        let mut b = fixtures::artist();
        b.name = String::from("!!!");
        b.sort_name = String::from("!!!");
        let mut c = fixtures::artist();
        c.name = String::from("Ásgeir");
        c.sort_name = String::from("ásgeir");
        a.sort_name = String::from("beyonce");
        let nodes = indexes(&[b, c, a], &Notes::new());
        let names: Vec<_> = nodes
            .iter()
            .filter_map(|n| n.get("name").cloned())
            .collect();
        assert_eq!(
            names,
            vec![Value::from("A"), Value::from("B"), Value::from("#")]
        );
    }

    #[test]
    fn dates_split_into_parts() {
        assert_eq!(
            item_date("2008-11-12"),
            Some(
                Node::new()
                    .attr("year", 2008)
                    .attr("month", 11u32)
                    .attr("day", 12u32)
            )
        );
        assert_eq!(item_date("2008"), Some(Node::new().attr("year", 2008)));
        assert_eq!(item_date("unknown"), None);
    }

    #[test]
    fn virtual_paths_never_contain_extra_separators() {
        let mut s = fixtures::song();
        s.title = String::from("AC/DC");
        assert_eq!(
            virtual_path(&s),
            "Beyoncé/I Am... Sasha Fierce/03 - AC_DC.flac"
        );
    }
}
