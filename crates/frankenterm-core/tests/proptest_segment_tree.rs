#![allow(clippy::needless_range_loop, clippy::comparison_chain)]
//! Property-based tests for segment_tree.rs — Lazy Segment Tree.
//!
//! Verifies the Segment Tree invariants:
//! - Query correctness: range sum matches naive computation
//! - Range update correctness: lazy propagation produces correct sums
//! - Point update equivalent to range_update(i, i, delta)
//! - Range decomposition: query(l,r) = query(l,m) + query(m+1,r)
//! - Total sum: total_sum() == query(0, n-1)
//! - Update commutativity: order of non-overlapping updates doesn't matter
//! - point_set correctness: query(i,i) == value after set
//! - to_vec roundtrip: from_slice(to_vec()) preserves queries
//! - Clone equivalence and independence
//! - Reset restores all zeros
//! - Config and stats serde roundtrip
//!
//! Bead: ft-283h4.28

use frankenterm_core::segment_tree::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_values(max_n: usize) -> impl Strategy<Value = Vec<i64>> {
    prop::collection::vec(-1000i64..=1000, 1..=max_n)
}

/// Generate a valid ordered range [l, r] where l <= r and both < n.
#[allow(dead_code)]
fn arb_range(n: usize) -> (usize, usize) {
    // Intentionally not a strategy — used to compute from fractions
    (0, n.saturating_sub(1))
}

// ── Query correctness ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// query(l, r) matches naive sum for all valid ranges.
    #[test]
    fn prop_query_matches_naive(
        values in arb_values(30),
        l_frac in 0.0f64..1.0,
        r_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let a = (l_frac * n as f64) as usize % n;
        let b = (r_frac * n as f64) as usize % n;
        let (l, r) = if a <= b { (a, b) } else { (b, a) };
        let mut st = SegmentTree::from_slice(&values);

        let naive: i64 = values[l..=r].iter().copied().fold(0i64, |a, b| a.wrapping_add(b));
        prop_assert_eq!(st.query(l, r), naive, "query({}, {}) mismatch", l, r);
    }

    /// query(i, i) matches the original value for all i.
    #[test]
    fn prop_point_query_matches_value(
        values in arb_values(30),
        i_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;
        let mut st = SegmentTree::from_slice(&values);
        prop_assert_eq!(st.query(i, i), values[i], "point query mismatch at {}", i);
    }

    /// total_sum() == query(0, n-1).
    #[test]
    fn prop_total_sum_equals_full_query(
        values in arb_values(30),
    ) {
        let mut st = SegmentTree::from_slice(&values);
        let total = st.total_sum();
        let full_query = st.query(0, values.len() - 1);
        prop_assert_eq!(total, full_query, "total_sum != full query");
    }
}

// ── Range decomposition ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// query(l, r) = query(l, m) + query(m+1, r) for any m in [l, r).
    #[test]
    fn prop_range_decomposition(
        values in arb_values(30),
        l_frac in 0.0f64..1.0,
        r_frac in 0.0f64..1.0,
        m_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        prop_assume!(n >= 2);
        let a = (l_frac * n as f64) as usize % n;
        let b = (r_frac * n as f64) as usize % n;
        let (l, r) = if a < b { (a, b) } else if b < a { (b, a) } else {
            // a == b, need l < r
            if a + 1 < n { (a, a + 1) } else { (a.saturating_sub(1), a) }
        };
        prop_assume!(l < r);
        let m = l + ((m_frac * (r - l) as f64) as usize % (r - l)); // m in [l, r)
        let mut st = SegmentTree::from_slice(&values);

        let full = st.query(l, r);
        let left_part = st.query(l, m);
        let right_part = st.query(m + 1, r);
        prop_assert_eq!(
            full,
            left_part.wrapping_add(right_part),
            "decomposition at m={}: query({},{}) != query({},{}) + query({},{})",
            m, l, r, l, m, m + 1, r
        );
    }
}

// ── Point update properties ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After point_update(i, delta), query(l, r) increases by delta if i in [l, r].
    #[test]
    fn prop_point_update_affects_containing_ranges(
        values in arb_values(20),
        i_frac in 0.0f64..1.0,
        delta in -500i64..=500,
        l_frac in 0.0f64..1.0,
        r_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;
        let a = (l_frac * n as f64) as usize % n;
        let b = (r_frac * n as f64) as usize % n;
        let (l, r) = if a <= b { (a, b) } else { (b, a) };
        let mut st = SegmentTree::from_slice(&values);
        let before = st.query(l, r);
        st.point_update(i, delta);
        let after = st.query(l, r);

        if l <= i && i <= r {
            prop_assert_eq!(after, before.wrapping_add(delta),
                "update at {} should affect range [{}, {}]", i, l, r);
        } else {
            prop_assert_eq!(after, before,
                "update at {} should NOT affect range [{}, {}]", i, l, r);
        }
    }

    /// Multiple point updates are additive at the same index.
    #[test]
    fn prop_point_update_additive(
        n in 1usize..=20,
        i_frac in 0.0f64..1.0,
        a in -500i64..=500,
        b in -500i64..=500,
    ) {
        let i = (i_frac * n as f64) as usize % n;

        let mut st1 = SegmentTree::new(n);
        st1.point_update(i, a);
        st1.point_update(i, b);

        let mut st2 = SegmentTree::new(n);
        st2.point_update(i, a.wrapping_add(b));

        for j in 0..n {
            prop_assert_eq!(
                st1.query(j, j), st2.query(j, j),
                "point update additivity violated at {}", j
            );
        }
    }
}

// ── Range update properties ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// range_update(l, r, delta) adds delta to each element in [l, r].
    #[test]
    fn prop_range_update_correct(
        values in arb_values(20),
        l_frac in 0.0f64..1.0,
        r_frac in 0.0f64..1.0,
        delta in -100i64..=100,
    ) {
        let n = values.len();
        let a = (l_frac * n as f64) as usize % n;
        let b = (r_frac * n as f64) as usize % n;
        let (l, r) = if a <= b { (a, b) } else { (b, a) };

        let mut st = SegmentTree::from_slice(&values);
        st.range_update(l, r, delta);

        for i in 0..n {
            let expected = if l <= i && i <= r {
                values[i].wrapping_add(delta)
            } else {
                values[i]
            };
            prop_assert_eq!(st.query(i, i), expected,
                "range_update({}, {}, {}) wrong at index {}", l, r, delta, i);
        }
    }

    /// range_update(i, i, delta) is equivalent to point_update(i, delta).
    #[test]
    fn prop_range_update_single_equals_point(
        values in arb_values(20),
        i_frac in 0.0f64..1.0,
        delta in -500i64..=500,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;

        let mut st_point = SegmentTree::from_slice(&values);
        st_point.point_update(i, delta);

        let mut st_range = SegmentTree::from_slice(&values);
        st_range.range_update(i, i, delta);

        for j in 0..n {
            prop_assert_eq!(
                st_point.query(j, j), st_range.query(j, j),
                "point vs range update mismatch at {}", j
            );
        }
    }

    /// Multiple non-overlapping range updates are order-independent.
    #[test]
    fn prop_nonoverlap_range_updates_commute(
        n in 4usize..=20,
        values in prop::collection::vec(-100i64..=100, 4..=20),
        d1 in -50i64..=50,
        d2 in -50i64..=50,
    ) {
        let n = n.min(values.len());
        let mid = n / 2;
        prop_assume!(mid > 0 && mid < n - 1);

        // Order 1: update left then right
        let mut st1 = SegmentTree::from_slice(&values[..n]);
        st1.range_update(0, mid - 1, d1);
        st1.range_update(mid, n - 1, d2);

        // Order 2: update right then left
        let mut st2 = SegmentTree::from_slice(&values[..n]);
        st2.range_update(mid, n - 1, d2);
        st2.range_update(0, mid - 1, d1);

        for i in 0..n {
            prop_assert_eq!(
                st1.query(i, i), st2.query(i, i),
                "non-overlapping range updates not commutative at {}", i
            );
        }
    }

    /// Overlapping range updates are additive.
    #[test]
    fn prop_overlapping_range_updates_additive(
        n in 2usize..=15,
        values in prop::collection::vec(-100i64..=100, 2..=15),
        d1 in -50i64..=50,
        d2 in -50i64..=50,
    ) {
        let n = n.min(values.len());

        // Two full-range updates
        let mut st1 = SegmentTree::from_slice(&values[..n]);
        st1.range_update(0, n - 1, d1);
        st1.range_update(0, n - 1, d2);

        // One combined update
        let mut st2 = SegmentTree::from_slice(&values[..n]);
        st2.range_update(0, n - 1, d1.wrapping_add(d2));

        for i in 0..n {
            prop_assert_eq!(
                st1.query(i, i), st2.query(i, i),
                "overlapping range updates not additive at {}", i
            );
        }
    }
}

// ── point_set properties ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// point_set(i, v) makes query(i, i) == v.
    #[test]
    fn prop_point_set_correct(
        values in arb_values(20),
        i_frac in 0.0f64..1.0,
        new_val in -500i64..=500,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;

        let mut st = SegmentTree::from_slice(&values);
        st.point_set(i, new_val);
        prop_assert_eq!(st.query(i, i), new_val, "point_set({}, {}) not reflected", i, new_val);
    }

    /// point_set doesn't affect other elements.
    #[test]
    fn prop_point_set_isolated(
        values in arb_values(15),
        i_frac in 0.0f64..1.0,
        new_val in -500i64..=500,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;

        let mut st = SegmentTree::from_slice(&values);
        st.point_set(i, new_val);

        for j in 0..n {
            if j != i {
                prop_assert_eq!(st.query(j, j), values[j],
                    "point_set({}, {}) affected element {}", i, new_val, j);
            }
        }
    }
}

// ── to_vec roundtrip ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// from_slice(st.to_vec()) preserves all queries.
    #[test]
    fn prop_to_vec_roundtrip(
        values in arb_values(30),
    ) {
        let mut st1 = SegmentTree::from_slice(&values);
        let recovered = st1.to_vec();
        let mut st2 = SegmentTree::from_slice(&recovered);

        for i in 0..values.len() {
            prop_assert_eq!(
                st1.query(i, i), st2.query(i, i),
                "to_vec roundtrip mismatch at {}", i
            );
        }
    }

    /// to_vec returns the original values.
    #[test]
    fn prop_to_vec_recovers_values(
        values in arb_values(30),
    ) {
        let mut st = SegmentTree::from_slice(&values);
        prop_assert_eq!(st.to_vec(), values);
    }

    /// to_vec reflects range updates.
    #[test]
    fn prop_to_vec_after_range_update(
        values in arb_values(20),
        l_frac in 0.0f64..1.0,
        r_frac in 0.0f64..1.0,
        delta in -100i64..=100,
    ) {
        let n = values.len();
        let a = (l_frac * n as f64) as usize % n;
        let b = (r_frac * n as f64) as usize % n;
        let (l, r) = if a <= b { (a, b) } else { (b, a) };

        let mut st = SegmentTree::from_slice(&values);
        st.range_update(l, r, delta);
        let result = st.to_vec();

        for i in 0..n {
            let expected = if l <= i && i <= r {
                values[i].wrapping_add(delta)
            } else {
                values[i]
            };
            prop_assert_eq!(result[i], expected,
                "to_vec after range_update wrong at {}", i);
        }
    }
}

// ── Reset properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// reset() zeroes all elements.
    #[test]
    fn prop_reset_zeroes(
        values in arb_values(30),
    ) {
        let mut st = SegmentTree::from_slice(&values);
        st.reset();
        prop_assert_eq!(st.total_sum(), 0, "total_sum not zero after reset");
        prop_assert_eq!(st.len(), values.len(), "len changed after reset");
        for i in 0..values.len() {
            prop_assert_eq!(st.query(i, i), 0, "element {} not zero after reset", i);
        }
    }
}

// ── Clone properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces identical queries.
    #[test]
    fn prop_clone_equivalence(
        values in arb_values(30),
    ) {
        let mut st = SegmentTree::from_slice(&values);
        let mut clone = st.clone();

        for i in 0..values.len() {
            prop_assert_eq!(
                st.query(i, i), clone.query(i, i),
                "clone mismatch at {}", i
            );
        }
    }

    /// Mutations to clone don't affect original.
    #[test]
    fn prop_clone_independence(
        values in arb_values(20),
    ) {
        let n = values.len();
        let mut st = SegmentTree::from_slice(&values);
        let original_total = st.total_sum();

        let mut clone = st.clone();
        clone.range_update(0, n - 1, 999);

        prop_assert_eq!(st.total_sum(), original_total, "original modified by clone mutation");
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SegmentTreeConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(cap in 0usize..1000) {
        let config = SegmentTreeConfig { capacity: cap };
        let json = serde_json::to_string(&config).unwrap();
        let back: SegmentTreeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// SegmentTreeStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(
        values in arb_values(20),
    ) {
        let mut st = SegmentTree::from_slice(&values);
        st.query(0, values.len() - 1);
        st.point_update(0, 1);
        let stats = st.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: SegmentTreeStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// Stats fields are consistent with tree state.
    #[test]
    fn prop_stats_consistent(
        values in arb_values(20),
    ) {
        let mut st = SegmentTree::from_slice(&values);
        let stats = st.stats();
        prop_assert_eq!(stats.element_count, st.len());
        prop_assert_eq!(stats.memory_bytes, st.memory_bytes());
    }
}

// ── Empty tree properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty tree invariants.
    #[test]
    fn prop_empty_invariants(_dummy in 0..1u8) {
        let mut st = SegmentTree::new(0);
        prop_assert!(st.is_empty());
        prop_assert_eq!(st.len(), 0);
        prop_assert_eq!(st.total_sum(), 0);
    }
}

// ── Memory properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Memory scales with n.
    #[test]
    fn prop_memory_scales(
        n1 in 1usize..=50,
        n2 in 1usize..=50,
    ) {
        let st1 = SegmentTree::new(n1);
        let st2 = SegmentTree::new(n2);
        if n2 > n1 {
            prop_assert!(st2.memory_bytes() > st1.memory_bytes(), "memory should scale with n");
        }
    }

    /// new(n) creates all-zero tree.
    #[test]
    fn prop_new_all_zeros(n in 1usize..=30) {
        let mut st = SegmentTree::new(n);
        for i in 0..n {
            prop_assert_eq!(st.query(i, i), 0, "new tree not zero at {}", i);
        }
        prop_assert_eq!(st.total_sum(), 0);
    }

    /// len() returns the size.
    #[test]
    fn prop_len_correct(values in arb_values(30)) {
        let st = SegmentTree::from_slice(&values);
        prop_assert_eq!(st.len(), values.len());
    }

    /// is_empty agrees with len.
    #[test]
    fn prop_is_empty_agrees(n in 0usize..=10) {
        let st = SegmentTree::new(n);
        prop_assert_eq!(st.is_empty(), n == 0);
        prop_assert_eq!(st.is_empty(), st.is_empty());
    }

    /// Reset then insert works.
    #[test]
    fn prop_reset_then_update(
        values in arb_values(15),
        i_frac in 0.0f64..1.0,
        delta in -500i64..=500,
    ) {
        let n = values.len();
        let i = (i_frac * n as f64) as usize % n;
        let mut st = SegmentTree::from_slice(&values);
        st.reset();
        st.point_update(i, delta);
        prop_assert_eq!(st.query(i, i), delta);
        prop_assert_eq!(st.total_sum(), delta);
    }

    /// point_set then point_update combines correctly.
    #[test]
    fn prop_set_then_update(
        n in 1usize..=15,
        i_frac in 0.0f64..1.0,
        val in -500i64..=500,
        delta in -500i64..=500,
    ) {
        let i = (i_frac * n as f64) as usize % n;
        let mut st = SegmentTree::new(n);
        st.point_set(i, val);
        st.point_update(i, delta);
        prop_assert_eq!(st.query(i, i), val.wrapping_add(delta));
    }

    /// to_vec length matches len().
    #[test]
    fn prop_to_vec_len(values in arb_values(30)) {
        let mut st = SegmentTree::from_slice(&values);
        prop_assert_eq!(st.to_vec().len(), st.len());
    }

    /// Clone after updates still correct.
    #[test]
    fn prop_clone_after_updates(
        values in arb_values(15),
        delta in -100i64..=100,
    ) {
        let n = values.len();
        let mut st = SegmentTree::from_slice(&values);
        st.range_update(0, n - 1, delta);
        let mut clone = st.clone();
        for i in 0..n {
            prop_assert_eq!(st.query(i, i), clone.query(i, i));
        }
    }
}

// ── Additional behavioral invariants ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Range update with delta=0 is a no-op.
    #[test]
    fn prop_zero_delta_noop(values in arb_values(20)) {
        let n = values.len();
        let mut st = SegmentTree::from_slice(&values);
        let before = st.total_sum();
        st.range_update(0, n - 1, 0);
        prop_assert_eq!(st.total_sum(), before);
    }

    /// point_set followed by point query returns the set value.
    #[test]
    fn prop_point_set_get(
        values in arb_values(20),
        new_val in -1000i64..=1000,
        idx_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let idx = (idx_frac * (n - 1) as f64) as usize;
        let mut st = SegmentTree::from_slice(&values);
        st.point_set(idx, new_val);
        prop_assert_eq!(st.query(idx, idx), new_val);
    }

    /// Double range update is additive.
    #[test]
    fn prop_double_update_additive(
        values in arb_values(15),
        d1 in -50i64..=50,
        d2 in -50i64..=50,
    ) {
        let n = values.len();
        let mut st = SegmentTree::from_slice(&values);
        st.range_update(0, n - 1, d1);
        st.range_update(0, n - 1, d2);
        let expected: i64 = values.iter().sum::<i64>() + (d1 + d2) * n as i64;
        prop_assert_eq!(st.total_sum(), expected);
    }

    /// SegmentTreeConfig serde JSON has expected field.
    #[test]
    fn prop_config_json_key(cap in 1usize..1000) {
        let config = SegmentTreeConfig { capacity: cap };
        let json = serde_json::to_string(&config).unwrap();
        prop_assert!(json.contains("\"capacity\""));
    }

    /// Stats len matches values.len() after construction.
    #[test]
    fn prop_stats_len_matches(values in arb_values(30)) {
        let mut st = SegmentTree::from_slice(&values);
        let stats = st.stats();
        prop_assert_eq!(stats.element_count, values.len());
    }

    /// Stats memory_bytes is positive.
    #[test]
    fn prop_stats_memory_positive(values in arb_values(20)) {
        let mut st = SegmentTree::from_slice(&values);
        let stats = st.stats();
        prop_assert!(stats.memory_bytes > 0, "memory should be positive");
    }

    /// Single-element query equals the element.
    #[test]
    fn prop_single_query_element(
        values in arb_values(20),
        idx_frac in 0.0f64..1.0,
    ) {
        let n = values.len();
        let idx = (idx_frac * (n - 1) as f64) as usize;
        let mut st = SegmentTree::from_slice(&values);
        prop_assert_eq!(st.query(idx, idx), values[idx]);
    }
}
