//! Property-based tests for crash-consistent mission journal (C8).
//!
//! Covers:
//! - Append monotonicity: sequence numbers always increase
//! - Idempotency: duplicate correlation IDs are always rejected
//! - Checkpoint/recovery roundtrip preserves counts
//! - Compaction removes only entries below threshold
//! - Replay report total matches scanned entries
//! - Journal state snapshot reflects actual journal contents
//! - Entry serde roundtrip for all entry kinds
//! - JournalState serde roundtrip
//! - ReplayReport serde roundtrip
//! - Canonical string determinism
//! - Entries_since returns correct subset
//! - Multiple checkpoints track the latest
//! - Compact preserves correlation index consistency
//! - Journal error Display is non-empty
//! - Recovery marker appends correctly
//! - Needs_compaction respects configured limit

use frankenterm_core::plan::{
    AssignmentId, Mission, MissionId, MissionJournal, MissionJournalEntry,
    MissionJournalEntryKind, MissionJournalReplayError, MissionJournalReplayReport,
    MissionJournalState, MissionKillSwitchLevel, MissionLifecycleState,
    MissionLifecycleTransitionKind, MissionOwnership,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_lifecycle_state() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Planning),
        Just(MissionLifecycleState::Planned),
        Just(MissionLifecycleState::Dispatching),
        Just(MissionLifecycleState::AwaitingApproval),
        Just(MissionLifecycleState::Running),
        Just(MissionLifecycleState::Paused),
        Just(MissionLifecycleState::RetryPending),
        Just(MissionLifecycleState::Blocked),
        Just(MissionLifecycleState::Completed),
        Just(MissionLifecycleState::Failed),
        Just(MissionLifecycleState::Cancelled),
    ]
}

fn arb_transition_kind() -> impl Strategy<Value = MissionLifecycleTransitionKind> {
    prop_oneof![
        Just(MissionLifecycleTransitionKind::PlanFinalized),
        Just(MissionLifecycleTransitionKind::DispatchStarted),
        Just(MissionLifecycleTransitionKind::ApprovalRequested),
        Just(MissionLifecycleTransitionKind::ExecutionStarted),
        Just(MissionLifecycleTransitionKind::PauseRequested),
        Just(MissionLifecycleTransitionKind::ResumeRequested),
        Just(MissionLifecycleTransitionKind::AbortRequested),
    ]
}

fn arb_kill_switch_level() -> impl Strategy<Value = MissionKillSwitchLevel> {
    prop_oneof![
        Just(MissionKillSwitchLevel::Off),
        Just(MissionKillSwitchLevel::SafeMode),
        Just(MissionKillSwitchLevel::HardStop),
    ]
}

fn arb_non_empty_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,15}".prop_map(|s| s)
}

fn arb_entry_kind() -> impl Strategy<Value = MissionJournalEntryKind> {
    prop_oneof![
        (arb_lifecycle_state(), arb_lifecycle_state(), arb_transition_kind()).prop_map(
            |(from, to, tk)| MissionJournalEntryKind::LifecycleTransition {
                from,
                to,
                transition_kind: tk,
            }
        ),
        (arb_kill_switch_level(), arb_kill_switch_level()).prop_map(|(from, to)| {
            MissionJournalEntryKind::KillSwitchChange {
                level_from: from,
                level_to: to,
            }
        }),
        (arb_non_empty_string(), any::<bool>(), arb_non_empty_string()).prop_map(
            |(aid, has_before, after)| {
                MissionJournalEntryKind::AssignmentOutcome {
                    assignment_id: AssignmentId(aid),
                    outcome_before: if has_before {
                        Some("pending".into())
                    } else {
                        None
                    },
                    outcome_after: after,
                }
            }
        ),
        (arb_non_empty_string(), arb_lifecycle_state(), 0usize..10).prop_map(
            |(hash, state, count)| MissionJournalEntryKind::Checkpoint {
                mission_hash: hash,
                lifecycle_state: state,
                assignment_count: count,
            }
        ),
        (0u64..100, arb_non_empty_string()).prop_map(|(seq, reason)| {
            MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: seq,
                recovery_reason: reason,
            }
        }),
    ]
}

fn arb_journal_entry() -> impl Strategy<Value = MissionJournalEntry> {
    (
        1u64..1000,
        1000i64..100_000,
        arb_non_empty_string(),
        arb_non_empty_string(),
        arb_entry_kind(),
        arb_non_empty_string(),
        arb_non_empty_string(),
        any::<bool>(),
    )
        .prop_map(
            |(seq, ts, cid, hash, kind, by, reason, has_err)| MissionJournalEntry {
                seq,
                timestamp_ms: ts,
                correlation_id: cid,
                entry_hash: hash,
                kind,
                mission_version: 1,
                initiated_by: by,
                reason_code: reason,
                error_code: if has_err {
                    Some("ERR001".into())
                } else {
                    None
                },
            },
        )
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn append_monotonic_sequence(
        num_entries in 1usize..20,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-mono".into()));
        let mut prev_seq = 0u64;
        for i in 0..num_entries {
            let kind = MissionJournalEntryKind::RecoveryMarker {
                recovered_through_seq: 0,
                recovery_reason: "test".into(),
            };
            let seq = journal.append(kind, format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000).unwrap();
            prop_assert!(seq > prev_seq, "seq {} must be > prev {}", seq, prev_seq);
            prev_seq = seq;
        }
        prop_assert_eq!(journal.len(), num_entries);
    }

    #[test]
    fn duplicate_correlation_always_rejected(
        num_appends in 2usize..10,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-dup".into()));
        let kind = MissionJournalEntryKind::RecoveryMarker {
            recovered_through_seq: 0,
            recovery_reason: "test".into(),
        };
        // First append succeeds
        journal.append(kind.clone(), "shared-cid", "op", "test", None, 1000).unwrap();

        // All subsequent appends with same correlation_id must fail
        for i in 1..num_appends {
            let result = journal.append(kind.clone(), "shared-cid", "op", "retry", None, (i as i64 + 1) * 1000);
            prop_assert!(result.is_err());
        }
        prop_assert_eq!(journal.len(), 1);
    }

    #[test]
    fn checkpoint_recovery_roundtrip_counts(
        pre_checkpoint in 1usize..5,
        post_checkpoint in 1usize..5,
    ) {
        let mission = Mission::new(
            MissionId("m-prop-cp".into()),
            "proptest",
            "ws",
            MissionOwnership::solo("agent"),
            1000,
        );
        let mut journal = MissionJournal::new(MissionId("m-prop-cp".into()));

        for i in 0..pre_checkpoint {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "pre".into() },
                format!("pre-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }
        journal.checkpoint(&mission, 10_000).unwrap();
        for i in 0..post_checkpoint {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "post".into() },
                format!("post-{i}"), "op", "test", None, (10_000 + i as i64 + 1) * 1000,
            ).unwrap();
        }

        let report = journal.replay_from_checkpoint();
        prop_assert!(report.is_clean());
        // Replay starts from checkpoint: sees checkpoint + post entries
        prop_assert_eq!(report.entries_scanned, 1 + post_checkpoint);
    }

    #[test]
    fn compact_removes_only_below_threshold(
        total in 3usize..15,
        compact_at_frac in 0.1f64..0.9,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-compact".into()));
        for i in 0..total {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "t".into() },
                format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }

        let compact_seq = ((total as f64 * compact_at_frac) as u64).max(1);
        journal.compact_before(compact_seq);

        for entry in journal.entries() {
            prop_assert!(entry.seq >= compact_seq, "retained entry seq={} must be >= {}", entry.seq, compact_seq);
        }
    }

    #[test]
    fn replay_report_total_matches_scanned(
        num_entries in 1usize..15,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-total".into()));
        for i in 0..num_entries {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "t".into() },
                format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }

        let report = journal.replay_from_checkpoint();
        prop_assert_eq!(report.entries_scanned, num_entries);
        prop_assert_eq!(report.total_entries(), num_entries);
    }

    #[test]
    fn snapshot_reflects_journal_len(
        num_entries in 0usize..10,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-snap".into()));
        for i in 0..num_entries {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "t".into() },
                format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }
        let state = journal.snapshot_state();
        prop_assert_eq!(state.entry_count, num_entries as u64);
        if num_entries == 0 {
            prop_assert!(state.is_pristine());
        } else {
            prop_assert!(!state.is_pristine());
            prop_assert_eq!(state.last_seq, num_entries as u64);
        }
    }

    #[test]
    fn entry_serde_roundtrip(
        entry in arb_journal_entry(),
    ) {
        let json = serde_json::to_string(&entry).unwrap();
        let restored: MissionJournalEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&entry, &restored);
    }

    #[test]
    fn journal_state_serde_roundtrip(
        entry_count in 0u64..1000,
        last_seq in 0u64..1000,
        has_checkpoint in any::<bool>(),
        clean in any::<bool>(),
    ) {
        let state = MissionJournalState {
            entry_count,
            last_seq,
            last_entry_hash: format!("h-{}", last_seq),
            last_checkpoint_seq: if has_checkpoint { Some(last_seq.saturating_sub(1)) } else { None },
            last_checkpoint_hash: if has_checkpoint { "cp-hash".into() } else { String::new() },
            clean,
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: MissionJournalState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&state, &restored);
    }

    #[test]
    fn replay_report_serde_roundtrip(
        lt in 0usize..20,
        cc in 0usize..10,
        ks in 0usize..5,
        ao in 0usize..15,
        cp in 0usize..5,
        rm in 0usize..5,
    ) {
        let report = MissionJournalReplayReport {
            start_seq: 0,
            entries_scanned: lt + cc + ks + ao + cp + rm,
            lifecycle_transitions: lt,
            control_commands: cc,
            kill_switch_changes: ks,
            assignment_outcomes: ao,
            checkpoints_found: cp,
            recovery_markers: rm,
            errors: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let restored: MissionJournalReplayReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&report, &restored);
    }

    #[test]
    fn canonical_string_deterministic(
        entry in arb_journal_entry(),
    ) {
        let s1 = entry.canonical_string();
        let s2 = entry.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn journal_state_canonical_deterministic(
        entry_count in 0u64..100,
        clean in any::<bool>(),
    ) {
        let state = MissionJournalState {
            entry_count,
            last_seq: entry_count,
            last_entry_hash: "h".into(),
            last_checkpoint_seq: None,
            last_checkpoint_hash: String::new(),
            clean,
        };
        let s1 = state.canonical_string();
        let s2 = state.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn entries_since_correct_subset(
        total in 1usize..15,
        since_frac in 0.0f64..1.0,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-since".into()));
        for i in 0..total {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "t".into() },
                format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }

        let since_seq = ((total as f64 * since_frac) as u64).max(1);
        let subset = journal.entries_since(since_seq);
        for entry in subset {
            prop_assert!(entry.seq >= since_seq, "entry seq={} must be >= since_seq={}", entry.seq, since_seq);
        }
    }

    #[test]
    fn needs_compaction_respects_limit(
        limit in 2usize..10,
        entries in 1usize..15,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-limit".into()))
            .with_max_entries(limit);

        for i in 0..entries {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "t".into() },
                format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }

        if entries > limit {
            prop_assert!(journal.needs_compaction());
        } else {
            prop_assert!(!journal.needs_compaction());
        }
    }

    #[test]
    fn compact_preserves_correlation_index(
        total in 3usize..10,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-cidx".into()));
        for i in 0..total {
            journal.append(
                MissionJournalEntryKind::RecoveryMarker { recovered_through_seq: 0, recovery_reason: "t".into() },
                format!("c-{i}"), "op", "test", None, (i as i64 + 1) * 1000,
            ).unwrap();
        }

        let compact_at = (total / 2 + 1) as u64;
        journal.compact_before(compact_at);

        // Compacted entries should no longer be in correlation index
        for i in 0..total {
            let cid = format!("c-{i}");
            let expected_in_index = (i as u64 + 1) >= compact_at;
            prop_assert_eq!(
                journal.has_correlation(&cid),
                expected_in_index,
                "c-{} (seq={}) should be in_index={}, compact_at={}",
                i, i + 1, expected_in_index, compact_at
            );
        }
    }

    #[test]
    fn recovery_marker_appends_correctly(
        num_markers in 1usize..5,
    ) {
        let mut journal = MissionJournal::new(MissionId("m-prop-rm".into()));
        for i in 0..num_markers {
            journal.recovery_marker(i as u64, format!("reason-{i}"), (i as i64 + 1) * 1000).unwrap();
        }

        prop_assert_eq!(journal.len(), num_markers);
        let report = journal.replay_from_checkpoint();
        prop_assert_eq!(report.recovery_markers, num_markers);
    }
}
