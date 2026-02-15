//! Property-based tests for cuckoo_filter.rs — probabilistic set with deletion.
//!
//! Bead: ft-283h4.18

use frankenterm_core::cuckoo_filter::*;
use proptest::prelude::*;
use std::collections::HashSet;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_config() -> impl Strategy<Value = CuckooConfig> {
    (2..64usize, 2..8usize, 10..200usize).prop_map(|(buckets, bsize, kicks)| CuckooConfig {
        num_buckets: buckets,
        bucket_size: bsize,
        max_kicks: kicks,
    })
}

fn arb_large_config() -> impl Strategy<Value = CuckooConfig> {
    (64..512usize, 2..8usize, 50..500usize).prop_map(|(buckets, bsize, kicks)| CuckooConfig {
        num_buckets: buckets,
        bucket_size: bsize,
        max_kicks: kicks,
    })
}

fn arb_items() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0..100000u64, 1..50)
}

fn arb_small_items() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0..1000u64, 1..20)
}

// ── Configuration properties ────────────────────────────────────────

proptest! {
    /// CuckooConfig serde roundtrip.
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: CuckooConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// CuckooStats serde roundtrip.
    #[test]
    fn stats_serde_roundtrip(
        capacity in 1..10000usize,
        count in 0..5000usize,
        num_buckets in 1..1000usize,
        bucket_size in 1..8usize,
        occupied in 0..1000usize
    ) {
        let load = if capacity > 0 {
            ((count.min(capacity) as f64 / capacity as f64) * 100.0) as u32
        } else { 0 };
        let stats = CuckooStats {
            capacity,
            count,
            load_percent: load,
            num_buckets,
            bucket_size,
            occupied_buckets: occupied,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: CuckooStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// num_buckets is always a power of 2.
    #[test]
    fn num_buckets_power_of_two(config in arb_config()) {
        let filter = CuckooFilter::with_config(config);
        prop_assert!(
            filter.num_buckets().is_power_of_two(),
            "num_buckets should be power of 2, got {}", filter.num_buckets()
        );
    }

    /// num_buckets >= configured value.
    #[test]
    fn num_buckets_ge_config(config in arb_config()) {
        let filter = CuckooFilter::with_config(config.clone());
        prop_assert!(
            filter.num_buckets() >= config.num_buckets,
            "num_buckets {} < configured {}", filter.num_buckets(), config.num_buckets
        );
    }

    /// capacity = num_buckets * bucket_size.
    #[test]
    fn capacity_correct(config in arb_config()) {
        let filter = CuckooFilter::with_config(config);
        prop_assert_eq!(
            filter.capacity(),
            filter.num_buckets() * filter.bucket_size()
        );
    }
}

// ── Insert properties ───────────────────────────────────────────────

proptest! {
    /// Successfully inserted items can be looked up.
    #[test]
    fn insert_then_lookup(
        config in arb_large_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        let mut inserted = Vec::new();

        for item in &items {
            if filter.insert(item) == InsertResult::Ok {
                inserted.push(*item);
            }
        }

        for item in &inserted {
            prop_assert!(
                filter.lookup(item),
                "inserted item {} should be found", item
            );
        }
    }

    /// Count matches number of successful insertions.
    #[test]
    fn count_matches_inserts(
        config in arb_large_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        let mut successes = 0;

        for item in &items {
            if filter.insert(item) == InsertResult::Ok {
                successes += 1;
            }
        }

        prop_assert_eq!(
            filter.count(), successes,
            "count should match number of successful inserts"
        );
    }

    /// Count never exceeds capacity.
    #[test]
    fn count_le_capacity(
        config in arb_config(),
        items in arb_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }
        prop_assert!(
            filter.count() <= filter.capacity(),
            "count {} exceeds capacity {}", filter.count(), filter.capacity()
        );
    }

    /// Empty filter has count 0 and is_empty.
    #[test]
    fn new_filter_is_empty(config in arb_config()) {
        let filter = CuckooFilter::with_config(config);
        prop_assert!(filter.is_empty());
        prop_assert_eq!(filter.count(), 0);
    }

    /// with_capacity creates filter with at least that capacity.
    #[test]
    fn with_capacity_sufficient(expected in 10..5000usize) {
        let filter = CuckooFilter::with_capacity(expected);
        prop_assert!(
            filter.capacity() >= expected,
            "capacity {} < expected {}", filter.capacity(), expected
        );
    }
}

// ── Lookup properties ───────────────────────────────────────────────

proptest! {
    /// No false negatives: lookup always finds inserted items.
    #[test]
    fn no_false_negatives(
        items in prop::collection::vec(0..100000u64, 1..30)
    ) {
        let mut filter = CuckooFilter::with_capacity(100);
        let mut inserted = HashSet::new();

        for item in &items {
            if filter.insert(item) == InsertResult::Ok {
                inserted.insert(*item);
            }
        }

        for item in &inserted {
            prop_assert!(
                filter.lookup(item),
                "false negative: inserted item {} not found", item
            );
        }
    }

    /// Lookup on empty filter always returns false.
    #[test]
    fn lookup_empty_always_false(item in any::<u64>()) {
        let filter = CuckooFilter::new();
        prop_assert!(!filter.lookup(&item));
    }
}

// ── Delete properties ───────────────────────────────────────────────

proptest! {
    /// Delete reduces count by 1.
    #[test]
    fn delete_decrements_count(
        items in prop::collection::vec(0..100000u64, 1..20)
    ) {
        let mut filter = CuckooFilter::with_capacity(100);
        let mut inserted = Vec::new();

        for item in &items {
            if filter.insert(item) == InsertResult::Ok {
                inserted.push(*item);
            }
        }

        for item in &inserted {
            let before = filter.count();
            let deleted = filter.delete(item);
            if deleted {
                prop_assert_eq!(
                    filter.count(), before - 1,
                    "count should decrease by 1 after delete"
                );
            }
        }
    }

    /// Deleted items are no longer found (when unique fingerprints).
    #[test]
    fn delete_removes_item(
        config in arb_large_config(),
        items in prop::collection::vec(0..100000u64, 1..10)
    ) {
        let mut filter = CuckooFilter::with_config(config);
        let unique: Vec<u64> = items.into_iter().collect::<HashSet<_>>().into_iter().collect();

        for item in &unique {
            filter.insert(item);
        }

        for item in &unique {
            filter.delete(item);
        }

        // After deleting all unique items, count should be 0
        prop_assert_eq!(
            filter.count(), 0,
            "count should be 0 after deleting all items"
        );
    }

    /// Delete of non-existent item returns false and doesn't change count.
    #[test]
    fn delete_nonexistent_noop(item in 0..100000u64) {
        let mut filter = CuckooFilter::with_capacity(100);
        let count_before = filter.count();
        let result = filter.delete(&item);
        prop_assert!(!result, "delete of non-existent should return false");
        prop_assert_eq!(filter.count(), count_before);
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    /// Clear empties the filter.
    #[test]
    fn clear_empties(
        config in arb_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }
        filter.clear();
        prop_assert!(filter.is_empty());
        prop_assert_eq!(filter.count(), 0);
    }

    /// After clear, previously inserted items are not found.
    #[test]
    fn clear_removes_all(
        items in prop::collection::vec(0..100000u64, 1..30)
    ) {
        let mut filter = CuckooFilter::with_capacity(100);
        for item in &items {
            filter.insert(item);
        }
        filter.clear();
        for item in &items {
            prop_assert!(!filter.lookup(item));
        }
    }
}

// ── Stats properties ────────────────────────────────────────────────

proptest! {
    /// Stats count matches filter count.
    #[test]
    fn stats_count_consistent(
        config in arb_large_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }
        let stats = filter.stats();
        prop_assert_eq!(stats.count, filter.count());
        prop_assert_eq!(stats.capacity, filter.capacity());
        prop_assert_eq!(stats.num_buckets, filter.num_buckets());
        prop_assert_eq!(stats.bucket_size, filter.bucket_size());
    }

    /// Load percent is bounded [0, 100].
    #[test]
    fn stats_load_percent_bounded(
        config in arb_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }
        let stats = filter.stats();
        prop_assert!(stats.load_percent <= 100);
    }

    /// Occupied buckets <= num_buckets.
    #[test]
    fn stats_occupied_bounded(
        config in arb_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }
        let stats = filter.stats();
        prop_assert!(
            stats.occupied_buckets <= stats.num_buckets,
            "occupied {} > num_buckets {}", stats.occupied_buckets, stats.num_buckets
        );
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// Insert then delete then insert maintains correct state.
    #[test]
    fn insert_delete_reinsert(
        items in prop::collection::vec(0..100000u64, 1..15)
    ) {
        let mut filter = CuckooFilter::with_capacity(100);
        let unique: Vec<u64> = items.into_iter().collect::<HashSet<_>>().into_iter().collect();

        // Insert all
        for item in &unique {
            filter.insert(item);
        }

        // Delete all
        for item in &unique {
            filter.delete(item);
        }
        prop_assert_eq!(filter.count(), 0);

        // Re-insert all
        for item in &unique {
            filter.insert(item);
        }

        // All should be found
        for item in &unique {
            prop_assert!(filter.lookup(item), "reinserted item {} not found", item);
        }
    }

    /// Load factor is in [0.0, 1.0].
    #[test]
    fn load_factor_bounded(
        config in arb_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }
        let lf = filter.load_factor();
        prop_assert!(lf >= 0.0 && lf <= 1.0, "load factor out of range: {}", lf);
    }

    /// Clone produces independent filter with same state.
    #[test]
    fn clone_independent(
        config in arb_large_config(),
        items in arb_small_items()
    ) {
        let mut filter = CuckooFilter::with_config(config);
        for item in &items {
            filter.insert(item);
        }

        let mut cloned = filter.clone();
        prop_assert_eq!(cloned.count(), filter.count());

        // Modify clone, original unchanged
        cloned.insert(&999999u64);
        if cloned.count() != filter.count() {
            // Clone modified independently
            prop_assert_eq!(cloned.count(), filter.count() + 1);
        }
    }

    /// Filter with no inserts has load_factor 0.
    #[test]
    fn empty_load_factor_zero(config in arb_config()) {
        let filter = CuckooFilter::with_config(config);
        prop_assert_eq!(filter.load_factor(), 0.0);
    }
}
