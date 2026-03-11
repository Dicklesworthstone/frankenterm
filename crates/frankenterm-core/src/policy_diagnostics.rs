//! PolicyEngine health diagnostics.
//!
//! Provides structured health checks for all PolicyEngine subsystems,
//! integrating with the [`HealthCheckRegistry`](crate::runtime_health::HealthCheckRegistry)
//! to surface policy-layer health alongside runtime health in `ft doctor` output.
//!
//! # Checks
//!
//! | Check ID | What it inspects |
//! |----------|-----------------|
//! | `policy.decision_log` | Decision log capacity and eviction pressure |
//! | `policy.quarantine` | Active quarantine entries and kill-switch state |
//! | `policy.audit_chain` | Audit chain capacity and entry count |
//! | `policy.compliance` | Active violations and compliance status |
//! | `policy.approvals` | Pending and expired approval requests |
//! | `policy.revocations` | Active revocation count |
//! | `policy.namespace` | Namespace isolation configuration |
//! | `policy.connector_governor` | Rate limiting, throttle, and rejection counters |
//! | `policy.connector_registry` | Package trust verification failures |
//! | `policy.connector_lifecycle` | Connector health states (degraded, failed) |
//! | `policy.connector_reliability` | Circuit breaker and DLQ pressure |
//! | `policy.connector_mesh` | Mesh host reachability |
//! | `policy.bundles` | Bundle registry tier distribution |
//! | `policy.ingestion` | Ingestion pipeline health |

use crate::policy::PolicyEngine;
use crate::runtime_health::{RemediationEffort, RemediationHint, RuntimeHealthCheck};

/// Run all PolicyEngine health checks and return individual results.
///
/// Each check inspects a specific subsystem and produces a
/// [`RuntimeHealthCheck`] with status, evidence, and remediation hints.
/// These can be registered directly into a [`HealthCheckRegistry`].
///
/// `now_ms` is the current time in epoch milliseconds, used for
/// time-dependent checks (expiry, staleness, snapshot timestamps).
pub fn check_policy_engine_health(
    engine: &mut PolicyEngine,
    now_ms: u64,
) -> Vec<RuntimeHealthCheck> {
    vec![
        check_decision_log(engine),
        check_quarantine(engine, now_ms),
        check_audit_chain(engine, now_ms),
        check_compliance(engine, now_ms),
        check_approvals(engine, now_ms),
        check_revocations(engine),
        check_namespace_isolation(engine),
        check_connector_governor(engine, now_ms),
        check_connector_registry(engine),
        check_connector_lifecycle(engine),
        check_connector_reliability(engine),
        check_connector_mesh(engine),
        check_bundles(engine, now_ms),
        check_ingestion(engine, now_ms),
    ]
}

// =============================================================================
// Decision log
// =============================================================================

fn check_decision_log(engine: &PolicyEngine) -> RuntimeHealthCheck {
    let snap = engine.decision_log().snapshot();
    let fill_ratio = if snap.max_entries > 0 {
        snap.current_entries as f64 / snap.max_entries as f64
    } else {
        0.0
    };

    if fill_ratio > 0.95 {
        RuntimeHealthCheck::warn(
            "policy.decision_log",
            "Decision Log",
            &format!(
                "Log near capacity: {}/{} entries ({:.0}% full), {} evicted",
                snap.current_entries,
                snap.max_entries,
                fill_ratio * 100.0,
                snap.total_evicted,
            ),
        )
        .with_evidence(&format!("total_recorded={}", snap.total_recorded))
        .with_evidence(&format!(
            "deny={} allow={} require_approval={}",
            snap.deny_count, snap.allow_count, snap.require_approval_count,
        ))
        .with_remediation(
            RemediationHint::text(
                "Increase decision log max_entries in SafetyConfig or review eviction rate",
            )
            .effort(RemediationEffort::Low),
        )
    } else {
        RuntimeHealthCheck::pass(
            "policy.decision_log",
            "Decision Log",
            &format!(
                "{}/{} entries ({:.0}% full), {} decisions recorded",
                snap.current_entries,
                snap.max_entries,
                fill_ratio * 100.0,
                snap.total_recorded,
            ),
        )
    }
}

// =============================================================================
// Quarantine
// =============================================================================

fn check_quarantine(engine: &PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let snap = engine.quarantine_registry().telemetry_snapshot(now_ms);
    let ks = engine.quarantine_registry().kill_switch();
    let ks_level = ks.level;
    let active = engine.quarantine_registry().active_quarantines();

    if ks_level >= crate::policy_quarantine::KillSwitchLevel::HardStop {
        RuntimeHealthCheck::fail(
            "policy.quarantine",
            "Quarantine & Kill Switch",
            &format!("Kill switch at {:?} — all actions blocked", ks_level),
        )
        .with_evidence(&format!("active_quarantines={}", active.len()))
        .with_remediation(
            RemediationHint::text("Review kill switch state: ft audit --decision deny")
                .effort(RemediationEffort::High),
        )
    } else if ks_level == crate::policy_quarantine::KillSwitchLevel::SoftStop {
        RuntimeHealthCheck::warn(
            "policy.quarantine",
            "Quarantine & Kill Switch",
            &format!(
                "Kill switch at SoftStop — new workflows paused, {} quarantined",
                active.len(),
            ),
        )
        .with_evidence(&format!(
            "quarantined_components: [{}]",
            active.join(", "),
        ))
        .with_remediation(
            RemediationHint::text("Investigate quarantine causes before clearing")
                .effort(RemediationEffort::Medium),
        )
    } else if !active.is_empty() {
        RuntimeHealthCheck::warn(
            "policy.quarantine",
            "Quarantine & Kill Switch",
            &format!("{} component(s) quarantined", active.len()),
        )
        .with_evidence(&format!(
            "quarantined: [{}]",
            active.join(", "),
        ))
    } else {
        RuntimeHealthCheck::pass(
            "policy.quarantine",
            "Quarantine & Kill Switch",
            &format!(
                "No quarantines active, kill switch disarmed (total_events={})",
                snap.counters.quarantines_imposed,
            ),
        )
    }
}

// =============================================================================
// Audit chain
// =============================================================================

fn check_audit_chain(engine: &PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let snap = engine.audit_chain().telemetry_snapshot(now_ms);
    let fill_ratio = if snap.max_entries > 0 {
        snap.chain_length as f64 / snap.max_entries as f64
    } else {
        0.0
    };

    if fill_ratio > 0.9 {
        RuntimeHealthCheck::warn(
            "policy.audit_chain",
            "Audit Chain",
            &format!(
                "Chain near capacity: {}/{} entries ({:.0}% full)",
                snap.chain_length,
                snap.max_entries,
                fill_ratio * 100.0,
            ),
        )
        .with_remediation(
            RemediationHint::text("Increase audit chain capacity in SafetyConfig")
                .effort(RemediationEffort::Low),
        )
    } else {
        RuntimeHealthCheck::pass(
            "policy.audit_chain",
            "Audit Chain",
            &format!(
                "{}/{} entries, seq={}",
                snap.chain_length, snap.max_entries, snap.next_sequence,
            ),
        )
    }
}

// =============================================================================
// Compliance
// =============================================================================

fn check_compliance(engine: &mut PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let snap = engine.compliance_engine_mut().snapshot(now_ms);
    let violation_count = snap.active_violations.len();

    let has_critical = snap.active_violations.iter().any(|v| {
        matches!(
            v.severity,
            crate::policy_compliance::ViolationSeverity::Critical
        )
    });

    if has_critical {
        RuntimeHealthCheck::fail(
            "policy.compliance",
            "Compliance Engine",
            &format!(
                "Critical violations detected: {} active, status={:?}",
                violation_count, snap.overall_status,
            ),
        )
        .with_evidence(&format!(
            "evaluations={} violations_total={}",
            snap.counters.total_evaluations, snap.counters.total_violations_detected,
        ))
        .with_remediation(
            RemediationHint::text("Review critical violations: ft audit --decision deny")
                .effort(RemediationEffort::High),
        )
    } else if violation_count > 0 {
        RuntimeHealthCheck::warn(
            "policy.compliance",
            "Compliance Engine",
            &format!(
                "{} active violation(s), status={:?}",
                violation_count, snap.overall_status,
            ),
        )
        .with_evidence(&format!(
            "evaluations={} violations_total={}",
            snap.counters.total_evaluations, snap.counters.total_violations_detected,
        ))
    } else {
        RuntimeHealthCheck::pass(
            "policy.compliance",
            "Compliance Engine",
            &format!(
                "No active violations, status={:?}, {} evaluations",
                snap.overall_status, snap.counters.total_evaluations,
            ),
        )
    }
}

// =============================================================================
// Approvals
// =============================================================================

fn check_approvals(engine: &mut PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let expired = engine.approval_tracker_mut().expire_stale(now_ms);
    let snap = engine.approval_tracker().snapshot();
    let pending = snap.pending;

    if pending > 10 {
        RuntimeHealthCheck::warn(
            "policy.approvals",
            "Approval Tracker",
            &format!(
                "{} pending approvals (backlog), {} expired this cycle",
                pending, expired,
            ),
        )
        .with_evidence(&format!(
            "total={} approved={} rejected={} revoked={}",
            snap.total, snap.approved, snap.rejected, snap.revoked,
        ))
        .with_remediation(
            RemediationHint::text("Review and resolve pending approvals: ft approve --list")
                .effort(RemediationEffort::Medium),
        )
    } else {
        RuntimeHealthCheck::pass(
            "policy.approvals",
            "Approval Tracker",
            &format!(
                "{} pending, {} total tracked, {} expired this cycle",
                pending, snap.total, expired,
            ),
        )
    }
}

// =============================================================================
// Revocations
// =============================================================================

fn check_revocations(engine: &PolicyEngine) -> RuntimeHealthCheck {
    let snap = engine.revocation_registry().snapshot();

    if snap.active_revocations > 20 {
        RuntimeHealthCheck::warn(
            "policy.revocations",
            "Revocation Registry",
            &format!(
                "{} active revocations — review if any should be reinstated",
                snap.active_revocations,
            ),
        )
        .with_evidence(&format!("total_records={}", snap.total_records))
    } else {
        RuntimeHealthCheck::pass(
            "policy.revocations",
            "Revocation Registry",
            &format!(
                "{} active revocations, {} total records",
                snap.active_revocations, snap.total_records,
            ),
        )
    }
}

// =============================================================================
// Namespace isolation
// =============================================================================

fn check_namespace_isolation(engine: &PolicyEngine) -> RuntimeHealthCheck {
    if !engine.namespace_isolation_enabled() {
        return RuntimeHealthCheck::pass(
            "policy.namespace",
            "Namespace Isolation",
            "Disabled (single-tenant mode)",
        );
    }

    let snap = engine.namespace_registry().snapshot();
    if snap.active_namespaces == 0 {
        RuntimeHealthCheck::warn(
            "policy.namespace",
            "Namespace Isolation",
            "Enabled but no namespaces registered — cross-tenant checks have no effect",
        )
        .with_remediation(
            RemediationHint::text("Register at least one namespace via PolicyEngine::bind_resource_to_namespace")
                .effort(RemediationEffort::Low),
        )
    } else {
        RuntimeHealthCheck::pass(
            "policy.namespace",
            "Namespace Isolation",
            &format!(
                "{} active namespaces, {} bindings, default={:?}",
                snap.active_namespaces, snap.total_bindings, snap.default_decision,
            ),
        )
    }
}

// =============================================================================
// Connector governor
// =============================================================================

fn check_connector_governor(engine: &mut PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let snap = engine.connector_governor_mut().snapshot(now_ms);

    if snap.telemetry.rejections > 0 {
        RuntimeHealthCheck::warn(
            "policy.connector_governor",
            "Connector Governor",
            &format!(
                "{} rejection(s) recorded — some connector actions blocked by rate/quota policy",
                snap.telemetry.rejections,
            ),
        )
        .with_evidence(&format!(
            "evaluations={} allows={} throttles={} rejections={}",
            snap.telemetry.evaluations,
            snap.telemetry.allows,
            snap.telemetry.throttles,
            snap.telemetry.rejections,
        ))
        .with_evidence(&format!(
            "global_rate_fill={:.1}%",
            snap.global_rate_fill_ratio * 100.0,
        ))
    } else {
        RuntimeHealthCheck::pass(
            "policy.connector_governor",
            "Connector Governor",
            &format!(
                "{} evaluations, rate fill={:.1}%",
                snap.telemetry.evaluations,
                snap.global_rate_fill_ratio * 100.0,
            ),
        )
    }
}

// =============================================================================
// Connector registry
// =============================================================================

fn check_connector_registry(engine: &PolicyEngine) -> RuntimeHealthCheck {
    let snap = engine.connector_registry().telemetry().snapshot();

    if snap.trust_denials > 0 || snap.digest_failures > 0 {
        RuntimeHealthCheck::warn(
            "policy.connector_registry",
            "Connector Registry",
            &format!(
                "{} trust denial(s), {} digest failure(s) — potential supply chain issues",
                snap.trust_denials, snap.digest_failures,
            ),
        )
        .with_evidence(&format!(
            "registered={} verified={} lookups={}",
            snap.packages_registered, snap.packages_verified, snap.lookups,
        ))
        .with_remediation(
            RemediationHint::text("Verify connector package signatures and trusted publishers")
                .effort(RemediationEffort::Medium),
        )
    } else {
        RuntimeHealthCheck::pass(
            "policy.connector_registry",
            "Connector Registry",
            &format!(
                "{} packages registered, {} verified, {} lookups",
                snap.packages_registered, snap.packages_verified, snap.lookups,
            ),
        )
    }
}

// =============================================================================
// Connector lifecycle
// =============================================================================

fn check_connector_lifecycle(engine: &PolicyEngine) -> RuntimeHealthCheck {
    let summary = engine.lifecycle_manager().summary();

    if summary.failed > 0 {
        RuntimeHealthCheck::fail(
            "policy.connector_lifecycle",
            "Connector Lifecycle",
            &format!(
                "{} connector(s) in failed state",
                summary.failed,
            ),
        )
        .with_evidence(&format!(
            "total={} running={} stopped={} degraded={} failed={}",
            summary.total, summary.running, summary.stopped, summary.degraded, summary.failed,
        ))
        .with_remediation(
            RemediationHint::text("Restart failed connectors or investigate root cause")
                .effort(RemediationEffort::High),
        )
    } else if summary.degraded > 0 {
        RuntimeHealthCheck::warn(
            "policy.connector_lifecycle",
            "Connector Lifecycle",
            &format!(
                "{} connector(s) degraded",
                summary.degraded,
            ),
        )
        .with_evidence(&format!(
            "total={} running={} stopped={} degraded={}",
            summary.total, summary.running, summary.stopped, summary.degraded,
        ))
    } else {
        RuntimeHealthCheck::pass(
            "policy.connector_lifecycle",
            "Connector Lifecycle",
            &format!(
                "{} total, {} running, {} enabled",
                summary.total, summary.running, summary.enabled,
            ),
        )
    }
}

// =============================================================================
// Connector reliability
// =============================================================================

fn check_connector_reliability(engine: &PolicyEngine) -> RuntimeHealthCheck {
    let snapshots = engine.reliability_registry().all_snapshots();
    let open_circuits: Vec<_> = snapshots
        .iter()
        .filter(|c| c.circuit_rejections > 0)
        .collect();

    if !open_circuits.is_empty() {
        RuntimeHealthCheck::warn(
            "policy.connector_reliability",
            "Connector Reliability",
            &format!(
                "{} connector(s) with circuit-breaker rejections",
                open_circuits.len(),
            ),
        )
        .with_evidence(&format!(
            "affected: [{}]",
            open_circuits
                .iter()
                .map(|c| format!("{}(rej={})", c.connector_id, c.circuit_rejections))
                .collect::<Vec<_>>()
                .join(", "),
        ))
    } else {
        let total_ops: u64 = snapshots.iter().map(|c| c.operations_attempted).sum();
        RuntimeHealthCheck::pass(
            "policy.connector_reliability",
            "Connector Reliability",
            &format!(
                "{} connector(s) tracked, {} total operations",
                snapshots.len(),
                total_ops,
            ),
        )
    }
}

// =============================================================================
// Connector mesh
// =============================================================================

fn check_connector_mesh(engine: &PolicyEngine) -> RuntimeHealthCheck {
    let snap = engine.connector_mesh().telemetry().snapshot();

    if snap.routing_failures > 0 {
        RuntimeHealthCheck::warn(
            "policy.connector_mesh",
            "Connector Mesh",
            &format!(
                "{} routing failure(s) — check mesh zone connectivity",
                snap.routing_failures,
            ),
        )
        .with_evidence(&format!(
            "hosts={} zones={} successes={} failures={}",
            snap.hosts_registered, snap.zones_created, snap.routing_successes, snap.routing_failures,
        ))
    } else {
        RuntimeHealthCheck::pass(
            "policy.connector_mesh",
            "Connector Mesh",
            &format!(
                "{} hosts, {} zones, {} routing requests",
                snap.hosts_registered, snap.zones_created, snap.routing_requests,
            ),
        )
    }
}

// =============================================================================
// Bundle registry
// =============================================================================

fn check_bundles(engine: &PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let snap = engine.bundle_registry().snapshot(now_ms);

    RuntimeHealthCheck::pass(
        "policy.bundles",
        "Bundle Registry",
        &format!(
            "{} bundles registered, {} audit log entries",
            snap.bundle_count, snap.audit_log_length,
        ),
    )
    .with_evidence(&format!(
        "registered={} removed={} validations={}",
        snap.counters.bundles_registered,
        snap.counters.bundles_removed,
        snap.counters.validations_run,
    ))
}

// =============================================================================
// Ingestion pipeline
// =============================================================================

fn check_ingestion(engine: &PolicyEngine, now_ms: u64) -> RuntimeHealthCheck {
    let snap = engine.ingestion_pipeline().snapshot(now_ms);

    if snap.counters.events_rejected > 0 {
        RuntimeHealthCheck::warn(
            "policy.ingestion",
            "Ingestion Pipeline",
            &format!(
                "{} event(s) rejected during ingestion",
                snap.counters.events_rejected,
            ),
        )
        .with_evidence(&format!(
            "ingested={} rejected={} audit_chain_length={}",
            snap.counters.events_recorded, snap.counters.events_rejected, snap.audit_chain_length,
        ))
    } else {
        RuntimeHealthCheck::pass(
            "policy.ingestion",
            "Ingestion Pipeline",
            &format!(
                "{} events ingested, {} audit chain entries",
                snap.counters.events_recorded, snap.audit_chain_length,
            ),
        )
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_health::CheckStatus;

    #[test]
    fn default_engine_all_checks_pass() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let now_ms = 1_700_000_000_000;
        let checks = check_policy_engine_health(&mut engine, now_ms);

        assert_eq!(checks.len(), 14, "expected 14 health checks");
        for check in &checks {
            assert!(
                check.status.is_healthy(),
                "check {} should be healthy, got {:?}: {}",
                check.check_id,
                check.status,
                check.summary,
            );
        }
    }

    #[test]
    fn default_engine_check_ids_are_namespaced() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let checks = check_policy_engine_health(&mut engine, 1_700_000_000_000);
        for check in &checks {
            assert!(
                check.check_id.starts_with("policy."),
                "check_id '{}' must start with 'policy.'",
                check.check_id,
            );
        }
    }

    #[test]
    fn quarantine_triggers_warn() {
        let mut engine = PolicyEngine::new(10, 100, true);
        engine
            .quarantine_registry_mut()
            .quarantine(
                "test-component",
                crate::policy_quarantine::ComponentKind::Connector,
                crate::policy_quarantine::QuarantineSeverity::Isolated,
                crate::policy_quarantine::QuarantineReason::OperatorDirected {
                    operator: "test".to_string(),
                    note: "Test reason".to_string(),
                },
                "test-operator",
                1_700_000_000_000,
                1_700_000_060_000,
            )
            .unwrap();
        let check = check_quarantine(&engine, 1_700_000_000_000);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.summary.contains("1 component"));
    }

    #[test]
    fn revocations_pass_when_empty() {
        let engine = PolicyEngine::new(10, 100, true);
        let check = check_revocations(&engine);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.summary.contains("0 active"));
    }

    #[test]
    fn namespace_disabled_is_pass() {
        let engine = PolicyEngine::new(10, 100, true);
        let check = check_namespace_isolation(&engine);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.summary.contains("Disabled"));
    }

    #[test]
    fn check_results_serialize_to_json() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let checks = check_policy_engine_health(&mut engine, 1_700_000_000_000);
        for check in &checks {
            let json = serde_json::to_string(check).unwrap();
            assert!(!json.is_empty());
            // Verify it deserializes back
            let _: RuntimeHealthCheck = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn approval_tracker_pass_when_empty() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let check = check_approvals(&mut engine, 1_700_000_000_000);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.summary.contains("0 pending"));
    }

    #[test]
    fn compliance_pass_when_no_violations() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let check = check_compliance(&mut engine, 1_700_000_000_000);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.summary.contains("No active violations"));
    }

    #[test]
    fn decision_log_pass_when_under_capacity() {
        let engine = PolicyEngine::new(10, 100, true);
        let check = check_decision_log(&engine);
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn all_checks_have_display_names() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let checks = check_policy_engine_health(&mut engine, 1_700_000_000_000);
        for check in &checks {
            assert!(
                !check.display_name.is_empty(),
                "check {} has empty display_name",
                check.check_id,
            );
        }
    }

    #[test]
    fn all_checks_have_summaries() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let checks = check_policy_engine_health(&mut engine, 1_700_000_000_000);
        for check in &checks {
            assert!(
                !check.summary.is_empty(),
                "check {} has empty summary",
                check.check_id,
            );
        }
    }

    #[test]
    fn health_report_integration() {
        use crate::runtime_health::HealthCheckRegistry;

        let mut engine = PolicyEngine::new(10, 100, true);
        let policy_checks = check_policy_engine_health(&mut engine, 1_700_000_000_000);

        let mut registry = HealthCheckRegistry::new();
        for check in policy_checks {
            registry.register(check);
        }
        let report = registry.build_report();

        assert!(report.overall_healthy());
        assert_eq!(report.status_counts.total(), 14);
        assert_eq!(report.status_counts.fail, 0);
    }
}
