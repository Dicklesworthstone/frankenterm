//! Property-based tests for cache GC primitives.
//!
//! Validates:
//! 1. free_page_ratio always returns [0.0, 1.0]
//! 2. normalized_vacuum_threshold always returns [0.0, 1.0]
//! 3. should_vacuum agrees with free_page_ratio > threshold
//! 4. compact_u64_map preserves active entries and only removes inactive ones
//! 5. CacheCompactionStats accounting is consistent
//! 6. freed_slots is self-consistent with before/after capacity
//! 7. compact_u64_map is idempotent when called twice without mutations
//! 8. Vacuum decision is monotonic in free_pages (more free pages = more likely)
//! 9. Empty map compaction is a no-op
//! 10. Full overlap (all entries active) preserves all entries
//! 11. No overlap (no entries active) removes all entries
//! 12. Superset active keys preserves all entries
//! 13. Stats removed + after_len = before_len
//! 14. Threshold clamping monotonicity
//! 15. Settings PartialEq reflexivity
//! 16. free_page_ratio is 1.0 when all pages are free

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use frankenterm_core::gc::{
    compact_u64_map, free_page_ratio, normalized_vacuum_threshold, should_vacuum,
    CacheCompactionStats, CacheGcSettings,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_page_counts() -> impl Strategy<Value = (i64, i64)> {
    (-10_i64..1_000_000, -10_i64..2_000_000)
}

fn arb_threshold() -> impl Strategy<Value = f64> {
    prop_oneof![
        // Normal range
        (0_u32..1000).prop_map(|n| n as f64 / 1000.0),
        // Out-of-range values
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(-1.0),
        Just(2.0),
        Just(-0.0),
    ]
}

fn arb_map_and_active(
    max_entries: usize,
) -> impl Strategy<Value = (HashMap<u64, u16>, HashSet<u64>)> {
    let entries = proptest::collection::hash_map(any::<u64>(), any::<u16>(), 0..max_entries);
    let active = proptest::collection::hash_set(any::<u64>(), 0..max_entries);
    (entries, active)
}

// =============================================================================
// Property: free_page_ratio always returns [0.0, 1.0]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn free_page_ratio_bounded(
        (page_count, free_pages) in arb_page_counts(),
    ) {
        let ratio = free_page_ratio(page_count, free_pages);
        prop_assert!(ratio >= 0.0);
        prop_assert!(ratio <= 1.0);
        prop_assert!(ratio.is_finite());
    }

    #[test]
    fn free_page_ratio_zero_when_no_pages(free_pages in -10_i64..1_000_000) {
        prop_assert!(free_page_ratio(0, free_pages).abs() < f64::EPSILON,
            "free_page_ratio(0, {}) should be 0.0", free_pages);
        prop_assert!(free_page_ratio(-1, free_pages).abs() < f64::EPSILON,
            "free_page_ratio(-1, {}) should be 0.0", free_pages);
    }

    #[test]
    fn free_page_ratio_zero_when_no_free_pages(page_count in 1_i64..1_000_000) {
        prop_assert!(free_page_ratio(page_count, 0).abs() < f64::EPSILON,
            "free_page_ratio({}, 0) should be 0.0", page_count);
        prop_assert!(free_page_ratio(page_count, -1).abs() < f64::EPSILON,
            "free_page_ratio({}, -1) should be 0.0", page_count);
    }

    #[test]
    fn free_page_ratio_monotonic_in_free_pages(
        page_count in 1_i64..100_000,
        free_a in 0_i64..100_000,
        free_b in 0_i64..100_000,
    ) {
        let ratio_a = free_page_ratio(page_count, free_a);
        let ratio_b = free_page_ratio(page_count, free_b);
        if free_a <= free_b {
            prop_assert!(
                ratio_a <= ratio_b,
                "ratio must be monotonic: ratio({})={} > ratio({})={}",
                free_a, ratio_a, free_b, ratio_b,
            );
        }
    }
}

// =============================================================================
// Property: normalized_vacuum_threshold always returns [0.0, 1.0]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn normalized_threshold_always_bounded(threshold in arb_threshold()) {
        let result = normalized_vacuum_threshold(threshold);
        prop_assert!(result >= 0.0);
        prop_assert!(result <= 1.0);
        prop_assert!(result.is_finite());
    }

    #[test]
    fn normalized_threshold_preserves_valid_range(
        threshold in 0.0_f64..=1.0,
    ) {
        let result = normalized_vacuum_threshold(threshold);
        prop_assert!((result - threshold).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_threshold_non_finite_uses_default(
        choice in 0_u8..3,
    ) {
        let input = match choice {
            0 => f64::NAN,
            1 => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        };
        let result = normalized_vacuum_threshold(input);
        let default = CacheGcSettings::default().vacuum_threshold;
        prop_assert!((result - default).abs() < f64::EPSILON);
    }
}

// =============================================================================
// Property: should_vacuum agrees with ratio > threshold
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn vacuum_decision_matches_ratio_comparison(
        page_count in -10_i64..1_000_000,
        free_pages in -10_i64..2_000_000,
        threshold in -0.5_f64..1.5,
    ) {
        let ratio = free_page_ratio(page_count, free_pages);
        let norm_thresh = normalized_vacuum_threshold(threshold);
        let expected = ratio > norm_thresh;
        let actual = should_vacuum(page_count, free_pages, threshold);
        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn vacuum_monotonic_in_free_pages(
        page_count in 1_i64..100_000,
        free_a in 0_i64..100_000,
        free_b in 0_i64..100_000,
        threshold in 0.01_f64..0.99,
    ) {
        // If free_a < free_b and vacuuming at free_a, must also vacuum at free_b.
        if free_a < free_b && should_vacuum(page_count, free_a, threshold) {
            prop_assert!(
                should_vacuum(page_count, free_b, threshold),
                "vacuum decision must be monotonic in free_pages"
            );
        }
    }
}

// =============================================================================
// Property: compact_u64_map preserves active entries
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn compaction_preserves_active_entries(
        (entries, active) in arb_map_and_active(500),
    ) {
        let mut map = entries.clone();
        let _ = compact_u64_map(&mut map, &active);

        // Every entry that was both in the map and in active_keys must still exist.
        for (key, value) in &entries {
            if active.contains(key) {
                prop_assert_eq!(map.get(key), Some(value));
            }
        }
    }

    #[test]
    fn compaction_removes_only_inactive_entries(
        (entries, active) in arb_map_and_active(500),
    ) {
        let mut map = entries;
        let _ = compact_u64_map(&mut map, &active);

        // Every remaining key must be in active_keys.
        for key in map.keys() {
            prop_assert!(active.contains(key));
        }
    }

    #[test]
    fn compaction_never_adds_entries(
        (entries, active) in arb_map_and_active(500),
    ) {
        let original_keys: HashSet<u64> = entries.keys().copied().collect();
        let mut map = entries;
        let _ = compact_u64_map(&mut map, &active);

        // No key should appear that wasn't in the original map.
        for key in map.keys() {
            prop_assert!(original_keys.contains(key));
        }
    }
}

// =============================================================================
// Property: CacheCompactionStats accounting
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn stats_accounting_consistent(
        (entries, active) in arb_map_and_active(500),
    ) {
        let mut map = entries.clone();
        let before_len = map.len();

        let stats = compact_u64_map(&mut map, &active);

        // before_len matches actual before length.
        prop_assert_eq!(stats.before_len, before_len);
        // after_len matches actual after length.
        prop_assert_eq!(stats.after_len, map.len());
        // removed_entries = before_len - after_len.
        prop_assert_eq!(
            stats.removed_entries,
            before_len.saturating_sub(map.len()),
            "removed_entries mismatch"
        );
        // Capacity never increases.
        prop_assert!(
            stats.after_capacity <= stats.before_capacity,
            "capacity increased: {} -> {}",
            stats.before_capacity,
            stats.after_capacity
        );
    }

    #[test]
    fn freed_slots_consistent(
        before_cap in 0_usize..10000,
        after_cap in 0_usize..10000,
    ) {
        let stats = CacheCompactionStats {
            before_len: 0,
            before_capacity: before_cap,
            after_len: 0,
            after_capacity: after_cap,
            removed_entries: 0,
        };
        let freed = stats.freed_slots();
        prop_assert_eq!(freed, before_cap.saturating_sub(after_cap));
    }
}

// =============================================================================
// Property: Idempotence — double compaction is a no-op
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn compaction_idempotent(
        (entries, active) in arb_map_and_active(300),
    ) {
        let mut map = entries;
        let _ = compact_u64_map(&mut map, &active);
        let snapshot: HashMap<u64, u16> = map.clone();
        let cap_after_first = map.capacity();

        let second_stats = compact_u64_map(&mut map, &active);

        prop_assert_eq!(map, snapshot, "second compaction changed map contents");
        prop_assert_eq!(second_stats.removed_entries, 0);
        prop_assert_eq!(second_stats.before_len, second_stats.after_len);
        prop_assert_eq!(second_stats.before_capacity, cap_after_first);
    }
}

// =============================================================================
// Property: Default settings are reasonable
// =============================================================================

#[test]
fn default_settings_are_valid() {
    let settings = CacheGcSettings::default();
    assert!(settings.enabled);
    assert!(settings.interval_secs > 0);
    assert!(settings.vacuum_threshold > 0.0);
    assert!(settings.vacuum_threshold <= 1.0);
}

#[test]
fn default_compaction_stats_are_zero() {
    let stats = CacheCompactionStats::default();
    assert_eq!(stats.before_len, 0);
    assert_eq!(stats.before_capacity, 0);
    assert_eq!(stats.after_len, 0);
    assert_eq!(stats.after_capacity, 0);
    assert_eq!(stats.removed_entries, 0);
    assert_eq!(stats.freed_slots(), 0);
}

// =============================================================================
// Property: Empty map compaction is a no-op
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn empty_map_compaction_noop(
        active in proptest::collection::hash_set(any::<u64>(), 0..100),
    ) {
        let mut map: HashMap<u64, u16> = HashMap::new();
        let stats = compact_u64_map(&mut map, &active);

        prop_assert!(map.is_empty(), "compacting empty map should keep it empty");
        prop_assert_eq!(stats.before_len, 0);
        prop_assert_eq!(stats.after_len, 0);
        prop_assert_eq!(stats.removed_entries, 0);
    }
}

// =============================================================================
// Property: Full overlap (all entries active) preserves all
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn full_overlap_preserves_all(
        entries in proptest::collection::hash_map(any::<u64>(), any::<u16>(), 1..200),
    ) {
        let active: HashSet<u64> = entries.keys().copied().collect();
        let original = entries.clone();
        let mut map = entries;
        let stats = compact_u64_map(&mut map, &active);

        prop_assert_eq!(map, original, "all-active compaction should preserve all entries");
        prop_assert_eq!(stats.removed_entries, 0);
        prop_assert_eq!(stats.before_len, stats.after_len);
    }

    /// Superset of active keys also preserves all entries.
    #[test]
    fn superset_active_preserves_all(
        entries in proptest::collection::hash_map(any::<u64>(), any::<u16>(), 1..100),
        extra_keys in proptest::collection::hash_set(any::<u64>(), 0..100),
    ) {
        let mut active: HashSet<u64> = entries.keys().copied().collect();
        active.extend(extra_keys);
        let original = entries.clone();
        let mut map = entries;
        let stats = compact_u64_map(&mut map, &active);

        prop_assert_eq!(map, original, "superset active should preserve all entries");
        prop_assert_eq!(stats.removed_entries, 0);
    }
}

// =============================================================================
// Property: No overlap (no active entries) removes all
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_overlap_removes_all(
        entries in proptest::collection::hash_map(0_u64..1000, any::<u16>(), 1..200),
    ) {
        // Active keys are in a disjoint range
        let active: HashSet<u64> = (1_000_000..1_000_100).collect();
        let before_len = entries.len();
        let mut map = entries;
        let stats = compact_u64_map(&mut map, &active);

        prop_assert!(map.is_empty(), "disjoint active should remove all entries");
        prop_assert_eq!(stats.removed_entries, before_len);
        prop_assert_eq!(stats.after_len, 0);
    }
}

// =============================================================================
// Property: Stats removed + after_len = before_len
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn stats_removed_plus_after_equals_before(
        (entries, active) in arb_map_and_active(500),
    ) {
        let mut map = entries;
        let stats = compact_u64_map(&mut map, &active);

        prop_assert_eq!(
            stats.removed_entries + stats.after_len,
            stats.before_len,
            "removed + after_len should equal before_len"
        );
    }

    /// after_len equals the intersection of map keys and active keys.
    #[test]
    fn after_len_equals_intersection_size(
        (entries, active) in arb_map_and_active(300),
    ) {
        let expected_after: usize = entries.keys().filter(|k| active.contains(k)).count();
        let mut map = entries;
        let stats = compact_u64_map(&mut map, &active);

        prop_assert_eq!(stats.after_len, expected_after,
            "after_len should equal |map keys ∩ active keys|");
        prop_assert_eq!(map.len(), expected_after);
    }
}

// =============================================================================
// Property: Threshold clamping is monotonic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// For finite inputs, normalized threshold is monotonically non-decreasing.
    #[test]
    fn threshold_clamping_monotonic(
        a in -2.0_f64..3.0,
        b in -2.0_f64..3.0,
    ) {
        if a.is_finite() && b.is_finite() && a <= b {
            let norm_a = normalized_vacuum_threshold(a);
            let norm_b = normalized_vacuum_threshold(b);
            prop_assert!(
                norm_a <= norm_b,
                "normalized({}) = {} > normalized({}) = {}",
                a, norm_a, b, norm_b
            );
        }
    }

    /// Threshold clamping is idempotent.
    #[test]
    fn threshold_clamping_idempotent(threshold in arb_threshold()) {
        let once = normalized_vacuum_threshold(threshold);
        let twice = normalized_vacuum_threshold(once);
        prop_assert!((once - twice).abs() < f64::EPSILON,
            "clamping should be idempotent: {} != {}", once, twice);
    }
}

// =============================================================================
// Property: free_page_ratio special values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Ratio is exactly 1.0 when free_pages >= page_count > 0.
    #[test]
    fn ratio_is_one_when_all_free(page_count in 1_i64..100_000) {
        let ratio = free_page_ratio(page_count, page_count);
        prop_assert!((ratio - 1.0).abs() < f64::EPSILON,
            "ratio should be 1.0 when all pages free, got {}", ratio);
    }

    /// Ratio is bounded by free_pages/page_count from above.
    #[test]
    fn ratio_bounded_by_fraction(
        page_count in 1_i64..100_000,
        free_pages in 0_i64..200_000,
    ) {
        let ratio = free_page_ratio(page_count, free_pages);
        let raw_fraction = free_pages.min(page_count) as f64 / page_count as f64;
        prop_assert!((ratio - raw_fraction).abs() < f64::EPSILON,
            "ratio {} should equal bounded fraction {}", ratio, raw_fraction);
    }

    /// Anti-monotonic in page_count: more total pages with same free = lower ratio.
    #[test]
    fn ratio_anti_monotonic_in_page_count(
        free_pages in 1_i64..10_000,
        pc_a in 1_i64..100_000,
        pc_b in 1_i64..100_000,
    ) {
        if pc_a <= pc_b && free_pages <= pc_a {
            let ratio_a = free_page_ratio(pc_a, free_pages);
            let ratio_b = free_page_ratio(pc_b, free_pages);
            prop_assert!(
                ratio_a >= ratio_b,
                "ratio should be anti-monotonic in page_count: r({})={} < r({})={}",
                pc_a, ratio_a, pc_b, ratio_b
            );
        }
    }
}

// =============================================================================
// Property: CacheGcSettings PartialEq
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Settings PartialEq is reflexive.
    #[test]
    fn settings_eq_reflexive(
        enabled in any::<bool>(),
        interval in 1_u64..100_000,
        threshold in 0.0_f64..=1.0,
    ) {
        let settings = CacheGcSettings {
            enabled,
            interval_secs: interval,
            vacuum_threshold: threshold,
        };
        prop_assert!(settings == settings, "PartialEq should be reflexive");
    }

    /// Default settings equal themselves.
    #[test]
    fn default_settings_eq_self(_dummy in 0u8..1) {
        let a = CacheGcSettings::default();
        let b = CacheGcSettings::default();
        prop_assert!(a == b, "default should equal default");
    }

    /// Different enabled flag means different settings.
    #[test]
    fn settings_ne_different_enabled(
        interval in 1_u64..100_000,
        threshold in 0.0_f64..=1.0,
    ) {
        let a = CacheGcSettings {
            enabled: true,
            interval_secs: interval,
            vacuum_threshold: threshold,
        };
        let b = CacheGcSettings {
            enabled: false,
            interval_secs: interval,
            vacuum_threshold: threshold,
        };
        prop_assert!(a != b, "different enabled should mean different settings");
    }
}

// =============================================================================
// Property: CacheCompactionStats freed_slots saturating
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// freed_slots uses saturating subtraction (never underflows).
    #[test]
    fn freed_slots_never_underflows(
        before_cap in 0_usize..10000,
        after_cap in 0_usize..10000,
    ) {
        let stats = CacheCompactionStats {
            before_len: 0,
            before_capacity: before_cap,
            after_len: 0,
            after_capacity: after_cap,
            removed_entries: 0,
        };
        // freed_slots should never panic or overflow
        let freed = stats.freed_slots();
        if after_cap > before_cap {
            prop_assert_eq!(freed, 0, "freed_slots should be 0 when after > before");
        } else {
            prop_assert_eq!(freed, before_cap - after_cap);
        }
    }

    /// Stats Copy/Clone equivalence.
    #[test]
    fn stats_copy_equivalence(
        before_len in 0_usize..1000,
        before_cap in 0_usize..2000,
        after_len in 0_usize..1000,
        after_cap in 0_usize..2000,
        removed in 0_usize..1000,
    ) {
        let stats = CacheCompactionStats {
            before_len,
            before_capacity: before_cap,
            after_len,
            after_capacity: after_cap,
            removed_entries: removed,
        };
        let copied = stats;
        prop_assert_eq!(stats, copied, "Copy should produce identical stats");
        prop_assert_eq!(stats.freed_slots(), copied.freed_slots());
    }
}
