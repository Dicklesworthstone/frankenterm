//! Bounded LRU (Least Recently Used) cache with O(1) operations.
//!
//! Uses a HashMap for key→index lookup and an arena-based doubly-linked list
//! for recency ordering. All get/put/remove operations are O(1) amortized.
//! No unsafe code — uses Vec<Node> with index-based links instead of raw pointers.
//!
//! # Features
//! - O(1) get, put, remove, peek
//! - Bounded capacity with automatic LRU eviction
//! - Hit/miss/eviction statistics
//! - Iterators (MRU→LRU and LRU→MRU order)
//! - Dynamic resize with bulk eviction
//!
//! # Example
//! ```
//! use frankenterm_core::lru_cache::LruCache;
//!
//! let mut cache = LruCache::new(3);
//! cache.put(1, "one");
//! cache.put(2, "two");
//! cache.put(3, "three");
//!
//! assert_eq!(cache.get(&1), Some(&"one"));
//! // 1 is now most-recently used, 2 is least-recently used
//!
//! cache.put(4, "four"); // evicts key=2 (LRU)
//! assert_eq!(cache.get(&2), None);
//! ```

use std::collections::HashMap;
use std::hash::Hash;

/// Sentinel value for null links in the doubly-linked list.
const SENTINEL: usize = usize::MAX;

/// A node in the arena-based doubly-linked list.
/// Uses Option<V> to allow safe extraction of values on removal/eviction.
#[derive(Debug)]
struct Node<K, V> {
    key: K,
    value: Option<V>,
    prev: usize,
    next: usize,
}

/// Cache hit/miss/eviction statistics.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub insertions: u64,
    pub updates: u64,
    pub removals: u64,
}

impl CacheStats {
    /// Hit rate as a fraction [0.0, 1.0]. Returns 0.0 if no lookups.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Total number of get() calls (hits + misses).
    pub fn total_lookups(&self) -> u64 {
        self.hits + self.misses
    }
}

/// Bounded LRU cache with O(1) operations.
///
/// Internally stores entries in a `Vec<Node>` arena with index-based
/// doubly-linked list links. A `HashMap<K, usize>` maps keys to arena indices.
/// The linked list maintains recency order: head = most recent, tail = least recent.
pub struct LruCache<K, V> {
    /// Maximum number of entries.
    capacity: usize,
    /// Key → arena index mapping.
    map: HashMap<K, usize>,
    /// Arena of nodes.
    arena: Vec<Node<K, V>>,
    /// Index of most-recently used node (head of list).
    head: usize,
    /// Index of least-recently used node (tail of list).
    tail: usize,
    /// Free-list head for recycling removed slots.
    free_head: usize,
    /// Cache statistics.
    stats: CacheStats,
}

impl<K, V> std::fmt::Debug for LruCache<K, V>
where
    K: std::fmt::Debug,
    V: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LruCache")
            .field("capacity", &self.capacity)
            .field("len", &self.map.len())
            .field("stats", &self.stats)
            .finish()
    }
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    /// Create a new LRU cache with the given maximum capacity.
    ///
    /// # Panics
    /// Panics if `capacity` is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "LruCache capacity must be > 0");
        Self {
            capacity,
            map: HashMap::with_capacity(capacity),
            arena: Vec::with_capacity(capacity),
            head: SENTINEL,
            tail: SENTINEL,
            free_head: SENTINEL,
            stats: CacheStats::default(),
        }
    }

    /// Returns the maximum capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of entries currently stored.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns true if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Returns a reference to the cache statistics.
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Resets the statistics counters.
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
    }

    /// Get a reference to the value for `key`, promoting it to most-recently used.
    /// Returns `None` if the key is not present.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if let Some(&idx) = self.map.get(key) {
            self.move_to_head(idx);
            self.stats.hits += 1;
            self.arena[idx].value.as_ref()
        } else {
            self.stats.misses += 1;
            None
        }
    }

    /// Get a mutable reference to the value for `key`, promoting it to most-recently used.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        if let Some(&idx) = self.map.get(key) {
            self.move_to_head(idx);
            self.stats.hits += 1;
            self.arena[idx].value.as_mut()
        } else {
            self.stats.misses += 1;
            None
        }
    }

    /// Peek at the value for `key` without promoting it (no recency change).
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.map
            .get(key)
            .and_then(|&idx| self.arena[idx].value.as_ref())
    }

    /// Returns true if the cache contains the given key (without promoting it).
    pub fn contains_key(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Insert or update a key-value pair. If the key already exists, updates the
    /// value and promotes to most-recently used, returning `None`.
    /// If at capacity, evicts the least-recently used entry and returns
    /// `Some((evicted_key, evicted_value))`.
    pub fn put(&mut self, key: K, value: V) -> Option<(K, V)> {
        if let Some(&idx) = self.map.get(&key) {
            // Update existing entry
            self.arena[idx].value = Some(value);
            self.move_to_head(idx);
            self.stats.updates += 1;
            return None;
        }

        // New entry — may need to evict
        let evicted = if self.map.len() >= self.capacity {
            self.evict_tail()
        } else {
            None
        };

        // Allocate slot
        let idx = self.alloc_slot(key.clone(), value);

        // Link at head
        self.push_head(idx);
        self.map.insert(key, idx);
        self.stats.insertions += 1;

        evicted
    }

    /// Remove a key from the cache, returning its value if present.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        if let Some(idx) = self.map.remove(key) {
            self.unlink(idx);
            let value = self.arena[idx].value.take();
            // Add to free list
            self.arena[idx].next = self.free_head;
            self.free_head = idx;
            self.stats.removals += 1;
            value
        } else {
            None
        }
    }

    /// Peek at the least-recently used entry without removing it.
    pub fn peek_lru(&self) -> Option<(&K, &V)> {
        if self.tail == SENTINEL {
            None
        } else {
            let node = &self.arena[self.tail];
            node.value.as_ref().map(|v| (&node.key, v))
        }
    }

    /// Peek at the most-recently used entry without removing it.
    pub fn peek_mru(&self) -> Option<(&K, &V)> {
        if self.head == SENTINEL {
            None
        } else {
            let node = &self.arena[self.head];
            node.value.as_ref().map(|v| (&node.key, v))
        }
    }

    /// Remove and return the least-recently used entry.
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        self.evict_tail()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.map.clear();
        self.arena.clear();
        self.head = SENTINEL;
        self.tail = SENTINEL;
        self.free_head = SENTINEL;
    }

    /// Iterate over entries from most-recently used to least-recently used.
    pub fn iter_mru(&self) -> MruIter<'_, K, V> {
        MruIter {
            arena: &self.arena,
            current: self.head,
            remaining: self.map.len(),
        }
    }

    /// Iterate over entries from least-recently used to most-recently used.
    pub fn iter_lru(&self) -> LruIter<'_, K, V> {
        LruIter {
            arena: &self.arena,
            current: self.tail,
            remaining: self.map.len(),
        }
    }

    /// Resize the cache capacity. If the new capacity is smaller, evicts
    /// the least-recently used entries until the size fits.
    /// Returns a Vec of evicted (key, value) pairs.
    ///
    /// # Panics
    /// Panics if `new_capacity` is 0.
    pub fn resize(&mut self, new_capacity: usize) -> Vec<(K, V)> {
        assert!(new_capacity > 0, "LruCache capacity must be > 0");
        let mut evicted = Vec::new();
        while self.map.len() > new_capacity {
            if let Some(pair) = self.evict_tail() {
                evicted.push(pair);
            }
        }
        self.capacity = new_capacity;
        evicted
    }

    /// Retain only entries for which the predicate returns true.
    /// Entries are visited in LRU→MRU order. Removed entries don't count as evictions.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&K, &V) -> bool,
    {
        // Collect keys to remove (can't mutate while iterating)
        let keys_to_remove: Vec<K> = self
            .iter_lru()
            .filter(|(k, v)| !f(k, v))
            .map(|(k, _)| k.clone())
            .collect();

        for key in keys_to_remove {
            self.remove(&key);
        }
    }

    // --- Internal linked-list operations ---

    /// Allocate a slot in the arena, reusing a free slot if available.
    fn alloc_slot(&mut self, key: K, value: V) -> usize {
        if self.free_head != SENTINEL {
            let idx = self.free_head;
            self.free_head = self.arena[idx].next;
            self.arena[idx] = Node {
                key,
                value: Some(value),
                prev: SENTINEL,
                next: SENTINEL,
            };
            idx
        } else {
            let idx = self.arena.len();
            self.arena.push(Node {
                key,
                value: Some(value),
                prev: SENTINEL,
                next: SENTINEL,
            });
            idx
        }
    }

    /// Remove node at `idx` from the doubly-linked list (does NOT free the slot).
    fn unlink(&mut self, idx: usize) {
        let prev = self.arena[idx].prev;
        let next = self.arena[idx].next;

        if prev != SENTINEL {
            self.arena[prev].next = next;
        } else {
            self.head = next;
        }

        if next != SENTINEL {
            self.arena[next].prev = prev;
        } else {
            self.tail = prev;
        }

        self.arena[idx].prev = SENTINEL;
        self.arena[idx].next = SENTINEL;
    }

    /// Push node at `idx` to the head of the list (most-recently used).
    fn push_head(&mut self, idx: usize) {
        self.arena[idx].prev = SENTINEL;
        self.arena[idx].next = self.head;

        if self.head != SENTINEL {
            self.arena[self.head].prev = idx;
        }
        self.head = idx;

        if self.tail == SENTINEL {
            self.tail = idx;
        }
    }

    /// Move an existing node to the head (most-recently used).
    fn move_to_head(&mut self, idx: usize) {
        if self.head == idx {
            return;
        }
        self.unlink(idx);
        self.push_head(idx);
    }

    /// Evict the tail (least-recently used) entry.
    fn evict_tail(&mut self) -> Option<(K, V)> {
        if self.tail == SENTINEL {
            return None;
        }
        let idx = self.tail;
        let key = self.arena[idx].key.clone();
        let value = self.arena[idx].value.take();

        self.unlink(idx);
        self.map.remove(&key);

        // Add to free list
        self.arena[idx].next = self.free_head;
        self.free_head = idx;

        self.stats.evictions += 1;

        value.map(|v| (key, v))
    }
}

/// Iterator from most-recently used to least-recently used.
pub struct MruIter<'a, K, V> {
    arena: &'a [Node<K, V>],
    current: usize,
    remaining: usize,
}

impl<'a, K, V> Iterator for MruIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current == SENTINEL || self.remaining == 0 {
            return None;
        }
        let node = &self.arena[self.current];
        self.current = node.next;
        self.remaining -= 1;
        node.value.as_ref().map(|v| (&node.key, v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

/// Iterator from least-recently used to most-recently used.
pub struct LruIter<'a, K, V> {
    arena: &'a [Node<K, V>],
    current: usize,
    remaining: usize,
}

impl<'a, K, V> Iterator for LruIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current == SENTINEL || self.remaining == 0 {
            return None;
        }
        let node = &self.arena[self.current];
        self.current = node.prev;
        self.remaining -= 1;
        node.value.as_ref().map(|v| (&node.key, v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_put_and_get() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        assert_eq!(cache.get(&1), Some(&"one"));
        assert_eq!(cache.get(&2), Some(&"two"));
        assert_eq!(cache.get(&3), Some(&"three"));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn miss_returns_none() {
        let mut cache: LruCache<i32, &str> = LruCache::new(2);
        cache.put(1, "one");
        assert_eq!(cache.get(&99), None);
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn evicts_lru_on_capacity() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        cache.put(2, "two");
        let evicted = cache.put(3, "three");

        assert_eq!(evicted, Some((1, "one")));
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), Some(&"two"));
        assert_eq!(cache.get(&3), Some(&"three"));
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn get_promotes_to_mru() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        cache.put(2, "two");

        // Access key=1, making it MRU (key=2 becomes LRU)
        cache.get(&1);

        let evicted = cache.put(3, "three");
        assert_eq!(evicted, Some((2, "two")));
        assert_eq!(cache.get(&1), Some(&"one"));
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), Some(&"three"));
    }

    #[test]
    fn update_existing_key() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        let evicted = cache.put(1, "ONE");

        assert!(evicted.is_none());
        assert_eq!(cache.get(&1), Some(&"ONE"));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.stats().updates, 1);
    }

    #[test]
    fn peek_does_not_promote() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        cache.put(2, "two");

        assert_eq!(cache.peek(&1), Some(&"one"));

        // key=1 is still LRU since peek didn't promote
        let evicted = cache.put(3, "three");
        assert_eq!(evicted, Some((1, "one")));
        assert_eq!(cache.get(&2), Some(&"two"));
    }

    #[test]
    fn contains_key_check() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        assert!(cache.contains_key(&1));
        assert!(!cache.contains_key(&2));
    }

    #[test]
    fn remove_entry() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        let removed = cache.remove(&2);
        assert_eq!(removed, Some("two"));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some(&"one"));
        assert_eq!(cache.get(&3), Some(&"three"));
    }

    #[test]
    fn remove_head() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three"); // 3 is MRU (head)

        cache.remove(&3);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.peek_mru(), Some((&2, &"two")));
    }

    #[test]
    fn remove_tail() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one"); // 1 is LRU (tail)
        cache.put(2, "two");
        cache.put(3, "three");

        cache.remove(&1);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.peek_lru(), Some((&2, &"two")));
    }

    #[test]
    fn peek_lru_and_mru() {
        let mut cache = LruCache::new(3);
        assert_eq!(cache.peek_lru(), None);
        assert_eq!(cache.peek_mru(), None);

        cache.put(1, "one");
        assert_eq!(cache.peek_lru(), Some((&1, &"one")));
        assert_eq!(cache.peek_mru(), Some((&1, &"one")));

        cache.put(2, "two");
        assert_eq!(cache.peek_lru(), Some((&1, &"one")));
        assert_eq!(cache.peek_mru(), Some((&2, &"two")));
    }

    #[test]
    fn pop_lru_entry() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        let popped = cache.pop_lru();
        assert_eq!(popped, Some((1, "one")));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.peek_lru(), Some((&2, &"two")));
    }

    #[test]
    fn pop_lru_empty() {
        let mut cache: LruCache<i32, &str> = LruCache::new(2);
        assert_eq!(cache.pop_lru(), None);
    }

    #[test]
    fn clear_cache() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get(&1), None);
    }

    #[test]
    fn iter_mru_order() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        let entries: Vec<_> = cache.iter_mru().collect();
        assert_eq!(entries, vec![(&3, &"three"), (&2, &"two"), (&1, &"one")]);
    }

    #[test]
    fn iter_lru_order() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        let entries: Vec<_> = cache.iter_lru().collect();
        assert_eq!(entries, vec![(&1, &"one"), (&2, &"two"), (&3, &"three")]);
    }

    #[test]
    fn iter_after_promotion() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");
        cache.get(&1); // promote 1 to MRU

        let keys: Vec<_> = cache.iter_mru().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 3, 2]);
    }

    #[test]
    fn stats_tracking() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one"); // insertion
        cache.put(2, "two"); // insertion
        cache.get(&1); // hit
        cache.get(&99); // miss
        cache.put(1, "ONE"); // update
        cache.put(3, "three"); // insertion + eviction

        let stats = cache.stats();
        assert_eq!(stats.insertions, 3);
        assert_eq!(stats.updates, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn hit_rate_calculation() {
        let stats = CacheStats {
            hits: 7,
            misses: 3,
            ..Default::default()
        };
        assert!((stats.hit_rate() - 0.7).abs() < 1e-10);
        assert_eq!(stats.total_lookups(), 10);
    }

    #[test]
    fn hit_rate_zero_lookups() {
        let stats = CacheStats::default();
        assert_eq!(stats.hit_rate(), 0.0);
    }

    #[test]
    fn reset_stats() {
        let mut cache = LruCache::new(2);
        cache.put(1, "one");
        cache.get(&1);
        cache.reset_stats();

        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().insertions, 0);
    }

    #[test]
    fn single_capacity() {
        let mut cache = LruCache::new(1);
        cache.put(1, "one");
        let evicted = cache.put(2, "two");

        assert_eq!(evicted, Some((1, "one")));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.get(&2), Some(&"two"));
    }

    #[test]
    fn slot_reuse_after_remove() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        cache.remove(&2);
        cache.put(4, "four"); // should reuse slot from key=2
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get(&4), Some(&"four"));
        assert_eq!(cache.get(&2), None);
    }

    #[test]
    fn get_mut_modifies_value() {
        let mut cache = LruCache::new(2);
        cache.put(1, vec![1, 2, 3]);

        if let Some(v) = cache.get_mut(&1) {
            v.push(4);
        }

        assert_eq!(cache.get(&1), Some(&vec![1, 2, 3, 4]));
    }

    #[test]
    fn remove_nonexistent_key() {
        let mut cache: LruCache<i32, &str> = LruCache::new(2);
        cache.put(1, "one");
        assert!(cache.remove(&99).is_none());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn resize_smaller_evicts() {
        let mut cache = LruCache::new(5);
        for i in 0..5 {
            cache.put(i, i * 10);
        }

        let evicted = cache.resize(3);
        assert_eq!(evicted.len(), 2);
        // LRU entries (0 and 1) should have been evicted
        assert_eq!(evicted[0], (0, 0));
        assert_eq!(evicted[1], (1, 10));
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.capacity(), 3);
    }

    #[test]
    fn resize_larger_no_eviction() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");

        let evicted = cache.resize(10);
        assert!(evicted.is_empty());
        assert_eq!(cache.capacity(), 10);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn retain_filter() {
        let mut cache = LruCache::new(5);
        for i in 0..5 {
            cache.put(i, i);
        }

        cache.retain(|_k, v| v % 2 == 0);
        assert_eq!(cache.len(), 3); // 0, 2, 4
        assert!(cache.contains_key(&0));
        assert!(!cache.contains_key(&1));
        assert!(cache.contains_key(&2));
        assert!(!cache.contains_key(&3));
        assert!(cache.contains_key(&4));
    }

    #[test]
    fn stress_sequential_access() {
        let mut cache = LruCache::new(100);
        for i in 0..1000 {
            cache.put(i, i * 2);
        }
        assert_eq!(cache.len(), 100);
        // Only last 100 entries should be present
        for i in 900..1000 {
            assert_eq!(cache.get(&i), Some(&(i * 2)));
        }
        for i in 0..900 {
            assert_eq!(cache.get(&i), None);
        }
    }

    #[test]
    fn stress_mixed_operations() {
        let mut cache = LruCache::new(50);
        for i in 0..500 {
            cache.put(i, format!("val-{}", i));
            if i % 3 == 0 {
                cache.get(&(i / 2));
            }
            if i % 7 == 0 && i > 0 {
                cache.remove(&(i - 1));
            }
        }
        assert!(cache.len() <= 50);
        let stats = cache.stats();
        assert!(stats.insertions > 0);
        assert!(stats.evictions > 0);
    }

    #[test]
    fn string_keys_and_values() {
        let mut cache = LruCache::new(2);
        cache.put("hello".to_string(), "world".to_string());
        cache.put("foo".to_string(), "bar".to_string());

        assert_eq!(cache.get(&"hello".to_string()), Some(&"world".to_string()));
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn zero_capacity_panics() {
        let _cache: LruCache<i32, i32> = LruCache::new(0);
    }

    #[test]
    fn eviction_cycle_reuses_slots() {
        let mut cache = LruCache::new(2);
        for round in 0..10 {
            let base = round * 10;
            cache.put(base, base);
            cache.put(base + 1, base + 1);
            cache.put(base + 2, base + 2); // evicts base
        }
        assert_eq!(cache.len(), 2);
        // Arena should not grow unboundedly
        assert!(cache.arena.len() <= 12); // bounded by reuse
    }

    #[test]
    fn iterator_size_hint() {
        let mut cache = LruCache::new(5);
        for i in 0..3 {
            cache.put(i, i);
        }
        let iter = cache.iter_mru();
        assert_eq!(iter.size_hint(), (3, Some(3)));
    }

    #[test]
    fn debug_output() {
        let mut cache = LruCache::new(5);
        cache.put(1, "one");
        let debug = format!("{:?}", cache);
        assert!(debug.contains("LruCache"));
        assert!(debug.contains("capacity: 5"));
        assert!(debug.contains("len: 1"));
    }

    #[test]
    fn evicted_values_are_correct() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);

        let evicted = cache.put("d", 4);
        assert_eq!(evicted, Some(("a", 1)));

        let evicted = cache.put("e", 5);
        assert_eq!(evicted, Some(("b", 2)));
    }

    #[test]
    fn update_promotes_to_mru() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Update key=1 (currently LRU) — should promote to MRU
        cache.put(1, "ONE");

        let keys: Vec<_> = cache.iter_mru().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 3, 2]);
    }

    #[test]
    fn remove_only_element() {
        let mut cache = LruCache::new(3);
        cache.put(1, "one");
        let removed = cache.remove(&1);
        assert_eq!(removed, Some("one"));
        assert!(cache.is_empty());
        assert_eq!(cache.peek_lru(), None);
        assert_eq!(cache.peek_mru(), None);
    }

    #[test]
    fn reuse_after_eviction_then_remove() {
        let mut cache = LruCache::new(2);
        cache.put(1, 10);
        cache.put(2, 20);
        cache.put(3, 30); // evicts 1, now {2,3}
        cache.remove(&2); // now {3}

        // Two free slots, insert two more
        cache.put(4, 40); // now {3,4}
        cache.put(5, 50); // evicts LRU=3, now {4,5}
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&3), None); // evicted
        assert_eq!(cache.get(&4), Some(&40));
        assert_eq!(cache.get(&5), Some(&50));
    }

    #[test]
    fn reuse_after_eviction_and_remove_complex() {
        let mut cache = LruCache::new(2);
        cache.put(1, 10);
        cache.put(2, 20);
        cache.put(3, 30); // evicts 1, now {2,3}
        cache.remove(&2); // now {3}

        cache.put(4, 40); // now {3,4}
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&3), Some(&30));
        assert_eq!(cache.get(&4), Some(&40));

        cache.put(5, 50); // evicts LRU=3 (since get(&3) promoted it... actually 4 was last get)
        // After get(&3), get(&4): MRU order is [4,3]. So LRU=3? No:
        // get(&3) promotes 3, get(&4) promotes 4. MRU=[4,3]. put(5) evicts tail=3.
        assert_eq!(cache.get(&3), None);
        assert_eq!(cache.get(&4), Some(&40));
        assert_eq!(cache.get(&5), Some(&50));
    }
}
