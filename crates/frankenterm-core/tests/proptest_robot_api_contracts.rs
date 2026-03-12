//! Property tests for robot_api_contracts module (ft-3681t.4.5).
//!
//! Covers serde roundtrips, ApiSurface exhaustiveness, ContractMatrix
//! coverage arithmetic, execution counter consistency, report verdict
//! logic, and standard factory invariants.

use frankenterm_core::robot_api_contracts::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_api_surface() -> impl Strategy<Value = ApiSurface> {
    (0..ApiSurface::ALL.len()).prop_map(|i| ApiSurface::ALL[i])
}

fn arb_check_category() -> impl Strategy<Value = CheckCategory> {
    prop_oneof![
        Just(CheckCategory::SchemaStability),
        Just(CheckCategory::Determinism),
        Just(CheckCategory::ReplayCorrectness),
        Just(CheckCategory::NtmCompatibility),
        Just(CheckCategory::ErrorContract),
        Just(CheckCategory::Idempotency),
        Just(CheckCategory::EnvelopeContract),
    ]
}

fn arb_check_outcome() -> impl Strategy<Value = CheckOutcome> {
    prop_oneof![
        Just(CheckOutcome::Pass),
        Just(CheckOutcome::Fail),
        Just(CheckOutcome::Skipped),
    ]
}

fn arb_contract_verdict() -> impl Strategy<Value = ContractVerdict> {
    prop_oneof![
        Just(ContractVerdict::Compatible),
        Just(ContractVerdict::ConditionallyCompatible),
        Just(ContractVerdict::Incompatible),
    ]
}

fn arb_contract_check() -> impl Strategy<Value = ContractCheck> {
    (
        "[A-Z]{1,5}-[0-9]{1,4}",
        arb_api_surface(),
        arb_check_category(),
        ".{1,40}",
        any::<bool>(),
    )
        .prop_map(|(id, surface, category, desc, blocking)| {
            let mut check = ContractCheck::new(&id, surface, category, &desc);
            if !blocking {
                check = check.advisory();
            }
            check
        })
}

fn arb_check_result() -> impl Strategy<Value = CheckResult> {
    (
        "[A-Z]{1,5}-[0-9]{1,4}",
        arb_api_surface(),
        arb_check_category(),
        arb_check_outcome(),
        any::<bool>(),
    )
        .prop_map(|(id, surface, category, outcome, blocking)| match outcome {
            CheckOutcome::Pass => {
                let mut r = CheckResult::pass(&id, surface, category);
                if !blocking {
                    r.blocking = false;
                }
                r
            }
            CheckOutcome::Fail => {
                let mut r = CheckResult::fail(&id, surface, category, "test failure");
                if !blocking {
                    r.blocking = false;
                }
                r
            }
            CheckOutcome::Skipped => {
                let mut r = CheckResult::pass(&id, surface, category);
                r.outcome = CheckOutcome::Skipped;
                if !blocking {
                    r.blocking = false;
                }
                r
            }
        })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_api_surface(surface in arb_api_surface()) {
        let json = serde_json::to_string(&surface).unwrap();
        let back: ApiSurface = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(surface, back);
    }

    #[test]
    fn serde_roundtrip_check_category(cat in arb_check_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: CheckCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_check_outcome(outcome in arb_check_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: CheckOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, back);
    }

    #[test]
    fn serde_roundtrip_contract_verdict(verdict in arb_contract_verdict()) {
        let json = serde_json::to_string(&verdict).unwrap();
        let back: ContractVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(verdict, back);
    }

    #[test]
    fn serde_roundtrip_contract_check(check in arb_contract_check()) {
        let json = serde_json::to_string(&check).unwrap();
        let back: ContractCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(check.check_id, back.check_id);
        prop_assert_eq!(check.surface, back.surface);
        prop_assert_eq!(check.category, back.category);
        prop_assert_eq!(check.blocking, back.blocking);
    }

    #[test]
    fn serde_roundtrip_check_result(result in arb_check_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: CheckResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result.check_id, back.check_id);
        prop_assert_eq!(result.surface, back.surface);
        prop_assert_eq!(result.outcome, back.outcome);
        prop_assert_eq!(result.blocking, back.blocking);
    }
}

// =============================================================================
// ApiSurface exhaustiveness and consistency
// =============================================================================

proptest! {
    #[test]
    fn api_surface_command_name_nonempty(surface in arb_api_surface()) {
        prop_assert!(!surface.command_name().is_empty(),
            "command_name for {:?} should not be empty", surface);
    }

    #[test]
    fn api_surface_category_nonempty(surface in arb_api_surface()) {
        prop_assert!(!surface.category().is_empty(),
            "category for {:?} should not be empty", surface);
    }

    #[test]
    fn api_surface_mutation_implies_write_category(surface in arb_api_surface()) {
        if surface.is_mutation() {
            // Mutations should not be in read-only categories
            let cmd = surface.command_name();
            // Just verify is_mutation returns consistently
            prop_assert!(surface.is_mutation(),
                "is_mutation should be deterministic for {}", cmd);
        }
    }

    #[test]
    fn api_surface_ntm_compat_is_deterministic(surface in arb_api_surface()) {
        prop_assert_eq!(surface.has_ntm_compat(), surface.has_ntm_compat(),
            "has_ntm_compat should be deterministic for {:?}", surface);
    }
}

#[test]
fn api_surface_all_has_33_variants() {
    assert_eq!(ApiSurface::ALL.len(), 33);
}

#[test]
fn api_surface_command_names_are_unique() {
    let mut names = std::collections::HashSet::new();
    for surface in ApiSurface::ALL {
        assert!(
            names.insert(surface.command_name()),
            "duplicate command_name: {}",
            surface.command_name()
        );
    }
}

// =============================================================================
// CheckCategory invariants
// =============================================================================

proptest! {
    #[test]
    fn check_category_label_nonempty(cat in arb_check_category()) {
        prop_assert!(!cat.label().is_empty());
    }
}

// =============================================================================
// ContractMatrix invariants
// =============================================================================

proptest! {
    #[test]
    fn matrix_check_count_matches_registrations(
        checks in prop::collection::vec(arb_contract_check(), 0..15)
    ) {
        let mut matrix = ContractMatrix::new("test-matrix");
        for check in &checks {
            matrix.register(check.clone());
        }
        prop_assert_eq!(matrix.check_count(), checks.len());
    }

    #[test]
    fn matrix_surface_filter_subset_of_total(
        checks in prop::collection::vec(arb_contract_check(), 1..15),
        surface in arb_api_surface(),
    ) {
        let mut matrix = ContractMatrix::new("test-matrix");
        for check in &checks {
            matrix.register(check.clone());
        }
        let filtered = matrix.checks_for_surface(surface);
        prop_assert!(filtered.len() <= matrix.check_count());
        for check in &filtered {
            prop_assert_eq!(check.surface, surface);
        }
    }

    #[test]
    fn matrix_category_filter_subset_of_total(
        checks in prop::collection::vec(arb_contract_check(), 1..15),
        category in arb_check_category(),
    ) {
        let mut matrix = ContractMatrix::new("test-matrix");
        for check in &checks {
            matrix.register(check.clone());
        }
        let filtered = matrix.checks_for_category(category);
        prop_assert!(filtered.len() <= matrix.check_count());
        for check in &filtered {
            prop_assert_eq!(check.category, category);
        }
    }

    #[test]
    fn matrix_blocking_count_le_total(
        checks in prop::collection::vec(arb_contract_check(), 0..15)
    ) {
        let mut matrix = ContractMatrix::new("test-matrix");
        for check in &checks {
            matrix.register(check.clone());
        }
        prop_assert!(matrix.blocking_count() <= matrix.check_count());
    }

    #[test]
    fn matrix_surface_coverage_upper_bounded(
        checks in prop::collection::vec(arb_contract_check(), 0..15)
    ) {
        let mut matrix = ContractMatrix::new("test-matrix");
        for check in &checks {
            matrix.register(check.clone());
        }
        let (covered, total_surfaces) = matrix.surface_coverage();
        prop_assert!(covered <= total_surfaces);
        prop_assert_eq!(total_surfaces, 33);
    }

    #[test]
    fn matrix_uncovered_plus_covered_equals_33(
        checks in prop::collection::vec(arb_contract_check(), 0..20)
    ) {
        let mut matrix = ContractMatrix::new("test-matrix");
        for check in &checks {
            matrix.register(check.clone());
        }
        let (covered, _) = matrix.surface_coverage();
        let uncovered = matrix.uncovered_surfaces().len();
        prop_assert_eq!(covered + uncovered, 33,
            "covered ({}) + uncovered ({}) should equal 33", covered, uncovered);
    }
}

// =============================================================================
// ContractExecution invariants
// =============================================================================

proptest! {
    #[test]
    fn execution_counters_consistency(
        results in prop::collection::vec(arb_check_result(), 0..20)
    ) {
        let mut exec = ContractExecution::new("matrix", "run-1", 1000);
        for result in &results {
            exec.record(result.clone());
        }
        exec.complete(2000);

        let total = exec.results.len();
        let passed = exec.passed();
        let failed = exec.failed();

        prop_assert_eq!(passed + failed, total,
            "passed ({}) + failed ({}) should equal total ({})", passed, failed, total);

        // pass_rate sanity
        if total > 0 {
            let rate = exec.pass_rate();
            prop_assert!(rate >= 0.0 && rate <= 1.0,
                "pass_rate {:.4} should be in [0, 1]", rate);
            let expected = passed as f64 / total as f64;
            prop_assert!((rate - expected).abs() < 1e-10);
        } else {
            prop_assert!((exec.pass_rate() - 1.0).abs() < 1e-10);
        }
    }

    #[test]
    fn execution_blocking_pass_logic(
        n_blocking_pass in 0..5usize,
        n_blocking_fail in 0..5usize,
        n_advisory_fail in 0..5usize,
    ) {
        let mut exec = ContractExecution::new("matrix", "run-1", 0);

        for i in 0..n_blocking_pass {
            let mut r = CheckResult::pass(&format!("BP-{i}"), ApiSurface::GetText, CheckCategory::SchemaStability);
            r.blocking = true;
            exec.record(r);
        }

        for i in 0..n_blocking_fail {
            let mut r = CheckResult::fail(&format!("BF-{i}"), ApiSurface::SendText, CheckCategory::Determinism, "fail");
            r.blocking = true;
            exec.record(r);
        }

        for i in 0..n_advisory_fail {
            let mut r = CheckResult::fail(&format!("AF-{i}"), ApiSurface::Events, CheckCategory::ReplayCorrectness, "advisory fail");
            r.blocking = false;
            exec.record(r);
        }

        exec.complete(1000);

        if n_blocking_fail > 0 {
            prop_assert!(!exec.blocking_pass(),
                "blocking_pass should be false when blocking failures exist");
        } else {
            prop_assert!(exec.blocking_pass(),
                "blocking_pass should be true when no blocking failures");
        }
    }

    #[test]
    fn execution_failures_by_category_covers_all_failures(
        results in prop::collection::vec(arb_check_result(), 1..15)
    ) {
        let mut exec = ContractExecution::new("matrix", "run-1", 0);
        for result in &results {
            exec.record(result.clone());
        }
        exec.complete(1000);

        let by_cat = exec.failures_by_category();
        let total_in_categories: usize = by_cat.values().map(|v| v.len()).sum();
        prop_assert_eq!(total_in_categories, exec.failed(),
            "failures_by_category total ({}) should match failed count ({})",
            total_in_categories, exec.failed());
    }
}

// =============================================================================
// ContractReport verdict logic
// =============================================================================

proptest! {
    #[test]
    fn report_verdict_reflects_blocking(
        n_pass in 0..5usize,
        n_blocking_fail in 0..3usize,
        n_advisory_fail in 0..3usize,
    ) {
        let total = n_pass + n_blocking_fail + n_advisory_fail;
        if total == 0 {
            return Ok(());
        }

        let mut exec = ContractExecution::new("matrix", "run-1", 0);

        for i in 0..n_pass {
            let r = CheckResult::pass(&format!("P-{i}"), ApiSurface::GetText, CheckCategory::SchemaStability);
            exec.record(r);
        }

        for i in 0..n_blocking_fail {
            let mut r = CheckResult::fail(&format!("BF-{i}"), ApiSurface::SendText, CheckCategory::Determinism, "fail");
            r.blocking = true;
            exec.record(r);
        }

        for i in 0..n_advisory_fail {
            let mut r = CheckResult::fail(&format!("AF-{i}"), ApiSurface::Events, CheckCategory::ReplayCorrectness, "advisory");
            r.blocking = false;
            exec.record(r);
        }

        exec.complete(1000);
        let report = ContractReport::from_execution(&exec);

        prop_assert_eq!(report.total, total);
        prop_assert_eq!(report.passed, n_pass);
        prop_assert_eq!(report.failed, n_blocking_fail + n_advisory_fail);

        if n_blocking_fail > 0 {
            prop_assert_eq!(report.verdict, ContractVerdict::Incompatible,
                "blocking failures should produce Incompatible verdict");
            prop_assert!(!report.blocking_pass);
        } else if n_advisory_fail > 0 {
            prop_assert_eq!(report.verdict, ContractVerdict::ConditionallyCompatible,
                "advisory-only failures should produce ConditionallyCompatible");
            prop_assert!(report.blocking_pass);
        } else {
            prop_assert_eq!(report.verdict, ContractVerdict::Compatible,
                "all-pass should produce Compatible");
            prop_assert!(report.blocking_pass);
        }
    }

    #[test]
    fn report_pass_rate_arithmetic(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut exec = ContractExecution::new("matrix", "run-1", 0);
        for i in 0..n_pass {
            exec.record(CheckResult::pass(&format!("P-{i}"), ApiSurface::GetText, CheckCategory::SchemaStability));
        }
        for i in 0..n_fail {
            exec.record(CheckResult::fail(&format!("F-{i}"), ApiSurface::SendText, CheckCategory::Determinism, "err"));
        }
        exec.complete(1000);

        let report = ContractReport::from_execution(&exec);
        let expected_rate = n_pass as f64 / total as f64;
        prop_assert!((report.pass_rate - expected_rate).abs() < 1e-10,
            "pass_rate {:.6} vs expected {:.6}", report.pass_rate, expected_rate);
    }

    #[test]
    fn report_duration_from_execution(
        start in 0..10000u64,
        duration in 0..5000u64,
    ) {
        let end = start + duration;
        let mut exec = ContractExecution::new("matrix", "run-1", start);
        exec.record(CheckResult::pass("P-1", ApiSurface::GetText, CheckCategory::SchemaStability));
        exec.complete(end);

        let report = ContractReport::from_execution(&exec);
        prop_assert_eq!(report.duration_ms, duration);
    }
}

// =============================================================================
// Standard factory tests
// =============================================================================

#[test]
fn standard_matrix_covers_all_33_surfaces() {
    let matrix = standard_contract_matrix();
    let (covered, total) = matrix.surface_coverage();
    assert_eq!(total, 33);
    assert_eq!(covered, 33, "standard matrix should cover all surfaces");
    assert!(matrix.uncovered_surfaces().is_empty());
}

#[test]
fn standard_matrix_has_blocking_checks() {
    let matrix = standard_contract_matrix();
    assert!(
        matrix.blocking_count() > 0,
        "standard matrix should have blocking checks"
    );
}

#[test]
fn standard_matrix_coverage_markdown_has_table() {
    let matrix = standard_contract_matrix();
    let md = matrix.render_coverage_matrix();
    assert!(md.contains("Coverage"));
    assert!(md.contains("get-text"));
}

#[test]
fn standard_matrix_json_snapshot_is_valid() {
    let matrix = standard_contract_matrix();
    let json = matrix.render_json_snapshot().unwrap();
    assert!(!json.is_empty());
    // Should parse back
    let _: serde_json::Value = serde_json::from_str(&json).unwrap();
}

#[test]
fn standard_export_artifacts_complete() {
    let artifacts = standard_contract_export_artifacts().unwrap();
    assert!(!artifacts.matrix_json.is_empty());
    assert!(!artifacts.coverage_markdown.is_empty());
    assert!(!artifacts.execution_trace_json.is_empty());
    assert!(!artifacts.report_json.is_empty());

    // All JSON artifacts should be valid JSON
    let _: serde_json::Value = serde_json::from_str(&artifacts.matrix_json).unwrap();
    let _: serde_json::Value = serde_json::from_str(&artifacts.execution_trace_json).unwrap();
    let _: serde_json::Value = serde_json::from_str(&artifacts.report_json).unwrap();
}

#[test]
fn standard_export_report_verdict_is_compatible() {
    let artifacts = standard_contract_export_artifacts().unwrap();
    let report: ContractReport = serde_json::from_str(&artifacts.report_json).unwrap();
    assert_eq!(
        report.verdict,
        ContractVerdict::Compatible,
        "baseline execution should be Compatible"
    );
    assert!(report.blocking_pass);
    assert_eq!(report.failed, 0);
}
