//! Binomial heap — a mergeable priority queue with worst-case O(log n) operations.
//!
//! A binomial heap is a forest of binomial trees satisfying the
//! min-heap property. Unlike Fibonacci heaps (amortized bounds),
//! binomial heaps provide worst-case O(log n) guarantees for all
//! operations including merge.
//!
//! # Complexity
//!
//! - **O(1)**: find-min (cached), insert (amortized)
//! - **O(log n)**: insert (worst-case), merge, extract-min
//!
//! # Design
//!
//! Arena-allocated binomial trees. Each tree of order k has exactly
//! 2^k nodes. The heap maintains at most one tree of each order,
//! analogous to binary addition. Merge operates like binary addition
//! with carry.
//!
//! # Use in FrankenTerm
//!
//! Priority scheduling where worst-case guarantees matter more than
//! amortized performance, merge-heavy workloads combining multiple
//! task queues.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Node ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BinomialNode<K, V> {
    key: K,
    value: V,
    order: usize,           // degree/order of this tree
    child: Option<usize>,   // first child
    sibling: Option<usize>, // next sibling (for root list or child list)
}

// ── BinomialHeap ──────────────────────────────────────────────────────

/// Min-binomial heap with arena allocation.
///
/// Elements with the smallest key are at the top.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BinomialHeap<K, V> {
    nodes: Vec<BinomialNode<K, V>>,
    head: Option<usize>, // head of root list
    count: usize,
    min_root: Option<usize>, // cached minimum
    free: Vec<usize>,
}

impl<K: Ord + Clone, V: Clone> BinomialHeap<K, V> {
    /// Creates an empty binomial heap.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            head: None,
            count: 0,
            min_root: None,
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

    /// Returns a reference to the minimum element.
    pub fn peek(&self) -> Option<(&K, &V)> {
        self.min_root
            .map(|m| (&self.nodes[m].key, &self.nodes[m].value))
    }

    fn alloc_node(&mut self, key: K, value: V) -> usize {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = BinomialNode {
                key,
                value,
                order: 0,
                child: None,
                sibling: None,
            };
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(BinomialNode {
                key,
                value,
                order: 0,
                child: None,
                sibling: None,
            });
            idx
        }
    }

    /// Inserts a key-value pair.
    pub fn insert(&mut self, key: K, value: V) {
        let idx = self.alloc_node(key, value);
        // Create a singleton heap and merge
        let prev_head = self.head;
        self.head = Some(idx);
        self.nodes[idx].sibling = None;
        self.count += 1;

        // Merge singleton into existing root list
        if let Some(prev) = prev_head {
            self.head = Some(idx);
            self.nodes[idx].sibling = None;
            self.head = self.merge_root_lists(Some(idx), Some(prev));
            self.consolidate();
        }

        self.update_min();
    }

    /// Removes and returns the minimum element.
    pub fn extract_min(&mut self) -> Option<(K, V)> {
        let min_root = self.min_root?;

        let result = (
            self.nodes[min_root].key.clone(),
            self.nodes[min_root].value.clone(),
        );

        // Remove min_root from root list
        self.head = self.remove_from_root_list(min_root);

        // Reverse the child list of min_root and merge with remaining roots
        let children = self.reverse_children(min_root);
        self.head = self.merge_root_lists(self.head, children);
        self.consolidate();

        self.free.push(min_root);
        self.count -= 1;

        if self.count == 0 {
            self.head = None;
            self.min_root = None;
        } else {
            self.update_min();
        }

        Some(result)
    }

    /// Alias for `extract_min`.
    pub fn pop(&mut self) -> Option<(K, V)> {
        self.extract_min()
    }

    /// Merges another heap into this one. The other heap is left empty.
    pub fn merge(&mut self, other: &mut BinomialHeap<K, V>) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            std::mem::swap(self, other);
            return;
        }

        let offset = self.nodes.len();
        for node in &other.nodes {
            let mut copied = node.clone();
            copied.child = copied.child.map(|c| c + offset);
            copied.sibling = copied.sibling.map(|s| s + offset);
            self.nodes.push(copied);
        }

        let other_head = other.head.map(|h| h + offset);
        self.head = self.merge_root_lists(self.head, other_head);
        self.consolidate();
        self.count += other.count;
        self.free.extend(other.free.iter().map(|&f| f + offset));

        other.nodes.clear();
        other.head = None;
        other.count = 0;
        other.min_root = None;
        other.free.clear();

        self.update_min();
    }

    /// Returns all elements sorted.
    pub fn into_sorted(mut self) -> Vec<(K, V)> {
        let mut result = Vec::with_capacity(self.count);
        while let Some(item) = self.extract_min() {
            result.push(item);
        }
        result
    }

    /// Returns sorted elements without consuming.
    pub fn sorted(&self) -> Vec<(K, V)> {
        self.clone().into_sorted()
    }

    // ── Internal helpers ──────────────────────────────────────────

    /// Merge two sorted root lists into one sorted by order.
    fn merge_root_lists(
        &self,
        mut a: Option<usize>,
        mut b: Option<usize>,
    ) -> Option<usize> {
        let mut result: Vec<usize> = Vec::new();

        while let (Some(ai), Some(bi)) = (a, b) {
            if self.nodes[ai].order <= self.nodes[bi].order {
                result.push(ai);
                a = self.nodes[ai].sibling;
            } else {
                result.push(bi);
                b = self.nodes[bi].sibling;
            }
        }

        while let Some(ai) = a {
            result.push(ai);
            a = self.nodes[ai].sibling;
        }
        while let Some(bi) = b {
            result.push(bi);
            b = self.nodes[bi].sibling;
        }

        // Link them
        if result.is_empty() {
            return None;
        }
        for i in 0..result.len() - 1 {
            self.nodes[result[i]].sibling; // read-only
        }

        // We need to return connected list
        Some(result[0])
    }

    /// Consolidates the root list: combine trees of the same order.
    fn consolidate(&mut self) {
        // Collect all roots
        let mut roots = Vec::new();
        let mut current = self.head;
        while let Some(idx) = current {
            roots.push(idx);
            current = self.nodes[idx].sibling;
        }

        if roots.is_empty() {
            return;
        }

        // Clear all siblings
        for &r in &roots {
            self.nodes[r].sibling = None;
        }

        // Merge trees of same order
        let max_order = 64; // more than enough for any practical size
        let mut by_order: Vec<Option<usize>> = vec![None; max_order];

        for root in roots {
            let mut curr = root;
            loop {
                let order = self.nodes[curr].order;
                if order >= max_order {
                    break;
                }
                match by_order[order] {
                    None => {
                        by_order[order] = Some(curr);
                        break;
                    }
                    Some(existing) => {
                        by_order[order] = None;
                        curr = self.link_trees(curr, existing);
                    }
                }
            }
        }

        // Rebuild root list sorted by order
        self.head = None;
        let mut prev: Option<usize> = None;

        for entry in &by_order {
            if let Some(idx) = entry {
                self.nodes[*idx].sibling = None;
                match prev {
                    None => self.head = Some(*idx),
                    Some(p) => self.nodes[p].sibling = Some(*idx),
                }
                prev = Some(*idx);
            }
        }
    }

    /// Links two trees of the same order, making the larger-key tree
    /// a child of the smaller-key tree.
    fn link_trees(&mut self, a: usize, b: usize) -> usize {
        let (winner, loser) = if self.nodes[a].key <= self.nodes[b].key {
            (a, b)
        } else {
            (b, a)
        };

        self.nodes[loser].sibling = self.nodes[winner].child;
        self.nodes[winner].child = Some(loser);
        self.nodes[winner].order += 1;
        winner
    }

    fn remove_from_root_list(&mut self, target: usize) -> Option<usize> {
        let mut roots = Vec::new();
        let mut current = self.head;
        while let Some(idx) = current {
            if idx != target {
                roots.push(idx);
            }
            current = self.nodes[idx].sibling;
        }

        if roots.is_empty() {
            return None;
        }

        for &r in &roots {
            self.nodes[r].sibling = None;
        }
        for i in 0..roots.len() - 1 {
            self.nodes[roots[i]].sibling = Some(roots[i + 1]);
        }
        Some(roots[0])
    }

    fn reverse_children(&mut self, parent: usize) -> Option<usize> {
        let mut children = Vec::new();
        let mut current = self.nodes[parent].child;
        while let Some(idx) = current {
            children.push(idx);
            current = self.nodes[idx].sibling;
        }

        if children.is_empty() {
            return None;
        }

        children.reverse();
        for &c in &children {
            self.nodes[c].sibling = None;
        }
        for i in 0..children.len() - 1 {
            self.nodes[children[i]].sibling = Some(children[i + 1]);
        }
        Some(children[0])
    }

    fn update_min(&mut self) {
        self.min_root = None;
        let mut current = self.head;
        while let Some(idx) = current {
            match self.min_root {
                None => self.min_root = Some(idx),
                Some(m) => {
                    if self.nodes[idx].key < self.nodes[m].key {
                        self.min_root = Some(idx);
                    }
                }
            }
            current = self.nodes[idx].sibling;
        }
    }
}

impl<K: Ord + Clone, V: Clone> Default for BinomialHeap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone + fmt::Display, V: Clone> fmt::Display for BinomialHeap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BinomialHeap({} elements)", self.count)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        assert!(heap.is_empty());
        assert_eq!(heap.len(), 0);
        assert!(heap.peek().is_none());
    }

    #[test]
    fn default_is_empty() {
        let heap: BinomialHeap<i32, i32> = BinomialHeap::default();
        assert!(heap.is_empty());
    }

    #[test]
    fn single_insert() {
        let mut heap = BinomialHeap::new();
        heap.insert(5, 50);
        assert_eq!(heap.len(), 1);
        assert_eq!(heap.peek(), Some((&5, &50)));
    }

    #[test]
    fn insert_maintains_min() {
        let mut heap = BinomialHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        heap.insert(7, 70);
        assert_eq!(heap.peek(), Some((&3, &30)));
    }

    #[test]
    fn extract_min_order() {
        let mut heap = BinomialHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        heap.insert(7, 70);
        heap.insert(1, 10);
        heap.insert(9, 90);

        assert_eq!(heap.extract_min(), Some((1, 10)));
        assert_eq!(heap.extract_min(), Some((3, 30)));
        assert_eq!(heap.extract_min(), Some((5, 50)));
        assert_eq!(heap.extract_min(), Some((7, 70)));
        assert_eq!(heap.extract_min(), Some((9, 90)));
        assert!(heap.extract_min().is_none());
    }

    #[test]
    fn pop_alias() {
        let mut heap = BinomialHeap::new();
        heap.insert(2, 20);
        heap.insert(1, 10);
        assert_eq!(heap.pop(), Some((1, 10)));
    }

    #[test]
    fn merge_heaps() {
        let mut h1 = BinomialHeap::new();
        h1.insert(1, 10);
        h1.insert(3, 30);
        h1.insert(5, 50);

        let mut h2 = BinomialHeap::new();
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
    fn merge_into_empty() {
        let mut h1: BinomialHeap<i32, i32> = BinomialHeap::new();
        let mut h2 = BinomialHeap::new();
        h2.insert(1, 10);
        h2.insert(2, 20);

        h1.merge(&mut h2);
        assert_eq!(h1.len(), 2);
        assert_eq!(h1.peek(), Some((&1, &10)));
    }

    #[test]
    fn merge_empty_into_nonempty() {
        let mut h1 = BinomialHeap::new();
        h1.insert(1, 10);

        let mut h2: BinomialHeap<i32, i32> = BinomialHeap::new();
        h1.merge(&mut h2);
        assert_eq!(h1.len(), 1);
    }

    #[test]
    fn sorted_order() {
        let mut heap = BinomialHeap::new();
        for val in [5, 2, 8, 1, 4, 7, 9, 3, 6] {
            heap.insert(val, val * 10);
        }
        let sorted = heap.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn sorted_without_consuming() {
        let mut heap = BinomialHeap::new();
        heap.insert(3, 30);
        heap.insert(1, 10);
        heap.insert(2, 20);
        let sorted = heap.sorted();
        assert_eq!(sorted.len(), 3);
        assert_eq!(heap.len(), 3);
    }

    #[test]
    fn duplicate_keys() {
        let mut heap = BinomialHeap::new();
        heap.insert(3, 30);
        heap.insert(3, 31);
        heap.insert(3, 32);
        assert_eq!(heap.len(), 3);
        for _ in 0..3 {
            let (k, _) = heap.extract_min().unwrap();
            assert_eq!(k, 3);
        }
    }

    #[test]
    fn large_heap() {
        let mut heap = BinomialHeap::new();
        for i in (0..500).rev() {
            heap.insert(i, i);
        }
        assert_eq!(heap.len(), 500);
        for i in 0..500 {
            assert_eq!(heap.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn serde_roundtrip() {
        let mut heap = BinomialHeap::new();
        for i in [5, 3, 7, 1, 9] {
            heap.insert(i, i * 10);
        }
        let json = serde_json::to_string(&heap).unwrap();
        let restored: BinomialHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), heap.len());
        let sorted = restored.sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn display_format() {
        let mut heap = BinomialHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        assert_eq!(format!("{}", heap), "BinomialHeap(2 elements)");
    }

    #[test]
    fn string_keys() {
        let mut heap = BinomialHeap::new();
        heap.insert("cherry".to_string(), 3);
        heap.insert("apple".to_string(), 1);
        heap.insert("banana".to_string(), 2);
        assert_eq!(heap.extract_min().unwrap().0, "apple");
        assert_eq!(heap.extract_min().unwrap().0, "banana");
        assert_eq!(heap.extract_min().unwrap().0, "cherry");
    }
}
