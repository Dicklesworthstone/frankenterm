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
    prop::collection::vec((any::<u64>(), any::<u64>(), any::<u64>()), 0..48).prop_map(|items| {
        items
            .into_iter()
            .map(|(region_id, bytes, frame_id)| staging_event(region_id, bytes, frame_id))
            .collect()
    })
}

fn disk_handoffs() -> impl Strategy<Value = Vec<DiskTierHandoff>> {
    prop::collection::vec((any::<u64>(), any::<u64>(), any::<u64>()), 0..48).prop_map(|items| {
        items
            .into_iter()
            .map(|(region_id, bytes, frame_id)| disk_handoff(region_id, bytes, frame_id))
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn frame_budget_partition_never_admits_after_first_deferral(
        events in staging_events(),
        throughput in 0_u64..=1_000_000,
        frame_budget_us in any::<u64>(),
    ) {
        let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(throughput);
        let (admitted, deferred) = deferrer.partition(&events, frame_budget_us);
        let split = admitted.len();

        prop_assert_eq!(admitted.as_slice(), &events[..split]);
        prop_assert_eq!(deferred.as_slice(), &events[split..]);
        prop_assert_eq!(admitted.len() + deferred.len(), events.len());
    }

    #[test]
    fn disk_budget_partition_never_admits_after_first_deferral(
        handoffs in disk_handoffs(),
        throughput in 0_u64..=1_000_000,
        disk_budget_us in any::<u64>(),
    ) {
        let mut estimator = DiskBudgetEstimator::with_throughput(throughput);
        let (admitted, deferred) = estimator.partition(&handoffs, disk_budget_us);
        let split = admitted.len();

        prop_assert_eq!(admitted.as_slice(), &handoffs[..split]);
        prop_assert_eq!(deferred.as_slice(), &handoffs[split..]);
        prop_assert_eq!(admitted.len() + deferred.len(), handoffs.len());
    }

    #[test]
    fn concrete_frame_budget_overflow_shape_defers_the_suffix(
        second_bytes in 0_u64..=3_000,
        extra_tail in staging_events(),
    ) {
        let mut pending = vec![staging_event(1, 4_000, 9), staging_event(2, second_bytes, 9)];
        pending.extend(extra_tail);
        let mut deferrer = FrameBudgetSwapDeferrer::with_throughput(1_000);
        let (admitted, deferred) = deferrer.partition(&pending, 3);

        prop_assert!(admitted.is_empty());
        prop_assert_eq!(deferred, pending);
    }

    #[test]
    fn concrete_disk_budget_overflow_shape_defers_the_suffix(
        second_bytes in 0_u64..=3_000,
        extra_tail in disk_handoffs(),
    ) {
        let mut pending = vec![disk_handoff(1, 4_000, 9), disk_handoff(2, second_bytes, 9)];
        pending.extend(extra_tail);
        let mut estimator = DiskBudgetEstimator::with_throughput(1_000);
        let (admitted, deferred) = estimator.partition(&pending, 3);

        prop_assert!(admitted.is_empty());
        prop_assert_eq!(deferred, pending);
    }
}
