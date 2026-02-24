//! Property-based tests for replay_robot (ft-og6q6.6.2).
//!
//! Invariants tested:
//! - RR-1: ReplayRobotCommand serde roundtrip
//! - RR-2: Command string roundtrip (as_str → from_str_command)
//! - RR-3: InspectData serde roundtrip
//! - RR-4: DiffData serde roundtrip
//! - RR-5: RegressionSuiteData serde roundtrip
//! - RR-6: ArtifactListData serde roundtrip
//! - RR-7: ArtifactInspectData serde roundtrip
//! - RR-8: ArtifactAddData serde roundtrip
//! - RR-9: ArtifactRetireData serde roundtrip
//! - RR-10: ArtifactPruneData serde roundtrip
//! - RR-11: All error codes start with "replay."
//! - RR-12: DiffRequest defaults (tolerance=100, budget=None)
//! - RR-13: classify_replay_command matches from_str_command
//! - RR-14: InspectData from_inspect_result preserves fields
//! - RR-15: RegressionSuiteData from_suite_result preserves counts
//! - RR-16: ArtifactPruneData from_prune_result preserves fields
//! - RR-17: Request envelope roundtrip
//! - RR-18: ArtifactSummary serde roundtrip
//! - RR-19: ArtifactResultData serde roundtrip
//! - RR-20: Non-replay commands return None from classify

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::replay_robot::{
    ArtifactAddData, ArtifactInspectData, ArtifactListData, ArtifactPruneData,
    ArtifactResultData, ArtifactRetireData, ArtifactSummary, DiffData, DiffRequest,
    InspectData, InspectRequest, ReplayRequest, ReplayRobotCommand, RegressionSuiteData,
    classify_replay_command,
    REPLAY_ERR_ALREADY_RETIRED, REPLAY_ERR_DUPLICATE, REPLAY_ERR_FILE_NOT_FOUND,
    REPLAY_ERR_INTEGRITY_ERROR, REPLAY_ERR_NOT_FOUND, REPLAY_ERR_PARSE_ERROR,
    REPLAY_ERR_SCHEMA_MISMATCH,
};

fn arb_command() -> impl Strategy<Value = ReplayRobotCommand> {
    prop_oneof![
        Just(ReplayRobotCommand::Inspect),
        Just(ReplayRobotCommand::Diff),
        Just(ReplayRobotCommand::RegressionSuite),
        Just(ReplayRobotCommand::ArtifactList),
        Just(ReplayRobotCommand::ArtifactInspect),
        Just(ReplayRobotCommand::ArtifactAdd),
        Just(ReplayRobotCommand::ArtifactRetire),
        Just(ReplayRobotCommand::ArtifactPrune),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── RR-1: Command serde roundtrip ────────────────────────────────────

    #[test]
    fn rr1_command_serde(cmd in arb_command()) {
        let json = serde_json::to_string(&cmd).unwrap();
        let restored: ReplayRobotCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, cmd);
    }

    // ── RR-2: Command string roundtrip ───────────────────────────────────

    #[test]
    fn rr2_command_str_roundtrip(cmd in arb_command()) {
        let s = cmd.as_str();
        let restored = ReplayRobotCommand::from_str_command(s);
        prop_assert_eq!(restored, Some(cmd));
    }

    // ── RR-3: InspectData serde ──────────────────────────────────────────

    #[test]
    fn rr3_inspect_data_serde(
        events in 0u64..1000,
        panes in 0u64..100,
        rules in 0u64..100,
        span in 0u64..999_999,
    ) {
        let data = InspectData {
            artifact_path: "test.ftreplay".into(),
            event_count: events,
            pane_count: panes,
            rule_count: rules,
            time_span_ms: span,
            decision_types: vec!["pattern_match".into()],
            integrity_ok: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: InspectData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-4: DiffData serde ─────────────────────────────────────────────

    #[test]
    fn rr4_diff_data_serde(
        passed in proptest::bool::ANY,
        exit_code in 0i32..4,
        divs in 0u64..100,
    ) {
        let data = DiffData {
            passed,
            exit_code,
            divergence_count: divs,
            recommendation: "fix it".into(),
            gate_result: if passed { "Pass" } else { "Fail" }.into(),
            severity_counts: BTreeMap::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: DiffData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-5: RegressionSuiteData serde ──────────────────────────────────

    #[test]
    fn rr5_suite_data_serde(
        total in 0u64..50,
        passed_count in 0u64..50,
        failed in 0u64..50,
    ) {
        let data = RegressionSuiteData {
            passed: failed == 0,
            total_artifacts: total,
            passed_count,
            failed_count: failed,
            errored_count: 0,
            results: Vec::new(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: RegressionSuiteData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-6: ArtifactListData serde ─────────────────────────────────────

    #[test]
    fn rr6_list_data_serde(n in 0usize..5) {
        let artifacts: Vec<ArtifactSummary> = (0..n).map(|i| ArtifactSummary {
            path: format!("art_{}.ftreplay", i),
            label: format!("label_{}", i),
            tier: "T1".into(),
            status: "active".into(),
            event_count: i as u64,
            size_bytes: 100,
            sha256: "a".repeat(64),
        }).collect();
        let data = ArtifactListData {
            count: n as u64,
            artifacts,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-7: ArtifactInspectData serde ──────────────────────────────────

    #[test]
    fn rr7_artifact_inspect_serde(
        integrity in proptest::bool::ANY,
        exists in proptest::bool::ANY,
    ) {
        let data = ArtifactInspectData {
            path: "test.ftreplay".into(),
            label: "test".into(),
            tier: "T1".into(),
            status: "active".into(),
            event_count: 10,
            decision_count: 3,
            size_bytes: 512,
            sha256: "a".repeat(64),
            integrity_ok: integrity,
            file_exists: exists,
            retire_reason: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactInspectData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-8: ArtifactAddData serde ──────────────────────────────────────

    #[test]
    fn rr8_add_data_serde(events in 0u64..1000, size in 0u64..999_999) {
        let data = ArtifactAddData {
            path: "new.ftreplay".into(),
            sha256: "b".repeat(64),
            event_count: events,
            size_bytes: size,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactAddData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-9: ArtifactRetireData serde ───────────────────────────────────

    #[test]
    fn rr9_retire_data_serde(ts in 1000u64..999_999) {
        let data = ArtifactRetireData {
            path: "old.ftreplay".into(),
            reason: "replaced".into(),
            retired_at_ms: ts,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactRetireData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-10: ArtifactPruneData serde ───────────────────────────────────

    #[test]
    fn rr10_prune_data_serde(
        count in 0u64..10,
        freed in 0u64..10_000,
        dry in proptest::bool::ANY,
    ) {
        let data = ArtifactPruneData {
            pruned_count: count,
            bytes_freed: freed,
            dry_run: dry,
            pruned_paths: (0..count).map(|i| format!("p_{}.ftreplay", i)).collect(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactPruneData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-11: Error codes namespace ─────────────────────────────────────

    #[test]
    fn rr11_error_codes_namespace(_dummy in 0u8..1) {
        let codes = [
            REPLAY_ERR_FILE_NOT_FOUND,
            REPLAY_ERR_PARSE_ERROR,
            REPLAY_ERR_INTEGRITY_ERROR,
            REPLAY_ERR_DUPLICATE,
            REPLAY_ERR_NOT_FOUND,
            REPLAY_ERR_ALREADY_RETIRED,
            REPLAY_ERR_SCHEMA_MISMATCH,
        ];
        for code in &codes {
            prop_assert!(code.starts_with("replay."), "code should start with replay.: {}", code);
        }
    }

    // ── RR-12: DiffRequest defaults ──────────────────────────────────────

    #[test]
    fn rr12_diff_defaults(_dummy in 0u8..1) {
        let json = r#"{"baseline":"b","candidate":"c"}"#;
        let req: DiffRequest = serde_json::from_str(json).unwrap();
        prop_assert_eq!(req.tolerance_ms, 100);
        let is_none = req.budget.is_none();
        prop_assert!(is_none);
    }

    // ── RR-13: classify matches from_str_command ─────────────────────────

    #[test]
    fn rr13_classify_matches(cmd in arb_command()) {
        let s = cmd.as_str();
        let from_classify = classify_replay_command(s);
        let from_parse = ReplayRobotCommand::from_str_command(s);
        prop_assert_eq!(from_classify, from_parse);
    }

    // ── RR-14: InspectData from_inspect_result preserves fields ──────────

    #[test]
    fn rr14_inspect_from_result(n in 1usize..10) {
        use frankenterm_core::replay_cli::InspectResult;
        use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionType};

        let events: Vec<DecisionEvent> = (0..n).map(|i| DecisionEvent {
            decision_type: DecisionType::PatternMatch,
            rule_id: format!("r_{}", i),
            definition_hash: "d".into(),
            input_hash: format!("in_{}", i),
            output_hash: "out".into(),
            timestamp_ms: (i as u64) * 10,
            pane_id: i as u64 % 3,
            triggered_by: None,
            overrides: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        }).collect();
        let result = InspectResult::from_events("test.ftreplay", &events);
        let data = InspectData::from_inspect_result(&result);
        prop_assert_eq!(data.event_count, result.event_count);
        prop_assert_eq!(data.pane_count, result.pane_count);
        prop_assert_eq!(data.rule_count, result.rule_count);
    }

    // ── RR-15: Suite from_suite_result preserves counts ──────────────────

    #[test]
    fn rr15_suite_from_result(n_pass in 0usize..5, n_fail in 0usize..5) {
        use frankenterm_core::replay_cli::{ArtifactResult, RegressionSuiteResult};
        use frankenterm_core::replay_robot::RegressionSuiteData;

        let mut results = Vec::new();
        for i in 0..n_pass {
            results.push(ArtifactResult {
                artifact_path: format!("p_{}.ftreplay", i),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            });
        }
        for i in 0..n_fail {
            results.push(ArtifactResult {
                artifact_path: format!("f_{}.ftreplay", i),
                passed: false,
                gate_result_summary: "Fail".into(),
                error: None,
            });
        }
        let suite = RegressionSuiteResult::from_results(results);
        let data = RegressionSuiteData::from_suite_result(&suite);
        prop_assert_eq!(data.total_artifacts, (n_pass + n_fail) as u64);
        prop_assert_eq!(data.passed_count, n_pass as u64);
        prop_assert_eq!(data.failed_count, n_fail as u64);
    }

    // ── RR-16: Prune from_prune_result preserves fields ──────────────────

    #[test]
    fn rr16_prune_from_result(count in 0u64..10, freed in 0u64..10_000) {
        use frankenterm_core::replay_artifact_registry::PruneResult;

        let result = PruneResult {
            pruned_count: count,
            pruned_paths: (0..count).map(|i| format!("p_{}.ftreplay", i)).collect(),
            bytes_freed: freed,
            dry_run: true,
        };
        let data = ArtifactPruneData::from_prune_result(&result);
        prop_assert_eq!(data.pruned_count, count);
        prop_assert_eq!(data.bytes_freed, freed);
        prop_assert!(data.dry_run);
    }

    // ── RR-17: Request envelope roundtrip ────────────────────────────────

    #[test]
    fn rr17_request_envelope(path in "[a-z]{3,10}\\.ftreplay") {
        let req = ReplayRequest {
            command: "replay.inspect".to_string(),
            args: InspectRequest { trace: path.clone() },
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: ReplayRequest<InspectRequest> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.args.trace, path);
    }

    // ── RR-18: ArtifactSummary serde ─────────────────────────────────────

    #[test]
    fn rr18_artifact_summary_serde(events in 0u64..1000, size in 0u64..999_999) {
        let summary = ArtifactSummary {
            path: "test.ftreplay".into(),
            label: "test".into(),
            tier: "T1".into(),
            status: "active".into(),
            event_count: events,
            size_bytes: size,
            sha256: "a".repeat(64),
        };
        let json = serde_json::to_string(&summary).unwrap();
        let restored: ArtifactSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, summary);
    }

    // ── RR-19: ArtifactResultData serde ──────────────────────────────────

    #[test]
    fn rr19_artifact_result_serde(passed in proptest::bool::ANY) {
        let data = ArtifactResultData {
            artifact_path: "test.ftreplay".into(),
            passed,
            gate_result_summary: if passed { "Pass" } else { "Fail" }.into(),
            error: if passed { None } else { Some("failed".into()) },
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactResultData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── RR-20: Non-replay commands → None ────────────────────────────────

    #[test]
    fn rr20_non_replay_none(prefix in "[a-z]{2,8}") {
        // Avoid accidentally matching "replay" prefix
        if prefix == "replay" {
            return Ok(());
        }
        let cmd = format!("{}.inspect", prefix);
        let result = classify_replay_command(&cmd);
        let is_none = result.is_none();
        prop_assert!(is_none);
    }
}
