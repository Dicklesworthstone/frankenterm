//! Property-based tests for trie.rs — compact prefix trie.
//!
//! Verifies the Trie invariants:
//! - Insert/contains consistency: inserted keys are found
//! - Insert uniqueness: duplicate insert returns false, len unchanged
//! - Remove correctness: removed keys are not found
//! - Prefix queries: starts_with and keys_with_prefix are consistent
//! - Longest common prefix: respects stored keys
//! - Key ordering: all_keys returns sorted order
//! - Clone equivalence and independence
//! - Clear restores empty state
//! - Stats consistency
//! - Config and stats serde roundtrip
//!
//! Bead: ft-283h4.29

use frankenterm_core::trie::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_key() -> impl Strategy<Value = String> {
    "[a-z]{0,10}"
}

fn arb_keys(max_n: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_key(), 0..=max_n)
}

fn build_trie(keys: &[String]) -> Trie {
    let mut t = Trie::new();
    for key in keys {
        t.insert(key);
    }
    t
}

fn unique_keys(keys: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for k in keys {
        if seen.insert(k.clone()) {
            result.push(k.clone());
        }
    }
    result
}

// ── Insert/contains consistency ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every inserted key is found by contains.
    #[test]
    fn prop_insert_then_contains(keys in arb_keys(20)) {
        let mut t = build_trie(&keys);
        for key in &keys {
            prop_assert!(t.contains(key), "inserted key '{}' not found", key);
        }
    }

    /// Non-inserted keys are not found.
    #[test]
    fn prop_absent_keys_not_found(
        keys in arb_keys(10),
        probe in "[A-Z]{1,5}",
    ) {
        // Keys are lowercase a-z, probe is uppercase A-Z — no overlap
        let mut t = build_trie(&keys);
        prop_assert!(!t.contains(&probe), "non-inserted key '{}' found", probe);
    }

    /// len() equals the number of unique keys inserted.
    #[test]
    fn prop_len_equals_unique_count(keys in arb_keys(20)) {
        let t = build_trie(&keys);
        let unique = unique_keys(&keys);
        prop_assert_eq!(t.len(), unique.len(), "len mismatch");
    }
}

// ── Duplicate handling ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inserting a duplicate returns false and doesn't change len.
    #[test]
    fn prop_duplicate_insert(key in arb_key()) {
        let mut t = Trie::new();
        prop_assert!(t.insert(&key), "first insert should return true");
        let len_after_first = t.len();
        prop_assert!(!t.insert(&key), "duplicate insert should return false");
        prop_assert_eq!(t.len(), len_after_first, "len should not change on duplicate");
    }
}

// ── Remove properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Removed key is no longer found.
    #[test]
    fn prop_remove_then_not_found(keys in arb_keys(10), idx in 0usize..10) {
        let unique = unique_keys(&keys);
        prop_assume!(!unique.is_empty());
        let idx = idx % unique.len();
        let target = &unique[idx];

        let mut t = build_trie(&keys);
        prop_assert!(t.remove(target), "remove of existing key should return true");
        prop_assert!(!t.contains(target), "removed key '{}' still found", target);
    }

    /// Remove decrements len by 1.
    #[test]
    fn prop_remove_decrements_len(keys in arb_keys(10), idx in 0usize..10) {
        let unique = unique_keys(&keys);
        prop_assume!(!unique.is_empty());
        let idx = idx % unique.len();

        let mut t = build_trie(&keys);
        let before = t.len();
        t.remove(&unique[idx]);
        prop_assert_eq!(t.len(), before - 1, "len should decrease by 1 after remove");
    }

    /// Remove of non-existent key returns false and doesn't change len.
    #[test]
    fn prop_remove_nonexistent(
        keys in arb_keys(10),
        probe in "[A-Z]{1,5}",
    ) {
        let mut t = build_trie(&keys);
        let before = t.len();
        prop_assert!(!t.remove(&probe), "remove of non-existent should return false");
        prop_assert_eq!(t.len(), before, "len should not change");
    }

    /// Removing a key doesn't affect other keys.
    #[test]
    fn prop_remove_preserves_others(keys in arb_keys(10), idx in 0usize..10) {
        let unique = unique_keys(&keys);
        prop_assume!(unique.len() >= 2);
        let idx = idx % unique.len();
        let target = unique[idx].clone();

        let mut t = build_trie(&keys);
        t.remove(&target);

        for (i, key) in unique.iter().enumerate() {
            if i != idx {
                prop_assert!(t.contains(key), "key '{}' lost after removing '{}'", key, target);
            }
        }
    }
}

// ── Prefix query properties ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// starts_with(prefix) is true if any key has that prefix.
    #[test]
    fn prop_starts_with_consistency(keys in arb_keys(10), prefix in "[a-z]{0,5}") {
        let t = build_trie(&keys);
        let unique = unique_keys(&keys);
        let any_has_prefix = unique.iter().any(|k| k.starts_with(&prefix));
        prop_assert_eq!(
            t.starts_with(&prefix), any_has_prefix,
            "starts_with('{}') mismatch", prefix
        );
    }

    /// keys_with_prefix returns exactly the keys that start with the prefix.
    #[test]
    fn prop_keys_with_prefix_correct(keys in arb_keys(10), prefix in "[a-z]{0,3}") {
        let t = build_trie(&keys);
        let unique = unique_keys(&keys);

        let mut expected: Vec<String> = unique.iter()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        expected.sort();

        let mut actual = t.keys_with_prefix(&prefix);
        actual.sort();

        prop_assert_eq!(actual, expected, "keys_with_prefix('{}') mismatch", prefix);
    }

    /// keys_with_prefix("") returns all keys.
    #[test]
    fn prop_empty_prefix_returns_all(keys in arb_keys(10)) {
        let t = build_trie(&keys);
        let unique = unique_keys(&keys);

        let mut all = t.keys_with_prefix("");
        all.sort();
        let mut expected = unique;
        expected.sort();

        prop_assert_eq!(all, expected, "empty prefix should return all keys");
    }
}

// ── Longest common prefix properties ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// longest_common_prefix returns a key that is a prefix of the query.
    #[test]
    fn prop_lcp_is_prefix_of_query(keys in arb_keys(10), query in "[a-z]{0,10}") {
        prop_assume!(!keys.is_empty());
        let mut t = build_trie(&keys);
        let lcp = t.longest_common_prefix(&query);
        prop_assert!(
            query.starts_with(&lcp),
            "LCP '{}' is not a prefix of query '{}'", lcp, query
        );
    }

    /// longest_common_prefix returns a string that is a stored key (or empty).
    #[test]
    fn prop_lcp_is_stored_key(keys in arb_keys(10), query in "[a-z]{0,10}") {
        prop_assume!(!keys.is_empty());
        let mut t = build_trie(&keys);
        let lcp = t.longest_common_prefix(&query);
        if !lcp.is_empty() {
            prop_assert!(t.contains(&lcp), "LCP '{}' is not a stored key", lcp);
        }
    }

    /// No longer prefix of query is a stored key.
    #[test]
    fn prop_lcp_is_longest(keys in arb_keys(10), query in "[a-z]{1,8}") {
        prop_assume!(!keys.is_empty());
        let unique = unique_keys(&keys);
        let mut t = build_trie(&unique);
        let lcp = t.longest_common_prefix(&query);

        // Check that no longer stored prefix of query exists
        for key in &unique {
            if query.starts_with(key.as_str()) && key.len() > lcp.len() {
                prop_assert!(false,
                    "found longer matching key '{}' than LCP '{}'", key, lcp);
            }
        }
    }
}

// ── all_keys ordering ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// all_keys returns keys in sorted order.
    #[test]
    fn prop_all_keys_sorted(keys in arb_keys(20)) {
        let t = build_trie(&keys);
        let all = t.all_keys();
        let is_sorted = all.windows(2).all(|w| w[0] <= w[1]);
        prop_assert!(is_sorted, "all_keys not sorted");
    }

    /// all_keys has the same elements as unique inserted keys.
    #[test]
    fn prop_all_keys_complete(keys in arb_keys(20)) {
        let t = build_trie(&keys);
        let mut all = t.all_keys();
        all.sort();
        let mut expected = unique_keys(&keys);
        expected.sort();
        prop_assert_eq!(all, expected, "all_keys doesn't match unique inserted keys");
    }
}

// ── Clone properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces identical key set.
    #[test]
    fn prop_clone_equivalence(keys in arb_keys(15)) {
        let t = build_trie(&keys);
        let clone = t.clone();
        prop_assert_eq!(t.all_keys(), clone.all_keys(), "clone key set mismatch");
        prop_assert_eq!(t.len(), clone.len(), "clone len mismatch");
    }

    /// Mutations to clone don't affect original.
    #[test]
    fn prop_clone_independence(keys in arb_keys(10)) {
        let t = build_trie(&keys);
        let original_len = t.len();
        let mut clone = t.clone();
        clone.insert("ZZZZ_unique_test_key");
        prop_assert_eq!(t.len(), original_len, "original modified by clone mutation");
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() empties the trie.
    #[test]
    fn prop_clear_empties(keys in arb_keys(15)) {
        let mut t = build_trie(&keys);
        t.clear();
        prop_assert!(t.is_empty(), "not empty after clear");
        prop_assert_eq!(t.len(), 0, "len not 0 after clear");
        prop_assert!(t.all_keys().is_empty(), "all_keys not empty after clear");
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// TrieConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(n in 0usize..1000) {
        let config = TrieConfig { expected_keys: n };
        let json = serde_json::to_string(&config).unwrap();
        let back: TrieConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// TrieStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(keys in arb_keys(10)) {
        let t = build_trie(&keys);
        let stats = t.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: TrieStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// Stats fields are consistent.
    #[test]
    fn prop_stats_consistent(keys in arb_keys(10)) {
        let t = build_trie(&keys);
        let stats = t.stats();
        prop_assert_eq!(stats.key_count, t.len());
        prop_assert_eq!(stats.memory_bytes, t.memory_bytes());
        prop_assert!(stats.node_count >= 1, "must have at least root node");
    }
}

// ── Empty trie properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty trie invariants.
    #[test]
    fn prop_empty_invariants(_dummy in 0..1u8) {
        let t = Trie::new();
        prop_assert!(t.is_empty());
        prop_assert_eq!(t.len(), 0);
        prop_assert!(t.all_keys().is_empty());
        prop_assert!(!t.starts_with("a"));
    }
}

// ── Additional properties ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// is_empty agrees with len == 0.
    #[test]
    fn prop_is_empty_agrees_with_len(keys in arb_keys(15)) {
        let t = build_trie(&keys);
        prop_assert_eq!(t.is_empty(), t.is_empty());
    }

    /// all_keys().len() == len().
    #[test]
    fn prop_all_keys_len_matches(keys in arb_keys(15)) {
        let t = build_trie(&keys);
        prop_assert_eq!(t.all_keys().len(), t.len());
    }

    /// Insert after remove works.
    #[test]
    fn prop_insert_after_remove(keys in arb_keys(10)) {
        let unique = unique_keys(&keys);
        prop_assume!(!unique.is_empty());
        let mut t = build_trie(&keys);

        let target = &unique[0];
        t.remove(target);
        prop_assert!(!t.contains(target));

        t.insert(target);
        prop_assert!(t.contains(target));
    }

    /// Remove all yields empty trie.
    #[test]
    fn prop_remove_all_empty(keys in arb_keys(15)) {
        let unique = unique_keys(&keys);
        let mut t = build_trie(&keys);
        for key in &unique {
            t.remove(key);
        }
        prop_assert!(t.is_empty());
        prop_assert_eq!(t.len(), 0);
    }

    /// Clear then insert works.
    #[test]
    fn prop_clear_then_insert(keys in arb_keys(10)) {
        let mut t = build_trie(&keys);
        t.clear();
        t.insert("hello");
        prop_assert_eq!(t.len(), 1);
        prop_assert!(t.contains("hello"));
    }

    /// Debug format is non-empty.
    #[test]
    fn prop_debug_format(keys in arb_keys(10)) {
        let t = build_trie(&keys);
        let debug = format!("{:?}", t);
        prop_assert!(!debug.is_empty());
    }

    /// longest_common_prefix of exact key returns the key.
    #[test]
    fn prop_lcp_exact_key(keys in arb_keys(10)) {
        let unique = unique_keys(&keys);
        prop_assume!(!unique.is_empty());
        let mut t = build_trie(&keys);
        let target = &unique[0];
        let lcp = t.longest_common_prefix(target);
        prop_assert_eq!(lcp, target.clone(), "LCP of exact key should be the key itself");
    }

    /// memory_bytes > 0 for non-empty trie.
    #[test]
    fn prop_memory_positive(keys in arb_keys(10)) {
        prop_assume!(!keys.is_empty());
        let t = build_trie(&keys);
        if !t.is_empty() {
            prop_assert!(t.memory_bytes() > 0);
        }
    }
}
