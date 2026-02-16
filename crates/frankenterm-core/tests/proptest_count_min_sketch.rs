//! Property-based tests for count_min_sketch.rs — Count-Min Sketch frequency estimation.
//!
//! Verifies the Count-Min Sketch invariants:
//! - Estimate upper bound: estimate(x) >= true_count(x) always
//! - Total count consistency: total_count == sum of all add() counts
//! - Merge commutativity: merge(a, b) == merge(b, a) for estimates
//! - Merge additivity: merged total_count == sum of parts
//! - Clear idempotence: clear then estimate == 0 for any item
//! - Inner product symmetry: ip(a, b) == ip(b, a)
//! - Dimension mismatch rejection: merge/inner_product fail on mismatched sizes
//! - Config roundtrip: serde preserves CmsConfig
//! - Stats roundtrip: serde preserves CmsStats
//! - Monotonic estimate: more add() calls never decrease estimate
//! - Clone equivalence: cloned sketch gives identical estimates
//! - Memory bound: memory_bytes == width * depth * 8
//! - Error bound consistency: epsilon and delta are positive and bounded
//!
//! Bead: ft-283h4.22

use frankenterm_core::count_min_sketch::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_width() -> impl Strategy<Value = usize> {
    4usize..=256
}

fn arb_depth() -> impl Strategy<Value = usize> {
    1usize..=8
}

fn arb_config() -> impl Strategy<Value = CmsConfig> {
    (arb_width(), arb_depth()).prop_map(|(w, d)| CmsConfig { width: w, depth: d })
}

fn arb_items(max_len: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..1000, 1..max_len)
}

fn arb_count() -> impl Strategy<Value = u64> {
    1u64..=100
}

fn arb_small_sketch() -> impl Strategy<Value = (CmsConfig, Vec<(u64, u64)>)> {
    arb_config().prop_flat_map(|config| {
        let entries = prop::collection::vec((0u64..500, arb_count()), 0..50);
        entries.prop_map(move |e| (config.clone(), e))
    })
}

fn build_sketch(config: &CmsConfig, entries: &[(u64, u64)]) -> CountMinSketch {
    let mut cms = CountMinSketch::with_config(config.clone());
    for &(ref item, count) in entries {
        cms.add(item, count);
    }
    cms
}

// ── Estimate upper bound ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// estimate(item) >= true_count(item) for every item.
    #[test]
    fn prop_estimate_upper_bound(
        width in arb_width(),
        depth in arb_depth(),
        items in arb_items(100),
    ) {
        let mut cms = CountMinSketch::with_dimensions(width, depth);
        let mut true_counts = std::collections::HashMap::new();
        for &item in &items {
            cms.increment(&item);
            *true_counts.entry(item).or_insert(0u64) += 1;
        }
        for (&item, &true_count) in &true_counts {
            let est = cms.estimate(&item);
            prop_assert!(
                est >= true_count,
                "estimate {} < true {} for item {}", est, true_count, item
            );
        }
    }

    /// estimate(item) >= true_count(item) when using add with arbitrary counts.
    #[test]
    fn prop_estimate_upper_bound_add(
        width in arb_width(),
        depth in arb_depth(),
        entries in prop::collection::vec((0u64..500, arb_count()), 1..50),
    ) {
        let mut cms = CountMinSketch::with_dimensions(width, depth);
        let mut true_counts: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        for &(item, count) in &entries {
            cms.add(&item, count);
            *true_counts.entry(item).or_insert(0) += count;
        }
        for (&item, &true_count) in &true_counts {
            let est = cms.estimate(&item);
            prop_assert!(
                est >= true_count,
                "estimate {} < true {} for item {}", est, true_count, item
            );
        }
    }
}

// ── Total count consistency ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// total_count equals the sum of all add() count arguments.
    #[test]
    fn prop_total_count_sum(
        entries in prop::collection::vec((0u64..1000, arb_count()), 0..100),
    ) {
        let mut cms = CountMinSketch::new();
        let mut expected_total: u64 = 0;
        for &(ref item, count) in &entries {
            cms.add(item, count);
            expected_total = expected_total.saturating_add(count);
        }
        prop_assert_eq!(cms.total_count(), expected_total);
    }

    /// total_count matches number of increment() calls.
    #[test]
    fn prop_total_count_increments(
        items in arb_items(200),
    ) {
        let mut cms = CountMinSketch::new();
        for item in &items {
            cms.increment(item);
        }
        prop_assert_eq!(cms.total_count(), items.len() as u64);
    }
}

// ── Merge properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Merging two sketches preserves the upper bound property.
    #[test]
    fn prop_merge_preserves_upper_bound(
        config in arb_config(),
        entries_a in prop::collection::vec((0u64..200, arb_count()), 0..30),
        entries_b in prop::collection::vec((0u64..200, arb_count()), 0..30),
    ) {
        let mut cms_a = build_sketch(&config, &entries_a);
        let cms_b = build_sketch(&config, &entries_b);

        // Compute true counts from both
        let mut true_counts: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
        for &(item, count) in entries_a.iter().chain(entries_b.iter()) {
            *true_counts.entry(item).or_insert(0) += count;
        }

        cms_a.merge(&cms_b).unwrap();

        for (&item, &true_count) in &true_counts {
            let est = cms_a.estimate(&item);
            prop_assert!(
                est >= true_count,
                "merged estimate {} < true {} for item {}", est, true_count, item
            );
        }
    }

    /// Merged total_count equals sum of both sketches' total_counts.
    #[test]
    fn prop_merge_total_count_additive(
        config in arb_config(),
        entries_a in prop::collection::vec((0u64..500, arb_count()), 0..30),
        entries_b in prop::collection::vec((0u64..500, arb_count()), 0..30),
    ) {
        let mut cms_a = build_sketch(&config, &entries_a);
        let cms_b = build_sketch(&config, &entries_b);
        let expected = cms_a.total_count().saturating_add(cms_b.total_count());
        cms_a.merge(&cms_b).unwrap();
        prop_assert_eq!(cms_a.total_count(), expected);
    }

    /// Merge is commutative: estimate after merge(a,b) == estimate after merge(b,a).
    #[test]
    fn prop_merge_commutative(
        config in arb_config(),
        entries_a in prop::collection::vec((0u64..100, arb_count()), 0..20),
        entries_b in prop::collection::vec((0u64..100, arb_count()), 0..20),
        query_items in prop::collection::vec(0u64..150, 1..20),
    ) {
        let mut ab = build_sketch(&config, &entries_a);
        let b = build_sketch(&config, &entries_b);
        ab.merge(&b).unwrap();

        let mut ba = build_sketch(&config, &entries_b);
        let a = build_sketch(&config, &entries_a);
        ba.merge(&a).unwrap();

        for &item in &query_items {
            prop_assert_eq!(
                ab.estimate(&item), ba.estimate(&item),
                "merge commutativity violated for item {}", item
            );
        }
    }

    /// Dimension mismatch always returns Err.
    #[test]
    fn prop_merge_dimension_mismatch(
        w1 in 4usize..=128,
        w2 in 4usize..=128,
        d1 in 1usize..=8,
        d2 in 1usize..=8,
    ) {
        prop_assume!(w1 != w2 || d1 != d2);
        let mut cms1 = CountMinSketch::with_dimensions(w1, d1);
        let cms2 = CountMinSketch::with_dimensions(w2, d2);
        let result = cms1.merge(&cms2);
        prop_assert!(result.is_err());
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After clear(), total_count == 0 and is_empty() == true.
    #[test]
    fn prop_clear_resets_state(
        entries in prop::collection::vec((0u64..500, arb_count()), 1..50),
    ) {
        let mut cms = CountMinSketch::new();
        for &(ref item, count) in &entries {
            cms.add(item, count);
        }
        cms.clear();
        prop_assert!(cms.is_empty());
        prop_assert_eq!(cms.total_count(), 0);
    }

    /// After clear(), estimate for any previously inserted item is 0.
    #[test]
    fn prop_clear_zeroes_estimates(
        entries in prop::collection::vec((0u64..500, arb_count()), 1..50),
    ) {
        let mut cms = CountMinSketch::new();
        let items: Vec<u64> = entries.iter().map(|&(item, _)| item).collect();
        for &(ref item, count) in &entries {
            cms.add(item, count);
        }
        cms.clear();
        for item in &items {
            prop_assert_eq!(cms.estimate(item), 0, "item {} should be 0 after clear", item);
        }
    }
}

// ── Inner product properties ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inner product is symmetric: ip(a, b) == ip(b, a).
    #[test]
    fn prop_inner_product_symmetric(
        config in arb_config(),
        entries_a in prop::collection::vec((0u64..100, arb_count()), 0..20),
        entries_b in prop::collection::vec((0u64..100, arb_count()), 0..20),
    ) {
        let a = build_sketch(&config, &entries_a);
        let b = build_sketch(&config, &entries_b);
        let ip_ab = a.inner_product(&b);
        let ip_ba = b.inner_product(&a);
        prop_assert_eq!(ip_ab, ip_ba);
    }

    /// Inner product with self is non-negative.
    #[test]
    fn prop_inner_product_self_nonnegative(
        (config, entries) in arb_small_sketch(),
    ) {
        let cms = build_sketch(&config, &entries);
        if let Some(ip) = cms.inner_product(&cms) {
            // u64 is always non-negative, but check it's reasonable
            if !cms.is_empty() {
                prop_assert!(ip > 0, "non-empty self inner product should be > 0");
            }
        }
    }

    /// Inner product dimension mismatch returns None.
    #[test]
    fn prop_inner_product_dimension_mismatch(
        w1 in 4usize..=128,
        w2 in 4usize..=128,
        d1 in 1usize..=8,
        d2 in 1usize..=8,
    ) {
        prop_assume!(w1 != w2 || d1 != d2);
        let cms1 = CountMinSketch::with_dimensions(w1, d1);
        let cms2 = CountMinSketch::with_dimensions(w2, d2);
        prop_assert!(cms1.inner_product(&cms2).is_none());
    }
}

// ── Monotonicity ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Estimate never decreases as more copies of the same item are added.
    #[test]
    fn prop_estimate_monotonic(
        width in arb_width(),
        depth in arb_depth(),
        counts in prop::collection::vec(arb_count(), 2..20),
    ) {
        let mut cms = CountMinSketch::with_dimensions(width, depth);
        let item = 42u64;
        let mut prev_est = 0u64;
        for &count in &counts {
            cms.add(&item, count);
            let est = cms.estimate(&item);
            prop_assert!(
                est >= prev_est,
                "estimate decreased from {} to {} after adding {}", prev_est, est, count
            );
            prev_est = est;
        }
    }
}

// ── Clone equivalence ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Cloned sketch gives identical estimates for all queried items.
    #[test]
    fn prop_clone_equivalence(
        (config, entries) in arb_small_sketch(),
        query_items in prop::collection::vec(0u64..600, 1..30),
    ) {
        let cms = build_sketch(&config, &entries);
        let cloned = cms.clone();
        prop_assert_eq!(cms.total_count(), cloned.total_count());
        prop_assert_eq!(cms.width(), cloned.width());
        prop_assert_eq!(cms.depth(), cloned.depth());
        for item in &query_items {
            prop_assert_eq!(
                cms.estimate(item), cloned.estimate(item),
                "clone diverged for item {}", item
            );
        }
    }

    /// Clone is independent: mutations on clone don't affect original.
    #[test]
    fn prop_clone_independence(
        (config, entries) in arb_small_sketch(),
    ) {
        let cms = build_sketch(&config, &entries);
        let original_total = cms.total_count();
        let mut cloned = cms.clone();
        cloned.add(&999u64, 100);
        prop_assert_eq!(cms.total_count(), original_total);
    }
}

// ── Memory and error bounds ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// memory_bytes == width * depth * sizeof(u64).
    #[test]
    fn prop_memory_bytes_formula(
        width in arb_width(),
        depth in arb_depth(),
    ) {
        let cms = CountMinSketch::with_dimensions(width, depth);
        let expected = cms.width() * cms.depth() * std::mem::size_of::<u64>();
        prop_assert_eq!(cms.memory_bytes(), expected);
    }

    /// epsilon is positive and inversely proportional to width.
    #[test]
    fn prop_epsilon_positive_bounded(
        width in arb_width(),
        depth in arb_depth(),
    ) {
        let cms = CountMinSketch::with_dimensions(width, depth);
        let eps = cms.epsilon();
        prop_assert!(eps > 0.0, "epsilon must be positive");
        prop_assert!(eps < 1.0, "epsilon should be < 1 for width >= 4");
    }

    /// delta is positive and less than 1 for depth >= 1.
    #[test]
    fn prop_delta_positive_bounded(
        width in arb_width(),
        depth in arb_depth(),
    ) {
        let cms = CountMinSketch::with_dimensions(width, depth);
        let d = cms.delta();
        prop_assert!(d > 0.0, "delta must be positive");
        prop_assert!(d < 1.0, "delta must be < 1 for depth >= 1");
    }

    /// Wider sketch has smaller epsilon (tighter error).
    #[test]
    fn prop_wider_means_smaller_epsilon(
        w1 in 4usize..=128,
        w2 in 4usize..=128,
        depth in arb_depth(),
    ) {
        prop_assume!(w1 != w2);
        let cms1 = CountMinSketch::with_dimensions(w1, depth);
        let cms2 = CountMinSketch::with_dimensions(w2, depth);
        if w1 > w2 {
            prop_assert!(cms1.epsilon() < cms2.epsilon());
        } else {
            prop_assert!(cms1.epsilon() > cms2.epsilon());
        }
    }

    /// Deeper sketch has smaller delta (higher confidence).
    #[test]
    fn prop_deeper_means_smaller_delta(
        width in arb_width(),
        d1 in 1usize..=7,
        d2 in 1usize..=7,
    ) {
        prop_assume!(d1 != d2);
        let cms1 = CountMinSketch::with_dimensions(width, d1);
        let cms2 = CountMinSketch::with_dimensions(width, d2);
        if d1 > d2 {
            prop_assert!(cms1.delta() < cms2.delta());
        } else {
            prop_assert!(cms1.delta() > cms2.delta());
        }
    }
}

// ── Config serde roundtrip ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// CmsConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: CmsConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// CmsStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(
        (config, entries) in arb_small_sketch(),
    ) {
        let cms = build_sketch(&config, &entries);
        let stats = cms.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: CmsStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.width, back.width);
        prop_assert_eq!(stats.depth, back.depth);
        prop_assert_eq!(stats.total_count, back.total_count);
        prop_assert_eq!(stats.memory_bytes, back.memory_bytes);
        let eps_diff = (stats.epsilon - back.epsilon).abs();
        prop_assert!(eps_diff < 1e-12, "epsilon mismatch after serde roundtrip");
        let delta_diff = (stats.delta - back.delta).abs();
        prop_assert!(delta_diff < 1e-12, "delta mismatch after serde roundtrip");
    }

    /// Stats reflect actual sketch dimensions and count.
    #[test]
    fn prop_stats_consistent(
        (config, entries) in arb_small_sketch(),
    ) {
        let cms = build_sketch(&config, &entries);
        let stats = cms.stats();
        prop_assert_eq!(stats.width, cms.width());
        prop_assert_eq!(stats.depth, cms.depth());
        prop_assert_eq!(stats.total_count, cms.total_count());
        prop_assert_eq!(stats.memory_bytes, cms.memory_bytes());
        // epsilon and delta from stats should match computed values
        let eps_diff = (stats.epsilon - cms.epsilon()).abs();
        prop_assert!(eps_diff < 1e-12, "epsilon mismatch");
        let delta_diff = (stats.delta - cms.delta()).abs();
        prop_assert!(delta_diff < 1e-12, "delta mismatch");
    }
}

// ── From error params ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// from_error_params produces sketch with epsilon <= requested epsilon.
    #[test]
    fn prop_error_params_epsilon_bound(
        epsilon in 0.001f64..=1.0,
        delta in 0.001f64..=0.5,
    ) {
        let cms = CountMinSketch::from_error_params(epsilon, delta);
        let actual_eps = cms.epsilon();
        prop_assert!(
            actual_eps <= epsilon + 0.01,
            "actual epsilon {} exceeds requested {} by too much", actual_eps, epsilon
        );
    }

    /// from_error_params produces sketch with delta <= requested delta (approximately).
    #[test]
    fn prop_error_params_delta_bound(
        epsilon in 0.001f64..=1.0,
        delta in 0.01f64..=0.5,
    ) {
        let cms = CountMinSketch::from_error_params(epsilon, delta);
        let actual_delta = cms.delta();
        prop_assert!(
            actual_delta <= delta + 0.01,
            "actual delta {} exceeds requested {} by too much", actual_delta, delta
        );
    }
}

// ── Empty sketch properties ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty sketch returns 0 estimate for any item.
    #[test]
    fn prop_empty_zero_estimate(
        config in arb_config(),
        items in prop::collection::vec(0u64..10000, 1..20),
    ) {
        let cms = CountMinSketch::with_config(config);
        for item in &items {
            prop_assert_eq!(cms.estimate(item), 0);
        }
    }

    /// Empty sketch has total_count 0 and is_empty true.
    #[test]
    fn prop_empty_invariants(config in arb_config()) {
        let cms = CountMinSketch::with_config(config);
        prop_assert!(cms.is_empty());
        prop_assert_eq!(cms.total_count(), 0);
    }

    /// Default sketch is empty.
    #[test]
    fn prop_default_is_empty(_dummy in 0..1u8) {
        let cms = CountMinSketch::new();
        prop_assert!(cms.is_empty());
        prop_assert_eq!(cms.total_count(), 0);
    }

    /// is_empty agrees with total_count == 0.
    #[test]
    fn prop_is_empty_agrees_with_total(
        entries in prop::collection::vec((0u64..500, arb_count()), 0..30),
    ) {
        let mut cms = CountMinSketch::new();
        for &(ref item, count) in &entries {
            cms.add(item, count);
        }
        prop_assert_eq!(cms.is_empty(), cms.total_count() == 0);
    }
}
