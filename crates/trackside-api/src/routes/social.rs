//! Listens, now playing, stars, ratings, playlists, the play queue and bookmarks (api.md
//! section 12).

use std::collections::HashMap;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use serde::Deserialize;
use trackside_auth::Scopes;
use trackside_auth::extract::Authed;
use trackside_core::now_playing::{NowPlayingReport, PlayState};
use trackside_core::records::{ItemKind, ItemRef, Song};
use trackside_core::repo::listens::{
    self, ListenOutcome, ListenSource, ListenStatus, NewListen, RejectReason,
};
use trackside_core::repo::play_queue::{self, PlayQueueUpdate};
use trackside_core::repo::playlists::{self, NewPlaylist, PlaylistPatch};
use trackside_core::repo::{albums, artists, bookmarks, ratings, songs, stars};
use trackside_core::{Access, Millis, PlaylistId, PublicId, SongId};

use super::{idempotency, read, songs_with_me, write};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, OptionalJson, Query, body_id, json_with, parse_json, path_id};
use crate::wire::library::{Starred, StarredItem};
use crate::wire::social::{
    AppendEntries, Bookmark, BookmarkBody, Bookmarks, ListenBatch, ListenItem, ListenResult,
    ListenResults, NowPlaying, PlaylistEntry, PlaylistPage, Playlists, RatingBody, ReplaceEntries,
    StarBody,
};
use crate::{ApiState, map};

const LIBRARY: u8 = Scopes::LIBRARY.bits();
const WRITE: u8 = Scopes::WRITE.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/listens", post(submit_listens))
        .route("/now-playing", post(now_playing))
        .route("/me/stars", get(list_stars))
        .route("/me/stars/{kind}/{id}", put(star).delete(unstar))
        .route("/me/ratings/{kind}/{id}", put(rate).delete(unrate))
        .route("/playlists", get(list_playlists).post(create_playlist))
        .route(
            "/playlists/{id}",
            get(get_playlist)
                .patch(patch_playlist)
                .delete(delete_playlist),
        )
        .route(
            "/playlists/{id}/entries",
            put(replace_entries).post(append_entries),
        )
        .route("/me/play-queue", get(get_queue).put(put_queue))
        .route("/me/bookmarks", get(list_bookmarks))
        .route(
            "/me/bookmarks/{song_id}",
            put(put_bookmark).delete(delete_bookmark),
        )
}

const MAX_LISTENS: usize = 1000;
/// A listen more than this far after the client's own clock is rejected as `future`.
const FUTURE_SLACK: Duration = Duration::from_secs(24 * 3600);

/// One listen, checked: either ready for core or already decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Checked {
    Ready(NewListen),
    Rejected(RejectReason),
}

/// Checks one item. `songs` holds the visible songs named by the batch, to fill in missing
/// text.
pub fn check_listen<S: std::hash::BuildHasher>(
    item: &ListenItem,
    client_time: Millis,
    songs: &HashMap<SongId, Song, S>,
) -> Checked {
    let id = item.client_listen_id.trim();
    if id.is_empty() || id.len() > 200 {
        return Checked::Rejected(RejectReason::Invalid);
    }
    let Some(started_at) = Millis::parse_rfc3339(&item.started_at) else {
        return Checked::Rejected(RejectReason::Invalid);
    };
    if started_at > client_time.saturating_add(FUTURE_SLACK) {
        return Checked::Rejected(RejectReason::Future);
    }
    let song = match item.song_id.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => match SongId::parse(raw) {
            Ok(id) => Some(id),
            Err(_) => return Checked::Rejected(RejectReason::Invalid),
        },
        None => None,
    };
    let known = song.as_ref().and_then(|id| songs.get(id));
    let text = |given: &Option<String>, from_song: Option<&String>| {
        given
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .or_else(|| from_song.cloned())
    };
    let (Some(title), Some(artist)) = (
        text(&item.title, known.map(|s| &s.title)),
        text(&item.artist, known.map(|s| &s.artist)),
    ) else {
        return Checked::Rejected(RejectReason::Invalid);
    };
    Checked::Ready(NewListen {
        client_listen_id: id.to_owned(),
        song: song.filter(|id| songs.contains_key(id)),
        title,
        artist,
        album: text(&item.album, known.and_then(|s| s.album.as_ref())),
        started_at,
        played_ms: item.played_ms,
        duration_ms: item.duration_ms.or_else(|| known.map(|s| s.duration_ms)),
        source: ListenSource::Native,
    })
}

pub fn listen_result(outcome: &ListenOutcome) -> ListenResult {
    let (status, reason) = match outcome.status {
        ListenStatus::Accepted => ("accepted", None),
        ListenStatus::Duplicate => ("duplicate", None),
        ListenStatus::Rejected(RejectReason::Future) => ("rejected", Some("future")),
        ListenStatus::Rejected(RejectReason::Invalid) => ("rejected", Some("invalid")),
    };
    ListenResult {
        client_listen_id: outcome.client_listen_id.clone(),
        status: status.to_owned(),
        reason: reason.map(str::to_owned),
    }
}

async fn submit_listens(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Json(batch): Json<ListenBatch>,
) -> ApiResult<Response> {
    if batch.listens.len() > MAX_LISTENS {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "At most 1000 listens per batch.",
        ));
    }
    let now = state.db.clock().now();
    let client_time = batch.client_time.map_or(now, |t| t.0);
    let ids: Vec<SongId> = batch
        .listens
        .iter()
        .filter_map(|l| l.song_id.as_deref().and_then(|s| SongId::parse(s).ok()))
        .collect();
    let access = principal.access.clone();
    let known: HashMap<SongId, Song> = read(&state, move |c| songs::get_many(c, &access, &ids))
        .await?
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect();
    let checked: Vec<(String, Checked)> = batch
        .listens
        .iter()
        .map(|item| {
            (
                item.client_listen_id.clone(),
                check_listen(item, client_time, &known),
            )
        })
        .collect();
    let ready: Vec<NewListen> = checked
        .iter()
        .filter_map(|(_, c)| match c {
            Checked::Ready(listen) => Some(listen.clone()),
            Checked::Rejected(_) => None,
        })
        .collect();
    let user = principal.access.user;
    let device = principal.device.as_ref().map(|(key, _)| *key);
    let stored = if ready.is_empty() {
        Vec::new()
    } else {
        write(&state, move |tx| listens::submit(tx, user, device, &ready)).await?
    };
    let mut stored = stored.into_iter();
    let results = checked
        .into_iter()
        .map(|(id, check)| match check {
            Checked::Rejected(reason) => listen_result(&ListenOutcome {
                client_listen_id: id,
                status: ListenStatus::Rejected(reason),
            }),
            Checked::Ready(_) => stored.next().map_or_else(
                || ListenResult {
                    client_listen_id: id.clone(),
                    status: String::from("rejected"),
                    reason: Some(String::from("invalid")),
                },
                |outcome| listen_result(&outcome),
            ),
        })
        .collect();
    Ok(json_with(StatusCode::OK, &ListenResults { results }))
}

async fn now_playing(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Json(body): Json<NowPlaying>,
) -> ApiResult<Response> {
    let song: SongId = body_id(&body.song_id, "song_id")?;
    let shown = match body.state.as_str() {
        "playing" => Some(PlayState::Playing),
        "paused" => Some(PlayState::Paused),
        "stopped" => None,
        _ => {
            return Err(ApiError::bad_field(
                "state",
                "state is playing, paused or stopped.",
            ));
        }
    };
    let registry = state.local.now_playing_registry();
    let device = principal.device.map(|(key, _)| key);
    let client = String::from(NOW_PLAYING_CLIENT);
    let Some(shown) = shown else {
        registry.stop(principal.access.user, device, &client);
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let access = principal.access.clone();
    let probe = song.clone();
    let found = read(&state, move |c| songs::get(c, &access, &probe)).await?;
    let Some(found) = found else {
        return Err(ApiError::not_found());
    };
    registry.report(
        NowPlayingReport {
            user: principal.access.user,
            username: principal.username.clone(),
            device,
            client,
            song,
            position_ms: body.position_ms,
            state: shown,
            duration_ms: found.duration_ms,
        },
        state.db.clock().now(),
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The client name native players show under in Subsonic's `getNowPlaying`.
const NOW_PLAYING_CLIENT: &str = "Tracks";

/// The item a path names, if the caller can see it.
async fn visible_item(
    state: &ApiState,
    access: &Access,
    kind: &str,
    id: &str,
) -> ApiResult<ItemRef> {
    let item = map::parse_item(kind, id).ok_or_else(ApiError::not_found)?;
    let access = access.clone();
    let probe = item.clone();
    let visible = read(state, move |c| {
        Ok(match &probe {
            ItemRef::Song(id) => songs::get(c, &access, id)?.is_some(),
            ItemRef::Album(id) => albums::get(c, &access, id)?.is_some(),
            ItemRef::Artist(id) => artists::get(c, &access, id)?.is_some(),
        })
    })
    .await?;
    if visible {
        Ok(item)
    } else {
        Err(ApiError::not_found())
    }
}

async fn star(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path((kind, id)): Path<(String, String)>,
    OptionalJson(body): OptionalJson<StarBody>,
) -> ApiResult<Response> {
    let item = visible_item(&state, &principal.access, &kind, &id).await?;
    let at = body.at.map_or_else(|| state.db.clock().now(), |t| t.0);
    let user = principal.access.user;
    write(&state, move |tx| stars::set(tx, user, &item, at)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn unstar(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let item = map::parse_item(&kind, &id).ok_or_else(ApiError::not_found)?;
    let user = principal.access.user;
    write(&state, move |tx| stars::clear(tx, user, &item)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn rate(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path((kind, id)): Path<(String, String)>,
    Json(body): Json<RatingBody>,
) -> ApiResult<Response> {
    if !(1..=5).contains(&body.rating) {
        return Err(ApiError::bad_field("rating", "rating is 1 to 5."));
    }
    let item = visible_item(&state, &principal.access, &kind, &id).await?;
    let user = principal.access.user;
    write(&state, move |tx| ratings::set(tx, user, &item, body.rating)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn unrate(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path((kind, id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let item = map::parse_item(&kind, &id).ok_or_else(ApiError::not_found)?;
    let user = principal.access.user;
    write(&state, move |tx| ratings::clear(tx, user, &item)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
struct StarsQuery {
    kind: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

async fn list_stars(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Query(query): Query<StarsQuery>,
) -> ApiResult<Response> {
    let kind = ItemKind::parse(query.kind.as_deref().unwrap_or("song"))
        .ok_or_else(|| ApiError::bad_field("kind", "kind is song, album or artist."))?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let access = principal.access.clone();
    let cursor = query.cursor;
    let (page, items) = read(&state, move |c| {
        let page = stars::list(c, &access, kind, cursor.as_deref(), limit)?;
        let mut song_ids = Vec::new();
        let mut album_ids = Vec::new();
        let mut artist_ids = Vec::new();
        for star in &page.items {
            match &star.item {
                ItemRef::Song(id) => song_ids.push(id.clone()),
                ItemRef::Album(id) => album_ids.push(id.clone()),
                ItemRef::Artist(id) => artist_ids.push(id.clone()),
            }
        }
        let items = (
            songs::get_many(c, &access, &song_ids)?,
            albums::get_many(c, &access, &album_ids)?,
            artists::get_many(c, &access, &artist_ids)?,
        );
        Ok((page.next, items))
    })
    .await?;
    let (found_songs, found_albums, found_artists) = items;
    let mut out: Vec<StarredItem> = songs_with_me(&state, &principal.access, found_songs)
        .await?
        .into_iter()
        .map(|s| StarredItem::Song(Box::new(s)))
        .collect();
    out.extend(
        found_albums
            .iter()
            .map(|a| StarredItem::Album(Box::new(map::album(a)))),
    );
    out.extend(
        found_artists
            .iter()
            .map(|a| StarredItem::Artist(Box::new(map::artist(a)))),
    );
    Ok(json_with(
        StatusCode::OK,
        &Starred {
            items: out,
            next_cursor: page,
        },
    ))
}

fn song_ids(raw: &[String], field: &str) -> ApiResult<Vec<SongId>> {
    raw.iter().map(|s| body_id::<SongId>(s, field)).collect()
}

fn playlist_etag(revision: i64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\"")).unwrap_or(HeaderValue::from_static("\"0\""))
}

fn with_etag(mut response: Response, revision: i64) -> Response {
    response
        .headers_mut()
        .insert(header::ETAG, playlist_etag(revision));
    response
}

async fn list_playlists(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let access = principal.access.clone();
    let list = read(&state, move |c| playlists::list(c, &access)).await?;
    Ok(json_with(
        StatusCode::OK,
        &Playlists {
            playlists: list.iter().map(map::playlist).collect(),
        },
    ))
}

async fn create_playlist(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let request: crate::wire::social::NewPlaylist = parse_json(&body)?;
    let user = principal.access.user;
    let state_ref = &state;
    idempotency::once(
        &state,
        user,
        &headers,
        &method,
        uri.path(),
        &body,
        || async move {
            let name = request.name.trim().to_owned();
            if name.is_empty() || name.chars().count() > 200 {
                return Err(ApiError::bad_field(
                    "name",
                    "name must be 1 to 200 characters.",
                ));
            }
            let new = NewPlaylist {
                name,
                comment: request.comment,
                is_public: request.is_public.unwrap_or(false),
                entries: song_ids(request.entries.as_deref().unwrap_or_default(), "entries")?,
            };
            let created = write(state_ref, move |tx| playlists::create(tx, user, &new)).await?;
            Ok((
                StatusCode::CREATED,
                serde_json::to_value(map::playlist(&created))?,
            ))
        },
    )
    .await
}

async fn get_playlist(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: PlaylistId = path_id(&id)?;
    let access = principal.access.clone();
    let (playlist, entries) = read(&state, move |c| playlists::get(c, &access, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    let ids: Vec<SongId> = entries.iter().map(|e| e.song.clone()).collect();
    let access = principal.access.clone();
    let found = read(&state, move |c| songs::get_many(c, &access, &ids)).await?;
    let with_me = songs_with_me(&state, &principal.access, found).await?;
    let by_id: HashMap<String, crate::wire::Song> =
        with_me.into_iter().map(|s| (s.id.clone(), s)).collect();
    let body = PlaylistPage {
        playlist: map::playlist(&playlist),
        entries: entries
            .iter()
            .map(|e| PlaylistEntry {
                position: e.position,
                song: by_id.get(e.song.as_str()).cloned(),
                song_id: e.song.to_string(),
                added_at: e.added_at.into(),
            })
            .collect(),
    };
    Ok(with_etag(
        json_with(StatusCode::OK, &body),
        playlist.revision,
    ))
}

async fn patch_playlist(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<crate::wire::social::PlaylistPatch>,
) -> ApiResult<Response> {
    let id: PlaylistId = path_id(&id)?;
    let patch = PlaylistPatch {
        name: body
            .name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty()),
        comment: body.comment,
        is_public: body.is_public,
    };
    let user = principal.access.user;
    let updated = write(&state, move |tx| playlists::update(tx, user, &id, &patch)).await?;
    Ok(with_etag(
        json_with(StatusCode::OK, &map::playlist(&updated)),
        updated.revision,
    ))
}

/// The revision in `If-Match: "12"`, or `None` when absent. A malformed one never matches.
pub fn if_match(headers: &HeaderMap) -> ApiResult<Option<i64>> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("W/");
    text.trim_matches('"').parse().map(Some).map_err(|_| {
        ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "precondition_failed",
            "If-Match does not name a revision.",
        )
    })
}

async fn replace_entries(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ReplaceEntries>,
) -> ApiResult<Response> {
    let id: PlaylistId = path_id(&id)?;
    let revision = if_match(&headers)?;
    let entries = song_ids(&body.entries, "entries")?;
    let user = principal.access.user;
    let updated = write(&state, move |tx| {
        playlists::replace_entries(tx, user, &id, revision, &entries)
    })
    .await?;
    Ok(with_etag(
        json_with(StatusCode::OK, &map::playlist(&updated)),
        updated.revision,
    ))
}

async fn append_entries(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(body): Json<AppendEntries>,
) -> ApiResult<Response> {
    let id: PlaylistId = path_id(&id)?;
    let entries = song_ids(&body.append, "append")?;
    let user = principal.access.user;
    let updated = write(&state, move |tx| playlists::append(tx, user, &id, &entries)).await?;
    Ok(with_etag(
        json_with(StatusCode::OK, &map::playlist(&updated)),
        updated.revision,
    ))
}

async fn delete_playlist(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: PlaylistId = path_id(&id)?;
    let user = principal.access.user;
    write(&state, move |tx| playlists::delete(tx, user, &id)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_queue(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let user = principal.access.user;
    let queue = read(&state, move |c| play_queue::get(c, user)).await?;
    Ok(match queue {
        Some(queue) if !queue.entries.is_empty() => {
            json_with(StatusCode::OK, &map::play_queue(&queue))
        }
        _ => StatusCode::NO_CONTENT.into_response(),
    })
}

const MAX_QUEUE: usize = 2000;

async fn put_queue(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Json(body): Json<crate::wire::social::PlayQueueUpdate>,
) -> ApiResult<Response> {
    if body.entries.len() > MAX_QUEUE {
        return Err(ApiError::bad_field("entries", "At most 2000 entries."));
    }
    let update = PlayQueueUpdate {
        entries: song_ids(&body.entries, "entries")?,
        current: body.current,
        position_ms: body.position_ms,
        changed_by: body
            .changed_by
            .map(|c| c.trim().chars().take(100).collect::<String>())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| String::from("Trackside")),
        device: principal.device.as_ref().map(|(key, _)| *key),
    };
    let user = principal.access.user;
    let stored = write(&state, move |tx| play_queue::put(tx, user, &update)).await?;
    if stored.entries.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(json_with(StatusCode::OK, &map::play_queue(&stored)))
}

fn bookmark(b: &bookmarks::Bookmark) -> Bookmark {
    Bookmark {
        song_id: b.song.to_string(),
        position_ms: b.position_ms,
        comment: b.comment.clone(),
        created_at: b.created_at.into(),
        updated_at: b.updated_at.into(),
    }
}

async fn list_bookmarks(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let user = principal.access.user;
    let list = read(&state, move |c| bookmarks::list(c, user)).await?;
    Ok(json_with(
        StatusCode::OK,
        &Bookmarks {
            bookmarks: list.iter().map(bookmark).collect(),
        },
    ))
}

async fn put_bookmark(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(song): Path<String>,
    Json(body): Json<BookmarkBody>,
) -> ApiResult<Response> {
    let song: SongId = path_id(&song)?;
    let access = principal.access.clone();
    let probe = song.clone();
    if read(&state, move |c| songs::get(c, &access, &probe))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found());
    }
    let user = principal.access.user;
    let stored = write(&state, move |tx| {
        bookmarks::put(tx, user, &song, body.position_ms, body.comment.as_deref())
    })
    .await?;
    Ok(json_with(StatusCode::OK, &bookmark(&stored)))
}

async fn delete_bookmark(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(song): Path<String>,
) -> ApiResult<Response> {
    let song: SongId = path_id(&song)?;
    let user = principal.access.user;
    match write(&state, move |tx| bookmarks::delete(tx, user, &song)).await {
        Ok(())
        | Err(ApiError {
            code: "not_found", ..
        }) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(started_at: &str) -> ListenItem {
        ListenItem {
            client_listen_id: String::from("6F1D9C2A"),
            song_id: None,
            started_at: started_at.to_owned(),
            played_ms: Some(254_000),
            title: Some(String::from("Halo")),
            artist: Some(String::from("Beyoncé")),
            album: None,
            duration_ms: Some(261_640),
        }
    }

    const CLIENT: Millis = Millis(1_794_648_600_000);

    #[test]
    fn a_listen_with_text_is_ready_without_a_song() {
        let checked = check_listen(&item("2026-11-13T21:10:02.000Z"), CLIENT, &HashMap::new());
        let Checked::Ready(listen) = checked else {
            panic!("expected ready, got {checked:?}");
        };
        assert_eq!(listen.song, None);
        assert_eq!(listen.title, "Halo");
        assert_eq!(listen.source, ListenSource::Native);
    }

    #[test]
    fn a_listen_far_after_the_client_clock_is_future() {
        assert_eq!(
            check_listen(&item("2026-11-16T09:30:00.000Z"), CLIENT, &HashMap::new()),
            Checked::Rejected(RejectReason::Future)
        );
    }

    #[test]
    fn malformed_listens_are_invalid() {
        assert_eq!(
            check_listen(&item("yesterday"), CLIENT, &HashMap::new()),
            Checked::Rejected(RejectReason::Invalid)
        );
        let mut nameless = item("2026-11-13T21:10:02.000Z");
        nameless.title = None;
        assert_eq!(
            check_listen(&nameless, CLIENT, &HashMap::new()),
            Checked::Rejected(RejectReason::Invalid)
        );
        let mut bad_id = item("2026-11-13T21:10:02.000Z");
        bad_id.song_id = Some(String::from("not-an-id"));
        assert_eq!(
            check_listen(&bad_id, CLIENT, &HashMap::new()),
            Checked::Rejected(RejectReason::Invalid)
        );
    }

    #[test]
    fn if_match_reads_quoted_revisions() -> Result<(), ApiError> {
        let mut headers = HeaderMap::new();
        assert_eq!(if_match(&headers)?, None);
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"12\""));
        assert_eq!(if_match(&headers)?, Some(12));
        headers.insert(header::IF_MATCH, HeaderValue::from_static("nope"));
        assert!(if_match(&headers).is_err());
        Ok(())
    }
}
