//! Property-based tests for `bimap` — bidirectional map with O(1) lookups.

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use frankenterm_core::bimap::BiMap;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_pair_vec() -> impl Strategy<Value = Vec<(u32, u32)>> {
    proptest::collection::vec((0..1000u32, 0..1000u32), 0..30)
}

fn arb_bimap() -> impl Strategy<Value = BiMap<u32, u32>> {
    arb_pair_vec().prop_map(|pairs| BiMap::from_pairs(pairs))
}

fn arb_string_bimap() -> impl Strategy<Value = BiMap<String, String>> {
    proptest::collection::vec(
        ("[a-z]{1,5}".prop_map(|s| s), "[A-Z]{1,5}".prop_map(|s| s)),
        0..20,
    )
    .prop_map(|pairs| BiMap::from_pairs(pairs))
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. Bijection invariant: forward and reverse agree
    #[test]
    fn bijection_invariant(bm in arb_bimap()) {
        for (k, v) in bm.iter() {
            prop_assert_eq!(bm.get_by_key(k), Some(v));
            prop_assert_eq!(bm.get_by_value(v), Some(k));
        }
    }

    // 2. Forward and reverse sizes match
    #[test]
    fn forward_reverse_size_match(pairs in arb_pair_vec()) {
        let bm = BiMap::from_pairs(pairs);
        // The bimap should have equal entries in both directions
        let key_count = bm.keys().count();
        let val_count = bm.values().count();
        prop_assert_eq!(key_count, val_count);
        prop_assert_eq!(key_count, bm.len());
    }

    // 3. All keys are unique
    #[test]
    fn keys_unique(bm in arb_bimap()) {
        let keys: Vec<u32> = bm.keys().copied().collect();
        let unique: HashSet<u32> = keys.iter().copied().collect();
        prop_assert_eq!(keys.len(), unique.len());
    }

    // 4. All values are unique
    #[test]
    fn values_unique(bm in arb_bimap()) {
        let vals: Vec<u32> = bm.values().copied().collect();
        let unique: HashSet<u32> = vals.iter().copied().collect();
        prop_assert_eq!(vals.len(), unique.len());
    }

    // 5. Insert then get_by_key returns the inserted value
    #[test]
    fn insert_then_get_key(key in 0..1000u32, val in 0..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key, val);
        prop_assert_eq!(bm.get_by_key(&key), Some(&val));
    }

    // 6. Insert then get_by_value returns the inserted key
    #[test]
    fn insert_then_get_value(key in 0..1000u32, val in 0..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key, val);
        prop_assert_eq!(bm.get_by_value(&val), Some(&key));
    }

    // 7. Remove by key clears both directions
    #[test]
    fn remove_by_key_clears_both(bm in arb_bimap()) {
        let pairs = bm.to_pairs();
        if let Some((k, v)) = pairs.into_iter().next() {
            let mut bm = bm;
            bm.remove_by_key(&k);
            prop_assert!(!bm.contains_key(&k));
            prop_assert!(!bm.contains_value(&v));
        }
    }

    // 8. Remove by value clears both directions
    #[test]
    fn remove_by_value_clears_both(bm in arb_bimap()) {
        let pairs = bm.to_pairs();
        if let Some((k, v)) = pairs.into_iter().next() {
            let mut bm = bm;
            bm.remove_by_value(&v);
            prop_assert!(!bm.contains_key(&k));
            prop_assert!(!bm.contains_value(&v));
        }
    }

    // 9. Clear makes empty
    #[test]
    fn clear_makes_empty(bm in arb_bimap()) {
        let mut bm = bm;
        bm.clear();
        prop_assert!(bm.is_empty());
        prop_assert_eq!(bm.len(), 0);
    }

    // 10. Inverse swaps key and value
    #[test]
    fn inverse_swaps(bm in arb_bimap()) {
        let inv = bm.inverse();
        for (k, v) in bm.iter() {
            prop_assert_eq!(inv.get_by_key(v), Some(k));
            prop_assert_eq!(inv.get_by_value(k), Some(v));
        }
    }

    // 11. Inverse of inverse equals original
    #[test]
    fn inverse_involution(bm in arb_bimap()) {
        let double_inv = bm.inverse().inverse();
        prop_assert_eq!(bm, double_inv);
    }

    // 12. Inverse has same length
    #[test]
    fn inverse_same_length(bm in arb_bimap()) {
        prop_assert_eq!(bm.len(), bm.inverse().len());
    }

    // 13. is_empty consistent with len
    #[test]
    fn is_empty_consistent(bm in arb_bimap()) {
        prop_assert_eq!(bm.is_empty(), bm.len() == 0);
    }

    // 14. contains_key iff get_by_key is Some
    #[test]
    fn contains_key_iff_get(bm in arb_bimap(), key in 0..1000u32) {
        prop_assert_eq!(bm.contains_key(&key), bm.get_by_key(&key).is_some());
    }

    // 15. contains_value iff get_by_value is Some
    #[test]
    fn contains_value_iff_get(bm in arb_bimap(), val in 0..1000u32) {
        prop_assert_eq!(bm.contains_value(&val), bm.get_by_value(&val).is_some());
    }

    // 16. Duplicate values are evicted (last write wins)
    #[test]
    fn duplicate_value_evicts(key1 in 0..100u32, key2 in 100..200u32, val in 0..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key1, val);
        bm.insert(key2, val);
        prop_assert_eq!(bm.len(), 1);
        prop_assert!(!bm.contains_key(&key1));
        prop_assert!(bm.contains_key(&key2));
        prop_assert_eq!(bm.get_by_value(&val), Some(&key2));
    }

    // 17. Duplicate keys update value
    #[test]
    fn duplicate_key_updates(key in 0..1000u32, val1 in 0..500u32, val2 in 500..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key, val1);
        bm.insert(key, val2);
        prop_assert_eq!(bm.len(), 1);
        prop_assert!(!bm.contains_value(&val1));
        prop_assert_eq!(bm.get_by_key(&key), Some(&val2));
    }

    // 18. to_pairs roundtrip
    #[test]
    fn to_pairs_roundtrip(bm in arb_bimap()) {
        let pairs = bm.to_pairs();
        let rebuilt = BiMap::from_pairs(pairs);
        prop_assert_eq!(bm, rebuilt);
    }

    // 19. serde roundtrip
    #[test]
    fn serde_roundtrip(bm in arb_bimap()) {
        let json = serde_json::to_string(&bm).unwrap();
        let back: BiMap<u32, u32> = serde_json::from_str(&json).unwrap();
        // After deserialization, forward map matches
        for (k, v) in bm.iter() {
            prop_assert_eq!(back.get_by_key(k), Some(v));
        }
        prop_assert_eq!(bm.len(), back.len());
    }

    // 20. retain preserves bijection
    #[test]
    fn retain_preserves_bijection(bm in arb_bimap(), threshold in 0..1000u32) {
        let mut retained = bm.clone();
        retained.retain(|k, _| *k < threshold);
        // Check bijection after retain
        for (k, v) in retained.iter() {
            prop_assert_eq!(retained.get_by_key(k), Some(v));
            prop_assert_eq!(retained.get_by_value(v), Some(k));
        }
        // All retained keys satisfy predicate
        for k in retained.keys() {
            prop_assert!(*k < threshold);
        }
    }

    // 21. insert returns previous value
    #[test]
    fn insert_returns_previous(key in 0..1000u32, val1 in 0..500u32, val2 in 500..1000u32) {
        let mut bm = BiMap::new();
        let first = bm.insert(key, val1);
        prop_assert_eq!(first, None);
        let second = bm.insert(key, val2);
        prop_assert_eq!(second, Some(val1));
    }

    // 22. remove_by_key returns value
    #[test]
    fn remove_by_key_returns_value(key in 0..1000u32, val in 0..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key, val);
        let removed = bm.remove_by_key(&key);
        prop_assert_eq!(removed, Some(val));
    }

    // 23. remove_by_value returns key
    #[test]
    fn remove_by_value_returns_key(key in 0..1000u32, val in 0..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key, val);
        let removed = bm.remove_by_value(&val);
        prop_assert_eq!(removed, Some(key));
    }

    // 24. Clone equality
    #[test]
    fn clone_eq(bm in arb_bimap()) {
        let cloned = bm.clone();
        prop_assert_eq!(bm, cloned);
    }

    // 25. from_pairs with no duplicates preserves all
    #[test]
    fn from_pairs_no_dup_preserves_all(count in 0..30usize) {
        let pairs: Vec<(u32, u32)> = (0..count as u32).map(|i| (i, i + 1000)).collect();
        let bm = BiMap::from_pairs(pairs.clone());
        prop_assert_eq!(bm.len(), count);
        for (k, v) in &pairs {
            prop_assert_eq!(bm.get_by_key(k), Some(v));
        }
    }

    // 26. String keys work
    #[test]
    fn string_keys_work(bm in arb_string_bimap()) {
        for (k, v) in bm.iter() {
            prop_assert_eq!(bm.get_by_key(k), Some(v));
            prop_assert_eq!(bm.get_by_value(v), Some(k));
        }
    }

    // 27. Remove nonexistent returns None
    #[test]
    fn remove_nonexistent_none(bm in arb_bimap(), key in 2000..3000u32) {
        let mut bm = bm;
        prop_assert_eq!(bm.remove_by_key(&key), None);
    }

    // 28. Inserting same k,v pair is idempotent
    #[test]
    fn same_pair_idempotent(key in 0..1000u32, val in 0..1000u32) {
        let mut bm = BiMap::new();
        bm.insert(key, val);
        let len_before = bm.len();
        bm.insert(key, val);
        prop_assert_eq!(bm.len(), len_before);
        prop_assert_eq!(bm.get_by_key(&key), Some(&val));
        prop_assert_eq!(bm.get_by_value(&val), Some(&key));
    }

    // 29. Sequential inserts maintain bijection
    #[test]
    fn sequential_inserts_bijection(pairs in arb_pair_vec()) {
        let bm = BiMap::from_pairs(pairs);
        // Verify bijection: every key maps to exactly one value and vice versa
        let mut seen_keys = HashSet::new();
        let mut seen_vals = HashSet::new();
        for (k, v) in bm.iter() {
            prop_assert!(!seen_keys.contains(k), "duplicate key {}", k);
            prop_assert!(!seen_vals.contains(v), "duplicate value {}", v);
            seen_keys.insert(*k);
            seen_vals.insert(*v);
        }
    }

    // 30. iter count matches len
    #[test]
    fn iter_count_matches_len(bm in arb_bimap()) {
        prop_assert_eq!(bm.iter().count(), bm.len());
    }

    // 31. BiMap acts as injection: no two keys map to same value
    #[test]
    fn no_duplicate_values(pairs in arb_pair_vec()) {
        let bm = BiMap::from_pairs(pairs);
        let vals: Vec<u32> = bm.values().copied().collect();
        let unique_vals: HashSet<u32> = vals.iter().copied().collect();
        prop_assert_eq!(vals.len(), unique_vals.len());
    }

    // 32. BiMap final state matches HashMap with last-write-wins
    #[test]
    fn matches_last_write_wins(pairs in arb_pair_vec()) {
        let bm = BiMap::from_pairs(pairs.clone());
        // Build expected state: apply pairs sequentially with eviction logic
        let mut expected_fwd = HashMap::new();
        let mut expected_rev = HashMap::new();
        for (k, v) in &pairs {
            // Remove old reverse for this key
            if let Some(old_v) = expected_fwd.get(k) {
                expected_rev.remove(old_v);
            }
            // Remove old forward for this value
            if let Some(old_k) = expected_rev.get(v) {
                expected_fwd.remove(old_k);
            }
            expected_fwd.insert(*k, *v);
            expected_rev.insert(*v, *k);
        }
        prop_assert_eq!(bm.len(), expected_fwd.len());
        for (k, v) in &expected_fwd {
            prop_assert_eq!(bm.get_by_key(k), Some(v));
        }
    }
}
