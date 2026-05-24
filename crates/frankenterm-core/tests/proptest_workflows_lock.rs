//! Property-based tests for `workflows::lock` public lock-manager behavior.

use frankenterm_core::workflows::{
    ConcurrencyLimitInfo, LockAcquisitionResult, LockManagerHealth, OwnedLockAcquisitionResult,
    PaneWorkflowLockManager,
};
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::sync::Arc;

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
    fn already_locked_attempt_preserves_holder_and_updates_conflict_counter(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
        conflicting_workflow in arb_label(),
        conflicting_execution in arb_label(),
    ) {
        prop_assume!(workflow_name != conflicting_workflow);
        prop_assume!(execution_id != conflicting_execution);

        let manager = PaneWorkflowLockManager::new();
        prop_assert_eq!(
            manager.try_acquire(pane_id, &workflow_name, &execution_id),
            LockAcquisitionResult::Acquired
        );

        let conflict = manager.try_acquire(pane_id, &conflicting_workflow, &conflicting_execution);

        if let LockAcquisitionResult::AlreadyLocked {
            held_by_workflow,
            held_by_execution,
            locked_since_ms,
        } = conflict
        {
            prop_assert_eq!(held_by_workflow, workflow_name);
            prop_assert_eq!(held_by_execution, execution_id);
            prop_assert!(locked_since_ms > 0);
        } else {
            prop_assert!(false, "second acquisition should report the existing holder");
        }

        let lock = manager.is_locked(pane_id).expect("original lock should remain held");
        let health = manager.health();
        prop_assert_eq!(lock.workflow_name, workflow_name);
        prop_assert_eq!(lock.execution_id, execution_id);
        prop_assert_eq!(health.acquisitions_total, 1);
        prop_assert_eq!(health.pane_already_locked_total, 1);
        prop_assert_eq!(health.active_locks, 1);
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

    #[test]
    fn proptest_workflow_lock_health_is_safe_matches_public_ratio_contract(
        releases_total in any::<u64>(),
        force_releases_total in any::<u64>(),
        mutex_poisoned_recoveries_total in 0_u64..4,
        active_locks in 0_u32..100,
    ) {
        let health = LockManagerHealth {
            releases_total,
            force_releases_total,
            mutex_poisoned_recoveries_total,
            active_locks,
            ..LockManagerHealth::default()
        };

        let expected = if mutex_poisoned_recoveries_total > 0 {
            false
        } else if releases_total == 0 {
            force_releases_total == 0
        } else {
            u128::from(force_releases_total) * 100 <= u128::from(releases_total) * 5
        };

        prop_assert_eq!(health.is_safe(), expected);
    }

    #[test]
    fn proptest_workflow_lock_zero_limit_behaves_as_unbounded(
        pane_ids in prop::collection::btree_set(1u64..10_000, 1..8),
        workflow_prefix in arb_label(),
        execution_prefix in arb_label(),
    ) {
        let manager = PaneWorkflowLockManager::new();

        for (idx, pane_id) in pane_ids.iter().enumerate() {
            let workflow = format!("{workflow_prefix}-{idx}");
            let execution = format!("{execution_prefix}-{idx}");
            let result = manager.try_acquire_with_limit(*pane_id, &workflow, &execution, 0);
            prop_assert_eq!(result.unwrap(), LockAcquisitionResult::Acquired);
        }

        prop_assert_eq!(manager.active_count(), pane_ids.len());
        prop_assert_eq!(manager.health().concurrency_limit_blocks_total, 0);
    }

    #[test]
    fn proptest_workflow_lock_wrong_execution_release_preserves_lock(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
        wrong_execution_id in arb_label(),
    ) {
        prop_assume!(execution_id != wrong_execution_id);

        let manager = PaneWorkflowLockManager::new();
        prop_assert_eq!(
            manager.try_acquire(pane_id, &workflow_name, &execution_id),
            LockAcquisitionResult::Acquired
        );

        prop_assert!(!manager.release(pane_id, &wrong_execution_id));
        let lock = manager.is_locked(pane_id).expect("lock should remain held");
        let health = manager.health();

        prop_assert_eq!(lock.workflow_name, workflow_name);
        prop_assert_eq!(lock.execution_id, execution_id);
        prop_assert_eq!(health.release_mismatched_total, 1);
        prop_assert_eq!(health.releases_total, 0);
        prop_assert_eq!(health.active_locks, 1);
    }

    #[test]
    fn proptest_workflow_lock_repeated_wrong_releases_are_observable_but_non_destructive(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
        wrong_execution_ids in prop::collection::vec(arb_label(), 1..8),
    ) {
        prop_assume!(wrong_execution_ids.iter().all(|wrong| wrong != &execution_id));

        let manager = PaneWorkflowLockManager::new();
        prop_assert_eq!(
            manager.try_acquire(pane_id, &workflow_name, &execution_id),
            LockAcquisitionResult::Acquired
        );

        for wrong_execution_id in &wrong_execution_ids {
            prop_assert!(!manager.release(pane_id, wrong_execution_id));
            let lock = manager.is_locked(pane_id).expect("lock should remain held");
            prop_assert_eq!(lock.workflow_name, workflow_name);
            prop_assert_eq!(lock.execution_id, execution_id);
            prop_assert_eq!(manager.active_count(), 1);
        }

        let health = manager.health();
        prop_assert_eq!(
            health.release_mismatched_total,
            u64::try_from(wrong_execution_ids.len()).expect("wrong execution count fits u64")
        );
        prop_assert_eq!(health.releases_total, 0);
        prop_assert_eq!(health.active_locks, 1);

        prop_assert!(manager.release(pane_id, &execution_id));
        let health_after_release = manager.health();
        prop_assert_eq!(health_after_release.releases_total, 1);
        prop_assert_eq!(health_after_release.active_locks, 0);
        prop_assert!(manager.is_locked(pane_id).is_none());
    }

    #[test]
    fn proptest_workflow_lock_force_release_missing_lock_updates_only_force_counter(
        pane_id in 1u64..10_000,
    ) {
        let manager = PaneWorkflowLockManager::new();

        prop_assert!(manager.force_release(pane_id).is_none());

        let health = manager.health();
        prop_assert_eq!(health.force_releases_total, 1);
        prop_assert_eq!(health.releases_total, 0);
        prop_assert_eq!(health.active_locks, 0);
        prop_assert!(!health.is_safe());
        prop_assert!(manager.is_locked(pane_id).is_none());
    }

    #[test]
    fn proptest_workflow_lock_force_release_updates_health(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
    ) {
        let manager = PaneWorkflowLockManager::new();
        prop_assert_eq!(
            manager.try_acquire(pane_id, &workflow_name, &execution_id),
            LockAcquisitionResult::Acquired
        );

        let removed = manager
            .force_release(pane_id)
            .expect("force release should return the held lock");
        prop_assert_eq!(removed.pane_id, pane_id);
        prop_assert_eq!(removed.workflow_name, workflow_name);
        prop_assert_eq!(removed.execution_id, execution_id);

        let health = manager.health();
        prop_assert_eq!(health.force_releases_total, 1);
        prop_assert_eq!(health.releases_total, 1);
        prop_assert_eq!(health.active_locks, 0);
        prop_assert!(!health.is_safe());
        prop_assert!(manager.is_locked(pane_id).is_none());
    }

    #[test]
    fn proptest_workflow_lock_owned_guard_drop_restores_unlocked_state(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
    ) {
        let manager = Arc::new(PaneWorkflowLockManager::new());

        {
            let guard = manager
                .try_acquire_owned_guarded(pane_id, &workflow_name, &execution_id)
                .expect("owned guard should acquire");
            prop_assert_eq!(guard.pane_id(), pane_id);
            prop_assert_eq!(guard.execution_id(), execution_id);
            prop_assert_eq!(manager.active_count(), 1);
        }

        let health = manager.health();
        prop_assert!(manager.is_locked(pane_id).is_none());
        prop_assert_eq!(health.acquisitions_total, 1);
        prop_assert_eq!(health.releases_total, 1);
        prop_assert_eq!(health.active_locks, 0);
    }

    #[test]
    fn proptest_workflow_lock_owned_guard_defuse_preserves_lock_until_manual_release(
        pane_id in 1u64..10_000,
        workflow_name in arb_label(),
        execution_id in arb_label(),
        wrong_execution_id in arb_label(),
    ) {
        prop_assume!(execution_id != wrong_execution_id);

        let manager = Arc::new(PaneWorkflowLockManager::new());
        let guard = manager
            .try_acquire_owned_guarded(pane_id, &workflow_name, &execution_id)
            .expect("owned guard should acquire");

        prop_assert_eq!(manager.active_count(), 1);
        guard.defuse();

        let health_after_defuse = manager.health();
        prop_assert_eq!(health_after_defuse.releases_total, 0);
        prop_assert_eq!(health_after_defuse.active_locks, 1);
        let lock = manager
            .is_locked(pane_id)
            .expect("defused guard leaves lock held for downstream handoff");
        prop_assert_eq!(lock.workflow_name, workflow_name);
        prop_assert_eq!(lock.execution_id, execution_id);

        prop_assert!(!manager.release(pane_id, &wrong_execution_id));
        prop_assert_eq!(manager.active_count(), 1);
        prop_assert!(manager.release(pane_id, &execution_id));

        let health_after_release = manager.health();
        prop_assert_eq!(health_after_release.release_mismatched_total, 1);
        prop_assert_eq!(health_after_release.releases_total, 1);
        prop_assert_eq!(health_after_release.active_locks, 0);
        prop_assert!(manager.is_locked(pane_id).is_none());
    }

    #[test]
    fn proptest_workflow_lock_owned_full_preserves_conflict_details_before_limit_errors(
        pane_ids in prop::collection::btree_set(1u64..10_000, 2..8),
        requested_limit in 1usize..8,
        workflow_prefix in arb_label(),
        execution_prefix in arb_label(),
    ) {
        prop_assume!(requested_limit < pane_ids.len());

        let manager = Arc::new(PaneWorkflowLockManager::new());
        let ordered_ids: Vec<u64> = pane_ids.iter().copied().collect();
        let mut guards = Vec::new();

        for (idx, pane_id) in ordered_ids.iter().take(requested_limit).enumerate() {
            let workflow = format!("{workflow_prefix}-{idx}");
            let execution = format!("{execution_prefix}-{idx}");
            let acquired = manager
                .try_acquire_with_limit_owned_full(*pane_id, &workflow, &execution, requested_limit)
                .expect("initial owned full acquire should not hit the limit");
            prop_assert!(
                acquired.is_acquired(),
                "fresh pane should not report an existing lock"
            );
            if let OwnedLockAcquisitionResult::Acquired(guard) = acquired {
                guards.push(guard);
            }
        }

        prop_assert_eq!(manager.active_count(), requested_limit);

        let first_pane = ordered_ids[0];
        let same_pane_conflict = manager
            .try_acquire_with_limit_owned_full(
                first_pane,
                "conflicting-workflow",
                "conflicting-execution",
                requested_limit,
            )
            .expect("same-pane conflict should be reported before the global limit");

        prop_assert!(
            same_pane_conflict.is_already_locked(),
            "same pane should preserve already-locked details"
        );
        if let OwnedLockAcquisitionResult::AlreadyLocked {
            held_by_workflow,
            held_by_execution,
            locked_since_ms,
        } = same_pane_conflict
        {
            prop_assert_eq!(held_by_workflow, format!("{workflow_prefix}-0"));
            prop_assert_eq!(held_by_execution, format!("{execution_prefix}-0"));
            prop_assert!(locked_since_ms > 0);
        }

        let overflow_pane = ordered_ids[requested_limit];
        let overflow = manager.try_acquire_with_limit_owned_full(
            overflow_pane,
            "overflow-workflow",
            "overflow-execution",
            requested_limit,
        );

        prop_assert_eq!(
            overflow.expect_err("different pane should hit the global limit"),
            ConcurrencyLimitInfo {
                active: requested_limit,
                limit: requested_limit,
            }
        );
        prop_assert_eq!(manager.active_count(), requested_limit);

        drop(guards);
        let health = manager.health();
        prop_assert_eq!(health.releases_total, u64::try_from(requested_limit).unwrap());
        prop_assert_eq!(health.active_locks, 0);
        prop_assert!(manager.active_locks().is_empty());
    }
}
