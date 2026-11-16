use frankenterm_core::atlas_staging_blit_plan::{
    AtlasBlitCommand, AtlasBlitDirection, AtlasBlitPlan,
};
use frankenterm_core::atlas_tiered_swap::{
    HostRamStagingAllocation, StagingTransferDirection, StagingTransferEvent,
};
use proptest::prelude::*;

fn transfer_direction() -> impl Strategy<Value = StagingTransferDirection> {
    prop_oneof![
        Just(StagingTransferDirection::Demote),
        Just(StagingTransferDirection::Promote),
    ]
}

fn staging_event() -> impl Strategy<Value = StagingTransferEvent> {
    (
        any::<u64>(),
        transfer_direction(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(region_id, direction, offset, bytes, frame_id)| StagingTransferEvent {
                region_id,
                direction,
                allocation: HostRamStagingAllocation {
                    region_id,
                    offset,
                    bytes,
                },
                frame_id,
            },
        )
}

fn staging_events() -> impl Strategy<Value = Vec<StagingTransferEvent>> {
    prop::collection::vec(staging_event(), 0..32)
}

fn expected_direction(direction: StagingTransferDirection) -> AtlasBlitDirection {
    match direction {
        StagingTransferDirection::Demote => AtlasBlitDirection::VramToStaging,
        StagingTransferDirection::Promote => AtlasBlitDirection::StagingToVram,
    }
}

fn expected_download_bytes(events: &[StagingTransferEvent]) -> u64 {
    events.iter().fold(0_u64, |acc, event| {
        if event.direction == StagingTransferDirection::Demote {
            acc.saturating_add(event.bytes())
        } else {
            acc
        }
    })
}

fn expected_upload_bytes(events: &[StagingTransferEvent]) -> u64 {
    events.iter().fold(0_u64, |acc, event| {
        if event.direction == StagingTransferDirection::Promote {
            acc.saturating_add(event.bytes())
        } else {
            acc
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_atlas_staging_blit_plan_command_from_event_preserves_fields(
        event in staging_event(),
    ) {
        let command = AtlasBlitCommand::from_event(event);

        prop_assert_eq!(command.region_id, event.region_id);
        prop_assert_eq!(command.direction, expected_direction(event.direction));
        prop_assert_eq!(command.staging_offset, event.offset());
        prop_assert_eq!(command.byte_len, event.bytes());
        prop_assert_eq!(command.frame_id, event.frame_id);
    }

    #[test]
    fn proptest_atlas_staging_blit_plan_preserves_event_order(events in staging_events()) {
        let plan = AtlasBlitPlan::from_events(&events);

        prop_assert_eq!(plan.commands().len(), events.len());
        prop_assert_eq!(plan.is_empty(), events.is_empty());

        for (command, event) in plan.commands().iter().zip(events.iter()) {
            prop_assert_eq!(
                *command,
                AtlasBlitCommand {
                    region_id: event.region_id,
                    direction: expected_direction(event.direction),
                    staging_offset: event.offset(),
                    byte_len: event.bytes(),
                    frame_id: event.frame_id,
                }
            );
        }
    }

    #[test]
    fn proptest_atlas_staging_blit_plan_accounts_bytes_by_direction(events in staging_events()) {
        let plan = AtlasBlitPlan::from_events(&events);
        let expected_download = expected_download_bytes(&events);
        let expected_upload = expected_upload_bytes(&events);

        prop_assert_eq!(plan.download_bytes(), expected_download);
        prop_assert_eq!(plan.upload_bytes(), expected_upload);
        prop_assert_eq!(
            plan.total_bytes(),
            expected_download.saturating_add(expected_upload)
        );
    }

    #[test]
    fn proptest_atlas_staging_blit_plan_zero_byte_events_do_not_change_totals(
        mut events in staging_events(),
        region_id in any::<u64>(),
        direction in transfer_direction(),
        offset in any::<u64>(),
        frame_id in any::<u64>(),
    ) {
        let before = AtlasBlitPlan::from_events(&events);
        events.push(StagingTransferEvent {
            region_id,
            direction,
            allocation: HostRamStagingAllocation {
                region_id,
                offset,
                bytes: 0,
            },
            frame_id,
        });
        let after = AtlasBlitPlan::from_events(&events);

        prop_assert_eq!(after.commands().len(), before.commands().len() + 1);
        prop_assert_eq!(after.download_bytes(), before.download_bytes());
        prop_assert_eq!(after.upload_bytes(), before.upload_bytes());
        prop_assert_eq!(after.total_bytes(), before.total_bytes());
    }

    #[test]
    fn proptest_atlas_staging_blit_plan_total_saturates_at_u64_max(
        demote_bytes in 1_u64..=u64::MAX,
        promote_bytes in 1_u64..=u64::MAX,
    ) {
        let events = [
            StagingTransferEvent {
                region_id: 1,
                direction: StagingTransferDirection::Demote,
                allocation: HostRamStagingAllocation {
                    region_id: 1,
                    offset: 0,
                    bytes: demote_bytes,
                },
                frame_id: 1,
            },
            StagingTransferEvent {
                region_id: 2,
                direction: StagingTransferDirection::Promote,
                allocation: HostRamStagingAllocation {
                    region_id: 2,
                    offset: demote_bytes,
                    bytes: promote_bytes,
                },
                frame_id: 1,
            },
        ];

        let plan = AtlasBlitPlan::from_events(&events);

        prop_assert_eq!(plan.download_bytes(), demote_bytes);
        prop_assert_eq!(plan.upload_bytes(), promote_bytes);
        prop_assert_eq!(plan.total_bytes(), demote_bytes.saturating_add(promote_bytes));
    }
}
