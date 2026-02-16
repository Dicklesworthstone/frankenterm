//! Self-adjusting binary search tree (splay tree).
//!
//! A splay tree moves accessed nodes to the root via zig/zig-zig/zig-zag
//! rotations, providing amortized O(log n) operations with excellent
//! cache locality for skewed access patterns.
//!
//! # Properties
//!
//! - **Amortized O(log n)**: insert, remove, get, kth, rank
//! - **Working set theorem**: Frequently-accessed items stay near root
//! - **Sequential access theorem**: In-order traversal is O(n)
//! - **Arena-allocated**: No pointer indirection, cache-friendly
//!
//! # Use in FrankenTerm
//!
//! Ideal for caching recently-accessed pane data, command history lookups,
//! and any access pattern where temporal locality matters.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Node ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Node<K, V> {
    key: K,
    value: V,
    left: Option<usize>,
    right: Option<usize>,
    size: usize,
}

// ── SplayTree ─────────────────────────────────────────────────────────

/// Self-adjusting binary search tree.
///
/// Accessed nodes are splayed (rotated) to the root, making recently
/// and frequently accessed items fast to find.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SplayTree<K, V> {
    nodes: Vec<Node<K, V>>,
    root: Option<usize>,
    free: Vec<usize>,
}

impl<K: Ord + Clone, V: Clone> SplayTree<K, V> {
    /// Creates an empty splay tree.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            free: Vec::new(),
        }
    }

    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        self.root.map_or(0, |r| self.nodes[r].size)
    }

    /// Returns true if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    // ── Internal helpers ──────────────────────────────────────────

    fn alloc_node(&mut self, key: K, value: V) -> usize {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = Node {
                key,
                value,
                left: None,
                right: None,
                size: 1,
            };
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(Node {
                key,
                value,
                left: None,
                right: None,
                size: 1,
            });
            idx
        }
    }

    fn node_size(&self, idx: Option<usize>) -> usize {
        idx.map_or(0, |i| self.nodes[i].size)
    }

    fn update_size(&mut self, idx: usize) {
        let left_size = self.node_size(self.nodes[idx].left);
        let right_size = self.node_size(self.nodes[idx].right);
        self.nodes[idx].size = left_size + right_size + 1;
    }

    // ── Splay operation (top-down) ────────────────────────────────

    /// Top-down splay: brings node with given key (or nearest) to root.
    /// Returns the new root.
    fn splay(&mut self, root: usize, key: &K) -> usize {
        // Use top-down splay with virtual left/right trees
        // We'll implement iterative top-down splay
        let mut left_tree: Option<usize> = None;
        let mut right_tree: Option<usize> = None;
        let mut left_tail: Option<usize> = None;
        let mut right_tail: Option<usize> = None;
        let mut current = root;

        loop {
            match key.cmp(&self.nodes[current].key) {
                std::cmp::Ordering::Less => {
                    let left_child = match self.nodes[current].left {
                        Some(l) => l,
                        None => break,
                    };

                    // Zig-zig: if key < left_child.key, rotate right first
                    if *key < self.nodes[left_child].key {
                        // Rotate right
                        self.nodes[current].left = self.nodes[left_child].right;
                        self.nodes[left_child].right = Some(current);
                        self.update_size(current);
                        current = left_child;

                        match self.nodes[current].left {
                            Some(_) => {}
                            None => break,
                        }
                    }

                    // Link right: current goes to right tree
                    match right_tail {
                        Some(rt) => {
                            self.nodes[rt].left = Some(current);
                        }
                        None => {
                            right_tree = Some(current);
                        }
                    }
                    right_tail = Some(current);
                    current = self.nodes[current].left.unwrap();
                    // Detach
                    if let Some(rt) = right_tail {
                        self.nodes[rt].left = None;
                    }
                }
                std::cmp::Ordering::Greater => {
                    let right_child = match self.nodes[current].right {
                        Some(r) => r,
                        None => break,
                    };

                    // Zig-zig: if key > right_child.key, rotate left first
                    if *key > self.nodes[right_child].key {
                        // Rotate left
                        self.nodes[current].right = self.nodes[right_child].left;
                        self.nodes[right_child].left = Some(current);
                        self.update_size(current);
                        current = right_child;

                        match self.nodes[current].right {
                            Some(_) => {}
                            None => break,
                        }
                    }

                    // Link left: current goes to left tree
                    match left_tail {
                        Some(lt) => {
                            self.nodes[lt].right = Some(current);
                        }
                        None => {
                            left_tree = Some(current);
                        }
                    }
                    left_tail = Some(current);
                    current = self.nodes[current].right.unwrap();
                    // Detach
                    if let Some(lt) = left_tail {
                        self.nodes[lt].right = None;
                    }
                }
                std::cmp::Ordering::Equal => break,
            }
        }

        // Reassemble: left_tree's rightmost -> current.left
        //             right_tree's leftmost -> current.right
        // Then current.left = left_tree, current.right = right_tree

        match left_tail {
            Some(lt) => {
                self.nodes[lt].right = self.nodes[current].left;
            }
            None => {
                left_tree = self.nodes[current].left;
            }
        }

        match right_tail {
            Some(rt) => {
                self.nodes[rt].left = self.nodes[current].right;
            }
            None => {
                right_tree = self.nodes[current].right;
            }
        }

        self.nodes[current].left = left_tree;
        self.nodes[current].right = right_tree;

        // Update sizes bottom-up for affected nodes
        self.rebuild_sizes_for_splay(left_tree, left_tail);
        self.rebuild_sizes_for_splay(right_tree, right_tail);
        self.update_size(current);

        current
    }

    /// Rebuild sizes along the splay path.
    fn rebuild_sizes_for_splay(&mut self, tree_root: Option<usize>, tail: Option<usize>) {
        // We need to update sizes from tail up to tree_root
        // Since we don't have parent pointers, we'll do a recursive update
        if let Some(root) = tree_root {
            self.rebuild_size(root);
        }
        let _ = tail; // tail is within the tree_root subtree
    }

    fn rebuild_size(&mut self, idx: usize) -> usize {
        let left_size = match self.nodes[idx].left {
            Some(l) => self.rebuild_size(l),
            None => 0,
        };
        let right_size = match self.nodes[idx].right {
            Some(r) => self.rebuild_size(r),
            None => 0,
        };
        self.nodes[idx].size = left_size + right_size + 1;
        self.nodes[idx].size
    }

    // ── Public operations ─────────────────────────────────────────

    /// Inserts a key-value pair. Returns the previous value if the key existed.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        if self.root.is_none() {
            let idx = self.alloc_node(key, value);
            self.root = Some(idx);
            return None;
        }

        let root = self.root.unwrap();
        let root = self.splay(root, &key);
        self.root = Some(root);

        match key.cmp(&self.nodes[root].key) {
            std::cmp::Ordering::Equal => {
                let old = std::mem::replace(&mut self.nodes[root].value, value);
                Some(old)
            }
            std::cmp::Ordering::Less => {
                let new_node = self.alloc_node(key, value);
                self.nodes[new_node].left = self.nodes[root].left;
                self.nodes[new_node].right = Some(root);
                self.nodes[root].left = None;
                self.update_size(root);
                self.update_size(new_node);
                self.root = Some(new_node);
                None
            }
            std::cmp::Ordering::Greater => {
                let new_node = self.alloc_node(key, value);
                self.nodes[new_node].right = self.nodes[root].right;
                self.nodes[new_node].left = Some(root);
                self.nodes[root].right = None;
                self.update_size(root);
                self.update_size(new_node);
                self.root = Some(new_node);
                None
            }
        }
    }

    /// Looks up a key, splaying it to the root if found.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if self.root.is_none() {
            return None;
        }

        let root = self.root.unwrap();
        let root = self.splay(root, key);
        self.root = Some(root);

        if self.nodes[root].key == *key {
            Some(&self.nodes[root].value)
        } else {
            None
        }
    }

    /// Looks up a key without splaying (for read-only reference access).
    pub fn peek(&self, key: &K) -> Option<&V> {
        let mut current = self.root;
        while let Some(idx) = current {
            match key.cmp(&self.nodes[idx].key) {
                std::cmp::Ordering::Equal => return Some(&self.nodes[idx].value),
                std::cmp::Ordering::Less => current = self.nodes[idx].left,
                std::cmp::Ordering::Greater => current = self.nodes[idx].right,
            }
        }
        None
    }

    /// Returns true if the key exists (splays it to root).
    pub fn contains_key(&mut self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Removes a key, returning its value if found.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        if self.root.is_none() {
            return None;
        }

        let root = self.root.unwrap();
        let root = self.splay(root, key);
        self.root = Some(root);

        if self.nodes[root].key != *key {
            return None;
        }

        let left = self.nodes[root].left;
        let right = self.nodes[root].right;

        // Free the node
        let value = self.nodes[root].value.clone();
        self.free.push(root);

        match left {
            None => {
                self.root = right;
            }
            Some(l) => {
                // Splay the max of left subtree, then attach right
                let new_root = self.splay_max(l);
                self.nodes[new_root].right = right;
                self.update_size(new_root);
                self.root = Some(new_root);
            }
        }

        Some(value)
    }

    /// Splay the maximum element to the root of the subtree.
    fn splay_max(&mut self, root: usize) -> usize {
        // Find a key larger than anything in the tree to splay max to root
        // We go right until we can't, that's our max
        let mut rightmost = root;
        loop {
            match self.nodes[rightmost].right {
                Some(r) => rightmost = r,
                None => break,
            }
        }
        let max_key = self.nodes[rightmost].key.clone();
        self.splay(root, &max_key)
    }

    /// Returns the kth smallest element (0-indexed).
    pub fn kth(&self, k: usize) -> Option<(&K, &V)> {
        if k >= self.len() {
            return None;
        }
        let mut current = self.root;
        let mut remaining = k;
        while let Some(idx) = current {
            let left_size = self.node_size(self.nodes[idx].left);
            match remaining.cmp(&left_size) {
                std::cmp::Ordering::Less => {
                    current = self.nodes[idx].left;
                }
                std::cmp::Ordering::Equal => {
                    return Some((&self.nodes[idx].key, &self.nodes[idx].value));
                }
                std::cmp::Ordering::Greater => {
                    remaining -= left_size + 1;
                    current = self.nodes[idx].right;
                }
            }
        }
        None
    }

    /// Returns the rank (0-indexed position) of a key in sorted order.
    /// For missing keys, returns the number of keys strictly less than `key`.
    pub fn rank(&self, key: &K) -> usize {
        let mut current = self.root;
        let mut rank = 0;
        while let Some(idx) = current {
            match key.cmp(&self.nodes[idx].key) {
                std::cmp::Ordering::Less => {
                    current = self.nodes[idx].left;
                }
                std::cmp::Ordering::Equal => {
                    return rank + self.node_size(self.nodes[idx].left);
                }
                std::cmp::Ordering::Greater => {
                    rank += self.node_size(self.nodes[idx].left) + 1;
                    current = self.nodes[idx].right;
                }
            }
        }
        rank
    }

    /// Returns the minimum key-value pair.
    pub fn min(&self) -> Option<(&K, &V)> {
        let mut current = self.root?;
        while let Some(left) = self.nodes[current].left {
            current = left;
        }
        Some((&self.nodes[current].key, &self.nodes[current].value))
    }

    /// Returns the maximum key-value pair.
    pub fn max(&self) -> Option<(&K, &V)> {
        let mut current = self.root?;
        while let Some(right) = self.nodes[current].right {
            current = right;
        }
        Some((&self.nodes[current].key, &self.nodes[current].value))
    }

    /// Returns all keys in sorted order.
    pub fn keys(&self) -> Vec<&K> {
        let mut result = Vec::with_capacity(self.len());
        self.collect_keys(self.root, &mut result);
        result
    }

    fn collect_keys<'a>(&'a self, node: Option<usize>, out: &mut Vec<&'a K>) {
        if let Some(idx) = node {
            self.collect_keys(self.nodes[idx].left, out);
            out.push(&self.nodes[idx].key);
            self.collect_keys(self.nodes[idx].right, out);
        }
    }

    /// Iterates over key-value pairs in sorted order.
    pub fn iter(&self) -> Vec<(&K, &V)> {
        let mut result = Vec::with_capacity(self.len());
        self.collect_pairs(self.root, &mut result);
        result
    }

    fn collect_pairs<'a>(&'a self, node: Option<usize>, out: &mut Vec<(&'a K, &'a V)>) {
        if let Some(idx) = node {
            self.collect_pairs(self.nodes[idx].left, out);
            out.push((&self.nodes[idx].key, &self.nodes[idx].value));
            self.collect_pairs(self.nodes[idx].right, out);
        }
    }
}

impl<K: Ord + Clone, V: Clone> Default for SplayTree<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone + fmt::Display, V: Clone + fmt::Display> fmt::Display for SplayTree<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SplayTree({} elements)", self.len())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree() {
        let tree: SplayTree<i32, i32> = SplayTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert!(tree.min().is_none());
        assert!(tree.max().is_none());
        assert!(tree.kth(0).is_none());
    }

    #[test]
    fn default_is_empty() {
        let tree: SplayTree<i32, i32> = SplayTree::default();
        assert!(tree.is_empty());
    }

    #[test]
    fn single_insert() {
        let mut tree = SplayTree::new();
        assert_eq!(tree.insert(1, 10), None);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(&1), Some(&10));
    }

    #[test]
    fn multiple_inserts() {
        let mut tree = SplayTree::new();
        tree.insert(3, 30);
        tree.insert(1, 10);
        tree.insert(2, 20);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.get(&1), Some(&10));
        assert_eq!(tree.get(&2), Some(&20));
        assert_eq!(tree.get(&3), Some(&30));
    }

    #[test]
    fn overwrite() {
        let mut tree = SplayTree::new();
        assert_eq!(tree.insert(1, 10), None);
        assert_eq!(tree.insert(1, 20), Some(10));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(&1), Some(&20));
    }

    #[test]
    fn contains_key() {
        let mut tree = SplayTree::new();
        tree.insert(5, 50);
        assert!(tree.contains_key(&5));
        assert!(!tree.contains_key(&3));
    }

    #[test]
    fn peek_no_splay() {
        let mut tree = SplayTree::new();
        tree.insert(5, 50);
        tree.insert(3, 30);
        tree.insert(7, 70);
        // Peek doesn't change structure
        assert_eq!(tree.peek(&3), Some(&30));
        assert_eq!(tree.peek(&99), None);
    }

    #[test]
    fn remove() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(2, 20);
        tree.insert(3, 30);
        assert_eq!(tree.remove(&2), Some(20));
        assert_eq!(tree.len(), 2);
        assert!(tree.get(&2).is_none());
        assert_eq!(tree.get(&1), Some(&10));
        assert_eq!(tree.get(&3), Some(&30));
    }

    #[test]
    fn remove_nonexistent() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        assert_eq!(tree.remove(&99), None);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn sorted_order() {
        let mut tree = SplayTree::new();
        for i in [5, 2, 8, 1, 4, 7, 9, 3, 6] {
            tree.insert(i, i * 10);
        }
        let keys: Vec<i32> = tree.keys().into_iter().copied().collect();
        assert_eq!(keys, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn kth_element() {
        let mut tree = SplayTree::new();
        for i in [5, 2, 8, 1, 4] {
            tree.insert(i, i * 10);
        }
        assert_eq!(tree.kth(0), Some((&1, &10)));
        assert_eq!(tree.kth(2), Some((&4, &40)));
        assert_eq!(tree.kth(4), Some((&8, &80)));
        assert!(tree.kth(5).is_none());
    }

    #[test]
    fn rank() {
        let mut tree = SplayTree::new();
        for i in [5, 2, 8, 1, 4] {
            tree.insert(i, i * 10);
        }
        assert_eq!(tree.rank(&1), 0);
        assert_eq!(tree.rank(&4), 2);
        assert_eq!(tree.rank(&8), 4);
        // Missing keys: rank = number of keys less than probe
        assert_eq!(tree.rank(&3), 2);
    }

    #[test]
    fn min_max() {
        let mut tree = SplayTree::new();
        tree.insert(5, 50);
        tree.insert(2, 20);
        tree.insert(8, 80);
        assert_eq!(tree.min(), Some((&2, &20)));
        assert_eq!(tree.max(), Some((&8, &80)));
    }

    #[test]
    fn serde_roundtrip() {
        let mut tree = SplayTree::new();
        for i in 0..20 {
            tree.insert(i, i * 100);
        }
        let json = serde_json::to_string(&tree).unwrap();
        let restored: SplayTree<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), tree.len());
        for i in 0..20 {
            assert_eq!(restored.peek(&i), Some(&(i * 100)));
        }
    }

    #[test]
    fn large_tree() {
        let mut tree = SplayTree::new();
        for i in 0..1000 {
            tree.insert(i, i);
        }
        assert_eq!(tree.len(), 1000);
        for i in 0..1000 {
            assert_eq!(tree.get(&i), Some(&i));
        }
    }

    #[test]
    fn display_format() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(2, 20);
        assert_eq!(format!("{}", tree), "SplayTree(2 elements)");
    }

    #[test]
    fn string_keys() {
        let mut tree = SplayTree::new();
        tree.insert("hello".to_string(), 1);
        tree.insert("world".to_string(), 2);
        assert_eq!(tree.get(&"hello".to_string()), Some(&1));
        assert_eq!(tree.get(&"world".to_string()), Some(&2));
    }

    #[test]
    fn iter_pairs() {
        let mut tree = SplayTree::new();
        tree.insert(3, 30);
        tree.insert(1, 10);
        tree.insert(2, 20);
        let pairs: Vec<(i32, i32)> = tree.iter().into_iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(pairs, vec![(1, 10), (2, 20), (3, 30)]);
    }

    #[test]
    fn access_promotes_to_root() {
        let mut tree = SplayTree::new();
        tree.insert(5, 50);
        tree.insert(3, 30);
        tree.insert(7, 70);
        tree.insert(1, 10);
        tree.insert(9, 90);

        // Access key 1 — it should be splayed to root
        tree.get(&1);
        // Root should now be 1
        let root = tree.root.unwrap();
        assert_eq!(tree.nodes[root].key, 1);
    }

    // ── Expanded test coverage ──────────────────────────────────────

    #[test]
    fn get_on_empty() {
        let mut tree: SplayTree<i32, i32> = SplayTree::new();
        assert!(tree.get(&42).is_none());
    }

    #[test]
    fn remove_from_empty() {
        let mut tree: SplayTree<i32, i32> = SplayTree::new();
        assert_eq!(tree.remove(&1), None);
    }

    #[test]
    fn contains_key_on_empty() {
        let mut tree: SplayTree<i32, i32> = SplayTree::new();
        assert!(!tree.contains_key(&1));
    }

    #[test]
    fn peek_on_empty() {
        let tree: SplayTree<i32, i32> = SplayTree::new();
        assert!(tree.peek(&1).is_none());
    }

    #[test]
    fn rank_on_empty() {
        let tree: SplayTree<i32, i32> = SplayTree::new();
        assert_eq!(tree.rank(&42), 0);
    }

    #[test]
    fn keys_on_empty() {
        let tree: SplayTree<i32, i32> = SplayTree::new();
        assert!(tree.keys().is_empty());
    }

    #[test]
    fn iter_on_empty() {
        let tree: SplayTree<i32, i32> = SplayTree::new();
        assert!(tree.iter().is_empty());
    }

    #[test]
    fn remove_sole_element() {
        let mut tree = SplayTree::new();
        tree.insert(42, 100);
        assert_eq!(tree.remove(&42), Some(100));
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert!(tree.min().is_none());
    }

    #[test]
    fn remove_all_elements() {
        let mut tree = SplayTree::new();
        for i in 0..10 {
            tree.insert(i, i * 10);
        }
        for i in 0..10 {
            assert_eq!(tree.remove(&i), Some(i * 10));
        }
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn insert_ascending_order() {
        let mut tree = SplayTree::new();
        for i in 0..50 {
            tree.insert(i, i);
        }
        assert_eq!(tree.len(), 50);
        let keys: Vec<i32> = tree.keys().into_iter().copied().collect();
        let expected: Vec<i32> = (0..50).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn insert_descending_order() {
        let mut tree = SplayTree::new();
        for i in (0..50).rev() {
            tree.insert(i, i);
        }
        assert_eq!(tree.len(), 50);
        let keys: Vec<i32> = tree.keys().into_iter().copied().collect();
        let expected: Vec<i32> = (0..50).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn min_max_single_element() {
        let mut tree = SplayTree::new();
        tree.insert(42, 100);
        assert_eq!(tree.min(), Some((&42, &100)));
        assert_eq!(tree.max(), Some((&42, &100)));
    }

    #[test]
    fn kth_all_elements() {
        let mut tree = SplayTree::new();
        for i in [5, 3, 7, 1, 4, 6, 8, 2] {
            tree.insert(i, i * 10);
        }
        let sorted = [1, 2, 3, 4, 5, 6, 7, 8];
        for (k, &expected_key) in sorted.iter().enumerate() {
            let (key, val) = tree.kth(k).unwrap();
            assert_eq!(*key, expected_key);
            assert_eq!(*val, expected_key * 10);
        }
    }

    #[test]
    fn rank_all_elements() {
        let mut tree = SplayTree::new();
        for i in [5, 2, 8, 1, 4, 7, 9, 3, 6] {
            tree.insert(i, 0);
        }
        for i in 1..=9 {
            assert_eq!(tree.rank(&i), (i - 1) as usize);
        }
    }

    #[test]
    fn rank_missing_keys() {
        let mut tree = SplayTree::new();
        tree.insert(10, 0);
        tree.insert(20, 0);
        tree.insert(30, 0);

        // Keys less than 10: 0
        assert_eq!(tree.rank(&5), 0);
        // Keys less than 15: 1 (just 10)
        assert_eq!(tree.rank(&15), 1);
        // Keys less than 25: 2 (10, 20)
        assert_eq!(tree.rank(&25), 2);
        // Keys less than 35: 3 (10, 20, 30)
        assert_eq!(tree.rank(&35), 3);
    }

    #[test]
    fn free_list_reuse() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(2, 20);
        tree.insert(3, 30);
        let arena_before = tree.nodes.len();

        // Remove and reinsert should reuse arena slots
        tree.remove(&2);
        tree.insert(4, 40);
        // Arena should not grow (free slot reused)
        assert_eq!(tree.nodes.len(), arena_before);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.get(&4), Some(&40));
    }

    #[test]
    fn clone_independence() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(2, 20);

        let mut cloned = tree.clone();
        cloned.insert(3, 30);
        cloned.remove(&1);

        assert_eq!(tree.len(), 2);
        assert_eq!(cloned.len(), 2);
        assert!(tree.peek(&1).is_some());
        assert!(cloned.peek(&1).is_none());
    }

    #[test]
    fn interleaved_insert_remove() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(2, 20);
        tree.remove(&1);
        tree.insert(3, 30);
        tree.insert(4, 40);
        tree.remove(&3);

        assert_eq!(tree.len(), 2);
        assert!(tree.peek(&1).is_none());
        assert_eq!(tree.peek(&2), Some(&20));
        assert!(tree.peek(&3).is_none());
        assert_eq!(tree.peek(&4), Some(&40));
    }

    #[test]
    fn overwrite_preserves_size() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(2, 20);
        tree.insert(3, 30);

        // Overwrite key 2
        tree.insert(2, 200);
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.get(&2), Some(&200));
    }

    #[test]
    fn size_consistency_after_operations() {
        let mut tree = SplayTree::new();
        for i in 0..20 {
            tree.insert(i, i);
            assert_eq!(tree.len(), (i + 1) as usize);
        }
        for i in 0..10 {
            tree.remove(&i);
            assert_eq!(tree.len(), (19 - i) as usize);
        }
        // Remaining: 10..20
        let keys: Vec<i32> = tree.keys().into_iter().copied().collect();
        assert_eq!(keys, (10..20).collect::<Vec<i32>>());
    }

    #[test]
    fn get_splays_to_root() {
        let mut tree = SplayTree::new();
        for i in 0..10 {
            tree.insert(i, i);
        }
        // Access key 5 — should become root
        tree.get(&5);
        let root = tree.root.unwrap();
        assert_eq!(tree.nodes[root].key, 5);

        // Access key 0 — should become root
        tree.get(&0);
        let root = tree.root.unwrap();
        assert_eq!(tree.nodes[root].key, 0);
    }

    #[test]
    fn get_nonexistent_still_splays() {
        let mut tree = SplayTree::new();
        tree.insert(1, 10);
        tree.insert(5, 50);
        tree.insert(10, 100);

        // Get nonexistent key 3 — nearest key splayed to root
        assert!(tree.get(&3).is_none());
        // Tree should still be valid
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn display_empty() {
        let tree: SplayTree<i32, i32> = SplayTree::new();
        assert_eq!(format!("{}", tree), "SplayTree(0 elements)");
    }

    #[test]
    fn serde_roundtrip_after_removes() {
        let mut tree = SplayTree::new();
        for i in 0..10 {
            tree.insert(i, i * 10);
        }
        tree.remove(&3);
        tree.remove(&7);

        let json = serde_json::to_string(&tree).unwrap();
        let restored: SplayTree<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 8);
        assert!(restored.peek(&3).is_none());
        assert!(restored.peek(&7).is_none());
        assert_eq!(restored.peek(&5), Some(&50));
    }

    #[test]
    fn remove_min_and_max() {
        let mut tree = SplayTree::new();
        for i in 1..=5 {
            tree.insert(i, i);
        }
        // Remove min
        let (min_key, _) = tree.min().map(|(&k, &v)| (k, v)).unwrap();
        tree.remove(&min_key);
        assert_eq!(tree.min().map(|(&k, _)| k), Some(2));

        // Remove max
        let (max_key, _) = tree.max().map(|(&k, &v)| (k, v)).unwrap();
        tree.remove(&max_key);
        assert_eq!(tree.max().map(|(&k, _)| k), Some(4));
    }

    #[test]
    fn remove_root_node() {
        let mut tree = SplayTree::new();
        tree.insert(5, 50);
        tree.insert(3, 30);
        tree.insert(7, 70);

        // Get 5 to ensure it's root
        tree.get(&5);
        let root = tree.root.unwrap();
        assert_eq!(tree.nodes[root].key, 5);

        // Remove root
        tree.remove(&5);
        assert_eq!(tree.len(), 2);
        assert!(tree.peek(&5).is_none());
        assert_eq!(tree.peek(&3), Some(&30));
        assert_eq!(tree.peek(&7), Some(&70));
    }

    #[test]
    fn kth_and_rank_inverse() {
        let mut tree = SplayTree::new();
        for i in [10, 20, 30, 40, 50] {
            tree.insert(i, 0);
        }
        // kth(rank(k)) == k for all k in tree
        for &k in &[10, 20, 30, 40, 50] {
            let r = tree.rank(&k);
            let (key, _) = tree.kth(r).unwrap();
            assert_eq!(*key, k);
        }
    }
}
