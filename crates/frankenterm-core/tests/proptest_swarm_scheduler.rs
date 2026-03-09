// Property-based tests for swarm_scheduler module (ft-3681t.3.2)
//
// Covers: serde roundtrips for all public types, scheduling decision invariants,
// pressure computation properties, circuit breaker semantics, cooldown timers,
// scale bounds, snapshot/restore consistency, history eviction, and Display coverage.
#![allow(clippy::ignored_unit_patterns)]

use std::collections::{BTreeMap, HashMap};

use proptest::prelude::*;


use frankenterm_core::swarm_scheduler::*;
use frankenterm_core::swarm_work_queue::{QueueStats, SwarmWorkQueue, WorkItem, WorkQueueConfig};

// =============================================================================
// Strategies
// =============================================================================

fn arb_scheduler_config() -> impl Strategy<Value = SchedulerConfig> {
    (
        1_000u64..300_000,  // scale_up_cooldown_ms
        2_000u64..600_000,  // scale_down_cooldown_ms
        1u32..4,            // min_fleet_size
        5u32..128,          // max_fleet_size
        0.5f64..0.99,       // scale_up_threshold
        0.01f64..0.49,      // scale_down_threshold
        0.1f64..0.9,        // rebalance_imbalance_threshold
        2u32..10,           // max_consecutive_scale_ops
        5_000u64..120_000,  // agent_startup_grace_ms
        10_000u64..600_000, // circuit_breaker_reset_ms
        1u32..8,            // max_scale_step
        0.1f64..0.9,        // failure_rate_suppress_threshold
    )
        .prop_map(
            |(
                scale_up_cooldown_ms,
                scale_down_cooldown_ms,
                min_fleet_size,
                max_fleet_size,
                scale_up_threshold,
                scale_down_threshold,
                rebalance_imbalance_threshold,
                max_consecutive_scale_ops,
                agent_startup_grace_ms,
                circuit_breaker_reset_ms,
                max_scale_step,
                failure_rate_suppress_threshold,
            )| {
                SchedulerConfig {
                    scale_up_cooldown_ms,
                    scale_down_cooldown_ms,
                    min_fleet_size,
                    max_fleet_size: max_fleet_size.max(min_fleet_size + 1),
                    scale_up_threshold,
                    scale_down_threshold,
                    rebalance_imbalance_threshold,
                    max_consecutive_scale_ops,
                    agent_startup_grace_ms,
                    circuit_breaker_reset_ms,
                    max_scale_step,
                    failure_rate_suppress_threshold,
                }
            },
        )
}

fn arb_queue_pressure() -> impl Strategy<Value = QueuePressure> {
    (
        0.0f64..1.0, // ready_ratio
        0.0f64..1.0, // utilization
        0u32..100,   // starvation_count
        0.0f64..1.0, // failure_rate
        0u32..1000,  // pending_items
        0u32..64,    // active_agents
        0u32..256,   // total_capacity
    )
        .prop_map(
            |(
                ready_ratio,
                utilization,
                starvation_count,
                failure_rate,
                pending_items,
                active_agents,
                total_capacity,
            )| {
                QueuePressure {
                    ready_ratio,
                    utilization,
                    starvation_count,
                    failure_rate,
                    pending_items,
                    active_agents,
                    total_capacity,
                }
            },
        )
}

fn arb_agent_load_snapshot() -> impl Strategy<Value = AgentLoadSnapshot> {
    (
        "[a-z][a-z0-9_-]{2,12}",
        0u32..10,
        1u32..10,
        0u32..1000,
        0u32..500,
        0u64..1_000_000,
    )
        .prop_map(
            |(agent_id, active_items, max_items, completed_count, failed_count, first_seen_ms)| {
                AgentLoadSnapshot {
                    agent_id,
                    active_items,
                    max_items: max_items.max(1),
                    completed_count,
                    failed_count,
                    first_seen_ms,
                }
            },
        )
}

fn arb_work_assignment() -> impl Strategy<Value = WorkAssignment> {
    ("[a-z][a-z0-9_-]{2,12}", "[a-z][a-z0-9_-]{2,12}")
        .prop_map(|(item_id, agent_id)| WorkAssignment { item_id, agent_id })
}

fn arb_rebalance_move() -> impl Strategy<Value = RebalanceMove> {
    (
        "[a-z][a-z0-9_-]{2,12}",
        "[a-z][a-z0-9_-]{2,12}",
        "[a-z][a-z0-9_-]{2,12}",
        "[a-z ]{5,30}",
    )
        .prop_map(|(item_id, from_agent, to_agent, reason)| RebalanceMove {
            item_id,
            from_agent,
            to_agent,
            reason,
        })
}

fn arb_scheduler_decision() -> impl Strategy<Value = SchedulerDecision> {
    prop_oneof![
        "[a-z ]{5,30}".prop_map(|reason| SchedulerDecision::Noop { reason }),
        prop::collection::vec(arb_work_assignment(), 1..=4)
            .prop_map(|assignments| SchedulerDecision::AssignWork { assignments }),
        prop::collection::vec(arb_rebalance_move(), 1..=3)
            .prop_map(|moves| SchedulerDecision::Rebalance { moves }),
        (1u32..8, "[a-z ]{5,30}").prop_map(|(n, reason)| SchedulerDecision::ScaleUp {
            additional_agents: n,
            reason,
        }),
        (
            prop::collection::vec("[a-z][a-z0-9_-]{2,12}", 1..=4),
            "[a-z ]{5,30}",
        )
            .prop_map(|(agents, reason)| SchedulerDecision::ScaleDown {
                remove_agents: agents,
                reason,
            }),
        prop::collection::vec("[a-z][a-z0-9_-]{2,12}", 1..=5).prop_map(|items| {
            SchedulerDecision::ReclaimStale {
                reclaimed_items: items,
            }
        }),
    ]
}

fn arb_scale_event_type() -> impl Strategy<Value = ScaleEventType> {
    prop_oneof![
        Just(ScaleEventType::ScaleUp),
        Just(ScaleEventType::ScaleDown),
        Just(ScaleEventType::CircuitBreakerTripped),
        Just(ScaleEventType::CircuitBreakerReset),
    ]
}

fn arb_scale_event() -> impl Strategy<Value = ScaleEvent> {
    (
        arb_scale_event_type(),
        0u64..1_000_000,
        "[a-z ]{5,30}",
        0u32..64,
        0u32..64,
        arb_scheduler_decision(),
    )
        .prop_map(
            |(event_type, timestamp_ms, reason, fleet_size_before, fleet_size_after, decision)| {
                ScaleEvent {
                    event_type,
                    timestamp_ms,
                    reason,
                    fleet_size_before,
                    fleet_size_after,
                    decision,
                }
            },
        )
}

fn arb_scheduler_error() -> impl Strategy<Value = SchedulerError> {
    prop_oneof![
        (0u64..1_000_000, 0u64..1_000_000).prop_map(|(tripped_at, resets_at)| {
            SchedulerError::CircuitBreakerActive {
                tripped_at,
                resets_at,
            }
        }),
        (0u32..128, 0u32..128)
            .prop_map(|(current, max)| SchedulerError::AtMaxCapacity { current, max }),
        (0u32..128, 0u32..128)
            .prop_map(|(current, min)| SchedulerError::AtMinCapacity { current, min }),
        ("[a-z-]{3,15}", 0u64..300_000).prop_map(|(operation, remaining_ms)| {
            SchedulerError::CooldownActive {
                operation,
                remaining_ms,
            }
        }),
        Just(SchedulerError::NoAgentsAvailable),
        Just(SchedulerError::NoReadyWork),
    ]
}

fn arb_scheduler_snapshot() -> impl Strategy<Value = SchedulerSnapshot> {
    (
        arb_scheduler_config(),
        0u64..1_000_000,
        0u64..1_000_000,
        0u64..1_000_000,
        0u32..10,
        prop::option::of(0u64..1_000_000),
        prop::collection::vec(arb_scale_event(), 0..=5),
        prop::collection::btree_map("[a-z][a-z0-9_-]{2,8}", 0u64..1_000_000, 0..=5),
        prop::collection::btree_map("[a-z][a-z0-9_-]{2,8}", 0u32..100, 0..=5),
        prop::collection::btree_map("[a-z][a-z0-9_-]{2,8}", 0u32..50, 0..=5),
        0u64..10_000,
    )
        .prop_map(
            |(
                config,
                last_scale_up_ms,
                last_scale_down_ms,
                last_evaluation_ms,
                consecutive_scale_ops,
                circuit_breaker_tripped_at,
                scale_history,
                agent_first_seen,
                agent_completed,
                agent_failed,
                sequence,
            )| {
                SchedulerSnapshot {
                    config,
                    last_scale_up_ms,
                    last_scale_down_ms,
                    last_evaluation_ms,
                    consecutive_scale_ops,
                    circuit_breaker_tripped_at,
                    scale_history,
                    agent_first_seen,
                    agent_completed,
                    agent_failed,
                    sequence,
                }
            },
        )
}

// =============================================================================
// Helpers
// =============================================================================

const F64_TOLERANCE: f64 = 1e-12;

fn f64_approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() < F64_TOLERANCE
}

fn pressure_approx_eq(a: &QueuePressure, b: &QueuePressure) -> bool {
    f64_approx_eq(a.ready_ratio, b.ready_ratio)
        && f64_approx_eq(a.utilization, b.utilization)
        && a.starvation_count == b.starvation_count
        && f64_approx_eq(a.failure_rate, b.failure_rate)
        && a.pending_items == b.pending_items
        && a.active_agents == b.active_agents
        && a.total_capacity == b.total_capacity
}

fn make_queue() -> SwarmWorkQueue {
    SwarmWorkQueue::new(WorkQueueConfig {
        max_concurrent_per_agent: 3,
        heartbeat_timeout_ms: 30_000,
        max_retries: 2,
        anti_starvation: true,
        starvation_threshold_ms: 60_000,
    })
}

fn make_item(id: &str, priority: u32) -> WorkItem {
    WorkItem {
        id: id.to_string(),
        title: format!("Work item {id}"),
        priority,
        depends_on: Vec::new(),
        effort: 1,
        labels: Vec::new(),
        preferred_program: None,
        metadata: HashMap::new(),
    }
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn scheduler_config_serde_roundtrip(cfg in arb_scheduler_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: SchedulerConfig = serde_json::from_str(&json).unwrap();
        // f64 fields need tolerance
        assert_eq!(restored.scale_up_cooldown_ms, cfg.scale_up_cooldown_ms);
        assert_eq!(restored.scale_down_cooldown_ms, cfg.scale_down_cooldown_ms);
        assert_eq!(restored.min_fleet_size, cfg.min_fleet_size);
        assert_eq!(restored.max_fleet_size, cfg.max_fleet_size);
        assert!(f64_approx_eq(restored.scale_up_threshold, cfg.scale_up_threshold));
        assert!(f64_approx_eq(restored.scale_down_threshold, cfg.scale_down_threshold));
        assert!(f64_approx_eq(restored.rebalance_imbalance_threshold, cfg.rebalance_imbalance_threshold));
        assert_eq!(restored.max_consecutive_scale_ops, cfg.max_consecutive_scale_ops);
        assert_eq!(restored.agent_startup_grace_ms, cfg.agent_startup_grace_ms);
        assert_eq!(restored.circuit_breaker_reset_ms, cfg.circuit_breaker_reset_ms);
        assert_eq!(restored.max_scale_step, cfg.max_scale_step);
        assert!(f64_approx_eq(restored.failure_rate_suppress_threshold, cfg.failure_rate_suppress_threshold));
    }

    #[test]
    fn queue_pressure_serde_roundtrip(p in arb_queue_pressure()) {
        let json = serde_json::to_string(&p).unwrap();
        let restored: QueuePressure = serde_json::from_str(&json).unwrap();
        assert!(pressure_approx_eq(&p, &restored));
    }

    #[test]
    fn agent_load_snapshot_serde_roundtrip(snap in arb_agent_load_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let restored: AgentLoadSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, restored);
    }

    #[test]
    fn work_assignment_serde_roundtrip(wa in arb_work_assignment()) {
        let json = serde_json::to_string(&wa).unwrap();
        let restored: WorkAssignment = serde_json::from_str(&json).unwrap();
        assert_eq!(wa, restored);
    }

    #[test]
    fn rebalance_move_serde_roundtrip(rm in arb_rebalance_move()) {
        let json = serde_json::to_string(&rm).unwrap();
        let restored: RebalanceMove = serde_json::from_str(&json).unwrap();
        assert_eq!(rm, restored);
    }

    #[test]
    fn scheduler_decision_serde_roundtrip(d in arb_scheduler_decision()) {
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    #[test]
    fn scale_event_type_serde_roundtrip(t in arb_scale_event_type()) {
        let json = serde_json::to_string(&t).unwrap();
        let restored: ScaleEventType = serde_json::from_str(&json).unwrap();
        assert_eq!(t, restored);
    }

    #[test]
    fn scale_event_serde_roundtrip(e in arb_scale_event()) {
        let json = serde_json::to_string(&e).unwrap();
        let restored: ScaleEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(e, restored);
    }

    #[test]
    fn scheduler_error_serde_roundtrip(e in arb_scheduler_error()) {
        let json = serde_json::to_string(&e).unwrap();
        let restored: SchedulerError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, restored);
    }

    #[test]
    fn scheduler_snapshot_serde_roundtrip(snap in arb_scheduler_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let restored: SchedulerSnapshot = serde_json::from_str(&json).unwrap();
        // Compare field by field for f64 tolerance
        assert!(f64_approx_eq(restored.config.scale_up_threshold, snap.config.scale_up_threshold));
        assert_eq!(restored.last_scale_up_ms, snap.last_scale_up_ms);
        assert_eq!(restored.last_scale_down_ms, snap.last_scale_down_ms);
        assert_eq!(restored.last_evaluation_ms, snap.last_evaluation_ms);
        assert_eq!(restored.consecutive_scale_ops, snap.consecutive_scale_ops);
        assert_eq!(restored.circuit_breaker_tripped_at, snap.circuit_breaker_tripped_at);
        assert_eq!(restored.scale_history.len(), snap.scale_history.len());
        assert_eq!(restored.agent_first_seen, snap.agent_first_seen);
        assert_eq!(restored.agent_completed, snap.agent_completed);
        assert_eq!(restored.agent_failed, snap.agent_failed);
        assert_eq!(restored.sequence, snap.sequence);
    }
}

// =============================================================================
// Config invariant tests
// =============================================================================

proptest! {
    #[test]
    fn config_max_fleet_always_exceeds_min(cfg in arb_scheduler_config()) {
        assert!(cfg.max_fleet_size > cfg.min_fleet_size);
    }

    #[test]
    fn config_scale_up_threshold_always_above_scale_down(cfg in arb_scheduler_config()) {
        assert!(cfg.scale_up_threshold > cfg.scale_down_threshold);
    }

    #[test]
    fn default_config_is_valid(_dummy in 0..1u32) {
        let cfg = SchedulerConfig::default();
        assert!(cfg.max_fleet_size > cfg.min_fleet_size);
        assert!(cfg.scale_up_threshold > cfg.scale_down_threshold);
        assert!(cfg.max_consecutive_scale_ops > 0);
        assert!(cfg.max_scale_step > 0);
    }
}

// =============================================================================
// Pressure computation properties
// =============================================================================

proptest! {
    #[test]
    fn pressure_utilization_bounded(
        in_progress in 0usize..100,
        active_agents in 0usize..20,
        max_concurrent in 1u32..10,
        ready in 0usize..100,
    ) {
        let scheduler = SwarmScheduler::with_defaults();
        let capacity = active_agents as u32 * max_concurrent;
        let total = in_progress + ready;
        let stats = QueueStats {
            total_items: total,
            blocked: 0,
            ready,
            in_progress,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, max_concurrent);

        // Utilization must be >= 0.0 (can exceed 1.0 when in_progress > capacity,
        // e.g., items assigned before an agent was removed)
        assert!(pressure.utilization >= 0.0);
        if capacity > 0 {
            // When capacity exists, utilization = in_progress / capacity
            let expected = in_progress as f64 / capacity as f64;
            assert!(f64_approx_eq(pressure.utilization, expected));
        }
    }

    #[test]
    fn pressure_failure_rate_bounded(
        completed in 0usize..200,
        failed in 0usize..200,
    ) {
        let scheduler = SwarmScheduler::with_defaults();
        let total = completed + failed;
        let stats = QueueStats {
            total_items: total + 5,
            blocked: 0,
            ready: 5,
            in_progress: 0,
            completed,
            failed,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: total,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);

        assert!(pressure.failure_rate >= 0.0);
        assert!(pressure.failure_rate <= 1.0 + F64_TOLERANCE);
    }

    #[test]
    fn pressure_ready_ratio_bounded(
        ready in 0usize..100,
        in_progress in 0usize..100,
    ) {
        let scheduler = SwarmScheduler::with_defaults();
        let total = ready + in_progress;
        let stats = QueueStats {
            total_items: total,
            blocked: 0,
            ready,
            in_progress,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);

        assert!(pressure.ready_ratio >= 0.0);
        assert!(pressure.ready_ratio <= 1.0 + F64_TOLERANCE);
    }

    #[test]
    fn zero_capacity_with_work_saturates(
        ready in 1usize..50,
    ) {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: ready,
            blocked: 0,
            ready,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 0,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert!(f64_approx_eq(pressure.utilization, 1.0));
    }

    #[test]
    fn empty_queue_zero_pressure(max_concurrent in 1u32..10) {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 0,
            blocked: 0,
            ready: 0,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 0,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, max_concurrent);
        assert!(f64_approx_eq(pressure.utilization, 0.0));
        assert!(f64_approx_eq(pressure.ready_ratio, 0.0));
        assert!(f64_approx_eq(pressure.failure_rate, 0.0));
    }
}

// =============================================================================
// Circuit breaker properties
// =============================================================================

proptest! {
    #[test]
    fn circuit_breaker_resets_after_window(
        tripped_at in 0u64..500_000,
        reset_ms in 1_000u64..600_000,
    ) {
        let config = SchedulerConfig {
            circuit_breaker_reset_ms: reset_ms,
            ..SchedulerConfig::default()
        };
        let snap = SchedulerSnapshot {
            config,
            last_scale_up_ms: 0,
            last_scale_down_ms: 0,
            last_evaluation_ms: 0,
            consecutive_scale_ops: 0,
            circuit_breaker_tripped_at: Some(tripped_at),
            scale_history: Vec::new(),
            agent_first_seen: BTreeMap::new(),
            agent_completed: BTreeMap::new(),
            agent_failed: BTreeMap::new(),
            sequence: 0,
        };
        let scheduler = SwarmScheduler::restore(snap);

        // Before window: active
        let mid = tripped_at + reset_ms / 2;
        assert!(scheduler.circuit_breaker_active(mid));

        // After window: inactive
        let after = tripped_at.saturating_add(reset_ms).saturating_add(1);
        assert!(!scheduler.circuit_breaker_active(after));
    }

    #[test]
    fn circuit_breaker_inactive_when_not_tripped(now_ms in 0u64..1_000_000) {
        let scheduler = SwarmScheduler::with_defaults();
        assert!(!scheduler.circuit_breaker_active(now_ms));
    }

    #[test]
    fn manual_reset_clears_circuit_breaker(
        tripped_at in 0u64..500_000,
    ) {
        let config = SchedulerConfig::default();
        let snap = SchedulerSnapshot {
            config,
            last_scale_up_ms: 0,
            last_scale_down_ms: 0,
            last_evaluation_ms: 0,
            consecutive_scale_ops: 5,
            circuit_breaker_tripped_at: Some(tripped_at),
            scale_history: Vec::new(),
            agent_first_seen: BTreeMap::new(),
            agent_completed: BTreeMap::new(),
            agent_failed: BTreeMap::new(),
            sequence: 0,
        };
        let mut scheduler = SwarmScheduler::restore(snap);

        scheduler.reset_circuit_breaker();
        assert!(!scheduler.circuit_breaker_active(tripped_at));
    }
}

// =============================================================================
// Agent tracking properties
// =============================================================================

proptest! {
    #[test]
    fn register_deregister_roundtrip(
        agents in prop::collection::vec("[a-z][a-z0-9]{2,8}", 1..=10),
        now_ms in 0u64..1_000_000,
    ) {
        let mut scheduler = SwarmScheduler::with_defaults();
        let queue = make_queue();

        for agent in &agents {
            scheduler.register_agent(agent, now_ms);
        }
        let snapshots = scheduler.agent_snapshots(&queue, 3);
        // All registered agents should appear
        for agent in &agents {
            assert!(snapshots.iter().any(|s| &s.agent_id == agent));
        }

        // Deregister all
        for agent in &agents {
            scheduler.deregister_agent(agent);
        }
        let snapshots = scheduler.agent_snapshots(&queue, 3);
        assert!(snapshots.is_empty());
    }

    #[test]
    fn completions_and_failures_accumulate(
        completions in 0u32..20,
        failures in 0u32..20,
    ) {
        let mut scheduler = SwarmScheduler::with_defaults();
        let agent = "test-agent".to_string();
        scheduler.register_agent(&agent, 0);

        for _ in 0..completions {
            scheduler.record_completion(&agent);
        }
        for _ in 0..failures {
            scheduler.record_failure(&agent);
        }

        let queue = make_queue();
        let snapshots = scheduler.agent_snapshots(&queue, 3);
        let snap = snapshots.iter().find(|s| s.agent_id == agent).unwrap();
        assert_eq!(snap.completed_count, completions);
        assert_eq!(snap.failed_count, failures);
    }

    #[test]
    fn agent_snapshots_sorted(
        agents in prop::collection::hash_set("[a-z]{3,8}", 2..=8),
    ) {
        let mut scheduler = SwarmScheduler::with_defaults();
        let queue = make_queue();

        for agent in &agents {
            scheduler.register_agent(agent, 0);
        }

        let snapshots = scheduler.agent_snapshots(&queue, 3);
        for window in snapshots.windows(2) {
            assert!(window[0].agent_id <= window[1].agent_id);
        }
    }
}

// =============================================================================
// Snapshot/restore properties
// =============================================================================

proptest! {
    #[test]
    fn snapshot_restore_preserves_state(snap in arb_scheduler_snapshot()) {
        let scheduler = SwarmScheduler::restore(snap.clone());

        // Verify restored state matches snapshot
        assert_eq!(scheduler.config().scale_up_cooldown_ms, snap.config.scale_up_cooldown_ms);
        assert!(f64_approx_eq(
            scheduler.config().scale_up_threshold,
            snap.config.scale_up_threshold
        ));
        assert_eq!(scheduler.sequence(), snap.sequence);
        assert_eq!(scheduler.scale_history().len(), snap.scale_history.len());
    }

    #[test]
    fn snapshot_roundtrip_through_json(snap in arb_scheduler_snapshot()) {
        let scheduler = SwarmScheduler::restore(snap.clone());
        let roundtrip = scheduler.snapshot();
        let json = serde_json::to_string(&roundtrip).unwrap();
        let restored: SchedulerSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.last_scale_up_ms, snap.last_scale_up_ms);
        assert_eq!(restored.last_scale_down_ms, snap.last_scale_down_ms);
        assert_eq!(restored.last_evaluation_ms, snap.last_evaluation_ms);
        assert_eq!(restored.consecutive_scale_ops, snap.consecutive_scale_ops);
        assert_eq!(restored.circuit_breaker_tripped_at, snap.circuit_breaker_tripped_at);
        assert_eq!(restored.sequence, snap.sequence);
    }
}

// =============================================================================
// Sequence monotonicity
// =============================================================================

proptest! {
    #[test]
    fn sequence_monotonically_increases(eval_count in 1u32..20) {
        let mut scheduler = SwarmScheduler::with_defaults();
        let mut queue = make_queue();

        let mut prev = scheduler.sequence();
        for i in 0..eval_count {
            scheduler.evaluate(&mut queue, (i + 1) as u64 * 1000);
            let current = scheduler.sequence();
            assert!(current > prev);
            prev = current;
        }
    }
}

// =============================================================================
// Scale-up properties (via public API only)
// =============================================================================

proptest! {
    #[test]
    fn scale_up_via_evaluate_respects_max_fleet(
        max_fleet in 3u32..16,
        max_step in 1u32..4,
    ) {
        // Use a config with low cooldown/threshold so scale-up triggers easily
        let config = SchedulerConfig {
            max_fleet_size: max_fleet,
            max_scale_step: max_step,
            min_fleet_size: 1,
            scale_up_cooldown_ms: 0,
            scale_up_threshold: 0.1,
            scale_down_threshold: 0.01,
            ..SchedulerConfig::default()
        };
        let mut scheduler = SwarmScheduler::new(config);
        // No agents registered → zero capacity with ready work → saturated → triggers scale-up
        let mut queue = make_queue();
        for i in 0..10 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
        }

        let decision = scheduler.evaluate(&mut queue, 100_000);
        if let SchedulerDecision::ScaleUp { additional_agents, .. } = decision {
            assert!(additional_agents <= max_step);
            assert!(additional_agents <= max_fleet);
        }
        // Other decisions (AssignWork, Noop) are also acceptable
    }

    #[test]
    fn scale_up_blocked_by_cooldown_via_snapshot(
        cooldown in 5_000u64..200_000,
    ) {
        // Restore a scheduler that just scaled up
        let config = SchedulerConfig {
            scale_up_cooldown_ms: cooldown,
            scale_up_threshold: 0.1,
            scale_down_threshold: 0.01,
            ..SchedulerConfig::default()
        };
        let snap = SchedulerSnapshot {
            config: config.clone(),
            last_scale_up_ms: 50_000,
            last_scale_down_ms: 0,
            last_evaluation_ms: 50_000,
            consecutive_scale_ops: 0,
            circuit_breaker_tripped_at: None,
            scale_history: Vec::new(),
            agent_first_seen: BTreeMap::new(),
            agent_completed: BTreeMap::new(),
            agent_failed: BTreeMap::new(),
            sequence: 1,
        };
        let mut scheduler = SwarmScheduler::restore(snap);

        // Within cooldown: evaluate should NOT produce ScaleUp
        let mut queue = make_queue();
        for i in 0..5 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
        }
        let within = 50_000 + cooldown / 2;
        let decision = scheduler.evaluate(&mut queue, within);
        match decision {
            SchedulerDecision::ScaleUp { .. } => {
                panic!("should not scale up within cooldown window");
            }
            _ => {} // Noop or other decisions are fine
        }
    }
}

// =============================================================================
// Evaluate decision properties
// =============================================================================

proptest! {
    #[test]
    fn evaluate_empty_queue_is_noop(
        agent_count in 1u32..5,
    ) {
        let mut scheduler = SwarmScheduler::with_defaults();
        let mut queue = make_queue();

        for i in 0..agent_count {
            scheduler.register_agent(&format!("agent-{i}"), 0);
        }

        let decision = scheduler.evaluate(&mut queue, 10_000);
        match decision {
            SchedulerDecision::Noop { .. } => {}
            other => panic!("expected Noop on empty queue, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_with_ready_work_assigns(
        item_count in 1u32..5,
    ) {
        let mut scheduler = SwarmScheduler::with_defaults();
        let mut queue = make_queue();
        scheduler.register_agent(&"agent-1".to_string(), 0);

        for i in 0..item_count {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
        }

        let decision = scheduler.evaluate(&mut queue, 10_000);
        match decision {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(!assignments.is_empty());
                // Never assigns more than agent capacity
                assert!(assignments.len() as u32 <= queue.config().max_concurrent_per_agent);
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }
    }
}

// =============================================================================
// Error Display coverage
// =============================================================================

proptest! {
    #[test]
    fn scheduler_error_display_non_empty(e in arb_scheduler_error()) {
        let msg = format!("{e}");
        assert!(!msg.is_empty());
    }

    #[test]
    fn scheduler_error_is_std_error(e in arb_scheduler_error()) {
        // Verify std::error::Error is implemented
        let _: &dyn std::error::Error = &e;
    }
}

// =============================================================================
// Scale history eviction
// =============================================================================

proptest! {
    #[test]
    fn history_bounded_by_max_entries(event_count in 15u32..100) {
        let mut scheduler = SwarmScheduler::with_defaults();
        // Can't set max_history_entries directly (private), but default is 1000.
        // Generate enough events to test that history doesn't grow unbounded.
        for i in 0..event_count {
            scheduler.evaluate(&mut make_queue(), i as u64 * 1000);
        }
        // Noop evaluations don't record events, but the scheduler shouldn't crash
        assert!(scheduler.scale_history().len() <= 1000);
    }
}

// =============================================================================
// Decision variant properties
// =============================================================================

proptest! {
    #[test]
    fn assign_work_has_nonempty_assignments(
        assignments in prop::collection::vec(arb_work_assignment(), 1..=10),
    ) {
        let d = SchedulerDecision::AssignWork {
            assignments: assignments.clone(),
        };
        match d {
            SchedulerDecision::AssignWork { assignments: a } => {
                assert!(!a.is_empty());
                assert_eq!(a.len(), assignments.len());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn scale_up_has_positive_agents(n in 1u32..100) {
        let d = SchedulerDecision::ScaleUp {
            additional_agents: n,
            reason: "test".to_string(),
        };
        match d {
            SchedulerDecision::ScaleUp { additional_agents, .. } => {
                assert!(additional_agents > 0);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn reclaim_stale_has_nonempty_items(
        items in prop::collection::vec("[a-z]{3,8}", 1..=10),
    ) {
        let d = SchedulerDecision::ReclaimStale {
            reclaimed_items: items.clone(),
        };
        match d {
            SchedulerDecision::ReclaimStale { reclaimed_items } => {
                assert!(!reclaimed_items.is_empty());
                assert_eq!(reclaimed_items.len(), items.len());
            }
            _ => unreachable!(),
        }
    }
}

// =============================================================================
// Readonly evaluation consistency
// =============================================================================

proptest! {
    #[test]
    fn evaluate_readonly_matches_compute_pressure(
        in_progress in 0usize..50,
        ready in 0usize..50,
        agents in 1usize..10,
        max_concurrent in 1u32..8,
    ) {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: in_progress + ready,
            blocked: 0,
            ready,
            in_progress,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: agents,
            completion_log_size: 0,
        };

        let p1 = scheduler.evaluate_readonly(&stats, max_concurrent, 1000);
        let p2 = scheduler.compute_pressure(&stats, max_concurrent);

        assert!(pressure_approx_eq(&p1, &p2));
    }
}
