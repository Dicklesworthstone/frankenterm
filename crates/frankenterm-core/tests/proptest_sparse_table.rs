//! Property-based tests for `sparse_table` module.
//!
//! Verifies correctness invariants:
//! - Range min matches brute-force scan
//! - Range max matches brute-force scan
//! - Single-element queries return the element
//! - Index queries return correct positions
//! - Serde roundtrip

use frankenterm_core::sparse_table::{IndexSparseTable, QueryOp, SparseTable};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn values_strategy(max_len: usize) -> impl Strategy<Value = Vec<i32>> {
    prop::collection::vec(-1000..1000i32, 1..max_len)
}

// ── Brute-force reference ────────────────────────────────────────────

fn brute_min(data: &[i32], left: usize, right: usize) -> i32 {
    data[left..=right].iter().copied().min().unwrap()
}

fn brute_max(data: &[i32], left: usize, right: usize) -> i32 {
    data[left..=right].iter().copied().max().unwrap()
}

fn brute_argmin(data: &[i32], left: usize, right: usize) -> usize {
    let min_val = brute_min(data, left, right);
    (left..=right).find(|&i| data[i] == min_val).unwrap()
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Range min matches brute force ────────────────────────────

    #[test]
    fn range_min_matches(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        // Test 20 random ranges
        for l in 0..n.min(20) {
            for step in [0, 1, n / 4, n / 2, n - 1] {
                let r = (l + step).min(n - 1);
                let got = st.query(l, r);
                let expected = brute_min(&data, l, r);
                prop_assert_eq!(got, expected, "min mismatch at [{}, {}]", l, r);
            }
        }
    }

    // ── Range max matches brute force ────────────────────────────

    #[test]
    fn range_max_matches(data in values_strategy(50)) {
        let st = SparseTable::max_table(&data);
        let n = data.len();

        for l in 0..n.min(20) {
            for step in [0, 1, n / 4, n / 2, n - 1] {
                let r = (l + step).min(n - 1);
                let got = st.query(l, r);
                let expected = brute_max(&data, l, r);
                prop_assert_eq!(got, expected, "max mismatch at [{}, {}]", l, r);
            }
        }
    }

    // ── Single-element queries ───────────────────────────────────

    #[test]
    fn single_element_queries(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        for (i, val) in data.iter().enumerate() {
            prop_assert_eq!(st.query(i, i), *val);
        }
    }

    // ── Full range is global min/max ─────────────────────────────

    #[test]
    fn full_range_is_global(data in values_strategy(50)) {
        let st_min = SparseTable::min_table(&data);
        let st_max = SparseTable::max_table(&data);
        let n = data.len();

        let global_min = *data.iter().min().unwrap();
        let global_max = *data.iter().max().unwrap();

        prop_assert_eq!(st_min.query(0, n - 1), global_min);
        prop_assert_eq!(st_max.query(0, n - 1), global_max);
    }

    // ── Length and get consistency ────────────────────────────────

    #[test]
    fn length_and_get(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        prop_assert_eq!(st.len(), data.len());
        prop_assert!(!st.is_empty());

        for (i, val) in data.iter().enumerate() {
            prop_assert_eq!(st.get(i), Some(val));
        }
        prop_assert!(st.get(data.len()).is_none());
    }

    // ── Index sparse table min matches ───────────────────────────

    #[test]
    fn index_min_matches(data in values_strategy(50)) {
        let ist = IndexSparseTable::build(&data, QueryOp::Min);
        let n = data.len();

        for l in 0..n.min(15) {
            for step in [0, 1, n / 3, n - 1] {
                let r = (l + step).min(n - 1);
                let idx = ist.query_index(l, r);
                let expected_idx = brute_argmin(&data, l, r);

                // Index might differ for ties, but value must match
                prop_assert_eq!(
                    data[idx], data[expected_idx],
                    "value at index mismatch for [{}, {}]", l, r
                );
                prop_assert!(idx >= l && idx <= r, "index out of range");
            }
        }
    }

    // ── Index query returns value correctly ───────────────────────

    #[test]
    fn index_query_value(data in values_strategy(50)) {
        let ist = IndexSparseTable::build(&data, QueryOp::Min);
        let n = data.len();

        if n >= 2 {
            let (idx, val) = ist.query(0, n - 1);
            let expected = brute_min(&data, 0, n - 1);
            prop_assert_eq!(*val, expected);
            prop_assert_eq!(data[idx], expected);
        }
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(data in values_strategy(30)) {
        let st = SparseTable::min_table(&data);
        let json = serde_json::to_string(&st).unwrap();
        let restored: SparseTable<i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), st.len());
        let n = data.len();
        // Verify a few queries match
        prop_assert_eq!(restored.query(0, n - 1), st.query(0, n - 1));
        if n >= 3 {
            prop_assert_eq!(restored.query(1, n - 2), st.query(1, n - 2));
        }
    }

    // ── Adjacent queries are consistent ──────────────────────────

    #[test]
    fn adjacent_queries_consistent(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        // min(l, r) == min(min(l, r-1), data[r])
        for l in 0..n.min(10) {
            for r in (l + 1)..n.min(l + 10) {
                let full = st.query(l, r);
                let left_part = st.query(l, r - 1);
                let expected = left_part.min(data[r]);
                prop_assert_eq!(full, expected, "adjacent inconsistency at [{}, {}]", l, r);
            }
        }
    }

    // ── Subrange min <= superrange min ────────────────────────────

    #[test]
    fn subrange_min_geq(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        // For min: subrange min >= superrange min
        if n >= 4 {
            let outer = st.query(0, n - 1);
            let inner = st.query(1, n - 2);
            prop_assert!(inner >= outer, "inner min should be >= outer min");
        }
    }

    // ── Subrange max <= superrange max ────────────────────────────

    #[test]
    fn subrange_max_leq(data in values_strategy(50)) {
        let st = SparseTable::max_table(&data);
        let n = data.len();

        if n >= 4 {
            let outer = st.query(0, n - 1);
            let inner = st.query(1, n - 2);
            prop_assert!(inner <= outer, "inner max should be <= outer max");
        }
    }

    // ── Min of entire range is min of any partition ──────────────

    #[test]
    fn partition_consistency(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        if n >= 3 {
            let mid = n / 2;
            let left_min = st.query(0, mid);
            let right_min = st.query(mid, n - 1);
            let full_min = st.query(0, n - 1);
            prop_assert!(full_min <= left_min);
            prop_assert!(full_min <= right_min);
            prop_assert!(full_min == left_min || full_min == right_min);
        }
    }
}
