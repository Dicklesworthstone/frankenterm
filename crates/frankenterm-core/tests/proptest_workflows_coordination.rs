// Property-based tests for workflows/coordination module.
//
// Covers: serde roundtrips for all public Serialize/Deserialize types,
// structural invariants for pane groups, broadcast results, coordination
// results, and unstick report properties.
#![allow(clippy::ignored_unit_patterns)]

use std::path::PathBuf;

use proptest::prelude::*;

use frankenterm_core::workflows::{
    BroadcastPrecondition, BroadcastResult, CoordinateAgentsConfig, CoordinationResult,
    GroupCoordinationEntry, GroupLockConflict, PaneBroadcastEntry, PaneBroadcastOutcome, PaneGroup,
    PaneGroupStrategy, UnstickConfig, UnstickFinding, UnstickFindingKind, UnstickReport,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_group_strategy() -> impl Strategy<Value = PaneGroupStrategy> {
    prop_oneof![
        Just(PaneGroupStrategy::ByDomain),
        Just(PaneGroupStrategy::ByAgent),
        Just(PaneGroupStrategy::ByProject),
        prop::collection::vec(0u64..10_000, 0..10)
            .prop_map(|pane_ids| PaneGroupStrategy::Explicit { pane_ids }),
    ]
}

fn arb_pane_group() -> impl Strategy<Value = PaneGroup> {
    (
        "[a-z_]{3,15}",
        prop::collection::vec(0u64..10_000, 0..10),
        arb_pane_group_strategy(),
    )
        .prop_map(|(name, pane_ids, strategy)| PaneGroup::new(name, pane_ids, strategy))
}

fn arb_broadcast_precondition() -> impl Strategy<Value = BroadcastPrecondition> {
    prop_oneof![
        Just(BroadcastPrecondition::PromptActive),
        Just(BroadcastPrecondition::NotAltScreen),
        Just(BroadcastPrecondition::NoRecentGap),
        Just(BroadcastPrecondition::NotReserved),
    ]
}

fn arb_pane_broadcast_outcome() -> impl Strategy<Value = PaneBroadcastOutcome> {
    prop_oneof![
        (0u64..60_000).prop_map(|ms| PaneBroadcastOutcome::Allowed { elapsed_ms: ms }),
        "[a-z ]{5,30}".prop_map(|reason| PaneBroadcastOutcome::Denied { reason }),
        prop::collection::vec("[a-z_]{3,12}".prop_map(String::from), 1..5)
            .prop_map(|failed| PaneBroadcastOutcome::PreconditionFailed { failed }),
        "[a-z ]{5,30}".prop_map(|reason| PaneBroadcastOutcome::Skipped { reason }),
        "[a-z ]{5,30}".prop_map(|reason| PaneBroadcastOutcome::VerificationFailed { reason }),
    ]
}

fn arb_pane_broadcast_entry() -> impl Strategy<Value = PaneBroadcastEntry> {
    (0u64..10_000, arb_pane_broadcast_outcome())
        .prop_map(|(pane_id, outcome)| PaneBroadcastEntry { pane_id, outcome })
}

fn arb_broadcast_result() -> impl Strategy<Value = BroadcastResult> {
    (
        "[a-z_]{3,20}",
        prop::collection::vec(arb_pane_broadcast_entry(), 0..8),
        0u64..120_000,
    )
        .prop_map(|(action, outcomes, total_elapsed_ms)| BroadcastResult {
            action,
            outcomes,
            total_elapsed_ms,
        })
}

fn arb_group_lock_conflict() -> impl Strategy<Value = GroupLockConflict> {
    (0u64..10_000, "[a-z_]{3,15}", "[a-z0-9]{8,16}").prop_map(
        |(pane_id, held_by_workflow, held_by_execution)| GroupLockConflict {
            pane_id,
            held_by_workflow,
            held_by_execution,
        },
    )
}

fn arb_coordinate_agents_config() -> impl Strategy<Value = CoordinateAgentsConfig> {
    (
        arb_pane_group_strategy(),
        prop::collection::vec(arb_broadcast_precondition(), 0..5),
        any::<bool>(),
    )
        .prop_map(
            |(strategy, preconditions, abort_on_lock_failure)| CoordinateAgentsConfig {
                strategy,
                preconditions,
                abort_on_lock_failure,
            },
        )
}

fn arb_group_coordination_entry() -> impl Strategy<Value = GroupCoordinationEntry> {
    (
        "[a-z_]{3,15}",
        0usize..100,
        0usize..100,
        0usize..100,
        0usize..100,
    )
        .prop_map(
            |(group_name, pane_count, acted_count, precondition_failed_count, skipped_count)| {
                GroupCoordinationEntry {
                    group_name,
                    pane_count,
                    acted_count,
                    precondition_failed_count,
                    skipped_count,
                }
            },
        )
}

fn arb_coordination_result() -> impl Strategy<Value = CoordinationResult> {
    (
        "[a-z_]{3,20}",
        prop::collection::vec(arb_group_coordination_entry(), 0..5),
        arb_broadcast_result(),
    )
        .prop_map(|(operation, groups, broadcast)| CoordinationResult {
            operation,
            groups,
            broadcast,
        })
}

fn arb_unstick_finding_kind() -> impl Strategy<Value = UnstickFindingKind> {
    prop_oneof![
        Just(UnstickFindingKind::TodoComment),
        Just(UnstickFindingKind::PanicSite),
        Just(UnstickFindingKind::SuppressedError),
    ]
}

fn arb_unstick_finding() -> impl Strategy<Value = UnstickFinding> {
    (
        arb_unstick_finding_kind(),
        "[a-z_/]{5,30}",
        1u32..10_000,
        "[a-zA-Z0-9_ ]{5,50}",
        "[a-zA-Z0-9_ ]{5,50}",
    )
        .prop_map(|(kind, file, line, snippet, suggestion)| UnstickFinding {
            kind,
            file,
            line,
            snippet,
            suggestion,
        })
}

fn arb_unstick_config() -> impl Strategy<Value = UnstickConfig> {
    (
        "[a-z_/]{3,20}".prop_map(PathBuf::from),
        1usize..50,
        1usize..100,
        prop::collection::vec("[a-z]{1,4}", 1..6),
    )
        .prop_map(
            |(root, max_findings_per_kind, max_total_findings, extensions)| UnstickConfig {
                root,
                max_findings_per_kind,
                max_total_findings,
                extensions,
            },
        )
}

fn arb_unstick_report() -> impl Strategy<Value = UnstickReport> {
    (
        prop::collection::vec(arb_unstick_finding(), 0..10),
        0usize..1000,
        any::<bool>(),
        prop_oneof![Just("ast-grep".to_string()), Just("text".to_string())],
        prop::collection::btree_map("[a-z_]{3,12}", 0usize..100, 0..5),
    )
        .prop_map(
            |(findings, files_scanned, truncated, scanner, counts)| UnstickReport {
                findings,
                files_scanned,
                truncated,
                scanner,
                counts,
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_group_strategy_serde_roundtrip(val in arb_pane_group_strategy()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn pane_group_serde_roundtrip(val in arb_pane_group()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PaneGroup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.name, back.name);
        prop_assert_eq!(val.pane_ids, back.pane_ids);
    }

    #[test]
    fn broadcast_precondition_serde_roundtrip(val in arb_broadcast_precondition()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: BroadcastPrecondition = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn pane_broadcast_outcome_serde_roundtrip(val in arb_pane_broadcast_outcome()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PaneBroadcastOutcome = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn pane_broadcast_entry_serde_roundtrip(val in arb_pane_broadcast_entry()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PaneBroadcastEntry = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn broadcast_result_serde_roundtrip(val in arb_broadcast_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: BroadcastResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.action, back.action);
        prop_assert_eq!(val.outcomes.len(), back.outcomes.len());
        prop_assert_eq!(val.total_elapsed_ms, back.total_elapsed_ms);
    }

    #[test]
    fn group_lock_conflict_serde_roundtrip(val in arb_group_lock_conflict()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: GroupLockConflict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.pane_id, back.pane_id);
        prop_assert_eq!(val.held_by_workflow, back.held_by_workflow);
        prop_assert_eq!(val.held_by_execution, back.held_by_execution);
    }

    #[test]
    fn coordinate_agents_config_serde_roundtrip(val in arb_coordinate_agents_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: CoordinateAgentsConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.abort_on_lock_failure, back.abort_on_lock_failure);
        prop_assert_eq!(val.preconditions.len(), back.preconditions.len());
    }

    #[test]
    fn group_coordination_entry_serde_roundtrip(val in arb_group_coordination_entry()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: GroupCoordinationEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.group_name, back.group_name);
        prop_assert_eq!(val.pane_count, back.pane_count);
        prop_assert_eq!(val.acted_count, back.acted_count);
    }

    #[test]
    fn coordination_result_serde_roundtrip(val in arb_coordination_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: CoordinationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.operation, back.operation);
        prop_assert_eq!(val.groups.len(), back.groups.len());
    }

    #[test]
    fn unstick_finding_kind_serde_roundtrip(val in arb_unstick_finding_kind()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: UnstickFindingKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn unstick_finding_serde_roundtrip(val in arb_unstick_finding()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: UnstickFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.file, back.file);
        prop_assert_eq!(val.line, back.line);
        prop_assert_eq!(val.snippet, back.snippet);
    }

    #[test]
    fn unstick_config_serde_roundtrip(val in arb_unstick_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: UnstickConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.root, back.root);
        prop_assert_eq!(val.max_findings_per_kind, back.max_findings_per_kind);
        prop_assert_eq!(val.max_total_findings, back.max_total_findings);
        prop_assert_eq!(val.extensions, back.extensions);
    }

    #[test]
    fn unstick_report_serde_roundtrip(val in arb_unstick_report()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: UnstickReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.findings.len(), back.findings.len());
        prop_assert_eq!(val.files_scanned, back.files_scanned);
        prop_assert_eq!(val.truncated, back.truncated);
        prop_assert_eq!(val.scanner, back.scanner);
        prop_assert_eq!(val.counts, back.counts);
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_group_len_matches_pane_ids(val in arb_pane_group()) {
        prop_assert_eq!(val.len(), val.pane_ids.len());
        prop_assert_eq!(val.is_empty(), val.pane_ids.is_empty());
    }

    #[test]
    fn broadcast_result_counts_consistent(val in arb_broadcast_result()) {
        let total = val.allowed_count()
            + val.denied_count()
            + val.precondition_failed_count()
            + val.skipped_count();
        // Verification failed entries are not counted by the individual counters,
        // so total <= outcomes.len()
        prop_assert!(total <= val.outcomes.len());

        if val.all_allowed() {
            prop_assert!(val.allowed_count() == val.outcomes.len());
            prop_assert!(!val.outcomes.is_empty());
        }
    }

    #[test]
    fn coordination_result_total_panes_matches_groups(val in arb_coordination_result()) {
        let expected: usize = val.groups.iter().map(|g| g.pane_count).sum();
        prop_assert_eq!(val.total_panes(), expected);
    }

    #[test]
    fn coordination_result_total_acted_matches_groups(val in arb_coordination_result()) {
        let expected: usize = val.groups.iter().map(|g| g.acted_count).sum();
        prop_assert_eq!(val.total_acted(), expected);
    }

    #[test]
    fn unstick_report_total_findings_matches_vec(val in arb_unstick_report()) {
        prop_assert_eq!(val.total_findings(), val.findings.len());
    }

    #[test]
    fn unstick_report_empty_produces_no_findings_summary(
        scanner in prop_oneof![Just("ast-grep"), Just("text")]
    ) {
        let report = UnstickReport::empty(scanner);
        prop_assert!(report.findings.is_empty());
        prop_assert_eq!(report.files_scanned, 0);
        prop_assert!(!report.truncated);
        let summary = report.human_summary();
        prop_assert!(summary.contains("No actionable findings"));
    }

    #[test]
    fn unstick_finding_kind_label_nonempty(val in arb_unstick_finding_kind()) {
        prop_assert!(!val.label().is_empty());
    }

    #[test]
    fn broadcast_precondition_label_nonempty(val in arb_broadcast_precondition()) {
        prop_assert!(!val.label().is_empty());
    }

    #[test]
    fn coordinate_agents_config_default_has_preconditions(_dummy in 0u8..1) {
        let config = CoordinateAgentsConfig::default();
        prop_assert!(!config.preconditions.is_empty());
        prop_assert!(!config.abort_on_lock_failure);
    }

    #[test]
    fn unstick_config_default_has_extensions(_dummy in 0u8..1) {
        let config = UnstickConfig::default();
        prop_assert!(!config.extensions.is_empty());
        prop_assert!(config.max_findings_per_kind > 0);
        prop_assert!(config.max_total_findings > 0);
    }

    #[test]
    fn unstick_report_human_summary_nonempty(val in arb_unstick_report()) {
        let summary = val.human_summary();
        prop_assert!(!summary.is_empty());
    }

    #[test]
    fn broadcast_result_new_starts_empty(action in "[a-z_]{3,20}") {
        let result = BroadcastResult::new(action.clone());
        prop_assert_eq!(&result.action, &action);
        prop_assert!(result.outcomes.is_empty());
        prop_assert_eq!(result.total_elapsed_ms, 0);
        prop_assert_eq!(result.allowed_count(), 0);
        prop_assert!(!result.all_allowed());
    }

    #[test]
    fn coordination_result_new_starts_empty(operation in "[a-z_]{3,20}") {
        let result = CoordinationResult::new(operation.clone());
        prop_assert_eq!(&result.operation, &operation);
        prop_assert!(result.groups.is_empty());
        prop_assert_eq!(result.total_panes(), 0);
        prop_assert_eq!(result.total_acted(), 0);
    }
}
