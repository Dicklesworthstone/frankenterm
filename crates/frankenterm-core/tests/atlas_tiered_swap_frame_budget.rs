use frankenterm_core::atlas_tiered_swap::{
    DiskBudgetEstimator, DiskHandoffDirection, DiskTierHandoff, FrameBudgetSwapDeferrer,
    HostRamStagingAllocation, StagingTransferDirection, StagingTransferEvent,
};
use proptest::prelude::*;

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

fn staging_events() -> impl Strategy<Value = Vec<StagingTransferEvent>> {
    prop::collection::vec((any::<u64>(), 1_u64..=1_000_000, any::<u64>()), 0..32).prop_map(
        |items| {
            items
                .into_iter()
                .map(|(region_id, bytes, frame_id)| staging_event(region_id, bytes, frame_id))
                .collect()
        },
    )
}

fn disk_handoffs() -> impl Strategy<Value = Vec<DiskTierHandoff>> {
    prop::collection::vec((any::<u64>(), 1_u64..=1_000_000, any::<u64>()), 0..32).prop_map(
        |items| {
            items
                .into_iter()
                .map(|(region_id, bytes, frame_id)| disk_handoff(region_id, bytes, frame_id))
                .collect()
        },
    )
}

#[test]
fn staging_swap_deferrer_requires_frame_boundary_reset_for_deferred_work() {
    let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(1000);
    let pending = [staging_event(1, 2000, 10), staging_event(2, 2000, 10)];

    let (admitted, delayed) = deferrer.partition(&pending, 3);

    assert_eq!(admitted, vec![pending[0]]);
    assert_eq!(delayed, vec![pending[1]]);
    assert_eq!(deferrer.admitted_bytes(), 2000);

    let (still_admitted, still_deferred) = deferrer.partition(&delayed, 3);

    assert!(still_admitted.is_empty());
    assert_eq!(still_deferred, delayed);

    deferrer.reset_for_new_frame();
    let (next_frame_admitted, next_frame_deferred) = deferrer.partition(&delayed, 3);

    assert_eq!(next_frame_admitted, delayed);
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

#[test]
fn staging_swap_deferrer_charges_fractional_carry_once_per_running_total() {
    let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(3000);
    let pending: Vec<_> = (1..=6)
        .map(|region_id| staging_event(region_id, 1500, 10))
        .collect();

    let (admitted, delayed) = deferrer.partition(&pending, 2);

    assert_eq!(admitted.as_slice(), &pending[..4]);
    assert_eq!(delayed.as_slice(), &pending[4..]);
    assert_eq!(deferrer.admitted_bytes(), 6000);
}

#[test]
fn disk_budget_estimator_charges_fractional_carry_once_per_running_total() {
    let mut estimator = DiskBudgetEstimator::with_throughput(3000);
    let pending: Vec<_> = (1..=6)
        .map(|region_id| disk_handoff(region_id, 1500, 22))
        .collect();

    let (admitted, deferred) = estimator.partition(&pending, 2);

    assert_eq!(admitted.as_slice(), &pending[..4]);
    assert_eq!(deferred.as_slice(), &pending[4..]);
    assert_eq!(estimator.admitted_bytes(), 6000);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_frame_budget_zero_throughput_defers_all_nonzero_events(
        events in staging_events(),
        frame_budget_us in any::<u64>(),
    ) {
        let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(0);
        let (admitted, deferred) = deferrer.partition(&events, frame_budget_us);

        prop_assert!(admitted.is_empty());
        prop_assert_eq!(deferred, events);
        prop_assert_eq!(deferrer.admitted_bytes(), 0);
    }

    #[test]
    fn proptest_disk_budget_zero_throughput_defers_all_nonzero_handoffs(
        handoffs in disk_handoffs(),
        disk_budget_us in any::<u64>(),
    ) {
        let mut estimator = DiskBudgetEstimator::with_throughput(0);
        let (admitted, deferred) = estimator.partition(&handoffs, disk_budget_us);

        prop_assert!(admitted.is_empty());
        prop_assert_eq!(deferred, handoffs);
        prop_assert_eq!(estimator.admitted_bytes(), 0);
    }

    #[test]
    fn proptest_frame_budget_zero_budget_defers_nonzero_events(
        bytes in 1_u64..=1_000_000,
        throughput in 1_u64..=1_000_000,
        frame_id in any::<u64>(),
    ) {
        let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(throughput);
        let event = staging_event(1, bytes, frame_id);
        let (admitted, deferred) = deferrer.partition(&[event], 0);

        prop_assert!(admitted.is_empty());
        prop_assert_eq!(deferred, vec![event]);
        prop_assert_eq!(deferrer.admitted_bytes(), 0);
    }

    #[test]
    fn proptest_disk_budget_zero_budget_defers_nonzero_handoffs(
        bytes in 1_u64..=1_000_000,
        throughput in 1_u64..=1_000_000,
        frame_id in any::<u64>(),
    ) {
        let mut estimator = DiskBudgetEstimator::with_throughput(throughput);
        let handoff = disk_handoff(1, bytes, frame_id);
        let (admitted, deferred) = estimator.partition(&[handoff], 0);

        prop_assert!(admitted.is_empty());
        prop_assert_eq!(deferred, vec![handoff]);
        prop_assert_eq!(estimator.admitted_bytes(), 0);
    }

    #[test]
    fn proptest_frame_budget_reset_replays_deferred_events_from_empty_accumulator(
        events in staging_events(),
        throughput in 1_u64..=1_000_000,
        frame_budget_us in 0_u64..=1_000,
    ) {
        let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(throughput);
        let (_, deferred) = deferrer.partition(&events, frame_budget_us);
        deferrer.reset_for_new_frame();
        let before = deferrer.admitted_bytes();
        let (next_admitted, _) = deferrer.partition(&deferred, frame_budget_us);

        prop_assert_eq!(before, 0);
        prop_assert_eq!(
            deferrer.admitted_bytes(),
            next_admitted
                .iter()
                .map(StagingTransferEvent::bytes)
                .fold(0_u64, u64::saturating_add)
        );
    }

    #[test]
    fn proptest_frame_budget_partition_returns_admitted_prefix_and_deferred_suffix(
        events in staging_events(),
        throughput in 1_u64..=1_000_000,
        frame_budget_us in 0_u64..=1_000,
    ) {
        let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(throughput);
        let (admitted, deferred) = deferrer.partition(&events, frame_budget_us);
        let split = admitted.len();

        prop_assert_eq!(admitted.as_slice(), &events[..split]);
        prop_assert_eq!(deferred.as_slice(), &events[split..]);
    }

    #[test]
    fn proptest_disk_budget_reset_replays_deferred_handoffs_from_empty_accumulator(
        handoffs in disk_handoffs(),
        throughput in 1_u64..=1_000_000,
        disk_budget_us in 0_u64..=1_000,
    ) {
        let mut estimator = DiskBudgetEstimator::with_throughput(throughput);
        let (_, deferred) = estimator.partition(&handoffs, disk_budget_us);
        estimator.reset_for_new_frame();
        let before = estimator.admitted_bytes();
        let (next_admitted, _) = estimator.partition(&deferred, disk_budget_us);

        prop_assert_eq!(before, 0);
        prop_assert_eq!(
            estimator.admitted_bytes(),
            next_admitted
                .iter()
                .map(|handoff| handoff.bytes)
                .fold(0_u64, u64::saturating_add)
        );
    }

    #[test]
    fn proptest_disk_budget_partition_returns_admitted_prefix_and_deferred_suffix(
        handoffs in disk_handoffs(),
        throughput in 1_u64..=1_000_000,
        disk_budget_us in 0_u64..=1_000,
    ) {
        let mut estimator = DiskBudgetEstimator::with_throughput(throughput);
        let (admitted, deferred) = estimator.partition(&handoffs, disk_budget_us);
        let split = admitted.len();

        prop_assert_eq!(admitted.as_slice(), &handoffs[..split]);
        prop_assert_eq!(deferred.as_slice(), &handoffs[split..]);
    }
}
