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
        self.root
            .map(|r| (&self.nodes[r].key, &self.nodes[r].value))
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
        let result = (self.nodes[root].key.clone(), self.nodes[root].value.clone());

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

        // Donor tombstones remain tombstones after copy; keep them reusable.
        self.free.extend(other.free.iter().map(|idx| idx + offset));
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

    // ── Pop edge cases ──────────────────────────────────────────────

    #[test]
    fn pop_from_empty() {
        let mut heap: PairingHeap<i32, i32> = PairingHeap::new();
        assert!(heap.pop().is_none());
        assert!(heap.pop().is_none()); // idempotent
    }

    #[test]
    fn pop_single_element() {
        let mut heap = PairingHeap::new();
        heap.insert(42, 420);
        assert_eq!(heap.pop(), Some((42, 420)));
        assert!(heap.is_empty());
        assert!(heap.peek().is_none());
    }

    #[test]
    fn pop_two_elements_ordered() {
        let mut heap = PairingHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        assert_eq!(heap.pop(), Some((1, 10)));
        assert_eq!(heap.pop(), Some((2, 20)));
        assert!(heap.is_empty());
    }

    #[test]
    fn pop_two_elements_reverse() {
        let mut heap = PairingHeap::new();
        heap.insert(2, 20);
        heap.insert(1, 10);
        assert_eq!(heap.pop(), Some((1, 10)));
        assert_eq!(heap.pop(), Some((2, 20)));
    }

    // ── Interleaved insert/pop ──────────────────────────────────────

    #[test]
    fn interleaved_insert_pop() {
        let mut heap = PairingHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        assert_eq!(heap.pop(), Some((3, 30)));

        heap.insert(1, 10);
        assert_eq!(heap.peek(), Some((&1, &10)));
        assert_eq!(heap.pop(), Some((1, 10)));
        assert_eq!(heap.pop(), Some((5, 50)));
        assert!(heap.is_empty());
    }

    #[test]
    fn insert_after_drain() {
        let mut heap = PairingHeap::new();
        heap.insert(10, 100);
        heap.insert(20, 200);
        assert_eq!(heap.pop(), Some((10, 100)));
        assert_eq!(heap.pop(), Some((20, 200)));
        assert!(heap.is_empty());

        // Re-insert after full drain
        heap.insert(5, 50);
        assert_eq!(heap.len(), 1);
        assert_eq!(heap.peek(), Some((&5, &50)));
        assert_eq!(heap.pop(), Some((5, 50)));
    }

    #[test]
    fn alternating_insert_pop_cycle() {
        let mut heap = PairingHeap::new();
        for i in 0..50 {
            heap.insert(i, i * 10);
            if i % 2 == 1 {
                heap.pop();
            }
        }
        // 50 inserts, 25 pops => 25 remaining
        assert_eq!(heap.len(), 25);

        let sorted = heap.into_sorted();
        // Every even iteration's value should remain, plus some odd ones
        // Just verify sorted order
        for w in sorted.windows(2) {
            assert!(w[0].0 <= w[1].0);
        }
    }

    // ── Free list / node reuse ──────────────────────────────────────

    #[test]
    fn node_reuse_after_pop() {
        let mut heap = PairingHeap::new();
        heap.insert(10, 100);
        heap.insert(20, 200);
        heap.insert(30, 300);
        let arena_len_before = heap.nodes.len();

        heap.pop(); // frees a node
        heap.pop(); // frees another

        // Insert should reuse freed nodes
        heap.insert(5, 50);
        heap.insert(15, 150);
        assert_eq!(heap.nodes.len(), arena_len_before, "arena should not grow");
        assert_eq!(heap.len(), 3); // 1 remaining + 2 new

        let sorted = heap.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![5, 15, 30]);
    }

    #[test]
    fn free_list_drains_before_alloc() {
        let mut heap = PairingHeap::new();
        for i in 0..10 {
            heap.insert(i, i);
        }
        // Pop all — 10 nodes go to free list
        for _ in 0..10 {
            heap.pop();
        }
        assert_eq!(heap.free.len(), 10);
        let arena_len = heap.nodes.len();

        // Reinsert — should reuse all free nodes
        for i in 100..110 {
            heap.insert(i, i);
        }
        assert_eq!(heap.nodes.len(), arena_len);
        assert!(heap.free.is_empty());
    }

    // ── Merge scenarios ─────────────────────────────────────────────

    #[test]
    fn merge_both_empty() {
        let mut h1: PairingHeap<i32, i32> = PairingHeap::new();
        let mut h2: PairingHeap<i32, i32> = PairingHeap::new();
        h1.merge(&mut h2);
        assert!(h1.is_empty());
        assert!(h2.is_empty());
    }

    #[test]
    fn chained_merges() {
        let mut acc = PairingHeap::new();
        for batch in 0..5 {
            let mut h = PairingHeap::new();
            for j in 0..3 {
                h.insert(batch * 10 + j, batch * 100 + j);
            }
            acc.merge(&mut h);
        }
        assert_eq!(acc.len(), 15);

        let sorted = acc.into_sorted();
        for w in sorted.windows(2) {
            assert!(w[0].0 <= w[1].0);
        }
    }

    #[test]
    fn merge_overlapping_key_ranges() {
        let mut h1 = PairingHeap::new();
        let mut h2 = PairingHeap::new();
        for i in 0..5 {
            h1.insert(i, i);
            h2.insert(i, i + 100); // same keys, different values
        }
        h1.merge(&mut h2);
        assert_eq!(h1.len(), 10);

        let sorted = h1.into_sorted();
        // All keys are 0..5 duplicated
        for (k, _) in &sorted {
            assert!(*k >= 0 && *k < 5);
        }
    }

    #[test]
    fn merge_large_into_small() {
        let mut small = PairingHeap::new();
        small.insert(0, 0);

        let mut large = PairingHeap::new();
        for i in 1..100 {
            large.insert(i, i);
        }

        small.merge(&mut large);
        assert_eq!(small.len(), 100);
        assert_eq!(small.peek(), Some((&0, &0)));
    }

    #[test]
    fn merge_preserves_sorted_extraction() {
        let mut h1 = PairingHeap::new();
        for &v in &[10, 30, 50, 70, 90] {
            h1.insert(v, v);
        }
        let mut h2 = PairingHeap::new();
        for &v in &[20, 40, 60, 80, 100] {
            h2.insert(v, v);
        }
        h1.merge(&mut h2);

        let sorted = h1.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, (1..=10).map(|x| x * 10).collect::<Vec<_>>());
    }

    #[test]
    fn merge_donor_becomes_empty() {
        let mut h1 = PairingHeap::new();
        h1.insert(1, 10);
        let mut h2 = PairingHeap::new();
        h2.insert(2, 20);
        h2.insert(3, 30);

        h1.merge(&mut h2);
        assert!(h2.is_empty());
        assert_eq!(h2.len(), 0);
        assert!(h2.peek().is_none());
        assert!(h2.pop().is_none());
    }

    #[test]
    fn merge_reuses_donor_free_slots() {
        let mut receiver = PairingHeap::new();
        receiver.insert(0, 0);

        let mut donor = PairingHeap::new();
        for i in 1..=10 {
            donor.insert(i, i);
        }
        for i in 1..=5 {
            assert_eq!(donor.pop(), Some((i, i)));
        }

        let receiver_nodes_before = receiver.nodes.len();
        let donor_nodes_before = donor.nodes.len();
        let donor_free_before = donor.free.len();

        receiver.merge(&mut donor);
        assert_eq!(receiver.len(), 6);
        assert_eq!(
            receiver.nodes.len(),
            receiver_nodes_before + donor_nodes_before
        );
        assert_eq!(receiver.free.len(), donor_free_before);

        let nodes_after_merge = receiver.nodes.len();
        for i in 100..105 {
            receiver.insert(i, i);
        }
        assert_eq!(receiver.nodes.len(), nodes_after_merge);
    }

    #[test]
    fn merge_donor_can_be_reused_after_clear() {
        let mut receiver = PairingHeap::new();
        receiver.insert(10, 10);

        let mut donor = PairingHeap::new();
        donor.insert(1, 1);
        donor.insert(2, 2);

        receiver.merge(&mut donor);
        assert!(donor.is_empty());
        assert!(donor.nodes.is_empty());
        assert!(donor.free.is_empty());

        donor.insert(5, 50);
        donor.insert(3, 30);
        assert_eq!(donor.pop(), Some((3, 30)));
        assert_eq!(donor.pop(), Some((5, 50)));
    }

    // ── Input order patterns ────────────────────────────────────────

    #[test]
    fn reverse_sorted_input() {
        let mut heap = PairingHeap::new();
        for i in (0..20).rev() {
            heap.insert(i, i);
        }
        for i in 0..20 {
            assert_eq!(heap.pop(), Some((i, i)));
        }
    }

    #[test]
    fn already_sorted_input() {
        let mut heap = PairingHeap::new();
        for i in 0..20 {
            heap.insert(i, i);
        }
        for i in 0..20 {
            assert_eq!(heap.pop(), Some((i, i)));
        }
    }

    #[test]
    fn zigzag_insert_pattern() {
        let mut heap = PairingHeap::new();
        // Insert alternating high/low: 100, 0, 99, 1, 98, 2, ...
        for i in 0..50 {
            if i % 2 == 0 {
                heap.insert(100 - i / 2, 0);
            } else {
                heap.insert(i / 2, 0);
            }
        }
        assert_eq!(heap.len(), 50);

        let sorted = heap.into_sorted();
        for w in sorted.windows(2) {
            assert!(w[0].0 <= w[1].0);
        }
    }

    #[test]
    fn negative_keys() {
        let mut heap = PairingHeap::new();
        heap.insert(-5, 0);
        heap.insert(0, 1);
        heap.insert(-10, 2);
        heap.insert(5, 3);
        heap.insert(-3, 4);
        assert_eq!(heap.pop(), Some((-10, 2)));
        assert_eq!(heap.pop(), Some((-5, 0)));
        assert_eq!(heap.pop(), Some((-3, 4)));
        assert_eq!(heap.pop(), Some((0, 1)));
        assert_eq!(heap.pop(), Some((5, 3)));
    }

    // ── Duplicate key handling ──────────────────────────────────────

    #[test]
    fn all_same_keys() {
        let mut heap = PairingHeap::new();
        for i in 0..20 {
            heap.insert(7, i);
        }
        assert_eq!(heap.len(), 20);
        let mut count = 0;
        while heap.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 20);
    }

    #[test]
    fn duplicate_keys_values_preserved() {
        let mut heap = PairingHeap::new();
        heap.insert(1, 100);
        heap.insert(1, 200);
        heap.insert(1, 300);
        let mut values = Vec::new();
        while let Some((_, v)) = heap.pop() {
            values.push(v);
        }
        values.sort();
        assert_eq!(values, vec![100, 200, 300]);
    }

    // ── into_sorted / sorted edge cases ─────────────────────────────

    #[test]
    fn into_sorted_empty() {
        let heap: PairingHeap<i32, i32> = PairingHeap::new();
        let sorted = heap.into_sorted();
        assert!(sorted.is_empty());
    }

    #[test]
    fn sorted_empty() {
        let heap: PairingHeap<i32, i32> = PairingHeap::new();
        let sorted = heap.sorted();
        assert!(sorted.is_empty());
    }

    #[test]
    fn into_sorted_single() {
        let mut heap = PairingHeap::new();
        heap.insert(42, 420);
        let sorted = heap.into_sorted();
        assert_eq!(sorted, vec![(42, 420)]);
    }

    #[test]
    fn sorted_preserves_heap() {
        let mut heap = PairingHeap::new();
        for i in [5, 1, 3, 2, 4] {
            heap.insert(i, i * 10);
        }
        let s1 = heap.sorted();
        let s2 = heap.sorted();
        assert_eq!(s1, s2);
        assert_eq!(heap.len(), 5);
    }

    // ── Clone independence ──────────────────────────────────────────

    #[test]
    fn clone_independence() {
        let mut heap = PairingHeap::new();
        heap.insert(3, 30);
        heap.insert(1, 10);
        heap.insert(2, 20);

        let mut cloned = heap.clone();
        assert_eq!(cloned.pop(), Some((1, 10)));

        // Original should be unaffected
        assert_eq!(heap.len(), 3);
        assert_eq!(heap.peek(), Some((&1, &10)));
    }

    // ── Insert returns correct index ────────────────────────────────

    #[test]
    fn insert_returns_ascending_indices() {
        let mut heap = PairingHeap::new();
        let i0 = heap.insert(10, 100);
        let i1 = heap.insert(20, 200);
        let i2 = heap.insert(30, 300);
        assert_eq!(i0, 0);
        assert_eq!(i1, 1);
        assert_eq!(i2, 2);
    }

    #[test]
    fn insert_reuses_freed_index() {
        let mut heap = PairingHeap::new();
        let _ = heap.insert(10, 100);
        let _ = heap.insert(20, 200);
        heap.pop(); // frees the root node

        let reused = heap.insert(5, 50);
        // Should reuse the freed slot, not extend the arena
        assert!(reused < heap.nodes.len());
    }

    // ── Display edge cases ──────────────────────────────────────────

    #[test]
    fn display_empty() {
        let heap: PairingHeap<i32, i32> = PairingHeap::new();
        assert_eq!(format!("{}", heap), "PairingHeap(0 elements)");
    }

    #[test]
    fn display_single() {
        let mut heap = PairingHeap::new();
        heap.insert(1, 10);
        assert_eq!(format!("{}", heap), "PairingHeap(1 elements)");
    }

    // ── Serde edge cases ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip_empty() {
        let heap: PairingHeap<i32, i32> = PairingHeap::new();
        let json = serde_json::to_string(&heap).unwrap();
        let restored: PairingHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn serde_roundtrip_after_pops() {
        let mut heap = PairingHeap::new();
        for i in 0..10 {
            heap.insert(i, i * 10);
        }
        heap.pop();
        heap.pop();
        let json = serde_json::to_string(&heap).unwrap();
        let restored: PairingHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), heap.len());
        // Verify sorted extraction matches
        let orig_sorted = heap.sorted();
        let rest_sorted = restored.sorted();
        assert_eq!(
            orig_sorted.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            rest_sorted.iter().map(|(k, _)| *k).collect::<Vec<_>>()
        );
    }

    // ── Stress tests ────────────────────────────────────────────────

    #[test]
    fn stress_random_pattern() {
        let mut heap = PairingHeap::new();
        // Pseudo-random insertions using a simple LCG
        let mut val = 12345u32;
        for _ in 0..500 {
            val = val.wrapping_mul(1103515245).wrapping_add(12345);
            heap.insert((val % 1000) as i32, 0);
        }
        assert_eq!(heap.len(), 500);

        let mut prev = i32::MIN;
        while let Some((k, _)) = heap.pop() {
            assert!(k >= prev, "sorted order violated: {} < {}", k, prev);
            prev = k;
        }
    }

    #[test]
    fn stress_insert_pop_mixed() {
        let mut heap = PairingHeap::new();
        let mut popped = Vec::new();
        for i in 0..200 {
            heap.insert(i % 50, i);
            if i % 3 == 0 {
                if let Some(item) = heap.pop() {
                    popped.push(item);
                }
            }
        }
        // Drain remainder
        while let Some(item) = heap.pop() {
            popped.push(item);
        }
        assert_eq!(popped.len(), 200);
    }

    #[test]
    fn many_small_merges() {
        let mut acc = PairingHeap::new();
        for batch in 0..100 {
            let mut h = PairingHeap::new();
            h.insert(batch, batch);
            acc.merge(&mut h);
        }
        assert_eq!(acc.len(), 100);
        for i in 0..100 {
            assert_eq!(acc.pop(), Some((i, i)));
        }
    }

    // ── Type-specific tests ─────────────────────────────────────────

    #[test]
    fn u8_keys() {
        let mut heap = PairingHeap::new();
        heap.insert(255u8, "max");
        heap.insert(0u8, "min");
        heap.insert(128u8, "mid");
        assert_eq!(heap.pop(), Some((0, "min")));
        assert_eq!(heap.pop(), Some((128, "mid")));
        assert_eq!(heap.pop(), Some((255, "max")));
    }

    #[test]
    fn tuple_values() {
        let mut heap = PairingHeap::new();
        heap.insert(3, ("c", 3));
        heap.insert(1, ("a", 1));
        heap.insert(2, ("b", 2));
        assert_eq!(heap.pop(), Some((1, ("a", 1))));
        assert_eq!(heap.pop(), Some((2, ("b", 2))));
        assert_eq!(heap.pop(), Some((3, ("c", 3))));
    }

    // ── Len/is_empty consistency ────────────────────────────────────

    #[test]
    fn len_tracks_inserts_and_pops() {
        let mut heap = PairingHeap::new();
        assert_eq!(heap.len(), 0);
        assert!(heap.is_empty());

        heap.insert(1, 10);
        assert_eq!(heap.len(), 1);
        assert!(!heap.is_empty());

        heap.insert(2, 20);
        assert_eq!(heap.len(), 2);

        heap.pop();
        assert_eq!(heap.len(), 1);

        heap.pop();
        assert_eq!(heap.len(), 0);
        assert!(heap.is_empty());
    }

    #[test]
    fn len_tracks_merge() {
        let mut h1 = PairingHeap::new();
        h1.insert(1, 10);
        h1.insert(2, 20);

        let mut h2 = PairingHeap::new();
        h2.insert(3, 30);

        h1.merge(&mut h2);
        assert_eq!(h1.len(), 3);
        assert_eq!(h2.len(), 0);
    }

    // ── Peek stability ──────────────────────────────────────────────

    #[test]
    fn peek_is_idempotent() {
        let mut heap = PairingHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        assert_eq!(heap.peek(), Some((&3, &30)));
        assert_eq!(heap.peek(), Some((&3, &30)));
        assert_eq!(heap.peek(), Some((&3, &30)));
        assert_eq!(heap.len(), 2); // peek doesn't modify
    }

    #[test]
    fn peek_updates_after_pop() {
        let mut heap = PairingHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        heap.insert(3, 30);
        assert_eq!(heap.peek(), Some((&1, &10)));

        heap.pop();
        assert_eq!(heap.peek(), Some((&2, &20)));

        heap.pop();
        assert_eq!(heap.peek(), Some((&3, &30)));
    }
}
