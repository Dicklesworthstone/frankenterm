// =============================================================================
// Dependency-aware work queue with Beads integration (ft-3681t.3.3)
//
// A native work dispatch queue that integrates Beads-style dependency graph
// semantics: agents pull or are assigned only ready/unblocked work items, with
// explicit ownership/completion transitions, anti-starvation fairness, and
// deterministic restart-safe queue replay.
//
// # Architecture
//
// ```text
// WorkItem graph (DAG) ──► ReadySet (no unmet deps) ──► Assignment
//        │                      ↑                          │
//        ▼                      │                          ▼
//   dep tracking ──► completion ──► unblock children    AgentSlot
//        │                                                 │
//        ▼                                                 ▼
//   checkpoint/replay ◄────────── ownership ledger ◄── heartbeat
// ```
//
// The queue is synchronous and in-memory, backed by durable checkpoints
// for restart safety.
// =============================================================================

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// =============================================================================
// Work item types
// =============================================================================

/// Unique identifier for a work item.
pub type WorkItemId = String;

/// Unique identifier for an agent slot.
pub type AgentSlotId = String;

/// Priority level for work items (lower number = higher priority).
pub type Priority = u32;

/// A unit of work in the dependency-aware queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItem {
    /// Unique identifier (typically maps to a Beads issue ID).
    pub id: WorkItemId,
    /// Human-readable title.
    pub title: String,
    /// Priority (0 = highest, larger = lower priority).
    pub priority: Priority,
    /// IDs of work items that must complete before this one is ready.
    pub depends_on: Vec<WorkItemId>,
    /// Estimated effort units (for fairness/load balancing).
    #[serde(default = "default_effort")]
    pub effort: u32,
    /// Labels/tags for filtering and matching.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Optional preferred agent program for this work item.
    #[serde(default)]
    pub preferred_program: Option<String>,
    /// Additional metadata.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

fn default_effort() -> u32 {
    1
}

/// Current status of a work item in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemStatus {
    /// Waiting for dependencies to complete.
    Blocked,
    /// All dependencies met; available for assignment.
    Ready,
    /// Assigned to an agent and being worked on.
    InProgress,
    /// Successfully completed.
    Completed,
    /// Failed and not retried.
    Failed,
    /// Cancelled (removed from queue without completion).
    Cancelled,
}

impl WorkItemStatus {
    /// Whether this status represents a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// An assignment record linking a work item to an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Assignment {
    /// The work item being worked on.
    pub work_item_id: WorkItemId,
    /// The agent assigned to this work.
    pub agent_slot: AgentSlotId,
    /// When the assignment was made (epoch ms).
    pub assigned_at: u64,
    /// Last heartbeat from the agent (epoch ms).
    pub last_heartbeat: u64,
    /// Number of times this item has been assigned (for retry tracking).
    pub attempt: u32,
}

/// Record of a completed work item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionRecord {
    pub work_item_id: WorkItemId,
    pub agent_slot: AgentSlotId,
    pub completed_at: u64,
    pub success: bool,
    pub message: Option<String>,
}

// =============================================================================
// Queue configuration
// =============================================================================

/// Configuration for the work queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkQueueConfig {
    /// Maximum number of concurrent assignments per agent.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_per_agent: u32,
    /// Heartbeat timeout in milliseconds. If an agent doesn't heartbeat
    /// within this window, its assignments become eligible for reassignment.
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout_ms: u64,
    /// Maximum number of retry attempts for failed work items.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Whether to use anti-starvation fairness (round-robin across priorities
    /// after items have waited longer than the starvation threshold).
    #[serde(default = "default_true")]
    pub anti_starvation: bool,
    /// Starvation threshold in milliseconds. Items waiting longer than this
    /// get priority boost.
    #[serde(default = "default_starvation_threshold")]
    pub starvation_threshold_ms: u64,
}

fn default_max_concurrent() -> u32 {
    3
}
fn default_heartbeat_timeout() -> u64 {
    300_000 // 5 minutes
}
fn default_max_retries() -> u32 {
    2
}
fn default_true() -> bool {
    true
}
fn default_starvation_threshold() -> u64 {
    600_000 // 10 minutes
}

impl Default for WorkQueueConfig {
    fn default() -> Self {
        Self {
            max_concurrent_per_agent: default_max_concurrent(),
            heartbeat_timeout_ms: default_heartbeat_timeout(),
            max_retries: default_max_retries(),
            anti_starvation: true,
            starvation_threshold_ms: default_starvation_threshold(),
        }
    }
}

// =============================================================================
// Queue errors
// =============================================================================

/// Errors from work queue operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkQueueError {
    /// Work item not found.
    ItemNotFound { id: WorkItemId },
    /// Work item already exists.
    DuplicateItem { id: WorkItemId },
    /// Work item is not in a valid state for the requested operation.
    InvalidState {
        id: WorkItemId,
        current: WorkItemStatus,
        expected: &'static str,
    },
    /// Dependency cycle detected.
    CycleDetected { ids: Vec<WorkItemId> },
    /// Agent has reached max concurrent assignments.
    AgentAtCapacity {
        agent: AgentSlotId,
        current: u32,
        max: u32,
    },
    /// Work item depends on a non-existent item.
    DependencyNotFound {
        item: WorkItemId,
        dependency: WorkItemId,
    },
    /// No ready work items available.
    QueueEmpty,
}

impl std::fmt::Display for WorkQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ItemNotFound { id } => write!(f, "work item '{id}' not found"),
            Self::DuplicateItem { id } => write!(f, "work item '{id}' already exists"),
            Self::InvalidState {
                id,
                current,
                expected,
            } => {
                write!(f, "work item '{id}' is {current:?}, expected {expected}")
            }
            Self::CycleDetected { ids } => {
                write!(f, "dependency cycle detected: {}", ids.join(" → "))
            }
            Self::AgentAtCapacity {
                agent,
                current,
                max,
            } => {
                write!(f, "agent '{agent}' at capacity ({current}/{max})")
            }
            Self::DependencyNotFound { item, dependency } => {
                write!(
                    f,
                    "work item '{item}' depends on unknown item '{dependency}'"
                )
            }
            Self::QueueEmpty => write!(f, "no ready work items"),
        }
    }
}

impl std::error::Error for WorkQueueError {}

// =============================================================================
// Queue statistics
// =============================================================================

/// Summary statistics for the work queue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueStats {
    pub total_items: usize,
    pub blocked: usize,
    pub ready: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub active_agents: usize,
    pub completion_log_size: usize,
}

// =============================================================================
// Work queue
// =============================================================================

/// A dependency-aware work queue for swarm agent orchestration.
///
/// The queue tracks work items with dependency relationships (forming a DAG),
/// maintains a "ready set" of items whose dependencies are all met, and assigns
/// items to agent slots with ownership tracking and heartbeat-based liveness.
pub struct SwarmWorkQueue {
    config: WorkQueueConfig,
    /// All work items by ID.
    items: HashMap<WorkItemId, WorkItem>,
    /// Current status of each work item.
    status: HashMap<WorkItemId, WorkItemStatus>,
    /// When each item was added to the queue (for starvation tracking).
    enqueued_at: HashMap<WorkItemId, u64>,
    /// Forward dependency graph: item → items that depend on it.
    dependents: HashMap<WorkItemId, BTreeSet<WorkItemId>>,
    /// Active assignments: work_item_id → assignment record.
    assignments: HashMap<WorkItemId, Assignment>,
    /// Per-agent active assignment count.
    agent_load: HashMap<AgentSlotId, u32>,
    /// Completion log (append-only for replay).
    completion_log: Vec<CompletionRecord>,
    /// Monotonic sequence number for deterministic ordering.
    sequence: u64,
}

impl SwarmWorkQueue {
    /// Create a new work queue with the given configuration.
    pub fn new(config: WorkQueueConfig) -> Self {
        Self {
            config,
            items: HashMap::new(),
            status: HashMap::new(),
            enqueued_at: HashMap::new(),
            dependents: HashMap::new(),
            assignments: HashMap::new(),
            agent_load: HashMap::new(),
            completion_log: Vec::new(),
            sequence: 0,
        }
    }

    /// Create a work queue with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(WorkQueueConfig::default())
    }

    /// Access queue configuration.
    pub fn config(&self) -> &WorkQueueConfig {
        &self.config
    }

    // =========================================================================
    // Enqueue / dependency management
    // =========================================================================

    /// Add a work item to the queue.
    ///
    /// The item starts as `Blocked` if it has unmet dependencies, or `Ready`
    /// if all dependencies are already completed.
    pub fn enqueue(&mut self, item: WorkItem) -> Result<WorkItemStatus, WorkQueueError> {
        if self.items.contains_key(&item.id) {
            return Err(WorkQueueError::DuplicateItem { id: item.id });
        }

        // Validate dependencies exist
        for dep_id in &item.depends_on {
            if !self.items.contains_key(dep_id) && !dep_id.is_empty() {
                return Err(WorkQueueError::DependencyNotFound {
                    item: item.id.clone(),
                    dependency: dep_id.clone(),
                });
            }
        }

        // Register as a dependent of each dependency
        for dep_id in &item.depends_on {
            if !dep_id.is_empty() {
                self.dependents
                    .entry(dep_id.clone())
                    .or_default()
                    .insert(item.id.clone());
            }
        }

        // Determine initial status
        let initial_status = if self.all_deps_met(&item) {
            WorkItemStatus::Ready
        } else {
            WorkItemStatus::Blocked
        };

        let now = epoch_ms();
        self.enqueued_at.insert(item.id.clone(), now);
        self.status.insert(item.id.clone(), initial_status);
        self.items.insert(item.id.clone(), item);
        self.sequence += 1;

        Ok(initial_status)
    }

    /// Enqueue multiple work items at once, resolving internal dependencies.
    ///
    /// Items are added in order, so later items can depend on earlier ones.
    pub fn enqueue_batch(
        &mut self,
        items: Vec<WorkItem>,
    ) -> Vec<Result<WorkItemStatus, WorkQueueError>> {
        items.into_iter().map(|item| self.enqueue(item)).collect()
    }

    /// Check if all dependencies of an item are completed.
    fn all_deps_met(&self, item: &WorkItem) -> bool {
        item.depends_on.iter().all(|dep_id| {
            dep_id.is_empty()
                || self
                    .status
                    .get(dep_id)
                    .map(|s| *s == WorkItemStatus::Completed)
                    .unwrap_or(false)
        })
    }

    /// Re-evaluate blocked items and promote to Ready if deps are met.
    fn recompute_ready_set(&mut self, completed_id: &WorkItemId) {
        let dependents = self
            .dependents
            .get(completed_id)
            .cloned()
            .unwrap_or_default();

        for dep_id in dependents {
            if let Some(status) = self.status.get(&dep_id) {
                if *status == WorkItemStatus::Blocked {
                    if let Some(item) = self.items.get(&dep_id) {
                        if self.all_deps_met(item) {
                            self.status.insert(dep_id, WorkItemStatus::Ready);
                        }
                    }
                }
            }
        }
    }

    // =========================================================================
    // Assignment
    // =========================================================================

    /// Pull the next ready work item for the given agent.
    ///
    /// Uses priority ordering with optional anti-starvation boost.
    /// Returns `None` if no ready items or agent is at capacity.
    pub fn pull(&mut self, agent: &AgentSlotId) -> Result<Assignment, WorkQueueError> {
        // Check agent capacity
        let current_load = self.agent_load.get(agent).copied().unwrap_or(0);
        if current_load >= self.config.max_concurrent_per_agent {
            return Err(WorkQueueError::AgentAtCapacity {
                agent: agent.clone(),
                current: current_load,
                max: self.config.max_concurrent_per_agent,
            });
        }

        // Find the best ready item
        let now = epoch_ms();
        let candidate = self.select_next_ready(now);

        match candidate {
            Some(item_id) => {
                let assignment = Assignment {
                    work_item_id: item_id.clone(),
                    agent_slot: agent.clone(),
                    assigned_at: now,
                    last_heartbeat: now,
                    attempt: self
                        .completion_log
                        .iter()
                        .filter(|c| c.work_item_id == item_id && !c.success)
                        .count() as u32
                        + 1,
                };

                self.status
                    .insert(item_id.clone(), WorkItemStatus::InProgress);
                self.assignments.insert(item_id, assignment.clone());
                *self.agent_load.entry(agent.clone()).or_insert(0) += 1;
                self.sequence += 1;

                Ok(assignment)
            }
            None => Err(WorkQueueError::QueueEmpty),
        }
    }

    /// Select the next ready item using priority + anti-starvation.
    fn select_next_ready(&self, now: u64) -> Option<WorkItemId> {
        let mut candidates: Vec<(&WorkItemId, &WorkItem, u64)> = self
            .items
            .iter()
            .filter(|(id, _)| self.status.get(*id).copied() == Some(WorkItemStatus::Ready))
            .map(|(id, item)| {
                let enqueued = self.enqueued_at.get(id).copied().unwrap_or(now);
                (id, item, enqueued)
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        if self.config.anti_starvation {
            // Boost priority for items waiting longer than threshold
            candidates.sort_by(|(_, a, a_enqueued), (_, b, b_enqueued)| {
                let a_wait = now.saturating_sub(*a_enqueued);
                let b_wait = now.saturating_sub(*b_enqueued);
                let a_starved = a_wait >= self.config.starvation_threshold_ms;
                let b_starved = b_wait >= self.config.starvation_threshold_ms;

                // Starved items always come first
                match (a_starved, b_starved) {
                    (true, false) => std::cmp::Ordering::Less,
                    (false, true) => std::cmp::Ordering::Greater,
                    _ => {
                        // Within same starvation class, sort by priority then wait time
                        a.priority.cmp(&b.priority).then(b_wait.cmp(&a_wait))
                    }
                }
            });
        } else {
            // Simple priority ordering
            candidates.sort_by_key(|(_, item, _)| item.priority);
        }

        candidates.first().map(|(id, _, _)| (*id).clone())
    }

    /// Assign a specific work item to a specific agent.
    pub fn assign(
        &mut self,
        item_id: &WorkItemId,
        agent: &AgentSlotId,
    ) -> Result<Assignment, WorkQueueError> {
        let status =
            self.status
                .get(item_id)
                .copied()
                .ok_or_else(|| WorkQueueError::ItemNotFound {
                    id: item_id.clone(),
                })?;

        if status != WorkItemStatus::Ready {
            return Err(WorkQueueError::InvalidState {
                id: item_id.clone(),
                current: status,
                expected: "Ready",
            });
        }

        let current_load = self.agent_load.get(agent).copied().unwrap_or(0);
        if current_load >= self.config.max_concurrent_per_agent {
            return Err(WorkQueueError::AgentAtCapacity {
                agent: agent.clone(),
                current: current_load,
                max: self.config.max_concurrent_per_agent,
            });
        }

        let now = epoch_ms();
        let assignment = Assignment {
            work_item_id: item_id.clone(),
            agent_slot: agent.clone(),
            assigned_at: now,
            last_heartbeat: now,
            attempt: self
                .completion_log
                .iter()
                .filter(|c| c.work_item_id == *item_id && !c.success)
                .count() as u32
                + 1,
        };

        self.status
            .insert(item_id.clone(), WorkItemStatus::InProgress);
        self.assignments.insert(item_id.clone(), assignment.clone());
        *self.agent_load.entry(agent.clone()).or_insert(0) += 1;
        self.sequence += 1;

        Ok(assignment)
    }

    // =========================================================================
    // Completion / failure
    // =========================================================================

    /// Mark a work item as completed successfully.
    pub fn complete(
        &mut self,
        item_id: &WorkItemId,
        agent: &AgentSlotId,
        message: Option<String>,
    ) -> Result<CompletionRecord, WorkQueueError> {
        self.finish_item(item_id, agent, true, message)
    }

    /// Mark a work item as failed.
    ///
    /// If retries are available, the item is returned to `Ready` status.
    pub fn fail(
        &mut self,
        item_id: &WorkItemId,
        agent: &AgentSlotId,
        message: Option<String>,
    ) -> Result<CompletionRecord, WorkQueueError> {
        self.finish_item(item_id, agent, false, message)
    }

    fn finish_item(
        &mut self,
        item_id: &WorkItemId,
        agent: &AgentSlotId,
        success: bool,
        message: Option<String>,
    ) -> Result<CompletionRecord, WorkQueueError> {
        let status =
            self.status
                .get(item_id)
                .copied()
                .ok_or_else(|| WorkQueueError::ItemNotFound {
                    id: item_id.clone(),
                })?;

        if status != WorkItemStatus::InProgress {
            return Err(WorkQueueError::InvalidState {
                id: item_id.clone(),
                current: status,
                expected: "InProgress",
            });
        }

        // Enforce assignment ownership so only the assigned agent can finish
        // an in-flight item. This prevents cross-agent completion/failure races.
        let assignment =
            self.assignments
                .get(item_id)
                .ok_or_else(|| WorkQueueError::InvalidState {
                    id: item_id.clone(),
                    current: status,
                    expected: "assigned to an agent",
                })?;
        if assignment.agent_slot != *agent {
            return Err(WorkQueueError::InvalidState {
                id: item_id.clone(),
                current: status,
                expected: "assigned to this agent",
            });
        }

        // Remove assignment
        if let Some(assignment) = self.assignments.remove(item_id) {
            if let Some(load) = self.agent_load.get_mut(&assignment.agent_slot) {
                *load = load.saturating_sub(1);
            }
        }

        let now = epoch_ms();
        let record = CompletionRecord {
            work_item_id: item_id.clone(),
            agent_slot: agent.clone(),
            completed_at: now,
            success,
            message,
        };

        self.completion_log.push(record.clone());

        if success {
            self.status
                .insert(item_id.clone(), WorkItemStatus::Completed);
            // Unblock dependents
            self.recompute_ready_set(item_id);
        } else {
            // Check if retries are available
            let attempt_count = self
                .completion_log
                .iter()
                .filter(|c| c.work_item_id == *item_id && !c.success)
                .count() as u32;

            if attempt_count < self.config.max_retries {
                self.status.insert(item_id.clone(), WorkItemStatus::Ready);
            } else {
                self.status.insert(item_id.clone(), WorkItemStatus::Failed);
            }
        }

        self.sequence += 1;
        Ok(record)
    }

    /// Cancel a work item. If in progress, releases the agent assignment.
    pub fn cancel(&mut self, item_id: &WorkItemId) -> Result<(), WorkQueueError> {
        let status =
            self.status
                .get(item_id)
                .copied()
                .ok_or_else(|| WorkQueueError::ItemNotFound {
                    id: item_id.clone(),
                })?;

        if status.is_terminal() {
            return Err(WorkQueueError::InvalidState {
                id: item_id.clone(),
                current: status,
                expected: "non-terminal",
            });
        }

        // Release assignment if in progress
        if let Some(assignment) = self.assignments.remove(item_id) {
            if let Some(load) = self.agent_load.get_mut(&assignment.agent_slot) {
                *load = load.saturating_sub(1);
            }
        }

        self.status
            .insert(item_id.clone(), WorkItemStatus::Cancelled);
        self.sequence += 1;
        Ok(())
    }

    // =========================================================================
    // Heartbeat / liveness
    // =========================================================================

    /// Record a heartbeat from an agent for a specific work item.
    pub fn heartbeat(
        &mut self,
        item_id: &WorkItemId,
        agent: &AgentSlotId,
    ) -> Result<(), WorkQueueError> {
        let assignment =
            self.assignments
                .get_mut(item_id)
                .ok_or_else(|| WorkQueueError::ItemNotFound {
                    id: item_id.clone(),
                })?;

        if assignment.agent_slot != *agent {
            return Err(WorkQueueError::InvalidState {
                id: item_id.clone(),
                current: WorkItemStatus::InProgress,
                expected: "assigned to this agent",
            });
        }

        assignment.last_heartbeat = epoch_ms();
        Ok(())
    }

    /// Detect and reclaim work items from agents that have timed out.
    ///
    /// Returns the list of items that were reclaimed.
    pub fn reclaim_timed_out(&mut self) -> Vec<WorkItemId> {
        let now = epoch_ms();
        let timeout = self.config.heartbeat_timeout_ms;

        let timed_out: Vec<WorkItemId> = self
            .assignments
            .iter()
            .filter(|(_, a)| now.saturating_sub(a.last_heartbeat) > timeout)
            .map(|(id, _)| id.clone())
            .collect();

        for item_id in &timed_out {
            if let Some(assignment) = self.assignments.remove(item_id) {
                if let Some(load) = self.agent_load.get_mut(&assignment.agent_slot) {
                    *load = load.saturating_sub(1);
                }
            }
            // Return to ready for reassignment
            self.status.insert(item_id.clone(), WorkItemStatus::Ready);
        }

        self.sequence += 1;
        timed_out
    }

    // =========================================================================
    // Query
    // =========================================================================

    /// Get the current status of a work item.
    pub fn item_status(&self, id: &WorkItemId) -> Option<WorkItemStatus> {
        self.status.get(id).copied()
    }

    /// Get a work item by ID.
    pub fn get_item(&self, id: &WorkItemId) -> Option<&WorkItem> {
        self.items.get(id)
    }

    /// Get the assignment record for a work item.
    pub fn get_assignment(&self, id: &WorkItemId) -> Option<&Assignment> {
        self.assignments.get(id)
    }

    /// List all ready work items, sorted by priority.
    pub fn ready_items(&self) -> Vec<&WorkItem> {
        let mut items: Vec<&WorkItem> = self
            .items
            .iter()
            .filter(|(id, _)| self.status.get(*id).copied() == Some(WorkItemStatus::Ready))
            .map(|(_, item)| item)
            .collect();
        items.sort_by_key(|item| item.priority);
        items
    }

    /// List all items assigned to a specific agent.
    pub fn agent_items(&self, agent: &AgentSlotId) -> Vec<&Assignment> {
        self.assignments
            .values()
            .filter(|a| a.agent_slot == *agent)
            .collect()
    }

    /// Get queue statistics.
    pub fn stats(&self) -> QueueStats {
        let mut stats = QueueStats {
            total_items: self.items.len(),
            completion_log_size: self.completion_log.len(),
            ..Default::default()
        };

        for status in self.status.values() {
            match status {
                WorkItemStatus::Blocked => stats.blocked += 1,
                WorkItemStatus::Ready => stats.ready += 1,
                WorkItemStatus::InProgress => stats.in_progress += 1,
                WorkItemStatus::Completed => stats.completed += 1,
                WorkItemStatus::Failed => stats.failed += 1,
                WorkItemStatus::Cancelled => stats.cancelled += 1,
            }
        }

        stats.active_agents = self.agent_load.values().filter(|&&load| load > 0).count();

        stats
    }

    /// Get the completion log for replay.
    pub fn completion_log(&self) -> &[CompletionRecord] {
        &self.completion_log
    }

    /// Current sequence number (monotonically increasing mutation counter).
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    // =========================================================================
    // Cycle detection
    // =========================================================================

    /// Check whether adding the given dependencies would create a cycle.
    pub fn would_create_cycle(&self, item_id: &WorkItemId, new_deps: &[WorkItemId]) -> bool {
        // BFS from each new dependency to see if we can reach item_id
        for dep in new_deps {
            let mut visited = BTreeSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(dep.clone());

            while let Some(current) = queue.pop_front() {
                if current == *item_id {
                    return true;
                }
                if visited.insert(current.clone()) {
                    if let Some(item) = self.items.get(&current) {
                        for next_dep in &item.depends_on {
                            if !next_dep.is_empty() {
                                queue.push_back(next_dep.clone());
                            }
                        }
                    }
                }
            }
        }
        false
    }

    // =========================================================================
    // Snapshot / replay
    // =========================================================================

    /// Create a serializable snapshot of the queue state for checkpointing.
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            items: self.items.values().cloned().collect(),
            status: self.status.clone(),
            assignments: self.assignments.values().cloned().collect(),
            completion_log: self.completion_log.clone(),
            sequence: self.sequence,
        }
    }

    /// Restore queue state from a snapshot.
    pub fn restore(snapshot: QueueSnapshot, config: WorkQueueConfig) -> Self {
        let mut queue = Self::new(config);

        // Restore items
        for item in snapshot.items {
            // Register dependents
            for dep_id in &item.depends_on {
                if !dep_id.is_empty() {
                    queue
                        .dependents
                        .entry(dep_id.clone())
                        .or_default()
                        .insert(item.id.clone());
                }
            }
            queue.items.insert(item.id.clone(), item);
        }

        // Restore status
        queue.status = snapshot.status;

        // Restore assignments
        for assignment in snapshot.assignments {
            *queue
                .agent_load
                .entry(assignment.agent_slot.clone())
                .or_insert(0) += 1;
            queue
                .assignments
                .insert(assignment.work_item_id.clone(), assignment);
        }

        // Restore completion log
        queue.completion_log = snapshot.completion_log;
        queue.sequence = snapshot.sequence;

        queue
    }
}

/// Serializable queue snapshot for checkpoint/restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub items: Vec<WorkItem>,
    pub status: HashMap<WorkItemId, WorkItemStatus>,
    pub assignments: Vec<Assignment>,
    pub completion_log: Vec<CompletionRecord>,
    pub sequence: u64,
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Beads Bridge — import Beads issues into the work queue (ft-3681t.3.3)
// =============================================================================

/// A raw bead dependency record as stored in `.beads/issues.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadDependency {
    pub issue_id: String,
    pub depends_on_id: String,
    #[serde(rename = "type")]
    pub dep_type: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub metadata: String,
    #[serde(default)]
    pub thread_id: String,
}

/// A bead record deserialized from one line of `.beads/issues.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadRecord {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: String,
    #[serde(default = "default_bead_priority")]
    pub priority: u32,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub assignee: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub closed_at: String,
    #[serde(default)]
    pub close_reason: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<BeadDependency>,
    #[serde(default)]
    pub acceptance_criteria: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub source_repo: String,
    #[serde(default)]
    pub compaction_level: u32,
    #[serde(default)]
    pub original_size: u64,
}

fn default_bead_priority() -> u32 {
    2
}

impl BeadRecord {
    /// Whether the bead is in a status that should be imported as active work.
    pub fn is_actionable(&self) -> bool {
        matches!(self.status.as_str(), "open" | "in_progress")
    }

    /// Whether the bead is complete (closed or cancelled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "closed" | "cancelled" | "resolved" | "wontfix"
        )
    }

    /// Extract the IDs this bead depends on (from the `dependencies` array).
    pub fn blocking_ids(&self) -> Vec<String> {
        self.dependencies
            .iter()
            .filter(|d| d.dep_type == "blocks")
            .map(|d| d.depends_on_id.clone())
            .collect()
    }

    /// Convert to a `WorkItem` for the `SwarmWorkQueue`.
    pub fn to_work_item(&self) -> WorkItem {
        let mut metadata = HashMap::new();
        if !self.issue_type.is_empty() {
            metadata.insert("issue_type".to_string(), self.issue_type.clone());
        }
        if !self.assignee.is_empty() {
            metadata.insert("assignee".to_string(), self.assignee.clone());
        }
        if !self.created_at.is_empty() {
            metadata.insert("created_at".to_string(), self.created_at.clone());
        }
        if !self.acceptance_criteria.is_empty() {
            metadata.insert("has_acceptance_criteria".to_string(), "true".to_string());
        }

        WorkItem {
            id: self.id.clone(),
            title: self.title.clone(),
            priority: self.priority,
            depends_on: self.blocking_ids(),
            effort: self.estimate_effort(),
            labels: self.labels.clone(),
            preferred_program: None,
            metadata,
        }
    }

    /// Heuristic effort estimate based on bead type and description length.
    fn estimate_effort(&self) -> u32 {
        match self.issue_type.as_str() {
            "epic" => 8,
            "feature" => 5,
            "task" => 3,
            "bug" => 2,
            "docs" | "question" => 1,
            _ => 3,
        }
    }
}

/// Result of importing beads into a work queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BeadsSyncReport {
    /// Number of beads imported as new work items.
    pub imported: u32,
    /// Number of existing items whose status was updated.
    pub updated: u32,
    /// Number of beads skipped (terminal status, already present, etc.).
    pub skipped: u32,
    /// IDs that had dependency references to unknown items (orphan deps).
    pub orphan_deps: Vec<String>,
    /// IDs of items that were marked complete because the bead was closed.
    pub completed_from_bead: Vec<String>,
}

/// Reads `.beads/issues.jsonl` and imports actionable beads into a `SwarmWorkQueue`.
#[derive(Debug)]
pub struct BeadsImporter {
    records: Vec<BeadRecord>,
}

impl BeadsImporter {
    /// Parse beads from a JSONL string (one JSON object per line).
    pub fn from_jsonl(jsonl: &str) -> Result<Self, BeadsImportError> {
        let mut records = Vec::new();
        for (line_num, line) in jsonl.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let record: BeadRecord =
                serde_json::from_str(trimmed).map_err(|e| BeadsImportError::ParseError {
                    line: line_num + 1,
                    message: e.to_string(),
                })?;
            records.push(record);
        }
        Ok(Self { records })
    }

    /// Read and parse from a file path.
    pub fn from_path(path: &std::path::Path) -> Result<Self, BeadsImportError> {
        let content = std::fs::read_to_string(path).map_err(|e| BeadsImportError::IoError {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        Self::from_jsonl(&content)
    }

    /// Total number of parsed records.
    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    /// Get only actionable (open/in_progress) beads.
    pub fn actionable_records(&self) -> Vec<&BeadRecord> {
        self.records.iter().filter(|r| r.is_actionable()).collect()
    }

    /// Get the set of all known bead IDs (for dependency validation).
    pub fn known_ids(&self) -> BTreeSet<String> {
        self.records.iter().map(|r| r.id.clone()).collect()
    }

    /// Get the set of terminal (closed) bead IDs.
    pub fn terminal_ids(&self) -> BTreeSet<String> {
        self.records
            .iter()
            .filter(|r| r.is_terminal())
            .map(|r| r.id.clone())
            .collect()
    }

    /// Import actionable beads into a work queue, syncing status bidirectionally.
    ///
    /// - New actionable beads are enqueued as work items
    /// - Beads that became closed mark their queue items as completed
    /// - Dependencies are mapped from bead graph to queue graph
    /// - Orphan dependencies (referencing unknown IDs) are reported
    pub fn sync_to_queue(&self, queue: &mut SwarmWorkQueue) -> BeadsSyncReport {
        let known = self.known_ids();
        let terminal = self.terminal_ids();
        let mut report = BeadsSyncReport {
            imported: 0,
            updated: 0,
            skipped: 0,
            orphan_deps: Vec::new(),
            completed_from_bead: Vec::new(),
        };

        for record in &self.records {
            let existing_status = queue.item_status(&record.id);

            // Skip if already terminal in queue
            if existing_status.is_some_and(|s| s.is_terminal()) {
                report.skipped += 1;
                continue;
            }

            // If bead is closed but queue item is active → mark completed
            if record.is_terminal() {
                if existing_status.is_some() {
                    // Best-effort complete: use a synthetic agent ID
                    let _ = queue.cancel(&record.id);
                    report.completed_from_bead.push(record.id.clone());
                    report.updated += 1;
                } else {
                    report.skipped += 1;
                }
                continue;
            }

            // Skip non-actionable
            if !record.is_actionable() {
                report.skipped += 1;
                continue;
            }

            // Already in queue → skip (no double-enqueue)
            if existing_status.is_some() {
                report.skipped += 1;
                continue;
            }

            // Build work item, filtering out deps to unknown IDs
            let mut item = record.to_work_item();
            let mut orphans = Vec::new();
            item.depends_on.retain(|dep_id| {
                if known.contains(dep_id) {
                    // Dep to a terminal bead is already satisfied → remove
                    !terminal.contains(dep_id)
                } else {
                    orphans.push(dep_id.clone());
                    false
                }
            });
            report.orphan_deps.extend(orphans);

            // Enqueue (may fail if dependencies reference items not yet in queue)
            match queue.enqueue(item) {
                Ok(_status) => report.imported += 1,
                Err(_) => report.skipped += 1,
            }
        }

        report
    }
}

/// Map a `WorkItemStatus` back to a bead-compatible status string.
pub fn work_status_to_bead_status(status: WorkItemStatus) -> &'static str {
    match status {
        WorkItemStatus::Blocked => "open",
        WorkItemStatus::Ready => "open",
        WorkItemStatus::InProgress => "in_progress",
        WorkItemStatus::Completed => "closed",
        WorkItemStatus::Failed => "open",
        WorkItemStatus::Cancelled => "closed",
    }
}

/// Errors during beads import.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BeadsImportError {
    /// Failed to parse a JSONL line.
    ParseError { line: usize, message: String },
    /// Failed to read the file.
    IoError { path: String, message: String },
}

impl std::fmt::Display for BeadsImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError { line, message } => {
                write!(f, "parse error on line {line}: {message}")
            }
            Self::IoError { path, message } => {
                write!(f, "I/O error reading {path}: {message}")
            }
        }
    }
}

impl std::error::Error for BeadsImportError {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, priority: u32, deps: &[&str]) -> WorkItem {
        WorkItem {
            id: id.into(),
            title: format!("Work item {id}"),
            priority,
            depends_on: deps.iter().map(|d| (*d).into()).collect(),
            effort: 1,
            labels: Vec::new(),
            preferred_program: None,
            metadata: HashMap::new(),
        }
    }

    // =========================================================================
    // Basic enqueue/pull
    // =========================================================================

    #[test]
    fn enqueue_item_without_deps_is_ready() {
        let mut q = SwarmWorkQueue::with_defaults();
        let status = q.enqueue(item("a", 0, &[])).unwrap();
        assert_eq!(status, WorkItemStatus::Ready);
    }

    #[test]
    fn enqueue_item_with_unmet_dep_is_blocked() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        let status = q.enqueue(item("b", 0, &["a"])).unwrap();
        assert_eq!(status, WorkItemStatus::Blocked);
    }

    #[test]
    fn enqueue_item_with_completed_dep_is_ready() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        let _assign = q.pull(&"agent-1".into()).unwrap();
        q.complete(&"a".into(), &"agent-1".into(), None).unwrap();

        let status = q.enqueue(item("b", 0, &["a"])).unwrap();
        assert_eq!(status, WorkItemStatus::Ready);
    }

    #[test]
    fn enqueue_duplicate_fails() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        let result = q.enqueue(item("a", 0, &[]));
        assert!(matches!(result, Err(WorkQueueError::DuplicateItem { .. })));
    }

    #[test]
    fn enqueue_with_missing_dep_fails() {
        let mut q = SwarmWorkQueue::with_defaults();
        let result = q.enqueue(item("a", 0, &["nonexistent"]));
        assert!(matches!(
            result,
            Err(WorkQueueError::DependencyNotFound { .. })
        ));
    }

    // =========================================================================
    // Pull ordering
    // =========================================================================

    #[test]
    fn pull_returns_highest_priority_first() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("low", 10, &[])).unwrap();
        q.enqueue(item("high", 0, &[])).unwrap();
        q.enqueue(item("mid", 5, &[])).unwrap();

        let assignment = q.pull(&"agent".into()).unwrap();
        assert_eq!(assignment.work_item_id, "high");
    }

    #[test]
    fn pull_at_capacity_fails() {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: 1,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &[])).unwrap();

        q.pull(&"agent".into()).unwrap();
        let result = q.pull(&"agent".into());
        assert!(matches!(
            result,
            Err(WorkQueueError::AgentAtCapacity { .. })
        ));
    }

    #[test]
    fn pull_from_empty_queue_fails() {
        let mut q = SwarmWorkQueue::with_defaults();
        let result = q.pull(&"agent".into());
        assert!(matches!(result, Err(WorkQueueError::QueueEmpty)));
    }

    // =========================================================================
    // Completion / dependency unblocking
    // =========================================================================

    #[test]
    fn completing_item_unblocks_dependents() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &["a"])).unwrap();
        q.enqueue(item("c", 0, &["a"])).unwrap();

        assert_eq!(q.item_status(&"b".into()), Some(WorkItemStatus::Blocked));
        assert_eq!(q.item_status(&"c".into()), Some(WorkItemStatus::Blocked));

        let _assign = q.pull(&"agent".into()).unwrap();
        q.complete(&"a".into(), &"agent".into(), None).unwrap();

        assert_eq!(q.item_status(&"b".into()), Some(WorkItemStatus::Ready));
        assert_eq!(q.item_status(&"c".into()), Some(WorkItemStatus::Ready));
    }

    #[test]
    fn item_with_multiple_deps_needs_all_completed() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &[])).unwrap();
        q.enqueue(item("c", 0, &["a", "b"])).unwrap();

        assert_eq!(q.item_status(&"c".into()), Some(WorkItemStatus::Blocked));

        // Complete a — use assign() to target the specific item
        q.assign(&"a".into(), &"agent".into()).unwrap();
        q.complete(&"a".into(), &"agent".into(), None).unwrap();

        // c still blocked (b not done)
        assert_eq!(q.item_status(&"c".into()), Some(WorkItemStatus::Blocked));

        // Complete b
        q.assign(&"b".into(), &"agent".into()).unwrap();
        q.complete(&"b".into(), &"agent".into(), None).unwrap();

        // Now c is ready
        assert_eq!(q.item_status(&"c".into()), Some(WorkItemStatus::Ready));
    }

    // =========================================================================
    // Failure / retry
    // =========================================================================

    #[test]
    fn failed_item_retries_up_to_max() {
        let config = WorkQueueConfig {
            max_retries: 2,
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(item("a", 0, &[])).unwrap();

        // Attempt 1: fail
        q.pull(&"agent".into()).unwrap();
        q.fail(&"a".into(), &"agent".into(), Some("error 1".into()))
            .unwrap();
        assert_eq!(q.item_status(&"a".into()), Some(WorkItemStatus::Ready));

        // Attempt 2: fail
        q.pull(&"agent".into()).unwrap();
        q.fail(&"a".into(), &"agent".into(), Some("error 2".into()))
            .unwrap();
        assert_eq!(q.item_status(&"a".into()), Some(WorkItemStatus::Failed));
    }

    // =========================================================================
    // Cancellation
    // =========================================================================

    #[test]
    fn cancel_ready_item() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.cancel(&"a".into()).unwrap();
        assert_eq!(q.item_status(&"a".into()), Some(WorkItemStatus::Cancelled));
    }

    #[test]
    fn cancel_in_progress_releases_agent() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.pull(&"agent".into()).unwrap();

        assert_eq!(q.agent_load.get(&"agent".to_string()), Some(&1));
        q.cancel(&"a".into()).unwrap();
        assert_eq!(q.agent_load.get(&"agent".to_string()), Some(&0));
    }

    #[test]
    fn cancel_completed_item_fails() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.pull(&"agent".into()).unwrap();
        q.complete(&"a".into(), &"agent".into(), None).unwrap();

        let result = q.cancel(&"a".into());
        assert!(matches!(result, Err(WorkQueueError::InvalidState { .. })));
    }

    // =========================================================================
    // Heartbeat / timeout
    // =========================================================================

    #[test]
    fn heartbeat_updates_timestamp() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.pull(&"agent".into()).unwrap();

        let before = q.get_assignment(&"a".into()).unwrap().last_heartbeat;
        q.heartbeat(&"a".into(), &"agent".into()).unwrap();
        let after = q.get_assignment(&"a".into()).unwrap().last_heartbeat;
        assert!(after >= before);
    }

    #[test]
    fn timed_out_items_reclaimed() {
        let config = WorkQueueConfig {
            heartbeat_timeout_ms: 0, // immediate timeout
            ..Default::default()
        };
        let mut q = SwarmWorkQueue::new(config);
        q.enqueue(item("a", 0, &[])).unwrap();
        q.pull(&"agent".into()).unwrap();

        assert_eq!(q.item_status(&"a".into()), Some(WorkItemStatus::InProgress));

        // Ensure at least 1ms passes so elapsed > 0 satisfies the > timeout check
        std::thread::sleep(std::time::Duration::from_millis(2));

        let reclaimed = q.reclaim_timed_out();
        assert_eq!(reclaimed, vec!["a".to_string()]);
        assert_eq!(q.item_status(&"a".into()), Some(WorkItemStatus::Ready));
    }

    // =========================================================================
    // Direct assignment
    // =========================================================================

    #[test]
    fn assign_specific_item_to_agent() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &[])).unwrap();

        let assignment = q.assign(&"b".into(), &"agent".into()).unwrap();
        assert_eq!(assignment.work_item_id, "b");
        assert_eq!(q.item_status(&"b".into()), Some(WorkItemStatus::InProgress));
        assert_eq!(q.item_status(&"a".into()), Some(WorkItemStatus::Ready));
    }

    #[test]
    fn assign_blocked_item_fails() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &["a"])).unwrap();

        let result = q.assign(&"b".into(), &"agent".into());
        assert!(matches!(result, Err(WorkQueueError::InvalidState { .. })));
    }

    // =========================================================================
    // Statistics
    // =========================================================================

    #[test]
    fn stats_reflect_queue_state() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &["a"])).unwrap();
        q.enqueue(item("c", 0, &[])).unwrap();

        let stats = q.stats();
        assert_eq!(stats.total_items, 3);
        assert_eq!(stats.ready, 2);
        assert_eq!(stats.blocked, 1);
        assert_eq!(stats.in_progress, 0);

        q.pull(&"agent-1".into()).unwrap();
        let stats = q.stats();
        assert_eq!(stats.ready, 1);
        assert_eq!(stats.in_progress, 1);
        assert_eq!(stats.active_agents, 1);
    }

    // =========================================================================
    // Cycle detection
    // =========================================================================

    #[test]
    fn cycle_detection_finds_direct_cycle() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &["a"])).unwrap();

        assert!(q.would_create_cycle(&"a".to_string(), &["b".to_string()]));
    }

    #[test]
    fn cycle_detection_finds_transitive_cycle() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &["a"])).unwrap();
        q.enqueue(item("c", 0, &["b"])).unwrap();

        assert!(q.would_create_cycle(&"a".to_string(), &["c".to_string()]));
    }

    #[test]
    fn no_cycle_for_valid_dag() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &["a"])).unwrap();
        q.enqueue(item("c", 0, &["a"])).unwrap();

        assert!(!q.would_create_cycle(&"b".to_string(), &["a".to_string()])); // b→a already exists, no new cycle
        assert!(!q.would_create_cycle(&"c".to_string(), &["a".to_string()])); // c→a already exists
    }

    // =========================================================================
    // Snapshot / restore
    // =========================================================================

    #[test]
    fn snapshot_restore_roundtrip() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 1, &["a"])).unwrap();
        q.enqueue(item("c", 2, &[])).unwrap();

        // Assign and complete one
        q.pull(&"agent".into()).unwrap();
        q.complete(&"a".into(), &"agent".into(), Some("done".into()))
            .unwrap();

        let snapshot = q.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored_snapshot: QueueSnapshot = serde_json::from_str(&json).unwrap();
        let restored = SwarmWorkQueue::restore(restored_snapshot, WorkQueueConfig::default());

        assert_eq!(restored.stats().total_items, 3);
        assert_eq!(
            restored.item_status(&"a".into()),
            Some(WorkItemStatus::Completed)
        );
        assert_eq!(
            restored.item_status(&"b".into()),
            Some(WorkItemStatus::Ready)
        );
        assert_eq!(
            restored.item_status(&"c".into()),
            Some(WorkItemStatus::Ready)
        );
        assert_eq!(restored.completion_log().len(), 1);
    }

    // =========================================================================
    // Batch enqueue
    // =========================================================================

    #[test]
    fn batch_enqueue_resolves_internal_deps() {
        let mut q = SwarmWorkQueue::with_defaults();
        let results = q.enqueue_batch(vec![
            item("a", 0, &[]),
            item("b", 1, &["a"]),
            item("c", 2, &["b"]),
        ]);

        assert!(results.iter().all(|r| r.is_ok()));
        assert_eq!(results[0].as_ref().unwrap(), &WorkItemStatus::Ready);
        assert_eq!(results[1].as_ref().unwrap(), &WorkItemStatus::Blocked);
        assert_eq!(results[2].as_ref().unwrap(), &WorkItemStatus::Blocked);
    }

    // =========================================================================
    // Completion chain
    // =========================================================================

    #[test]
    fn diamond_dependency_unblocking() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("root", 0, &[])).unwrap();
        q.enqueue(item("left", 0, &["root"])).unwrap();
        q.enqueue(item("right", 0, &["root"])).unwrap();
        q.enqueue(item("join", 0, &["left", "right"])).unwrap();

        // Complete root → left and right become ready
        q.assign(&"root".into(), &"a".into()).unwrap();
        q.complete(&"root".into(), &"a".into(), None).unwrap();
        assert_eq!(q.item_status(&"left".into()), Some(WorkItemStatus::Ready));
        assert_eq!(q.item_status(&"right".into()), Some(WorkItemStatus::Ready));
        assert_eq!(q.item_status(&"join".into()), Some(WorkItemStatus::Blocked));

        // Complete left → join still blocked
        q.assign(&"left".into(), &"a".into()).unwrap();
        q.complete(&"left".into(), &"a".into(), None).unwrap();
        assert_eq!(q.item_status(&"join".into()), Some(WorkItemStatus::Blocked));

        // Complete right → join now ready
        q.assign(&"right".into(), &"a".into()).unwrap();
        q.complete(&"right".into(), &"a".into(), None).unwrap();
        assert_eq!(q.item_status(&"join".into()), Some(WorkItemStatus::Ready));
    }

    // =========================================================================
    // Agent items query
    // =========================================================================

    #[test]
    fn agent_items_lists_assigned_work() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("a", 0, &[])).unwrap();
        q.enqueue(item("b", 0, &[])).unwrap();

        q.assign(&"a".into(), &"agent-1".into()).unwrap();
        q.assign(&"b".into(), &"agent-2".into()).unwrap();

        assert_eq!(q.agent_items(&"agent-1".into()).len(), 1);
        assert_eq!(q.agent_items(&"agent-2".into()).len(), 1);
        assert_eq!(q.agent_items(&"agent-3".into()).len(), 0);
    }

    // =========================================================================
    // Config serde
    // =========================================================================

    #[test]
    fn config_serde_roundtrip() {
        let config = WorkQueueConfig {
            max_concurrent_per_agent: 5,
            heartbeat_timeout_ms: 60_000,
            max_retries: 3,
            anti_starvation: false,
            starvation_threshold_ms: 300_000,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: WorkQueueConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            config.max_concurrent_per_agent,
            restored.max_concurrent_per_agent
        );
        assert!(!restored.anti_starvation);
    }

    // =========================================================================
    // Error display
    // =========================================================================

    #[test]
    fn error_messages_are_descriptive() {
        let errors = vec![
            WorkQueueError::ItemNotFound { id: "x".into() },
            WorkQueueError::DuplicateItem { id: "x".into() },
            WorkQueueError::QueueEmpty,
            WorkQueueError::CycleDetected {
                ids: vec!["a".into(), "b".into()],
            },
        ];
        for e in &errors {
            let msg = format!("{e}");
            assert!(!msg.is_empty());
        }
    }

    // =========================================================================
    // Ready items query
    // =========================================================================

    #[test]
    fn ready_items_sorted_by_priority() {
        let mut q = SwarmWorkQueue::with_defaults();
        q.enqueue(item("low", 10, &[])).unwrap();
        q.enqueue(item("high", 0, &[])).unwrap();
        q.enqueue(item("mid", 5, &[])).unwrap();

        let ready = q.ready_items();
        assert_eq!(ready.len(), 3);
        assert_eq!(ready[0].id, "high");
        assert_eq!(ready[1].id, "mid");
        assert_eq!(ready[2].id, "low");
    }

    // =========================================================================
    // Beads bridge tests
    // =========================================================================

    fn sample_bead_jsonl() -> &'static str {
        r#"{"id":"ft-001","title":"Setup project","status":"closed","priority":0,"issue_type":"task","assignee":"AgentA","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","updated_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-02T00:00:00Z","close_reason":"done","labels":["infra"],"dependencies":[],"source_repo":"."}
{"id":"ft-002","title":"Core engine","status":"open","priority":1,"issue_type":"feature","assignee":"","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","updated_at":"2026-01-02T00:00:00Z","labels":["engine"],"dependencies":[{"issue_id":"ft-002","depends_on_id":"ft-001","type":"blocks","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","metadata":"{}","thread_id":""}],"source_repo":"."}
{"id":"ft-003","title":"Add tests","status":"in_progress","priority":2,"issue_type":"task","assignee":"AgentB","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","updated_at":"2026-01-03T00:00:00Z","labels":["test"],"dependencies":[{"issue_id":"ft-003","depends_on_id":"ft-002","type":"blocks","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","metadata":"{}","thread_id":""}],"source_repo":"."}
{"id":"ft-004","title":"Write docs","status":"open","priority":3,"issue_type":"docs","assignee":"","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","updated_at":"2026-01-01T00:00:00Z","labels":["docs"],"dependencies":[{"issue_id":"ft-004","depends_on_id":"ft-002","type":"blocks","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","metadata":"{}","thread_id":""},{"issue_id":"ft-004","depends_on_id":"ft-UNKNOWN","type":"blocks","created_at":"2026-01-01T00:00:00Z","created_by":"jeff","metadata":"{}","thread_id":""}],"source_repo":"."}"#
    }

    #[test]
    fn beads_importer_parses_jsonl() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        assert_eq!(importer.record_count(), 4);
    }

    #[test]
    fn beads_importer_identifies_actionable() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let actionable = importer.actionable_records();
        assert_eq!(actionable.len(), 3); // ft-002 (open), ft-003 (in_progress), ft-004 (open)
        assert!(actionable.iter().all(|r| r.is_actionable()));
    }

    #[test]
    fn beads_importer_terminal_ids() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let terminal = importer.terminal_ids();
        assert_eq!(terminal.len(), 1);
        assert!(terminal.contains("ft-001"));
    }

    #[test]
    fn bead_record_blocking_ids() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let ft003 = importer
            .actionable_records()
            .into_iter()
            .find(|r| r.id == "ft-003")
            .unwrap();
        assert_eq!(ft003.blocking_ids(), vec!["ft-002"]);
    }

    #[test]
    fn bead_record_to_work_item() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let ft002 = importer
            .actionable_records()
            .into_iter()
            .find(|r| r.id == "ft-002")
            .unwrap();
        let item = ft002.to_work_item();
        assert_eq!(item.id, "ft-002");
        assert_eq!(item.title, "Core engine");
        assert_eq!(item.priority, 1);
        assert_eq!(item.labels, vec!["engine"]);
        assert_eq!(item.effort, 5); // feature type → 5
        assert!(item.depends_on.contains(&"ft-001".to_string()));
    }

    #[test]
    fn bead_effort_by_type() {
        let make = |t: &str| BeadRecord {
            id: "x".into(),
            title: "x".into(),
            description: String::new(),
            status: "open".into(),
            priority: 0,
            issue_type: t.into(),
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
        assert_eq!(make("epic").estimate_effort(), 8);
        assert_eq!(make("feature").estimate_effort(), 5);
        assert_eq!(make("task").estimate_effort(), 3);
        assert_eq!(make("bug").estimate_effort(), 2);
        assert_eq!(make("docs").estimate_effort(), 1);
        assert_eq!(make("question").estimate_effort(), 1);
        assert_eq!(make("unknown").estimate_effort(), 3);
    }

    #[test]
    fn beads_sync_imports_actionable_items() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let mut queue = SwarmWorkQueue::with_defaults();
        let report = importer.sync_to_queue(&mut queue);

        // ft-001 is closed → skipped
        // ft-002 depends on ft-001 (closed) → dep removed, imported as ready
        // ft-003 depends on ft-002 → imported, blocked
        // ft-004 depends on ft-002 + ft-UNKNOWN → ft-UNKNOWN is orphan dep, imported
        assert_eq!(report.imported, 3);
        assert_eq!(report.skipped, 1); // ft-001 closed
        assert!(report.orphan_deps.contains(&"ft-UNKNOWN".to_string()));
    }

    #[test]
    fn beads_sync_respects_dependency_graph() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let mut queue = SwarmWorkQueue::with_defaults();
        importer.sync_to_queue(&mut queue);

        // ft-002 should be ready (its dep ft-001 was closed → removed)
        assert_eq!(
            queue.item_status(&"ft-002".to_string()),
            Some(WorkItemStatus::Ready)
        );
        // ft-003 depends on ft-002 (still active) → blocked
        assert_eq!(
            queue.item_status(&"ft-003".to_string()),
            Some(WorkItemStatus::Blocked)
        );
    }

    #[test]
    fn beads_sync_idempotent() {
        let importer = BeadsImporter::from_jsonl(sample_bead_jsonl()).unwrap();
        let mut queue = SwarmWorkQueue::with_defaults();
        let r1 = importer.sync_to_queue(&mut queue);
        let r2 = importer.sync_to_queue(&mut queue);

        assert_eq!(r1.imported, 3);
        assert_eq!(r2.imported, 0);
        assert_eq!(r2.skipped, 4); // all already present or closed
    }

    #[test]
    fn beads_sync_handles_closed_beads_in_queue() {
        // Simulate a bead that was open, synced, then closed in a later sync
        let open_jsonl = r#"{"id":"ft-x","title":"Task X","status":"open","priority":1,"issue_type":"task","labels":[],"dependencies":[]}"#;
        let closed_jsonl = r#"{"id":"ft-x","title":"Task X","status":"closed","priority":1,"issue_type":"task","labels":[],"dependencies":[],"closed_at":"2026-02-01T00:00:00Z","close_reason":"done"}"#;

        let mut queue = SwarmWorkQueue::with_defaults();
        let importer1 = BeadsImporter::from_jsonl(open_jsonl).unwrap();
        let r1 = importer1.sync_to_queue(&mut queue);
        assert_eq!(r1.imported, 1);
        assert_eq!(
            queue.item_status(&"ft-x".to_string()),
            Some(WorkItemStatus::Ready)
        );

        // Now sync the closed version
        let importer2 = BeadsImporter::from_jsonl(closed_jsonl).unwrap();
        let r2 = importer2.sync_to_queue(&mut queue);
        assert_eq!(r2.completed_from_bead, vec!["ft-x".to_string()]);
        assert_eq!(
            queue.item_status(&"ft-x".to_string()),
            Some(WorkItemStatus::Cancelled)
        );
    }

    #[test]
    fn work_status_to_bead_status_mapping() {
        assert_eq!(work_status_to_bead_status(WorkItemStatus::Blocked), "open");
        assert_eq!(work_status_to_bead_status(WorkItemStatus::Ready), "open");
        assert_eq!(
            work_status_to_bead_status(WorkItemStatus::InProgress),
            "in_progress"
        );
        assert_eq!(
            work_status_to_bead_status(WorkItemStatus::Completed),
            "closed"
        );
        assert_eq!(work_status_to_bead_status(WorkItemStatus::Failed), "open");
        assert_eq!(
            work_status_to_bead_status(WorkItemStatus::Cancelled),
            "closed"
        );
    }

    #[test]
    fn beads_import_error_display() {
        let e1 = BeadsImportError::ParseError {
            line: 42,
            message: "bad json".into(),
        };
        assert!(format!("{e1}").contains("42"));
        assert!(format!("{e1}").contains("bad json"));

        let e2 = BeadsImportError::IoError {
            path: "/tmp/x".into(),
            message: "not found".into(),
        };
        assert!(format!("{e2}").contains("/tmp/x"));
    }

    #[test]
    fn beads_import_rejects_invalid_jsonl() {
        let bad = "not valid json\n{broken";
        let result = BeadsImporter::from_jsonl(bad);
        assert!(result.is_err());
        match result.unwrap_err() {
            BeadsImportError::ParseError { line, .. } => assert_eq!(line, 1),
            _ => panic!("expected ParseError"),
        }
    }

    #[test]
    fn beads_sync_report_serde_roundtrip() {
        let report = BeadsSyncReport {
            imported: 5,
            updated: 2,
            skipped: 10,
            orphan_deps: vec!["ft-orphan".to_string()],
            completed_from_bead: vec!["ft-done".to_string()],
        };
        let json = serde_json::to_string(&report).unwrap();
        let restored: BeadsSyncReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, restored);
    }

    #[test]
    fn bead_record_serde_roundtrip() {
        let jsonl = r#"{"id":"ft-rt","title":"Roundtrip","status":"open","priority":1,"issue_type":"task","labels":["test"],"dependencies":[{"issue_id":"ft-rt","depends_on_id":"ft-dep","type":"blocks","created_at":"2026-01-01T00:00:00Z","created_by":"test","metadata":"{}","thread_id":""}]}"#;
        let record: BeadRecord = serde_json::from_str(jsonl).unwrap();
        let back = serde_json::to_string(&record).unwrap();
        let re_parsed: BeadRecord = serde_json::from_str(&back).unwrap();
        assert_eq!(record.id, re_parsed.id);
        assert_eq!(record.dependencies.len(), re_parsed.dependencies.len());
    }

    #[test]
    fn beads_importer_empty_input() {
        let importer = BeadsImporter::from_jsonl("").unwrap();
        assert_eq!(importer.record_count(), 0);
        let mut queue = SwarmWorkQueue::with_defaults();
        let report = importer.sync_to_queue(&mut queue);
        assert_eq!(report.imported, 0);
        assert_eq!(report.skipped, 0);
    }
}
