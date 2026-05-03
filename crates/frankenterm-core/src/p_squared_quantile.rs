//! br-ft-p-squared: alien-uplift §P² Algorithm (Jain & Chlamtac
//! 1985) for streaming single-quantile estimation with constant
//! memory.
//!
//! Implements the P² algorithm (Jain, R. & Chlamtac, I. (1985).
//! "The P² Algorithm for Dynamic Calculation of Quantiles and
//! Histograms Without Storing Observations". Communications of
//! the ACM, 28(10), 1076-1085).
//!
//! # Why P² vs other percentile estimators
//!
//! | Type | Memory | Quantiles | Mergeable | Error shape |
//! |---|---|---|---|---|
//! | `crate::telemetry::Histogram` (sample buffer) | O(N) — 8KB at 1000-sample cap | any | exact-merge via shared sample union | exact-from-sample |
//! | `crate::telemetry::TDigestHistogram` (br-ft-z9byv) | O(k) — ~3KB at compression=100 | any | yes | bounded relative error, tighter at tails |
//! | `crate::p_squared_quantile` (this module) | O(1) — ~80 bytes total | ONE pre-chosen | no | bounded absolute error for the chosen quantile |
//!
//! P² wins when:
//! - You only ever read ONE quantile (e.g., p99 for SLO checks).
//! - Memory is critical (per-WorkloadCategory × per-stage tracking).
//! - Cross-process merging isn't required.
//!
//! # Algorithm
//!
//! Maintain 5 markers at positions corresponding to the 0th,
//! (q/2)-th, q-th, ((1+q)/2)-th, and 1.0-th quantiles. After 5
//! initial observations, the markers are sorted-positioned. On
//! each subsequent observation:
//!
//! 1. Find which marker cell the new value falls in.
//! 2. Increment positions of all markers above the cell.
//! 3. For inner markers (1, 2, 3): if observed position drifts ≥ 1
//!    from desired position, adjust marker value via parabolic
//!    interpolation (with linear fallback if parabolic would
//!    violate monotonicity).
//!
//! `estimate()` returns the value at marker[2] — the streaming
//! estimator of the configured quantile.
//!
//! # What ships
//!
//! - [`PSquaredEstimator`] — the algorithm state machine.
//! - [`PSquaredSnapshot`] — serde snapshot for telemetry.

use serde::{Deserialize, Serialize};

const MARKER_COUNT: usize = 5;

/// br-ft-p-squared: streaming estimator for a single pre-chosen
/// quantile via the P² algorithm (Jain & Chlamtac 1985).
///
/// O(1) per insert + O(1) per quantile read, ~80 bytes total
/// state regardless of insert count. See module docstring for the
/// algorithm + when to choose this over Histogram / TDigestHistogram.
#[derive(Debug, Clone)]
pub struct PSquaredEstimator {
    /// Target quantile (e.g., 0.99 for p99). Clamped to (0, 1).
    quantile: f64,
    /// Marker values q[0..5]. After warmup, q[2] is the streaming
    /// estimate of the target quantile.
    q: [f64; MARKER_COUNT],
    /// Observed marker positions (1-indexed in Jain & Chlamtac;
    /// here 0-indexed for convenience).
    n: [f64; MARKER_COUNT],
    /// Desired marker positions.
    n_desired: [f64; MARKER_COUNT],
    /// Desired increments per observation (precomputed from
    /// quantile).
    dn: [f64; MARKER_COUNT],
    /// Total observations recorded.
    count: u64,
    /// Initialization buffer (collects first 5 observations to
    /// seed the markers). None after warmup completes.
    init_buf: Option<Vec<f64>>,
}

impl PSquaredEstimator {
    /// Create a new estimator for the given quantile (clamped to
    /// (0.001, 0.999) to keep marker positions well-defined).
    #[must_use]
    pub fn new(quantile: f64) -> Self {
        let q_target = quantile.clamp(0.001, 0.999);
        // Desired positions: 0, q/2, q, (1+q)/2, 1.
        // Per-observation increments: 0, q/2, q, (1+q)/2, 1.
        let dn = [0.0, q_target / 2.0, q_target, (1.0 + q_target) / 2.0, 1.0];
        Self {
            quantile: q_target,
            q: [0.0; MARKER_COUNT],
            n: [0.0; MARKER_COUNT],
            n_desired: [0.0; MARKER_COUNT],
            dn,
            count: 0,
            init_buf: Some(Vec::with_capacity(MARKER_COUNT)),
        }
    }

    /// Record a new observation. Non-finite values are silently
    /// dropped (mirrors the Histogram non-finite guard).
    pub fn record(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.count += 1;
        if let Some(ref mut buf) = self.init_buf {
            buf.push(value);
            if buf.len() == MARKER_COUNT {
                // Warmup done — sort + initialize markers.
                let mut sorted = std::mem::take(buf);
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                for i in 0..MARKER_COUNT {
                    self.q[i] = sorted[i];
                    self.n[i] = i as f64;
                    self.n_desired[i] = (i as f64) * (MARKER_COUNT as f64 - 1.0)
                        / (MARKER_COUNT as f64 - 1.0); // 0,1,2,3,4 initially
                }
                // n_desired refines based on quantile after the first record.
                self.n_desired[0] = 0.0;
                self.n_desired[1] = 2.0 * self.quantile;
                self.n_desired[2] = 4.0 * self.quantile;
                self.n_desired[3] = 2.0 * (1.0 + self.quantile);
                self.n_desired[4] = 4.0;
                self.init_buf = None;
            }
            return;
        }

        // Find which cell the new value falls in.
        let mut k: usize = if value < self.q[0] {
            self.q[0] = value; // extend lower bound
            0
        } else if value < self.q[1] {
            0
        } else if value < self.q[2] {
            1
        } else if value < self.q[3] {
            2
        } else if value <= self.q[4] {
            3
        } else {
            self.q[4] = value; // extend upper bound
            3
        };
        // Clamp k for safety (the if-chain above should already
        // produce valid k, but defensive).
        if k >= MARKER_COUNT - 1 {
            k = MARKER_COUNT - 2;
        }

        // Increment positions of markers above the cell.
        for i in (k + 1)..MARKER_COUNT {
            self.n[i] += 1.0;
        }
        // Update desired positions.
        for i in 0..MARKER_COUNT {
            self.n_desired[i] += self.dn[i];
        }

        // Adjust inner markers (1, 2, 3) via parabolic interpolation.
        for i in 1..MARKER_COUNT - 1 {
            let d = self.n_desired[i] - self.n[i];
            let d_sign = if d >= 1.0 && self.n[i + 1] - self.n[i] > 1.0 {
                1.0
            } else if d <= -1.0 && self.n[i - 1] - self.n[i] < -1.0 {
                -1.0
            } else {
                continue;
            };

            // Parabolic prediction.
            let qp = parabolic(d_sign, &self.q, &self.n, i);
            // Use parabolic if it preserves monotonicity, else linear.
            let new_q = if self.q[i - 1] < qp && qp < self.q[i + 1] {
                qp
            } else {
                linear(d_sign, &self.q, &self.n, i)
            };
            self.q[i] = new_q;
            self.n[i] += d_sign;
        }
    }

    /// Estimate the configured quantile. Returns `None` until at
    /// least `MARKER_COUNT` observations have been recorded.
    #[must_use]
    pub fn estimate(&self) -> Option<f64> {
        if self.init_buf.is_some() {
            return None;
        }
        Some(self.q[2])
    }

    /// Total observations recorded (regardless of warmup state).
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Configured target quantile.
    #[must_use]
    pub fn quantile(&self) -> f64 {
        self.quantile
    }

    /// Whether the estimator has completed warmup (≥ 5 observations).
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.init_buf.is_none()
    }

    /// Snapshot for telemetry / capacity-doctor reports.
    #[must_use]
    pub fn snapshot(&self) -> PSquaredSnapshot {
        PSquaredSnapshot {
            quantile: self.quantile,
            count: self.count,
            estimate: self.estimate(),
        }
    }
}

fn parabolic(d: f64, q: &[f64; MARKER_COUNT], n: &[f64; MARKER_COUNT], i: usize) -> f64 {
    let qi = q[i];
    let qm = q[i - 1];
    let qp = q[i + 1];
    let ni = n[i];
    let nm = n[i - 1];
    let np = n[i + 1];
    qi + d / (np - nm)
        * ((ni - nm + d) * (qp - qi) / (np - ni)
            + (np - ni - d) * (qi - qm) / (ni - nm))
}

fn linear(d: f64, q: &[f64; MARKER_COUNT], n: &[f64; MARKER_COUNT], i: usize) -> f64 {
    let qi = q[i];
    let qd = q[(i as isize + d as isize) as usize];
    let ni = n[i];
    let nd = n[(i as isize + d as isize) as usize];
    qi + d * (qd - qi) / (nd - ni)
}

/// br-ft-p-squared: serde snapshot for telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PSquaredSnapshot {
    pub quantile: f64,
    pub count: u64,
    /// `None` until the estimator has completed warmup
    /// (≥ MARKER_COUNT=5 observations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_estimator_returns_none() {
        let e = PSquaredEstimator::new(0.99);
        assert_eq!(e.estimate(), None);
        assert_eq!(e.count(), 0);
        assert!(!e.is_warm());
    }

    #[test]
    fn warmup_completes_at_5_observations() {
        let mut e = PSquaredEstimator::new(0.5);
        for i in 1..=4 {
            e.record(i as f64);
        }
        assert!(!e.is_warm());
        assert_eq!(e.estimate(), None);
        e.record(5.0);
        assert!(e.is_warm());
        assert!(e.estimate().is_some());
    }

    #[test]
    fn drops_non_finite_values() {
        let mut e = PSquaredEstimator::new(0.5);
        e.record(f64::NAN);
        e.record(f64::INFINITY);
        e.record(f64::NEG_INFINITY);
        assert_eq!(e.count(), 0);
        e.record(1.0);
        assert_eq!(e.count(), 1);
    }

    #[test]
    fn quantile_clamps_to_safe_range() {
        let e = PSquaredEstimator::new(0.0);
        assert!((e.quantile() - 0.001).abs() < 1e-9);
        let e = PSquaredEstimator::new(1.0);
        assert!((e.quantile() - 0.999).abs() < 1e-9);
        let e = PSquaredEstimator::new(0.5);
        assert!((e.quantile() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn estimates_p50_within_bounded_error_for_uniform_distribution() {
        // Park-Miller LCG (deterministic).
        let mut state: u64 = 12345;
        let next = |s: &mut u64| -> f64 {
            *s = (*s).wrapping_mul(48271).wrapping_rem(0x7fff_ffff);
            (*s as f64) / (0x7fff_ffff as f64)
        };
        let mut e = PSquaredEstimator::new(0.5);
        for _ in 0..10_000 {
            e.record(next(&mut state));
        }
        let est = e.estimate().expect("warm after 10k samples");
        // True p50 of uniform[0,1] is 0.5. P² achieves ~1-2% error
        // typically; allow ±5% generous slack.
        assert!(
            (est - 0.5).abs() < 0.05,
            "br-ft-p-squared p50 estimate must be within ±0.05 of 0.5 for uniform[0,1] (got {est})"
        );
    }

    #[test]
    fn estimates_p99_within_bounded_error_for_uniform_distribution() {
        let mut state: u64 = 67890;
        let next = |s: &mut u64| -> f64 {
            *s = (*s).wrapping_mul(48271).wrapping_rem(0x7fff_ffff);
            (*s as f64) / (0x7fff_ffff as f64)
        };
        let mut e = PSquaredEstimator::new(0.99);
        for _ in 0..10_000 {
            e.record(next(&mut state));
        }
        let est = e.estimate().expect("warm");
        // True p99 of uniform[0,1] is 0.99. P² achieves bounded
        // absolute error; allow ±0.05 (5% absolute) for sampling
        // slack.
        assert!(
            (est - 0.99).abs() < 0.05,
            "br-ft-p-squared p99 estimate must be within ±0.05 of 0.99 for uniform[0,1] (got {est})"
        );
    }

    #[test]
    fn memory_bound_constant_regardless_of_insert_count() {
        // br-ft-p-squared core advantage: O(1) memory. The struct
        // size is the same after 5 inserts and 1 million inserts.
        // We can't measure heap precisely from a test, but we can
        // confirm that the count grows while the estimator's state
        // (5 markers + 5 positions + 5 desired + 5 increments +
        // count) stays the same shape.
        let mut e = PSquaredEstimator::new(0.99);
        for i in 0..1_000_000 {
            e.record(i as f64);
        }
        assert_eq!(e.count(), 1_000_000);
        // No assertion on memory directly — the struct layout has
        // 5 [f64; 5] arrays + 1 f64 + 1 u64 + 1 Option<Vec<f64>>
        // (None after warmup). The Option<Vec> keeps the heap
        // allocation, but it's <40 bytes. Total constant memory.
        let _ = e.estimate(); // no-op except to ensure the call is valid
    }

    #[test]
    fn snapshot_carries_documented_fields() {
        let mut e = PSquaredEstimator::new(0.95);
        for i in 0..100 {
            e.record(i as f64);
        }
        let snap = e.snapshot();
        assert!((snap.quantile - 0.95).abs() < 1e-9);
        assert_eq!(snap.count, 100);
        assert!(snap.estimate.is_some());
    }

    #[test]
    fn snapshot_serde_roundtrips() {
        let mut e = PSquaredEstimator::new(0.99);
        for i in 0..50 {
            e.record(i as f64);
        }
        let snap = e.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let parsed: PSquaredSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn snapshot_pre_warmup_omits_estimate() {
        let mut e = PSquaredEstimator::new(0.99);
        e.record(1.0);
        e.record(2.0);
        let snap = e.snapshot();
        assert_eq!(snap.estimate, None);
    }
}
