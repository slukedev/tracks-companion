//! What clients push and pull, as the domain sees it. The wire shapes are in `trackside-api`.

use serde_json::{Map, Value};
use trackside_core::{DbError, Millis};

use crate::entity::Entity;

/// The fields of a record: the app's own JSON object, stored and returned as it came, apart
/// from what the entity's merge rule decides.
pub type Fields = Map<String, Value>;

/// One item of a push batch.
#[derive(Clone, Debug, PartialEq)]
pub struct PushItem {
    pub id: String,
    /// Compare-and-set: the version the client last saw, or 0 for "must not exist yet".
    pub base_version: Option<i64>,
    pub fields: Option<Fields>,
    pub client_modified_at: Millis,
    /// The app's own id for the device making the change.
    pub device_id: String,
    /// Set for a delete (a tombstone).
    pub deleted_at: Option<Millis>,
    /// For a play deleted because the client merged it into another.
    pub merged_into: Option<String>,
    /// An explicit undelete: an upsert of a tombstoned id is refused without it.
    pub undelete: bool,
}

impl PushItem {
    pub fn is_delete(&self) -> bool {
        self.deleted_at.is_some()
    }
}

/// A record as pulled, or as the current copy in a conflict.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: String,
    pub version: i64,
    pub device_id: String,
    pub client_modified_at: Millis,
    pub body: Body,
}

/// A live record's fields, or a tombstone.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Live(Fields),
    Deleted {
        deleted_at: Millis,
        merged_into: Option<String>,
    },
}

/// Why an item was refused. The client drops it rather than retrying, except for
/// `QuotaExceeded`, which clears when the user frees space or the admin raises the quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Malformed: a required field missing, a wrong type, a bad id.
    Invalid,
    /// Over a size limit (fields, a string, a setting).
    TooLarge,
    /// The id is tombstoned and the item is not an explicit undelete.
    Deleted,
    /// A session written by a device that neither owns it nor was restored from its owner.
    NotOwner,
    /// A delete of something that only grows (forgotten songs).
    GrowOnly,
    /// The user's quota is full.
    QuotaExceeded,
    /// Dated more than a day ahead of the server's clock.
    Future,
    /// The id is not the key the fields give (`stationKey(name)`, `songIdentity`).
    IdentityMismatch,
    /// A secret setting sent without client-side encryption.
    EncryptionRequired,
}

impl Reject {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::TooLarge => "too_large",
            Self::Deleted => "deleted",
            Self::NotOwner => "not_owner",
            Self::GrowOnly => "grow_only",
            Self::QuotaExceeded => "quota_exceeded",
            Self::Future => "future",
            Self::IdentityMismatch => "identity_mismatch",
            Self::EncryptionRequired => "encryption_required",
        }
    }
}

/// What happened to one pushed item.
#[derive(Clone, Debug, PartialEq)]
pub enum PushOutcome {
    /// Stored at `version`.
    Accepted { version: i64 },
    /// Already stored; nothing changed. Success for the client.
    Duplicate { version: i64 },
    /// The stored copy wins, or `base_version` did not match. `current` is the stored copy,
    /// absent when there is none.
    Conflict { current: Option<Record> },
    Rejected { reason: Reject },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PushResult {
    /// The record's id: for a keyed entity pushed with an empty id, the key the server derived.
    pub id: String,
    pub outcome: PushOutcome,
}

/// One page of a pull.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub upserts: Vec<Record>,
    pub deletes: Vec<Record>,
    /// The cursor to send next: the highest version on this page, or the user's current seq
    /// on the last page.
    pub cursor: i64,
    pub has_more: bool,
}

/// What a user stores in Trackside Cloud: the totals and each entity's share. Numbers only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub items: u64,
    pub bytes: u64,
    pub entities: Vec<UsageEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageEntry {
    pub entity: Entity,
    pub items: u64,
    pub bytes: u64,
}

/// A failure of a whole request rather than of one item.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error(transparent)]
    Db(#[from] DbError),
    /// The cursor is older than pruned tombstones, or newer than anything this server has
    /// (a restored backup): pull from an empty cursor.
    #[error("the cursor can no longer be served")]
    SyncReset,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("too many items")]
    TooMany,
}
