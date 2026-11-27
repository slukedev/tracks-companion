//! Device-code linking (RFC 8628 shape, api.md section 6). A device that cannot scan a QR code
//! or type a long code (a TV) starts a link and shows an 8-character user code; someone signed
//! in, in Tracks or the dashboard, looks it up and approves or denies it; the device polls with
//! its device code and collects a device key once.
//!
//! The user code is 8 Crockford base32 characters (40 bits). Like a pairing offer's code, its
//! first 4 characters find the link (`user_code_locator`) and the whole code is checked against
//! a second hash in constant time, so a wrong guess that reaches a live link counts against it,
//! and the link locks after [`LINK_ATTEMPTS`]. Wrong codes also count against the approver and
//! against the whole server. The device code is 256 bits and is looked up by its hash.
//!
//! The key is made when the device collects it, not when the link is approved, so nothing
//! secret waits in the database and an approved link that is never collected leaves no device
//! behind.

use std::net::IpAddr;
use std::time::Duration;

use trackside_core::repo::device_links::{self, LinkRow, LinkState, NewLink};
use trackside_core::{DbError, Millis, PublicId, ServerEvent, UserKey, WriteTx};

use crate::account::{active_user, log};
use crate::credentials::{issue_in_tx, validate_device};
use crate::limit::IpKey;
use crate::request::log_safe;
use crate::token::{self, ct_eq, normalize_crockford, tagged_sha256};
use crate::{
    Auth, AuthError, DeviceLinkStart, IssuedDeviceKey, LinkDecision, LinkError, LinkRequest,
    NewDevice, Principal, Scopes, Via,
};

/// How long a link waits for a decision and collection.
pub const LINK_LIFETIME: Duration = Duration::from_mins(10);
/// The poll interval a device starts with.
pub const LINK_INTERVAL: Duration = Duration::from_secs(5);
/// What `slow_down` adds to the interval, as RFC 8628 says.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// The longest interval `slow_down` grows to.
pub const MAX_INTERVAL: Duration = Duration::from_mins(1);
/// How much earlier than the interval a poll may arrive, for network jitter.
const POLL_TOLERANCE: Duration = Duration::from_secs(1);
/// Wrong codes that reach one live link before it locks.
pub const LINK_ATTEMPTS: u32 = 5;
/// Links waiting for a decision at once, server-wide.
pub const MAX_PENDING_LINKS: u64 = 100;
/// Links one address may start: a burst, then one a minute.
pub(crate) const STARTS_PER_IP: (u32, u32) = (1, 5);
/// Links the whole server starts: a burst, then this many a minute.
pub(crate) const STARTS_GLOBAL: (u32, u32) = (30, 60);
/// Polls one address may make a minute, with a burst. Several TVs behind one router fit.
pub(crate) const POLLS_PER_IP: (u32, u32) = (120, 60);
/// Wrong codes an approver may enter in 10 minutes.
pub(crate) const GUESSES_PER_USER: u32 = 5;
/// Wrong codes the whole server accepts in an hour, across every approver.
pub(crate) const GUESSES_GLOBAL_PER_HOUR: u32 = 200;
/// Expired links are kept this long, so the approver hears "expired" rather than "wrong code".
pub const KEEP_EXPIRED: Duration = Duration::from_hours(1);
const INSERT_TRIES: usize = 5;
const USER_CODE_LEN: usize = 8;

fn locator(code: &str) -> [u8; 32] {
    tagged_sha256("link locator", code.get(..4).unwrap_or(code).as_bytes())
}

fn code_check(code: &str) -> [u8; 32] {
    tagged_sha256("link code", code.as_bytes())
}

fn device_code_hash(device_code: &str) -> [u8; 32] {
    tagged_sha256("link device code", device_code.as_bytes())
}

/// The typed user code without dashes or spaces, upper case, with O read as 0 and I or L as 1.
pub(crate) fn normalize_user_code(code: &str) -> Option<zeroize::Zeroizing<String>> {
    normalize_crockford(code.trim()).filter(|c| c.len() == USER_CODE_LEN)
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn duration(ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

fn request_of(row: &LinkRow) -> LinkRequest {
    LinkRequest {
        device_name: row.device.name.clone(),
        platform: row.device.platform.clone(),
        model: row.device.model.clone(),
        app_name: row.device.app_name.clone(),
        nearby: row.nearby,
        requested_at: row.created_at,
        expires_at: row.expires_at,
    }
}

/// What a typed code found.
enum Found {
    Live(Box<LinkRow>),
    Wrong,
    Expired,
    Locked,
    Used,
}

/// Finds the link a typed code names, counting a wrong code that reaches a live link.
fn find(tx: &mut WriteTx<'_>, code: &str, now: Millis) -> Result<Found, DbError> {
    let Some(row) = device_links::by_locator(tx.conn(), &locator(code))? else {
        return Ok(Found::Wrong);
    };
    if !ct_eq(&row.user_code_hash, &code_check(code)) {
        if row.state == LinkState::Pending && row.expires_at > now {
            device_links::fail_attempt(tx, row.key, LINK_ATTEMPTS)?;
        }
        return Ok(Found::Wrong);
    }
    Ok(if row.expires_at <= now {
        Found::Expired
    } else {
        match row.state {
            LinkState::Pending => Found::Live(Box::new(row)),
            LinkState::Locked => Found::Locked,
            LinkState::Approved | LinkState::Denied | LinkState::Completed => Found::Used,
        }
    })
}

impl Auth {
    /// `POST /api/v1/link/start`: a device asks to be linked. Limited per address and
    /// server-wide, and refused while [`MAX_PENDING_LINKS`] are waiting.
    pub async fn start_device_link(
        &self,
        device: NewDevice,
        ip: IpAddr,
    ) -> Result<DeviceLinkStart, AuthError> {
        let device = validate_device(device)?;
        self.link_starts
            .check(&IpKey::from(ip), &self.clock)
            .map_err(AuthError::rate_limited)?;
        self.link_starts_global
            .check(&(), &self.clock)
            .map_err(AuthError::rate_limited)?;
        let nearby = self.is_nearby(ip);
        let expires_at = self.now().saturating_add(LINK_LIFETIME);
        for _ in 0..INSERT_TRIES {
            let user_code = token::pairing_code()?;
            let device_code = token::url_token()?;
            let new = NewLink {
                device_code_hash: device_code_hash(&device_code),
                user_code_locator: locator(&user_code),
                user_code_hash: code_check(&user_code),
                device: device.clone(),
                nearby,
                interval_ms: millis(LINK_INTERVAL),
                expires_at,
            };
            let inserted = self
                .write(move |tx| {
                    if device_links::count_pending(tx.conn(), tx.now())? >= MAX_PENDING_LINKS {
                        return Ok(None);
                    }
                    device_links::insert(tx, &new).map(Some)
                })
                .await;
            match inserted {
                Ok(Some(_)) => {
                    tracing::info!(
                        device = %log_safe(&device.name),
                        platform = %device.platform,
                        %ip,
                        "device link started"
                    );
                    return Ok(DeviceLinkStart {
                        device_code: device_code.to_string(),
                        user_code: token::display_pairing_code(&user_code),
                        interval: LINK_INTERVAL,
                        expires_at,
                    });
                }
                Ok(None) => {
                    tracing::warn!("device link refused: too many links are waiting");
                    return Err(AuthError::rate_limited(Duration::from_mins(1)));
                }
                Err(AuthError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(AuthError::Conflict(String::from("no free link code")))
    }

    /// Only a person's own credentials may approve: a dashboard session or a device key.
    /// API keys and app passwords belong to other apps and scripts.
    fn may_decide(by: &Principal) -> Result<(), LinkError> {
        if matches!(by.via, Via::Session | Via::DeviceKey) {
            Ok(())
        } else {
            Err(LinkError::Auth(AuthError::Forbidden))
        }
    }

    fn check_guesses(&self, by: UserKey) -> Result<(), LinkError> {
        self.link_guesses
            .check(&by)
            .and_then(|()| self.link_guesses_global.check(&()))
            .map_err(|wait| LinkError::Auth(AuthError::rate_limited(wait)))
    }

    /// A wrong code counts against the approver and the server. It is answered as wrong
    /// even when it is the one that blocks what follows.
    fn wrong_guess(&self, by: &Principal) -> LinkError {
        tracing::warn!(
            "auth_failure user={} ip=- via=device_link",
            log_safe(&by.username)
        );
        let _ = self.link_guesses.fail(&by.access.user);
        let _ = self.link_guesses_global.fail(&());
        LinkError::NotFound
    }

    /// `POST /api/v1/link/lookup`: what a code would link, so the approver can confirm it.
    pub async fn lookup_device_link(
        &self,
        by: &Principal,
        code: &str,
    ) -> Result<LinkRequest, LinkError> {
        Self::may_decide(by)?;
        self.check_guesses(by.access.user)?;
        let Some(code) = normalize_user_code(code) else {
            return Err(self.wrong_guess(by));
        };
        let now = self.now();
        let found = self
            .write(move |tx| find(tx, &code, now))
            .await
            .map_err(LinkError::Auth)?;
        match found {
            Found::Live(row) => Ok(request_of(&row)),
            Found::Wrong => Err(self.wrong_guess(by)),
            Found::Expired => Err(LinkError::Expired),
            Found::Locked => Err(LinkError::Locked),
            Found::Used => Err(LinkError::Used),
        }
    }

    /// `POST /api/v1/link/approve` and `/deny`. Approving binds the link to the approver
    /// with their credential's scopes as the ceiling for the new key.
    pub async fn decide_device_link(
        &self,
        by: &Principal,
        code: &str,
        decision: LinkDecision,
        ip: IpAddr,
    ) -> Result<LinkRequest, LinkError> {
        Self::may_decide(by)?;
        self.check_guesses(by.access.user)?;
        let Some(code) = normalize_user_code(code) else {
            return Err(self.wrong_guess(by));
        };
        let now = self.now();
        let user = by.access.user;
        let scopes = (decision == LinkDecision::Approve)
            .then(|| by.scopes.intersection(Scopes::ALL).to_string());
        let found = self
            .write(move |tx| {
                let found = find(tx, &code, now)?;
                if let Found::Live(row) = &found {
                    let actor = active_user(tx.conn(), user)?;
                    device_links::decide(tx, row.key, user, scopes.as_deref())?;
                    let action = match decision {
                        LinkDecision::Approve => "device.link.approve",
                        LinkDecision::Deny => "device.link.deny",
                    };
                    log(tx, Some(&actor), Some(ip), action, Some(&row.device.name))?;
                }
                Ok(found)
            })
            .await
            .map_err(|e| match e {
                AuthError::Conflict(_) => LinkError::Used,
                other => LinkError::Auth(other),
            })?;
        match found {
            Found::Live(row) => {
                tracing::info!(
                    user = %log_safe(&by.username),
                    device = %log_safe(&row.device.name),
                    decision = decision.as_str(),
                    "device link decided"
                );
                Ok(request_of(&row))
            }
            Found::Wrong => Err(self.wrong_guess(by)),
            Found::Expired => Err(LinkError::Expired),
            Found::Locked => Err(LinkError::Locked),
            Found::Used => Err(LinkError::Used),
        }
    }

    /// `POST /api/v1/link/poll`: `Pending`, `SlowDown`, `Denied` and `Expired` until the link
    /// is approved; then the key, exactly once. An unknown code is `Expired` too, so a device
    /// whose link was swept starts again.
    pub async fn poll_device_link(
        &self,
        device_code: &str,
        ip: IpAddr,
    ) -> Result<IssuedDeviceKey, LinkError> {
        self.link_polls
            .check(&IpKey::from(ip), &self.clock)
            .map_err(|wait| LinkError::Auth(AuthError::rate_limited(wait)))?;
        let presented = device_code_hash(device_code.trim());
        let lookup = presented;
        let row = self
            .read(move |c| device_links::by_device_code(c, &lookup))
            .await
            .map_err(LinkError::Auth)?
            .filter(|row| ct_eq(&row.device_code_hash, &presented))
            .ok_or(LinkError::Expired)?;
        let now = self.now();
        if row.expires_at <= now {
            return Err(LinkError::Expired);
        }
        match row.state {
            LinkState::Locked | LinkState::Completed => return Err(LinkError::Expired),
            LinkState::Denied => return Err(LinkError::Denied),
            LinkState::Pending | LinkState::Approved => {}
        }
        let interval = duration(row.interval_ms);
        let too_soon = row
            .last_polled_at
            .is_some_and(|last| now.since(last).saturating_add(POLL_TOLERANCE) < interval);
        if too_soon {
            let slower = interval.saturating_add(SLOW_DOWN_STEP).min(MAX_INTERVAL);
            let key = row.key;
            self.write(move |tx| device_links::polled(tx, key, millis(slower)))
                .await
                .map_err(LinkError::Auth)?;
            return Err(LinkError::SlowDown { interval: slower });
        }
        if row.state == LinkState::Pending {
            let key = row.key;
            self.write(move |tx| device_links::polled(tx, key, millis(interval)))
                .await
                .map_err(LinkError::Auth)?;
            return Err(LinkError::Pending);
        }
        self.collect(row, ip).await
    }

    /// Makes the approved link's device and key, and marks the link collected, in one
    /// transaction: a second poll finds it collected.
    async fn collect(&self, row: LinkRow, ip: IpAddr) -> Result<IssuedDeviceKey, LinkError> {
        let Some(user) = row.user else {
            return Err(LinkError::Expired);
        };
        let scopes = row
            .scopes
            .as_deref()
            .and_then(Scopes::parse)
            .unwrap_or(Scopes::NONE)
            .intersection(Scopes::ALL);
        if scopes == Scopes::NONE {
            return Err(LinkError::Expired);
        }
        let secret = token::api_key()?;
        let hidden = secret.clone();
        let key = row.key;
        let device = row.device;
        let issued = self
            .write(move |tx| {
                device_links::complete(tx, key)?;
                let owner = active_user(tx.conn(), user)?;
                let issued = issue_in_tx(tx, user, &device, &hidden, Some(ip), scopes)?;
                log(
                    tx,
                    Some(&owner),
                    Some(ip),
                    "device.link.complete",
                    Some(issued.1.id.as_str()),
                )?;
                Ok(issued)
            })
            .await
            .map_err(|e| match e {
                AuthError::Conflict(_) | AuthError::NotFound => LinkError::Expired,
                other => LinkError::Auth(other),
            })?;
        let (row, device) = issued;
        tracing::info!(
            device = %device.id,
            platform = %device.platform,
            %ip,
            "device link completed"
        );
        self.db.events().publish(ServerEvent::DeviceLinked {
            user,
            device: device.id.clone(),
            name: device.name.clone(),
            platform: device.platform.clone(),
        });
        Ok(self
            .finish_issue(user, secret.to_string(), row, device)
            .await?)
    }
}

impl LinkDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_codes_are_forgiving() -> Result<(), AuthError> {
        let code = token::pairing_code()?;
        let shown = token::display_pairing_code(&code);
        let typed = shown.to_lowercase();
        assert_eq!(
            normalize_user_code(&typed).as_deref().map(String::as_str),
            Some(code.as_str())
        );
        assert_eq!(
            normalize_user_code(&shown.replace('-', " "))
                .as_deref()
                .map(String::as_str),
            Some(code.as_str())
        );
        Ok(())
    }

    #[test]
    fn look_alike_letters_read_as_digits() {
        assert_eq!(
            normalize_user_code("oil0-abcd")
                .as_deref()
                .map(String::as_str),
            Some("0110ABCD")
        );
    }

    #[test]
    fn malformed_codes_are_refused() {
        assert!(normalize_user_code("ABCD-EFG").is_none());
        assert!(normalize_user_code("ABCD-EFGHJ").is_none());
        assert!(normalize_user_code("ABCD-EFG!").is_none());
        assert!(normalize_user_code("ABCD-EFGU").is_none());
    }

    #[test]
    fn codes_sharing_a_first_half_share_only_the_locator() {
        assert_eq!(locator("ABCD1234"), locator("ABCD5678"));
        assert_ne!(code_check("ABCD1234"), code_check("ABCD5678"));
    }

    #[test]
    fn link_hashes_never_match_pairing_hashes() {
        assert_ne!(
            code_check("ABCD1234"),
            tagged_sha256("offer code", b"ABCD1234")
        );
        assert_ne!(device_code_hash("x"), tagged_sha256("link code", b"x"));
    }
}
