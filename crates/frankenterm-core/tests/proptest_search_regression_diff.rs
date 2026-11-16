//! Property-based tests for the search regression diff module.
//!
//! Covers serde roundtrip, Clone, and invariant checks for
//! `RegressionScenario`, `ScenarioOutcome`, `DiffArtifact`,
//! `ReplayGateConfig`, and `ReplayGateVerdict`.

use frankenterm_core::search::regression_diff::{
    DiffArtifact, RegressionScenario, ReplayGateConfig, ReplayGateVerdict, ScenarioOutcome,
};
use frankenterm_core::search::schema_gate::SchemaGateResult;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_regression_scenario() -> impl Strategy<Value = RegressionScenario> {
    (
        "[a-z_]{3,20}",
        proptest::collection::vec((0_u64..1000, 0.0_f32..100.0), 0..5),
        proptest::collection::vec((0_u64..1000, 0.0_f32..1.0), 0..5),
        1_usize..100,
        0.01_f32..0.5,
        1e-6_f32..0.1,
    )
        .prop_map(
            |(name, lexical, semantic, top_k, tau_tolerance, score_tolerance)| RegressionScenario {
                name,
                lexical,
                semantic,
                top_k,
                tau_tolerance,
                score_tolerance,
            },
        )
}

fn arb_scenario_outcome() -> impl Strategy<Value = ScenarioOutcome> {
    (
        "[a-z_]{3,20}",
        proptest::bool::ANY,
        -1.0_f32..1.0,
        0.0_f32..10.0,
        proptest::bool::ANY,
        proptest::option::of("[a-zA-Z ]{5,40}"),
    )
        .prop_map(
            |(name, passed, kendall_tau, max_score_diff, ranking_match, failure_reason)| {
                ScenarioOutcome {
                    name,
                    passed,
                    kendall_tau,
                    max_score_diff,
                    ranking_match,
                    failure_reason,
                }
            },
        )
}

fn arb_diff_artifact() -> impl Strategy<Value = DiffArtifact> {
    (
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z",
        proptest::collection::vec(arb_scenario_outcome(), 0..5),
    )
        .prop_map(|(run_at, outcomes)| {
            let passed = outcomes.iter().filter(|o| o.passed).count();
            let failed = outcomes.len() - passed;
            let pass_rate = if outcomes.is_empty() {
                1.0
            } else {
                passed as f32 / outcomes.len() as f32
            };
            let min_tau = outcomes
                .iter()
                .map(|o| o.kendall_tau)
                .fold(f32::INFINITY, f32::min);
            let max_score_diff = outcomes
                .iter()
                .map(|o| o.max_score_diff)
                .fold(0.0_f32, f32::max);
            DiffArtifact {
                run_at,
                outcomes,
                passed,
                failed,
                pass_rate,
                min_tau: if min_tau.is_infinite() { 1.0 } else { min_tau },
                max_score_diff,
            }
        })
}

fn arb_schema_gate_result() -> impl Strategy<Value = SchemaGateResult> {
    (
        proptest::bool::ANY,
        proptest::collection::vec("[a-z_]{3,15}", 0..3),
        proptest::collection::vec("[a-z_]{3,15}", 0..3),
        "[a-zA-Z ]{5,40}",
    )
        .prop_map(
            |(safe, missing_fields, added_fields, summary)| SchemaGateResult {
                safe,
                missing_fields,
                type_mismatches: vec![],
                added_fields,
                summary,
            },
        )
}

fn arb_replay_gate_verdict() -> impl Strategy<Value = ReplayGateVerdict> {
    (
        proptest::bool::ANY,
        arb_diff_artifact(),
        arb_schema_gate_result(),
        arb_schema_gate_result(),
        proptest::option::of("[a-zA-Z ;]{5,50}"),
    )
        .prop_map(
            |(go, regression, schema_fusion, schema_orchestration, reason)| ReplayGateVerdict {
                go,
                regression,
                schema_fusion,
                schema_orchestration,
                reason,
            },
        )
}

// =========================================================================
// Serde roundtrip tests
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// RegressionScenario serde roundtrip preserves key fields.
    #[test]
    fn prop_scenario_serde_roundtrip(scenario in arb_regression_scenario()) {
        let json = serde_json::to_string(&scenario).unwrap();
        let back: RegressionScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &scenario.name);
        prop_assert_eq!(back.top_k, scenario.top_k);
        prop_assert_eq!(back.lexical.len(), scenario.lexical.len());
        prop_assert_eq!(back.semantic.len(), scenario.semantic.len());
        // f32 tolerance for tau/score thresholds
        prop_assert!((back.tau_tolerance - scenario.tau_tolerance).abs() < 1e-6);
        prop_assert!((back.score_tolerance - scenario.score_tolerance).abs() < 1e-6);
    }

    /// ScenarioOutcome serde roundtrip preserves fields.
    #[test]
    fn prop_outcome_serde_roundtrip(outcome in arb_scenario_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ScenarioOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &outcome.name);
        prop_assert_eq!(back.passed, outcome.passed);
        prop_assert_eq!(back.ranking_match, outcome.ranking_match);
        prop_assert_eq!(&back.failure_reason, &outcome.failure_reason);
        prop_assert!((back.kendall_tau - outcome.kendall_tau).abs() < 1e-6);
        prop_assert!((back.max_score_diff - outcome.max_score_diff).abs() < 1e-6);
    }

    /// DiffArtifact serde roundtrip preserves aggregate fields.
    #[test]
    fn prop_artifact_serde_roundtrip(artifact in arb_diff_artifact()) {
        let json = serde_json::to_string(&artifact).unwrap();
        let back: DiffArtifact = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.run_at, &artifact.run_at);
        prop_assert_eq!(back.passed, artifact.passed);
        prop_assert_eq!(back.failed, artifact.failed);
        prop_assert_eq!(back.outcomes.len(), artifact.outcomes.len());
        prop_assert!((back.pass_rate - artifact.pass_rate).abs() < 1e-6);
    }

    /// ReplayGateVerdict serde roundtrip.
    #[test]
    fn prop_verdict_serde_roundtrip(verdict in arb_replay_gate_verdict()) {
        let json = serde_json::to_string(&verdict).unwrap();
        let back: ReplayGateVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.go, verdict.go);
        prop_assert_eq!(&back.reason, &verdict.reason);
        prop_assert_eq!(back.schema_fusion.safe, verdict.schema_fusion.safe);
        prop_assert_eq!(back.schema_orchestration.safe, verdict.schema_orchestration.safe);
        prop_assert_eq!(back.regression.passed, verdict.regression.passed);
    }
}

// =========================================================================
// Clone tests
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// RegressionScenario Clone preserves fields.
    #[test]
    fn prop_scenario_clone(scenario in arb_regression_scenario()) {
        let cloned = scenario.clone();
        prop_assert_eq!(&cloned.name, &scenario.name);
        prop_assert_eq!(cloned.top_k, scenario.top_k);
        prop_assert_eq!(cloned.lexical.len(), scenario.lexical.len());
    }

    /// ScenarioOutcome Clone preserves fields.
    #[test]
    fn prop_outcome_clone(outcome in arb_scenario_outcome()) {
        let cloned = outcome.clone();
        prop_assert_eq!(&cloned.name, &outcome.name);
        prop_assert_eq!(cloned.passed, outcome.passed);
    }

    /// DiffArtifact Clone preserves fields.
    #[test]
    fn prop_artifact_clone(artifact in arb_diff_artifact()) {
        let cloned = artifact.clone();
        prop_assert_eq!(cloned.passed, artifact.passed);
        prop_assert_eq!(cloned.failed, artifact.failed);
        prop_assert_eq!(cloned.outcomes.len(), artifact.outcomes.len());
    }

    /// ReplayGateVerdict Clone preserves fields.
    #[test]
    fn prop_verdict_clone(verdict in arb_replay_gate_verdict()) {
        let cloned = verdict.clone();
        prop_assert_eq!(cloned.go, verdict.go);
        prop_assert_eq!(&cloned.reason, &verdict.reason);
    }
}

// =========================================================================
// Invariant tests
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// DiffArtifact passed + failed = outcomes.len().
    #[test]
    fn prop_artifact_count_invariant(artifact in arb_diff_artifact()) {
        prop_assert_eq!(artifact.passed + artifact.failed, artifact.outcomes.len());
    }

    /// DiffArtifact pass_rate is between 0.0 and 1.0.
    #[test]
    fn prop_artifact_pass_rate_bounded(artifact in arb_diff_artifact()) {
        prop_assert!(artifact.pass_rate >= 0.0 && artifact.pass_rate <= 1.0,
            "pass_rate {} out of bounds", artifact.pass_rate);
    }

    /// RegressionScenario JSON is a valid object.
    #[test]
    fn prop_scenario_json_valid(scenario in arb_regression_scenario()) {
        let json = serde_json::to_string(&scenario).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// ReplayGateVerdict JSON is a valid object.
    #[test]
    fn prop_verdict_json_valid(verdict in arb_replay_gate_verdict()) {
        let json = serde_json::to_string(&verdict).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn replay_gate_config_default() {
    let config = ReplayGateConfig::default();
    assert!((config.min_pass_rate - 1.0).abs() < 1e-6);
    assert!(config.schema_gate_required);
}

#[test]
fn replay_gate_config_serde_roundtrip() {
    let config = ReplayGateConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let back: ReplayGateConfig = serde_json::from_str(&json).unwrap();
    assert!((back.min_pass_rate - config.min_pass_rate).abs() < 1e-6);
    assert_eq!(back.schema_gate_required, config.schema_gate_required);
}

#[test]
fn schema_gate_result_serde_roundtrip() {
    let result = SchemaGateResult {
        safe: true,
        missing_fields: vec![],
        type_mismatches: vec![],
        added_fields: vec!["new_field".to_string()],
        summary: "all good".to_string(),
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: SchemaGateResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back.safe, result.safe);
    assert_eq!(back.summary, result.summary);
    assert_eq!(back.added_fields.len(), 1);
}

#[test]
fn diff_artifact_empty_outcomes() {
    let artifact = DiffArtifact {
        run_at: "2026-03-20T00:00:00Z".to_string(),
        outcomes: vec![],
        passed: 0,
        failed: 0,
        pass_rate: 1.0,
        min_tau: 1.0,
        max_score_diff: 0.0,
    };
    let json = serde_json::to_string(&artifact).unwrap();
    let back: DiffArtifact = serde_json::from_str(&json).unwrap();
    assert!(back.outcomes.is_empty());
    assert_eq!(back.passed, 0);
}
