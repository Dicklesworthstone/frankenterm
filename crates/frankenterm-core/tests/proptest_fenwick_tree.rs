//! Property-based tests for fenwick_tree.rs — Binary Indexed Tree.
//!
//! Verifies the Fenwick Tree invariants:
//! - Prefix sum correctness: matches naive sum
//! - Range sum decomposition: range_sum(l,r) == prefix_sum(r) - prefix_sum(l-1)
//! - Update additivity: update(i, a); update(i, b) == update(i, a+b)
//! - Point query consistency: point_query(i) recovers original value
//! - from_slice equivalence: matches incremental construction
//! - Total sum: total_sum() == prefix_sum(n-1)
//! - find_kth correctness: for non-negative values, finds correct rank
//! - find_kth monotonicity: larger target => larger or equal index
//! - Merge commutativity: merge order doesn't affect result
//! - Clone equivalence and independence
//! - Reset restores all zeros
//! - Config and stats serde roundtrip
//! - to_vec roundtrip: from_slice(to_vec()) preserves prefix sums
//!
//! Bead: ft-283h4.26

use frankenterm_core::fenwick_tree::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_size() -> impl Strategy<Value = usize> {
    1usize..=50
}

// ── Prefix sum correctness ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// prefix_sum matches naive cumulative sum for all indices.
    #[test]
    fn prop_prefix_sum_matches_naive(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let vals = &values[..n];
        let ft = FenwickTree::from_slice(vals);

        let mut naive_sum = 0i64;
        for i in 0..n {
            naive_sum = naive_sum.wrapping_add(vals[i]);
            prop_assert_eq!(
                ft.prefix_sum(i), naive_sum,
                "prefix_sum({}) mismatch: fenwick={}, naive={}",
                i, ft.prefix_sum(i), naive_sum
            );
        }
    }

    /// prefix_sum after updates matches naive computation.
    #[test]
    fn prop_prefix_sum_after_updates(
        n in 1usize..=30,
        updates in prop::collection::vec((0usize..30, -500i64..=500), 0..30),
    ) {
        let mut naive = vec![0i64; n];
        let mut ft = FenwickTree::new(n);

        for &(idx, delta) in &updates {
            if idx < n {
                naive[idx] = naive[idx].wrapping_add(delta);
                ft.update(idx, delta);
            }
        }

        let mut naive_sum = 0i64;
        for i in 0..n {
            naive_sum = naive_sum.wrapping_add(naive[i]);
            prop_assert_eq!(
                ft.prefix_sum(i), naive_sum,
                "prefix_sum({}) after updates mismatch", i
            );
        }
    }
}

// ── Range sum properties ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// range_sum(l, r) == prefix_sum(r) - prefix_sum(l-1) when l > 0.
    #[test]
    fn prop_range_sum_decomposition(
        values in prop::collection::vec(-1000i64..=1000, 2..=30),
        l_frac in 0.0f64..1.0,
        r_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let a = (l_frac * n as f64) as usize % n;
        let b = (r_frac * n as f64) as usize % n;
        let (l, r) = if a <= b { (a, b) } else { (b, a) };
        let ft = FenwickTree::from_slice(&values[..n]);

        let range = ft.range_sum(l, r);
        let expected = if l == 0 {
            ft.prefix_sum(r)
        } else {
            ft.prefix_sum(r).wrapping_sub(ft.prefix_sum(l - 1))
        };
        prop_assert_eq!(range, expected, "range_sum({}, {}) decomposition mismatch", l, r);
    }

    /// range_sum(i, i) == point_query(i) for all i.
    #[test]
    fn prop_range_sum_single_equals_point(
        values in prop::collection::vec(-1000i64..=1000, 1..=30),
        i_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;
        let ft = FenwickTree::from_slice(&values[..n]);

        prop_assert_eq!(
            ft.range_sum(i, i), ft.point_query(i),
            "range_sum({i}, {i}) != point_query({i})",
            i = i
        );
    }

    /// range_sum(0, n-1) == total_sum().
    #[test]
    fn prop_full_range_equals_total(
        n in 1usize..=30,
        values in prop::collection::vec(-1000i64..=1000, 1..=30),
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);
        prop_assert_eq!(ft.range_sum(0, n - 1), ft.total_sum());
    }
}

// ── Update properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Two updates to same index are additive.
    #[test]
    fn prop_update_additivity(
        n in 1usize..=30,
        i_frac in 0.0f64..1.0,
        a in -500i64..=500,
        b in -500i64..=500,
    ) {
        let i = (i_frac * n as f64) as usize % n;

        // Two separate updates
        let mut ft1 = FenwickTree::new(n);
        ft1.update(i, a);
        ft1.update(i, b);

        // Single combined update
        let mut ft2 = FenwickTree::new(n);
        ft2.update(i, a.wrapping_add(b));

        for j in 0..n {
            prop_assert_eq!(
                ft1.prefix_sum(j), ft2.prefix_sum(j),
                "update additivity violated at index {}", j
            );
        }
    }

    /// Update order doesn't matter for final state (commutativity).
    #[test]
    fn prop_update_order_independent(
        n in 2usize..=20,
        updates in prop::collection::vec((0usize..20, -500i64..=500), 2..10),
    ) {
        let valid: Vec<_> = updates.iter().filter(|&&(i, _)| i < n).copied().collect();
        prop_assume!(valid.len() >= 2);

        let mut ft1 = FenwickTree::new(n);
        for &(i, d) in &valid {
            ft1.update(i, d);
        }

        // Reverse order
        let mut ft2 = FenwickTree::new(n);
        for &(i, d) in valid.iter().rev() {
            ft2.update(i, d);
        }

        for j in 0..n {
            prop_assert_eq!(
                ft1.prefix_sum(j), ft2.prefix_sum(j),
                "update commutativity violated at index {}", j
            );
        }
    }
}

// ── Point query properties ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// point_query recovers the values passed to from_slice.
    #[test]
    fn prop_point_query_recovers_values(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let vals = &values[..n];
        let ft = FenwickTree::from_slice(vals);

        for i in 0..n {
            prop_assert_eq!(
                ft.point_query(i), vals[i],
                "point_query({}) != original value", i
            );
        }
    }

    /// Sum of all point_query values equals total_sum.
    #[test]
    fn prop_point_queries_sum_to_total(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);

        let point_sum: i64 = (0..n).map(|i| ft.point_query(i)).fold(0i64, |a, b| a.wrapping_add(b));
        prop_assert_eq!(point_sum, ft.total_sum(), "sum of point queries != total_sum");
    }
}

// ── from_slice equivalence ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// from_slice produces identical prefix sums as incremental updates.
    #[test]
    fn prop_from_slice_matches_incremental(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let vals = &values[..n];

        let ft_slice = FenwickTree::from_slice(vals);
        let mut ft_inc = FenwickTree::new(n);
        for (i, &v) in vals.iter().enumerate() {
            ft_inc.update(i, v);
        }

        for i in 0..n {
            prop_assert_eq!(
                ft_slice.prefix_sum(i), ft_inc.prefix_sum(i),
                "from_slice vs incremental mismatch at {}", i
            );
        }
    }
}

// ── to_vec roundtrip ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// from_slice(ft.to_vec()) preserves all prefix sums.
    #[test]
    fn prop_to_vec_roundtrip(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let ft1 = FenwickTree::from_slice(&values[..n]);
        let recovered = ft1.to_vec();
        let ft2 = FenwickTree::from_slice(&recovered);

        for i in 0..n {
            prop_assert_eq!(
                ft1.prefix_sum(i), ft2.prefix_sum(i),
                "to_vec roundtrip mismatch at {}", i
            );
        }
    }

    /// to_vec returns the original values.
    #[test]
    fn prop_to_vec_recovers_values(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let vals = &values[..n];
        let ft = FenwickTree::from_slice(vals);
        prop_assert_eq!(ft.to_vec(), vals.to_vec());
    }
}

// ── find_kth properties ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// find_kth returns correct index for non-negative values.
    #[test]
    fn prop_find_kth_correct(
        n in 1usize..=30,
        values in prop::collection::vec(0i64..=50, 1..=30),
        target in 0i64..=500,
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);

        match ft.find_kth(target) {
            Some(idx) => {
                // prefix_sum(idx) >= target
                prop_assert!(
                    ft.prefix_sum(idx) >= target,
                    "find_kth({}) = {}, but prefix_sum({}) = {} < target",
                    target, idx, idx, ft.prefix_sum(idx)
                );
                // And it's the smallest such index
                if idx > 0 {
                    prop_assert!(
                        ft.prefix_sum(idx - 1) < target,
                        "find_kth({}) = {}, but prefix_sum({}) = {} >= target (not minimal)",
                        target, idx, idx - 1, ft.prefix_sum(idx - 1)
                    );
                }
            }
            None => {
                // total sum < target
                prop_assert!(
                    ft.total_sum() < target,
                    "find_kth({}) returned None, but total_sum {} >= target",
                    target, ft.total_sum()
                );
            }
        }
    }

    /// find_kth is monotonic: larger target => larger or equal index.
    #[test]
    fn prop_find_kth_monotonic(
        n in 1usize..=20,
        values in prop::collection::vec(1i64..=20, 1..=20),
        t1 in 1i64..=100,
        t2 in 1i64..=100,
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);

        let (lo, hi) = if t1 <= t2 { (t1, t2) } else { (t2, t1) };
        match (ft.find_kth(lo), ft.find_kth(hi)) {
            (Some(i_lo), Some(i_hi)) => {
                prop_assert!(
                    i_lo <= i_hi,
                    "find_kth not monotonic: find_kth({}) = {} > find_kth({}) = {}",
                    lo, i_lo, hi, i_hi
                );
            }
            (Some(_), None) => {
                // hi too large, lo found — consistent
            }
            (None, _) => {
                // If smaller target not found, larger shouldn't be either
                let is_none = ft.find_kth(hi).is_none();
                prop_assert!(
                    is_none,
                    "find_kth({}) = None but find_kth({}) found something", lo, hi
                );
            }
        }
    }

    /// find_kth(0) returns Some(0) when tree has non-negative values.
    #[test]
    fn prop_find_kth_zero(
        n in 1usize..=20,
        values in prop::collection::vec(0i64..=50, 1..=20),
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);
        prop_assert_eq!(ft.find_kth(0), Some(0), "find_kth(0) should return Some(0)");
    }
}

// ── Merge properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Merge is commutative for prefix sums.
    #[test]
    fn prop_merge_commutative(
        n in 1usize..=20,
        vals_a in prop::collection::vec(-100i64..=100, 1..=20),
        vals_b in prop::collection::vec(-100i64..=100, 1..=20),
    ) {
        let n = n.min(vals_a.len()).min(vals_b.len());
        let a = &vals_a[..n];
        let b = &vals_b[..n];

        // a + b
        let mut ft_ab = FenwickTree::from_slice(a);
        ft_ab.merge(&FenwickTree::from_slice(b));

        // b + a
        let mut ft_ba = FenwickTree::from_slice(b);
        ft_ba.merge(&FenwickTree::from_slice(a));

        for i in 0..n {
            prop_assert_eq!(
                ft_ab.prefix_sum(i), ft_ba.prefix_sum(i),
                "merge commutativity violated at index {}", i
            );
        }
    }

    /// Merge with zero tree is identity.
    #[test]
    fn prop_merge_identity(
        n in 1usize..=20,
        values in prop::collection::vec(-100i64..=100, 1..=20),
    ) {
        let n = n.min(values.len());
        let vals = &values[..n];

        let original = FenwickTree::from_slice(vals);
        let mut merged = FenwickTree::from_slice(vals);
        merged.merge(&FenwickTree::new(n));

        for i in 0..n {
            prop_assert_eq!(
                original.prefix_sum(i), merged.prefix_sum(i),
                "merge with zero tree changed prefix sums at {}", i
            );
        }
    }

    /// Merged tree values are element-wise sums.
    #[test]
    fn prop_merge_elementwise(
        n in 1usize..=20,
        vals_a in prop::collection::vec(-100i64..=100, 1..=20),
        vals_b in prop::collection::vec(-100i64..=100, 1..=20),
    ) {
        let n = n.min(vals_a.len()).min(vals_b.len());
        let a = &vals_a[..n];
        let b = &vals_b[..n];

        let mut ft = FenwickTree::from_slice(a);
        ft.merge(&FenwickTree::from_slice(b));

        for i in 0..n {
            let expected = a[i].wrapping_add(b[i]);
            prop_assert_eq!(
                ft.point_query(i), expected,
                "merge element-wise sum wrong at {}", i
            );
        }
    }
}

// ── Set properties ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// set(i, v) makes point_query(i) == v.
    #[test]
    fn prop_set_then_query(
        initial in prop::collection::vec(-100i64..=100, 1..=20),
        i_frac in 0.0f64..1.0,
        new_val in -500i64..=500,
    ) {
        let n = initial.len();
        let i = (i_frac * n as f64) as usize % n;

        let mut ft = FenwickTree::from_slice(&initial[..n]);
        ft.set(i, new_val);
        prop_assert_eq!(ft.point_query(i), new_val, "set({}, {}) not reflected in point_query", i, new_val);
    }

    /// set doesn't affect other elements.
    #[test]
    fn prop_set_isolated(
        values in prop::collection::vec(-100i64..=100, 2..=15),
        i_frac in 0.0f64..1.0,
        new_val in -500i64..=500,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;

        let mut ft = FenwickTree::from_slice(&values[..n]);
        ft.set(i, new_val);

        for j in 0..n {
            if j != i {
                prop_assert_eq!(
                    ft.point_query(j), values[j],
                    "set({}, {}) affected element {}", i, new_val, j
                );
            }
        }
    }
}

// ── Reset properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// reset() zeroes all prefix sums.
    #[test]
    fn prop_reset_zeroes(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let mut ft = FenwickTree::from_slice(&values[..n]);
        ft.reset();

        prop_assert_eq!(ft.total_sum(), 0, "total_sum not zero after reset");
        prop_assert_eq!(ft.len(), n, "len changed after reset");
        for i in 0..n {
            prop_assert_eq!(ft.point_query(i), 0, "element {} not zero after reset", i);
        }
    }
}

// ── Clone properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces identical prefix sums.
    #[test]
    fn prop_clone_equivalence(
        n in arb_size(),
        values in prop::collection::vec(-1000i64..=1000, 1..=50),
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);
        let clone = ft.clone();

        for i in 0..n {
            prop_assert_eq!(
                ft.prefix_sum(i), clone.prefix_sum(i),
                "clone prefix_sum mismatch at {}", i
            );
        }
    }

    /// Mutations to clone don't affect original.
    #[test]
    fn prop_clone_independence(
        n in 1usize..=20,
        values in prop::collection::vec(-100i64..=100, 1..=20),
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);
        let original_total = ft.total_sum();

        let mut clone = ft.clone();
        for i in 0..n {
            clone.update(i, 999);
        }

        prop_assert_eq!(ft.total_sum(), original_total, "original modified by clone mutation");
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// FenwickConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(cap in 0usize..1000) {
        let config = FenwickConfig { capacity: cap };
        let json = serde_json::to_string(&config).unwrap();
        let back: FenwickConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// FenwickStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(
        n in 1usize..=20,
        values in prop::collection::vec(-100i64..=100, 1..=20),
    ) {
        let n = n.min(values.len());
        let mut ft = FenwickTree::from_slice(&values[..n]);
        // Do some updates to populate stats counters
        for i in 0..n {
            ft.update(i, 1);
        }
        let stats = ft.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: FenwickStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// Stats fields are consistent with tree state.
    #[test]
    fn prop_stats_consistent(
        n in 1usize..=20,
        values in prop::collection::vec(-100i64..=100, 1..=20),
    ) {
        let n = n.min(values.len());
        let ft = FenwickTree::from_slice(&values[..n]);
        let stats = ft.stats();

        prop_assert_eq!(stats.element_count, ft.len());
        prop_assert_eq!(stats.total_sum, ft.total_sum());
        prop_assert_eq!(stats.memory_bytes, ft.memory_bytes());
    }
}

// ── Empty tree properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty tree invariants hold.
    #[test]
    fn prop_empty_invariants(_dummy in 0..1u8) {
        let ft = FenwickTree::new(0);
        prop_assert!(ft.is_empty());
        prop_assert_eq!(ft.len(), 0);
        prop_assert_eq!(ft.total_sum(), 0);
        let is_none = ft.find_kth(1).is_none();
        prop_assert!(is_none, "find_kth on empty tree should return None");
    }
}

// ── Memory properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// is_empty agrees with len == 0.
    #[test]
    fn prop_is_empty_agrees(
        n in 0usize..=30,
        values in prop::collection::vec(-100i64..=100, 0..=30),
    ) {
        let n = n.min(values.len());
        let ft = if n == 0 { FenwickTree::new(0) } else { FenwickTree::from_slice(&values[..n]) };
        prop_assert_eq!(ft.is_empty(), ft.len() == 0);
    }

    /// from_config produces tree with correct len.
    #[test]
    fn prop_from_config_len(cap in 0usize..=100) {
        let config = FenwickConfig { capacity: cap };
        let ft = FenwickTree::from_config(&config);
        prop_assert_eq!(ft.len(), cap);
        prop_assert_eq!(ft.total_sum(), 0);
    }

    /// Memory scales linearly with n.
    #[test]
    fn prop_memory_scales(
        n1 in 1usize..=50,
        n2 in 1usize..=50,
    ) {
        let ft1 = FenwickTree::new(n1);
        let ft2 = FenwickTree::new(n2);
        if n2 > n1 {
            prop_assert!(ft2.memory_bytes() > ft1.memory_bytes(), "memory should scale with n");
        }
    }
}
