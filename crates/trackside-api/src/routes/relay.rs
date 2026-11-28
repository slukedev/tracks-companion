//! The phone playback relay (api.md section 22, contract section 3): a device's session, and
//! its live HLS stream under the device's media token. Controls are the speaker routes with
//! the session's `speaker_id`.
//!
//! Audio is real-time only: playlists and segments are `no-store` and come from memory.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use trackside_auth::extract::{Authed, MediaAuthed};
use trackside_auth::{Principal, Scopes, Via};
use trackside_core::assistant::SpeakerCommand;
use trackside_core::relay::{
    RelayBody, RelayCaller, RelayControl, RelayOpen, RelayPart, RelayTier, RelayView,
};
use trackside_core::repo::devices;
use trackside_core::{PublicId, RelaySessionId};

use super::{idempotency, read};
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{json_with, parse_json};
use crate::wire::Time;
use crate::wire::relay as wire;

const LIBRARY: u8 = Scopes::LIBRARY.bits();
const STREAM: u8 = Scopes::STREAM.bits();

/// The longest name Music Assistant is given for a phone.
const MAX_NAME: usize = 64;

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/relay/sessions", post(open).get(list))
        .route("/relay/sessions/{id}", get(one).patch(patch).delete(close))
        .route("/relay/{id}/{generation}/{file}", get(fetch))
}

/// A session as its owner sees it. `base_path` is the server's (empty unless it runs under a
/// path prefix).
pub fn session_body(view: &RelayView, base_path: &str) -> wire::RelaySession {
    wire::RelaySession {
        created_at: Time(view.created_at),
        detail: view.detail.clone(),
        device_id: view.device_id.to_string(),
        ended_reason: view.ended_reason.map(str::to_owned),
        fetch_timeout_s: view.fetch_timeout_s,
        generation: view.generation,
        id: view.id.to_string(),
        idle_timeout_s: view.idle_timeout_s,
        latency_ms: view.latency_ms,
        speaker_id: view.speaker_id.to_string(),
        state: view.state.as_str().to_owned(),
        stream: view.hls.as_ref().map(|hls| wire::RelayStream {
            format: wire::RelayFormat {
                bit_depth: view.format.bit_depth,
                bitrate_kbps: view.format.bitrate_kbps,
                channels: view.format.channels,
                codec: view.format.codec.to_owned(),
                container: view.format.container.to_owned(),
                sample_rate: view.format.sample_rate,
            },
            hls: format!("{base_path}/api/v1{hls}"),
        }),
        tier: view.tier.as_str().to_owned(),
    }
}

fn relay(state: &ApiState) -> ApiResult<&dyn RelayControl> {
    if !state.assistant.enabled() {
        return Err(ApiError::not_found());
    }
    state.assistant.relay().ok_or_else(ApiError::not_found)
}

fn tier_of(raw: Option<&str>) -> ApiResult<RelayTier> {
    match raw {
        None => Ok(RelayTier::Lossless),
        Some(raw) => RelayTier::parse(raw)
            .ok_or_else(|| ApiError::bad_field("tier", "tier is lossless or cellular.")),
    }
}

/// The calling app device, when it may relay at `tier` (contract 3.7 and 5.3): a device key
/// with `stream`, a user who may play services, and a server that can relay.
pub(crate) async fn caller(
    state: &ApiState,
    principal: &Principal,
    tier: RelayTier,
    name: Option<String>,
) -> ApiResult<RelayCaller> {
    let control = relay(state)?;
    let Some((device, device_id)) = principal.device.clone() else {
        return Err(ApiError::forbidden());
    };
    if principal.via != Via::DeviceKey || !principal.permissions.can_play_services {
        return Err(ApiError::forbidden());
    }
    if !control.supported() {
        return Err(trackside_core::relay::RelayError::Unsupported("relay").into());
    }
    if tier == RelayTier::Cellular && !control.cellular() {
        return Err(trackside_core::relay::RelayError::Unsupported("relay_cellular").into());
    }
    let name = match name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
        Some(name) if name.len() > MAX_NAME * 4 => {
            return Err(ApiError::bad_field("name", "name is too long."));
        }
        Some(name) => name,
        None => read(state, move |c| devices::get(c, device))
            .await?
            .map_or_else(|| String::from("Tracks"), |d| d.name),
    };
    Ok(RelayCaller {
        user: principal.access.user,
        device,
        device_id,
        name,
    })
}

fn session_id(raw: &str) -> ApiResult<RelaySessionId> {
    RelaySessionId::parse(raw).map_err(|_| ApiError::not_found())
}

async fn open(
    Authed(principal): Authed<STREAM>,
    State(state): State<ApiState>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let request: wire::OpenRelay = if body.iter().all(u8::is_ascii_whitespace) {
        wire::OpenRelay::default()
    } else {
        parse_json(&body)?
    };
    let tier = tier_of(request.tier.as_deref())?;
    let play = request
        .play
        .as_ref()
        .map(|p| {
            super::speakers::play_from_wire(p.clone(), principal.permissions.can_play_services)
        })
        .transpose()?;
    let caller = caller(&state, &principal, tier, request.name).await?;
    state.auth.relay_change(&principal)?;
    if play.is_some() {
        state.auth.speaker_command(&principal, true)?;
    }
    let access = principal.access.clone();
    let base = state.server.base_path.clone();
    let state = &state;
    idempotency::once_in_memory(
        state,
        access.user,
        &headers,
        &method,
        uri.path(),
        &body,
        || async move {
            let control = relay(state)?;
            let opened = control.open(&caller, RelayOpen { tier }).await?;
            if let Some(play) = play {
                state
                    .assistant
                    .command(
                        &access,
                        &opened.session.speaker_id,
                        SpeakerCommand::Play(play),
                    )
                    .await?;
            }
            let view = control
                .session(caller.user, &opened.session.id)
                .unwrap_or(opened.session);
            let status = if opened.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            Ok((status, serde_json::to_value(session_body(&view, &base))?))
        },
    )
    .await
}

async fn list(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
) -> ApiResult<Response> {
    let control = relay(&state)?;
    let base = &state.server.base_path;
    let sessions = control
        .sessions(principal.access.user)
        .iter()
        .map(|v| session_body(v, base))
        .collect();
    Ok(json_with(StatusCode::OK, &wire::RelaySessions { sessions }))
}

async fn one(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let control = relay(&state)?;
    let view = control
        .session(principal.access.user, &session_id(&id)?)
        .ok_or_else(ApiError::not_found)?;
    Ok(json_with(
        StatusCode::OK,
        &session_body(&view, &state.server.base_path),
    ))
}

async fn patch(
    Authed(principal): Authed<STREAM>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let id = session_id(&id)?;
    let request: wire::RelayPatch = parse_json(&body)?;
    let tier = tier_of(Some(&request.tier))?;
    let caller = caller(&state, &principal, tier, None).await?;
    state.auth.relay_change(&principal)?;
    let view = relay(&state)?.set_tier(&caller, &id, tier)?;
    Ok(json_with(
        StatusCode::OK,
        &session_body(&view, &state.server.base_path),
    ))
}

async fn close(
    Authed(principal): Authed<STREAM>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id = session_id(&id)?;
    relay(&state)?.close(principal.access.user, &id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `?mt=...` of the request, carried onto the URLs inside playlists.
fn media_token(uri: &http::Uri) -> String {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .find(|pair| pair.starts_with("mt="))
        .unwrap_or_default()
        .to_owned()
}

fn no_store(mut response: Response, content_type: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A playlist or segment, for the device that owns the session; anyone else gets `404`.
async fn fetch(
    MediaAuthed(principal): MediaAuthed,
    State(state): State<ApiState>,
    Path((id, generation, file)): Path<(String, String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let control = relay(&state)?;
    let Some((device, _)) = principal.device.as_ref() else {
        return Err(ApiError::not_found());
    };
    let id = session_id(&id)?;
    let generation: u32 = generation.parse().map_err(|_| ApiError::not_found())?;
    let part = RelayPart::parse(&file).ok_or_else(ApiError::not_found)?;
    let query = media_token(request.uri());
    let response = match control
        .fetch(*device, &id, generation, part, &query)
        .await?
    {
        RelayBody::Playlist(text) => {
            no_store(text.into_response(), "application/vnd.apple.mpegurl")
        }
        RelayBody::Media(bytes) => no_store(Body::from(bytes).into_response(), "audio/mp4"),
    };
    Ok(response)
}
