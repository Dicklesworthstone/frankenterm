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

    // -- Batch: DarkBadger wa-1u90p.7.1 ----------------------------------------

    #[test]
    fn insert_result_debug_clone_copy() {
        let a = InsertResult::Ok;
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a.clone();
        assert_eq!(a, c);
        let dbg = format!("{:?}", a);
        assert_eq!(dbg, "Ok");
    }

    #[test]
    fn insert_result_all_three_distinct() {
        let variants = [
            InsertResult::Ok,
            InsertResult::Full,
            InsertResult::Duplicate,
        ];
        for i in 0..variants.len() {
            for j in (i + 1)..variants.len() {
                assert_ne!(variants[i], variants[j]);
            }
        }
    }

    #[test]
    fn cuckoo_config_debug_clone_eq() {
        let a = CuckooConfig::default();
        let b = a.clone();
        assert_eq!(a, b);
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("CuckooConfig"));
    }

    #[test]
    fn cuckoo_stats_debug_clone_eq() {
        let a = CuckooStats {
            capacity: 100,
            count: 10,
            load_percent: 10,
            num_buckets: 25,
            bucket_size: 4,
            occupied_buckets: 5,
        };
        let b = a.clone();
        assert_eq!(a, b);
        let dbg = format!("{:?}", a);
        assert!(dbg.contains("CuckooStats"));
    }

    #[test]
    fn cuckoo_filter_default_trait() {
        let filter = CuckooFilter::default();
        assert!(filter.is_empty());
        assert_eq!(filter.count(), 0);
    }

    #[test]
    fn cuckoo_filter_accessors() {
        let filter = CuckooFilter::new();
        assert!(filter.num_buckets().is_power_of_two());
        assert_eq!(filter.bucket_size(), 4);
    }

    #[test]
    fn cuckoo_filter_capacity() {
        let filter = CuckooFilter::new();
        assert_eq!(
            filter.capacity(),
            filter.num_buckets() * filter.bucket_size()
        );
    }

    #[test]
    fn cuckoo_filter_delete_reduces_count() {
        let mut filter = CuckooFilter::new();
        filter.insert(&1);
        filter.insert(&2);
        assert_eq!(filter.count(), 2);
        filter.delete(&1);
        assert_eq!(filter.count(), 1);
    }

    #[test]
    fn cuckoo_config_min_bucket_size() {
        // bucket_size of 0 gets clamped to 1
        let filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 4,
            bucket_size: 0,
            max_kicks: 10,
        });
        assert_eq!(filter.bucket_size(), 1);
    }

    #[test]
    fn cuckoo_config_min_num_buckets() {
        // num_buckets rounds up to power of 2, minimum 2
        let filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 1,
            bucket_size: 4,
            max_kicks: 10,
        });
        assert!(filter.num_buckets() >= 2);
    }

    // ── Expanded coverage (DarkMill ft-353k0) ────────────────────────

    #[test]
    fn is_empty_after_insert_then_delete() {
        let mut filter = CuckooFilter::new();
        filter.insert(&99);
        assert!(!filter.is_empty());
        filter.delete(&99);
        assert!(filter.is_empty());
        assert_eq!(filter.count(), 0);
    }

    #[test]
    fn count_after_multiple_deletes() {
        let mut filter = CuckooFilter::new();
        for i in 0..5 {
            filter.insert(&i);
        }
        assert_eq!(filter.count(), 5);
        filter.delete(&0);
        filter.delete(&2);
        filter.delete(&4);
        assert_eq!(filter.count(), 2);
        assert!(filter.lookup(&1));
        assert!(filter.lookup(&3));
    }

    #[test]
    fn load_factor_increases_with_inserts() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 8,
            bucket_size: 4,
            max_kicks: 100,
        });
        let lf0 = filter.load_factor();
        assert_eq!(lf0, 0.0);
        filter.insert(&1);
        let lf1 = filter.load_factor();
        assert!(lf1 > lf0, "load_factor should increase after insert");
        filter.insert(&2);
        let lf2 = filter.load_factor();
        assert!(lf2 > lf1, "load_factor should increase further");
    }

    #[test]
    fn load_factor_bounded_by_one() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 4,
            bucket_size: 2,
            max_kicks: 50,
        });
        for i in 0..100 {
            filter.insert(&i);
        }
        assert!(filter.load_factor() <= 1.0, "load_factor should not exceed 1.0");
    }

    #[test]
    fn stats_count_matches_count_method() {
        let mut filter = CuckooFilter::new();
        for i in 0..10 {
            filter.insert(&i);
        }
        assert_eq!(filter.stats().count, filter.count());
    }

    #[test]
    fn stats_capacity_matches_capacity_method() {
        let filter = CuckooFilter::new();
        assert_eq!(filter.stats().capacity, filter.capacity());
    }

    #[test]
    fn stats_occupied_buckets_bounded() {
        let mut filter = CuckooFilter::new();
        for i in 0..50 {
            filter.insert(&i);
        }
        let stats = filter.stats();
        assert!(stats.occupied_buckets <= stats.num_buckets,
            "occupied {} > num_buckets {}", stats.occupied_buckets, stats.num_buckets);
    }

    #[test]
    fn stats_load_percent_bounded() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 4,
            bucket_size: 2,
            max_kicks: 50,
        });
        for i in 0..100 {
            filter.insert(&i);
        }
        assert!(filter.stats().load_percent <= 100);
    }

    #[test]
    fn with_capacity_holds_expected_items() {
        let expected = 500;
        let mut filter = CuckooFilter::with_capacity(expected);
        let mut inserted = 0;
        for i in 0..expected {
            if filter.insert(&i) == InsertResult::Ok {
                inserted += 1;
            }
        }
        // Should hold at least 90% of expected
        assert!(inserted >= expected * 9 / 10,
            "only inserted {} out of {} expected", inserted, expected);
    }

    #[test]
    fn with_config_custom_bucket_size() {
        let filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 16,
            bucket_size: 8,
            max_kicks: 100,
        });
        assert_eq!(filter.bucket_size(), 8);
        assert_eq!(filter.capacity(), 16 * 8);
    }

    #[test]
    fn clear_resets_load_factor() {
        let mut filter = CuckooFilter::new();
        for i in 0..50 {
            filter.insert(&i);
        }
        assert!(filter.load_factor() > 0.0);
        filter.clear();
        assert_eq!(filter.load_factor(), 0.0);
    }

    #[test]
    fn lookup_no_false_negatives() {
        let mut filter = CuckooFilter::new();
        let items: Vec<i32> = (0..200).collect();
        for &item in &items {
            filter.insert(&item);
        }
        for &item in &items {
            assert!(filter.lookup(&item), "false negative for item {}", item);
        }
    }

    #[test]
    fn delete_all_leaves_empty() {
        let mut filter = CuckooFilter::new();
        let n = 30;
        for i in 0..n {
            filter.insert(&i);
        }
        assert_eq!(filter.count(), n);
        for i in 0..n {
            assert!(filter.delete(&i), "should delete item {}", i);
        }
        assert!(filter.is_empty());
        assert_eq!(filter.count(), 0);
    }

    #[test]
    fn clone_independence() {
        let mut filter = CuckooFilter::new();
        filter.insert(&1);
        filter.insert(&2);
        let mut clone = filter.clone();
        clone.insert(&3);
        clone.delete(&1);
        // Original unchanged
        assert_eq!(filter.count(), 2);
        assert!(filter.lookup(&1));
        // Clone modified
        assert_eq!(clone.count(), 2);
        assert!(!clone.lookup(&1));
        assert!(clone.lookup(&3));
    }

    #[test]
    fn insert_various_hashable_types() {
        let mut filter = CuckooFilter::new();
        assert_eq!(filter.insert(&42i32), InsertResult::Ok);
        assert_eq!(filter.insert(&"hello"), InsertResult::Ok);
        assert_eq!(filter.insert(&true), InsertResult::Ok);
        assert_eq!(filter.insert(&(1u8, 2u8)), InsertResult::Ok);
        assert_eq!(filter.insert(&vec![1, 2, 3]), InsertResult::Ok);
        assert_eq!(filter.count(), 5);
    }

    #[test]
    fn double_delete_second_returns_false() {
        let mut filter = CuckooFilter::new();
        filter.insert(&42);
        assert!(filter.delete(&42));
        assert!(!filter.delete(&42), "second delete of same item should return false");
    }

    #[test]
    fn stats_after_clear_shows_zero() {
        let mut filter = CuckooFilter::new();
        for i in 0..20 {
            filter.insert(&i);
        }
        filter.clear();
        let stats = filter.stats();
        assert_eq!(stats.count, 0);
        assert_eq!(stats.load_percent, 0);
        assert_eq!(stats.occupied_buckets, 0);
    }

    #[test]
    fn small_max_kicks_fills_sooner() {
        let mut small_kicks = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 4,
            bucket_size: 2,
            max_kicks: 1,
        });
        let mut large_kicks = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 4,
            bucket_size: 2,
            max_kicks: 500,
        });
        let mut small_inserted = 0;
        let mut large_inserted = 0;
        for i in 0..100 {
            if small_kicks.insert(&i) == InsertResult::Ok {
                small_inserted += 1;
            }
            if large_kicks.insert(&i) == InsertResult::Ok {
                large_inserted += 1;
            }
        }
        assert!(large_inserted >= small_inserted,
            "more kicks should allow more inserts: large={}, small={}", large_inserted, small_inserted);
    }

    #[test]
    fn large_scale_insert_lookup() {
        let mut filter = CuckooFilter::with_capacity(2000);
        for i in 0..1000 {
            assert_eq!(filter.insert(&i), InsertResult::Ok);
        }
        assert_eq!(filter.count(), 1000);
        for i in 0..1000 {
            assert!(filter.lookup(&i));
        }
    }

    #[test]
    fn stress_interleaved_insert_delete() {
        let mut filter = CuckooFilter::new();
        // Insert 0..50
        for i in 0..50 {
            filter.insert(&i);
        }
        // Delete even numbers
        for i in (0..50).step_by(2) {
            assert!(filter.delete(&i));
        }
        assert_eq!(filter.count(), 25);
        // Odd numbers still present
        for i in (1..50).step_by(2) {
            assert!(filter.lookup(&i), "odd number {} should still be present", i);
        }
        // Insert more
        for i in 50..75 {
            filter.insert(&i);
        }
        assert_eq!(filter.count(), 50);
    }

    #[test]
    fn alt_index_always_within_bounds() {
        for n in [2, 4, 8, 16, 32, 64, 128, 256] {
            for i in 0..n {
                for fp in [1u32, 42, 255, 1000, u32::MAX] {
                    let j = alt_index(i, fp, n);
                    assert!(j < n, "alt_index({}, {}, {}) = {} out of bounds", i, fp, n, j);
                }
            }
        }
    }

    #[test]
    fn insert_after_full_returns_full() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 2,
            bucket_size: 1,
            max_kicks: 0,
        });
        // With 2 buckets, 1 slot each, and 0 kicks, capacity is 2
        let mut full_seen = false;
        for i in 0..100 {
            if filter.insert(&i) == InsertResult::Full {
                full_seen = true;
                break;
            }
        }
        assert!(full_seen, "should see InsertResult::Full on tiny filter");
    }

    #[test]
    fn occupied_buckets_zero_when_empty() {
        let filter = CuckooFilter::new();
        assert_eq!(filter.stats().occupied_buckets, 0);
    }

    #[test]
    fn occupied_buckets_increases_with_inserts() {
        let mut filter = CuckooFilter::with_config(CuckooConfig {
            num_buckets: 32,
            bucket_size: 4,
            max_kicks: 100,
        });
        let before = filter.stats().occupied_buckets;
        for i in 0..20 {
            filter.insert(&i);
        }
        let after = filter.stats().occupied_buckets;
        assert!(after > before, "occupied should increase after inserts");
    }
}
