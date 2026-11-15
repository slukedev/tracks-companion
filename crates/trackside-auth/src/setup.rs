//! First-run setup with the one-time token, invites, and password reset links.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use trackside_core::repo::invites::{self, InviteRow};
use trackside_core::repo::password_resets::{self, PasswordReset};
use trackside_core::repo::{sessions, settings, users};
use trackside_core::{DbError, Millis, Permissions, PublicId, Role, UserKey};

use crate::account::{active_admin, log, resolve_libraries};
use crate::secret::replace_private_file;
use crate::token::{self, ct_eq, hex, normalize_crockford, sha256};
use crate::{
    Auth, AuthError, IssuedInvite, IssuedReset, NewInvite, NewUser, SetupState,
    password::validate_new_password,
};

/// Settings key holding the SHA-256 (hex) of the current setup token.
const SETUP_TOKEN_HASH: &str = "setup.token_hash";
/// File name of the setup token in the data directory.
pub const SETUP_TOKEN_FILE: &str = "setup-token";
const DEFAULT_INVITE_LIFETIME: Duration = Duration::from_secs(7 * 86_400);
const INVITE_LIFETIME: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(3600)..=Duration::from_secs(90 * 86_400);
const RESET_LIFETIME: Duration = Duration::from_secs(86_400);
const MAX_NOTE_CHARS: usize = 500;

/// The banner printed at start while no admin exists (architecture section 7).
pub fn setup_banner(token: &str, setup_url: &str, token_file: &Path) -> String {
    format!(
        "Trackside needs an admin account. Open {setup_url} and enter this setup token:\n  {token}\nThe token is also in {}. It stops working once the admin exists.",
        token_file.display()
    )
}

fn token_hash(token: &str) -> Option<String> {
    let normalized = normalize_crockford(token)?;
    (normalized.len() == 52).then(|| hex(&sha256(normalized.as_bytes())))
}

impl Auth {
    /// While no admin exists, makes sure a setup token exists in `<data>/setup-token` (mode
    /// 0600) and returns it for the banner; the database keeps only its hash. Once an admin
    /// exists, removes the file and returns `None`.
    pub async fn ensure_setup_token(&self, data_dir: &Path) -> Result<Option<String>, AuthError> {
        let path = data_dir.join(SETUP_TOKEN_FILE);
        if self.setup_state().await? == SetupState::Done {
            remove_token_file(path).await?;
            return Ok(None);
        }
        *crate::limit::lock(&self.setup_token_file) = Some(path.clone());
        let read_path = path.clone();
        let existing = blocking(move || match std::fs::read_to_string(&read_path) {
            Ok(text) => Ok(Some(text.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        })
        .await?;
        let token = match existing.filter(|t| token_hash(t).is_some()) {
            Some(token) => token,
            None => {
                let token = token::setup_token()?.to_string();
                let bytes = format!("{token}\n");
                blocking(move || replace_private_file(&path, bytes.as_bytes())).await?;
                token
            }
        };
        self.store_setup_token_hash(&token).await?;
        Ok(Some(token))
    }

    /// `trackside admin create-setup-token`: replaces the setup token while no admin exists.
    pub async fn rotate_setup_token(&self, data_dir: &Path) -> Result<String, AuthError> {
        if self.setup_state().await? == SetupState::Done {
            return Err(AuthError::Conflict(String::from("an admin already exists")));
        }
        let path = data_dir.join(SETUP_TOKEN_FILE);
        let token = token::setup_token()?.to_string();
        let bytes = format!("{token}\n");
        let write_path = path.clone();
        blocking(move || replace_private_file(&write_path, bytes.as_bytes())).await?;
        *crate::limit::lock(&self.setup_token_file) = Some(path);
        self.store_setup_token_hash(&token).await?;
        Ok(token)
    }

    async fn store_setup_token_hash(&self, token: &str) -> Result<(), AuthError> {
        let hash = token_hash(token)
            .ok_or_else(|| AuthError::Invalid(String::from("malformed setup token")))?;
        self.write(move |tx| settings::put(tx, SETUP_TOKEN_HASH, &hash))
            .await
    }

    /// Records that an admin exists, for admins created at start from
    /// `TRACKSIDE_ADMIN_USERNAME` or by the CLI.
    pub async fn mark_setup_done(&self) -> Result<(), AuthError> {
        self.write(|tx| {
            settings::put(tx, settings::keys::SETUP_STATE, "done")?;
            settings::delete(tx, SETUP_TOKEN_HASH)
        })
        .await?;
        self.setup_done.store(true, Ordering::Release);
        self.remove_known_token_file().await
    }

    /// `POST /api/v1/setup`: creates the first admin with the setup token, once.
    pub async fn complete_setup(
        &self,
        token: &str,
        admin: NewUser,
        ip: IpAddr,
    ) -> Result<UserKey, AuthError> {
        self.check_ip(ip)?;
        if self.setup_state().await? == SetupState::Done {
            return Err(AuthError::Conflict(String::from("an admin already exists")));
        }
        let presented = token_hash(token);
        let stored: Option<String> = self.read(|c| settings::get(c, SETUP_TOKEN_HASH)).await?;
        if !token_matches(stored.as_deref(), presented.as_deref()) {
            self.fail_ip(&admin.username, ip, "setup_token")?;
            return Err(AuthError::Unauthenticated);
        }
        let mut new = self
            .prepare_user(&NewUser {
                role: Role::Admin,
                permissions: Permissions::default(),
                libraries: Vec::new(),
                ..admin
            })
            .await?;
        new.role = Role::Admin;
        let user = self
            .write(move |tx| {
                let stored: Option<String> = settings::get(tx.conn(), SETUP_TOKEN_HASH)?;
                let state: Option<String> = settings::get(tx.conn(), settings::keys::SETUP_STATE)?;
                if state.as_deref() == Some("done") || users::count_admins(tx.conn())? > 0 {
                    return Err(DbError::Conflict(String::from("an admin already exists")));
                }
                if !token_matches(stored.as_deref(), presented.as_deref()) {
                    return Err(DbError::Conflict(String::from("the setup token changed")));
                }
                let created = users::create(tx, &new)?;
                settings::put(tx, settings::keys::SETUP_STATE, "done")?;
                settings::delete(tx, SETUP_TOKEN_HASH)?;
                log(
                    tx,
                    Some(&created),
                    Some(ip),
                    "setup.complete",
                    Some(created.id.as_str()),
                )?;
                Ok(created.key)
            })
            .await?;
        self.setup_done.store(true, Ordering::Release);
        self.remove_known_token_file().await?;
        Ok(user)
    }

    async fn remove_known_token_file(&self) -> Result<(), AuthError> {
        let path = crate::limit::lock(&self.setup_token_file).take();
        match path {
            Some(path) => remove_token_file(path).await,
            None => Ok(()),
        }
    }

    /// `POST /admin/invites`. `by` must be an active admin.
    pub async fn create_invite(
        &self,
        by: UserKey,
        invite: NewInvite,
    ) -> Result<IssuedInvite, AuthError> {
        let lifetime = if invite.expires_in.is_zero() {
            DEFAULT_INVITE_LIFETIME
        } else {
            invite
                .expires_in
                .clamp(*INVITE_LIFETIME.start(), *INVITE_LIFETIME.end())
        };
        let note = invite
            .note
            .map(|n| n.trim().chars().take(MAX_NOTE_CHARS).collect::<String>())
            .filter(|n| !n.is_empty());
        let token = token::url_token()?;
        let token_hash = sha256(token.as_bytes());
        let expires_at = self.now().saturating_add(lifetime);
        let row = self
            .write(move |tx| {
                let actor = active_admin(tx.conn(), by)?;
                let libraries = resolve_libraries(tx.conn(), &invite.libraries)?;
                let row = invites::insert(
                    tx,
                    &invites::NewInvite {
                        token_hash,
                        created_by: Some(by),
                        role: invite.role,
                        permissions: invite.permissions.clone(),
                        note,
                        libraries,
                        expires_at,
                    },
                )?;
                log(
                    tx,
                    Some(&actor),
                    None,
                    "invite.create",
                    Some(row.id.as_str()),
                )?;
                Ok(row)
            })
            .await?;
        Ok(IssuedInvite {
            id: row.id,
            token: token.to_string(),
            expires_at: row.expires_at,
        })
    }

    /// `GET /api/v1/invites/{token}`: the invite while it can still be accepted, else
    /// `NotFound`.
    pub async fn invite_info(&self, token: &str) -> Result<InviteRow, AuthError> {
        let hash = sha256(token.as_bytes());
        let now = self.now();
        self.read(move |c| invites::by_hash(c, &hash))
            .await?
            .filter(|i| invite_open(i, now))
            .ok_or(AuthError::NotFound)
    }

    /// `POST /api/v1/invites/{token}/accept`: creates the user with the invite's role,
    /// permissions and libraries (what the invitee sends for those is ignored). Single use,
    /// even when two acceptances race.
    pub async fn accept_invite(
        &self,
        token: &str,
        user: NewUser,
        ip: IpAddr,
    ) -> Result<UserKey, AuthError> {
        self.check_ip(ip)?;
        let hash = sha256(token.as_bytes());
        let now = self.now();
        let invite = self.read(move |c| invites::by_hash(c, &hash)).await?;
        let Some(invite) = invite.filter(|i| invite_open(i, now)) else {
            self.fail_ip(&user.username, ip, "invite")?;
            return Err(AuthError::NotFound);
        };
        let mut new = self
            .prepare_user(&NewUser {
                role: invite.role,
                ..user
            })
            .await?;
        new.role = invite.role;
        new.permissions = invite.permissions.clone();
        new.libraries.clone_from(&invite.libraries);
        let created = self
            .write(move |tx| {
                let current = invites::by_hash(tx.conn(), &hash)?
                    .filter(|i| invite_open(i, now))
                    .ok_or(DbError::NotFound)?;
                let created = users::create(tx, &new)?;
                invites::accept(tx, &current.id, created.key).map_err(|e| match e {
                    DbError::Conflict(_) => DbError::NotFound,
                    other => other,
                })?;
                log(
                    tx,
                    Some(&created),
                    Some(ip),
                    "invite.accept",
                    Some(current.id.as_str()),
                )?;
                Ok(created.key)
            })
            .await?;
        Ok(created)
    }

    /// `POST /admin/users/{id}/password-reset`: a link valid for 24 hours, once.
    pub async fn create_password_reset(
        &self,
        by: UserKey,
        user: UserKey,
    ) -> Result<IssuedReset, AuthError> {
        let token = token::url_token()?;
        let hash = sha256(token.as_bytes());
        let expires_at = self.now().saturating_add(RESET_LIFETIME);
        self.write(move |tx| {
            let actor = active_admin(tx.conn(), by)?;
            let target = users::get(tx.conn(), user)?.ok_or(DbError::NotFound)?;
            password_resets::insert(tx, &hash, user, Some(by), expires_at)?;
            log(
                tx,
                Some(&actor),
                None,
                "user.reset_link",
                Some(target.id.as_str()),
            )
        })
        .await?;
        Ok(IssuedReset {
            token: token.to_string(),
            expires_at,
        })
    }

    /// `GET /api/v1/password-resets/{token}`: the reset while it is usable, else `NotFound`.
    pub async fn password_reset_info(&self, token: &str) -> Result<PasswordReset, AuthError> {
        let hash = sha256(token.as_bytes());
        let now = self.now();
        self.read(move |c| password_resets::by_hash(c, &hash))
            .await?
            .filter(|r| reset_open(r, now))
            .ok_or(AuthError::NotFound)
    }

    /// Sets the new password and ends all the user's sessions. Device keys stay; revoking
    /// them is a separate choice.
    pub async fn use_password_reset(
        &self,
        token: &str,
        new_password: &str,
    ) -> Result<(), AuthError> {
        validate_new_password(new_password)?;
        let reset = self.password_reset_info(token).await?;
        let new_hash = self.hash_password(new_password).await?;
        let hash = sha256(token.as_bytes());
        let now = self.now();
        let user = self
            .write(move |tx| {
                let current = password_resets::by_hash(tx.conn(), &hash)?
                    .filter(|r| reset_open(r, now) && r.id == reset.id)
                    .ok_or(DbError::NotFound)?;
                password_resets::mark_used(tx, current.id).map_err(|e| match e {
                    DbError::Conflict(_) => DbError::NotFound,
                    other => other,
                })?;
                users::set_password(tx, current.user, &new_hash)?;
                sessions::delete_for_user(tx, current.user, None)?;
                let target = users::get(tx.conn(), current.user)?.ok_or(DbError::NotFound)?;
                log(
                    tx,
                    Some(&target),
                    None,
                    "user.password_reset",
                    Some(target.id.as_str()),
                )?;
                Ok(current.user)
            })
            .await?;
        self.revoked(user, None);
        Ok(())
    }
}

fn token_matches(stored: Option<&str>, presented: Option<&str>) -> bool {
    match (stored, presented) {
        (Some(stored), Some(presented)) => ct_eq(stored.as_bytes(), presented.as_bytes()),
        _ => false,
    }
}

fn invite_open(invite: &InviteRow, now: Millis) -> bool {
    invite.used_at.is_none() && invite.revoked_at.is_none() && invite.expires_at > now
}

fn reset_open(reset: &PasswordReset, now: Millis) -> bool {
    reset.used_at.is_none() && reset.expires_at > now
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, AuthError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| AuthError::Io(std::io::Error::other(e)))?
        .map_err(AuthError::Io)
}

async fn remove_token_file(path: PathBuf) -> Result<(), AuthError> {
    blocking(move || match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    })
    .await
}
