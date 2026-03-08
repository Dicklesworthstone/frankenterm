//! O(1) Least Frequently Used (LFU) cache.
//!
//! Implements the O(1) LFU eviction algorithm for fixed-capacity caches.
//! When the cache is full, the entry with the lowest access frequency is
//! evicted. Ties are broken by evicting the least recently used among
//! entries with the same frequency (LRU within each frequency bucket).
//!
//! # Use Cases
//!
//! - Evict least-used pane data under memory pressure
//! - Cache hot patterns/commands based on access frequency
//! - Resource pool management for infrequently used connections
//! - Complement to LRU cache for frequency-based eviction policies
//!
//! # Complexity
//!
//! | Operation | Time |
//! |-----------|------|
//! | `get`     | O(1) |
//! | `insert`  | O(1) amortized |
//! | `remove`  | O(1) |
//! | `peek`    | O(1) |
//!
//! Bead: ft-283h4.38

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::Hash;

/// Configuration for an LFU cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfuCacheConfig {
    /// Maximum number of entries.
    pub capacity: usize,
}

impl Default for LfuCacheConfig {
    fn default() -> Self {
        Self { capacity: 128 }
    }
}

/// Statistics about an LFU cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LfuCacheStats {
    /// Number of entries currently stored.
    pub entry_count: usize,
    /// Maximum capacity.
    pub capacity: usize,
    /// Total number of get hits.
    pub hits: u64,
    /// Total number of get misses.
    pub misses: u64,
    /// Total number of evictions.
    pub evictions: u64,
    /// Minimum frequency among stored entries (0 if empty).
    pub min_frequency: u64,
}

/// Internal entry storing value and metadata.
#[derive(Debug, Clone)]
struct CacheEntry<K, V> {
    value: V,
    frequency: u64,
    prev: Option<K>,
    next: Option<K>,
}

/// Keys in each frequency bucket are maintained in insertion/access order so
/// ties can be evicted by LRU policy within the same frequency.
#[derive(Debug, Clone)]
struct FrequencyBucket<K> {
    head: Option<K>,
    tail: Option<K>,
    len: usize,
    prev_frequency: Option<u64>,
    next_frequency: Option<u64>,
}

/// O(1) Least Frequently Used cache.
///
/// # Example
///
/// ```
/// use frankenterm_core::lfu_cache::LfuCache;
///
/// let mut cache: LfuCache<&str, i32> = LfuCache::new(2);
/// cache.insert("a", 1);
/// cache.insert("b", 2);
/// cache.get(&"a");        // frequency of "a" is now 2
/// cache.insert("c", 3);   // evicts "b" (freq=1, least frequent)
///
/// assert!(cache.get(&"a").is_some());
/// assert!(cache.get(&"b").is_none());
/// assert!(cache.get(&"c").is_some());
/// ```
#[derive(Debug, Clone)]
pub struct LfuCache<K, V> {
    /// Key -> entry mapping.
    entries: HashMap<K, CacheEntry<K, V>>,
    /// Frequency -> linked bucket of keys for O(1) promote/evict operations.
    frequency_buckets: HashMap<u64, FrequencyBucket<K>>,
    /// Maximum capacity.
    capacity: usize,
    /// Minimum frequency in the cache.
    min_frequency: u64,
    /// Hit counter.
    hits: u64,
    /// Miss counter.
    misses: u64,
    /// Eviction counter.
    evictions: u64,
}

impl<K: Hash + Eq + Clone, V> LfuCache<K, V> {
    /// Create a new LFU cache with the given capacity.
    ///
    /// A capacity of 0 means nothing can be stored.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            frequency_buckets: HashMap::new(),
            capacity,
            min_frequency: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// Create from config.
    #[must_use]
    pub fn from_config(config: &LfuCacheConfig) -> Self {
        Self::new(config.capacity)
    }

    /// Number of entries currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Maximum capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the cache is at capacity.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }

    /// Look up a key, returning a reference to the value.
    ///
    /// Increments the access frequency of the entry.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let old_frequency = if let Some(entry) = self.entries.get(key) {
            entry.frequency
        } else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;

        self.bump_frequency(key.clone(), old_frequency);

        self.entries.get(key).map(|e| &e.value)
    }

    /// Look up a key without incrementing frequency.
    #[must_use]
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|e| &e.value)
    }

    /// Check if a key exists without incrementing frequency.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Insert a key-value pair. Returns the evicted key-value pair if the
    /// cache was full and an eviction occurred.
    ///
    /// If the key already exists, the value is updated and the frequency
    /// is incremented.
    pub fn insert(&mut self, key: K, value: V) -> Option<(K, V)> {
        if self.capacity == 0 {
            return None;
        }

        // If key exists, update in place and increment frequency
        if let Some(old_frequency) = self.entries.get(&key).map(|entry| entry.frequency) {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.value = value;
            }
            self.bump_frequency(key, old_frequency);
            return None;
        }

        // Evict if at capacity
        let evicted = if self.entries.len() >= self.capacity {
            self.evict_lfu()
        } else {
            None
        };

        // Insert new entry with frequency 1
        self.entries.insert(
            key.clone(),
            CacheEntry {
                value,
                frequency: 1,
                prev: None,
                next: None,
            },
        );
        self.ensure_frequency_bucket_as_min(1);
        self.link_key_to_bucket_tail(&key, 1);
        self.min_frequency = 1;

        evicted
    }

    /// Remove a key explicitly. Returns the value if it existed.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.remove_internal(key, false).map(|(_, value)| value)
    }

    /// Get the frequency of a key, or None if not present.
    #[must_use]
    pub fn frequency(&self, key: &K) -> Option<u64> {
        self.entries.get(key).map(|e| e.frequency)
    }

    /// Get all keys in the cache.
    pub fn keys(&self) -> Vec<K> {
        self.entries.keys().cloned().collect()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.frequency_buckets.clear();
        self.min_frequency = 0;
    }

    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> LfuCacheStats {
        LfuCacheStats {
            entry_count: self.entries.len(),
            capacity: self.capacity,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            min_frequency: if self.entries.is_empty() {
                0
            } else {
                self.min_frequency
            },
        }
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn bump_frequency(&mut self, key: K, old_frequency: u64) {
        let new_frequency = old_frequency.saturating_add(1);
        self.ensure_frequency_bucket_after(new_frequency, old_frequency);

        let (prev, next) = if let Some(entry) = self.entries.get(&key) {
            (entry.prev.clone(), entry.next.clone())
        } else {
            return;
        };
        self.unlink_from_bucket(&key, old_frequency, prev, next);

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.frequency = new_frequency;
        }
        self.link_key_to_bucket_tail(&key, new_frequency);
    }

    fn remove_internal(&mut self, key: &K, count_eviction: bool) -> Option<(K, V)> {
        let (frequency, prev, next) = if let Some(entry) = self.entries.get(key) {
            (entry.frequency, entry.prev.clone(), entry.next.clone())
        } else {
            return None;
        };

        self.unlink_from_bucket(key, frequency, prev, next);
        let removed = self.entries.remove(key)?;

        if self.entries.is_empty() {
            self.min_frequency = 0;
        }
        if count_eviction {
            self.evictions = self.evictions.saturating_add(1);
        }

        Some((key.clone(), removed.value))
    }

    fn unlink_from_bucket(&mut self, key: &K, frequency: u64, prev: Option<K>, next: Option<K>) {
        if let Some(prev_key) = prev.as_ref()
            && let Some(prev_entry) = self.entries.get_mut(prev_key)
        {
            prev_entry.next.clone_from(&next);
        }
        if let Some(next_key) = next.as_ref()
            && let Some(next_entry) = self.entries.get_mut(next_key)
        {
            next_entry.prev.clone_from(&prev);
        }

        let mut remove_bucket = false;
        let mut bucket_prev_frequency = None;
        let mut bucket_next_frequency = None;

        if let Some(bucket) = self.frequency_buckets.get_mut(&frequency) {
            if bucket
                .head
                .as_ref()
                .is_some_and(|bucket_head| bucket_head == key)
            {
                bucket.head.clone_from(&next);
            }
            if bucket
                .tail
                .as_ref()
                .is_some_and(|bucket_tail| bucket_tail == key)
            {
                bucket.tail.clone_from(&prev);
            }
            bucket.len = bucket.len.saturating_sub(1);

            if bucket.len == 0 {
                remove_bucket = true;
                bucket_prev_frequency = bucket.prev_frequency;
                bucket_next_frequency = bucket.next_frequency;
            }
        }

        if let Some(entry) = self.entries.get_mut(key) {
            entry.prev = None;
            entry.next = None;
        }

        if remove_bucket {
            self.frequency_buckets.remove(&frequency);

            if let Some(prev_frequency) = bucket_prev_frequency
                && let Some(prev_bucket) = self.frequency_buckets.get_mut(&prev_frequency)
            {
                prev_bucket.next_frequency = bucket_next_frequency;
            }
            if let Some(next_frequency) = bucket_next_frequency
                && let Some(next_bucket) = self.frequency_buckets.get_mut(&next_frequency)
            {
                next_bucket.prev_frequency = bucket_prev_frequency;
            }

            if self.min_frequency == frequency {
                self.min_frequency = bucket_next_frequency.or(bucket_prev_frequency).unwrap_or(0);
            }
        }
    }

    fn link_key_to_bucket_tail(&mut self, key: &K, frequency: u64) {
        let current_tail = self
            .frequency_buckets
            .get(&frequency)
            .and_then(|bucket| bucket.tail.clone());

        if let Some(tail_key) = current_tail.as_ref()
            && let Some(tail_entry) = self.entries.get_mut(tail_key)
        {
            tail_entry.next = Some(key.clone());
        }

        if let Some(entry) = self.entries.get_mut(key) {
            entry.prev = current_tail;
            entry.next = None;
            entry.frequency = frequency;
        }

        if let Some(bucket) = self.frequency_buckets.get_mut(&frequency) {
            if bucket.head.is_none() {
                bucket.head = Some(key.clone());
            }
            bucket.tail = Some(key.clone());
            bucket.len += 1;
        }
    }

    fn ensure_frequency_bucket_as_min(&mut self, frequency: u64) {
        if self.frequency_buckets.contains_key(&frequency) {
            return;
        }

        if self.min_frequency == 0 {
            self.frequency_buckets.insert(
                frequency,
                FrequencyBucket {
                    head: None,
                    tail: None,
                    len: 0,
                    prev_frequency: None,
                    next_frequency: None,
                },
            );
            self.min_frequency = frequency;
            return;
        }

        let current_min_frequency = self.min_frequency;
        self.frequency_buckets.insert(
            frequency,
            FrequencyBucket {
                head: None,
                tail: None,
                len: 0,
                prev_frequency: None,
                next_frequency: Some(current_min_frequency),
            },
        );

        if let Some(current_min_bucket) = self.frequency_buckets.get_mut(&current_min_frequency) {
            current_min_bucket.prev_frequency = Some(frequency);
        }
        self.min_frequency = frequency;
    }

    fn ensure_frequency_bucket_after(&mut self, frequency: u64, previous_frequency: u64) {
        if self.frequency_buckets.contains_key(&frequency) {
            return;
        }
        let Some(previous_bucket) = self.frequency_buckets.get(&previous_frequency) else {
            self.ensure_frequency_bucket_as_min(frequency);
            return;
        };
        let next_frequency = previous_bucket.next_frequency;

        self.frequency_buckets.insert(
            frequency,
            FrequencyBucket {
                head: None,
                tail: None,
                len: 0,
                prev_frequency: Some(previous_frequency),
                next_frequency,
            },
        );

        if let Some(previous_bucket_mut) = self.frequency_buckets.get_mut(&previous_frequency) {
            previous_bucket_mut.next_frequency = Some(frequency);
        }
        if let Some(next_frequency) = next_frequency
            && let Some(next_bucket) = self.frequency_buckets.get_mut(&next_frequency)
        {
            next_bucket.prev_frequency = Some(frequency);
        }
    }

    fn evict_lfu(&mut self) -> Option<(K, V)> {
        let evict_key = self
            .frequency_buckets
            .get(&self.min_frequency)
            .and_then(|bucket| bucket.head.clone())?;
        self.remove_internal(&evict_key, true)
    }
}

impl<K: Hash + Eq + Clone, V> Default for LfuCache<K, V> {
    fn default() -> Self {
        Self::new(LfuCacheConfig::default().capacity)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_cache() {
        let cache: LfuCache<String, i32> = LfuCache::new(5);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.capacity(), 5);
    }

    #[test]
    fn test_insert_and_get() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), None);
    }

    #[test]
    fn test_eviction_lfu() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.get(&"a"); // a: freq=2, b: freq=1
        cache.insert("c", 3); // evicts b (lowest freq)
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn test_eviction_lru_tiebreak() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1); // freq=1, inserted first
        cache.insert("b", 2); // freq=1, inserted second
        cache.insert("c", 3); // evicts a (same freq, but a is oldest)
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn test_update_existing() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("a", 10); // updates value, increments freq
        assert_eq!(cache.peek(&"a"), Some(&10));
        assert_eq!(cache.frequency(&"a"), Some(2)); // freq incremented
    }

    #[test]
    fn test_remove() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        assert_eq!(cache.remove(&"a"), Some(1));
        assert_eq!(cache.len(), 1);
        assert!(!cache.contains_key(&"a"));
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut cache: LfuCache<&str, i32> = LfuCache::new(3);
        assert_eq!(cache.remove(&"x"), None);
    }

    #[test]
    fn test_peek_doesnt_increment() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        assert_eq!(cache.frequency(&"a"), Some(1));
        let _ = cache.peek(&"a");
        assert_eq!(cache.frequency(&"a"), Some(1)); // unchanged
    }

    #[test]
    fn test_get_increments_frequency() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        assert_eq!(cache.frequency(&"a"), Some(1));
        cache.get(&"a");
        assert_eq!(cache.frequency(&"a"), Some(2));
        cache.get(&"a");
        assert_eq!(cache.frequency(&"a"), Some(3));
    }

    #[test]
    fn test_zero_capacity() {
        let mut cache: LfuCache<&str, i32> = LfuCache::new(0);
        assert_eq!(cache.insert("a", 1), None);
        assert!(cache.is_empty());
        assert_eq!(cache.get(&"a"), None);
    }

    #[test]
    fn test_is_full() {
        let mut cache = LfuCache::new(2);
        assert!(!cache.is_full());
        cache.insert("a", 1);
        assert!(!cache.is_full());
        cache.insert("b", 2);
        assert!(cache.is_full());
    }

    #[test]
    fn test_clear() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_keys() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        let mut keys = cache.keys();
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn test_stats() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.get(&"a"); // hit
        cache.get(&"b"); // miss
        cache.insert("b", 2);
        cache.insert("c", 3); // eviction

        let stats = cache.stats();
        assert_eq!(stats.entry_count, 2);
        assert_eq!(stats.capacity, 2);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn test_config_serde() {
        let config = LfuCacheConfig { capacity: 42 };
        let json = serde_json::to_string(&config).unwrap();
        let back: LfuCacheConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn test_stats_serde() {
        let mut cache = LfuCache::new(5);
        cache.insert("a", 1);
        let stats = cache.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: LfuCacheStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn test_from_config() {
        let config = LfuCacheConfig { capacity: 16 };
        let cache: LfuCache<String, i32> = LfuCache::from_config(&config);
        assert_eq!(cache.capacity(), 16);
    }

    #[test]
    fn test_clone_independence() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        let mut clone = cache.clone();
        clone.insert("b", 2);
        assert_eq!(cache.len(), 1);
        assert_eq!(clone.len(), 2);
    }

    #[test]
    fn test_complex_eviction_sequence() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1); // freq=1
        cache.insert("b", 2); // freq=1
        cache.insert("c", 3); // freq=1

        // Boost a and c
        cache.get(&"a"); // a: freq=2
        cache.get(&"c"); // c: freq=2

        // Insert d — should evict b (freq=1, lowest)
        cache.insert("d", 4);
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"c"), Some(&3));
        assert_eq!(cache.get(&"d"), Some(&4));
    }

    #[test]
    fn test_eviction_returns_correct_pair() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.get(&"b"); // b: freq=2

        let evicted = cache.insert("c", 3);
        assert_eq!(evicted, Some(("a", 1)));
    }

    #[test]
    fn test_single_capacity() {
        let mut cache = LfuCache::new(1);
        cache.insert("a", 1);
        assert_eq!(cache.get(&"a"), Some(&1));

        let evicted = cache.insert("b", 2);
        assert_eq!(evicted, Some(("a", 1)));
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn test_contains_key() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        assert!(cache.contains_key(&"a"));
        assert!(!cache.contains_key(&"b"));
    }

    // ── Additional tests ──────────────────────────────────────────────

    #[test]
    fn test_default_cache() {
        let cache: LfuCache<String, i32> = LfuCache::default();
        assert!(cache.is_empty());
        assert_eq!(cache.capacity(), 128);
    }

    #[test]
    fn test_get_miss_increments() {
        let mut cache: LfuCache<&str, i32> = LfuCache::new(5);
        cache.get(&"x");
        cache.get(&"y");
        cache.get(&"z");
        let stats = cache.stats();
        assert_eq!(stats.misses, 3);
        assert_eq!(stats.hits, 0);
    }

    #[test]
    fn test_multiple_evictions() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        let ev1 = cache.insert("c", 3); // evicts a
        assert_eq!(ev1, Some(("a", 1)));
        let ev2 = cache.insert("d", 4); // evicts b or c (both freq=1, b is older)
        assert_eq!(ev2, Some(("b", 2)));
    }

    #[test]
    fn test_frequency_increases_on_get() {
        let mut cache = LfuCache::new(5);
        cache.insert("a", 1);
        assert_eq!(cache.frequency(&"a"), Some(1));
        cache.get(&"a");
        assert_eq!(cache.frequency(&"a"), Some(2));
        cache.get(&"a");
        cache.get(&"a");
        assert_eq!(cache.frequency(&"a"), Some(4));
    }

    #[test]
    fn test_frequency_increases_on_update() {
        let mut cache = LfuCache::new(5);
        cache.insert("a", 1);
        assert_eq!(cache.frequency(&"a"), Some(1));
        cache.insert("a", 10); // update increments frequency
        assert_eq!(cache.frequency(&"a"), Some(2));
        assert_eq!(cache.peek(&"a"), Some(&10));
    }

    #[test]
    fn test_frequency_nonexistent() {
        let cache: LfuCache<&str, i32> = LfuCache::new(5);
        assert_eq!(cache.frequency(&"x"), None);
    }

    #[test]
    fn test_min_frequency_tracking() {
        let mut cache = LfuCache::new(5);
        let stats = cache.stats();
        assert_eq!(stats.min_frequency, 0); // empty cache
        cache.insert("a", 1);
        let stats = cache.stats();
        assert_eq!(stats.min_frequency, 1); // just inserted
        cache.get(&"a"); // freq=2
        cache.insert("b", 2); // min_frequency resets to 1
        let stats = cache.stats();
        assert_eq!(stats.min_frequency, 1);
    }

    #[test]
    fn test_eviction_all_same_frequency() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1); // freq=1, oldest
        cache.insert("b", 2); // freq=1
        cache.insert("c", 3); // freq=1, newest
        // All same frequency — evicts by LRU (oldest = "a")
        let evicted = cache.insert("d", 4);
        assert_eq!(evicted, Some(("a", 1)));
    }

    #[test]
    fn test_eviction_respects_frequency_over_recency() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);
        // Boost a and b
        cache.get(&"a"); // freq=2
        cache.get(&"b"); // freq=2
        // c has freq=1, should be evicted
        let evicted = cache.insert("d", 4);
        assert_eq!(evicted, Some(("c", 3)));
    }

    #[test]
    fn test_remove_from_full_then_insert() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        assert!(cache.is_full());
        cache.remove(&"a");
        assert!(!cache.is_full());
        let evicted = cache.insert("c", 3);
        assert_eq!(evicted, None); // no eviction needed
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_contains_key_after_eviction() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3); // evicts a
        assert!(!cache.contains_key(&"a"));
        assert!(cache.contains_key(&"b"));
        assert!(cache.contains_key(&"c"));
    }

    #[test]
    fn test_is_full_after_eviction() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1);
        cache.insert("b", 2);
        assert!(cache.is_full());
        cache.insert("c", 3); // evicts one, inserts one — still full
        assert!(cache.is_full());
    }

    #[test]
    fn test_peek_nonexistent() {
        let cache: LfuCache<&str, i32> = LfuCache::new(5);
        assert_eq!(cache.peek(&"x"), None);
    }

    #[test]
    fn test_peek_doesnt_count_stats() {
        let mut cache = LfuCache::new(5);
        cache.insert("a", 1);
        let _ = cache.peek(&"a");
        let _ = cache.peek(&"b");
        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 0);
    }

    #[test]
    fn test_clear_then_reinsert() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.get(&"a");
        cache.clear();
        assert!(cache.is_empty());
        cache.insert("c", 3);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn test_keys_unordered() {
        let mut cache = LfuCache::new(5);
        cache.insert("z", 26);
        cache.insert("a", 1);
        cache.insert("m", 13);
        let mut keys = cache.keys();
        keys.sort();
        assert_eq!(keys, vec!["a", "m", "z"]);
    }

    #[test]
    fn test_integer_keys() {
        let mut cache = LfuCache::new(3);
        cache.insert(1, "one");
        cache.insert(2, "two");
        cache.insert(3, "three");
        assert_eq!(cache.get(&1), Some(&"one"));
        cache.insert(4, "four"); // evicts least freq
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_string_owned_keys() {
        let mut cache = LfuCache::new(2);
        cache.insert("hello".to_string(), 1);
        cache.insert("world".to_string(), 2);
        assert_eq!(cache.get(&"hello".to_string()), Some(&1));
    }

    #[test]
    fn test_update_same_key_many_times() {
        let mut cache = LfuCache::new(3);
        for i in 0..10 {
            cache.insert("a", i);
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.peek(&"a"), Some(&9));
        assert_eq!(cache.frequency(&"a"), Some(10));
    }

    #[test]
    fn test_eviction_counter() {
        let mut cache = LfuCache::new(1);
        cache.insert("a", 1);
        cache.insert("b", 2); // evicts a
        cache.insert("c", 3); // evicts b
        let stats = cache.stats();
        assert_eq!(stats.evictions, 2);
    }

    #[test]
    fn test_remove_does_not_count_eviction() {
        let mut cache = LfuCache::new(5);
        cache.insert("a", 1);
        cache.remove(&"a");
        let stats = cache.stats();
        assert_eq!(stats.evictions, 0);
    }

    #[test]
    fn test_capacity_one_repeated_insert() {
        let mut cache = LfuCache::new(1);
        for i in 0..10 {
            cache.insert(i, i * 10);
        }
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&9), Some(&90));
    }

    #[test]
    fn test_large_cache_stress() {
        let mut cache = LfuCache::new(100);
        for i in 0..200 {
            cache.insert(i, i * 10);
        }
        assert_eq!(cache.len(), 100);
        let stats = cache.stats();
        assert_eq!(stats.evictions, 100);
    }

    #[test]
    fn test_frequency_promotion_chain() {
        let mut cache = LfuCache::new(3);
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.insert("c", 3);
        // Promote a to freq=3
        cache.get(&"a");
        cache.get(&"a");
        // Promote b to freq=2
        cache.get(&"b");
        // c stays at freq=1 — should be evicted
        let evicted = cache.insert("d", 4);
        assert_eq!(evicted, Some(("c", 3)));
        // Now d(freq=1) is lowest, should be next eviction
        let evicted2 = cache.insert("e", 5);
        assert_eq!(evicted2, Some(("d", 4)));
    }

    #[test]
    fn test_stats_comprehensive() {
        let mut cache = LfuCache::new(2);
        cache.insert("a", 1); // insert
        cache.insert("b", 2); // insert
        cache.get(&"a"); // hit
        cache.get(&"c"); // miss
        cache.insert("c", 3); // evict + insert

        let stats = cache.stats();
        assert_eq!(stats.entry_count, 2);
        assert_eq!(stats.capacity, 2);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn test_config_default() {
        let config = LfuCacheConfig::default();
        assert_eq!(config.capacity, 128);
    }

    #[test]
    fn test_config_equality() {
        let c1 = LfuCacheConfig { capacity: 42 };
        let c2 = LfuCacheConfig { capacity: 42 };
        let c3 = LfuCacheConfig { capacity: 100 };
        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
    }

    #[test]
    fn test_stats_equality() {
        let mut cache = LfuCache::new(5);
        cache.insert("a", 1);
        let s1 = cache.stats();
        let s2 = cache.stats();
        assert_eq!(s1, s2);
    }

    mod proptest_lfu_cache {
        use super::*;
        use proptest::prelude::*;

        fn arb_entries(max_len: usize) -> impl Strategy<Value = Vec<(u16, i64)>> {
            proptest::collection::vec((0u16..80, -500i64..500), 0..=max_len)
        }

        #[derive(Debug, Clone)]
        enum Op {
            Insert(u16, i64),
            Get(u16),
            Peek(u16),
            Remove(u16),
        }

        fn arb_op() -> impl Strategy<Value = Op> {
            prop_oneof![
                (0u16..40, -200i64..200).prop_map(|(k, v)| Op::Insert(k, v)),
                (0u16..40).prop_map(Op::Get),
                (0u16..40).prop_map(Op::Peek),
                (0u16..40).prop_map(Op::Remove),
            ]
        }

        fn arb_ops(max_len: usize) -> impl Strategy<Value = Vec<Op>> {
            proptest::collection::vec(arb_op(), 0..=max_len)
        }

        fn assert_invariants(cache: &LfuCache<u16, i64>) {
            assert!(cache.len() <= cache.capacity());
            assert_eq!(cache.is_empty(), cache.len() == 0);
            assert_eq!(cache.is_full(), cache.len() >= cache.capacity());
            // Every key in entries should be peek-able
            for key in cache.keys() {
                assert!(cache.contains_key(&key));
                assert!(cache.peek(&key).is_some());
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            #[test]
            fn len_bounded_by_capacity(
                cap in 1usize..=20,
                entries in arb_entries(60)
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                prop_assert!(cache.len() <= cap);
                assert_invariants(&cache);
            }

            #[test]
            fn get_after_insert_returns_value(
                cap in 1usize..=20,
                entries in arb_entries(40),
                target in 0u16..80
            ) {
                let mut cache = LfuCache::new(cap);
                let mut last_value = None;
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                    if *k == target {
                        last_value = Some(*v);
                    }
                }
                if let Some(expected) = last_value {
                    // Key might have been evicted, but if present, value matches
                    if let Some(&actual) = cache.peek(&target) {
                        prop_assert_eq!(actual, expected);
                    }
                }
            }

            #[test]
            fn peek_does_not_change_frequency(
                cap in 2usize..=10,
                entries in arb_entries(20),
                peek_key in 0u16..80
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                let freq_before = cache.frequency(&peek_key);
                let _ = cache.peek(&peek_key);
                let freq_after = cache.frequency(&peek_key);
                prop_assert_eq!(freq_before, freq_after);
            }

            #[test]
            fn get_increments_frequency(
                cap in 2usize..=10,
                entries in arb_entries(15),
                target in 0u16..80
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                if cache.contains_key(&target) {
                    let freq_before = cache.frequency(&target).unwrap();
                    cache.get(&target);
                    let freq_after = cache.frequency(&target).unwrap();
                    prop_assert_eq!(freq_after, freq_before + 1);
                }
            }

            #[test]
            fn remove_makes_absent(
                cap in 1usize..=10,
                entries in arb_entries(20),
                target in 0u16..80
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                cache.remove(&target);
                prop_assert!(!cache.contains_key(&target));
                prop_assert!(cache.peek(&target).is_none());
                assert_invariants(&cache);
            }

            #[test]
            fn clear_empties_everything(
                cap in 1usize..=10,
                entries in arb_entries(20)
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                cache.clear();
                prop_assert!(cache.is_empty());
                prop_assert_eq!(cache.len(), 0);
                assert_invariants(&cache);
            }

            #[test]
            fn eviction_targets_lowest_frequency(
                entries in arb_entries(10)
            ) {
                // Cache of size 2: insert items, access some to raise frequency,
                // then insert a new item. The evicted should be lowest frequency.
                let mut cache = LfuCache::new(2);
                cache.insert(1u16, 100);
                cache.insert(2u16, 200);
                // Access key=1 to raise its frequency
                cache.get(&1u16);
                // Now key=1 has freq=2, key=2 has freq=1
                // Inserting key=3 should evict key=2 (lowest freq)
                let evicted = cache.insert(3u16, 300);
                if let Some((ek, _)) = evicted {
                    prop_assert_eq!(ek, 2u16);
                }
                prop_assert!(cache.contains_key(&1u16));
                prop_assert!(cache.contains_key(&3u16));
                prop_assert!(!cache.contains_key(&2u16));
            }

            #[test]
            fn stats_hits_plus_misses_equals_gets(
                cap in 1usize..=10,
                entries in arb_entries(20),
                queries in proptest::collection::vec(0u16..80, 1..=30)
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                let hits_before = cache.stats().hits;
                let misses_before = cache.stats().misses;
                let total_gets = queries.len() as u64;
                for q in &queries {
                    cache.get(q);
                }
                let stats = cache.stats();
                let delta_hits = stats.hits - hits_before;
                let delta_misses = stats.misses - misses_before;
                prop_assert_eq!(delta_hits + delta_misses, total_gets);
            }

            #[test]
            fn insert_existing_updates_value(
                cap in 1usize..=10,
                key in 0u16..40,
                v1 in -200i64..200,
                v2 in -200i64..200
            ) {
                let mut cache = LfuCache::new(cap);
                cache.insert(key, v1);
                let len_before = cache.len();
                let evicted = cache.insert(key, v2);
                prop_assert!(evicted.is_none());
                prop_assert_eq!(cache.len(), len_before);
                prop_assert_eq!(cache.peek(&key), Some(&v2));
            }

            #[test]
            fn random_ops_maintain_invariants(
                cap in 1usize..=15,
                ops in arb_ops(60)
            ) {
                let mut cache = LfuCache::new(cap);
                for op in &ops {
                    match op {
                        Op::Insert(k, v) => { cache.insert(*k, *v); }
                        Op::Get(k) => { cache.get(k); }
                        Op::Peek(k) => { cache.peek(k); }
                        Op::Remove(k) => { cache.remove(k); }
                    }
                    assert_invariants(&cache);
                }
            }

            #[test]
            fn stats_serde_roundtrip(
                cap in 1usize..=10,
                entries in arb_entries(15)
            ) {
                let mut cache = LfuCache::new(cap);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                let stats = cache.stats();
                let json = serde_json::to_string(&stats).unwrap();
                let back: LfuCacheStats = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(stats, back);
            }

            #[test]
            fn zero_capacity_stores_nothing(entries in arb_entries(10)) {
                let mut cache = LfuCache::new(0);
                for (k, v) in &entries {
                    cache.insert(*k, *v);
                }
                prop_assert!(cache.is_empty());
                prop_assert_eq!(cache.len(), 0);
            }

            #[test]
            fn frequency_starts_at_one_on_insert(
                cap in 1usize..=10,
                key in 0u16..40,
                val in -200i64..200
            ) {
                let mut cache = LfuCache::new(cap);
                cache.insert(key, val);
                prop_assert_eq!(cache.frequency(&key), Some(1));
            }
        }
    }
}
