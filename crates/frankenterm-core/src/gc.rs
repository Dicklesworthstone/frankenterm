//! Cache GC primitives for long-running watcher processes.
//!
//! This module focuses on two goals:
//! 1. Reclaim excess `HashMap` capacity after churn (pane create/destroy loops).
//! 2. Decide when SQLite free-page fragmentation warrants a full `VACUUM`.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

/// Runtime settings for periodic cache GC.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheGcSettings {
    /// Enable/disable periodic GC.
    pub enabled: bool,
    /// GC cadence in seconds.
    pub interval_secs: u64,
    /// Vacuum trigger threshold over free-page ratio (0.0..=1.0).
    pub vacuum_threshold: f64,
}

impl Default for CacheGcSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 3600,
            vacuum_threshold: 0.20,
        }
    }
}

/// Result of compacting one map-like cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheCompactionStats {
    pub before_len: usize,
    pub before_capacity: usize,
    pub after_len: usize,
    pub after_capacity: usize,
    pub removed_entries: usize,
}

impl CacheCompactionStats {
    #[must_use]
    pub fn freed_slots(self) -> usize {
        self.before_capacity.saturating_sub(self.after_capacity)
    }
}

/// Clamp vacuum threshold to a safe, deterministic range.
#[must_use]
pub fn normalized_vacuum_threshold(threshold: f64) -> f64 {
    if threshold.is_finite() {
        threshold.clamp(0.0, 1.0)
    } else {
        CacheGcSettings::default().vacuum_threshold
    }
}

/// Compute SQLite free-page ratio from raw page counters.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn free_page_ratio(page_count: i64, free_pages: i64) -> f64 {
    if page_count <= 0 || free_pages <= 0 {
        return 0.0;
    }

    let bounded_free = free_pages.min(page_count);
    bounded_free as f64 / page_count as f64
}

/// Should we run full `VACUUM` given current page stats and threshold?
#[must_use]
pub fn should_vacuum(page_count: i64, free_pages: i64, threshold: f64) -> bool {
    free_page_ratio(page_count, free_pages) > normalized_vacuum_threshold(threshold)
}

/// Compact a `HashMap<u64, V>` by removing non-active keys and shrinking excess capacity.
///
/// This is intentionally deterministic:
/// - retain keys present in `active_keys`
/// - reclaim capacity only when slack is materially large
#[must_use]
pub fn compact_u64_map<V, MapHasher, SetHasher>(
    map: &mut HashMap<u64, V, MapHasher>,
    active_keys: &HashSet<u64, SetHasher>,
) -> CacheCompactionStats
where
    MapHasher: BuildHasher,
    SetHasher: BuildHasher,
{
    let before_len = map.len();
    let before_capacity = map.capacity();

    map.retain(|key, _| active_keys.contains(key));
    let removed_entries = before_len.saturating_sub(map.len());

    // Avoid paying `shrink_to_fit()` cost when slack is minimal.
    if removed_entries > 0 && map.capacity() > map.len().saturating_mul(2) {
        map.shrink_to_fit();
    }

    let after_len = map.len();
    let after_capacity = map.capacity();

    CacheCompactionStats {
        before_len,
        before_capacity,
        after_len,
        after_capacity,
        removed_entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn vacuum_threshold_is_clamped() {
        assert!((normalized_vacuum_threshold(-1.0) - 0.0).abs() < f64::EPSILON);
        assert!((normalized_vacuum_threshold(2.0) - 1.0).abs() < f64::EPSILON);
        assert!(
            (normalized_vacuum_threshold(f64::NAN) - CacheGcSettings::default().vacuum_threshold)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn vacuum_decision_obeys_ratio() {
        assert!(!should_vacuum(100, 20, 0.20));
        assert!(should_vacuum(100, 21, 0.20));
        assert!(!should_vacuum(0, 50, 0.20));
    }

    #[test]
    fn compact_map_keeps_active_entries_and_is_idempotent() {
        let mut map = HashMap::new();
        map.insert(1, "a");
        map.insert(2, "b");
        map.insert(3, "c");
        map.insert(99, "dead");

        let active: HashSet<u64> = [1, 2, 3].into_iter().collect();

        let first = compact_u64_map(&mut map, &active);
        assert_eq!(map.len(), 3);
        assert!(map.contains_key(&1));
        assert!(map.contains_key(&2));
        assert!(map.contains_key(&3));
        assert!(!map.contains_key(&99));
        assert_eq!(first.removed_entries, 1);
        assert!(first.after_capacity <= first.before_capacity);

        // Second pass without mutations should be a no-op.
        let second = compact_u64_map(&mut map, &active);
        assert_eq!(second.removed_entries, 0);
        assert_eq!(second.freed_slots(), 0);
    }

    // =====================================================================
    // CacheGcSettings tests
    // =====================================================================

    #[test]
    fn gc_settings_default_values() {
        let s = CacheGcSettings::default();
        assert!(s.enabled);
        assert_eq!(s.interval_secs, 3600);
        assert!((s.vacuum_threshold - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn gc_settings_clone_eq() {
        let s = CacheGcSettings {
            enabled: false,
            interval_secs: 600,
            vacuum_threshold: 0.5,
        };
        let s2 = s;
        assert_eq!(s, s2);
    }

    #[test]
    fn gc_settings_debug() {
        let s = CacheGcSettings::default();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("CacheGcSettings"));
        assert!(dbg.contains("3600"));
    }

    // =====================================================================
    // CacheCompactionStats tests
    // =====================================================================

    #[test]
    fn compaction_stats_default() {
        let s = CacheCompactionStats::default();
        assert_eq!(s.before_len, 0);
        assert_eq!(s.before_capacity, 0);
        assert_eq!(s.after_len, 0);
        assert_eq!(s.after_capacity, 0);
        assert_eq!(s.removed_entries, 0);
    }

    #[test]
    fn compaction_stats_freed_slots() {
        let s = CacheCompactionStats {
            before_len: 10,
            before_capacity: 32,
            after_len: 5,
            after_capacity: 8,
            removed_entries: 5,
        };
        assert_eq!(s.freed_slots(), 24);
    }

    #[test]
    fn compaction_stats_freed_slots_zero_when_no_change() {
        let s = CacheCompactionStats {
            before_capacity: 16,
            after_capacity: 16,
            ..Default::default()
        };
        assert_eq!(s.freed_slots(), 0);
    }

    #[test]
    fn compaction_stats_freed_slots_saturating() {
        // after_capacity > before_capacity shouldn't happen in practice,
        // but freed_slots() uses saturating_sub for safety
        let s = CacheCompactionStats {
            before_capacity: 4,
            after_capacity: 8,
            ..Default::default()
        };
        assert_eq!(s.freed_slots(), 0);
    }

    #[test]
    fn compaction_stats_clone_eq() {
        let s = CacheCompactionStats {
            before_len: 3,
            before_capacity: 16,
            after_len: 2,
            after_capacity: 4,
            removed_entries: 1,
        };
        let s2 = s;
        assert_eq!(s, s2);
    }

    // =====================================================================
    // normalized_vacuum_threshold tests
    // =====================================================================

    #[test]
    fn normalized_threshold_exact_boundaries() {
        assert!((normalized_vacuum_threshold(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((normalized_vacuum_threshold(0.5) - 0.5).abs() < f64::EPSILON);
        assert!((normalized_vacuum_threshold(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalized_threshold_infinity() {
        let def = CacheGcSettings::default().vacuum_threshold;
        assert!((normalized_vacuum_threshold(f64::INFINITY) - def).abs() < f64::EPSILON);
        assert!((normalized_vacuum_threshold(f64::NEG_INFINITY) - def).abs() < f64::EPSILON);
    }

    // =====================================================================
    // free_page_ratio tests
    // =====================================================================

    #[test]
    fn free_page_ratio_zero_pages() {
        assert!((free_page_ratio(0, 10) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_page_ratio_zero_free() {
        assert!((free_page_ratio(100, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_page_ratio_negative_pages() {
        assert!((free_page_ratio(-10, 5) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_page_ratio_negative_free() {
        assert!((free_page_ratio(100, -5) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_page_ratio_all_free() {
        assert!((free_page_ratio(100, 100) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_page_ratio_free_exceeds_total() {
        // free_pages clamped to page_count
        assert!((free_page_ratio(100, 200) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_page_ratio_normal() {
        assert!((free_page_ratio(100, 25) - 0.25).abs() < f64::EPSILON);
    }

    // =====================================================================
    // should_vacuum tests
    // =====================================================================

    #[test]
    fn should_vacuum_exact_boundary() {
        // At exactly the threshold ratio (not strictly greater), should NOT vacuum
        assert!(!should_vacuum(100, 20, 0.20)); // 0.20 == 0.20
    }

    #[test]
    fn should_vacuum_just_above() {
        assert!(should_vacuum(1000, 201, 0.20)); // 0.201 > 0.20
    }

    #[test]
    fn should_vacuum_zero_threshold() {
        // With threshold 0.0, any non-zero free pages trigger vacuum
        assert!(should_vacuum(100, 1, 0.0));
    }

    #[test]
    fn should_vacuum_threshold_one() {
        // Threshold 1.0: never triggers (ratio can't exceed 1.0)
        assert!(!should_vacuum(100, 100, 1.0));
    }

    // =====================================================================
    // compact_u64_map additional tests
    // =====================================================================

    #[test]
    fn compact_empty_map() {
        let mut map: HashMap<u64, &str> = HashMap::new();
        let active: HashSet<u64> = HashSet::new();
        let stats = compact_u64_map(&mut map, &active);
        assert_eq!(stats.before_len, 0);
        assert_eq!(stats.after_len, 0);
        assert_eq!(stats.removed_entries, 0);
    }

    #[test]
    fn compact_empty_active_set_removes_all() {
        let mut map = HashMap::new();
        map.insert(1, "a");
        map.insert(2, "b");
        map.insert(3, "c");
        let active: HashSet<u64> = HashSet::new();
        let stats = compact_u64_map(&mut map, &active);
        assert_eq!(stats.removed_entries, 3);
        assert_eq!(stats.after_len, 0);
        assert!(map.is_empty());
    }

    #[test]
    fn compact_all_keys_active_removes_none() {
        let mut map = HashMap::new();
        map.insert(1, "a");
        map.insert(2, "b");
        let active: HashSet<u64> = [1, 2].into_iter().collect();
        let stats = compact_u64_map(&mut map, &active);
        assert_eq!(stats.removed_entries, 0);
        assert_eq!(stats.after_len, 2);
    }

    #[test]
    fn compact_active_keys_superset_of_map() {
        let mut map = HashMap::new();
        map.insert(1, "a");
        let active: HashSet<u64> = [1, 2, 3, 4, 5].into_iter().collect();
        let stats = compact_u64_map(&mut map, &active);
        assert_eq!(stats.removed_entries, 0);
        assert_eq!(stats.after_len, 1);
    }

    proptest! {
        #[test]
        fn capacity_never_increases_after_compaction(
            entries in proptest::collection::hash_map(any::<u64>(), any::<u16>(), 0..500),
            active_raw in proptest::collection::vec(any::<u64>(), 0..500),
        ) {
            let mut map = entries;
            let active: HashSet<u64> = active_raw.into_iter().collect();
            let before_capacity = map.capacity();

            let stats = compact_u64_map(&mut map, &active);

            prop_assert_eq!(stats.before_capacity, before_capacity);
            prop_assert!(stats.after_capacity <= stats.before_capacity);
            prop_assert!(map.capacity() <= before_capacity);
        }

        #[test]
        fn active_entries_are_never_dropped(
            entries in proptest::collection::hash_map(any::<u64>(), any::<u16>(), 0..300),
            active_raw in proptest::collection::vec(any::<u64>(), 0..300),
        ) {
            let mut map = entries.clone();
            let active: HashSet<u64> = active_raw.into_iter().collect();
            let expected: Vec<(u64, u16)> = entries
                .iter()
                .filter(|(key, _)| active.contains(key))
                .map(|(key, value)| (*key, *value))
                .collect();

            let _ = compact_u64_map(&mut map, &active);

            for (key, value) in expected {
                prop_assert_eq!(map.get(&key), Some(&value));
            }
            prop_assert!(map.keys().all(|key| active.contains(key)));
        }

        #[test]
        fn gc_is_idempotent_without_mutations(
            entries in proptest::collection::hash_map(any::<u64>(), any::<u16>(), 0..300),
            active_raw in proptest::collection::vec(any::<u64>(), 0..300),
        ) {
            let mut map = entries;
            let active: HashSet<u64> = active_raw.into_iter().collect();

            let _first = compact_u64_map(&mut map, &active);
            let snapshot = map.clone();

            let second = compact_u64_map(&mut map, &active);

            prop_assert_eq!(map, snapshot);
            prop_assert_eq!(second.removed_entries, 0);
            prop_assert_eq!(second.freed_slots(), 0);
            prop_assert_eq!(second.before_capacity, second.after_capacity);
        }

        #[test]
        fn vacuum_decision_matches_ratio_threshold_rule(
            page_count in 1_i64..1_000_000_i64,
            free_pages in 0_i64..2_000_000_i64,
            threshold in -0.5_f64..1.5_f64,
        ) {
            let expected = free_page_ratio(page_count, free_pages)
                > normalized_vacuum_threshold(threshold);

            prop_assert_eq!(should_vacuum(page_count, free_pages, threshold), expected);
            prop_assert_eq!(should_vacuum(page_count, free_pages, threshold), expected);
        }
    }
}
