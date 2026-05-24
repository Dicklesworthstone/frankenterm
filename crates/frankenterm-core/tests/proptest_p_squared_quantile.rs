//! Property-based tests for the P² streaming quantile estimator
//! ([`frankenterm_core::p_squared_quantile::PSquaredEstimator`]).
//!
//! The estimator had example-based unit tests but no property coverage.
//! These pin its structural invariants:
//!
//! 1. **Finite-count** — `count()` equals the number of finite records;
//!    non-finite (NaN, ±Inf) values are silently dropped.
//! 2. **Warmup threshold** — `estimate()` is `None` and `is_warm()` is
//!    false until 5 observations, then `Some` / true.
//! 3. **Range containment** — once warm, the estimate lies within the
//!    `[min, max]` of all recorded values (markers q[0]/q[4] track the
//!    true extremes and the parabolic update preserves monotonicity).
//! 4. **Constant stream** — a stream of a single repeated value estimates
//!    that value exactly.
//! 5. **Non-finite invariance** — interleaving NaN/±Inf among finite
//!    values changes neither the count nor the estimate.

use proptest::prelude::*;

use frankenterm_core::p_squared_quantile::PSquaredEstimator;

fn arb_value() -> impl Strategy<Value = f64> {
    -10_000.0_f64..10_000.0
}

fn arb_quantile() -> impl Strategy<Value = f64> {
    0.01_f64..0.99
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// count() equals the number of finite records; non-finite dropped.
    #[test]
    fn count_tracks_finite_inserts_only(
        q in arb_quantile(),
        values in proptest::collection::vec(arb_value(), 0..60),
    ) {
        let mut est = PSquaredEstimator::new(q);
        let pollutants = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for (i, &v) in values.iter().enumerate() {
            est.record(v);
            est.record(pollutants[i % pollutants.len()]); // dropped
        }
        prop_assert_eq!(est.count(), values.len() as u64,
            "count must equal finite inserts, ignoring non-finite");
    }

    /// estimate()/is_warm() flip exactly at the 5-observation threshold.
    #[test]
    fn estimate_warm_threshold(
        q in arb_quantile(),
        values in proptest::collection::vec(arb_value(), 0..12),
    ) {
        let mut est = PSquaredEstimator::new(q);
        for &v in &values {
            est.record(v);
        }
        if values.len() < 5 {
            prop_assert!(!est.is_warm(), "must not be warm before 5 observations");
            prop_assert!(est.estimate().is_none(), "estimate must be None before warmup");
        } else {
            prop_assert!(est.is_warm(), "must be warm at >= 5 observations");
            prop_assert!(est.estimate().is_some(), "estimate must be Some after warmup");
        }
    }

    /// Once warm, the estimate is contained in [min, max] of all records.
    #[test]
    fn estimate_within_observed_range(
        q in arb_quantile(),
        values in proptest::collection::vec(arb_value(), 5..60),
    ) {
        let mut est = PSquaredEstimator::new(q);
        for &v in &values {
            est.record(v);
        }
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let e = est.estimate().expect("warm estimator must produce an estimate");
        // Tiny epsilon absorbs float noise in the parabolic interpolation.
        prop_assert!(e >= min - 1e-6, "estimate {} below observed min {}", e, min);
        prop_assert!(e <= max + 1e-6, "estimate {} above observed max {}", e, max);
    }

    /// A constant stream estimates the constant exactly (min == max == c).
    #[test]
    fn constant_stream_estimates_constant(
        q in arb_quantile(),
        c in arb_value(),
        n in 5usize..40,
    ) {
        let mut est = PSquaredEstimator::new(q);
        for _ in 0..n {
            est.record(c);
        }
        let e = est.estimate().expect("warm estimator must produce an estimate");
        prop_assert!((e - c).abs() <= 1e-6,
            "constant stream of {} must estimate {}, got {}", c, c, e);
    }

    /// Interleaving non-finite values among finite ones changes neither
    /// the count nor the estimate (non-finite is dropped before any state
    /// mutation, so the finite subsequence — and its order — is identical).
    #[test]
    fn non_finite_does_not_change_state(
        q in arb_quantile(),
        values in proptest::collection::vec(arb_value(), 5..50),
    ) {
        let mut clean = PSquaredEstimator::new(q);
        let mut dirty = PSquaredEstimator::new(q);
        let pollutants = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for (i, &v) in values.iter().enumerate() {
            clean.record(v);
            dirty.record(v);
            dirty.record(pollutants[i % pollutants.len()]);
        }
        prop_assert_eq!(clean.count(), dirty.count(),
            "non-finite must not change count");
        prop_assert_eq!(clean.estimate(), dirty.estimate(),
            "non-finite must not change the estimate");
    }
}
