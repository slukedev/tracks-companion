//! Device-code linking for TVs (api.md section 6, Linking a TV): `/link/start` and
//! `/link/poll` for the device, `/link/lookup`, `/link/approve` and `/link/deny` for whoever
//! approves it in Tracks or the dashboard.

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use http::StatusCode;
use trackside_auth::extract::{Authed, ClientIp, SecureTransport};
use trackside_auth::{DeviceLinkStart, LinkDecision, LinkRequest, Scopes};

use super::account::{new_device, pair_result};
use super::candidate_urls;
use crate::ApiState;
use crate::error::{ApiResult, device_link_error};
use crate::extract::{Json, json_with};
use crate::wire::auth::{
    LinkCodeRequest, LinkDevice, LinkInfo, LinkPollRequest, LinkStart, LinkStartRequest, LinkState,
    ServerUrl, UrlKind,
};

const LIBRARY: u8 = Scopes::LIBRARY.bits();
const WRITE: u8 = Scopes::WRITE.bits();

pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/link/start", post(start))
        .route("/link/poll", post(poll))
        .route("/link/lookup", post(lookup))
        .route("/link/approve", post(approve))
        .route("/link/deny", post(deny))
}

/// The address a phone's browser is most likely to reach and trust: a public HTTPS address,
/// then the tailnet's, then any HTTPS address, then the first one. Without any, a path.
fn verification_base(urls: &[ServerUrl], base_path: &str) -> String {
    let rank = |u: &ServerUrl| match (u.kind, u.tls) {
        (UrlKind::Public, true) => 0,
        (UrlKind::Tailnet, true) => 1,
        (_, true) => 2,
        _ => 3,
    };
    urls.iter()
        .enumerate()
        .min_by_key(|(i, u)| (rank(u), *i))
        .map_or_else(|| base_path.to_owned(), |(_, u)| u.url.clone())
}

/// The `/link/start` response. Pure, for the fixture test.
pub fn link_start_body(
    start: &DeviceLinkStart,
    server_id: &str,
    urls: &[ServerUrl],
    base_path: &str,
) -> LinkStart {
    LinkStart {
        device_code: start.device_code.clone(),
        user_code: start.user_code.clone(),
        verification_url: format!(
            "{}/link?code={}",
            verification_base(urls, base_path),
            start.user_code
        ),
        link_url: format!("tracks://link?server={server_id}&code={}", start.user_code),
        interval_s: start.interval.as_secs(),
        expires_at: start.expires_at.into(),
    }
}

/// What a code links. Pure, for the fixture test.
pub fn link_info_body(request: &LinkRequest, state: LinkState) -> LinkInfo {
    LinkInfo {
        state,
        device: LinkDevice {
            name: request.device_name.clone(),
            platform: request.platform.clone(),
            model: request.model.clone(),
            app_name: request.app_name.clone(),
        },
        nearby: request.nearby,
        requested_at: request.requested_at.into(),
        expires_at: request.expires_at.into(),
    }
}

async fn start(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LinkStartRequest>,
) -> ApiResult<Response> {
    let device = new_device(body.device)?;
    let started = state.auth.start_device_link(device, ip).await?;
    let urls = candidate_urls(&state).await;
    let body = link_start_body(
        &started,
        &state.server.server_id.to_string(),
        &urls,
        &state.server.base_path,
    );
    Ok(json_with(StatusCode::CREATED, &body))
}

async fn poll(
    _secure: SecureTransport,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LinkPollRequest>,
) -> ApiResult<Response> {
    let issued = state
        .auth
        .poll_device_link(&body.device_code, ip)
        .await
        .map_err(device_link_error)?;
    let result = pair_result(&state, issued).await?;
    Ok(json_with(StatusCode::OK, &result))
}

async fn lookup(
    Authed(principal): Authed<LIBRARY>,
    State(state): State<ApiState>,
    Json(body): Json<LinkCodeRequest>,
) -> ApiResult<Response> {
    let request = state
        .auth
        .lookup_device_link(&principal, &body.user_code)
        .await?;
    Ok(json_with(
        StatusCode::OK,
        &link_info_body(&request, LinkState::Pending),
    ))
}

async fn approve(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LinkCodeRequest>,
) -> ApiResult<Response> {
    let request = state
        .auth
        .decide_device_link(&principal, &body.user_code, LinkDecision::Approve, ip)
        .await?;
    Ok(json_with(
        StatusCode::OK,
        &link_info_body(&request, LinkState::Approved),
    ))
}

async fn deny(
    Authed(principal): Authed<WRITE>,
    State(state): State<ApiState>,
    ClientIp(ip): ClientIp,
    Json(body): Json<LinkCodeRequest>,
) -> ApiResult<Response> {
    let request = state
        .auth
        .decide_device_link(&principal, &body.user_code, LinkDecision::Deny, ip)
        .await?;
    Ok(json_with(
        StatusCode::OK,
        &link_info_body(&request, LinkState::Denied),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(url: &str, kind: UrlKind) -> ServerUrl {
        ServerUrl {
            url: url.to_owned(),
            kind,
            tls: url.starts_with("https://"),
            fingerprint: None,
        }
    }

    #[test]
    fn verification_prefers_an_address_a_phone_trusts() {
        let lan = url("http://192.168.1.20:4747", UrlKind::Lan);
        let tailnet = url("https://box.tail4a2c.ts.net", UrlKind::Tailnet);
        let public = url("https://music.example.com", UrlKind::Public);
        assert_eq!(
            verification_base(&[lan.clone(), tailnet.clone(), public], ""),
            "https://music.example.com"
        );
        assert_eq!(
            verification_base(&[lan.clone(), tailnet], ""),
            "https://box.tail4a2c.ts.net"
        );
        assert_eq!(verification_base(&[lan], ""), "http://192.168.1.20:4747");
        assert_eq!(verification_base(&[], "/music"), "/music");
    }
}
