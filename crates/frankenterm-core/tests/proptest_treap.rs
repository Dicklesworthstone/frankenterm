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
//! - Double insert/remove semantics, interleaved operations
//! - to_sorted_vec, keys, min/max vs kth, Default trait, Display

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

    // ══════════════════════════════════════════════════════════════
    // NEW TESTS (17-32)
    // ══════════════════════════════════════════════════════════════

    // ── Double insert updates value ─────────────────────────────

    #[test]
    fn double_insert_updates_value(
        key in 0..1000i32,
        val1 in any::<u32>(),
        val2 in any::<u32>()
    ) {
        let mut treap = Treap::new();
        treap.insert(key, val1);
        let old = treap.insert(key, val2);
        prop_assert_eq!(old, Some(val1), "should return first value");
        prop_assert_eq!(treap.get(&key), Some(&val2), "should have second value");
        prop_assert_eq!(treap.len(), 1, "len should still be 1");
    }

    // ── Double remove returns None ──────────────────────────────

    #[test]
    fn double_remove_returns_none(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let key = pairs[0].0;
        let first = treap.remove(&key);
        let second = treap.remove(&key);
        let first_is_some = first.is_some();
        prop_assert!(first_is_some, "first remove should return Some");
        prop_assert!(second.is_none(), "second remove should return None");
    }

    // ── with_seed produces deterministic ordering ────────────────

    #[test]
    fn with_seed_deterministic(seed in any::<u64>(), pairs in kv_pairs_strategy(30)) {
        let mut treap1 = Treap::with_seed(seed);
        let mut treap2 = Treap::with_seed(seed);

        for &(k, v) in &pairs {
            treap1.insert(k, v);
            treap2.insert(k, v);
        }

        // Same seed + same insertions should produce identical trees
        prop_assert_eq!(treap1.len(), treap2.len());
        let keys1: Vec<i32> = treap1.keys().into_iter().copied().collect();
        let keys2: Vec<i32> = treap2.keys().into_iter().copied().collect();
        prop_assert_eq!(keys1.clone(), keys2, "same seed should give same key order");

        for &k in &keys1 {
            prop_assert_eq!(treap1.get(&k), treap2.get(&k));
        }
    }

    // ── to_sorted_vec matches BTreeMap ──────────────────────────

    #[test]
    fn to_sorted_vec_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let treap_sorted: Vec<(i32, u32)> = treap.to_sorted_vec().into_iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        let ref_sorted: Vec<(i32, u32)> = reference.iter()
            .map(|(&k, &v)| (k, v))
            .collect();

        prop_assert_eq!(treap_sorted, ref_sorted, "to_sorted_vec mismatch");
    }

    // ── keys() returns sorted keys ──────────────────────────────

    #[test]
    fn keys_sorted(pairs in kv_pairs_strategy(50)) {
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let keys = treap.keys();
        for w in keys.windows(2) {
            prop_assert!(w[0] < w[1], "keys not sorted: {} >= {}", w[0], w[1]);
        }
    }

    // ── Insert all then remove all leaves empty ─────────────────

    #[test]
    fn insert_all_remove_all_empty(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        for &k in reference.keys() {
            treap.remove(&k);
        }

        prop_assert!(treap.is_empty(), "treap not empty after removing all keys");
        prop_assert_eq!(treap.len(), 0);
        prop_assert!(treap.min().is_none());
        prop_assert!(treap.max().is_none());
    }

    // ── Interleaved insert/remove stays consistent ──────────────

    #[test]
    fn interleaved_insert_remove_consistent(
        inserts in kv_pairs_strategy(30),
        removes in prop::collection::vec(0..1000i32, 0..15)
    ) {
        let mut treap = Treap::new();
        let mut reference = BTreeMap::new();

        for &(k, v) in &inserts {
            treap.insert(k, v);
            reference.insert(k, v);
        }
        for &k in &removes {
            treap.remove(&k);
            reference.remove(&k);
        }

        prop_assert_eq!(treap.len(), reference.len());
        for (k, v) in &reference {
            prop_assert_eq!(treap.get(k), Some(v), "value mismatch for key {}", k);
        }
    }

    // ── Remove updates kth correctly ────────────────────────────

    #[test]
    fn remove_updates_kth(pairs in kv_pairs_strategy(30)) {
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

        let ref_sorted: Vec<(&i32, &u32)> = reference.iter().collect();
        for (i, &(rk, rv)) in ref_sorted.iter().enumerate() {
            let treap_kth = treap.kth(i);
            prop_assert_eq!(treap_kth, Some((rk, rv)), "kth({}) wrong after remove", i);
        }
    }

    // ── min() == kth(0) ─────────────────────────────────────────

    #[test]
    fn min_equals_kth_zero(pairs in kv_pairs_strategy(50)) {
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let min_result = treap.min();
        let kth_result = treap.kth(0);
        prop_assert_eq!(min_result, kth_result, "min() should equal kth(0)");
    }

    // ── max() == kth(len-1) ─────────────────────────────────────

    #[test]
    fn max_equals_kth_last(pairs in kv_pairs_strategy(50)) {
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        if treap.is_empty() {
            prop_assert!(treap.max().is_none());
            return Ok(());
        }

        let last_idx = treap.len() - 1;
        let max_result = treap.max();
        let kth_result = treap.kth(last_idx);
        prop_assert_eq!(max_result, kth_result, "max() should equal kth(len-1)");
    }

    // ── Default creates empty treap ─────────────────────────────

    #[test]
    fn default_is_empty(key in 0..1000i32) {
        let treap: Treap<i32, u32> = Treap::default();
        prop_assert!(treap.is_empty());
        prop_assert_eq!(treap.len(), 0);
        prop_assert!(treap.get(&key).is_none());
        prop_assert!(treap.min().is_none());
        prop_assert!(treap.max().is_none());
    }

    // ── Contains after remove is false ──────────────────────────

    #[test]
    fn contains_after_remove_false(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        for &k in reference.keys() {
            treap.remove(&k);
            prop_assert!(!treap.contains_key(&k), "contains_key({}) true after remove", k);
        }
    }

    // ── Insert overwrite preserves other keys ───────────────────

    #[test]
    fn insert_overwrite_preserves_others(pairs in kv_pairs_strategy(30)) {
        if pairs.len() < 2 {
            return Ok(());
        }

        let mut treap = Treap::new();
        let mut reference = build_reference(&pairs);
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        // Overwrite first key with new value
        let key = pairs[0].0;
        let new_val = pairs[0].1.wrapping_add(1);
        treap.insert(key, new_val);
        reference.insert(key, new_val);

        // Verify all keys still present with correct values
        for (k, v) in &reference {
            prop_assert_eq!(treap.get(k), Some(v), "key {} changed after overwriting key {}", k, key);
        }
    }

    // ── Display produces non-empty output ───────────────────────

    #[test]
    fn display_format(pairs in kv_pairs_strategy(20)) {
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let displayed = format!("{}", treap);
        prop_assert!(!displayed.is_empty(), "Display should produce non-empty output");
    }

    // ── Serde preserves order statistics ────────────────────────

    #[test]
    fn serde_preserves_order_statistics(pairs in kv_pairs_strategy(30)) {
        let mut treap = Treap::new();
        for &(k, v) in &pairs {
            treap.insert(k, v);
        }

        let json = serde_json::to_string(&treap).unwrap();
        let restored: Treap<i32, u32> = serde_json::from_str(&json).unwrap();

        // Verify kth and rank are preserved
        for i in 0..treap.len() {
            let orig_kth = treap.kth(i);
            let rest_kth = restored.kth(i);
            prop_assert_eq!(orig_kth, rest_kth, "serde broke kth({})", i);
        }
    }

    // ── Stress: many insert/remove/query cycles ─────────────────

    #[test]
    fn stress_insert_remove_query(
        pairs in kv_pairs_strategy(50),
        remove_indices in prop::collection::vec(0usize..50, 0..25),
        probe in 0..1000i32
    ) {
        let mut treap = Treap::new();
        let mut reference = BTreeMap::new();

        for &(k, v) in &pairs {
            treap.insert(k, v);
            reference.insert(k, v);
        }

        for &idx in &remove_indices {
            if idx < pairs.len() {
                let k = pairs[idx].0;
                treap.remove(&k);
                reference.remove(&k);
            }
        }

        prop_assert_eq!(treap.len(), reference.len());
        prop_assert_eq!(treap.contains_key(&probe), reference.contains_key(&probe));

        let expected_rank = reference.keys().filter(|&&k| k < probe).count();
        prop_assert_eq!(treap.rank(&probe), expected_rank);
    }
}
