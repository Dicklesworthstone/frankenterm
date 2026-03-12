//! Property-based tests for the connector_bundles module.
//!
//! Tests serde roundtrips for BundleTier, BundleCategory, BundleConnectorEntry,
//! ConnectorBundle, BundleValidationResult, IngestionOutcome, IngestionPipelineConfig,
//! IngestionTelemetry, IngestionTelemetrySnapshot, BundleAuditAction, BundleAuditEntry,
//! BundleRegistryConfig, BundleRegistryTelemetry, BundleRegistrySnapshot, and
//! behavioral invariants for validation, registry, and ingestion pipeline.

use std::collections::BTreeMap;

use frankenterm_core::connector_bundles::*;
use frankenterm_core::connector_host_runtime::ConnectorCapability;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_bundle_tier() -> impl Strategy<Value = BundleTier> {
    prop_oneof![
        Just(BundleTier::Tier1),
        Just(BundleTier::Tier2),
        Just(BundleTier::Tier3),
        Just(BundleTier::Custom),
    ]
}

fn arb_bundle_category() -> impl Strategy<Value = BundleCategory> {
    prop_oneof![
        Just(BundleCategory::SourceControl),
        Just(BundleCategory::Messaging),
        Just(BundleCategory::ProjectManagement),
        Just(BundleCategory::Knowledge),
        Just(BundleCategory::CiCd),
        Just(BundleCategory::Email),
        Just(BundleCategory::Monitoring),
        Just(BundleCategory::General),
    ]
}

fn arb_connector_capability() -> impl Strategy<Value = ConnectorCapability> {
    prop_oneof![
        Just(ConnectorCapability::Invoke),
        Just(ConnectorCapability::ReadState),
        Just(ConnectorCapability::StreamEvents),
        Just(ConnectorCapability::FilesystemRead),
        Just(ConnectorCapability::FilesystemWrite),
        Just(ConnectorCapability::NetworkEgress),
        Just(ConnectorCapability::SecretBroker),
        Just(ConnectorCapability::ProcessExec),
    ]
}

fn arb_bundle_connector_entry() -> impl Strategy<Value = BundleConnectorEntry> {
    (
        "[a-z][a-z0-9_-]{2,20}",
        "[A-Z][a-zA-Z ]{2,20}",
        "[0-9]+\\.[0-9]+\\.[0-9]+",
        any::<bool>(),
        proptest::collection::vec(arb_connector_capability(), 0..4),
        proptest::collection::btree_map("[a-z]{2,8}", "[a-z]{2,8}", 0..3),
    )
        .prop_map(
            |(package_id, display_name, min_version, required, caps, metadata)| {
                BundleConnectorEntry {
                    package_id,
                    display_name,
                    min_version,
                    required,
                    required_capabilities: caps,
                    manifest_snapshot: None,
                    metadata,
                }
            },
        )
}

fn arb_connector_bundle() -> impl Strategy<Value = ConnectorBundle> {
    (
        "[a-z][a-z0-9_-]{3,20}",
        "[A-Z][a-zA-Z ]{2,20}",
        "[a-zA-Z ]{0,50}",
        "[0-9]+\\.[0-9]+\\.[0-9]+",
        arb_bundle_tier(),
        arb_bundle_category(),
        "[a-z]{3,15}",
        any::<u64>(),
        proptest::collection::vec(arb_bundle_connector_entry(), 1..5),
        proptest::collection::btree_set("[a-z]{2,8}", 0..4),
    )
        .prop_map(
            |(
                bundle_id,
                display_name,
                description,
                version,
                tier,
                category,
                author,
                now_ms,
                connectors,
                labels,
            )| {
                ConnectorBundle {
                    bundle_id,
                    display_name,
                    description,
                    version,
                    tier,
                    category,
                    author,
                    min_ft_version: None,
                    connectors,
                    labels,
                    created_at_ms: now_ms,
                    updated_at_ms: now_ms,
                    metadata: BTreeMap::new(),
                }
            },
        )
}

fn arb_ingestion_outcome() -> impl Strategy<Value = IngestionOutcome> {
    prop_oneof![
        Just(IngestionOutcome::Recorded),
        Just(IngestionOutcome::Filtered),
        "[a-z ]{3,30}".prop_map(|reason| IngestionOutcome::Rejected { reason }),
    ]
}

fn arb_ingestion_pipeline_config() -> impl Strategy<Value = IngestionPipelineConfig> {
    (
        0..10000u64,
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        0..10u32,
        1..10000usize,
    )
        .prop_map(
            |(
                max_ingest_per_sec,
                ingest_lifecycle,
                ingest_inbound,
                ingest_outbound,
                min_severity_level,
                max_audit_entries,
            )| {
                IngestionPipelineConfig {
                    max_ingest_per_sec,
                    ingest_lifecycle,
                    ingest_inbound,
                    ingest_outbound,
                    min_severity_level,
                    max_audit_entries,
                }
            },
        )
}

fn arb_ingestion_telemetry() -> impl Strategy<Value = IngestionTelemetry> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(received, recorded, filtered, rejected, lifecycle, inbound, outbound)| {
                IngestionTelemetry {
                    events_received: received,
                    events_recorded: recorded,
                    events_filtered: filtered,
                    events_rejected: rejected,
                    lifecycle_events: lifecycle,
                    inbound_events: inbound,
                    outbound_events: outbound,
                }
            },
        )
}

fn arb_ingestion_telemetry_snapshot() -> impl Strategy<Value = IngestionTelemetrySnapshot> {
    (
        any::<u64>(),
        arb_ingestion_telemetry(),
        0..10000usize,
        arb_ingestion_pipeline_config(),
    )
        .prop_map(|(captured_at_ms, counters, chain_length, config)| {
            IngestionTelemetrySnapshot {
                captured_at_ms,
                counters,
                audit_chain_length: chain_length,
                pipeline_config: config,
            }
        })
}

fn arb_bundle_audit_action() -> impl Strategy<Value = BundleAuditAction> {
    prop_oneof![
        Just(BundleAuditAction::Registered),
        Just(BundleAuditAction::Updated),
        Just(BundleAuditAction::Removed),
        Just(BundleAuditAction::Validated),
        Just(BundleAuditAction::ConnectorActivated),
        Just(BundleAuditAction::ConnectorDeactivated),
    ]
}

fn arb_bundle_audit_entry() -> impl Strategy<Value = BundleAuditEntry> {
    (
        arb_bundle_audit_action(),
        "[a-z_-]{3,20}",
        "[a-z_-]{3,15}",
        any::<u64>(),
        "[a-zA-Z ]{0,30}",
    )
        .prop_map(
            |(action, bundle_id, actor, timestamp_ms, detail)| BundleAuditEntry {
                action,
                bundle_id,
                actor,
                timestamp_ms,
                detail,
            },
        )
}

fn arb_bundle_registry_config() -> impl Strategy<Value = BundleRegistryConfig> {
    (1..1000usize, 1..5000usize).prop_map(|(max_bundles, max_audit_entries)| BundleRegistryConfig {
        max_bundles,
        max_audit_entries,
    })
}

fn arb_bundle_registry_telemetry() -> impl Strategy<Value = BundleRegistryTelemetry> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(|(registered, removed, updated, validations, failures)| {
            BundleRegistryTelemetry {
                bundles_registered: registered,
                bundles_removed: removed,
                bundles_updated: updated,
                validations_run: validations,
                validation_failures: failures,
            }
        })
}

fn arb_bundle_registry_snapshot() -> impl Strategy<Value = BundleRegistrySnapshot> {
    (
        any::<u64>(),
        arb_bundle_registry_telemetry(),
        0..500usize,
        0..2000usize,
        proptest::collection::btree_map("[a-z]{3,8}", 0..100usize, 0..5),
        proptest::collection::btree_map("[a-z]{3,8}", 0..100usize, 0..5),
    )
        .prop_map(
            |(captured_at_ms, counters, count, audit_len, by_tier, by_cat)| {
                BundleRegistrySnapshot {
                    captured_at_ms,
                    counters,
                    bundle_count: count,
                    audit_log_length: audit_len,
                    bundles_by_tier: by_tier,
                    bundles_by_category: by_cat,
                }
            },
        )
}

fn arb_bundle_validation_result() -> impl Strategy<Value = BundleValidationResult> {
    prop_oneof![
        Just(BundleValidationResult::ok()),
        proptest::collection::vec("[a-z ]{3,20}", 1..5).prop_map(|errors| {
            let mut r = BundleValidationResult::ok();
            for e in errors {
                r.error(e);
            }
            r
        }),
        proptest::collection::vec("[a-z ]{3,20}", 1..3).prop_map(|warnings| {
            let mut r = BundleValidationResult::ok();
            for w in warnings {
                r.warn(w);
            }
            r
        }),
    ]
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn bundle_tier_serde_roundtrip(tier in arb_bundle_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: BundleTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    #[test]
    fn bundle_category_serde_roundtrip(cat in arb_bundle_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: BundleCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn bundle_connector_entry_serde_roundtrip(entry in arb_bundle_connector_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: BundleConnectorEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry, back);
    }

    #[test]
    fn connector_bundle_serde_roundtrip(bundle in arb_connector_bundle()) {
        let json = serde_json::to_string(&bundle).unwrap();
        let back: ConnectorBundle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(bundle, back);
    }

    #[test]
    fn ingestion_outcome_serde_roundtrip(outcome in arb_ingestion_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: IngestionOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, back);
    }

    #[test]
    fn ingestion_pipeline_config_serde_roundtrip(config in arb_ingestion_pipeline_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: IngestionPipelineConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn ingestion_telemetry_serde_roundtrip(telem in arb_ingestion_telemetry()) {
        let json = serde_json::to_string(&telem).unwrap();
        let back: IngestionTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(telem, back);
    }

    #[test]
    fn ingestion_telemetry_snapshot_serde_roundtrip(snap in arb_ingestion_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: IngestionTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn bundle_audit_action_serde_roundtrip(action in arb_bundle_audit_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: BundleAuditAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, back);
    }

    #[test]
    fn bundle_audit_entry_serde_roundtrip(entry in arb_bundle_audit_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: BundleAuditEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry, back);
    }

    #[test]
    fn bundle_registry_config_serde_roundtrip(config in arb_bundle_registry_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: BundleRegistryConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn bundle_registry_telemetry_serde_roundtrip(telem in arb_bundle_registry_telemetry()) {
        let json = serde_json::to_string(&telem).unwrap();
        let back: BundleRegistryTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(telem, back);
    }

    #[test]
    fn bundle_registry_snapshot_serde_roundtrip(snap in arb_bundle_registry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: BundleRegistrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn bundle_validation_result_serde_roundtrip(result in arb_bundle_validation_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: BundleValidationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result, back);
    }
}

// =============================================================================
// Display property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn bundle_tier_display_nonempty(tier in arb_bundle_tier()) {
        let s = tier.to_string();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn bundle_category_display_nonempty(cat in arb_bundle_category()) {
        let s = cat.to_string();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn ingestion_outcome_display_nonempty(outcome in arb_ingestion_outcome()) {
        let s = outcome.to_string();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn bundle_audit_action_display_nonempty(action in arb_bundle_audit_action()) {
        let s = action.to_string();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn bundle_validation_result_display_consistency(result in arb_bundle_validation_result()) {
        let s = result.to_string();
        if result.valid {
            prop_assert!(s.contains("valid"));
        } else {
            prop_assert!(s.contains("INVALID"));
        }
    }
}

// =============================================================================
// Behavioral invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn bundle_tier_ord_is_total(a in arb_bundle_tier(), b in arb_bundle_tier()) {
        // Ord is total: a <= b OR b <= a
        prop_assert!(a <= b || b <= a);
    }

    #[test]
    fn tier_trust_monotonicity(tier in arb_bundle_tier()) {
        // Higher tiers (Tier1/Tier2) require Trusted, lower require less.
        let trust = tier.minimum_trust();
        let trust_str = format!("{trust:?}");
        prop_assert!(!trust_str.is_empty());
        // Tier1/Tier2 require signed, Tier3/Custom don't.
        match tier {
            BundleTier::Tier1 | BundleTier::Tier2 => {
                prop_assert!(tier.requires_signed_manifest());
            }
            BundleTier::Tier3 | BundleTier::Custom => {
                prop_assert!(!tier.requires_signed_manifest());
            }
        }
    }

    #[test]
    fn connector_entry_builder_dedup_caps(
        cap in arb_connector_capability(),
    ) {
        let entry = BundleConnectorEntry::required("test", "Test")
            .with_capability(cap)
            .with_capability(cap);
        // Dedup: adding same cap twice should only store one.
        prop_assert_eq!(entry.required_capabilities.len(), 1);
    }

    #[test]
    fn bundle_count_invariants(bundle in arb_connector_bundle()) {
        // required_count + optional_count == connector_count
        prop_assert_eq!(
            bundle.required_count() + bundle.optional_count(),
            bundle.connector_count()
        );
    }

    #[test]
    fn bundle_package_ids_len_matches_count(bundle in arb_connector_bundle()) {
        // package_ids() returns one entry per connector
        prop_assert_eq!(bundle.package_ids().len(), bundle.connector_count());
    }

    #[test]
    fn validate_empty_bundle_id_is_invalid(
        cat in arb_bundle_category(),
        tier in arb_bundle_tier(),
    ) {
        let mut bundle = ConnectorBundle::new("", "Test", tier, cat, 0);
        bundle.add_connector(BundleConnectorEntry::required("c", "C"));
        let result = validate_bundle(&bundle);
        prop_assert!(!result.valid);
    }

    #[test]
    fn validate_empty_connectors_is_invalid(
        id in "[a-z]{3,10}",
        tier in arb_bundle_tier(),
        cat in arb_bundle_category(),
    ) {
        let bundle = ConnectorBundle::new(id, "Test", tier, cat, 0);
        let result = validate_bundle(&bundle);
        prop_assert!(!result.valid);
    }

    #[test]
    fn validate_tier1_bundles_always_valid(now_ms in any::<u64>()) {
        // The built-in tier1 factory functions should produce valid bundles.
        let bundles = vec![
            tier1_devtools_bundle(now_ms),
            tier1_comms_bundle(now_ms),
            tier1_observability_bundle(now_ms),
        ];
        for bundle in bundles {
            let result = validate_bundle(&bundle);
            prop_assert!(result.valid, "tier1 bundle {} is invalid: {:?}", bundle.bundle_id, result.errors);
        }
    }

    #[test]
    fn registry_register_then_get_consistent(
        tier in arb_bundle_tier(),
        cat in arb_bundle_category(),
        now_ms in any::<u64>(),
    ) {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let mut bundle = ConnectorBundle::new("test-bundle", "Test", tier, cat, now_ms);
        bundle.add_connector(BundleConnectorEntry::required("c1", "C1"));
        let _ = reg.register(bundle.clone(), "agent", now_ms);
        if let Some(got) = reg.get("test-bundle") {
            prop_assert_eq!(&bundle, got);
        }
    }

    #[test]
    fn registry_snapshot_count_matches_len(
        tier in arb_bundle_tier(),
        cat in arb_bundle_category(),
    ) {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let mut b1 = ConnectorBundle::new("b1", "B1", tier, cat, 1000);
        b1.add_connector(BundleConnectorEntry::required("c1", "C1"));
        let _ = reg.register(b1, "a", 1000);
        let snap = reg.snapshot(2000);
        prop_assert_eq!(snap.bundle_count, reg.len());
    }

    #[test]
    fn registry_audit_log_bounded(
        max_entries in 1..10usize,
        n_ops in 1..20usize,
    ) {
        let config = BundleRegistryConfig {
            max_bundles: 256,
            max_audit_entries: max_entries,
        };
        let mut reg = BundleRegistry::new(config);
        for i in 0..n_ops {
            let id = format!("b-{i}");
            let mut bundle = ConnectorBundle::new(
                &id,
                "Test",
                BundleTier::Custom,
                BundleCategory::General,
                i as u64 * 1000,
            );
            bundle.add_connector(BundleConnectorEntry::required(
                format!("c-{i}"),
                "C",
            ));
            let _ = reg.register(bundle, "a", i as u64 * 1000);
        }
        // Audit log should never exceed max_entries.
        prop_assert!(reg.audit_log().len() <= max_entries);
    }

    #[test]
    fn ingestion_pipeline_config_default_has_sane_values(_dummy in 0..1u8) {
        let config = IngestionPipelineConfig::default();
        prop_assert_eq!(config.max_ingest_per_sec, 0); // unlimited
        prop_assert!(config.ingest_lifecycle);
        prop_assert!(config.ingest_inbound);
        prop_assert!(config.ingest_outbound);
        prop_assert_eq!(config.min_severity_level, 0);
        prop_assert!(config.max_audit_entries > 0);
    }
}
