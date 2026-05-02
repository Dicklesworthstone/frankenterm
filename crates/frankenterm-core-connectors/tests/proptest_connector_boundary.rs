use frankenterm_core_connectors::{
    BundleRegistrySnapshot, BundleRegistryTelemetry, CONNECTOR_RUNTIME_IMPORTERS_REMAINING,
    ConnectorExtractionStatus, CredentialAuditType, CredentialBrokerTelemetry,
    IngestionPipelineConfig, IngestionTelemetry, IngestionTelemetrySnapshot,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn connector_audit_type_strategy() -> impl Strategy<Value = CredentialAuditType> {
    prop::sample::select(vec![
        CredentialAuditType::CredentialRegistered,
        CredentialAuditType::LeaseIssued,
        CredentialAuditType::LeaseExpired,
        CredentialAuditType::LeaseRevoked,
        CredentialAuditType::CredentialRotated,
        CredentialAuditType::CredentialRevoked,
        CredentialAuditType::CredentialExpired,
        CredentialAuditType::AccessDenied,
        CredentialAuditType::ProviderRegistered,
        CredentialAuditType::ProviderStatusChanged,
    ])
}

fn small_label() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_.:-]{0,24}"
}

fn small_map() -> impl Strategy<Value = BTreeMap<String, usize>> {
    prop::collection::btree_map(small_label(), 0_usize..=10_000, 0..=16)
}

fn broker_telemetry_strategy() -> impl Strategy<Value = CredentialBrokerTelemetry> {
    any::<[u64; 9]>().prop_map(|values| CredentialBrokerTelemetry {
        leases_issued: values[0],
        leases_expired: values[1],
        leases_revoked: values[2],
        access_denied: values[3],
        rotations_completed: values[4],
        rotations_failed: values[5],
        credentials_registered: values[6],
        credentials_revoked: values[7],
        providers_registered: values[8],
    })
}

fn ingestion_config_strategy() -> impl Strategy<Value = IngestionPipelineConfig> {
    (
        any::<u64>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<u32>(),
        0_usize..=32_768,
    )
        .prop_map(
            |(
                max_ingest_per_sec,
                ingest_lifecycle,
                ingest_inbound,
                ingest_outbound,
                min_severity_level,
                max_audit_entries,
            )| IngestionPipelineConfig {
                max_ingest_per_sec,
                ingest_lifecycle,
                ingest_inbound,
                ingest_outbound,
                min_severity_level,
                max_audit_entries,
            },
        )
}

fn ingestion_telemetry_strategy() -> impl Strategy<Value = IngestionTelemetry> {
    any::<[u64; 7]>().prop_map(|values| IngestionTelemetry {
        events_received: values[0],
        events_recorded: values[1],
        events_filtered: values[2],
        events_rejected: values[3],
        lifecycle_events: values[4],
        inbound_events: values[5],
        outbound_events: values[6],
    })
}

fn bundle_telemetry_strategy() -> impl Strategy<Value = BundleRegistryTelemetry> {
    any::<[u64; 5]>().prop_map(|values| BundleRegistryTelemetry {
        bundles_registered: values[0],
        bundles_removed: values[1],
        bundles_updated: values[2],
        validations_run: values[3],
        validation_failures: values[4],
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_connector_boundary_status_matches_exported_runtime_importers(index in any::<usize>()) {
        let status = ConnectorExtractionStatus::current();

        prop_assert_eq!(status.type_crate, "frankenterm-core-connector-types");
        prop_assert_eq!(status.runtime_importers_remaining, CONNECTOR_RUNTIME_IMPORTERS_REMAINING);
        prop_assert!(!status.runtime_importers_remaining.is_empty());

        let importer = status.runtime_importers_remaining[index % status.runtime_importers_remaining.len()];
        prop_assert!(importer.ends_with(".rs"));
        prop_assert!(!importer.starts_with('/'));
        prop_assert!(!importer.contains(".."));
    }

    #[test]
    fn proptest_connector_boundary_audit_type_display_and_json_are_stable(kind in connector_audit_type_strategy()) {
        let display = kind.to_string();
        let encoded = serde_json::to_string(&kind).expect("audit kind should serialize");
        let decoded: CredentialAuditType =
            serde_json::from_str(&encoded).expect("audit kind should deserialize");

        prop_assert_eq!(decoded, kind);
        prop_assert_eq!(encoded, format!("\"{display}\""));
        prop_assert!(display.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'));
    }

    #[test]
    fn proptest_connector_boundary_broker_telemetry_roundtrips(counters in broker_telemetry_strategy()) {
        let encoded = serde_json::to_string(&counters).expect("broker telemetry should serialize");
        let decoded: CredentialBrokerTelemetry =
            serde_json::from_str(&encoded).expect("broker telemetry should deserialize");

        prop_assert_eq!(decoded, counters);
        prop_assert_eq!(
            decoded.leases_issued
                .saturating_add(decoded.leases_expired)
                .saturating_add(decoded.leases_revoked),
            counters
                .leases_issued
                .saturating_add(counters.leases_expired)
                .saturating_add(counters.leases_revoked),
        );
    }

    #[test]
    fn proptest_connector_boundary_ingestion_snapshot_roundtrips(
        captured_at_ms in any::<u64>(),
        counters in ingestion_telemetry_strategy(),
        audit_chain_length in 0_usize..=65_536,
        pipeline_config in ingestion_config_strategy(),
    ) {
        let snapshot = IngestionTelemetrySnapshot {
            captured_at_ms,
            counters,
            audit_chain_length,
            pipeline_config,
        };

        let encoded = serde_json::to_string(&snapshot).expect("ingestion snapshot should serialize");
        let decoded: IngestionTelemetrySnapshot =
            serde_json::from_str(&encoded).expect("ingestion snapshot should deserialize");

        prop_assert_eq!(decoded, snapshot);
        prop_assert_eq!(decoded.pipeline_config.max_audit_entries, snapshot.pipeline_config.max_audit_entries);
    }

    #[test]
    fn proptest_connector_boundary_bundle_registry_snapshot_roundtrips(
        captured_at_ms in any::<u64>(),
        counters in bundle_telemetry_strategy(),
        bundle_count in 0_usize..=65_536,
        audit_log_length in 0_usize..=65_536,
        bundles_by_tier in small_map(),
        bundles_by_category in small_map(),
    ) {
        let snapshot = BundleRegistrySnapshot {
            captured_at_ms,
            counters,
            bundle_count,
            audit_log_length,
            bundles_by_tier,
            bundles_by_category,
        };

        let encoded = serde_json::to_string(&snapshot).expect("bundle snapshot should serialize");
        let decoded: BundleRegistrySnapshot =
            serde_json::from_str(&encoded).expect("bundle snapshot should deserialize");

        prop_assert_eq!(decoded, snapshot);
        prop_assert_eq!(decoded.bundles_by_tier.keys().collect::<Vec<_>>(), snapshot.bundles_by_tier.keys().collect::<Vec<_>>());
        prop_assert_eq!(decoded.bundles_by_category.keys().collect::<Vec<_>>(), snapshot.bundles_by_category.keys().collect::<Vec<_>>());
    }
}
