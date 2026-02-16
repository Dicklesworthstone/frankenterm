#![allow(clippy::needless_range_loop)]
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

fn brute_argmax(data: &[i32], left: usize, right: usize) -> usize {
    let max_val = brute_max(data, left, right);
    (left..=right).find(|&i| data[i] == max_val).unwrap()
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

    // ══════════════════════════════════════════════════════════════
    //  NEW TESTS (13 additional properties)
    // ══════════════════════════════════════════════════════════════

    // ── Exhaustive small min ─────────────────────────────────────

    #[test]
    fn exhaustive_small_min(data in prop::collection::vec(-100..100i32, 1..10usize)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        for l in 0..n {
            for r in l..n {
                let got = st.query(l, r);
                let expected = brute_min(&data, l, r);
                prop_assert_eq!(got, expected, "exhaustive min mismatch at [{}, {}]", l, r);
            }
        }
    }

    // ── Exhaustive small max ─────────────────────────────────────

    #[test]
    fn exhaustive_small_max(data in prop::collection::vec(-100..100i32, 1..10usize)) {
        let st = SparseTable::max_table(&data);
        let n = data.len();

        for l in 0..n {
            for r in l..n {
                let got = st.query(l, r);
                let expected = brute_max(&data, l, r);
                prop_assert_eq!(got, expected, "exhaustive max mismatch at [{}, {}]", l, r);
            }
        }
    }

    // ── Index max matches ────────────────────────────────────────

    #[test]
    fn index_max_matches(data in values_strategy(50)) {
        let ist = IndexSparseTable::build(&data, QueryOp::Max);
        let n = data.len();

        for l in 0..n.min(15) {
            for step in [0, 1, n / 3, n - 1] {
                let r = (l + step).min(n - 1);
                let idx = ist.query_index(l, r);
                let expected_idx = brute_argmax(&data, l, r);

                // Index might differ for ties, but value must match
                prop_assert_eq!(
                    data[idx], data[expected_idx],
                    "max index value mismatch for [{}, {}]", l, r
                );
                prop_assert!(idx >= l && idx <= r, "max index out of range");
            }
        }
    }

    // ── Index always in range ────────────────────────────────────

    #[test]
    fn index_always_in_range(data in values_strategy(50)) {
        let ist_min = IndexSparseTable::build(&data, QueryOp::Min);
        let ist_max = IndexSparseTable::build(&data, QueryOp::Max);
        let n = data.len();

        for l in 0..n.min(15) {
            for step in [0, 1, n / 4, n / 2, n - 1] {
                let r = (l + step).min(n - 1);
                let idx_min = ist_min.query_index(l, r);
                let idx_max = ist_max.query_index(l, r);

                prop_assert!(
                    idx_min >= l && idx_min <= r,
                    "min index {} not in [{}, {}]", idx_min, l, r
                );
                prop_assert!(
                    idx_max >= l && idx_max <= r,
                    "max index {} not in [{}, {}]", idx_max, l, r
                );
            }
        }
    }

    // ── Index serde roundtrip ────────────────────────────────────

    #[test]
    fn index_serde_roundtrip(data in values_strategy(30)) {
        let ist = IndexSparseTable::build(&data, QueryOp::Min);
        let json = serde_json::to_string(&ist).unwrap();
        let restored: IndexSparseTable<i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), ist.len());
        let n = data.len();

        // Verify full-range query matches
        let (orig_idx, orig_val) = ist.query(0, n - 1);
        let (rest_idx, rest_val) = restored.query(0, n - 1);
        prop_assert_eq!(*rest_val, *orig_val, "index serde value mismatch");
        prop_assert_eq!(
            data[rest_idx], data[orig_idx],
            "index serde index-value mismatch"
        );

        // Verify a mid-range query if possible
        if n >= 3 {
            let (oi, ov) = ist.query(1, n - 2);
            let (ri, rv) = restored.query(1, n - 2);
            prop_assert_eq!(*rv, *ov, "index serde mid-range value mismatch");
            prop_assert_eq!(
                data[ri], data[oi],
                "index serde mid-range index-value mismatch"
            );
        }
    }

    // ── Clone equivalence ────────────────────────────────────────

    #[test]
    fn clone_equivalence(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let cloned = st.clone();
        let n = data.len();

        prop_assert_eq!(cloned.len(), st.len());
        prop_assert_eq!(cloned.operation(), st.operation());

        // Verify queries match on several ranges
        for l in 0..n.min(10) {
            for step in [0, 1, n / 3, n - 1] {
                let r = (l + step).min(n - 1);
                prop_assert_eq!(
                    cloned.query(l, r), st.query(l, r),
                    "clone mismatch at [{}, {}]", l, r
                );
            }
        }
    }

    // ── Operation preserved ──────────────────────────────────────

    #[test]
    fn operation_preserved(data in values_strategy(50)) {
        let st_min = SparseTable::min_table(&data);
        let st_max = SparseTable::max_table(&data);

        prop_assert_eq!(st_min.operation(), QueryOp::Min, "min_table should have Min op");
        prop_assert_eq!(st_max.operation(), QueryOp::Max, "max_table should have Max op");

        // Also verify build() with explicit op
        let st_build_min = SparseTable::build(&data, QueryOp::Min);
        let st_build_max = SparseTable::build(&data, QueryOp::Max);
        prop_assert_eq!(st_build_min.operation(), QueryOp::Min, "build Min op");
        prop_assert_eq!(st_build_max.operation(), QueryOp::Max, "build Max op");
    }

    // ── Constant data all same ───────────────────────────────────

    #[test]
    fn constant_data_all_same(
        val in -1000..1000i32,
        len in 1..50usize,
    ) {
        let data: Vec<i32> = vec![val; len];
        let st_min = SparseTable::min_table(&data);
        let st_max = SparseTable::max_table(&data);

        for l in 0..len.min(15) {
            for step in [0, 1, len / 3, len - 1] {
                let r = (l + step).min(len - 1);
                prop_assert_eq!(
                    st_min.query(l, r), val,
                    "constant min mismatch at [{}, {}]", l, r
                );
                prop_assert_eq!(
                    st_max.query(l, r), val,
                    "constant max mismatch at [{}, {}]", l, r
                );
            }
        }
    }

    // ── Min <= max for same data ─────────────────────────────────

    #[test]
    fn min_leq_max(data in values_strategy(50)) {
        let st_min = SparseTable::min_table(&data);
        let st_max = SparseTable::max_table(&data);
        let n = data.len();

        for l in 0..n.min(15) {
            for step in [0, 1, n / 4, n / 2, n - 1] {
                let r = (l + step).min(n - 1);
                let min_val = st_min.query(l, r);
                let max_val = st_max.query(l, r);
                prop_assert!(
                    min_val <= max_val,
                    "min {} > max {} at [{}, {}]", min_val, max_val, l, r
                );
            }
        }
    }

    // ── Overlapping ranges ───────────────────────────────────────

    #[test]
    fn overlapping_ranges(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        if n >= 3 {
            for mid in 1..(n - 1).min(15) {
                let full = st.query(0, n - 1);
                let left_part = st.query(0, mid);
                let right_part = st.query(mid, n - 1);

                // The min of each half must be >= the overall min
                prop_assert!(
                    left_part >= full,
                    "left part min {} < full min {} at mid {}", left_part, full, mid
                );
                prop_assert!(
                    right_part >= full,
                    "right part min {} < full min {} at mid {}", right_part, full, mid
                );
            }
        }
    }

    // ── Prefix suffix min ────────────────────────────────────────

    #[test]
    fn prefix_suffix_min(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let n = data.len();

        // min(0, k) for increasing k is non-increasing
        let mut prev_min = data[0];
        for k in 0..n {
            let current_min = st.query(0, k);
            prop_assert!(
                current_min <= prev_min,
                "prefix min increased at k={}: {} > {}", k, current_min, prev_min
            );
            prev_min = current_min;
        }
    }

    // ── Index value matches direct ───────────────────────────────

    #[test]
    fn index_value_matches_direct(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let ist = IndexSparseTable::build(&data, QueryOp::Min);
        let n = data.len();

        for l in 0..n.min(15) {
            for step in [0, 1, n / 3, n - 1] {
                let r = (l + step).min(n - 1);
                let direct_val = st.query(l, r);
                let (idx, idx_val) = ist.query(l, r);
                prop_assert_eq!(
                    *idx_val, direct_val,
                    "index query value {} != direct {} at [{}, {}]", idx_val, direct_val, l, r
                );
                prop_assert_eq!(
                    data[idx], direct_val,
                    "data at index {} != direct {} at [{}, {}]", data[idx], direct_val, l, r
                );
            }
        }
    }

    // ── Get matches original data ────────────────────────────────

    #[test]
    fn get_matches_original_data(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);

        for (i, val) in data.iter().enumerate() {
            let got = st.get(i);
            prop_assert_eq!(got, Some(val), "get({}) mismatch", i);
        }

        // Out-of-bounds returns None
        prop_assert!(st.get(data.len()).is_none(), "get(len) should be None");
        prop_assert!(st.get(data.len() + 100).is_none(), "get(len+100) should be None");
    }

    // ══════════════════════════════════════════════════════════════
    //  ROUND 2: 6 additional property tests (26→31)
    // ══════════════════════════════════════════════════════════════

    // ── is_empty agrees with len ─────────────────────────────────

    #[test]
    fn is_empty_agrees_with_len(data in values_strategy(50)) {
        let st = SparseTable::min_table(&data);
        let ist = IndexSparseTable::build(&data, QueryOp::Min);

        // Non-empty data => not empty, len > 0
        let st_len = st.len();
        let st_empty = st.is_empty();
        prop_assert!(st_len > 0, "len should be > 0 for non-empty data");
        prop_assert!(!st_empty, "is_empty should be false for non-empty data");
        // is_empty <=> len == 0
        prop_assert_eq!(st_empty, st_len == 0, "is_empty must agree with len");

        let ist_len = ist.len();
        let ist_empty = ist.is_empty();
        prop_assert!(ist_len > 0, "IndexSparseTable len should be > 0");
        prop_assert!(!ist_empty, "IndexSparseTable is_empty should be false");
        prop_assert_eq!(ist_empty, ist_len == 0, "IndexSparseTable is_empty must agree with len");
    }

    // ── Debug format does not panic ──────────────────────────────

    #[test]
    fn debug_format_non_empty(data in values_strategy(30)) {
        let st_min = SparseTable::min_table(&data);
        let st_max = SparseTable::max_table(&data);
        let ist = IndexSparseTable::build(&data, QueryOp::Min);

        // Debug format should not panic and should produce non-empty output
        let dbg_min = format!("{:?}", st_min);
        let dbg_max = format!("{:?}", st_max);
        let dbg_ist = format!("{:?}", ist);

        prop_assert!(!dbg_min.is_empty(), "Debug for min table should not be empty");
        prop_assert!(!dbg_max.is_empty(), "Debug for max table should not be empty");
        prop_assert!(!dbg_ist.is_empty(), "Debug for index table should not be empty");

        // Debug should contain type name
        prop_assert!(
            dbg_min.contains("SparseTable"),
            "Debug should contain SparseTable, got: {}", dbg_min
        );
        prop_assert!(
            dbg_ist.contains("IndexSparseTable"),
            "Debug should contain IndexSparseTable, got: {}", dbg_ist
        );
    }

    // ── Display format consistency ───────────────────────────────

    #[test]
    fn display_format_consistency(data in values_strategy(30)) {
        let st_min = SparseTable::min_table(&data);
        let st_max = SparseTable::max_table(&data);

        let disp_min = format!("{}", st_min);
        let disp_max = format!("{}", st_max);
        let expected_len = data.len();

        // Display should show correct count and operation
        let expected_min = format!("SparseTable({} elements, Min)", expected_len);
        let expected_max = format!("SparseTable({} elements, Max)", expected_len);
        prop_assert_eq!(disp_min, expected_min, "Display min mismatch");
        prop_assert_eq!(disp_max, expected_max, "Display max mismatch");
    }

    // ── build() vs min_table()/max_table() equivalence ──────────

    #[test]
    fn build_vs_convenience_constructors(data in values_strategy(50)) {
        let st_min = SparseTable::min_table(&data);
        let st_build_min = SparseTable::build(&data, QueryOp::Min);
        let st_max = SparseTable::max_table(&data);
        let st_build_max = SparseTable::build(&data, QueryOp::Max);
        let n = data.len();

        // Same length and operation
        prop_assert_eq!(st_min.len(), st_build_min.len());
        prop_assert_eq!(st_min.operation(), st_build_min.operation());
        prop_assert_eq!(st_max.len(), st_build_max.len());
        prop_assert_eq!(st_max.operation(), st_build_max.operation());

        // Same query results across several ranges
        for l in 0..n.min(15) {
            for step in [0, 1, n / 3, n - 1] {
                let r = (l + step).min(n - 1);
                let min_conv = st_min.query(l, r);
                let min_build = st_build_min.query(l, r);
                prop_assert_eq!(min_conv, min_build, "build vs min_table at [{}, {}]", l, r);

                let max_conv = st_max.query(l, r);
                let max_build = st_build_max.query(l, r);
                prop_assert_eq!(max_conv, max_build, "build vs max_table at [{}, {}]", l, r);
            }
        }
    }

    // ── Suffix max is non-decreasing ─────────────────────────────

    #[test]
    fn suffix_max_non_decreasing(data in values_strategy(50)) {
        let st = SparseTable::max_table(&data);
        let n = data.len();

        // max(k, n-1) for decreasing k is non-decreasing
        let mut prev_max = data[n - 1];
        for k in (0..n).rev() {
            let current_max = st.query(k, n - 1);
            prop_assert!(
                current_max >= prev_max,
                "suffix max decreased at k={}: {} < {}", k, current_max, prev_max
            );
            prev_max = current_max;
        }
    }

    // ── IndexSparseTable clone independence ──────────────────────

    #[test]
    fn index_clone_independence(data in values_strategy(50)) {
        let ist = IndexSparseTable::build(&data, QueryOp::Min);
        let cloned = ist.clone();
        let n = data.len();

        // Cloned table has same length
        prop_assert_eq!(cloned.len(), ist.len());

        // Verify queries produce identical results
        for l in 0..n.min(10) {
            for step in [0, 1, n / 3, n - 1] {
                let r = (l + step).min(n - 1);
                let (orig_idx, orig_val) = ist.query(l, r);
                let (clone_idx, clone_val) = cloned.query(l, r);

                // Values must match (indices may differ for ties)
                prop_assert_eq!(
                    *clone_val, *orig_val,
                    "clone value mismatch at [{}, {}]", l, r
                );
                prop_assert_eq!(
                    data[clone_idx], data[orig_idx],
                    "clone index-value mismatch at [{}, {}]", l, r
                );
            }
        }
    }
}
