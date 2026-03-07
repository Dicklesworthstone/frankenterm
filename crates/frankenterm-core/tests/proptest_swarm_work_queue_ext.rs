//! Extended property-based tests for swarm_work_queue module.
//!
//! Supplements proptest_swarm_work_queue.rs with coverage for:
//! - Agent capacity limits
//! - Completion cascading unblocks
//! - Failure/retry lifecycle
//! - Cancel behavior
//! - QueueStats consistency after mixed operations
//! - Assignment tracking and heartbeat
//! - WorkItemStatus::is_terminal consistency
//! - WorkItem serde roundtrip
//! - CompletionRecord append-only property
//! - Sequence monotonicity

use frankenterm_core::swarm_work_queue::*;
use proptest::prelude::*;

// =============================================================================
// Helpers
// =============================================================================

fn make_item(id: &str, prio: u32) -> WorkItem {
    WorkItem {
        id: id.to_string(),
        title: format!("Task {id}"),
        priority: prio,
        depends_on: Vec::new(),
        effort: 1,
        labels: Vec::new(),
        preferred_program: None,
        metadata: Default::default(),
    }
}

fn make_dep_item(id: &str, prio: u32, deps: Vec<&str>) -> WorkItem {
    WorkItem {
        id: id.to_string(),
        title: format!("Task {id}"),
        priority: prio,
        depends_on: deps.into_iter().map(String::from).collect(),
        effort: 1,
        labels: Vec::new(),
        preferred_program: None,
        metadata: Default::default(),
    }
}

fn work_item_id() -> impl Strategy<Value = String> {
    "[a-z]{2,8}"
}

// =============================================================================
// Agent Capacity Properties
// =============================================================================

proptest! {
    /// Agent cannot exceed max_concurrent_per_agent
    #[test]
    fn agent_capacity_enforced(max_concurrent in 1u32..5) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: max_concurrent,
            ..WorkQueueConfig::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        let agent = "Agent1".to_string();

        // Enqueue more items than the limit
        for i in 0..(max_concurrent + 2) {
            q.enqueue(make_item(&format!("item{i}"), 1)).unwrap();
        }

        // Pull items until capacity hit
        let mut assigned = 0u32;
        loop {
            match q.pull(&agent) {
                Ok(_) => assigned += 1,
                Err(WorkQueueError::AgentAtCapacity { .. }) => break,
                Err(WorkQueueError::QueueEmpty) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        prop_assert_eq!(assigned, max_concurrent,
            "should assign exactly max_concurrent={} items, got {}", max_concurrent, assigned);
    }

    /// Multiple agents can each pull up to max_concurrent
    #[test]
    fn multiple_agents_independent_capacity(
        n_agents in 2usize..4,
        max_concurrent in 1u32..3,
    ) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: max_concurrent,
            ..WorkQueueConfig::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        let total_items = n_agents as u32 * max_concurrent + 2;

        for i in 0..total_items {
            q.enqueue(make_item(&format!("item{i}"), 1)).unwrap();
        }

        let mut total_assigned = 0u32;
        for a in 0..n_agents {
            let agent = format!("Agent{a}");
            for _ in 0..max_concurrent {
                if q.pull(&agent).is_ok() {
                    total_assigned += 1;
                }
            }
        }
        prop_assert_eq!(total_assigned, n_agents as u32 * max_concurrent);
    }
}

// =============================================================================
// Completion Cascading Properties
// =============================================================================

proptest! {
    /// Completing a dependency unblocks dependents
    #[test]
    fn completion_unblocks_dependents(chain_len in 2usize..6) {
        let mut q = SwarmWorkQueue::with_defaults();

        // Build a linear chain: item0 → item1 → item2 → ...
        q.enqueue(make_item("item0", 1)).unwrap();
        for i in 1..chain_len {
            q.enqueue(make_dep_item(
                &format!("item{i}"),
                1,
                vec![&format!("item{}", i - 1)],
            )).unwrap();
        }

        // Only item0 should be ready
        prop_assert_eq!(q.ready_items().len(), 1);
        prop_assert_eq!(&q.ready_items()[0].id, "item0");

        // Assign and complete the chain
        let agent = "worker".to_string();
        for i in 0..chain_len {
            let item_id = format!("item{i}");
            let assignment = q.pull(&agent).unwrap();
            prop_assert_eq!(&assignment.work_item_id, &item_id);
            q.complete(&item_id, &agent, None).unwrap();

            // After completing item i, item i+1 should be ready (if it exists)
            if i + 1 < chain_len {
                let next_id = format!("item{}", i + 1);
                let status = q.item_status(&next_id).unwrap();
                prop_assert_eq!(status, WorkItemStatus::Ready,
                    "item{} should be Ready after item{} completes, got {:?}", i + 1, i, status);
            }
        }
    }

    /// Fan-in: item with multiple deps only unblocks when ALL complete
    #[test]
    fn fanin_all_deps_required(n_deps in 2usize..5) {
        let mut q = SwarmWorkQueue::with_defaults();
        let agent = "worker".to_string();

        // Create n_deps independent items
        let dep_ids: Vec<String> = (0..n_deps).map(|i| format!("dep{i}")).collect();
        for id in &dep_ids {
            q.enqueue(make_item(id, 1)).unwrap();
        }

        // Create the fan-in item depending on all of them
        let dep_refs: Vec<&str> = dep_ids.iter().map(|s| s.as_str()).collect();
        q.enqueue(make_dep_item("fanin", 1, dep_refs)).unwrap();

        // fanin should be Blocked
        prop_assert_eq!(q.item_status(&"fanin".to_string()).unwrap(), WorkItemStatus::Blocked);

        // Complete all but the last dependency
        for dep_id in dep_ids.iter().take(n_deps - 1) {
            q.assign(dep_id, &agent).unwrap();
            q.complete(dep_id, &agent, None).unwrap();
            // fanin should still be Blocked
            prop_assert_eq!(q.item_status(&"fanin".to_string()).unwrap(), WorkItemStatus::Blocked,
                "fanin should stay Blocked until all {} deps complete", n_deps);
        }

        // Complete the last dependency
        let last = &dep_ids[n_deps - 1];
        q.assign(last, &agent).unwrap();
        q.complete(last, &agent, None).unwrap();
        // NOW fanin should be Ready
        prop_assert_eq!(q.item_status(&"fanin".to_string()).unwrap(), WorkItemStatus::Ready);
    }
}

// =============================================================================
// Failure/Retry Properties
// =============================================================================

proptest! {
    /// Failing an item returns it to Ready for retry (within max_retries)
    #[test]
    fn fail_returns_to_ready(max_retries in 1u32..4) {
        let config = WorkQueueConfig {
            max_retries,
            ..WorkQueueConfig::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        let agent = "worker".to_string();

        q.enqueue(make_item("retry-me", 1)).unwrap();

        // Fail the item multiple times
        for attempt in 0..max_retries {
            let a = q.pull(&agent).unwrap();
            prop_assert_eq!(a.work_item_id, "retry-me");
            q.fail(&"retry-me".to_string(), &agent, Some("oops".to_string())).unwrap();

            if attempt + 1 < max_retries {
                // Should go back to Ready for retry
                let status = q.item_status(&"retry-me".to_string()).unwrap();
                prop_assert_eq!(status, WorkItemStatus::Ready,
                    "attempt {}: should return to Ready", attempt + 1);
            }
        }
    }

    /// After max_retries failures, item becomes Failed (terminal)
    #[test]
    fn fail_beyond_max_retries_is_terminal(max_retries in 1u32..4) {
        let config = WorkQueueConfig {
            max_retries,
            ..WorkQueueConfig::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        let agent = "worker".to_string();

        q.enqueue(make_item("doomed", 1)).unwrap();

        for _ in 0..max_retries {
            q.pull(&agent).unwrap();
            q.fail(&"doomed".to_string(), &agent, None).unwrap();
        }

        let status = q.item_status(&"doomed".to_string()).unwrap();
        prop_assert!(status.is_terminal(),
            "after {} retries, status should be terminal, got {:?}", max_retries, status);
    }
}

// =============================================================================
// Cancel Properties
// =============================================================================

proptest! {
    /// Cancel on a Ready item makes it Cancelled
    #[test]
    fn cancel_ready_item(id in work_item_id()) {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(make_item(&id, 1)).unwrap();
        q.cancel(&id).unwrap();
        let status = q.item_status(&id).unwrap();
        prop_assert_eq!(status, WorkItemStatus::Cancelled);
        prop_assert!(status.is_terminal());
    }

    /// Cancel on a non-existent item returns error
    #[test]
    fn cancel_missing_item_errors(id in work_item_id()) {
        let mut q = SwarmWorkQueue::with_defaults();
        let result = q.cancel(&id);
        prop_assert!(result.is_err());
    }
}

// =============================================================================
// QueueStats Consistency
// =============================================================================

proptest! {
    /// Stats counts sum to total_items
    #[test]
    fn stats_sum_to_total(
        n_items in 2usize..10,
        n_complete in 0usize..3,
    ) {
        let mut q = SwarmWorkQueue::with_defaults();
        let agent = "worker".to_string();

        for i in 0..n_items {
            q.enqueue(make_item(&format!("i{i}"), 1)).unwrap();
        }

        // Complete some items
        let to_complete = n_complete.min(n_items);
        for i in 0..to_complete {
            let id = format!("i{i}");
            q.assign(&id, &agent).unwrap();
            q.complete(&id, &agent, None).unwrap();
        }

        let stats = q.stats();
        let sum = stats.blocked + stats.ready + stats.in_progress
            + stats.completed + stats.failed + stats.cancelled;
        prop_assert_eq!(sum, stats.total_items,
            "stats sum {} != total_items {}", sum, stats.total_items);
    }

    /// Stats total_items matches enqueued count
    #[test]
    fn stats_total_matches_enqueued(n in 1usize..20) {
        let mut q = SwarmWorkQueue::with_defaults();
        for i in 0..n {
            q.enqueue(make_item(&format!("t{i}"), 1)).unwrap();
        }
        prop_assert_eq!(q.stats().total_items, n);
    }
}

// =============================================================================
// WorkItemStatus Properties
// =============================================================================

proptest! {
    /// is_terminal only for Completed, Failed, Cancelled
    #[test]
    fn status_is_terminal_correct(idx in 0u8..6) {
        let status = match idx {
            0 => WorkItemStatus::Blocked,
            1 => WorkItemStatus::Ready,
            2 => WorkItemStatus::InProgress,
            3 => WorkItemStatus::Completed,
            4 => WorkItemStatus::Failed,
            _ => WorkItemStatus::Cancelled,
        };
        let expected_terminal = matches!(status,
            WorkItemStatus::Completed | WorkItemStatus::Failed | WorkItemStatus::Cancelled);
        prop_assert_eq!(status.is_terminal(), expected_terminal,
            "{:?}.is_terminal() should be {}", status, expected_terminal);
    }
}

// =============================================================================
// Serde Roundtrip Properties
// =============================================================================

proptest! {
    /// WorkItem serde roundtrip
    #[test]
    fn work_item_serde_roundtrip(
        id in work_item_id(),
        prio in 0u32..100,
        effort in 1u32..50,
    ) {
        let item = WorkItem {
            id: id.clone(),
            title: format!("Task {id}"),
            priority: prio,
            depends_on: Vec::new(),
            effort,
            labels: vec!["test".to_string()],
            preferred_program: None,
            metadata: Default::default(),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: WorkItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(item, back);
    }

    /// WorkQueueConfig serde roundtrip
    #[test]
    fn config_serde_roundtrip(
        max_concurrent in 1u32..10,
        heartbeat_ms in 1000u64..600_000,
        max_retries in 0u32..10,
    ) {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: max_concurrent,
            heartbeat_timeout_ms: heartbeat_ms,
            max_retries,
            anti_starvation: true,
            starvation_threshold_ms: 600_000,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: WorkQueueConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.max_concurrent_per_agent, back.max_concurrent_per_agent);
        prop_assert_eq!(config.heartbeat_timeout_ms, back.heartbeat_timeout_ms);
        prop_assert_eq!(config.max_retries, back.max_retries);
    }

    /// WorkItemStatus serde roundtrip
    #[test]
    fn status_serde_roundtrip(idx in 0u8..6) {
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
}

// =============================================================================
// Sequence Monotonicity
// =============================================================================

proptest! {
    /// Sequence number always increases with mutations
    #[test]
    fn sequence_monotonic(n_items in 2usize..8) {
        let mut q = SwarmWorkQueue::with_defaults();
        let agent = "worker".to_string();
        let mut prev_seq = q.sequence();

        for i in 0..n_items {
            q.enqueue(make_item(&format!("s{i}"), 1)).unwrap();
            let seq = q.sequence();
            prop_assert!(seq > prev_seq, "sequence should increase on enqueue");
            prev_seq = seq;
        }

        // Assign and complete first item
        q.assign(&"s0".to_string(), &agent).unwrap();
        let seq_after_assign = q.sequence();
        prop_assert!(seq_after_assign > prev_seq);

        q.complete(&"s0".to_string(), &agent, None).unwrap();
        let seq_after_complete = q.sequence();
        prop_assert!(seq_after_complete > seq_after_assign);
    }
}

// =============================================================================
// Completion Log Properties
// =============================================================================

proptest! {
    /// Completion log grows monotonically and records are accurate
    #[test]
    fn completion_log_append_only(n_items in 1usize..6) {
        let mut q = SwarmWorkQueue::with_defaults();
        let agent = "worker".to_string();

        for i in 0..n_items {
            q.enqueue(make_item(&format!("cl{i}"), 1)).unwrap();
        }

        for i in 0..n_items {
            let id = format!("cl{i}");
            q.assign(&id, &agent).unwrap();
            let success = i % 2 == 0; // alternate success/fail
            if success {
                q.complete(&id, &agent, None).unwrap();
            } else {
                q.fail(&id, &agent, Some("nope".to_string())).unwrap();
            }
        }

        let log = q.completion_log();
        prop_assert_eq!(log.len(), n_items);

        for (i, record) in log.iter().enumerate() {
            prop_assert_eq!(&record.work_item_id, &format!("cl{i}"));
            prop_assert_eq!(&record.agent_slot, "worker");
            prop_assert_eq!(record.success, i % 2 == 0);
        }
    }
}

// =============================================================================
// Ready Items Properties
// =============================================================================

proptest! {
    /// ready_items only returns items with Ready status
    #[test]
    fn ready_items_all_actually_ready(n in 1usize..10) {
        let mut q = SwarmWorkQueue::with_defaults();
        for i in 0..n {
            q.enqueue(make_item(&format!("r{i}"), i as u32)).unwrap();
        }

        // Assign some items to make them InProgress
        let agent = "worker".to_string();
        if n > 1 {
            q.assign(&"r0".to_string(), &agent).unwrap();
        }

        for item in q.ready_items() {
            let status = q.item_status(&item.id).unwrap();
            prop_assert_eq!(status, WorkItemStatus::Ready,
                "ready_items() returned item {} with status {:?}", item.id, status);
        }
    }
}
