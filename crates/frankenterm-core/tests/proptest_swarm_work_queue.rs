//! Property-based tests for swarm_work_queue (ft-3681t.3.3.1).
//!
//! Coverage:
//! - DAG invariants: no blocked→ready transitions while deps are incomplete
//! - Cycle rejection: enqueue never creates cycles in the dependency graph
//! - Batch ordering: batch_enqueue resolves internal dependencies correctly
//! - Priority ordering: pull always returns highest-priority (lowest number) ready item
//! - Beads bridge: JSONL roundtrip integrity, status mapping completeness
//! - Snapshot/restore: queue state survives serialization roundtrips

use std::collections::{HashMap, HashSet};
use std::thread::sleep;
use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::swarm_work_queue::{
    Assignment, BeadRecord, BeadsImportError, BeadsImporter, BeadsSyncReport, CompletionRecord,
    QueueSnapshot, QueueStats, SwarmWorkQueue, WorkItem, WorkItemStatus, WorkQueueConfig,
    WorkQueueError,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_item(id: &str, prio: u32) -> WorkItem {
    WorkItem {
        id: id.into(),
        title: format!("item {id}"),
        priority: prio,
        depends_on: Vec::new(),
        effort: 1,
        labels: Vec::new(),
        preferred_program: None,
        metadata: HashMap::new(),
    }
}

fn make_dep_item(id: &str, prio: u32, deps: Vec<&str>) -> WorkItem {
    WorkItem {
        id: id.into(),
        title: format!("item {id}"),
        priority: prio,
        depends_on: deps.into_iter().map(String::from).collect(),
        effort: 1,
        labels: Vec::new(),
        preferred_program: None,
        metadata: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn work_item_id() -> impl Strategy<Value = String> {
    "[a-z]{1,4}-[0-9]{1,3}".prop_map(|s| s)
}

fn priority() -> impl Strategy<Value = u32> {
    0u32..10
}

fn work_item_no_deps() -> impl Strategy<Value = WorkItem> {
    (work_item_id(), priority()).prop_map(|(id, prio)| make_item(&id, prio))
}

fn bead_status() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("open".to_string()),
        Just("in_progress".to_string()),
        Just("closed".to_string()),
        Just("cancelled".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// DAG invariant: blocked items never become ready while deps are incomplete
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn dag_invariant_blocked_items_stay_blocked_until_deps_done(
        root_prio in priority(),
        mid_prio in priority(),
        leaf_prio in priority(),
    ) {
        let mut q = SwarmWorkQueue::with_defaults();

        q.enqueue(make_dep_item("root", root_prio, vec![])).unwrap();
        q.enqueue(make_dep_item("mid", mid_prio, vec!["root"])).unwrap();
        q.enqueue(make_dep_item("leaf", leaf_prio, vec!["mid"])).unwrap();

        // mid and leaf must be blocked
        prop_assert_eq!(q.item_status(&"mid".into()), Some(WorkItemStatus::Blocked));
        prop_assert_eq!(q.item_status(&"leaf".into()), Some(WorkItemStatus::Blocked));

        // Complete root -> mid becomes ready, leaf still blocked
        q.assign(&"root".into(), &"agent".into()).unwrap();
        q.complete(&"root".into(), &"agent".into(), None).unwrap();
        prop_assert_eq!(q.item_status(&"mid".into()), Some(WorkItemStatus::Ready));
        prop_assert_eq!(q.item_status(&"leaf".into()), Some(WorkItemStatus::Blocked));

        // Complete mid -> leaf becomes ready
        q.assign(&"mid".into(), &"agent".into()).unwrap();
        q.complete(&"mid".into(), &"agent".into(), None).unwrap();
        prop_assert_eq!(q.item_status(&"leaf".into()), Some(WorkItemStatus::Ready));
    }

    #[test]
    fn dag_fanin_requires_all_dependencies_before_ready(
        left_prio in priority(),
        right_prio in priority(),
        sink_prio in priority(),
        complete_left_first in any::<bool>(),
    ) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("left", left_prio)).unwrap();
        q.enqueue(make_item("right", right_prio)).unwrap();
        q.enqueue(make_dep_item("sink", sink_prio, vec!["left", "right"])).unwrap();

        prop_assert_eq!(q.item_status(&"sink".into()), Some(WorkItemStatus::Blocked));

        let (first, second) = if complete_left_first {
            ("left", "right")
        } else {
            ("right", "left")
        };

        q.assign(&first.into(), &"agent".into()).unwrap();
        q.complete(&first.into(), &"agent".into(), None).unwrap();
        prop_assert_eq!(
            q.item_status(&"sink".into()),
            Some(WorkItemStatus::Blocked),
            "sink must remain blocked until both deps complete"
        );

        q.assign(&second.into(), &"agent".into()).unwrap();
        q.complete(&second.into(), &"agent".into(), None).unwrap();
        prop_assert_eq!(q.item_status(&"sink".into()), Some(WorkItemStatus::Ready));
    }

    // -----------------------------------------------------------------------
    // Cycle rejection: adding deps that would create a cycle always fails
    // -----------------------------------------------------------------------

    #[test]
    fn cycle_detection_rejects_all_self_loops(id in work_item_id()) {
        let q = SwarmWorkQueue::with_defaults();
        let has_cycle = q.would_create_cycle(&id, std::slice::from_ref(&id));
        prop_assert!(has_cycle, "self-loop must be detected as cycle");
    }

    #[test]
    fn cycle_detection_rejects_back_edges_in_linear_chain(
        chain_len in 2usize..8,
        back_edge_to in 1usize..8,
    ) {
        prop_assume!(back_edge_to < chain_len);
        let mut q = SwarmWorkQueue::with_defaults();

        // Build n-0 <- n-1 <- ... <- n-(len-1)
        for i in 0..chain_len {
            let id = format!("n-{i}");
            let deps = if i == 0 {
                vec![]
            } else {
                vec![format!("n-{}", i - 1)]
            };
            q.enqueue(WorkItem {
                id: id.clone(),
                title: id,
                priority: i as u32,
                depends_on: deps,
                effort: 1,
                labels: Vec::new(),
                preferred_program: None,
                metadata: HashMap::new(),
            })
            .unwrap();
        }

        // Adding dependency n-(k-1) -> n-k creates cycle because n-k already depends on n-(k-1).
        let from = format!("n-{}", back_edge_to - 1);
        let to = format!("n-{back_edge_to}");
        prop_assert!(
            q.would_create_cycle(&from, &[to]),
            "back-edge must be rejected as cycle"
        );
    }

    // -----------------------------------------------------------------------
    // Priority ordering: pull returns lowest-numbered priority first
    // -----------------------------------------------------------------------

    #[test]
    fn pull_returns_highest_priority_item(
        items in prop::collection::vec(work_item_no_deps(), 2..8)
    ) {
        let config = WorkQueueConfig {
            anti_starvation: false,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);

        // Deduplicate IDs
        let mut seen = std::collections::HashSet::new();
        let unique_items: Vec<WorkItem> = items
            .into_iter()
            .filter(|item| seen.insert(item.id.clone()))
            .collect();

        if unique_items.len() < 2 {
            return Ok(());
        }

        for item in &unique_items {
            q.enqueue(item.clone()).unwrap();
        }

        // Find the minimum priority among all items
        let min_priority = unique_items.iter().map(|i| i.priority).min().unwrap();

        // Pull should return an item with the minimum priority
        let assignment = q.pull(&"agent".into()).unwrap();
        let pulled_item = unique_items.iter().find(|i| i.id == assignment.work_item_id).unwrap();
        prop_assert_eq!(pulled_item.priority, min_priority);
    }

    #[test]
    fn anti_starvation_enabled_without_elapsed_wait_still_respects_priority(
        items in prop::collection::vec(work_item_no_deps(), 2..8)
    ) {
        let config = WorkQueueConfig {
            anti_starvation: true,
            starvation_threshold_ms: u64::MAX,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);

        let mut seen = HashSet::new();
        let unique_items: Vec<WorkItem> = items
            .into_iter()
            .filter(|item| seen.insert(item.id.clone()))
            .collect();

        if unique_items.len() < 2 {
            return Ok(());
        }

        for item in &unique_items {
            q.enqueue(item.clone()).unwrap();
        }

        let min_priority = unique_items.iter().map(|i| i.priority).min().unwrap();
        let assignment = q.pull(&"agent".into()).unwrap();
        let pulled_item = unique_items.iter().find(|i| i.id == assignment.work_item_id).unwrap();
        prop_assert_eq!(pulled_item.priority, min_priority);
    }

    // -----------------------------------------------------------------------
    // Batch enqueue: items referencing each other resolve internally
    // -----------------------------------------------------------------------

    #[test]
    fn batch_enqueue_resolves_internal_deps_correctly(
        root_prio in priority(),
        child_prio in priority(),
    ) {
        let mut q = SwarmWorkQueue::with_defaults();

        let root = make_item("batch-root", root_prio);
        let child = make_dep_item("batch-child", child_prio, vec!["batch-root"]);

        let results = q.enqueue_batch(vec![root, child]);

        prop_assert!(results[0].is_ok());
        prop_assert!(results[1].is_ok());
        prop_assert_eq!(q.item_status(&"batch-root".into()), Some(WorkItemStatus::Ready));
        prop_assert_eq!(q.item_status(&"batch-child".into()), Some(WorkItemStatus::Blocked));
    }

    #[test]
    fn batch_enqueue_topological_chain_keeps_only_head_ready(
        chain_len in 2usize..8,
        prios in prop::collection::vec(priority(), 8),
    ) {
        prop_assume!(chain_len <= prios.len());
        let mut q = SwarmWorkQueue::with_defaults();
        let mut batch = Vec::new();

        for (i, prio) in prios.iter().enumerate().take(chain_len) {
            let id = format!("chain-{i}");
            let deps = if i == 0 {
                vec![]
            } else {
                vec![format!("chain-{}", i - 1)]
            };
            batch.push(WorkItem {
                id: id.clone(),
                title: id,
                priority: *prio,
                depends_on: deps,
                effort: 1,
                labels: Vec::new(),
                preferred_program: None,
                metadata: HashMap::new(),
            });
        }

        let results = q.enqueue_batch(batch);
        prop_assert!(results.iter().all(Result::is_ok));
        prop_assert_eq!(q.item_status(&"chain-0".into()), Some(WorkItemStatus::Ready));
        for i in 1..chain_len {
            prop_assert_eq!(
                q.item_status(&format!("chain-{i}")),
                Some(WorkItemStatus::Blocked),
                "only the chain head should be ready"
            );
        }
    }

    #[test]
    fn batch_enqueue_reverse_order_reports_dependency_errors(
        chain_len in 2usize..8,
        prios in prop::collection::vec(priority(), 8),
    ) {
        prop_assume!(chain_len <= prios.len());
        let mut q = SwarmWorkQueue::with_defaults();
        let mut batch = Vec::new();

        for (i, prio) in prios.iter().enumerate().take(chain_len) {
            let id = format!("rev-{i}");
            let deps = if i == 0 {
                vec![]
            } else {
                vec![format!("rev-{}", i - 1)]
            };
            batch.push(WorkItem {
                id: id.clone(),
                title: id,
                priority: *prio,
                depends_on: deps,
                effort: 1,
                labels: Vec::new(),
                preferred_program: None,
                metadata: HashMap::new(),
            });
        }
        batch.reverse();

        let results = q.enqueue_batch(batch);
        prop_assert!(
            results
                .iter()
                .any(|r| matches!(r, Err(WorkQueueError::DependencyNotFound { .. }))),
            "reverse order must surface dependency ordering errors"
        );
    }

    // -----------------------------------------------------------------------
    // Stats consistency: stats always reflect actual queue state
    // -----------------------------------------------------------------------

    #[test]
    fn stats_are_consistent_after_operations(
        n_items in 1usize..6,
        prios in prop::collection::vec(priority(), 6),
    ) {
        let mut q = SwarmWorkQueue::with_defaults();

        for (i, prio) in prios.iter().enumerate().take(n_items) {
            q.enqueue(make_item(&format!("s-{i}"), *prio)).unwrap();
        }

        let stats = q.stats();
        prop_assert_eq!(stats.total_items, n_items);
        prop_assert_eq!(stats.ready, n_items);
        prop_assert_eq!(stats.blocked, 0);
        prop_assert_eq!(stats.in_progress, 0);
        prop_assert_eq!(stats.completed, 0);

        // Pull one item
        if n_items > 0 {
            q.pull(&"agent".into()).unwrap();
            let stats2 = q.stats();
            prop_assert_eq!(stats2.in_progress, 1);
            prop_assert_eq!(stats2.ready, n_items - 1);
        }
    }

    // -----------------------------------------------------------------------
    // Snapshot/restore roundtrip preserves queue state
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_restore_roundtrip_preserves_state(
        n_items in 1usize..5,
        prios in prop::collection::vec(priority(), 5),
    ) {
        let config = WorkQueueConfig {
            anti_starvation: false,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config.clone());

        for (i, prio) in prios.iter().enumerate().take(n_items) {
            q.enqueue(make_item(&format!("snap-{i}"), *prio)).unwrap();
        }

        let snapshot = q.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: QueueSnapshot = serde_json::from_str(&json).unwrap();
        let q2 = SwarmWorkQueue::restore(restored, config);

        let orig_stats = q.stats();
        let rest_stats = q2.stats();
        prop_assert_eq!(orig_stats.total_items, rest_stats.total_items);
        prop_assert_eq!(orig_stats.ready, rest_stats.ready);
        prop_assert_eq!(orig_stats.blocked, rest_stats.blocked);
    }

    // -----------------------------------------------------------------------
    // Beads bridge: JSONL parsing roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn beads_jsonl_roundtrip_preserves_record_count(n_records in 1usize..5) {
        let mut lines = Vec::new();
        for i in 0..n_records {
            lines.push(format!(
                r#"{{"id":"pt-{i}","title":"proptest item {i}","status":"open","priority":1,"issue_type":"task","labels":[]}}"#
            ));
        }
        let jsonl = lines.join("\n");

        let importer = BeadsImporter::from_jsonl(&jsonl).unwrap();
        let mut q = SwarmWorkQueue::with_defaults();
        let report = importer.sync_to_queue(&mut q);

        prop_assert_eq!(report.imported as usize, n_records);
        prop_assert_eq!(report.skipped, 0u32);
    }

    #[test]
    fn beads_importer_roundtrip_preserves_actionable_membership(
        records in prop::collection::vec((work_item_id(), bead_status(), priority()), 1..10)
    ) {
        let mut seen = HashSet::new();
        let deduped: Vec<(String, String, u32)> = records
            .into_iter()
            .filter(|(id, _, _)| seen.insert(id.clone()))
            .collect();

        if deduped.is_empty() {
            return Ok(());
        }

        let jsonl = deduped
            .iter()
            .map(|(id, status, prio)| {
                format!(
                    r#"{{"id":"{id}","title":"{id} title","status":"{status}","priority":{prio},"issue_type":"task","labels":[]}}"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let importer = BeadsImporter::from_jsonl(&jsonl).unwrap();
        let mut q = SwarmWorkQueue::with_defaults();
        let report = importer.sync_to_queue(&mut q);

        let actionable: Vec<String> = deduped
            .iter()
            .filter(|(_, status, _)| status == "open" || status == "in_progress")
            .map(|(id, _, _)| id.clone())
            .collect();
        let terminal: Vec<String> = deduped
            .iter()
            .filter(|(_, status, _)| status == "closed" || status == "cancelled")
            .map(|(id, _, _)| id.clone())
            .collect();

        prop_assert_eq!(report.imported as usize, actionable.len());
        for id in actionable {
            prop_assert_eq!(q.item_status(&id), Some(WorkItemStatus::Ready));
        }
        for id in terminal {
            prop_assert_eq!(q.item_status(&id), None);
        }
    }

    // -----------------------------------------------------------------------
    // Beads bridge: closed beads are not imported
    // -----------------------------------------------------------------------

    #[test]
    fn beads_closed_records_not_imported(
        status in prop_oneof![
            Just("closed".to_string()),
            Just("cancelled".to_string()),
            Just("resolved".to_string()),
            Just("wontfix".to_string()),
        ],
    ) {
        let jsonl = format!(
            r#"{{"id":"closed-1","title":"closed item","status":"{status}","priority":1}}"#
        );

        let importer = BeadsImporter::from_jsonl(&jsonl).unwrap();
        let mut q = SwarmWorkQueue::with_defaults();
        let report = importer.sync_to_queue(&mut q);

        prop_assert_eq!(report.imported, 0u32, "closed/terminal beads should not be imported");
    }

    // -----------------------------------------------------------------------
    // Status mapping is total: every WorkItemStatus maps to a valid string
    // -----------------------------------------------------------------------

    #[test]
    fn work_status_to_bead_covers_all_variants(status_idx in 0u8..6) {
        let status = match status_idx {
            0 => WorkItemStatus::Blocked,
            1 => WorkItemStatus::Ready,
            2 => WorkItemStatus::InProgress,
            3 => WorkItemStatus::Completed,
            4 => WorkItemStatus::Failed,
            _ => WorkItemStatus::Cancelled,
        };

        let bead_status = frankenterm_core::swarm_work_queue::work_status_to_bead_status(status);
        prop_assert!(!bead_status.is_empty(), "bead status must not be empty");
        prop_assert!(
            ["open", "in_progress", "closed"].contains(&bead_status),
            "unexpected bead status: {}", bead_status
        );
    }

    // -----------------------------------------------------------------------
    // Duplicate enqueue is always rejected
    // -----------------------------------------------------------------------

    #[test]
    fn duplicate_enqueue_always_rejected(item in work_item_no_deps()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item.clone()).unwrap();
        let result = q.enqueue(item);
        let is_dup = matches!(result, Err(WorkQueueError::DuplicateItem { .. }));
        prop_assert!(is_dup, "expected DuplicateItem error");
    }

    // -----------------------------------------------------------------------
    // Complete/fail only works on in-progress items
    // -----------------------------------------------------------------------

    #[test]
    fn complete_only_works_on_in_progress(item in work_item_no_deps()) {
        let mut q = SwarmWorkQueue::with_defaults();
        let id = item.id.clone();
        q.enqueue(item).unwrap();

        // Direct complete on Ready item should fail
        let result = q.complete(&id, &"agent".into(), None);
        prop_assert!(result.is_err(), "complete on Ready item should fail");

        // Assign then complete should succeed
        q.assign(&id, &"agent".into()).unwrap();
        let result = q.complete(&id, &"agent".into(), None);
        prop_assert!(result.is_ok(), "complete on InProgress item should succeed");
    }
}

// =============================================================================
// Additional coverage tests (SQ-19 through SQ-40)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── SQ-19: WorkItem serde roundtrip ─────────────────────────────────────

    #[test]
    fn sq19_work_item_serde(
        id in "[a-z]{2,6}-[0-9]{1,3}",
        title in "[a-zA-Z ]{5,30}",
        prio in 0u32..100,
        effort in 1u32..20,
    ) {
        let item = WorkItem {
            id,
            title,
            priority: prio,
            depends_on: Vec::new(),
            effort,
            labels: vec!["test".into()],
            preferred_program: Some("prog".into()),
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: WorkItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&item, &back);
    }

    // ── SQ-20: WorkItemStatus serde roundtrip for all 6 variants ────────────

    #[test]
    fn sq20_work_item_status_serde(idx in 0u8..6) {
        let status = match idx {
            0 => WorkItemStatus::Blocked,
            1 => WorkItemStatus::Ready,
            2 => WorkItemStatus::InProgress,
            3 => WorkItemStatus::Completed,
            4 => WorkItemStatus::Failed,
            _ => WorkItemStatus::Cancelled,
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: WorkItemStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    // ── SQ-21: is_terminal true for Completed/Failed/Cancelled ──────────────

    #[test]
    fn sq21_is_terminal_correct(idx in 0u8..6) {
        let status = match idx {
            0 => WorkItemStatus::Blocked,
            1 => WorkItemStatus::Ready,
            2 => WorkItemStatus::InProgress,
            3 => WorkItemStatus::Completed,
            4 => WorkItemStatus::Failed,
            _ => WorkItemStatus::Cancelled,
        };
        let expected = matches!(idx, 3 | 4 | 5);
        prop_assert_eq!(status.is_terminal(), expected);
    }

    // ── SQ-22: Assignment serde roundtrip ───────────────────────────────────

    #[test]
    fn sq22_assignment_serde(
        item_id in "[a-z]{2,6}",
        agent in "[a-z]{2,6}",
        ts in 0u64..1_000_000_000,
        attempt in 0u32..10,
    ) {
        let assignment = Assignment {
            work_item_id: item_id,
            agent_slot: agent,
            assigned_at: ts,
            last_heartbeat: ts + 100,
            attempt,
        };
        let json = serde_json::to_string(&assignment).unwrap();
        let back: Assignment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&assignment, &back);
    }

    // ── SQ-23: CompletionRecord serde roundtrip ─────────────────────────────

    #[test]
    fn sq23_completion_record_serde(
        item_id in "[a-z]{2,6}",
        agent in "[a-z]{2,6}",
        success in any::<bool>(),
    ) {
        let rec = CompletionRecord {
            work_item_id: item_id,
            agent_slot: agent,
            completed_at: 12345,
            success,
            message: Some("done".into()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: CompletionRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&rec, &back);
    }

    // ── SQ-24: WorkQueueConfig serde roundtrip ──────────────────────────────

    #[test]
    fn sq24_config_serde(
        max_concurrent in 1u32..10,
        timeout in 1000u64..600_000,
        max_retries in 0u32..5,
        anti_starvation in any::<bool>(),
        threshold in 1000u64..1_000_000,
    ) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: max_concurrent,
            heartbeat_timeout_ms: timeout,
            max_retries,
            anti_starvation,
            starvation_threshold_ms: threshold,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: WorkQueueConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.max_concurrent_per_agent, back.max_concurrent_per_agent);
        prop_assert_eq!(config.heartbeat_timeout_ms, back.heartbeat_timeout_ms);
        prop_assert_eq!(config.max_retries, back.max_retries);
        prop_assert_eq!(config.anti_starvation, back.anti_starvation);
        prop_assert_eq!(config.starvation_threshold_ms, back.starvation_threshold_ms);
    }

    // ── SQ-25: QueueStats serde roundtrip ───────────────────────────────────

    #[test]
    fn sq25_queue_stats_serde(
        total in 0usize..100,
        blocked in 0usize..50,
        ready in 0usize..50,
    ) {
        let stats = QueueStats {
            total_items: total,
            blocked,
            ready,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 0,
            completion_log_size: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: QueueStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.total_items, back.total_items);
        prop_assert_eq!(stats.blocked, back.blocked);
        prop_assert_eq!(stats.ready, back.ready);
    }

    // ── SQ-26: WorkQueueError Display for all 7 variants ────────────────────

    #[test]
    fn sq26_error_display_non_empty(idx in 0u8..7) {
        let err = match idx {
            0 => WorkQueueError::ItemNotFound { id: "x".into() },
            1 => WorkQueueError::DuplicateItem { id: "x".into() },
            2 => WorkQueueError::InvalidState {
                id: "x".into(),
                current: WorkItemStatus::Ready,
                expected: "InProgress",
            },
            3 => WorkQueueError::CycleDetected { ids: vec!["a".into(), "b".into()] },
            4 => WorkQueueError::AgentAtCapacity {
                agent: "ag".into(),
                current: 3,
                max: 3,
            },
            5 => WorkQueueError::DependencyNotFound {
                item: "x".into(),
                dependency: "y".into(),
            },
            _ => WorkQueueError::QueueEmpty,
        };
        let msg = err.to_string();
        prop_assert!(!msg.is_empty());
    }

    // ── SQ-27: cancel transitions Ready→Cancelled ──────────────────────────

    #[test]
    fn sq27_cancel_ready_item(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("cancel-me", prio)).unwrap();
        prop_assert_eq!(q.item_status(&"cancel-me".into()), Some(WorkItemStatus::Ready));
        q.cancel(&"cancel-me".into()).unwrap();
        prop_assert_eq!(q.item_status(&"cancel-me".into()), Some(WorkItemStatus::Cancelled));
    }

    // ── SQ-28: cancel on terminal item fails ────────────────────────────────

    #[test]
    fn sq28_cancel_terminal_fails(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("term", prio)).unwrap();
        q.assign(&"term".into(), &"ag".into()).unwrap();
        q.complete(&"term".into(), &"ag".into(), None).unwrap();
        let result = q.cancel(&"term".into());
        let is_invalid = matches!(result, Err(WorkQueueError::InvalidState { .. }));
        prop_assert!(is_invalid);
    }

    // ── SQ-29: heartbeat updates last_heartbeat ─────────────────────────────

    #[test]
    fn sq29_heartbeat_updates_timestamp(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("hb", prio)).unwrap();
        q.assign(&"hb".into(), &"ag".into()).unwrap();
        let before = q.get_assignment(&"hb".into()).unwrap().last_heartbeat;
        // Small sleep to ensure timestamp advances
        sleep(Duration::from_millis(2));
        q.heartbeat(&"hb".into(), &"ag".into()).unwrap();
        let after = q.get_assignment(&"hb".into()).unwrap().last_heartbeat;
        prop_assert!(after >= before);
    }

    // ── SQ-30: heartbeat from wrong agent fails ─────────────────────────────

    #[test]
    fn sq30_heartbeat_wrong_agent(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("hba", prio)).unwrap();
        q.assign(&"hba".into(), &"agent-a".into()).unwrap();
        let result = q.heartbeat(&"hba".into(), &"agent-b".into());
        prop_assert!(result.is_err());
    }

    // ── SQ-31: item_status returns None for unknown ID ──────────────────────

    #[test]
    fn sq31_unknown_status_none(id in "[a-z]{5,10}") {
        let q = SwarmWorkQueue::with_defaults();
        prop_assert_eq!(q.item_status(&id), None);
    }

    // ── SQ-32: get_item returns correct WorkItem ────────────────────────────

    #[test]
    fn sq32_get_item_matches(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        let item = make_item("get-me", prio);
        q.enqueue(item.clone()).unwrap();
        let got = q.get_item(&"get-me".into());
        prop_assert!(got.is_some());
        prop_assert_eq!(got.unwrap().priority, prio);
    }

    // ── SQ-33: ready_items returns only Ready status items ──────────────────

    #[test]
    fn sq33_ready_items_only_ready(n in 1usize..5) {
        let mut q = SwarmWorkQueue::with_defaults();
        for i in 0..n {
            q.enqueue(make_item(&format!("r-{i}"), i as u32)).unwrap();
        }
        // Assign first item -> no longer in ready_items
        if n > 1 {
            q.assign(&"r-0".into(), &"ag".into()).unwrap();
            let ready = q.ready_items();
            prop_assert_eq!(ready.len(), n - 1);
            for item in &ready {
                let check = q.item_status(&item.id) == Some(WorkItemStatus::Ready);
                prop_assert!(check);
            }
        }
    }

    // ── SQ-34: agent_items returns only assigned items for that agent ───────

    #[test]
    fn sq34_agent_items_scoped(prio_a in priority(), prio_b in priority()) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: 5,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(make_item("a-1", prio_a)).unwrap();
        q.enqueue(make_item("b-1", prio_b)).unwrap();
        q.assign(&"a-1".into(), &"alice".into()).unwrap();
        q.assign(&"b-1".into(), &"bob".into()).unwrap();
        let alice_items = q.agent_items(&"alice".into());
        prop_assert_eq!(alice_items.len(), 1);
        prop_assert_eq!(&alice_items[0].work_item_id, "a-1");
    }

    // ── SQ-35: sequence increments on state transitions ─────────────────────

    #[test]
    fn sq35_sequence_increments(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        let s0 = q.sequence();
        q.enqueue(make_item("seq", prio)).unwrap();
        let s1 = q.sequence();
        prop_assert!(s1 > s0);
        q.assign(&"seq".into(), &"ag".into()).unwrap();
        let s2 = q.sequence();
        prop_assert!(s2 > s1);
        q.complete(&"seq".into(), &"ag".into(), None).unwrap();
        let s3 = q.sequence();
        prop_assert!(s3 > s2);
    }

    // ── SQ-36: completion_log grows on complete/fail ────────────────────────

    #[test]
    fn sq36_completion_log_grows(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("cl", prio)).unwrap();
        prop_assert_eq!(q.completion_log().len(), 0);
        q.assign(&"cl".into(), &"ag".into()).unwrap();
        q.complete(&"cl".into(), &"ag".into(), Some("msg".into())).unwrap();
        prop_assert_eq!(q.completion_log().len(), 1);
        let rec = &q.completion_log()[0];
        prop_assert!(rec.success);
        prop_assert_eq!(&rec.work_item_id, "cl");
    }

    // ── SQ-37: fail with retries returns item to Ready ──────────────────────

    #[test]
    fn sq37_fail_with_retries(prio in priority()) {
        let config = WorkQueueConfig {
            max_retries: 2,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(make_item("retry", prio)).unwrap();
        // First failure → back to Ready (attempt 1 < max_retries 2)
        q.assign(&"retry".into(), &"ag".into()).unwrap();
        q.fail(&"retry".into(), &"ag".into(), None).unwrap();
        prop_assert_eq!(q.item_status(&"retry".into()), Some(WorkItemStatus::Ready));
    }

    // ── SQ-38: BeadRecord serde roundtrip ───────────────────────────────────

    #[test]
    fn sq38_bead_record_serde(
        id in "[a-z]{3,8}",
        title in "[a-zA-Z ]{5,20}",
        status in prop_oneof![
            Just("open".to_string()),
            Just("closed".to_string()),
        ],
    ) {
        let record = BeadRecord {
            id: id.clone(),
            title,
            description: String::new(),
            status,
            priority: 2,
            issue_type: "task".into(),
            assignee: String::new(),
            created_at: String::new(),
            created_by: String::new(),
            updated_at: String::new(),
            closed_at: String::new(),
            close_reason: String::new(),
            labels: vec![],
            dependencies: vec![],
            acceptance_criteria: String::new(),
            notes: String::new(),
            source_repo: String::new(),
            compaction_level: 0,
            original_size: 0,
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: BeadRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&record.id, &back.id);
        prop_assert_eq!(&record.status, &back.status);
    }

    // ── SQ-39: BeadRecord is_actionable / is_terminal ───────────────────────

    #[test]
    fn sq39_bead_actionable_terminal(status in prop_oneof![
        Just("open"),
        Just("in_progress"),
        Just("closed"),
        Just("cancelled"),
        Just("resolved"),
        Just("wontfix"),
        Just("draft"),
    ]) {
        let record = BeadRecord {
            id: "test".into(),
            title: "t".into(),
            description: String::new(),
            status: status.to_string(),
            priority: 1,
            issue_type: String::new(),
            assignee: String::new(),
            created_at: String::new(),
            created_by: String::new(),
            updated_at: String::new(),
            closed_at: String::new(),
            close_reason: String::new(),
            labels: vec![],
            dependencies: vec![],
            acceptance_criteria: String::new(),
            notes: String::new(),
            source_repo: String::new(),
            compaction_level: 0,
            original_size: 0,
        };
        let actionable = record.is_actionable();
        let terminal = record.is_terminal();
        // Actionable and terminal should be mutually exclusive
        let both = actionable && terminal;
        prop_assert!(!both, "cannot be both actionable and terminal");
        // Known statuses have defined behavior
        match status {
            "open" | "in_progress" => prop_assert!(actionable),
            "closed" | "cancelled" | "resolved" | "wontfix" => prop_assert!(terminal),
            _ => {} // other statuses are neither
        }
    }

    // ── SQ-40: BeadsSyncReport serde roundtrip ──────────────────────────────

    #[test]
    fn sq40_sync_report_serde(
        imported in 0u32..100,
        updated in 0u32..50,
        skipped in 0u32..50,
    ) {
        let report = BeadsSyncReport {
            imported,
            updated,
            skipped,
            orphan_deps: vec!["dep-1".into()],
            completed_from_bead: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: BeadsSyncReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&report, &back);
    }

    // ── SQ-41: BeadsImportError Display non-empty ───────────────────────────

    #[test]
    fn sq41_import_error_display(line in 1usize..100) {
        let err = BeadsImportError::ParseError {
            line,
            message: "bad json".into(),
        };
        let msg = err.to_string();
        prop_assert!(!msg.is_empty());
        prop_assert!(msg.contains(&line.to_string()));

        let io_err = BeadsImportError::IoError {
            path: "/tmp/test".into(),
            message: "not found".into(),
        };
        prop_assert!(!io_err.to_string().is_empty());
    }

    // ── SQ-42: would_create_cycle returns false for valid deps ──────────────

    #[test]
    fn sq42_no_cycle_valid_deps(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("root", prio)).unwrap();
        // Adding "child" depending on "root" should NOT be a cycle
        let has_cycle = q.would_create_cycle(&"child".to_string(), &["root".to_string()]);
        prop_assert!(!has_cycle);
    }

    // ── SQ-43: QueueSnapshot serde roundtrip ────────────────────────────────

    #[test]
    fn sq43_queue_snapshot_serde(n in 1usize..4) {
        let mut q = SwarmWorkQueue::with_defaults();
        for i in 0..n {
            q.enqueue(make_item(&format!("sn-{i}"), i as u32)).unwrap();
        }
        let snap = q.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: QueueSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.items.len(), back.items.len());
        prop_assert_eq!(snap.sequence, back.sequence);
        prop_assert_eq!(snap.status.len(), back.status.len());
    }

    // ── SQ-44: BeadRecord.to_work_item preserves id/title/priority ─────────

    #[test]
    fn sq44_to_work_item_preserves_fields(
        id in "[a-z]{3,8}",
        prio in 0u32..10,
    ) {
        let record = BeadRecord {
            id: id.clone(),
            title: format!("title-{id}"),
            description: String::new(),
            status: "open".into(),
            priority: prio,
            issue_type: "task".into(),
            assignee: String::new(),
            created_at: String::new(),
            created_by: String::new(),
            updated_at: String::new(),
            closed_at: String::new(),
            close_reason: String::new(),
            labels: vec!["label-a".into()],
            dependencies: vec![],
            acceptance_criteria: String::new(),
            notes: String::new(),
            source_repo: String::new(),
            compaction_level: 0,
            original_size: 0,
        };
        let item = record.to_work_item();
        prop_assert_eq!(&item.id, &id);
        prop_assert_eq!(item.priority, prio);
        prop_assert_eq!(item.labels, vec!["label-a".to_string()]);
        // task effort = 3
        prop_assert_eq!(item.effort, 3);
    }

    // ── SQ-45: fail exhausting retries transitions to Failed ────────────────

    #[test]
    fn sq45_fail_exhausts_retries(prio in priority()) {
        let config = WorkQueueConfig {
            max_retries: 2,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(make_item("exhaust", prio)).unwrap();

        // First fail (attempt_count=1 < max_retries=2) → back to Ready
        q.assign(&"exhaust".into(), &"ag".into()).unwrap();
        q.fail(&"exhaust".into(), &"ag".into(), None).unwrap();
        prop_assert_eq!(q.item_status(&"exhaust".into()), Some(WorkItemStatus::Ready));

        // Second fail (attempt_count=2, not < 2) → Failed
        q.assign(&"exhaust".into(), &"ag".into()).unwrap();
        q.fail(&"exhaust".into(), &"ag".into(), None).unwrap();
        prop_assert_eq!(q.item_status(&"exhaust".into()), Some(WorkItemStatus::Failed));
    }

    // ── SQ-46: agent capacity enforcement ───────────────────────────────────

    #[test]
    fn sq46_agent_capacity_enforced(
        max_concurrent in 1u32..4,
    ) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: max_concurrent,
            anti_starvation: false,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);

        for i in 0..(max_concurrent + 1) {
            q.enqueue(make_item(&format!("cap-{i}"), 0)).unwrap();
        }

        // Assign up to capacity
        for i in 0..max_concurrent {
            q.assign(&format!("cap-{i}"), &"ag".into()).unwrap();
        }

        // One more should fail
        let result = q.assign(&format!("cap-{max_concurrent}"), &"ag".into());
        let is_at_cap = matches!(result, Err(WorkQueueError::AgentAtCapacity { .. }));
        prop_assert!(is_at_cap);
    }

    // ── SQ-47: stats.cancelled tracks cancel operations ─────────────────────

    #[test]
    fn sq47_stats_cancelled_count(n in 1usize..5) {
        let mut q = SwarmWorkQueue::with_defaults();
        for i in 0..n {
            q.enqueue(make_item(&format!("cx-{i}"), i as u32)).unwrap();
        }
        for i in 0..n {
            q.cancel(&format!("cx-{i}")).unwrap();
        }
        let stats = q.stats();
        prop_assert_eq!(stats.cancelled, n);
        prop_assert_eq!(stats.ready, 0);
    }

    // ── SQ-48: BeadsImportError serde roundtrip ─────────────────────────────

    #[test]
    fn sq48_import_error_serde(line in 1usize..200) {
        let err = BeadsImportError::ParseError {
            line,
            message: "test".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: BeadsImportError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&err, &back);
    }

    // ── SQ-49: complete from wrong agent fails ──────────────────────────────

    #[test]
    fn sq49_complete_wrong_agent(prio in priority()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item("wa", prio)).unwrap();
        q.assign(&"wa".into(), &"agent-a".into()).unwrap();
        let result = q.complete(&"wa".into(), &"agent-b".into(), None);
        prop_assert!(result.is_err());
    }

    // ── SQ-50: pull on empty queue returns QueueEmpty ────────────────────────

    #[test]
    fn sq50_pull_empty_queue(_dummy in 0u8..1) {
        let mut q = SwarmWorkQueue::with_defaults();
        let result = q.pull(&"ag".into());
        let is_empty = matches!(result, Err(WorkQueueError::QueueEmpty));
        prop_assert!(is_empty);
    }

    // ── SQ-51: cancel InProgress releases agent load ────────────────────────

    #[test]
    fn sq51_cancel_in_progress_releases_agent(prio in priority()) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: 1,
            anti_starvation: false,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(make_item("ip-1", prio)).unwrap();
        q.enqueue(make_item("ip-2", prio)).unwrap();
        q.assign(&"ip-1".into(), &"ag".into()).unwrap();
        // Agent at capacity
        let cap_err = q.assign(&"ip-2".into(), &"ag".into());
        let at_cap = matches!(cap_err, Err(WorkQueueError::AgentAtCapacity { .. }));
        prop_assert!(at_cap);
        // Cancel releases the slot
        q.cancel(&"ip-1".into()).unwrap();
        let result = q.assign(&"ip-2".into(), &"ag".into());
        prop_assert!(result.is_ok());
    }
}

#[test]
fn anti_starvation_boosts_long_waiting_item_over_newer_higher_priority_item() {
    let mut q = SwarmWorkQueue::new(WorkQueueConfig {
        anti_starvation: true,
        starvation_threshold_ms: 50,
        ..Default::default()
    });

    // Lower priority value means more urgent; this older item should still win once starved.
    q.enqueue(make_item("older-low-priority", 9)).unwrap();
    sleep(Duration::from_millis(80));
    q.enqueue(make_item("newer-high-priority", 0)).unwrap();

    let assignment = q.pull(&"agent".into()).unwrap();
    assert_eq!(
        assignment.work_item_id, "older-low-priority",
        "starved item should be selected before newer non-starved item"
    );
}
