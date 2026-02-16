//! Segment Tree with lazy propagation for O(log n) range queries and updates.
//!
//! Supports range sum queries and range additive updates with lazy propagation,
//! achieving O(log n) per operation. The tree is built in O(n) from an initial
//! array of values.
//!
//! # Use Cases
//!
//! - Range sum/min/max queries on telemetry time series
//! - Batch metric adjustments via range updates
//! - Interval occupancy tracking and scheduling
//! - Complements Fenwick Tree (point update only) with range update capability
//!
//! # Complexity
//!
//! | Operation       | Time     | Space |
//! |----------------|----------|-------|
//! | `build`        | O(n)     | O(n)  |
//! | `query`        | O(log n) | O(1)  |
//! | `point_update` | O(log n) | O(1)  |
//! | `range_update` | O(log n) | O(1)  |
//!
//! Bead: ft-283h4.28

use serde::{Deserialize, Serialize};

/// Configuration for a Segment Tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentTreeConfig {
    /// Number of elements.
    pub capacity: usize,
}

impl Default for SegmentTreeConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

/// Statistics about a Segment Tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentTreeStats {
    /// Number of logical elements.
    pub element_count: usize,
    /// Total number of nodes in the internal tree.
    pub node_count: usize,
    /// Number of query operations performed.
    pub query_count: u64,
    /// Number of update operations performed.
    pub update_count: u64,
    /// Approximate memory usage in bytes.
    pub memory_bytes: usize,
}

/// Segment Tree with lazy propagation for range sum queries and range updates.
///
/// Internal representation uses a 1-indexed implicit binary tree stored in
/// a flat array. Lazy values are propagated on demand during queries and updates.
///
/// # Example
///
/// ```
/// use frankenterm_core::segment_tree::SegmentTree;
///
/// let mut st = SegmentTree::from_slice(&[1, 3, 5, 7, 9]);
/// assert_eq!(st.query(0, 4), 25);  // sum of all
/// assert_eq!(st.query(1, 3), 15);  // 3 + 5 + 7
///
/// st.range_update(1, 3, 10);       // add 10 to indices 1..=3
/// assert_eq!(st.query(1, 3), 45);  // 13 + 15 + 17
/// ```
#[derive(Debug, Clone)]
pub struct SegmentTree {
    /// Number of logical elements.
    n: usize,
    /// Internal node sums (1-indexed, size 4*n for safety).
    tree: Vec<i64>,
    /// Lazy propagation buffer.
    lazy: Vec<i64>,
    /// Query operation counter.
    query_ops: u64,
    /// Update operation counter.
    update_ops: u64,
}

impl SegmentTree {
    /// Create a new Segment Tree with `n` elements, all initialized to 0.
    #[must_use]
    pub fn new(n: usize) -> Self {
        let size = if n == 0 { 1 } else { 4 * n };
        Self {
            n,
            tree: vec![0i64; size],
            lazy: vec![0i64; size],
            query_ops: 0,
            update_ops: 0,
        }
    }

    /// Build a Segment Tree from a slice of initial values.
    #[must_use]
    pub fn from_slice(values: &[i64]) -> Self {
        let n = values.len();
        let size = if n == 0 { 1 } else { 4 * n };
        let mut st = Self {
            n,
            tree: vec![0i64; size],
            lazy: vec![0i64; size],
            query_ops: 0,
            update_ops: 0,
        };
        if n > 0 {
            st.build(values, 1, 0, n - 1);
        }
        st
    }

    /// Create from config.
    #[must_use]
    pub fn from_config(config: &SegmentTreeConfig) -> Self {
        Self::new(config.capacity)
    }

    /// Number of logical elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether the tree is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Query the sum of elements in range `[left..=right]`.
    ///
    /// # Panics
    ///
    /// Panics if `left > right` or `right >= len()`.
    pub fn query(&mut self, left: usize, right: usize) -> i64 {
        assert!(left <= right, "left {left} > right {right}");
        assert!(right < self.n, "right {right} out of bounds for len {}", self.n);
        self.query_ops += 1;
        self.query_impl(1, 0, self.n - 1, left, right)
    }

    /// Update a single element: set `values[index] += delta`.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len()`.
    pub fn point_update(&mut self, index: usize, delta: i64) {
        assert!(index < self.n, "index {index} out of bounds for len {}", self.n);
        self.update_ops += 1;
        self.range_update_impl(1, 0, self.n - 1, index, index, delta);
    }

    /// Set a single element to a specific value.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len()`.
    pub fn point_set(&mut self, index: usize, value: i64) {
        let current = self.query(index, index);
        let delta = value.wrapping_sub(current);
        if delta != 0 {
            self.point_update(index, delta);
        }
    }

    /// Add `delta` to all elements in range `[left..=right]`.
    ///
    /// Uses lazy propagation for O(log n) performance.
    ///
    /// # Panics
    ///
    /// Panics if `left > right` or `right >= len()`.
    pub fn range_update(&mut self, left: usize, right: usize, delta: i64) {
        assert!(left <= right, "left {left} > right {right}");
        assert!(right < self.n, "right {right} out of bounds for len {}", self.n);
        self.update_ops += 1;
        self.range_update_impl(1, 0, self.n - 1, left, right, delta);
    }

    /// Get all element values as a Vec.
    ///
    /// This is O(n log n) as it queries each element individually.
    pub fn to_vec(&mut self) -> Vec<i64> {
        (0..self.n).map(|i| self.query(i, i)).collect()
    }

    /// Total sum of all elements.
    pub fn total_sum(&mut self) -> i64 {
        if self.n == 0 {
            0
        } else {
            self.query(0, self.n - 1)
        }
    }

    /// Reset all elements to zero.
    pub fn reset(&mut self) {
        self.tree.fill(0);
        self.lazy.fill(0);
    }

    /// Get statistics.
    pub fn stats(&mut self) -> SegmentTreeStats {
        SegmentTreeStats {
            element_count: self.n,
            node_count: self.tree.len(),
            query_count: self.query_ops,
            update_count: self.update_ops,
            memory_bytes: self.memory_bytes(),
        }
    }

    /// Approximate memory usage in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.tree.len() * std::mem::size_of::<i64>()
            + self.lazy.len() * std::mem::size_of::<i64>()
    }

    // ── Internal implementation ─────────────────────────────────────

    fn build(&mut self, values: &[i64], node: usize, start: usize, end: usize) {
        if start == end {
            self.tree[node] = values[start];
            return;
        }
        let mid = start + (end - start) / 2;
        let left_child = 2 * node;
        let right_child = 2 * node + 1;
        self.build(values, left_child, start, mid);
        self.build(values, right_child, mid + 1, end);
        self.tree[node] = self.tree[left_child].wrapping_add(self.tree[right_child]);
    }

    fn push_down(&mut self, node: usize, start: usize, end: usize) {
        if self.lazy[node] != 0 {
            let mid = start + (end - start) / 2;
            let left_child = 2 * node;
            let right_child = 2 * node + 1;
            let left_len = (mid - start + 1) as i64;
            let right_len = (end - mid) as i64;

            self.tree[left_child] = self.tree[left_child]
                .wrapping_add(self.lazy[node].wrapping_mul(left_len));
            self.tree[right_child] = self.tree[right_child]
                .wrapping_add(self.lazy[node].wrapping_mul(right_len));

            self.lazy[left_child] = self.lazy[left_child].wrapping_add(self.lazy[node]);
            self.lazy[right_child] = self.lazy[right_child].wrapping_add(self.lazy[node]);

            self.lazy[node] = 0;
        }
    }

    fn query_impl(
        &mut self,
        node: usize,
        start: usize,
        end: usize,
        left: usize,
        right: usize,
    ) -> i64 {
        if left > end || right < start {
            return 0;
        }
        if left <= start && end <= right {
            return self.tree[node];
        }
        self.push_down(node, start, end);
        let mid = start + (end - start) / 2;
        let left_sum = self.query_impl(2 * node, start, mid, left, right);
        let right_sum = self.query_impl(2 * node + 1, mid + 1, end, left, right);
        left_sum.wrapping_add(right_sum)
    }

    fn range_update_impl(
        &mut self,
        node: usize,
        start: usize,
        end: usize,
        left: usize,
        right: usize,
        delta: i64,
    ) {
        if left > end || right < start {
            return;
        }
        if left <= start && end <= right {
            let len = (end - start + 1) as i64;
            self.tree[node] = self.tree[node].wrapping_add(delta.wrapping_mul(len));
            self.lazy[node] = self.lazy[node].wrapping_add(delta);
            return;
        }
        self.push_down(node, start, end);
        let mid = start + (end - start) / 2;
        self.range_update_impl(2 * node, start, mid, left, right, delta);
        self.range_update_impl(2 * node + 1, mid + 1, end, left, right, delta);
        self.tree[node] = self.tree[2 * node].wrapping_add(self.tree[2 * node + 1]);
    }
}

impl Default for SegmentTree {
    fn default() -> Self {
        Self::new(SegmentTreeConfig::default().capacity)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_tree() {
        let st = SegmentTree::new(0);
        assert!(st.is_empty());
        assert_eq!(st.len(), 0);
    }

    #[test]
    fn test_single_element() {
        let mut st = SegmentTree::from_slice(&[42]);
        assert_eq!(st.query(0, 0), 42);
        assert_eq!(st.len(), 1);
    }

    #[test]
    fn test_basic_query() {
        let mut st = SegmentTree::from_slice(&[1, 3, 5, 7, 9]);
        assert_eq!(st.query(0, 4), 25);
        assert_eq!(st.query(0, 0), 1);
        assert_eq!(st.query(1, 3), 15);
        assert_eq!(st.query(2, 2), 5);
        assert_eq!(st.query(3, 4), 16);
    }

    #[test]
    fn test_point_update() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3, 4, 5]);
        st.point_update(2, 10); // [1, 2, 13, 4, 5]
        assert_eq!(st.query(0, 4), 25);
        assert_eq!(st.query(2, 2), 13);
        assert_eq!(st.query(0, 2), 16);
    }

    #[test]
    fn test_point_set() {
        let mut st = SegmentTree::from_slice(&[10, 20, 30]);
        st.point_set(1, 5);
        assert_eq!(st.query(1, 1), 5);
        assert_eq!(st.query(0, 2), 45);
    }

    #[test]
    fn test_range_update() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3, 4, 5]);
        st.range_update(1, 3, 10); // [1, 12, 13, 14, 5]
        assert_eq!(st.query(0, 4), 45);
        assert_eq!(st.query(1, 3), 39);
        assert_eq!(st.query(0, 0), 1);
        assert_eq!(st.query(4, 4), 5);
    }

    #[test]
    fn test_range_update_full() {
        let mut st = SegmentTree::from_slice(&[0, 0, 0, 0]);
        st.range_update(0, 3, 5);
        assert_eq!(st.query(0, 3), 20);
        for i in 0..4 {
            assert_eq!(st.query(i, i), 5);
        }
    }

    #[test]
    fn test_multiple_range_updates() {
        let mut st = SegmentTree::from_slice(&[0, 0, 0, 0, 0]);
        st.range_update(0, 2, 3);  // [3, 3, 3, 0, 0]
        st.range_update(2, 4, 5);  // [3, 3, 8, 5, 5]
        assert_eq!(st.query(0, 4), 24);
        assert_eq!(st.query(2, 2), 8);
    }

    #[test]
    fn test_negative_values() {
        let mut st = SegmentTree::from_slice(&[10, -5, 3, -2, 8]);
        assert_eq!(st.query(0, 4), 14);
        assert_eq!(st.query(1, 1), -5);
        st.range_update(0, 4, -1); // [9, -6, 2, -3, 7]
        assert_eq!(st.query(0, 4), 9);
    }

    #[test]
    fn test_from_new_then_update() {
        let mut st = SegmentTree::new(5);
        st.point_update(0, 10);
        st.point_update(2, 20);
        st.point_update(4, 30);
        assert_eq!(st.query(0, 4), 60);
        assert_eq!(st.query(1, 3), 20);
    }

    #[test]
    fn test_to_vec() {
        let values = [5, 3, 8, 1, 6];
        let mut st = SegmentTree::from_slice(&values);
        assert_eq!(st.to_vec(), values.to_vec());
    }

    #[test]
    fn test_to_vec_after_range_update() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3, 4, 5]);
        st.range_update(1, 3, 10);
        assert_eq!(st.to_vec(), vec![1, 12, 13, 14, 5]);
    }

    #[test]
    fn test_total_sum() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(st.total_sum(), 15);
    }

    #[test]
    fn test_total_sum_empty() {
        let mut st = SegmentTree::new(0);
        assert_eq!(st.total_sum(), 0);
    }

    #[test]
    fn test_reset() {
        let mut st = SegmentTree::from_slice(&[10, 20, 30]);
        st.range_update(0, 2, 5);
        st.reset();
        assert_eq!(st.total_sum(), 0);
        assert_eq!(st.len(), 3);
    }

    #[test]
    fn test_clone_independence() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3]);
        let original_sum = st.total_sum();
        let mut clone = st.clone();
        clone.range_update(0, 2, 100);
        assert_eq!(st.total_sum(), original_sum);
    }

    #[test]
    fn test_stats() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3]);
        st.query(0, 2);
        st.point_update(1, 5);
        let stats = st.stats();
        assert_eq!(stats.element_count, 3);
        assert_eq!(stats.query_count, 1);
        assert_eq!(stats.update_count, 1);
    }

    #[test]
    fn test_config_serde() {
        let config = SegmentTreeConfig { capacity: 42 };
        let json = serde_json::to_string(&config).unwrap();
        let back: SegmentTreeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn test_stats_serde() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3]);
        let stats = st.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: SegmentTreeStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn test_from_config() {
        let config = SegmentTreeConfig { capacity: 8 };
        let st = SegmentTree::from_config(&config);
        assert_eq!(st.len(), 8);
    }

    #[test]
    fn test_default() {
        let st = SegmentTree::default();
        assert_eq!(st.len(), 64);
    }

    #[test]
    fn test_memory_bytes_scales() {
        let st1 = SegmentTree::new(10);
        let st2 = SegmentTree::new(100);
        assert!(st2.memory_bytes() > st1.memory_bytes());
    }

    #[test]
    fn test_large_tree() {
        let n = 1000;
        let values: Vec<i64> = (1..=n as i64).collect();
        let mut st = SegmentTree::from_slice(&values);
        let expected = (n as i64) * (n as i64 + 1) / 2;
        assert_eq!(st.total_sum(), expected);
    }

    #[test]
    fn test_alternating_updates_and_queries() {
        let mut st = SegmentTree::from_slice(&[0, 0, 0, 0]);
        st.range_update(0, 1, 5);
        assert_eq!(st.query(0, 3), 10);
        st.range_update(2, 3, 3);
        assert_eq!(st.query(0, 3), 16);
        st.point_update(1, -2);
        assert_eq!(st.query(0, 3), 14);
    }

    #[test]
    fn test_power_of_two_size() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(st.query(0, 7), 36);
        assert_eq!(st.query(0, 3), 10);
        assert_eq!(st.query(4, 7), 26);
    }

    #[test]
    fn test_two_elements() {
        let mut st = SegmentTree::from_slice(&[10, 20]);
        assert_eq!(st.query(0, 1), 30);
        assert_eq!(st.query(0, 0), 10);
        assert_eq!(st.query(1, 1), 20);
        st.range_update(0, 1, 5);
        assert_eq!(st.query(0, 1), 40);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_query_out_of_bounds() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3]);
        st.query(0, 3);
    }

    #[test]
    #[should_panic(expected = "left")]
    fn test_query_invalid_range() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3]);
        st.query(2, 1);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_point_update_out_of_bounds() {
        let mut st = SegmentTree::new(3);
        st.point_update(3, 1);
    }

    #[test]
    fn stats_counts_operations() {
        let mut st = SegmentTree::from_slice(&[1, 2, 3, 4]);
        st.query(0, 3);
        st.query(1, 2);
        st.point_update(0, 5);
        let stats = st.stats();
        assert_eq!(stats.query_count, 2);
        assert_eq!(stats.update_count, 1);
        assert_eq!(stats.element_count, 4);
    }

    #[test]
    fn config_default() {
        let config = SegmentTreeConfig::default();
        assert_eq!(config.capacity, 64);
    }
}
