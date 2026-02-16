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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BinomialNode<K, V> {
    key: K,
    value: V,
    order: usize,
    child: Option<usize>,
    sibling: Option<usize>,
}

/// Min-binomial heap with arena allocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BinomialHeap<K, V> {
    nodes: Vec<BinomialNode<K, V>>,
    head: Option<usize>,
    count: usize,
    min_root: Option<usize>,
    free: Vec<usize>,
}

impl<K: Ord + Clone, V: Clone> BinomialHeap<K, V> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            head: None,
            count: 0,
            min_root: None,
            free: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

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

    fn collect_roots(&self) -> Vec<usize> {
        let mut roots = Vec::new();
        let mut cur = self.head;
        while let Some(idx) = cur {
            roots.push(idx);
            cur = self.nodes[idx].sibling;
        }
        roots
    }

    fn rebuild_root_list(&mut self, roots: &[usize]) {
        if roots.is_empty() {
            self.head = None;
            self.min_root = None;
            return;
        }
        self.head = Some(roots[0]);
        for i in 0..roots.len() {
            self.nodes[roots[i]].sibling = if i + 1 < roots.len() {
                Some(roots[i + 1])
            } else {
                None
            };
        }
        self.update_min();
    }

    fn consolidate_roots(&mut self, roots: Vec<usize>) {
        for &r in &roots {
            self.nodes[r].sibling = None;
        }
        let max_order = 64;
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
        let final_roots: Vec<usize> = by_order.into_iter().flatten().collect();
        self.rebuild_root_list(&final_roots);
    }

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

    fn update_min(&mut self) {
        self.min_root = None;
        let mut cur = self.head;
        while let Some(idx) = cur {
            match self.min_root {
                None => self.min_root = Some(idx),
                Some(m) => {
                    if self.nodes[idx].key < self.nodes[m].key {
                        self.min_root = Some(idx);
                    }
                }
            }
            cur = self.nodes[idx].sibling;
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        let idx = self.alloc_node(key, value);
        self.count += 1;
        let mut roots = self.collect_roots();
        roots.push(idx);
        self.consolidate_roots(roots);
    }

    pub fn extract_min(&mut self) -> Option<(K, V)> {
        let min_root = self.min_root?;
        let result = (
            self.nodes[min_root].key.clone(),
            self.nodes[min_root].value.clone(),
        );
        let mut roots: Vec<usize> = self
            .collect_roots()
            .into_iter()
            .filter(|&r| r != min_root)
            .collect();
        let mut child = self.nodes[min_root].child;
        while let Some(c) = child {
            let next = self.nodes[c].sibling;
            self.nodes[c].sibling = None;
            roots.push(c);
            child = next;
        }
        self.free.push(min_root);
        self.count -= 1;
        if self.count == 0 {
            self.head = None;
            self.min_root = None;
        } else {
            self.consolidate_roots(roots);
        }
        Some(result)
    }

    pub fn pop(&mut self) -> Option<(K, V)> {
        self.extract_min()
    }

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
        let mut roots = self.collect_roots();
        let mut other_cur = other.head;
        while let Some(idx) = other_cur {
            roots.push(idx + offset);
            other_cur = other.nodes[idx].sibling;
        }
        self.count += other.count;
        self.free.extend(other.free.iter().map(|&f| f + offset));
        self.consolidate_roots(roots);
        other.nodes.clear();
        other.head = None;
        other.count = 0;
        other.min_root = None;
        other.free.clear();
    }

    pub fn into_sorted(mut self) -> Vec<(K, V)> {
        let mut result = Vec::with_capacity(self.count);
        while let Some(item) = self.extract_min() {
            result.push(item);
        }
        result
    }

    pub fn sorted(&self) -> Vec<(K, V)> {
        self.clone().into_sorted()
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

    #[test]
    fn extract_min_from_empty() {
        let mut heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        assert_eq!(heap.extract_min(), None);
    }

    #[test]
    fn pop_from_empty() {
        let mut heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        assert_eq!(heap.pop(), None);
    }

    #[test]
    fn into_sorted_empty() {
        let heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        assert!(heap.into_sorted().is_empty());
    }

    #[test]
    fn sorted_single_element() {
        let mut heap = BinomialHeap::new();
        heap.insert(42, "answer");
        let s = heap.sorted();
        assert_eq!(s, vec![(42, "answer")]);
        // original heap unchanged
        assert_eq!(heap.len(), 1);
    }

    #[test]
    fn extract_single_then_empty() {
        let mut heap = BinomialHeap::new();
        heap.insert(10, 100);
        assert_eq!(heap.extract_min(), Some((10, 100)));
        assert!(heap.is_empty());
        assert_eq!(heap.len(), 0);
        assert!(heap.peek().is_none());
    }

    #[test]
    fn ascending_insertion() {
        let mut heap = BinomialHeap::new();
        for i in 0..20 {
            heap.insert(i, i);
        }
        for i in 0..20 {
            assert_eq!(heap.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn descending_insertion() {
        let mut heap = BinomialHeap::new();
        for i in (0..20).rev() {
            heap.insert(i, i);
        }
        for i in 0..20 {
            assert_eq!(heap.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn two_element_heap() {
        let mut heap = BinomialHeap::new();
        heap.insert(2, 20);
        heap.insert(1, 10);
        assert_eq!(heap.peek(), Some((&1, &10)));
        assert_eq!(heap.extract_min(), Some((1, 10)));
        assert_eq!(heap.peek(), Some((&2, &20)));
        assert_eq!(heap.extract_min(), Some((2, 20)));
        assert!(heap.is_empty());
    }

    #[test]
    fn negative_keys() {
        let mut heap = BinomialHeap::new();
        heap.insert(-3, 30);
        heap.insert(5, 50);
        heap.insert(-10, 100);
        heap.insert(0, 0);
        assert_eq!(heap.extract_min(), Some((-10, 100)));
        assert_eq!(heap.extract_min(), Some((-3, 30)));
        assert_eq!(heap.extract_min(), Some((0, 0)));
        assert_eq!(heap.extract_min(), Some((5, 50)));
    }

    #[test]
    fn interleaved_insert_extract() {
        let mut heap = BinomialHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        assert_eq!(heap.extract_min(), Some((3, 30)));
        heap.insert(1, 10);
        heap.insert(4, 40);
        assert_eq!(heap.extract_min(), Some((1, 10)));
        assert_eq!(heap.extract_min(), Some((4, 40)));
        assert_eq!(heap.extract_min(), Some((5, 50)));
        assert!(heap.is_empty());
    }

    #[test]
    fn peek_does_not_remove() {
        let mut heap = BinomialHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        assert_eq!(heap.peek(), Some((&1, &10)));
        assert_eq!(heap.peek(), Some((&1, &10)));
        assert_eq!(heap.len(), 2);
    }

    #[test]
    fn extract_min_updates_peek() {
        let mut heap = BinomialHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        heap.insert(3, 30);
        assert_eq!(heap.peek(), Some((&1, &10)));
        heap.extract_min();
        assert_eq!(heap.peek(), Some((&2, &20)));
        heap.extract_min();
        assert_eq!(heap.peek(), Some((&3, &30)));
    }

    #[test]
    fn power_of_two_sizes() {
        // Insert exactly 2^k elements — should produce a single binomial tree
        for &n in &[1, 2, 4, 8, 16] {
            let mut heap = BinomialHeap::new();
            for i in 0..n {
                heap.insert(i, i);
            }
            assert_eq!(heap.len(), n as usize);
            for i in 0..n {
                assert_eq!(heap.extract_min(), Some((i, i)));
            }
        }
    }

    #[test]
    fn extract_half_then_check_remaining() {
        let mut heap = BinomialHeap::new();
        for i in 0..10 {
            heap.insert(i, i * 10);
        }
        // extract first 5
        for i in 0..5 {
            assert_eq!(heap.extract_min(), Some((i, i * 10)));
        }
        assert_eq!(heap.len(), 5);
        // remaining 5 still sorted
        let sorted = heap.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|&(k, _)| k).collect();
        assert_eq!(keys, vec![5, 6, 7, 8, 9]);
    }

    #[test]
    fn free_list_reuse() {
        let mut heap = BinomialHeap::new();
        for i in 0..10 {
            heap.insert(i, i);
        }
        // extract all — fills free list
        for _ in 0..10 {
            heap.extract_min();
        }
        let arena_size = heap.nodes.len();
        // reinsert — should reuse arena slots
        for i in 100..110 {
            heap.insert(i, i);
        }
        // arena should not grow (slots reused from free list)
        assert_eq!(heap.nodes.len(), arena_size);
        // verify correctness
        for i in 100..110 {
            assert_eq!(heap.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn clone_independence() {
        let mut heap = BinomialHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        heap.insert(3, 30);
        let mut cloned = heap.clone();
        // modify original
        heap.extract_min();
        heap.insert(0, 0);
        // clone should be unaffected
        assert_eq!(cloned.len(), 3);
        assert_eq!(cloned.extract_min(), Some((1, 10)));
        assert_eq!(cloned.extract_min(), Some((2, 20)));
        assert_eq!(cloned.extract_min(), Some((3, 30)));
    }

    #[test]
    fn merge_two_large_heaps() {
        let mut h1 = BinomialHeap::new();
        let mut h2 = BinomialHeap::new();
        for i in (0..100).step_by(2) {
            h1.insert(i, i);
        }
        for i in (1..100).step_by(2) {
            h2.insert(i, i);
        }
        h1.merge(&mut h2);
        assert_eq!(h1.len(), 100);
        assert!(h2.is_empty());
        for i in 0..100 {
            assert_eq!(h1.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn merge_three_heaps_sequentially() {
        let mut h1 = BinomialHeap::new();
        let mut h2 = BinomialHeap::new();
        let mut h3 = BinomialHeap::new();
        h1.insert(3, 30);
        h2.insert(1, 10);
        h3.insert(2, 20);
        h1.merge(&mut h2);
        h1.merge(&mut h3);
        assert_eq!(h1.len(), 3);
        assert_eq!(h1.extract_min(), Some((1, 10)));
        assert_eq!(h1.extract_min(), Some((2, 20)));
        assert_eq!(h1.extract_min(), Some((3, 30)));
    }

    #[test]
    fn merge_after_partial_extraction() {
        let mut h1 = BinomialHeap::new();
        h1.insert(1, 10);
        h1.insert(3, 30);
        h1.insert(5, 50);
        h1.extract_min(); // remove 1
        let mut h2 = BinomialHeap::new();
        h2.insert(2, 20);
        h2.insert(4, 40);
        h1.merge(&mut h2);
        assert_eq!(h1.len(), 4);
        let sorted = h1.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|&(k, _)| k).collect();
        assert_eq!(keys, vec![2, 3, 4, 5]);
    }

    #[test]
    fn all_same_keys_extract_all() {
        let mut heap = BinomialHeap::new();
        for i in 0..10 {
            heap.insert(5, i);
        }
        assert_eq!(heap.len(), 10);
        let mut values = Vec::new();
        while let Some((k, v)) = heap.extract_min() {
            assert_eq!(k, 5);
            values.push(v);
        }
        // all 10 values extracted
        values.sort();
        assert_eq!(values, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn serde_roundtrip_after_extract() {
        let mut heap = BinomialHeap::new();
        for i in [5, 3, 7, 1, 9] {
            heap.insert(i, i * 10);
        }
        heap.extract_min(); // remove 1
        let json = serde_json::to_string(&heap).unwrap();
        let mut restored: BinomialHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 4);
        assert_eq!(restored.extract_min(), Some((3, 30)));
        assert_eq!(restored.extract_min(), Some((5, 50)));
        assert_eq!(restored.extract_min(), Some((7, 70)));
        assert_eq!(restored.extract_min(), Some((9, 90)));
    }

    #[test]
    fn serde_empty_roundtrip() {
        let heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        let json = serde_json::to_string(&heap).unwrap();
        let restored: BinomialHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn display_empty() {
        let heap: BinomialHeap<i32, i32> = BinomialHeap::new();
        assert_eq!(format!("{}", heap), "BinomialHeap(0 elements)");
    }

    #[test]
    fn insert_extract_insert_cycle() {
        let mut heap = BinomialHeap::new();
        // first batch
        for i in 0..5 {
            heap.insert(i, i);
        }
        // drain all
        while heap.extract_min().is_some() {}
        assert!(heap.is_empty());
        // second batch
        for i in 10..15 {
            heap.insert(i, i);
        }
        assert_eq!(heap.len(), 5);
        for i in 10..15 {
            assert_eq!(heap.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn len_consistency_through_operations() {
        let mut heap = BinomialHeap::new();
        for i in 0..20 {
            heap.insert(i, i);
            assert_eq!(heap.len(), i as usize + 1);
        }
        for i in (0..20).rev() {
            heap.extract_min();
            assert_eq!(heap.len(), i as usize);
        }
    }

    #[test]
    fn merge_both_empty() {
        let mut h1: BinomialHeap<i32, i32> = BinomialHeap::new();
        let mut h2: BinomialHeap<i32, i32> = BinomialHeap::new();
        h1.merge(&mut h2);
        assert!(h1.is_empty());
        assert!(h2.is_empty());
    }

    #[test]
    fn merge_reuses_donor_free_slots() {
        let mut receiver = BinomialHeap::new();
        receiver.insert(0, 0);

        let mut donor = BinomialHeap::new();
        for i in 1..=10 {
            donor.insert(i, i);
        }
        for i in 1..=5 {
            assert_eq!(donor.extract_min(), Some((i, i)));
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
        let mut receiver = BinomialHeap::new();
        receiver.insert(10, 10);

        let mut donor = BinomialHeap::new();
        donor.insert(1, 1);
        donor.insert(2, 2);

        receiver.merge(&mut donor);
        assert!(donor.is_empty());
        assert!(donor.nodes.is_empty());
        assert!(donor.free.is_empty());
        assert!(donor.head.is_none());
        assert!(donor.min_root.is_none());

        donor.insert(5, 50);
        donor.insert(3, 30);
        assert_eq!(donor.extract_min(), Some((3, 30)));
        assert_eq!(donor.extract_min(), Some((5, 50)));
    }

    #[test]
    fn tuple_keys() {
        let mut heap = BinomialHeap::new();
        heap.insert((1, 2), "a");
        heap.insert((1, 1), "b");
        heap.insert((0, 9), "c");
        assert_eq!(heap.extract_min(), Some(((0, 9), "c")));
        assert_eq!(heap.extract_min(), Some(((1, 1), "b")));
        assert_eq!(heap.extract_min(), Some(((1, 2), "a")));
    }
}
