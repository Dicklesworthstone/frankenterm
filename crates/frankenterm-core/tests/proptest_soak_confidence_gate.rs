//! Property tests for soak_confidence_gate module (ft-e34d9.10.8.5).
//!
//! Covers serde roundtrips, SoakMatrix plan generation, execution result
//! counter arithmetic, confidence gate evaluation logic, and standard
//! factory invariants.

use frankenterm_core::soak_confidence_gate::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_journey_category() -> impl Strategy<Value = JourneyCategory> {
    (0..JourneyCategory::ALL.len()).prop_map(|i| JourneyCategory::ALL[i])
}

fn arb_workload_profile() -> impl Strategy<Value = WorkloadProfile> {
    (0..WorkloadProfile::ALL.len()).prop_map(|i| WorkloadProfile::ALL[i])
}

fn arb_failure_injection() -> impl Strategy<Value = FailureInjectionProfile> {
    (0..FailureInjectionProfile::ALL.len()).prop_map(|i| FailureInjectionProfile::ALL[i])
}

fn arb_confidence_decision() -> impl Strategy<Value = ConfidenceDecision> {
    prop_oneof![
        Just(ConfidenceDecision::Confident),
        Just(ConfidenceDecision::ConditionallyConfident),
        Just(ConfidenceDecision::NotConfident),
    ]
}

fn _arb_cell_result(passed: bool) -> impl Strategy<Value = CellResult> {
    (
        "[a-z-]{3,15}",
        arb_journey_category(),
        arb_workload_profile(),
        arb_failure_injection(),
        any::<bool>(),
        0..10000u64,
        0.0..0.5f64,
        0.0..500.0f64,
    )
        .prop_map(
            move |(id, cat, wl, inj, blocking, dur, err_rate, p95)| CellResult {
                cell_id: id,
                category: cat,
                workload: wl,
                injection: inj,
                passed,
                blocking,
                duration_ms: dur,
                failure_reason: if passed {
                    None
                } else {
                    Some("test fail".into())
                },
                error_rate: err_rate,
                p95_latency_ms: p95,
                seed: None,
                telemetry: CellTelemetry::default(),
            },
        )
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_journey_category(cat in arb_journey_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: JourneyCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_workload_profile(wl in arb_workload_profile()) {
        let json = serde_json::to_string(&wl).unwrap();
        let back: WorkloadProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wl, back);
    }

    #[test]
    fn serde_roundtrip_failure_injection(inj in arb_failure_injection()) {
        let json = serde_json::to_string(&inj).unwrap();
        let back: FailureInjectionProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(inj, back);
    }

    #[test]
    fn serde_roundtrip_confidence_decision(dec in arb_confidence_decision()) {
        let json = serde_json::to_string(&dec).unwrap();
        let back: ConfidenceDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dec, back);
    }
}

// =============================================================================
// JourneyCategory invariants
// =============================================================================

proptest! {
    #[test]
    fn journey_category_label_nonempty(cat in arb_journey_category()) {
        prop_assert!(!cat.label().is_empty());
    }

    #[test]
    fn journey_category_critical_deterministic(cat in arb_journey_category()) {
        prop_assert_eq!(cat.is_critical(), cat.is_critical());
    }
}

// =============================================================================
// SoakMatrix invariants
// =============================================================================

proptest! {
    #[test]
    fn matrix_cell_count_is_product(
        n_scenarios in 1..5usize,
        n_workloads in 1..4usize,
        n_injections in 1..4usize,
    ) {
        let scenarios: Vec<UserJourneyScenario> = (0..n_scenarios)
            .map(|i| UserJourneyScenario {
                scenario_id: format!("s-{i}"),
                category: JourneyCategory::ALL[i % JourneyCategory::ALL.len()],
                description: "test".into(),
                expected_duration_ms: 1000,
                blocking: true,
                seed: None,
                command: String::new(),
            })
            .collect();

        let workloads: Vec<WorkloadProfile> = WorkloadProfile::ALL[..n_workloads].to_vec();
        let injections: Vec<FailureInjectionProfile> = FailureInjectionProfile::ALL[..n_injections].to_vec();

        let matrix = SoakMatrix::custom(scenarios, workloads, injections);
        let expected = n_scenarios * n_workloads * n_injections;
        prop_assert_eq!(matrix.cell_count(), expected,
            "cell_count should be scenarios * workloads * injections: {} * {} * {} = {}",
            n_scenarios, n_workloads, n_injections, expected);
    }

    #[test]
    fn matrix_plan_cell_count_matches(
        n_scenarios in 1..4usize,
    ) {
        let scenarios: Vec<UserJourneyScenario> = (0..n_scenarios)
            .map(|i| UserJourneyScenario {
                scenario_id: format!("s-{i}"),
                category: JourneyCategory::ALL[i % JourneyCategory::ALL.len()],
                description: "test".into(),
                expected_duration_ms: 1000,
                blocking: true,
                seed: None,
                command: String::new(),
            })
            .collect();

        let matrix = SoakMatrix::custom(
            scenarios,
            WorkloadProfile::ALL.to_vec(),
            FailureInjectionProfile::ALL.to_vec(),
        );
        let plan = matrix.to_plan();
        prop_assert_eq!(plan.total_cells(), matrix.cell_count());
    }
}

// =============================================================================
// Execution result counter arithmetic
// =============================================================================

proptest! {
    #[test]
    fn execution_pass_fail_sum(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut exec = SoakExecutionResult::new(0);

        for i in 0..n_pass {
            exec.record_cell(CellResult {
                cell_id: format!("p-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: true,
                blocking: true,
                duration_ms: 100,
                failure_reason: None,
                error_rate: 0.0,
                p95_latency_ms: 10.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        for i in 0..n_fail {
            exec.record_cell(CellResult {
                cell_id: format!("f-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: true,
                duration_ms: 100,
                failure_reason: Some("err".into()),
                error_rate: 0.1,
                p95_latency_ms: 50.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        exec.complete(1000);

        prop_assert_eq!(exec.cells_passed(), n_pass);
        prop_assert_eq!(exec.cells_failed(), n_fail);

        let rate = exec.pass_rate();
        if total > 0 {
            let expected = n_pass as f64 / total as f64;
            prop_assert!((rate - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn blocking_failures_subset_of_failures(
        n_pass in 0..5usize,
        n_blocking_fail in 0..3usize,
        n_nonblocking_fail in 0..3usize,
    ) {
        let mut exec = SoakExecutionResult::new(0);

        for i in 0..n_pass {
            exec.record_cell(CellResult {
                cell_id: format!("p-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: true,
                blocking: true,
                duration_ms: 100,
                failure_reason: None,
                error_rate: 0.0,
                p95_latency_ms: 10.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        for i in 0..n_blocking_fail {
            exec.record_cell(CellResult {
                cell_id: format!("bf-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: true,
                duration_ms: 100,
                failure_reason: Some("err".into()),
                error_rate: 0.1,
                p95_latency_ms: 50.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        for i in 0..n_nonblocking_fail {
            exec.record_cell(CellResult {
                cell_id: format!("nf-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: false,
                duration_ms: 100,
                failure_reason: Some("minor".into()),
                error_rate: 0.05,
                p95_latency_ms: 30.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        exec.complete(1000);
        let blocking = exec.blocking_failures();
        prop_assert!(blocking <= exec.cells_failed());
        prop_assert_eq!(blocking, n_blocking_fail);
    }
}

// =============================================================================
// Confidence gate evaluation
// =============================================================================

proptest! {
    #[test]
    fn gate_evaluation_deterministic(
        n_pass in 1..5usize,
        n_fail in 0..3usize,
    ) {
        let mut exec = SoakExecutionResult::new(0);
        for i in 0..n_pass {
            exec.record_cell(CellResult {
                cell_id: format!("p-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: true,
                blocking: true,
                duration_ms: 100,
                failure_reason: None,
                error_rate: 0.0,
                p95_latency_ms: 10.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }
        for i in 0..n_fail {
            exec.record_cell(CellResult {
                cell_id: format!("f-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: true,
                duration_ms: 100,
                failure_reason: Some("err".into()),
                error_rate: 0.1,
                p95_latency_ms: 50.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }
        exec.complete(1000);

        let gate = ConfidenceGate::standard();
        let v1 = gate.evaluate(&exec);
        let v2 = gate.evaluate(&exec);
        prop_assert_eq!(v1.decision, v2.decision, "evaluation should be deterministic");
    }
}

// =============================================================================
// Standard factories
// =============================================================================

#[test]
fn standard_matrix_has_cells() {
    let matrix = SoakMatrix::standard();
    assert!(matrix.cell_count() > 0);
    assert!(matrix.blocking_scenario_count() > 0);
}

#[test]
fn ci_minimal_matrix_is_smaller() {
    let standard = SoakMatrix::standard();
    let ci = SoakMatrix::ci_minimal();
    assert!(ci.cell_count() <= standard.cell_count());
}

#[test]
fn standard_gate_thresholds_reasonable() {
    let gate = ConfidenceGate::standard();
    assert!(gate.min_pass_rate > 0.0 && gate.min_pass_rate <= 1.0);
    assert!(gate.max_error_rate >= 0.0 && gate.max_error_rate <= 1.0);
    assert!(gate.max_p95_latency_ms > 0.0);
}

#[test]
fn strict_gate_stricter_than_standard() {
    let standard = ConfidenceGate::standard();
    let strict = ConfidenceGate::strict();
    assert!(strict.min_pass_rate >= standard.min_pass_rate);
    assert!(strict.max_error_rate <= standard.max_error_rate);
}

#[test]
fn confidence_verdict_summary_renders() {
    let mut exec = SoakExecutionResult::new(0);
    exec.record_cell(CellResult {
        cell_id: "test".into(),
        category: JourneyCategory::Watch,
        workload: WorkloadProfile::Steady,
        injection: FailureInjectionProfile::None,
        passed: true,
        blocking: true,
        duration_ms: 100,
        failure_reason: None,
        error_rate: 0.0,
        p95_latency_ms: 10.0,
        seed: None,
        telemetry: CellTelemetry::default(),
    });
    exec.complete(1000);

    let gate = ConfidenceGate::standard();
    let verdict = gate.evaluate(&exec);
    let summary = verdict.render_summary();
    assert!(!summary.is_empty());
}
