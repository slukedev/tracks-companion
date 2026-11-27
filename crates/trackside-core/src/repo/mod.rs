//! Repositories: plain functions over a connection. Every read of library data takes an
//! [`Access`](crate::Access) and filters by it; every write takes a
//! [`WriteTx`](crate::WriteTx) and records its changes for delta sync.

pub mod acquisitions;
pub mod albums;
pub mod annotations;
pub mod artists;
pub mod artwork;
pub mod audit;
pub mod bookmarks;
pub mod device_links;
pub mod devices;
pub mod enrichment;
pub mod folders;
pub mod genres;
pub mod health;
pub mod idempotency;
pub mod imports;
pub mod invites;
pub mod keys;
pub mod libraries;
pub mod listens;
pub mod lyrics;
pub mod pairings;
pub mod password_resets;
pub mod play_queue;
pub mod playlists;
pub mod ratings;
pub mod scan_runs;
pub mod sessions;
pub mod settings;
pub mod songs;
pub(crate) mod sql;
pub mod stars;
pub mod stats;
pub mod users;
