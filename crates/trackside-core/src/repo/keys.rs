//! Device keys, API keys and app passwords (`api_keys`). Only hashes and sealed copies are
//! stored; the auth crate makes and checks the secrets.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::conflict_on_unique;
use crate::db::{DbError, WriteTx};
use crate::ids::{DeviceKey, KeyId, UserKey, random_id};
use crate::records::text_enum;
use crate::time::Millis;

text_enum!(
    /// What a key is for.
    KeyKind {
        Device => "device",
        ApiKey => "api_key",
        AppPassword => "app_password",
    }
);

/// A key as stored, without its secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeyRow {
    pub id: KeyId,
    pub user: UserKey,
    pub device: Option<DeviceKey>,
    pub kind: KeyKind,
    pub name: String,
    /// The first 10 characters of the secret, shown in lists.
    pub prefix: String,
    /// Space-separated scope names.
    pub scopes: String,
    pub created_at: Millis,
    pub last_used_at: Option<Millis>,
    pub last_used_ip: Option<String>,
    pub expires_at: Option<Millis>,
    pub revoked_at: Option<Millis>,
}

/// A key to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewKey {
    pub user: UserKey,
    pub device: Option<DeviceKey>,
    pub kind: KeyKind,
    pub name: String,
    pub prefix: String,
    /// SHA-256 of the whole secret.
    pub secret_hash: [u8; 32],
    /// App passwords only: the secret sealed with the server key.
    pub secret_sealed: Option<Vec<u8>>,
    pub scopes: String,
    pub expires_at: Option<Millis>,
}

const SELECT: &str = "SELECT pid, user_id, device_id, kind, name, prefix, scopes, created_at,
        last_used_at, last_used_ip, expires_at, revoked_at
    FROM api_keys";

fn map_row(row: &Row<'_>) -> rusqlite::Result<ApiKeyRow> {
    let kind: String = row.get(3)?;
    Ok(ApiKeyRow {
        id: row.get(0)?,
        user: row.get(1)?,
        device: row.get(2)?,
        kind: KeyKind::parse(&kind).unwrap_or(KeyKind::ApiKey),
        name: row.get(4)?,
        prefix: row.get(5)?,
        scopes: row.get(6)?,
        created_at: row.get(7)?,
        last_used_at: row.get(8)?,
        last_used_ip: row.get(9)?,
        expires_at: row.get(10)?,
        revoked_at: row.get(11)?,
    })
}

fn by_id(c: &Connection, id: &KeyId) -> Result<Option<ApiKeyRow>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} WHERE pid = ?1"))?
        .query_row([id], map_row)
        .optional()?)
}

pub fn insert(tx: &mut WriteTx<'_>, new: &NewKey) -> Result<ApiKeyRow, DbError> {
    let id: KeyId = random_id();
    tx.prepare_cached(
        "INSERT INTO api_keys (pid, user_id, device_id, kind, name, prefix, secret_hash,
             secret_sealed, scopes, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        id,
        new.user,
        new.device,
        new.kind.as_str(),
        new.name,
        new.prefix,
        new.secret_hash.as_slice(),
        new.secret_sealed,
        new.scopes,
        tx.now(),
        new.expires_at
    ])
    .map_err(|e| conflict_on_unique(e, "key already exists"))?;
    by_id(tx, &id)?.ok_or(DbError::NotFound)
}

/// The unrevoked key with this secret hash. Expiry is the caller's check.
pub fn by_hash(c: &Connection, secret_hash: &[u8; 32]) -> Result<Option<ApiKeyRow>, DbError> {
    Ok(c.prepare_cached(&format!(
        "{SELECT} WHERE secret_hash = ?1 AND revoked_at IS NULL"
    ))?
    .query_row([secret_hash.as_slice()], map_row)
    .optional()?)
}

/// The secret hash of a device's newest working key, by the device's public id, for the
/// endpoint proof. A revoked or expired key, or a revoked device, gives `None`.
pub fn device_secret_hash(
    c: &Connection,
    device: &crate::ids::DeviceId,
    now: Millis,
) -> Result<Option<[u8; 32]>, DbError> {
    let hash: Option<Vec<u8>> = c
        .prepare_cached(
            "SELECT k.secret_hash FROM api_keys k JOIN devices d ON d.id = k.device_id
             WHERE d.pid = ?1 AND d.revoked_at IS NULL AND k.kind = 'device'
               AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > ?2)
             ORDER BY k.created_at DESC, k.id DESC LIMIT 1",
        )?
        .query_row(params![device, now], |row| row.get(0))
        .optional()?;
    Ok(hash.and_then(|h| <[u8; 32]>::try_from(h.as_slice()).ok()))
}

/// One user's keys, or everyone's for `None`, newest first. Device keys are included.
pub fn list(c: &Connection, user: Option<UserKey>) -> Result<Vec<ApiKeyRow>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "{SELECT} WHERE ?1 IS NULL OR user_id = ?1 ORDER BY created_at DESC, id DESC"
    ))?;
    let rows = statement.query_map([user], map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Revokes a key. `user` restricts it to that user's keys (`NotFound` otherwise); `None` is
/// for admins.
pub fn revoke(
    tx: &mut WriteTx<'_>,
    user: Option<UserKey>,
    id: &KeyId,
) -> Result<ApiKeyRow, DbError> {
    match by_id(tx, id)? {
        Some(key) if user.is_none_or(|u| u == key.user) => {
            tx.prepare_cached(
                "UPDATE api_keys SET revoked_at = coalesce(revoked_at, ?2) WHERE pid = ?1",
            )?
            .execute(params![id, tx.now()])?;
            by_id(tx, id)?.ok_or(DbError::NotFound)
        }
        _ => Err(DbError::NotFound),
    }
}

/// Revokes every key of a device.
pub fn revoke_for_device(tx: &mut WriteTx<'_>, device: DeviceKey) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached(
            "UPDATE api_keys SET revoked_at = ?2 WHERE device_id = ?1 AND revoked_at IS NULL",
        )?
        .execute(params![device, tx.now()])?;
    Ok(n as u64)
}

pub fn touch_last_used(tx: &mut WriteTx<'_>, id: &KeyId, ip: Option<&str>) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE api_keys SET last_used_at = ?2, last_used_ip = coalesce(?3, last_used_ip)
         WHERE pid = ?1",
    )?
    .execute(params![id, tx.now(), ip])?;
    Ok(())
}

/// A user's active app passwords with their sealed secrets, for Subsonic token checks.
pub fn app_passwords_for(
    c: &Connection,
    user: UserKey,
) -> Result<Vec<(ApiKeyRow, Vec<u8>)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT pid, user_id, device_id, kind, name, prefix, scopes, created_at, last_used_at,
                last_used_ip, expires_at, revoked_at, secret_sealed
         FROM api_keys
         WHERE user_id = ?1 AND kind = 'app_password' AND revoked_at IS NULL
           AND secret_sealed IS NOT NULL
         ORDER BY id",
    )?;
    let rows = statement.query_map([user], |row| {
        Ok((map_row(row)?, row.get::<_, Vec<u8>>(12)?))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every unrevoked app password with a sealed secret, so the server can find the ones a lost
/// secret key made unreadable.
pub fn sealed_app_passwords(c: &Connection) -> Result<Vec<(ApiKeyRow, Vec<u8>)>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT pid, user_id, device_id, kind, name, prefix, scopes, created_at, last_used_at,
                last_used_ip, expires_at, revoked_at, secret_sealed
         FROM api_keys
         WHERE kind = 'app_password' AND revoked_at IS NULL AND secret_sealed IS NOT NULL
         ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| Ok((map_row(row)?, row.get::<_, Vec<u8>>(12)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}
