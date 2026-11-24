//! Trackside Cloud: per-user sync of the Tracks apps' own data (plays, sessions, stations,
//! settings, playlists, likes and the per-item lists), as an alternative to iCloud (ADR 0012).
//!
//! Every record belongs to one user and is keyed by the client's id. Each write takes the
//! next value of that user's change seq, which is both the record's `version` (for
//! compare-and-set) and the pull cursor. The merge rules live in [`merge`] and [`settings`]
//! and are pure; [`store`] runs them on the single writer in short chunks, so a large import
//! never holds the writer for long. The HTTP routes in `trackside-api` are thin.
//!
//! The server never interprets the apps' fields beyond what a merge rule needs, never shows
//! them to anyone but their owner, and stores client-encrypted values ([`envelope`]) as
//! opaque blobs.

pub mod devices;
pub mod entity;
pub mod envelope;
pub mod epoch;
pub mod limits;
pub mod merge;
pub mod model;
mod repo;
pub mod settings;
pub mod store;
pub mod validate;

pub use devices::{CloudDevice, Registry};
pub use entity::Entity;
pub use epoch::Cursor;
pub use limits::{Limits, PushLimiter};
pub use model::{
    Body, CloudError, Page, PushItem, PushOutcome, PushResult, Record, Reject, Usage, UsageEntry,
};
pub use settings::{Setting, SettingOutcome, SettingResult, SettingWrite, SettingsPage};
