//! Property-based tests for replay_guide (ft-og6q6.6.6).
//!
//! Invariants tested:
//! - GU-1: GuideWorkflow serde roundtrip
//! - GU-2: GuideWorkflow as_str/from_str_arg roundtrip
//! - GU-3: Step count matches step_descriptions length
//! - GU-4: execute_step out-of-range → Error
//! - GU-5: execute_step in-range → non-Error status
//! - GU-6: Last step has has_next=false
//! - GU-7: Non-last step has has_next=true
//! - GU-8: GuideStepOutput serde roundtrip
//! - GU-9: GuideProgress update monotonic progress
//! - GU-10: GuideProgress complete when processed==total
//! - GU-11: GuideContext default tolerance is 100
//! - GU-12: GuideContext serde roundtrip
//! - GU-13: GuideRobotCommand as_str/from_str roundtrip
//! - GU-14: start_workflow returns correct step count
//! - GU-15: list_workflows returns 3 workflows
//! - GU-16: GuideStartData serde roundtrip
//! - GU-17: GuideListData serde roundtrip
//! - GU-18: All workflow step_ids are unique
//! - GU-19: MCP schema has additionalProperties=false
//! - GU-20: GuideProgress ETA decreases as progress increases

use proptest::prelude::*;

use frankenterm_core::replay_guide::{
    ALL_WORKFLOWS, GuideContext, GuideProgress, GuideRobotCommand, GuideStepInput, GuideStepOutput,
    GuideStepStatus, GuideWorkflow, execute_step, guide_tool_schema, list_workflows,
    start_workflow,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── GU-1: GuideWorkflow serde roundtrip ────────────────────────

    #[test]
    fn gu01_workflow_serde(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let json = serde_json::to_string(&wf).unwrap();
        let restored: GuideWorkflow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, wf);
    }

    // ── GU-2: as_str/from_str_arg roundtrip ────────────────────────

    #[test]
    fn gu02_workflow_str_roundtrip(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let s = wf.as_str();
        let parsed = GuideWorkflow::from_str_arg(s);
        prop_assert_eq!(parsed, Some(wf));
    }

    // ── GU-3: Step count matches descriptions ──────────────────────

    #[test]
    fn gu03_step_count_matches(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let descs = wf.step_descriptions();
        prop_assert_eq!(descs.len(), wf.step_count());
    }

    // ── GU-4: Out-of-range step → Error ────────────────────────────

    #[test]
    fn gu04_out_of_range_error(idx in 0usize..3, extra in 1usize..100) {
        let wf = ALL_WORKFLOWS[idx];
        let step = wf.step_count() + extra;
        let input = GuideStepInput {
            workflow: wf,
            step,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        prop_assert_eq!(output.status, GuideStepStatus::Error);
        let has_next = output.has_next;
        prop_assert!(!has_next);
    }

    // ── GU-5: In-range step → non-Error ────────────────────────────

    #[test]
    fn gu05_in_range_not_error(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        // Only test step 0 with appropriate context
        let ctx = match wf {
            GuideWorkflow::Investigate => GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
            GuideWorkflow::TestRule => GuideContext {
                baseline_path: Some("base.ftreplay".into()),
                ..Default::default()
            },
            GuideWorkflow::RegressionCheck => GuideContext::default(),
        };
        let input = GuideStepInput {
            workflow: wf,
            step: 0,
            context: ctx,
        };
        let output = execute_step(&input);
        let is_error = output.status == GuideStepStatus::Error;
        prop_assert!(!is_error, "step 0 should not error with valid context");
    }

    // ── GU-6: Last step has has_next=false ─────────────────────────

    #[test]
    fn gu06_last_step_no_next(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let last_step = wf.step_count() - 1;
        let ctx = match wf {
            GuideWorkflow::Investigate => GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
            GuideWorkflow::TestRule => {
                let mut c = GuideContext {
                    baseline_path: Some("base.ftreplay".into()),
                    candidate_path: Some("cand.ftreplay".into()),
                    ..Default::default()
                };
                c.results.insert("gate_result".into(), serde_json::json!("Pass"));
                c
            },
            GuideWorkflow::RegressionCheck => {
                let mut c = GuideContext::default();
                c.results.insert("suite_passed".into(), serde_json::json!(true));
                c
            },
        };
        let input = GuideStepInput {
            workflow: wf,
            step: last_step,
            context: ctx,
        };
        let output = execute_step(&input);
        let has_next = output.has_next;
        prop_assert!(!has_next, "last step should have has_next=false");
        prop_assert!(output.next_step.is_none());
    }

    // ── GU-7: Non-last step has has_next=true ──────────────────────

    #[test]
    fn gu07_non_last_step_has_next(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        // Step 0 should have has_next=true for all workflows
        let ctx = match wf {
            GuideWorkflow::Investigate => GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
            GuideWorkflow::TestRule => GuideContext {
                baseline_path: Some("base.ftreplay".into()),
                ..Default::default()
            },
            GuideWorkflow::RegressionCheck => GuideContext::default(),
        };
        let input = GuideStepInput {
            workflow: wf,
            step: 0,
            context: ctx,
        };
        let output = execute_step(&input);
        if output.status != GuideStepStatus::Error {
            prop_assert!(output.has_next, "step 0 should have has_next=true");
            prop_assert_eq!(output.next_step, Some(1));
        }
    }

    // ── GU-8: GuideStepOutput serde roundtrip ──────────────────────

    #[test]
    fn gu08_step_output_serde(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let ctx = GuideContext {
            artifact_paths: vec!["test.ftreplay".into()],
            baseline_path: Some("base.ftreplay".into()),
            ..Default::default()
        };
        let input = GuideStepInput {
            workflow: wf,
            step: 0,
            context: ctx,
        };
        let output = execute_step(&input);
        let json = serde_json::to_string(&output).unwrap();
        let restored: GuideStepOutput = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, output);
    }

    // ── GU-9: Progress update monotonic ────────────────────────────

    #[test]
    fn gu09_progress_monotonic(
        processed1 in 0u64..500,
        processed2 in 500u64..1000,
        total in 1000u64..2000
    ) {
        let mut p = GuideProgress::new(GuideWorkflow::Investigate, 0);
        p.update(processed1, total, 1000);
        let prog1 = p.progress;
        p.update(processed2, total, 2000);
        let prog2 = p.progress;
        prop_assert!(prog2 >= prog1, "progress should be monotonically increasing");
    }

    // ── GU-10: Progress complete at total ──────────────────────────

    #[test]
    fn gu10_progress_complete(total in 1u64..10000) {
        let mut p = GuideProgress::new(GuideWorkflow::Investigate, 0);
        p.update(total, total, 1000);
        prop_assert!(p.is_complete());
    }

    // ── GU-11: Default tolerance is 100 ────────────────────────────

    #[test]
    fn gu11_default_tolerance(_dummy in 0u8..1) {
        let ctx: GuideContext = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(ctx.tolerance_ms, 100);
    }

    // ── GU-12: GuideContext serde roundtrip ─────────────────────────

    #[test]
    fn gu12_context_serde(tol in 1u64..10000) {
        let ctx = GuideContext {
            artifact_paths: vec!["a.ftreplay".into()],
            baseline_path: Some("base.ftreplay".into()),
            candidate_path: Some("cand.ftreplay".into()),
            override_path: None,
            suite_dir: Some("tests/".into()),
            budget_path: None,
            tolerance_ms: tol,
            results: std::collections::BTreeMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let restored: GuideContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.tolerance_ms, tol);
        prop_assert_eq!(restored.artifact_paths, ctx.artifact_paths);
    }

    // ── GU-13: Robot command roundtrip ──────────────────────────────

    #[test]
    fn gu13_robot_command_roundtrip(idx in 0usize..3) {
        let cmds = [
            GuideRobotCommand::Start,
            GuideRobotCommand::Step,
            GuideRobotCommand::List,
        ];
        let cmd = &cmds[idx];
        let s = cmd.as_str();
        let parsed = GuideRobotCommand::from_str_command(s);
        prop_assert_eq!(parsed.as_ref(), Some(cmd));
    }

    // ── GU-14: start_workflow returns correct step count ───────────

    #[test]
    fn gu14_start_step_count(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let data = start_workflow(wf, GuideContext {
            artifact_paths: vec!["test.ftreplay".into()],
            baseline_path: Some("base.ftreplay".into()),
            ..Default::default()
        });
        prop_assert_eq!(data.total_steps, wf.step_count());
        prop_assert_eq!(data.steps.len(), wf.step_count());
    }

    // ── GU-15: list_workflows returns 3 ────────────────────────────

    #[test]
    fn gu15_list_count(_dummy in 0u8..1) {
        let list = list_workflows();
        prop_assert_eq!(list.workflows.len(), 3);
    }

    // ── GU-16: GuideStartData serde roundtrip ──────────────────────

    #[test]
    fn gu16_start_data_serde(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let data = start_workflow(wf, GuideContext {
            artifact_paths: vec!["test.ftreplay".into()],
            baseline_path: Some("base.ftreplay".into()),
            ..Default::default()
        });
        let json = serde_json::to_string(&data).unwrap();
        let restored: frankenterm_core::replay_guide::GuideStartData =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, data);
    }

    // ── GU-17: GuideListData serde roundtrip ───────────────────────

    #[test]
    fn gu17_list_data_serde(_dummy in 0u8..1) {
        let list = list_workflows();
        let json = serde_json::to_string(&list).unwrap();
        let restored: frankenterm_core::replay_guide::GuideListData =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, list);
    }

    // ── GU-18: Step IDs are unique within workflow ─────────────────

    #[test]
    fn gu18_step_ids_unique(idx in 0usize..3) {
        let wf = ALL_WORKFLOWS[idx];
        let descs = wf.step_descriptions();
        let mut ids: Vec<&str> = descs.iter().map(|d| d.step_id.as_str()).collect();
        let len_before = ids.len();
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), len_before, "duplicate step_ids in {:?}", wf);
    }

    // ── GU-19: MCP schema has additionalProperties=false ───────────

    #[test]
    fn gu19_schema_strict(_dummy in 0u8..1) {
        let schema = guide_tool_schema();
        let addl = schema["additionalProperties"].as_bool();
        prop_assert_eq!(addl, Some(false));
    }

    // ── GU-20: ETA decreases as progress increases ─────────────────

    #[test]
    fn gu20_eta_decreasing(total in 1000u64..10000) {
        let mut p = GuideProgress::new(GuideWorkflow::Investigate, 0);

        p.update(total / 4, total, 1000);
        let eta1 = p.eta_ms;

        p.update(total / 2, total, 2000);
        let eta2 = p.eta_ms;

        p.update(3 * total / 4, total, 3000);
        let eta3 = p.eta_ms;

        // ETA should generally decrease (or stay same if rate varies)
        // With constant rate, eta should strictly decrease
        prop_assert!(eta2 <= eta1, "ETA should decrease: {} <= {}", eta2, eta1);
        prop_assert!(eta3 <= eta2, "ETA should decrease: {} <= {}", eta3, eta2);
    }
}
