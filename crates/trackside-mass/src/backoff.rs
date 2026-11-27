use std::time::Duration;

/// Exponential reconnect backoff with jitter: attempt `n` waits a random time between half and
/// all of `min(max, initial * 2^n)`, so many Trackside servers never retry in lockstep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// The ceiling for an attempt, before jitter.
    pub fn ceiling(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
        self.initial.saturating_mul(factor).min(self.max)
    }

    pub(crate) fn delay(&self, attempt: u32, rng: &mut fastrand::Rng) -> Duration {
        let ceiling = self.ceiling(attempt);
        let half = ceiling / 2;
        let spread = u64::try_from(ceiling.saturating_sub(half).as_millis()).unwrap_or(u64::MAX);
        half + Duration::from_millis(rng.u64(0..=spread))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceiling_doubles_up_to_the_cap() {
        let backoff = Backoff::default();
        assert_eq!(backoff.ceiling(0), Duration::from_secs(1));
        assert_eq!(backoff.ceiling(3), Duration::from_secs(8));
        assert_eq!(backoff.ceiling(6), Duration::from_secs(60));
        assert_eq!(backoff.ceiling(400), Duration::from_secs(60));
    }

    #[test]
    fn delays_stay_between_half_and_the_ceiling() {
        let backoff = Backoff::default();
        let mut rng = fastrand::Rng::with_seed(7);
        for attempt in 0..40 {
            let delay = backoff.delay(attempt, &mut rng);
            let ceiling = backoff.ceiling(attempt);
            assert!(
                delay >= ceiling / 2 && delay <= ceiling,
                "{attempt}: {delay:?}"
            );
        }
    }
}
