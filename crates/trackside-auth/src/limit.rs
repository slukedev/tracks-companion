//! Rate limits (architecture section 7): a per-IP bucket for auth failures, exponential
//! backoff per account and IP, a per-account hourly ceiling, per-key request rates and
//! per-user concurrency slots. All of it reads time from the server [`Clock`], so tests drive
//! it with a `TestClock`.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use trackside_core::{Clock, Millis, UserKey};

/// Consecutive failures for one account from one address before backoff starts.
pub const FREE_FAILURES: u32 = 5;
/// The first backoff delay; it doubles with each further failure.
pub const FIRST_BACKOFF: Duration = Duration::from_secs(2);
/// The longest backoff delay.
pub const MAX_BACKOFF: Duration = Duration::from_mins(15);
/// Failures for one account from anywhere in an hour before attempts from addresses it has
/// not signed in from are refused.
pub const ACCOUNT_HOURLY_CEILING: usize = 100;
/// How long a success from an address keeps that address past the hourly ceiling, so guesses
/// from elsewhere cannot lock the owner out from where they usually sign in.
pub const KNOWN_ADDRESS_FOR: Duration = Duration::from_hours(30 * 24);
/// Addresses remembered per account: a phone, a laptop and a few networks they roam on.
pub(crate) const KNOWN_PER_ACCOUNT: usize = 16;
const HOUR: Duration = Duration::from_secs(3600);
/// Failed attempts an IP may make at once before its rate applies.
pub(crate) const IP_BURST: u32 = 5;
/// Tracked entries per map before old ones are dropped.
const MAX_TRACKED: usize = 50_000;
/// How often entries that no longer limit anything are dropped, however few there are.
const PRUNE_EVERY: Duration = Duration::from_secs(60);

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adapts the server clock for governor, whose instants are durations since the epoch here.
#[derive(Clone, Debug)]
pub(crate) struct ServerClock(pub(crate) Arc<dyn Clock>);

impl governor::clock::Clock for ServerClock {
    type Instant = Duration;

    fn now(&self) -> Duration {
        Duration::from_millis(u64::try_from(self.0.now().0).unwrap_or(0))
    }
}

type Keyed<K> = RateLimiter<K, DefaultKeyedStateStore<K>, ServerClock, NoOpMiddleware<Duration>>;

/// The address an IP limit applies to: IPv4 addresses one by one, IPv6 by /64, since one
/// host usually holds a whole /64.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IpKey(IpAddr);

impl From<IpAddr> for IpKey {
    fn from(ip: IpAddr) -> Self {
        match crate::proxy::canonical(ip) {
            IpAddr::V6(v6) => {
                let bits = u128::from(v6) & !((1u128 << 64) - 1);
                Self(IpAddr::V6(bits.into()))
            }
            v4 @ IpAddr::V4(_) => Self(v4),
        }
    }
}

/// Counts failures against a token bucket; once it is empty, further attempts are refused
/// until the bucket would allow one again. Successes cost nothing.
pub(crate) struct FailureLimiter<K: Hash + Eq + Clone> {
    bucket: Keyed<K>,
    blocked: Mutex<HashMap<K, Millis>>,
    pruned: Mutex<Millis>,
    clock: Arc<dyn Clock>,
}

impl<K: Hash + Eq + Clone> FailureLimiter<K> {
    pub(crate) fn new(quota: Quota, clock: Arc<dyn Clock>) -> Self {
        Self {
            bucket: RateLimiter::new(
                quota,
                DefaultKeyedStateStore::default(),
                ServerClock(Arc::clone(&clock)),
            ),
            blocked: Mutex::new(HashMap::new()),
            pruned: Mutex::new(clock.now()),
            clock,
        }
    }

    /// Drops expired blocks and buckets that are full again, once a minute or when a map
    /// grows past [`MAX_TRACKED`], so addresses that failed once are not kept forever.
    fn prune(&self, blocked: &mut HashMap<K, Millis>) {
        let now = self.clock.now();
        let mut pruned = lock(&self.pruned);
        let due = now.since(*pruned) >= PRUNE_EVERY;
        if due || blocked.len() >= MAX_TRACKED || self.bucket.len() >= MAX_TRACKED {
            blocked.retain(|_, until| *until > now);
            self.bucket.retain_recent();
            *pruned = now;
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> (usize, usize) {
        (self.bucket.len(), lock(&self.blocked).len())
    }

    /// `count` failures per `per`, at most `burst` at once.
    pub(crate) fn per(count: u32, per: Duration, burst: u32, clock: Arc<dyn Clock>) -> Self {
        let period = per / count.max(1);
        let burst = NonZeroU32::new(burst.max(1)).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::with_period(period.max(Duration::from_millis(1)))
            .unwrap_or_else(|| Quota::per_second(NonZeroU32::MIN))
            .allow_burst(burst);
        Self::new(quota, clock)
    }

    /// Err with the wait when `key` is blocked.
    pub(crate) fn check(&self, key: &K) -> Result<(), Duration> {
        let now = self.clock.now();
        let mut blocked = lock(&self.blocked);
        match blocked.get(key) {
            Some(&until) if until > now => Err(until.since(now)),
            Some(_) => {
                blocked.remove(key);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Records a failure. Err with the wait when this failure was one too many.
    pub(crate) fn fail(&self, key: &K) -> Result<(), Duration> {
        self.prune(&mut lock(&self.blocked));
        match self.bucket.check_key(key) {
            Ok(()) => Ok(()),
            Err(not_until) => {
                let wait = not_until
                    .wait_time_from(governor::clock::Clock::now(&ServerClock(Arc::clone(
                        &self.clock,
                    ))))
                    .max(Duration::from_secs(1));
                let mut blocked = lock(&self.blocked);
                blocked.insert(key.clone(), self.clock.now().saturating_add(wait));
                Err(wait)
            }
        }
    }
}

/// Requests per key: 50 a second, bursts of 200.
pub(crate) struct RequestLimiter<K: Hash + Eq + Clone> {
    bucket: Keyed<K>,
}

impl<K: Hash + Eq + Clone> RequestLimiter<K> {
    pub(crate) fn new(per_second: u32, burst: u32, clock: Arc<dyn Clock>) -> Self {
        let rate = NonZeroU32::new(per_second).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(burst).unwrap_or(rate);
        Self {
            bucket: RateLimiter::new(
                Quota::per_second(rate).allow_burst(burst),
                DefaultKeyedStateStore::default(),
                ServerClock(clock),
            ),
        }
    }

    pub(crate) fn check(&self, key: &K, clock: &Arc<dyn Clock>) -> Result<(), Duration> {
        if self.bucket.len() >= MAX_TRACKED {
            self.bucket.retain_recent();
        }
        self.bucket.check_key(key).map_err(|not_until| {
            not_until
                .wait_time_from(governor::clock::Clock::now(&ServerClock(Arc::clone(clock))))
                .max(Duration::from_millis(1))
        })
    }
}

/// Who a login attempt was for: a real account, or a name nobody has (so that guessing at
/// unknown names is limited the same way).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Account {
    User(UserKey),
    Name(String),
    /// Subsonic app password checks, counted apart from the login password: app passwords
    /// are random, and a legacy client retrying with a stale one must not hold up sign-in.
    AppPasswords(Box<Account>),
}

impl Account {
    pub(crate) fn new(user: Option<UserKey>, typed: &str) -> Self {
        match user {
            Some(user) => Self::User(user),
            None => Self::Name(typed.trim().to_lowercase()),
        }
    }

    pub(crate) fn app_passwords(self) -> Self {
        Self::AppPasswords(Box::new(self))
    }
}

#[derive(Clone, Copy, Debug)]
struct Streak {
    failures: u32,
    last: Millis,
}

/// Exponential backoff per account and address after [`FREE_FAILURES`], plus the hourly
/// per-account ceiling. Keyed by the pair so that someone elsewhere cannot lock a user out.
pub(crate) struct Backoff {
    streaks: Mutex<HashMap<(Account, IpKey), Streak>>,
    hourly: Mutex<HashMap<Account, VecDeque<Millis>>>,
    /// The last success per account and address, which exempts that pair from the ceiling.
    /// At most [`KNOWN_PER_ACCOUNT`] per account, the oldest going first.
    known: Mutex<HashMap<Account, Vec<(IpKey, Millis)>>>,
    clock: Arc<dyn Clock>,
}

impl Backoff {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            streaks: Mutex::new(HashMap::new()),
            hourly: Mutex::new(HashMap::new()),
            known: Mutex::new(HashMap::new()),
            clock,
        }
    }

    fn known(&self, pair: &(Account, IpKey), now: Millis) -> bool {
        lock(&self.known).get(&pair.0).is_some_and(|addresses| {
            addresses
                .iter()
                .any(|(ip, at)| *ip == pair.1 && *at <= now && now.since(*at) < KNOWN_ADDRESS_FOR)
        })
    }

    /// The delay owed after `failures` consecutive failures.
    pub(crate) fn delay_after(failures: u32) -> Duration {
        if failures < FREE_FAILURES {
            return Duration::ZERO;
        }
        let doublings = (failures - FREE_FAILURES).min(16);
        FIRST_BACKOFF
            .saturating_mul(1 << doublings)
            .min(MAX_BACKOFF)
    }

    /// Err with the wait while this account and address owe a delay, or the account is over
    /// its hourly ceiling.
    pub(crate) fn check(&self, account: &Account, ip: IpAddr) -> Result<(), Duration> {
        let now = self.clock.now();
        let pair = (account.clone(), IpKey::from(ip));
        if let Some(streak) = lock(&self.streaks).get(&pair) {
            let until = streak
                .last
                .saturating_add(Self::delay_after(streak.failures));
            if until > now {
                return Err(until.since(now));
            }
        }
        if self.known(&pair, now) {
            return Ok(());
        }
        let mut hourly = lock(&self.hourly);
        if let Some(times) = hourly.get_mut(account) {
            let hour_ago = now.saturating_sub(HOUR);
            while times.front().is_some_and(|t| *t <= hour_ago) {
                times.pop_front();
            }
            if times.len() >= ACCOUNT_HOURLY_CEILING
                && let Some(oldest) = times.front()
            {
                return Err(oldest.saturating_add(HOUR).since(now));
            }
        }
        Ok(())
    }

    pub(crate) fn fail(&self, account: &Account, ip: IpAddr) {
        let now = self.clock.now();
        {
            let mut streaks = lock(&self.streaks);
            if streaks.len() >= MAX_TRACKED {
                let stale = now.saturating_sub(MAX_BACKOFF.saturating_mul(2));
                streaks.retain(|_, s| s.last > stale);
            }
            let streak = streaks
                .entry((account.clone(), IpKey::from(ip)))
                .or_insert(Streak {
                    failures: 0,
                    last: now,
                });
            streak.failures = streak.failures.saturating_add(1);
            streak.last = now;
        }
        let mut hourly = lock(&self.hourly);
        if hourly.len() >= MAX_TRACKED {
            let hour_ago = now.saturating_sub(HOUR);
            hourly.retain(|_, times| times.back().is_some_and(|t| *t > hour_ago));
        }
        let times = hourly.entry(account.clone()).or_default();
        times.push_back(now);
        while times.len() > ACCOUNT_HOURLY_CEILING {
            times.pop_front();
        }
    }

    pub(crate) fn succeed(&self, account: &Account, ip: IpAddr) {
        let pair = (account.clone(), IpKey::from(ip));
        lock(&self.streaks).remove(&pair);
        let now = self.clock.now();
        let fresh = |at: &Millis| *at <= now && now.since(*at) < KNOWN_ADDRESS_FOR;
        let mut known = lock(&self.known);
        if known.len() >= MAX_TRACKED && !known.contains_key(account) {
            known.retain(|_, addresses| {
                addresses.retain(|(_, at)| fresh(at));
                !addresses.is_empty()
            });
            // Still full of live entries: the account that signed in longest ago goes, so a
            // flood of new accounts cannot wipe out everyone's exemption at once.
            if known.len() >= MAX_TRACKED
                && let Some(stalest) = known
                    .iter()
                    .min_by_key(|(_, addresses)| addresses.iter().map(|(_, at)| *at).max())
                    .map(|(account, _)| account.clone())
            {
                known.remove(&stalest);
            }
        }
        let addresses = known.entry(account.clone()).or_default();
        addresses.retain(|(ip, at)| *ip != pair.1 && fresh(at));
        if addresses.len() >= KNOWN_PER_ACCOUNT
            && let Some(oldest) = addresses
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(i, _)| i)
        {
            addresses.swap_remove(oldest);
        }
        addresses.push((pair.1, now));
    }
}

/// Concurrency slots per user, for streams and transcodes.
#[derive(Debug)]
pub(crate) struct Slots {
    per_user: usize,
    users: Mutex<HashMap<UserKey, Arc<Semaphore>>>,
}

impl Slots {
    pub(crate) fn new(per_user: usize) -> Self {
        Self {
            per_user,
            users: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn try_take(&self, user: UserKey) -> Option<OwnedSemaphorePermit> {
        let semaphore = {
            let mut users = lock(&self.users);
            if users.len() >= MAX_TRACKED {
                users.retain(|_, s| s.available_permits() < self.per_user);
            }
            Arc::clone(
                users
                    .entry(user)
                    .or_insert_with(|| Arc::new(Semaphore::new(self.per_user))),
            )
        };
        semaphore.try_acquire_owned().ok()
    }
}

#[cfg(test)]
mod tests {
    use trackside_core::TestClock;

    use super::*;

    fn clock() -> Arc<TestClock> {
        Arc::new(TestClock::new(Millis(1_794_648_600_000)))
    }

    const A: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1));
    const B: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7));

    #[test]
    fn addresses_that_failed_once_are_forgotten_once_their_bucket_refills() {
        let clock = clock();
        let limiter = FailureLimiter::per(5, Duration::from_secs(60), 5, clock.clone());
        for n in 0..200u32 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0xc000_0200 + n));
            assert!(limiter.fail(&IpKey::from(ip)).is_ok());
        }
        assert_eq!(limiter.tracked().0, 200);
        clock.advance(Duration::from_secs(120));
        assert!(limiter.fail(&IpKey::from(A)).is_ok());
        assert_eq!(limiter.tracked(), (1, 0));
    }

    #[test]
    fn fifth_failure_starts_backoff_for_that_account_and_address_only() {
        let clock = clock();
        let backoff = Backoff::new(clock.clone());
        let luke = Account::User(UserKey(1));
        let mia = Account::User(UserKey(2));
        for _ in 0..4 {
            backoff.fail(&luke, A);
            assert!(backoff.check(&luke, A).is_ok());
        }
        backoff.fail(&luke, A);
        assert_eq!(backoff.check(&luke, A), Err(FIRST_BACKOFF));
        assert!(backoff.check(&luke, B).is_ok());
        assert!(backoff.check(&mia, A).is_ok());
        clock.advance(FIRST_BACKOFF);
        assert!(backoff.check(&luke, A).is_ok());
    }

    #[test]
    fn backoff_doubles_up_to_fifteen_minutes() {
        assert_eq!(Backoff::delay_after(4), Duration::ZERO);
        assert_eq!(Backoff::delay_after(5), Duration::from_secs(2));
        assert_eq!(Backoff::delay_after(6), Duration::from_secs(4));
        assert_eq!(Backoff::delay_after(9), Duration::from_secs(32));
        assert_eq!(Backoff::delay_after(14), MAX_BACKOFF);
        assert_eq!(Backoff::delay_after(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn success_clears_the_streak() {
        let backoff = Backoff::new(clock());
        let luke = Account::User(UserKey(1));
        for _ in 0..5 {
            backoff.fail(&luke, A);
        }
        backoff.succeed(&luke, A);
        assert!(backoff.check(&luke, A).is_ok());
    }

    #[test]
    fn distributed_guessing_hits_the_hourly_ceiling() {
        let clock = clock();
        let backoff = Backoff::new(clock.clone());
        let luke = Account::User(UserKey(1));
        for i in 0..ACCOUNT_HOURLY_CEILING {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(
                0x0a00_0000 + u32::try_from(i).unwrap_or(0),
            ));
            backoff.fail(&luke, ip);
            clock.advance(Duration::from_secs(1));
        }
        assert!(backoff.check(&luke, B).is_err());
        clock.advance(HOUR);
        assert!(backoff.check(&luke, B).is_ok());
    }

    #[test]
    fn the_ceiling_does_not_lock_the_owner_out_where_they_signed_in_before() {
        let clock = clock();
        let backoff = Backoff::new(clock.clone());
        let luke = Account::User(UserKey(1));
        backoff.succeed(&luke, A);
        for i in 0..ACCOUNT_HOURLY_CEILING {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(
                0x0a00_0000 + u32::try_from(i).unwrap_or(0),
            ));
            backoff.fail(&luke, ip);
        }
        assert!(backoff.check(&luke, B).is_err());
        assert!(backoff.check(&luke, A).is_ok());
        // The exemption is for the ceiling only: guessing from a known address still backs off.
        for _ in 0..FREE_FAILURES {
            backoff.fail(&luke, A);
        }
        assert_eq!(backoff.check(&luke, A), Err(FIRST_BACKOFF));
        backoff.succeed(&luke, A);
        clock.advance(KNOWN_ADDRESS_FOR);
        assert!(backoff.check(&luke, A).is_ok());
        for i in 0..ACCOUNT_HOURLY_CEILING {
            backoff.fail(
                &luke,
                IpAddr::V4(std::net::Ipv4Addr::from(0x0b00_0000 + i as u32)),
            );
        }
        clock.advance(Duration::from_secs(1));
        assert!(backoff.check(&luke, A).is_err());
    }

    #[test]
    fn known_addresses_keep_the_newest_per_account() {
        let clock = clock();
        let backoff = Backoff::new(clock.clone());
        let luke = Account::User(UserKey(1));
        backoff.succeed(&luke, A);
        for i in 0..KNOWN_PER_ACCOUNT {
            clock.advance(Duration::from_secs(1));
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + i as u32));
            backoff.succeed(&luke, ip);
        }
        let now = clock.now();
        let pair = |ip| (luke.clone(), IpKey::from(ip));
        assert!(!backoff.known(&pair(A), now), "the oldest went first");
        let newest = IpAddr::V4(std::net::Ipv4Addr::from(
            0x0a00_0000 + KNOWN_PER_ACCOUNT as u32 - 1,
        ));
        assert!(backoff.known(&pair(newest), now));
        // Signing in again from an address refreshes it instead of adding another.
        backoff.succeed(&luke, newest);
        assert_eq!(
            lock(&backoff.known).get(&luke).map(Vec::len),
            Some(KNOWN_PER_ACCOUNT)
        );
    }

    #[test]
    fn app_password_failures_do_not_count_against_sign_in() {
        let backoff = Backoff::new(clock());
        let luke = Account::User(UserKey(1));
        let legacy = luke.clone().app_passwords();
        for i in 0..ACCOUNT_HOURLY_CEILING {
            backoff.fail(
                &legacy,
                IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + i as u32)),
            );
        }
        assert!(backoff.check(&legacy, B).is_err());
        assert!(backoff.check(&luke, B).is_ok());
    }

    #[test]
    fn ip_bucket_allows_a_burst_then_refuses_until_it_refills() {
        let clock = clock();
        let limiter =
            FailureLimiter::<IpKey>::per(10, Duration::from_secs(60), IP_BURST, clock.clone());
        let a = IpKey::from(A);
        for _ in 0..IP_BURST {
            assert!(limiter.fail(&a).is_ok());
        }
        let wait = limiter.fail(&a).err();
        assert_eq!(wait, Some(Duration::from_secs(6)));
        assert!(limiter.check(&a).is_err());
        assert!(limiter.check(&IpKey::from(B)).is_ok());
        clock.advance(Duration::from_secs(6));
        assert!(limiter.check(&a).is_ok());
        assert!(limiter.fail(&a).is_ok());
    }

    #[test]
    fn ipv6_addresses_in_one_slash_64_share_a_bucket() {
        let one: IpAddr = "2001:db8:1:2::1".parse().unwrap_or(A);
        let two: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap_or(B);
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap_or(B);
        assert_eq!(IpKey::from(one), IpKey::from(two));
        assert_ne!(IpKey::from(one), IpKey::from(other));
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap_or(B);
        assert_eq!(IpKey::from(mapped), IpKey::from(A));
    }

    #[test]
    fn request_limiter_allows_bursts_of_two_hundred() {
        let clock: Arc<dyn Clock> = clock();
        let limiter = RequestLimiter::new(50, 200, Arc::clone(&clock));
        for _ in 0..200 {
            assert!(limiter.check(&1u8, &clock).is_ok());
        }
        assert!(limiter.check(&1u8, &clock).is_err());
        assert!(limiter.check(&2u8, &clock).is_ok());
    }

    #[test]
    fn slots_cap_concurrency_per_user() {
        let slots = Slots::new(2);
        let first = slots.try_take(UserKey(1));
        let second = slots.try_take(UserKey(1));
        assert!(first.is_some() && second.is_some());
        assert!(slots.try_take(UserKey(1)).is_none());
        assert!(slots.try_take(UserKey(2)).is_some());
        drop(first);
        assert!(slots.try_take(UserKey(1)).is_some());
    }
}
