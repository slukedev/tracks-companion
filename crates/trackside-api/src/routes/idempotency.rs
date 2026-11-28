//! `Idempotency-Key` for creating `POST`s (api.md section 3): the first successful response is
//! stored for 24 hours per user and replayed for a repeat of the same request; the same key on
//! a different request is `409 idempotency_mismatch`.

use std::future::Future;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use trackside_core::repo::idempotency::{self, StoredResponse};
use trackside_core::{Millis, UserKey};

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

/// Where replays are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    Database,
    /// In memory only, for answers that carry streaming-service metadata (ADR-0013): lost on
    /// restart, which only makes a late retry run again.
    Memory,
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
    run(
        Store::Database,
        state,
        user,
        headers,
        method,
        path,
        body,
        create,
    )
    .await
}

/// [`once`] keeping the replay in memory, never on disk: for speaker commands and the relay,
/// whose answers name what a streaming service plays.
pub async fn once_in_memory<F, Fut>(
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
    run(
        Store::Memory,
        state,
        user,
        headers,
        method,
        path,
        body,
        create,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "once's arguments and where to keep the replay"
)]
async fn run<F, Fut>(
    store: Store,
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
    let now = state.db.clock().now();
    let stored = match store {
        Store::Database => {
            let lookup = key.clone();
            read(state, move |c| idempotency::get(c, user, &lookup)).await?
        }
        Store::Memory => state.local.idempotent.get(user, &key, now),
    };
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
        created_at: now,
    };
    match store {
        Store::Database => {
            if let Err(e) =
                write(state, move |tx| idempotency::put(tx, user, &key, &response)).await
            {
                tracing::warn!(error = %e, "idempotent response not stored");
            }
        }
        Store::Memory => state.local.idempotent.put(user, key, response),
    }
    let mut out = (status, Body::from(bytes)).into_response();
    out.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(out)
}

/// Replays kept in memory, 24 hours like the stored ones, and at most [`MEMORY_CAP`].
#[derive(Debug, Default)]
pub struct MemoryReplays(std::sync::Mutex<HashMap<(UserKey, String), StoredResponse>>);

const MEMORY_CAP: usize = 4096;
const KEPT_MS: i64 = 24 * 3600 * 1000;

impl MemoryReplays {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(UserKey, String), StoredResponse>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn get(&self, user: UserKey, key: &str, now: Millis) -> Option<StoredResponse> {
        self.lock()
            .get(&(user, key.to_owned()))
            .filter(|r| now.0 - r.created_at.0 < KEPT_MS)
            .cloned()
    }

    fn put(&self, user: UserKey, key: String, response: StoredResponse) {
        let mut map = self.lock();
        let now = response.created_at;
        map.retain(|_, r| now.0 - r.created_at.0 < KEPT_MS);
        if map.len() >= MEMORY_CAP
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, r)| r.created_at)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.entry((user, key)).or_insert(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
