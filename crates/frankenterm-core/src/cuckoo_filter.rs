//! Cuckoo filter — space-efficient probabilistic set with deletion.
//!
//! A Cuckoo filter supports `insert`, `lookup`, and `delete` operations in
//! O(1) time with a configurable false positive rate. Unlike Bloom filters,
//! Cuckoo filters support deletion without false negatives, making them
//! suitable for dynamic sets.
//!
//! # Algorithm
//!
//! Each item is hashed to produce a fingerprint (truncated hash) and two
//! candidate bucket indices. If both buckets are full, the filter performs
//! cuckoo eviction — displacing an existing fingerprint and relocating it
//! to its alternate bucket, up to a maximum number of kicks.
//!
//! ```text
//!  item ──→ hash ──→ fingerprint (f bits)
//!              │
//!              ├──→ bucket_1 = hash(item) mod num_buckets
//!              └──→ bucket_2 = bucket_1 XOR hash(fingerprint) mod num_buckets
//! ```
//!
//! # Use Cases in FrankenTerm
//!
//! - **Event deduplication**: Track seen event IDs, remove stale ones.
//! - **Pane membership**: Quick "is this pane in my watch set?" checks.
//! - **Route filtering**: Block/allow lists that change over time.
//! - **Content fingerprinting**: Track previously seen output hashes.

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

// ── Configuration ───────────────────────────────────────────────────

/// Configuration for a Cuckoo filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuckooConfig {
    /// Number of buckets (will be rounded up to power of 2).
    pub num_buckets: usize,
    /// Number of entries per bucket (typically 2, 4, or 8).
    pub bucket_size: usize,
    /// Maximum number of kick operations before declaring the filter full.
    pub max_kicks: usize,
}

impl Default for CuckooConfig {
    fn default() -> Self {
        Self {
            num_buckets: 1024,
            bucket_size: 4,
            max_kicks: 500,
        }
    }
}

// ── Fingerprint type ────────────────────────────────────────────────

/// A fingerprint is a non-zero u32 hash fragment.
type Fingerprint = u32;

/// Hash an item to produce a fingerprint and primary bucket index.
fn hash_item<T: Hash>(item: &T, num_buckets: usize) -> (Fingerprint, usize) {
    let mut hasher = FnvHasher::new();
    item.hash(&mut hasher);
    let h = hasher.finish();

    // Fingerprint from upper bits (never zero)
    let fp = ((h >> 32) as u32) | 1; // ensure non-zero

    // Primary bucket from lower bits
    let idx = (h as usize) & (num_buckets - 1);

    (fp, idx)
}

/// Compute the alternate bucket index from a fingerprint and bucket index.
fn alt_index(idx: usize, fp: Fingerprint, num_buckets: usize) -> usize {
    let fp_hash = {
        let mut h = FnvHasher::new();
        fp.hash(&mut h);
        h.finish() as usize
    };
    (idx ^ fp_hash) & (num_buckets - 1)
}

// ── FNV-1a hasher (simple, fast, non-cryptographic) ─────────────────

struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    fn new() -> Self {
        Self {
            state: 0xcbf29ce484222325,
        }
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state ^= b as u64;
            self.state = self.state.wrapping_mul(0x100000001b3);
        }
    }
}

// ── Bucket ──────────────────────────────────────────────────────────

/// A bucket holding up to `bucket_size` fingerprints.
#[derive(Debug, Clone)]
struct Bucket {
    entries: Vec<Fingerprint>,
    capacity: usize,
}

impl Bucket {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    fn insert(&mut self, fp: Fingerprint) -> bool {
        if self.is_full() {
            return false;
        }
        self.entries.push(fp);
        true
    }

    fn contains(&self, fp: Fingerprint) -> bool {
        self.entries.contains(&fp)
    }

    fn remove(&mut self, fp: Fingerprint) -> bool {
        if let Some(pos) = self.entries.iter().position(|&x| x == fp) {
            self.entries.swap_remove(pos);
            true
        } else {
            false
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Swap a random entry with a new fingerprint, returning the evicted one.
    fn swap(&mut self, idx: usize, fp: Fingerprint) -> Fingerprint {
        let old = self.entries[idx];
        self.entries[idx] = fp;
        old
    }
}

// ── CuckooFilter ────────────────────────────────────────────────────

/// A Cuckoo filter for probabilistic set membership with deletion support.
#[derive(Debug, Clone)]
pub struct CuckooFilter {
    buckets: Vec<Bucket>,
    num_buckets: usize,
    bucket_size: usize,
    max_kicks: usize,
    count: usize,
}

/// Result of an insert operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    /// Item was successfully inserted.
    Ok,
    /// Filter is full — too many evictions.
    Full,
    /// Item is likely already present (duplicate fingerprint).
    Duplicate,
}

/// Statistics about the filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuckooStats {
    /// Total capacity (num_buckets * bucket_size).
    pub capacity: usize,
    /// Number of stored fingerprints.
    pub count: usize,
    /// Load factor as percentage (0-100).
    pub load_percent: u32,
    /// Number of buckets.
    pub num_buckets: usize,
    /// Entries per bucket.
    pub bucket_size: usize,
    /// Number of non-empty buckets.
    pub occupied_buckets: usize,
}

impl CuckooFilter {
    /// Create a new Cuckoo filter with default configuration.
    pub fn new() -> Self {
        Self::with_config(CuckooConfig::default())
    }

    /// Create a new Cuckoo filter with custom configuration.
    pub fn with_config(config: CuckooConfig) -> Self {
        let num_buckets = config.num_buckets.next_power_of_two().max(2);
        let bucket_size = config.bucket_size.max(1);
        let buckets = (0..num_buckets).map(|_| Bucket::new(bucket_size)).collect();
        Self {
            buckets,
            num_buckets,
            bucket_size,
            max_kicks: config.max_kicks,
            count: 0,
        }
    }

    /// Create a filter sized for an expected number of items.
    ///
    /// Uses a load factor of ~95% to balance space and insertion success.
    pub fn with_capacity(expected_items: usize) -> Self {
        let bucket_size = 4;
        let num_buckets = ((expected_items as f64 / bucket_size as f64 / 0.95).ceil() as usize)
            .next_power_of_two()
            .max(2);
        Self::with_config(CuckooConfig {
            num_buckets,
            bucket_size,
            max_kicks: 500,
        })
    }

    /// Insert an item into the filter.
    pub fn insert<T: Hash>(&mut self, item: &T) -> InsertResult {
        let (fp, i1) = hash_item(item, self.num_buckets);
        let i2 = alt_index(i1, fp, self.num_buckets);

        // Try primary bucket
        if self.buckets[i1].insert(fp) {
            self.count += 1;
            return InsertResult::Ok;
        }

        // Try alternate bucket
        if self.buckets[i2].insert(fp) {
            self.count += 1;
            return InsertResult::Ok;
        }

        // Both full — start cuckoo eviction
        let mut idx = if self.count % 2 == 0 { i1 } else { i2 };
        let mut evicted_fp = fp;

        for _ in 0..self.max_kicks {
            // Pick a random slot in the bucket to evict
            let slot = (evicted_fp as usize) % self.buckets[idx].len();
            evicted_fp = self.buckets[idx].swap(slot, evicted_fp);

            // Try to insert evicted fingerprint in its alternate bucket
            idx = alt_index(idx, evicted_fp, self.num_buckets);
            if self.buckets[idx].insert(evicted_fp) {
                self.count += 1;
                return InsertResult::Ok;
            }
        }

        InsertResult::Full
    }

    /// Check if an item is likely in the filter.
    ///
    /// Returns `true` if the item is probably present, `false` if definitely absent.
    /// False positives are possible; false negatives are not.
    pub fn lookup<T: Hash>(&self, item: &T) -> bool {
        let (fp, i1) = hash_item(item, self.num_buckets);
        let i2 = alt_index(i1, fp, self.num_buckets);
        self.buckets[i1].contains(fp) || self.buckets[i2].contains(fp)
    }

    /// Delete an item from the filter.
    ///
    /// Returns `true` if the item was found and removed.
    ///
    /// **Important**: Only delete items that were previously inserted.
    /// Deleting items that were never inserted can cause false negatives.
    pub fn delete<T: Hash>(&mut self, item: &T) -> bool {
        let (fp, i1) = hash_item(item, self.num_buckets);
        let i2 = alt_index(i1, fp, self.num_buckets);

        if self.buckets[i1].remove(fp) {
            self.count -= 1;
            return true;
        }
        if self.buckets[i2].remove(fp) {
            self.count -= 1;
            return true;
        }
        false
    }

    /// Number of items in the filter.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Total capacity of the filter.
    pub fn capacity(&self) -> usize {
        self.num_buckets * self.bucket_size
    }

    /// Load factor (0.0 to 1.0).
    pub fn load_factor(&self) -> f64 {
        self.count as f64 / self.capacity() as f64
    }

    /// Whether the filter is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Get statistics about the filter.
    pub fn stats(&self) -> CuckooStats {
        let occupied = self.buckets.iter().filter(|b| !b.is_empty()).count();
        let capacity = self.capacity();
        CuckooStats {
            capacity,
            count: self.count,
            load_percent: if capacity > 0 {
                ((self.count as f64 / capacity as f64) * 100.0) as u32
            } else {
                0
            },
            num_buckets: self.num_buckets,
            bucket_size: self.bucket_size,
            occupied_buckets: occupied,
        }
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        for bucket in &mut self.buckets {
            bucket.entries.clear();
        }
        self.count = 0;
    }

    /// Number of buckets.
    pub fn num_buckets(&self) -> usize {
        self.num_buckets
    }

    /// Entries per bucket.
    pub fn bucket_size(&self) -> usize {
        self.bucket_size
    }
}

impl Default for CuckooFilter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter() {
        let filter = CuckooFilter::new();
        assert!(filter.is_empty());
        assert_eq!(filter.count(), 0);
        assert!(!filter.lookup(&42));
    }

    #[test]
    fn insert_and_lookup() {
        let mut filter = CuckooFilter::new();
        assert_eq!(filter.insert(&"hello"), InsertResult::Ok);
        assert!(filter.lookup(&"hello"));
        assert!(!filter.lookup(&"world"));
        assert_eq!(filter.count(), 1);
    }

    #[test]
    fn insert_multiple() {
        let mut filter = CuckooFilter::new();
        for i in 0..100 {
            assert_eq!(filter.insert(&i), InsertResult::Ok);
        }
        assert_eq!(filter.count(), 100);
        for i in 0..100 {
            assert!(filter.lookup(&i), "should find {}", i);
        }
    }

    #[test]
    fn delete() {
        let mut filter = CuckooFilter::new();
        filter.insert(&42);
        assert!(filter.lookup(&42));
        assert!(filter.delete(&42));
        assert!(!filter.lookup(&42));
        assert_eq!(filter.count(), 0);
    }

    #[test]
    fn delete_nonexistent() {
        let mut filter = CuckooFilter::new();
        assert!(!filter.delete(&42));
    }

    #[test]
    fn insert_delete_insert() {
        let mut filter = CuckooFilter::new();
        filter.insert(&"key");
        filter.delete(&"key");
        filter.insert(&"key");
        assert!(filter.lookup(&"key"));
        assert_eq!(filter.count(), 1);
    }

    #[test]
    fn clear() {
        let mut filter = CuckooFilter::new();
        for i in 0..50 {
            filter.insert(&i);
        }
        assert_eq!(filter.count(), 50);
        filter.clear();
        assert!(filter.is_empty());
        assert_eq!(filter.count(), 0);
        for i in 0..50 {
            assert!(!filter.lookup(&i));
        }
    }

    #[test]
    fn stats() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 16,
            bucket_size: 4,
            max_kicks: 100,
        });
        filter.insert(&1);
        filter.insert(&2);
        let stats = filter.stats();
        assert_eq!(stats.count, 2);
        assert_eq!(stats.capacity, 64); // 16 * 4
        assert!(stats.occupied_buckets > 0);
    }

    #[test]
    fn with_capacity() {
        let filter = CuckooFilter::with_capacity(1000);
        assert!(filter.capacity() >= 1000);
    }

    #[test]
    fn load_factor() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 8,
            bucket_size: 4,
            max_kicks: 100,
        });
        assert_eq!(filter.load_factor(), 0.0);
        for i in 0..16 {
            filter.insert(&i);
        }
        assert!(filter.load_factor() > 0.0);
        assert!(filter.load_factor() <= 1.0);
    }

    #[test]
    fn config_default() {
        let config = CuckooConfig::default();
        assert_eq!(config.num_buckets, 1024);
        assert_eq!(config.bucket_size, 4);
        assert_eq!(config.max_kicks, 500);
    }

    #[test]
    fn config_serde() {
        let config = CuckooConfig {
            num_buckets: 256,
            bucket_size: 8,
            max_kicks: 200,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: CuckooConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn stats_serde() {
        let stats = CuckooStats {
            capacity: 100,
            count: 50,
            load_percent: 50,
            num_buckets: 25,
            bucket_size: 4,
            occupied_buckets: 20,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: CuckooStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn small_filter_fills() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 2,
            bucket_size: 2,
            max_kicks: 10,
        });
        let mut inserted = 0;
        for i in 0..100 {
            if filter.insert(&i) == InsertResult::Ok {
                inserted += 1;
            }
        }
        // Small filter (4 slots) should fill up
        assert!(inserted <= 4);
        assert!(inserted > 0);
    }

    #[test]
    fn alt_index_is_involution() {
        // alt_index(alt_index(i, fp, n), fp, n) == i
        // This is the key property that makes cuckoo hashing work
        let n = 16;
        for i in 0..n {
            for fp in [1u32, 42, 255, 1000, u32::MAX] {
                let j = alt_index(i, fp, n);
                let k = alt_index(j, fp, n);
                assert_eq!(k, i, "alt_index is not involutory for i={}, fp={}", i, fp);
            }
        }
    }

    #[test]
    fn fingerprint_nonzero() {
        // Fingerprints should always be non-zero (we OR with 1)
        for i in 0..1000u64 {
            let (fp, _) = hash_item(&i, 256);
            assert_ne!(fp, 0, "fingerprint should never be zero for item {}", i);
        }
    }

    #[test]
    fn num_buckets_power_of_two() {
        let filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 13, // not power of 2
            bucket_size: 4,
            max_kicks: 100,
        });
        assert!(filter.num_buckets().is_power_of_two());
        assert!(filter.num_buckets() >= 13);
    }

    #[test]
    fn debug_and_clone() {
        let filter = CuckooFilter::new();
        let dbg = format!("{:?}", filter);
        assert!(dbg.contains("CuckooFilter"), "got: {}", dbg);
        let cloned = filter.clone();
        assert_eq!(cloned.count(), filter.count());
    }

    #[test]
    fn insert_result_eq() {
        assert_eq!(InsertResult::Ok, InsertResult::Ok);
        assert_ne!(InsertResult::Ok, InsertResult::Full);
        assert_ne!(InsertResult::Full, InsertResult::Duplicate);
    }
}
