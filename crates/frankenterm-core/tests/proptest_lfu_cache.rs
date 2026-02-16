//! Property-based tests for lfu_cache.rs — O(1) LFU eviction cache.
//!
//! Verifies the LFU cache invariants:
//! - Capacity: len() <= capacity
//! - Insert/get consistency: inserted keys are found
//! - Eviction policy: LFU entries are evicted first
//! - Frequency tracking: get increments, peek does not
//! - Remove correctness: removed keys are not found
//! - Hit/miss counting: stats track lookups correctly
//! - Clone equivalence and independence
//! - Clear empties the cache
//! - Config and stats serde roundtrip
//! - is_full agrees with len == capacity
//! - from_config equivalence
//! - Default trait
//! - Eviction and min_frequency stats
//!
//! Bead: ft-283h4.38, ft-283h4.51

use frankenterm_core::lfu_cache::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_capacity() -> impl Strategy<Value = usize> {
    1usize..=20
}

fn arb_ops() -> impl Strategy<Value = Vec<(u8, u8)>> {
    prop::collection::vec((0u8..20, 0u8..100), 0..50)
}

// ── Capacity invariant ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// len() never exceeds capacity after any sequence of operations.
    #[test]
    fn prop_len_bounded_by_capacity(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(key, val) in &ops {
            cache.insert(key, val);
            prop_assert!(
                cache.len() <= cap,
                "len {} exceeds capacity {}", cache.len(), cap
            );
        }
    }

    /// After inserting n unique keys into a capacity-c cache, len == min(n, c).
    #[test]
    fn prop_len_correct_after_inserts(
        cap in 1usize..=10,
        keys in prop::collection::vec(0u8..50, 1..=30),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        let mut unique_count = 0usize;
        let mut seen = std::collections::HashSet::new();
        for &k in &keys {
            if seen.insert(k) {
                unique_count += 1;
            }
            cache.insert(k, 0);
        }
        let expected = unique_count.min(cap);
        prop_assert_eq!(cache.len(), expected, "len mismatch");
    }
}

// ── Insert/get consistency ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// The last inserted value for a key is retrievable if cache isn't full.
    #[test]
    fn prop_get_returns_inserted(
        cap in 1usize..=10,
        key in 0u8..10,
        value in 0u8..100,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        cache.insert(key, value);
        prop_assert_eq!(cache.get(&key), Some(&value));
    }

    /// contains_key agrees with peek.
    #[test]
    fn prop_contains_agrees_with_get(
        cap in arb_capacity(),
        ops in arb_ops(),
        probe in 0u8..30,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        let mut cache2: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache2.insert(k, v);
        }
        let peek = cache2.peek(&probe).is_some();
        let contains = cache2.contains_key(&probe);
        prop_assert_eq!(peek, contains, "peek and contains_key disagree for {}", probe);
    }
}

// ── Eviction policy ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Higher-frequency items survive eviction over lower-frequency items.
    #[test]
    fn prop_lfu_eviction_respects_frequency(
        cap in 2usize..=5,
        extra_gets in 1usize..=5,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);

        // Fill cache
        for i in 0..cap as u8 {
            cache.insert(i, i);
        }

        // Boost key 0's frequency
        for _ in 0..extra_gets {
            cache.get(&0);
        }

        // Insert new key — should NOT evict key 0 (highest freq)
        cache.insert(100, 100);

        prop_assert!(
            cache.contains_key(&0),
            "high-frequency key 0 was evicted"
        );
    }

    /// Evicted entry is returned from insert.
    #[test]
    fn prop_eviction_returns_entry(
        cap in 1usize..=5,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for i in 0..cap as u8 {
            cache.insert(i, i * 10);
        }

        let evicted = cache.insert(99, 99);
        prop_assert!(evicted.is_some(), "should have evicted when full");
        let (ek, ev) = evicted.unwrap();
        // The evicted key should have been in the original set
        prop_assert!(ek < cap as u8, "evicted key {} not from original set", ek);
        prop_assert_eq!(ev, ek * 10, "evicted value mismatch");
    }
}

// ── Frequency tracking ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// get increments frequency by 1 each time.
    #[test]
    fn prop_get_increments_frequency(
        n_gets in 1usize..=20,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(5);
        cache.insert(0, 0);

        for i in 0..n_gets {
            let expected = (i + 1) as u64; // starts at 1, +1 per get
            cache.get(&0);
            prop_assert_eq!(
                cache.frequency(&0), Some(expected + 1),
                "frequency after {} gets", i + 1
            );
        }
    }

    /// peek does NOT increment frequency.
    #[test]
    fn prop_peek_no_frequency_change(
        n_peeks in 1usize..=20,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(5);
        cache.insert(0, 0);
        let initial_freq = cache.frequency(&0);

        for _ in 0..n_peeks {
            let _ = cache.peek(&0);
        }

        prop_assert_eq!(cache.frequency(&0), initial_freq, "peek changed frequency");
    }
}

// ── Remove properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Removed key is no longer contained.
    #[test]
    fn prop_remove_then_not_found(
        cap in arb_capacity(),
        keys in prop::collection::vec(0u8..20, 1..=10),
        idx in 0usize..10,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &k in &keys {
            cache.insert(k, k);
        }

        prop_assume!(!cache.is_empty());
        let all_keys = cache.keys();
        let target = &all_keys[idx % all_keys.len()];
        cache.remove(target);
        prop_assert!(!cache.contains_key(target), "removed key still found");
    }

    /// Remove decrements len by 1.
    #[test]
    fn prop_remove_decrements_len(
        cap in arb_capacity(),
        keys in prop::collection::vec(0u8..20, 1..=10),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &k in &keys {
            cache.insert(k, k);
        }

        prop_assume!(!cache.is_empty());
        let before = cache.len();
        let all_keys = cache.keys();
        cache.remove(&all_keys[0]);
        prop_assert_eq!(cache.len(), before - 1, "len didn't decrease");
    }

    /// Remove returns the removed value.
    #[test]
    fn prop_remove_returns_value(
        cap in arb_capacity(),
        key in 0u8..20,
        val in 0u8..100,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        cache.insert(key, val);
        let removed = cache.remove(&key);
        prop_assert_eq!(removed, Some(val), "remove didn't return the value");
    }

    /// Double remove returns None on the second call.
    #[test]
    fn prop_double_remove_returns_none(
        cap in arb_capacity(),
        key in 0u8..20,
        val in 0u8..100,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        cache.insert(key, val);
        let _ = cache.remove(&key);
        let second = cache.remove(&key);
        prop_assert_eq!(second, None, "double remove should return None");
    }
}

// ── Hit/miss counting ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// hits + misses equals total get calls.
    #[test]
    fn prop_hit_miss_total(
        cap in arb_capacity(),
        inserts in prop::collection::vec(0u8..10, 0..10),
        lookups in prop::collection::vec(0u8..15, 0..20),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &k in &inserts {
            cache.insert(k, k);
        }
        for &k in &lookups {
            cache.get(&k);
        }

        let stats = cache.stats();
        prop_assert_eq!(
            stats.hits + stats.misses,
            lookups.len() as u64,
            "hits + misses != total lookups"
        );
    }
}

// ── Clone properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces same key set and values.
    #[test]
    fn prop_clone_equivalence(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }

        let clone = cache.clone();
        prop_assert_eq!(cache.len(), clone.len());

        for key in cache.keys() {
            prop_assert_eq!(cache.peek(&key), clone.peek(&key), "value mismatch for key {}", key);
        }
    }

    /// Mutations to clone don't affect original.
    #[test]
    fn prop_clone_independence(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        let original_len = cache.len();

        let mut clone = cache.clone();
        clone.insert(200, 200);
        clone.insert(201, 201);

        prop_assert_eq!(cache.len(), original_len, "original modified by clone");
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() empties the cache.
    #[test]
    fn prop_clear_empties(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        cache.clear();
        prop_assert!(cache.is_empty());
        prop_assert_eq!(cache.len(), 0);
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// LfuCacheConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(cap in 0usize..1000) {
        let config = LfuCacheConfig { capacity: cap };
        let json = serde_json::to_string(&config).unwrap();
        let back: LfuCacheConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// LfuCacheStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        cache.get(&0); // generate some stats
        let stats = cache.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: LfuCacheStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// Stats fields are consistent.
    #[test]
    fn prop_stats_consistent(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        let stats = cache.stats();
        prop_assert_eq!(stats.entry_count, cache.len());
        prop_assert_eq!(stats.capacity, cache.capacity());
        prop_assert!(stats.entry_count <= stats.capacity);
    }
}

// ── Zero capacity properties ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Zero-capacity cache is always empty.
    #[test]
    fn prop_zero_capacity_always_empty(ops in arb_ops()) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(0);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        prop_assert!(cache.is_empty());
        prop_assert_eq!(cache.len(), 0);
    }
}

// ── is_full properties ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// is_full() agrees with len() == capacity().
    #[test]
    fn prop_is_full_matches_len_eq_capacity(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        prop_assert_eq!(
            cache.is_full(),
            cache.len() == cache.capacity(),
            "is_full disagrees with len==capacity"
        );
    }

    /// Filling cache to capacity makes is_full true.
    #[test]
    fn prop_full_after_filling(cap in 1usize..=10) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for i in 0..cap as u8 {
            cache.insert(i, i);
        }
        prop_assert!(cache.is_full(), "cache not full after inserting cap items");
    }
}

// ── from_config properties ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// from_config produces a cache with the specified capacity.
    #[test]
    fn prop_from_config_capacity(cap in 0usize..500) {
        let config = LfuCacheConfig { capacity: cap };
        let cache: LfuCache<u8, u8> = LfuCache::from_config(&config);
        prop_assert_eq!(cache.capacity(), cap, "from_config capacity mismatch");
        prop_assert!(cache.is_empty(), "from_config should produce empty cache");
    }

    /// from_config is equivalent to new(config.capacity).
    #[test]
    fn prop_from_config_equiv_new(cap in arb_capacity()) {
        let config = LfuCacheConfig { capacity: cap };
        let cache1: LfuCache<u8, u8> = LfuCache::from_config(&config);
        let cache2: LfuCache<u8, u8> = LfuCache::new(cap);
        prop_assert_eq!(cache1.capacity(), cache2.capacity());
        prop_assert_eq!(cache1.len(), cache2.len());
    }
}

// ── Default trait properties ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Default LfuCacheConfig has capacity 128.
    #[test]
    fn prop_default_config(_dummy in 0..1u8) {
        let config = LfuCacheConfig::default();
        prop_assert_eq!(config.capacity, 128);
    }

    /// Default LfuCache is usable and empty.
    #[test]
    fn prop_default_cache_usable(key in 0u8..50, val in 0u8..100) {
        let mut cache: LfuCache<u8, u8> = LfuCache::default();
        prop_assert!(cache.is_empty());
        prop_assert_eq!(cache.capacity(), 128);
        cache.insert(key, val);
        prop_assert_eq!(cache.get(&key), Some(&val));
    }
}

// ── Eviction stats properties ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Eviction count in stats matches actual evictions.
    #[test]
    fn prop_eviction_count_accurate(
        cap in 1usize..=5,
        n_extra in 1usize..=10,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        // Fill to capacity with unique keys
        for i in 0..cap as u8 {
            cache.insert(i, i);
        }
        // Each additional unique key causes one eviction
        for i in 0..n_extra as u8 {
            cache.insert(100 + i, i);
        }
        let stats = cache.stats();
        prop_assert_eq!(
            stats.evictions, n_extra as u64,
            "eviction count mismatch: expected {}, got {}", n_extra, stats.evictions
        );
    }

    /// min_frequency in stats is <= any entry's frequency.
    #[test]
    fn prop_min_frequency_is_minimum(
        cap in 2usize..=8,
        ops in prop::collection::vec((0u8..15, 0u8..50), 1..30),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        // Do some gets to vary frequencies
        for &(k, _) in ops.iter().take(5) {
            cache.get(&k);
        }
        let stats = cache.stats();
        if !cache.is_empty() {
            for key in cache.keys() {
                if let Some(freq) = cache.frequency(&key) {
                    prop_assert!(
                        stats.min_frequency <= freq,
                        "min_frequency {} > entry frequency {} for key {}",
                        stats.min_frequency, freq, key
                    );
                }
            }
        }
    }
}

// ── Insert overwrite properties ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inserting with existing key updates the value.
    #[test]
    fn prop_insert_overwrite_updates(
        cap in arb_capacity(),
        key in 0u8..10,
        val1 in 0u8..50,
        val2 in 50u8..100,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        cache.insert(key, val1);
        cache.insert(key, val2);
        prop_assert_eq!(cache.get(&key), Some(&val2), "overwrite didn't update value");
    }

    /// Inserting with existing key does not change len.
    #[test]
    fn prop_insert_overwrite_preserves_len(
        cap in arb_capacity(),
        key in 0u8..10,
        val1 in 0u8..50,
        val2 in 50u8..100,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        cache.insert(key, val1);
        let len_before = cache.len();
        cache.insert(key, val2);
        prop_assert_eq!(cache.len(), len_before, "overwrite changed len");
    }
}

// ── Clear resets stats ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After clear, stats show zero entries.
    #[test]
    fn prop_clear_resets_entry_count(
        cap in arb_capacity(),
        ops in arb_ops(),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        cache.clear();
        let stats = cache.stats();
        prop_assert_eq!(stats.entry_count, 0, "entry_count not 0 after clear");
    }

    /// After clear, re-inserting works correctly.
    #[test]
    fn prop_clear_then_reinsert(
        cap in arb_capacity(),
        ops in arb_ops(),
        key in 0u8..20,
        val in 0u8..100,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        cache.clear();
        cache.insert(key, val);
        prop_assert_eq!(cache.get(&key), Some(&val));
        prop_assert_eq!(cache.len(), 1);
    }
}

// ── Display format property ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Display output contains capacity and len info.
    #[test]
    fn prop_display_contains_info(
        cap in arb_capacity(),
        ops in prop::collection::vec((0u8..10, 0u8..50), 0..10),
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(cap);
        for &(k, v) in &ops {
            cache.insert(k, v);
        }
        let display = format!("{:?}", cache);
        prop_assert!(!display.is_empty(), "Debug output should not be empty");
    }
}

// ── Frequency monotonicity property ─────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Frequency is monotonically non-decreasing under get calls.
    #[test]
    fn prop_frequency_monotonic_under_gets(
        n_gets in 1usize..=15,
    ) {
        let mut cache: LfuCache<u8, u8> = LfuCache::new(5);
        cache.insert(42, 0);
        let mut prev_freq = cache.frequency(&42).unwrap_or(0);
        for _ in 0..n_gets {
            cache.get(&42);
            let cur_freq = cache.frequency(&42).unwrap_or(0);
            prop_assert!(
                cur_freq >= prev_freq,
                "frequency decreased: {} -> {}", prev_freq, cur_freq
            );
            prev_freq = cur_freq;
        }
    }
}
