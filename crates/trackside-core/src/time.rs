//! Time as Unix milliseconds, and the clock every component reads it from.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::{FromSql, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// Unix milliseconds, UTC. Every `*_at` column holds one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Millis(pub i64);

impl Millis {
    /// Converts a system time, clamping times before 1970 to zero.
    pub fn from_system(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(since) => Self(i64::try_from(since.as_millis()).unwrap_or(i64::MAX)),
            Err(_) => Self(0),
        }
    }

    /// This time plus a duration, saturating at the far future.
    #[must_use]
    pub fn saturating_add(self, by: Duration) -> Self {
        Self(
            self.0
                .saturating_add(i64::try_from(by.as_millis()).unwrap_or(i64::MAX)),
        )
    }

    /// This time minus a duration, saturating at the far past.
    #[must_use]
    pub fn saturating_sub(self, by: Duration) -> Self {
        Self(
            self.0
                .saturating_sub(i64::try_from(by.as_millis()).unwrap_or(i64::MAX)),
        )
    }

    /// How long after `earlier` this is, or zero if it is not later.
    pub fn since(self, earlier: Self) -> Duration {
        Duration::from_millis(u64::try_from(self.0.saturating_sub(earlier.0)).unwrap_or(0))
    }

    /// RFC 3339 UTC with milliseconds, as the API sends times: `2026-11-14T09:30:00.000Z`.
    pub fn to_rfc3339(self) -> String {
        match jiff::Timestamp::from_millisecond(self.0) {
            Ok(ts) => ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            Err(_) => String::from("1970-01-01T00:00:00.000Z"),
        }
    }

    /// Parses an RFC 3339 time with any offset and precision.
    pub fn parse_rfc3339(text: &str) -> Option<Self> {
        let ts: jiff::Timestamp = text.parse().ok()?;
        Some(Self(ts.as_millisecond()))
    }
}

impl ToSql for Millis {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0))
    }
}

impl FromSql for Millis {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_i64().map(Self)
    }
}

/// Where the current time comes from. Tests use [`TestClock`].
pub trait Clock: Send + Sync + std::fmt::Debug + 'static {
    /// The current time.
    fn now(&self) -> Millis;
}

/// The system's wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Millis {
        Millis::from_system(SystemTime::now())
    }
}

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct TestClock {
    now: AtomicI64,
}

impl TestClock {
    /// A clock standing at `start`.
    pub fn new(start: Millis) -> Self {
        Self {
            now: AtomicI64::new(start.0),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        let by = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
        self.now.fetch_add(by, Ordering::SeqCst);
    }

    /// Puts the clock at an exact time, forwards or backwards.
    pub fn set(&self, to: Millis) {
        self.now.store(to.0, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Millis {
        Millis(self.now.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_has_milliseconds_and_z() {
        assert_eq!(
            Millis(1_763_112_600_000).to_rfc3339(),
            "2025-11-14T09:30:00.000Z"
        );
        assert_eq!(
            Millis(1_763_112_600_042).to_rfc3339(),
            "2025-11-14T09:30:00.042Z"
        );
    }

    #[test]
    fn rfc3339_round_trips() {
        let at = Millis(1_763_112_600_123);
        assert_eq!(Millis::parse_rfc3339(&at.to_rfc3339()), Some(at));
        assert_eq!(
            Millis::parse_rfc3339("2025-11-14T10:30:00.123+01:00"),
            Some(at)
        );
        assert_eq!(Millis::parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn test_clock_moves_only_when_told() {
        let clock = TestClock::new(Millis(1_000));
        assert_eq!(clock.now(), Millis(1_000));
        clock.advance(Duration::from_secs(2));
        assert_eq!(clock.now(), Millis(3_000));
        clock.set(Millis(10));
        assert_eq!(clock.now(), Millis(10));
    }
}
