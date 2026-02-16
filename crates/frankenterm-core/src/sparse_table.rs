//! Sparse table — O(1) range minimum/maximum queries after O(n log n) preprocessing.
//!
//! A sparse table is a static data structure for answering range queries
//! on idempotent operations (min, max, gcd) in constant time. It
//! precomputes answers for all power-of-two-length intervals.
//!
//! # Complexity
//!
//! - **O(n log n)**: build time and space
//! - **O(1)**: range query (for idempotent operations)
//!
//! # Design
//!
//! For each position `i` and power `k`, stores the result of the query
//! on `[i, i + 2^k)`. Queries decompose into two overlapping intervals
//! of length `2^floor(log2(len))`.
//!
//! # Use in FrankenTerm
//!
//! Fast range-min/max queries on scrollback metrics, timestamp sequences,
//! and output-rate windows for anomaly detection.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── SparseTable ───────────────────────────────────────────────────────

/// Operation mode for the sparse table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryOp {
    /// Range minimum query.
    Min,
    /// Range maximum query.
    Max,
}

/// Static sparse table for O(1) range min/max queries.
///
/// Built once from a slice of values, then supports constant-time
/// queries on any subrange.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseTable<T> {
    table: Vec<Vec<T>>,
    log_table: Vec<usize>,
    op: QueryOp,
    len: usize,
}

impl<T: Ord + Clone> SparseTable<T> {
    /// Builds a sparse table from a slice with the given query operation.
    pub fn build(data: &[T], op: QueryOp) -> Self {
        let n = data.len();
        if n == 0 {
            return Self {
                table: Vec::new(),
                log_table: Vec::new(),
                op,
                len: 0,
            };
        }

        // Precompute floor(log2(i)) for all i
        let mut log_table = vec![0usize; n + 1];
        for i in 2..=n {
            log_table[i] = log_table[i / 2] + 1;
        }

        let max_log = log_table[n] + 1;
        let mut table = vec![Vec::with_capacity(n); max_log];

        // Level 0: individual elements
        table[0] = data.to_vec();

        // Fill levels 1..max_log
        for k in 1..max_log {
            let half = 1 << (k - 1);
            let row_len = if n >= (1 << k) { n - (1 << k) + 1 } else { 0 };
            table[k] = Vec::with_capacity(row_len);
            for i in 0..row_len {
                let left = &table[k - 1][i];
                let right = &table[k - 1][i + half];
                let val = match op {
                    QueryOp::Min => {
                        if left <= right {
                            left.clone()
                        } else {
                            right.clone()
                        }
                    }
                    QueryOp::Max => {
                        if left >= right {
                            left.clone()
                        } else {
                            right.clone()
                        }
                    }
                };
                table[k].push(val);
            }
        }

        Self {
            table,
            log_table,
            op,
            len: n,
        }
    }

    /// Builds a range minimum query table.
    pub fn min_table(data: &[T]) -> Self {
        Self::build(data, QueryOp::Min)
    }

    /// Builds a range maximum query table.
    pub fn max_table(data: &[T]) -> Self {
        Self::build(data, QueryOp::Max)
    }

    /// Queries the range `[left, right]` (inclusive).
    ///
    /// # Panics
    ///
    /// Panics if `left > right` or `right >= len`.
    pub fn query(&self, left: usize, right: usize) -> T {
        assert!(left <= right, "left must be <= right");
        assert!(right < self.len, "right must be < len");

        let range_len = right - left + 1;
        let k = self.log_table[range_len];
        let left_val = &self.table[k][left];
        // right + 1 - (1 << k) avoids usize underflow
        let right_start = right + 1 - (1 << k);
        let right_val = &self.table[k][right_start];

        match self.op {
            QueryOp::Min => {
                if left_val <= right_val {
                    left_val.clone()
                } else {
                    right_val.clone()
                }
            }
            QueryOp::Max => {
                if left_val >= right_val {
                    left_val.clone()
                } else {
                    right_val.clone()
                }
            }
        }
    }

    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the query operation mode.
    pub fn operation(&self) -> QueryOp {
        self.op
    }

    /// Returns the value at the given index.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len {
            Some(&self.table[0][index])
        } else {
            None
        }
    }
}

impl<T: Ord + Clone + fmt::Display> fmt::Display for SparseTable<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SparseTable({} elements, {:?})",
            self.len, self.op
        )
    }
}

// ── Index-returning variant ───────────────────────────────────────────

/// Sparse table that returns the *index* of the min/max element.
///
/// Useful when you need the position, not just the value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexSparseTable<T> {
    data: Vec<T>,
    table: Vec<Vec<usize>>, // stores indices
    log_table: Vec<usize>,
    op: QueryOp,
}

impl<T: Ord + Clone> IndexSparseTable<T> {
    /// Builds an index-returning sparse table.
    pub fn build(data: &[T], op: QueryOp) -> Self {
        let n = data.len();
        if n == 0 {
            return Self {
                data: Vec::new(),
                table: Vec::new(),
                log_table: Vec::new(),
                op,
            };
        }

        let mut log_table = vec![0usize; n + 1];
        for i in 2..=n {
            log_table[i] = log_table[i / 2] + 1;
        }

        let max_log = log_table[n] + 1;
        let mut table = vec![Vec::with_capacity(n); max_log];

        // Level 0: each element is its own answer
        table[0] = (0..n).collect();

        for k in 1..max_log {
            let half = 1 << (k - 1);
            let row_len = if n >= (1 << k) { n - (1 << k) + 1 } else { 0 };
            table[k] = Vec::with_capacity(row_len);
            for i in 0..row_len {
                let li = table[k - 1][i];
                let ri = table[k - 1][i + half];
                let winner = match op {
                    QueryOp::Min => {
                        if data[li] <= data[ri] {
                            li
                        } else {
                            ri
                        }
                    }
                    QueryOp::Max => {
                        if data[li] >= data[ri] {
                            li
                        } else {
                            ri
                        }
                    }
                };
                table[k].push(winner);
            }
        }

        Self {
            data: data.to_vec(),
            table,
            log_table,
            op,
        }
    }

    /// Queries the range `[left, right]` (inclusive), returning the index.
    ///
    /// # Panics
    ///
    /// Panics if `left > right` or `right >= len`.
    pub fn query_index(&self, left: usize, right: usize) -> usize {
        assert!(left <= right, "left must be <= right");
        assert!(right < self.data.len(), "right must be < len");

        let range_len = right - left + 1;
        let k = self.log_table[range_len];
        let li = self.table[k][left];
        let right_start = right + 1 - (1 << k);
        let ri = self.table[k][right_start];

        match self.op {
            QueryOp::Min => {
                if self.data[li] <= self.data[ri] {
                    li
                } else {
                    ri
                }
            }
            QueryOp::Max => {
                if self.data[li] >= self.data[ri] {
                    li
                } else {
                    ri
                }
            }
        }
    }

    /// Queries the range, returning both the index and value.
    pub fn query(&self, left: usize, right: usize) -> (usize, &T) {
        let idx = self.query_index(left, right);
        (idx, &self.data[idx])
    }

    /// Returns the number of elements.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns true if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table() {
        let st = SparseTable::<i32>::min_table(&[]);
        assert!(st.is_empty());
        assert_eq!(st.len(), 0);
    }

    #[test]
    fn single_element() {
        let st = SparseTable::min_table(&[42]);
        assert_eq!(st.query(0, 0), 42);
    }

    #[test]
    fn min_query_basic() {
        let data = [5, 2, 8, 1, 4, 7, 9, 3, 6];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 8), 1); // global min
        assert_eq!(st.query(0, 2), 2); // min of [5,2,8]
        assert_eq!(st.query(3, 5), 1); // min of [1,4,7]
        assert_eq!(st.query(6, 8), 3); // min of [9,3,6]
        assert_eq!(st.query(4, 4), 4); // single element
    }

    #[test]
    fn max_query_basic() {
        let data = [5, 2, 8, 1, 4, 7, 9, 3, 6];
        let st = SparseTable::max_table(&data);
        assert_eq!(st.query(0, 8), 9); // global max
        assert_eq!(st.query(0, 2), 8); // max of [5,2,8]
        assert_eq!(st.query(3, 5), 7); // max of [1,4,7]
        assert_eq!(st.query(6, 8), 9); // max of [9,3,6]
    }

    #[test]
    fn all_same_values() {
        let data = [3, 3, 3, 3, 3];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 4), 3);
        assert_eq!(st.query(1, 3), 3);
    }

    #[test]
    fn sorted_ascending() {
        let data = [1, 2, 3, 4, 5];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 4), 1);
        assert_eq!(st.query(2, 4), 3);

        let st_max = SparseTable::max_table(&data);
        assert_eq!(st_max.query(0, 4), 5);
        assert_eq!(st_max.query(0, 2), 3);
    }

    #[test]
    fn sorted_descending() {
        let data = [5, 4, 3, 2, 1];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 4), 1);
        assert_eq!(st.query(0, 2), 3);
    }

    #[test]
    fn index_sparse_table_min() {
        let data = [5, 2, 8, 1, 4, 7, 9, 3, 6];
        let ist = IndexSparseTable::build(&data, QueryOp::Min);
        assert_eq!(ist.query_index(0, 8), 3); // index of min=1
        assert_eq!(ist.query_index(0, 2), 1); // index of min=2
        assert_eq!(ist.query_index(6, 8), 7); // index of min=3
    }

    #[test]
    fn index_sparse_table_max() {
        let data = [5, 2, 8, 1, 4, 7, 9, 3, 6];
        let ist = IndexSparseTable::build(&data, QueryOp::Max);
        assert_eq!(ist.query_index(0, 8), 6); // index of max=9
        let (idx, val) = ist.query(0, 2);
        assert_eq!(idx, 2);
        assert_eq!(*val, 8);
    }

    #[test]
    fn get_element() {
        let data = [10, 20, 30];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.get(0), Some(&10));
        assert_eq!(st.get(2), Some(&30));
        assert_eq!(st.get(3), None);
    }

    #[test]
    fn operation_mode() {
        let st_min = SparseTable::min_table(&[1, 2, 3]);
        assert_eq!(st_min.operation(), QueryOp::Min);

        let st_max = SparseTable::max_table(&[1, 2, 3]);
        assert_eq!(st_max.operation(), QueryOp::Max);
    }

    #[test]
    fn two_elements() {
        let data = [7, 3];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 1), 3);
        assert_eq!(st.query(0, 0), 7);
        assert_eq!(st.query(1, 1), 3);
    }

    #[test]
    fn power_of_two_length() {
        let data = [4, 1, 3, 2, 8, 5, 7, 6];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 7), 1);
        assert_eq!(st.query(4, 7), 5);
    }

    #[test]
    fn serde_roundtrip() {
        let data = [5, 2, 8, 1, 4];
        let st = SparseTable::min_table(&data);
        let json = serde_json::to_string(&st).unwrap();
        let restored: SparseTable<i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), st.len());
        assert_eq!(restored.query(0, 4), st.query(0, 4));
        assert_eq!(restored.query(1, 3), st.query(1, 3));
    }

    #[test]
    fn display_format() {
        let st = SparseTable::min_table(&[1, 2, 3]);
        assert_eq!(format!("{}", st), "SparseTable(3 elements, Min)");
    }

    #[test]
    #[should_panic(expected = "left must be <= right")]
    fn query_left_greater_than_right() {
        let st = SparseTable::min_table(&[1, 2, 3]);
        st.query(2, 1);
    }

    #[test]
    #[should_panic(expected = "right must be < len")]
    fn query_out_of_bounds() {
        let st = SparseTable::min_table(&[1, 2, 3]);
        st.query(0, 3);
    }

    #[test]
    fn string_keys() {
        let data = ["cherry", "apple", "banana", "date"];
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 3), "apple");

        let st_max = SparseTable::max_table(&data);
        assert_eq!(st_max.query(0, 3), "date");
    }

    #[test]
    fn large_table() {
        let data: Vec<i32> = (0..1000).rev().collect();
        let st = SparseTable::min_table(&data);
        assert_eq!(st.query(0, 999), 0);
        assert_eq!(st.query(500, 999), 0);
        assert_eq!(st.query(0, 499), 500);
    }
}
