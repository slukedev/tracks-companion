//! Pairing offers (made by a signed-in user, claimed by a device) and pairing requests (started
//! by a device, approved by a signed-in user). Only hashes of codes and secrets are stored.

use rusqlite::{Connection, OptionalExtension, params};

use super::sql::{conflict_on_unique, hash32, to_u32};
use crate::db::{DbError, WriteTx};
use crate::ids::{KeyId, PairingId, UserKey, random_id};
use crate::records::text_enum;
use crate::repo::devices::NewDevice;
use crate::time::Millis;

text_enum!(
    /// Who started the pairing.
    PairingKind {
        Offer => "offer",
        Request => "request",
    }
);

/// A pairing as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingRow {
    pub id: PairingId,
    pub kind: PairingKind,
    /// The offering user, or for a request the approving user once approved.
    pub user: Option<UserKey>,
    /// Request: the poll secret's hash. Offer: whatever second check value the auth crate
    /// keeps for the typed code.
    pub poll_secret_hash: Option<[u8; 32]>,
    pub device_name: Option<String>,
    pub platform: Option<String>,
    /// Request only: what the device said about itself, so the key it gets is bound to its
    /// real install id.
    pub device: Option<NewDevice>,
    /// Request only: the issued device key, sealed, until claimed.
    pub issued_key_sealed: Option<Vec<u8>>,
    pub api_key: Option<KeyId>,
    pub failed_attempts: u32,
    pub created_at: Millis,
    pub expires_at: Millis,
    pub approved_at: Option<Millis>,
    pub claimed_at: Option<Millis>,
}

/// A pairing to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPairing {
    pub kind: PairingKind,
    pub user: Option<UserKey>,
    /// The typed code of an offer, or the 6 digits of a request.
    pub code_hash: [u8; 32],
    /// Offer only: the 128-bit secret in the QR payload.
    pub qr_secret_hash: Option<[u8; 32]>,
    /// Request: the poll secret's hash. Offer: any second check value for the typed code.
    pub poll_secret_hash: Option<[u8; 32]>,
    pub device_name: Option<String>,
    pub platform: Option<String>,
    /// Request only: the device as it described itself.
    pub device: Option<NewDevice>,
    pub expires_at: Millis,
}

const SELECT: &str = "SELECT g.pid, g.kind, g.user_id, g.poll_secret_hash, g.device_name,
        g.platform, g.issued_key_sealed, k.pid, g.failed_attempts, g.created_at, g.expires_at,
        g.approved_at, g.claimed_at, g.client_device_id, g.device_model, g.app_name,
        g.app_version
    FROM pairings g LEFT JOIN api_keys k ON k.id = g.api_key_id";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PairingRow> {
    let kind: String = row.get(1)?;
    Ok(PairingRow {
        id: row.get(0)?,
        kind: PairingKind::parse(&kind).unwrap_or(PairingKind::Offer),
        user: row.get(2)?,
        poll_secret_hash: hash32(row.get(3)?),
        device_name: row.get(4)?,
        platform: row.get(5)?,
        issued_key_sealed: row.get(6)?,
        api_key: row.get(7)?,
        failed_attempts: to_u32(row.get(8)?),
        created_at: row.get(9)?,
        expires_at: row.get(10)?,
        approved_at: row.get(11)?,
        claimed_at: row.get(12)?,
        device: row
            .get::<_, Option<String>>(13)?
            .map(|client_device_id| -> rusqlite::Result<NewDevice> {
                Ok(NewDevice {
                    client_device_id,
                    name: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    platform: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    model: row.get(14)?,
                    app_name: row.get(15)?,
                    app_version: row.get(16)?,
                })
            })
            .transpose()?,
    })
}

/// Fails with `Conflict` when the code hash collides with a live pairing; retry with a new code.
/// Expired pairings holding the same code are cleared first.
pub fn insert(tx: &mut WriteTx<'_>, new: &NewPairing) -> Result<PairingRow, DbError> {
    let now = tx.now();
    let hashes: Vec<&[u8]> = std::iter::once(new.code_hash.as_slice())
        .chain(new.qr_secret_hash.as_ref().map(<[u8; 32]>::as_slice))
        .collect();
    for hash in &hashes {
        tx.prepare_cached(
            "DELETE FROM pairings WHERE (code_hash = ?1 OR qr_secret_hash = ?1) AND expires_at <= ?2",
        )?
        .execute(params![hash, now])?;
    }
    let id: PairingId = random_id();
    tx.prepare_cached(
        "INSERT INTO pairings (pid, kind, user_id, code_hash, qr_secret_hash, poll_secret_hash,
             device_name, platform, created_at, expires_at, client_device_id, device_model,
             app_name, app_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?
    .execute(params![
        id,
        new.kind.as_str(),
        new.user,
        new.code_hash.as_slice(),
        new.qr_secret_hash.as_ref().map(|h| h.to_vec()),
        new.poll_secret_hash.as_ref().map(|h| h.to_vec()),
        new.device_name
            .as_deref()
            .or(new.device.as_ref().map(|d| d.name.as_str())),
        new.platform
            .as_deref()
            .or(new.device.as_ref().map(|d| d.platform.as_str())),
        now,
        new.expires_at,
        new.device.as_ref().map(|d| d.client_device_id.as_str()),
        new.device.as_ref().and_then(|d| d.model.as_deref()),
        new.device.as_ref().and_then(|d| d.app_name.as_deref()),
        new.device.as_ref().and_then(|d| d.app_version.as_deref())
    ])
    .map_err(|e| conflict_on_unique(e, "pairing code in use"))?;
    get(tx, &id)?.ok_or(DbError::NotFound)
}

pub fn get(c: &Connection, id: &PairingId) -> Result<Option<PairingRow>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} WHERE g.pid = ?1"))?
        .query_row([id], map_row)
        .optional()?)
}

/// A live, unclaimed offer's QR secret hash, for the endpoint proof.
pub fn offer_qr_hash(
    c: &Connection,
    id: &PairingId,
    now: Millis,
) -> Result<Option<[u8; 32]>, DbError> {
    let hash: Option<Option<Vec<u8>>> = c
        .prepare_cached(
            "SELECT qr_secret_hash FROM pairings
             WHERE pid = ?1 AND kind = 'offer' AND claimed_at IS NULL AND expires_at > ?2",
        )?
        .query_row(params![id, now], |row| row.get(0))
        .optional()?;
    Ok(hash32(hash.flatten()))
}

/// The pairing whose typed code, 6-digit code or QR secret hashes to this.
pub fn by_code_hash(c: &Connection, code_hash: &[u8; 32]) -> Result<Option<PairingRow>, DbError> {
    Ok(c.prepare_cached(&format!(
        "{SELECT} WHERE g.code_hash = ?1 OR g.qr_secret_hash = ?1"
    ))?
    .query_row([code_hash.as_slice()], map_row)
    .optional()?)
}

/// Binds a request to the approving user and stores the sealed key it will hand out.
pub fn approve(
    tx: &mut WriteTx<'_>,
    id: &PairingId,
    user: UserKey,
    issued_key_sealed: &[u8],
    api_key: &KeyId,
) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE pairings SET user_id = ?2, issued_key_sealed = ?3,
                 api_key_id = (SELECT id FROM api_keys WHERE pid = ?4), approved_at = ?5
             WHERE pid = ?1 AND approved_at IS NULL AND claimed_at IS NULL",
        )?
        .execute(params![id, user, issued_key_sealed, api_key, tx.now()])?;
    if changed == 0 {
        return Err(if get(tx, id)?.is_some() {
            DbError::Conflict(String::from("pairing already approved"))
        } else {
            DbError::NotFound
        });
    }
    Ok(())
}

/// Marks the pairing claimed and clears any sealed key. Fails with `Conflict` if already
/// claimed.
pub fn claim(tx: &mut WriteTx<'_>, id: &PairingId) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE pairings SET claimed_at = ?2, issued_key_sealed = NULL
             WHERE pid = ?1 AND claimed_at IS NULL",
        )?
        .execute(params![id, tx.now()])?;
    if changed == 0 {
        return Err(if get(tx, id)?.is_some() {
            DbError::Conflict(String::from("pairing already claimed"))
        } else {
            DbError::NotFound
        });
    }
    Ok(())
}

/// Counts a wrong code. Returns the new count.
pub fn fail_attempt(tx: &mut WriteTx<'_>, id: &PairingId) -> Result<u32, DbError> {
    tx.prepare_cached("UPDATE pairings SET failed_attempts = failed_attempts + 1 WHERE pid = ?1")?
        .execute([id])?;
    get(tx, id)?
        .map(|row| row.failed_attempts)
        .ok_or(DbError::NotFound)
}

pub fn delete(tx: &mut WriteTx<'_>, id: &PairingId) -> Result<(), DbError> {
    tx.prepare_cached("DELETE FROM pairings WHERE pid = ?1")?
        .execute([id])?;
    Ok(())
}

/// Deletes expired pairings.
pub fn prune(tx: &mut WriteTx<'_>, now: Millis) -> Result<u64, DbError> {
    let n = tx
        .prepare_cached("DELETE FROM pairings WHERE expires_at <= ?1")?
        .execute([now])?;
    Ok(n as u64)
}
