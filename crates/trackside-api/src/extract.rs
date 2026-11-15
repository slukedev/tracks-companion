//! Request helpers: JSON bodies and queries that fail with api.md errors, id parsing, and the
//! request-context middleware (request ids, error bodies for every refusal).

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::de::DeserializeOwned;
use trackside_core::PublicId;

use crate::error::{ApiError, REQUEST_ID};

/// The request id as the server's outer middleware chose it (honouring a trusted proxy's
/// `X-Request-Id`). When absent, the API makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(pub String);

impl RequestId {
    /// A new random id.
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

pub const X_REQUEST_ID: &str = "x-request-id";

/// A JSON request body; a malformed one is `400 bad_request`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|rejection| body_rejection(rejection.status()))?;
        parse_json(&bytes).map(Json)
    }
}

/// An optional JSON body: an empty body is `T::default()`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OptionalJson<T>(pub T);

impl<S, T> FromRequest<S> for OptionalJson<T>
where
    T: DeserializeOwned + Default,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|rejection| body_rejection(rejection.status()))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self(T::default()));
        }
        parse_json(&bytes).map(OptionalJson)
    }
}

fn body_rejection(status: StatusCode) -> ApiError {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(status, "too_large", "The request body is over the limit.")
    } else {
        ApiError::bad_request("The request body could not be read.")
    }
}

/// Parses a JSON body, naming the field at fault when serde can.
pub fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| {
        let message = format!("The JSON body is not valid: {e}");
        match field_of(&e.to_string()) {
            Some(field) => ApiError::bad_field(&field, message),
            None => ApiError::bad_request(message),
        }
    })
}

fn field_of(message: &str) -> Option<String> {
    let start = message.find("field `")? + "field `".len();
    let end = message.get(start..)?.find('`')? + start;
    message.get(start..end).map(str::to_owned)
}

/// A query string; a malformed one is `400 bad_request`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<S, T> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        let query = parts.uri.query().unwrap_or_default();
        std::future::ready(serde_urlencoded_like(query).map(Query))
    }
}

fn serde_urlencoded_like<T: DeserializeOwned>(query: &str) -> Result<T, ApiError> {
    axum::extract::Query::<T>::try_from_uri(
        &format!("/?{query}")
            .parse()
            .map_err(|_| ApiError::bad_request("The query string is not valid."))?,
    )
    .map(|q| q.0)
    .map_err(|e| ApiError::bad_request(format!("The query string is not valid: {e}")))
}

/// A path id of kind `T`. A malformed id names nothing the caller can see: `404`.
pub fn path_id<T: PublicId>(raw: &str) -> Result<T, ApiError> {
    T::parse(raw).map_err(|_| ApiError::not_found())
}

/// A body id of kind `T`; a malformed one is `400` naming `field`.
pub fn body_id<T: PublicId>(raw: &str, field: &str) -> Result<T, ApiError> {
    T::parse(raw).map_err(|e| ApiError::bad_field(field, format!("{field}: {e}")))
}

/// Middleware for every `/api/v1` request: picks the request id, makes it available to error
/// bodies, echoes it in `X-Request-Id`, and turns refusals that are not yet in api.md's shape
/// (axum's own 404 and 405, extractor rejections) into it.
pub async fn request_context(mut request: Request, next: Next) -> Response {
    let id = match request.extensions().get::<RequestId>() {
        Some(id) => id.clone(),
        None => {
            let id = RequestId::new();
            request.extensions_mut().insert(id.clone());
            id
        }
    };
    let response = REQUEST_ID.scope(id.0.clone(), next.run(request)).await;
    let mut response = REQUEST_ID
        .scope(id.0.clone(), normalize_error(response))
        .await;
    if let Ok(value) = HeaderValue::from_str(&id.0) {
        response.headers_mut().insert(X_REQUEST_ID, value);
    }
    response
}

const ERROR_BODY_LIMIT: usize = 64 * 1024;

async fn normalize_error(response: Response) -> Response {
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let is_json = is_json(response.headers());
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, ERROR_BODY_LIMIT).await else {
        return ApiError::new(status, code_for_status(status), "Request failed.").into_response();
    };
    if is_json
        && let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(error) = value.get_mut("error").and_then(|e| e.as_object_mut())
    {
        if !error.contains_key("request_id")
            && let Ok(id) = REQUEST_ID.try_with(Clone::clone)
        {
            error.insert(String::from("request_id"), serde_json::Value::String(id));
        }
        let body = serde_json::to_vec(&value).unwrap_or_default();
        let mut parts = parts;
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::from(body));
    }
    let message = String::from_utf8_lossy(&bytes).trim().to_owned();
    let message = if message.is_empty() {
        status
            .canonical_reason()
            .unwrap_or("Request failed.")
            .to_owned()
    } else {
        message
    };
    let mut fresh = ApiError::new(status, code_for_status(status), message).into_response();
    for name in [header::RETRY_AFTER, header::ALLOW, header::WWW_AUTHENTICATE] {
        if let Some(value) = parts.headers.get(&name) {
            fresh.headers_mut().insert(name, value.clone());
        }
    }
    fresh
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

fn code_for_status(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST
        | StatusCode::UNPROCESSABLE_ENTITY
        | StatusCode::LENGTH_REQUIRED => "bad_request",
        StatusCode::UNAUTHORIZED => "unauthenticated",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::CONFLICT => "conflict",
        StatusCode::PRECONDITION_FAILED => "precondition_failed",
        StatusCode::PAYLOAD_TOO_LARGE => "too_large",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
        StatusCode::RANGE_NOT_SATISFIABLE => "range_not_satisfiable",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        StatusCode::INSUFFICIENT_STORAGE => "insufficient_storage",
        _ => "internal",
    }
}

/// A JSON response with a status.
pub fn json_with<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    (status, axum::Json(value)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_field_names_are_found() {
        assert_eq!(
            field_of("missing field `username` at line 1 column 2"),
            Some(String::from("username"))
        );
        assert_eq!(field_of("expected value at line 1"), None);
    }

    #[test]
    fn statuses_map_to_codes() {
        assert_eq!(
            code_for_status(StatusCode::METHOD_NOT_ALLOWED),
            "method_not_allowed"
        );
        assert_eq!(code_for_status(StatusCode::IM_A_TEAPOT), "internal");
    }
}
