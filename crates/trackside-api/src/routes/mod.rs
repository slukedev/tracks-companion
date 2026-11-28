//! The `/api/v1` routes and what the handlers share.

pub(crate) mod account;
pub(crate) mod acquisition;
pub(crate) mod addresses;
pub(crate) mod admin;
pub(crate) mod apple_music;
pub(crate) mod catalog;
pub(crate) mod cloud;
pub(crate) mod enrichment;
mod events;
mod idempotency;
pub(crate) mod library;
pub(crate) mod link;
pub(crate) mod media;
pub(crate) mod music_assistant;
pub(crate) mod public;
pub(crate) mod settings;
pub(crate) mod social;
pub(crate) mod speakers;
pub(crate) mod stats;
pub(crate) mod sync;
pub(crate) mod uploads;

use std::collections::HashMap;
use std::net::IpAddr;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware::from_fn;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use trackside_auth::Principal;
use trackside_core::records::{ItemRef, Song, UserAnnotations};
use trackside_core::repo::audit::AuditEntry;
use trackside_core::{Access, DbError, LibraryId, WriteTx};

use crate::error::{ApiError, ApiResult};
use crate::extract::request_context;
use crate::wire::auth::{ServerSummary, ServerUrl, UrlKind};
use crate::{ApiState, map, wire};

pub use account::Https;

/// Request bodies are limited to 1 MiB except uploads.
const BODY_LIMIT: usize = 1 << 20;

pub(crate) fn router(state: ApiState) -> Router {
    let api = Router::new()
        .merge(public::routes())
        .merge(account::routes())
        .merge(link::routes())
        .merge(addresses::routes())
        .merge(library::routes())
        .merge(sync::routes())
        .merge(media::routes())
        .merge(social::routes())
        .merge(events::routes())
        .merge(uploads::routes(&state))
        .merge(acquisition::routes())
        .merge(cloud::routes())
        .merge(apple_music::routes())
        .merge(speakers::routes())
        .merge(catalog::routes())
        .nest(
            "/admin",
            admin::routes()
                .merge(stats::routes())
                .merge(enrichment::routes())
                .merge(apple_music::admin_routes())
                .merge(music_assistant::admin_routes()),
        )
        .fallback(|| async { ApiError::not_found() })
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            addresses::https_urls_on_refusal,
        ))
        .layer(from_fn(request_context))
        .with_state(state);
    Router::new().nest("/api/v1", api)
}

/// Runs a read on the reader pool.
pub(crate) async fn read<T, F>(state: &ApiState, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite_conn::Connection) -> Result<T, DbError> + Send + 'static,
{
    state.db.read(f).await.map_err(ApiError::from)
}

/// Runs a write transaction.
pub(crate) async fn write<T, F>(state: &ApiState, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&mut WriteTx<'_>) -> Result<T, DbError> + Send + 'static,
{
    state.db.write(f).await.map_err(ApiError::from)
}

/// The connection type repositories take, named without depending on rusqlite directly.
pub(crate) mod rusqlite_conn {
    pub type Connection = <trackside_core::WriteTx<'static> as std::ops::Deref>::Target;
}

/// A server-relative URL under `/api/v1`, with the base path in front.
pub(crate) fn api_url(state: &ApiState, path: &str) -> String {
    format!("{}/api/v1{path}", state.server.base_path)
}

/// The server's name: the dashboard's setting unless the configuration sets it.
pub(crate) async fn server_name(state: &ApiState) -> String {
    match settings::effective(state, "server.name").await {
        Some(serde_json::Value::String(name)) if !name.trim().is_empty() => name,
        _ => state.server.name.clone(),
    }
}

/// Every candidate URL: detected LAN URLs and configured public ones.
pub(crate) async fn candidate_urls(state: &ApiState) -> Vec<ServerUrl> {
    let mut urls = state.server.urls.get();
    let configured = match settings::effective(state, "server.public_urls").await {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => state.config.server.public_urls.clone(),
    };
    for url in configured {
        let url = url.trim().trim_end_matches('/').to_owned();
        if url.is_empty() || urls.iter().any(|u| u.url == url) {
            continue;
        }
        urls.push(ServerUrl {
            kind: url_kind(&url),
            tls: url.starts_with("https://"),
            url,
            fingerprint: None,
        });
    }
    urls
}

/// `tailnet` for Tailscale names and addresses (`*.ts.net`, 100.64.0.0/10), `public`
/// otherwise.
pub(crate) fn url_kind(url: &str) -> UrlKind {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let cgnat = host
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64);
    if host.ends_with(".ts.net") || cgnat {
        UrlKind::Tailnet
    } else {
        UrlKind::Public
    }
}

pub(crate) async fn server_summary(state: &ApiState, version: bool) -> ServerSummary {
    ServerSummary {
        server_id: state.server.server_id.to_string(),
        name: server_name(state).await,
        version: version.then(|| state.server.version.to_owned()),
        urls: candidate_urls(state).await,
    }
}

/// Writes an audit entry. A failure to audit is logged and does not undo the action.
pub(crate) async fn audit(
    state: &ApiState,
    actor: Option<&Principal>,
    ip: Option<IpAddr>,
    action: &str,
    target: Option<String>,
    detail: Option<serde_json::Value>,
) {
    let entry = AuditEntry {
        actor: actor.map(|p| p.access.user),
        actor_name: actor.map_or_else(|| String::from("system"), |p| p.username.clone()),
        ip: ip.map(|ip| ip.to_string()),
        action: action.to_owned(),
        target,
        detail,
    };
    if let Err(e) = state
        .db
        .write(move |tx| trackside_core::repo::audit::log(tx, &entry))
        .await
    {
        tracing::warn!(error = %e, action, "audit entry not written");
    }
}

/// Songs with the caller's stars, ratings and plays.
pub(crate) async fn songs_with_me(
    state: &ApiState,
    access: &Access,
    songs: Vec<Song>,
) -> ApiResult<Vec<wire::Song>> {
    let annotations = annotations(
        state,
        access,
        songs.iter().map(|s| ItemRef::Song(s.id.clone())),
    )
    .await?;
    Ok(map::songs_with_me(&songs, &annotations))
}

pub(crate) async fn annotations(
    state: &ApiState,
    access: &Access,
    items: impl Iterator<Item = ItemRef>,
) -> ApiResult<HashMap<ItemRef, UserAnnotations>> {
    let items: Vec<ItemRef> = items.collect();
    if items.is_empty() {
        return Ok(HashMap::new());
    }
    let user = access.user;
    read(state, move |c| {
        trackside_core::repo::annotations::for_items(c, user, &items)
    })
    .await
}

/// Whether the scanner sees the library right now.
pub(crate) fn available(state: &ApiState, library: &LibraryId) -> bool {
    state.scan.is_available(library)
}

/// The weak `ETag` of library reads: the latest change seq and the caller's sync epoch, with
/// the process start so a restart never reuses one.
pub(crate) fn library_etag(state: &ApiState, access: &Access) -> String {
    format!(
        "W/\"{:x}-{}-{}-{}\"",
        state.server.started_at.0,
        state.db.current_seq().0,
        access.user.0,
        access.sync_epoch
    )
}

/// The weak `ETag` of a suggestion list that does not depend on the time: the database
/// generation (which moves with every change suggestions read, listens and Last.fm data
/// included), the caller and their sync epoch, and `what` (the endpoint and its parameters),
/// with the process start so a restart never reuses one.
pub(crate) fn suggestion_etag(state: &ApiState, access: &Access, what: &str) -> String {
    format!(
        "W/\"{:x}-g{}-{}-{}-{what}\"",
        state.server.started_at.0,
        state.db.generation(),
        access.user.0,
        access.sync_epoch
    )
}

/// Whether `If-None-Match` names `etag` (weak comparison).
pub(crate) fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    let strip = |t: &str| t.trim().trim_start_matches("W/").to_owned();
    let wanted = strip(etag);
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|t| t.trim() == "*" || strip(t) == wanted)
}

/// A JSON library read with its weak `ETag`, or `304` when the client has it.
pub(crate) fn cached_json<T: serde::Serialize>(
    headers: &HeaderMap,
    etag: &str,
    body: &T,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut response = if matches_etag(headers, etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        axum::Json(body).into_response()
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    response
}

/// Refuses a sensitive action on a session whose last password check is older than
/// [`trackside_auth::RECENT_AUTH`].
pub(crate) fn require_recent_auth(
    state: &ApiState,
    principal: &trackside_auth::Principal,
) -> ApiResult<()> {
    if principal.recently_authenticated(state.db.clock().now()) {
        Ok(())
    } else {
        Err(ApiError::reauth_required())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_urls_are_tailnet() {
        assert_eq!(url_kind("https://box.tail4a2c.ts.net"), UrlKind::Tailnet);
        assert_eq!(url_kind("http://100.84.12.7:4747"), UrlKind::Tailnet);
        assert_eq!(url_kind("http://100.200.1.1:4747"), UrlKind::Public);
        assert_eq!(url_kind("https://music.example.com/x"), UrlKind::Public);
    }

    #[test]
    fn etags_compare_weakly() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"other\", W/\"a-1\""),
        );
        assert!(matches_etag(&headers, "W/\"a-1\""));
        assert!(!matches_etag(&headers, "W/\"a-2\""));
        assert!(!matches_etag(&HeaderMap::new(), "W/\"a-1\""));
    }
}
