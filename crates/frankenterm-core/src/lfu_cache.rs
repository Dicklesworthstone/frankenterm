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
struct CacheEntry<V> {
    value: V,
    frequency: u64,
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
    entries: HashMap<K, CacheEntry<V>>,
    /// Frequency -> ordered list of keys (oldest first = eviction candidate).
    frequency_buckets: HashMap<u64, Vec<K>>,
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
        if !self.entries.contains_key(key) {
            self.misses += 1;
            return None;
        }
        self.hits += 1;

        // Increment frequency
        let old_freq;
        {
            let entry = self.entries.get_mut(key).unwrap();
            old_freq = entry.frequency;
            entry.frequency += 1;
        }
        let new_freq = old_freq + 1;

        // Move key from old frequency bucket to new
        self.remove_from_bucket(old_freq, key);
        self.frequency_buckets
            .entry(new_freq)
            .or_default()
            .push(key.clone());

        // Update min_frequency if we emptied the min bucket
        if old_freq == self.min_frequency {
            if let Some(bucket) = self.frequency_buckets.get(&old_freq) {
                if bucket.is_empty() {
                    self.min_frequency = new_freq;
                }
            } else {
                self.min_frequency = new_freq;
            }
        }

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
        if self.entries.contains_key(&key) {
            let old_freq;
            {
                let entry = self.entries.get_mut(&key).unwrap();
                old_freq = entry.frequency;
                entry.frequency += 1;
                entry.value = value;
            }
            let new_freq = old_freq + 1;

            self.remove_from_bucket(old_freq, &key);
            self.frequency_buckets
                .entry(new_freq)
                .or_default()
                .push(key);

            if old_freq == self.min_frequency {
                if let Some(bucket) = self.frequency_buckets.get(&old_freq) {
                    if bucket.is_empty() {
                        self.min_frequency = new_freq;
                    }
                } else {
                    self.min_frequency = new_freq;
                }
            }

            return None;
        }

        // Evict if at capacity
        let evicted = if self.entries.len() >= self.capacity {
            self.evict()
        } else {
            None
        };

        // Insert new entry with frequency 1
        self.entries.insert(
            key.clone(),
            CacheEntry {
                value,
                frequency: 1,
            },
        );
        self.frequency_buckets.entry(1).or_default().push(key);
        self.min_frequency = 1;

        evicted
    }

    /// Remove a key explicitly. Returns the value if it existed.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        if let Some(entry) = self.entries.remove(key) {
            self.remove_from_bucket(entry.frequency, key);

            // Update min_frequency if needed
            if !self.entries.is_empty()
                && entry.frequency == self.min_frequency
            {
                if let Some(bucket) = self.frequency_buckets.get(&self.min_frequency) {
                    if bucket.is_empty() {
                        // Find next non-empty bucket
                        self.recompute_min_frequency();
                    }
                }
            }

            Some(entry.value)
        } else {
            None
        }
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

    fn remove_from_bucket(&mut self, freq: u64, key: &K) {
        if let Some(bucket) = self.frequency_buckets.get_mut(&freq) {
            if let Some(pos) = bucket.iter().position(|k| k == key) {
                bucket.remove(pos);
            }
        }
    }

    fn evict(&mut self) -> Option<(K, V)> {
        // Find the LFU bucket (min_frequency)
        if let Some(bucket) = self.frequency_buckets.get_mut(&self.min_frequency) {
            if let Some(evict_key) = bucket.first().cloned() {
                bucket.remove(0);
                if let Some(entry) = self.entries.remove(&evict_key) {
                    self.evictions += 1;

                    // If bucket is now empty, we'll set min_frequency when inserting new entry
                    return Some((evict_key, entry.value));
                }
            }
        }
        None
    }

    fn recompute_min_frequency(&mut self) {
        // Simple: find minimum frequency among all entries
        self.min_frequency = self
            .entries
            .values()
            .map(|e| e.frequency)
            .min()
            .unwrap_or(0);
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
        cache.peek(&"a");
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
}
