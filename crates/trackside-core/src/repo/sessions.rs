//! Dashboard sessions. Only the SHA-256 of the cookie value is stored.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::conflict_on_unique;
use crate::db::{DbError, WriteTx};
use crate::ids::UserKey;
use crate::time::Millis;

/// `sessions.id`. Shown to admins so they can end a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey(pub i64);

/// A session as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub key: SessionKey,
    pub user: UserKey,
    pub created_at: Millis,
    pub last_seen_at: Millis,
    pub idle_expires_at: Millis,
    pub expires_at: Millis,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// When the password was last checked for this session: at sign-in, or by a later
    /// re-authentication.
    pub recent_auth_at: Millis,
}

/// A session to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSession {
    pub token_hash: [u8; 32],
    pub user: UserKey,
    pub idle_expires_at: Millis,
    pub expires_at: Millis,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

const SELECT: &str =
    "SELECT id, user_id, created_at, last_seen_at, idle_expires_at, expires_at, ip,
        user_agent, coalesce(recent_auth_at, created_at) FROM sessions";

fn map_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        key: SessionKey(row.get(0)?),
        user: row.get(1)?,
        created_at: row.get(2)?,
        last_seen_at: row.get(3)?,
        idle_expires_at: row.get(4)?,
        expires_at: row.get(5)?,
        ip: row.get(6)?,
        user_agent: row.get(7)?,
        recent_auth_at: row.get(8)?,
    })
}

pub fn insert(tx: &mut WriteTx<'_>, new: &NewSession) -> Result<SessionRow, DbError> {
    let now = tx.now();
    tx.prepare_cached(
        "INSERT INTO sessions (token_hash, user_id, created_at, last_seen_at, idle_expires_at,
             expires_at, ip, user_agent)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        new.token_hash.as_slice(),
        new.user,
        now,
        new.idle_expires_at,
        new.expires_at,
        new.ip,
        new.user_agent
    ])
    .map_err(|e| conflict_on_unique(e, "session exists"))?;
    let key = tx.last_insert_rowid();
    Ok(tx
        .prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
        .query_row([key], map_row)?)
}

/// The session with this token hash, expired or not.
pub fn by_hash(c: &Connection, token_hash: &[u8; 32]) -> Result<Option<SessionRow>, DbError> {
    Ok(
        c.prepare_cached(&format!("{SELECT} WHERE token_hash = ?1"))?
            .query_row([token_hash.as_slice()], map_row)
            .optional()?,
    )
}

/// Records use and pushes the idle expiry out.
pub fn touch(
    tx: &mut WriteTx<'_>,
    key: SessionKey,
    idle_expires_at: Millis,
) -> Result<(), DbError> {
    tx.prepare_cached("UPDATE sessions SET last_seen_at = ?2, idle_expires_at = ?3 WHERE id = ?1")?
        .execute(params![key.0, tx.now(), idle_expires_at])?;
    Ok(())
}

/// Records that the session's user just proved their password again.
pub fn mark_recent_auth(tx: &mut WriteTx<'_>, key: SessionKey) -> Result<(), DbError> {
    tx.prepare_cached("UPDATE sessions SET recent_auth_at = ?2 WHERE id = ?1")?
        .execute(params![key.0, tx.now()])?;
    Ok(())
}

pub fn delete(tx: &mut WriteTx<'_>, key: SessionKey) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM sessions WHERE id = ?1")?
        .execute([key.0])?;
    Ok(())
}

/// Ends a user's sessions except `keep`. Returns how many ended.
pub fn delete_for_user(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    keep: Option<SessionKey>,
) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("DELETE FROM sessions WHERE user_id = ?1 AND id IS NOT ?2")?
        .execute(params![user, keep.map(|k| k.0)])?;
    Ok(n as u64)
}

/// Deletes sessions past either expiry.
pub fn prune(tx: &mut WriteTx<'_>, now: Millis) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("DELETE FROM sessions WHERE expires_at <= ?1 OR idle_expires_at <= ?1")?
        .execute([now])?;
    Ok(n as u64)
}

/// One user's sessions, or everyone's for `None`, most recently used first.
pub fn list(c: &Connection, user: Option<UserKey>) -> Result<Vec<SessionRow>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "{SELECT} WHERE ?1 IS NULL OR user_id = ?1 ORDER BY last_seen_at DESC, id DESC"
    ))?;
    let rows = statement.query_map([user], map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}
