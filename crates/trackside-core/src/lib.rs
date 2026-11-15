//! Trackside's core: ids, records, the database and its repositories, delta sync, search,
//! similarity, configuration, the event bus, and the traits the HTTP crates use to reach the
//! scanner and the acquirer.
//!
//! SQL lives only here. Other crates call repository functions with a `&Connection` from
//! [`Db::read`] or a [`WriteTx`] from [`Db::write`].

pub mod access;
pub mod config;
pub mod control;
pub mod db;
pub mod enrich;
pub mod events;
pub mod ids;
pub mod ingest;
pub mod now_playing;
pub mod records;
pub mod repo;
pub mod search;
pub mod similar;
pub mod sync;
pub mod time;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use access::{Access, LibrarySet, Permissions, Role};
pub use config::{Config, ConfigReport, ConfigSources};
pub use control::{AcquireControl, BoxFuture, ScanControl};
pub use db::{Change, ChangeKind, ChangeOp, Db, DbError, DbOptions, WriteTx};
pub use enrich::{EnrichControl, NoEnrich};
pub use events::{Audience, EventBus, ServerEvent};
pub use ids::{
    AcquisitionId, AlbumId, ArtistId, ArtworkId, AudioDigest, CandidateToken, DeviceId, DeviceKey,
    FolderId, IdError, ImportId, InviteId, KeyId, LibraryId, LibraryKey, PairingId, PlaylistId,
    PublicId, ScanRunId, Seq, SongId, UserId, UserKey, random_id,
};
pub use records::Page;
pub use time::{Clock, Millis, SystemClock, TestClock};

/// The version of `tracks-model` folding that stored keys (`song_key`, `album_key`,
/// `identity`, `search_text`) were built with. Stored in `settings.schema.tracks_model`; a
/// change to folding output bumps it and ships with a migration that recomputes the keys.
pub const TRACKS_MODEL_FOLDING: u32 = 1;
