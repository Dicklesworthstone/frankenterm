use frankenterm_core_connectors::{
    BundleRegistrySnapshot, BundleRegistryTelemetry, CONNECTOR_RUNTIME_IMPORTERS_REMAINING,
    ConnectorExtractionStatus, ConnectorGovernorSnapshot, CostBudgetSnapshot, CredentialAuditType,
    CredentialBrokerTelemetry, GovernorSnapshot, GovernorTelemetrySnapshot,
    IngestionPipelineConfig, IngestionTelemetry, IngestionTelemetrySnapshot,
    QueueBackpressureSnapshot, QuotaSnapshot,
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

fn finite_ratio() -> impl Strategy<Value = f64> {
    prop::sample::select(vec![0.0, 0.125, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0])
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

fn governor_snapshot_strategy() -> impl Strategy<Value = GovernorSnapshot> {
    (
        finite_ratio(),
        finite_ratio(),
        finite_ratio(),
        finite_ratio(),
        finite_ratio(),
    )
        .prop_map(
            |(
                global_rate_fill_ratio,
                global_quota_usage,
                queue_depth_fraction,
                connector_rate_fill,
                connector_quota_usage,
            )| GovernorSnapshot {
                global_rate_fill_ratio,
                global_quota: QuotaSnapshot {
                    used: 4,
                    max: 8,
                    remaining: 4,
                    usage_fraction: global_quota_usage,
                    total_lifetime: 16,
                    window_ms: 60_000,
                },
                queue: QueueBackpressureSnapshot {
                    current_depth: 2,
                    max_depth: 8,
                    peak_depth: 4,
                    depth_fraction: queue_depth_fraction,
                    total_enqueued: 32,
                    total_rejected: 1,
                },
                connectors: vec![ConnectorGovernorSnapshot {
                    connector_id: "connector.alpha".to_owned(),
                    rate_limit_fill_ratio: connector_rate_fill,
                    quota: QuotaSnapshot {
                        used: 3,
                        max: 6,
                        remaining: 3,
                        usage_fraction: connector_quota_usage,
                        total_lifetime: 12,
                        window_ms: 30_000,
                    },
                    cost: CostBudgetSnapshot {
                        window_cost_cents: 125,
                        max_cost_cents: 500,
                        remaining_cents: 375,
                        usage_fraction: 0.25,
                        total_lifetime_cents: 1_000,
                        window_ms: 30_000,
                    },
                    backoff_active: false,
                    backoff_remaining_ms: 0,
                    consecutive_failures: 0,
                }],
                telemetry: GovernorTelemetrySnapshot {
                    evaluations: 8,
                    allows: 7,
                    throttles: 1,
                    rejections: 0,
                },
            },
        )
}

#[test]
fn connector_snapshot_ratio_serialization_rejects_non_finite_values() {
    let quota = QuotaSnapshot {
        used: 0,
        max: 1,
        remaining: 1,
        usage_fraction: f64::NAN,
        total_lifetime: 0,
        window_ms: 1,
    };
    assert!(serde_json::to_string(&quota).is_err());

    let cost = CostBudgetSnapshot {
        window_cost_cents: 0,
        max_cost_cents: 1,
        remaining_cents: 1,
        usage_fraction: f64::INFINITY,
        total_lifetime_cents: 0,
        window_ms: 1,
    };
    assert!(serde_json::to_string(&cost).is_err());

    let queue = QueueBackpressureSnapshot {
        current_depth: 0,
        max_depth: 1,
        peak_depth: 0,
        depth_fraction: f64::NEG_INFINITY,
        total_enqueued: 0,
        total_rejected: 0,
    };
    assert!(serde_json::to_string(&queue).is_err());

    let connector = ConnectorGovernorSnapshot {
        connector_id: "connector.alpha".to_owned(),
        rate_limit_fill_ratio: f64::NAN,
        quota: QuotaSnapshot {
            used: 0,
            max: 1,
            remaining: 1,
            usage_fraction: 0.0,
            total_lifetime: 0,
            window_ms: 1,
        },
        cost: CostBudgetSnapshot {
            window_cost_cents: 0,
            max_cost_cents: 1,
            remaining_cents: 1,
            usage_fraction: 0.0,
            total_lifetime_cents: 0,
            window_ms: 1,
        },
        backoff_active: false,
        backoff_remaining_ms: 0,
        consecutive_failures: 0,
    };
    assert!(serde_json::to_string(&connector).is_err());

    let governor = GovernorSnapshot {
        global_rate_fill_ratio: f64::INFINITY,
        global_quota: quota_with_ratio(0.0),
        queue: QueueBackpressureSnapshot {
            current_depth: 0,
            max_depth: 1,
            peak_depth: 0,
            depth_fraction: 0.0,
            total_enqueued: 0,
            total_rejected: 0,
        },
        connectors: Vec::new(),
        telemetry: GovernorTelemetrySnapshot {
            evaluations: 0,
            allows: 0,
            throttles: 0,
            rejections: 0,
        },
    };
    assert!(serde_json::to_string(&governor).is_err());
}

fn quota_with_ratio(usage_fraction: f64) -> QuotaSnapshot {
    QuotaSnapshot {
        used: 0,
        max: 1,
        remaining: 1,
        usage_fraction,
        total_lifetime: 0,
        window_ms: 1,
    }
}

#[test]
fn connector_snapshot_ratio_deserialization_rejects_json_null() {
    let input = r#"{
        "used": 0,
        "max": 1,
        "remaining": 1,
        "usage_fraction": null,
        "total_lifetime": 0,
        "window_ms": 1
    }"#;

    let result = serde_json::from_str::<QuotaSnapshot>(input);

    assert!(result.is_err());
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
        prop_assert!(
            std::path::Path::new(importer)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        );
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

        prop_assert_eq!(&decoded, &counters);
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

        prop_assert_eq!(&decoded, &snapshot);
        prop_assert_eq!(
            decoded.pipeline_config.max_audit_entries,
            snapshot.pipeline_config.max_audit_entries
        );
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

        prop_assert_eq!(&decoded, &snapshot);
        prop_assert_eq!(
            decoded.bundles_by_tier.keys().collect::<Vec<_>>(),
            snapshot.bundles_by_tier.keys().collect::<Vec<_>>()
        );
        prop_assert_eq!(
            decoded.bundles_by_category.keys().collect::<Vec<_>>(),
            snapshot.bundles_by_category.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn proptest_connector_boundary_governor_snapshot_roundtrips(snapshot in governor_snapshot_strategy()) {
        let encoded = serde_json::to_string(&snapshot).expect("governor snapshot should serialize");
        let decoded: GovernorSnapshot =
            serde_json::from_str(&encoded).expect("governor snapshot should deserialize");

        prop_assert_eq!(
            decoded.global_rate_fill_ratio.to_bits(),
            snapshot.global_rate_fill_ratio.to_bits()
        );
        prop_assert_eq!(
            decoded.global_quota.usage_fraction.to_bits(),
            snapshot.global_quota.usage_fraction.to_bits()
        );
        prop_assert_eq!(
            decoded.queue.depth_fraction.to_bits(),
            snapshot.queue.depth_fraction.to_bits()
        );
        prop_assert_eq!(
            decoded.connectors[0].rate_limit_fill_ratio.to_bits(),
            snapshot.connectors[0].rate_limit_fill_ratio.to_bits()
        );
        prop_assert_eq!(
            decoded.connectors[0].quota.usage_fraction.to_bits(),
            snapshot.connectors[0].quota.usage_fraction.to_bits()
        );
        prop_assert_eq!(
            decoded.connectors[0].cost.usage_fraction.to_bits(),
            snapshot.connectors[0].cost.usage_fraction.to_bits()
        );
    }
}
