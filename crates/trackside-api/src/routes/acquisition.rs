//! Acquisition (api.md section 15). Every endpoint answers `404` unless `/me` would carry
//! `features.acquisition` for this caller, as if it did not exist.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use http::{HeaderMap, Method, StatusCode};
use serde::Deserialize;
use trackside_auth::extract::{Authed, MediaAuthed};
use trackside_auth::{Principal, Scopes};
use trackside_core::control::{
    AcquireRequest, AcquisitionFilter, AcquisitionState, AcquisitionStateFilter, ExternalPlay,
    ExternalPlayRequest, ExternalQuery, ExternalSearch, GrowState, InstantStream,
};
use trackside_core::{AcquisitionId, CandidateToken};
use trackside_media::{HlsPart, Priority};

use super::account::acquisition_feature;
use super::{api_url, idempotency};
use crate::error::{ApiError, ApiResult};
use crate::extract::{Json, Query, body_id, json_with, parse_json, path_id};
use crate::wire::acquisition as wire_acq;
use crate::wire::acquisition::{Acquisitions, Search, SearchRequest};
use crate::{ApiState, map};

const REQUEST: u8 = Scopes::REQUEST.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/acquisitions/search", post(search))
        .route("/acquisitions/searches/{id}", get(search_more))
        .route("/acquisitions", get(list).post(request))
        .route("/acquisitions/{id}", get(status).delete(cancel))
        .route("/acquisitions/{id}/stream", get(progressive))
        .route("/acquisitions/{id}/stream.m3u8", get(hls_playlist))
        .route("/acquisitions/{id}/{part}", get(hls_part))
        .route("/external/play", post(play_external))
        .route("/external/plays/{id}/stream", get(external_progressive))
        .route("/external/plays/{id}/stream.m3u8", get(external_playlist))
        .route("/external/plays/{id}/{part}", get(external_part))
}

fn available(state: &ApiState, principal: &Principal) -> ApiResult<()> {
    if acquisition_feature(state, principal).is_some() {
        Ok(())
    } else {
        Err(ApiError::not_found())
    }
}

/// The search's wire shape.
pub fn search_body(found: &ExternalSearch) -> Search {
    Search {
        search_id: found.search_id.clone(),
        complete: found.complete,
        in_library: found.in_library.iter().map(map::song).collect(),
        candidates: found.candidates.iter().map(map::candidate).collect(),
    }
}

/// Free text, or title and artist.
pub fn external_query(body: SearchRequest) -> ApiResult<ExternalQuery> {
    let trimmed = |v: Option<String>| v.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    if let Some(text) = trimmed(body.query) {
        return Ok(ExternalQuery::Text(text));
    }
    match (trimmed(body.title), trimmed(body.artist)) {
        (Some(title), Some(artist)) => Ok(ExternalQuery::Fields {
            title,
            artist,
            album: trimmed(body.album),
            duration_ms: body.duration_ms,
        }),
        _ => Err(ApiError::bad_field(
            "query",
            "Send query, or title and artist.",
        )),
    }
}

async fn search(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    Json(body): Json<SearchRequest>,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let query = external_query(body)?;
    let found = state.acquire.search(&principal.access, query).await?;
    Ok(json_with(StatusCode::OK, &search_body(&found)))
}

async fn search_more(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let found = state.acquire.search_more(&principal.access, &id).await?;
    Ok(json_with(StatusCode::OK, &search_body(&found)))
}

async fn request(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let wanted: crate::wire::acquisition::AcquireRequest = parse_json(&body)?;
    let user = principal.access.user;
    let state_ref = &state;
    let principal_ref = &principal;
    idempotency::once(
        &state,
        user,
        &headers,
        &method,
        uri.path(),
        &body,
        || async move {
            let title = wanted.title.trim().to_owned();
            let artist = wanted.artist.trim().to_owned();
            if title.is_empty() || artist.is_empty() {
                return Err(ApiError::bad_field(
                    "title",
                    "title and artist are required.",
                ));
            }
            let candidate = wanted
                .candidate
                .as_deref()
                .filter(|c| !c.is_empty())
                .map(|c| body_id::<CandidateToken>(c, "candidate"))
                .transpose()?;
            let view = state_ref
                .acquire
                .request(
                    &principal_ref.access,
                    AcquireRequest {
                        title,
                        artist,
                        album: wanted
                            .album
                            .map(|a| a.trim().to_owned())
                            .filter(|a| !a.is_empty()),
                        duration_ms: wanted.duration_ms,
                        candidate,
                        allow_versions: wanted.allow_versions,
                        instant: wanted.instant,
                    },
                )
                .await?;
            let status = if view.state == AcquisitionState::Done && view.existing {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            let body = map::acquisition(&view, &state_ref.server.base_path);
            Ok((status, serde_json::to_value(body)?))
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    state: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

pub fn state_filter(raw: Option<&str>) -> ApiResult<Option<AcquisitionStateFilter>> {
    Ok(match raw {
        None | Some("") => None,
        Some("active") => Some(AcquisitionStateFilter::Active),
        Some("finished") => Some(AcquisitionStateFilter::Finished),
        Some(other) => Some(AcquisitionStateFilter::Exactly(
            AcquisitionState::parse(other)
                .ok_or_else(|| ApiError::bad_field("state", "Unknown state."))?,
        )),
    })
}

async fn list(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let filter = AcquisitionFilter {
        user: Some(principal.access.user),
        state: state_filter(query.state.as_deref())?,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(100).clamp(1, 500),
    };
    let page = state.acquire.list(&principal.access, filter).await?;
    let base = &state.server.base_path;
    Ok(json_with(
        StatusCode::OK,
        &Acquisitions {
            acquisitions: page
                .items
                .iter()
                .map(|v| map::acquisition(v, base))
                .collect(),
            next_cursor: page.next,
        },
    ))
}

async fn status(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let id: AcquisitionId = path_id(&id)?;
    let view = state.acquire.get(&principal.access, &id).await?;
    Ok(json_with(
        StatusCode::OK,
        &map::acquisition(&view, &state.server.base_path),
    ))
}

async fn cancel(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let id: AcquisitionId = path_id(&id)?;
    state.acquire.cancel(&principal.access, &id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn media_available(state: &ApiState, principal: &Principal) -> ApiResult<()> {
    if principal.scopes.contains(Scopes::STREAM) {
        available(state, principal)
    } else {
        Err(ApiError::forbidden())
    }
}

async fn progressive(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    media_available(&state, &principal)?;
    let id: AcquisitionId = path_id(&id)?;
    let view = state.acquire.get(&principal.access, &id).await?;
    if let (AcquisitionState::Done, Some(song)) = (view.state, view.song.as_ref()) {
        let (parts, _) = request.into_parts();
        let query = parts
            .uri
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let target = api_url(&state, &format!("/songs/{song}/stream{query}"));
        return Ok(Redirect::permanent(&target).into_response());
    }
    let instant = state.acquire.instant_stream(&principal.access, &id)?;
    let (parts, _) = request.into_parts();
    let growing = &instant.progressive;
    if matches!(*growing.state.borrow(), GrowState::Failed) {
        return Err(ApiError::not_found());
    }
    let slot = state.auth.stream_slot(&principal)?;
    let response = state
        .media
        .serve_growing(growing, &parts, instant.content_type)
        .await?;
    Ok(trackside_media::hold(response, slot))
}

async fn hls_job(
    state: &ApiState,
    principal: &Principal,
    id: &AcquisitionId,
) -> ApiResult<trackside_media::HlsJobId> {
    let instant = state.acquire.instant_stream(&principal.access, id)?;
    if let Some(job) = instant.hls_job {
        return Ok(job);
    }
    // Held while a job starts, so a playlist and its parts asked at once start one.
    let mut started = state.local.instant_hls().lock().await;
    started.retain(|_, job| state.media.hls_job(job).is_some());
    if let Some(job) = started.get(id) {
        return Ok(job.clone());
    }
    let duration_ms = instant.duration_ms.ok_or_else(ApiError::not_found)?;
    let profile = cellular_profile(state);
    let job = state
        .media
        .start_hls(
            trackside_media::HlsInput::Growing {
                file: instant.progressive,
                duration_ms,
            },
            profile,
            Priority::Interactive,
            super::media::hls_owner(state, principal)?,
        )
        .await?;
    started.insert(id.clone(), job.clone());
    Ok(job)
}

async fn hls_playlist(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    media_available(&state, &principal)?;
    let id: AcquisitionId = path_id(&id)?;
    let job = hls_job(&state, &principal, &id).await?;
    playlist_response(&state, &job, request).await
}

async fn playlist_response(
    state: &ApiState,
    job: &trackside_media::HlsJobId,
    request: Request,
) -> ApiResult<Response> {
    let (parts, _) = request.into_parts();
    let suffix = parts
        .uri
        .query()
        .unwrap_or_default()
        .split('&')
        .find(|pair| pair.starts_with("mt="))
        .map(|pair| format!("?{pair}"))
        .unwrap_or_default();
    let text = state.media.hls_playlist(job, &suffix).await?;
    let mut response = text.into_response();
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    Ok(response)
}

async fn hls_part(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path((id, part)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    media_available(&state, &principal)?;
    let id: AcquisitionId = path_id(&id)?;
    let part: HlsPart = super::media::hls_part_name(&part).ok_or_else(ApiError::not_found)?;
    let job = hls_job(&state, &principal, &id).await?;
    let (parts, _) = request.into_parts();
    Ok(state.media.hls_part(&job, part, &parts).await?)
}

/// The wire shape of a play without keeping. Stream URLs get the base path in front.
pub fn external_play_body(play: &ExternalPlay, base_path: &str) -> wire_acq::ExternalPlay {
    wire_acq::ExternalPlay {
        title: play.title.clone(),
        artist: play.artist.clone(),
        duration_ms: play.duration_ms,
        song_id: play.song.as_ref().map(ToString::to_string),
        stream: play.token.as_ref().map(|token| wire_acq::Stream {
            hls: format!("{base_path}/api/v1/external/plays/{token}/stream.m3u8"),
            progressive: format!("{base_path}/api/v1/external/plays/{token}/stream"),
        }),
    }
}

async fn play_external(
    Authed(principal): Authed<REQUEST>,
    State(state): State<ApiState>,
    Json(body): Json<wire_acq::ExternalPlayRequest>,
) -> ApiResult<Response> {
    available(&state, &principal)?;
    let trimmed = |v: Option<String>| v.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    let request = if let Some(token) = trimmed(body.candidate) {
        ExternalPlayRequest::Candidate(body_id::<CandidateToken>(&token, "candidate")?)
    } else {
        match (trimmed(body.title), trimmed(body.artist)) {
            (Some(title), Some(artist)) => ExternalPlayRequest::Song {
                title,
                artist,
                album: trimmed(body.album),
                duration_ms: body.duration_ms,
            },
            _ => {
                return Err(ApiError::bad_field(
                    "candidate",
                    "Send candidate, or title and artist.",
                ));
            }
        }
    };
    let play = state
        .acquire
        .play_external(&principal.access, request)
        .await?;
    Ok(json_with(
        StatusCode::OK,
        &external_play_body(&play, &state.server.base_path),
    ))
}

fn external_stream(
    state: &ApiState,
    principal: &Principal,
    id: &str,
) -> ApiResult<(CandidateToken, InstantStream)> {
    media_available(state, principal)?;
    let token: CandidateToken = path_id(id)?;
    let stream = state.acquire.external_stream(&principal.access, &token)?;
    Ok((token, stream))
}

async fn external_progressive(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let (_, stream) = external_stream(&state, &principal, &id)?;
    let (parts, _) = request.into_parts();
    if matches!(*stream.progressive.state.borrow(), GrowState::Failed) {
        return Err(ApiError::not_found());
    }
    let slot = state.auth.stream_slot(&principal)?;
    let response = state
        .media
        .serve_growing(&stream.progressive, &parts, stream.content_type)
        .await?;
    Ok(trackside_media::hold(response, slot))
}

async fn external_hls_job(
    state: &ApiState,
    principal: &Principal,
    id: &str,
) -> ApiResult<trackside_media::HlsJobId> {
    let (token, stream) = external_stream(state, principal, id)?;
    let key = (principal.access.user, token);
    let mut started = state.local.external_hls().lock().await;
    started.retain(|_, job| state.media.hls_job(job).is_some());
    if let Some(job) = started.get(&key) {
        return Ok(job.clone());
    }
    let duration_ms = stream.duration_ms.ok_or_else(ApiError::not_found)?;
    let job = state
        .media
        .start_hls(
            trackside_media::HlsInput::Growing {
                file: stream.progressive,
                duration_ms,
            },
            cellular_profile(state),
            Priority::Interactive,
            super::media::hls_owner(state, principal)?,
        )
        .await?;
    started.insert(key, job.clone());
    Ok(job)
}

fn cellular_profile(state: &ApiState) -> trackside_media::Profile {
    trackside_media::Profile::parse(&state.config.media.cellular_profile)
        .unwrap_or(trackside_media::Profile::Aac256)
}

async fn external_playlist(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let job = external_hls_job(&state, &principal, &id).await?;
    playlist_response(&state, &job, request).await
}

async fn external_part(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path((id, part)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let part: HlsPart = super::media::hls_part_name(&part).ok_or_else(ApiError::not_found)?;
    let job = external_hls_job(&state, &principal, &id).await?;
    let (parts, _) = request.into_parts();
    Ok(state.media.hls_part(&job, part, &parts).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_need_text_or_title_and_artist() -> Result<(), ApiError> {
        let text = external_query(SearchRequest {
            query: Some(String::from(" beyonce halo ")),
            ..SearchRequest::default()
        })?;
        assert_eq!(text, ExternalQuery::Text(String::from("beyonce halo")));
        let fields = external_query(SearchRequest {
            title: Some(String::from("Halo")),
            artist: Some(String::from("Beyoncé")),
            ..SearchRequest::default()
        })?;
        assert!(matches!(fields, ExternalQuery::Fields { .. }));
        assert!(
            external_query(SearchRequest {
                title: Some(String::from("Halo")),
                ..SearchRequest::default()
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn state_filters_parse() -> Result<(), ApiError> {
        assert_eq!(
            state_filter(Some("active"))?,
            Some(AcquisitionStateFilter::Active)
        );
        assert_eq!(
            state_filter(Some("failed"))?,
            Some(AcquisitionStateFilter::Exactly(AcquisitionState::Failed))
        );
        assert!(state_filter(Some("sleeping")).is_err());
        Ok(())
    }
}
