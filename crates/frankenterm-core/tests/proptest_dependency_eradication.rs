//! Property tests for dependency_eradication module.
//!
//! Covers serde roundtrips for all serializable types, ViolationSeverity
//! ordering, ScanReport verdict logic, RegressionGuardSet invariants,
//! SurfaceContractStatus arithmetic, and MigrationReport cutover readiness.

use frankenterm_core::dependency_eradication::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_forbidden_runtime() -> impl Strategy<Value = ForbiddenRuntime> {
    prop_oneof![
        Just(ForbiddenRuntime::Tokio),
        Just(ForbiddenRuntime::Smol),
        Just(ForbiddenRuntime::AsyncIo),
        Just(ForbiddenRuntime::AsyncExecutor),
    ]
}

fn arb_violation_severity() -> impl Strategy<Value = ViolationSeverity> {
    prop_oneof![
        Just(ViolationSeverity::Info),
        Just(ViolationSeverity::Warning),
        Just(ViolationSeverity::Error),
        Just(ViolationSeverity::Critical),
    ]
}

fn arb_scan_verdict() -> impl Strategy<Value = ScanVerdict> {
    prop_oneof![
        Just(ScanVerdict::Clean),
        Just(ScanVerdict::CleanWithNotes),
        Just(ScanVerdict::Violations),
    ]
}

fn arb_guard_type() -> impl Strategy<Value = GuardType> {
    prop_oneof![
        Just(GuardType::SourceScan),
        Just(GuardType::CargoDependency),
        Just(GuardType::FeatureFlag),
        Just(GuardType::RuntimeApi),
        Just(GuardType::BuildScript),
    ]
}

fn arb_migration_status() -> impl Strategy<Value = MigrationStatus> {
    prop_oneof![
        Just(MigrationStatus::InProgress),
        Just(MigrationStatus::PendingVerification),
        Just(MigrationStatus::Complete),
        Just(MigrationStatus::Blocked),
    ]
}

fn make_violation(
    severity: ViolationSeverity,
    runtime: ForbiddenRuntime,
    in_test: bool,
) -> ForbiddenImport {
    ForbiddenImport {
        pattern_id: "FP-test".into(),
        file_path: "src/test.rs".into(),
        line_number: 1,
        line_content: "use tokio::runtime;".into(),
        severity,
        in_test_context: in_test,
        runtime,
    }
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_forbidden_runtime(rt in arb_forbidden_runtime()) {
        let json = serde_json::to_string(&rt).unwrap();
        let back: ForbiddenRuntime = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, back);
    }

    #[test]
    fn serde_roundtrip_violation_severity(sev in arb_violation_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: ViolationSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn serde_roundtrip_scan_verdict(v in arb_scan_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: ScanVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    #[test]
    fn serde_roundtrip_guard_type(gt in arb_guard_type()) {
        let json = serde_json::to_string(&gt).unwrap();
        let back: GuardType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(gt, back);
    }

    #[test]
    fn serde_roundtrip_migration_status(st in arb_migration_status()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_forbidden_import(
        sev in arb_violation_severity(),
        rt in arb_forbidden_runtime(),
        in_test in proptest::bool::ANY,
    ) {
        let v = make_violation(sev, rt, in_test);
        let json = serde_json::to_string(&v).unwrap();
        let back: ForbiddenImport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.severity, v.severity);
        prop_assert_eq!(back.runtime, v.runtime);
        prop_assert_eq!(back.in_test_context, v.in_test_context);
    }

    #[test]
    fn serde_roundtrip_scan_report_clean(
        files in 0..10000u64,
        lines in 0..1_000_000u64,
        patterns in 0..20usize,
    ) {
        let report = ScanReport::clean(files, lines, patterns);
        let json = serde_json::to_string(&report).unwrap();
        let back: ScanReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.files_scanned, files);
        prop_assert_eq!(back.lines_scanned, lines);
        prop_assert_eq!(back.verdict, ScanVerdict::Clean);
    }

    #[test]
    fn serde_roundtrip_dependency_guard(passed in proptest::bool::ANY) {
        let guard = DependencyGuard {
            guard_id: "DG-test".into(),
            description: "test guard".into(),
            guard_type: GuardType::SourceScan,
            passed,
            evidence: "some evidence".into(),
            command: "grep test".into(),
        };
        let json = serde_json::to_string(&guard).unwrap();
        let back: DependencyGuard = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.passed, passed);
        prop_assert_eq!(back.guard_type, GuardType::SourceScan);
    }

    #[test]
    fn serde_roundtrip_surface_contract(
        keep in 0..50usize,
        replace in 0..50usize,
        retire in 0..50usize,
    ) {
        let status = SurfaceContractStatus {
            keep_count: keep,
            replace_count: replace,
            retire_count: retire,
            replaced_count: replace,
            retired_count: retire,
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: SurfaceContractStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.keep_count, keep);
        prop_assert_eq!(back.replace_count, replace);
    }

    #[test]
    fn serde_roundtrip_migration_report(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("test-migration", scan, guards);
        let json = serde_json::to_string(&report).unwrap();
        let back: MigrationReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.migration_id, "test-migration");
    }
}

// =============================================================================
// ViolationSeverity ordering
// =============================================================================

proptest! {
    #[test]
    fn severity_total_order(a in arb_violation_severity(), b in arb_violation_severity()) {
        prop_assert!(a <= b || a > b);
    }

    #[test]
    fn severity_info_is_minimum(sev in arb_violation_severity()) {
        prop_assert!(sev >= ViolationSeverity::Info);
    }

    #[test]
    fn severity_critical_is_maximum(sev in arb_violation_severity()) {
        prop_assert!(sev <= ViolationSeverity::Critical);
    }

    #[test]
    fn severity_ordering_chain(_dummy in 0..1u32) {
        prop_assert!(ViolationSeverity::Info < ViolationSeverity::Warning);
        prop_assert!(ViolationSeverity::Warning < ViolationSeverity::Error);
        prop_assert!(ViolationSeverity::Error < ViolationSeverity::Critical);
    }
}

// =============================================================================
// ForbiddenRuntime label
// =============================================================================

proptest! {
    #[test]
    fn forbidden_runtime_label_not_empty(rt in arb_forbidden_runtime()) {
        prop_assert!(!rt.label().is_empty());
    }

    #[test]
    fn forbidden_runtime_label_stable(rt in arb_forbidden_runtime()) {
        prop_assert_eq!(rt.label(), rt.label()); // idempotent
    }
}

// =============================================================================
// ScanReport verdict logic
// =============================================================================

proptest! {
    #[test]
    fn clean_report_always_clean(files in 0..10000u64, lines in 0..1_000_000u64) {
        let report = ScanReport::clean(files, lines, 9);
        prop_assert_eq!(report.verdict, ScanVerdict::Clean);
        prop_assert!(report.is_cutover_ready());
        prop_assert_eq!(report.critical_count(), 0);
        prop_assert_eq!(report.error_count(), 0);
    }

    #[test]
    fn empty_violations_is_clean(_dummy in 0..1u32) {
        let report = ScanReport::from_violations(100, 5000, 9, vec![]);
        prop_assert_eq!(report.verdict, ScanVerdict::Clean);
    }

    #[test]
    fn info_only_is_clean_with_notes(_dummy in 0..1u32) {
        let violations = vec![make_violation(
            ViolationSeverity::Info,
            ForbiddenRuntime::Tokio,
            false,
        )];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        prop_assert_eq!(report.verdict, ScanVerdict::CleanWithNotes);
        prop_assert!(report.is_cutover_ready());
    }

    #[test]
    fn test_context_only_is_clean_with_notes(sev in arb_violation_severity()) {
        let violations = vec![make_violation(sev, ForbiddenRuntime::Tokio, true)];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        // All violations are in test context → CleanWithNotes
        prop_assert_eq!(report.verdict, ScanVerdict::CleanWithNotes);
    }

    #[test]
    fn non_test_error_or_critical_is_violations(_dummy in 0..1u32) {
        let violations = vec![make_violation(
            ViolationSeverity::Critical,
            ForbiddenRuntime::Tokio,
            false,
        )];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        prop_assert_eq!(report.verdict, ScanVerdict::Violations);
        prop_assert!(!report.is_cutover_ready());
    }

    #[test]
    fn non_test_warning_is_violations(_dummy in 0..1u32) {
        let violations = vec![make_violation(
            ViolationSeverity::Warning,
            ForbiddenRuntime::Smol,
            false,
        )];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        prop_assert_eq!(report.verdict, ScanVerdict::Violations);
    }

    #[test]
    fn cutover_ready_iff_clean_or_notes(v in arb_scan_verdict()) {
        let ready = matches!(v, ScanVerdict::Clean | ScanVerdict::CleanWithNotes);
        // Construct a report with that verdict
        let violations = match v {
            ScanVerdict::Clean => vec![],
            ScanVerdict::CleanWithNotes => {
                vec![make_violation(ViolationSeverity::Info, ForbiddenRuntime::Tokio, false)]
            }
            ScanVerdict::Violations => {
                vec![make_violation(ViolationSeverity::Critical, ForbiddenRuntime::Tokio, false)]
            }
        };
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        prop_assert_eq!(report.is_cutover_ready(), ready);
    }
}

// =============================================================================
// ScanReport counting
// =============================================================================

proptest! {
    #[test]
    fn critical_count_excludes_test_context(_dummy in 0..1u32) {
        let violations = vec![
            make_violation(ViolationSeverity::Critical, ForbiddenRuntime::Tokio, false),
            make_violation(ViolationSeverity::Critical, ForbiddenRuntime::Tokio, true),
        ];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        prop_assert_eq!(report.critical_count(), 1);
    }

    #[test]
    fn error_count_excludes_test_context(_dummy in 0..1u32) {
        let violations = vec![
            make_violation(ViolationSeverity::Error, ForbiddenRuntime::Smol, false),
            make_violation(ViolationSeverity::Error, ForbiddenRuntime::Smol, true),
        ];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        prop_assert_eq!(report.error_count(), 1);
    }

    #[test]
    fn by_runtime_groups_correctly(_dummy in 0..1u32) {
        let violations = vec![
            make_violation(ViolationSeverity::Error, ForbiddenRuntime::Tokio, false),
            make_violation(ViolationSeverity::Error, ForbiddenRuntime::Tokio, false),
            make_violation(ViolationSeverity::Error, ForbiddenRuntime::Smol, false),
        ];
        let report = ScanReport::from_violations(100, 5000, 9, violations);
        let by_rt = report.by_runtime();
        prop_assert_eq!(by_rt.get("tokio").unwrap().len(), 2);
        prop_assert_eq!(by_rt.get("smol").unwrap().len(), 1);
    }
}

// =============================================================================
// RegressionGuardSet invariants
// =============================================================================

proptest! {
    #[test]
    fn empty_guard_set_not_all_pass(_dummy in 0..1u32) {
        let set = RegressionGuardSet::new();
        prop_assert!(!set.all_pass());
        prop_assert_eq!(set.total_count(), 0);
        prop_assert_eq!(set.pass_count(), 0);
    }

    #[test]
    fn all_passing_guards_all_pass(count in 1..10usize) {
        let mut set = RegressionGuardSet::new();
        for i in 0..count {
            set.add(DependencyGuard {
                guard_id: format!("DG-{i}"),
                description: "test".into(),
                guard_type: GuardType::SourceScan,
                passed: true,
                evidence: "ok".into(),
                command: "true".into(),
            });
        }
        prop_assert!(set.all_pass());
        prop_assert_eq!(set.pass_count(), count);
        prop_assert!(set.failing().is_empty());
    }

    #[test]
    fn one_failing_guard_not_all_pass(count in 1..10usize) {
        let mut set = RegressionGuardSet::new();
        for i in 0..count {
            set.add(DependencyGuard {
                guard_id: format!("DG-{i}"),
                description: "test".into(),
                guard_type: GuardType::SourceScan,
                passed: true,
                evidence: "ok".into(),
                command: "true".into(),
            });
        }
        set.add(DependencyGuard {
            guard_id: "DG-fail".into(),
            description: "failing".into(),
            guard_type: GuardType::CargoDependency,
            passed: false,
            evidence: "bad".into(),
            command: "false".into(),
        });
        prop_assert!(!set.all_pass());
        prop_assert_eq!(set.failing().len(), 1);
    }

    #[test]
    fn pass_count_plus_failing_equals_total(
        passing in 0..5usize,
        failing in 0..5usize,
    ) {
        let mut set = RegressionGuardSet::new();
        for i in 0..passing {
            set.add(DependencyGuard {
                guard_id: format!("pass-{i}"),
                description: "test".into(),
                guard_type: GuardType::SourceScan,
                passed: true,
                evidence: "ok".into(),
                command: "true".into(),
            });
        }
        for i in 0..failing {
            set.add(DependencyGuard {
                guard_id: format!("fail-{i}"),
                description: "test".into(),
                guard_type: GuardType::RuntimeApi,
                passed: false,
                evidence: "bad".into(),
                command: "false".into(),
            });
        }
        prop_assert_eq!(set.pass_count() + set.failing().len(), set.total_count());
    }

    #[test]
    fn standard_guards_from_clean_scan_all_pass(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        prop_assert!(guards.all_pass());
        prop_assert_eq!(guards.total_count(), 4);
    }
}

// =============================================================================
// SurfaceContractStatus arithmetic
// =============================================================================

proptest! {
    #[test]
    fn total_count_formula(keep in 0..50usize, replace in 0..50usize, retire in 0..50usize) {
        let status = SurfaceContractStatus {
            keep_count: keep,
            replace_count: replace,
            retire_count: retire,
            replaced_count: 0,
            retired_count: 0,
        };
        prop_assert_eq!(status.total_count(), keep + replace + retire);
    }

    #[test]
    fn all_transitional_resolved_when_matched(
        replace in 0..50usize,
        retire in 0..50usize,
    ) {
        let status = SurfaceContractStatus {
            keep_count: 10,
            replace_count: replace,
            retire_count: retire,
            replaced_count: replace,
            retired_count: retire,
        };
        prop_assert!(status.all_transitional_resolved());
        prop_assert_eq!(status.remaining_transitional(), 0);
    }

    #[test]
    fn not_resolved_when_replace_pending(
        replace in 1..50usize,
        done in 0..50usize,
    ) {
        let replaced = done.min(replace - 1); // ensure at least 1 pending
        let status = SurfaceContractStatus {
            keep_count: 10,
            replace_count: replace,
            retire_count: 0,
            replaced_count: replaced,
            retired_count: 0,
        };
        prop_assert!(!status.all_transitional_resolved());
        prop_assert!(status.remaining_transitional() > 0);
    }

    #[test]
    fn remaining_transitional_formula(
        replace in 0..50usize,
        replaced in 0..50usize,
        retire in 0..50usize,
        retired in 0..50usize,
    ) {
        let status = SurfaceContractStatus {
            keep_count: 0,
            replace_count: replace,
            retire_count: retire,
            replaced_count: replaced,
            retired_count: retired,
        };
        let expected = replace.saturating_sub(replaced) + retire.saturating_sub(retired);
        prop_assert_eq!(status.remaining_transitional(), expected);
    }
}

// =============================================================================
// MigrationReport cutover readiness
// =============================================================================

proptest! {
    #[test]
    fn clean_migration_is_pending_verification(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("test", scan, guards);
        prop_assert_eq!(report.status, MigrationStatus::PendingVerification);
        prop_assert!(report.is_cutover_ready());
    }

    #[test]
    fn dirty_scan_is_in_progress(_dummy in 0..1u32) {
        let violations = vec![make_violation(
            ViolationSeverity::Critical,
            ForbiddenRuntime::Tokio,
            false,
        )];
        let scan = ScanReport::from_violations(100, 5000, 9, violations);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("test", scan, guards);
        prop_assert_eq!(report.status, MigrationStatus::InProgress);
        prop_assert!(!report.is_cutover_ready());
    }

    #[test]
    fn unmitigated_error_risk_blocks_cutover(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("test", scan, guards);
        report.add_risk(ResidualRisk {
            risk_id: "R-1".into(),
            description: "test risk".into(),
            severity: ViolationSeverity::Error,
            mitigation: None,
            owner: None,
            accepted: false,
        });
        prop_assert!(!report.is_cutover_ready());
    }

    #[test]
    fn accepted_error_risk_allows_cutover(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("test", scan, guards);
        report.add_risk(ResidualRisk {
            risk_id: "R-1".into(),
            description: "test risk".into(),
            severity: ViolationSeverity::Error,
            mitigation: None,
            owner: None,
            accepted: true,
        });
        prop_assert!(report.is_cutover_ready());
    }

    #[test]
    fn mitigated_error_risk_allows_cutover(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("test", scan, guards);
        report.add_risk(ResidualRisk {
            risk_id: "R-1".into(),
            description: "test risk".into(),
            severity: ViolationSeverity::Critical,
            mitigation: Some("monitoring added".into()),
            owner: Some("team".into()),
            accepted: false,
        });
        prop_assert!(report.is_cutover_ready());
    }

    #[test]
    fn warning_risk_doesnt_block_cutover(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("test", scan, guards);
        report.add_risk(ResidualRisk {
            risk_id: "R-low".into(),
            description: "minor concern".into(),
            severity: ViolationSeverity::Warning,
            mitigation: None,
            owner: None,
            accepted: false,
        });
        prop_assert!(report.is_cutover_ready());
    }

    #[test]
    fn mark_complete_changes_status(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("test", scan, guards);
        prop_assert_eq!(report.status, MigrationStatus::PendingVerification);
        report.mark_complete();
        prop_assert_eq!(report.status, MigrationStatus::Complete);
    }

    #[test]
    fn render_summary_not_empty(_dummy in 0..1u32) {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("test", scan, guards);
        let summary = report.render_summary();
        prop_assert!(!summary.is_empty());
        prop_assert!(summary.contains("Migration Report"));
    }
}

// =============================================================================
// Standard forbidden patterns
// =============================================================================

#[test]
fn standard_patterns_non_empty() {
    let patterns = standard_forbidden_patterns();
    assert!(patterns.len() >= 7);
    for p in &patterns {
        assert!(!p.pattern_id.is_empty());
        assert!(!p.pattern.is_empty());
    }
}

#[test]
fn standard_patterns_unique_ids() {
    let patterns = standard_forbidden_patterns();
    let mut ids = std::collections::HashSet::new();
    for p in &patterns {
        assert!(ids.insert(&p.pattern_id));
    }
}

#[test]
fn standard_patterns_all_have_exclude_paths() {
    let patterns = standard_forbidden_patterns();
    for p in &patterns {
        assert!(!p.exclude_paths.is_empty(), "pattern {} has no exclude paths", p.pattern_id);
    }
}
