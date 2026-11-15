//! Stored responses for `Idempotency-Key`, kept 24 hours per user.

use crate::db::{DbError, WriteTx};
use crate::ids::UserKey;
use crate::time::Millis;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResponse {
    /// SHA-256 of method, path and body, to detect a reused key.
    pub request_hash: [u8; 32],
    pub status: u16,
    pub body: Vec<u8>,
    pub created_at: Millis,
}

pub fn get(c: &Connection, user: UserKey, key: &str) -> Result<Option<StoredResponse>, DbError> {
    Ok(c.prepare_cached(
        "SELECT request_hash, response_status, response_body, created_at FROM idempotency_keys
         WHERE user_id = ?1 AND key = ?2",
    )?
    .query_row(params![user, key], |row| {
        let hash: Vec<u8> = row.get(0)?;
        let status: i64 = row.get(1)?;
        Ok(StoredResponse {
            request_hash: <[u8; 32]>::try_from(hash.as_slice()).unwrap_or([0; 32]),
            status: u16::try_from(status).unwrap_or(500),
            body: row.get(2)?,
            created_at: row.get(3)?,
        })
    })
    .optional()?)
}

/// Stores a response. A response already stored under the key is kept: the first one wins.
pub fn put(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    key: &str,
    response: &StoredResponse,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO idempotency_keys (user_id, key, request_hash, response_status, response_body,
             created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (user_id, key) DO NOTHING",
    )?
    .execute(params![
        user,
        key,
        response.request_hash.as_slice(),
        response.status,
        response.body,
        response.created_at
    ])?;
    Ok(())
}

/// Deletes responses created before `older_than`.
pub fn prune(tx: &mut WriteTx<'_>, older_than: Millis) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("DELETE FROM idempotency_keys WHERE created_at < ?1")?
        .execute([older_than])?;
    Ok(n as u64)
}
