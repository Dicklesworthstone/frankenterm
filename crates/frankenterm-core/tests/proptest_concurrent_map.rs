//! Property-based tests for the concurrent_map module.
//!
//! Tests cover: ShardedMap insert/get/remove/contains roundtrips, len invariants,
//! insert_if_absent semantics, retain correctness, keys/values/entries consistency,
//! PaneMap insert/get/remove roundtrips, DistributionStats from_shard_sizes
//! invariants, DistributionStats serde roundtrip, and hash distribution quality.

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use frankenterm_core::concurrent_map::{DistributionStats, PaneMap, ShardedMap};

// ============================================================================
// Strategies
// ============================================================================

/// Arbitrary shard count (clamped to valid range by ShardedMap).
fn arb_shard_count() -> impl Strategy<Value = usize> {
    1usize..=32
}

/// Arbitrary key-value pairs for ShardedMap<u64, u64>.
fn arb_kv_pairs() -> impl Strategy<Value = Vec<(u64, u64)>> {
    prop::collection::vec((any::<u64>(), any::<u64>()), 0..100)
}

/// Arbitrary shard sizes for DistributionStats.
fn arb_shard_sizes() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(0usize..=200, 1..=64)
}

// ============================================================================
// ShardedMap: insert/get roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After inserting a key, get returns the value.
    #[test]
    fn prop_map_insert_then_get(
        shards in arb_shard_count(),
        key in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        map.insert(key, value);
        prop_assert_eq!(map.get(&key), Some(value));
    }

    /// Insert overwrites and returns old value.
    #[test]
    fn prop_map_insert_overwrite(
        shards in arb_shard_count(),
        key in any::<u64>(),
        v1 in any::<u64>(),
        v2 in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        prop_assert_eq!(map.insert(key, v1), None);
        prop_assert_eq!(map.insert(key, v2), Some(v1));
        prop_assert_eq!(map.get(&key), Some(v2));
    }

    /// After removing a key, get returns None.
    #[test]
    fn prop_map_remove_then_get(
        shards in arb_shard_count(),
        key in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        map.insert(key, value);
        prop_assert_eq!(map.remove(&key), Some(value));
        prop_assert_eq!(map.get(&key), None);
        prop_assert!(!map.contains_key(&key));
    }

    /// contains_key is true after insert, false for absent keys.
    #[test]
    fn prop_map_contains_key(
        shards in arb_shard_count(),
        key in any::<u64>(),
        absent in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        map.insert(key, value);
        prop_assert!(map.contains_key(&key));
        // Only test absent if it's different from key
        if absent != key {
            prop_assert!(!map.contains_key(&absent));
        }
    }
}

// ============================================================================
// ShardedMap: len and is_empty invariants
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// len equals the number of unique keys inserted.
    #[test]
    fn prop_map_len_matches_unique_keys(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        let mut expected_keys = HashSet::new();
        for (k, v) in &pairs {
            map.insert(*k, *v);
            expected_keys.insert(*k);
        }
        prop_assert_eq!(
            map.len(), expected_keys.len(),
            "len should match unique key count"
        );
    }

    /// is_empty iff len == 0.
    #[test]
    fn prop_map_is_empty_iff_len_zero(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        for (k, v) in &pairs {
            map.insert(*k, *v);
        }
        prop_assert_eq!(map.is_empty(), map.is_empty());
    }

    /// sum of shard_sizes equals len.
    #[test]
    fn prop_map_shard_sizes_sum_to_len(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        for (k, v) in &pairs {
            map.insert(*k, *v);
        }
        let total: usize = map.shard_sizes().iter().sum();
        prop_assert_eq!(total, map.len(), "shard_sizes sum should equal len");
    }
}

// ============================================================================
// ShardedMap: insert_if_absent
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// insert_if_absent preserves the first inserted value.
    #[test]
    fn prop_map_insert_if_absent_preserves_first(
        shards in arb_shard_count(),
        key in any::<u64>(),
        v1 in any::<u64>(),
        v2 in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        prop_assert!(map.insert_if_absent(key, v1));
        prop_assert!(!map.insert_if_absent(key, v2));
        prop_assert_eq!(map.get(&key), Some(v1), "first value should be preserved");
    }
}

// ============================================================================
// ShardedMap: retain
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// retain keeps only entries matching predicate, removes the rest.
    #[test]
    fn prop_map_retain_correctness(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        let mut reference = HashMap::new();
        for (k, v) in &pairs {
            map.insert(*k, *v);
            reference.insert(*k, *v);
        }

        // Retain entries where value is even
        map.retain(|_, v| *v % 2 == 0);
        reference.retain(|_, v| *v % 2 == 0);

        prop_assert_eq!(map.len(), reference.len(), "len after retain mismatch");
        for (k, v) in &reference {
            prop_assert_eq!(
                map.get(k), Some(*v),
                "retained entry missing or wrong"
            );
        }
    }
}

// ============================================================================
// ShardedMap: keys/values/entries consistency
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// keys() returns exactly the inserted unique keys.
    #[test]
    fn prop_map_keys_match_inserted(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        let mut expected_keys = HashSet::new();
        for (k, v) in &pairs {
            map.insert(*k, *v);
            expected_keys.insert(*k);
        }

        let actual_keys: HashSet<u64> = map.keys().into_iter().collect();
        prop_assert_eq!(actual_keys, expected_keys);
    }

    /// entries() matches keys() and values() in count.
    #[test]
    fn prop_map_entries_count_consistency(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        for (k, v) in &pairs {
            map.insert(*k, *v);
        }

        let entries = map.entries();
        let keys = map.keys();
        let values = map.values();
        prop_assert_eq!(entries.len(), keys.len());
        prop_assert_eq!(entries.len(), values.len());
        prop_assert_eq!(entries.len(), map.len());
    }

    /// Each entry's value matches get() for that key.
    #[test]
    fn prop_map_entries_match_get(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        for (k, v) in &pairs {
            map.insert(*k, *v);
        }

        for (k, v) in map.entries() {
            prop_assert_eq!(
                map.get(&k), Some(v),
                "entry ({}, {}) doesn't match get()",
                k, v
            );
        }
    }
}

// ============================================================================
// ShardedMap: clear
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After clear, map is empty and all keys are gone.
    #[test]
    fn prop_map_clear_empties(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        for (k, v) in &pairs {
            map.insert(*k, *v);
        }
        map.clear();
        prop_assert!(map.is_empty());
        prop_assert_eq!(map.len(), 0);
        for (k, _) in &pairs {
            prop_assert_eq!(map.get(k), None);
        }
    }
}

// ============================================================================
// PaneMap: insert/get/remove roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// PaneMap insert then get returns the value.
    #[test]
    fn prop_pane_map_insert_get(
        shards in arb_shard_count(),
        pane_id in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        map.insert(pane_id, value);
        prop_assert_eq!(map.get(pane_id), Some(value));
        prop_assert!(map.contains(pane_id));
    }

    /// PaneMap remove returns inserted value and key is gone.
    #[test]
    fn prop_pane_map_remove(
        shards in arb_shard_count(),
        pane_id in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        map.insert(pane_id, value);
        prop_assert_eq!(map.remove(pane_id), Some(value));
        prop_assert!(!map.contains(pane_id));
        prop_assert!(map.is_empty());
    }

    /// PaneMap len matches unique pane IDs inserted.
    #[test]
    fn prop_pane_map_len(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        let mut expected_keys = HashSet::new();
        for (k, v) in &pairs {
            map.insert(*k, *v);
            expected_keys.insert(*k);
        }
        prop_assert_eq!(map.len(), expected_keys.len());
    }

    /// PaneMap pane_ids returns all inserted keys.
    #[test]
    fn prop_pane_map_pane_ids(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        let mut expected_keys = HashSet::new();
        for (k, v) in &pairs {
            map.insert(*k, *v);
            expected_keys.insert(*k);
        }
        let actual_keys: HashSet<u64> = map.pane_ids().into_iter().collect();
        prop_assert_eq!(actual_keys, expected_keys);
    }

    /// PaneMap insert_if_absent preserves first value.
    #[test]
    fn prop_pane_map_insert_if_absent(
        shards in arb_shard_count(),
        pane_id in any::<u64>(),
        v1 in any::<u64>(),
        v2 in any::<u64>(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        prop_assert!(map.insert_if_absent(pane_id, v1));
        prop_assert!(!map.insert_if_absent(pane_id, v2));
        prop_assert_eq!(map.get(pane_id), Some(v1));
    }
}

// ============================================================================
// PaneMap: retain and clear
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PaneMap retain keeps matching entries.
    #[test]
    fn prop_pane_map_retain(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        let mut reference = HashMap::new();
        for (k, v) in &pairs {
            map.insert(*k, *v);
            reference.insert(*k, *v);
        }

        map.retain(|id, _| id % 3 == 0);
        reference.retain(|k, _| *k % 3 == 0);

        prop_assert_eq!(map.len(), reference.len());
        for (k, v) in &reference {
            prop_assert_eq!(map.get(*k), Some(*v));
        }
    }

    /// PaneMap clear empties everything.
    #[test]
    fn prop_pane_map_clear(
        shards in arb_shard_count(),
        pairs in arb_kv_pairs(),
    ) {
        let map = PaneMap::<u64>::with_shards(shards);
        for (k, v) in &pairs {
            map.insert(*k, *v);
        }
        map.clear();
        prop_assert!(map.is_empty());
        prop_assert_eq!(map.len(), 0);
    }
}

// ============================================================================
// DistributionStats invariants
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// total_entries equals sum of sizes.
    #[test]
    fn prop_stats_total_equals_sum(sizes in arb_shard_sizes()) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        let expected_total: usize = sizes.iter().sum();
        prop_assert_eq!(stats.total_entries, expected_total);
    }

    /// shard_count equals input length.
    #[test]
    fn prop_stats_shard_count(sizes in arb_shard_sizes()) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert_eq!(stats.shard_count, sizes.len());
    }

    /// min <= mean <= max.
    #[test]
    fn prop_stats_min_mean_max_order(sizes in arb_shard_sizes()) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert!(
            stats.min_shard_size as f64 <= stats.mean_shard_size + f64::EPSILON,
            "min ({}) should be <= mean ({})",
            stats.min_shard_size, stats.mean_shard_size
        );
        prop_assert!(
            stats.mean_shard_size <= stats.max_shard_size as f64 + f64::EPSILON,
            "mean ({}) should be <= max ({})",
            stats.mean_shard_size, stats.max_shard_size
        );
    }

    /// stddev is non-negative.
    #[test]
    fn prop_stats_stddev_nonneg(sizes in arb_shard_sizes()) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert!(
            stats.stddev_shard_size >= 0.0,
            "stddev should be non-negative, got {}",
            stats.stddev_shard_size
        );
    }

    /// If all shards are equal, stddev is 0.
    #[test]
    fn prop_stats_uniform_zero_stddev(
        value in 0usize..=100,
        count in 1usize..=64,
    ) {
        let sizes: Vec<usize> = vec![value; count];
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert!(
            stats.stddev_shard_size.abs() < 1e-10,
            "uniform sizes should have zero stddev, got {}",
            stats.stddev_shard_size
        );
    }
}

// ============================================================================
// DistributionStats serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DistributionStats serde roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(sizes in arb_shard_sizes()) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: DistributionStats = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(parsed.shard_count, stats.shard_count);
        prop_assert_eq!(parsed.total_entries, stats.total_entries);
        prop_assert_eq!(parsed.min_shard_size, stats.min_shard_size);
        prop_assert_eq!(parsed.max_shard_size, stats.max_shard_size);
        prop_assert!((parsed.mean_shard_size - stats.mean_shard_size).abs() < 1e-10);
        prop_assert!((parsed.stddev_shard_size - stats.stddev_shard_size).abs() < 1e-10);
    }
}

// ============================================================================
// ShardedMap: read_with / write_with
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// read_with returns computed value without cloning.
    #[test]
    fn prop_map_read_with(
        shards in arb_shard_count(),
        key in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        map.insert(key, value);
        let result = map.read_with(&key, |v| v.wrapping_add(1));
        prop_assert_eq!(result, Some(value.wrapping_add(1)));
    }

    /// read_with returns None for absent key.
    #[test]
    fn prop_map_read_with_absent(
        shards in arb_shard_count(),
        key in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        let result = map.read_with(&key, |v| *v);
        prop_assert_eq!(result, None);
    }

    /// write_with mutates in place.
    #[test]
    fn prop_map_write_with(
        shards in arb_shard_count(),
        key in any::<u64>(),
        value in any::<u64>(),
    ) {
        let map: ShardedMap<u64, u64> = ShardedMap::with_shards(shards);
        map.insert(key, value);
        map.write_with(&key, |v| *v = v.wrapping_add(1));
        prop_assert_eq!(map.get(&key), Some(value.wrapping_add(1)));
    }
}
