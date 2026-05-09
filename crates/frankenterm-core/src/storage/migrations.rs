//! SQLite schema migration types and runner.
//!
//! Extracted from `storage.rs` for ft-2dorr while preserving the
//! `frankenterm_core::storage::*` facade through re-exports in the parent
//! module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
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
        up_sql: r"
            CREATE TABLE IF NOT EXISTS segment_embeddings (
                segment_id INTEGER PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
                embedder_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector BLOB NOT NULL,
                embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
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
/// - Up-to-date database (user_version = SCHEMA_VERSION): No-op
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

    if current != 0 {
        check_ft_version_compatibility(conn)?;
    }

    if current == SCHEMA_VERSION {
        // Already up to date
        ensure_ft_meta(conn, SCHEMA_VERSION)?;
        return Ok(());
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
        return run_v0_init_in_transaction(conn);
    }
    // `needs_init` is intentionally consulted only via the path above;
    // a future caller that wants to discriminate "really fresh" vs
    // "existing v0" should call `needs_initialization` directly.
    let _ = needs_init;

    // Apply pending migrations for existing databases (version > 0)
    run_migrations(conn, current)?;

    ensure_ft_meta(conn, SCHEMA_VERSION)?;

    Ok(())
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
            embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
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

#[cfg(test)]
static V0_INIT_FAULT_AT: std::sync::atomic::AtomicI8 = std::sync::atomic::AtomicI8::new(-1);

#[cfg(test)]
pub(crate) fn set_v0_init_fault_for_test(step: Option<V0InitStep>) {
    let value: i8 = match step {
        None => -1,
        Some(V0InitStep::RepairComplete) => 0,
        Some(V0InitStep::SchemaSqlApplied) => 1,
        Some(V0InitStep::MigrationsApplied) => 2,
    };
    V0_INIT_FAULT_AT.store(value, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
fn check_v0_init_fault(step: V0InitStep) -> Result<()> {
    let active = V0_INIT_FAULT_AT.load(std::sync::atomic::Ordering::SeqCst);
    let target = match step {
        V0InitStep::RepairComplete => 0,
        V0InitStep::SchemaSqlApplied => 1,
        V0InitStep::MigrationsApplied => 2,
    };
    if active == target {
        // Clear the fault so the test can re-enter the helper and verify
        // success on the second try. Mirrors how a real crash would not
        // re-fire on a subsequent open.
        V0_INIT_FAULT_AT.store(-1, std::sync::atomic::Ordering::SeqCst);
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
    for line in SCHEMA_SQL.lines() {
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
fn run_v0_init_in_transaction(conn: &Connection) -> Result<()> {
    let (pragma_preamble, schema_body) = split_schema_sql_pragmas();

    if !pragma_preamble.trim().is_empty() {
        conn.execute_batch(&pragma_preamble).map_err(|e| {
            StorageError::MigrationFailed(format!("Schema PRAGMA preamble failed: {e}"))
        })?;
    }

    conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| {
        StorageError::MigrationFailed(format!("v0 init: failed to begin transaction: {e}"))
    })?;

    let result: Result<()> = (|| {
        repair_existing_v0_tables_before_schema_sql(conn)?;
        #[cfg(test)]
        check_v0_init_fault(V0InitStep::RepairComplete)?;

        conn.execute_batch(&schema_body)
            .map_err(|e| StorageError::MigrationFailed(format!("Schema init failed: {e}")))?;
        #[cfg(test)]
        check_v0_init_fault(V0InitStep::SchemaSqlApplied)?;

        run_migrations(conn, 0)?;
        #[cfg(test)]
        check_v0_init_fault(V0InitStep::MigrationsApplied)?;

        ensure_ft_meta(conn, SCHEMA_VERSION)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(|e| {
                StorageError::MigrationFailed(format!("v0 init: failed to commit: {e}"))
            })?;
            Ok(())
        }
        Err(err) => {
            // Best-effort rollback; the original error is what the caller cares about.
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
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
    if to_version > SCHEMA_VERSION {
        return Err(StorageError::MigrationFailed(format!(
            "Target schema version ({to_version}) is newer than supported ({SCHEMA_VERSION}). \
             Please upgrade wa to a newer version."
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

pub(crate) fn apply_migration_step(conn: &Connection, step: &MigrationStep) -> Result<()> {
    let Some(migration) = migration_for_version(step.migration_version) else {
        return Err(StorageError::MigrationFailed(format!(
            "Unknown migration version {}",
            step.migration_version
        ))
        .into());
    };

    // ft-k542h: when an outer caller (e.g., `run_v0_init_in_transaction`) has
    // already opened a transaction, skip the per-step BEGIN/COMMIT/ROLLBACK.
    // SQLite rejects nested BEGIN with an error, and the outer transaction's
    // ROLLBACK on failure already provides atomicity for the wrapped scope.
    let owns_tx = conn.is_autocommit();
    if owns_tx {
        conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| {
            StorageError::MigrationFailed(format!(
                "Failed to start migration transaction for v{}: {e}",
                migration.version
            ))
        })?;
    }

    let result: Result<()> = match step.direction {
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
    };

    match (result, owns_tx) {
        (Ok(()), true) => {
            conn.execute_batch("COMMIT").map_err(|e| {
                StorageError::MigrationFailed(format!(
                    "Failed to commit migration transaction for v{}: {e}",
                    migration.version
                ))
            })?;
            Ok(())
        }
        (Ok(()), false) => Ok(()),
        (Err(err), true) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
        (Err(err), false) => {
            // Outer caller owns the transaction and is responsible for the
            // ROLLBACK; just surface the error.
            Err(err)
        }
    }
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

#[derive(Debug, Clone)]
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

    let now_ms = now_epoch_ms();
    let mut current_ft = crate::VERSION.to_string();

    let existing = load_ft_meta(conn)?;
    match existing {
        None => {
            conn.execute(
                &format!(
                    "INSERT INTO {meta_table} \
                     (id, schema_version, {col_min}, {col_created}, created_at) \
                     VALUES (1, ?1, ?2, ?3, ?4)"
                ),
                params![schema_version, current_ft, current_ft, now_ms],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        }
        Some(meta) => {
            let mut min_compatible = meta.min_compatible_ft.clone();
            if let (Some(current), Some(existing_min)) = (
                FtVersion::parse(&current_ft),
                FtVersion::parse(&meta.min_compatible_ft),
            ) {
                if current > existing_min {
                    min_compatible.clone_from(&current_ft);
                }
            } else if meta.min_compatible_ft != current_ft {
                min_compatible.clone_from(&current_ft);
            }

            let created_by = if meta.created_by_ft.is_empty() {
                std::mem::take(&mut current_ft)
            } else {
                meta.created_by_ft.clone()
            };
            let created_at = if meta.created_at <= 0 {
                now_ms
            } else {
                meta.created_at
            };

            if meta.schema_version != schema_version
                || meta.min_compatible_ft != min_compatible
                || meta.created_by_ft != created_by
                || meta.created_at != created_at
            {
                conn.execute(
                    &format!(
                        "UPDATE {meta_table} \
                         SET schema_version=?1, {col_min}=?2, {col_created}=?3, created_at=?4 \
                         WHERE id = 1"
                    ),
                    params![schema_version, min_compatible, created_by, created_at],
                )
                .map_err(|e| StorageError::Database(e.to_string()))?;
            }
        }
    }

    Ok(())
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
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
