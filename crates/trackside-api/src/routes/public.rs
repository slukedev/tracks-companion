//! Public endpoints: ping, capabilities and setup status (api.md section 5).

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use http::HeaderMap;
use serde::Deserialize;
use trackside_auth::extract::ClientIp;
use trackside_auth::proof::ProofKey;
use trackside_core::repo::settings::{self, keys};
use trackside_core::{DeviceId, PairingId, PublicId};

use super::{read, server_name};
use crate::error::ApiResult;
use crate::extract::Query;
use crate::wire::{
    ApiVersions, AuthFeatures, Capabilities, Features, MediaFeatures, Ping, SubsonicInfo, Time,
};
use crate::{ApiState, wire};

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/ping", get(ping))
        .route("/capabilities", get(capabilities))
        .route("/setup/status", get(setup_status))
}

/// `?challenge=<nonce>&host=<address>` with `device=<id>` or `offer=<id>`.
#[derive(Debug, Default, Deserialize)]
struct PingQuery {
    challenge: Option<String>,
    host: Option<String>,
    device: Option<String>,
    offer: Option<String>,
}

async fn ping(
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Query(query): Query<PingQuery>,
) -> ApiResult<Json<Ping>> {
    let server_id = state.server.server_id.to_string();
    let key = match (&query.device, &query.offer) {
        (Some(device), None) => DeviceId::parse(device).ok().map(ProofKey::Device),
        (None, Some(offer)) => PairingId::parse(offer).ok().map(ProofKey::Offer),
        _ => None,
    };
    let proof = match (key, &query.challenge, &query.host) {
        (Some(key), Some(nonce), Some(host)) => {
            let nearby = state.auth.is_nearby_client(&headers, ip);
            state
                .auth
                .endpoint_proof(key, &server_id, host, nonce, nearby)
                .await?
        }
        _ => None,
    };
    Ok(Json(Ping {
        server_id,
        time: Time(state.db.clock().now()),
        proof,
    }))
}

/// Whether the first admin still has to be created. Read from the stored setup state so it
/// answers before anyone signs in.
pub(crate) async fn setup_required(state: &ApiState) -> ApiResult<bool> {
    let stored = read(state, |c| settings::get::<String>(c, keys::SETUP_STATE)).await?;
    Ok(stored.as_deref() != Some("done"))
}

/// The capabilities document. Never mentions acquisition.
pub fn capabilities_body(
    server: &crate::ServerInfo,
    media: &trackside_media::MediaCapabilities,
    name: String,
    setup_required: bool,
    uploads: bool,
    cloud: bool,
) -> Capabilities {
    Capabilities {
        product: String::from("trackside"),
        server_id: server.server_id.to_string(),
        name,
        version: server.version.to_owned(),
        api: ApiVersions {
            versions: vec![String::from("v1")],
            current: String::from("v1"),
            deprecated: Vec::new(),
        },
        setup_required,
        auth: AuthFeatures {
            password_login: true,
            pairing: true,
            pair_requests: true,
        },
        media: MediaFeatures {
            transcoding: media.ffmpeg.is_some(),
            profiles: media
                .profiles
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect(),
            hls: media.hls,
            suggested_cellular_profile: media
                .suggested_cellular_profile
                .map(|p| p.as_str().to_owned()),
        },
        features: Features {
            lyrics: true,
            uploads,
            events: true,
            animated_art: false,
            cloud,
        },
        subsonic: SubsonicInfo {
            enabled: true,
            path: format!("{}/rest", server.base_path),
        },
        cloud: cloud.then(super::cloud::cloud_info),
    }
}

async fn capabilities(State(state): State<ApiState>) -> ApiResult<Json<Capabilities>> {
    let setup = setup_required(&state).await?;
    let uploads = super::settings::effective_bool(&state, "uploads.enabled").await;
    let cloud = super::settings::effective_bool(&state, super::cloud::ENABLED).await;
    let name = server_name(&state).await;
    Ok(Json(capabilities_body(
        &state.server,
        &state.media.capabilities(),
        name,
        setup,
        uploads,
        cloud,
    )))
}

async fn setup_status(State(state): State<ApiState>) -> ApiResult<Json<wire::auth::SetupStatus>> {
    Ok(Json(wire::auth::SetupStatus {
        setup_required: setup_required(&state).await?,
    }))
}
