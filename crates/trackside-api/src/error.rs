//! The api.md section 2 error shape, and how every lower layer's errors map onto it.

use std::time::Duration;

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use trackside_auth::{AuthError, LinkError};
use trackside_core::DbError;
use trackside_core::assistant::{AssistantError, SpeakerError};
use trackside_core::control::{AcquireError, ScanControlError};
use trackside_core::sync::SyncError;
use trackside_media::MediaError;

use crate::wire::{ErrorBody, ErrorEnvelope};

tokio::task_local! {
    /// The current request's id, set by the request-context middleware so error bodies can
    /// repeat it.
    pub(crate) static REQUEST_ID: String;
}

/// An error answered with api.md's error shape.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub retry_after: Option<Duration>,
    /// For `416`: the length sent as `Content-Range: bytes */<len>`.
    pub unsatisfied_len: Option<u64>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
            retry_after: None,
            unsatisfied_len: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    /// `400 bad_request` naming the field at fault.
    pub fn bad_field(field: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
            .with_details(serde_json::json!({ "field": field }))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", "Not allowed.")
    }

    /// `403 reauth_required`: a sensitive action on a session whose password check is not
    /// recent; `POST /auth/reauth` and retry.
    pub fn reauth_required() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "reauth_required",
            "Confirm your password to do that.",
        )
    }

    /// `409 conflict` with `details.reason`.
    pub fn conflict(reason: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
            .with_details(serde_json::json!({ "reason": reason }))
    }

    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_state", message)
    }

    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "Not signed in, or the credentials are wrong.",
        )
    }

    pub fn library_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "library_unavailable",
            "The song's library is offline.",
        )
        .with_retry_after(Duration::from_secs(30))
    }

    pub fn internal(error: &dyn std::fmt::Display) -> Self {
        tracing::error!(%error, "request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error. The server log has the details.",
        )
    }

    /// The error as a response body, with the request id when there is one.
    pub fn envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            error: ErrorBody {
                code: self.code.to_owned(),
                message: self.message.clone(),
                details: self.details.clone(),
                request_id: REQUEST_ID.try_with(Clone::clone).ok(),
            },
        }
    }
}

/// The `error.code` of an error response, as a response extension for middleware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorCode(pub &'static str);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.status.as_u16(),
            self.code,
            self.message
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(self.envelope())).into_response();
        response.extensions_mut().insert(ErrorCode(self.code));
        if self.code == "https_required" {
            trackside_auth::extract::clear_plain_cookie(&mut response);
        }
        if let Some(after) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&after.as_secs().max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        if let Some(len) = self.unsatisfied_len
            && let Ok(value) = HeaderValue::from_str(&format!("bytes */{len}"))
        {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
        response
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::NotFound => Self::not_found(),
            DbError::Forbidden => Self::forbidden(),
            DbError::PreconditionFailed => Self::new(
                StatusCode::PRECONDITION_FAILED,
                "precondition_failed",
                "The revision moved; fetch it again and merge.",
            ),
            DbError::Conflict(reason) => conflict_from_reason(&reason),
            DbError::Invalid(message) => Self::bad_request(message),
            DbError::Full => Self::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_storage",
                "The data volume is full.",
            ),
            DbError::Closed => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "The server is shutting down.",
            ),
            other => Self::internal(&other),
        }
    }
}

/// Core's conflicts carry a reason in their text; the dashboard has messages for a few.
fn conflict_from_reason(reason: &str) -> ApiError {
    let lower = reason.to_ascii_lowercase();
    let code = if lower.contains("admin") {
        "last_admin"
    } else if lower.contains("username") {
        "username_taken"
    } else if lower.contains("running") {
        "running"
    } else {
        "conflict"
    };
    ApiError::conflict(code, reason.to_owned())
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        let (status, code) = error.status_and_code();
        match error {
            AuthError::Db(db) => db.into(),
            AuthError::RateLimited { retry_after } => {
                Self::new(status, code, "Too many attempts.").with_retry_after(retry_after)
            }
            AuthError::Conflict(reason) => conflict_from_reason(&reason),
            AuthError::Io(_) | AuthError::Unimplemented(_) => Self::internal(&error),
            other => Self::new(status, code, other.to_string()),
        }
    }
}

impl From<LinkError> for ApiError {
    fn from(error: LinkError) -> Self {
        match error {
            LinkError::NotFound => Self::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "No device is waiting with that code.",
            ),
            LinkError::Expired => Self::new(StatusCode::GONE, "link_expired", error.to_string()),
            LinkError::Used => Self::conflict("link_used", error.to_string()),
            LinkError::Locked => Self::new(StatusCode::GONE, "link_locked", error.to_string()),
            LinkError::Auth(auth) => auth.into(),
            device => polling_error(&device),
        }
    }
}

/// The device's answers while it polls, named as RFC 8628 names them.
fn polling_error(error: &LinkError) -> ApiError {
    match error {
        LinkError::Pending => ApiError::new(
            StatusCode::PRECONDITION_REQUIRED,
            "authorization_pending",
            error.to_string(),
        ),
        LinkError::SlowDown { interval } => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            error.to_string(),
        )
        .with_details(serde_json::json!({ "interval_s": interval.as_secs() }))
        .with_retry_after(*interval),
        LinkError::Denied => {
            ApiError::new(StatusCode::FORBIDDEN, "access_denied", error.to_string())
        }
        _ => ApiError::new(
            StatusCode::GONE,
            "expired_token",
            "Start again with a new code.",
        ),
    }
}

/// What the device hears while it polls: `expired_token` for a link that ended, whatever the
/// approver would have been told, and RFC 8628's other answers.
pub fn device_link_error(error: LinkError) -> ApiError {
    match error {
        LinkError::Expired | LinkError::NotFound | LinkError::Locked | LinkError::Used => {
            polling_error(&LinkError::Expired)
        }
        other => other.into(),
    }
}

impl From<SyncError> for ApiError {
    fn from(error: SyncError) -> Self {
        match error {
            SyncError::Reset => Self::new(
                StatusCode::GONE,
                "sync_reset",
                "This cursor can no longer be served; take a new snapshot.",
            ),
            SyncError::BadCursor => Self::bad_field("cursor", "The cursor is not valid."),
            SyncError::Db(db) => db.into(),
            other => Self::internal(&other),
        }
    }
}

impl From<AcquireError> for ApiError {
    fn from(error: AcquireError) -> Self {
        match error {
            AcquireError::NotFound | AcquireError::Disabled => Self::not_found(),
            AcquireError::QuotaExceeded { resets_at } => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "quota_exceeded",
                "The request quota is used up.",
            )
            .with_details(serde_json::json!({ "resets_at": resets_at.to_rfc3339() }))
            .with_retry_after(Duration::from_secs(60)),
            AcquireError::CandidateExpired => Self::new(
                StatusCode::GONE,
                "candidate_expired",
                "That candidate is no longer known; search again.",
            ),
            AcquireError::InvalidState => Self::invalid_state("The request has finished."),
            AcquireError::RateLimited { retry_after } => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Too many searches.",
            )
            .with_retry_after(retry_after),
            AcquireError::Invalid(message) => Self::bad_request(message),
            AcquireError::Busy => Self::conflict("running", "One is already running."),
            AcquireError::Unavailable { retry_after } => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Nothing can play this right now.",
            )
            .with_retry_after(retry_after),
            AcquireError::Db(db) => db.into(),
            other => Self::internal(&other),
        }
    }
}

impl From<MediaError> for ApiError {
    fn from(error: MediaError) -> Self {
        match error {
            MediaError::NotFound | MediaError::JobExpired => Self::not_found(),
            MediaError::ContentChanged => Self::new(
                StatusCode::CONFLICT,
                "content_changed",
                "The file changed on disk since it was indexed; retry shortly.",
            ),
            MediaError::RangeNotSatisfiable { len } => {
                let mut error = Self::new(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "range_not_satisfiable",
                    "The range is outside the file.",
                );
                error.details = Some(serde_json::json!({ "size": len }));
                error.unsatisfied_len = Some(len);
                error
            }
            MediaError::TranscoderBusy | MediaError::NoTranscoder => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "transcoder_busy",
                "No transcoder is free.",
            )
            .with_retry_after(Duration::from_secs(5)),
            MediaError::Timeout => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "transcoder_busy",
                "The transcoder has not produced this part yet.",
            )
            .with_retry_after(Duration::from_secs(1)),
            MediaError::TooLarge => {
                Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Too large.")
            }
            MediaError::Unsupported(message) => Self::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                message,
            ),
            MediaError::InsufficientStorage => Self::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_storage",
                "There is not enough free space for the transcode.",
            ),
            MediaError::LibraryUnavailable => Self::library_unavailable(),
            MediaError::Db(db) => db.into(),
            other => Self::internal(&other),
        }
    }
}

impl From<ScanControlError> for ApiError {
    fn from(error: ScanControlError) -> Self {
        match error {
            ScanControlError::UnknownLibrary | ScanControlError::UnknownImport => Self::not_found(),
            ScanControlError::Unavailable => Self::library_unavailable(),
            ScanControlError::NotWritable => {
                Self::conflict("not_writable", "The library is not writable.")
            }
            ScanControlError::Rejected(message) => Self::conflict("rejected", message),
            ScanControlError::ShuttingDown => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "The server is shutting down.",
            ),
            ScanControlError::Db(db) => db.into(),
            other => Self::internal(&other),
        }
    }
}

impl ApiError {
    /// `503 speakers_unavailable`: Music Assistant cannot be reached right now.
    pub fn speakers_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "speakers_unavailable",
            "Speakers aren't reachable right now.",
        )
        .with_retry_after(Duration::from_secs(10))
    }

    /// `503 catalog_unavailable`.
    pub fn catalog_unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "catalog_unavailable",
            "Music services aren't reachable right now.",
        )
        .with_retry_after(Duration::from_secs(10))
    }
}

impl From<SpeakerError> for ApiError {
    fn from(error: SpeakerError) -> Self {
        use serde_json::json;
        match error {
            SpeakerError::NotFound => Self::not_found(),
            SpeakerError::Unavailable => Self::speakers_unavailable(),
            SpeakerError::Offline => {
                Self::conflict("speaker_offline", "The speaker is unavailable.")
            }
            SpeakerError::PoweredOff => Self::conflict("powered_off", "The speaker is off."),
            SpeakerError::Unsupported {
                capability,
                speaker,
            } => {
                let mut details = json!({ "capability": capability });
                if let Some(speaker) = speaker {
                    details["speaker_id"] = json!(speaker.to_string());
                }
                Self::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported",
                    "This speaker can't do that.",
                )
                .with_details(details)
            }
            SpeakerError::NotPlayable { song, reference } => {
                let mut details = json!({ "reason": "not_playable" });
                if let Some(song) = song {
                    details["song_id"] = json!(song.to_string());
                }
                if let Some(reference) = reference {
                    details["ref"] = json!(reference);
                }
                Self::new(
                    StatusCode::CONFLICT,
                    "conflict",
                    "Nothing in the request can play.",
                )
                .with_details(details)
            }
            SpeakerError::ServiceSignedOut { service } => {
                let mut details = json!({ "reason": "service_signed_out" });
                if let Some(service) = service {
                    details["service_id"] = json!(service.to_string());
                }
                Self::new(
                    StatusCode::CONFLICT,
                    "conflict",
                    "The music service is signed out.",
                )
                .with_details(details)
            }
            SpeakerError::Invalid { field, message } => Self::bad_field(field, message),
            other => Self::internal(&other),
        }
    }
}

impl From<AssistantError> for ApiError {
    fn from(error: AssistantError) -> Self {
        match error {
            AssistantError::Invalid { field, message } => Self::bad_field(field, message),
            AssistantError::SetByConfig { field } => Self::new(
                StatusCode::CONFLICT,
                "conflict",
                format!("{field} is set in the configuration file or environment."),
            )
            .with_details(serde_json::json!({ "reason": "set_by_config", "field": field })),
            AssistantError::NotConfigured => {
                Self::conflict("not_set_up", "Music Assistant is not set up.")
            }
            AssistantError::Unavailable => Self::catalog_unavailable(),
            AssistantError::Failed(message) => Self::conflict("failed", message),
            other => Self::internal(&other),
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self::bad_request(format!("The JSON body is not valid: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_errors_map_to_their_statuses_and_headers() {
        let unsatisfiable =
            ApiError::from(MediaError::RangeNotSatisfiable { len: 1234 }).into_response();
        assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            unsatisfiable
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            Some("bytes */1234")
        );
        let timeout = ApiError::from(MediaError::Timeout).into_response();
        assert_eq!(timeout.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            timeout
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
        let busy = ApiError::from(MediaError::TranscoderBusy).into_response();
        assert!(busy.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(
            ApiError::from(MediaError::JobExpired).status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(MediaError::ContentChanged).code,
            "content_changed"
        );
        assert_eq!(
            ApiError::from(MediaError::InsufficientStorage).status,
            StatusCode::INSUFFICIENT_STORAGE
        );
        let offline = ApiError::from(MediaError::LibraryUnavailable).into_response();
        assert_eq!(offline.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            offline
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("30")
        );
    }
}
