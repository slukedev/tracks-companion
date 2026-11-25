//! What the HTTP crates need from Last.fm enrichment without depending on it.
//! `trackside-enrich` implements [`EnrichControl`] and `trackside-server` wires it in.

use std::time::Duration;

use crate::config::Secret;
use crate::control::BoxFuture;
use crate::time::Millis;

/// The most lookups requests may have waiting at once; more are dropped until the worker
/// catches up, and asked for again by a later request.
pub const QUEUE_MAX: usize = 2000;

/// Something to look up on Last.fm. Only these names ever leave the server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Artist {
        /// `tracks_model::folded(name)`.
        identity: String,
        name: String,
    },
    Song {
        /// `tracks_model::song_key(title, artist)`.
        key: String,
        title: String,
        artist: String,
    },
    /// Cover art for a suggested song the server does not have.
    Art {
        /// `tracks_model::song_key(title, artist)`.
        key: String,
        title: String,
        artist: String,
    },
}

impl Target {
    pub fn artist(name: &str) -> Self {
        Self::Artist {
            identity: tracks_model::folded(name),
            name: name.to_owned(),
        }
    }

    pub fn song(title: &str, artist: &str) -> Self {
        Self::Song {
            key: tracks_model::song_key(title, artist),
            title: title.to_owned(),
            artist: artist.to_owned(),
        }
    }

    pub fn art(title: &str, artist: &str) -> Self {
        Self::Art {
            key: tracks_model::song_key(title, artist),
            title: title.to_owned(),
            artist: artist.to_owned(),
        }
    }
}

/// The dashboard's status line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnrichStatus {
    /// An API key is set.
    pub configured: bool,
    /// Artists and songs Last.fm knew.
    pub artists: u64,
    pub songs: u64,
    pub last_fetch: Option<Millis>,
    /// The latest failure, cleared by the next success.
    pub last_error: Option<(Millis, String)>,
}

/// The result of one test call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrichTest {
    pub ok: bool,
    pub elapsed: Duration,
    pub detail: Option<String>,
}

pub trait EnrichControl: Send + Sync + 'static {
    /// Looks up the targets that are not stored or have expired, first in line, and waits up
    /// to `wait` for them. Returns at once without a key. Requests never call this: they
    /// [`queue`](Self::queue) and answer from what is stored.
    fn ensure(&self, targets: Vec<Target>, wait: Duration) -> BoxFuture<'_, ()>;
    /// Puts the targets first in line without waiting: similar songs and artists first, art
    /// after them. Targets already stored and fresh are skipped by the worker. Does nothing
    /// without a key.
    fn queue(&self, targets: Vec<Target>);
    /// A new key from the dashboard, or none.
    fn set_key(&self, key: Option<Secret>);
    fn status(&self) -> BoxFuture<'_, EnrichStatus>;
    /// One call with `key`, or the current key when `None`.
    fn test(&self, key: Option<Secret>) -> BoxFuture<'_, EnrichTest>;
}

/// Enrichment switched off, for tests and tools.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoEnrich;

impl EnrichControl for NoEnrich {
    fn ensure(&self, _targets: Vec<Target>, _wait: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn queue(&self, _targets: Vec<Target>) {}

    fn set_key(&self, _key: Option<Secret>) {}

    fn status(&self) -> BoxFuture<'_, EnrichStatus> {
        Box::pin(async { EnrichStatus::default() })
    }

    fn test(&self, _key: Option<Secret>) -> BoxFuture<'_, EnrichTest> {
        Box::pin(async {
            EnrichTest {
                ok: false,
                elapsed: Duration::ZERO,
                detail: Some(String::from("No API key is set.")),
            }
        })
    }
}

/// Where cover art looked up for suggestions is kept: the media crate's artwork cache, which
/// checks the bytes and counts them against its size limit.
pub trait ExternalArtStore: Send + Sync + 'static {
    /// Whether the image is held.
    fn has(&self, id: &crate::ExternalArtId) -> bool;
    /// Checks and stores the image. Blocking. The error says why it was refused.
    fn put(&self, id: &crate::ExternalArtId, bytes: &[u8]) -> Result<(), String>;
}
