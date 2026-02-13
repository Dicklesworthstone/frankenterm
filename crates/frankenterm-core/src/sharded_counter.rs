//! Sharded atomic counters — eliminate false sharing on hot paths
//!
//! Traditional `AtomicU64` counters suffer from cache-line bouncing when
//! multiple cores increment the same counter simultaneously. This module
//! provides cache-line-padded, per-shard counters that distribute writes
//! across independent cache lines and aggregate on read.
//!
//! # Design
//!
//! Each [`ShardedCounter`] holds `N` padded slots (default: number of CPUs,
//! capped at 64). Writers pick a shard via `thread_id % N` for zero-overhead
//! distribution. Readers aggregate all shards — an infrequent operation.
//!
//! [`ShardedMax`] tracks a running maximum using the same sharding, with
//! per-shard CAS loops and a cross-shard max on read.
//!
//! [`ShardedMetrics`] bundles multiple named counters and maxes into a
//! single cache-friendly struct.
//!
//! # Cache Line Padding
//!
//! Each shard is aligned to 128 bytes (two cache lines on x86_64 / one on
//! Apple Silicon) to prevent false sharing. The `#[repr(align(128))]`
//! attribute is safe Rust — no `unsafe` required.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of shards (caps memory usage for small-core machines).
const MAX_SHARDS: usize = 64;

/// Number of shards, determined once per process.
fn shard_count() -> usize {
    // Use available parallelism as a proxy for core count.
    std::thread::available_parallelism()
        .map(|n| n.get().min(MAX_SHARDS).max(1))
        .unwrap_or(4)
}

// ---------------------------------------------------------------------------
// Cache-line-padded slot
// ---------------------------------------------------------------------------

/// A single atomic u64 padded to a full cache line.
///
/// 128-byte alignment covers both x86_64 (64-byte cache line, with adjacent
/// prefetch protection) and Apple Silicon (128-byte cache line).
#[repr(align(128))]
#[derive(Debug)]
struct PaddedAtomicU64 {
    value: AtomicU64,
}

impl PaddedAtomicU64 {
    const fn new(v: u64) -> Self {
        Self {
            value: AtomicU64::new(v),
        }
    }
}

// ---------------------------------------------------------------------------
// Shard selection
// ---------------------------------------------------------------------------

/// Fast, deterministic shard index for the current thread.
///
/// Uses a thread-local cached hash so subsequent calls are zero-cost
/// (no allocation, no format!, no hashing — just a modulo).
#[inline]
fn shard_index(shard_count: usize) -> usize {
    thread_local! {
        static THREAD_HASH: u64 = {
            // ThreadId::as_u64() is nightly-only; hash the Debug repr once.
            let id = std::thread::current().id();
            let s = format!("{id:?}");
            let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
            for byte in s.bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h
        };
    }
    THREAD_HASH.with(|h| (*h as usize) % shard_count)
}

// ---------------------------------------------------------------------------
// ShardedCounter
// ---------------------------------------------------------------------------

/// A sharded atomic counter optimized for high-frequency increment/add.
///
/// Writes are distributed across cache-line-padded shards.
/// Reads aggregate all shards (O(N) where N = shard count).
///
/// # Thread Safety
///
/// Fully `Send + Sync`. No locks, no unsafe.
#[derive(Debug)]
pub struct ShardedCounter {
    shards: Box<[PaddedAtomicU64]>,
}

impl ShardedCounter {
    /// Create a new counter with the default shard count.
    #[must_use]
    pub fn new() -> Self {
        Self::with_shards(shard_count())
    }

    /// Create a counter with a specific number of shards.
    ///
    /// Useful for testing. Clamped to `[1, MAX_SHARDS]`.
    #[must_use]
    pub fn with_shards(n: usize) -> Self {
        let n = n.clamp(1, MAX_SHARDS);
        let shards: Vec<PaddedAtomicU64> = (0..n).map(|_| PaddedAtomicU64::new(0)).collect();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    /// Increment the counter by 1 (the common case).
    #[inline]
    pub fn increment(&self) {
        self.add(1);
    }

    /// Add `value` to the counter.
    #[inline]
    pub fn add(&self, value: u64) {
        let idx = shard_index(self.shards.len());
        self.shards[idx].value.fetch_add(value, Ordering::Relaxed);
    }

    /// Read the aggregate value (sum of all shards).
    ///
    /// This is an O(N) operation across shards. Use sparingly on hot paths.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.value.load(Ordering::Relaxed))
            .sum()
    }

    /// Reset all shards to zero.
    pub fn reset(&self) {
        for shard in self.shards.iter() {
            shard.value.store(0, Ordering::Relaxed);
        }
    }

    /// Number of shards.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Read individual shard values (for diagnostics / false-sharing detection).
    #[must_use]
    pub fn shard_values(&self) -> Vec<u64> {
        self.shards
            .iter()
            .map(|s| s.value.load(Ordering::Relaxed))
            .collect()
    }

    // -- AtomicU64-compatible shims for drop-in replacement -----------------

    /// AtomicU64-compatible: read aggregate value (ignores ordering parameter).
    #[inline]
    #[must_use]
    pub fn load(&self, _ordering: Ordering) -> u64 {
        self.get()
    }

    /// AtomicU64-compatible: add and return previous aggregate (approximate).
    #[inline]
    pub fn fetch_add(&self, value: u64, _ordering: Ordering) -> u64 {
        let prev = self.get();
        self.add(value);
        prev
    }
}

impl Default for ShardedCounter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ShardedMax
// ---------------------------------------------------------------------------

/// A sharded atomic maximum tracker.
///
/// Each shard tracks a local maximum. Reading aggregates the global max.
/// Uses CAS loops within each shard (same as RuntimeMetrics, but padded).
#[derive(Debug)]
pub struct ShardedMax {
    shards: Box<[PaddedAtomicU64]>,
}

impl ShardedMax {
    /// Create with default shard count.
    #[must_use]
    pub fn new() -> Self {
        Self::with_shards(shard_count())
    }

    /// Create with a specific shard count.
    #[must_use]
    pub fn with_shards(n: usize) -> Self {
        let n = n.clamp(1, MAX_SHARDS);
        let shards: Vec<PaddedAtomicU64> = (0..n).map(|_| PaddedAtomicU64::new(0)).collect();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    /// Observe a value, updating the per-shard max if it's larger.
    #[inline]
    pub fn observe(&self, value: u64) {
        let idx = shard_index(self.shards.len());
        let shard = &self.shards[idx];
        let mut current = shard.value.load(Ordering::Relaxed);
        while value > current {
            match shard.value.compare_exchange_weak(
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

    /// Read the global maximum across all shards.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.value.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0)
    }

    /// Reset all shards to zero.
    pub fn reset(&self) {
        for shard in self.shards.iter() {
            shard.value.store(0, Ordering::Relaxed);
        }
    }

    /// Number of shards.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }
}

impl Default for ShardedMax {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ShardedGauge (set/get, not additive)
// ---------------------------------------------------------------------------

/// A sharded atomic gauge for "last-write-wins" values (e.g. timestamps).
///
/// Each shard stores a value; reading returns the max (for timestamps)
/// or the most recently written value.
#[derive(Debug)]
pub struct ShardedGauge {
    shards: Box<[PaddedAtomicU64]>,
}

impl ShardedGauge {
    /// Create with default shard count.
    #[must_use]
    pub fn new() -> Self {
        Self::with_shards(shard_count())
    }

    /// Create with a specific shard count.
    #[must_use]
    pub fn with_shards(n: usize) -> Self {
        let n = n.clamp(1, MAX_SHARDS);
        let shards: Vec<PaddedAtomicU64> = (0..n).map(|_| PaddedAtomicU64::new(0)).collect();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    /// Set the gauge value (written to the current thread's shard).
    #[inline]
    pub fn set(&self, value: u64) {
        let idx = shard_index(self.shards.len());
        self.shards[idx].value.store(value, Ordering::Relaxed);
    }

    /// Read the maximum value across all shards.
    ///
    /// For monotonically increasing values (timestamps), this gives the
    /// most recent value. For non-monotonic values, it gives the maximum.
    #[must_use]
    pub fn get_max(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.value.load(Ordering::Relaxed))
            .max()
            .unwrap_or(0)
    }

    /// Reset all shards to zero.
    pub fn reset(&self) {
        for shard in self.shards.iter() {
            shard.value.store(0, Ordering::Relaxed);
        }
    }

    // -- AtomicU64-compatible shims for drop-in replacement -----------------

    /// AtomicU64-compatible: set gauge value (ignores ordering parameter).
    #[inline]
    pub fn store(&self, value: u64, _ordering: Ordering) {
        self.set(value);
    }

    /// AtomicU64-compatible: read max value (ignores ordering parameter).
    #[inline]
    #[must_use]
    pub fn load(&self, _ordering: Ordering) -> u64 {
        self.get_max()
    }
}

impl Default for ShardedGauge {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Snapshot (for serialization)
// ---------------------------------------------------------------------------

/// Snapshot of a set of sharded metrics, suitable for JSON/display.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardedSnapshot {
    pub counters: Vec<(String, u64)>,
    pub maxes: Vec<(String, u64)>,
    pub gauges: Vec<(String, u64)>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // ShardedCounter
    // -----------------------------------------------------------------------

    #[test]
    fn counter_starts_at_zero() {
        let c = ShardedCounter::with_shards(4);
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn counter_single_thread_increment() {
        let c = ShardedCounter::with_shards(4);
        for _ in 0..1000 {
            c.increment();
        }
        assert_eq!(c.get(), 1000);
    }

    #[test]
    fn counter_single_thread_add() {
        let c = ShardedCounter::with_shards(4);
        c.add(100);
        c.add(200);
        c.add(300);
        assert_eq!(c.get(), 600);
    }

    #[test]
    fn counter_reset() {
        let c = ShardedCounter::with_shards(4);
        c.add(999);
        assert_eq!(c.get(), 999);
        c.reset();
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn counter_shard_values() {
        let c = ShardedCounter::with_shards(4);
        c.add(100);
        let values = c.shard_values();
        assert_eq!(values.len(), 4);
        assert_eq!(values.iter().sum::<u64>(), 100);
    }

    #[test]
    fn counter_concurrent_increments() {
        let c = Arc::new(ShardedCounter::with_shards(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        c.increment();
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.get(), 80_000);
    }

    #[test]
    fn counter_concurrent_adds() {
        let c = Arc::new(ShardedCounter::with_shards(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for i in 0..1_000u64 {
                        c.add(i);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        // Each thread adds 0+1+2+...+999 = 499500
        assert_eq!(c.get(), 8 * 499_500);
    }

    #[test]
    fn counter_min_shards() {
        let c = ShardedCounter::with_shards(0); // clamps to 1
        assert_eq!(c.shard_count(), 1);
        c.increment();
        assert_eq!(c.get(), 1);
    }

    #[test]
    fn counter_max_shards() {
        let c = ShardedCounter::with_shards(1000); // clamps to MAX_SHARDS
        assert_eq!(c.shard_count(), MAX_SHARDS);
    }

    #[test]
    fn counter_default() {
        let c = ShardedCounter::default();
        assert!(c.shard_count() >= 1);
        assert_eq!(c.get(), 0);
    }

    // -----------------------------------------------------------------------
    // ShardedMax
    // -----------------------------------------------------------------------

    #[test]
    fn max_starts_at_zero() {
        let m = ShardedMax::with_shards(4);
        assert_eq!(m.get(), 0);
    }

    #[test]
    fn max_single_thread() {
        let m = ShardedMax::with_shards(4);
        m.observe(10);
        m.observe(50);
        m.observe(30);
        assert_eq!(m.get(), 50);
    }

    #[test]
    fn max_concurrent() {
        let m = Arc::new(ShardedMax::with_shards(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let m = Arc::clone(&m);
                std::thread::spawn(move || {
                    for j in 0..1_000u64 {
                        m.observe(i * 1000 + j);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(m.get(), 7 * 1000 + 999);
    }

    #[test]
    fn max_reset() {
        let m = ShardedMax::with_shards(4);
        m.observe(42);
        m.reset();
        assert_eq!(m.get(), 0);
    }

    #[test]
    fn max_monotonic_values() {
        let m = ShardedMax::with_shards(4);
        for i in 0..100u64 {
            m.observe(i);
        }
        assert_eq!(m.get(), 99);
    }

    #[test]
    fn max_default() {
        let m = ShardedMax::default();
        assert_eq!(m.get(), 0);
    }

    // -----------------------------------------------------------------------
    // ShardedGauge
    // -----------------------------------------------------------------------

    #[test]
    fn gauge_starts_at_zero() {
        let g = ShardedGauge::with_shards(4);
        assert_eq!(g.get_max(), 0);
    }

    #[test]
    fn gauge_set_and_read() {
        let g = ShardedGauge::with_shards(1);
        g.set(42);
        assert_eq!(g.get_max(), 42);
        g.set(100);
        assert_eq!(g.get_max(), 100);
    }

    #[test]
    fn gauge_concurrent_timestamps() {
        let g = Arc::new(ShardedGauge::with_shards(8));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let g = Arc::clone(&g);
                std::thread::spawn(move || {
                    // Each thread writes its "timestamp" (thread index * 1000)
                    g.set((i + 1) * 1000);
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        // The max should be 8000
        assert_eq!(g.get_max(), 8000);
    }

    #[test]
    fn gauge_reset() {
        let g = ShardedGauge::with_shards(4);
        g.set(42);
        g.reset();
        assert_eq!(g.get_max(), 0);
    }

    // -----------------------------------------------------------------------
    // PaddedAtomicU64 alignment
    // -----------------------------------------------------------------------

    #[test]
    fn padded_slot_alignment() {
        assert!(std::mem::align_of::<PaddedAtomicU64>() >= 128);
        // Size should be at least 128 bytes due to alignment
        assert!(std::mem::size_of::<PaddedAtomicU64>() >= 128);
    }

    #[test]
    fn shards_are_on_separate_cache_lines() {
        let c = ShardedCounter::with_shards(4);
        let ptrs: Vec<*const PaddedAtomicU64> = c.shards.iter().map(|s| s as *const _).collect();
        for i in 1..ptrs.len() {
            let distance = (ptrs[i] as usize) - (ptrs[i - 1] as usize);
            assert!(
                distance >= 128,
                "Shards {i} and {} are only {distance} bytes apart (need >= 128)",
                i - 1
            );
        }
    }

    // -----------------------------------------------------------------------
    // shard_index stability
    // -----------------------------------------------------------------------

    #[test]
    fn shard_index_is_deterministic_for_same_thread() {
        let idx1 = shard_index(8);
        let idx2 = shard_index(8);
        assert_eq!(idx1, idx2, "Same thread should always get same shard");
    }

    #[test]
    fn shard_index_within_bounds() {
        for n in 1..=64 {
            let idx = shard_index(n);
            assert!(idx < n, "shard_index({n}) = {idx} out of bounds");
        }
    }

    // -----------------------------------------------------------------------
    // Snapshot serialization
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = ShardedSnapshot {
            counters: vec![("events".to_string(), 42), ("segments".to_string(), 7)],
            maxes: vec![("lag_ms".to_string(), 150)],
            gauges: vec![("last_write".to_string(), 1707753600000)],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ShardedSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // -----------------------------------------------------------------------
    // High contention stress test
    // -----------------------------------------------------------------------

    #[test]
    fn stress_high_contention_counter() {
        // 16 threads, 100K increments each = 1.6M total
        let c = Arc::new(ShardedCounter::with_shards(16));
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for _ in 0..100_000 {
                        c.increment();
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(c.get(), 1_600_000);
    }

    #[test]
    fn stress_mixed_counter_and_max() {
        let counter = Arc::new(ShardedCounter::with_shards(8));
        let max = Arc::new(ShardedMax::with_shards(8));

        let threads: Vec<_> = (0..8)
            .map(|i| {
                let counter = Arc::clone(&counter);
                let max = Arc::clone(&max);
                std::thread::spawn(move || {
                    for j in 0..10_000u64 {
                        counter.add(j);
                        max.observe(i * 10_000 + j);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        // Each thread adds 0+1+...+9999 = 49_995_000
        assert_eq!(counter.get(), 8 * 49_995_000);
        assert_eq!(max.get(), 7 * 10_000 + 9999);
    }
}
