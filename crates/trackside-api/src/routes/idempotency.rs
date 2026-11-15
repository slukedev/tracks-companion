//! `Idempotency-Key` for creating `POST`s (api.md section 3): the first successful response is
//! stored for 24 hours per user and replayed for a repeat of the same request; the same key on
//! a different request is `409 idempotency_mismatch`.

use std::future::Future;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use sha2::{Digest, Sha256};
use trackside_core::UserKey;
use trackside_core::repo::idempotency::{self, StoredResponse};

use super::{read, write};
use crate::ApiState;
use crate::error::{ApiError, ApiResult};

const HEADER: &str = "idempotency-key";
const MAX_KEY_LEN: usize = 200;

/// The hash a stored response is matched by: method, path and body.
pub fn request_hash(method: &Method, path: &str, body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(method.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(body);
    hasher.finalize().into()
}

/// The client's key, if it sent a usable one.
pub fn key(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(HEADER) else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|t| !t.is_empty() && t.len() <= MAX_KEY_LEN)
        .ok_or_else(|| {
            ApiError::bad_field("Idempotency-Key", "The Idempotency-Key is not valid.")
        })?;
    Ok(Some(text.to_owned()))
}

/// What a stored response means for a new request with the same key.
#[derive(Debug, PartialEq, Eq)]
pub enum Replay {
    Replay { status: u16, body: Vec<u8> },
    Mismatch,
}

pub fn compare(stored: &StoredResponse, hash: &[u8; 32]) -> Replay {
    if stored.request_hash == *hash {
        Replay::Replay {
            status: stored.status,
            body: stored.body.clone(),
        }
    } else {
        Replay::Mismatch
    }
}

fn replayed(status: u16, body: Vec<u8>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let mut response = (status, Body::from(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert("idempotent-replayed", HeaderValue::from_static("true"));
    response
}

/// Runs `create` once per key: replays a stored success, refuses a reused key, stores a new
/// success. `create` returns the status and the JSON body.
pub async fn once<F, Fut>(
    state: &ApiState,
    user: UserKey,
    headers: &HeaderMap,
    method: &Method,
    path: &str,
    body: &[u8],
    create: F,
) -> ApiResult<Response>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ApiResult<(StatusCode, serde_json::Value)>>,
{
    let Some(key) = key(headers)? else {
        let (status, value) = create().await?;
        return Ok((status, axum::Json(value)).into_response());
    };
    let hash = request_hash(method, path, body);
    let lookup = key.clone();
    let stored = read(state, move |c| idempotency::get(c, user, &lookup)).await?;
    if let Some(stored) = stored {
        return match compare(&stored, &hash) {
            Replay::Replay { status, body } => Ok(replayed(status, body)),
            Replay::Mismatch => Err(ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_mismatch",
                "This Idempotency-Key was used for a different request.",
            )),
        };
    }
    let (status, value) = create().await?;
    let bytes = serde_json::to_vec(&value).map_err(|e| ApiError::internal(&e))?;
    let response = StoredResponse {
        request_hash: hash,
        status: status.as_u16(),
        body: bytes.clone(),
        created_at: state.db.clock().now(),
    };
    if let Err(e) = write(state, move |tx| idempotency::put(tx, user, &key, &response)).await {
        tracing::warn!(error = %e, "idempotent response not stored");
    }
    let mut out = (status, Body::from(bytes)).into_response();
    out.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trackside_core::Millis;

    #[test]
    fn same_request_replays_and_a_different_one_mismatches() {
        let hash = request_hash(&Method::POST, "/api/v1/playlists", b"{\"name\":\"a\"}");
        let stored = StoredResponse {
            request_hash: hash,
            status: 201,
            body: b"{}".to_vec(),
            created_at: Millis(0),
        };
        assert_eq!(
            compare(&stored, &hash),
            Replay::Replay {
                status: 201,
                body: b"{}".to_vec()
            }
        );
        let other = request_hash(&Method::POST, "/api/v1/playlists", b"{\"name\":\"b\"}");
        assert_eq!(compare(&stored, &other), Replay::Mismatch);
        let other_path = request_hash(&Method::POST, "/api/v1/uploads", b"{\"name\":\"a\"}");
        assert_ne!(hash, other_path);
    }

    #[test]
    fn keys_must_be_short_text() {
        let mut headers = HeaderMap::new();
        assert!(matches!(key(&headers), Ok(None)));
        headers.insert(HEADER, HeaderValue::from_static("6F1D9C2A"));
        assert!(matches!(key(&headers), Ok(Some(k)) if k == "6F1D9C2A"));
        let long = "x".repeat(MAX_KEY_LEN + 1);
        headers.insert(
            HEADER,
            HeaderValue::from_str(&long).unwrap_or(HeaderValue::from_static("")),
        );
        assert!(key(&headers).is_err());
    }
}
