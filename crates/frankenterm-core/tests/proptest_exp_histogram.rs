//! Property-based tests for exp_histogram module.
//!
//! Verifies the exponential histogram invariants:
//! - Count conservation: count == sum(buckets) + underflow + overflow
//! - Percentile monotonicity: percentile(p1) <= percentile(p2) if p1 <= p2
//! - Mean within [min, max]
//! - record_n equivalence: record(v) n times ≡ record_n(v, n)
//! - Min/max tracking correctness
//! - Sum accuracy
//! - Clear resets all state
//! - Merge commutativity and count conservation
//! - Stats serde roundtrip
//! - Bucket detail sum matches count

use proptest::prelude::*;

use frankenterm_core::exp_histogram::{ExpHistogram, HistogramStats};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

/// Positive value suitable for histogram recording.
fn arb_positive_value() -> impl Strategy<Value = f64> {
    (1u32..10000).prop_map(|v| v as f64 / 10.0) // 0.1 to 1000.0
}

/// Values including zero and negatives.
fn arb_any_value() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(0.0),
        (-100i32..0).prop_map(|v| v as f64),
        arb_positive_value(),
    ]
}

fn arb_positive_values(max_len: usize) -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(arb_positive_value(), 1..max_len)
}

fn arb_any_values(max_len: usize) -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(arb_any_value(), 1..max_len)
}

/// Base > 1.0 for constructing histograms.
fn arb_base() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(2.0),
        Just(10.0),
        (15u32..50).prop_map(|b| b as f64 / 10.0), // 1.5 to 5.0
    ]
}

/// A compatible pair of (min_exp, max_exp) for histogram construction.
fn arb_exp_range() -> impl Strategy<Value = (i32, i32)> {
    (0i32..5).prop_flat_map(|min_exp| {
        let max_exp_range = (min_exp + 2)..=(min_exp + 20);
        max_exp_range.prop_map(move |max_exp| (min_exp, max_exp))
    })
}

// ────────────────────────────────────────────────────────────────────
// Count conservation: count == sum(buckets) + underflow + overflow
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// The total count equals the sum of all bucket counts plus under/overflow.
    #[test]
    fn prop_count_conservation(
        values in arb_any_values(50),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        let bucket_sum: u64 = h.bucket_details().iter().map(|d| d.count).sum();
        prop_assert_eq!(
            h.count(), bucket_sum,
            "count {} != bucket_details sum {}", h.count(), bucket_sum
        );
    }

    /// After any sequence, count == len(values recorded).
    #[test]
    fn prop_count_matches_insertions(
        values in arb_any_values(50),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }
        prop_assert_eq!(h.count(), values.len() as u64);
    }
}

// ────────────────────────────────────────────────────────────────────
// Percentile monotonicity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// percentile(p1) <= percentile(p2) when p1 <= p2.
    #[test]
    fn prop_percentile_monotonic(
        values in arb_positive_values(30),
        p1_raw in 0u32..100,
        p2_raw in 0u32..100,
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        let (lo, hi) = if p1_raw <= p2_raw {
            (p1_raw as f64 / 100.0, p2_raw as f64 / 100.0)
        } else {
            (p2_raw as f64 / 100.0, p1_raw as f64 / 100.0)
        };

        if let (Some(plo), Some(phi)) = (h.percentile(lo), h.percentile(hi)) {
            prop_assert!(
                plo <= phi + 1e-9,
                "percentile({}) = {} > percentile({}) = {}", lo, plo, hi, phi
            );
        }
    }

    /// p50 <= p90 <= p99 when all defined.
    #[test]
    fn prop_percentile_ordering(
        values in arb_positive_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        if let (Some(p50), Some(p90), Some(p99)) = (h.p50(), h.p90(), h.p99()) {
            prop_assert!(p50 <= p90 + 1e-9, "p50 {} > p90 {}", p50, p90);
            prop_assert!(p90 <= p99 + 1e-9, "p90 {} > p99 {}", p90, p99);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Mean within [min, max]
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// mean() is within [min(), max()] when values are recorded.
    #[test]
    fn prop_mean_within_bounds(
        values in arb_positive_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        if let (Some(mean), Some(min), Some(max)) = (h.mean(), h.min(), h.max()) {
            prop_assert!(
                mean >= min - 1e-9 && mean <= max + 1e-9,
                "mean {} not in [{}, {}]", mean, min, max
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Min/max tracking
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// min() and max() track the actual extremes of recorded values.
    #[test]
    fn prop_min_max_correct(
        values in arb_any_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        let expected_min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let expected_max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        prop_assert_eq!(h.min(), Some(expected_min));
        prop_assert_eq!(h.max(), Some(expected_max));
    }

    /// min() <= max() always.
    #[test]
    fn prop_min_le_max(
        values in arb_any_values(20),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        if let (Some(min), Some(max)) = (h.min(), h.max()) {
            prop_assert!(min <= max, "min {} > max {}", min, max);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Sum accuracy
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// sum() matches manual accumulation of recorded values.
    #[test]
    fn prop_sum_matches_accumulation(
        values in arb_positive_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        let expected_sum: f64 = values.iter().sum();
        for &v in &values {
            h.record(v);
        }

        prop_assert!(
            (h.sum() - expected_sum).abs() < 1e-6,
            "sum {} != expected {}", h.sum(), expected_sum
        );
    }

    /// mean == sum / count.
    #[test]
    fn prop_mean_eq_sum_div_count(
        values in arb_positive_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        if let Some(mean) = h.mean() {
            let expected = h.sum() / h.count() as f64;
            prop_assert!(
                (mean - expected).abs() < 1e-9,
                "mean {} != sum/count {}", mean, expected
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// record_n equivalence
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// record_n(v, n) produces the same result as n calls to record(v).
    #[test]
    fn prop_record_n_equivalence(
        value in arb_positive_value(),
        n in 1u64..20,
    ) {
        let mut h_single = ExpHistogram::power_of_two(20);
        for _ in 0..n {
            h_single.record(value);
        }

        let mut h_batch = ExpHistogram::power_of_two(20);
        h_batch.record_n(value, n);

        prop_assert_eq!(h_single.count(), h_batch.count());
        prop_assert!((h_single.sum() - h_batch.sum()).abs() < 1e-9);
        prop_assert_eq!(h_single.min(), h_batch.min());
        prop_assert_eq!(h_single.max(), h_batch.max());
        prop_assert_eq!(h_single.underflow(), h_batch.underflow());
        prop_assert_eq!(h_single.overflow(), h_batch.overflow());
    }
}

// ────────────────────────────────────────────────────────────────────
// Clear resets
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() resets all state to empty.
    #[test]
    fn prop_clear_resets_all(
        values in arb_any_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        h.clear();

        prop_assert_eq!(h.count(), 0);
        prop_assert!((h.sum() - 0.0).abs() < 1e-9);
        prop_assert_eq!(h.min(), None);
        prop_assert_eq!(h.max(), None);
        prop_assert_eq!(h.mean(), None);
        prop_assert_eq!(h.percentile(0.5), None);
        prop_assert_eq!(h.underflow(), 0);
        prop_assert_eq!(h.overflow(), 0);
    }

    /// After clear, new records work normally.
    #[test]
    fn prop_clear_then_reuse(
        values1 in arb_positive_values(20),
        values2 in arb_positive_values(20),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values1 {
            h.record(v);
        }
        h.clear();
        for &v in &values2 {
            h.record(v);
        }

        prop_assert_eq!(h.count(), values2.len() as u64);
        let expected_sum: f64 = values2.iter().sum();
        prop_assert!((h.sum() - expected_sum).abs() < 1e-6);
    }
}

// ────────────────────────────────────────────────────────────────────
// Merge
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// merge preserves total count: count(A) + count(B) == count(merged).
    #[test]
    fn prop_merge_count_conservation(
        values_a in arb_positive_values(20),
        values_b in arb_positive_values(20),
    ) {
        let mut ha = ExpHistogram::power_of_two(20);
        let mut hb = ExpHistogram::power_of_two(20);

        for &v in &values_a {
            ha.record(v);
        }
        for &v in &values_b {
            hb.record(v);
        }

        let count_a = ha.count();
        let count_b = hb.count();
        ha.merge(&hb);

        prop_assert_eq!(
            ha.count(), count_a + count_b,
            "Merged count {} != {} + {}", ha.count(), count_a, count_b
        );
    }

    /// merge preserves sum: sum(A) + sum(B) == sum(merged).
    #[test]
    fn prop_merge_sum_conservation(
        values_a in arb_positive_values(20),
        values_b in arb_positive_values(20),
    ) {
        let mut ha = ExpHistogram::power_of_two(20);
        let mut hb = ExpHistogram::power_of_two(20);

        for &v in &values_a {
            ha.record(v);
        }
        for &v in &values_b {
            hb.record(v);
        }

        let sum_a = ha.sum();
        let sum_b = hb.sum();
        ha.merge(&hb);

        prop_assert!(
            (ha.sum() - (sum_a + sum_b)).abs() < 1e-6,
            "Merged sum {} != {} + {}", ha.sum(), sum_a, sum_b
        );
    }

    /// merge(A, B) equals recording all values into one histogram.
    #[test]
    fn prop_merge_matches_single_histogram(
        values_a in arb_positive_values(20),
        values_b in arb_positive_values(20),
    ) {
        // Method 1: record everything in one histogram
        let mut h_all = ExpHistogram::power_of_two(20);
        for &v in values_a.iter().chain(values_b.iter()) {
            h_all.record(v);
        }

        // Method 2: record separately and merge
        let mut ha = ExpHistogram::power_of_two(20);
        let mut hb = ExpHistogram::power_of_two(20);
        for &v in &values_a {
            ha.record(v);
        }
        for &v in &values_b {
            hb.record(v);
        }
        ha.merge(&hb);

        prop_assert_eq!(ha.count(), h_all.count());
        prop_assert!((ha.sum() - h_all.sum()).abs() < 1e-6);
        prop_assert_eq!(ha.min(), h_all.min());
        prop_assert_eq!(ha.max(), h_all.max());
        prop_assert_eq!(ha.underflow(), h_all.underflow());
        prop_assert_eq!(ha.overflow(), h_all.overflow());
    }

    /// Merging with an empty histogram is identity.
    #[test]
    fn prop_merge_empty_is_identity(
        values in arb_positive_values(20),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        let count_before = h.count();
        let sum_before = h.sum();
        let empty = ExpHistogram::power_of_two(20);
        h.merge(&empty);

        prop_assert_eq!(h.count(), count_before);
        prop_assert!((h.sum() - sum_before).abs() < 1e-9);
    }
}

// ────────────────────────────────────────────────────────────────────
// Stats serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// HistogramStats JSON roundtrip preserves all fields.
    #[test]
    fn prop_stats_serde_roundtrip(
        values in arb_positive_values(20),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        let stats = h.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: HistogramStats = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(stats.count, back.count);
        prop_assert!((stats.sum - back.sum).abs() < 1e-9);
        prop_assert_eq!(stats.underflow, back.underflow);
        prop_assert_eq!(stats.overflow, back.overflow);
        prop_assert_eq!(stats.num_buckets, back.num_buckets);

        // Optional fields
        match (stats.min, back.min) {
            (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-9),
            (None, None) => {}
            _ => prop_assert!(false, "min mismatch"),
        }
        match (stats.max, back.max) {
            (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-9),
            (None, None) => {}
            _ => prop_assert!(false, "max mismatch"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Empty histogram
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty histogram returns None for all optional accessors.
    #[test]
    fn prop_empty_returns_none(
        base in arb_base(),
        (min_exp, max_exp) in arb_exp_range(),
        p in 0.0f64..=1.0,
    ) {
        let h = ExpHistogram::new(base, min_exp, max_exp);
        prop_assert_eq!(h.count(), 0);
        prop_assert_eq!(h.percentile(p), None);
        prop_assert_eq!(h.mean(), None);
        prop_assert_eq!(h.min(), None);
        prop_assert_eq!(h.max(), None);
    }
}

// ────────────────────────────────────────────────────────────────────
// Custom base histograms
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Count conservation holds for any valid base and exp range.
    #[test]
    fn prop_custom_base_count_conservation(
        base in arb_base(),
        (min_exp, max_exp) in arb_exp_range(),
        values in arb_any_values(20),
    ) {
        let mut h = ExpHistogram::new(base, min_exp, max_exp);
        for &v in &values {
            h.record(v);
        }

        prop_assert_eq!(h.count(), values.len() as u64);

        // underflow + bucket_counts + overflow == count
        let detail_sum: u64 = h.bucket_details().iter().map(|d| d.count).sum();
        prop_assert_eq!(
            h.count(), detail_sum,
            "count {} != detail_sum {} (base={}, range=[{}, {}))",
            h.count(), detail_sum, base, min_exp, max_exp
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Underflow/overflow classification
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Zero and negative values always go to underflow.
    #[test]
    fn prop_non_positive_goes_to_underflow(
        neg_val in (-1000i32..=0).prop_map(|v| v as f64),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        h.record(neg_val);
        prop_assert_eq!(h.underflow(), 1, "Non-positive {} not in underflow", neg_val);
    }

    /// Values above max boundary go to overflow.
    #[test]
    fn prop_large_values_overflow(
        max_exp in 3i32..10,
        multiplier in 2u32..100,
    ) {
        let mut h = ExpHistogram::power_of_two(max_exp);
        let boundary = 2.0f64.powi(max_exp);
        let value = boundary * multiplier as f64;
        h.record(value);
        prop_assert_eq!(h.overflow(), 1, "Value {} not in overflow (boundary={})", value, boundary);
    }
}

// ────────────────────────────────────────────────────────────────────
// Bucket boundaries
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Bucket detail boundaries are monotonically increasing.
    #[test]
    fn prop_bucket_boundaries_monotonic(
        values in arb_positive_values(30),
    ) {
        let mut h = ExpHistogram::power_of_two(20);
        for &v in &values {
            h.record(v);
        }

        let details = h.bucket_details();
        for window in details.windows(2) {
            prop_assert!(
                window[0].upper <= window[1].lower + 1e-9,
                "Bucket boundary gap: [{}, {}) then [{}, {})",
                window[0].lower, window[0].upper,
                window[1].lower, window[1].upper
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// num_buckets matches construction
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// num_buckets() == max_exp - min_exp.
    #[test]
    fn prop_num_buckets_matches_params(
        base in arb_base(),
        (min_exp, max_exp) in arb_exp_range(),
    ) {
        let h = ExpHistogram::new(base, min_exp, max_exp);
        prop_assert_eq!(
            h.num_buckets(), (max_exp - min_exp) as usize,
            "num_buckets {} != max_exp({}) - min_exp({})", h.num_buckets(), max_exp, min_exp
        );
    }
}
