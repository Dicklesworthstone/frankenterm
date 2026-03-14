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

// =============================================================================
// Serde roundtrip tests for 19 uncovered types (PinkForge session 16)
// =============================================================================

fn arb_ce_str() -> impl Strategy<Value = String> {
    "[a-z0-9_]{1,15}"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn ce_s01_go_no_go_verdict_serde(decision in arb_go_no_go_decision(), rationale in "[a-z ]{5,30}") {
        let val = GoNoGoVerdict {
            decision, rationale: rationale.clone(),
            checklist: GoNoGoChecklist { checks: vec![] },
            migration_id: "mig-1".to_string(), evaluated_at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&val).unwrap();
        let back: GoNoGoVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.rationale, &rationale);
        prop_assert_eq!(&back.migration_id, "mig-1");
    }

    #[test]
    fn ce_s02_prerequisite_gate_serde(key in arb_ce_str(), closed in proptest::bool::ANY) {
        let mut gate = PrerequisiteGate::new();
        gate.require(key.clone(), "test prereq");
        if closed {
            gate.mark_closed(&key);
        }
        let json = serde_json::to_string(&gate).unwrap();
        let back: PrerequisiteGate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_count(), 1);
        if closed {
            prop_assert!(back.all_closed());
            prop_assert_eq!(back.closed_count(), 1);
        } else {
            prop_assert!(!back.all_closed());
            prop_assert_eq!(back.closed_count(), 0);
        }
    }

    #[test]
    fn ce_s03_prerequisite_entry_serde(desc in "[a-z ]{5,30}", closed in proptest::bool::ANY) {
        let entry = PrerequisiteEntry { description: desc.clone(), closed };
        let json = serde_json::to_string(&entry).unwrap();
        let back: PrerequisiteEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.description, &desc);
        prop_assert_eq!(back.closed, closed);
    }

    #[test]
    fn ce_s04_regression_guard_serde(gid in arb_ce_str(), passed in proptest::bool::ANY) {
        let guard = RegressionGuard {
            guard_id: gid.clone(), description: "test guard".to_string(),
            category: GuardCategory::CompileTime, passed,
            evidence: "ok".to_string(), command: "cargo test".to_string(),
        };
        let json = serde_json::to_string(&guard).unwrap();
        let back: RegressionGuard = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.guard_id, &gid);
        prop_assert_eq!(back.passed, passed);
    }

    #[test]
    fn ce_s05_persistence_proof_suite_serde(pid in arb_ce_str()) {
        let suite = PersistenceProofSuite {
            proofs: vec![PersistenceProof {
                proof_id: pid.clone(), workflow: "test-wf".to_string(),
                description: "test proof".to_string(), verified: true,
                seed: Some(42), state_hash_before: Some("abc".to_string()),
                state_hash_after: Some("def".to_string()),
                evidence: "matched".to_string(), command: "ft test".to_string(),
            }],
        };
        let json = serde_json::to_string(&suite).unwrap();
        let back: PersistenceProofSuite = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.proofs.len(), 1);
        prop_assert_eq!(&back.proofs[0].proof_id, &pid);
    }

    #[test]
    fn ce_s06_persistence_proof_serde(pid in arb_ce_str(), verified in proptest::bool::ANY) {
        let proof = PersistenceProof {
            proof_id: pid.clone(), workflow: "wf".to_string(),
            description: "desc".to_string(), verified,
            seed: None, state_hash_before: None, state_hash_after: None,
            evidence: "evidence".to_string(), command: "cmd".to_string(),
        };
        let json = serde_json::to_string(&proof).unwrap();
        let back: PersistenceProof = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.proof_id, &pid);
        prop_assert_eq!(back.verified, verified);
    }

    #[test]
    fn ce_s07_test_suite_result_serde(name in arb_ce_str(), passed in 0u64..100, failed in 0u64..100) {
        let result = TestSuiteResult {
            suite_name: name.clone(), passed, failed, skipped: 0,
            duration_ms: 5000, seed: Some(42), command: "cargo test".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: TestSuiteResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.suite_name, &name);
        prop_assert_eq!(back.passed, passed);
        prop_assert_eq!(back.failed, failed);
    }

    #[test]
    fn ce_s08_benchmark_summary_serde(threshold in 0.0f64..1.0) {
        let summary = BenchmarkSummary {
            comparisons: vec![BenchmarkComparison {
                name: "latency".to_string(), metric: "p95".to_string(),
                before: 10.0, after: 12.0, unit: "ms".to_string(), lower_is_better: true,
            }],
            regression_threshold: threshold,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: BenchmarkSummary = serde_json::from_str(&json).unwrap();
        prop_assert!((back.regression_threshold - threshold).abs() < 1e-10);
        prop_assert_eq!(back.comparisons.len(), 1);
    }

    #[test]
    fn ce_s09_benchmark_comparison_serde(name in arb_ce_str(), before in 0.0f64..1000.0) {
        let comp = BenchmarkComparison {
            name: name.clone(), metric: "throughput".to_string(),
            before, after: before * 1.1, unit: "ops/s".to_string(), lower_is_better: false,
        };
        let json = serde_json::to_string(&comp).unwrap();
        let back: BenchmarkComparison = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert!((back.before - before).abs() < 1e-10);
    }

    #[test]
    fn ce_s10_incident_registry_serde(iid in arb_ce_str(), priority in 1u8..5) {
        let registry = IncidentRegistry {
            incidents: vec![IncidentRecord {
                incident_id: iid.clone(), priority, title: "test".to_string(),
                description: "test incident".to_string(), status: IncidentStatus::Open,
                root_cause: None, remediation: None,
                reported_at_ms: 1_700_000_000_000, resolved_at_ms: None,
                related_beads: vec![],
            }],
        };
        let json = serde_json::to_string(&registry).unwrap();
        let back: IncidentRegistry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.incidents.len(), 1);
        prop_assert_eq!(&back.incidents[0].incident_id, &iid);
        prop_assert_eq!(back.incidents[0].priority, priority);
    }

    #[test]
    fn ce_s11_incident_record_serde(iid in arb_ce_str(), status in arb_incident_status()) {
        let rec = IncidentRecord {
            incident_id: iid.clone(), priority: 2, title: "test".to_string(),
            description: "desc".to_string(), status,
            root_cause: Some("root".to_string()), remediation: Some("fix".to_string()),
            reported_at_ms: 1_700_000_000_000, resolved_at_ms: Some(1_700_000_060_000),
            related_beads: vec!["b1".to_string()],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: IncidentRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.incident_id, &iid);
    }

    #[test]
    fn ce_s12_rollback_rehearsal_log_serde(rid in arb_ce_str(), success in proptest::bool::ANY) {
        let log = RollbackRehearsalLog {
            rehearsals: vec![RollbackRehearsal {
                rehearsal_id: rid.clone(), performed_at_ms: 1_700_000_000_000,
                successful: success, rollback_duration_ms: 5000,
                data_integrity_preserved: true, notes: "ok".to_string(),
                command: "ft rollback".to_string(),
            }],
        };
        let json = serde_json::to_string(&log).unwrap();
        let back: RollbackRehearsalLog = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.rehearsals.len(), 1);
        prop_assert_eq!(&back.rehearsals[0].rehearsal_id, &rid);
        prop_assert_eq!(back.rehearsals[0].successful, success);
    }

    #[test]
    fn ce_s13_rollback_rehearsal_serde(rid in arb_ce_str(), dur in 0u64..60_000) {
        let reh = RollbackRehearsal {
            rehearsal_id: rid.clone(), performed_at_ms: 1_700_000_000_000,
            successful: true, rollback_duration_ms: dur,
            data_integrity_preserved: true, notes: "clean".to_string(),
            command: "cmd".to_string(),
        };
        let json = serde_json::to_string(&reh).unwrap();
        let back: RollbackRehearsal = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.rehearsal_id, &rid);
        prop_assert_eq!(back.rollback_duration_ms, dur);
    }

    #[test]
    fn ce_s14_soak_outcome_serde(period in arb_ce_str(), conforming in proptest::bool::ANY) {
        let outcome = SoakOutcome {
            period_id: period.clone(), start_ms: 1_700_000_000_000,
            end_ms: 1_700_003_600_000, slo_conforming: conforming,
            error_rate: 0.001, p95_latency_ms: 42.5,
            incident_count: 0, rollback_triggered: false,
            notes: "clean soak".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: SoakOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.period_id, &period);
        prop_assert_eq!(back.slo_conforming, conforming);
    }

    #[test]
    fn ce_s15_risk_registry_serde(rid in arb_ce_str(), accepted in proptest::bool::ANY) {
        let registry = RiskRegistry {
            risks: vec![RiskRecord {
                risk_id: rid.clone(), severity: RiskSeverity::Medium,
                description: "test risk".to_string(), mitigation: Some("mitigate".to_string()),
                owner: Some("ops".to_string()), follow_up: None,
                accepted, related_beads: vec!["b1".to_string()],
            }],
        };
        let json = serde_json::to_string(&registry).unwrap();
        let back: RiskRegistry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.risks.len(), 1);
        prop_assert_eq!(&back.risks[0].risk_id, &rid);
        prop_assert_eq!(back.risks[0].accepted, accepted);
    }

    #[test]
    fn ce_s16_risk_record_serde(rid in arb_ce_str(), sev in arb_risk_severity()) {
        let rec = RiskRecord {
            risk_id: rid.clone(), severity: sev,
            description: "risk".to_string(), mitigation: None,
            owner: None, follow_up: None, accepted: false, related_beads: vec![],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: RiskRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.risk_id, &rid);
    }

    #[test]
    fn ce_s17_evidence_telemetry_serde(
        prereqs in 0u64..100, guards in 0u64..100,
        proofs in 0u64..50, suites in 0u64..50,
    ) {
        let tel = EvidenceTelemetry {
            prerequisite_checks: prereqs, guards_evaluated: guards,
            persistence_proofs_collected: proofs, test_suites_recorded: suites,
            benchmarks_recorded: 0, incidents_recorded: 0,
            rehearsals_recorded: 0, soak_outcomes_recorded: 0,
            risks_documented: 0, evaluations_performed: 0,
        };
        let json = serde_json::to_string(&tel).unwrap();
        let back: EvidenceTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.prerequisite_checks, prereqs);
        prop_assert_eq!(back.guards_evaluated, guards);
    }

    #[test]
    fn ce_s18_evidence_package_serde(mid in arb_ce_str(), version in 1u32..10) {
        let pkg = EvidencePackage {
            migration_id: mid.clone(), schema_version: version,
            assembled_at_ms: 1_700_000_000_000,
            prerequisites: PrerequisiteGate::new(),
            regression_guards: RegressionGuardSuite { guards: vec![] },
            persistence_proofs: PersistenceProofSuite { proofs: vec![] },
            test_gates: TestGateSummary { suites: vec![] },
            benchmarks: BenchmarkSummary { comparisons: vec![], regression_threshold: 0.1 },
            incidents: IncidentRegistry { incidents: vec![] },
            rollback_rehearsals: RollbackRehearsalLog { rehearsals: vec![] },
            soak_outcomes: vec![],
            risks: RiskRegistry { risks: vec![] },
            telemetry: EvidenceTelemetry {
                prerequisite_checks: 0, guards_evaluated: 0,
                persistence_proofs_collected: 0, test_suites_recorded: 0,
                benchmarks_recorded: 0, incidents_recorded: 0,
                rehearsals_recorded: 0, soak_outcomes_recorded: 0,
                risks_documented: 0, evaluations_performed: 0,
            },
        };
        let json = serde_json::to_string(&pkg).unwrap();
        let back: EvidencePackage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.migration_id, &mid);
        prop_assert_eq!(back.schema_version, version);
    }
}
