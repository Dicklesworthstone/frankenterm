// =============================================================================
// Property/contract tests for the telemetry sketch types.
//
// The inline suites cover individual percentile values, convergence, and stats
// serde roundtrips, but not these cross-input invariants:
//   - ExpHistogram::percentile is non-decreasing in p (quantile monotonicity)
//   - count() is monotone non-decreasing under record and exact
//   - merge() is additive in count and never decreases it
//   - Ewma approaches a constant target monotonically and stays bounded by it
//     (convex-combination invariant)
//
// All sketch ops here are synchronous, so this needs no runtime/feature gate
// and proves under `cargo test -p frankenterm-core-telemetry-types`.
// =============================================================================

use frankenterm_core_telemetry_types::ewma::{Ewma, EwmaWithVariance};
use frankenterm_core_telemetry_types::exp_histogram::ExpHistogram;

/// Quantile monotonicity: `percentile(p)` is non-decreasing in `p`. The cdf
/// walk targets `ceil(p * count)`, which is monotone in `p`, so the returned
/// bucket bound can only stay equal or grow — a fundamental quantile invariant
/// the per-value tests don't assert across `p`.
#[test]
fn percentile_is_monotonic_nondecreasing_in_p() {
    let mut h = ExpHistogram::power_of_two(20);
    // A wide spread so the percentiles land across many buckets.
    for v in 1..=1000u32 {
        h.record(f64::from(v));
    }

    let ps = [0.0, 0.05, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99, 1.0];
    let mut prev = f64::NEG_INFINITY;
    for &p in &ps {
        let q = h
            .percentile(p)
            .expect("a populated histogram yields a percentile for every valid p");
        assert!(q.is_finite() && q > 0.0, "percentile({p}) = {q} must be finite/positive");
        assert!(
            q >= prev,
            "percentile must be non-decreasing in p: percentile({p}) = {q} < previous {prev}"
        );
        prev = q;
    }
}

/// An empty histogram has no quantile; a populated one always does — the
/// definedness boundary for the quantile estimator.
#[test]
fn percentile_is_none_only_when_empty() {
    let mut h = ExpHistogram::power_of_two(20);
    assert_eq!(h.percentile(0.5), None, "empty histogram has no percentile");
    h.record(42.0);
    assert!(h.percentile(0.5).is_some(), "a populated histogram yields a percentile");
}

/// `count()` is monotone non-decreasing under `record` and exactly counts the
/// number of recorded values.
#[test]
fn count_is_monotonic_and_exact_under_record() {
    let mut h = ExpHistogram::power_of_two(20);
    let mut prev = h.count();
    assert_eq!(prev, 0, "a fresh histogram has count 0");
    for v in 1..=200u32 {
        h.record(f64::from(v));
        let now = h.count();
        assert!(now >= prev, "count must be non-decreasing under record ({now} < {prev})");
        prev = now;
    }
    assert_eq!(h.count(), 200, "count must equal the number of recorded values");
}

/// `merge` is additive in count and never decreases the receiver's count.
#[test]
fn merge_is_additive_in_count() {
    let mut a = ExpHistogram::power_of_two(20);
    let mut b = ExpHistogram::power_of_two(20);
    for v in 1..=50u32 {
        a.record(f64::from(v));
    }
    for v in 1..=30u32 {
        b.record(f64::from(v));
    }
    let (count_a, count_b) = (a.count(), b.count());
    a.merge(&b);
    assert!(a.count() >= count_a, "merge must not decrease the receiver's count");
    assert_eq!(
        a.count(),
        count_a + count_b,
        "merge must sum the two counts (no values lost)"
    );
}

/// EWMA monotone bounded approach: starting at 0 and repeatedly observing a
/// higher constant target, the smoothed value strictly increases toward the
/// target and stays strictly below it (each step is a convex combination).
#[test]
fn ewma_monotone_bounded_approach_to_constant() {
    let mut e = Ewma::with_half_life_ms(100.0);
    e.observe(0.0, 0); // initialize at 0
    assert_eq!(e.value(), 0.0, "first observation seeds the value");

    let target = 100.0_f64;
    let mut prev = e.value();
    let mut t = 0u64;
    for _ in 0..40 {
        t += 25;
        e.observe(target, t);
        let v = e.value();
        assert!(
            v > prev,
            "ewma must strictly increase toward a higher constant ({v} <= {prev})"
        );
        assert!(
            v < target,
            "ewma is a convex combination and must stay strictly below the target ({v} >= {target})"
        );
        prev = v;
    }
}

/// Regression (NaN-poisoning defect): non-finite observations must be dropped,
/// not fold into the smoothed value. Before the `value.is_finite()` guard in
/// `Ewma::observe`, a single NaN/inf permanently poisoned `value`.
#[test]
fn ewma_ignores_non_finite_observations() {
    let mut e = Ewma::with_half_life_ms(100.0);
    e.observe(10.0, 0);
    let baseline = e.value();
    assert_eq!(baseline, 10.0, "first finite observation seeds the value");

    e.observe(f64::NAN, 25);
    assert_eq!(e.value(), baseline, "a NaN observation must be ignored");
    e.observe(f64::INFINITY, 50);
    assert_eq!(e.value(), baseline, "a +inf observation must be ignored");
    e.observe(f64::NEG_INFINITY, 75);
    assert_eq!(e.value(), baseline, "a -inf observation must be ignored");
    assert!(e.value().is_finite(), "value stays finite after non-finite inputs");

    // A finite observation after the skipped ones still updates normally.
    e.observe(20.0, 100);
    let v = e.value();
    assert!(
        v.is_finite() && v > baseline && v < 20.0,
        "a finite observation after dropped NaNs still moves the ewma ({v})"
    );
}

/// Regression: `EwmaWithVariance` must also drop non-finite inputs before they
/// poison either the mean or the variance (`(value - old_mean)^2`).
#[test]
fn ewma_with_variance_ignores_non_finite_observations() {
    let mut e = EwmaWithVariance::with_half_life_ms(100.0);
    e.observe(10.0, 0);
    e.observe(14.0, 50); // establishes both mean and variance
    let mean_before = e.mean();
    let var_before = e.variance();
    assert!(mean_before.is_finite() && var_before.is_finite());

    e.observe(f64::NAN, 100);
    assert_eq!(e.mean(), mean_before, "NaN must not change the mean");
    assert_eq!(e.variance(), var_before, "NaN must not change the variance");

    e.observe(f64::INFINITY, 150);
    assert!(
        e.mean().is_finite() && e.variance().is_finite(),
        "mean/variance stay finite after non-finite inputs"
    );
}
