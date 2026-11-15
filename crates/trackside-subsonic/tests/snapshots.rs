//! Golden XML and JSON for every implemented endpoint against the seeded library. Random ids
//! (libraries, users, and the album and folder ids derived from a library's id) are replaced
//! with numbered placeholders in order of appearance.
//!
//! After an intended change, review with
//! `cargo insta review`.

use std::fmt::Write as _;

mod support;

use support::{Fixture, TestResult, pid};

const ALPHABET: &str = "0123456789abcdefghjkmnpqrstvwxyz";

/// Replaces ids of the kinds whose values change between runs.
fn redact(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut seen: Vec<String> = Vec::new();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let boundary_before = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        let candidate: String = chars.iter().skip(i).take(21).collect();
        let boundary_after = chars.get(i + 21).is_none_or(|c| !c.is_ascii_alphanumeric());
        let is_id = boundary_before
            && boundary_after
            && candidate.len() == 21
            && candidate.starts_with(['a', 'l', 'f', 'u', 'p', 'k', 'd'])
            && candidate[1..].chars().all(|c| ALPHABET.contains(c));
        if is_id {
            let n = match seen.iter().position(|s| *s == candidate) {
                Some(n) => n,
                None => {
                    seen.push(candidate.clone());
                    seen.len() - 1
                }
            };
            let _ = write!(out, "<{}{}>", &candidate[..1], n + 1);
            i += 21;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// An endpoint call: snapshot name, endpoint, parameters.
type Call<'a> = (&'a str, &'a str, Vec<(&'a str, &'a str)>);

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "one row per endpoint")]
async fn every_endpoint_matches_its_snapshot() -> TestResult {
    let fx = Fixture::start().await?;
    let library = fx.library_a.to_string();
    let album = fx
        .halo
        .album
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let artist = fx
        .halo
        .artist
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let song = pid(&fx.halo.id).to_owned();
    fx.ok(&fx.key_url(&fx.member_key, "star", &[("id", &song)]))
        .await?;
    let calls: Vec<Call<'_>> = vec![
        ("ping", "ping", vec![]),
        ("getLicense", "getLicense", vec![]),
        ("tokenInfo", "tokenInfo", vec![]),
        ("getMusicFolders", "getMusicFolders", vec![]),
        ("getIndexes", "getIndexes", vec![]),
        ("getArtists", "getArtists", vec![]),
        (
            "getMusicDirectory_library",
            "getMusicDirectory",
            vec![("id", &library)],
        ),
        (
            "getMusicDirectory_artist",
            "getMusicDirectory",
            vec![("id", &artist)],
        ),
        (
            "getMusicDirectory_album",
            "getMusicDirectory",
            vec![("id", &album)],
        ),
        ("getGenres", "getGenres", vec![]),
        ("getArtist", "getArtist", vec![("id", &artist)]),
        ("getAlbum", "getAlbum", vec![("id", &album)]),
        ("getSong", "getSong", vec![("id", &song)]),
        ("getArtistInfo", "getArtistInfo", vec![("id", &artist)]),
        ("getArtistInfo2", "getArtistInfo2", vec![("id", &artist)]),
        ("getAlbumInfo", "getAlbumInfo", vec![("id", &album)]),
        ("getAlbumInfo2", "getAlbumInfo2", vec![("id", &album)]),
        ("getSimilarSongs", "getSimilarSongs", vec![("id", &song)]),
        ("getSimilarSongs2", "getSimilarSongs2", vec![("id", &song)]),
        ("getTopSongs", "getTopSongs", vec![("artist", "Beyoncé")]),
        (
            "getAlbumList",
            "getAlbumList",
            vec![("type", "alphabeticalByName")],
        ),
        (
            "getAlbumList2",
            "getAlbumList2",
            vec![("type", "alphabeticalByName")],
        ),
        ("getSongsByGenre", "getSongsByGenre", vec![("genre", "R&B")]),
        ("getNowPlaying", "getNowPlaying", vec![]),
        ("getStarred", "getStarred", vec![]),
        ("getStarred2", "getStarred2", vec![]),
        ("search", "search", vec![("any", "Halo")]),
        ("search2", "search2", vec![("query", "Halo")]),
        ("search3", "search3", vec![("query", "Halo")]),
        ("search3_everything", "search3", vec![("query", "")]),
        ("getPlaylists", "getPlaylists", vec![]),
        ("getPlayQueue", "getPlayQueue", vec![]),
        ("getBookmarks", "getBookmarks", vec![]),
        (
            "getLyrics",
            "getLyrics",
            vec![("artist", "Beyoncé"), ("title", "Halo")],
        ),
        (
            "getLyricsBySongId",
            "getLyricsBySongId",
            vec![("id", &song)],
        ),
        ("getScanStatus", "getScanStatus", vec![]),
        (
            "getUser",
            "getUser",
            vec![("username", "luke+music@example.com")],
        ),
        ("getUsers", "getUsers", vec![]),
        ("getShares", "getShares", vec![]),
        ("getPodcasts", "getPodcasts", vec![]),
        (
            "getInternetRadioStations",
            "getInternetRadioStations",
            vec![],
        ),
        ("getCoverArt_missing", "getCoverArt", vec![("id", &song)]),
        (
            "getAnimatedArt",
            "getAnimatedArt",
            vec![("artist", "Beyoncé"), ("album", "Lungs")],
        ),
    ];
    for (name, endpoint, params) in &calls {
        let json = fx
            .get(&fx.key_url(&fx.member_key, endpoint, params))
            .await?;
        insta::assert_snapshot!(format!("{name}.json"), redact(&json.text()));
        let mut xml_params: Vec<(&str, &str)> =
            vec![("apiKey", &fx.member_key), ("v", "1.16.1"), ("c", "DSub")];
        xml_params.extend(params.iter().copied());
        let xml = fx
            .get(&format!(
                "/rest/{endpoint}.view?{}",
                support::query(&xml_params)
            ))
            .await?;
        insta::assert_snapshot!(format!("{name}.xml"), redact(&xml.text()));
    }
    Ok(())
}

#[test]
fn redaction_numbers_ids_in_order() {
    let a = "as06d81szxxem2cgn2cyw";
    let l = "l0a8f2c9e7d1b3k5m6n4q";
    let text =
        format!(r#"{{"id":"{a}","parent":"{l}","again":"{a}","song":"sdjcz08vf67h7cx28snq6"}}"#);
    assert_eq!(
        redact(&text),
        r#"{"id":"<a1>","parent":"<l2>","again":"<a1>","song":"sdjcz08vf67h7cx28snq6"}"#
    );
}
