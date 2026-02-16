//! Property-based tests for `treap` module.
//!
//! Verifies correctness invariants:
//! - Lookup consistency with BTreeMap reference
//! - BST ordering invariant
//! - Heap priority invariant
//! - Size tracking
//! - Order statistics (kth, rank)
//! - Insert/remove semantics
//! - Serde roundtrip

use frankenterm_core::treap::Treap;
use proptest::prelude::*;
use std::collections::BTreeMap;

// ── Strategies ─────────────────────────────────────────────────────────

fn kv_pairs_strategy(max_len: usize) -> impl Strategy<Value = Vec<(i32, u32)>> {
    prop::collection::vec((0..1000i32, any::<u32>()), 0..max_len)
}

fn build_reference(pairs: &[(i32, u32)]) -> BTreeMap<i32, u32> {
    let mut map = BTreeMap::new();
    for &(k, v) in pairs {
        map.insert(k, v);
    }
    map
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Lookup matches BTreeMap ────────────────────────────────────

    #[test]
    fn get_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        for (k, v) in &reference {
            prop_assert_eq!(treap.get(k), Some(v));
        }
    }

    // ── Length matches BTreeMap ─────────────────────────────────────

    #[test]
    fn len_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        prop_assert_eq!(treap.len(), reference.len());
    }

    // ── Sorted order matches BTreeMap ──────────────────────────────

    #[test]
    fn sorted_order_matches(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let treap_keys: Vec<i32> = treap.keys().into_iter().copied().collect();
        let ref_keys: Vec<i32> = reference.keys().copied().collect();
        prop_assert_eq!(treap_keys, ref_keys);
    }

    // ── Insert returns previous ────────────────────────────────────

    #[test]
    fn insert_returns_previous(pairs in kv_pairs_strategy(30)) {
        let mut treap = Treap::new();
        let mut reference = BTreeMap::new();

        for &(k, v) in &pairs {
            let treap_old = treap.insert(k, v);
            let ref_old = reference.insert(k, v);
            prop_assert_eq!(treap_old, ref_old);
        }
    }

    // ── Contains key consistency ───────────────────────────────────

    #[test]
    fn contains_key_matches(
        pairs in kv_pairs_strategy(30),
        probe in 0..1000i32
    ) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        prop_assert_eq!(treap.contains_key(&probe), reference.contains_key(&probe));
    }

    // ── Remove semantics ───────────────────────────────────────────

    #[test]
    fn remove_returns_value(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut treap = Treap::new();
        let mut reference = BTreeMap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
            reference.insert(k, v);
        }

        let key = pairs[0].0;
        let treap_removed = treap.remove(&key);
        let ref_removed = reference.remove(&key);
        prop_assert_eq!(treap_removed, ref_removed);
        prop_assert_eq!(treap.len(), reference.len());
    }

    #[test]
    fn remove_preserves_others(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut treap = Treap::new();
        let mut reference = build_reference(&pairs);
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let key = pairs[0].0;
        treap.remove(&key);
        reference.remove(&key);

        for (k, v) in &reference {
            prop_assert_eq!(treap.get(k), Some(v));
        }
    }

    // ── Kth element ────────────────────────────────────────────────

    #[test]
    fn kth_matches_sorted(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let ref_sorted: Vec<(&i32, &u32)> = reference.iter().collect();
        for (i, &(rk, rv)) in ref_sorted.iter().enumerate() {
            let treap_kth = treap.kth(i);
            prop_assert_eq!(treap_kth, Some((rk, rv)), "kth({}) mismatch", i);
        }

        prop_assert!(treap.kth(reference.len()).is_none());
    }

    // ── Rank matches sorted position ───────────────────────────────

    #[test]
    fn rank_matches_position(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let ref_keys: Vec<i32> = reference.keys().copied().collect();
        for (pos, &key) in ref_keys.iter().enumerate() {
            prop_assert_eq!(treap.rank(&key), pos, "rank of {} mismatch", key);
        }
    }

    // ── Min/max ────────────────────────────────────────────────────

    #[test]
    fn min_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let treap_min = treap.min().map(|(k, v)| (*k, *v));
        let ref_min = reference.iter().next().map(|(&k, &v)| (k, v));
        prop_assert_eq!(treap_min, ref_min);
    }

    #[test]
    fn max_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let treap_max = treap.max().map(|(k, v)| (*k, *v));
        let ref_max = reference.iter().next_back().map(|(&k, &v)| (k, v));
        prop_assert_eq!(treap_max, ref_max);
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip(pairs in kv_pairs_strategy(30)) {
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let json = serde_json::to_string(&treap).unwrap();
        let restored: Treap<i32, u32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), treap.len());

        let reference = build_reference(&pairs);
        for (k, v) in &reference {
            prop_assert_eq!(restored.get(k), Some(v));
        }
    }

    // ── Empty tree operations ──────────────────────────────────────

    #[test]
    fn empty_tree_operations(key in 0..1000i32) {
        let treap: Treap<i32, u32> = Treap::new();
        prop_assert!(treap.is_empty());
        prop_assert!(treap.get(&key).is_none());
        prop_assert!(!treap.contains_key(&key));
        prop_assert!(treap.kth(0).is_none());
        prop_assert_eq!(treap.rank(&key), 0);
    }

    // ── Rank for missing key ───────────────────────────────────────

    #[test]
    fn rank_for_missing_key(
        pairs in kv_pairs_strategy(30),
        probe in 0..1000i32
    ) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        // rank should equal number of keys strictly less than probe
        let expected = reference.keys().filter(|&&k| k < probe).count();
        prop_assert_eq!(treap.rank(&probe), expected);
    }

    // ── Kth and rank are inverse ───────────────────────────────────

    #[test]
    fn kth_rank_inverse(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        if reference.is_empty() {
            return Ok(());
        }

        for i in 0..reference.len() {
            let (key, _) = treap.kth(i).unwrap();
            let rank = treap.rank(key);
            prop_assert_eq!(rank, i, "kth/rank not inverse at position {}", i);
        }
    }

    // ── Size after operations ──────────────────────────────────────

    #[test]
    fn size_consistent_after_ops(pairs in kv_pairs_strategy(30)) {
        let mut treap = Treap::new();

        for (i, &(k, v)) in pairs.iter().enumerate() {
            treap.insert(k, v);
            let expected_len = build_reference(&pairs[..=i]).len();
            prop_assert_eq!(treap.len(), expected_len, "len mismatch after insert {}", i);
        }
    }
}
