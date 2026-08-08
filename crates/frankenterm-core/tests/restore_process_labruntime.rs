//! Deterministic LabRuntime coverage for Cx-aware process disposition.
//!
//! Every settlement test threads the LabRuntime-provided `Cx` into
//! `ProcessLauncher::execute_cx`; capability-failure cases use isolated test
//! contexts so cancelling one case cannot cancel the LabRuntime root task.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

use std::collections::HashMap;

use frankenterm_core::cx::{Budget, Cx};
use frankenterm_core::outcome::CancelKind;
use frankenterm_core::restore_process::{
    LAUNCH_RESULT_SAMPLE_CAP, LaunchAction, LaunchInterruptionPhase, LaunchInterruptionReason,
    ProcessDispositionInput, ProcessDispositionReason, ProcessLauncher, ProcessPlan,
};
use frankenterm_core::test_fixtures::lab_runtime::{
    assert_ran_to_completion, lab_runtime_test_with_seed,
};

const PROCESS_DISPOSITION_LAB_SEED: u64 = 0xF17E_D150_0517_10A0;

fn run_under_lab(test: impl FnOnce(Cx) + Send + 'static) {
    let report = lab_runtime_test_with_seed(PROCESS_DISPOSITION_LAB_SEED, move |cx| async move {
        test(cx);
    });
    assert_ran_to_completion(&report);
    assert!(report.oracles_passed, "LabRuntime oracles must pass");
}

// ===========================================================================
// 1. manual shell disposition has no external-effect path
// ===========================================================================

#[test]
fn execute_manual_shell_disposition_has_no_external_effect_path() {
    run_under_lab(|cx| {
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual(
                ProcessDispositionReason::CapturedShellRequiresManualRecovery,
            ),
        }];

        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.plans_total(), 1);
        assert_eq!(report.plans_settled(), 1);
        assert_eq!(report.manual_count(), 1);
        assert_eq!(report.result_sample().len(), 1);
    });
}

// ===========================================================================
// 2. execute_mixed_plan
// ===========================================================================

#[test]
fn execute_mixed_plan() {
    run_under_lab(|cx| {
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedShellRequiresManualRecovery,
                ),
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated),
            },
            ProcessPlan {
                old_pane_id: 3,
                new_pane_id: 300,
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedInteractiveProgramRequiresManualRecovery,
                ),
            },
        ];

        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.manual_count(), 2);
        assert_eq!(report.plans_settled(), 3);
        assert_eq!(report.result_sample().len(), 3);
    });
}

// ===========================================================================
// 3. execute_empty_plans
// ===========================================================================

#[test]
fn execute_empty_plans() {
    run_under_lab(|cx| {
        let report = ProcessLauncher::execute_cx(&cx, &[]);
        assert_eq!(report.plans_total(), 0);
        assert_eq!(report.plans_settled(), 0);
        assert!(report.result_sample().is_empty());
    });
}

// ===========================================================================
// 4. execute_skip_only
// ===========================================================================

#[test]
fn execute_skip_only() {
    run_under_lab(|cx| {
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated),
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated),
            },
        ];
        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.skipped_count(), 2);
        assert_eq!(report.plans_settled(), 2);
        assert_eq!(report.result_sample().len(), 2);
    });
}

// ===========================================================================
// 5. execute_manual_only
// ===========================================================================

#[test]
fn execute_manual_only() {
    run_under_lab(|cx| {
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual(
                ProcessDispositionReason::CapturedInteractiveProgramRequiresManualRecovery,
            ),
        }];
        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.manual_count(), 1);
        assert_eq!(report.plans_settled(), 1);
        assert_eq!(report.result_sample().len(), 1);
    });
}

// ===========================================================================
// 6. large agent disposition report stays bounded
// ===========================================================================

#[test]
fn execute_large_manual_plan_keeps_bounded_sample() {
    run_under_lab(|cx| {
        let plans = (0..1_000_u64)
            .map(|pane_id| ProcessPlan {
                old_pane_id: pane_id,
                new_pane_id: pane_id.saturating_add(10_000),
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
                ),
            })
            .collect::<Vec<_>>();
        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.plans_total(), 1_000);
        assert_eq!(report.plans_settled(), 1_000);
        assert_eq!(report.manual_count(), 1_000);
        assert_eq!(report.result_sample().len(), LAUNCH_RESULT_SAMPLE_CAP);
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let report_keys = value
            .as_object()
            .expect("launch report must serialize as an object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            report_keys,
            std::collections::BTreeSet::from([
                "interruption",
                "manual",
                "plans_settled",
                "plans_total",
                "result_sample",
                "skipped",
            ])
        );
        for result in value["result_sample"]
            .as_array()
            .expect("result sample must serialize as an array")
        {
            let result_keys = result
                .as_object()
                .expect("sampled result must serialize as an object")
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                result_keys,
                std::collections::BTreeSet::from([
                    "action",
                    "new_pane_id",
                    "old_pane_id",
                    "reason",
                ])
            );
        }
        assert!(json.len() < 10_000);
    });
}

// ===========================================================================
// 7. execute_report_result_order_preserved
// ===========================================================================

#[test]
fn execute_report_result_order_preserved() {
    run_under_lab(|cx| {
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedShellRequiresManualRecovery,
                ),
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated),
            },
        ];
        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.result_sample()[0].old_pane_id, 1);
        assert_eq!(report.result_sample()[1].old_pane_id, 2);
    });
}

// ===========================================================================
// 8. pre-cancellation is typed and settles nothing
// ===========================================================================

#[test]
fn execute_cx_pre_cancelled_reports_typed_zero_progress() {
    run_under_lab(|_lab_cx| {
        const CANCEL_CANARY: &str = "process-disposition-private-cancel-reason";
        let cx = Cx::for_testing();
        cx.cancel_with(CancelKind::User, Some(CANCEL_CANARY));
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual(
                ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
            ),
        }];

        let report = ProcessLauncher::execute_cx(&cx, &plans);
        let interruption = report
            .interruption()
            .expect("pre-cancelled execution must retain an interruption");
        assert_eq!(interruption.plan_index, 0);
        assert_eq!(interruption.phase, LaunchInterruptionPhase::BeforePlan);
        assert_eq!(interruption.reason, LaunchInterruptionReason::Cancelled);
        assert_eq!(report.plans_total(), 1);
        assert_eq!(report.plans_settled(), 0);
        assert_eq!(report.manual_count(), 0);
        assert!(report.result_sample().is_empty());

        let encoded = serde_json::to_string(&report).expect("serialize interrupted report");
        assert!(!encoded.contains(CANCEL_CANARY));
        assert!(!format!("{report:?}").contains(CANCEL_CANARY));
    });
}

// ===========================================================================
// 9. exhausted deadline and quotas retain distinct failure classes
// ===========================================================================

#[test]
fn execute_cx_preserves_deadline_poll_and_cost_failure_classes() {
    run_under_lab(|_lab_cx| {
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated),
        }];

        for (budget, expected) in [
            (
                Budget::new().with_deadline(Default::default()),
                LaunchInterruptionReason::DeadlineExceeded,
            ),
            (
                Budget::new().with_poll_quota(0),
                LaunchInterruptionReason::PollQuotaExhausted,
            ),
            (
                Budget::new().with_cost_quota(0),
                LaunchInterruptionReason::CostQuotaExhausted,
            ),
        ] {
            let cx = Cx::for_testing_with_budget(budget);
            let report = ProcessLauncher::execute_cx(&cx, &plans);
            assert_eq!(
                report.interruption().map(|value| value.reason),
                Some(expected)
            );
            assert_eq!(report.plans_settled(), 0);
            assert_eq!(report.skipped_count(), 0);
            assert!(report.result_sample().is_empty());
        }
    });
}

// ===========================================================================
// 10. quota exhaustion preserves an exact settled prefix
// ===========================================================================

#[test]
fn execute_cx_poll_quota_exhaustion_preserves_exact_prefix() {
    run_under_lab(|_lab_cx| {
        let plans = (0_u64..3)
            .map(|pane_id| ProcessPlan {
                old_pane_id: pane_id,
                new_pane_id: pane_id.saturating_add(100),
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedShellRequiresManualRecovery,
                ),
            })
            .collect::<Vec<_>>();
        let cx = Cx::for_testing_with_budget(Budget::new().with_poll_quota(2));

        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.plans_total(), 3);
        assert_eq!(report.plans_settled(), 2);
        assert_eq!(report.manual_count(), 2);
        assert_eq!(
            report
                .result_sample()
                .iter()
                .map(|result| result.old_pane_id)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            report
                .interruption()
                .map(|value| (value.plan_index, value.phase, value.reason,)),
            Some((
                2,
                LaunchInterruptionPhase::BeforePlan,
                LaunchInterruptionReason::PollQuotaExhausted,
            ))
        );
    });
}

// ===========================================================================
// 11. borrowed classification text never reaches plans or reports
// ===========================================================================

#[test]
fn execute_cx_does_not_retain_borrowed_process_name() {
    run_under_lab(|cx| {
        const PROCESS_NAME_CANARY: &str = "private-project-token-process";
        let pane_id_map = HashMap::from([(7_u64, 70_u64)]);
        let plans = ProcessLauncher::plan_inputs(
            &pane_id_map,
            [ProcessDispositionInput {
                pane_id: 7,
                foreground_process_name: Some(PROCESS_NAME_CANARY),
                shell_present: false,
                agent_present: false,
            }],
        );
        assert!(!format!("{plans:?}").contains(PROCESS_NAME_CANARY));

        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.plans_settled(), 1);
        assert_eq!(report.manual_count(), 1);
        assert_eq!(
            report.result_sample()[0].reason,
            ProcessDispositionReason::CapturedForegroundProcessRequiresManualRecovery
        );
        let encoded = serde_json::to_string(&report).expect("serialize content-free report");
        assert!(!encoded.contains(PROCESS_NAME_CANARY));
        assert!(!format!("{report:?}").contains(PROCESS_NAME_CANARY));
    });
}
