//! Authenticating native API requests: bearer keys, session cookies and media tokens.

use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use http::request::Parts;
use rusqlite::Connection;
use trackside_core::repo::keys::{ApiKeyRow, KeyKind};
use trackside_core::repo::{devices, keys, sessions, settings, users};
use trackside_core::{DbError, DeviceId, DeviceKey, KeyId, Millis, PublicId, UserId};

use crate::media_token::{self, Claims};
use crate::token::{KEY_PREFIX, sha256, tagged_sha256};
use crate::{
    Auth, AuthError, CLOCK_SKEW, Principal, RouteClass, Scopes, SetupState, Via, load_principal,
    request,
};

/// How often last-use times are written for a busy key or session.
pub(crate) const TOUCH_EVERY: Duration = Duration::from_secs(60);
const MAX_KEY_LEN: usize = 128;
/// What a media token may do: play and download, never more.
const MEDIA_SCOPES: Scopes = Scopes::STREAM.union(Scopes::DOWNLOAD);

impl Auth {
    /// Authenticates a native API request by bearer key, session cookie, or (media routes only)
    /// `mt`. Session requests that change state must pass the CSRF and Origin check. Admin
    /// routes accept only sessions; whether the user is an admin is the extractor's check.
    pub async fn authenticate(
        &self,
        parts: &Parts,
        ip: IpAddr,
        class: RouteClass,
    ) -> Result<Principal, AuthError> {
        if class != RouteClass::Media && request::credential_in_query(parts) {
            return Err(AuthError::CredentialInQuery);
        }
        if let Some(token) = request::bearer(parts) {
            if class == RouteClass::Admin {
                return Err(AuthError::Forbidden);
            }
            self.require_secure_transport(parts)?;
            return self.authenticate_key(token, ip, class).await;
        }
        if class == RouteClass::Media
            && let Some(token) = request::query_param(parts, "mt")
        {
            return self.authenticate_media_token(&token).await;
        }
        if let Some(cookie) = request::session_cookie(parts) {
            // The plain cookie is only ever set for nearby clients. Reaching a public address
            // over plain HTTP with it (split-horizon DNS, a laptop that left home) would send
            // the session in the clear, so it is refused and cleared.
            if cookie.plain && !self.secrets_allowed(parts) {
                return Err(AuthError::HttpsRequired);
            }
            request::check_csrf(parts, self.trusted_forwarded(parts, "x-forwarded-host"))?;
            return self.authenticate_session(cookie.value).await;
        }
        if self.setup_state().await? == SetupState::NeedsAdmin {
            return Err(AuthError::SetupRequired);
        }
        Err(AuthError::Unauthenticated)
    }

    /// Whether the first admin exists. Once it does, the answer is kept in memory.
    pub async fn setup_state(&self) -> Result<SetupState, AuthError> {
        if self.setup_done.load(Ordering::Acquire) {
            return Ok(SetupState::Done);
        }
        let state: Option<String> = self
            .read(|c| settings::get(c, settings::keys::SETUP_STATE))
            .await?;
        if state.as_deref() == Some("done") {
            self.setup_done.store(true, Ordering::Release);
            Ok(SetupState::Done)
        } else {
            Ok(SetupState::NeedsAdmin)
        }
    }

    pub(crate) async fn authenticate_key(
        &self,
        token: &str,
        ip: IpAddr,
        class: RouteClass,
    ) -> Result<Principal, AuthError> {
        let (principal, key) = self.key_principal(token).await.inspect_err(|e| {
            if matches!(e, AuthError::Unauthenticated) {
                tracing::warn!("auth_failure user=- ip={ip} via=key");
            }
        })?;
        self.key_used(&principal, &key, ip, class).await?;
        Ok(principal)
    }

    /// Applies the per-key request rate (not on media routes) and records use.
    pub(crate) async fn key_used(
        &self,
        principal: &Principal,
        key: &trackside_core::KeyId,
        ip: IpAddr,
        class: RouteClass,
    ) -> Result<(), AuthError> {
        if class != RouteClass::Media {
            self.key_requests
                .check(key, &self.clock)
                .map_err(AuthError::rate_limited)?;
        }
        self.touch_key(key, principal.device.as_ref().map(|(d, _)| *d), ip)
            .await;
        Ok(())
    }

    /// The principal of a device key or API key (not an app password), from the cache when
    /// it was seen in the last second.
    pub(crate) async fn key_principal(
        &self,
        token: &str,
    ) -> Result<(Principal, trackside_core::KeyId), AuthError> {
        if !token.starts_with(KEY_PREFIX) || token.len() > MAX_KEY_LEN {
            return Err(AuthError::Unauthenticated);
        }
        let hash = sha256(token.as_bytes());
        let now = self.now();
        if let Some(hit) = self.cache.get(&hash, now)
            && let Some(key) = hit.key
        {
            return Ok((hit.principal, key));
        }
        let found = self.read(move |c| key_lookup(c, &hash, now)).await?;
        let Some((principal, row, epoch)) = found else {
            return Err(AuthError::Unauthenticated);
        };
        if let (Some((device, _)), Some(epoch)) = (&principal.device, epoch) {
            self.remember_epoch(*device, epoch);
        }
        self.cache
            .put(hash, principal.clone(), Some(row.id.clone()), None, now);
        Ok((principal, row.id))
    }

    async fn touch_key(&self, key: &trackside_core::KeyId, device: Option<DeviceKey>, ip: IpAddr) {
        let now = self.now();
        {
            let mut touched = crate::limit::lock(&self.touched);
            if touched
                .get(key)
                .is_some_and(|last| *last <= now && now.since(*last) < TOUCH_EVERY)
            {
                return;
            }
            if touched.len() > 100_000 {
                touched.clear();
            }
            touched.insert(key.clone(), now);
        }
        let key = key.clone();
        let ip = ip.to_string();
        let result = self
            .db
            .write(move |tx| {
                keys::touch_last_used(tx, &key, Some(&ip))?;
                if let Some(device) = device {
                    devices::touch(tx, device, Some(&ip))?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::debug!(error = %e, "could not record key use");
        }
    }

    pub(crate) async fn authenticate_media_token(
        &self,
        token: &str,
    ) -> Result<Principal, AuthError> {
        let Some(claims) = media_token::verify(&self.media_key, token) else {
            return Err(AuthError::Unauthenticated);
        };
        let now = self.now();
        if now.0 > claims.x {
            return Err(AuthError::MediaTokenExpired);
        }
        let latest = now.saturating_add(self.config.media_token_ttl + CLOCK_SKEW);
        if claims.x > latest.0 {
            return Err(AuthError::Unauthenticated);
        }
        let hash = tagged_sha256("media token", token.as_bytes());
        if let Some(hit) = self.cache.get(&hash, now) {
            return Ok(hit.principal);
        }
        let expires = Millis(claims.x);
        let found = self.read(move |c| media_lookup(c, &claims, now)).await?;
        let Some((principal, epoch)) = found else {
            return Err(AuthError::MediaTokenExpired);
        };
        if let Some((device, _)) = &principal.device {
            self.remember_epoch(*device, epoch);
        }
        self.cache.put(
            hash,
            principal.clone(),
            principal.key.clone(),
            Some(expires),
            now,
        );
        Ok(principal)
    }

    pub(crate) async fn authenticate_session(&self, cookie: &str) -> Result<Principal, AuthError> {
        let hash = sha256(cookie.as_bytes());
        let now = self.now();
        let found = self
            .read(move |c| {
                let Some(session) = sessions::by_hash(c, &hash)? else {
                    return Ok(None);
                };
                if now >= session.idle_expires_at || now >= session.expires_at {
                    return Ok(None);
                }
                Ok(
                    load_principal(c, session.user, Scopes::ALL, Via::Session, None)?
                        .map(|p| (p, session)),
                )
            })
            .await?;
        let Some((mut principal, session)) = found else {
            return Err(AuthError::Unauthenticated);
        };
        principal.session = Some(crate::SessionInfo {
            key: session.key,
            recent_auth_until: session.recent_auth_at.saturating_add(crate::RECENT_AUTH),
        });
        if session.last_seen_at > now || now.since(session.last_seen_at) >= TOUCH_EVERY {
            let idle_until = now
                .saturating_add(self.config.session_idle)
                .min(session.expires_at);
            if let Err(e) = self
                .db
                .write(move |tx| sessions::touch(tx, session.key, idle_until))
                .await
            {
                tracing::debug!(error = %e, "could not record session use");
            }
        }
        Ok(principal)
    }
}

/// The principal for a key secret hash, with the key row and the device's media epoch.
pub(crate) fn key_lookup(
    c: &Connection,
    hash: &[u8; 32],
    now: Millis,
) -> Result<Option<(Principal, ApiKeyRow, Option<i64>)>, DbError> {
    let Some(row) = keys::by_hash(c, hash)? else {
        return Ok(None);
    };
    if row.revoked_at.is_some() || row.expires_at.is_some_and(|x| x <= now) {
        return Ok(None);
    }
    let scopes = Scopes::parse(&row.scopes).unwrap_or(Scopes::NONE);
    let (via, device, epoch) = match row.kind {
        KeyKind::Device => {
            let Some(device) = row
                .device
                .map(|d| devices::get(c, d))
                .transpose()?
                .flatten()
            else {
                return Ok(None);
            };
            if device.revoked_at.is_some() || device.user != row.user {
                return Ok(None);
            }
            (
                Via::DeviceKey,
                Some((device.key, device.id)),
                Some(device.media_epoch),
            )
        }
        KeyKind::ApiKey => (Via::ApiKey, None, None),
        KeyKind::AppPassword => return Ok(None),
    };
    Ok(
        load_principal(c, row.user, scopes, via, device)?.map(|mut p| {
            p.key = Some(row.id.clone());
            (p, row, epoch)
        }),
    )
}

fn media_lookup(
    c: &Connection,
    claims: &Claims,
    now: Millis,
) -> Result<Option<(Principal, i64)>, DbError> {
    let Ok(user_id) = UserId::parse(&claims.u) else {
        return Ok(None);
    };
    match (&claims.d, &claims.k) {
        (Some(device), None) => device_media_lookup(c, &user_id, device, claims.e),
        (None, Some(key)) => key_media_lookup(c, &user_id, key, now),
        _ => Ok(None),
    }
}

fn device_media_lookup(
    c: &Connection,
    user_id: &UserId,
    device: &str,
    epoch: i64,
) -> Result<Option<(Principal, i64)>, DbError> {
    let Ok(device_id) = DeviceId::parse(device) else {
        return Ok(None);
    };
    let Some(device) = devices::by_pid(c, &device_id)? else {
        return Ok(None);
    };
    if device.revoked_at.is_some() || device.media_epoch != epoch {
        return Ok(None);
    }
    let Some(user) = users::get(c, device.user)? else {
        return Ok(None);
    };
    if user.id != *user_id {
        return Ok(None);
    }
    Ok(load_principal(
        c,
        device.user,
        MEDIA_SCOPES,
        Via::MediaToken,
        Some((device.key, device.id)),
    )?
    .map(|p| (p, device.media_epoch)))
}

/// A token minted by an API key or app password lives only while the key is active, and
/// carries no more than the key's own media scopes.
fn key_media_lookup(
    c: &Connection,
    user_id: &UserId,
    key: &str,
    now: Millis,
) -> Result<Option<(Principal, i64)>, DbError> {
    let Ok(key_id) = KeyId::parse(key) else {
        return Ok(None);
    };
    let Some(user) = users::by_pid(c, user_id)? else {
        return Ok(None);
    };
    let Some(row) = keys::list(c, Some(user.key))?.into_iter().find(|row| {
        row.id == key_id
            && row.kind != KeyKind::Device
            && row.revoked_at.is_none()
            && row.expires_at.is_none_or(|x| x > now)
    }) else {
        return Ok(None);
    };
    let key_scopes = Scopes::parse(&row.scopes).unwrap_or(Scopes::NONE);
    if !key_scopes.contains(Scopes::STREAM) {
        return Ok(None);
    }
    let scopes = Scopes::from_bits(key_scopes.bits() & MEDIA_SCOPES.bits());
    Ok(
        load_principal(c, user.key, scopes, Via::MediaToken, None)?.map(|mut p| {
            p.key = Some(row.id);
            (p, 0)
        }),
    )
}

impl Auth {
    /// A media token for `mt=` on media URLs, valid for `auth.media_token_ttl`. A device key
    /// mints one bound to its device and media epoch; an API key or app password (Subsonic
    /// `hls`) mints one bound to the user and that key, which dies with the key. The
    /// credential needs the `stream` scope; sessions and media tokens cannot mint.
    pub fn mint_media_token(&self, principal: &Principal) -> Result<crate::MediaToken, AuthError> {
        if !principal.scopes.contains(Scopes::STREAM) {
            return Err(AuthError::Forbidden);
        }
        let expires_at = self.now().saturating_add(self.config.media_token_ttl);
        let user = principal.user.as_str().to_owned();
        let claims = match (principal.via, &principal.device, &principal.key) {
            (Via::DeviceKey, Some((device, device_id)), _) => {
                let epoch = crate::limit::lock(&self.epochs)
                    .get(device)
                    .copied()
                    .ok_or(AuthError::Unauthenticated)?;
                Claims {
                    u: user,
                    d: Some(device_id.as_str().to_owned()),
                    k: None,
                    e: epoch,
                    x: expires_at.0,
                }
            }
            (Via::ApiKey | Via::AppPassword, None, Some(key)) => Claims {
                u: user,
                d: None,
                k: Some(key.as_str().to_owned()),
                e: 0,
                x: expires_at.0,
            },
            _ => return Err(AuthError::Forbidden),
        };
        Ok(crate::MediaToken {
            token: media_token::mint(&self.media_key, &claims),
            expires_at,
        })
    }
}
