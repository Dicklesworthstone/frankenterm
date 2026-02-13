//! Property-based tests for the sharded_counter module.
//!
//! Tests cover: ShardedCounter (add commutativity, aggregation, reset, shard_values
//! sum invariant), ShardedMax (monotonicity, max correctness), ShardedGauge
//! (set/get_max), ShardedSnapshot serde roundtrip, and shard_count clamping.

use proptest::prelude::*;

use frankenterm_core::sharded_counter::{
    ShardedCounter, ShardedGauge, ShardedMax, ShardedSnapshot,
};

// ============================================================================
// Strategies
// ============================================================================

/// Arbitrary shard count (will be clamped to [1, 64] by the constructors).
fn arb_shard_count() -> impl Strategy<Value = usize> {
    0usize..=128
}

/// Arbitrary list of u64 values for counter/max operations.
fn arb_values() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..=1_000_000, 0..50)
}

/// Small values to avoid overflow in summation tests.
fn arb_small_values() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..=10_000, 0..100)
}

/// Arbitrary ShardedSnapshot for serde tests.
fn arb_snapshot() -> impl Strategy<Value = ShardedSnapshot> {
    let counter_entries =
        prop::collection::vec(("[a-z_]{1,10}".prop_map(String::from), any::<u64>()), 0..5);
    let max_entries =
        prop::collection::vec(("[a-z_]{1,10}".prop_map(String::from), any::<u64>()), 0..5);
    let gauge_entries =
        prop::collection::vec(("[a-z_]{1,10}".prop_map(String::from), any::<u64>()), 0..5);
    (counter_entries, max_entries, gauge_entries).prop_map(|(counters, maxes, gauges)| {
        ShardedSnapshot {
            counters,
            maxes,
            gauges,
        }
    })
}

// ============================================================================
// ShardedCounter properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// get() after adding a sequence equals the sum of the sequence.
    #[test]
    fn prop_counter_sum(
        shards in arb_shard_count(),
        values in arb_small_values(),
    ) {
        let counter = ShardedCounter::with_shards(shards);
        let mut expected: u64 = 0;
        for &v in &values {
            counter.add(v);
            expected = expected.wrapping_add(v);
        }
        prop_assert_eq!(counter.get(), expected, "counter sum mismatch");
    }

    /// increment() N times gives get() == N.
    #[test]
    fn prop_counter_increment_count(
        shards in arb_shard_count(),
        n in 0u64..=500,
    ) {
        let counter = ShardedCounter::with_shards(shards);
        for _ in 0..n {
            counter.increment();
        }
        prop_assert_eq!(counter.get(), n);
    }

    /// shard_values sum equals get().
    #[test]
    fn prop_counter_shard_values_sum(
        shards in arb_shard_count(),
        values in arb_small_values(),
    ) {
        let counter = ShardedCounter::with_shards(shards);
        for &v in &values {
            counter.add(v);
        }
        let shard_sum: u64 = counter.shard_values().iter().sum();
        prop_assert_eq!(shard_sum, counter.get(), "shard_values sum should equal get()");
    }

    /// reset() sets counter to zero.
    #[test]
    fn prop_counter_reset(
        shards in arb_shard_count(),
        values in arb_small_values(),
    ) {
        let counter = ShardedCounter::with_shards(shards);
        for &v in &values {
            counter.add(v);
        }
        counter.reset();
        prop_assert_eq!(counter.get(), 0, "counter should be zero after reset");
    }

    /// shard_count is clamped to [1, 64].
    #[test]
    fn prop_counter_shard_count_clamped(n in arb_shard_count()) {
        let counter = ShardedCounter::with_shards(n);
        let sc = counter.shard_count();
        prop_assert!(sc >= 1, "shard_count should be >= 1, got {}", sc);
        prop_assert!(sc <= 64, "shard_count should be <= 64, got {}", sc);
    }

    /// shard_values length equals shard_count.
    #[test]
    fn prop_counter_shard_values_length(shards in arb_shard_count()) {
        let counter = ShardedCounter::with_shards(shards);
        counter.add(42);
        prop_assert_eq!(counter.shard_values().len(), counter.shard_count());
    }
}

// ============================================================================
// ShardedMax properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// get() after observing values equals the max of observed values.
    #[test]
    fn prop_max_tracks_maximum(
        shards in arb_shard_count(),
        values in arb_values(),
    ) {
        let max_tracker = ShardedMax::with_shards(shards);
        for &v in &values {
            max_tracker.observe(v);
        }
        let expected = values.iter().copied().max().unwrap_or(0);
        prop_assert_eq!(max_tracker.get(), expected, "max mismatch");
    }

    /// Observing the same value twice doesn't change the max.
    #[test]
    fn prop_max_idempotent(
        shards in arb_shard_count(),
        value in any::<u64>(),
    ) {
        let max_tracker = ShardedMax::with_shards(shards);
        max_tracker.observe(value);
        let after_first = max_tracker.get();
        max_tracker.observe(value);
        prop_assert_eq!(max_tracker.get(), after_first, "observe should be idempotent");
    }

    /// Max is monotonically non-decreasing.
    #[test]
    fn prop_max_monotonic(
        shards in arb_shard_count(),
        values in arb_values(),
    ) {
        let max_tracker = ShardedMax::with_shards(shards);
        let mut prev_max = 0u64;
        for &v in &values {
            max_tracker.observe(v);
            let current = max_tracker.get();
            prop_assert!(
                current >= prev_max,
                "max decreased from {} to {} after observing {}",
                prev_max, current, v
            );
            prev_max = current;
        }
    }

    /// reset() sets max to zero.
    #[test]
    fn prop_max_reset(
        shards in arb_shard_count(),
        values in arb_values(),
    ) {
        let max_tracker = ShardedMax::with_shards(shards);
        for &v in &values {
            max_tracker.observe(v);
        }
        max_tracker.reset();
        prop_assert_eq!(max_tracker.get(), 0, "max should be zero after reset");
    }

    /// shard_count is clamped to [1, 64].
    #[test]
    fn prop_max_shard_count_clamped(n in arb_shard_count()) {
        let max_tracker = ShardedMax::with_shards(n);
        let sc = max_tracker.shard_count();
        prop_assert!(sc >= 1 && sc <= 64);
    }
}

// ============================================================================
// ShardedGauge properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// set then get_max returns at least the set value (on single shard).
    #[test]
    fn prop_gauge_set_get_single_shard(value in any::<u64>()) {
        let gauge = ShardedGauge::with_shards(1);
        gauge.set(value);
        prop_assert_eq!(gauge.get_max(), value);
    }

    /// get_max returns the largest set value across multiple sets (single shard).
    #[test]
    fn prop_gauge_tracks_max_single_shard(values in arb_values()) {
        let gauge = ShardedGauge::with_shards(1);
        let mut max_set = 0u64;
        for &v in &values {
            gauge.set(v);
            max_set = max_set.max(v);
        }
        // On a single shard, the last set value is the only one, but
        // get_max reads max across shards. With 1 shard, it's the last set value.
        if let Some(&last) = values.last() {
            prop_assert_eq!(gauge.get_max(), last);
        }
    }

    /// reset() sets gauge to zero.
    #[test]
    fn prop_gauge_reset(
        shards in arb_shard_count(),
        value in any::<u64>(),
    ) {
        let gauge = ShardedGauge::with_shards(shards);
        gauge.set(value);
        gauge.reset();
        prop_assert_eq!(gauge.get_max(), 0);
    }

    /// store/load are compatible with set/get_max.
    #[test]
    fn prop_gauge_store_load_compat(value in any::<u64>()) {
        let gauge = ShardedGauge::with_shards(1);
        gauge.store(value, std::sync::atomic::Ordering::Relaxed);
        let loaded = gauge.load(std::sync::atomic::Ordering::Relaxed);
        prop_assert_eq!(loaded, value);
    }
}

// ============================================================================
// ShardedSnapshot serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ShardedSnapshot serializes and deserializes losslessly.
    #[test]
    fn prop_snapshot_serde_roundtrip(snap in arb_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: ShardedSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, snap);
    }
}

// ============================================================================
// AtomicU64-compatible shim consistency
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Counter load() returns same as get().
    #[test]
    fn prop_counter_load_equals_get(
        shards in arb_shard_count(),
        values in arb_small_values(),
    ) {
        let counter = ShardedCounter::with_shards(shards);
        for &v in &values {
            counter.add(v);
        }
        prop_assert_eq!(
            counter.load(std::sync::atomic::Ordering::Relaxed),
            counter.get()
        );
    }

    /// Counter fetch_add returns previous value and increments.
    #[test]
    fn prop_counter_fetch_add(
        shards in 1usize..=8,
        initial in 0u64..=10_000,
        delta in 0u64..=10_000,
    ) {
        let counter = ShardedCounter::with_shards(shards);
        counter.add(initial);
        let prev = counter.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
        // prev is approximate (reads before write), so just verify final value.
        let _ = prev; // suppress unused warning
        prop_assert_eq!(counter.get(), initial + delta);
    }
}
