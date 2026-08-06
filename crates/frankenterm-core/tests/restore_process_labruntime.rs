//! Deterministic LabRuntime coverage for finite process-disposition reports.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

use frankenterm_core::restore_process::{
    LAUNCH_RESULT_SAMPLE_CAP, LaunchAction, ProcessDispositionReason, ProcessLauncher, ProcessPlan,
};
use frankenterm_core::test_fixtures::lab_runtime::{
    assert_ran_to_completion, lab_runtime_test_with_seed,
};

const PROCESS_DISPOSITION_LAB_SEED: u64 = 0xF17E_D150_0517_10A0;

fn run_under_lab(test: impl FnOnce() + Send + 'static) {
    let report = lab_runtime_test_with_seed(PROCESS_DISPOSITION_LAB_SEED, move |_cx| async move {
        test();
    });
    assert_ran_to_completion(&report);
    assert!(report.oracles_passed, "LabRuntime oracles must pass");
}

// ===========================================================================
// 1. manual shell disposition has no external-effect path
// ===========================================================================

#[test]
fn execute_manual_shell_disposition_has_no_external_effect_path() {
    run_under_lab(|| {
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual(
                ProcessDispositionReason::CapturedShellRequiresManualRecovery,
            ),
        }];

        let report = ProcessLauncher::execute(&plans);
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
    run_under_lab(|| {
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

        let report = ProcessLauncher::execute(&plans);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.manual_count(), 2);
        assert_eq!(report.plans_settled(), 3);
        assert_eq!(report.result_sample().len(), 3);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret"));
    });
}

// ===========================================================================
// 3. execute_empty_plans
// ===========================================================================

#[test]
fn execute_empty_plans() {
    run_under_lab(|| {
        let report = ProcessLauncher::execute(&[]);
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
    run_under_lab(|| {
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
        let report = ProcessLauncher::execute(&plans);
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
    run_under_lab(|| {
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual(
                ProcessDispositionReason::CapturedInteractiveProgramRequiresManualRecovery,
            ),
        }];
        let report = ProcessLauncher::execute(&plans);
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
    run_under_lab(|| {
        let plans = (0..1_000_u64)
            .map(|pane_id| ProcessPlan {
                old_pane_id: pane_id,
                new_pane_id: pane_id.saturating_add(10_000),
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
                ),
            })
            .collect::<Vec<_>>();
        let report = ProcessLauncher::execute(&plans);
        assert_eq!(report.plans_total(), 1_000);
        assert_eq!(report.plans_settled(), 1_000);
        assert_eq!(report.manual_count(), 1_000);
        assert_eq!(report.result_sample().len(), LAUNCH_RESULT_SAMPLE_CAP);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("command"));
        assert!(!json.contains("cwd"));
    });
}

// ===========================================================================
// 7. execute_report_result_order_preserved
// ===========================================================================

#[test]
fn execute_report_result_order_preserved() {
    run_under_lab(|| {
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
        let report = ProcessLauncher::execute(&plans);
        assert_eq!(report.result_sample()[0].old_pane_id, 1);
        assert_eq!(report.result_sample()[1].old_pane_id, 2);
    });
}
