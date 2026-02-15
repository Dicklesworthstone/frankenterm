//! Property-based linearizability tests for lock-free data structures
//!
//! Verifies that ShardedCounter, ShardedMax, ShardedGauge, PaneMap, and
//! ShardedMap produce results consistent with a sequential execution of
//! the same operations.

use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use frankenterm_core::concurrent_map::{DistributionStats, PaneMap, ShardedMap};
use frankenterm_core::sharded_counter::{
    ShardedCounter, ShardedGauge, ShardedMax, ShardedSnapshot,
};

// ===========================================================================
// Strategies
// ===========================================================================

/// Operations on a counter.
#[derive(Debug, Clone)]
enum CounterOp {
    Add(u64),
    Reset,
    Read,
}

fn arb_counter_ops(max_len: usize) -> impl Strategy<Value = Vec<CounterOp>> {
    prop::collection::vec(
        prop_oneof![
            (1..1000u64).prop_map(CounterOp::Add),
            Just(CounterOp::Reset),
            Just(CounterOp::Read),
        ],
        1..max_len,
    )
}

/// Operations on a max tracker.
#[derive(Debug, Clone)]
enum MaxOp {
    Observe(u64),
    Reset,
    Read,
}

fn arb_max_ops(max_len: usize) -> impl Strategy<Value = Vec<MaxOp>> {
    prop::collection::vec(
        prop_oneof![
            (0..10000u64).prop_map(MaxOp::Observe),
            Just(MaxOp::Reset),
            Just(MaxOp::Read),
        ],
        1..max_len,
    )
}

/// Operations on a gauge.
#[derive(Debug, Clone)]
enum GaugeOp {
    Set(u64),
    Reset,
    ReadMax,
}

fn arb_gauge_ops(max_len: usize) -> impl Strategy<Value = Vec<GaugeOp>> {
    prop::collection::vec(
        prop_oneof![
            (0..10000u64).prop_map(GaugeOp::Set),
            Just(GaugeOp::Reset),
            Just(GaugeOp::ReadMax),
        ],
        1..max_len,
    )
}

/// Operations on a pane map.
#[derive(Debug, Clone)]
enum MapOp {
    Insert(u64, u64),
    Get(u64),
    Remove(u64),
    Contains(u64),
}

fn arb_map_ops(max_len: usize) -> impl Strategy<Value = Vec<MapOp>> {
    prop::collection::vec(
        prop_oneof![
            (0..50u64, 0..1000u64).prop_map(|(k, v)| MapOp::Insert(k, v)),
            (0..50u64).prop_map(MapOp::Get),
            (0..50u64).prop_map(MapOp::Remove),
            (0..50u64).prop_map(MapOp::Contains),
        ],
        1..max_len,
    )
}

/// Operations on a generic ShardedMap.
#[derive(Debug, Clone)]
enum GenericMapOp {
    Insert(String, u64),
    Get(String),
    Remove(String),
    ContainsKey(String),
}

fn arb_generic_map_ops(max_len: usize) -> impl Strategy<Value = Vec<GenericMapOp>> {
    let key_strat = prop_oneof![
        Just("alpha".to_string()),
        Just("beta".to_string()),
        Just("gamma".to_string()),
        Just("delta".to_string()),
        Just("epsilon".to_string()),
    ];
    prop::collection::vec(
        prop_oneof![
            (key_strat.clone(), 0..1000u64).prop_map(|(k, v)| GenericMapOp::Insert(k, v)),
            key_strat.clone().prop_map(GenericMapOp::Get),
            key_strat.clone().prop_map(GenericMapOp::Remove),
            key_strat.prop_map(GenericMapOp::ContainsKey),
        ],
        1..max_len,
    )
}

// ===========================================================================
// Sequential reference implementations
// ===========================================================================

fn sequential_counter(ops: &[CounterOp]) -> Vec<Option<u64>> {
    let mut val: u64 = 0;
    ops.iter()
        .map(|op| match op {
            CounterOp::Add(v) => {
                val = val.wrapping_add(*v);
                None
            }
            CounterOp::Reset => {
                val = 0;
                None
            }
            CounterOp::Read => Some(val),
        })
        .collect()
}

fn sequential_max(ops: &[MaxOp]) -> Vec<Option<u64>> {
    let mut val: u64 = 0;
    ops.iter()
        .map(|op| match op {
            MaxOp::Observe(v) => {
                val = val.max(*v);
                None
            }
            MaxOp::Reset => {
                val = 0;
                None
            }
            MaxOp::Read => Some(val),
        })
        .collect()
}

fn sequential_gauge(ops: &[GaugeOp]) -> Vec<Option<u64>> {
    let mut val: u64 = 0;
    ops.iter()
        .map(|op| match op {
            GaugeOp::Set(v) => {
                // Single-threaded: set is just an update; get_max returns max across shards
                // but with one thread, max == last set (since only one shard is written)
                val = *v;
                None
            }
            GaugeOp::Reset => {
                val = 0;
                None
            }
            // In single-thread: get_max returns the last set value
            GaugeOp::ReadMax => Some(val),
        })
        .collect()
}

fn sequential_map(ops: &[MapOp]) -> Vec<Option<u64>> {
    let mut map: HashMap<u64, u64> = HashMap::new();
    ops.iter()
        .map(|op| match op {
            MapOp::Insert(k, v) => {
                map.insert(*k, *v);
                None
            }
            MapOp::Get(k) => map.get(k).copied().or(Some(u64::MAX)), // sentinel for None
            MapOp::Remove(k) => {
                map.remove(k);
                None
            }
            MapOp::Contains(k) => Some(u64::from(map.contains_key(k))),
        })
        .collect()
}

fn sequential_generic_map(ops: &[GenericMapOp]) -> Vec<Option<u64>> {
    let mut map: HashMap<String, u64> = HashMap::new();
    ops.iter()
        .map(|op| match op {
            GenericMapOp::Insert(k, v) => {
                map.insert(k.clone(), *v);
                None
            }
            GenericMapOp::Get(k) => map.get(k).copied().or(Some(u64::MAX)),
            GenericMapOp::Remove(k) => {
                map.remove(k);
                None
            }
            GenericMapOp::ContainsKey(k) => Some(u64::from(map.contains_key(k))),
        })
        .collect()
}

// ===========================================================================
// Property tests -- sequential linearizability
// ===========================================================================

proptest! {
    /// ShardedCounter sequential linearizability: applying ops sequentially
    /// on the sharded counter produces the same results as a simple u64.
    #[test]
    fn sharded_counter_linearizable(ops in arb_counter_ops(200)) {
        let counter = ShardedCounter::with_shards(4);
        let expected = sequential_counter(&ops);

        let mut actual: Vec<Option<u64>> = Vec::new();
        for op in &ops {
            match op {
                CounterOp::Add(v) => {
                    counter.add(*v);
                    actual.push(None);
                }
                CounterOp::Reset => {
                    counter.reset();
                    actual.push(None);
                }
                CounterOp::Read => {
                    actual.push(Some(counter.get()));
                }
            }
        }

        // Compare read results
        for (i, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
            if let (Some(e), Some(a)) = (exp, act) {
                prop_assert_eq!(
                    e, a,
                    "Mismatch at op {}: expected {}, got {}", i, e, a
                );
            }
        }
    }

    /// ShardedMax sequential linearizability.
    #[test]
    fn sharded_max_linearizable(ops in arb_max_ops(200)) {
        let max = ShardedMax::with_shards(4);
        let expected = sequential_max(&ops);

        let mut actual: Vec<Option<u64>> = Vec::new();
        for op in &ops {
            match op {
                MaxOp::Observe(v) => {
                    max.observe(*v);
                    actual.push(None);
                }
                MaxOp::Reset => {
                    max.reset();
                    actual.push(None);
                }
                MaxOp::Read => {
                    actual.push(Some(max.get()));
                }
            }
        }

        for (i, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
            if let (Some(e), Some(a)) = (exp, act) {
                prop_assert_eq!(
                    e, a,
                    "Mismatch at op {}: expected {}, got {}", i, e, a
                );
            }
        }
    }

    /// ShardedGauge sequential linearizability: set/get_max/reset produce
    /// consistent results when single-threaded.
    #[test]
    fn sharded_gauge_linearizable(ops in arb_gauge_ops(200)) {
        let gauge = ShardedGauge::with_shards(4);
        let expected = sequential_gauge(&ops);

        let mut actual: Vec<Option<u64>> = Vec::new();
        for op in &ops {
            match op {
                GaugeOp::Set(v) => {
                    gauge.set(*v);
                    actual.push(None);
                }
                GaugeOp::Reset => {
                    gauge.reset();
                    actual.push(None);
                }
                GaugeOp::ReadMax => {
                    actual.push(Some(gauge.get_max()));
                }
            }
        }

        for (i, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
            if let (Some(e), Some(a)) = (exp, act) {
                prop_assert_eq!(
                    e, a,
                    "Gauge mismatch at op {}: expected {}, got {}", i, e, a
                );
            }
        }
    }

    /// PaneMap sequential linearizability.
    #[test]
    fn pane_map_linearizable(ops in arb_map_ops(200)) {
        let map = PaneMap::<u64>::with_shards(16);
        let expected = sequential_map(&ops);

        let mut actual: Vec<Option<u64>> = Vec::new();
        for op in &ops {
            match op {
                MapOp::Insert(k, v) => {
                    map.insert(*k, *v);
                    actual.push(None);
                }
                MapOp::Get(k) => {
                    let v = map.get(*k).unwrap_or(u64::MAX);
                    actual.push(Some(v));
                }
                MapOp::Remove(k) => {
                    map.remove(*k);
                    actual.push(None);
                }
                MapOp::Contains(k) => {
                    actual.push(Some(u64::from(map.contains(*k))));
                }
            }
        }

        for (i, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
            if let (Some(e), Some(a)) = (exp, act) {
                prop_assert_eq!(
                    e, a,
                    "Mismatch at op {}: expected {}, got {}", i, e, a
                );
            }
        }
    }

    /// ShardedMap sequential linearizability with String keys.
    #[test]
    fn sharded_map_linearizable(ops in arb_generic_map_ops(200)) {
        let map = ShardedMap::<String, u64>::with_shards(8);
        let expected = sequential_generic_map(&ops);

        let mut actual: Vec<Option<u64>> = Vec::new();
        for op in &ops {
            match op {
                GenericMapOp::Insert(k, v) => {
                    map.insert(k.clone(), *v);
                    actual.push(None);
                }
                GenericMapOp::Get(k) => {
                    let v = map.get(k).unwrap_or(u64::MAX);
                    actual.push(Some(v));
                }
                GenericMapOp::Remove(k) => {
                    map.remove(k);
                    actual.push(None);
                }
                GenericMapOp::ContainsKey(k) => {
                    actual.push(Some(u64::from(map.contains_key(k))));
                }
            }
        }

        for (i, (exp, act)) in expected.iter().zip(actual.iter()).enumerate() {
            if let (Some(e), Some(a)) = (exp, act) {
                prop_assert_eq!(
                    e, a,
                    "ShardedMap mismatch at op {}: expected {}, got {}", i, e, a
                );
            }
        }
    }
}

// ===========================================================================
// Property tests -- concurrent correctness
// ===========================================================================

proptest! {
    /// ShardedCounter concurrent correctness: N threads adding known values
    /// must produce the exact expected sum.
    #[test]
    fn counter_concurrent_sum(
        thread_count in 2..8usize,
        ops_per_thread in 100..500usize,
        add_value in 1..100u64,
    ) {
        let counter = Arc::new(ShardedCounter::with_shards(thread_count.max(4)));
        let handles: Vec<_> = (0..thread_count)
            .map(|_| {
                let counter = Arc::clone(&counter);
                std::thread::spawn(move || {
                    for _ in 0..ops_per_thread {
                        counter.add(add_value);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let expected = thread_count as u64 * ops_per_thread as u64 * add_value;
        prop_assert_eq!(counter.get(), expected);
    }

    /// ShardedMax concurrent correctness: the global max equals the maximum
    /// value observed across all threads.
    #[test]
    fn max_concurrent_correctness(
        thread_count in 2..8usize,
        max_val in 1..10000u64,
    ) {
        let max_tracker = Arc::new(ShardedMax::with_shards(thread_count.max(4)));
        let handles: Vec<_> = (0..thread_count)
            .map(|t| {
                let max_tracker = Arc::clone(&max_tracker);
                std::thread::spawn(move || {
                    // Each thread observes values up to t * 1000 + max_val
                    let local_max = t as u64 * 1000 + max_val;
                    for v in 0..=local_max {
                        max_tracker.observe(v);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let expected = (thread_count - 1) as u64 * 1000 + max_val;
        prop_assert_eq!(max_tracker.get(), expected);
    }

    /// PaneMap concurrent insert-then-read: all inserted values must be
    /// retrievable after all threads complete.
    #[test]
    fn pane_map_concurrent_insert_read(
        thread_count in 2..8usize,
        entries_per_thread in 10..100usize,
    ) {
        let map = Arc::new(PaneMap::<u64>::with_shards(32));
        let handles: Vec<_> = (0..thread_count)
            .map(|t| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for j in 0..entries_per_thread {
                        let key = (t * 10000 + j) as u64;
                        map.insert(key, key * 3);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Verify all entries
        for t in 0..thread_count {
            for j in 0..entries_per_thread {
                let key = (t * 10000 + j) as u64;
                let got = map.get(key);
                prop_assert_eq!(
                    got,
                    Some(key * 3),
                    "Missing or wrong value for key {}", key
                );
            }
        }

        prop_assert_eq!(
            map.len(),
            thread_count * entries_per_thread
        );
    }

    /// ShardedGauge concurrent: get_max returns a value set by one of the threads.
    /// Note: ShardedGauge is last-write-wins per shard, so two threads writing to
    /// the same shard means earlier values are lost.
    #[test]
    fn gauge_concurrent_max_convergence(
        thread_count in 2..6usize,
        base_val in 1..1000u64,
    ) {
        let gauge = Arc::new(ShardedGauge::with_shards(thread_count.max(4)));
        let handles: Vec<_> = (0..thread_count)
            .map(|t| {
                let gauge = Arc::clone(&gauge);
                let val = base_val + t as u64 * 100;
                std::thread::spawn(move || {
                    gauge.set(val);
                    val
                })
            })
            .collect();

        let mut values_set = Vec::new();
        for h in handles {
            values_set.push(h.join().unwrap());
        }

        let result = gauge.get_max();
        // Result must be one of the values that was set
        prop_assert!(
            values_set.contains(&result),
            "gauge max {} should be one of the values set: {:?}", result, values_set
        );
    }

    /// ShardedMap concurrent insert-then-read: all values retrievable.
    #[test]
    fn sharded_map_concurrent_insert_read(
        thread_count in 2..6usize,
        entries_per_thread in 10..50usize,
    ) {
        let map = Arc::new(ShardedMap::<String, u64>::with_shards(16));
        let handles: Vec<_> = (0..thread_count)
            .map(|t| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for j in 0..entries_per_thread {
                        let key = format!("t{}_k{}", t, j);
                        map.insert(key, (t * 1000 + j) as u64);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        for t in 0..thread_count {
            for j in 0..entries_per_thread {
                let key = format!("t{}_k{}", t, j);
                let got = map.get(&key);
                prop_assert_eq!(
                    got,
                    Some((t * 1000 + j) as u64),
                    "Missing or wrong value for key {}", key
                );
            }
        }

        prop_assert_eq!(map.len(), thread_count * entries_per_thread);
    }
}

// ===========================================================================
// Property tests -- structural invariants
// ===========================================================================

proptest! {
    /// ShardedCounter shard_count matches the requested count.
    #[test]
    fn counter_shard_count_matches(n in 1..32usize) {
        let counter = ShardedCounter::with_shards(n);
        // Clamped to MAX_SHARDS, but for n <= 32 should match
        prop_assert_eq!(counter.shard_count(), n.clamp(1, 64));
    }

    /// ShardedCounter shard_values sum equals get().
    #[test]
    fn counter_shard_values_sum_equals_get(adds in prop::collection::vec(1..1000u64, 1..100)) {
        let counter = ShardedCounter::with_shards(4);
        for v in &adds {
            counter.add(*v);
        }
        let sum_shards: u64 = counter.shard_values().iter().sum();
        prop_assert_eq!(sum_shards, counter.get());
    }

    /// PaneMap insert_if_absent: returns true on first insert, false on second.
    #[test]
    fn pane_map_insert_if_absent(key in 0..1000u64, v1 in 0..1000u64, v2 in 0..1000u64) {
        let map = PaneMap::<u64>::with_shards(8);
        let first = map.insert_if_absent(key, v1);
        prop_assert!(first, "first insert_if_absent should return true");
        let second = map.insert_if_absent(key, v2);
        prop_assert!(!second, "second insert_if_absent should return false");
        // Value should remain v1
        prop_assert_eq!(map.get(key), Some(v1));
    }

    /// PaneMap retain keeps only matching entries.
    #[test]
    fn pane_map_retain_filters(entries in prop::collection::vec((0..100u64, 0..1000u64), 1..50)) {
        let map = PaneMap::<u64>::with_shards(8);
        for (k, v) in &entries {
            map.insert(*k, *v);
        }

        let threshold = 500u64;
        map.retain(|_k, v| *v >= threshold);

        // All remaining values should be >= threshold
        for (k, v) in map.entries() {
            prop_assert!(
                v >= threshold,
                "key {} has value {} which is below threshold {}", k, v, threshold
            );
        }

        // All keys with v >= threshold should still exist
        let mut expected: HashMap<u64, u64> = HashMap::new();
        for (k, v) in &entries {
            expected.insert(*k, *v); // last-write-wins
        }
        for (k, v) in &expected {
            if *v >= threshold {
                prop_assert!(map.contains(*k), "key {} with v {} should survive retain", k, v);
            }
        }
    }

    /// PaneMap keys/entries/len consistency.
    #[test]
    fn pane_map_keys_entries_len_consistent(entries in prop::collection::vec((0..50u64, 0..1000u64), 1..30)) {
        let map = PaneMap::<u64>::with_shards(8);
        for (k, v) in &entries {
            map.insert(*k, *v);
        }

        let keys = map.pane_ids();
        let all_entries = map.entries();
        let len = map.len();

        prop_assert_eq!(keys.len(), len, "keys.len() should equal len()");
        prop_assert_eq!(all_entries.len(), len, "entries.len() should equal len()");

        // All entry keys should be in keys list
        for (k, _) in &all_entries {
            prop_assert!(keys.contains(k), "entry key {} not in keys list", k);
        }
    }

    /// PaneMap shard_sizes sum equals len.
    #[test]
    fn pane_map_shard_sizes_sum(entries in prop::collection::vec((0..100u64, 0..1000u64), 1..50)) {
        let map = PaneMap::<u64>::with_shards(16);
        for (k, v) in &entries {
            map.insert(*k, *v);
        }

        let shard_sum: usize = map.shard_sizes().iter().sum();
        prop_assert_eq!(shard_sum, map.len(), "shard sizes sum should equal len");
    }

    /// PaneMap clear empties all entries.
    #[test]
    fn pane_map_clear_empties(entries in prop::collection::vec((0..100u64, 0..1000u64), 1..50)) {
        let map = PaneMap::<u64>::with_shards(8);
        for (k, v) in &entries {
            map.insert(*k, *v);
        }
        prop_assert!(!map.is_empty());

        map.clear();
        prop_assert_eq!(map.len(), 0);
        prop_assert!(map.is_empty());
        prop_assert!(map.pane_ids().is_empty());
    }

    /// ShardedMap keys, values, entries consistency.
    #[test]
    fn sharded_map_collections_consistent(
        entries in prop::collection::vec(
            ("[a-z]{2,5}", 0..1000u64),
            1..30
        )
    ) {
        let map = ShardedMap::<String, u64>::with_shards(8);
        for (k, v) in &entries {
            map.insert(k.clone(), *v);
        }

        let keys = map.keys();
        let values = map.values();
        let all_entries = map.entries();
        let len = map.len();

        prop_assert_eq!(keys.len(), len);
        prop_assert_eq!(values.len(), len);
        prop_assert_eq!(all_entries.len(), len);
    }

    /// ShardedMap insert_if_absent returns true once, false on duplicate.
    #[test]
    fn sharded_map_insert_if_absent(v1 in 0..1000u64, v2 in 0..1000u64) {
        let map = ShardedMap::<String, u64>::with_shards(4);
        let key = "test_key".to_string();
        let first = map.insert_if_absent(key.clone(), v1);
        prop_assert!(first);
        let second = map.insert_if_absent(key.clone(), v2);
        prop_assert!(!second);
        prop_assert_eq!(map.get(&key), Some(v1));
    }

    /// ShardedSnapshot serde roundtrip.
    #[test]
    fn sharded_snapshot_serde_roundtrip(
        counters in prop::collection::vec(("[a-z]{3,8}", 0..10000u64), 0..5),
        maxes in prop::collection::vec(("[a-z]{3,8}", 0..10000u64), 0..5),
        gauges in prop::collection::vec(("[a-z]{3,8}", 0..10000u64), 0..5),
    ) {
        let snapshot = ShardedSnapshot { counters, maxes, gauges };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: ShardedSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &snapshot);
    }

    /// ShardedGauge store/load compatibility with set/get_max.
    #[test]
    fn gauge_store_load_compat(val in 0..10000u64) {
        let gauge = ShardedGauge::with_shards(4);
        gauge.store(val, std::sync::atomic::Ordering::Relaxed);
        let loaded = gauge.load(std::sync::atomic::Ordering::Relaxed);
        // load returns get_max, which should be >= val (since we just stored it)
        prop_assert!(loaded >= val, "loaded {} should be >= stored {}", loaded, val);
    }

    /// ShardedCounter increment is equivalent to add(1).
    #[test]
    fn counter_increment_is_add_one(n in 1..500usize) {
        let c1 = ShardedCounter::with_shards(4);
        let c2 = ShardedCounter::with_shards(4);
        for _ in 0..n {
            c1.increment();
            c2.add(1);
        }
        prop_assert_eq!(c1.get(), c2.get());
        prop_assert_eq!(c1.get(), n as u64);
    }

    /// ShardedCounter fetch_add returns previous value.
    #[test]
    fn counter_fetch_add_returns_previous(adds in prop::collection::vec(1..100u64, 1..20)) {
        let counter = ShardedCounter::with_shards(4);
        let mut expected_sum = 0u64;
        for v in &adds {
            let prev = counter.fetch_add(*v, std::sync::atomic::Ordering::Relaxed);
            prop_assert_eq!(prev, expected_sum, "fetch_add should return previous sum");
            expected_sum += v;
        }
        prop_assert_eq!(counter.get(), expected_sum);
    }

    /// PaneMap values snapshot matches get for all keys.
    #[test]
    fn pane_map_values_match_gets(entries in prop::collection::vec((0..50u64, 0..1000u64), 1..20)) {
        let map = PaneMap::<u64>::with_shards(8);
        for (k, v) in &entries {
            map.insert(*k, *v);
        }

        let all_entries = map.entries();
        for (k, v) in &all_entries {
            let got = map.get(*k);
            prop_assert_eq!(got, Some(*v), "get({}) should match entries value", k);
        }
    }

    /// ShardedSnapshot Debug is non-empty.
    #[test]
    fn sharded_snapshot_debug_nonempty(
        counters in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
        maxes in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
        gauges in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
    ) {
        let snap = ShardedSnapshot { counters, maxes, gauges };
        let debug = format!("{:?}", snap);
        prop_assert!(!debug.is_empty());
    }

    /// ShardedSnapshot deterministic serialization.
    #[test]
    fn sharded_snapshot_deterministic(
        counters in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
        maxes in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
        gauges in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
    ) {
        let snap = ShardedSnapshot { counters, maxes, gauges };
        let json1 = serde_json::to_string(&snap).unwrap();
        let json2 = serde_json::to_string(&snap).unwrap();
        prop_assert_eq!(json1, json2);
    }

    /// ShardedSnapshot Clone preserves fields.
    #[test]
    fn sharded_snapshot_clone_preserves(
        counters in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
        maxes in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
        gauges in prop::collection::vec(("[a-z]{3,6}", 0..1000u64), 0..3),
    ) {
        let snap = ShardedSnapshot { counters, maxes, gauges };
        let cloned = snap.clone();
        prop_assert_eq!(cloned, snap);
    }

    /// Counter reset zeroes get.
    #[test]
    fn counter_reset_zeroes(adds in prop::collection::vec(1..1000u64, 1..50)) {
        let counter = ShardedCounter::with_shards(4);
        for v in &adds {
            counter.add(*v);
        }
        prop_assert!(counter.get() > 0);
        counter.reset();
        prop_assert_eq!(counter.get(), 0, "reset should zero the counter");
    }

    /// ShardedMax reset zeroes get.
    #[test]
    fn max_reset_zeroes(vals in prop::collection::vec(1..10000u64, 1..50)) {
        let max_tracker = ShardedMax::with_shards(4);
        for v in &vals {
            max_tracker.observe(*v);
        }
        prop_assert!(max_tracker.get() > 0);
        max_tracker.reset();
        prop_assert_eq!(max_tracker.get(), 0, "reset should zero the max");
    }

    /// PaneMap remove makes key absent.
    #[test]
    fn pane_map_remove_makes_absent(key in 0..1000u64, val in 0..1000u64) {
        let map = PaneMap::<u64>::with_shards(8);
        map.insert(key, val);
        prop_assert!(map.contains(key));
        map.remove(key);
        prop_assert!(!map.contains(key), "key should be absent after remove");
        prop_assert_eq!(map.get(key), None, "get should return None after remove");
    }
}

// ===========================================================================
// Property tests -- read_with / write_with accessor methods
// ===========================================================================

proptest! {
    /// PaneMap read_with returns value via closure without cloning.
    #[test]
    fn pane_map_read_with(key in 0..1000u64, val in 0..10000u64) {
        let map = PaneMap::<u64>::with_shards(8);
        map.insert(key, val);

        let result = map.read_with(key, |v| *v * 2);
        prop_assert_eq!(result, Some(val * 2),
            "read_with should apply closure to value");

        // Non-existent key returns None
        let missing = map.read_with(key + 1, |v| *v);
        prop_assert_eq!(missing, None,
            "read_with on missing key should return None");
    }

    /// PaneMap write_with mutates value in-place.
    #[test]
    fn pane_map_write_with(key in 0..1000u64, val in 0..5000u64, delta in 1..5000u64) {
        let map = PaneMap::<u64>::with_shards(8);
        map.insert(key, val);

        let old = map.write_with(key, |v| {
            let old = *v;
            *v += delta;
            old
        });
        prop_assert_eq!(old, Some(val), "write_with should return old value via closure");
        prop_assert_eq!(map.get(key), Some(val + delta),
            "value should be mutated after write_with");
    }

    /// ShardedMap read_with works the same as PaneMap.
    #[test]
    fn sharded_map_read_with(val in 0..10000u64) {
        let map = ShardedMap::<String, u64>::with_shards(4);
        let key = "test".to_string();
        map.insert(key.clone(), val);

        let doubled = map.read_with(&key, |v| *v * 2);
        prop_assert_eq!(doubled, Some(val * 2));

        let missing = map.read_with(&"absent".to_string(), |v: &u64| *v);
        prop_assert_eq!(missing, None);
    }

    /// ShardedMap write_with mutates value in-place.
    #[test]
    fn sharded_map_write_with(val in 0..5000u64, delta in 1..5000u64) {
        let map = ShardedMap::<String, u64>::with_shards(4);
        let key = "test".to_string();
        map.insert(key.clone(), val);

        map.write_with(&key, |v| { *v += delta; });
        prop_assert_eq!(map.get(&key), Some(val + delta));

        // write_with on missing key returns None
        let missing_result = map.write_with(&"absent".to_string(), |v: &mut u64| { *v += 1; });
        prop_assert!(missing_result.is_none());
    }
}

// ===========================================================================
// Property tests -- for_each_mut / map_all_mut
// ===========================================================================

proptest! {
    /// PaneMap for_each_mut applies mutation to all entries.
    #[test]
    fn pane_map_for_each_mut(entries in prop::collection::vec((0..100u64, 1..1000u64), 1..30)) {
        let map = PaneMap::<u64>::with_shards(8);
        let mut expected: HashMap<u64, u64> = HashMap::new();
        for (k, v) in &entries {
            map.insert(*k, *v);
            expected.insert(*k, *v); // last-write-wins
        }

        // Double all values
        map.for_each_mut(|_k, v| { *v *= 2; });

        for (k, original) in &expected {
            let got = map.get(*k);
            prop_assert_eq!(got, Some(original * 2),
                "for_each_mut should have doubled value for key {}", k);
        }
    }

    /// PaneMap map_all_mut collects results from all entries.
    #[test]
    fn pane_map_map_all_mut(entries in prop::collection::vec((0..100u64, 1..1000u64), 1..30)) {
        let map = PaneMap::<u64>::with_shards(8);
        let mut expected: HashMap<u64, u64> = HashMap::new();
        for (k, v) in &entries {
            map.insert(*k, *v);
            expected.insert(*k, *v);
        }

        let results: Vec<(u64, u64)> = map.map_all_mut(|_k, v| {
            let old = *v;
            *v += 10;
            old
        });

        // Results should contain one entry per unique key
        prop_assert_eq!(results.len(), expected.len(),
            "map_all_mut should return one result per entry");

        // Each result should be the old value
        for (k, old_val) in &results {
            let exp = expected.get(k);
            prop_assert_eq!(exp, Some(old_val),
                "map_all_mut result for key {} should be original value", k);
        }

        // Values should now be original + 10
        for (k, original) in &expected {
            let got = map.get(*k);
            prop_assert_eq!(got, Some(original + 10),
                "value for key {} should be incremented by 10", k);
        }
    }
}

// ===========================================================================
// Property tests -- insert returns old value
// ===========================================================================

proptest! {
    /// PaneMap insert returns None for new key, Some(old) for existing key.
    #[test]
    fn pane_map_insert_returns_old(key in 0..1000u64, v1 in 0..1000u64, v2 in 0..1000u64) {
        let map = PaneMap::<u64>::with_shards(8);

        let first = map.insert(key, v1);
        prop_assert_eq!(first, None, "first insert should return None");

        let second = map.insert(key, v2);
        prop_assert_eq!(second, Some(v1), "second insert should return old value");

        prop_assert_eq!(map.get(key), Some(v2), "value should be updated");
    }

    /// ShardedMap insert returns None for new key, Some(old) for existing key.
    #[test]
    fn sharded_map_insert_returns_old(v1 in 0..1000u64, v2 in 0..1000u64) {
        let map = ShardedMap::<String, u64>::with_shards(4);
        let key = "k".to_string();

        let first = map.insert(key.clone(), v1);
        prop_assert_eq!(first, None);

        let second = map.insert(key.clone(), v2);
        prop_assert_eq!(second, Some(v1));

        prop_assert_eq!(map.get(&key), Some(v2));
    }

    /// ShardedMap remove returns the removed value or None.
    #[test]
    fn sharded_map_remove_returns_value(v1 in 0..1000u64) {
        let map = ShardedMap::<String, u64>::with_shards(4);
        let key = "k".to_string();

        let absent = map.remove(&key);
        prop_assert_eq!(absent, None, "removing absent key returns None");

        map.insert(key.clone(), v1);
        let removed = map.remove(&key);
        prop_assert_eq!(removed, Some(v1), "removing existing key returns the value");

        prop_assert!(!map.contains_key(&key), "key should be gone after remove");
    }
}

// ===========================================================================
// Property tests -- monotonicity and mathematical properties
// ===========================================================================

proptest! {
    /// ShardedMax is monotonically non-decreasing with observe (no resets).
    #[test]
    fn max_monotone_without_resets(values in prop::collection::vec(0..10000u64, 2..100)) {
        let max_tracker = ShardedMax::with_shards(4);
        let mut prev = 0u64;
        for v in &values {
            max_tracker.observe(*v);
            let current = max_tracker.get();
            prop_assert!(current >= prev,
                "max should be monotone: {} >= {}", current, prev);
            prev = current;
        }
    }

    /// ShardedCounter is monotonically non-decreasing with add (no resets).
    #[test]
    fn counter_monotone_without_resets(adds in prop::collection::vec(1..100u64, 2..100)) {
        let counter = ShardedCounter::with_shards(4);
        let mut prev = 0u64;
        for v in &adds {
            counter.add(*v);
            let current = counter.get();
            prop_assert!(current > prev,
                "counter should strictly increase: {} > {}", current, prev);
            prev = current;
        }
    }

    /// ShardedMax.get() >= any observed value (no resets).
    #[test]
    fn max_dominates_all_observed(values in prop::collection::vec(0..10000u64, 1..50)) {
        let max_tracker = ShardedMax::with_shards(4);
        for v in &values {
            max_tracker.observe(*v);
        }
        let result = max_tracker.get();
        for v in &values {
            prop_assert!(result >= *v,
                "max {} should be >= observed {}", result, v);
        }
    }

    /// ShardedCounter add is commutative: sum is order-independent.
    #[test]
    fn counter_add_commutative(values in prop::collection::vec(1..1000u64, 2..50)) {
        let c1 = ShardedCounter::with_shards(4);
        let c2 = ShardedCounter::with_shards(4);

        // Forward order
        for v in &values {
            c1.add(*v);
        }

        // Reverse order
        for v in values.iter().rev() {
            c2.add(*v);
        }

        prop_assert_eq!(c1.get(), c2.get(),
            "counter sum should be independent of add order");
    }

    /// Double reset is idempotent: reset twice still yields zero.
    #[test]
    fn counter_double_reset_idempotent(adds in prop::collection::vec(1..1000u64, 1..20)) {
        let counter = ShardedCounter::with_shards(4);
        for v in &adds {
            counter.add(*v);
        }
        counter.reset();
        counter.reset();
        prop_assert_eq!(counter.get(), 0, "double reset should still yield 0");
    }

    /// ShardedMax double reset is idempotent.
    #[test]
    fn max_double_reset_idempotent(vals in prop::collection::vec(1..10000u64, 1..20)) {
        let m = ShardedMax::with_shards(4);
        for v in &vals {
            m.observe(*v);
        }
        m.reset();
        m.reset();
        prop_assert_eq!(m.get(), 0, "double max reset should still yield 0");
    }
}

// ===========================================================================
// Property tests -- DistributionStats
// ===========================================================================

proptest! {
    /// DistributionStats total_entries equals sum of shard sizes.
    #[test]
    fn distribution_stats_total(sizes in prop::collection::vec(0..100usize, 1..20)) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        let expected_total: usize = sizes.iter().sum();
        prop_assert_eq!(stats.total_entries, expected_total,
            "total_entries should equal sum of sizes");
        prop_assert_eq!(stats.shard_count, sizes.len());
    }

    /// DistributionStats min <= mean <= max.
    #[test]
    fn distribution_stats_ordering(sizes in prop::collection::vec(0..100usize, 1..20)) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert!(stats.min_shard_size as f64 <= stats.mean_shard_size + 0.001,
            "min {} should be <= mean {}", stats.min_shard_size, stats.mean_shard_size);
        prop_assert!(stats.mean_shard_size <= stats.max_shard_size as f64 + 0.001,
            "mean {} should be <= max {}", stats.mean_shard_size, stats.max_shard_size);
    }

    /// DistributionStats stddev is non-negative.
    #[test]
    fn distribution_stats_stddev_nonneg(sizes in prop::collection::vec(0..100usize, 1..20)) {
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert!(stats.stddev_shard_size >= 0.0,
            "stddev should be non-negative, got {}", stats.stddev_shard_size);
    }

    /// DistributionStats for uniform sizes has zero stddev.
    #[test]
    fn distribution_stats_uniform_zero_stddev(val in 0..100usize, n in 1..20usize) {
        let sizes = vec![val; n];
        let stats = DistributionStats::from_shard_sizes(&sizes);
        prop_assert!(stats.stddev_shard_size < 0.001,
            "uniform sizes should have ~0 stddev, got {}", stats.stddev_shard_size);
        prop_assert_eq!(stats.min_shard_size, val);
        prop_assert_eq!(stats.max_shard_size, val);
    }
}

// ===========================================================================
// Property tests -- ShardedMap retain / clear / shard_sizes
// ===========================================================================

proptest! {
    /// ShardedMap retain keeps only matching entries.
    #[test]
    fn sharded_map_retain_filters(
        entries in prop::collection::vec(
            ("[a-z]{2,4}", 0..1000u64),
            1..30
        )
    ) {
        let map = ShardedMap::<String, u64>::with_shards(8);
        let mut expected: HashMap<String, u64> = HashMap::new();
        for (k, v) in &entries {
            map.insert(k.clone(), *v);
            expected.insert(k.clone(), *v);
        }

        let threshold = 500u64;
        map.retain(|_k, v| *v >= threshold);

        // All remaining values should be >= threshold
        for (_, v) in map.entries() {
            prop_assert!(v >= threshold,
                "retained value {} should be >= threshold {}", v, threshold);
        }

        // Count surviving entries matches expected
        let expected_count = expected.values().filter(|v| **v >= threshold).count();
        prop_assert_eq!(map.len(), expected_count);
    }

    /// ShardedMap clear empties all entries.
    #[test]
    fn sharded_map_clear_empties(
        entries in prop::collection::vec(
            ("[a-z]{2,4}", 0..1000u64),
            1..20
        )
    ) {
        let map = ShardedMap::<String, u64>::with_shards(4);
        for (k, v) in &entries {
            map.insert(k.clone(), *v);
        }
        prop_assert!(!map.is_empty());

        map.clear();
        prop_assert_eq!(map.len(), 0);
        prop_assert!(map.is_empty());
        prop_assert!(map.keys().is_empty());
    }

    /// ShardedMap shard_sizes sum equals len.
    #[test]
    fn sharded_map_shard_sizes_sum(
        entries in prop::collection::vec(
            ("[a-z]{2,5}", 0..1000u64),
            1..30
        )
    ) {
        let map = ShardedMap::<String, u64>::with_shards(8);
        for (k, v) in &entries {
            map.insert(k.clone(), *v);
        }

        let shard_sum: usize = map.shard_sizes().iter().sum();
        prop_assert_eq!(shard_sum, map.len(),
            "shard sizes sum should equal len");
    }

    /// ShardedMap shard_count matches constructor.
    #[test]
    fn sharded_map_shard_count(n in 1..32usize) {
        let map = ShardedMap::<String, u64>::with_shards(n);
        prop_assert_eq!(map.shard_count(), n.clamp(1, 64));
    }
}

// ===========================================================================
// Property tests -- counter load() consistency
// ===========================================================================

proptest! {
    /// ShardedCounter load() returns the same as get().
    #[test]
    fn counter_load_equals_get(adds in prop::collection::vec(1..1000u64, 1..50)) {
        let counter = ShardedCounter::with_shards(4);
        for v in &adds {
            counter.add(*v);
        }
        let via_get = counter.get();
        let via_load = counter.load(std::sync::atomic::Ordering::Relaxed);
        prop_assert_eq!(via_get, via_load,
            "get() and load() should return the same value");
    }

    /// ShardedGauge reset zeroes get_max.
    #[test]
    fn gauge_reset_zeroes(vals in prop::collection::vec(1..10000u64, 1..30)) {
        let gauge = ShardedGauge::with_shards(4);
        for v in &vals {
            gauge.set(*v);
        }
        prop_assert!(gauge.get_max() > 0);
        gauge.reset();
        prop_assert_eq!(gauge.get_max(), 0, "reset should zero the gauge");
    }

    /// ShardedMax shard_count matches constructor.
    #[test]
    fn max_shard_count(n in 1..32usize) {
        let m = ShardedMax::with_shards(n);
        prop_assert_eq!(m.shard_count(), n.clamp(1, 64));
    }

    /// new() defaults produce zero-valued structures.
    #[test]
    fn defaults_are_zero(_dummy in 0..1u8) {
        let counter = ShardedCounter::new();
        prop_assert_eq!(counter.get(), 0);

        let max = ShardedMax::new();
        prop_assert_eq!(max.get(), 0);

        let gauge = ShardedGauge::new();
        prop_assert_eq!(gauge.get_max(), 0);

        let pmap = PaneMap::<u64>::new();
        prop_assert!(pmap.is_empty());

        let smap = ShardedMap::<String, u64>::new();
        prop_assert!(smap.is_empty());
    }
}
