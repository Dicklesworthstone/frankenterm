//! Session retention policy DTOs (ft-xcsm0 / ft-8nqx0 Phase 4).
//!
//! Lifted out of `frankenterm-core::session_retention` so the cleanup
//! summary can be reviewed independently from the SQLite deletion
//! pipeline (`cleanup_sessions`, `delete_sessions_by_age`,
//! `cleanup_orphaned_data`, …) which still lives in core. The result
//! shape is pure data — no `rusqlite::Connection`, no
//! `crate::config`, just counters.

/// Result of a session cleanup operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupResult {
    /// Sessions deleted by age policy.
    pub deleted_by_age: usize,
    /// Sessions deleted by count limit.
    pub deleted_by_count: usize,
    /// Sessions deleted by size budget.
    pub deleted_by_size: usize,
    /// Orphaned checkpoint rows cleaned.
    pub orphaned_checkpoints: usize,
    /// Orphaned pane_state rows cleaned.
    pub orphaned_pane_states: usize,
    /// Whether VACUUM was run.
    pub vacuumed: bool,
}

impl CleanupResult {
    /// Total number of sessions deleted.
    #[must_use]
    pub fn total_sessions_deleted(&self) -> usize {
        self.deleted_by_age + self.deleted_by_count + self.deleted_by_size
    }

    /// Whether any cleanup was performed.
    #[must_use]
    pub fn any_work_done(&self) -> bool {
        self.total_sessions_deleted() > 0
            || self.orphaned_checkpoints > 0
            || self.orphaned_pane_states > 0
    }
}
