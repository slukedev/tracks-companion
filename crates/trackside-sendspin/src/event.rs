use std::time::Duration;

use crate::frame::{AudioChunk, FrameError};
use crate::noise::NoiseError;
use crate::wire::{GroupUpdate, StreamFormat};

/// Where a connection attempt was when it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Connect,
    Auth,
    Init,
    Handshake,
    Hello,
}

/// Why a connection failed or ended. None of them carries the token or a key.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("timed out at {0:?}")]
    Timeout(Stage),
    /// Music Assistant's proxy refused the token (close code 4001). Not retried.
    #[error("the token was refused: {0}")]
    Auth(String),
    /// `server/error` in place of `server/init`. Not retried.
    #[error("the server refused the connection: {0}")]
    Rejected(String),
    #[error("the handshake failed: {0}")]
    Noise(#[from] NoiseError),
    #[error("a framing error: {0}")]
    Frame(#[from] FrameError),
    #[error("a protocol error: {0}")]
    Protocol(String),
    /// The server sent an activation this client may not accept and was told so with
    /// `client/goodbye`. Not retried.
    #[error("the server asked for something this client does not allow: {0}")]
    Unauthorized(String),
    #[error("the connection closed")]
    Closed,
    #[error("the connection stopped answering")]
    Dead,
    #[error("could not set up TLS: {0}")]
    Tls(String),
}

impl Error {
    /// Whether trying again could help.
    pub fn is_retryable(&self) -> bool {
        !matches!(
            self,
            Self::Auth(_) | Self::Rejected(_) | Self::Unauthorized(_)
        )
    }
}

/// Why the client says goodbye (`client/goodbye`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoodbyeReason {
    /// Trackside is shutting down. Music Assistant keeps the player for a while.
    Shutdown,
    Restart,
    /// The relay session ended. Music Assistant removes the connection at once.
    UserRequest,
}

impl GoodbyeReason {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Restart => "restart",
            Self::UserRequest => "user_request",
        }
    }
}

/// What the client reports to its [`Handler`], in the order it happened on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The handshake and hello exchange finished.
    Connected {
        server_id: String,
        server_name: String,
        /// Counts connections since the client started, from 1.
        generation: u64,
        /// A reconnect within Music Assistant's retention window: the same player came back.
        /// Music Assistant stopped playback when the connection dropped.
        warm: bool,
    },
    /// `server/activate`: whether the player role is active and playback declared.
    Activated {
        player: bool,
        playback: bool,
    },
    /// The clock has its first measurement; timestamps map to local time from now on.
    Synced,
    /// `stream/start` for the player. `in_place` when a stream was already running.
    StreamStart {
        format: StreamFormat,
        in_place: bool,
    },
    Audio(AudioChunk),
    /// `stream/clear` for the player. With `from_timestamp`, only audio at or after it goes.
    Clear {
        from_timestamp: Option<i64>,
    },
    /// `stream/end` for the player, or the player role went away during a stream.
    End,
    Group(GroupUpdate),
    /// The connection dropped, which also ends any stream; the client tries again after
    /// `retry_in`. Music Assistant stops the player's playback when its connection drops.
    Disconnected {
        error: Error,
        retry_in: Duration,
    },
    /// The client stopped for good, which also ends any stream: shut down, said goodbye, or
    /// gave up (`error`).
    Stopped {
        error: Option<Error>,
    },
}

/// Receives the client's events. Called on the connection task: it must return quickly and
/// never block, so a slow consumer cannot stall the socket, time sync or control messages.
pub trait Handler: Send + Sync + 'static {
    fn handle(&self, event: Event);
}

impl<F: Fn(Event) + Send + Sync + 'static> Handler for F {
    fn handle(&self, event: Event) {
        self(event);
    }
}
