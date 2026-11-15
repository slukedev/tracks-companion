//! Library browsing and search (api.md sections 8 and 9). Every read filters by the caller's
//! libraries inside the repositories.

use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use http::{HeaderMap, StatusCode};
use serde::Deserialize;
use trackside_auth::Scopes;
use trackside_auth::extract::Authed;
use trackside_core::records::ItemRef;
use trackside_core::repo::albums::{self, AlbumFilter, AlbumSort};
use trackside_core::repo::enrichment as enrichment_repo;
use trackside_core::repo::songs::{self, SongFilter};
use trackside_core::repo::{artists, folders, genres, libraries, lyrics};
use trackside_core::search::{self, SearchKinds, SearchQuery};
use trackside_core::{AlbumId, ArtistId, FolderId, LibraryId, PublicId, SongId, similar};

use super::{annotations, cached_json, enrichment, library_etag, read, songs_with_me};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Query, body_id, json_with, path_id};
use crate::wire::library::{
    AlbumPage, Albums, ArtistPage, Artists, ExternalSection, Folder, FolderPage, Genre, Genres,
    SearchResults, SimilarArtist, SimilarArtists, Songs, SuggestedSongs, Suggestion,
};
use crate::wire::{Libraries, Section};
use crate::{ApiState, map};

const LIBRARY: u8 = Scopes::LIBRARY.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/libraries", get(list_libraries))
        .route("/songs", get(songs_by_ids))
        .route("/songs/random", get(random_songs))
        .route("/songs/{id}", get(song))
        .route("/songs/{id}/similar", get(similar_songs))
        .route("/songs/{id}/lyrics", get(song_lyrics))
        .route("/albums", get(list_albums))
        .route("/albums/{id}", get(album))
        .route("/artists", get(list_artists))
        .route("/artists/{id}", get(artist))
        .route("/artists/{id}/top-songs", get(top_songs))
        .route("/artists/{id}/similar", get(similar_artists))
        .route("/genres", get(list_genres))
        .route("/folders/{id}", get(folder))
        .route("/search", get(search_library))
}

fn limit(value: Option<u32>, default: u32, max: u32) -> u32 {
    value.unwrap_or(default).clamp(1, max)
}

/// The caller's library narrowed by `library_id`, checked against what they may see.
async fn narrow(
    state: &ApiState,
    access: &trackside_core::Access,
    library_id: Option<&str>,
) -> ApiResult<Option<trackside_core::LibraryKey>> {
    let Some(raw) = library_id else {
        return Ok(None);
    };
    let id: LibraryId = body_id(raw, "library_id")?;
    let access = access.clone();
    let library = read(state, move |c| libraries::get(c, &access, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Some(library.key))
}

async fn list_libraries(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let access = principal.access.clone();
    let list = read(&state, move |c| libraries::list(c, &access)).await?;
    let body = Libraries {
        libraries: list
            .iter()
            .map(|l| map::library(l, super::available(&state, &l.id)))
            .collect(),
    };
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

async fn song(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let id: SongId = path_id(&id)?;
    let access = principal.access.clone();
    let found = read(&state, move |c| songs::get(c, &access, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mut out = songs_with_me(&state, &principal.access, vec![found]).await?;
    let body = out.pop().ok_or_else(ApiError::not_found)?;
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

#[derive(Debug, Deserialize)]
struct IdsQuery {
    ids: Option<String>,
}

async fn songs_by_ids(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<IdsQuery>,
) -> ApiResult<Response> {
    let raw = query.ids.unwrap_or_default();
    let ids: Vec<SongId> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| SongId::parse(s).ok())
        .collect();
    if ids.len() > 500 {
        return Err(ApiError::bad_field("ids", "At most 500 ids at once."));
    }
    let access = principal.access.clone();
    let found = read(&state, move |c| songs::get_many(c, &access, &ids)).await?;
    let songs = songs_with_me(&state, &principal.access, found).await?;
    Ok(json_with(StatusCode::OK, &Songs { songs }))
}

#[derive(Debug, Deserialize)]
struct RandomQuery {
    count: Option<u32>,
    genre: Option<String>,
    year_from: Option<i32>,
    year_to: Option<i32>,
    library_id: Option<String>,
}

async fn random_songs(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<RandomQuery>,
) -> ApiResult<Response> {
    let filter = SongFilter {
        library: narrow(&state, &principal.access, query.library_id.as_deref()).await?,
        genre: query.genre.map(|g| tracks_folded(&g)),
        year_from: query.year_from,
        year_to: query.year_to,
    };
    let count = limit(query.count, 40, 500);
    let access = principal.access.clone();
    let found = read(&state, move |c| songs::random(c, &access, &filter, count)).await?;
    let songs = songs_with_me(&state, &principal.access, found).await?;
    Ok(json_with(StatusCode::OK, &Songs { songs }))
}

/// Genre names are matched folded, as core stores them.
fn tracks_folded(text: &str) -> String {
    tracks_model::folded(text)
}

#[derive(Debug, Deserialize)]
struct CountQuery {
    count: Option<u32>,
}

/// How many not-yet-owned songs a suggestion list carries at most.
const EXTERNAL_MAX: u32 = 20;

/// Whether this caller may see songs the server does not have (api.md section 9).
fn sees_external(state: &ApiState, principal: &trackside_auth::Principal) -> bool {
    super::account::acquisition_feature(state, principal).is_some()
        && principal.scopes.contains(Scopes::REQUEST)
}

async fn similar_songs(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<CountQuery>,
) -> ApiResult<Response> {
    let id: SongId = path_id(&id)?;
    let count = limit(query.count, 20, 100);
    let access = principal.access.clone();
    let seed_id = id.clone();
    let seed = read(&state, move |c| {
        songs::get(c, &access, &seed_id)?.ok_or(trackside_core::DbError::NotFound)
    })
    .await?;
    enrichment::look_up(&state, enrichment::song_targets(&seed)).await;
    let access = principal.access.clone();
    let external = sees_external(&state, &principal);
    let (found, missing) = read(&state, move |c| {
        let found = similar::similar_songs(c, &access, &id, count)?;
        let missing = if external {
            enrichment_repo::missing_similar(c, &access, &seed.song_key, count.min(EXTERNAL_MAX))?
        } else {
            Vec::new()
        };
        Ok((found, missing))
    })
    .await?;
    let songs = songs_with_me(&state, &principal.access, found).await?;
    Ok(json_with(
        StatusCode::OK,
        &SuggestedSongs {
            songs,
            external: suggestions(external, missing),
        },
    ))
}

fn suggestions(external: bool, missing: Vec<(String, String)>) -> Option<Vec<Suggestion>> {
    external.then(|| {
        missing
            .into_iter()
            .map(|(title, artist)| Suggestion { title, artist })
            .collect()
    })
}

async fn song_lyrics(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: SongId = path_id(&id)?;
    let access = principal.access.clone();
    let found = read(&state, move |c| lyrics::for_song(c, &access, &id)).await?;
    if found.is_empty() {
        return Err(ApiError::not_found());
    }
    Ok(json_with(StatusCode::OK, &map::lyrics(&found)))
}

#[derive(Debug, Deserialize)]
struct AlbumsQuery {
    sort: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    genre: Option<String>,
    year_from: Option<i32>,
    year_to: Option<i32>,
    seed: Option<u64>,
    library_id: Option<String>,
}

pub(crate) fn album_sort(sort: Option<&str>, seed: Option<u64>) -> ApiResult<AlbumSort> {
    Ok(match sort.unwrap_or("newest") {
        "newest" => AlbumSort::Newest,
        "recent" => AlbumSort::Recent,
        "frequent" => AlbumSort::Frequent,
        "random" => AlbumSort::Random {
            seed: seed.unwrap_or_else(rand_seed),
        },
        "alphabetical" => AlbumSort::Alphabetical,
        "alphabetical_by_artist" => AlbumSort::AlphabeticalByArtist,
        "starred" => AlbumSort::Starred,
        "year" => AlbumSort::Year,
        "highest" => AlbumSort::Highest,
        _ => return Err(ApiError::bad_field("sort", "Unknown sort.")),
    })
}

fn rand_seed() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

async fn list_albums(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<AlbumsQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let sort = album_sort(query.sort.as_deref(), query.seed)?;
    let filter = AlbumFilter {
        library: narrow(&state, &principal.access, query.library_id.as_deref()).await?,
        genre: query.genre,
        year_from: query.year_from,
        year_to: query.year_to,
    };
    let count = limit(query.limit, 100, 500);
    let cursor = if matches!(sort, AlbumSort::Random { .. }) {
        None
    } else {
        query.cursor
    };
    let access = principal.access.clone();
    let page = read(&state, move |c| {
        albums::list(c, &access, sort, cursor.as_deref(), count, &filter)
    })
    .await?;
    let annotations = annotations(
        &state,
        &principal.access,
        page.items.iter().map(|a| ItemRef::Album(a.id.clone())),
    )
    .await?;
    let body = Albums {
        albums: map::albums_with_me(&page.items, &annotations),
        next_cursor: page.next,
    };
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

async fn album(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let id: AlbumId = path_id(&id)?;
    let access = principal.access.clone();
    let (found, tracks) = read(&state, move |c| {
        let album = albums::get(c, &access, &id)?.ok_or(trackside_core::DbError::NotFound)?;
        let tracks = songs::by_album(c, &access, &id)?;
        Ok((album, tracks))
    })
    .await?;
    let annotations = annotations(
        &state,
        &principal.access,
        std::iter::once(ItemRef::Album(found.id.clone()))
            .chain(tracks.iter().map(|s| ItemRef::Song(s.id.clone()))),
    )
    .await?;
    let body = AlbumPage {
        album: map::albums_with_me(std::slice::from_ref(&found), &annotations)
            .pop()
            .ok_or_else(ApiError::not_found)?,
        songs: map::songs_with_me(&tracks, &annotations),
    };
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

#[derive(Debug, Deserialize)]
struct PageQuery {
    limit: Option<u32>,
    cursor: Option<String>,
    library_id: Option<String>,
}

async fn list_artists(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let library = narrow(&state, &principal.access, query.library_id.as_deref()).await?;
    let count = limit(query.limit, 100, 500);
    let access = principal.access.clone();
    let cursor = query.cursor;
    let page = read(&state, move |c| {
        artists::list(c, &access, library, cursor.as_deref(), count)
    })
    .await?;
    let body = Artists {
        artists: page.items.iter().map(map::artist).collect(),
        next_cursor: page.next,
    };
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

async fn artist(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let id: ArtistId = path_id(&id)?;
    let access = principal.access.clone();
    let (found, own, appears, top, bio) = read(&state, move |c| {
        let artist = artists::get(c, &access, &id)?.ok_or(trackside_core::DbError::NotFound)?;
        let own = albums::by_artist(c, &access, &id)?;
        let appears = albums::appears_on(c, &access, &id)?;
        let top = songs::top_for_artist(c, &access, &id, 10)?;
        let bio = enrichment_repo::bio(c, &artist.identity)?;
        Ok((artist, own, appears, top, bio))
    })
    .await?;
    if bio.is_none() {
        // The page answers at once; the next visit has what this lookup finds.
        let state = state.clone();
        let target = enrichment::artist_target(&found);
        tokio::spawn(async move { enrichment::look_up(&state, vec![target]).await });
    }
    let items = std::iter::once(ItemRef::Artist(found.id.clone()))
        .chain(
            own.iter()
                .chain(&appears)
                .map(|a| ItemRef::Album(a.id.clone())),
        )
        .chain(top.iter().map(|s| ItemRef::Song(s.id.clone())));
    let annotations = annotations(&state, &principal.access, items).await?;
    let mut artist = map::artist(&found);
    artist.me = annotations
        .get(&ItemRef::Artist(found.id.clone()))
        .map(map::me);
    let body = ArtistPage {
        artist,
        albums: map::albums_with_me(&own, &annotations),
        appears_on: map::albums_with_me(&appears, &annotations),
        top_songs: map::songs_with_me(&top, &annotations),
        bio,
    };
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

async fn top_songs(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<CountQuery>,
) -> ApiResult<Response> {
    let id: ArtistId = path_id(&id)?;
    let count = limit(query.count, 20, 100);
    let found = visible_artist(&state, &principal.access, &id).await?;
    enrichment::look_up(&state, vec![enrichment::artist_target(&found)]).await;
    let access = principal.access.clone();
    let external = sees_external(&state, &principal);
    let (top, missing) = read(&state, move |c| {
        let top = songs::top_for_artist(c, &access, &id, count)?;
        let missing = if external {
            enrichment_repo::missing_top(c, &access, &found.identity, count.min(EXTERNAL_MAX))?
                .into_iter()
                .map(|title| (title, found.name.clone()))
                .collect()
        } else {
            Vec::new()
        };
        Ok((top, missing))
    })
    .await?;
    let songs = songs_with_me(&state, &principal.access, top).await?;
    Ok(json_with(
        StatusCode::OK,
        &SuggestedSongs {
            songs,
            external: suggestions(external, missing),
        },
    ))
}

async fn visible_artist(
    state: &ApiState,
    access: &trackside_core::Access,
    id: &ArtistId,
) -> ApiResult<trackside_core::records::Artist> {
    let (access, id) = (access.clone(), id.clone());
    read(state, move |c| {
        artists::get(c, &access, &id)?.ok_or(trackside_core::DbError::NotFound)
    })
    .await
}

async fn similar_artists(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<CountQuery>,
) -> ApiResult<Response> {
    let id: ArtistId = path_id(&id)?;
    let count = limit(query.count, 15, 100);
    let seed = visible_artist(&state, &principal.access, &id).await?;
    enrichment::look_up(&state, vec![enrichment::artist_target(&seed)]).await;
    let access = principal.access.clone();
    let found = read(&state, move |c| {
        similar::similar_artists(c, &access, &id, count)
    })
    .await?;
    let body = SimilarArtists {
        artists: found
            .iter()
            .map(|s| SimilarArtist {
                artist: s.artist.as_ref().map(map::artist),
                name: s.name.clone(),
                identity: s.identity.clone(),
                score: s.score,
                in_library: s.in_library,
            })
            .collect(),
    };
    Ok(json_with(StatusCode::OK, &body))
}

async fn list_genres(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let access = principal.access.clone();
    let found = read(&state, move |c| genres::list(c, &access)).await?;
    let body = Genres {
        genres: found
            .into_iter()
            .map(|g| Genre {
                name: g.name,
                song_count: g.song_count,
                album_count: g.album_count,
            })
            .collect(),
    };
    Ok(cached_json(
        &headers,
        &library_etag(&state, &principal.access),
        &body,
    ))
}

fn folder_wire(folder: &trackside_core::records::Folder) -> Folder {
    Folder {
        id: folder.id.to_string(),
        name: folder.name.clone(),
        parent_id: folder.parent.as_ref().map(ToString::to_string),
    }
}

async fn folder(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: FolderId = path_id(&id)?;
    let access = principal.access.clone();
    let (found, children, tracks) = read(&state, move |c| {
        let folder = folders::get(c, &access, &id)?.ok_or(trackside_core::DbError::NotFound)?;
        let children = folders::children(c, &access, &id)?;
        let tracks = songs::by_folder(c, &access, &id)?;
        Ok((folder, children, tracks))
    })
    .await?;
    let body = FolderPage {
        folder: folder_wire(&found),
        folders: children.iter().map(folder_wire).collect(),
        songs: songs_with_me(&state, &principal.access, tracks).await?,
    };
    Ok(json_with(StatusCode::OK, &body))
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
    types: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    library_id: Option<String>,
}

/// Which sections `types` asks for. `external` is never on by default.
pub(crate) fn search_kinds(types: Option<&str>) -> ApiResult<(SearchKinds, bool)> {
    let Some(types) = types.filter(|t| !t.trim().is_empty()) else {
        return Ok((SearchKinds::ALL, false));
    };
    let mut kinds = SearchKinds {
        songs: false,
        albums: false,
        artists: false,
    };
    let mut external = false;
    for name in types.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match name {
            "song" | "songs" => kinds.songs = true,
            "album" | "albums" => kinds.albums = true,
            "artist" | "artists" => kinds.artists = true,
            "external" => external = true,
            _ => return Err(ApiError::bad_field("types", "Unknown type.")),
        }
    }
    Ok((kinds, external))
}

/// How long the library search waits for not-yet-owned results when they are asked for.
const EXTERNAL_WAIT: Duration = Duration::from_secs(8);

async fn search_library(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Response> {
    let text = params.q.unwrap_or_default();
    let (kinds, external) = search_kinds(params.types.as_deref())?;
    if params.cursor.is_some()
        && [kinds.songs, kinds.albums, kinds.artists]
            .iter()
            .filter(|k| **k)
            .count()
            != 1
    {
        return Err(ApiError::bad_field(
            "types",
            "A cursor pages one type; name it in types.",
        ));
    }
    let query = SearchQuery {
        text: text.clone(),
        kinds,
        limit: limit(params.limit, 20, 500),
        cursor: params.cursor,
        library: narrow(&state, &principal.access, params.library_id.as_deref()).await?,
    };
    let any_library = kinds.songs || kinds.albums || kinds.artists;
    let access = principal.access.clone();
    let results = if any_library && !text.trim().is_empty() {
        read(&state, move |c| search::search(c, &access, &query)).await?
    } else {
        search::SearchResults::default()
    };
    let annotations = annotations(
        &state,
        &principal.access,
        results
            .songs
            .items
            .iter()
            .map(|s| ItemRef::Song(s.id.clone())),
    )
    .await?;
    let mut body = SearchResults {
        songs: kinds.songs.then(|| {
            Section::new(
                map::songs_with_me(&results.songs.items, &annotations),
                results.songs.next.clone(),
            )
        }),
        albums: kinds.albums.then(|| {
            Section::new(
                results.albums.items.iter().map(map::album).collect(),
                results.albums.next.clone(),
            )
        }),
        artists: kinds.artists.then(|| {
            Section::new(
                results.artists.items.iter().map(map::artist).collect(),
                results.artists.next.clone(),
            )
        }),
        external: None,
    };
    let available = super::account::acquisition_feature(&state, &principal).is_some()
        && principal.scopes.contains(Scopes::REQUEST);
    if external && available && !text.trim().is_empty() {
        let search = tokio::time::timeout(
            EXTERNAL_WAIT,
            state.acquire.search(
                &principal.access,
                trackside_core::control::ExternalQuery::Text(text),
            ),
        )
        .await;
        match search {
            Ok(Ok(found)) => {
                body.external = Some(ExternalSection {
                    search_id: found.search_id,
                    complete: found.complete,
                    candidates: found.candidates.iter().map(map::candidate).collect(),
                });
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "external search failed"),
            Err(_) => tracing::warn!("external search timed out"),
        }
    }
    Ok(json_with(StatusCode::OK, &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_types_default_to_the_library_only() -> Result<(), ApiError> {
        assert_eq!(search_kinds(None)?, (SearchKinds::ALL, false));
        let (kinds, external) = search_kinds(Some("song,external"))?;
        assert!(kinds.songs && !kinds.albums && !kinds.artists && external);
        assert!(search_kinds(Some("video")).is_err());
        Ok(())
    }

    #[test]
    fn album_sorts_parse() {
        assert!(matches!(album_sort(None, None), Ok(AlbumSort::Newest)));
        assert!(matches!(
            album_sort(Some("random"), Some(7)),
            Ok(AlbumSort::Random { seed: 7 })
        ));
        assert!(album_sort(Some("loudest"), None).is_err());
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(limit(None, 100, 500), 100);
        assert_eq!(limit(Some(0), 100, 500), 1);
        assert_eq!(limit(Some(9000), 100, 500), 500);
    }
}
