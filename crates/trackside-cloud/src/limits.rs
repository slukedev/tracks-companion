//! Sizes, quotas and the push rate limit.

use std::num::NonZeroU32;
use std::time::Duration;

use governor::{Quota, RateLimiter};
use trackside_core::UserKey;

/// Items in one push batch.
pub const MAX_BATCH: usize = 1000;
/// A pull page's default and maximum size.
pub const DEFAULT_PAGE: u32 = 500;
pub const MAX_PAGE: u32 = 2000;
/// An item's fields as JSON.
pub const MAX_FIELDS_BYTES: usize = 16 * 1024;
/// One string inside an item's fields.
pub const MAX_STRING_BYTES: usize = 4096;
/// Keys in one object, elements in one array, and how deep they nest.
pub const MAX_KEYS: usize = 100;
pub const MAX_ARRAY: usize = 1000;
pub const MAX_DEPTH: usize = 5;
/// Ids, device ids and field names.
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_DEVICE_ID_BYTES: usize = 128;
pub const MAX_FIELD_NAME_BYTES: usize = 64;
/// One setting's value as JSON, and settings per user.
pub const MAX_SETTING_BYTES: usize = 256 * 1024;
pub const MAX_SETTINGS: u64 = 1000;
/// Devices and predecessors in the registry.
pub const MAX_DEVICES: usize = 200;
pub const MAX_PREDECESSORS: usize = 32;
/// How far ahead of the server's clock a client time may be.
pub const MAX_CLOCK_AHEAD: Duration = Duration::from_secs(86_400);
/// How long tombstones are kept, except plays', which are kept forever.
pub const TOMBSTONE_DAYS: u64 = 90;
/// One write transaction applies at most this many items, or works this long, before it
/// commits and lets other writers in (ADR 0002).
pub const CHUNK_ITEMS: usize = 250;
pub const CHUNK_TIME: Duration = Duration::from_millis(50);

/// Per-user quotas. The admin sets them with `cloud.quota_items` and `cloud.quota_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_items: u64,
    pub max_bytes: u64,
}

impl Limits {
    pub const DEFAULT_ITEMS: u64 = 2_000_000;
    pub const DEFAULT_BYTES: u64 = 1 << 30;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_items: Self::DEFAULT_ITEMS,
            max_bytes: Self::DEFAULT_BYTES,
        }
    }
}

type Keyed = RateLimiter<
    UserKey,
    governor::state::keyed::DefaultKeyedStateStore<UserKey>,
    governor::clock::DefaultClock,
>;

/// Push requests and pushed items per user, on top of the per-key request limit every
/// authenticated route has: 20 batches a second (bursts of 40) and 10 000 items a second
/// (bursts of 50 000), so an import of 100k plays takes a few seconds of budget and a
/// runaway client cannot keep the writer busy.
pub struct PushLimiter {
    requests: Keyed,
    items: Keyed,
}

impl std::fmt::Debug for PushLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushLimiter").finish_non_exhaustive()
    }
}

const fn non_zero(n: u32) -> NonZeroU32 {
    match NonZeroU32::new(n) {
        Some(n) => n,
        None => NonZeroU32::MIN,
    }
}

impl Default for PushLimiter {
    fn default() -> Self {
        Self::new(20, 40, 10_000, 50_000)
    }
}

impl PushLimiter {
    pub fn new(requests: u32, request_burst: u32, items: u32, item_burst: u32) -> Self {
        Self {
            requests: RateLimiter::keyed(
                Quota::per_second(non_zero(requests)).allow_burst(non_zero(request_burst)),
            ),
            items: RateLimiter::keyed(
                Quota::per_second(non_zero(items)).allow_burst(non_zero(item_burst)),
            ),
        }
    }

    /// Takes one request and `items` items from `user`'s budget, or says how long to wait.
    pub fn check(&self, user: UserKey, items: usize) -> Result<(), Duration> {
        let clock = governor::clock::DefaultClock::default();
        let wait = |not_until: governor::NotUntil<_>| {
            not_until
                .wait_time_from(governor::clock::Clock::now(&clock))
                .max(Duration::from_secs(1))
        };
        if self.requests.len() > 50_000 {
            self.requests.retain_recent();
            self.items.retain_recent();
        }
        self.requests.check_key(&user).map_err(wait)?;
        let Some(n) = NonZeroU32::new(u32::try_from(items).unwrap_or(u32::MAX)) else {
            return Ok(());
        };
        match self.items.check_key_n(&user, n) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(not_until)) => Err(wait(not_until)),
            // More than the burst in one go: never happens with MAX_BATCH below the burst.
            Err(_) => Err(Duration::from_secs(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_past_the_burst_wait() {
        let limiter = PushLimiter::new(1, 2, 1000, 1000);
        assert!(limiter.check(UserKey(1), 10).is_ok());
        assert!(limiter.check(UserKey(1), 10).is_ok());
        assert!(limiter.check(UserKey(1), 10).is_err());
        assert!(limiter.check(UserKey(2), 10).is_ok(), "per user");
        let items = PushLimiter::new(100, 100, 10, 50);
        assert!(items.check(UserKey(1), 50).is_ok());
        assert!(items.check(UserKey(1), 1).is_err());
    }
}
