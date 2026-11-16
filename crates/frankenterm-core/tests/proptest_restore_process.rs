//! Property-based tests for the `restore_process` module.
//!
//! Covers finite classification and bounded aggregate-report invariants.

use frankenterm_core::restore_process::{
    LAUNCH_RESULT_SAMPLE_CAP, LaunchAction, ProcessDispositionReason, ProcessLauncher, ProcessPlan,
};
use proptest::prelude::*;

fn finite_action() -> impl Strategy<Value = LaunchAction> {
    prop_oneof![
        Just(LaunchAction::Skip(
            ProcessDispositionReason::DefaultShellCreated
        )),
        Just(LaunchAction::Manual(
            ProcessDispositionReason::CapturedShellRequiresManualRecovery
        )),
        Just(LaunchAction::Manual(
            ProcessDispositionReason::CapturedAgentRequiresManualRecovery
        )),
        Just(LaunchAction::Manual(
            ProcessDispositionReason::CapturedInteractiveProgramRequiresManualRecovery
        )),
        Just(LaunchAction::Manual(
            ProcessDispositionReason::CapturedForegroundProcessRequiresManualRecovery
        )),
        Just(LaunchAction::Manual(
            ProcessDispositionReason::CapturedProcessStateUnavailableRequiresManualRecovery
        )),
    ]
}

proptest! {
    #[test]
    fn report_totals_are_exact_and_sample_is_bounded(
        actions in proptest::collection::vec(finite_action(), 0..2_000)
    ) {
        let plans = actions
            .into_iter()
            .enumerate()
            .map(|(index, action)| ProcessPlan {
                old_pane_id: u64::try_from(index).expect("generated index fits u64"),
                new_pane_id: u64::try_from(index.saturating_add(10_000))
                    .expect("generated index fits u64"),
                action,
            })
            .collect::<Vec<_>>();
        let report = ProcessLauncher::execute(&plans);
        prop_assert_eq!(report.plans_total(), plans.len());
        prop_assert_eq!(report.plans_settled(), plans.len());
        prop_assert_eq!(
            report
                .manual_count()
                .saturating_add(report.skipped_count()),
            report.plans_settled(),
        );
        prop_assert_eq!(
            report.result_sample().len(),
            plans.len().min(LAUNCH_RESULT_SAMPLE_CAP),
        );
        prop_assert!(report.interruption().is_none());
    }

    #[test]
    fn report_sample_is_the_deterministic_plan_prefix(
        actions in proptest::collection::vec(finite_action(), 0..500)
    ) {
        let plans = actions
            .into_iter()
            .enumerate()
            .map(|(index, action)| ProcessPlan {
                old_pane_id: u64::try_from(index).expect("generated index fits u64"),
                new_pane_id: u64::try_from(index.saturating_add(100)).expect("fits u64"),
                action,
            })
            .collect::<Vec<_>>();
        let first = ProcessLauncher::execute(&plans);
        let second = ProcessLauncher::execute(&plans);
        prop_assert_eq!(first.result_sample(), second.result_sample());
        for (sample, plan) in first.result_sample().iter().zip(plans.iter()) {
            prop_assert_eq!(sample.old_pane_id, plan.old_pane_id);
            prop_assert_eq!(sample.new_pane_id, plan.new_pane_id);
        }
    }
}
