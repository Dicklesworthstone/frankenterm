//! Property-based tests for `workflows::lock` public lock-manager behavior.

use frankenterm_core::workflows::{
    ConcurrencyLimitInfo, LockAcquisitionResult, PaneWorkflowLockManager,
};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn arb_label() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_.-]{1,24}".prop_map(|s| s)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn active_locks_match_unique_successful_acquisitions(
        pane_ids in prop::collection::btree_set(1u64..10_000, 1..8),
        workflow_prefix in arb_label(),
        execution_prefix in arb_label(),
    ) {
        let manager = PaneWorkflowLockManager::new();

        for (idx, pane_id) in pane_ids.iter().enumerate() {
            let workflow = format!("{workflow_prefix}-{idx}");
            let execution = format!("{execution_prefix}-{idx}");
            let result = manager.try_acquire(*pane_id, &workflow, &execution);
            prop_assert_eq!(result, LockAcquisitionResult::Acquired);
        }

        let active_panes: BTreeSet<u64> =
            manager.active_locks().into_iter().map(|info| info.pane_id).collect();

        prop_assert_eq!(manager.active_count(), pane_ids.len());
        prop_assert_eq!(active_panes, pane_ids);
    }

    #[test]
    fn concurrency_limit_rejects_after_limit_is_reached(
        pane_ids in prop::collection::btree_set(1u64..10_000, 2..8),
        requested_limit in 1usize..8,
        workflow_prefix in arb_label(),
        execution_prefix in arb_label(),
    ) {
        prop_assume!(requested_limit < pane_ids.len());

        let manager = PaneWorkflowLockManager::new();
        let ordered_ids: Vec<u64> = pane_ids.iter().copied().collect();

        for (idx, pane_id) in ordered_ids.iter().take(requested_limit).enumerate() {
            let workflow = format!("{workflow_prefix}-{idx}");
            let execution = format!("{execution_prefix}-{idx}");
            let result = manager.try_acquire_with_limit(*pane_id, &workflow, &execution, requested_limit);
            prop_assert_eq!(result.unwrap(), LockAcquisitionResult::Acquired);
        }

        let overflow_pane = ordered_ids[requested_limit];
        let overflow = manager.try_acquire_with_limit(
            overflow_pane,
            "overflow-workflow",
            "overflow-exec",
            requested_limit,
        );

        prop_assert_eq!(
            overflow,
            Err(ConcurrencyLimitInfo {
                active: requested_limit,
                limit: requested_limit,
            })
        );
        prop_assert_eq!(manager.active_count(), requested_limit);
    }

    #[test]
    fn guard_drop_and_force_release_restore_unlocked_state(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
        second_execution in arb_label(),
    ) {
        let manager = PaneWorkflowLockManager::new();

        {
            let guard = manager
                .acquire_guard(pane_id, &workflow_name, &execution_id)
                .expect("guard should acquire");
            prop_assert_eq!(guard.pane_id(), pane_id);
            prop_assert_eq!(guard.execution_id(), execution_id);
            prop_assert!(manager.is_locked(pane_id).is_some());
        }

        prop_assert!(manager.is_locked(pane_id).is_none());

        let reacquire = manager.try_acquire(pane_id, &workflow_name, &second_execution);
        prop_assert_eq!(reacquire, LockAcquisitionResult::Acquired);

        let removed = manager.force_release(pane_id).expect("force release should return lock");
        prop_assert_eq!(removed.pane_id, pane_id);
        prop_assert_eq!(removed.workflow_name, workflow_name);
        prop_assert_eq!(removed.execution_id, second_execution);
        prop_assert!(manager.is_locked(pane_id).is_none());
        prop_assert_eq!(manager.active_count(), 0);
    }
}
