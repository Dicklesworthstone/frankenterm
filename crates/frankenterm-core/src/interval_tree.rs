//! Interval tree — augmented BST for efficient interval overlap queries.
//!
//! An interval tree stores intervals `[low, high)` and supports efficient
//! queries for all intervals overlapping a point or range. Built on an
//! augmented binary search tree where each node tracks the maximum endpoint
//! in its subtree, enabling O(log n + k) overlap queries.
//!
//! # Design
//!
//! ```text
//!                    [10,20) max=30
//!                   /              \
//!           [5,15) max=15    [25,30) max=30
//!                           /
//!                   [18,28) max=28
//! ```
//!
//! Each node stores:
//! - An interval `[low, high)`
//! - The maximum `high` value in its entire subtree
//! - Left/right child indices (arena-allocated)
//!
//! # Use Cases in FrankenTerm
//!
//! - **Scrollback region tracking**: Find which pane regions are visible in a viewport.
//! - **Session time overlap**: Correlate concurrent agent sessions by time range.
//! - **Scheduling windows**: Find conflicting reservation periods for resources.
//! - **Memory region management**: Track allocated buffer ranges for deduplication.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Interval Type ──────────────────────────────────────────────────────

/// A half-open interval `[low, high)`.
///
/// The interval is empty if `low >= high`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval<T> {
    pub low: T,
    pub high: T,
}

impl<T: Ord + Clone> Interval<T> {
    /// Create a new interval `[low, high)`.
    pub fn new(low: T, high: T) -> Self {
        Self { low, high }
    }

    /// Check if this interval overlaps with another.
    /// Two intervals `[a, b)` and `[c, d)` overlap iff `a < d && c < b`.
    /// Empty intervals never overlap anything.
    pub fn overlaps(&self, other: &Self) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.low < other.high && other.low < self.high
    }

    /// Check if this interval contains a point.
    pub fn contains_point(&self, point: &T) -> bool {
        self.low <= *point && *point < self.high
    }

    /// Check if this interval is empty (low >= high).
    pub fn is_empty(&self) -> bool {
        self.low >= self.high
    }
}

impl<T: fmt::Display> fmt::Display for Interval<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.low, self.high)
    }
}

// ── Arena Node ─────────────────────────────────────────────────────────

/// Internal node in the interval tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node<T, V> {
    interval: Interval<T>,
    value: V,
    /// Maximum `high` endpoint in this node's subtree (including self).
    max_high: T,
    left: Option<usize>,
    right: Option<usize>,
    /// Height for AVL balancing.
    height: i32,
}

// ── Interval Tree ──────────────────────────────────────────────────────

/// An augmented, self-balancing interval tree.
///
/// Stores intervals with associated values and supports efficient overlap
/// and stabbing queries. Internally uses AVL balancing to maintain
/// O(log n) height.
///
/// # Type Parameters
/// - `T`: The interval endpoint type. Must be `Ord + Clone`.
/// - `V`: The value associated with each interval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntervalTree<T, V> {
    nodes: Vec<Node<T, V>>,
    root: Option<usize>,
    len: usize,
}

impl<T: Ord + Clone, V> Default for IntervalTree<T, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Ord + Clone, V> IntervalTree<T, V> {
    /// Create an empty interval tree.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            len: 0,
        }
    }

    /// Return the number of intervals in the tree.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Insert an interval with an associated value.
    pub fn insert(&mut self, interval: Interval<T>, value: V) {
        let idx = self.nodes.len();
        let max_high = interval.high.clone();
        self.nodes.push(Node {
            interval,
            value,
            max_high,
            left: None,
            right: None,
            height: 1,
        });
        self.root = Some(self.insert_at(self.root, idx));
        self.len += 1;
    }

    /// Find all intervals that overlap with the query interval.
    ///
    /// Returns references to `(interval, value)` pairs.
    /// Time complexity: O(log n + k) where k is the number of results.
    pub fn query_overlap(&self, query: &Interval<T>) -> Vec<(&Interval<T>, &V)> {
        let mut results = Vec::new();
        self.overlap_search(self.root, query, &mut results);
        results
    }

    /// Find all intervals that contain the given point.
    ///
    /// A stabbing query: returns all intervals `[low, high)` where `low <= point < high`.
    /// Time complexity: O(log n + k) where k is the number of results.
    pub fn query_point(&self, point: &T) -> Vec<(&Interval<T>, &V)> {
        let mut results = Vec::new();
        self.point_search(self.root, point, &mut results);
        results
    }

    /// Remove all intervals that exactly match the given interval.
    ///
    /// Returns the values of removed intervals.
    pub fn remove(&mut self, interval: &Interval<T>) -> Vec<V>
    where
        V: Clone,
    {
        let mut removed = Vec::new();
        // Collect indices of matching nodes
        let indices: Vec<usize> = self.find_exact(self.root, interval);
        // Remove in reverse order to keep indices stable during removal
        for &idx in indices.iter().rev() {
            if let Some(val) = self.remove_node(idx) {
                removed.push(val);
            }
        }
        removed
    }

    /// Iterate over all intervals in sorted order by low endpoint.
    pub fn iter(&self) -> IntervalTreeIter<'_, T, V> {
        let mut stack = Vec::new();
        let mut current = self.root;
        while let Some(idx) = current {
            stack.push(idx);
            current = self.nodes[idx].left;
        }
        IntervalTreeIter {
            tree: self,
            stack,
        }
    }

    /// Return the minimum low endpoint across all intervals, if any.
    pub fn min_low(&self) -> Option<&T> {
        let mut current = self.root?;
        while let Some(left) = self.nodes[current].left {
            current = left;
        }
        Some(&self.nodes[current].interval.low)
    }

    /// Return the maximum high endpoint across all intervals, if any.
    pub fn max_high(&self) -> Option<&T> {
        self.root.map(|r| &self.nodes[r].max_high)
    }

    // ── Internal: AVL insertion ────────────────────────────────────────

    fn insert_at(&mut self, node: Option<usize>, new_idx: usize) -> usize {
        let Some(idx) = node else {
            return new_idx;
        };

        if self.nodes[new_idx].interval.low <= self.nodes[idx].interval.low {
            let left = self.nodes[idx].left;
            let new_left = self.insert_at(left, new_idx);
            self.nodes[idx].left = Some(new_left);
        } else {
            let right = self.nodes[idx].right;
            let new_right = self.insert_at(right, new_idx);
            self.nodes[idx].right = Some(new_right);
        }

        self.update_augment(idx);
        self.balance(idx)
    }

    // ── Internal: Overlap search ───────────────────────────────────────

    fn overlap_search<'a>(
        &'a self,
        node: Option<usize>,
        query: &Interval<T>,
        results: &mut Vec<(&'a Interval<T>, &'a V)>,
    ) {
        let Some(idx) = node else { return };

        // If max_high in this subtree <= query.low, no overlaps possible
        if self.nodes[idx].max_high <= query.low {
            return;
        }

        // Search left subtree
        self.overlap_search(self.nodes[idx].left, query, results);

        // Check this node
        if self.nodes[idx].interval.overlaps(query) {
            results.push((&self.nodes[idx].interval, &self.nodes[idx].value));
        }

        // If this node's low >= query.high, no right subtree overlaps
        if self.nodes[idx].interval.low >= query.high {
            return;
        }

        // Search right subtree
        self.overlap_search(self.nodes[idx].right, query, results);
    }

    // ── Internal: Point (stabbing) search ──────────────────────────────

    fn point_search<'a>(
        &'a self,
        node: Option<usize>,
        point: &T,
        results: &mut Vec<(&'a Interval<T>, &'a V)>,
    ) {
        let Some(idx) = node else { return };

        // If max_high in subtree <= point, no intervals contain it
        if self.nodes[idx].max_high <= *point {
            return;
        }

        // Search left subtree
        self.point_search(self.nodes[idx].left, point, results);

        // Check this node
        if self.nodes[idx].interval.contains_point(point) {
            results.push((&self.nodes[idx].interval, &self.nodes[idx].value));
        }

        // If this node's low > point, right subtree can't contain point
        if self.nodes[idx].interval.low > *point {
            return;
        }

        // Search right subtree
        self.point_search(self.nodes[idx].right, point, results);
    }

    // ── Internal: Find exact matches ───────────────────────────────────

    fn find_exact(&self, node: Option<usize>, interval: &Interval<T>) -> Vec<usize> {
        let Some(idx) = node else {
            return Vec::new();
        };

        let mut results = Vec::new();

        // Search left if query.low <= node.low
        if interval.low <= self.nodes[idx].interval.low {
            results.extend(self.find_exact(self.nodes[idx].left, interval));
        }

        // Check this node
        if self.nodes[idx].interval.low == interval.low
            && self.nodes[idx].interval.high == interval.high
        {
            results.push(idx);
        }

        // Search right if query.low >= node.low
        if interval.low >= self.nodes[idx].interval.low {
            results.extend(self.find_exact(self.nodes[idx].right, interval));
        }

        results
    }

    // ── Internal: Remove by index ──────────────────────────────────────

    fn remove_node(&mut self, target: usize) -> Option<V>
    where
        V: Clone,
    {
        let value = self.nodes[target].value.clone();
        self.root = self.remove_at(self.root, target);
        self.len -= 1;
        Some(value)
    }

    fn remove_at(&mut self, node: Option<usize>, target: usize) -> Option<usize>
    where
        V: Clone,
    {
        let idx = node?;

        if idx == target {
            // Node to remove found
            match (self.nodes[idx].left, self.nodes[idx].right) {
                (None, None) => None,
                (Some(child), None) | (None, Some(child)) => Some(child),
                (Some(_left), Some(right)) => {
                    // Find in-order successor (leftmost in right subtree)
                    let succ = self.find_min(right);
                    // Remove successor from right subtree
                    let new_right = self.remove_at(Some(right), succ);
                    // Replace current node data with successor data
                    let succ_interval = self.nodes[succ].interval.clone();
                    let succ_value = self.nodes[succ].value.clone();
                    self.nodes[idx].interval = succ_interval;
                    self.nodes[idx].value = succ_value;
                    self.nodes[idx].right = new_right;
                    self.update_augment(idx);
                    Some(self.balance(idx))
                }
            }
        } else if target < self.nodes.len() && self.nodes[target].interval.low <= self.nodes[idx].interval.low {
            let left = self.nodes[idx].left;
            self.nodes[idx].left = self.remove_at(left, target);
            self.update_augment(idx);
            Some(self.balance(idx))
        } else {
            let right = self.nodes[idx].right;
            self.nodes[idx].right = self.remove_at(right, target);
            self.update_augment(idx);
            Some(self.balance(idx))
        }
    }

    fn find_min(&self, mut idx: usize) -> usize {
        while let Some(left) = self.nodes[idx].left {
            idx = left;
        }
        idx
    }

    // ── Internal: AVL balancing ────────────────────────────────────────

    fn node_height(&self, node: Option<usize>) -> i32 {
        node.map_or(0, |idx| self.nodes[idx].height)
    }

    fn balance_factor(&self, idx: usize) -> i32 {
        self.node_height(self.nodes[idx].left) - self.node_height(self.nodes[idx].right)
    }

    fn update_augment(&mut self, idx: usize) {
        let mut max = self.nodes[idx].interval.high.clone();
        if let Some(left) = self.nodes[idx].left {
            if self.nodes[left].max_high > max {
                max = self.nodes[left].max_high.clone();
            }
        }
        if let Some(right) = self.nodes[idx].right {
            if self.nodes[right].max_high > max {
                max = self.nodes[right].max_high.clone();
            }
        }
        self.nodes[idx].max_high = max;

        let lh = self.node_height(self.nodes[idx].left);
        let rh = self.node_height(self.nodes[idx].right);
        self.nodes[idx].height = 1 + lh.max(rh);
    }

    fn balance(&mut self, idx: usize) -> usize {
        let bf = self.balance_factor(idx);

        // Left-heavy
        if bf > 1 {
            if let Some(left) = self.nodes[idx].left {
                if self.balance_factor(left) < 0 {
                    // Left-Right case
                    let new_left = self.rotate_left(left);
                    self.nodes[idx].left = Some(new_left);
                }
            }
            return self.rotate_right(idx);
        }

        // Right-heavy
        if bf < -1 {
            if let Some(right) = self.nodes[idx].right {
                if self.balance_factor(right) > 0 {
                    // Right-Left case
                    let new_right = self.rotate_right(right);
                    self.nodes[idx].right = Some(new_right);
                }
            }
            return self.rotate_left(idx);
        }

        idx
    }

    fn rotate_left(&mut self, idx: usize) -> usize {
        let right = self.nodes[idx].right.expect("rotate_left requires right child");
        let right_left = self.nodes[right].left;

        self.nodes[right].left = Some(idx);
        self.nodes[idx].right = right_left;

        self.update_augment(idx);
        self.update_augment(right);

        right
    }

    fn rotate_right(&mut self, idx: usize) -> usize {
        let left = self.nodes[idx].left.expect("rotate_right requires left child");
        let left_right = self.nodes[left].right;

        self.nodes[left].right = Some(idx);
        self.nodes[idx].left = left_right;

        self.update_augment(idx);
        self.update_augment(left);

        left
    }

    // ── Internal: Validation ───────────────────────────────────────────

    /// Validate tree invariants (for testing).
    #[cfg(test)]
    fn validate(&self) -> bool {
        if let Some(root) = self.root {
            self.validate_node(root)
        } else {
            true
        }
    }

    #[cfg(test)]
    fn validate_node(&self, idx: usize) -> bool {
        let node = &self.nodes[idx];

        // Check max_high augmentation
        let mut expected_max = node.interval.high.clone();
        if let Some(left) = node.left {
            if !self.validate_node(left) {
                return false;
            }
            if self.nodes[left].max_high > expected_max {
                expected_max = self.nodes[left].max_high.clone();
            }
        }
        if let Some(right) = node.right {
            if !self.validate_node(right) {
                return false;
            }
            if self.nodes[right].max_high > expected_max {
                expected_max = self.nodes[right].max_high.clone();
            }
        }
        if node.max_high != expected_max {
            return false;
        }

        // Check AVL balance
        let bf = self.balance_factor(idx);
        if bf < -1 || bf > 1 {
            return false;
        }

        // Check height
        let lh = self.node_height(node.left);
        let rh = self.node_height(node.right);
        if node.height != 1 + lh.max(rh) {
            return false;
        }

        true
    }
}

// ── Iterator ───────────────────────────────────────────────────────────

/// In-order iterator over intervals in the tree.
pub struct IntervalTreeIter<'a, T, V> {
    tree: &'a IntervalTree<T, V>,
    stack: Vec<usize>,
}

impl<'a, T: Ord + Clone, V> Iterator for IntervalTreeIter<'a, T, V> {
    type Item = (&'a Interval<T>, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let idx = self.stack.pop()?;
        let node = &self.tree.nodes[idx];

        // Push left spine of right subtree
        let mut current = node.right;
        while let Some(c) = current {
            self.stack.push(c);
            current = self.tree.nodes[c].left;
        }

        Some((&node.interval, &node.value))
    }
}

// ── Convenience constructors ───────────────────────────────────────────

impl<T: Ord + Clone, V> FromIterator<(Interval<T>, V)> for IntervalTree<T, V> {
    fn from_iter<I: IntoIterator<Item = (Interval<T>, V)>>(iter: I) -> Self {
        let mut tree = Self::new();
        for (interval, value) in iter {
            tree.insert(interval, value);
        }
        tree
    }
}

// ── Statistics ─────────────────────────────────────────────────────────

impl<T: Ord + Clone, V> IntervalTree<T, V> {
    /// Return the height of the tree (0 for empty).
    pub fn height(&self) -> i32 {
        self.root.map_or(0, |r| self.nodes[r].height)
    }

    /// Collect all intervals sorted by low endpoint.
    pub fn intervals_sorted(&self) -> Vec<&Interval<T>> {
        self.iter().map(|(iv, _)| iv).collect()
    }
}

// ── Display ────────────────────────────────────────────────────────────

impl<T: Ord + Clone + fmt::Display, V> fmt::Display for IntervalTree<T, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IntervalTree({} intervals", self.len)?;
        if let Some(max) = self.max_high() {
            write!(f, ", max_high={}", max)?;
        }
        write!(f, ")")
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn iv(low: i32, high: i32) -> Interval<i32> {
        Interval::new(low, high)
    }

    // ── Interval basic tests ───────────────────────────────────────

    #[test]
    fn interval_overlap_positive() {
        assert!(iv(1, 5).overlaps(&iv(3, 7)));
        assert!(iv(3, 7).overlaps(&iv(1, 5)));
    }

    #[test]
    fn interval_overlap_touching() {
        // Half-open: [1,5) and [5,10) do NOT overlap
        assert!(!iv(1, 5).overlaps(&iv(5, 10)));
    }

    #[test]
    fn interval_overlap_contained() {
        assert!(iv(1, 10).overlaps(&iv(3, 7)));
        assert!(iv(3, 7).overlaps(&iv(1, 10)));
    }

    #[test]
    fn interval_no_overlap() {
        assert!(!iv(1, 3).overlaps(&iv(5, 7)));
    }

    #[test]
    fn interval_contains_point() {
        assert!(iv(1, 5).contains_point(&1));
        assert!(iv(1, 5).contains_point(&4));
        assert!(!iv(1, 5).contains_point(&5)); // half-open
        assert!(!iv(1, 5).contains_point(&0));
    }

    #[test]
    fn interval_is_empty() {
        assert!(iv(5, 5).is_empty());
        assert!(iv(5, 3).is_empty());
        assert!(!iv(1, 5).is_empty());
    }

    // ── Tree insertion and query tests ─────────────────────────────

    #[test]
    fn empty_tree() {
        let tree: IntervalTree<i32, &str> = IntervalTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.query_overlap(&iv(0, 10)).len(), 0);
        assert_eq!(tree.query_point(&5).len(), 0);
    }

    #[test]
    fn single_insert_and_query() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 10), "a");

        assert_eq!(tree.len(), 1);
        assert!(!tree.is_empty());

        let results = tree.query_overlap(&iv(5, 15));
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, "a");

        let results = tree.query_point(&5);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn multiple_inserts_overlap_query() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "a");
        tree.insert(iv(3, 8), "b");
        tree.insert(iv(10, 15), "c");
        tree.insert(iv(12, 20), "d");

        assert_eq!(tree.len(), 4);

        // Query overlapping [4, 13)
        let results = tree.query_overlap(&iv(4, 13));
        assert_eq!(results.len(), 3); // a=[1,5), b=[3,8), c=[10,15)

        // Query overlapping [20, 25) — should only get nothing (d is [12,20))
        let results = tree.query_overlap(&iv(20, 25));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn stabbing_query() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 10), "a");
        tree.insert(iv(5, 15), "b");
        tree.insert(iv(20, 30), "c");

        let at_7 = tree.query_point(&7);
        assert_eq!(at_7.len(), 2); // a and b

        let at_12 = tree.query_point(&12);
        assert_eq!(at_12.len(), 1); // just b

        let at_25 = tree.query_point(&25);
        assert_eq!(at_25.len(), 1); // just c

        let at_0 = tree.query_point(&0);
        assert_eq!(at_0.len(), 0);
    }

    #[test]
    fn max_high_tracking() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), ());
        assert_eq!(*tree.max_high().unwrap(), 5);

        tree.insert(iv(3, 20), ());
        assert_eq!(*tree.max_high().unwrap(), 20);

        tree.insert(iv(10, 15), ());
        assert_eq!(*tree.max_high().unwrap(), 20);
    }

    #[test]
    fn min_low_tracking() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(10, 20), ());
        assert_eq!(*tree.min_low().unwrap(), 10);

        tree.insert(iv(5, 15), ());
        assert_eq!(*tree.min_low().unwrap(), 5);

        tree.insert(iv(1, 3), ());
        assert_eq!(*tree.min_low().unwrap(), 1);
    }

    #[test]
    fn iterator_in_order() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(10, 20), "c");
        tree.insert(iv(1, 5), "a");
        tree.insert(iv(5, 15), "b");
        tree.insert(iv(20, 30), "d");

        let lows: Vec<i32> = tree.iter().map(|(iv, _)| iv.low).collect();
        // Should be sorted by low endpoint
        for w in lows.windows(2) {
            assert!(w[0] <= w[1]);
        }
    }

    #[test]
    fn from_iterator() {
        let tree: IntervalTree<i32, &str> = vec![
            (iv(1, 5), "a"),
            (iv(3, 8), "b"),
            (iv(10, 15), "c"),
        ]
        .into_iter()
        .collect();

        assert_eq!(tree.len(), 3);
        assert_eq!(tree.query_overlap(&iv(4, 9)).len(), 2);
    }

    #[test]
    fn duplicate_intervals() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "first");
        tree.insert(iv(1, 5), "second");

        assert_eq!(tree.len(), 2);
        let results = tree.query_overlap(&iv(2, 3));
        assert_eq!(results.len(), 2);
    }

    // ── Removal tests ──────────────────────────────────────────────

    #[test]
    fn remove_single() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "a".to_string());
        tree.insert(iv(3, 8), "b".to_string());
        tree.insert(iv(10, 15), "c".to_string());

        let removed = tree.remove(&iv(3, 8));
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], "b");
        assert_eq!(tree.len(), 2);

        let results = tree.query_overlap(&iv(4, 6));
        assert_eq!(results.len(), 1); // only "a"
    }

    #[test]
    fn remove_nonexistent() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "a".to_string());

        let removed = tree.remove(&iv(10, 20));
        assert!(removed.is_empty());
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn remove_duplicates() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "first".to_string());
        tree.insert(iv(1, 5), "second".to_string());
        tree.insert(iv(3, 8), "other".to_string());

        let removed = tree.remove(&iv(1, 5));
        assert_eq!(removed.len(), 2);
        assert_eq!(tree.len(), 1);
    }

    // ── AVL balance tests ──────────────────────────────────────────

    #[test]
    fn tree_stays_balanced_ascending() {
        let mut tree = IntervalTree::new();
        for i in 0..100 {
            tree.insert(iv(i, i + 10), i);
        }
        assert!(tree.validate());
        assert_eq!(tree.len(), 100);
        // AVL height should be O(log n)
        let h = IntervalTree::height(&tree);
        assert!(h <= 10); // log2(100) ~ 6.6, AVL guarantees <= 1.44 * log2(n+2)
    }

    #[test]
    fn tree_stays_balanced_descending() {
        let mut tree = IntervalTree::new();
        for i in (0..100).rev() {
            tree.insert(iv(i, i + 10), i);
        }
        assert!(tree.validate());
        let h = IntervalTree::height(&tree);
        assert!(h <= 10);
    }

    #[test]
    fn tree_stays_balanced_random_pattern() {
        let mut tree = IntervalTree::new();
        // Insert in a zigzag pattern to stress rotations
        let values = [50, 25, 75, 10, 40, 60, 90, 5, 15, 30, 45, 55, 65, 80, 95];
        for &v in &values {
            tree.insert(iv(v, v + 10), v);
        }
        assert!(tree.validate());
    }

    // ── Edge cases ─────────────────────────────────────────────────

    #[test]
    fn query_with_empty_interval() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 10), "a");

        // Empty query interval [5,5) should match nothing
        let results = tree.query_overlap(&iv(5, 5));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn adjacent_non_overlapping() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "a");
        tree.insert(iv(5, 10), "b");

        // [1,5) and [5,10) are adjacent but non-overlapping
        let at_5 = tree.query_point(&5);
        assert_eq!(at_5.len(), 1); // only b

        let at_4 = tree.query_point(&4);
        assert_eq!(at_4.len(), 1); // only a
    }

    #[test]
    fn large_overlapping_set() {
        let mut tree = IntervalTree::new();
        // Insert 50 overlapping intervals
        for i in 0..50 {
            tree.insert(iv(i, i + 100), i);
        }

        // Point 25 should be in all 50 intervals
        let results = tree.query_point(&25);
        assert_eq!(results.len(), 26); // intervals [0,100) through [25,125)
        // Wait, [0,100) through [25, 125) = 26 intervals contain 25
        // Actually: intervals i where i <= 25 && i+100 > 25, i.e., i <= 25
        // That's i=0..=25, which is 26 intervals
    }

    #[test]
    fn serde_roundtrip() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 5), "a".to_string());
        tree.insert(iv(3, 8), "b".to_string());
        tree.insert(iv(10, 15), "c".to_string());

        let json = serde_json::to_string(&tree).unwrap();
        let restored: IntervalTree<i32, String> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 3);
        let results = restored.query_overlap(&iv(4, 9));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn display_format() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(1, 10), ());
        tree.insert(iv(5, 20), ());
        let s = format!("{}", tree);
        assert!(s.contains("2 intervals"));
        assert!(s.contains("max_high=20"));
    }

    #[test]
    fn default_is_empty() {
        let tree: IntervalTree<i32, ()> = IntervalTree::default();
        assert!(tree.is_empty());
    }

    #[test]
    fn string_endpoints() {
        let mut tree = IntervalTree::new();
        tree.insert(
            Interval::new("aaa".to_string(), "mmm".to_string()),
            1,
        );
        tree.insert(
            Interval::new("ggg".to_string(), "zzz".to_string()),
            2,
        );

        let results = tree.query_point(&"hello".to_string());
        assert_eq!(results.len(), 2); // both intervals contain "hello"
    }

    #[test]
    fn intervals_sorted_method() {
        let mut tree = IntervalTree::new();
        tree.insert(iv(30, 40), ());
        tree.insert(iv(10, 20), ());
        tree.insert(iv(20, 30), ());

        let sorted = tree.intervals_sorted();
        assert_eq!(sorted.len(), 3);
        assert!(sorted[0].low <= sorted[1].low);
        assert!(sorted[1].low <= sorted[2].low);
    }
}
