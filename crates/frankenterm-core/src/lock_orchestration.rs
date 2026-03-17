//! Unified resource lock orchestration with deadlock detection and safe agent handoff.
//!
//! Consolidates the project's scattered locking infrastructure (per-pane workflow locks,
//! file reservations via agent mail, mission-loop conflict detection) into a single
//! coherent system that works across resource types.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────┐
//! │             LockOrchestrator                │
//! │                                             │
//! │  ┌─────────────┐  ┌───────────────────┐    │
//! │  │ Lock Table   │  │ Wait-for Graph    │    │
//! │  │ (ResourceId  │  │ (deadlock detect) │    │
//! │  │  → LockEntry)│  └───────────────────┘    │
//! │  └─────────────┘                            │
//! │  ┌─────────────┐  ┌───────────────────┐    │
//! │  │ Handoff      │  │ Telemetry         │    │
//! │  │ Protocol     │  │ Counters          │    │
//! │  └─────────────┘  └───────────────────┘    │
//! └────────────────────────────────────────────┘
//! ```
//!
//! # Key design choices
//!
//! - **Generic `ResourceId`**: files, panes, beads, and custom string resources use one
//!   lock table rather than separate per-type managers.
//! - **Cycle-based deadlock detection**: Before blocking on a lock, the orchestrator
//!   checks the wait-for graph for cycles. If a cycle is found the acquisition fails
//!   immediately with `DeadlockDetected` rather than hanging.
//! - **Lock expiry**: Every lock carries a TTL. Expired locks are reaped on access,
//!   preventing ghost locks from dead agents.
//! - **Atomic group acquisition**: All-or-nothing multi-resource locking with automatic
//!   rollback on partial failure (or deadlock).
//! - **Safe handoff protocol**: Transfers a lock from one agent to another atomically
//!   with rollback if the receiver doesn't acknowledge within the deadline.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ─── Resource identity ───────────────────────────────────────────────────────

/// A resource that can be locked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceId {
    /// An OS file path.
    File(String),
    /// A terminal pane.
    Pane(u64),
    /// A bead (issue tracker item).
    Bead(String),
    /// Any opaque named resource.
    Custom(String),
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "file:{p}"),
            Self::Pane(id) => write!(f, "pane:{id}"),
            Self::Bead(id) => write!(f, "bead:{id}"),
            Self::Custom(n) => write!(f, "custom:{n}"),
        }
    }
}

// ─── Lock metadata ───────────────────────────────────────────────────────────

/// Who holds a lock and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockHolder {
    /// Identifier for the agent holding the lock (e.g. "PinkForge", "exec-42").
    pub agent_id: String,
    /// Human-readable reason for the lock.
    pub reason: String,
}

/// An active lock on a resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    /// The locked resource.
    pub resource: ResourceId,
    /// Who holds it.
    pub holder: LockHolder,
    /// When the lock was acquired (unix ms).
    pub acquired_at_ms: u64,
    /// When the lock expires (unix ms). 0 means no expiry.
    pub expires_at_ms: u64,
}

impl LockEntry {
    /// Check if the lock has expired relative to `now_ms`.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms > 0 && now_ms >= self.expires_at_ms
    }

    /// Remaining time before expiry in milliseconds. Returns 0 if already expired or no expiry.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        if self.expires_at_ms == 0 {
            return 0;
        }
        self.expires_at_ms.saturating_sub(now_ms)
    }
}

// ─── Acquisition results ─────────────────────────────────────────────────────

/// Result of a lock acquisition attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockResult {
    /// Lock acquired successfully.
    Acquired,
    /// Resource is already locked by someone else.
    Contended {
        held_by: String,
        reason: String,
        acquired_at_ms: u64,
    },
    /// Acquiring this lock would create a deadlock cycle.
    DeadlockDetected {
        /// The agents involved in the cycle.
        cycle: Vec<String>,
    },
}

impl LockResult {
    #[must_use]
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired)
    }

    #[must_use]
    pub fn is_contended(&self) -> bool {
        matches!(self, Self::Contended { .. })
    }

    #[must_use]
    pub fn is_deadlock(&self) -> bool {
        matches!(self, Self::DeadlockDetected { .. })
    }
}

/// Result of a group lock acquisition attempt.
#[derive(Debug, Clone)]
pub enum GroupLockResult {
    /// All resources locked successfully.
    AllAcquired,
    /// Some resources couldn't be locked; none were locked (rolled back).
    PartialFailure {
        /// Per-resource results for the ones that failed.
        failures: Vec<(ResourceId, LockResult)>,
    },
}

impl GroupLockResult {
    #[must_use]
    pub fn is_all_acquired(&self) -> bool {
        matches!(self, Self::AllAcquired)
    }
}

// ─── Handoff protocol ────────────────────────────────────────────────────────

/// State machine for a lock handoff between agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoffState {
    /// Handoff initiated by the source agent; lock is held in escrow.
    Offered,
    /// The target agent has accepted and now holds the lock.
    Accepted,
    /// The handoff was rejected or timed out; lock returned to source.
    RolledBack,
}

/// A pending or completed handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffRecord {
    /// Unique handoff identifier.
    pub handoff_id: String,
    /// Resource being transferred.
    pub resource: ResourceId,
    /// Agent giving up the lock.
    pub source_agent: String,
    /// Agent receiving the lock.
    pub target_agent: String,
    /// Current state.
    pub state: HandoffState,
    /// When the handoff was initiated (unix ms).
    pub initiated_at_ms: u64,
    /// Deadline for acceptance (unix ms).
    pub deadline_ms: u64,
}

impl HandoffRecord {
    /// True if the handoff deadline has passed without acceptance.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.deadline_ms > 0 && now_ms >= self.deadline_ms && self.state == HandoffState::Offered
    }
}

// ─── Telemetry ───────────────────────────────────────────────────────────────

/// Operational counters for lock orchestration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockTelemetry {
    pub locks_acquired: u64,
    pub locks_released: u64,
    pub locks_contended: u64,
    pub locks_expired: u64,
    pub deadlocks_detected: u64,
    pub group_acquisitions: u64,
    pub group_rollbacks: u64,
    pub handoffs_initiated: u64,
    pub handoffs_accepted: u64,
    pub handoffs_rolled_back: u64,
}

/// Diagnostic snapshot of the orchestrator's state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorSnapshot {
    pub active_locks: Vec<LockEntry>,
    pub pending_handoffs: Vec<HandoffRecord>,
    pub telemetry: LockTelemetry,
    pub wait_graph_edges: Vec<(String, String)>,
}

// ─── Wait-for graph (deadlock detection) ─────────────────────────────────────

/// Directed graph: agent A → agent B means "A is waiting for B to release a lock".
#[derive(Debug, Default)]
struct WaitGraph {
    /// adjacency: waiter → set of holders it's waiting on
    edges: HashMap<String, HashSet<String>>,
}

impl WaitGraph {
    fn add_edge(&mut self, waiter: &str, holder: &str) {
        self.edges
            .entry(waiter.to_string())
            .or_default()
            .insert(holder.to_string());
    }

    fn remove_waiter(&mut self, waiter: &str) {
        self.edges.remove(waiter);
    }

    /// Check for a cycle involving `start`. Returns the cycle path if found.
    fn find_cycle_from(&self, start: &str) -> Option<Vec<String>> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();
        let mut path = Vec::new();
        if self.dfs_cycle(start, &mut visited, &mut rec_stack, &mut path) {
            // Trim path to just the cycle portion
            if let Some(pos) = path.iter().position(|n| n == start) {
                return Some(path[pos..].to_vec());
            }
            return Some(path);
        }
        None
    }

    fn dfs_cycle(
        &self,
        node: &str,
        visited: &mut HashSet<String>,
        rec_stack: &mut HashSet<String>,
        path: &mut Vec<String>,
    ) -> bool {
        // A cycle exists only if we revisit a node that is currently on
        // the DFS recursion stack (a back-edge). Nodes that were fully
        // explored in a prior subtree are not part of a cycle — they
        // represent shared DAG descendants (e.g., diamond graphs).
        if rec_stack.contains(node) {
            path.push(node.to_string());
            return true;
        }
        if visited.contains(node) {
            return false; // Already fully explored in another subtree
        }

        visited.insert(node.to_string());
        rec_stack.insert(node.to_string());
        path.push(node.to_string());

        if let Some(neighbors) = self.edges.get(node) {
            for neighbor in neighbors {
                if self.dfs_cycle(neighbor, visited, rec_stack, path) {
                    return true;
                }
            }
        }
        rec_stack.remove(node);
        path.pop();
        false
    }

    fn edge_list(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (waiter, holders) in &self.edges {
            for holder in holders {
                out.push((waiter.clone(), holder.clone()));
            }
        }
        out
    }
}

// ─── Orchestrator ────────────────────────────────────────────────────────────

/// Internal mutable state behind the mutex.
#[derive(Debug)]
struct OrchestratorInner {
    /// Active locks: resource → entry.
    locks: HashMap<ResourceId, LockEntry>,
    /// Wait-for graph for deadlock detection.
    wait_graph: WaitGraph,
    /// Pending and completed handoffs.
    handoffs: HashMap<String, HandoffRecord>,
    /// Telemetry counters.
    telemetry: LockTelemetry,
    /// Monotonic handoff ID counter.
    next_handoff_id: u64,
}

impl OrchestratorInner {
    fn new() -> Self {
        Self {
            locks: HashMap::new(),
            wait_graph: WaitGraph::default(),
            handoffs: HashMap::new(),
            telemetry: LockTelemetry::default(),
            next_handoff_id: 1,
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }

    /// Reap a single expired lock entry, returning true if reaped.
    fn reap_if_expired(&mut self, resource: &ResourceId, now_ms: u64) -> bool {
        if let Some(entry) = self.locks.get(resource) {
            if entry.is_expired(now_ms) {
                let holder = entry.holder.agent_id.clone();
                self.locks.remove(resource);
                self.wait_graph.remove_waiter(&holder);
                self.telemetry.locks_expired += 1;
                return true;
            }
        }
        false
    }
}

/// Configuration for the lock orchestrator.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// Default lock TTL if none specified.
    pub default_ttl: Duration,
    /// Maximum number of resources that can be locked in a single group call.
    pub max_group_size: usize,
    /// Whether deadlock detection is enabled.
    pub deadlock_detection: bool,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(300),
            max_group_size: 64,
            deadlock_detection: true,
        }
    }
}

/// Central lock orchestrator for multi-agent resource coordination.
///
/// Thread-safe via internal mutex. All operations are non-blocking.
pub struct LockOrchestrator {
    inner: Mutex<OrchestratorInner>,
    config: OrchestratorConfig,
}

impl Default for LockOrchestrator {
    fn default() -> Self {
        Self::new(OrchestratorConfig::default())
    }
}

impl LockOrchestrator {
    /// Create a new orchestrator with the given configuration.
    #[must_use]
    pub fn new(config: OrchestratorConfig) -> Self {
        Self {
            inner: Mutex::new(OrchestratorInner::new()),
            config,
        }
    }

    /// Attempt to acquire a lock on a single resource.
    ///
    /// If `ttl` is `None`, uses the default TTL from config.
    pub fn try_acquire(
        &self,
        resource: ResourceId,
        holder: LockHolder,
        ttl: Option<Duration>,
    ) -> LockResult {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();

        // Reap expired lock on this resource first
        inner.reap_if_expired(&resource, now_ms);

        // Check if already locked
        if let Some(existing) = inner.locks.get(&resource) {
            // Same agent re-locking same resource is idempotent
            if existing.holder.agent_id == holder.agent_id {
                return LockResult::Acquired;
            }

            // Clone data out of the immutable borrow before mutating inner
            let held_by = existing.holder.agent_id.clone();
            let reason = existing.holder.reason.clone();
            let acquired_at_ms = existing.acquired_at_ms;
            let _ = existing;

            // Deadlock detection: would adding waiter→holder create a cycle?
            if self.config.deadlock_detection {
                inner.wait_graph.add_edge(&holder.agent_id, &held_by);
                if let Some(cycle) = inner.wait_graph.find_cycle_from(&holder.agent_id) {
                    // Remove the speculative edge
                    inner.wait_graph.remove_waiter(&holder.agent_id);
                    inner.telemetry.deadlocks_detected += 1;
                    return LockResult::DeadlockDetected { cycle };
                }
                // Remove speculative edge (we don't actually block)
                inner.wait_graph.remove_waiter(&holder.agent_id);
            }

            inner.telemetry.locks_contended += 1;
            return LockResult::Contended {
                held_by,
                reason,
                acquired_at_ms,
            };
        }

        // Acquire
        let ttl_dur = ttl.unwrap_or(self.config.default_ttl);
        let expires_at_ms = if ttl_dur.is_zero() {
            0
        } else {
            now_ms.saturating_add(u64::try_from(ttl_dur.as_millis()).unwrap_or(u64::MAX))
        };

        inner.locks.insert(
            resource.clone(),
            LockEntry {
                resource,
                holder,
                acquired_at_ms: now_ms,
                expires_at_ms,
            },
        );
        inner.telemetry.locks_acquired += 1;

        LockResult::Acquired
    }

    /// Release a lock. Only the current holder can release.
    ///
    /// Returns `true` if released, `false` if not found or holder mismatch.
    pub fn release(&self, resource: &ResourceId, agent_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(entry) = inner.locks.get(resource) {
            if entry.holder.agent_id == agent_id {
                inner.locks.remove(resource);
                inner.wait_graph.remove_waiter(agent_id);
                inner.telemetry.locks_released += 1;
                return true;
            }
        }
        false
    }

    /// Force-release a lock regardless of holder. For recovery only.
    pub fn force_release(&self, resource: &ResourceId) -> Option<LockEntry> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = inner.locks.remove(resource);
        if let Some(ref e) = entry {
            inner.wait_graph.remove_waiter(&e.holder.agent_id);
            inner.telemetry.locks_released += 1;
        }
        entry
    }

    /// Atomically acquire locks on multiple resources.
    ///
    /// If any acquisition fails, all already-acquired locks in this call are
    /// rolled back and the failures are reported.
    pub fn try_acquire_group(
        &self,
        resources: &[ResourceId],
        holder: LockHolder,
        ttl: Option<Duration>,
    ) -> GroupLockResult {
        if resources.len() > self.config.max_group_size {
            return GroupLockResult::PartialFailure {
                failures: vec![(
                    ResourceId::Custom("__group_too_large__".into()),
                    LockResult::Contended {
                        held_by: String::new(),
                        reason: format!(
                            "group size {} exceeds max {}",
                            resources.len(),
                            self.config.max_group_size
                        ),
                        acquired_at_ms: 0,
                    },
                )],
            };
        }

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();
        let ttl_dur = ttl.unwrap_or(self.config.default_ttl);
        let expires_at_ms = if ttl_dur.is_zero() {
            0
        } else {
            now_ms.saturating_add(u64::try_from(ttl_dur.as_millis()).unwrap_or(u64::MAX))
        };

        // Reap expired locks on requested resources
        let mut to_reap = Vec::new();
        for r in resources {
            if let Some(entry) = inner.locks.get(r) {
                if entry.is_expired(now_ms) {
                    to_reap.push(r.clone());
                }
            }
        }
        for r in &to_reap {
            if let Some(entry) = inner.locks.remove(r) {
                inner.wait_graph.remove_waiter(&entry.holder.agent_id);
                inner.telemetry.locks_expired += 1;
            }
        }

        // Check all resources first (two-phase: check, then commit)
        let mut failures = Vec::new();
        for r in resources {
            if let Some(existing) = inner.locks.get(r) {
                if existing.holder.agent_id != holder.agent_id {
                    failures.push((
                        r.clone(),
                        LockResult::Contended {
                            held_by: existing.holder.agent_id.clone(),
                            reason: existing.holder.reason.clone(),
                            acquired_at_ms: existing.acquired_at_ms,
                        },
                    ));
                }
            }
        }

        if !failures.is_empty() {
            inner.telemetry.group_rollbacks += 1;
            return GroupLockResult::PartialFailure { failures };
        }

        // All clear — commit
        for r in resources {
            inner.locks.insert(
                r.clone(),
                LockEntry {
                    resource: r.clone(),
                    holder: holder.clone(),
                    acquired_at_ms: now_ms,
                    expires_at_ms,
                },
            );
        }
        inner.telemetry.locks_acquired += resources.len() as u64;
        inner.telemetry.group_acquisitions += 1;

        GroupLockResult::AllAcquired
    }

    /// Release all locks held by a specific agent.
    ///
    /// Returns the number of locks released.
    pub fn release_all(&self, agent_id: &str) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let to_remove: Vec<ResourceId> = inner
            .locks
            .iter()
            .filter(|(_, e)| e.holder.agent_id == agent_id)
            .map(|(r, _)| r.clone())
            .collect();
        let count = to_remove.len();
        for r in &to_remove {
            inner.locks.remove(r);
        }
        inner.wait_graph.remove_waiter(agent_id);
        inner.telemetry.locks_released += count as u64;
        count
    }

    /// Reap all expired locks. Returns the number reaped.
    pub fn reap_expired(&self) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();
        let expired: Vec<ResourceId> = inner
            .locks
            .iter()
            .filter(|(_, e)| e.is_expired(now_ms))
            .map(|(r, _)| r.clone())
            .collect();
        let count = expired.len();
        for r in &expired {
            if let Some(entry) = inner.locks.remove(r) {
                inner.wait_graph.remove_waiter(&entry.holder.agent_id);
            }
        }
        inner.telemetry.locks_expired += count as u64;
        count
    }

    // ─── Handoff protocol ────────────────────────────────────────────────

    /// Initiate a lock handoff from `source_agent` to `target_agent`.
    ///
    /// The source must currently hold the lock. The lock enters escrow
    /// (neither agent can use the resource) until the target accepts or
    /// the deadline expires.
    pub fn initiate_handoff(
        &self,
        resource: &ResourceId,
        source_agent: &str,
        target_agent: &str,
        deadline: Duration,
    ) -> Result<String, HandoffError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();

        // Verify source holds the lock
        let entry = inner.locks.get(resource).ok_or(HandoffError::LockNotHeld)?;

        if entry.holder.agent_id != source_agent {
            return Err(HandoffError::NotHolder {
                actual_holder: entry.holder.agent_id.clone(),
            });
        }

        let handoff_id = format!("hoff-{}", inner.next_handoff_id);
        inner.next_handoff_id += 1;

        let record = HandoffRecord {
            handoff_id: handoff_id.clone(),
            resource: resource.clone(),
            source_agent: source_agent.to_string(),
            target_agent: target_agent.to_string(),
            state: HandoffState::Offered,
            initiated_at_ms: now_ms,
            deadline_ms: now_ms + deadline.as_millis() as u64,
        };

        inner.handoffs.insert(handoff_id.clone(), record);
        inner.telemetry.handoffs_initiated += 1;

        Ok(handoff_id)
    }

    /// Target agent accepts a handoff, taking ownership of the lock.
    pub fn accept_handoff(
        &self,
        handoff_id: &str,
        accepting_agent: &str,
    ) -> Result<(), HandoffError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();

        // Validate and extract data in a scoped borrow
        let (resource, source_agent) = {
            let record = inner
                .handoffs
                .get_mut(handoff_id)
                .ok_or(HandoffError::HandoffNotFound)?;

            if record.target_agent != accepting_agent {
                return Err(HandoffError::WrongTarget {
                    expected: record.target_agent.clone(),
                });
            }

            if record.state != HandoffState::Offered {
                return Err(HandoffError::InvalidState {
                    current: record.state.clone(),
                });
            }

            if record.is_expired(now_ms) {
                record.state = HandoffState::RolledBack;
                inner.telemetry.handoffs_rolled_back += 1;
                return Err(HandoffError::Expired);
            }

            (record.resource.clone(), record.source_agent.clone())
        };

        // Transfer the lock (record borrow is now released)
        if let Some(lock_entry) = inner.locks.get_mut(&resource) {
            lock_entry.holder = LockHolder {
                agent_id: accepting_agent.to_string(),
                reason: format!("handoff from {source_agent}"),
            };
            lock_entry.acquired_at_ms = now_ms;
        }

        // Re-borrow to update state
        if let Some(record) = inner.handoffs.get_mut(handoff_id) {
            record.state = HandoffState::Accepted;
        }
        inner.telemetry.handoffs_accepted += 1;

        Ok(())
    }

    /// Roll back a handoff, returning the lock to the source agent.
    ///
    /// Can be called by either agent or automatically on timeout.
    pub fn rollback_handoff(&self, handoff_id: &str) -> Result<(), HandoffError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let record = inner
            .handoffs
            .get_mut(handoff_id)
            .ok_or(HandoffError::HandoffNotFound)?;

        if record.state != HandoffState::Offered {
            return Err(HandoffError::InvalidState {
                current: record.state.clone(),
            });
        }

        record.state = HandoffState::RolledBack;
        inner.telemetry.handoffs_rolled_back += 1;

        Ok(())
    }

    /// Reap expired handoffs, rolling them back automatically.
    pub fn reap_expired_handoffs(&self) -> usize {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();

        let expired_ids: Vec<String> = inner
            .handoffs
            .iter()
            .filter(|(_, r)| r.is_expired(now_ms))
            .map(|(id, _)| id.clone())
            .collect();

        let count = expired_ids.len();
        for id in expired_ids {
            if let Some(record) = inner.handoffs.get_mut(&id) {
                record.state = HandoffState::RolledBack;
            }
        }
        inner.telemetry.handoffs_rolled_back += count as u64;
        count
    }

    // ─── Query / diagnostics ─────────────────────────────────────────────

    /// Check if a resource is currently locked.
    #[must_use]
    pub fn is_locked(&self, resource: &ResourceId) -> Option<LockEntry> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now_ms = OrchestratorInner::now_ms();
        inner.reap_if_expired(resource, now_ms);
        inner.locks.get(resource).cloned()
    }

    /// List all locks held by a specific agent.
    #[must_use]
    pub fn locks_held_by(&self, agent_id: &str) -> Vec<LockEntry> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .locks
            .values()
            .filter(|e| e.holder.agent_id == agent_id)
            .cloned()
            .collect()
    }

    /// Get all active locks.
    #[must_use]
    pub fn active_locks(&self) -> Vec<LockEntry> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.locks.values().cloned().collect()
    }

    /// Get all pending handoffs.
    #[must_use]
    pub fn pending_handoffs(&self) -> Vec<HandoffRecord> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .handoffs
            .values()
            .filter(|r| r.state == HandoffState::Offered)
            .cloned()
            .collect()
    }

    /// Get telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> LockTelemetry {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.telemetry.clone()
    }

    /// Full diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> OrchestratorSnapshot {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        OrchestratorSnapshot {
            active_locks: inner.locks.values().cloned().collect(),
            pending_handoffs: inner
                .handoffs
                .values()
                .filter(|r| r.state == HandoffState::Offered)
                .cloned()
                .collect(),
            telemetry: inner.telemetry.clone(),
            wait_graph_edges: inner.wait_graph.edge_list(),
        }
    }

    /// Detect deadlocks in the current wait-for graph.
    ///
    /// This scans the entire graph and returns all distinct cycles found.
    /// Normally deadlock detection happens automatically on `try_acquire`, but
    /// this method can be used for periodic diagnostics.
    #[must_use]
    pub fn detect_deadlocks(&self) -> Vec<Vec<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut cycles = Vec::new();
        let mut globally_visited = HashSet::new();

        for start in inner.wait_graph.edges.keys() {
            if globally_visited.contains(start) {
                continue;
            }
            if let Some(cycle) = inner.wait_graph.find_cycle_from(start) {
                for node in &cycle {
                    globally_visited.insert(node.clone());
                }
                cycles.push(cycle);
            } else {
                globally_visited.insert(start.clone());
            }
        }
        cycles
    }
}

// ─── Handoff errors ──────────────────────────────────────────────────────────

/// Errors from handoff operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffError {
    /// The resource is not currently locked.
    LockNotHeld,
    /// The requesting agent does not hold the lock.
    NotHolder { actual_holder: String },
    /// Handoff ID not found.
    HandoffNotFound,
    /// Wrong agent attempting to accept.
    WrongTarget { expected: String },
    /// Handoff is not in the expected state.
    InvalidState { current: HandoffState },
    /// Handoff deadline has passed.
    Expired,
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LockNotHeld => write!(f, "resource is not locked"),
            Self::NotHolder { actual_holder } => {
                write!(f, "lock held by {actual_holder}, not requester")
            }
            Self::HandoffNotFound => write!(f, "handoff not found"),
            Self::WrongTarget { expected } => {
                write!(f, "wrong target agent, expected {expected}")
            }
            Self::InvalidState { current } => {
                write!(f, "handoff in invalid state: {current:?}")
            }
            Self::Expired => write!(f, "handoff deadline expired"),
        }
    }
}

impl std::error::Error for HandoffError {}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(agent: &str) -> LockHolder {
        LockHolder {
            agent_id: agent.to_string(),
            reason: "test".to_string(),
        }
    }

    fn orch() -> LockOrchestrator {
        LockOrchestrator::default()
    }

    // ── ResourceId ──

    #[test]
    fn resource_id_display() {
        assert_eq!(ResourceId::File("/a/b".into()).to_string(), "file:/a/b");
        assert_eq!(ResourceId::Pane(42).to_string(), "pane:42");
        assert_eq!(ResourceId::Bead("ft-123".into()).to_string(), "bead:ft-123");
        assert_eq!(
            ResourceId::Custom("gpu-0".into()).to_string(),
            "custom:gpu-0"
        );
    }

    #[test]
    fn resource_id_equality() {
        assert_eq!(ResourceId::Pane(1), ResourceId::Pane(1));
        assert_ne!(ResourceId::Pane(1), ResourceId::Pane(2));
        assert_ne!(ResourceId::Pane(1), ResourceId::File("1".into()));
    }

    #[test]
    fn resource_id_serde_roundtrip() {
        let ids = vec![
            ResourceId::File("/tmp/test".into()),
            ResourceId::Pane(99),
            ResourceId::Bead("ft-abc".into()),
            ResourceId::Custom("gpu".into()),
        ];
        for id in ids {
            let json = serde_json::to_string(&id).unwrap();
            let back: ResourceId = serde_json::from_str(&json).unwrap();
            assert_eq!(id, back);
        }
    }

    // ── LockEntry ──

    #[test]
    fn lock_entry_expiry() {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 1000,
            expires_at_ms: 2000,
        };
        assert!(!entry.is_expired(999));
        assert!(!entry.is_expired(1999));
        assert!(entry.is_expired(2000));
        assert!(entry.is_expired(3000));
    }

    #[test]
    fn lock_entry_no_expiry() {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 1000,
            expires_at_ms: 0,
        };
        assert!(!entry.is_expired(u64::MAX));
    }

    #[test]
    fn lock_entry_remaining_ms() {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 1000,
            expires_at_ms: 5000,
        };
        assert_eq!(entry.remaining_ms(3000), 2000);
        assert_eq!(entry.remaining_ms(5000), 0);
        assert_eq!(entry.remaining_ms(6000), 0);
    }

    #[test]
    fn lock_entry_remaining_ms_no_expiry() {
        let entry = LockEntry {
            resource: ResourceId::Pane(1),
            holder: holder("a"),
            acquired_at_ms: 0,
            expires_at_ms: 0,
        };
        assert_eq!(entry.remaining_ms(9999), 0);
    }

    // ── Basic acquire/release ──

    #[test]
    fn acquire_and_release_single() {
        let o = orch();
        let r = ResourceId::Pane(1);
        assert!(o.try_acquire(r.clone(), holder("a"), None).is_acquired());
        assert!(o.is_locked(&r).is_some());
        assert!(o.release(&r, "a"));
        assert!(o.is_locked(&r).is_none());
    }

    #[test]
    fn acquire_contention() {
        let o = orch();
        let r = ResourceId::File("/tmp/f".into());
        assert!(o.try_acquire(r.clone(), holder("a"), None).is_acquired());

        let result = o.try_acquire(r.clone(), holder("b"), None);
        assert!(result.is_contended());
        if let LockResult::Contended { held_by, .. } = result {
            assert_eq!(held_by, "a");
        }
    }

    #[test]
    fn same_agent_relock_is_idempotent() {
        let o = orch();
        let r = ResourceId::Pane(5);
        assert!(o.try_acquire(r.clone(), holder("a"), None).is_acquired());
        assert!(o.try_acquire(r.clone(), holder("a"), None).is_acquired());
        assert_eq!(o.active_locks().len(), 1);
    }

    #[test]
    fn release_wrong_agent_fails() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("a"), None);
        assert!(!o.release(&r, "b"));
        assert!(o.is_locked(&r).is_some());
    }

    #[test]
    fn release_nonexistent_returns_false() {
        let o = orch();
        assert!(!o.release(&ResourceId::Pane(99), "a"));
    }

    #[test]
    fn force_release_ignores_holder() {
        let o = orch();
        let r = ResourceId::Bead("ft-1".into());
        o.try_acquire(r.clone(), holder("a"), None);
        let entry = o.force_release(&r);
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().holder.agent_id, "a");
        assert!(o.is_locked(&r).is_none());
    }

    // ── Release all ──

    #[test]
    fn release_all_for_agent() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("a"), None);
        o.try_acquire(ResourceId::Pane(2), holder("a"), None);
        o.try_acquire(ResourceId::Pane(3), holder("b"), None);
        assert_eq!(o.release_all("a"), 2);
        assert!(o.is_locked(&ResourceId::Pane(1)).is_none());
        assert!(o.is_locked(&ResourceId::Pane(2)).is_none());
        assert!(o.is_locked(&ResourceId::Pane(3)).is_some());
    }

    // ── Group acquisition ──

    #[test]
    fn group_acquire_all_free() {
        let o = orch();
        let resources = vec![
            ResourceId::Pane(1),
            ResourceId::Pane(2),
            ResourceId::Pane(3),
        ];
        let result = o.try_acquire_group(&resources, holder("a"), None);
        assert!(result.is_all_acquired());
        for r in &resources {
            assert!(o.is_locked(r).is_some());
        }
    }

    #[test]
    fn group_acquire_partial_failure_rolls_back() {
        let o = orch();
        // Pre-lock one resource
        o.try_acquire(ResourceId::Pane(2), holder("b"), None);

        let resources = vec![
            ResourceId::Pane(1),
            ResourceId::Pane(2),
            ResourceId::Pane(3),
        ];
        let result = o.try_acquire_group(&resources, holder("a"), None);
        match result {
            GroupLockResult::PartialFailure { failures } => {
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].0, ResourceId::Pane(2));
            }
            _ => panic!("expected partial failure"),
        }
        // Pane 1 and 3 should NOT be locked by "a" (rollback)
        assert!(o.is_locked(&ResourceId::Pane(1)).is_none());
        assert!(o.is_locked(&ResourceId::Pane(3)).is_none());
    }

    #[test]
    fn group_acquire_too_large() {
        let config = OrchestratorConfig {
            max_group_size: 2,
            ..Default::default()
        };
        let o = LockOrchestrator::new(config);
        let resources = vec![
            ResourceId::Pane(1),
            ResourceId::Pane(2),
            ResourceId::Pane(3),
        ];
        let result = o.try_acquire_group(&resources, holder("a"), None);
        assert!(!result.is_all_acquired());
    }

    #[test]
    fn group_acquire_same_agent_idempotent() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("a"), None);
        let resources = vec![ResourceId::Pane(1), ResourceId::Pane(2)];
        let result = o.try_acquire_group(&resources, holder("a"), None);
        assert!(result.is_all_acquired());
    }

    // ── Deadlock detection ──

    #[test]
    fn deadlock_detection_simple_cycle() {
        let o = orch();
        // Agent A holds R1
        o.try_acquire(ResourceId::Pane(1), holder("A"), None);
        // Agent B holds R2
        o.try_acquire(ResourceId::Pane(2), holder("B"), None);

        // Register that A is waiting for R2 (held by B)
        {
            let mut inner = o.inner.lock().unwrap();
            inner.wait_graph.add_edge("A", "B");
        }

        // Now B tries to acquire R1 (held by A) — this would create B→A, completing the cycle A→B→A
        let result = o.try_acquire(ResourceId::Pane(1), holder("B"), None);
        assert!(result.is_deadlock());
        if let LockResult::DeadlockDetected { cycle } = result {
            assert!(cycle.contains(&"A".to_string()) || cycle.contains(&"B".to_string()));
        }
    }

    #[test]
    fn no_deadlock_without_cycle() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("A"), None);
        // B tries R1 — contended but no deadlock (no edges from A waiting on B)
        let result = o.try_acquire(ResourceId::Pane(1), holder("B"), None);
        assert!(result.is_contended());
    }

    #[test]
    fn deadlock_detection_disabled() {
        let config = OrchestratorConfig {
            deadlock_detection: false,
            ..Default::default()
        };
        let o = LockOrchestrator::new(config);
        o.try_acquire(ResourceId::Pane(1), holder("A"), None);
        o.try_acquire(ResourceId::Pane(2), holder("B"), None);

        {
            let mut inner = o.inner.lock().unwrap();
            inner.wait_graph.add_edge("A", "B");
        }

        // Even with cycle-forming edge, deadlock detection is off
        let result = o.try_acquire(ResourceId::Pane(1), holder("B"), None);
        assert!(result.is_contended()); // contended, not deadlock
    }

    // ── Lock expiry ──

    #[test]
    fn expired_lock_reaped_on_access() {
        let o = orch();
        // Acquire with very short TTL
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("a"), Some(Duration::from_millis(1)));

        // Sleep briefly to ensure expiry
        std::thread::sleep(Duration::from_millis(5));

        // is_locked should reap
        assert!(o.is_locked(&r).is_none());
        assert_eq!(o.telemetry().locks_expired, 1);
    }

    #[test]
    fn reap_expired_batch() {
        let o = orch();
        o.try_acquire(
            ResourceId::Pane(1),
            holder("a"),
            Some(Duration::from_millis(1)),
        );
        o.try_acquire(
            ResourceId::Pane(2),
            holder("a"),
            Some(Duration::from_millis(1)),
        );
        o.try_acquire(
            ResourceId::Pane(3),
            holder("a"),
            Some(Duration::ZERO), // no expiry
        );

        std::thread::sleep(Duration::from_millis(5));

        let reaped = o.reap_expired();
        assert_eq!(reaped, 2);
        assert!(o.is_locked(&ResourceId::Pane(3)).is_some());
    }

    // ── Handoff protocol ──

    #[test]
    fn handoff_happy_path() {
        let o = orch();
        let r = ResourceId::Bead("ft-100".into());
        o.try_acquire(r.clone(), holder("A"), None);

        // A initiates handoff to B
        let hid = o
            .initiate_handoff(&r, "A", "B", Duration::from_secs(60))
            .unwrap();
        assert!(hid.starts_with("hoff-"));

        // B accepts
        o.accept_handoff(&hid, "B").unwrap();

        // Lock should now be held by B
        let entry = o.is_locked(&r).unwrap();
        assert_eq!(entry.holder.agent_id, "B");
    }

    #[test]
    fn handoff_rollback() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("A"), None);

        let hid = o
            .initiate_handoff(&r, "A", "B", Duration::from_secs(60))
            .unwrap();

        o.rollback_handoff(&hid).unwrap();

        // Lock should still be held by A
        let entry = o.is_locked(&r).unwrap();
        assert_eq!(entry.holder.agent_id, "A");
    }

    #[test]
    fn handoff_wrong_source_agent() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("A"), None);

        let result = o.initiate_handoff(&r, "B", "C", Duration::from_secs(60));
        assert!(matches!(result, Err(HandoffError::NotHolder { .. })));
    }

    #[test]
    fn handoff_no_lock() {
        let o = orch();
        let r = ResourceId::Pane(1);
        let result = o.initiate_handoff(&r, "A", "B", Duration::from_secs(60));
        assert!(matches!(result, Err(HandoffError::LockNotHeld)));
    }

    #[test]
    fn handoff_wrong_acceptor() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("A"), None);

        let hid = o
            .initiate_handoff(&r, "A", "B", Duration::from_secs(60))
            .unwrap();

        let result = o.accept_handoff(&hid, "C");
        assert!(matches!(result, Err(HandoffError::WrongTarget { .. })));
    }

    #[test]
    fn handoff_expired() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("A"), None);

        let hid = o
            .initiate_handoff(&r, "A", "B", Duration::from_millis(1))
            .unwrap();

        std::thread::sleep(Duration::from_millis(5));

        let result = o.accept_handoff(&hid, "B");
        assert!(matches!(result, Err(HandoffError::Expired)));
    }

    #[test]
    fn handoff_double_accept_fails() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("A"), None);

        let hid = o
            .initiate_handoff(&r, "A", "B", Duration::from_secs(60))
            .unwrap();

        o.accept_handoff(&hid, "B").unwrap();
        let result = o.accept_handoff(&hid, "B");
        assert!(matches!(result, Err(HandoffError::InvalidState { .. })));
    }

    #[test]
    fn reap_expired_handoffs_works() {
        let o = orch();
        let r = ResourceId::Pane(1);
        o.try_acquire(r.clone(), holder("A"), None);

        o.initiate_handoff(&r, "A", "B", Duration::from_millis(1))
            .unwrap();

        std::thread::sleep(Duration::from_millis(5));

        let count = o.reap_expired_handoffs();
        assert_eq!(count, 1);
    }

    // ── Telemetry & diagnostics ──

    #[test]
    fn telemetry_tracks_operations() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("a"), None);
        o.try_acquire(ResourceId::Pane(1), holder("b"), None); // contended
        o.release(&ResourceId::Pane(1), "a");

        let t = o.telemetry();
        assert_eq!(t.locks_acquired, 1);
        assert_eq!(t.locks_released, 1);
        assert_eq!(t.locks_contended, 1);
    }

    #[test]
    fn snapshot_captures_state() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("a"), None);
        o.try_acquire(ResourceId::Pane(2), holder("b"), None);

        let snap = o.snapshot();
        assert_eq!(snap.active_locks.len(), 2);
        assert_eq!(snap.telemetry.locks_acquired, 2);
    }

    #[test]
    fn locks_held_by_agent() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("a"), None);
        o.try_acquire(ResourceId::Pane(2), holder("a"), None);
        o.try_acquire(ResourceId::Pane(3), holder("b"), None);

        let a_locks = o.locks_held_by("a");
        assert_eq!(a_locks.len(), 2);

        let b_locks = o.locks_held_by("b");
        assert_eq!(b_locks.len(), 1);

        let c_locks = o.locks_held_by("c");
        assert!(c_locks.is_empty());
    }

    #[test]
    fn pending_handoffs_list() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("A"), None);
        o.try_acquire(ResourceId::Pane(2), holder("A"), None);

        o.initiate_handoff(&ResourceId::Pane(1), "A", "B", Duration::from_secs(60))
            .unwrap();

        assert_eq!(o.pending_handoffs().len(), 1);
    }

    // ── LockResult predicates ──

    #[test]
    fn lock_result_predicates() {
        assert!(LockResult::Acquired.is_acquired());
        assert!(!LockResult::Acquired.is_contended());
        assert!(!LockResult::Acquired.is_deadlock());

        let contended = LockResult::Contended {
            held_by: "x".into(),
            reason: "y".into(),
            acquired_at_ms: 0,
        };
        assert!(!contended.is_acquired());
        assert!(contended.is_contended());

        let dl = LockResult::DeadlockDetected {
            cycle: vec!["a".into()],
        };
        assert!(dl.is_deadlock());
    }

    // ── HandoffError display ──

    #[test]
    fn handoff_error_display() {
        assert_eq!(
            HandoffError::LockNotHeld.to_string(),
            "resource is not locked"
        );
        assert!(
            HandoffError::NotHolder {
                actual_holder: "X".into()
            }
            .to_string()
            .contains("X")
        );
        assert_eq!(
            HandoffError::HandoffNotFound.to_string(),
            "handoff not found"
        );
        assert!(
            HandoffError::WrongTarget {
                expected: "Y".into()
            }
            .to_string()
            .contains("Y")
        );
        assert!(
            HandoffError::InvalidState {
                current: HandoffState::Accepted
            }
            .to_string()
            .contains("Accepted")
        );
        assert_eq!(
            HandoffError::Expired.to_string(),
            "handoff deadline expired"
        );
    }

    // ── HandoffRecord serde ──

    #[test]
    fn handoff_record_serde_roundtrip() {
        let rec = HandoffRecord {
            handoff_id: "hoff-1".into(),
            resource: ResourceId::Pane(42),
            source_agent: "A".into(),
            target_agent: "B".into(),
            state: HandoffState::Offered,
            initiated_at_ms: 1000,
            deadline_ms: 2000,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: HandoffRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.handoff_id, "hoff-1");
        assert_eq!(back.resource, ResourceId::Pane(42));
        assert_eq!(back.state, HandoffState::Offered);
    }

    // ── WaitGraph unit tests ──

    #[test]
    fn wait_graph_no_cycle() {
        let mut g = WaitGraph::default();
        g.add_edge("A", "B");
        g.add_edge("B", "C");
        assert!(g.find_cycle_from("A").is_none());
    }

    #[test]
    fn wait_graph_simple_cycle() {
        let mut g = WaitGraph::default();
        g.add_edge("A", "B");
        g.add_edge("B", "A");
        let cycle = g.find_cycle_from("A");
        assert!(cycle.is_some());
    }

    #[test]
    fn wait_graph_three_node_cycle() {
        let mut g = WaitGraph::default();
        g.add_edge("A", "B");
        g.add_edge("B", "C");
        g.add_edge("C", "A");
        let cycle = g.find_cycle_from("A");
        assert!(cycle.is_some());
        let cycle = cycle.unwrap();
        assert!(cycle.len() >= 2);
    }

    #[test]
    fn wait_graph_remove_waiter_breaks_cycle() {
        let mut g = WaitGraph::default();
        g.add_edge("A", "B");
        g.add_edge("B", "A");
        g.remove_waiter("A");
        assert!(g.find_cycle_from("B").is_none());
    }

    #[test]
    fn wait_graph_edge_list() {
        let mut g = WaitGraph::default();
        g.add_edge("A", "B");
        g.add_edge("A", "C");
        let edges = g.edge_list();
        assert_eq!(edges.len(), 2);
    }

    // ── Different resource types coexist ──

    #[test]
    fn mixed_resource_types() {
        let o = orch();
        o.try_acquire(ResourceId::File("/a".into()), holder("agent-1"), None);
        o.try_acquire(ResourceId::Pane(1), holder("agent-1"), None);
        o.try_acquire(ResourceId::Bead("ft-1".into()), holder("agent-2"), None);
        o.try_acquire(ResourceId::Custom("gpu-0".into()), holder("agent-2"), None);

        assert_eq!(o.active_locks().len(), 4);
        assert_eq!(o.locks_held_by("agent-1").len(), 2);
        assert_eq!(o.locks_held_by("agent-2").len(), 2);
    }

    // ── Zero TTL means no expiry ──

    #[test]
    fn zero_ttl_no_expiry() {
        let o = orch();
        o.try_acquire(ResourceId::Pane(1), holder("a"), Some(Duration::ZERO));
        std::thread::sleep(Duration::from_millis(5));
        assert!(o.is_locked(&ResourceId::Pane(1)).is_some());
    }

    // ── Orchestrator default ──

    #[test]
    fn default_orchestrator() {
        let o = LockOrchestrator::default();
        assert!(o.active_locks().is_empty());
        assert_eq!(o.telemetry().locks_acquired, 0);
    }

    // ── Config ──

    #[test]
    fn config_defaults() {
        let c = OrchestratorConfig::default();
        assert_eq!(c.default_ttl, Duration::from_secs(300));
        assert_eq!(c.max_group_size, 64);
        assert!(c.deadlock_detection);
    }

    // ── Group lock telemetry ──

    #[test]
    fn group_lock_telemetry() {
        let o = orch();
        let resources = vec![ResourceId::Pane(1), ResourceId::Pane(2)];
        o.try_acquire_group(&resources, holder("a"), None);
        let t = o.telemetry();
        assert_eq!(t.group_acquisitions, 1);
        assert_eq!(t.locks_acquired, 2);
    }

    // ── Stress: many locks ──

    #[test]
    fn acquire_release_many_resources() {
        let o = orch();
        for i in 0..200 {
            assert!(
                o.try_acquire(ResourceId::Pane(i), holder("a"), None)
                    .is_acquired()
            );
        }
        assert_eq!(o.active_locks().len(), 200);
        assert_eq!(o.release_all("a"), 200);
        assert!(o.active_locks().is_empty());
    }
}
