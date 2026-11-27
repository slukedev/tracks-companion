//! Device keys, API keys and app passwords: issuing and revoking.

use std::net::IpAddr;

use trackside_core::repo::devices::{self, Device};
use trackside_core::repo::keys::{self, ApiKeyRow, NewKey};
use trackside_core::{DbError, DeviceId, KeyId, PublicId, Role, UserKey, WriteTx};

use crate::account::{active_user, log};
use crate::token::{self, sha256, shown_prefix};
use crate::{
    Auth, AuthError, IssuedDeviceKey, IssuedKey, KeyKind, NewDevice, Scopes, Via, load_principal,
};

/// What sealed app passwords are sealed for.
pub(crate) const APP_PASSWORD_PURPOSE: &str = "app password";
const PLATFORMS: &[&str] = &["ios", "macos", "android", "android_tv", "web", "other"];
const MAX_NAME_CHARS: usize = 100;
const MAX_CLIENT_ID_CHARS: usize = 200;

impl Auth {
    /// Creates or refreshes the device and issues it a new key, revoking its previous one.
    /// The same install id under two users gives two devices with separate keys.
    pub async fn issue_device_key(
        &self,
        user: UserKey,
        device: NewDevice,
    ) -> Result<IssuedDeviceKey, AuthError> {
        let device = validate_device(device)?;
        let secret = token::api_key()?;
        let hidden = secret.clone();
        let (row, device) = self
            .write(move |tx| {
                let actor = active_user(tx.conn(), user)?;
                let issued = issue_in_tx(tx, user, &device, &hidden, None, Scopes::ALL)?;
                log(
                    tx,
                    Some(&actor),
                    None,
                    "device.sign_in",
                    Some(issued.1.id.as_str()),
                )?;
                Ok(issued)
            })
            .await?;
        self.finish_issue(user, secret.to_string(), row, device)
            .await
    }

    /// Builds the result of a key issued in a transaction, remembering the device's media
    /// epoch and dropping anything cached for the device's previous key.
    pub(crate) async fn finish_issue(
        &self,
        user: UserKey,
        secret: String,
        row: ApiKeyRow,
        device: Device,
    ) -> Result<IssuedDeviceKey, AuthError> {
        self.revoked(user, Some(device.key));
        self.remember_epoch(device.key, device.media_epoch);
        let scopes = Scopes::parse(&row.scopes).unwrap_or(Scopes::NONE);
        let handle = (device.key, device.id.clone());
        let mut principal = self
            .read(move |c| load_principal(c, user, scopes, Via::DeviceKey, Some(handle)))
            .await?
            .ok_or(AuthError::Unauthenticated)?;
        principal.key = Some(row.id.clone());
        Ok(IssuedDeviceKey {
            secret,
            key: row.id,
            device: device.id,
            device_name: device.name,
            principal,
        })
    }

    /// Creates an API key or an app password. App passwords need the server's
    /// `auth.legacy_subsonic` and the user's `legacy_subsonic` permission.
    pub async fn create_key(
        &self,
        user: UserKey,
        kind: KeyKind,
        name: &str,
        scopes: Scopes,
    ) -> Result<IssuedKey, AuthError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Err(AuthError::Invalid(format!(
                "key names are 1 to {MAX_NAME_CHARS} characters"
            )));
        }
        if scopes == Scopes::NONE {
            return Err(AuthError::Invalid(String::from(
                "a key needs at least one scope",
            )));
        }
        let (secret, sealed) = match kind {
            KeyKind::Device => {
                return Err(AuthError::Invalid(String::from(
                    "device keys come from sign-in or pairing",
                )));
            }
            KeyKind::ApiKey => (token::api_key()?, None),
            KeyKind::AppPassword => {
                if !self.legacy_subsonic() {
                    return Err(AuthError::Forbidden);
                }
                let secret = token::app_password()?;
                let sealed = crate::seal(&self.secret, APP_PASSWORD_PURPOSE, secret.as_bytes())?;
                (secret, Some(sealed))
            }
        };
        let new = NewKey {
            user,
            device: None,
            kind,
            name: name.to_owned(),
            prefix: shown_prefix(&secret),
            secret_hash: sha256(secret.as_bytes()),
            secret_sealed: sealed,
            scopes: scopes.to_string(),
            expires_at: None,
        };
        let (row, username) = self
            .write(move |tx| {
                let owner = active_user(tx.conn(), user)?;
                let may_use_legacy = owner.role == Role::Admin || owner.permissions.legacy_subsonic;
                if kind == KeyKind::AppPassword && !may_use_legacy {
                    return Err(DbError::Forbidden);
                }
                let row = keys::insert(tx, &new)?;
                log(tx, Some(&owner), None, "key.create", Some(row.id.as_str()))?;
                Ok((row, owner.username))
            })
            .await?;
        Ok(IssuedKey {
            secret: secret.to_string(),
            key: row,
            username,
        })
    }

    /// Revokes a key. `user` restricts it to that user's keys; `None` is for admins.
    pub async fn revoke_key(&self, user: Option<UserKey>, key: &KeyId) -> Result<(), AuthError> {
        let id = key.clone();
        let row = self
            .write(move |tx| {
                let row = keys::revoke(tx, user, &id)?;
                let actor = user.map(|u| active_user(tx.conn(), u)).transpose()?;
                log(tx, actor.as_ref(), None, "key.revoke", Some(id.as_str()))?;
                Ok(row)
            })
            .await?;
        self.cache.forget_key(key);
        self.revoked(row.user, row.device);
        Ok(())
    }

    /// Revokes the device's key and media tokens. `user` restricts it to that user's
    /// devices; `None` is for admins.
    pub async fn revoke_device(
        &self,
        user: Option<UserKey>,
        device: &DeviceId,
    ) -> Result<(), AuthError> {
        let id = device.clone();
        let revoked = self
            .write(move |tx| {
                let revoked = devices::revoke(tx, user, &id)?;
                let actor = user.map(|u| active_user(tx.conn(), u)).transpose()?;
                log(tx, actor.as_ref(), None, "device.revoke", Some(id.as_str()))?;
                Ok(revoked)
            })
            .await?;
        self.revoked(revoked.user, Some(revoked.key));
        Ok(())
    }
}

/// Creates or refreshes the device and stores a new device key for it with `scopes`, inside
/// `tx`. Sign-in and pairing give every scope; a link gives what its approver had.
pub(crate) fn issue_in_tx(
    tx: &mut WriteTx<'_>,
    user: UserKey,
    device: &NewDevice,
    secret: &str,
    ip: Option<IpAddr>,
    scopes: Scopes,
) -> Result<(ApiKeyRow, Device), DbError> {
    let ip = ip.map(|ip| ip.to_string());
    let device = devices::upsert_for_login(tx, user, device, ip.as_deref())?;
    keys::revoke_for_device(tx, device.key)?;
    let row = keys::insert(
        tx,
        &NewKey {
            user,
            device: Some(device.key),
            kind: KeyKind::Device,
            name: device.name.clone(),
            prefix: shown_prefix(secret),
            secret_hash: sha256(secret.as_bytes()),
            secret_sealed: None,
            scopes: scopes.to_string(),
            expires_at: None,
        },
    )?;
    Ok((row, device))
}

/// Trims what a client says about itself; unknown platforms become `other`.
pub(crate) fn validate_device(device: NewDevice) -> Result<NewDevice, AuthError> {
    let client_device_id = device.client_device_id.trim().to_owned();
    if client_device_id.is_empty() || client_device_id.chars().count() > MAX_CLIENT_ID_CHARS {
        return Err(AuthError::Invalid(String::from(
            "client_device_id must be 1 to 200 characters",
        )));
    }
    let short = |text: Option<String>| {
        text.map(|t| t.trim().chars().take(MAX_NAME_CHARS).collect::<String>())
            .filter(|t| !t.is_empty())
    };
    let platform = device.platform.trim().to_ascii_lowercase();
    Ok(NewDevice {
        client_device_id,
        name: short(Some(device.name)).unwrap_or_else(|| String::from("Unnamed device")),
        platform: if PLATFORMS.contains(&platform.as_str()) {
            platform
        } else {
            String::from("other")
        },
        model: short(device.model),
        app_name: short(device.app_name),
        app_version: short(device.app_version),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(platform: &str, name: &str) -> NewDevice {
        NewDevice {
            client_device_id: String::from(" 8C2E "),
            name: name.to_owned(),
            platform: platform.to_owned(),
            model: Some(String::from("  ")),
            app_name: Some(String::from("Tracks")),
            app_version: None,
        }
    }

    #[test]
    fn devices_are_tidied() -> Result<(), AuthError> {
        let tidy = validate_device(device("iOS", "  Luke's iPhone "))?;
        assert_eq!(tidy.client_device_id, "8C2E");
        assert_eq!(tidy.platform, "ios");
        assert_eq!(tidy.name, "Luke's iPhone");
        assert_eq!(tidy.model, None);
        assert_eq!(validate_device(device("toaster", ""))?.platform, "other");
        assert_eq!(validate_device(device("web", " "))?.name, "Unnamed device");
        Ok(())
    }

    #[test]
    fn empty_install_id_is_refused() {
        let mut bad = device("ios", "x");
        bad.client_device_id = String::from("  ");
        assert!(validate_device(bad).is_err());
    }
}
