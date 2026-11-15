//! What the HTTP crates need from Last.fm enrichment without depending on it.
//! `trackside-enrich` implements [`EnrichControl`] and `trackside-server` wires it in.

use std::time::Duration;

use crate::config::Secret;
use crate::control::BoxFuture;
use crate::time::Millis;

/// How long a request waits for a lookup it started before answering from what is stored.
pub const ON_DEMAND_WAIT: Duration = Duration::from_millis(1500);

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
    /// to `wait` for them. Returns at once without a key.
    fn ensure(&self, targets: Vec<Target>, wait: Duration) -> BoxFuture<'_, ()>;
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
