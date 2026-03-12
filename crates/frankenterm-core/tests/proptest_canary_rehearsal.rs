//! Property tests for canary_rehearsal module.
//!
//! Covers serde roundtrips, cohort fraction invariants, drill step
//! counting, disruption budget enforcement, promotion criteria evaluation,
//! rollback trigger logic, and standard factory invariants.

use frankenterm_core::canary_rehearsal::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_drill_type() -> impl Strategy<Value = DrillType> {
    prop_oneof![
        Just(DrillType::Promotion),
        Just(DrillType::Rollback),
        Just(DrillType::FailSafe),
        Just(DrillType::FullRollout),
        Just(DrillType::PartialFailureRecovery),
    ]
}

fn arb_rollback_trigger_type() -> impl Strategy<Value = RollbackTriggerType> {
    prop_oneof![
        Just(RollbackTriggerType::ErrorRateSpike),
        Just(RollbackTriggerType::LatencySpike),
        Just(RollbackTriggerType::CrashRate),
        Just(RollbackTriggerType::DisruptionBudgetExceeded),
        Just(RollbackTriggerType::HealthCheckFailures),
        Just(RollbackTriggerType::OperatorManual),
    ]
}

fn arb_rehearsal_verdict() -> impl Strategy<Value = RehearsalVerdict> {
    prop_oneof![
        Just(RehearsalVerdict::Ready),
        Just(RehearsalVerdict::Conditional),
        Just(RehearsalVerdict::NotReady),
    ]
}

fn arb_drill_step(passed: bool) -> impl Strategy<Value = DrillStep> {
    ("[a-z-]{3,10}", ".{1,30}", 0..5000u64).prop_map(move |(id, desc, elapsed)| {
        if passed {
            DrillStep::pass(id, desc, elapsed)
        } else {
            DrillStep::fail(id, desc, elapsed, "test failure")
        }
    })
}

fn arb_disruption_accounting() -> impl Strategy<Value = DisruptionAccounting> {
    (0..500u64, 0.0..0.5f64, 0..10u32, prop::option::of(0..60000u64)).prop_map(
        |(lat, err, events, recovery)| DisruptionAccounting {
            latency_increase_ms: lat,
            error_rate_increase: err,
            disruption_events: events,
            recovery_time_ms: recovery,
        },
    )
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_drill_type(dt in arb_drill_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let back: DrillType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dt, back);
    }

    #[test]
    fn serde_roundtrip_trigger_type(tt in arb_rollback_trigger_type()) {
        let json = serde_json::to_string(&tt).unwrap();
        let back: RollbackTriggerType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tt, back);
    }

    #[test]
    fn serde_roundtrip_verdict(v in arb_rehearsal_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: RehearsalVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    #[test]
    fn serde_roundtrip_disruption_accounting(da in arb_disruption_accounting()) {
        let json = serde_json::to_string(&da).unwrap();
        let back: DisruptionAccounting = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(da.latency_increase_ms, back.latency_increase_ms);
        prop_assert_eq!(da.disruption_events, back.disruption_events);
    }
}

// =============================================================================
// DrillType invariants
// =============================================================================

proptest! {
    #[test]
    fn drill_type_label_nonempty(dt in arb_drill_type()) {
        prop_assert!(!dt.label().is_empty());
    }
}

// =============================================================================
// Cohort definition invariants
// =============================================================================

proptest! {
    #[test]
    fn cohort_member_count_matches(
        members in prop::collection::vec("[a-z]{3,8}", 0..10)
    ) {
        let mut cohort = CohortDefinition::new("test", 0.1, 0);
        for m in &members {
            cohort.add_member(m);
        }
        prop_assert_eq!(cohort.member_count(), members.len());
    }

    #[test]
    fn cohort_fraction_preserved(frac in 0.0..1.0f64) {
        let cohort = CohortDefinition::new("test", frac, 0);
        prop_assert!((cohort.fraction - frac).abs() < 1e-10);
    }
}

// =============================================================================
// DrillResult step counting
// =============================================================================

proptest! {
    #[test]
    fn drill_step_counts_sum_to_total(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut steps = Vec::new();
        for i in 0..n_pass {
            steps.push(DrillStep::pass(format!("p-{i}"), "ok", 100));
        }
        for i in 0..n_fail {
            steps.push(DrillStep::fail(format!("f-{i}"), "fail", 100, "err"));
        }

        let result = DrillResult {
            drill_id: "test".into(),
            drill_type: DrillType::Promotion,
            cohort_id: "c1".into(),
            started_at_ms: 0,
            ended_at_ms: 1000,
            steps,
            disruption: DisruptionAccounting::zero(),
            success: n_fail == 0,
            failure_reason: if n_fail > 0 { Some("failures".into()) } else { None },
        };

        prop_assert_eq!(result.steps_passed(), n_pass);
        prop_assert_eq!(result.steps_failed(), n_fail);

        if total > 0 {
            let rate = result.step_pass_rate();
            let expected = n_pass as f64 / total as f64;
            prop_assert!((rate - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn drill_elapsed_time_correct(
        start in 0..10000u64,
        duration in 0..5000u64,
    ) {
        let result = DrillResult {
            drill_id: "test".into(),
            drill_type: DrillType::Rollback,
            cohort_id: "c1".into(),
            started_at_ms: start,
            ended_at_ms: start + duration,
            steps: vec![DrillStep::pass("s1", "ok", 100)],
            disruption: DisruptionAccounting::zero(),
            success: true,
            failure_reason: None,
        };
        prop_assert_eq!(result.total_elapsed_ms(), duration);
    }
}

// =============================================================================
// Disruption budget enforcement
// =============================================================================

proptest! {
    #[test]
    fn zero_disruption_within_any_budget(
        budget_lat in 0..1000u64,
        budget_err in 0.0..1.0f64,
        budget_events in 0..10u32,
        budget_recovery in 0..60000u64,
    ) {
        let budget = DisruptionBudget {
            max_latency_increase_ms: budget_lat,
            max_error_rate_increase: budget_err,
            max_disruption_events: budget_events,
            max_recovery_time_ms: budget_recovery,
        };
        let zero = DisruptionAccounting::zero();
        prop_assert!(zero.within_budget(&budget),
            "zero disruption should always be within budget");
    }

    #[test]
    fn over_budget_detected(
        budget_lat in 1..100u64,
    ) {
        let budget = DisruptionBudget {
            max_latency_increase_ms: budget_lat,
            max_error_rate_increase: 0.01,
            max_disruption_events: 1,
            max_recovery_time_ms: 1000,
        };
        let over = DisruptionAccounting {
            latency_increase_ms: budget_lat + 1,
            error_rate_increase: 0.0,
            disruption_events: 0,
            recovery_time_ms: None,
        };
        prop_assert!(!over.within_budget(&budget),
            "over-budget latency should be detected");
    }
}

// =============================================================================
// RehearsalPlan invariants
// =============================================================================

#[test]
fn standard_plan_has_cohorts_and_drills() {
    let plan = RehearsalPlan::standard();
    assert!(!plan.cohorts.is_empty());
    assert!(!plan.drill_types.is_empty());
    assert!(!plan.rollback_triggers.is_empty());
    assert!(plan.total_drill_count() > 0);
}

#[test]
fn production_plan_has_stricter_criteria() {
    let standard = RehearsalPlan::standard();
    let production = RehearsalPlan::production();
    assert!(
        production.promotion_criteria.min_pass_rate >= standard.promotion_criteria.min_pass_rate
    );
}

proptest! {
    #[test]
    fn plan_total_drills_is_cohorts_times_types(
        n_cohorts in 1..5usize,
        n_types in 1..4usize,
    ) {
        let cohorts: Vec<CohortDefinition> = (0..n_cohorts)
            .map(|i| CohortDefinition::new(format!("c-{i}"), 0.1, i as u32))
            .collect();
        let types: Vec<DrillType> = vec![
            DrillType::Promotion,
            DrillType::Rollback,
            DrillType::FailSafe,
            DrillType::FullRollout,
        ][..n_types]
            .to_vec();

        let plan = RehearsalPlan {
            plan_id: "test".into(),
            cohorts,
            promotion_criteria: PromotionCriteria::rehearsal(),
            rollback_triggers: Vec::new(),
            disruption_budget: DisruptionBudget::rehearsal(),
            drill_types: types,
        };

        prop_assert_eq!(plan.total_drill_count(), n_cohorts * n_types);
    }
}

// =============================================================================
// RehearsalReport invariants
// =============================================================================

proptest! {
    #[test]
    fn report_drill_counts_sum(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut drills = Vec::new();
        for i in 0..n_pass {
            drills.push(DrillResult {
                drill_id: format!("p-{i}"),
                drill_type: DrillType::Promotion,
                cohort_id: "c1".into(),
                started_at_ms: 0,
                ended_at_ms: 1000,
                steps: vec![DrillStep::pass("s1", "ok", 100)],
                disruption: DisruptionAccounting::zero(),
                success: true,
                failure_reason: None,
            });
        }
        for i in 0..n_fail {
            drills.push(DrillResult {
                drill_id: format!("f-{i}"),
                drill_type: DrillType::Rollback,
                cohort_id: "c1".into(),
                started_at_ms: 0,
                ended_at_ms: 1000,
                steps: vec![DrillStep::fail("s1", "fail", 100, "err")],
                disruption: DisruptionAccounting::zero(),
                success: false,
                failure_reason: Some("err".into()),
            });
        }

        let budget = DisruptionBudget::rehearsal();
        let report = RehearsalReport::from_drills("plan-1", 0, drills, &budget);

        prop_assert_eq!(report.drills_passed(), n_pass);
        prop_assert_eq!(report.drills_failed(), n_fail);

        if total > 0 {
            let rate = report.drill_pass_rate();
            let expected = n_pass as f64 / total as f64;
            prop_assert!((rate - expected).abs() < 1e-10);
        }
    }
}

// =============================================================================
// Rollback trigger evaluation
// =============================================================================

proptest! {
    #[test]
    fn trigger_fires_when_exceeded(
        threshold in 0.01..0.5f64,
        observed_over in 0.01..0.5f64,
    ) {
        let trigger = RollbackTrigger {
            trigger_id: "test".into(),
            description: "test trigger".into(),
            trigger_type: RollbackTriggerType::ErrorRateSpike,
            threshold,
            window_ms: 60000,
        };

        let metrics = ObservedMetrics {
            error_rate: threshold + observed_over,
            p95_latency_ms: 0.0,
            crash_rate: 0.0,
            disruption_events: 0,
            health_check_failure_rate: 0.0,
            operator_triggered: false,
        };

        let fired = evaluate_rollback_triggers(&[trigger], &metrics);
        prop_assert!(!fired.is_empty(), "trigger should fire when threshold exceeded");
        prop_assert!(fired[0].observed_value > fired[0].threshold);
    }
}

// =============================================================================
// Telemetry
// =============================================================================

#[test]
fn telemetry_default_is_zeroed() {
    let t = RehearsalTelemetry::new();
    assert_eq!(t.rehearsals_executed, 0);
    assert_eq!(t.total_drills, 0);
}

#[test]
fn report_summary_renders() {
    let drills = vec![DrillResult {
        drill_id: "d1".into(),
        drill_type: DrillType::Promotion,
        cohort_id: "c1".into(),
        started_at_ms: 0,
        ended_at_ms: 1000,
        steps: vec![DrillStep::pass("s1", "ok", 100)],
        disruption: DisruptionAccounting::zero(),
        success: true,
        failure_reason: None,
    }];
    let budget = DisruptionBudget::rehearsal();
    let report = RehearsalReport::from_drills("plan-1", 0, drills, &budget);
    let summary = report.render_summary();
    assert!(!summary.is_empty());
}
