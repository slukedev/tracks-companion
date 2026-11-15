//! Password reset links made by admins. Only the SHA-256 of the token is stored.

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::conflict_on_unique;
use crate::db::{DbError, WriteTx};
use crate::ids::UserKey;
use crate::time::Millis;

/// A reset as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordReset {
    pub id: i64,
    pub user: UserKey,
    pub created_by: Option<UserKey>,
    pub created_at: Millis,
    pub expires_at: Millis,
    pub used_at: Option<Millis>,
}

const SELECT: &str =
    "SELECT id, user_id, created_by, created_at, expires_at, used_at FROM password_resets";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PasswordReset> {
    Ok(PasswordReset {
        id: row.get(0)?,
        user: row.get(1)?,
        created_by: row.get(2)?,
        created_at: row.get(3)?,
        expires_at: row.get(4)?,
        used_at: row.get(5)?,
    })
}

pub fn insert(
    tx: &mut WriteTx<'_>,
    token_hash: &[u8; 32],
    user: UserKey,
    created_by: Option<UserKey>,
    expires_at: Millis,
) -> Result<PasswordReset, DbError> {
    tx.prepare_cached(
        "INSERT INTO password_resets (token_hash, user_id, created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        token_hash.as_slice(),
        user,
        created_by,
        tx.now(),
        expires_at
    ])
    .map_err(|e| conflict_on_unique(e, "reset exists"))?;
    let id = tx.last_insert_rowid();
    Ok(tx
        .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
        .query_row([id], map_row)?)
}

pub fn by_hash(c: &Connection, token_hash: &[u8; 32]) -> Result<Option<PasswordReset>, DbError> {
    Ok(
        c.prepare_cached(&format!("{SELECT} WHERE token_hash = ?1"))?
            .query_row([token_hash.as_slice()], map_row)
            .optional()?,
    )
}

/// Fails with `Conflict` if it was already used.
pub fn mark_used(tx: &mut WriteTx<'_>, id: i64) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE password_resets SET used_at = ?2 WHERE id = ?1 AND used_at IS NULL",
        )?
        .execute(params![id, tx.now()])?;
    if changed == 0 {
        return Err(DbError::Conflict(String::from("reset already used")));
    }
    Ok(())
}
