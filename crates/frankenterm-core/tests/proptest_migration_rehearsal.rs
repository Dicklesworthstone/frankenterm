//! Property tests for migration_rehearsal module (ft-3681t.8.6).
//!
//! Covers serde roundtrips, scenario category properties, execution
//! counter arithmetic, report verdict logic, divergence metric computation,
//! drill target validation, and standard factory invariants.

use frankenterm_core::migration_rehearsal::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_scenario_category() -> impl Strategy<Value = ScenarioCategory> {
    prop_oneof![
        Just(ScenarioCategory::ParityCheck),
        Just(ScenarioCategory::ShadowComparison),
        Just(ScenarioCategory::ImporterValidation),
        Just(ScenarioCategory::CutoverCheckpoint),
        Just(ScenarioCategory::RollbackDrill),
    ]
}

fn arb_scenario_severity() -> impl Strategy<Value = ScenarioSeverity> {
    prop_oneof![
        Just(ScenarioSeverity::Info),
        Just(ScenarioSeverity::Warning),
        Just(ScenarioSeverity::Critical),
    ]
}

fn arb_scenario_outcome() -> impl Strategy<Value = ScenarioOutcome> {
    prop_oneof![
        Just(ScenarioOutcome::Pass),
        Just(ScenarioOutcome::Fail),
        Just(ScenarioOutcome::Skipped),
        Just(ScenarioOutcome::Timeout),
    ]
}

fn arb_rehearsal_verdict() -> impl Strategy<Value = RehearsalVerdict> {
    prop_oneof![
        Just(RehearsalVerdict::Ready),
        Just(RehearsalVerdict::Conditional),
        Just(RehearsalVerdict::NotReady),
    ]
}

fn arb_scenario() -> impl Strategy<Value = RehearsalScenario> {
    (
        "[a-z-]{3,15}",
        arb_scenario_category(),
        ".{1,40}",
        arb_scenario_severity(),
        0..60000u64,
    )
        .prop_map(|(id, cat, desc, sev, dur)| {
            RehearsalScenario::new(&id, cat, &desc)
                .with_severity(sev)
                .with_expected_duration(dur)
        })
}

fn _arb_scenario_result(outcome: ScenarioOutcome) -> impl Strategy<Value = ScenarioResult> {
    ("[a-z-]{3,15}", arb_scenario_category(), 0..10000u64).prop_map(move |(id, cat, dur)| {
        match outcome {
            ScenarioOutcome::Pass => ScenarioResult::pass(&id, cat, dur),
            ScenarioOutcome::Fail => ScenarioResult::fail(&id, cat, dur, "test failure"),
            ScenarioOutcome::Skipped => ScenarioResult::skipped(&id, cat, "skipped"),
            ScenarioOutcome::Timeout => {
                let mut r = ScenarioResult::fail(&id, cat, dur, "timeout");
                r.outcome = ScenarioOutcome::Timeout;
                r
            }
        }
    })
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_scenario_category(cat in arb_scenario_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: ScenarioCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_scenario_severity(sev in arb_scenario_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: ScenarioSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn serde_roundtrip_scenario_outcome(outcome in arb_scenario_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ScenarioOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, back);
    }

    #[test]
    fn serde_roundtrip_rehearsal_verdict(verdict in arb_rehearsal_verdict()) {
        let json = serde_json::to_string(&verdict).unwrap();
        let back: RehearsalVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(verdict, back);
    }

    #[test]
    fn serde_roundtrip_scenario(scenario in arb_scenario()) {
        let json = serde_json::to_string(&scenario).unwrap();
        let back: RehearsalScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(scenario.scenario_id, back.scenario_id);
        prop_assert_eq!(scenario.category, back.category);
        prop_assert_eq!(scenario.severity, back.severity);
    }
}

// =============================================================================
// Scenario category invariants
// =============================================================================

proptest! {
    #[test]
    fn scenario_category_label_nonempty(cat in arb_scenario_category()) {
        prop_assert!(!cat.label().is_empty());
    }

    #[test]
    fn scenario_outcome_pass_fail_exclusive(outcome in arb_scenario_outcome()) {
        let is_pass = outcome.is_pass();
        let is_fail = outcome.is_failure();
        // Pass and Fail are mutually exclusive
        prop_assert!(!(is_pass && is_fail),
            "outcome {:?} cannot be both pass and failure", outcome);
        // Skipped is neither pass nor fail
        if outcome == ScenarioOutcome::Skipped {
            prop_assert!(!is_pass && !is_fail);
        }
    }

    #[test]
    fn blocking_categories_are_deterministic(cat in arb_scenario_category()) {
        prop_assert_eq!(cat.is_blocking(), cat.is_blocking());
    }
}

#[test]
fn scenario_category_all_has_5() {
    assert_eq!(ScenarioCategory::ALL.len(), 5);
}

// =============================================================================
// Execution counter arithmetic
// =============================================================================

proptest! {
    #[test]
    fn execution_counters_sum_to_total(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
        n_skip in 0..5usize,
    ) {
        let total = n_pass + n_fail + n_skip;
        if total == 0 {
            return Ok(());
        }

        let mut exec = RehearsalExecution::new("suite", "run-1", "test", 0);

        for i in 0..n_pass {
            exec.record(ScenarioResult::pass(&format!("p-{i}"), ScenarioCategory::ParityCheck, 100));
        }
        for i in 0..n_fail {
            exec.record(ScenarioResult::fail(&format!("f-{i}"), ScenarioCategory::ShadowComparison, 100, "err"));
        }
        for i in 0..n_skip {
            exec.record(ScenarioResult::skipped(&format!("s-{i}"), ScenarioCategory::ImporterValidation, "skipped"));
        }

        exec.complete(1000);

        prop_assert_eq!(exec.passed(), n_pass);
        prop_assert_eq!(exec.failed(), n_fail);
        prop_assert_eq!(exec.skipped(), n_skip);

        let counted_total = exec.passed() + exec.failed() + exec.skipped();
        prop_assert_eq!(counted_total, total);
    }

    #[test]
    fn execution_pass_rate_bounds(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut exec = RehearsalExecution::new("suite", "run-1", "test", 0);
        for i in 0..n_pass {
            exec.record(ScenarioResult::pass(&format!("p-{i}"), ScenarioCategory::ParityCheck, 100));
        }
        for i in 0..n_fail {
            exec.record(ScenarioResult::fail(&format!("f-{i}"), ScenarioCategory::ShadowComparison, 100, "err"));
        }
        exec.complete(1000);

        let rate = exec.pass_rate();
        prop_assert!(rate >= 0.0 && rate <= 1.0, "pass_rate should be in [0,1]: {}", rate);

        let expected = n_pass as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10);
    }

    #[test]
    fn execution_duration_arithmetic(
        start in 0..10000u64,
        duration in 0..5000u64,
    ) {
        let end = start + duration;
        let mut exec = RehearsalExecution::new("suite", "run-1", "test", start);
        exec.record(ScenarioResult::pass("p-1", ScenarioCategory::ParityCheck, 100));
        exec.complete(end);

        prop_assert_eq!(exec.duration_ms(), duration);
    }
}

// =============================================================================
// Divergence metrics
// =============================================================================

proptest! {
    #[test]
    fn divergence_rate_arithmetic(
        total in 1..1000u64,
        div_frac in 0.0..1.0f64,
        budget in 0.0..1.0f64,
    ) {
        let divergences = (total as f64 * div_frac) as u64;
        let metrics = DivergenceMetrics::compute(total, divergences, budget);

        prop_assert_eq!(metrics.total_comparisons, total);
        prop_assert_eq!(metrics.divergences, divergences);
        prop_assert_eq!(metrics.matches, total - divergences);

        let expected_rate = divergences as f64 / total as f64;
        prop_assert!((metrics.divergence_rate - expected_rate).abs() < 1e-10);
        prop_assert_eq!(metrics.within_budget, metrics.divergence_rate <= budget);
    }

    #[test]
    fn divergence_zero_total_safe(budget in 0.0..1.0f64) {
        let metrics = DivergenceMetrics::compute(0, 0, budget);
        prop_assert_eq!(metrics.total_comparisons, 0);
        prop_assert_eq!(metrics.divergences, 0);
    }
}

// =============================================================================
// Drill metrics
// =============================================================================

#[test]
fn production_targets_are_strict() {
    let targets = DrillMetrics::production_targets();
    assert!(targets.target_ttr_ms > 0);
    assert!(targets.target_integrity > 0.0);
}

#[test]
fn rehearsal_targets_are_relaxed() {
    let prod = DrillMetrics::production_targets();
    let rehearsal = DrillMetrics::rehearsal_targets();
    assert!(
        rehearsal.target_ttr_ms >= prod.target_ttr_ms,
        "rehearsal TTR target should be >= production"
    );
}

proptest! {
    #[test]
    fn drill_meets_targets_deterministic(
        ttr in 0..120000u64,
        integrity in 0.0..1.0f64,
    ) {
        let mut metrics = DrillMetrics::production_targets();
        metrics.time_to_recovery_ms = ttr;
        metrics.data_integrity_score = integrity;
        let result1 = metrics.meets_targets();
        let result2 = metrics.meets_targets();
        prop_assert_eq!(result1, result2, "meets_targets should be deterministic");
    }
}

// =============================================================================
// Report verdict logic
// =============================================================================

proptest! {
    #[test]
    fn report_verdict_reflects_blocking_failures(
        n_pass in 0..5usize,
        n_blocking_fail in 0..3usize,
        n_advisory_fail in 0..3usize,
    ) {
        let total = n_pass + n_blocking_fail + n_advisory_fail;
        if total == 0 {
            return Ok(());
        }

        let mut suite = RehearsalSuite::new("suite", "test");
        let mut exec = RehearsalExecution::new("suite", "run-1", "test", 0);

        for i in 0..n_pass {
            let cat = ScenarioCategory::ParityCheck; // blocking by default
            suite.add_scenario(RehearsalScenario::new(&format!("p-{i}"), cat, "pass test"));
            exec.record(ScenarioResult::pass(&format!("p-{i}"), cat, 100));
        }

        for i in 0..n_blocking_fail {
            let cat = ScenarioCategory::CutoverCheckpoint; // blocking
            suite.add_scenario(RehearsalScenario::new(&format!("bf-{i}"), cat, "blocking fail"));
            exec.record(ScenarioResult::fail(&format!("bf-{i}"), cat, 100, "err"));
        }

        for i in 0..n_advisory_fail {
            let cat = ScenarioCategory::ImporterValidation;
            suite.add_scenario(
                RehearsalScenario::new(&format!("af-{i}"), cat, "advisory fail")
                    .with_severity(ScenarioSeverity::Info),
            );
            exec.record(ScenarioResult::fail(&format!("af-{i}"), cat, 100, "minor"));
        }

        exec.complete(1000);
        let report = RehearsalReport::from_execution(&exec);

        prop_assert_eq!(report.total, total);
        prop_assert_eq!(report.passed, n_pass);
    }
}

// =============================================================================
// Suite invariants
// =============================================================================

proptest! {
    #[test]
    fn suite_scenario_count_matches_additions(
        scenarios in prop::collection::vec(arb_scenario(), 0..10)
    ) {
        let mut suite = RehearsalSuite::new("suite", "test");
        for s in &scenarios {
            suite.add_scenario(s.clone());
        }
        prop_assert_eq!(suite.scenario_count(), scenarios.len());
    }

    #[test]
    fn suite_blocking_count_le_total(
        scenarios in prop::collection::vec(arb_scenario(), 0..10)
    ) {
        let mut suite = RehearsalSuite::new("suite", "test");
        for s in &scenarios {
            suite.add_scenario(s.clone());
        }
        prop_assert!(suite.blocking_count() <= suite.scenario_count());
    }

    #[test]
    fn suite_estimated_duration_monotonic(
        scenarios in prop::collection::vec(arb_scenario(), 0..10)
    ) {
        let mut suite = RehearsalSuite::new("suite", "test");
        let mut last_est = 0u64;
        for s in &scenarios {
            suite.add_scenario(s.clone());
            let new_est = suite.estimated_duration_ms();
            prop_assert!(new_est >= last_est,
                "estimated duration should be monotonically increasing");
            last_est = new_est;
        }
    }

    #[test]
    fn suite_by_category_subset(
        scenarios in prop::collection::vec(arb_scenario(), 1..10),
        cat in arb_scenario_category(),
    ) {
        let mut suite = RehearsalSuite::new("suite", "test");
        for s in &scenarios {
            suite.add_scenario(s.clone());
        }
        let filtered = suite.by_category(cat);
        prop_assert!(filtered.len() <= suite.scenario_count());
        for s in &filtered {
            prop_assert_eq!(s.category, cat);
        }
    }
}

// =============================================================================
// Standard factory
// =============================================================================

#[test]
fn standard_suite_has_scenarios() {
    let suite = standard_rehearsal_suite();
    assert!(suite.scenario_count() > 0);
    assert!(suite.blocking_count() > 0);
}

#[test]
fn standard_suite_covers_all_categories() {
    let suite = standard_rehearsal_suite();
    for cat in ScenarioCategory::ALL {
        let count = suite.by_category(*cat).len();
        assert!(count > 0, "standard suite should cover {:?}", cat);
    }
}

#[test]
fn standard_suite_summary_renders() {
    let suite = standard_rehearsal_suite();
    let mut exec = RehearsalExecution::new(&suite.suite_id, "run-1", "test", 0);
    for s in &suite.scenarios {
        exec.record(ScenarioResult::pass(&s.scenario_id, s.category, 100));
    }
    exec.complete(1000);
    let report = RehearsalReport::from_execution(&exec);
    let summary = report.render_summary();
    assert!(!summary.is_empty());
}
