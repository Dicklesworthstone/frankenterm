//! Property tests for fleet_dashboard module (ft-3681t.7.2).
//!
//! Covers serde roundtrips for alert severity/route/condition types,
//! alert manager lifecycle invariants, dashboard view construction,
//! and default policy configuration checks.

use frankenterm_core::fleet_dashboard::*;
use frankenterm_core::unified_telemetry::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_fleet_alert_severity() -> impl Strategy<Value = FleetAlertSeverity> {
    prop_oneof![
        Just(FleetAlertSeverity::Info),
        Just(FleetAlertSeverity::Warning),
        Just(FleetAlertSeverity::Critical),
        Just(FleetAlertSeverity::Emergency),
    ]
}


fn make_ingest_payload() -> SubsystemPayload {
    SubsystemPayload::Ingest(IngestPayload {
        snapshot: frankenterm_core::tailer::SchedulerSnapshot {
            budget_active: false,
            max_captures_per_sec: 0,
            max_bytes_per_sec: 0,
            captures_remaining: 0,
            bytes_remaining: 0,
            total_rate_limited: 0,
            total_byte_budget_exceeded: 0,
            total_throttle_events: 0,
            tracked_panes: 0,
        },
    })
}

fn healthy_snapshot() -> UnifiedFleetSnapshot {
    let env = EnvelopeBuilder::new(SubsystemLayer::Policy, 1_710_000_000_000)
        .health(HealthStatus::Healthy)
        .build(make_ingest_payload());
    UnifiedFleetSnapshot::from_envelopes(1_710_000_000_000, vec![env], vec![])
}

fn degraded_snapshot() -> UnifiedFleetSnapshot {
    let e1 = EnvelopeBuilder::new(SubsystemLayer::Policy, 1_710_000_000_000)
        .health(HealthStatus::Healthy)
        .build(make_ingest_payload());
    let e2 = EnvelopeBuilder::new(SubsystemLayer::Mux, 1_710_000_000_000)
        .health(HealthStatus::Degraded)
        .build(make_ingest_payload());
    UnifiedFleetSnapshot::from_envelopes(1_710_000_000_000, vec![e1, e2], vec![])
}

fn unhealthy_snapshot() -> UnifiedFleetSnapshot {
    let e1 = EnvelopeBuilder::new(SubsystemLayer::Policy, 1_710_000_000_000)
        .health(HealthStatus::Unhealthy)
        .build(make_ingest_payload());
    let e2 = EnvelopeBuilder::new(SubsystemLayer::Mux, 1_710_000_000_000)
        .health(HealthStatus::Unhealthy)
        .build(make_ingest_payload());
    let e3 = EnvelopeBuilder::new(SubsystemLayer::Swarm, 1_710_000_000_000)
        .health(HealthStatus::Unhealthy)
        .build(make_ingest_payload());
    UnifiedFleetSnapshot::from_envelopes(1_710_000_000_000, vec![e1, e2, e3], vec![])
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_fleet_alert_severity(sev in arb_fleet_alert_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: FleetAlertSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn serde_roundtrip_alert_route_log(_dummy in 0..1u32) {
        let route = AlertRoute::Log;
        let json = serde_json::to_string(&route).unwrap();
        let back: AlertRoute = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(route, back);
    }

    #[test]
    fn serde_roundtrip_alert_route_dashboard(_dummy in 0..1u32) {
        let route = AlertRoute::Dashboard;
        let json = serde_json::to_string(&route).unwrap();
        let back: AlertRoute = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(route, back);
    }

    #[test]
    fn serde_roundtrip_runbook_ref(_dummy in 0..1u32) {
        let rb = RunbookRef {
            id: "RB-001".into(),
            title: "Test".into(),
            doc_link: Some("https://example.com".into()),
            steps: vec!["step-1".into()],
        };
        let json = serde_json::to_string(&rb).unwrap();
        let back: RunbookRef = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rb, back);
    }

    #[test]
    fn serde_roundtrip_alert_context(_dummy in 0..1u32) {
        let ctx = AlertContext {
            fleet_health: HealthStatus::Degraded,
            layer_health: [("policy".to_string(), HealthStatus::Healthy)]
                .into_iter()
                .collect(),
            redaction_ceiling: RedactionLabel::Internal,
            envelope_count: 5,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: AlertContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.fleet_health, HealthStatus::Degraded);
        prop_assert_eq!(back.envelope_count, 5);
    }
}

// =============================================================================
// Alert severity ordering
// =============================================================================

proptest! {
    #[test]
    fn severity_total_order(a in arb_fleet_alert_severity(), b in arb_fleet_alert_severity()) {
        prop_assert!(a <= b || a > b);
    }

    #[test]
    fn info_is_minimum(sev in arb_fleet_alert_severity()) {
        prop_assert!(sev >= FleetAlertSeverity::Info);
    }

    #[test]
    fn emergency_is_maximum(sev in arb_fleet_alert_severity()) {
        prop_assert!(sev <= FleetAlertSeverity::Emergency);
    }
}

// =============================================================================
// Alert manager lifecycle
// =============================================================================

proptest! {
    #[test]
    fn healthy_fleet_no_alerts(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let fired = mgr.evaluate(&snap);
        prop_assert!(fired.is_empty());
        prop_assert!(mgr.active_alerts().is_empty());
    }

    #[test]
    fn degraded_fleet_fires_alerts(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = degraded_snapshot();
        let fired = mgr.evaluate(&snap);
        prop_assert!(!fired.is_empty());
    }

    #[test]
    fn unhealthy_fleet_fires_critical(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = unhealthy_snapshot();
        let fired = mgr.evaluate(&snap);
        let has_critical = fired.iter().any(|a| a.severity >= FleetAlertSeverity::Critical);
        prop_assert!(has_critical);
    }

    #[test]
    fn acknowledge_makes_acked(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = degraded_snapshot();
        mgr.evaluate(&snap);
        let active = mgr.active_alerts();
        if !active.is_empty() {
            let id = active[0].alert_id;
            let ok = mgr.acknowledge(id, "op");
            prop_assert!(ok);
            let alert = mgr.all_alerts().iter().find(|a| a.alert_id == id).unwrap();
            prop_assert!(alert.is_acked());
            prop_assert!(alert.is_active()); // Still active until resolved
        }
    }

    #[test]
    fn resolve_makes_inactive(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = degraded_snapshot();
        mgr.evaluate(&snap);
        let active = mgr.active_alerts();
        if !active.is_empty() {
            let id = active[0].alert_id;
            let ok = mgr.resolve(id, "fixed");
            prop_assert!(ok);
            let alert = mgr.all_alerts().iter().find(|a| a.alert_id == id).unwrap();
            prop_assert!(!alert.is_active());
        }
    }

    #[test]
    fn acknowledge_nonexistent_returns_false(id in 1000..2000u64) {
        let mut mgr = FleetAlertManager::with_defaults();
        prop_assert!(!mgr.acknowledge(id, "nobody"));
    }

    #[test]
    fn resolve_nonexistent_returns_false(id in 1000..2000u64) {
        let mut mgr = FleetAlertManager::with_defaults();
        prop_assert!(!mgr.resolve(id, "nothing"));
    }

    #[test]
    fn active_counts_sum_matches_active_alerts(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = unhealthy_snapshot();
        mgr.evaluate(&snap);

        let counts = mgr.active_counts_by_severity();
        let sum: usize = counts.values().sum();
        prop_assert_eq!(sum, mgr.active_alerts().len());
    }
}

// =============================================================================
// Dashboard view invariants
// =============================================================================

proptest! {
    #[test]
    fn dashboard_fleet_health_matches_snapshot(_dummy in 0..1u32) {
        let mgr = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let view = FleetDashboardView::from_snapshot(&snap, &mgr);
        prop_assert_eq!(view.fleet_health, snap.fleet_health);
        prop_assert_eq!(view.total_envelopes, snap.envelope_count());
    }

    #[test]
    fn dashboard_redaction_matches_snapshot(_dummy in 0..1u32) {
        let mgr = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let view = FleetDashboardView::from_snapshot(&snap, &mgr);
        prop_assert_eq!(view.redaction_ceiling, snap.redaction_ceiling);
    }

    #[test]
    fn dashboard_no_critical_alerts_on_healthy(_dummy in 0..1u32) {
        let mgr = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let view = FleetDashboardView::from_snapshot(&snap, &mgr);
        prop_assert!(!view.has_critical_alerts());
    }

    #[test]
    fn dashboard_has_critical_on_unhealthy(_dummy in 0..1u32) {
        let mut mgr = FleetAlertManager::with_defaults();
        let snap = unhealthy_snapshot();
        mgr.evaluate(&snap);
        let view = FleetDashboardView::from_snapshot(&snap, &mgr);
        prop_assert!(view.has_critical_alerts());
    }

    #[test]
    fn dashboard_summary_line_not_empty(_dummy in 0..1u32) {
        let mgr = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let view = FleetDashboardView::from_snapshot(&snap, &mgr);
        prop_assert!(!view.summary_line.is_empty());
    }

    #[test]
    fn dashboard_view_serde_roundtrip(_dummy in 0..1u32) {
        let mgr = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let view = FleetDashboardView::from_snapshot(&snap, &mgr);
        let json = serde_json::to_string(&view).unwrap();
        let back: FleetDashboardView = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.fleet_health, view.fleet_health);
        prop_assert_eq!(back.total_envelopes, view.total_envelopes);
        prop_assert_eq!(back.redaction_ceiling, view.redaction_ceiling);
    }
}

// =============================================================================
// Default policies invariants
// =============================================================================

#[test]
fn default_policies_all_enabled() {
    let policies = default_policies();
    assert!(policies.len() >= 5);
    for p in &policies {
        assert!(p.enabled);
    }
}

#[test]
fn default_policies_unique_class_ids() {
    let policies = default_policies();
    let mut seen = std::collections::HashSet::new();
    for p in &policies {
        assert!(seen.insert(&p.class_id));
    }
}

#[test]
fn alert_severity_ordering_correct() {
    assert!(FleetAlertSeverity::Info < FleetAlertSeverity::Warning);
    assert!(FleetAlertSeverity::Warning < FleetAlertSeverity::Critical);
    assert!(FleetAlertSeverity::Critical < FleetAlertSeverity::Emergency);
}
