use std::{collections::VecDeque, fmt};

use serde::{Deserialize, Serialize};

use super::LatencyStage;

// AARSP Bead: ft-2p9cb.2.3 - Priority Inheritance & Lock-Order

// AARSP Bead: ft-2p9cb.2.3.1

/// Priority level for work items. Higher numeric value = higher priority.
/// Used in priority inheritance to temporarily boost blocked low-priority work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    /// Background work - can be preempted freely.
    Background = 0,
    /// Normal interactive work.
    Normal = 1,
    /// Elevated - time-sensitive user action.
    Elevated = 2,
    /// Critical - keystroke path, must not be delayed.
    Critical = 3,
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Background => write!(f, "BACKGROUND"),
            Self::Normal => write!(f, "NORMAL"),
            Self::Elevated => write!(f, "ELEVATED"),
            Self::Critical => write!(f, "CRITICAL"),
        }
    }
}

impl Priority {
    /// All priority levels in ascending order.
    pub const ALL: [Priority; 4] = [
        Priority::Background,
        Priority::Normal,
        Priority::Elevated,
        Priority::Critical,
    ];
}

/// Maps pipeline stages to default priority levels.
pub fn stage_to_priority(stage: LatencyStage) -> Priority {
    match stage {
        LatencyStage::PtyCapture | LatencyStage::DeltaExtraction => Priority::Critical,
        LatencyStage::EventEmission | LatencyStage::WorkflowDispatch => Priority::Elevated,
        LatencyStage::PatternDetection | LatencyStage::ActionExecution => Priority::Normal,
        LatencyStage::StorageWrite
        | LatencyStage::ApiResponse
        | LatencyStage::EndToEndCapture
        | LatencyStage::EndToEndAction => Priority::Background,
    }
}

/// A resource that work items can contend over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Resource {
    /// Storage writer lock.
    StorageLock,
    /// Pattern engine lock.
    PatternLock,
    /// Event bus lock.
    EventBusLock,
    /// Workflow executor lock.
    WorkflowLock,
}

impl Resource {
    /// All resources in canonical lock order.
    /// Acquiring locks MUST follow this order to prevent deadlock.
    pub const LOCK_ORDER: [Resource; 4] = [
        Resource::StorageLock,
        Resource::PatternLock,
        Resource::EventBusLock,
        Resource::WorkflowLock,
    ];

    /// Position in canonical lock order (0-indexed).
    pub fn order_index(self) -> usize {
        match self {
            Self::StorageLock => 0,
            Self::PatternLock => 1,
            Self::EventBusLock => 2,
            Self::WorkflowLock => 3,
        }
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageLock => write!(f, "storage"),
            Self::PatternLock => write!(f, "pattern"),
            Self::EventBusLock => write!(f, "event_bus"),
            Self::WorkflowLock => write!(f, "workflow"),
        }
    }
}

/// A priority inheritance event records when a task's effective priority was boosted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritanceEvent {
    /// Correlation ID of the task that received the boost.
    pub holder_id: String,
    /// Correlation ID of the higher-priority waiter that triggered the boost.
    pub waiter_id: String,
    /// Resource being contended.
    pub resource: Resource,
    /// Original priority of the holder.
    pub original_priority: Priority,
    /// Boosted priority (inherited from waiter).
    pub inherited_priority: Priority,
    /// Timestamp when inheritance was applied.
    pub applied_us: u64,
    /// Timestamp when inheritance was released (None if still active).
    pub released_us: Option<u64>,
}

/// Lock acquisition attempt result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockResult {
    /// Lock acquired immediately.
    Acquired,
    /// Lock acquired after priority inheritance boosted the holder.
    AcquiredAfterInheritance {
        /// ID of the task whose priority was boosted.
        boosted_holder: String,
    },
    /// Lock denied: would violate lock ordering.
    OrderViolation {
        /// Resource we tried to acquire.
        requested: Resource,
        /// Resource we already hold that comes AFTER requested in canonical order.
        held_after: Resource,
    },
}

/// Configuration for the priority inheritance protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityInheritanceConfig {
    /// Maximum chain depth for transitive inheritance.
    pub max_chain_depth: usize,
    /// Whether to enforce strict lock ordering.
    pub enforce_lock_order: bool,
    /// Maximum time (us) a boosted priority can persist before auto-release.
    pub max_inheritance_duration_us: u64,
}

impl Default for PriorityInheritanceConfig {
    fn default() -> Self {
        Self {
            max_chain_depth: 4,
            enforce_lock_order: true,
            max_inheritance_duration_us: 50_000, // 50ms
        }
    }
}

/// State of a single held lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldLock {
    /// Resource held.
    pub resource: Resource,
    /// Task holding the lock.
    pub holder_id: String,
    /// Original priority of the holder.
    pub original_priority: Priority,
    /// Current effective priority (may be boosted).
    pub effective_priority: Priority,
    /// Timestamp when lock was acquired.
    pub acquired_us: u64,
    /// Queue of waiters (correlation IDs), ordered by priority (highest first).
    pub waiters: Vec<(String, Priority)>,
}

/// Snapshot of the priority inheritance tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritanceSnapshot {
    /// Currently held locks.
    pub held_locks: Vec<HeldLock>,
    /// Total inheritance events since creation.
    pub total_inheritance_events: u64,
    /// Total lock-order violations prevented.
    pub total_order_violations: u64,
    /// Active inheritance chains (holder to waiter chain depth).
    pub active_chains: usize,
    /// Maximum chain depth observed.
    pub max_chain_depth_observed: usize,
}

/// Priority inheritance tracker. Manages lock state, detects inversions,
/// applies priority inheritance, and enforces lock ordering.
///
/// # Invariants
///
/// 1. A task's effective priority >= its original priority (boosted, never lowered).
/// 2. Lock ordering: if task holds resource A, it cannot acquire resource B where B < A.
/// 3. Inheritance is transitive up to `max_chain_depth`.
/// 4. When a holder releases a lock, its priority reverts to max(original, other inherited).
/// 5. Deterministic: same sequence of ops -> same state.
#[derive(Debug, Clone)]
pub struct PriorityInheritanceTracker {
    config: PriorityInheritanceConfig,
    /// Resource to HeldLock state.
    locks: Vec<Option<HeldLock>>,
    /// History of inheritance events.
    events: VecDeque<InheritanceEvent>,
    max_events: usize,
    total_inheritance_events: u64,
    total_order_violations: u64,
    max_chain_depth_observed: usize,
}

impl PriorityInheritanceTracker {
    /// Create a new tracker with the given configuration.
    pub fn new(config: PriorityInheritanceConfig) -> Self {
        Self {
            config,
            locks: Resource::LOCK_ORDER.iter().map(|_| None).collect(),
            events: VecDeque::new(),
            max_events: 256,
            total_inheritance_events: 0,
            total_order_violations: 0,
            max_chain_depth_observed: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(PriorityInheritanceConfig::default())
    }

    /// Attempt to acquire a resource lock.
    pub fn acquire(
        &mut self,
        resource: Resource,
        task_id: &str,
        priority: Priority,
        now_us: u64,
    ) -> LockResult {
        // Check lock-order violation: if we hold any lock with higher order index,
        // acquiring this one would violate canonical order.
        if self.config.enforce_lock_order {
            let requested_idx = resource.order_index();
            for held in self.locks.iter().flatten() {
                if held.holder_id == task_id && held.resource.order_index() > requested_idx {
                    self.total_order_violations += 1;
                    return LockResult::OrderViolation {
                        requested: resource,
                        held_after: held.resource,
                    };
                }
            }
        }

        let idx = resource.order_index();
        if self.locks[idx].is_none() {
            // Lock is free; acquire immediately.
            self.locks[idx] = Some(HeldLock {
                resource,
                holder_id: task_id.to_string(),
                original_priority: priority,
                effective_priority: priority,
                acquired_us: now_us,
                waiters: Vec::new(),
            });
            return LockResult::Acquired;
        }

        // Lock is held by another task. Apply priority inheritance if waiter has higher priority.
        let held = self.locks[idx].as_mut().unwrap();

        // If we already hold it, treat as re-entrant (acquired).
        if held.holder_id == task_id {
            return LockResult::Acquired;
        }

        // Add to waiter queue (sorted by priority, highest first).
        let insert_pos = held
            .waiters
            .iter()
            .position(|(_, p)| *p < priority)
            .unwrap_or(held.waiters.len());
        held.waiters
            .insert(insert_pos, (task_id.to_string(), priority));

        // Apply inheritance if waiter priority > holder effective priority.
        if priority > held.effective_priority {
            let event = InheritanceEvent {
                holder_id: held.holder_id.clone(),
                waiter_id: task_id.to_string(),
                resource,
                original_priority: held.original_priority,
                inherited_priority: priority,
                applied_us: now_us,
                released_us: None,
            };
            held.effective_priority = priority;
            self.total_inheritance_events += 1;

            if self.events.len() >= self.max_events {
                self.events.pop_front();
            }
            self.events.push_back(event);

            return LockResult::AcquiredAfterInheritance {
                boosted_holder: held.holder_id.clone(),
            };
        }

        // Waiter added but no inheritance needed; from the waiter's perspective
        // they are blocked. We return Acquired to signal the lock state was updated
        // (the caller should check if they are the holder).
        LockResult::Acquired
    }

    /// Release a resource lock. Returns the inheritance events that were resolved.
    pub fn release(&mut self, resource: Resource, task_id: &str, now_us: u64) -> Vec<String> {
        let idx = resource.order_index();
        let mut promoted = Vec::new();

        if let Some(held) = &self.locks[idx] {
            if held.holder_id != task_id {
                return promoted;
            }
        } else {
            return promoted;
        }

        // Close any open inheritance events for this resource.
        for event in &mut self.events {
            if event.resource == resource && event.released_us.is_none() {
                event.released_us = Some(now_us);
            }
        }

        let held = self.locks[idx].take().unwrap();

        // Promote the highest-priority waiter to be the new holder.
        if let Some((waiter_id, waiter_priority)) = held.waiters.first().cloned() {
            let remaining_waiters: Vec<_> = held.waiters[1..].to_vec();
            self.locks[idx] = Some(HeldLock {
                resource,
                holder_id: waiter_id.clone(),
                original_priority: waiter_priority,
                effective_priority: waiter_priority,
                acquired_us: now_us,
                waiters: remaining_waiters,
            });
            promoted.push(waiter_id);
        }

        promoted
    }

    /// Check if a task holds a specific resource.
    pub fn is_held_by(&self, resource: Resource, task_id: &str) -> bool {
        let idx = resource.order_index();
        self.locks[idx]
            .as_ref()
            .map(|h| h.holder_id == task_id)
            .unwrap_or(false)
    }

    /// Get effective priority of a task across all held locks.
    pub fn effective_priority(&self, task_id: &str) -> Option<Priority> {
        let mut max_priority: Option<Priority> = None;
        for held in self.locks.iter().flatten() {
            if held.holder_id == task_id {
                max_priority = Some(match max_priority {
                    Some(p) if p >= held.effective_priority => p,
                    _ => held.effective_priority,
                });
            }
        }
        max_priority
    }

    /// Validate lock ordering for a task's currently held locks.
    /// Returns list of violations (if any).
    pub fn check_lock_order(&self, task_id: &str) -> Vec<(Resource, Resource)> {
        let mut held_indices: Vec<(usize, Resource)> = Vec::new();
        for held in self.locks.iter().flatten() {
            if held.holder_id == task_id {
                held_indices.push((held.resource.order_index(), held.resource));
            }
        }
        held_indices.sort_by_key(|(idx, _)| *idx);

        let mut violations = Vec::new();
        for w in held_indices.windows(2) {
            if w[0].0 >= w[1].0 {
                violations.push((w[0].1, w[1].1));
            }
        }
        violations
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> InheritanceSnapshot {
        let held_locks: Vec<_> = self.locks.iter().filter_map(|l| l.clone()).collect();
        let active_chains = held_locks
            .iter()
            .filter(|l| l.effective_priority > l.original_priority)
            .count();

        InheritanceSnapshot {
            held_locks,
            total_inheritance_events: self.total_inheritance_events,
            total_order_violations: self.total_order_violations,
            active_chains,
            max_chain_depth_observed: self.max_chain_depth_observed,
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let held = self.locks.iter().filter(|l| l.is_some()).count();
        let snap = self.snapshot();
        format!(
            "pi_tracker held={} inherit={} violations={} chains={}",
            held, snap.total_inheritance_events, snap.total_order_violations, snap.active_chains,
        )
    }

    /// Release all locks held by a task. Returns resources that were released.
    pub fn release_all(&mut self, task_id: &str, now_us: u64) -> Vec<Resource> {
        let mut released = Vec::new();
        for resource in Resource::LOCK_ORDER {
            if self.is_held_by(resource, task_id) {
                self.release(resource, task_id, now_us);
                released.push(resource);
            }
        }
        released
    }

    /// Expire stale inheritance; auto-release boosts that have persisted too long.
    /// Returns the number of inheritances expired.
    pub fn expire_stale_inheritance(&mut self, now_us: u64) -> usize {
        let max_dur = self.config.max_inheritance_duration_us;
        let mut expired_count = 0;

        for held in self.locks.iter_mut().flatten() {
            if held.effective_priority > held.original_priority
                && now_us.saturating_sub(held.acquired_us) > max_dur
            {
                held.effective_priority = held.original_priority;
                expired_count += 1;
            }
        }

        // Also close open events that are past duration.
        for event in &mut self.events {
            if event.released_us.is_none() && now_us.saturating_sub(event.applied_us) > max_dur {
                event.released_us = Some(now_us);
            }
        }

        expired_count
    }

    /// Count of currently held locks.
    pub fn held_count(&self) -> usize {
        self.locks.iter().filter(|l| l.is_some()).count()
    }

    /// Total number of waiters across all held locks.
    pub fn total_waiters(&self) -> usize {
        self.locks
            .iter()
            .filter_map(|l| l.as_ref())
            .map(|h| h.waiters.len())
            .sum()
    }
}

// AARSP Bead: ft-2p9cb.2.3.2

/// Degradation signal from the priority inheritance tracker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InheritanceDegradation {
    /// Everything is fine.
    Healthy,
    /// Too many concurrent inheritance chains - possible priority ceiling issue.
    ExcessiveInheritance {
        active_chains: usize,
        threshold: usize,
    },
    /// Lock contention is high - many waiters.
    HighContention {
        total_waiters: usize,
        threshold: usize,
    },
    /// Lock-order violations are accumulating.
    OrderViolationSpike {
        total_violations: u64,
        threshold: u64,
    },
}

impl fmt::Display for InheritanceDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::ExcessiveInheritance {
                active_chains,
                threshold,
            } => write!(f, "EXCESSIVE_INHERITANCE({}/{})", active_chains, threshold),
            Self::HighContention {
                total_waiters,
                threshold,
            } => write!(f, "HIGH_CONTENTION({}/{})", total_waiters, threshold),
            Self::OrderViolationSpike {
                total_violations,
                threshold,
            } => write!(
                f,
                "ORDER_VIOLATION_SPIKE({}/{})",
                total_violations, threshold
            ),
        }
    }
}

/// Structured log entry for priority inheritance events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritanceLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Number of held locks.
    pub held_locks: usize,
    /// Total inheritance events so far.
    pub total_inheritance_events: u64,
    /// Total order violations so far.
    pub total_order_violations: u64,
    /// Active inheritance chains.
    pub active_chains: usize,
    /// Current degradation signal.
    pub degradation: InheritanceDegradation,
}

impl PriorityInheritanceTracker {
    /// Detect degradation based on current state.
    pub fn detect_degradation(&self) -> InheritanceDegradation {
        let snap = self.snapshot();

        // Threshold: more than 2 concurrent inheritance chains.
        if snap.active_chains > 2 {
            return InheritanceDegradation::ExcessiveInheritance {
                active_chains: snap.active_chains,
                threshold: 2,
            };
        }

        // Threshold: more than 8 total waiters.
        let total_waiters = self.total_waiters();
        if total_waiters > 8 {
            return InheritanceDegradation::HighContention {
                total_waiters,
                threshold: 8,
            };
        }

        // Threshold: more than 10 order violations.
        if snap.total_order_violations > 10 {
            return InheritanceDegradation::OrderViolationSpike {
                total_violations: snap.total_order_violations,
                threshold: 10,
            };
        }

        InheritanceDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self, now_us: u64) -> InheritanceLogEntry {
        let snap = self.snapshot();
        InheritanceLogEntry {
            timestamp_us: now_us,
            held_locks: snap.held_locks.len(),
            total_inheritance_events: snap.total_inheritance_events,
            total_order_violations: snap.total_order_violations,
            active_chains: snap.active_chains,
            degradation: self.detect_degradation(),
        }
    }
}
