//! Property-based tests for beads_types.rs.
//!
//! Covers the DAG readiness resolver (`resolve_bead_readiness`), serde
//! roundtrips for all vendored types, status counting, actionability,
//! blocker semantics, transitive unblock counts, cycle detection, and
//! the from_summary degraded-detail path.

#![cfg(feature = "subprocess-bridge")]
#![allow(clippy::manual_range_contains)]
#![allow(clippy::comparison_chain)]

use std::collections::{HashMap, HashSet};

use frankenterm_core::beads_types::{
    BeadDependencyRef, BeadIssueDetail, BeadIssueType, BeadPriority, BeadReadinessReport,
    BeadReadyCandidate, BeadResolverReasonCode, BeadStatus, BeadStatusCounts, BeadSummary,
    resolve_bead_readiness,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_status() -> impl Strategy<Value = BeadStatus> {
    prop_oneof![
        Just(BeadStatus::Open),
        Just(BeadStatus::InProgress),
        Just(BeadStatus::Blocked),
        Just(BeadStatus::Deferred),
        Just(BeadStatus::Closed),
    ]
}

fn arb_issue_type() -> impl Strategy<Value = BeadIssueType> {
    prop_oneof![
        Just(BeadIssueType::Epic),
        Just(BeadIssueType::Feature),
        Just(BeadIssueType::Task),
        Just(BeadIssueType::Bug),
    ]
}

fn arb_bead_id() -> impl Strategy<Value = String> {
    "ft-[a-z]{2,4}[0-9]{0,2}".prop_map(|s| s)
}

fn arb_priority() -> impl Strategy<Value = u8> {
    0..=4u8
}

fn arb_summary() -> impl Strategy<Value = BeadSummary> {
    (
        arb_bead_id(),
        "[A-Z][a-z ]{3,15}",
        arb_status(),
        arb_priority(),
        arb_issue_type(),
    )
        .prop_map(|(id, title, status, priority, issue_type)| BeadSummary {
            id,
            title,
            status,
            priority,
            issue_type,
            assignee: None,
            labels: Vec::new(),
            dependency_count: 0,
            dependent_count: 0,
            extra: HashMap::new(),
        })
}

fn arb_dep_type() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(Some("blocks".to_string())),
        Just(Some("parent-child".to_string())),
        Just(None),
    ]
}

/// Generate a simple DAG of issues: all actionable (Open/InProgress), with
/// forward-only blocking edges (node i can only depend on node j where j < i).
fn arb_dag(max_nodes: usize) -> impl Strategy<Value = Vec<BeadIssueDetail>> {
    (1..=max_nodes)
        .prop_flat_map(|n| {
            let statuses = prop::collection::vec(
                prop_oneof![Just(BeadStatus::Open), Just(BeadStatus::InProgress)],
                n,
            );
            let priorities = prop::collection::vec(0..=4u8, n);
            // For each node (except node 0), decide which earlier nodes to depend on
            let deps = (0..n)
                .map(|i| {
                    if i == 0 {
                        Just(Vec::<usize>::new()).boxed()
                    } else {
                        prop::collection::vec(0..i, 0..=2)
                            .prop_map(|v| {
                                let mut dedup: Vec<usize> = v.into_iter().collect::<HashSet<_>>().into_iter().collect();
                                dedup.sort();
                                dedup
                            })
                            .boxed()
                    }
                })
                .collect::<Vec<_>>();
            (statuses, priorities, deps)
        })
        .prop_map(|(statuses, priorities, deps)| {
            let n = statuses.len();
            let ids: Vec<String> = (0..n).map(|i| format!("n{}", i)).collect();

            (0..n)
                .map(|i| {
                    let dependencies: Vec<BeadDependencyRef> = deps[i]
                        .iter()
                        .map(|&j| BeadDependencyRef {
                            id: ids[j].clone(),
                            title: None,
                            status: None,
                            priority: None,
                            dependency_type: Some("blocks".to_string()),
                        })
                        .collect();

                    BeadIssueDetail {
                        id: ids[i].clone(),
                        title: format!("Node {}", i),
                        status: statuses[i],
                        priority: priorities[i],
                        issue_type: BeadIssueType::Task,
                        assignee: None,
                        labels: Vec::new(),
                        dependencies,
                        dependents: Vec::new(),
                        parent: None,
                        ingest_warning: None,
                        extra: HashMap::new(),
                    }
                })
                .collect()
        })
}

fn make_detail(
    id: &str,
    status: BeadStatus,
    priority: u8,
    dep_ids: &[(&str, &str)],
) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {}", id),
        status,
        priority,
        issue_type: BeadIssueType::Task,
        assignee: None,
        labels: Vec::new(),
        dependencies: dep_ids
            .iter()
            .map(|(dep_id, dep_type)| BeadDependencyRef {
                id: (*dep_id).to_string(),
                title: None,
                status: None,
                priority: None,
                dependency_type: Some((*dep_type).to_string()),
            })
            .collect(),
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    }
}

// ── Serde roundtrips ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. BeadStatus serde roundtrip
    #[test]
    fn status_serde_roundtrip(status in arb_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: BeadStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, status);
    }

    // 2. BeadIssueType serde roundtrip
    #[test]
    fn issue_type_serde_roundtrip(issue_type in arb_issue_type()) {
        let json = serde_json::to_string(&issue_type).unwrap();
        let restored: BeadIssueType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, issue_type);
    }

    // 3. BeadPriority serde roundtrip + label consistency
    #[test]
    fn priority_serde_roundtrip(p in 0..=10u8) {
        let prio = BeadPriority(p);
        let json = serde_json::to_string(&prio).unwrap();
        let restored: BeadPriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, prio);
        prop_assert_eq!(prio.label(), format!("P{}", p));
        prop_assert_eq!(format!("{}", prio), format!("P{}", p));
    }

    // 4. BeadPriority ordering: lower value = higher priority
    #[test]
    fn priority_ordering(a in 0..=4u8, b in 0..=4u8) {
        let pa = BeadPriority(a);
        let pb = BeadPriority(b);
        if a < b {
            prop_assert!(pa < pb);
        } else if a > b {
            prop_assert!(pa > pb);
        } else {
            prop_assert_eq!(pa, pb);
        }
    }

    // 5. BeadSummary serde roundtrip
    #[test]
    fn summary_serde_roundtrip(summary in arb_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let restored: BeadSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.id, summary.id);
        prop_assert_eq!(restored.status, summary.status);
        prop_assert_eq!(restored.priority, summary.priority);
        prop_assert_eq!(restored.issue_type, summary.issue_type);
    }

    // 6. BeadResolverReasonCode serde roundtrip
    #[test]
    fn reason_code_serde_roundtrip(
        code in prop_oneof![
            Just(BeadResolverReasonCode::MissingDependencyNode),
            Just(BeadResolverReasonCode::CyclicDependencyGraph),
            Just(BeadResolverReasonCode::PartialGraphData),
        ]
    ) {
        let json = serde_json::to_string(&code).unwrap();
        let restored: BeadResolverReasonCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, code);
    }
}

// ── Actionability ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 7. is_actionable only for Open and InProgress
    #[test]
    fn actionable_only_open_or_in_progress(status in arb_status()) {
        let summary = BeadSummary {
            id: "test".into(),
            title: "t".into(),
            status,
            priority: 0,
            issue_type: BeadIssueType::Task,
            assignee: None,
            labels: Vec::new(),
            dependency_count: 0,
            dependent_count: 0,
            extra: HashMap::new(),
        };
        let expected = matches!(status, BeadStatus::Open | BeadStatus::InProgress);
        prop_assert_eq!(summary.is_actionable(), expected);
    }

    // 8. BeadIssueDetail actionability matches summary
    #[test]
    fn detail_actionable_matches(status in arb_status()) {
        let detail = make_detail("x", status, 0, &[]);
        let expected = matches!(status, BeadStatus::Open | BeadStatus::InProgress);
        prop_assert_eq!(detail.is_actionable(), expected);
    }
}

// ── BeadDependencyRef.blocks_readiness ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 9. parent-child never blocks; everything else does
    #[test]
    fn dep_ref_blocking_semantics(dep_type in arb_dep_type()) {
        let dep = BeadDependencyRef {
            id: "x".into(),
            title: None,
            status: None,
            priority: None,
            dependency_type: dep_type.clone(),
        };
        let expected_blocks = !matches!(dep_type.as_deref(), Some("parent-child"));
        prop_assert_eq!(dep.blocks_readiness(), expected_blocks);
    }
}

// ── StatusCounts ────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 10. StatusCounts.total() = sum of all fields
    #[test]
    fn status_counts_total(summaries in prop::collection::vec(arb_summary(), 0..=20)) {
        let counts = BeadStatusCounts::from_summaries(&summaries);
        prop_assert_eq!(counts.total(), summaries.len());
    }

    // 11. StatusCounts.actionable() = open + in_progress
    #[test]
    fn status_counts_actionable(summaries in prop::collection::vec(arb_summary(), 0..=20)) {
        let counts = BeadStatusCounts::from_summaries(&summaries);
        let expected_actionable = summaries
            .iter()
            .filter(|s| matches!(s.status, BeadStatus::Open | BeadStatus::InProgress))
            .count();
        prop_assert_eq!(counts.actionable(), expected_actionable);
    }

    // 12. StatusCounts conservation: individual counts sum to total
    #[test]
    fn status_counts_conservation(summaries in prop::collection::vec(arb_summary(), 0..=20)) {
        let c = BeadStatusCounts::from_summaries(&summaries);
        let sum = c.open + c.in_progress + c.blocked + c.deferred + c.closed;
        prop_assert_eq!(sum, summaries.len());
    }
}

// ── from_summary degradation ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 13. from_summary always adds PartialGraphData warning
    #[test]
    fn from_summary_adds_degradation(summary in arb_summary()) {
        let detail = BeadIssueDetail::from_summary(summary.clone());
        prop_assert_eq!(detail.id, summary.id);
        prop_assert_eq!(detail.status, summary.status);
        prop_assert_eq!(
            detail.ingest_warning,
            Some(BeadResolverReasonCode::PartialGraphData)
        );
        prop_assert!(detail.dependencies.is_empty());
        prop_assert!(detail.dependents.is_empty());
        prop_assert!(detail.parent.is_none());
    }
}

// ── Readiness resolver: basic invariants ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 14. Empty input → empty report
    #[test]
    fn readiness_empty_is_empty(_seed in 0..=10u32) {
        let report = resolve_bead_readiness(&[]);
        prop_assert!(report.candidates.is_empty());
        prop_assert!(report.ready_ids.is_empty());
        prop_assert!(report.degraded_reason_codes.is_empty());
    }

    // 15. Non-actionable issues excluded from candidates
    #[test]
    fn readiness_non_actionable_excluded(status in prop_oneof![
        Just(BeadStatus::Blocked),
        Just(BeadStatus::Deferred),
        Just(BeadStatus::Closed),
    ]) {
        let issues = vec![make_detail("x", status, 0, &[])];
        let report = resolve_bead_readiness(&issues);
        prop_assert!(report.candidates.is_empty());
        prop_assert!(report.ready_ids.is_empty());
    }

    // 16. Standalone actionable issue is always ready
    #[test]
    fn readiness_standalone_is_ready(
        status in prop_oneof![Just(BeadStatus::Open), Just(BeadStatus::InProgress)],
        priority in arb_priority(),
    ) {
        let issues = vec![make_detail("solo", status, priority, &[])];
        let report = resolve_bead_readiness(&issues);
        prop_assert_eq!(report.ready_count(), 1);
        let c = &report.candidates[0];
        prop_assert!(c.ready);
        prop_assert_eq!(c.blocker_count, 0);
        prop_assert!(c.blocker_ids.is_empty());
    }

    // 17. Candidates sorted by (priority, id)
    #[test]
    fn readiness_candidates_sorted(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        for pair in report.candidates.windows(2) {
            let cmp = (pair[0].priority, &pair[0].id).cmp(&(pair[1].priority, &pair[1].id));
            prop_assert!(
                cmp != std::cmp::Ordering::Greater,
                "candidates not sorted: {:?} before {:?}",
                (pair[0].priority, &pair[0].id),
                (pair[1].priority, &pair[1].id),
            );
        }
    }

    // 18. ready_ids is sorted
    #[test]
    fn readiness_ready_ids_sorted(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        for pair in report.ready_ids.windows(2) {
            prop_assert!(pair[0] <= pair[1], "ready_ids not sorted: {} > {}", pair[0], pair[1]);
        }
    }

    // 19. ready_ids ⊆ candidates
    #[test]
    fn readiness_ready_subset_of_candidates(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        let candidate_ids: HashSet<&str> = report.candidates.iter().map(|c| c.id.as_str()).collect();
        for rid in &report.ready_ids {
            prop_assert!(
                candidate_ids.contains(rid.as_str()),
                "ready_id {} not in candidates",
                rid
            );
        }
    }

    // 20. Every candidate with blocker_count == 0 should be ready
    #[test]
    fn readiness_zero_blockers_means_ready(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        for c in &report.candidates {
            if c.blocker_count == 0 {
                prop_assert!(c.ready, "candidate {} has 0 blockers but not ready", c.id);
            }
        }
    }

    // 21. Every candidate with blocker_count > 0 should NOT be ready
    #[test]
    fn readiness_nonzero_blockers_means_not_ready(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        for c in &report.candidates {
            if c.blocker_count > 0 {
                prop_assert!(!c.ready, "candidate {} has {} blockers but is ready", c.id, c.blocker_count);
            }
        }
    }

    // 22. blocker_count == blocker_ids.len()
    #[test]
    fn readiness_blocker_count_matches_ids(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        for c in &report.candidates {
            prop_assert_eq!(
                c.blocker_count,
                c.blocker_ids.len(),
                "candidate {}: count {} != ids.len() {}",
                c.id,
                c.blocker_count,
                c.blocker_ids.len(),
            );
        }
    }
}

// ── Readiness resolver: DAG properties ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 23. In a DAG with all nodes Open, node 0 (no deps) is always ready
    #[test]
    fn readiness_dag_root_always_ready(issues in arb_dag(5)) {
        let report = resolve_bead_readiness(&issues);
        // Node 0 has no dependencies by construction
        if let Some(root) = report.candidates.iter().find(|c| c.id == "n0") {
            prop_assert!(root.ready, "root node n0 should always be ready");
            prop_assert_eq!(root.blocker_count, 0);
        }
    }

    // 24. Transitive unblock count >= direct dependent count for root
    #[test]
    fn readiness_transitive_ge_direct(issues in arb_dag(6)) {
        let report = resolve_bead_readiness(&issues);
        if let Some(root) = report.candidates.iter().find(|c| c.id == "n0") {
            // Direct dependents
            let direct = issues.iter().filter(|iss| {
                iss.dependencies.iter().any(|d| d.id == "n0")
            }).count();
            prop_assert!(
                root.transitive_unblock_count >= direct,
                "transitive {} < direct {}",
                root.transitive_unblock_count,
                direct,
            );
        }
    }

    // 25. All candidates come from actionable issues
    #[test]
    fn readiness_only_actionable_in_candidates(issues in arb_dag(8)) {
        let report = resolve_bead_readiness(&issues);
        let actionable_ids: HashSet<&str> = issues
            .iter()
            .filter(|i| i.is_actionable())
            .map(|i| i.id.as_str())
            .collect();
        for c in &report.candidates {
            prop_assert!(
                actionable_ids.contains(c.id.as_str()),
                "non-actionable {} in candidates",
                c.id
            );
        }
    }
}

// ── Readiness resolver: cycle detection ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 26. Two-node cycle detected
    #[test]
    fn readiness_two_node_cycle(
        p1 in arb_priority(),
        p2 in arb_priority(),
    ) {
        let issues = vec![
            make_detail("A", BeadStatus::Open, p1, &[("B", "blocks")]),
            make_detail("B", BeadStatus::Open, p2, &[("A", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        prop_assert!(
            report.degraded_reason_codes.contains(&BeadResolverReasonCode::CyclicDependencyGraph),
            "cycle not detected"
        );
        for c in &report.candidates {
            prop_assert!(
                c.degraded_reasons.contains(&BeadResolverReasonCode::CyclicDependencyGraph),
                "candidate {} missing cycle flag",
                c.id
            );
        }
    }

    // 27. Three-node cycle detected
    #[test]
    fn readiness_three_node_cycle(_seed in 0..=10u32) {
        let issues = vec![
            make_detail("A", BeadStatus::Open, 0, &[("C", "blocks")]),
            make_detail("B", BeadStatus::Open, 0, &[("A", "blocks")]),
            make_detail("C", BeadStatus::Open, 0, &[("B", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        prop_assert!(
            report.degraded_reason_codes.contains(&BeadResolverReasonCode::CyclicDependencyGraph)
        );
    }

    // 28. No cycle in a valid DAG
    #[test]
    fn readiness_no_cycle_in_dag(issues in arb_dag(6)) {
        // arb_dag only generates forward edges (j < i), so no cycles
        let report = resolve_bead_readiness(&issues);
        prop_assert!(
            !report.degraded_reason_codes.contains(&BeadResolverReasonCode::CyclicDependencyGraph),
            "false cycle detected in a valid DAG"
        );
    }
}

// ── Readiness resolver: missing dependencies ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 29. Missing dependency creates blocker and MissingDependencyNode flag
    #[test]
    fn readiness_missing_dep_detected(
        missing_id in "ghost-[a-z]{2,4}",
        priority in arb_priority(),
    ) {
        let issues = vec![make_detail(
            "a",
            BeadStatus::Open,
            priority,
            &[(&missing_id, "blocks")],
        )];
        let report = resolve_bead_readiness(&issues);
        let a = &report.candidates[0];
        prop_assert!(!a.ready);
        prop_assert_eq!(a.blocker_count, 1);
        prop_assert!(
            a.degraded_reasons.contains(&BeadResolverReasonCode::MissingDependencyNode)
        );
        prop_assert!(
            report.degraded_reason_codes.contains(&BeadResolverReasonCode::MissingDependencyNode)
        );
    }

    // 30. Closed dependency doesn't block
    #[test]
    fn readiness_closed_dep_unblocks(priority in arb_priority()) {
        let issues = vec![
            make_detail("dep", BeadStatus::Closed, 0, &[]),
            make_detail("a", BeadStatus::Open, priority, &[("dep", "blocks")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let a = report.candidates.iter().find(|c| c.id == "a").unwrap();
        prop_assert!(a.ready);
        prop_assert_eq!(a.blocker_count, 0);
    }
}

// ── Readiness resolver: parent-child edges ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 31. parent-child edges don't block readiness
    #[test]
    fn readiness_parent_child_non_blocking(
        parent_status in prop_oneof![Just(BeadStatus::Open), Just(BeadStatus::InProgress)],
    ) {
        let issues = vec![
            make_detail("parent", parent_status, 0, &[]),
            make_detail("child", BeadStatus::Open, 1, &[("parent", "parent-child")]),
        ];
        let report = resolve_bead_readiness(&issues);
        let child = report.candidates.iter().find(|c| c.id == "child").unwrap();
        prop_assert!(child.ready, "parent-child should not block");
        prop_assert_eq!(child.blocker_count, 0);
    }
}

// ── Readiness report serde roundtrip ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 32. Full readiness report serde roundtrip
    #[test]
    fn readiness_report_serde_roundtrip(issues in arb_dag(6)) {
        let report = resolve_bead_readiness(&issues);
        let json = serde_json::to_string(&report).unwrap();
        let restored: BeadReadinessReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.ready_ids, report.ready_ids);
        prop_assert_eq!(restored.candidates.len(), report.candidates.len());
        prop_assert_eq!(restored.degraded_reason_codes, report.degraded_reason_codes);
        for (orig, rest) in report.candidates.iter().zip(restored.candidates.iter()) {
            prop_assert_eq!(&rest.id, &orig.id);
            prop_assert_eq!(rest.ready, orig.ready);
            prop_assert_eq!(rest.blocker_count, orig.blocker_count);
            prop_assert_eq!(&rest.blocker_ids, &orig.blocker_ids);
        }
    }

    // 33. BeadReadyCandidate serde roundtrip
    #[test]
    fn ready_candidate_serde_roundtrip(
        id in arb_bead_id(),
        priority in arb_priority(),
        blockers in 0..=3usize,
    ) {
        let candidate = BeadReadyCandidate {
            id: id.clone(),
            title: "test".into(),
            status: BeadStatus::Open,
            priority,
            blocker_count: blockers,
            blocker_ids: (0..blockers).map(|i| format!("b{}", i)).collect(),
            transitive_unblock_count: 0,
            critical_path_depth_hint: 0,
            ready: blockers == 0,
            degraded_reasons: Vec::new(),
        };
        let json = serde_json::to_string(&candidate).unwrap();
        let restored: BeadReadyCandidate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.id, &id);
        prop_assert_eq!(restored.blocker_count, blockers);
        prop_assert_eq!(restored.ready, blockers == 0);
    }

    // 34. BeadStatusCounts serde roundtrip
    #[test]
    fn status_counts_serde_roundtrip(
        open in 0..=10usize,
        in_progress in 0..=10usize,
        blocked in 0..=10usize,
        deferred in 0..=5usize,
        closed in 0..=20usize,
    ) {
        let counts = BeadStatusCounts {
            open,
            in_progress,
            blocked,
            deferred,
            closed,
        };
        let json = serde_json::to_string(&counts).unwrap();
        let restored: BeadStatusCounts = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.total(), counts.total());
        prop_assert_eq!(restored.actionable(), counts.actionable());
    }
}

// ── Determinism ─────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 35. resolve_bead_readiness is deterministic
    #[test]
    fn readiness_deterministic(issues in arb_dag(6)) {
        let r1 = resolve_bead_readiness(&issues);
        let r2 = resolve_bead_readiness(&issues);
        prop_assert_eq!(r1.ready_ids, r2.ready_ids);
        prop_assert_eq!(r1.candidates.len(), r2.candidates.len());
        for (a, b) in r1.candidates.iter().zip(r2.candidates.iter()) {
            prop_assert_eq!(&a.id, &b.id);
            prop_assert_eq!(a.ready, b.ready);
            prop_assert_eq!(a.blocker_count, b.blocker_count);
            prop_assert_eq!(a.transitive_unblock_count, b.transitive_unblock_count);
            prop_assert_eq!(a.critical_path_depth_hint, b.critical_path_depth_hint);
        }
    }
}
