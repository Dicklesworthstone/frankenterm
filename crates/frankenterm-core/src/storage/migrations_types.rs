//! Schema-migration row-shape types extracted from
//! `storage/migrations.rs`
//! (br-ft-94ito / ft-dn2tu Phase 2.2).
//!
//! Pure data types + their associated `as_str` impls. The
//! migration RUNNER functions (those that touch a SQLite
//! `Connection`) stay in `storage/migrations.rs`. The
//! `MIGRATIONS` static, the rollback-classifier helpers, and
//! the forensic-bundle types also stay in `migrations.rs` — this
//! split lifts only the row-shape contract that downstream
//! callers serialize / pattern-match against.
//!
//! `storage/migrations.rs` re-exports each public item via
//! `pub use super::migrations_types::*;` so the
//! `frankenterm_core::storage::migrations::*` facade is
//! preserved byte-for-byte for external callers.
//!
//! Cross-references:
//! - parent: ft-dn2tu storage.rs split
//! - prior: ft-6qkx1 (Phase 2 schema_ddl extraction at 412d277f2)
//! - sibling: ft-bbhwz (Phase 2.3 — types.rs for Sections 3+4)

use serde::{Deserialize, Serialize};

/// A schema migration.
#[derive(Debug, Clone)]
pub struct Migration {
    /// Target version after this migration is applied.
    pub version: i32,
    /// Human-readable description.
    pub description: &'static str,
    /// SQL to execute for the upgrade.
    pub up_sql: &'static str,
    /// SQL to execute for rollback (`None` means rollback unsupported).
    pub down_sql: Option<&'static str>,
}

/// Direction for a migration plan or execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationDirection {
    /// Upgrade schema to a newer version.
    Up,
    /// Roll back schema to an older version.
    Down,
}

impl MigrationDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// A single migration step in a plan or report.
#[derive(Debug, Clone)]
pub struct MigrationStep {
    /// Migration version being applied or rolled back.
    pub migration_version: i32,
    /// Resulting schema version after this step.
    pub resulting_version: i32,
    /// Human-readable description.
    pub description: &'static str,
    /// Direction of this step.
    pub direction: MigrationDirection,
}

/// Migration plan or execution report.
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    /// Starting version.
    pub from_version: i32,
    /// Target version.
    pub to_version: i32,
    /// Direction (up/down).
    pub direction: MigrationDirection,
    /// Steps to apply.
    pub steps: Vec<MigrationStep>,
}

/// Status entry for a migration.
#[derive(Debug, Clone)]
pub struct MigrationStatusEntry {
    /// Schema version after the migration is applied.
    pub version: i32,
    /// Human-readable description.
    pub description: &'static str,
    /// Whether this migration is applied.
    pub applied: bool,
    /// Whether rollback is available.
    pub rollback_supported: bool,
}

/// Status report for schema migrations.
#[derive(Debug, Clone)]
pub struct MigrationStatusReport {
    /// Whether the database file exists.
    pub db_exists: bool,
    /// Whether schema initialization is required.
    pub needs_initialization: bool,
    /// Current schema version (`PRAGMA user_version`).
    pub current_version: i32,
    /// Target schema version (`SCHEMA_VERSION`).
    pub target_version: i32,
    /// All migration entries with applied/pending status.
    pub entries: Vec<MigrationStatusEntry>,
}

/// Migration phase for rollback trigger classification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStage {
    /// M0: preflight and writer freeze.
    #[default]
    Preflight,
    /// M1: canonical export from source backend.
    Export,
    /// M2: canonical import into target backend.
    Import,
    /// M3: checkpoint synchronization.
    CheckpointSync,
    /// M4: projection reconciliation / rebuild.
    ProjectionRebuild,
    /// M5: target backend activation.
    Activate,
    /// Post-cutover soak / canary window.
    Soak,
}

impl MigrationStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Export => "export",
            Self::Import => "import",
            Self::CheckpointSync => "checkpoint_sync",
            Self::ProjectionRebuild => "projection_rebuild",
            Self::Activate => "activate",
            Self::Soak => "soak",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// br-ft-94ito: stable as_str slugs for both enums (any
    /// downstream caller serializing these to a log line or
    /// telemetry counter must keep seeing the same strings).
    #[test]
    fn migration_direction_as_str_stable() {
        assert_eq!(MigrationDirection::Up.as_str(), "up");
        assert_eq!(MigrationDirection::Down.as_str(), "down");
    }

    #[test]
    fn migration_stage_as_str_stable() {
        assert_eq!(MigrationStage::Preflight.as_str(), "preflight");
        assert_eq!(MigrationStage::Export.as_str(), "export");
        assert_eq!(MigrationStage::Import.as_str(), "import");
        assert_eq!(MigrationStage::CheckpointSync.as_str(), "checkpoint_sync");
        assert_eq!(
            MigrationStage::ProjectionRebuild.as_str(),
            "projection_rebuild"
        );
        assert_eq!(MigrationStage::Activate.as_str(), "activate");
        assert_eq!(MigrationStage::Soak.as_str(), "soak");
    }

    /// br-ft-94ito: Default for MigrationStage is Preflight (M0).
    #[test]
    fn migration_stage_default_is_preflight() {
        assert_eq!(MigrationStage::default(), MigrationStage::Preflight);
    }

    /// br-ft-94ito: serde round-trip for the snake_case slugs.
    /// Pinned at the type-extraction boundary so the JSON shape
    /// downstream callers depend on cannot drift.
    #[test]
    fn migration_stage_serde_roundtrip() {
        for stage in [
            MigrationStage::Preflight,
            MigrationStage::Export,
            MigrationStage::Import,
            MigrationStage::CheckpointSync,
            MigrationStage::ProjectionRebuild,
            MigrationStage::Activate,
            MigrationStage::Soak,
        ] {
            let json = serde_json::to_string(&stage).expect("serialize");
            let back: MigrationStage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(stage, back);
        }
    }
}
