//! Fibonacci heap — optimal amortized priority queue with decrease-key.
//!
//! A Fibonacci heap provides the best amortized bounds among comparison-based
//! priority queues, making it ideal for graph algorithms like Dijkstra's
//! shortest path and Prim's minimum spanning tree.
//!
//! # Complexity
//!
//! - **O(1) amortized**: insert, find-min, merge, decrease-key
//! - **O(log n) amortized**: extract-min, delete
//!
//! # Design
//!
//! Arena-allocated nodes with doubly-linked circular sibling lists and
//! parent/child pointers. Lazy consolidation on extract-min merges trees
//! of equal degree using a degree table.
//!
//! # Use in FrankenTerm
//!
//! Optimal scheduling of pane capture tasks where priority changes are
//! frequent (activity-based rescheduling), and shortest-path computations
//! in dependency graphs.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Node ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FibNode<K, V> {
    key: K,
    value: V,
    degree: usize,
    marked: bool,
    parent: Option<usize>,
    child: Option<usize>,
    prev: usize, // circular doubly-linked list
    next: usize,
}

// ── FibonacciHeap ─────────────────────────────────────────────────────

/// Min-Fibonacci heap with arena allocation.
///
/// Elements with the smallest key are at the top. Supports efficient
/// decrease-key operations for graph algorithm optimization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FibonacciHeap<K, V> {
    nodes: Vec<FibNode<K, V>>,
    min: Option<usize>,
    count: usize,
    free: Vec<usize>,
}

impl<K: Ord + Clone, V: Clone> FibonacciHeap<K, V> {
    /// Creates an empty Fibonacci heap.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            min: None,
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
        self.min.map(|m| (&self.nodes[m].key, &self.nodes[m].value))
    }

    fn alloc_node(&mut self, key: K, value: V) -> usize {
        let idx = if let Some(idx) = self.free.pop() {
            self.nodes[idx] = FibNode {
                key,
                value,
                degree: 0,
                marked: false,
                parent: None,
                child: None,
                prev: idx,
                next: idx,
            };
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(FibNode {
                key,
                value,
                degree: 0,
                marked: false,
                parent: None,
                child: None,
                prev: idx,
                next: idx,
            });
            idx
        };
        idx
    }

    /// Inserts a key-value pair. Returns a handle (node index) that can
    /// be used with `decrease_key`.
    pub fn insert(&mut self, key: K, value: V) -> usize {
        let idx = self.alloc_node(key, value);
        self.add_to_root_list(idx);
        self.count += 1;

        match self.min {
            None => self.min = Some(idx),
            Some(m) => {
                if self.nodes[idx].key < self.nodes[m].key {
                    self.min = Some(idx);
                }
            }
        }
        idx
    }

    /// Removes and returns the minimum element.
    pub fn extract_min(&mut self) -> Option<(K, V)> {
        let min_idx = self.min?;

        // Add all children of min to root list
        if let Some(child) = self.nodes[min_idx].child {
            let mut children = Vec::new();
            let mut curr = child;
            loop {
                children.push(curr);
                curr = self.nodes[curr].next;
                if curr == child {
                    break;
                }
            }
            for c in children {
                self.nodes[c].parent = None;
                self.add_to_root_list(c);
            }
        }

        // Remove min from root list
        let result = (
            self.nodes[min_idx].key.clone(),
            self.nodes[min_idx].value.clone(),
        );

        self.remove_from_list(min_idx);
        self.free.push(min_idx);
        self.count -= 1;

        if self.count == 0 {
            self.min = None;
        } else {
            // Set min to some root node, then consolidate
            // Find any remaining root node
            let next = self.nodes[min_idx].next;
            if next == min_idx {
                // min was the only root and had no children — shouldn't happen if count > 0
                // This means children were added above
                self.min = None;
                // Find a root node by scanning
                for i in 0..self.nodes.len() {
                    if !self.free.contains(&i) && self.nodes[i].parent.is_none() {
                        // Check it's a root (not a freed node)
                        self.min = Some(i);
                        break;
                    }
                }
            } else {
                self.min = Some(next);
            }

            if self.min.is_some() {
                self.consolidate();
            }
        }

        Some(result)
    }

    /// Alias for `extract_min` to match common heap interface.
    pub fn pop(&mut self) -> Option<(K, V)> {
        self.extract_min()
    }

    /// Decreases the key of the element at the given handle.
    ///
    /// # Panics
    ///
    /// Panics if the new key is greater than the current key.
    pub fn decrease_key(&mut self, handle: usize, new_key: K) {
        assert!(
            new_key <= self.nodes[handle].key,
            "new key must be less than or equal to current key"
        );
        self.nodes[handle].key = new_key;

        let parent = self.nodes[handle].parent;
        if let Some(p) = parent {
            if self.nodes[handle].key < self.nodes[p].key {
                self.cut(handle, p);
                self.cascading_cut(p);
            }
        }

        if let Some(m) = self.min {
            if self.nodes[handle].key < self.nodes[m].key {
                self.min = Some(handle);
            }
        }
    }

    /// Returns the current key for a handle.
    pub fn get_key(&self, handle: usize) -> Option<&K> {
        if self.free.contains(&handle) || handle >= self.nodes.len() {
            None
        } else {
            Some(&self.nodes[handle].key)
        }
    }

    /// Returns the current value for a handle.
    pub fn get_value(&self, handle: usize) -> Option<&V> {
        if self.free.contains(&handle) || handle >= self.nodes.len() {
            None
        } else {
            Some(&self.nodes[handle].value)
        }
    }

    /// Merges another heap into this one. The other heap is left empty.
    pub fn merge(&mut self, other: &mut FibonacciHeap<K, V>) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            std::mem::swap(self, other);
            return;
        }

        // Remap all node indices from other
        let offset = self.nodes.len();
        for node in &other.nodes {
            let mut copied = node.clone();
            copied.prev = copied.prev + offset;
            copied.next = copied.next + offset;
            copied.parent = copied.parent.map(|p| p + offset);
            copied.child = copied.child.map(|c| c + offset);
            self.nodes.push(copied);
        }

        // Merge root lists
        let other_min = other.min.unwrap() + offset;
        let self_min = self.min.unwrap();

        // Splice the two circular lists
        let self_next = self.nodes[self_min].next;
        let other_prev = self.nodes[other_min].prev;

        self.nodes[self_min].next = other_min;
        self.nodes[other_min].prev = self_min;
        self.nodes[other_prev].next = self_next;
        self.nodes[self_next].prev = other_prev;

        // Update min
        if self.nodes[other_min].key < self.nodes[self_min].key {
            self.min = Some(other_min);
        }

        self.count += other.count;
        self.free
            .extend(other.free.iter().map(|&f| f + offset));

        other.nodes.clear();
        other.min = None;
        other.count = 0;
        other.free.clear();
    }

    /// Returns all elements in sorted order by consuming the heap.
    pub fn into_sorted(mut self) -> Vec<(K, V)> {
        let mut result = Vec::with_capacity(self.count);
        while let Some(item) = self.extract_min() {
            result.push(item);
        }
        result
    }

    /// Returns all elements in sorted order without consuming.
    pub fn sorted(&self) -> Vec<(K, V)> {
        self.clone().into_sorted()
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Adds a node to the root list (as a singleton circular list entry
    /// spliced into the existing root list).
    fn add_to_root_list(&mut self, idx: usize) {
        match self.min {
            None => {
                self.nodes[idx].prev = idx;
                self.nodes[idx].next = idx;
            }
            Some(m) => {
                let m_next = self.nodes[m].next;
                self.nodes[idx].prev = m;
                self.nodes[idx].next = m_next;
                self.nodes[m].next = idx;
                self.nodes[m_next].prev = idx;
            }
        }
        self.nodes[idx].parent = None;
    }

    /// Removes a node from its doubly-linked circular list.
    fn remove_from_list(&mut self, idx: usize) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        self.nodes[prev].next = next;
        self.nodes[next].prev = prev;
        // Make it self-linked (safe default)
        self.nodes[idx].prev = idx;
        self.nodes[idx].next = idx;
    }

    /// Links y as a child of x (both must be roots).
    fn link(&mut self, child: usize, parent: usize) {
        self.remove_from_list(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[child].marked = false;

        match self.nodes[parent].child {
            None => {
                self.nodes[parent].child = Some(child);
                self.nodes[child].prev = child;
                self.nodes[child].next = child;
            }
            Some(c) => {
                let c_next = self.nodes[c].next;
                self.nodes[child].prev = c;
                self.nodes[child].next = c_next;
                self.nodes[c].next = child;
                self.nodes[c_next].prev = child;
            }
        }
        self.nodes[parent].degree += 1;
    }

    /// Consolidates the root list so no two roots have the same degree.
    fn consolidate(&mut self) {
        if self.min.is_none() {
            return;
        }

        // Max degree is O(log n), upper bound: log2(count) + 2
        let max_degree = if self.count <= 1 {
            2
        } else {
            (self.count as f64).log2() as usize + 3
        };
        let mut degree_table: Vec<Option<usize>> = vec![None; max_degree];

        // Collect all root nodes
        let mut roots = Vec::new();
        let start = self.min.unwrap();
        let mut curr = start;
        loop {
            roots.push(curr);
            curr = self.nodes[curr].next;
            if curr == start {
                break;
            }
        }

        for root in roots {
            let mut x = root;
            let mut d = self.nodes[x].degree;

            while d < degree_table.len() {
                match degree_table[d] {
                    None => break,
                    Some(y) => {
                        degree_table[d] = None;
                        let (winner, loser) = if self.nodes[x].key <= self.nodes[y].key {
                            (x, y)
                        } else {
                            (y, x)
                        };
                        self.link(loser, winner);
                        x = winner;
                        d = self.nodes[x].degree;
                    }
                }
            }
            if d >= degree_table.len() {
                degree_table.resize(d + 1, None);
            }
            degree_table[d] = Some(x);
        }

        // Rebuild root list and find new min
        self.min = None;
        for entry in degree_table {
            if let Some(idx) = entry {
                self.nodes[idx].parent = None;
                self.nodes[idx].prev = idx;
                self.nodes[idx].next = idx;

                match self.min {
                    None => {
                        self.min = Some(idx);
                    }
                    Some(m) => {
                        // Add to root list
                        let m_next = self.nodes[m].next;
                        self.nodes[idx].prev = m;
                        self.nodes[idx].next = m_next;
                        self.nodes[m].next = idx;
                        self.nodes[m_next].prev = idx;

                        if self.nodes[idx].key < self.nodes[m].key {
                            self.min = Some(idx);
                        }
                    }
                }
            }
        }
    }

    /// Cuts x from its parent y and adds x to the root list.
    fn cut(&mut self, x: usize, y: usize) {
        // Remove x from y's child list
        if self.nodes[x].next == x {
            // x is y's only child
            self.nodes[y].child = None;
        } else {
            let next = self.nodes[x].next;
            let prev = self.nodes[x].prev;
            self.nodes[prev].next = next;
            self.nodes[next].prev = prev;
            if self.nodes[y].child == Some(x) {
                self.nodes[y].child = Some(next);
            }
        }
        self.nodes[y].degree -= 1;

        // Add x to root list
        self.nodes[x].parent = None;
        self.nodes[x].marked = false;
        self.add_to_root_list(x);
    }

    /// Cascading cut: if y is marked, cut it from its parent too.
    fn cascading_cut(&mut self, y: usize) {
        if let Some(parent) = self.nodes[y].parent {
            if self.nodes[y].marked {
                self.cut(y, parent);
                self.cascading_cut(parent);
            } else {
                self.nodes[y].marked = true;
            }
        }
    }
}

impl<K: Ord + Clone, V: Clone> Default for FibonacciHeap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone + fmt::Display, V: Clone> fmt::Display for FibonacciHeap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FibonacciHeap({} elements)", self.count)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let heap: FibonacciHeap<i32, i32> = FibonacciHeap::new();
        assert!(heap.is_empty());
        assert_eq!(heap.len(), 0);
        assert!(heap.peek().is_none());
    }

    #[test]
    fn default_is_empty() {
        let heap: FibonacciHeap<i32, i32> = FibonacciHeap::default();
        assert!(heap.is_empty());
    }

    #[test]
    fn single_insert() {
        let mut heap = FibonacciHeap::new();
        let h = heap.insert(5, 50);
        assert_eq!(heap.len(), 1);
        assert_eq!(heap.peek(), Some((&5, &50)));
        assert_eq!(heap.get_key(h), Some(&5));
        assert_eq!(heap.get_value(h), Some(&50));
    }

    #[test]
    fn insert_maintains_min() {
        let mut heap = FibonacciHeap::new();
        heap.insert(5, 50);
        heap.insert(3, 30);
        heap.insert(7, 70);
        assert_eq!(heap.peek(), Some((&3, &30)));
    }

    #[test]
    fn extract_min_order() {
        let mut heap = FibonacciHeap::new();
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
        let mut heap = FibonacciHeap::new();
        heap.insert(2, 20);
        heap.insert(1, 10);
        assert_eq!(heap.pop(), Some((1, 10)));
    }

    #[test]
    fn decrease_key_basic() {
        let mut heap = FibonacciHeap::new();
        heap.insert(5, 50);
        let h = heap.insert(10, 100);
        heap.insert(3, 30);

        // Extract min to trigger consolidation
        assert_eq!(heap.extract_min(), Some((3, 30)));

        // Decrease 10 -> 1
        heap.decrease_key(h, 1);
        assert_eq!(heap.peek(), Some((&1, &100)));
        assert_eq!(heap.extract_min(), Some((1, 100)));
        assert_eq!(heap.extract_min(), Some((5, 50)));
    }

    #[test]
    fn decrease_key_cascading() {
        let mut heap = FibonacciHeap::new();
        // Insert many to build deep structure
        let handles: Vec<usize> = (0..10).map(|i| heap.insert(i * 10, i)).collect();

        // Extract min to consolidate
        heap.extract_min(); // removes 0

        // Decrease several keys to trigger cascading cuts
        heap.decrease_key(handles[9], 5); // 90 -> 5
        heap.decrease_key(handles[8], 4); // 80 -> 4
        heap.decrease_key(handles[7], 3); // 70 -> 3

        assert_eq!(heap.extract_min(), Some((3, 7)));
        assert_eq!(heap.extract_min(), Some((4, 8)));
        assert_eq!(heap.extract_min(), Some((5, 9)));
    }

    #[test]
    #[should_panic(expected = "new key must be less than or equal")]
    fn decrease_key_panics_on_increase() {
        let mut heap = FibonacciHeap::new();
        let h = heap.insert(5, 50);
        heap.decrease_key(h, 10);
    }

    #[test]
    fn merge_heaps() {
        let mut h1 = FibonacciHeap::new();
        h1.insert(1, 10);
        h1.insert(3, 30);
        h1.insert(5, 50);

        let mut h2 = FibonacciHeap::new();
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
        let mut h1: FibonacciHeap<i32, i32> = FibonacciHeap::new();
        let mut h2 = FibonacciHeap::new();
        h2.insert(1, 10);
        h2.insert(2, 20);

        h1.merge(&mut h2);
        assert_eq!(h1.len(), 2);
        assert_eq!(h1.peek(), Some((&1, &10)));
    }

    #[test]
    fn merge_empty_into_nonempty() {
        let mut h1 = FibonacciHeap::new();
        h1.insert(1, 10);

        let mut h2: FibonacciHeap<i32, i32> = FibonacciHeap::new();
        h1.merge(&mut h2);
        assert_eq!(h1.len(), 1);
    }

    #[test]
    fn sorted_order() {
        let mut heap = FibonacciHeap::new();
        for val in [5, 2, 8, 1, 4, 7, 9, 3, 6] {
            heap.insert(val, val * 10);
        }
        let sorted = heap.into_sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn sorted_without_consuming() {
        let mut heap = FibonacciHeap::new();
        heap.insert(3, 30);
        heap.insert(1, 10);
        heap.insert(2, 20);
        let sorted = heap.sorted();
        assert_eq!(sorted.len(), 3);
        assert_eq!(heap.len(), 3);
    }

    #[test]
    fn duplicate_keys() {
        let mut heap = FibonacciHeap::new();
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
        let mut heap = FibonacciHeap::new();
        for i in (0..500).rev() {
            heap.insert(i, i);
        }
        assert_eq!(heap.len(), 500);
        for i in 0..500 {
            assert_eq!(heap.extract_min(), Some((i, i)));
        }
    }

    #[test]
    fn get_key_value_invalid() {
        let heap: FibonacciHeap<i32, i32> = FibonacciHeap::new();
        assert!(heap.get_key(0).is_none());
        assert!(heap.get_value(999).is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let mut heap = FibonacciHeap::new();
        for i in [5, 3, 7, 1, 9] {
            heap.insert(i, i * 10);
        }
        let json = serde_json::to_string(&heap).unwrap();
        let restored: FibonacciHeap<i32, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), heap.len());
        let sorted = restored.sorted();
        let keys: Vec<i32> = sorted.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![1, 3, 5, 7, 9]);
    }

    #[test]
    fn display_format() {
        let mut heap = FibonacciHeap::new();
        heap.insert(1, 10);
        heap.insert(2, 20);
        assert_eq!(format!("{}", heap), "FibonacciHeap(2 elements)");
    }

    #[test]
    fn string_keys() {
        let mut heap = FibonacciHeap::new();
        heap.insert("cherry".to_string(), 3);
        heap.insert("apple".to_string(), 1);
        heap.insert("banana".to_string(), 2);
        assert_eq!(heap.extract_min().unwrap().0, "apple");
        assert_eq!(heap.extract_min().unwrap().0, "banana");
        assert_eq!(heap.extract_min().unwrap().0, "cherry");
    }
}
