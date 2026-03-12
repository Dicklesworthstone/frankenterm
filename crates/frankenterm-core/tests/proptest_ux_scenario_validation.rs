//! Property tests for ux_scenario_validation module (ft-3681t.9.1).
//!
//! Covers serde roundtrips, workflow class / step type / friction category
//! label invariants, phase acceptance evaluation, scenario verdict logic,
//! execution counter arithmetic, UxTelemetry aggregation, GoNoGoEvaluation
//! gate pass logic, and standard scenario factory invariants.

use frankenterm_core::ux_scenario_validation::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_workflow_class() -> impl Strategy<Value = WorkflowClass> {
    prop_oneof![
        Just(WorkflowClass::Launch),
        Just(WorkflowClass::Triage),
        Just(WorkflowClass::Intervention),
        Just(WorkflowClass::Approval),
        Just(WorkflowClass::IncidentHandling),
        Just(WorkflowClass::MigrationOversight),
        Just(WorkflowClass::ContextManagement),
        Just(WorkflowClass::DashboardReview),
    ]
}

fn arb_step_type() -> impl Strategy<Value = StepType> {
    prop_oneof![
        Just(StepType::Navigate),
        Just(StepType::Inspect),
        Just(StepType::Execute),
        Just(StepType::WaitForFeedback),
        Just(StepType::Confirm),
        Just(StepType::Recover),
        Just(StepType::Verify),
    ]
}

fn arb_friction_category() -> impl Strategy<Value = FrictionCategory> {
    prop_oneof![
        Just(FrictionCategory::UnexpectedPrompt),
        Just(FrictionCategory::Retry),
        Just(FrictionCategory::ConfusingFeedback),
        Just(FrictionCategory::MissingInfo),
        Just(FrictionCategory::Sluggish),
        Just(FrictionCategory::AccessibilityBarrier),
        Just(FrictionCategory::NavigationConfusion),
    ]
}

fn arb_scenario_verdict() -> impl Strategy<Value = ScenarioVerdict> {
    prop_oneof![
        Just(ScenarioVerdict::Pass),
        Just(ScenarioVerdict::Degraded),
        Just(ScenarioVerdict::Fail),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_workflow_class(wc in arb_workflow_class()) {
        let json = serde_json::to_string(&wc).unwrap();
        let back: WorkflowClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wc, back);
    }

    #[test]
    fn serde_roundtrip_step_type(st in arb_step_type()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: StepType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_friction_category(fc in arb_friction_category()) {
        let json = serde_json::to_string(&fc).unwrap();
        let back: FrictionCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(fc, back);
    }

    #[test]
    fn serde_roundtrip_scenario_verdict(sv in arb_scenario_verdict()) {
        let json = serde_json::to_string(&sv).unwrap();
        let back: ScenarioVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sv, back);
    }

    #[test]
    fn serde_roundtrip_phase_acceptance(
        lat in 0..10000u64,
        req in any::<bool>(),
        friction in 0..20u32,
    ) {
        let pa = PhaseAcceptance {
            max_latency_ms: lat,
            required: req,
            max_friction_events: friction,
        };
        let json = serde_json::to_string(&pa).unwrap();
        let back: PhaseAcceptance = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(pa.max_latency_ms, back.max_latency_ms);
        prop_assert_eq!(pa.required, back.required);
        prop_assert_eq!(pa.max_friction_events, back.max_friction_events);
    }
}

// =============================================================================
// Label invariants
// =============================================================================

proptest! {
    #[test]
    fn workflow_class_label_nonempty(wc in arb_workflow_class()) {
        prop_assert!(!wc.label().is_empty());
    }

    #[test]
    fn friction_category_label_nonempty(fc in arb_friction_category()) {
        prop_assert!(!fc.label().is_empty());
    }
}

// =============================================================================
// PhaseResult constructor invariants
// =============================================================================

proptest! {
    #[test]
    fn success_phase_result_invariants(
        id in "[a-z-]{3,15}",
        elapsed in 0..10000u64,
    ) {
        let result = PhaseResult::success(&id, elapsed);
        prop_assert!(result.success);
        prop_assert_eq!(result.elapsed_ms, elapsed);
        prop_assert!(result.error.is_none());
        prop_assert!(result.acceptance_met);
        prop_assert_eq!(result.friction_count(), 0);
    }

    #[test]
    fn failure_phase_result_invariants(
        id in "[a-z-]{3,15}",
        elapsed in 0..10000u64,
        error in ".{1,30}",
    ) {
        let result = PhaseResult::failure(&id, elapsed, &error);
        prop_assert!(!result.success);
        prop_assert_eq!(result.elapsed_ms, elapsed);
        prop_assert!(result.error.is_some());
        prop_assert!(!result.acceptance_met);
    }
}

// =============================================================================
// ScenarioEvaluator phase acceptance
// =============================================================================

proptest! {
    #[test]
    fn phase_acceptance_pass_when_within_limits(
        max_lat in 100..5000u64,
        max_friction in 1..5u32,
    ) {
        let phase = ScenarioPhase {
            phase_id: "test".into(),
            description: "test".into(),
            step_type: StepType::Execute,
            acceptance: PhaseAcceptance {
                max_latency_ms: max_lat,
                required: true,
                max_friction_events: max_friction,
            },
        };
        let result = PhaseResult::success("test", max_lat - 1);
        prop_assert!(ScenarioEvaluator::evaluate_phase(&phase, &result));
    }

    #[test]
    fn phase_acceptance_fail_when_over_latency(
        max_lat in 100..5000u64,
        overshoot in 1..1000u64,
    ) {
        let phase = ScenarioPhase {
            phase_id: "test".into(),
            description: "test".into(),
            step_type: StepType::Execute,
            acceptance: PhaseAcceptance {
                max_latency_ms: max_lat,
                required: true,
                max_friction_events: 5,
            },
        };
        let result = PhaseResult::success("test", max_lat + overshoot);
        prop_assert!(!ScenarioEvaluator::evaluate_phase(&phase, &result));
    }

    #[test]
    fn phase_acceptance_fail_when_failed(
        max_lat in 100..5000u64,
    ) {
        let phase = ScenarioPhase {
            phase_id: "test".into(),
            description: "test".into(),
            step_type: StepType::Verify,
            acceptance: PhaseAcceptance {
                max_latency_ms: max_lat,
                required: true,
                max_friction_events: 10,
            },
        };
        let result = PhaseResult::failure("test", 10, "err");
        prop_assert!(!ScenarioEvaluator::evaluate_phase(&phase, &result));
    }
}

// =============================================================================
// ScenarioExecution counter arithmetic
// =============================================================================

proptest! {
    #[test]
    fn execution_pass_fail_sum(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut phases = Vec::new();
        for i in 0..n_pass {
            phases.push(PhaseResult::success(format!("p-{i}"), 100));
        }
        for i in 0..n_fail {
            phases.push(PhaseResult::failure(format!("f-{i}"), 100, "err"));
        }

        let exec = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 0,
            ended_at_ms: 1000,
            phase_results: phases,
            verdict: ScenarioVerdict::Pass,
            notes: Vec::new(),
        };

        prop_assert_eq!(exec.phases_passed(), n_pass);
        prop_assert_eq!(exec.phases_failed(), n_fail);
        prop_assert_eq!(exec.phases_passed() + exec.phases_failed(), total);

        let rate = exec.completion_rate();
        let expected = n_pass as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10);
    }

    #[test]
    fn execution_elapsed_time(
        start in 0..10000u64,
        duration in 0..5000u64,
    ) {
        let exec = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Triage,
            started_at_ms: start,
            ended_at_ms: start + duration,
            phase_results: vec![PhaseResult::success("s1", 100)],
            verdict: ScenarioVerdict::Pass,
            notes: Vec::new(),
        };
        prop_assert_eq!(exec.total_elapsed_ms(), duration);
    }

    #[test]
    fn execution_friction_sum(
        n_friction_per_phase in 0..3usize,
        n_phases in 1..4usize,
    ) {
        let mut phases = Vec::new();
        for i in 0..n_phases {
            let mut result = PhaseResult::success(format!("p-{i}"), 100);
            for j in 0..n_friction_per_phase {
                result.add_friction(FrictionEvent {
                    description: format!("friction-{j}"),
                    category: FrictionCategory::Retry,
                    at_ms: j as u64 * 10,
                });
            }
            phases.push(result);
        }

        let exec = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 0,
            ended_at_ms: 1000,
            phase_results: phases,
            verdict: ScenarioVerdict::Pass,
            notes: Vec::new(),
        };

        let expected_total = (n_friction_per_phase * n_phases) as u32;
        prop_assert_eq!(exec.total_friction(), expected_total);

        let expected_mean = expected_total as f64 / n_phases as f64;
        prop_assert!((exec.mean_friction() - expected_mean).abs() < 1e-10);
    }
}

// =============================================================================
// ScenarioEvaluator verdict logic
// =============================================================================

proptest! {
    #[test]
    fn verdict_pass_when_all_phases_meet_acceptance(
        n_phases in 1..5usize,
    ) {
        let mut phases = Vec::new();
        let mut results = Vec::new();
        for i in 0..n_phases {
            let pid = format!("p-{i}");
            phases.push(ScenarioPhase {
                phase_id: pid.clone(),
                description: "test".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 1000,
                    required: true,
                    max_friction_events: 5,
                },
            });
            results.push(PhaseResult::success(&pid, 100));
        }

        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "test".into(),
            workflow_class: WorkflowClass::Launch,
            phases,
            thresholds: UxThresholds::development(),
        };
        let exec = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 0,
            ended_at_ms: 1000,
            phase_results: results,
            verdict: ScenarioVerdict::Pass,
            notes: Vec::new(),
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &exec);
        prop_assert_eq!(verdict, ScenarioVerdict::Pass);
    }

    #[test]
    fn verdict_fail_when_required_phase_fails(
        n_ok in 0..3usize,
    ) {
        let mut phases = Vec::new();
        let mut results = Vec::new();

        for i in 0..n_ok {
            let pid = format!("ok-{i}");
            phases.push(ScenarioPhase {
                phase_id: pid.clone(),
                description: "ok".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 1000,
                    required: true,
                    max_friction_events: 5,
                },
            });
            results.push(PhaseResult::success(&pid, 100));
        }

        // Add one required phase that fails
        let fail_pid = "fail-required";
        phases.push(ScenarioPhase {
            phase_id: fail_pid.into(),
            description: "required fail".into(),
            step_type: StepType::Verify,
            acceptance: PhaseAcceptance {
                max_latency_ms: 1000,
                required: true,
                max_friction_events: 0,
            },
        });
        results.push(PhaseResult::failure(fail_pid, 100, "required failed"));

        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "test".into(),
            workflow_class: WorkflowClass::Triage,
            phases,
            thresholds: UxThresholds::development(),
        };
        let exec = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Triage,
            started_at_ms: 0,
            ended_at_ms: 1000,
            phase_results: results,
            verdict: ScenarioVerdict::Pass,
            notes: Vec::new(),
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &exec);
        prop_assert_eq!(verdict, ScenarioVerdict::Fail);
    }

    #[test]
    fn verdict_degraded_when_optional_fails(
        n_ok in 1..3usize,
    ) {
        let mut phases = Vec::new();
        let mut results = Vec::new();

        for i in 0..n_ok {
            let pid = format!("ok-{i}");
            phases.push(ScenarioPhase {
                phase_id: pid.clone(),
                description: "ok".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 1000,
                    required: true,
                    max_friction_events: 5,
                },
            });
            results.push(PhaseResult::success(&pid, 100));
        }

        // Add one optional phase that fails
        let opt_pid = "fail-optional";
        phases.push(ScenarioPhase {
            phase_id: opt_pid.into(),
            description: "optional fail".into(),
            step_type: StepType::Inspect,
            acceptance: PhaseAcceptance {
                max_latency_ms: 100,
                required: false,
                max_friction_events: 0,
            },
        });
        results.push(PhaseResult::failure(opt_pid, 200, "optional failed"));

        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "test".into(),
            workflow_class: WorkflowClass::DashboardReview,
            phases,
            thresholds: UxThresholds::development(),
        };
        let exec = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::DashboardReview,
            started_at_ms: 0,
            ended_at_ms: 1000,
            phase_results: results,
            verdict: ScenarioVerdict::Pass,
            notes: Vec::new(),
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &exec);
        prop_assert_eq!(verdict, ScenarioVerdict::Degraded);
    }
}

// =============================================================================
// GoNoGoEvaluation gate logic
// =============================================================================

proptest! {
    #[test]
    fn go_when_all_thresholds_met(
        n_pass in 1..5usize,
    ) {
        let spec = launch_scenario();
        let mut telemetry = UxTelemetry::new();

        for i in 0..n_pass {
            let mut results = Vec::new();
            for phase in &spec.phases {
                results.push(PhaseResult::success(&phase.phase_id, 50));
            }
            let exec = ScenarioExecution {
                scenario_id: format!("exec-{i}"),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 500,
                phase_results: results,
                verdict: ScenarioVerdict::Pass,
                notes: Vec::new(),
            };
            telemetry.record_execution(&spec, &exec);
        }

        let eval = GoNoGoEvaluation::evaluate(&telemetry, &spec.thresholds);
        prop_assert!(eval.go, "all thresholds met but evaluation says NO-GO: {}", eval.summary);
    }

    #[test]
    fn evaluation_deterministic(
        n_scenarios in 1..3usize,
    ) {
        let spec = launch_scenario();
        let mut telemetry = UxTelemetry::new();

        for i in 0..n_scenarios {
            let mut results = Vec::new();
            for phase in &spec.phases {
                results.push(PhaseResult::success(&phase.phase_id, 100));
            }
            let exec = ScenarioExecution {
                scenario_id: format!("exec-{i}"),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 1000,
                phase_results: results,
                verdict: ScenarioVerdict::Pass,
                notes: Vec::new(),
            };
            telemetry.record_execution(&spec, &exec);
        }

        let thresholds = UxThresholds::release_gate();
        let eval1 = GoNoGoEvaluation::evaluate(&telemetry, &thresholds);
        let eval2 = GoNoGoEvaluation::evaluate(&telemetry, &thresholds);
        prop_assert_eq!(eval1.go, eval2.go);
        prop_assert_eq!(eval1.checks.len(), eval2.checks.len());
    }
}

// =============================================================================
// UxTelemetry rate arithmetic
// =============================================================================

proptest! {
    #[test]
    fn telemetry_scenario_pass_rate(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let spec = launch_scenario();
        let mut telemetry = UxTelemetry::new();

        for i in 0..n_pass {
            let results: Vec<PhaseResult> = spec.phases.iter()
                .map(|p| PhaseResult::success(&p.phase_id, 100))
                .collect();
            let exec = ScenarioExecution {
                scenario_id: format!("pass-{i}"),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 1000,
                phase_results: results,
                verdict: ScenarioVerdict::Pass,
                notes: Vec::new(),
            };
            telemetry.record_execution(&spec, &exec);
        }

        for i in 0..n_fail {
            let results: Vec<PhaseResult> = spec.phases.iter()
                .map(|p| PhaseResult::failure(&p.phase_id, 100, "err"))
                .collect();
            let exec = ScenarioExecution {
                scenario_id: format!("fail-{i}"),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 1000,
                phase_results: results,
                verdict: ScenarioVerdict::Fail,
                notes: Vec::new(),
            };
            telemetry.record_execution(&spec, &exec);
        }

        let rate = telemetry.scenario_pass_rate();
        let expected = n_pass as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10,
            "scenario_pass_rate {} != expected {}", rate, expected);
    }

    #[test]
    fn telemetry_counters_consistent(
        n_scenarios in 1..5usize,
    ) {
        let spec = launch_scenario();
        let mut telemetry = UxTelemetry::new();

        for i in 0..n_scenarios {
            let results: Vec<PhaseResult> = spec.phases.iter()
                .map(|p| PhaseResult::success(&p.phase_id, 100))
                .collect();
            let exec = ScenarioExecution {
                scenario_id: format!("exec-{i}"),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 1000,
                phase_results: results,
                verdict: ScenarioVerdict::Pass,
                notes: Vec::new(),
            };
            telemetry.record_execution(&spec, &exec);
        }

        prop_assert_eq!(telemetry.scenarios_executed, n_scenarios as u64);
        let sum = telemetry.scenarios_passed + telemetry.scenarios_degraded + telemetry.scenarios_failed;
        prop_assert_eq!(sum, telemetry.scenarios_executed,
            "pass+degraded+fail should equal total executed");
    }
}

// =============================================================================
// ScenarioSpec invariants
// =============================================================================

proptest! {
    #[test]
    fn required_count_le_total(
        n_required in 0..5usize,
        n_optional in 0..5usize,
    ) {
        let total = n_required + n_optional;
        if total == 0 {
            return Ok(());
        }

        let mut phases = Vec::new();
        for i in 0..n_required {
            phases.push(ScenarioPhase {
                phase_id: format!("r-{i}"),
                description: "required".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            });
        }
        for i in 0..n_optional {
            phases.push(ScenarioPhase {
                phase_id: format!("o-{i}"),
                description: "optional".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance::lenient(),
            });
        }

        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "test".into(),
            workflow_class: WorkflowClass::Launch,
            phases,
            thresholds: UxThresholds::development(),
        };

        prop_assert_eq!(spec.required_phase_count(), n_required);
        prop_assert_eq!(spec.total_phase_count(), total);
        prop_assert!(spec.required_phase_count() <= spec.total_phase_count());
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn launch_scenario_has_phases() {
    let spec = launch_scenario();
    assert!(!spec.phases.is_empty());
    assert_eq!(spec.workflow_class, WorkflowClass::Launch);
    assert!(spec.required_phase_count() > 0);
}

#[test]
fn triage_scenario_has_phases() {
    let spec = triage_scenario();
    assert!(!spec.phases.is_empty());
    assert_eq!(spec.workflow_class, WorkflowClass::Triage);
}

#[test]
fn release_gate_thresholds_strict() {
    let release = UxThresholds::release_gate();
    let dev = UxThresholds::development();
    assert!(release.min_completion_rate >= dev.min_completion_rate);
    assert!(release.max_p95_latency_ms <= dev.max_p95_latency_ms);
}

#[test]
fn phase_acceptance_strict_tighter_than_lenient() {
    let strict = PhaseAcceptance::strict();
    let lenient = PhaseAcceptance::lenient();
    assert!(strict.max_latency_ms <= lenient.max_latency_ms);
    assert!(strict.max_friction_events <= lenient.max_friction_events);
}

#[test]
fn go_no_go_renders() {
    let telemetry = UxTelemetry::new();
    let thresholds = UxThresholds::development();
    let eval = GoNoGoEvaluation::evaluate(&telemetry, &thresholds);
    let rendered = eval.render();
    assert!(!rendered.is_empty());
    assert!(rendered.contains("Go/No-Go"));
}
