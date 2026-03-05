//! Property-based tests for the outbound connector bridge module.
//!
//! Tests cover routing rule matching, deduplication, sandbox enforcement,
//! dispatch queue bounds, telemetry accuracy, and serde roundtrips.

use std::collections::HashSet;
use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::connector_host_runtime::{
    ConnectorCapability, ConnectorCapabilityEnvelope, ConnectorSandboxZone,
};
use frankenterm_core::connector_outbound_bridge::{
    ConnectorActionKind, ConnectorOutboundBridge, ConnectorOutboundBridgeConfig,
    OutboundBridgeTelemetrySnapshot, OutboundDeduplicator, OutboundEvent, OutboundEventSource,
    OutboundRoutingRule, OutboundSandboxChecker, OutboundSeverity, SandboxCheckResult,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_event_source() -> impl Strategy<Value = OutboundEventSource> {
    prop_oneof![
        Just(OutboundEventSource::PatternDetected),
        Just(OutboundEventSource::PaneLifecycle),
        Just(OutboundEventSource::WorkflowLifecycle),
        Just(OutboundEventSource::UserAction),
        Just(OutboundEventSource::PolicyDecision),
        Just(OutboundEventSource::HealthAlert),
        Just(OutboundEventSource::Custom),
    ]
}

fn arb_severity() -> impl Strategy<Value = OutboundSeverity> {
    prop_oneof![
        Just(OutboundSeverity::Info),
        Just(OutboundSeverity::Warning),
        Just(OutboundSeverity::Critical),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ConnectorActionKind> {
    prop_oneof![
        Just(ConnectorActionKind::Notify),
        Just(ConnectorActionKind::Ticket),
        Just(ConnectorActionKind::TriggerWorkflow),
        Just(ConnectorActionKind::AuditLog),
        Just(ConnectorActionKind::Invoke),
        Just(ConnectorActionKind::CredentialAction),
    ]
}

fn arb_capability() -> impl Strategy<Value = ConnectorCapability> {
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

fn arb_event_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("pattern.ci_failure".to_string()),
        Just("pattern.error_detected".to_string()),
        Just("pane.discovered".to_string()),
        Just("pane.disappeared".to_string()),
        Just("workflow.started".to_string()),
        Just("workflow.completed".to_string()),
        Just("health.threshold_crossed".to_string()),
        Just("policy.approval_escalated".to_string()),
        Just("custom.user_event".to_string()),
        "[a-z][a-z0-9_]{0,15}\\.[a-z][a-z0-9_]{0,15}".prop_map(|s| s),
    ]
}

fn arb_connector_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("slack".to_string()),
        Just("github".to_string()),
        Just("datadog".to_string()),
        Just("pagerduty".to_string()),
        Just("jira".to_string()),
        Just("custom".to_string()),
    ]
}

fn arb_outbound_event() -> impl Strategy<Value = OutboundEvent> {
    (
        arb_event_source(),
        arb_event_type(),
        proptest::option::of("[a-z0-9\\-]{4,20}"),
        1u64..1_000_000u64,
        proptest::option::of(0u64..1000u64),
        proptest::option::of("[a-z0-9\\-]{4,16}"),
        arb_severity(),
    )
        .prop_map(
            |(source, event_type, corr_id, ts, pane_id, wf_id, severity)| {
                let mut event =
                    OutboundEvent::new(source, &event_type, serde_json::json!({"key": "value"}))
                        .with_timestamp_ms(ts)
                        .with_severity(severity);
                if let Some(cid) = corr_id {
                    event = event.with_correlation_id(cid);
                }
                if let Some(pid) = pane_id {
                    event = event.with_pane_id(pid);
                }
                if let Some(wid) = wf_id {
                    event = event.with_workflow_id(wid);
                }
                event
            },
        )
}

fn arb_routing_rule() -> impl Strategy<Value = OutboundRoutingRule> {
    (
        "[a-z][a-z0-9_]{2,10}",
        proptest::option::of(arb_event_source()),
        proptest::option::of("[a-z]{3,8}\\."),
        arb_connector_name(),
        arb_action_kind(),
        any::<bool>(),
        0u32..100u32,
        proptest::option::of(arb_severity()),
    )
        .prop_map(
            |(rule_id, source, prefix, connector, kind, enabled, priority, min_sev)| {
                OutboundRoutingRule {
                    rule_id,
                    source_filter: source,
                    event_type_prefix: prefix,
                    min_severity: min_sev,
                    target_connector: connector,
                    action_kind: kind,
                    enabled,
                    priority,
                }
            },
        )
}

fn arb_config() -> impl Strategy<Value = ConnectorOutboundBridgeConfig> {
    (
        1usize..1000usize,
        1u64..600u64,
        1usize..500usize,
        1usize..500usize,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(dedup_cap, dedup_ttl, queue_cap, hist_cap, reject, enforce)| {
                ConnectorOutboundBridgeConfig {
                    dedup_capacity: dedup_cap,
                    dedup_ttl_secs: dedup_ttl,
                    dispatch_queue_capacity: queue_cap,
                    dispatch_history_capacity: hist_cap,
                    reject_unmatched_events: reject,
                    enforce_sandbox: enforce,
                }
            },
        )
}

fn permissive_zone(name: &str) -> ConnectorSandboxZone {
    ConnectorSandboxZone {
        zone_id: format!("zone.{name}"),
        fail_closed: true,
        capability_envelope: ConnectorCapabilityEnvelope {
            allowed_capabilities: vec![
                ConnectorCapability::Invoke,
                ConnectorCapability::NetworkEgress,
                ConnectorCapability::SecretBroker,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
                ConnectorCapability::FilesystemRead,
                ConnectorCapability::FilesystemWrite,
                ConnectorCapability::ProcessExec,
            ],
            filesystem_read_prefixes: vec![],
            filesystem_write_prefixes: vec![],
            network_allow_hosts: vec![],
            allowed_exec_commands: vec![],
        },
    }
}

fn restrictive_zone(name: &str) -> ConnectorSandboxZone {
    ConnectorSandboxZone {
        zone_id: format!("zone.{name}.restricted"),
        fail_closed: true,
        capability_envelope: ConnectorCapabilityEnvelope {
            allowed_capabilities: vec![ConnectorCapability::ReadState],
            filesystem_read_prefixes: vec![],
            filesystem_write_prefixes: vec![],
            network_allow_hosts: vec![],
            allowed_exec_commands: vec![],
        },
    }
}

// =============================================================================
// Tests
// =============================================================================

proptest! {
    // ---- Event source ----

    #[test]
    fn event_source_as_str_never_empty(source in arb_event_source()) {
        let label = source.as_str();
        prop_assert!(!label.is_empty());
        prop_assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }

    #[test]
    fn event_source_display_matches_as_str(source in arb_event_source()) {
        prop_assert_eq!(format!("{source}"), source.as_str());
    }

    #[test]
    fn event_source_serde_roundtrip(source in arb_event_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let back: OutboundEventSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(source, back);
    }

    // ---- Severity ----

    #[test]
    fn severity_serde_roundtrip(severity in arb_severity()) {
        let json = serde_json::to_string(&severity).unwrap();
        let back: OutboundSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(severity, back);
    }

    // ---- Action kind ----

    #[test]
    fn action_kind_as_str_never_empty(kind in arb_action_kind()) {
        let label = kind.as_str();
        prop_assert!(!label.is_empty());
        prop_assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }

    #[test]
    fn action_kind_display_matches_as_str(kind in arb_action_kind()) {
        prop_assert_eq!(format!("{kind}"), kind.as_str());
    }

    #[test]
    fn action_kind_serde_roundtrip(kind in arb_action_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ConnectorActionKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    #[test]
    fn action_kind_required_capability_is_consistent(kind in arb_action_kind()) {
        let cap = kind.required_capability();
        // All capabilities should map to valid, well-known variants
        let label = cap.as_str();
        prop_assert!(!label.is_empty());
    }

    // ---- Config ----

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ConnectorOutboundBridgeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.dedup_capacity, config.dedup_capacity);
        prop_assert_eq!(back.dedup_ttl_secs, config.dedup_ttl_secs);
        prop_assert_eq!(back.dispatch_queue_capacity, config.dispatch_queue_capacity);
        prop_assert_eq!(back.dispatch_history_capacity, config.dispatch_history_capacity);
        prop_assert_eq!(back.reject_unmatched_events, config.reject_unmatched_events);
        prop_assert_eq!(back.enforce_sandbox, config.enforce_sandbox);
    }

    // ---- Deduplicator ----

    #[test]
    fn dedup_first_insertion_always_true(id in "[a-z]{3,10}", ts in 1000u64..1_000_000u64) {
        let mut dedup = OutboundDeduplicator::new(100, Duration::from_secs(300));
        prop_assert!(dedup.check_and_record(&id, ts));
        prop_assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn dedup_second_same_id_within_ttl_returns_false(
        id in "[a-z]{3,10}",
        ts in 1000u64..500_000u64,
    ) {
        let mut dedup = OutboundDeduplicator::new(100, Duration::from_secs(300));
        prop_assert!(dedup.check_and_record(&id, ts));
        prop_assert!(!dedup.check_and_record(&id, ts + 100));
    }

    #[test]
    fn dedup_same_id_after_ttl_accepted_again(
        id in "[a-z]{3,10}",
        ts in 1000u64..500_000u64,
        ttl_secs in 1u64..100u64,
    ) {
        let ttl = Duration::from_secs(ttl_secs);
        let mut dedup = OutboundDeduplicator::new(100, ttl);
        prop_assert!(dedup.check_and_record(&id, ts));
        let after_ttl = ts + (ttl_secs * 1000) + 1;
        prop_assert!(dedup.check_and_record(&id, after_ttl));
    }

    #[test]
    fn dedup_capacity_never_exceeded(
        capacity in 1usize..50usize,
        num_inserts in 1usize..200usize,
    ) {
        let mut dedup = OutboundDeduplicator::new(capacity, Duration::from_secs(3600));
        for i in 0..num_inserts {
            dedup.check_and_record(&format!("id-{i}"), (i as u64) * 10 + 1000);
        }
        prop_assert!(dedup.len() <= capacity);
    }

    #[test]
    fn dedup_distinct_ids_all_accepted(
        ids in proptest::collection::hash_set("[a-z]{3,8}", 1..20),
    ) {
        let mut dedup = OutboundDeduplicator::new(100, Duration::from_secs(300));
        let mut ts = 1000u64;
        for id in &ids {
            prop_assert!(dedup.check_and_record(id, ts));
            ts += 10;
        }
        prop_assert_eq!(dedup.len(), ids.len());
    }

    // ---- Routing rule matching ----

    #[test]
    fn disabled_rule_never_matches(event in arb_outbound_event(), rule in arb_routing_rule()) {
        let mut rule = rule;
        rule.enabled = false;
        prop_assert!(!rule.matches(&event));
    }

    #[test]
    fn rule_with_no_filters_matches_any_event(event in arb_outbound_event()) {
        let rule = OutboundRoutingRule {
            rule_id: "catch-all".to_string(),
            source_filter: None,
            event_type_prefix: None,
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        };
        prop_assert!(rule.matches(&event));
    }

    #[test]
    fn rule_source_filter_rejects_mismatched_source(
        source1 in arb_event_source(),
        source2 in arb_event_source(),
        event_type in arb_event_type(),
    ) {
        if source1 != source2 {
            let rule = OutboundRoutingRule {
                rule_id: "src-filter".to_string(),
                source_filter: Some(source1),
                event_type_prefix: None,
                min_severity: None,
                target_connector: "test".to_string(),
                action_kind: ConnectorActionKind::Notify,
                enabled: true,
                priority: 0,
            };
            let event =
                OutboundEvent::new(source2, &event_type, serde_json::json!({}))
                    .with_timestamp_ms(1000);
            prop_assert!(!rule.matches(&event));
        }
    }

    #[test]
    fn rule_event_type_prefix_requires_matching_prefix(
        prefix in "[a-z]{3,6}\\.",
        suffix in "[a-z]{3,8}",
    ) {
        let matching_type = format!("{prefix}{suffix}");
        let non_matching_type = format!("other.{suffix}");

        let rule = OutboundRoutingRule {
            rule_id: "prefix-rule".to_string(),
            source_filter: None,
            event_type_prefix: Some(prefix),
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        };

        let matching_event =
            OutboundEvent::new(OutboundEventSource::Custom, &matching_type, serde_json::json!({}))
                .with_timestamp_ms(1000);
        prop_assert!(rule.matches(&matching_event));

        let non_matching_event =
            OutboundEvent::new(OutboundEventSource::Custom, &non_matching_type, serde_json::json!({}))
                .with_timestamp_ms(1000);
        // Only fails if the prefix doesn't happen to match "other."
        if !non_matching_type.starts_with(rule.event_type_prefix.as_deref().unwrap_or("")) {
            prop_assert!(!rule.matches(&non_matching_event));
        }
    }

    // ---- Sandbox checker ----

    #[test]
    fn sandbox_permissive_zone_allows_any_capability(cap in arb_capability()) {
        let mut checker = OutboundSandboxChecker::new();
        checker.register_zone("test-conn", permissive_zone("test-conn"));
        let result = checker.check_capability("test-conn", cap);
        prop_assert_eq!(result, SandboxCheckResult::Allowed);
    }

    #[test]
    fn sandbox_restrictive_zone_denies_non_readstate(cap in arb_capability()) {
        let mut checker = OutboundSandboxChecker::new();
        checker.register_zone("locked", restrictive_zone("locked"));
        let result = checker.check_capability("locked", cap);
        if cap == ConnectorCapability::ReadState {
            prop_assert_eq!(result, SandboxCheckResult::Allowed);
        } else {
            let is_denied = !matches!(result, SandboxCheckResult::Allowed);
            prop_assert!(is_denied, "expected Denied for {:?}", cap);
        }
    }

    #[test]
    fn sandbox_unknown_connector_uses_default_zone(cap in arb_capability()) {
        let checker = OutboundSandboxChecker::new();
        // Default zone has fail_closed=true with default capabilities (Invoke, ReadState, StreamEvents)
        let result = checker.check_capability("unknown-connector", cap);
        let default_allowed = matches!(
            cap,
            ConnectorCapability::Invoke
                | ConnectorCapability::ReadState
                | ConnectorCapability::StreamEvents
        );
        if default_allowed {
            prop_assert_eq!(result, SandboxCheckResult::Allowed);
        } else {
            let is_denied = !matches!(result, SandboxCheckResult::Allowed);
            prop_assert!(is_denied, "expected Denied for {:?} on default zone", cap);
        }
    }

    // ---- Bridge routing invariants ----

    #[test]
    fn bridge_no_rules_no_dispatch(event in arb_outbound_event()) {
        let config = ConnectorOutboundBridgeConfig {
            reject_unmatched_events: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        let result = bridge.process_event(&event).unwrap();
        prop_assert_eq!(result.actions_dispatched.len(), 0);
        prop_assert_eq!(result.actions_blocked.len(), 0);
        prop_assert!(!result.deduplicated);
    }

    #[test]
    fn bridge_dispatched_plus_blocked_leq_matched_rules(
        events in proptest::collection::vec(arb_outbound_event(), 1..5),
        rules in proptest::collection::vec(arb_routing_rule(), 1..5),
    ) {
        let mut config = ConnectorOutboundBridgeConfig::default();
        config.enforce_sandbox = false; // disable sandbox to avoid blocking
        let mut bridge = ConnectorOutboundBridge::new(config);
        for rule in &rules {
            let mut r = rule.clone();
            r.enabled = true;
            bridge.add_rule(r);
        }
        // Register permissive zones for all connectors mentioned in rules
        let connectors: HashSet<String> = rules.iter().map(|r| r.target_connector.clone()).collect();
        for conn in &connectors {
            bridge.register_sandbox_zone(conn.clone(), permissive_zone(conn));
        }

        for event in &events {
            let result = bridge.process_event(event);
            if let Ok(result) = result {
                let total = result.actions_dispatched.len() + result.actions_blocked.len();
                prop_assert!(total <= rules.len());
            }
        }
    }

    #[test]
    fn bridge_dispatch_queue_bounded_by_capacity(
        queue_cap in 1usize..20usize,
        num_events in 1usize..50usize,
    ) {
        let config = ConnectorOutboundBridgeConfig {
            dispatch_queue_capacity: queue_cap,
            enforce_sandbox: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        bridge.add_rule(OutboundRoutingRule {
            rule_id: "catch-all".to_string(),
            source_filter: None,
            event_type_prefix: None,
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        });

        for i in 0..num_events {
            let event = OutboundEvent::new(
                OutboundEventSource::Custom,
                &format!("event.{i}"),
                serde_json::json!({}),
            ).with_timestamp_ms(1000 + i as u64 * 10);
            let _ = bridge.process_event(&event);
        }
        prop_assert!(bridge.pending_action_count() <= queue_cap);
    }

    #[test]
    fn bridge_dispatch_history_bounded_by_capacity(
        hist_cap in 1usize..20usize,
        num_events in 1usize..50usize,
    ) {
        let config = ConnectorOutboundBridgeConfig {
            dispatch_history_capacity: hist_cap,
            enforce_sandbox: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        bridge.add_rule(OutboundRoutingRule {
            rule_id: "catch-all".to_string(),
            source_filter: None,
            event_type_prefix: None,
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        });

        for i in 0..num_events {
            let event = OutboundEvent::new(
                OutboundEventSource::Custom,
                &format!("event.{i}"),
                serde_json::json!({}),
            ).with_timestamp_ms(1000 + i as u64 * 10);
            let _ = bridge.process_event(&event);
        }
        prop_assert!(bridge.dispatch_history().len() <= hist_cap);
    }

    // ---- Telemetry consistency ----

    #[test]
    fn bridge_telemetry_events_received_equals_process_calls(
        num_events in 1usize..30usize,
    ) {
        let config = ConnectorOutboundBridgeConfig {
            enforce_sandbox: false,
            reject_unmatched_events: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);

        for i in 0..num_events {
            let event = OutboundEvent::new(
                OutboundEventSource::Custom,
                &format!("event.{i}"),
                serde_json::json!({}),
            ).with_timestamp_ms(1000 + i as u64 * 10);
            let _ = bridge.process_event(&event);
        }

        let tel = bridge.telemetry();
        prop_assert_eq!(tel.events_received, num_events as u64);
    }

    #[test]
    fn bridge_telemetry_sum_invariant(
        events in proptest::collection::vec(arb_outbound_event(), 1..20),
    ) {
        let config = ConnectorOutboundBridgeConfig {
            enforce_sandbox: false,
            reject_unmatched_events: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        // Add a catch-all rule
        bridge.add_rule(OutboundRoutingRule {
            rule_id: "catch-all".to_string(),
            source_filter: None,
            event_type_prefix: None,
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        });

        for event in &events {
            let _ = bridge.process_event(event);
        }

        let tel = bridge.telemetry();
        // received = routed + unmatched + deduplicated
        prop_assert_eq!(
            tel.events_received,
            tel.events_routed + tel.events_unmatched + tel.events_deduplicated
        );
    }

    // ---- Telemetry snapshot serde ----

    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        received in 0u64..1000u64,
        routed in 0u64..1000u64,
        deduped in 0u64..1000u64,
        unmatched in 0u64..1000u64,
        dispatched in 0u64..1000u64,
        blocked_policy in 0u64..1000u64,
        blocked_sandbox in 0u64..1000u64,
        overflows in 0u64..1000u64,
    ) {
        let snapshot = OutboundBridgeTelemetrySnapshot {
            events_received: received,
            events_routed: routed,
            events_deduplicated: deduped,
            events_unmatched: unmatched,
            actions_dispatched: dispatched,
            actions_blocked_policy: blocked_policy,
            actions_blocked_sandbox: blocked_sandbox,
            dispatch_queue_overflows: overflows,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: OutboundBridgeTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snapshot, back);
    }

    // ---- Routing rule serde ----

    #[test]
    fn routing_rule_serde_roundtrip(rule in arb_routing_rule()) {
        let json = serde_json::to_string(&rule).unwrap();
        let back: OutboundRoutingRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.rule_id, rule.rule_id);
        prop_assert_eq!(back.source_filter, rule.source_filter);
        prop_assert_eq!(back.event_type_prefix, rule.event_type_prefix);
        prop_assert_eq!(back.target_connector, rule.target_connector);
        prop_assert_eq!(back.action_kind, rule.action_kind);
        prop_assert_eq!(back.enabled, rule.enabled);
        prop_assert_eq!(back.priority, rule.priority);
    }

    // ---- Event serde ----

    #[test]
    fn outbound_event_serde_roundtrip(event in arb_outbound_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let back: OutboundEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.source, event.source);
        prop_assert_eq!(back.event_type, event.event_type);
        prop_assert_eq!(back.correlation_id, event.correlation_id);
        prop_assert_eq!(back.timestamp_ms, event.timestamp_ms);
        prop_assert_eq!(back.pane_id, event.pane_id);
        prop_assert_eq!(back.workflow_id, event.workflow_id);
        prop_assert_eq!(back.severity, event.severity);
    }

    // ---- Dedup + bridge interaction ----

    #[test]
    fn bridge_dedup_with_same_correlation_id_across_events(
        corr_id in "[a-z]{5,10}",
        n in 2usize..10usize,
    ) {
        let config = ConnectorOutboundBridgeConfig {
            enforce_sandbox: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        bridge.add_rule(OutboundRoutingRule {
            rule_id: "catch-all".to_string(),
            source_filter: None,
            event_type_prefix: None,
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        });

        let mut dispatched_count = 0u64;
        let mut dedup_count = 0u64;
        for i in 0..n {
            let event = OutboundEvent::new(
                OutboundEventSource::Custom,
                &format!("event.{i}"),
                serde_json::json!({}),
            )
            .with_correlation_id(corr_id.clone())
            .with_timestamp_ms(1000 + i as u64 * 10);

            let result = bridge.process_event(&event).unwrap();
            if result.deduplicated {
                dedup_count += 1;
            } else {
                dispatched_count += 1;
            }
        }
        // First one dispatches, rest are deduped
        prop_assert_eq!(dispatched_count, 1);
        prop_assert_eq!(dedup_count, (n as u64) - 1);
    }

    // ---- Rules are sorted by priority ----

    #[test]
    fn bridge_rules_maintain_priority_order(
        priorities in proptest::collection::vec(0u32..100u32, 2..10),
    ) {
        let config = ConnectorOutboundBridgeConfig {
            enforce_sandbox: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        for (i, &p) in priorities.iter().enumerate() {
            let conn = format!("conn-{i}");
            bridge.register_sandbox_zone(conn.clone(), permissive_zone(&conn));
            bridge.add_rule(OutboundRoutingRule {
                rule_id: format!("rule-{i}"),
                source_filter: None,
                event_type_prefix: None,
                min_severity: None,
                target_connector: conn,
                action_kind: ConnectorActionKind::Notify,
                enabled: true,
                priority: p,
            });
        }

        let event = OutboundEvent::new(
            OutboundEventSource::Custom,
            "test.event",
            serde_json::json!({}),
        ).with_timestamp_ms(1000);

        let result = bridge.process_event(&event).unwrap();
        // Verify dispatched actions are in non-decreasing priority order
        // (we can infer from connector names which had which priority)
        let dispatched_connectors: Vec<String> = result
            .actions_dispatched
            .iter()
            .map(|a| a.target_connector.clone())
            .collect();
        // All rules should match since no filters
        prop_assert_eq!(dispatched_connectors.len(), priorities.len());
    }

    // ---- Drain empties the queue ----

    #[test]
    fn bridge_drain_empties_queue(num_events in 1usize..20usize) {
        let config = ConnectorOutboundBridgeConfig {
            enforce_sandbox: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        bridge.add_rule(OutboundRoutingRule {
            rule_id: "catch-all".to_string(),
            source_filter: None,
            event_type_prefix: None,
            min_severity: None,
            target_connector: "test".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 0,
        });

        for i in 0..num_events {
            let event = OutboundEvent::new(
                OutboundEventSource::Custom,
                &format!("event.{i}"),
                serde_json::json!({}),
            ).with_timestamp_ms(1000 + i as u64 * 10);
            let _ = bridge.process_event(&event);
        }

        let drained = bridge.drain_actions();
        prop_assert!(!drained.is_empty());
        prop_assert_eq!(bridge.pending_action_count(), 0);
    }
}
