//! Property tests for workflows::coordination module.
//!
//! Covers serde roundtrip for all 14 serializable types plus behavioral
//! invariants for BroadcastResult counters and UnstickReport summaries.

use frankenterm_core::workflows::*;
use proptest::prelude::*;


// =============================================================================
// Arbitrary strategies
// =============================================================================

fn arb_pane_group_strategy() -> impl Strategy<Value = PaneGroupStrategy> {
    prop_oneof![
        Just(PaneGroupStrategy::ByDomain),
        Just(PaneGroupStrategy::ByAgent),
        Just(PaneGroupStrategy::ByProject),
        prop::collection::vec(0..10_000u64, 0..10)
            .prop_map(|pane_ids| PaneGroupStrategy::Explicit { pane_ids }),
    ]
}

fn arb_pane_group() -> impl Strategy<Value = PaneGroup> {
    (
        "[a-z_]{1,15}",
        prop::collection::vec(0..10_000u64, 0..10),
        arb_pane_group_strategy(),
    )
        .prop_map(|(name, pane_ids, strategy)| PaneGroup::new(name, pane_ids, strategy))
}

fn arb_group_lock_conflict() -> impl Strategy<Value = GroupLockConflict> {
    (0..10_000u64, "[a-z_]{1,15}", "[a-z0-9_-]{1,15}").prop_map(
        |(pane_id, held_by_workflow, held_by_execution)| GroupLockConflict {
            pane_id,
            held_by_workflow,
            held_by_execution,
        },
    )
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
        (0..100_000u64).prop_map(|elapsed_ms| PaneBroadcastOutcome::Allowed { elapsed_ms }),
        "[a-z ]{1,30}".prop_map(|reason| PaneBroadcastOutcome::Denied { reason }),
        prop::collection::vec("[a-z_]{1,15}", 1..5)
            .prop_map(|failed| PaneBroadcastOutcome::PreconditionFailed { failed }),
        "[a-z ]{1,30}".prop_map(|reason| PaneBroadcastOutcome::Skipped { reason }),
        "[a-z ]{1,30}"
            .prop_map(|reason| PaneBroadcastOutcome::VerificationFailed { reason }),
    ]
}

fn arb_pane_broadcast_entry() -> impl Strategy<Value = PaneBroadcastEntry> {
    (0..10_000u64, arb_pane_broadcast_outcome()).prop_map(|(pane_id, outcome)| {
        PaneBroadcastEntry { pane_id, outcome }
    })
}

fn arb_broadcast_result() -> impl Strategy<Value = BroadcastResult> {
    (
        "[a-z_]{1,15}",
        prop::collection::vec(arb_pane_broadcast_entry(), 0..10),
        0..100_000u64,
    )
        .prop_map(|(action, outcomes, total_elapsed_ms)| BroadcastResult {
            action,
            outcomes,
            total_elapsed_ms,
        })
}

fn arb_coordinate_agents_config() -> impl Strategy<Value = CoordinateAgentsConfig> {
    (
        arb_pane_group_strategy(),
        prop::collection::vec(arb_broadcast_precondition(), 0..5),
        prop::bool::ANY,
    )
        .prop_map(|(strategy, preconditions, abort_on_lock_failure)| {
            CoordinateAgentsConfig {
                strategy,
                preconditions,
                abort_on_lock_failure,
            }
        })
}

fn arb_group_coordination_entry() -> impl Strategy<Value = GroupCoordinationEntry> {
    (
        "[a-z_]{1,15}",
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
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
        "[a-z_]{1,15}",
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
        "[a-z/_.]{1,30}",
        1..10_000u32,
        "[a-z ]{1,50}",
        "[a-z ]{1,40}",
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
        "[a-z/]{1,20}",
        1..50usize,
        1..100usize,
        prop::collection::vec("[a-z]{1,5}", 1..5),
    )
        .prop_map(
            |(root, max_findings_per_kind, max_total_findings, extensions)| UnstickConfig {
                root: std::path::PathBuf::from(root),
                max_findings_per_kind,
                max_total_findings,
                extensions,
            },
        )
}

fn arb_unstick_report() -> impl Strategy<Value = UnstickReport> {
    (
        prop::collection::vec(arb_unstick_finding(), 0..10),
        0..1000usize,
        prop::bool::ANY,
        "[a-z_-]{1,15}",
        prop::collection::btree_map("[a-z_]{1,10}", 0..100usize, 0..5),
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
    fn pane_group_strategy_json_roundtrip(s in arb_pane_group_strategy()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&s, &back);
    }

    #[test]
    fn pane_group_json_roundtrip(g in arb_pane_group()) {
        let json = serde_json::to_string(&g).unwrap();
        let back: PaneGroup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&g.name, &back.name);
        prop_assert_eq!(&g.pane_ids, &back.pane_ids);
        prop_assert_eq!(&g.strategy, &back.strategy);
    }

    #[test]
    fn group_lock_conflict_json_roundtrip(c in arb_group_lock_conflict()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: GroupLockConflict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c.pane_id, back.pane_id);
        prop_assert_eq!(&c.held_by_workflow, &back.held_by_workflow);
        prop_assert_eq!(&c.held_by_execution, &back.held_by_execution);
    }

    #[test]
    fn broadcast_precondition_json_roundtrip(p in arb_broadcast_precondition()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: BroadcastPrecondition = serde_json::from_str(&json).unwrap();
        // BroadcastPrecondition doesn't derive PartialEq, compare via label
        prop_assert_eq!(p.label(), back.label());
    }

    #[test]
    fn pane_broadcast_outcome_json_roundtrip(o in arb_pane_broadcast_outcome()) {
        let json = serde_json::to_string(&o).unwrap();
        let _back: PaneBroadcastOutcome = serde_json::from_str(&json).unwrap();
        // Deserialize succeeds — variant tag preserved
    }

    #[test]
    fn pane_broadcast_entry_json_roundtrip(e in arb_pane_broadcast_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: PaneBroadcastEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e.pane_id, back.pane_id);
    }

    #[test]
    fn broadcast_result_json_roundtrip(r in arb_broadcast_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: BroadcastResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&r.action, &back.action);
        prop_assert_eq!(r.outcomes.len(), back.outcomes.len());
        prop_assert_eq!(r.total_elapsed_ms, back.total_elapsed_ms);
    }

    #[test]
    fn coordinate_agents_config_json_roundtrip(c in arb_coordinate_agents_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: CoordinateAgentsConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&c.strategy, &back.strategy);
        prop_assert_eq!(c.abort_on_lock_failure, back.abort_on_lock_failure);
        prop_assert_eq!(c.preconditions.len(), back.preconditions.len());
    }

    #[test]
    fn group_coordination_entry_json_roundtrip(e in arb_group_coordination_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: GroupCoordinationEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&e.group_name, &back.group_name);
        prop_assert_eq!(e.pane_count, back.pane_count);
        prop_assert_eq!(e.acted_count, back.acted_count);
        prop_assert_eq!(e.precondition_failed_count, back.precondition_failed_count);
        prop_assert_eq!(e.skipped_count, back.skipped_count);
    }

    #[test]
    fn coordination_result_json_roundtrip(r in arb_coordination_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: CoordinationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&r.operation, &back.operation);
        prop_assert_eq!(r.groups.len(), back.groups.len());
    }

    #[test]
    fn unstick_finding_kind_json_roundtrip(k in arb_unstick_finding_kind()) {
        let json = serde_json::to_string(&k).unwrap();
        let back: UnstickFindingKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(k, back);
    }

    #[test]
    fn unstick_finding_json_roundtrip(f in arb_unstick_finding()) {
        let json = serde_json::to_string(&f).unwrap();
        let back: UnstickFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(f.kind, back.kind);
        prop_assert_eq!(&f.file, &back.file);
        prop_assert_eq!(f.line, back.line);
        prop_assert_eq!(&f.snippet, &back.snippet);
        prop_assert_eq!(&f.suggestion, &back.suggestion);
    }

    #[test]
    fn unstick_config_json_roundtrip(c in arb_unstick_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: UnstickConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c.root, back.root);
        prop_assert_eq!(c.max_findings_per_kind, back.max_findings_per_kind);
        prop_assert_eq!(c.max_total_findings, back.max_total_findings);
        prop_assert_eq!(&c.extensions, &back.extensions);
    }

    #[test]
    fn unstick_report_json_roundtrip(r in arb_unstick_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: UnstickReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r.findings.len(), back.findings.len());
        prop_assert_eq!(r.files_scanned, back.files_scanned);
        prop_assert_eq!(r.truncated, back.truncated);
        prop_assert_eq!(&r.scanner, &back.scanner);
        prop_assert_eq!(&r.counts, &back.counts);
    }
}

// =============================================================================
// Behavioral property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    // -- PaneGroup len/is_empty consistency --

    #[test]
    fn pane_group_len_consistent(g in arb_pane_group()) {
        prop_assert_eq!(g.len(), g.pane_ids.len());
        prop_assert_eq!(g.is_empty(), g.pane_ids.is_empty());
    }

    // -- BroadcastResult counter consistency --

    #[test]
    fn broadcast_result_counters_sum_to_len(r in arb_broadcast_result()) {
        let allowed = r.allowed_count();
        let denied = r.denied_count();
        let precond = r.precondition_failed_count();
        let skipped = r.skipped_count();
        // Verification failures are not counted by any named counter,
        // but the sum of named + verification should equal total
        let verification = r.outcomes.iter().filter(|e| {
            matches!(e.outcome, PaneBroadcastOutcome::VerificationFailed { .. })
        }).count();
        prop_assert_eq!(
            allowed + denied + precond + skipped + verification,
            r.outcomes.len(),
            "counter sum must equal total outcomes"
        );
    }

    #[test]
    fn broadcast_result_all_allowed_iff_all_outcomes_allowed(r in arb_broadcast_result()) {
        if r.outcomes.is_empty() {
            prop_assert!(!r.all_allowed(), "empty results are not all_allowed");
        } else {
            let expected = r.allowed_count() == r.outcomes.len();
            prop_assert_eq!(r.all_allowed(), expected);
        }
    }

    // -- CoordinationResult total counts --

    #[test]
    fn coordination_result_total_panes_is_sum(r in arb_coordination_result()) {
        let sum: usize = r.groups.iter().map(|g| g.pane_count).sum();
        prop_assert_eq!(r.total_panes(), sum);
    }

    #[test]
    fn coordination_result_total_acted_is_sum(r in arb_coordination_result()) {
        let sum: usize = r.groups.iter().map(|g| g.acted_count).sum();
        prop_assert_eq!(r.total_acted(), sum);
    }

    // -- UnstickReport total_findings --

    #[test]
    fn unstick_report_total_findings_is_len(r in arb_unstick_report()) {
        prop_assert_eq!(r.total_findings(), r.findings.len());
    }

    #[test]
    fn unstick_report_empty_has_no_findings_message(_scanner in "[a-z]{1,10}") {
        let report = UnstickReport::empty(&_scanner);
        prop_assert_eq!(report.total_findings(), 0);
        prop_assert!(!report.truncated);
        let summary = report.human_summary();
        prop_assert!(summary.contains("No actionable findings"));
    }

    #[test]
    fn unstick_report_nonempty_summary_mentions_count(r in arb_unstick_report()) {
        let summary = r.human_summary();
        if r.findings.is_empty() {
            prop_assert!(summary.contains("No actionable findings"));
        } else {
            let expected = format!("Found {}", r.findings.len());
            let has_count = summary.contains(&expected);
            prop_assert!(has_count);
        }
    }

    // -- UnstickFindingKind label consistency --

    #[test]
    fn unstick_finding_kind_label_nonempty(k in arb_unstick_finding_kind()) {
        prop_assert!(!k.label().is_empty());
    }

    // -- BroadcastPrecondition label consistency --

    #[test]
    fn broadcast_precondition_label_nonempty(p in arb_broadcast_precondition()) {
        prop_assert!(!p.label().is_empty());
    }

    // -- Default impls --

    #[test]
    fn coordinate_agents_config_default_has_4_preconditions(_dummy in 0..1u8) {
        let cfg = CoordinateAgentsConfig::default();
        prop_assert_eq!(cfg.preconditions.len(), 4);
        prop_assert!(!cfg.abort_on_lock_failure);
    }

    #[test]
    fn unstick_config_default_has_sane_limits(_dummy in 0..1u8) {
        let cfg = UnstickConfig::default();
        prop_assert_eq!(cfg.max_findings_per_kind, 10);
        prop_assert_eq!(cfg.max_total_findings, 25);
        prop_assert!(!cfg.extensions.is_empty());
        prop_assert!(cfg.extensions.contains(&"rs".to_string()));
    }

    // -- truncate_snippet --

    #[test]
    fn truncate_snippet_short_input_unchanged(s in "[a-z]{1,10}") {
        let result = truncate_snippet(&s, 100);
        prop_assert_eq!(result, s.trim());
    }

    #[test]
    fn truncate_snippet_long_input_bounded(s in "[a-z]{50,100}", max_len in 10..30usize) {
        let result = truncate_snippet(&s, max_len);
        prop_assert!(result.len() <= max_len + 3, "truncated result too long: {} > {}", result.len(), max_len + 3);
        if s.trim().len() > max_len {
            prop_assert!(result.ends_with("..."));
        }
    }
}
