//! Devices: one row per app install per user.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::{DbError, WriteTx};
use crate::ids::{DeviceId, DeviceKey, UserKey, random_id};
use crate::time::Millis;

/// A device as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub key: DeviceKey,
    pub id: DeviceId,
    pub user: UserKey,
    pub client_device_id: String,
    pub name: String,
    /// `ios`, `macos`, `android`, `android_tv`, `web` or `other`.
    pub platform: String,
    pub model: Option<String>,
    pub app_name: Option<String>,
    pub app_version: Option<String>,
    /// Bumped to invalidate the device's media tokens.
    pub media_epoch: i64,
    pub created_at: Millis,
    pub last_seen_at: Millis,
    pub last_ip: Option<String>,
    pub revoked_at: Option<Millis>,
}

/// What a client says about itself when signing in or pairing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDevice {
    pub client_device_id: String,
    pub name: String,
    pub platform: String,
    pub model: Option<String>,
    pub app_name: Option<String>,
    pub app_version: Option<String>,
}

const SELECT: &str = "SELECT id, pid, user_id, client_device_id, name, platform, model, app_name,
        app_version, media_epoch, created_at, last_seen_at, last_ip, revoked_at
    FROM devices";

fn map_row(row: &Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        key: row.get(0)?,
        id: row.get(1)?,
        user: row.get(2)?,
        client_device_id: row.get(3)?,
        name: row.get(4)?,
        platform: row.get(5)?,
        model: row.get(6)?,
        app_name: row.get(7)?,
        app_version: row.get(8)?,
        media_epoch: row.get(9)?,
        created_at: row.get(10)?,
        last_seen_at: row.get(11)?,
        last_ip: row.get(12)?,
        revoked_at: row.get(13)?,
    })
}

fn one(c: &Connection, tail: &str, param: &dyn rusqlite::ToSql) -> Result<Option<Device>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} {tail}"))?
        .query_row([param], map_row)
        .optional()?)
}

pub fn get(c: &Connection, key: DeviceKey) -> Result<Option<Device>, DbError> {
    one(c, "WHERE id = ?1", &key)
}

pub fn by_pid(c: &Connection, id: &DeviceId) -> Result<Option<Device>, DbError> {
    one(c, "WHERE pid = ?1", id)
}

const PLATFORMS: [&str; 6] = ["ios", "macos", "android", "android_tv", "web", "other"];

fn platform(value: &str) -> &'static str {
    let lower = value.trim().to_ascii_lowercase();
    PLATFORMS
        .iter()
        .find(|p| **p == lower)
        .copied()
        .unwrap_or("other")
}

/// Creates the device, or for a known `(user, client_device_id)` updates it, clears
/// `revoked_at`, revokes its previous keys and bumps its media epoch.
pub fn upsert_for_login(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    device: &NewDevice,
    ip: Option<&str>,
) -> Result<Device, DbError> {
    let client_id = device.client_device_id.trim();
    if client_id.is_empty() {
        return Err(DbError::Invalid(String::from("client_device_id")));
    }
    let name = device.name.trim();
    let name = if name.is_empty() { "Device" } else { name };
    let now = tx.now();
    let existing: Option<DeviceKey> = tx
        .prepare_cached("SELECT id FROM devices WHERE user_id = ?1 AND client_device_id = ?2")?
        .query_row(params![user, client_id], |row| row.get(0))
        .optional()?;
    let key = match existing {
        Some(key) => {
            super::keys::revoke_for_device(tx, key)?;
            tx.prepare_cached(
                "UPDATE devices SET name = ?2, platform = ?3, model = ?4, app_name = ?5,
                     app_version = ?6, media_epoch = media_epoch + 1, last_seen_at = ?7,
                     last_ip = ?8, revoked_at = NULL
                 WHERE id = ?1",
            )?
            .execute(params![
                key,
                name,
                platform(&device.platform),
                device.model,
                device.app_name,
                device.app_version,
                now,
                ip
            ])?;
            key
        }
        None => {
            let id: DeviceId = random_id();
            tx.prepare_cached(
                "INSERT INTO devices (pid, user_id, client_device_id, name, platform, model,
                     app_name, app_version, created_at, last_seen_at, last_ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
            )?
            .execute(params![
                id,
                user,
                client_id,
                name,
                platform(&device.platform),
                device.model,
                device.app_name,
                device.app_version,
                now,
                ip
            ])?;
            DeviceKey(tx.last_insert_rowid())
        }
    };
    get(tx, key)?.ok_or(DbError::NotFound)
}

fn owned(tx: &WriteTx<'_>, user: Option<UserKey>, id: &DeviceId) -> Result<Device, DbError> {
    match by_pid(tx, id)? {
        Some(device) if user.is_none_or(|u| u == device.user) => Ok(device),
        _ => Err(DbError::NotFound),
    }
}

/// Revokes the device: sets `revoked_at`, bumps `media_epoch` and revokes its keys. `user`
/// restricts it to that user's devices (`NotFound` otherwise); `None` is for admins.
pub fn revoke(
    tx: &mut WriteTx<'_>,
    user: Option<UserKey>,
    id: &DeviceId,
) -> Result<Device, DbError> {
    let device = owned(tx, user, id)?;
    tx.prepare_cached(
        "UPDATE devices SET revoked_at = coalesce(revoked_at, ?2), media_epoch = media_epoch + 1
         WHERE id = ?1",
    )?
    .execute(params![device.key, tx.now()])?;
    super::keys::revoke_for_device(tx, device.key)?;
    get(tx, device.key)?.ok_or(DbError::NotFound)
}

pub fn rename(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    id: &DeviceId,
    name: &str,
) -> Result<Device, DbError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DbError::Invalid(String::from("name")));
    }
    let device = owned(tx, Some(user), id)?;
    tx.prepare_cached("UPDATE devices SET name = ?2 WHERE id = ?1")?
        .execute(params![device.key, name])?;
    get(tx, device.key)?.ok_or(DbError::NotFound)
}

/// Updates `last_seen_at` and `last_ip`.
pub fn touch(tx: &mut WriteTx<'_>, key: DeviceKey, ip: Option<&str>) -> Result<(), DbError> {
    tx.prepare_cached(
        "UPDATE devices SET last_seen_at = ?2, last_ip = coalesce(?3, last_ip) WHERE id = ?1",
    )?
    .execute(params![key, tx.now(), ip])?;
    Ok(())
}

/// One user's devices, or everyone's for `None`, most recently seen first.
pub fn list(c: &Connection, user: Option<UserKey>) -> Result<Vec<Device>, DbError> {
    let mut statement = c.prepare_cached(&format!(
        "{SELECT} WHERE ?1 IS NULL OR user_id = ?1 ORDER BY last_seen_at DESC, id DESC"
    ))?;
    let rows = statement.query_map([user], map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}
