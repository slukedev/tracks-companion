//! Errors. None of them ever carries the token.

use std::time::Duration;

/// Music Assistant's error codes (models `errors.py`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCode {
    Generic,
    ProviderUnavailable,
    MediaNotFound,
    InvalidData,
    AlreadyRegistered,
    SetupFailed,
    LoginFailed,
    AudioError,
    QueueEmpty,
    UnsupportedFeature,
    PlayerUnavailable,
    PlayerCommandFailed,
    InvalidCommand,
    UnplayableMedia,
    InvalidProviderUri,
    InvalidProviderId,
    RetriesExhausted,
    ResourceTemporarilyUnavailable,
    ProviderPermissionDenied,
    ActionUnavailable,
    AuthenticationRequired,
    AuthenticationFailed,
    InsufficientPermissions,
    InvalidToken,
    ResourceBusy,
    RateLimited,
    UnsupportedSystem,
    UserNotFound,
    InvalidConfigValue,
    ProviderStreamHeld,
    /// 503, sent before the first user exists.
    SetupRequired,
    /// 999, an unexpected exception on the server.
    Unexpected,
    Other(i64),
}

impl ErrorCode {
    const TABLE: [(i64, Self); 32] = [
        (0, Self::Generic),
        (1, Self::ProviderUnavailable),
        (2, Self::MediaNotFound),
        (3, Self::InvalidData),
        (4, Self::AlreadyRegistered),
        (5, Self::SetupFailed),
        (6, Self::LoginFailed),
        (7, Self::AudioError),
        (8, Self::QueueEmpty),
        (9, Self::UnsupportedFeature),
        (10, Self::PlayerUnavailable),
        (11, Self::PlayerCommandFailed),
        (12, Self::InvalidCommand),
        (13, Self::UnplayableMedia),
        (14, Self::InvalidProviderUri),
        (15, Self::InvalidProviderId),
        (16, Self::RetriesExhausted),
        (17, Self::ResourceTemporarilyUnavailable),
        (18, Self::ProviderPermissionDenied),
        (19, Self::ActionUnavailable),
        (20, Self::AuthenticationRequired),
        (21, Self::AuthenticationFailed),
        (22, Self::InsufficientPermissions),
        (23, Self::InvalidToken),
        (24, Self::ResourceBusy),
        (25, Self::RateLimited),
        (26, Self::UnsupportedSystem),
        (27, Self::UserNotFound),
        (28, Self::InvalidConfigValue),
        (29, Self::ProviderStreamHeld),
        (503, Self::SetupRequired),
        (999, Self::Unexpected),
    ];

    pub fn from_code(code: i64) -> Self {
        Self::TABLE
            .iter()
            .find(|(c, _)| *c == code)
            .map_or(Self::Other(code), |(_, e)| *e)
    }

    pub fn code(self) -> i64 {
        match self {
            Self::Other(code) => code,
            known => Self::TABLE
                .iter()
                .find(|(_, e)| *e == known)
                .map_or(0, |(c, _)| *c),
        }
    }

    /// Whether the token was missing, wrong, expired or revoked.
    pub fn is_auth(self) -> bool {
        matches!(
            self,
            Self::AuthenticationRequired | Self::AuthenticationFailed | Self::InvalidToken
        )
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code())
    }
}

/// An error answer from Music Assistant (`{error_code, details}`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Music Assistant error {code}: {}", details.as_deref().unwrap_or("no details"))]
pub struct ServerError {
    pub code: ErrorCode,
    pub details: Option<String>,
}

/// Why a command got no usable answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RequestError {
    #[error("not connected to Music Assistant")]
    NotConnected,
    #[error("the connection to Music Assistant closed before the answer arrived")]
    ConnectionLost,
    #[error("Music Assistant did not answer within {0:?}")]
    Timeout(Duration),
    #[error("too many Music Assistant requests in flight")]
    Busy,
    #[error(transparent)]
    Server(#[from] ServerError),
    #[error("could not decode the answer to {command}: {source}")]
    Decode {
        command: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("the answer to {command} has more than {limit} items")]
    TooLarge { command: String, limit: usize },
    #[error("{0}")]
    InvalidArgument(&'static str),
    #[error("the Music Assistant client has shut down")]
    ShutDown,
}

impl RequestError {
    /// The server's error code, for an error answer.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Server(e) => Some(e.code),
            _ => None,
        }
    }

    /// Whether the same request could succeed later without any change.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::NotConnected | Self::ConnectionLost | Self::Timeout(_) | Self::Busy => true,
            Self::Server(e) => matches!(
                e.code,
                ErrorCode::ProviderUnavailable
                    | ErrorCode::ResourceTemporarilyUnavailable
                    | ErrorCode::ResourceBusy
                    | ErrorCode::RateLimited
                    | ErrorCode::RetriesExhausted
            ),
            _ => false,
        }
    }
}

/// Which step of connecting timed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// TCP, TLS and the WebSocket upgrade.
    Connect,
    /// Waiting for the server-info greeting.
    Greeting,
    /// Waiting for the answer to `auth`.
    Auth,
}

/// Why the token was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Music Assistant refused the token ({code}: {})", details.as_deref().unwrap_or("no details"))]
pub struct AuthFailure {
    pub code: ErrorCode,
    pub details: Option<String>,
}

impl AuthFailure {
    /// Invalid, expired or revoked, as opposed to refused for another reason.
    pub fn is_invalid_token(&self) -> bool {
        self.code.is_auth()
    }
}

/// Which side is too old.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mismatch {
    /// The server predates the oldest schema this client supports.
    ServerTooOld,
    /// The server no longer supports the schema this client speaks.
    ClientTooOld,
}

/// The server's API schema is one this client cannot work with.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "Music Assistant {server_version} (schema {schema_version}, supports clients from {min_supported_schema_version}) is not compatible: {}",
    match mismatch { Mismatch::ServerTooOld => "the server is too old", Mismatch::ClientTooOld => "Trackside is too old" }
)]
pub struct Incompatibility {
    pub server_version: String,
    pub schema_version: u32,
    pub min_supported_schema_version: u32,
    pub mismatch: Mismatch,
}

/// Why a connection could not be set up. Clonable so the connection state can carry it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    #[error("could not reach Music Assistant: {0}")]
    Network(String),
    #[error("TLS failed: {0}")]
    Tls(String),
    #[error("Music Assistant answered the upgrade with HTTP {0}")]
    Http(u16),
    #[error("timed out ({0:?})")]
    Timeout(Stage),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("Music Assistant has no users yet; finish its setup first")]
    SetupRequired,
    #[error(transparent)]
    Auth(AuthFailure),
    #[error(transparent)]
    Incompatible(Incompatibility),
    #[error("no token is configured")]
    NoToken,
    #[error("the connection closed during the handshake")]
    Closed,
    #[error("the connection dropped: {0}")]
    Dropped(String),
    #[error("Music Assistant stopped answering")]
    Unresponsive,
}

impl ConnectError {
    pub(crate) fn from_ws(error: tokio_tungstenite::tungstenite::Error) -> Self {
        use tokio_tungstenite::tungstenite::Error as E;
        match error {
            E::Io(e) => Self::Network(e.to_string()),
            E::Tls(e) => Self::Tls(e.to_string()),
            E::Http(response) => Self::Http(response.status().as_u16()),
            E::ConnectionClosed | E::AlreadyClosed => Self::Closed,
            E::Url(e) => Self::Network(e.to_string()),
            other => Self::Protocol(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    #[test]
    fn codes_round_trip() {
        for code in [0, 10, 23, 29, 503, 999, 4242] {
            assert_eq!(ErrorCode::from_code(code).code(), code);
        }
    }

    #[test]
    fn auth_codes_are_recognised() {
        assert!(ErrorCode::from_code(23).is_auth());
        assert!(ErrorCode::from_code(20).is_auth());
        assert!(!ErrorCode::from_code(22).is_auth());
    }
}
