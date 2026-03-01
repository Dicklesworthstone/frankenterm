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
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(existing) = locks.get(&pane_id) {
            return LockAcquisitionResult::AlreadyLocked {
                held_by_workflow: existing.workflow_name.clone(),
                held_by_execution: existing.execution_id.clone(),
                locked_since_ms: existing.locked_at_ms,
            };
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

        LockAcquisitionResult::Acquired
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
}
