#![cfg(feature = "semantic-search")]
//! Property-based tests for the model_registry module.
//!
//! Verifies ModelRegistry and ModelInfo invariants:
//! - Constructor: new registry is empty, preserves cache_dir
//! - Register + Get roundtrip: all fields preserved after registration
//! - Overwrite semantics: same name replaces, list count stable
//! - List: length equals distinct names, contains all names
//! - Multiple registrations: N distinct → N items, all retrievable
//! - ModelInfo: clone preserves fields, debug non-empty
//! - Edge cases: empty name, long name, zero dimension, zero size_bytes

use proptest::prelude::*;
use std::collections::HashSet;
use std::path::PathBuf;

use frankenterm_core::search::{ModelInfo, ModelRegistry};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_model_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,20}"
}

fn arb_cache_dir() -> impl Strategy<Value = PathBuf> {
    (0u32..1000).prop_map(|n| PathBuf::from(format!("/tmp/test-{}", n)))
}

fn arb_model_info() -> impl Strategy<Value = ModelInfo> {
    (
        arb_model_name(),
        "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
        0usize..2048,
        0u64..10_000_000,
        prop::option::of(arb_cache_dir()),
    )
        .prop_map(
            |(name, version, dimension, size_bytes, cache_path)| ModelInfo {
                name,
                version,
                dimension,
                size_bytes,
                cache_path,
            },
        )
}

fn arb_model_info_with_name(name: String) -> impl Strategy<Value = ModelInfo> {
    (
        "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
        0usize..2048,
        0u64..10_000_000,
        prop::option::of(arb_cache_dir()),
    )
        .prop_map(
            move |(version, dimension, size_bytes, cache_path)| ModelInfo {
                name: name.clone(),
                version,
                dimension,
                size_bytes,
                cache_path,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// Group 1: Constructor invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 1. New registry has empty list.
    #[test]
    fn new_registry_has_empty_list(cache_dir in arb_cache_dir()) {
        let reg = ModelRegistry::new(cache_dir);
        prop_assert!(reg.list().is_empty(), "newly created registry should have empty list");
    }

    /// 2. New registry preserves cache_dir.
    #[test]
    fn new_registry_preserves_cache_dir(cache_dir in arb_cache_dir()) {
        let reg = ModelRegistry::new(cache_dir.clone());
        prop_assert_eq!(reg.cache_dir(), &cache_dir, "cache_dir should be preserved");
    }

    /// 3. Different cache_dirs produce distinct registries (by cache_dir).
    #[test]
    fn different_cache_dirs_produce_distinct_registries(
        dir_a in arb_cache_dir(),
        dir_b in arb_cache_dir(),
    ) {
        let reg_a = ModelRegistry::new(dir_a.clone());
        let reg_b = ModelRegistry::new(dir_b.clone());
        if dir_a != dir_b {
            prop_assert!(
                reg_a.cache_dir() != reg_b.cache_dir(),
                "different input dirs should yield different cache_dir values"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 2: Register + Get roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// 4. After register(info), get(info.name) returns Some.
    #[test]
    fn register_then_get_returns_some(info in arb_model_info()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let name = info.name.clone();
        reg.register(info);
        prop_assert!(
            reg.get(&name).is_some(),
            "get should return Some after register, name = {}", name
        );
    }

    /// 5. get returns matching name.
    #[test]
    fn get_returns_matching_name(info in arb_model_info()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let expected_name = info.name.clone();
        reg.register(info);
        let retrieved = reg.get(&expected_name).unwrap();
        prop_assert_eq!(
            &retrieved.name, &expected_name,
            "retrieved name should match, expected = {}", expected_name
        );
    }

    /// 6. get returns matching version.
    #[test]
    fn get_returns_matching_version(info in arb_model_info()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let name = info.name.clone();
        let expected_version = info.version.clone();
        reg.register(info);
        let retrieved = reg.get(&name).unwrap();
        prop_assert_eq!(
            &retrieved.version, &expected_version,
            "retrieved version should match, expected = {}", expected_version
        );
    }

    /// 7. get returns matching dimension.
    #[test]
    fn get_returns_matching_dimension(info in arb_model_info()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let name = info.name.clone();
        let expected_dim = info.dimension;
        reg.register(info);
        let retrieved = reg.get(&name).unwrap();
        prop_assert_eq!(
            retrieved.dimension, expected_dim,
            "retrieved dimension should match, expected = {}", expected_dim
        );
    }

    /// 8. get returns matching size_bytes.
    #[test]
    fn get_returns_matching_size_bytes(info in arb_model_info()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let name = info.name.clone();
        let expected_size = info.size_bytes;
        reg.register(info);
        let retrieved = reg.get(&name).unwrap();
        prop_assert_eq!(
            retrieved.size_bytes, expected_size,
            "retrieved size_bytes should match, expected = {}", expected_size
        );
    }

    /// 9. get returns None for unregistered name.
    #[test]
    fn get_returns_none_for_unregistered(
        info in arb_model_info(),
        query in "[A-Z]{5,10}",
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        reg.register(info);
        // query is uppercase, arb_model_name is lowercase — guaranteed different
        prop_assert!(
            reg.get(&query).is_none(),
            "get should return None for unregistered name = {}", query
        );
    }

    /// 10. get is deterministic — calling twice yields same result.
    #[test]
    fn get_is_deterministic(info in arb_model_info()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let name = info.name.clone();
        reg.register(info);
        let first = reg.get(&name).map(|m| (m.name.clone(), m.version.clone(), m.dimension, m.size_bytes));
        let second = reg.get(&name).map(|m| (m.name.clone(), m.version.clone(), m.dimension, m.size_bytes));
        prop_assert_eq!(first, second, "get should be deterministic for name = {}", name);
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 3: Overwrite semantics
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 11. Registering same name twice overwrites (get returns latest).
    #[test]
    fn overwrite_returns_latest(
        name in arb_model_name(),
        info_a in arb_model_info(),
        info_b in arb_model_info(),
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");

        let mut first = info_a;
        first.name = name.clone();
        let mut second = info_b;
        second.name = name.clone();

        let expected_version = second.version.clone();
        let expected_dim = second.dimension;
        let expected_size = second.size_bytes;

        reg.register(first);
        reg.register(second);

        let retrieved = reg.get(&name).unwrap();
        prop_assert_eq!(
            &retrieved.version, &expected_version,
            "overwritten version should match latest, got = {}", retrieved.version
        );
        prop_assert_eq!(
            retrieved.dimension, expected_dim,
            "overwritten dimension should match latest, got = {}", retrieved.dimension
        );
        prop_assert_eq!(
            retrieved.size_bytes, expected_size,
            "overwritten size_bytes should match latest, got = {}", retrieved.size_bytes
        );
    }

    /// 12. After overwrite, list count doesn't increase.
    #[test]
    fn overwrite_does_not_increase_list_count(
        name in arb_model_name(),
        info_a in arb_model_info(),
        info_b in arb_model_info(),
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");

        let mut first = info_a;
        first.name = name.clone();
        reg.register(first);
        let count_after_first = reg.list().len();

        let mut second = info_b;
        second.name = name.clone();
        reg.register(second);
        let count_after_second = reg.list().len();

        prop_assert_eq!(
            count_after_first, count_after_second,
            "overwriting same name should not increase list count"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 4: List invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 13. List length equals number of distinct registered names.
    #[test]
    fn list_length_equals_distinct_names(
        infos in prop::collection::vec(arb_model_info(), 1..20),
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let distinct_names: HashSet<String> = infos.iter().map(|i| i.name.clone()).collect();

        for info in infos {
            reg.register(info);
        }

        prop_assert_eq!(
            reg.list().len(), distinct_names.len(),
            "list length should equal distinct name count = {}", distinct_names.len()
        );
    }

    /// 14. List contains all registered names.
    #[test]
    fn list_contains_all_registered_names(
        infos in prop::collection::vec(arb_model_info(), 1..15),
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let expected_names: HashSet<String> = infos.iter().map(|i| i.name.clone()).collect();

        for info in infos {
            reg.register(info);
        }

        let listed_names: HashSet<String> = reg.list().iter().map(|m| m.name.clone()).collect();
        prop_assert_eq!(
            listed_names, expected_names,
            "listed names should match all registered distinct names"
        );
    }

    /// 15. Empty registry list is empty.
    #[test]
    fn empty_registry_list_is_empty(cache_dir in arb_cache_dir()) {
        let reg = ModelRegistry::new(cache_dir);
        prop_assert!(reg.list().is_empty(), "empty registry should have empty list");
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 5: Multiple registrations
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 16. N distinct registrations → list has N items.
    #[test]
    fn n_distinct_registrations_yield_n_items(n in 1usize..20) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        for i in 0..n {
            reg.register(ModelInfo {
                name: format!("model_{}", i),
                version: "1.0.0".to_string(),
                dimension: 128,
                size_bytes: 1000,
                cache_path: None,
            });
        }
        prop_assert_eq!(
            reg.list().len(), n,
            "expected list length = {}", n
        );
    }

    /// 17. N distinct registrations → all retrievable by name.
    #[test]
    fn n_distinct_registrations_all_retrievable(n in 1usize..20) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        for i in 0..n {
            reg.register(ModelInfo {
                name: format!("model_{}", i),
                version: format!("{}.0.0", i),
                dimension: i * 64,
                size_bytes: i as u64 * 1000,
                cache_path: None,
            });
        }
        for i in 0..n {
            let name = format!("model_{}", i);
            let retrieved = reg.get(&name);
            prop_assert!(
                retrieved.is_some(),
                "model should be retrievable, name = {}", name
            );
            let retrieved = retrieved.unwrap();
            prop_assert_eq!(
                retrieved.dimension, i * 64,
                "dimension mismatch for model_{}", i
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 6: ModelInfo invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// 18. ModelInfo clone preserves all fields.
    #[test]
    fn model_info_clone_preserves_fields(info in arb_model_info()) {
        let cloned = info.clone();
        prop_assert_eq!(&cloned.name, &info.name, "clone name mismatch");
        prop_assert_eq!(&cloned.version, &info.version, "clone version mismatch");
        prop_assert_eq!(cloned.dimension, info.dimension, "clone dimension mismatch");
        prop_assert_eq!(cloned.size_bytes, info.size_bytes, "clone size_bytes mismatch");
        prop_assert_eq!(cloned.cache_path, info.cache_path, "clone cache_path mismatch");
    }

    /// 19. ModelInfo debug is non-empty.
    #[test]
    fn model_info_debug_non_empty(info in arb_model_info()) {
        let debug_str = format!("{:?}", info);
        prop_assert!(!debug_str.is_empty(), "Debug output should be non-empty");
    }

    /// 20. ModelInfo with None cache_path works through register/get cycle.
    #[test]
    fn model_info_none_cache_path_roundtrips(
        name in arb_model_name(),
        version in "[0-9]{1,3}\\.[0-9]{1,3}",
        dimension in 0usize..1024,
        size_bytes in 0u64..5_000_000,
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        let info = ModelInfo {
            name: name.clone(),
            version,
            dimension,
            size_bytes,
            cache_path: None,
        };
        reg.register(info);
        let retrieved = reg.get(&name).unwrap();
        prop_assert!(
            retrieved.cache_path.is_none(),
            "cache_path should remain None after roundtrip"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Group 7: Edge cases
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 21. Empty string name works.
    #[test]
    fn empty_name_works(
        version in "[0-9]{1,3}\\.[0-9]{1,3}",
        dimension in 0usize..512,
        size_bytes in 0u64..1_000_000,
    ) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        reg.register(ModelInfo {
            name: String::new(),
            version,
            dimension,
            size_bytes,
            cache_path: None,
        });
        prop_assert!(
            reg.get("").is_some(),
            "empty string name should be retrievable"
        );
        prop_assert_eq!(reg.list().len(), 1, "list should contain exactly one item");
    }

    /// 22. Very long name works.
    #[test]
    fn very_long_name_works(
        suffix in "[a-z]{200,300}",
        dimension in 0usize..512,
    ) {
        let long_name = format!("model_{}", suffix);
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        reg.register(ModelInfo {
            name: long_name.clone(),
            version: "1.0.0".to_string(),
            dimension,
            size_bytes: 42,
            cache_path: None,
        });
        let retrieved = reg.get(&long_name);
        prop_assert!(
            retrieved.is_some(),
            "very long name should be retrievable, len = {}", long_name.len()
        );
        prop_assert_eq!(
            &retrieved.unwrap().name, &long_name,
            "retrieved name should match the long name"
        );
    }

    /// 23. Zero dimension works.
    #[test]
    fn zero_dimension_works(name in arb_model_name()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        reg.register(ModelInfo {
            name: name.clone(),
            version: "0.0.1".to_string(),
            dimension: 0,
            size_bytes: 100,
            cache_path: None,
        });
        let retrieved = reg.get(&name).unwrap();
        prop_assert_eq!(
            retrieved.dimension, 0,
            "zero dimension should roundtrip correctly"
        );
    }

    /// 24. Zero size_bytes works.
    #[test]
    fn zero_size_bytes_works(name in arb_model_name()) {
        let mut reg = ModelRegistry::new("/tmp/test-registry");
        reg.register(ModelInfo {
            name: name.clone(),
            version: "0.0.1".to_string(),
            dimension: 384,
            size_bytes: 0,
            cache_path: None,
        });
        let retrieved = reg.get(&name).unwrap();
        prop_assert_eq!(
            retrieved.size_bytes, 0,
            "zero size_bytes should roundtrip correctly"
        );
    }
}
