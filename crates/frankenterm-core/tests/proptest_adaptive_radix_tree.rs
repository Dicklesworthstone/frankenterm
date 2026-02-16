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
        for k in reference.keys() {
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
        if !art.is_empty() {
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

    // ── String API consistency ───────────────────────────────────

    #[test]
    fn insert_str_matches_insert_bytes(
        pairs in prop::collection::vec(("[a-z]{1,15}", any::<u32>()), 0..30)
    ) {
        let mut art_bytes = AdaptiveRadixTree::new();
        let mut art_str = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art_bytes.insert(k.as_bytes(), *v);
            art_str.insert_str(k, *v);
        }
        prop_assert_eq!(art_bytes.len(), art_str.len());
        for (k, _) in &pairs {
            prop_assert_eq!(art_bytes.get(k.as_bytes()), art_str.get_str(k));
        }
    }

    #[test]
    fn contains_str_matches_contains_key(
        pairs in prop::collection::vec(("[a-z]{1,15}", any::<u32>()), 0..30),
        probe in "[a-z]{1,15}"
    ) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert_str(k, *v);
        }
        prop_assert_eq!(art.contains_str(&probe), art.contains_key(probe.as_bytes()));
    }

    #[test]
    fn remove_str_matches_remove_bytes(
        pairs in prop::collection::vec(("[a-z]{1,10}", any::<u32>()), 1..20)
    ) {
        let mut art_bytes = AdaptiveRadixTree::new();
        let mut art_str = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art_bytes.insert(k.as_bytes(), *v);
            art_str.insert_str(k, *v);
        }

        let key = &pairs[0].0;
        let removed_bytes = art_bytes.remove(key.as_bytes());
        let removed_str = art_str.remove_str(key);
        prop_assert_eq!(removed_bytes, removed_str);
        prop_assert_eq!(art_bytes.len(), art_str.len());
    }

    #[test]
    fn prefix_search_str_matches_bytes(
        pairs in prop::collection::vec(("[a-z]{1,8}", any::<u32>()), 0..20),
        prefix in "[a-z]{1,4}"
    ) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert_str(k, *v);
        }

        let bytes_results = art.prefix_search(prefix.as_bytes());
        let str_results = art.prefix_search_str(&prefix);
        prop_assert_eq!(bytes_results.len(), str_results.len());
    }

    // ── Multiple removes ────────────────────────────────────────

    #[test]
    fn multiple_removes_consistent(pairs in kv_pairs_strategy(30)) {
        let mut art = AdaptiveRadixTree::new();
        let mut reference = build_reference(&pairs);
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        // Remove half the keys
        let keys_to_remove: Vec<Vec<u8>> = reference.keys().take(reference.len() / 2).cloned().collect();
        for k in &keys_to_remove {
            art.remove(k);
            reference.remove(k);
        }

        prop_assert_eq!(art.len(), reference.len());
        for (k, v) in &reference {
            prop_assert_eq!(art.get(k), Some(v));
        }
    }

    // ── Double remove returns None ──────────────────────────────

    #[test]
    fn double_remove_returns_none(pairs in kv_pairs_strategy(20)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let key = &pairs[0].0;
        let _ = art.remove(key);
        let second = art.remove(key);
        prop_assert!(second.is_none());
    }

    // ── Display format ──────────────────────────────────────────

    #[test]
    fn display_format(pairs in kv_pairs_strategy(20)) {
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }
        let display = format!("{}", art);
        prop_assert!(!display.is_empty());
        prop_assert!(display.contains("AdaptiveRadixTree"));
    }

    // ── Default is empty ────────────────────────────────────────

    #[test]
    fn default_is_empty(_dummy in 0..10u8) {
        let art: AdaptiveRadixTree<u32> = AdaptiveRadixTree::default();
        prop_assert!(art.is_empty());
        prop_assert_eq!(art.len(), 0);
        prop_assert_eq!(art.node_count(), 0);
    }

    // ── Overwrite updates value ─────────────────────────────────

    #[test]
    fn overwrite_updates_value(pairs in kv_pairs_strategy(20)) {
        if pairs.is_empty() {
            return Ok(());
        }

        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let key = &pairs[0].0;
        let len_before = art.len();
        art.insert(key, 99999);
        prop_assert_eq!(art.len(), len_before);
        prop_assert_eq!(art.get(key), Some(&99999));
    }

    // ── Remove all yields empty ─────────────────────────────────

    #[test]
    fn remove_all_yields_empty(pairs in kv_pairs_strategy(20)) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        for k in reference.keys() {
            art.remove(k);
        }

        prop_assert!(art.is_empty());
        prop_assert_eq!(art.len(), 0);
    }

    // ── Serde preserves all values ─────────────────────────────

    #[test]
    fn serde_preserves_all_values(pairs in kv_pairs_strategy(30)) {
        let reference = build_reference(&pairs);
        let mut art = AdaptiveRadixTree::new();
        for (k, v) in &pairs {
            art.insert(k, *v);
        }

        let json = serde_json::to_string(&art).unwrap();
        let restored: AdaptiveRadixTree<u32> = serde_json::from_str(&json).unwrap();

        // Verify every reference key is present with correct value
        for (k, v) in &reference {
            prop_assert_eq!(restored.get(k), Some(v), "key {:?} missing after serde", k);
        }
        prop_assert_eq!(restored.len(), reference.len());
    }
}
