//! Users and their library grants.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::sql::{conflict_on_unique, opt_u32, to_u32, to_u64};
use crate::access::{Access, LibrarySet, Permissions, Role};
use crate::db::{DbError, WriteTx};
use crate::ids::{LibraryKey, UserId, UserKey, random_id};
use crate::time::Millis;

/// A user account, without the password hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub key: UserKey,
    pub id: UserId,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub permissions: Permissions,
    pub sync_epoch: i64,
    pub created_at: Millis,
    pub updated_at: Millis,
    pub password_changed_at: Millis,
    pub disabled_at: Option<Millis>,
    /// The user must pick a new password before doing anything else (set after an admin
    /// reset from the command line); cleared by [`set_password`].
    pub must_change_password: bool,
}

/// A user to create. The password is already hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewUser {
    pub username: String,
    pub display_name: String,
    pub password_hash: String,
    pub role: Role,
    pub permissions: Permissions,
    pub libraries: Vec<LibraryKey>,
}

/// `PATCH /admin/users/{id}` and `PATCH /api/v1/me`. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserPatch {
    pub display_name: Option<String>,
    pub role: Option<Role>,
    pub permissions: Option<Permissions>,
    pub disabled: Option<bool>,
}

const SELECT: &str = "SELECT id, pid, username, display_name, role, can_download, can_upload,
        can_request, legacy_subsonic, max_bitrate_kbps, sync_epoch, created_at, updated_at,
        password_changed_at, disabled_at, must_change_password
    FROM users";

fn map_row(row: &Row<'_>) -> rusqlite::Result<User> {
    let role: String = row.get(4)?;
    Ok(User {
        key: row.get(0)?,
        id: row.get(1)?,
        username: row.get(2)?,
        display_name: row.get(3)?,
        role: Role::parse(&role).unwrap_or(Role::Member),
        permissions: Permissions {
            can_download: row.get(5)?,
            can_upload: row.get(6)?,
            can_request: row.get(7)?,
            legacy_subsonic: row.get(8)?,
            max_bitrate_kbps: opt_u32(row.get(9)?),
        },
        sync_epoch: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        password_changed_at: row.get(13)?,
        disabled_at: row.get(14)?,
        must_change_password: row.get(15)?,
    })
}

fn one(c: &Connection, tail: &str, param: &dyn rusqlite::ToSql) -> Result<Option<User>, DbError> {
    Ok(c.prepare_cached(&format!("{SELECT} {tail}"))?
        .query_row([param], map_row)
        .optional()?)
}

pub fn get(c: &Connection, key: UserKey) -> Result<Option<User>, DbError> {
    one(c, "WHERE id = ?1", &key)
}

pub fn by_pid(c: &Connection, id: &UserId) -> Result<Option<User>, DbError> {
    one(c, "WHERE pid = ?1", id)
}

/// Looks a user up by `tracks_model::folded(username)`.
pub fn by_username(c: &Connection, username: &str) -> Result<Option<User>, DbError> {
    one(
        c,
        "WHERE username_key = ?1",
        &tracks_model::folded(username),
    )
}

/// The Argon2id PHC string of a user's password.
pub fn password_hash(c: &Connection, key: UserKey) -> Result<Option<String>, DbError> {
    Ok(
        c.prepare_cached("SELECT password_hash FROM users WHERE id = ?1")?
            .query_row([key], |row| row.get(0))
            .optional()?,
    )
}

/// Every user, by username.
pub fn list(c: &Connection) -> Result<Vec<User>, DbError> {
    let mut statement = c.prepare_cached(&format!("{SELECT} ORDER BY username_key"))?;
    let rows = statement.query_map([], map_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// What the user may see: all libraries for admins, granted ones for members. Disabled users
/// still get an answer; refusing them is the auth crate's job.
pub fn access(c: &Connection, key: UserKey) -> Result<Option<Access>, DbError> {
    let Some(user) = get(c, key)? else {
        return Ok(None);
    };
    let libraries = match user.role {
        Role::Admin => LibrarySet::All,
        Role::Member => LibrarySet::Only(Arc::from(libraries(c, key)?)),
    };
    Ok(Some(Access {
        user: key,
        role: user.role,
        libraries,
        sync_epoch: user.sync_epoch,
    }))
}

/// Granted libraries (empty for admins, who see all without grants).
pub fn libraries(c: &Connection, key: UserKey) -> Result<Vec<LibraryKey>, DbError> {
    let mut statement = c.prepare_cached(
        "SELECT library_id FROM library_access WHERE user_id = ?1 ORDER BY library_id",
    )?;
    let rows = statement.query_map([key], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn validate_username(username: &str) -> Result<&str, DbError> {
    let username = username.trim();
    if username.is_empty()
        || username.chars().count() > 64
        || username.chars().any(char::is_control)
    {
        return Err(DbError::Invalid(String::from("username")));
    }
    Ok(username)
}

/// Fails with `Conflict` when the username is taken.
pub fn create(tx: &mut WriteTx<'_>, new: &NewUser) -> Result<User, DbError> {
    let username = validate_username(&new.username)?;
    let display = new.display_name.trim();
    let display = if display.is_empty() {
        username
    } else {
        display
    };
    let id: UserId = random_id();
    let now = tx.now();
    let p = &new.permissions;
    tx.prepare_cached(
        "INSERT INTO users (pid, username, username_key, display_name, password_hash, role,
             can_download, can_upload, can_request, legacy_subsonic, max_bitrate_kbps,
             created_at, updated_at, password_changed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12, ?12)",
    )?
    .execute(params![
        id,
        username,
        tracks_model::folded(username),
        display,
        new.password_hash,
        new.role.as_str(),
        p.can_download,
        p.can_upload,
        p.can_request,
        p.legacy_subsonic,
        p.max_bitrate_kbps,
        now
    ])
    .map_err(|e| conflict_on_unique(e, "username taken"))?;
    let key = UserKey(tx.last_insert_rowid());
    grant(tx, key, &new.libraries, None)?;
    get(tx, key)?.ok_or(DbError::NotFound)
}

fn grant(
    tx: &WriteTx<'_>,
    key: UserKey,
    libraries: &[LibraryKey],
    granted_by: Option<UserKey>,
) -> Result<(), DbError> {
    let now = tx.now();
    for library in libraries {
        tx.prepare_cached(
            "INSERT OR IGNORE INTO library_access (user_id, library_id, granted_at, granted_by)
             SELECT ?1, id, ?3, ?4 FROM libraries WHERE id = ?2",
        )?
        .execute(params![key, library, now, granted_by])?;
    }
    Ok(())
}

fn other_active_admins(c: &Connection, key: UserKey) -> Result<u32, DbError> {
    let n: i64 = c
        .prepare_cached(
            "SELECT count(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL AND id != ?1",
        )?
        .query_row([key], |row| row.get(0))?;
    Ok(to_u32(n))
}

/// Fails with `Conflict` when the change would demote or disable the last active admin.
pub fn update(tx: &mut WriteTx<'_>, key: UserKey, patch: &UserPatch) -> Result<User, DbError> {
    let current = get(tx, key)?.ok_or(DbError::NotFound)?;
    let role = patch.role.unwrap_or(current.role);
    let disabled_at = match patch.disabled {
        Some(true) => current.disabled_at.or(Some(tx.now())),
        Some(false) => None,
        None => current.disabled_at,
    };
    let stays_admin = role == Role::Admin && disabled_at.is_none();
    if current.role == Role::Admin
        && current.disabled_at.is_none()
        && !stays_admin
        && other_active_admins(tx, key)? == 0
    {
        return Err(DbError::Conflict(String::from("last admin")));
    }
    let display = match &patch.display_name {
        Some(name) if name.trim().is_empty() => {
            return Err(DbError::Invalid(String::from("display_name")));
        }
        Some(name) => name.trim().to_owned(),
        None => current.display_name.clone(),
    };
    let p = patch.permissions.as_ref().unwrap_or(&current.permissions);
    // Admins see every library and members only granted ones, so a role change changes what
    // the user can see.
    let epoch_bump = i64::from(role != current.role);
    tx.prepare_cached(
        "UPDATE users SET display_name = ?2, role = ?3, can_download = ?4, can_upload = ?5,
             can_request = ?6, legacy_subsonic = ?7, max_bitrate_kbps = ?8, disabled_at = ?9,
             sync_epoch = sync_epoch + ?10, updated_at = ?11
         WHERE id = ?1",
    )?
    .execute(params![
        key,
        display,
        role.as_str(),
        p.can_download,
        p.can_upload,
        p.can_request,
        p.legacy_subsonic,
        p.max_bitrate_kbps,
        disabled_at,
        epoch_bump,
        tx.now()
    ])?;
    get(tx, key)?.ok_or(DbError::NotFound)
}

pub fn set_password(
    tx: &mut WriteTx<'_>,
    key: UserKey,
    password_hash: &str,
) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached(
            "UPDATE users SET password_hash = ?2, password_changed_at = ?3, updated_at = ?3,
                 must_change_password = 0
             WHERE id = ?1",
        )?
        .execute(params![key, password_hash, tx.now()])?;
    if changed == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Sets or clears the must-change-password flag. Call after [`set_password`] to keep it set.
pub fn set_must_change_password(
    tx: &mut WriteTx<'_>,
    key: UserKey,
    required: bool,
) -> Result<(), DbError> {
    let changed = tx
        .prepare_cached("UPDATE users SET must_change_password = ?2 WHERE id = ?1")?
        .execute(params![key, required])?;
    if changed == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Deletes the user and their listens, playlists, stars and devices. Fails with `Conflict` for
/// the last admin.
pub fn delete(tx: &mut WriteTx<'_>, key: UserKey) -> Result<(), DbError> {
    let current = get(tx, key)?.ok_or(DbError::NotFound)?;
    if current.role == Role::Admin
        && current.disabled_at.is_none()
        && other_active_admins(tx, key)? == 0
    {
        return Err(DbError::Conflict(String::from("last admin")));
    }
    super::playlists::delete_all_for(tx, key)?;
    tx.prepare_cached("DELETE FROM change_log WHERE user_id = ?1")?
        .execute([key])?;
    tx.prepare_cached("DELETE FROM users WHERE id = ?1")?
        .execute([key])?;
    Ok(())
}

/// Replaces the user's library grants and bumps their sync epoch when they changed.
pub fn set_libraries(
    tx: &mut WriteTx<'_>,
    key: UserKey,
    libraries: &[LibraryKey],
    granted_by: Option<UserKey>,
) -> Result<(), DbError> {
    if get(tx, key)?.is_none() {
        return Err(DbError::NotFound);
    }
    let before = self::libraries(tx, key)?;
    tx.prepare_cached("DELETE FROM library_access WHERE user_id = ?1")?
        .execute([key])?;
    grant(tx, key, libraries, granted_by)?;
    if self::libraries(tx, key)? != before {
        tx.prepare_cached(
            "UPDATE users SET sync_epoch = sync_epoch + 1, updated_at = ?2 WHERE id = ?1",
        )?
        .execute(params![key, tx.now()])?;
    }
    Ok(())
}

/// Active (not disabled) admins.
pub fn count_admins(c: &Connection) -> Result<u32, DbError> {
    let n: i64 = c
        .prepare_cached("SELECT count(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL")?
        .query_row([], |row| row.get(0))?;
    Ok(to_u32(n))
}

pub fn count(c: &Connection) -> Result<u64, DbError> {
    let n: i64 = c
        .prepare_cached("SELECT count(*) FROM users")?
        .query_row([], |row| row.get(0))?;
    Ok(to_u64(n))
}

/// Bumps every user's sync epoch, after a change that rewrites ids or keys.
pub fn bump_all_epochs(tx: &mut WriteTx<'_>) -> Result<(), DbError> {
    tx.prepare_cached("UPDATE users SET sync_epoch = sync_epoch + 1")?
        .execute([])?;
    Ok(())
}
