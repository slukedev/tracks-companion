//! Axum extractors. The router state must give access to `Arc<Auth>` through `FromRef`.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;

use crate::{Auth, AuthError, Principal, RouteClass, Scopes, Via};

/// An API route caller with the scope bits `SCOPE`, e.g. `Authed<{ Scopes::WRITE.bits() }>`.
/// `request` also needs the user's `can_request`, and `download` the user's `can_download`.
#[derive(Clone, Debug)]
pub struct Authed<const SCOPE: u8>(pub Principal);

/// A media route caller: header, cookie or `mt`, with the `stream` scope.
#[derive(Clone, Debug)]
pub struct MediaAuthed(pub Principal);

/// An admin's browser session, with CSRF checked on changes.
#[derive(Clone, Debug)]
pub struct AdminSession(pub Principal);

/// Any signed-in browser session, with CSRF checked on changes.
#[derive(Clone, Debug)]
pub struct SessionUser(pub Principal);

/// A request on which a password or key may be sent: over HTTPS, or from a nearby client
/// (see [`Auth::secrets_allowed`]). Otherwise `403 https_required`, before anything is checked.
#[derive(Clone, Copy, Debug)]
pub struct SecureTransport;

/// The client's address after trusted-proxy handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

impl AuthError {
    /// The HTTP status and api.md error code.
    pub fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthenticated => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            Self::MediaTokenExpired => (StatusCode::UNAUTHORIZED, "media_token_expired"),
            Self::Forbidden | Self::Db(trackside_core::DbError::Forbidden) => {
                (StatusCode::FORBIDDEN, "forbidden")
            }
            Self::Csrf => (StatusCode::FORBIDDEN, "csrf"),
            Self::SetupRequired => (StatusCode::FORBIDDEN, "setup_required"),
            Self::HttpsRequired => (StatusCode::FORBIDDEN, "https_required"),
            Self::CredentialInQuery | Self::Invalid(_) => {
                (StatusCode::BAD_REQUEST, self.bad_request_code())
            }
            Self::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::NotFound | Self::Db(trackside_core::DbError::NotFound) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Self::Conflict(_) | Self::Db(trackside_core::DbError::Conflict(_)) => {
                (StatusCode::CONFLICT, "conflict")
            }
            Self::Db(trackside_core::DbError::Full) => {
                (StatusCode::INSUFFICIENT_STORAGE, "insufficient_storage")
            }
            Self::Db(trackside_core::DbError::Invalid(_)) => {
                (StatusCode::BAD_REQUEST, "bad_request")
            }
            Self::Db(_) | Self::Io(_) | Self::Unimplemented(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        }
    }

    fn bad_request_code(&self) -> &'static str {
        if matches!(self, Self::CredentialInQuery) {
            "credential_in_query"
        } else {
            "bad_request"
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "auth failed internally");
            String::from("internal error")
        } else {
            self.to_string()
        };
        let body = serde_json::json!({"error": {"code": code, "message": message}});
        let mut response = (status, axum::Json(body)).into_response();
        if matches!(self, Self::HttpsRequired) {
            clear_plain_cookie(&mut response);
        }
        if let Self::RateLimited { retry_after } = self
            && let Ok(value) =
                http::HeaderValue::from_str(&retry_after.as_secs().max(1).to_string())
        {
            response
                .headers_mut()
                .insert(http::header::RETRY_AFTER, value);
        }
        response
    }
}

/// Clears the plain session cookie, which `https_required` refuses: the browser stops sending
/// a session over plain HTTP that the server will not accept there.
pub fn clear_plain_cookie(response: &mut Response) {
    if let Ok(value) = http::HeaderValue::from_str(&crate::SessionCookie::clear_cookie(false)) {
        response
            .headers_mut()
            .append(http::header::SET_COOKIE, value);
    }
}

fn peer_ip(auth: &Auth, parts: &Parts) -> IpAddr {
    // Without ConnectInfo (a router driven directly in tests) there is no peer to name.
    match parts.extensions.get::<ConnectInfo<SocketAddr>>() {
        Some(ConnectInfo(peer)) => auth.client_ip(parts, *peer),
        None => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

async fn principal<S>(
    parts: &mut Parts,
    state: &S,
    class: RouteClass,
) -> Result<Principal, AuthError>
where
    Arc<Auth>: FromRef<S>,
{
    let auth = Arc::<Auth>::from_ref(state);
    let ip = peer_ip(&auth, parts);
    auth.authenticate(parts, ip, class).await
}

impl<S, const SCOPE: u8> FromRequestParts<S> for Authed<SCOPE>
where
    S: Send + Sync,
    Arc<Auth>: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AuthError> {
        let principal = principal(parts, state, RouteClass::Api).await?;
        if !principal.allows(Scopes::from_bits(SCOPE)) {
            return Err(AuthError::Forbidden);
        }
        Ok(Self(principal))
    }
}

impl<S> FromRequestParts<S> for MediaAuthed
where
    S: Send + Sync,
    Arc<Auth>: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AuthError> {
        let principal = principal(parts, state, RouteClass::Media).await?;
        if !principal.scopes.contains(Scopes::STREAM) {
            return Err(AuthError::Forbidden);
        }
        Ok(Self(principal))
    }
}

impl<S> FromRequestParts<S> for AdminSession
where
    S: Send + Sync,
    Arc<Auth>: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AuthError> {
        let principal = principal(parts, state, RouteClass::Admin).await?;
        if principal.via != Via::Session || !principal.is_admin() {
            return Err(AuthError::Forbidden);
        }
        Ok(Self(principal))
    }
}

impl<S> FromRequestParts<S> for SessionUser
where
    S: Send + Sync,
    Arc<Auth>: FromRef<S>,
{
    type Rejection = AuthError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AuthError> {
        let principal = principal(parts, state, RouteClass::Api).await?;
        if principal.via != Via::Session {
            return Err(AuthError::Forbidden);
        }
        Ok(Self(principal))
    }
}

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
    Arc<Auth>: FromRef<S>,
{
    type Rejection = AuthError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl Future<Output = Result<Self, AuthError>> + Send {
        let auth = Arc::<Auth>::from_ref(state);
        std::future::ready(Ok(Self(peer_ip(&auth, parts))))
    }
}

impl<S> FromRequestParts<S> for SecureTransport
where
    S: Send + Sync,
    Arc<Auth>: FromRef<S>,
{
    type Rejection = AuthError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl Future<Output = Result<Self, AuthError>> + Send {
        let auth = Arc::<Auth>::from_ref(state);
        std::future::ready(auth.require_secure_transport(parts).map(|()| Self))
    }
}
