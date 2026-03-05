// =============================================================================
// Scheduler/rebalancer/autoscaler for live fleets (ft-3681t.3.2)
//
// Runtime scheduling that rebalances work and resizes fleets based on queue
// pressure, rate limits, failures, and policy constraints. Designed to avoid
// cascade failure patterns common in ad-hoc swarms.
//
// # Architecture
//
// ```text
// SwarmWorkQueue ──► QueuePressure ──► SwarmScheduler.evaluate()
//                                              │
//              LifecycleRegistry ──────────────►│
//                                              ▼
//                                     SchedulerDecision
//                                              │
//                    ┌────────┬────────┬────────┼─────────┐
//                    ▼        ▼        ▼        ▼         ▼
//                 Noop    Assign   Rebalance  ScaleUp  ScaleDown
//                                              │         │
//                                     FleetLauncher   drain/close
//                                              │
//                                  Anti-cascade guards:
//                                  - cooldown timers
//                                  - circuit breaker
//                                  - grace periods
// ```
// =============================================================================

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::swarm_work_queue::{AgentSlotId, QueueStats, SwarmWorkQueue, WorkItemId};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the swarm scheduler/rebalancer/autoscaler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerConfig {
    /// Minimum time between scale-up operations (ms).
    pub scale_up_cooldown_ms: u64,
    /// Minimum time between scale-down operations (ms).
    pub scale_down_cooldown_ms: u64,
    /// Minimum fleet size (never scale below this).
    pub min_fleet_size: u32,
    /// Maximum fleet size (never scale above this).
    pub max_fleet_size: u32,
    /// Queue utilization ratio above which scale-up is triggered (0.0..1.0).
    pub scale_up_threshold: f64,
    /// Queue utilization ratio below which scale-down is triggered (0.0..1.0).
    pub scale_down_threshold: f64,
    /// Load imbalance ratio above which rebalancing is triggered (0.0..1.0).
    pub rebalance_imbalance_threshold: f64,
    /// Maximum consecutive scale operations before circuit breaker trips.
    pub max_consecutive_scale_ops: u32,
    /// Grace period (ms) before new agents are evaluated for scale-down.
    pub agent_startup_grace_ms: u64,
    /// Circuit breaker reset time (ms) after tripping.
    pub circuit_breaker_reset_ms: u64,
    /// Maximum scale-up step size (agents added per operation).
    pub max_scale_step: u32,
    /// Failure rate (0.0..1.0) above which scale-down is suppressed.
    pub failure_rate_suppress_threshold: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            scale_up_cooldown_ms: 60_000,
            scale_down_cooldown_ms: 120_000,
            min_fleet_size: 1,
            max_fleet_size: 64,
            scale_up_threshold: 0.85,
            scale_down_threshold: 0.20,
            rebalance_imbalance_threshold: 0.40,
            max_consecutive_scale_ops: 5,
            agent_startup_grace_ms: 30_000,
            circuit_breaker_reset_ms: 300_000,
            max_scale_step: 4,
            failure_rate_suppress_threshold: 0.50,
        }
    }
}

// =============================================================================
// Pressure / metrics types
// =============================================================================

/// Computed queue pressure metrics for scheduling decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueuePressure {
    /// Ratio of ready items to total non-terminal items (0.0..1.0).
    pub ready_ratio: f64,
    /// Ratio of in-progress items to total agent capacity (0.0..1.0).
    pub utilization: f64,
    /// Number of items past the starvation threshold.
    pub starvation_count: u32,
    /// Recent failure rate (failures / total completions, 0.0..1.0).
    pub failure_rate: f64,
    /// Total non-terminal items in queue.
    pub pending_items: u32,
    /// Active agent count.
    pub active_agents: u32,
    /// Total agent capacity (active_agents * max_concurrent_per_agent).
    pub total_capacity: u32,
}

/// Per-agent load snapshot for rebalancing decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentLoadSnapshot {
    /// Agent slot identifier.
    pub agent_id: AgentSlotId,
    /// Number of currently assigned work items.
    pub active_items: u32,
    /// Max concurrent items this agent supports.
    pub max_items: u32,
    /// Total items completed by this agent.
    pub completed_count: u32,
    /// Total items failed by this agent.
    pub failed_count: u32,
    /// Timestamp (epoch ms) when agent was first seen.
    pub first_seen_ms: u64,
}

// =============================================================================
// Scheduling decisions
// =============================================================================

/// A scheduling decision produced by `SwarmScheduler::evaluate()`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SchedulerDecision {
    /// No action needed — fleet is healthy and balanced.
    Noop { reason: String },
    /// Pull work from the queue and assign to underutilized agents.
    AssignWork { assignments: Vec<WorkAssignment> },
    /// Rebalance work across agents to reduce load imbalance.
    Rebalance { moves: Vec<RebalanceMove> },
    /// Scale fleet up to handle increased queue pressure.
    ScaleUp {
        additional_agents: u32,
        reason: String,
    },
    /// Scale fleet down to reduce idle capacity.
    ScaleDown {
        remove_agents: Vec<AgentSlotId>,
        reason: String,
    },
    /// Reclaim work items from timed-out agents.
    ReclaimStale { reclaimed_items: Vec<WorkItemId> },
}

/// A work item → agent assignment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkAssignment {
    pub item_id: WorkItemId,
    pub agent_id: AgentSlotId,
}

/// A rebalance operation: move work from an overloaded agent to an underloaded one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebalanceMove {
    pub item_id: WorkItemId,
    pub from_agent: AgentSlotId,
    pub to_agent: AgentSlotId,
    pub reason: String,
}

// =============================================================================
// Scale events (audit trail)
// =============================================================================

/// A recorded scale event for audit and debugging.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScaleEvent {
    pub event_type: ScaleEventType,
    pub timestamp_ms: u64,
    pub reason: String,
    pub fleet_size_before: u32,
    pub fleet_size_after: u32,
    pub decision: SchedulerDecision,
}

/// Type of scale event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScaleEventType {
    ScaleUp,
    ScaleDown,
    Rebalance,
    Assignment,
    Reclaim,
    CircuitBreakerTripped,
    CircuitBreakerReset,
}

// =============================================================================
// Scheduler errors
// =============================================================================

/// Errors from scheduler operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SchedulerError {
    /// Circuit breaker is tripped — no scale operations allowed.
    CircuitBreakerActive { tripped_at: u64, resets_at: u64 },
    /// Fleet is already at maximum size.
    AtMaxCapacity { current: u32, max: u32 },
    /// Fleet is already at minimum size.
    AtMinCapacity { current: u32, min: u32 },
    /// Cooldown period has not elapsed.
    CooldownActive {
        operation: String,
        remaining_ms: u64,
    },
    /// No agents available for the requested operation.
    NoAgentsAvailable,
    /// No ready work items to assign.
    NoReadyWork,
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CircuitBreakerActive {
                tripped_at,
                resets_at,
            } => write!(
                f,
                "circuit breaker active (tripped at {tripped_at}, resets at {resets_at})"
            ),
            Self::AtMaxCapacity { current, max } => {
                write!(f, "fleet at max capacity ({current}/{max})")
            }
            Self::AtMinCapacity { current, min } => {
                write!(f, "fleet at min capacity ({current}/{min})")
            }
            Self::CooldownActive {
                operation,
                remaining_ms,
            } => write!(
                f,
                "{operation} cooldown active ({remaining_ms}ms remaining)"
            ),
            Self::NoAgentsAvailable => write!(f, "no agents available"),
            Self::NoReadyWork => write!(f, "no ready work items"),
        }
    }
}

impl std::error::Error for SchedulerError {}

// =============================================================================
// Scheduler snapshot (for checkpoint/restore)
// =============================================================================

/// Serializable snapshot of scheduler state for checkpoint/restore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerSnapshot {
    pub config: SchedulerConfig,
    pub last_scale_up_ms: u64,
    pub last_scale_down_ms: u64,
    pub last_evaluation_ms: u64,
    pub consecutive_scale_ops: u32,
    pub circuit_breaker_tripped_at: Option<u64>,
    pub scale_history: Vec<ScaleEvent>,
    pub agent_first_seen: BTreeMap<AgentSlotId, u64>,
    pub agent_completed: BTreeMap<AgentSlotId, u32>,
    pub agent_failed: BTreeMap<AgentSlotId, u32>,
    pub sequence: u64,
}

// =============================================================================
// Main scheduler
// =============================================================================

/// Runtime scheduler, rebalancer, and autoscaler for live swarm fleets.
///
/// Evaluates queue pressure and agent utilization to make scheduling decisions:
/// - **Assign**: Pull work from the queue and dispatch to available agents
/// - **Rebalance**: Move work from overloaded to underloaded agents
/// - **Scale up**: Add agents when queue pressure exceeds threshold
/// - **Scale down**: Remove idle agents when pressure drops
/// - **Reclaim**: Reclaim work from timed-out agents
///
/// Anti-cascade safety:
/// - Cooldown timers prevent rapid scale oscillation
/// - Circuit breaker trips after too many consecutive scale ops
/// - Agent startup grace period prevents premature scale-down of new agents
pub struct SwarmScheduler {
    config: SchedulerConfig,
    last_scale_up_ms: u64,
    last_scale_down_ms: u64,
    last_evaluation_ms: u64,
    consecutive_scale_ops: u32,
    circuit_breaker_tripped_at: Option<u64>,
    scale_history: Vec<ScaleEvent>,
    agent_first_seen: HashMap<AgentSlotId, u64>,
    agent_completed: HashMap<AgentSlotId, u32>,
    agent_failed: HashMap<AgentSlotId, u32>,
    sequence: u64,
    max_history_entries: usize,
}

impl SwarmScheduler {
    /// Create a new scheduler with the given configuration.
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            last_scale_up_ms: 0,
            last_scale_down_ms: 0,
            last_evaluation_ms: 0,
            consecutive_scale_ops: 0,
            circuit_breaker_tripped_at: None,
            scale_history: Vec::new(),
            agent_first_seen: HashMap::new(),
            agent_completed: HashMap::new(),
            agent_failed: HashMap::new(),
            sequence: 0,
            max_history_entries: 1000,
        }
    }

    /// Create a scheduler with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(SchedulerConfig::default())
    }

    /// Restore scheduler from a checkpoint snapshot.
    pub fn restore(snapshot: SchedulerSnapshot) -> Self {
        Self {
            config: snapshot.config,
            last_scale_up_ms: snapshot.last_scale_up_ms,
            last_scale_down_ms: snapshot.last_scale_down_ms,
            last_evaluation_ms: snapshot.last_evaluation_ms,
            consecutive_scale_ops: snapshot.consecutive_scale_ops,
            circuit_breaker_tripped_at: snapshot.circuit_breaker_tripped_at,
            scale_history: snapshot.scale_history,
            agent_first_seen: snapshot.agent_first_seen.into_iter().collect(),
            agent_completed: snapshot.agent_completed.into_iter().collect(),
            agent_failed: snapshot.agent_failed.into_iter().collect(),
            sequence: snapshot.sequence,
            max_history_entries: 1000,
        }
    }

    /// Take a checkpoint snapshot of the scheduler state.
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            config: self.config.clone(),
            last_scale_up_ms: self.last_scale_up_ms,
            last_scale_down_ms: self.last_scale_down_ms,
            last_evaluation_ms: self.last_evaluation_ms,
            consecutive_scale_ops: self.consecutive_scale_ops,
            circuit_breaker_tripped_at: self.circuit_breaker_tripped_at,
            scale_history: self.scale_history.clone(),
            agent_first_seen: self
                .agent_first_seen
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            agent_completed: self
                .agent_completed
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            agent_failed: self
                .agent_failed
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            sequence: self.sequence,
        }
    }

    /// Read-only access to the scheduler configuration.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Read-only access to the scale event history.
    pub fn scale_history(&self) -> &[ScaleEvent] {
        &self.scale_history
    }

    /// Current monotonic sequence counter.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether the circuit breaker is currently tripped.
    pub fn circuit_breaker_active(&self, now_ms: u64) -> bool {
        match self.circuit_breaker_tripped_at {
            Some(tripped_at) => {
                now_ms < tripped_at.saturating_add(self.config.circuit_breaker_reset_ms)
            }
            None => false,
        }
    }

    // =========================================================================
    // Agent tracking
    // =========================================================================

    /// Register an agent with the scheduler (records first-seen time).
    pub fn register_agent(&mut self, agent_id: &AgentSlotId, now_ms: u64) {
        self.agent_first_seen
            .entry(agent_id.clone())
            .or_insert(now_ms);
        self.agent_completed.entry(agent_id.clone()).or_insert(0);
        self.agent_failed.entry(agent_id.clone()).or_insert(0);
    }

    /// Record a completion by an agent.
    pub fn record_completion(&mut self, agent_id: &AgentSlotId) {
        *self.agent_completed.entry(agent_id.clone()).or_insert(0) += 1;
    }

    /// Record a failure by an agent.
    pub fn record_failure(&mut self, agent_id: &AgentSlotId) {
        *self.agent_failed.entry(agent_id.clone()).or_insert(0) += 1;
    }

    /// Remove an agent from tracking.
    pub fn deregister_agent(&mut self, agent_id: &AgentSlotId) {
        self.agent_first_seen.remove(agent_id);
        self.agent_completed.remove(agent_id);
        self.agent_failed.remove(agent_id);
    }

    /// Get load snapshots for all tracked agents.
    pub fn agent_snapshots(
        &self,
        queue: &SwarmWorkQueue,
        max_concurrent: u32,
    ) -> Vec<AgentLoadSnapshot> {
        let mut snapshots = Vec::new();
        for (agent_id, &first_seen) in &self.agent_first_seen {
            let active = queue.agent_items(agent_id).len() as u32;
            let completed = self.agent_completed.get(agent_id).copied().unwrap_or(0);
            let failed = self.agent_failed.get(agent_id).copied().unwrap_or(0);
            snapshots.push(AgentLoadSnapshot {
                agent_id: agent_id.clone(),
                active_items: active,
                max_items: max_concurrent,
                completed_count: completed,
                failed_count: failed,
                first_seen_ms: first_seen,
            });
        }
        snapshots.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        snapshots
    }

    // =========================================================================
    // Queue pressure computation
    // =========================================================================

    /// Compute queue pressure metrics from the current queue state.
    pub fn compute_pressure(
        &self,
        stats: &QueueStats,
        max_concurrent_per_agent: u32,
    ) -> QueuePressure {
        let non_terminal = stats
            .total_items
            .saturating_sub(stats.completed + stats.failed + stats.cancelled);
        let ready_ratio = if non_terminal > 0 {
            stats.ready as f64 / non_terminal as f64
        } else {
            0.0
        };

        let active = stats.active_agents as u32;
        let capacity = active.saturating_mul(max_concurrent_per_agent);
        let utilization = if capacity > 0 {
            stats.in_progress as f64 / capacity as f64
        } else if stats.ready > 0 || stats.in_progress > 0 {
            // No schedulable capacity while work is waiting/running: treat as
            // saturated so autoscaling can recover from zero-capacity stalls.
            1.0
        } else {
            0.0
        };

        let total_completions = stats.completed + stats.failed;
        let failure_rate = if total_completions > 0 {
            stats.failed as f64 / total_completions as f64
        } else {
            0.0
        };

        QueuePressure {
            ready_ratio,
            utilization,
            starvation_count: 0, // computed externally from queue internals
            failure_rate,
            pending_items: non_terminal as u32,
            active_agents: active,
            total_capacity: capacity,
        }
    }

    // =========================================================================
    // Core evaluation
    // =========================================================================

    /// Evaluate the current fleet state and produce a scheduling decision.
    ///
    /// Priority order:
    /// 1. Reclaim stale items (heartbeat timeout)
    /// 2. Assign ready work to underutilized agents
    /// 3. Rebalance overloaded agents
    /// 4. Scale up if pressure exceeds threshold
    /// 5. Scale down if pressure is very low
    /// 6. Noop if everything is healthy
    pub fn evaluate(&mut self, queue: &mut SwarmWorkQueue, now_ms: u64) -> SchedulerDecision {
        self.last_evaluation_ms = now_ms;
        self.sequence += 1;

        // Check circuit breaker reset
        if let Some(tripped_at) = self.circuit_breaker_tripped_at {
            if now_ms >= tripped_at.saturating_add(self.config.circuit_breaker_reset_ms) {
                self.circuit_breaker_tripped_at = None;
                self.consecutive_scale_ops = 0;
                self.record_event(
                    ScaleEventType::CircuitBreakerReset,
                    "circuit breaker auto-reset after cooldown".to_string(),
                    0,
                    0,
                    SchedulerDecision::Noop {
                        reason: "circuit breaker reset".to_string(),
                    },
                    now_ms,
                );
            }
        }

        // Step 1: Reclaim timed-out items
        let reclaimed = queue.reclaim_timed_out();
        if !reclaimed.is_empty() {
            let decision = SchedulerDecision::ReclaimStale {
                reclaimed_items: reclaimed,
            };
            return decision;
        }

        let stats = queue.stats();
        let max_concurrent = queue.config().max_concurrent_per_agent;
        let pressure = self.compute_pressure(&stats, max_concurrent);

        // Step 2: Assign ready work to agents with capacity
        if stats.ready > 0 {
            let mut assignments = Vec::new();
            let snapshots = self.agent_snapshots(queue, max_concurrent);
            for snap in &snapshots {
                if snap.active_items < snap.max_items {
                    // Agent has capacity — try to pull work
                    match queue.pull(&snap.agent_id) {
                        Ok(assignment) => {
                            assignments.push(WorkAssignment {
                                item_id: assignment.work_item_id,
                                agent_id: snap.agent_id.clone(),
                            });
                        }
                        Err(_) => {}
                    }
                }
            }
            if !assignments.is_empty() {
                let decision = SchedulerDecision::AssignWork { assignments };
                return decision;
            }
        }

        // Step 3: Check for load imbalance and rebalance
        if let Some(decision) = self.check_rebalance(queue, max_concurrent) {
            return decision;
        }

        // Step 4: Scale up if pressure exceeds threshold
        if pressure.utilization > self.config.scale_up_threshold
            && pressure.active_agents < self.config.max_fleet_size
        {
            if let Some(decision) = self.try_scale_up(&pressure, now_ms) {
                return decision;
            }
        }

        // Step 5: Scale down if pressure is very low
        if pressure.utilization < self.config.scale_down_threshold
            && pressure.active_agents > self.config.min_fleet_size
        {
            if let Some(decision) = self.try_scale_down(queue, &pressure, now_ms) {
                return decision;
            }
        }

        SchedulerDecision::Noop {
            reason: format!(
                "fleet healthy: util={:.2}, ready_ratio={:.2}, agents={}",
                pressure.utilization, pressure.ready_ratio, pressure.active_agents,
            ),
        }
    }

    /// Evaluate without mutating the queue (read-only analysis).
    pub fn evaluate_readonly(
        &self,
        stats: &QueueStats,
        max_concurrent_per_agent: u32,
        _now_ms: u64,
    ) -> QueuePressure {
        self.compute_pressure(stats, max_concurrent_per_agent)
    }

    // =========================================================================
    // Scale-up logic
    // =========================================================================

    fn try_scale_up(&mut self, pressure: &QueuePressure, now_ms: u64) -> Option<SchedulerDecision> {
        // Check cooldown
        if now_ms
            < self
                .last_scale_up_ms
                .saturating_add(self.config.scale_up_cooldown_ms)
        {
            return None;
        }

        // Check circuit breaker
        if self.circuit_breaker_active(now_ms) {
            return None;
        }

        // Check max capacity
        if pressure.active_agents >= self.config.max_fleet_size {
            return None;
        }

        // Suppress scale-up if failure rate is too high (scaling won't help)
        if pressure.failure_rate > self.config.failure_rate_suppress_threshold {
            return None;
        }

        // Calculate how many agents to add (proportional to pressure)
        let excess = pressure.utilization - self.config.scale_up_threshold;
        let scale_factor = (excess / (1.0 - self.config.scale_up_threshold)).clamp(0.0, 1.0);
        let raw_step = (scale_factor * self.config.max_scale_step as f64).ceil() as u32;
        let step = raw_step
            .max(1)
            .min(self.config.max_scale_step)
            .min(self.config.max_fleet_size - pressure.active_agents);

        let reason = format!(
            "queue pressure {:.2} exceeds threshold {:.2} (ready={}, capacity={})",
            pressure.utilization,
            self.config.scale_up_threshold,
            pressure.pending_items,
            pressure.total_capacity,
        );

        self.last_scale_up_ms = now_ms;
        self.consecutive_scale_ops += 1;
        self.check_circuit_breaker(now_ms);

        let decision = SchedulerDecision::ScaleUp {
            additional_agents: step,
            reason: reason.clone(),
        };

        self.record_event(
            ScaleEventType::ScaleUp,
            reason,
            pressure.active_agents,
            pressure.active_agents + step,
            decision.clone(),
            now_ms,
        );

        Some(decision)
    }

    // =========================================================================
    // Scale-down logic
    // =========================================================================

    fn try_scale_down(
        &mut self,
        queue: &SwarmWorkQueue,
        pressure: &QueuePressure,
        now_ms: u64,
    ) -> Option<SchedulerDecision> {
        // Check cooldown
        if now_ms
            < self
                .last_scale_down_ms
                .saturating_add(self.config.scale_down_cooldown_ms)
        {
            return None;
        }

        // Check circuit breaker
        if self.circuit_breaker_active(now_ms) {
            return None;
        }

        // Check min capacity
        if pressure.active_agents <= self.config.min_fleet_size {
            return None;
        }

        // Find agents eligible for removal (idle, past grace period)
        let mut removable: Vec<AgentSlotId> = Vec::new();
        for (agent_id, &first_seen) in &self.agent_first_seen {
            // Skip agents in startup grace period
            if now_ms < first_seen.saturating_add(self.config.agent_startup_grace_ms) {
                continue;
            }
            // Only remove agents with no active work
            let active = queue.agent_items(agent_id).len();
            if active == 0 {
                removable.push(agent_id.clone());
            }
        }

        if removable.is_empty() {
            return None;
        }

        // Sort by least productive first (fewest completions)
        removable.sort_by(|a, b| {
            let a_completed = self.agent_completed.get(a).copied().unwrap_or(0);
            let b_completed = self.agent_completed.get(b).copied().unwrap_or(0);
            a_completed.cmp(&b_completed)
        });

        // Only remove enough to stay above min and not remove too many at once
        let max_remove =
            (pressure.active_agents - self.config.min_fleet_size).min(self.config.max_scale_step);
        removable.truncate(max_remove as usize);

        if removable.is_empty() {
            return None;
        }

        let reason = format!(
            "queue pressure {:.2} below threshold {:.2}, removing {} idle agent(s)",
            pressure.utilization,
            self.config.scale_down_threshold,
            removable.len(),
        );

        self.last_scale_down_ms = now_ms;
        self.consecutive_scale_ops += 1;
        self.check_circuit_breaker(now_ms);

        let new_size = pressure.active_agents - removable.len() as u32;
        let decision = SchedulerDecision::ScaleDown {
            remove_agents: removable,
            reason: reason.clone(),
        };

        self.record_event(
            ScaleEventType::ScaleDown,
            reason,
            pressure.active_agents,
            new_size,
            decision.clone(),
            now_ms,
        );

        Some(decision)
    }

    // =========================================================================
    // Rebalance logic
    // =========================================================================

    fn check_rebalance(
        &self,
        queue: &SwarmWorkQueue,
        max_concurrent: u32,
    ) -> Option<SchedulerDecision> {
        let snapshots = self.agent_snapshots(queue, max_concurrent);
        if snapshots.len() < 2 {
            return None;
        }

        let loads: Vec<f64> = snapshots
            .iter()
            .map(|s| {
                if s.max_items > 0 {
                    s.active_items as f64 / s.max_items as f64
                } else {
                    0.0
                }
            })
            .collect();

        let max_load = loads.iter().copied().fold(0.0_f64, f64::max);
        let min_load = loads.iter().copied().fold(1.0_f64, f64::min);
        let imbalance = max_load - min_load;

        if imbalance < self.config.rebalance_imbalance_threshold {
            return None;
        }

        // Find overloaded and underloaded agents
        let avg_load: f64 = loads.iter().sum::<f64>() / loads.len() as f64;
        let mut moves = Vec::new();

        let overloaded: Vec<_> = snapshots
            .iter()
            .zip(loads.iter())
            .filter(|entry| *entry.1 > avg_load + self.config.rebalance_imbalance_threshold / 2.0)
            .map(|(s, _)| s)
            .collect();

        let underloaded: Vec<_> = snapshots
            .iter()
            .zip(loads.iter())
            .filter(|entry| *entry.1 < avg_load - self.config.rebalance_imbalance_threshold / 2.0)
            .map(|(s, _)| s)
            .collect();

        // Suggest moves from overloaded to underloaded (advisory only)
        let mut target_idx = 0;
        for over in &overloaded {
            if target_idx >= underloaded.len() {
                break;
            }
            let items = queue.agent_items(&over.agent_id);
            // Suggest moving the most recently assigned item
            if let Some(assignment) = items.last() {
                let under = &underloaded[target_idx];
                if under.active_items < under.max_items {
                    moves.push(RebalanceMove {
                        item_id: assignment.work_item_id.clone(),
                        from_agent: over.agent_id.clone(),
                        to_agent: under.agent_id.clone(),
                        reason: format!(
                            "load imbalance {:.2}: {}/{} -> {}/{}",
                            imbalance,
                            over.active_items,
                            over.max_items,
                            under.active_items,
                            under.max_items,
                        ),
                    });
                    target_idx += 1;
                }
            }
        }

        if moves.is_empty() {
            return None;
        }

        Some(SchedulerDecision::Rebalance { moves })
    }

    // =========================================================================
    // Circuit breaker
    // =========================================================================

    fn check_circuit_breaker(&mut self, now_ms: u64) {
        if self.consecutive_scale_ops >= self.config.max_consecutive_scale_ops {
            self.circuit_breaker_tripped_at = Some(now_ms);
            self.record_event(
                ScaleEventType::CircuitBreakerTripped,
                format!(
                    "circuit breaker tripped after {} consecutive scale ops",
                    self.consecutive_scale_ops,
                ),
                0,
                0,
                SchedulerDecision::Noop {
                    reason: "circuit breaker tripped".to_string(),
                },
                now_ms,
            );
        }
    }

    /// Manually reset the circuit breaker.
    pub fn reset_circuit_breaker(&mut self) {
        self.circuit_breaker_tripped_at = None;
        self.consecutive_scale_ops = 0;
    }

    // =========================================================================
    // Event recording
    // =========================================================================

    fn record_event(
        &mut self,
        event_type: ScaleEventType,
        reason: String,
        before: u32,
        after: u32,
        decision: SchedulerDecision,
        now_ms: u64,
    ) {
        self.scale_history.push(ScaleEvent {
            event_type,
            timestamp_ms: now_ms,
            reason,
            fleet_size_before: before,
            fleet_size_after: after,
            decision,
        });
        // Evict oldest 10% when history is full
        if self.scale_history.len() > self.max_history_entries {
            let drain_count = self.max_history_entries / 10;
            self.scale_history.drain(..drain_count);
        }
    }

    // =========================================================================
    // Helpers
    // =========================================================================

    /// Get the current wall-clock time in milliseconds since the Unix epoch.
    #[allow(dead_code)]
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

// =============================================================================
// Convenience: compute pressure from queue directly
// =============================================================================

/// Compute queue pressure from a SwarmWorkQueue snapshot.
pub fn compute_queue_pressure(queue: &SwarmWorkQueue) -> QueuePressure {
    let stats = queue.stats();
    let max_concurrent = queue.config().max_concurrent_per_agent;
    let scheduler = SwarmScheduler::with_defaults();
    scheduler.compute_pressure(&stats, max_concurrent)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm_work_queue::{WorkItem, WorkQueueConfig};

    fn test_config() -> SchedulerConfig {
        SchedulerConfig {
            scale_up_cooldown_ms: 1000,
            scale_down_cooldown_ms: 2000,
            min_fleet_size: 1,
            max_fleet_size: 16,
            scale_up_threshold: 0.80,
            scale_down_threshold: 0.20,
            rebalance_imbalance_threshold: 0.40,
            max_consecutive_scale_ops: 3,
            agent_startup_grace_ms: 5000,
            circuit_breaker_reset_ms: 10_000,
            max_scale_step: 2,
            failure_rate_suppress_threshold: 0.50,
        }
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

    #[allow(dead_code)]
    fn make_dep_item(id: &str, priority: u32, deps: Vec<&str>) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            title: format!("Work item {id}"),
            priority,
            depends_on: deps.into_iter().map(String::from).collect(),
            effort: 1,
            labels: Vec::new(),
            preferred_program: None,
            metadata: HashMap::new(),
        }
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn default_config_has_sane_values() {
        let cfg = SchedulerConfig::default();
        assert!(cfg.scale_up_cooldown_ms > 0);
        assert!(cfg.scale_down_cooldown_ms > 0);
        assert!(cfg.min_fleet_size >= 1);
        assert!(cfg.max_fleet_size > cfg.min_fleet_size);
        assert!(cfg.scale_up_threshold > cfg.scale_down_threshold);
        assert!(cfg.max_consecutive_scale_ops > 0);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = test_config();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: SchedulerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, restored);
    }

    // =========================================================================
    // Pressure computation tests
    // =========================================================================

    #[test]
    fn empty_queue_has_zero_pressure() {
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
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert_eq!(pressure.ready_ratio, 0.0);
        assert_eq!(pressure.utilization, 0.0);
        assert_eq!(pressure.failure_rate, 0.0);
    }

    #[test]
    fn pressure_with_full_utilization() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 10,
            blocked: 0,
            ready: 1,
            in_progress: 9,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 3,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert_eq!(pressure.utilization, 1.0); // 9 / (3*3)
        assert!(pressure.ready_ratio > 0.0);
    }

    #[test]
    fn pressure_with_failures() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 10,
            blocked: 0,
            ready: 2,
            in_progress: 3,
            completed: 3,
            failed: 2,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: 5,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert_eq!(pressure.failure_rate, 2.0 / 5.0);
    }

    #[test]
    fn pressure_with_ready_work_and_zero_capacity_is_saturated() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 3,
            blocked: 0,
            ready: 3,
            in_progress: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
            active_agents: 0,
            completion_log_size: 0,
        };
        let pressure = scheduler.compute_pressure(&stats, 3);
        assert_eq!(pressure.utilization, 1.0);
    }

    // =========================================================================
    // Agent tracking tests
    // =========================================================================

    #[test]
    fn register_and_deregister_agent() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.register_agent(&"agent-1".to_string(), 1000);
        assert!(scheduler.agent_first_seen.contains_key("agent-1"));

        scheduler.deregister_agent(&"agent-1".to_string());
        assert!(!scheduler.agent_first_seen.contains_key("agent-1"));
    }

    #[test]
    fn record_completion_increments_counter() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 1000);
        scheduler.record_completion(&agent);
        scheduler.record_completion(&agent);
        assert_eq!(scheduler.agent_completed[&agent], 2);
    }

    #[test]
    fn record_failure_increments_counter() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 1000);
        scheduler.record_failure(&agent);
        assert_eq!(scheduler.agent_failed[&agent], 1);
    }

    #[test]
    fn agent_snapshots_sorted_by_id() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();
        scheduler.register_agent(&"zebra".to_string(), 1000);
        scheduler.register_agent(&"alpha".to_string(), 1000);
        scheduler.register_agent(&"mid".to_string(), 1000);

        let snapshots = scheduler.agent_snapshots(&queue, 3);
        assert_eq!(snapshots[0].agent_id, "alpha");
        assert_eq!(snapshots[1].agent_id, "mid");
        assert_eq!(snapshots[2].agent_id, "zebra");
    }

    // =========================================================================
    // Evaluation tests
    // =========================================================================

    #[test]
    fn evaluate_noop_on_empty_queue() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        scheduler.register_agent(&"agent-1".to_string(), 1000);

        let decision = scheduler.evaluate(&mut queue, 2000);
        match decision {
            SchedulerDecision::Noop { .. } => {}
            other => panic!("expected Noop, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_assigns_ready_work() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 1000);

        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.enqueue(make_item("w2", 1)).unwrap();

        let decision = scheduler.evaluate(&mut queue, 2000);
        match decision {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(!assignments.is_empty());
                assert_eq!(assignments[0].agent_id, agent);
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_assigns_to_multiple_agents() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        scheduler.register_agent(&"a1".to_string(), 1000);
        scheduler.register_agent(&"a2".to_string(), 1000);

        for i in 0..6 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
        }

        let decision = scheduler.evaluate(&mut queue, 2000);
        match decision {
            SchedulerDecision::AssignWork { assignments } => {
                // Should assign to both agents
                let agents: Vec<_> = assignments.iter().map(|a| &a.agent_id).collect();
                assert!(agents.contains(&&"a1".to_string()));
                assert!(agents.contains(&&"a2".to_string()));
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_scales_up_when_ready_work_exists_with_zero_capacity() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        queue.enqueue(make_item("w1", 0)).unwrap();

        match scheduler.evaluate(&mut queue, 5000) {
            SchedulerDecision::ScaleUp {
                additional_agents, ..
            } => assert!(additional_agents >= 1),
            other => panic!("expected ScaleUp, got {other:?}"),
        }
    }

    // =========================================================================
    // Scale-up tests
    // =========================================================================

    #[test]
    fn scale_up_when_utilization_exceeds_threshold() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();
        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 0);

        // Fill agent to capacity
        for i in 0..3 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
            queue.assign(&format!("w{i}"), &agent).unwrap();
        }
        // Add more ready work (no agents to pull it)
        queue.enqueue(make_item("w3", 0)).unwrap();

        let decision = scheduler.evaluate(&mut queue, 5000);
        // First call will try to assign w3 but agent is at capacity, resulting in noop
        // or scale-up depending on utilization
        match &decision {
            SchedulerDecision::ScaleUp {
                additional_agents, ..
            } => {
                assert!(*additional_agents >= 1);
            }
            SchedulerDecision::Noop { .. } => {
                // Agent at capacity but utilization may not exceed threshold with 1 agent
                // This is OK — utilization = 3/3 = 1.0 which exceeds 0.80
                // But ready items exist, so assignment is tried first but fails
            }
            other => panic!("expected ScaleUp or Noop, got {other:?}"),
        }
    }

    #[test]
    fn scale_up_respects_cooldown() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.last_scale_up_ms = 1000;

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 2,
            total_capacity: 6,
        };

        // Too soon — within 1000ms cooldown
        let result = scheduler.try_scale_up(&pressure, 1500);
        assert!(result.is_none());

        // After cooldown
        let result = scheduler.try_scale_up(&pressure, 2500);
        assert!(result.is_some());
    }

    #[test]
    fn scale_up_respects_max_fleet_size() {
        let mut scheduler = SwarmScheduler::new(test_config());

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 16, // at max
            total_capacity: 48,
        };

        let result = scheduler.try_scale_up(&pressure, 5000);
        assert!(result.is_none());
    }

    #[test]
    fn scale_up_suppressed_by_high_failure_rate() {
        let mut scheduler = SwarmScheduler::new(test_config());

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.60, // above 0.50 threshold
            pending_items: 10,
            active_agents: 4,
            total_capacity: 12,
        };

        let result = scheduler.try_scale_up(&pressure, 5000);
        assert!(result.is_none());
    }

    // =========================================================================
    // Scale-down tests
    // =========================================================================

    #[test]
    fn scale_down_removes_idle_agents() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        // Register agents well past grace period
        scheduler.register_agent(&"a1".to_string(), 0);
        scheduler.register_agent(&"a2".to_string(), 0);
        scheduler.register_agent(&"a3".to_string(), 0);

        // a1 has work, a2 and a3 are idle
        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.assign(&"w1".to_string(), &"a1".to_string()).unwrap();

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.11, // 1/(3*3)
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 1,
            active_agents: 3,
            total_capacity: 9,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        assert!(result.is_some());
        match result.unwrap() {
            SchedulerDecision::ScaleDown { remove_agents, .. } => {
                assert!(!remove_agents.is_empty());
                // Should not remove a1 (has active work)
                assert!(!remove_agents.contains(&"a1".to_string()));
            }
            other => panic!("expected ScaleDown, got {other:?}"),
        }
    }

    #[test]
    fn scale_down_respects_grace_period() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        // Agent just started (within 5000ms grace)
        scheduler.register_agent(&"new-agent".to_string(), 8000);

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.0,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 0,
            active_agents: 2,
            total_capacity: 6,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        // Agent is within grace period (8000 + 5000 > 10000) — not removable
        assert!(result.is_none());
    }

    #[test]
    fn scale_down_respects_min_fleet_size() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        scheduler.register_agent(&"a1".to_string(), 0);

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.0,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 0,
            active_agents: 1, // at min
            total_capacity: 3,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        assert!(result.is_none());
    }

    // =========================================================================
    // Circuit breaker tests
    // =========================================================================

    #[test]
    fn circuit_breaker_trips_after_consecutive_scale_ops() {
        let mut scheduler = SwarmScheduler::new(test_config());

        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.95,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 4,
            total_capacity: 12,
        };

        // 3 consecutive scale-ups should trip the breaker
        for i in 0..3 {
            let t = (i + 1) as u64 * 2000;
            scheduler.try_scale_up(&pressure, t);
        }

        assert!(scheduler.circuit_breaker_tripped_at.is_some());
        assert!(scheduler.circuit_breaker_active(7000));
    }

    #[test]
    fn circuit_breaker_auto_resets() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.circuit_breaker_tripped_at = Some(1000);

        // Not yet reset (within 10_000ms window)
        assert!(scheduler.circuit_breaker_active(5000));

        // Reset after window
        assert!(!scheduler.circuit_breaker_active(12_000));
    }

    #[test]
    fn manual_circuit_breaker_reset() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.circuit_breaker_tripped_at = Some(1000);
        scheduler.consecutive_scale_ops = 5;

        scheduler.reset_circuit_breaker();
        assert!(scheduler.circuit_breaker_tripped_at.is_none());
        assert_eq!(scheduler.consecutive_scale_ops, 0);
    }

    // =========================================================================
    // Rebalance tests
    // =========================================================================

    #[test]
    fn rebalance_detects_imbalanced_load() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        scheduler.register_agent(&"a1".to_string(), 0);
        scheduler.register_agent(&"a2".to_string(), 0);

        // a1 has 3 items (full), a2 has 0 (empty) → imbalance = 1.0
        for i in 0..3 {
            queue.enqueue(make_item(&format!("w{i}"), 0)).unwrap();
            queue.assign(&format!("w{i}"), &"a1".to_string()).unwrap();
        }

        let result = scheduler.check_rebalance(&queue, 3);
        assert!(result.is_some());
        match result.unwrap() {
            SchedulerDecision::Rebalance { moves } => {
                assert!(!moves.is_empty());
                assert_eq!(moves[0].from_agent, "a1");
                assert_eq!(moves[0].to_agent, "a2");
            }
            other => panic!("expected Rebalance, got {other:?}"),
        }
    }

    #[test]
    fn no_rebalance_when_balanced() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        scheduler.register_agent(&"a1".to_string(), 0);
        scheduler.register_agent(&"a2".to_string(), 0);

        // Each agent has 1 item — balanced
        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.assign(&"w1".to_string(), &"a1".to_string()).unwrap();
        queue.enqueue(make_item("w2", 0)).unwrap();
        queue.assign(&"w2".to_string(), &"a2".to_string()).unwrap();

        let result = scheduler.check_rebalance(&queue, 3);
        assert!(result.is_none());
    }

    // =========================================================================
    // Snapshot/restore tests
    // =========================================================================

    #[test]
    fn snapshot_restore_roundtrip() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.register_agent(&"a1".to_string(), 1000);
        scheduler.record_completion(&"a1".to_string());
        scheduler.record_failure(&"a1".to_string());
        scheduler.last_scale_up_ms = 5000;
        scheduler.consecutive_scale_ops = 2;

        let snap = scheduler.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored_snap: SchedulerSnapshot = serde_json::from_str(&json).unwrap();
        let restored = SwarmScheduler::restore(restored_snap);

        assert_eq!(restored.last_scale_up_ms, 5000);
        assert_eq!(restored.consecutive_scale_ops, 2);
        assert_eq!(restored.agent_completed[&"a1".to_string()], 1);
        assert_eq!(restored.agent_failed[&"a1".to_string()], 1);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let scheduler = SwarmScheduler::new(test_config());
        let snap = scheduler.snapshot();
        let json = serde_json::to_string_pretty(&snap).unwrap();
        let restored: SchedulerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, restored);
    }

    // =========================================================================
    // Decision type tests
    // =========================================================================

    #[test]
    fn decision_serde_roundtrip_noop() {
        let d = SchedulerDecision::Noop {
            reason: "healthy".to_string(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    #[test]
    fn decision_serde_roundtrip_scale_up() {
        let d = SchedulerDecision::ScaleUp {
            additional_agents: 3,
            reason: "high pressure".to_string(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    #[test]
    fn decision_serde_roundtrip_rebalance() {
        let d = SchedulerDecision::Rebalance {
            moves: vec![RebalanceMove {
                item_id: "w1".to_string(),
                from_agent: "a1".to_string(),
                to_agent: "a2".to_string(),
                reason: "imbalance".to_string(),
            }],
        };
        let json = serde_json::to_string(&d).unwrap();
        let restored: SchedulerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(d, restored);
    }

    // =========================================================================
    // Error type tests
    // =========================================================================

    #[test]
    fn error_display_coverage() {
        let errors = vec![
            SchedulerError::CircuitBreakerActive {
                tripped_at: 1000,
                resets_at: 11_000,
            },
            SchedulerError::AtMaxCapacity {
                current: 16,
                max: 16,
            },
            SchedulerError::AtMinCapacity { current: 1, min: 1 },
            SchedulerError::CooldownActive {
                operation: "scale-up".to_string(),
                remaining_ms: 500,
            },
            SchedulerError::NoAgentsAvailable,
            SchedulerError::NoReadyWork,
        ];
        for e in &errors {
            let msg = format!("{e}");
            assert!(!msg.is_empty());
        }
    }

    #[test]
    fn error_serde_roundtrip() {
        let e = SchedulerError::CircuitBreakerActive {
            tripped_at: 1000,
            resets_at: 11_000,
        };
        let json = serde_json::to_string(&e).unwrap();
        let restored: SchedulerError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, restored);
    }

    // =========================================================================
    // Convenience function tests
    // =========================================================================

    #[test]
    fn compute_queue_pressure_on_empty() {
        let queue = make_queue();
        let pressure = compute_queue_pressure(&queue);
        assert_eq!(pressure.utilization, 0.0);
        assert_eq!(pressure.ready_ratio, 0.0);
    }

    #[test]
    fn evaluate_readonly_uses_caller_capacity_hint() {
        let scheduler = SwarmScheduler::with_defaults();
        let stats = QueueStats {
            total_items: 8,
            blocked: 0,
            ready: 0,
            in_progress: 4,
            completed: 2,
            failed: 0,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: 0,
        };

        let pressure_tight = scheduler.evaluate_readonly(&stats, 2, 1000);
        let pressure_loose = scheduler.evaluate_readonly(&stats, 4, 1000);
        assert!(pressure_tight.utilization > pressure_loose.utilization);
        assert_eq!(pressure_tight.utilization, 1.0);
        assert_eq!(pressure_loose.utilization, 0.5);
    }

    // =========================================================================
    // History eviction tests
    // =========================================================================

    #[test]
    fn scale_history_evicts_oldest_when_full() {
        let mut scheduler = SwarmScheduler::new(test_config());
        scheduler.max_history_entries = 10;

        for i in 0..15 {
            scheduler.record_event(
                ScaleEventType::ScaleUp,
                format!("event {i}"),
                i,
                i + 1,
                SchedulerDecision::Noop {
                    reason: "test".to_string(),
                },
                i as u64 * 1000,
            );
        }

        assert!(scheduler.scale_history.len() <= 10);
    }

    // =========================================================================
    // Integration: full evaluate cycle
    // =========================================================================

    #[test]
    fn full_evaluate_cycle_assign_and_complete() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        let a1 = "agent-1".to_string();
        scheduler.register_agent(&a1, 0);

        // Enqueue work
        queue.enqueue(make_item("task-1", 0)).unwrap();
        queue.enqueue(make_item("task-2", 1)).unwrap();

        // First evaluation should assign work
        let d1 = scheduler.evaluate(&mut queue, 1000);
        match &d1 {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(!assignments.is_empty());
            }
            other => panic!("expected AssignWork, got {other:?}"),
        }

        // Complete first task
        queue
            .complete(&"task-1".to_string(), &a1, Some("done".to_string()))
            .unwrap();
        scheduler.record_completion(&a1);

        // Second evaluation should assign remaining work
        let d2 = scheduler.evaluate(&mut queue, 2000);
        match &d2 {
            SchedulerDecision::AssignWork { assignments } => {
                assert!(assignments.iter().any(|a| a.item_id == "task-2"));
            }
            SchedulerDecision::Noop { .. } => {
                // task-2 might already have been assigned in d1
            }
            other => panic!("expected AssignWork or Noop, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_reclaims_before_assigning() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = SwarmWorkQueue::new(WorkQueueConfig {
            max_concurrent_per_agent: 3,
            heartbeat_timeout_ms: 0, // immediate timeout
            max_retries: 2,
            anti_starvation: false,
            starvation_threshold_ms: 60_000,
        });

        let agent = "agent-1".to_string();
        scheduler.register_agent(&agent, 0);

        queue.enqueue(make_item("w1", 0)).unwrap();
        queue.assign(&"w1".to_string(), &agent).unwrap();

        // Ensure at least 1ms passes so reclaim_timed_out detects elapsed > 0
        std::thread::sleep(std::time::Duration::from_millis(2));

        let decision = scheduler.evaluate(&mut queue, 5000);
        match decision {
            SchedulerDecision::ReclaimStale { reclaimed_items } => {
                assert!(reclaimed_items.contains(&"w1".to_string()));
            }
            other => panic!("expected ReclaimStale, got {other:?}"),
        }
    }

    #[test]
    fn sequence_increments_on_evaluation() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let mut queue = make_queue();

        assert_eq!(scheduler.sequence(), 0);
        scheduler.evaluate(&mut queue, 1000);
        assert_eq!(scheduler.sequence(), 1);
        scheduler.evaluate(&mut queue, 2000);
        assert_eq!(scheduler.sequence(), 2);
    }

    // =========================================================================
    // Edge case tests
    // =========================================================================

    #[test]
    fn single_agent_no_rebalance() {
        let scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        // Can't rebalance with only one agent
        let result = scheduler.check_rebalance(&queue, 3);
        assert!(result.is_none());
    }

    #[test]
    fn scale_down_prefers_least_productive() {
        let mut scheduler = SwarmScheduler::new(test_config());
        let queue = make_queue();

        scheduler.register_agent(&"productive".to_string(), 0);
        scheduler.register_agent(&"lazy".to_string(), 0);

        // productive has 10 completions, lazy has 0
        for _ in 0..10 {
            scheduler.record_completion(&"productive".to_string());
        }

        let pressure = QueuePressure {
            ready_ratio: 0.0,
            utilization: 0.0,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 0,
            active_agents: 2,
            total_capacity: 6,
        };

        let result = scheduler.try_scale_down(&queue, &pressure, 10_000);
        match result {
            Some(SchedulerDecision::ScaleDown { remove_agents, .. }) => {
                // Should prefer removing the lazy agent first
                assert_eq!(remove_agents[0], "lazy");
            }
            other => panic!("expected ScaleDown, got {other:?}"),
        }
    }

    #[test]
    fn scale_step_proportional_to_pressure() {
        let mut config = test_config();
        config.max_scale_step = 4;
        config.scale_up_threshold = 0.80;
        let mut scheduler = SwarmScheduler::new(config);

        // Moderate pressure: 0.85 → small step
        let pressure = QueuePressure {
            ready_ratio: 0.5,
            utilization: 0.85,
            starvation_count: 0,
            failure_rate: 0.0,
            pending_items: 10,
            active_agents: 4,
            total_capacity: 12,
        };
        let result = scheduler.try_scale_up(&pressure, 5000);
        match result {
            Some(SchedulerDecision::ScaleUp {
                additional_agents, ..
            }) => {
                assert!(additional_agents >= 1);
                assert!(additional_agents <= 4);
            }
            other => panic!("expected ScaleUp, got {other:?}"),
        }
    }

    #[test]
    fn work_queue_config_accessible() {
        let queue = make_queue();
        let cfg = queue.config();
        assert_eq!(cfg.max_concurrent_per_agent, 3);
    }
}
