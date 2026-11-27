//! Clock synchronisation (messaging.md, Clock Synchronization): the spec's two-state Kalman
//! filter over `client/time` exchanges, and the mapping from server timestamps to local time.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::Instant;

const PROCESS_STD_DEV: f64 = 0.0;
const DRIFT_PROCESS_STD_DEV: f64 = 1e-11;
const FORGET_FACTOR: f64 = 2.0;
const ADAPTIVE_CUTOFF: f64 = 3.0;
const MIN_SAMPLES: u32 = 100;
const DRIFT_SIGNIFICANCE_THRESHOLD: f64 = 2.0;
const MAX_ERROR_SCALE: f64 = 0.5;

/// One `client/time` exchange: `T1` to `T4` in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exchange {
    pub client_transmitted: i64,
    pub server_received: i64,
    pub server_transmitted: i64,
    pub client_received: i64,
}

impl Exchange {
    pub fn measurement(&self) -> i64 {
        i64::midpoint(
            self.server_received - self.client_transmitted,
            self.server_transmitted - self.client_received,
        )
    }

    pub fn max_error(&self) -> i64 {
        ((self.client_received - self.client_transmitted)
            - (self.server_transmitted - self.server_received))
            / 2
    }
}

/// The spec's time filter, parameters as recommended.
#[derive(Clone, Debug, PartialEq)]
pub struct TimeFilter {
    offset: f64,
    drift: f64,
    p_oo: f64,
    p_od: f64,
    p_dd: f64,
    last_update: i64,
    count: u32,
    use_drift: bool,
}

impl Default for TimeFilter {
    fn default() -> Self {
        Self {
            offset: 0.0,
            drift: 0.0,
            p_oo: f64::INFINITY,
            p_od: 0.0,
            p_dd: 0.0,
            last_update: 0,
            count: 0,
            use_drift: false,
        }
    }
}

impl TimeFilter {
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Server minus client clock, microseconds.
    pub fn offset(&self) -> f64 {
        self.offset
    }

    pub fn drift(&self) -> f64 {
        self.drift
    }

    pub fn uses_drift(&self) -> bool {
        self.use_drift
    }

    /// The standard deviation of the offset estimate, microseconds.
    pub fn error(&self) -> f64 {
        self.p_oo.sqrt()
    }

    pub fn update_exchange(&mut self, exchange: &Exchange) {
        self.update(
            exchange.measurement(),
            exchange.max_error(),
            exchange.client_received,
        );
    }

    #[expect(clippy::similar_names, reason = "the spec's names for the covariances")]
    pub fn update(&mut self, measurement: i64, max_error: i64, time_added: i64) {
        if time_added <= self.last_update || max_error <= 0 {
            return;
        }
        let dt = (time_added - self.last_update) as f64;
        self.last_update = time_added;
        let measurement = measurement as f64;
        let max_error = max_error as f64;
        let r = (MAX_ERROR_SCALE * max_error).powi(2);

        if self.count == 0 {
            self.offset = measurement;
            self.p_oo = r;
            self.drift = 0.0;
            self.count = 1;
            return;
        }
        if self.count == 1 {
            self.drift = (measurement - self.offset) / dt;
            self.p_dd = (self.p_oo + r) / (dt * dt);
            self.offset = measurement;
            self.p_oo = r;
            self.count = 2;
            return;
        }

        let offset_p = self.offset + self.drift * dt;
        let mut p_oo_p =
            self.p_oo + 2.0 * self.p_od * dt + self.p_dd * dt * dt + PROCESS_STD_DEV.powi(2) * dt;
        let mut p_od_p = self.p_od + self.p_dd * dt;
        let mut p_dd_p = self.p_dd + DRIFT_PROCESS_STD_DEV.powi(2) * dt;

        let y = measurement - offset_p;
        if self.count < MIN_SAMPLES {
            self.count += 1;
        } else if y.abs() > ADAPTIVE_CUTOFF * max_error {
            let forget = FORGET_FACTOR.powi(2);
            p_oo_p *= forget;
            p_od_p *= forget;
            p_dd_p *= forget;
        }

        let s = p_oo_p + r;
        let k_o = p_oo_p / s;
        let k_d = p_od_p / s;
        self.offset = offset_p + k_o * y;
        self.drift += k_d * y;
        self.p_oo = p_oo_p - k_o * p_oo_p;
        self.p_od = p_od_p - k_d * p_oo_p;
        self.p_dd = p_dd_p - k_d * p_od_p;
        self.use_drift = self.drift.powi(2) > DRIFT_SIGNIFICANCE_THRESHOLD.powi(2) * self.p_dd;
    }

    fn effective_drift(&self) -> f64 {
        if self.use_drift { self.drift } else { 0.0 }
    }

    pub fn server_time(&self, client: i64) -> i64 {
        let d = self.effective_drift();
        client + round_away(self.offset + d * (client - self.last_update) as f64)
    }

    pub fn client_time(&self, server: i64) -> i64 {
        let d = self.effective_drift();
        round_away((server as f64 - self.offset + d * self.last_update as f64) / (1.0 + d))
    }
}

fn round_away(value: f64) -> i64 {
    value.round() as i64
}

/// The local monotonic clock the client reports in `client/time`, and the filter that maps
/// server timestamps onto it. Cheap to clone; shared by the connection and its consumers.
#[derive(Clone, Debug)]
pub struct Clock {
    base: Instant,
    filter: Arc<Mutex<TimeFilter>>,
}

/// Where the local clock starts counting, so the first `T4` is well above the filter's
/// initial `last_update` of 0.
const LOCAL_EPOCH_US: i64 = 1_000_000;

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            filter: Arc::new(Mutex::new(TimeFilter::default())),
        }
    }

    /// The local clock, microseconds.
    pub fn now_us(&self) -> i64 {
        LOCAL_EPOCH_US + i64::try_from(self.base.elapsed().as_micros()).unwrap_or(i64::MAX / 2)
    }

    /// The local instant of a local-clock reading.
    pub fn instant_at(&self, local_us: i64) -> Instant {
        let since = u64::try_from(local_us - LOCAL_EPOCH_US).unwrap_or(0);
        self.base + std::time::Duration::from_micros(since)
    }

    pub fn is_synced(&self) -> bool {
        self.with(|f| f.count() >= 1)
    }

    /// A server timestamp on the local clock, once at least one measurement is in.
    pub fn to_local_us(&self, server_us: i64) -> Option<i64> {
        self.with(|f| (f.count() >= 1).then(|| f.client_time(server_us)))
    }

    /// The server clock now, once synced.
    pub fn server_now_us(&self) -> Option<i64> {
        let now = self.now_us();
        self.with(|f| (f.count() >= 1).then(|| f.server_time(now)))
    }

    pub fn filter(&self) -> TimeFilter {
        self.with(Clone::clone)
    }

    /// Forgets every measurement: a new connection may reach a restarted server clock.
    pub(crate) fn reset(&self) {
        *self.filter.lock().unwrap_or_else(PoisonError::into_inner) = TimeFilter::default();
    }

    pub(crate) fn update(&self, exchange: &Exchange) {
        self.filter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .update_exchange(exchange);
    }

    fn with<T>(&self, f: impl FnOnce(&TimeFilter) -> T) -> T {
        f(&self.filter.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(filter: &mut TimeFilter, offset: i64, count: usize) -> i64 {
        let mut t = 1_000_000;
        for _ in 0..count {
            filter.update(offset, 1000, t);
            t += 1_000_000;
        }
        t
    }

    #[test]
    fn the_first_sample_seeds_the_offset() {
        let mut f = TimeFilter::default();
        f.update(12_345, 500, 1_000_000);
        assert_eq!(f.count(), 1);
        assert!((f.offset() - 12_345.0).abs() < f64::EPSILON);
        assert!(f.drift().abs() < f64::EPSILON);
    }

    #[test]
    fn the_second_sample_estimates_drift_from_the_slope() {
        let mut f = TimeFilter::default();
        f.update(1_000, 500, 1_000_000);
        f.update(2_000, 500, 2_000_000);
        assert_eq!(f.count(), 2);
        assert!((f.drift() - 0.001).abs() < 1e-12);
        assert!(!f.uses_drift());
    }

    #[test]
    fn samples_without_positive_error_or_going_backwards_are_ignored() {
        let mut f = TimeFilter::default();
        let t = feed(&mut f, 1_000, 3);
        let before = f.clone();
        f.update(9_999, 0, t);
        f.update(9_999, -1, t);
        f.update(9_999, 500, t - 2_000_000);
        assert_eq!(f, before);
    }

    #[test]
    fn a_constant_offset_converges() {
        let mut f = TimeFilter::default();
        let t = feed(&mut f, 250_000, 20);
        let client = t + 500_000;
        assert!((f.server_time(client) - (client + 250_000)).abs() <= 1);
    }

    #[test]
    fn conversions_invert_each_other_with_drift() {
        let mut f = TimeFilter::default();
        let mut t = 1_000_000i64;
        for _ in 0..50 {
            f.update((100_000.0 + 1e-4 * t as f64).round() as i64, 200, t);
            t += 1_000_000;
        }
        for client in [t, t + 500_000, t + 5_000_000] {
            assert!((f.client_time(f.server_time(client)) - client).abs() <= 1);
        }
    }

    #[test]
    fn exchange_arithmetic_matches_the_spec() {
        let e = Exchange {
            client_transmitted: 100,
            server_received: 1150,
            server_transmitted: 1160,
            client_received: 130,
        };
        assert_eq!(e.measurement(), 1040);
        assert_eq!(e.max_error(), (30 - 10) / 2);
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        assert_eq!(round_away(2.5), 3);
        assert_eq!(round_away(-2.5), -3);
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_maps_nothing_before_a_measurement() {
        let clock = Clock::new();
        assert_eq!(clock.to_local_us(5), None);
        clock.update(&Exchange {
            client_transmitted: clock.now_us(),
            server_received: 10_000_000,
            server_transmitted: 10_000_000,
            client_received: clock.now_us() + 2,
        });
        assert!(clock.is_synced());
        assert!(clock.to_local_us(10_000_000).is_some());
    }
}
