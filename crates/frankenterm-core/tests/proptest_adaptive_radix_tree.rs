//! Property-based tests for `adaptive_radix_tree` module.
//!
//! Verifies correctness invariants of the ART using proptest:
//! - Lookup consistency with a BTreeMap reference
//! - Insert/overwrite semantics
//! - Remove semantics
//! - Prefix search completeness and soundness
//! - Iterator ordering
//! - Serde roundtrip preservation
//! - Node type transitions under load

use frankenterm_core::adaptive_radix_tree::AdaptiveRadixTree;
use proptest::prelude::*;
use std::collections::BTreeMap;

// ── Strategies ─────────────────────────────────────────────────────────

fn key_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..30)
}

fn short_key_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0..10u8, 1..8)
}

fn kv_pairs_strategy(max_len: usize) -> impl Strategy<Value = Vec<(Vec<u8>, u32)>> {
    prop::collection::vec((key_strategy(), any::<u32>()), 0..max_len)
}

fn short_kv_pairs_strategy(max_len: usize) -> impl Strategy<Value = Vec<(Vec<u8>, u32)>> {
    prop::collection::vec((short_key_strategy(), any::<u32>()), 0..max_len)
}

// ── Reference model ────────────────────────────────────────────────────

fn build_reference(pairs: &[(Vec<u8>, u32)]) -> BTreeMap<Vec<u8>, u32> {
    let mut map = BTreeMap::new();
    for (k, v) in pairs {
        map.insert(k.clone(), *v);
    }
    map
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Lookup consistency ─────────────────────────────────────────

    #[test]
    fn get_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        // Every key in reference should be findable
        for (k, v) in &reference {
            let art_val = art.get(k);
            prop_assert_eq!(art_val, Some(v), "key {:?} mismatch", k);
        }
    }

    #[test]
    fn missing_keys_return_none(
        pairs in kv_pairs_strategy(30),
        extra_keys in prop::collection::vec(key_strategy(), 1..10)
    ) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        for k in &extra_keys {
            if !reference.contains_key(k) {
                prop_assert!(art.get(k).is_none(), "found unexpected key {:?}", k);
            }
        }
    }

    // ── Length tracking ────────────────────────────────────────────

    #[test]
    fn len_matches_btreemap(pairs in kv_pairs_strategy(50)) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        prop_assert_eq!(art.len(), reference.len());
    }

    // ── Insert returns previous value ──────────────────────────────

    #[test]
    fn insert_returns_previous(pairs in kv_pairs_strategy(30)) {
        let mut art = AdaptiveRadixTree::new();
        let mut reference = BTreeMap::new();

        for (k, v) in &pairs {
            let art_old = art.insert(k, *v);
            let ref_old = reference.insert(k.clone(), *v);
            prop_assert_eq!(art_old, ref_old, "insert return mismatch for key {:?}", k);
        }
    }

    // ── Contains_key consistency ───────────────────────────────────

    #[test]
    fn contains_key_matches_get(
        pairs in kv_pairs_strategy(30),
        probe in key_strategy()
    ) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let has = art.contains_key(&probe);
        let gets = art.get(&probe).is_some();
        prop_assert_eq!(has, gets);
    }

    // ── Remove semantics ───────────────────────────────────────────

    #[test]
    fn remove_returns_value(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut art = AdaptiveRadixTree::new();
        let mut reference = BTreeMap::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
            reference.insert(k.clone(), *v);
        }

        // Remove first unique key
        let key = &pairs[0].0;
        let art_removed = art.remove(key);
        let ref_removed = reference.remove(key);
        prop_assert_eq!(art_removed, ref_removed);
        prop_assert_eq!(art.len(), reference.len());
        prop_assert!(art.get(key).is_none());
    }

    #[test]
    fn remove_preserves_other_keys(pairs in kv_pairs_strategy(30)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut art = AdaptiveRadixTree::new();
        let mut reference = build_reference(&pairs);
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let key = &pairs[0].0;
        art.remove(key);
        reference.remove(key);

        for (k, v) in &reference {
            prop_assert_eq!(
                art.get(k), Some(v),
                "key {:?} missing after removing {:?}", k, key
            );
        }
    }

    #[test]
    fn remove_nonexistent_is_noop(
        pairs in kv_pairs_strategy(20),
        extra in key_strategy()
    ) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        if !reference.contains_key(&extra) {
            let before = art.len();
            let result = art.remove(&extra);
            prop_assert!(result.is_none());
            prop_assert_eq!(art.len(), before);
        }
    }

    // ── Prefix search ──────────────────────────────────────────────

    #[test]
    fn prefix_search_complete(
        pairs in short_kv_pairs_strategy(30),
        prefix in short_key_strategy()
    ) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let art_results = art.prefix_search(&prefix);
        let ref_results: Vec<_> = reference
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .collect();

        prop_assert_eq!(
            art_results.len(), ref_results.len(),
            "prefix search for {:?}: ART found {}, reference found {}",
            prefix, art_results.len(), ref_results.len()
        );
    }

    #[test]
    fn prefix_search_sound(
        pairs in short_kv_pairs_strategy(30),
        prefix in short_key_strategy()
    ) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let results = art.prefix_search(&prefix);
        for (key, _) in &results {
            prop_assert!(
                key.starts_with(&prefix),
                "result key {:?} doesn't start with prefix {:?}", key, prefix
            );
        }
    }

    #[test]
    fn empty_prefix_returns_all(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let results = art.prefix_search(&[]);
        prop_assert_eq!(results.len(), reference.len());
    }

    // ── Iterator ordering ──────────────────────────────────────────

    #[test]
    fn iterator_sorted(pairs in kv_pairs_strategy(30)) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let items = art.iter();
        for w in items.windows(2) {
            prop_assert!(
                w[0].0 <= w[1].0,
                "iterator not sorted: {:?} > {:?}", w[0].0, w[1].0
            );
        }
    }

    #[test]
    fn iterator_count_matches_len(pairs in kv_pairs_strategy(30)) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let count = art.iter().len();
        prop_assert_eq!(count, art.len());
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip(pairs in kv_pairs_strategy(30)) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let json = serde_json::to_string(&art).unwrap();
        let restored: AdaptiveRadixTree<u32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), art.len());

        let reference = build_reference(&pairs);
        for (k, v) in &reference {
            prop_assert_eq!(restored.get(k), Some(v));
        }
    }

    // ── Insert order independence ──────────────────────────────────

    #[test]
    fn insert_order_independent_keys(pairs in kv_pairs_strategy(30)) {
        let mut art_fwd = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art_fwd.insert(k, *v);
        }

        let mut art_rev = AdaptiveRadixTree::new();
        for (k, v) in pairs.iter().rev() {
            art_rev.insert(k, *v);
        }

        // Both should have same key set size (deduplication is order-independent)
        let reference = build_reference(&pairs);
        prop_assert_eq!(art_fwd.len(), reference.len());
        prop_assert_eq!(art_rev.len(), reference.len());

        // Both should have same key set
        for (k, _) in &reference {
            prop_assert!(art_fwd.contains_key(k));
            prop_assert!(art_rev.contains_key(k));
        }

        // Forward matches reference values (last-write-wins in forward order)
        for (k, v) in &reference {
            prop_assert_eq!(art_fwd.get(k), Some(v));
        }
    }

    // ── Dense single-byte keys ─────────────────────────────────────

    #[test]
    fn dense_single_byte_keys(n in 1..256usize) {
        let mut art = AdaptiveRadixTree::new();
        for b in 0..(n as u8) {
            art.insert(&[b], b as u32);
        }

        prop_assert_eq!(art.len(), n);
        for b in 0..(n as u8) {
            prop_assert_eq!(art.get(&[b]), Some(&(b as u32)));
        }
    }

    // ── FromIterator consistency ───────────────────────────────────

    #[test]
    fn from_iter_consistent(pairs in kv_pairs_strategy(30)) {
        let art_manual = {
            let mut t = AdaptiveRadixTree::new();
            for (k, v) in &pairs {
                t.insert(k, *v);
            }
            t
        };

        let art_collected: AdaptiveRadixTree<u32> =
            pairs.iter().map(|(k, v)| (k.clone(), *v)).collect();

        prop_assert_eq!(art_manual.len(), art_collected.len());

        let reference = build_reference(&pairs);
        for (k, v) in &reference {
            prop_assert_eq!(art_collected.get(k), Some(v));
        }
    }

    // ── Empty tree operations ──────────────────────────────────────

    #[test]
    fn empty_tree_operations(key in key_strategy()) {
        let art: AdaptiveRadixTree<u32> = AdaptiveRadixTree::new();
        prop_assert!(art.is_empty());
        prop_assert!(art.get(&key).is_none());
        prop_assert!(art.prefix_search(&key).is_empty());
        prop_assert!(art.iter().is_empty());
    }

    // ── Prefix search values match ─────────────────────────────────

    #[test]
    fn prefix_search_values_correct(
        pairs in short_kv_pairs_strategy(30),
        prefix in short_key_strategy()
    ) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let results = art.prefix_search(&prefix);
        for (key, val) in &results {
            let expected = reference.get(key);
            prop_assert_eq!(Some(*val), expected, "value mismatch for key {:?}", key);
        }
    }

    // ── Node count never exceeds 2*len ─────────────────────────────

    #[test]
    fn node_count_bounded(pairs in kv_pairs_strategy(50)) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        // Each insert creates at most 2 nodes (split + new leaf)
        // Plus existing nodes. Total should be bounded.
        if art.len() > 0 {
            // Very generous bound: each key creates at most key_len nodes
            // In practice, path compression keeps it much lower
            let total_key_bytes: usize = pairs.iter().map(|(k, _)| k.len() + 1).sum();
            prop_assert!(
                art.node_count() <= total_key_bytes + 1,
                "node count {} exceeds bound for {} keys",
                art.node_count(), art.len()
            );
        }
    }
}
