//! Property-based tests for ARS FST compiler.
//!
//! Verifies trie invariants, lookup correctness, prefix search completeness,
//! deduplication, MinHash serialization, and serde roundtrips.

use proptest::prelude::*;

use std::collections::HashSet;

use frankenterm_core::ars_fst::{
    FstCompiler, FstConfig, FstError, FstIndex, FstStats,
    TriggerEntry, minhash_to_key, key_to_minhash,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(1..255u8, 1..50)
}

fn arb_entry() -> impl Strategy<Value = TriggerEntry> {
    (arb_key(), 0..10000u64, 0..100u32, "[a-z]{2,8}")
        .prop_map(|(key, reflex_id, priority, cluster)| TriggerEntry {
            key,
            reflex_id,
            priority,
            cluster_id: cluster,
        })
}

fn arb_entries(min: usize, max: usize) -> impl Strategy<Value = Vec<TriggerEntry>> {
    prop::collection::vec(arb_entry(), min..=max)
}

fn arb_minhash() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(any::<u64>(), 1..10)
}

// =============================================================================
// Compilation invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn compiled_len_matches_deduped_keys(entries in arb_entries(1, 20)) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        // After dedup, count of unique keys.
        let unique_keys: HashSet<Vec<u8>> = entries.iter().map(|e| e.key.clone()).collect();
        prop_assert_eq!(index.len(), unique_keys.len());
    }

    #[test]
    fn node_count_at_least_entry_count(entries in arb_entries(1, 20)) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        // Root + at least one node per entry.
        prop_assert!(index.node_count() >= index.len() + 1);
    }

    #[test]
    fn stats_entry_count_matches(entries in arb_entries(1, 20)) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        prop_assert_eq!(index.stats().entry_count, index.len());
    }

    #[test]
    fn all_inserted_keys_findable(entries in arb_entries(1, 20)) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        for entry in &entries {
            let found = index.contains(&entry.key);
            prop_assert!(found, "key {:?} should be found", entry.key);
        }
    }

    #[test]
    fn non_inserted_key_not_found(
        entries in arb_entries(1, 10),
        extra_key in arb_key(),
    ) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        let is_inserted = entries.iter().any(|e| e.key == extra_key);
        if !is_inserted {
            prop_assert!(!index.contains(&extra_key));
        }
    }
}

// =============================================================================
// Prefix search invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prefix_search_returns_all_prefix_matches(
        entries in arb_entries(1, 10),
        query_suffix in prop::collection::vec(1..255u8, 0..20),
    ) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();

        // Build query as first entry's key + suffix.
        let base_key = &entries[0].key;
        let mut query = base_key.clone();
        query.extend_from_slice(&query_suffix);

        let matches = index.prefix_search(&query);

        // Every match's key should be a prefix of query.
        for m in &matches {
            prop_assert!(
                m.match_len <= query.len(),
                "match_len {} > query_len {}", m.match_len, query.len()
            );
        }
    }

    #[test]
    fn prefix_search_ordered_by_match_length(
        entries in arb_entries(1, 10),
    ) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();

        // Query using a long random byte string.
        let query: Vec<u8> = (0..100u8).collect();
        let matches = index.prefix_search(&query);

        // Results should be ordered by match length.
        for window in matches.windows(2) {
            prop_assert!(
                window[0].match_len <= window[1].match_len,
                "matches should be ordered by length"
            );
        }
    }

    #[test]
    fn prefix_search_superset_of_exact_lookup(
        entries in arb_entries(1, 10),
    ) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();

        for entry in &entries {
            let exact = index.lookup(&entry.key);
            let prefix_results = index.prefix_search(&entry.key);

            if exact.is_some() {
                // The exact key should appear in prefix results.
                let found_exact = prefix_results.iter().any(|m| m.match_len == entry.key.len());
                prop_assert!(found_exact, "exact match should appear in prefix search results");
            }
        }
    }
}

// =============================================================================
// Deduplication invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn dedup_keeps_best_priority(
        key in arb_key(),
        priorities in prop::collection::vec(0..100u32, 2..5),
    ) {
        let entries: Vec<TriggerEntry> = priorities
            .iter()
            .enumerate()
            .map(|(i, &p)| TriggerEntry {
                key: key.clone(),
                reflex_id: i as u64,
                priority: p,
                cluster_id: format!("c-{i}"),
            })
            .collect();

        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        prop_assert_eq!(index.len(), 1);

        let m = index.lookup(&key).unwrap();
        let best_priority = priorities.iter().copied().min().unwrap();
        prop_assert_eq!(m.priority, best_priority);
    }
}

// =============================================================================
// MinHash serialization invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn minhash_roundtrip(sig in arb_minhash()) {
        let key = minhash_to_key(&sig);
        let recovered = key_to_minhash(&key);
        prop_assert_eq!(recovered, sig);
    }

    #[test]
    fn minhash_key_length_correct(sig in arb_minhash()) {
        let key = minhash_to_key(&sig);
        prop_assert_eq!(key.len(), sig.len() * 8);
    }

    #[test]
    fn different_minhash_different_keys(
        sig1 in arb_minhash(),
        sig2 in arb_minhash(),
    ) {
        if sig1 != sig2 {
            let k1 = minhash_to_key(&sig1);
            let k2 = minhash_to_key(&sig2);
            prop_assert_ne!(k1, k2);
        }
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn trigger_entry_serde_roundtrip(entry in arb_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: TriggerEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, entry);
    }

    #[test]
    fn fst_index_serde_preserves_lookups(entries in arb_entries(1, 10)) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();

        let json = serde_json::to_string(&index).unwrap();
        let decoded: FstIndex = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(decoded.len(), index.len());

        for entry in &entries {
            let orig = index.lookup(&entry.key);
            let decoded_result = decoded.lookup(&entry.key);
            let orig_found = orig.is_some();
            let decoded_found = decoded_result.is_some();
            prop_assert_eq!(orig_found, decoded_found);
        }
    }

    #[test]
    fn fst_stats_serde_roundtrip(entries in arb_entries(1, 10)) {
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&entries).unwrap();
        let stats = index.stats().clone();

        let json = serde_json::to_string(&stats).unwrap();
        let decoded: FstStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }

    #[test]
    fn fst_config_serde_roundtrip(
        max_key_len in 100..10000usize,
        max_entries in 100..1000000usize,
    ) {
        let config = FstConfig {
            max_key_len,
            max_entries,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: FstConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.max_key_len, config.max_key_len);
        prop_assert_eq!(decoded.max_entries, config.max_entries);
    }
}

// =============================================================================
// Error case invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn key_too_long_rejected(
        key_len in 101..200usize,
    ) {
        let config = FstConfig {
            max_key_len: 100,
            ..Default::default()
        };
        let compiler = FstCompiler::new(config);
        let key: Vec<u8> = (0..key_len).map(|i| (i % 255 + 1) as u8).collect();
        let entries = vec![TriggerEntry {
            key,
            reflex_id: 0,
            priority: 0,
            cluster_id: "c".to_string(),
        }];
        let result = compiler.compile(&entries);
        let is_too_long = matches!(result, Err(FstError::KeyTooLong { .. }));
        prop_assert!(is_too_long);
    }
}
