//! Property-based linearizability tests for lock-free data structures
//!
//! Verifies that ShardedCounter, ShardedMax, ShardedGauge, PaneMap, and
//! ShardedMap produce results consistent with a sequential execution of
//! the same operations.

use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use frankenterm_core::concurrent_map::{PaneMap, ShardedMap};
use frankenterm_core::sharded_counter::{ShardedCounter, ShardedGauge, ShardedMax, ShardedSnapshot};

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
// Property tests — sequential linearizability
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
// Property tests — concurrent correctness
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
// Property tests — structural invariants
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
}
