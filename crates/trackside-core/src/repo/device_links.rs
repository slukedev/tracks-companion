//! Device links (RFC 8628 shape): started by a device that shows a short user code, decided by
//! someone signed in, and collected by the device with its device code. Only hashes of the
//! codes are stored; the auth crate makes and checks them.

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::{conflict_on_unique, hash32, to_u32};
use crate::db::{DbError, WriteTx};
use crate::ids::UserKey;
use crate::records::text_enum;
use crate::repo::devices::NewDevice;
use crate::time::Millis;

text_enum!(
    /// Where a link is. Expiry is read from `expires_at`, not stored. `Locked`: too many wrong
    /// codes reached it, so it cannot be approved any more. `Completed`: the device collected
    /// its key.
    LinkState {
        Pending => "pending",
        Approved => "approved",
        Denied => "denied",
        Locked => "locked",
        Completed => "completed",
    }
);

/// A link as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkRow {
    pub key: i64,
    pub device_code_hash: [u8; 32],
    pub user_code_hash: [u8; 32],
    pub device: NewDevice,
    /// Started from a nearby address (the home network, loopback, the tailnet).
    pub nearby: bool,
    pub state: LinkState,
    /// Who approved or denied it.
    pub user: Option<UserKey>,
    /// The scopes of the approver's credential, the ceiling for the new device's key.
    pub scopes: Option<String>,
    pub failed_attempts: u32,
    pub interval_ms: i64,
    pub last_polled_at: Option<Millis>,
    pub created_at: Millis,
    pub expires_at: Millis,
}

/// A link to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewLink {
    pub device_code_hash: [u8; 32],
    pub user_code_locator: [u8; 32],
    pub user_code_hash: [u8; 32],
    pub device: NewDevice,
    pub nearby: bool,
    pub interval_ms: i64,
    pub expires_at: Millis,
}

const SELECT: &str = "SELECT id, device_code_hash, user_code_hash, client_device_id, device_name,
        platform, device_model, app_name, app_version, nearby, state, user_id, scopes,
        failed_attempts, interval_ms, last_polled_at, created_at, expires_at
    FROM device_links";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LinkRow> {
    let state: String = row.get(10)?;
    Ok(LinkRow {
        key: row.get(0)?,
        device_code_hash: hash32(row.get(1)?).unwrap_or_default(),
        user_code_hash: hash32(row.get(2)?).unwrap_or_default(),
        device: NewDevice {
            client_device_id: row.get(3)?,
            name: row.get(4)?,
            platform: row.get(5)?,
            model: row.get(6)?,
            app_name: row.get(7)?,
            app_version: row.get(8)?,
        },
        nearby: row.get::<_, i64>(9)? != 0,
        // An unknown state is never approvable.
        state: LinkState::parse(&state).unwrap_or(LinkState::Locked),
        user: row.get(11)?,
        scopes: row.get(12)?,
        failed_attempts: to_u32(row.get(13)?),
        interval_ms: row.get(14)?,
        last_polled_at: row.get(15)?,
        created_at: row.get(16)?,
        expires_at: row.get(17)?,
    })
}

/// Fails with `Conflict` when a code collides with a link still kept; retry with new codes.
/// Expired links holding the same locator are cleared first.
pub fn insert(tx: &mut WriteTx<'_>, new: &NewLink) -> Result<LinkRow, DbError> {
    let now = tx.now();
    tx.prepare_cached(
        "DELETE FROM device_links WHERE user_code_locator = ?1 AND expires_at <= ?2",
    )?
    .execute(params![new.user_code_locator.as_slice(), now])?;
    tx.prepare_cached(
        "INSERT INTO device_links (device_code_hash, user_code_locator, user_code_hash,
             client_device_id, device_name, platform, device_model, app_name, app_version,
             nearby, state, interval_ms, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11, ?12, ?13)",
    )?
    .execute(params![
        new.device_code_hash.as_slice(),
        new.user_code_locator.as_slice(),
        new.user_code_hash.as_slice(),
        new.device.client_device_id,
        new.device.name,
        new.device.platform,
        new.device.model,
        new.device.app_name,
        new.device.app_version,
        i64::from(new.nearby),
        new.interval_ms,
        now,
        new.expires_at,
    ])
    .map_err(|e| conflict_on_unique(e, "link code in use"))?;
    let key = tx.last_insert_rowid();
    get(tx, key)?.ok_or(DbError::NotFound)
}

pub fn get(c: &Connection, key: i64) -> Result<Option<LinkRow>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} WHERE id = ?1"))?
        .query_row([key], map_row)
        .optional()?)
}

/// The link whose user code starts with the half that hashes to this.
pub fn by_locator(c: &Connection, locator: &[u8; 32]) -> Result<Option<LinkRow>, DbError> {
    Ok(
        c.prepare_cached(&format!("{SELECT} WHERE user_code_locator = ?1"))?
            .query_row([locator.as_slice()], map_row)
            .optional()?,
    )
}

/// The link whose device code hashes to this.
pub fn by_device_code(c: &Connection, hash: &[u8; 32]) -> Result<Option<LinkRow>, DbError> {
    Ok(
        c.prepare_cached(&format!("{SELECT} WHERE device_code_hash = ?1"))?
            .query_row([hash.as_slice()], map_row)
            .optional()?,
    )
}

/// Links still waiting for a decision.
pub fn count_pending(c: &Connection, now: Millis) -> Result<u64, DbError> {
    let n: i64 = c
        .prepare_cached(
            "SELECT count(*) FROM device_links WHERE state = 'pending' AND expires_at > ?1",
        )?
        .query_row([now], |row| row.get(0))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Counts a wrong code against a pending link and locks it at `max`. Returns the new count.
pub fn fail_attempt(tx: &mut WriteTx<'_>, key: i64, max: u32) -> Result<u32, DbError> {
    tx.prepare_cached(
        "UPDATE device_links SET failed_attempts = failed_attempts + 1,
             state = CASE WHEN failed_attempts + 1 >= ?2 THEN 'locked' ELSE state END
         WHERE id = ?1 AND state = 'pending'",
    )?
    .execute(params![key, max])?;
    get(tx, key)?
        .map(|row| row.failed_attempts)
        .ok_or(DbError::NotFound)
}

/// Approves (with the scope ceiling) or denies a pending link. `Conflict` when it was decided
/// already.
pub fn decide(
    tx: &mut WriteTx<'_>,
    key: i64,
    user: UserKey,
    approve: Option<&str>,
) -> Result<(), DbError> {
    let state = if approve.is_some() {
        LinkState::Approved
    } else {
        LinkState::Denied
    };
    let changed = tx
        .prepare_cached(
            "UPDATE device_links SET state = ?2, user_id = ?3, scopes = ?4, decided_at = ?5
             WHERE id = ?1 AND state = 'pending'",
        )?
        .execute(params![key, state.as_str(), user, approve, tx.now()])?;
    if changed == 0 {
        return Err(DbError::Conflict(String::from("link already decided")));
    }
    Ok(())
}

/// Records a poll, and the interval the device must keep from now on.
pub fn polled(tx: &mut WriteTx<'_>, key: i64, interval_ms: i64) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE device_links SET last_polled_at = ?2, interval_ms = ?3 WHERE id = ?1",
    )?
    .execute(params![key, tx.now(), interval_ms])?;
    Ok(())
}

/// Marks an approved link collected. `Conflict` when it was not approved or already collected,
/// so a key is handed out once.
pub fn complete(tx: &mut WriteTx<'_>, key: i64) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE device_links SET state = 'completed', completed_at = ?2
             WHERE id = ?1 AND state = 'approved'",
        )?
        .execute(params![key, tx.now()])?;
    if changed == 0 {
        return Err(DbError::Conflict(String::from("link already collected")));
    }
    Ok(())
}

/// Deletes links that expired before `before`. Expired links are kept a while after expiry so
/// the approver is told "expired" rather than "wrong code".
pub fn prune(tx: &mut WriteTx<'_>, before: Millis) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("DELETE FROM device_links WHERE expires_at <= ?1")?
        .execute([before])?;
    Ok(n as u64)
}
