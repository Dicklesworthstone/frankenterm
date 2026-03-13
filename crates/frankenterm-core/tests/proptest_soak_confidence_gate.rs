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

// =============================================================================
// Additional serde roundtrip tests for uncovered types
// =============================================================================

fn arb_scg_str() -> impl Strategy<Value = String> {
    "[a-z]{3,12}".prop_map(String::from)
}

fn arb_cell_telemetry() -> impl Strategy<Value = CellTelemetry> {
    (0u64..1000, 0u64..1000, 0u64..100, 0u64..500, 0u64..500, 0u64..100, 0u64..50, 0u64..50)
        .prop_map(|(attempted, succeeded, failed, spawned, completed, cancelled, faults, recoveries)| {
            CellTelemetry {
                ops_attempted: attempted, ops_succeeded: succeeded, ops_failed: failed,
                tasks_spawned: spawned, tasks_completed: completed, tasks_cancelled: cancelled,
                faults_injected: faults, recoveries,
            }
        })
}

fn arb_cell_result() -> impl Strategy<Value = CellResult> {
    (
        arb_scg_str(), arb_journey_category(), arb_workload_profile(),
        arb_failure_injection(), proptest::bool::ANY, proptest::bool::ANY,
        0u64..60_000, arb_cell_telemetry(),
    )
        .prop_map(|(cell_id, category, workload, injection, passed, blocking, dur, telemetry)| {
            CellResult {
                cell_id, category, workload, injection, passed, blocking,
                duration_ms: dur, failure_reason: None,
                error_rate: 0.01, p95_latency_ms: 42.5,
                seed: Some(42), telemetry,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn scg_s01_user_journey_scenario_serde(
        sid in arb_scg_str(), cat in arb_journey_category(), blocking in proptest::bool::ANY,
    ) {
        let scenario = UserJourneyScenario {
            scenario_id: sid.clone(), category: cat,
            description: "test scenario".to_string(), expected_duration_ms: 60_000,
            blocking, seed: Some(42), command: "cargo test".to_string(),
        };
        let json = serde_json::to_string(&scenario).unwrap();
        let back: UserJourneyScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.scenario_id, &sid);
        prop_assert_eq!(back.blocking, blocking);
    }

    #[test]
    fn scg_s02_soak_matrix_serde(cat in arb_journey_category()) {
        let matrix = SoakMatrix::custom(
            vec![UserJourneyScenario {
                scenario_id: "s1".to_string(), category: cat,
                description: "test".to_string(), expected_duration_ms: 1000,
                blocking: true, seed: None, command: "echo".to_string(),
            }],
            vec![WorkloadProfile::Steady],
            vec![FailureInjectionProfile::None],
        );
        let json = serde_json::to_string(&matrix).unwrap();
        let back: SoakMatrix = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.scenarios.len(), 1);
        prop_assert_eq!(back.workload_profiles.len(), 1);
    }

    #[test]
    fn scg_s03_soak_execution_plan_serde(cat in arb_journey_category(), wp in arb_workload_profile()) {
        let plan = SoakExecutionPlan {
            cells: vec![SoakCell {
                cell_id: "cell-1".to_string(), scenario_id: "s1".to_string(),
                category: cat, workload: wp, injection: FailureInjectionProfile::None,
                blocking: true, seed: Some(42),
            }],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: SoakExecutionPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cells.len(), 1);
        prop_assert_eq!(&back.cells[0].cell_id, "cell-1");
    }

    #[test]
    fn scg_s04_soak_cell_serde(
        cid in arb_scg_str(), cat in arb_journey_category(),
        wp in arb_workload_profile(), fi in arb_failure_injection(),
    ) {
        let cell = SoakCell {
            cell_id: cid.clone(), scenario_id: "s1".to_string(),
            category: cat, workload: wp, injection: fi,
            blocking: true, seed: Some(99),
        };
        let json = serde_json::to_string(&cell).unwrap();
        let back: SoakCell = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cell_id, &cid);
        prop_assert_eq!(back.seed, Some(99));
    }

    #[test]
    fn scg_s05_soak_execution_result_serde(dur in 1000u64..60_000) {
        let result = SoakExecutionResult {
            cell_results: vec![], invariant_checks: vec![],
            total_duration_ms: dur, started_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_000_000 + dur,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: SoakExecutionResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_duration_ms, dur);
    }

    #[test]
    fn scg_s06_cell_result_serde(cr in arb_cell_result()) {
        let cid = cr.cell_id.clone();
        let passed = cr.passed;
        let json = serde_json::to_string(&cr).unwrap();
        let back: CellResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cell_id, &cid);
        prop_assert_eq!(back.passed, passed);
    }

    #[test]
    fn scg_s07_cell_telemetry_serde(tel in arb_cell_telemetry()) {
        let attempted = tel.ops_attempted;
        let json = serde_json::to_string(&tel).unwrap();
        let back: CellTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ops_attempted, attempted);
    }

    #[test]
    fn scg_s08_soak_invariant_check_serde(iid in arb_scg_str(), passed in proptest::bool::ANY) {
        let check = SoakInvariantCheck {
            invariant_id: iid.clone(), description: "test invariant".to_string(),
            passed, evidence: "ok".to_string(), mandatory: true,
        };
        let json = serde_json::to_string(&check).unwrap();
        let back: SoakInvariantCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.invariant_id, &iid);
        prop_assert_eq!(back.passed, passed);
    }

    #[test]
    fn scg_s09_aggregated_soak_telemetry_serde(ops in 0u64..10_000, faults in 0u64..500) {
        let tel = AggregatedSoakTelemetry {
            ops_attempted: ops, ops_succeeded: ops, ops_failed: 0,
            tasks_spawned: 100, tasks_completed: 100, tasks_cancelled: 0,
            faults_injected: faults, recoveries: faults,
            deadlock_detected_count: 0, max_p95_latency_ms: 42.5,
        };
        let json = serde_json::to_string(&tel).unwrap();
        let back: AggregatedSoakTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ops_attempted, ops);
        prop_assert_eq!(back.faults_injected, faults);
    }

    #[test]
    fn scg_s10_confidence_gate_serde(
        pass_rate in 0.5f64..1.0, error_rate in 0.0f64..0.1,
    ) {
        let gate = ConfidenceGate {
            min_pass_rate: pass_rate, max_error_rate: error_rate,
            max_p95_latency_ms: 5000.0,
            blocking_failures_are_hard_stop: true,
            mandatory_invariants_are_hard_stop: true,
        };
        let json = serde_json::to_string(&gate).unwrap();
        let back: ConfidenceGate = serde_json::from_str(&json).unwrap();
        prop_assert!((back.min_pass_rate - pass_rate).abs() < 1e-10);
        prop_assert!((back.max_error_rate - error_rate).abs() < 1e-10);
    }

    #[test]
    fn scg_s11_confidence_verdict_serde(
        decision in arb_confidence_decision(),
        total in 1usize..100, passed_count in 0usize..100,
    ) {
        let verdict = ConfidenceVerdict {
            decision, checks: vec![],
            cells_total: total, cells_passed: passed_count.min(total),
            cells_failed: total.saturating_sub(passed_count.min(total)),
            soak_duration_ms: 60_000,
        };
        let json = serde_json::to_string(&verdict).unwrap();
        let back: ConfidenceVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cells_total, total);
    }

    #[test]
    fn scg_s12_gate_condition_serde(cid in arb_scg_str(), passed in proptest::bool::ANY, blocking in proptest::bool::ANY) {
        let cond = GateCondition {
            condition_id: cid.clone(), description: "test check".to_string(),
            passed, measured: "42%".to_string(), blocking,
        };
        let json = serde_json::to_string(&cond).unwrap();
        let back: GateCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.condition_id, &cid);
        prop_assert_eq!(back.passed, passed);
        prop_assert_eq!(back.blocking, blocking);
    }
}
