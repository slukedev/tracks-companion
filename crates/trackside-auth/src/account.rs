//! Accounts: login, sessions, password changes and resets by the CLI, and user management
//! with the last-admin rule.

use std::net::IpAddr;

use rusqlite::Connection;
use trackside_core::repo::audit::{self, AuditEntry};
use trackside_core::repo::sessions::{self, NewSession};
use trackside_core::repo::users::{self, User, UserPatch};
use trackside_core::repo::{devices, libraries};
use trackside_core::{
    Access, DbError, LibraryId, LibraryKey, PublicId, Role, UserId, UserKey, WriteTx,
};
use zeroize::Zeroizing;

use crate::Principal;
use crate::limit::Account;
use crate::password::validate_new_password;
use crate::token::{self, sha256};
use crate::{Auth, AuthError, NewUser, SessionCookie};

const MAX_USERNAME_CHARS: usize = 64;
const MAX_DISPLAY_NAME_CHARS: usize = 100;
const MAX_USER_AGENT_CHARS: usize = 256;

impl Auth {
    /// Argon2id on the blocking pool behind a semaphore.
    pub async fn hash_password(&self, password: &str) -> Result<String, AuthError> {
        validate_new_password(password)?;
        self.hasher.hash(Zeroizing::new(password.to_owned())).await
    }

    /// Checks a username and password. Unknown users, disabled users and wrong passwords
    /// fail the same way and take the same time. Repeated failures back off per account and
    /// address, and failures count against the address's limit.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
        ip: IpAddr,
    ) -> Result<UserKey, AuthError> {
        self.check_ip(ip)?;
        let typed = username.to_owned();
        let found = self
            .read(move |c| {
                let Some(user) = users::by_username(c, &typed)? else {
                    return Ok(None);
                };
                let hash = users::password_hash(c, user.key)?;
                Ok(Some((user, hash)))
            })
            .await?;
        let account = Account::new(found.as_ref().map(|(u, _)| u.key), username);
        self.backoff
            .check(&account, ip)
            .map_err(AuthError::rate_limited)?;
        let (user, hash) = match found {
            Some((user, hash)) => (Some(user), hash),
            None => (None, None),
        };
        let password_fits = crate::PASSWORD_BYTES.contains(&password.len());
        let matched = self
            .hasher
            .verify(
                Zeroizing::new(if password_fits { password } else { "" }.to_owned()),
                hash,
            )
            .await?;
        match user {
            Some(user) if matched && password_fits && user.disabled_at.is_none() => {
                self.backoff.succeed(&account, ip);
                Ok(user.key)
            }
            _ => {
                self.backoff.fail(&account, ip);
                self.fail_ip(username, ip, "password")?;
                Err(AuthError::Unauthenticated)
            }
        }
    }

    /// Starts a dashboard session. The cookie value is returned once; only its hash is
    /// stored.
    pub async fn create_session(
        &self,
        user: UserKey,
        ip: IpAddr,
        user_agent: Option<&str>,
    ) -> Result<SessionCookie, AuthError> {
        let value = token::url_token()?;
        let now = self.now();
        let expires_at = now.saturating_add(self.config.session_max);
        let new = NewSession {
            token_hash: sha256(value.as_bytes()),
            user,
            idle_expires_at: now.saturating_add(self.config.session_idle).min(expires_at),
            expires_at,
            ip: Some(ip.to_string()),
            user_agent: user_agent.map(|ua| ua.chars().take(MAX_USER_AGENT_CHARS).collect()),
        };
        self.write(move |tx| {
            active_user(tx.conn(), user)?;
            sessions::insert(tx, &new)
        })
        .await?;
        Ok(SessionCookie {
            value: value.to_string(),
            expires_at,
        })
    }

    /// `POST /api/v1/auth/reauth`: checks the session user's password again, with the login
    /// backoff, and marks the session as recently authenticated.
    pub async fn reauth(
        &self,
        principal: &Principal,
        password: &str,
        ip: IpAddr,
    ) -> Result<(), AuthError> {
        let Some(session) = principal.session else {
            return Err(AuthError::Forbidden);
        };
        let user = self.login(&principal.username, password, ip).await?;
        if user != principal.access.user {
            return Err(AuthError::Unauthenticated);
        }
        self.write(move |tx| sessions::mark_recent_auth(tx, session.key))
            .await
    }

    /// Signs a browser out. Unknown cookies are not an error.
    pub async fn end_session(&self, cookie_value: &str) -> Result<(), AuthError> {
        let hash = sha256(cookie_value.as_bytes());
        self.write(move |tx| {
            if let Some(session) = sessions::by_hash(tx.conn(), &hash)? {
                sessions::delete(tx, session.key)?;
            }
            Ok(())
        })
        .await
    }

    /// `POST /api/v1/me/password`: checks the current password, sets the new one, and ends
    /// every session except `keep_session` (the caller's cookie). With `sign_out_devices`
    /// it also revokes every device.
    pub async fn change_password(
        &self,
        user: UserKey,
        current: &str,
        new_password: &str,
        keep_session: Option<&str>,
        sign_out_devices: bool,
        ip: IpAddr,
    ) -> Result<(), AuthError> {
        validate_new_password(new_password)?;
        self.check_ip(ip)?;
        let account = Account::User(user);
        self.backoff
            .check(&account, ip)
            .map_err(AuthError::rate_limited)?;
        let (record, hash) = self
            .read(move |c| Ok((active_user(c, user)?, users::password_hash(c, user)?)))
            .await?;
        let matched = self
            .hasher
            .verify(Zeroizing::new(current.to_owned()), hash)
            .await?;
        if !matched {
            self.backoff.fail(&account, ip);
            self.fail_ip(&record.username, ip, "password_change")?;
            return Err(AuthError::Forbidden);
        }
        self.backoff.succeed(&account, ip);
        let new_hash = self
            .hasher
            .hash(Zeroizing::new(new_password.to_owned()))
            .await?;
        let keep = keep_session.map(|cookie| sha256(cookie.as_bytes()));
        let revoked_devices = self
            .write(move |tx| {
                users::set_password(tx, user, &new_hash)?;
                let keep = match keep {
                    Some(hash) => sessions::by_hash(tx.conn(), &hash)?
                        .filter(|s| s.user == user)
                        .map(|s| s.key),
                    None => None,
                };
                sessions::delete_for_user(tx, user, keep)?;
                let revoked = if sign_out_devices {
                    revoke_all_devices(tx, user)?
                } else {
                    Vec::new()
                };
                log(
                    tx,
                    Some(&record),
                    Some(ip),
                    "user.password_change",
                    Some(record.id.as_str()),
                )?;
                Ok(revoked)
            })
            .await?;
        for device in revoked_devices {
            self.revoked(user, Some(device));
        }
        self.revoked(user, None);
        Ok(())
    }

    /// Creates a user (admin `POST /admin/users`). `by` is the acting admin.
    pub async fn create_user(
        &self,
        by: UserKey,
        user: NewUser,
        ip: Option<IpAddr>,
    ) -> Result<UserKey, AuthError> {
        let new = self.prepare_user(&user).await?;
        let libraries = user.libraries;
        self.write(move |tx| {
            let actor = active_admin(tx.conn(), by)?;
            let mut new = new;
            new.libraries = resolve_libraries(tx.conn(), &libraries)?;
            let created = users::create(tx, &new)?;
            log(
                tx,
                Some(&actor),
                ip,
                "user.create",
                Some(created.id.as_str()),
            )?;
            Ok(created.key)
        })
        .await
    }

    /// Validates the username and password and hashes the password.
    pub(crate) async fn prepare_user(&self, user: &NewUser) -> Result<users::NewUser, AuthError> {
        let username = validate_username(&user.username)?;
        let display_name = display_name(&user.display_name, &username);
        let password_hash = self.hash_password(&user.password).await?;
        Ok(users::NewUser {
            username,
            display_name,
            password_hash,
            role: user.role,
            permissions: user.permissions.clone(),
            libraries: Vec::new(),
            service: None,
        })
    }

    /// Changes a user (admin `PATCH /admin/users/{id}`, or `PATCH /me` for the display name).
    /// `by` is the acting user, `None` for the CLI. Only admins change roles, permissions or
    /// the disabled flag, and the last active admin cannot be demoted or disabled. Cached
    /// credentials of the user are dropped so the change applies to the next request.
    pub async fn update_user(
        &self,
        by: Option<UserKey>,
        user: UserKey,
        patch: UserPatch,
        ip: Option<IpAddr>,
    ) -> Result<User, AuthError> {
        let mut patch = patch;
        if let Some(name) = &patch.display_name {
            patch.display_name = Some(validate_display_name(name)?);
        }
        let updated = self
            .write(move |tx| {
                let actor = by.map(|by| active_user(tx.conn(), by)).transpose()?;
                let own_name_only = by == Some(user)
                    && patch.role.is_none()
                    && patch.permissions.is_none()
                    && patch.disabled.is_none();
                if let Some(actor) = &actor
                    && actor.role != Role::Admin
                    && !own_name_only
                {
                    return Err(DbError::Forbidden);
                }
                let current = users::get(tx.conn(), user)?.ok_or(DbError::NotFound)?;
                let demotes = current.role == Role::Admin && patch.role == Some(Role::Member);
                let disables = current.disabled_at.is_none() && patch.disabled == Some(true);
                if demotes || disables {
                    ensure_not_last_admin(tx.conn(), &current)?;
                }
                let updated = users::update(tx, user, &patch)?;
                log(
                    tx,
                    actor.as_ref(),
                    ip,
                    "user.update",
                    Some(current.id.as_str()),
                )?;
                Ok((updated, disables))
            })
            .await?;
        let (updated, disabled) = updated;
        if disabled {
            self.revoked(user, None);
        } else {
            self.access_changed(user);
        }
        Ok(updated)
    }

    /// Replaces a member's library grants; bumps their sync epoch.
    pub async fn set_libraries(
        &self,
        by: UserKey,
        user: UserKey,
        libraries: Vec<LibraryId>,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        self.write(move |tx| {
            let actor = active_admin(tx.conn(), by)?;
            let target = users::get(tx.conn(), user)?.ok_or(DbError::NotFound)?;
            let keys = resolve_libraries(tx.conn(), &libraries)?;
            users::set_libraries(tx, user, &keys, Some(by))?;
            log(
                tx,
                Some(&actor),
                ip,
                "user.libraries",
                Some(target.id.as_str()),
            )
        })
        .await?;
        self.access_changed(user);
        Ok(())
    }

    /// The first step of deleting a user whose data is purged in several transactions: in one
    /// write transaction, refuses the last active admin and disables the user. A disabled
    /// user no longer counts as an admin, so a concurrent demotion of the other admins is
    /// refused from here on and the final [`Self::delete_user`] cannot be refused after the
    /// purge has started.
    pub async fn begin_delete_user(
        &self,
        by: UserKey,
        user: UserKey,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        let disabled = self
            .write(move |tx| {
                let actor = active_admin(tx.conn(), by)?;
                let current = users::get(tx.conn(), user)?.ok_or(DbError::NotFound)?;
                ensure_not_last_admin(tx.conn(), &current)?;
                if current.disabled_at.is_some() {
                    return Ok(false);
                }
                let patch = UserPatch {
                    disabled: Some(true),
                    ..UserPatch::default()
                };
                users::update(tx, user, &patch)?;
                log(
                    tx,
                    Some(&actor),
                    ip,
                    "user.update",
                    Some(current.id.as_str()),
                )?;
                Ok(true)
            })
            .await?;
        if disabled {
            self.revoked(user, None);
        }
        Ok(())
    }

    /// Deletes a user and their data. Refused for the last active admin.
    pub async fn delete_user(
        &self,
        by: UserKey,
        user: UserKey,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        self.write(move |tx| {
            let actor = active_admin(tx.conn(), by)?;
            let current = users::get(tx.conn(), user)?.ok_or(DbError::NotFound)?;
            ensure_not_last_admin(tx.conn(), &current)?;
            users::delete(tx, user)?;
            log(
                tx,
                Some(&actor),
                ip,
                "user.delete",
                Some(current.id.as_str()),
            )
        })
        .await?;
        self.revoked(user, None);
        Ok(())
    }

    /// Ends a user's sessions; with `devices`, also revokes every device key.
    pub async fn sign_out_user(
        &self,
        by: Option<UserKey>,
        user: UserKey,
        devices: bool,
        ip: Option<IpAddr>,
    ) -> Result<(), AuthError> {
        let revoked = self
            .write(move |tx| {
                let actor = by.map(|by| active_user(tx.conn(), by)).transpose()?;
                let target = users::get(tx.conn(), user)?.ok_or(DbError::NotFound)?;
                sessions::delete_for_user(tx, user, None)?;
                let revoked = if devices {
                    revoke_all_devices(tx, user)?
                } else {
                    Vec::new()
                };
                log(
                    tx,
                    actor.as_ref(),
                    ip,
                    "user.sign_out",
                    Some(target.id.as_str()),
                )?;
                Ok(revoked)
            })
            .await?;
        for device in revoked {
            self.revoked(user, Some(device));
        }
        self.revoked(user, None);
        Ok(())
    }

    /// `trackside admin reset-password <username>`: sets a random one-time password, ends
    /// the user's sessions, marks the password for change at next sign-in, and audits as
    /// `cli`. Works while the server runs. Returns the one-time password.
    pub async fn admin_reset_password(
        &self,
        username: &str,
    ) -> Result<Zeroizing<String>, AuthError> {
        let password = token::app_password()?;
        let hash = self.hasher.hash(password.clone()).await?;
        let typed = username.to_owned();
        let user = self
            .write(move |tx| {
                let user = users::by_username(tx.conn(), &typed)?.ok_or(DbError::NotFound)?;
                users::set_password(tx, user.key, &hash)?;
                sessions::delete_for_user(tx, user.key, None)?;
                users::set_must_change_password(tx, user.key, true)?;
                audit::log(
                    tx,
                    &AuditEntry {
                        actor: None,
                        actor_name: String::from("cli"),
                        ip: None,
                        action: String::from("user.password_reset"),
                        target: Some(user.id.as_str().to_owned()),
                        detail: None,
                    },
                )?;
                Ok(user.key)
            })
            .await?;
        self.revoked(user, None);
        Ok(password)
    }

    /// Whether the user signed in with a one-time password and must choose a new one.
    pub async fn password_change_required(&self, user: &UserId) -> Result<bool, AuthError> {
        let user = user.clone();
        self.read(move |c| Ok(users::by_pid(c, &user)?.is_some_and(|u| u.must_change_password)))
            .await
    }
}

fn revoke_all_devices(
    tx: &mut WriteTx<'_>,
    user: UserKey,
) -> Result<Vec<trackside_core::DeviceKey>, DbError> {
    let mut revoked = Vec::new();
    for device in devices::list(tx.conn(), Some(user))? {
        if device.revoked_at.is_none() {
            devices::revoke(tx, Some(user), &device.id)?;
            revoked.push(device.key);
        }
    }
    Ok(revoked)
}

/// Refuses to remove the last active admin by demotion, disabling or deletion.
pub(crate) fn ensure_not_last_admin(c: &Connection, user: &User) -> Result<(), DbError> {
    if user.role == Role::Admin && user.disabled_at.is_none() && users::count_admins(c)? <= 1 {
        return Err(DbError::Conflict(String::from("last_admin")));
    }
    Ok(())
}

pub(crate) fn active_user(c: &Connection, user: UserKey) -> Result<User, DbError> {
    users::get(c, user)?
        .filter(|u| u.disabled_at.is_none())
        .ok_or(DbError::NotFound)
}

pub(crate) fn active_admin(c: &Connection, user: UserKey) -> Result<User, DbError> {
    let user = active_user(c, user)?;
    if user.role == Role::Admin {
        Ok(user)
    } else {
        Err(DbError::Forbidden)
    }
}

pub(crate) fn resolve_libraries(
    c: &Connection,
    ids: &[LibraryId],
) -> Result<Vec<LibraryKey>, DbError> {
    let system = Access::system();
    ids.iter()
        .map(|id| {
            libraries::get(c, &system, id)?
                .map(|library| library.key)
                .ok_or_else(|| DbError::Invalid(format!("unknown library {id}")))
        })
        .collect()
}

/// Writes an audit entry for `actor` (none means the system).
pub(crate) fn log(
    tx: &mut WriteTx<'_>,
    actor: Option<&User>,
    ip: Option<IpAddr>,
    action: &str,
    target: Option<&str>,
) -> Result<(), DbError> {
    audit::log(
        tx,
        &AuditEntry {
            actor: actor.map(|a| a.key),
            actor_name: actor.map_or_else(|| String::from("system"), |a| a.username.clone()),
            ip: ip.map(|ip| ip.to_string()),
            action: action.to_owned(),
            target: target.map(str::to_owned),
            detail: None,
        },
    )
}

/// Trims and checks a new username: 1 to 64 characters, no spaces or control characters.
pub(crate) fn validate_username(username: &str) -> Result<String, AuthError> {
    let username = username.trim();
    let chars = username.chars().count();
    if chars == 0 || chars > MAX_USERNAME_CHARS {
        return Err(AuthError::Invalid(format!(
            "usernames are 1 to {MAX_USERNAME_CHARS} characters"
        )));
    }
    if username
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(AuthError::Invalid(String::from(
            "usernames cannot contain spaces or control characters",
        )));
    }
    Ok(username.to_owned())
}

fn validate_display_name(name: &str) -> Result<String, AuthError> {
    let name = name.trim();
    if name.chars().count() > MAX_DISPLAY_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(AuthError::Invalid(format!(
            "display names are at most {MAX_DISPLAY_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

pub(crate) fn display_name(display_name: &str, username: &str) -> String {
    match validate_display_name(display_name) {
        Ok(name) if !name.is_empty() => name,
        _ => username.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_trimmed_and_checked() {
        assert_eq!(validate_username("  luke ").ok().as_deref(), Some("luke"));
        assert!(validate_username("").is_err());
        assert!(validate_username("two words").is_err());
        assert!(validate_username("bell\u{7}").is_err());
        assert!(validate_username(&"x".repeat(65)).is_err());
        assert!(validate_username("Zoë").is_ok());
    }

    #[test]
    fn empty_display_name_falls_back_to_username() {
        assert_eq!(display_name("  ", "luke"), "luke");
        assert_eq!(display_name(" Luke ", "luke"), "Luke");
    }
}
