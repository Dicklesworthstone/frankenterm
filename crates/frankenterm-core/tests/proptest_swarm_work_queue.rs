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
    BeadsImporter, QueueSnapshot, SwarmWorkQueue, WorkItem, WorkItemStatus, WorkQueueConfig,
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
