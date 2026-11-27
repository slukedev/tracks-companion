//! A client for Music Assistant's WebSocket API, built to run unattended for years.
//!
//! [`Client::start`] keeps one connection to `ws(s)://host:port/ws` up: it reads the
//! server-info greeting, checks the API schema (2.10 stable and 2.11 beta), authenticates with
//! a long-lived token, and reconnects with jittered exponential backoff when the connection
//! drops or stops answering pings. One task owns the socket; callers send commands through a
//! channel, matched to answers by `message_id`, with a timeout and a bound on how many are in
//! flight. Chunked (`partial`) answers are assembled.
//!
//! Events fan out to any number of [`Subscription`]s. After every connect, and whenever a
//! subscriber lags or an event cannot be decoded, every subscriber gets a full [`Snapshot`] of
//! players and queues, so no state is silently lost. The connection state is a `watch`
//! channel ([`Client::state`]).
//!
//! The token never appears in `Debug` output, logs or errors.
//!
//! Music Assistant itself offers no download or audio URL for provider tracks, and this crate
//! adds none: it controls playback and reads the catalogue.

mod api;
mod backoff;
mod client;
mod connection;
mod de;
mod endpoint;
mod error;
mod event;
mod image;
pub mod models;
mod options;
mod protocol;
pub mod schema;
#[cfg(feature = "testing")]
pub mod testing;
mod token;

pub use api::{
    LibraryQuery, OPENSUBSONIC_DOMAIN, OpenSubsonicSource, PlayMedia, PlayerCommand, SearchQuery,
    SetupError,
};
pub use backoff::Backoff;
pub use client::{Client, ConnectionState, Session, Subscription, probe};
pub use endpoint::{DEFAULT_PORT, Endpoint, EndpointError};
pub use error::{
    AuthFailure, ConnectError, ErrorCode, Incompatibility, Mismatch, RequestError, ServerError,
    Stage,
};
pub use event::{Event, RawEvent, Snapshot, SnapshotReason, Update};
pub use image::{ImageFormat, ImageSize, image_url};
pub use options::{CaError, CustomCa, Options};
pub use token::Token;
