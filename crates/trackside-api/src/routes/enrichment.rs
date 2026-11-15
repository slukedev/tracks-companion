//! Last.fm enrichment: its status and a test call for the dashboard (api.md section 16,
//! Suggestions), and the on-demand lookups library requests start.

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use http::StatusCode;
use trackside_auth::extract::AdminSession;
use trackside_core::config::Secret;
use trackside_core::enrich::{ON_DEMAND_WAIT, Target};

use super::settings;
use crate::ApiState;
use crate::error::{ApiError, ApiResult};
use crate::extract::{OptionalJson, json_with};
use crate::wire::enrichment::{
    EnrichmentError, EnrichmentStatus, EnrichmentTestRequest, EnrichmentTestResult,
};

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/enrichment", get(status))
        .route("/enrichment/test", post(test))
}

pub fn status_body(status: &trackside_core::enrich::EnrichStatus) -> EnrichmentStatus {
    EnrichmentStatus {
        configured: status.configured,
        artists: status.artists,
        songs: status.songs,
        last_fetch_at: status.last_fetch.map(Into::into),
        last_error: status
            .last_error
            .as_ref()
            .map(|(at, message)| EnrichmentError {
                at: (*at).into(),
                message: message.clone(),
            }),
    }
}

async fn status(AdminSession(_): AdminSession, State(state): State<ApiState>) -> Response {
    json_with(StatusCode::OK, &status_body(&state.enrich.status().await))
}

async fn test(
    AdminSession(_): AdminSession,
    State(state): State<ApiState>,
    OptionalJson(body): OptionalJson<EnrichmentTestRequest>,
) -> ApiResult<Response> {
    let key = match body.api_key.filter(|k| !k.trim().is_empty()) {
        Some(key) => {
            let checked = settings::validate(settings::Kind::Secret, &serde_json::json!(key))
                .map_err(|m| ApiError::bad_field("api_key", format!("The key {m}.")))?;
            Some(Secret::new(checked.as_str().unwrap_or_default()))
        }
        None => None,
    };
    let result = state.enrich.test(key).await;
    Ok(json_with(
        StatusCode::OK,
        &EnrichmentTestResult {
            ok: result.ok,
            elapsed_ms: u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX),
            detail: result.detail,
        },
    ))
}

/// Starts lookups a request needs and waits briefly for them; answers come from what is
/// stored either way.
pub(crate) async fn look_up(state: &ApiState, targets: Vec<Target>) {
    if !targets.is_empty() {
        state.enrich.ensure(targets, ON_DEMAND_WAIT).await;
    }
}

/// The lookups a song's suggestions use: the song, and its lead artist.
pub(crate) fn song_targets(song: &trackside_core::records::Song) -> Vec<Target> {
    let mut targets = vec![Target::Song {
        key: song.song_key.clone(),
        title: song.title.clone(),
        artist: song.artist.clone(),
    }];
    let lead = song
        .credits
        .first()
        .map_or(song.artist.as_str(), |c| c.name.as_str());
    targets.push(Target::artist(lead));
    targets
}

pub(crate) fn artist_target(artist: &trackside_core::records::Artist) -> Target {
    Target::Artist {
        identity: artist.identity.clone(),
        name: artist.name.clone(),
    }
}
