//! The streaming catalog (api.md section 21, contract section 2): the services connected in
//! Music Assistant, search across them, item detail, each service's saved library and their
//! recommendations. The
//! routes answer only callers whose `/me` has `features.catalog` (`404` otherwise). Library
//! songs are never mixed in: a catalog track that matches a song the caller can see names
//! it in `song_id`, and the client folds the two.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use http::StatusCode;
use serde::Deserialize;
use trackside_auth::extract::Authed;
use trackside_auth::{Principal, Scopes};
use trackside_core::assistant::SpeakersState;
use trackside_core::catalog::{
    ArtistCredit, CatalogAlbum, CatalogArtist, CatalogControl, CatalogDetail, CatalogEntry,
    CatalogKind, CatalogKinds, CatalogPage, CatalogPlaylist, CatalogRadio, CatalogResults,
    CatalogSearch, CatalogTrack, LibraryPage, MAX_QUERY_LEN, MAX_SERVICES, PAGE_LIMIT,
    PAGE_LIMIT_MAX, RECOMMENDATIONS_LIMIT, RECOMMENDATIONS_LIMIT_MAX, Recommendations,
    RecommendationsQuery, SEARCH_LIMIT, SEARCH_LIMIT_MAX, ServiceList, ShelfKind,
};
use trackside_core::{Access, ServiceId, SongId};

use super::read;
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Query, body_id, json_with, path_id};
use crate::wire::Time;
use crate::wire::catalog as wire;

const LIBRARY: u8 = Scopes::LIBRARY.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/catalog/services", get(services))
        .route("/catalog/search", get(search))
        .route("/catalog/items/{reference}", get(item))
        .route("/catalog/services/{id}/library", get(library))
        .route("/catalog/recommendations", get(recommendations))
}

/// The catalog for a caller with `features.catalog`; the routes do not exist otherwise.
pub(crate) fn catalog_for<'s>(
    state: &'s ApiState,
    principal: &Principal,
) -> Option<&'s dyn CatalogControl> {
    super::account::catalog_feature(state, principal)?;
    state.assistant.catalog()
}

fn allowed<'s>(state: &'s ApiState, principal: &Principal) -> ApiResult<&'s dyn CatalogControl> {
    catalog_for(state, principal).ok_or_else(ApiError::not_found)
}

/// Owned songs by `song_key`, for every track that has one.
pub(crate) type Owned = HashMap<String, SongId>;

/// The songs the caller can see with the same `song_key` as any of `tracks`.
pub(crate) async fn owned<'t>(
    state: &ApiState,
    access: &Access,
    tracks: impl Iterator<Item = &'t CatalogTrack>,
) -> ApiResult<Owned> {
    let mut keys: Vec<String> = tracks.filter_map(|t| t.song_key.clone()).collect();
    keys.sort_unstable();
    keys.dedup();
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let access = access.clone();
    read(state, move |c| {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        trackside_core::repo::songs::ids_by_keys(c, &access, &keys)
    })
    .await
}

fn credits(credits: &[ArtistCredit]) -> Vec<wire::ArtistCredit> {
    credits
        .iter()
        .map(|c| wire::ArtistCredit {
            name: c.name.clone(),
            reference: c.reference.clone(),
        })
        .collect()
}

pub fn track_body(track: &CatalogTrack, owned: &Owned) -> wire::CatalogTrack {
    wire::CatalogTrack {
        album: track.album.clone(),
        album_ref: track.album_ref.clone(),
        artist: track.artist.clone(),
        artists: credits(&track.artists),
        artwork_id: track.artwork.as_ref().map(ToString::to_string),
        disc_number: track.disc_number,
        duration_ms: track.duration_ms,
        explicit: track
            .explicit
            .map(|e| if e { "explicit" } else { "clean" }.to_owned()),
        isrc: track.isrc.clone(),
        kind: String::from("track"),
        playable: (!track.playable).then_some(false),
        quality: track.quality.map(|q| q.as_str().to_owned()),
        reference: track.reference.clone(),
        service_id: track.service.to_string(),
        song_id: track
            .song_key
            .as_ref()
            .and_then(|k| owned.get(k))
            .map(ToString::to_string),
        song_key: track.song_key.clone(),
        title: track.title.clone(),
        track_number: track.track_number,
    }
}

pub fn album_body(album: &CatalogAlbum) -> wire::CatalogAlbum {
    wire::CatalogAlbum {
        album_key: album.album_key.clone(),
        artist: album.artist.clone(),
        artists: credits(&album.artists),
        artwork_id: album.artwork.as_ref().map(ToString::to_string),
        kind: String::from("album"),
        name: album.name.clone(),
        reference: album.reference.clone(),
        service_id: album.service.to_string(),
        track_count: album.track_count,
        year: album.year,
    }
}

pub fn artist_body(artist: &CatalogArtist) -> wire::CatalogArtist {
    wire::CatalogArtist {
        artwork_id: artist.artwork.as_ref().map(ToString::to_string),
        identity: artist.identity.clone(),
        kind: String::from("artist"),
        name: artist.name.clone(),
        reference: artist.reference.clone(),
        service_id: artist.service.to_string(),
    }
}

pub fn playlist_body(playlist: &CatalogPlaylist) -> wire::CatalogPlaylist {
    wire::CatalogPlaylist {
        artwork_id: playlist.artwork.as_ref().map(ToString::to_string),
        editable: playlist.editable,
        kind: String::from("playlist"),
        name: playlist.name.clone(),
        owner: playlist.owner.clone(),
        reference: playlist.reference.clone(),
        service_id: playlist.service.to_string(),
        track_count: playlist.track_count,
    }
}

pub fn radio_body(radio: &CatalogRadio) -> wire::CatalogRadio {
    wire::CatalogRadio {
        artwork_id: radio.artwork.as_ref().map(ToString::to_string),
        kind: String::from("radio"),
        name: radio.name.clone(),
        reference: radio.reference.clone(),
        service_id: radio.service.to_string(),
    }
}

pub fn services_body(list: &ServiceList) -> wire::CatalogServices {
    wire::CatalogServices {
        services: list
            .services
            .iter()
            .map(|s| wire::CatalogService {
                account: wire::Account {
                    detail: s.account_detail.clone(),
                    state: s.account.as_str().to_owned(),
                },
                capabilities: s
                    .capabilities
                    .iter()
                    .map(|c| c.as_str().to_owned())
                    .collect(),
                id: s.id.to_string(),
                logo_key: s.logo_key.clone(),
                name: s.name.clone(),
                quality: s.quality.map(|q| q.as_str().to_owned()),
            })
            .collect(),
        since: list.since.map(Time),
        state: list.state.as_str().to_owned(),
    }
}

pub fn services_event_body(state: SpeakersState) -> wire::CatalogServicesEvent {
    wire::CatalogServicesEvent {
        state: state.as_str().to_owned(),
    }
}

/// A search's sections: only those asked for, each as Music Assistant ranked it.
pub fn search_body(results: &CatalogResults, owned: &Owned) -> wire::CatalogSearch {
    let kinds = results.kinds;
    let items = |on: bool| on.then_some(());
    wire::CatalogSearch {
        albums: items(kinds.albums).map(|()| wire::Items {
            items: results.albums.iter().map(album_body).collect(),
        }),
        artists: items(kinds.artists).map(|()| wire::Items {
            items: results.artists.iter().map(artist_body).collect(),
        }),
        complete: results.complete,
        playlists: items(kinds.playlists).map(|()| wire::Items {
            items: results.playlists.iter().map(playlist_body).collect(),
        }),
        radio: items(kinds.radio).map(|()| wire::Items {
            items: results.radio.iter().map(radio_body).collect(),
        }),
        services: results
            .services
            .iter()
            .map(|s| wire::ServiceState {
                id: s.service.to_string(),
                state: s.state.as_str().to_owned(),
            })
            .collect(),
        state: None,
        tracks: items(kinds.tracks).map(|()| wire::Items {
            items: results
                .tracks
                .iter()
                .map(|t| track_body(t, owned))
                .collect(),
        }),
    }
}

/// The `catalog` section of `/search` while Music Assistant is not `ok`.
pub fn search_state_body(state: SpeakersState) -> wire::CatalogSearch {
    wire::CatalogSearch {
        albums: None,
        artists: None,
        complete: true,
        playlists: None,
        radio: None,
        services: Vec::new(),
        state: Some(state.as_str().to_owned()),
        tracks: None,
    }
}

pub fn detail_body(detail: &CatalogDetail, owned: &Owned) -> wire::CatalogDetail {
    let tracks = |list: &[CatalogTrack]| list.iter().map(|t| track_body(t, owned)).collect();
    match detail {
        CatalogDetail::Track(track) => wire::CatalogDetail::Track {
            track: track_body(track, owned),
        },
        CatalogDetail::Album { album, tracks: t } => wire::CatalogDetail::Album {
            album: album_body(album),
            tracks: tracks(t),
        },
        CatalogDetail::Artist {
            artist,
            top_tracks,
            albums,
        } => wire::CatalogDetail::Artist {
            albums: albums.iter().map(album_body).collect(),
            artist: artist_body(artist),
            top_tracks: tracks(top_tracks),
        },
        CatalogDetail::Playlist {
            playlist,
            tracks: t,
            next_cursor,
        } => wire::CatalogDetail::Playlist {
            next_cursor: next_cursor.clone(),
            playlist: playlist_body(playlist),
            tracks: tracks(t),
        },
        CatalogDetail::Radio(radio) => wire::CatalogDetail::Radio {
            radio: radio_body(radio),
        },
    }
}

fn detail_tracks(detail: &CatalogDetail) -> Vec<&CatalogTrack> {
    match detail {
        CatalogDetail::Track(track) => vec![track],
        CatalogDetail::Album { tracks, .. } | CatalogDetail::Playlist { tracks, .. } => {
            tracks.iter().collect()
        }
        CatalogDetail::Artist { top_tracks, .. } => top_tracks.iter().collect(),
        CatalogDetail::Radio(_) => Vec::new(),
    }
}

pub fn item_body(entry: &CatalogEntry, owned: &Owned) -> wire::CatalogItem {
    match entry {
        CatalogEntry::Track(t) => wire::CatalogItem::Track(track_body(t, owned)),
        CatalogEntry::Album(a) => wire::CatalogItem::Album(album_body(a)),
        CatalogEntry::Artist(a) => wire::CatalogItem::Artist(artist_body(a)),
        CatalogEntry::Playlist(p) => wire::CatalogItem::Playlist(playlist_body(p)),
        CatalogEntry::Radio(r) => wire::CatalogItem::Radio(radio_body(r)),
    }
}

/// The shelves in Music Assistant's order, and how each service answered.
pub fn recommendations_body(
    found: &Recommendations,
    owned: &Owned,
) -> wire::CatalogRecommendations {
    wire::CatalogRecommendations {
        complete: found.complete,
        services: found
            .services
            .iter()
            .map(|s| wire::ServiceState {
                id: s.service.to_string(),
                state: s.state.as_str().to_owned(),
            })
            .collect(),
        shelves: found
            .shelves
            .iter()
            .map(|shelf| wire::CatalogShelf {
                id: shelf.id.to_string(),
                items: shelf.items.iter().map(|e| item_body(e, owned)).collect(),
                service_id: shelf.service.to_string(),
                subtitle: shelf.subtitle.clone(),
                title: shelf.title.clone(),
                kind: (shelf.kind != ShelfKind::Default).then(|| shelf.kind.as_str().to_owned()),
            })
            .collect(),
    }
}

pub fn library_body(page: &LibraryPage, owned: &Owned) -> wire::CatalogLibrary {
    wire::CatalogLibrary {
        items: page.items.iter().map(|e| item_body(e, owned)).collect(),
        next_cursor: page.next_cursor.clone(),
        synced_at: page.synced_at.map(Time),
    }
}

/// `types` of a catalog search: kinds or their plurals; absent means the default four.
pub(crate) fn kinds_of(types: Option<&str>, field: &str) -> ApiResult<CatalogKinds> {
    let Some(types) = types.filter(|t| !t.trim().is_empty()) else {
        return Ok(CatalogKinds::DEFAULT);
    };
    let mut kinds = CatalogKinds::NONE;
    for name in types.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let kind = CatalogKind::parse_plural(name).ok_or_else(|| {
            ApiError::bad_field(
                field,
                format!("{field} holds track, album, artist, playlist or radio."),
            )
        })?;
        kinds = kinds.with(kind);
    }
    Ok(kinds)
}

/// The search a request asks for, checked.
pub(crate) fn search_query(
    q: Option<&str>,
    kinds: CatalogKinds,
    services: Option<&str>,
    limit: Option<u32>,
) -> ApiResult<CatalogSearch> {
    let text = q.unwrap_or_default().trim().to_owned();
    if text.chars().count() > MAX_QUERY_LEN {
        return Err(ApiError::bad_field(
            "q",
            format!("q is at most {MAX_QUERY_LEN} characters."),
        ));
    }
    Ok(CatalogSearch {
        text,
        kinds,
        services: services_of(services)?,
        limit: limit.unwrap_or(SEARCH_LIMIT).clamp(1, SEARCH_LIMIT_MAX),
    })
}

/// `services`: comma-separated `b` ids, at most [`MAX_SERVICES`]; absent or empty means
/// every service.
fn services_of(services: Option<&str>) -> ApiResult<Option<Vec<ServiceId>>> {
    services
        .filter(|s| !s.trim().is_empty())
        .map(|list| {
            let names: Vec<&str> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if names.len() > MAX_SERVICES {
                return Err(ApiError::bad_field(
                    "services",
                    format!("services holds at most {MAX_SERVICES}."),
                ));
            }
            names
                .into_iter()
                .map(|s| body_id::<ServiceId>(s, "services"))
                .collect::<ApiResult<Vec<_>>>()
        })
        .transpose()
}

/// The recommendations a request asks for, checked: `limit` 1 to
/// [`RECOMMENDATIONS_LIMIT_MAX`] items per shelf.
pub(crate) fn recommendations_query(
    services: Option<&str>,
    limit: Option<u32>,
) -> ApiResult<RecommendationsQuery> {
    let limit = limit.unwrap_or(RECOMMENDATIONS_LIMIT);
    if !(1..=RECOMMENDATIONS_LIMIT_MAX).contains(&limit) {
        return Err(ApiError::bad_field(
            "limit",
            format!("limit is 1 to {RECOMMENDATIONS_LIMIT_MAX}."),
        ));
    }
    Ok(RecommendationsQuery {
        services: services_of(services)?.filter(|ids| !ids.is_empty()),
        limit,
    })
}

/// Runs a search for `/catalog/search` or `/search`: counted against the caller's limit,
/// with owned songs named.
pub(crate) async fn run_search(
    state: &ApiState,
    principal: &Principal,
    catalog: &dyn CatalogControl,
    query: CatalogSearch,
) -> ApiResult<wire::CatalogSearch> {
    state.auth.catalog_search(principal)?;
    let results = catalog.search(&principal.access, query).await?;
    let owned = owned(state, &principal.access, results.tracks.iter()).await?;
    Ok(search_body(&results, &owned))
}

async fn services(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let catalog = allowed(&state, &principal)?;
    let list = catalog.services(&principal.access).await;
    Ok(json_with(StatusCode::OK, &services_body(&list)))
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
    types: Option<String>,
    services: Option<String>,
    limit: Option<u32>,
}

async fn search(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Response> {
    let catalog = allowed(&state, &principal)?;
    let query = search_query(
        params.q.as_deref(),
        kinds_of(params.types.as_deref(), "types")?,
        params.services.as_deref(),
        params.limit,
    )?;
    let body = run_search(&state, &principal, catalog, query).await?;
    Ok(json_with(StatusCode::OK, &body))
}

#[derive(Debug, Deserialize)]
struct PageParams {
    cursor: Option<String>,
    limit: Option<u32>,
    kind: Option<String>,
}

fn page_of(params: &PageParams) -> ApiResult<CatalogPage> {
    let limit = params.limit.unwrap_or(PAGE_LIMIT);
    if !(1..=PAGE_LIMIT_MAX).contains(&limit) {
        return Err(ApiError::bad_field(
            "limit",
            format!("limit is 1 to {PAGE_LIMIT_MAX}."),
        ));
    }
    Ok(CatalogPage {
        cursor: params.cursor.clone(),
        limit,
    })
}

async fn item(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(reference): Path<String>,
    Query(params): Query<PageParams>,
) -> ApiResult<Response> {
    let catalog = allowed(&state, &principal)?;
    state.auth.catalog_read(&principal)?;
    let page = page_of(&params)?;
    let detail = catalog.item(&principal.access, &reference, page).await?;
    let owned = owned(
        &state,
        &principal.access,
        detail_tracks(&detail).into_iter(),
    )
    .await?;
    Ok(json_with(StatusCode::OK, &detail_body(&detail, &owned)))
}

async fn library(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<PageParams>,
) -> ApiResult<Response> {
    let catalog = allowed(&state, &principal)?;
    let service: ServiceId = path_id(&id)?;
    state.auth.catalog_read(&principal)?;
    let kind = match params.kind.as_deref() {
        None | Some("") => CatalogKind::Playlist,
        Some(kind) => CatalogKind::parse_plural(kind)
            .filter(|k| *k != CatalogKind::Radio)
            .ok_or_else(|| {
                ApiError::bad_field("kind", "kind is track, album, artist or playlist.")
            })?,
    };
    let page = page_of(&params)?;
    let found = catalog
        .library(&principal.access, &service, kind, page)
        .await?;
    let tracks = found.items.iter().filter_map(|e| match e {
        CatalogEntry::Track(t) => Some(t),
        _ => None,
    });
    let owned = owned(&state, &principal.access, tracks).await?;
    Ok(json_with(StatusCode::OK, &library_body(&found, &owned)))
}

#[derive(Debug, Deserialize)]
struct RecommendationParams {
    services: Option<String>,
    limit: Option<u32>,
}

async fn recommendations(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(params): Query<RecommendationParams>,
) -> ApiResult<Response> {
    let catalog = allowed(&state, &principal)?;
    let query = recommendations_query(params.services.as_deref(), params.limit)?;
    state.auth.catalog_read(&principal)?;
    let found = catalog.recommendations(&principal.access, query).await?;
    let owned = owned(&state, &principal.access, found.tracks()).await?;
    Ok(json_with(
        StatusCode::OK,
        &recommendations_body(&found, &owned),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_queries_are_checked() -> ApiResult<()> {
        let query = recommendations_query(None, None)?;
        assert_eq!(query.limit, RECOMMENDATIONS_LIMIT);
        assert_eq!(query.services, None);
        assert_eq!(recommendations_query(Some(" , "), Some(50))?.services, None);
        assert!(recommendations_query(None, Some(0)).is_err());
        assert!(recommendations_query(None, Some(51)).is_err());
        assert!(recommendations_query(Some("nope"), None).is_err());
        let many = vec!["bqtyy4kcexcsg6wa0y4w5"; MAX_SERVICES + 1].join(",");
        assert!(recommendations_query(Some(&many), None).is_err());
        let one = recommendations_query(Some("bqtyy4kcexcsg6wa0y4w5"), Some(5))?;
        assert_eq!(one.services.map(|s| s.len()), Some(1));
        Ok(())
    }

    #[test]
    fn types_parse_with_plurals_and_default_to_four() -> ApiResult<()> {
        assert_eq!(kinds_of(None, "types")?, CatalogKinds::DEFAULT);
        let kinds = kinds_of(Some("tracks, radio"), "types")?;
        assert!(kinds.tracks && kinds.radio && !kinds.albums);
        assert!(kinds_of(Some("song"), "types").is_err());
        Ok(())
    }

    #[test]
    fn queries_are_checked() -> ApiResult<()> {
        let query = search_query(Some("  halo "), CatalogKinds::DEFAULT, None, Some(500))?;
        assert_eq!(query.text, "halo");
        assert_eq!(query.limit, SEARCH_LIMIT_MAX);
        assert!(search_query(Some(&"x".repeat(201)), CatalogKinds::DEFAULT, None, None).is_err());
        assert!(
            search_query(Some("halo"), CatalogKinds::DEFAULT, Some("nope"), None).is_err(),
            "a malformed service id"
        );
        Ok(())
    }
}
