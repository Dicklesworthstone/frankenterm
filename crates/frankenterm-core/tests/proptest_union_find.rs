//! Property-based tests for union_find.rs — Disjoint Set Union data structure.
//!
//! Verifies the Union-Find invariants:
//! - Idempotence: find(x) == find(find(x))
//! - Reflexivity: connected(x, x) is always true
//! - Symmetry: connected(x, y) == connected(y, x)
//! - Transitivity: connected(a,b) && connected(b,c) => connected(a,c)
//! - Union commutativity: union(a,b) == union(b,a) for connectivity
//! - Component count monotonicity: union never increases component_count
//! - Component count bounds: 1 <= component_count <= n
//! - Component sizes sum to n
//! - make_set grows length by 1
//! - reset restores all singletons
//! - Clone equivalence and independence
//! - Config and stats serde roundtrip
//!
//! Bead: ft-283h4.23

use frankenterm_core::union_find::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_size() -> impl Strategy<Value = usize> {
    1usize..=50
}

fn arb_union_ops(n: usize) -> impl Strategy<Value = Vec<(usize, usize)>> {
    prop::collection::vec((0usize..n, 0usize..n), 0..n * 2)
}

fn arb_union_find(max_n: usize) -> impl Strategy<Value = (usize, Vec<(usize, usize)>)> {
    arb_size().prop_flat_map(move |n| {
        let n = n.min(max_n);
        arb_union_ops(n).prop_map(move |ops| (n, ops))
    })
}

fn build_uf(n: usize, ops: &[(usize, usize)]) -> UnionFind {
    let mut uf = UnionFind::new(n);
    for &(x, y) in ops {
        uf.union(x, y);
    }
    uf
}

/// Map a fraction [0.0, 1.0) to an index in [0, n).
fn frac_to_idx(frac: f64, n: usize) -> usize {
    (frac * n as f64) as usize % n
}

// ── Find idempotence ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// find(x) == find(find(x)) for any x after any sequence of unions.
    #[test]
    fn prop_find_idempotent(
        (n, ops) in arb_union_find(30),
    ) {
        let mut uf = build_uf(n, &ops);
        for x in 0..n {
            let r1 = uf.find(x);
            let r2 = uf.find(r1);
            prop_assert_eq!(r1, r2, "find is not idempotent for {}", x);
        }
    }

    /// Immutable find agrees with mutable find.
    #[test]
    fn prop_immutable_find_agrees(
        (n, ops) in arb_union_find(30),
    ) {
        let mut uf = build_uf(n, &ops);
        for x in 0..n {
            let mutable_root = uf.find(x);
            let immutable_root = uf.find_immutable(x);
            prop_assert_eq!(mutable_root, immutable_root, "mutable and immutable find disagree for {}", x);
        }
    }
}

// ── Connectivity properties ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// connected(x, x) is always true (reflexivity).
    #[test]
    fn prop_connected_reflexive(
        (n, ops) in arb_union_find(30),
    ) {
        let mut uf = build_uf(n, &ops);
        for x in 0..n {
            prop_assert!(uf.connected(x, x), "element {} should be connected to itself", x);
        }
    }

    /// connected(x, y) == connected(y, x) (symmetry).
    #[test]
    fn prop_connected_symmetric(
        (n, ops) in arb_union_find(20),
        x_frac in 0.0f64..1.0,
        y_frac in 0.0f64..1.0,
    ) {
        let x = frac_to_idx(x_frac, n);
        let y = frac_to_idx(y_frac, n);
        let mut uf = build_uf(n, &ops);
        let xy = uf.connected(x, y);
        let yx = uf.connected(y, x);
        prop_assert_eq!(xy, yx, "connected({}, {}) != connected({}, {})", x, y, y, x);
    }

    /// If connected(a,b) and connected(b,c), then connected(a,c) (transitivity).
    #[test]
    fn prop_connected_transitive(
        (n, ops) in arb_union_find(20),
        a_frac in 0.0f64..1.0,
        b_frac in 0.0f64..1.0,
        c_frac in 0.0f64..1.0,
    ) {
        let a = frac_to_idx(a_frac, n);
        let b = frac_to_idx(b_frac, n);
        let c = frac_to_idx(c_frac, n);
        let mut uf = build_uf(n, &ops);
        if uf.connected(a, b) && uf.connected(b, c) {
            prop_assert!(uf.connected(a, c), "transitivity violated: connected({},{}) && connected({},{}) but not connected({},{})", a, b, b, c, a, c);
        }
    }
}

// ── Union properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// union(x, y) makes x and y connected.
    #[test]
    fn prop_union_connects(
        n in arb_size(),
        x_frac in 0.0f64..1.0,
        y_frac in 0.0f64..1.0,
    ) {
        let x = frac_to_idx(x_frac, n);
        let y = frac_to_idx(y_frac, n);
        let mut uf = UnionFind::new(n);
        uf.union(x, y);
        prop_assert!(uf.connected(x, y));
    }

    /// union(x, y) and union(y, x) produce same connectivity.
    #[test]
    fn prop_union_commutative(
        n in 2usize..=30,
        ops in prop::collection::vec((0usize..30, 0usize..30), 0..20),
        qx_frac in 0.0f64..1.0,
        qy_frac in 0.0f64..1.0,
    ) {
        let query_x = frac_to_idx(qx_frac, n);
        let query_y = frac_to_idx(qy_frac, n);
        let valid_ops: Vec<(usize, usize)> = ops.iter()
            .filter(|&&(a, b)| a < n && b < n)
            .copied()
            .collect();

        let mut uf1 = UnionFind::new(n);
        let mut uf2 = UnionFind::new(n);
        for &(a, b) in &valid_ops {
            uf1.union(a, b);
            uf2.union(b, a); // reversed order
        }
        prop_assert_eq!(
            uf1.connected(query_x, query_y),
            uf2.connected(query_x, query_y),
            "union commutativity violated"
        );
    }
}

// ── Component count properties ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// component_count starts at n and never increases.
    #[test]
    fn prop_component_count_monotone(
        n in arb_size(),
        ops in prop::collection::vec((0usize..50, 0usize..50), 0..50),
    ) {
        let mut uf = UnionFind::new(n);
        let mut prev_count = n;
        for &(x, y) in &ops {
            if x < n && y < n {
                uf.union(x, y);
                let cur = uf.component_count();
                prop_assert!(cur <= prev_count, "component count increased: {} -> {}", prev_count, cur);
                prev_count = cur;
            }
        }
    }

    /// component_count is always >= 1 and <= n for non-empty UF.
    #[test]
    fn prop_component_count_bounds(
        (n, ops) in arb_union_find(30),
    ) {
        let uf = build_uf(n, &ops);
        prop_assert!(uf.component_count() >= 1, "component count < 1");
        prop_assert!(uf.component_count() <= n, "component count > n");
    }

    /// component_count decreases by exactly 1 on successful union.
    #[test]
    fn prop_successful_union_decrements(
        n in 3usize..=30,
        x_frac in 0.0f64..1.0,
        y_frac in 0.0f64..1.0,
    ) {
        let x = frac_to_idx(x_frac, n);
        let y = frac_to_idx(y_frac, n);
        prop_assume!(x != y);
        let mut uf = UnionFind::new(n);
        let before = uf.component_count();
        let merged = uf.union(x, y);
        if merged {
            prop_assert_eq!(uf.component_count(), before - 1);
        } else {
            prop_assert_eq!(uf.component_count(), before);
        }
    }
}

// ── Component size properties ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Sum of all component sizes equals n.
    #[test]
    fn prop_sizes_sum_to_n(
        (n, ops) in arb_union_find(30),
    ) {
        let mut uf = build_uf(n, &ops);
        let comps = uf.all_components();
        let total: usize = comps.iter().map(|c| c.len()).sum();
        prop_assert_eq!(total, n, "component sizes don't sum to n");
    }

    /// Number of all_components equals component_count.
    #[test]
    fn prop_all_components_count(
        (n, ops) in arb_union_find(30),
    ) {
        let mut uf = build_uf(n, &ops);
        let expected = uf.component_count();
        let comps = uf.all_components();
        prop_assert_eq!(comps.len(), expected, "all_components count mismatch");
    }

    /// component_size(x) == component_members(x).len() for all x.
    #[test]
    fn prop_component_size_matches_members(
        (n, ops) in arb_union_find(20),
        x_frac in 0.0f64..1.0,
    ) {
        let x = frac_to_idx(x_frac, n);
        let mut uf = build_uf(n, &ops);
        let size = uf.component_size(x);
        let members = uf.component_members(x);
        prop_assert_eq!(size, members.len(), "size/members mismatch for element {}", x);
    }
}

// ── make_set properties ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// make_set increases len() by 1 and component_count by 1.
    #[test]
    fn prop_make_set_grows(
        (n, ops) in arb_union_find(20),
    ) {
        let mut uf = build_uf(n, &ops);
        let old_len = uf.len();
        let old_comps = uf.component_count();
        let idx = uf.make_set();
        prop_assert_eq!(idx, old_len);
        prop_assert_eq!(uf.len(), old_len + 1);
        prop_assert_eq!(uf.component_count(), old_comps + 1);
    }

    /// New element from make_set is not connected to any existing element.
    #[test]
    fn prop_make_set_isolated(
        (n, ops) in arb_union_find(20),
    ) {
        let mut uf = build_uf(n, &ops);
        let new_idx = uf.make_set();
        for i in 0..n {
            prop_assert!(!uf.connected(i, new_idx), "new element {} should not be connected to {}", new_idx, i);
        }
    }
}

// ── Reset properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// reset() restores component_count to n.
    #[test]
    fn prop_reset_restores_count(
        (n, ops) in arb_union_find(30),
    ) {
        let mut uf = build_uf(n, &ops);
        uf.reset();
        prop_assert_eq!(uf.component_count(), n);
        prop_assert_eq!(uf.len(), n);
    }

    /// After reset, no two distinct elements are connected.
    #[test]
    fn prop_reset_disconnects_all(
        (n, ops) in arb_union_find(15),
        x_frac in 0.0f64..1.0,
        y_frac in 0.0f64..1.0,
    ) {
        let x = frac_to_idx(x_frac, n);
        let y = frac_to_idx(y_frac, n);
        prop_assume!(x != y);
        let mut uf = build_uf(n, &ops);
        uf.reset();
        prop_assert!(!uf.connected(x, y), "elements {} and {} should be disconnected after reset", x, y);
    }
}

// ── Clone properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Cloned UF gives identical connectivity for all pairs.
    #[test]
    fn prop_clone_equivalence(
        (n, ops) in arb_union_find(15),
        queries in prop::collection::vec((0usize..15, 0usize..15), 1..20),
    ) {
        let mut uf = build_uf(n, &ops);
        let mut clone = uf.clone();
        for &(x, y) in &queries {
            if x < n && y < n {
                prop_assert_eq!(
                    uf.connected(x, y),
                    clone.connected(x, y),
                    "clone diverged for ({}, {})", x, y
                );
            }
        }
    }

    /// Mutations to clone don't affect original.
    #[test]
    fn prop_clone_independence(
        (n, ops) in arb_union_find(15),
    ) {
        let uf = build_uf(n, &ops);
        let original_count = uf.component_count();
        let mut clone = uf.clone();
        // Union everything in clone
        for i in 1..n {
            clone.union(0, i);
        }
        prop_assert_eq!(uf.component_count(), original_count, "original modified by clone mutation");
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// UnionFindConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(cap in 0usize..1000) {
        let config = UnionFindConfig { capacity: cap };
        let json = serde_json::to_string(&config).unwrap();
        let back: UnionFindConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// UnionFindStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(
        (n, ops) in arb_union_find(20),
    ) {
        let mut uf = build_uf(n, &ops);
        let stats = uf.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: UnionFindStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// Stats fields are consistent with UF state.
    #[test]
    fn prop_stats_consistent(
        (n, ops) in arb_union_find(20),
    ) {
        let mut uf = build_uf(n, &ops);
        let stats = uf.stats();
        prop_assert_eq!(stats.element_count, uf.len());
        prop_assert_eq!(stats.component_count, uf.component_count());
        prop_assert!(stats.largest_component >= 1, "largest component must be >= 1");
        prop_assert!(stats.largest_component <= n, "largest component must be <= n");
        prop_assert_eq!(stats.memory_bytes, uf.memory_bytes());
    }
}

// ── Empty UF properties ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty UF has no components.
    #[test]
    fn prop_empty_invariants(_dummy in 0..1u8) {
        let uf = UnionFind::new(0);
        prop_assert!(uf.is_empty());
        prop_assert_eq!(uf.component_count(), 0);
        prop_assert_eq!(uf.len(), 0);
    }
}

// ── Additional properties ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// is_empty agrees with len == 0.
    #[test]
    fn prop_is_empty_agrees(n in 0usize..=20) {
        let uf = UnionFind::new(n);
        prop_assert_eq!(uf.is_empty(), n == 0);
    }

    /// Union with self is a no-op (returns false).
    #[test]
    fn prop_self_union_noop(
        n in 1usize..=20,
        x_frac in 0.0f64..1.0,
    ) {
        let x = (x_frac * n as f64) as usize % n;
        let mut uf = UnionFind::new(n);
        let merged = uf.union(x, x);
        prop_assert!(!merged, "union(x, x) should return false");
        prop_assert_eq!(uf.component_count(), n);
    }

    /// Component members all have the same root.
    #[test]
    fn prop_members_share_root(
        (n, ops) in arb_union_find(15),
        x_frac in 0.0f64..1.0,
    ) {
        let x = frac_to_idx(x_frac, n);
        let mut uf = build_uf(n, &ops);
        let root = uf.find(x);
        let members = uf.component_members(x);
        for &m in &members {
            prop_assert_eq!(uf.find(m), root, "member {} has different root", m);
        }
    }

    /// Largest component <= n.
    #[test]
    fn prop_largest_component_bounded(
        (n, ops) in arb_union_find(20),
    ) {
        let mut uf = build_uf(n, &ops);
        let stats = uf.stats();
        prop_assert!(stats.largest_component <= n);
        prop_assert!(stats.largest_component >= 1 || n == 0);
    }

    /// memory_bytes > 0 for non-empty UF.
    #[test]
    fn prop_memory_positive(n in 1usize..=20) {
        let uf = UnionFind::new(n);
        prop_assert!(uf.memory_bytes() > 0);
    }

    /// make_set then union with existing works.
    #[test]
    fn prop_make_set_then_union(
        (n, ops) in arb_union_find(15),
    ) {
        prop_assume!(n > 0);
        let mut uf = build_uf(n, &ops);
        let new_idx = uf.make_set();
        prop_assert!(!uf.connected(0, new_idx));
        uf.union(0, new_idx);
        prop_assert!(uf.connected(0, new_idx));
    }

    /// find is idempotent after make_set.
    #[test]
    fn prop_find_after_make_set(
        (n, ops) in arb_union_find(15),
    ) {
        let mut uf = build_uf(n, &ops);
        let idx = uf.make_set();
        let root = uf.find(idx);
        prop_assert_eq!(root, idx, "new element should be its own root");
        prop_assert_eq!(uf.find(root), root);
    }
}

// ── Additional behavioral invariants ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Union with self is a no-op.
    #[test]
    fn prop_union_self_noop(
        (n, ops) in arb_union_find(15),
    ) {
        let mut uf = build_uf(n, &ops);
        let count_before = uf.component_count();
        uf.union(0, 0);
        prop_assert_eq!(uf.component_count(), count_before);
    }

    /// After reset, component_count == len.
    #[test]
    fn prop_reset_restores_singletons(
        (n, ops) in arb_union_find(15),
    ) {
        let mut uf = build_uf(n, &ops);
        uf.reset();
        prop_assert_eq!(uf.component_count(), uf.len());
    }

    /// After reset, no two elements are connected (extended).
    #[test]
    fn prop_reset_disconnects_all_extended(
        (n, ops) in arb_union_find(10),
    ) {
        let mut uf = build_uf(n, &ops);
        uf.reset();
        if n > 1 {
            prop_assert!(!uf.connected(0, 1), "elements should be disconnected after reset");
        }
    }

    /// Stats config serde JSON has expected key.
    #[test]
    fn prop_config_json_has_capacity(n in 1usize..100) {
        let config = UnionFindConfig { capacity: n };
        let json = serde_json::to_string(&config).unwrap();
        prop_assert!(json.contains("\"capacity\""));
    }

    /// Stats component_count is within bounds after ops.
    #[test]
    fn prop_stats_component_count_bounded(
        (n, ops) in arb_union_find(20),
    ) {
        let mut uf = build_uf(n, &ops);
        let stats = uf.stats();
        prop_assert!(stats.component_count >= 1);
        prop_assert!(stats.component_count <= n);
    }

    /// Clone preserves connectivity.
    #[test]
    fn prop_clone_preserves_connectivity(
        (n, ops) in arb_union_find(10),
    ) {
        let mut uf = build_uf(n, &ops);
        let mut clone = uf.clone();
        for i in 0..n {
            for j in 0..n {
                prop_assert_eq!(uf.connected(i, j), clone.connected(i, j),
                    "connectivity mismatch at ({}, {})", i, j);
            }
        }
    }

    /// Stats len equals the initial n.
    #[test]
    fn prop_stats_len_equals_n(
        (n, ops) in arb_union_find(20),
    ) {
        let uf = build_uf(n, &ops);
        prop_assert_eq!(uf.len(), n);
    }
}
