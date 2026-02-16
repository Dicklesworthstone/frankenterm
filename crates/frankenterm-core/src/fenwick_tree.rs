//! Binary Indexed Tree (Fenwick Tree) for O(log n) prefix sum queries.
//!
//! Provides efficient point updates and prefix sum queries over a sequence of
//! values. All operations run in O(log n) time with O(n) space.
//!
//! # Use Cases
//!
//! - Cumulative output byte tracking across pane timelines
//! - Range frequency queries for monitoring dashboards
//! - Rank queries for quantile estimation in telemetry
//! - Histogram bucket aggregation in metric pipelines
//!
//! # Complexity
//!
//! | Operation      | Time     | Space |
//! |---------------|----------|-------|
//! | `update`      | O(log n) | O(1)  |
//! | `prefix_sum`  | O(log n) | O(1)  |
//! | `range_sum`   | O(log n) | O(1)  |
//! | `find_kth`    | O(log²n) | O(1)  |
//! | `new`         | O(n)     | O(n)  |
//!
//! Bead: ft-283h4.26

use serde::{Deserialize, Serialize};

/// Configuration for a Fenwick Tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenwickConfig {
    /// Number of elements in the tree.
    pub capacity: usize,
}

impl Default for FenwickConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

/// Statistics about a Fenwick Tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenwickStats {
    /// Number of elements.
    pub element_count: usize,
    /// Total sum of all elements.
    pub total_sum: i64,
    /// Number of update operations performed.
    pub update_count: u64,
    /// Number of query operations performed.
    pub query_count: u64,
    /// Approximate memory usage in bytes.
    pub memory_bytes: usize,
}

/// Binary Indexed Tree (Fenwick Tree) over `i64` values.
///
/// Internally uses 1-based indexing. The public API uses 0-based indexing
/// for consistency with Rust conventions.
///
/// # Example
///
/// ```
/// use frankenterm_core::fenwick_tree::FenwickTree;
///
/// let mut ft = FenwickTree::new(5);
/// ft.update(0, 3);  // [3, 0, 0, 0, 0]
/// ft.update(1, 7);  // [3, 7, 0, 0, 0]
/// ft.update(3, 2);  // [3, 7, 0, 2, 0]
///
/// assert_eq!(ft.prefix_sum(0), 3);
/// assert_eq!(ft.prefix_sum(1), 10);
/// assert_eq!(ft.range_sum(1, 3), 9);
/// ```
#[derive(Debug, Clone)]
pub struct FenwickTree {
    /// Internal BIT array (1-indexed, element 0 unused).
    tree: Vec<i64>,
    /// Number of logical elements.
    n: usize,
    /// Count of update operations.
    update_ops: u64,
    /// Count of query operations.
    query_ops: u64,
}

impl FenwickTree {
    /// Create a new Fenwick Tree with `n` elements, all initialized to 0.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            tree: vec![0i64; n + 1],
            n,
            update_ops: 0,
            query_ops: 0,
        }
    }

    /// Create a Fenwick Tree from a slice of initial values.
    ///
    /// This is O(n) — more efficient than calling `update` n times.
    #[must_use]
    pub fn from_slice(values: &[i64]) -> Self {
        let n = values.len();
        let mut tree = vec![0i64; n + 1];

        // Copy values into 1-indexed positions.
        for (i, &v) in values.iter().enumerate() {
            tree[i + 1] = v;
        }

        // Build the tree in O(n) using the standard technique:
        // each node accumulates into its parent.
        for i in 1..=n {
            let parent = i + lowest_set_bit(i);
            if parent <= n {
                tree[parent] = tree[parent].wrapping_add(tree[i]);
            }
        }

        Self {
            tree,
            n,
            update_ops: 0,
            query_ops: 0,
        }
    }

    /// Create from config.
    #[must_use]
    pub fn from_config(config: &FenwickConfig) -> Self {
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

    /// Add `delta` to the element at `index`.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len()`.
    pub fn update(&mut self, index: usize, delta: i64) {
        assert!(index < self.n, "index {index} out of bounds for len {}", self.n);
        self.update_ops += 1;
        let mut i = index + 1; // convert to 1-based
        while i <= self.n {
            self.tree[i] = self.tree[i].wrapping_add(delta);
            i += lowest_set_bit(i);
        }
    }

    /// Set the element at `index` to `value`.
    ///
    /// This computes the delta from the current value and applies it.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len()`.
    pub fn set(&mut self, index: usize, value: i64) {
        let current = self.point_query(index);
        let delta = value.wrapping_sub(current);
        if delta != 0 {
            self.update(index, delta);
        }
    }

    /// Compute the prefix sum of elements `[0..=index]`.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len()`.
    #[must_use]
    pub fn prefix_sum(&self, index: usize) -> i64 {
        assert!(index < self.n, "index {index} out of bounds for len {}", self.n);
        self.prefix_sum_internal(index + 1)
    }

    /// Internal prefix sum using 1-based index.
    fn prefix_sum_internal(&self, mut i: usize) -> i64 {
        let mut sum = 0i64;
        while i > 0 {
            sum = sum.wrapping_add(self.tree[i]);
            i -= lowest_set_bit(i);
        }
        sum
    }

    /// Compute the sum of elements in range `[left..=right]`.
    ///
    /// # Panics
    ///
    /// Panics if `left > right` or `right >= len()`.
    #[must_use]
    pub fn range_sum(&self, left: usize, right: usize) -> i64 {
        assert!(left <= right, "left {left} > right {right}");
        assert!(right < self.n, "right {right} out of bounds for len {}", self.n);
        // Increment query count (interior mutability not needed for stats).
        let right_sum = self.prefix_sum_internal(right + 1);
        if left == 0 {
            right_sum
        } else {
            right_sum.wrapping_sub(self.prefix_sum_internal(left))
        }
    }

    /// Query the value of a single element at `index`.
    ///
    /// This is O(log n), computed as `prefix_sum(index) - prefix_sum(index - 1)`.
    ///
    /// # Panics
    ///
    /// Panics if `index >= len()`.
    #[must_use]
    pub fn point_query(&self, index: usize) -> i64 {
        assert!(index < self.n, "index {index} out of bounds for len {}", self.n);
        if index == 0 {
            self.prefix_sum_internal(1)
        } else {
            self.prefix_sum_internal(index + 1)
                .wrapping_sub(self.prefix_sum_internal(index))
        }
    }

    /// Find the smallest index where `prefix_sum(index) >= target`.
    ///
    /// Returns `None` if no such index exists (i.e., total sum < target).
    ///
    /// **Requires all values to be non-negative** for correct results.
    /// With negative values, the prefix sum is not monotonic and the result
    /// is undefined.
    ///
    /// Runs in O(log² n) using binary search over prefix sums.
    #[must_use]
    pub fn find_kth(&self, target: i64) -> Option<usize> {
        if self.n == 0 {
            return None;
        }
        let total = self.prefix_sum_internal(self.n);
        if total < target {
            return None;
        }
        // Binary search: find smallest i such that prefix_sum(i) >= target
        let mut lo = 0usize;
        let mut hi = self.n - 1;
        let mut result = hi;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            if self.prefix_sum_internal(mid + 1) >= target {
                result = mid;
                if mid == 0 {
                    break;
                }
                hi = mid - 1;
            } else {
                lo = mid + 1;
            }
        }
        Some(result)
    }

    /// Total sum of all elements.
    #[must_use]
    pub fn total_sum(&self) -> i64 {
        if self.n == 0 {
            0
        } else {
            self.prefix_sum_internal(self.n)
        }
    }

    /// Reset all elements to zero.
    pub fn reset(&mut self) {
        self.tree.fill(0);
    }

    /// Get statistics about this tree.
    #[must_use]
    pub fn stats(&self) -> FenwickStats {
        FenwickStats {
            element_count: self.n,
            total_sum: self.total_sum(),
            update_count: self.update_ops,
            query_count: self.query_ops,
            memory_bytes: self.memory_bytes(),
        }
    }

    /// Approximate memory usage in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        // Vec overhead + i64 per element (n+1 slots) + struct fields
        std::mem::size_of::<Self>() + (self.n + 1) * std::mem::size_of::<i64>()
    }

    /// Extract all element values as a Vec.
    ///
    /// Each element is recovered via `point_query`, so this is O(n log n).
    #[must_use]
    pub fn to_vec(&self) -> Vec<i64> {
        (0..self.n).map(|i| self.point_query(i)).collect()
    }

    /// Merge another Fenwick Tree into this one by element-wise addition.
    ///
    /// Both trees must have the same length.
    ///
    /// # Panics
    ///
    /// Panics if lengths differ.
    pub fn merge(&mut self, other: &FenwickTree) {
        assert_eq!(self.n, other.n, "cannot merge trees of different lengths");
        // Add each element from other into self.
        for i in 0..self.n {
            let val = other.point_query(i);
            if val != 0 {
                self.update(i, val);
            }
        }
    }
}

impl Default for FenwickTree {
    fn default() -> Self {
        Self::new(FenwickConfig::default().capacity)
    }
}

/// Extract the lowest set bit of `x`.
///
/// For a number like 12 (0b1100), this returns 4 (0b0100).
#[inline]
fn lowest_set_bit(x: usize) -> usize {
    // Two's complement trick: x & (-x)
    // In Rust with usize, we use wrapping_neg.
    x & x.wrapping_neg()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_tree() {
        let ft = FenwickTree::new(0);
        assert!(ft.is_empty());
        assert_eq!(ft.len(), 0);
        assert_eq!(ft.total_sum(), 0);
    }

    #[test]
    fn test_single_element() {
        let mut ft = FenwickTree::new(1);
        assert_eq!(ft.prefix_sum(0), 0);
        ft.update(0, 5);
        assert_eq!(ft.prefix_sum(0), 5);
        assert_eq!(ft.total_sum(), 5);
    }

    #[test]
    fn test_basic_updates() {
        let mut ft = FenwickTree::new(5);
        ft.update(0, 3);
        ft.update(1, 7);
        ft.update(2, 2);
        ft.update(3, 5);
        ft.update(4, 1);

        assert_eq!(ft.prefix_sum(0), 3);
        assert_eq!(ft.prefix_sum(1), 10);
        assert_eq!(ft.prefix_sum(2), 12);
        assert_eq!(ft.prefix_sum(3), 17);
        assert_eq!(ft.prefix_sum(4), 18);
    }

    #[test]
    fn test_range_sum() {
        let mut ft = FenwickTree::new(5);
        ft.update(0, 3);
        ft.update(1, 7);
        ft.update(2, 2);
        ft.update(3, 5);
        ft.update(4, 1);

        assert_eq!(ft.range_sum(0, 4), 18);
        assert_eq!(ft.range_sum(1, 3), 14);
        assert_eq!(ft.range_sum(2, 2), 2);
        assert_eq!(ft.range_sum(0, 0), 3);
    }

    #[test]
    fn test_point_query() {
        let mut ft = FenwickTree::new(4);
        ft.update(0, 10);
        ft.update(1, 20);
        ft.update(2, 30);
        ft.update(3, 40);

        assert_eq!(ft.point_query(0), 10);
        assert_eq!(ft.point_query(1), 20);
        assert_eq!(ft.point_query(2), 30);
        assert_eq!(ft.point_query(3), 40);
    }

    #[test]
    fn test_from_slice() {
        let values = [3, 7, 2, 5, 1];
        let ft = FenwickTree::from_slice(&values);

        assert_eq!(ft.len(), 5);
        assert_eq!(ft.prefix_sum(0), 3);
        assert_eq!(ft.prefix_sum(1), 10);
        assert_eq!(ft.prefix_sum(2), 12);
        assert_eq!(ft.prefix_sum(3), 17);
        assert_eq!(ft.prefix_sum(4), 18);
    }

    #[test]
    fn test_from_slice_matches_incremental() {
        let values = [1, 2, 3, 4, 5, 6, 7, 8];
        let ft_slice = FenwickTree::from_slice(&values);

        let mut ft_inc = FenwickTree::new(8);
        for (i, &v) in values.iter().enumerate() {
            ft_inc.update(i, v);
        }

        for i in 0..8 {
            assert_eq!(ft_slice.prefix_sum(i), ft_inc.prefix_sum(i));
        }
    }

    #[test]
    fn test_set() {
        let mut ft = FenwickTree::new(3);
        ft.set(0, 10);
        ft.set(1, 20);
        ft.set(2, 30);

        assert_eq!(ft.point_query(0), 10);
        assert_eq!(ft.point_query(1), 20);
        assert_eq!(ft.point_query(2), 30);

        ft.set(1, 5); // change from 20 to 5
        assert_eq!(ft.point_query(1), 5);
        assert_eq!(ft.total_sum(), 45);
    }

    #[test]
    fn test_negative_values() {
        let mut ft = FenwickTree::new(3);
        ft.update(0, 10);
        ft.update(1, -5);
        ft.update(2, 3);

        assert_eq!(ft.prefix_sum(0), 10);
        assert_eq!(ft.prefix_sum(1), 5);
        assert_eq!(ft.prefix_sum(2), 8);
        assert_eq!(ft.point_query(1), -5);
    }

    #[test]
    fn test_find_kth_basic() {
        let mut ft = FenwickTree::new(5);
        ft.update(0, 3);
        ft.update(1, 0);
        ft.update(2, 7);
        ft.update(3, 0);
        ft.update(4, 2);

        // prefix sums: [3, 3, 10, 10, 12]
        assert_eq!(ft.find_kth(1), Some(0));   // first index with prefix >= 1
        assert_eq!(ft.find_kth(3), Some(0));   // prefix_sum(0) = 3 >= 3
        assert_eq!(ft.find_kth(4), Some(2));   // prefix_sum(2) = 10 >= 4
        assert_eq!(ft.find_kth(10), Some(2));  // prefix_sum(2) = 10 >= 10
        assert_eq!(ft.find_kth(11), Some(4));  // prefix_sum(4) = 12 >= 11
        assert_eq!(ft.find_kth(12), Some(4));  // prefix_sum(4) = 12 >= 12
        assert_eq!(ft.find_kth(13), None);     // total = 12 < 13
    }

    #[test]
    fn test_find_kth_empty() {
        let ft = FenwickTree::new(0);
        assert_eq!(ft.find_kth(1), None);
    }

    #[test]
    fn test_find_kth_all_zeros() {
        let ft = FenwickTree::new(5);
        assert_eq!(ft.find_kth(0), Some(0));
        assert_eq!(ft.find_kth(1), None);
    }

    #[test]
    fn test_total_sum() {
        let mut ft = FenwickTree::new(4);
        assert_eq!(ft.total_sum(), 0);
        ft.update(0, 10);
        ft.update(3, 5);
        assert_eq!(ft.total_sum(), 15);
    }

    #[test]
    fn test_reset() {
        let mut ft = FenwickTree::new(3);
        ft.update(0, 10);
        ft.update(1, 20);
        ft.update(2, 30);
        assert_eq!(ft.total_sum(), 60);

        ft.reset();
        assert_eq!(ft.total_sum(), 0);
        assert_eq!(ft.len(), 3);
        for i in 0..3 {
            assert_eq!(ft.point_query(i), 0);
        }
    }

    #[test]
    fn test_to_vec() {
        let values = [5, 3, 8, 1, 6];
        let ft = FenwickTree::from_slice(&values);
        assert_eq!(ft.to_vec(), values.to_vec());
    }

    #[test]
    fn test_merge() {
        let mut ft1 = FenwickTree::from_slice(&[1, 2, 3]);
        let ft2 = FenwickTree::from_slice(&[10, 20, 30]);
        ft1.merge(&ft2);

        assert_eq!(ft1.to_vec(), vec![11, 22, 33]);
    }

    #[test]
    fn test_clone_independence() {
        let mut ft = FenwickTree::from_slice(&[1, 2, 3]);
        let original_sum = ft.total_sum();
        let mut clone = ft.clone();
        clone.update(0, 100);

        assert_eq!(ft.total_sum(), original_sum);
        assert_eq!(clone.total_sum(), original_sum + 100);
    }

    #[test]
    fn test_stats() {
        let mut ft = FenwickTree::new(10);
        ft.update(0, 5);
        ft.update(5, 3);

        let stats = ft.stats();
        assert_eq!(stats.element_count, 10);
        assert_eq!(stats.total_sum, 8);
        assert_eq!(stats.update_count, 2);
        assert_eq!(stats.memory_bytes, ft.memory_bytes());
    }

    #[test]
    fn test_config_serde() {
        let config = FenwickConfig { capacity: 42 };
        let json = serde_json::to_string(&config).unwrap();
        let back: FenwickConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn test_stats_serde() {
        let mut ft = FenwickTree::new(5);
        ft.update(0, 10);
        let stats = ft.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: FenwickStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn test_from_config() {
        let config = FenwickConfig { capacity: 8 };
        let ft = FenwickTree::from_config(&config);
        assert_eq!(ft.len(), 8);
        assert_eq!(ft.total_sum(), 0);
    }

    #[test]
    fn test_default() {
        let ft = FenwickTree::default();
        assert_eq!(ft.len(), 64);
    }

    #[test]
    fn test_memory_bytes_scales() {
        let ft1 = FenwickTree::new(10);
        let ft2 = FenwickTree::new(100);
        assert!(ft2.memory_bytes() > ft1.memory_bytes());
    }

    #[test]
    fn test_cumulative_updates() {
        let mut ft = FenwickTree::new(3);
        ft.update(1, 5);
        ft.update(1, 3);
        ft.update(1, -2);
        assert_eq!(ft.point_query(1), 6);
    }

    #[test]
    fn test_large_tree() {
        let n = 1000;
        let values: Vec<i64> = (0..n as i64).collect();
        let ft = FenwickTree::from_slice(&values);

        // prefix_sum(n-1) should be sum of 0..n = n*(n-1)/2
        let expected = (n as i64) * (n as i64 - 1) / 2;
        assert_eq!(ft.total_sum(), expected);
    }

    #[test]
    fn test_from_empty_slice() {
        let ft = FenwickTree::from_slice(&[]);
        assert!(ft.is_empty());
        assert_eq!(ft.total_sum(), 0);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_update_out_of_bounds() {
        let mut ft = FenwickTree::new(3);
        ft.update(3, 1);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn test_prefix_sum_out_of_bounds() {
        let ft = FenwickTree::new(3);
        ft.prefix_sum(3);
    }

    #[test]
    #[should_panic(expected = "left")]
    fn test_range_sum_invalid() {
        let ft = FenwickTree::new(5);
        ft.range_sum(3, 1);
    }

    #[test]
    fn test_lowest_set_bit() {
        assert_eq!(lowest_set_bit(1), 1);
        assert_eq!(lowest_set_bit(2), 2);
        assert_eq!(lowest_set_bit(3), 1);
        assert_eq!(lowest_set_bit(4), 4);
        assert_eq!(lowest_set_bit(6), 2);
        assert_eq!(lowest_set_bit(12), 4);
    }

    #[test]
    #[should_panic(expected = "cannot merge trees of different lengths")]
    fn test_merge_different_lengths() {
        let mut ft1 = FenwickTree::new(3);
        let ft2 = FenwickTree::new(5);
        ft1.merge(&ft2);
    }
}
