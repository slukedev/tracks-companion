//! The streaming catalog's shapes (api.md section 21, contract 6.1), built with the handlers'
//! own builders from core records. All are top-level, so the apps copy them.

use std::collections::HashMap;

use serde_json::Value;
use trackside_api::{ApiError, build, wire};
use trackside_core::assistant::SpeakersState;
use trackside_core::catalog::{
    AccountState, ArtistCredit, CatalogAlbum, CatalogArtist, CatalogDetail, CatalogEntry,
    CatalogError, CatalogKind, CatalogKinds, CatalogPlaylist, CatalogRadio, CatalogResults,
    CatalogService, CatalogTrack, LibraryPage, MatchResult, MatchState, Matches, Quality,
    Recommendations, SavedState, ServiceCapability, ServiceList, ServiceMatch, ServiceOutcome,
    ServiceSearchState, Shelf, ShelfKind,
};
use trackside_core::{PublicId, ServiceId, ShelfId, SongId};

use super::at;

type Out = Vec<(&'static str, Value)>;

fn pid<T: PublicId>(text: &str) -> T {
    T::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

const SPOTIFY: &str = "bqtyy4kcexcsg6wa0y4w5";
const TIDAL: &str = "bnr6z9ds6zzv5xjpv0j1s";
const QOBUZ: &str = "br5wm04d54mx0cw00fwzr";
const APPLE_MUSIC: &str = "bm1zt0k6dk6mzz0n3vw5d";
const HALO_SONG: &str = "sdjcz08vf67h7cx28snq6";

const ALBUM_REF: &str = "mc1.YWxidW0fc3BvdGlmeR8zOVA3VkQ3cWxnM1owbHRxNjBlSHA3";
const ARTIST_REF: &str = "mc1.YXJ0aXN0H3Nwb3RpZnkfNnZXRE85NjlQdk5xTllISU9XNXYwbQ";
const PLAYLIST_REF: &str = "mc1.cGxheWxpc3Qfc3BvdGlmeR8zN2k5ZFFaRjFEWDRzV1Nwd3EzTGlP";
const ALBUM_ART: &str = "myweqhq0j9dpzb5wt5pmr";

fn json<T: serde::Serialize>(value: &T) -> Result<Value, serde_json::Error> {
    serde_json::to_value(value)
}

fn service(
    id: &str,
    name: &str,
    capabilities: &[ServiceCapability],
    quality: Quality,
) -> CatalogService {
    CatalogService {
        id: pid(id),
        name: name.to_owned(),
        logo_key: name.to_ascii_lowercase(),
        capabilities: capabilities.to_vec(),
        account: AccountState::Ok,
        account_detail: None,
        quality: Some(quality),
        library_edit_kinds: if capabilities.contains(&ServiceCapability::LibraryEdit) {
            vec![
                CatalogKind::Album,
                CatalogKind::Artist,
                CatalogKind::Playlist,
                CatalogKind::Track,
            ]
        } else {
            Vec::new()
        },
    }
}

fn beyonce() -> Vec<ArtistCredit> {
    vec![ArtistCredit {
        name: String::from("Beyoncé"),
        reference: Some(String::from(ARTIST_REF)),
    }]
}

fn track(title: &str, reference: &str, duration_ms: u64) -> CatalogTrack {
    CatalogTrack {
        reference: reference.to_owned(),
        service: pid(SPOTIFY),
        title: title.to_owned(),
        artist: Some(String::from("Beyoncé")),
        artists: beyonce(),
        album: Some(String::from("I Am... Sasha Fierce")),
        album_ref: Some(String::from(ALBUM_REF)),
        artwork: Some(pid(ALBUM_ART)),
        duration_ms: Some(duration_ms),
        explicit: None,
        isrc: None,
        track_number: None,
        disc_number: None,
        quality: None,
        song_key: Some(tracks_model::song_key(title, "Beyoncé")),
        playable: true,
        saved: None,
    }
}

fn halo() -> CatalogTrack {
    track(
        "Halo",
        "mc1.dHJhY2sfc3BvdGlmeR8wV3FkMHpBNXdOcFFuUUJHRVVIMGY0",
        261_640,
    )
}

fn single_ladies() -> CatalogTrack {
    track(
        "Single Ladies (Put a Ring on It)",
        "mc1.dHJhY2sfc3BvdGlmeR83aFFKQTUwWHJDV0FCQXU1djZRWjRp",
        193_213,
    )
}

fn album() -> CatalogAlbum {
    CatalogAlbum {
        reference: String::from(ALBUM_REF),
        service: pid(SPOTIFY),
        name: String::from("I Am... Sasha Fierce"),
        artist: Some(String::from("Beyoncé")),
        artists: Vec::new(),
        year: Some(2008),
        artwork: Some(pid(ALBUM_ART)),
        track_count: Some(11),
        album_key: tracks_model::album_key("I Am... Sasha Fierce", "beyonce"),
        saved: None,
    }
}

fn artist() -> CatalogArtist {
    CatalogArtist {
        reference: String::from(ARTIST_REF),
        service: pid(SPOTIFY),
        name: String::from("Beyoncé"),
        identity: Some(String::from("beyonce")),
        artwork: Some(pid("m70csqp0tb9g09j0fg8qt")),
        saved: None,
    }
}

fn playlist() -> CatalogPlaylist {
    CatalogPlaylist {
        reference: String::from(PLAYLIST_REF),
        service: pid(SPOTIFY),
        name: String::from("This Is Beyoncé"),
        owner: Some(String::from("Spotify")),
        track_count: Some(50),
        artwork: Some(pid("m78a6d72awx3fd6jkzaht")),
        editable: None,
        saved: None,
    }
}

fn outcome(id: &str, state: ServiceSearchState) -> ServiceOutcome {
    ServiceOutcome {
        service: pid::<ServiceId>(id),
        state,
    }
}

/// The library songs the caller owns, by `song_key`.
fn owned() -> HashMap<String, SongId> {
    HashMap::from([(tracks_model::song_key("Halo", "Beyoncé"), pid(HALO_SONG))])
}

/// The catalog fixtures; `songs` is the library section `search-with-catalog.json` keeps
/// unchanged.
#[expect(clippy::too_many_lines, reason = "one fixture after another")]
pub fn fixtures(songs: wire::Section<wire::Song>) -> Result<Out, serde_json::Error> {
    let mut out: Out = Vec::new();
    let owned = owned();

    let mut qobuz = service(
        QOBUZ,
        "Qobuz",
        &[ServiceCapability::Library, ServiceCapability::Search],
        Quality::Lossless,
    );
    qobuz.account = AccountState::SignedOut;
    qobuz.account_detail = Some(String::from("The sign-in expired."));
    let services = ServiceList {
        services: vec![
            service(SPOTIFY, "Spotify", ServiceCapability::ALL, Quality::High),
            service(
                TIDAL,
                "Tidal",
                &[
                    ServiceCapability::Library,
                    ServiceCapability::Playlists,
                    ServiceCapability::Search,
                    ServiceCapability::TopTracks,
                ],
                Quality::Lossless,
            ),
            qobuz,
        ],
        state: SpeakersState::Ok,
        since: None,
    };
    out.push((
        "catalog-services.json",
        json(&build::catalog_services_body(&services))?,
    ));
    out.push((
        "catalog-services-event.json",
        json(&build::catalog_services_event_body(SpeakersState::Ok))?,
    ));

    let mut searched_halo = halo();
    searched_halo.explicit = Some(false);
    searched_halo.isrc = Some(String::from("USSM10804555"));
    searched_halo.track_number = Some(3);
    let search = CatalogResults {
        kinds: CatalogKinds::DEFAULT,
        tracks: vec![searched_halo],
        albums: vec![album()],
        artists: vec![artist()],
        playlists: vec![playlist()],
        radio: Vec::new(),
        services: vec![
            outcome(SPOTIFY, ServiceSearchState::Ok),
            outcome(TIDAL, ServiceSearchState::TimedOut),
        ],
        complete: false,
    };
    out.push((
        "catalog-search.json",
        json(&build::catalog_search_body(&search, &owned))?,
    ));

    let tidal_halo = CatalogTrack {
        reference: String::from("mc1.dHJhY2sfdGlkYWwfMTc4MTg4Nw"),
        service: pid(TIDAL),
        artists: vec![ArtistCredit {
            name: String::from("Beyoncé"),
            reference: None,
        }],
        album_ref: Some(String::from("mc1.YWxidW0fdGlkYWwfMTc4MTg4MA")),
        artwork: Some(pid("mkyrr28c5tyn1907njt60")),
        track_number: Some(3),
        quality: Some(Quality::Lossless),
        ..halo()
    };
    let with_catalog = CatalogResults {
        kinds: CatalogKinds::NONE.with(CatalogKind::Track),
        tracks: vec![tidal_halo],
        albums: Vec::new(),
        artists: Vec::new(),
        playlists: Vec::new(),
        radio: Vec::new(),
        services: vec![outcome(TIDAL, ServiceSearchState::Ok)],
        complete: true,
    };
    out.push((
        "search-with-catalog.json",
        json(&wire::library::SearchResults {
            songs: Some(songs),
            albums: None,
            artists: None,
            external: None,
            catalog: Some(build::catalog_search_body(&with_catalog, &owned)),
        })?,
    ));

    let on_album = |mut t: CatalogTrack, number: u32| {
        t.explicit = Some(false);
        t.disc_number = Some(1);
        t.track_number = Some(number);
        t
    };
    let mut full_album = album();
    full_album.artists = beyonce();
    full_album.track_count = Some(2);
    full_album.saved = Some(true);
    let mut saved_halo = on_album(halo(), 3);
    saved_halo.saved = Some(true);
    let mut unsaved_ladies = on_album(single_ladies(), 9);
    unsaved_ladies.saved = Some(false);
    let album_detail = CatalogDetail::Album {
        album: full_album,
        tracks: vec![saved_halo, unsaved_ladies],
    };
    out.push((
        "catalog-album.json",
        json(&build::catalog_detail_body(&album_detail, &owned))?,
    ));

    let artist_detail = CatalogDetail::Artist {
        artist: artist(),
        top_tracks: vec![single_ladies()],
        albums: vec![album()],
    };
    out.push((
        "catalog-artist.json",
        json(&build::catalog_detail_body(&artist_detail, &owned))?,
    ));

    let mut listed = playlist();
    listed.editable = Some(false);
    listed.saved = Some(true);
    let playlist_detail = CatalogDetail::Playlist {
        playlist: listed.clone(),
        tracks: vec![halo()],
        next_cursor: Some(String::from("eyJvIjoxMDB9")),
    };
    out.push((
        "catalog-playlist.json",
        json(&build::catalog_detail_body(&playlist_detail, &owned))?,
    ));

    let library = LibraryPage {
        items: vec![CatalogEntry::Playlist(listed)],
        next_cursor: Some(String::from("eyJvIjoxMDB9")),
        synced_at: Some(at(-1800)),
    };
    out.push((
        "catalog-library.json",
        json(&build::catalog_library_body(&library, &owned))?,
    ));

    let server = "8f2e4b6a1c3d5e7f9a0b2c4d6e8f0a1b";
    let station = CatalogRadio {
        reference: String::from(
            "mc1.cmFkaW8fYXBwbGVfbXVzaWMfcmEucS1HQUk2SURFMk1qZ3dOall3TlRFH2FwcGxlX211c2ljLS1IcTdjVDI",
        ),
        service: pid(APPLE_MUSIC),
        name: String::from("Beyoncé & Similar Artists Station"),
        artwork: Some(pid("mf3w0g2x8kq1v7r5nd9ce")),
    };
    let recommendations = Recommendations {
        shelves: vec![
            Shelf {
                id: ShelfId::derive(server, "apple_music--Hq7cT2", "stations-for-you"),
                service: pid(APPLE_MUSIC),
                title: String::from("Stations for You"),
                subtitle: None,
                kind: ShelfKind::Default,
                items: vec![CatalogEntry::Radio(station)],
            },
            Shelf {
                id: ShelfId::derive(server, "spotify--Xb4LkP2q", "recently-played"),
                service: pid(SPOTIFY),
                title: String::from("Jump back in"),
                subtitle: Some(String::from("Your recent rotation")),
                kind: ShelfKind::Timeline,
                items: vec![
                    CatalogEntry::Track(halo()),
                    CatalogEntry::Album(album()),
                    CatalogEntry::Playlist(playlist()),
                    CatalogEntry::Artist(artist()),
                ],
            },
        ],
        services: vec![
            outcome(APPLE_MUSIC, ServiceSearchState::Ok),
            outcome(SPOTIFY, ServiceSearchState::Ok),
            outcome(TIDAL, ServiceSearchState::TimedOut),
        ],
        complete: false,
    };
    out.push((
        "catalog-recommendations.json",
        json(&build::catalog_recommendations_body(
            &recommendations,
            &owned,
        ))?,
    ));

    out.push((
        "catalog-saved.json",
        json(&build::catalog_saved_body(&SavedState {
            reference: halo().reference,
            service: pid(SPOTIFY),
            saved: true,
        }))?,
    ));

    out.push((
        "catalog-match-request.json",
        json(&wire::catalog::MatchRequest {
            items: vec![
                wire::catalog::MatchItem {
                    album: Some(String::from("I Am... Sasha Fierce")),
                    artist: String::from("Beyoncé"),
                    duration_ms: Some(261_640),
                    id: String::from("sdjcz08vf67h7cx28snq6"),
                    isrc: Some(String::from("USSM10804555")),
                    title: String::from("Halo"),
                },
                wire::catalog::MatchItem {
                    album: None,
                    artist: String::from("Beyoncé"),
                    duration_ms: Some(193_213),
                    id: String::from("local-2"),
                    isrc: None,
                    title: String::from("Single Ladies (Put a Ring on It)"),
                },
            ],
            services: Some(vec![String::from(SPOTIFY), String::from(TIDAL)]),
        })?,
    ));
    let found = |service: &str, state: MatchState, track: Option<CatalogTrack>| ServiceMatch {
        service: pid(service),
        state,
        track,
    };
    let matches = Matches {
        results: vec![
            MatchResult {
                id: String::from("sdjcz08vf67h7cx28snq6"),
                matches: vec![
                    found(SPOTIFY, MatchState::Found, Some(halo())),
                    found(TIDAL, MatchState::TimedOut, None),
                ],
            },
            MatchResult {
                id: String::from("local-2"),
                matches: vec![
                    found(SPOTIFY, MatchState::Deferred, None),
                    found(TIDAL, MatchState::None, None),
                ],
            },
        ],
        complete: false,
        retry_after: Some(std::time::Duration::from_secs(12)),
    };
    out.push((
        "catalog-match.json",
        json(&build::catalog_matches_body(&matches, &owned))?,
    ));

    let mut unavailable = ApiError::from(CatalogError::Unavailable).envelope();
    unavailable.error.request_id = Some(String::from("4e6a8c0b2d4f6a8c0e2b4d6f8a0c2e4b"));
    out.push(("error-catalog-unavailable.json", json(&unavailable)?));
    Ok(out)
}
