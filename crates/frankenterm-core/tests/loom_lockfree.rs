//! Loom model-checking tests for lock-free data structures
//!
//! Loom exhaustively explores all possible thread interleavings, proving
//! that our lock-free algorithms are correct under every schedule —
//! not just the ones that happen to run during random stress testing.
//!
//! These tests use loom's own atomic types (which loom can intercept
//! and permute) rather than the production std::sync::atomic types.

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::Arc;
use loom::thread;

// ===========================================================================
// Simplified sharded counter using loom atomics
// ===========================================================================

/// Minimal sharded counter for loom verification.
///
/// This is a simplified version of `ShardedCounter` that uses loom's
/// atomic types so loom can explore all interleavings.
struct LoomShardedCounter {
    shards: Vec<AtomicU64>,
}

impl LoomShardedCounter {
    fn new(n: usize) -> Self {
        let shards = (0..n).map(|_| AtomicU64::new(0)).collect();
        Self { shards }
    }

    fn add(&self, shard: usize, value: u64) {
        self.shards[shard].fetch_add(value, Ordering::Relaxed);
    }

    fn get(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .sum()
    }
}

/// Minimal sharded max for loom verification.
struct LoomShardedMax {
    shards: Vec<AtomicU64>,
}

impl LoomShardedMax {
    fn new(n: usize) -> Self {
        let shards = (0..n).map(|_| AtomicU64::new(0)).collect();
        Self { shards }
    }

    fn observe(&self, shard: usize, value: u64) {
        let s = &self.shards[shard];
        let mut current = s.load(Ordering::Relaxed);
        while value > current {
            match s.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => current = v,
            }
        }
    }

    fn get(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0)
    }
}

// ===========================================================================
// Loom tests
// ===========================================================================

/// Two threads, each incrementing their own shard.
/// The final sum must equal the total of all increments.
///
/// This verifies that fetch_add on separate shards is non-interfering
/// and that the aggregate read sees all writes after join.
#[test]
fn loom_counter_two_threads_separate_shards() {
    loom::model(|| {
        let counter = Arc::new(LoomShardedCounter::new(2));

        let c1 = Arc::clone(&counter);
        let t1 = thread::spawn(move || {
            c1.add(0, 3);
            c1.add(0, 7);
        });

        let c2 = Arc::clone(&counter);
        let t2 = thread::spawn(move || {
            c2.add(1, 5);
            c2.add(1, 11);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // 3 + 7 + 5 + 11 = 26
        assert_eq!(counter.get(), 26);
    });
}

/// Two threads incrementing the SAME shard (contention case).
/// fetch_add is atomic so the sum must be exact.
#[test]
fn loom_counter_two_threads_same_shard() {
    loom::model(|| {
        let counter = Arc::new(LoomShardedCounter::new(1));

        let c1 = Arc::clone(&counter);
        let t1 = thread::spawn(move || {
            c1.add(0, 10);
            c1.add(0, 20);
        });

        let c2 = Arc::clone(&counter);
        let t2 = thread::spawn(move || {
            c2.add(0, 30);
            c2.add(0, 40);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // 10 + 20 + 30 + 40 = 100
        assert_eq!(counter.get(), 100);
    });
}

/// Two threads doing CAS-based max on the SAME shard.
/// The final max must be the largest value observed.
#[test]
fn loom_max_two_threads_same_shard() {
    loom::model(|| {
        let max = Arc::new(LoomShardedMax::new(1));

        let m1 = Arc::clone(&max);
        let t1 = thread::spawn(move || {
            m1.observe(0, 50);
            m1.observe(0, 100);
        });

        let m2 = Arc::clone(&max);
        let t2 = thread::spawn(move || {
            m2.observe(0, 75);
            m2.observe(0, 200);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(max.get(), 200);
    });
}

/// Two threads doing max on separate shards.
/// Global max must be the largest value across all shards.
#[test]
fn loom_max_two_threads_separate_shards() {
    loom::model(|| {
        let max = Arc::new(LoomShardedMax::new(2));

        let m1 = Arc::clone(&max);
        let t1 = thread::spawn(move || {
            m1.observe(0, 42);
        });

        let m2 = Arc::clone(&max);
        let t2 = thread::spawn(move || {
            m2.observe(1, 99);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(max.get(), 99);
    });
}

/// Two threads: one incrementing a counter, the other observing a max.
/// After join, both aggregates must be correct.
/// This verifies independence between counter and max operations.
#[test]
fn loom_mixed_counter_and_max() {
    loom::model(|| {
        let counter = Arc::new(LoomShardedCounter::new(2));
        let max = Arc::new(LoomShardedMax::new(2));

        let c1 = Arc::clone(&counter);
        let m1 = Arc::clone(&max);
        let t1 = thread::spawn(move || {
            c1.add(0, 5);
            m1.observe(0, 30);
        });

        let c2 = Arc::clone(&counter);
        let m2 = Arc::clone(&max);
        let t2 = thread::spawn(move || {
            c2.add(1, 15);
            m2.observe(1, 50);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(counter.get(), 20);
        assert_eq!(max.get(), 50);
    });
}

/// CAS retry correctness: two threads racing to set a max on the same shard,
/// where thread 2's value is smaller. The CAS loop must not clobber a
/// larger value with a smaller one.
#[test]
fn loom_max_cas_no_clobber() {
    loom::model(|| {
        let max = Arc::new(LoomShardedMax::new(1));

        let m1 = Arc::clone(&max);
        let t1 = thread::spawn(move || {
            m1.observe(0, 1000);
        });

        let m2 = Arc::clone(&max);
        let t2 = thread::spawn(move || {
            m2.observe(0, 1);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // The max must be 1000, never 1
        assert_eq!(max.get(), 1000);
    });
}
