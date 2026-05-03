//! Per-pane workflow lock manager.
//!
//! Ensures only one workflow runs per pane at a time. This is an internal
//! concurrency primitive that prevents workflow collisions, separate from
//! user-facing pane reservations.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

/// Result of attempting to acquire a pane workflow lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockAcquisitionResult {
    /// Lock acquired successfully.
    Acquired,
    /// Lock is already held by another workflow.
    AlreadyLocked {
        /// Name of the workflow holding the lock.
        held_by_workflow: String,
        /// Execution ID of the workflow holding the lock.
        held_by_execution: String,
        /// When the lock was acquired (unix timestamp ms).
        locked_since_ms: i64,
    },
}

impl LockAcquisitionResult {
    /// Check if the lock was acquired.
    #[must_use]
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired)
    }

    /// Check if the lock is already held.
    #[must_use]
    pub fn is_already_locked(&self) -> bool {
        matches!(self, Self::AlreadyLocked { .. })
    }
}

/// Information about a rejected global concurrency-limited lock attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyLimitInfo {
    /// Current number of active pane locks.
    pub active: usize,
    /// Configured maximum number of active pane locks.
    pub limit: usize,
}

/// Information about an active pane lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneLockInfo {
    /// Pane ID that is locked.
    pub pane_id: u64,
    /// Workflow name holding the lock.
    pub workflow_name: String,
    /// Execution ID holding the lock.
    pub execution_id: String,
    /// When the lock was acquired (unix timestamp ms).
    pub locked_at_ms: i64,
}

/// In-memory workflow lock manager for panes.
///
/// Ensures only one workflow runs per pane at a time. This is an internal
/// concurrency primitive that prevents workflow collisions, separate from
/// user-facing pane reservations.
///
/// # Design
///
/// - In-memory lock table keyed by `pane_id`
/// - Thread-safe via internal mutex
/// - Lock acquisition returns detailed info about existing locks
/// - Supports RAII-based release via `PaneWorkflowLockGuard`
///
/// # Example
///
/// ```no_run
/// use frankenterm_core::workflows::{PaneWorkflowLockManager, LockAcquisitionResult};
///
/// let manager = PaneWorkflowLockManager::new();
///
/// // Try to acquire lock
/// match manager.try_acquire(42, "handle_compaction", "exec-001") {
///     LockAcquisitionResult::Acquired => {
///         // Run workflow...
///         manager.release(42, "exec-001");
///     }
///     LockAcquisitionResult::AlreadyLocked { held_by_workflow, .. } => {
///         println!("Pane 42 is locked by {}", held_by_workflow);
///     }
/// }
/// ```
pub struct PaneWorkflowLockManager {
    /// Active locks keyed by pane_id.
    locks: Mutex<HashMap<u64, PaneLockInfo>>,
    /// Lifetime telemetry counters (atomic so reads from
    /// the doctor surface don't contend with the lock-table
    /// mutex).
    telemetry: LockManagerTelemetryInner,
}

/// Inner telemetry struct — atomics for lock-free read.
#[derive(Debug, Default)]
struct LockManagerTelemetryInner {
    acquisitions_total: std::sync::atomic::AtomicU64,
    releases_total: std::sync::atomic::AtomicU64,
    /// Mismatched-execution release attempts (release()
    /// called with wrong execution_id — could indicate
    /// stale caller or coordination bug).
    release_mismatched_total: std::sync::atomic::AtomicU64,
    /// Bypassed-id force-releases. The "use with caution"
    /// path. Operators watch this for abuse + runaway
    /// recovery rate.
    force_releases_total: std::sync::atomic::AtomicU64,
    /// try_acquire blocked because the global concurrency
    /// limit was hit.
    concurrency_limit_blocks_total: std::sync::atomic::AtomicU64,
    /// try_acquire blocked because the pane was already
    /// locked by another execution.
    pane_already_locked_total: std::sync::atomic::AtomicU64,
    /// Mutex was poisoned and we silently recovered via
    /// `unwrap_or_else(|e| e.into_inner())`. Non-zero is a
    /// strong signal that some prior holder panicked.
    mutex_poisoned_recoveries_total: std::sync::atomic::AtomicU64,
}

/// Doctor-friendly snapshot of the lock manager's
/// lifetime telemetry. Plain `u64` so the doctor can
/// serialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LockManagerHealth {
    pub acquisitions_total: u64,
    pub releases_total: u64,
    pub release_mismatched_total: u64,
    pub force_releases_total: u64,
    pub concurrency_limit_blocks_total: u64,
    pub pane_already_locked_total: u64,
    pub mutex_poisoned_recoveries_total: u64,
    /// Currently active lock count — read at snapshot time.
    pub active_locks: u32,
}

impl LockManagerHealth {
    /// True iff the lock manager is in a healthy state:
    /// no mutex poisoning observed AND force-release rate
    /// is non-pathological (≤5% of total releases).
    /// Occasional admin-driven force-release is expected;
    /// runaway force-release indicates panicking workflows
    /// or abuse.
    ///
    /// Per ft-ismxr fix: the previous releases_total == 0
    /// short-circuit returned true regardless of force-release
    /// count. That mis-classified the worst case (only
    /// force-releases, no normal releases) as healthy. Now:
    /// truly idle (both counters zero) returns true; any
    /// force-release with zero normal releases returns false.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        if self.mutex_poisoned_recoveries_total > 0 {
            return false;
        }
        if self.releases_total == 0 {
            // Truly idle is healthy. Force-releases without
            // any normal releases is the pathological 100%
            // case and must NOT report healthy.
            return self.force_releases_total == 0;
        }
        let force_ratio = self.force_releases_total as f64 / self.releases_total as f64;
        force_ratio <= 0.05
    }
}

impl Default for PaneWorkflowLockManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneWorkflowLockManager {
    /// Create a new lock manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
            telemetry: LockManagerTelemetryInner::default(),
        }
    }

    /// Project the lifetime telemetry counters into a
    /// doctor-friendly snapshot. Lock-free read except for
    /// `active_locks`.
    #[must_use]
    pub fn health(&self) -> LockManagerHealth {
        use std::sync::atomic::Ordering::Relaxed;
        let active_locks = match self.locks.lock() {
            Ok(g) => g.len() as u32,
            Err(e) => {
                self.telemetry
                    .mutex_poisoned_recoveries_total
                    .fetch_add(1, Relaxed);
                e.into_inner().len() as u32
            }
        };
        LockManagerHealth {
            acquisitions_total: self.telemetry.acquisitions_total.load(Relaxed),
            releases_total: self.telemetry.releases_total.load(Relaxed),
            release_mismatched_total: self.telemetry.release_mismatched_total.load(Relaxed),
            force_releases_total: self.telemetry.force_releases_total.load(Relaxed),
            concurrency_limit_blocks_total: self
                .telemetry
                .concurrency_limit_blocks_total
                .load(Relaxed),
            pane_already_locked_total: self.telemetry.pane_already_locked_total.load(Relaxed),
            mutex_poisoned_recoveries_total: self
                .telemetry
                .mutex_poisoned_recoveries_total
                .load(Relaxed),
            active_locks,
        }
    }

    /// Attempt to acquire a lock for a pane.
    ///
    /// Returns `Acquired` if the lock was obtained, or `AlreadyLocked` with
    /// information about the current lock holder.
    ///
    /// # Arguments
    ///
    /// * `pane_id` - The pane to lock
    /// * `workflow_name` - Name of the workflow requesting the lock
    /// * `execution_id` - Unique execution ID for this workflow run
    pub fn try_acquire(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
    ) -> LockAcquisitionResult {
        self.try_acquire_with_optional_limit(pane_id, workflow_name, execution_id, None)
            .expect("unbounded pane lock acquisition cannot hit concurrency limit")
    }

    /// Attempt to acquire a lock while atomically enforcing a global active-lock limit.
    ///
    /// The active-count check and insertion happen under the same mutex so the
    /// caller cannot oversubscribe the configured limit under contention.
    pub fn try_acquire_with_limit(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
        max_active: usize,
    ) -> Result<LockAcquisitionResult, ConcurrencyLimitInfo> {
        self.try_acquire_with_optional_limit(pane_id, workflow_name, execution_id, Some(max_active))
    }

    fn try_acquire_with_optional_limit(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
        max_active: Option<usize>,
    ) -> Result<LockAcquisitionResult, ConcurrencyLimitInfo> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut locks = self.locks.lock().unwrap_or_else(|e| {
            self.telemetry
                .mutex_poisoned_recoveries_total
                .fetch_add(1, Relaxed);
            e.into_inner()
        });

        if let Some(existing) = locks.get(&pane_id) {
            self.telemetry
                .pane_already_locked_total
                .fetch_add(1, Relaxed);
            return Ok(LockAcquisitionResult::AlreadyLocked {
                held_by_workflow: existing.workflow_name.clone(),
                held_by_execution: existing.execution_id.clone(),
                locked_since_ms: existing.locked_at_ms,
            });
        }

        if let Some(limit) = max_active.filter(|limit| *limit > 0) {
            let active = locks.len();
            if active >= limit {
                self.telemetry
                    .concurrency_limit_blocks_total
                    .fetch_add(1, Relaxed);
                return Err(ConcurrencyLimitInfo { active, limit });
            }
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));

        locks.insert(
            pane_id,
            PaneLockInfo {
                pane_id,
                workflow_name: workflow_name.to_string(),
                execution_id: execution_id.to_string(),
                locked_at_ms: now_ms,
            },
        );
        drop(locks);

        self.telemetry
            .acquisitions_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        tracing::debug!(
            pane_id,
            workflow_name,
            execution_id,
            "Acquired pane workflow lock"
        );

        Ok(LockAcquisitionResult::Acquired)
    }

    /// Release a lock for a pane.
    ///
    /// Only releases if the execution_id matches the current lock holder.
    /// This prevents accidental release by unrelated code.
    ///
    /// # Returns
    ///
    /// `true` if the lock was released, `false` if not found or mismatched.
    pub fn release(&self, pane_id: u64, execution_id: &str) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let mut locks = self.locks.lock().unwrap_or_else(|e| {
            self.telemetry
                .mutex_poisoned_recoveries_total
                .fetch_add(1, Relaxed);
            e.into_inner()
        });

        if let Some(existing) = locks.get(&pane_id) {
            if existing.execution_id == execution_id {
                locks.remove(&pane_id);
                drop(locks);
                self.telemetry.releases_total.fetch_add(1, Relaxed);
                tracing::debug!(pane_id, execution_id, "Released pane workflow lock");
                return true;
            }
            let held_by = existing.execution_id.clone();
            drop(locks);
            self.telemetry
                .release_mismatched_total
                .fetch_add(1, Relaxed);
            tracing::warn!(
                pane_id,
                execution_id,
                held_by = %held_by,
                "Attempted to release lock held by different execution"
            );
            return false;
        }

        false
    }

    /// Check if a pane is currently locked.
    ///
    /// Returns lock information if locked, `None` if free.
    #[must_use]
    pub fn is_locked(&self, pane_id: u64) -> Option<PaneLockInfo> {
        // br-ft-wd0fc: wire the read-side path through the existing
        // per-struct mutex_poisoned_recoveries_total counter (matches
        // the wired pattern at L266, L333, L615). Pre-fix this read
        // path silently recovered without observability — operators
        // saw 0 in the counter even when poison events had hit the
        // read paths.
        use std::sync::atomic::Ordering::Relaxed;
        let locks = self.locks.lock().unwrap_or_else(|e| {
            self.telemetry
                .mutex_poisoned_recoveries_total
                .fetch_add(1, Relaxed);
            e.into_inner()
        });
        locks.get(&pane_id).cloned()
    }

    /// Get all currently active locks.
    ///
    /// Number of panes currently locked by running workflows.
    #[must_use]
    pub fn active_count(&self) -> usize {
        // br-ft-wd0fc: wired through existing telemetry counter.
        use std::sync::atomic::Ordering::Relaxed;
        let locks = self.locks.lock().unwrap_or_else(|e| {
            self.telemetry
                .mutex_poisoned_recoveries_total
                .fetch_add(1, Relaxed);
            e.into_inner()
        });
        locks.len()
    }

    /// Useful for diagnostics and monitoring.
    #[must_use]
    pub fn active_locks(&self) -> Vec<PaneLockInfo> {
        // br-ft-wd0fc: wired through existing telemetry counter.
        use std::sync::atomic::Ordering::Relaxed;
        let locks = self.locks.lock().unwrap_or_else(|e| {
            self.telemetry
                .mutex_poisoned_recoveries_total
                .fetch_add(1, Relaxed);
            e.into_inner()
        });
        locks.values().cloned().collect()
    }

    /// Try to acquire a lock and return an RAII guard.
    ///
    /// The lock is automatically released when the guard is dropped.
    ///
    /// # Returns
    ///
    /// `Some(guard)` if acquired, `None` if already locked.
    pub fn acquire_guard(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
    ) -> Option<PaneWorkflowLockGuard<'_>> {
        match self.try_acquire(pane_id, workflow_name, execution_id) {
            LockAcquisitionResult::Acquired => Some(PaneWorkflowLockGuard {
                manager: self,
                pane_id,
                execution_id: execution_id.to_string(),
            }),
            LockAcquisitionResult::AlreadyLocked { .. } => None,
        }
    }

    /// Force-release a lock regardless of execution_id.
    ///
    /// Attempt to acquire a lock and return an RAII guard.
    ///
    /// Convenience wrapper over [`Self::try_acquire`] that bridges
    /// the existing `LockAcquisitionResult` enum into the
    /// [`PaneWorkflowLockGuard`] RAII type so callers don't need
    /// to remember a manual `release` call on every error path.
    ///
    /// **Bead:** ft-qkd2f. The runner.rs site has 14 manual
    /// release sites; future migrations should reach for this
    /// guarded form instead so the lock is released on every
    /// path (including panic-unwind) by construction.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(guard))` if the lock was acquired.
    /// - `Ok(None)` if the lock is already held — the
    ///   `LockAcquisitionResult::AlreadyLocked` shape isn't
    ///   carried through; callers that need the holder details
    ///   should keep using [`Self::try_acquire`] + manual
    ///   `release`.
    pub fn try_acquire_guarded(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
    ) -> Option<PaneWorkflowLockGuard<'_>> {
        match self.try_acquire(pane_id, workflow_name, execution_id) {
            LockAcquisitionResult::Acquired => Some(PaneWorkflowLockGuard {
                manager: self,
                pane_id,
                execution_id: execution_id.to_string(),
            }),
            LockAcquisitionResult::AlreadyLocked { .. } => None,
        }
    }

    /// Like [`Self::try_acquire_guarded`] but with a global
    /// active-lock limit. Returns:
    ///
    /// - `Ok(Some(guard))` on success.
    /// - `Ok(None)` if the pane is already locked.
    /// - `Err(ConcurrencyLimitInfo)` if the global limit is hit.
    pub fn try_acquire_with_limit_guarded(
        &self,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
        max_active: usize,
    ) -> Result<Option<PaneWorkflowLockGuard<'_>>, ConcurrencyLimitInfo> {
        match self.try_acquire_with_limit(pane_id, workflow_name, execution_id, max_active)? {
            LockAcquisitionResult::Acquired => Ok(Some(PaneWorkflowLockGuard {
                manager: self,
                pane_id,
                execution_id: execution_id.to_string(),
            })),
            LockAcquisitionResult::AlreadyLocked { .. } => Ok(None),
        }
    }

    /// Attempt to acquire a lock and return an owned RAII guard.
    ///
    /// Use this from callers that hold `Arc<PaneWorkflowLockManager>`
    /// and need the guard to travel across async boundaries
    /// (spawned tasks, awaited futures). The guard owns its own
    /// `Arc<Self>` clone so its lifetime is independent of the
    /// caller's `&Arc<Self>` borrow.
    ///
    /// **Bead:** ft-qkd2f. Used by `WorkflowRunner` (filed
    /// migration: ft-haa2b).
    pub fn try_acquire_owned_guarded(
        self: &std::sync::Arc<Self>,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
    ) -> Option<OwnedPaneWorkflowLockGuard> {
        match self.try_acquire(pane_id, workflow_name, execution_id) {
            LockAcquisitionResult::Acquired => Some(OwnedPaneWorkflowLockGuard {
                manager: Some(std::sync::Arc::clone(self)),
                pane_id,
                execution_id: execution_id.to_string(),
            }),
            LockAcquisitionResult::AlreadyLocked { .. } => None,
        }
    }

    /// Like [`Self::try_acquire_owned_guarded`] but with a global
    /// active-lock limit.
    pub fn try_acquire_with_limit_owned_guarded(
        self: &std::sync::Arc<Self>,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
        max_active: usize,
    ) -> Result<Option<OwnedPaneWorkflowLockGuard>, ConcurrencyLimitInfo> {
        match self.try_acquire_with_limit(pane_id, workflow_name, execution_id, max_active)? {
            LockAcquisitionResult::Acquired => Ok(Some(OwnedPaneWorkflowLockGuard {
                manager: Some(std::sync::Arc::clone(self)),
                pane_id,
                execution_id: execution_id.to_string(),
            })),
            LockAcquisitionResult::AlreadyLocked { .. } => Ok(None),
        }
    }

    /// Wrap an already-acquired lock into an owned RAII release
    /// guard.
    ///
    /// **Bead:** ft-haa2b. This is the workhorse for
    /// `WorkflowRunner::run_workflow_inner` — the lock is acquired
    /// up the stack (in `handle_detection_with_cx` or the resume
    /// loop), then `run_workflow_inner` constructs a release guard
    /// at function entry so every early-return path (and panic
    /// unwind) drops the lock by construction. Replaces a long
    /// chain of manual `release(pane_id, execution_id)` calls.
    ///
    /// # Precondition
    ///
    /// The caller must have previously acquired the lock for
    /// `(pane_id, execution_id)` via `try_acquire` /
    /// `try_acquire_with_limit` (or one of their guarded variants
    /// followed by handoff). This API does not verify the lock
    /// is currently held — it just constructs a guard whose
    /// `Drop` will call `release(pane_id, execution_id)`.
    ///
    /// # Idempotency note
    ///
    /// `release` is execution-id-aware: if the lock entry has
    /// already been removed (or replaced by a different
    /// `execution_id`), `Drop` is a no-op (with a warning logged
    /// at the lock layer). Migrating away from manual `release`
    /// callers should remove the manual call to avoid the
    /// warning + spurious `release_mismatched_total` bump.
    #[must_use = "the guard releases the lock when dropped — bind it to a name (typically `_release_guard`)"]
    pub fn held_lock_release_guard(
        self: &std::sync::Arc<Self>,
        pane_id: u64,
        execution_id: &str,
    ) -> OwnedPaneWorkflowLockGuard {
        OwnedPaneWorkflowLockGuard {
            manager: Some(std::sync::Arc::clone(self)),
            pane_id,
            execution_id: execution_id.to_string(),
        }
    }

    /// Like [`Self::try_acquire_with_limit_owned_guarded`] but
    /// preserves the `AlreadyLocked` details so callers that need
    /// them (e.g. surfacing `held_by_workflow` / `held_by_execution`
    /// in a user-facing response) don't have to fall back to the
    /// bare `try_acquire_with_limit`.
    ///
    /// **Bead:** ft-rlbvg. Used by
    /// `WorkflowRunner::handle_detection_with_cx` so the post-acquire
    /// engine-error path uses Drop-based release without losing
    /// the `WorkflowStartResult::PaneLocked` payload.
    ///
    /// # Returns
    ///
    /// - `Ok(OwnedLockAcquisitionResult::Acquired(guard))` on success.
    /// - `Ok(OwnedLockAcquisitionResult::AlreadyLocked { … })` if the
    ///   pane is already locked. The original `LockAcquisitionResult`
    ///   payload (held_by_workflow, held_by_execution, locked_since_ms)
    ///   is forwarded as-is.
    /// - `Err(ConcurrencyLimitInfo)` if the global active-lock
    ///   limit is hit.
    pub fn try_acquire_with_limit_owned_full(
        self: &std::sync::Arc<Self>,
        pane_id: u64,
        workflow_name: &str,
        execution_id: &str,
        max_active: usize,
    ) -> Result<OwnedLockAcquisitionResult, ConcurrencyLimitInfo> {
        match self.try_acquire_with_limit(pane_id, workflow_name, execution_id, max_active)? {
            LockAcquisitionResult::Acquired => Ok(OwnedLockAcquisitionResult::Acquired(
                OwnedPaneWorkflowLockGuard {
                    manager: Some(std::sync::Arc::clone(self)),
                    pane_id,
                    execution_id: execution_id.to_string(),
                },
            )),
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                locked_since_ms,
            } => Ok(OwnedLockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                locked_since_ms,
            }),
        }
    }

    /// **Use with caution** - only for recovery scenarios.
    ///
    /// Bumps `force_releases_total` regardless of whether
    /// the pane was actually locked. Operators watch this
    /// counter to spot abuse + runaway recovery.
    pub fn force_release(&self, pane_id: u64) -> Option<PaneLockInfo> {
        use std::sync::atomic::Ordering::Relaxed;
        self.telemetry.force_releases_total.fetch_add(1, Relaxed);
        let removed = self
            .locks
            .lock()
            .unwrap_or_else(|e| {
                self.telemetry
                    .mutex_poisoned_recoveries_total
                    .fetch_add(1, Relaxed);
                e.into_inner()
            })
            .remove(&pane_id);
        if let Some(ref info) = removed {
            self.telemetry.releases_total.fetch_add(1, Relaxed);
            tracing::warn!(
                pane_id,
                execution_id = %info.execution_id,
                "Force-released pane workflow lock"
            );
        }
        removed
    }
}

/// RAII guard for pane workflow lock.
///
/// The lock is automatically released when this guard is dropped.
pub struct PaneWorkflowLockGuard<'a> {
    manager: &'a PaneWorkflowLockManager,
    pane_id: u64,
    execution_id: String,
}

impl std::fmt::Debug for PaneWorkflowLockGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneWorkflowLockGuard")
            .field("pane_id", &self.pane_id)
            .field("execution_id", &self.execution_id)
            .finish()
    }
}

impl PaneWorkflowLockGuard<'_> {
    /// Get the pane ID this guard is locking.
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Get the execution ID that holds this lock.
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Explicitly release the lock, consuming the guard.
    pub fn release(self) {
        // Drop will handle the release
    }
}

impl Drop for PaneWorkflowLockGuard<'_> {
    fn drop(&mut self) {
        self.manager.release(self.pane_id, &self.execution_id);
    }
}

/// Owned RAII guard for pane workflow lock — holds the manager
/// via `Arc` rather than `&'a` borrow.
///
/// **Bead:** ft-qkd2f. The borrowed [`PaneWorkflowLockGuard`]
/// works for short-scope acquires within a single `&self`
/// stack frame, but `WorkflowRunner` stores `lock_manager:
/// Arc<PaneWorkflowLockManager>` and the lock typically crosses
/// async boundaries (spawned tasks, awaited futures). For those
/// callers, this owned variant lets the guard travel with the
/// task without lifetime entanglement.
///
/// `Drop` semantics match the borrowed guard: lock released on
/// every path, including panic-unwind.
pub struct OwnedPaneWorkflowLockGuard {
    /// `None` after `defuse()` consumes the guard without
    /// releasing — the lock entry stays in the manager's HashMap
    /// awaiting downstream release. `Some` in the normal case;
    /// `Drop` releases iff `Some`.
    manager: Option<std::sync::Arc<PaneWorkflowLockManager>>,
    pane_id: u64,
    execution_id: String,
}

impl std::fmt::Debug for OwnedPaneWorkflowLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedPaneWorkflowLockGuard")
            .field("pane_id", &self.pane_id)
            .field("execution_id", &self.execution_id)
            .field("defused", &self.manager.is_none())
            .finish()
    }
}

impl OwnedPaneWorkflowLockGuard {
    /// Get the pane ID this guard is locking.
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Get the execution ID that holds this lock.
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Explicitly release the lock, consuming the guard.
    pub fn release(self) {
        // Drop will handle the release.
    }

    /// Consume the guard **without** releasing the lock.
    ///
    /// **Bead:** ft-rlbvg. Used at the boundary between
    /// `WorkflowRunner::handle_detection_with_cx` (acquires the
    /// lock) and `run_workflow_inner` (takes its own RAII guard).
    /// On the success handoff path the upstream caller must
    /// surrender ownership without releasing — `defuse()` is the
    /// safe, leak-free way to do that. The `Arc` and execution_id
    /// are dropped normally; the lock entry remains in the
    /// manager's HashMap awaiting the downstream release.
    pub fn defuse(mut self) {
        // Take the Arc out so Drop's `if let Some` skip-fires.
        // The Arc itself drops normally here, decrementing the
        // refcount — no leak. The execution_id String drops
        // when `self` falls out of scope at the end of this fn.
        self.manager.take();
    }
}

impl Drop for OwnedPaneWorkflowLockGuard {
    fn drop(&mut self) {
        if let Some(manager) = self.manager.take() {
            manager.release(self.pane_id, &self.execution_id);
        }
    }
}

/// Owned-guard variant of [`LockAcquisitionResult`] returned by
/// [`PaneWorkflowLockManager::try_acquire_with_limit_owned_full`].
///
/// **Bead:** ft-rlbvg. Provides the `Acquired(guard)` ergonomic of
/// the guarded-API family while preserving the `AlreadyLocked`
/// payload that callers like
/// `WorkflowRunner::handle_detection_with_cx` need to surface
/// `held_by_workflow` / `held_by_execution` in their response.
#[derive(Debug)]
pub enum OwnedLockAcquisitionResult {
    /// Lock acquired; guard handles release on Drop.
    Acquired(OwnedPaneWorkflowLockGuard),
    /// Pane is already locked by a different execution.
    AlreadyLocked {
        held_by_workflow: String,
        held_by_execution: String,
        locked_since_ms: i64,
    },
}

impl OwnedLockAcquisitionResult {
    /// Convenience predicate: did the lock acquire?
    #[must_use]
    pub const fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired(_))
    }

    /// Convenience predicate: was the pane already locked?
    #[must_use]
    pub const fn is_already_locked(&self) -> bool {
        matches!(self, Self::AlreadyLocked { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // LockAcquisitionResult
    // ========================================================================

    #[test]
    fn lock_acquisition_result_predicates() {
        let acquired = LockAcquisitionResult::Acquired;
        assert!(acquired.is_acquired());
        assert!(!acquired.is_already_locked());

        let locked = LockAcquisitionResult::AlreadyLocked {
            held_by_workflow: "wf".into(),
            held_by_execution: "e1".into(),
            locked_since_ms: 1000,
        };
        assert!(!locked.is_acquired());
        assert!(locked.is_already_locked());
    }

    // ========================================================================
    // PaneWorkflowLockManager basic operations
    // ========================================================================

    #[test]
    fn try_acquire_and_release() {
        let mgr = PaneWorkflowLockManager::new();
        let result = mgr.try_acquire(1, "wf_a", "exec-1");
        assert!(result.is_acquired());

        // Second acquire on same pane should fail
        let result2 = mgr.try_acquire(1, "wf_b", "exec-2");
        assert!(result2.is_already_locked());
        if let LockAcquisitionResult::AlreadyLocked {
            held_by_workflow,
            held_by_execution,
            ..
        } = result2
        {
            assert_eq!(held_by_workflow, "wf_a");
            assert_eq!(held_by_execution, "exec-1");
        }

        // Release with correct execution_id
        assert!(mgr.release(1, "exec-1"));

        // Now should be able to acquire again
        let result3 = mgr.try_acquire(1, "wf_b", "exec-2");
        assert!(result3.is_acquired());

        mgr.release(1, "exec-2");
    }

    #[test]
    fn try_acquire_with_limit_blocks_new_pane_when_limit_reached() {
        let mgr = PaneWorkflowLockManager::new();
        let first = mgr
            .try_acquire_with_limit(1, "wf_a", "exec-1", 1)
            .expect("first lock should succeed");
        assert!(first.is_acquired());

        let err = mgr
            .try_acquire_with_limit(2, "wf_b", "exec-2", 1)
            .expect_err("second pane should be blocked by global limit");
        assert_eq!(err.active, 1);
        assert_eq!(err.limit, 1);
    }

    #[test]
    fn try_acquire_with_limit_prefers_existing_pane_conflict_over_limit() {
        let mgr = PaneWorkflowLockManager::new();
        let first = mgr
            .try_acquire_with_limit(1, "wf_a", "exec-1", 1)
            .expect("first lock should succeed");
        assert!(first.is_acquired());

        let result = mgr
            .try_acquire_with_limit(1, "wf_b", "exec-2", 1)
            .expect("same pane should report lock conflict, not global limit");
        assert!(result.is_already_locked());
    }

    #[test]
    fn release_wrong_execution_id() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "exec-1");
        // Release with wrong execution_id should fail
        assert!(!mgr.release(1, "exec-wrong"));
        // Lock should still be held
        assert!(mgr.is_locked(1).is_some());
        mgr.release(1, "exec-1");
    }

    #[test]
    fn release_nonexistent_lock() {
        let mgr = PaneWorkflowLockManager::new();
        assert!(!mgr.release(999, "exec-1"));
    }

    #[test]
    fn different_panes_independent() {
        let mgr = PaneWorkflowLockManager::new();
        assert!(mgr.try_acquire(1, "wf_a", "e1").is_acquired());
        assert!(mgr.try_acquire(2, "wf_b", "e2").is_acquired());
        assert!(mgr.try_acquire(3, "wf_c", "e3").is_acquired());

        assert!(mgr.is_locked(1).is_some());
        assert!(mgr.is_locked(2).is_some());
        assert!(mgr.is_locked(3).is_some());
        assert!(mgr.is_locked(4).is_none());

        mgr.release(1, "e1");
        mgr.release(2, "e2");
        mgr.release(3, "e3");
    }

    // ========================================================================
    // is_locked
    // ========================================================================

    #[test]
    fn is_locked_returns_info() {
        let mgr = PaneWorkflowLockManager::new();
        assert!(mgr.is_locked(1).is_none());

        mgr.try_acquire(1, "my_workflow", "exec-42");
        let info = mgr.is_locked(1).unwrap();
        assert_eq!(info.pane_id, 1);
        assert_eq!(info.workflow_name, "my_workflow");
        assert_eq!(info.execution_id, "exec-42");
        assert!(info.locked_at_ms > 0);

        mgr.release(1, "exec-42");
        assert!(mgr.is_locked(1).is_none());
    }

    // ========================================================================
    // active_locks
    // ========================================================================

    #[test]
    fn active_locks_empty_initially() {
        let mgr = PaneWorkflowLockManager::new();
        assert!(mgr.active_locks().is_empty());
    }

    #[test]
    fn active_locks_returns_all() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(10, "wf_a", "e1");
        mgr.try_acquire(20, "wf_b", "e2");
        mgr.try_acquire(30, "wf_c", "e3");

        let locks = mgr.active_locks();
        assert_eq!(locks.len(), 3);
        let pane_ids: Vec<u64> = locks.iter().map(|l| l.pane_id).collect();
        assert!(pane_ids.contains(&10));
        assert!(pane_ids.contains(&20));
        assert!(pane_ids.contains(&30));

        mgr.release(10, "e1");
        let locks = mgr.active_locks();
        assert_eq!(locks.len(), 2);

        mgr.release(20, "e2");
        mgr.release(30, "e3");
    }

    // ========================================================================
    // acquire_guard (RAII)
    // ========================================================================

    #[test]
    fn acquire_guard_locks_and_drops() {
        let mgr = PaneWorkflowLockManager::new();

        {
            let guard = mgr.acquire_guard(1, "wf", "e1");
            assert!(guard.is_some());
            let guard = guard.unwrap();
            assert_eq!(guard.pane_id(), 1);
            assert_eq!(guard.execution_id(), "e1");

            // Pane should be locked while guard exists
            assert!(mgr.is_locked(1).is_some());

            // Second acquire should fail
            assert!(mgr.acquire_guard(1, "wf2", "e2").is_none());
        }
        // Guard dropped — pane should be unlocked now
        assert!(mgr.is_locked(1).is_none());
    }

    #[test]
    fn acquire_guard_explicit_release() {
        let mgr = PaneWorkflowLockManager::new();
        let guard = mgr.acquire_guard(5, "wf", "e1").unwrap();
        assert!(mgr.is_locked(5).is_some());
        guard.release(); // explicit release
        assert!(mgr.is_locked(5).is_none());
    }

    #[test]
    fn acquire_guard_returns_none_when_locked() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf_a", "e1");
        assert!(mgr.acquire_guard(1, "wf_b", "e2").is_none());
        mgr.release(1, "e1");
    }

    // ========================================================================
    // force_release
    // ========================================================================

    #[test]
    fn force_release_removes_lock() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "e1");

        let info = mgr.force_release(1);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.execution_id, "e1");

        assert!(mgr.is_locked(1).is_none());
    }

    #[test]
    fn force_release_nonexistent() {
        let mgr = PaneWorkflowLockManager::new();
        assert!(mgr.force_release(999).is_none());
    }

    // ========================================================================
    // LockManagerHealth telemetry
    // ========================================================================

    #[test]
    fn health_baseline_is_safe_with_zero_counters() {
        let mgr = PaneWorkflowLockManager::new();
        let h = mgr.health();
        assert_eq!(h.acquisitions_total, 0);
        assert_eq!(h.releases_total, 0);
        assert_eq!(h.force_releases_total, 0);
        assert_eq!(h.active_locks, 0);
        assert!(h.is_safe());
    }

    #[test]
    fn health_records_acquisition_and_release_cycle() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "e1");
        mgr.try_acquire(2, "wf", "e2");
        mgr.release(1, "e1");
        let h = mgr.health();
        assert_eq!(h.acquisitions_total, 2);
        assert_eq!(h.releases_total, 1);
        assert_eq!(h.active_locks, 1);
        assert!(h.is_safe());
    }

    #[test]
    fn health_records_pane_already_locked_blocks() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf_a", "e1");
        mgr.try_acquire(1, "wf_b", "e2"); // blocked
        mgr.try_acquire(1, "wf_c", "e3"); // blocked
        let h = mgr.health();
        assert_eq!(h.pane_already_locked_total, 2);
    }

    #[test]
    fn health_records_concurrency_limit_blocks() {
        let mgr = PaneWorkflowLockManager::new();
        let _ = mgr.try_acquire_with_limit(1, "wf", "e1", 1);
        let _ = mgr.try_acquire_with_limit(2, "wf", "e2", 1); // limit hit
        let _ = mgr.try_acquire_with_limit(3, "wf", "e3", 1); // limit hit
        let h = mgr.health();
        assert_eq!(h.concurrency_limit_blocks_total, 2);
    }

    #[test]
    fn health_records_release_mismatched() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "e1");
        mgr.release(1, "wrong-id"); // mismatched
        let h = mgr.health();
        assert_eq!(h.release_mismatched_total, 1);
        assert_eq!(h.releases_total, 0);
    }

    #[test]
    fn health_force_release_increments_force_counter() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "e1");
        mgr.force_release(1);
        let h = mgr.health();
        assert_eq!(h.force_releases_total, 1);
        // force_release that actually removed a lock also
        // counts as a release for the doctor's release-rate
        // accounting.
        assert_eq!(h.releases_total, 1);
    }

    #[test]
    fn health_force_release_on_empty_pane_still_increments_force_counter() {
        // Operators can spot abuse where force_release is
        // called repeatedly on already-empty pane ids.
        let mgr = PaneWorkflowLockManager::new();
        mgr.force_release(99);
        mgr.force_release(99);
        mgr.force_release(99);
        let h = mgr.health();
        assert_eq!(h.force_releases_total, 3);
        assert_eq!(h.releases_total, 0);
    }

    #[test]
    fn health_unsafe_when_force_release_rate_pathological() {
        // 1 normal release + 2 force-releases = 67% force
        // ratio → unsafe.
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "e1");
        mgr.release(1, "e1");
        mgr.try_acquire(2, "wf", "e2");
        mgr.force_release(2);
        mgr.try_acquire(3, "wf", "e3");
        mgr.force_release(3);
        let h = mgr.health();
        assert!(!h.is_safe());
    }

    #[test]
    fn double_release_with_same_id_returns_false_on_second() {
        // Coverage gap: idempotency-style behavior pinned.
        // First release succeeds; second returns false
        // because the lock is already gone.
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf", "e1");
        assert!(mgr.release(1, "e1"));
        assert!(!mgr.release(1, "e1"));
        let h = mgr.health();
        // Only 1 release counted (second call hit the
        // "lock not found" branch, doesn't bump
        // releases_total).
        assert_eq!(h.releases_total, 1);
    }

    #[test]
    fn empty_execution_id_round_trips_at_acquire_release() {
        // Coverage gap: lock manager is opaque to the
        // shape of execution_id. Pin the round-trip so a
        // future maintainer doesn't add a non-empty
        // validator without a bead.
        let mgr = PaneWorkflowLockManager::new();
        let result = mgr.try_acquire(1, "wf", "");
        assert!(result.is_acquired());
        assert!(mgr.release(1, ""));
        // String comparison is strict — empty != "x".
        mgr.try_acquire(2, "wf", "");
        assert!(!mgr.release(2, "x"));
    }

    #[test]
    fn pane_id_zero_is_accepted() {
        // Coverage gap: pane id 0 is just a u64. No
        // sentinel meaning at this layer.
        let mgr = PaneWorkflowLockManager::new();
        let result = mgr.try_acquire(0, "wf", "e1");
        assert!(result.is_acquired());
        assert!(mgr.is_locked(0).is_some());
        assert!(mgr.release(0, "e1"));
    }

    #[test]
    fn health_safe_with_low_force_release_rate() {
        // 19 normal releases + 1 force-release = 5% ratio
        // → safe boundary.
        let mgr = PaneWorkflowLockManager::new();
        for i in 0..19 {
            mgr.try_acquire(i, "wf", &format!("e{i}"));
            mgr.release(i, &format!("e{i}"));
        }
        mgr.try_acquire(99, "wf", "e99");
        mgr.force_release(99);
        let h = mgr.health();
        assert!(h.is_safe());
    }

    #[test]
    fn force_release_allows_reacquire() {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(1, "wf_a", "e1");

        // Can't acquire normally
        assert!(mgr.try_acquire(1, "wf_b", "e2").is_already_locked());

        // Force release
        mgr.force_release(1);

        // Now can acquire
        assert!(mgr.try_acquire(1, "wf_b", "e2").is_acquired());
        mgr.release(1, "e2");
    }

    #[test]
    fn ft_xbnl0_4_4_workflow_lock_table_returns_to_baseline_after_storm_cycles() {
        let mgr = PaneWorkflowLockManager::new();

        for cycle in 0_u64..12 {
            for pane_id in 0_u64..4 {
                let result = mgr
                    .try_acquire_with_limit(
                        pane_id,
                        "workflow-storm",
                        &format!("storm-{cycle}-{pane_id}"),
                        4,
                    )
                    .expect("storm cycle should stay within global lock limit");
                assert!(
                    result.is_acquired(),
                    "pane {pane_id} should acquire during storm cycle {cycle}"
                );
            }

            assert_eq!(
                mgr.active_count(),
                4,
                "lock table grew unexpectedly during storm cycle {cycle}"
            );

            for pane_id in 0_u64..2 {
                assert!(
                    mgr.release(pane_id, &format!("storm-{cycle}-{pane_id}")),
                    "pane {pane_id} should release cleanly during storm cycle {cycle}"
                );
            }

            for pane_id in 2_u64..4 {
                let released = mgr
                    .force_release(pane_id)
                    .expect("force release should recover leaked storm locks");
                assert_eq!(released.pane_id, pane_id);
                assert_eq!(released.execution_id, format!("storm-{cycle}-{pane_id}"));
            }

            assert_eq!(
                mgr.active_count(),
                0,
                "lock table failed to return to baseline after storm cycle {cycle}"
            );
            assert!(
                mgr.active_locks().is_empty(),
                "active lock table retained entries after storm cycle {cycle}"
            );
        }
    }

    // ========================================================================
    // PaneLockInfo serde
    // ========================================================================

    #[test]
    fn pane_lock_info_serde_roundtrip() {
        let info = PaneLockInfo {
            pane_id: 42,
            workflow_name: "test_workflow".to_string(),
            execution_id: "exec-123".to_string(),
            locked_at_ms: 1709328000000,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneLockInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 42);
        assert_eq!(parsed.workflow_name, "test_workflow");
        assert_eq!(parsed.execution_id, "exec-123");
        assert_eq!(parsed.locked_at_ms, 1709328000000);
    }

    // ========================================================================
    // Default trait
    // ========================================================================

    #[test]
    fn default_creates_empty_manager() {
        let mgr = PaneWorkflowLockManager::default();
        assert!(mgr.active_locks().is_empty());
    }

    // ========================================================================
    // Stress: many panes
    // ========================================================================

    #[test]
    fn acquire_release_many_panes() {
        let mgr = PaneWorkflowLockManager::new();
        for i in 0..100 {
            assert!(mgr.try_acquire(i, "wf", &format!("e{i}")).is_acquired());
        }
        assert_eq!(mgr.active_locks().len(), 100);

        for i in 0..100 {
            assert!(mgr.release(i, &format!("e{i}")));
        }
        assert!(mgr.active_locks().is_empty());
    }

    // ========================================================================
    // ft-qkd2f: try_acquire_guarded / try_acquire_with_limit_guarded
    // ========================================================================

    #[test]
    fn try_acquire_guarded_returns_some_on_acquire() {
        let mgr = PaneWorkflowLockManager::new();
        let guard = mgr.try_acquire_guarded(1, "wf", "exec-1");
        assert!(guard.is_some());
        assert_eq!(mgr.active_locks().len(), 1);
        drop(guard);
        assert_eq!(mgr.active_locks().len(), 0);
    }

    #[test]
    fn try_acquire_guarded_returns_none_when_already_locked() {
        let mgr = PaneWorkflowLockManager::new();
        let _g1 = mgr.try_acquire_guarded(1, "wf", "exec-1");
        let g2 = mgr.try_acquire_guarded(1, "wf", "exec-2");
        assert!(g2.is_none());
    }

    #[test]
    fn try_acquire_with_limit_guarded_returns_err_on_limit() {
        let mgr = PaneWorkflowLockManager::new();
        let g1 = match mgr.try_acquire_with_limit_guarded(1, "wf", "exec-1", 1) {
            Ok(Some(g)) => g,
            other => panic!("acquire under limit: {:?}", other.is_ok()),
        };
        let _ = g1; // hold the lock
        // Limit of 1 active lock; second pane should hit the limit.
        let result = mgr.try_acquire_with_limit_guarded(2, "wf", "exec-2", 1);
        match result {
            Err(info) => {
                assert_eq!(info.active, 1);
                assert_eq!(info.limit, 1);
            }
            Ok(_) => panic!("expected ConcurrencyLimitInfo error"),
        }
    }

    #[test]
    fn try_acquire_with_limit_guarded_returns_ok_none_when_pane_locked() {
        let mgr = PaneWorkflowLockManager::new();
        let g1 = match mgr.try_acquire_with_limit_guarded(1, "wf", "exec-1", 10) {
            Ok(Some(g)) => g,
            _ => panic!("first acquire should succeed"),
        };
        let _ = g1;
        // Same pane id but different execution → already locked.
        match mgr.try_acquire_with_limit_guarded(1, "wf", "exec-2", 10) {
            Ok(None) => {}
            other => panic!("expected Ok(None), got is_ok={}", other.is_ok()),
        }
    }

    #[test]
    fn guard_releases_on_panic_unwind() {
        // The bug ft-qkd2f calls out: a panic during execute_steps
        // bypasses manual release calls. The RAII guard's Drop runs
        // during unwind, so the lock must be released.
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let mgr_clone = std::sync::Arc::clone(&mgr);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mgr_clone
                .try_acquire_guarded(42, "wf", "exec-panic")
                .expect("acquire ok");
            assert_eq!(mgr_clone.active_locks().len(), 1);
            panic!("simulated execute_steps panic");
        }));
        assert!(result.is_err(), "panic should propagate");
        assert_eq!(
            mgr.active_locks().len(),
            0,
            "lock must be released on panic-unwind via Drop — this is the load-bearing invariant ft-qkd2f cites"
        );
    }

    #[test]
    fn explicit_release_via_consuming_method_releases_exactly_once() {
        let mgr = PaneWorkflowLockManager::new();
        let guard = mgr.try_acquire_guarded(7, "wf", "exec-1").expect("acquire");
        assert_eq!(mgr.active_locks().len(), 1);
        guard.release();
        assert_eq!(mgr.active_locks().len(), 0);
    }

    // ========================================================================
    // ft-qkd2f: try_acquire_owned_guarded — Arc-flavored guard for
    // callers that need the guard to travel across async boundaries.
    // ========================================================================

    #[test]
    fn try_acquire_owned_guarded_returns_some_on_acquire() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let guard = mgr.try_acquire_owned_guarded(1, "wf", "exec-1");
        assert!(guard.is_some());
        assert_eq!(mgr.active_locks().len(), 1);
        drop(guard);
        assert_eq!(mgr.active_locks().len(), 0);
    }

    #[test]
    fn try_acquire_owned_guarded_returns_none_when_already_locked() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let _g1 = mgr.try_acquire_owned_guarded(1, "wf", "exec-1");
        let g2 = mgr.try_acquire_owned_guarded(1, "wf", "exec-2");
        assert!(g2.is_none());
    }

    #[test]
    fn try_acquire_with_limit_owned_guarded_returns_err_on_limit() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let g1 = match mgr.try_acquire_with_limit_owned_guarded(1, "wf", "exec-1", 1) {
            Ok(Some(g)) => g,
            other => panic!("acquire under limit: ok={}", other.is_ok()),
        };
        let _ = g1;
        match mgr.try_acquire_with_limit_owned_guarded(2, "wf", "exec-2", 1) {
            Err(info) => {
                assert_eq!(info.active, 1);
                assert_eq!(info.limit, 1);
            }
            Ok(_) => panic!("expected ConcurrencyLimitInfo error"),
        }
    }

    #[test]
    fn owned_guard_releases_on_panic_unwind() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let mgr_clone = std::sync::Arc::clone(&mgr);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mgr_clone
                .try_acquire_owned_guarded(42, "wf", "exec-panic")
                .expect("acquire ok");
            assert_eq!(mgr_clone.active_locks().len(), 1);
            panic!("simulated execute_steps panic");
        }));
        assert!(result.is_err());
        assert_eq!(
            mgr.active_locks().len(),
            0,
            "owned guard must release on panic-unwind via Drop"
        );
    }

    #[test]
    fn owned_guard_can_outlive_arc_borrow_scope() {
        // The defining property of the owned variant: the guard
        // doesn't borrow the Arc, so the original Arc handle can
        // drop and the manager stays alive via the guard's clone.
        let guard = {
            let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
            mgr.try_acquire_owned_guarded(99, "wf", "exec-1")
                .expect("acquire")
        };
        assert_eq!(guard.pane_id(), 99);
        assert_eq!(guard.execution_id(), "exec-1");
        drop(guard);
    }

    #[test]
    fn owned_guard_explicit_release() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let guard = mgr
            .try_acquire_owned_guarded(5, "wf", "exec-1")
            .expect("acquire");
        assert_eq!(mgr.active_locks().len(), 1);
        guard.release();
        assert_eq!(mgr.active_locks().len(), 0);
    }

    // ========================================================================
    // ft-rlbvg: defuse() — handoff between phases of a workflow lifecycle.
    // ========================================================================

    #[test]
    fn defuse_consumes_guard_without_releasing_lock() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let guard = mgr
            .try_acquire_owned_guarded(21, "wf", "exec-defuse")
            .expect("acquire");
        assert_eq!(mgr.active_locks().len(), 1);
        guard.defuse();
        // Lock entry stays in the manager's HashMap.
        assert_eq!(
            mgr.active_locks().len(),
            1,
            "defuse must NOT release the lock"
        );
        // Caller can release later via the bare API.
        assert!(mgr.release(21, "exec-defuse"));
        assert_eq!(mgr.active_locks().len(), 0);
    }

    #[test]
    fn defuse_does_not_leak_arc_refcount() {
        // Defuse drops the Arc cleanly via Option::take — no leak.
        // A leak would manifest as the manager outliving every
        // outstanding strong handle.
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        assert_eq!(std::sync::Arc::strong_count(&mgr), 1);
        {
            let guard = mgr
                .try_acquire_owned_guarded(22, "wf", "exec-1")
                .expect("acquire");
            // Guard holds one Arc clone.
            assert_eq!(std::sync::Arc::strong_count(&mgr), 2);
            guard.defuse();
            // Arc clone returned to the runtime's allocator.
            assert_eq!(std::sync::Arc::strong_count(&mgr), 1);
        }
        // Cleanup: release the still-held lock.
        let _ = mgr.release(22, "exec-1");
    }

    // ========================================================================
    // ft-rlbvg: try_acquire_with_limit_owned_full preserves AlreadyLocked
    // payload while delivering an owned guard on success.
    // ========================================================================

    #[test]
    fn try_acquire_with_limit_owned_full_acquired_returns_guard() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let result = mgr
            .try_acquire_with_limit_owned_full(31, "wf", "exec-1", 10)
            .expect("limit not hit");
        assert!(result.is_acquired());
        assert!(!result.is_already_locked());
        match result {
            OwnedLockAcquisitionResult::Acquired(g) => {
                assert_eq!(g.pane_id(), 31);
                assert_eq!(g.execution_id(), "exec-1");
            }
            _ => panic!("expected Acquired"),
        }
    }

    #[test]
    fn try_acquire_with_limit_owned_full_already_locked_preserves_payload() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        // First acquire to populate the entry.
        let g1 = mgr
            .try_acquire_with_limit_owned_full(32, "wf-first", "exec-first", 10)
            .expect("limit not hit");
        let _g1 = match g1 {
            OwnedLockAcquisitionResult::Acquired(g) => g,
            _ => panic!("first acquire should succeed"),
        };

        // Second acquire on same pane → AlreadyLocked with payload.
        let result = mgr
            .try_acquire_with_limit_owned_full(32, "wf-second", "exec-second", 10)
            .expect("limit not hit");
        match result {
            OwnedLockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                locked_since_ms,
            } => {
                assert_eq!(held_by_workflow, "wf-first");
                assert_eq!(held_by_execution, "exec-first");
                assert!(locked_since_ms > 0, "locked_since_ms must be populated");
            }
            _ => panic!("expected AlreadyLocked with payload"),
        }
    }

    #[test]
    fn try_acquire_with_limit_owned_full_concurrency_limit() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        let _g1 = mgr
            .try_acquire_with_limit_owned_full(33, "wf", "exec-1", 1)
            .expect("first under limit");
        let err = mgr
            .try_acquire_with_limit_owned_full(34, "wf", "exec-2", 1)
            .expect_err("second should hit limit");
        assert_eq!(err.active, 1);
        assert_eq!(err.limit, 1);
    }

    // ========================================================================
    // ft-haa2b: held_lock_release_guard — wraps an upstream-acquired lock
    // into a Drop-on-exit RAII guard so callers like
    // `WorkflowRunner::run_workflow_inner` don't need a manual `release`
    // on every early-return branch.
    // ========================================================================

    #[test]
    fn held_lock_release_guard_drops_release_held_lock() {
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        // Caller acquires upstream.
        assert!(mgr.try_acquire(11, "wf", "exec-h1").is_acquired());
        assert_eq!(mgr.active_locks().len(), 1);
        {
            let _release_guard = mgr.held_lock_release_guard(11, "exec-h1");
            assert_eq!(mgr.active_locks().len(), 1, "guard does not re-acquire");
        }
        assert_eq!(
            mgr.active_locks().len(),
            0,
            "guard's Drop must release the held lock"
        );
    }

    #[test]
    fn held_lock_release_guard_releases_on_panic_unwind() {
        // The exact invariant ft-haa2b enforces: a panic inside
        // run_workflow_inner's step loop must release the pane
        // workflow lock by Drop, with no manual release needed on
        // any branch.
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        assert!(mgr.try_acquire(77, "wf", "exec-panic-h").is_acquired());
        let mgr_clone = std::sync::Arc::clone(&mgr);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _release_guard = mgr_clone.held_lock_release_guard(77, "exec-panic-h");
            assert_eq!(mgr_clone.active_locks().len(), 1);
            panic!("simulated step-loop panic");
        }));
        assert!(result.is_err());
        assert_eq!(
            mgr.active_locks().len(),
            0,
            "held-lock release guard must drop-release on panic unwind"
        );
    }

    #[test]
    fn held_lock_release_guard_idempotent_with_prior_manual_release() {
        // Migration safety: if a caller still has a stray manual
        // `release` somewhere on the path before the guard drops,
        // the guard's Drop is a no-op (lock already gone). This is
        // the property that makes the per-site removal reversible
        // — partial migrations cannot double-release.
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        assert!(mgr.try_acquire(12, "wf", "exec-mig").is_acquired());
        let release_guard = mgr.held_lock_release_guard(12, "exec-mig");
        // Stray manual release happens upstream of the guard drop.
        assert!(mgr.release(12, "exec-mig"));
        assert_eq!(mgr.active_locks().len(), 0);
        // Guard drops here — must NOT panic and must leave count at 0.
        drop(release_guard);
        assert_eq!(mgr.active_locks().len(), 0);
    }

    #[test]
    fn held_lock_release_guard_does_not_release_replacement_lock() {
        // If the held lock was force-released and re-acquired by a
        // different execution_id between the held_lock_release_guard
        // construction and its Drop, the Drop must not release the
        // new owner's lock. The execution_id check in `release()`
        // already protects this — pin it as a regression test.
        let mgr = std::sync::Arc::new(PaneWorkflowLockManager::new());
        assert!(mgr.try_acquire(13, "wf", "exec-old").is_acquired());
        let release_guard = mgr.held_lock_release_guard(13, "exec-old");
        // Force-release + new owner.
        let _ = mgr.force_release(13);
        assert!(mgr.try_acquire(13, "wf", "exec-new").is_acquired());
        // Old guard drops — must NOT release the new owner's lock.
        drop(release_guard);
        assert_eq!(
            mgr.active_locks().len(),
            1,
            "stale guard must not release replacement lock"
        );
        assert_eq!(mgr.is_locked(13).unwrap().execution_id, "exec-new");
    }

    // ========================================================================
    // ft-r0y4h: force_release race + limit-zero + concurrent stress
    // ========================================================================

    /// Pins the protection where an outstanding guard's Drop runs
    /// after a `force_release(pane)` + `acquire(pane, "new-exec")`.
    /// The Drop must NOT release the new lock because the
    /// execution_id check in `release()` rejects the mismatch.
    /// Without this guard, force_release would create a window
    /// where stale guard drops corrupt the new owner's state.
    #[test]
    fn force_release_does_not_let_stale_guard_drop_release_new_lock() {
        let mgr = PaneWorkflowLockManager::new();
        // Acquire with old execution_id, take a guard.
        let _ = mgr.try_acquire(1, "wf", "old-exec");
        let stale_guard = PaneWorkflowLockGuard {
            manager: &mgr,
            pane_id: 1,
            execution_id: "old-exec".to_string(),
        };
        // Force-release.
        mgr.force_release(1);
        assert!(mgr.is_locked(1).is_none());

        // New owner acquires.
        let result = mgr.try_acquire(1, "wf", "new-exec");
        assert!(result.is_acquired());
        assert_eq!(mgr.is_locked(1).unwrap().execution_id, "new-exec");

        // Stale guard drops — release() sees execution_id mismatch
        // ("old-exec" vs "new-exec"), warns, does NOT release.
        drop(stale_guard);

        // New owner's lock must still be intact.
        assert!(
            mgr.is_locked(1).is_some(),
            "stale guard drop must not release the new owner's lock"
        );
        assert_eq!(mgr.is_locked(1).unwrap().execution_id, "new-exec");
    }

    /// `try_acquire_with_limit(_, _, _, max_active=0)` treats 0 as
    /// "no limit" per the `filter(|limit| *limit > 0)` clause. Pin
    /// this so a future tightening that interprets 0 as
    /// reject-everything doesn't silently break callers passing 0
    /// as the disabled-limit signal.
    #[test]
    fn try_acquire_with_limit_zero_is_unbounded() {
        let mgr = PaneWorkflowLockManager::new();
        // Acquire many panes with limit=0; all should succeed.
        for i in 0..50u64 {
            let result = mgr.try_acquire_with_limit(
                i,
                "wf",
                &format!("exec-{i}"),
                0, // disabled-limit signal
            );
            match result {
                Ok(LockAcquisitionResult::Acquired) => {}
                other => panic!("limit=0 must permit acquire, got {other:?}"),
            }
        }
        assert_eq!(
            mgr.active_locks().len(),
            50,
            "limit=0 should permit unbounded growth"
        );
    }

    #[test]
    fn try_acquire_with_limit_zero_still_rejects_already_locked() {
        // limit=0 disables the active-count gate, but the
        // already-locked check still fires (single-owner-per-pane
        // invariant is independent of the limit).
        let mgr = PaneWorkflowLockManager::new();
        let _ = mgr.try_acquire_with_limit(1, "wf", "exec-1", 0);
        let second = mgr.try_acquire_with_limit(1, "wf", "exec-2", 0);
        match second {
            Ok(LockAcquisitionResult::AlreadyLocked { .. }) => {}
            other => panic!("limit=0 must not bypass already-locked check, got {other:?}"),
        }
    }

    /// ft-ismxr regression guard: previously `is_safe()` returned
    /// `true` when `releases_total == 0` regardless of
    /// `force_releases_total`. The early return mis-classified
    /// the worst-case "only force-releases" scenario as healthy.
    #[test]
    fn lock_manager_health_is_safe_rejects_force_releases_without_normal_releases() {
        let h = LockManagerHealth {
            releases_total: 0,
            force_releases_total: 10, // pure force-release storm
            ..LockManagerHealth::default()
        };
        assert!(
            !h.is_safe(),
            "force_releases > 0 with zero normal releases is the pathological case and must NOT report healthy"
        );
    }

    #[test]
    fn lock_manager_health_is_safe_accepts_truly_idle() {
        // Both counters zero = truly idle, no operations yet =
        // healthy. Pin the boundary so the fix doesn't over-correct.
        let h = LockManagerHealth::default();
        assert!(h.is_safe(), "default (all zeros) must report healthy");
    }

    #[test]
    fn lock_manager_health_is_safe_accepts_low_force_ratio() {
        let h = LockManagerHealth {
            releases_total: 100,
            force_releases_total: 3, // 3% — under the 5% threshold
            ..LockManagerHealth::default()
        };
        assert!(h.is_safe());
    }

    #[test]
    fn lock_manager_health_is_safe_rejects_high_force_ratio() {
        let h = LockManagerHealth {
            releases_total: 100,
            force_releases_total: 50, // 50% — well over threshold
            ..LockManagerHealth::default()
        };
        assert!(!h.is_safe());
    }

    #[test]
    fn lock_manager_health_is_safe_rejects_mutex_poisoning() {
        let h = LockManagerHealth {
            mutex_poisoned_recoveries_total: 1,
            ..LockManagerHealth::default()
        };
        assert!(!h.is_safe());
    }

    /// Concurrent stress: 4 threads contending for 16 distinct
    /// panes, each thread doing N acquires/releases. The mutex-
    /// protected lock table must never lose a lock or surface a
    /// torn state. Final state must have zero active locks.
    #[test]
    fn concurrent_acquire_release_stress_returns_to_baseline() {
        use std::sync::Arc;
        use std::thread;

        let mgr = Arc::new(PaneWorkflowLockManager::new());
        let mut handles = Vec::new();
        const THREADS: usize = 4;
        const PANES_PER_THREAD: u64 = 4;
        const ITERS: u64 = 100;

        for thread_id in 0..THREADS as u64 {
            let mgr = Arc::clone(&mgr);
            handles.push(thread::spawn(move || {
                for iter in 0..ITERS {
                    let pane_id = thread_id * PANES_PER_THREAD + (iter % PANES_PER_THREAD);
                    let exec_id = format!("t{thread_id}-i{iter}");
                    if mgr.try_acquire(pane_id, "wf", &exec_id).is_acquired() {
                        // Tiny work to widen the contention window.
                        std::hint::spin_loop();
                        mgr.release(pane_id, &exec_id);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            mgr.active_locks().len(),
            0,
            "after concurrent stress, no locks should be held"
        );
    }
}
