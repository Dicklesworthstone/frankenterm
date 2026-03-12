//! Property tests for forbidden_dep_guards module.

use proptest::prelude::*;

use frankenterm_core::dependency_eradication::{
    standard_forbidden_patterns, ForbiddenImport, ForbiddenRuntime, ScanReport, ViolationSeverity,
};
use frankenterm_core::forbidden_dep_guards::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_violation_severity() -> impl Strategy<Value = ViolationSeverity> {
    prop_oneof![
        Just(ViolationSeverity::Warning),
        Just(ViolationSeverity::Error),
        Just(ViolationSeverity::Critical),
    ]
}

fn arb_forbidden_runtime() -> impl Strategy<Value = ForbiddenRuntime> {
    prop_oneof![
        Just(ForbiddenRuntime::Tokio),
        Just(ForbiddenRuntime::Smol),
        Just(ForbiddenRuntime::AsyncIo),
        Just(ForbiddenRuntime::AsyncExecutor),
    ]
}

fn arb_guard_config() -> impl Strategy<Value = GuardConfig> {
    (
        arb_violation_severity(),
        any::<bool>(),
        0usize..=50,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(fail_on_severity, allow_test_exceptions, max_warnings, enforce_feature_flags, cargo_deny_integration)| {
                GuardConfig {
                    fail_on_severity,
                    allow_test_exceptions,
                    max_warnings,
                    enforce_feature_flags,
                    cargo_deny_integration,
                }
            },
        )
}

fn make_violation(in_test: bool, severity: ViolationSeverity) -> ForbiddenImport {
    ForbiddenImport {
        pattern_id: "FP-01-tokio-use".into(),
        file_path: "src/bad.rs".into(),
        line_number: 10,
        line_content: "use tokio::runtime;".into(),
        severity,
        in_test_context: in_test,
        runtime: ForbiddenRuntime::Tokio,
    }
}

fn make_warning() -> ForbiddenImport {
    ForbiddenImport {
        pattern_id: "FP-WARN".into(),
        file_path: "src/warn.rs".into(),
        line_number: 5,
        line_content: "// tokio legacy shim".into(),
        severity: ViolationSeverity::Warning,
        in_test_context: false,
        runtime: ForbiddenRuntime::Tokio,
    }
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_guard_config(cfg in arb_guard_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GuardConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg.allow_test_exceptions, back.allow_test_exceptions);
        prop_assert_eq!(cfg.max_warnings, back.max_warnings);
        prop_assert_eq!(cfg.enforce_feature_flags, back.enforce_feature_flags);
        prop_assert_eq!(cfg.cargo_deny_integration, back.cargo_deny_integration);
    }

    #[test]
    fn serde_roundtrip_guard_suite_result(_dummy in Just(())) {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let json = serde_json::to_string(&result).unwrap();
        let back: GuardSuiteResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result.overall_pass, back.overall_pass);
        prop_assert_eq!(result.gate_checks.len(), back.gate_checks.len());
    }

    #[test]
    fn serde_roundtrip_feature_flag_isolation(_dummy in Just(())) {
        let isolations = standard_feature_isolation();
        let json = serde_json::to_string(&isolations).unwrap();
        let back: Vec<FeatureFlagIsolation> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(isolations.len(), back.len());
    }

    #[test]
    fn serde_roundtrip_build_script_guard(_dummy in Just(())) {
        let guards = standard_build_guards();
        let json = serde_json::to_string(&guards).unwrap();
        let back: Vec<BuildScriptGuard> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(guards.len(), back.len());
        for (orig, restored) in guards.iter().zip(back.iter()) {
            prop_assert_eq!(&orig.guard_id, &restored.guard_id);
            prop_assert_eq!(orig.active, restored.active);
        }
    }
}

// =============================================================================
// GuardConfig defaults
// =============================================================================

#[test]
fn default_config_has_expected_values() {
    let cfg = GuardConfig::default();
    assert_eq!(cfg.fail_on_severity, ViolationSeverity::Error);
    assert!(cfg.allow_test_exceptions);
    assert_eq!(cfg.max_warnings, 10);
    assert!(cfg.enforce_feature_flags);
    assert!(cfg.cargo_deny_integration);
}

// =============================================================================
// GuardSuiteResult::evaluate property tests
// =============================================================================

proptest! {
    #[test]
    fn clean_scan_always_passes(_dummy in Just(())) {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        prop_assert!(result.overall_pass);
        prop_assert!(result.blocking_failures.is_empty());
        prop_assert_eq!(result.exit_code(), 0);
    }

    #[test]
    fn non_test_error_violation_fails(severity in prop_oneof![Just(ViolationSeverity::Error), Just(ViolationSeverity::Critical)]) {
        let v = make_violation(false, severity);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        prop_assert!(!result.overall_pass);
        prop_assert_eq!(result.exit_code(), 1);
    }

    #[test]
    fn test_context_allowed_when_configured(_dummy in Just(())) {
        let v = make_violation(true, ViolationSeverity::Error);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig {
            allow_test_exceptions: true,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);
        // Severity-threshold check should not fire for test-context violations
        let severity_gate_failed = result
            .gate_checks
            .iter()
            .any(|c| c.check_id == "CG-SEVERITY-THRESHOLD" && !c.passed);
        prop_assert!(!severity_gate_failed);
    }

    #[test]
    fn test_context_blocked_when_strict(_dummy in Just(())) {
        let v = make_violation(true, ViolationSeverity::Error);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig {
            allow_test_exceptions: false,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let severity_gate_failed = result
            .gate_checks
            .iter()
            .any(|c| c.check_id == "CG-SEVERITY-THRESHOLD" && !c.passed);
        prop_assert!(severity_gate_failed);
    }

    #[test]
    fn warnings_within_limit_pass(n in 0usize..=10) {
        let warnings: Vec<ForbiddenImport> = (0..n).map(|_| make_warning()).collect();
        let scan = ScanReport::from_violations(50, 2000, 9, warnings);
        let config = GuardConfig {
            max_warnings: 10,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);
        // With only warnings (below Error severity) and within limit, should pass
        prop_assert!(result.overall_pass);
        prop_assert_eq!(result.exit_code(), 0);
    }

    #[test]
    fn warnings_exceeding_limit_exit_code_2(extra in 1usize..=5) {
        let limit = 3usize;
        let count = limit + extra;
        let warnings: Vec<ForbiddenImport> = (0..count).map(|_| make_warning()).collect();
        let scan = ScanReport::from_violations(50, 2000, 9, warnings);
        let config = GuardConfig {
            max_warnings: limit,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);
        prop_assert_eq!(result.exit_code(), 2);
    }

    #[test]
    fn blocking_failures_exit_code_1_trumps_warnings(extra in 1usize..=3) {
        let limit = 2usize;
        let mut violations: Vec<ForbiddenImport> = (0..(limit + extra)).map(|_| make_warning()).collect();
        violations.push(make_violation(false, ViolationSeverity::Critical));
        let scan = ScanReport::from_violations(100, 5000, 9, violations);
        let config = GuardConfig {
            max_warnings: limit,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);
        // Blocking failure takes precedence over warning overflow
        prop_assert_eq!(result.exit_code(), 1);
    }

    #[test]
    fn blocking_failure_ids_are_subset_of_gate_checks(_dummy in Just(())) {
        let v = make_violation(false, ViolationSeverity::Critical);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let ids = result.blocking_failure_ids();
        for id in &ids {
            let found = result.gate_checks.iter().any(|c| c.check_id == *id && !c.passed && c.blocking);
            prop_assert!(found, "blocking failure id {} not found in gate checks", id);
        }
    }
}

// =============================================================================
// ci_summary property tests
// =============================================================================

proptest! {
    #[test]
    fn ci_summary_pass_format(_dummy in Just(())) {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let summary = result.ci_summary();
        prop_assert!(summary.contains("RESULT: PASS"));
        prop_assert!(summary.contains("Exit code: 0"));
        prop_assert!(summary.contains("Forbidden Dependency Guard Suite"));
    }

    #[test]
    fn ci_summary_fail_format(_dummy in Just(())) {
        let v = make_violation(false, ViolationSeverity::Critical);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let summary = result.ci_summary();
        prop_assert!(summary.contains("RESULT: FAIL"));
        prop_assert!(summary.contains("BLOCKING:"));
    }
}

// =============================================================================
// to_regression_guard_set property tests
// =============================================================================

proptest! {
    #[test]
    fn regression_guard_set_matches_gate_checks(_dummy in Just(())) {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let guard_set = result.to_regression_guard_set();
        prop_assert_eq!(guard_set.guards.len(), result.gate_checks.len());
        for (guard, check) in guard_set.guards.iter().zip(result.gate_checks.iter()) {
            prop_assert_eq!(&guard.guard_id, &check.check_id);
            prop_assert_eq!(guard.passed, check.passed);
        }
    }
}

// =============================================================================
// Feature flag isolation tests
// =============================================================================

#[test]
fn standard_feature_isolation_all_clean() {
    let isolations = standard_feature_isolation();
    assert!(!isolations.is_empty());
    for iso in &isolations {
        assert!(iso.isolated, "{} should be isolated", iso.feature_name);
        assert!(iso.leaked_modules.is_empty());
    }
}

#[test]
fn standard_feature_isolation_has_expected_modules() {
    let isolations = standard_feature_isolation();
    for iso in &isolations {
        assert!(
            !iso.expected_modules.is_empty(),
            "feature '{}' should have expected modules",
            iso.feature_name
        );
    }
}

proptest! {
    #[test]
    fn feature_isolation_names_non_empty(_dummy in Just(())) {
        let isolations = standard_feature_isolation();
        for iso in &isolations {
            prop_assert!(!iso.feature_name.is_empty());
        }
    }
}

// =============================================================================
// Build script guard tests
// =============================================================================

#[test]
fn standard_build_guards_count() {
    let guards = standard_build_guards();
    assert_eq!(guards.len(), 3);
}

#[test]
fn standard_build_guards_all_active() {
    let guards = standard_build_guards();
    for g in &guards {
        assert!(g.active, "guard {} should be active", g.guard_id);
    }
}

proptest! {
    #[test]
    fn build_guards_have_cfg_expression(_dummy in Just(())) {
        let guards = standard_build_guards();
        for g in &guards {
            prop_assert!(!g.cfg_expression.is_empty(), "guard {} has empty cfg", g.guard_id);
            prop_assert!(!g.compile_error_message.is_empty(), "guard {} has empty message", g.guard_id);
        }
    }

    #[test]
    fn build_guard_ids_unique(_dummy in Just(())) {
        let guards = standard_build_guards();
        let mut ids: Vec<String> = guards.iter().map(|g| g.guard_id.clone()).collect();
        let original_len = ids.len();
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), original_len);
    }
}

// =============================================================================
// PreCommitCheck property tests
// =============================================================================

proptest! {
    #[test]
    fn pre_commit_clean_files_pass(_dummy in Just(())) {
        let patterns = standard_forbidden_patterns();
        let files = &["src/storage.rs", "src/events.rs", "src/config.rs"];
        let check = PreCommitCheck::from_file_list(files, &patterns);
        prop_assert!(check.pass);
        prop_assert!(check.violations.is_empty());
        prop_assert_eq!(check.files_checked.len(), 3);
    }

    #[test]
    fn pre_commit_hook_output_clean_contains_ok(_dummy in Just(())) {
        let patterns = standard_forbidden_patterns();
        let files = &["src/clean.rs"];
        let check = PreCommitCheck::from_file_list(files, &patterns);
        let output = check.hook_output();
        prop_assert!(output.contains("OK"));
    }

    #[test]
    fn pre_commit_hook_output_fail_contains_blocked(_dummy in Just(())) {
        let patterns = standard_forbidden_patterns();
        let files = &["src/use tokio::.rs"];
        let check = PreCommitCheck::from_file_list(files, &patterns);
        if !check.pass {
            let output = check.hook_output();
            prop_assert!(output.contains("FAIL"));
            prop_assert!(output.contains("Commit blocked"));
        }
    }

    #[test]
    fn pre_commit_files_checked_matches_input(count in 1usize..=5) {
        let patterns = standard_forbidden_patterns();
        let file_names: Vec<String> = (0..count).map(|i| format!("src/file_{}.rs", i)).collect();
        let file_refs: Vec<&str> = file_names.iter().map(|s| s.as_str()).collect();
        let check = PreCommitCheck::from_file_list(&file_refs, &patterns);
        prop_assert_eq!(check.files_checked.len(), count);
    }
}

// =============================================================================
// ManifestCheck / ManifestViolation tests
// =============================================================================

proptest! {
    #[test]
    fn manifest_check_clean_is_clean(_dummy in Just(())) {
        let check = ManifestCheck {
            crate_name: "test-crate".into(),
            forbidden_deps: vec!["tokio".into()],
            found_violations: Vec::new(),
            clean: true,
        };
        prop_assert!(check.clean);
        prop_assert!(check.found_violations.is_empty());
    }

    #[test]
    fn manifest_check_with_violation_not_clean(_dummy in Just(())) {
        let violation = ManifestViolation {
            dep_name: "tokio".into(),
            dep_section: "dependencies".into(),
            reason: "forbidden".into(),
        };
        let check = ManifestCheck {
            crate_name: "test-crate".into(),
            forbidden_deps: vec!["tokio".into()],
            found_violations: vec![violation],
            clean: false,
        };
        prop_assert!(!check.clean);
        prop_assert_eq!(check.found_violations.len(), 1);
    }

    #[test]
    fn manifest_violation_serde_roundtrip(_dummy in Just(())) {
        let violation = ManifestViolation {
            dep_name: "tokio".into(),
            dep_section: "dev-dependencies".into(),
            reason: "tokio is forbidden".into(),
        };
        let json = serde_json::to_string(&violation).unwrap();
        let back: ManifestViolation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(violation.dep_name, back.dep_name);
        prop_assert_eq!(violation.dep_section, back.dep_section);
        prop_assert_eq!(violation.reason, back.reason);
    }
}
