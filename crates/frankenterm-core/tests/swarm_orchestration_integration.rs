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
    AgentMixEntry, FleetLauncher, FleetLauncherConfig, FleetLaunchStatus, FleetSpec,
    LaunchOutcome, SlotStatus, StartupStrategy,
};
use frankenterm_core::session_profiles::{
    AgentIdentitySpec, FleetProgramTarget, FleetSlot, FleetStartupStrategy, FleetTemplate,
    Persona, ProfilePolicy, ProfileRegistry, ProfileRole, ResourceHints, SessionProfile,
    SpawnCommand,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState,
};
use frankenterm_core::swarm_scheduler::{
    AgentLoadSnapshot, QueuePressure, SchedulerConfig, SchedulerDecision, SwarmScheduler,
};
use frankenterm_core::swarm_work_queue::{
    SwarmWorkQueue, SwarmWorkQueueConfig, WorkItem, WorkItemStatus,
};

// =============================================================================
// Test helpers
// =============================================================================

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
        role: ProfileRole::Worker,
        spawn: SpawnCommand::Shell {
            command: "echo test".to_string(),
            args: vec![],
        },
        persona: None,
        environment: HashMap::new(),
        tags: vec![],
        policy: ProfilePolicy::default(),
        resource_hints: ResourceHints::default(),
        identity: AgentIdentitySpec::default(),
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

fn test_queue_config() -> SwarmWorkQueueConfig {
    SwarmWorkQueueConfig {
        max_concurrent_per_agent: 3,
        starvation_threshold_ms: 60_000,
        max_history_len: 100,
        max_items: 1000,
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
        new_agent_grace_ms: 5000,
        max_scale_step: 3,
    }
}

// =============================================================================
// Fleet launch + lifecycle integration
// =============================================================================

#[test]
fn fleet_launch_registers_lifecycle_entities() {
    let mut profiles = ProfileRegistry::new();
    profiles.register(test_profile());

    let launcher = FleetLauncher::new(FleetLauncherConfig::default(), profiles);
    let spec = test_fleet_spec("alpha", 6);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    assert_eq!(plan.slots.len(), 6);

    let mut registry = LifecycleRegistry::new();
    let outcome = launcher.execute_with_subsystems(&plan, &mut registry, None, None);

    assert_eq!(outcome.status, FleetLaunchStatus::AllRegistered);
    assert_eq!(outcome.total_slots, 6);
    assert_eq!(outcome.successful_slots, 6);
    assert_eq!(outcome.failed_slots, 0);

    // Verify lifecycle entities were registered
    assert!(!outcome.registry_snapshot.is_empty());
}

#[test]
fn fleet_plan_respects_weighted_mix() {
    let mut profiles = ProfileRegistry::new();
    profiles.register(test_profile());

    let launcher = FleetLauncher::new(FleetLauncherConfig::default(), profiles);
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
    queue.assign("a", "agent-1").unwrap();
    queue.complete("a").unwrap();

    // Now "b" and "c" should be ready
    let ready = queue.ready_items();
    assert_eq!(ready.len(), 2);
    let ready_ids: Vec<&str> = ready.iter().map(|r| r.id.as_str()).collect();
    assert!(ready_ids.contains(&"b"));
    assert!(ready_ids.contains(&"c"));

    // "d" still blocked
    let status_d = queue.status("d").unwrap();
    assert_eq!(status_d, WorkItemStatus::Blocked);
}

#[test]
fn work_queue_full_dag_completion_flow() {
    let mut queue = SwarmWorkQueue::new(test_queue_config());

    // Diamond dependency: a → (b, c) → d
    queue.enqueue(work_item("root", 0, &[])).unwrap();
    queue.enqueue(work_item("left", 1, &["root"])).unwrap();
    queue.enqueue(work_item("right", 1, &["root"])).unwrap();
    queue.enqueue(work_item("sink", 2, &["left", "right"])).unwrap();

    // Complete entire DAG
    queue.assign("root", "agent-1").unwrap();
    queue.complete("root").unwrap();

    queue.assign("left", "agent-1").unwrap();
    queue.assign("right", "agent-2").unwrap();
    queue.complete("left").unwrap();
    queue.complete("right").unwrap();

    // Sink should now be ready
    let ready = queue.ready_items();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "sink");

    queue.assign("sink", "agent-1").unwrap();
    queue.complete("sink").unwrap();

    // All items completed
    let stats = queue.stats();
    assert_eq!(stats.completed, 4);
    assert_eq!(stats.pending, 0);
}

// =============================================================================
// Scheduler + queue pressure integration
// =============================================================================

#[test]
fn scheduler_recommends_scale_up_on_high_pressure() {
    let mut scheduler = SwarmScheduler::new(test_scheduler_config());

    let pressure = QueuePressure {
        ready_ratio: 0.7,
        utilization: 0.95, // above scale_up_threshold (0.8)
        starvation_count: 2,
        failure_rate: 0.0,
        pending_items: 20,
        active_agents: 3,
        total_capacity: 9,
    };

    let loads = vec![
        AgentLoadSnapshot {
            agent_id: "a1".to_string(),
            active_items: 3,
            max_items: 3,
            completed_count: 10,
            failed_count: 0,
            first_seen_ms: 0,
        },
        AgentLoadSnapshot {
            agent_id: "a2".to_string(),
            active_items: 3,
            max_items: 3,
            completed_count: 8,
            failed_count: 0,
            first_seen_ms: 0,
        },
        AgentLoadSnapshot {
            agent_id: "a3".to_string(),
            active_items: 3,
            max_items: 3,
            completed_count: 5,
            failed_count: 0,
            first_seen_ms: 0,
        },
    ];

    let decision = scheduler.evaluate(&pressure, &loads, 10_000);

    // With high utilization and remaining capacity, should recommend scale-up
    match decision {
        SchedulerDecision::ScaleUp {
            additional_agents,
            reason,
        } => {
            assert!(additional_agents >= 1, "should scale up by at least 1");
            assert!(additional_agents <= 3, "should not exceed max_scale_step");
            assert!(!reason.is_empty(), "should have a reason");
        }
        SchedulerDecision::AssignWork { .. } => {
            // Also acceptable if there's work to assign first
        }
        other => {
            panic!("expected ScaleUp or AssignWork, got {other:?}");
        }
    }
}

#[test]
fn scheduler_recommends_scale_down_on_low_pressure() {
    let mut scheduler = SwarmScheduler::new(test_scheduler_config());

    let pressure = QueuePressure {
        ready_ratio: 0.1,
        utilization: 0.1, // below scale_down_threshold (0.2)
        starvation_count: 0,
        failure_rate: 0.0,
        pending_items: 1,
        active_agents: 8,
        total_capacity: 24,
    };

    let loads: Vec<AgentLoadSnapshot> = (0..8)
        .map(|i| AgentLoadSnapshot {
            agent_id: format!("a{i}"),
            active_items: if i < 1 { 1 } else { 0 },
            max_items: 3,
            completed_count: 5,
            failed_count: 0,
            first_seen_ms: 0,
        })
        .collect();

    let decision = scheduler.evaluate(&pressure, &loads, 10_000);

    match decision {
        SchedulerDecision::ScaleDown {
            remove_agents,
            reason,
        } => {
            assert!(
                !remove_agents.is_empty(),
                "should recommend removing agents"
            );
            assert!(
                remove_agents.len() <= 3,
                "should not remove more than max_scale_step"
            );
            assert!(!reason.is_empty());
        }
        SchedulerDecision::Noop { .. } => {
            // Also acceptable if cooldown or grace period prevents action
        }
        other => {
            panic!("expected ScaleDown or Noop, got {other:?}");
        }
    }
}

// =============================================================================
// End-to-end: fleet launch → work dispatch → schedule cycle
// =============================================================================

#[test]
fn e2e_fleet_launch_then_work_dispatch() {
    // 1. Launch a fleet
    let mut profiles = ProfileRegistry::new();
    profiles.register(test_profile());

    let launcher = FleetLauncher::new(FleetLauncherConfig::default(), profiles);
    let spec = test_fleet_spec("e2e-fleet", 3);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    let mut registry = LifecycleRegistry::new();
    let outcome = launcher.execute_with_subsystems(&plan, &mut registry, None, None);
    assert_eq!(outcome.status, FleetLaunchStatus::AllRegistered);
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

    queue.assign("task-1", &agent_ids[0]).unwrap();
    queue.assign("task-2", &agent_ids[1]).unwrap();

    // 4. Complete tasks and verify unblocking
    queue.complete("task-1").unwrap();
    let ready = queue.ready_items();
    assert!(
        ready.iter().any(|r| r.id == "task-3"),
        "task-3 should be unblocked after task-1 completion"
    );

    queue.complete("task-2").unwrap();
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
        queue.enqueue(work_item(&format!("t-{i}"), i % 3, &[])).unwrap();
    }

    // 2. Assign some work
    for i in 0..6 {
        queue.assign(&format!("t-{i}"), &format!("agent-{}", i % 3)).unwrap();
    }

    // 3. Build queue pressure from stats
    let stats = queue.stats();
    let pressure = QueuePressure {
        ready_ratio: stats.ready as f64 / stats.total.max(1) as f64,
        utilization: stats.in_progress as f64 / 9.0_f64.max(1.0), // 3 agents * 3 concurrent
        starvation_count: 0,
        failure_rate: 0.0,
        pending_items: stats.ready + stats.blocked,
        active_agents: 3,
        total_capacity: 9,
    };

    // 4. Evaluate with scheduler
    let mut scheduler = SwarmScheduler::new(test_scheduler_config());
    let loads = vec![
        AgentLoadSnapshot {
            agent_id: "agent-0".to_string(),
            active_items: 2,
            max_items: 3,
            completed_count: 0,
            failed_count: 0,
            first_seen_ms: 0,
        },
        AgentLoadSnapshot {
            agent_id: "agent-1".to_string(),
            active_items: 2,
            max_items: 3,
            completed_count: 0,
            failed_count: 0,
            first_seen_ms: 0,
        },
        AgentLoadSnapshot {
            agent_id: "agent-2".to_string(),
            active_items: 2,
            max_items: 3,
            completed_count: 0,
            failed_count: 0,
            first_seen_ms: 0,
        },
    ];

    let decision = scheduler.evaluate(&pressure, &loads, 10_000);

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
    queue.assign("a", "agent-1").unwrap();

    // Take snapshot
    let snapshot = queue.snapshot();

    // Create new queue from snapshot
    let restored = SwarmWorkQueue::from_snapshot(snapshot);

    // Verify state preserved
    let status_a = restored.status("a").unwrap();
    assert_eq!(status_a, WorkItemStatus::InProgress);

    let status_b = restored.status("b").unwrap();
    assert_eq!(status_b, WorkItemStatus::Blocked);
}

#[test]
fn fleet_launch_with_durable_state_creates_checkpoint() {
    let mut profiles = ProfileRegistry::new();
    profiles.register(test_profile());

    let launcher = FleetLauncher::new(FleetLauncherConfig::default(), profiles);
    let spec = test_fleet_spec("durable-fleet", 3);

    let plan = launcher.plan(&spec).expect("plan should succeed");
    let mut registry = LifecycleRegistry::new();
    let mut durable = frankenterm_core::durable_state::DurableStateManager::new();

    // Create initial checkpoint so durable_state has history
    durable.checkpoint(
        frankenterm_core::durable_state::CheckpointTrigger::Manual,
        &registry,
    );

    let outcome = launcher.execute_with_subsystems(&plan, &mut registry, Some(&mut durable), None);

    assert_eq!(outcome.status, FleetLaunchStatus::AllRegistered);
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

#[test]
fn scheduler_circuit_breaker_prevents_cascade() {
    let config = SchedulerConfig {
        max_consecutive_scale_ops: 2,
        scale_up_cooldown_ms: 0, // No cooldown for test
        ..test_scheduler_config()
    };
    let mut scheduler = SwarmScheduler::new(config);

    let high_pressure = QueuePressure {
        ready_ratio: 0.9,
        utilization: 0.99,
        starvation_count: 5,
        failure_rate: 0.0,
        pending_items: 50,
        active_agents: 3,
        total_capacity: 9,
    };

    let loads = vec![AgentLoadSnapshot {
        agent_id: "a1".to_string(),
        active_items: 3,
        max_items: 3,
        completed_count: 0,
        failed_count: 0,
        first_seen_ms: 0,
    }];

    // First scale-up should succeed
    let d1 = scheduler.evaluate(&high_pressure, &loads, 1000);
    let d2 = scheduler.evaluate(&high_pressure, &loads, 2000);
    let d3 = scheduler.evaluate(&high_pressure, &loads, 3000);

    // After max_consecutive_scale_ops (2), circuit breaker should trip
    // and produce Noop instead of more ScaleUp
    let scale_ups = [&d1, &d2, &d3]
        .iter()
        .filter(|d| matches!(d, SchedulerDecision::ScaleUp { .. }))
        .count();

    // Should have at most max_consecutive_scale_ops scale-ups
    assert!(
        scale_ups <= 2,
        "circuit breaker should limit consecutive scale ops, got {scale_ups}"
    );
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
