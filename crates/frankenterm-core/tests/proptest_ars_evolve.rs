//! Property-based tests for ARS Reflex Evolution Engine.
//!
//! Verifies versioning invariants, lineage correctness, status transitions,
//! deprecation logic, and serde roundtrips.

use proptest::prelude::*;

use std::collections::HashMap;

use frankenterm_core::ars_evolve::{
    CreationReason, EvolutionConfig, EvolutionEngine, EvolutionRequest, EvolutionResult,
    EvolutionStats, ReflexVersion, VersionStatus,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_status() -> impl Strategy<Value = VersionStatus> {
    prop_oneof![
        Just(VersionStatus::Active),
        Just(VersionStatus::Incubating),
        Just(VersionStatus::Deprecated),
        Just(VersionStatus::Disabled),
    ]
}

fn arb_trigger_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(1..255u8, 1..20)
}

fn arb_commands() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z]{2,10}", 1..5)
}

fn arb_cluster() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("c1".to_string()),
        Just("c2".to_string()),
        Just("c3".to_string()),
    ]
}

// =============================================================================
// Registration invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn register_assigns_unique_ids(
        n_reflexes in 1..20usize,
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        let mut ids = Vec::new();

        for i in 0..n_reflexes {
            let id = engine.register_original("c1", vec![i as u8], vec!["cmd".into()], 1000);
            ids.push(id);
        }

        // All IDs should be unique.
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        prop_assert_eq!(unique.len(), n_reflexes);
    }

    #[test]
    fn originals_are_v1_active(
        cluster in arb_cluster(),
        key in arb_trigger_key(),
        cmds in arb_commands(),
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        let id = engine.register_original(&cluster, key, cmds, 1000);
        let v = engine.get_version(id).unwrap();

        prop_assert_eq!(v.version, 1);
        prop_assert_eq!(v.status, VersionStatus::Active);
        prop_assert!(v.parent_reflex_id.is_none());
        prop_assert!(v.parent_version.is_none());
        let is_original = matches!(v.creation_reason, CreationReason::Original);
        prop_assert!(is_original);
    }

    #[test]
    fn reflex_count_matches_registrations(
        n in 1..10usize,
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        for i in 0..n {
            engine.register_original("c1", vec![i as u8], vec!["cmd".into()], 1000);
        }
        prop_assert_eq!(engine.reflex_count(), n);
    }
}

// =============================================================================
// Evolution invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn evolution_increments_version(
        cmds in arb_commands(),
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);

        let req = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: cmds,
            timestamp_ms: 2000,
        };

        let result = engine.evolve(&req);
        if let EvolutionResult::Evolved { new_version, .. } = result {
            prop_assert_eq!(new_version, 2);
        } else {
            prop_assert!(false, "should evolve successfully");
        }
    }

    #[test]
    fn evolution_deprecates_parent_when_configured(
        auto_deprecate in prop::bool::ANY,
    ) {
        let config = EvolutionConfig {
            auto_deprecate_parent: auto_deprecate,
            ..Default::default()
        };
        let mut engine = EvolutionEngine::new(config);
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);

        let req = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: vec!["new".into()],
            timestamp_ms: 2000,
        };
        engine.evolve(&req);

        let parent = engine.get_version(v1).unwrap();
        if auto_deprecate {
            prop_assert_eq!(parent.status, VersionStatus::Deprecated);
        } else {
            prop_assert_eq!(parent.status, VersionStatus::Active);
        }
    }

    #[test]
    fn evolution_creates_incubating_or_active(
        incubate in prop::bool::ANY,
    ) {
        let config = EvolutionConfig {
            incubate_evolutions: incubate,
            ..Default::default()
        };
        let mut engine = EvolutionEngine::new(config);
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);

        let req = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: vec!["new".into()],
            timestamp_ms: 2000,
        };
        let result = engine.evolve(&req);

        if let EvolutionResult::Evolved { new_reflex_id, .. } = result {
            let new = engine.get_version(new_reflex_id).unwrap();
            let expected = if incubate {
                VersionStatus::Incubating
            } else {
                VersionStatus::Active
            };
            prop_assert_eq!(new.status, expected);
        }
    }

    #[test]
    fn double_evolve_fails(
        cmds in arb_commands(),
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);

        let req = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: cmds,
            timestamp_ms: 2000,
        };
        engine.evolve(&req); // First: succeeds, deprecates v1.

        let req2 = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: vec!["newer".into()],
            timestamp_ms: 3000,
        };
        let result = engine.evolve(&req2);
        let is_deprecated = matches!(result, EvolutionResult::AlreadyDeprecated { .. });
        prop_assert!(is_deprecated);
    }

    #[test]
    fn empty_commands_rejected(
        key in arb_trigger_key(),
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        let v1 = engine.register_original("c1", key.clone(), vec!["old".into()], 1000);

        let req = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: key,
            new_commands: vec![],
            timestamp_ms: 2000,
        };
        let result = engine.evolve(&req);
        let is_empty = matches!(result, EvolutionResult::EmptyCommands);
        prop_assert!(is_empty);
    }
}

// =============================================================================
// Lineage invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn lineage_depth_increases_with_evolutions(
        n_evolutions in 1..5usize,
    ) {
        let config = EvolutionConfig {
            max_lineage_depth: 20,
            auto_deprecate_parent: false, // Don't deprecate so we can chain.
            incubate_evolutions: false,   // Active so we can re-evolve.
        };
        let mut engine = EvolutionEngine::new(config);
        let mut current_id = engine.register_original("c1", vec![1], vec!["v1".into()], 1000);

        for i in 0..n_evolutions {
            let req = EvolutionRequest {
                parent_reflex_id: current_id,
                cluster_id: "c1".to_string(),
                new_trigger_key: vec![1],
                new_commands: vec![format!("v{}", i + 2)],
                timestamp_ms: (i as u64 + 2) * 1000,
            };
            let result = engine.evolve(&req);
            if let EvolutionResult::Evolved { new_reflex_id, .. } = result {
                let depth = engine.lineage_depth(new_reflex_id);
                prop_assert_eq!(depth, (i + 1) as u32);
                current_id = new_reflex_id;
            }
        }
    }

    #[test]
    fn lineage_terminates_at_original(
        n in 1..5usize,
    ) {
        let config = EvolutionConfig {
            auto_deprecate_parent: false,
            incubate_evolutions: false,
            ..Default::default()
        };
        let mut engine = EvolutionEngine::new(config);
        let v1 = engine.register_original("c1", vec![1], vec!["v1".into()], 1000);
        let mut current = v1;

        for i in 0..n {
            let req = EvolutionRequest {
                parent_reflex_id: current,
                cluster_id: "c1".to_string(),
                new_trigger_key: vec![1],
                new_commands: vec![format!("v{}", i + 2)],
                timestamp_ms: (i as u64 + 2) * 1000,
            };
            if let EvolutionResult::Evolved { new_reflex_id, .. } = engine.evolve(&req) {
                current = new_reflex_id;
            }
        }

        // Lineage of the last version should end at v1.
        let chain = engine.lineage(current);
        if !chain.is_empty() {
            prop_assert_eq!(*chain.last().unwrap(), v1);
        }
    }

    #[test]
    fn max_depth_enforced(
        max_depth in 1..5u32,
    ) {
        let config = EvolutionConfig {
            max_lineage_depth: max_depth,
            auto_deprecate_parent: false,
            incubate_evolutions: false,
        };
        let mut engine = EvolutionEngine::new(config);
        let mut current = engine.register_original("c1", vec![1], vec!["v1".into()], 1000);

        // Evolve max_depth times (should succeed).
        for i in 0..max_depth {
            let req = EvolutionRequest {
                parent_reflex_id: current,
                cluster_id: "c1".to_string(),
                new_trigger_key: vec![1],
                new_commands: vec![format!("v{}", i + 2)],
                timestamp_ms: (i as u64 + 2) * 1000,
            };
            if let EvolutionResult::Evolved { new_reflex_id, .. } = engine.evolve(&req) {
                current = new_reflex_id;
            }
        }

        // One more should fail.
        let req = EvolutionRequest {
            parent_reflex_id: current,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: vec!["overflow".into()],
            timestamp_ms: 99999,
        };
        let result = engine.evolve(&req);
        let is_too_deep = matches!(result, EvolutionResult::LineageTooDeep { .. });
        prop_assert!(is_too_deep);
    }
}

// =============================================================================
// Status transition invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn usable_statuses(status in arb_status()) {
        let expected = matches!(status, VersionStatus::Active | VersionStatus::Incubating);
        prop_assert_eq!(status.is_usable(), expected);
    }

    #[test]
    fn promote_only_from_incubating(status in arb_status()) {
        let config = EvolutionConfig {
            incubate_evolutions: true,
            ..Default::default()
        };
        let mut engine = EvolutionEngine::new(config);
        let id = engine.register_original("c1", vec![1], vec!["a".into()], 1000);

        // Force status.
        if status != VersionStatus::Active {
            match status {
                VersionStatus::Incubating => {
                    // Evolve to get incubating.
                    let req = EvolutionRequest {
                        parent_reflex_id: id,
                        cluster_id: "c1".to_string(),
                        new_trigger_key: vec![1],
                        new_commands: vec!["new".into()],
                        timestamp_ms: 2000,
                    };
                    if let EvolutionResult::Evolved { new_reflex_id, .. } = engine.evolve(&req) {
                        let promoted = engine.promote(new_reflex_id);
                        // Should succeed — was Incubating.
                        prop_assert!(promoted);
                    }
                }
                _ => {
                    // Active can't be promoted.
                    let promoted = engine.promote(id);
                    prop_assert!(!promoted);
                }
            }
        } else {
            let promoted = engine.promote(id);
            prop_assert!(!promoted, "Active should not be promotable");
        }
    }
}

// =============================================================================
// Stats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn stats_reflex_count_matches(
        n in 1..10usize,
    ) {
        let mut engine = EvolutionEngine::with_defaults();
        for i in 0..n {
            engine.register_original("c1", vec![i as u8], vec!["cmd".into()], 1000);
        }
        let stats = engine.stats();
        prop_assert_eq!(stats.total_reflexes, n);
    }

    #[test]
    fn stats_by_status_sums_to_total(
        n_originals in 1..5usize,
        n_evolve in 0..3usize,
    ) {
        let config = EvolutionConfig {
            auto_deprecate_parent: true,
            incubate_evolutions: true,
            ..Default::default()
        };
        let mut engine = EvolutionEngine::new(config);
        let mut ids = Vec::new();
        for i in 0..n_originals {
            let id = engine.register_original("c1", vec![i as u8], vec!["cmd".into()], 1000);
            ids.push(id);
        }

        for (i, &parent_id) in ids.iter().enumerate().take(n_evolve.min(n_originals)) {
            let req = EvolutionRequest {
                parent_reflex_id: parent_id,
                cluster_id: "c1".to_string(),
                new_trigger_key: vec![i as u8],
                new_commands: vec!["new".into()],
                timestamp_ms: 2000,
            };
            engine.evolve(&req);
        }

        let stats = engine.stats();
        let status_sum: usize = stats.by_status.values().sum();
        prop_assert_eq!(status_sum, stats.total_reflexes);
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn version_status_serde_roundtrip(status in arb_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let decoded: VersionStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, status);
    }

    #[test]
    fn reflex_version_serde_roundtrip(
        version in 1..100u32,
        cluster in arb_cluster(),
        key in arb_trigger_key(),
        cmds in arb_commands(),
        status in arb_status(),
    ) {
        let v = ReflexVersion {
            reflex_id: 1,
            version,
            cluster_id: cluster,
            trigger_key: key,
            commands: cmds,
            parent_version: if version > 1 { Some(version - 1) } else { None },
            parent_reflex_id: if version > 1 { Some(0) } else { None },
            status,
            creation_reason: CreationReason::Original,
            created_at_ms: 1000,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ReflexVersion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, v);
    }

    #[test]
    fn evolution_result_serde_roundtrip(
        new_id in 1..1000u64,
        ver in 1..100u32,
        parent_id in 1..1000u64,
    ) {
        let results = vec![
            EvolutionResult::Evolved {
                new_reflex_id: new_id,
                new_version: ver,
                deprecated_reflex_id: parent_id,
            },
            EvolutionResult::ParentNotFound { reflex_id: parent_id },
            EvolutionResult::AlreadyDeprecated { reflex_id: parent_id },
            EvolutionResult::EmptyCommands,
            EvolutionResult::LineageTooDeep { depth: ver, max_depth: ver + 1 },
        ];
        for result in results {
            let json = serde_json::to_string(&result).unwrap();
            let decoded: EvolutionResult = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(decoded, result);
        }
    }

    #[test]
    fn evolution_stats_serde_roundtrip(
        total in 0..100usize,
        evolutions in 0..50u64,
        deprecations in 0..50u64,
    ) {
        let stats = EvolutionStats {
            total_reflexes: total,
            total_evolutions: evolutions,
            total_deprecations: deprecations,
            by_status: HashMap::from([("Active".to_string(), total)]),
            max_lineage_depth: 3,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: EvolutionStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }

    #[test]
    fn config_serde_roundtrip(
        max_depth in 1..50u32,
        auto_dep in prop::bool::ANY,
        incubate in prop::bool::ANY,
    ) {
        let config = EvolutionConfig {
            max_lineage_depth: max_depth,
            auto_deprecate_parent: auto_dep,
            incubate_evolutions: incubate,
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: EvolutionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.max_lineage_depth, config.max_lineage_depth);
        prop_assert_eq!(decoded.auto_deprecate_parent, config.auto_deprecate_parent);
        prop_assert_eq!(decoded.incubate_evolutions, config.incubate_evolutions);
    }
}
