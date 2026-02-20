//! Treap — randomized BST with heap-ordered priorities.
//!
//! A treap combines a binary search tree (ordered by key) with a max-heap
//! (ordered by random priority), yielding O(log n) expected time for all
//! operations without explicit rebalancing. The split/merge primitives
//! enable efficient range operations.
//!
//! # Design
//!
//! ```text
//!         (key=5, pri=90)
//!        /               \
//!  (key=2, pri=70)  (key=8, pri=80)
//!       \                /
//!  (key=3, pri=50)  (key=7, pri=60)
//! ```
//!
//! - BST property: left.key < node.key < right.key
//! - Heap property: node.priority >= children.priority
//! - Split(key): partition tree into (<key, >=key)
//! - Merge(left, right): combine two treaps (all keys in left < all in right)
//!
//! # Use Cases in FrankenTerm
//!
//! - **Order statistics**: Find the k-th most active pane.
//! - **Merge sorted streams**: Combine event streams from multiple panes.
//! - **Range operations**: Count/sum elements in a key range.
//! - **Dynamic ranking**: Maintain a ranking that changes with updates.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Deterministic PRNG ─────────────────────────────────────────────────

/// SplitMix64 for deterministic priority generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rng {
    state: u64,
}

impl Rng {
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
}

// ── Node ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TreapNode<K, V> {
    key: K,
    value: V,
    priority: u64,
    left: Option<usize>,
    right: Option<usize>,
    /// Subtree size (for order statistics).
    size: usize,
}

// ── Treap ──────────────────────────────────────────────────────────────

/// A treap (tree + heap) providing expected O(log n) operations.
///
/// Supports insert, remove, search, k-th element, rank, split, and merge.
/// Uses arena allocation with index-based pointers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Treap<K, V> {
    nodes: Vec<TreapNode<K, V>>,
    root: Option<usize>,
    rng: Rng,
}

impl<K: Ord + Clone, V> Default for Treap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone, V> Treap<K, V> {
    /// Create an empty treap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_seed(0x12345678)
    }

    /// Create an empty treap with a specific seed for reproducibility.
    #[must_use]
    pub fn with_seed(seed: u64) -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            rng: Rng::new(seed),
        }
    }

    /// Return the number of key-value pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.root.map_or(0, |r| self.nodes[r].size)
    }

    /// Check if the treap is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Insert a key-value pair. Returns the previous value if the key existed.
    pub fn insert(&mut self, key: K, value: V) -> Option<V>
    where
        V: Clone,
    {
        let priority = self.rng.next();
        let new_idx = self.nodes.len();
        self.nodes.push(TreapNode {
            key: key.clone(),
            value,
            priority,
            left: None,
            right: None,
            size: 1,
        });

        let (left, existing, right) = self.split_by_key(self.root, &key);
        let old_value = existing.map(|idx| self.nodes[idx].value.clone());

        // Merge left + new_node + right
        let merged = self.merge(left, Some(new_idx));
        self.root = self.merge(merged, right);
        old_value
    }

    /// Look up a value by key.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&V> {
        self.find_node(self.root, key)
            .map(|idx| &self.nodes[idx].value)
    }

    /// Check if a key exists.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.find_node(self.root, key).is_some()
    }

    /// Remove a key-value pair. Returns the value if the key existed.
    pub fn remove(&mut self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let (left, existing, right) = self.split_by_key(self.root, key);
        let old_value = existing.map(|idx| self.nodes[idx].value.clone());
        self.root = self.merge(left, right);
        old_value
    }

    /// Get the k-th smallest element (0-based).
    ///
    /// Returns `None` if `k >= len()`.
    #[must_use]
    pub fn kth(&self, k: usize) -> Option<(&K, &V)> {
        self.kth_node(self.root, k)
    }

    /// Return the rank (0-based position) of a key in sorted order.
    ///
    /// Returns the number of keys strictly less than the given key.
    #[must_use]
    pub fn rank(&self, key: &K) -> usize {
        self.rank_of(self.root, key)
    }

    /// Get the minimum key-value pair.
    #[must_use]
    pub fn min(&self) -> Option<(&K, &V)> {
        self.kth(0)
    }

    /// Get the maximum key-value pair.
    #[must_use]
    pub fn max(&self) -> Option<(&K, &V)> {
        let len = self.len();
        if len == 0 { None } else { self.kth(len - 1) }
    }

    /// Collect all key-value pairs in sorted order.
    #[must_use]
    pub fn to_sorted_vec(&self) -> Vec<(&K, &V)> {
        let mut result = Vec::with_capacity(self.len());
        self.inorder(self.root, &mut result);
        result
    }

    /// Iterate over all keys in sorted order.
    #[must_use]
    pub fn keys(&self) -> Vec<&K> {
        self.to_sorted_vec().into_iter().map(|(k, _)| k).collect()
    }

    // ── Internal: Find ─────────────────────────────────────────────

    fn find_node(&self, node: Option<usize>, key: &K) -> Option<usize> {
        let idx = node?;
        match key.cmp(&self.nodes[idx].key) {
            std::cmp::Ordering::Equal => Some(idx),
            std::cmp::Ordering::Less => self.find_node(self.nodes[idx].left, key),
            std::cmp::Ordering::Greater => self.find_node(self.nodes[idx].right, key),
        }
    }

    // ── Internal: Split ────────────────────────────────────────────

    /// Split by key into (< key, == key, > key).
    fn split_by_key(
        &mut self,
        node: Option<usize>,
        key: &K,
    ) -> (Option<usize>, Option<usize>, Option<usize>) {
        let Some(idx) = node else {
            return (None, None, None);
        };

        match key.cmp(&self.nodes[idx].key) {
            std::cmp::Ordering::Equal => {
                let left = self.nodes[idx].left;
                let right = self.nodes[idx].right;
                self.nodes[idx].left = None;
                self.nodes[idx].right = None;
                self.update_size(idx);
                (left, Some(idx), right)
            }
            std::cmp::Ordering::Less => {
                let left = self.nodes[idx].left;
                let (ll, eq, lr) = self.split_by_key(left, key);
                self.nodes[idx].left = lr;
                self.update_size(idx);
                (ll, eq, Some(idx))
            }
            std::cmp::Ordering::Greater => {
                let right = self.nodes[idx].right;
                let (rl, eq, rr) = self.split_by_key(right, key);
                self.nodes[idx].right = rl;
                self.update_size(idx);
                (Some(idx), eq, rr)
            }
        }
    }

    // ── Internal: Merge ────────────────────────────────────────────

    /// Merge two treaps where all keys in `left` < all keys in `right`.
    fn merge(&mut self, left: Option<usize>, right: Option<usize>) -> Option<usize> {
        match (left, right) {
            (None, right) => right,
            (left, None) => left,
            (Some(l), Some(r)) => {
                if self.nodes[l].priority >= self.nodes[r].priority {
                    let l_right = self.nodes[l].right;
                    self.nodes[l].right = self.merge(l_right, Some(r));
                    self.update_size(l);
                    Some(l)
                } else {
                    let r_left = self.nodes[r].left;
                    self.nodes[r].left = self.merge(Some(l), r_left);
                    self.update_size(r);
                    Some(r)
                }
            }
        }
    }

    // ── Internal: Size updates ─────────────────────────────────────

    fn node_size(&self, node: Option<usize>) -> usize {
        node.map_or(0, |idx| self.nodes[idx].size)
    }

    fn update_size(&mut self, idx: usize) {
        let left_size = self.node_size(self.nodes[idx].left);
        let right_size = self.node_size(self.nodes[idx].right);
        self.nodes[idx].size = 1 + left_size + right_size;
    }

    // ── Internal: Order statistics ─────────────────────────────────

    fn kth_node(&self, node: Option<usize>, k: usize) -> Option<(&K, &V)> {
        let idx = node?;
        let left_size = self.node_size(self.nodes[idx].left);

        match k.cmp(&left_size) {
            std::cmp::Ordering::Less => self.kth_node(self.nodes[idx].left, k),
            std::cmp::Ordering::Equal => Some((&self.nodes[idx].key, &self.nodes[idx].value)),
            std::cmp::Ordering::Greater => self.kth_node(self.nodes[idx].right, k - left_size - 1),
        }
    }

    fn rank_of(&self, node: Option<usize>, key: &K) -> usize {
        let Some(idx) = node else { return 0 };

        match key.cmp(&self.nodes[idx].key) {
            std::cmp::Ordering::Less => self.rank_of(self.nodes[idx].left, key),
            std::cmp::Ordering::Equal => self.node_size(self.nodes[idx].left),
            std::cmp::Ordering::Greater => {
                1 + self.node_size(self.nodes[idx].left) + self.rank_of(self.nodes[idx].right, key)
            }
        }
    }

    // ── Internal: Traversal ────────────────────────────────────────

    fn inorder<'a>(&'a self, node: Option<usize>, result: &mut Vec<(&'a K, &'a V)>) {
        let Some(idx) = node else { return };
        self.inorder(self.nodes[idx].left, result);
        result.push((&self.nodes[idx].key, &self.nodes[idx].value));
        self.inorder(self.nodes[idx].right, result);
    }
}

// ── Display ────────────────────────────────────────────────────────────

impl<K: Ord + Clone + fmt::Debug, V> fmt::Display for Treap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Treap({} elements)", self.len())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_treap() {
        let treap: Treap<i32, &str> = Treap::new();
        assert!(treap.is_empty());
        assert_eq!(treap.len(), 0);
        assert!(treap.get(&1).is_none());
        assert!(treap.min().is_none());
        assert!(treap.max().is_none());
    }

    #[test]
    fn single_insert() {
        let mut treap = Treap::new();
        assert!(treap.insert(5, "hello").is_none());
        assert_eq!(treap.len(), 1);
        assert_eq!(*treap.get(&5).unwrap(), "hello");
    }

    #[test]
    fn multiple_inserts() {
        let mut treap = Treap::new();
        treap.insert(3, "c");
        treap.insert(1, "a");
        treap.insert(5, "e");
        treap.insert(2, "b");
        treap.insert(4, "d");

        assert_eq!(treap.len(), 5);
        assert_eq!(*treap.get(&1).unwrap(), "a");
        assert_eq!(*treap.get(&5).unwrap(), "e");
    }

    #[test]
    fn overwrite() {
        let mut treap = Treap::new();
        treap.insert(1, "first".to_string());
        let old = treap.insert(1, "second".to_string());
        assert_eq!(old, Some("first".to_string()));
        assert_eq!(treap.len(), 1);
        assert_eq!(*treap.get(&1).unwrap(), "second");
    }

    #[test]
    fn remove() {
        let mut treap = Treap::new();
        treap.insert(1, "a".to_string());
        treap.insert(2, "b".to_string());
        treap.insert(3, "c".to_string());

        assert_eq!(treap.remove(&2), Some("b".to_string()));
        assert_eq!(treap.len(), 2);
        assert!(treap.get(&2).is_none());
        assert!(treap.get(&1).is_some());
        assert!(treap.get(&3).is_some());
    }

    #[test]
    fn remove_nonexistent() {
        let mut treap: Treap<i32, String> = Treap::new();
        treap.insert(1, "a".to_string());
        assert!(treap.remove(&99).is_none());
        assert_eq!(treap.len(), 1);
    }

    #[test]
    fn kth_element() {
        let mut treap = Treap::new();
        for i in [5, 3, 8, 1, 4, 7, 9] {
            treap.insert(i, i * 10);
        }

        // Sorted: 1, 3, 4, 5, 7, 8, 9
        assert_eq!(treap.kth(0), Some((&1, &10)));
        assert_eq!(treap.kth(3), Some((&5, &50)));
        assert_eq!(treap.kth(6), Some((&9, &90)));
        assert!(treap.kth(7).is_none());
    }

    #[test]
    fn rank() {
        let mut treap = Treap::new();
        for i in [10, 20, 30, 40, 50] {
            treap.insert(i, ());
        }

        assert_eq!(treap.rank(&10), 0);
        assert_eq!(treap.rank(&30), 2);
        assert_eq!(treap.rank(&50), 4);
        assert_eq!(treap.rank(&25), 2); // Between 20 and 30
    }

    #[test]
    fn min_max() {
        let mut treap = Treap::new();
        treap.insert(5, "five");
        treap.insert(1, "one");
        treap.insert(9, "nine");

        assert_eq!(treap.min(), Some((&1, &"one")));
        assert_eq!(treap.max(), Some((&9, &"nine")));
    }

    #[test]
    fn sorted_order() {
        let mut treap = Treap::new();
        for i in [5, 2, 8, 1, 3, 7, 9] {
            treap.insert(i, ());
        }

        let keys: Vec<&i32> = treap.keys();
        let expected: Vec<i32> = vec![1, 2, 3, 5, 7, 8, 9];
        let keys_owned: Vec<i32> = keys.iter().map(|&&k| k).collect();
        assert_eq!(keys_owned, expected);
    }

    #[test]
    fn contains_key() {
        let mut treap = Treap::new();
        treap.insert(1, ());
        treap.insert(3, ());

        assert!(treap.contains_key(&1));
        assert!(!treap.contains_key(&2));
        assert!(treap.contains_key(&3));
    }

    #[test]
    fn large_treap() {
        let mut treap = Treap::new();
        for i in 0..1000 {
            treap.insert(i, i * 2);
        }

        assert_eq!(treap.len(), 1000);
        assert_eq!(treap.min(), Some((&0, &0)));
        assert_eq!(treap.max(), Some((&999, &1998)));
        assert_eq!(treap.kth(500), Some((&500, &1000)));
    }

    #[test]
    fn serde_roundtrip() {
        let mut treap = Treap::new();
        treap.insert(1, "a".to_string());
        treap.insert(2, "b".to_string());
        treap.insert(3, "c".to_string());

        let json = serde_json::to_string(&treap).unwrap();
        let restored: Treap<i32, String> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 3);
        assert_eq!(*restored.get(&2).unwrap(), "b");
    }

    #[test]
    fn display_format() {
        let mut treap = Treap::new();
        treap.insert(1, ());
        treap.insert(2, ());
        let s = format!("{}", treap);
        assert!(s.contains("2 elements"));
    }

    #[test]
    fn default_is_empty() {
        let treap: Treap<i32, ()> = Treap::default();
        assert!(treap.is_empty());
    }

    #[test]
    fn string_keys() {
        let mut treap = Treap::new();
        treap.insert("banana".to_string(), 1);
        treap.insert("apple".to_string(), 2);
        treap.insert("cherry".to_string(), 3);

        assert_eq!(treap.min().map(|(k, _)| k.as_str()), Some("apple"));
        assert_eq!(treap.max().map(|(k, _)| k.as_str()), Some("cherry"));
    }

    // ── Expanded test coverage ──────────────────────────────────────

    #[test]
    fn remove_from_empty() {
        let mut treap: Treap<i32, String> = Treap::new();
        assert!(treap.remove(&1).is_none());
    }

    #[test]
    fn remove_sole_element() {
        let mut treap = Treap::new();
        treap.insert(42, "only".to_string());
        assert_eq!(treap.remove(&42), Some("only".to_string()));
        assert!(treap.is_empty());
        assert_eq!(treap.len(), 0);
    }

    #[test]
    fn remove_all_elements() {
        let mut treap = Treap::new();
        for i in 0..20 {
            treap.insert(i, i.to_string());
        }
        for i in 0..20 {
            assert!(treap.remove(&i).is_some());
        }
        assert!(treap.is_empty());
    }

    #[test]
    fn insert_ascending() {
        let mut treap = Treap::new();
        for i in 0..50 {
            treap.insert(i, i);
        }
        assert_eq!(treap.len(), 50);
        let keys: Vec<i32> = treap.keys().iter().map(|&&k| k).collect();
        assert_eq!(keys, (0..50).collect::<Vec<i32>>());
    }

    #[test]
    fn insert_descending() {
        let mut treap = Treap::new();
        for i in (0..50).rev() {
            treap.insert(i, i);
        }
        assert_eq!(treap.len(), 50);
        let keys: Vec<i32> = treap.keys().iter().map(|&&k| k).collect();
        assert_eq!(keys, (0..50).collect::<Vec<i32>>());
    }

    #[test]
    fn kth_out_of_bounds() {
        let mut treap = Treap::new();
        treap.insert(1, "a");
        treap.insert(2, "b");
        assert!(treap.kth(2).is_none());
        assert!(treap.kth(100).is_none());
    }

    #[test]
    fn rank_on_empty() {
        let treap: Treap<i32, ()> = Treap::new();
        assert_eq!(treap.rank(&42), 0);
    }

    #[test]
    fn rank_missing_keys() {
        let mut treap = Treap::new();
        treap.insert(10, ());
        treap.insert(20, ());
        treap.insert(30, ());

        assert_eq!(treap.rank(&5), 0);
        assert_eq!(treap.rank(&15), 1);
        assert_eq!(treap.rank(&25), 2);
        assert_eq!(treap.rank(&35), 3);
    }

    #[test]
    fn kth_rank_inverse() {
        let mut treap = Treap::new();
        for i in [10, 20, 30, 40, 50] {
            treap.insert(i, ());
        }
        for &k in &[10, 20, 30, 40, 50] {
            let r = treap.rank(&k);
            let (key, ()) = treap.kth(r).unwrap();
            assert_eq!(*key, k);
        }
    }

    #[test]
    fn min_max_single() {
        let mut treap = Treap::new();
        treap.insert(42, "only");
        assert_eq!(treap.min(), Some((&42, &"only")));
        assert_eq!(treap.max(), Some((&42, &"only")));
    }

    #[test]
    fn min_max_empty() {
        let treap: Treap<i32, ()> = Treap::new();
        assert!(treap.min().is_none());
        assert!(treap.max().is_none());
    }

    #[test]
    fn to_sorted_vec_empty() {
        let treap: Treap<i32, ()> = Treap::new();
        assert!(treap.to_sorted_vec().is_empty());
    }

    #[test]
    fn keys_empty() {
        let treap: Treap<i32, ()> = Treap::new();
        assert!(treap.keys().is_empty());
    }

    #[test]
    fn clone_independence() {
        let mut treap = Treap::new();
        treap.insert(1, "a".to_string());
        treap.insert(2, "b".to_string());

        let mut cloned = treap.clone();
        cloned.insert(3, "c".to_string());
        cloned.remove(&1);

        assert_eq!(treap.len(), 2);
        assert_eq!(cloned.len(), 2);
        assert!(treap.contains_key(&1));
        assert!(!cloned.contains_key(&1));
    }

    #[test]
    fn interleaved_insert_remove() {
        let mut treap = Treap::new();
        treap.insert(1, 10);
        treap.insert(2, 20);
        treap.remove(&1);
        treap.insert(3, 30);
        treap.insert(4, 40);
        treap.remove(&3);

        assert_eq!(treap.len(), 2);
        assert!(!treap.contains_key(&1));
        assert!(treap.contains_key(&2));
        assert!(!treap.contains_key(&3));
        assert!(treap.contains_key(&4));
    }

    #[test]
    fn with_seed_reproducible() {
        let mut t1 = Treap::with_seed(42);
        let mut t2 = Treap::with_seed(42);
        for i in 0..10 {
            t1.insert(i, i);
            t2.insert(i, i);
        }
        // Same seed → same tree structure
        assert_eq!(t1.len(), t2.len());
        let keys1: Vec<&i32> = t1.keys();
        let keys2: Vec<&i32> = t2.keys();
        assert_eq!(keys1, keys2);
    }

    #[test]
    fn serde_roundtrip_preserves_queries() {
        let mut treap = Treap::new();
        for i in 0..15 {
            treap.insert(i, i * 100);
        }

        let json = serde_json::to_string(&treap).unwrap();
        let restored: Treap<i32, i32> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), treap.len());
        for i in 0..15 {
            assert_eq!(restored.get(&i), treap.get(&i));
        }
        assert_eq!(restored.min(), treap.min());
        assert_eq!(restored.max(), treap.max());
    }

    #[test]
    fn display_empty() {
        let treap: Treap<i32, ()> = Treap::new();
        assert_eq!(format!("{}", treap), "Treap(0 elements)");
    }

    #[test]
    fn size_consistency_after_operations() {
        let mut treap = Treap::new();
        for i in 0..30 {
            treap.insert(i, i);
            assert_eq!(treap.len(), (i + 1) as usize);
        }
        for i in 0..15 {
            treap.remove(&i);
            assert_eq!(treap.len(), (29 - i) as usize);
        }
    }

    #[test]
    fn overwrite_preserves_size() {
        let mut treap = Treap::new();
        treap.insert(1, "first".to_string());
        treap.insert(2, "second".to_string());
        treap.insert(3, "third".to_string());

        treap.insert(2, "updated".to_string());
        assert_eq!(treap.len(), 3);
        assert_eq!(*treap.get(&2).unwrap(), "updated");
    }

    #[test]
    fn contains_key_empty_is_false() {
        let treap: Treap<i32, ()> = Treap::new();
        assert!(!treap.contains_key(&0));
        assert!(!treap.contains_key(&99));
    }

    #[test]
    fn get_nonexistent_in_nonempty_treap() {
        let mut treap = Treap::new();
        for i in [10, 20, 30, 40] {
            treap.insert(i, i);
        }
        assert!(treap.get(&15).is_none());
        assert!(treap.get(&50).is_none());
    }

    #[test]
    fn remove_root_with_two_children() {
        let mut treap = Treap::new();
        treap.insert(2, "root".to_string());
        treap.insert(1, "left".to_string());
        treap.insert(3, "right".to_string());

        assert_eq!(treap.remove(&2), Some("root".to_string()));
        assert_eq!(treap.len(), 2);
        assert_eq!(*treap.get(&1).unwrap(), "left");
        assert_eq!(*treap.get(&3).unwrap(), "right");
        assert!(treap.get(&2).is_none());
    }

    #[test]
    fn kth_after_multiple_removals() {
        let mut treap = Treap::new();
        for i in 1..=10 {
            treap.insert(i, i);
        }
        for i in [2, 4, 6, 8, 10] {
            treap.remove(&i);
        }

        // Remaining sorted keys: 1, 3, 5, 7, 9
        let remaining = [1, 3, 5, 7, 9];
        for (idx, expected_key) in remaining.iter().enumerate() {
            let (k, v) = treap.kth(idx).unwrap();
            assert_eq!(*k, *expected_key);
            assert_eq!(*v, *expected_key);
        }
        assert!(treap.kth(5).is_none());
    }

    #[test]
    fn rank_matches_index_from_sorted_keys() {
        let mut treap = Treap::new();
        for i in [12, 7, 19, 3, 15, 1, 9] {
            treap.insert(i, i * 10);
        }

        let keys: Vec<i32> = treap.keys().iter().map(|&&k| k).collect();
        for (idx, key) in keys.iter().enumerate() {
            assert_eq!(treap.rank(key), idx);
        }
    }

    #[test]
    fn keys_reflect_removals() {
        let mut treap = Treap::new();
        for i in 1..=6 {
            treap.insert(i, i);
        }
        treap.remove(&1);
        treap.remove(&4);
        treap.remove(&6);

        let keys: Vec<i32> = treap.keys().iter().map(|&&k| k).collect();
        assert_eq!(keys, vec![2, 3, 5]);
    }

    #[test]
    fn with_seed_different_seeds_same_key_order() {
        let mut t1 = Treap::with_seed(1);
        let mut t2 = Treap::with_seed(2);

        for i in [30, 10, 50, 20, 40] {
            t1.insert(i, i);
            t2.insert(i, i);
        }

        let keys1: Vec<i32> = t1.keys().iter().map(|&&k| k).collect();
        let keys2: Vec<i32> = t2.keys().iter().map(|&&k| k).collect();
        assert_eq!(keys1, vec![10, 20, 30, 40, 50]);
        assert_eq!(keys1, keys2);
    }

    #[test]
    fn serde_roundtrip_empty_treap() {
        let treap: Treap<i32, i32> = Treap::new();
        let json = serde_json::to_string(&treap).unwrap();
        let restored: Treap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert!(restored.is_empty());
        assert_eq!(restored.len(), 0);
        assert!(restored.min().is_none());
        assert!(restored.max().is_none());
    }

    #[test]
    fn to_sorted_vec_correctness() {
        let mut treap = Treap::new();
        for i in [5, 3, 8, 1, 4, 7, 9, 2, 6] {
            treap.insert(i, i * 10);
        }
        let sorted: Vec<(i32, i32)> = treap
            .to_sorted_vec()
            .iter()
            .map(|&(&k, &v)| (k, v))
            .collect();
        let expected: Vec<(i32, i32)> = (1..=9).map(|i| (i, i * 10)).collect();
        assert_eq!(sorted, expected);
    }
}
