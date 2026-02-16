//! Property-based tests for `van_emde_boas` module.
//!
//! Verifies correctness invariants against BTreeSet reference:
//! - Contains matches reference
//! - Min/max match reference
//! - Successor/predecessor match reference
//! - Insert/remove semantics
//! - Iteration order
//! - Serde roundtrip

use frankenterm_core::van_emde_boas::VanEmdeBoas;
use proptest::prelude::*;
use std::collections::BTreeSet;

// ── Strategies ─────────────────────────────────────────────────────────

fn values_strategy(universe: u32, max_len: usize) -> impl Strategy<Value = Vec<u32>> {
    prop::collection::vec(0..universe, 0..max_len)
}

fn build_reference(vals: &[u32]) -> BTreeSet<u32> {
    vals.iter().copied().collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Contains matches BTreeSet ─────────────────────────────────

    #[test]
    fn contains_matches(
        vals in values_strategy(256, 50),
        probe in 0u32..256
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        prop_assert_eq!(veb.contains(probe), reference.contains(&probe));
    }

    // ── Length matches BTreeSet ────────────────────────────────────

    #[test]
    fn len_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        prop_assert_eq!(veb.len(), reference.len());
    }

    // ── Min matches BTreeSet ──────────────────────────────────────

    #[test]
    fn min_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.iter().next().copied();
        prop_assert_eq!(veb.min(), expected);
    }

    // ── Max matches BTreeSet ──────────────────────────────────────

    #[test]
    fn max_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.iter().next_back().copied();
        prop_assert_eq!(veb.max(), expected);
    }

    // ── Successor matches BTreeSet ────────────────────────────────

    #[test]
    fn successor_matches(
        vals in values_strategy(256, 30),
        probe in 0u32..256
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.range((probe + 1)..).next().copied();
        prop_assert_eq!(veb.successor(probe), expected, "successor({}) mismatch", probe);
    }

    // ── Predecessor matches BTreeSet ──────────────────────────────

    #[test]
    fn predecessor_matches(
        vals in values_strategy(256, 30),
        probe in 0u32..256
    ) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let expected = reference.range(..probe).next_back().copied();
        prop_assert_eq!(veb.predecessor(probe), expected, "predecessor({}) mismatch", probe);
    }

    // ── Insert returns correct boolean ────────────────────────────

    #[test]
    fn insert_returns_correct(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        let mut reference = BTreeSet::new();

        for &v in &vals {
            let veb_new = veb.insert(v);
            let ref_new = reference.insert(v);
            prop_assert_eq!(veb_new, ref_new, "insert({}) mismatch", v);
        }
    }

    // ── Remove returns correct boolean ────────────────────────────

    #[test]
    fn remove_returns_correct(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        let mut reference = BTreeSet::new();

        // Insert all
        for &v in &vals {
            veb.insert(v);
            reference.insert(v);
        }

        // Remove in reverse order
        for &v in vals.iter().rev() {
            let veb_had = veb.remove(v);
            let ref_had = reference.remove(&v);
            prop_assert_eq!(veb_had, ref_had, "remove({}) mismatch", v);
        }
    }

    // ── Iteration order matches ───────────────────────────────────

    #[test]
    fn iter_matches(vals in values_strategy(256, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let veb_sorted = veb.iter();
        let ref_sorted: Vec<u32> = reference.iter().copied().collect();
        prop_assert_eq!(veb_sorted, ref_sorted);
    }

    // ── Remove preserves others ───────────────────────────────────

    #[test]
    fn remove_preserves_others(vals in values_strategy(256, 30)) {
        if vals.is_empty() {
            return Ok(());
        }

        let mut veb = VanEmdeBoas::new(256);
        let mut reference: BTreeSet<u32> = BTreeSet::new();
        for &v in &vals {
            veb.insert(v);
            reference.insert(v);
        }

        let key = vals[0];
        veb.remove(key);
        reference.remove(&key);

        for &v in &reference {
            prop_assert!(veb.contains(v), "missing {} after removing {}", v, key);
        }
    }

    // ── Serde roundtrip ───────────────────────────────────────────

    #[test]
    fn serde_roundtrip(vals in values_strategy(256, 30)) {
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        let json = serde_json::to_string(&veb).unwrap();
        let restored: VanEmdeBoas = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), veb.len());
        prop_assert_eq!(restored.min(), veb.min());
        prop_assert_eq!(restored.max(), veb.max());
        prop_assert_eq!(restored.iter(), veb.iter());
    }

    // ── Empty operations ──────────────────────────────────────────

    #[test]
    fn empty_operations(probe in 0u32..256) {
        let veb = VanEmdeBoas::new(256);
        prop_assert!(veb.is_empty());
        prop_assert!(!veb.contains(probe));
        prop_assert!(veb.successor(probe).is_none());
        prop_assert!(veb.predecessor(probe).is_none());
        prop_assert!(veb.min().is_none());
        prop_assert!(veb.max().is_none());
    }

    // ── Successor chain covers all elements ───────────────────────

    #[test]
    fn successor_chain(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        if reference.is_empty() {
            return Ok(());
        }

        // Walk successor chain from min
        let mut current = veb.min();
        let mut chain = Vec::new();
        while let Some(val) = current {
            chain.push(val);
            current = veb.successor(val);
        }

        let ref_sorted: Vec<u32> = reference.iter().copied().collect();
        prop_assert_eq!(chain, ref_sorted, "successor chain doesn't match sorted set");
    }

    // ── Predecessor chain covers all elements ─────────────────────

    #[test]
    fn predecessor_chain(vals in values_strategy(256, 30)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(256);
        for &v in &vals {
            veb.insert(v);
        }

        if reference.is_empty() {
            return Ok(());
        }

        // Walk predecessor chain from max
        let mut current = veb.max();
        let mut chain = Vec::new();
        while let Some(val) = current {
            chain.push(val);
            current = veb.predecessor(val);
        }
        chain.reverse();

        let ref_sorted: Vec<u32> = reference.iter().copied().collect();
        prop_assert_eq!(chain, ref_sorted, "predecessor chain doesn't match sorted set");
    }

    // ── Larger universe ───────────────────────────────────────────

    #[test]
    fn larger_universe(vals in values_strategy(4096, 50)) {
        let reference = build_reference(&vals);
        let mut veb = VanEmdeBoas::new(4096);
        for &v in &vals {
            veb.insert(v);
        }

        prop_assert_eq!(veb.len(), reference.len());
        prop_assert_eq!(veb.min(), reference.iter().next().copied());
        prop_assert_eq!(veb.max(), reference.iter().next_back().copied());
    }
}
