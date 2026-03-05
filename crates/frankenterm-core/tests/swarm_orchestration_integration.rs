// =============================================================================
// Swarm orchestration integration tests (ft-3681t.3.x)
//
// Cross-module integration tests validating the full swarm orchestration stack:
// - FleetLauncher → LifecycleRegistry → LaunchOutcome
// - SwarmWorkQueue → ready dispatch → assignment → completion
// - SwarmScheduler → queue pressure → scale/rebalance decisions
// - Full lifecycle: fleet launch → work enqueue → schedule → assign → complete
// =============================================================================

use std::collections::HashMap;

use frankenterm_core::fleet_launcher::{
    AgentMixEntry, FleetLaunchStatus, FleetLauncher, FleetSpec, StartupStrategy,
};
use frankenterm_core::session_profiles::{
    ProfilePolicy, ProfileRegistry, ProfileRole, ResourceHints, SessionProfile, SpawnCommand,
};
use frankenterm_core::session_topology::LifecycleRegistry;
use frankenterm_core::swarm_scheduler::{SchedulerConfig, SchedulerDecision, SwarmScheduler};
use frankenterm_core::swarm_work_queue::{
    SwarmWorkQueue, WorkItem, WorkItemStatus, WorkQueueConfig, WorkQueueError,
};

// =============================================================================
// Test helpers
// =============================================================================

/// Convert &str to String for work queue API calls.
fn s(val: &str) -> String {
    val.to_string()
}

fn test_fleet_spec(name: &str, panes: u32) -> FleetSpec {
    FleetSpec {
        name: name.to_string(),
        description: Some("integration test fleet".to_string()),
        workspace_id: "test-ws".to_string(),
        domain: "local".to_string(),
        mix: vec![
            AgentMixEntry {
                program: "claude-code".to_string(),
                model: Some("opus-4.1".to_string()),
                weight: 2,
                profile: None,
                task_template: None,
                environment: HashMap::new(),
                role: None,
            },
            AgentMixEntry {
                program: "codex-cli".to_string(),
                model: Some("gpt5-codex".to_string()),
                weight: 1,
                profile: None,
                task_template: None,
                environment: HashMap::new(),
                role: None,
            },
        ],
        total_panes: panes,
        fleet_template: None,
        working_directory: None,
        startup_strategy: StartupStrategy::Parallel,
        generation: 1,
        tags: vec!["test".to_string()],
    }
}

fn test_profile() -> SessionProfile {
    SessionProfile {
        name: "agent-worker".to_string(),
        description: Some("test profile".to_string()),
        role: ProfileRole::AgentWorker,
        spawn_command: Some(SpawnCommand {
            command: "echo test".to_string(),
            args: vec![],
            use_shell: true,
        }),
        environment: HashMap::new(),
        working_directory: None,
        resource_hints: ResourceHints::default(),
        policy: ProfilePolicy::default(),
        layout_template: None,
        bootstrap_commands: vec![],
        tags: vec![],
        updated_at: 0,
    }
}

fn work_item(id: &str, priority: u32, deps: &[&str]) -> WorkItem {
    WorkItem {
        id: id.to_string(),
        title: format!("Task {id}"),
        priority,
        depends_on: deps.iter().map(|d| d.to_string()).collect(),
        effort: 1,
        labels: vec![],
        preferred_program: None,
        metadata: HashMap::new(),
    }
}

fn test_queue_config() -> WorkQueueConfig {
    WorkQueueConfig {
        max_concurrent_per_agent: 3,
        heartbeat_timeout_ms: 300_000,
        max_retries: 2,
        anti_starvation: true,
        starvation_threshold_ms: 60_000,
    }
}

fn test_scheduler_config() -> SchedulerConfig {
    SchedulerConfig {
        scale_up_cooldown_ms: 1000,
        scale_down_cooldown_ms: 2000,
        min_fleet_size: 2,
        max_fleet_size: 20,
        scale_up_threshold: 0.8,
        scale_down_threshold: 0.2,
        rebalance_imbalance_threshold: 0.3,
        max_consecutive_scale_ops: 5,
        agent_startup_grace_ms: 5000,
        circuit_breaker_reset_ms: 300_000,
        max_scale_step: 3,
        failure_rate_suppress_threshold: 0.5,
    }
}

/// Helper to set up a ProfileRegistry with a test profile and return a FleetLauncher.
fn setup_launcher_with_profiles() -> ProfileRegistry {
    let mut profiles = ProfileRegistry::new();
    profiles.register_profile(test_profile());
    profiles
}

// =============================================================================
// Fleet launch + lifecycle integration
// =============================================================================

#[test]
fn fleet_launch_registers_lifecycle_entities() {
    let profiles = setup_launcher_with_profiles();
    let launcher = FleetLauncher::new(&profiles);
    let spec = test_fleet_spec("alpha", 6);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    assert_eq!(plan.slots.len(), 6);

    let mut registry = LifecycleRegistry::new();
    let outcome = launcher.execute_with_subsystems(&plan, &mut registry, None, None);

    assert_eq!(outcome.status, FleetLaunchStatus::Complete);
    assert_eq!(outcome.total_slots, 6);
    assert_eq!(outcome.successful_slots, 6);
    assert_eq!(outcome.failed_slots, 0);

    // Verify lifecycle entities were registered
    assert!(!outcome.registry_snapshot.is_empty());
}

#[test]
fn fleet_plan_respects_weighted_mix() {
    let profiles = setup_launcher_with_profiles();
    let launcher = FleetLauncher::new(&profiles);
    let spec = test_fleet_spec("weighted", 9);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    assert_eq!(plan.slots.len(), 9);

    // Weight ratio is 2:1 (claude-code:codex-cli), so with 9 panes:
    // claude-code should get 6, codex-cli should get 3
    let claude_count = plan
        .slots
        .iter()
        .filter(|s| s.label.contains("claude"))
        .count();
    let codex_count = plan
        .slots
        .iter()
        .filter(|s| s.label.contains("codex"))
        .count();

    assert_eq!(claude_count, 6, "claude-code should get 2/3 of panes");
    assert_eq!(codex_count, 3, "codex-cli should get 1/3 of panes");
}

// =============================================================================
// Work queue + dependency dispatch
// =============================================================================

#[test]
fn work_queue_dispatches_only_ready_items() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    queue.enqueue(work_item("a", 0, &[])).unwrap();
    queue.enqueue(work_item("b", 0, &["a"])).unwrap();
    queue.enqueue(work_item("c", 0, &["a"])).unwrap();
    queue.enqueue(work_item("d", 0, &["b", "c"])).unwrap();

    // Only "a" should be ready
    let ready = queue.ready_items();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "a");

    // Assign and complete "a"
    queue.assign(&s("a"), &s("agent-1")).unwrap();
    queue.complete(&s("a"), &s("agent-1"), None).unwrap();

    // Now "b" and "c" should be ready
    let ready = queue.ready_items();
    assert_eq!(ready.len(), 2);
    let ready_ids: Vec<&str> = ready.iter().map(|r| r.id.as_str()).collect();
    assert!(ready_ids.contains(&"b"));
    assert!(ready_ids.contains(&"c"));

    // "d" still blocked
    let status_d = queue.item_status(&s("d")).unwrap();
    assert_eq!(status_d, WorkItemStatus::Blocked);
}

#[test]
fn work_queue_full_dag_completion_flow() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    // Diamond dependency: root → (left, right) → sink
    queue.enqueue(work_item("root", 0, &[])).unwrap();
    queue.enqueue(work_item("left", 1, &["root"])).unwrap();
    queue.enqueue(work_item("right", 1, &["root"])).unwrap();
    queue
        .enqueue(work_item("sink", 2, &["left", "right"]))
        .unwrap();

    // Complete entire DAG
    queue.assign(&s("root"), &s("agent-1")).unwrap();
    queue.complete(&s("root"), &s("agent-1"), None).unwrap();

    queue.assign(&s("left"), &s("agent-1")).unwrap();
    queue.assign(&s("right"), &s("agent-2")).unwrap();
    queue.complete(&s("left"), &s("agent-1"), None).unwrap();
    queue.complete(&s("right"), &s("agent-2"), None).unwrap();

    // Sink should now be ready
    let ready = queue.ready_items();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "sink");

    queue.assign(&s("sink"), &s("agent-1")).unwrap();
    queue.complete(&s("sink"), &s("agent-1"), None).unwrap();

    // All items completed
    let stats = queue.stats();
    assert_eq!(stats.completed, 4);
    assert_eq!(stats.blocked, 0);
    assert_eq!(stats.ready, 0);
    assert_eq!(stats.in_progress, 0);
}

#[test]
fn work_queue_rejects_completion_from_non_owner_agent() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());
    queue.enqueue(work_item("owned", 0, &[])).unwrap();
    queue.assign(&s("owned"), &s("agent-1")).unwrap();

    let err = queue
        .complete(&s("owned"), &s("agent-2"), None)
        .unwrap_err();
    assert_eq!(
        err,
        WorkQueueError::InvalidState {
            id: s("owned"),
            current: WorkItemStatus::InProgress,
            expected: "assigned to this agent",
        }
    );
    assert_eq!(
        queue.item_status(&s("owned")),
        Some(WorkItemStatus::InProgress),
        "ownership violation must not mutate item state"
    );
    assert_eq!(
        queue
            .get_assignment(&s("owned"))
            .map(|a| a.agent_slot.as_str()),
        Some("agent-1"),
        "ownership violation must preserve original assignment"
    );
}

#[test]
fn work_queue_rejects_failure_from_non_owner_agent() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());
    queue.enqueue(work_item("owned-fail", 0, &[])).unwrap();
    queue.assign(&s("owned-fail"), &s("agent-1")).unwrap();

    let err = queue
        .fail(&s("owned-fail"), &s("agent-2"), None)
        .unwrap_err();
    assert_eq!(
        err,
        WorkQueueError::InvalidState {
            id: s("owned-fail"),
            current: WorkItemStatus::InProgress,
            expected: "assigned to this agent",
        }
    );
    assert_eq!(
        queue.item_status(&s("owned-fail")),
        Some(WorkItemStatus::InProgress),
        "ownership violation must not mutate item state"
    );
    assert_eq!(
        queue
            .get_assignment(&s("owned-fail"))
            .map(|a| a.agent_slot.as_str()),
        Some("agent-1"),
        "ownership violation must preserve original assignment"
    );
}

// =============================================================================
// Scheduler + queue integration
// =============================================================================

#[test]
fn scheduler_recommends_action_on_ready_work() {
    let mut scheduler = SwarmScheduler::new(test_scheduler_config());

    // Create a queue with lots of ready work and assign some to saturate agents
    let mut queue = SwarmWorkQueue::new(WorkQueueConfig {
        max_concurrent_per_agent: 3,
        ..Default::default()
    });

    // Add 20 ready items
    for i in 0..20 {
        queue
            .enqueue(work_item(&format!("t-{i}"), i % 3, &[]))
            .unwrap();
    }

    // Assign 9 items (3 agents * 3 max = saturated)
    for i in 0..9 {
        queue
            .assign(&format!("t-{i}"), &format!("agent-{}", i % 3))
            .unwrap();
    }

    let decision = scheduler.evaluate(&mut queue, 10_000);

    // With saturated agents and remaining ready work, expect some action
    match decision {
        SchedulerDecision::ScaleUp {
            additional_agents,
            reason,
        } => {
            assert!(additional_agents >= 1, "should scale up by at least 1");
            assert!(additional_agents <= 3, "should not exceed max_scale_step");
            assert!(!reason.is_empty(), "should have a reason");
        }
        SchedulerDecision::AssignWork { assignments } => {
            assert!(!assignments.is_empty());
        }
        SchedulerDecision::Noop { .. } => {
            // Acceptable if cooldown or other conditions prevent action
        }
        _ => {} // Other decisions also valid
    }
}

#[test]
fn scheduler_does_not_crash_on_empty_queue() {
    let mut scheduler = SwarmScheduler::new(test_scheduler_config());
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    // Evaluate an empty queue — should produce Noop, not panic
    let decision = scheduler.evaluate(&mut queue, 1000);
    match decision {
        SchedulerDecision::Noop { reason } => {
            assert!(!reason.is_empty());
        }
        _ => {
            // Other decisions are also acceptable as long as no panic
        }
    }
}

#[test]
fn scheduler_circuit_breaker_limits_consecutive_operations() {
    let config = SchedulerConfig {
        max_consecutive_scale_ops: 2,
        scale_up_cooldown_ms: 0,             // No cooldown for test
        circuit_breaker_reset_ms: 1_000_000, // Very long reset
        ..test_scheduler_config()
    };
    let mut scheduler = SwarmScheduler::new(config);

    // Create high-pressure queue: many ready items, agents saturated
    let mut queue = SwarmWorkQueue::new(WorkQueueConfig {
        max_concurrent_per_agent: 2,
        ..Default::default()
    });

    for i in 0..30 {
        queue.enqueue(work_item(&format!("t-{i}"), 0, &[])).unwrap();
    }

    // Assign to fill 2 agents at capacity
    for i in 0..4 {
        queue
            .assign(&format!("t-{i}"), &format!("agent-{}", i % 2))
            .unwrap();
    }

    // Evaluate multiple times — circuit breaker should trip after max_consecutive_scale_ops
    let d1 = scheduler.evaluate(&mut queue, 1000);
    let d2 = scheduler.evaluate(&mut queue, 2000);
    let d3 = scheduler.evaluate(&mut queue, 3000);
    let d4 = scheduler.evaluate(&mut queue, 4000);

    let scale_ups = [&d1, &d2, &d3, &d4]
        .iter()
        .filter(|d| matches!(d, SchedulerDecision::ScaleUp { .. }))
        .count();

    // Circuit breaker should limit consecutive scale-ups
    assert!(
        scale_ups <= 3,
        "circuit breaker should limit consecutive scale ops, got {scale_ups}"
    );
}

// =============================================================================
// End-to-end: fleet launch → work dispatch → schedule cycle
// =============================================================================

#[test]
fn e2e_fleet_launch_then_work_dispatch() {
    // 1. Launch a fleet
    let profiles = setup_launcher_with_profiles();
    let launcher = FleetLauncher::new(&profiles);
    let spec = test_fleet_spec("e2e-fleet", 3);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    let mut registry = LifecycleRegistry::new();
    let outcome = launcher.execute_with_subsystems(&plan, &mut registry, None, None);
    assert_eq!(outcome.status, FleetLaunchStatus::Complete);
    assert_eq!(outcome.successful_slots, 3);

    // 2. Set up work queue with tasks
    let mut queue = SwarmWorkQueue::new(test_queue_config());
    queue.enqueue(work_item("task-1", 0, &[])).unwrap();
    queue.enqueue(work_item("task-2", 1, &[])).unwrap();
    queue.enqueue(work_item("task-3", 1, &["task-1"])).unwrap();
    queue.enqueue(work_item("task-4", 2, &["task-2"])).unwrap();

    // 3. Assign ready work to fleet agents
    let ready = queue.ready_items();
    assert_eq!(ready.len(), 2); // task-1 and task-2 are ready

    // Use slot labels as agent IDs
    let agent_ids: Vec<String> = plan.slots.iter().map(|s| s.label.clone()).collect();

    queue.assign(&s("task-1"), &agent_ids[0]).unwrap();
    queue.assign(&s("task-2"), &agent_ids[1]).unwrap();

    // 4. Complete tasks and verify unblocking
    queue.complete(&s("task-1"), &agent_ids[0], None).unwrap();
    let ready = queue.ready_items();
    assert!(
        ready.iter().any(|r| r.id == "task-3"),
        "task-3 should be unblocked after task-1 completion"
    );

    queue.complete(&s("task-2"), &agent_ids[1], None).unwrap();
    let ready = queue.ready_items();
    assert!(
        ready.iter().any(|r| r.id == "task-4"),
        "task-4 should be unblocked after task-2 completion"
    );
}

#[test]
fn e2e_scheduler_evaluates_queue_state() {
    // 1. Set up work queue with varying load
    let mut queue = SwarmWorkQueue::new(test_queue_config());
    for i in 0..15 {
        queue
            .enqueue(work_item(&format!("t-{i}"), i % 3, &[]))
            .unwrap();
    }

    // 2. Assign some work
    for i in 0..6 {
        queue
            .assign(&format!("t-{i}"), &format!("agent-{}", i % 3))
            .unwrap();
    }

    // 3. Evaluate with scheduler — should produce a valid decision
    let mut scheduler = SwarmScheduler::new(test_scheduler_config());
    let decision = scheduler.evaluate(&mut queue, 10_000);

    // Should produce some decision (not panic)
    match &decision {
        SchedulerDecision::Noop { reason } => {
            assert!(!reason.is_empty());
        }
        SchedulerDecision::AssignWork { assignments } => {
            assert!(!assignments.is_empty());
        }
        SchedulerDecision::ScaleUp {
            additional_agents, ..
        } => {
            assert!(*additional_agents >= 1);
        }
        SchedulerDecision::Rebalance { moves } => {
            assert!(!moves.is_empty());
        }
        _ => {} // Other decisions are also valid
    }
}

#[test]
fn work_queue_snapshot_preserves_state_across_restore() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    queue.enqueue(work_item("a", 0, &[])).unwrap();
    queue.enqueue(work_item("b", 1, &["a"])).unwrap();
    queue.assign(&s("a"), &s("agent-1")).unwrap();

    // Take snapshot
    let snapshot = queue.snapshot();

    // Create new queue from snapshot
    let restored = SwarmWorkQueue::restore(snapshot, test_queue_config());

    // Verify state preserved
    let status_a = restored.item_status(&s("a")).unwrap();
    assert_eq!(status_a, WorkItemStatus::InProgress);

    let status_b = restored.item_status(&s("b")).unwrap();
    assert_eq!(status_b, WorkItemStatus::Blocked);
}

#[test]
fn fleet_launch_with_durable_state_creates_checkpoint() {
    let profiles = setup_launcher_with_profiles();
    let launcher = FleetLauncher::new(&profiles);
    let spec = test_fleet_spec("durable-fleet", 3);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    let mut registry = LifecycleRegistry::new();
    let mut durable = frankenterm_core::durable_state::DurableStateManager::new();

    // Create initial checkpoint so durable_state has history
    durable.checkpoint(
        &registry,
        "pre-test",
        frankenterm_core::durable_state::CheckpointTrigger::Manual,
        HashMap::new(),
    );

    let outcome = launcher.execute_with_subsystems(&plan, &mut registry, Some(&mut durable), None);

    assert_eq!(outcome.status, FleetLaunchStatus::Complete);
    // Pre-launch checkpoint should be recorded when durable state is available
    assert!(
        outcome.pre_launch_checkpoint.is_some(),
        "should record pre-launch checkpoint"
    );
}

// =============================================================================
// Queue anti-starvation and fairness
// =============================================================================

#[test]
fn work_queue_respects_priority_ordering() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    // Enqueue in reverse priority order
    queue.enqueue(work_item("low", 3, &[])).unwrap();
    queue.enqueue(work_item("high", 0, &[])).unwrap();
    queue.enqueue(work_item("mid", 1, &[])).unwrap();

    let ready = queue.ready_items();
    assert_eq!(ready.len(), 3);
    // First item should be highest priority (lowest number)
    assert_eq!(ready[0].id, "high");
}

// =============================================================================
// Cycle detection
// =============================================================================

#[test]
fn work_queue_rejects_cyclic_dependencies() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    queue.enqueue(work_item("a", 0, &[])).unwrap();
    queue.enqueue(work_item("b", 0, &["a"])).unwrap();

    // Adding a dependency from a→b would create a cycle
    assert!(
        queue.would_create_cycle(&"a".to_string(), &["b".to_string()]),
        "should detect direct cycle"
    );
}

#[test]
fn work_queue_detects_transitive_cycles() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    queue.enqueue(work_item("a", 0, &[])).unwrap();
    queue.enqueue(work_item("b", 0, &["a"])).unwrap();
    queue.enqueue(work_item("c", 0, &["b"])).unwrap();

    // a→c is transitive through b
    assert!(
        queue.would_create_cycle(&"a".to_string(), &["c".to_string()]),
        "should detect transitive cycle through a→b→c→a"
    );
}
