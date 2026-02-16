//! Property-based tests for work_stealing_deque.
//!
//! Bead: ft-t58vf

use frankenterm_core::work_stealing_deque::{
    new_deque, new_deque_default, StealResult, WorkStealingPool, WsDequeConfig, WsDequeStats,
};
use proptest::prelude::*;

// ── Strategies ───────────────────────────────────────────────────────

fn capacity_strategy() -> impl Strategy<Value = usize> {
    prop_oneof![Just(1), Just(4), Just(16), Just(64), Just(256)]
}

fn items_strategy() -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0..10_000u32, 0..100)
}

fn small_items_strategy() -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0..1000u32, 1..30)
}

// ── LIFO ordering ────────────────────────────────────────────────────

proptest! {
    /// Worker pop returns items in LIFO order (reverse of push order).
    #[test]
    fn pop_is_lifo(items in items_strategy(), cap in capacity_strategy()) {
        let (worker, _stealer) = new_deque(cap);
        for &item in &items {
            worker.push(item);
        }
        let mut popped = Vec::new();
        while let Some(v) = worker.pop() {
            popped.push(v);
        }
        let mut expected = items.clone();
        expected.reverse();
        prop_assert_eq!(popped, expected);
    }

    /// Stealer steal returns items in FIFO order (same as push order).
    #[test]
    fn steal_is_fifo(items in items_strategy(), cap in capacity_strategy()) {
        let (worker, stealer) = new_deque(cap);
        for &item in &items {
            worker.push(item);
        }
        let mut stolen = Vec::new();
        loop {
            match stealer.steal() {
                StealResult::Success(v) => stolen.push(v),
                StealResult::Empty => break,
                StealResult::Retry => continue,
            }
        }
        prop_assert_eq!(stolen, items);
    }
}

// ── Conservation ─────────────────────────────────────────────────────

proptest! {
    /// Total items = popped + stolen + remaining. Nothing is lost or duplicated.
    #[test]
    fn conservation_pop_then_steal(
        items in small_items_strategy(),
        pop_count in 0..30usize
    ) {
        let (worker, stealer) = new_deque(64);
        for &item in &items {
            worker.push(item);
        }

        let actual_pops = pop_count.min(items.len());
        let mut popped = Vec::new();
        for _ in 0..actual_pops {
            if let Some(v) = worker.pop() {
                popped.push(v);
            }
        }

        let mut stolen = Vec::new();
        loop {
            match stealer.steal() {
                StealResult::Success(v) => stolen.push(v),
                StealResult::Empty => break,
                StealResult::Retry => continue,
            }
        }

        let remaining = worker.len();
        prop_assert_eq!(
            popped.len() + stolen.len() + remaining,
            items.len(),
            "conservation violated: popped={} stolen={} remaining={} total={}",
            popped.len(), stolen.len(), remaining, items.len()
        );
    }

    /// No item appears in both popped and stolen sets.
    #[test]
    fn no_duplication(
        items in small_items_strategy(),
        pop_count in 0..30usize
    ) {
        let (worker, stealer) = new_deque(64);
        for &item in &items {
            worker.push(item);
        }

        let mut all_retrieved = Vec::new();

        let actual_pops = pop_count.min(items.len());
        for _ in 0..actual_pops {
            if let Some(v) = worker.pop() {
                all_retrieved.push(v);
            }
        }

        loop {
            match stealer.steal() {
                StealResult::Success(v) => all_retrieved.push(v),
                StealResult::Empty => break,
                StealResult::Retry => continue,
            }
        }

        // All retrieved items must be a subset of pushed items
        let mut sorted_retrieved = all_retrieved.clone();
        sorted_retrieved.sort();
        let mut sorted_items = items.clone();
        sorted_items.sort();

        // Every retrieved item must exist in the original
        for val in &sorted_retrieved {
            let is_present = sorted_items.contains(val);
            prop_assert!(is_present, "retrieved item {} not in original set", val);
        }

        // Count must match
        prop_assert_eq!(all_retrieved.len(), items.len());
    }
}

// ── Stats accuracy ───────────────────────────────────────────────────

proptest! {
    /// Stats counters accurately track push/pop/steal counts.
    #[test]
    fn stats_accuracy(
        items in small_items_strategy(),
        pop_count in 0..30usize
    ) {
        let (worker, stealer) = new_deque(64);
        for &item in &items {
            worker.push(item);
        }

        let mut actual_popped = 0u64;
        let actual_pops = pop_count.min(items.len());
        for _ in 0..actual_pops {
            if worker.pop().is_some() {
                actual_popped += 1;
            }
        }

        let mut actual_stolen = 0u64;
        loop {
            match stealer.steal() {
                StealResult::Success(_) => actual_stolen += 1,
                StealResult::Empty => break,
                StealResult::Retry => continue,
            }
        }

        let stats = worker.stats();
        prop_assert_eq!(stats.total_pushed, items.len() as u64);
        prop_assert_eq!(stats.total_popped, actual_popped);
        prop_assert_eq!(stats.total_stolen, actual_stolen);
        prop_assert_eq!(stats.len, 0, "all items should be drained");
    }
}

// ── Batch steal ──────────────────────────────────────────────────────

proptest! {
    /// Batch steal never returns more than requested.
    #[test]
    fn batch_steal_bounded(
        items in small_items_strategy(),
        batch_max in 1..50usize
    ) {
        let (worker, stealer) = new_deque(64);
        for &item in &items {
            worker.push(item);
        }
        let batch = stealer.steal_batch(batch_max);
        prop_assert!(batch.len() <= batch_max, "batch {} > max {}", batch.len(), batch_max);
        prop_assert!(batch.len() <= items.len(), "batch {} > items {}", batch.len(), items.len());
    }

    /// Batch steal returns items in FIFO order.
    #[test]
    fn batch_steal_fifo(items in small_items_strategy()) {
        let (worker, stealer) = new_deque(64);
        for &item in &items {
            worker.push(item);
        }
        let batch = stealer.steal_batch(items.len());
        prop_assert_eq!(batch, items, "batch steal should return FIFO order");
    }

    /// After batch steal, remaining items are accessible via pop (LIFO of remainder).
    #[test]
    fn batch_steal_remainder(
        items in small_items_strategy(),
        batch_size in 0..30usize
    ) {
        let (worker, stealer) = new_deque(64);
        for &item in &items {
            worker.push(item);
        }

        let batch = stealer.steal_batch(batch_size);
        let batch_taken = batch.len();

        let mut popped = Vec::new();
        while let Some(v) = worker.pop() {
            popped.push(v);
        }

        prop_assert_eq!(
            batch_taken + popped.len(),
            items.len(),
            "batch + popped must equal total"
        );
    }
}

// ── Pool invariants ──────────────────────────────────────────────────

proptest! {
    /// Pool conservation: all pushed items are retrievable via pop or steal.
    #[test]
    fn pool_conservation(
        n_workers in 2..5usize,
        items_per_worker in prop::collection::vec(
            prop::collection::vec(0..1000u32, 0..20),
            2..5
        )
    ) {
        let n = n_workers.min(items_per_worker.len());
        let pool = WorkStealingPool::new(n);

        let mut total_pushed = 0usize;
        for (i, items) in items_per_worker.iter().take(n).enumerate() {
            for &item in items {
                pool.push(i, item);
                total_pushed += 1;
            }
        }

        let mut total_retrieved = 0usize;
        for i in 0..n {
            while let Some(_) = pool.pop_or_steal(i) {
                total_retrieved += 1;
            }
        }

        prop_assert_eq!(total_retrieved, total_pushed);
    }

    /// Pool stats total_pushed matches actual pushes.
    #[test]
    fn pool_stats_pushed(
        n_workers in 2..4usize,
        count in 0..50usize
    ) {
        let pool = WorkStealingPool::new(n_workers);
        for i in 0..count {
            pool.push(i % n_workers, i as u32);
        }
        let stats = pool.stats();
        prop_assert_eq!(stats.total_pushed, count as u64);
    }
}

// ── StealResult properties ──────────────────────────────────────────

proptest! {
    /// into_option returns Some for Success, None otherwise.
    #[test]
    fn steal_result_into_option(val in any::<u32>()) {
        let s = StealResult::Success(val);
        prop_assert_eq!(s.into_option(), Some(val));

        let e: StealResult<u32> = StealResult::Empty;
        prop_assert_eq!(e.into_option(), None);

        let r: StealResult<u32> = StealResult::Retry;
        prop_assert_eq!(r.into_option(), None);
    }

    /// unwrap returns the value for Success.
    #[test]
    fn steal_result_unwrap(val in any::<u32>()) {
        let s = StealResult::Success(val);
        prop_assert_eq!(s.unwrap(), val);
    }
}

// ── Serde roundtrip ─────────────────────────────────────────────────

proptest! {
    /// WsDequeConfig survives serde roundtrip.
    #[test]
    fn config_serde_roundtrip(cap in 1..10_000usize) {
        let config = WsDequeConfig { initial_capacity: cap };
        let json = serde_json::to_string(&config).expect("serialize");
        let back: WsDequeConfig = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(config, back);
    }

    /// WsDequeStats survives serde roundtrip.
    #[test]
    fn stats_serde_roundtrip(
        len in 0..1000usize,
        pushed in 0..10_000u64,
        popped in 0..10_000u64,
        stolen in 0..10_000u64,
        failures in 0..10_000u64,
    ) {
        let stats = WsDequeStats {
            len,
            total_pushed: pushed,
            total_popped: popped,
            total_stolen: stolen,
            steal_failures: failures,
        };
        let json = serde_json::to_string(&stats).expect("serialize");
        let back: WsDequeStats = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(stats, back);
    }
}

// ── Empty deque properties ──────────────────────────────────────────

proptest! {
    /// Empty deque always returns None/Empty for all operations.
    #[test]
    fn empty_deque_invariants(cap in capacity_strategy()) {
        let (worker, stealer) = new_deque::<u32>(cap);
        prop_assert!(worker.is_empty());
        prop_assert_eq!(worker.len(), 0);
        prop_assert_eq!(worker.pop(), None);
        let is_empty = stealer.steal().is_empty();
        prop_assert!(is_empty, "steal on empty deque should return Empty");
        let batch = stealer.steal_batch(10);
        prop_assert!(batch.is_empty());
    }

    /// Default constructor produces a working deque.
    #[test]
    fn default_deque_works(val in any::<u32>()) {
        let (worker, stealer) = new_deque_default::<u32>();
        worker.push(val);
        prop_assert_eq!(worker.len(), 1);
        let result = stealer.steal();
        let is_success = result.is_success();
        prop_assert!(is_success);
    }
}

// ── Interleaved push/pop/steal ──────────────────────────────────────

#[derive(Debug, Clone)]
enum Op {
    Push(u32),
    Pop,
    Steal,
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            (0..1000u32).prop_map(Op::Push),
            Just(Op::Pop),
            Just(Op::Steal),
        ],
        1..80,
    )
}

proptest! {
    /// Interleaved operations maintain conservation invariant.
    #[test]
    fn interleaved_conservation(ops in ops_strategy()) {
        let (worker, stealer) = new_deque(64);
        let mut pushed = 0usize;
        let mut popped = 0usize;
        let mut stolen = 0usize;

        for op in &ops {
            match op {
                Op::Push(v) => {
                    worker.push(*v);
                    pushed += 1;
                }
                Op::Pop => {
                    if worker.pop().is_some() {
                        popped += 1;
                    }
                }
                Op::Steal => {
                    match stealer.steal() {
                        StealResult::Success(_) => stolen += 1,
                        _ => {}
                    }
                }
            }
        }

        let remaining = worker.len();
        prop_assert_eq!(
            pushed,
            popped + stolen + remaining,
            "conservation: pushed={} popped={} stolen={} remaining={}",
            pushed, popped, stolen, remaining
        );
    }

    /// Stats match actual operation counts after interleaved operations.
    #[test]
    fn interleaved_stats(ops in ops_strategy()) {
        let (worker, stealer) = new_deque(64);
        let mut expected_pushed = 0u64;
        let mut expected_popped = 0u64;
        let mut expected_stolen = 0u64;

        for op in &ops {
            match op {
                Op::Push(v) => {
                    worker.push(*v);
                    expected_pushed += 1;
                }
                Op::Pop => {
                    if worker.pop().is_some() {
                        expected_popped += 1;
                    }
                }
                Op::Steal => {
                    match stealer.steal() {
                        StealResult::Success(_) => expected_stolen += 1,
                        _ => {}
                    }
                }
            }
        }

        let stats = worker.stats();
        prop_assert_eq!(stats.total_pushed, expected_pushed);
        prop_assert_eq!(stats.total_popped, expected_popped);
        prop_assert_eq!(stats.total_stolen, expected_stolen);
    }
}
