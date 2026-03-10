//! Property-based tests for the policy_compliance module.
//!
//! Tests serde roundtrips for ComplianceConfig, ComplianceStatus, ViolationSeverity,
//! TrendDirection, ComplianceViolation, ComplianceSnapshot, ComplianceCounters,
//! and behavioral invariants of the compliance engine.

use frankenterm_core::policy_compliance::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_compliance_config() -> impl Strategy<Value = ComplianceConfig> {
    (1..5000usize, 1000..10_000_000u64).prop_map(|(max, sla)| ComplianceConfig {
        max_violations: max,
        sla_threshold_ms: sla,
    })
}

fn arb_compliance_status() -> impl Strategy<Value = ComplianceStatus> {
    prop_oneof![
        Just(ComplianceStatus::Compliant),
        Just(ComplianceStatus::Advisory),
        Just(ComplianceStatus::NonCompliant),
        Just(ComplianceStatus::Critical),
    ]
}

fn arb_violation_severity() -> impl Strategy<Value = ViolationSeverity> {
    prop_oneof![
        Just(ViolationSeverity::Info),
        Just(ViolationSeverity::Low),
        Just(ViolationSeverity::Medium),
        Just(ViolationSeverity::High),
        Just(ViolationSeverity::Critical),
    ]
}

fn arb_trend_direction() -> impl Strategy<Value = TrendDirection> {
    prop_oneof![
        Just(TrendDirection::Improving),
        Just(TrendDirection::Stable),
        Just(TrendDirection::Degrading),
    ]
}

fn arb_compliance_violation() -> impl Strategy<Value = ComplianceViolation> {
    (
        "[a-z0-9]{8}",
        1..1_000_000u64,
        "[a-z.]{5,20}",
        "[a-z]{3,10}",
        "[a-z]{3,10}",
        arb_violation_severity(),
        "[a-z ]{5,30}",
        any::<bool>(),
    )
        .prop_map(
            |(id, ts, rule_id, surface, actor, severity, desc, remediated)| ComplianceViolation {
                violation_id: id,
                detected_at_ms: ts,
                rule_id,
                surface,
                actor_id: actor,
                severity,
                description: desc,
                remediated,
                remediated_at_ms: if remediated { Some(ts + 1000) } else { None },
                remediated_by: if remediated {
                    Some("admin".to_string())
                } else {
                    None
                },
            },
        )
}

fn arb_compliance_counters() -> impl Strategy<Value = ComplianceCounters> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(evals, denials, detected, remediated, quarantines, trips, forensic, snaps)| {
                ComplianceCounters {
                    total_evaluations: evals,
                    total_denials: denials,
                    total_violations_detected: detected,
                    total_violations_remediated: remediated,
                    total_quarantines: quarantines,
                    total_kill_switch_trips: trips,
                    total_forensic_records: forensic,
                    snapshots_generated: snaps,
                }
            },
        )
}

fn arb_violation_trend() -> impl Strategy<Value = ViolationTrend> {
    (
        0..100u32,
        0..100u32,
        0..100u32,
        0..100u32,
        arb_trend_direction(),
    )
        .prop_map(
            |(violations, remediations, new_v, carried, direction)| ViolationTrend {
                violations_in_period: violations,
                remediations_in_period: remediations,
                new_violations: new_v,
                carried_over: carried,
                direction,
            },
        )
}

fn arb_remediation_summary() -> impl Strategy<Value = RemediationSummary> {
    (0..100u32, any::<u64>(), any::<u64>(), 0..50u32).prop_map(
        |(completed, avg, oldest, past_sla)| RemediationSummary {
            completed,
            avg_time_to_remediate_ms: avg,
            oldest_open_violation_age_ms: oldest,
            past_sla_count: past_sla,
        },
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn compliance_config_json_roundtrip(config in arb_compliance_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ComplianceConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn compliance_status_json_roundtrip(status in arb_compliance_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: ComplianceStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn violation_severity_json_roundtrip(severity in arb_violation_severity()) {
        let json = serde_json::to_string(&severity).unwrap();
        let back: ViolationSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(severity, back);
    }

    #[test]
    fn trend_direction_json_roundtrip(direction in arb_trend_direction()) {
        let json = serde_json::to_string(&direction).unwrap();
        let back: TrendDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(direction, back);
    }

    #[test]
    fn compliance_violation_json_roundtrip(violation in arb_compliance_violation()) {
        let json = serde_json::to_string(&violation).unwrap();
        let back: ComplianceViolation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(violation, back);
    }

    #[test]
    fn compliance_counters_json_roundtrip(counters in arb_compliance_counters()) {
        let json = serde_json::to_string(&counters).unwrap();
        let back: ComplianceCounters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(counters, back);
    }

    #[test]
    fn violation_trend_json_roundtrip(trend in arb_violation_trend()) {
        let json = serde_json::to_string(&trend).unwrap();
        let back: ViolationTrend = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(trend, back);
    }

    #[test]
    fn remediation_summary_json_roundtrip(summary in arb_remediation_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let back: RemediationSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(summary, back);
    }
}

// =============================================================================
// Behavioral invariants
// =============================================================================

proptest! {
    #[test]
    fn config_default_deserialization_valid(json_subset in prop_oneof![
        Just("{}".to_string()),
        Just(r#"{"max_violations":100}"#.to_string()),
        Just(r#"{"sla_threshold_ms":5000}"#.to_string()),
    ]) {
        let config: ComplianceConfig = serde_json::from_str(&json_subset).unwrap();
        // Deserialized config should have reasonable values
        prop_assert!(config.max_violations <= 10_000);
        prop_assert!(config.sla_threshold_ms <= 86_400_000);
    }

    #[test]
    fn from_config_creates_compliant_engine(config in arb_compliance_config()) {
        let engine = ComplianceEngine::from_config(&config);
        prop_assert_eq!(engine.compute_status(), ComplianceStatus::Compliant);
        prop_assert_eq!(engine.active_violation_count(), 0);
    }

    #[test]
    fn evaluation_counting_consistent(allows in 0..50u32, denials in 0..50u32) {
        let mut engine = ComplianceEngine::new(100, 3_600_000);
        for _ in 0..allows {
            engine.record_evaluation(false);
        }
        for _ in 0..denials {
            engine.record_evaluation(true);
        }
        let counters = engine.counters();
        prop_assert_eq!(counters.total_evaluations, u64::from(allows + denials));
        prop_assert_eq!(counters.total_denials, u64::from(denials));
    }

    #[test]
    fn severity_ordering_consistent(a in arb_violation_severity(), b in arb_violation_severity()) {
        // Verify transitivity: if a <= b and b <= a, then a == b
        if a <= b && b <= a {
            prop_assert_eq!(a, b);
        }
    }

    #[test]
    fn status_ordering_consistent(a in arb_compliance_status(), b in arb_compliance_status()) {
        if a <= b && b <= a {
            prop_assert_eq!(a, b);
        }
    }

    #[test]
    fn active_count_never_exceeds_total(n_violations in 0..20usize, n_remediate in 0..10usize) {
        let mut engine = ComplianceEngine::new(100, 3_600_000);
        for i in 0..n_violations {
            engine.record_violation(ComplianceViolation {
                violation_id: format!("v{i}"),
                detected_at_ms: (i as u64) * 1000,
                rule_id: "rule-1".to_string(),
                surface: "policy".to_string(),
                actor_id: "actor-1".to_string(),
                severity: ViolationSeverity::Medium,
                description: "test".to_string(),
                remediated: false,
                remediated_at_ms: None,
                remediated_by: None,
            });
        }
        // Remediate some
        for i in 0..n_remediate.min(n_violations) {
            engine.remediate(&format!("v{i}"), "admin", 100_000);
        }
        prop_assert!(engine.active_violation_count() <= n_violations);
    }
}
