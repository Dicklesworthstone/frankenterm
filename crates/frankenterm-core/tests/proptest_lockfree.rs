//! Property-based linearizability tests for lock-free data structures
//!
//! Verifies that ShardedCounter, ShardedMax, and PaneMap produce results
//! consistent with a sequential execution of the same operations.

use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

use frankenterm_core::concurrent_map::PaneMap;
use frankenterm_core::sharded_counter::{ShardedCounter, ShardedMax};

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
            MapOp::Contains(k) => {
                if map.contains_key(k) {
                    Some(1)
                } else {
                    Some(0)
                }
            }
        })
        .collect()
}

// ===========================================================================
// Property tests
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
                    let v = if map.contains(*k) { 1 } else { 0 };
                    actual.push(Some(v));
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
}
