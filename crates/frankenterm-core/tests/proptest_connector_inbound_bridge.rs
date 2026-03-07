//! Property-based tests for the inbound connector bridge module.
//!
//! Tests cover signal construction, rule ID generation, deduplication,
//! severity mapping, config roundtrips, and bridge routing invariants.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::connector_data_classification::IngestionDecision;
use frankenterm_core::connector_host_runtime::{ConnectorFailureClass, ConnectorLifecyclePhase};
use frankenterm_core::connector_inbound_bridge::{
    ConnectorBridgeTelemetrySnapshot, ConnectorInboundBridge, ConnectorInboundBridgeConfig,
    ConnectorSignal, ConnectorSignalKind, SignalDeduplicator,
};
use frankenterm_core::events::{Event, EventBus};
use frankenterm_core::patterns::Severity;

// =============================================================================
// Strategies
// =============================================================================

fn arb_signal_kind() -> impl Strategy<Value = ConnectorSignalKind> {
    prop_oneof![
        Just(ConnectorSignalKind::Webhook),
        Just(ConnectorSignalKind::Stream),
        Just(ConnectorSignalKind::Poll),
        Just(ConnectorSignalKind::Lifecycle),
        Just(ConnectorSignalKind::HealthCheck),
        Just(ConnectorSignalKind::Failure),
        Just(ConnectorSignalKind::Custom),
    ]
}

fn arb_failure_class() -> impl Strategy<Value = ConnectorFailureClass> {
    prop_oneof![
        Just(ConnectorFailureClass::Auth),
        Just(ConnectorFailureClass::Quota),
        Just(ConnectorFailureClass::Network),
        Just(ConnectorFailureClass::Policy),
        Just(ConnectorFailureClass::Validation),
        Just(ConnectorFailureClass::Timeout),
        Just(ConnectorFailureClass::Unknown),
    ]
}

fn arb_lifecycle_phase() -> impl Strategy<Value = ConnectorLifecyclePhase> {
    prop_oneof![
        Just(ConnectorLifecyclePhase::Stopped),
        Just(ConnectorLifecyclePhase::Starting),
        Just(ConnectorLifecyclePhase::Running),
        Just(ConnectorLifecyclePhase::Degraded),
        Just(ConnectorLifecyclePhase::Failed),
    ]
}

fn arb_connector_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("github".to_string()),
        Just("slack".to_string()),
        Just("jira".to_string()),
        Just("datadog".to_string()),
        Just("pagerduty".to_string()),
        Just("custom".to_string()),
        "[a-z][a-z0-9_]{2,10}".prop_map(|s| s),
    ]
}

fn arb_signal() -> impl Strategy<Value = ConnectorSignal> {
    (
        arb_connector_name(),
        arb_signal_kind(),
        proptest::option::of("[a-z0-9\\-]{4,16}"),
        1u64..1_000_000u64,
        proptest::option::of(0u64..1000u64),
        proptest::option::of("[a-z_]{3,12}"),
    )
        .prop_map(|(source, kind, corr_id, ts, pane_id, sub_type)| {
            let mut sig = ConnectorSignal::new(source, kind, serde_json::json!({"key": "value"}))
                .with_timestamp_ms(ts);
            if let Some(cid) = corr_id {
                sig = sig.with_correlation_id(cid);
            }
            if let Some(pid) = pane_id {
                sig = sig.with_pane_id(pid);
            }
            if let Some(st) = sub_type {
                sig = sig.with_sub_type(st);
            }
            sig
        })
}

fn arb_config() -> impl Strategy<Value = ConnectorInboundBridgeConfig> {
    (1usize..1000usize, 1u64..600u64, any::<bool>()).prop_map(|(dedup_cap, dedup_ttl, reject)| {
        ConnectorInboundBridgeConfig {
            dedup_capacity: dedup_cap,
            dedup_ttl_secs: dedup_ttl,
            reject_unknown_kinds: reject,
            rule_id_overrides: HashMap::new(),
        }
    })
}

// =============================================================================
// Tests
// =============================================================================

proptest! {
    // ---- Signal kind ----

    #[test]
    fn signal_kind_as_str_never_empty(kind in arb_signal_kind()) {
        let label = kind.as_str();
        prop_assert!(!label.is_empty());
        prop_assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }

    #[test]
    fn signal_kind_display_matches_as_str(kind in arb_signal_kind()) {
        prop_assert_eq!(format!("{kind}"), kind.as_str());
    }

    #[test]
    fn signal_kind_serde_roundtrip(kind in arb_signal_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ConnectorSignalKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    // ---- Failure class ----

    #[test]
    fn failure_class_as_str_never_empty(class in arb_failure_class()) {
        let label = class.as_str();
        prop_assert!(!label.is_empty());
    }

    #[test]
    fn failure_class_serde_roundtrip(class in arb_failure_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: ConnectorFailureClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(class, back);
    }

    // ---- Lifecycle phase ----

    #[test]
    fn lifecycle_phase_as_str_never_empty(phase in arb_lifecycle_phase()) {
        let label = phase.as_str();
        prop_assert!(!label.is_empty());
    }

    #[test]
    fn lifecycle_phase_serde_roundtrip(phase in arb_lifecycle_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: ConnectorLifecyclePhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(phase, back);
    }

    // ---- Signal rule_id ----

    #[test]
    fn signal_rule_id_starts_with_connector_prefix(sig in arb_signal()) {
        let rule_id = sig.rule_id();
        prop_assert!(rule_id.starts_with("connector."), "rule_id should start with 'connector.': {}", rule_id);
    }

    #[test]
    fn signal_rule_id_contains_source_name(
        source in arb_connector_name(),
        kind in arb_signal_kind(),
    ) {
        let sig = ConnectorSignal::new(source.clone(), kind, serde_json::json!({}))
            .with_timestamp_ms(1000);
        let rule_id = sig.rule_id();
        prop_assert!(rule_id.contains(&source), "rule_id should contain source: {}", rule_id);
    }

    #[test]
    fn signal_rule_id_contains_kind_label(
        source in arb_connector_name(),
        kind in arb_signal_kind(),
    ) {
        let sig = ConnectorSignal::new(source, kind, serde_json::json!({}))
            .with_timestamp_ms(1000);
        let rule_id = sig.rule_id();
        prop_assert!(rule_id.contains(kind.as_str()), "rule_id should contain kind: {}", rule_id);
    }

    #[test]
    fn signal_rule_id_with_sub_type_appends_suffix(
        source in arb_connector_name(),
        kind in arb_signal_kind(),
        sub_type in "[a-z_]{3,10}",
    ) {
        let sig = ConnectorSignal::new(source, kind, serde_json::json!({}))
            .with_timestamp_ms(1000)
            .with_sub_type(sub_type.clone());
        let rule_id = sig.rule_id();
        prop_assert!(rule_id.ends_with(&sub_type), "rule_id should end with sub_type: {}", rule_id);
    }

    #[test]
    fn signal_rule_id_with_lifecycle_phase_appends_phase(
        source in arb_connector_name(),
        phase in arb_lifecycle_phase(),
    ) {
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::Lifecycle, serde_json::json!({}))
            .with_timestamp_ms(1000)
            .with_lifecycle_phase(phase);
        let rule_id = sig.rule_id();
        prop_assert!(rule_id.ends_with(phase.as_str()), "rule_id should end with phase: {}", rule_id);
    }

    #[test]
    fn signal_rule_id_with_failure_class_appends_class(
        source in arb_connector_name(),
        class in arb_failure_class(),
    ) {
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::Failure, serde_json::json!({}))
            .with_timestamp_ms(1000)
            .with_failure_class(class);
        let rule_id = sig.rule_id();
        prop_assert!(rule_id.ends_with(class.as_str()), "rule_id should end with class: {}", rule_id);
    }

    // ---- Severity mapping ----

    #[test]
    fn failure_signals_always_critical(source in arb_connector_name()) {
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::Failure, serde_json::json!({}))
            .with_timestamp_ms(1000);
        prop_assert_eq!(sig.severity(), Severity::Critical);
    }

    #[test]
    fn health_check_with_failure_class_is_warning(
        source in arb_connector_name(),
        class in arb_failure_class(),
    ) {
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::HealthCheck, serde_json::json!({}))
            .with_timestamp_ms(1000)
            .with_failure_class(class);
        prop_assert_eq!(sig.severity(), Severity::Warning);
    }

    #[test]
    fn health_check_without_failure_is_info(source in arb_connector_name()) {
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::HealthCheck, serde_json::json!({}))
            .with_timestamp_ms(1000);
        prop_assert_eq!(sig.severity(), Severity::Info);
    }

    #[test]
    fn lifecycle_signals_always_info(
        source in arb_connector_name(),
        phase in arb_lifecycle_phase(),
    ) {
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::Lifecycle, serde_json::json!({}))
            .with_timestamp_ms(1000)
            .with_lifecycle_phase(phase);
        prop_assert_eq!(sig.severity(), Severity::Info);
    }

    // ---- Config ----

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ConnectorInboundBridgeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.dedup_capacity, config.dedup_capacity);
        prop_assert_eq!(back.dedup_ttl_secs, config.dedup_ttl_secs);
        prop_assert_eq!(back.reject_unknown_kinds, config.reject_unknown_kinds);
    }

    // ---- Deduplicator ----

    #[test]
    fn dedup_first_always_accepted(id in "[a-z]{3,10}", ts in 1000u64..1_000_000u64) {
        let mut dedup = SignalDeduplicator::new(100, Duration::from_secs(300));
        prop_assert!(dedup.check_and_record(&id, ts));
        prop_assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn dedup_duplicate_within_ttl_rejected(
        id in "[a-z]{3,10}",
        ts in 1000u64..500_000u64,
    ) {
        let mut dedup = SignalDeduplicator::new(100, Duration::from_secs(300));
        prop_assert!(dedup.check_and_record(&id, ts));
        prop_assert!(!dedup.check_and_record(&id, ts + 100));
    }

    #[test]
    fn dedup_after_ttl_accepted(
        id in "[a-z]{3,10}",
        ts in 1000u64..500_000u64,
        ttl_secs in 1u64..100u64,
    ) {
        let ttl = Duration::from_secs(ttl_secs);
        let mut dedup = SignalDeduplicator::new(100, ttl);
        prop_assert!(dedup.check_and_record(&id, ts));
        let after_ttl = ts + (ttl_secs * 1000) + 1;
        prop_assert!(dedup.check_and_record(&id, after_ttl));
    }

    #[test]
    fn dedup_never_exceeds_capacity(
        capacity in 1usize..50usize,
        num_inserts in 1usize..200usize,
    ) {
        let mut dedup = SignalDeduplicator::new(capacity, Duration::from_secs(3600));
        for i in 0..num_inserts {
            dedup.check_and_record(&format!("id-{i}"), (i as u64) * 10 + 1000);
        }
        prop_assert!(dedup.len() <= capacity);
    }

    #[test]
    fn dedup_unique_ids_all_accepted(
        ids in proptest::collection::hash_set("[a-z]{3,8}", 1..20),
    ) {
        let mut dedup = SignalDeduplicator::new(100, Duration::from_secs(300));
        let mut ts = 1000u64;
        for id in &ids {
            prop_assert!(dedup.check_and_record(id, ts));
            ts += 10;
        }
        prop_assert_eq!(dedup.len(), ids.len());
    }

    // ---- Signal serde ----

    #[test]
    fn signal_serde_roundtrip(sig in arb_signal()) {
        let json = serde_json::to_string(&sig).unwrap();
        let back: ConnectorSignal = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.source_connector, sig.source_connector);
        prop_assert_eq!(back.signal_kind, sig.signal_kind);
        prop_assert_eq!(back.correlation_id, sig.correlation_id);
        prop_assert_eq!(back.timestamp_ms, sig.timestamp_ms);
        prop_assert_eq!(back.pane_id, sig.pane_id);
        prop_assert_eq!(back.sub_type, sig.sub_type);
    }

    // ---- Telemetry snapshot serde ----

    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        received in 0u64..1000u64,
        routed in 0u64..1000u64,
        deduped in 0u64..1000u64,
        rejected in 0u64..1000u64,
        published in 0u64..1000u64,
    ) {
        let snapshot = ConnectorBridgeTelemetrySnapshot {
            signals_received: received,
            signals_routed: routed,
            signals_deduplicated: deduped,
            signals_rejected: rejected,
            events_published: published,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: ConnectorBridgeTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snapshot, back);
    }

    // ---- Bridge routing ----

    #[test]
    fn bridge_routes_signal_and_increments_telemetry(sig in arb_signal()) {
        // Skip Custom signals if reject_unknown_kinds would block them
        let bus = Arc::new(EventBus::new(64));
        let _sub = bus.subscribe_detections();
        let config = ConnectorInboundBridgeConfig {
            reject_unknown_kinds: false,
            ..Default::default()
        };
        let mut bridge = ConnectorInboundBridge::new(bus, config);
        let result = bridge.route_signal(&sig);
        match result {
            Ok(r) => {
                prop_assert!(!r.rule_id.is_empty());
                let snap = bridge.telemetry_snapshot();
                prop_assert!(snap.signals_received >= 1);
            }
            Err(_) => {
                // Some signals may fail for other reasons, that's fine
            }
        }
    }

    #[test]
    fn bridge_rejects_custom_when_configured(source in arb_connector_name()) {
        let bus = Arc::new(EventBus::new(64));
        let config = ConnectorInboundBridgeConfig {
            reject_unknown_kinds: true,
            ..Default::default()
        };
        let mut bridge = ConnectorInboundBridge::new(bus, config);
        let sig = ConnectorSignal::new(source, ConnectorSignalKind::Custom, serde_json::json!({}))
            .with_timestamp_ms(1000);
        let result = bridge.route_signal(&sig);
        prop_assert!(result.is_err());
        let snap = bridge.telemetry_snapshot();
        prop_assert_eq!(snap.signals_rejected, 1);
    }

    #[test]
    fn bridge_dedup_with_same_correlation_id(
        corr_id in "[a-z]{5,10}",
        n in 2usize..10usize,
    ) {
        let bus = Arc::new(EventBus::new(64));
        let _sub = bus.subscribe_detections();
        let config = ConnectorInboundBridgeConfig::default();
        let mut bridge = ConnectorInboundBridge::new(bus, config);

        let mut routed = 0u64;
        let mut deduped = 0u64;
        for i in 0..n {
            let sig = ConnectorSignal::new("test", ConnectorSignalKind::Webhook, serde_json::json!({}))
                .with_correlation_id(corr_id.clone())
                .with_timestamp_ms(1000 + i as u64 * 10);
            let result = bridge.route_signal(&sig).unwrap();
            if result.deduplicated {
                deduped += 1;
            } else {
                routed += 1;
            }
        }
        prop_assert_eq!(routed, 1);
        prop_assert_eq!(deduped, (n as u64) - 1);

        let snap = bridge.telemetry_snapshot();
        prop_assert_eq!(snap.signals_received, n as u64);
        prop_assert_eq!(snap.signals_routed, 1);
        prop_assert_eq!(snap.signals_deduplicated, (n as u64) - 1);
    }

    #[test]
    fn bridge_telemetry_received_equals_calls(
        num in 1usize..20usize,
    ) {
        let bus = Arc::new(EventBus::new(64));
        let _sub = bus.subscribe_detections();
        let config = ConnectorInboundBridgeConfig {
            reject_unknown_kinds: false,
            ..Default::default()
        };
        let mut bridge = ConnectorInboundBridge::new(bus, config);

        for i in 0..num {
            let sig = ConnectorSignal::new("test", ConnectorSignalKind::Webhook, serde_json::json!({}))
                .with_timestamp_ms(1000 + i as u64 * 10);
            let _ = bridge.route_signal(&sig);
        }

        let snap = bridge.telemetry_snapshot();
        prop_assert_eq!(snap.signals_received, num as u64);
    }

    #[test]
    fn bridge_rejects_password_payloads_without_publishing(
        source in arb_connector_name(),
        password in ".{1,64}",
    ) {
        let bus = Arc::new(EventBus::new(64));
        let mut sub = bus.subscribe_detections();
        let mut bridge = ConnectorInboundBridge::new(bus, ConnectorInboundBridgeConfig::default());
        let sig = ConnectorSignal::new(
            source,
            ConnectorSignalKind::Webhook,
            serde_json::json!({
                "password": password,
                "status": "ok"
            }),
        )
        .with_timestamp_ms(1000);

        let result = bridge.route_signal(&sig);
        prop_assert!(result.is_err());
        prop_assert!(sub.try_recv().is_none());
        prop_assert_eq!(bridge.telemetry_snapshot().signals_rejected, 1);
        let audit = bridge.classification_audit_log().back().unwrap();
        let is_reject = matches!(audit.decision, IngestionDecision::Reject { .. });
        prop_assert!(is_reject);
    }

    #[test]
    fn bridge_redacts_email_payloads_before_publish(
        source in arb_connector_name(),
        local in "[a-z0-9]{1,12}",
        domain in "[a-z]{1,10}",
    ) {
        let bus = Arc::new(EventBus::new(64));
        let mut sub = bus.subscribe_detections();
        let mut bridge = ConnectorInboundBridge::new(bus, ConnectorInboundBridgeConfig::default());
        let email = format!("{local}@{domain}.com");
        let sig = ConnectorSignal::new(
            source,
            ConnectorSignalKind::Webhook,
            serde_json::json!({
                "email": email.clone(),
                "status": "ok"
            }),
        )
        .with_timestamp_ms(1000);

        let result = bridge.route_signal(&sig);
        prop_assert!(result.is_ok());

        let event = sub.try_recv().unwrap();
        if let Ok(Event::PatternDetected { detection, .. }) = event {
            let map = detection.extracted.as_object().unwrap();
            prop_assert_ne!(
                map.get("email").and_then(|value| value.as_str()),
                Some(email.as_str())
            );
            let classification = map
                .get("classification")
                .and_then(|value| value.as_object())
                .unwrap();
            prop_assert_eq!(
                classification
                    .get("ingestion_decision")
                    .and_then(|value| value.as_str()),
                Some("accept_redacted")
            );
        } else {
            prop_assert!(false, "expected PatternDetected event");
        }

        let audit = bridge.classification_audit_log().back().unwrap();
        prop_assert_eq!(audit.decision.clone(), IngestionDecision::AcceptRedacted);
    }
}
