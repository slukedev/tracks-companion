//! Per-provider rate limits and cooldowns.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;
use trackside_core::Millis;
use trackside_core::config::Rate;

/// A token bucket: `count` events per `per`, refilled continuously.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    per_token: Duration,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    pub fn new(rate: Rate) -> Self {
        let count = rate.count.max(1);
        Self {
            capacity: f64::from(count),
            per_token: rate.per / count,
            state: Mutex::new((f64::from(count), Instant::now())),
        }
    }

    /// Takes a token now, or says how long until one is free.
    pub fn try_take(&self) -> Result<(), Duration> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let (tokens, last) = *state;
        let refilled = (tokens
            + now.duration_since(last).as_secs_f64()
                / self.per_token.as_secs_f64().max(f64::EPSILON))
        .min(self.capacity);
        if refilled >= 1.0 {
            *state = (refilled - 1.0, now);
            Ok(())
        } else {
            *state = (refilled, now);
            Err(self.per_token.mul_f64(1.0 - refilled))
        }
    }

    /// Waits for a token.
    pub async fn take(&self) {
        while let Err(wait) = self.try_take() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// The first cooldown after a bot check or throttling.
pub const FIRST_COOLDOWN: Duration = Duration::from_mins(15);
/// The longest cooldown.
pub const MAX_COOLDOWN: Duration = Duration::from_hours(6);

/// A provider's cooldown: 15 minutes, doubling each time up to 6 hours, reset by a success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cooldown {
    until: Option<Millis>,
    next: Duration,
}

impl Default for Cooldown {
    fn default() -> Self {
        Self {
            until: None,
            next: FIRST_COOLDOWN,
        }
    }
}

impl Cooldown {
    /// Starts a cooldown, at least `at_least` long, and returns when it ends.
    pub fn trip(&mut self, now: Millis, at_least: Option<Duration>) -> Millis {
        let length = at_least.map_or(self.next, |d| d.max(self.next));
        let until = now.saturating_add(length);
        self.until = Some(self.until.map_or(until, |u| u.max(until)));
        self.next = (self.next * 2).min(MAX_COOLDOWN);
        until
    }

    /// When the cooldown ends, if it is running.
    pub fn active(&self, now: Millis) -> Option<Millis> {
        self.until.filter(|until| *until > now)
    }

    /// A success: the next cooldown starts from 15 minutes again.
    pub fn succeed(&mut self) {
        self.next = FIRST_COOLDOWN;
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_doubles_up_to_six_hours() {
        let mut cooldown = Cooldown::default();
        let start = Millis(0);
        let lengths: Vec<u64> = (0..8)
            .map(|_| cooldown.trip(start, None).0 as u64 / 60_000)
            .collect();
        assert_eq!(lengths, [15, 30, 60, 120, 240, 360, 360, 360]);
        assert_eq!(cooldown.active(Millis(1)), Some(Millis(360 * 60_000)));
        cooldown.succeed();
        assert_eq!(cooldown.trip(start, None), Millis(15 * 60_000));
    }

    #[test]
    fn retry_after_lengthens_a_cooldown() {
        let mut cooldown = Cooldown::default();
        let until = cooldown.trip(Millis(0), Some(Duration::from_secs(3600)));
        assert_eq!(until, Millis(3_600_000));
    }

    #[tokio::test(start_paused = true)]
    async fn the_bucket_allows_its_rate() {
        let bucket = TokenBucket::new(Rate {
            count: 2,
            per: Duration::from_secs(6),
        });
        assert!(bucket.try_take().is_ok());
        assert!(bucket.try_take().is_ok());
        let wait = bucket.try_take().unwrap_err();
        assert!(wait <= Duration::from_secs(3));
        let start = Instant::now();
        bucket.take().await;
        assert!(start.elapsed() >= Duration::from_secs(2));
    }
}
