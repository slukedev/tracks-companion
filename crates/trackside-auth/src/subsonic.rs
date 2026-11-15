//! Subsonic credential checks (ADR 0005). `apiKey` takes any device key or API key. `u`+`t`+`s`
//! and `u`+`p` are checked only against the user's app passwords, which are opened from their
//! sealed copies; login passwords are never accepted here.

use std::net::IpAddr;

use md5::{Digest, Md5};
use trackside_core::repo::keys::{self, ApiKeyRow};
use trackside_core::repo::users;
use zeroize::Zeroizing;

use crate::credentials::APP_PASSWORD_PURPOSE;
use crate::limit::Account;
use crate::token::{ct_eq, hex, unhex};
use crate::{
    Auth, AuthError, Principal, RouteClass, Scopes, SubsonicAuthError, SubsonicCredentials, Via,
    load_principal,
};

fn present(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

enum Legacy<'a> {
    Token { token: &'a str, salt: &'a str },
    Password(Zeroizing<String>),
}

impl Auth {
    /// Verifies Subsonic credentials. Failures are rate-limited like native ones.
    pub async fn authenticate_subsonic(
        &self,
        params: &SubsonicCredentials,
        ip: IpAddr,
    ) -> Result<Principal, SubsonicAuthError> {
        let username = present(params.username.as_ref());
        let token = present(params.token.as_ref());
        let salt = present(params.salt.as_ref());
        let password = present(params.password.as_ref());
        if let Some(key) = present(params.api_key.as_ref()) {
            if username.is_some() || token.is_some() || salt.is_some() || password.is_some() {
                return Err(SubsonicAuthError::Conflicting);
            }
            self.check_ip(ip).map_err(subsonic_error)?;
            return self.subsonic_api_key(key, ip).await;
        }
        let Some(username) = username else {
            return Err(SubsonicAuthError::Missing);
        };
        let legacy = match (token, salt, password) {
            (Some(_), _, Some(_)) => return Err(SubsonicAuthError::Conflicting),
            (Some(token), Some(salt), None) => Legacy::Token { token, salt },
            (None, None, Some(password)) => Legacy::Password(
                decode_password(password).ok_or(SubsonicAuthError::WrongCredentials)?,
            ),
            _ => return Err(SubsonicAuthError::Missing),
        };
        self.check_ip(ip).map_err(subsonic_error)?;
        if !self.config.legacy_subsonic {
            return Err(SubsonicAuthError::TokenNotSupported);
        }
        self.subsonic_legacy(username, &legacy, ip).await
    }

    async fn subsonic_api_key(
        &self,
        key: &str,
        ip: IpAddr,
    ) -> Result<Principal, SubsonicAuthError> {
        match self.key_principal(key).await {
            Ok((principal, id)) => {
                self.key_used(&principal, &id, ip, RouteClass::Api)
                    .await
                    .map_err(subsonic_error)?;
                Ok(principal)
            }
            Err(AuthError::Unauthenticated) => {
                self.fail_ip("-", ip, "subsonic_api_key")
                    .map_err(subsonic_error)?;
                Err(SubsonicAuthError::InvalidApiKey)
            }
            Err(e) => Err(subsonic_error(e)),
        }
    }

    async fn subsonic_legacy(
        &self,
        username: &str,
        legacy: &Legacy<'_>,
        ip: IpAddr,
    ) -> Result<Principal, SubsonicAuthError> {
        let typed = username.to_owned();
        let found = self
            .read(move |c| {
                let Some(user) = users::by_username(c, &typed)? else {
                    return Ok(None);
                };
                let passwords = keys::app_passwords_for(c, user.key)?;
                Ok(Some((user, passwords)))
            })
            .await
            .map_err(subsonic_error)?;
        let account = Account::new(found.as_ref().map(|(u, _)| u.key), username);
        self.backoff
            .check(&account, ip)
            .map_err(|wait| SubsonicAuthError::RateLimited { retry_after: wait })?;
        let usable = found.filter(|(user, passwords)| {
            let allowed =
                user.role == trackside_core::Role::Admin || user.permissions.legacy_subsonic;
            user.disabled_at.is_none() && allowed && !passwords.is_empty()
        });
        // Unknown users answer like users without app passwords, so names cannot be probed.
        let Some((user, passwords)) = usable else {
            self.backoff.fail(&account, ip);
            self.fail_ip(username, ip, "subsonic")
                .map_err(subsonic_error)?;
            return Err(SubsonicAuthError::TokenNotSupported);
        };
        let now = self.now();
        let matched = passwords
            .into_iter()
            .filter(|(row, _)| row.revoked_at.is_none() && row.expires_at.is_none_or(|x| x > now))
            .find_map(|(row, sealed)| self.app_password_matches(&sealed, legacy).then_some(row));
        let Some(row) = matched else {
            self.backoff.fail(&account, ip);
            self.fail_ip(username, ip, "subsonic")
                .map_err(subsonic_error)?;
            return Err(SubsonicAuthError::WrongCredentials);
        };
        self.backoff.succeed(&account, ip);
        let principal = self.app_password_principal(user.key, &row).await?;
        self.touch_app_password(&row, ip).await;
        Ok(principal)
    }

    fn app_password_matches(&self, sealed: &[u8], legacy: &Legacy<'_>) -> bool {
        let Ok(password) = crate::open(&self.secret, APP_PASSWORD_PURPOSE, sealed) else {
            return false;
        };
        match legacy {
            Legacy::Token { token, salt } => {
                let mut input = Zeroizing::new(Vec::with_capacity(password.len() + salt.len()));
                input.extend_from_slice(&password);
                input.extend_from_slice(salt.as_bytes());
                let expected = hex(&Md5::digest(input.as_slice()));
                ct_eq(expected.as_bytes(), token.to_ascii_lowercase().as_bytes())
            }
            Legacy::Password(given) => ct_eq(&password, given.as_bytes()),
        }
    }

    async fn app_password_principal(
        &self,
        user: trackside_core::UserKey,
        row: &ApiKeyRow,
    ) -> Result<Principal, SubsonicAuthError> {
        let scopes = Scopes::parse(&row.scopes).unwrap_or(Scopes::NONE);
        let key = row.id.clone();
        let mut principal = self
            .read(move |c| load_principal(c, user, scopes, Via::AppPassword, None))
            .await
            .map_err(subsonic_error)?
            .ok_or(SubsonicAuthError::WrongCredentials)?;
        principal.key = Some(key);
        Ok(principal)
    }

    async fn touch_app_password(&self, row: &ApiKeyRow, ip: IpAddr) {
        let id = row.id.clone();
        let ip = ip.to_string();
        if let Err(e) = self
            .db
            .write(move |tx| keys::touch_last_used(tx, &id, Some(&ip)))
            .await
        {
            tracing::debug!(error = %e, "could not record app password use");
        }
    }
}

/// `p` is the password, or `enc:` and its hex.
fn decode_password(p: &str) -> Option<Zeroizing<String>> {
    match p.strip_prefix("enc:") {
        Some(hex) => String::from_utf8(unhex(hex)?.to_vec())
            .ok()
            .map(Zeroizing::new),
        None => Some(Zeroizing::new(p.to_owned())),
    }
}

fn subsonic_error(error: AuthError) -> SubsonicAuthError {
    match error {
        AuthError::RateLimited { retry_after } => SubsonicAuthError::RateLimited { retry_after },
        AuthError::Unauthenticated | AuthError::MediaTokenExpired => {
            SubsonicAuthError::WrongCredentials
        }
        AuthError::Forbidden => SubsonicAuthError::InvalidApiKey,
        other => {
            tracing::error!(error = %other, "subsonic authentication failed internally");
            SubsonicAuthError::Internal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_passwords_are_hex() {
        assert_eq!(
            decode_password("enc:736573616d65")
                .as_deref()
                .map(String::as_str),
            Some("sesame")
        );
        assert_eq!(
            decode_password("sesame").as_deref().map(String::as_str),
            Some("sesame")
        );
        assert!(decode_password("enc:zz").is_none());
    }

    #[test]
    fn subsonic_token_is_md5_of_password_and_salt() {
        // The example from the Subsonic API documentation.
        assert_eq!(
            hex(&Md5::digest(b"sesamec19b2d")),
            "26719a1196d2a940705a59634eb18eab"
        );
    }
}
