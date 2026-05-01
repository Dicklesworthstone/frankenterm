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
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(existing) = locks.get(&pane_id) {
            return Ok(LockAcquisitionResult::AlreadyLocked {
                held_by_workflow: existing.workflow_name.clone(),
                held_by_execution: existing.execution_id.clone(),
                locked_since_ms: existing.locked_at_ms,
            });
        }

        if let Some(limit) = max_active.filter(|limit| *limit > 0) {
            let active = locks.len();
            if active >= limit {
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
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(existing) = locks.get(&pane_id) {
            if existing.execution_id == execution_id {
                locks.remove(&pane_id);
                drop(locks);
                tracing::debug!(pane_id, execution_id, "Released pane workflow lock");
                return true;
            }
            let held_by = existing.execution_id.clone();
            drop(locks);
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
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.get(&pane_id).cloned()
    }

    /// Get all currently active locks.
    ///
    /// Number of panes currently locked by running workflows.
    #[must_use]
    pub fn active_count(&self) -> usize {
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.len()
    }

    /// Useful for diagnostics and monitoring.
    #[must_use]
    pub fn active_locks(&self) -> Vec<PaneLockInfo> {
        let locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
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
                manager: std::sync::Arc::clone(self),
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
                manager: std::sync::Arc::clone(self),
                pane_id,
                execution_id: execution_id.to_string(),
            })),
            LockAcquisitionResult::AlreadyLocked { .. } => Ok(None),
        }
    }

    /// **Use with caution** - only for recovery scenarios.
    pub fn force_release(&self, pane_id: u64) -> Option<PaneLockInfo> {
        let removed = self
            .locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pane_id);
        if let Some(ref info) = removed {
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
    manager: std::sync::Arc<PaneWorkflowLockManager>,
    pane_id: u64,
    execution_id: String,
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
}

impl Drop for OwnedPaneWorkflowLockGuard {
    fn drop(&mut self) {
        self.manager.release(self.pane_id, &self.execution_id);
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
