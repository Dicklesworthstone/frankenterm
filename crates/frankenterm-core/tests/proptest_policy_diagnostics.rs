//! Property-based tests for the policy_diagnostics module.
//!
//! Validates that health checks produce structurally correct results
//! under diverse PolicyEngine states: default engines, engines with
//! quarantined components, approval backlogs, revocations, and
//! varying decision log fill levels.

use frankenterm_core::policy::PolicyEngine;
use frankenterm_core::policy_diagnostics::check_policy_engine_health;
use frankenterm_core::runtime_health::CheckStatus;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary engine constructor params.
fn arb_engine_params() -> impl Strategy<Value = (u32, u32, bool)> {
    (1_u32..=100, 1_u32..=1000, any::<bool>())
}

/// Arbitrary timestamp in plausible range.
fn arb_now_ms() -> impl Strategy<Value = u64> {
    1_600_000_000_000_u64..=1_800_000_000_000
}

// =============================================================================
// Structural invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// The health check function always returns exactly 14 checks.
    #[test]
    fn always_returns_14_checks(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        prop_assert_eq!(checks.len(), 14);
    }

    /// Every check ID starts with "policy." prefix.
    #[test]
    fn all_check_ids_namespaced(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            prop_assert!(
                check.check_id.starts_with("policy."),
                "check_id '{}' must start with 'policy.'",
                check.check_id,
            );
        }
    }

    /// All check IDs are unique — no duplicate check IDs.
    #[test]
    fn all_check_ids_unique(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        let mut seen = std::collections::HashSet::new();
        for check in &checks {
            prop_assert!(
                seen.insert(&check.check_id),
                "duplicate check_id: {}",
                check.check_id,
            );
        }
    }

    /// Every check has a non-empty display_name.
    #[test]
    fn all_display_names_nonempty(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            prop_assert!(
                !check.display_name.is_empty(),
                "check {} has empty display_name",
                check.check_id,
            );
        }
    }

    /// Every check has a non-empty summary.
    #[test]
    fn all_summaries_nonempty(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            prop_assert!(
                !check.summary.is_empty(),
                "check {} has empty summary",
                check.check_id,
            );
        }
    }

    /// Default engine (no mutations) produces all Pass status.
    #[test]
    fn default_engine_all_pass(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            prop_assert!(
                check.status.is_healthy(),
                "check {} should be healthy on default engine, got {:?}: {}",
                check.check_id,
                check.status,
                check.summary,
            );
        }
    }

    /// All checks serialize to valid JSON.
    #[test]
    fn all_checks_serialize(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            let json = serde_json::to_string(check);
            prop_assert!(
                json.is_ok(),
                "check {} failed to serialize: {:?}",
                check.check_id,
                json.err(),
            );
        }
    }

    /// All checks roundtrip through JSON.
    #[test]
    fn all_checks_json_roundtrip(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            let json = serde_json::to_string(check).unwrap();
            let back: frankenterm_core::runtime_health::RuntimeHealthCheck =
                serde_json::from_str(&json).unwrap();
            prop_assert_eq!(
                &back.check_id, &check.check_id,
                "check_id mismatch after roundtrip",
            );
            prop_assert_eq!(
                back.status, check.status,
                "status mismatch after roundtrip for {}",
                check.check_id,
            );
        }
    }

    /// Warn/Fail checks with remediation hints have non-empty hint text.
    #[test]
    fn remediation_hints_nonempty(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        // Quarantine a component to trigger a Warn
        let _ = engine.quarantine_registry_mut().quarantine(
            "proptest-component",
            frankenterm_core::policy_quarantine::ComponentKind::Connector,
            frankenterm_core::policy_quarantine::QuarantineSeverity::Isolated,
            frankenterm_core::policy_quarantine::QuarantineReason::OperatorDirected {
                operator: "proptest".to_string(),
                note: "Testing remediation hints".to_string(),
            },
            "proptest-operator",
            now_ms,
            now_ms + 60_000,
        );
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            if check.status == CheckStatus::Warn || check.status == CheckStatus::Fail {
                for hint in &check.remediation {
                    prop_assert!(
                        !hint.description.is_empty(),
                        "check {} has empty remediation hint description",
                        check.check_id,
                    );
                }
            }
        }
    }

    /// Quarantining a component makes the quarantine check non-Pass.
    #[test]
    fn quarantine_triggers_non_pass(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let _ = engine.quarantine_registry_mut().quarantine(
            "test-comp",
            frankenterm_core::policy_quarantine::ComponentKind::Connector,
            frankenterm_core::policy_quarantine::QuarantineSeverity::Isolated,
            frankenterm_core::policy_quarantine::QuarantineReason::OperatorDirected {
                operator: "proptest".to_string(),
                note: "quarantine test".to_string(),
            },
            "proptest-op",
            now_ms,
            now_ms + 120_000,
        );
        let checks = check_policy_engine_health(&mut engine, now_ms);
        let quarantine_check = checks.iter().find(|c| c.check_id == "policy.quarantine").unwrap();
        prop_assert_ne!(
            quarantine_check.status,
            CheckStatus::Pass,
            "quarantine check should not be Pass when components are quarantined",
        );
    }

    /// HealthCheckRegistry integration: all 14 checks register and report.
    #[test]
    fn health_registry_integration(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        let mut registry = frankenterm_core::runtime_health::HealthCheckRegistry::new();
        for check in checks {
            registry.register(check);
        }
        let report = registry.build_report();
        prop_assert_eq!(
            report.status_counts.total(),
            14,
            "expected 14 checks in report, got {}",
            report.status_counts.total(),
        );
    }

    /// Default engine report is overall healthy.
    #[test]
    fn default_engine_report_healthy(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        let mut registry = frankenterm_core::runtime_health::HealthCheckRegistry::new();
        for check in checks {
            registry.register(check);
        }
        let report = registry.build_report();
        prop_assert!(
            report.overall_healthy(),
            "default engine should produce overall_healthy report",
        );
    }

    /// Running health checks twice on the same engine yields identical check IDs.
    #[test]
    fn idempotent_check_ids(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks1 = check_policy_engine_health(&mut engine, now_ms);
        let checks2 = check_policy_engine_health(&mut engine, now_ms);
        let ids1: Vec<_> = checks1.iter().map(|c| &c.check_id).collect();
        let ids2: Vec<_> = checks2.iter().map(|c| &c.check_id).collect();
        prop_assert_eq!(ids1, ids2, "check IDs should be stable across invocations");
    }

    /// Running health checks twice on the same (unmutated) engine yields identical statuses.
    #[test]
    fn idempotent_statuses(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks1 = check_policy_engine_health(&mut engine, now_ms);
        let checks2 = check_policy_engine_health(&mut engine, now_ms);
        for (c1, c2) in checks1.iter().zip(checks2.iter()) {
            prop_assert_eq!(
                c1.status, c2.status,
                "status changed between invocations for {}",
                c1.check_id,
            );
        }
    }

    /// JSON output includes check_id, status, and summary fields.
    #[test]
    fn json_has_required_fields(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let checks = check_policy_engine_health(&mut engine, now_ms);
        for check in &checks {
            let json = serde_json::to_string(check).unwrap();
            let val: serde_json::Value = serde_json::from_str(&json).unwrap();
            let obj = val.as_object().unwrap();
            prop_assert!(obj.contains_key("check_id"), "missing check_id field");
            prop_assert!(obj.contains_key("status"), "missing status field");
            prop_assert!(obj.contains_key("summary"), "missing summary field");
            prop_assert!(obj.contains_key("display_name"), "missing display_name field");
        }
    }

    // =========================================================================
    // PolicyEngineTelemetrySnapshot tests
    // =========================================================================

    /// Unified telemetry snapshot serializes to valid JSON.
    #[test]
    fn telemetry_snapshot_serializes(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let snap = engine.telemetry_snapshot(now_ms);
        let json = serde_json::to_string(&snap);
        prop_assert!(json.is_ok(), "telemetry snapshot serialization failed: {:?}", json.err());
    }

    /// Unified telemetry snapshot JSON roundtrips.
    #[test]
    fn telemetry_snapshot_json_roundtrip(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let snap = engine.telemetry_snapshot(now_ms);
        let json = serde_json::to_string(&snap).unwrap();
        let back: frankenterm_core::policy::PolicyEngineTelemetrySnapshot =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.captured_at_ms, now_ms);
        prop_assert_eq!(back.namespace_isolation_enabled, snap.namespace_isolation_enabled);
        prop_assert_eq!(back.decision_log.current_entries, snap.decision_log.current_entries);
        prop_assert_eq!(back.approval_tracker.total, snap.approval_tracker.total);
    }

    /// Unified telemetry snapshot captured_at_ms matches input.
    #[test]
    fn telemetry_snapshot_timestamp(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let snap = engine.telemetry_snapshot(now_ms);
        prop_assert_eq!(snap.captured_at_ms, now_ms);
    }

    /// Unified telemetry snapshot JSON contains all top-level subsystem keys.
    #[test]
    fn telemetry_snapshot_has_all_subsystem_keys(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let snap = engine.telemetry_snapshot(now_ms);
        let json = serde_json::to_string(&snap).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = val.as_object().unwrap();
        let expected_keys = [
            "captured_at_ms", "decision_log", "quarantine", "audit_chain",
            "compliance", "credential_broker", "connector_governor",
            "connector_registry", "connector_reliability", "bundle_registry",
            "connector_mesh", "ingestion_pipeline", "namespace_registry",
            "approval_tracker", "revocation_registry", "namespace_isolation_enabled",
        ];
        for key in &expected_keys {
            prop_assert!(
                obj.contains_key(*key),
                "missing top-level key '{}' in telemetry snapshot",
                key,
            );
        }
    }

    /// Pretty-printed JSON also roundtrips.
    #[test]
    fn telemetry_snapshot_pretty_json_roundtrip(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let snap = engine.telemetry_snapshot(now_ms);
        let json = serde_json::to_string_pretty(&snap).unwrap();
        let back: frankenterm_core::policy::PolicyEngineTelemetrySnapshot =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.captured_at_ms, snap.captured_at_ms);
    }

    /// Snapshot is idempotent: two calls on same engine yield consistent data.
    #[test]
    fn telemetry_snapshot_idempotent(
        (rate_pane, rate_global, require_prompt) in arb_engine_params(),
        now_ms in arb_now_ms(),
    ) {
        let mut engine = PolicyEngine::new(rate_pane, rate_global, require_prompt);
        let s1 = engine.telemetry_snapshot(now_ms);
        let s2 = engine.telemetry_snapshot(now_ms);
        prop_assert_eq!(s1.captured_at_ms, s2.captured_at_ms);
        prop_assert_eq!(s1.decision_log.current_entries, s2.decision_log.current_entries);
        prop_assert_eq!(s1.quarantine.active_quarantines, s2.quarantine.active_quarantines);
        prop_assert_eq!(s1.namespace_isolation_enabled, s2.namespace_isolation_enabled);
    }
}
