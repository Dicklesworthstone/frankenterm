//! Property-based tests for replay_cli serde types and aggregation helpers.

use std::collections::BTreeSet;

use frankenterm_core::replay_cli::{
    ArtifactResult, EquivalenceLevelArg, InspectResult, RegressionSuiteResult, ReplayExitCode,
    ReplayOutputMode, SpeedArg,
};
use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionType};
use proptest::prelude::*;

fn arb_output_mode() -> impl Strategy<Value = ReplayOutputMode> {
    prop_oneof![
        Just(ReplayOutputMode::Human),
        Just(ReplayOutputMode::Robot),
        Just(ReplayOutputMode::Verbose),
        Just(ReplayOutputMode::Quiet),
    ]
}

fn arb_exit_code() -> impl Strategy<Value = ReplayExitCode> {
    prop_oneof![
        Just(ReplayExitCode::Pass),
        Just(ReplayExitCode::Regression),
        Just(ReplayExitCode::InvalidInput),
        Just(ReplayExitCode::InternalError),
    ]
}

fn arb_equivalence_arg() -> impl Strategy<Value = EquivalenceLevelArg> {
    prop_oneof![
        Just(EquivalenceLevelArg::Structural),
        Just(EquivalenceLevelArg::Decision),
        Just(EquivalenceLevelArg::Full),
    ]
}

fn arb_speed_arg() -> impl Strategy<Value = SpeedArg> {
    prop_oneof![
        Just(SpeedArg::Normal),
        Just(SpeedArg::Double),
        Just(SpeedArg::Instant),
        (0.0f64..100.0f64)
            .prop_filter("finite custom speed", |v| v.is_finite())
            .prop_map(SpeedArg::Custom),
    ]
}

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyDecision),
        Just(DecisionType::AlertFired),
        Just(DecisionType::OverrideApplied),
        Just(DecisionType::BarrierDecision),
        Just(DecisionType::NoOp),
        Just(DecisionType::PolicyEvaluation),
    ]
}

fn arb_decision_event() -> impl Strategy<Value = DecisionEvent> {
    (
        arb_decision_type(),
        "[a-z][a-z0-9_.-]{2,20}",
        "[0-9a-f]{16}",
        "[0-9a-f]{16}",
        "[a-zA-Z0-9 _-]{0,40}",
        "[0-9a-f]{16}",
        any::<u64>(),
        any::<u64>(),
        prop::option::of("[a-z0-9_-]{1,20}"),
        prop::option::of(0.0f64..1.0f64),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        any::<u64>(),
        "[a-z0-9_-]{0,20}",
    )
        .prop_map(
            |(
                decision_type,
                rule_id,
                definition_hash,
                input_hash,
                input_summary,
                output_hash,
                timestamp_ms,
                pane_id,
                parent_event_id,
                confidence,
                triggered_by,
                overrides,
                wall_clock_ms,
                replay_run_id,
            )| DecisionEvent {
                decision_type,
                rule_id,
                definition_hash,
                input_hash,
                input_summary,
                output_hash,
                timestamp_ms,
                pane_id,
                parent_event_id,
                confidence,
                triggered_by,
                overrides,
                wall_clock_ms,
                replay_run_id,
            },
        )
}

fn arb_artifact_result() -> impl Strategy<Value = ArtifactResult> {
    (
        ".{1,40}",
        any::<bool>(),
        ".{0,40}",
        prop::option::of(".{1,40}"),
    )
        .prop_map(
            |(artifact_path, passed, gate_result_summary, error)| ArtifactResult {
                artifact_path,
                passed,
                gate_result_summary,
                error,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn prop_output_mode_serde_roundtrip(mode in arb_output_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: ReplayOutputMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, mode);
    }

    #[test]
    fn prop_exit_code_serde_roundtrip(code in arb_exit_code()) {
        let json = serde_json::to_string(&code).unwrap();
        let back: ReplayExitCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, code);
    }

    #[test]
    fn prop_exit_code_numeric_mapping(code in arb_exit_code()) {
        let expected = match code {
            ReplayExitCode::Pass => 0,
            ReplayExitCode::Regression => 1,
            ReplayExitCode::InvalidInput => 2,
            ReplayExitCode::InternalError => 3,
        };
        prop_assert_eq!(code.code(), expected);
    }

    #[test]
    fn prop_equivalence_arg_serde_roundtrip(level in arb_equivalence_arg()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: EquivalenceLevelArg = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, level);
    }

    #[test]
    fn prop_speed_arg_serde_roundtrip(speed in arb_speed_arg()) {
        let json = serde_json::to_string(&speed).unwrap();
        let back: SpeedArg = serde_json::from_str(&json).unwrap();
        match (speed, back) {
            (SpeedArg::Custom(a), SpeedArg::Custom(b)) => prop_assert!((a - b).abs() < 1e-10),
            (lhs, rhs) => prop_assert_eq!(lhs, rhs),
        }
    }

    #[test]
    fn prop_speed_arg_from_str_canonical(speed in arb_speed_arg()) {
        let text = match speed {
            SpeedArg::Normal => "1x".to_string(),
            SpeedArg::Double => "2x".to_string(),
            SpeedArg::Instant => "instant".to_string(),
            SpeedArg::Custom(multiplier) => format!("{multiplier}x"),
        };
        let parsed = SpeedArg::from_str_arg(&text).unwrap();
        match (speed, parsed) {
            (SpeedArg::Custom(a), SpeedArg::Custom(b)) => prop_assert!((a - b).abs() < 1e-10),
            (lhs, rhs) => prop_assert_eq!(lhs, rhs),
        }
    }

    #[test]
    fn prop_inspect_result_from_events_counts(
        artifact_path in ".{1,30}",
        events in prop::collection::vec(arb_decision_event(), 0..20)
    ) {
        let inspect = InspectResult::from_events(&artifact_path, &events);

        let pane_count = events.iter().map(|event| event.pane_id).collect::<BTreeSet<_>>().len() as u64;
        let rule_count = events.iter().map(|event| event.rule_id.clone()).collect::<BTreeSet<_>>().len() as u64;
        let decision_types = events
            .iter()
            .map(|event| format!("{:?}", event.decision_type))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let expected_span = if events.is_empty() {
            0
        } else {
            let min_ts = events.iter().map(|event| event.timestamp_ms).min().unwrap();
            let max_ts = events.iter().map(|event| event.timestamp_ms).max().unwrap();
            max_ts - min_ts
        };

        prop_assert_eq!(inspect.artifact_path, artifact_path);
        prop_assert_eq!(inspect.event_count, events.len() as u64);
        prop_assert_eq!(inspect.pane_count, pane_count);
        prop_assert_eq!(inspect.rule_count, rule_count);
        prop_assert_eq!(inspect.time_span_ms, expected_span);
        prop_assert_eq!(inspect.decision_types, decision_types);
        prop_assert!(inspect.integrity_ok);
    }

    #[test]
    fn prop_inspect_result_serde_roundtrip(
        artifact_path in ".{1,30}",
        events in prop::collection::vec(arb_decision_event(), 0..20)
    ) {
        let inspect = InspectResult::from_events(&artifact_path, &events);
        let json = serde_json::to_string(&inspect).unwrap();
        let back: InspectResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.artifact_path, inspect.artifact_path);
        prop_assert_eq!(back.event_count, inspect.event_count);
        prop_assert_eq!(back.pane_count, inspect.pane_count);
        prop_assert_eq!(back.rule_count, inspect.rule_count);
        prop_assert_eq!(back.time_span_ms, inspect.time_span_ms);
        prop_assert_eq!(back.decision_types, inspect.decision_types);
        prop_assert_eq!(back.integrity_ok, inspect.integrity_ok);
    }

    #[test]
    fn prop_regression_suite_counts(results in prop::collection::vec(arb_artifact_result(), 0..20)) {
        let suite = RegressionSuiteResult::from_results(results.clone());
        let passed = results.iter().filter(|result| result.passed).count() as u64;
        let errored = results.iter().filter(|result| result.error.is_some()).count() as u64;
        let failed = results.len() as u64 - passed - errored;

        prop_assert_eq!(suite.total_artifacts, results.len() as u64);
        prop_assert_eq!(suite.passed, passed);
        prop_assert_eq!(suite.errored, errored);
        prop_assert_eq!(suite.failed, failed);
        prop_assert_eq!(suite.overall_pass, failed == 0 && errored == 0);
        prop_assert_eq!(suite.results.len(), results.len());
    }

    #[test]
    fn prop_regression_suite_serde_roundtrip(results in prop::collection::vec(arb_artifact_result(), 0..20)) {
        let suite = RegressionSuiteResult::from_results(results);
        let json = serde_json::to_string(&suite).unwrap();
        let back: RegressionSuiteResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_artifacts, suite.total_artifacts);
        prop_assert_eq!(back.passed, suite.passed);
        prop_assert_eq!(back.failed, suite.failed);
        prop_assert_eq!(back.errored, suite.errored);
        prop_assert_eq!(back.overall_pass, suite.overall_pass);
        prop_assert_eq!(back.results.len(), suite.results.len());
    }
}
