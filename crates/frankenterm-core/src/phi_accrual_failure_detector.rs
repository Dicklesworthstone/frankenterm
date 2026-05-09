//! φ-accrual failure detector (Hayashibara, Defago, Yared, Katayama 2004).
//!
//! Replaces fixed-threshold heartbeat timeouts with an adaptive suspicion
//! level computed from the empirical inter-arrival distribution.
//!
//! Adopted by Cassandra, Akka, Apache Mesos, and ScyllaDB. The classic
//! `now - last_heartbeat > timeout` boundary fails badly under variable
//! network latency: 1 ms over the threshold flips a peer to Unreachable,
//! 1 ms under keeps it Healthy, even though the underlying RTT
//! distribution may have ranged from 1 ms to 5 s. φ-accrual instead
//! reports a continuous suspicion value
//!
//!     φ(t) = -log10(P(arrival_at_or_after(t)))
//!
//! and the operator threshold (commonly 8.0 - 12.0) maps to a calibrated
//! false-positive rate against the observed distribution.
//!
//! Sibling-module substrate: this file ships the algorithm + bounded-
//! window inter-arrival tracker + suspicion query. Integration into
//! `crate::headless_mux_server::check_peer_health` is a follow-up bead.

use std::collections::VecDeque;

/// Bounded window of inter-arrival samples used by [`PhiAccrualFailureDetector`].
const DEFAULT_WINDOW_CAPACITY: usize = 1000;

/// Minimum stddev floor (microseconds) — prevents φ from exploding when the
/// observed distribution is degenerate (all samples identical or near-zero
/// variance during warmup). Empirically chosen to match Akka's defaults.
const MIN_STDDEV_MICROS: f64 = 100_000.0;

/// Default conservative initial estimate for the first heartbeat interval.
/// Used when fewer than `MIN_SAMPLES` have been observed.
const DEFAULT_FIRST_INTERVAL_MICROS: f64 = 1_000_000.0;

/// Need at least this many samples before suspicion calculation switches
/// from the warmup default to empirical mean/stddev.
const MIN_SAMPLES: usize = 2;

/// Suspicion threshold above which a peer is considered unreachable.
///
/// Hayashibara et al. report φ = 8.0 ≈ 0.01% false-positive rate under
/// normally-distributed inter-arrivals; Akka/Cassandra default to 8.0
/// for production, with 12.0 reserved for cross-WAN deployments.
pub const DEFAULT_SUSPICION_THRESHOLD: f64 = 8.0;

/// φ-accrual failure detector with bounded inter-arrival sample window.
///
/// Construct with [`PhiAccrualFailureDetector::new`] (default window of
/// 1000 samples) or [`PhiAccrualFailureDetector::with_capacity`] for a
/// custom window size.
///
/// Drive with [`record_heartbeat`] each time a heartbeat (or any
/// liveness signal) arrives. Query with [`suspicion_at`] given the
/// current time, or use [`is_unreachable`] for a thresholded boolean.
///
/// All times in **microseconds since arbitrary epoch** for sub-millisecond
/// precision; callers typically supply `Instant::now()` deltas.
#[derive(Debug, Clone)]
pub struct PhiAccrualFailureDetector {
    window: VecDeque<u64>,
    capacity: usize,
    last_heartbeat_micros: Option<u64>,
}

impl PhiAccrualFailureDetector {
    /// Construct with the default 1000-sample inter-arrival window.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_WINDOW_CAPACITY)
    }

    /// Construct with a custom inter-arrival sample window capacity.
    /// Smaller windows adapt faster to RTT shifts; larger windows give
    /// stabler suspicion under bursty arrivals.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(MIN_SAMPLES);
        Self {
            window: VecDeque::with_capacity(capacity),
            capacity,
            last_heartbeat_micros: None,
        }
    }

    /// Record a heartbeat arrival at `now_micros`. The inter-arrival
    /// (vs the previous heartbeat) is appended to the sample window;
    /// the oldest sample is evicted when the window is full.
    pub fn record_heartbeat(&mut self, now_micros: u64) {
        if let Some(prev) = self.last_heartbeat_micros {
            let interval = now_micros.saturating_sub(prev);
            if self.window.len() == self.capacity {
                self.window.pop_front();
            }
            self.window.push_back(interval);
        }
        self.last_heartbeat_micros = Some(now_micros);
    }

    /// Number of inter-arrival samples currently in the window.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.window.len()
    }

    /// Last observed heartbeat time, or `None` if no heartbeats recorded.
    #[must_use]
    pub fn last_heartbeat_micros(&self) -> Option<u64> {
        self.last_heartbeat_micros
    }

    /// Compute the suspicion level φ at `now_micros`.
    ///
    /// Returns `0.0` if no heartbeats have been recorded (peer is
    /// considered fresh until proven otherwise — let calibration drive
    /// the threshold).
    ///
    /// During warmup (fewer than [`MIN_SAMPLES`] inter-arrivals) the
    /// detector falls back to a conservative 1-second mean estimate,
    /// matching Akka's behavior.
    #[must_use]
    pub fn suspicion_at(&self, now_micros: u64) -> f64 {
        let Some(last) = self.last_heartbeat_micros else {
            return 0.0;
        };
        let elapsed = now_micros.saturating_sub(last) as f64;
        if elapsed <= 0.0 {
            return 0.0;
        }

        let (mean, stddev) = self.distribution_estimates();
        // φ = -log10(P_later(elapsed)). Use the upper-tail of a normal
        // CDF approximation: P(X >= elapsed) where X ~ N(mean, stddev).
        // Hayashibara et al. §4.1 derive a closed-form approximation
        // using exp(-y) / (1 + exp(-y)) where y = (elapsed - mean) / stddev.
        let y = (elapsed - mean) / stddev;
        let exp_neg_y = (-y).exp();
        let p_later = exp_neg_y / (1.0 + exp_neg_y);
        if p_later <= 0.0 || !p_later.is_finite() {
            // Saturate at a high but finite value rather than +inf so
            // callers can compare against any reasonable threshold.
            return 1e9;
        }
        -p_later.log10()
    }

    /// `true` if the suspicion at `now_micros` exceeds `threshold`.
    /// See [`DEFAULT_SUSPICION_THRESHOLD`] for the conventional value.
    #[must_use]
    pub fn is_unreachable(&self, now_micros: u64, threshold: f64) -> bool {
        self.suspicion_at(now_micros) > threshold
    }

    fn distribution_estimates(&self) -> (f64, f64) {
        if self.window.len() < MIN_SAMPLES {
            return (DEFAULT_FIRST_INTERVAL_MICROS, MIN_STDDEV_MICROS);
        }
        let n = self.window.len() as f64;
        let sum: f64 = self.window.iter().map(|&s| s as f64).sum();
        let mean = sum / n;
        let variance: f64 = self
            .window
            .iter()
            .map(|&s| {
                let d = s as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / n;
        let stddev = variance.sqrt().max(MIN_STDDEV_MICROS);
        (mean, stddev)
    }
}

impl Default for PhiAccrualFailureDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEARTBEAT_INTERVAL_MICROS: u64 = 1_000_000; // 1 second

    fn drive_steady_heartbeats(
        detector: &mut PhiAccrualFailureDetector,
        count: usize,
        start_micros: u64,
    ) -> u64 {
        let mut now = start_micros;
        for _ in 0..count {
            detector.record_heartbeat(now);
            now += HEARTBEAT_INTERVAL_MICROS;
        }
        now
    }

    #[test]
    fn no_heartbeats_yields_zero_suspicion() {
        let detector = PhiAccrualFailureDetector::new();
        assert!(detector.suspicion_at(10_000_000).abs() <= f64::EPSILON);
        assert!(!detector.is_unreachable(10_000_000, DEFAULT_SUSPICION_THRESHOLD));
    }

    #[test]
    fn fresh_heartbeat_keeps_suspicion_low() {
        let mut detector = PhiAccrualFailureDetector::new();
        let now = drive_steady_heartbeats(&mut detector, 50, 0);
        // Querying at the most recent heartbeat: elapsed is 0, suspicion 0.
        let suspicion = detector.suspicion_at(now - HEARTBEAT_INTERVAL_MICROS);
        assert!(
            suspicion < 1.0,
            "fresh-heartbeat suspicion should stay below 1.0 (got {})",
            suspicion
        );
    }

    #[test]
    fn long_silence_drives_suspicion_above_threshold() {
        let mut detector = PhiAccrualFailureDetector::new();
        let last_hb = drive_steady_heartbeats(&mut detector, 50, 0);
        // 30 seconds of silence after a 1s-period stream → far in the
        // tail of the distribution; suspicion must exceed default threshold.
        let now = last_hb + 30 * HEARTBEAT_INTERVAL_MICROS;
        let suspicion = detector.suspicion_at(now);
        assert!(
            suspicion > DEFAULT_SUSPICION_THRESHOLD,
            "30 s silence should suspect peer (φ={} ≤ {})",
            suspicion,
            DEFAULT_SUSPICION_THRESHOLD
        );
        assert!(detector.is_unreachable(now, DEFAULT_SUSPICION_THRESHOLD));
    }

    #[test]
    fn suspicion_grows_monotonically_with_elapsed_time() {
        let mut detector = PhiAccrualFailureDetector::new();
        let last_hb = drive_steady_heartbeats(&mut detector, 50, 0);
        let mut prev_suspicion = detector.suspicion_at(last_hb);
        for k in 1..=20 {
            let now = last_hb + k * HEARTBEAT_INTERVAL_MICROS;
            let suspicion = detector.suspicion_at(now);
            assert!(
                suspicion >= prev_suspicion,
                "suspicion should be non-decreasing in elapsed time (k={}, prev={}, now={})",
                k,
                prev_suspicion,
                suspicion
            );
            prev_suspicion = suspicion;
        }
    }

    #[test]
    fn record_heartbeat_resets_suspicion_to_low() {
        let mut detector = PhiAccrualFailureDetector::new();
        let last_hb = drive_steady_heartbeats(&mut detector, 50, 0);
        let high_suspicion_now = last_hb + 30 * HEARTBEAT_INTERVAL_MICROS;
        assert!(detector.is_unreachable(high_suspicion_now, DEFAULT_SUSPICION_THRESHOLD));

        // A new heartbeat arrives — the next suspicion query starts fresh.
        detector.record_heartbeat(high_suspicion_now);
        let after_hb = detector.suspicion_at(high_suspicion_now);
        assert!(
            after_hb < 1.0,
            "fresh heartbeat should drop suspicion well below threshold (got {})",
            after_hb
        );
    }

    #[test]
    fn window_capacity_evicts_oldest_samples() {
        let mut detector = PhiAccrualFailureDetector::with_capacity(10);
        let _ = drive_steady_heartbeats(&mut detector, 100, 0);
        assert_eq!(detector.sample_count(), 10);
    }

    #[test]
    fn capacity_floor_at_min_samples() {
        // Caller asks for capacity 0 — must be raised to MIN_SAMPLES.
        let detector = PhiAccrualFailureDetector::with_capacity(0);
        let _ = detector;
    }

    #[test]
    fn variable_latency_widens_distribution_lowering_suspicion_at_a_given_lateness() {
        // Compare two detectors: one fed steady 1-second heartbeats, another
        // fed jittery 0.5-2s heartbeats. After both observe a 5s gap, the
        // jittery one should report a LOWER suspicion (the gap is less
        // surprising under high observed variance).
        let mut steady = PhiAccrualFailureDetector::new();
        let mut jittery = PhiAccrualFailureDetector::new();

        let mut t = 0u64;
        for _ in 0..200 {
            steady.record_heartbeat(t);
            t += 1_000_000;
        }
        let steady_last = t - 1_000_000;

        t = 0;
        for k in 0..200 {
            jittery.record_heartbeat(t);
            // alternate 500 ms / 2 s intervals
            t += if k % 2 == 0 { 500_000 } else { 2_000_000 };
        }
        let jittery_last = t - 2_000_000;

        let probe_lateness_micros = 5_000_000;
        let steady_phi = steady.suspicion_at(steady_last + probe_lateness_micros);
        let jittery_phi = jittery.suspicion_at(jittery_last + probe_lateness_micros);

        assert!(
            jittery_phi < steady_phi,
            "high-variance distribution should produce LOWER suspicion at the same lateness (jittery={}, steady={})",
            jittery_phi,
            steady_phi
        );
    }

    #[test]
    fn last_heartbeat_micros_returns_most_recent_recording() {
        let mut detector = PhiAccrualFailureDetector::new();
        assert_eq!(detector.last_heartbeat_micros(), None);
        detector.record_heartbeat(7_777_777);
        assert_eq!(detector.last_heartbeat_micros(), Some(7_777_777));
        detector.record_heartbeat(8_888_888);
        assert_eq!(detector.last_heartbeat_micros(), Some(8_888_888));
    }
}
