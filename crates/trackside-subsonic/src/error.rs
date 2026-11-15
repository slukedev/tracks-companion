//! Subsonic errors: a code, a message, an optional `helpUrl`, and the HTTP status used when a
//! media request fails.

use std::time::Duration;

use http::StatusCode;
use trackside_auth::SubsonicAuthError;
use trackside_core::DbError;
use trackside_media::MediaError;

/// A failed request, sent as `<error code=".." message=".."/>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub code: u16,
    pub message: String,
    pub help_url: Option<String>,
    /// The status for media endpoints, which never send an error body with `200`. Other
    /// endpoints answer `200`, as Subsonic clients expect.
    pub media_status: StatusCode,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        let media_status = match code {
            10 => StatusCode::BAD_REQUEST,
            40..=44 => StatusCode::UNAUTHORIZED,
            50 => StatusCode::FORBIDDEN,
            70 => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            code,
            message: message.into(),
            help_url: None,
            media_status,
            retry_after: None,
        }
    }

    pub fn generic(message: impl Into<String>) -> Self {
        Self::new(0, message)
    }

    pub fn missing(name: &str) -> Self {
        Self::new(10, format!("Required parameter is missing: {name}"))
    }

    pub fn invalid(name: &str) -> Self {
        Self::new(0, format!("Invalid value for parameter {name}"))
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(70, format!("{what} not found"))
    }

    pub fn not_authorized(message: impl Into<String>) -> Self {
        Self::new(50, message)
    }

    pub fn not_supported(endpoint: &str) -> Self {
        Self::new(0, format!("{endpoint} is not supported by this server"))
    }

    pub fn unavailable(message: impl Into<String>, retry_after: Duration) -> Self {
        let mut error = Self::new(0, message);
        error.media_status = StatusCode::SERVICE_UNAVAILABLE;
        error.retry_after = Some(retry_after);
        error
    }

    #[must_use]
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.media_status = status;
        self
    }

    #[must_use]
    pub fn with_help_url(mut self, url: Option<String>) -> Self {
        self.help_url = url;
        self
    }

    fn internal(what: &dyn std::fmt::Display) -> Self {
        tracing::error!(error = %what, "subsonic request failed");
        Self::generic("Internal server error")
    }

    pub fn from_auth(error: SubsonicAuthError) -> Self {
        let message = match error {
            SubsonicAuthError::Missing => {
                String::from("Required parameter is missing: credentials")
            }
            SubsonicAuthError::WrongCredentials | SubsonicAuthError::RateLimited { .. } => {
                String::from("Wrong username or password")
            }
            SubsonicAuthError::TokenNotSupported => String::from(
                "Token and password authentication need an app password. Create one in the dashboard, or use an API key.",
            ),
            SubsonicAuthError::MechanismNotSupported => {
                String::from("Authentication mechanism not supported")
            }
            SubsonicAuthError::Conflicting => {
                String::from("Multiple conflicting authentication mechanisms provided")
            }
            SubsonicAuthError::InvalidApiKey => String::from("Invalid API key"),
            _ => String::from("Authentication failed"),
        };
        let mut api = Self::new(error.code(), message);
        if let SubsonicAuthError::RateLimited { retry_after } = error {
            api.retry_after = Some(retry_after);
            api.media_status = StatusCode::TOO_MANY_REQUESTS;
        }
        api
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::NotFound => Self::not_found("Item"),
            DbError::Forbidden => Self::not_authorized("Not allowed to change this item"),
            DbError::Invalid(message) | DbError::Conflict(message) => Self::generic(message),
            DbError::PreconditionFailed => Self::generic("The item changed meanwhile; try again"),
            DbError::Full => {
                Self::unavailable("The server's storage is full", Duration::from_secs(60))
            }
            DbError::Unimplemented(what) => Self::generic(format!("Not implemented yet: {what}")),
            other => Self::internal(&other),
        }
    }
}

impl From<MediaError> for ApiError {
    fn from(error: MediaError) -> Self {
        match error {
            MediaError::NotFound => Self::not_found("File"),
            MediaError::ContentChanged => Self::unavailable(
                "The file changed on disk since it was indexed; try again shortly",
                Duration::from_secs(5),
            ),
            MediaError::RangeNotSatisfiable { .. } => Self::generic("Range not satisfiable")
                .with_status(StatusCode::RANGE_NOT_SATISFIABLE),
            MediaError::TranscoderBusy | MediaError::Timeout => {
                Self::unavailable("The transcoder is busy", Duration::from_secs(5))
            }
            MediaError::NoTranscoder | MediaError::Unsupported(_) => {
                Self::generic("Transcoding is not available")
                    .with_status(StatusCode::NOT_IMPLEMENTED)
            }
            MediaError::JobExpired => Self::not_found("Transcode"),
            MediaError::Unimplemented(what) => {
                Self::generic(format!("Not implemented yet: {what}"))
            }
            MediaError::Db(db) => db.into(),
            other => Self::internal(&other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_status_follows_the_code() {
        assert_eq!(
            ApiError::not_found("Song").media_status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::new(40, "x").media_status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::missing("id").media_status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::from(MediaError::ContentChanged).media_status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn rate_limited_auth_is_40_with_retry_after() {
        let error = ApiError::from_auth(SubsonicAuthError::RateLimited {
            retry_after: Duration::from_secs(12),
        });
        assert_eq!(error.code, 40);
        assert_eq!(error.retry_after, Some(Duration::from_secs(12)));
    }

    #[test]
    fn db_not_found_is_70() {
        assert_eq!(ApiError::from(DbError::NotFound).code, 70);
        assert_eq!(ApiError::from(DbError::Forbidden).code, 50);
    }
}
