//! Exponentially Weighted Moving Average (EWMA) for smoothed metric tracking.
//!
//! EWMA provides a smoothed estimate of a time series that gives more weight
//! to recent observations while exponentially decaying older ones. The decay
//! rate is controlled by a half-life parameter.
//!
//! # Formula
//!
//! Given half-life `h` and time delta `dt`:
//!
//! > alpha = 1 - 0.5^(dt/h)
//! > EWMA = alpha * new_value + (1 - alpha) * old_EWMA
//!
//! # Use cases in FrankenTerm
//!
//! - **Throughput smoothing**: Smooth per-pane byte rates for display.
//! - **Error rate tracking**: Smoothed error rate for alert thresholds.
//! - **Latency trends**: Detect latency regressions via EWMA + Z-score.
//! - **Rate estimation**: Events/sec from irregular timestamps.

use serde::{Deserialize, Serialize};

// =============================================================================
// Ewma
// =============================================================================

/// Exponentially Weighted Moving Average with time-based decay.
///
/// Automatically adjusts the smoothing factor based on the actual time
/// between observations, making it robust to irregular sampling intervals.
///
/// # Example
///
/// ```ignore
/// let mut ewma = Ewma::with_half_life_ms(1000); // 1 second half-life
/// ewma.observe(100.0, 0);
/// ewma.observe(200.0, 500);  // alpha ≈ 0.29
/// ewma.observe(150.0, 1000); // alpha ≈ 0.29
/// let smoothed = ewma.value(); // ~158.6
/// ```
#[derive(Debug, Clone)]
pub struct Ewma {
    /// Half-life in milliseconds.
    half_life_ms: f64,
    /// Current smoothed value.
    value: f64,
    /// Whether we've received at least one observation.
    initialized: bool,
    /// Timestamp of last observation (ms).
    last_time_ms: u64,
    /// Number of observations.
    count: u64,
}

impl Ewma {
    /// Create a new EWMA with the given half-life in milliseconds.
    ///
    /// # Panics
    ///
    /// Panics if `half_life_ms` is not positive.
    #[must_use]
    pub fn with_half_life_ms(half_life_ms: f64) -> Self {
        assert!(half_life_ms > 0.0, "half_life_ms must be positive");
        Self {
            half_life_ms,
            value: 0.0,
            initialized: false,
            last_time_ms: 0,
            count: 0,
        }
    }

    /// Create a new EWMA with the given half-life in seconds.
    #[must_use]
    pub fn with_half_life_secs(half_life_secs: f64) -> Self {
        Self::with_half_life_ms(half_life_secs * 1000.0)
    }

    /// Observe a new value at the given timestamp (milliseconds).
    pub fn observe(&mut self, value: f64, time_ms: u64) {
        self.count += 1;
        if !self.initialized {
            self.value = value;
            self.initialized = true;
            self.last_time_ms = time_ms;
            return;
        }

        let dt = time_ms.saturating_sub(self.last_time_ms) as f64;
        let alpha = if dt <= 0.0 {
            0.5 // instantaneous: weight equally
        } else {
            1.0 - 0.5_f64.powf(dt / self.half_life_ms)
        };

        self.value = alpha * value + (1.0 - alpha) * self.value;
        self.last_time_ms = time_ms;
    }

    /// Current smoothed value.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Whether any observations have been made.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Number of observations.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Half-life in milliseconds.
    #[must_use]
    pub fn half_life_ms(&self) -> f64 {
        self.half_life_ms
    }

    /// Reset to uninitialized state.
    pub fn reset(&mut self) {
        self.value = 0.0;
        self.initialized = false;
        self.last_time_ms = 0;
        self.count = 0;
    }
}

// =============================================================================
// EwmaWithVariance (EWMA + variance for Z-score anomaly detection)
// =============================================================================

/// EWMA with online variance tracking for anomaly detection.
///
/// Tracks both the smoothed mean and smoothed variance, enabling Z-score
/// based anomaly detection without storing historical data.
///
/// # Example
///
/// ```ignore
/// let mut tracker = EwmaWithVariance::with_half_life_ms(1000);
/// // ... observe many normal values ...
/// let z = tracker.z_score(extreme_value);
/// if z.abs() > 3.0 { // 3-sigma anomaly
///     alert("anomaly detected!");
/// }
/// ```
#[derive(Debug, Clone)]
pub struct EwmaWithVariance {
    mean: Ewma,
    /// Smoothed variance (EWMA of squared deviations).
    variance: f64,
    /// Half-life for variance tracking.
    half_life_ms: f64,
    /// Last observation timestamp.
    last_time_ms: u64,
    /// Whether variance has been initialized (needs at least 2 observations).
    variance_initialized: bool,
}

impl EwmaWithVariance {
    /// Create a new EWMA tracker with variance.
    #[must_use]
    pub fn with_half_life_ms(half_life_ms: f64) -> Self {
        Self {
            mean: Ewma::with_half_life_ms(half_life_ms),
            variance: 0.0,
            half_life_ms,
            last_time_ms: 0,
            variance_initialized: false,
        }
    }

    /// Observe a new value.
    pub fn observe(&mut self, value: f64, time_ms: u64) {
        let old_mean = self.mean.value();
        self.mean.observe(value, time_ms);

        if self.mean.count() >= 2 {
            let dt = time_ms.saturating_sub(self.last_time_ms) as f64;
            let alpha = if dt <= 0.0 {
                0.5
            } else {
                1.0 - 0.5_f64.powf(dt / self.half_life_ms)
            };

            let deviation_sq = (value - old_mean).powi(2);
            if self.variance_initialized {
                self.variance = alpha * deviation_sq + (1.0 - alpha) * self.variance;
            } else {
                self.variance = deviation_sq;
                self.variance_initialized = true;
            }
        }
        self.last_time_ms = time_ms;
    }

    /// Current smoothed mean.
    #[must_use]
    pub fn mean(&self) -> f64 {
        self.mean.value()
    }

    /// Current smoothed variance.
    #[must_use]
    pub fn variance(&self) -> f64 {
        self.variance
    }

    /// Current smoothed standard deviation.
    #[must_use]
    pub fn stddev(&self) -> f64 {
        self.variance.sqrt()
    }

    /// Z-score of a value relative to the current mean and stddev.
    ///
    /// Returns 0.0 if stddev is zero (insufficient data).
    #[must_use]
    pub fn z_score(&self, value: f64) -> f64 {
        let sd = self.stddev();
        if sd < f64::EPSILON {
            0.0
        } else {
            (value - self.mean.value()) / sd
        }
    }

    /// Whether the value is anomalous (|z-score| > threshold).
    #[must_use]
    pub fn is_anomaly(&self, value: f64, sigma_threshold: f64) -> bool {
        self.z_score(value).abs() > sigma_threshold
    }

    /// Number of observations.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.mean.count()
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.mean.reset();
        self.variance = 0.0;
        self.last_time_ms = 0;
        self.variance_initialized = false;
    }
}

// =============================================================================
// RateEstimator (events/sec from irregular timestamps)
// =============================================================================

/// Estimates the rate (events per second) from irregularly-timed events.
///
/// Uses EWMA to smooth the inter-arrival time, then inverts to get rate.
///
/// # Example
///
/// ```ignore
/// let mut rate = RateEstimator::with_half_life_ms(5000);
/// rate.tick(1000);
/// rate.tick(1100);  // 100ms interval → ~10 events/sec
/// rate.tick(1200);  // 100ms interval → ~10 events/sec
/// assert!((rate.rate_per_sec() - 10.0).abs() < 1.0);
/// ```
#[derive(Debug, Clone)]
pub struct RateEstimator {
    /// EWMA of inter-arrival times (ms).
    interval_ewma: Ewma,
    /// Last event timestamp.
    last_tick_ms: Option<u64>,
    /// Total events.
    total_events: u64,
}

impl RateEstimator {
    /// Create a new rate estimator with the given half-life.
    #[must_use]
    pub fn with_half_life_ms(half_life_ms: f64) -> Self {
        Self {
            interval_ewma: Ewma::with_half_life_ms(half_life_ms),
            last_tick_ms: None,
            total_events: 0,
        }
    }

    /// Record an event at the given timestamp.
    pub fn tick(&mut self, time_ms: u64) {
        self.total_events += 1;
        if let Some(last) = self.last_tick_ms {
            let interval = time_ms.saturating_sub(last) as f64;
            self.interval_ewma.observe(interval, time_ms);
        }
        self.last_tick_ms = Some(time_ms);
    }

    /// Estimated rate in events per second.
    ///
    /// Returns 0.0 if fewer than 2 events have been observed.
    #[must_use]
    pub fn rate_per_sec(&self) -> f64 {
        if !self.interval_ewma.is_initialized() {
            return 0.0;
        }
        let avg_interval_ms = self.interval_ewma.value();
        if avg_interval_ms < f64::EPSILON {
            0.0
        } else {
            1000.0 / avg_interval_ms
        }
    }

    /// Total events observed.
    #[must_use]
    pub fn total_events(&self) -> u64 {
        self.total_events
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.interval_ewma.reset();
        self.last_tick_ms = None;
        self.total_events = 0;
    }
}

// =============================================================================
// EwmaStats (serializable)
// =============================================================================

/// Serializable EWMA statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EwmaStats {
    /// Current smoothed value.
    pub value: f64,
    /// Number of observations.
    pub count: u64,
    /// Half-life in milliseconds.
    pub half_life_ms: f64,
}

impl Ewma {
    /// Get serializable statistics.
    #[must_use]
    pub fn stats(&self) -> EwmaStats {
        EwmaStats {
            value: self.value,
            count: self.count,
            half_life_ms: self.half_life_ms,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Ewma basic -------------------------------------------------------------

    #[test]
    fn empty_ewma() {
        let ewma = Ewma::with_half_life_ms(1000.0);
        assert!(!ewma.is_initialized());
        assert_eq!(ewma.count(), 0);
        assert!(ewma.value().abs() < f64::EPSILON);
    }

    #[test]
    fn first_observation_sets_value() {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        ewma.observe(42.0, 0);
        assert!(ewma.is_initialized());
        assert!((ewma.value() - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn decays_toward_new_value() {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        ewma.observe(0.0, 0);
        ewma.observe(100.0, 1000); // exactly one half-life later
        // After one half-life, alpha = 0.5, so EWMA = 0.5*100 + 0.5*0 = 50
        assert!((ewma.value() - 50.0).abs() < 0.1, "value={}", ewma.value());
    }

    #[test]
    fn long_time_converges() {
        let mut ewma = Ewma::with_half_life_ms(100.0);
        ewma.observe(0.0, 0);
        ewma.observe(100.0, 10_000); // 100 half-lives later
        // Should be very close to 100.
        assert!((ewma.value() - 100.0).abs() < 0.01);
    }

    #[test]
    fn short_time_barely_moves() {
        let mut ewma = Ewma::with_half_life_ms(10_000.0);
        ewma.observe(0.0, 0);
        ewma.observe(100.0, 1); // 1ms vs 10s half-life
        // Should barely move from 0.
        assert!(ewma.value() < 1.0);
    }

    #[test]
    fn from_seconds() {
        let ewma = Ewma::with_half_life_secs(1.0);
        assert!(
            (ewma.half_life_ms() - 1000.0).abs() < f64::EPSILON,
            "half_life_ms: {}",
            ewma.half_life_ms()
        );
    }

    #[test]
    fn reset() {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        ewma.observe(42.0, 0);
        ewma.reset();
        assert!(!ewma.is_initialized());
        assert_eq!(ewma.count(), 0);
    }

    #[test]
    fn stats_serializable() {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        ewma.observe(42.0, 0);
        let s = ewma.stats();
        let json = serde_json::to_string(&s).unwrap();
        let back: EwmaStats = serde_json::from_str(&json).unwrap();
        assert!((s.value - back.value).abs() < f64::EPSILON);
    }

    // -- EwmaWithVariance -------------------------------------------------------

    #[test]
    fn variance_needs_two_observations() {
        let mut t = EwmaWithVariance::with_half_life_ms(1000.0);
        t.observe(10.0, 0);
        assert!(t.variance().abs() < f64::EPSILON);
    }

    #[test]
    fn variance_after_observations() {
        let mut t = EwmaWithVariance::with_half_life_ms(1000.0);
        t.observe(10.0, 0);
        t.observe(20.0, 500);
        // Variance should be > 0 after divergent observations.
        assert!(t.variance() > 0.0, "variance={}", t.variance());
    }

    #[test]
    fn z_score_zero_when_no_variance() {
        let mut t = EwmaWithVariance::with_half_life_ms(1000.0);
        t.observe(10.0, 0);
        assert!(t.z_score(20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn z_score_detects_anomaly() {
        let mut t = EwmaWithVariance::with_half_life_ms(1000.0);
        // Feed steady values.
        for i in 0..20 {
            t.observe(100.0 + (i % 3) as f64, i * 100);
        }
        // Large spike.
        let z = t.z_score(500.0);
        assert!(z > 3.0, "z={z}, expected > 3.0");
        assert!(t.is_anomaly(500.0, 3.0));
    }

    #[test]
    fn z_score_normal_not_anomaly() {
        let mut t = EwmaWithVariance::with_half_life_ms(1000.0);
        for i in 0..20 {
            t.observe(100.0, i * 100);
        }
        // Value close to the mean.
        assert!(!t.is_anomaly(101.0, 3.0));
    }

    #[test]
    fn variance_reset() {
        let mut t = EwmaWithVariance::with_half_life_ms(1000.0);
        t.observe(10.0, 0);
        t.observe(20.0, 100);
        t.reset();
        assert_eq!(t.count(), 0);
        assert!(t.variance().abs() < f64::EPSILON);
    }

    // -- RateEstimator ----------------------------------------------------------

    #[test]
    fn rate_empty() {
        let r = RateEstimator::with_half_life_ms(1000.0);
        assert!(r.rate_per_sec().abs() < f64::EPSILON);
        assert_eq!(r.total_events(), 0);
    }

    #[test]
    fn rate_single_event() {
        let mut r = RateEstimator::with_half_life_ms(1000.0);
        r.tick(0);
        assert!(r.rate_per_sec().abs() < f64::EPSILON); // need at least 2
    }

    #[test]
    fn rate_uniform_events() {
        let mut r = RateEstimator::with_half_life_ms(5000.0);
        // 10 events/sec = 100ms apart.
        for i in 0..20 {
            r.tick(i * 100);
        }
        let rate = r.rate_per_sec();
        assert!((rate - 10.0).abs() < 2.0, "rate={rate}, expected ~10.0");
    }

    #[test]
    fn rate_changing_speed() {
        let mut r = RateEstimator::with_half_life_ms(500.0);
        // First: fast events (100ms apart = 10/sec).
        for i in 0..10 {
            r.tick(i * 100);
        }
        let fast_rate = r.rate_per_sec();

        // Then: slow events (1000ms apart = 1/sec).
        let base = 1000;
        for i in 0..10 {
            r.tick(base + i * 1000);
        }
        let slow_rate = r.rate_per_sec();

        assert!(fast_rate > slow_rate, "fast={fast_rate}, slow={slow_rate}");
    }

    #[test]
    fn rate_reset() {
        let mut r = RateEstimator::with_half_life_ms(1000.0);
        r.tick(0);
        r.tick(100);
        r.reset();
        assert_eq!(r.total_events(), 0);
        assert!(r.rate_per_sec().abs() < f64::EPSILON);
    }

    // -- Panics -----------------------------------------------------------------

    #[test]
    #[should_panic(expected = "half_life_ms must be positive")]
    fn zero_half_life_panics() {
        let _ = Ewma::with_half_life_ms(0.0);
    }

    #[test]
    #[should_panic(expected = "half_life_ms must be positive")]
    fn negative_half_life_panics() {
        let _ = Ewma::with_half_life_ms(-1.0);
    }
}
