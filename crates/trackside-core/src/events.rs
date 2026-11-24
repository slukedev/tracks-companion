//! The in-process event bus. [`crate::Db::write`] publishes [`ServerEvent::Changed`] after
//! every commit that recorded changes; the scanner and acquirer publish their progress. The
//! SSE endpoint filters events by [`Audience`] before sending them to a client.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::control::AcquisitionView;
use crate::ids::{DeviceKey, ImportId, LibraryId, LibraryKey, ScanRunId, Seq, SongId, UserKey};
use crate::records::{ImportState, ScanCounts, ScanState};

/// Who may see an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Everyone who can see `library`, or everyone at all when it is `None`.
    Everyone { library: Option<LibraryKey> },
    /// One user.
    User(UserKey),
    /// Admins only.
    Admins,
}

/// A scan's progress, sent to admins at most once a second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub library: LibraryId,
    pub run: ScanRunId,
    pub state: ScanState,
    pub counts: ScanCounts,
}

/// Something happened that a client may want to know about.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ServerEvent {
    /// A write committed changes up to `seq`, visible to `audiences`.
    Changed { seq: Seq, audiences: Vec<Audience> },
    /// A library became available or unavailable.
    LibraryState { library: LibraryId, available: bool },
    /// Scan progress (admins only).
    Scan(ScanProgress),
    /// An acquisition changed state or progressed. The owner sees it, and admins see all.
    Acquisition {
        user: UserKey,
        view: AcquisitionView,
    },
    /// An upload changed state.
    Upload {
        user: UserKey,
        import: ImportId,
        state: ImportState,
        song: Option<SongId>,
    },
    /// A credential was revoked: the user's sessions and keys, or one device's.
    CredentialRevoked {
        user: UserKey,
        device: Option<DeviceKey>,
    },
    /// A user's role, permissions or library grants changed: what they may see is read again.
    AccessChanged { user: UserKey },
    /// The addresses the server can be reached at changed; `version` names the new list.
    ServerUrls { version: String },
    /// A user's Trackside Cloud data changed: their clients should pull `entity`. Never carries
    /// the data itself.
    CloudChanged { user: UserKey, entity: &'static str },
}

/// A broadcast channel of [`ServerEvent`]s. Cloning shares the channel.
#[derive(Clone, Debug)]
pub struct EventBus {
    sender: broadcast::Sender<Arc<ServerEvent>>,
}

impl EventBus {
    /// Default capacity: a subscriber more than this many events behind gets `Lagged` and
    /// should send the client `resync`.
    pub const DEFAULT_CAPACITY: usize = 1024;

    /// A bus holding up to `capacity` unread events per subscriber.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// Sends an event to every current subscriber. Nobody listening is not an error.
    pub fn publish(&self, event: ServerEvent) {
        let _ = self.sender.send(Arc::new(event));
    }

    /// Receives events published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ServerEvent>> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}
