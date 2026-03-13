//! Property tests for robot_ntm_surface module (ft-3681t.4.1).
//!
//! Covers serde roundtrips for all 5 NTM command families
//! (Checkpoint, Context, Work, Fleet, Profile), their request/response
//! types, NTM equivalence metadata, and the API surface enum.

use frankenterm_core::robot_ntm_surface::*;
use proptest::prelude::*;
use std::collections::HashMap;

// =============================================================================
// Strategies
// =============================================================================

fn arb_convergence_classification() -> impl Strategy<Value = ConvergenceClassification> {
    prop_oneof![
        Just(ConvergenceClassification::DirectReplacement),
        Just(ConvergenceClassification::Upgrade),
        Just(ConvergenceClassification::Novel),
        Just(ConvergenceClassification::Partial),
    ]
}

fn arb_rotation_strategy() -> impl Strategy<Value = RotationStrategy> {
    prop_oneof![
        Just(RotationStrategy::AgentDefault),
        Just(RotationStrategy::Aggressive),
        Just(RotationStrategy::Gentle),
    ]
}

fn arb_rebalance_strategy() -> impl Strategy<Value = RebalanceStrategy> {
    prop_oneof![
        Just(RebalanceStrategy::LoadBased),
        Just(RebalanceStrategy::CapabilityBased),
        Just(RebalanceStrategy::RoundRobin),
    ]
}

fn arb_ntm_api_surface() -> impl Strategy<Value = NtmApiSurface> {
    prop_oneof![
        Just(NtmApiSurface::CheckpointSave),
        Just(NtmApiSurface::CheckpointList),
        Just(NtmApiSurface::CheckpointShow),
        Just(NtmApiSurface::CheckpointDelete),
        Just(NtmApiSurface::CheckpointRollback),
        Just(NtmApiSurface::ContextStatus),
        Just(NtmApiSurface::ContextRotate),
        Just(NtmApiSurface::ContextHistory),
        Just(NtmApiSurface::WorkClaim),
        Just(NtmApiSurface::WorkRelease),
        Just(NtmApiSurface::WorkComplete),
        Just(NtmApiSurface::WorkList),
        Just(NtmApiSurface::WorkReady),
        Just(NtmApiSurface::WorkAssign),
        Just(NtmApiSurface::FleetStatus),
        Just(NtmApiSurface::FleetScale),
        Just(NtmApiSurface::FleetRebalance),
        Just(NtmApiSurface::FleetAgents),
        Just(NtmApiSurface::ProfileList),
        Just(NtmApiSurface::ProfileShow),
        Just(NtmApiSurface::ProfileApply),
        Just(NtmApiSurface::ProfileValidate),
    ]
}

fn arb_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,30}"
}

fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![Just(None), arb_string().prop_map(Some)]
}

fn arb_string_vec() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_string(), 0..5)
}

fn arb_pane_ids() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0..1000u64, 0..5)
}

fn arb_string_map() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map(arb_string(), arb_string(), 0..3)
}

// =============================================================================
// NTM Equivalence
// =============================================================================

fn arb_ntm_equivalence() -> impl Strategy<Value = NtmEquivalence> {
    (arb_string_vec(), arb_string(), arb_convergence_classification()).prop_map(
        |(cmds, domain, classification)| NtmEquivalence {
            ntm_commands: cmds,
            census_domain: domain,
            classification,
        },
    )
}

proptest! {
    #[test]
    fn ntm_equivalence_serde_roundtrip(val in arb_ntm_equivalence()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: NtmEquivalence = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn convergence_classification_serde_roundtrip(val in arb_convergence_classification()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ConvergenceClassification = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, rt);
    }
}

// =============================================================================
// Checkpoint family
// =============================================================================

fn arb_checkpoint_save_request() -> impl Strategy<Value = CheckpointSaveRequest> {
    (arb_opt_string(), any::<bool>(), arb_pane_ids()).prop_map(|(label, scrollback, pane_ids)| {
        CheckpointSaveRequest {
            label,
            include_scrollback: scrollback,
            pane_ids,
        }
    })
}

fn arb_checkpoint_list_request() -> impl Strategy<Value = CheckpointListRequest> {
    (1..200usize, 0..100usize).prop_map(|(limit, offset)| CheckpointListRequest { limit, offset })
}

fn arb_checkpoint_show_request() -> impl Strategy<Value = CheckpointShowRequest> {
    arb_string().prop_map(|id| CheckpointShowRequest { checkpoint_id: id })
}

fn arb_checkpoint_delete_request() -> impl Strategy<Value = CheckpointDeleteRequest> {
    arb_string().prop_map(|id| CheckpointDeleteRequest { checkpoint_id: id })
}

fn arb_checkpoint_rollback_request() -> impl Strategy<Value = CheckpointRollbackRequest> {
    (arb_string(), any::<bool>()).prop_map(|(id, dry_run)| CheckpointRollbackRequest {
        checkpoint_id: id,
        dry_run,
    })
}

fn arb_checkpoint_command() -> impl Strategy<Value = CheckpointCommand> {
    prop_oneof![
        arb_checkpoint_save_request().prop_map(CheckpointCommand::Save),
        arb_checkpoint_list_request().prop_map(CheckpointCommand::List),
        arb_checkpoint_show_request().prop_map(CheckpointCommand::Show),
        arb_checkpoint_delete_request().prop_map(CheckpointCommand::Delete),
        arb_checkpoint_rollback_request().prop_map(CheckpointCommand::Rollback),
    ]
}

fn arb_checkpoint_save_data() -> impl Strategy<Value = CheckpointSaveData> {
    (
        arb_string(),
        arb_opt_string(),
        0..100usize,
        0..100000u64,
        any::<bool>(),
        0..u64::MAX,
    )
        .prop_map(
            |(id, label, pane_count, bytes, scrollback, ts)| CheckpointSaveData {
                checkpoint_id: id,
                label,
                pane_count,
                bytes_persisted: bytes,
                scrollback_included: scrollback,
                created_at: ts,
            },
        )
}

fn arb_checkpoint_summary() -> impl Strategy<Value = CheckpointSummary> {
    (arb_string(), arb_opt_string(), 0..100usize, 0..100000u64, 0..u64::MAX).prop_map(
        |(id, label, pane_count, size, ts)| CheckpointSummary {
            checkpoint_id: id,
            label,
            pane_count,
            size_bytes: size,
            created_at: ts,
        },
    )
}

fn arb_checkpoint_list_data() -> impl Strategy<Value = CheckpointListData> {
    (
        prop::collection::vec(arb_checkpoint_summary(), 0..5),
        0..200usize,
    )
        .prop_map(|(checkpoints, total)| CheckpointListData { checkpoints, total })
}

fn arb_checkpoint_pane_snapshot() -> impl Strategy<Value = CheckpointPaneSnapshot> {
    (
        0..1000u64,
        arb_string(),
        arb_opt_string(),
        any::<bool>(),
        0..10000usize,
    )
        .prop_map(|(pane_id, title, wd, has_sb, sb_lines)| CheckpointPaneSnapshot {
            pane_id,
            title,
            working_dir: wd,
            has_scrollback: has_sb,
            scrollback_lines: sb_lines,
        })
}

fn arb_checkpoint_show_data() -> impl Strategy<Value = CheckpointShowData> {
    (
        arb_string(),
        arb_opt_string(),
        prop::collection::vec(arb_checkpoint_pane_snapshot(), 0..3),
        0..100000u64,
        arb_string(),
        0..u64::MAX,
    )
        .prop_map(|(id, label, panes, size, hash, ts)| CheckpointShowData {
            checkpoint_id: id,
            label,
            panes,
            size_bytes: size,
            content_hash: hash,
            created_at: ts,
        })
}

fn arb_checkpoint_delete_data() -> impl Strategy<Value = CheckpointDeleteData> {
    (arb_string(), 0..100000u64).prop_map(|(id, bytes)| CheckpointDeleteData {
        checkpoint_id: id,
        bytes_freed: bytes,
    })
}

fn arb_checkpoint_rollback_data() -> impl Strategy<Value = CheckpointRollbackData> {
    (
        arb_string(),
        0..100usize,
        0..50usize,
        any::<bool>(),
        arb_string_vec(),
    )
        .prop_map(
            |(id, restored, skipped, dry_run, warnings)| CheckpointRollbackData {
                checkpoint_id: id,
                panes_restored: restored,
                panes_skipped: skipped,
                dry_run,
                warnings,
            },
        )
}

proptest! {
    #[test]
    fn checkpoint_command_serde_roundtrip(val in arb_checkpoint_command()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn checkpoint_save_data_serde_roundtrip(val in arb_checkpoint_save_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointSaveData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn checkpoint_list_data_serde_roundtrip(val in arb_checkpoint_list_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn checkpoint_show_data_serde_roundtrip(val in arb_checkpoint_show_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointShowData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn checkpoint_pane_snapshot_serde_roundtrip(val in arb_checkpoint_pane_snapshot()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointPaneSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn checkpoint_delete_data_serde_roundtrip(val in arb_checkpoint_delete_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointDeleteData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn checkpoint_rollback_data_serde_roundtrip(val in arb_checkpoint_rollback_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CheckpointRollbackData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }
}

// =============================================================================
// Context family
// =============================================================================

fn arb_context_status_request() -> impl Strategy<Value = ContextStatusRequest> {
    prop_oneof![
        Just(ContextStatusRequest { pane_id: None }),
        (0..1000u64).prop_map(|id| ContextStatusRequest {
            pane_id: Some(id)
        }),
    ]
}

fn arb_context_rotate_request() -> impl Strategy<Value = ContextRotateRequest> {
    (0..1000u64, arb_rotation_strategy())
        .prop_map(|(pane_id, strategy)| ContextRotateRequest { pane_id, strategy })
}

fn arb_context_history_request() -> impl Strategy<Value = ContextHistoryRequest> {
    (0..1000u64, 1..200usize).prop_map(|(pane_id, limit)| ContextHistoryRequest { pane_id, limit })
}

fn arb_context_command() -> impl Strategy<Value = ContextCommand> {
    prop_oneof![
        arb_context_status_request().prop_map(ContextCommand::Status),
        arb_context_rotate_request().prop_map(ContextCommand::Rotate),
        arb_context_history_request().prop_map(ContextCommand::History),
    ]
}

fn arb_fleet_context_pressure() -> impl Strategy<Value = FleetContextPressure> {
    (0..200usize, 0..200usize, 0..50usize, 0..20usize, 0..10usize).prop_map(
        |(total, green, yellow, red, black)| FleetContextPressure {
            total_panes: total,
            green_count: green,
            yellow_count: yellow,
            red_count: red,
            black_count: black,
        },
    )
}

fn arb_pane_context_status() -> impl Strategy<Value = PaneContextStatus> {
    (
        0..1000u64,
        arb_string(),
        0..1000000u64,
        0..2000000u64,
        0..100u32,
        prop_oneof![Just(None), (0..100000u64).prop_map(Some)],
    )
        .prop_map(
            |(pane_id, tier, consumed, budget, count, ms)| PaneContextStatus {
                pane_id,
                pressure_tier: tier,
                utilization: if budget > 0 {
                    consumed as f64 / budget as f64
                } else {
                    0.0
                },
                tokens_consumed: consumed,
                token_budget: budget,
                compaction_count: count,
                ms_since_last_compaction: ms,
            },
        )
}

fn arb_context_status_data() -> impl Strategy<Value = ContextStatusData> {
    (
        prop::collection::vec(arb_pane_context_status(), 0..3),
        arb_fleet_context_pressure(),
    )
        .prop_map(|(panes, fleet_pressure)| ContextStatusData {
            panes,
            fleet_pressure,
        })
}

fn arb_context_rotate_data() -> impl Strategy<Value = ContextRotateData> {
    (
        0..1000u64,
        any::<bool>(),
        arb_opt_string(),
        arb_rotation_strategy(),
    )
        .prop_map(|(pane_id, accepted, reason, strategy)| ContextRotateData {
            pane_id,
            accepted,
            reason,
            strategy,
        })
}

fn arb_compaction_event() -> impl Strategy<Value = CompactionEvent> {
    (
        0..u64::MAX,
        0..1000000u64,
        0..1000000u64,
        0..500000u64,
        arb_string(),
    )
        .prop_map(|(ts, before_tok, after_tok, freed, trigger)| CompactionEvent {
            timestamp_ms: ts,
            utilization_before: before_tok as f64 / 1_000_000.0,
            utilization_after: after_tok as f64 / 1_000_000.0,
            tokens_freed: freed,
            trigger,
        })
}

fn arb_context_history_data() -> impl Strategy<Value = ContextHistoryData> {
    (
        0..1000u64,
        prop::collection::vec(arb_compaction_event(), 0..5),
    )
        .prop_map(|(pane_id, events)| ContextHistoryData { pane_id, events })
}

proptest! {
    #[test]
    fn context_command_serde_roundtrip(val in arb_context_command()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ContextCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn rotation_strategy_serde_roundtrip(val in arb_rotation_strategy()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: RotationStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, rt);
    }

    #[test]
    fn context_status_data_serde_roundtrip(val in arb_context_status_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ContextStatusData = serde_json::from_str(&json).unwrap();
        // f64 utilization field loses precision through JSON roundtrip
        prop_assert_eq!(val.panes.len(), rt.panes.len());
        for (a, b) in val.panes.iter().zip(rt.panes.iter()) {
            prop_assert_eq!(a.pane_id, b.pane_id);
            prop_assert_eq!(&a.pressure_tier, &b.pressure_tier);
            prop_assert!((a.utilization - b.utilization).abs() < 1e-10);
            prop_assert_eq!(a.tokens_consumed, b.tokens_consumed);
            prop_assert_eq!(a.token_budget, b.token_budget);
            prop_assert_eq!(a.compaction_count, b.compaction_count);
            prop_assert_eq!(a.ms_since_last_compaction, b.ms_since_last_compaction);
        }
        prop_assert_eq!(&val.fleet_pressure, &rt.fleet_pressure);
    }

    #[test]
    fn context_rotate_data_serde_roundtrip(val in arb_context_rotate_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ContextRotateData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn compaction_event_serde_roundtrip(val in arb_compaction_event()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: CompactionEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn context_history_data_serde_roundtrip(val in arb_context_history_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ContextHistoryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn fleet_context_pressure_serde_roundtrip(val in arb_fleet_context_pressure()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: FleetContextPressure = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn pane_context_status_serde_roundtrip(val in arb_pane_context_status()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: PaneContextStatus = serde_json::from_str(&json).unwrap();
        // f64 comparison via JSON roundtrip
        let check = val.utilization == rt.utilization
            || (val.utilization - rt.utilization).abs() < 1e-10;
        prop_assert!(check, "utilization mismatch: {} vs {}", val.utilization, rt.utilization);
    }
}

// =============================================================================
// Work family
// =============================================================================

fn arb_work_claim_request() -> impl Strategy<Value = WorkClaimRequest> {
    (arb_string(), arb_string()).prop_map(|(item_id, agent_id)| WorkClaimRequest {
        item_id,
        agent_id,
    })
}

fn arb_work_release_request() -> impl Strategy<Value = WorkReleaseRequest> {
    (arb_string(), arb_opt_string()).prop_map(|(item_id, reason)| WorkReleaseRequest {
        item_id,
        reason,
    })
}

fn arb_work_complete_request() -> impl Strategy<Value = WorkCompleteRequest> {
    (arb_string(), arb_opt_string(), arb_string_vec()).prop_map(
        |(item_id, summary, evidence)| WorkCompleteRequest {
            item_id,
            summary,
            evidence,
        },
    )
}

fn arb_work_list_request() -> impl Strategy<Value = WorkListRequest> {
    (
        arb_opt_string(),
        arb_opt_string(),
        arb_opt_string(),
        1..200usize,
    )
        .prop_map(
            |(status_filter, agent_filter, label_filter, limit)| WorkListRequest {
                status_filter,
                agent_filter,
                label_filter,
                limit,
            },
        )
}

fn arb_work_ready_request() -> impl Strategy<Value = WorkReadyRequest> {
    (arb_opt_string(), 1..200usize).prop_map(|(agent_id, limit)| WorkReadyRequest {
        agent_id,
        limit,
    })
}

fn arb_work_assign_request() -> impl Strategy<Value = WorkAssignRequest> {
    (arb_string(), arb_string(), arb_opt_string()).prop_map(
        |(item_id, agent_id, strategy)| WorkAssignRequest {
            item_id,
            agent_id,
            strategy,
        },
    )
}

fn arb_work_command() -> impl Strategy<Value = WorkCommand> {
    prop_oneof![
        arb_work_claim_request().prop_map(WorkCommand::Claim),
        arb_work_release_request().prop_map(WorkCommand::Release),
        arb_work_complete_request().prop_map(WorkCommand::Complete),
        arb_work_list_request().prop_map(WorkCommand::List),
        arb_work_ready_request().prop_map(WorkCommand::Ready),
        arb_work_assign_request().prop_map(WorkCommand::Assign),
    ]
}

fn arb_work_claim_data() -> impl Strategy<Value = WorkClaimData> {
    (arb_string(), arb_string(), arb_string(), 0..10u32, 0..u64::MAX).prop_map(
        |(item_id, agent_id, title, priority, claimed_at)| WorkClaimData {
            item_id,
            agent_id,
            title,
            priority,
            claimed_at,
        },
    )
}

fn arb_work_release_data() -> impl Strategy<Value = WorkReleaseData> {
    (arb_string(), arb_string()).prop_map(|(item_id, new_status)| WorkReleaseData {
        item_id,
        new_status,
    })
}

fn arb_work_complete_data() -> impl Strategy<Value = WorkCompleteData> {
    (arb_string(), arb_string_vec()).prop_map(|(item_id, unblocked)| WorkCompleteData {
        item_id,
        unblocked,
    })
}

fn arb_work_item_summary() -> impl Strategy<Value = WorkItemSummary> {
    (
        arb_string(),
        arb_string(),
        0..10u32,
        arb_string(),
        arb_opt_string(),
        arb_string_vec(),
        0..20usize,
        0..20usize,
    )
        .prop_map(
            |(id, title, priority, status, assigned, labels, blocked, unblocks)| WorkItemSummary {
                id,
                title,
                priority,
                status,
                assigned_to: assigned,
                labels,
                blocked_by_count: blocked,
                unblocks_count: unblocks,
            },
        )
}

fn arb_work_list_data() -> impl Strategy<Value = WorkListData> {
    (
        prop::collection::vec(arb_work_item_summary(), 0..5),
        0..200usize,
    )
        .prop_map(|(items, total)| WorkListData { items, total })
}

fn arb_work_ready_data() -> impl Strategy<Value = WorkReadyData> {
    (
        prop::collection::vec(arb_work_item_summary(), 0..5),
        0..100usize,
        0..100usize,
    )
        .prop_map(|(items, total_ready, total_blocked)| WorkReadyData {
            items,
            total_ready,
            total_blocked,
        })
}

fn arb_work_assign_data() -> impl Strategy<Value = WorkAssignData> {
    (arb_string(), arb_string(), arb_string()).prop_map(
        |(item_id, agent_id, strategy_used)| WorkAssignData {
            item_id,
            agent_id,
            strategy_used,
        },
    )
}

proptest! {
    #[test]
    fn work_command_serde_roundtrip(val in arb_work_command()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_claim_data_serde_roundtrip(val in arb_work_claim_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkClaimData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_release_data_serde_roundtrip(val in arb_work_release_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkReleaseData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_complete_data_serde_roundtrip(val in arb_work_complete_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkCompleteData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_item_summary_serde_roundtrip(val in arb_work_item_summary()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkItemSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_list_data_serde_roundtrip(val in arb_work_list_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_ready_data_serde_roundtrip(val in arb_work_ready_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkReadyData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_assign_data_serde_roundtrip(val in arb_work_assign_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkAssignData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }
}

// =============================================================================
// Fleet family
// =============================================================================

fn arb_fleet_status_request() -> impl Strategy<Value = FleetStatusRequest> {
    any::<bool>().prop_map(|detailed| FleetStatusRequest { detailed })
}

fn arb_fleet_scale_request() -> impl Strategy<Value = FleetScaleRequest> {
    (arb_string(), 0..100u32, any::<bool>()).prop_map(|(program, target_count, dry_run)| {
        FleetScaleRequest {
            program,
            target_count,
            dry_run,
        }
    })
}

fn arb_fleet_rebalance_request() -> impl Strategy<Value = FleetRebalanceRequest> {
    (arb_rebalance_strategy(), any::<bool>()).prop_map(|(strategy, dry_run)| {
        FleetRebalanceRequest { strategy, dry_run }
    })
}

fn arb_fleet_agents_request() -> impl Strategy<Value = FleetAgentsRequest> {
    (arb_opt_string(), arb_opt_string()).prop_map(|(program_filter, state_filter)| {
        FleetAgentsRequest {
            program_filter,
            state_filter,
        }
    })
}

fn arb_fleet_command() -> impl Strategy<Value = FleetCommand> {
    prop_oneof![
        arb_fleet_status_request().prop_map(FleetCommand::Status),
        arb_fleet_scale_request().prop_map(FleetCommand::Scale),
        arb_fleet_rebalance_request().prop_map(FleetCommand::Rebalance),
        arb_fleet_agents_request().prop_map(FleetCommand::Agents),
    ]
}

fn arb_program_slot_summary() -> impl Strategy<Value = ProgramSlotSummary> {
    (0..50usize, 0..50usize, 0..50usize).prop_map(|(count, active, idle)| ProgramSlotSummary {
        count,
        active,
        idle,
    })
}

fn arb_work_queue_summary() -> impl Strategy<Value = WorkQueueSummary> {
    (
        0..200usize,
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
    )
        .prop_map(
            |(total, ready, blocked, in_progress, completed)| WorkQueueSummary {
                total_items: total,
                ready,
                blocked,
                in_progress,
                completed,
            },
        )
}

fn arb_fleet_status_data() -> impl Strategy<Value = FleetStatusData> {
    (
        0..200usize,
        0..100usize,
        0..100usize,
        0..50usize,
        prop::collection::hash_map(arb_string(), arb_program_slot_summary(), 0..3),
        arb_work_queue_summary(),
    )
        .prop_map(
            |(total, active, idle, stalled, by_program, work_queue)| FleetStatusData {
                total_agents: total,
                active_agents: active,
                idle_agents: idle,
                stalled_agents: stalled,
                by_program,
                work_queue,
            },
        )
}

fn arb_fleet_scale_data() -> impl Strategy<Value = FleetScaleData> {
    (
        arb_string(),
        0..100u32,
        0..100u32,
        any::<bool>(),
        arb_pane_ids(),
        arb_pane_ids(),
    )
        .prop_map(
            |(program, prev, new, dry_run, spawned, terminated)| FleetScaleData {
                program,
                previous_count: prev,
                new_count: new,
                dry_run,
                spawned_pane_ids: spawned,
                terminated_pane_ids: terminated,
            },
        )
}

fn arb_rebalance_action() -> impl Strategy<Value = RebalanceAction> {
    (arb_string(), arb_opt_string(), arb_string(), arb_string()).prop_map(
        |(item_id, from, to, reason)| RebalanceAction {
            item_id,
            from_agent: from,
            to_agent: to,
            reason,
        },
    )
}

fn arb_fleet_rebalance_data() -> impl Strategy<Value = FleetRebalanceData> {
    (
        arb_rebalance_strategy(),
        0..50usize,
        any::<bool>(),
        prop::collection::vec(arb_rebalance_action(), 0..3),
    )
        .prop_map(
            |(strategy, reassigned, dry_run, reassignments)| FleetRebalanceData {
                strategy,
                items_reassigned: reassigned,
                dry_run,
                reassignments,
            },
        )
}

fn arb_agent_slot_info() -> impl Strategy<Value = AgentSlotInfo> {
    (
        arb_string(),
        0..1000u64,
        arb_string(),
        arb_string(),
        arb_opt_string(),
        0..100000u64,
        arb_string_map(),
    )
        .prop_map(
            |(slot_id, pane_id, program, state, work, uptime, metadata)| AgentSlotInfo {
                slot_id,
                pane_id,
                program,
                state,
                assigned_work: work,
                uptime_secs: uptime,
                metadata,
            },
        )
}

fn arb_fleet_agents_data() -> impl Strategy<Value = FleetAgentsData> {
    prop::collection::vec(arb_agent_slot_info(), 0..5)
        .prop_map(|agents| FleetAgentsData { agents })
}

proptest! {
    #[test]
    fn fleet_command_serde_roundtrip(val in arb_fleet_command()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: FleetCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn rebalance_strategy_serde_roundtrip(val in arb_rebalance_strategy()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: RebalanceStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, rt);
    }

    #[test]
    fn fleet_status_data_serde_roundtrip(val in arb_fleet_status_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: FleetStatusData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn fleet_scale_data_serde_roundtrip(val in arb_fleet_scale_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: FleetScaleData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn fleet_rebalance_data_serde_roundtrip(val in arb_fleet_rebalance_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: FleetRebalanceData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn rebalance_action_serde_roundtrip(val in arb_rebalance_action()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: RebalanceAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn agent_slot_info_serde_roundtrip(val in arb_agent_slot_info()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: AgentSlotInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn fleet_agents_data_serde_roundtrip(val in arb_fleet_agents_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: FleetAgentsData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn program_slot_summary_serde_roundtrip(val in arb_program_slot_summary()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProgramSlotSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn work_queue_summary_serde_roundtrip(val in arb_work_queue_summary()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: WorkQueueSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }
}

// =============================================================================
// Profile family
// =============================================================================

fn arb_profile_list_request() -> impl Strategy<Value = ProfileListRequest> {
    (arb_opt_string(), arb_opt_string()).prop_map(|(role_filter, tag_filter)| ProfileListRequest {
        role_filter,
        tag_filter,
    })
}

fn arb_profile_show_request() -> impl Strategy<Value = ProfileShowRequest> {
    arb_string().prop_map(|name| ProfileShowRequest { name })
}

fn arb_profile_apply_request() -> impl Strategy<Value = ProfileApplyRequest> {
    (arb_string(), 1..20u32, arb_string_map(), any::<bool>()).prop_map(
        |(name, count, env_overrides, dry_run)| ProfileApplyRequest {
            name,
            count,
            env_overrides,
            dry_run,
        },
    )
}

fn arb_profile_validate_request() -> impl Strategy<Value = ProfileValidateRequest> {
    arb_string().prop_map(|name| ProfileValidateRequest { name })
}

fn arb_profile_command() -> impl Strategy<Value = ProfileCommand> {
    prop_oneof![
        arb_profile_list_request().prop_map(ProfileCommand::List),
        arb_profile_show_request().prop_map(ProfileCommand::Show),
        arb_profile_apply_request().prop_map(ProfileCommand::Apply),
        arb_profile_validate_request().prop_map(ProfileCommand::Validate),
    ]
}

fn arb_profile_summary() -> impl Strategy<Value = ProfileSummary> {
    (arb_string(), arb_opt_string(), arb_string(), arb_string_vec()).prop_map(
        |(name, description, role, tags)| ProfileSummary {
            name,
            description,
            role,
            tags,
        },
    )
}

fn arb_profile_list_data() -> impl Strategy<Value = ProfileListData> {
    prop::collection::vec(arb_profile_summary(), 0..5)
        .prop_map(|profiles| ProfileListData { profiles })
}

fn arb_profile_show_data() -> impl Strategy<Value = ProfileShowData> {
    (
        arb_string(),
        arb_opt_string(),
        arb_string(),
        arb_opt_string(),
        arb_string_map(),
        arb_opt_string(),
        arb_opt_string(),
        arb_string_vec(),
        arb_string_vec(),
    )
        .prop_map(
            |(name, desc, role, spawn, env, wd, layout, bootstrap, tags)| ProfileShowData {
                name,
                description: desc,
                role,
                spawn_command: spawn,
                environment: env,
                working_directory: wd,
                layout_template: layout,
                bootstrap_commands: bootstrap,
                tags,
            },
        )
}

fn arb_profile_apply_data() -> impl Strategy<Value = ProfileApplyData> {
    (arb_string(), arb_pane_ids(), any::<bool>()).prop_map(
        |(profile_name, panes_spawned, dry_run)| ProfileApplyData {
            profile_name,
            panes_spawned,
            dry_run,
        },
    )
}

fn arb_profile_validate_data() -> impl Strategy<Value = ProfileValidateData> {
    (arb_string(), any::<bool>(), arb_string_vec()).prop_map(|(name, valid, issues)| {
        ProfileValidateData {
            name,
            valid,
            issues,
        }
    })
}

proptest! {
    #[test]
    fn profile_command_serde_roundtrip(val in arb_profile_command()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProfileCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn profile_summary_serde_roundtrip(val in arb_profile_summary()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProfileSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn profile_list_data_serde_roundtrip(val in arb_profile_list_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProfileListData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn profile_show_data_serde_roundtrip(val in arb_profile_show_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProfileShowData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn profile_apply_data_serde_roundtrip(val in arb_profile_apply_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProfileApplyData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn profile_validate_data_serde_roundtrip(val in arb_profile_validate_data()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: ProfileValidateData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }
}

// =============================================================================
// Top-level RobotNtmCommand
// =============================================================================

fn arb_robot_ntm_command() -> impl Strategy<Value = RobotNtmCommand> {
    prop_oneof![
        arb_checkpoint_command().prop_map(RobotNtmCommand::Checkpoint),
        arb_context_command().prop_map(RobotNtmCommand::Context),
        arb_work_command().prop_map(RobotNtmCommand::Work),
        arb_fleet_command().prop_map(RobotNtmCommand::Fleet),
        arb_profile_command().prop_map(RobotNtmCommand::Profile),
    ]
}

proptest! {
    #[test]
    fn robot_ntm_command_serde_roundtrip(val in arb_robot_ntm_command()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: RobotNtmCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &rt);
    }

    #[test]
    fn robot_ntm_command_family_name_nonempty(val in arb_robot_ntm_command()) {
        prop_assert!(!val.family_name().is_empty());
    }

    #[test]
    fn robot_ntm_command_action_name_nonempty(val in arb_robot_ntm_command()) {
        prop_assert!(!val.action_name().is_empty());
    }

    #[test]
    fn robot_ntm_command_ntm_equivalence_valid(val in arb_robot_ntm_command()) {
        let eq = val.ntm_equivalence();
        prop_assert!(!eq.census_domain.is_empty());
        // classification label is always a valid static string
        let label = eq.classification.label();
        prop_assert!(!label.is_empty());
    }
}

// =============================================================================
// NtmApiSurface
// =============================================================================

proptest! {
    #[test]
    fn ntm_api_surface_serde_roundtrip(val in arb_ntm_api_surface()) {
        let json = serde_json::to_string(&val).unwrap();
        let rt: NtmApiSurface = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, rt);
    }

    #[test]
    fn ntm_api_surface_command_path_nonempty(val in arb_ntm_api_surface()) {
        prop_assert!(!val.command_path().is_empty());
        prop_assert!(val.command_path().contains(' '));
    }
}

// =============================================================================
// Deterministic invariant checks
// =============================================================================

#[test]
fn ntm_api_surface_all_has_22_entries() {
    assert_eq!(NtmApiSurface::ALL.len(), 22);
}

#[test]
fn ntm_api_surface_all_unique() {
    let mut seen = std::collections::HashSet::new();
    for surface in NtmApiSurface::ALL {
        assert!(seen.insert(surface), "duplicate surface: {:?}", surface);
    }
}

#[test]
fn ntm_api_surface_mutation_classification() {
    let mutations: Vec<_> = NtmApiSurface::ALL
        .iter()
        .filter(|s| s.is_mutation())
        .collect();
    let queries: Vec<_> = NtmApiSurface::ALL
        .iter()
        .filter(|s| !s.is_mutation())
        .collect();
    // Mutations: save, delete, rollback, rotate, claim, release, complete, assign, scale, rebalance, apply = 11
    assert_eq!(mutations.len(), 11);
    // Queries: list, show, status, history, list, ready, status, agents, list, show, validate = 11
    assert_eq!(queries.len(), 11);
}

#[test]
fn convergence_classification_labels_unique() {
    let labels = [
        ConvergenceClassification::DirectReplacement.label(),
        ConvergenceClassification::Upgrade.label(),
        ConvergenceClassification::Novel.label(),
        ConvergenceClassification::Partial.label(),
    ];
    let mut seen = std::collections::HashSet::new();
    for label in &labels {
        assert!(seen.insert(label), "duplicate label: {}", label);
    }
}
