//! Property tests for cutover_evidence module.
//!
//! Covers serde roundtrips, prerequisite gate counting, regression guard
//! pass/fail logic, test gate pass rate arithmetic, benchmark threshold
//! detection, incident registry filtering, risk severity ordering, and
//! go/no-go verdict logic.

use frankenterm_core::cutover_evidence::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_go_no_go_decision() -> impl Strategy<Value = GoNoGoDecision> {
    prop_oneof![
        Just(GoNoGoDecision::Go),
        Just(GoNoGoDecision::NoGo),
        Just(GoNoGoDecision::Conditional),
    ]
}

fn arb_guard_category() -> impl Strategy<Value = GuardCategory> {
    prop_oneof![
        Just(GuardCategory::CompileTime),
        Just(GuardCategory::Runtime),
        Just(GuardCategory::ApiContract),
        Just(GuardCategory::Determinism),
        Just(GuardCategory::Safety),
    ]
}

fn arb_check_status() -> impl Strategy<Value = CheckStatus> {
    prop_oneof![
        Just(CheckStatus::Pass),
        Just(CheckStatus::Fail),
        Just(CheckStatus::Warn),
        Just(CheckStatus::Skip),
    ]
}

fn arb_incident_status() -> impl Strategy<Value = IncidentStatus> {
    prop_oneof![
        Just(IncidentStatus::Open),
        Just(IncidentStatus::Investigating),
        Just(IncidentStatus::Resolved),
        Just(IncidentStatus::FalsePositive),
    ]
}

fn arb_risk_severity() -> impl Strategy<Value = RiskSeverity> {
    prop_oneof![
        Just(RiskSeverity::Low),
        Just(RiskSeverity::Medium),
        Just(RiskSeverity::High),
        Just(RiskSeverity::Critical),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_go_no_go(dec in arb_go_no_go_decision()) {
        let json = serde_json::to_string(&dec).unwrap();
        let back: GoNoGoDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dec, back);
    }

    #[test]
    fn serde_roundtrip_guard_category(cat in arb_guard_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: GuardCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_check_status(st in arb_check_status()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: CheckStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_incident_status(st in arb_incident_status()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: IncidentStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_risk_severity(sev in arb_risk_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: RiskSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }
}

// =============================================================================
// PrerequisiteGate invariants
// =============================================================================

proptest! {
    #[test]
    fn prerequisite_closed_le_total(
        n_total in 1..10usize,
        n_close in 0..10usize,
    ) {
        let mut gate = PrerequisiteGate::new();
        for i in 0..n_total {
            gate.require(format!("pre-{i}"), format!("description {i}"));
        }
        for i in 0..n_close.min(n_total) {
            gate.mark_closed(&format!("pre-{i}"));
        }

        prop_assert!(gate.closed_count() <= gate.total_count());
        prop_assert_eq!(gate.total_count(), n_total);

        if n_close >= n_total {
            prop_assert!(gate.all_closed());
        }
    }

    #[test]
    fn prerequisite_unclosed_complement(
        n_total in 1..8usize,
        n_close in 0..8usize,
    ) {
        let mut gate = PrerequisiteGate::new();
        for i in 0..n_total {
            gate.require(format!("pre-{i}"), format!("desc {i}"));
        }
        for i in 0..n_close.min(n_total) {
            gate.mark_closed(&format!("pre-{i}"));
        }

        let unclosed = gate.unclosed();
        let closed = gate.closed_count();
        prop_assert_eq!(closed + unclosed.len(), gate.total_count());
    }
}

// =============================================================================
// RegressionGuardSuite invariants
// =============================================================================

proptest! {
    #[test]
    fn guard_suite_pass_count_le_total(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let mut suite = RegressionGuardSuite::new();
        for i in 0..n_pass {
            suite.record(RegressionGuard {
                guard_id: format!("p-{i}"),
                description: "pass".into(),
                category: GuardCategory::CompileTime,
                passed: true,
                evidence: "ok".into(),
                command: String::new(),
            });
        }
        for i in 0..n_fail {
            suite.record(RegressionGuard {
                guard_id: format!("f-{i}"),
                description: "fail".into(),
                category: GuardCategory::Runtime,
                passed: false,
                evidence: "err".into(),
                command: String::new(),
            });
        }

        prop_assert_eq!(suite.pass_count(), n_pass);
        prop_assert_eq!(suite.total_count(), n_pass + n_fail);
        prop_assert_eq!(suite.failing().len(), n_fail);

        if n_fail == 0 {
            prop_assert!(suite.all_pass());
        } else {
            prop_assert!(!suite.all_pass());
        }
    }
}

// =============================================================================
// TestGateSummary pass rate
// =============================================================================

proptest! {
    #[test]
    fn test_gate_pass_rate_bounds(
        passed in 0..100u64,
        failed in 0..100u64,
    ) {
        let total = passed + failed;
        if total == 0 {
            return Ok(());
        }

        let mut gate = TestGateSummary::new();
        gate.record_suite(TestSuiteResult {
            suite_name: "test".into(),
            passed,
            failed,
            skipped: 0,
            duration_ms: 1000,
            seed: None,
            command: String::new(),
        });

        let rate = gate.pass_rate();
        prop_assert!(rate >= 0.0 && rate <= 1.0, "pass_rate should be in [0,1]: {}", rate);

        let expected = passed as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10);
    }

    #[test]
    fn test_gate_total_tests(
        n_suites in 1..5usize,
        passed_per in 0..50u64,
        failed_per in 0..10u64,
    ) {
        let mut gate = TestGateSummary::new();
        for i in 0..n_suites {
            gate.record_suite(TestSuiteResult {
                suite_name: format!("suite-{i}"),
                passed: passed_per,
                failed: failed_per,
                skipped: 0,
                duration_ms: 100,
                seed: None,
                command: String::new(),
            });
        }

        prop_assert_eq!(gate.total_suites(), n_suites);
        prop_assert_eq!(gate.total_tests(), n_suites as u64 * (passed_per + failed_per));
        prop_assert_eq!(gate.total_failures(), n_suites as u64 * failed_per);
    }
}

// =============================================================================
// BenchmarkSummary threshold detection
// =============================================================================

proptest! {
    #[test]
    fn benchmark_within_threshold_consistency(
        before in 1.0..100.0f64,
        after in 1.0..200.0f64,
        threshold in 0.1..2.0f64,
    ) {
        let comp = BenchmarkComparison {
            name: "test".into(),
            metric: "ops/s".into(),
            before,
            after,
            unit: "ops/s".into(),
            lower_is_better: false,
        };

        let ratio = comp.ratio();
        let within = comp.within_threshold(threshold);

        // For higher-is-better: ratio = after/before
        // Within threshold if ratio >= (1 - threshold)
        if ratio >= (1.0 - threshold) {
            prop_assert!(within, "ratio {:.4} >= {:.4} should be within threshold",
                ratio, 1.0 - threshold);
        }
    }

    #[test]
    fn benchmark_summary_regression_count(
        n_ok in 0..5usize,
        n_regressed in 0..5usize,
    ) {
        let mut summary = BenchmarkSummary::new();
        for i in 0..n_ok {
            summary.record(BenchmarkComparison {
                name: format!("ok-{i}"),
                metric: "ops/s".into(),
                before: 100.0,
                after: 100.0,
                unit: "ops/s".into(),
                lower_is_better: false,
            });
        }
        for i in 0..n_regressed {
            summary.record(BenchmarkComparison {
                name: format!("reg-{i}"),
                metric: "ops/s".into(),
                before: 100.0,
                after: 1.0, // massive regression
                unit: "ops/s".into(),
                lower_is_better: false,
            });
        }

        prop_assert_eq!(summary.total_count(), n_ok + n_regressed);
        prop_assert!(summary.within_threshold_count() <= summary.total_count());

        if n_regressed == 0 {
            prop_assert!(summary.all_within_threshold());
        }
    }
}

// =============================================================================
// IncidentRegistry filtering
// =============================================================================

proptest! {
    #[test]
    fn incident_unresolved_le_total(
        n_resolved in 0..5usize,
        n_unresolved in 0..5usize,
    ) {
        let mut registry = IncidentRegistry::new();
        for i in 0..n_resolved {
            registry.record(IncidentRecord {
                incident_id: format!("r-{i}"),
                priority: 2,
                title: "resolved".into(),
                description: String::new(),
                status: IncidentStatus::Resolved,
                root_cause: None,
                remediation: None,
                reported_at_ms: 0,
                resolved_at_ms: Some(1000),
                related_beads: Vec::new(),
            });
        }
        for i in 0..n_unresolved {
            registry.record(IncidentRecord {
                incident_id: format!("u-{i}"),
                priority: 1,
                title: "open".into(),
                description: String::new(),
                status: IncidentStatus::Open,
                root_cause: None,
                remediation: None,
                reported_at_ms: 0,
                resolved_at_ms: None,
                related_beads: Vec::new(),
            });
        }

        prop_assert!(registry.unresolved_count() <= n_resolved + n_unresolved);
        prop_assert_eq!(registry.resolved().len(), n_resolved);
    }

    #[test]
    fn incident_p1_count_subset(
        n_p1 in 0..3usize,
        n_p2 in 0..3usize,
    ) {
        let mut registry = IncidentRegistry::new();
        for i in 0..n_p1 {
            registry.record(IncidentRecord {
                incident_id: format!("p1-{i}"),
                priority: 1,
                title: "p1".into(),
                description: String::new(),
                status: IncidentStatus::Open,
                root_cause: None,
                remediation: None,
                reported_at_ms: 0,
                resolved_at_ms: None,
                related_beads: Vec::new(),
            });
        }
        for i in 0..n_p2 {
            registry.record(IncidentRecord {
                incident_id: format!("p2-{i}"),
                priority: 2,
                title: "p2".into(),
                description: String::new(),
                status: IncidentStatus::Open,
                root_cause: None,
                remediation: None,
                reported_at_ms: 0,
                resolved_at_ms: None,
                related_beads: Vec::new(),
            });
        }

        prop_assert!(registry.unresolved_p1_count() <= registry.unresolved_count());
        prop_assert_eq!(registry.unresolved_p1_count(), n_p1);
    }
}

// =============================================================================
// RiskRegistry invariants
// =============================================================================

proptest! {
    #[test]
    fn risk_critical_mitigated_logic(
        n_mitigated in 0..3usize,
        n_unmitigated in 0..3usize,
    ) {
        let mut registry = RiskRegistry::new();
        for i in 0..n_mitigated {
            registry.record(RiskRecord {
                risk_id: format!("m-{i}"),
                severity: RiskSeverity::Critical,
                description: "mitigated critical".into(),
                mitigation: Some("fix applied".into()),
                owner: None,
                follow_up: None,
                accepted: false,
                related_beads: Vec::new(),
            });
        }
        for i in 0..n_unmitigated {
            registry.record(RiskRecord {
                risk_id: format!("u-{i}"),
                severity: RiskSeverity::Critical,
                description: "unmitigated critical".into(),
                mitigation: None,
                owner: None,
                follow_up: None,
                accepted: false,
                related_beads: Vec::new(),
            });
        }

        prop_assert_eq!(registry.total_count(), n_mitigated + n_unmitigated);
        prop_assert_eq!(registry.unmitigated_critical_count(), n_unmitigated);

        if n_unmitigated == 0 {
            prop_assert!(registry.all_critical_mitigated());
        } else {
            prop_assert!(!registry.all_critical_mitigated());
        }
    }

    #[test]
    fn risk_severity_ordering(a in arb_risk_severity(), b in arb_risk_severity()) {
        // PartialOrd should be consistent
        if a <= b && b <= a {
            prop_assert_eq!(a, b);
        }
    }
}

// =============================================================================
// RollbackRehearsalLog
// =============================================================================

proptest! {
    #[test]
    fn rollback_log_success_count(
        n_success in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let mut log = RollbackRehearsalLog::new();
        for i in 0..n_success {
            log.record(RollbackRehearsal {
                rehearsal_id: format!("s-{i}"),
                performed_at_ms: 0,
                successful: true,
                rollback_duration_ms: 1000,
                data_integrity_preserved: true,
                notes: String::new(),
                command: String::new(),
            });
        }
        for i in 0..n_fail {
            log.record(RollbackRehearsal {
                rehearsal_id: format!("f-{i}"),
                performed_at_ms: 0,
                successful: false,
                rollback_duration_ms: 5000,
                data_integrity_preserved: false,
                notes: String::new(),
                command: String::new(),
            });
        }

        prop_assert_eq!(log.success_count(), n_success);
        prop_assert_eq!(log.total_count(), n_success + n_fail);

        if n_success > 0 {
            prop_assert!(log.has_successful_rehearsal());
        } else {
            prop_assert!(!log.has_successful_rehearsal());
        }
    }
}

// =============================================================================
// GoNoGoChecklist
// =============================================================================

proptest! {
    #[test]
    fn checklist_counts_sum_correctly(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
        n_warn in 0..3usize,
    ) {
        let mut checklist = GoNoGoChecklist::new();
        for i in 0..n_pass {
            checklist.add_check(ChecklistItem {
                gate_id: format!("p-{i}"),
                description: "pass".into(),
                status: CheckStatus::Pass,
                detail: String::new(),
                blocking: true,
            });
        }
        for i in 0..n_fail {
            checklist.add_check(ChecklistItem {
                gate_id: format!("f-{i}"),
                description: "fail".into(),
                status: CheckStatus::Fail,
                detail: String::new(),
                blocking: true,
            });
        }
        for i in 0..n_warn {
            checklist.add_check(ChecklistItem {
                gate_id: format!("w-{i}"),
                description: "warn".into(),
                status: CheckStatus::Warn,
                detail: String::new(),
                blocking: false,
            });
        }

        prop_assert_eq!(checklist.pass_count(), n_pass);
        prop_assert_eq!(checklist.failure_count(), n_fail);
        prop_assert_eq!(checklist.warning_count(), n_warn);
        prop_assert_eq!(checklist.blocking_failure_count(), n_fail);

        if n_fail == 0 && n_warn == 0 {
            prop_assert!(checklist.all_pass());
        }
    }
}

// =============================================================================
// EvidencePackage summary
// =============================================================================

#[test]
fn empty_evidence_package_evaluates() {
    let pkg = EvidencePackage::new("test", 1);
    let verdict = pkg.evaluate();
    // Empty package should produce some verdict
    assert!(!verdict.rationale.is_empty());
}

#[test]
fn evidence_summary_serializes() {
    let pkg = EvidencePackage::new("test", 1);
    let summary = pkg.summary();
    let json = serde_json::to_string(&summary).unwrap();
    let back: EvidenceSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(back.migration_id, "test");
}
