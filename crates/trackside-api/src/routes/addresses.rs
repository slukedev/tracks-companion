//! `GET /api/v1/server/urls`: the addresses a paired device may use now, with a version, so it
//! learns new ones without pairing again (api.md section 7). The list comes only from the
//! server's own interfaces and its configuration or settings, never from a request.

use std::fmt::Write as _;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use http::HeaderMap;
use sha2::{Digest, Sha256};
use trackside_auth::Scopes;
use trackside_auth::extract::Authed;
use trackside_core::ServerEvent;

use crate::ApiState;
use crate::wire::auth::{ServerUrl, ServerUrls};

const LIBRARY: u8 = Scopes::LIBRARY.bits();

/// How often the list is compared with the last one announced, to catch interface changes.
const CHECK_EVERY: Duration = Duration::from_secs(15);

pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/server/urls", get(server_urls))
}

/// Names a list of addresses: the same list always has the same version, across restarts.
pub fn urls_version(urls: &[ServerUrl]) -> String {
    let canonical = serde_json::to_vec(urls).unwrap_or_default();
    let digest = Sha256::digest(&canonical);
    digest[..16]
        .iter()
        .fold(String::with_capacity(32), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

pub(crate) async fn body(state: &ApiState) -> ServerUrls {
    let urls = super::candidate_urls(state).await;
    ServerUrls {
        server_id: state.server.server_id.to_string(),
        version: urls_version(&urls),
        urls,
    }
}

async fn server_urls(
    Authed(_): Authed<LIBRARY>,
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Response {
    let body = body(&state).await;
    let etag = format!("\"{}\"", body.version);
    super::cached_json(&headers, &etag, &body)
}

/// Middleware: adds `details.https_urls` to a `403 https_required`, so a client refused over
/// plain HTTP can switch to an HTTPS address without first reading `/server/urls`, which it
/// could not authenticate for. The addresses come from the same list, never from the request.
pub(crate) async fn https_urls_on_refusal(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() != http::StatusCode::FORBIDDEN {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await else {
        return Response::from_parts(parts, axum::body::Body::empty());
    };
    let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Response::from_parts(parts, axum::body::Body::from(bytes)),
    };
    let Some(error) = value
        .get_mut("error")
        .and_then(|e| e.as_object_mut())
        .filter(|e| e.get("code").and_then(|c| c.as_str()) == Some("https_required"))
    else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    let https: Vec<ServerUrl> = super::candidate_urls(&state)
        .await
        .into_iter()
        .filter(|u| u.tls)
        .collect();
    if https.is_empty() {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    let details = error
        .entry("details")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(details) = details.as_object_mut() {
        details.insert(
            String::from("https_urls"),
            serde_json::to_value(&https).unwrap_or_default(),
        );
    }
    parts.headers.remove(http::header::CONTENT_LENGTH);
    Response::from_parts(
        parts,
        axum::body::Body::from(serde_json::to_vec(&value).unwrap_or_default()),
    )
}

/// Sends `server_urls` to every event stream when the list differs from the last one sent.
pub(crate) async fn announce(state: &ApiState) {
    let version = body(state).await.version;
    let changed = {
        let mut last = state
            .local
            .urls_version()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = last.as_deref() != Some(version.as_str());
        if changed {
            *last = Some(version.clone());
        }
        changed
    };
    if changed {
        state.events.publish(ServerEvent::ServerUrls { version });
    }
}

/// Compares the list with the last one announced until shutdown. Settings changes announce
/// at once; this catches the server's own addresses changing.
pub(crate) async fn watch(state: ApiState) {
    let stop = state.local.shutdown().clone();
    loop {
        announce(&state).await;
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(CHECK_EVERY) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::auth::UrlKind;

    fn url(url: &str, kind: UrlKind) -> ServerUrl {
        ServerUrl {
            url: url.to_owned(),
            kind,
            tls: url.starts_with("https://"),
            fingerprint: None,
        }
    }

    #[test]
    fn the_version_follows_the_list_and_nothing_else() {
        let lan = url("http://192.168.1.20:4747", UrlKind::Lan);
        let public = url("https://music.example.com", UrlKind::Public);
        let one = urls_version(std::slice::from_ref(&lan));
        assert_eq!(one.len(), 32);
        assert_eq!(one, urls_version(std::slice::from_ref(&lan)));
        assert_ne!(one, urls_version(&[lan.clone(), public.clone()]));
        assert_ne!(
            urls_version(&[lan.clone(), public.clone()]),
            urls_version(&[public.clone(), lan.clone()]),
            "order is preference"
        );
        let mut pinned = public.clone();
        pinned.fingerprint = Some(String::from("3a:7f"));
        assert_ne!(
            urls_version(&[lan.clone(), public]),
            urls_version(&[lan, pinned])
        );
    }
}
