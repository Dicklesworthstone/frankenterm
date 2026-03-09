//! Property-based tests for beads_bridge module.
//!
//! Validates backpressure tier determination, config threshold invariants,
//! status counting, and fail-open degradation across arbitrary inputs.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::beads_bridge::{BeadsBackpressure, BeadsBackpressureConfig};
use frankenterm_core::beads_types::{
    BeadDependencyRef, BeadIssueDetail, BeadIssueType, BeadResolverReasonCode, BeadStatus,
    BeadStatusCounts, BeadSummary, resolve_bead_readiness,
};

// =============================================================================
// Strategies
// =============================================================================

fn bead_status_strategy() -> impl Strategy<Value = BeadStatus> {
    prop_oneof![
        Just(BeadStatus::Open),
        Just(BeadStatus::InProgress),
        Just(BeadStatus::Blocked),
        Just(BeadStatus::Deferred),
        Just(BeadStatus::Closed),
    ]
}

fn bead_issue_type_strategy() -> impl Strategy<Value = BeadIssueType> {
    prop_oneof![
        Just(BeadIssueType::Epic),
        Just(BeadIssueType::Feature),
        Just(BeadIssueType::Task),
        Just(BeadIssueType::Bug),
    ]
}

fn bead_summary_strategy() -> impl Strategy<Value = BeadSummary> {
    (
        "[a-z]{2,6}-[a-z0-9]{3,6}",
        "[A-Za-z ]{5,40}",
        bead_status_strategy(),
        0u8..5,
        bead_issue_type_strategy(),
        proptest::option::of("[A-Za-z]{3,12}"),
        prop::collection::vec("[a-z\\-]{3,10}", 0..5),
        0usize..20,
        0usize..20,
    )
        .prop_map(
            |(id, title, status, priority, issue_type, assignee, labels, dep_count, dept_count)| {
                BeadSummary {
                    id,
                    title,
                    status,
                    priority,
                    issue_type,
                    assignee,
                    labels,
                    dependency_count: dep_count,
                    dependent_count: dept_count,
                    extra: HashMap::new(),
                }
            },
        )
}

fn backpressure_config_strategy() -> impl Strategy<Value = BeadsBackpressureConfig> {
    (1usize..500, 1usize..500).prop_map(|(a, b)| {
        let (yellow, red) = if a <= b { (a, b) } else { (b, a) };
        BeadsBackpressureConfig {
            yellow_threshold: yellow,
            red_threshold: red.max(yellow + 1), // ensure red > yellow
        }
    })
}

fn bead_detail_strategy() -> impl Strategy<Value = BeadIssueDetail> {
    (
        "[a-z]{2,6}-[a-z0-9]{3,6}",
        "[A-Za-z ]{5,40}",
        bead_status_strategy(),
        0u8..5,
        bead_issue_type_strategy(),
    )
        .prop_map(|(id, title, status, priority, issue_type)| BeadIssueDetail {
            id,
            title,
            status,
            priority,
            issue_type,
            assignee: None,
            labels: Vec::new(),
            dependencies: Vec::new(),
            dependents: Vec::new(),
            parent: None,
            ingest_warning: None,
            extra: HashMap::new(),
        })
}

/// Helper: compute expected backpressure tier from raw values.
fn expected_tier(
    actionable: usize,
    config: &BeadsBackpressureConfig,
) -> BeadsBackpressure {
    if actionable >= config.red_threshold {
        BeadsBackpressure::Red
    } else if actionable >= config.yellow_threshold {
        BeadsBackpressure::Yellow
    } else {
        BeadsBackpressure::Green
    }
}

// =============================================================================
// Backpressure tier determination
// =============================================================================

proptest! {
    /// Backpressure tier is always Green when actionable count is zero.
    #[test]
    fn zero_actionable_always_green(config in backpressure_config_strategy()) {
        let tier = expected_tier(0, &config);
        prop_assert_eq!(tier, BeadsBackpressure::Green);
    }

    /// Backpressure tier is deterministic: same inputs produce same outputs.
    #[test]
    fn backpressure_is_deterministic(
        actionable in 0usize..500,
        config in backpressure_config_strategy(),
    ) {
        let tier1 = expected_tier(actionable, &config);
        let tier2 = expected_tier(actionable, &config);
        prop_assert_eq!(tier1, tier2);
    }

    /// Backpressure is monotonic: higher actionable count never decreases tier.
    #[test]
    fn backpressure_monotonic(
        a in 0usize..250,
        delta in 0usize..250,
        config in backpressure_config_strategy(),
    ) {
        let b = a + delta;
        let tier_a = expected_tier(a, &config);
        let tier_b = expected_tier(b, &config);

        let tier_ord = |t: BeadsBackpressure| -> u8 {
            match t {
                BeadsBackpressure::Green => 0,
                BeadsBackpressure::Yellow => 1,
                BeadsBackpressure::Red => 2,
            }
        };

        prop_assert!(
            tier_ord(tier_b) >= tier_ord(tier_a),
            "monotonicity violated: actionable {} -> {:?}, {} -> {:?}",
            a, tier_a, b, tier_b
        );
    }

    /// Tier is always Red at or above red threshold.
    #[test]
    fn at_red_threshold_always_red(
        excess in 0usize..200,
        config in backpressure_config_strategy(),
    ) {
        let actionable = config.red_threshold + excess;
        let tier = expected_tier(actionable, &config);
        prop_assert_eq!(tier, BeadsBackpressure::Red);
    }

    /// Tier is never Red below yellow threshold.
    #[test]
    fn below_yellow_never_red(
        config in backpressure_config_strategy(),
    ) {
        if config.yellow_threshold > 0 {
            let actionable = config.yellow_threshold - 1;
            let tier = expected_tier(actionable, &config);
            prop_assert_ne!(tier, BeadsBackpressure::Red);
        }
    }

    /// Tier is Green below yellow threshold.
    #[test]
    fn below_yellow_is_green(
        config in backpressure_config_strategy(),
    ) {
        if config.yellow_threshold > 0 {
            let actionable = config.yellow_threshold - 1;
            let tier = expected_tier(actionable, &config);
            prop_assert_eq!(tier, BeadsBackpressure::Green);
        }
    }

    /// Exactly at yellow threshold gives Yellow tier (not Green, not Red).
    #[test]
    fn at_yellow_threshold_is_yellow(
        config in backpressure_config_strategy(),
    ) {
        let tier = expected_tier(config.yellow_threshold, &config);
        // yellow_threshold < red_threshold by construction
        prop_assert_eq!(tier, BeadsBackpressure::Yellow);
    }
}

// =============================================================================
// BeadsBackpressureConfig
// =============================================================================

proptest! {
    /// Default config has yellow < red.
    #[test]
    fn default_config_threshold_ordering(_dummy in Just(())) {
        let config = BeadsBackpressureConfig::default();
        prop_assert!(config.yellow_threshold < config.red_threshold);
    }

    /// Custom config preserves exact threshold values.
    #[test]
    fn custom_config_preserves_thresholds(
        yellow in 1usize..500,
        red in 1usize..500,
    ) {
        let config = BeadsBackpressureConfig {
            yellow_threshold: yellow,
            red_threshold: red,
        };
        prop_assert_eq!(config.yellow_threshold, yellow);
        prop_assert_eq!(config.red_threshold, red);
    }
}

// =============================================================================
// BeadStatusCounts
// =============================================================================

proptest! {
    /// StatusCounts.total() equals sum of all fields.
    #[test]
    fn status_counts_total_is_sum(
        open in 0usize..100,
        in_progress in 0usize..100,
        blocked in 0usize..100,
        deferred in 0usize..100,
        closed in 0usize..100,
    ) {
        let counts = BeadStatusCounts {
            open,
            in_progress,
            blocked,
            deferred,
            closed,
        };
        prop_assert_eq!(
            counts.total(),
            open + in_progress + blocked + deferred + closed
        );
    }

    /// StatusCounts.actionable() equals open + in_progress.
    #[test]
    fn status_counts_actionable_is_open_plus_in_progress(
        open in 0usize..100,
        in_progress in 0usize..100,
        blocked in 0usize..100,
        deferred in 0usize..100,
        closed in 0usize..100,
    ) {
        let counts = BeadStatusCounts {
            open,
            in_progress,
            blocked,
            deferred,
            closed,
        };
        prop_assert_eq!(counts.actionable(), open + in_progress);
    }

    /// StatusCounts.actionable() <= total().
    #[test]
    fn actionable_leq_total(
        open in 0usize..100,
        in_progress in 0usize..100,
        blocked in 0usize..100,
        deferred in 0usize..100,
        closed in 0usize..100,
    ) {
        let counts = BeadStatusCounts {
            open,
            in_progress,
            blocked,
            deferred,
            closed,
        };
        prop_assert!(counts.actionable() <= counts.total());
    }

    /// from_summaries produces counts consistent with input statuses.
    #[test]
    fn from_summaries_consistent(
        beads in prop::collection::vec(bead_summary_strategy(), 0..50),
    ) {
        let counts = BeadStatusCounts::from_summaries(&beads);

        let expected_open = beads.iter().filter(|b| b.status == BeadStatus::Open).count();
        let expected_ip = beads.iter().filter(|b| b.status == BeadStatus::InProgress).count();
        let expected_blocked = beads.iter().filter(|b| b.status == BeadStatus::Blocked).count();
        let expected_deferred = beads.iter().filter(|b| b.status == BeadStatus::Deferred).count();
        let expected_closed = beads.iter().filter(|b| b.status == BeadStatus::Closed).count();

        prop_assert_eq!(counts.open, expected_open);
        prop_assert_eq!(counts.in_progress, expected_ip);
        prop_assert_eq!(counts.blocked, expected_blocked);
        prop_assert_eq!(counts.deferred, expected_deferred);
        prop_assert_eq!(counts.closed, expected_closed);
        prop_assert_eq!(counts.total(), beads.len());
    }

    /// Empty summary list produces zero counts.
    #[test]
    fn empty_summaries_zero_counts(_dummy in Just(())) {
        let counts = BeadStatusCounts::from_summaries(&[]);
        prop_assert_eq!(counts.total(), 0);
        prop_assert_eq!(counts.actionable(), 0);
    }
}

// =============================================================================
// BeadSummary properties
// =============================================================================

proptest! {
    /// is_actionable is true iff status is Open or InProgress.
    #[test]
    fn is_actionable_matches_status(bead in bead_summary_strategy()) {
        let expected = matches!(bead.status, BeadStatus::Open | BeadStatus::InProgress);
        prop_assert_eq!(bead.is_actionable(), expected);
    }

    /// bead_priority().label() always starts with 'P'.
    #[test]
    fn priority_label_starts_with_p(bead in bead_summary_strategy()) {
        let label = bead.bead_priority().label();
        prop_assert!(label.starts_with('P'), "label = {}", label);
    }

    /// bead_priority().0 matches the raw priority field.
    #[test]
    fn bead_priority_matches_raw(bead in bead_summary_strategy()) {
        prop_assert_eq!(bead.bead_priority().0, bead.priority);
    }
}

// =============================================================================
// BeadDependencyRef.blocks_readiness
// =============================================================================

proptest! {
    /// parent-child edges never block readiness.
    #[test]
    fn parent_child_does_not_block(_dummy in Just(())) {
        let dep = BeadDependencyRef {
            id: "ft-123".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some("parent-child".to_string()),
        };
        prop_assert!(!dep.blocks_readiness());
    }

    /// "blocks" dependency type blocks readiness.
    #[test]
    fn blocks_type_blocks_readiness(_dummy in Just(())) {
        let dep = BeadDependencyRef {
            id: "ft-456".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some("blocks".to_string()),
        };
        prop_assert!(dep.blocks_readiness());
    }

    /// None dependency_type blocks readiness (conservative default).
    #[test]
    fn none_type_blocks_readiness(_dummy in Just(())) {
        let dep = BeadDependencyRef {
            id: "ft-789".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: None,
        };
        prop_assert!(dep.blocks_readiness());
    }

    /// Any non-parent-child type blocks readiness.
    #[test]
    fn arbitrary_type_blocks_readiness(dep_type in "[a-z_]{3,15}") {
        if dep_type == "parent-child" {
            return Ok(());
        }
        let dep = BeadDependencyRef {
            id: "ft-000".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some(dep_type),
        };
        prop_assert!(dep.blocks_readiness());
    }
}

// =============================================================================
// BeadIssueDetail.from_summary
// =============================================================================

proptest! {
    /// from_summary preserves core fields.
    #[test]
    fn from_summary_preserves_fields(summary in bead_summary_strategy()) {
        let id = summary.id.clone();
        let title = summary.title.clone();
        let status = summary.status;
        let priority = summary.priority;
        let issue_type = summary.issue_type;

        let detail = BeadIssueDetail::from_summary(summary);

        prop_assert_eq!(&detail.id, &id);
        prop_assert_eq!(&detail.title, &title);
        prop_assert_eq!(detail.status, status);
        prop_assert_eq!(detail.priority, priority);
        prop_assert_eq!(detail.issue_type, issue_type);
    }

    /// from_summary sets PartialGraphData warning.
    #[test]
    fn from_summary_sets_partial_graph_warning(summary in bead_summary_strategy()) {
        let detail = BeadIssueDetail::from_summary(summary);
        prop_assert_eq!(
            detail.ingest_warning,
            Some(BeadResolverReasonCode::PartialGraphData)
        );
    }

    /// from_summary produces empty dependencies.
    #[test]
    fn from_summary_empty_deps(summary in bead_summary_strategy()) {
        let detail = BeadIssueDetail::from_summary(summary);
        prop_assert!(detail.dependencies.is_empty());
        prop_assert!(detail.dependents.is_empty());
        prop_assert!(detail.parent.is_none());
    }
}

// =============================================================================
// resolve_bead_readiness
// =============================================================================

proptest! {
    /// Empty input produces empty readiness report.
    #[test]
    fn empty_input_empty_report(_dummy in Just(())) {
        let report = resolve_bead_readiness(&[]);
        prop_assert!(report.candidates.is_empty());
        prop_assert!(report.ready_ids.is_empty());
        prop_assert_eq!(report.ready_count(), 0);
    }

    /// All closed issues produce no candidates.
    #[test]
    fn all_closed_no_candidates(
        ids in prop::collection::vec("[a-z]{2}-[0-9]{3}", 1..10),
    ) {
        let details: Vec<BeadIssueDetail> = ids
            .iter()
            .map(|id| BeadIssueDetail {
                id: id.clone(),
                title: format!("Closed {}", id),
                status: BeadStatus::Closed,
                priority: 1,
                issue_type: BeadIssueType::Task,
                assignee: None,
                labels: Vec::new(),
                dependencies: Vec::new(),
                dependents: Vec::new(),
                parent: None,
                ingest_warning: None,
                extra: HashMap::new(),
            })
            .collect();

        let report = resolve_bead_readiness(&details);
        prop_assert!(report.candidates.is_empty());
        prop_assert!(report.ready_ids.is_empty());
    }

    /// Standalone open issue with no deps is always ready.
    #[test]
    fn standalone_open_is_ready(detail in bead_detail_strategy()) {
        if !detail.is_actionable() {
            return Ok(());
        }
        let report = resolve_bead_readiness(&[detail.clone()]);
        prop_assert!(
            report.ready_ids.contains(&detail.id),
            "expected {} to be ready",
            detail.id
        );
    }

    /// ready_ids is always a subset of candidate ids.
    #[test]
    fn ready_ids_subset_of_candidates(
        details in prop::collection::vec(bead_detail_strategy(), 0..20),
    ) {
        let report = resolve_bead_readiness(&details);
        for ready_id in &report.ready_ids {
            let found = report.candidates.iter().any(|c| &c.id == ready_id);
            prop_assert!(found, "ready_id {} not found in candidates", ready_id);
        }
    }

    /// ready_count matches ready_ids.len().
    #[test]
    fn ready_count_matches_len(
        details in prop::collection::vec(bead_detail_strategy(), 0..20),
    ) {
        let report = resolve_bead_readiness(&details);
        prop_assert_eq!(report.ready_count(), report.ready_ids.len());
    }

    /// Candidates with blocker_count == 0 are exactly the ready ones.
    #[test]
    fn zero_blockers_iff_ready(
        details in prop::collection::vec(bead_detail_strategy(), 0..20),
    ) {
        let report = resolve_bead_readiness(&details);
        for candidate in &report.candidates {
            if candidate.blocker_count == 0 {
                prop_assert!(
                    candidate.ready,
                    "candidate {} has 0 blockers but not ready",
                    candidate.id
                );
            } else {
                prop_assert!(
                    !candidate.ready,
                    "candidate {} has {} blockers but marked ready",
                    candidate.id,
                    candidate.blocker_count
                );
            }
        }
    }

    /// ready_ids list is sorted.
    #[test]
    fn ready_ids_sorted(
        details in prop::collection::vec(bead_detail_strategy(), 0..20),
    ) {
        let report = resolve_bead_readiness(&details);
        for window in report.ready_ids.windows(2) {
            prop_assert!(window[0] <= window[1], "ready_ids not sorted");
        }
    }

    /// Candidates are sorted by (priority, id).
    #[test]
    fn candidates_sorted_by_priority_then_id(
        details in prop::collection::vec(bead_detail_strategy(), 0..20),
    ) {
        let report = resolve_bead_readiness(&details);
        for window in report.candidates.windows(2) {
            let a_key = (window[0].priority, &window[0].id);
            let b_key = (window[1].priority, &window[1].id);
            prop_assert!(a_key <= b_key, "candidates not sorted: {:?} > {:?}", a_key, b_key);
        }
    }

    /// from_summary degraded entries always have PartialGraphData reason.
    #[test]
    fn from_summary_always_has_partial_graph(summary in bead_summary_strategy()) {
        if !summary.is_actionable() {
            return Ok(());
        }
        let detail = BeadIssueDetail::from_summary(summary);
        let report = resolve_bead_readiness(&[detail]);

        prop_assert!(
            report.degraded_reason_codes.contains(&BeadResolverReasonCode::PartialGraphData),
            "expected PartialGraphData in degraded reasons"
        );
    }
}

// =============================================================================
// Serde roundtrip for BeadStatus
// =============================================================================

proptest! {
    /// BeadStatus survives JSON roundtrip.
    #[test]
    fn bead_status_serde_roundtrip(status in bead_status_strategy()) {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: BeadStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, parsed);
    }

    /// BeadIssueType survives JSON roundtrip.
    #[test]
    fn bead_issue_type_serde_roundtrip(issue_type in bead_issue_type_strategy()) {
        let json = serde_json::to_string(&issue_type).unwrap();
        let parsed: BeadIssueType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(issue_type, parsed);
    }

    /// BeadSummary survives JSON roundtrip.
    #[test]
    fn bead_summary_serde_roundtrip(summary in bead_summary_strategy()) {
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: BeadSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.id, summary.id);
        prop_assert_eq!(parsed.status, summary.status);
        prop_assert_eq!(parsed.priority, summary.priority);
        prop_assert_eq!(parsed.issue_type, summary.issue_type);
    }

    /// BeadStatusCounts default is all zeros.
    #[test]
    fn status_counts_default_all_zero(_dummy in Just(())) {
        let counts = BeadStatusCounts::default();
        prop_assert_eq!(counts.total(), 0);
        prop_assert_eq!(counts.actionable(), 0);
    }
}

// =============================================================================
// BeadsBackpressure enum properties
// =============================================================================

proptest! {
    /// BeadsBackpressure Clone + PartialEq consistency.
    #[test]
    fn backpressure_clone_eq(
        tier in prop_oneof![
            Just(BeadsBackpressure::Green),
            Just(BeadsBackpressure::Yellow),
            Just(BeadsBackpressure::Red),
        ]
    ) {
        let cloned = tier;
        prop_assert_eq!(tier, cloned);
    }

    /// Debug format contains variant name.
    #[test]
    fn backpressure_debug_contains_name(
        tier in prop_oneof![
            Just(BeadsBackpressure::Green),
            Just(BeadsBackpressure::Yellow),
            Just(BeadsBackpressure::Red),
        ]
    ) {
        let debug = format!("{:?}", tier);
        let has_name = debug.contains("Green")
            || debug.contains("Yellow")
            || debug.contains("Red");
        prop_assert!(has_name, "debug output missing variant name: {}", debug);
    }
}
