//! Property-based tests for replay_cli (ft-og6q6.6.1).
//!
//! Invariants tested:
//! - RC-1: ReplayOutputMode serde roundtrip
//! - RC-2: ReplayExitCode serde roundtrip
//! - RC-3: ReplayExitCode::code() values are distinct
//! - RC-4: SpeedArg parsing roundtrip for valid strings
//! - RC-5: SpeedArg::multiplier() > 0
//! - RC-6: EquivalenceLevelArg serde roundtrip
//! - RC-7: InspectResult from_events event_count matches input
//! - RC-8: InspectResult pane_count ≤ event_count
//! - RC-9: InspectResult rule_count ≤ event_count
//! - RC-10: InspectResult render_human contains artifact path
//! - RC-11: DiffRunner identical inputs → Pass
//! - RC-12: DiffRunner format output non-empty (except Quiet on pass)
//! - RC-13: RegressionSuiteResult counts consistent
//! - RC-14: RegressionSuiteResult overall_pass ↔ no failures
//! - RC-15: SpeedArg Custom multiplier preserved

use proptest::prelude::*;

use frankenterm_core::replay_cli::{
    ArtifactResult, DiffRunner, EquivalenceLevelArg, InspectResult, RegressionSuiteResult,
    ReplayExitCode, ReplayOutputMode, SpeedArg,
};
use frankenterm_core::replay_decision_diff::DiffConfig;
use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionType};
use frankenterm_core::replay_report::ReportMeta;

fn make_event(rule_id: &str, ts: u64, pane: u64) -> DecisionEvent {
    let input = format!("rule={rule_id};ts={ts};pane={pane}");
    DecisionEvent::new(
        DecisionType::PatternMatch,
        pane,
        rule_id,
        "d",
        &input,
        serde_json::Value::String("o".into()),
        None,
        Some(1.0),
        ts,
    )
}

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

fn arb_equiv_level() -> impl Strategy<Value = EquivalenceLevelArg> {
    prop_oneof![
        Just(EquivalenceLevelArg::Structural),
        Just(EquivalenceLevelArg::Decision),
        Just(EquivalenceLevelArg::Full),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── RC-1: ReplayOutputMode serde ──────────────────────────────────

    #[test]
    fn rc1_output_mode_serde(mode in arb_output_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let restored: ReplayOutputMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, mode);
    }

    // ── RC-2: ReplayExitCode serde ────────────────────────────────────

    #[test]
    fn rc2_exit_code_serde(ec in arb_exit_code()) {
        let json = serde_json::to_string(&ec).unwrap();
        let restored: ReplayExitCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, ec);
    }

    // ── RC-3: Exit codes are distinct ─────────────────────────────────

    #[test]
    fn rc3_exit_codes_distinct(_dummy in 0u8..1) {
        let codes = [
            ReplayExitCode::Pass.code(),
            ReplayExitCode::Regression.code(),
            ReplayExitCode::InvalidInput.code(),
            ReplayExitCode::InternalError.code(),
        ];
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                prop_assert_ne!(codes[i], codes[j]);
            }
        }
    }

    // ── RC-4: SpeedArg parsing for valid strings ──────────────────────

    #[test]
    fn rc4_speed_parse_valid(s in prop_oneof![
        Just("1x".to_string()),
        Just("2x".to_string()),
        Just("instant".to_string()),
        Just("normal".to_string()),
        (1.0f64..100.0).prop_map(|v| format!("{}x", v as u32)),
    ]) {
        let result = SpeedArg::from_str_arg(&s);
        prop_assert!(result.is_ok(), "should parse '{}' successfully", s);
    }

    // ── RC-5: SpeedArg multiplier > 0 ─────────────────────────────────

    #[test]
    fn rc5_speed_positive(_dummy in 0u8..1) {
        for sa in &[SpeedArg::Normal, SpeedArg::Double, SpeedArg::Instant, SpeedArg::Custom(3.0)] {
            prop_assert!(sa.multiplier() > 0.0);
        }
    }

    // ── RC-6: EquivalenceLevelArg serde ───────────────────────────────

    #[test]
    fn rc6_equiv_serde(level in arb_equiv_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let restored: EquivalenceLevelArg = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, level);
    }

    // ── RC-7: InspectResult event_count matches ───────────────────────

    #[test]
    fn rc7_inspect_event_count(n in 0usize..20) {
        let events: Vec<DecisionEvent> = (0..n)
            .map(|i| make_event(&format!("r_{}", i), (i as u64) * 10, 0))
            .collect();
        let result = InspectResult::from_events("test.replay", &events);
        prop_assert_eq!(result.event_count, n as u64);
    }

    // ── RC-8: pane_count ≤ event_count ────────────────────────────────

    #[test]
    fn rc8_pane_count(n in 1usize..20) {
        let events: Vec<DecisionEvent> = (0..n)
            .map(|i| make_event(&format!("r_{}", i), (i as u64) * 10, i as u64 % 5))
            .collect();
        let result = InspectResult::from_events("test.replay", &events);
        prop_assert!(result.pane_count <= result.event_count);
    }

    // ── RC-9: rule_count ≤ event_count ────────────────────────────────

    #[test]
    fn rc9_rule_count(n in 1usize..20) {
        let events: Vec<DecisionEvent> = (0..n)
            .map(|i| make_event(&format!("r_{}", i % 3), (i as u64) * 10, 0))
            .collect();
        let result = InspectResult::from_events("test.replay", &events);
        prop_assert!(result.rule_count <= result.event_count);
    }

    // ── RC-10: render_human contains artifact path ────────────────────

    #[test]
    fn rc10_render_has_path(name in "[a-z]{3,8}\\.replay") {
        let events = vec![make_event("r1", 100, 1)];
        let result = InspectResult::from_events(&name, &events);
        let human = result.render_human();
        prop_assert!(human.contains(&name));
    }

    // ── RC-11: DiffRunner identical → Pass ────────────────────────────

    #[test]
    fn rc11_identical_pass(n in 1usize..10) {
        let events: Vec<DecisionEvent> = (0..n)
            .map(|i| make_event(&format!("r_{}", i), (i as u64) * 10, 0))
            .collect();
        let runner = DiffRunner::new();
        let result = runner.run(&events, &events, &DiffConfig::default());
        prop_assert_eq!(result.exit_code, ReplayExitCode::Pass);
    }

    // ── RC-12: Format output non-empty (except Quiet pass) ────────────

    #[test]
    fn rc12_format_nonempty(mode in arb_output_mode()) {
        let events = vec![
            make_event("r1", 100, 1),
            make_event("r2", 200, 1),
        ];
        let runner = DiffRunner::new();
        let result = runner.run(&events, &events, &DiffConfig::default());
        let formatted = runner.format_result(&result, mode, &ReportMeta::default());
        if mode == ReplayOutputMode::Quiet && result.exit_code == ReplayExitCode::Pass {
            prop_assert!(formatted.is_empty());
        } else {
            prop_assert!(!formatted.is_empty());
        }
    }

    // ── RC-13: Suite counts consistent ────────────────────────────────

    #[test]
    fn rc13_suite_counts(n_pass in 0usize..5, n_fail in 0usize..5, n_err in 0usize..3) {
        let mut results = Vec::new();
        for i in 0..n_pass {
            results.push(ArtifactResult {
                artifact_path: format!("pass_{}.replay", i),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            });
        }
        for i in 0..n_fail {
            results.push(ArtifactResult {
                artifact_path: format!("fail_{}.replay", i),
                passed: false,
                gate_result_summary: "Fail".into(),
                error: None,
            });
        }
        for i in 0..n_err {
            results.push(ArtifactResult {
                artifact_path: format!("err_{}.replay", i),
                passed: false,
                gate_result_summary: "Error".into(),
                error: Some("error".into()),
            });
        }
        let suite = RegressionSuiteResult::from_results(results);
        prop_assert_eq!(suite.total_artifacts, (n_pass + n_fail + n_err) as u64);
        prop_assert_eq!(suite.passed, n_pass as u64);
    }

    // ── RC-14: overall_pass ↔ no failures ─────────────────────────────

    #[test]
    fn rc14_overall_pass(n_pass in 1usize..5, n_fail in 0usize..3) {
        let mut results = Vec::new();
        for i in 0..n_pass {
            results.push(ArtifactResult {
                artifact_path: format!("p_{}.replay", i),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            });
        }
        for i in 0..n_fail {
            results.push(ArtifactResult {
                artifact_path: format!("f_{}.replay", i),
                passed: false,
                gate_result_summary: "Fail".into(),
                error: None,
            });
        }
        let suite = RegressionSuiteResult::from_results(results);
        if n_fail == 0 {
            prop_assert!(suite.overall_pass);
        } else {
            prop_assert!(!suite.overall_pass);
        }
    }

    // ── RC-15: Custom multiplier preserved ────────────────────────────

    #[test]
    fn rc15_custom_multiplier(m in 0.1f64..100.0) {
        let speed = SpeedArg::Custom(m);
        prop_assert!((speed.multiplier() - m).abs() < 1e-10);
    }
}
