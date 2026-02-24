//! Property-based tests for replay_usability_pilot (ft-og6q6.7.8).
//!
//! Invariants tested:
//! - UP-1: PilotScenario str roundtrip
//! - UP-2: Success count + failure count + skipped <= total
//! - UP-3: Error rate in [0, 1]
//! - UP-4: Confusion rate in [0, 1]
//! - UP-5: Within-time-budget count <= total scenarios
//! - UP-6: Empty log has zero metrics
//! - UP-7: All-success log has 100% success rate
//! - UP-8: Evaluation with 100% success passes default criteria
//! - UP-9: Friction points count matches sum across results
//! - UP-10: Improvement items have IMP- prefix IDs
//! - UP-11: FeedbackLog serde roundtrip
//! - UP-12: PilotMetrics serde roundtrip
//! - UP-13: PilotEvaluation serde roundtrip
//! - UP-14: ScenarioValidation map has 6 entries
//! - UP-15: Scenario max_duration always positive
//! - UP-16: SuccessCriteria default has valid ranges
//! - UP-17: Summary report contains heading
//! - UP-18: Improvements sorted by priority
//! - UP-19: Multiple failures exceed success rate threshold
//! - UP-20: ScenarioResult time budget check consistent

use proptest::prelude::*;

use frankenterm_core::replay_usability_pilot::{
    PilotScenario, ALL_SCENARIOS, ScenarioOutcome, ScenarioResult,
    FeedbackLog, FrictionPoint, FrictionCategory, Participant, ParticipantType,
    calculate_metrics, evaluate_pilot, SuccessCriteria, pilot_summary_report,
    validate_scenario_interfaces, extract_improvements, PilotMetrics, PilotEvaluation,
};

fn arb_scenario() -> impl Strategy<Value = PilotScenario> {
    (0usize..6).prop_map(|i| ALL_SCENARIOS[i])
}

fn arb_outcome() -> impl Strategy<Value = ScenarioOutcome> {
    prop_oneof![
        Just(ScenarioOutcome::Success),
        Just(ScenarioOutcome::SuccessWithFriction),
        Just(ScenarioOutcome::Failed),
        Just(ScenarioOutcome::Skipped),
    ]
}

fn arb_result() -> impl Strategy<Value = ScenarioResult> {
    (arb_scenario(), arb_outcome(), 1u64..600).prop_map(|(scenario, outcome, dur)| {
        ScenarioResult {
            scenario,
            participant_id: "OP-001".into(),
            outcome,
            duration_secs: dur,
            errors: if outcome == ScenarioOutcome::Failed { vec!["error".into()] } else { vec![] },
            friction_points: vec![],
            notes: None,
        }
    })
}

fn arb_log(count: usize) -> impl Strategy<Value = FeedbackLog> {
    proptest::collection::vec(arb_result(), count..=count).prop_map(|results| {
        let mut log = FeedbackLog::new("PILOT-T", "2026-01-01T00:00:00Z");
        log.add_participant(Participant {
            id: "OP-001".into(),
            participant_type: ParticipantType::HumanOperator,
            name: None,
        });
        for r in results {
            log.add_result(r);
        }
        log
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── UP-1: Scenario str roundtrip ────────────────────────────────────────

    #[test]
    fn up01_scenario_roundtrip(idx in 0usize..6) {
        let scenario = ALL_SCENARIOS[idx];
        let s = scenario.as_str();
        let parsed = PilotScenario::from_str_scenario(s);
        prop_assert_eq!(parsed, Some(scenario));
    }

    // ── UP-2: Success + failure + skipped <= total ──────────────────────────

    #[test]
    fn up02_count_invariant(log in arb_log(5)) {
        let metrics = calculate_metrics(&log);
        prop_assert!(metrics.successful + metrics.failed + metrics.skipped <= metrics.total_scenarios);
    }

    // ── UP-3: Error rate in [0, 1] ──────────────────────────────────────────

    #[test]
    fn up03_error_rate_bounded(log in arb_log(5)) {
        let rate = log.error_rate();
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0);
    }

    // ── UP-4: Confusion rate in [0, 1] ──────────────────────────────────────

    #[test]
    fn up04_confusion_rate_bounded(log in arb_log(5)) {
        let rate = log.confusion_rate();
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0);
    }

    // ── UP-5: Within-time-budget <= total ───────────────────────────────────

    #[test]
    fn up05_time_budget_bounded(log in arb_log(5)) {
        let metrics = calculate_metrics(&log);
        prop_assert!(metrics.within_time_budget <= metrics.total_scenarios);
    }

    // ── UP-6: Empty log zero metrics ────────────────────────────────────────

    #[test]
    fn up06_empty_log(_dummy in 0u8..1) {
        let log = FeedbackLog::new("P1", "now");
        let metrics = calculate_metrics(&log);
        prop_assert_eq!(metrics.total_scenarios, 0);
        prop_assert_eq!(metrics.successful, 0);
        prop_assert!((metrics.avg_duration_secs - 0.0).abs() < f64::EPSILON);
    }

    // ── UP-7: All success = 100% rate ───────────────────────────────────────

    #[test]
    fn up07_all_success(count in 1usize..10) {
        let mut log = FeedbackLog::new("P1", "now");
        for _ in 0..count {
            log.add_result(ScenarioResult {
                scenario: PilotScenario::CaptureSession,
                participant_id: "OP-001".into(),
                outcome: ScenarioOutcome::Success,
                duration_secs: 60,
                errors: vec![],
                friction_points: vec![],
                notes: None,
            });
        }
        let metrics = calculate_metrics(&log);
        prop_assert_eq!(metrics.successful, count);
        prop_assert_eq!(metrics.failed, 0);
    }

    // ── UP-8: All success passes default criteria ───────────────────────────

    #[test]
    fn up08_all_success_passes(count in 1usize..10) {
        let mut log = FeedbackLog::new("P1", "now");
        for _ in 0..count {
            log.add_result(ScenarioResult {
                scenario: PilotScenario::CaptureSession,
                participant_id: "OP-001".into(),
                outcome: ScenarioOutcome::Success,
                duration_secs: 60,
                errors: vec![],
                friction_points: vec![],
                notes: None,
            });
        }
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        prop_assert!(eval.passed);
    }

    // ── UP-9: Friction point count matches ──────────────────────────────────

    #[test]
    fn up09_friction_count(friction_per_result in 0usize..3, result_count in 1usize..5) {
        let mut log = FeedbackLog::new("P1", "now");
        for _ in 0..result_count {
            let fps: Vec<FrictionPoint> = (0..friction_per_result).map(|i| FrictionPoint {
                category: FrictionCategory::ConfusingOutput,
                description: format!("friction {}", i),
                severity: None,
                suggested_fix: None,
            }).collect();
            log.add_result(ScenarioResult {
                scenario: PilotScenario::CaptureSession,
                participant_id: "OP-001".into(),
                outcome: ScenarioOutcome::Success,
                duration_secs: 60,
                errors: vec![],
                friction_points: fps,
                notes: None,
            });
        }
        let metrics = calculate_metrics(&log);
        prop_assert_eq!(metrics.friction_point_count, friction_per_result * result_count);
    }

    // ── UP-10: Improvement IDs have prefix ──────────────────────────────────

    #[test]
    fn up10_improvement_ids(count in 1usize..5) {
        let mut log = FeedbackLog::new("P1", "now");
        for i in 0..count {
            log.add_result(ScenarioResult {
                scenario: PilotScenario::CaptureSession,
                participant_id: "OP-001".into(),
                outcome: ScenarioOutcome::SuccessWithFriction,
                duration_secs: 60,
                errors: vec![],
                friction_points: vec![FrictionPoint {
                    category: FrictionCategory::MissingFeature,
                    description: format!("missing feature {}", i),
                    severity: None,
                    suggested_fix: None,
                }],
                notes: None,
            });
        }
        let items = extract_improvements(&log);
        for item in &items {
            let has_prefix = item.id.starts_with("IMP-");
            prop_assert!(has_prefix);
        }
    }

    // ── UP-11: FeedbackLog serde ────────────────────────────────────────────

    #[test]
    fn up11_log_serde(log in arb_log(3)) {
        let json = serde_json::to_string(&log).unwrap();
        let restored: FeedbackLog = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, log);
    }

    // ── UP-12: PilotMetrics serde ───────────────────────────────────────────

    #[test]
    fn up12_metrics_serde(log in arb_log(3)) {
        let metrics = calculate_metrics(&log);
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: PilotMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.total_scenarios, metrics.total_scenarios);
        prop_assert_eq!(restored.successful, metrics.successful);
        // f64 fields: use tolerance
        prop_assert!((restored.error_rate - metrics.error_rate).abs() < 1e-10);
        prop_assert!((restored.avg_duration_secs - metrics.avg_duration_secs).abs() < 1e-10);
    }

    // ── UP-13: PilotEvaluation serde ────────────────────────────────────────

    #[test]
    fn up13_eval_serde(log in arb_log(3)) {
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        let json = serde_json::to_string(&eval).unwrap();
        let restored: PilotEvaluation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.passed, eval.passed);
        prop_assert_eq!(restored.violations.len(), eval.violations.len());
    }

    // ── UP-14: Validation map has 6 entries ─────────────────────────────────

    #[test]
    fn up14_validation_map(_dummy in 0u8..1) {
        let validations = validate_scenario_interfaces();
        prop_assert_eq!(validations.len(), 6);
    }

    // ── UP-15: Scenario max_duration positive ───────────────────────────────

    #[test]
    fn up15_max_duration_positive(idx in 0usize..6) {
        let scenario = ALL_SCENARIOS[idx];
        prop_assert!(scenario.max_duration_secs() > 0);
    }

    // ── UP-16: SuccessCriteria default valid ────────────────────────────────

    #[test]
    fn up16_criteria_valid(_dummy in 0u8..1) {
        let criteria = SuccessCriteria::default();
        prop_assert!(criteria.max_error_rate > 0.0);
        prop_assert!(criteria.max_error_rate <= 1.0);
        prop_assert!(criteria.max_confusion_rate > 0.0);
        prop_assert!(criteria.max_confusion_rate <= 1.0);
        prop_assert!(criteria.min_success_rate > 0.0);
        prop_assert!(criteria.min_success_rate <= 1.0);
    }

    // ── UP-17: Summary report has heading ───────────────────────────────────

    #[test]
    fn up17_report_heading(log in arb_log(3)) {
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        let report = pilot_summary_report(&eval);
        prop_assert!(report.contains("# Replay Usability Pilot Report"));
    }

    // ── UP-18: Improvements sorted by priority ──────────────────────────────

    #[test]
    fn up18_improvements_sorted(_dummy in 0u8..1) {
        let mut log = FeedbackLog::new("P1", "now");
        // Add varied friction categories
        log.add_result(ScenarioResult {
            scenario: PilotScenario::CaptureSession,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::SuccessWithFriction,
            duration_secs: 60,
            errors: vec![],
            friction_points: vec![
                FrictionPoint { category: FrictionCategory::ConfusingOutput, description: "a".into(), severity: None, suggested_fix: None },
                FrictionPoint { category: FrictionCategory::UnclearErrorMessage, description: "b".into(), severity: None, suggested_fix: None },
            ],
            notes: None,
        });
        let items = extract_improvements(&log);
        if items.len() >= 2 {
            // High priority should come before Low
            let p0 = match items[0].priority {
                frankenterm_core::replay_usability_pilot::ImprovementPriority::Critical => 0,
                frankenterm_core::replay_usability_pilot::ImprovementPriority::High => 1,
                frankenterm_core::replay_usability_pilot::ImprovementPriority::Medium => 2,
                frankenterm_core::replay_usability_pilot::ImprovementPriority::Low => 3,
            };
            let p1 = match items[1].priority {
                frankenterm_core::replay_usability_pilot::ImprovementPriority::Critical => 0,
                frankenterm_core::replay_usability_pilot::ImprovementPriority::High => 1,
                frankenterm_core::replay_usability_pilot::ImprovementPriority::Medium => 2,
                frankenterm_core::replay_usability_pilot::ImprovementPriority::Low => 3,
            };
            prop_assert!(p0 <= p1);
        }
    }

    // ── UP-19: Multiple failures violate success rate ───────────────────────

    #[test]
    fn up19_failures_violate(fail_count in 5usize..10) {
        let mut log = FeedbackLog::new("P1", "now");
        // Add 1 success and many failures
        log.add_result(ScenarioResult {
            scenario: PilotScenario::CaptureSession,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::Success,
            duration_secs: 60,
            errors: vec![],
            friction_points: vec![],
            notes: None,
        });
        for _ in 0..fail_count {
            log.add_result(ScenarioResult {
                scenario: PilotScenario::ReplayTrace,
                participant_id: "OP-001".into(),
                outcome: ScenarioOutcome::Failed,
                duration_secs: 100,
                errors: vec!["crash".into()],
                friction_points: vec![],
                notes: None,
            });
        }
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        let is_passed = eval.passed;
        prop_assert!(!is_passed);
    }

    // ── UP-20: Time budget check consistent ─────────────────────────────────

    #[test]
    fn up20_time_budget_consistent(dur in 1u64..600, idx in 0usize..6) {
        let scenario = ALL_SCENARIOS[idx];
        let result = ScenarioResult {
            scenario,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::Success,
            duration_secs: dur,
            errors: vec![],
            friction_points: vec![],
            notes: None,
        };
        let within = result.is_within_time_budget();
        let expected = dur <= scenario.max_duration_secs();
        prop_assert_eq!(within, expected);
    }
}
