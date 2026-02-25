//! Property-based tests for regression diff harness + replay gate (ft-dr6zv.1.3.C2).

use proptest::prelude::*;

use frankenterm_core::search::{
    DiffArtifact, FacadeConfig, FacadeRouting, RegressionScenario, ReplayGateConfig,
    ReplayGateVerdict, default_scenarios, run_regression_suite, run_replay_gate,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_ranked_list(max_len: usize) -> impl Strategy<Value = Vec<(u64, f32)>> {
    proptest::collection::vec((1u64..500, 0.1f32..50.0), 0..=max_len)
}

fn arb_scenario() -> impl Strategy<Value = RegressionScenario> {
    (
        "[a-z]{3,10}",
        arb_ranked_list(8),
        arb_ranked_list(8),
        1usize..20,
        0.01f32..0.5,
        1e-6f32..1.0,
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

fn arb_scenarios(max_count: usize) -> impl Strategy<Value = Vec<RegressionScenario>> {
    proptest::collection::vec(arb_scenario(), 0..=max_count)
}

// ---------------------------------------------------------------------------
// RD-1: Suite never panics on arbitrary inputs
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rd_1_suite_never_panics(
        scenarios in arb_scenarios(8),
    ) {
        let _report = run_regression_suite(&scenarios, &FacadeConfig::default());
    }
}

// ---------------------------------------------------------------------------
// RD-2: pass_rate is always in [0.0, 1.0]
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rd_2_pass_rate_bounded(
        scenarios in arb_scenarios(8),
    ) {
        let report = run_regression_suite(&scenarios, &FacadeConfig::default());
        prop_assert!(
            report.artifact.pass_rate >= 0.0 && report.artifact.pass_rate <= 1.0,
            "pass_rate {} out of bounds",
            report.artifact.pass_rate
        );
    }
}

// ---------------------------------------------------------------------------
// RD-3: passed + failed == total outcomes
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rd_3_count_consistency(
        scenarios in arb_scenarios(8),
    ) {
        let report = run_regression_suite(&scenarios, &FacadeConfig::default());
        let total = report.artifact.outcomes.len();
        prop_assert_eq!(
            report.artifact.passed + report.artifact.failed,
            total,
            "passed + failed must equal total outcomes"
        );
    }
}

// ---------------------------------------------------------------------------
// RD-4: Empty corpus yields all_passed = true
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn rd_4_empty_corpus_all_passed(_seed in 0u32..100) {
        let report = run_regression_suite(&[], &FacadeConfig::default());
        prop_assert!(report.all_passed());
        prop_assert!((report.artifact.pass_rate - 1.0).abs() < 1e-6);
    }
}

// ---------------------------------------------------------------------------
// RD-5: DiffArtifact serde roundtrip preserves structure
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn rd_5_artifact_serde_roundtrip(
        scenarios in arb_scenarios(5),
    ) {
        let report = run_regression_suite(&scenarios, &FacadeConfig::default());
        let json = serde_json::to_string(&report.artifact).unwrap();
        let parsed: DiffArtifact = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.artifact.passed, parsed.passed);
        prop_assert_eq!(report.artifact.failed, parsed.failed);
        prop_assert_eq!(report.artifact.outcomes.len(), parsed.outcomes.len());
        prop_assert!((report.artifact.pass_rate - parsed.pass_rate).abs() < 1e-6);
    }
}

// ---------------------------------------------------------------------------
// RD-6: Suite forces shadow mode regardless of config routing
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn rd_6_forces_shadow(
        routing in prop_oneof![
            Just(FacadeRouting::Legacy),
            Just(FacadeRouting::Orchestrated),
            Just(FacadeRouting::Shadow),
        ],
    ) {
        let config = FacadeConfig {
            routing,
            ..FacadeConfig::default()
        };
        let report = run_regression_suite(&default_scenarios(), &config);
        // If shadow was not forced, all scenarios would fail (no comparison).
        prop_assert!(
            report.all_passed(),
            "suite should force shadow mode: {}",
            report.summary()
        );
    }
}

// ---------------------------------------------------------------------------
// RD-7: Replay gate verdict consistency
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn rd_7_verdict_consistency(
        min_pass_rate in 0.0f32..1.0,
        schema_required in proptest::bool::ANY,
    ) {
        let config = ReplayGateConfig {
            min_pass_rate,
            schema_gate_required: schema_required,
            facade: FacadeConfig::default(),
        };
        let verdict = run_replay_gate(&default_scenarios(), &config);

        // If go=true, then reason must be None.
        if verdict.go {
            prop_assert!(
                verdict.reason.is_none(),
                "go=true but reason is {:?}",
                verdict.reason
            );
        } else {
            prop_assert!(
                verdict.reason.is_some(),
                "go=false but reason is None"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// RD-8: ReplayGateVerdict serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn rd_8_verdict_serde_roundtrip(
        min_pass_rate in 0.5f32..1.0,
    ) {
        let config = ReplayGateConfig {
            min_pass_rate,
            schema_gate_required: true,
            facade: FacadeConfig::default(),
        };
        let verdict = run_replay_gate(&default_scenarios(), &config);
        let json = serde_json::to_string(&verdict).unwrap();
        let parsed: ReplayGateVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(verdict.go, parsed.go);
        prop_assert_eq!(
            verdict.regression.passed,
            parsed.regression.passed
        );
    }
}
