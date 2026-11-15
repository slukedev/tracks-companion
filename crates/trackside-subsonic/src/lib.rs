//! The Subsonic 1.16.1 API with `OpenSubsonic` extensions under `/rest`, for clients other than
//! Tracks. It maps core records to Subsonic shapes and holds no domain logic of its own.
//!
//! Every endpoint answers `GET` and `POST` (formPost), with or without the `.view` suffix, in
//! XML (the default) or JSON. Ids are the native public ids.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::request::Parts;
use http::{HeaderValue, StatusCode, header};
use trackside_auth::extract::ClientIp;
use trackside_auth::{Auth, Principal, Scopes, SubsonicCredentials};
use trackside_core::now_playing::NowPlayingRegistry;
use trackside_core::{Access, Config, Db, DbError, Millis, ScanControl};
use trackside_media::Media;

mod error;
mod handlers;
mod ids;
mod lyrics;
mod map;
mod params;
mod response;
mod state;

pub use error::ApiError;
pub use response::{API_VERSION, SERVER_TYPE};

use params::Params;
use response::{Body, Format, render};
use state::Inner;

/// What the `/rest` handlers need.
#[derive(Clone)]
pub struct SubsonicState {
    pub db: Db,
    pub auth: Arc<Auth>,
    pub media: Arc<Media>,
    pub scan: Arc<dyn ScanControl>,
    pub config: Arc<Config>,
    pub server_id: uuid::Uuid,
}

impl std::fmt::Debug for SubsonicState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubsonicState")
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

impl axum::extract::FromRef<SubsonicState> for Arc<Auth> {
    fn from_ref(state: &SubsonicState) -> Self {
        Arc::clone(&state.auth)
    }
}

/// The largest formPost body accepted.
const MAX_FORM_BYTES: usize = 1 << 20;

/// The Subsonic routes with their full paths (`/rest/...`), merged into the server's router,
/// with a now-playing registry of their own.
pub fn router(state: SubsonicState) -> Router {
    router_with(state, NowPlayingRegistry::new())
}

/// [`router`] sharing `now_playing` with the native API, so `getNowPlaying` shows what Tracks
/// reports to `POST /api/v1/now-playing` and the native view shows Subsonic players.
pub fn router_with(state: SubsonicState, now_playing: NowPlayingRegistry) -> Router {
    let inner = Arc::new(Inner {
        state,
        now_playing,
        sessions: state::Sessions::default(),
        cursors: state::SongCursors::default(),
    });
    Router::new()
        .route("/rest/{endpoint}", get(dispatch).post(dispatch))
        .with_state(state::Shared(inner))
}

/// What a handler produces: a Subsonic body, or a raw response for binary endpoints.
pub(crate) enum Reply {
    Body(Body),
    Raw(Response),
}

impl From<Body> for Reply {
    fn from(body: Body) -> Self {
        Self::Body(body)
    }
}

/// One authenticated call.
pub(crate) struct Call<'a> {
    pub inner: &'a Arc<Inner>,
    pub parts: &'a Parts,
    pub params: &'a Params,
    pub principal: Principal,
}

impl Call<'_> {
    pub fn state(&self) -> &SubsonicState {
        &self.inner.state
    }

    pub fn access(&self) -> Access {
        self.principal.access.clone()
    }

    pub fn now(&self) -> Millis {
        self.state().db.clock().now()
    }

    /// The `c` parameter, naming the client.
    pub fn client(&self) -> String {
        self.params.get("c").unwrap_or("Subsonic client").to_owned()
    }

    pub fn is_admin(&self) -> bool {
        self.principal.access.is_admin()
    }

    /// Error 50 unless the credential carries `scope`.
    pub fn require(&self, scope: Scopes) -> Result<(), ApiError> {
        if self.principal.scopes.contains(scope) {
            Ok(())
        } else {
            Err(ApiError::not_authorized(format!(
                "This credential lacks the {scope} scope"
            )))
        }
    }

    /// Runs `f` on a reader connection with the caller's access.
    pub async fn read<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection, &Access) -> Result<T, ApiError> + Send + 'static,
    {
        let access = self.access();
        self.state().db.read(move |c| Ok(f(c, &access))).await?
    }

    /// Runs `f` in a write transaction with the caller's access. An error from `f` rolls the
    /// whole transaction back.
    pub async fn write<T, F>(&self, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&mut trackside_core::WriteTx<'_>, &Access) -> Result<T, ApiError>
            + Send
            + 'static,
    {
        let access = self.access();
        let failure = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&failure);
        let result = self
            .state()
            .db
            .write(move |tx| {
                f(tx, &access).map_err(|error| {
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
                    DbError::Invalid(String::from("rolled back"))
                })
            })
            .await;
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                let api = failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                Err(api.unwrap_or_else(|| error.into()))
            }
        }
    }
}

/// Binary endpoints, whose errors carry a non-200 status.
fn is_media(endpoint: &str) -> bool {
    matches!(
        endpoint,
        "stream" | "download" | "getcoverart" | "hls" | "getavatar"
    )
}

/// The help page for errors 41 and 42: the dashboard's account page, where app passwords and
/// API keys are made.
fn help_url(state: &SubsonicState, parts: &Parts) -> Option<String> {
    let base = state.config.server.base_path.trim_end_matches('/');
    if let Some(public) = state.config.server.public_urls.first() {
        return Some(format!("{}{base}/#/account", public.trim_end_matches('/')));
    }
    let host = parts.headers.get(header::HOST)?.to_str().ok()?;
    let scheme = parts.uri.scheme_str().unwrap_or("http");
    Some(format!("{scheme}://{host}{base}/#/account"))
}

async fn authenticate(
    inner: &Inner,
    parts: &Parts,
    params: &Params,
    ip: IpAddr,
) -> Result<Principal, ApiError> {
    let credentials = SubsonicCredentials {
        username: params.get("u").map(str::to_owned),
        token: params.get("t").map(str::to_owned),
        salt: params.get("s").map(str::to_owned),
        password: params.get("p").map(str::to_owned),
        api_key: params.get("apiKey").map(str::to_owned),
    };
    let legacy = credentials.username.is_some()
        || credentials.password.is_some()
        || credentials.token.is_some()
        || credentials.salt.is_some();
    if credentials.api_key.is_some() && legacy {
        return Err(ApiError::new(
            43,
            "Multiple conflicting authentication mechanisms provided",
        ));
    }
    if credentials.api_key.is_none() {
        let has_secret = credentials.password.is_some()
            || (credentials.token.is_some() && credentials.salt.is_some());
        if credentials.username.is_none() {
            return Err(ApiError::missing("u or apiKey"));
        }
        if !has_secret {
            return Err(ApiError::missing("p, or t and s"));
        }
    }
    inner
        .state
        .auth
        .authenticate_subsonic(&credentials, ip)
        .await
        .map_err(|error| {
            let api = ApiError::from_auth(error);
            let help = matches!(api.code, 41 | 42).then(|| help_url(&inner.state, parts));
            api.with_help_url(help.flatten())
        })
}

fn body_response(body: &Body, format: Format, status: StatusCode) -> Response {
    let text = render(body, format);
    let mut response = (status, text).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(format.content_type()),
    );
    response
}

fn error_response(
    error: ApiError,
    format: Format,
    media: bool,
    status: Option<StatusCode>,
) -> Response {
    let status = status.unwrap_or(if media {
        error.media_status
    } else {
        StatusCode::OK
    });
    let retry_after = error.retry_after;
    let mut response = body_response(&Body::Error(error), format, status);
    if let Some(after) = retry_after
        && let Ok(value) = HeaderValue::from_str(&after.as_secs().max(1).to_string())
    {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

async fn dispatch(
    State(state::Shared(inner)): State<state::Shared>,
    Path(endpoint): Path<String>,
    ClientIp(ip): ClientIp,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let mut params = Params::from_encoded(parts.uri.query().unwrap_or(""));
    let is_form = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if parts.method == http::Method::POST && is_form {
        match axum::body::to_bytes(body, MAX_FORM_BYTES).await {
            Ok(bytes) => params.extend_from_encoded(&String::from_utf8_lossy(&bytes)),
            Err(_) => {
                return error_response(
                    ApiError::generic("The request body is too large"),
                    Format::parse(params.get("f")),
                    false,
                    Some(StatusCode::PAYLOAD_TOO_LARGE),
                );
            }
        }
    }
    let format = Format::parse(params.get("f"));
    let mut name = endpoint
        .strip_suffix(".view")
        .unwrap_or(&endpoint)
        .to_ascii_lowercase();
    // The Subsonic API documents the playlist as `hls.m3u8`, and players ask for it so.
    if name == "hls.m3u8" {
        name.truncate(3);
    }
    let media = is_media(&name);
    if format == Format::Jsonp {
        return error_response(
            ApiError::generic("jsonp is not supported; use f=json"),
            format,
            media,
            None,
        );
    }
    if !handlers::is_known(&name) {
        return error_response(
            ApiError::generic(format!("Unknown endpoint {endpoint}")),
            format,
            false,
            Some(StatusCode::NOT_FOUND),
        );
    }
    if name == "getopensubsonicextensions" {
        return body_response(&handlers::system::extensions(), format, StatusCode::OK);
    }
    let principal = match authenticate(&inner, &parts, &params, ip).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error, format, media, None),
    };
    let call = Call {
        inner: &inner,
        parts: &parts,
        params: &params,
        principal,
    };
    match handlers::handle(&name, &call).await {
        Ok(Reply::Body(body)) => body_response(&body, format, StatusCode::OK),
        Ok(Reply::Raw(response)) => response,
        Err(error) => error_response(error, format, media, None),
    }
}

/// A raw response with a body and content type, for `hls` playlists.
pub(crate) fn raw(content_type: &'static str, body: String) -> Response {
    let mut response = Response::new(HttpBody::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// How long a client waits before retrying a song whose library is offline.
pub(crate) const LIBRARY_RETRY: Duration = Duration::from_secs(30);
