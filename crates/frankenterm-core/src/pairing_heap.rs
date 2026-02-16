//! Pairing heap — a simple, efficient mergeable priority queue.
//!
//! A pairing heap supports O(1) insert and merge, with amortized
//! O(log n) delete-min. It has excellent practical performance and
//! is one of the simplest heap implementations.
//!
//! # Properties
//!
//! - **O(1)**: insert, find-min, merge
//! - **O(log n) amortized**: delete-min, decrease-key
//! - **Mergeable**: Two heaps can be combined in O(1)
//! - **Arena-allocated**: Cache-friendly, no Box indirection
//!
//! # Use in FrankenTerm
//!
//! Priority scheduling of pane capture tasks, event queue management,
//! and timer heaps where merge operations are needed.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Node ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HeapNode<K, V> {
    key: K,
    value: V,
    child: Option<usize>,   // First child
    sibling: Option<usize>, // Next sibling
}

// ── PairingHeap ───────────────────────────────────────────────────────

/// Min-pairing heap with arena allocation.
///
/// Elements with the smallest key are at the top.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairingHeap<K, V> {
    nodes: Vec<HeapNode<K, V>>,
    root: Option<usize>,
    count: usize,
    free: Vec<usize>,
}

impl<K: Ord + Clone, V: Clone> PairingHeap<K, V> {
    /// Creates an empty pairing heap.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            count: 0,
            free: Vec::new(),
        }
    }

    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the heap is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns a reference to the minimum element, or None if empty.
    pub fn peek(&self) -> Option<(&K, &V)> {
        self.root.map(|r| (&self.nodes[r].key, &self.nodes[r].value))
    }

    fn alloc_node(&mut self, key: K, value: V) -> usize {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = HeapNode {
                key,
                value,
                child: None,
                sibling: None,
            };
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(HeapNode {
                key,
                value,
                child: None,
                sibling: None,
            });
            idx
        }
    }

    /// Links two heap nodes, making the one with larger key a child of the other.
    /// Returns the index of the winner (smaller key).
    fn link(&mut self, a: usize, b: usize) -> usize {
        if self.nodes[a].key <= self.nodes[b].key {
            // b becomes child of a
            self.nodes[b].sibling = self.nodes[a].child;
            self.nodes[a].child = Some(b);
            a
        } else {
            // a becomes child of b
            self.nodes[a].sibling = self.nodes[b].child;
            self.nodes[b].child = Some(a);
            b
        }
    }

    /// Inserts a key-value pair. Returns the node index.
    pub fn insert(&mut self, key: K, value: V) -> usize {
        let idx = self.alloc_node(key, value);
        self.root = match self.root {
            None => Some(idx),
            Some(r) => Some(self.link(r, idx)),
        };
        self.count += 1;
        idx
    }

    /// Removes and returns the minimum element.
    pub fn pop(&mut self) -> Option<(K, V)> {
        let root = self.root?;
        let result = (
            self.nodes[root].key.clone(),
            self.nodes[root].value.clone(),
        );

        // Merge children using two-pass pairing
        let first_child = self.nodes[root].child;
        self.root = self.merge_pairs(first_child);

        self.free.push(root);
        self.count -= 1;

        Some(result)
    }

    /// Two-pass pairing: pair up siblings left-to-right, then merge right-to-left.
    fn merge_pairs(&mut self, first: Option<usize>) -> Option<usize> {
        let first = first?;

        let second = self.nodes[first].sibling;
        if second.is_none() {
            self.nodes[first].sibling = None;
            return Some(first);
        }
        let second = second.unwrap();

        let rest = self.nodes[second].sibling;

        // Disconnect siblings
        self.nodes[first].sibling = None;
        self.nodes[second].sibling = None;

        // Pair first two
        let paired = self.link(first, second);

        // Recursively merge the rest
        match self.merge_pairs(rest) {
            None => Some(paired),
            Some(rest_root) => Some(self.link(paired, rest_root)),
        }
    }

    /// Merges another pairing heap into this one.
    /// The other heap is consumed (left empty).
    pub fn merge(&mut self, other: &mut PairingHeap<K, V>) {
        if other.is_empty() {
            return;
        }

        // Copy all nodes from other into our arena
        let offset = self.nodes.len();
        for node in &other.nodes {
            let mut copied = node.clone();
            copied.child = copied.child.map(|c| c + offset);
            copied.sibling = copied.sibling.map(|s| s + offset);
            self.nodes.push(copied);
        }

        let other_root = other.root.map(|r| r + offset);
        self.root = match (self.root, other_root) {
            (None, r) => r,
            (r, None) => r,
            (Some(a), Some(b)) => Some(self.link(a, b)),
        };

        self.count += other.count;

        // Clear other
        other.nodes.clear();
        other.root = None;
        other.count = 0;
        other.free.clear();
    }

    /// Returns all elements in sorted order (ascending by key).
    /// This consumes the heap's elements.
    pub fn into_sorted(mut self) -> Vec<(K, V)> {
        let mut result = Vec::with_capacity(self.count);
        while let Some(item) = self.pop() {
            result.push(item);
        }
        result
    }

    /// Returns all elements in sorted order without consuming.
    pub fn sorted(&self) -> Vec<(K, V)> {
        self.clone().into_sorted()
    }
}

impl<K: Ord + Clone, V: Clone> Default for PairingHeap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone + fmt::Display, V: Clone> fmt::Display for PairingHeap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PairingHeap({} elements)", self.count)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let heap: PairingHeap<i32, i32> = PairingHeap::new();
        assert!(heap.is_empty());
        assert_eq!(heap.len(), 0);
        assert!(heap.peek().is_none());
    }

    #[test]
    fn default_is_empty() {
        let heap: PairingHeap<i32, i32> = PairingHeap::default();
        assert!(heap.is_empty());
    }

    #[test]
    fn single_insert() {
        let mut heap = PairingHeap::new();
        heap.insert(5, 50);
        assert_eq!(heap.len(), 1);
        assert_eq!(heap.peek(), Some((&5, &50)));
    }

    #[test]
    fn insert_maintains_min() {
        let mut heap = PairingHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        heap.insert(7, 70);
        assert_eq!(heap.peek(), Some((&3, &30)));
    }

    #[test]
    fn pop_returns_min() {
        let mut heap = PairingHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        heap.insert(7, 70);
        assert_eq!(heap.pop(), Some((3, 30)));
        assert_eq!(heap.pop(), Some((5, 50)));
        assert_eq!(heap.pop(), Some((7, 70)));
        assert!(heap.pop().is_none());
    }

    #[test]
    fn sorted_order() {
        let mut heap = PairingHeap::new();
        for val in [5, 2, 8, 1, 4, 7, 9, 3, 6] {
            heap.insert(val, val * 10);
        }
        let sorted = heap.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn merge_heaps() {
        let mut h1 = PairingHeap::new();
        h1.insert(1, 10);
        h1.insert(3, 30);
        h1.insert(5, 50);

        let mut h2 = PairingHeap::new();
        h2.insert(2, 20);
        h2.insert(4, 40);

        h1.merge(&mut h2);
        assert_eq!(h1.len(), 5);
        assert!(h2.is_empty());

        let sorted = h1.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn merge_empty() {
        let mut h1 = PairingHeap::new();
        h1.insert(1, 10);

        let mut h2: PairingHeap<i32, i32> = PairingHeap::new();
        h1.merge(&mut h2);
        assert_eq!(h1.len(), 1);
    }

    #[test]
    fn merge_into_empty() {
        let mut h1: PairingHeap<i32, i32> = PairingHeap::new();

        let mut h2 = PairingHeap::new();
        h2.insert(1, 10);
        h2.insert(2, 20);

        h1.merge(&mut h2);
        assert_eq!(h1.len(), 2);
        assert_eq!(h1.peek(), Some((&1, &10)));
    }

    #[test]
    fn duplicate_keys() {
        let mut heap = PairingHeap::new();
        heap.insert(3, 30);
        heap.insert(3, 31);
        heap.insert(3, 32);
        assert_eq!(heap.len(), 3);
        assert_eq!(heap.pop().unwrap().0, 3);
        assert_eq!(heap.pop().unwrap().0, 3);
        assert_eq!(heap.pop().unwrap().0, 3);
    }

    #[test]
    fn large_heap() {
        let mut heap = PairingHeap::new();
        for i in (0..1000).rev() {
            heap.insert(i, i);
        }
        assert_eq!(heap.len(), 1000);
        for i in 0..1000 {
            assert_eq!(heap.pop(), Some((i, i)));
        }
    }

    #[test]
    fn serde_roundtrip() {
        let mut heap = PairingHeap::new();
        for i in [5, 3, 7, 1, 9] {
            heap.insert(i, i * 10);
        }
        let json = serde_json::to_string(&heap).unwrap();
        let restored: PairingHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), heap.len());
        let sorted = restored.sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn display_format() {
        let mut heap = PairingHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        assert_eq!(format!("{}", heap), "PairingHeap(2 elements)");
    }

    #[test]
    fn sorted_without_consuming() {
        let mut heap = PairingHeap::new();
        heap.insert(3, 30);
        heap.insert(1, 10);
        heap.insert(2, 20);
        let sorted = heap.sorted();
        assert_eq!(sorted.len(), 3);
        // Original heap should still have its elements
        assert_eq!(heap.len(), 3);
    }

    #[test]
    fn string_keys() {
        let mut heap = PairingHeap::new();
        heap.insert("cherry".to_string(), 3);
        heap.insert("apple".to_string(), 1);
        heap.insert("banana".to_string(), 2);
        assert_eq!(heap.pop().unwrap().0, "apple");
        assert_eq!(heap.pop().unwrap().0, "banana");
        assert_eq!(heap.pop().unwrap().0, "cherry");
    }
}
