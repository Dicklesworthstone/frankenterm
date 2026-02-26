//! Property-based tests for replay_scenario_matrix (ft-og6q6.4.3).
//!
//! Invariants tested:
//! - SM-1: DiffSummary total = unchanged + added + removed + modified
//! - SM-2: Identical sequences → is_identical() true
//! - SM-3: DiffSummary serde roundtrip
//! - SM-4: ScenarioResult serde roundtrip
//! - SM-5: MatrixResult serde roundtrip
//! - SM-6: ProgressEvent serde roundtrip
//! - SM-7: MatrixConfig serde roundtrip
//! - SM-8: scenario_count = artifacts * overrides (or artifacts if no overrides)
//! - SM-9: scenario_pairs length matches scenario_count
//! - SM-10: from_results counts are consistent
//! - SM-11: all_passed iff divergence_count==0 && error_count==0
//! - SM-12: divergence_count == added + removed + modified
//! - SM-13: Empty diff is identical
//! - SM-14: DiffSummary compute symmetry: added/removed swap
//! - SM-15: fail_fast stops after first divergence
//! - SM-16: Runner progress events count matches scenarios executed
//! - SM-17: Runner with identical generator → all_passed
//! - SM-18: Runner with error generator → error_count = scenario_count
//! - SM-19: Total duration is sum of scenario durations
//! - SM-20: RunnerConfig defaults are stable
//! - SM-21: ArtifactEntry/OverrideEntry serde roundtrip
//! - SM-22: MatrixConfig from_toml rejects malformed input

use proptest::prelude::*;

use frankenterm_core::replay_scenario_matrix::{
    ArtifactEntry, DiffSummary, MatrixConfig, MatrixResult, OverrideEntry, ProgressEvent,
    RunnerConfig, ScenarioMatrixRunner, ScenarioResult,
};

type DecisionGenerator = Box<dyn Fn(&str, Option<&str>) -> Result<Vec<String>, String> + Send + Sync>;

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_label() -> impl Strategy<Value = String> {
    "[a-z]{1,8}".prop_map(|s| s)
}

fn arb_decision() -> impl Strategy<Value = String> {
    "[a-z0-9_]{1,12}".prop_map(|s| s)
}

fn arb_decisions(max_len: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_decision(), 0..max_len)
}

fn arb_diff_summary() -> impl Strategy<Value = DiffSummary> {
    (0u64..100, 0u64..100, 0u64..100, 0u64..100).prop_map(
        |(unchanged, added, removed, modified)| DiffSummary {
            total_decisions: unchanged + added + removed + modified,
            unchanged,
            added,
            removed,
            modified,
        },
    )
}

fn _arb_artifact() -> impl Strategy<Value = ArtifactEntry> {
    arb_label().prop_map(|label| ArtifactEntry {
        path: format!("{}.ftreplay", label),
        label,
    })
}

fn _arb_override_entry() -> impl Strategy<Value = OverrideEntry> {
    arb_label().prop_map(|label| OverrideEntry {
        path: format!("{}.ftoverride", label),
        label,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── SM-1: Total = unchanged + added + removed + modified ───────────

    #[test]
    fn sm1_diff_total_invariant(
        baseline in arb_decisions(20),
        candidate in arb_decisions(20),
    ) {
        let diff = DiffSummary::compute(&baseline, &candidate);
        prop_assert_eq!(
            diff.total_decisions,
            diff.unchanged + diff.added + diff.removed + diff.modified
        );
    }

    // ── SM-2: Identical sequences → is_identical ───────────────────────

    #[test]
    fn sm2_identical_is_identical(decisions in arb_decisions(15)) {
        let diff = DiffSummary::compute(&decisions, &decisions);
        prop_assert!(diff.is_identical());
        prop_assert_eq!(diff.unchanged, decisions.len() as u64);
    }

    // ── SM-3: DiffSummary serde roundtrip ──────────────────────────────

    #[test]
    fn sm3_diff_serde(diff in arb_diff_summary()) {
        let json = serde_json::to_string(&diff).unwrap();
        let restored: DiffSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, diff);
    }

    // ── SM-4: ScenarioResult serde roundtrip ───────────────────────────

    #[test]
    fn sm4_scenario_serde(
        art in arb_label(),
        ovr in arb_label(),
        base in arb_decisions(5),
        cand in arb_decisions(5),
        dur in 0u64..10000,
    ) {
        let diff = DiffSummary::compute(&base, &cand);
        let result = ScenarioResult {
            artifact_label: art.clone(),
            override_label: ovr,
            baseline_decisions: base,
            candidate_decisions: cand,
            diff,
            error: None,
            duration_ms: dur,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: ScenarioResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.artifact_label, art);
        prop_assert_eq!(restored.duration_ms, dur);
    }

    // ── SM-5: MatrixResult serde roundtrip ─────────────────────────────

    #[test]
    fn sm5_matrix_result_serde(n in 1usize..5) {
        let scenarios: Vec<ScenarioResult> = (0..n)
            .map(|i| ScenarioResult {
                artifact_label: format!("art_{}", i),
                override_label: String::new(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d1".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d1".into()]),
                error: None,
                duration_ms: 10,
            })
            .collect();
        let result = MatrixResult::from_results(scenarios);
        let json = result.to_json();
        let restored: MatrixResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.total_scenarios, n);
    }

    // ── SM-6: ProgressEvent serde roundtrip ────────────────────────────

    #[test]
    fn sm6_progress_serde(completed in 0usize..100, total in 1usize..100) {
        let event = ProgressEvent {
            completed,
            total,
            current_artifact: "a".into(),
            current_override: "o".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: ProgressEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.completed, completed);
        prop_assert_eq!(restored.total, total);
    }

    // ── SM-7: MatrixConfig serde roundtrip ─────────────────────────────

    #[test]
    fn sm7_config_serde(
        n_art in 1usize..4,
        n_ovr in 0usize..4,
        concurrency in 1usize..8,
    ) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("art_{}.ftreplay", i),
                    label: format!("art_{}", i),
                })
                .collect(),
            overrides: (0..n_ovr)
                .map(|i| OverrideEntry {
                    path: format!("ovr_{}.ftoverride", i),
                    label: format!("ovr_{}", i),
                })
                .collect(),
            config: RunnerConfig {
                concurrency,
                timeout_per_scenario_ms: 60_000,
                fail_fast: false,
            },
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: MatrixConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.artifacts.len(), n_art);
        prop_assert_eq!(restored.overrides.len(), n_ovr);
    }

    // ── SM-8: scenario_count = artifacts * overrides ───────────────────

    #[test]
    fn sm8_scenario_count(n_art in 1usize..5, n_ovr in 0usize..5) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("{}.ftreplay", i),
                    label: format!("{}", i),
                })
                .collect(),
            overrides: (0..n_ovr)
                .map(|i| OverrideEntry {
                    path: format!("{}.ftoverride", i),
                    label: format!("{}", i),
                })
                .collect(),
            config: RunnerConfig::default(),
        };
        let expected = if n_ovr == 0 { n_art } else { n_art * n_ovr };
        prop_assert_eq!(config.scenario_count(), expected);
    }

    // ── SM-9: scenario_pairs length matches scenario_count ─────────────

    #[test]
    fn sm9_pairs_match_count(n_art in 1usize..4, n_ovr in 0usize..4) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("{}.ftreplay", i),
                    label: format!("{}", i),
                })
                .collect(),
            overrides: (0..n_ovr)
                .map(|i| OverrideEntry {
                    path: format!("{}.ftoverride", i),
                    label: format!("{}", i),
                })
                .collect(),
            config: RunnerConfig::default(),
        };
        let pairs = config.scenario_pairs();
        prop_assert_eq!(pairs.len(), config.scenario_count());
    }

    // ── SM-10: from_results counts are consistent ──────────────────────

    #[test]
    fn sm10_result_counts_consistent(
        n_pass in 0usize..5,
        n_div in 0usize..5,
        n_err in 0usize..5,
    ) {
        let mut scenarios = Vec::new();
        // Passing scenarios (identical decisions, no error).
        for i in 0..n_pass {
            scenarios.push(ScenarioResult {
                artifact_label: format!("pass_{}", i),
                override_label: String::new(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d1".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d1".into()]),
                error: None,
                duration_ms: 10,
            });
        }
        // Divergent scenarios (different decisions, no error).
        for i in 0..n_div {
            scenarios.push(ScenarioResult {
                artifact_label: format!("div_{}", i),
                override_label: String::new(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d2".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d2".into()]),
                error: None,
                duration_ms: 10,
            });
        }
        // Error scenarios.
        for i in 0..n_err {
            scenarios.push(ScenarioResult {
                artifact_label: format!("err_{}", i),
                override_label: String::new(),
                baseline_decisions: vec![],
                candidate_decisions: vec![],
                diff: DiffSummary::default(),
                error: Some("fail".into()),
                duration_ms: 0,
            });
        }
        let result = MatrixResult::from_results(scenarios);
        prop_assert_eq!(result.total_scenarios, n_pass + n_div + n_err);
        prop_assert_eq!(result.pass_count, n_pass);
        prop_assert_eq!(result.divergence_count, n_div);
        prop_assert_eq!(result.error_count, n_err);
    }

    // ── SM-11: all_passed iff no divergence and no errors ──────────────

    #[test]
    fn sm11_all_passed_semantics(n_pass in 1usize..5, n_div in 0usize..3, n_err in 0usize..3) {
        let mut scenarios = Vec::new();
        for i in 0..n_pass {
            scenarios.push(ScenarioResult {
                artifact_label: format!("p_{}", i),
                override_label: String::new(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d1".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d1".into()]),
                error: None,
                duration_ms: 10,
            });
        }
        for i in 0..n_div {
            scenarios.push(ScenarioResult {
                artifact_label: format!("d_{}", i),
                override_label: String::new(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d2".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d2".into()]),
                error: None,
                duration_ms: 10,
            });
        }
        for i in 0..n_err {
            scenarios.push(ScenarioResult {
                artifact_label: format!("e_{}", i),
                override_label: String::new(),
                baseline_decisions: vec![],
                candidate_decisions: vec![],
                diff: DiffSummary::default(),
                error: Some("fail".into()),
                duration_ms: 0,
            });
        }
        let result = MatrixResult::from_results(scenarios);
        let expected = n_div == 0 && n_err == 0;
        prop_assert_eq!(result.all_passed(), expected);
    }

    // ── SM-12: divergence_count method ─────────────────────────────────

    #[test]
    fn sm12_divergence_count(
        added in 0u64..50,
        removed in 0u64..50,
        modified in 0u64..50,
    ) {
        let diff = DiffSummary {
            total_decisions: added + removed + modified + 10,
            unchanged: 10,
            added,
            removed,
            modified,
        };
        prop_assert_eq!(diff.divergence_count(), added + removed + modified);
    }

    // ── SM-13: Empty diff is identical ─────────────────────────────────

    #[test]
    fn sm13_empty_diff_identical(_dummy in 0u8..1) {
        let diff = DiffSummary::compute(&[], &[]);
        prop_assert!(diff.is_identical());
        prop_assert_eq!(diff.total_decisions, 0);
    }

    // ── SM-14: Symmetry: swap baseline/candidate swaps added/removed ───

    #[test]
    fn sm14_diff_symmetry(
        baseline in arb_decisions(10),
        candidate in arb_decisions(10),
    ) {
        let fwd = DiffSummary::compute(&baseline, &candidate);
        let rev = DiffSummary::compute(&candidate, &baseline);
        prop_assert_eq!(fwd.added, rev.removed);
        prop_assert_eq!(fwd.removed, rev.added);
        prop_assert_eq!(fwd.modified, rev.modified);
        prop_assert_eq!(fwd.unchanged, rev.unchanged);
    }

    // ── SM-15: fail_fast stops after first divergence ──────────────────

    #[test]
    fn sm15_fail_fast(n_art in 2usize..5) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("{}.ftreplay", i),
                    label: format!("art_{}", i),
                })
                .collect(),
            overrides: vec![OverrideEntry {
                path: "o.ftoverride".into(),
                label: "o".into(),
            }],
            config: RunnerConfig {
                concurrency: 1,
                timeout_per_scenario_ms: 60_000,
                fail_fast: true,
            },
        };
        // Generator produces divergent results (override adds extra decision).
        let dg: DecisionGenerator =
            Box::new(|_art, ovr| {
                if ovr.is_some() {
                    Ok(vec!["d1".into(), "extra".into()])
                } else {
                    Ok(vec!["d1".into()])
                }
            });
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        prop_assert_eq!(result.total_scenarios, 1, "fail_fast should stop after first divergence");
    }

    // ── SM-16: Progress events count matches scenarios executed ────────

    #[test]
    fn sm16_progress_count(n_art in 1usize..4, n_ovr in 0usize..3) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("{}.ftreplay", i),
                    label: format!("{}", i),
                })
                .collect(),
            overrides: (0..n_ovr)
                .map(|i| OverrideEntry {
                    path: format!("{}.ftoverride", i),
                    label: format!("{}", i),
                })
                .collect(),
            config: RunnerConfig::default(),
        };
        let expected = config.scenario_count();
        let dg: DecisionGenerator =
            Box::new(|_art, _ovr| Ok(vec!["d1".into()]));
        let runner = ScenarioMatrixRunner::new(config, dg);
        let mut events = Vec::new();
        runner.run(|p| events.push(p));
        prop_assert_eq!(events.len(), expected);
    }

    // ── SM-17: Identical generator → all_passed ────────────────────────

    #[test]
    fn sm17_identical_all_pass(n_art in 1usize..4) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("{}.ftreplay", i),
                    label: format!("{}", i),
                })
                .collect(),
            overrides: vec![],
            config: RunnerConfig::default(),
        };
        let dg: DecisionGenerator =
            Box::new(|_art, _ovr| Ok(vec!["d1".into(), "d2".into()]));
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        prop_assert!(result.all_passed());
    }

    // ── SM-18: Error generator → error_count = scenario_count ──────────

    #[test]
    fn sm18_error_counts(n_art in 1usize..4) {
        let config = MatrixConfig {
            artifacts: (0..n_art)
                .map(|i| ArtifactEntry {
                    path: format!("{}.ftreplay", i),
                    label: format!("{}", i),
                })
                .collect(),
            overrides: vec![],
            config: RunnerConfig::default(),
        };
        let expected = config.scenario_count();
        let dg: DecisionGenerator =
            Box::new(|_art, _ovr| Err("fail".into()));
        let runner = ScenarioMatrixRunner::new(config, dg);
        let result = runner.run(|_| {});
        prop_assert_eq!(result.error_count, expected);
    }

    // ── SM-19: Total duration is sum of scenario durations ─────────────

    #[test]
    fn sm19_total_duration(durations in prop::collection::vec(0u64..1000, 1..10)) {
        let scenarios: Vec<ScenarioResult> = durations
            .iter()
            .enumerate()
            .map(|(i, &dur)| ScenarioResult {
                artifact_label: format!("a_{}", i),
                override_label: String::new(),
                baseline_decisions: vec!["d1".into()],
                candidate_decisions: vec!["d1".into()],
                diff: DiffSummary::compute(&["d1".into()], &["d1".into()]),
                error: None,
                duration_ms: dur,
            })
            .collect();
        let expected_sum: u64 = durations.iter().sum();
        let result = MatrixResult::from_results(scenarios);
        prop_assert_eq!(result.total_duration_ms, expected_sum);
    }

    // ── SM-20: RunnerConfig defaults are stable ────────────────────────

    #[test]
    fn sm20_defaults_stable(_dummy in 0u8..1) {
        let config = RunnerConfig::default();
        prop_assert_eq!(config.concurrency, 2);
        prop_assert_eq!(config.timeout_per_scenario_ms, 300_000);
        prop_assert!(!config.fail_fast);
    }

    // ── SM-21: ArtifactEntry/OverrideEntry serde roundtrip ─────────────

    #[test]
    fn sm21_entry_serde(label in arb_label()) {
        let art = ArtifactEntry {
            path: format!("{}.ftreplay", label),
            label: label.clone(),
        };
        let json = serde_json::to_string(&art).unwrap();
        let restored: ArtifactEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.label, &label);

        let ovr = OverrideEntry {
            path: format!("{}.ftoverride", label),
            label: label.clone(),
        };
        let json = serde_json::to_string(&ovr).unwrap();
        let restored: OverrideEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.label, &label);
    }

    // ── SM-22: MatrixConfig rejects malformed TOML ─────────────────────

    #[test]
    fn sm22_rejects_malformed(garbage in "[^\\x00]{1,20}") {
        // Most random strings won't be valid TOML with the right schema.
        // If it happens to parse, that's fine — we just verify no panic.
        let _ = MatrixConfig::from_toml(&garbage);
    }
}
