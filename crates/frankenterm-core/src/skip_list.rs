//! Skip list — probabilistic ordered map with O(log n) operations.
//!
//! A skip list is a layered linked list that provides O(log n) search,
//! insertion, and deletion with simpler implementation than balanced trees.
//! Random level assignment gives probabilistic balance guarantees equivalent
//! to a balanced BST.
//!
//! # Design
//!
//! ```text
//! Level 3:  HEAD ──────────────────────────────────→ 50 ──→ NIL
//! Level 2:  HEAD ──────→ 20 ──────────────────────→ 50 ──→ NIL
//! Level 1:  HEAD ──→ 10 → 20 ──→ 30 ──────────────→ 50 ──→ NIL
//! Level 0:  HEAD → 5 → 10 → 20 → 25 → 30 → 40 → 50 → 60 → NIL
//! ```
//!
//! # Use Cases in FrankenTerm
//!
//! - **Time-indexed event lookup**: O(log n) search by timestamp.
//! - **Priority scheduling**: Ordered by priority, efficient insert/remove.
//! - **Range queries**: "All events between t1 and t2" in O(log n + k).
//! - **Concurrent-friendly**: Simpler to make lock-free than red-black trees.

use serde::{Deserialize, Serialize};

// ── Constants ───────────────────────────────────────────────────────

/// Maximum number of levels in the skip list.
const MAX_LEVEL: usize = 16;

/// Probability factor for level promotion (1/P chance of going up).
const P_DENOM: u64 = 4;

// ── Deterministic RNG (SplitMix64) ──────────────────────────────────

/// SplitMix64 PRNG for deterministic level generation.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn random_level(&mut self) -> usize {
        let mut level = 0;
        while level < MAX_LEVEL - 1 && (self.next() % P_DENOM) == 0 {
            level += 1;
        }
        level
    }
}

// ── Skip List Node ──────────────────────────────────────────────────

/// A node in the skip list.
#[derive(Debug, Clone)]
struct Node<K, V> {
    key: Option<K>,
    value: Option<V>,
    /// Forward pointers for each level. `None` = end of list at that level.
    forward: Vec<Option<usize>>,
}

impl<K, V> Node<K, V> {
    fn head() -> Self {
        Self {
            key: None,
            value: None,
            forward: vec![None; MAX_LEVEL],
        }
    }

    fn new(key: K, value: V, level: usize) -> Self {
        Self {
            key: Some(key),
            value: Some(value),
            forward: vec![None; level + 1],
        }
    }

    fn level(&self) -> usize {
        self.forward.len().saturating_sub(1)
    }
}

// ── SkipList ────────────────────────────────────────────────────────

/// An ordered map backed by a skip list.
///
/// Keys must be `Ord`. Values can be any type. The list uses a
/// deterministic PRNG (SplitMix64) so that behavior is reproducible
/// given the same seed.
#[derive(Debug, Clone)]
pub struct SkipList<K: Ord, V> {
    /// Node storage (arena-style). Index 0 is always the head sentinel.
    nodes: Vec<Node<K, V>>,
    /// Current maximum level in use.
    current_level: usize,
    /// Number of key-value pairs.
    len: usize,
    /// PRNG for level generation.
    rng: SplitMix64,
    /// Free list of deleted node indices for reuse.
    free: Vec<usize>,
}

impl<K: Ord, V> SkipList<K, V> {
    /// Create a new empty skip list with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            nodes: vec![Node::head()],
            current_level: 0,
            len: 0,
            rng: SplitMix64::new(seed),
            free: Vec::new(),
        }
    }

    /// Number of key-value pairs.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Current maximum level in use.
    pub fn current_level(&self) -> usize {
        self.current_level
    }

    /// Insert a key-value pair. Returns the previous value if the key existed.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let mut update = [0usize; MAX_LEVEL];
        let mut current = 0; // head

        // Find position at each level
        for level in (0..=self.current_level).rev() {
            loop {
                if let Some(next_idx) = self.nodes[current].forward[level] {
                    if let Some(ref next_key) = self.nodes[next_idx].key {
                        if *next_key < key {
                            current = next_idx;
                            continue;
                        }
                    }
                }
                break;
            }
            update[level] = current;
        }

        // Check if key already exists
        if let Some(next_idx) = self.nodes[current].forward[0] {
            if let Some(ref next_key) = self.nodes[next_idx].key {
                if *next_key == key {
                    // Update existing value
                    let old = self.nodes[next_idx].value.take();
                    self.nodes[next_idx].value = Some(value);
                    return old;
                }
            }
        }

        // Generate random level for new node
        let new_level = self.rng.random_level();

        // If new level exceeds current, update head pointers
        if new_level > self.current_level {
            #[allow(clippy::needless_range_loop)]
            for level in (self.current_level + 1)..=new_level {
                update[level] = 0; // head
            }
            self.current_level = new_level;
        }

        // Allocate node
        let new_node = Node::new(key, value, new_level);
        let new_idx = if let Some(idx) = self.free.pop() {
            self.nodes[idx] = new_node;
            idx
        } else {
            self.nodes.push(new_node);
            self.nodes.len() - 1
        };

        // Wire in forward pointers
        #[allow(clippy::needless_range_loop)]
        for level in 0..=new_level {
            self.nodes[new_idx].forward[level] = self.nodes[update[level]].forward[level];
            self.nodes[update[level]].forward[level] = Some(new_idx);
        }

        self.len += 1;
        None
    }

    /// Look up a value by key.
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut current = 0; // head

        for level in (0..=self.current_level).rev() {
            loop {
                if let Some(next_idx) = self.nodes[current].forward[level] {
                    if let Some(ref next_key) = self.nodes[next_idx].key {
                        match next_key.cmp(key) {
                            std::cmp::Ordering::Less => {
                                current = next_idx;
                                continue;
                            }
                            std::cmp::Ordering::Equal => {
                                return self.nodes[next_idx].value.as_ref();
                            }
                            std::cmp::Ordering::Greater => break,
                        }
                    }
                }
                break;
            }
        }

        None
    }

    /// Check if a key exists.
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Remove a key-value pair. Returns the value if found.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let mut update = [0usize; MAX_LEVEL];
        let mut current = 0;

        for level in (0..=self.current_level).rev() {
            loop {
                if let Some(next_idx) = self.nodes[current].forward[level] {
                    if let Some(ref next_key) = self.nodes[next_idx].key {
                        if *next_key < *key {
                            current = next_idx;
                            continue;
                        }
                    }
                }
                break;
            }
            update[level] = current;
        }

        // Check if the next node at level 0 has the target key
        if let Some(target_idx) = self.nodes[current].forward[0] {
            if let Some(ref target_key) = self.nodes[target_idx].key {
                if *target_key != *key {
                    return None;
                }
            } else {
                return None;
            }

            // Unwire from all levels
            let target_level = self.nodes[target_idx].level();
            #[allow(clippy::needless_range_loop)]
            for level in 0..=target_level {
                if self.nodes[update[level]].forward[level] == Some(target_idx) {
                    self.nodes[update[level]].forward[level] =
                        self.nodes[target_idx].forward[level];
                }
            }

            // Extract value
            let value = self.nodes[target_idx].value.take();
            self.nodes[target_idx].key = None;
            self.free.push(target_idx);

            // Adjust current_level if needed
            while self.current_level > 0 && self.nodes[0].forward[self.current_level].is_none() {
                self.current_level -= 1;
            }

            self.len -= 1;
            value
        } else {
            None
        }
    }

    /// Get the minimum key-value pair.
    pub fn min(&self) -> Option<(&K, &V)> {
        self.nodes[0].forward[0].and_then(|idx| {
            let node = &self.nodes[idx];
            match (node.key.as_ref(), node.value.as_ref()) {
                (Some(k), Some(v)) => Some((k, v)),
                _ => None,
            }
        })
    }

    /// Get the maximum key-value pair.
    pub fn max(&self) -> Option<(&K, &V)> {
        let mut current = 0;
        for level in (0..=self.current_level).rev() {
            while let Some(next_idx) = self.nodes[current].forward[level] {
                current = next_idx;
            }
        }
        if current == 0 {
            return None;
        }
        let node = &self.nodes[current];
        match (node.key.as_ref(), node.value.as_ref()) {
            (Some(k), Some(v)) => Some((k, v)),
            _ => None,
        }
    }

    /// Iterate over all key-value pairs in order.
    #[allow(clippy::iter_without_into_iter)]
    pub fn iter(&self) -> SkipListIter<'_, K, V> {
        SkipListIter {
            list: self,
            current: self.nodes[0].forward[0],
        }
    }

    /// Iterate over key-value pairs in the range [from, to].
    pub fn range(&self, from: &K, to: &K) -> Vec<(&K, &V)> {
        let mut result = Vec::new();
        let mut current = 0;

        // Find the starting position
        for level in (0..=self.current_level).rev() {
            loop {
                if let Some(next_idx) = self.nodes[current].forward[level] {
                    if let Some(ref next_key) = self.nodes[next_idx].key {
                        if *next_key < *from {
                            current = next_idx;
                            continue;
                        }
                    }
                }
                break;
            }
        }

        // Walk level 0 collecting entries in range
        let mut idx = self.nodes[current].forward[0];
        while let Some(i) = idx {
            if let (Some(k), Some(v)) = (&self.nodes[i].key, &self.nodes[i].value) {
                if *k > *to {
                    break;
                }
                if *k >= *from {
                    result.push((k, v));
                }
            }
            idx = self.nodes[i].forward[0];
        }

        result
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.nodes.push(Node::head());
        self.current_level = 0;
        self.len = 0;
        self.free.clear();
    }
}

/// Iterator over skip list key-value pairs in order.
pub struct SkipListIter<'a, K: Ord, V> {
    list: &'a SkipList<K, V>,
    current: Option<usize>,
}

impl<'a, K: Ord, V> Iterator for SkipListIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(idx) = self.current {
            self.current = self.list.nodes[idx].forward[0];
            if let (Some(k), Some(v)) = (&self.list.nodes[idx].key, &self.list.nodes[idx].value) {
                return Some((k, v));
            }
        }
        None
    }
}

// ── Statistics ──────────────────────────────────────────────────────

/// Statistics about the skip list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipListStats {
    /// Number of key-value pairs.
    pub len: usize,
    /// Current maximum level.
    pub current_level: usize,
    /// Total number of node slots (including free and head).
    pub total_nodes: usize,
    /// Number of free (reusable) slots.
    pub free_slots: usize,
}

impl<K: Ord, V> SkipList<K, V> {
    /// Get statistics.
    pub fn stats(&self) -> SkipListStats {
        SkipListStats {
            len: self.len,
            current_level: self.current_level,
            total_nodes: self.nodes.len(),
            free_slots: self.free.len(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::needless_range_loop, clippy::needless_collect)]
mod tests {
    use super::*;

    #[test]
    fn empty_list() {
        let list: SkipList<i32, String> = SkipList::new(42);
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert!(list.get(&1).is_none());
        assert!(list.min().is_none());
        assert!(list.max().is_none());
    }

    #[test]
    fn insert_and_get() {
        let mut list = SkipList::new(42);
        assert!(list.insert(5, "five").is_none());
        assert_eq!(list.get(&5), Some(&"five"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn insert_overwrites() {
        let mut list = SkipList::new(42);
        list.insert(1, "one");
        let old = list.insert(1, "ONE");
        assert_eq!(old, Some("one"));
        assert_eq!(list.get(&1), Some(&"ONE"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn insert_multiple_ordered() {
        let mut list = SkipList::new(42);
        for i in 0..20 {
            list.insert(i, i * 10);
        }
        assert_eq!(list.len(), 20);

        let items: Vec<_> = list.iter().map(|(k, v)| (*k, *v)).collect();
        for i in 0..20 {
            assert_eq!(items[i], (i, i * 10));
        }
    }

    #[test]
    fn insert_reverse_ordered() {
        let mut list = SkipList::new(42);
        for i in (0..20).rev() {
            list.insert(i, i * 10);
        }
        let items: Vec<_> = list.iter().map(|(k, _)| *k).collect();
        for i in 0..20 {
            assert_eq!(items[i], i);
        }
    }

    #[test]
    fn remove() {
        let mut list = SkipList::new(42);
        list.insert(1, "a");
        list.insert(2, "b");
        list.insert(3, "c");

        assert_eq!(list.remove(&2), Some("b"));
        assert_eq!(list.len(), 2);
        assert!(list.get(&2).is_none());
        assert_eq!(list.get(&1), Some(&"a"));
        assert_eq!(list.get(&3), Some(&"c"));
    }

    #[test]
    fn remove_nonexistent() {
        let mut list: SkipList<i32, i32> = SkipList::new(42);
        list.insert(1, 10);
        assert!(list.remove(&99).is_none());
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn min_max() {
        let mut list = SkipList::new(42);
        list.insert(30, "thirty");
        list.insert(10, "ten");
        list.insert(50, "fifty");

        assert_eq!(list.min(), Some((&10, &"ten")));
        assert_eq!(list.max(), Some((&50, &"fifty")));
    }

    #[test]
    fn range_query() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i * 10, i);
        }
        let range: Vec<_> = list.range(&20, &60);
        assert_eq!(range.len(), 5); // 20, 30, 40, 50, 60
        assert_eq!(*range[0].0, 20);
        assert_eq!(*range[4].0, 60);
    }

    #[test]
    fn clear() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        list.clear();
        assert!(list.is_empty());
        assert!(list.get(&5).is_none());
    }

    #[test]
    fn contains_key() {
        let mut list = SkipList::new(42);
        list.insert(1, "one");
        assert!(list.contains_key(&1));
        assert!(!list.contains_key(&2));
    }

    #[test]
    fn stats() {
        let mut list = SkipList::new(42);
        for i in 0..5 {
            list.insert(i, i);
        }
        let stats = list.stats();
        assert_eq!(stats.len, 5);
        assert!(stats.total_nodes >= 6); // 5 + head
    }

    #[test]
    fn stats_serde() {
        let stats = SkipListStats {
            len: 10,
            current_level: 3,
            total_nodes: 15,
            free_slots: 2,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: SkipListStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn remove_and_reinsert() {
        let mut list = SkipList::new(42);
        list.insert(1, 10);
        list.remove(&1);
        list.insert(1, 20);
        assert_eq!(list.get(&1), Some(&20));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn insert_delete_many() {
        let mut list = SkipList::new(42);
        for i in 0..100 {
            list.insert(i, i * 2);
        }
        for i in (0..100).step_by(2) {
            list.remove(&i);
        }
        assert_eq!(list.len(), 50);
        for i in (1..100).step_by(2) {
            assert!(list.contains_key(&i));
        }
    }

    #[test]
    fn deterministic_with_same_seed() {
        let mut list1 = SkipList::new(42);
        let mut list2 = SkipList::new(42);
        for i in 0..20 {
            list1.insert(i, i);
            list2.insert(i, i);
        }
        assert_eq!(list1.current_level(), list2.current_level());
        assert_eq!(list1.len(), list2.len());
    }

    // -- Batch: DarkBadger wa-1u90p.7.1 ----------------------------------------

    #[test]
    fn skip_list_debug_clone() {
        let mut list = SkipList::new(42);
        list.insert(1, "a");
        list.insert(2, "b");
        let cloned = list.clone();
        assert_eq!(cloned.len(), 2);
        assert_eq!(cloned.get(&1), Some(&"a"));
        let dbg = format!("{:?}", list);
        assert!(dbg.contains("SkipList"));
    }

    #[test]
    fn skip_list_stats_debug_clone_eq() {
        let stats = SkipListStats {
            len: 10,
            current_level: 3,
            total_nodes: 15,
            free_slots: 2,
        };
        let cloned = stats.clone();
        assert_eq!(stats, cloned);
        let dbg = format!("{:?}", stats);
        assert!(dbg.contains("SkipListStats"));
    }

    #[test]
    fn skip_list_stats_free_slots_after_remove() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        list.remove(&5);
        let stats = list.stats();
        assert_eq!(stats.len, 9);
        assert_eq!(stats.free_slots, 1);
    }

    #[test]
    fn skip_list_iter_empty() {
        let list: SkipList<i32, i32> = SkipList::new(42);
        let items: Vec<_> = list.iter().collect();
        assert!(items.is_empty());
    }

    #[test]
    fn skip_list_range_empty_result() {
        let mut list = SkipList::new(42);
        list.insert(10, "ten");
        list.insert(20, "twenty");
        let result = list.range(&100, &200);
        assert!(result.is_empty());
    }

    #[test]
    fn skip_list_range_single_element() {
        let mut list = SkipList::new(42);
        list.insert(10, "ten");
        list.insert(20, "twenty");
        list.insert(30, "thirty");
        let result = list.range(&20, &20);
        assert_eq!(result.len(), 1);
        assert_eq!(*result[0].0, 20);
    }

    #[test]
    fn skip_list_min_max_single() {
        let mut list = SkipList::new(42);
        list.insert(7, "seven");
        assert_eq!(list.min(), Some((&7, &"seven")));
        assert_eq!(list.max(), Some((&7, &"seven")));
    }

    #[test]
    fn skip_list_current_level_increases() {
        let mut list = SkipList::new(42);
        assert_eq!(list.current_level(), 0);
        // Inserting many items should increase the level
        for i in 0..100 {
            list.insert(i, i);
        }
        assert!(list.current_level() > 0);
    }

    #[test]
    fn skip_list_clear_resets_level() {
        let mut list = SkipList::new(42);
        for i in 0..50 {
            list.insert(i, i);
        }
        let level_before = list.current_level();
        assert!(level_before > 0);
        list.clear();
        assert_eq!(list.current_level(), 0);
    }

    #[test]
    fn skip_list_get_after_overwrite() {
        let mut list = SkipList::new(42);
        list.insert(1, 100);
        list.insert(1, 200);
        list.insert(1, 300);
        assert_eq!(list.get(&1), Some(&300));
        assert_eq!(list.len(), 1);
    }

    // -- Batch: DarkMill ft-283h4.53 ----------------------------------------

    #[test]
    fn remove_first_element() {
        let mut list = SkipList::new(42);
        list.insert(1, "a");
        list.insert(2, "b");
        list.insert(3, "c");
        assert_eq!(list.remove(&1), Some("a"));
        assert_eq!(list.min(), Some((&2, &"b")));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn remove_last_element() {
        let mut list = SkipList::new(42);
        list.insert(1, "a");
        list.insert(2, "b");
        list.insert(3, "c");
        assert_eq!(list.remove(&3), Some("c"));
        assert_eq!(list.max(), Some((&2, &"b")));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn remove_only_element() {
        let mut list = SkipList::new(42);
        list.insert(42, "only");
        assert_eq!(list.remove(&42), Some("only"));
        assert!(list.is_empty());
        assert!(list.min().is_none());
        assert!(list.max().is_none());
    }

    #[test]
    fn double_remove_same_key() {
        let mut list = SkipList::new(42);
        list.insert(5, "five");
        assert_eq!(list.remove(&5), Some("five"));
        assert!(list.remove(&5).is_none());
        assert!(list.is_empty());
    }

    #[test]
    fn remove_all_one_by_one() {
        let mut list = SkipList::new(42);
        let keys: Vec<i32> = (0..20).collect();
        for &k in &keys {
            list.insert(k, k * 10);
        }
        for &k in &keys {
            assert!(list.remove(&k).is_some());
        }
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert!(list.min().is_none());
    }

    #[test]
    fn remove_all_reverse_order() {
        let mut list = SkipList::new(42);
        for i in 0..30 {
            list.insert(i, i);
        }
        for i in (0..30).rev() {
            assert_eq!(list.remove(&i), Some(i));
        }
        assert!(list.is_empty());
    }

    #[test]
    fn negative_keys() {
        let mut list = SkipList::new(42);
        list.insert(-10, "neg10");
        list.insert(0, "zero");
        list.insert(10, "pos10");
        assert_eq!(list.min(), Some((&-10, &"neg10")));
        assert_eq!(list.max(), Some((&10, &"pos10")));
        assert_eq!(list.get(&0), Some(&"zero"));
    }

    #[test]
    fn string_keys() {
        let mut list = SkipList::new(42);
        list.insert("banana".to_string(), 2);
        list.insert("apple".to_string(), 1);
        list.insert("cherry".to_string(), 3);
        let items: Vec<_> = list.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(items, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn range_from_gt_to_is_empty() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        let result = list.range(&8, &3);
        assert!(result.is_empty());
    }

    #[test]
    fn range_covers_entire_list() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        let result = list.range(&0, &9);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn range_no_keys_in_interval() {
        let mut list = SkipList::new(42);
        list.insert(10, "a");
        list.insert(50, "b");
        let result = list.range(&20, &40);
        assert!(result.is_empty());
    }

    #[test]
    fn range_bounds_exceed_list() {
        let mut list = SkipList::new(42);
        list.insert(10, 1);
        list.insert(20, 2);
        list.insert(30, 3);
        let result = list.range(&0, &100);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn range_on_empty_list() {
        let list: SkipList<i32, i32> = SkipList::new(42);
        let result = list.range(&0, &100);
        assert!(result.is_empty());
    }

    #[test]
    fn iter_sorted_after_random_inserts() {
        let mut list = SkipList::new(99);
        let vals = [37, 12, 85, 3, 56, 91, 24, 68, 7, 43];
        for &v in &vals {
            list.insert(v, v);
        }
        let keys: Vec<i32> = list.iter().map(|(k, _)| *k).collect();
        let mut sorted = vals.to_vec();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn iter_count_matches_len() {
        let mut list = SkipList::new(42);
        for i in 0..50 {
            list.insert(i, i);
        }
        // Remove some
        for i in (0..50).step_by(3) {
            list.remove(&i);
        }
        assert_eq!(list.iter().count(), list.len());
    }

    #[test]
    fn min_max_after_removals() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        list.remove(&0); // remove min
        assert_eq!(list.min(), Some((&1, &1)));
        list.remove(&9); // remove max
        assert_eq!(list.max(), Some((&8, &8)));
    }

    #[test]
    fn min_max_updates_with_new_extremes() {
        let mut list = SkipList::new(42);
        list.insert(50, "fifty");
        assert_eq!(list.min(), Some((&50, &"fifty")));
        list.insert(10, "ten");
        assert_eq!(list.min(), Some((&10, &"ten")));
        list.insert(90, "ninety");
        assert_eq!(list.max(), Some((&90, &"ninety")));
    }

    #[test]
    fn stats_after_clear() {
        let mut list = SkipList::new(42);
        for i in 0..20 {
            list.insert(i, i);
        }
        list.clear();
        let stats = list.stats();
        assert_eq!(stats.len, 0);
        assert_eq!(stats.current_level, 0);
        assert_eq!(stats.free_slots, 0);
    }

    #[test]
    fn stats_free_slots_accumulate() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        list.remove(&3);
        list.remove(&7);
        list.remove(&1);
        let stats = list.stats();
        assert_eq!(stats.free_slots, 3);
        assert_eq!(stats.len, 7);
    }

    #[test]
    fn free_slot_reuse() {
        let mut list = SkipList::new(42);
        for i in 0..10 {
            list.insert(i, i);
        }
        let nodes_before = list.stats().total_nodes;
        // Remove 5 elements to create free slots
        for i in 0..5 {
            list.remove(&i);
        }
        assert_eq!(list.stats().free_slots, 5);
        // Reinsert — should reuse free slots, not grow arena
        for i in 0..5 {
            list.insert(i, i * 100);
        }
        assert_eq!(list.stats().free_slots, 0);
        assert_eq!(list.stats().total_nodes, nodes_before);
    }

    #[test]
    fn different_seeds_different_levels() {
        // Same data, different seeds should have same content but may differ in structure
        let mut list1 = SkipList::new(1);
        let mut list2 = SkipList::new(999_999);
        for i in 0..100 {
            list1.insert(i, i);
            list2.insert(i, i);
        }
        assert_eq!(list1.len(), list2.len());
        // Content should be identical
        let items1: Vec<_> = list1.iter().map(|(k, v)| (*k, *v)).collect();
        let items2: Vec<_> = list2.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(items1, items2);
    }

    #[test]
    fn large_scale_insert_verify() {
        let mut list = SkipList::new(42);
        for i in 0..1000 {
            list.insert(i, i * 3);
        }
        assert_eq!(list.len(), 1000);
        for i in 0..1000 {
            assert_eq!(list.get(&i), Some(&(i * 3)), "missing key {}", i);
        }
        assert_eq!(list.min(), Some((&0, &0)));
        assert_eq!(list.max(), Some((&999, &2997)));
    }

    #[test]
    fn large_scale_insert_remove_all() {
        let mut list = SkipList::new(42);
        for i in 0..500 {
            list.insert(i, i);
        }
        for i in 0..500 {
            assert!(list.remove(&i).is_some());
        }
        assert!(list.is_empty());
        assert!(list.min().is_none());
        assert!(list.max().is_none());
    }

    #[test]
    fn interleaved_insert_remove() {
        let mut list = SkipList::new(42);
        // Insert 0..10, remove evens, insert 10..20, remove odds from first batch
        for i in 0..10 {
            list.insert(i, i);
        }
        for i in (0..10).step_by(2) {
            list.remove(&i);
        }
        for i in 10..20 {
            list.insert(i, i);
        }
        for i in (1..10).step_by(2) {
            list.remove(&i);
        }
        // Only 10..20 should remain
        assert_eq!(list.len(), 10);
        for i in 10..20 {
            assert!(list.contains_key(&i));
        }
        for i in 0..10 {
            assert!(!list.contains_key(&i));
        }
    }

    #[test]
    fn contains_key_after_removal() {
        let mut list = SkipList::new(42);
        list.insert(1, "one");
        list.insert(2, "two");
        assert!(list.contains_key(&1));
        list.remove(&1);
        assert!(!list.contains_key(&1));
        assert!(list.contains_key(&2));
    }

    #[test]
    fn get_returns_none_for_removed() {
        let mut list = SkipList::new(42);
        list.insert(10, 100);
        list.insert(20, 200);
        list.remove(&10);
        assert!(list.get(&10).is_none());
        assert_eq!(list.get(&20), Some(&200));
    }

    #[test]
    fn overwrite_returns_previous_value() {
        let mut list = SkipList::new(42);
        assert!(list.insert(1, "first").is_none());
        assert_eq!(list.insert(1, "second"), Some("first"));
        assert_eq!(list.insert(1, "third"), Some("second"));
    }

    #[test]
    fn insert_remove_reinsert_cycles() {
        let mut list = SkipList::new(42);
        for cycle in 0..5 {
            for i in 0..20 {
                list.insert(i, cycle * 100 + i);
            }
            for i in 0..20 {
                list.remove(&i);
            }
            assert!(list.is_empty());
        }
        // Final insert to verify structure still works
        list.insert(42, 999);
        assert_eq!(list.get(&42), Some(&999));
    }

    #[test]
    fn range_boundary_precision() {
        let mut list = SkipList::new(42);
        for i in [10, 20, 30, 40, 50] {
            list.insert(i, i);
        }
        // Exact boundaries
        assert_eq!(list.range(&20, &40).len(), 3); // 20, 30, 40
        // Just inside
        assert_eq!(list.range(&21, &39).len(), 1); // 30 only
        // Just outside
        assert_eq!(list.range(&11, &19).len(), 0); // nothing between 10 and 20
    }

    #[test]
    fn stats_total_nodes_includes_head() {
        let list: SkipList<i32, i32> = SkipList::new(42);
        let stats = list.stats();
        assert_eq!(stats.total_nodes, 1); // just head
        assert_eq!(stats.len, 0);
    }

    #[test]
    fn clone_is_independent() {
        let mut list = SkipList::new(42);
        list.insert(1, "a");
        list.insert(2, "b");
        let mut cloned = list.clone();
        cloned.insert(3, "c");
        cloned.remove(&1);
        // Original unchanged
        assert_eq!(list.len(), 2);
        assert!(list.contains_key(&1));
        assert!(!list.contains_key(&3));
        // Clone has changes
        assert_eq!(cloned.len(), 2);
        assert!(!cloned.contains_key(&1));
        assert!(cloned.contains_key(&3));
    }

    #[test]
    fn remove_from_empty() {
        let mut list: SkipList<i32, i32> = SkipList::new(42);
        assert!(list.remove(&1).is_none());
        assert!(list.is_empty());
    }

    #[test]
    fn current_level_decreases_after_removals() {
        let mut list = SkipList::new(42);
        for i in 0..200 {
            list.insert(i, i);
        }
        let level_high = list.current_level();
        assert!(level_high > 0);
        // Remove all — level should go back to 0
        for i in 0..200 {
            list.remove(&i);
        }
        assert_eq!(list.current_level(), 0);
    }
}
