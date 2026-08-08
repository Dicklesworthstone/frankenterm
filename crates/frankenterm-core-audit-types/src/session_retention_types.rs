//! Session retention policy DTOs (ft-xcsm0 / ft-8nqx0 Phase 4).
//!
//! Lifted out of `frankenterm-core::session_retention` so the cleanup
//! summary can be reviewed independently from the SQLite deletion
//! pipeline (`cleanup_sessions`, `delete_sessions_by_age`,
//! `cleanup_orphaned_data`, …) which still lives in core. The result
//! shape is pure data — no `rusqlite::Connection`, no
//! `crate::config`, just counters.

/// Result of a session cleanup operation.
///
/// This receipt covers logical row reclamation only: session retention issues
/// no `VACUUM` or `incremental_vacuum` operation. Under FrankenTerm's normal
/// `auto_vacuum=NONE` policy, SQLite keeps freed pages on its freelist for reuse.
/// An externally-created `auto_vacuum=FULL` database may still compact pages at
/// transaction commit and is outside that physical-behavior guarantee.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupResult {
    /// Sessions deleted by age policy.
    pub deleted_by_age: usize,
    /// Sessions deleted by count limit.
    pub deleted_by_count: usize,
    /// Sessions deleted by size budget.
    pub deleted_by_size: usize,
    /// Exact logical retained-session bytes measured before size cleanup.
    pub size_measured_bytes: u64,
    /// Exact logical retained-session bytes removed by size cleanup.
    pub size_deleted_bytes: u64,
    /// Exact logical retained-session bytes remaining after size cleanup.
    pub size_retained_bytes: u64,
    /// Bytes still above budget because no additional session was eligible.
    pub size_ineligible_shortfall_bytes: u64,
    /// Orphaned restore-attempt lifecycle rows cleaned.
    pub orphaned_restore_lifecycle_rows: usize,
    /// Orphaned checkpoint rows cleaned.
    pub orphaned_checkpoints: usize,
    /// Orphaned pane_state rows cleaned.
    pub orphaned_pane_states: usize,
}

impl CleanupResult {
    /// Total number of sessions deleted.
    #[must_use]
    pub fn total_sessions_deleted(&self) -> usize {
        self.deleted_by_age
            .saturating_add(self.deleted_by_count)
            .saturating_add(self.deleted_by_size)
    }

    /// Whether any cleanup was performed.
    #[must_use]
    pub fn any_work_done(&self) -> bool {
        self.total_sessions_deleted() > 0
            || self.orphaned_restore_lifecycle_rows > 0
            || self.orphaned_checkpoints > 0
            || self.orphaned_pane_states > 0
    }
}

#[cfg(test)]
mod tests {
    use super::CleanupResult;

    #[test]
    fn total_sessions_deleted_saturates_instead_of_wrapping() {
        let result = CleanupResult {
            deleted_by_age: usize::MAX,
            deleted_by_count: 1,
            deleted_by_size: 1,
            ..CleanupResult::default()
        };

        assert_eq!(result.total_sessions_deleted(), usize::MAX);
        assert!(result.any_work_done());
    }

    #[test]
    fn orphaned_restore_lifecycle_rows_are_reported_as_work() {
        let result = CleanupResult {
            orphaned_restore_lifecycle_rows: 1,
            ..CleanupResult::default()
        };

        assert_eq!(result.total_sessions_deleted(), 0);
        assert!(result.any_work_done());
    }
}
