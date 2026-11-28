//! Decoding Music Assistant's JSON.
//!
//! The samples in `tests/data/` follow the field lists and `__post_serialize__` hooks of
//! `music-assistant-models` 1.1.205 (the version 2.10.13 pins) and 1.1.254 (2.11 beta):
//! `api.py` (`ServerInfoMessage`), `player.py` (`Player`, `PlayerMedia`, `DeviceInfo`),
//! `player_queue.py`, `queue_item.py`, `media_items/media_item.py`, `media_items/metadata.py`,
//! `media_items/provider_mapping.py`, `media_items/__init__.py` (`SearchResults`),
//! `config_entries.py` (`ProviderConfig`) and `setup_flow.py` (`SetupFlowStep`). The
//! `provider-instance.json` payload is copied from `tests/test_provider_instance.py`
//! (`BASE_PAYLOAD` with the `supported_artist_types` case), and the `protocols` entry of
//! `player-2.11.json` from `tests/test_player.py`
//! (`test_player_protocol_info_defaults_and_unknown_role`). The recommendation samples follow
//! `RecommendationFolder` and `_deserialize_recommendation_items` of the same files, with the
//! Apple Music provider's station rows of 2.10.14 (stations as dynamic `ra.` playlists) and
//! of 2.11 (stations as radios) and the library provider's rows.

use serde::de::DeserializeOwned;
use trackside_mass::models::{
    FlowStepType, ImageType, MediaItem, MediaType, PlaybackState, Player, PlayerFeature,
    PlayerQueue, PlayerType, ProviderConfig, ProviderInstance, ProviderType, RecommendationFolder,
    RecommendationFolderType, RecommendationItem, RepeatMode, SearchResults, ServerInfo,
    SetupFlowStep,
};
use trackside_mass::{Endpoint, ImageSize, image_url, schema};

fn load<T: DeserializeOwned>(name: &str) -> T {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).expect("sample exists");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn server_info_from_2_10_is_supported() {
    let info: ServerInfo = load("server-info-2.10.json");
    assert_eq!(info.schema_version, 65);
    assert_eq!(schema::check(&info), Ok(schema::Compatibility::Tested));
}

#[test]
fn server_info_from_2_11_is_supported() {
    let info: ServerInfo = load("server-info-2.11.json");
    assert_eq!(info.server_version, "2.11.0b11");
    assert!(info.has_remote_access);
    assert_eq!(schema::check(&info), Ok(schema::Compatibility::Tested));
}

#[test]
fn a_2_10_player_decodes_every_field_trackside_uses() {
    let player: Player = load("player-2.10.json");
    assert_eq!(player.player_id, "ap8c1f2e3d4b5a");
    assert_eq!(player.provider, "airplay--Hx7Kq2Lm");
    assert_eq!(player.player_type, PlayerType::Player);
    assert_eq!(player.name, "Kitchen");
    assert!(player.available);
    assert_eq!(player.powered, Some(true));
    assert_eq!(player.state(), PlaybackState::Playing);
    assert_eq!(player.volume_level, Some(35));
    assert_eq!(player.volume_muted, Some(false));
    assert_eq!(player.group_members, ["ap8c1f2e3d4b5a", "ap99aa88bb77cc"]);
    assert_eq!(player.synced_to, None);
    assert!(player.supports(&PlayerFeature::SetMembers));
    assert!(player.supports(&PlayerFeature::Other("gapless_playback".into())));
    assert_eq!(player.elapsed_time, Some(73.4));
    assert_eq!(player.elapsed_time_last_updated, Some(1_764_280_000.25));
    let media = player.current_media.as_ref().expect("current media");
    assert_eq!(media.uri, "library://track/412");
    assert_eq!(media.media_type, MediaType::Track);
    assert_eq!(media.title.as_deref(), Some("Digital Love"));
    assert_eq!(
        player.device_info.as_ref().and_then(|d| d.model.as_deref()),
        Some("HomePod mini")
    );
}

#[test]
fn a_playing_players_position_advances() {
    let player: Player = load("player-2.10.json");
    let position = player.position_at(1_764_280_010.25).expect("position");
    assert!((position - 83.4).abs() < 1e-6, "{position}");
}

#[test]
fn a_2_11_player_with_new_types_and_fields_decodes() {
    let player: Player = load("player-2.11.json");
    assert_eq!(
        player.player_type,
        PlayerType::Other("multi_channel".into())
    );
    assert_eq!(player.state(), PlaybackState::Paused);
    assert_eq!(player.volume_level, Some(18));
    assert_eq!(player.powered, None);
    assert!(player.supports(&PlayerFeature::Other("night_mode_toggle".into())));
}

#[test]
fn the_legacy_state_field_is_used_when_playback_state_is_missing() {
    let mut value: serde_json::Value = load("player-2.10.json");
    value
        .as_object_mut()
        .expect("object")
        .remove("playback_state");
    value["state"] = "paused".into();
    let player: Player = serde_json::from_value(value).expect("decodes");
    assert_eq!(player.state(), PlaybackState::Paused);
}

#[test]
fn a_player_with_odd_values_still_decodes() {
    let mut value: serde_json::Value = load("player-2.10.json");
    value["current_media"] = "unexpected".into();
    value["volume_level"] = serde_json::json!({"nested": 1});
    value["group_members"] = serde_json::json!(["a", 7, null]);
    value["name"] = serde_json::Value::Null;
    let player: Player = serde_json::from_value(value).expect("decodes");
    assert_eq!(player.current_media, None);
    assert_eq!(player.volume_level, None);
    assert_eq!(player.group_members, ["a"]);
    assert_eq!(player.name, "");
}

#[test]
fn a_queue_decodes_with_its_current_item() {
    let queue: PlayerQueue = load("queue-2.10.json");
    assert_eq!(queue.queue_id, "ap8c1f2e3d4b5a");
    assert!(queue.active);
    assert!(queue.shuffle_enabled);
    assert_eq!(queue.repeat_mode, RepeatMode::All);
    assert_eq!(queue.current_index, Some(3));
    assert_eq!(queue.items, 14);
    assert!((queue.elapsed_time - 73.4).abs() < 1e-9);
    assert!(queue.autoplay_enabled);
    let item = queue.current_item.as_ref().expect("current item");
    assert_eq!(item.uri(), "library://track/412");
    let media = item.media_item.as_ref().expect("media item");
    assert_eq!(media.artist_names(), "Daft Punk");
    assert_eq!(
        media.album.as_ref().map(|a| a.name.as_str()),
        Some("Discovery")
    );
}

#[test]
fn a_track_decodes_with_mappings_images_and_external_ids() {
    let track: MediaItem = load("track.json");
    assert_eq!(track.media_type, MediaType::Track);
    assert_eq!(track.uri.as_deref(), Some("library://track/412"));
    assert_eq!(track.duration, Some(301.0));
    assert_eq!(track.track_number, Some(4));
    assert_eq!(track.external_ids.len(), 2, "the malformed pair is dropped");
    assert_eq!(track.external_ids[0].kind, "isrc");
    assert_eq!(track.provider_mappings.len(), 2);
    assert!(track.is_available());
    let spotify = &track.provider_mappings[1];
    assert_eq!(spotify.provider_domain, "spotify");
    assert_eq!(
        spotify.audio_format.as_ref().and_then(|f| f.bit_rate),
        Some(320)
    );
    let thumb = track.thumb().expect("thumb");
    assert_eq!(thumb.kind, ImageType::Thumb);
    assert!(!track.is_mapping());
    assert!(track.artists[0].is_mapping());
    assert_eq!(track.album.as_ref().and_then(|a| a.year), Some(2001));
}

#[test]
fn artwork_urls_go_through_the_image_proxy() {
    let track: MediaItem = load("track.json");
    let endpoint = Endpoint::parse("http://192.168.1.20:8095").expect("endpoint");
    let url = image_url(
        &endpoint,
        track.thumb().expect("thumb"),
        ImageSize::Px512,
        None,
    );
    assert_eq!(
        url.as_deref(),
        Some(
            "http://192.168.1.20:8095/imageproxy/3f8a2c9b1d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8?size=512"
        )
    );
}

#[test]
fn search_results_drop_entries_that_are_not_items() {
    let results: SearchResults = load("search-results.json");
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].provider, "spotify--Pq3Rs");
    assert_eq!(results.artists[0].name, "Daft Punk");
    assert_eq!(results.radio[0].duration, None);
}

#[test]
fn a_provider_instance_decodes() {
    let instance: ProviderInstance = load("provider-instance.json");
    assert_eq!(instance.provider_type, ProviderType::Music);
    assert_eq!(instance.instance_id, "audiobookshelf_1");
    assert!(instance.available);
}

#[test]
fn a_provider_config_decodes_and_reads_values() {
    let config: ProviderConfig = load("provider-config.json");
    assert_eq!(config.instance_id, "opensubsonic--T5ks");
    assert_eq!(config.display_name(), "Trackside");
    assert_eq!(
        config.value("recommendation_count"),
        Some(&serde_json::json!(20))
    );
    assert_eq!(config.value("log_level"), None);
    assert_eq!(
        config.last_error, None,
        "a legacy string error is tolerated"
    );
}

#[test]
fn a_setup_flow_form_with_errors_decodes() {
    let step: SetupFlowStep = load("setup-flow-form.json");
    assert_eq!(step.step_type, FlowStepType::Form);
    assert_eq!(step.entries.len(), 2);
    assert_eq!(
        step.errors.get("base").map(String::as_str),
        Some("Could not connect to http://192.168.1.10")
    );
    assert!(step.result.is_empty());
}

/// A list as the client decodes one: entry by entry, dropping those that do not decode.
fn load_list<T: DeserializeOwned>(name: &str) -> Vec<T> {
    let entries: Vec<serde_json::Value> = load(name);
    entries
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect()
}

fn media(item: &RecommendationItem) -> Option<&MediaItem> {
    match item {
        RecommendationItem::Media(media) => Some(media),
        _ => None,
    }
}

#[test]
fn recommendation_rows_from_2_10_decode_and_bad_rows_are_dropped() {
    let rows: Vec<RecommendationFolder> = load_list("recommendations-2.10.json");
    let ids: Vec<&str> = rows.iter().map(|r| r.item_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "recently_played",
            "made-for-you",
            "random_artists",
            "new-releases"
        ]
    );
    let apple = &rows[1];
    assert_eq!(apple.provider, "apple_music--Hq7cT2");
    assert_eq!(apple.name, "Made for You");
    assert!(apple.items.is_empty(), "2.10.14 lists rows without items");
    assert!(apple.enabled_by_default);
    assert!(!apple.supports_hide_played, "2.10 does not send it");
    assert_eq!(rows[0].icon.as_deref(), Some("mdi-motion-play"));
    assert!(!rows[2].enabled_by_default);
    let deezer = &rows[3];
    assert_eq!(deezer.subtitle.as_deref(), Some("Fresh this week"));
    assert_eq!(
        deezer.kind,
        RecommendationFolderType::Other(String::from("some_future_type"))
    );
    assert!(deezer.items.is_empty(), "null items are none");
    assert!(deezer.enabled_by_default, "absent means on");
}

#[test]
fn recommendation_rows_from_2_11_decode_with_inline_items() {
    let rows: Vec<RecommendationFolder> = load_list("recommendations-2.11.json");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, RecommendationFolderType::Timeline);
    assert!(rows[0].supports_hide_played && rows[0].supports_provider_filter);
    assert_eq!(rows[1].kind, RecommendationFolderType::Default);
    let spotify = &rows[2];
    assert!(spotify.enabled_by_default, "null means on");
    assert_eq!(spotify.items.len(), 1);
    let mix = media(&spotify.items[0]).expect("a media item");
    assert_eq!(mix.media_type, MediaType::Playlist);
    assert_eq!(mix.owner.as_deref(), Some("Spotify"));
}

#[test]
fn apple_music_stations_of_2_10_are_dynamic_playlists() {
    let items: Vec<RecommendationItem> = load_list("recommendation-items-2.10.json");
    assert_eq!(items.len(), 3);
    let station = media(&items[0]).expect("a media item");
    assert_eq!(station.media_type, MediaType::Playlist);
    assert!(station.is_dynamic);
    assert!(station.item_id.starts_with("ra."));
    assert_eq!(
        station.provider_mappings[0].provider_instance,
        "apple_music--Hq7cT2"
    );
    assert!(station.thumb().is_some_and(|i| i.remotely_accessible));
    let mapping = media(&items[2]).expect("an item mapping");
    assert!(mapping.is_mapping());
    assert_eq!(
        mapping.provider, "apple_music",
        "a mapping may name the domain"
    );
    assert!(mapping.thumb().is_some_and(|i| i.proxy_id.is_some()));
}

#[test]
fn apple_music_stations_of_2_11_are_radios_among_mixed_items() {
    let items: Vec<RecommendationItem> = load_list("recommendation-items-2.11.json");
    assert_eq!(items.len(), 6, "entries that are not objects are dropped");
    let station = media(&items[0]).expect("a media item");
    assert_eq!(station.media_type, MediaType::Radio);
    assert!(station.is_dynamic);
    assert_eq!(station.duration, Some(172_800.0));
    let album = media(&items[1]).expect("a media item");
    assert_eq!(album.media_type, MediaType::Album);
    assert_eq!(album.artist_names(), "Beyoncé");
    let track = media(&items[2]).expect("an item mapping");
    assert!(track.is_mapping() && track.media_type == MediaType::Track);
    assert!(
        matches!(&items[3], RecommendationItem::Folder(f) if f.path == "apple_music--Hq7cT2://genres")
    );
    assert_eq!(
        media(&items[4]).map(|m| m.media_type.clone()),
        Some(MediaType::Podcast)
    );
    assert_eq!(
        media(&items[5]).map(|m| m.media_type.clone()),
        Some(MediaType::Other(String::from("audio_source"))),
        "an unknown kind is kept for the caller to skip"
    );
}
