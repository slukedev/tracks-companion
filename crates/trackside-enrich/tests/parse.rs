//! The client's readers against answers in Last.fm's format. No network.

use trackside_enrich::lastfm::{self, LastFmError};

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json(name: &str) -> serde_json::Map<String, serde_json::Value> {
    lastfm::object(200, &fixture(name)).expect("a Last.fm object")
}

#[test]
fn similar_artists_keep_their_match_and_skip_blank_names() {
    let similar = lastfm::similar_artists(&json("artist.getSimilar.json"));
    let names: Vec<(&str, f64)> = similar
        .iter()
        .map(|r| (r.artist.as_str(), r.score))
        .collect();
    assert_eq!(
        names,
        [
            ("Thom Yorke", 1.0),
            ("Atoms for Peace", 0.862_114),
            ("Portishead", 0.523)
        ]
    );
}

#[test]
fn a_list_of_one_arrives_as_an_object() {
    let similar = lastfm::similar_artists(&json("artist.getSimilar-one.json"));
    assert_eq!(similar.len(), 1);
    assert_eq!(similar[0].artist, "Björk");
}

#[test]
fn similar_tracks_are_best_first_with_their_artist() {
    let similar = lastfm::similar_tracks(&json("track.getSimilar.json"));
    let got: Vec<(&str, &str)> = similar
        .iter()
        .map(|r| (r.title.as_str(), r.artist.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("Weird Fishes/Arpeggi", "Radiohead"),
            ("Roads", "Portishead"),
            ("Teardrop", "Massive Attack")
        ]
    );
}

#[test]
fn top_tracks_follow_rank_once_each() {
    assert_eq!(
        lastfm::top_tracks(&json("artist.getTopTracks.json")),
        ["Creep", "Karma Police", "No Surprises"]
    );
}

#[test]
fn artist_info_gives_a_plain_short_bio() {
    assert_eq!(
        lastfm::artist_bio(&json("artist.getInfo.json")).as_deref(),
        Some(
            "Radiohead are an English rock band formed in Abingdon, Oxfordshire, in 1985. \
             The band consists of Thom Yorke & Jonny Greenwood."
        )
    );
}

#[test]
fn errors_are_read_from_the_body_whatever_the_status() {
    let not_found = lastfm::object(400, &fixture("error-not-found.json")).unwrap_err();
    assert!(not_found.is_unknown());
    let limited = lastfm::object(200, &fixture("error-rate-limit.json")).unwrap_err();
    assert!(matches!(limited, LastFmError::Service { code: 29, .. }) && limited.is_retryable());
    let key = lastfm::object(403, &fixture("error-invalid-key.json")).unwrap_err();
    assert!(key.is_key_problem());
}

#[test]
fn an_empty_answer_has_nothing_in_it() {
    let empty = serde_json::Map::new();
    assert_eq!(lastfm::similar_artists(&empty).len(), 0);
    assert_eq!(lastfm::similar_tracks(&empty).len(), 0);
    assert_eq!(lastfm::top_tracks(&empty).len(), 0);
    assert_eq!(lastfm::artist_bio(&empty), None);
}

fn json_with_images(name: &str) -> serde_json::Map<String, serde_json::Value> {
    let text = String::from_utf8(fixture(name))
        .expect("text")
        .replace("IMAGE_BASE", "https://lastfm.freetls.fastly.net");
    lastfm::object(200, text.as_bytes()).expect("a Last.fm object")
}

#[test]
fn a_tracks_cover_is_its_albums_largest_image() {
    let cover = lastfm::track_cover(&json_with_images("track.getInfo.json"));
    assert_eq!(
        cover.image.as_deref(),
        Some("https://lastfm.freetls.fastly.net/i/u/300x300/4128a6eb29f94943c9d206c08e625904.png")
    );
    assert_eq!(cover.album.as_deref(), Some("Dummy"));
    assert_eq!(cover.album_artist.as_deref(), Some("Portishead"));
}

#[test]
fn the_placeholder_star_and_blank_urls_are_no_image() {
    let cover = lastfm::track_cover(&json_with_images("track.getInfo-placeholder.json"));
    assert_eq!(cover.image, None);
    assert_eq!(cover.album.as_deref(), Some("Blur"));
    assert_eq!(
        lastfm::album_cover(&json_with_images("album.getInfo-placeholder.json")),
        None
    );
    assert_eq!(lastfm::usable_image("  "), None);
    assert_eq!(
        lastfm::usable_image(
            "https://lastfm.freetls.fastly.net/i/u/34s/2A96CBD8B46E442FC41C2B86B821562F.png"
        ),
        None
    );
    assert_eq!(
        lastfm::track_cover(&serde_json::Map::new()),
        lastfm::TrackCover::default()
    );
}
