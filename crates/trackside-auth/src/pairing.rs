//! Pairing (architecture section 10). Offers are made by a signed-in user and claimed by a
//! device with the typed code or the QR secret. Requests are started by a device, approved
//! by a signed-in user who types the device's 6 digits, and collected by the device's poll.
//!
//! A typed offer code is 8 characters: the first 4 find the offer (its `code_hash`) and the
//! whole code is checked against a second hash, so a wrong guess at a live offer counts
//! against that offer, which dies after 5. The QR secret has 128 bits and needs no count.

use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use trackside_core::repo::devices;
use trackside_core::repo::pairings::{self, NewPairing, PairingKind, PairingRow};
use trackside_core::{DbError, DeviceId, KeyId, Millis, PairingId, PublicId, UserKey};
use zeroize::Zeroizing;

use crate::account::{active_user, log};
use crate::credentials::{issue_in_tx, validate_device};
use crate::token::{self, ct_eq, normalize_crockford, sha256, tagged_sha256};
use crate::{
    Auth, AuthError, IssuedDeviceKey, NewDevice, PairPoll, PairRequest, PairRequestApproved,
    PairingOffer, Scopes, Via, load_principal,
};

const OFFER_LIFETIME: Duration = Duration::from_secs(600);
const REQUEST_LIFETIME: Duration = Duration::from_secs(600);
/// Wrong typed codes an offer survives.
pub const OFFER_ATTEMPTS: u32 = 5;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PAIR_REQUEST_PURPOSE: &str = "pairing request";
const INSERT_TRIES: usize = 5;
const QR_SECRET_LEN: usize = 22;

fn locator_hash(code: &str) -> [u8; 32] {
    tagged_sha256("offer locator", code.get(..4).unwrap_or(code).as_bytes())
}

fn check_hash(code: &str) -> [u8; 32] {
    tagged_sha256("offer code", code.as_bytes())
}

fn qr_hash(secret: &str) -> [u8; 32] {
    tagged_sha256("offer qr", secret.as_bytes())
}

fn request_code_hash(digits: &str) -> [u8; 32] {
    tagged_sha256("request code", digits.as_bytes())
}

enum Presented {
    Typed { locator: [u8; 32], check: [u8; 32] },
    Qr([u8; 32]),
}

impl Presented {
    fn parse(code: &str) -> Option<Self> {
        let code = code.trim();
        if code.len() == QR_SECRET_LEN
            && code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && normalize_crockford(code).is_none_or(|n| n.len() != 8)
        {
            return Some(Self::Qr(qr_hash(code)));
        }
        let typed = normalize_crockford(code).filter(|n| n.len() == 8)?;
        Some(Self::Typed {
            locator: locator_hash(&typed),
            check: check_hash(&typed),
        })
    }

    fn lookup(&self) -> &[u8; 32] {
        match self {
            Self::Typed { locator, .. } => locator,
            Self::Qr(hash) => hash,
        }
    }
}

enum Claim {
    Unknown,
    Wrong,
    Done(
        Box<(
            UserKey,
            trackside_core::repo::keys::ApiKeyRow,
            devices::Device,
        )>,
    ),
}

/// What an approved request hands out, sealed until the device collects it.
#[derive(Serialize, Deserialize)]
struct Handout {
    secret: String,
    key: KeyId,
    device: DeviceId,
}

impl Drop for Handout {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.secret);
    }
}

impl Auth {
    /// `POST /api/v1/me/pairing-codes`: an offer valid for 10 minutes.
    pub async fn create_pairing_offer(
        &self,
        user: UserKey,
        device_name: Option<&str>,
    ) -> Result<PairingOffer, AuthError> {
        let device_name = device_name
            .map(|n| n.trim().chars().take(100).collect::<String>())
            .filter(|n| !n.is_empty());
        let expires_at = self.now().saturating_add(OFFER_LIFETIME);
        for _ in 0..INSERT_TRIES {
            let code = token::pairing_code()?;
            let qr_secret = token::qr_secret()?;
            let new = NewPairing {
                kind: PairingKind::Offer,
                user: Some(user),
                code_hash: locator_hash(&code),
                qr_secret_hash: Some(qr_hash(&qr_secret)),
                poll_secret_hash: Some(check_hash(&code)),
                device_name: device_name.clone(),
                platform: None,
                device: None,
                expires_at,
            };
            let inserted = self
                .write(move |tx| {
                    active_user(tx.conn(), user)?;
                    pairings::insert(tx, &new)
                })
                .await;
            match inserted {
                Ok(row) => {
                    return Ok(PairingOffer {
                        id: row.id,
                        code: token::display_pairing_code(&code),
                        qr_secret: qr_secret.to_string(),
                        expires_at,
                    });
                }
                Err(AuthError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(AuthError::Conflict(String::from("no free pairing code")))
    }

    /// `DELETE /api/v1/me/pairing-codes/{id}`.
    pub async fn cancel_pairing_offer(
        &self,
        user: UserKey,
        id: &PairingId,
    ) -> Result<(), AuthError> {
        let id = id.clone();
        self.write(move |tx| {
            let row = pairings::get(tx.conn(), &id)?
                .filter(|r| r.kind == PairingKind::Offer && r.user == Some(user))
                .ok_or(DbError::NotFound)?;
            pairings::delete(tx, &row.id)
        })
        .await
    }

    /// Claims an offer by typed code or QR secret. An offer dies after 5 wrong attempts.
    pub async fn claim_pairing(
        &self,
        code: &str,
        device: NewDevice,
        ip: IpAddr,
    ) -> Result<IssuedDeviceKey, AuthError> {
        self.check_ip(ip)?;
        let device = validate_device(device)?;
        let Some(presented) = Presented::parse(code) else {
            self.fail_ip("-", ip, "pairing")?;
            return Err(AuthError::Unauthenticated);
        };
        let secret = token::api_key()?;
        let hidden = secret.clone();
        let now = self.now();
        let outcome = self
            .write(move |tx| {
                let Some(row) = pairings::by_code_hash(tx.conn(), presented.lookup())?
                    .filter(|r| offer_live(r, now))
                else {
                    return Ok(Claim::Unknown);
                };
                if let Presented::Typed { check, .. } = &presented
                    && !row.poll_secret_hash.is_some_and(|h| ct_eq(&h, check))
                {
                    if pairings::fail_attempt(tx, &row.id)? >= OFFER_ATTEMPTS {
                        pairings::delete(tx, &row.id)?;
                    }
                    return Ok(Claim::Wrong);
                }
                let Some(user) = row.user else {
                    return Ok(Claim::Unknown);
                };
                let owner = active_user(tx.conn(), user)?;
                pairings::claim(tx, &row.id)?;
                let device = NewDevice {
                    name: row.device_name.clone().unwrap_or(device.name),
                    ..device
                };
                let (key, device) = issue_in_tx(tx, user, &device, &hidden, Some(ip))?;
                log(
                    tx,
                    Some(&owner),
                    Some(ip),
                    "device.pair",
                    Some(device.id.as_str()),
                )?;
                Ok(Claim::Done(Box::new((user, key, device))))
            })
            .await?;
        match outcome {
            Claim::Done(done) => {
                let (user, key, device) = *done;
                self.finish_issue(user, secret.to_string(), key, device)
                    .await
            }
            Claim::Unknown | Claim::Wrong => {
                self.fail_ip("-", ip, "pairing")?;
                Err(AuthError::Unauthenticated)
            }
        }
    }

    /// `POST /api/v1/pair/requests`: a device asks to be approved. Each request counts
    /// against the address's limit.
    pub async fn start_pair_request(
        &self,
        device: NewDevice,
        ip: IpAddr,
    ) -> Result<PairRequest, AuthError> {
        self.check_ip(ip)?;
        let device = validate_device(device)?;
        self.ip_failures
            .fail(&crate::limit::IpKey::from(ip))
            .map_err(AuthError::rate_limited)?;
        let expires_at = self.now().saturating_add(REQUEST_LIFETIME);
        for _ in 0..INSERT_TRIES {
            let code = token::pair_request_code()?;
            let poll_secret = token::url_token()?;
            let new = NewPairing {
                kind: PairingKind::Request,
                user: None,
                code_hash: request_code_hash(&code),
                qr_secret_hash: None,
                poll_secret_hash: Some(sha256(poll_secret.as_bytes())),
                device_name: Some(device.name.clone()),
                platform: Some(device.platform.clone()),
                device: Some(device.clone()),
                expires_at,
            };
            match self.write(move |tx| pairings::insert(tx, &new)).await {
                Ok(row) => {
                    return Ok(PairRequest {
                        id: row.id,
                        code: token::display_digits(&code),
                        poll_secret: poll_secret.to_string(),
                        expires_at,
                        poll_interval: POLL_INTERVAL,
                    });
                }
                Err(AuthError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(AuthError::Conflict(String::from("no free pairing code")))
    }

    /// `POST /api/v1/pair/requests/approve`: binds the request with these digits to `by` and
    /// issues its key, sealed until the device polls. Five wrong codes per user per 10
    /// minutes, then `RateLimited`.
    pub async fn approve_pair_request(
        &self,
        by: UserKey,
        code: &str,
    ) -> Result<PairRequestApproved, AuthError> {
        self.approvals.check(&by).map_err(AuthError::rate_limited)?;
        let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let well_formed = digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit());
        let secret = token::api_key()?;
        let server_key = std::sync::Arc::clone(&self.secret);
        let now = self.now();
        let outcome = if well_formed {
            let hash = request_code_hash(&digits);
            self.write(move |tx| {
                let Some(row) = pairings::by_code_hash(tx.conn(), &hash)?.filter(|r| {
                    r.kind == PairingKind::Request
                        && r.approved_at.is_none()
                        && r.claimed_at.is_none()
                        && r.expires_at > now
                }) else {
                    return Ok(None);
                };
                let approver = active_user(tx.conn(), by)?;
                // The device as it described itself, so signing in again from the same install
                // replaces its key. Rows written before the device was stored fall back to an
                // install id of their own.
                let device = row.device.clone().unwrap_or_else(|| NewDevice {
                    client_device_id: format!("pair-request-{}", row.id),
                    name: row
                        .device_name
                        .clone()
                        .unwrap_or_else(|| String::from("New device")),
                    platform: row
                        .platform
                        .clone()
                        .unwrap_or_else(|| String::from("other")),
                    model: None,
                    app_name: None,
                    app_version: None,
                });
                let (key, device) = issue_in_tx(tx, by, &device, &secret, None)?;
                let handout = Handout {
                    secret: secret.to_string(),
                    key: key.id.clone(),
                    device: device.id.clone(),
                };
                let plain = Zeroizing::new(
                    serde_json::to_vec(&handout)
                        .map_err(|e| DbError::Invalid(format!("pairing handout: {e}")))?,
                );
                let sealed = crate::seal(&server_key, PAIR_REQUEST_PURPOSE, &plain)
                    .map_err(|e| DbError::Invalid(e.to_string()))?;
                pairings::approve(tx, &row.id, by, &sealed, &key.id)?;
                log(
                    tx,
                    Some(&approver),
                    None,
                    "device.pair",
                    Some(device.id.as_str()),
                )?;
                Ok(Some((
                    device.key,
                    device.media_epoch,
                    PairRequestApproved {
                        device_name: row.device_name,
                        platform: row.platform,
                    },
                )))
            })
            .await?
        } else {
            None
        };
        match outcome {
            Some((device, epoch, approved)) => {
                self.revoked(by, Some(device));
                self.remember_epoch(device, epoch);
                Ok(approved)
            }
            None => {
                // A wrong code is answered as wrong even when it is the one that blocks.
                let _ = self.approvals.fail(&by);
                Err(AuthError::NotFound)
            }
        }
    }

    /// `GET /api/v1/pair/requests/{id}` with `Authorization: Pair <poll_secret>`.
    pub async fn poll_pair_request(
        &self,
        id: &PairingId,
        poll_secret: &str,
    ) -> Result<PairPoll, AuthError> {
        let presented = sha256(poll_secret.as_bytes());
        let now = self.now();
        let lookup = id.clone();
        let row = self
            .read(move |c| pairings::get(c, &lookup))
            .await?
            .filter(|r| {
                r.kind == PairingKind::Request
                    && r.claimed_at.is_none()
                    && r.poll_secret_hash.is_some_and(|h| ct_eq(&h, &presented))
            })
            .ok_or(AuthError::NotFound)?;
        if now >= row.expires_at {
            return Ok(PairPoll::Expired);
        }
        let (Some(_), Some(user), Some(sealed)) =
            (row.approved_at, row.user, row.issued_key_sealed.as_ref())
        else {
            return Ok(PairPoll::Pending);
        };
        let Some(mut handout) = crate::open(&self.secret, PAIR_REQUEST_PURPOSE, sealed)
            .ok()
            .and_then(|plain| serde_json::from_slice::<Handout>(&plain).ok())
        else {
            tracing::warn!(pairing = %row.id, "an approved pairing request cannot be opened; the secret key changed");
            return Ok(PairPoll::Expired);
        };
        let device_id = handout.device.clone();
        let key = handout.key.clone();
        let id = id.clone();
        let (principal, device) = self
            .write(move |tx| {
                pairings::claim(tx, &id).map_err(|e| match e {
                    DbError::Conflict(_) => DbError::NotFound,
                    other => other,
                })?;
                let device = devices::by_pid(tx.conn(), &device_id)?
                    .filter(|d| d.revoked_at.is_none() && d.user == user)
                    .ok_or(DbError::NotFound)?;
                let mut principal = load_principal(
                    tx.conn(),
                    user,
                    Scopes::ALL,
                    Via::DeviceKey,
                    Some((device.key, device.id.clone())),
                )?
                .ok_or(DbError::NotFound)?;
                principal.key = Some(key);
                Ok((principal, device))
            })
            .await?;
        self.remember_epoch(device.key, device.media_epoch);
        Ok(PairPoll::Approved(IssuedDeviceKey {
            secret: std::mem::take(&mut handout.secret),
            key: handout.key.clone(),
            device: device.id,
            device_name: device.name,
            principal,
        }))
    }
}

fn offer_live(row: &PairingRow, now: Millis) -> bool {
    row.kind == PairingKind::Offer
        && row.claimed_at.is_none()
        && row.expires_at > now
        && row.failed_attempts < OFFER_ATTEMPTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_codes_and_qr_secrets_are_told_apart() -> Result<(), AuthError> {
        let code = token::pairing_code()?;
        assert!(matches!(
            Presented::parse(&token::display_pairing_code(&code)),
            Some(Presented::Typed { .. })
        ));
        let qr = token::qr_secret()?;
        assert!(matches!(Presented::parse(&qr), Some(Presented::Qr(_))));
        assert!(Presented::parse("short").is_none());
        Ok(())
    }

    #[test]
    fn offers_with_the_same_first_half_share_a_locator_only() {
        assert_eq!(locator_hash("ABCD1234"), locator_hash("ABCD5678"));
        assert_ne!(check_hash("ABCD1234"), check_hash("ABCD5678"));
    }
}
