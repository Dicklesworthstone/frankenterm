//! Property-based tests for quantile_sketch.rs — t-digest streaming quantile estimation.
//!
//! Bead: ft-283h4.20

use frankenterm_core::quantile_sketch::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_values() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(-10000.0..10000.0f64, 1..200)
}

fn arb_small_values() -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(0.0..1000.0f64, 10..100)
}

fn arb_compression() -> impl Strategy<Value = f64> {
    20.0..300.0f64
}

fn arb_quantile() -> impl Strategy<Value = f64> {
    0.0..=1.0f64
}

// ── Count / weight properties ───────────────────────────────────────

proptest! {
    /// Count equals number of inserted values.
    #[test]
    fn count_matches_inserts(values in arb_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        prop_assert_eq!(td.count(), values.len() as f64);
    }

    /// Empty digest has count 0.
    #[test]
    fn empty_count_zero(compression in arb_compression()) {
        let td = TDigest::with_compression(compression);
        prop_assert_eq!(td.count(), 0.0);
        prop_assert!(td.is_empty());
    }

    /// is_empty matches count == 0.
    #[test]
    fn is_empty_consistent(values in arb_values()) {
        let mut td = TDigest::new();
        prop_assert!(td.is_empty());
        for &v in &values {
            td.insert(v);
        }
        prop_assert!(!td.is_empty());
    }
}

// ── Min / max properties ────────────────────────────────────────────

proptest! {
    /// Min tracks the smallest value.
    #[test]
    fn min_is_smallest(values in arb_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let expected_min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        prop_assert_eq!(td.min(), Some(expected_min));
    }

    /// Max tracks the largest value.
    #[test]
    fn max_is_largest(values in arb_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let expected_max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        prop_assert_eq!(td.max(), Some(expected_max));
    }

    /// min <= max.
    #[test]
    fn min_le_max(values in arb_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        if let (Some(min), Some(max)) = (td.min(), td.max()) {
            prop_assert!(min <= max, "min {} > max {}", min, max);
        }
    }
}

// ── Quantile properties ─────────────────────────────────────────────

proptest! {
    /// Quantile(0) returns min.
    #[test]
    fn quantile_zero_is_min(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let q0 = td.quantile(0.0);
        prop_assert_eq!(q0, td.min().unwrap());
    }

    /// Quantile(1) returns max.
    #[test]
    fn quantile_one_is_max(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let q1 = td.quantile(1.0);
        prop_assert_eq!(q1, td.max().unwrap());
    }

    /// Quantile is monotonically non-decreasing.
    #[test]
    fn quantile_monotonic(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let quantiles: Vec<f64> = (0..=20)
            .map(|i| td.quantile(i as f64 / 20.0))
            .collect();
        for i in 1..quantiles.len() {
            prop_assert!(
                quantiles[i] >= quantiles[i - 1],
                "quantile not monotonic at {}: {} < {}", i, quantiles[i], quantiles[i-1]
            );
        }
    }

    /// Quantile is bounded by [min, max].
    #[test]
    fn quantile_within_range(values in arb_small_values(), q in arb_quantile()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let result = td.quantile(q);
        let min = td.min().unwrap();
        let max = td.max().unwrap();
        prop_assert!(
            result >= min && result <= max,
            "quantile({})={} outside [{}, {}]", q, result, min, max
        );
    }

    /// Median of uniform [0, N) is approximately N/2.
    #[test]
    fn median_uniform_approximate(n in 100..1000usize) {
        let mut td = TDigest::new();
        for i in 0..n {
            td.insert(i as f64);
        }
        let median = td.quantile(0.5);
        let expected = (n as f64 - 1.0) / 2.0;
        let tolerance = n as f64 * 0.1; // 10% tolerance
        prop_assert!(
            (median - expected).abs() < tolerance,
            "median {} not near expected {} (tol {})", median, expected, tolerance
        );
    }
}

// ── CDF properties ──────────────────────────────────────────────────

proptest! {
    /// CDF(min) is near 0.
    #[test]
    fn cdf_at_min_near_zero(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let min = td.min().unwrap();
        let cdf = td.cdf(min);
        prop_assert!(
            cdf <= 0.1,
            "cdf(min={})={} should be near 0", min, cdf
        );
    }

    /// CDF(max) is 1.0.
    #[test]
    fn cdf_at_max_is_one(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let max = td.max().unwrap();
        prop_assert_eq!(td.cdf(max), 1.0);
    }

    /// CDF is bounded [0, 1].
    #[test]
    fn cdf_bounded(values in arb_small_values(), x in -20000.0..20000.0f64) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let cdf = td.cdf(x);
        prop_assert!(cdf >= 0.0 && cdf <= 1.0, "cdf({})={} out of [0,1]", x, cdf);
    }

    /// CDF is monotonically non-decreasing.
    #[test]
    fn cdf_monotonic(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let min = td.min().unwrap();
        let max = td.max().unwrap();
        let span = max - min;
        if span > 0.0 {
            let cdfs: Vec<f64> = (0..=20)
                .map(|i| td.cdf(min + (i as f64 / 20.0) * span))
                .collect();
            for i in 1..cdfs.len() {
                prop_assert!(
                    cdfs[i] >= cdfs[i - 1],
                    "cdf not monotonic at {}: {} < {}", i, cdfs[i], cdfs[i-1]
                );
            }
        }
    }
}

// ── Mean properties ─────────────────────────────────────────────────

proptest! {
    /// Mean is within [min, max].
    #[test]
    fn mean_within_range(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let mean = td.mean();
        let min = td.min().unwrap();
        let max = td.max().unwrap();
        prop_assert!(
            mean >= min && mean <= max,
            "mean {} outside [{}, {}]", mean, min, max
        );
    }

    /// Mean approximates the true mean.
    #[test]
    fn mean_approximate(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let td_mean = td.mean();
        let true_mean: f64 = values.iter().sum::<f64>() / values.len() as f64;
        let tolerance = (values.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - values.iter().cloned().fold(f64::INFINITY, f64::min))
            * 0.05;
        prop_assert!(
            (td_mean - true_mean).abs() < tolerance.max(1.0),
            "td mean {} not near true mean {} (tol {})", td_mean, true_mean, tolerance
        );
    }
}

// ── Merge properties ────────────────────────────────────────────────

proptest! {
    /// Merged count equals sum of parts.
    #[test]
    fn merge_count_additive(
        v1 in arb_small_values(),
        v2 in arb_small_values()
    ) {
        let mut td1 = TDigest::new();
        let mut td2 = TDigest::new();
        for &v in &v1 { td1.insert(v); }
        for &v in &v2 { td2.insert(v); }

        let expected = td1.count() + td2.count();
        td1.merge(&td2);
        prop_assert_eq!(td1.count(), expected);
    }

    /// Merged min is min of both.
    #[test]
    fn merge_min_correct(
        v1 in arb_small_values(),
        v2 in arb_small_values()
    ) {
        let mut td1 = TDigest::new();
        let mut td2 = TDigest::new();
        for &v in &v1 { td1.insert(v); }
        for &v in &v2 { td2.insert(v); }

        let expected_min = td1.min().unwrap().min(td2.min().unwrap());
        td1.merge(&td2);
        prop_assert_eq!(td1.min(), Some(expected_min));
    }

    /// Merged max is max of both.
    #[test]
    fn merge_max_correct(
        v1 in arb_small_values(),
        v2 in arb_small_values()
    ) {
        let mut td1 = TDigest::new();
        let mut td2 = TDigest::new();
        for &v in &v1 { td1.insert(v); }
        for &v in &v2 { td2.insert(v); }

        let expected_max = td1.max().unwrap().max(td2.max().unwrap());
        td1.merge(&td2);
        prop_assert_eq!(td1.max(), Some(expected_max));
    }

    /// Merging empty into non-empty preserves state.
    #[test]
    fn merge_empty_noop(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let count_before = td.count();
        let min_before = td.min();
        let max_before = td.max();

        let empty = TDigest::new();
        td.merge(&empty);

        prop_assert_eq!(td.count(), count_before);
        prop_assert_eq!(td.min(), min_before);
        prop_assert_eq!(td.max(), max_before);
    }
}

// ── Clear / reset properties ────────────────────────────────────────

proptest! {
    /// Clear empties the digest.
    #[test]
    fn clear_empties(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        td.clear();
        prop_assert!(td.is_empty());
        prop_assert_eq!(td.count(), 0.0);
        prop_assert_eq!(td.centroid_count(), 0);
        prop_assert_eq!(td.min(), None);
        prop_assert_eq!(td.max(), None);
    }

    /// Reset changes compression and clears.
    #[test]
    fn reset_clears_and_changes_compression(
        values in arb_small_values(),
        new_comp in arb_compression()
    ) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        td.reset(new_comp);
        prop_assert!(td.is_empty());
    }
}

// ── Compression parameter properties ────────────────────────────────

proptest! {
    /// Higher compression = more centroids (for large enough inputs).
    #[test]
    fn higher_compression_more_centroids(n in 500..2000usize) {
        let mut low = TDigest::with_compression(20.0);
        let mut high = TDigest::with_compression(200.0);
        for i in 0..n {
            low.insert(i as f64);
            high.insert(i as f64);
        }
        // Force compression
        let _ = low.quantile(0.5);
        let _ = high.quantile(0.5);
        prop_assert!(
            low.centroid_count() <= high.centroid_count(),
            "low comp ({} centroids) should have <= high comp ({} centroids)",
            low.centroid_count(), high.centroid_count()
        );
    }

    /// Centroid count is bounded by compression parameter.
    #[test]
    fn centroid_count_bounded(values in arb_small_values(), comp in arb_compression()) {
        let mut td = TDigest::with_compression(comp);
        for &v in &values {
            td.insert(v);
        }
        let _ = td.quantile(0.5); // force compress
        // T-digest centroid count is roughly proportional to compression
        // Upper bound is approximately π * δ / 2 + some overhead
        let upper_bound = (std::f64::consts::PI * comp + 100.0) as usize;
        prop_assert!(
            td.centroid_count() <= upper_bound,
            "centroid count {} exceeds bound {} for compression {}", td.centroid_count(), upper_bound, comp
        );
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    /// TDigestConfig serde roundtrip.
    #[test]
    fn config_serde_roundtrip(comp in arb_compression(), buf in 10..2000usize) {
        let config = TDigestConfig { compression: comp, buffer_size: buf };
        let json = serde_json::to_string(&config).unwrap();
        let back: TDigestConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.buffer_size, config.buffer_size);
        prop_assert!((back.compression - config.compression).abs() < 1e-10,
            "compression mismatch: {} vs {}", back.compression, config.compression);
    }

    /// TDigestStats serde roundtrip.
    #[test]
    fn stats_serde_roundtrip(
        centroids in 0..500usize,
        weight in 0.0..10000.0f64,
        buf in 0..500usize,
        comp in arb_compression()
    ) {
        let stats = TDigestStats {
            centroid_count: centroids,
            total_weight: weight,
            buffer_len: buf,
            compression: comp,
            min: Some(0.0),
            max: Some(weight),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: TDigestStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.centroid_count, stats.centroid_count);
        prop_assert_eq!(back.buffer_len, stats.buffer_len);
        prop_assert!((back.compression - stats.compression).abs() < 1e-10,
            "compression mismatch");
        prop_assert!((back.total_weight - stats.total_weight).abs() < 1e-10,
            "weight mismatch");
    }
}

// ── Stats consistency ───────────────────────────────────────────────

proptest! {
    /// Stats fields match direct accessors.
    #[test]
    fn stats_consistent(values in arb_small_values()) {
        let mut td = TDigest::new();
        for &v in &values {
            td.insert(v);
        }
        let stats = td.stats();
        prop_assert_eq!(stats.total_weight, td.count());
        prop_assert_eq!(stats.min, td.min());
        prop_assert_eq!(stats.max, td.max());
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// Insert then clear then insert works correctly.
    #[test]
    fn insert_clear_reinsert(
        v1 in arb_small_values(),
        v2 in arb_small_values()
    ) {
        let mut td = TDigest::new();
        for &v in &v1 { td.insert(v); }
        td.clear();
        for &v in &v2 { td.insert(v); }

        prop_assert_eq!(td.count(), v2.len() as f64);
        let min = v2.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = v2.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        prop_assert_eq!(td.min(), Some(min));
        prop_assert_eq!(td.max(), Some(max));
    }

    /// FromIterator matches sequential inserts.
    #[test]
    fn from_iter_matches_insert(values in arb_small_values()) {
        let mut td1 = TDigest::new();
        for &v in &values {
            td1.insert(v);
        }
        let td2: TDigest = values.iter().cloned().collect();

        prop_assert_eq!(td1.count(), td2.count());
        prop_assert_eq!(td1.min(), td2.min());
        prop_assert_eq!(td1.max(), td2.max());
    }

    /// Weighted insert with weight 1.0 approximates normal insert.
    #[test]
    fn weighted_insert_matches_normal(values in arb_small_values()) {
        let mut td_normal = TDigest::new();
        let mut td_weighted = TDigest::new();
        for &v in &values {
            td_normal.insert(v);
            td_weighted.insert_weighted(v, 1.0);
        }
        prop_assert_eq!(td_normal.count(), td_weighted.count());
        prop_assert_eq!(td_normal.min(), td_weighted.min());
        prop_assert_eq!(td_normal.max(), td_weighted.max());
    }
}
