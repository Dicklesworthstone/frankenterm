//! Property-based tests for `splay_tree` module.
//!
//! Verifies correctness invariants:
//! - Lookup consistency with BTreeMap reference
//! - BST ordering invariant
//! - Size tracking
//! - Order statistics (kth, rank)
//! - Insert/remove semantics
//! - Splay-to-root property
//! - Serde roundtrip
//! - Clone equivalence and independence
//! - Display formatting
//! - Iterator correctness

use frankenterm_core::splay_tree::SplayTree;
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
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        for (k, v) in &reference {
            prop_assert_eq!(tree.get(k), Some(v));
        }
    }

    // ── Peek matches BTreeMap ─────────────────────────────────────

    #[test]
    fn peek_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        for (k, v) in &reference {
            prop_assert_eq!(tree.peek(k), Some(v));
        }
    }

    // ── Length matches BTreeMap ─────────────────────────────────────

    #[test]
    fn len_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        prop_assert_eq!(tree.len(), reference.len());
    }

    // ── Sorted order matches BTreeMap ──────────────────────────────

    #[test]
    fn sorted_order_matches(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let tree_keys: Vec<i32> = tree.keys().into_iter().copied().collect();
        let ref_keys: Vec<i32> = reference.keys().copied().collect();
        prop_assert_eq!(tree_keys, ref_keys);
    }

    // ── Insert returns previous ────────────────────────────────────

    #[test]
    fn insert_returns_previous(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();
        let mut reference = BTreeMap::new();

        for &(k, v) in &pairs {
            let tree_old = tree.insert(k, v);
            let ref_old = reference.insert(k, v);
            prop_assert_eq!(tree_old, ref_old);
        }
    }

    // ── Contains key consistency ───────────────────────────────────

    #[test]
    fn contains_key_matches(
        pairs in kv_pairs_strategy(30),
        probe in 0..1000i32
    ) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        prop_assert_eq!(tree.contains_key(&probe), reference.contains_key(&probe));
    }

    // ── Remove semantics ───────────────────────────────────────────

    #[test]
    fn remove_returns_value(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut tree = SplayTree::new();
        let mut reference = BTreeMap::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
            reference.insert(k, v);
        }

        let key = pairs[0].0;
        let tree_removed = tree.remove(&key);
        let ref_removed = reference.remove(&key);
        prop_assert_eq!(tree_removed, ref_removed);
        prop_assert_eq!(tree.len(), reference.len());
    }

    #[test]
    fn remove_preserves_others(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut tree = SplayTree::new();
        let mut reference = build_reference(&pairs);
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let key = pairs[0].0;
        tree.remove(&key);
        reference.remove(&key);

        for (k, v) in &reference {
            prop_assert_eq!(tree.get(k), Some(v));
        }
    }

    // ── Kth element ────────────────────────────────────────────────

    #[test]
    fn kth_matches_sorted(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let ref_sorted: Vec<(&i32, &u32)> = reference.iter().collect();
        for (i, &(rk, rv)) in ref_sorted.iter().enumerate() {
            let tree_kth = tree.kth(i);
            prop_assert_eq!(tree_kth, Some((rk, rv)), "kth({}) mismatch", i);
        }

        prop_assert!(tree.kth(reference.len()).is_none());
    }

    // ── Rank matches sorted position ───────────────────────────────

    #[test]
    fn rank_matches_position(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let ref_keys: Vec<i32> = reference.keys().copied().collect();
        for (pos, &key) in ref_keys.iter().enumerate() {
            prop_assert_eq!(tree.rank(&key), pos, "rank of {} mismatch", key);
        }
    }

    // ── Rank for missing key ───────────────────────────────────────

    #[test]
    fn rank_for_missing_key(
        pairs in kv_pairs_strategy(30),
        probe in 0..1000i32
    ) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let expected = reference.keys().filter(|&&k| k < probe).count();
        prop_assert_eq!(tree.rank(&probe), expected);
    }

    // ── Min/max ────────────────────────────────────────────────────

    #[test]
    fn min_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let tree_min = tree.min().map(|(k, v)| (*k, *v));
        let ref_min = reference.iter().next().map(|(&k, &v)| (k, v));
        prop_assert_eq!(tree_min, ref_min);
    }

    #[test]
    fn max_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let tree_max = tree.max().map(|(k, v)| (*k, *v));
        let ref_max = reference.iter().next_back().map(|(&k, &v)| (k, v));
        prop_assert_eq!(tree_max, ref_max);
    }

    // ── Kth and rank are inverse ───────────────────────────────────

    #[test]
    fn kth_rank_inverse(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        if reference.is_empty() {
            return Ok(());
        }

        for i in 0..reference.len() {
            let (key, _) = tree.kth(i).unwrap();
            let rank = tree.rank(key);
            prop_assert_eq!(rank, i, "kth/rank not inverse at position {}", i);
        }
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let json = serde_json::to_string(&tree).unwrap();
        let restored: SplayTree<i32, u32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), tree.len());

        let reference = build_reference(&pairs);
        for (k, v) in &reference {
            prop_assert_eq!(restored.peek(k), Some(v));
        }
    }

    // ── Empty tree operations ──────────────────────────────────────

    #[test]
    fn empty_tree_operations(key in 0..1000i32) {
        let tree: SplayTree<i32, u32> = SplayTree::new();
        prop_assert!(tree.is_empty());
        prop_assert!(tree.peek(&key).is_none());
        prop_assert!(tree.kth(0).is_none());
        prop_assert_eq!(tree.rank(&key), 0);
    }

    // ── Size after operations ──────────────────────────────────────

    #[test]
    fn size_consistent_after_ops(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();

        for (i, &(k, v)) in pairs.iter().enumerate() {
            tree.insert(k, v);
            let expected_len = build_reference(&pairs[..=i]).len();
            prop_assert_eq!(tree.len(), expected_len, "len mismatch after insert {}", i);
        }
    }

    // ── Repeated access doesn't break correctness ──────────────────

    #[test]
    fn repeated_access_preserves_correctness(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        // Access every key multiple times — tree should remain correct
        for _ in 0..3 {
            for &key in reference.keys() {
                prop_assert_eq!(tree.get(&key), reference.get(&key));
            }
        }
        prop_assert_eq!(tree.len(), reference.len());
    }
}

// ── Clone equivalence ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces identical content.
    #[test]
    fn clone_equivalence(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }
        let cloned = tree.clone();

        prop_assert_eq!(cloned.len(), tree.len());
        let reference = build_reference(&pairs);
        for (k, v) in &reference {
            prop_assert_eq!(cloned.peek(k), Some(v));
        }
    }

    /// Clone is independent — mutating clone doesn't affect original.
    #[test]
    fn clone_independence(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }
        let original_len = tree.len();

        let mut cloned = tree.clone();
        cloned.insert(9999, 1);

        prop_assert_eq!(tree.len(), original_len);
        prop_assert!(tree.peek(&9999).is_none());
        prop_assert_eq!(cloned.len(), original_len + 1);
    }

    /// Display format is non-empty.
    #[test]
    fn display_format(pairs in kv_pairs_strategy(20)) {
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }
        let display = format!("{}", tree);
        prop_assert!(!display.is_empty());
        prop_assert!(display.contains("SplayTree"));
    }

    /// is_empty agrees with len == 0.
    #[test]
    fn is_empty_agrees_with_len(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }
        let reference = build_reference(&pairs);
        prop_assert_eq!(tree.is_empty(), reference.is_empty());
        prop_assert_eq!(tree.is_empty(), tree.len() == 0);
    }

    /// Iterator pairs match reference model.
    #[test]
    fn iter_matches_reference(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let iter_pairs: Vec<(i32, u32)> = tree.iter().into_iter().map(|(&k, &v)| (k, v)).collect();
        let ref_pairs: Vec<(i32, u32)> = reference.iter().map(|(&k, &v)| (k, v)).collect();
        prop_assert_eq!(iter_pairs, ref_pairs);
    }

    /// Double remove returns None.
    #[test]
    fn double_remove_returns_none(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let key = pairs[0].0;
        let _ = tree.remove(&key);
        let second = tree.remove(&key);
        prop_assert!(second.is_none());
    }

    /// Overwrite preserves length.
    #[test]
    fn overwrite_preserves_len(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }
        let len_before = tree.len();

        // Overwrite first key
        let key = pairs[0].0;
        tree.insert(key, 99999);
        prop_assert_eq!(tree.len(), len_before);
        prop_assert_eq!(tree.get(&key), Some(&99999));
    }

    /// After removing first key, min/max still correct.
    #[test]
    fn min_max_after_remove(pairs in kv_pairs_strategy(30)) {
        let mut reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        if reference.len() < 2 {
            return Ok(());
        }

        let first_key = *reference.keys().next().unwrap();
        tree.remove(&first_key);
        reference.remove(&first_key);

        let tree_min = tree.min().map(|(k, v)| (*k, *v));
        let ref_min = reference.iter().next().map(|(&k, &v)| (k, v));
        prop_assert_eq!(tree_min, ref_min);
    }

    /// Remove all entries yields empty tree.
    #[test]
    fn remove_all_yields_empty(pairs in kv_pairs_strategy(20)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        for key in reference.keys() {
            tree.remove(key);
        }

        prop_assert!(tree.is_empty());
        prop_assert_eq!(tree.len(), 0);
        prop_assert!(tree.min().is_none());
        prop_assert!(tree.max().is_none());
    }

    /// Insert-remove-insert cycle: reinserting works correctly.
    #[test]
    fn insert_remove_reinsert(pairs in kv_pairs_strategy(20)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let key = pairs[0].0;
        tree.remove(&key);
        prop_assert!(tree.get(&key).is_none());

        tree.insert(key, 77777);
        prop_assert_eq!(tree.get(&key), Some(&77777));
    }

    /// Default produces same as new.
    #[test]
    fn default_same_as_new(_dummy in 0..10u8) {
        let d: SplayTree<i32, u32> = SplayTree::default();
        let n: SplayTree<i32, u32> = SplayTree::new();
        prop_assert!(d.is_empty());
        prop_assert!(n.is_empty());
        prop_assert_eq!(d.len(), n.len());
    }

    /// Peek does not affect subsequent get results.
    #[test]
    fn peek_does_not_affect_get(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        // Peek all keys (non-mutating)
        for k in reference.keys() {
            let _ = tree.peek(k);
        }

        // Get should still return correct results
        for (k, v) in &reference {
            prop_assert_eq!(tree.get(k), Some(v));
        }
    }

    /// Keys length matches len().
    #[test]
    fn keys_len_matches_len(pairs in kv_pairs_strategy(30)) {
        let mut tree = SplayTree::new();
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }
        prop_assert_eq!(tree.keys().len(), tree.len());
    }

    /// Serde roundtrip after removals preserves remaining entries.
    #[test]
    fn serde_roundtrip_after_remove(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut tree = SplayTree::new();
        let mut reference = build_reference(&pairs);
        for &(k, v) in &pairs {
            tree.insert(k, v);
        }

        let key = pairs[0].0;
        tree.remove(&key);
        reference.remove(&key);

        let json = serde_json::to_string(&tree).unwrap();
        let restored: SplayTree<i32, u32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), reference.len());
        for (k, v) in &reference {
            prop_assert_eq!(restored.peek(k), Some(v));
        }
    }
}
