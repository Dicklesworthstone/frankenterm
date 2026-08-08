//! SQLite schema migration types and runner.
//!
//! Extracted from `storage.rs` for ft-2dorr while preserving the
//! `frankenterm_core::storage::*` facade through re-exports in the parent
//! module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{
    Connection, DropBehavior, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};

use super::{
    SCHEMA_SQL, SCHEMA_VERSION, StoragePipelineSnapshot, ensure_parent_dir, now_epoch_ms, now_ms,
};
use crate::error::{Result, StorageError};
use crate::recorder_invariants::{InvariantReport, ViolationSeverity};
use crate::recorder_storage::{RecorderBackendKind, RecorderOffset};
use crate::storage_telemetry::{SloStatus, StorageHealthTier};

// =============================================================================
// Schema Migrations  →  TYPES moved to `storage/migrations_types.rs`
// =============================================================================
//
// [br-ft-94ito / ft-dn2tu Phase 2.2] The row-shape migration types
// (`Migration`, `MigrationDirection`, `MigrationStep`,
// `MigrationPlan`, `MigrationStatusEntry`, `MigrationStatusReport`,
// `MigrationStage`) and their `as_str` impls now live in
// `storage/migrations_types.rs`. The runner functions, the
// `MIGRATIONS` static, the rollback-classifier helpers, and the
// forensic-bundle types remain in this file. The
// `frankenterm_core::storage::migrations::*` facade is preserved
// byte-for-byte through the re-exports below.
pub use super::migrations_types::{
    Migration, MigrationDirection, MigrationPlan, MigrationStage, MigrationStatusEntry,
    MigrationStatusReport, MigrationStep,
};

/// Rollback class selected by the classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationRollbackClass {
    /// In-cutover invariant break; abort immediately and restore source backend.
    Immediate,
    /// Post-cutover degradation; controlled backend reversion and projection rebuild.
    PostCutover,
    /// Canonical data integrity emergency; freeze writes and restore known-good source.
    DataIntegrityEmergency,
}

impl MigrationRollbackClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::PostCutover => "post_cutover",
            Self::DataIntegrityEmergency => "data_integrity_emergency",
        }
    }
}

/// Trigger signal emitted by migration rollback classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationRollbackTrigger {
    ImportDigestMismatch,
    EventCardinalityMismatch,
    CheckpointRegression,
    CorruptImport,
    InvariantErrors,
    InvariantCritical,
    SustainedSloBreach,
    SloAppendP95Breached,
    SloFlushP95Breached,
    HealthTierBlack,
    ProjectionLagBreach,
    RepeatedWriteFailures,
    RepeatedIndexFailures,
    PolicyAuditRegression,
    CanonicalDataLossConfirmed,
    CanonicalCorruptionSuspected,
}

impl MigrationRollbackTrigger {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ImportDigestMismatch => "import_digest_mismatch",
            Self::EventCardinalityMismatch => "event_cardinality_mismatch",
            Self::CheckpointRegression => "checkpoint_regression",
            Self::CorruptImport => "corrupt_import",
            Self::InvariantErrors => "invariant_errors",
            Self::InvariantCritical => "invariant_critical",
            Self::SustainedSloBreach => "sustained_slo_breach",
            Self::SloAppendP95Breached => "slo_append_p95_breached",
            Self::SloFlushP95Breached => "slo_flush_p95_breached",
            Self::HealthTierBlack => "health_tier_black",
            Self::ProjectionLagBreach => "projection_lag_breach",
            Self::RepeatedWriteFailures => "repeated_write_failures",
            Self::RepeatedIndexFailures => "repeated_index_failures",
            Self::PolicyAuditRegression => "policy_audit_regression",
            Self::CanonicalDataLossConfirmed => "canonical_data_loss_confirmed",
            Self::CanonicalCorruptionSuspected => "canonical_corruption_suspected",
        }
    }
}

/// Reduced invariant summary consumed by rollback classifier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationInvariantSummary {
    pub warning_count: usize,
    pub error_count: usize,
    pub critical_count: usize,
}

impl MigrationInvariantSummary {
    #[must_use]
    pub fn from_report(report: &InvariantReport) -> Self {
        Self {
            warning_count: report.count_by_severity(ViolationSeverity::Warning),
            error_count: report.count_by_severity(ViolationSeverity::Error),
            critical_count: report.count_by_severity(ViolationSeverity::Critical),
        }
    }

    #[must_use]
    pub const fn has_breakage(self) -> bool {
        self.error_count > 0 || self.critical_count > 0
    }
}

/// Reduced storage/SLO summary consumed by rollback classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStorageSloSummary {
    pub health_tier: StorageHealthTier,
    pub slo_append_p95: SloStatus,
    pub slo_flush_p95: SloStatus,
}

impl MigrationStorageSloSummary {
    #[must_use]
    pub fn from_snapshot(snapshot: &StoragePipelineSnapshot) -> Self {
        Self {
            health_tier: snapshot.health_tier,
            slo_append_p95: snapshot.slo_append_p95,
            slo_flush_p95: snapshot.slo_flush_p95,
        }
    }
}

/// Thresholds for migration rollback classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MigrationRollbackClassifierConfig {
    /// Number of consecutive breach windows required before SLO-only rollback.
    pub sustained_slo_windows: u32,
    /// Threshold for repeated high-severity write failures.
    pub repeated_write_failure_threshold: u32,
    /// Threshold for repeated high-severity index failures.
    pub repeated_index_failure_threshold: u32,
}

impl Default for MigrationRollbackClassifierConfig {
    fn default() -> Self {
        Self {
            sustained_slo_windows: 3,
            repeated_write_failure_threshold: 3,
            repeated_index_failure_threshold: 3,
        }
    }
}

/// Input signal bundle for rollback trigger classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MigrationRollbackClassifierInput {
    pub stage: MigrationStage,
    pub invariants: Option<MigrationInvariantSummary>,
    pub storage_slo: Option<MigrationStorageSloSummary>,
    pub import_digest_mismatch: bool,
    pub event_cardinality_mismatch: bool,
    pub checkpoint_regression: bool,
    pub corrupt_import: bool,
    pub projection_lag_breach: bool,
    pub policy_audit_regression: bool,
    pub confirmed_canonical_data_loss: bool,
    pub suspected_canonical_corruption: bool,
    pub high_severity_write_failures: u32,
    pub high_severity_index_failures: u32,
    pub consecutive_slo_breach_windows: u32,
    pub config: MigrationRollbackClassifierConfig,
}

impl Default for MigrationRollbackClassifierInput {
    fn default() -> Self {
        Self {
            stage: MigrationStage::Preflight,
            invariants: None,
            storage_slo: None,
            import_digest_mismatch: false,
            event_cardinality_mismatch: false,
            checkpoint_regression: false,
            corrupt_import: false,
            projection_lag_breach: false,
            policy_audit_regression: false,
            confirmed_canonical_data_loss: false,
            suspected_canonical_corruption: false,
            high_severity_write_failures: 0,
            high_severity_index_failures: 0,
            consecutive_slo_breach_windows: 0,
            config: MigrationRollbackClassifierConfig::default(),
        }
    }
}

/// Decision produced by rollback trigger classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRollbackDecision {
    pub should_rollback: bool,
    pub rollback_class: Option<MigrationRollbackClass>,
    pub triggers: Vec<MigrationRollbackTrigger>,
    pub stage: MigrationStage,
    pub rationale: String,
}

impl MigrationRollbackDecision {
    #[must_use]
    fn no_rollback(stage: MigrationStage, rationale: String) -> Self {
        Self {
            should_rollback: false,
            rollback_class: None,
            triggers: Vec::new(),
            stage,
            rationale,
        }
    }
}

/// Classify rollback triggers from migration invariants and SLO signals.
///
/// Mapping follows the frankensqlite rollout contract:
/// - immediate rollback: invariant breaks during cutover
/// - post-cutover rollback: sustained SLO breaches or repeated failures
/// - data-integrity emergency rollback: canonical data loss/corruption signals
#[must_use]
pub fn classify_migration_rollback_trigger(
    input: &MigrationRollbackClassifierInput,
) -> MigrationRollbackDecision {
    let mut emergency_triggers = Vec::new();
    let mut immediate_triggers = Vec::new();
    let mut post_cutover_triggers = Vec::new();

    let push_unique = |dst: &mut Vec<MigrationRollbackTrigger>, t: MigrationRollbackTrigger| {
        if !dst.contains(&t) {
            dst.push(t);
        }
    };

    if input.confirmed_canonical_data_loss {
        push_unique(
            &mut emergency_triggers,
            MigrationRollbackTrigger::CanonicalDataLossConfirmed,
        );
    }
    if input.suspected_canonical_corruption {
        push_unique(
            &mut emergency_triggers,
            MigrationRollbackTrigger::CanonicalCorruptionSuspected,
        );
    }

    if input.import_digest_mismatch {
        push_unique(
            &mut immediate_triggers,
            MigrationRollbackTrigger::ImportDigestMismatch,
        );
    }
    if input.event_cardinality_mismatch {
        push_unique(
            &mut immediate_triggers,
            MigrationRollbackTrigger::EventCardinalityMismatch,
        );
    }
    if input.checkpoint_regression {
        push_unique(
            &mut immediate_triggers,
            MigrationRollbackTrigger::CheckpointRegression,
        );
    }
    if input.corrupt_import {
        push_unique(
            &mut immediate_triggers,
            MigrationRollbackTrigger::CorruptImport,
        );
    }
    if let Some(invariants) = input.invariants {
        if invariants.critical_count > 0 {
            push_unique(
                &mut immediate_triggers,
                MigrationRollbackTrigger::InvariantCritical,
            );
        }
        if invariants.error_count > 0 {
            push_unique(
                &mut immediate_triggers,
                MigrationRollbackTrigger::InvariantErrors,
            );
        }
    }

    let mut slo_detail_triggers = Vec::new();
    if let Some(storage_slo) = input.storage_slo {
        if matches!(storage_slo.health_tier, StorageHealthTier::Black) {
            push_unique(
                &mut slo_detail_triggers,
                MigrationRollbackTrigger::HealthTierBlack,
            );
        }
        if matches!(storage_slo.slo_append_p95, SloStatus::Breached) {
            push_unique(
                &mut slo_detail_triggers,
                MigrationRollbackTrigger::SloAppendP95Breached,
            );
        }
        if matches!(storage_slo.slo_flush_p95, SloStatus::Breached) {
            push_unique(
                &mut slo_detail_triggers,
                MigrationRollbackTrigger::SloFlushP95Breached,
            );
        }
    }
    if input.projection_lag_breach {
        push_unique(
            &mut slo_detail_triggers,
            MigrationRollbackTrigger::ProjectionLagBreach,
        );
    }

    if !slo_detail_triggers.is_empty()
        && input.consecutive_slo_breach_windows >= input.config.sustained_slo_windows
    {
        push_unique(
            &mut post_cutover_triggers,
            MigrationRollbackTrigger::SustainedSloBreach,
        );
        for trigger in slo_detail_triggers {
            push_unique(&mut post_cutover_triggers, trigger);
        }
    }

    if input.high_severity_write_failures >= input.config.repeated_write_failure_threshold {
        push_unique(
            &mut post_cutover_triggers,
            MigrationRollbackTrigger::RepeatedWriteFailures,
        );
    }
    if input.high_severity_index_failures >= input.config.repeated_index_failure_threshold {
        push_unique(
            &mut post_cutover_triggers,
            MigrationRollbackTrigger::RepeatedIndexFailures,
        );
    }
    if input.policy_audit_regression {
        push_unique(
            &mut post_cutover_triggers,
            MigrationRollbackTrigger::PolicyAuditRegression,
        );
    }

    let (rollback_class, mut triggers, rationale) = if !emergency_triggers.is_empty() {
        (
            Some(MigrationRollbackClass::DataIntegrityEmergency),
            emergency_triggers.clone(),
            "canonical data integrity emergency signal detected".to_string(),
        )
    } else if !immediate_triggers.is_empty() {
        (
            Some(MigrationRollbackClass::Immediate),
            immediate_triggers.clone(),
            "cutover invariant break detected; immediate rollback required".to_string(),
        )
    } else if !post_cutover_triggers.is_empty() {
        (
            Some(MigrationRollbackClass::PostCutover),
            post_cutover_triggers.clone(),
            "post-cutover degradation detected; controlled rollback required".to_string(),
        )
    } else {
        let decision = MigrationRollbackDecision::no_rollback(
            input.stage,
            "no rollback trigger conditions satisfied".to_string(),
        );
        tracing::info!(
            stage = input.stage.as_str(),
            consecutive_slo_breach_windows = input.consecutive_slo_breach_windows,
            high_severity_write_failures = input.high_severity_write_failures,
            high_severity_index_failures = input.high_severity_index_failures,
            "Migration rollback classifier found no trigger"
        );
        return decision;
    };

    if !immediate_triggers.is_empty() {
        for trigger in immediate_triggers {
            if !triggers.contains(&trigger) {
                triggers.push(trigger);
            }
        }
    }
    if !post_cutover_triggers.is_empty() {
        for trigger in post_cutover_triggers {
            if !triggers.contains(&trigger) {
                triggers.push(trigger);
            }
        }
    }

    let trigger_labels: Vec<&'static str> = triggers.iter().map(|t| t.as_str()).collect();
    tracing::warn!(
        stage = input.stage.as_str(),
        rollback_class = rollback_class
            .map(MigrationRollbackClass::as_str)
            .unwrap_or("none"),
        triggers = ?trigger_labels,
        consecutive_slo_breach_windows = input.consecutive_slo_breach_windows,
        high_severity_write_failures = input.high_severity_write_failures,
        high_severity_index_failures = input.high_severity_index_failures,
        "Migration rollback classifier triggered rollback"
    );

    MigrationRollbackDecision {
        should_rollback: true,
        rollback_class,
        triggers,
        stage: input.stage,
        rationale,
    }
}

/// Context for executing migration rollback automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRollbackPlaybookContext {
    /// Selected rollback class from rollback trigger classification.
    pub rollback_class: MigrationRollbackClass,
    /// Stage at which rollback was initiated.
    pub from_stage: MigrationStage,
    /// Checkpoints captured before migration started; restored during post-cutover rollback.
    pub pre_migration_checkpoints: BTreeMap<String, RecorderOffset>,
    /// Optional forensic capture payload for Tier3 data-integrity emergencies.
    pub forensic_capture: Option<MigrationForensicCaptureContext>,
    /// Directory where forensic bundles are persisted.
    pub forensics_output_dir: PathBuf,
}

/// Source/target backend state included in forensic bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationForensicBackendState {
    pub health: bool,
    pub head_offset: Option<RecorderOffset>,
    pub last_checkpoint: Option<RecorderOffset>,
}

/// Migration checkpoint metadata included in forensic bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationForensicMigrationCheckpoint {
    pub last_completed_stage: MigrationStage,
    pub manifest: String,
}

/// Corruption details captured during data integrity emergency rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationForensicCorruptionDetail {
    pub location: String,
    pub affected_ordinals: Vec<u64>,
    pub detail: String,
}

/// Forensic capture context supplied when executing Tier3 rollback automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationForensicCaptureContext {
    pub source_state: MigrationForensicBackendState,
    pub target_state: MigrationForensicBackendState,
    pub migration_checkpoint: MigrationForensicMigrationCheckpoint,
    pub corruption_detail: MigrationForensicCorruptionDetail,
}

/// Forensic artifact persisted to disk during Tier3 rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationForensicBundle {
    pub captured_at_ms: i64,
    pub rollback_class: MigrationRollbackClass,
    pub from_stage: MigrationStage,
    pub source_state: MigrationForensicBackendState,
    pub target_state: MigrationForensicBackendState,
    pub migration_checkpoint: MigrationForensicMigrationCheckpoint,
    pub corruption_detail: MigrationForensicCorruptionDetail,
}

/// Mutable runtime state used by rollback playbook automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRollbackExecutionState {
    /// Whether migration mode is currently active (source writes quiesced).
    pub migration_active: bool,
    /// Current recorder backend selector.
    pub backend_selector: RecorderBackendKind,
    /// Whether target backend has partial migration artifacts.
    pub target_has_partial_data: bool,
    /// Current durable checkpoint view by consumer id.
    pub checkpoints: BTreeMap<String, RecorderOffset>,
    /// Latest source backend health verdict.
    pub source_health: bool,
    /// Latest index/projection health verdict.
    pub index_health: bool,
    /// Whether projection rebuild has been requested.
    pub projection_rebuild_triggered: bool,
    /// Emergency write freeze flag set by Tier3 data-integrity rollback.
    pub emergency_freeze: bool,
}

impl Default for MigrationRollbackExecutionState {
    fn default() -> Self {
        Self {
            migration_active: false,
            backend_selector: RecorderBackendKind::AppendLog,
            target_has_partial_data: false,
            checkpoints: BTreeMap::new(),
            source_health: true,
            index_health: true,
            projection_rebuild_triggered: false,
            emergency_freeze: false,
        }
    }
}

impl MigrationRollbackExecutionState {
    /// Whether recorder writes are currently blocked by emergency freeze.
    #[must_use]
    pub const fn recorder_writes_blocked(&self) -> bool {
        self.emergency_freeze
    }

    /// Manual human-operated re-enable path after Tier3 freeze.
    pub fn manual_reenable_recorder_writes(&mut self) {
        self.emergency_freeze = false;
    }
}

/// Summary emitted after successful rollback playbook execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRollbackExecutionReport {
    pub tier: MigrationRollbackClass,
    pub from_stage: MigrationStage,
    pub migration_active: bool,
    pub backend_selector: RecorderBackendKind,
    pub target_cleared: bool,
    pub projection_rebuild_triggered: bool,
    pub checkpoints_reset: bool,
    pub source_health_verified: bool,
    pub index_health_verified: bool,
    pub emergency_freeze_active: bool,
    pub forensic_bundle_path: Option<PathBuf>,
}

/// Failure cases for rollback playbook automation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MigrationRollbackExecutionError {
    UnsupportedTier {
        rollback_class: MigrationRollbackClass,
    },
    SourceHealthFailed {
        tier: MigrationRollbackClass,
    },
    IndexHealthFailedPostCutover,
    ForensicCaptureMissing,
    ForensicPersistFailed {
        path: PathBuf,
        error: String,
    },
}

/// Execute Tier1/Tier2/Tier3 rollback playbook actions.
///
/// Tier1 (`Immediate`) actions:
/// - clear migration_active flag
/// - clear partial target data marker
/// - reset backend selector to append_log
/// - verify source health
///
/// Tier2 (`PostCutover`) actions:
/// - reset backend selector to append_log
/// - trigger projection rebuild
/// - reset consumer checkpoints to pre-migration snapshot
/// - verify source and index health
///
/// Tier3 (`DataIntegrityEmergency`) actions:
/// - freeze recorder writes until manual re-enable
/// - capture forensic source/target/checkpoint/corruption bundle
/// - persist forensic bundle to timestamped JSON file
/// - emit CRITICAL structured alert logs
pub fn execute_migration_rollback_playbook(
    state: &mut MigrationRollbackExecutionState,
    context: &MigrationRollbackPlaybookContext,
) -> std::result::Result<MigrationRollbackExecutionReport, MigrationRollbackExecutionError> {
    tracing::warn!(
        rollback_executing = true,
        tier = context.rollback_class.as_str(),
        from_stage = context.from_stage.as_str(),
        "Executing migration rollback playbook"
    );

    match context.rollback_class {
        MigrationRollbackClass::Immediate => {
            state.migration_active = false;
            state.target_has_partial_data = false;
            state.backend_selector = RecorderBackendKind::AppendLog;

            if !state.source_health {
                return Err(MigrationRollbackExecutionError::SourceHealthFailed {
                    tier: MigrationRollbackClass::Immediate,
                });
            }

            let report = MigrationRollbackExecutionReport {
                tier: MigrationRollbackClass::Immediate,
                from_stage: context.from_stage,
                migration_active: state.migration_active,
                backend_selector: state.backend_selector,
                target_cleared: !state.target_has_partial_data,
                projection_rebuild_triggered: false,
                checkpoints_reset: false,
                source_health_verified: true,
                index_health_verified: false,
                emergency_freeze_active: state.emergency_freeze,
                forensic_bundle_path: None,
            };

            tracing::info!(
                rollback_complete = true,
                tier = report.tier.as_str(),
                source_health = state.source_health,
                "Migration rollback playbook complete"
            );

            Ok(report)
        }
        MigrationRollbackClass::PostCutover => {
            state.migration_active = false;
            state.backend_selector = RecorderBackendKind::AppendLog;
            state.projection_rebuild_triggered = true;
            state.checkpoints = context.pre_migration_checkpoints.clone();

            if !state.source_health {
                return Err(MigrationRollbackExecutionError::SourceHealthFailed {
                    tier: MigrationRollbackClass::PostCutover,
                });
            }
            if !state.index_health {
                return Err(MigrationRollbackExecutionError::IndexHealthFailedPostCutover);
            }

            let report = MigrationRollbackExecutionReport {
                tier: MigrationRollbackClass::PostCutover,
                from_stage: context.from_stage,
                migration_active: state.migration_active,
                backend_selector: state.backend_selector,
                target_cleared: !state.target_has_partial_data,
                projection_rebuild_triggered: state.projection_rebuild_triggered,
                checkpoints_reset: true,
                source_health_verified: true,
                index_health_verified: true,
                emergency_freeze_active: state.emergency_freeze,
                forensic_bundle_path: None,
            };

            tracing::info!(
                rollback_complete = true,
                tier = report.tier.as_str(),
                source_health = state.source_health,
                index_health = state.index_health,
                "Migration rollback playbook complete"
            );

            Ok(report)
        }
        MigrationRollbackClass::DataIntegrityEmergency => {
            state.migration_active = false;
            state.backend_selector = RecorderBackendKind::AppendLog;
            state.emergency_freeze = true;

            let forensic_capture = context
                .forensic_capture
                .as_ref()
                .ok_or(MigrationRollbackExecutionError::ForensicCaptureMissing)?;

            std::fs::create_dir_all(&context.forensics_output_dir).map_err(|error| {
                MigrationRollbackExecutionError::ForensicPersistFailed {
                    path: context.forensics_output_dir.clone(),
                    error: error.to_string(),
                }
            })?;

            let captured_at = Utc::now();
            let forensic_file_name =
                format!("forensics_{}.json", captured_at.format("%Y%m%d_%H%M%S"));
            let forensic_bundle_path = context.forensics_output_dir.join(forensic_file_name);

            let bundle = MigrationForensicBundle {
                captured_at_ms: captured_at.timestamp_millis(),
                rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
                from_stage: context.from_stage,
                source_state: forensic_capture.source_state.clone(),
                target_state: forensic_capture.target_state.clone(),
                migration_checkpoint: forensic_capture.migration_checkpoint.clone(),
                corruption_detail: forensic_capture.corruption_detail.clone(),
            };

            let serialized = serde_json::to_vec_pretty(&bundle).map_err(|error| {
                MigrationRollbackExecutionError::ForensicPersistFailed {
                    path: forensic_bundle_path.clone(),
                    error: error.to_string(),
                }
            })?;

            std::fs::write(&forensic_bundle_path, serialized).map_err(|error| {
                MigrationRollbackExecutionError::ForensicPersistFailed {
                    path: forensic_bundle_path.clone(),
                    error: error.to_string(),
                }
            })?;

            tracing::error!(
                CRITICAL = true,
                forensic_bundle = %forensic_bundle_path.display(),
                corruption_detail = %bundle.corruption_detail.detail,
                "Data integrity emergency forensic bundle captured"
            );
            tracing::error!(
                recorder_frozen = true,
                reason = "data_integrity_emergency",
                require = "manual_reenable",
                "Recorder writes frozen pending manual re-enable"
            );

            let report = MigrationRollbackExecutionReport {
                tier: MigrationRollbackClass::DataIntegrityEmergency,
                from_stage: context.from_stage,
                migration_active: state.migration_active,
                backend_selector: state.backend_selector,
                target_cleared: !state.target_has_partial_data,
                projection_rebuild_triggered: false,
                checkpoints_reset: false,
                source_health_verified: false,
                index_health_verified: false,
                emergency_freeze_active: true,
                forensic_bundle_path: Some(forensic_bundle_path),
            };

            tracing::info!(
                rollback_complete = true,
                tier = report.tier.as_str(),
                recorder_frozen = state.emergency_freeze,
                "Migration rollback playbook complete"
            );

            Ok(report)
        }
    }
}

/// Registry of all migrations.
///
/// Migrations are applied in order. Each migration's `version` field indicates
/// the schema version AFTER the migration is applied.
///
/// # Adding New Migrations
///
/// 1. Increment `SCHEMA_VERSION` constant
/// 2. Add a new `Migration` entry here with `version = SCHEMA_VERSION`
/// 3. Write idempotent SQL (use IF NOT EXISTS, IF EXISTS where appropriate)
/// 4. Add upgrade test using fixture from previous version
pub(crate) static MIGRATIONS: &[Migration] = &[
    // Version 1: Initial schema (baseline)
    // No migration SQL needed - SCHEMA_SQL creates the full schema
    Migration {
        version: 1,
        description: "Initial schema",
        up_sql: "", // Empty - baseline schema is created via SCHEMA_SQL
        down_sql: None,
    },
    Migration {
        version: 2,
        description: "Add decision_context to audit_actions",
        up_sql: "ALTER TABLE audit_actions ADD COLUMN decision_context TEXT;",
        down_sql: Some("ALTER TABLE audit_actions DROP COLUMN decision_context;"),
    },
    Migration {
        version: 3,
        description: "Add pane_uuid to panes for stable identity",
        up_sql: "ALTER TABLE panes ADD COLUMN pane_uuid TEXT;",
        down_sql: Some("ALTER TABLE panes DROP COLUMN pane_uuid;"),
    },
    Migration {
        version: 4,
        description: "Add action_undo + action_history view + audit_action_id on step logs",
        up_sql: r"
            CREATE INDEX IF NOT EXISTS idx_step_logs_audit_action ON workflow_step_logs(audit_action_id);

            CREATE TABLE IF NOT EXISTS action_undo (
                audit_action_id INTEGER PRIMARY KEY REFERENCES audit_actions(id) ON DELETE CASCADE,
                undoable INTEGER NOT NULL DEFAULT 0,
                undo_strategy TEXT NOT NULL,
                undo_hint TEXT,
                undo_payload TEXT,
                undone_at INTEGER,
                undone_by TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_action_undo_undoable ON action_undo(undoable) WHERE undoable = 1;

            CREATE VIEW IF NOT EXISTS action_history AS
            SELECT a.*,
                   u.undoable, u.undo_strategy, u.undo_hint, u.undone_at, u.undone_by,
                   w.workflow_id, w.step_name
            FROM audit_actions a
            LEFT JOIN action_undo u ON u.audit_action_id = a.id
            LEFT JOIN workflow_step_logs w ON w.audit_action_id = a.id;
        ",
        down_sql: Some(
            r"
            DROP VIEW IF EXISTS action_history;
            DROP INDEX IF EXISTS idx_action_undo_undoable;
            DROP TABLE IF EXISTS action_undo;
            DROP INDEX IF EXISTS idx_step_logs_audit_action;
            ALTER TABLE workflow_step_logs DROP COLUMN audit_action_id;
        ",
        ),
    },
    Migration {
        version: 5,
        description: "Add accounts table for usage tracking and failover selection",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY,
                account_id TEXT NOT NULL,
                service TEXT NOT NULL,
                name TEXT,
                percent_remaining REAL NOT NULL,
                reset_at TEXT,
                tokens_used INTEGER,
                tokens_remaining INTEGER,
                tokens_limit INTEGER,
                last_refreshed_at INTEGER NOT NULL,
                last_used_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_service_account ON accounts(service, account_id);
            CREATE INDEX IF NOT EXISTS idx_accounts_service ON accounts(service);
            CREATE INDEX IF NOT EXISTS idx_accounts_percent ON accounts(service, percent_remaining DESC);
            CREATE INDEX IF NOT EXISTS idx_accounts_last_used ON accounts(service, last_used_at);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_accounts_last_used;
            DROP INDEX IF EXISTS idx_accounts_percent;
            DROP INDEX IF EXISTS idx_accounts_service;
            DROP INDEX IF EXISTS idx_accounts_service_account;
            DROP TABLE IF EXISTS accounts;
        ",
        ),
    },
    Migration {
        version: 6,
        description: "Add wa_meta for version compatibility tracking",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS wa_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                min_compatible_wa TEXT NOT NULL,
                created_by_wa TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
        ",
        down_sql: Some("DROP TABLE IF EXISTS wa_meta;"),
    },
    Migration {
        version: 7,
        description: "Persist workflow action plans and enrich step logs",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS workflow_action_plans (
                workflow_id TEXT PRIMARY KEY REFERENCES workflow_executions(id) ON DELETE CASCADE,
                plan_id TEXT NOT NULL,
                plan_hash TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_action_plans_hash ON workflow_action_plans(plan_hash);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_action_plans_hash;
            DROP TABLE IF EXISTS workflow_action_plans;
        ",
        ),
    },
    Migration {
        version: 8,
        description: "Add pane_reservations for per-pane workflow lock/reservation",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS pane_reservations (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
                owner_kind TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                reason TEXT,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                released_at INTEGER,
                status TEXT NOT NULL DEFAULT 'active'
            );

            CREATE INDEX IF NOT EXISTS idx_reservations_pane_status
                ON pane_reservations(pane_id, status);
            CREATE INDEX IF NOT EXISTS idx_reservations_status
                ON pane_reservations(status);
            CREATE INDEX IF NOT EXISTS idx_reservations_expires
                ON pane_reservations(expires_at) WHERE status = 'active';
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_reservations_expires;
            DROP INDEX IF EXISTS idx_reservations_status;
            DROP INDEX IF EXISTS idx_reservations_pane_status;
            DROP TABLE IF EXISTS pane_reservations;
        ",
        ),
    },
    Migration {
        version: 9,
        description: "Add external_meta to agent_sessions for correlation metadata",
        up_sql: "ALTER TABLE agent_sessions ADD COLUMN external_meta TEXT;",
        down_sql: Some("ALTER TABLE agent_sessions DROP COLUMN external_meta;"),
    },
    Migration {
        version: 10,
        description: "Add FTS index state tables for incremental sync and recovery",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS fts_index_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                index_version INTEGER NOT NULL DEFAULT 1,
                last_full_rebuild_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS fts_pane_progress (
                pane_id INTEGER PRIMARY KEY REFERENCES panes(pane_id) ON DELETE CASCADE,
                last_indexed_seq INTEGER NOT NULL DEFAULT 0,
                indexed_count INTEGER NOT NULL DEFAULT 0,
                last_indexed_at INTEGER NOT NULL
            );

            -- Initialize state with current timestamp
            INSERT OR IGNORE INTO fts_index_state (id, index_version, created_at, updated_at)
            VALUES (1, 1, strftime('%s', 'now') * 1000, strftime('%s', 'now') * 1000);
        ",
        down_sql: Some(
            r"
            DROP TABLE IF EXISTS fts_pane_progress;
            DROP TABLE IF EXISTS fts_index_state;
        ",
        ),
    },
    Migration {
        version: 11,
        description: "Add prepared_plans for prepare/commit plan previews",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS prepared_plans (
                plan_id TEXT PRIMARY KEY,
                plan_hash TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                action_kind TEXT NOT NULL,
                pane_id INTEGER,
                pane_uuid TEXT,
                params_json TEXT,
                plan_json TEXT NOT NULL,
                requires_approval INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                consumed_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_prepared_plans_hash ON prepared_plans(plan_hash);
            CREATE INDEX IF NOT EXISTS idx_prepared_plans_workspace ON prepared_plans(workspace_id);
            CREATE INDEX IF NOT EXISTS idx_prepared_plans_expires ON prepared_plans(expires_at)
                WHERE consumed_at IS NULL;
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_prepared_plans_expires;
            DROP INDEX IF EXISTS idx_prepared_plans_workspace;
            DROP INDEX IF EXISTS idx_prepared_plans_hash;
            DROP TABLE IF EXISTS prepared_plans;
        ",
        ),
    },
    Migration {
        version: 12,
        description: "Add correlation_id to audit_actions for prepare/commit chains",
        up_sql: r"
            ALTER TABLE audit_actions ADD COLUMN correlation_id TEXT;
            CREATE INDEX IF NOT EXISTS idx_audit_actions_correlation ON audit_actions(correlation_id);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_audit_actions_correlation;
            ALTER TABLE audit_actions DROP COLUMN correlation_id;
        ",
        ),
    },
    Migration {
        version: 13,
        description: "Add secret_scan_reports for incremental scan checkpoints",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS secret_scan_reports (
                id INTEGER PRIMARY KEY,
                scope_hash TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                report_version INTEGER NOT NULL,
                last_segment_id INTEGER,
                report_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_secret_scan_reports_scope
                ON secret_scan_reports(scope_hash, created_at);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_secret_scan_reports_scope;
            DROP TABLE IF EXISTS secret_scan_reports;
        ",
        ),
    },
    Migration {
        version: 14,
        description: "Add saved_searches for persisted search definitions",
        up_sql: r#"
            CREATE TABLE IF NOT EXISTS saved_searches (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                query TEXT NOT NULL,
                pane_id INTEGER,
                "limit" INTEGER NOT NULL DEFAULT 50,
                since_mode TEXT NOT NULL DEFAULT 'last_run',
                since_ms INTEGER,
                schedule_interval_ms INTEGER,
                enabled INTEGER NOT NULL DEFAULT 0,
                last_run_at INTEGER,
                last_result_count INTEGER,
                last_error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_saved_searches_enabled
                ON saved_searches(enabled);
            CREATE INDEX IF NOT EXISTS idx_saved_searches_last_run
                ON saved_searches(last_run_at);
        "#,
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_saved_searches_last_run;
            DROP INDEX IF EXISTS idx_saved_searches_enabled;
            DROP TABLE IF EXISTS saved_searches;
        ",
        ),
    },
    Migration {
        version: 15,
        description: "Add event_mutes for noise suppression by identity key",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS event_mutes (
                identity_key TEXT PRIMARY KEY,
                scope TEXT NOT NULL DEFAULT 'workspace',
                created_at INTEGER NOT NULL,
                expires_at INTEGER,
                created_by TEXT,
                reason TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_event_mutes_expires
                ON event_mutes(expires_at) WHERE expires_at IS NOT NULL;
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_event_mutes_expires;
            DROP TABLE IF EXISTS event_mutes;
        ",
        ),
    },
    Migration {
        version: 16,
        description: "Add usage_metrics table for analytics tracking",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS usage_metrics (
                id INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                metric_type TEXT NOT NULL,
                pane_id INTEGER,
                agent_type TEXT,
                account_id TEXT,
                workflow_id TEXT,
                count INTEGER,
                amount REAL,
                tokens INTEGER,
                metadata TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_usage_metrics_timestamp ON usage_metrics(timestamp);
            CREATE INDEX IF NOT EXISTS idx_usage_metrics_type_ts ON usage_metrics(metric_type, timestamp);
            CREATE INDEX IF NOT EXISTS idx_usage_metrics_agent_ts ON usage_metrics(agent_type, timestamp);
            CREATE INDEX IF NOT EXISTS idx_usage_metrics_account_ts ON usage_metrics(account_id, timestamp);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_usage_metrics_account_ts;
            DROP INDEX IF EXISTS idx_usage_metrics_agent_ts;
            DROP INDEX IF EXISTS idx_usage_metrics_type_ts;
            DROP INDEX IF EXISTS idx_usage_metrics_timestamp;
            DROP TABLE IF EXISTS usage_metrics;
        ",
        ),
    },
    Migration {
        version: 17,
        description: "Add notification_history table for persistent notification log",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS notification_history (
                id INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                event_id INTEGER,
                channel TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                severity TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                error_message TEXT,
                acknowledged_at INTEGER,
                acknowledged_by TEXT,
                action_taken TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0,
                metadata TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_notification_history_timestamp ON notification_history(timestamp);
            CREATE INDEX IF NOT EXISTS idx_notification_history_status ON notification_history(status);
            CREATE INDEX IF NOT EXISTS idx_notification_history_event ON notification_history(event_id);
            CREATE INDEX IF NOT EXISTS idx_notification_history_channel_ts ON notification_history(channel, timestamp);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_notification_history_channel_ts;
            DROP INDEX IF EXISTS idx_notification_history_event;
            DROP INDEX IF EXISTS idx_notification_history_status;
            DROP INDEX IF EXISTS idx_notification_history_timestamp;
            DROP TABLE IF EXISTS notification_history;
        ",
        ),
    },
    Migration {
        version: 18,
        description: "Add event triage state + annotations (labels/notes)",
        up_sql: r"
            ALTER TABLE events ADD COLUMN triage_state TEXT;
            ALTER TABLE events ADD COLUMN triage_updated_at INTEGER;
            ALTER TABLE events ADD COLUMN triage_updated_by TEXT;

            CREATE INDEX IF NOT EXISTS idx_events_triage_state
                ON events(triage_state) WHERE triage_state IS NOT NULL;

            CREATE TABLE IF NOT EXISTS event_labels (
                event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                label TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                created_by TEXT,
                PRIMARY KEY (event_id, label)
            );

            CREATE INDEX IF NOT EXISTS idx_event_labels_event ON event_labels(event_id);
            CREATE INDEX IF NOT EXISTS idx_event_labels_label ON event_labels(label);

            CREATE TABLE IF NOT EXISTS event_notes (
                event_id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
                note TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                updated_by TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_event_notes_updated_at ON event_notes(updated_at);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_event_notes_updated_at;
            DROP TABLE IF EXISTS event_notes;

            DROP INDEX IF EXISTS idx_event_labels_label;
            DROP INDEX IF EXISTS idx_event_labels_event;
            DROP TABLE IF EXISTS event_labels;

            DROP INDEX IF EXISTS idx_events_triage_state;

            ALTER TABLE events DROP COLUMN triage_updated_by;
            ALTER TABLE events DROP COLUMN triage_updated_at;
            ALTER TABLE events DROP COLUMN triage_state;
        ",
        ),
    },
    Migration {
        version: 19,
        description: "Add plan_hash binding to approval_tokens",
        up_sql: r"
            ALTER TABLE approval_tokens ADD COLUMN plan_hash TEXT;
            ALTER TABLE approval_tokens ADD COLUMN plan_version INTEGER;
            ALTER TABLE approval_tokens ADD COLUMN risk_summary TEXT;

            CREATE INDEX IF NOT EXISTS idx_approval_tokens_plan_hash
                ON approval_tokens(plan_hash) WHERE plan_hash IS NOT NULL;
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_approval_tokens_plan_hash;
            ALTER TABLE approval_tokens DROP COLUMN risk_summary;
            ALTER TABLE approval_tokens DROP COLUMN plan_version;
            ALTER TABLE approval_tokens DROP COLUMN plan_hash;
        ",
        ),
    },
    Migration {
        version: 20,
        description: "Add pane_bookmarks table for named pane aliases with tags",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS pane_bookmarks (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                alias TEXT NOT NULL UNIQUE,
                tags TEXT,
                description TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_pane_id
                ON pane_bookmarks(pane_id);
            CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_alias
                ON pane_bookmarks(alias);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_pane_bookmarks_alias;
            DROP INDEX IF EXISTS idx_pane_bookmarks_pane_id;
            DROP TABLE IF EXISTS pane_bookmarks;
        ",
        ),
    },
    Migration {
        version: 21,
        description: "Add session persistence tables and rename wa_meta to ft_meta",
        up_sql: r"
            -- Rename wa_meta → ft_meta (rebuild table for column renames)
            CREATE TABLE IF NOT EXISTS ft_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                min_compatible_ft TEXT NOT NULL,
                created_by_ft TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            INSERT OR IGNORE INTO ft_meta (id, schema_version, min_compatible_ft, created_by_ft, created_at)
                SELECT id, schema_version, min_compatible_wa, created_by_wa, created_at
                FROM wa_meta WHERE id = 1;

            DROP TABLE IF EXISTS wa_meta;

            -- Session persistence tables
            CREATE TABLE IF NOT EXISTS mux_sessions (
                session_id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                last_checkpoint_at INTEGER,
                shutdown_clean INTEGER NOT NULL DEFAULT 0,
                topology_json TEXT NOT NULL,
                window_metadata_json TEXT,
                ft_version TEXT NOT NULL,
                host_id TEXT
            );

            CREATE TABLE IF NOT EXISTS session_checkpoints (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                checkpoint_at INTEGER NOT NULL,
                checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_checkpoints_session
                ON session_checkpoints(session_id, checkpoint_at);

            CREATE TABLE IF NOT EXISTS mux_pane_state (
                id INTEGER PRIMARY KEY,
                checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                pane_id INTEGER NOT NULL,
                cwd TEXT,
                command TEXT,
                env_json TEXT,
                terminal_state_json TEXT NOT NULL,
                agent_metadata_json TEXT,
                scrollback_checkpoint_seq INTEGER,
                last_output_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
            CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);
        ",
        down_sql: Some(
            r"
            -- Drop session persistence tables
            DROP INDEX IF EXISTS idx_pane_state_pane;
            DROP INDEX IF EXISTS idx_pane_state_checkpoint;
            DROP TABLE IF EXISTS mux_pane_state;

            DROP INDEX IF EXISTS idx_checkpoints_session;
            DROP TABLE IF EXISTS session_checkpoints;

            DROP TABLE IF EXISTS mux_sessions;

            -- Restore wa_meta from ft_meta
            CREATE TABLE IF NOT EXISTS wa_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                min_compatible_wa TEXT NOT NULL,
                created_by_wa TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            INSERT OR IGNORE INTO wa_meta (id, schema_version, min_compatible_wa, created_by_wa, created_at)
                SELECT id, schema_version, min_compatible_ft, created_by_ft, created_at
                FROM ft_meta WHERE id = 1;

            DROP TABLE IF EXISTS ft_meta;
        ",
        ),
    },
    Migration {
        version: 22,
        description: "Add segment embeddings table for semantic search",
        // Historical note: this migration originally created a non-canonical
        // shape (`segment_id INTEGER PRIMARY KEY REFERENCES segments(id)`) —
        // a dangling FK to a table that never existed plus a single-column
        // PK. Migration v23 (`ensure_segment_embeddings_schema`) was shipped
        // to repair databases that ran the broken DDL. The SQL below now
        // matches `create_segment_embeddings_table` so legacy upgrades create
        // the canonical table directly and v23 degrades to a no-op repair.
        up_sql: r"
            CREATE TABLE IF NOT EXISTS segment_embeddings (
                segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
                embedder_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector BLOB NOT NULL,
                -- epoch MILLISECONDS (ft-ayy9x / ft-wi24o); kept in sync with
                -- create_segment_embeddings_table and schema_ddl.rs.
                embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
                PRIMARY KEY (segment_id, embedder_id)
            );

            CREATE INDEX IF NOT EXISTS idx_segment_embeddings_embedder
                ON segment_embeddings(embedder_id);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_segment_embeddings_embedder;
            DROP TABLE IF EXISTS segment_embeddings;
        ",
        ),
    },
    Migration {
        version: 23,
        description: "Repair segment embeddings schema for semantic retrieval",
        up_sql: "",
        down_sql: Some(""),
    },
    // ft-h90rh: dedicated audit stream for policy-denied MCP mutations.
    // Separate from audit_actions (which records successful actions + decisions),
    // this table captures the deny/require_approval attempts that mcp_authorize_mcp_mutation
    // currently only surfaces through tracing::warn!. Additive-only, idempotent schema so
    // down-rollback is a straight DROP TABLE.
    Migration {
        version: 24,
        description: "Add policy_denied_audit table for persistent policy-denial records",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS policy_denied_audit (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            ts_ms        INTEGER NOT NULL,
            agent_id     TEXT,
            tool_name    TEXT    NOT NULL,
            intent_hash  TEXT,
            reason       TEXT    NOT NULL,
            reason_code  TEXT    NOT NULL,
            rule_id      TEXT,
            decision     TEXT    NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_policy_denied_audit_ts
            ON policy_denied_audit(ts_ms);
        CREATE INDEX IF NOT EXISTS idx_policy_denied_audit_tool_ts
            ON policy_denied_audit(tool_name, ts_ms);
        ",
        down_sql: Some(
            "DROP INDEX IF EXISTS idx_policy_denied_audit_tool_ts;
             DROP INDEX IF EXISTS idx_policy_denied_audit_ts;
             DROP TABLE IF EXISTS policy_denied_audit;",
        ),
    },
    // br-ft-4yr9i: register the agent_profiles schema in the
    // migration runner. The substrate constants
    // AGENT_PROFILES_SCHEMA + AGENT_PROFILES_ROLE_INDEX (from
    // crates/frankenterm-core/src/agent_profiles.rs, shipped at
    // ft-df3cz first slice 810a4f0cd) are re-emitted here so a
    // fresh database picks up the table on first migration and
    // an existing database picks it up on the next migrate-up.
    //
    // Both statements use IF NOT EXISTS, so the migration is
    // idempotent — re-running it on a database that already has
    // the table is a no-op. Down-rollback is a straight DROP
    // INDEX + DROP TABLE.
    Migration {
        version: 25,
        description: "Add agent_profiles table + role index (ft-4yr9i / ft-df3cz)",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS agent_profiles (
            name           TEXT PRIMARY KEY NOT NULL,
            role           TEXT NOT NULL DEFAULT '',
            tags           TEXT NOT NULL DEFAULT '[]',
            shell          TEXT NOT NULL DEFAULT '',
            command        TEXT,
            env            TEXT NOT NULL DEFAULT '{}',
            metadata       TEXT NOT NULL DEFAULT '{}',
            created_at_ms  INTEGER NOT NULL,
            updated_at_ms  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS agent_profiles_role_idx
            ON agent_profiles(role);
        ",
        down_sql: Some(
            "DROP INDEX IF EXISTS agent_profiles_role_idx;
             DROP TABLE IF EXISTS agent_profiles;",
        ),
    },
    // br-ft-4iz0q substrate-pass: register the profiles_applied_log
    // schema in the migration runner. The daemon-side
    // RobotProfile.apply handler (the wired-pass cont-bead under
    // ft-4iz0q) writes one row per non-dry-run apply request;
    // duplicate apply requests with the same content_hash hit
    // the existing row and short-circuit to ProfileApplyData {
    // panes_spawned: <prior>, dry_run: false } per the bead's
    // idempotency rule.
    //
    // Schema mirrors the `ApplyReceipt` substrate type at
    // crates/frankenterm-core/src/robot_profile_handler.rs:
    //   - content_hash: PRIMARY KEY (hex SHA-256, 64 chars)
    //   - profile_name: profile.name at apply time
    //   - profile_updated_at_ms: profile.updated_at_ms at apply time
    //   - count: requested pane count
    //   - panes_spawned_json: JSON-encoded Vec<u64> of pane IDs
    //     the original apply spawned
    //   - recorded_at_ms: unix epoch ms of receipt write
    //
    // panes_spawned is stored as JSON because the substrate-pass
    // table is typed-key + JSON-blob until the wired-pass cont-bead
    // promotes it to a proper FK relationship to a panes table
    // (which doesn't exist in this scope).
    //
    // Both statements use IF NOT EXISTS, so the migration is
    // idempotent. Down-rollback drops the table.
    Migration {
        version: 26,
        description: "Add profiles_applied_log table for RobotProfile.apply idempotency \
                      (ft-4iz0q substrate-pass)",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS profiles_applied_log (
            content_hash         TEXT PRIMARY KEY NOT NULL,
            profile_name         TEXT NOT NULL,
            profile_updated_at_ms INTEGER NOT NULL,
            count                INTEGER NOT NULL,
            panes_spawned_json   TEXT NOT NULL DEFAULT '[]',
            recorded_at_ms       INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS profiles_applied_log_profile_name_idx
            ON profiles_applied_log(profile_name);
        ",
        down_sql: Some(
            "DROP INDEX IF EXISTS profiles_applied_log_profile_name_idx;
             DROP TABLE IF EXISTS profiles_applied_log;",
        ),
    },
    // ft-27rlg: durable fleet mutation receipts for non-dry-run
    // `ft robot fleet scale` and `ft robot fleet rebalance`. The
    // in-memory FleetMutationLedger still executes and builds typed
    // receipts; this table records the completed receipt keyed by the
    // plan idempotency key so a fresh CLI/daemon process can replay
    // identical requests and reject same-key/different-payload retries
    // before side effects.
    Migration {
        version: 27,
        description: "Add fleet_mutation_receipts table for durable fleet scale/rebalance \
                      idempotency (ft-27rlg)",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS fleet_mutation_receipts (
            idempotency_key     TEXT PRIMARY KEY NOT NULL,
            payload_fingerprint TEXT NOT NULL,
            action              TEXT NOT NULL,
            plan_id             TEXT NOT NULL,
            dry_run             INTEGER NOT NULL DEFAULT 0,
            receipt_json        TEXT NOT NULL,
            recorded_at_ms      INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS fleet_mutation_receipts_action_time_idx
            ON fleet_mutation_receipts(action, recorded_at_ms DESC);
        ",
        down_sql: Some(
            "DROP INDEX IF EXISTS fleet_mutation_receipts_action_time_idx;
             DROP TABLE IF EXISTS fleet_mutation_receipts;",
        ),
    },
    // ft-7h5da.8.1: durable limit windows for usage/rate-limit reset
    // forecasting. The table stores one idempotent row per pane/account
    // service key; known accounts link to the existing accounts table, while
    // unknown account detections stay durable under account_id='unknown' so
    // unparseable or under-specified limit events cannot disappear.
    Migration {
        version: 28,
        description: "Add limit_windows table for pane/account rate-limit reset ledger \
                      (ft-7h5da.8.1)",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS limit_windows (
            id INTEGER PRIMARY KEY,
            pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
            service TEXT NOT NULL,
            account_id TEXT NOT NULL,
            account_db_id INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
            account_known INTEGER NOT NULL DEFAULT 0,
            agent_type TEXT,
            rule_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            limited_at INTEGER NOT NULL,
            reset_at INTEGER,
            reset_source TEXT NOT NULL,
            reset_text TEXT,
            conservative_ttl_ms INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            seen_count INTEGER NOT NULL DEFAULT 1,
            metadata TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            CHECK(account_known IN (0, 1)),
            CHECK(reset_source IN ('absolute', 'retry_after', 'unknown_ttl')),
            CHECK(seen_count >= 1),
            UNIQUE(pane_id, service, account_id)
        );

        CREATE INDEX IF NOT EXISTS idx_limit_windows_pane_account
            ON limit_windows(pane_id, service, account_id);
        CREATE INDEX IF NOT EXISTS idx_limit_windows_service_reset
            ON limit_windows(service, reset_at);
        CREATE INDEX IF NOT EXISTS idx_limit_windows_last_seen
            ON limit_windows(last_seen_at);
        ",
        down_sql: Some(
            "DROP INDEX IF EXISTS idx_limit_windows_last_seen;
             DROP INDEX IF EXISTS idx_limit_windows_service_reset;
             DROP INDEX IF EXISTS idx_limit_windows_pane_account;
             DROP TABLE IF EXISTS limit_windows;",
        ),
    },
    Migration {
        version: 29,
        description: "Stamp output_segments with the redaction catalog version in \
                      effect at capture, so corpus cleanliness is queryable per \
                      segment (ft-7h5da.1.5). Existing rows keep NULL = catalog \
                      unknown at capture.",
        up_sql: r"
        ALTER TABLE output_segments ADD COLUMN redaction_catalog_version TEXT;
        ",
        down_sql: Some("ALTER TABLE output_segments DROP COLUMN redaction_catalog_version;"),
    },
    Migration {
        version: 30,
        description: "Normalize segment_embeddings.embedded_at from epoch seconds \
                      to epoch milliseconds (ft-ayy9x). The column was written with \
                      strftime('%s','now') (seconds) while the schema convention is \
                      epoch ms, a latent 1000x unit trap for future embedding GC. \
                      Existing rows whose value is clearly seconds (< 1e11, i.e. any \
                      timestamp before year ~5138 in seconds / ~1973 in ms) are scaled \
                      to ms; already-ms rows are left untouched. Idempotent: re-running \
                      is a no-op because converted values are >= 1e11.",
        up_sql: "UPDATE segment_embeddings \
                 SET embedded_at = embedded_at * 1000 \
                 WHERE embedded_at < 100000000000;",
        // Not cleanly reversible: after the up-migration, genuinely-ms rows and
        // converted-from-seconds rows are indistinguishable, so a down-migration
        // would wrongly divide both. Leave as a forward-only normalization.
        down_sql: None,
    },
    Migration {
        version: 31,
        description: "Stamp output_segments with best-effort semantic zone type \
                      metadata at capture (ft-7h5da.2.3). Existing historical \
                      rows keep NULL = untyped/unavailable; raw captured bytes \
                      remain canonical.",
        up_sql: r"
        ALTER TABLE output_segments ADD COLUMN zone_type TEXT;
        CREATE INDEX IF NOT EXISTS idx_segments_zone_type ON output_segments(zone_type);
        ",
        down_sql: Some(
            "DROP INDEX IF EXISTS idx_segments_zone_type;
             ALTER TABLE output_segments DROP COLUMN zone_type;",
        ),
    },
    Migration {
        version: 32,
        description: "Repair segment_embeddings.embedded_at column DEFAULT on \
                      upgraded DBs (ft-wi24o). Migration v30 normalized existing \
                      embedded_at VALUES from epoch seconds to ms but left the \
                      column DEFAULT as the v22/v23 seconds expression \
                      strftime('%s','now'), so an INSERT omitting embedded_at \
                      still stored seconds — reintroducing the 1000x unit trap \
                      v30 closes and diverging from a fresh DB's ms default. The \
                      real work runs in apply_migration_step \
                      (ensure_segment_embeddings_embedded_at_default_ms): a \
                      conditional table rebuild that swaps the seconds default \
                      for strftime('%s','now')*1000 ONLY when the current default \
                      is still seconds, preserving rows (already-ms after v30) \
                      and the embedder index. Idempotent / no-op on fresh and \
                      already-ms databases.",
        // Repair is applied by the Rust step in `apply_migration_step` (the
        // default change requires a table rebuild that cannot be expressed as a
        // single guarded SQL statement); this string is reference-only and never
        // executed (apply_raw_up_sql is set false for v32).
        up_sql: "-- ft-wi24o: see ensure_segment_embeddings_embedded_at_default_ms",
        // Forward-only: the seconds default was a latent unit bug, not a state
        // worth restoring.
        down_sql: None,
    },
    Migration {
        version: 33,
        description: "Add expiring token-owned event-delivery leases for flush-before-handle streams",
        // `SCHEMA_SQL` already contains these columns for fresh databases.
        // `apply_migration_step` routes v33 through the guarded Rust helper
        // below, so this SQL is reference/rollback-plan material rather than an
        // unconditional fresh-init ALTER.
        up_sql: r"
        ALTER TABLE events ADD COLUMN delivery_lease_token TEXT;
        ALTER TABLE events ADD COLUMN delivery_lease_acquired_at INTEGER;
        ALTER TABLE events ADD COLUMN delivery_lease_expires_at INTEGER;
        ",
        down_sql: Some(
            r"
            ALTER TABLE events DROP COLUMN delivery_lease_expires_at;
            ALTER TABLE events DROP COLUMN delivery_lease_acquired_at;
            ALTER TABLE events DROP COLUMN delivery_lease_token;
            ",
        ),
    },
    Migration {
        version: 34,
        description: "Add transactional event-retention interval evidence and monotonic event IDs",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS event_retention_state (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            cursor_epoch TEXT NOT NULL CHECK (
                length(cursor_epoch) = 32
                AND cursor_epoch NOT GLOB '*[^0-9a-f]*'
            ),
            legacy_history_complete INTEGER NOT NULL CHECK (legacy_history_complete IN (0, 1)),
            generation INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0),
            evidence_from_event_id INTEGER NOT NULL CHECK (evidence_from_event_id > 0),
            max_event_id INTEGER NOT NULL DEFAULT 0
                CHECK (max_event_id >= 0 AND max_event_id >= evidence_from_event_id - 1),
            deleted_event_count INTEGER NOT NULL DEFAULT 0 CHECK (deleted_event_count >= 0),
            last_deleted_at INTEGER CHECK (last_deleted_at IS NULL OR last_deleted_at >= 0),
            CHECK (legacy_history_complete = 0 OR evidence_from_event_id = 1),
            CHECK (deleted_event_count >= generation),
            CHECK (
                (generation = 0 AND deleted_event_count = 0 AND last_deleted_at IS NULL)
                OR (generation > 0 AND deleted_event_count > 0 AND last_deleted_at IS NOT NULL)
            )
        );

        INSERT OR IGNORE INTO event_retention_state (
            singleton, cursor_epoch, legacy_history_complete,
            generation, evidence_from_event_id, max_event_id,
            deleted_event_count, last_deleted_at
        )
        SELECT
            1,
            lower(hex(randomblob(16))),
            0,
            0,
            CASE
                WHEN MAX(id) IS NULL THEN 1
                WHEN MAX(id) >= 9223372036854775807 THEN 9223372036854775807
                ELSE MAX(id) + 1
            END,
            COALESCE(MAX(id), 0),
            0,
            NULL
        FROM events;

        CREATE INDEX IF NOT EXISTS idx_events_segment_id
            ON events(segment_id) WHERE segment_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_workflows_trigger_event_id
            ON workflow_executions(trigger_event_id) WHERE trigger_event_id IS NOT NULL;

        CREATE TABLE IF NOT EXISTS event_retention_intervals (
            start_id INTEGER PRIMARY KEY CHECK (start_id > 0),
            end_id INTEGER NOT NULL CHECK (end_id >= start_id),
            first_generation INTEGER NOT NULL CHECK (first_generation > 0),
            last_generation INTEGER NOT NULL CHECK (last_generation >= first_generation),
            first_deleted_at INTEGER NOT NULL CHECK (first_deleted_at >= 0),
            last_deleted_at INTEGER NOT NULL CHECK (last_deleted_at >= first_deleted_at)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_event_retention_intervals_end
            ON event_retention_intervals(end_id);

        CREATE TABLE IF NOT EXISTS event_retention_delete_authorizations (
            event_id INTEGER PRIMARY KEY CHECK (event_id > 0)
        );

        CREATE TABLE IF NOT EXISTS event_retention_rotation_authorizations (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1)
        );

        CREATE TRIGGER IF NOT EXISTS event_retention_state_delete_guard
        BEFORE DELETE ON event_retention_state
        BEGIN
            SELECT RAISE(ABORT, 'event retention state is permanent');
        END;

        CREATE TRIGGER IF NOT EXISTS event_retention_state_monotonic_guard
        BEFORE UPDATE ON event_retention_state
        WHEN NEW.generation < OLD.generation
          OR NEW.evidence_from_event_id < OLD.evidence_from_event_id
          OR NEW.max_event_id < OLD.max_event_id
          OR NEW.deleted_event_count < OLD.deleted_event_count
          OR (
              OLD.last_deleted_at IS NOT NULL
              AND (NEW.last_deleted_at IS NULL OR NEW.last_deleted_at < OLD.last_deleted_at)
          )
          OR (OLD.legacy_history_complete = 0 AND NEW.legacy_history_complete = 1)
        BEGIN
            SELECT RAISE(ABORT, 'event retention authority cannot move backwards');
        END;

        CREATE TRIGGER IF NOT EXISTS event_retention_state_rotation_guard
        BEFORE UPDATE ON event_retention_state
        WHEN (
                NEW.cursor_epoch != OLD.cursor_epoch
                OR NEW.evidence_from_event_id != OLD.evidence_from_event_id
                OR NEW.legacy_history_complete != OLD.legacy_history_complete
             )
         AND (
                NOT EXISTS (
                    SELECT 1 FROM event_retention_rotation_authorizations
                    WHERE singleton = 1
                )
                OR NEW.cursor_epoch = OLD.cursor_epoch
                OR NEW.legacy_history_complete != 0
                OR OLD.max_event_id >= 9223372036854775807
                OR NEW.evidence_from_event_id != OLD.max_event_id + 1
                OR NEW.generation != OLD.generation
                OR NEW.max_event_id != OLD.max_event_id
                OR NEW.deleted_event_count != OLD.deleted_event_count
                OR NEW.last_deleted_at IS NOT OLD.last_deleted_at
             )
        BEGIN
            SELECT RAISE(ABORT, 'event retention epoch rotation must be atomic and authorized');
        END;

        CREATE TRIGGER IF NOT EXISTS event_retention_state_rotation_clear_intervals
        AFTER UPDATE OF cursor_epoch ON event_retention_state
        WHEN NEW.cursor_epoch != OLD.cursor_epoch
        BEGIN
            DELETE FROM event_retention_intervals;
        END;

        CREATE TRIGGER IF NOT EXISTS event_retention_intervals_insert_guard
        BEFORE INSERT ON event_retention_intervals
        WHEN EXISTS (
            SELECT 1 FROM event_retention_intervals AS existing
            WHERE existing.end_id >= CASE
                      WHEN NEW.start_id > 1 THEN NEW.start_id - 1 ELSE 1
                  END
              AND existing.start_id <= CASE
                      WHEN NEW.end_id < 9223372036854775807 THEN NEW.end_id + 1
                      ELSE 9223372036854775807
                  END
        )
        BEGIN
            SELECT RAISE(ABORT, 'event retention intervals must be disjoint and non-adjacent');
        END;

        CREATE TRIGGER IF NOT EXISTS event_retention_intervals_update_guard
        BEFORE UPDATE ON event_retention_intervals
        BEGIN
            SELECT RAISE(ABORT, 'event retention intervals are replace-only');
        END;

        CREATE TRIGGER IF NOT EXISTS event_retention_intervals_delete_guard
        BEFORE DELETE ON event_retention_intervals
        WHEN NOT EXISTS (SELECT 1 FROM event_retention_delete_authorizations)
         AND NOT EXISTS (
             SELECT 1 FROM event_retention_rotation_authorizations WHERE singleton = 1
         )
        BEGIN
            SELECT RAISE(ABORT, 'event retention interval deletion requires an authorized batch or epoch rotation');
        END;

        CREATE TRIGGER IF NOT EXISTS events_monotonic_id_guard
        BEFORE INSERT ON events
        WHEN NOT EXISTS (
                 SELECT 1 FROM event_retention_state WHERE singleton = 1
             )
             OR NEW.id <= COALESCE((
                 SELECT max_event_id FROM event_retention_state WHERE singleton = 1
             ), 9223372036854775807)
        BEGIN
            SELECT RAISE(ABORT, 'events.id must advance the durable event high-water mark');
        END;

        CREATE TRIGGER IF NOT EXISTS events_monotonic_id_advance
        AFTER INSERT ON events
        BEGIN
            UPDATE event_retention_state
            SET max_event_id = NEW.id
            WHERE singleton = 1;
        END;

        CREATE TRIGGER IF NOT EXISTS events_id_update_guard
        BEFORE UPDATE OF id ON events
        WHEN NEW.id != OLD.id
        BEGIN
            SELECT RAISE(ABORT, 'events.id is immutable once allocated');
        END;

        CREATE TRIGGER IF NOT EXISTS events_retention_delete_guard
        BEFORE DELETE ON events
        WHEN NOT EXISTS (
            SELECT 1 FROM event_retention_delete_authorizations
            WHERE event_id = OLD.id
        )
        BEGIN
            SELECT RAISE(ABORT, 'event deletion requires transactional retention evidence');
        END;
        ",
        // Retention deletions are irreversible.  Dropping their evidence on a
        // schema rollback would turn known data loss back into a silent cursor
        // skip, so v34 deliberately establishes a new forward-only floor.
        down_sql: None,
    },
    Migration {
        version: 35,
        description: "Add unhandled-event hot-path and per-pane output activity indexes",
        // Drop same-name objects first so the migration repairs a malformed
        // pre-existing index instead of letting IF NOT EXISTS preserve it.
        // The v34 constant-key partial index is replaced by indexes matching
        // the incompatible hot-path access patterns: newest-first refreshes,
        // ascending durable-cursor delivery, and exact pane counts. Keeping
        // only the detected-at order would force cursor reads and pane counts
        // to scan or sort the full unhandled set.
        up_sql: r"
        DROP INDEX IF EXISTS idx_events_unhandled;
        DROP INDEX IF EXISTS idx_events_unhandled_detected;
        DROP INDEX IF EXISTS idx_events_unhandled_id;
        DROP INDEX IF EXISTS idx_events_unhandled_pane;
        CREATE INDEX idx_events_unhandled_detected
            ON events(detected_at DESC, id DESC) WHERE handled_at IS NULL;
        CREATE INDEX idx_events_unhandled_id
            ON events(id ASC) WHERE handled_at IS NULL;
        CREATE INDEX idx_events_unhandled_pane
            ON events(pane_id ASC) WHERE handled_at IS NULL;

        DROP INDEX IF EXISTS idx_segments_pane_captured;
        CREATE INDEX idx_segments_pane_captured
            ON output_segments(pane_id, captured_at DESC);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_events_unhandled_detected;
            DROP INDEX IF EXISTS idx_events_unhandled_id;
            DROP INDEX IF EXISTS idx_events_unhandled_pane;
            DROP INDEX IF EXISTS idx_events_unhandled;
            CREATE INDEX idx_events_unhandled
                ON events(handled_at) WHERE handled_at IS NULL;

            DROP INDEX IF EXISTS idx_segments_pane_captured;
            ",
        ),
    },
    Migration {
        version: 36,
        description: "Bind checkpoint role/topology and deterministic latest indexes",
        // Applied by ensure_checkpoint_snapshot_authority_schema so fresh v0
        // initialization and v35 upgrades share one idempotent path.
        up_sql: "",
        // Historical per-checkpoint topology cannot be reconstructed after a
        // downgrade. Keep this authority boundary forward-only rather than
        // silently returning to latest-topology/historical-pane hybrids.
        down_sql: None,
    },
    Migration {
        version: 37,
        description: "Bind clean session state to an exact checkpoint receipt",
        // Applied by ensure_clean_checkpoint_receipt_schema so fresh v0
        // initialization and v36 upgrades share one idempotent path.
        up_sql: "",
        // Removing the receipt identity would make clean-state invalidation
        // ambiguous again, so this authority migration is forward-only.
        down_sql: None,
    },
    Migration {
        version: 38,
        description: "Make checkpoint IDs non-reusable and model restore intent linkage",
        // Applied by ensure_checkpoint_identity_v38_schema. Rebuilding the
        // circularly referenced checkpoint table requires foreign_keys=OFF
        // before BEGIN, so apply_migration_step uses the dedicated wrapper.
        up_sql: "",
        // Reverting to reusable rowids can silently alias durable clean and
        // restore-attempt identities. This authority boundary is forward-only.
        down_sql: None,
    },
    Migration {
        version: 39,
        description: "Maintain exact retained scrollback segment metadata for bounded snapshots",
        up_sql: r"
        CREATE TABLE IF NOT EXISTS pane_scrollback_summary (
            pane_id INTEGER PRIMARY KEY
                REFERENCES panes(pane_id) ON DELETE NO ACTION
                DEFERRABLE INITIALLY DEFERRED,
            retained_segment_count INTEGER NOT NULL DEFAULT 0
                CHECK(typeof(retained_segment_count) = 'integer' AND retained_segment_count >= 0),
            first_seq INTEGER
                CHECK(first_seq IS NULL OR (typeof(first_seq) = 'integer' AND first_seq >= 0)),
            last_seq INTEGER
                CHECK(last_seq IS NULL OR (typeof(last_seq) = 'integer' AND last_seq >= 0)),
            first_captured_at INTEGER
                CHECK(first_captured_at IS NULL OR
                      (typeof(first_captured_at) = 'integer' AND first_captured_at >= 0)),
            last_captured_at INTEGER
                CHECK(last_captured_at IS NULL OR
                      (typeof(last_captured_at) = 'integer' AND last_captured_at >= 0)),
            CHECK(
                (retained_segment_count = 0 AND
                 first_seq IS NULL AND last_seq IS NULL AND
                 first_captured_at IS NULL AND last_captured_at IS NULL)
                OR
                (retained_segment_count > 0 AND
                 first_seq IS NOT NULL AND last_seq IS NOT NULL AND first_seq <= last_seq AND
                 first_captured_at IS NOT NULL AND last_captured_at IS NOT NULL AND
                 first_captured_at <= last_captured_at)
            )
        );

        INSERT OR IGNORE INTO pane_scrollback_summary (
            pane_id, retained_segment_count, first_seq, last_seq,
            first_captured_at, last_captured_at
        )
        SELECT panes.pane_id,
               count(output_segments.id),
               min(output_segments.seq),
               max(output_segments.seq),
               min(output_segments.captured_at),
               max(output_segments.captured_at)
        FROM panes
        LEFT JOIN output_segments ON output_segments.pane_id = panes.pane_id
        GROUP BY panes.pane_id;

        CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_panes_ai
        AFTER INSERT ON panes BEGIN
            INSERT INTO pane_scrollback_summary (pane_id) VALUES (new.pane_id);
        END;

        CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_panes_ad
        AFTER DELETE ON panes BEGIN
            DELETE FROM pane_scrollback_summary WHERE pane_id = old.pane_id;
        END;

        CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_bi
        BEFORE INSERT ON pane_scrollback_summary
        WHEN NOT EXISTS (SELECT 1 FROM panes WHERE pane_id = new.pane_id) BEGIN
            SELECT RAISE(ABORT, 'scrollback summary requires a persisted pane');
        END;

        CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_bd
        BEFORE DELETE ON pane_scrollback_summary
        WHEN EXISTS (SELECT 1 FROM panes WHERE pane_id = old.pane_id) BEGIN
            SELECT RAISE(ABORT, 'live pane scrollback summary is permanent');
        END;

        CREATE TRIGGER IF NOT EXISTS pane_scrollback_summary_pane_id_bu
        BEFORE UPDATE OF pane_id ON pane_scrollback_summary
        WHEN new.pane_id != old.pane_id BEGIN
            SELECT RAISE(ABORT, 'scrollback summary pane identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_bi
        BEFORE INSERT ON output_segments
        WHEN typeof(new.seq) != 'integer' OR new.seq < 0 OR
             typeof(new.captured_at) != 'integer' OR new.captured_at < 0 OR
             NOT EXISTS (
                 SELECT 1 FROM pane_scrollback_summary WHERE pane_id = new.pane_id
             ) BEGIN
            SELECT RAISE(ABORT, 'invalid output segment metadata or missing scrollback summary');
        END;

        CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_ai
        AFTER INSERT ON output_segments BEGIN
            UPDATE pane_scrollback_summary
            SET retained_segment_count = retained_segment_count + 1,
                first_seq = CASE
                    WHEN retained_segment_count = 0 THEN new.seq
                    ELSE min(first_seq, new.seq)
                END,
                last_seq = CASE
                    WHEN retained_segment_count = 0 THEN new.seq
                    ELSE max(last_seq, new.seq)
                END,
                first_captured_at = CASE
                    WHEN retained_segment_count = 0 THEN new.captured_at
                    ELSE min(first_captured_at, new.captured_at)
                END,
                last_captured_at = CASE
                    WHEN retained_segment_count = 0 THEN new.captured_at
                    ELSE max(last_captured_at, new.captured_at)
                END
            WHERE pane_id = new.pane_id;
        END;

        CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_bd
        BEFORE DELETE ON output_segments
        WHEN EXISTS (SELECT 1 FROM panes WHERE pane_id = old.pane_id) AND
             NOT EXISTS (
                 SELECT 1 FROM pane_scrollback_summary WHERE pane_id = old.pane_id
             ) BEGIN
            SELECT RAISE(ABORT, 'missing scrollback summary during output retention');
        END;

        CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_summary_ad
        AFTER DELETE ON output_segments BEGIN
            UPDATE pane_scrollback_summary
            SET retained_segment_count = retained_segment_count - 1,
                first_seq = CASE
                    WHEN retained_segment_count = 1 THEN NULL
                    WHEN old.seq = first_seq THEN (
                        SELECT min(seq) FROM output_segments WHERE pane_id = old.pane_id
                    )
                    ELSE first_seq
                END,
                last_seq = CASE
                    WHEN retained_segment_count = 1 THEN NULL
                    WHEN old.seq = last_seq THEN (
                        SELECT max(seq) FROM output_segments WHERE pane_id = old.pane_id
                    )
                    ELSE last_seq
                END,
                first_captured_at = CASE
                    WHEN retained_segment_count = 1 THEN NULL
                    WHEN old.captured_at = first_captured_at THEN (
                        SELECT min(captured_at)
                        FROM output_segments WHERE pane_id = old.pane_id
                    )
                    ELSE first_captured_at
                END,
                last_captured_at = CASE
                    WHEN retained_segment_count = 1 THEN NULL
                    WHEN old.captured_at = last_captured_at THEN (
                        SELECT max(captured_at)
                        FROM output_segments WHERE pane_id = old.pane_id
                    )
                    ELSE last_captured_at
                END
            WHERE pane_id = old.pane_id;
        END;

        CREATE TRIGGER IF NOT EXISTS output_segments_scrollback_metadata_bu
        BEFORE UPDATE OF pane_id, seq, captured_at ON output_segments
        WHEN new.pane_id != old.pane_id OR new.seq != old.seq OR
             new.captured_at != old.captured_at BEGIN
            SELECT RAISE(ABORT, 'output segment scrollback metadata is immutable');
        END;
        ",
        // The summary is derived, but rolling the schema back would restore an
        // unbounded snapshot query and remove the invariant guards while the
        // current binary still depends on them. Keep this performance and
        // correctness boundary forward-only.
        down_sql: None,
    },
    Migration {
        version: 40,
        description: "Maintain exact logical retained-session byte authority",
        // Applied by `ensure_session_retained_size_schema` from the canonical
        // marked SCHEMA_SQL section so the fresh and upgrade definitions
        // cannot drift apart.
        up_sql: "",
        // Removing the summary would make cleanup fall back to the historical
        // incomplete pane-JSON estimate. Keep the exact byte authority
        // forward-only.
        down_sql: None,
    },
];

// =============================================================================
// Schema Initialization & Migrations
// =============================================================================

/// Get the current schema version from PRAGMA user_version.
///
/// Returns 0 for fresh databases that haven't been initialized.
pub fn get_user_version(conn: &Connection) -> Result<i32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| StorageError::Database(format!("Failed to read user_version: {e}")).into())
}

/// Set the schema version using PRAGMA user_version.
pub(crate) fn set_user_version(conn: &Connection, version: i32) -> Result<()> {
    // PRAGMA doesn't support parameters, so we format directly
    // Version is an i32, so no SQL injection risk
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))
        .map_err(|e| {
            StorageError::MigrationFailed(format!("Failed to set user_version: {e}")).into()
        })
}

/// Record a migration in the schema_version audit table.
fn record_migration(conn: &Connection, version: i32, description: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_version (version, applied_at, description) VALUES (?1, ?2, ?3)",
        params![version, now_ms(), description],
    )
    .map_err(|e| StorageError::MigrationFailed(format!("Failed to record migration: {e}")))?;

    Ok(())
}

/// Initialize or migrate the database schema.
///
/// This function handles both fresh databases and existing databases that
/// need migration to a newer schema version.
///
/// # Behavior
///
/// - Fresh database (user_version = 0): Creates all tables via SCHEMA_SQL
/// - Existing database (user_version < SCHEMA_VERSION): Applies pending migrations
/// - Up-to-date database (user_version = SCHEMA_VERSION): Atomically validates
///   and repairs the FrankenTerm metadata row when necessary
///
/// # Errors
///
/// Returns an error if:
/// - The database has a newer schema than this code supports
/// - Any migration fails to apply
pub fn initialize_schema(conn: &Connection) -> Result<()> {
    let current = get_user_version(conn)?;
    let needs_init = needs_initialization(conn)?;

    if current > SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            current,
            supported: SCHEMA_VERSION,
        }
        .into());
    }

    if current != 0 && needs_init {
        return Err(StorageError::Corruption {
            details: format!("schema version {current} is missing mandatory panes authority table"),
        }
        .into());
    }

    if current == SCHEMA_VERSION {
        validate_current_ft_meta_authority(conn)?;
        validate_checkpoint_snapshot_authority_schema(conn)?;
        validate_clean_checkpoint_receipt_schema(conn)?;
        validate_checkpoint_identity_v38_schema(conn)?;
        validate_pane_scrollback_summary_schema(conn)?;
        validate_session_retained_size_schema(conn)?;
        check_ft_version_compatibility(conn)?;
        // The overwhelmingly common reopen path is read-only and must not
        // acquire SQLite's singleton writer lock. If the optimistic probe
        // finds drift, acquire BEGIN IMMEDIATE and then recheck under that
        // authority before applying the repair atomically; another process may
        // have repaired the row between the probe and lock acquisition.
        if !ft_meta_needs_repair(conn, SCHEMA_VERSION)? {
            return Ok(());
        }
        return run_owned_migration_transaction(conn, "schema metadata", |transaction| {
            if ft_meta_needs_repair(transaction, SCHEMA_VERSION)? {
                ensure_ft_meta(transaction, SCHEMA_VERSION)?;
            }
            Ok(())
        });
    }

    if current != 0 {
        check_ft_version_compatibility(conn)?;
    }

    // Both the fresh-DB case (current == 0 && needs_init: no `panes` table
    // exists) AND the existing-but-unversioned case (current == 0 &&
    // !needs_init: tables present, user_version stamp lost) route through
    // the same path. They share the same fix-up sequence, just with
    // `repair_existing_v0_tables_before_schema_sql` becoming a no-op
    // when no pre-existing tables are present.
    //
    // Why this matters (ft-7tq4z): the previous fresh-DB branch ran
    // SCHEMA_SQL and stamped user_version=SCHEMA_VERSION directly,
    // skipping run_migrations(). SCHEMA_SQL was frozen against the
    // schema at the time it was last regenerated; any table or
    // index added since then via a migration (e.g. v24's
    // policy_denied_audit) was missing on fresh DBs, even though
    // user_version claimed to be at HEAD. The fix is to make the
    // truly-fresh path go through the same `repair → SCHEMA_SQL →
    // run_migrations(0) → ensure_ft_meta` sequence the v0-with-tables
    // path already uses; `repair_existing_v0_tables_before_schema_sql`
    // gates each repair on `table_exists(...)`, so it is a no-op on a
    // genuinely fresh DB. The migration plan is wrapped in a single
    // BEGIN IMMEDIATE / COMMIT (ft-k542h, c06b230a follow-up) so a
    // crash mid-init never leaves half-applied state.
    if current == 0 {
        run_v0_init_in_transaction(conn, needs_init)?;
        return Ok(());
    }
    // `needs_init` is intentionally consulted only via the path above;
    // a future caller that wants to discriminate "really fresh" vs
    // "existing v0" should call `needs_initialization` directly.
    let _ = needs_init;

    // Apply pending migrations for existing databases (version > 0)
    run_migrations(conn, current)?;

    run_owned_migration_transaction(conn, "schema metadata", |transaction| {
        ensure_ft_meta(transaction, SCHEMA_VERSION)
    })
}

pub(crate) fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| StorageError::Database(e.to_string()))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| StorageError::Database(e.to_string()))?;

    while let Some(row) = rows
        .next()
        .map_err(|e| StorageError::Database(e.to_string()))?
    {
        let name: String = row
            .get(1)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        if name == column {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Validate the bounded scrollback projection without rescanning retained
/// segment history. Schema objects and the one-row-per-pane relationship are
/// bounded by schema/pane cardinality; trigger-maintained counters remain the
/// runtime authority.
fn validate_pane_scrollback_summary_schema(conn: &Connection) -> Result<()> {
    let table_sql =
        load_schema_object_sql(conn, "table", "pane_scrollback_summary")?.ok_or_else(|| {
            StorageError::Corruption {
                details: "schema v39 is missing pane_scrollback_summary".to_string(),
            }
        })?;
    let compact_table_sql = compact_schema_sql(&table_sql);
    for required in [
        "pane_id INTEGER PRIMARY KEY",
        "REFERENCES panes(pane_id) ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED",
        "typeof(retained_segment_count) = 'integer' AND retained_segment_count >= 0",
        "first_seq IS NULL OR (typeof(first_seq) = 'integer' AND first_seq >= 0)",
        "last_seq IS NULL OR (typeof(last_seq) = 'integer' AND last_seq >= 0)",
        "first_captured_at IS NULL OR (typeof(first_captured_at) = 'integer' AND first_captured_at >= 0)",
        "last_captured_at IS NULL OR (typeof(last_captured_at) = 'integer' AND last_captured_at >= 0)",
        "retained_segment_count = 0",
        "retained_segment_count > 0",
        "first_seq <= last_seq",
        "first_captured_at <= last_captured_at",
    ] {
        if !compact_table_sql.contains(&compact_schema_sql(required)) {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v39 pane_scrollback_summary is missing invariant {required}"
                ),
            }
            .into());
        }
    }
    for column in [
        "pane_id",
        "retained_segment_count",
        "first_seq",
        "last_seq",
        "first_captured_at",
        "last_captured_at",
    ] {
        if !table_has_column(conn, "pane_scrollback_summary", column)? {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v39 pane_scrollback_summary is missing required column {column}"
                ),
            }
            .into());
        }
    }
    for (trigger, required_action) in [
        (
            "pane_scrollback_summary_panes_ai",
            "INSERT INTO pane_scrollback_summary",
        ),
        (
            "pane_scrollback_summary_panes_ad",
            "DELETE FROM pane_scrollback_summary",
        ),
        (
            "pane_scrollback_summary_bi",
            "scrollback summary requires a persisted pane",
        ),
        (
            "pane_scrollback_summary_bd",
            "live pane scrollback summary is permanent",
        ),
        (
            "pane_scrollback_summary_pane_id_bu",
            "scrollback summary pane identity is immutable",
        ),
        (
            "output_segments_scrollback_summary_bi",
            "invalid output segment metadata or missing scrollback summary",
        ),
        (
            "output_segments_scrollback_summary_ai",
            "retained_segment_count = retained_segment_count + 1",
        ),
        (
            "output_segments_scrollback_summary_bd",
            "missing scrollback summary during output retention",
        ),
        (
            "output_segments_scrollback_summary_ad",
            "retained_segment_count = retained_segment_count - 1",
        ),
        (
            "output_segments_scrollback_metadata_bu",
            "output segment scrollback metadata is immutable",
        ),
    ] {
        let trigger_sql = load_schema_object_sql(conn, "trigger", trigger)?.ok_or_else(|| {
            StorageError::Corruption {
                details: format!("schema v39 is missing required trigger {trigger}"),
            }
        })?;
        if !compact_schema_sql(&trigger_sql).contains(&compact_schema_sql(required_action)) {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v39 trigger {trigger} is missing required action {required_action}"
                ),
            }
            .into());
        }
    }

    let missing_summary: Option<i64> = conn
        .query_row(
            "SELECT panes.pane_id
             FROM panes
             LEFT JOIN pane_scrollback_summary
               ON pane_scrollback_summary.pane_id = panes.pane_id
             WHERE pane_scrollback_summary.pane_id IS NULL
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if let Some(pane_id) = missing_summary {
        return Err(StorageError::Corruption {
            details: format!("persisted pane {pane_id} is missing its scrollback summary"),
        }
        .into());
    }

    let orphan_summary: Option<i64> = conn
        .query_row(
            "SELECT pane_scrollback_summary.pane_id
             FROM pane_scrollback_summary
             LEFT JOIN panes ON panes.pane_id = pane_scrollback_summary.pane_id
             WHERE panes.pane_id IS NULL
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if let Some(pane_id) = orphan_summary {
        return Err(StorageError::Corruption {
            details: format!("scrollback summary {pane_id} has no persisted pane"),
        }
        .into());
    }
    Ok(())
}

const SESSION_RETAINED_SIZE_V40_BEGIN: &str = "-- FT_SESSION_RETAINED_SIZE_V40_BEGIN";
const SESSION_RETAINED_SIZE_V40_END: &str = "-- FT_SESSION_RETAINED_SIZE_V40_END";

/// Return the single canonical schema-v40 retained-size DDL section.
///
/// The marker cardinality checks make test fixtures and the migration path use
/// the same fail-closed authority instead of maintaining a second trigger copy.
///
/// # Errors
///
/// Returns a corruption error if the embedded workspace schema does not contain
/// exactly one ordered begin/end marker pair.
pub fn session_retained_size_schema_sql() -> Result<&'static str> {
    if SCHEMA_SQL
        .match_indices(SESSION_RETAINED_SIZE_V40_BEGIN)
        .count()
        != 1
        || SCHEMA_SQL
            .match_indices(SESSION_RETAINED_SIZE_V40_END)
            .count()
            != 1
    {
        return Err(StorageError::Corruption {
            details: "SCHEMA_SQL must contain exactly one v40 retained-size marker pair"
                .to_string(),
        }
        .into());
    }
    let start = SCHEMA_SQL
        .find(SESSION_RETAINED_SIZE_V40_BEGIN)
        .ok_or_else(|| StorageError::Corruption {
            details: "SCHEMA_SQL is missing the v40 retained-size begin marker".to_string(),
        })?
        .checked_add(SESSION_RETAINED_SIZE_V40_BEGIN.len())
        .ok_or_else(|| StorageError::Corruption {
            details: "SCHEMA_SQL v40 retained-size marker offset overflow".to_string(),
        })?;
    let end = SCHEMA_SQL
        .find(SESSION_RETAINED_SIZE_V40_END)
        .ok_or_else(|| StorageError::Corruption {
            details: "SCHEMA_SQL is missing the v40 retained-size end marker".to_string(),
        })?;
    if start >= end {
        return Err(StorageError::Corruption {
            details: "SCHEMA_SQL v40 retained-size markers are reversed or empty".to_string(),
        }
        .into());
    }
    Ok(&SCHEMA_SQL[start..end])
}

fn canonical_session_retained_size_object_sql(
    declaration_kind: &'static str,
    object_name: &'static str,
) -> Result<&'static str> {
    let schema_sql = session_retained_size_schema_sql()?;
    let declaration = format!("CREATE {declaration_kind} IF NOT EXISTS {object_name}");
    let mut matches = schema_sql.match_indices(&declaration);
    let Some((start, _)) = matches.next() else {
        return Err(StorageError::Corruption {
            details: format!("SCHEMA_SQL v40 is missing canonical object {object_name}"),
        }
        .into());
    };
    if matches.next().is_some() {
        return Err(StorageError::Corruption {
            details: format!(
                "SCHEMA_SQL v40 declares canonical object {object_name} more than once"
            ),
        }
        .into());
    }

    let tail = &schema_sql[start..];
    let terminator = if declaration_kind == "TRIGGER" {
        "\nEND;"
    } else {
        ";"
    };
    let end = tail
        .find(terminator)
        .and_then(|offset| offset.checked_add(terminator.len()))
        .ok_or_else(|| StorageError::Corruption {
            details: format!("SCHEMA_SQL v40 canonical object {object_name} is unterminated"),
        })?;
    Ok(&tail[..end])
}

fn ensure_session_retained_size_schema(conn: &Connection) -> Result<()> {
    let sql = session_retained_size_schema_sql()?;
    conn.execute_batch(sql).map_err(|error| {
        StorageError::MigrationFailed(format!(
            "session retained-size schema installation failed: {error}"
        ))
    })?;
    validate_session_retained_size_schema(conn)
}

/// Validate the O(sessions) retained-byte authority without walking checkpoint
/// or pane history. Full row-by-row recomputation is deliberately owned by the
/// infrequent cleanup transaction, where a mismatch fails closed before any
/// retention deletion.
fn validate_session_retained_size_schema(conn: &Connection) -> Result<()> {
    for (object_type, declaration_kind, object_name) in [
        ("view", "VIEW", "session_retained_size_recomputed"),
        ("table", "TABLE", "session_retained_size"),
        ("trigger", "TRIGGER", "session_retained_size_bi"),
        ("trigger", "TRIGGER", "session_retained_size_bd"),
        ("trigger", "TRIGGER", "session_retained_size_session_id_bu"),
        ("trigger", "TRIGGER", "mux_sessions_retained_size_ai"),
        ("trigger", "TRIGGER", "mux_sessions_retained_size_au"),
        ("trigger", "TRIGGER", "mux_sessions_retained_size_ad"),
        ("trigger", "TRIGGER", "session_checkpoints_retained_size_ai"),
        ("trigger", "TRIGGER", "session_checkpoints_retained_size_au"),
        ("trigger", "TRIGGER", "session_checkpoints_retained_size_bd"),
        ("trigger", "TRIGGER", "mux_pane_state_retained_size_ai"),
        ("trigger", "TRIGGER", "mux_pane_state_retained_size_au"),
        ("trigger", "TRIGGER", "mux_pane_state_retained_size_ad"),
        (
            "trigger",
            "TRIGGER",
            "restore_attempt_lifecycle_retained_size_ai",
        ),
        (
            "trigger",
            "TRIGGER",
            "restore_attempt_lifecycle_retained_size_au",
        ),
        (
            "trigger",
            "TRIGGER",
            "restore_attempt_lifecycle_retained_size_ad",
        ),
    ] {
        let actual = load_schema_object_sql(conn, object_type, object_name)?.ok_or_else(|| {
            StorageError::Corruption {
                details: format!("schema v40 is missing required {object_type} {object_name}"),
            }
        })?;
        let canonical = canonical_session_retained_size_object_sql(declaration_kind, object_name)?;
        if compact_schema_sql(&actual) != compact_schema_sql(canonical) {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v40 {object_type} {object_name} differs from its canonical definition"
                ),
            }
            .into());
        }
    }

    let view_sql = load_schema_object_sql(conn, "view", "session_retained_size_recomputed")?
        .ok_or_else(|| StorageError::Corruption {
            details: "schema v40 is missing session_retained_size_recomputed".to_string(),
        })?;
    let compact_view_sql = compact_schema_sql(&view_sql);
    for required in [
        "length(CAST(s.session_id AS BLOB))",
        "FROM session_checkpoints c WHERE c.session_id = s.session_id",
        "FROM mux_pane_state p INNER JOIN session_checkpoints c ON c.id = p.checkpoint_id",
        "FROM restore_attempt_lifecycle r WHERE r.session_id = s.session_id",
        "AS session_row_bytes",
        "AS checkpoint_row_bytes",
        "AS pane_state_row_bytes",
        "AS restore_lifecycle_row_bytes",
    ] {
        if !compact_view_sql.contains(&compact_schema_sql(required)) {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v40 retained-size recomputation view is missing contract {required}"
                ),
            }
            .into());
        }
    }

    let table_sql =
        load_schema_object_sql(conn, "table", "session_retained_size")?.ok_or_else(|| {
            StorageError::Corruption {
                details: "schema v40 is missing session_retained_size".to_string(),
            }
        })?;
    let compact_table_sql = compact_schema_sql(&table_sql);
    for required in [
        "session_id TEXT PRIMARY KEY",
        "REFERENCES mux_sessions(session_id) ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED",
        "typeof(session_row_bytes) = 'integer' AND session_row_bytes >= 0",
        "typeof(checkpoint_row_bytes) = 'integer' AND checkpoint_row_bytes >= 0",
        "typeof(pane_state_row_bytes) = 'integer' AND pane_state_row_bytes >= 0",
        "typeof(restore_lifecycle_row_bytes) = 'integer' AND restore_lifecycle_row_bytes >= 0",
        "session_row_bytes + checkpoint_row_bytes + pane_state_row_bytes + restore_lifecycle_row_bytes",
        "typeof(retained_bytes) = 'integer' AND retained_bytes >= 0",
    ] {
        if !compact_table_sql.contains(&compact_schema_sql(required)) {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v40 session_retained_size is missing invariant {required}"
                ),
            }
            .into());
        }
    }
    for column in [
        "session_id",
        "session_row_bytes",
        "checkpoint_row_bytes",
        "pane_state_row_bytes",
        "restore_lifecycle_row_bytes",
    ] {
        if !table_has_column(conn, "session_retained_size", column)? {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v40 session_retained_size is missing required column {column}"
                ),
            }
            .into());
        }
    }

    for (trigger, required_action) in [
        (
            "session_retained_size_bi",
            "session retained-size row requires a persisted session",
        ),
        (
            "session_retained_size_bd",
            "live session retained-size authority is permanent",
        ),
        (
            "session_retained_size_session_id_bu",
            "session retained-size identity is immutable",
        ),
        (
            "mux_sessions_retained_size_ai",
            "INSERT INTO session_retained_size",
        ),
        ("mux_sessions_retained_size_au", "SET session_row_bytes ="),
        (
            "mux_sessions_retained_size_ad",
            "DELETE FROM session_retained_size",
        ),
        (
            "session_checkpoints_retained_size_ai",
            "checkpoint_row_bytes = checkpoint_row_bytes + 8",
        ),
        (
            "session_checkpoints_retained_size_au",
            "checkpoint retained-size identity is immutable",
        ),
        (
            "session_checkpoints_retained_size_bd",
            "pane_state_row_bytes = pane_state_row_bytes - COALESCE",
        ),
        (
            "mux_pane_state_retained_size_ai",
            "pane_state_row_bytes = pane_state_row_bytes + 8 + 8 + 8",
        ),
        (
            "mux_pane_state_retained_size_au",
            "pane-state retained-size identity is immutable",
        ),
        (
            "mux_pane_state_retained_size_ad",
            "pane_state_row_bytes = pane_state_row_bytes -",
        ),
        (
            "restore_attempt_lifecycle_retained_size_ai",
            "restore_lifecycle_row_bytes = restore_lifecycle_row_bytes + 8",
        ),
        (
            "restore_attempt_lifecycle_retained_size_au",
            "restore-lifecycle retained-size identity is immutable",
        ),
        (
            "restore_attempt_lifecycle_retained_size_ad",
            "restore_lifecycle_row_bytes = restore_lifecycle_row_bytes -",
        ),
    ] {
        let trigger_sql = load_schema_object_sql(conn, "trigger", trigger)?.ok_or_else(|| {
            StorageError::Corruption {
                details: format!("schema v40 is missing required trigger {trigger}"),
            }
        })?;
        if !compact_schema_sql(&trigger_sql).contains(&compact_schema_sql(required_action)) {
            return Err(StorageError::Corruption {
                details: format!(
                    "schema v40 trigger {trigger} is missing required action {required_action}"
                ),
            }
            .into());
        }
    }

    let missing_summary: Option<String> = conn
        .query_row(
            "SELECT s.session_id
             FROM mux_sessions s
             LEFT JOIN session_retained_size z ON z.session_id = s.session_id
             WHERE z.session_id IS NULL
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if missing_summary.is_some() {
        return Err(StorageError::Corruption {
            details: "a persisted session is missing retained-size authority".to_string(),
        }
        .into());
    }
    let orphan_summary: Option<String> = conn
        .query_row(
            "SELECT z.session_id
             FROM session_retained_size z
             LEFT JOIN mux_sessions s ON s.session_id = z.session_id
             WHERE s.session_id IS NULL
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if orphan_summary.is_some() {
        return Err(StorageError::Corruption {
            details: "a retained-size authority row has no persisted session".to_string(),
        }
        .into());
    }
    Ok(())
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
    Ok(count > 0)
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    sql: &str,
    context: &str,
) -> Result<()> {
    if !table_exists(conn, table)? || table_has_column(conn, table, column)? {
        return Ok(());
    }

    conn.execute_batch(sql).map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to add {column} to {table} during {context}: {e}"
        ))
    })?;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SqliteColumnDescriptor {
    cid: i64,
    name: String,
    declared_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key: bool,
}

fn load_column_descriptor(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<Option<SqliteColumnDescriptor>> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let mut rows = statement
        .query([])
        .map_err(|error| StorageError::Database(error.to_string()))?;

    while let Some(row) = rows
        .next()
        .map_err(|error| StorageError::Database(error.to_string()))?
    {
        let name: String = row
            .get(1)
            .map_err(|error| StorageError::Database(error.to_string()))?;
        if name == column {
            return Ok(Some(SqliteColumnDescriptor {
                cid: row
                    .get(0)
                    .map_err(|error| StorageError::Database(error.to_string()))?,
                name,
                declared_type: row
                    .get(2)
                    .map_err(|error| StorageError::Database(error.to_string()))?,
                not_null: row
                    .get::<_, i64>(3)
                    .map_err(|error| StorageError::Database(error.to_string()))?
                    != 0,
                default_value: row
                    .get(4)
                    .map_err(|error| StorageError::Database(error.to_string()))?,
                primary_key: row
                    .get::<_, i64>(5)
                    .map_err(|error| StorageError::Database(error.to_string()))?
                    != 0,
            }));
        }
    }

    Ok(None)
}

fn require_exact_column_descriptor(
    conn: &Connection,
    table: &str,
    expected: &SqliteColumnDescriptor,
    context: &str,
) -> Result<()> {
    let actual = load_column_descriptor(conn, table, &expected.name)?;
    if actual.as_ref() == Some(expected) {
        return Ok(());
    }

    Err(StorageError::Corruption {
        details: format!(
            "{context}: non-canonical {table}.{} descriptor: expected {expected:?}, found {actual:?}",
            expected.name
        ),
    }
    .into())
}

fn require_exact_table_column_count(
    conn: &Connection,
    table: &str,
    expected: usize,
    context: &str,
) -> Result<()> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let mut rows = statement
        .query([])
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let mut actual = 0_usize;
    while rows
        .next()
        .map_err(|error| StorageError::Database(error.to_string()))?
        .is_some()
    {
        actual = actual.saturating_add(1);
    }
    if actual == expected {
        return Ok(());
    }
    Err(StorageError::Corruption {
        details: format!(
            "{context}: non-canonical {table} column count: expected {expected}, found {actual}"
        ),
    }
    .into())
}

fn compact_schema_sql(sql: &str) -> String {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
        Backtick,
        Bracket,
    }

    let mut compact = String::with_capacity(sql.len());
    let mut quote = None;
    let mut characters = sql.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some(Quote::Single) => {
                compact.push(character);
                if character == '\'' {
                    if characters.peek() == Some(&'\'') {
                        compact.push(characters.next().expect("peeked escaped quote"));
                    } else {
                        quote = None;
                    }
                }
            }
            Some(Quote::Double) => {
                compact.push(character);
                if character == '"' {
                    if characters.peek() == Some(&'"') {
                        compact.push(characters.next().expect("peeked escaped identifier quote"));
                    } else {
                        quote = None;
                    }
                }
            }
            Some(Quote::Backtick) => {
                compact.push(character);
                if character == '`' {
                    if characters.peek() == Some(&'`') {
                        compact.push(characters.next().expect("peeked escaped backtick"));
                    } else {
                        quote = None;
                    }
                }
            }
            Some(Quote::Bracket) => {
                compact.push(character);
                if character == ']' {
                    quote = None;
                }
            }
            None => match character {
                '\'' => {
                    compact.push(character);
                    quote = Some(Quote::Single);
                }
                '"' => {
                    compact.push(character);
                    quote = Some(Quote::Double);
                }
                '`' => {
                    compact.push(character);
                    quote = Some(Quote::Backtick);
                }
                '[' => {
                    compact.push(character);
                    quote = Some(Quote::Bracket);
                }
                character if character.is_ascii_whitespace() => {}
                character => compact.push(character.to_ascii_lowercase()),
            },
        }
    }
    while compact.ends_with(';') {
        compact.pop();
    }
    if let Some(suffix) = compact.strip_prefix("createuniqueindexifnotexists") {
        return format!("createuniqueindex{suffix}");
    }
    if let Some(suffix) = compact.strip_prefix("createindexifnotexists") {
        return format!("createindex{suffix}");
    }
    if let Some(suffix) = compact.strip_prefix("createtableifnotexists") {
        return format!("createtable{suffix}");
    }
    if let Some(suffix) = compact.strip_prefix("createviewifnotexists") {
        return format!("createview{suffix}");
    }
    if let Some(suffix) = compact.strip_prefix("createtriggerifnotexists") {
        return format!("createtrigger{suffix}");
    }
    compact
}

fn load_schema_object_sql(
    conn: &Connection,
    object_type: &str,
    name: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
        params![object_type, name],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
    .map_err(|error| StorageError::Database(error.to_string()).into())
}

fn require_table_sql_fragment(
    conn: &Connection,
    table: &str,
    expected_fragment: &str,
    context: &str,
) -> Result<()> {
    let sql =
        load_schema_object_sql(conn, "table", table)?.ok_or_else(|| StorageError::Corruption {
            details: format!("{context}: missing required table {table}"),
        })?;
    let compact_sql = compact_schema_sql(&sql);
    let compact_fragment = compact_schema_sql(expected_fragment);
    if compact_sql.contains(&compact_fragment) {
        return Ok(());
    }

    Err(StorageError::Corruption {
        details: format!("{context}: {table} is missing canonical constraint {expected_fragment}"),
    }
    .into())
}

fn validate_exact_index(
    conn: &Connection,
    name: &str,
    canonical_sql: &str,
    context: &str,
) -> Result<()> {
    let actual = load_schema_object_sql(conn, "index", name)?;
    if actual
        .as_deref()
        .is_some_and(|sql| compact_schema_sql(sql) == compact_schema_sql(canonical_sql))
    {
        return Ok(());
    }

    Err(StorageError::Corruption {
        details: format!(
            "{context}: non-canonical index {name}: expected {}, found {actual:?}",
            compact_schema_sql(canonical_sql)
        ),
    }
    .into())
}

fn ensure_exact_index(
    conn: &Connection,
    name: &str,
    drop_sql: &str,
    canonical_sql: &str,
    context: &str,
) -> Result<()> {
    let actual = load_schema_object_sql(conn, "index", name)?;
    if actual
        .as_deref()
        .is_some_and(|sql| compact_schema_sql(sql) == compact_schema_sql(canonical_sql))
    {
        return Ok(());
    }

    conn.execute_batch(drop_sql)
        .and_then(|()| conn.execute_batch(canonical_sql))
        .map_err(|error| {
            StorageError::MigrationFailed(format!(
                "Failed to replace non-canonical index {name} during {context}: {error}"
            ))
        })?;
    validate_exact_index(conn, name, canonical_sql, context)
}

fn ensure_audit_actions_decision_context(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "audit_actions",
        "decision_context",
        "ALTER TABLE audit_actions ADD COLUMN decision_context TEXT;",
        "migration v2",
    )
}

fn ensure_panes_pane_uuid(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "panes",
        "pane_uuid",
        "ALTER TABLE panes ADD COLUMN pane_uuid TEXT;",
        "migration v3",
    )
}

/// ft-7h5da.1.5: `redaction_catalog_version` is present in the current
/// `SCHEMA_SQL` baseline, so on a fresh DB the column already exists by the time
/// `run_migrations(0)` reaches v29. Guard the ALTER (like the other
/// SCHEMA_SQL-coexisting column migrations) so v29 is a no-op on fresh DBs and a
/// real add only on pre-v29 upgrades — without it, fresh init fails with
/// "duplicate column name: redaction_catalog_version".
fn ensure_output_segments_redaction_catalog_version(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "output_segments",
        "redaction_catalog_version",
        "ALTER TABLE output_segments ADD COLUMN redaction_catalog_version TEXT;",
        "migration v29",
    )
}

/// ft-7h5da.2.3: `zone_type` is present in the current `SCHEMA_SQL` baseline,
/// so guard the v31 ALTER for the same fresh-DB/upgrade split as v29.
fn ensure_output_segments_zone_type(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "output_segments",
        "zone_type",
        "ALTER TABLE output_segments ADD COLUMN zone_type TEXT;",
        "migration v31",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_segments_zone_type ON output_segments(zone_type);",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to create idx_segments_zone_type during migration v31: {e}"
        ))
    })?;
    Ok(())
}

/// `SCHEMA_SQL` carries the v33 delivery-lease columns for fresh databases, so
/// each ALTER must be guarded for the same fresh/init versus upgrade split as
/// the v29 and v31 column migrations. Keeping the three additions in one
/// migration transaction prevents a partially upgraded lease state.
fn ensure_event_delivery_lease_schema(conn: &Connection) -> Result<()> {
    for (column, sql) in [
        (
            "delivery_lease_token",
            "ALTER TABLE events ADD COLUMN delivery_lease_token TEXT;",
        ),
        (
            "delivery_lease_acquired_at",
            "ALTER TABLE events ADD COLUMN delivery_lease_acquired_at INTEGER;",
        ),
        (
            "delivery_lease_expires_at",
            "ALTER TABLE events ADD COLUMN delivery_lease_expires_at INTEGER;",
        ),
    ] {
        add_column_if_missing(conn, "events", column, sql, "migration v33")?;
    }
    Ok(())
}

/// Schema v36: make checkpoint purpose and topology immutable row-local
/// authority instead of inferring both from overloaded type/session columns.
const CHECKPOINT_ROLE_LATEST_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_latest
     ON session_checkpoints(session_id, checkpoint_role, checkpoint_at DESC, id DESC);";

const CHECKPOINT_GLOBAL_LATEST_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_checkpoints_global_latest
     ON session_checkpoints(checkpoint_at DESC, id DESC);";

const CHECKPOINT_GLOBAL_SNAPSHOT_LATEST_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_checkpoints_global_snapshot_latest
     ON session_checkpoints(checkpoint_at DESC, id DESC)
     WHERE checkpoint_role = 'snapshot';";

const CHECKPOINT_ROLE_CAUSAL_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_causal
     ON session_checkpoints(session_id, checkpoint_role, id DESC);";

const CHECKPOINT_RESTORE_INTENT_OUTCOME_INDEX_SQL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_restore_intent_outcome
     ON session_checkpoints(restore_intent_checkpoint_id)
     WHERE restore_intent_checkpoint_id IS NOT NULL;";

const CLEAN_CHECKPOINT_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_mux_sessions_clean_checkpoint
     ON mux_sessions(clean_checkpoint_id);";

fn validate_checkpoint_snapshot_authority_columns(conn: &Connection) -> Result<()> {
    require_exact_column_descriptor(
        conn,
        "session_checkpoints",
        &SqliteColumnDescriptor {
            cid: 8,
            name: "checkpoint_role".to_string(),
            declared_type: "TEXT".to_string(),
            not_null: true,
            default_value: Some("'snapshot'".to_string()),
            primary_key: false,
        },
        "checkpoint snapshot authority",
    )?;
    require_exact_column_descriptor(
        conn,
        "session_checkpoints",
        &SqliteColumnDescriptor {
            cid: 9,
            name: "topology_json".to_string(),
            declared_type: "TEXT".to_string(),
            not_null: false,
            default_value: None,
            primary_key: false,
        },
        "checkpoint snapshot authority",
    )?;
    let table_sql =
        load_schema_object_sql(conn, "table", "session_checkpoints")?.ok_or_else(|| {
            StorageError::Corruption {
                details:
                    "checkpoint snapshot authority: missing required table session_checkpoints"
                        .to_string(),
            }
        })?;
    let compact_table_sql = compact_schema_sql(&table_sql);
    let v36_role_check = compact_schema_sql(
        "checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
         CHECK(checkpoint_role IN ('snapshot','restore_receipt'))",
    );
    let v38_role_check = compact_schema_sql(
        "checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
         CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt'))",
    );
    if !compact_table_sql.contains(&v36_role_check) && !compact_table_sql.contains(&v38_role_check)
    {
        return Err(StorageError::Corruption {
            details: "checkpoint snapshot authority: session_checkpoints is missing a canonical checkpoint-role constraint"
                .to_string(),
        }
        .into());
    }
    Ok(())
}

fn validate_checkpoint_snapshot_authority_schema(conn: &Connection) -> Result<()> {
    validate_checkpoint_snapshot_authority_columns(conn)?;
    for (name, canonical_sql) in [
        (
            "idx_checkpoints_session_role_latest",
            CHECKPOINT_ROLE_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_latest",
            CHECKPOINT_GLOBAL_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_snapshot_latest",
            CHECKPOINT_GLOBAL_SNAPSHOT_LATEST_INDEX_SQL,
        ),
    ] {
        validate_exact_index(conn, name, canonical_sql, "checkpoint snapshot authority")?;
    }
    Ok(())
}

fn ensure_checkpoint_snapshot_authority_indexes(conn: &Connection, context: &str) -> Result<()> {
    for (name, drop_sql, canonical_sql) in [
        (
            "idx_checkpoints_session_role_latest",
            "DROP INDEX IF EXISTS idx_checkpoints_session_role_latest;",
            CHECKPOINT_ROLE_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_latest",
            "DROP INDEX IF EXISTS idx_checkpoints_global_latest;",
            CHECKPOINT_GLOBAL_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_snapshot_latest",
            "DROP INDEX IF EXISTS idx_checkpoints_global_snapshot_latest;",
            CHECKPOINT_GLOBAL_SNAPSHOT_LATEST_INDEX_SQL,
        ),
    ] {
        ensure_exact_index(conn, name, drop_sql, canonical_sql, context)?;
    }
    Ok(())
}

fn ensure_checkpoint_snapshot_authority_schema(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "session_checkpoints",
        "checkpoint_role",
        "ALTER TABLE session_checkpoints
         ADD COLUMN checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
         CHECK(checkpoint_role IN ('snapshot','restore_receipt'));",
        "migration v36",
    )?;
    add_column_if_missing(
        conn,
        "session_checkpoints",
        "topology_json",
        "ALTER TABLE session_checkpoints ADD COLUMN topology_json TEXT;",
        "migration v36",
    )?;
    validate_checkpoint_snapshot_authority_columns(conn)?;
    // Establish the per-session role path before the historical backfill. On
    // large session databases it prevents the correlated latest lookup below
    // from degenerating into a checkpoint-table scan per row.
    ensure_exact_index(
        conn,
        "idx_checkpoints_session_role_latest",
        "DROP INDEX IF EXISTS idx_checkpoints_session_role_latest;",
        CHECKPOINT_ROLE_LATEST_INDEX_SQL,
        "migration v36",
    )?;

    // Classify only the exact legacy receipt shape that the pre-v36 writer
    // produced and loader accepted. An empty startup snapshot alone is
    // ambiguous: missing pane rows or metadata can also be corruption, and a
    // migration must never mint
    // clean authority from that negative evidence. JSON1 is already a schema
    // dependency elsewhere in this database.
    conn.execute_batch(
        "UPDATE session_checkpoints AS checkpoint
         SET checkpoint_role = 'restore_receipt'
         WHERE checkpoint.checkpoint_role = 'snapshot'
           AND checkpoint.id > 0
           AND checkpoint.checkpoint_at >= 0
           AND checkpoint.checkpoint_type = 'startup'
           AND checkpoint.topology_json IS NULL
           AND checkpoint.total_bytes = 0
           AND checkpoint.metadata_json IS NOT NULL
           AND json_valid(checkpoint.metadata_json)
           AND json_type(checkpoint.metadata_json) = 'object'
           AND json_type(checkpoint.metadata_json, '$.old_to_new') = 'object'
           AND (
               SELECT COUNT(*)
               FROM json_each(checkpoint.metadata_json)
           ) = 1
           AND checkpoint.pane_count = (
               SELECT COUNT(*)
               FROM json_each(checkpoint.metadata_json, '$.old_to_new')
           )
           AND (
               SELECT COUNT(DISTINCT mapping.key)
               FROM json_each(checkpoint.metadata_json, '$.old_to_new') AS mapping
           ) = checkpoint.pane_count
           AND NOT EXISTS (
               SELECT 1
               FROM json_each(checkpoint.metadata_json, '$.old_to_new') AS mapping
               WHERE mapping.key = ''
                  OR mapping.key GLOB '*[^0-9]*'
                  OR (length(mapping.key) > 1 AND substr(mapping.key, 1, 1) = '0')
                  OR length(mapping.key) > 19
                  OR (length(mapping.key) = 19
                      AND mapping.key > '9223372036854775807')
                  OR mapping.type <> 'integer'
                  OR mapping.value < 0
           )
           AND (
               SELECT COUNT(DISTINCT mapping.value)
               FROM json_each(checkpoint.metadata_json, '$.old_to_new') AS mapping
           ) = checkpoint.pane_count
           AND (
               checkpoint.state_hash = 'restore'
               OR (
                   length(checkpoint.state_hash) = 16
                   AND checkpoint.state_hash NOT GLOB '*[^0-9a-f]*'
               )
           )
           AND NOT EXISTS (
               SELECT 1
               FROM mux_pane_state AS pane_state
               WHERE pane_state.checkpoint_id = checkpoint.id
           );

         UPDATE session_checkpoints AS checkpoint
         SET topology_json = (
             SELECT session.topology_json
             FROM mux_sessions AS session
             WHERE session.session_id = checkpoint.session_id
         )
         WHERE checkpoint.checkpoint_role = 'snapshot'
           AND checkpoint.topology_json IS NULL
           AND checkpoint.id = (
               SELECT newest.id
               FROM session_checkpoints AS newest
               WHERE newest.session_id = checkpoint.session_id
                 AND newest.checkpoint_role = 'snapshot'
               ORDER BY newest.id DESC
               LIMIT 1
           );",
    )
    .map_err(|error| {
        StorageError::MigrationFailed(format!(
            "Failed to establish checkpoint snapshot authority during migration v36: {error}"
        ))
    })?;
    // Build the global paths only after role classification so the partial
    // snapshot index is populated once with its final row set. The all-role
    // index bounds robot pagination/latest, while the partial index bounds
    // snapshot-only latest/list; neither can replace the other without filtered
    // or deep scans.
    ensure_checkpoint_snapshot_authority_indexes(conn, "migration v36")?;
    validate_checkpoint_snapshot_authority_schema(conn)
}

/// Schema v37: retain the exact checkpoint identity that authorizes a clean
/// session claim. A migration cannot prove which historical checkpoint
/// authorized a legacy boolean, so legacy clean rows fail safe to unclean
/// rather than minting a new receipt identity after the fact.
fn validate_clean_checkpoint_receipt_column_and_foreign_key(conn: &Connection) -> Result<()> {
    require_exact_column_descriptor(
        conn,
        "mux_sessions",
        &SqliteColumnDescriptor {
            cid: 8,
            name: "clean_checkpoint_id".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: false,
            default_value: None,
            primary_key: false,
        },
        "clean checkpoint receipt authority",
    )?;

    let mut statement = conn
        .prepare("PRAGMA foreign_key_list(mux_sessions)")
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let foreign_keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })
        .map_err(|error| StorageError::Database(error.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let expected_foreign_key = (
        "session_checkpoints".to_string(),
        "clean_checkpoint_id".to_string(),
        "id".to_string(),
        "NO ACTION".to_string(),
        "SET NULL".to_string(),
        "NONE".to_string(),
    );
    if foreign_keys.len() != 1 || foreign_keys.first() != Some(&expected_foreign_key) {
        return Err(StorageError::Corruption {
            details: format!(
                "clean checkpoint receipt authority: non-canonical mux_sessions foreign keys: {foreign_keys:?}"
            ),
        }
        .into());
    }

    Ok(())
}

fn validate_clean_checkpoint_receipt_schema(conn: &Connection) -> Result<()> {
    validate_clean_checkpoint_receipt_column_and_foreign_key(conn)?;
    validate_exact_index(
        conn,
        "idx_mux_sessions_clean_checkpoint",
        CLEAN_CHECKPOINT_INDEX_SQL,
        "clean checkpoint receipt authority",
    )
}

fn ensure_clean_checkpoint_receipt_schema(conn: &Connection) -> Result<()> {
    // A database entering v37 from v36 will not replay its migration step.
    // Validate the immutable columns/constraint and idempotently repair only
    // the exact v36 indexes inside this transaction; do not rescan/reclassify a
    // large checkpoint history that v36 already migrated.
    validate_checkpoint_snapshot_authority_columns(conn)?;
    ensure_checkpoint_snapshot_authority_indexes(conn, "migration v37 prerequisite")?;
    validate_checkpoint_snapshot_authority_schema(conn)?;
    add_column_if_missing(
        conn,
        "mux_sessions",
        "clean_checkpoint_id",
        "ALTER TABLE mux_sessions
         ADD COLUMN clean_checkpoint_id INTEGER
         REFERENCES session_checkpoints(id) ON DELETE SET NULL;",
        "migration v37",
    )?;
    validate_clean_checkpoint_receipt_column_and_foreign_key(conn)?;

    conn.execute_batch(
        "UPDATE mux_sessions
         SET shutdown_clean = 0,
             clean_checkpoint_id = NULL
         WHERE shutdown_clean <> 1
           AND (shutdown_clean <> 0 OR clean_checkpoint_id IS NOT NULL);

         UPDATE mux_sessions AS session
         SET shutdown_clean = 0,
             clean_checkpoint_id = NULL
         WHERE session.shutdown_clean = 1
           AND NOT EXISTS (
               SELECT 1
               FROM session_checkpoints AS checkpoint
               WHERE checkpoint.id = session.clean_checkpoint_id
                 AND checkpoint.session_id = session.session_id
                 AND checkpoint.checkpoint_role IN ('snapshot', 'restore_receipt')
                 AND checkpoint.id = (
                     SELECT latest.id
                     FROM session_checkpoints AS latest
                     WHERE latest.session_id = session.session_id
                     ORDER BY latest.id DESC
                     LIMIT 1
                 )
           );",
    )
    .map_err(|error| {
        StorageError::MigrationFailed(format!(
            "Failed to establish clean-checkpoint receipt authority during migration v37: {error}"
        ))
    })?;
    ensure_exact_index(
        conn,
        "idx_mux_sessions_clean_checkpoint",
        "DROP INDEX IF EXISTS idx_mux_sessions_clean_checkpoint;",
        CLEAN_CHECKPOINT_INDEX_SQL,
        "migration v37",
    )?;
    validate_clean_checkpoint_receipt_schema(conn)
}

/// Schema v38: make checkpoint row identities durable across deletion and
/// reserve an explicit, one-outcome-per-intent restore-attempt relation.
const CHECKPOINT_SESSION_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS idx_checkpoints_session
     ON session_checkpoints(session_id, checkpoint_at);";

const CHECKPOINT_V38_TABLE_SQL: &str = r"
CREATE TABLE session_checkpoints_v38 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    checkpoint_at INTEGER NOT NULL,
    checkpoint_type TEXT NOT NULL
        CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
    state_hash TEXT NOT NULL,
    pane_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,
    metadata_json TEXT,
    checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
        CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt')),
    topology_json TEXT,
    restore_intent_checkpoint_id INTEGER
        -- Name the post-cutover authority directly. Foreign-key enforcement is
        -- suspended for the rebuild, so this avoids depending on SQLite's
        -- ALTER TABLE self-reference rewrite or legacy_alter_table setting.
        REFERENCES session_checkpoints(id) ON DELETE CASCADE,
    CHECK(checkpoint_role = 'restore_receipt' OR restore_intent_checkpoint_id IS NULL)
);";

const RESTORE_ATTEMPT_LIFECYCLE_TABLE_SQL: &str = r"
CREATE TABLE restore_attempt_lifecycle (
    intent_checkpoint_id INTEGER PRIMARY KEY
        REFERENCES session_checkpoints(id)
        ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
    session_id TEXT NOT NULL
        REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    source_checkpoint_id INTEGER NOT NULL,
    outcome_checkpoint_id INTEGER
        REFERENCES session_checkpoints(id) ON DELETE SET NULL,
    status TEXT NOT NULL
        CHECK(status IN ('intent','outcome_complete','resolved','reconciliation_required')),
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    CHECK(intent_checkpoint_id <> source_checkpoint_id),
    CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> intent_checkpoint_id),
    CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> source_checkpoint_id),
    CHECK(created_at >= 0),
    CHECK(resolved_at IS NULL OR resolved_at >= created_at),
    CHECK(
        (status = 'intent'
            AND outcome_checkpoint_id IS NULL
            AND resolved_at IS NULL)
        OR (status = 'outcome_complete'
            AND outcome_checkpoint_id IS NOT NULL
            AND resolved_at IS NULL)
        OR (status = 'reconciliation_required'
            AND resolved_at IS NULL)
        OR (status = 'resolved'
            AND resolved_at IS NOT NULL)
    )
);";

const RESTORE_ATTEMPT_SESSION_STATUS_INDEX_SQL: &str =
    "CREATE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_session_status
     ON restore_attempt_lifecycle(session_id, status, intent_checkpoint_id);";

const RESTORE_ATTEMPT_OUTCOME_INDEX_SQL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_outcome
     ON restore_attempt_lifecycle(outcome_checkpoint_id)
     WHERE outcome_checkpoint_id IS NOT NULL;";

fn ensure_no_checkpoint_authority_triggers(conn: &Connection, context: &str) -> Result<()> {
    let trigger_count: i64 = conn
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'trigger'
                    AND tbl_name COLLATE NOCASE
                        IN ('mux_sessions', 'session_checkpoints', 'mux_pane_state',
                            'restore_attempt_lifecycle'))
               + (SELECT COUNT(*) FROM sqlite_temp_schema
                  WHERE type = 'trigger'
                    AND tbl_name COLLATE NOCASE
                        IN ('mux_sessions', 'session_checkpoints', 'mux_pane_state',
                            'restore_attempt_lifecycle'))",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if trigger_count == 0 {
        return Ok(());
    }
    Err(StorageError::Corruption {
        details: format!(
            "{context}: refuses to rebuild checkpoint authority with {trigger_count} unaudited trigger(s)"
        ),
    }
    .into())
}

fn checkpoint_v38_table_has_identity_surface(conn: &Connection) -> Result<bool> {
    let Some(sql) = load_schema_object_sql(conn, "table", "session_checkpoints")? else {
        return Ok(false);
    };
    Ok(
        compact_schema_sql(&sql).contains("integerprimarykeyautoincrement")
            || table_has_column(conn, "session_checkpoints", "restore_intent_checkpoint_id")?,
    )
}

type CheckpointForeignKey = (String, String, String, String, String, String);

fn checkpoint_foreign_keys(conn: &Connection, table: &str) -> Result<Vec<CheckpointForeignKey>> {
    let mut statement = conn
        .prepare(&format!("PRAGMA foreign_key_list({table})"))
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let mut foreign_keys = statement
        .query_map([], |row| {
            Ok((
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .map_err(|error| StorageError::Database(error.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    foreign_keys.sort();
    Ok(foreign_keys)
}

fn ensure_no_foreign_key_violations(conn: &Connection, context: &str) -> Result<()> {
    let mut statement = conn
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let first_violation = statement
        .query_row([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .optional()
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if let Some((table, rowid, parent, foreign_key_id)) = first_violation {
        return Err(StorageError::Corruption {
            details: format!(
                "{context}: foreign-key violation table={table} rowid={rowid:?} parent={parent} foreign_key_id={foreign_key_id}"
            ),
        }
        .into());
    }
    Ok(())
}

fn validate_restore_attempt_lifecycle_v38_schema(conn: &Connection) -> Result<()> {
    require_exact_table_column_count(
        conn,
        "restore_attempt_lifecycle",
        7,
        "restore attempt lifecycle v38",
    )?;
    for expected in [
        SqliteColumnDescriptor {
            cid: 0,
            name: "intent_checkpoint_id".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: false,
            default_value: None,
            primary_key: true,
        },
        SqliteColumnDescriptor {
            cid: 1,
            name: "session_id".to_string(),
            declared_type: "TEXT".to_string(),
            not_null: true,
            default_value: None,
            primary_key: false,
        },
        SqliteColumnDescriptor {
            cid: 2,
            name: "source_checkpoint_id".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: true,
            default_value: None,
            primary_key: false,
        },
        SqliteColumnDescriptor {
            cid: 3,
            name: "outcome_checkpoint_id".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: false,
            default_value: None,
            primary_key: false,
        },
        SqliteColumnDescriptor {
            cid: 4,
            name: "status".to_string(),
            declared_type: "TEXT".to_string(),
            not_null: true,
            default_value: None,
            primary_key: false,
        },
        SqliteColumnDescriptor {
            cid: 5,
            name: "created_at".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: true,
            default_value: None,
            primary_key: false,
        },
        SqliteColumnDescriptor {
            cid: 6,
            name: "resolved_at".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: false,
            default_value: None,
            primary_key: false,
        },
    ] {
        require_exact_column_descriptor(
            conn,
            "restore_attempt_lifecycle",
            &expected,
            "restore attempt lifecycle v38",
        )?;
    }
    for fragment in [
        "status TEXT NOT NULL
         CHECK(status IN ('intent','outcome_complete','resolved','reconciliation_required'))",
        "intent_checkpoint_id INTEGER PRIMARY KEY
         REFERENCES session_checkpoints(id)
         ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED",
        "CHECK(created_at >= 0)",
        "CHECK(resolved_at IS NULL OR resolved_at >= created_at)",
        "CHECK(intent_checkpoint_id <> source_checkpoint_id)",
        "CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> intent_checkpoint_id)",
        "CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> source_checkpoint_id)",
        "status = 'intent'
         AND outcome_checkpoint_id IS NULL
         AND resolved_at IS NULL",
        "status = 'outcome_complete'
         AND outcome_checkpoint_id IS NOT NULL
         AND resolved_at IS NULL",
        "status = 'reconciliation_required'
         AND resolved_at IS NULL",
        "status = 'resolved'
         AND resolved_at IS NOT NULL",
    ] {
        require_table_sql_fragment(
            conn,
            "restore_attempt_lifecycle",
            fragment,
            "restore attempt lifecycle v38",
        )?;
    }
    for (name, canonical_sql) in [
        (
            "idx_restore_attempt_lifecycle_session_status",
            RESTORE_ATTEMPT_SESSION_STATUS_INDEX_SQL,
        ),
        (
            "idx_restore_attempt_lifecycle_outcome",
            RESTORE_ATTEMPT_OUTCOME_INDEX_SQL,
        ),
    ] {
        validate_exact_index(conn, name, canonical_sql, "restore attempt lifecycle v38")?;
    }

    let expected_foreign_keys = vec![
        (
            "mux_sessions".to_string(),
            "session_id".to_string(),
            "session_id".to_string(),
            "NO ACTION".to_string(),
            "CASCADE".to_string(),
            "NONE".to_string(),
        ),
        (
            "session_checkpoints".to_string(),
            "intent_checkpoint_id".to_string(),
            "id".to_string(),
            "NO ACTION".to_string(),
            "NO ACTION".to_string(),
            "NONE".to_string(),
        ),
        (
            "session_checkpoints".to_string(),
            "outcome_checkpoint_id".to_string(),
            "id".to_string(),
            "NO ACTION".to_string(),
            "SET NULL".to_string(),
            "NONE".to_string(),
        ),
    ];
    let actual_foreign_keys = checkpoint_foreign_keys(conn, "restore_attempt_lifecycle")?;
    if actual_foreign_keys != expected_foreign_keys {
        return Err(StorageError::Corruption {
            details: format!(
                "restore attempt lifecycle v38: non-canonical foreign keys: {actual_foreign_keys:?}"
            ),
        }
        .into());
    }
    Ok(())
}

fn validate_checkpoint_identity_v38_schema(conn: &Connection) -> Result<()> {
    require_exact_table_column_count(conn, "session_checkpoints", 11, "checkpoint identity v38")?;
    require_exact_column_descriptor(
        conn,
        "session_checkpoints",
        &SqliteColumnDescriptor {
            cid: 0,
            name: "id".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: false,
            default_value: None,
            primary_key: true,
        },
        "checkpoint identity v38",
    )?;
    require_exact_column_descriptor(
        conn,
        "session_checkpoints",
        &SqliteColumnDescriptor {
            cid: 10,
            name: "restore_intent_checkpoint_id".to_string(),
            declared_type: "INTEGER".to_string(),
            not_null: false,
            default_value: None,
            primary_key: false,
        },
        "checkpoint identity v38",
    )?;
    require_table_sql_fragment(
        conn,
        "session_checkpoints",
        "id INTEGER PRIMARY KEY AUTOINCREMENT",
        "checkpoint identity v38",
    )?;
    require_table_sql_fragment(
        conn,
        "session_checkpoints",
        "checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
         CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt'))",
        "checkpoint identity v38",
    )?;
    require_table_sql_fragment(
        conn,
        "session_checkpoints",
        "CHECK(checkpoint_role = 'restore_receipt' OR restore_intent_checkpoint_id IS NULL)",
        "checkpoint identity v38",
    )?;

    for (name, canonical_sql) in [
        ("idx_checkpoints_session", CHECKPOINT_SESSION_INDEX_SQL),
        (
            "idx_checkpoints_session_role_latest",
            CHECKPOINT_ROLE_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_session_role_causal",
            CHECKPOINT_ROLE_CAUSAL_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_latest",
            CHECKPOINT_GLOBAL_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_snapshot_latest",
            CHECKPOINT_GLOBAL_SNAPSHOT_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_restore_intent_outcome",
            CHECKPOINT_RESTORE_INTENT_OUTCOME_INDEX_SQL,
        ),
    ] {
        validate_exact_index(conn, name, canonical_sql, "checkpoint identity v38")?;
    }

    let expected_checkpoint_foreign_keys = vec![
        (
            "mux_sessions".to_string(),
            "session_id".to_string(),
            "session_id".to_string(),
            "NO ACTION".to_string(),
            "CASCADE".to_string(),
            "NONE".to_string(),
        ),
        (
            "session_checkpoints".to_string(),
            "restore_intent_checkpoint_id".to_string(),
            "id".to_string(),
            "NO ACTION".to_string(),
            "CASCADE".to_string(),
            "NONE".to_string(),
        ),
    ];
    let actual_checkpoint_foreign_keys = checkpoint_foreign_keys(conn, "session_checkpoints")?;
    if actual_checkpoint_foreign_keys != expected_checkpoint_foreign_keys {
        return Err(StorageError::Corruption {
            details: format!(
                "checkpoint identity v38: non-canonical session_checkpoints foreign keys: {actual_checkpoint_foreign_keys:?}"
            ),
        }
        .into());
    }
    validate_clean_checkpoint_receipt_column_and_foreign_key(conn)?;
    let pane_foreign_keys = checkpoint_foreign_keys(conn, "mux_pane_state")?;
    let expected_pane_foreign_keys = vec![(
        "session_checkpoints".to_string(),
        "checkpoint_id".to_string(),
        "id".to_string(),
        "NO ACTION".to_string(),
        "CASCADE".to_string(),
        "NONE".to_string(),
    )];
    if pane_foreign_keys != expected_pane_foreign_keys {
        return Err(StorageError::Corruption {
            details: format!(
                "checkpoint identity v38: non-canonical mux_pane_state foreign keys: {pane_foreign_keys:?}"
            ),
        }
        .into());
    }
    validate_restore_attempt_lifecycle_v38_schema(conn)?;
    ensure_no_checkpoint_authority_triggers(conn, "checkpoint identity v38")
}

fn ensure_checkpoint_identity_v38_schema(conn: &Connection) -> Result<()> {
    if checkpoint_v38_table_has_identity_surface(conn)? {
        return validate_checkpoint_identity_v38_schema(conn);
    }

    validate_checkpoint_snapshot_authority_schema(conn)?;
    validate_clean_checkpoint_receipt_schema(conn)?;
    ensure_no_checkpoint_authority_triggers(conn, "migration v38")?;

    let staging_exists: i64 = conn
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM sqlite_schema
                  WHERE name IN (
                      'session_checkpoints_v38',
                      'restore_attempt_lifecycle'
                  ))
               + (SELECT COUNT(*) FROM sqlite_temp_schema
                  WHERE name IN (
                      'session_checkpoints_v38',
                      'restore_attempt_lifecycle'
                  ))",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if staging_exists != 0 {
        return Err(StorageError::Corruption {
            details: "migration v38: a v38 staging/lifecycle object already exists".to_string(),
        }
        .into());
    }

    let (source_count, source_max_id): (i64, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(*), MAX(id) FROM session_checkpoints",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;

    conn.execute_batch(CHECKPOINT_V38_TABLE_SQL)
        .and_then(|()| {
            conn.execute_batch(
                "INSERT INTO session_checkpoints_v38 (
                     id, session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, metadata_json, checkpoint_role,
                     topology_json, restore_intent_checkpoint_id
                 )
                 SELECT id, session_id, checkpoint_at, checkpoint_type, state_hash,
                        pane_count, total_bytes, metadata_json, checkpoint_role,
                        topology_json, NULL
                 FROM session_checkpoints
                 ORDER BY id ASC;",
            )
        })
        .map_err(|error| {
            StorageError::MigrationFailed(format!(
                "migration v38: failed to create and populate checkpoint authority table: {error}"
            ))
        })?;

    let copied_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints_v38", [], |row| {
            row.get(0)
        })
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let source_minus_copy: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (
                 SELECT id, session_id, checkpoint_at, checkpoint_type, state_hash,
                        pane_count, total_bytes, metadata_json, checkpoint_role, topology_json
                 FROM session_checkpoints
                 EXCEPT
                 SELECT id, session_id, checkpoint_at, checkpoint_type, state_hash,
                        pane_count, total_bytes, metadata_json, checkpoint_role, topology_json
                 FROM session_checkpoints_v38
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    let copy_minus_source: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (
                 SELECT id, session_id, checkpoint_at, checkpoint_type, state_hash,
                        pane_count, total_bytes, metadata_json, checkpoint_role, topology_json
                 FROM session_checkpoints_v38
                 EXCEPT
                 SELECT id, session_id, checkpoint_at, checkpoint_type, state_hash,
                        pane_count, total_bytes, metadata_json, checkpoint_role, topology_json
                 FROM session_checkpoints
             )",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if copied_count != source_count || source_minus_copy != 0 || copy_minus_source != 0 {
        return Err(StorageError::Corruption {
            details: format!(
                "migration v38: checkpoint copy mismatch source_count={source_count} copied_count={copied_count} source_minus_copy={source_minus_copy} copy_minus_source={copy_minus_source}"
            ),
        }
        .into());
    }

    conn.execute_batch(
        "DROP TABLE session_checkpoints;
         ALTER TABLE session_checkpoints_v38 RENAME TO session_checkpoints;",
    )
    .map_err(|error| {
        StorageError::MigrationFailed(format!(
            "migration v38: failed checkpoint authority cutover: {error}"
        ))
    })?;
    for (name, drop_sql, canonical_sql) in [
        (
            "idx_checkpoints_session",
            "DROP INDEX IF EXISTS idx_checkpoints_session;",
            CHECKPOINT_SESSION_INDEX_SQL,
        ),
        (
            "idx_checkpoints_session_role_latest",
            "DROP INDEX IF EXISTS idx_checkpoints_session_role_latest;",
            CHECKPOINT_ROLE_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_session_role_causal",
            "DROP INDEX IF EXISTS idx_checkpoints_session_role_causal;",
            CHECKPOINT_ROLE_CAUSAL_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_latest",
            "DROP INDEX IF EXISTS idx_checkpoints_global_latest;",
            CHECKPOINT_GLOBAL_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_global_snapshot_latest",
            "DROP INDEX IF EXISTS idx_checkpoints_global_snapshot_latest;",
            CHECKPOINT_GLOBAL_SNAPSHOT_LATEST_INDEX_SQL,
        ),
        (
            "idx_checkpoints_restore_intent_outcome",
            "DROP INDEX IF EXISTS idx_checkpoints_restore_intent_outcome;",
            CHECKPOINT_RESTORE_INTENT_OUTCOME_INDEX_SQL,
        ),
    ] {
        ensure_exact_index(conn, name, drop_sql, canonical_sql, "migration v38")?;
    }

    conn.execute_batch(RESTORE_ATTEMPT_LIFECYCLE_TABLE_SQL)
        .map_err(|error| {
            StorageError::MigrationFailed(format!(
                "migration v38: failed to create restore-attempt lifecycle: {error}"
            ))
        })?;
    let legacy_intent_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM session_checkpoints AS intent
             WHERE intent.checkpoint_role = 'restore_receipt'
               AND json_valid(intent.metadata_json)
               AND json_extract(
                       intent.metadata_json,
                       '$.restore_attempt.phase'
                   ) = 'intent'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    conn.execute_batch(
        "INSERT INTO restore_attempt_lifecycle (
             intent_checkpoint_id, session_id, source_checkpoint_id,
             outcome_checkpoint_id, status, created_at, resolved_at
         )
         SELECT intent.id,
                intent.session_id,
                source.id,
                NULL,
                'reconciliation_required',
                intent.checkpoint_at,
                NULL
         FROM session_checkpoints AS intent
         INNER JOIN session_checkpoints AS source
             ON source.id = json_extract(
                    intent.metadata_json,
                    '$.restore_attempt.source_checkpoint_id'
                )
            AND source.session_id = intent.session_id
            AND source.checkpoint_role = 'snapshot'
            AND source.checkpoint_at = json_extract(
                    intent.metadata_json,
                    '$.restore_attempt.source_checkpoint_at'
                )
            AND source.state_hash = json_extract(
                    intent.metadata_json,
                    '$.restore_attempt.source_state_hash'
                )
         WHERE intent.checkpoint_role = 'restore_receipt'
           AND intent.checkpoint_type = 'startup'
           AND intent.pane_count = 0
           AND intent.total_bytes = 0
           AND intent.topology_json IS NULL
           AND length(intent.state_hash) = 69
           AND substr(intent.state_hash, 1, 5) = 'rst2:'
           AND substr(intent.state_hash, 6) NOT GLOB '*[^0-9a-f]*'
           AND json_valid(intent.metadata_json)
           AND json_extract(
                   intent.metadata_json,
                   '$.restore_attempt.phase'
               ) = 'intent'
           AND json_type(
                   intent.metadata_json,
                   '$.restore_attempt.source_checkpoint_id'
               ) = 'integer'
           AND json_type(
                   intent.metadata_json,
                   '$.restore_attempt.source_checkpoint_at'
               ) = 'integer'
           AND json_extract(
                   intent.metadata_json,
                   '$.restore_attempt.source_checkpoint_role'
               ) = 'snapshot'
           AND json_type(
                   intent.metadata_json,
                   '$.restore_attempt.source_state_hash'
               ) = 'text'
           AND NOT EXISTS (
               SELECT 1
               FROM mux_pane_state AS pane
               WHERE pane.checkpoint_id = intent.id
           );",
    )
    .map_err(|error| {
        StorageError::MigrationFailed(format!(
            "migration v38: failed to backfill restore-attempt lifecycle: {error}"
        ))
    })?;
    let backfilled_intent_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM restore_attempt_lifecycle",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if backfilled_intent_count != legacy_intent_count {
        return Err(StorageError::Corruption {
            details: format!(
                "migration v38: {legacy_intent_count} legacy restore intent(s) found but only {backfilled_intent_count} had a complete same-session source identity"
            ),
        }
        .into());
    }
    for (name, drop_sql, canonical_sql) in [
        (
            "idx_restore_attempt_lifecycle_session_status",
            "DROP INDEX IF EXISTS idx_restore_attempt_lifecycle_session_status;",
            RESTORE_ATTEMPT_SESSION_STATUS_INDEX_SQL,
        ),
        (
            "idx_restore_attempt_lifecycle_outcome",
            "DROP INDEX IF EXISTS idx_restore_attempt_lifecycle_outcome;",
            RESTORE_ATTEMPT_OUTCOME_INDEX_SQL,
        ),
    ] {
        ensure_exact_index(conn, name, drop_sql, canonical_sql, "migration v38")?;
    }

    let staging_sequence_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_sequence
             WHERE name = 'session_checkpoints_v38'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| StorageError::Database(error.to_string()))?;
    if staging_sequence_rows != 0 {
        return Err(StorageError::Corruption {
            details: "migration v38: stale staging sqlite_sequence authority".to_string(),
        }
        .into());
    }
    if let Some(max_id) = source_max_id.filter(|id| *id > 0) {
        let sequence: Option<i64> = conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'session_checkpoints'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| StorageError::Database(error.to_string()))?;
        if sequence.is_none_or(|sequence| sequence < max_id) {
            return Err(StorageError::Corruption {
                details: format!(
                    "migration v38: sqlite_sequence is below preserved checkpoint high-water id {max_id}: {sequence:?}"
                ),
            }
            .into());
        }
    }

    validate_checkpoint_identity_v38_schema(conn)
}

fn ensure_workflow_step_logs_audit_action_id(conn: &Connection) -> Result<()> {
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='workflow_step_logs'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;

    if table_exists == 0 {
        // Will be created via SCHEMA_SQL; nothing to do here.
        return Ok(());
    }

    if table_has_column(conn, "workflow_step_logs", "audit_action_id")? {
        return Ok(());
    }

    conn.execute_batch(
        "ALTER TABLE workflow_step_logs ADD COLUMN audit_action_id INTEGER REFERENCES audit_actions(id) ON DELETE SET NULL;",
    )
    .map_err(|e| StorageError::MigrationFailed(format!("Failed to add audit_action_id to workflow_step_logs: {e}")))?;

    Ok(())
}

fn ensure_workflow_step_log_columns(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "workflow_step_logs")? {
        return Ok(());
    }

    let columns = [
        ("step_id", "TEXT"),
        ("step_kind", "TEXT"),
        ("policy_summary", "TEXT"),
        ("verification_refs", "TEXT"),
        ("error_code", "TEXT"),
    ];

    for (column, column_type) in columns {
        if table_has_column(conn, "workflow_step_logs", column)? {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE workflow_step_logs ADD COLUMN {column} {column_type};"),
            [],
        )
        .map_err(|e| {
            StorageError::MigrationFailed(format!(
                "Failed to add {column} to workflow_step_logs: {e}"
            ))
        })?;
    }

    Ok(())
}

fn ensure_agent_sessions_external_meta(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "agent_sessions",
        "external_meta",
        "ALTER TABLE agent_sessions ADD COLUMN external_meta TEXT;",
        "migration v9",
    )
}

fn ensure_audit_actions_correlation_id(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "audit_actions",
        "correlation_id",
        "ALTER TABLE audit_actions ADD COLUMN correlation_id TEXT;",
        "migration v12",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_audit_actions_correlation ON audit_actions(correlation_id);",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to ensure correlation_id index during migration v12: {e}"
        ))
    })?;

    Ok(())
}

fn ensure_event_triage_schema(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "events",
        "triage_state",
        "ALTER TABLE events ADD COLUMN triage_state TEXT;",
        "migration v18",
    )?;
    add_column_if_missing(
        conn,
        "events",
        "triage_updated_at",
        "ALTER TABLE events ADD COLUMN triage_updated_at INTEGER;",
        "migration v18",
    )?;
    add_column_if_missing(
        conn,
        "events",
        "triage_updated_by",
        "ALTER TABLE events ADD COLUMN triage_updated_by TEXT;",
        "migration v18",
    )?;

    conn.execute_batch(
        r"
        CREATE INDEX IF NOT EXISTS idx_events_triage_state
            ON events(triage_state) WHERE triage_state IS NOT NULL;

        CREATE TABLE IF NOT EXISTS event_labels (
            event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
            label TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            created_by TEXT,
            PRIMARY KEY (event_id, label)
        );

        CREATE INDEX IF NOT EXISTS idx_event_labels_event ON event_labels(event_id);
        CREATE INDEX IF NOT EXISTS idx_event_labels_label ON event_labels(label);

        CREATE TABLE IF NOT EXISTS event_notes (
            event_id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
            note TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            updated_by TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_event_notes_updated_at ON event_notes(updated_at);
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to ensure event triage schema during migration v18: {e}"
        ))
    })?;

    Ok(())
}

fn ensure_approval_tokens_plan_hash_schema(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "approval_tokens",
        "plan_hash",
        "ALTER TABLE approval_tokens ADD COLUMN plan_hash TEXT;",
        "migration v19",
    )?;
    add_column_if_missing(
        conn,
        "approval_tokens",
        "plan_version",
        "ALTER TABLE approval_tokens ADD COLUMN plan_version INTEGER;",
        "migration v19",
    )?;
    add_column_if_missing(
        conn,
        "approval_tokens",
        "risk_summary",
        "ALTER TABLE approval_tokens ADD COLUMN risk_summary TEXT;",
        "migration v19",
    )?;

    conn.execute_batch(
        r"
        CREATE INDEX IF NOT EXISTS idx_approval_tokens_plan_hash
            ON approval_tokens(plan_hash) WHERE plan_hash IS NOT NULL;
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to ensure approval_tokens plan-hash index during migration v19: {e}"
        ))
    })?;

    Ok(())
}

fn ensure_ft_meta_rename_and_session_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS ft_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            schema_version INTEGER NOT NULL,
            min_compatible_ft TEXT NOT NULL,
            created_by_ft TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to ensure ft_meta table during migration v21: {e}"
        ))
    })?;

    if table_exists(conn, "wa_meta")? {
        conn.execute_batch(
            r"
            INSERT OR IGNORE INTO ft_meta (id, schema_version, min_compatible_ft, created_by_ft, created_at)
                SELECT id, schema_version, min_compatible_wa, created_by_wa, created_at
                FROM wa_meta WHERE id = 1;

            DROP TABLE IF EXISTS wa_meta;
            ",
        )
        .map_err(|e| {
            StorageError::MigrationFailed(format!(
                "Failed to migrate wa_meta to ft_meta during migration v21: {e}"
            ))
        })?;
    }

    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS mux_sessions (
            session_id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL,
            last_checkpoint_at INTEGER,
            shutdown_clean INTEGER NOT NULL DEFAULT 0,
            topology_json TEXT NOT NULL,
            window_metadata_json TEXT,
            ft_version TEXT NOT NULL,
            host_id TEXT
        );

        CREATE TABLE IF NOT EXISTS session_checkpoints (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
            checkpoint_at INTEGER NOT NULL,
            checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
            state_hash TEXT NOT NULL,
            pane_count INTEGER NOT NULL,
            total_bytes INTEGER NOT NULL,
            metadata_json TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_checkpoints_session
            ON session_checkpoints(session_id, checkpoint_at);

        CREATE TABLE IF NOT EXISTS mux_pane_state (
            id INTEGER PRIMARY KEY,
            checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
            pane_id INTEGER NOT NULL,
            cwd TEXT,
            command TEXT,
            env_json TEXT,
            terminal_state_json TEXT NOT NULL,
            agent_metadata_json TEXT,
            scrollback_checkpoint_seq INTEGER,
            last_output_at INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
        CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to ensure session persistence tables during migration v21: {e}"
        ))
    })?;

    Ok(())
}

fn create_segment_embeddings_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS segment_embeddings (
            segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
            embedder_id TEXT NOT NULL,
            dimension INTEGER NOT NULL,
            vector BLOB NOT NULL,
            -- epoch MILLISECONDS (schema-wide convention, ft-ayy9x / ft-wi24o):
            -- strftime('%s') is seconds, so scale by 1000 to match schema_ddl.
            embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
            PRIMARY KEY (segment_id, embedder_id)
        );

        CREATE INDEX IF NOT EXISTS idx_segment_embeddings_embedder
            ON segment_embeddings(embedder_id);
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!("Failed to create segment_embeddings table: {e}"))
            .into()
    })
}

pub(crate) fn segment_embeddings_table_is_canonical(conn: &Connection) -> Result<bool> {
    if !table_exists(conn, "segment_embeddings")? {
        return Ok(false);
    }

    let mut has_segment_id = false;
    let mut has_embedder_id = false;
    let mut has_dimension = false;
    let mut has_vector = false;
    let mut has_embedded_at = false;
    let mut segment_pk: Option<i64> = None;
    let mut embedder_pk: Option<i64> = None;

    let mut stmt = conn
        .prepare("PRAGMA table_info(segment_embeddings)")
        .map_err(|e| StorageError::MigrationFailed(format!("PRAGMA table_info failed: {e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| StorageError::MigrationFailed(format!("Failed to query table_info: {e}")))?;

    while let Some(row) = rows
        .next()
        .map_err(|e| StorageError::MigrationFailed(format!("Failed to read table_info row: {e}")))?
    {
        let name: String = row.get(1).map_err(|e| {
            StorageError::MigrationFailed(format!("table_info name read failed: {e}"))
        })?;
        let pk_pos: i64 = row.get(5).map_err(|e| {
            StorageError::MigrationFailed(format!("table_info pk read failed: {e}"))
        })?;

        match name.as_str() {
            "segment_id" => {
                has_segment_id = true;
                segment_pk = Some(pk_pos);
            }
            "embedder_id" => {
                has_embedder_id = true;
                embedder_pk = Some(pk_pos);
            }
            "dimension" => has_dimension = true,
            "vector" => has_vector = true,
            "embedded_at" => has_embedded_at = true,
            _ => {}
        }
    }

    let has_expected_columns =
        has_segment_id && has_embedder_id && has_dimension && has_vector && has_embedded_at;
    let has_expected_pk = segment_pk == Some(1) && embedder_pk == Some(2);
    if !has_expected_columns || !has_expected_pk {
        return Ok(false);
    }

    let mut fk_stmt = conn
        .prepare("PRAGMA foreign_key_list(segment_embeddings)")
        .map_err(|e| {
            StorageError::MigrationFailed(format!("PRAGMA foreign_key_list failed: {e}"))
        })?;
    let mut fk_rows = fk_stmt.query([]).map_err(|e| {
        StorageError::MigrationFailed(format!("Failed to query foreign_key_list: {e}"))
    })?;

    while let Some(row) = fk_rows.next().map_err(|e| {
        StorageError::MigrationFailed(format!("Failed to read foreign_key_list row: {e}"))
    })? {
        let table: String = row.get(2).map_err(|e| {
            StorageError::MigrationFailed(format!("foreign_key_list table read failed: {e}"))
        })?;
        let from_col: String = row.get(3).map_err(|e| {
            StorageError::MigrationFailed(format!("foreign_key_list from read failed: {e}"))
        })?;
        let to_col: String = row.get(4).map_err(|e| {
            StorageError::MigrationFailed(format!("foreign_key_list to read failed: {e}"))
        })?;
        let on_delete: String = row.get(6).map_err(|e| {
            StorageError::MigrationFailed(format!("foreign_key_list on_delete read failed: {e}"))
        })?;

        if table == "output_segments"
            && from_col == "segment_id"
            && to_col == "id"
            && on_delete.eq_ignore_ascii_case("CASCADE")
        {
            return Ok(true);
        }
    }

    Ok(false)
}

fn ensure_segment_embeddings_schema(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "segment_embeddings")? {
        return create_segment_embeddings_table(conn);
    }

    if segment_embeddings_table_is_canonical(conn)? {
        return conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segment_embeddings_embedder ON segment_embeddings(embedder_id);",
        )
        .map_err(|e| {
            StorageError::MigrationFailed(format!(
                "Failed to ensure segment_embeddings index exists: {e}"
            ))
            .into()
        });
    }

    conn.execute_batch(
        r"
        DROP TABLE IF EXISTS segment_embeddings_legacy;
        ALTER TABLE segment_embeddings RENAME TO segment_embeddings_legacy;
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to stage legacy segment_embeddings table for rebuild: {e}"
        ))
    })?;

    create_segment_embeddings_table(conn)?;

    conn.execute(
        r"
        INSERT OR REPLACE INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at)
        SELECT legacy.segment_id,
               legacy.embedder_id,
               legacy.dimension,
               legacy.vector,
               COALESCE(legacy.embedded_at, strftime('%s', 'now'))
        FROM segment_embeddings_legacy legacy
        INNER JOIN output_segments seg ON seg.id = legacy.segment_id
        WHERE legacy.embedder_id IS NOT NULL
        ",
        [],
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to migrate legacy segment_embeddings rows: {e}"
        ))
    })?;

    conn.execute_batch("DROP TABLE IF EXISTS segment_embeddings_legacy;")
        .map_err(|e| {
            StorageError::MigrationFailed(format!(
                "Failed to drop legacy segment_embeddings table: {e}"
            ))
        })?;

    Ok(())
}

/// ft-wi24o (migration v32): repair `segment_embeddings.embedded_at`'s column
/// DEFAULT to epoch milliseconds on databases upgraded through v22/v23.
///
/// Migration v30 normalized existing row *values* (seconds → ms) but left the
/// column DEFAULT as the v22/v23 seconds expression `strftime('%s','now')`, so
/// an INSERT that omits `embedded_at` still stored seconds — reintroducing the
/// 1000× unit trap v30 closes and diverging from a fresh DB's ms default.
///
/// SQLite cannot `ALTER COLUMN ... SET DEFAULT`, so the table is rebuilt when
/// (and only when) the current default is still the seconds expression. Rows
/// (already ms after v30) and the embedder index are preserved; the
/// `INNER JOIN output_segments` drops any FK-orphan rows. No-op on fresh and
/// already-ms databases, so it is safe to replay on the `run_migrations(0)`
/// fresh-init path.
fn ensure_segment_embeddings_embedded_at_default_ms(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "segment_embeddings")? {
        return Ok(());
    }
    if segment_embeddings_embedded_at_default_is_ms(conn)? {
        return Ok(());
    }

    conn.execute_batch(
        r"
        DROP TABLE IF EXISTS segment_embeddings_legacy;
        ALTER TABLE segment_embeddings RENAME TO segment_embeddings_legacy;
        ",
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "v32: failed to stage legacy segment_embeddings for default rebuild: {e}"
        ))
    })?;

    create_segment_embeddings_table(conn)?;

    conn.execute(
        r"
        INSERT OR REPLACE INTO segment_embeddings
            (segment_id, embedder_id, dimension, vector, embedded_at)
        SELECT legacy.segment_id,
               legacy.embedder_id,
               legacy.dimension,
               legacy.vector,
               legacy.embedded_at
        FROM segment_embeddings_legacy legacy
        INNER JOIN output_segments seg ON seg.id = legacy.segment_id
        WHERE legacy.embedder_id IS NOT NULL
        ",
        [],
    )
    .map_err(|e| {
        StorageError::MigrationFailed(format!(
            "v32: failed to copy segment_embeddings rows during default rebuild: {e}"
        ))
    })?;

    conn.execute_batch("DROP TABLE IF EXISTS segment_embeddings_legacy;")
        .map_err(|e| {
            StorageError::MigrationFailed(format!(
                "v32: failed to drop legacy segment_embeddings after default rebuild: {e}"
            ))
        })?;

    Ok(())
}

/// Whether `segment_embeddings.embedded_at` currently defaults to epoch ms
/// (`strftime('%s','now') * 1000`) rather than the legacy seconds expression.
/// Reads `PRAGMA table_info`'s `dflt_value` (column index 4); the seconds
/// default has no `1000` literal, the ms default does.
fn segment_embeddings_embedded_at_default_is_ms(conn: &Connection) -> Result<bool> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(segment_embeddings)")
        .map_err(|e| StorageError::MigrationFailed(format!("PRAGMA table_info failed: {e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| StorageError::MigrationFailed(format!("table_info query failed: {e}")))?;
    while let Some(row) = rows
        .next()
        .map_err(|e| StorageError::MigrationFailed(format!("table_info row read failed: {e}")))?
    {
        let name: String = row.get(1).map_err(|e| {
            StorageError::MigrationFailed(format!("table_info name read failed: {e}"))
        })?;
        if name == "embedded_at" {
            let dflt: Option<String> = row.get(4).map_err(|e| {
                StorageError::MigrationFailed(format!("table_info dflt read failed: {e}"))
            })?;
            return Ok(dflt.as_deref().is_some_and(|d| d.contains("1000")));
        }
    }
    // `embedded_at` column absent — treat as not-ms so the caller can rebuild.
    Ok(false)
}

/// Steps inside `run_v0_init_in_transaction`, exposed for fault-injection
/// in tests so we can simulate a crash between any two steps and assert that
/// the outer transaction rolls back cleanly. Production builds optimize this
/// to a no-op.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum V0InitStep {
    RepairComplete,
    SchemaSqlApplied,
    MigrationsApplied,
}

// ft-0eby0: the fault plan is THREAD-LOCAL, not process-global. Every
// fresh-DB open in the whole --lib binary routes through
// run_v0_init_in_transaction (the ft-7tq4z fresh-path merge), so a
// process-global fault armed by one test was consumed — and cleared —
// by whichever unrelated StorageHandle::new landed first on another
// thread, failing that test AND starving the arming test. The arming
// test drives initialize_schema directly on its own thread, so a
// thread-local cannot escape; consumers on writer threads see -1.
#[cfg(test)]
thread_local! {
    static V0_INIT_FAULT_AT: std::cell::Cell<i8> = const { std::cell::Cell::new(-1) };
}

#[cfg(test)]
pub(crate) fn set_v0_init_fault_for_test(step: Option<V0InitStep>) {
    let value: i8 = match step {
        None => -1,
        Some(V0InitStep::RepairComplete) => 0,
        Some(V0InitStep::SchemaSqlApplied) => 1,
        Some(V0InitStep::MigrationsApplied) => 2,
    };
    V0_INIT_FAULT_AT.with(|cell| cell.set(value));
}

#[cfg(test)]
fn check_v0_init_fault(step: V0InitStep) -> Result<()> {
    let active = V0_INIT_FAULT_AT.with(std::cell::Cell::get);
    let target = match step {
        V0InitStep::RepairComplete => 0,
        V0InitStep::SchemaSqlApplied => 1,
        V0InitStep::MigrationsApplied => 2,
    };
    if active == target {
        // Clear the fault so the test can re-enter the helper and verify
        // success on the second try. Mirrors how a real crash would not
        // re-fire on a subsequent open.
        V0_INIT_FAULT_AT.with(|cell| cell.set(-1));
        return Err(StorageError::MigrationFailed(format!(
            "ft-k542h fault injection: forced failure at {step:?}"
        ))
        .into());
    }
    Ok(())
}

/// Split `SCHEMA_SQL` into (PRAGMA preamble, table/index body). PRAGMA
/// statements like `journal_mode = WAL` and `synchronous = NORMAL` are
/// rejected by SQLite when run inside a transaction; the v0-init wrapper
/// applies the preamble before BEGIN and the body inside.
pub(crate) fn split_schema_sql_pragmas() -> (String, String) {
    let mut preamble = String::new();
    let mut body = String::new();
    let mut skipping_v40_retained_size = false;
    for line in SCHEMA_SQL.lines() {
        let trimmed = line.trim();
        if trimmed == SESSION_RETAINED_SIZE_V40_BEGIN {
            skipping_v40_retained_size = true;
            continue;
        }
        if trimmed == SESSION_RETAINED_SIZE_V40_END {
            skipping_v40_retained_size = false;
            continue;
        }
        if skipping_v40_retained_size {
            continue;
        }
        if line
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("PRAGMA ")
        {
            preamble.push_str(line);
            preamble.push('\n');
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    (preamble, body)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MigrationTransactionFault {
    Begin,
    MutationPanic,
    Commit,
    Rollback,
    ClosureVerification,
    AuthorityVerification,
}

#[cfg(test)]
thread_local! {
    static MIGRATION_TRANSACTION_FAULT: std::cell::Cell<i8> = const { std::cell::Cell::new(-1) };
}

#[cfg(test)]
fn set_migration_transaction_fault_for_test(fault: Option<MigrationTransactionFault>) {
    let value = match fault {
        None => -1,
        Some(MigrationTransactionFault::Begin) => 0,
        Some(MigrationTransactionFault::MutationPanic) => 1,
        Some(MigrationTransactionFault::Commit) => 2,
        Some(MigrationTransactionFault::Rollback) => 3,
        Some(MigrationTransactionFault::ClosureVerification) => 4,
        Some(MigrationTransactionFault::AuthorityVerification) => 5,
    };
    MIGRATION_TRANSACTION_FAULT.with(|cell| cell.set(value));
}

#[cfg(test)]
fn take_migration_transaction_fault(fault: MigrationTransactionFault) -> bool {
    let expected = match fault {
        MigrationTransactionFault::Begin => 0,
        MigrationTransactionFault::MutationPanic => 1,
        MigrationTransactionFault::Commit => 2,
        MigrationTransactionFault::Rollback => 3,
        MigrationTransactionFault::ClosureVerification => 4,
        MigrationTransactionFault::AuthorityVerification => 5,
    };
    MIGRATION_TRANSACTION_FAULT.with(|cell| {
        if cell.get() == expected {
            cell.set(-1);
            true
        } else {
            false
        }
    })
}

#[cfg(not(test))]
const fn take_migration_begin_fault() -> bool {
    false
}

#[cfg(test)]
fn take_migration_begin_fault() -> bool {
    take_migration_transaction_fault(MigrationTransactionFault::Begin)
}

#[cfg(not(test))]
const fn take_migration_mutation_panic_fault() -> bool {
    false
}

#[cfg(test)]
fn take_migration_mutation_panic_fault() -> bool {
    take_migration_transaction_fault(MigrationTransactionFault::MutationPanic)
}

#[cfg(not(test))]
const fn take_migration_commit_fault() -> bool {
    false
}

#[cfg(test)]
fn take_migration_commit_fault() -> bool {
    take_migration_transaction_fault(MigrationTransactionFault::Commit)
}

#[cfg(not(test))]
const fn take_migration_rollback_fault() -> bool {
    false
}

#[cfg(test)]
fn take_migration_rollback_fault() -> bool {
    take_migration_transaction_fault(MigrationTransactionFault::Rollback)
}

#[cfg(not(test))]
const fn take_migration_closure_verification_fault() -> bool {
    false
}

#[cfg(test)]
fn take_migration_closure_verification_fault() -> bool {
    take_migration_transaction_fault(MigrationTransactionFault::ClosureVerification)
}

#[cfg(not(test))]
const fn take_migration_authority_verification_fault() -> bool {
    false
}

#[cfg(test)]
fn take_migration_authority_verification_fault() -> bool {
    take_migration_transaction_fault(MigrationTransactionFault::AuthorityVerification)
}

fn migration_connection_is_query_only(conn: &Connection) -> Result<bool> {
    if take_migration_authority_verification_fault() {
        return Err(poison_migration_connection_epoch(conn));
    }
    match frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| {
            conn.query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
        }),
    ) {
        Ok(Ok(enabled)) => Ok(enabled != 0),
        Ok(Err(_)) | Err(_) => Err(poison_migration_connection_epoch(conn)),
    }
}

fn migration_connection_is_autocommit(conn: &Connection) -> Result<bool> {
    match frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| conn.is_autocommit()),
    ) {
        Ok(is_autocommit) => Ok(is_autocommit),
        Err(_) => Err(poison_migration_connection_epoch(conn)),
    }
}

fn poison_migration_connection_epoch(conn: &Connection) -> crate::error::Error {
    // query_only is connection-local and survives transaction closure. It is a
    // second fail-closed fence in addition to the typed terminal error: even a
    // caller that mistakenly retains this Connection cannot issue another
    // schema or ordinary write. Reopening the database creates a fresh epoch.
    let _ = frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| conn.execute_batch("PRAGMA query_only = ON")),
    );
    StorageError::MigrationEpochPoisoned.into()
}

fn migration_transaction_closed(conn: &Connection) -> Result<bool> {
    if take_migration_closure_verification_fault() {
        return Err(poison_migration_connection_epoch(conn));
    }
    migration_connection_is_autocommit(conn)
}

fn rollback_migration_transaction(
    mut transaction: Transaction<'_>,
    primary: crate::error::Error,
) -> Result<()> {
    if take_migration_rollback_fault() {
        // Deterministically model a rollback API failure without allowing the
        // rusqlite Drop fallback to hide the open-transaction outcome.
        transaction.set_drop_behavior(DropBehavior::Ignore);
        return Err(poison_migration_connection_epoch(&transaction));
    }

    let rollback = frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| transaction.execute_batch("ROLLBACK")),
    );
    if !matches!(rollback, Ok(Ok(()))) {
        transaction.set_drop_behavior(DropBehavior::Ignore);
        return Err(poison_migration_connection_epoch(&transaction));
    }
    // The transaction was closed explicitly. Disable rusqlite's default Drop
    // rollback before any fallible verification so a poisoned verification
    // path cannot issue a second backend control call while unwinding.
    transaction.set_drop_behavior(DropBehavior::Ignore);
    match migration_transaction_closed(&transaction) {
        Ok(true) => Err(primary),
        Ok(false) => Err(poison_migration_connection_epoch(&transaction)),
        Err(error) => Err(error),
    }
}

fn run_owned_migration_transaction(
    conn: &Connection,
    context: &'static str,
    operation: impl FnOnce(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    if !migration_connection_is_autocommit(conn)? || migration_connection_is_query_only(conn)? {
        return Err(poison_migration_connection_epoch(conn));
    }
    if take_migration_begin_fault() {
        return Err(StorageError::MigrationFailed(format!(
            "{context}: failed to begin migration transaction"
        ))
        .into());
    }

    let begin = frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| {
            Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        }),
    );
    let mut transaction = match begin {
        Ok(Ok(transaction)) => transaction,
        Ok(Err(_)) | Err(_) => {
            if migration_connection_is_autocommit(conn)? {
                return Err(StorageError::MigrationFailed(format!(
                    "{context}: failed to begin migration transaction"
                ))
                .into());
            }
            return Err(poison_migration_connection_epoch(conn));
        }
    };

    let mutation = frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| {
            let result = operation(&transaction);
            assert!(
                !take_migration_mutation_panic_fault(),
                "synthetic migration mutation panic"
            );
            result
        }),
    );
    match mutation {
        Err(_panic) => rollback_migration_transaction(
            transaction,
            StorageError::MigrationFailed(format!("{context}: migration mutation panicked")).into(),
        ),
        Ok(Err(_error)) => rollback_migration_transaction(
            transaction,
            StorageError::MigrationFailed(format!("{context}: migration mutation failed")).into(),
        ),
        Ok(Ok(())) => {
            let commit_failed = take_migration_commit_fault()
                || !matches!(
                    frankenterm_sigpipe::catch_recoverable(
                        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
                        std::panic::AssertUnwindSafe(|| transaction.execute_batch("COMMIT")),
                    ),
                    Ok(Ok(()))
                );
            if commit_failed {
                // COMMIT may or may not have closed the transaction. Suppress
                // implicit Drop control before the fallible authority probe;
                // an explicitly open transaction is still rolled back below.
                transaction.set_drop_behavior(DropBehavior::Ignore);
                match migration_connection_is_autocommit(&transaction) {
                    Ok(true) => {
                        return Err(poison_migration_connection_epoch(&transaction));
                    }
                    Ok(false) => {}
                    Err(error) => return Err(error),
                }
                return rollback_migration_transaction(
                    transaction,
                    StorageError::MigrationFailed(format!(
                        "{context}: failed to commit migration transaction"
                    ))
                    .into(),
                );
            }
            // COMMIT succeeded explicitly. Prevent Transaction::drop from
            // issuing a redundant ROLLBACK, including when closure verification
            // itself errors and fences this connection epoch.
            transaction.set_drop_behavior(DropBehavior::Ignore);
            match migration_transaction_closed(&transaction) {
                Ok(true) => Ok(()),
                Ok(false) => Err(poison_migration_connection_epoch(&transaction)),
                Err(error) => Err(error),
            }
        }
    }
}

fn migration_foreign_keys_enabled(conn: &Connection) -> Result<bool> {
    match frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| {
            conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
        }),
    ) {
        Ok(Ok(enabled)) => Ok(enabled != 0),
        Ok(Err(_)) | Err(_) => Err(poison_migration_connection_epoch(conn)),
    }
}

fn set_migration_foreign_keys(conn: &Connection, enabled: bool) -> Result<()> {
    if !migration_connection_is_autocommit(conn)? {
        return Err(poison_migration_connection_epoch(conn));
    }
    let sql = if enabled {
        "PRAGMA foreign_keys = ON"
    } else {
        "PRAGMA foreign_keys = OFF"
    };
    let applied = frankenterm_sigpipe::catch_recoverable(
        frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
        std::panic::AssertUnwindSafe(|| conn.execute_batch(sql)),
    );
    if !matches!(applied, Ok(Ok(()))) || migration_foreign_keys_enabled(conn)? != enabled {
        return Err(poison_migration_connection_epoch(conn));
    }
    Ok(())
}

/// Run a migration transaction that must rebuild a table participating in a
/// circular foreign-key graph. SQLite ignores `PRAGMA foreign_keys` inside a
/// transaction, and dropping the referenced table with enforcement enabled
/// performs implicit deletes. Suspend enforcement before BEGIN, validate the
/// complete graph before COMMIT, and restore the caller's connection setting
/// only after transaction closure is authoritative.
fn run_owned_migration_transaction_with_foreign_keys_suspended(
    conn: &Connection,
    context: &'static str,
    operation: impl FnOnce(&Transaction<'_>) -> Result<()>,
) -> Result<()> {
    if !migration_connection_is_autocommit(conn)? || migration_connection_is_query_only(conn)? {
        return Err(poison_migration_connection_epoch(conn));
    }
    let foreign_keys_were_enabled = migration_foreign_keys_enabled(conn)?;
    set_migration_foreign_keys(conn, false)?;

    let result = run_owned_migration_transaction(conn, context, operation);
    if !migration_connection_is_autocommit(conn)? {
        return Err(poison_migration_connection_epoch(conn));
    }
    set_migration_foreign_keys(conn, foreign_keys_were_enabled)?;
    result
}

/// Atomically run the v0-existing-database init triple (ft-k542h):
/// `repair_existing_v0_tables_before_schema_sql` → `SCHEMA_SQL` →
/// `run_migrations(0)` → `ensure_ft_meta`, all inside one BEGIN IMMEDIATE /
/// COMMIT. ROLLBACK on any error so a crashed init never leaves
/// half-applied repair ALTERs with `user_version` still at 0.
///
/// `SCHEMA_SQL`'s PRAGMA preamble is applied *before* BEGIN because SQLite
/// rejects `journal_mode` / `synchronous` changes inside a transaction.
/// The PRAGMAs are connection-level settings and idempotent, so applying
/// them up-front does not affect atomicity of the table/index work.
fn run_v0_init_in_transaction(conn: &Connection, stamp_fresh_fts_index: bool) -> Result<()> {
    let (pragma_preamble, schema_body) = split_schema_sql_pragmas();

    if !pragma_preamble.trim().is_empty() {
        let applied = frankenterm_sigpipe::catch_recoverable(
            frankenterm_sigpipe::RecoverablePanicSite::StorageWriter,
            std::panic::AssertUnwindSafe(|| conn.execute_batch(&pragma_preamble)),
        );
        if !matches!(applied, Ok(Ok(()))) {
            return Err(StorageError::MigrationFailed(
                "v0 init: schema PRAGMA preamble failed".to_string(),
            )
            .into());
        }
    }

    run_owned_migration_transaction_with_foreign_keys_suspended(conn, "v0 init", |transaction| {
        repair_existing_v0_tables_before_schema_sql(transaction)?;
        #[cfg(test)]
        check_v0_init_fault(V0InitStep::RepairComplete)?;

        transaction
            .execute_batch(&schema_body)
            .map_err(|e| StorageError::MigrationFailed(format!("Schema init failed: {e}")))?;
        #[cfg(test)]
        check_v0_init_fault(V0InitStep::SchemaSqlApplied)?;

        run_migrations_in_existing_transaction(transaction, 0)?;
        #[cfg(test)]
        check_v0_init_fault(V0InitStep::MigrationsApplied)?;

        ensure_ft_meta(transaction, SCHEMA_VERSION)?;
        if stamp_fresh_fts_index {
            // A genuinely fresh database has no historical postings to
            // repair. Keep this authority stamp in the same transaction as
            // schema creation so a failed update cannot expose a committed
            // schema with an indeterminate index-version marker.
            transaction
                .execute(
                    "UPDATE fts_index_state
                         SET index_version = ?1, updated_at = strftime('%s', 'now') * 1000
                         WHERE id = 1",
                    params![i64::from(super::FTS_INDEX_VERSION)],
                )
                .map_err(|_| {
                    StorageError::MigrationFailed(
                        "v0 init: failed to stamp fresh FTS index authority".to_string(),
                    )
                })?;
        }
        Ok(())
    })
}

fn repair_existing_v0_tables_before_schema_sql(conn: &Connection) -> Result<()> {
    if table_exists(conn, "audit_actions")? {
        ensure_audit_actions_correlation_id(conn)?;
    }
    if table_exists(conn, "workflow_step_logs")? {
        ensure_workflow_step_logs_audit_action_id(conn)?;
    }
    if table_exists(conn, "events")? {
        ensure_event_triage_schema(conn)?;
    }

    Ok(())
}

fn migration_for_version(version: i32) -> Option<&'static Migration> {
    MIGRATIONS.iter().find(|m| m.version == version)
}

pub(crate) fn previous_migration_version(version: i32) -> i32 {
    let mut prev = 0;
    for migration in MIGRATIONS {
        if migration.version < version {
            prev = migration.version;
        } else {
            break;
        }
    }
    prev
}

pub(crate) fn build_migration_plan(from_version: i32, to_version: i32) -> Result<MigrationPlan> {
    // Fail closed on a future source DB (ft-men4p). Without this guard a DB whose
    // user_version exceeds SCHEMA_VERSION falls into the rollback branch below,
    // finds no known migrations above SCHEMA_VERSION, and returns an EMPTY plan —
    // which tooling reports as "already current", a false-safe result for an
    // incompatible/newer database. Mirror initialize_schema's SchemaTooNew stop
    // so every planning/migrate entry point (migration_plan_for_path,
    // migrate_database_to_version, `ft db migrate --dry-run`/status) refuses.
    if from_version > SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            current: from_version,
            supported: SCHEMA_VERSION,
        }
        .into());
    }

    if to_version > SCHEMA_VERSION {
        return Err(StorageError::MigrationFailed(format!(
            "Target schema version ({to_version}) is newer than supported ({SCHEMA_VERSION}). \
             Please upgrade FrankenTerm to a newer version."
        ))
        .into());
    }

    if to_version < 1 {
        return Err(StorageError::MigrationFailed(format!(
            "Target schema version ({to_version}) is not supported. \
             The minimum supported schema version is 1."
        ))
        .into());
    }

    if from_version == to_version {
        return Ok(MigrationPlan {
            from_version,
            to_version,
            direction: MigrationDirection::Up,
            steps: Vec::new(),
        });
    }

    if to_version > from_version {
        let steps = MIGRATIONS
            .iter()
            .filter(|m| m.version > from_version && m.version <= to_version)
            .map(|m| MigrationStep {
                migration_version: m.version,
                resulting_version: m.version,
                description: m.description,
                direction: MigrationDirection::Up,
            })
            .collect();

        return Ok(MigrationPlan {
            from_version,
            to_version,
            direction: MigrationDirection::Up,
            steps,
        });
    }

    // Downgrade contract (ft-ftwck): migrations with `down_sql: None` are
    // deliberately forward-only, so the lowest reachable downgrade target is
    // the HIGHEST forward-only version crossed by the requested path. v34
    // protects durable retention-loss evidence; v36 protects row-local snapshot
    // authority; v37 protects exact clean-checkpoint receipt identity; v38
    // protects never-reused IDs and restore-attempt settlement evidence; v39
    // protects the transactional bounded scrollback projection; v40 protects
    // exact retained-session byte authority. V40 is
    // therefore the current downgrade floor.
    let mut steps = Vec::new();
    for migration in MIGRATIONS.iter().rev() {
        if migration.version <= to_version || migration.version > from_version {
            continue;
        }
        if migration.down_sql.is_none() {
            return Err(StorageError::MigrationFailed(format!(
                "Rollback not supported for migration v{} ({})",
                migration.version, migration.description
            ))
            .into());
        }
        let resulting_version = previous_migration_version(migration.version);
        steps.push(MigrationStep {
            migration_version: migration.version,
            resulting_version,
            description: migration.description,
            direction: MigrationDirection::Down,
        });
    }

    Ok(MigrationPlan {
        from_version,
        to_version,
        direction: MigrationDirection::Down,
        steps,
    })
}

fn apply_migration_mutation(
    conn: &Connection,
    step: &MigrationStep,
    migration: &Migration,
) -> Result<()> {
    match step.direction {
        MigrationDirection::Up => {
            let mut apply_raw_up_sql = true;
            match migration.version {
                2 => {
                    ensure_audit_actions_decision_context(conn)?;
                    apply_raw_up_sql = false;
                }
                3 => {
                    ensure_panes_pane_uuid(conn)?;
                    apply_raw_up_sql = false;
                }
                4 => {
                    ensure_workflow_step_logs_audit_action_id(conn)?;
                }
                7 => {
                    ensure_workflow_step_log_columns(conn)?;
                }
                9 => {
                    ensure_agent_sessions_external_meta(conn)?;
                    apply_raw_up_sql = false;
                }
                12 => {
                    ensure_audit_actions_correlation_id(conn)?;
                    apply_raw_up_sql = false;
                }
                18 => {
                    ensure_event_triage_schema(conn)?;
                    apply_raw_up_sql = false;
                }
                19 => {
                    ensure_approval_tokens_plan_hash_schema(conn)?;
                    apply_raw_up_sql = false;
                }
                21 => {
                    ensure_ft_meta_rename_and_session_tables(conn)?;
                    apply_raw_up_sql = false;
                }
                23 => {
                    ensure_segment_embeddings_schema(conn)?;
                }
                29 => {
                    ensure_output_segments_redaction_catalog_version(conn)?;
                    apply_raw_up_sql = false;
                }
                31 => {
                    ensure_output_segments_zone_type(conn)?;
                    apply_raw_up_sql = false;
                }
                32 => {
                    ensure_segment_embeddings_embedded_at_default_ms(conn)?;
                    apply_raw_up_sql = false;
                }
                33 => {
                    ensure_event_delivery_lease_schema(conn)?;
                    apply_raw_up_sql = false;
                }
                36 => {
                    ensure_checkpoint_snapshot_authority_schema(conn)?;
                    apply_raw_up_sql = false;
                }
                37 => {
                    ensure_clean_checkpoint_receipt_schema(conn)?;
                    apply_raw_up_sql = false;
                }
                38 => {
                    ensure_checkpoint_identity_v38_schema(conn)?;
                    ensure_no_foreign_key_violations(conn, "migration v38")?;
                    apply_raw_up_sql = false;
                }
                40 => {
                    ensure_session_retained_size_schema(conn)?;
                    apply_raw_up_sql = false;
                }
                _ => {}
            }
            if apply_raw_up_sql && !migration.up_sql.is_empty() {
                conn.execute_batch(migration.up_sql).map_err(|e| {
                    StorageError::MigrationFailed(format!(
                        "Migration to v{} ({}) failed: {e}",
                        migration.version, migration.description
                    ))
                })?;
            }
            if migration.version == 39 {
                validate_pane_scrollback_summary_schema(conn)?;
                ensure_no_foreign_key_violations(conn, "migration v39")?;
            }
            if migration.version == 40 {
                validate_session_retained_size_schema(conn)?;
                ensure_no_foreign_key_violations(conn, "migration v40")?;
            }
            set_user_version(conn, migration.version)?;
            record_migration(conn, migration.version, migration.description)?;
            Ok(())
        }
        MigrationDirection::Down => {
            let down_sql = migration.down_sql.ok_or_else(|| {
                StorageError::MigrationFailed(format!(
                    "Rollback not supported for migration v{} ({})",
                    migration.version, migration.description
                ))
            })?;
            if !down_sql.is_empty() {
                conn.execute_batch(down_sql).map_err(|e| {
                    StorageError::MigrationFailed(format!(
                        "Rollback of v{} ({}) failed: {e}",
                        migration.version, migration.description
                    ))
                })?;
            }
            set_user_version(conn, step.resulting_version)?;
            record_migration(
                conn,
                step.resulting_version,
                &format!("Rollback: {}", migration.description),
            )?;
            Ok(())
        }
    }
}

pub(crate) fn apply_migration_step(conn: &Connection, step: &MigrationStep) -> Result<()> {
    let Some(migration) = migration_for_version(step.migration_version) else {
        return Err(StorageError::MigrationFailed(format!(
            "Unknown migration version {}",
            step.migration_version
        ))
        .into());
    };

    if step.direction == MigrationDirection::Up && migration.version == 38 {
        return run_owned_migration_transaction_with_foreign_keys_suspended(
            conn,
            "migration v38",
            |transaction| apply_migration_mutation(transaction, step, migration),
        );
    }

    run_owned_migration_transaction(conn, "migration step", |transaction| {
        apply_migration_mutation(transaction, step, migration)
    })
}

pub(crate) fn apply_migration_plan(conn: &Connection, plan: &MigrationPlan) -> Result<()> {
    for step in &plan.steps {
        apply_migration_step(conn, step)?;
        tracing::info!(
            direction = step.direction.as_str(),
            version = step.migration_version,
            resulting_version = step.resulting_version,
            description = step.description,
            "Applied schema migration step"
        );
    }
    Ok(())
}

fn apply_migration_plan_in_existing_transaction(
    conn: &Connection,
    plan: &MigrationPlan,
) -> Result<()> {
    if conn.is_autocommit() {
        return Err(StorageError::MigrationFailed(
            "outer migration transaction is not active".to_string(),
        )
        .into());
    }
    for step in &plan.steps {
        let Some(migration) = migration_for_version(step.migration_version) else {
            return Err(StorageError::MigrationFailed(format!(
                "Unknown migration version {}",
                step.migration_version
            ))
            .into());
        };
        apply_migration_mutation(conn, step, migration)?;
    }
    Ok(())
}

/// Apply all pending migrations from the current version to SCHEMA_VERSION.
///
/// Each migration is applied in order, and the user_version is updated after
/// each successful migration. This ensures that if a migration fails partway
/// through, the database version correctly reflects which migrations have
/// been applied.
fn run_migrations(conn: &Connection, from_version: i32) -> Result<()> {
    let plan = build_migration_plan(from_version, SCHEMA_VERSION)?;
    apply_migration_plan(conn, &plan)
}

fn run_migrations_in_existing_transaction(conn: &Connection, from_version: i32) -> Result<()> {
    let plan = build_migration_plan(from_version, SCHEMA_VERSION)?;
    apply_migration_plan_in_existing_transaction(conn, &plan)
}

/// Get the current schema version from the schema_version audit table.
///
/// This returns the version from the audit table, which should match
/// PRAGMA user_version but provides history of when migrations were applied.
pub fn get_schema_version(conn: &Connection) -> Result<Option<i32>> {
    conn.query_row(
        "SELECT version FROM schema_version ORDER BY applied_at DESC, rowid DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| StorageError::Database(e.to_string()).into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FtMeta {
    pub(crate) schema_version: i32,
    pub(crate) min_compatible_ft: String,
    pub(crate) created_by_ft: String,
    pub(crate) created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FtVersion {
    pub(crate) major: u64,
    pub(crate) minor: u64,
    pub(crate) patch: u64,
}

impl FtVersion {
    pub(crate) fn parse(input: &str) -> Option<Self> {
        let core = input.split(['-', '+']).next().unwrap_or_default();
        let mut parts = core.split('.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
        let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

pub(crate) fn load_ft_meta(conn: &Connection) -> Result<Option<FtMeta>> {
    // Check for new ft_meta table first, fall back to legacy wa_meta
    if table_exists(conn, "ft_meta")? {
        return conn
            .query_row(
                "SELECT schema_version, min_compatible_ft, created_by_ft, created_at \
                 FROM ft_meta WHERE id = 1",
                [],
                |row| {
                    Ok(FtMeta {
                        schema_version: row.get(0)?,
                        min_compatible_ft: row.get(1)?,
                        created_by_ft: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| StorageError::Database(e.to_string()).into());
    }

    // Fall back to legacy wa_meta table for databases not yet migrated
    if table_exists(conn, "wa_meta")? {
        return conn
            .query_row(
                "SELECT schema_version, min_compatible_wa, created_by_wa, created_at \
                 FROM wa_meta WHERE id = 1",
                [],
                |row| {
                    Ok(FtMeta {
                        schema_version: row.get(0)?,
                        min_compatible_ft: row.get(1)?,
                        created_by_ft: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| StorageError::Database(e.to_string()).into());
    }

    Ok(None)
}

fn ensure_ft_meta(conn: &Connection, schema_version: i32) -> Result<()> {
    // Use ft_meta if it exists; otherwise fall back to wa_meta for pre-v21 databases
    let meta_table = if table_exists(conn, "ft_meta")? {
        "ft_meta"
    } else if table_exists(conn, "wa_meta")? {
        "wa_meta"
    } else {
        return Ok(());
    };

    // Column names differ between tables
    let (col_min, col_created) = if meta_table == "ft_meta" {
        ("min_compatible_ft", "created_by_ft")
    } else {
        ("min_compatible_wa", "created_by_wa")
    };

    let existing = load_ft_meta(conn)?;
    let desired = canonical_ft_meta(existing.as_ref(), schema_version);
    match existing.as_ref() {
        None => {
            conn.execute(
                &format!(
                    "INSERT INTO {meta_table} \
                     (id, schema_version, {col_min}, {col_created}, created_at) \
                     VALUES (1, ?1, ?2, ?3, ?4)"
                ),
                params![
                    desired.schema_version,
                    desired.min_compatible_ft,
                    desired.created_by_ft,
                    desired.created_at
                ],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        }
        Some(meta) => {
            if meta != &desired {
                conn.execute(
                    &format!(
                        "UPDATE {meta_table} \
                         SET schema_version=?1, {col_min}=?2, {col_created}=?3, created_at=?4 \
                         WHERE id = 1"
                    ),
                    params![
                        desired.schema_version,
                        desired.min_compatible_ft,
                        desired.created_by_ft,
                        desired.created_at
                    ],
                )
                .map_err(|e| StorageError::Database(e.to_string()))?;
            }
        }
    }

    Ok(())
}

fn canonical_ft_meta(existing: Option<&FtMeta>, schema_version: i32) -> FtMeta {
    let current_ft = crate::VERSION.to_string();
    let Some(existing) = existing else {
        return FtMeta {
            schema_version,
            min_compatible_ft: current_ft.clone(),
            created_by_ft: current_ft,
            created_at: now_epoch_ms(),
        };
    };

    let min_compatible_ft = match (
        FtVersion::parse(&current_ft),
        FtVersion::parse(&existing.min_compatible_ft),
    ) {
        (Some(current), Some(existing_min)) if current <= existing_min => {
            existing.min_compatible_ft.clone()
        }
        _ if existing.min_compatible_ft == current_ft => existing.min_compatible_ft.clone(),
        _ => current_ft.clone(),
    };
    FtMeta {
        schema_version,
        min_compatible_ft,
        created_by_ft: if existing.created_by_ft.is_empty() {
            current_ft
        } else {
            existing.created_by_ft.clone()
        },
        created_at: if existing.created_at <= 0 {
            now_epoch_ms()
        } else {
            existing.created_at
        },
    }
}

fn ft_meta_needs_repair(conn: &Connection, schema_version: i32) -> Result<bool> {
    validate_current_ft_meta_authority(conn)?;
    let existing = load_ft_meta(conn)?;
    Ok(existing
        .as_ref()
        .is_none_or(|meta| meta != &canonical_ft_meta(Some(meta), schema_version)))
}

fn validate_current_ft_meta_authority(conn: &Connection) -> Result<()> {
    let has_ft_meta = table_exists(conn, "ft_meta")?;
    let has_wa_meta = table_exists(conn, "wa_meta")?;
    match (has_ft_meta, has_wa_meta) {
        (true, false) => Ok(()),
        (false, false) => Err(StorageError::Corruption {
            details: "current schema is missing mandatory ft_meta authority table".to_string(),
        }
        .into()),
        (false, true) => Err(StorageError::Corruption {
            details: "current schema retains legacy wa_meta without ft_meta authority".to_string(),
        }
        .into()),
        (true, true) => Err(StorageError::Corruption {
            details: "current schema has conflicting ft_meta and legacy wa_meta authorities"
                .to_string(),
        }
        .into()),
    }
}

fn check_ft_version_compatibility(conn: &Connection) -> Result<()> {
    let Some(meta) = load_ft_meta(conn)? else {
        return Ok(());
    };

    let Some(current) = FtVersion::parse(crate::VERSION) else {
        return Err(StorageError::MigrationFailed(format!(
            "Invalid ft version string: {}",
            crate::VERSION
        ))
        .into());
    };

    let Some(min) = FtVersion::parse(&meta.min_compatible_ft) else {
        return Err(StorageError::MigrationFailed(format!(
            "Invalid min_compatible_ft value in database: {}",
            meta.min_compatible_ft
        ))
        .into());
    };

    if current < min {
        return Err(StorageError::WaTooOld {
            current: crate::VERSION.to_string(),
            min_compatible: meta.min_compatible_ft,
        }
        .into());
    }

    Ok(())
}

/// Check if schema needs initialization (fresh database).
pub fn needs_initialization(conn: &Connection) -> Result<bool> {
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='panes'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;

    Ok(table_exists == 0)
}

/// Get list of pending migrations that would be applied.
///
/// Useful for dry-run scenarios or displaying upgrade information.
#[must_use]
pub fn pending_migrations(current_version: i32) -> Vec<&'static Migration> {
    MIGRATIONS
        .iter()
        .filter(|m| m.version > current_version)
        .collect()
}

/// Build a migration plan for a database path without executing it.
pub fn migration_plan_for_path(db_path: &Path, target_version: i32) -> Result<MigrationPlan> {
    if !db_path.exists() {
        return Err(StorageError::MigrationFailed(format!(
            "Database not found at {}",
            db_path.display()
        ))
        .into());
    }

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    // Migration planning reads the schema version from a possibly-live DB;
    // busy_timeout prevents spurious SQLITE_BUSY when a writer is active.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let needs_init = needs_initialization(&conn)?;
    if needs_init {
        return Err(StorageError::MigrationFailed(
            "Database is uninitialized; run migration without --status to initialize".to_string(),
        )
        .into());
    }

    let current = get_user_version(&conn)?;
    build_migration_plan(current, target_version)
}

/// Return a migration status report for a database path.
pub fn migration_status_for_path(db_path: &Path) -> Result<MigrationStatusReport> {
    let db_exists = db_path.exists();
    if !db_exists {
        return Ok(MigrationStatusReport {
            db_exists,
            needs_initialization: true,
            current_version: 0,
            target_version: SCHEMA_VERSION,
            entries: MIGRATIONS
                .iter()
                .map(|m| MigrationStatusEntry {
                    version: m.version,
                    description: m.description,
                    applied: false,
                    rollback_supported: m.down_sql.is_some(),
                })
                .collect(),
        });
    }

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let needs_init = needs_initialization(&conn)?;
    let current = get_user_version(&conn)?;
    let entries = MIGRATIONS
        .iter()
        .map(|m| MigrationStatusEntry {
            version: m.version,
            description: m.description,
            applied: !needs_init && m.version <= current,
            rollback_supported: m.down_sql.is_some(),
        })
        .collect();

    Ok(MigrationStatusReport {
        db_exists,
        needs_initialization: needs_init,
        current_version: current,
        target_version: SCHEMA_VERSION,
        entries,
    })
}

/// Migrate a database at the given path to a target schema version.
///
/// If the database is uninitialized, it will be initialized to the current
/// schema version (SCHEMA_VERSION). Initializing to older versions is not supported.
pub fn migrate_database_to_version(db_path: &Path, target_version: i32) -> Result<MigrationPlan> {
    ensure_parent_dir(db_path)?;

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    // Migrations need the write lock for ALTER/CREATE; without busy_timeout
    // any concurrent reader makes the migration abort at the first ALTER.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let needs_init = needs_initialization(&conn)?;
    let current = get_user_version(&conn)?;

    if needs_init {
        if target_version != SCHEMA_VERSION {
            return Err(StorageError::MigrationFailed(format!(
                "Database is uninitialized; can only initialize to current schema version ({SCHEMA_VERSION})."
            ))
            .into());
        }
        initialize_schema(&conn)?;
        return Ok(MigrationPlan {
            from_version: 0,
            to_version: SCHEMA_VERSION,
            direction: MigrationDirection::Up,
            steps: vec![MigrationStep {
                migration_version: SCHEMA_VERSION,
                resulting_version: SCHEMA_VERSION,
                description: "Initial schema",
                direction: MigrationDirection::Up,
            }],
        });
    }

    let plan = build_migration_plan(current, target_version)?;
    apply_migration_plan(&conn, &plan)?;
    run_owned_migration_transaction(&conn, "schema metadata", |transaction| {
        ensure_ft_meta(transaction, target_version)
    })?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_sql_comparison_normalizes_only_non_semantic_syntax() {
        assert_eq!(
            compact_schema_sql("CREATE INDEX IF NOT EXISTS idx_example ON example(value DESC);",),
            compact_schema_sql("CREATE INDEX idx_example ON example(value DESC)"),
            "sqlite_schema may omit the semicolon accepted by execute_batch"
        );
        assert_eq!(
            compact_schema_sql("CREATE UNIQUE INDEX IF NOT EXISTS idx_unique ON example(value);",),
            compact_schema_sql("CREATE UNIQUE INDEX idx_unique ON example(value)"),
            "sqlite_schema also omits IF NOT EXISTS from unique indexes"
        );
        for (canonical, sqlite_schema) in [
            (
                "CREATE TABLE IF NOT EXISTS example (id INTEGER);",
                "CREATE TABLE example (id INTEGER)",
            ),
            (
                "CREATE VIEW IF NOT EXISTS example_view AS SELECT 1;",
                "CREATE VIEW example_view AS SELECT 1",
            ),
            (
                "CREATE TRIGGER IF NOT EXISTS example_trigger AFTER INSERT ON example BEGIN SELECT 1; END;",
                "CREATE TRIGGER example_trigger AFTER INSERT ON example BEGIN SELECT 1; END",
            ),
        ] {
            assert_eq!(
                compact_schema_sql(canonical),
                compact_schema_sql(sqlite_schema),
                "sqlite_schema omits IF NOT EXISTS from persisted schema objects"
            );
        }
        assert!(
            compact_schema_sql(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_literal ON example(value) \
                 WHERE value <> 'createuniqueindexifnotexists';",
            )
            .ends_with("wherevalue<>'createuniqueindexifnotexists'"),
            "normalization must not rewrite SQL string literals that happen to contain the prefix"
        );
        assert_ne!(
            compact_schema_sql(
                "CREATE UNIQUE INDEX idx_literal ON example(value) WHERE value = 'A B';",
            ),
            compact_schema_sql(
                "create unique index idx_literal on example(value) where value = 'ab'",
            ),
            "normalization must preserve case and whitespace inside SQL string literals"
        );
        assert_eq!(
            compact_schema_sql(
                "CREATE INDEX idx_literal ON example(value) WHERE value = 'O''Reilly A';",
            ),
            "createindexidx_literalonexample(value)wherevalue='O''Reilly A'",
            "an escaped quote must not end literal-preservation mode early"
        );
        assert_eq!(
            compact_schema_sql(
                "CREATE INDEX IF NOT EXISTS \"Index Name\" ON [Table Name](`Column Name`);",
            ),
            "createindex\"Index Name\"on[Table Name](`Column Name`)",
            "quoted identifier spelling must remain fail-closed while surrounding SQL is normalized"
        );
    }

    #[test]
    fn head_schema_ddl_satisfies_checkpoint_identity_v38_contract() {
        let conn = Connection::open_in_memory().expect("open head-schema fixture");
        let (pragma_preamble, schema_body) = split_schema_sql_pragmas();
        conn.execute_batch(&pragma_preamble)
            .expect("apply schema PRAGMA preamble");
        conn.execute_batch(&schema_body)
            .expect("apply head schema body");

        validate_checkpoint_identity_v38_schema(&conn)
            .expect("head DDL must satisfy the v38 checkpoint authority contract");
    }

    #[test]
    fn v0_schema_body_defers_v40_until_legacy_session_columns_exist() {
        let (_, schema_body) = split_schema_sql_pragmas();
        assert!(!schema_body.contains(SESSION_RETAINED_SIZE_V40_BEGIN));
        assert!(!schema_body.contains("CREATE TABLE IF NOT EXISTS session_retained_size"));
        assert!(!schema_body.contains("session_checkpoints_retained_size_ai"));

        let conn = Connection::open_in_memory().expect("open current-schema fixture");
        initialize_schema(&conn).expect("v0 migration sequence installs current schema");
        validate_session_retained_size_schema(&conn)
            .expect("v40 must be installed after all legacy session columns exist");
        assert_eq!(get_user_version(&conn).unwrap(), 40);
    }

    fn create_v35_checkpoint_fixture(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE mux_sessions (
                 session_id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL,
                 last_checkpoint_at INTEGER,
                 shutdown_clean INTEGER NOT NULL DEFAULT 0,
                 topology_json TEXT NOT NULL,
                 window_metadata_json TEXT,
                 ft_version TEXT NOT NULL,
                 host_id TEXT
             );
             CREATE TABLE session_checkpoints (
                 id INTEGER PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                 checkpoint_at INTEGER NOT NULL,
                 checkpoint_type TEXT NOT NULL
                     CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
                 state_hash TEXT NOT NULL,
                 pane_count INTEGER NOT NULL,
                 total_bytes INTEGER NOT NULL,
                 metadata_json TEXT
             );
             CREATE INDEX idx_checkpoints_session
                 ON session_checkpoints(session_id, checkpoint_at);
             CREATE TABLE mux_pane_state (
                 id INTEGER PRIMARY KEY,
                 checkpoint_id INTEGER NOT NULL
                     REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                 pane_id INTEGER NOT NULL
             );
             CREATE TABLE schema_version (
                 version INTEGER NOT NULL,
                 applied_at INTEGER NOT NULL,
                 description TEXT
             );
             PRAGMA user_version = 35;",
        )
        .expect("create canonical v35 checkpoint fixture");
    }

    fn create_v37_checkpoint_fixture(conn: &Connection) {
        create_v35_checkpoint_fixture(conn);
        ensure_checkpoint_snapshot_authority_schema(conn)
            .expect("upgrade checkpoint fixture to v36");
        ensure_clean_checkpoint_receipt_schema(conn).expect("upgrade checkpoint fixture to v37");
        set_user_version(conn, 37).expect("stamp canonical v37 fixture");
    }

    fn table_descriptors(conn: &Connection, table: &str) -> Vec<SqliteColumnDescriptor> {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare table descriptor query");
        let rows = statement
            .query_map([], |row| {
                Ok(SqliteColumnDescriptor {
                    cid: row.get(0)?,
                    name: row.get(1)?,
                    declared_type: row.get(2)?,
                    not_null: row.get::<_, i64>(3)? != 0,
                    default_value: row.get(4)?,
                    primary_key: row.get::<_, i64>(5)? != 0,
                })
            })
            .expect("query table descriptors");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect table descriptors")
    }

    fn is_migration_epoch_poisoned(error: &crate::error::Error) -> bool {
        matches!(
            error,
            crate::error::Error::Storage(StorageError::MigrationEpochPoisoned)
        )
    }

    fn execute_migration_test_sql(conn: &Connection, sql: &str) -> Result<()> {
        conn.execute_batch(sql).map_err(|_| {
            StorageError::MigrationFailed("test migration SQL failed".to_string()).into()
        })
    }

    #[test]
    fn migration_begin_failure_does_not_mutate_or_poison_connection() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        set_migration_transaction_fault_for_test(Some(MigrationTransactionFault::Begin));
        let error = run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE begin_probe(id INTEGER)")?;
            Ok(())
        })
        .expect_err("injected begin failure must be visible");

        assert!(error.to_string().contains("failed to begin"));
        assert!(conn.is_autocommit());
        assert!(!migration_connection_is_query_only(&conn).unwrap());
        assert!(!table_exists(&conn, "begin_probe").unwrap());

        run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE begin_probe(id INTEGER)")?;
            Ok(())
        })
        .expect("one-shot begin fault must permit a clean retry");
        assert!(table_exists(&conn, "begin_probe").unwrap());
    }

    #[cfg(panic = "unwind")]
    #[test]
    fn migration_mutation_panic_rolls_back_schema_and_version_before_retry() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        set_migration_transaction_fault_for_test(Some(MigrationTransactionFault::MutationPanic));
        let error = run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE panic_probe(id INTEGER)")?;
            set_user_version(transaction, 17)?;
            Ok(())
        })
        .expect_err("injected mutation panic must be contained");

        assert!(error.to_string().contains("migration mutation panicked"));
        assert!(!error.to_string().contains("synthetic migration"));
        assert!(conn.is_autocommit());
        assert!(!migration_connection_is_query_only(&conn).unwrap());
        assert!(!table_exists(&conn, "panic_probe").unwrap());
        assert_eq!(get_user_version(&conn).unwrap(), 0);

        run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE panic_probe(id INTEGER)")?;
            set_user_version(transaction, 17)?;
            Ok(())
        })
        .expect("connection must remain reusable after a proven rollback");
        assert!(table_exists(&conn, "panic_probe").unwrap());
        assert_eq!(get_user_version(&conn).unwrap(), 17);
    }

    #[test]
    fn migration_mutation_error_is_finite_after_proven_rollback() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        let error = run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE error_probe(id INTEGER)")?;
            Err(StorageError::MigrationFailed("synthetic mutation failure".to_string()).into())
        })
        .expect_err("mutation error must be visible");

        assert!(error.to_string().contains("migration mutation failed"));
        assert!(!error.to_string().contains("synthetic mutation failure"));
        assert!(conn.is_autocommit());
        assert!(!migration_connection_is_query_only(&conn).unwrap());
        assert!(!table_exists(&conn, "error_probe").unwrap());
    }

    #[test]
    fn migration_commit_failure_rolls_back_before_connection_reuse() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        set_migration_transaction_fault_for_test(Some(MigrationTransactionFault::Commit));
        let error = run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE commit_probe(id INTEGER)")?;
            set_user_version(transaction, 23)?;
            Ok(())
        })
        .expect_err("injected commit failure must be visible");

        assert!(error.to_string().contains("failed to commit"));
        assert!(conn.is_autocommit());
        assert!(!migration_connection_is_query_only(&conn).unwrap());
        assert!(!table_exists(&conn, "commit_probe").unwrap());
        assert_eq!(get_user_version(&conn).unwrap(), 0);
    }

    #[test]
    fn migration_rollback_failure_poison_fences_same_connection_epoch() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        set_migration_transaction_fault_for_test(Some(MigrationTransactionFault::Rollback));
        let error = run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE rollback_probe(id INTEGER)")?;
            Err(StorageError::MigrationFailed("force rollback".to_string()).into())
        })
        .expect_err("rollback failure must poison the epoch");

        assert!(is_migration_epoch_poisoned(&error));
        assert!(!conn.is_autocommit());
        assert!(migration_connection_is_query_only(&conn).unwrap());
        assert!(
            conn.execute_batch("CREATE TABLE forbidden_after_poison(id INTEGER)")
                .is_err(),
            "query_only must fence ordinary writes in the poisoned epoch"
        );
        let later = run_owned_migration_transaction(&conn, "migration step", |_| Ok(()))
            .expect_err("later migration must not enter the poisoned epoch");
        assert!(is_migration_epoch_poisoned(&later));

        // Test cleanup only: production callers retire the Connection instead.
        conn.execute_batch("ROLLBACK")
            .expect("cleanup open test transaction");
        conn.execute_batch("PRAGMA query_only = OFF")
            .expect("cleanup test query_only fence");
    }

    #[test]
    fn migration_unverifiable_closure_requires_fresh_connection_epoch() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("migration-epoch.sqlite3");
        let conn = Connection::open(&path).expect("open test database");
        set_migration_transaction_fault_for_test(Some(
            MigrationTransactionFault::ClosureVerification,
        ));
        let error = run_owned_migration_transaction(&conn, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE closure_probe(id INTEGER)")?;
            Err(StorageError::MigrationFailed("force rollback".to_string()).into())
        })
        .expect_err("unverifiable closure must poison the epoch");

        assert!(is_migration_epoch_poisoned(&error));
        assert!(conn.is_autocommit());
        assert!(migration_connection_is_query_only(&conn).unwrap());
        assert!(!table_exists(&conn, "closure_probe").unwrap());
        assert!(
            conn.execute_batch("CREATE TABLE forbidden_after_closure(id INTEGER)")
                .is_err()
        );
        drop(conn);

        let reopened = Connection::open(&path).expect("reopen fresh connection epoch");
        assert!(!migration_connection_is_query_only(&reopened).unwrap());
        run_owned_migration_transaction(&reopened, "migration step", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE closure_probe(id INTEGER)")?;
            Ok(())
        })
        .expect("fresh connection epoch must permit retry");
        assert!(table_exists(&reopened, "closure_probe").unwrap());
    }

    #[test]
    fn migration_authority_verification_fault_fences_same_connection() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("migration-authority.sqlite3");
        let conn = Connection::open(&path).expect("open test database");
        set_migration_transaction_fault_for_test(Some(
            MigrationTransactionFault::AuthorityVerification,
        ));

        let error = run_owned_migration_transaction(&conn, "authority verification", |_| Ok(()))
            .expect_err("unverifiable write authority must poison the connection epoch");
        assert!(is_migration_epoch_poisoned(&error));
        assert!(migration_connection_is_query_only(&conn).unwrap());
        assert!(
            conn.execute_batch("CREATE TABLE forbidden_after_authority_fault(id INTEGER)")
                .is_err()
        );
        drop(conn);

        let reopened = Connection::open(&path).expect("open fresh connection epoch");
        assert!(!migration_connection_is_query_only(&reopened).unwrap());
        run_owned_migration_transaction(&reopened, "authority verification", |transaction| {
            execute_migration_test_sql(transaction, "CREATE TABLE allowed_after_reopen(id INTEGER)")
        })
        .expect("fresh connection must regain write authority");
    }

    #[test]
    fn current_schema_noop_avoids_writer_lock_but_repair_requires_it() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("current-schema-lock.sqlite3");
        let conn = Connection::open(&path).expect("open schema connection");
        initialize_schema(&conn).expect("initialize current schema");
        let holder = Connection::open(&path).expect("open competing writer");

        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold SQLite writer lock");
        initialize_schema(&conn)
            .expect("healthy current-schema reopen must remain read-only under writer lock");
        holder
            .execute_batch("ROLLBACK")
            .expect("release writer lock");

        conn.execute("UPDATE ft_meta SET schema_version = 0 WHERE id = 1", [])
            .expect("seed repairable metadata drift");
        conn.busy_timeout(std::time::Duration::ZERO)
            .expect("disable wait for deterministic lock proof");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold writer lock during repair");
        initialize_schema(&conn).expect_err("metadata repair must acquire writer authority");
        holder
            .execute_batch("ROLLBACK")
            .expect("release repair lock");

        initialize_schema(&conn).expect("repair succeeds after lock release");
        assert_eq!(
            load_ft_meta(&conn)
                .expect("load repaired metadata")
                .expect("metadata row")
                .schema_version,
            SCHEMA_VERSION
        );
    }

    #[test]
    fn current_schema_missing_ft_meta_authority_fails_closed() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch("DROP TABLE ft_meta")
            .expect("remove mandatory metadata authority in test fixture");

        let error = initialize_schema(&conn)
            .expect_err("a current-version stamp cannot authorize a missing ft_meta table");
        assert!(matches!(
            error,
            crate::error::Error::Storage(StorageError::Corruption { details })
                if details.contains("missing mandatory ft_meta")
        ));
    }

    #[test]
    fn nonzero_schema_missing_panes_authority_fails_closed() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE panes; PRAGMA foreign_keys = ON")
            .expect("remove mandatory panes authority in test fixture");

        let error = initialize_schema(&conn)
            .expect_err("a nonzero version stamp cannot authorize a missing panes table");
        assert!(matches!(
            error,
            crate::error::Error::Storage(StorageError::Corruption { details })
                if details.contains("missing mandatory panes")
        ));
    }

    #[test]
    fn current_schema_legacy_only_metadata_authority_fails_closed() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch("ALTER TABLE ft_meta RENAME TO wa_meta")
            .expect("replace current metadata authority with legacy table in fixture");

        let error = initialize_schema(&conn)
            .expect_err("current schema must not accept legacy-only wa_meta authority");
        assert!(matches!(
            error,
            crate::error::Error::Storage(StorageError::Corruption { details })
                if details.contains("legacy wa_meta without ft_meta")
        ));
    }

    #[test]
    fn current_schema_dual_metadata_authority_fails_closed() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch("CREATE TABLE wa_meta (id INTEGER PRIMARY KEY)")
            .expect("seed conflicting legacy metadata authority");

        let error = initialize_schema(&conn)
            .expect_err("current schema must reject dual metadata authorities");
        assert!(matches!(
            error,
            crate::error::Error::Storage(StorageError::Corruption { details })
                if details.contains("conflicting ft_meta")
        ));
    }

    #[test]
    fn pending_migrations_reports_all_versions_from_zero() {
        let pending = pending_migrations(0);
        assert_eq!(pending.len(), MIGRATIONS.len());
        assert!(pending.iter().all(|migration| migration.version > 0));
    }

    #[test]
    fn pending_migrations_empty_at_head() {
        assert!(pending_migrations(SCHEMA_VERSION).is_empty());
    }

    #[test]
    fn migration_plan_noops_when_versions_match() {
        let plan = build_migration_plan(SCHEMA_VERSION, SCHEMA_VERSION).unwrap();
        assert_eq!(plan.from_version, SCHEMA_VERSION);
        assert_eq!(plan.to_version, SCHEMA_VERSION);
        assert_eq!(plan.direction, MigrationDirection::Up);
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn build_migration_plan_rejects_future_from_version() {
        // ft-men4p: a DB whose user_version exceeds SCHEMA_VERSION must fail
        // closed (SchemaTooNew), not fall into the rollback branch and return an
        // empty "already current" plan that tooling reports as safe.
        let future = SCHEMA_VERSION + 1;
        let err = build_migration_plan(future, SCHEMA_VERSION)
            .expect_err("a future source schema version must be rejected, not planned as empty");
        let message = err.to_string();
        assert!(
            message.contains(&future.to_string()),
            "fail-closed error should cite the future version {future}: {message}"
        );
        // Rolling a future DB down to an older target is likewise refused.
        assert!(build_migration_plan(future, SCHEMA_VERSION - 1).is_err());
    }

    #[test]
    fn build_migration_plan_fuzz_matrix_is_fail_closed_and_sound() {
        // Exhaustive sweep over a wide (from, to) version matrix — a fuzz of every
        // user_version/target pairing. Future source/target and sub-1 targets must
        // FAIL CLOSED (ft-men4p); valid pairs must yield a structurally-sound,
        // monotonic plan and never a misleading empty "already current" result.
        for from in -3..=(SCHEMA_VERSION + 8) {
            for to in -3..=(SCHEMA_VERSION + 8) {
                let result = build_migration_plan(from, to);

                if from > SCHEMA_VERSION {
                    assert!(
                        result.is_err(),
                        "future from_version {from} must fail closed"
                    );
                    continue;
                }
                if !(1..=SCHEMA_VERSION).contains(&to) {
                    assert!(
                        result.is_err(),
                        "out-of-range to_version {to} must be rejected"
                    );
                    continue;
                }

                // Valid target range (1..=SCHEMA_VERSION) with a non-future source.
                if from <= to {
                    let plan = result.expect("forward/no-op plan in valid range must succeed");
                    assert_eq!(plan.from_version, from);
                    assert_eq!(plan.to_version, to);
                    if from == to {
                        assert!(plan.steps.is_empty(), "no-op plan must have no steps");
                    } else {
                        assert_eq!(plan.direction, MigrationDirection::Up);
                        let mut prev = from;
                        for step in &plan.steps {
                            assert!(
                                step.migration_version > prev && step.migration_version <= to,
                                "up step {} must lie in (from {from}, to {to}]",
                                step.migration_version
                            );
                            prev = step.migration_version;
                        }
                    }
                } else if let Ok(plan) = result {
                    // Downgrade within range: a valid Down plan, or an explicit
                    // error when a migration lacks down_sql — never a wrong/empty
                    // result.
                    assert_eq!(plan.direction, MigrationDirection::Down);
                    assert_eq!(plan.from_version, from);
                    assert_eq!(plan.to_version, to);
                }
            }
        }
    }

    #[test]
    fn initialized_schema_segment_embeddings_default_is_ms() {
        // ft-wi24o: after the full init sequence (SCHEMA_SQL + run_migrations(0),
        // which replays the v32 default repair), segment_embeddings.embedded_at
        // must default to epoch MILLISECONDS — not the legacy epoch-seconds
        // strftime that reintroduced the 1000x timestamp-unit bug.
        let conn = Connection::open_in_memory().expect("open in-memory db");
        initialize_schema(&conn).expect("initialize schema");

        // Golden on the schema shape: the default scales strftime seconds to ms.
        let create_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='segment_embeddings'",
                [],
                |row| row.get(0),
            )
            .expect("read segment_embeddings DDL");
        assert!(
            create_sql.contains("embedded_at"),
            "segment_embeddings must define embedded_at: {create_sql}"
        );
        assert!(
            create_sql.contains("* 1000"),
            "embedded_at default must be epoch ms (ft-wi24o), got: {create_sql}"
        );

        // Behavioural check: an insert omitting embedded_at stores ms. Epoch ms
        // (~1.7e12) is far above the 100_000_000_000 threshold the v30 normalizer
        // uses; epoch seconds (~1.7e9) would fall below it.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("disable fk for orphan insert");
        conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector) \
             VALUES (1, 'embedder-x', 8, X'0102030405060708')",
            [],
        )
        .expect("insert omitting embedded_at");
        let embedded_at: i64 = conn
            .query_row(
                "SELECT embedded_at FROM segment_embeddings WHERE segment_id = 1",
                [],
                |row| row.get(0),
            )
            .expect("read embedded_at");
        assert!(
            embedded_at > 100_000_000_000,
            "embedded_at default must be epoch ms (ft-wi24o); got {embedded_at} (looks like seconds)"
        );
    }

    /// br-ft-4yr9i: SCHEMA_VERSION is in lockstep with the
    /// MIGRATIONS array. Pinned at the migration boundary so a
    /// future migration entry that doesn't bump SCHEMA_VERSION
    /// (or vice versa) trips this test.
    #[test]
    fn schema_version_matches_migrations_array_length() {
        // MIGRATIONS[i].version == i + 1 (versions start at 1).
        // The latest entry's version equals SCHEMA_VERSION.
        let last = MIGRATIONS.last().expect("MIGRATIONS array is non-empty");
        assert_eq!(
            last.version, SCHEMA_VERSION,
            "last migration version must equal SCHEMA_VERSION; \
             bump SCHEMA_VERSION when adding a Migration entry",
        );
        assert_eq!(
            MIGRATIONS.len() as i32,
            SCHEMA_VERSION,
            "MIGRATIONS array length must equal SCHEMA_VERSION; \
             versions are 1-indexed contiguous",
        );
    }

    /// br-ft-4yr9i: the version-25 migration entry exists and
    /// targets the agent_profiles substrate. Caller-facing
    /// description string is part of the contract (operator
    /// inspecting `ft doctor --migrations` reads it).
    #[test]
    fn agent_profiles_migration_entry_present_at_version_25() {
        let m25 = MIGRATIONS
            .iter()
            .find(|m| m.version == 25)
            .expect("version 25 migration must be registered");
        assert!(
            m25.description.contains("agent_profiles"),
            "description must reference agent_profiles, got: {:?}",
            m25.description,
        );
        // Both substrate statements must appear in up_sql so the
        // table + role index land together.
        assert!(
            m25.up_sql
                .contains("CREATE TABLE IF NOT EXISTS agent_profiles"),
            "up_sql must include the agent_profiles CREATE TABLE",
        );
        assert!(
            m25.up_sql.contains("agent_profiles_role_idx"),
            "up_sql must include the role index",
        );
        // Down-rollback must drop both, in reverse order.
        let down = m25.down_sql.expect("down_sql must be supported");
        assert!(down.contains("DROP INDEX IF EXISTS agent_profiles_role_idx"));
        assert!(down.contains("DROP TABLE IF EXISTS agent_profiles"));
    }

    #[test]
    fn fleet_mutation_receipts_migration_entry_present_at_version_27() {
        let m27 = MIGRATIONS
            .iter()
            .find(|m| m.version == 27)
            .expect("version 27 migration must be registered");
        assert!(
            m27.description.contains("fleet_mutation_receipts"),
            "description must reference fleet_mutation_receipts, got: {:?}",
            m27.description,
        );
        assert!(
            m27.up_sql
                .contains("CREATE TABLE IF NOT EXISTS fleet_mutation_receipts"),
            "up_sql must include the fleet_mutation_receipts CREATE TABLE",
        );
        assert!(
            m27.up_sql
                .contains("fleet_mutation_receipts_action_time_idx"),
            "up_sql must include the action/time index",
        );
        let down = m27.down_sql.expect("down_sql must be supported");
        assert!(down.contains("DROP INDEX IF EXISTS fleet_mutation_receipts_action_time_idx"));
        assert!(down.contains("DROP TABLE IF EXISTS fleet_mutation_receipts"));
    }

    #[test]
    fn limit_windows_migration_entry_present_at_version_28() {
        let m28 = MIGRATIONS
            .iter()
            .find(|m| m.version == 28)
            .expect("version 28 migration must be registered");
        assert!(
            m28.description.contains("limit_windows"),
            "description must reference limit_windows, got: {:?}",
            m28.description,
        );
        assert!(
            m28.up_sql
                .contains("CREATE TABLE IF NOT EXISTS limit_windows"),
            "up_sql must include the limit_windows CREATE TABLE",
        );
        assert!(
            m28.up_sql.contains("idx_limit_windows_pane_account"),
            "up_sql must include the pane/account index",
        );
        assert!(
            m28.up_sql.contains("UNIQUE(pane_id, service, account_id)"),
            "up_sql must preserve idempotency key",
        );
        let down = m28.down_sql.expect("down_sql must be supported");
        assert!(down.contains("DROP INDEX IF EXISTS idx_limit_windows_pane_account"));
        assert!(down.contains("DROP TABLE IF EXISTS limit_windows"));
    }

    #[test]
    fn redaction_catalog_version_migration_entry_present_at_version_29() {
        let m29 = MIGRATIONS
            .iter()
            .find(|m| m.version == 29)
            .expect("version 29 migration must be registered");
        assert!(
            m29.description.contains("redaction catalog version"),
            "description must reference the redaction catalog version, got: {:?}",
            m29.description,
        );
        assert!(
            m29.up_sql
                .contains("ALTER TABLE output_segments ADD COLUMN redaction_catalog_version"),
            "up_sql must add the redaction_catalog_version column",
        );
        let down = m29.down_sql.expect("down_sql must be supported");
        assert!(down.contains("DROP COLUMN redaction_catalog_version"));
    }

    /// ft-7h5da.1.5: applying v29 to an output_segments table adds the
    /// redaction_catalog_version column; the down-rollback removes it.
    #[test]
    fn redaction_catalog_version_migration_adds_and_drops_column() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        // Minimal pre-v29 output_segments shape (sufficient to ALTER).
        conn.execute_batch(
            "CREATE TABLE output_segments (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_len INTEGER NOT NULL,
                content_hash TEXT,
                captured_at INTEGER NOT NULL,
                UNIQUE(pane_id, seq)
            );",
        )
        .expect("base output_segments");

        let m29 = MIGRATIONS.iter().find(|m| m.version == 29).expect("v29");
        conn.execute_batch(m29.up_sql).expect("apply v29");
        assert!(
            output_segments_has_column(&conn, "redaction_catalog_version"),
            "column must exist after v29",
        );

        conn.execute_batch(m29.down_sql.unwrap())
            .expect("rollback v29");
        assert!(
            !output_segments_has_column(&conn, "redaction_catalog_version"),
            "column must be gone after down-rollback",
        );
    }

    fn output_segments_has_column(conn: &rusqlite::Connection, col: &str) -> bool {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('output_segments')")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        names.iter().any(|n| n == col)
    }

    #[test]
    fn embedded_at_normalization_migration_entry_present_at_version_30() {
        let m30 = MIGRATIONS
            .iter()
            .find(|m| m.version == 30)
            .expect("version 30 migration must be registered");
        assert!(
            m30.description.contains("embedded_at") && m30.description.contains("millisecond"),
            "description must reference the embedded_at ms normalization, got: {:?}",
            m30.description,
        );
        assert!(
            m30.up_sql.contains("UPDATE segment_embeddings")
                && m30.up_sql.contains("* 1000")
                && m30.up_sql.contains("100000000000"),
            "up_sql must scale sub-1e11 (seconds) embedded_at values by 1000",
        );
        assert!(
            m30.down_sql.is_none(),
            "v30 is a forward-only normalization (seconds and ms become \
             indistinguishable after the up-migration)",
        );
    }

    /// ft-ayy9x: applying v30 scales epoch-seconds `embedded_at` values to
    /// epoch ms while leaving already-ms values untouched; re-running is a
    /// no-op (converted values land above the 1e11 seconds/ms boundary).
    #[test]
    fn embedded_at_normalization_migration_seconds_to_ms_is_idempotent() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE segment_embeddings (
                segment_id INTEGER NOT NULL,
                embedder_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector BLOB NOT NULL,
                embedded_at INTEGER NOT NULL,
                PRIMARY KEY (segment_id, embedder_id)
            );
            INSERT INTO segment_embeddings VALUES (1, 'e', 4, X'00', 1700000000);
            INSERT INTO segment_embeddings VALUES (2, 'e', 4, X'00', 1700000000000);",
        )
        .expect("seed segment_embeddings");

        let m30 = MIGRATIONS.iter().find(|m| m.version == 30).expect("v30");
        conn.execute_batch(m30.up_sql).expect("apply v30");

        let read = |seg: i64| -> i64 {
            conn.query_row(
                "SELECT embedded_at FROM segment_embeddings WHERE segment_id = ?1",
                [seg],
                |row| row.get(0),
            )
            .unwrap()
        };
        // Seconds row scaled to ms; already-ms row untouched.
        assert_eq!(read(1), 1_700_000_000_000, "seconds row scaled to ms");
        assert_eq!(read(2), 1_700_000_000_000, "already-ms row untouched");

        // Idempotent: a second application changes nothing.
        conn.execute_batch(m30.up_sql).expect("re-apply v30");
        assert_eq!(read(1), 1_700_000_000_000, "re-run is a no-op for seg 1");
        assert_eq!(read(2), 1_700_000_000_000, "re-run is a no-op for seg 2");
    }

    #[test]
    fn segment_embeddings_default_repair_migration_entry_present_at_version_32() {
        let m32 = MIGRATIONS
            .iter()
            .find(|m| m.version == 32)
            .expect("version 32 migration must be registered");
        assert!(
            m32.description.contains("embedded_at") && m32.description.contains("DEFAULT"),
            "description must reference the embedded_at default repair, got: {:?}",
            m32.description,
        );
        assert!(
            m32.down_sql.is_none(),
            "v32 is a forward-only default repair",
        );
        assert!(
            SCHEMA_VERSION >= 32,
            "schema head must retain the registered v32 repair"
        );
    }

    /// ft-wi24o: on a DB upgraded through v22/v23 the segment_embeddings
    /// `embedded_at` DEFAULT is still epoch seconds (v30 only fixed values).
    /// v32 rebuilds the table to the ms default, preserving rows, and a
    /// default-omitting insert then stores ms. Idempotent on already-ms tables.
    #[test]
    fn segment_embeddings_v32_rebuilds_seconds_default_to_ms() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        // FK target for the rebuild's INNER JOIN.
        conn.execute_batch(
            "CREATE TABLE output_segments (id INTEGER PRIMARY KEY);
             INSERT INTO output_segments (id) VALUES (1);",
        )
        .expect("output_segments");

        // Upgraded-DB shape: LEGACY seconds default (what v22/v23 created), with
        // a value already normalized to ms by v30.
        conn.execute_batch(
            "CREATE TABLE segment_embeddings (
                segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
                embedder_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector BLOB NOT NULL,
                embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                PRIMARY KEY (segment_id, embedder_id)
            );
            INSERT INTO segment_embeddings
                (segment_id, embedder_id, dimension, vector, embedded_at)
                VALUES (1, 'e', 4, X'00', 1700000000000);",
        )
        .expect("seed legacy-default segment_embeddings");

        assert!(
            !segment_embeddings_embedded_at_default_is_ms(&conn).unwrap(),
            "fixture must start on the legacy seconds default",
        );

        ensure_segment_embeddings_embedded_at_default_ms(&conn).expect("v32 repair");

        assert!(
            segment_embeddings_embedded_at_default_is_ms(&conn).unwrap(),
            "default must be repaired to epoch ms",
        );
        let kept: i64 = conn
            .query_row(
                "SELECT embedded_at FROM segment_embeddings WHERE segment_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1_700_000_000_000, "existing ms row preserved");

        // A default-omitting insert now stores epoch ms (>= 1e11), not seconds.
        conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector)
             VALUES (1, 'e2', 4, X'00')",
            [],
        )
        .expect("insert omitting embedded_at");
        let defaulted: i64 = conn
            .query_row(
                "SELECT embedded_at FROM segment_embeddings WHERE embedder_id = 'e2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            defaulted >= 100_000_000_000,
            "default insert must store epoch ms, got {defaulted}",
        );

        // Idempotent: re-running on an already-ms table is a no-op.
        ensure_segment_embeddings_embedded_at_default_ms(&conn).expect("v32 repair re-run");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM segment_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 2, "re-run must not lose or duplicate rows");
        assert!(segment_embeddings_embedded_at_default_is_ms(&conn).unwrap());
    }

    #[test]
    fn event_delivery_lease_migration_entry_present_at_version_33() {
        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 33)
            .expect("version 33 migration must be registered");
        assert!(migration.description.contains("event-delivery leases"));
        for column in [
            "delivery_lease_token",
            "delivery_lease_acquired_at",
            "delivery_lease_expires_at",
        ] {
            assert!(
                migration.up_sql.contains(column),
                "v33 up migration must mention {column}"
            );
            assert!(
                migration
                    .down_sql
                    .expect("v33 is reversibly additive")
                    .contains(column),
                "v33 down migration must remove {column}"
            );
        }
        assert!(
            SCHEMA_VERSION >= 33,
            "schema head must retain the registered v33 lease migration"
        );
    }

    #[test]
    fn event_delivery_lease_v33_upgrades_and_rolls_back_without_partial_columns() {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             CREATE TABLE schema_version (
                 version INTEGER NOT NULL,
                 applied_at INTEGER NOT NULL,
                 description TEXT
             );
             PRAGMA user_version = 32;",
        )
        .expect("create pre-v33 events schema");
        conn.execute("INSERT INTO events (id) VALUES (7)", [])
            .expect("seed pre-v33 event row");

        let step = MigrationStep {
            migration_version: 33,
            resulting_version: 33,
            description: "test v33 event delivery lease upgrade",
            direction: MigrationDirection::Up,
        };
        apply_migration_step(&conn, &step).expect("apply routed v33 migration");
        apply_migration_step(&conn, &step).expect("reapply routed v33 migration");
        assert_eq!(get_user_version(&conn).expect("user_version"), 33);
        for column in [
            "delivery_lease_token",
            "delivery_lease_acquired_at",
            "delivery_lease_expires_at",
        ] {
            assert!(
                table_has_column(&conn, "events", column).expect("inspect events schema"),
                "upgrade must add {column}"
            );
        }

        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 33)
            .expect("v33");
        conn.execute_batch(migration.down_sql.expect("v33 down SQL"))
            .expect("roll back v33");
        for column in [
            "delivery_lease_token",
            "delivery_lease_acquired_at",
            "delivery_lease_expires_at",
        ] {
            assert!(
                !table_has_column(&conn, "events", column).expect("inspect rolled-back schema"),
                "rollback must remove {column}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT id FROM events", [], |row| row.get::<_, i64>(0))
                .expect("read event preserved across v33 up/down"),
            7
        );
    }

    #[test]
    fn event_retention_v34_upgrade_starts_a_new_fail_closed_cursor_epoch() {
        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, segment_id INTEGER);
             CREATE TABLE workflow_executions (trigger_event_id INTEGER);
             CREATE TABLE schema_version (
                 version INTEGER NOT NULL,
                 applied_at INTEGER NOT NULL,
                 description TEXT
             );
             INSERT INTO events (id) VALUES (7), (9);
             PRAGMA user_version = 33;",
        )
        .expect("create pre-v34 fixture");
        let step = MigrationStep {
            migration_version: 34,
            resulting_version: 34,
            description: "test v34 retention evidence upgrade",
            direction: MigrationDirection::Up,
        };
        apply_migration_step(&conn, &step).expect("apply v34 migration");

        let state = conn
            .query_row(
                "SELECT cursor_epoch, legacy_history_complete,
                        evidence_from_event_id, max_event_id, generation,
                        deleted_event_count, last_deleted_at
                 FROM event_retention_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                    ))
                },
            )
            .expect("read v34 state");
        assert_eq!(state.0.len(), 32, "epoch is a 128-bit hex token");
        assert_eq!(state.1, 0, "legacy deletion history is unknowable");
        assert_eq!(state.2, 10);
        assert_eq!(state.3, 9);
        assert_eq!((state.4, state.5, state.6), (0, 0, None));
        assert!(
            conn.execute("INSERT INTO events (id) VALUES (8)", [])
                .is_err(),
            "upgraded epoch must reject a non-advancing id"
        );
        conn.execute("INSERT INTO events (id) VALUES (10)", [])
            .expect("first advancing id in new epoch");
        assert!(
            conn.execute(
                "UPDATE event_retention_state SET max_event_id = 9 WHERE singleton = 1",
                [],
            )
            .is_err(),
            "the durable high-water mark must not be rewound"
        );

        let empty = Connection::open_in_memory().expect("open empty v33 sqlite");
        empty
            .execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, segment_id INTEGER);
                 CREATE TABLE workflow_executions (trigger_event_id INTEGER);
                 CREATE TABLE schema_version (
                     version INTEGER NOT NULL,
                     applied_at INTEGER NOT NULL,
                     description TEXT
                 );
                 PRAGMA user_version = 33;",
            )
            .expect("create empty pre-v34 fixture");
        apply_migration_step(&empty, &step).expect("upgrade empty v33 fixture");
        let empty_state = empty
            .query_row(
                "SELECT legacy_history_complete, evidence_from_event_id, max_event_id
                 FROM event_retention_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("read empty-upgrade retention state");
        assert_eq!(
            empty_state,
            (0, 1, 0),
            "an empty v33 table still cannot prove that legacy IDs were never deleted"
        );
    }

    #[test]
    fn event_retention_v34_is_forward_only_and_interval_shape_is_enforced() {
        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 34)
            .expect("v34 migration");
        assert!(migration.down_sql.is_none());
        assert!(build_migration_plan(34, 33).is_err());

        let conn = Connection::open_in_memory().expect("open fresh sqlite");
        initialize_schema(&conn).expect("initialize current schema");
        let state = conn
            .query_row(
                "SELECT length(cursor_epoch), legacy_history_complete,
                        evidence_from_event_id, max_event_id
                 FROM event_retention_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .expect("read fresh state");
        assert_eq!(state, (32, 1, 1, 0));
        assert!(
            conn.execute(
                "UPDATE event_retention_state
                 SET cursor_epoch = 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'
                 WHERE singleton = 1",
                [],
            )
            .is_err(),
            "cursor epochs must remain canonical lowercase 128-bit hex"
        );
        assert!(
            conn.execute(
                "UPDATE event_retention_state
                 SET cursor_epoch = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
                 WHERE singleton = 1",
                [],
            )
            .is_err(),
            "a canonical epoch still cannot change outside an authorized atomic rotation"
        );

        conn.execute(
            "INSERT INTO event_retention_intervals (
                 start_id, end_id, first_generation, last_generation,
                 first_deleted_at, last_deleted_at
             ) VALUES (1, 10, 1, 1, 100, 100)",
            [],
        )
        .expect("insert canonical interval");
        for (start, end) in [(5, 15), (11, 12)] {
            assert!(
                conn.execute(
                    "INSERT INTO event_retention_intervals (
                         start_id, end_id, first_generation, last_generation,
                         first_deleted_at, last_deleted_at
                     ) VALUES (?1, ?2, 2, 2, 101, 101)",
                    rusqlite::params![start, end],
                )
                .is_err(),
                "overlapping or adjacent interval [{start},{end}] must fail"
            );
        }
        assert!(
            conn.execute("DELETE FROM event_retention_intervals", [])
                .is_err(),
            "interval evidence may only change inside an authorized retention transaction"
        );
        assert!(
            conn.execute("DELETE FROM event_retention_state WHERE singleton = 1", [])
                .is_err(),
            "singleton state must be permanent"
        );
    }

    #[test]
    fn fresh_schema_and_v32_upgrade_have_identical_event_column_descriptors_and_order() {
        fn events_table_info(
            conn: &Connection,
        ) -> Vec<(i64, String, String, i64, Option<String>, i64)> {
            let mut stmt = conn
                .prepare("PRAGMA table_info(events)")
                .expect("prepare events table_info");
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .expect("query events table_info");
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .expect("collect events table_info")
        }

        let fresh = Connection::open_in_memory().expect("open fresh sqlite");
        initialize_schema(&fresh).expect("initialize fresh current schema");
        let fresh_descriptor = events_table_info(&fresh);
        let fresh_names = fresh_descriptor
            .iter()
            .map(|descriptor| descriptor.1.as_str())
            .collect::<Vec<_>>();
        assert!(
            fresh_names.ends_with(&[
                "dedupe_key",
                "delivery_lease_token",
                "delivery_lease_acquired_at",
                "delivery_lease_expires_at",
            ]),
            "v33 lease columns must be the append-compatible events-table tail"
        );

        let upgraded = Connection::open_in_memory().expect("open upgrade sqlite");
        initialize_schema(&upgraded).expect("initialize upgrade fixture");
        // v34 is deliberately forward-only because rolling it back would
        // discard deletion evidence.  Construct the empty v32 fixture by
        // removing only the additive v34 objects and applying v33's reversible
        // column teardown, rather than asking the production planner to cross
        // the forward-only boundary.
        upgraded
            .execute_batch(
                "DROP TRIGGER IF EXISTS events_retention_delete_guard;
                 DROP TRIGGER IF EXISTS events_id_update_guard;
                 DROP TRIGGER IF EXISTS events_monotonic_id_advance;
                 DROP TRIGGER IF EXISTS events_monotonic_id_guard;
                 DROP TRIGGER IF EXISTS event_retention_intervals_delete_guard;
                 DROP TRIGGER IF EXISTS event_retention_intervals_update_guard;
                 DROP TRIGGER IF EXISTS event_retention_intervals_insert_guard;
                 DROP TRIGGER IF EXISTS event_retention_state_rotation_clear_intervals;
                 DROP TRIGGER IF EXISTS event_retention_state_rotation_guard;
                 DROP TRIGGER IF EXISTS event_retention_state_monotonic_guard;
                 DROP TRIGGER IF EXISTS event_retention_state_delete_guard;
                 DROP INDEX IF EXISTS idx_events_segment_id;
                 DROP INDEX IF EXISTS idx_workflows_trigger_event_id;
                 DROP TABLE IF EXISTS event_retention_delete_authorizations;
                 DROP TABLE IF EXISTS event_retention_rotation_authorizations;
                 DROP TABLE IF EXISTS event_retention_intervals;
                 DROP TABLE IF EXISTS event_retention_state;",
            )
            .expect("remove additive v34 objects from fixture");
        let v33 = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 33)
            .expect("v33 migration");
        upgraded
            .execute_batch(v33.down_sql.expect("v33 down SQL"))
            .expect("remove v33 columns from fixture");
        set_user_version(&upgraded, 32).expect("stamp v32 fixture");
        for column in [
            "delivery_lease_token",
            "delivery_lease_acquired_at",
            "delivery_lease_expires_at",
        ] {
            assert!(
                !table_has_column(&upgraded, "events", column).expect("inspect v32 fixture schema"),
                "v32 fixture must omit {column}"
            );
        }

        let up = build_migration_plan(32, SCHEMA_VERSION)
            .expect("build v32 -> current-head upgrade plan");
        apply_migration_plan(&upgraded, &up).expect("upgrade fixture to current head");
        ensure_event_delivery_lease_schema(&upgraded)
            .expect("guarded v33 must be idempotent after upgrade");

        assert_eq!(
            get_user_version(&fresh).expect("fresh user_version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            get_user_version(&upgraded).expect("upgraded user_version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            events_table_info(&upgraded),
            fresh_descriptor,
            "fresh and v32-upgraded events tables must have identical PRAGMA descriptors and order"
        );
    }

    #[test]
    fn checkpoint_authority_v36_classifies_and_backfills_deterministically_once() {
        let conn = Connection::open_in_memory().expect("open v35 fixture");
        create_v35_checkpoint_fixture(&conn);
        conn.execute_batch(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, ft_version
             ) VALUES ('session-a', 1, 200, 0, '{\"tab_order\":[2,1]}', 'test');
             INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json
             ) VALUES
                 (-4, 'session-a',  50, 'startup',  'ffffffffffffffff', 0, 0,
                      '{\"old_to_new\":{},\"unexpected\":true}'),
                 (-3, 'session-a',  60, 'startup',  'eeeeeeeeeeeeeeee', 2, 0,
                      '{\"old_to_new\":{\"7\":8,\"07\":9}}'),
                 (-2, 'session-a',  70, 'startup',  'dddddddddddddddd', 2, 0,
                      '{\"old_to_new\":{\"7\":8,\"7\":9}}'),
                 (-1, 'session-a',  80, 'startup',  'restore',          0, 0,
                      '{\"old_to_new\":{}}'),
                 ( 0, 'session-a',  90, 'startup',  '0000000000000000', 0, 0,
                      NULL),
                 ( 1, 'session-a', 100, 'startup',  '1111111111111111', 1, 0,
                      '{\"old_to_new\":{\"7\":8}}'),
                 ( 2, 'session-a', 150, 'startup',  '2222222222222222', 1, 2,
                      NULL),
                 ( 3, 'session-a', 200, 'periodic', '3333333333333333', 1, 3,
                      NULL),
                 ( 4, 'session-a', 200, 'event',    '4444444444444444', 1, 4,
                      NULL),
                 ( 5, 'session-a',  80, 'startup',  'restore',          0, 0,
                      '{\"old_to_new\":{}}');
             INSERT INTO mux_pane_state (checkpoint_id, pane_id)
             VALUES (2, 20), (3, 30), (4, 40);",
        )
        .expect("seed pre-v36 checkpoint history");

        ensure_checkpoint_snapshot_authority_schema(&conn).expect("apply v36 authority schema");
        let rows = conn
            .prepare(
                "SELECT id, checkpoint_role, topology_json, state_hash
                 FROM session_checkpoints ORDER BY id",
            )
            .expect("prepare upgraded checkpoint query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("query upgraded checkpoints")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect upgraded checkpoints");
        assert_eq!(
            rows,
            vec![
                (
                    -4,
                    "snapshot".to_string(),
                    None,
                    "ffffffffffffffff".to_string(),
                ),
                (
                    -3,
                    "snapshot".to_string(),
                    None,
                    "eeeeeeeeeeeeeeee".to_string(),
                ),
                (
                    -2,
                    "snapshot".to_string(),
                    None,
                    "dddddddddddddddd".to_string(),
                ),
                (-1, "snapshot".to_string(), None, "restore".to_string(),),
                (
                    0,
                    "snapshot".to_string(),
                    None,
                    "0000000000000000".to_string(),
                ),
                (
                    1,
                    "restore_receipt".to_string(),
                    None,
                    "1111111111111111".to_string(),
                ),
                (
                    2,
                    "snapshot".to_string(),
                    None,
                    "2222222222222222".to_string(),
                ),
                (
                    3,
                    "snapshot".to_string(),
                    None,
                    "3333333333333333".to_string(),
                ),
                (
                    4,
                    "snapshot".to_string(),
                    Some("{\"tab_order\":[2,1]}".to_string()),
                    "4444444444444444".to_string(),
                ),
                (
                    5,
                    "restore_receipt".to_string(),
                    None,
                    "restore".to_string(),
                ),
            ],
            "only structurally proven legacy receipts are classified; the deterministic newest snapshot receives topology and witnesses stay byte-identical"
        );

        conn.execute(
            "UPDATE mux_sessions SET topology_json = '{\"tab_order\":[9]}'
             WHERE session_id = 'session-a'",
            [],
        )
        .expect("change mutable session-level latest topology");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 6, 'session-a', 300, 'startup', '5555555555555555',
                 0, 0, 'snapshot', '{\"explicit_empty\":true}'
             )",
            [],
        )
        .expect("insert an explicit post-v36 empty snapshot");
        ensure_checkpoint_snapshot_authority_schema(&conn)
            .expect("v36 authority repair must be idempotent");

        assert_eq!(
            conn.query_row(
                "SELECT checkpoint_role, topology_json
                 FROM session_checkpoints WHERE id = 4",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("read retained historical topology"),
            (
                "snapshot".to_string(),
                Some("{\"tab_order\":[2,1]}".to_string())
            ),
            "rerunning v36 must not overwrite already-bound row-local topology"
        );
        assert_eq!(
            conn.query_row(
                "SELECT checkpoint_role, topology_json
                 FROM session_checkpoints WHERE id = 6",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("read explicit empty snapshot"),
            (
                "snapshot".to_string(),
                Some("{\"explicit_empty\":true}".to_string())
            ),
            "an explicit post-v36 empty snapshot must never be reclassified as bookkeeping"
        );
        assert!(
            conn.execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, checkpoint_role
                 ) VALUES ('session-a', 400, 'event', '6666666666666666', 0, 0, 'other')",
                [],
            )
            .is_err(),
            "the canonical v36 CHECK must reject unknown checkpoint roles"
        );
    }

    #[test]
    fn checkpoint_authority_v36_to_v38_fresh_and_upgrade_shapes_are_identical() {
        let fresh = Connection::open_in_memory().expect("open fresh current fixture");
        initialize_schema(&fresh).expect("initialize fresh current schema");

        let upgraded = Connection::open_in_memory().expect("open v35 upgrade fixture");
        create_v35_checkpoint_fixture(&upgraded);
        let plan = build_migration_plan(35, 38).expect("build v35-to-v38 authority plan");
        apply_migration_plan(&upgraded, &plan).expect("apply v36-to-v38 authority migrations");
        assert_eq!(
            get_user_version(&upgraded).expect("upgraded user version"),
            38
        );

        assert_eq!(
            table_descriptors(&upgraded, "session_checkpoints"),
            table_descriptors(&fresh, "session_checkpoints"),
            "fresh and upgraded checkpoint column descriptors/order must be exact"
        );
        assert_eq!(
            table_descriptors(&upgraded, "mux_sessions"),
            table_descriptors(&fresh, "mux_sessions"),
            "fresh and upgraded session column descriptors/order must be exact"
        );
        for index_name in [
            "idx_checkpoints_session_role_latest",
            "idx_checkpoints_session_role_causal",
            "idx_checkpoints_global_latest",
            "idx_checkpoints_global_snapshot_latest",
            "idx_checkpoints_restore_intent_outcome",
        ] {
            assert_eq!(
                compact_schema_sql(
                    &load_schema_object_sql(&upgraded, "index", index_name)
                        .expect("read upgraded v36 index")
                        .expect("upgraded v36 index")
                ),
                compact_schema_sql(
                    &load_schema_object_sql(&fresh, "index", index_name)
                        .expect("read fresh v36 index")
                        .expect("fresh v36 index")
                ),
                "fresh and upgraded v36 index {index_name} must be identical"
            );
        }
        assert_eq!(
            compact_schema_sql(
                &load_schema_object_sql(&upgraded, "index", "idx_mux_sessions_clean_checkpoint",)
                    .expect("read upgraded v37 index")
                    .expect("upgraded v37 index")
            ),
            compact_schema_sql(
                &load_schema_object_sql(&fresh, "index", "idx_mux_sessions_clean_checkpoint",)
                    .expect("read fresh v37 index")
                    .expect("fresh v37 index")
            )
        );
        validate_checkpoint_snapshot_authority_schema(&fresh)
            .expect("fresh v36 CHECK and index semantics");
        validate_checkpoint_snapshot_authority_schema(&upgraded)
            .expect("upgraded v36 CHECK and index semantics");
        validate_clean_checkpoint_receipt_schema(&fresh).expect("fresh v37 FK and index semantics");
        validate_clean_checkpoint_receipt_schema(&upgraded)
            .expect("upgraded v37 FK and index semantics");
        validate_checkpoint_identity_v38_schema(&fresh)
            .expect("fresh v38 identity and lifecycle semantics");
        validate_checkpoint_identity_v38_schema(&upgraded)
            .expect("upgraded v38 identity and lifecycle semantics");
    }

    #[test]
    fn checkpoint_authority_malformed_columns_fail_closed_and_indexes_are_replaced() {
        let malformed_column = Connection::open_in_memory().expect("open malformed fixture");
        create_v35_checkpoint_fixture(&malformed_column);
        malformed_column
            .execute_batch(
                "ALTER TABLE session_checkpoints
                     ADD COLUMN checkpoint_role INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session_checkpoints ADD COLUMN topology_json TEXT;",
            )
            .expect("seed malformed checkpoint role descriptor");
        let error = ensure_checkpoint_snapshot_authority_schema(&malformed_column)
            .expect_err("a pre-existing malformed authority column must fail closed");
        assert!(error.to_string().contains("non-canonical"));

        let missing_check = Connection::open_in_memory().expect("open missing-CHECK fixture");
        create_v35_checkpoint_fixture(&missing_check);
        missing_check
            .execute_batch(
                "ALTER TABLE session_checkpoints
                     ADD COLUMN checkpoint_role TEXT NOT NULL DEFAULT 'snapshot';
                 ALTER TABLE session_checkpoints ADD COLUMN topology_json TEXT;",
            )
            .expect("seed role column without its canonical CHECK");
        let error = ensure_checkpoint_snapshot_authority_schema(&missing_check)
            .expect_err("a role column without its CHECK must fail closed");
        assert!(error.to_string().contains("canonical constraint"));

        let malformed_clean_column =
            Connection::open_in_memory().expect("open malformed clean-column fixture");
        create_v35_checkpoint_fixture(&malformed_clean_column);
        ensure_checkpoint_snapshot_authority_schema(&malformed_clean_column)
            .expect("apply v36 before malformed v37 column");
        malformed_clean_column
            .execute_batch("ALTER TABLE mux_sessions ADD COLUMN clean_checkpoint_id TEXT;")
            .expect("seed malformed clean checkpoint descriptor");
        let error = ensure_clean_checkpoint_receipt_schema(&malformed_clean_column)
            .expect_err("a malformed clean-checkpoint descriptor must fail closed");
        assert!(error.to_string().contains("non-canonical"));

        let missing_foreign_key =
            Connection::open_in_memory().expect("open missing-clean-FK fixture");
        create_v35_checkpoint_fixture(&missing_foreign_key);
        ensure_checkpoint_snapshot_authority_schema(&missing_foreign_key)
            .expect("apply v36 before missing v37 FK");
        missing_foreign_key
            .execute_batch("ALTER TABLE mux_sessions ADD COLUMN clean_checkpoint_id INTEGER;")
            .expect("seed clean checkpoint column without its canonical FK");
        let error = ensure_clean_checkpoint_receipt_schema(&missing_foreign_key)
            .expect_err("a clean-checkpoint column without its FK must fail closed");
        assert!(error.to_string().contains("foreign keys"));

        let malformed_indexes = Connection::open_in_memory().expect("open malformed indexes");
        create_v35_checkpoint_fixture(&malformed_indexes);
        malformed_indexes
            .execute_batch(
                "ALTER TABLE session_checkpoints
                     ADD COLUMN checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                     CHECK(checkpoint_role IN ('snapshot','restore_receipt'));
                 ALTER TABLE session_checkpoints ADD COLUMN topology_json TEXT;
                 CREATE INDEX idx_checkpoints_session_role_latest
                     ON session_checkpoints(id);
                 CREATE INDEX idx_checkpoints_global_latest
                     ON session_checkpoints(session_id);
                 CREATE INDEX idx_checkpoints_global_snapshot_latest
                     ON session_checkpoints(checkpoint_role);",
            )
            .expect("seed malformed same-name v36 index");
        ensure_checkpoint_snapshot_authority_schema(&malformed_indexes)
            .expect("v36 must replace only the malformed same-name indexes");
        for (index_name, canonical_sql) in [
            (
                "idx_checkpoints_session_role_latest",
                CHECKPOINT_ROLE_LATEST_INDEX_SQL,
            ),
            (
                "idx_checkpoints_global_latest",
                CHECKPOINT_GLOBAL_LATEST_INDEX_SQL,
            ),
            (
                "idx_checkpoints_global_snapshot_latest",
                CHECKPOINT_GLOBAL_SNAPSHOT_LATEST_INDEX_SQL,
            ),
        ] {
            assert_eq!(
                compact_schema_sql(
                    &load_schema_object_sql(&malformed_indexes, "index", index_name)
                        .expect("read repaired v36 index")
                        .expect("repaired v36 index")
                ),
                compact_schema_sql(canonical_sql),
                "v36 must install exact deterministic order for {index_name}"
            );
        }

        malformed_indexes
            .execute_batch(
                "DROP INDEX idx_checkpoints_global_latest;
                 CREATE INDEX idx_checkpoints_global_latest
                     ON session_checkpoints(session_id);",
            )
            .expect("reseed malformed v36 prerequisite index before v37");
        ensure_clean_checkpoint_receipt_schema(&malformed_indexes).expect("apply v37");
        assert_eq!(
            compact_schema_sql(
                &load_schema_object_sql(
                    &malformed_indexes,
                    "index",
                    "idx_checkpoints_global_latest",
                )
                .expect("read v37-repaired prerequisite index")
                .expect("v37-repaired prerequisite index")
            ),
            compact_schema_sql(CHECKPOINT_GLOBAL_LATEST_INDEX_SQL),
            "v37 must idempotently repair exact v36 indexes without replaying history"
        );
        malformed_indexes
            .execute_batch(
                "DROP INDEX idx_mux_sessions_clean_checkpoint;
                 CREATE INDEX idx_mux_sessions_clean_checkpoint
                     ON mux_sessions(session_id);",
            )
            .expect("seed malformed same-name v37 index");
        ensure_clean_checkpoint_receipt_schema(&malformed_indexes)
            .expect("v37 must replace only the malformed same-name index");
        assert_eq!(
            compact_schema_sql(
                &load_schema_object_sql(
                    &malformed_indexes,
                    "index",
                    "idx_mux_sessions_clean_checkpoint",
                )
                .expect("read repaired v37 index")
                .expect("repaired v37 index")
            ),
            compact_schema_sql(CLEAN_CHECKPOINT_INDEX_SQL)
        );
    }

    #[test]
    fn current_checkpoint_authority_schema_drift_fails_closed_without_mutation() {
        let conn = Connection::open_in_memory().expect("open current fixture");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch(
            "DROP INDEX idx_checkpoints_session_role_latest;
             CREATE INDEX idx_checkpoints_session_role_latest
                 ON session_checkpoints(id);",
        )
        .expect("seed current-schema index drift");

        let error =
            initialize_schema(&conn).expect_err("current-version authority drift must fail closed");
        assert!(error.to_string().contains("non-canonical index"));
        let still_drifted = compact_schema_sql(
            &load_schema_object_sql(&conn, "index", "idx_checkpoints_session_role_latest")
                .expect("inspect drifted current index")
                .expect("drifted current index"),
        );
        assert!(
            still_drifted.contains("onsession_checkpoints(id)"),
            "current-schema validation must not silently mutate authority: {still_drifted}"
        );

        let extra_lifecycle_column =
            Connection::open_in_memory().expect("open lifecycle-drift fixture");
        initialize_schema(&extra_lifecycle_column).expect("initialize lifecycle-drift fixture");
        extra_lifecycle_column
            .execute_batch(
                "ALTER TABLE restore_attempt_lifecycle
                 ADD COLUMN unaudited_state TEXT;",
            )
            .expect("seed unaudited lifecycle column");
        let error = initialize_schema(&extra_lifecycle_column)
            .expect_err("current lifecycle column drift must fail closed");
        assert!(error.to_string().contains("non-canonical"));
    }

    #[test]
    fn clean_checkpoint_v37_never_synthesizes_or_rebinds_authority() {
        let conn = Connection::open_in_memory().expect("open v36 fixture");
        create_v35_checkpoint_fixture(&conn);
        ensure_checkpoint_snapshot_authority_schema(&conn).expect("apply v36");
        conn.execute_batch(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, ft_version
             ) VALUES
                 ('empty', 1, NULL, 1, '{}', 'test'),
                 ('a', 1, 100, 1, '{}', 'test'),
                 ('b', 1, NULL, 1, '{}', 'test');
             INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (1, 'a', 100, 'shutdown', 'aaaaaaaaaaaaaaaa', 0, 0,
                       'snapshot', '{}');",
        )
        .expect("seed legacy clean booleans");

        ensure_clean_checkpoint_receipt_schema(&conn).expect("apply fail-safe v37");
        let initial = conn
            .prepare(
                "SELECT session_id, shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions ORDER BY session_id",
            )
            .expect("prepare initial clean-state query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })
            .expect("query initial clean state")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect initial clean state");
        assert_eq!(
            initial,
            vec![
                ("a".to_string(), 0, None),
                ("b".to_string(), 0, None),
                ("empty".to_string(), 0, None),
            ],
            "v37 cannot prove a legacy receipt and must never synthesize one"
        );

        conn.execute_batch(
            "UPDATE mux_sessions
             SET shutdown_clean = 1, clean_checkpoint_id = 1
             WHERE session_id IN ('a', 'b');",
        )
        .expect("seed one exact and one cross-session receipt pointer");
        ensure_clean_checkpoint_receipt_schema(&conn).expect("revalidate v37 receipt pointers");
        assert_eq!(
            conn.query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'a'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("read valid exact receipt"),
            (1, Some(1))
        );
        assert_eq!(
            conn.query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'b'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("read rejected cross-session receipt"),
            (0, None)
        );

        conn.execute(
            "INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (2, 'a', 100, 'shutdown', 'bbbbbbbbbbbbbbbb', 0, 0,
                       'snapshot', '{}')",
            [],
        )
        .expect("insert same-time checkpoint with a larger deterministic ID");
        ensure_clean_checkpoint_receipt_schema(&conn)
            .expect("invalidate a pointer displaced by the ID tie-break");
        assert_eq!(
            conn.query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'a'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("read tie-displaced receipt state"),
            (0, None),
            "the larger checkpoint ID wins an equal-timestamp latest tie"
        );

        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1, clean_checkpoint_id = 2
             WHERE session_id = 'a'",
            [],
        )
        .expect("bind the new deterministic latest receipt");
        conn.execute("DELETE FROM session_checkpoints WHERE id = 2", [])
            .expect("delete exact clean checkpoint receipt");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (3, 'a', 200, 'shutdown', 'cccccccccccccccc', 0, 0,
                       'snapshot', '{}')",
            [],
        )
        .expect("insert a later unrelated checkpoint after deletion");
        ensure_clean_checkpoint_receipt_schema(&conn)
            .expect("rerun v37 after clean receipt deletion");
        assert_eq!(
            conn.query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'a'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("read post-deletion state"),
            (0, None),
            "rerunning v37 must not bind a replacement checkpoint"
        );

        conn.execute_batch(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, ft_version
             ) VALUES ('cascade', 1, 300, 0, '{}', 'test');
             INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (4, 'cascade', 300, 'shutdown', 'dddddddddddddddd', 0, 0,
                       'snapshot', '{}');
             UPDATE mux_sessions
             SET shutdown_clean = 1, clean_checkpoint_id = 4
             WHERE session_id = 'cascade';",
        )
        .expect("seed circular-FK cascade case");
        conn.execute("DELETE FROM mux_sessions WHERE session_id = 'cascade'", [])
            .expect("session deletion must cascade through its bound checkpoint");
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM session_checkpoints WHERE id = 4",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count cascaded checkpoint"),
            0
        );
    }

    #[test]
    fn checkpoint_identity_v38_preserves_rows_relations_and_high_water() {
        let conn = Connection::open_in_memory().expect("open v37 fixture");
        create_v37_checkpoint_fixture(&conn);
        conn.execute_batch(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, ft_version, clean_checkpoint_id
             ) VALUES ('v38-session', 1, NULL, 0, '{}', 'test', NULL);
             INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 topology_json
             ) VALUES
                 (7, 'v38-session', 100, 'periodic', '7777777777777777',
                  1, 2, '{\"source\":\"snapshot\"}', 'snapshot', '{}'),
                 (42, 'v38-session', 90, 'startup', 'restore',
                  0, 0, '{\"old_to_new\":{}}', 'restore_receipt', NULL);
             INSERT INTO mux_pane_state (checkpoint_id, pane_id)
             VALUES (7, 700);
             UPDATE mux_sessions
             SET shutdown_clean = 1,
                 last_checkpoint_at = 90,
                 clean_checkpoint_id = 42
             WHERE session_id = 'v38-session';",
        )
        .expect("seed nonempty v37 authority graph");

        let plan = build_migration_plan(37, 38).expect("build v38 migration plan");
        apply_migration_plan(&conn, &plan).expect("apply v38 migration");
        assert_eq!(get_user_version(&conn).expect("read v38 user_version"), 38);
        assert!(migration_foreign_keys_enabled(&conn).expect("read restored FK mode"));
        validate_checkpoint_identity_v38_schema(&conn).expect("validate v38 authority schema");
        ensure_no_foreign_key_violations(&conn, "v38 preservation regression")
            .expect("preserved graph has no FK violations");

        let preserved_rows = conn
            .prepare(
                "SELECT id, session_id, checkpoint_at, checkpoint_type, state_hash,
                        pane_count, total_bytes, metadata_json, checkpoint_role,
                        topology_json, restore_intent_checkpoint_id
                 FROM session_checkpoints ORDER BY id",
            )
            .expect("prepare preserved checkpoint query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            })
            .expect("query preserved checkpoints")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect preserved checkpoints");
        assert_eq!(preserved_rows.len(), 2);
        assert_eq!(preserved_rows[0].0, 7);
        assert_eq!(
            preserved_rows[0].7.as_deref(),
            Some("{\"source\":\"snapshot\"}")
        );
        assert_eq!(preserved_rows[0].9.as_deref(), Some("{}"));
        assert_eq!(preserved_rows[0].10, None);
        assert_eq!(preserved_rows[1].0, 42);
        assert_eq!(preserved_rows[1].8, "restore_receipt");
        assert_eq!(preserved_rows[1].10, None);
        assert_eq!(
            conn.query_row(
                "SELECT clean_checkpoint_id FROM mux_sessions
                 WHERE session_id = 'v38-session'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .expect("read preserved clean pointer"),
            Some(42)
        );
        assert_eq!(
            conn.query_row(
                "SELECT checkpoint_id FROM mux_pane_state WHERE pane_id = 700",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("read preserved pane relation"),
            7
        );

        conn.execute("DELETE FROM session_checkpoints WHERE id = 42", [])
            .expect("delete prior high-water row");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 'v38-session', 80, 'event', 'aaaaaaaaaaaaaaaa',
                 0, 0, 'snapshot', '{}'
             )",
            [],
        )
        .expect("allocate after deleted high-water row");
        let first_new_id = conn.last_insert_rowid();
        assert!(
            first_new_id > 42,
            "AUTOINCREMENT must not reuse deleted id 42"
        );
        conn.execute(
            "DELETE FROM session_checkpoints WHERE id = ?1",
            [first_new_id],
        )
        .expect("delete newly allocated high-water row");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 'v38-session', 70, 'event', 'bbbbbbbbbbbbbbbb',
                 0, 0, 'snapshot', '{}'
             )",
            [],
        )
        .expect("allocate after second high-water deletion");
        assert!(conn.last_insert_rowid() > first_new_id);

        ensure_checkpoint_identity_v38_schema(&conn).expect("v38 helper is idempotent");
    }

    #[test]
    fn checkpoint_identity_v38_enforces_unique_intent_outcome_links_and_cascades() {
        let conn = Connection::open_in_memory().expect("open v37 fixture");
        create_v37_checkpoint_fixture(&conn);
        conn.execute(
            "INSERT INTO mux_sessions (
                 session_id, created_at, shutdown_clean, topology_json, ft_version
             ) VALUES ('attempt-session', 1, 0, '{}', 'test')",
            [],
        )
        .expect("seed attempt session");
        apply_migration_plan(
            &conn,
            &build_migration_plan(37, 38).expect("build v38 migration plan"),
        )
        .expect("apply v38 migration");

        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 'attempt-session', 9, 'periodic', 'aaaaaaaaaaaaaaaa',
                 0, 0, 'snapshot', '{}'
             )",
            [],
        )
        .expect("insert restore source snapshot");
        let source_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (
                 'attempt-session', 10, 'startup', 'pending:rsi2',
                 0, 0, '{\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent'
             )",
            [],
        )
        .expect("insert restore intent");
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at
             ) VALUES (?1, 'attempt-session', ?2, 'intent', 10)",
            rusqlite::params![intent_id, source_id],
        )
        .expect("insert authoritative intent lifecycle");
        assert!(
            conn.execute(
                "UPDATE restore_attempt_lifecycle
                 SET status = 'outcome_complete'
                 WHERE intent_checkpoint_id = ?1",
                [intent_id],
            )
            .is_err(),
            "outcome_complete requires an exact outcome checkpoint identity"
        );
        assert!(
            conn.execute(
                "UPDATE restore_attempt_lifecycle
                 SET status = 'resolved'
                 WHERE intent_checkpoint_id = ?1",
                [intent_id],
            )
            .is_err(),
            "resolved requires an explicit resolution timestamp"
        );
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 restore_intent_checkpoint_id
             ) VALUES (
                 'attempt-session', 11, 'startup', 'pending:rst2',
                 0, 0, '{\"old_to_new\":{}}', 'restore_receipt', ?1
             )",
            [intent_id],
        )
        .expect("insert uniquely linked outcome");
        let outcome_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE restore_attempt_lifecycle
             SET outcome_checkpoint_id = ?2,
                 status = 'outcome_complete'
             WHERE intent_checkpoint_id = ?1",
            rusqlite::params![intent_id, outcome_id],
        )
        .expect("advance lifecycle to incomplete clean-binding outcome");
        assert_eq!(
            conn.query_row(
                "SELECT status FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [intent_id],
                |row| row.get::<_, String>(0),
            )
            .expect("read outcome lifecycle"),
            "outcome_complete"
        );
        assert!(
            conn.execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, metadata_json, checkpoint_role,
                     restore_intent_checkpoint_id
                 ) VALUES (
                     'attempt-session', 12, 'startup', 'pending:rst2',
                     0, 0, '{\"old_to_new\":{}}', 'restore_receipt', ?1
                 )",
                [intent_id],
            )
            .is_err(),
            "one intent must not acquire two durable outcomes"
        );
        assert!(
            conn.execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, checkpoint_role,
                     restore_intent_checkpoint_id
                 ) VALUES (
                     'attempt-session', 13, 'event', 'cccccccccccccccc',
                     0, 0, 'snapshot', ?1
                 )",
                [intent_id],
            )
            .is_err(),
            "snapshots cannot masquerade as restore outcomes"
        );
        conn.execute(
            "UPDATE restore_attempt_lifecycle
             SET status = 'resolved', resolved_at = 14
             WHERE intent_checkpoint_id = ?1
               AND outcome_checkpoint_id = ?2",
            rusqlite::params![intent_id, outcome_id],
        )
        .expect("durably resolve restore attempt");
        conn.execute(
            "DELETE FROM mux_sessions WHERE session_id = 'attempt-session'",
            [],
        )
        .expect("session cascade removes linked intent and outcome together");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count cascaded restore-attempt rows"),
            0
        );
    }

    #[test]
    fn checkpoint_identity_v38_backfills_legacy_intent_as_reconciliation_required() {
        let conn = Connection::open_in_memory().expect("open v37 fixture");
        create_v37_checkpoint_fixture(&conn);
        let source_hash = format!("snp2:{}", "a".repeat(64));
        let intent_hash = format!("rst2:{}", "b".repeat(64));
        let intent_metadata = format!(
            "{{\"old_to_new\":{{}},\"restore_attempt\":{{\"phase\":\"intent\",\"source_checkpoint_at\":10,\"source_checkpoint_id\":5,\"source_checkpoint_role\":\"snapshot\",\"source_state_hash\":\"{source_hash}\"}}}}"
        );
        conn.execute(
            "INSERT INTO mux_sessions (
                 session_id, created_at, shutdown_clean, topology_json, ft_version
             ) VALUES ('legacy-attempt', 1, 0, '{}', 'test')",
            [],
        )
        .expect("seed legacy attempt session");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 5, 'legacy-attempt', 10, 'periodic', ?1,
                 0, 0, 'snapshot', '{}'
             )",
            [source_hash.as_str()],
        )
        .expect("seed legacy attempt source");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 topology_json
             ) VALUES (
                 6, 'legacy-attempt', 11, 'startup', ?1,
                 0, 0, ?2, 'restore_receipt', NULL
             )",
            rusqlite::params![intent_hash, intent_metadata],
        )
        .expect("seed overloaded v37 intent row");

        apply_migration_plan(
            &conn,
            &build_migration_plan(37, 38).expect("build v38 migration plan"),
        )
        .expect("migrate legacy intent");
        assert_eq!(
            conn.query_row(
                "SELECT session_id, source_checkpoint_id, outcome_checkpoint_id,
                        status, created_at, resolved_at
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = 6",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .expect("read backfilled lifecycle"),
            (
                "legacy-attempt".to_string(),
                5,
                None,
                "reconciliation_required".to_string(),
                11,
                None,
            ),
            "migration must never infer resolution from latest-row or clean-pointer state"
        );
    }

    #[test]
    fn checkpoint_identity_v38_mutation_failure_rolls_back_and_restores_fk_mode() {
        let conn = Connection::open_in_memory().expect("open v37 fixture");
        create_v37_checkpoint_fixture(&conn);
        conn.execute_batch(
            "INSERT INTO mux_sessions (
                 session_id, created_at, shutdown_clean, topology_json, ft_version
             ) VALUES ('rollback-session', 1, 0, '{}', 'test');
             INSERT INTO session_checkpoints (
                 id, session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role, topology_json
             ) VALUES (
                 9, 'rollback-session', 1, 'event', '9999999999999999',
                 0, 0, 'snapshot', '{}'
             );",
        )
        .expect("seed rollback fixture");
        let step = build_migration_plan(37, 38)
            .expect("build v38 plan")
            .steps
            .into_iter()
            .next()
            .expect("v38 step");
        set_migration_transaction_fault_for_test(Some(MigrationTransactionFault::MutationPanic));
        let error = apply_migration_step(&conn, &step)
            .expect_err("synthetic post-mutation panic must roll v38 back");
        assert!(error.to_string().contains("migration mutation panicked"));
        assert_eq!(
            get_user_version(&conn).expect("rolled-back user_version"),
            37
        );
        assert!(migration_foreign_keys_enabled(&conn).expect("restored FK mode"));
        assert!(
            !checkpoint_v38_table_has_identity_surface(&conn)
                .expect("inspect rolled-back checkpoint table"),
            "rolled-back v37 table must not expose a partial v38 identity surface"
        );
        assert_eq!(
            conn.query_row(
                "SELECT id FROM session_checkpoints WHERE session_id = 'rollback-session'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("preserved source row"),
            9
        );
        assert!(
            !table_exists(&conn, "session_checkpoints_v38")
                .expect("inspect rolled-back staging table")
        );
        assert!(
            !table_exists(&conn, "restore_attempt_lifecycle")
                .expect("inspect rolled-back lifecycle table")
        );
    }

    #[test]
    fn checkpoint_scrollback_and_size_authority_v36_through_v40_are_forward_only() {
        for version in [36, 37, 38, 39, 40] {
            let migration = MIGRATIONS
                .iter()
                .find(|migration| migration.version == version)
                .expect("authority migration must exist");
            assert!(
                migration.down_sql.is_none(),
                "authority migration v{version} must remain forward-only"
            );
        }
        assert!(build_migration_plan(36, 35).is_err());
        assert!(build_migration_plan(37, 36).is_err());
        assert!(build_migration_plan(37, 35).is_err());
        assert!(build_migration_plan(38, 37).is_err());
        assert!(build_migration_plan(38, 35).is_err());
        assert!(build_migration_plan(39, 38).is_err());
        assert!(build_migration_plan(39, 35).is_err());
        assert!(build_migration_plan(40, 39).is_err());
        assert!(build_migration_plan(40, 35).is_err());
    }

    #[test]
    fn hot_path_index_v35_fresh_upgrade_and_downgrade_are_exact() {
        fn normalized_index_sql(conn: &Connection, name: &str) -> Option<String> {
            let sql = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
                    [name],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .expect("inspect sqlite index definition")?;
            Some(
                sql.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_ascii_lowercase()
                    .replace("if not exists ", ""),
            )
        }

        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 35)
            .expect("v35 migration");
        assert!(migration.down_sql.is_some(), "v35 must remain reversible");

        let fresh = Connection::open_in_memory().expect("open fresh sqlite");
        initialize_schema(&fresh).expect("initialize fresh current schema");
        let fresh_unhandled = normalized_index_sql(&fresh, "idx_events_unhandled_detected")
            .expect("fresh schema newest-first unhandled index");
        let fresh_unhandled_cursor = normalized_index_sql(&fresh, "idx_events_unhandled_id")
            .expect("fresh schema ascending unhandled-cursor index");
        let fresh_unhandled_pane = normalized_index_sql(&fresh, "idx_events_unhandled_pane")
            .expect("fresh schema pane-grouped unhandled index");
        let fresh_pane_activity = normalized_index_sql(&fresh, "idx_segments_pane_captured")
            .expect("fresh schema pane-activity index");
        assert_eq!(
            normalized_index_sql(&fresh, "idx_events_unhandled"),
            None,
            "fresh v35 schema must not retain the constant-key v34 index"
        );

        let upgraded = Connection::open_in_memory().expect("open upgrade sqlite");
        initialize_schema(&upgraded).expect("initialize current upgrade fixture");
        let down = build_migration_plan(35, 34).expect("build reversible v35 down plan");
        apply_migration_plan(&upgraded, &down).expect("apply v35 down migration");
        assert_eq!(get_user_version(&upgraded).expect("v34 user_version"), 34);
        assert!(normalized_index_sql(&upgraded, "idx_events_unhandled").is_some());
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_detected"),
            None
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_id"),
            None
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_pane"),
            None
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_segments_pane_captured"),
            None
        );

        // Same-name malformed indexes can be left by an interrupted manual
        // repair or a pre-release build. v35 deliberately replaces, rather
        // than preserves, those definitions.
        upgraded
            .execute_batch(
                "CREATE INDEX idx_events_unhandled_detected ON events(id);
                 CREATE INDEX idx_events_unhandled_id ON events(detected_at);
                 CREATE INDEX idx_events_unhandled_pane ON events(detected_at);
                 CREATE INDEX idx_segments_pane_captured ON output_segments(captured_at);",
            )
            .expect("seed malformed same-name v35 indexes");
        let up = build_migration_plan(34, 35).expect("build v34 to v35 plan");
        apply_migration_plan(&upgraded, &up).expect("apply v35 upgrade");
        assert_eq!(get_user_version(&upgraded).expect("v35 user_version"), 35);
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_detected"),
            Some(fresh_unhandled),
            "upgraded and fresh unhandled indexes must be identical"
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_id"),
            Some(fresh_unhandled_cursor),
            "upgraded and fresh unhandled cursor indexes must be identical"
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_pane"),
            Some(fresh_unhandled_pane),
            "upgraded and fresh pane-grouped unhandled indexes must be identical"
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_segments_pane_captured"),
            Some(fresh_pane_activity),
            "upgraded and fresh pane-activity indexes must be identical"
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled"),
            None
        );

        let down_again = build_migration_plan(35, 34).expect("rebuild v35 down plan");
        apply_migration_plan(&upgraded, &down_again).expect("reapply v35 down migration");
        assert_eq!(
            get_user_version(&upgraded).expect("restored v34 user_version"),
            34
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled"),
            Some(
                "create index idx_events_unhandled on events(handled_at) where handled_at is null"
                    .to_string()
            )
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_detected"),
            None
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_id"),
            None
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_events_unhandled_pane"),
            None
        );
        assert_eq!(
            normalized_index_sql(&upgraded, "idx_segments_pane_captured"),
            None
        );
    }

    #[test]
    fn zone_type_migration_entry_present_at_version_31() {
        let m31 = MIGRATIONS
            .iter()
            .find(|m| m.version == 31)
            .expect("version 31 migration must be registered");
        assert!(
            m31.description.contains("semantic zone type"),
            "description must reference semantic zone type metadata, got: {:?}",
            m31.description,
        );
        assert!(
            m31.up_sql
                .contains("ALTER TABLE output_segments ADD COLUMN zone_type"),
            "up_sql must add the zone_type column",
        );
        assert!(
            m31.up_sql.contains("idx_segments_zone_type"),
            "up_sql must create the zone_type index",
        );
        let down = m31.down_sql.expect("down_sql must be supported");
        assert!(down.contains("DROP INDEX IF EXISTS idx_segments_zone_type"));
        assert!(down.contains("DROP COLUMN zone_type"));
    }

    #[test]
    fn zone_type_migration_adds_indexed_nullable_column() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        conn.execute_batch(
            "CREATE TABLE output_segments (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_len INTEGER NOT NULL,
                content_hash TEXT,
                captured_at INTEGER NOT NULL,
                redaction_catalog_version TEXT,
                UNIQUE(pane_id, seq)
            );",
        )
        .expect("base output_segments");

        let m31 = MIGRATIONS.iter().find(|m| m.version == 31).expect("v31");
        conn.execute_batch(m31.up_sql).expect("apply v31");
        assert!(
            output_segments_has_column(&conn, "zone_type"),
            "column must exist after v31",
        );
        assert!(
            output_segments_has_index(&conn, "idx_segments_zone_type"),
            "zone_type index must exist after v31",
        );

        conn.execute_batch(m31.down_sql.unwrap())
            .expect("rollback v31");
        assert!(
            !output_segments_has_column(&conn, "zone_type"),
            "column must be gone after down-rollback",
        );
        assert!(
            !output_segments_has_index(&conn, "idx_segments_zone_type"),
            "zone_type index must be gone after down-rollback",
        );
    }

    fn output_segments_has_index(conn: &rusqlite::Connection, index: &str) -> bool {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_index_list('output_segments')")
            .unwrap();
        // The projection has exactly one column, so the name is at index 0.
        // Index 1 made every row an InvalidColumnIndex error that
        // `filter_map(ok)` silently dropped, so this helper always returned
        // false — and the down-rollback `!has_index` assertion passed
        // vacuously, hiding it (ft-kccj8).
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        names.iter().any(|n| n == index)
    }

    /// br-ft-4yr9i: applying the version-25 migration to a fresh
    /// in-memory SQLite db creates the agent_profiles table; a
    /// re-run is idempotent (substrate's IF NOT EXISTS form).
    #[test]
    fn agent_profiles_migration_creates_table_and_is_idempotent() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory sqlite");
        // Apply just the version-25 migration directly (full
        // initialize_schema would also run versions 1..24 which
        // have unrelated dependencies — keep this unit-scoped).
        let m25 = MIGRATIONS
            .iter()
            .find(|m| m.version == 25)
            .expect("version 25 migration");

        conn.execute_batch(m25.up_sql).expect("first apply");
        assert!(
            table_exists(&conn, "agent_profiles").unwrap(),
            "agent_profiles table must exist after the version-25 migration",
        );

        // Re-running must be a no-op (idempotent via IF NOT EXISTS).
        conn.execute_batch(m25.up_sql).expect("idempotent re-apply");
        assert!(
            table_exists(&conn, "agent_profiles").unwrap(),
            "table still present after re-apply",
        );

        // The role index must also be discoverable via SQLite's
        // pragma_index_list.
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='agent_profiles_role_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 1, "agent_profiles_role_idx must exist");

        // Down-rollback must remove both. The substrate ships a
        // straight DROP INDEX + DROP TABLE.
        conn.execute_batch(m25.down_sql.unwrap()).expect("down");
        assert!(
            !table_exists(&conn, "agent_profiles").unwrap(),
            "table must be removed by down_sql",
        );
    }

    #[test]
    fn scrollback_summary_v39_backfills_and_tracks_exact_retained_segments() {
        use rusqlite::{Connection, params};

        type SummaryRow = (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>);

        fn load_summary(conn: &Connection, pane_id: i64) -> rusqlite::Result<SummaryRow> {
            conn.query_row(
                "SELECT retained_segment_count, first_seq, last_seq,
                        first_captured_at, last_captured_at
                 FROM pane_scrollback_summary WHERE pane_id = ?1",
                [pane_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
        }

        let file = tempfile::NamedTempFile::new().expect("temporary database");
        let path = file.path().to_path_buf();
        let conn = Connection::open(&path).expect("open pre-v39 database");
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE panes (pane_id INTEGER PRIMARY KEY);
             CREATE TABLE output_segments (
                 id INTEGER PRIMARY KEY,
                 pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 content_len INTEGER NOT NULL,
                 captured_at INTEGER NOT NULL,
                 UNIQUE(pane_id, seq)
             );
             CREATE INDEX idx_segments_pane_seq ON output_segments(pane_id, seq);
             CREATE INDEX idx_segments_pane_captured
                 ON output_segments(pane_id, captured_at DESC);
             CREATE TABLE output_gaps (
                 id INTEGER PRIMARY KEY,
                 pane_id INTEGER NOT NULL,
                 reason TEXT NOT NULL
             );
             INSERT INTO panes (pane_id) VALUES (1), (2);
             INSERT INTO output_segments
                 (pane_id, seq, content, content_len, captured_at)
             VALUES
                 (1, 2, 'multi\nline\n', 11, 500),
                 (1, 4, 'no newline', 10, 100),
                 (1, 6, 'rewrite\rover', 12, 300),
                 (1, 8, 'crlf\r\n', 6, 200),
                 (1, 9, 'λ', 2, 50);
             INSERT INTO output_gaps (pane_id, reason) VALUES (1, 'capture_gap');",
        )
        .expect("seed legacy segment history");

        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 39)
            .expect("v39 migration");
        assert!(migration.down_sql.is_none(), "v39 must remain forward-only");
        conn.execute_batch(migration.up_sql)
            .expect("apply scrollback summary migration");
        conn.execute_batch(migration.up_sql)
            .expect("v39 migration SQL is idempotent");

        let fresh = Connection::open_in_memory().expect("open fresh canonical database");
        initialize_schema(&fresh).expect("initialize fresh canonical schema");
        for (object_type, object_name) in [
            ("table", "pane_scrollback_summary"),
            ("trigger", "pane_scrollback_summary_panes_ai"),
            ("trigger", "pane_scrollback_summary_panes_ad"),
            ("trigger", "pane_scrollback_summary_bi"),
            ("trigger", "pane_scrollback_summary_bd"),
            ("trigger", "pane_scrollback_summary_pane_id_bu"),
            ("trigger", "output_segments_scrollback_summary_bi"),
            ("trigger", "output_segments_scrollback_summary_ai"),
            ("trigger", "output_segments_scrollback_summary_bd"),
            ("trigger", "output_segments_scrollback_summary_ad"),
            ("trigger", "output_segments_scrollback_metadata_bu"),
        ] {
            let upgraded_sql = load_schema_object_sql(&conn, object_type, object_name)
                .unwrap()
                .unwrap_or_else(|| panic!("upgraded schema is missing {object_name}"));
            let fresh_sql = load_schema_object_sql(&fresh, object_type, object_name)
                .unwrap()
                .unwrap_or_else(|| panic!("fresh schema is missing {object_name}"));
            assert_eq!(
                compact_schema_sql(&upgraded_sql),
                compact_schema_sql(&fresh_sql),
                "fresh and upgraded definitions must match for {object_name}"
            );
        }

        assert_eq!(
            load_summary(&conn, 1).unwrap(),
            (5, Some(2), Some(9), Some(50), Some(500))
        );
        assert_eq!(load_summary(&conn, 2).unwrap(), (0, None, None, None, None));

        conn.execute("INSERT INTO panes (pane_id) VALUES (3)", [])
            .expect("pane trigger creates zero summary");
        assert_eq!(load_summary(&conn, 3).unwrap(), (0, None, None, None, None));
        conn.execute(
            "INSERT INTO output_segments
                 (pane_id, seq, content, content_len, captured_at)
             VALUES (3, 0, 'first', 5, 700)",
            [],
        )
        .expect("append updates summary");
        assert_eq!(
            load_summary(&conn, 3).unwrap(),
            (1, Some(0), Some(0), Some(700), Some(700))
        );

        conn.execute(
            "UPDATE output_segments SET content = 'masked', content_len = 6
             WHERE pane_id = 1 AND seq = 4",
            [],
        )
        .expect("content redaction does not mutate summary metadata");
        assert_eq!(
            load_summary(&conn, 1).unwrap(),
            (5, Some(2), Some(9), Some(50), Some(500))
        );

        conn.execute(
            "DELETE FROM output_segments WHERE pane_id = 1 AND seq IN (2, 9)",
            [],
        )
        .expect("retention removes both sequence and timestamp boundaries");
        assert_eq!(
            load_summary(&conn, 1).unwrap(),
            (3, Some(4), Some(8), Some(100), Some(300))
        );
        conn.execute("DELETE FROM output_segments WHERE pane_id = 1", [])
            .expect("full retention truncation");
        assert_eq!(load_summary(&conn, 1).unwrap(), (0, None, None, None, None));

        assert!(
            conn.execute(
                "UPDATE output_segments SET seq = 1 WHERE pane_id = 3 AND seq = 0",
                [],
            )
            .is_err(),
            "retained segment identity must be immutable"
        );
        assert!(
            conn.execute("DELETE FROM pane_scrollback_summary WHERE pane_id = 3", [])
                .is_err(),
            "a live pane summary cannot be removed"
        );
        assert!(
            conn.execute(
                "INSERT INTO output_segments
                     (pane_id, seq, content, content_len, captured_at)
                 VALUES (3, -1, 'invalid', 7, 800)",
                [],
            )
            .is_err(),
            "negative sequence metadata must fail before persistence"
        );

        conn.execute(
            "UPDATE pane_scrollback_summary
             SET retained_segment_count = ?1
             WHERE pane_id = 3",
            [i64::MAX],
        )
        .expect("seed arithmetic boundary");
        assert!(
            conn.execute(
                "INSERT INTO output_segments
                     (pane_id, seq, content, content_len, captured_at)
                 VALUES (3, 1, 'overflow', 8, 800)",
                [],
            )
            .is_err(),
            "retained segment arithmetic must not wrap or promote to REAL"
        );
        let overflow_row_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM output_segments WHERE pane_id = 3 AND seq = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            overflow_row_count, 0,
            "failed summary update rolls back append"
        );
        conn.execute(
            "UPDATE pane_scrollback_summary
             SET retained_segment_count = 1
             WHERE pane_id = 3",
            [],
        )
        .expect("restore valid summary after overflow negative control");

        drop(conn);
        let reopened = Connection::open(&path).expect("reopen migrated database");
        reopened
            .execute_batch("PRAGMA foreign_keys = ON;")
            .expect("enable foreign keys after restart");
        reopened
            .execute(
                "INSERT INTO output_segments
                     (pane_id, seq, content, content_len, captured_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![3_i64, 1_i64, "after restart", 13_i64, 900_i64],
            )
            .expect("triggers persist across restart");
        let restarted_summary: (i64, i64, i64) = reopened
            .query_row(
                "SELECT retained_segment_count, first_seq, last_seq
                 FROM pane_scrollback_summary WHERE pane_id = 3",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(restarted_summary, (2, 0, 1));

        reopened
            .execute("DELETE FROM panes WHERE pane_id = 3", [])
            .expect("pane deletion settles segments before removing summary");
        let remaining_summary: i64 = reopened
            .query_row(
                "SELECT count(*) FROM pane_scrollback_summary WHERE pane_id = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_summary, 0);
    }

    #[test]
    fn scrollback_summary_v39_serializes_concurrent_append_transactions() {
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        use rusqlite::Connection;

        let file = tempfile::NamedTempFile::new().expect("temporary database");
        let path = file.path().to_path_buf();
        let conn = Connection::open(&path).expect("open pre-v39 database");
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE panes (pane_id INTEGER PRIMARY KEY);
             CREATE TABLE output_segments (
                 id INTEGER PRIMARY KEY,
                 pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 content_len INTEGER NOT NULL,
                 captured_at INTEGER NOT NULL,
                 UNIQUE(pane_id, seq)
             );
             CREATE INDEX idx_segments_pane_seq ON output_segments(pane_id, seq);
             CREATE INDEX idx_segments_pane_captured
                 ON output_segments(pane_id, captured_at DESC);
             INSERT INTO panes (pane_id) VALUES (11);",
        )
        .expect("seed concurrent append database");
        let migration = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 39)
            .expect("v39 migration");
        conn.execute_batch(migration.up_sql)
            .expect("apply scrollback summary migration");
        drop(conn);

        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for worker in 0_i64..2 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                let mut conn = Connection::open(path).expect("open concurrent writer");
                conn.busy_timeout(Duration::from_secs(10))
                    .expect("configure SQLite busy timeout");
                conn.execute_batch("PRAGMA foreign_keys = ON;")
                    .expect("enable writer foreign keys");
                barrier.wait();
                let tx = conn.transaction().expect("begin append transaction");
                for offset in 0_i64..64 {
                    let seq = worker * 64 + offset;
                    let captured_at = (worker + 1) * 1_000 + offset;
                    tx.execute(
                        "INSERT INTO output_segments
                             (pane_id, seq, content, content_len, captured_at)
                         VALUES (11, ?1, 'x', 1, ?2)",
                        [seq, captured_at],
                    )
                    .expect("append concurrent segment");
                }
                tx.commit().expect("commit append transaction");
            }));
        }
        for worker in workers {
            worker.join().expect("concurrent writer did not panic");
        }

        let conn = Connection::open(&path).expect("reopen concurrent append database");
        let history_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM output_segments WHERE pane_id = 11",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let summary: (i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT retained_segment_count, first_seq, last_seq,
                        first_captured_at, last_captured_at
                 FROM pane_scrollback_summary WHERE pane_id = 11",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(history_count, 128);
        assert_eq!(summary, (128, 0, 127, 1_000, 2_063));
    }

    #[test]
    fn session_retained_size_v40_upgrade_backfills_exact_bytes_and_tracks_mutations() {
        use rusqlite::{Connection, params};

        type Components = (i64, i64, i64, i64, i64);

        fn load_components(conn: &Connection, relation: &str, session_id: &str) -> Components {
            let sql = format!(
                "SELECT session_row_bytes, checkpoint_row_bytes,
                        pane_state_row_bytes, restore_lifecycle_row_bytes,
                        retained_bytes
                 FROM {relation} WHERE session_id = ?1"
            );
            conn.query_row(&sql, [session_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
        }

        fn assert_authority_matches_recomputation(conn: &Connection, session_id: &str) {
            let stored = load_components(conn, "session_retained_size", session_id);
            let recomputed_four: (i64, i64, i64, i64) = conn
                .query_row(
                    "SELECT session_row_bytes, checkpoint_row_bytes,
                            pane_state_row_bytes, restore_lifecycle_row_bytes
                     FROM session_retained_size_recomputed WHERE session_id = ?1",
                    [session_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!((stored.0, stored.1, stored.2, stored.3), recomputed_four);
            assert_eq!(stored.4, stored.0 + stored.1 + stored.2 + stored.3);
        }

        let file = tempfile::NamedTempFile::new().expect("temporary pre-v40 database");
        let path = file.path().to_path_buf();
        let conn = Connection::open(&path).expect("open pre-v40 database");
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE schema_version (
                 version INTEGER NOT NULL,
                 applied_at INTEGER NOT NULL,
                 description TEXT
             );
             CREATE TABLE mux_sessions (
                 session_id TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL,
                 last_checkpoint_at INTEGER,
                 shutdown_clean INTEGER NOT NULL DEFAULT 0,
                 topology_json TEXT NOT NULL,
                 window_metadata_json TEXT,
                 ft_version TEXT NOT NULL,
                 host_id TEXT,
                 clean_checkpoint_id INTEGER
                     REFERENCES session_checkpoints(id) ON DELETE SET NULL
             );
             CREATE TABLE session_checkpoints (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL
                     REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                 checkpoint_at INTEGER NOT NULL,
                 checkpoint_type TEXT NOT NULL,
                 state_hash TEXT NOT NULL,
                 pane_count INTEGER NOT NULL,
                 total_bytes INTEGER NOT NULL,
                 metadata_json TEXT,
                 checkpoint_role TEXT NOT NULL DEFAULT 'snapshot',
                 topology_json TEXT,
                 restore_intent_checkpoint_id INTEGER
                     REFERENCES session_checkpoints(id) ON DELETE CASCADE
             );
             CREATE TABLE restore_attempt_lifecycle (
                 intent_checkpoint_id INTEGER PRIMARY KEY
                     REFERENCES session_checkpoints(id)
                     ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
                 session_id TEXT NOT NULL
                     REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
                 source_checkpoint_id INTEGER NOT NULL,
                 outcome_checkpoint_id INTEGER
                     REFERENCES session_checkpoints(id) ON DELETE SET NULL,
                 status TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 resolved_at INTEGER
             );
             CREATE TABLE mux_pane_state (
                 id INTEGER PRIMARY KEY,
                 checkpoint_id INTEGER NOT NULL
                     REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                 pane_id INTEGER NOT NULL,
                 cwd TEXT,
                 command TEXT,
                 env_json TEXT,
                 terminal_state_json TEXT NOT NULL,
                 agent_metadata_json TEXT,
                 scrollback_checkpoint_seq INTEGER,
                 last_output_at INTEGER
             );
             PRAGMA user_version = 39;",
        )
        .expect("create pre-v40 session schema");

        let session_id = "session-λ";
        let session_topology = r#"{"tabs":["α"]}"#;
        let host_id = "host-值";
        conn.execute(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, window_metadata_json, ft_version, host_id
             ) VALUES (?1, 11, NULL, 0, ?2, NULL, '0.40.0', ?3)",
            params![session_id, session_topology, host_id],
        )
        .unwrap();

        let metadata_one = r#"{"m":"é"}"#;
        let checkpoint_topology = r#"{"tab":"β"}"#;
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 topology_json
             ) VALUES (?1, 12, 'periodic', 'hash-α', 2, 99, ?2,
                       'snapshot', ?3)",
            params![session_id, metadata_one, checkpoint_topology],
        )
        .unwrap();
        let snapshot_id = conn.last_insert_rowid();
        let intent_metadata = r#"{"restore_attempt":{"phase":"intent"}}"#;
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (?1, 13, 'startup', 'intent-hash', 0, 0, ?2,
                       'restore_intent')",
            params![session_id, intent_metadata],
        )
        .unwrap();
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO mux_pane_state (
                 checkpoint_id, pane_id, cwd, command, env_json,
                 terminal_state_json, agent_metadata_json,
                 scrollback_checkpoint_seq, last_output_at
             ) VALUES (?1, 7, '/tmp/λ', 'cargo', '{\"K\":\"值\"}',
                       '{\"cursor\":\"é\"}', NULL, 4, 14)",
            [snapshot_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mux_pane_state (
                 checkpoint_id, pane_id, terminal_state_json
             ) VALUES (?1, 8, '{}')",
            [snapshot_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at
             ) VALUES (?1, ?2, ?3, 'intent', 15)",
            params![intent_id, session_id, snapshot_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1, last_checkpoint_at = 13,
                 clean_checkpoint_id = ?2
             WHERE session_id = ?1",
            params![session_id, snapshot_id],
        )
        .unwrap();

        let step = build_migration_plan(39, 40)
            .expect("build v40 migration plan")
            .steps
            .into_iter()
            .next()
            .expect("v40 migration step");
        apply_migration_step(&conn, &step).expect("apply v40 retained-size migration");
        assert_eq!(get_user_version(&conn).unwrap(), 40);
        assert_authority_matches_recomputation(&conn, session_id);

        let text_bytes = |value: &str| i64::try_from(value.len()).unwrap();
        let expected_session = text_bytes(session_id)
            + 8
            + 8
            + 8
            + text_bytes(session_topology)
            + text_bytes("0.40.0")
            + text_bytes(host_id)
            + 8;
        let checkpoint_bytes = |checkpoint_type: &str,
                                state_hash: &str,
                                metadata: Option<&str>,
                                role: &str,
                                topology: Option<&str>| {
            8 + text_bytes(session_id)
                + 8
                + text_bytes(checkpoint_type)
                + text_bytes(state_hash)
                + 8
                + 8
                + metadata.map_or(0, text_bytes)
                + text_bytes(role)
                + topology.map_or(0, text_bytes)
        };
        let expected_checkpoints = checkpoint_bytes(
            "periodic",
            "hash-α",
            Some(metadata_one),
            "snapshot",
            Some(checkpoint_topology),
        ) + checkpoint_bytes(
            "startup",
            "intent-hash",
            Some(intent_metadata),
            "restore_intent",
            None,
        );
        let expected_panes = 8
            + 8
            + 8
            + text_bytes("/tmp/λ")
            + text_bytes("cargo")
            + text_bytes(r#"{"K":"值"}"#)
            + text_bytes(r#"{"cursor":"é"}"#)
            + 8
            + 8
            + 8
            + 8
            + 8
            + text_bytes("{}");
        let expected_lifecycle = 8 + text_bytes(session_id) + 8 + text_bytes("intent") + 8;
        let stored = load_components(&conn, "session_retained_size", session_id);
        assert_eq!(
            stored,
            (
                expected_session,
                expected_checkpoints,
                expected_panes,
                expected_lifecycle,
                expected_session + expected_checkpoints + expected_panes + expected_lifecycle,
            )
        );

        let maximum_metadata = format!(
            "\"{}\"",
            "x".repeat(crate::checkpoint_witness::MAX_CHECKPOINT_METADATA_BYTES - 2)
        );
        conn.execute(
            "UPDATE session_checkpoints SET metadata_json = ?1 WHERE id = ?2",
            params![maximum_metadata, snapshot_id],
        )
        .expect("replace metadata with maximum supported payload");
        assert_authority_matches_recomputation(&conn, session_id);
        conn.execute(
            "UPDATE session_checkpoints SET metadata_json = NULL WHERE id = ?1",
            [snapshot_id],
        )
        .expect("exercise NULL payload accounting");
        conn.execute(
            "UPDATE mux_sessions
             SET topology_json = '{\"tabs\":[\"値\",\"値\"]}',
                 window_metadata_json = '{\"columns\":240}', host_id = NULL
             WHERE session_id = ?1",
            [session_id],
        )
        .expect("replace session-level topology without multiplying it");
        assert_authority_matches_recomputation(&conn, session_id);

        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES (?1, 16, 'event', 'third', 1, 1, 'snapshot')",
            [session_id],
        )
        .unwrap();
        let third_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO mux_pane_state (
                 checkpoint_id, pane_id, terminal_state_json
             ) VALUES (?1, 9, '{\"wide\":\"界\"}')",
            [third_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE restore_attempt_lifecycle
             SET outcome_checkpoint_id = ?2, status = 'outcome_complete'
             WHERE intent_checkpoint_id = ?1",
            params![intent_id, third_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE restore_attempt_lifecycle
             SET status = 'resolved', resolved_at = 17
             WHERE intent_checkpoint_id = ?1",
            [intent_id],
        )
        .unwrap();
        assert_authority_matches_recomputation(&conn, session_id);
        conn.execute(
            "DELETE FROM restore_attempt_lifecycle WHERE intent_checkpoint_id = ?1",
            [intent_id],
        )
        .unwrap();
        conn.execute("DELETE FROM session_checkpoints WHERE id = ?1", [third_id])
            .expect("checkpoint cascade subtracts each pane exactly once");
        assert_authority_matches_recomputation(&conn, session_id);

        drop(conn);
        let reopened = Connection::open(&path).expect("reopen migrated v40 database");
        reopened.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        validate_session_retained_size_schema(&reopened)
            .expect("v40 schema and authority survive restart");
        assert_authority_matches_recomputation(&reopened, session_id);
        reopened
            .execute(
                "DELETE FROM session_checkpoints WHERE id = ?1",
                [snapshot_id],
            )
            .expect("clean-checkpoint and pane cascades remain exact after restart");
        assert_authority_matches_recomputation(&reopened, session_id);
        reopened
            .execute(
                "DELETE FROM mux_sessions WHERE session_id = ?1",
                [session_id],
            )
            .expect("session cascade removes retained-size authority");
        let retained_size_rows: i64 = reopened
            .query_row("SELECT count(*) FROM session_retained_size", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(retained_size_rows, 0);
    }

    #[test]
    fn current_schema_reopen_fails_closed_when_retained_size_trigger_drifts() {
        let conn = Connection::open_in_memory().expect("open database");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch(
            "DROP TRIGGER mux_sessions_retained_size_ai;
             CREATE TRIGGER mux_sessions_retained_size_ai
             AFTER INSERT ON mux_sessions BEGIN SELECT 1; END;",
        )
        .expect("inject same-name trigger drift");

        let error = initialize_schema(&conn)
            .expect_err("an up-to-date version stamp must not mask trigger-body drift");
        assert!(
            error.to_string().contains("mux_sessions_retained_size_ai"),
            "drifted trigger identity must be explicit: {error}"
        );
    }

    #[test]
    fn current_schema_reopen_fails_closed_when_scrollback_trigger_is_missing() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open database");
        initialize_schema(&conn).expect("initialize current schema");
        conn.execute_batch("DROP TRIGGER output_segments_scrollback_summary_ai;")
            .expect("inject missing-trigger corruption");

        let error = initialize_schema(&conn)
            .expect_err("an up-to-date version stamp must not mask missing summary authority");
        assert!(
            error
                .to_string()
                .contains("output_segments_scrollback_summary_ai"),
            "missing trigger identity must be explicit: {error}"
        );
    }
}
