//! Server settings stored as JSON in the `settings` table.

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::db::{DbError, WriteTx};

/// Keys with fixed meaning. Dashboard-editable configuration keys use their dotted config
/// names (architecture section 11).
pub mod keys {
    /// UUID string: the server's identity in pairing, mDNS and capabilities.
    pub const SERVER_ID: &str = "server.id";
    /// `"needs_admin"` or `"done"`.
    pub const SETUP_STATE: &str = "setup.state";
    /// Highest `change_log` seq whose tombstones were pruned.
    pub const SYNC_PRUNED_THROUGH: &str = "sync.pruned_through";
    /// [`crate::TRACKS_MODEL_FOLDING`] the stored keys were built with.
    pub const SCHEMA_TRACKS_MODEL: &str = "schema.tracks_model";
    /// Pid of the writable library uploads go to.
    pub const UPLOADS_LIBRARY: &str = "uploads.library";
    /// Pid of the writable library acquisitions go to.
    pub const ACQUIRE_LIBRARY: &str = "acquire.library";
}

/// The value of `key`, or `None` when unset.
pub fn get<T: DeserializeOwned>(c: &Connection, key: &str) -> Result<Option<T>, DbError> {
    let raw: Option<String> = c
        .prepare_cached("SELECT value FROM settings WHERE key = ?1")?
        .query_row([key], |row| row.get(0))
        .optional()?;
    raw.map(|raw| {
        serde_json::from_str(&raw).map_err(|e| DbError::Invalid(format!("setting {key}: {e}")))
    })
    .transpose()
}

/// Sets `key`.
pub fn put<T: Serialize + ?Sized>(
    tx: &mut WriteTx<'_>,
    key: &str,
    value: &T,
) -> Result<(), DbError> {
    let json = serde_json::to_string(value)
        .map_err(|e| DbError::Invalid(format!("setting {key}: {e}")))?;
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )?
    .execute(rusqlite::params![key, json, now])?;
    Ok(())
}

/// Removes `key`, returning to its default.
pub fn delete(tx: &mut WriteTx<'_>, key: &str) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM settings WHERE key = ?1")?
        .execute([key])?;
    Ok(())
}

/// Every stored setting, by key.
pub fn all(c: &Connection) -> Result<Vec<(String, serde_json::Value)>, DbError> {
    let mut statement = c.prepare_cached("SELECT key, value FROM settings ORDER BY key")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (key, raw) = row?;
        let value = serde_json::from_str(&raw)
            .map_err(|e| DbError::Invalid(format!("setting {key}: {e}")))?;
        out.push((key, value));
    }
    Ok(out)
}

/// The server's id, written at first start.
pub fn server_id(c: &Connection) -> Result<uuid::Uuid, DbError> {
    let text: String = get(c, keys::SERVER_ID)?.ok_or(DbError::NotFound)?;
    uuid::Uuid::parse_str(&text).map_err(|e| DbError::Invalid(format!("server.id: {e}")))
}
