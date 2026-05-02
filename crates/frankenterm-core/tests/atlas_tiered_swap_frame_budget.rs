use frankenterm_core::atlas_tiered_swap::{
    DiskBudgetEstimator, DiskHandoffDirection, DiskTierHandoff, FrameBudgetSwapDeferrer,
    HostRamStagingAllocation, StagingTransferDirection, StagingTransferEvent,
};

fn staging_event(region_id: u64, bytes: u64, frame_id: u64) -> StagingTransferEvent {
    StagingTransferEvent {
        region_id,
        direction: StagingTransferDirection::Promote,
        allocation: HostRamStagingAllocation {
            region_id,
            offset: region_id.saturating_mul(4096),
            bytes,
        },
        frame_id,
    }
}

fn disk_handoff(region_id: u64, bytes: u64, frame_id: u64) -> DiskTierHandoff {
    DiskTierHandoff {
        region_id,
        direction: DiskHandoffDirection::Demote,
        bytes,
        frame_id,
    }
}

#[test]
fn staging_swap_deferrer_requires_frame_boundary_reset_for_deferred_work() {
    let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(1000);
    let pending = [staging_event(1, 2000, 10), staging_event(2, 2000, 10)];

    let (admitted, deferred) = deferrer.partition(&pending, 3);

    assert_eq!(admitted, vec![pending[0]]);
    assert_eq!(deferred, vec![pending[1]]);
    assert_eq!(deferrer.admitted_bytes(), 2000);

    let (still_admitted, still_deferred) = deferrer.partition(&deferred, 3);

    assert!(still_admitted.is_empty());
    assert_eq!(still_deferred, deferred);

    deferrer.reset_for_new_frame();
    let (next_frame_admitted, next_frame_deferred) = deferrer.partition(&deferred, 3);

    assert_eq!(next_frame_admitted, deferred);
    assert!(next_frame_deferred.is_empty());
    assert_eq!(deferrer.admitted_bytes(), 2000);
}

#[test]
fn disk_budget_estimator_requires_frame_boundary_reset_for_deferred_handoffs() {
    let mut estimator = DiskBudgetEstimator::with_throughput(1000);
    let pending = [disk_handoff(7, 2000, 22), disk_handoff(8, 2000, 22)];

    let (admitted, deferred) = estimator.partition(&pending, 3);

    assert_eq!(admitted, vec![pending[0]]);
    assert_eq!(deferred, vec![pending[1]]);
    assert_eq!(estimator.admitted_bytes(), 2000);

    let (still_admitted, still_deferred) = estimator.partition(&deferred, 3);

    assert!(still_admitted.is_empty());
    assert_eq!(still_deferred, deferred);

    estimator.reset_for_new_frame();
    let (next_frame_admitted, next_frame_deferred) = estimator.partition(&deferred, 3);

    assert_eq!(next_frame_admitted, deferred);
    assert!(next_frame_deferred.is_empty());
    assert_eq!(estimator.admitted_bytes(), 2000);
}
