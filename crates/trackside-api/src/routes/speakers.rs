//! Speakers (api.md section 20, contract section 1): Music Assistant players in Devices, their
//! controls and queue. The server holds the Music Assistant connection; clients only talk to
//! Trackside.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post, put};
use http::{HeaderMap, Method, StatusCode};
use serde::Deserialize;
use trackside_auth::extract::Authed;
use trackside_auth::{Principal, Scopes};
use trackside_core::assistant::{
    CommandAccepted, MAX_PLAY_ITEMS, MediaInfo, PlayContext, PlayItem, PlayMode, PlayRequest,
    PlaybackAction, QueuePage, Repeat, Speaker, SpeakerCommand, SpeakerError, SpeakerEvent,
    SpeakerList, SpeakerPatch, SpeakerQueue, SpeakersState,
};
use trackside_core::relay::{RelayOpen, RelayTier};
use trackside_core::{Access, DeviceKey, SongId, SpeakerId};

use super::{cached_json, idempotency};
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{Query, body_id, json_with, parse_json, path_id};
use crate::wire::Time;
use crate::wire::speakers as wire;

const LIBRARY: u8 = Scopes::LIBRARY.bits();
const WRITE: u8 = Scopes::WRITE.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/speakers", get(list))
        .route("/speakers/{id}", get(one).patch(patch))
        .route("/speakers/{id}/queue", get(queue))
        .route(
            "/speakers/{id}/queue/{item_id}",
            axum::routing::delete(remove_item),
        )
        .route("/speakers/{id}/playback", post(playback))
        .route("/speakers/{id}/members", put(members))
        .route("/speakers/{id}/group", axum::routing::delete(leave_group))
        .route("/speakers/{id}/play", post(play))
        .route("/speakers/{id}/transfer", post(transfer))
}

/// What a caller may know about one item: a library song only when they see its library.
fn media_fields(media: &MediaInfo, access: &Access) -> MediaFields {
    let song = media.song.as_ref().filter(|s| access.can_see(s.library));
    MediaFields {
        song_id: song.map(|s| s.id.to_string()),
        artwork_id: song
            .and_then(|s| s.artwork.as_ref().map(ToString::to_string))
            .or_else(|| {
                media
                    .catalog
                    .as_ref()
                    .and_then(|c| c.artwork.as_ref().map(ToString::to_string))
            }),
        reference: media.catalog.as_ref().map(|c| c.reference.clone()),
        service_id: media.catalog.as_ref().map(|c| c.service.to_string()),
    }
}

struct MediaFields {
    song_id: Option<String>,
    artwork_id: Option<String>,
    reference: Option<String>,
    service_id: Option<String>,
}

fn context_body(context: &PlayContext) -> wire::PlayContext {
    wire::PlayContext {
        id: context.id.clone(),
        kind: context.kind.clone(),
        name: context.name.clone(),
    }
}

/// A speaker as `access` may see it.
pub fn speaker_body(speaker: &Speaker, access: &Access) -> wire::Speaker {
    speaker_body_on(speaker, access, None)
}

/// A speaker as `access` sees it from `device`: a relay speaker says whether it is this one.
pub fn speaker_body_on(
    speaker: &Speaker,
    access: &Access,
    device: Option<DeviceKey>,
) -> wire::Speaker {
    wire::Speaker {
        availability: speaker.availability.as_str().to_owned(),
        can_group_with: speaker
            .can_group_with
            .iter()
            .map(ToString::to_string)
            .collect(),
        capabilities: speaker
            .capabilities
            .iter()
            .map(|c| c.as_str().to_owned())
            .collect(),
        context: speaker.context.as_ref().map(context_body),
        group: speaker.group.as_ref().map(|g| wire::SpeakerGroup {
            leader_id: g.leader_id.to_string(),
            member_ids: g.member_ids.iter().map(ToString::to_string).collect(),
            role: g.role.as_str().to_owned(),
            fixed: g.fixed,
        }),
        group_volume: speaker.group_volume,
        id: speaker.id.to_string(),
        kind: speaker.kind.as_str().to_owned(),
        manufacturer: speaker.manufacturer.clone(),
        model: speaker.model.clone(),
        muted: speaker.muted,
        name: speaker.name.clone(),
        now_playing: speaker.now_playing.as_ref().map(|n| {
            let fields = media_fields(&n.media, access);
            wire::NowPlaying {
                album: n.media.album.clone(),
                artist: n.media.artist.clone(),
                artwork_id: fields.artwork_id,
                duration_ms: n.media.duration_ms,
                item_id: n.item_id.clone(),
                origin: n.origin.as_str().to_owned(),
                reference: fields.reference,
                service_id: fields.service_id,
                song_id: fields.song_id,
                song_key: n.media.song_key.clone(),
                title: n.media.title.clone(),
            }
        }),
        playback: speaker.playback.as_ref().map(|p| wire::Playback {
            position_at: Time(p.position_at),
            position_ms: p.position_ms,
            repeat: p.repeat.as_str().to_owned(),
            shuffle: p.shuffle,
            state: p.state.as_str().to_owned(),
        }),
        powered: speaker.powered,
        protocol: speaker.protocol.clone(),
        relay: speaker.relay.as_ref().map(|r| wire::SpeakerRelay {
            device_id: r.device_id.to_string(),
            session_id: r.session.to_string(),
            this_device: device == Some(r.device),
        }),
        revision: speaker.revision,
        volume: speaker.volume,
    }
}

pub fn speakers_body(list: &SpeakerList, access: &Access) -> wire::Speakers {
    speakers_body_on(list, access, None)
}

pub fn speakers_body_on(
    list: &SpeakerList,
    access: &Access,
    device: Option<DeviceKey>,
) -> wire::Speakers {
    let state = list.state.unwrap_or(SpeakersState::Unavailable);
    wire::Speakers {
        since: list.since.map(Time),
        speakers: list
            .speakers
            .iter()
            .map(|s| speaker_body_on(s, access, device))
            .collect(),
        state: state.as_str().to_owned(),
    }
}

pub fn command_body(accepted: &CommandAccepted, access: &Access) -> wire::CommandAccepted {
    command_body_on(accepted, access, None)
}

pub fn command_body_on(
    accepted: &CommandAccepted,
    access: &Access,
    device: Option<DeviceKey>,
) -> wire::CommandAccepted {
    wire::CommandAccepted {
        action: accepted.action.to_owned(),
        command_id: accepted.command_id.to_string(),
        relay: None,
        settle_by: Time(accepted.settle_by),
        speaker: speaker_body_on(&accepted.speaker, access, device),
    }
}

pub fn queue_body(queue: &SpeakerQueue, access: &Access) -> wire::SpeakerQueue {
    wire::SpeakerQueue {
        current_index: queue.current_index,
        id: queue.id.to_string(),
        items: queue
            .items
            .iter()
            .map(|item| {
                let fields = media_fields(&item.media, access);
                wire::QueueItem {
                    album: item.media.album.clone(),
                    artist: item.media.artist.clone(),
                    artwork_id: fields.artwork_id,
                    duration_ms: item.media.duration_ms,
                    index: item.index,
                    item_id: item.item_id.clone(),
                    reference: fields.reference,
                    service_id: fields.service_id,
                    song_id: fields.song_id,
                    song_key: item.media.song_key.clone(),
                    title: item.media.title.clone(),
                }
            })
            .collect(),
        next_cursor: queue.next_cursor.clone(),
        repeat: queue.repeat.as_str().to_owned(),
        revision: queue.revision,
        shuffle: queue.shuffle,
        total: queue.total,
    }
}

/// An SSE speaker event for one listener: its name and data, or `None` when it is not for
/// them.
pub fn event_body(event: &SpeakerEvent, access: &Access) -> Option<(&'static str, String)> {
    event_body_on(event, access, None, "")
}

/// [`event_body`] for a listener on `device`, under the server's `base_path`.
pub fn event_body_on(
    event: &SpeakerEvent,
    access: &Access,
    device: Option<DeviceKey>,
    base_path: &str,
) -> Option<(&'static str, String)> {
    let data = |value: &dyn erased::Json| value.json();
    Some(match event {
        SpeakerEvent::Added(speaker) => (
            "speaker_added",
            data(&speaker_body_on(speaker, access, device)),
        ),
        SpeakerEvent::Updated(speaker) => (
            "speaker_updated",
            data(&speaker_body_on(speaker, access, device)),
        ),
        SpeakerEvent::Owned { user, event } => {
            if *user != access.user {
                return None;
            }
            return event_body_on(event, access, device, base_path);
        }
        SpeakerEvent::Relay(view) => {
            if view.user != access.user {
                return None;
            }
            ("relay", data(&super::relay::session_body(view, base_path)))
        }
        SpeakerEvent::Removed(id) => (
            "speaker_removed",
            data(&wire::SpeakerRemoved { id: id.to_string() }),
        ),
        SpeakerEvent::Progress {
            id,
            position_ms,
            position_at,
            revision,
            state,
        } => (
            "speaker_progress",
            data(&wire::SpeakerProgress {
                id: id.to_string(),
                position_at: Time(*position_at),
                position_ms: *position_ms,
                revision: *revision,
                state: state.as_str().to_owned(),
            }),
        ),
        SpeakerEvent::Queue { id, revision } => (
            "speaker_queue",
            data(&wire::SpeakerQueueEvent {
                id: id.to_string(),
                revision: *revision,
            }),
        ),
        SpeakerEvent::Command {
            user,
            command,
            speaker,
            outcome,
        } => {
            if *user != access.user {
                return None;
            }
            (
                "speaker_command",
                data(&command_event_body(
                    command,
                    speaker,
                    outcome.as_ref().err(),
                )),
            )
        }
        SpeakerEvent::State { state, since } => (
            "speakers",
            data(&wire::SpeakersEvent {
                since: since.map(Time),
                state: state.as_str().to_owned(),
            }),
        ),
        _ => return None,
    })
}

pub fn command_event_body(
    command: &trackside_core::SpeakerCommandId,
    speaker: &SpeakerId,
    failure: Option<&trackside_core::assistant::CommandFailure>,
) -> wire::SpeakerCommandEvent {
    wire::SpeakerCommandEvent {
        command_id: command.to_string(),
        error: failure.map(|f| wire::CommandError {
            code: f.code.to_owned(),
            holder_id: f.holder.as_ref().map(ToString::to_string),
            holder_name: f.holder_name.clone(),
            message: f.message.clone(),
            reason: f.reason.map(str::to_owned),
            service_id: f.service.as_ref().map(ToString::to_string),
        }),
        outcome: if failure.is_some() { "failed" } else { "done" }.to_owned(),
        speaker_id: speaker.to_string(),
    }
}

mod erased {
    pub trait Json {
        fn json(&self) -> String;
    }
    impl<T: serde::Serialize> Json for T {
        fn json(&self) -> String {
            serde_json::to_string(self).unwrap_or_else(|_| String::from("{}"))
        }
    }
}

/// The caller may see speakers (`features.speakers`); otherwise the routes do not exist.
fn allowed(state: &ApiState, principal: &Principal) -> ApiResult<()> {
    super::account::speakers_feature(state, principal)
        .map(drop)
        .ok_or_else(ApiError::not_found)
}

fn speaker_id(raw: &str) -> ApiResult<SpeakerId> {
    path_id(raw)
}

async fn list(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    allowed(&state, &principal)?;
    let list = state.assistant.speakers(&principal.access).await;
    let device = principal.device.as_ref().map(|(key, _)| *key);
    let body = speakers_body_on(&list, &principal.access, device);
    let newest = list.speakers.iter().map(|s| s.revision).max().unwrap_or(0);
    let etag = format!(
        "W/\"s{:x}-{newest}-{}-{}-{}\"",
        state.server.started_at.0,
        body.state,
        list.speakers.len(),
        principal.access.sync_epoch
    );
    Ok(cached_json(&headers, &etag, &body))
}

async fn one(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    allowed(&state, &principal)?;
    let id = speaker_id(&id)?;
    let speaker = state.assistant.speaker(&principal.access, &id).await?;
    let device = principal.device.as_ref().map(|(key, _)| *key);
    Ok(json_with(
        StatusCode::OK,
        &speaker_body_on(&speaker, &principal.access, device),
    ))
}

#[derive(Debug, Deserialize)]
struct QueueQuery {
    cursor: Option<String>,
    limit: Option<u32>,
    history: Option<u32>,
}

async fn queue(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Query(query): Query<QueueQuery>,
) -> ApiResult<Response> {
    allowed(&state, &principal)?;
    let id = speaker_id(&id)?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(ApiError::bad_field("limit", "limit is 1 to 500."));
    }
    let history = query.history.unwrap_or(0);
    if history > 50 {
        return Err(ApiError::bad_field("history", "history is 0 to 50."));
    }
    let page = QueuePage {
        cursor: query.cursor,
        limit,
        history,
    };
    let queue = state.assistant.queue(&principal.access, &id, page).await?;
    Ok(json_with(
        StatusCode::OK,
        &queue_body(&queue, &principal.access),
    ))
}

/// Runs a command: the feature, the rate limit, then the speaker, with `Idempotency-Key`.
#[expect(clippy::too_many_arguments, reason = "the parts of a command request")]
async fn run(
    state: &ApiState,
    principal: &Principal,
    headers: &HeaderMap,
    method: &Method,
    uri: &http::Uri,
    body: &[u8],
    id: &str,
    command: impl FnOnce() -> ApiResult<SpeakerCommand>,
) -> ApiResult<Response> {
    allowed(state, principal)?;
    let id = speaker_id(id)?;
    let command = command()?;
    let starts = matches!(
        command,
        SpeakerCommand::Play(_) | SpeakerCommand::Transfer { .. }
    );
    state.auth.speaker_command(principal, starts)?;
    let access = principal.access.clone();
    let device = principal.device.as_ref().map(|(key, _)| *key);
    idempotency::once_in_memory(
        state,
        access.user,
        headers,
        method,
        uri.path(),
        body,
        || async move {
            let accepted = state.assistant.command(&access, &id, command).await?;
            Ok((
                StatusCode::ACCEPTED,
                serde_json::to_value(command_body_on(&accepted, &access, device))?,
            ))
        },
    )
    .await
}

fn repeat_of(raw: &str, field: &'static str) -> ApiResult<Repeat> {
    Repeat::parse(raw)
        .ok_or_else(|| ApiError::bad_field(field, format!("{field} is off, one or all.")))
}

async fn playback(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    run(
        &state,
        &principal,
        &headers,
        &method,
        &uri,
        &body,
        &id,
        || {
            let request: wire::PlaybackRequest = parse_json(&body)?;
            Ok(match request.action.as_str() {
                "seek" => SpeakerCommand::Seek {
                    position_ms: request.position_ms.ok_or_else(|| {
                        ApiError::bad_field("position_ms", "seek needs position_ms.")
                    })?,
                },
                "jump" => SpeakerCommand::Jump {
                    item_id: request
                        .item_id
                        .ok_or_else(|| ApiError::bad_field("item_id", "jump needs item_id."))?,
                },
                other => {
                    SpeakerCommand::Playback(PlaybackAction::parse(other).ok_or_else(|| {
                        ApiError::bad_field(
                            "action",
                            "action is play, pause, toggle, stop, next, previous, seek or jump.",
                        )
                    })?)
                }
            })
        },
    )
    .await
}

async fn patch(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    run(
        &state,
        &principal,
        &headers,
        &method,
        &uri,
        &body,
        &id,
        || {
            let request: wire::SpeakerPatch = parse_json(&body)?;
            for (field, value) in [
                ("volume", request.volume),
                ("group_volume", request.group_volume),
            ] {
                if value.is_some_and(|v| v > 100) {
                    return Err(ApiError::bad_field(field, format!("{field} is 0 to 100.")));
                }
            }
            let patch = SpeakerPatch {
                volume: request.volume,
                muted: request.muted,
                powered: request.powered,
                shuffle: request.shuffle,
                repeat: request
                    .repeat
                    .as_deref()
                    .map(|r| repeat_of(r, "repeat"))
                    .transpose()?,
                group_volume: request.group_volume,
            };
            if patch.is_empty() {
                return Err(ApiError::bad_request("Nothing to change."));
            }
            Ok(SpeakerCommand::Set(patch))
        },
    )
    .await
}

async fn members(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    run(
        &state,
        &principal,
        &headers,
        &method,
        &uri,
        &body,
        &id,
        || {
            let request: wire::Members = parse_json(&body)?;
            if request.member_ids.len() > 100 {
                return Err(ApiError::bad_field(
                    "member_ids",
                    "member_ids holds at most 100.",
                ));
            }
            let ids = request
                .member_ids
                .iter()
                .map(|m| body_id::<SpeakerId>(m, "member_ids"))
                .collect::<ApiResult<Vec<_>>>()?;
            Ok(SpeakerCommand::Members(ids))
        },
    )
    .await
}

async fn leave_group(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    run(
        &state,
        &principal,
        &headers,
        &method,
        &uri,
        &[],
        &id,
        || Ok(SpeakerCommand::LeaveGroup),
    )
    .await
}

async fn remove_item(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path((id, item_id)): Path<(String, String)>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    run(
        &state,
        &principal,
        &headers,
        &method,
        &uri,
        &[],
        &id,
        || Ok(SpeakerCommand::RemoveItem { item_id }),
    )
    .await
}

fn play_request(body: &[u8], allow_catalog: bool) -> ApiResult<PlayRequest> {
    play_from_wire(parse_json(body)?, allow_catalog)
}

/// A `play` body, checked.
pub(crate) fn play_from_wire(
    request: wire::PlayRequest,
    allow_catalog: bool,
) -> ApiResult<PlayRequest> {
    if request.items.is_empty() || request.items.len() > MAX_PLAY_ITEMS {
        return Err(ApiError::bad_field(
            "items",
            format!("items holds 1 to {MAX_PLAY_ITEMS} entries."),
        ));
    }
    let items = request
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| match (&item.song_id, &item.reference) {
            (Some(song), None) => body_id::<SongId>(song, "items").map(PlayItem::Song),
            (None, Some(reference)) => Ok(PlayItem::Ref(reference.clone())),
            _ => Err(ApiError::bad_field(
                "items",
                format!("items[{i}] needs exactly one of song_id and ref."),
            )),
        })
        .collect::<ApiResult<Vec<_>>>()?;
    let mode = match request.mode.as_deref() {
        None => PlayMode::Replace,
        Some(mode) => PlayMode::parse(mode)
            .ok_or_else(|| ApiError::bad_field("mode", "mode is replace, next or add."))?,
    };
    let start_index = request.start_index.unwrap_or(0);
    if start_index >= items.len() {
        return Err(ApiError::bad_field(
            "start_index",
            "start_index is past the last item.",
        ));
    }
    if request.radio {
        radio_checked(&request, &items, mode)?;
    }
    if request.hold && mode != PlayMode::Replace {
        return Err(ApiError::bad_field(
            "hold",
            "hold is only for mode replace.",
        ));
    }
    if let Some(context) = &request.context
        && (context.kind.is_empty()
            || context.kind.len() > 32
            || context.name.as_ref().is_some_and(|n| n.len() > 500)
            || context.id.as_ref().is_some_and(|n| n.len() > 1024))
    {
        return Err(ApiError::bad_field("context", "context is not valid."));
    }
    Ok(PlayRequest {
        items,
        mode,
        start_index,
        position_ms: request.position_ms.unwrap_or(0),
        shuffle: request.shuffle,
        repeat: request
            .repeat
            .as_deref()
            .map(|r| repeat_of(r, "repeat"))
            .transpose()?,
        context: request.context.map(|c| PlayContext {
            kind: c.kind,
            id: c.id,
            name: c.name,
        }),
        allow_catalog,
        radio: request.radio,
        hold: request.hold,
        takeover: request.takeover,
    })
}

/// A radio starts from one catalog item that can seed one, replacing the queue from its
/// start.
fn radio_checked(request: &wire::PlayRequest, items: &[PlayItem], mode: PlayMode) -> ApiResult<()> {
    let [PlayItem::Ref(reference)] = items else {
        return Err(ApiError::bad_field(
            "items",
            "A radio starts from exactly one ref.",
        ));
    };
    if mode != PlayMode::Replace {
        return Err(ApiError::bad_field("mode", "A radio replaces the queue."));
    }
    if request.shuffle.is_some() || request.position_ms.is_some_and(|p| p > 0) {
        return Err(ApiError::bad_field(
            "radio",
            "A radio starts at its beginning, without shuffle.",
        ));
    }
    let seeds = trackside_core::catalog_ref::CatalogRef::decode(reference)
        .is_ok_and(|r| r.kind != trackside_core::catalog_ref::RefKind::Radio);
    if !seeds {
        return Err(ApiError::bad_field(
            "items",
            "A radio starts from a track, artist, album or playlist ref.",
        ));
    }
    Ok(())
}

async fn play(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let allow_catalog = principal.permissions.can_play_services;
    run(
        &state,
        &principal,
        &headers,
        &method,
        &uri,
        &body,
        &id,
        || play_request(&body, allow_catalog).map(SpeakerCommand::Play),
    )
    .await
}

async fn transfer(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    method: Method,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let request: wire::Transfer = parse_json(&body)?;
    if request.to != "this_device" {
        if request.tier.is_some() {
            return Err(ApiError::bad_field(
                "tier",
                "tier is only for \"to\": \"this_device\".",
            ));
        }
        return run(
            &state,
            &principal,
            &headers,
            &method,
            &uri,
            &body,
            &id,
            || {
                Ok(SpeakerCommand::Transfer {
                    to: body_id(&request.to, "to")?,
                    keep_playing_here: request.keep_playing_here,
                })
            },
        )
        .await;
    }
    to_this_device(
        &state,
        &principal,
        request,
        &id,
        (&method, &uri, &headers, &body),
    )
    .await
}

/// Moves the queue to the calling device's relay, creating or reusing it (contract 1.7).
async fn to_this_device(
    state: &ApiState,
    principal: &Principal,
    request: wire::Transfer,
    id: &str,
    (method, uri, headers, body): (&Method, &http::Uri, &HeaderMap, &Bytes),
) -> ApiResult<Response> {
    allowed(state, principal)?;
    let id = speaker_id(id)?;
    let tier = match request.tier.as_deref() {
        None => RelayTier::Lossless,
        Some(raw) => RelayTier::parse(raw)
            .ok_or_else(|| ApiError::bad_field("tier", "tier is lossless or cellular."))?,
    };
    let caller = match super::relay::caller(state, principal, tier, None).await {
        Err(e) if e.status == StatusCode::FORBIDDEN || e.status == StatusCode::NOT_FOUND => {
            return Err(SpeakerError::Unsupported {
                capability: "relay",
                speaker: None,
            }
            .into());
        }
        other => other?,
    };
    state.auth.speaker_command(principal, true)?;
    let access = principal.access.clone();
    let keep_playing_here = request.keep_playing_here;
    idempotency::once_in_memory(
        state,
        access.user,
        headers,
        method,
        uri.path(),
        body,
        || async move {
            let control = state.assistant.relay().ok_or_else(ApiError::not_found)?;
            let opened = control.open(&caller, RelayOpen { tier }).await?;
            let accepted = state
                .assistant
                .command(
                    &access,
                    &id,
                    SpeakerCommand::Transfer {
                        to: opened.session.speaker_id.clone(),
                        keep_playing_here,
                    },
                )
                .await?;
            let view = control
                .session(caller.user, &opened.session.id)
                .unwrap_or(opened.session);
            let mut reply = command_body_on(&accepted, &access, Some(caller.device));
            reply.relay = Some(super::relay::session_body(&view, &state.server.base_path));
            Ok((StatusCode::ACCEPTED, serde_json::to_value(reply)?))
        },
    )
    .await
}
