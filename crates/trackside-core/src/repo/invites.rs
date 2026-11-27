//! Invites. Only the SHA-256 of the token is stored.

use rusqlite::{Connection, params};

use super::sql::conflict_on_unique;
use crate::access::{Permissions, Role};
use crate::db::{DbError, WriteTx};
use crate::ids::{InviteId, LibraryKey, UserKey, random_id};
use crate::time::Millis;

/// An invite as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteRow {
    pub id: InviteId,
    pub created_by: Option<UserKey>,
    pub role: Role,
    /// What the new account may do (admins get everything regardless).
    pub permissions: Permissions,
    pub note: Option<String>,
    pub libraries: Vec<LibraryKey>,
    pub created_at: Millis,
    pub expires_at: Millis,
    pub used_at: Option<Millis>,
    pub used_by: Option<UserKey>,
    pub revoked_at: Option<Millis>,
}

/// An invite to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewInvite {
    pub token_hash: [u8; 32],
    pub created_by: Option<UserKey>,
    pub role: Role,
    pub permissions: Permissions,
    pub note: Option<String>,
    pub libraries: Vec<LibraryKey>,
    pub expires_at: Millis,
}

const SELECT: &str = "SELECT id, pid, created_by, role, can_request, can_upload, note, created_at,
        expires_at, used_at, used_by, revoked_at, can_download, legacy_subsonic, max_bitrate_kbps,
        can_use_speakers, can_play_services
    FROM invites";

fn read(
    c: &Connection,
    tail: &str,
    param: &dyn rusqlite::ToSql,
) -> Result<Vec<InviteRow>, DbError> {
    let mut statement = c.prepare_cached(&format!("{SELECT} {tail}"))?;
    let rows: Vec<(i64, InviteRow)> = statement
        .query_map([param], |row| {
            let role: String = row.get(3)?;
            Ok((
                row.get(0)?,
                InviteRow {
                    id: row.get(1)?,
                    created_by: row.get(2)?,
                    role: Role::parse(&role).unwrap_or(Role::Member),
                    permissions: Permissions {
                        can_download: row.get(12)?,
                        can_upload: row.get(5)?,
                        can_request: row.get(4)?,
                        legacy_subsonic: row.get(13)?,
                        max_bitrate_kbps: row.get(14)?,
                        can_use_speakers: row.get(15)?,
                        can_play_services: row.get(16)?,
                    },
                    note: row.get(6)?,
                    libraries: Vec::new(),
                    created_at: row.get(7)?,
                    expires_at: row.get(8)?,
                    used_at: row.get(9)?,
                    used_by: row.get(10)?,
                    revoked_at: row.get(11)?,
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    let mut libraries = c.prepare_cached(
        "SELECT library_id FROM invite_libraries WHERE invite_id = ?1 ORDER BY library_id",
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for (key, mut invite) in rows {
        invite.libraries = libraries
            .query_map([key], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        out.push(invite);
    }
    Ok(out)
}

pub fn insert(tx: &mut WriteTx<'_>, new: &NewInvite) -> Result<InviteRow, DbError> {
    let id: InviteId = random_id();
    tx.prepare_cached(
        "INSERT INTO invites (pid, token_hash, created_by, role, can_request, can_upload, note,
             created_at, expires_at, can_download, legacy_subsonic, max_bitrate_kbps,
             can_use_speakers, can_play_services)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )?
    .execute(params![
        id,
        new.token_hash.as_slice(),
        new.created_by,
        new.role.as_str(),
        new.permissions.can_request,
        new.permissions.can_upload,
        new.note,
        tx.now(),
        new.expires_at,
        new.permissions.can_download,
        new.permissions.legacy_subsonic,
        new.permissions.max_bitrate_kbps,
        new.permissions.can_use_speakers,
        new.permissions.can_play_services
    ])
    .map_err(|e| conflict_on_unique(e, "invite exists"))?;
    let key = tx.last_insert_rowid();
    for library in &new.libraries {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO invite_libraries (invite_id, library_id)
             SELECT ?1, id FROM libraries WHERE id = ?2",
        )?
        .execute(params![key, library])?;
    }
    read(tx, "WHERE id = ?1", &key)?
        .into_iter()
        .next()
        .ok_or(DbError::NotFound)
}

/// The invite with this token hash, in any state.
pub fn by_hash(c: &Connection, token_hash: &[u8; 32]) -> Result<Option<InviteRow>, DbError> {
    Ok(read(c, "WHERE token_hash = ?1", &token_hash.as_slice())?
        .into_iter()
        .next())
}

/// Marks an open invite used. Fails with `Conflict` if it was used, revoked or expired.
pub fn accept(tx: &mut WriteTx<'_>, id: &InviteId, used_by: UserKey) -> Result<(), DbError> {
    let now = tx.now();
    let changed = tx
        .prepare_cached(
            "UPDATE invites SET used_at = ?2, used_by = ?3
             WHERE pid = ?1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?2",
        )?
        .execute(params![id, now, used_by])?;
    if changed == 0 {
        let exists: bool = tx
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM invites WHERE pid = ?1)")?
            .query_row([id], |row| row.get(0))?;
        return Err(if exists {
            DbError::Conflict(String::from("invite is no longer open"))
        } else {
            DbError::NotFound
        });
    }
    Ok(())
}

pub fn revoke(tx: &mut WriteTx<'_>, id: &InviteId) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached("UPDATE invites SET revoked_at = coalesce(revoked_at, ?2) WHERE pid = ?1")?
        .execute(params![id, tx.now()])?;
    if changed == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Newest first.
pub fn list(c: &Connection) -> Result<Vec<InviteRow>, DbError> {
    read(c, "WHERE ?1 = 1 ORDER BY created_at DESC, id DESC", &1)
}
