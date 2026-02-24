//! Property-based tests for ARS GitOps Federation & Webhook Dispatcher.
//!
//! Verifies export filtering, import validation, webhook dispatch,
//! hex encoding/decoding, and serde roundtrips.

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::ars_blast_radius::MaturityTier;
use frankenterm_core::ars_drift::EValueConfig;
use frankenterm_core::ars_evidence::EvidenceVerdict;
use frankenterm_core::ars_evolve::VersionStatus;
use frankenterm_core::ars_federation::{
    DeliveryStatus, FederationConfig, FederationEngine, FederationEvent, FederationEventKind,
    FederationStats, ImportResult, ReflexExport, WebhookConfig, WebhookKind,
};
use frankenterm_core::ars_serialize::{
    DriftSnapshot, EvidenceSummary, ReflexRecord, ReflexStore,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_maturity() -> impl Strategy<Value = MaturityTier> {
    prop_oneof![
        Just(MaturityTier::Incubating),
        Just(MaturityTier::Graduated),
        Just(MaturityTier::Veteran),
    ]
}

fn arb_webhook_kind() -> impl Strategy<Value = WebhookKind> {
    prop_oneof![
        Just(WebhookKind::Slack),
        Just(WebhookKind::Discord),
        Just(WebhookKind::Generic),
    ]
}

fn arb_event_kind() -> impl Strategy<Value = FederationEventKind> {
    prop_oneof![
        Just(FederationEventKind::ReflexExported),
        Just(FederationEventKind::ReflexEvolved),
        Just(FederationEventKind::ReflexPruned),
        Just(FederationEventKind::DriftDetected),
        Just(FederationEventKind::TierPromotion),
        Just(FederationEventKind::ReflexImported),
    ]
}

fn make_record(id: u64, tier: MaturityTier, e_value: f64) -> ReflexRecord {
    ReflexRecord {
        reflex_id: id,
        cluster_id: "cluster".to_string(),
        version: 1,
        trigger_key: vec![0xAB, 0xCD],
        commands: vec!["restart".to_string()],
        status: VersionStatus::Active,
        tier,
        successes: 50,
        failures: 2,
        consecutive_failures: 0,
        drift_state: DriftSnapshot {
            e_value,
            null_rate: 0.9,
            total_observations: 100,
            post_cal_successes: 90,
            post_cal_observations: 100,
            drift_count: 0,
            calibrated: true,
            config: EValueConfig::default(),
        },
        evidence_summary: EvidenceSummary {
            entry_count: 3,
            is_complete: true,
            overall_verdict: EvidenceVerdict::Support,
            root_hash: "abc".to_string(),
            categories: vec![],
        },
        parent_reflex_id: None,
        parent_version: None,
        created_at_ms: 0,
        updated_at_ms: 1000,
    }
}

// =============================================================================
// Export invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn export_respects_tier_filter(
        min_tier in arb_maturity(),
    ) {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, MaturityTier::Incubating, 5.0));
        store.upsert(make_record(2, MaturityTier::Graduated, 5.0));
        store.upsert(make_record(3, MaturityTier::Veteran, 5.0));

        let config = FederationConfig {
            min_export_tier: min_tier,
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let exports = engine.export(&store, 5000);

        let min_rank = match min_tier {
            MaturityTier::Incubating => 0,
            MaturityTier::Graduated => 1,
            MaturityTier::Veteran => 2,
        };

        for export in &exports {
            let export_rank = match export.tier {
                MaturityTier::Incubating => 0,
                MaturityTier::Graduated => 1,
                MaturityTier::Veteran => 2,
            };
            prop_assert!(export_rank >= min_rank);
        }
    }

    #[test]
    fn export_respects_e_value_filter(
        min_e in 0.5..10.0f64,
    ) {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, MaturityTier::Graduated, min_e + 1.0));
        store.upsert(make_record(2, MaturityTier::Graduated, min_e - 0.1));

        let config = FederationConfig {
            min_export_e_value: min_e,
            min_export_tier: MaturityTier::Graduated,
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let exports = engine.export(&store, 5000);

        // Only the record above threshold should export.
        for export in &exports {
            prop_assert!(export.e_value >= min_e);
        }
    }

    #[test]
    fn export_count_matches_stats(
        n in 1..5u64,
    ) {
        let mut store = ReflexStore::new();
        for i in 0..n {
            store.upsert(make_record(i, MaturityTier::Graduated, 5.0));
        }
        let mut engine = FederationEngine::with_defaults();
        engine.export(&store, 5000);
        prop_assert_eq!(engine.stats().total_exports, n);
        prop_assert_eq!(engine.export_log().len(), n as usize);
    }

    #[test]
    fn export_swarm_id_matches_config(
        swarm in "[a-z]{3,10}",
    ) {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, MaturityTier::Graduated, 5.0));

        let config = FederationConfig {
            swarm_id: swarm.clone(),
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let exports = engine.export(&store, 5000);

        for export in &exports {
            prop_assert_eq!(&export.swarm_id, &swarm);
        }
    }
}

// =============================================================================
// Import invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn import_rejects_self_swarm(swarm in "[a-z]{3,8}") {
        let record = make_record(1, MaturityTier::Graduated, 5.0);
        let export = ReflexExport::from_record(&record, &swarm, 3000);
        let config = FederationConfig {
            swarm_id: swarm,
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let result = engine.import(&export, 5000);
        prop_assert_eq!(result, ImportResult::SelfImport);
    }

    #[test]
    fn import_accepts_other_swarm(
        src in "[a-z]{3,8}",
        dst in "[a-z]{3,8}",
    ) {
        prop_assume!(src != dst);
        let record = make_record(1, MaturityTier::Graduated, 5.0);
        let export = ReflexExport::from_record(&record, &src, 3000);
        let config = FederationConfig {
            swarm_id: dst,
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        let result = engine.import(&export, 5000);
        let is_imported = matches!(result, ImportResult::Imported { .. });
        prop_assert!(is_imported);
    }

    #[test]
    fn import_rejects_bad_schema(version in 2..100u32) {
        let record = make_record(1, MaturityTier::Graduated, 5.0);
        let mut export = ReflexExport::from_record(&record, "other", 3000);
        export.schema_version = version;
        let mut engine = FederationEngine::with_defaults();
        let result = engine.import(&export, 5000);
        let is_unsupported = matches!(result, ImportResult::UnsupportedSchema { .. });
        prop_assert!(is_unsupported);
    }
}

// =============================================================================
// Webhook invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn webhook_dispatch_matches_filter(
        event_kind in arb_event_kind(),
        filter_kind in arb_event_kind(),
    ) {
        let config = FederationConfig {
            webhooks: vec![WebhookConfig {
                url: "https://example.com".to_string(),
                kind: WebhookKind::Generic,
                event_filter: vec![filter_kind.clone()],
                enabled: true,
            }],
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        engine.emit_event(FederationEvent {
            kind: event_kind.clone(),
            swarm_id: "s".to_string(),
            reflex_id: 1,
            cluster_id: "c".to_string(),
            summary: "test".to_string(),
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        });

        if event_kind == filter_kind {
            prop_assert_eq!(engine.pending_deliveries(), 1);
        } else {
            prop_assert_eq!(engine.pending_deliveries(), 0);
        }
    }

    #[test]
    fn event_payload_is_valid_json(
        kind in arb_webhook_kind(),
        event_kind in arb_event_kind(),
        summary in "[a-z ]{3,20}",
    ) {
        let event = FederationEvent {
            kind: event_kind,
            swarm_id: "s".to_string(),
            reflex_id: 1,
            cluster_id: "c".to_string(),
            summary,
            timestamp_ms: 1000,
            metadata: HashMap::new(),
        };
        let payload = event.format_for(&kind);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&payload);
        prop_assert!(parsed.is_ok(), "invalid JSON: {}", payload);
    }

    #[test]
    fn drain_empties_queue(n in 1..5u32) {
        let config = FederationConfig {
            webhooks: vec![WebhookConfig {
                url: "https://example.com".to_string(),
                kind: WebhookKind::Generic,
                event_filter: vec![FederationEventKind::ReflexExported],
                enabled: true,
            }],
            ..Default::default()
        };
        let mut engine = FederationEngine::new(config);
        for _ in 0..n {
            engine.emit_event(FederationEvent {
                kind: FederationEventKind::ReflexExported,
                swarm_id: "s".to_string(),
                reflex_id: 1,
                cluster_id: "c".to_string(),
                summary: "test".to_string(),
                timestamp_ms: 1000,
                metadata: HashMap::new(),
            });
        }
        prop_assert_eq!(engine.pending_deliveries(), n as usize);
        let drained = engine.drain_deliveries();
        prop_assert_eq!(drained.len(), n as usize);
        prop_assert_eq!(engine.pending_deliveries(), 0);
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn export_serde_roundtrip(
        id in 0..1000u64,
        tier in arb_maturity(),
        e_value in 1.0..100.0f64,
    ) {
        let record = make_record(id, tier, e_value);
        let export = ReflexExport::from_record(&record, "swarm1", 5000);
        let json = serde_json::to_string(&export).unwrap();
        let decoded: ReflexExport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.reflex_id, id);
        prop_assert_eq!(decoded.swarm_id, "swarm1");
    }

    #[test]
    fn import_result_serde_roundtrip(
        id in 0..1000u64,
        swarm in "[a-z]{3,8}",
    ) {
        let r = ImportResult::Imported { reflex_id: id, source_swarm: swarm };
        let json = serde_json::to_string(&r).unwrap();
        let decoded: ImportResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, r);
    }

    #[test]
    fn federation_stats_serde_roundtrip(
        exports in 0..1000u64,
        imports in 0..500u64,
        webhooks in 0..2000u64,
        pending in 0..100u64,
    ) {
        let stats = FederationStats {
            total_exports: exports,
            total_imports: imports,
            total_webhooks: webhooks,
            pending_deliveries: pending,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: FederationStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }

    #[test]
    fn event_kind_serde_roundtrip(kind in arb_event_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let decoded: FederationEventKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, kind);
    }

    #[test]
    fn webhook_kind_serde_roundtrip(kind in arb_webhook_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let decoded: WebhookKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, kind);
    }

    #[test]
    fn delivery_status_serde_roundtrip(reason in "[a-z]{3,15}") {
        let statuses = [
            DeliveryStatus::Pending,
            DeliveryStatus::Delivered,
            DeliveryStatus::Failed { reason },
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let decoded: DeliveryStatus = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&decoded, s);
        }
    }
}
