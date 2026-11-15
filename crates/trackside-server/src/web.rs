//! What every request goes through (request ids, HTTPS detection, logging, metrics, security
//! headers, timeouts, JSON-only compression) and the embedded dashboard.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use tower_http::compression::predicate::Predicate;
use trackside_api::{Https, RequestId, X_REQUEST_ID};
use trackside_auth::Auth;

/// Whether the listener a request came in on serves TLS. Set per listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListenerTls(pub bool);

/// Counters for `/metrics`, by normalised route and status.
#[derive(Debug, Default)]
pub struct Metrics {
    requests: Mutex<HashMap<(String, u16), u64>>,
    latency: Mutex<HashMap<String, (u64, f64)>>,
}

impl Metrics {
    pub fn record(&self, route: &str, status: u16, elapsed: Duration) {
        *self
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry((route.to_owned(), status))
            .or_default() += 1;
        let mut latency = self.latency.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = latency.entry(route.to_owned()).or_default();
        entry.0 += 1;
        entry.1 += elapsed.as_secs_f64();
    }

    /// The request counters in Prometheus text format.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write;
        let requests = self.requests.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<_> = requests.iter().collect();
        rows.sort();
        let _ = writeln!(out, "# TYPE trackside_http_requests_total counter");
        for ((route, status), count) in rows {
            let _ = writeln!(
                out,
                "trackside_http_requests_total{{route=\"{route}\",status=\"{status}\"}} {count}"
            );
        }
        drop(requests);
        let latency = self.latency.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<_> = latency.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let _ = writeln!(out, "# TYPE trackside_http_request_seconds summary");
        for (route, (count, sum)) in rows {
            let _ = writeln!(
                out,
                "trackside_http_request_seconds_sum{{route=\"{route}\"}} {sum:.6}"
            );
            let _ = writeln!(
                out,
                "trackside_http_request_seconds_count{{route=\"{route}\"}} {count}"
            );
        }
    }
}

/// The route template a request matched, such as `/api/v1/songs/{id}/stream`, put on the
/// response by [`mark_route`] so metrics have one series per route whatever the path held.
#[derive(Clone, Debug)]
pub struct RouteLabel(pub Arc<str>);

/// What requests that matched no route (the dashboard, unknown paths) are counted under.
pub const UNMATCHED_ROUTE: &str = "other";

/// A route layer that records the matched route template on the response.
pub async fn mark_route(request: Request, next: Next) -> Response {
    let label = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| RouteLabel(Arc::from(m.as_str())));
    let mut response = next.run(request).await;
    if let Some(label) = label {
        response.extensions_mut().insert(label);
    }
    response
}

fn route_label(response: &Response) -> &str {
    response
        .extensions()
        .get::<RouteLabel>()
        .map_or(UNMATCHED_ROUTE, |l| &l.0)
}

#[derive(Clone)]
pub struct WebState {
    pub auth: Arc<Auth>,
    /// `None` when metrics are off, so nothing is counted.
    pub metrics: Option<Arc<Metrics>>,
    pub request_timeout: Duration,
    pub playback: Arc<trackside_api::PlaybackLoad>,
}

impl std::fmt::Debug for WebState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebState").finish_non_exhaustive()
    }
}

/// Paths whose handlers may legitimately take longer than the request timeout before the
/// first byte: downloads that wait for a transcode, uploads, backups.
fn exempt_from_timeout(path: &str) -> bool {
    [
        "/download",
        "/uploads",
        "/backups/",
        "/rest/download",
        "/rest/stream",
        "/events",
        "/listens/export",
    ]
    .iter()
    .any(|p| path.contains(p))
}

/// Request id, HTTPS marker, timeout, log line and metrics for every request.
pub async fn context(State(web): State<WebState>, mut request: Request, next: Next) -> Response {
    let started = Instant::now();
    let (mut parts, body) = request.into_parts();
    let listener_tls = parts.extensions.get::<ListenerTls>().is_some_and(|t| t.0);
    let forwarded_https = web
        .auth
        .trusted_forwarded(&parts, "x-forwarded-proto")
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    let id = web
        .auth
        .trusted_forwarded(&parts, X_REQUEST_ID)
        .filter(|v| v.len() <= 128 && v.chars().all(|c| c.is_ascii_graphic()))
        .map_or_else(RequestId::new, |v| RequestId(v.to_owned()));
    if listener_tls || forwarded_https {
        parts.extensions.insert(Https);
    }
    parts.extensions.insert(id.clone());
    let method = parts.method.clone();
    let uri = trackside_auth::redact_uri(&parts.uri);
    let path = parts.uri.path().to_owned();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| web.auth.client_ip(&parts, c.0));
    request = Request::from_parts(parts, body);
    let media = is_media_path(&path);
    let guard = media.then(|| web.playback.start());
    let mut response = if exempt_from_timeout(&path) {
        next.run(request).await
    } else {
        match tokio::time::timeout(web.request_timeout, next.run(request)).await {
            Ok(response) => response,
            Err(_) => timeout_response(&id),
        }
    };
    if let Ok(value) = HeaderValue::from_str(&id.0) {
        response.headers_mut().insert(X_REQUEST_ID, value);
    }
    security_headers(response.headers_mut());
    if is_api_path(&path) {
        api_headers(response.headers_mut());
    }
    ensure_retry_after(&mut response);
    if media && media_failed(&response) {
        web.playback.fail();
    }
    if let Some(guard) = guard {
        response = response.map(|body| {
            Body::new(Guarded {
                body,
                _guard: guard,
            })
        });
    }
    let status = response.status().as_u16();
    let elapsed = started.elapsed();
    if let Some(metrics) = &web.metrics {
        metrics.record(route_label(&response), status, elapsed);
    }
    tracing::info!(
        request_id = %id.0,
        %method,
        uri = %uri,
        status,
        ms = elapsed.as_millis() as u64,
        ip = peer.map(|p| p.to_string()).unwrap_or_default(),
        "request"
    );
    response
}

fn timeout_response(id: &RequestId) -> Response {
    let body = serde_json::json!({"error": {
        "code": "unavailable",
        "message": "The request took too long.",
        "request_id": id.0,
    }});
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

/// Headers on every response.
pub fn security_headers(headers: &mut HeaderMap) {
    headers
        .entry(header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(HeaderValue::from_static("nosniff"));
    headers
        .entry(header::REFERRER_POLICY)
        .or_insert(HeaderValue::from_static("no-referrer"));
}

/// Headers on every `/api/v1` response. Nothing there is meant to be framed or read by
/// another origin's pages; responses that set their own caching (media, artwork, library
/// reads with `ETag`s) keep it, everything else is `no-store`.
pub fn api_headers(headers: &mut HeaderMap) {
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(API_CSP));
    isolation_headers(headers);
}

/// Framing and cross-origin isolation, for the dashboard and the native API. Not on `/rest`,
/// where web Subsonic players on other origins load covers and audio.
fn isolation_headers(headers: &mut HeaderMap) {
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        http::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        http::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        http::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static(PERMISSIONS_POLICY),
    );
}

/// The dashboard's policy: everything from this origin, no inline code, never framed.
pub const DASHBOARD_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
/// API responses are data, never documents.
pub const API_CSP: &str = "default-src 'none'; frame-ancestors 'none'";
pub const PERMISSIONS_POLICY: &str = "camera=(), microphone=(), geolocation=(), interest-cohort=()";

/// Paths that send audio: streams, downloads, HLS, instant play, and Subsonic's media.
fn is_media_path(path: &str) -> bool {
    let api_media = path.contains("/api/v1/")
        && (path.ends_with("/stream")
            || path.ends_with("/download")
            || path.contains("/transcodes/")
            || path.contains("/acquisitions/") && path.contains("/stream"));
    let subsonic = ["/rest/stream", "/rest/download", "/rest/hls"]
        .iter()
        .any(|p| path.contains(p));
    api_media || subsonic
}

/// Whether a media response counts as a failure on the streams chart: a server error, or a
/// file that changed under the index. Client disconnects never get here.
fn media_failed(response: &Response) -> bool {
    response.status().is_server_error()
        || response
            .extensions()
            .get::<trackside_api::ErrorCode>()
            .is_some_and(|c| c.0 == "content_changed")
}

/// A response body that holds a playback slot until it is sent or dropped.
struct Guarded {
    body: Body,
    _guard: trackside_api::StreamGuard,
}

impl http_body::Body for Guarded {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// A `429` that reached here without saying when to come back gets a minute.
fn ensure_retry_after(response: &mut Response) {
    if response.status() == StatusCode::TOO_MANY_REQUESTS
        && !response.headers().contains_key(header::RETRY_AFTER)
    {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    }
}

fn is_api_path(path: &str) -> bool {
    path.contains("/api/v1/") || path.ends_with("/api/v1")
}

/// Compresses text formats only; see [`compressible`].
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonOnly;

/// Whether a content type is worth compressing: JSON, JSON lines, HTML, CSS, JavaScript,
/// SVG and plain text. Never audio, video, images, HLS playlists or segments, or event streams.
pub fn compressible(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "application/json"
            | "application/x-ndjson"
            | "text/html"
            | "text/css"
            | "text/javascript"
            | "application/javascript"
            | "text/plain"
            | "image/svg+xml"
    )
}

impl Predicate for JsonOnly {
    fn should_compress<B>(&self, response: &http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        let wanted = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(compressible);
        let small = axum::body::HttpBody::size_hint(response.body())
            .exact()
            .is_some_and(|n| n < 860);
        wanted && !small
    }
}

#[derive(rust_embed::Embed)]
#[folder = "../../dashboard/"]
#[exclude = "dev/*"]
#[exclude = "*.md"]
struct Dashboard;

/// The file a dashboard path serves: `/setup`, `/invite/<token>` and `/reset/<token>` are
/// their pages; `dev/` and docs are not served.
pub fn dashboard_file(path: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    let file = match path {
        "" | "index.html" => "index.html",
        "setup" | "setup/" => "setup.html",
        p if p.starts_with("invite/") && page_token(p) => "invite.html",
        p if p.starts_with("reset/") && page_token(p) => "reset.html",
        p => p,
    };
    let markdown = Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"));
    let hidden = file.starts_with("dev/")
        || markdown
        || file.split('/').any(|s| s == ".." || s.starts_with('.'));
    (!hidden).then(|| file.to_owned())
}

/// `invite/<token>`: one segment after the page name, not one of the page's assets.
fn page_token(path: &str) -> bool {
    let has_extension = Path::new(path).extension().is_some();
    path.matches('/').count() == 1 && !has_extension
}

async fn dashboard(method: Method, request_headers: HeaderMap, request: Request) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(file) = dashboard_file(request.uri().path()) else {
        return not_found_page();
    };
    let Some(found) = Dashboard::get(&file) else {
        return not_found_page();
    };
    let etag = found.metadata.sha256_hash()[..12]
        .iter()
        .fold(String::from("\""), |mut out, b| {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
            out
        })
        + "\"";
    let mut response = if request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        Body::from(found.data.into_owned()).into_response()
    };
    let headers = response.headers_mut();
    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    if let Ok(value) = HeaderValue::from_str(mime.essence_str()) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    // The pages can be reached with a token in their path, so they are never stored; the
    // assets revalidate by ETag.
    let page = Path::new(&file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("html"));
    let cache = if page { "no-store" } else { "no-cache" };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    dashboard_headers(headers);
    response
}

fn dashboard_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DASHBOARD_CSP),
    );
    isolation_headers(headers);
}

fn not_found_page() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "Not found").into_response();
    dashboard_headers(response.headers_mut());
    response
}

/// The dashboard as the router's fallback.
pub fn dashboard_router() -> Router {
    Router::new().fallback(dashboard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_paths_map_to_their_pages() {
        assert_eq!(dashboard_file("/").as_deref(), Some("index.html"));
        assert_eq!(dashboard_file("/setup").as_deref(), Some("setup.html"));
        assert_eq!(
            dashboard_file("/invite/abc123").as_deref(),
            Some("invite.html")
        );
        assert_eq!(
            dashboard_file("/reset/abc123").as_deref(),
            Some("reset.html")
        );
        assert_eq!(
            dashboard_file("/pages/users.js").as_deref(),
            Some("pages/users.js")
        );
        assert_eq!(dashboard_file("/dev/fixtures/me.json"), None);
        assert_eq!(dashboard_file("/README.md"), None);
        assert_eq!(dashboard_file("/../Cargo.toml"), None);
    }

    #[test]
    fn the_dashboard_is_embedded_without_dev_files_or_docs() {
        assert!(Dashboard::get("dev/preview.js").is_none());
        assert!(Dashboard::get("README.md").is_none());
        assert!(Dashboard::get("index.html").is_some());
        assert!(Dashboard::get("app.js").is_some());
    }

    #[tokio::test]
    async fn route_labels_are_the_matched_template_whatever_the_path_holds()
    -> Result<(), Box<dyn std::error::Error>> {
        use tower::ServiceExt;
        let router = Router::new()
            .route(
                "/api/v1/songs/{id}/stream",
                axum::routing::get(|| async { "ok" }),
            )
            .route_layer(axum::middleware::from_fn(mark_route))
            .fallback(|| async { StatusCode::NOT_FOUND });
        for path in [
            "/api/v1/songs/s7m2k4q9x1v8b3n6c0de/stream",
            "/api/v1/songs/short/stream",
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty())?)
                .await?;
            assert_eq!(route_label(&response), "/api/v1/songs/{id}/stream");
        }
        let unknown = router
            .oneshot(
                Request::builder()
                    .uri("/no/such/x7k2m9")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(route_label(&unknown), UNMATCHED_ROUTE);
        Ok(())
    }

    #[tokio::test]
    async fn a_timeout_body_names_the_request() -> Result<(), Box<dyn std::error::Error>> {
        let response = timeout_response(&RequestId(String::from("req-1")));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 4096).await?;
        let json: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(json["error"]["request_id"], "req-1");
        Ok(())
    }

    #[test]
    fn only_json_is_compressed() -> Result<(), http::Error> {
        let big = "x".repeat(2000);
        let json = http::Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(big.clone()))?;
        assert!(JsonOnly.should_compress(&json));
        let audio = http::Response::builder()
            .header(header::CONTENT_TYPE, "audio/flac")
            .body(Body::from(big.clone()))?;
        assert!(!JsonOnly.should_compress(&audio));
        let events = http::Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(big))?;
        assert!(!JsonOnly.should_compress(&events));
        for kind in [
            "audio/mp4",
            "video/mp4",
            "application/vnd.apple.mpegurl",
            "image/jpeg",
        ] {
            assert!(!compressible(kind), "{kind}");
        }
        assert!(compressible("text/css; charset=utf-8"));
        Ok(())
    }

    #[test]
    fn metrics_render_counters() {
        let metrics = Metrics::default();
        metrics.record("/healthz", 200, Duration::from_millis(3));
        metrics.record("/healthz", 200, Duration::from_millis(5));
        let mut out = String::new();
        metrics.render(&mut out);
        assert!(out.contains("trackside_http_requests_total{route=\"/healthz\",status=\"200\"} 2"));
        assert!(out.contains("trackside_http_request_seconds_count{route=\"/healthz\"} 2"));
    }
}
