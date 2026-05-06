//! Storage layer with SQLite and FTS5
//!
//! Provides persistent storage for captured output, events, and workflows.
//!
//! # Schema Design
//!
//! The database uses WAL mode for concurrent reads and single-writer semantics.
//! All timestamps are epoch milliseconds (i64) for hot-path performance.
//! JSON columns are stored as TEXT for SQLite compatibility.
//!
//! # Tables
//!
//! - `panes`: Pane metadata and observation decisions
//! - `output_segments`: Append-only captured terminal output
//! - `output_gaps`: Explicit discontinuities in capture
//! - `events`: Pattern detections with lifecycle tracking
//! - `workflow_executions`: Durable workflow state
//! - `workflow_step_logs`: Step execution history
//! - `workflow_action_plans`: Canonical action plans for workflows
//! - `audit_actions`: Audit trail for policy decisions and outcomes
//! - `action_undo`: Undo metadata for audit actions
//! - `action_history`: View joining audit + undo + workflow step info
//! - `approval_tokens`: Allow-once approvals scoped to actions
//! - `config`: Key-value settings
//! - `saved_searches`: Persisted search definitions
//! - `maintenance_log`: System events and metrics
//!
//! FTS5 virtual table `output_segments_fts` enables full-text search.

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::{
    collections::{HashMap, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    time::Instant,
};

use crate::runtime_async::oneshot;
use frankenterm_core_audit_types::storage_audit::AuditFieldRedactor;
use rusqlite::{Connection, OptionalExtension, params, types::Value as SqlValue};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StorageError};
use crate::events::event_identity_key;
use crate::lru_cache::LruCache;
use crate::policy::Redactor;
#[cfg(test)]
use crate::recorder_invariants::InvariantReport;
#[cfg(test)]
use crate::recorder_storage::{RecorderBackendKind, RecorderOffset};
use crate::redactor::{RedactionResult, StreamingRedactor};
use crate::runtime_async::mpsc;
use crate::runtime_telemetry::{SwarmCapacityStage, SwarmCapacityStageTimer};
use crate::search::{FusionBackend, HybridSearchService, SearchMode};
use crate::storage::io_scheduler::{
    StorageIoAdmissionDecision, StorageIoClass, StorageIoScheduler, StorageIoSchedulerConfig,
    StorageIoWorkItem,
};
use crate::storage_backend_helpers::{count_table_where, execute_typed, row_exists_where};
use crate::storage_backend_row_helpers::{CellRowReader, RowReader};
use crate::storage_backend_trait::{
    BackendError, RusqliteBackend, SqlCell, StorageBackend, ToSqlValue,
};
use crate::storage_pane_id_set::{PaneIdSet, PaneIdTempTablePlan};
use crate::storage_telemetry::StoragePipelineSnapshot;
#[cfg(test)]
use crate::storage_telemetry::{SloStatus, StorageHealthTier};

pub mod io_scheduler;
pub mod mmap_store;

const TIMELINE_PANE_ID_INLINE_LIMIT: usize = 96;

pub use frankenterm_core_audit_types::storage_audit::{
    ActionHistoryQuery, ActionHistoryRecord, ActionUndoRecord, AuditActionRecord, AuditQuery,
    AuditStreamPage, AuditStreamQuery, AuditStreamRecord, PolicyDeniedAuditRecord,
    WorkflowStepLogRecord,
};

impl AuditFieldRedactor for Redactor {
    fn redact(&self, value: &str) -> String {
        Redactor::redact(self, value)
    }
}

// [ft-fxymo / ft-dn2tu Phase 3] Database health-check & repair surface
// extracted into `storage/health.rs`. Re-exported here so existing call
// sites continue to use `frankenterm_core::storage::{...}` unchanged.
pub mod health;
pub use health::{
    DbCheckItem, DbCheckReport, DbCheckStatus, DbRepairItem, DbRepairReport, DbStatsReport,
    EventTypeStats, PaneStats, TableStats, check_database_health, database_stats, repair_database,
};

// [ft-6qkx1 / ft-dn2tu Phase 2] Schema DDL strings + version constant
// extracted into `storage/schema_ddl.rs`. Re-exported so existing call
// sites (`frankenterm_core::storage::SCHEMA_VERSION` / `SCHEMA_SQL` in
// session_retention, chunk_vector_store, proptest_session_retention)
// continue to work unchanged. `FTS_TRIGGER_RECREATE_SQL` stays
// `pub(crate)` and is reached from sibling production code paths via
// `schema_ddl::FTS_TRIGGER_RECREATE_SQL` (no public re-export).
pub mod schema_ddl;
pub use schema_ddl::{SCHEMA_SQL, SCHEMA_VERSION};

// [ft-nsb8c / ft-dn2tu Phase 5] Export query helpers — read-only,
// no writer-thread interaction. Internal-only (not re-exported); called
// from `StorageHandle::export_*` methods within this crate via
// `export::query_export_*`.
mod export;

// [ft-aw52a / ft-dn2tu Phase 6] StorageHandle impl-split scaffolding.
// Per-feature impl blocks live under `storage/handle/`; the first
// beachhead covers the event-mute methods. See `storage::handle::mod`
// for the scaffolding plan and follow-up cluster list.
mod handle;

// br-ft-94ito / ft-dn2tu Phase 2.2: types are in a sibling module
// that `migrations.rs` re-exports from, so the
// `frankenterm_core::storage::migrations::*` facade is preserved.
pub mod migrations;
pub(crate) mod migrations_types;

// br-ft-bbhwz / ft-dn2tu Phase 2.3: storage row-shape types
// (Segment, CheckpointResult, DatabasePageStats, Gap,
// FtsSyncConfig — first slice). Re-exported from this module
// via `pub use self::types::{...}` blocks below so the
// `frankenterm_core::storage::*` facade is preserved
// byte-for-byte.
pub(crate) mod types;

// br-ft-43lpu / ft-4yr9i.cont: synchronous SQL primitives for
// the agent_profiles table (insert/get/list/delete) callable
// from the storage writer thread or from tests.
pub mod agent_profiles_sql;
// br-ft-4iz0q substrate-pass: synchronous SQL primitives for
// the profiles_applied_log table (insert/get/list/delete)
// callable from the storage writer thread or from tests.
// Schema lives at MIGRATIONS[25] (v26).
pub mod profiles_applied_log_sql;
#[cfg(test)]
pub(crate) use migrations::{
    FtVersion, MIGRATIONS, V0InitStep, apply_migration_plan, apply_migration_step,
    build_migration_plan, load_ft_meta, previous_migration_version,
    segment_embeddings_table_is_canonical, set_user_version, set_v0_init_fault_for_test,
    split_schema_sql_pragmas, table_exists, table_has_column,
};
pub use migrations::{
    Migration, MigrationDirection, MigrationForensicBackendState, MigrationForensicBundle,
    MigrationForensicCaptureContext, MigrationForensicCorruptionDetail,
    MigrationForensicMigrationCheckpoint, MigrationInvariantSummary, MigrationPlan,
    MigrationRollbackClass, MigrationRollbackClassifierConfig, MigrationRollbackClassifierInput,
    MigrationRollbackDecision, MigrationRollbackExecutionError, MigrationRollbackExecutionReport,
    MigrationRollbackExecutionState, MigrationRollbackPlaybookContext, MigrationRollbackTrigger,
    MigrationStage, MigrationStatusEntry, MigrationStatusReport, MigrationStep,
    MigrationStorageSloSummary, classify_migration_rollback_trigger,
    execute_migration_rollback_playbook, get_schema_version, get_user_version, initialize_schema,
    migrate_database_to_version, migration_plan_for_path, migration_status_for_path,
    needs_initialization, pending_migrations,
};

// =============================================================================
// Schema Definition  →  moved to `storage/schema_ddl.rs`
// =============================================================================
//
// [ft-6qkx1 / ft-dn2tu Phase 2] `SCHEMA_VERSION`, `SCHEMA_SQL`, and
// the `FTS_TRIGGER_RECREATE_SQL` helper now live in
// `storage/schema_ddl.rs`. They are re-exported via
// `pub use schema_ddl::{...}` near the top of this file so the
// public surface (`frankenterm_core::storage::SCHEMA_VERSION`,
// `frankenterm_core::storage::SCHEMA_SQL`) is unchanged.

// =============================================================================
// Schema Migrations  →  moved to `storage/migrations.rs`
// =============================================================================

// =============================================================================
// Data Structures
// =============================================================================

// br-ft-bbhwz / ft-dn2tu Phase 2.3: Segment, CheckpointResult,
// DatabasePageStats (+ free_ratio impl) moved to
// `storage/types.rs`. Re-exported via the `pub use` line below
// so `frankenterm_core::storage::*` keeps resolving for
// downstream callers. Gap + FtsSyncConfig follow in their own
// re-export blocks below.
pub use self::types::{CheckpointResult, DatabasePageStats, Segment};

// br-ft-8bvg0 slice 2: 10 search/index result types lifted to
// `storage/types.rs`. All pure-data with serde derives, no
// private-helper deps from this module.
pub use self::types::{
    EmbeddingStats, FtsIndexState, FtsPaneProgress, FtsSyncResult, HybridSearchBundle,
    HybridSearchResult, IndexingHealthReport, PaneIndexingStats, SearchResult, SemanticSearchHit,
};

// br-ft-bbhwz: Gap + FtsSyncConfig (+ Default impl) moved to
// `storage/types.rs`; re-exported here.
pub use self::types::{FtsSyncConfig, Gap};

// br-ft-8bvg0 slice 3: Section 3 finish — PaneRecord +
// StoredEvent + EventAnnotations + EventMuteRecord +
// AgentSessionRecord (+ new_start impl) moved to
// `storage/types.rs`. AgentSessionRecord::new_start calls
// `super::now_ms()` (which is `pub fn` here) so the cross-module
// dep stays clean.
pub use self::types::{
    AgentSessionRecord, EventAnnotations, EventMuteRecord, PaneRecord, StoredEvent,
};

// =============================================================================
// Timeline Data Model (wa-6sk.1)
// =============================================================================

// br-ft-8bvg0 slice 4: Timeline data model first half — 8 types
// (CorrelationType + Display, Correlation, CorrelationRef,
// PaneInfo, HandledInfo, TimelineEvent, Timeline, TimelineQuery
// + builder impl) moved to `storage/types.rs`. Pure data + a
// pure builder; no private-helper deps.
pub use self::types::{
    Correlation, CorrelationRef, CorrelationType, HandledInfo, PaneInfo, Timeline, TimelineEvent,
    TimelineQuery,
};

// br-ft-8bvg0 slice 5: workflow + maintenance + secret-scan +
// metrics records (10 types + 3 impls) moved to
// `storage/types.rs`.
pub use self::types::{
    AgentMetricBreakdown, DailyMetricSummary, MaintenanceRecord, MetricQuery, MetricType,
    PreparedPlanRecord, SecretScanReportRecord, UsageMetricRecord, WorkflowActionPlanRecord,
    WorkflowRecord,
};

// br-ft-8bvg0 slice 6: notification + saved search + bookmark +
// approval token + pane reservation (8 types + 6 impls + 3
// SAVED_SEARCH_* constants) moved to `storage/types.rs`.
// SavedSearchRecord::new uses super::now_ms + rand::random
// (rand is a workspace dep). SAVED_SEARCH_DEFAULT_LIMIT mirrors
// crate::tuning_config::SearchTuning::DEFAULT_SAVED_SEARCH_LIMIT.
pub use self::types::{
    ApprovalTokenRecord, NotificationHistoryQuery, NotificationHistoryRecord, NotificationStatus,
    PaneBookmarkRecord, PaneReservation, PaneReservationConfig, SAVED_SEARCH_DEFAULT_LIMIT,
    SAVED_SEARCH_SINCE_MODE_FIXED, SAVED_SEARCH_SINCE_MODE_LAST_RUN, SavedSearchRecord,
};

// =============================================================================
// Schema Initialization & Migrations  →  moved to `storage/migrations.rs`
// =============================================================================

/// WAL recovery threshold: if WAL has more than this many frames, do a full checkpoint.
///
/// `pub(crate)` so `storage::health::check_database_health` can reuse the same
/// threshold without owning the constant. Used by `check_and_recover_wal` here
/// and by the WAL-checkpoint health check in [`storage::health`].
pub(crate) const WAL_RECOVERY_THRESHOLD: i64 = 10_000;

/// Check for and recover from unclean shutdown.
///
/// Handles WAL/journal files left over from crashes by:
/// 1. Detecting recovery situation (WAL/journal files exist)
/// 2. Running quick integrity check
/// 3. Checkpointing WAL if it's large
///
/// # Errors
///
/// Returns an error if:
/// - Database corruption is detected
/// - WAL checkpoint fails
pub fn check_and_recover_wal(conn: &Connection, db_path: &str) -> Result<()> {
    let wal_path = format!("{db_path}-wal");
    let journal_path = format!("{db_path}-journal");

    let wal_exists = Path::new(&wal_path).exists();
    let journal_exists = Path::new(&journal_path).exists();

    if wal_exists || journal_exists {
        tracing::info!(
            wal_exists,
            journal_exists,
            "Recovery situation detected, attempting recovery"
        );
    }

    // Run quick integrity check
    let integrity_result: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|e| StorageError::Database(format!("Integrity check failed: {e}")))?;

    if integrity_result != "ok" {
        tracing::error!(result = %integrity_result, "Database corruption detected");
        return Err(StorageError::Corruption {
            details: integrity_result,
        }
        .into());
    }

    // Checkpoint WAL using PASSIVE mode (doesn't block readers)
    let (busy, wal_frames, checkpointed): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|e| StorageError::Database(format!("WAL checkpoint failed: {e}")))?;

    if wal_frames > 0 {
        tracing::info!(busy, wal_frames, checkpointed, "WAL checkpoint completed");
    }

    // If WAL is huge, do a full checkpoint to truncate it
    if wal_frames > WAL_RECOVERY_THRESHOLD {
        tracing::warn!(
            frames = wal_frames,
            threshold = WAL_RECOVERY_THRESHOLD,
            "Large WAL detected, performing full checkpoint"
        );

        let (busy2, wal_frames2, checkpointed2): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| StorageError::Database(format!("WAL truncate checkpoint failed: {e}")))?;

        tracing::info!(
            busy = busy2,
            wal_frames = wal_frames2,
            checkpointed = checkpointed2,
            "WAL truncate checkpoint completed"
        );
    }

    if wal_exists || journal_exists {
        tracing::info!("Database recovery complete");
    }

    Ok(())
}

// =============================================================================
// Database Health Check & Repair  →  moved to `storage/health.rs`
// =============================================================================
//
// [ft-fxymo / ft-dn2tu Phase 3] The Db*Report types and the
// `database_stats` / `check_database_health` / `repair_database`
// functions live in `storage/health.rs`. They are re-exported via
// `pub use health::{...}` near the top of this file so the public
// surface (`frankenterm_core::storage::DbCheckReport`, etc.) is
// unchanged.

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn canonicalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                if let Some(val) = map.get(&key) {
                    canonical.insert(key, canonicalize_json_value(val));
                }
            }
            serde_json::Value::Object(canonical)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonicalize_json_value).collect())
        }
        _ => value.clone(),
    }
}

fn canonical_json_string(value: &serde_json::Value) -> Result<String> {
    let canonical = canonicalize_json_value(value);
    serde_json::to_string(&canonical).map_err(|e| {
        StorageError::Database(format!("Failed to serialize canonical JSON: {e}")).into()
    })
}

fn action_plan_record_from_plan(
    workflow_id: &str,
    plan: &crate::plan::ActionPlan,
) -> Result<WorkflowActionPlanRecord> {
    let mut plan = plan.clone();
    if plan.created_at.is_none() {
        plan.created_at = Some(now_epoch_ms());
    }
    let plan_hash = plan.compute_hash();
    let plan_json_value =
        serde_json::to_value(&plan).map_err(|e| StorageError::Database(e.to_string()))?;
    let plan_json = canonical_json_string(&plan_json_value)?;
    let created_at = plan.created_at.unwrap_or_else(now_epoch_ms);
    Ok(WorkflowActionPlanRecord {
        workflow_id: workflow_id.to_string(),
        plan_id: plan.plan_id.to_string(),
        plan_hash,
        plan_json,
        created_at,
    })
}

// =============================================================================
// Writer Command Types
// =============================================================================

/// Commands sent to the writer thread
enum WriteCommand {
    /// Append a segment (pane_id, content, content_hash, response channel)
    AppendSegment {
        pane_id: u64,
        content: String,
        content_hash: Option<String>,
        respond: oneshot::Sender<Result<Segment>>,
    },
    /// Record a gap event
    RecordGap {
        pane_id: u64,
        reason: String,
        respond: oneshot::Sender<Result<Option<Gap>>>,
    },
    /// Record an event/detection
    RecordEvent {
        event: StoredEvent,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Mark event as handled
    MarkEventHandled {
        event_id: i64,
        workflow_id: Option<String>,
        status: String,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Set or clear triage state for an event.
    SetEventTriageState {
        event_id: i64,
        triage_state: Option<String>,
        updated_by: Option<String>,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Set or clear the note for an event (note text is redacted before persist).
    SetEventNote {
        event_id: i64,
        note: Option<String>,
        updated_by: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Add a label to an event (idempotent).
    AddEventLabel {
        event_id: i64,
        label: String,
        created_by: Option<String>,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Remove a label from an event.
    RemoveEventLabel {
        event_id: i64,
        label: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Insert or update a persistent event mute
    UpsertEventMute {
        record: EventMuteRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Delete a persistent event mute
    DeleteEventMute {
        identity_key: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Upsert a pane record
    UpsertPane {
        pane: PaneRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert or update a workflow execution
    UpsertWorkflow {
        workflow: WorkflowRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert or update a workflow action plan
    UpsertActionPlan {
        record: WorkflowActionPlanRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert a prepared plan preview
    InsertPreparedPlan {
        record: PreparedPlanRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Consume a prepared plan (mark as used)
    ConsumePreparedPlan {
        plan_id: String,
        now_ms: i64,
        respond: oneshot::Sender<Result<Option<PreparedPlanRecord>>>,
    },
    /// Insert a workflow step log
    InsertStepLog {
        workflow_id: String,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: String,
        step_id: Option<String>,
        step_kind: Option<String>,
        result_type: String,
        result_data: Option<String>,
        policy_summary: Option<String>,
        verification_refs: Option<String>,
        error_code: Option<String>,
        started_at: i64,
        completed_at: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Upsert undo metadata for an audit action
    UpsertActionUndo {
        record: ActionUndoRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Mark an undo record as executed by setting undone_at/undone_by.
    MarkActionUndone {
        audit_action_id: i64,
        undone_at: i64,
        undone_by: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Upsert an agent session record
    UpsertSession {
        session: AgentSessionRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Record an audit action
    RecordAuditAction {
        action: AuditActionRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// ft-h90rh: insert a policy-denied audit row (Deny / RequireApproval
    /// from the MCP mutation gate). Separate from `RecordAuditAction` so the
    /// two streams stay queryable independently.
    RecordPolicyDenialAudit {
        record: PolicyDeniedAuditRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Purge audit actions older than a cutoff timestamp
    PurgeAuditActions {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Insert an approval token
    InsertApprovalToken {
        token: ApprovalTokenRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Consume (use) an approval token if it matches scope
    ConsumeApprovalToken {
        code_hash: String,
        workspace_id: String,
        action_kind: String,
        pane_id: Option<u64>,
        action_fingerprint: String,
        respond: oneshot::Sender<Result<Option<ApprovalTokenRecord>>>,
    },
    /// Consume an approval token by code hash only (without fingerprint validation)
    ConsumeApprovalTokenByCode {
        code_hash: String,
        workspace_id: String,
        respond: oneshot::Sender<Result<Option<ApprovalTokenRecord>>>,
    },
    /// Record a maintenance event
    RecordMaintenance {
        record: MaintenanceRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Record a secret scan report
    RecordSecretScanReport {
        record: SecretScanReportRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Insert a saved search definition
    InsertSavedSearch {
        record: SavedSearchRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Update last-run metadata for a saved search
    UpdateSavedSearchRun {
        id: String,
        last_run_at: i64,
        last_result_count: Option<i64>,
        last_error: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Update scheduling settings for a saved search
    UpdateSavedSearchSchedule {
        id: String,
        enabled: bool,
        schedule_interval_ms: Option<i64>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Delete a saved search by name
    DeleteSavedSearch {
        name: String,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Run deferred incremental FTS catch-up on the writer connection.
    SyncFts {
        config: FtsSyncConfig,
        respond: oneshot::Sender<Result<FtsSyncResult>>,
    },
    /// Run a full FTS rebuild on the writer connection.
    RebuildFts {
        config: FtsSyncConfig,
        respond: oneshot::Sender<Result<FtsSyncResult>>,
    },
    /// Prune output segments older than a cutoff
    PruneSegments {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Vacuum the database (explicit)
    Vacuum {
        respond: oneshot::Sender<Result<()>>,
    },
    /// Upsert an account record (insert or update by service+account_id)
    UpsertAccount {
        account: crate::accounts::AccountRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Update an account's last_used_at timestamp
    UpdateAccountLastUsed {
        service: String,
        account_id: String,
        last_used_at: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Delete an account by service and account_id
    DeleteAccount {
        service: String,
        account_id: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Create a pane reservation (exclusive lock)
    CreateReservation {
        pane_id: u64,
        owner_kind: String,
        owner_id: String,
        reason: Option<String>,
        ttl_ms: i64,
        respond: oneshot::Sender<Result<PaneReservation>>,
    },
    /// Release a pane reservation by ID
    ReleaseReservation {
        reservation_id: i64,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Expire all stale reservations (past their TTL)
    ExpireStaleReservations {
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Checkpoint WAL (incremental, non-blocking)
    Checkpoint {
        respond: oneshot::Sender<Result<CheckpointResult>>,
    },
    /// Record a usage metric
    RecordUsageMetric {
        record: UsageMetricRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Record multiple usage metrics in a single transaction.
    ///
    /// This is used by higher-level collectors to avoid DB spam when a single
    /// event produces multiple metric rows (eg, caut refresh -> N accounts).
    RecordUsageMetricsBatch {
        records: Vec<UsageMetricRecord>,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Purge usage metrics older than a cutoff timestamp
    PurgeUsageMetrics {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Record a notification in the history log
    RecordNotification {
        record: NotificationHistoryRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Update the status of a notification
    UpdateNotificationStatus {
        id: i64,
        status: NotificationStatus,
        error_message: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Acknowledge a notification
    AcknowledgeNotification {
        id: i64,
        acknowledged_by: String,
        action_taken: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Increment retry count for a notification
    IncrementNotificationRetry {
        id: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Purge notification history older than a cutoff timestamp
    PurgeNotificationHistory {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Delete events older than a cutoff (flat, no tier filters)
    DeleteEventsBefore {
        before_ts: i64,
        batch_size: usize,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Delete events matching tier criteria older than a cutoff
    DeleteEventsByTier {
        before_ts: i64,
        severities: Vec<String>,
        event_types: Vec<String>,
        handled: Option<bool>,
        batch_size: usize,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Insert a pane bookmark
    InsertPaneBookmark {
        record: PaneBookmarkRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Delete a pane bookmark by alias
    DeletePaneBookmark {
        alias: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// br-ft-dngp2 / ft-43lpu.cont: insert an agent profile.
    /// Returns the row's `name` (PRIMARY KEY) on success;
    /// duplicate names surface as `StorageError::Database`
    /// wrapping the SQLite UNIQUE constraint violation.
    InsertAgentProfile {
        profile: crate::agent_profiles::AgentProfile,
        respond: oneshot::Sender<Result<String>>,
    },
    /// br-ft-dngp2 / ft-43lpu.cont: get an agent profile by name.
    /// Returns `None` when no row matches.
    GetAgentProfile {
        name: String,
        respond: oneshot::Sender<Result<Option<crate::agent_profiles::AgentProfile>>>,
    },
    /// br-ft-dngp2 / ft-43lpu.cont: list agent profiles. When
    /// `role_filter` is `Some`, restricts to profiles with the
    /// matching `role` (uses `agent_profiles_role_idx`); when
    /// `None`, returns every row ordered by `name` ASC for
    /// stable output.
    ListAgentProfiles {
        role_filter: Option<String>,
        respond: oneshot::Sender<Result<Vec<crate::agent_profiles::AgentProfile>>>,
    },
    /// br-ft-dngp2 / ft-43lpu.cont: delete an agent profile by
    /// name. Returns `true` if a row was removed, `false` if
    /// no row matched.
    DeleteAgentProfile {
        name: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Insert a new mux session record
    InsertMuxSession {
        session_id: String,
        topology_json: String,
        ft_version: String,
        host_id: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert a session checkpoint with per-pane state
    InsertSessionCheckpoint {
        session_id: String,
        checkpoint_type: String,
        state_hash: String,
        pane_count: usize,
        total_bytes: usize,
        metadata_json: Option<String>,
        pane_states: Vec<SessionPaneStateRow>,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Prune old checkpoints beyond retention limit
    PruneSessionCheckpoints {
        session_id: String,
        retention: usize,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Mark a session as cleanly shut down
    MarkSessionShutdownClean {
        session_id: String,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Shutdown the writer thread (flush pending writes)
    Shutdown { respond: oneshot::Sender<()> },
}

impl std::fmt::Debug for WriteCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let variant = match self {
            Self::AppendSegment { .. } => "AppendSegment",
            Self::RecordGap { .. } => "RecordGap",
            Self::RecordEvent { .. } => "RecordEvent",
            Self::MarkEventHandled { .. } => "MarkEventHandled",
            Self::SetEventTriageState { .. } => "SetEventTriageState",
            Self::SetEventNote { .. } => "SetEventNote",
            Self::AddEventLabel { .. } => "AddEventLabel",
            Self::RemoveEventLabel { .. } => "RemoveEventLabel",
            Self::UpsertEventMute { .. } => "UpsertEventMute",
            Self::DeleteEventMute { .. } => "DeleteEventMute",
            Self::UpsertPane { .. } => "UpsertPane",
            Self::UpsertWorkflow { .. } => "UpsertWorkflow",
            Self::UpsertActionPlan { .. } => "UpsertActionPlan",
            Self::InsertPreparedPlan { .. } => "InsertPreparedPlan",
            Self::ConsumePreparedPlan { .. } => "ConsumePreparedPlan",
            Self::InsertStepLog { .. } => "InsertStepLog",
            Self::UpsertActionUndo { .. } => "UpsertActionUndo",
            Self::MarkActionUndone { .. } => "MarkActionUndone",
            Self::UpsertSession { .. } => "UpsertSession",
            Self::RecordAuditAction { .. } => "RecordAuditAction",
            Self::RecordPolicyDenialAudit { .. } => "RecordPolicyDenialAudit",
            Self::PurgeAuditActions { .. } => "PurgeAuditActions",
            Self::InsertApprovalToken { .. } => "InsertApprovalToken",
            Self::ConsumeApprovalToken { .. } => "ConsumeApprovalToken",
            Self::ConsumeApprovalTokenByCode { .. } => "ConsumeApprovalTokenByCode",
            Self::RecordMaintenance { .. } => "RecordMaintenance",
            Self::RecordSecretScanReport { .. } => "RecordSecretScanReport",
            Self::InsertSavedSearch { .. } => "InsertSavedSearch",
            Self::UpdateSavedSearchRun { .. } => "UpdateSavedSearchRun",
            Self::UpdateSavedSearchSchedule { .. } => "UpdateSavedSearchSchedule",
            Self::DeleteSavedSearch { .. } => "DeleteSavedSearch",
            Self::SyncFts { .. } => "SyncFts",
            Self::RebuildFts { .. } => "RebuildFts",
            Self::PruneSegments { .. } => "PruneSegments",
            Self::Vacuum { .. } => "Vacuum",
            Self::UpsertAccount { .. } => "UpsertAccount",
            Self::UpdateAccountLastUsed { .. } => "UpdateAccountLastUsed",
            Self::DeleteAccount { .. } => "DeleteAccount",
            Self::CreateReservation { .. } => "CreateReservation",
            Self::ReleaseReservation { .. } => "ReleaseReservation",
            Self::ExpireStaleReservations { .. } => "ExpireStaleReservations",
            Self::Checkpoint { .. } => "Checkpoint",
            Self::RecordUsageMetric { .. } => "RecordUsageMetric",
            Self::RecordUsageMetricsBatch { .. } => "RecordUsageMetricsBatch",
            Self::PurgeUsageMetrics { .. } => "PurgeUsageMetrics",
            Self::RecordNotification { .. } => "RecordNotification",
            Self::UpdateNotificationStatus { .. } => "UpdateNotificationStatus",
            Self::AcknowledgeNotification { .. } => "AcknowledgeNotification",
            Self::IncrementNotificationRetry { .. } => "IncrementNotificationRetry",
            Self::PurgeNotificationHistory { .. } => "PurgeNotificationHistory",
            Self::DeleteEventsBefore { .. } => "DeleteEventsBefore",
            Self::DeleteEventsByTier { .. } => "DeleteEventsByTier",
            Self::InsertPaneBookmark { .. } => "InsertPaneBookmark",
            Self::DeletePaneBookmark { .. } => "DeletePaneBookmark",
            Self::InsertAgentProfile { .. } => "InsertAgentProfile",
            Self::GetAgentProfile { .. } => "GetAgentProfile",
            Self::ListAgentProfiles { .. } => "ListAgentProfiles",
            Self::DeleteAgentProfile { .. } => "DeleteAgentProfile",
            Self::InsertMuxSession { .. } => "InsertMuxSession",
            Self::InsertSessionCheckpoint { .. } => "InsertSessionCheckpoint",
            Self::PruneSessionCheckpoints { .. } => "PruneSessionCheckpoints",
            Self::MarkSessionShutdownClean { .. } => "MarkSessionShutdownClean",
            Self::Shutdown { .. } => "Shutdown",
        };
        write!(f, "WriteCommand::{variant}")
    }
}

/// Row data for inserting into mux_pane_state.
#[derive(Debug, Clone)]
pub struct SessionPaneStateRow {
    /// WezTerm pane ID.
    pub pane_id: u64,
    /// Current working directory.
    pub cwd: Option<String>,
    /// Best-effort process name.
    pub command: Option<String>,
    /// Selected environment variables (JSON, redacted).
    pub env_json: Option<String>,
    /// Terminal state (JSON: cursor, alt-screen, scrollback ref).
    pub terminal_state_json: String,
    /// Agent metadata (JSON: agent type, session ID, state).
    pub agent_metadata_json: Option<String>,
    /// Links to output_segments.seq for replay.
    pub scrollback_checkpoint_seq: Option<i64>,
    /// Epoch ms of last captured output.
    pub last_output_at: Option<i64>,
}

/// Configuration for the storage handle
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Maximum number of pending write commands before backpressure
    pub write_queue_size: usize,
    /// [ft-wk5fo] Opt out of the synchronous FTS5 triggers.
    ///
    /// When `true`, the three `output_segments_a[iud]` triggers at
    /// storage.rs:169/173/177 are dropped immediately after schema init.
    /// New segments are NOT automatically indexed — callers MUST invoke
    /// [`StorageHandle::sync_fts`] (or the Cx-first `sync_fts_with_cx`)
    /// periodically to catch the FTS index up to the latest segments via
    /// the existing `fts_pane_progress` + `sync_fts_on_startup` engine.
    ///
    /// Default `false` preserves the immediate-indexing behavior the
    /// subsystem has always had. Flip this to `true` only in deployments
    /// that have a periodic catchup tick wired and can tolerate
    /// search-freshness lag equal to the tick interval. See ft-wk5fo for
    /// the full deferred-indexing rollout plan.
    pub defer_fts_triggers: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            write_queue_size: 1024,
            defer_fts_triggers: false,
        }
    }
}

const FT_STORAGE_MMAP_ENABLE_ENV: &str = "FT_STORAGE_MMAP_ENABLE";
const FT_STORAGE_MMAP_DIR_ENV: &str = "FT_STORAGE_MMAP_DIR";

#[derive(Debug, Clone)]
struct MmapMirrorRuntimeConfig {
    base_dir: PathBuf,
}

impl MmapMirrorRuntimeConfig {
    fn from_db_path(db_path: &str) -> Option<Self> {
        let enabled = std::env::var(FT_STORAGE_MMAP_ENABLE_ENV)
            .ok()
            .map(|value| env_value_is_truthy(&value))
            .unwrap_or(false);
        if !enabled {
            return None;
        }

        let db_path = Path::new(db_path);
        let db_parent = db_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let fallback_dir = db_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map_or_else(
                || db_parent.join("ft.mmap_scrollback"),
                |stem| db_parent.join(format!("{stem}.mmap_scrollback")),
            );

        let base_dir = std::env::var(FT_STORAGE_MMAP_DIR_ENV)
            .ok()
            .map(|raw| raw.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|candidate| {
                if candidate.is_relative() {
                    db_parent.join(candidate)
                } else {
                    candidate
                }
            })
            .unwrap_or(fallback_dir);

        Some(Self { base_dir })
    }
}

fn env_value_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let existed = parent.exists();
            std::fs::create_dir_all(parent)
                .map_err(|e| StorageError::Database(format!("Failed to create directory: {e}")))?;
            #[cfg(unix)]
            if !existed {
                set_permissions(parent, 0o700)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions).map_err(|e| {
        StorageError::Database(format!(
            "Failed to set permissions on {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(unix)]
fn ensure_db_permissions(path: &Path, is_new: bool) -> Result<()> {
    if is_new {
        set_permissions(path, 0o600)?;
    }

    let wal_path = std::path::PathBuf::from(format!("{}-wal", path.display()));
    if wal_path.exists() {
        set_permissions(&wal_path, 0o600)?;
    }

    let shm_path = std::path::PathBuf::from(format!("{}-shm", path.display()));
    if shm_path.exists() {
        set_permissions(&shm_path, 0o600)?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn ensure_db_permissions(_path: &Path, _is_new: bool) -> Result<()> {
    Ok(())
}

// =============================================================================
// Storage Handle
// =============================================================================

/// Async-safe storage handle
///
/// Provides an async API for storage operations. Writes are serialized through
/// a dedicated writer thread to avoid blocking the async runtime. Reads use
/// spawn_blocking with WAL mode for concurrent access.
#[derive(Clone)]
struct WriteCommandSender {
    inner: mpsc::Sender<WriteCommand>,
}

impl WriteCommandSender {
    fn new(inner: mpsc::Sender<WriteCommand>) -> Self {
        Self { inner }
    }

    async fn send(
        &self,
        command: WriteCommand,
    ) -> std::result::Result<(), mpsc::SendError<WriteCommand>> {
        let cx = crate::cx::for_request();
        self.inner.send(&cx, command).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`WriteCommandSender::send`].
    ///
    /// Plugs the orphan-cx hole at the root of the storage write
    /// path: the legacy `send` uses `crate::cx::for_request()` for
    /// its inner mpsc reserve wait, severing the cancellation
    /// chain from every storage `_with_cx` caller. This sibling
    /// threads the caller's cx all the way into
    /// `self.inner.send(cx, command)` so a full writer queue
    /// under a cancelled parent cx releases immediately rather
    /// than waiting for backpressure to drain.
    ///
    /// Per-call-site migration is incremental; this tick wires
    /// the 6 event-annotation writes from tick 136. Future ticks
    /// can progressively migrate the remaining ~50+ `_with_cx`
    /// storage methods. Legacy `send` stays available for
    /// ambient-cx callers.
    async fn send_with_cx(
        &self,
        cx: &crate::cx::Cx,
        command: WriteCommand,
    ) -> std::result::Result<(), mpsc::SendError<WriteCommand>> {
        self.inner.send(cx, command).await
    }

    fn max_capacity(&self) -> usize {
        self.inner.capacity()
    }

    fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

/// Thread-safe handle to the storage writer and its backing database.
#[derive(Clone)]
pub struct StorageHandle {
    /// Sender for write commands
    write_tx: WriteCommandSender,
    /// Database path for read connections
    db_path: Arc<String>,
    /// Optional mmap mirror directory for segment fast-path reads.
    mmap_mirror_dir: Option<Arc<PathBuf>>,
    /// Writer thread join handle (for shutdown) - shared to allow Clone
    writer_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Semantic budget state for hybrid search guardrails/telemetry.
    semantic_budget_state: Arc<Mutex<SemanticBudgetState>>,
}

impl StorageHandle {
    /// Create a new storage handle
    ///
    /// Opens/creates the database at `db_path`, initializes the schema,
    /// and starts the writer thread.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or schema fails.
    pub async fn new(db_path: &str) -> Result<Self> {
        Self::with_config(db_path, StorageConfig::default()).await
    }

    /// Return the database path backing this storage handle.
    #[must_use]
    pub fn db_path(&self) -> &str {
        self.db_path.as_str()
    }

    /// Update semantic latency/cost budget configuration.
    pub fn set_semantic_budget_config(&self, config: SemanticBudgetConfig) {
        if let Ok(mut state) = self.semantic_budget_state.lock() {
            state.configure(config);
        }
    }

    /// Return semantic budget telemetry snapshot for operator dashboards.
    #[must_use]
    pub fn semantic_budget_snapshot(&self) -> SemanticBudgetSnapshot {
        match self.semantic_budget_state.lock() {
            Ok(state) => state.snapshot(),
            Err(_) => SemanticBudgetSnapshot {
                config: SemanticBudgetConfig::default(),
                metrics: SemanticBudgetMetrics::default(),
                ewma_semantic_latency_ms: 0.0,
                backoff_until_ms: None,
                cache_entries: 0,
            },
        }
    }

    fn invalidate_semantic_cache(&self) {
        if let Ok(mut state) = self.semantic_budget_state.lock() {
            state.invalidate_cache();
        }
    }

    async fn spawn_blocking_storage<T, F>(work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        Self::spawn_blocking_storage_with_join_error("Task join error", work).await
    }

    async fn spawn_blocking_storage_with_join_error<T, F>(
        join_error_prefix: &'static str,
        work: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        crate::runtime_async::spawn_blocking(work)
            .await
            .map_err(|e| StorageError::Database(format!("{join_error_prefix}: {e}")))?
    }

    /// br-ft-6qoxd: Cx-aware sibling of [`spawn_blocking_storage`]
    /// that select-races the blocking JoinHandle against the
    /// caller's Cx cancellation watcher.
    ///
    /// Pre-cancel: returns immediately if `cx` is already cancelled.
    /// Mid-flight cancel: the await unblocks within ~50–100 ms with
    /// a typed cancellation error if `cx` cancels while the blocking
    /// closure is still executing. The orphaned blocking task
    /// continues to run on the blocking thread pool until the
    /// closure returns naturally; its result is discarded.
    ///
    /// See [`crate::runtime_async::spawn_blocking_with_cx`] for the
    /// full contract documentation.
    async fn spawn_blocking_storage_with_cx<T, F>(cx: &crate::cx::Cx, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", work).await
    }

    /// br-ft-6qoxd: Cx-aware sibling of
    /// [`spawn_blocking_storage_with_join_error`].
    async fn spawn_blocking_storage_with_cx_with_join_error<T, F>(
        cx: &crate::cx::Cx,
        join_error_prefix: &'static str,
        work: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        crate::runtime_async::spawn_blocking_with_cx(cx, work)
            .await
            .map_err(|e| StorageError::Database(format!("{join_error_prefix}: {e}")))?
    }

    async fn recv_writer_response<T>(rx: oneshot::Receiver<Result<T>>) -> Result<T> {
        crate::runtime_async::oneshot_recv(rx)
            .await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    async fn recv_writer_shutdown_ack(rx: oneshot::Receiver<()>) {
        let _ = crate::runtime_async::oneshot_recv(rx).await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`new`].
    ///
    /// Pre-flight checkpoint gates storage handle creation
    /// before any filesystem work (parent-dir creation, DB file
    /// open, schema init, writer thread spawn). CLI startup
    /// paths that are cx-driven can bail before taking the DB
    /// lock and spawning the writer thread.
    pub async fn new_with_cx(cx: &crate::cx::Cx, db_path: &str) -> Result<Self> {
        Self::with_config_with_cx(cx, db_path, StorageConfig::default()).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`with_config`].
    ///
    /// br-ft-8sjv4: routes through [`Self::with_config_inner`]
    /// so the legacy entry point shares the exact init path.
    /// `cx` threads through the blocking init closure via
    /// `spawn_blocking_storage_with_cx_with_join_error` (br-ft-6qoxd
    /// substrate) and the per-phase `checkpoint_storage_open` calls.
    ///
    /// Cancellation seams (in execution order):
    /// 1. **Pre-flight** — before parent-directory setup.
    /// 2. **Spawn-side** mid-flight cancel via the cx-aware
    ///    `spawn_blocking` that select-races the JoinHandle
    ///    against the cx cancel watcher (~150 ms responsiveness).
    /// 3. **Inside the init closure**, between every major
    ///    phase: before database open, after database open,
    ///    after WAL recovery, after foreign-key setup, after
    ///    schema initialization, after FTS-trigger setup, and
    ///    after permission setup. Each phase boundary
    ///    short-circuits if the cx was cancelled while the
    ///    previous phase ran. SQLite calls themselves are not
    ///    preempted (rusqlite has no progress-handler hook in
    ///    our build), so per-phase cancel responsiveness is
    ///    bounded by the slowest phase (typically
    ///    `initialize_schema` on a multi-version migration).
    /// 4. **Between schema init and writer-thread spawn** in
    ///    the outer body, so a cancelled caller doesn't leak a
    ///    background thread after the DB open succeeded.
    pub async fn with_config_with_cx(
        cx: &crate::cx::Cx,
        db_path: &str,
        config: StorageConfig,
    ) -> Result<Self> {
        Self::with_config_inner(db_path, config, Some(cx)).await
    }

    /// Create a storage handle with custom configuration.
    ///
    /// Do not hold this main-store handle concurrently with a
    /// [`crate::search::chunk_vector_store::ChunkVectorStore`] connection
    /// in the same async task. SQLite WAL isolates the usual per-file
    /// writer state, but both stores use a five-second `busy_timeout`;
    /// dual-holding the connections inside one blocking closure can make
    /// contention surface as `SQLITE_BUSY` on the slower side.
    pub async fn with_config(db_path: &str, config: StorageConfig) -> Result<Self> {
        Self::with_config_inner(db_path, config, None).await
    }

    fn checkpoint_storage_open(cx: &crate::cx::Cx, phase: &str) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("storage open cancelled {phase}: {err}")).into()
        })
    }

    async fn with_config_inner(
        db_path: &str,
        config: StorageConfig,
        cx: Option<&crate::cx::Cx>,
    ) -> Result<Self> {
        if let Some(cx) = cx {
            Self::checkpoint_storage_open(cx, "before parent directory setup")?;
        }

        // Ensure parent directory exists
        ensure_parent_dir(Path::new(db_path))?;
        let mmap_runtime = MmapMirrorRuntimeConfig::from_db_path(db_path);

        // Open connection, recover WAL if needed, and initialize schema (blocking)
        let db_path_owned = db_path.to_string();
        let db_existed = Path::new(&db_path_owned).exists();
        let defer_fts_triggers = config.defer_fts_triggers;
        let init_cx = cx.cloned();
        let open_initialized_connection = move || -> Result<Connection> {
            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "before database open")?;
            }

            let conn = Connection::open(&db_path_owned)
                .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;

            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "after database open")?;
            }

            // The primary writer connection: every subsequent operation
            // (WAL recovery, schema init, ALTER/CREATE migrations, normal
            // writes) needs the write lock. Without busy_timeout an active
            // reader (any spawn_blocking read path opened via
            // `open_read_storage_conn`) makes the very next PRAGMA fail
            // with SQLITE_BUSY. SCHEMA_SQL doesn't set this (and is
            // short-circuited on reopen of an up-to-date DB), so it must
            // be applied here on every connection-open path. The discard
            // is intentional — failure to apply busy_timeout is non-fatal,
            // just makes the writer more contention-sensitive.
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));

            // Check for and recover from unclean shutdown (wa-o8j)
            check_and_recover_wal(&conn, &db_path_owned)?;

            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "after WAL recovery")?;
            }

            // [ft-s4myu] SQLite's `PRAGMA foreign_keys` is per-connection.
            // `SCHEMA_SQL` enables it on schema init, but
            // `initialize_schema` short-circuits for up-to-date databases
            // (current == SCHEMA_VERSION → return at line ~4344 without
            // executing SCHEMA_SQL), so without this explicit pragma every
            // writer connection on a reopen of an existing DB would run
            // with whatever the SQLite runtime default is. That silently
            // disables every `ON DELETE CASCADE` in the schema
            // (mux_pane_state → session_checkpoints, output_segments →
            // panes, events → panes, and 12+ more). Concrete breakage:
            // prune_session_checkpoints_sync leaves orphan mux_pane_state
            // rows across restarts. Enforce FKs unconditionally on every
            // connection open — idempotent, O(1).
            conn.pragma_update(None, "foreign_keys", true)
                .map_err(|e| {
                    StorageError::Database(format!(
                        "Failed to enable foreign_keys PRAGMA (ft-s4myu): {e}"
                    ))
                })?;

            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "after foreign key setup")?;
            }

            initialize_schema(&conn)?;

            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "after schema initialization")?;
            }

            // [ft-wk5fo] Deferred FTS indexing: drop the three
            // per-INSERT/DELETE/UPDATE triggers on output_segments so new
            // segment writes no longer synchronously rebuild the FTS
            // inverted-index pages inside the append-segment writer
            // path. Callers are responsible for invoking
            // `StorageHandle::sync_fts` periodically to catch the index
            // up — the `fts_pane_progress` table + `sync_fts_on_startup`
            // engine already support resumable batched indexing.
            //
            // [ft-ih4tm] Both branches are now idempotent so the flag is
            // truly bidirectional. `initialize_schema` short-circuits for
            // up-to-date databases (line ~4315 — returns without
            // re-running SCHEMA_SQL), so a DB opened first with
            // `defer_fts_triggers: true` and then reopened with `false`
            // needs the explicit re-create here; otherwise the triggers
            // stay dropped and the operator's "turn deferred off" intent
            // is silently ignored. The CREATE statements mirror the ones
            // in SCHEMA_SQL at line ~169 (see FTS_TRIGGER_RECREATE_SQL);
            // keep the two in lockstep.
            if defer_fts_triggers {
                conn.execute_batch(
                    "DROP TRIGGER IF EXISTS output_segments_ai;
                     DROP TRIGGER IF EXISTS output_segments_ad;
                     DROP TRIGGER IF EXISTS output_segments_au;",
                )
                .map_err(|e| {
                    StorageError::Database(format!(
                        "Failed to drop FTS triggers for deferred indexing (ft-wk5fo): {e}"
                    ))
                })?;
            } else {
                conn.execute_batch(schema_ddl::FTS_TRIGGER_RECREATE_SQL).map_err(|e| {
                    StorageError::Database(format!(
                        "Failed to re-create FTS triggers after deferred indexing was disabled (ft-ih4tm): {e}"
                    ))
                })?;
            }

            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "after FTS trigger setup")?;
            }

            #[cfg(unix)]
            {
                ensure_db_permissions(Path::new(&db_path_owned), !db_existed)?;
            }

            if let Some(cx) = init_cx.as_ref() {
                Self::checkpoint_storage_open(cx, "after permission setup")?;
            }

            Ok(conn)
        };
        let init_result = if let Some(cx) = cx {
            Self::spawn_blocking_storage_with_cx_with_join_error(
                cx,
                "Storage open task join error",
                open_initialized_connection,
            )
            .await?
        } else {
            Self::spawn_blocking_storage(open_initialized_connection).await?
        };

        if let Some(cx) = cx {
            Self::checkpoint_storage_open(cx, "before writer thread spawn")?;
        }

        // Create bounded channel for write commands
        let (write_tx, mut write_rx) = mpsc::channel::<WriteCommand>(config.write_queue_size);
        let mmap_runtime_for_writer = mmap_runtime.clone();

        // Spawn writer thread
        let writer_handle = thread::Builder::new()
            .name("ft-storage-writer".to_string())
            .spawn(move || {
                let mut conn = init_result;
                let mut mmap_mirror = init_mmap_mirror_store(mmap_runtime_for_writer.as_ref());
                writer_loop(&mut conn, &mut write_rx, &mut mmap_mirror);
            })
            .map_err(|e| {
                StorageError::Database(format!("Failed to spawn storage writer thread: {e}"))
            })?;

        Ok(Self {
            write_tx: WriteCommandSender::new(write_tx),
            db_path: Arc::new(db_path.to_string()),
            mmap_mirror_dir: mmap_runtime.map(|runtime| Arc::new(runtime.base_dir)),
            writer_handle: Arc::new(Mutex::new(Some(writer_handle))),
            semantic_budget_state: Arc::new(Mutex::new(SemanticBudgetState::new(
                SemanticBudgetConfig::default(),
            ))),
        })
    }

    /// Append a segment to storage
    ///
    /// Automatically assigns the next sequence number for the pane.
    /// The pane must exist (call `upsert_pane` first).
    pub async fn append_segment(
        &self,
        pane_id: u64,
        content: &str,
        content_hash: Option<String>,
    ) -> Result<Segment> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.append_segment_with_cx(&cx, pane_id, content, content_hash)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`append_segment`].
    ///
    /// Pre-flight checkpoint gates the segment-write hot path
    /// before the command is enqueued on the writer channel.
    /// The ingest pipeline lives downstream of this method, so
    /// threading cx here lets a cx-cancelled observation loop
    /// bail before enqueuing the write.
    ///
    /// Tick 175: inlined to route the mpsc send through
    /// `send_with_cx` — segment writes are the highest-volume
    /// per-pane ingest path.
    pub async fn append_segment_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        content: &str,
        content_hash: Option<String>,
    ) -> Result<Segment> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("append_segment cancelled: {err}")))?;
        let timer = SwarmCapacityStageTimer::start(SwarmCapacityStage::StorageWrite, 0);
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::AppendSegment {
                    pane_id,
                    content: content.to_string(),
                    content_hash,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        let result = Self::recv_writer_response(rx).await;
        timer.finish_result(&result);
        result
    }

    /// Record a gap event
    ///
    /// Indicates a discontinuity in capture for the given pane.
    /// Returns `None` if the gap was skipped (e.g. at start of stream).
    pub async fn record_gap(&self, pane_id: u64, reason: &str) -> Result<Option<Gap>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_gap_with_cx(&cx, pane_id, reason).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_gap`].
    ///
    /// Pre-flight checkpoint gates gap recording. Called from
    /// ingest detector paths where caller cx propagation is
    /// common.
    ///
    /// Tick 175: inlined to route the mpsc send through `send_with_cx`.
    pub async fn record_gap_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        reason: &str,
    ) -> Result<Option<Gap>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("record_gap cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordGap {
                    pane_id,
                    reason: reason.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Record an event (pattern detection)
    ///
    /// Returns the event ID.
    pub async fn record_event(&self, event: StoredEvent) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_event_with_cx(&cx, event).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_event`].
    ///
    /// Pre-flight checkpoint gates event recording — the hot
    /// write path for pattern detections. A cx-driven detection
    /// pipeline can bail before enqueuing the write if the
    /// caller has already been cancelled.
    ///
    /// Tick 174: inlined to route the mpsc send through
    /// `send_with_cx` so a backpressured writer queue releases
    /// immediately under caller cancellation.
    pub async fn record_event_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event: StoredEvent,
    ) -> Result<i64> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("record_event cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(cx, WriteCommand::RecordEvent { event, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Mark an event as handled
    pub async fn mark_event_handled(
        &self,
        event_id: i64,
        workflow_id: Option<String>,
        status: &str,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.mark_event_handled_with_cx(&cx, event_id, workflow_id, status)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`mark_event_handled`].
    ///
    /// Tick 169: inlined the write-send so the mpsc reserve wait
    /// routes through `write_tx.send_with_cx(cx, ...)`. Prior to
    /// tick 169 this delegated to the legacy `mark_event_handled`,
    /// which under asupersync-runtime reserves with an orphan
    /// `cx::for_request()` — a latent hole in the cancellation
    /// chain when the writer queue is backpressured.
    pub async fn mark_event_handled_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
        workflow_id: Option<String>,
        status: &str,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("mark_event_handled cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::MarkEventHandled {
                    event_id,
                    workflow_id,
                    status: status.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Set or clear an event's triage state.
    ///
    /// Returns true if an event row was updated.
    pub async fn set_event_triage_state(
        &self,
        event_id: i64,
        triage_state: Option<String>,
        updated_by: Option<String>,
    ) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.set_event_triage_state_with_cx(&cx, event_id, triage_state, updated_by)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`set_event_triage_state`].
    /// Tick 169: inlined to route the mpsc send through `send_with_cx`.
    pub async fn set_event_triage_state_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
        triage_state: Option<String>,
        updated_by: Option<String>,
    ) -> Result<bool> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("set_event_triage_state cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::SetEventTriageState {
                    event_id,
                    triage_state,
                    updated_by,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Set or clear an event's note.
    ///
    /// Note text is redacted before being persisted.
    pub async fn set_event_note(
        &self,
        event_id: i64,
        note: Option<String>,
        updated_by: Option<String>,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.set_event_note_with_cx(&cx, event_id, note, updated_by)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`set_event_note`].
    /// Tick 169: inlined to route the mpsc send through `send_with_cx`.
    pub async fn set_event_note_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
        note: Option<String>,
        updated_by: Option<String>,
    ) -> Result<()> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("set_event_note cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::SetEventNote {
                    event_id,
                    note,
                    updated_by,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Add a label to an event.
    ///
    /// Returns true if a new label row was inserted.
    pub async fn add_event_label(
        &self,
        event_id: i64,
        label: String,
        created_by: Option<String>,
    ) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.add_event_label_with_cx(&cx, event_id, label, created_by)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`add_event_label`].
    /// Tick 169: inlined to route the mpsc send through `send_with_cx`.
    pub async fn add_event_label_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
        label: String,
        created_by: Option<String>,
    ) -> Result<bool> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("add_event_label cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::AddEventLabel {
                    event_id,
                    label,
                    created_by,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Remove a label from an event.
    ///
    /// Returns true if a label row was deleted.
    pub async fn remove_event_label(&self, event_id: i64, label: String) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.remove_event_label_with_cx(&cx, event_id, label).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`remove_event_label`].
    /// Tick 169: inlined to route the mpsc send through `send_with_cx`.
    pub async fn remove_event_label_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
        label: String,
    ) -> Result<bool> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("remove_event_label cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RemoveEventLabel {
                    event_id,
                    label,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Fetch triage state, note, and labels for an event.
    pub async fn get_event_annotations(&self, event_id: i64) -> Result<Option<EventAnnotations>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_event_annotations_with_cx(&cx, event_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_event_annotations`].
    pub async fn get_event_annotations_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
    ) -> Result<Option<EventAnnotations>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_event_annotations cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_event_annotations_backend(backend, event_id)
            })
        })
        .await
    }

    // [ft-aw52a / ft-dn2tu Phase 6] event-mute methods extracted to
    // `storage/handle/event_mutes.rs`. The `impl StorageHandle` block
    // there carries `add_event_mute`, `remove_event_mute`,
    // `is_event_muted`, `list_active_mutes` (+ each `_with_cx`
    // sibling). They reach `self.write_tx` and friends via the
    // `storage::handle::*` submodule descendant relationship.

    /// Fetch an event's dedupe/identity key by ID.
    pub async fn get_event_identity_key(&self, event_id: i64) -> Result<Option<String>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_event_identity_key_with_cx(&cx, event_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_event_identity_key`].
    pub async fn get_event_identity_key_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_id: i64,
    ) -> Result<Option<String>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_event_identity_key cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_event_identity_key_backend(backend, event_id)
            })
        })
        .await
    }

    /// Record an audit action
    pub async fn record_audit_action(&self, action: AuditActionRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_audit_action_with_cx(&cx, action).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_audit_action`].
    ///
    /// Pre-flight checkpoint gates audit writes. Audit-trail
    /// emitters are on a hot path (14+ call sites) — a cx-driven
    /// caller bails before enqueuing the write if cancelled.
    ///
    /// Tick 174: inlined to route the mpsc send through `send_with_cx`.
    pub async fn record_audit_action_with_cx(
        &self,
        cx: &crate::cx::Cx,
        action: AuditActionRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_audit_action cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordAuditAction {
                    action,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Record an audit action after applying redaction
    pub async fn record_audit_action_redacted(&self, action: AuditActionRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_audit_action_redacted_with_cx(&cx, action).await
    }

    /// ft-h90rh: persist a policy-denied audit row (Deny / RequireApproval
    /// from the MCP mutation gate). Complements
    /// `record_audit_action_redacted` but writes to the dedicated
    /// `policy_denied_audit` table. `reason` is expected to already be
    /// policy-engine-redacted; this method does not re-redact.
    pub async fn record_policy_denial_audit(&self, record: PolicyDeniedAuditRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_policy_denial_audit_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_policy_denial_audit`].
    pub async fn record_policy_denial_audit_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: PolicyDeniedAuditRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_policy_denial_audit cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordPolicyDenialAudit {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_audit_action_redacted`].
    ///
    /// Redacts in-process (synchronous), then routes through the
    /// cx-first record_audit_action path. 11+ callsites.
    pub async fn record_audit_action_redacted_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mut action: AuditActionRecord,
    ) -> Result<i64> {
        let redactor = Redactor::new();
        action.redact_fields(&redactor);
        self.record_audit_action_with_cx(cx, action).await
    }

    /// Upsert undo metadata for an audit action
    pub async fn upsert_action_undo(&self, record: ActionUndoRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_action_undo_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_action_undo`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn upsert_action_undo_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: ActionUndoRecord,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("upsert_action_undo cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpsertActionUndo {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Upsert undo metadata after applying redaction
    pub async fn upsert_action_undo_redacted(&self, record: ActionUndoRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_action_undo_redacted_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_action_undo_redacted`].
    ///
    /// Routes the post-redaction persistence through
    /// `upsert_action_undo_with_cx` so the full composite honours
    /// cancellation.
    pub async fn upsert_action_undo_redacted_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mut record: ActionUndoRecord,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("upsert_action_undo_redacted cancelled: {err}"))
        })?;
        let redactor = Redactor::new();
        record.redact_fields(&redactor);
        self.upsert_action_undo_with_cx(cx, record).await
    }

    /// Fetch undo metadata for a specific audit action ID.
    pub async fn get_action_undo(&self, audit_action_id: i64) -> Result<Option<ActionUndoRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_action_undo_with_cx(&cx, audit_action_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_action_undo`].
    pub async fn get_action_undo_with_cx(
        &self,
        cx: &crate::cx::Cx,
        audit_action_id: i64,
    ) -> Result<Option<ActionUndoRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_action_undo cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_action_undo_backend(backend, audit_action_id)
            })
        })
        .await
    }

    /// Mark an undo record as executed.
    ///
    /// Returns `true` when the row was updated and `false` when the target
    /// action was already undone, non-undoable, or missing undo metadata.
    pub async fn mark_action_undone(&self, audit_action_id: i64, undone_by: &str) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.mark_action_undone_with_cx(&cx, audit_action_id, undone_by)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`mark_action_undone`].
    pub async fn mark_action_undone_with_cx(
        &self,
        cx: &crate::cx::Cx,
        audit_action_id: i64,
        undone_by: &str,
    ) -> Result<bool> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("mark_action_undone cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::MarkActionUndone {
                    audit_action_id,
                    undone_at: now_ms(),
                    undone_by: undone_by.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Purge audit actions older than a cutoff timestamp
    pub async fn purge_audit_actions_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.purge_audit_actions_before_with_cx(&cx, before_ts)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`purge_audit_actions_before`].
    pub async fn purge_audit_actions_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("purge_audit_actions_before cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::PurgeAuditActions {
                    before_ts,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Record a maintenance event
    pub async fn record_maintenance(&self, record: MaintenanceRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_maintenance_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_maintenance`].
    /// Tick 175: inlined to route the mpsc send through `send_with_cx`.
    pub async fn record_maintenance_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: MaintenanceRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_maintenance cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordMaintenance {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Record a secret scan report (checkpoint + payload).
    pub async fn record_secret_scan_report(&self, record: SecretScanReportRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_secret_scan_report_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_secret_scan_report`].
    pub async fn record_secret_scan_report_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: SecretScanReportRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_secret_scan_report cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordSecretScanReport {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Insert a saved search definition.
    pub async fn insert_saved_search(&self, record: SavedSearchRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_saved_search_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_saved_search`].
    /// Tick 170: inlined to route the mpsc send through `send_with_cx`.
    pub async fn insert_saved_search_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: SavedSearchRecord,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_saved_search cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertSavedSearch {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Update last-run metadata for a saved search.
    pub async fn update_saved_search_run(
        &self,
        id: &str,
        last_run_at: i64,
        last_result_count: Option<i64>,
        last_error: Option<String>,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.update_saved_search_run_with_cx(&cx, id, last_run_at, last_result_count, last_error)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`update_saved_search_run`].
    /// Tick 170: inlined to route the mpsc send through `send_with_cx`.
    pub async fn update_saved_search_run_with_cx(
        &self,
        cx: &crate::cx::Cx,
        id: &str,
        last_run_at: i64,
        last_result_count: Option<i64>,
        last_error: Option<String>,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("update_saved_search_run cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpdateSavedSearchRun {
                    id: id.to_string(),
                    last_run_at,
                    last_result_count,
                    last_error,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Update scheduling settings for a saved search.
    pub async fn update_saved_search_schedule(
        &self,
        id: &str,
        enabled: bool,
        schedule_interval_ms: Option<i64>,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.update_saved_search_schedule_with_cx(&cx, id, enabled, schedule_interval_ms)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`update_saved_search_schedule`].
    /// Tick 170: inlined to route the mpsc send through `send_with_cx`.
    pub async fn update_saved_search_schedule_with_cx(
        &self,
        cx: &crate::cx::Cx,
        id: &str,
        enabled: bool,
        schedule_interval_ms: Option<i64>,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("update_saved_search_schedule cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpdateSavedSearchSchedule {
                    id: id.to_string(),
                    enabled,
                    schedule_interval_ms,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Delete a saved search by name. Returns number of rows deleted.
    pub async fn delete_saved_search(&self, name: &str) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_saved_search_with_cx(&cx, name).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`delete_saved_search`].
    /// Tick 170: inlined to route the mpsc send through `send_with_cx`.
    pub async fn delete_saved_search_with_cx(
        &self,
        cx: &crate::cx::Cx,
        name: &str,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("delete_saved_search cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeleteSavedSearch {
                    name: name.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Fetch a saved search by name.
    pub async fn get_saved_search_by_name(&self, name: &str) -> Result<Option<SavedSearchRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_saved_search_by_name_with_cx(&cx, name).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_saved_search_by_name`].
    pub async fn get_saved_search_by_name_with_cx(
        &self,
        cx: &crate::cx::Cx,
        name: &str,
    ) -> Result<Option<SavedSearchRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_saved_search_by_name cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let name = name.to_string();
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-3twzm: use the pooled backend so the
            // br-ft-l1jgo migration doesn't bypass ft-bhyxz's
            // read-connection pool.
            pooled_backend(db_path.as_str(), |backend| {
                query_saved_search_by_name_backend(backend, &name)
            })
        })
        .await
    }

    /// List saved searches in deterministic order.
    pub async fn list_saved_searches(&self) -> Result<Vec<SavedSearchRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_saved_searches_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`list_saved_searches`].
    pub async fn list_saved_searches_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<SavedSearchRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("list_saved_searches cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-l4yxp: trait-typed pooled_backend (was
            // pooled_rusqlite_backend per br-ft-3twzm; promoted to
            // type-enforced trait surface for strict swap-readiness).
            pooled_backend(db_path.as_str(), list_saved_searches_backend)
        })
        .await
    }

    /// Insert a pane bookmark. Returns the row ID.
    pub async fn insert_pane_bookmark(&self, record: PaneBookmarkRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_pane_bookmark_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_pane_bookmark`].
    /// Tick 170: inlined to route the mpsc send through `send_with_cx`.
    pub async fn insert_pane_bookmark_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: PaneBookmarkRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_pane_bookmark cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertPaneBookmark {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Delete a pane bookmark by alias. Returns true if a row was deleted.
    pub async fn delete_pane_bookmark(&self, alias: &str) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_pane_bookmark_with_cx(&cx, alias).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`delete_pane_bookmark`].
    /// Tick 170: inlined to route the mpsc send through `send_with_cx`.
    pub async fn delete_pane_bookmark_with_cx(
        &self,
        cx: &crate::cx::Cx,
        alias: &str,
    ) -> Result<bool> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("delete_pane_bookmark cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeletePaneBookmark {
                    alias: alias.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    // ------------------------------------------------------------------------
    // br-ft-dngp2 / ft-43lpu.cont: agent_profiles async surface.
    // Mirrors the insert_pane_bookmark pattern: legacy method
    // installs a Cx via `current()` / `for_request()` then
    // delegates to the `_with_cx` sibling.
    // ------------------------------------------------------------------------

    /// Insert an agent profile. Returns the row's `name` (PRIMARY KEY)
    /// on success; duplicate names surface as a SQLite UNIQUE
    /// constraint violation wrapped in `StorageError::Database`.
    pub async fn insert_agent_profile(
        &self,
        profile: crate::agent_profiles::AgentProfile,
    ) -> Result<String> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_agent_profile_with_cx(&cx, profile).await
    }

    /// br-ft-dngp2: Cx-first sibling of [`Self::insert_agent_profile`].
    pub async fn insert_agent_profile_with_cx(
        &self,
        cx: &crate::cx::Cx,
        profile: crate::agent_profiles::AgentProfile,
    ) -> Result<String> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_agent_profile cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertAgentProfile {
                    profile,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get an agent profile by name. Returns `None` when no row
    /// matches.
    pub async fn get_agent_profile(
        &self,
        name: &str,
    ) -> Result<Option<crate::agent_profiles::AgentProfile>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_agent_profile_with_cx(&cx, name).await
    }

    /// br-ft-dngp2: Cx-first sibling of [`Self::get_agent_profile`].
    pub async fn get_agent_profile_with_cx(
        &self,
        cx: &crate::cx::Cx,
        name: &str,
    ) -> Result<Option<crate::agent_profiles::AgentProfile>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_agent_profile cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::GetAgentProfile {
                    name: name.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// List agent profiles. When `role_filter` is `Some`, restricts
    /// to rows with that `role`; when `None`, returns every row
    /// ordered by `name` ASC.
    pub async fn list_agent_profiles(
        &self,
        role_filter: Option<&str>,
    ) -> Result<Vec<crate::agent_profiles::AgentProfile>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_agent_profiles_with_cx(&cx, role_filter).await
    }

    /// br-ft-dngp2: Cx-first sibling of [`Self::list_agent_profiles`].
    pub async fn list_agent_profiles_with_cx(
        &self,
        cx: &crate::cx::Cx,
        role_filter: Option<&str>,
    ) -> Result<Vec<crate::agent_profiles::AgentProfile>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("list_agent_profiles cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::ListAgentProfiles {
                    role_filter: role_filter.map(str::to_string),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Delete an agent profile by name. Returns `true` if a row
    /// was removed, `false` if no row matched.
    pub async fn delete_agent_profile(&self, name: &str) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_agent_profile_with_cx(&cx, name).await
    }

    /// br-ft-dngp2: Cx-first sibling of [`Self::delete_agent_profile`].
    pub async fn delete_agent_profile_with_cx(
        &self,
        cx: &crate::cx::Cx,
        name: &str,
    ) -> Result<bool> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("delete_agent_profile cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeleteAgentProfile {
                    name: name.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get a pane bookmark by alias.
    pub async fn get_pane_bookmark_by_alias(
        &self,
        alias: &str,
    ) -> Result<Option<PaneBookmarkRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_pane_bookmark_by_alias_with_cx(&cx, alias).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_pane_bookmark_by_alias`].
    pub async fn get_pane_bookmark_by_alias_with_cx(
        &self,
        cx: &crate::cx::Cx,
        alias: &str,
    ) -> Result<Option<PaneBookmarkRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_pane_bookmark_by_alias cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let alias = alias.to_string();
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-l4yxp: trait-typed pool helper.
            pooled_backend(db_path.as_str(), |backend| {
                query_pane_bookmark_by_alias_backend(backend, &alias)
            })
        })
        .await
    }

    /// List all pane bookmarks in alias order.
    pub async fn list_pane_bookmarks(&self) -> Result<Vec<PaneBookmarkRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_pane_bookmarks_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`list_pane_bookmarks`].
    pub async fn list_pane_bookmarks_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<PaneBookmarkRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("list_pane_bookmarks cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-l4yxp: trait-typed pool helper.
            pooled_backend(db_path.as_str(), list_pane_bookmarks_backend)
        })
        .await
    }

    /// List pane bookmarks filtered by tag.
    pub async fn list_pane_bookmarks_by_tag(&self, tag: &str) -> Result<Vec<PaneBookmarkRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_pane_bookmarks_by_tag_with_cx(&cx, tag).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`list_pane_bookmarks_by_tag`].
    pub async fn list_pane_bookmarks_by_tag_with_cx(
        &self,
        cx: &crate::cx::Cx,
        tag: &str,
    ) -> Result<Vec<PaneBookmarkRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("list_pane_bookmarks_by_tag cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let tag = tag.to_string();
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-l4yxp: trait-typed pool helper.
            pooled_backend(db_path.as_str(), |backend| {
                list_pane_bookmarks_by_tag_backend(backend, &tag)
            })
        })
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`prune_segments_before`].
    pub async fn prune_segments_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("prune_segments_before cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::PruneSegments {
                    before_ts,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Prune output segments older than a cutoff timestamp
    pub async fn prune_segments_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.prune_segments_before_with_cx(&cx, before_ts).await
    }

    /// Run retention cleanup and log the maintenance event
    pub async fn retention_cleanup(&self, before_ts: i64) -> Result<usize> {
        let deleted = self.prune_segments_before(before_ts).await?;
        let metadata = serde_json::json!({
            "deleted_segments": deleted,
            "before_ts": before_ts,
        })
        .to_string();
        let record = MaintenanceRecord {
            id: 0,
            event_type: "retention_cleanup".to_string(),
            message: Some(format!("Deleted {deleted} output segments")),
            metadata: Some(metadata),
            timestamp: now_ms(),
        };
        let _ = self.record_maintenance(record).await?;
        Ok(deleted)
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`retention_cleanup`].
    ///
    /// Routes both the prune and the maintenance-log write through their
    /// cx-first siblings so the full composite honours cancellation.
    pub async fn retention_cleanup_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("retention_cleanup cancelled: {err}")))?;
        let deleted = self.prune_segments_before_with_cx(cx, before_ts).await?;
        let metadata = serde_json::json!({
            "deleted_segments": deleted,
            "before_ts": before_ts,
        })
        .to_string();
        let record = MaintenanceRecord {
            id: 0,
            event_type: "retention_cleanup".to_string(),
            message: Some(format!("Deleted {deleted} output segments")),
            metadata: Some(metadata),
            timestamp: now_ms(),
        };
        let _ = self.record_maintenance_with_cx(cx, record).await?;
        Ok(deleted)
    }

    /// Record a usage metric for analytics tracking.
    pub async fn record_usage_metric(&self, record: UsageMetricRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_usage_metric_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_usage_metric`].
    /// Tick 175: inlined to route the mpsc send through `send_with_cx`.
    pub async fn record_usage_metric_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: UsageMetricRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_usage_metric cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordUsageMetric {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Record multiple usage metrics for analytics tracking in a single transaction.
    ///
    /// Returns the number of rows inserted.
    pub async fn record_usage_metrics_batch(
        &self,
        records: Vec<UsageMetricRecord>,
    ) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_usage_metrics_batch_with_cx(&cx, records).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_usage_metrics_batch`].
    pub async fn record_usage_metrics_batch_with_cx(
        &self,
        cx: &crate::cx::Cx,
        records: Vec<UsageMetricRecord>,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_usage_metrics_batch cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordUsageMetricsBatch {
                    records,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Purge usage metrics older than a cutoff timestamp.
    pub async fn purge_usage_metrics(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.purge_usage_metrics_with_cx(&cx, before_ts).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`purge_usage_metrics`].
    pub async fn purge_usage_metrics_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("purge_usage_metrics cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::PurgeUsageMetrics {
                    before_ts,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Query usage metrics with filters (read-only, uses read connection).
    pub async fn query_usage_metrics(&self, query: MetricQuery) -> Result<Vec<UsageMetricRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.query_usage_metrics_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`query_usage_metrics`].
    pub async fn query_usage_metrics_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: MetricQuery,
    ) -> Result<Vec<UsageMetricRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("query_usage_metrics cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    query_usage_metrics_backend(backend, &query)
                })
            },
        )
        .await
    }

    /// Get daily aggregated metric summaries since a given timestamp.
    pub async fn aggregate_daily_metrics(&self, since_ts: i64) -> Result<Vec<DailyMetricSummary>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.aggregate_daily_metrics_with_cx(&cx, since_ts).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`aggregate_daily_metrics`].
    pub async fn aggregate_daily_metrics_with_cx(
        &self,
        cx: &crate::cx::Cx,
        since_ts: i64,
    ) -> Result<Vec<DailyMetricSummary>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("aggregate_daily_metrics cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    aggregate_daily_backend(backend, since_ts)
                })
            },
        )
        .await
    }

    /// Get per-agent metric breakdown since a given timestamp.
    pub async fn aggregate_by_agent(&self, since_ts: i64) -> Result<Vec<AgentMetricBreakdown>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.aggregate_by_agent_with_cx(&cx, since_ts).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`aggregate_by_agent`].
    pub async fn aggregate_by_agent_with_cx(
        &self,
        cx: &crate::cx::Cx,
        since_ts: i64,
    ) -> Result<Vec<AgentMetricBreakdown>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("aggregate_by_agent cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    aggregate_by_agent_backend(backend, since_ts)
                })
            },
        )
        .await
    }

    // ---- Notification History ----

    /// Record a notification in the persistent history log.
    pub async fn record_notification(&self, record: NotificationHistoryRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.record_notification_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`record_notification`].
    /// Tick 171: inlined to route the mpsc send through `send_with_cx`.
    pub async fn record_notification_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: NotificationHistoryRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("record_notification cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RecordNotification {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Update the delivery status of a notification.
    pub async fn update_notification_status(
        &self,
        id: i64,
        status: NotificationStatus,
        error_message: Option<String>,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.update_notification_status_with_cx(&cx, id, status, error_message)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`update_notification_status`].
    /// Tick 171: inlined to route the mpsc send through `send_with_cx`.
    pub async fn update_notification_status_with_cx(
        &self,
        cx: &crate::cx::Cx,
        id: i64,
        status: NotificationStatus,
        error_message: Option<String>,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("update_notification_status cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpdateNotificationStatus {
                    id,
                    status,
                    error_message,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Acknowledge a notification (marks when and by whom).
    pub async fn acknowledge_notification(
        &self,
        id: i64,
        acknowledged_by: String,
        action_taken: Option<String>,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.acknowledge_notification_with_cx(&cx, id, acknowledged_by, action_taken)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`acknowledge_notification`].
    /// Tick 171: inlined to route the mpsc send through `send_with_cx`.
    pub async fn acknowledge_notification_with_cx(
        &self,
        cx: &crate::cx::Cx,
        id: i64,
        acknowledged_by: String,
        action_taken: Option<String>,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("acknowledge_notification cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::AcknowledgeNotification {
                    id,
                    acknowledged_by,
                    action_taken,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Increment the retry count for a notification and reset its status to pending.
    pub async fn increment_notification_retry(&self, id: i64) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.increment_notification_retry_with_cx(&cx, id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`increment_notification_retry`].
    /// Tick 171: inlined to route the mpsc send through `send_with_cx`.
    pub async fn increment_notification_retry_with_cx(
        &self,
        cx: &crate::cx::Cx,
        id: i64,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("increment_notification_retry cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::IncrementNotificationRetry { id, respond: tx },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Purge notification history older than the given timestamp.
    pub async fn purge_notification_history(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.purge_notification_history_with_cx(&cx, before_ts)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`purge_notification_history`].
    pub async fn purge_notification_history_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("purge_notification_history cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::PurgeNotificationHistory {
                    before_ts,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    // =========================================================================
    // Cleanup engine: count + delete helpers
    // =========================================================================

    /// Count output_segments older than a cutoff (read-path).
    pub async fn count_segments_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_segments_before_with_cx(&cx, before_ts).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_segments_before`].
    pub async fn count_segments_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_segments_before cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    count_segments_before_backend(backend, before_ts)
                })
            },
        )
        .await
    }

    /// Count events older than a cutoff (flat, no tier filters; read-path).
    pub async fn count_events_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_events_before_with_cx(&cx, before_ts).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_events_before`].
    pub async fn count_events_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_events_before cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    count_events_before_backend(backend, before_ts)
                })
            },
        )
        .await
    }

    /// Count events matching tier criteria older than a cutoff (read-path).
    pub async fn count_events_by_tier(
        &self,
        before_ts: i64,
        severities: &[String],
        event_types: &[String],
        handled: Option<bool>,
    ) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_events_by_tier_with_cx(&cx, before_ts, severities, event_types, handled)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_events_by_tier`].
    pub async fn count_events_by_tier_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
        severities: &[String],
        event_types: &[String],
        handled: Option<bool>,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_events_by_tier cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let severities = severities.to_vec();
        let event_types = event_types.to_vec();
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    count_events_by_tier_backend(
                        backend,
                        before_ts,
                        &severities,
                        &event_types,
                        handled,
                    )
                })
            },
        )
        .await
    }

    /// Count audit_actions older than a cutoff (read-path).
    pub async fn count_audit_actions_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_audit_actions_before_with_cx(&cx, before_ts)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_audit_actions_before`].
    pub async fn count_audit_actions_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_audit_actions_before cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    count_audit_actions_before_backend(backend, before_ts)
                })
            },
        )
        .await
    }

    /// Count usage_metrics older than a cutoff (read-path).
    pub async fn count_usage_metrics_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_usage_metrics_before_with_cx(&cx, before_ts)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_usage_metrics_before`].
    pub async fn count_usage_metrics_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_usage_metrics_before cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    count_usage_metrics_before_backend(backend, before_ts)
                })
            },
        )
        .await
    }

    /// Count notification_history older than a cutoff (read-path).
    pub async fn count_notification_history_before(&self, before_ts: i64) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_notification_history_before_with_cx(&cx, before_ts)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_notification_history_before`].
    pub async fn count_notification_history_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!(
                "count_notification_history_before cancelled: {err}"
            ))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    count_notification_history_before_backend(backend, before_ts)
                })
            },
        )
        .await
    }

    /// Delete events older than a cutoff (flat, no tier; write-path).
    pub async fn delete_events_before(&self, before_ts: i64, batch_size: usize) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_events_before_with_cx(&cx, before_ts, batch_size)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`delete_events_before`].
    pub async fn delete_events_before_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
        batch_size: usize,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("delete_events_before cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeleteEventsBefore {
                    before_ts,
                    batch_size,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Delete events matching tier criteria older than a cutoff (write-path).
    pub async fn delete_events_by_tier(
        &self,
        before_ts: i64,
        severities: &[String],
        event_types: &[String],
        handled: Option<bool>,
        batch_size: usize,
    ) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_events_by_tier_with_cx(
            &cx,
            before_ts,
            severities,
            event_types,
            handled,
            batch_size,
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`delete_events_by_tier`].
    pub async fn delete_events_by_tier_with_cx(
        &self,
        cx: &crate::cx::Cx,
        before_ts: i64,
        severities: &[String],
        event_types: &[String],
        handled: Option<bool>,
        batch_size: usize,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("delete_events_by_tier cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeleteEventsByTier {
                    before_ts,
                    severities: severities.to_vec(),
                    event_types: event_types.to_vec(),
                    handled,
                    batch_size,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Query notification history with filters.
    pub async fn query_notification_history(
        &self,
        query: NotificationHistoryQuery,
    ) -> Result<Vec<NotificationHistoryRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.query_notification_history_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`query_notification_history`].
    pub async fn query_notification_history_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: NotificationHistoryQuery,
    ) -> Result<Vec<NotificationHistoryRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("query_notification_history cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    query_notification_history_backend(backend, &query)
                })
            },
        )
        .await
    }

    /// Get a single notification by ID.
    pub async fn get_notification(&self, id: i64) -> Result<NotificationHistoryRecord> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_notification_with_cx(&cx, id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_notification`].
    pub async fn get_notification_with_cx(
        &self,
        cx: &crate::cx::Cx,
        id: i64,
    ) -> Result<NotificationHistoryRecord> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_notification cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Spawn blocking failed",
            move || {
                pooled_backend(db_path.as_str(), |backend| {
                    get_notification_backend(backend, id)
                })
            },
        )
        .await
    }

    /// Current write queue depth (pending commands waiting for the writer thread).
    ///
    /// Computed as `max_capacity - available_capacity`.  Zero means the writer
    /// is idle; approaching `write_queue_size` means backpressure.
    pub fn write_queue_depth(&self) -> usize {
        self.write_tx.max_capacity() - self.write_tx.capacity()
    }

    /// Maximum write queue capacity (from `StorageConfig.write_queue_size`).
    pub fn write_queue_capacity(&self) -> usize {
        self.write_tx.max_capacity()
    }

    /// Vacuum the database (explicit)
    pub async fn vacuum(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::Vacuum { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`vacuum`].
    /// Tick 171: inlined to route the mpsc send through `send_with_cx`.
    pub async fn vacuum_with_cx(&self, cx: &crate::cx::Cx) -> Result<()> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("vacuum cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(cx, WriteCommand::Vacuum { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Lightweight WAL checkpoint (PASSIVE) + PRAGMA optimize.
    ///
    /// Prefer this over `vacuum()` for periodic maintenance — it is
    /// non-blocking and much cheaper than a full VACUUM.
    pub async fn checkpoint(&self) -> Result<CheckpointResult> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::Checkpoint { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`checkpoint`].
    /// Tick 171: inlined to route the mpsc send through `send_with_cx`.
    pub async fn checkpoint_with_cx(&self, cx: &crate::cx::Cx) -> Result<CheckpointResult> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("checkpoint cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(cx, WriteCommand::Checkpoint { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Read SQLite page statistics used to decide whether VACUUM is worthwhile.
    pub async fn database_page_stats(&self) -> Result<DatabasePageStats> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.database_page_stats_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`database_page_stats`].
    pub async fn database_page_stats_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<DatabasePageStats> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("database_page_stats cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-gz4dt: trait-typed pool helper; PRAGMA page_count
            // and freelist_count routed via pragma_value helper which
            // is read-only and trait-method-only (query_row_strings).
            pooled_backend(db_path.as_str(), database_page_stats_backend)
        })
        .await
    }

    /// Get per-pane indexing statistics (read-only, uses read connection).
    pub async fn get_pane_indexing_stats(&self) -> Result<Vec<PaneIndexingStats>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_pane_indexing_stats_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_pane_indexing_stats`].
    pub async fn get_pane_indexing_stats_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<PaneIndexingStats>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_pane_indexing_stats cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), get_pane_indexing_stats_backend)
        })
        .await
    }

    /// Get a full indexing health report (per-pane stats + FTS integrity).
    pub async fn get_indexing_health(&self) -> Result<IndexingHealthReport> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_indexing_health_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_indexing_health`].
    pub async fn get_indexing_health_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<IndexingHealthReport> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_indexing_health cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                let stats = get_pane_indexing_stats_backend(backend)?;
                let fts_ok = check_fts_integrity_backend(backend)?;
                Ok(build_indexing_health_report(stats, fts_ok))
            })
        })
        .await
    }

    /// Perform incremental FTS sync on startup.
    ///
    /// This checks the FTS index state and either:
    /// 1. Does nothing if index is healthy and version matches
    /// 2. Syncs only new segments if index is healthy but has gaps
    /// 3. Performs a full rebuild if index is corrupt or version mismatches
    ///
    /// Returns a result describing what was synced.
    pub async fn sync_fts(&self, config: FtsSyncConfig) -> Result<FtsSyncResult> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.sync_fts_with_cx(&cx, config).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`sync_fts`].
    pub async fn sync_fts_with_cx(
        &self,
        cx: &crate::cx::Cx,
        config: FtsSyncConfig,
    ) -> Result<FtsSyncResult> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("sync_fts cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::SyncFts {
                    config,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Perform a full FTS rebuild regardless of current state.
    ///
    /// This drops the FTS index and reindexes all segments with batched progress.
    /// Use this for recovery or when a clean rebuild is needed.
    pub async fn rebuild_fts(&self, config: FtsSyncConfig) -> Result<FtsSyncResult> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.rebuild_fts_with_cx(&cx, config).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`rebuild_fts`].
    pub async fn rebuild_fts_with_cx(
        &self,
        cx: &crate::cx::Cx,
        config: FtsSyncConfig,
    ) -> Result<FtsSyncResult> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("rebuild_fts cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::RebuildFts {
                    config,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get the current FTS index state (version, last rebuild time).
    pub async fn get_fts_index_state(&self) -> Result<Option<FtsIndexState>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_fts_index_state_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_fts_index_state`].
    pub async fn get_fts_index_state_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Option<FtsIndexState>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_fts_index_state cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), get_fts_index_state_backend)
        })
        .await
    }

    /// Insert an approval token
    pub async fn insert_approval_token(&self, token: ApprovalTokenRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_approval_token_with_cx(&cx, token).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_approval_token`].
    /// Tick 174: inlined to route the mpsc send through `send_with_cx`.
    pub async fn insert_approval_token_with_cx(
        &self,
        cx: &crate::cx::Cx,
        token: ApprovalTokenRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_approval_token cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(cx, WriteCommand::InsertApprovalToken { token, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Consume an approval token if it matches scope and is valid
    #[allow(clippy::too_many_arguments)]
    pub async fn consume_approval_token(
        &self,
        code_hash: &str,
        workspace_id: &str,
        action_kind: &str,
        pane_id: Option<u64>,
        action_fingerprint: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.consume_approval_token_with_cx(
            &cx,
            code_hash,
            workspace_id,
            action_kind,
            pane_id,
            action_fingerprint,
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`consume_approval_token`].
    /// Tick 175: inlined to route the mpsc send through `send_with_cx`.
    pub async fn consume_approval_token_with_cx(
        &self,
        cx: &crate::cx::Cx,
        code_hash: &str,
        workspace_id: &str,
        action_kind: &str,
        pane_id: Option<u64>,
        action_fingerprint: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("consume_approval_token cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::ConsumeApprovalToken {
                    code_hash: code_hash.to_string(),
                    workspace_id: workspace_id.to_string(),
                    action_kind: action_kind.to_string(),
                    pane_id,
                    action_fingerprint: action_fingerprint.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get an approval token by code hash (without consuming)
    pub async fn get_approval_token_by_code(
        &self,
        code_hash: &str,
        workspace_id: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_approval_token_by_code_with_cx(&cx, code_hash, workspace_id)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_approval_token_by_code`].
    pub async fn get_approval_token_by_code_with_cx(
        &self,
        cx: &crate::cx::Cx,
        code_hash: &str,
        workspace_id: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_approval_token_by_code cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let code_hash = code_hash.to_string();
        let workspace_id = workspace_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_approval_token_by_code_backend(backend, &code_hash, &workspace_id)
            })
        })
        .await
    }

    /// Consume an approval token by code hash only (without fingerprint validation).
    ///
    /// **Warning**: This does NOT validate `action_kind`, `pane_id`, or
    /// `action_fingerprint`. A token issued for one action can be consumed
    /// for a different action. Prefer [`consume_approval_token`] when the
    /// full policy context is available.
    pub async fn consume_approval_token_by_code(
        &self,
        code_hash: &str,
        workspace_id: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.consume_approval_token_by_code_with_cx(&cx, code_hash, workspace_id)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`consume_approval_token_by_code`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    pub async fn consume_approval_token_by_code_with_cx(
        &self,
        cx: &crate::cx::Cx,
        code_hash: &str,
        workspace_id: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("consume_approval_token_by_code cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::ConsumeApprovalTokenByCode {
                    code_hash: code_hash.to_string(),
                    workspace_id: workspace_id.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Upsert a pane record
    pub async fn upsert_pane(&self, pane: PaneRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_pane_with_cx(&cx, pane).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_pane`].
    ///
    /// Pre-flight checkpoint gates the pane upsert before the
    /// command is enqueued on the writer channel. Hot path for
    /// pane discovery — the most-called storage mutation (29+
    /// call sites across the codebase), so adding a cx-first
    /// entry point lets observation loops propagate caller
    /// cancellation into the write pipeline.
    ///
    /// Tick 174: inlined to route the mpsc send through
    /// `send_with_cx` — closes the orphan-cx hole in the hottest
    /// storage write in the tree. A backpressured writer queue
    /// under a cancelled observation loop now releases immediately.
    pub async fn upsert_pane_with_cx(&self, cx: &crate::cx::Cx, pane: PaneRecord) -> Result<()> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("upsert_pane cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(cx, WriteCommand::UpsertPane { pane, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Upsert a workflow execution record
    pub async fn upsert_workflow(&self, workflow: WorkflowRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_workflow_with_cx(&cx, workflow).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_workflow`].
    ///
    /// Pre-flight checkpoint gates workflow state writes
    /// (18+ call sites) — second most-called business mutation
    /// after upsert_pane.
    ///
    /// Tick 175: inlined to route the mpsc send through `send_with_cx`.
    pub async fn upsert_workflow_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow: WorkflowRecord,
    ) -> Result<()> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("upsert_workflow cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpsertWorkflow {
                    workflow,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Upsert a workflow action plan (canonical JSON + hash)
    pub async fn upsert_action_plan(
        &self,
        workflow_id: &str,
        plan: &crate::plan::ActionPlan,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_action_plan_with_cx(&cx, workflow_id, plan)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_action_plan`].
    /// Tick 175: inlined to route the mpsc send through `send_with_cx`.
    pub async fn upsert_action_plan_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow_id: &str,
        plan: &crate::plan::ActionPlan,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("upsert_action_plan cancelled: {err}"))
        })?;
        let record = action_plan_record_from_plan(workflow_id, plan)?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpsertActionPlan {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Insert a prepared plan preview for later commit
    pub async fn insert_prepared_plan(&self, record: PreparedPlanRecord) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_prepared_plan_with_cx(&cx, record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_prepared_plan`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    pub async fn insert_prepared_plan_with_cx(
        &self,
        cx: &crate::cx::Cx,
        record: PreparedPlanRecord,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_prepared_plan cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertPreparedPlan {
                    record,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Consume a prepared plan by plan_id (marks as used if valid)
    pub async fn consume_prepared_plan(
        &self,
        plan_id: &str,
        now_ms: i64,
    ) -> Result<Option<PreparedPlanRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.consume_prepared_plan_with_cx(&cx, plan_id, now_ms)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`consume_prepared_plan`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    pub async fn consume_prepared_plan_with_cx(
        &self,
        cx: &crate::cx::Cx,
        plan_id: &str,
        now_ms: i64,
    ) -> Result<Option<PreparedPlanRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("consume_prepared_plan cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::ConsumePreparedPlan {
                    plan_id: plan_id.to_string(),
                    now_ms,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Insert a workflow step log entry
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_step_log(
        &self,
        workflow_id: &str,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: &str,
        step_id: Option<String>,
        step_kind: Option<String>,
        result_type: &str,
        result_data: Option<String>,
        policy_summary: Option<String>,
        verification_refs: Option<String>,
        error_code: Option<String>,
        started_at: i64,
        completed_at: i64,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_step_log_with_cx(
            &cx,
            workflow_id,
            audit_action_id,
            step_index,
            step_name,
            step_id,
            step_kind,
            result_type,
            result_data,
            policy_summary,
            verification_refs,
            error_code,
            started_at,
            completed_at,
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_step_log`].
    /// Tick 174: inlined to route the mpsc send through `send_with_cx`.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_step_log_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow_id: &str,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: &str,
        step_id: Option<String>,
        step_kind: Option<String>,
        result_type: &str,
        result_data: Option<String>,
        policy_summary: Option<String>,
        verification_refs: Option<String>,
        error_code: Option<String>,
        started_at: i64,
        completed_at: i64,
    ) -> Result<()> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("insert_step_log cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertStepLog {
                    workflow_id: workflow_id.to_string(),
                    audit_action_id,
                    step_index,
                    step_name: step_name.to_string(),
                    step_id,
                    step_kind,
                    result_type: result_type.to_string(),
                    result_data,
                    policy_summary,
                    verification_refs,
                    error_code,
                    started_at,
                    completed_at,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    // Upsert an agent session record
    //
    // Creates a new session or updates an existing one.
    // Returns the session ID.
    // =========================================================================
    // Session Checkpoint Methods
    // =========================================================================

    /// Insert a new mux session record.
    pub async fn insert_mux_session(
        &self,
        session_id: String,
        topology_json: String,
        ft_version: String,
        host_id: Option<String>,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_mux_session_with_cx(&cx, session_id, topology_json, ft_version, host_id)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_mux_session`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    pub async fn insert_mux_session_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: String,
        topology_json: String,
        ft_version: String,
        host_id: Option<String>,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_mux_session cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertMuxSession {
                    session_id,
                    topology_json,
                    ft_version,
                    host_id,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Insert a session checkpoint with per-pane state rows.
    /// Returns the checkpoint ID.
    pub async fn insert_session_checkpoint(
        &self,
        session_id: String,
        checkpoint_type: String,
        state_hash: String,
        pane_count: usize,
        total_bytes: usize,
        metadata_json: Option<String>,
        pane_states: Vec<SessionPaneStateRow>,
    ) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.insert_session_checkpoint_with_cx(
            &cx,
            session_id,
            checkpoint_type,
            state_hash,
            pane_count,
            total_bytes,
            metadata_json,
            pane_states,
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`insert_session_checkpoint`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_session_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: String,
        checkpoint_type: String,
        state_hash: String,
        pane_count: usize,
        total_bytes: usize,
        metadata_json: Option<String>,
        pane_states: Vec<SessionPaneStateRow>,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("insert_session_checkpoint cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::InsertSessionCheckpoint {
                    session_id,
                    checkpoint_type,
                    state_hash,
                    pane_count,
                    total_bytes,
                    metadata_json,
                    pane_states,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Prune old checkpoints beyond the retention limit.
    /// Returns the number of pruned checkpoints.
    pub async fn prune_session_checkpoints(
        &self,
        session_id: String,
        retention: usize,
    ) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::PruneSessionCheckpoints {
                session_id,
                retention,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`prune_session_checkpoints`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    pub async fn prune_session_checkpoints_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: String,
        retention: usize,
    ) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("prune_session_checkpoints cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::PruneSessionCheckpoints {
                    session_id,
                    retention,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Mark a session as cleanly shut down.
    pub async fn mark_session_shutdown_clean(&self, session_id: String) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.mark_session_shutdown_clean_with_cx(&cx, session_id)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`mark_session_shutdown_clean`].
    /// Tick 172: inlined to route the mpsc send through `send_with_cx`.
    pub async fn mark_session_shutdown_clean_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: String,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("mark_session_shutdown_clean cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::MarkSessionShutdownClean {
                    session_id,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get the state_hash of the latest checkpoint for a session.
    pub async fn get_latest_checkpoint_hash(&self, session_id: String) -> Result<Option<String>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_latest_checkpoint_hash_with_cx(&cx, session_id)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_latest_checkpoint_hash`].
    pub async fn get_latest_checkpoint_hash_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: String,
    ) -> Result<Option<String>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_latest_checkpoint_hash cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx(cx, move || {
            // br-ft-l1jgo: trait-typed pooled_backend (was direct
            // PooledReadConn::acquire + get_latest_checkpoint_hash).
            pooled_backend(db_path.as_str(), |backend| {
                get_latest_checkpoint_hash_backend(backend, &session_id)
            })
        })
        .await
    }

    pub async fn upsert_agent_session(&self, session: AgentSessionRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_agent_session_with_cx(&cx, session).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_agent_session`].
    pub async fn upsert_agent_session_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session: AgentSessionRecord,
    ) -> Result<i64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("upsert_agent_session cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpsertSession {
                    session,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        Self::recv_writer_response(rx).await
    }

    /// Get an agent session by ID
    pub async fn get_agent_session(&self, session_id: i64) -> Result<Option<AgentSessionRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_agent_session_with_cx(&cx, session_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_agent_session`].
    pub async fn get_agent_session_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session_id: i64,
    ) -> Result<Option<AgentSessionRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_agent_session cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_agent_session_backend(backend, session_id)
            })
        })
        .await
    }

    /// Get active agent sessions (those without an ended_at timestamp)
    pub async fn get_active_sessions(&self) -> Result<Vec<AgentSessionRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_active_sessions_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_active_sessions`].
    pub async fn get_active_sessions_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<AgentSessionRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_active_sessions cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), query_active_sessions_backend)
        })
        .await
    }

    /// Get agent sessions for a specific pane
    pub async fn get_sessions_for_pane(&self, pane_id: u64) -> Result<Vec<AgentSessionRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_sessions_for_pane_with_cx(&cx, pane_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_sessions_for_pane`].
    pub async fn get_sessions_for_pane_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Vec<AgentSessionRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_sessions_for_pane cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx(cx, move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_sessions_for_pane_backend(backend, pane_id)
            })
        })
        .await
    }

    /// Search segments using FTS5
    ///
    /// Returns matching segments ordered by BM25 relevance score.
    pub async fn search(&self, query: &str) -> Result<Vec<Segment>> {
        let results = self
            .search_with_results(query, SearchOptions::default())
            .await?;
        Ok(results.into_iter().map(|r| r.segment).collect())
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`search`].
    ///
    /// Routes through `search_with_results_with_cx` so the inner call
    /// honours cancellation as well.
    pub async fn search_with_cx(&self, cx: &crate::cx::Cx, query: &str) -> Result<Vec<Segment>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("search cancelled: {err}")))?;
        let results = self
            .search_with_results_with_cx(cx, query, SearchOptions::default())
            .await?;
        Ok(results.into_iter().map(|r| r.segment).collect())
    }

    /// Search segments with options (legacy, returns segments only)
    pub async fn search_with_options(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<Segment>> {
        let results = self.search_with_results(query, options).await?;
        Ok(results.into_iter().map(|r| r.segment).collect())
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`search_with_options`].
    ///
    /// Routes through `search_with_results_with_cx` so the inner call
    /// honours cancellation as well.
    pub async fn search_with_options_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<Segment>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("search_with_options cancelled: {err}"))
        })?;
        let results = self.search_with_results_with_cx(cx, query, options).await?;
        Ok(results.into_iter().map(|r| r.segment).collect())
    }

    /// Search segments with full results including snippets, highlights, and scores
    ///
    /// Returns `SearchResult` objects with:
    /// - The matching segment
    /// - A snippet with highlighted matching terms
    /// - Highlighted content (full segment with markers)
    /// - The BM25 relevance score
    ///
    /// # Errors
    ///
    /// Returns `StorageError::FtsQueryError` if the query syntax is invalid.
    /// FTS5 syntax supports:
    /// - Simple words: `hello world` (matches both terms)
    /// - Phrases: `"hello world"` (matches exact phrase)
    /// - Prefix: `hel*` (matches words starting with "hel")
    /// - Boolean: `hello AND world`, `hello OR world`, `NOT hello`
    /// - Column filter: `content:hello` (search specific column)
    pub async fn search_with_results(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.search_with_results_with_cx(&cx, query, options).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`search_with_results`].
    pub async fn search_with_results_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("search_with_results cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let query = query.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                search_fts_with_snippets_backend(backend, &query, &options)
            })
        })
        .await
    }

    // =========================================================================
    // Embedding storage (semantic search)
    // =========================================================================

    /// Store an embedding vector for a segment.
    pub async fn store_embedding(
        &self,
        segment_id: i64,
        embedder_id: &str,
        dimension: i32,
        vector: &[u8],
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.store_embedding_with_cx(&cx, segment_id, embedder_id, dimension, vector)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`store_embedding`].
    pub async fn store_embedding_with_cx(
        &self,
        cx: &crate::cx::Cx,
        segment_id: i64,
        embedder_id: &str,
        dimension: i32,
        vector: &[u8],
    ) -> Result<()> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("store_embedding cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let embedder_id = embedder_id.to_string();
        let vector = vector.to_vec();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
            pooled_backend(db_path.as_str(), |backend| {
                execute_typed(
                    backend,
                    "INSERT OR REPLACE INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at)
                     VALUES (?1, ?2, ?3, ?4, strftime('%s', 'now'))",
                    &[
                        ToSqlValue::Integer(segment_id),
                        ToSqlValue::Text(&embedder_id),
                        ToSqlValue::Integer(i64::from(dimension)),
                        ToSqlValue::Blob(&vector),
                    ],
                )
                .map_err(|err| storage_backend_error("store_embedding", err))?;
                Ok(())
            })
        })
        .await?;

        self.invalidate_semantic_cache();
        Ok(())
    }

    /// Get segment IDs that have no embedding for the given embedder.
    pub async fn get_unembedded_segments(
        &self,
        embedder_id: &str,
        limit: usize,
    ) -> Result<Vec<i64>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_unembedded_segments_with_cx(&cx, embedder_id, limit)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_unembedded_segments`].
    pub async fn get_unembedded_segments_with_cx(
        &self,
        cx: &crate::cx::Cx,
        embedder_id: &str,
        limit: usize,
    ) -> Result<Vec<i64>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_unembedded_segments cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let embedder_id = embedder_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
            pooled_backend(db_path.as_str(), |backend| {
                let rows = backend
                    .query_map_typed(
                        "SELECT s.id FROM output_segments s
                         LEFT JOIN segment_embeddings se ON s.id = se.segment_id AND se.embedder_id = ?1
                         WHERE se.segment_id IS NULL
                         ORDER BY s.id ASC
                         LIMIT ?2",
                        &[
                            ToSqlValue::Text(&embedder_id),
                            ToSqlValue::Integer(limit as i64),
                        ],
                    )
                    .map_err(|err| storage_backend_error("get_unembedded_segments", err))?;

                let ids = rows
                    .iter()
                    .map(|row| RowReader::new(row).i64(0))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|err| storage_backend_error("get_unembedded_segments row", err))?;

                Ok(ids)
            })
        })
        .await
    }

    /// Get the embedding for a specific segment.
    pub async fn get_embedding(
        &self,
        segment_id: i64,
        embedder_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_embedding_with_cx(&cx, segment_id, embedder_id)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_embedding`].
    pub async fn get_embedding_with_cx(
        &self,
        cx: &crate::cx::Cx,
        segment_id: i64,
        embedder_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_embedding cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let embedder_id = embedder_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
            pooled_backend(db_path.as_str(), |backend| {
                let result = backend
                    .query_row_cells(
                        "SELECT vector FROM segment_embeddings WHERE segment_id = ?1 AND embedder_id = ?2",
                        &[ToSqlValue::Integer(segment_id), ToSqlValue::Text(&embedder_id)],
                    )
                    .map_err(|err| storage_backend_error("get_embedding", err))?
                    .map(|row| match row.first() {
                        Some(SqlCell::Blob(bytes)) => Ok(bytes.clone()),
                        Some(other) => Err(StorageError::Database(format!(
                            "get_embedding: expected blob cell, got {other:?}"
                        ))),
                        None => Err(StorageError::Database(
                            "get_embedding: query returned row with no columns".to_string(),
                        )),
                    })
                    .transpose()?;

                Ok(result)
            })
        })
        .await
    }

    /// Get embedding statistics per embedder.
    pub async fn embedding_stats(&self) -> Result<Vec<EmbeddingStats>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.embedding_stats_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`embedding_stats`].
    pub async fn embedding_stats_with_cx(&self, cx: &crate::cx::Cx) -> Result<Vec<EmbeddingStats>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("embedding_stats cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
            pooled_backend(db_path.as_str(), |backend| {
                let rows = backend
                    .query_map_typed(
                        "SELECT embedder_id, dimension, COUNT(*) as count,
                                MIN(embedded_at) as earliest, MAX(embedded_at) as latest
                         FROM segment_embeddings
                         GROUP BY embedder_id, dimension",
                        &[],
                    )
                    .map_err(|err| storage_backend_error("embedding_stats", err))?;

                let stats = rows
                    .iter()
                    .map(|row| {
                        let reader = RowReader::new(row);
                        let raw_dim = reader.i64(1)?;
                        // br-ft-ir23j: i32::try_from instead of `as i32`.
                        // SQLite stores dimensions as INTEGER (i64); a row
                        // with dim > i32::MAX would silently wrap under
                        // `as i32`. Realistic schema values fit, but the
                        // pre-refactor `row.get::<_, i32>(1)?` returned
                        // a typed conversion error on overflow — preserve
                        // that defensive contract.
                        let dimension = i32::try_from(raw_dim).map_err(|_| {
                            BackendError::Query(format!(
                                "embedding_stats: dimension column out of i32 range: {raw_dim}"
                            ))
                        })?;
                        Ok(EmbeddingStats {
                            embedder_id: reader.string(0)?,
                            dimension,
                            count: reader.i64(2)?,
                            earliest_at: reader.i64(3)?,
                            latest_at: reader.i64(4)?,
                        })
                    })
                    .collect::<std::result::Result<Vec<_>, BackendError>>()
                    .map_err(|err| storage_backend_error("embedding_stats row", err))?;

                Ok(stats)
            })
        })
        .await
    }

    /// Store an f32 embedding vector (little-endian packed) for a segment.
    pub async fn store_embedding_f32(
        &self,
        segment_id: i64,
        embedder_id: &str,
        vector: &[f32],
    ) -> Result<()> {
        if vector.is_empty() {
            return Err(
                StorageError::Database("store_embedding_f32: vector is empty".to_string()).into(),
            );
        }
        let bytes = encode_f32_embedding_blob(vector)?;
        let dimension = i32::try_from(vector.len()).map_err(|_| {
            StorageError::Database("store_embedding_f32: vector dimension exceeds i32".to_string())
        })?;
        self.store_embedding(segment_id, embedder_id, dimension, &bytes)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`store_embedding_f32`].
    ///
    /// Composite: packs f32 → bytes, then routes through
    /// `store_embedding_with_cx` so the inner write honours cancellation.
    pub async fn store_embedding_f32_with_cx(
        &self,
        cx: &crate::cx::Cx,
        segment_id: i64,
        embedder_id: &str,
        vector: &[f32],
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("store_embedding_f32 cancelled: {err}"))
        })?;
        if vector.is_empty() {
            return Err(
                StorageError::Database("store_embedding_f32: vector is empty".to_string()).into(),
            );
        }
        let bytes = encode_f32_embedding_blob(vector)?;
        let dimension = i32::try_from(vector.len()).map_err(|_| {
            StorageError::Database("store_embedding_f32: vector dimension exceeds i32".to_string())
        })?;
        self.store_embedding_with_cx(cx, segment_id, embedder_id, dimension, &bytes)
            .await
    }

    /// Semantic retrieval over stored embeddings for a single embedder.
    ///
    /// Returns segment ids ranked by cosine similarity against `query_vector`.
    pub async fn semantic_search(
        &self,
        embedder_id: &str,
        query_vector: &[f32],
        options: SearchOptions,
    ) -> Result<Vec<SemanticSearchHit>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.semantic_search_with_cx(&cx, embedder_id, query_vector, options)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`semantic_search`].
    pub async fn semantic_search_with_cx(
        &self,
        cx: &crate::cx::Cx,
        embedder_id: &str,
        query_vector: &[f32],
        options: SearchOptions,
    ) -> Result<Vec<SemanticSearchHit>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("semantic_search cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let embedder_id = embedder_id.to_string();
        let query_vector = query_vector.to_vec();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l1jgo: route semantic read through the storage trait path.
            pooled_backend(db_path.as_str(), |backend| {
                search_semantic_backend(backend, &embedder_id, &query_vector, &options)
            })
        })
        .await
    }

    /// Hybrid lexical+semantic retrieval using deterministic fusion.
    ///
    /// This is a storage-level bridge between FTS lexical results and the
    /// semantic retrieval lane, suitable for CLI/robot/MCP integration.
    pub async fn hybrid_search_with_results(
        &self,
        query: &str,
        options: SearchOptions,
        embedder_id: &str,
        query_vector: &[f32],
        mode: SearchMode,
        rrf_k: u32,
        lexical_weight: f32,
        semantic_weight: f32,
        fusion_backend: Option<FusionBackend>,
    ) -> Result<HybridSearchBundle> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.hybrid_search_with_results_with_cx(
            &cx,
            query,
            options,
            embedder_id,
            query_vector,
            mode,
            rrf_k,
            lexical_weight,
            semantic_weight,
            fusion_backend,
        )
        .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`hybrid_search_with_results`].
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search_with_results_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: &str,
        options: SearchOptions,
        embedder_id: &str,
        query_vector: &[f32],
        mode: SearchMode,
        rrf_k: u32,
        lexical_weight: f32,
        semantic_weight: f32,
        fusion_backend: Option<FusionBackend>,
    ) -> Result<HybridSearchBundle> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("hybrid_search_with_results cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let semantic_budget_state = Arc::clone(&self.semantic_budget_state);
        let query = query.to_string();
        let embedder_id = embedder_id.to_string();
        let query_vector = query_vector.to_vec();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                hybrid_search_with_results_backend(
                    backend,
                    &query,
                    &options,
                    &embedder_id,
                    &query_vector,
                    mode,
                    rrf_k,
                    lexical_weight,
                    semantic_weight,
                    fusion_backend,
                    &semantic_budget_state,
                )
            })
        })
        .await
    }

    /// Get unhandled events
    pub async fn get_unhandled_events(&self, limit: usize) -> Result<Vec<StoredEvent>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_unhandled_events_with_cx(&cx, limit).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_unhandled_events`].
    pub async fn get_unhandled_events_with_cx(
        &self,
        cx: &crate::cx::Cx,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_unhandled_events cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_unhandled_events_backend(backend, limit)
            })
        })
        .await
    }

    /// Query events with filters
    pub async fn get_events(&self, query: EventQuery) -> Result<Vec<StoredEvent>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_events_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_events`].
    pub async fn get_events_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: EventQuery,
    ) -> Result<Vec<StoredEvent>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_events cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_events_backend(backend, &query)
            })
        })
        .await
    }

    /// Query events using an ID cursor for deterministic replay/resume.
    ///
    /// Results are ordered by ascending event ID so callers can checkpoint using
    /// the last seen `id` and resume with `after_id`.
    pub async fn get_events_stream(&self, query: EventStreamQuery) -> Result<Vec<StoredEvent>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_events_stream_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_events_stream`].
    pub async fn get_events_stream_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: EventStreamQuery,
    ) -> Result<Vec<StoredEvent>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_events_stream cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_events_stream_backend(backend, &query)
            })
        })
        .await
    }

    /// Get a unified timeline of events across panes.
    ///
    /// Returns events enriched with pane info and correlations,
    /// sorted chronologically with pagination support.
    pub async fn get_timeline(&self, query: TimelineQuery) -> Result<Timeline> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_timeline_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_timeline`].
    pub async fn get_timeline_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: TimelineQuery,
    ) -> Result<Timeline> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_timeline cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_timeline_backend(backend, &query)
            })
        })
        .await
    }

    /// Count unhandled events grouped by pane ID
    ///
    /// Returns a map from pane_id to the count of unhandled events for that pane.
    pub async fn count_unhandled_events_by_pane(
        &self,
    ) -> Result<std::collections::HashMap<u64, u32>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_unhandled_events_by_pane_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_unhandled_events_by_pane`].
    pub async fn count_unhandled_events_by_pane_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<std::collections::HashMap<u64, u32>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_unhandled_events_by_pane cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_unhandled_event_counts(backend)
            })
        })
        .await
    }

    /// Get the most recent activity timestamp for each pane
    ///
    /// Returns a map from pane_id to the most recent segment captured_at timestamp.
    pub async fn get_last_activity_by_pane(&self) -> Result<std::collections::HashMap<u64, i64>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_last_activity_by_pane_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_last_activity_by_pane`].
    pub async fn get_last_activity_by_pane_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<std::collections::HashMap<u64, i64>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_last_activity_by_pane cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_last_activity_by_pane_backend(backend)
            })
        })
        .await
    }

    /// Query audit actions with filters
    pub async fn get_audit_actions(&self, query: AuditQuery) -> Result<Vec<AuditActionRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_audit_actions_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_audit_actions`].
    ///
    /// Pre-flight checkpoint gates the audit query before
    /// spawn_blocking. 14+ call sites on the operator-facing
    /// audit-history read path.
    pub async fn get_audit_actions_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: AuditQuery,
    ) -> Result<Vec<AuditActionRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_audit_actions cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_audit_actions_backend(backend, &query)
            })
        })
        .await
    }

    /// Stream audit actions using a cursor and stable ordering.
    ///
    /// Records are ordered by monotonically increasing ID for deterministic paging.
    pub async fn get_audit_actions_stream(
        &self,
        query: AuditStreamQuery,
    ) -> Result<AuditStreamPage> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_audit_actions_stream_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_audit_actions_stream`].
    pub async fn get_audit_actions_stream_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: AuditStreamQuery,
    ) -> Result<AuditStreamPage> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_audit_actions_stream cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_audit_actions_stream_backend(backend, &query)
            })
        })
        .await
    }

    /// Query action history view with filters
    pub async fn get_action_history(
        &self,
        query: ActionHistoryQuery,
    ) -> Result<Vec<ActionHistoryRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_action_history_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_action_history`].
    pub async fn get_action_history_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: ActionHistoryQuery,
    ) -> Result<Vec<ActionHistoryRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_action_history cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_action_history_backend(backend, &query)
            })
        })
        .await
    }

    /// Count active (unused + unexpired) approval tokens for a workspace
    pub async fn count_active_approvals(&self, workspace_id: &str, now_ms: i64) -> Result<u32> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.count_active_approvals_with_cx(&cx, workspace_id, now_ms)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`count_active_approvals`].
    pub async fn count_active_approvals_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workspace_id: &str,
        now_ms: i64,
    ) -> Result<u32> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("count_active_approvals cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let workspace_id = workspace_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_active_approvals_count_backend(backend, &workspace_id, now_ms)
            })
        })
        .await
    }

    /// Look up an approval token by code hash (without consuming it)
    ///
    /// Returns the token record if found, regardless of whether it's expired or consumed.
    /// Use this for validation and dry-run checks.
    pub async fn get_approval_token(&self, code_hash: &str) -> Result<Option<ApprovalTokenRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_approval_token_with_cx(&cx, code_hash).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_approval_token`].
    pub async fn get_approval_token_with_cx(
        &self,
        cx: &crate::cx::Cx,
        code_hash: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_approval_token cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let code_hash = code_hash.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_approval_token_by_hash_backend(backend, &code_hash)
            })
        })
        .await
    }

    /// Get the maximum sequence number for a pane (to resume capture).
    pub async fn get_max_seq(&self, pane_id: u64) -> Result<Option<u64>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_max_seq_with_cx(&cx, pane_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_max_seq`].
    pub async fn get_max_seq_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<u64>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_max_seq cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l1jgo: trait-typed pooled_backend (was direct
            // PooledReadConn::acquire + query_max_seq).
            pooled_backend(db_path.as_str(), |backend| {
                query_max_seq_backend(backend, pane_id)
            })
        })
        .await
    }

    /// Get all panes
    pub async fn get_panes(&self) -> Result<Vec<PaneRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_panes_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_panes`].
    pub async fn get_panes_with_cx(&self, cx: &crate::cx::Cx) -> Result<Vec<PaneRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_panes cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l1jgo: trait-typed pooled_backend pane-list
            // read path (was direct PooledReadConn::acquire + query_panes).
            pooled_backend(db_path.as_str(), query_panes_backend)
        })
        .await
    }

    /// Get a specific pane
    pub async fn get_pane(&self, pane_id: u64) -> Result<Option<PaneRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_pane_with_cx(&cx, pane_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_pane`].
    pub async fn get_pane_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<PaneRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_pane cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l1jgo: trait-typed pooled_backend pane lookup
            // (was direct PooledReadConn::acquire + query_pane).
            pooled_backend(db_path.as_str(), |backend| {
                query_pane_backend(backend, pane_id)
            })
        })
        .await
    }

    /// Get a specific pane using a synchronous read path.
    ///
    /// This is intended for prepare-phase policy evaluation, where the caller
    /// is already on a synchronous execution path.
    pub fn get_pane_blocking(&self, pane_id: u64) -> Result<Option<PaneRecord>> {
        // br-ft-l1jgo: keep the blocking path on the same trait-typed
        // pooled backend as the async sibling.
        pooled_backend(self.db_path.as_str(), |backend| {
            query_pane_backend(backend, pane_id)
        })
    }

    /// Get recent segments for a pane
    pub async fn get_segments(&self, pane_id: u64, limit: usize) -> Result<Vec<Segment>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_segments_with_cx(&cx, pane_id, limit).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_segments`].
    pub async fn get_segments_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        limit: usize,
    ) -> Result<Vec<Segment>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_segments cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let mmap_mirror_dir = self
            .mmap_mirror_dir
            .as_ref()
            .map(|dir| dir.as_ref().clone());

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            if let Some(mmap_dir) = mmap_mirror_dir.as_ref() {
                match query_segments_from_mmap(mmap_dir, pane_id, limit) {
                    Ok(Some(segments)) => return Ok(segments),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            pane_id,
                            limit,
                            path = %mmap_dir.display(),
                            error = %error,
                            "mmap segment read failed; falling back to sqlite"
                        );
                    }
                }
            }

            pooled_backend(db_path.as_str(), |backend| {
                query_segments_backend(backend, pane_id, limit)
            })
        })
        .await
    }

    /// Scan segments in ascending id order with incremental paging.
    pub async fn scan_segments(&self, query: SegmentScanQuery) -> Result<Vec<Segment>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.scan_segments_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`scan_segments`].
    pub async fn scan_segments_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: SegmentScanQuery,
    ) -> Result<Vec<Segment>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("scan_segments cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l1jgo: trait-typed pooled_backend scan path (was direct
            // PooledReadConn::acquire + query_scan_segments).
            pooled_backend(db_path.as_str(), |backend| {
                query_scan_segments_backend(backend, &query)
            })
        })
        .await
    }

    /// Fetch the most recent secret scan report for a scope hash.
    pub async fn latest_secret_scan_report(
        &self,
        scope_hash: &str,
    ) -> Result<Option<SecretScanReportRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.latest_secret_scan_report_with_cx(&cx, scope_hash)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`latest_secret_scan_report`].
    pub async fn latest_secret_scan_report_with_cx(
        &self,
        cx: &crate::cx::Cx,
        scope_hash: &str,
    ) -> Result<Option<SecretScanReportRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("latest_secret_scan_report cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let scope_hash = scope_hash.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l1jgo: route the secret-scan checkpoint read through
            // the trait-typed pooled backend instead of a direct read conn.
            pooled_backend(db_path.as_str(), |backend| {
                query_latest_secret_scan_report_backend(backend, &scope_hash)
            })
        })
        .await
    }

    /// Get workflow by ID
    pub async fn get_workflow(&self, workflow_id: &str) -> Result<Option<WorkflowRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_workflow_with_cx(&cx, workflow_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_workflow`].
    pub async fn get_workflow_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow_id: &str,
    ) -> Result<Option<WorkflowRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_workflow cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_workflow_backend(backend, &workflow_id)
            })
        })
        .await
    }

    /// Get step logs for a workflow
    ///
    /// Returns all step logs for the given workflow, ordered by step index.
    pub async fn get_step_logs(&self, workflow_id: &str) -> Result<Vec<WorkflowStepLogRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_step_logs_with_cx(&cx, workflow_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_step_logs`].
    ///
    /// Pre-flight checkpoint gates the step-log query before
    /// spawn_blocking. 16+ call sites on the workflow
    /// observability read path (diagnostic bundles, CLI step
    /// history, etc.).
    pub async fn get_step_logs_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow_id: &str,
    ) -> Result<Vec<WorkflowStepLogRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_step_logs cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_step_logs_backend(backend, &workflow_id)
            })
        })
        .await
    }

    /// Get the latest step log for a workflow (highest step_index).
    pub async fn get_latest_step_log(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowStepLogRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_latest_step_log_with_cx(&cx, workflow_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_latest_step_log`].
    pub async fn get_latest_step_log_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow_id: &str,
    ) -> Result<Option<WorkflowStepLogRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_latest_step_log cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_latest_step_log_backend(backend, &workflow_id)
            })
        })
        .await
    }

    /// Get the persisted action plan for a workflow execution, if available
    pub async fn get_action_plan(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowActionPlanRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_action_plan_with_cx(&cx, workflow_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_action_plan`].
    pub async fn get_action_plan_with_cx(
        &self,
        cx: &crate::cx::Cx,
        workflow_id: &str,
    ) -> Result<Option<WorkflowActionPlanRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_action_plan cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_action_plan_backend(backend, &workflow_id)
            })
        })
        .await
    }

    /// Get a prepared plan preview by plan_id
    pub async fn get_prepared_plan(&self, plan_id: &str) -> Result<Option<PreparedPlanRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_prepared_plan_with_cx(&cx, plan_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_prepared_plan`].
    pub async fn get_prepared_plan_with_cx(
        &self,
        cx: &crate::cx::Cx,
        plan_id: &str,
    ) -> Result<Option<PreparedPlanRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_prepared_plan cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        let plan_id = plan_id.to_string();

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_prepared_plan_backend(backend, &plan_id)
            })
        })
        .await
    }

    /// Find incomplete workflows for resume on restart
    ///
    /// Returns all workflows with status 'running' or 'waiting', ordered by started_at.
    /// These are workflows that were interrupted and should be resumed.
    pub async fn find_incomplete_workflows(&self) -> Result<Vec<WorkflowRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.find_incomplete_workflows_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`find_incomplete_workflows`].
    pub async fn find_incomplete_workflows_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<WorkflowRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("find_incomplete_workflows cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);

        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                query_incomplete_workflows_backend(backend)
            })
        })
        .await
    }

    /// Check if the storage is writable (writer thread is alive and responsive).
    ///
    /// This is a lightweight health check that sends a ping to the writer thread.
    pub async fn is_writable(&self) -> bool {
        // A simple check: if the channel is not closed, writer should be alive
        // We can't easily send a ping without adding a new WriteCommand variant,
        // so we check if the channel has capacity (indicates writer is processing)
        !self.write_tx.is_closed()
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`is_writable`].
    ///
    /// Health probe routed through a cx seam so a cancelled context
    /// surfaces before the channel state is inspected. Note the underlying
    /// check is infallible, so we return `Ok(bool)` and fold cancellation
    /// into the error path.
    pub async fn is_writable_with_cx(&self, cx: &crate::cx::Cx) -> Result<bool> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("is_writable cancelled: {err}")))?;
        Ok(self.is_writable().await)
    }

    // =========================================================================
    // Account Operations
    // =========================================================================

    /// Upsert an account record (insert or update by service+account_id)
    ///
    /// Returns the row ID of the upserted account.
    pub async fn upsert_account(&self, account: crate::accounts::AccountRecord) -> Result<i64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.upsert_account_with_cx(&cx, account).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`upsert_account`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn upsert_account_with_cx(
        &self,
        cx: &crate::cx::Cx,
        account: crate::accounts::AccountRecord,
    ) -> Result<i64> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("upsert_account cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpsertAccount {
                    account,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Update an account's last_used_at timestamp
    ///
    /// Call this when an account is selected for use to maintain LRU ordering.
    pub async fn update_account_last_used(
        &self,
        service: &str,
        account_id: &str,
        last_used_at: i64,
    ) -> Result<()> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.update_account_last_used_with_cx(&cx, service, account_id, last_used_at)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`update_account_last_used`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn update_account_last_used_with_cx(
        &self,
        cx: &crate::cx::Cx,
        service: &str,
        account_id: &str,
        last_used_at: i64,
    ) -> Result<()> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("update_account_last_used cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::UpdateAccountLastUsed {
                    service: service.to_string(),
                    account_id: account_id.to_string(),
                    last_used_at,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Delete an account by service and account_id
    ///
    /// Returns true if an account was deleted, false if not found.
    pub async fn delete_account(&self, service: &str, account_id: &str) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.delete_account_with_cx(&cx, service, account_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`delete_account`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn delete_account_with_cx(
        &self,
        cx: &crate::cx::Cx,
        service: &str,
        account_id: &str,
    ) -> Result<bool> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("delete_account cancelled: {err}")))?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::DeleteAccount {
                    service: service.to_string(),
                    account_id: account_id.to_string(),
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get all accounts for a service
    ///
    /// Returns accounts sorted by percent_remaining DESC, last_used_at ASC.
    pub async fn get_accounts_by_service(
        &self,
        service: &str,
    ) -> Result<Vec<crate::accounts::AccountRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_accounts_by_service_with_cx(&cx, service).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_accounts_by_service`].
    pub async fn get_accounts_by_service_with_cx(
        &self,
        cx: &crate::cx::Cx,
        service: &str,
    ) -> Result<Vec<crate::accounts::AccountRecord>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_accounts_by_service cancelled: {err}"))
        })?;
        let db_path = self.db_path.clone();
        let service = service.to_string();
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l4gdl: trait-typed pool helper.
            pooled_backend(db_path.as_str(), |backend| {
                get_accounts_by_service_backend(backend, &service)
            })
        })
        .await
    }

    /// Get a single account by service and account_id
    pub async fn get_account(
        &self,
        service: &str,
        account_id: &str,
    ) -> Result<Option<crate::accounts::AccountRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_account_with_cx(&cx, service, account_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_account`].
    pub async fn get_account_with_cx(
        &self,
        cx: &crate::cx::Cx,
        service: &str,
        account_id: &str,
    ) -> Result<Option<crate::accounts::AccountRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_account cancelled: {err}")))?;
        let db_path = self.db_path.clone();
        let service = service.to_string();
        let account_id = account_id.to_string();
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-l4gdl: trait-typed pool helper.
            pooled_backend(db_path.as_str(), |backend| {
                get_account_backend(backend, &service, &account_id)
            })
        })
        .await
    }

    /// Select the best account for a service according to selection policy
    ///
    /// This combines fetching accounts with the selection algorithm from the
    /// accounts module.
    pub async fn select_account(
        &self,
        service: &str,
        config: &crate::accounts::AccountSelectionConfig,
    ) -> Result<crate::accounts::AccountSelectionResult> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.select_account_with_cx(&cx, service, config).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`select_account`].
    ///
    /// Composite that routes through `get_accounts_by_service_with_cx` so
    /// the full selection flow honours cancellation.
    pub async fn select_account_with_cx(
        &self,
        cx: &crate::cx::Cx,
        service: &str,
        config: &crate::accounts::AccountSelectionConfig,
    ) -> Result<crate::accounts::AccountSelectionResult> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("select_account cancelled: {err}")))?;
        let accounts = self.get_accounts_by_service_with_cx(cx, service).await?;
        Ok(crate::accounts::select_account(&accounts, config))
    }

    // =========================================================================
    // Pane Reservation Operations
    // =========================================================================

    /// Create an exclusive pane reservation.
    ///
    /// Returns a conflict error if the pane already has an active reservation.
    /// TTL is clamped to `[1s, max_ttl]` via `PaneReservationConfig`.
    pub async fn create_reservation(
        &self,
        pane_id: u64,
        owner_kind: &str,
        owner_id: &str,
        reason: Option<&str>,
        ttl_ms: i64,
    ) -> Result<PaneReservation> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.create_reservation_with_cx(&cx, pane_id, owner_kind, owner_id, reason, ttl_ms)
            .await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`create_reservation`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn create_reservation_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        owner_kind: &str,
        owner_id: &str,
        reason: Option<&str>,
        ttl_ms: i64,
    ) -> Result<PaneReservation> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("create_reservation cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::CreateReservation {
                    pane_id,
                    owner_kind: owner_kind.to_string(),
                    owner_id: owner_id.to_string(),
                    reason: reason.map(String::from),
                    ttl_ms,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Release a pane reservation by ID.
    ///
    /// Returns true if released, false if not found or already released.
    pub async fn release_reservation(&self, reservation_id: i64) -> Result<bool> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.release_reservation_with_cx(&cx, reservation_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`release_reservation`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn release_reservation_with_cx(
        &self,
        cx: &crate::cx::Cx,
        reservation_id: i64,
    ) -> Result<bool> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("release_reservation cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(
                cx,
                WriteCommand::ReleaseReservation {
                    reservation_id,
                    respond: tx,
                },
            )
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Get the active reservation for a pane (read-only).
    pub async fn get_active_reservation(&self, pane_id: u64) -> Result<Option<PaneReservation>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_active_reservation_with_cx(&cx, pane_id).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_active_reservation`].
    pub async fn get_active_reservation_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<Option<PaneReservation>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_active_reservation cancelled: {err}"))
        })?;
        let db_path = self.db_path.clone();
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-v5wjv: trait-typed pool helper (pooled_backend)
            // routes the closure through `&dyn StorageBackend`, so
            // a future frankensqlite swap is one feature-flag flip.
            pooled_backend(db_path.as_str(), |backend| {
                get_active_reservation_backend(backend, pane_id)
            })
        })
        .await
    }

    /// Get the active reservation for a pane using a synchronous read path.
    pub fn get_active_reservation_blocking(&self, pane_id: u64) -> Result<Option<PaneReservation>> {
        // br-ft-v5wjv: trait-typed pool helper.
        pooled_backend(self.db_path.as_str(), |backend| {
            get_active_reservation_backend(backend, pane_id)
        })
    }

    /// List all active (unexpired) pane reservations (read-only).
    pub async fn list_active_reservations(&self) -> Result<Vec<PaneReservation>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.list_active_reservations_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`list_active_reservations`].
    pub async fn list_active_reservations_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<Vec<PaneReservation>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("list_active_reservations cancelled: {err}"))
        })?;
        let db_path = self.db_path.clone();
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            // br-ft-v5wjv: trait-typed pool helper.
            pooled_backend(db_path.as_str(), list_active_reservations_backend)
        })
        .await
    }

    /// Check whether there is an active approval token for the exact scope on a
    /// synchronous read path.
    pub fn has_active_approval_for_scope_blocking(
        &self,
        workspace_id: &str,
        action_kind: &str,
        pane_id: Option<u64>,
        action_fingerprint: &str,
        now_ms: i64,
    ) -> Result<bool> {
        pooled_backend(self.db_path.as_str(), |backend| {
            query_active_approval_for_scope_backend(
                backend,
                workspace_id,
                action_kind,
                pane_id,
                action_fingerprint,
                now_ms,
            )
        })
    }

    // =========================================================================
    // Export Query Operations
    // =========================================================================

    /// Export segments with optional pane/time/limit filters
    pub async fn export_segments(&self, query: ExportQuery) -> Result<Vec<Segment>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.export_segments_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`export_segments`].
    pub async fn export_segments_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: ExportQuery,
    ) -> Result<Vec<Segment>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("export_segments cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                export::query_export_segments(backend, &query)
            })
        })
        .await
    }

    /// Export output gaps with optional pane/time/limit filters
    pub async fn export_gaps(&self, query: ExportQuery) -> Result<Vec<Gap>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.export_gaps_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`export_gaps`].
    pub async fn export_gaps_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: ExportQuery,
    ) -> Result<Vec<Gap>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("export_gaps cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                export::query_export_gaps(backend, &query)
            })
        })
        .await
    }

    /// Get all output gaps (for search explain diagnostics)
    pub async fn get_gaps(&self) -> Result<Vec<Gap>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_gaps_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_gaps`].
    pub async fn get_gaps_with_cx(&self, cx: &crate::cx::Cx) -> Result<Vec<Gap>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("get_gaps cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Task join error",
            move || -> Result<Vec<Gap>> {
                // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
                pooled_backend(db_path.as_str(), |backend| {
                    let rows = backend
                        .query_map_typed(
                            "SELECT id, pane_id, seq_before, seq_after, reason, detected_at \
                             FROM output_gaps ORDER BY detected_at DESC",
                            &[],
                        )
                        .map_err(|err| storage_backend_error("Query gaps", err))?;
                    rows.iter()
                        .map(|row| {
                            let reader = RowReader::new(row);
                            Ok(Gap {
                                id: reader.i64(0)?,
                                pane_id: u64::try_from(reader.i64(1)?).map_err(|_| {
                                    BackendError::Query(
                                        "output_gaps.pane_id out of range".to_string(),
                                    )
                                })?,
                                seq_before: u64::try_from(reader.i64(2)?).map_err(|_| {
                                    BackendError::Query(
                                        "output_gaps.seq_before out of range".to_string(),
                                    )
                                })?,
                                seq_after: u64::try_from(reader.i64(3)?).map_err(|_| {
                                    BackendError::Query(
                                        "output_gaps.seq_after out of range".to_string(),
                                    )
                                })?,
                                reason: reader.string(4)?,
                                detected_at: reader.i64(5)?,
                            })
                        })
                        .collect::<std::result::Result<Vec<_>, BackendError>>()
                        .map_err(|err| storage_backend_error("Collect gaps", err).into())
                })
            },
        )
        .await
    }

    /// Count retention cleanup events (for search explain diagnostics)
    pub async fn get_retention_cleanup_count(&self) -> Result<u64> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_retention_cleanup_count_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_retention_cleanup_count`].
    pub async fn get_retention_cleanup_count_with_cx(&self, cx: &crate::cx::Cx) -> Result<u64> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_retention_cleanup_count cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || -> Result<u64> {
            // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
            pooled_backend(db_path.as_str(), |backend| {
                let row = backend
                    .query_row_typed(
                        "SELECT COUNT(*) FROM maintenance_log WHERE event_type = 'retention_cleanup'",
                        &[],
                    )
                    .map_err(|err| storage_backend_error("Count retention cleanups", err))?
                    .ok_or_else(|| {
                        StorageError::Database(
                            "Count retention cleanups returned no row".to_string(),
                        )
                    })?;
                let count = RowReader::new(&row)
                    .i64(0)
                    .map_err(|err| storage_backend_error("Count retention cleanups row", err))?;
                u64::try_from(count).map_err(|_| {
                    StorageError::Database(format!(
                        "Count retention cleanups out of range: {count}"
                    ))
                    .into()
                })
            })
        })
        .await
    }

    /// Get the min/max captured_at timestamps across all segments (for search explain diagnostics)
    pub async fn get_segment_time_range(&self) -> Result<(Option<i64>, Option<i64>)> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.get_segment_time_range_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`get_segment_time_range`].
    pub async fn get_segment_time_range_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Result<(Option<i64>, Option<i64>)> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("get_segment_time_range cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(
            cx,
            "Task join error",
            move || -> Result<(Option<i64>, Option<i64>)> {
                // br-ft-3twzm: pooled backend re-fixes ft-bhyxz.
                pooled_backend(db_path.as_str(), |backend| {
                    let row = backend
                        .query_row_typed(
                            "SELECT MIN(captured_at), MAX(captured_at) FROM output_segments",
                            &[],
                        )
                        .map_err(|err| storage_backend_error("Query segment time range", err))?
                        .ok_or_else(|| {
                            StorageError::Database(
                                "Query segment time range returned no row".to_string(),
                            )
                        })?;
                    let reader = RowReader::new(&row);
                    let earliest = reader.optional_i64(0).map_err(|err| {
                        storage_backend_error("Query segment time range row", err)
                    })?;
                    let latest = reader.optional_i64(1).map_err(|err| {
                        storage_backend_error("Query segment time range row", err)
                    })?;
                    Ok((earliest, latest))
                })
            },
        )
        .await
    }

    /// Export workflow executions with optional pane/time/limit filters
    pub async fn export_workflows(&self, query: ExportQuery) -> Result<Vec<WorkflowRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.export_workflows_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`export_workflows`].
    pub async fn export_workflows_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: ExportQuery,
    ) -> Result<Vec<WorkflowRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("export_workflows cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                export::query_export_workflows(backend, &query)
            })
        })
        .await
    }

    /// Export agent sessions with optional pane/time/limit filters
    pub async fn export_sessions(&self, query: ExportQuery) -> Result<Vec<AgentSessionRecord>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.export_sessions_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`export_sessions`].
    pub async fn export_sessions_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: ExportQuery,
    ) -> Result<Vec<AgentSessionRecord>> {
        cx.checkpoint()
            .map_err(|err| StorageError::Database(format!("export_sessions cancelled: {err}")))?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                export::query_export_sessions(backend, &query)
            })
        })
        .await
    }

    /// Export pane reservations (active + historical) with optional pane/time/limit filters
    pub async fn export_reservations(&self, query: ExportQuery) -> Result<Vec<PaneReservation>> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.export_reservations_with_cx(&cx, query).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`export_reservations`].
    pub async fn export_reservations_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: ExportQuery,
    ) -> Result<Vec<PaneReservation>> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("export_reservations cancelled: {err}"))
        })?;
        let db_path = Arc::clone(&self.db_path);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            pooled_backend(db_path.as_str(), |backend| {
                export::query_export_reservations(backend, &query)
            })
        })
        .await
    }

    /// Expire all stale reservations (past their TTL).
    ///
    /// Returns the number of reservations expired.
    pub async fn expire_stale_reservations(&self) -> Result<usize> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.expire_stale_reservations_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`expire_stale_reservations`].
    /// Tick 173: inlined to route the mpsc send through `send_with_cx`.
    pub async fn expire_stale_reservations_with_cx(&self, cx: &crate::cx::Cx) -> Result<usize> {
        cx.checkpoint().map_err(|err| {
            StorageError::Database(format!("expire_stale_reservations cancelled: {err}"))
        })?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send_with_cx(cx, WriteCommand::ExpireStaleReservations { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;
        Self::recv_writer_response(rx).await
    }

    /// Shutdown the storage handle
    ///
    /// Flushes all pending writes and waits for the writer thread to exit.
    /// Safe to call multiple times - subsequent calls are no-ops.
    ///
    /// br-ft-cdcbv: the writer-thread `JoinHandle::join()` runs on the
    /// blocking thread pool via `runtime_async::spawn_blocking` so the
    /// async executor is never stalled on a slow shutdown (mid-batch
    /// writes can push exit latency to hundreds of ms; a writer panic
    /// can stall the join indefinitely until the panic surfaces).
    pub async fn shutdown(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        // Send shutdown command
        let _ = self
            .write_tx
            .send(WriteCommand::Shutdown { respond: tx })
            .await;

        // Wait for acknowledgment
        Self::recv_writer_shutdown_ack(rx).await;

        // Wait for thread to finish (only the first caller does this)
        let handle = self
            .writer_handle
            .lock()
            .map_err(|_| StorageError::Database("Writer handle mutex poisoned".to_string()))?
            .take();
        if let Some(handle) = handle {
            // Run the blocking thread join on the blocking thread
            // pool so the async executor stays responsive. The
            // inner Result distinguishes panic (Err(())) from
            // clean exit (Ok(())); the outer Result is the
            // spawn_blocking JoinError surface.
            crate::runtime_async::spawn_blocking(move || handle.join().map_err(|_| ()))
                .await
                .map_err(|e| StorageError::Database(format!("Shutdown spawn_blocking error: {e}")))?
                .map_err(|()| StorageError::Database("Writer thread panicked".to_string()))?;
        }

        Ok(())
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`shutdown`].
    ///
    /// Always enqueues the shutdown command (so the writer
    /// thread always gets a chance to drain), then awaits the
    /// shutdown ack + thread join only if the cx is not already
    /// cancelled. Matches the `WatchdogHandle::join_with_cx` and
    /// `PaneOutputSubscription::shutdown_with_cx` patterns: a
    /// cx-cancelled caller bails fast while the background
    /// writer still winds down on its own.
    ///
    /// Note: if the cx is cancelled before the thread join, the
    /// writer_handle is left in place (take is skipped), so a
    /// subsequent `shutdown()` or `shutdown_with_cx(fresh_cx)`
    /// call will drive the join.
    ///
    /// Tick 176: the shutdown enqueue now routes through
    /// `send_with_cx` — closing the last remaining orphan-cx
    /// hole in storage write-path methods. This also means a
    /// cx-cancelled shutdown caller releases the mpsc reserve
    /// immediately instead of holding it until backpressure
    /// drains, which matters if callers race shutdown against
    /// a saturating writer queue.
    ///
    /// br-ft-cdcbv: the writer-thread join runs on the blocking
    /// thread pool via `runtime_async::spawn_blocking` so the
    /// executor is never stalled on a slow shutdown. The await
    /// is also select-raced against the caller's Cx cancellation
    /// watcher (50 ms poll period, matching
    /// `distributed::race_with_cx_cancel`'s pattern) so a mid-
    /// flight cancel returns Ok promptly. The orphaned join
    /// runs to completion in the background; the writer thread
    /// terminates either way + the handle is consumed once.
    pub async fn shutdown_with_cx(&self, cx: &crate::cx::Cx) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .write_tx
            .send_with_cx(cx, WriteCommand::Shutdown { respond: tx })
            .await;
        Self::recv_writer_shutdown_ack(rx).await;

        if cx.checkpoint().is_err() {
            return Ok(());
        }

        let handle = self
            .writer_handle
            .lock()
            .map_err(|_| StorageError::Database("Writer handle mutex poisoned".to_string()))?
            .take();
        if let Some(handle) = handle {
            // Run the join on the blocking thread pool; select-
            // race against a cx cancel-watcher so a cancelled
            // caller bails fast while the writer still winds down
            // independently. Same shape as
            // distributed::race_with_cx_cancel (tick 387).
            use futures::future::{Either, select};
            let join_fut = std::pin::pin!(crate::runtime_async::spawn_blocking(move || {
                handle.join().map_err(|_| ())
            }));
            let cancel_watcher = std::pin::pin!(async {
                loop {
                    let _ = crate::runtime_async::sleep_with_cx(
                        cx,
                        std::time::Duration::from_millis(50),
                    )
                    .await;
                    if cx.is_cancel_requested() {
                        return;
                    }
                }
            });
            match select(join_fut, cancel_watcher).await {
                Either::Left((Ok(Ok(())), _)) => {}
                Either::Left((Ok(Err(())), _)) => {
                    return Err(StorageError::Database("Writer thread panicked".to_string()).into());
                }
                Either::Left((Err(e), _)) => {
                    return Err(StorageError::Database(format!(
                        "Shutdown spawn_blocking error: {e}"
                    ))
                    .into());
                }
                Either::Right(((), _)) => {
                    // Cancelled mid-join. The writer_handle is
                    // already taken, so subsequent shutdown calls
                    // skip the join arm; the orphaned blocking
                    // task continues + drops its result when the
                    // writer thread eventually exits.
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

/// Search options for FTS queries
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Maximum number of results
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by time range (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range (epoch ms)
    pub until: Option<i64>,
    /// Include snippets in results (default: true)
    pub include_snippets: Option<bool>,
    /// Include full highlighted content in results (default: same as
    /// `include_snippets`). Set to `Some(false)` to skip the FTS5
    /// `highlight()` materialization while still receiving snippets —
    /// useful when callers display only the snippet column. (ft-okhhj)
    pub include_highlights: Option<bool>,
    /// Maximum tokens per snippet (default: 64)
    pub snippet_max_tokens: Option<usize>,
    /// Snippet highlight prefix (default: ">>>")
    pub highlight_prefix: Option<String>,
    /// Snippet highlight suffix (default: "<<<")
    pub highlight_suffix: Option<String>,
}

/// Semantic lane budget/caching controls for hybrid retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticBudgetConfig {
    /// Maximum allowed semantic-lane latency before adaptive backoff activates.
    pub max_semantic_latency_ms: u64,
    /// Cooldown duration applied after budget overruns.
    pub semantic_backoff_cooldown_ms: i64,
    /// Maximum semantic queries allowed per rate-limit window.
    pub max_semantic_queries_per_window: u32,
    /// Rate-limit window size in milliseconds.
    pub rate_limit_window_ms: i64,
    /// Maximum semantic query cache entries.
    pub cache_capacity: usize,
    /// Time-to-live for semantic cache entries.
    pub cache_ttl_ms: i64,
    /// Maximum candidate rows scanned per semantic query.
    pub max_semantic_scan_rows: usize,
    /// EWMA smoothing factor for adaptive latency tracking in [0.0, 1.0].
    pub latency_ewma_alpha: f64,
}

impl Default for SemanticBudgetConfig {
    fn default() -> Self {
        Self {
            max_semantic_latency_ms: 75,
            semantic_backoff_cooldown_ms: 5_000,
            max_semantic_queries_per_window: 32,
            rate_limit_window_ms: 1_000,
            cache_capacity: 256,
            cache_ttl_ms: 30_000,
            max_semantic_scan_rows: 4_000,
            latency_ewma_alpha: 0.25,
        }
    }
}

/// Aggregate semantic budget telemetry for operator observability.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SemanticBudgetMetrics {
    /// Total semantic-lane requests evaluated.
    pub total_semantic_requests: u64,
    /// Semantic searches executed against storage.
    pub semantic_queries_executed: u64,
    /// Semantic requests served from cache.
    pub semantic_cache_hits: u64,
    /// Semantic cache misses.
    pub semantic_cache_misses: u64,
    /// Semantic cache entries invalidated (stale generation/ttl).
    pub semantic_cache_invalidations: u64,
    /// Cache evictions due to bounded capacity.
    pub semantic_cache_evictions: u64,
    /// Semantic requests skipped due to active budget backoff.
    pub semantic_skipped_backoff: u64,
    /// Semantic requests skipped due to semantic rate limiting.
    pub semantic_skipped_rate_limited: u64,
    /// Semantic executions that exceeded latency budget.
    pub semantic_latency_exceeded: u64,
    /// Backoff activations triggered by latency/rate controls.
    pub semantic_backoff_activations: u64,
    /// Total candidate rows scanned across semantic executions.
    pub semantic_rows_scanned_total: u64,
    /// Total semantic hits returned across executions.
    pub semantic_hits_returned_total: u64,
    /// Last observed semantic-lane latency.
    pub last_semantic_latency_ms: u64,
    /// Last observed semantic candidate rows scanned.
    pub last_semantic_rows_scanned: usize,
    /// Whether the last semantic response came from cache.
    pub last_semantic_cache_hit: bool,
    /// Last semantic fallback reason when semantic lane was unavailable.
    pub last_fallback_reason: Option<String>,
}

/// Snapshot of semantic budget configuration and live telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticBudgetSnapshot {
    /// Effective semantic budget controls.
    pub config: SemanticBudgetConfig,
    /// Collected telemetry counters.
    pub metrics: SemanticBudgetMetrics,
    /// EWMA latency tracker for adaptive guardrails.
    pub ewma_semantic_latency_ms: f64,
    /// Active backoff deadline (epoch ms) if semantic lane is paused.
    pub backoff_until_ms: Option<i64>,
    /// Current semantic cache entry count.
    pub cache_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SemanticQueryCacheKey {
    embedder_id: String,
    pane_id: Option<u64>,
    since: Option<i64>,
    until: Option<i64>,
    limit: usize,
    query_vector_hash: u64,
    query_vector_len: usize,
}

#[derive(Debug, Clone)]
struct CachedSemanticHits {
    hits: Vec<SemanticSearchHit>,
    expires_at_ms: i64,
    generation: u64,
}

#[derive(Debug)]
enum SemanticBudgetDecision {
    Execute {
        key: SemanticQueryCacheKey,
        max_scan_rows: usize,
    },
    UseCache {
        hits: Vec<SemanticSearchHit>,
    },
    Skip {
        reason: String,
        budget_state: String,
        backoff_until_ms: Option<i64>,
    },
}

#[derive(Debug)]
struct SemanticBudgetState {
    config: SemanticBudgetConfig,
    metrics: SemanticBudgetMetrics,
    ewma_semantic_latency_ms: f64,
    backoff_until_ms: Option<i64>,
    rate_window_started_at_ms: i64,
    rate_window_queries: u32,
    generation: u64,
    cache: LruCache<SemanticQueryCacheKey, CachedSemanticHits>,
}

impl SemanticBudgetState {
    fn new(config: SemanticBudgetConfig) -> Self {
        let cache_capacity = config.cache_capacity.max(1);
        Self {
            config,
            metrics: SemanticBudgetMetrics::default(),
            ewma_semantic_latency_ms: 0.0,
            backoff_until_ms: None,
            rate_window_started_at_ms: 0,
            rate_window_queries: 0,
            generation: 0,
            cache: LruCache::new(cache_capacity),
        }
    }

    fn snapshot(&self) -> SemanticBudgetSnapshot {
        SemanticBudgetSnapshot {
            config: self.config.clone(),
            metrics: self.metrics.clone(),
            ewma_semantic_latency_ms: self.ewma_semantic_latency_ms,
            backoff_until_ms: self.backoff_until_ms,
            cache_entries: self.cache.len(),
        }
    }

    fn configure(&mut self, config: SemanticBudgetConfig) {
        self.config = config;
        self.cache = LruCache::new(self.config.cache_capacity.max(1));
        self.backoff_until_ms = None;
        self.rate_window_started_at_ms = 0;
        self.rate_window_queries = 0;
        self.ewma_semantic_latency_ms = 0.0;
    }

    fn invalidate_cache(&mut self) {
        let removed = self.cache.len();
        self.cache.clear();
        self.generation = self.generation.wrapping_add(1);
        if removed > 0 {
            self.metrics.semantic_cache_invalidations = self
                .metrics
                .semantic_cache_invalidations
                .saturating_add(u64::try_from(removed).unwrap_or(u64::MAX));
        }
    }

    fn begin_semantic_lane(
        &mut self,
        now_ms: i64,
        options: &SearchOptions,
        embedder_id: &str,
        query_vector: &[f32],
    ) -> SemanticBudgetDecision {
        self.metrics.total_semantic_requests =
            self.metrics.total_semantic_requests.saturating_add(1);

        if let Some(until_ms) = self.backoff_until_ms {
            if now_ms < until_ms {
                self.metrics.semantic_skipped_backoff =
                    self.metrics.semantic_skipped_backoff.saturating_add(1);
                self.metrics.last_semantic_cache_hit = false;
                self.metrics.last_fallback_reason = Some("semantic_budget_backoff".to_string());
                return SemanticBudgetDecision::Skip {
                    reason: "semantic_budget_backoff".to_string(),
                    budget_state: "backoff".to_string(),
                    backoff_until_ms: Some(until_ms),
                };
            }
            self.backoff_until_ms = None;
        }

        if self.config.max_semantic_queries_per_window > 0 && self.config.rate_limit_window_ms > 0 {
            if self.rate_window_started_at_ms == 0
                || now_ms.saturating_sub(self.rate_window_started_at_ms)
                    >= self.config.rate_limit_window_ms
            {
                self.rate_window_started_at_ms = now_ms;
                self.rate_window_queries = 0;
            }

            if self.rate_window_queries >= self.config.max_semantic_queries_per_window {
                self.metrics.semantic_skipped_rate_limited =
                    self.metrics.semantic_skipped_rate_limited.saturating_add(1);
                self.metrics.last_semantic_cache_hit = false;
                self.metrics.last_fallback_reason = Some("semantic_rate_limited".to_string());
                let backoff_until_ms =
                    now_ms.saturating_add(self.config.semantic_backoff_cooldown_ms.max(0));
                self.backoff_until_ms = Some(backoff_until_ms);
                self.metrics.semantic_backoff_activations =
                    self.metrics.semantic_backoff_activations.saturating_add(1);
                return SemanticBudgetDecision::Skip {
                    reason: "semantic_rate_limited".to_string(),
                    budget_state: "rate_limited".to_string(),
                    backoff_until_ms: self.backoff_until_ms,
                };
            }
            self.rate_window_queries = self.rate_window_queries.saturating_add(1);
        }

        let key = semantic_query_cache_key(embedder_id, options, query_vector);
        if let Some(entry) = self.cache.get(&key).cloned() {
            if entry.generation == self.generation && entry.expires_at_ms >= now_ms {
                self.metrics.semantic_cache_hits =
                    self.metrics.semantic_cache_hits.saturating_add(1);
                self.metrics.last_semantic_cache_hit = true;
                self.metrics.last_fallback_reason = None;
                self.metrics.last_semantic_latency_ms = 0;
                self.metrics.last_semantic_rows_scanned = 0;
                return SemanticBudgetDecision::UseCache { hits: entry.hits };
            }

            let _ = self.cache.remove(&key);
            self.metrics.semantic_cache_invalidations =
                self.metrics.semantic_cache_invalidations.saturating_add(1);
        }

        self.metrics.semantic_cache_misses = self.metrics.semantic_cache_misses.saturating_add(1);
        self.metrics.last_semantic_cache_hit = false;

        let configured_scan = self.config.max_semantic_scan_rows.max(1);
        let requested_limit = options.limit.unwrap_or(100).max(1);
        let max_scan_rows = configured_scan.max(requested_limit);

        SemanticBudgetDecision::Execute { key, max_scan_rows }
    }

    fn complete_semantic_lane(
        &mut self,
        now_ms: i64,
        key: SemanticQueryCacheKey,
        hits: &[SemanticSearchHit],
        latency_ms: u64,
        rows_scanned: usize,
    ) -> Option<i64> {
        self.metrics.semantic_queries_executed =
            self.metrics.semantic_queries_executed.saturating_add(1);
        self.metrics.last_semantic_latency_ms = latency_ms;
        self.metrics.last_semantic_rows_scanned = rows_scanned;
        self.metrics.last_fallback_reason = None;
        self.metrics.semantic_rows_scanned_total = self
            .metrics
            .semantic_rows_scanned_total
            .saturating_add(u64::try_from(rows_scanned).unwrap_or(u64::MAX));
        self.metrics.semantic_hits_returned_total = self
            .metrics
            .semantic_hits_returned_total
            .saturating_add(u64::try_from(hits.len()).unwrap_or(u64::MAX));

        let alpha = self.config.latency_ewma_alpha.clamp(0.0, 1.0);
        if self.metrics.semantic_queries_executed == 1 || self.ewma_semantic_latency_ms <= 0.0 {
            self.ewma_semantic_latency_ms = latency_ms as f64;
        } else {
            self.ewma_semantic_latency_ms =
                (1.0 - alpha).mul_add(self.ewma_semantic_latency_ms, alpha * latency_ms as f64);
        }

        let latency_limit = self.config.max_semantic_latency_ms;
        if latency_ms >= latency_limit {
            self.metrics.semantic_latency_exceeded =
                self.metrics.semantic_latency_exceeded.saturating_add(1);
            let backoff_until_ms =
                now_ms.saturating_add(self.config.semantic_backoff_cooldown_ms.max(0));
            self.backoff_until_ms = Some(backoff_until_ms);
            self.metrics.semantic_backoff_activations =
                self.metrics.semantic_backoff_activations.saturating_add(1);
        }

        let ttl_ms = self.config.cache_ttl_ms.max(1);
        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        let evicted = self.cache.put(
            key,
            CachedSemanticHits {
                hits: hits.to_vec(),
                expires_at_ms,
                generation: self.generation,
            },
        );
        if evicted.is_some() {
            self.metrics.semantic_cache_evictions =
                self.metrics.semantic_cache_evictions.saturating_add(1);
        }

        self.backoff_until_ms
    }

    fn note_semantic_fallback_reason(&mut self, reason: &str) {
        self.metrics.last_fallback_reason = Some(reason.to_string());
    }
}

fn semantic_query_cache_key(
    embedder_id: &str,
    options: &SearchOptions,
    query_vector: &[f32],
) -> SemanticQueryCacheKey {
    SemanticQueryCacheKey {
        embedder_id: embedder_id.to_string(),
        pane_id: options.pane_id,
        since: options.since,
        until: options.until,
        limit: options.limit.unwrap_or(100),
        query_vector_hash: hash_query_vector(query_vector),
        query_vector_len: query_vector.len(),
    }
}

fn hash_query_vector(query_vector: &[f32]) -> u64 {
    let mut hasher = DefaultHasher::new();
    query_vector.len().hash(&mut hasher);
    for value in query_vector {
        value.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// Severity level for query lint findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchLintSeverity {
    /// Query is invalid and should not be executed.
    Error,
    /// Query is valid but likely unintended.
    Warning,
}

/// Lint finding for a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchLint {
    /// Stable lint identifier.
    pub code: String,
    /// Severity of the lint finding.
    pub severity: SearchLintSeverity,
    /// Human-readable description.
    pub message: String,
    /// Suggested fix or example query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Suggestion for completing a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSuggestion {
    /// Suggested query fragment.
    pub text: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

struct SearchSuggestionTemplate {
    text: &'static str,
    description: &'static str,
}

const SEARCH_SUGGESTION_TEMPLATES: &[SearchSuggestionTemplate] = &[
    SearchSuggestionTemplate {
        text: "error",
        description: "Common errors",
    },
    SearchSuggestionTemplate {
        text: "warning",
        description: "Warnings in output",
    },
    SearchSuggestionTemplate {
        text: "panic",
        description: "Rust panics",
    },
    SearchSuggestionTemplate {
        text: "\"usage limit\"",
        description: "Usage limit messages",
    },
    SearchSuggestionTemplate {
        text: "\"rate limit\"",
        description: "Rate limit messages",
    },
    SearchSuggestionTemplate {
        text: "\"approval needed\"",
        description: "Approval prompts",
    },
    SearchSuggestionTemplate {
        text: "compaction",
        description: "Compaction output",
    },
    SearchSuggestionTemplate {
        text: "AND",
        description: "Boolean AND operator",
    },
    SearchSuggestionTemplate {
        text: "OR",
        description: "Boolean OR operator",
    },
    SearchSuggestionTemplate {
        text: "NOT",
        description: "Boolean NOT operator",
    },
    SearchSuggestionTemplate {
        text: "\"exact phrase\"",
        description: "Quoted phrase search",
    },
    SearchSuggestionTemplate {
        text: "term*",
        description: "Prefix wildcard search",
    },
    SearchSuggestionTemplate {
        text: "content:term",
        description: "Restrict to content column",
    },
];

/// Provide deterministic search query suggestions for CLI/TUI autocomplete.
#[must_use]
pub fn search_query_suggestions(query: &str, limit: usize) -> Vec<SearchSuggestion> {
    if limit == 0 {
        return Vec::new();
    }

    let prefix = search_suggestion_prefix(query);
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut suggestions = Vec::new();

    for template in SEARCH_SUGGESTION_TEMPLATES {
        if prefix.is_empty()
            || template
                .text
                .to_ascii_lowercase()
                .starts_with(prefix_lower.as_str())
        {
            suggestions.push(SearchSuggestion {
                text: template.text.to_string(),
                description: Some(template.description.to_string()),
            });
            if suggestions.len() >= limit {
                return suggestions;
            }
        }
    }

    if suggestions.is_empty() && !prefix.is_empty() {
        for template in SEARCH_SUGGESTION_TEMPLATES {
            if template
                .text
                .to_ascii_lowercase()
                .contains(prefix_lower.as_str())
            {
                suggestions.push(SearchSuggestion {
                    text: template.text.to_string(),
                    description: Some(template.description.to_string()),
                });
                if suggestions.len() >= limit {
                    break;
                }
            }
        }
    }

    suggestions
}

fn search_suggestion_prefix(query: &str) -> &str {
    let trimmed = query.trim_end();
    if trimmed.is_empty() {
        return "";
    }
    trimmed.split_whitespace().next_back().unwrap_or("")
}

/// Lint an FTS query for common mistakes.
#[must_use]
pub fn lint_fts_query(query: &str) -> Vec<SearchLint> {
    let trimmed = query.trim();
    let mut lints = Vec::new();

    if trimmed.is_empty() {
        lints.push(SearchLint {
            code: "empty_query".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Query is empty.".to_string(),
            suggestion: Some("Try: ft search \"error\"".to_string()),
        });
        return lints;
    }

    let (tokens, unbalanced_quotes, paren_imbalance, paren_underflow) = tokenize_fts_query(trimmed);

    if unbalanced_quotes {
        lints.push(SearchLint {
            code: "unbalanced_quotes".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Unbalanced double quotes in query.".to_string(),
            suggestion: Some("Close the quote or remove it.".to_string()),
        });
    }

    if paren_underflow {
        lints.push(SearchLint {
            code: "unmatched_paren_close".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Unmatched closing parenthesis in query.".to_string(),
            suggestion: Some("Remove the extra ')' or add a matching '('.".to_string()),
        });
    } else if paren_imbalance {
        lints.push(SearchLint {
            code: "unbalanced_parentheses".to_string(),
            severity: SearchLintSeverity::Warning,
            message: "Unbalanced parentheses in query.".to_string(),
            suggestion: Some("Check grouping parentheses for a match.".to_string()),
        });
    }

    if tokens.is_empty() {
        lints.push(SearchLint {
            code: "empty_tokens".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Query contains no searchable tokens.".to_string(),
            suggestion: Some("Add at least one term, e.g. \"error\".".to_string()),
        });
        return lints;
    }

    let mut prev_operator = false;
    for (idx, token) in tokens.iter().enumerate() {
        let token_trim = token.trim();
        if token_trim.is_empty() {
            continue;
        }

        if is_operator_token(token_trim) {
            if idx == 0 {
                lints.push(SearchLint {
                    code: "leading_operator".to_string(),
                    severity: SearchLintSeverity::Error,
                    message: format!("Query starts with operator '{token_trim}'."),
                    suggestion: Some("Start with a term or a quoted phrase.".to_string()),
                });
            }
            if idx + 1 == tokens.len() {
                lints.push(SearchLint {
                    code: "trailing_operator".to_string(),
                    severity: SearchLintSeverity::Error,
                    message: format!("Query ends with operator '{token_trim}'."),
                    suggestion: Some("Add a term after the operator.".to_string()),
                });
            }
            if prev_operator {
                lints.push(SearchLint {
                    code: "double_operator".to_string(),
                    severity: SearchLintSeverity::Error,
                    message: "Consecutive boolean operators detected.".to_string(),
                    suggestion: Some("Remove the extra operator.".to_string()),
                });
            }
            prev_operator = true;
            continue;
        }

        prev_operator = false;

        if is_quoted_token(token_trim) {
            continue;
        }

        if token_trim == "*" {
            lints.push(SearchLint {
                code: "wildcard_only".to_string(),
                severity: SearchLintSeverity::Error,
                message: "Wildcard '*' cannot be used alone.".to_string(),
                suggestion: Some("Use a term with prefix wildcard, e.g. \"err*\".".to_string()),
            });
        } else if token_trim.contains('*') {
            if !token_trim.ends_with('*') {
                lints.push(SearchLint {
                    code: "wildcard_position".to_string(),
                    severity: SearchLintSeverity::Warning,
                    message: format!("Wildcard in '{token_trim}' is not in suffix position."),
                    suggestion: Some("Use prefix search syntax like \"term*\".".to_string()),
                });
            } else if token_trim.starts_with('*') {
                lints.push(SearchLint {
                    code: "wildcard_prefix".to_string(),
                    severity: SearchLintSeverity::Warning,
                    message: format!("Leading wildcard in '{token_trim}' is not supported."),
                    suggestion: Some("Use a suffix wildcard like \"term*\".".to_string()),
                });
            }
        }
    }

    lints
}

fn tokenize_fts_query(query: &str) -> (Vec<String>, bool, bool, bool) {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut paren_balance: i32 = 0;
    let mut paren_underflow = false;

    for ch in query.chars() {
        if escaped {
            buf.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            buf.push(ch);
            escaped = true;
            continue;
        }

        if ch == '"' {
            in_quotes = !in_quotes;
            buf.push(ch);
            continue;
        }

        if !in_quotes {
            match ch {
                '(' => paren_balance += 1,
                ')' => {
                    paren_balance -= 1;
                    if paren_balance < 0 {
                        paren_underflow = true;
                    }
                }
                _ => {}
            }
        }

        if ch.is_whitespace() && !in_quotes {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
        } else {
            buf.push(ch);
        }
    }

    if !buf.is_empty() {
        tokens.push(buf);
    }

    let unbalanced_quotes = in_quotes;
    let paren_imbalance = paren_balance != 0;

    (tokens, unbalanced_quotes, paren_imbalance, paren_underflow)
}

fn is_operator_token(token: &str) -> bool {
    let upper = token.to_ascii_uppercase();
    matches!(upper.as_str(), "AND" | "OR" | "NOT")
}

fn is_quoted_token(token: &str) -> bool {
    token.len() >= 2 && token.starts_with('"') && token.ends_with('"')
}

/// Query options for events
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Maximum number of results (default: 20)
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by rule ID (exact match)
    pub rule_id: Option<String>,
    /// Filter by event type (e.g., "compaction_warning")
    pub event_type: Option<String>,
    /// Filter by triage state (exact match)
    pub triage_state: Option<String>,
    /// Filter by label (exact match)
    pub label: Option<String>,
    /// Only return unhandled events
    pub unhandled_only: bool,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

/// Query options for cursor-based event streaming/replay.
#[derive(Debug, Clone, Default)]
pub struct EventStreamQuery {
    /// Resume after this event ID (exclusive)
    pub after_id: Option<i64>,
    /// Maximum number of results (default: 100)
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by rule ID (exact match)
    pub rule_id: Option<String>,
    /// Filter by event type (e.g., "compaction_warning")
    pub event_type: Option<String>,
    /// Filter by triage state (exact match)
    pub triage_state: Option<String>,
    /// Filter by label (exact match)
    pub label: Option<String>,
    /// Only return unhandled events
    pub unhandled_only: bool,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

/// Query options for export operations (shared across all export data kinds)
#[derive(Debug, Clone, Default)]
pub struct ExportQuery {
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
    /// Maximum number of results
    pub limit: Option<usize>,
}

/// Query options for incremental segment scans.
#[derive(Debug, Clone)]
pub struct SegmentScanQuery {
    /// Return segments with id strictly greater than this value.
    pub after_id: Option<i64>,
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by time range start (epoch ms).
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms).
    pub until: Option<i64>,
    /// Maximum number of results to return.
    pub limit: usize,
}

impl Default for SegmentScanQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            pane_id: None,
            since: None,
            until: None,
            limit: 1_000,
        }
    }
}

// =============================================================================
// Writer Thread Implementation
// =============================================================================

/// Maximum commands to drain per batch iteration.
const WRITER_BATCH_CAP: usize = 128;

#[derive(Debug, Serialize, Deserialize)]
struct MmapSegmentLine {
    id: i64,
    pane_id: u64,
    seq: u64,
    content: String,
    content_hash: Option<String>,
    captured_at: i64,
}

fn encode_mmap_segment_line(segment: &Segment) -> Result<String> {
    let line = MmapSegmentLine {
        id: segment.id,
        pane_id: segment.pane_id,
        seq: segment.seq,
        content: segment.content.clone(),
        content_hash: segment.content_hash.clone(),
        captured_at: segment.captured_at,
    };
    serde_json::to_string(&line).map_err(|error| {
        StorageError::Database(format!("Failed to encode mmap segment line: {error}")).into()
    })
}

fn decode_mmap_segment_line(raw_line: &str) -> Result<Segment> {
    let line: MmapSegmentLine = serde_json::from_str(raw_line).map_err(|error| {
        StorageError::Database(format!("Failed to decode mmap segment line: {error}"))
    })?;
    let content_len = line.content.len();
    Ok(Segment {
        id: line.id,
        pane_id: line.pane_id,
        seq: line.seq,
        content: line.content,
        content_len,
        content_hash: line.content_hash,
        captured_at: line.captured_at,
    })
}

fn init_mmap_mirror_store(
    runtime: Option<&MmapMirrorRuntimeConfig>,
) -> Option<mmap_store::MmapScrollbackStore> {
    let runtime = runtime?;

    let config = mmap_store::MmapStoreConfig::new(runtime.base_dir.clone());
    match mmap_store::MmapScrollbackStore::new(config) {
        Ok(store) => {
            tracing::info!(
                path = %runtime.base_dir.display(),
                "mmap segment mirror enabled"
            );
            Some(store)
        }
        Err(error) => {
            tracing::warn!(
                path = %runtime.base_dir.display(),
                error = %error,
                "failed to initialize mmap segment mirror; continuing with sqlite only"
            );
            None
        }
    }
}

fn mirror_segment_into_mmap(
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
    segment: &Segment,
) {
    let Some(store) = mmap_mirror.as_mut() else {
        return;
    };

    let write_result = encode_mmap_segment_line(segment).and_then(|line| {
        store.append_line(segment.pane_id, &line).map_err(|error| {
            StorageError::Database(format!(
                "Failed to append mmap segment mirror line: {error}"
            ))
            .into()
        })
    });

    if let Err(error) = write_result {
        tracing::warn!(
            pane_id = segment.pane_id,
            seq = segment.seq,
            error = %error,
            "mmap segment mirror write failed; disabling mirror lane"
        );
        *mmap_mirror = None;
    }
}

#[derive(Debug)]
struct StorageIoWriterGate {
    scheduler: StorageIoScheduler,
    next_work_id: u64,
    next_ordering_sequence_by_stream: HashMap<String, u64>,
}

impl Default for StorageIoWriterGate {
    fn default() -> Self {
        Self::new(StorageIoSchedulerConfig::default())
    }
}

impl StorageIoWriterGate {
    fn new(config: StorageIoSchedulerConfig) -> Self {
        Self {
            scheduler: StorageIoScheduler::new(config),
            next_work_id: 1,
            next_ordering_sequence_by_stream: HashMap::new(),
        }
    }

    fn admit_command(&mut self, cmd: &WriteCommand) -> Option<(u64, StorageIoAdmissionDecision)> {
        let item = self.work_item_for_command(cmd)?;
        let work_id = item.id;
        let decision = self.scheduler.admit(item, storage_io_now_ms());
        Some((work_id, decision))
    }

    fn pop_next(&mut self) -> Option<crate::storage::io_scheduler::StorageIoDispatchedWork> {
        self.scheduler.pop_next(storage_io_now_ms())
    }

    fn work_item_for_command(&mut self, cmd: &WriteCommand) -> Option<StorageIoWorkItem> {
        match cmd {
            WriteCommand::AppendSegment {
                pane_id, content, ..
            } => Some(self.ordered_pane_work_item(
                StorageIoClass::PaneSegmentDurable,
                *pane_id,
                storage_io_str_bytes(content),
            )),
            WriteCommand::RecordGap {
                pane_id, reason, ..
            } => Some(self.ordered_pane_work_item(
                StorageIoClass::GapAndContinuity,
                *pane_id,
                storage_io_str_bytes(reason),
            )),
            WriteCommand::RecordEvent { event, .. } => Some(StorageIoWorkItem::new(
                self.next_work_id(),
                StorageIoClass::WorkflowEvent,
                storage_io_stored_event_bytes(event),
            )),
            WriteCommand::RecordAuditAction { action, .. } => Some(StorageIoWorkItem::new(
                self.next_work_id(),
                StorageIoClass::PolicyAudit,
                storage_io_audit_action_bytes(action),
            )),
            WriteCommand::RecordPolicyDenialAudit { record, .. } => Some(StorageIoWorkItem::new(
                self.next_work_id(),
                StorageIoClass::PolicyAudit,
                storage_io_policy_denial_bytes(record),
            )),
            WriteCommand::SyncFts { config, .. } => Some(StorageIoWorkItem::new(
                self.next_work_id(),
                StorageIoClass::FtsIncremental,
                storage_io_fts_sync_bytes(config),
            )),
            WriteCommand::RebuildFts { config, .. } => Some(StorageIoWorkItem::new(
                self.next_work_id(),
                StorageIoClass::FtsRebuild,
                storage_io_fts_rebuild_bytes(config),
            )),
            _ => None,
        }
    }

    fn ordered_pane_work_item(
        &mut self,
        class: StorageIoClass,
        pane_id: u64,
        estimated_bytes: u64,
    ) -> StorageIoWorkItem {
        let stream = format!("pane:{pane_id}");
        let sequence = self.next_ordering_sequence(&stream);
        StorageIoWorkItem::new(self.next_work_id(), class, estimated_bytes)
            .with_ordering(stream, sequence)
    }

    fn next_ordering_sequence(&mut self, stream: &str) -> u64 {
        let next = self
            .next_ordering_sequence_by_stream
            .entry(stream.to_string())
            .or_insert(0);
        let sequence = *next;
        *next = next.saturating_add(1);
        sequence
    }

    fn next_work_id(&mut self) -> u64 {
        let work_id = self.next_work_id;
        self.next_work_id = self.next_work_id.saturating_add(1);
        work_id
    }
}

fn dispatch_write_command_batch(
    conn: &mut Connection,
    batch: VecDeque<WriteCommand>,
    should_break: &mut bool,
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
    segment_redactors: &mut HashMap<u64, StreamingRedactor>,
    io_gate: &mut StorageIoWriterGate,
) {
    let mut pending_io = HashMap::<u64, WriteCommand>::new();

    for cmd in batch {
        if *should_break {
            break;
        }

        if let Some((work_id, decision)) = io_gate.admit_command(&cmd) {
            if decision.outcome.accepted() && decision.queued {
                tracing::debug!(
                    command = storage_io_command_name(&cmd),
                    io_class = decision.class.as_str(),
                    outcome = decision.outcome.as_str(),
                    reason_code = %decision.reason_code(),
                    queue_depth = decision.queue_depth,
                    bytes_pending = decision.bytes_pending,
                    "storage IO scheduler admitted writer command"
                );
                pending_io.insert(work_id, cmd);
            } else {
                let message = storage_io_admission_failure_message(&cmd, &decision);
                tracing::warn!(
                    command = storage_io_command_name(&cmd),
                    io_class = decision.class.as_str(),
                    outcome = decision.outcome.as_str(),
                    reason_code = %decision.reason_code(),
                    queue_depth = decision.queue_depth,
                    bytes_pending = decision.bytes_pending,
                    retry_after_ms = decision.retry_after_ms,
                    "storage IO scheduler rejected writer command before persistence"
                );
                respond_storage_io_rejection(cmd, message);
            }
            continue;
        }

        flush_storage_io_pending_commands(
            conn,
            &mut pending_io,
            should_break,
            mmap_mirror,
            segment_redactors,
            io_gate,
        );
        if !*should_break {
            dispatch_write_command_raw(conn, cmd, should_break, mmap_mirror, segment_redactors);
        }
    }

    flush_storage_io_pending_commands(
        conn,
        &mut pending_io,
        should_break,
        mmap_mirror,
        segment_redactors,
        io_gate,
    );
}

fn flush_storage_io_pending_commands(
    conn: &mut Connection,
    pending_io: &mut HashMap<u64, WriteCommand>,
    should_break: &mut bool,
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
    segment_redactors: &mut HashMap<u64, StreamingRedactor>,
    io_gate: &mut StorageIoWriterGate,
) {
    while !pending_io.is_empty() && !*should_break {
        let Some(dispatched) = io_gate.pop_next() else {
            let message =
                "storage IO scheduler lost queued writer commands before dispatch".to_string();
            tracing::error!(
                pending = pending_io.len(),
                "storage IO scheduler returned no dispatchable work for queued commands"
            );
            fail_pending_storage_io_commands(std::mem::take(pending_io), message);
            break;
        };

        let work_id = dispatched.item.id;
        let Some(cmd) = pending_io.remove(&work_id) else {
            tracing::error!(
                work_id,
                io_class = dispatched.item.class.as_str(),
                "storage IO scheduler dispatched unknown writer work item"
            );
            continue;
        };

        tracing::debug!(
            command = storage_io_command_name(&cmd),
            work_id,
            io_class = dispatched.item.class.as_str(),
            queued_for_ms = dispatched.queued_for_ms,
            "storage IO scheduler dispatching writer command"
        );
        dispatch_write_command_raw(conn, cmd, should_break, mmap_mirror, segment_redactors);
    }
}

fn fail_pending_storage_io_commands(commands: HashMap<u64, WriteCommand>, message: String) {
    for (_, cmd) in commands {
        respond_storage_io_rejection(cmd, message.clone());
    }
}

fn storage_io_now_ms() -> u64 {
    u64::try_from(now_epoch_ms()).unwrap_or(0)
}

fn storage_io_admission_failure_message(
    cmd: &WriteCommand,
    decision: &StorageIoAdmissionDecision,
) -> String {
    format!(
        "storage IO scheduler rejected {} before durable persistence: reason_code={} class={} outcome={} queue_depth={} bytes_pending={} retry_after_ms={:?}",
        storage_io_command_name(cmd),
        decision.reason_code(),
        decision.class.as_str(),
        decision.outcome.as_str(),
        decision.queue_depth,
        decision.bytes_pending,
        decision.retry_after_ms
    )
}

fn storage_io_command_name(cmd: &WriteCommand) -> &'static str {
    match cmd {
        WriteCommand::AppendSegment { .. } => "AppendSegment",
        WriteCommand::RecordGap { .. } => "RecordGap",
        WriteCommand::RecordEvent { .. } => "RecordEvent",
        WriteCommand::RecordAuditAction { .. } => "RecordAuditAction",
        WriteCommand::RecordPolicyDenialAudit { .. } => "RecordPolicyDenialAudit",
        WriteCommand::SyncFts { .. } => "SyncFts",
        WriteCommand::RebuildFts { .. } => "RebuildFts",
        _ => "WriteCommand",
    }
}

fn respond_storage_io_rejection(cmd: WriteCommand, message: String) {
    match cmd {
        WriteCommand::AppendSegment { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        WriteCommand::RecordGap { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        WriteCommand::RecordEvent { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        WriteCommand::RecordAuditAction { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        WriteCommand::RecordPolicyDenialAudit { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        WriteCommand::SyncFts { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        WriteCommand::RebuildFts { respond, .. } => {
            respond_oneshot_best_effort(respond, Err(StorageError::Database(message).into()));
        }
        other => {
            tracing::error!(
                command = ?other,
                "storage IO scheduler rejection reached non-routed writer command"
            );
        }
    }
}

fn storage_io_str_bytes(value: &str) -> u64 {
    u64::try_from(value.len()).unwrap_or(u64::MAX).max(1)
}

fn storage_io_usize_bytes(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX).max(1)
}

fn storage_io_option_str_bytes(value: Option<&str>) -> u64 {
    value.map_or(0, storage_io_str_bytes)
}

fn storage_io_json_bytes(value: Option<&serde_json::Value>) -> u64 {
    value.map_or(0, |json| storage_io_str_bytes(&json.to_string()))
}

fn storage_io_stored_event_bytes(event: &StoredEvent) -> u64 {
    128_u64
        .saturating_add(storage_io_str_bytes(&event.rule_id))
        .saturating_add(storage_io_str_bytes(&event.agent_type))
        .saturating_add(storage_io_str_bytes(&event.event_type))
        .saturating_add(storage_io_str_bytes(&event.severity))
        .saturating_add(storage_io_option_str_bytes(event.matched_text.as_deref()))
        .saturating_add(storage_io_option_str_bytes(event.dedupe_key.as_deref()))
        .saturating_add(storage_io_option_str_bytes(
            event.handled_by_workflow_id.as_deref(),
        ))
        .saturating_add(storage_io_option_str_bytes(event.handled_status.as_deref()))
        .saturating_add(storage_io_json_bytes(event.extracted.as_ref()))
        .max(1)
}

fn storage_io_audit_action_bytes(action: &AuditActionRecord) -> u64 {
    128_u64
        .saturating_add(storage_io_str_bytes(&action.actor_kind))
        .saturating_add(storage_io_option_str_bytes(action.actor_id.as_deref()))
        .saturating_add(storage_io_option_str_bytes(
            action.correlation_id.as_deref(),
        ))
        .saturating_add(storage_io_option_str_bytes(action.domain.as_deref()))
        .saturating_add(storage_io_str_bytes(&action.action_kind))
        .saturating_add(storage_io_str_bytes(&action.policy_decision))
        .saturating_add(storage_io_option_str_bytes(
            action.decision_reason.as_deref(),
        ))
        .saturating_add(storage_io_option_str_bytes(action.rule_id.as_deref()))
        .saturating_add(storage_io_option_str_bytes(action.input_summary.as_deref()))
        .saturating_add(storage_io_option_str_bytes(
            action.verification_summary.as_deref(),
        ))
        .saturating_add(storage_io_option_str_bytes(
            action.decision_context.as_deref(),
        ))
        .saturating_add(storage_io_str_bytes(&action.result))
        .max(1)
}

fn storage_io_policy_denial_bytes(record: &PolicyDeniedAuditRecord) -> u64 {
    128_u64
        .saturating_add(storage_io_option_str_bytes(record.agent_id.as_deref()))
        .saturating_add(storage_io_str_bytes(&record.tool_name))
        .saturating_add(storage_io_option_str_bytes(record.intent_hash.as_deref()))
        .saturating_add(storage_io_str_bytes(&record.reason))
        .saturating_add(storage_io_str_bytes(&record.reason_code))
        .saturating_add(storage_io_option_str_bytes(record.rule_id.as_deref()))
        .saturating_add(storage_io_str_bytes(&record.decision))
        .max(1)
}

fn storage_io_fts_sync_bytes(config: &FtsSyncConfig) -> u64 {
    storage_io_usize_bytes(config.max_batch_bytes)
        .max(storage_io_usize_bytes(config.batch_size))
        .max(1)
}

fn storage_io_fts_rebuild_bytes(config: &FtsSyncConfig) -> u64 {
    storage_io_fts_sync_bytes(config).saturating_mul(4).max(1)
}

/// Main loop for the writer thread.
///
/// Opportunistically processes burst traffic while preserving immediate
/// per-command dispatch semantics.
///
/// Every `WriteCommand` resolves a caller-facing oneshot from
/// `dispatch_write_command_raw()`. Wrapping multiple commands in an explicit
/// transaction would let individual commands report success before a later
/// `COMMIT` could still fail, turning a durability failure into a false `Ok`.
/// Until the response path can defer replies until the transaction outcome is
/// known, the writer must stay in SQLite's per-statement autocommit mode.
///
/// Additional queued commands are still drained opportunistically after the first
/// wakeup. Segment, gap, event, and audit commands first pass through the
/// storage IO scheduler; non-routed commands act as barriers and every routed
/// command is executed before its caller receives `Ok`.
fn writer_loop(
    conn: &mut Connection,
    rx: &mut mpsc::Receiver<WriteCommand>,
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
) {
    let mut segment_redactors = HashMap::<u64, StreamingRedactor>::new();
    let mut io_gate = StorageIoWriterGate::default();

    // ft-ixgqo: removed the per-thread asupersync runtime + `block_on`
    // bridge that ft-3tvvt's audit flagged. The writer runs on a
    // dedicated `std::thread`, so async-channel recv was bridged via
    // `RuntimeBuilder::current_thread().block_on(...)` — i.e., the
    // library secretly stood up its own executor for one purpose.
    //
    // The fix: poll the channel with `try_recv()` (a sync, non-
    // allocating call exposed by the asupersync mpsc receiver) and
    // park the OS thread for 1 ms when no command is queued. SQL
    // dispatch was already sync; now the entire writer thread is
    // sync end-to-end with no runtime dependency. Wake-up latency
    // under no-load is bounded at 1 ms (negligible relative to
    // SQLite per-statement autocommit cost). Channel close —
    // whether by `Disconnected` (sender dropped) or `Cancelled`
    // (cx-aware shutdown) — terminates the loop cleanly.
    'main: loop {
        match rx.try_recv() {
            Ok(first_cmd) => {
                let mut should_break = false;
                let mut batch = VecDeque::with_capacity(WRITER_BATCH_CAP);
                batch.push_back(first_cmd);

                while batch.len() < WRITER_BATCH_CAP {
                    let Ok(cmd) = rx.try_recv() else {
                        break;
                    };
                    batch.push_back(cmd);
                }

                dispatch_write_command_batch(
                    conn,
                    batch,
                    &mut should_break,
                    mmap_mirror,
                    &mut segment_redactors,
                    &mut io_gate,
                );

                if should_break {
                    break 'main;
                }
            }
            Err(mpsc::RecvError::Empty) => {
                // Park briefly to avoid busy-waiting under no-load.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(mpsc::RecvError::Disconnected | mpsc::RecvError::Cancelled) => {
                break 'main;
            }
        }
    }

    flush_segment_redactors(conn, mmap_mirror, &mut segment_redactors);
}

thread_local! {
    /// Writer-thread placeholder cache for [`with_writer_backend`].
    ///
    /// `RusqliteBackend::new` consumes the connection, so the bridge has
    /// to swap the live `Connection` out of `&mut Connection` against
    /// some other `Connection` for the duration of the wrap. Allocating
    /// a fresh `Connection::open_in_memory()` on every call burned
    /// ~200 µs per WriteCommand. This thread-local caches that
    /// allocation so the bridge pays the cost ONCE per writer thread
    /// (or per test thread), not once per command.
    ///
    /// `RefCell` (not `Cell`) so the Drop guard inside
    /// [`with_writer_backend`] can hand the placeholder back through
    /// a borrow without moving it — `Cell::set` on a `Connection`
    /// would force a clone we don't have.
    static WRITER_PLACEHOLDER_POOL: std::cell::RefCell<Option<Connection>> =
        const { std::cell::RefCell::new(None) };
}

/// br-ft-l1jgo writer-thread bridge: temporarily wrap the
/// writer thread's owned `Connection` into a `RusqliteBackend`
/// for the duration of `f`, then put the `Connection` back so
/// the writer thread keeps running with the same wrapped state.
///
/// Mirrors `PooledReadConn::with_borrowed_backend` at line ~8957
/// (read-side bridge for ft-3twzm), but for the writer thread:
/// the writer holds `&mut Connection` (not `Option<Connection>`),
/// so the bridge swaps a placeholder `Connection` into that slot
/// for the duration of the wrap. `RusqliteBackend::new` consumes
/// the connection it wraps, so a placeholder is the only way to
/// keep the `&mut Connection` slot non-empty without rewriting
/// the whole writer-thread API.
///
/// **Panic safety.** The original placeholder bridge had a
/// silent-corruption hazard: if `f` panicked, the live
/// `Connection` was already inside `RusqliteBackend` and the
/// `*conn = backend.into_connection()` line was never reached.
/// Today the writer thread propagates the panic and dies cleanly,
/// but anyone wrapping `dispatch_write_command` in
/// `panic::catch_unwind` would have left `*conn` pointing at the
/// in-memory placeholder — every subsequent WriteCommand would
/// silently target a fresh empty in-memory DB. Replaced with a
/// Drop-guarded restore so the live `Connection` is always put
/// back, panic or not. `RusqliteBackend::into_connection`
/// recovers from a poisoned mutex (a rusqlite-internal panic
/// during a held lock), so the Drop path itself can't double-
/// panic and abort the process.
///
/// **Cost.** `Connection::open_in_memory()` is called at most
/// once per writer thread via [`WRITER_PLACEHOLDER_POOL`] (vs
/// every WriteCommand previously). The thread-local placeholder
/// is recycled across all calls on the same thread.
///
/// **Re-entrancy.** Not safe. The thread-local placeholder is
/// taken at the top of each call and re-parked on Drop; a nested
/// `with_writer_backend` call from inside `f` would panic at
/// `RefCell::borrow_mut` because the outer call is mid-flight
/// (the placeholder has been taken and not yet re-parked when
/// the inner call's `take()` runs — wait, that's wrong, the
/// outer's `take()` already released its borrow before invoking
/// `f`; the actual hazard is the inner call's Drop and the outer
/// call's Drop racing for the same RefCell, which they wouldn't
/// because `f` runs single-threaded). In practice the writer
/// thread never recurses, and the closure receives only
/// `&dyn StorageBackend` so it can't re-enter without explicit
/// access to a `&mut Connection`. Documented as non-reentrant
/// for clarity.
///
/// Used by per-WriteCommand dispatch handlers that have been
/// migrated to call a `_backend` helper instead of a `_sync`
/// helper.
fn with_writer_backend<F, R>(conn: &mut Connection, f: F) -> R
where
    F: FnOnce(&dyn StorageBackend) -> R,
{
    let placeholder = WRITER_PLACEHOLDER_POOL
        .with(|cell| cell.borrow_mut().take())
        .unwrap_or_else(|| {
            Connection::open_in_memory()
                .expect("temp placeholder Connection for writer-thread backend wrap")
        });
    let owned = std::mem::replace(conn, placeholder);

    // Drop guard restores the live Connection from the wrapped
    // backend even if `f` panics, and parks the placeholder back
    // in the thread-local pool for the next call.
    struct WriterBridgeRestore<'a> {
        conn: &'a mut Connection,
        backend: Option<RusqliteBackend>,
    }
    impl Drop for WriterBridgeRestore<'_> {
        fn drop(&mut self) {
            if let Some(backend) = self.backend.take() {
                // `into_connection` is poison-tolerant (see
                // RusqliteBackend impl), so this branch can't
                // double-panic during unwinding.
                let placeholder = std::mem::replace(self.conn, backend.into_connection());
                WRITER_PLACEHOLDER_POOL.with(|cell| {
                    *cell.borrow_mut() = Some(placeholder);
                });
            }
        }
    }

    let restore = WriterBridgeRestore {
        conn,
        backend: Some(RusqliteBackend::new(owned)),
    };
    // `restore.backend` is `Some` from construction until the
    // Drop impl `take()`s it, so the `expect` here is provably
    // total. The borrow scoped to `restore` ends at the last use
    // of `backend_ref`, before `drop(restore)` consumes it.
    let backend_ref: &RusqliteBackend = restore
        .backend
        .as_ref()
        .expect("writer bridge backend is Some until restore Drop");
    let result = f(backend_ref);
    drop(restore);
    result
}

#[cfg(test)]
mod writer_bridge_tests {
    use super::{Connection, WRITER_PLACEHOLDER_POOL, with_writer_backend};

    /// Pinned: the live `Connection` is restored even when `f` panics.
    /// Pre-fix the `mem::replace(conn, placeholder)` had already
    /// happened by the time the closure unwound, so a hypothetical
    /// catch_unwind around `dispatch_write_command` would have left
    /// every subsequent WriteCommand pointing at the in-memory
    /// placeholder.
    #[test]
    fn with_writer_backend_restores_conn_on_panic() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE writer_bridge_panic_pin (k INTEGER)", [])
            .unwrap();
        // Identity probe: the post-panic conn must accept queries
        // against the table we just created. The placeholder would
        // not have it.
        let probe = std::panic::AssertUnwindSafe(|| {
            with_writer_backend(&mut conn, |_backend| {
                panic!("simulate dispatch_write_command panic");
            });
        });
        let unwound = std::panic::catch_unwind(probe);
        assert!(unwound.is_err(), "the panic must propagate");

        // The schema we created MUST still be reachable through `conn`.
        // If the bridge had left `conn` pointing at the in-memory
        // placeholder, this query would fail with "no such table".
        let exists: i64 = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='writer_bridge_panic_pin'",
                [],
                |row| row.get(0),
            )
            .expect("post-panic conn must still see the live schema");
        assert_eq!(exists, 1);
    }

    /// Pinned: the placeholder is recycled across calls (once-per-thread
    /// allocation). Pre-fix every WriteCommand re-allocated the
    /// in-memory placeholder.
    #[test]
    fn with_writer_backend_recycles_thread_local_placeholder() {
        // Drain any leftover placeholder from previous tests on this
        // thread so we measure a clean slate.
        WRITER_PLACEHOLDER_POOL.with(|cell| {
            cell.borrow_mut().take();
        });

        let mut conn = Connection::open_in_memory().unwrap();

        // Cold call: bridge allocates a placeholder, runs closure,
        // parks placeholder in thread-local on Drop.
        with_writer_backend(&mut conn, |_| ());
        let parked_after_cold = WRITER_PLACEHOLDER_POOL.with(|cell| cell.borrow().is_some());
        assert!(
            parked_after_cold,
            "placeholder must be parked back in the thread-local pool after the cold call"
        );

        // Warm call: bridge reuses the parked placeholder. We can't
        // observe the absence of allocation directly, but we can
        // observe that the parked placeholder was taken (during the
        // call) and re-parked (after Drop).
        with_writer_backend(&mut conn, |_| {
            // Inside the closure the placeholder is currently swapped
            // into the live-conn slot, so the thread-local pool
            // should be empty.
            let parked_during_call = WRITER_PLACEHOLDER_POOL.with(|cell| cell.borrow().is_some());
            assert!(
                !parked_during_call,
                "placeholder must be in-flight (not in thread-local) for the duration of f"
            );
        });
        let parked_after_warm = WRITER_PLACEHOLDER_POOL.with(|cell| cell.borrow().is_some());
        assert!(
            parked_after_warm,
            "placeholder must be parked back after the warm call too"
        );
    }
}

#[cfg(test)]
mod writer_io_scheduler_tests {
    use super::io_scheduler::{StorageIoAdmissionOutcome, StorageIoClassBudget};
    use super::*;

    fn tiny_writer_gate() -> StorageIoWriterGate {
        let mut cfg = StorageIoSchedulerConfig {
            aggregate_max_items: 8,
            aggregate_max_bytes: 8192,
            max_consecutive_per_class: 1,
            ..StorageIoSchedulerConfig::default()
        };
        cfg.class_budgets.insert(
            StorageIoClass::PaneSegmentDurable,
            StorageIoClassBudget::deferrable(4, 4096, 1),
        );
        cfg.class_budgets.insert(
            StorageIoClass::GapAndContinuity,
            StorageIoClassBudget::strict(4, 4096, 0),
        );
        cfg.class_budgets.insert(
            StorageIoClass::PolicyAudit,
            StorageIoClassBudget::strict(1, 4096, 0),
        );
        cfg.class_budgets.insert(
            StorageIoClass::WorkflowEvent,
            StorageIoClassBudget::deferrable(1, 4096, 3),
        );
        cfg.class_budgets.insert(
            StorageIoClass::FtsIncremental,
            StorageIoClassBudget::deferrable(2, 4096, 4),
        );
        cfg.class_budgets.insert(
            StorageIoClass::FtsRebuild,
            StorageIoClassBudget::deferrable(1, 8192, 5),
        );
        StorageIoWriterGate::new(cfg)
    }

    fn segment_command(pane_id: u64, content: &str) -> WriteCommand {
        let (tx, _rx) = oneshot::channel();
        WriteCommand::AppendSegment {
            pane_id,
            content: content.to_string(),
            content_hash: None,
            respond: tx,
        }
    }

    fn gap_command(pane_id: u64, reason: &str) -> WriteCommand {
        let (tx, _rx) = oneshot::channel();
        WriteCommand::RecordGap {
            pane_id,
            reason: reason.to_string(),
            respond: tx,
        }
    }

    fn event_command(pane_id: u64, rule_id: &str) -> WriteCommand {
        let (tx, _rx) = oneshot::channel();
        WriteCommand::RecordEvent {
            event: StoredEvent {
                id: 0,
                pane_id,
                rule_id: rule_id.to_string(),
                agent_type: "codex".to_string(),
                event_type: "state_transition".to_string(),
                severity: "info".to_string(),
                confidence: 1.0,
                extracted: None,
                matched_text: Some("done".to_string()),
                segment_id: None,
                detected_at: 1,
                dedupe_key: None,
                handled_at: None,
                handled_by_workflow_id: None,
                handled_status: None,
            },
            respond: tx,
        }
    }

    fn policy_denial_command(tool_name: &str) -> WriteCommand {
        let (tx, _rx) = oneshot::channel();
        WriteCommand::RecordPolicyDenialAudit {
            record: PolicyDeniedAuditRecord {
                id: 0,
                ts_ms: 1,
                agent_id: Some("agent".to_string()),
                tool_name: tool_name.to_string(),
                intent_hash: Some("intent".to_string()),
                reason: "denied by test policy".to_string(),
                reason_code: PolicyDeniedAuditRecord::REASON_CODE_DENIED.to_string(),
                rule_id: Some("rule".to_string()),
                decision: PolicyDeniedAuditRecord::DECISION_DENIED.to_string(),
            },
            respond: tx,
        }
    }

    fn fts_sync_command(batch_size: usize, max_batch_bytes: usize) -> WriteCommand {
        let (tx, _rx) = oneshot::channel();
        WriteCommand::SyncFts {
            config: FtsSyncConfig {
                batch_size,
                max_batch_bytes,
                commit_progress: true,
            },
            respond: tx,
        }
    }

    fn fts_rebuild_command(batch_size: usize, max_batch_bytes: usize) -> WriteCommand {
        let (tx, _rx) = oneshot::channel();
        WriteCommand::RebuildFts {
            config: FtsSyncConfig {
                batch_size,
                max_batch_bytes,
                commit_progress: true,
            },
            respond: tx,
        }
    }

    #[test]
    fn writer_io_gate_preserves_segment_gap_order_inside_batch() {
        let mut gate = tiny_writer_gate();
        let segment = segment_command(7, "first durable segment");
        let gap = gap_command(7, "capture gap after segment");
        let event = event_command(7, "state-ready");

        let (segment_id, segment_decision) = gate.admit_command(&segment).unwrap();
        let (gap_id, gap_decision) = gate.admit_command(&gap).unwrap();
        let (event_id, event_decision) = gate.admit_command(&event).unwrap();

        assert_eq!(segment_decision.class, StorageIoClass::PaneSegmentDurable);
        assert_eq!(gap_decision.class, StorageIoClass::GapAndContinuity);
        assert_eq!(event_decision.class, StorageIoClass::WorkflowEvent);
        assert!(segment_decision.outcome.accepted());
        assert!(gap_decision.outcome.accepted());
        assert!(event_decision.outcome.accepted());

        let first = gate.pop_next().unwrap();
        let second = gate.pop_next().unwrap();
        let third = gate.pop_next().unwrap();

        assert_eq!(first.item.id, segment_id);
        assert_eq!(second.item.id, gap_id);
        assert_eq!(third.item.id, event_id);
    }

    #[test]
    fn writer_io_gate_policy_audit_fail_closed_has_reason_code() {
        let mut gate = tiny_writer_gate();
        let first = policy_denial_command("ft.first");
        let second = policy_denial_command("ft.second");

        let (_, first_decision) = gate.admit_command(&first).unwrap();
        let (_, second_decision) = gate.admit_command(&second).unwrap();

        assert!(first_decision.outcome.accepted());
        assert_eq!(
            second_decision.outcome,
            StorageIoAdmissionOutcome::FailClosed
        );
        assert_eq!(
            second_decision.reason_code(),
            "storage_io.fail_closed.audit_required"
        );

        let message = storage_io_admission_failure_message(&second, &second_decision);
        assert!(message.contains("RecordPolicyDenialAudit"));
        assert!(message.contains("storage_io.fail_closed.audit_required"));
    }

    #[test]
    fn writer_io_gate_event_defer_has_explicit_diagnostic() {
        let mut gate = tiny_writer_gate();
        let first = event_command(9, "first-event");
        let second = event_command(9, "second-event");

        let (_, first_decision) = gate.admit_command(&first).unwrap();
        let (_, second_decision) = gate.admit_command(&second).unwrap();

        assert!(first_decision.outcome.accepted());
        assert_eq!(second_decision.outcome, StorageIoAdmissionOutcome::Defer);
        assert_eq!(
            second_decision.reason_code(),
            "storage_io.defer.class_budget_exhausted"
        );

        let message = storage_io_admission_failure_message(&second, &second_decision);
        assert!(message.contains("RecordEvent"));
        assert!(message.contains("storage_io.defer.class_budget_exhausted"));
        assert!(message.contains("before durable persistence"));
    }

    #[test]
    fn writer_io_gate_keeps_segment_ahead_of_fts_catchup() {
        let mut gate = tiny_writer_gate();
        let fts = fts_sync_command(2, 512);
        let segment = segment_command(42, "fresh pane output must stay durable first");

        let (fts_id, fts_decision) = gate.admit_command(&fts).unwrap();
        let (segment_id, segment_decision) = gate.admit_command(&segment).unwrap();

        assert_eq!(fts_decision.class, StorageIoClass::FtsIncremental);
        assert_eq!(segment_decision.class, StorageIoClass::PaneSegmentDurable);
        assert!(fts_decision.outcome.accepted());
        assert!(segment_decision.outcome.accepted());

        let first = gate.pop_next().unwrap();
        let second = gate.pop_next().unwrap();

        assert_eq!(first.item.id, segment_id);
        assert_eq!(first.item.class, StorageIoClass::PaneSegmentDurable);
        assert_eq!(second.item.id, fts_id);
        assert_eq!(second.item.class, StorageIoClass::FtsIncremental);
    }

    #[test]
    fn writer_io_gate_fts_budget_defer_has_search_reason_code() {
        let mut gate = tiny_writer_gate();
        let first = fts_sync_command(2, 512);
        let second = fts_sync_command(2, 512);
        let third = fts_sync_command(2, 512);

        let (_, first_decision) = gate.admit_command(&first).unwrap();
        let (_, second_decision) = gate.admit_command(&second).unwrap();
        let (_, third_decision) = gate.admit_command(&third).unwrap();

        assert!(first_decision.outcome.accepted());
        assert!(second_decision.outcome.accepted());
        assert_eq!(third_decision.outcome, StorageIoAdmissionOutcome::Defer);
        assert_eq!(
            third_decision.reason_code(),
            "storage_io.defer.class_budget_exhausted"
        );

        let message = storage_io_admission_failure_message(&third, &third_decision);
        assert!(message.contains("SyncFts"));
        assert!(message.contains("fts_incremental"));
        assert!(message.contains("storage_io.defer.class_budget_exhausted"));
    }

    #[test]
    fn writer_io_gate_routes_full_rebuild_as_lower_priority_index_work() {
        let mut gate = tiny_writer_gate();
        let incremental = fts_sync_command(1, 256);
        let rebuild = fts_rebuild_command(1, 256);

        let (incremental_id, incremental_decision) = gate.admit_command(&incremental).unwrap();
        let (rebuild_id, rebuild_decision) = gate.admit_command(&rebuild).unwrap();

        assert_eq!(incremental_decision.class, StorageIoClass::FtsIncremental);
        assert_eq!(rebuild_decision.class, StorageIoClass::FtsRebuild);
        assert!(incremental_decision.outcome.accepted());
        assert!(rebuild_decision.outcome.accepted());

        let first = gate.pop_next().unwrap();
        let second = gate.pop_next().unwrap();

        assert_eq!(first.item.id, incremental_id);
        assert_eq!(first.item.class, StorageIoClass::FtsIncremental);
        assert_eq!(second.item.id, rebuild_id);
        assert_eq!(second.item.class, StorageIoClass::FtsRebuild);
    }
}

/// br-ft-x2oyy: ship `value` back through the
/// `WriteCommand::respond` oneshot channel under the
/// receiver-dropped-is-fine contract documented on
/// [`dispatch_write_command_raw`]. The
/// `Result<(), oneshot::SendError<T>>` is intentionally
/// discarded — see the dispatcher's rustdoc for the design
/// rationale (oneshot send fails iff Receiver dropped → caller
/// cancelled → no longer cares about the result; the SQLite
/// write itself has already happened by this point and is
/// folded into writer-side telemetry regardless of the return path).
///
/// Replaces the 62 inline `let _ = respond.send(value);`
/// sites in `dispatch_write_command_raw`'s match arms — the helper
/// name makes the contract self-documenting at every callsite.
fn respond_oneshot_best_effort<T>(tx: oneshot::Sender<T>, value: T) {
    let _ = tx.send(value);
}

/// Dispatch a single already-admitted write command to the appropriate sync handler.
///
/// br-ft-x2oyy: every `WriteCommand` variant carries a `respond:
/// oneshot::Sender<Result<...>>` channel; the dispatcher computes
/// the result and ships it back via `let _ = respond.send(result)`.
/// The `Result<(), SendError<T>>` from `oneshot::Sender::send` is
/// intentionally discarded because:
///
/// 1. **Receiver-dropped is the only failure mode**: `oneshot::send`
///    fails iff the corresponding `Receiver` was dropped before the
///    write completed. That happens when the caller cancelled
///    (deadline exceeded, task aborted, Cx-budget exhausted) — at
///    which point the caller no longer cares about the result.
///    Surfacing the `SendError` would only spam the writer-thread
///    log with cancellation noise.
/// 2. **The write itself already happened**: by the time we
///    `respond.send(result)`, the SQLite write has either
///    committed or returned an error to the local `result`
///    binding. A failed `send` does not roll the write back; it
///    just means the return path was severed.
/// 3. **Forensics are preserved**: the `result` itself, including
///    any `Err` from `append_segment_backend` / `record_gap_backend`
///    / `record_event_backend` / etc., is still folded into the
///    writer-side telemetry counters via the worker-loop wrapper.
///    Operators see write-side failures via that telemetry, not
///    via the per-command `respond.send`.
///
/// The 61 call sites in this function ALL share the above
/// contract and route through [`respond_oneshot_best_effort`]
/// (defined above) — the helper's name makes the contract
/// self-documenting at every callsite. The original inline
/// `let _ = respond.send(...)` pattern is no longer present in
/// the dispatch arms.
fn dispatch_write_command_raw(
    conn: &mut Connection,
    cmd: WriteCommand,
    should_break: &mut bool,
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
    segment_redactors: &mut HashMap<u64, StreamingRedactor>,
) {
    match cmd {
        WriteCommand::AppendSegment {
            pane_id,
            content,
            content_hash,
            respond,
        } => {
            let redacted_content =
                redact_segment_for_persistence(pane_id, &content, segment_redactors);
            let persisted_hash = if redacted_content == content {
                content_hash.as_deref()
            } else {
                None
            };
            let result = with_writer_backend(conn, |backend| {
                append_segment_backend(backend, pane_id, &redacted_content, persisted_hash)
            });
            if let Ok(segment) = &result {
                mirror_segment_into_mmap(mmap_mirror, segment);
            }
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordGap {
            pane_id,
            reason,
            respond,
        } => {
            let result =
                flush_segment_redactor_for_pane(conn, mmap_mirror, segment_redactors, pane_id)
                    .and_then(|()| {
                        with_writer_backend(conn, |backend| {
                            record_gap_backend(backend, pane_id, &reason)
                        })
                    });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordEvent { event, respond } => {
            let result = with_writer_backend(conn, |backend| record_event_backend(backend, &event));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::MarkEventHandled {
            event_id,
            workflow_id,
            status,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                mark_event_handled_backend(backend, event_id, workflow_id.as_deref(), &status)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::SetEventTriageState {
            event_id,
            triage_state,
            updated_by,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                set_event_triage_state_backend(
                    backend,
                    event_id,
                    triage_state.as_deref(),
                    updated_by.as_deref(),
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::SetEventNote {
            event_id,
            note,
            updated_by,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                set_event_note_backend(backend, event_id, note.as_deref(), updated_by.as_deref())
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::AddEventLabel {
            event_id,
            label,
            created_by,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                add_event_label_backend(backend, event_id, &label, created_by.as_deref())
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RemoveEventLabel {
            event_id,
            label,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                remove_event_label_backend(backend, event_id, &label)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertEventMute { record, respond } => {
            let result =
                with_writer_backend(conn, |backend| upsert_event_mute_backend(backend, &record));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeleteEventMute {
            identity_key,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                delete_event_mute_backend(backend, &identity_key)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertPane { pane, respond } => {
            let result = with_writer_backend(conn, |backend| upsert_pane_backend(backend, &pane));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertWorkflow { workflow, respond } => {
            let result =
                with_writer_backend(conn, |backend| upsert_workflow_backend(backend, &workflow));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertActionPlan { record, respond } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `upsert_action_plan_sync(&Connection, &WorkflowActionPlanRecord)`
            // direct-rusqlite path.
            let result =
                with_writer_backend(conn, |backend| upsert_action_plan_backend(backend, &record));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertPreparedPlan { record, respond } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `insert_prepared_plan_sync(&Connection, &PreparedPlanRecord)`
            // direct-rusqlite path.
            let result = with_writer_backend(conn, |backend| {
                insert_prepared_plan_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::ConsumePreparedPlan {
            plan_id,
            now_ms,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                consume_prepared_plan_backend(backend, &plan_id, now_ms)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertStepLog {
            workflow_id,
            audit_action_id,
            step_index,
            step_name,
            step_id,
            step_kind,
            result_type,
            result_data,
            policy_summary,
            verification_refs,
            error_code,
            started_at,
            completed_at,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                insert_step_log_backend(
                    backend,
                    &workflow_id,
                    audit_action_id,
                    step_index,
                    &step_name,
                    step_id.as_deref(),
                    step_kind.as_deref(),
                    &result_type,
                    result_data.as_deref(),
                    policy_summary.as_deref(),
                    verification_refs.as_deref(),
                    error_code.as_deref(),
                    started_at,
                    completed_at,
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertSession { session, respond } => {
            let result = with_writer_backend(conn, |backend| {
                upsert_agent_session_backend(backend, &session)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordAuditAction { action, respond } => {
            let result = with_writer_backend(conn, |backend| {
                record_audit_action_backend(backend, &action)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordPolicyDenialAudit { record, respond } => {
            let result = with_writer_backend(conn, |backend| {
                record_policy_denial_audit_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertActionUndo { record, respond } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `upsert_action_undo_sync(&Connection, &ActionUndoRecord)`
            // direct-rusqlite path.
            let result =
                with_writer_backend(conn, |backend| upsert_action_undo_backend(backend, &record));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::MarkActionUndone {
            audit_action_id,
            undone_at,
            undone_by,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                mark_action_undone_backend(backend, audit_action_id, undone_at, &undone_by)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::PurgeAuditActions { before_ts, respond } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `purge_audit_actions_sync(&Connection, i64)` direct-
            // rusqlite path.
            let result = with_writer_backend(conn, |backend| {
                purge_audit_actions_backend(backend, before_ts)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertApprovalToken { token, respond } => {
            let result = with_writer_backend(conn, |backend| {
                insert_approval_token_backend(backend, &token)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::ConsumeApprovalToken {
            code_hash,
            workspace_id,
            action_kind,
            pane_id,
            action_fingerprint,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                consume_approval_token_backend(
                    backend,
                    &code_hash,
                    &workspace_id,
                    &action_kind,
                    pane_id,
                    &action_fingerprint,
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::ConsumeApprovalTokenByCode {
            code_hash,
            workspace_id,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                consume_approval_token_by_code_backend(backend, &code_hash, &workspace_id)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordMaintenance { record, respond } => {
            let result =
                with_writer_backend(conn, |backend| record_maintenance_backend(backend, &record));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordSecretScanReport { record, respond } => {
            let result = with_writer_backend(conn, |backend| {
                record_secret_scan_report_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertSavedSearch { record, respond } => {
            let result = with_writer_backend(conn, |backend| {
                insert_saved_search_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpdateSavedSearchRun {
            id,
            last_run_at,
            last_result_count,
            last_error,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                update_saved_search_run_backend(
                    backend,
                    &id,
                    last_run_at,
                    last_result_count,
                    last_error.as_deref(),
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpdateSavedSearchSchedule {
            id,
            enabled,
            schedule_interval_ms,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                update_saved_search_schedule_backend(backend, &id, enabled, schedule_interval_ms)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeleteSavedSearch { name, respond } => {
            let result =
                with_writer_backend(conn, |backend| delete_saved_search_backend(backend, &name));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::SyncFts { config, respond } => {
            let result = with_writer_backend(conn, |backend| {
                sync_fts_on_startup_backend(backend, &config)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RebuildFts { config, respond } => {
            let result =
                with_writer_backend(conn, |backend| full_fts_rebuild_backend(backend, &config));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::PruneSegments { before_ts, respond } => {
            let result =
                with_writer_backend(conn, |backend| prune_segments_backend(backend, before_ts));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::Vacuum { respond } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `vacuum_sync(&Connection)` direct-rusqlite path.
            let result = with_writer_backend(conn, vacuum_backend);
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::Checkpoint { respond } => {
            let result = with_writer_backend(conn, checkpoint_backend);
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpsertAccount { account, respond } => {
            let result =
                with_writer_backend(conn, |backend| upsert_account_backend(backend, &account));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpdateAccountLastUsed {
            service,
            account_id,
            last_used_at,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                update_account_last_used_backend(backend, &service, &account_id, last_used_at)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeleteAccount {
            service,
            account_id,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                delete_account_backend(backend, &service, &account_id)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::CreateReservation {
            pane_id,
            owner_kind,
            owner_id,
            reason,
            ttl_ms,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                create_reservation_backend(
                    backend,
                    pane_id,
                    &owner_kind,
                    &owner_id,
                    reason.as_deref(),
                    ttl_ms,
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::ReleaseReservation {
            reservation_id,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                release_reservation_backend(backend, reservation_id)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::ExpireStaleReservations { respond } => {
            let result = with_writer_backend(conn, expire_stale_reservations_backend);
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordUsageMetric { record, respond } => {
            let result = with_writer_backend(conn, |backend| {
                record_usage_metric_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordUsageMetricsBatch { records, respond } => {
            // br-ft-l1jgo: trait-typed bulk insert via execute_many
            // (was direct rusqlite Transaction + prepare + loop in
            // record_usage_metrics_batch_sync).
            let result = with_writer_backend(conn, |backend| {
                record_usage_metrics_batch_backend(backend, &records)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::PurgeUsageMetrics { before_ts, respond } => {
            let result = with_writer_backend(conn, |backend| {
                purge_usage_metrics_backend(backend, before_ts)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::RecordNotification { record, respond } => {
            let result = with_writer_backend(conn, |backend| {
                record_notification_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::UpdateNotificationStatus {
            id,
            status,
            error_message,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                update_notification_status_backend(backend, id, status, error_message.as_deref())
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::AcknowledgeNotification {
            id,
            acknowledged_by,
            action_taken,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                acknowledge_notification_backend(
                    backend,
                    id,
                    &acknowledged_by,
                    action_taken.as_deref(),
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::IncrementNotificationRetry { id, respond } => {
            let result = with_writer_backend(conn, |backend| {
                increment_notification_retry_backend(backend, id)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::PurgeNotificationHistory { before_ts, respond } => {
            let result = with_writer_backend(conn, |backend| {
                purge_notification_history_backend(backend, before_ts)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeleteEventsBefore {
            before_ts,
            batch_size,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                delete_events_before_backend(backend, before_ts, batch_size)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeleteEventsByTier {
            before_ts,
            severities,
            event_types,
            handled,
            batch_size,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                delete_events_by_tier_backend(
                    backend,
                    before_ts,
                    &severities,
                    &event_types,
                    handled,
                    batch_size,
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertPaneBookmark { record, respond } => {
            let result = with_writer_backend(conn, |backend| {
                insert_pane_bookmark_backend(backend, &record)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeletePaneBookmark { alias, respond } => {
            let result = with_writer_backend(conn, |backend| {
                delete_pane_bookmark_backend(backend, &alias)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertAgentProfile { profile, respond } => {
            let result = with_writer_backend(conn, |backend| {
                insert_agent_profile_backend(backend, &profile)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::GetAgentProfile { name, respond } => {
            let result =
                with_writer_backend(conn, |backend| get_agent_profile_backend(backend, &name));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::ListAgentProfiles {
            role_filter,
            respond,
        } => {
            let result = with_writer_backend(conn, |backend| {
                list_agent_profiles_backend(backend, role_filter.as_deref())
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::DeleteAgentProfile { name, respond } => {
            let result =
                with_writer_backend(conn, |backend| delete_agent_profile_backend(backend, &name));
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertMuxSession {
            session_id,
            topology_json,
            ft_version,
            host_id,
            respond,
        } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `insert_mux_session_sync(&Connection, ...)` direct-
            // rusqlite path.
            let result = with_writer_backend(conn, |backend| {
                insert_mux_session_backend(
                    backend,
                    &session_id,
                    &topology_json,
                    &ft_version,
                    host_id.as_deref(),
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::InsertSessionCheckpoint {
            session_id,
            checkpoint_type,
            state_hash,
            pane_count,
            total_bytes,
            metadata_json,
            pane_states,
            respond,
        } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `insert_session_checkpoint_sync(&mut Connection, ...)`
            // direct-rusqlite transaction path.
            let result = with_writer_backend(conn, |backend| {
                insert_session_checkpoint_backend(
                    backend,
                    &session_id,
                    &checkpoint_type,
                    &state_hash,
                    pane_count,
                    total_bytes,
                    metadata_json.as_deref(),
                    &pane_states,
                )
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::PruneSessionCheckpoints {
            session_id,
            retention,
            respond,
        } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `prune_session_checkpoints_sync(&Connection, &str, usize)`
            // direct-rusqlite path.
            let result = with_writer_backend(conn, |backend| {
                prune_session_checkpoints_backend(backend, &session_id, retention)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::MarkSessionShutdownClean {
            session_id,
            respond,
        } => {
            // br-ft-l1jgo: routes through the trait via the writer-
            // thread wrap-unwrap bridge. Replaces the legacy
            // `mark_session_shutdown_clean_sync(&Connection, ...)`
            // direct-rusqlite path.
            let result = with_writer_backend(conn, |backend| {
                mark_session_shutdown_clean_backend(backend, &session_id)
            });
            respond_oneshot_best_effort(respond, result);
        }
        WriteCommand::Shutdown { respond } => {
            flush_segment_redactors(conn, mmap_mirror, segment_redactors);
            respond_oneshot_best_effort(respond, ());
            *should_break = true;
        }
    }
}

fn redact_segment_for_persistence(
    pane_id: u64,
    content: &str,
    segment_redactors: &mut HashMap<u64, StreamingRedactor>,
) -> String {
    let result = segment_redactors
        .entry(pane_id)
        .or_default()
        .redact_chunk(content.as_bytes());
    redaction_result_to_string(result)
}

fn redaction_result_to_string(result: RedactionResult) -> String {
    String::from_utf8(result.bytes).unwrap_or_else(|error| {
        let bytes = error.into_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn flush_segment_redactor_for_pane(
    conn: &mut Connection,
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
    segment_redactors: &mut HashMap<u64, StreamingRedactor>,
    pane_id: u64,
) -> Result<()> {
    let Some(mut redactor) = segment_redactors.remove(&pane_id) else {
        return Ok(());
    };

    let content = redaction_result_to_string(redactor.finish());
    if content.is_empty() {
        return Ok(());
    }

    let segment = with_writer_backend(conn, |backend| {
        append_segment_backend(backend, pane_id, &content, None)
    })?;
    mirror_segment_into_mmap(mmap_mirror, &segment);
    Ok(())
}

fn flush_segment_redactors(
    conn: &mut Connection,
    mmap_mirror: &mut Option<mmap_store::MmapScrollbackStore>,
    segment_redactors: &mut HashMap<u64, StreamingRedactor>,
) {
    let pane_ids = segment_redactors.keys().copied().collect::<Vec<_>>();
    for pane_id in pane_ids {
        if let Err(error) =
            flush_segment_redactor_for_pane(conn, mmap_mirror, segment_redactors, pane_id)
        {
            tracing::warn!(
                pane_id,
                error = %error,
                "failed to flush pending segment redaction tail"
            );
        }
    }
}

// =============================================================================
// Synchronous Database Operations
// =============================================================================

/// Get current timestamp in epoch milliseconds
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn u64_to_i64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        StorageError::Database(format!("{label} value {value} exceeds i64 range")).into()
    })
}

fn usize_to_i64(value: usize, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        StorageError::Database(format!("{label} value {value} exceeds i64 range")).into()
    })
}

fn sql_integer_decode_error(
    column_index: usize,
    label: &str,
    value: i64,
    reason: &str,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column_index,
        rusqlite::types::Type::Integer,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label} value {value} {reason}"),
        )),
    )
}

fn i64_to_u64_sql(value: i64, column_index: usize, label: &str) -> rusqlite::Result<u64> {
    u64::try_from(value)
        .map_err(|_| sql_integer_decode_error(column_index, label, value, "is out of u64 range"))
}

fn i64_to_bool_sql(value: i64, column_index: usize, label: &str) -> rusqlite::Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(sql_integer_decode_error(
            column_index,
            label,
            value,
            "must be 0 or 1",
        )),
    }
}

fn i64_to_usize(value: i64) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

fn pane_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaneRecord> {
    Ok(PaneRecord {
        pane_id: i64_to_u64_sql(row.get(0)?, 0, "panes.pane_id")?,
        pane_uuid: row.get(1)?,
        domain: row.get(2)?,
        window_id: row
            .get::<_, Option<i64>>(3)?
            .map(|v| i64_to_u64_sql(v, 3, "panes.window_id"))
            .transpose()?,
        tab_id: row
            .get::<_, Option<i64>>(4)?
            .map(|v| i64_to_u64_sql(v, 4, "panes.tab_id"))
            .transpose()?,
        title: row.get(5)?,
        cwd: row.get(6)?,
        tty_name: row.get(7)?,
        first_seen_at: row.get(8)?,
        last_seen_at: row.get(9)?,
        observed: i64_to_bool_sql(row.get(10)?, 10, "panes.observed")?,
        ignore_reason: row.get(11)?,
        last_decision_at: row.get(12)?,
    })
}

#[cfg(test)]
fn prepared_plan_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PreparedPlanRecord> {
    Ok(PreparedPlanRecord {
        plan_id: row.get(0)?,
        plan_hash: row.get(1)?,
        workspace_id: row.get(2)?,
        action_kind: row.get(3)?,
        pane_id: row
            .get::<_, Option<i64>>(4)?
            .map(|v| i64_to_u64_sql(v, 4, "prepared_plans.pane_id"))
            .transpose()?,
        pane_uuid: row.get(5)?,
        params_json: row.get(6)?,
        plan_json: row.get(7)?,
        requires_approval: i64_to_bool_sql(row.get(8)?, 8, "prepared_plans.requires_approval")?,
        created_at: row.get(9)?,
        expires_at: row.get(10)?,
        consumed_at: row.get(11)?,
    })
}

fn approval_token_from_backend_row(row: &[String]) -> Result<ApprovalTokenRecord> {
    let reader = RowReader::new(row);
    let pane_id = reader
        .optional_i64(7)
        .map_err(|err| storage_backend_error("Approval token pane_id", err))?
        .map(|value| backend_i64_to_u64(value, "approval_tokens.pane_id"))
        .transpose()
        .map_err(|err| storage_backend_error("Approval token pane_id", err))?;

    Ok(ApprovalTokenRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Approval token id", err))?,
        code_hash: reader
            .string(1)
            .map_err(|err| storage_backend_error("Approval token code_hash", err))?,
        created_at: reader
            .i64(2)
            .map_err(|err| storage_backend_error("Approval token created_at", err))?,
        expires_at: reader
            .i64(3)
            .map_err(|err| storage_backend_error("Approval token expires_at", err))?,
        used_at: reader
            .optional_i64(4)
            .map_err(|err| storage_backend_error("Approval token used_at", err))?,
        workspace_id: reader
            .string(5)
            .map_err(|err| storage_backend_error("Approval token workspace_id", err))?,
        action_kind: reader
            .string(6)
            .map_err(|err| storage_backend_error("Approval token action_kind", err))?,
        pane_id,
        action_fingerprint: reader
            .string(8)
            .map_err(|err| storage_backend_error("Approval token action_fingerprint", err))?,
        plan_hash: reader
            .optional_string(9)
            .map_err(|err| storage_backend_error("Approval token plan_hash", err))?,
        plan_version: reader
            .optional_i64(10)
            .map_err(|err| storage_backend_error("Approval token plan_version", err))?
            .map(|value| backend_i64_to_i32(value, "approval_tokens.plan_version"))
            .transpose()
            .map_err(|err| storage_backend_error("Approval token plan_version", err))?,
        risk_summary: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Approval token risk_summary", err))?,
    })
}

fn prepared_plan_from_backend_row(row: &[String]) -> Result<PreparedPlanRecord> {
    let reader = RowReader::new(row);
    let pane_id = reader
        .optional_i64(4)
        .map_err(|err| storage_backend_error("Prepared plan pane_id", err))?
        .map(|value| backend_i64_to_u64(value, "prepared_plans.pane_id"))
        .transpose()
        .map_err(|err| storage_backend_error("Prepared plan pane_id", err))?;
    let requires_raw = reader
        .i64(8)
        .map_err(|err| storage_backend_error("Prepared plan requires_approval", err))?;
    let requires_approval =
        backend_i64_to_bool(requires_raw, "prepared_plans.requires_approval")
            .map_err(|err| storage_backend_error("Prepared plan requires_approval", err))?;

    Ok(PreparedPlanRecord {
        plan_id: reader
            .string(0)
            .map_err(|err| storage_backend_error("Prepared plan plan_id", err))?,
        plan_hash: reader
            .string(1)
            .map_err(|err| storage_backend_error("Prepared plan plan_hash", err))?,
        workspace_id: reader
            .string(2)
            .map_err(|err| storage_backend_error("Prepared plan workspace_id", err))?,
        action_kind: reader
            .string(3)
            .map_err(|err| storage_backend_error("Prepared plan action_kind", err))?,
        pane_id,
        pane_uuid: reader
            .optional_string(5)
            .map_err(|err| storage_backend_error("Prepared plan pane_uuid", err))?,
        params_json: reader
            .optional_string(6)
            .map_err(|err| storage_backend_error("Prepared plan params_json", err))?,
        plan_json: reader
            .string(7)
            .map_err(|err| storage_backend_error("Prepared plan plan_json", err))?,
        requires_approval,
        created_at: reader
            .i64(9)
            .map_err(|err| storage_backend_error("Prepared plan created_at", err))?,
        expires_at: reader
            .i64(10)
            .map_err(|err| storage_backend_error("Prepared plan expires_at", err))?,
        consumed_at: reader
            .optional_i64(11)
            .map_err(|err| storage_backend_error("Prepared plan consumed_at", err))?,
    })
}

fn query_approval_token_by_code_backend(
    backend: &dyn StorageBackend,
    code_hash: &str,
    workspace_id: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let row = backend
        .query_row_typed(
            "SELECT id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
                    pane_id, action_fingerprint, plan_hash, plan_version, risk_summary
             FROM approval_tokens
             WHERE code_hash = ?1
               AND workspace_id = ?2
             LIMIT 1",
            &[ToSqlValue::Text(code_hash), ToSqlValue::Text(workspace_id)],
        )
        .map_err(|err| storage_backend_error("Query approval token by code", err))?;

    row.as_deref()
        .map(approval_token_from_backend_row)
        .transpose()
}

fn query_active_approvals_count_backend(
    backend: &dyn StorageBackend,
    workspace_id: &str,
    now_ms: i64,
) -> Result<u32> {
    let row = backend
        .query_row_typed(
            "SELECT COUNT(*) FROM approval_tokens
             WHERE workspace_id = ?1 AND used_at IS NULL AND expires_at >= ?2",
            &[ToSqlValue::Text(workspace_id), ToSqlValue::Integer(now_ms)],
        )
        .map_err(|err| storage_backend_error("Count active approvals", err))?;

    let Some(row) = row else {
        return Ok(0);
    };
    let count = RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Active approval count", err))?;
    u32::try_from(count).map_err(|_| {
        StorageError::Database(format!("Active approval count {count} exceeds u32 range")).into()
    })
}

fn query_approval_token_by_hash_backend(
    backend: &dyn StorageBackend,
    code_hash: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let row = backend
        .query_row_typed(
            "SELECT id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
                    pane_id, action_fingerprint, plan_hash, plan_version, risk_summary
             FROM approval_tokens
             WHERE code_hash = ?1
             LIMIT 1",
            &[ToSqlValue::Text(code_hash)],
        )
        .map_err(|err| storage_backend_error("Query approval token by hash", err))?;

    row.as_deref()
        .map(approval_token_from_backend_row)
        .transpose()
}

fn query_prepared_plan_backend(
    backend: &dyn StorageBackend,
    plan_id: &str,
) -> Result<Option<PreparedPlanRecord>> {
    let row = backend
        .query_row_typed(
            "SELECT plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
                    plan_json, requires_approval, created_at, expires_at, consumed_at
             FROM prepared_plans
             WHERE plan_id = ?1",
            &[ToSqlValue::Text(plan_id)],
        )
        .map_err(|err| storage_backend_error("Query prepared plan", err))?;

    row.as_deref()
        .map(prepared_plan_from_backend_row)
        .transpose()
}

fn query_active_approval_for_scope_backend(
    backend: &dyn StorageBackend,
    workspace_id: &str,
    action_kind: &str,
    pane_id: Option<u64>,
    action_fingerprint: &str,
    now_ms: i64,
) -> Result<bool> {
    let exists = if let Some(pane_id) = pane_id {
        let pane_id = u64_to_i64(pane_id, "approval_tokens.pane_id")?;
        row_exists_where(
            backend,
            "approval_tokens",
            "workspace_id = ?1
             AND action_kind = ?2
             AND pane_id = ?3
             AND action_fingerprint = ?4
             AND used_at IS NULL
             AND expires_at >= ?5",
            &[
                ToSqlValue::Text(workspace_id),
                ToSqlValue::Text(action_kind),
                ToSqlValue::Integer(pane_id),
                ToSqlValue::Text(action_fingerprint),
                ToSqlValue::Integer(now_ms),
            ],
        )
    } else {
        row_exists_where(
            backend,
            "approval_tokens",
            "workspace_id = ?1
             AND action_kind = ?2
             AND pane_id IS NULL
             AND action_fingerprint = ?3
             AND used_at IS NULL
             AND expires_at >= ?4",
            &[
                ToSqlValue::Text(workspace_id),
                ToSqlValue::Text(action_kind),
                ToSqlValue::Text(action_fingerprint),
                ToSqlValue::Integer(now_ms),
            ],
        )
    }
    .map_err(|err| storage_backend_error("Query active approval for scope", err))?;

    Ok(exists)
}

/// Append a segment through the storage backend (called from writer thread).
fn append_segment_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
    content: &str,
    content_hash: Option<&str>,
) -> Result<Segment> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    let next_seq_i64 = backend
        .query_row_typed(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM output_segments WHERE pane_id = ?1",
            &[ToSqlValue::Integer(pane_id_i64)],
        )
        .map_err(|e| storage_backend_error("Failed to get next seq", e))?
        .ok_or_else(|| StorageError::Database("Failed to get next seq: no row".to_string()))
        .and_then(|row| {
            RowReader::new(&row)
                .i64(0)
                .map_err(|e| storage_backend_error("Failed to parse next seq", e))
        })?;
    let next_seq = u64::try_from(next_seq_i64).map_err(|_| {
        StorageError::Database(format!(
            "output_segments.next_seq out of range: {next_seq_i64}"
        ))
    })?;

    let now = now_ms();
    let content_len = content.len();

    let next_seq_i64 = u64_to_i64(next_seq, "seq")?;
    let content_len_i64 = usize_to_i64(content_len, "content_len")?;

    let row = backend
        .query_row_typed(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, content_hash, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         RETURNING id",
        &[
            ToSqlValue::Integer(pane_id_i64),
            ToSqlValue::Integer(next_seq_i64),
            ToSqlValue::Text(content),
            ToSqlValue::Integer(content_len_i64),
            ToSqlValue::optional_text(content_hash),
            ToSqlValue::Integer(now),
        ],
    )
    .map_err(|e| storage_backend_error("Failed to insert segment", e))?
    .ok_or_else(|| StorageError::Database("Failed to insert segment: no id returned".to_string()))?;
    let id = RowReader::new(&row)
        .i64(0)
        .map_err(|e| storage_backend_error("Failed to parse inserted segment id", e))?;

    Ok(Segment {
        id,
        pane_id,
        seq: next_seq,
        content: content.to_string(),
        content_len,
        content_hash: content_hash.map(String::from),
        captured_at: now,
    })
}

/// Record a gap event through the storage backend.
fn record_gap_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
    reason: &str,
) -> Result<Option<Gap>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    let explicit_bounds = parse_distributed_gap_reason(reason);
    let (seq_before, seq_after) = if let Some((seq_before, seq_after)) = explicit_bounds {
        (seq_before, seq_after)
    } else {
        // Get the last sequence for this pane
        let last_seq_i64 = backend
            .query_row_typed(
                "SELECT MAX(seq) FROM output_segments WHERE pane_id = ?1",
                &[ToSqlValue::Integer(pane_id_i64)],
            )
            .map_err(|e| storage_backend_error("Failed to get last seq", e))?
            .map(|row| RowReader::new(&row).optional_i64(0))
            .transpose()
            .map_err(|e| storage_backend_error("Failed to parse last seq", e))?
            .flatten();

        let last_seq = last_seq_i64
            .map(|seq| {
                u64::try_from(seq).map_err(|_| {
                    StorageError::Database(format!("output_segments.seq out of range: {seq}"))
                })
            })
            .transpose()?;

        let Some(seq_before) = last_seq else {
            // No segments yet, so no local continuity gap to record.
            return Ok(None);
        };
        (seq_before, seq_before + 1)
    };

    let now = now_ms();
    let seq_before_i64 = u64_to_i64(seq_before, "seq_before")?;
    let seq_after_i64 = u64_to_i64(seq_after, "seq_after")?;

    let row = backend
        .query_row_typed(
            "INSERT INTO output_gaps (pane_id, seq_before, seq_after, reason, detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         RETURNING id",
            &[
                ToSqlValue::Integer(pane_id_i64),
                ToSqlValue::Integer(seq_before_i64),
                ToSqlValue::Integer(seq_after_i64),
                ToSqlValue::Text(reason),
                ToSqlValue::Integer(now),
            ],
        )
        .map_err(|e| storage_backend_error("Failed to insert gap", e))?
        .ok_or_else(|| {
            StorageError::Database("Failed to insert gap: no id returned".to_string())
        })?;
    let id = RowReader::new(&row)
        .i64(0)
        .map_err(|e| storage_backend_error("Failed to parse inserted gap id", e))?;

    Ok(Some(Gap {
        id,
        pane_id,
        seq_before,
        seq_after,
        reason: reason.to_string(),
        detected_at: now,
    }))
}

fn parse_distributed_gap_reason(reason: &str) -> Option<(u64, u64)> {
    let payload = reason.strip_prefix("distributed_gap:")?;
    let (prefix_with_reason, seq_after_raw) = payload.rsplit_once(':')?;
    let (reason_text, seq_before_raw) = prefix_with_reason.rsplit_once(':')?;
    let seq_before = seq_before_raw.parse::<u64>().ok()?;
    let seq_after = seq_after_raw.parse::<u64>().ok()?;
    if reason_text.is_empty() || seq_after <= seq_before {
        return None;
    }
    Some((seq_before, seq_after))
}

/// Record an event through the storage backend.
fn record_event_backend(backend: &dyn StorageBackend, event: &StoredEvent) -> Result<i64> {
    let extracted_json = event.extracted.as_ref().map(|v| {
        serde_json::to_string(v).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "event extracted field serialization failed");
            String::new()
        })
    });

    let pane_id_i64 = u64_to_i64(event.pane_id, "pane_id")?;
    let row = backend
        .query_row_typed(
            "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence,
             extracted, matched_text, segment_id, detected_at, dedupe_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(dedupe_key) DO UPDATE SET dedupe_key = excluded.dedupe_key
             RETURNING id",
            &[
                ToSqlValue::Integer(pane_id_i64),
                ToSqlValue::Text(&event.rule_id),
                ToSqlValue::Text(&event.agent_type),
                ToSqlValue::Text(&event.event_type),
                ToSqlValue::Text(&event.severity),
                ToSqlValue::Real(event.confidence),
                ToSqlValue::optional_text(extracted_json.as_deref()),
                ToSqlValue::optional_text(event.matched_text.as_deref()),
                ToSqlValue::optional_i64(event.segment_id),
                ToSqlValue::Integer(event.detected_at),
                ToSqlValue::optional_text(event.dedupe_key.as_deref()),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to insert event", err))?
        .ok_or_else(|| StorageError::Database("event insert returned no id".to_string()))?;

    RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Event id", err).into())
}

/// Mark event as handled through the storage backend.
fn mark_event_handled_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
    workflow_id: Option<&str>,
    status: &str,
) -> Result<()> {
    let now = now_ms();

    execute_typed(
        backend,
        "UPDATE events SET handled_at = ?1, handled_by_workflow_id = ?2, handled_status = ?3
         WHERE id = ?4",
        &[
            ToSqlValue::Integer(now),
            ToSqlValue::optional_text(workflow_id),
            ToSqlValue::Text(status),
            ToSqlValue::Integer(event_id),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to mark event handled", err))?;

    Ok(())
}

/// Set or clear triage state on an event row.
///
/// Returns true if an event row was updated.
fn set_event_triage_state_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
    triage_state: Option<&str>,
    updated_by: Option<&str>,
) -> Result<bool> {
    let row = if let Some(state) = triage_state {
        let now = now_ms();
        backend.query_row_typed(
            "UPDATE events
             SET triage_state = ?1,
                 triage_updated_at = ?2,
                 triage_updated_by = ?3
             WHERE id = ?4
             RETURNING 1",
            &[
                ToSqlValue::Text(state),
                ToSqlValue::Integer(now),
                ToSqlValue::optional_text(updated_by),
                ToSqlValue::Integer(event_id),
            ],
        )
    } else {
        backend.query_row_typed(
            "UPDATE events
             SET triage_state = NULL,
                 triage_updated_at = NULL,
                 triage_updated_by = NULL
             WHERE id = ?1
             RETURNING 1",
            &[ToSqlValue::Integer(event_id)],
        )
    }
    .map_err(|err| storage_backend_error("Failed to set triage state", err))?;

    Ok(row.is_some())
}

/// Set or clear the note associated with an event.
///
/// Note content is redacted before persistence to avoid storing secrets.
fn set_event_note_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
    note: Option<&str>,
    updated_by: Option<&str>,
) -> Result<()> {
    if let Some(note) = note {
        let redactor = Redactor::new();
        let note = redactor.redact(note);
        let now = now_ms();
        execute_typed(
            backend,
            "INSERT INTO event_notes (event_id, note, updated_at, updated_by)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(event_id) DO UPDATE SET
                note = excluded.note,
                updated_at = excluded.updated_at,
                updated_by = excluded.updated_by",
            &[
                ToSqlValue::Integer(event_id),
                ToSqlValue::Text(&note),
                ToSqlValue::Integer(now),
                ToSqlValue::optional_text(updated_by),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to set event note", err))?;
        return Ok(());
    }

    execute_typed(
        backend,
        "DELETE FROM event_notes WHERE event_id = ?1",
        &[ToSqlValue::Integer(event_id)],
    )
    .map_err(|err| storage_backend_error("Failed to clear event note", err))?;

    Ok(())
}

/// Add a label to an event (idempotent).
///
/// Returns true if a new label was inserted.
fn add_event_label_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
    label: &str,
    created_by: Option<&str>,
) -> Result<bool> {
    let now = now_ms();
    let row = backend
        .query_row_typed(
            "INSERT OR IGNORE INTO event_labels (event_id, label, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4)
             RETURNING 1",
            &[
                ToSqlValue::Integer(event_id),
                ToSqlValue::Text(label),
                ToSqlValue::Integer(now),
                ToSqlValue::optional_text(created_by),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to add event label", err))?;

    Ok(row.is_some())
}

/// Remove a label from an event.
///
/// Returns true if a label row was deleted.
fn remove_event_label_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
    label: &str,
) -> Result<bool> {
    let row = backend
        .query_row_typed(
            "DELETE FROM event_labels WHERE event_id = ?1 AND label = ?2 RETURNING 1",
            &[ToSqlValue::Integer(event_id), ToSqlValue::Text(label)],
        )
        .map_err(|err| storage_backend_error("Failed to remove event label", err))?;
    Ok(row.is_some())
}

/// Query all annotations for an event through the storage backend.
fn query_event_annotations_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
) -> Result<Option<EventAnnotations>> {
    let triage = backend
        .query_row_typed(
            "SELECT triage_state, triage_updated_at, triage_updated_by FROM events WHERE id = ?1",
            &[ToSqlValue::Integer(event_id)],
        )
        .map_err(|err| storage_backend_error("Failed to query triage state", err))?;

    let Some(triage) = triage else {
        return Ok(None);
    };
    let triage = RowReader::new(&triage);
    let triage_state = triage
        .optional_string(0)
        .map_err(|err| storage_backend_error("Decode triage state", err))?;
    let triage_updated_at = triage
        .optional_i64(1)
        .map_err(|err| storage_backend_error("Decode triage updated_at", err))?;
    let triage_updated_by = triage
        .optional_string(2)
        .map_err(|err| storage_backend_error("Decode triage updated_by", err))?;

    let note_row = backend
        .query_row_typed(
            "SELECT note, updated_at, updated_by FROM event_notes WHERE event_id = ?1",
            &[ToSqlValue::Integer(event_id)],
        )
        .map_err(|err| storage_backend_error("Failed to query event note", err))?;

    let (note, note_updated_at, note_updated_by) = note_row
        .as_ref()
        .map(|row| {
            let row = RowReader::new(row);
            Ok::<_, StorageError>((
                Some(
                    row.string(0)
                        .map_err(|err| storage_backend_error("Decode event note", err))?,
                ),
                Some(
                    row.i64(1)
                        .map_err(|err| storage_backend_error("Decode event note timestamp", err))?,
                ),
                row.optional_string(2)
                    .map_err(|err| storage_backend_error("Decode event note updater", err))?,
            ))
        })
        .transpose()?
        .unwrap_or((None, None, None));

    let label_rows = backend
        .query_map_typed(
            "SELECT label FROM event_labels WHERE event_id = ?1 ORDER BY label ASC",
            &[ToSqlValue::Integer(event_id)],
        )
        .map_err(|err| storage_backend_error("Labels query failed", err))?;

    let labels = label_rows
        .iter()
        .map(|row| {
            RowReader::new(row)
                .string(0)
                .map_err(|err| storage_backend_error("Decode event label", err).into())
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Some(EventAnnotations {
        triage_state,
        triage_updated_at,
        triage_updated_by,
        note,
        note_updated_at,
        note_updated_by,
        labels,
    }))
}

/// Insert or update a persistent event mute.
fn upsert_event_mute_backend(backend: &dyn StorageBackend, record: &EventMuteRecord) -> Result<()> {
    execute_typed(
        backend,
        "INSERT INTO event_mutes (identity_key, scope, created_at, expires_at, created_by, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(identity_key) DO UPDATE SET
            scope = excluded.scope,
            created_at = excluded.created_at,
            expires_at = excluded.expires_at,
            created_by = excluded.created_by,
            reason = excluded.reason",
        &[
            ToSqlValue::Text(record.identity_key.as_str()),
            ToSqlValue::Text(record.scope.as_str()),
            ToSqlValue::Integer(record.created_at),
            ToSqlValue::optional_i64(record.expires_at),
            ToSqlValue::optional_text(record.created_by.as_deref()),
            ToSqlValue::optional_text(record.reason.as_deref()),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert event mute", err))?;

    Ok(())
}

/// Delete a persistent event mute by identity key.
fn delete_event_mute_backend(backend: &dyn StorageBackend, identity_key: &str) -> Result<bool> {
    let row = backend
        .query_row_typed(
            "DELETE FROM event_mutes WHERE identity_key = ?1 RETURNING 1",
            &[ToSqlValue::Text(identity_key)],
        )
        .map_err(|err| storage_backend_error("Failed to delete event mute", err))?;
    Ok(row.is_some())
}

/// Check if an identity key is muted (and not expired).
fn query_event_mute_backend(
    backend: &dyn StorageBackend,
    identity_key: &str,
    now_ms: i64,
) -> Result<bool> {
    row_exists_where(
        backend,
        "event_mutes",
        "identity_key = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
        &[ToSqlValue::Text(identity_key), ToSqlValue::Integer(now_ms)],
    )
    .map_err(|err| storage_backend_error("Mute query failed", err).into())
}

/// List all active (non-expired) mutes.
fn list_active_mutes_backend(
    backend: &dyn StorageBackend,
    now_ms: i64,
) -> Result<Vec<EventMuteRecord>> {
    let rows = backend
        .query_map_typed(
            "SELECT identity_key, scope, created_at, expires_at, created_by, reason
             FROM event_mutes
             WHERE expires_at IS NULL OR expires_at > ?1
             ORDER BY created_at DESC",
            &[ToSqlValue::Integer(now_ms)],
        )
        .map_err(|err| storage_backend_error("Mute list query failed", err))?;

    rows.iter()
        .map(|row| {
            let reader = RowReader::new(row);
            Ok(EventMuteRecord {
                identity_key: reader
                    .string(0)
                    .map_err(|err| storage_backend_error("Mute identity_key", err))?,
                scope: reader
                    .string(1)
                    .map_err(|err| storage_backend_error("Mute scope", err))?,
                created_at: reader
                    .i64(2)
                    .map_err(|err| storage_backend_error("Mute created_at", err))?,
                expires_at: reader
                    .optional_i64(3)
                    .map_err(|err| storage_backend_error("Mute expires_at", err))?,
                created_by: reader
                    .optional_string(4)
                    .map_err(|err| storage_backend_error("Mute created_by", err))?,
                reason: reader
                    .optional_string(5)
                    .map_err(|err| storage_backend_error("Mute reason", err))?,
            })
        })
        .collect()
}

/// Compute the event identity key for a stored event.
fn query_event_identity_key_backend(
    backend: &dyn StorageBackend,
    event_id: i64,
) -> Result<Option<String>> {
    let row = backend
        .query_row_cells(
            "SELECT e.rule_id, e.event_type, e.extracted, e.pane_id, p.pane_uuid
             FROM events e
             LEFT JOIN panes p ON p.pane_id = e.pane_id
             WHERE e.id = ?1",
            &[ToSqlValue::Integer(event_id)],
        )
        .map_err(|err| storage_backend_error("Identity query failed", err))?;

    if let Some(row) = row {
        let row = CellRowReader::new(&row);
        let rule_id = row
            .string(0)
            .map_err(|err| storage_backend_error("Failed to read rule_id", err))?;
        let event_type = row
            .string(1)
            .map_err(|err| storage_backend_error("Failed to read event_type", err))?;
        let extracted_str = row
            .optional_string(2)
            .map_err(|err| storage_backend_error("Failed to read extracted", err))?;
        let pane_id_i64 = row
            .i64(3)
            .map_err(|err| storage_backend_error("Failed to read pane_id", err))?;
        let pane_uuid = row
            .optional_string(4)
            .map_err(|err| storage_backend_error("Failed to read pane_uuid", err))?;
        // br-ft-4d6ic: route silent serde failure through observability counter.
        let extracted = parse_storage_json_col::<serde_json::Value>(
            extracted_str.as_deref(),
            "events",
            "extracted",
        )
        .unwrap_or(serde_json::Value::Null);

        let detection = crate::patterns::Detection {
            rule_id,
            agent_type: crate::patterns::AgentType::Unknown,
            event_type,
            severity: crate::patterns::Severity::Info,
            confidence: 0.0,
            extracted,
            matched_text: String::new(),
            span: (0, 0),
        };

        let pane_id = u64::try_from(pane_id_i64).unwrap_or(0);
        return Ok(Some(event_identity_key(
            &detection,
            pane_id,
            pane_uuid.as_deref(),
        )));
    }

    Ok(None)
}

/// Upsert pane record through the StorageBackend trait path.
fn upsert_pane_backend(backend: &dyn StorageBackend, pane: &PaneRecord) -> Result<()> {
    let pane_id_i64 = u64_to_i64(pane.pane_id, "pane_id")?;
    let window_id_i64 = pane
        .window_id
        .map(|v| u64_to_i64(v, "window_id"))
        .transpose()?;
    let tab_id_i64 = pane.tab_id.map(|v| u64_to_i64(v, "tab_id")).transpose()?;

    execute_typed(
        backend,
        "INSERT INTO panes (pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
         first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(pane_id) DO UPDATE SET
            pane_uuid = COALESCE(excluded.pane_uuid, panes.pane_uuid),
            domain = excluded.domain,
            window_id = excluded.window_id,
            tab_id = excluded.tab_id,
            title = excluded.title,
            cwd = excluded.cwd,
            tty_name = excluded.tty_name,
            last_seen_at = excluded.last_seen_at,
            observed = excluded.observed,
            ignore_reason = excluded.ignore_reason,
            last_decision_at = excluded.last_decision_at",
        &[
            ToSqlValue::Integer(pane_id_i64),
            ToSqlValue::optional_text(pane.pane_uuid.as_deref()),
            ToSqlValue::Text(pane.domain.as_str()),
            ToSqlValue::optional_i64(window_id_i64),
            ToSqlValue::optional_i64(tab_id_i64),
            ToSqlValue::optional_text(pane.title.as_deref()),
            ToSqlValue::optional_text(pane.cwd.as_deref()),
            ToSqlValue::optional_text(pane.tty_name.as_deref()),
            ToSqlValue::Integer(pane.first_seen_at),
            ToSqlValue::Integer(pane.last_seen_at),
            ToSqlValue::Integer(i64::from(pane.observed)),
            ToSqlValue::optional_text(pane.ignore_reason.as_deref()),
            ToSqlValue::optional_i64(pane.last_decision_at),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert pane", err))?;

    Ok(())
}

/// Upsert workflow execution through the StorageBackend trait path.
fn upsert_workflow_backend(backend: &dyn StorageBackend, workflow: &WorkflowRecord) -> Result<()> {
    let wait_condition_json = workflow.wait_condition.as_ref().map(|v| {
        serde_json::to_string(v).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "workflow wait_condition serialization failed");
            String::new()
        })
    });
    let context_json = workflow.context.as_ref().map(|v| {
        serde_json::to_string(v).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "workflow context serialization failed");
            String::new()
        })
    });
    let result_json = workflow.result.as_ref().map(|v| {
        serde_json::to_string(v).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "workflow result serialization failed");
            String::new()
        })
    });

    let pane_id_i64 = u64_to_i64(workflow.pane_id, "pane_id")?;
    let current_step_i64 = usize_to_i64(workflow.current_step, "current_step")?;

    execute_typed(
        backend,
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, trigger_event_id,
         current_step, status, wait_condition, context, result, error, started_at, updated_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET
            current_step = excluded.current_step,
            status = excluded.status,
            wait_condition = excluded.wait_condition,
            context = excluded.context,
            result = excluded.result,
            error = excluded.error,
            updated_at = excluded.updated_at,
            completed_at = excluded.completed_at",
        &[
            ToSqlValue::Text(workflow.id.as_str()),
            ToSqlValue::Text(workflow.workflow_name.as_str()),
            ToSqlValue::Integer(pane_id_i64),
            ToSqlValue::optional_i64(workflow.trigger_event_id),
            ToSqlValue::Integer(current_step_i64),
            ToSqlValue::Text(workflow.status.as_str()),
            ToSqlValue::optional_text(wait_condition_json.as_deref()),
            ToSqlValue::optional_text(context_json.as_deref()),
            ToSqlValue::optional_text(result_json.as_deref()),
            ToSqlValue::optional_text(workflow.error.as_deref()),
            ToSqlValue::Integer(workflow.started_at),
            ToSqlValue::Integer(workflow.updated_at),
            ToSqlValue::optional_i64(workflow.completed_at),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert workflow", err))?;

    Ok(())
}

/// Upsert workflow action plan (synchronous)
/// Upsert a workflow action plan (writer-thread, backend-trait path).
///
/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `upsert_action_plan_sync(&Connection, &WorkflowActionPlanRecord)`
/// direct-rusqlite helper. Routes the `INSERT ... ON CONFLICT DO UPDATE`
/// through the trait surface using `execute_typed`. Same shape as the
/// `upsert_action_undo_backend` (81589276c) and
/// `insert_prepared_plan_backend` (1c3e5e433) slices. Called from the
/// writer-thread dispatcher inside `with_writer_backend(...)`.
fn upsert_action_plan_backend(
    backend: &dyn StorageBackend,
    record: &WorkflowActionPlanRecord,
) -> Result<()> {
    execute_typed(
        backend,
        "INSERT INTO workflow_action_plans (workflow_id, plan_id, plan_hash, plan_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(workflow_id) DO UPDATE SET
            plan_id = excluded.plan_id,
            plan_hash = excluded.plan_hash,
            plan_json = excluded.plan_json,
            created_at = excluded.created_at",
        &[
            ToSqlValue::Text(record.workflow_id.as_str()),
            ToSqlValue::Text(record.plan_id.as_str()),
            ToSqlValue::Text(record.plan_hash.as_str()),
            ToSqlValue::Text(record.plan_json.as_str()),
            ToSqlValue::Integer(record.created_at),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert action plan", err))?;
    Ok(())
}

/// Insert workflow step log (synchronous)
#[allow(clippy::too_many_arguments)]
fn insert_step_log_backend(
    backend: &dyn StorageBackend,
    workflow_id: &str,
    audit_action_id: Option<i64>,
    step_index: usize,
    step_name: &str,
    step_id: Option<&str>,
    step_kind: Option<&str>,
    result_type: &str,
    result_data: Option<&str>,
    policy_summary: Option<&str>,
    verification_refs: Option<&str>,
    error_code: Option<&str>,
    started_at: i64,
    completed_at: i64,
) -> Result<()> {
    let duration_ms = completed_at.saturating_sub(started_at);
    let step_index_i64 = usize_to_i64(step_index, "step_index")?;

    execute_typed(
        backend,
        "INSERT INTO workflow_step_logs (workflow_id, audit_action_id, step_index, step_name, step_id,
         step_kind, result_type, result_data, policy_summary, verification_refs, error_code,
         started_at, completed_at, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        &[
            ToSqlValue::Text(workflow_id),
            ToSqlValue::optional_i64(audit_action_id),
            ToSqlValue::Integer(step_index_i64),
            ToSqlValue::Text(step_name),
            ToSqlValue::optional_text(step_id),
            ToSqlValue::optional_text(step_kind),
            ToSqlValue::Text(result_type),
            ToSqlValue::optional_text(result_data),
            ToSqlValue::optional_text(policy_summary),
            ToSqlValue::optional_text(verification_refs),
            ToSqlValue::optional_text(error_code),
            ToSqlValue::Integer(started_at),
            ToSqlValue::Integer(completed_at),
            ToSqlValue::Integer(duration_ms),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to insert step log", err))?;

    Ok(())
}

/// Upsert agent session through the StorageBackend trait path.
///
/// If the session has id == 0, creates a new session. Otherwise,
/// updates the existing session and preserves the legacy behavior of
/// returning the supplied id even when no row matched the update.
fn upsert_agent_session_backend(
    backend: &dyn StorageBackend,
    session: &AgentSessionRecord,
) -> Result<i64> {
    let pane_id_i64 = u64_to_i64(session.pane_id, "pane_id")?;
    let external_meta_json = session.external_meta.as_ref().and_then(|value| {
        serde_json::to_string(value)
            .inspect_err(
                |e| tracing::warn!(error = %e, "agent session external_meta serialization failed"),
            )
            .ok()
    });
    let estimated_cost_usd = match session.estimated_cost_usd {
        Some(value) => ToSqlValue::Real(value),
        None => ToSqlValue::Null,
    };

    if session.id == 0 {
        let row = backend
            .query_row_typed(
            "INSERT INTO agent_sessions (pane_id, agent_type, session_id, external_id, external_meta,
             started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
             cached_tokens, reasoning_tokens, model_name, estimated_cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             RETURNING id",
                &[
                    ToSqlValue::Integer(pane_id_i64),
                    ToSqlValue::Text(session.agent_type.as_str()),
                    ToSqlValue::optional_text(session.session_id.as_deref()),
                    ToSqlValue::optional_text(session.external_id.as_deref()),
                    ToSqlValue::optional_text(external_meta_json.as_deref()),
                    ToSqlValue::Integer(session.started_at),
                    ToSqlValue::optional_i64(session.ended_at),
                    ToSqlValue::optional_text(session.end_reason.as_deref()),
                    ToSqlValue::optional_i64(session.total_tokens),
                    ToSqlValue::optional_i64(session.input_tokens),
                    ToSqlValue::optional_i64(session.output_tokens),
                    ToSqlValue::optional_i64(session.cached_tokens),
                    ToSqlValue::optional_i64(session.reasoning_tokens),
                    ToSqlValue::optional_text(session.model_name.as_deref()),
                    estimated_cost_usd,
                ],
            )
            .map_err(|err| storage_backend_error("Failed to insert session", err))?
            .ok_or_else(|| StorageError::Database("session insert returned no id".to_string()))?;

        Ok(RowReader::new(&row)
            .i64(0)
            .map_err(|err| storage_backend_error("Failed to parse inserted session id", err))?)
    } else {
        execute_typed(
            backend,
            "UPDATE agent_sessions SET
             pane_id = ?1, agent_type = ?2, session_id = ?3, external_id = ?4, external_meta = ?5,
             started_at = ?6, ended_at = ?7, end_reason = ?8, total_tokens = ?9,
             input_tokens = ?10, output_tokens = ?11, cached_tokens = ?12,
             reasoning_tokens = ?13, model_name = ?14, estimated_cost_usd = ?15
             WHERE id = ?16",
            &[
                ToSqlValue::Integer(pane_id_i64),
                ToSqlValue::Text(session.agent_type.as_str()),
                ToSqlValue::optional_text(session.session_id.as_deref()),
                ToSqlValue::optional_text(session.external_id.as_deref()),
                ToSqlValue::optional_text(external_meta_json.as_deref()),
                ToSqlValue::Integer(session.started_at),
                ToSqlValue::optional_i64(session.ended_at),
                ToSqlValue::optional_text(session.end_reason.as_deref()),
                ToSqlValue::optional_i64(session.total_tokens),
                ToSqlValue::optional_i64(session.input_tokens),
                ToSqlValue::optional_i64(session.output_tokens),
                ToSqlValue::optional_i64(session.cached_tokens),
                ToSqlValue::optional_i64(session.reasoning_tokens),
                ToSqlValue::optional_text(session.model_name.as_deref()),
                estimated_cost_usd,
                ToSqlValue::Integer(session.id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to update session", err))?;

        Ok(session.id)
    }
}

/// ft-h90rh: insert a policy-denied audit row.
///
/// Assumes `reason` is already redacted — the caller pulls it from
/// `PolicyDecision` which the policy engine has already sanitised. We do
/// NOT re-redact here (unlike `record_audit_action_redacted`) because
/// `PolicyDeniedAuditRecord.reason` is a policy-produced decision
/// message, not pane-sourced free text.
///
/// br-ft-l1jgo writer-thread migration: routes through
/// `StorageBackend::query_row_typed` so both the async writer command
/// and the sync blocking MCP fallback stay on the trait surface.
fn record_policy_denial_audit_backend(
    backend: &dyn StorageBackend,
    record: &PolicyDeniedAuditRecord,
) -> Result<i64> {
    let ts_ms = if record.ts_ms == 0 {
        i64::try_from(now_ms()).unwrap_or(0)
    } else {
        record.ts_ms
    };
    let row = backend
        .query_row_typed(
            "INSERT INTO policy_denied_audit
         (ts_ms, agent_id, tool_name, intent_hash, reason, reason_code, rule_id, decision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
            &[
                ToSqlValue::Integer(ts_ms),
                ToSqlValue::optional_text(record.agent_id.as_deref()),
                ToSqlValue::Text(record.tool_name.as_str()),
                ToSqlValue::optional_text(record.intent_hash.as_deref()),
                ToSqlValue::Text(record.reason.as_str()),
                ToSqlValue::Text(record.reason_code.as_str()),
                ToSqlValue::optional_text(record.rule_id.as_deref()),
                ToSqlValue::Text(record.decision.as_str()),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to insert policy_denied_audit row", err))?
        .ok_or_else(|| {
            StorageError::Database("policy_denied_audit insert returned no id".to_string())
        })?;

    Ok(RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("policy_denied_audit insert id", err))?)
}

/// Open a short-lived read Connection for a `spawn_blocking` query path.
///
/// All `StorageHandle` reader helpers funnel through this. It applies the
/// standard 5s `busy_timeout` so a concurrent writer holding the WAL
/// write lock does not surface as an immediate `SQLITE_BUSY` to the
/// caller — SQLite retries internally for the configured window. The
/// `let _ =` discard on `busy_timeout` is intentional: failure to apply
/// the PRAGMA is non-fatal; the read may simply contend more aggressively.
///
/// Match the recipe in `record_policy_denial_audit_blocking` and the
/// PRAGMA recipe in `session_restore::open_conn`. Without this, every
/// MCP / robot / TUI read path could fail under modest writer load.
fn open_read_storage_conn(db_path: &str) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open read connection: {e}")))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Ok(conn)
}

fn storage_backend_error(context: &str, err: BackendError) -> StorageError {
    StorageError::Database(format!("{context}: {err}"))
}

/// br-ft-3twzm: pooled-backend helper that re-fixes ft-bhyxz.
///
/// Lends a pre-warmed `rusqlite::Connection` from the per-`db_path`
/// LIFO read pool, temporarily moves it into a `RusqliteBackend` so
/// the closure can call the typed `StorageBackend` trait methods
/// (`query_row_typed`, `query_map_typed`, etc. introduced by
/// br-ft-qgj81), then moves the `Connection` back into the
/// `PooledReadConn` whose `Drop` returns it to the pool.
///
/// This pattern preserves the connection-pool optimization the
/// br-ft-l1jgo migration accidentally bypassed by going through the
/// fresh `RusqliteBackend::open(db_path, &OpenConfig::default())`
/// path (which opened a new file handle + ran `PRAGMA journal_mode = WAL`
/// on every call).
///
/// The closure does its own per-call `BackendError` → `StorageError`
/// wrapping via `storage_backend_error(context, err)` so the helper
/// stays a thin lend wrapper that doesn't impose a single error
/// context on the whole closure body.
///
/// Current read migrations should prefer [`pooled_backend`]. This
/// concrete sibling is kept for future FTS5/PRAGMA/prepared-statement
/// loops that need `RusqliteBackend` specifically.
#[allow(dead_code)]
fn pooled_rusqlite_backend<F, R>(db_path: &str, f: F) -> Result<R>
where
    F: FnOnce(&RusqliteBackend) -> Result<R>,
{
    let pooled = PooledReadConn::acquire(db_path)?;
    pooled.with_borrowed_backend(f)
}

/// br-ft-l1jgo: trait-typed sibling for swap-ready read migrations.
///
/// Lends a pooled read connection as `&dyn StorageBackend` so migration
/// bodies can stay on the storage trait surface. Use this as the default
/// for ft-l1jgo.* read-path migrations; keep the concrete sibling for
/// FTS5 setup, PRAGMA mutation, and prepared-statement iterator loops.
#[doc(hidden)]
pub fn pooled_backend<F, R>(db_path: &str, f: F) -> Result<R>
where
    F: FnOnce(&dyn StorageBackend) -> Result<R>,
{
    let pooled = PooledReadConn::acquire(db_path)?;
    pooled.with_borrowed_backend(|backend| f(backend as &dyn StorageBackend))
}

// ─── Read-connection pool (ft-bhyxz) ──────────────────────────────────────
//
// Every `StorageHandle::*_with_cx` reader path used to call
// `open_read_storage_conn(db_path)` inside a `spawn_blocking_storage`
// closure. With ~78 call sites and a per-call `Connection::open` (file
// open + page cache warmup + 5s busy_timeout PRAGMA), a 200-agent fleet
// hammering `wa.search` / `wa.get_text` / web `/search` could rack up
// hundreds of SQLite-open syscalls per second.
//
// `PooledReadConn` is a small LIFO pool keyed by db_path:
//   - `acquire(db_path)` pops a pre-warmed Connection or opens fresh.
//   - On Drop, the Connection returns to the pool (capped at 8 per path)
//     instead of closing.
//   - Connection's existing schema/PRAGMA state survives the round-trip
//     (busy_timeout is a connection-scoped PRAGMA).
//
// `Deref<Target = Connection>` means call sites that previously held a
// `Connection` (e.g. `&conn` passed to functions taking `&Connection`,
// or method calls like `conn.prepare(...)`) work unchanged via Rust's
// auto-deref + deref-coercion rules.
//
// Pool is process-global (`OnceLock`-backed `Mutex<HashMap<...>>`) rather
// than per-StorageHandle because the spawn_blocking closures capture
// `db_path` by clone, not the `StorageHandle` itself. Process-global keeps
// the migration drop-in.

const READ_POOL_MAX_PER_PATH: usize = 8;

static READ_POOL: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Vec<Connection>>>,
> = std::sync::OnceLock::new();

fn read_pool() -> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<Connection>>> {
    READ_POOL.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

// ─── br-ft-rvt1z: PooledReadConn telemetry (ft-q4udk follow-up) ──────
//
// Pool acquire counters so regression tests + ops dashboards can
// verify that the pool is actually hitting (the br-ft-l1jgo migration
// regression silently bypassed it for 9 paths until ft-3twzm). Counts
// are process-global since the pool itself is process-global.
//
// `hits`    — `acquire` found a recycled connection in the LIFO.
// `misses`  — `acquire` fell through to `open_read_storage_conn`
//             (pool empty for this db_path or first-ever acquire).
// `returns` — `Drop` successfully placed a connection back in the
//             pool (autocommit + slot available + lock OK).
//
// Returns can lag hits+misses by one acquire because the count
// records *successful* return-to-pool, not just Drop. A connection
// that's discarded (transaction left open, pool full, lock poisoned)
// is NOT counted as a return.

static POOL_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static POOL_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static POOL_RETURNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// br-ft-ac4j0: pool-lock poison-recovery counter.
//
// Pre-fix `PooledReadConn::acquire` used `.expect("read connection
// pool mutex not poisoned")` on the read pool Mutex — a panic in
// any thread holding the pool lock cascaded to every subsequent
// database read. `PooledReadConn::Drop` already had silent
// fall-through on poison (drops connection, lets rusqlite close
// it cleanly) but no observability.
//
// Post-fix: both sites recover via `PoisonError::into_inner()` (or
// the silent drop in Drop's case) AND bump this counter so
// operators can detect pool degradation. Same observability defect
// family as ft-luav8 / ft-skec1 / ft-tpdl5 / ft-wzk10 / ft-4socw /
// ft-4pxzi / ft-as3w7 / ft-h2vyr / ft-iaxog / ft-zvhav.
static POOL_LOCK_POISONED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ============================================================================
// br-ft-lqj5g: workflow_executions row parse-drop observability
// ============================================================================

/// br-ft-lqj5g: cumulative count of workflow_executions row column
/// parse failures observed since process load. Pre-fix the reader
/// silently dropped malformed wait_condition / context / result
/// JSON columns to None — indistinguishable from "the column was
/// actually NULL". Workflow consumers reading these rows lost the
/// WHY of the workflow's wait/context/result with no diagnostic.
///
/// NULL columns are NOT counted (legitimate absence); only
/// column-present-but-fails-to-parse bumps. The structured
/// `tracing::warn` carries the column tag so operators can
/// discriminate which column drifted.
///
/// Same observability defect family as ft-zhnaw (in-memory
/// WorkflowDecision::parse counter) — this is the storage-layer
/// counterpart for DB-backed workflow rows.
static WORKFLOW_EXECUTION_ROW_PARSE_DROP_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// br-ft-lqj5g: cumulative count of workflow_executions column
/// parse failures.
#[must_use]
pub fn workflow_execution_row_parse_drop_count() -> u64 {
    WORKFLOW_EXECUTION_ROW_PARSE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// br-ft-lqj5g: test helper to reset the parse-drop counter.
#[cfg(test)]
pub fn reset_workflow_execution_row_parse_drop_count_for_test() {
    WORKFLOW_EXECUTION_ROW_PARSE_DROP_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// br-ft-lqj5g: parse a JSON column from a workflow_executions row.
/// `None` input (NULL column) returns `Ok(None)` with no counter
/// bump. A column that fails to parse bumps the counter, emits a
/// structured `tracing::warn` at target `ft.storage.workflow`
/// carrying the column tag, and returns `None` so the scalar
/// contract stays the same.
fn parse_workflow_execution_column<T: serde::de::DeserializeOwned>(
    value: Option<&str>,
    column: &'static str,
) -> Option<T> {
    let raw = value?;
    match serde_json::from_str(raw) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            WORKFLOW_EXECUTION_ROW_PARSE_DROP_COUNT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                target: "ft.storage.workflow",
                event = "workflow_execution_row_parse_drop",
                column = column,
                error = %err,
                "workflow_executions row column failed to parse; consumer will see None (br-ft-lqj5g)"
            );
            None
        }
    }
}

// ============================================================================
// br-ft-4d6ic: umbrella JSON-column parse-drop observability
// ============================================================================
//
// 7 silent serde drops in storage.rs (post ft-pewat sweep) read
// JSON-typed columns across 3 distinct (table, column) pairs:
//
//   - events.extracted (3 sites: 9035 + 14054 + 14219 + 14315 — 4
//     actually; pane_uuid lookup at 9035 also reads `extracted`)
//   - agent_sessions.external_meta (3 sites: 13903 + 13949 + 14001)
//
// Per-site counters would create 6+ separate observability surfaces.
// Operators don't need that resolution — table+column tags in the
// structured tracing::warn give per-site discrimination through
// logs, while a single umbrella counter answers the high-level
// question "is JSON-column parse fidelity silently degrading?".
// Same shape as ft-yygus, ft-k6uwb, ft-zhnaw, ft-94cdu, ft-lqj5g,
// ft-pewat, ft-l3u5k, ft-bn6qi.

static STORAGE_JSON_COL_PARSE_DROP_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// br-ft-4d6ic: cumulative count of JSON-typed column parse
/// failures across storage.rs query paths since process load.
/// NULL columns are NOT counted (legitimate absence); only
/// column-present-but-fails-to-parse bumps. Operators reading
/// this > 0 should investigate schema drift on the tagged
/// (table, column) pair.
#[must_use]
pub fn storage_json_col_parse_drop_count() -> u64 {
    STORAGE_JSON_COL_PARSE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// br-ft-4d6ic: test helper to reset the umbrella parse-drop counter.
#[cfg(test)]
pub fn reset_storage_json_col_parse_drop_count_for_test() {
    STORAGE_JSON_COL_PARSE_DROP_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// br-ft-4d6ic: parse a JSON-typed column from a storage row.
/// `None` input (NULL column) returns `None` with no counter
/// bump. A column that fails to parse bumps the umbrella counter,
/// emits a structured `tracing::warn` at target `ft.storage.json_col`
/// with the (table, column) tags + raw_len + error, and returns
/// `None` so the scalar contract stays the same.
fn parse_storage_json_col<T: serde::de::DeserializeOwned>(
    value: Option<&str>,
    table: &'static str,
    column: &'static str,
) -> Option<T> {
    let raw = value?;
    match serde_json::from_str(raw) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            STORAGE_JSON_COL_PARSE_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                target: "ft.storage.json_col",
                event = "json_col_parse_drop",
                table = table,
                column = column,
                raw_len = raw.len(),
                error = %err,
                "storage JSON-typed column failed to parse; consumer will see None (br-ft-4d6ic)"
            );
            None
        }
    }
}

// ============================================================================
// br-ft-pewat: pane_bookmark.tags parse-drop observability
// ============================================================================

/// br-ft-pewat: cumulative count of `pane_bookmarks.tags` column
/// parse failures observed since process load. The tags column
/// stores a JSON `Vec<String>`; a malformed value (schema drift,
/// manual DB edit, corrupted backup) silently turned into
/// `tags = None` pre-fix — indistinguishable from "no tags set".
/// Operators running tag-based filtering saw bookmarks "missing"
/// with no signal.
///
/// NULL column is NOT counted (legitimate "no tags set"); only
/// column-present-but-fails-to-parse bumps.
///
/// Same observability defect family as ft-yygus, ft-k6uwb,
/// ft-zhnaw, ft-94cdu, ft-lqj5g, ft-4ymqn, ft-l3u5k, ft-bn6qi,
/// ft-crpvd, ft-0n4nx.
static PANE_BOOKMARK_TAGS_PARSE_DROP_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// br-ft-pewat: cumulative count of pane_bookmark.tags parse failures.
#[must_use]
pub fn pane_bookmark_tags_parse_drop_count() -> u64 {
    PANE_BOOKMARK_TAGS_PARSE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// br-ft-pewat: test helper to reset the parse-drop counter.
#[cfg(test)]
pub fn reset_pane_bookmark_tags_parse_drop_count_for_test() {
    PANE_BOOKMARK_TAGS_PARSE_DROP_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// br-ft-pewat: parse the `pane_bookmarks.tags` JSON column. On
/// failure bumps the counter, emits a structured `tracing::warn`
/// at target `ft.storage.pane_bookmark` with bookmark_id,
/// pane_id, raw_len, and error, then returns `None` so the
/// scalar contract stays the same.
fn parse_pane_bookmark_tags(raw: &str, bookmark_id: i64, pane_id: u64) -> Option<Vec<String>> {
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(tags) => Some(tags),
        Err(err) => {
            PANE_BOOKMARK_TAGS_PARSE_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                target: "ft.storage.pane_bookmark",
                event = "pane_bookmark_tags_parse_drop",
                bookmark_id = bookmark_id,
                pane_id = pane_id,
                raw_len = raw.len(),
                error = %err,
                "pane_bookmarks.tags column failed to parse; consumer will see None (br-ft-pewat)"
            );
            None
        }
    }
}

#[cfg(test)]
mod pane_bookmark_tags_parse_drop_tests {
    use super::*;

    #[test]
    fn well_formed_tags_no_bump_ft_pewat() {
        reset_pane_bookmark_tags_parse_drop_count_for_test();
        let tags = parse_pane_bookmark_tags(r#"["urgent","review"]"#, 1, 7);
        assert_eq!(tags, Some(vec!["urgent".to_string(), "review".to_string()]));
        assert_eq!(pane_bookmark_tags_parse_drop_count(), 0);
    }

    #[test]
    fn empty_tags_array_no_bump_ft_pewat() {
        // Boundary: legitimate empty tag list.
        reset_pane_bookmark_tags_parse_drop_count_for_test();
        let tags = parse_pane_bookmark_tags("[]", 1, 7);
        assert_eq!(tags, Some(Vec::<String>::new()));
        assert_eq!(pane_bookmark_tags_parse_drop_count(), 0);
    }

    #[test]
    fn malformed_tags_bumps_counter_ft_pewat() {
        // br-ft-pewat: a malformed tags value bumps the counter.
        reset_pane_bookmark_tags_parse_drop_count_for_test();
        let tags = parse_pane_bookmark_tags("{not an array", 42, 100);
        assert!(tags.is_none());
        assert_eq!(pane_bookmark_tags_parse_drop_count(), 1);

        // Wrong shape (object, not array): also bumps.
        let _ = parse_pane_bookmark_tags(r#"{"k":"v"}"#, 43, 101);
        assert_eq!(pane_bookmark_tags_parse_drop_count(), 2);
    }
}

#[cfg(test)]
mod workflow_execution_row_parse_drop_tests {
    use super::*;

    #[test]
    fn null_column_does_not_bump_ft_lqj5g() {
        // NULL column is legitimate absence; the counter must NOT
        // bump.
        reset_workflow_execution_row_parse_drop_count_for_test();
        let result: Option<serde_json::Value> =
            parse_workflow_execution_column(None, "wait_condition");
        assert!(result.is_none());
        assert_eq!(workflow_execution_row_parse_drop_count(), 0);
    }

    #[test]
    fn well_formed_column_does_not_bump_ft_lqj5g() {
        reset_workflow_execution_row_parse_drop_count_for_test();
        let result: Option<serde_json::Value> =
            parse_workflow_execution_column(Some(r#"{"key":"value"}"#), "context");
        assert!(result.is_some());
        assert_eq!(workflow_execution_row_parse_drop_count(), 0);
    }

    #[test]
    fn malformed_column_bumps_counter_ft_lqj5g() {
        // br-ft-lqj5g: a malformed JSON column bumps the counter
        // exactly once. Multiple drops accumulate.
        reset_workflow_execution_row_parse_drop_count_for_test();
        let r1: Option<serde_json::Value> =
            parse_workflow_execution_column(Some("{not json"), "wait_condition");
        assert!(r1.is_none());
        assert_eq!(workflow_execution_row_parse_drop_count(), 1);

        let r2: Option<serde_json::Value> =
            parse_workflow_execution_column(Some("{also broken"), "context");
        assert!(r2.is_none());
        assert_eq!(workflow_execution_row_parse_drop_count(), 2);
    }
}

#[cfg(test)]
mod storage_json_col_parse_drop_tests {
    //! br-ft-4d6ic: tests pinning the umbrella JSON-column parse
    //! observability counter. The 7 silent serde drops in storage.rs
    //! query paths (events.extracted ×4, agent_sessions.external_meta
    //! ×3) all route through `parse_storage_json_col` which bumps a
    //! single shared counter and emits structured tracing::warn with
    //! per-site (table, column) tags.
    use super::*;

    /// br-ft-4d6ic: NULL column (None input) does not bump the
    /// counter. NULL is legitimate absence; only column-present-but-
    /// fails-to-parse counts as a drop.
    #[test]
    fn null_column_does_not_bump_ft_4d6ic() {
        reset_storage_json_col_parse_drop_count_for_test();
        let result: Option<serde_json::Value> = parse_storage_json_col(None, "events", "extracted");
        assert!(result.is_none());
        assert_eq!(storage_json_col_parse_drop_count(), 0);
    }

    /// br-ft-4d6ic: well-formed JSON returns Some + does not bump.
    #[test]
    fn well_formed_column_does_not_bump_ft_4d6ic() {
        reset_storage_json_col_parse_drop_count_for_test();
        let result: Option<serde_json::Value> =
            parse_storage_json_col(Some(r#"{"key":"value"}"#), "events", "extracted");
        assert!(result.is_some());
        assert_eq!(storage_json_col_parse_drop_count(), 0);
    }

    /// br-ft-4d6ic: malformed JSON bumps the umbrella counter once.
    /// Pre-fix this drop was a silent `.ok()` — operators had ZERO
    /// signal that JSON-column parse fidelity was degrading.
    #[test]
    fn malformed_column_bumps_counter_ft_4d6ic() {
        reset_storage_json_col_parse_drop_count_for_test();
        let result: Option<serde_json::Value> =
            parse_storage_json_col(Some("{not json"), "events", "extracted");
        assert!(result.is_none());
        assert_eq!(storage_json_col_parse_drop_count(), 1);
    }

    /// br-ft-4d6ic: drops from DIFFERENT (table, column) pairs all
    /// accumulate into the SAME umbrella counter. Operators read
    /// the counter for the high-level question; the per-site
    /// discrimination comes through the structured tracing::warn's
    /// table+column tags. This test pins both the accumulation
    /// behavior AND that table/column don't accidentally split into
    /// per-pair counters.
    #[test]
    fn drops_accumulate_across_distinct_tables_ft_4d6ic() {
        reset_storage_json_col_parse_drop_count_for_test();

        let _: Option<serde_json::Value> =
            parse_storage_json_col(Some("{ malformed"), "events", "extracted");
        assert_eq!(storage_json_col_parse_drop_count(), 1);

        let _: Option<serde_json::Value> =
            parse_storage_json_col(Some("[malformed"), "agent_sessions", "external_meta");
        assert_eq!(
            storage_json_col_parse_drop_count(),
            2,
            "ft-4d6ic: drops from a different (table, column) pair must accumulate \
             into the same umbrella counter"
        );

        // Drop from the same pair again — counter advances by 1.
        let _: Option<serde_json::Value> =
            parse_storage_json_col(Some("definitely not json"), "events", "extracted");
        assert_eq!(storage_json_col_parse_drop_count(), 3);
    }

    /// br-ft-4d6ic: counter independence — the umbrella counter must
    /// NOT spill into the sibling per-table counters
    /// (PANE_BOOKMARK_TAGS_PARSE_DROP_COUNT,
    /// WORKFLOW_EXECUTION_ROW_PARSE_DROP_COUNT) and vice versa.
    /// Pins the boundary between the umbrella and the dedicated
    /// per-table counters so a future refactor that consolidates
    /// can't silently merge classes operators rely on for
    /// discrimination.
    #[test]
    fn umbrella_counter_independent_of_per_table_counters_ft_4d6ic() {
        reset_storage_json_col_parse_drop_count_for_test();
        reset_pane_bookmark_tags_parse_drop_count_for_test();
        reset_workflow_execution_row_parse_drop_count_for_test();

        // Umbrella bump only.
        let _: Option<serde_json::Value> =
            parse_storage_json_col(Some("{not json"), "events", "extracted");
        assert_eq!(storage_json_col_parse_drop_count(), 1);
        assert_eq!(
            pane_bookmark_tags_parse_drop_count(),
            0,
            "ft-4d6ic: umbrella bump must NOT spill into pane_bookmark counter"
        );
        assert_eq!(
            workflow_execution_row_parse_drop_count(),
            0,
            "ft-4d6ic: umbrella bump must NOT spill into workflow_execution counter"
        );

        // Pane-bookmark bump only.
        let _ = parse_pane_bookmark_tags("{not array", 1, 2);
        assert_eq!(pane_bookmark_tags_parse_drop_count(), 1);
        assert_eq!(
            storage_json_col_parse_drop_count(),
            1,
            "ft-4d6ic: pane_bookmark bump must NOT spill into umbrella counter"
        );
    }
}

/// br-ft-rvt1z: snapshot of the per-process `PooledReadConn` pool
/// counters. Returned by [`pool_telemetry_snapshot`] so regression
/// tests can assert hit-rate invariants and ops dashboards can
/// surface pool effectiveness over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTelemetrySnapshot {
    pub hits: u64,
    pub misses: u64,
    pub returns: u64,
    /// br-ft-ac4j0: count of read-pool Mutex-poison recoveries since
    /// process load. Non-zero values mean a prior thread panicked
    /// while holding the pool lock; the storage layer recovered
    /// (acquire path) or silently dropped the connection (Drop path)
    /// instead of cascading. Operators monitor this for pool
    /// degradation.
    pub pool_lock_poisoned: u64,
}

impl PoolTelemetrySnapshot {
    /// Hit rate as a fraction of total acquires. Returns 0.0 when no
    /// acquires have happened yet (avoids 0/0 NaN).
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        self.hits as f64 / total as f64
    }

    /// Total acquire calls (hits + misses).
    #[must_use]
    pub fn total_acquires(&self) -> u64 {
        self.hits + self.misses
    }
}

/// br-ft-rvt1z: snapshot the process-global pool counters.
///
/// Tests and ops use this to verify the read pool is actually hitting; the
/// br-ft-l1jgo migration regression silently bypassed it for 9 paths until
/// ft-3twzm.
#[must_use]
pub fn pool_telemetry_snapshot() -> PoolTelemetrySnapshot {
    use std::sync::atomic::Ordering;
    PoolTelemetrySnapshot {
        hits: POOL_HITS.load(Ordering::Relaxed),
        misses: POOL_MISSES.load(Ordering::Relaxed),
        returns: POOL_RETURNS.load(Ordering::Relaxed),
        // br-ft-ac4j0: surface pool-lock poison recoveries.
        pool_lock_poisoned: POOL_LOCK_POISONED.load(Ordering::Relaxed),
    }
}

/// br-ft-ac4j0: test-only reset of the pool-lock-poisoned counter.
#[cfg(test)]
pub(crate) fn reset_pool_lock_poisoned_count_for_test() {
    POOL_LOCK_POISONED.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Pooled read connection (ft-bhyxz). On Drop returns the Connection to
/// the per-`db_path` LIFO pool (capped at 8) instead of closing it.
///
/// Use `PooledReadConn::acquire(db_path)` in place of
/// `open_read_storage_conn(db_path)` inside `spawn_blocking_storage`
/// closures. Method calls + `&conn` passing both work unchanged via
/// `Deref<Target = Connection>` + auto-deref coercion.
pub(crate) struct PooledReadConn {
    conn: Option<Connection>,
    db_path: String,
}

impl PooledReadConn {
    pub(crate) fn acquire(db_path: &str) -> Result<Self> {
        let timer = SwarmCapacityStageTimer::start(SwarmCapacityStage::StorageReadPool, 0);
        let result = (|| {
            use std::sync::atomic::Ordering;
            let recycled = {
                // br-ft-ac4j0: recover from poison instead of cascading.
                // Pre-fix used .expect — a panic in any thread holding
                // the pool Mutex turned every subsequent database read
                // into a re-panic. Post-fix bumps the observability
                // counter (visible via PoolTelemetrySnapshot.pool_lock_poisoned)
                // and continues with the recovered HashMap. The .get_mut+pop
                // pattern below is safe under recovery — if the inner Vec
                // is in a transient state, get_mut returns None or the pop
                // is a no-op; worst case a stale entry survives until the
                // next return.
                let mut pool = read_pool().lock().unwrap_or_else(|poison| {
                    POOL_LOCK_POISONED.fetch_add(1, Ordering::Relaxed);
                    poison.into_inner()
                });
                pool.get_mut(db_path).and_then(|v| v.pop())
            };
            let conn = match recycled {
                Some(c) => {
                    // br-ft-rvt1z: pool hit — recycled an existing
                    // pre-warmed connection.
                    POOL_HITS.fetch_add(1, Ordering::Relaxed);
                    c
                }
                None => {
                    // br-ft-rvt1z: pool miss — first acquire for this
                    // db_path or pool drained. Counter bumps BEFORE the
                    // open call so a failed open still counts as a miss
                    // attempt (the test invariant is hit-rate, not
                    // success-rate).
                    POOL_MISSES.fetch_add(1, Ordering::Relaxed);
                    open_read_storage_conn(db_path)?
                }
            };
            Ok(Self {
                conn: Some(conn),
                db_path: db_path.to_string(),
            })
        })();
        timer.finish_result(&result);
        result
    }

    /// br-ft-3twzm: lend the pooled `Connection` as a
    /// `RusqliteBackend` to the closure. Used by the
    /// `pooled_rusqlite_backend` helper so storage.rs's
    /// br-ft-l1jgo migration sites can call typed
    /// `StorageBackend` trait methods without bypassing the
    /// connection pool.
    ///
    /// The dance: `PooledReadConn` owns the `Connection` in an
    /// `Option`. We `take()` it out, hand it to
    /// `RusqliteBackend::new`, run the closure, then move the
    /// `Connection` back via `RusqliteBackend::into_connection`.
    /// `Self`'s `Drop` then returns the `Connection` to the
    /// pool's LIFO.
    ///
    /// Closure-panic semantics: if `f` panics, `self` drops
    /// while `self.conn` is `None`. The Drop impl handles this
    /// (early return), so the pool slot for this `db_path`
    /// stays consistent — the panicked connection is discarded
    /// (since `RusqliteBackend` owns it through the panic and
    /// drops it via its own `Mutex<Connection>` Drop), preserving
    /// the existing post-panic invariant.
    pub(crate) fn with_borrowed_backend<F, R>(mut self, f: F) -> R
    where
        F: FnOnce(&RusqliteBackend) -> R,
    {
        let conn = self.conn.take().expect("connection present until Drop");
        let backend = RusqliteBackend::new(conn);
        let result = f(&backend);
        self.conn = Some(backend.into_connection());
        // self drops here, returning the connection to the
        // per-db_path pool LIFO.
        result
    }
}

impl std::ops::Deref for PooledReadConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("connection still present until Drop")
    }
}

impl Drop for PooledReadConn {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if let Some(conn) = self.conn.take() {
            // If the closure panicked or returned mid-transaction, the
            // Connection has an open transaction. Returning it to the pool
            // would leak that transaction state to the next consumer.
            // Discard the connection in that case; rusqlite's Drop closes it
            // cleanly (which also rolls back the open transaction).
            if !conn.is_autocommit() {
                drop(conn);
                return;
            }
            // br-ft-ac4j0: pre-fix this site silently dropped the
            // connection on poison without observability. The drop is
            // correct (rusqlite Drop closes the connection cleanly),
            // but operators had no signal that the pool was degraded.
            // Post-fix the silent drop is preserved BUT the poison
            // event bumps the pool_lock_poisoned counter so it's
            // visible via PoolTelemetrySnapshot.
            match read_pool().lock() {
                Ok(mut pool) => {
                    let entry = pool.entry(self.db_path.clone()).or_default();
                    if entry.len() < READ_POOL_MAX_PER_PATH {
                        entry.push(conn);
                        // br-ft-rvt1z: counted only on successful return.
                        POOL_RETURNS.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                }
                Err(_) => {
                    POOL_LOCK_POISONED.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Pool full (or lock poisoned) — let conn drop, which closes it.
            drop(conn);
        }
    }
}

/// ft-rsqap: sync-path convenience for writing a policy denial audit row
/// without going through the `StorageHandle` writer thread.
///
/// Opens a short-lived `Connection`, sets a 5s busy timeout so a contended
/// WAL doesn't drop the audit silently, runs one INSERT, closes. Intended
/// for callers that live in a synchronous context (e.g.
/// `mcp_authorize_mcp_mutation`) and can't easily await the async
/// `StorageHandle::record_policy_denial_audit` path.
///
/// Best-effort observability: callers should `tracing::warn!` a returned
/// `Err` and continue. A failed denial-audit write must never block the
/// caller's policy-denied response to the client.
pub fn record_policy_denial_audit_blocking(
    db_path: &Path,
    record: &PolicyDeniedAuditRecord,
) -> Result<i64> {
    let conn = Connection::open(db_path).map_err(|e| {
        StorageError::Database(format!(
            "open {} for policy_denied_audit: {e}",
            db_path.display()
        ))
    })?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| {
            StorageError::Database(format!("set busy_timeout for policy_denied_audit: {e}"))
        })?;
    let backend = RusqliteBackend::new(conn);
    record_policy_denial_audit_backend(&backend, record)
}

/// Record an audit action through the writer-thread backend bridge.
fn record_audit_action_backend(
    backend: &dyn StorageBackend,
    action: &AuditActionRecord,
) -> Result<i64> {
    let pane_id_i64 = action
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;
    let ts = if action.ts == 0 { now_ms() } else { action.ts };

    let row = backend
        .query_row_typed(
        "INSERT INTO audit_actions (ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         RETURNING id",
        &[
            ToSqlValue::Integer(ts),
            ToSqlValue::Text(action.actor_kind.as_str()),
            ToSqlValue::optional_text(action.actor_id.as_deref()),
            ToSqlValue::optional_text(action.correlation_id.as_deref()),
            ToSqlValue::optional_i64(pane_id_i64),
            ToSqlValue::optional_text(action.domain.as_deref()),
            ToSqlValue::Text(action.action_kind.as_str()),
            ToSqlValue::Text(action.policy_decision.as_str()),
            ToSqlValue::optional_text(action.decision_reason.as_deref()),
            ToSqlValue::optional_text(action.rule_id.as_deref()),
            ToSqlValue::optional_text(action.input_summary.as_deref()),
            ToSqlValue::optional_text(action.verification_summary.as_deref()),
            ToSqlValue::optional_text(action.decision_context.as_deref()),
            ToSqlValue::Text(action.result.as_str()),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to insert audit action", err))?
    .ok_or_else(|| {
        StorageError::Database("insert audit action returned no id".to_string())
    })?;

    RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Audit action id", err).into())
}

/// Upsert an action_undo record (writer-thread, backend-trait path).
///
/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `upsert_action_undo_sync(&Connection, &ActionUndoRecord)`
/// direct-rusqlite helper. Routes the `INSERT ... ON CONFLICT DO UPDATE`
/// through the trait surface using `execute_typed`. Same shape as the
/// `purge_audit_actions_backend` and `mark_action_undone_backend`
/// helpers; called from the writer-thread dispatcher inside
/// `with_writer_backend(...)`.
fn upsert_action_undo_backend(
    backend: &dyn StorageBackend,
    record: &ActionUndoRecord,
) -> Result<()> {
    let undoable_i64 = i64::from(record.undoable);
    let undo_hint = match record.undo_hint.as_deref() {
        Some(s) => ToSqlValue::Text(s),
        None => ToSqlValue::Null,
    };
    let undo_payload = match record.undo_payload.as_deref() {
        Some(s) => ToSqlValue::Text(s),
        None => ToSqlValue::Null,
    };
    let undone_at = match record.undone_at {
        Some(v) => ToSqlValue::Integer(v),
        None => ToSqlValue::Null,
    };
    let undone_by = match record.undone_by.as_deref() {
        Some(s) => ToSqlValue::Text(s),
        None => ToSqlValue::Null,
    };
    execute_typed(
        backend,
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(audit_action_id) DO UPDATE SET
            undoable = excluded.undoable,
            undo_strategy = excluded.undo_strategy,
            undo_hint = excluded.undo_hint,
            undo_payload = excluded.undo_payload,
            undone_at = excluded.undone_at,
            undone_by = excluded.undone_by",
        &[
            ToSqlValue::Integer(record.audit_action_id),
            ToSqlValue::Integer(undoable_i64),
            ToSqlValue::Text(&record.undo_strategy),
            undo_hint,
            undo_payload,
            undone_at,
            undone_by,
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert action_undo", err))?;
    Ok(())
}

fn action_undo_from_backend_cells(row: &[SqlCell]) -> Result<ActionUndoRecord> {
    let row = CellRowReader::new(row);
    Ok(ActionUndoRecord {
        audit_action_id: row
            .i64(0)
            .map_err(|err| storage_backend_error("action undo audit_action_id", err))?,
        undoable: row
            .bool(1)
            .map_err(|err| storage_backend_error("action undo undoable", err))?,
        undo_strategy: row
            .string(2)
            .map_err(|err| storage_backend_error("action undo undo_strategy", err))?,
        undo_hint: row
            .optional_string(3)
            .map_err(|err| storage_backend_error("action undo undo_hint", err))?,
        undo_payload: row
            .optional_string(4)
            .map_err(|err| storage_backend_error("action undo undo_payload", err))?,
        undone_at: row
            .optional_i64(5)
            .map_err(|err| storage_backend_error("action undo undone_at", err))?,
        undone_by: row
            .optional_string(6)
            .map_err(|err| storage_backend_error("action undo undone_by", err))?,
    })
}

fn query_action_undo_backend(
    backend: &dyn StorageBackend,
    audit_action_id: i64,
) -> Result<Option<ActionUndoRecord>> {
    let row = backend
        .query_row_cells(
            "SELECT audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by
         FROM action_undo WHERE audit_action_id = ?1",
            &[ToSqlValue::Integer(audit_action_id)],
        )
        .map_err(|err| storage_backend_error("Failed to query action_undo", err))?;
    row.as_deref()
        .map(action_undo_from_backend_cells)
        .transpose()
}

fn mark_action_undone_backend(
    backend: &dyn StorageBackend,
    audit_action_id: i64,
    undone_at: i64,
    undone_by: &str,
) -> Result<bool> {
    let changed = backend
        .query_row_typed(
            "UPDATE action_undo
             SET undone_at = ?1, undone_by = ?2
             WHERE audit_action_id = ?3 AND undoable = 1 AND undone_at IS NULL
             RETURNING 1",
            &[
                ToSqlValue::Integer(undone_at),
                ToSqlValue::Text(undone_by),
                ToSqlValue::Integer(audit_action_id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to mark action as undone", err))?;
    Ok(changed.is_some())
}

/// Purge audit actions before a cutoff timestamp (synchronous)
/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `purge_audit_actions_sync(&Connection, i64)` direct-rusqlite
/// helper. Routes the DELETE through the trait surface using
/// `RETURNING id` + `query_map_typed` so the affected-row count
/// is recovered without a separate `SELECT changes()` call —
/// matches the pattern used by other backend-side delete helpers
/// (e.g. `delete_saved_search_backend` at line ~9711). Called
/// from the writer-thread dispatcher inside
/// `with_writer_backend(...)`.
///
/// Returns the number of audit_actions rows deleted (rows whose
/// `ts < before_ts`).
fn purge_audit_actions_backend(backend: &dyn StorageBackend, before_ts: i64) -> Result<usize> {
    let returned = backend
        .query_map_typed(
            "DELETE FROM audit_actions WHERE ts < ?1 RETURNING id",
            &[ToSqlValue::Integer(before_ts)],
        )
        .map_err(|err| storage_backend_error("Failed to purge audit actions", err))?;
    Ok(returned.len())
}

fn record_maintenance_backend(
    backend: &dyn StorageBackend,
    record: &MaintenanceRecord,
) -> Result<i64> {
    let ts = if record.timestamp == 0 {
        now_ms()
    } else {
        record.timestamp
    };

    let row = backend
        .query_row_typed(
            "INSERT INTO maintenance_log (event_type, message, metadata, timestamp)
             VALUES (?1, ?2, ?3, ?4)
             RETURNING id",
            &[
                ToSqlValue::Text(&record.event_type),
                ToSqlValue::optional_text(record.message.as_deref()),
                ToSqlValue::optional_text(record.metadata.as_deref()),
                ToSqlValue::Integer(ts),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to record maintenance", err))?
        .ok_or_else(|| {
            StorageError::Database("record maintenance insert returned no id".to_string())
        })?;

    RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Maintenance id", err).into())
}

fn record_secret_scan_report_backend(
    backend: &dyn StorageBackend,
    record: &SecretScanReportRecord,
) -> Result<i64> {
    let created_at = if record.created_at == 0 {
        now_ms()
    } else {
        record.created_at
    };

    let row = backend
        .query_row_typed(
            "INSERT INTO secret_scan_reports (scope_hash, scope_json, report_version, \
             last_segment_id, report_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             RETURNING id",
            &[
                ToSqlValue::Text(&record.scope_hash),
                ToSqlValue::Text(&record.scope_json),
                ToSqlValue::Integer(record.report_version),
                ToSqlValue::optional_i64(record.last_segment_id),
                ToSqlValue::Text(&record.report_json),
                ToSqlValue::Integer(created_at),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to record secret scan report", err))?
        .ok_or_else(|| {
            StorageError::Database("record secret scan report insert returned no id".to_string())
        })?;

    RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Secret scan report id", err).into())
}

fn insert_saved_search_backend(
    backend: &dyn StorageBackend,
    record: &SavedSearchRecord,
) -> Result<()> {
    let enabled = i64::from(record.enabled);
    let limit = if record.limit <= 0 {
        SAVED_SEARCH_DEFAULT_LIMIT
    } else {
        record.limit
    };

    execute_typed(
        backend,
        "INSERT INTO saved_searches (
            id, name, query, pane_id, \"limit\", since_mode, since_ms,
            schedule_interval_ms, enabled, last_run_at, last_result_count, last_error,
            created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        &[
            ToSqlValue::Text(&record.id),
            ToSqlValue::Text(&record.name),
            ToSqlValue::Text(&record.query),
            ToSqlValue::optional_i64(record.pane_id.map(|v| v as i64)),
            ToSqlValue::Integer(limit),
            ToSqlValue::Text(&record.since_mode),
            ToSqlValue::optional_i64(record.since_ms),
            ToSqlValue::optional_i64(record.schedule_interval_ms),
            ToSqlValue::Integer(enabled),
            ToSqlValue::optional_i64(record.last_run_at),
            ToSqlValue::optional_i64(record.last_result_count),
            ToSqlValue::optional_text(record.last_error.as_deref()),
            ToSqlValue::Integer(record.created_at),
            ToSqlValue::Integer(record.updated_at),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to insert saved search", err))?;

    Ok(())
}

fn update_saved_search_run_backend(
    backend: &dyn StorageBackend,
    id: &str,
    last_run_at: i64,
    last_result_count: Option<i64>,
    last_error: Option<&str>,
) -> Result<()> {
    let updated = backend
        .query_row_typed(
            "UPDATE saved_searches
             SET last_run_at = ?1,
                 last_result_count = ?2,
                 last_error = ?3,
                 updated_at = ?4
             WHERE id = ?5
             RETURNING 1",
            &[
                ToSqlValue::Integer(last_run_at),
                ToSqlValue::optional_i64(last_result_count),
                ToSqlValue::optional_text(last_error),
                ToSqlValue::Integer(now_ms()),
                ToSqlValue::Text(id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to update saved search", err))?;

    if updated.is_none() {
        return Err(StorageError::NotFound(format!("Saved search not found: {id}")).into());
    }
    Ok(())
}

fn update_saved_search_schedule_backend(
    backend: &dyn StorageBackend,
    id: &str,
    enabled: bool,
    schedule_interval_ms: Option<i64>,
) -> Result<()> {
    let enabled_i64 = i64::from(enabled);
    let updated = backend
        .query_row_typed(
            "UPDATE saved_searches
             SET enabled = ?1,
                 schedule_interval_ms = ?2,
                 updated_at = ?3
             WHERE id = ?4
             RETURNING 1",
            &[
                ToSqlValue::Integer(enabled_i64),
                ToSqlValue::optional_i64(schedule_interval_ms),
                ToSqlValue::Integer(now_ms()),
                ToSqlValue::Text(id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to update saved search", err))?;

    if updated.is_none() {
        return Err(StorageError::NotFound(format!("Saved search not found: {id}")).into());
    }
    Ok(())
}

fn delete_saved_search_backend(backend: &dyn StorageBackend, name: &str) -> Result<usize> {
    let deleted = backend
        .query_row_typed(
            "DELETE FROM saved_searches WHERE name = ?1 RETURNING 1",
            &[ToSqlValue::Text(name)],
        )
        .map_err(|err| storage_backend_error("Failed to delete saved search", err))?;
    Ok(usize::from(deleted.is_some()))
}

fn backend_i64_to_u64(value: i64, label: &str) -> std::result::Result<u64, BackendError> {
    u64::try_from(value)
        .map_err(|_| BackendError::Query(format!("{label} value {value} is out of u64 range")))
}

fn backend_i64_to_i32(value: i64, label: &str) -> std::result::Result<i32, BackendError> {
    i32::try_from(value)
        .map_err(|_| BackendError::Query(format!("{label} value {value} is out of i32 range")))
}

fn backend_i64_to_bool(value: i64, label: &str) -> std::result::Result<bool, BackendError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(BackendError::Query(format!(
            "{label} value {value} must be 0 or 1"
        ))),
    }
}

fn saved_search_from_backend_row(
    row: &[String],
) -> std::result::Result<SavedSearchRecord, BackendError> {
    let reader = RowReader::new(row);
    let pane_id = reader
        .optional_i64(3)?
        .map(|v| backend_i64_to_u64(v, "saved_searches.pane_id"))
        .transpose()?;
    let enabled = backend_i64_to_bool(reader.i64(8)?, "saved_searches.enabled")?;

    Ok(SavedSearchRecord {
        id: reader.string(0)?,
        name: reader.string(1)?,
        query: reader.string(2)?,
        pane_id,
        limit: reader.i64(4)?,
        since_mode: reader.string(5)?,
        since_ms: reader.optional_i64(6)?,
        schedule_interval_ms: reader.optional_i64(7)?,
        enabled,
        last_run_at: reader.optional_i64(9)?,
        last_result_count: reader.optional_i64(10)?,
        last_error: reader.optional_string(11)?,
        created_at: reader.i64(12)?,
        updated_at: reader.i64(13)?,
    })
}

fn query_saved_search_by_name_backend(
    backend: &dyn StorageBackend,
    name: &str,
) -> Result<Option<SavedSearchRecord>> {
    let row = backend
        .query_row_typed(
            "SELECT id, name, query, pane_id, \"limit\", since_mode, since_ms, schedule_interval_ms,
                    enabled, last_run_at, last_result_count, last_error, created_at, updated_at
             FROM saved_searches
             WHERE name = ?1",
            &[ToSqlValue::Text(name)],
        )
        .map_err(|err| storage_backend_error("Query saved search", err))?;

    row.as_deref()
        .map(saved_search_from_backend_row)
        .transpose()
        .map_err(|err| storage_backend_error("Decode saved search", err).into())
}

fn list_saved_searches_backend(backend: &dyn StorageBackend) -> Result<Vec<SavedSearchRecord>> {
    let rows = backend
        .query_map_typed(
            "SELECT id, name, query, pane_id, \"limit\", since_mode, since_ms, schedule_interval_ms,
                    enabled, last_run_at, last_result_count, last_error, created_at, updated_at
             FROM saved_searches
             ORDER BY name ASC",
            &[],
        )
        .map_err(|err| storage_backend_error("List saved searches", err))?;

    rows.iter()
        .map(|row| {
            saved_search_from_backend_row(row)
                .map_err(|err| storage_backend_error("Decode saved search", err).into())
        })
        .collect()
}

// =============================================================================
// Pane Bookmarks
// =============================================================================

fn insert_pane_bookmark_backend(
    backend: &dyn StorageBackend,
    record: &PaneBookmarkRecord,
) -> Result<i64> {
    let tags_json = record
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string()));
    let pane_id_i64 = u64_to_i64(record.pane_id, "pane_bookmarks.pane_id")?;

    let row = backend
        .query_row_typed(
            "INSERT INTO pane_bookmarks (pane_id, alias, tags, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         RETURNING id",
            &[
                ToSqlValue::Integer(pane_id_i64),
                ToSqlValue::Text(&record.alias),
                ToSqlValue::optional_text(tags_json.as_deref()),
                ToSqlValue::optional_text(record.description.as_deref()),
                ToSqlValue::Integer(record.created_at),
                ToSqlValue::Integer(record.updated_at),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to insert pane bookmark", err))?
        .ok_or_else(|| StorageError::Database("Pane bookmark insert returned no id".to_string()))?;

    Ok(RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Pane bookmark insert id", err))?)
}

fn delete_pane_bookmark_backend(backend: &dyn StorageBackend, alias: &str) -> Result<bool> {
    let deleted = backend
        .query_row_typed(
            "DELETE FROM pane_bookmarks WHERE alias = ?1 RETURNING 1",
            &[ToSqlValue::Text(alias)],
        )
        .map_err(|err| storage_backend_error("Failed to delete pane bookmark", err))?;
    Ok(deleted.is_some())
}

/// br-ft-dngp2: collapse the agent_profiles sync layer's typed
/// error into the top-level [`crate::error::Error`] expected by
/// the writer-loop response channel. The slice-1 module
/// distinguishes `Sqlite` / `Invalid` / `Decode` for operator
/// diagnosis; the async surface preserves that detail in the
/// `Database` variant's message so callers logging the response
/// see which arm fired.
fn agent_profile_sql_to_error(
    err: agent_profiles_sql::AgentProfileSqlError,
) -> crate::error::Error {
    crate::error::Error::Storage(StorageError::Database(format!("agent_profiles: {err}")))
}

/// br-ft-l1jgo writer-thread migration: insert agent profiles through
/// [`StorageBackend`] while preserving the existing
/// [`crate::agent_profiles::AgentProfile::validate`] preflight.
fn insert_agent_profile_backend(
    backend: &dyn StorageBackend,
    profile: &crate::agent_profiles::AgentProfile,
) -> Result<String> {
    profile.validate().map_err(|err| {
        agent_profile_sql_to_error(agent_profiles_sql::AgentProfileSqlError::Invalid(err))
    })?;
    let tags_json = serde_json::to_string(&profile.tags).expect("tags serialize");
    let env_json = serde_json::to_string(&profile.env).expect("env serialize");
    let metadata_json = serde_json::to_string(&profile.metadata).expect("metadata serialize");
    let row = backend
        .query_row_typed(
            "INSERT INTO agent_profiles
             (name, role, tags, shell, command, env, metadata, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             RETURNING name",
            &[
                ToSqlValue::Text(profile.name.as_str()),
                ToSqlValue::Text(profile.role.as_str()),
                ToSqlValue::Text(tags_json.as_str()),
                ToSqlValue::Text(profile.shell.as_str()),
                ToSqlValue::optional_text(profile.command.as_deref()),
                ToSqlValue::Text(env_json.as_str()),
                ToSqlValue::Text(metadata_json.as_str()),
                ToSqlValue::Integer(profile.created_at_ms),
                ToSqlValue::Integer(profile.updated_at_ms),
            ],
        )
        .map_err(|err| storage_backend_error("agent_profiles insert", err))?
        .ok_or_else(|| {
            StorageError::Database("agent_profiles insert returned no name".to_string())
        })?;
    RowReader::new(&row)
        .string(0)
        .map_err(|err| storage_backend_error("agent_profiles inserted name", err).into())
}

fn get_agent_profile_backend(
    backend: &dyn StorageBackend,
    name: &str,
) -> Result<Option<crate::agent_profiles::AgentProfile>> {
    let row = backend
        .query_row_cells(
            "SELECT name, role, tags, shell, command, env, metadata,
                    created_at_ms, updated_at_ms
             FROM agent_profiles
             WHERE name = ?1",
            &[ToSqlValue::Text(name)],
        )
        .map_err(|err| storage_backend_error("agent_profiles get", err))?;
    row.as_deref()
        .map(agent_profile_from_backend_cells)
        .transpose()
}

fn list_agent_profiles_backend(
    backend: &dyn StorageBackend,
    role_filter: Option<&str>,
) -> Result<Vec<crate::agent_profiles::AgentProfile>> {
    let rows = match role_filter {
        Some(role) => backend.query_map_cells(
            "SELECT name, role, tags, shell, command, env, metadata,
                    created_at_ms, updated_at_ms
             FROM agent_profiles
             WHERE role = ?1
             ORDER BY name ASC",
            &[ToSqlValue::Text(role)],
        ),
        None => backend.query_map_cells(
            "SELECT name, role, tags, shell, command, env, metadata,
                    created_at_ms, updated_at_ms
             FROM agent_profiles
             ORDER BY name ASC",
            &[],
        ),
    }
    .map_err(|err| storage_backend_error("agent_profiles list", err))?;
    rows.iter()
        .map(|row| agent_profile_from_backend_cells(row))
        .collect()
}

fn delete_agent_profile_backend(backend: &dyn StorageBackend, name: &str) -> Result<bool> {
    let deleted = backend
        .query_row_typed(
            "DELETE FROM agent_profiles WHERE name = ?1 RETURNING 1",
            &[ToSqlValue::Text(name)],
        )
        .map_err(|err| storage_backend_error("agent_profiles delete", err))?;
    Ok(deleted.is_some())
}

fn agent_profile_from_backend_cells(
    row: &[SqlCell],
) -> Result<crate::agent_profiles::AgentProfile> {
    let reader = CellRowReader::new(row);
    let tags_json = reader
        .string(2)
        .map_err(|err| storage_backend_error("agent_profiles tags", err))?;
    let env_json = reader
        .string(5)
        .map_err(|err| storage_backend_error("agent_profiles env", err))?;
    let metadata_json = reader
        .string(6)
        .map_err(|err| storage_backend_error("agent_profiles metadata", err))?;
    Ok(crate::agent_profiles::AgentProfile {
        name: reader
            .string(0)
            .map_err(|err| storage_backend_error("agent_profiles name", err))?,
        role: reader
            .string(1)
            .map_err(|err| storage_backend_error("agent_profiles role", err))?,
        tags: serde_json::from_str(&tags_json).map_err(|err| {
            agent_profile_sql_to_error(agent_profiles_sql::AgentProfileSqlError::Decode {
                column: "tags",
                msg: err.to_string(),
            })
        })?,
        shell: reader
            .string(3)
            .map_err(|err| storage_backend_error("agent_profiles shell", err))?,
        command: reader
            .optional_string(4)
            .map_err(|err| storage_backend_error("agent_profiles command", err))?,
        env: serde_json::from_str(&env_json).map_err(|err| {
            agent_profile_sql_to_error(agent_profiles_sql::AgentProfileSqlError::Decode {
                column: "env",
                msg: err.to_string(),
            })
        })?,
        metadata: serde_json::from_str(&metadata_json).map_err(|err| {
            agent_profile_sql_to_error(agent_profiles_sql::AgentProfileSqlError::Decode {
                column: "metadata",
                msg: err.to_string(),
            })
        })?,
        created_at_ms: reader
            .i64(7)
            .map_err(|err| storage_backend_error("agent_profiles created_at_ms", err))?,
        updated_at_ms: reader
            .i64(8)
            .map_err(|err| storage_backend_error("agent_profiles updated_at_ms", err))?,
    })
}

// =============================================================================
// Session Checkpoint Sync Operations
// =============================================================================

/// Insert a new mux_sessions row (writer-thread, backend-trait path).
///
/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `insert_mux_session_sync(&Connection, ...)` direct-rusqlite
/// helper. Routes the INSERT through the trait surface using
/// `execute_typed`. Same shape as the
/// `prune_session_checkpoints_backend` (c72156eb4),
/// `upsert_action_undo_backend` (81589276c), and
/// `insert_prepared_plan_backend` (1c3e5e433) slices. Called from
/// the writer-thread dispatcher inside `with_writer_backend(...)`.
fn insert_mux_session_backend(
    backend: &dyn StorageBackend,
    session_id: &str,
    topology_json: &str,
    ft_version: &str,
    host_id: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    let host_id_value = match host_id {
        Some(s) => ToSqlValue::Text(s),
        None => ToSqlValue::Null,
    };
    execute_typed(
        backend,
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, host_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        &[
            ToSqlValue::Text(session_id),
            ToSqlValue::Integer(now),
            ToSqlValue::Text(topology_json),
            ToSqlValue::Text(ft_version),
            host_id_value,
        ],
    )
    .map_err(|err| storage_backend_error("Failed to insert mux session", err))?;
    Ok(())
}

/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `insert_session_checkpoint_sync(&mut Connection, ...)` direct-
/// rusqlite helper. Preserves the original IMMEDIATE transaction
/// semantics while inserting the checkpoint row, pane-state batch,
/// and mux-session timestamp update through [`StorageBackend`].
/// Called from the writer-thread dispatcher inside
/// `with_writer_backend(...)`.
fn insert_session_checkpoint_backend(
    backend: &dyn StorageBackend,
    session_id: &str,
    checkpoint_type: &str,
    state_hash: &str,
    pane_count: usize,
    total_bytes: usize,
    metadata_json: Option<&str>,
    pane_states: &[SessionPaneStateRow],
) -> Result<i64> {
    let now = now_ms();
    let pane_count_i64 = usize_to_i64(pane_count, "session_checkpoints.pane_count")?;
    let total_bytes_i64 = usize_to_i64(total_bytes, "session_checkpoints.total_bytes")?;
    let pane_state_rows: Vec<Vec<ToSqlValue<'_>>> = pane_states
        .iter()
        .map(|ps| {
            Ok(vec![
                ToSqlValue::Integer(u64_to_i64(ps.pane_id, "mux_pane_state.pane_id")?),
                ToSqlValue::optional_text(ps.cwd.as_deref()),
                ToSqlValue::optional_text(ps.command.as_deref()),
                ToSqlValue::optional_text(ps.env_json.as_deref()),
                ToSqlValue::Text(ps.terminal_state_json.as_str()),
                ToSqlValue::optional_text(ps.agent_metadata_json.as_deref()),
                ToSqlValue::optional_i64(ps.scrollback_checkpoint_seq),
                ToSqlValue::optional_i64(ps.last_output_at),
            ])
        })
        .collect::<Result<Vec<_>>>()?;

    backend
        .execute("BEGIN IMMEDIATE")
        .map_err(|err| storage_backend_error("Failed to begin checkpoint txn", err))?;

    let tx_result = (|| -> Result<i64> {
        let row = backend
            .query_row_typed(
                "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes, metadata_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
                &[
                    ToSqlValue::Text(session_id),
                    ToSqlValue::Integer(now),
                    ToSqlValue::Text(checkpoint_type),
                    ToSqlValue::Text(state_hash),
                    ToSqlValue::Integer(pane_count_i64),
                    ToSqlValue::Integer(total_bytes_i64),
                    ToSqlValue::optional_text(metadata_json),
                ],
            )
            .map_err(|err| storage_backend_error("Failed to insert session checkpoint", err))?
            .ok_or_else(|| {
                StorageError::Database(
                    "Failed to insert session checkpoint: no id returned".to_string(),
                )
            })?;
        let checkpoint_id = RowReader::new(&row)
            .i64(0)
            .map_err(|err| storage_backend_error("Failed to parse session checkpoint id", err))?;

        let pane_state_param_rows: Vec<Vec<ToSqlValue<'_>>> = pane_state_rows
            .iter()
            .map(|row| {
                let mut params = Vec::with_capacity(9);
                params.push(ToSqlValue::Integer(checkpoint_id));
                params.extend(row.iter().cloned());
                params
            })
            .collect();

        backend
            .execute_many(
                "INSERT INTO mux_pane_state
                 (checkpoint_id, pane_id, cwd, command, env_json, terminal_state_json,
                  agent_metadata_json, scrollback_checkpoint_seq, last_output_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                &pane_state_param_rows,
            )
            .map_err(|err| storage_backend_error("Failed to insert pane state", err))?;

        execute_typed(
            backend,
            "UPDATE mux_sessions SET last_checkpoint_at = ?1 WHERE session_id = ?2",
            &[ToSqlValue::Integer(now), ToSqlValue::Text(session_id)],
        )
        .map_err(|err| {
            storage_backend_error("Failed to update session checkpoint timestamp", err)
        })?;

        Ok(checkpoint_id)
    })();

    match tx_result {
        Ok(checkpoint_id) => match backend.execute("COMMIT") {
            Ok(_) => Ok(checkpoint_id),
            Err(commit_err) => {
                let _ = backend.execute("ROLLBACK");
                Err(storage_backend_error("Failed to commit checkpoint", commit_err).into())
            }
        },
        Err(err) => {
            let _ = backend.execute("ROLLBACK");
            Err(err)
        }
    }
}

/// Prune session_checkpoints down to the most-recent `retention`
/// rows per session (writer-thread, backend-trait path).
///
/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `prune_session_checkpoints_sync(&Connection, &str, usize)`
/// direct-rusqlite helper. Routes the DELETE through the trait
/// surface using `RETURNING id` + `query_map_typed`; the affected-
/// row count is recovered from `returned.len()` without a separate
/// `SELECT changes()` call — same shape as the
/// `purge_audit_actions_backend` slice (c64527d9c) and
/// `delete_events_before_backend` slice (81589276c). The inner
/// `id NOT IN (...)` subquery preserving the most-recent N rows
/// is unchanged. Called from the writer-thread dispatcher inside
/// `with_writer_backend(...)`.
fn prune_session_checkpoints_backend(
    backend: &dyn StorageBackend,
    session_id: &str,
    retention: usize,
) -> Result<usize> {
    let keep_count = i64::try_from(retention).unwrap_or(i64::MAX);
    let returned = backend
        .query_map_typed(
            "DELETE FROM session_checkpoints
             WHERE session_id = ?1
             AND id NOT IN (
                 SELECT id FROM session_checkpoints
                 WHERE session_id = ?1
                 ORDER BY checkpoint_at DESC
                 LIMIT ?2
             )
             RETURNING id",
            &[
                ToSqlValue::Text(session_id),
                ToSqlValue::Integer(keep_count),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to prune session checkpoints", err))?;
    Ok(returned.len())
}

/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `mark_session_shutdown_clean_sync(&Connection, ...)` direct-
/// rusqlite helper. Routes through the StorageBackend trait via
/// `execute_typed`. Called from the writer-thread dispatcher
/// inside `with_writer_backend(...)`.
fn mark_session_shutdown_clean_backend(
    backend: &dyn StorageBackend,
    session_id: &str,
) -> Result<()> {
    execute_typed(
        backend,
        "UPDATE mux_sessions SET shutdown_clean = 1 WHERE session_id = ?1",
        &[ToSqlValue::Text(session_id)],
    )
    .map_err(|err| storage_backend_error("Failed to mark session shutdown clean", err))?;
    Ok(())
}

/// Get the state_hash of the latest checkpoint for a session (read-only).
///
/// Direct-rusqlite path. Kept as a fallback while the
/// [`get_latest_checkpoint_hash_backend`] migration target settles
/// in (br-ft-l1jgo); will be removed once no callers remain.
#[allow(dead_code)]
pub fn get_latest_checkpoint_hash(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let result = conn
        .query_row(
            "SELECT state_hash FROM session_checkpoints
             WHERE session_id = ?1
             ORDER BY checkpoint_at DESC LIMIT 1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| {
            StorageError::Database(format!("Failed to get latest checkpoint hash: {e}"))
        })?;
    Ok(result)
}

/// br-ft-l1jgo: trait-typed sibling of [`get_latest_checkpoint_hash`].
///
/// `state_hash` is a content hash (SHA-derived) and the column is
/// NOT NULL by schema, so `query_row_typed`'s string-substrate path
/// — which can't distinguish NULL from empty TEXT — is sound here.
fn get_latest_checkpoint_hash_backend(
    backend: &dyn StorageBackend,
    session_id: &str,
) -> Result<Option<String>> {
    let row = backend
        .query_row_typed(
            "SELECT state_hash FROM session_checkpoints
             WHERE session_id = ?1
             ORDER BY checkpoint_at DESC LIMIT 1",
            &[ToSqlValue::Text(session_id)],
        )
        .map_err(|err| storage_backend_error("Get latest checkpoint hash", err))?;
    Ok(row.and_then(|cells| cells.into_iter().next()))
}

fn pane_bookmark_from_backend_row(row: &[String]) -> Result<PaneBookmarkRecord> {
    let reader = RowReader::new(row);
    let bookmark_id = reader
        .i64(0)
        .map_err(|err| storage_backend_error("Pane bookmark id", err))?;
    let pane_id_i64 = reader
        .i64(1)
        .map_err(|err| storage_backend_error("Pane bookmark pane_id", err))?;
    let pane_id = backend_i64_to_u64(pane_id_i64, "pane_bookmarks.pane_id")
        .map_err(|err| storage_backend_error("Pane bookmark pane_id", err))?;
    let tags_raw = reader
        .optional_string(3)
        .map_err(|err| storage_backend_error("Pane bookmark tags", err))?;
    // br-ft-pewat: route through the parse-drop helper so a
    // malformed tags JSON (schema drift, manual DB edit) bumps
    // PANE_BOOKMARK_TAGS_PARSE_DROP_COUNT instead of silently
    // turning into None — operators running tag-filtered queries
    // would otherwise see bookmarks "missing" with no signal.
    let tags = tags_raw
        .as_deref()
        .and_then(|s| parse_pane_bookmark_tags(s, bookmark_id, pane_id));
    Ok(PaneBookmarkRecord {
        id: bookmark_id,
        pane_id,
        alias: reader
            .string(2)
            .map_err(|err| storage_backend_error("Pane bookmark alias", err))?,
        tags,
        description: reader
            .optional_string(4)
            .map_err(|err| storage_backend_error("Pane bookmark description", err))?,
        created_at: reader
            .i64(5)
            .map_err(|err| storage_backend_error("Pane bookmark created_at", err))?,
        updated_at: reader
            .i64(6)
            .map_err(|err| storage_backend_error("Pane bookmark updated_at", err))?,
    })
}

fn query_pane_bookmark_by_alias_backend(
    backend: &dyn StorageBackend,
    alias: &str,
) -> Result<Option<PaneBookmarkRecord>> {
    let row = backend
        .query_row_typed(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks WHERE alias = ?1",
            &[ToSqlValue::Text(alias)],
        )
        .map_err(|err| storage_backend_error("Failed to query pane bookmark", err))?;
    row.as_deref()
        .map(pane_bookmark_from_backend_row)
        .transpose()
}

fn list_pane_bookmarks_backend(backend: &dyn StorageBackend) -> Result<Vec<PaneBookmarkRecord>> {
    let rows = backend
        .query_map_typed(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks ORDER BY alias ASC",
            &[],
        )
        .map_err(|err| storage_backend_error("Failed to list pane bookmarks", err))?;
    rows.iter()
        .map(|row| pane_bookmark_from_backend_row(row))
        .collect()
}

fn list_pane_bookmarks_by_tag_backend(
    backend: &dyn StorageBackend,
    tag: &str,
) -> Result<Vec<PaneBookmarkRecord>> {
    // Use JSON containment check: tags column is a JSON array.
    let pattern = format!("%\"{tag}\"%");
    let rows = backend
        .query_map_typed(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks WHERE tags LIKE ?1 ORDER BY alias ASC",
            &[ToSqlValue::Text(&pattern)],
        )
        .map_err(|err| storage_backend_error("Failed to list pane bookmarks by tag", err))?;
    rows.iter()
        .map(|row| pane_bookmark_from_backend_row(row))
        .collect()
}

fn prune_segments_backend(backend: &dyn StorageBackend, before_ts: i64) -> Result<usize> {
    let deleted_rows = backend
        .query_map_typed(
            "DELETE FROM output_segments WHERE captured_at < ?1 RETURNING id",
            &[ToSqlValue::Integer(before_ts)],
        )
        .map_err(|err| storage_backend_error("Failed to prune segments", err))?;
    let deleted = deleted_rows.len();

    // [ft-znu6v] Rewind stranded FTS progress after pruning.
    //
    // `append_segment_backend` assigns `seq = COALESCE(MAX(seq) + 1, 0)`,
    // so after a full prune a live pane's
    // next append restarts at seq=0. If a progress row still carries
    // the pre-prune high-water mark, the strict `seq > last_indexed_seq`
    // branch in `get_unindexed_segments_sync` (15949-15953) would never
    // surface the reset-chain rows. This is especially damaging under
    // deferred-FTS (`defer_fts_triggers=true`), where the output_segments_fts
    // delete triggers have been dropped and the normal cascade does not
    // clean stale FTS state either.
    //
    // Drop any progress row whose `last_indexed_seq` no longer maps to a
    // surviving row for that pane. The COALESCE(..., -1) treats the
    // "no remaining rows for pane" case as `max_seq = -1`, so any
    // recorded `last_indexed_seq >= 0` triggers the delete. A subsequent
    // `sync_fts_for_pane` sees `had_prior_progress = false` and takes
    // the inclusive `WHERE pane_id = ?1` branch, picking up seq=0.
    if deleted > 0 {
        backend
            .query_map_typed(
                "DELETE FROM fts_pane_progress
             WHERE last_indexed_seq > COALESCE(
                 (SELECT MAX(seq) FROM output_segments
                  WHERE output_segments.pane_id = fts_pane_progress.pane_id),
                 -1
             )
             RETURNING pane_id",
                &[],
            )
            .map_err(|err| storage_backend_error("Failed to rewind stranded FTS progress", err))?;
    }

    Ok(deleted)
}

/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `vacuum_sync(&Connection)` direct-rusqlite helper. Routes
/// `VACUUM` through `StorageBackend::execute_batch`. Called
/// from the writer-thread dispatcher inside
/// `with_writer_backend(...)`.
fn vacuum_backend(backend: &dyn StorageBackend) -> Result<()> {
    backend
        .execute_batch("VACUUM")
        .map_err(|err| storage_backend_error("Failed to vacuum database", err))?;
    Ok(())
}

/// Checkpoint WAL (PASSIVE — non-blocking, does not stall readers or writers)
/// and run PRAGMA optimize to maintain query planner statistics.
fn checkpoint_backend(backend: &dyn StorageBackend) -> Result<CheckpointResult> {
    let row = backend
        .query_row_typed("PRAGMA wal_checkpoint(PASSIVE)", &[])
        .map_err(|err| storage_backend_error("WAL checkpoint failed", err))?
        .ok_or_else(|| StorageError::Database("WAL checkpoint returned no row".to_string()))?;
    let wal_pages = RowReader::new(&row)
        .i64(1)
        .map_err(|err| storage_backend_error("WAL checkpoint page count", err))?;

    backend
        .execute_batch("PRAGMA optimize")
        .map_err(|err| storage_backend_error("PRAGMA optimize failed", err))?;

    Ok(CheckpointResult {
        wal_pages,
        optimized: true,
    })
}

fn database_page_stats_backend(backend: &dyn StorageBackend) -> Result<DatabasePageStats> {
    use crate::storage_backend_helpers::pragma_value;
    let page_count = pragma_value(backend, "page_count")
        .map_err(|err| storage_backend_error("PRAGMA page_count", err))?
        .ok_or_else(|| StorageError::Database("PRAGMA page_count returned no row".to_string()))?
        .parse::<i64>()
        .map_err(|e| StorageError::Database(format!("PRAGMA page_count parse: {e}")))?;
    let free_pages = pragma_value(backend, "freelist_count")
        .map_err(|err| storage_backend_error("PRAGMA freelist_count", err))?
        .ok_or_else(|| StorageError::Database("PRAGMA freelist_count returned no row".to_string()))?
        .parse::<i64>()
        .map_err(|e| StorageError::Database(format!("PRAGMA freelist_count parse: {e}")))?;
    Ok(DatabasePageStats {
        page_count,
        free_pages,
    })
}

// =============================================================================
// Usage Metrics Operations (Synchronous)
// =============================================================================

fn record_usage_metric_backend(
    backend: &dyn StorageBackend,
    record: &UsageMetricRecord,
) -> Result<i64> {
    let ts = if record.timestamp == 0 {
        now_ms()
    } else {
        record.timestamp
    };
    let created = if record.created_at == 0 {
        now_ms()
    } else {
        record.created_at
    };
    let pane_id = record.pane_id.map(|id| id as i64);
    let amount = match record.amount {
        Some(value) => ToSqlValue::Real(value),
        None => ToSqlValue::Null,
    };

    let row = backend
        .query_row_typed(
            "INSERT INTO usage_metrics (
                timestamp, metric_type, pane_id, agent_type, account_id,
                workflow_id, count, amount, tokens, metadata, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             RETURNING id",
            &[
                ToSqlValue::Integer(ts),
                ToSqlValue::Text(record.metric_type.as_str()),
                ToSqlValue::optional_i64(pane_id),
                ToSqlValue::optional_text(record.agent_type.as_deref()),
                ToSqlValue::optional_text(record.account_id.as_deref()),
                ToSqlValue::optional_text(record.workflow_id.as_deref()),
                ToSqlValue::optional_i64(record.count),
                amount,
                ToSqlValue::optional_i64(record.tokens),
                ToSqlValue::optional_text(record.metadata.as_deref()),
                ToSqlValue::Integer(created),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to record usage metric", err))?
        .ok_or_else(|| StorageError::Database("Usage metric insert returned no id".to_string()))?;

    Ok(RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Usage metric insert id", err))?)
}

/// Direct-rusqlite path. Kept as a fallback while the
/// [`record_usage_metrics_batch_backend`] migration target settles
/// in (br-ft-l1jgo); will be removed once no callers remain.
#[allow(dead_code)]
fn record_usage_metrics_batch_sync(
    conn: &mut Connection,
    records: &[UsageMetricRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| StorageError::Database(format!("Failed to start metrics batch tx: {e}")))?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO usage_metrics (timestamp, metric_type, pane_id, agent_type, account_id, workflow_id, count, amount, tokens, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(|e| {
                StorageError::Database(format!("Failed to prepare metrics batch insert: {e}"))
            })?;

        for record in records {
            let ts = if record.timestamp == 0 {
                now_ms()
            } else {
                record.timestamp
            };
            let created = if record.created_at == 0 {
                now_ms()
            } else {
                record.created_at
            };
            let pane_id = record.pane_id.map(|id| id as i64);

            stmt.execute(params![
                ts,
                record.metric_type.as_str(),
                pane_id,
                record.agent_type,
                record.account_id,
                record.workflow_id,
                record.count,
                record.amount,
                record.tokens,
                record.metadata,
                created,
            ])
            .map_err(|e| StorageError::Database(format!("Failed to insert usage metric: {e}")))?;
        }
    }

    tx.commit()
        .map_err(|e| StorageError::Database(format!("Failed to commit metrics batch tx: {e}")))?;

    Ok(records.len())
}

/// br-ft-l1jgo: trait-typed sibling of [`record_usage_metrics_batch_sync`].
/// Uses [`StorageBackend::execute_many`] (ft-qgj81 slice 5) to batch the
/// inserts through the backend's prepare-cached path, wrapped in an
/// explicit BEGIN/COMMIT for one fsync per batch instead of one per row
/// (the textbook SQLite bulk-insert optimization).
fn record_usage_metrics_batch_backend(
    backend: &dyn StorageBackend,
    records: &[UsageMetricRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }

    // Build the param table up front so the BEGIN/COMMIT window is tight.
    // `agent_type` / `account_id` / `workflow_id` / `metadata` are
    // `Option<String>` on the record so they're owned here; the helper
    // builds `ToSqlValue::OwnedText` from `s.clone()` for those, and
    // borrowed `ToSqlValue::Text` for `metric_type.as_str()` (which lives
    // inside the record). The mapped `pane_id` (`Option<u64>` → i64) goes
    // through `optional_i64`. `amount` is `Option<f64>` and matched
    // manually since `ToSqlValue` lacks an `optional_real` constructor.
    let now = now_ms();
    let owned_rows: Vec<Vec<ToSqlValue<'_>>> = records
        .iter()
        .map(|record| {
            let ts = if record.timestamp == 0 {
                now
            } else {
                record.timestamp
            };
            let created = if record.created_at == 0 {
                now
            } else {
                record.created_at
            };
            let pane_id = record.pane_id.map(|id| {
                #[allow(clippy::cast_possible_wrap)]
                {
                    id as i64
                }
            });
            let amount = match record.amount {
                Some(v) => ToSqlValue::Real(v),
                None => ToSqlValue::Null,
            };
            vec![
                ToSqlValue::Integer(ts),
                ToSqlValue::Text(record.metric_type.as_str()),
                ToSqlValue::optional_i64(pane_id),
                ToSqlValue::optional_text(record.agent_type.as_deref()),
                ToSqlValue::optional_text(record.account_id.as_deref()),
                ToSqlValue::optional_text(record.workflow_id.as_deref()),
                ToSqlValue::optional_i64(record.count),
                amount,
                ToSqlValue::optional_i64(record.tokens),
                ToSqlValue::optional_text(record.metadata.as_deref()),
                ToSqlValue::Integer(created),
            ]
        })
        .collect();

    // Wrap in BEGIN/COMMIT for atomic-batch + per-batch fsync. See the
    // execute_many recipe in docs/storage/backend-migration-guide.md
    // for the error-handling rationale.
    backend
        .execute("BEGIN IMMEDIATE")
        .map_err(|err| storage_backend_error("Start metrics batch tx", err))?;
    let inserted = match backend.execute_many(
        "INSERT INTO usage_metrics (timestamp, metric_type, pane_id, agent_type, \
         account_id, workflow_id, count, amount, tokens, metadata, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        &owned_rows,
    ) {
        Ok(count) => match backend.execute("COMMIT") {
            Ok(_) => Ok(count),
            Err(commit_err) => {
                let _ = backend.execute("ROLLBACK");
                Err(storage_backend_error("Commit metrics batch tx", commit_err))
            }
        },
        Err(batch_err) => {
            let _ = backend.execute("ROLLBACK");
            Err(storage_backend_error(
                "Insert usage metric batch",
                batch_err,
            ))
        }
    }?;
    Ok(inserted)
}

fn purge_usage_metrics_backend(backend: &dyn StorageBackend, before_ts: i64) -> Result<usize> {
    let deleted = backend
        .query_map_typed(
            "DELETE FROM usage_metrics
             WHERE timestamp < ?1
             RETURNING 1",
            &[ToSqlValue::Integer(before_ts)],
        )
        .map_err(|err| storage_backend_error("Failed to purge usage metrics", err))?;
    Ok(deleted.len())
}

fn build_usage_metrics_query(query: &MetricQuery) -> (String, Vec<SqlValue>) {
    let mut sql = String::from(
        "SELECT id, timestamp, metric_type, pane_id, agent_type, account_id, workflow_id, count, amount, tokens, metadata, created_at FROM usage_metrics WHERE 1=1",
    );
    let mut param_values = Vec::new();

    if let Some(ref mt) = query.metric_type {
        param_values.push(SqlValue::Text(mt.as_str().to_string()));
        sql.push_str(&format!(" AND metric_type = ?{}", param_values.len()));
    }
    if let Some(ref agent) = query.agent_type {
        param_values.push(SqlValue::Text(agent.clone()));
        sql.push_str(&format!(" AND agent_type = ?{}", param_values.len()));
    }
    if let Some(ref account) = query.account_id {
        param_values.push(SqlValue::Text(account.clone()));
        sql.push_str(&format!(" AND account_id = ?{}", param_values.len()));
    }
    if let Some(since) = query.since {
        param_values.push(SqlValue::Integer(since));
        sql.push_str(&format!(" AND timestamp >= ?{}", param_values.len()));
    }
    if let Some(until) = query.until {
        param_values.push(SqlValue::Integer(until));
        sql.push_str(&format!(" AND timestamp < ?{}", param_values.len()));
    }
    sql.push_str(" ORDER BY timestamp DESC");
    if let Some(limit) = query.limit {
        param_values.push(SqlValue::Integer(limit as i64));
        sql.push_str(&format!(" LIMIT ?{}", param_values.len()));
    }

    (sql, param_values)
}

fn optional_backend_f64(
    reader: &RowReader<'_>,
    idx: usize,
) -> std::result::Result<Option<f64>, BackendError> {
    let Some(value) = reader.optional_string(idx)? else {
        return Ok(None);
    };
    value.parse::<f64>().map(Some).map_err(|err| {
        BackendError::Query(format!(
            "row column {idx}: optional f64 parse failed for `{value}`: {err}"
        ))
    })
}

fn usage_metric_record_from_backend_row(row: &[String]) -> Result<UsageMetricRecord> {
    let reader = RowReader::new(row);
    let metric_type_str = reader
        .string(2)
        .map_err(|err| storage_backend_error("Usage metric type", err))?;
    let pane_id = reader
        .optional_i64(3)
        .map_err(|err| storage_backend_error("Usage metric pane_id", err))?
        .map(|value| backend_i64_to_u64(value, "usage_metrics.pane_id"))
        .transpose()
        .map_err(|err| storage_backend_error("Usage metric pane_id", err))?;

    Ok(UsageMetricRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Usage metric id", err))?,
        timestamp: reader
            .i64(1)
            .map_err(|err| storage_backend_error("Usage metric timestamp", err))?,
        metric_type: metric_type_str.parse().unwrap_or(MetricType::ApiCall),
        pane_id,
        agent_type: reader
            .optional_string(4)
            .map_err(|err| storage_backend_error("Usage metric agent_type", err))?,
        account_id: reader
            .optional_string(5)
            .map_err(|err| storage_backend_error("Usage metric account_id", err))?,
        workflow_id: reader
            .optional_string(6)
            .map_err(|err| storage_backend_error("Usage metric workflow_id", err))?,
        count: reader
            .optional_i64(7)
            .map_err(|err| storage_backend_error("Usage metric count", err))?,
        amount: optional_backend_f64(&reader, 8)
            .map_err(|err| storage_backend_error("Usage metric amount", err))?,
        tokens: reader
            .optional_i64(9)
            .map_err(|err| storage_backend_error("Usage metric tokens", err))?,
        metadata: reader
            .optional_string(10)
            .map_err(|err| storage_backend_error("Usage metric metadata", err))?,
        created_at: reader
            .i64(11)
            .map_err(|err| storage_backend_error("Usage metric created_at", err))?,
    })
}

fn query_usage_metrics_backend(
    backend: &dyn StorageBackend,
    query: &MetricQuery,
) -> Result<Vec<UsageMetricRecord>> {
    let (sql, param_values) = build_usage_metrics_query(query);
    let params = sql_values_to_backend_params(&param_values);
    let rows = backend
        .query_map_typed(&sql, &params)
        .map_err(|err| storage_backend_error("Failed to query metrics", err))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(usage_metric_record_from_backend_row(&row)?);
    }
    Ok(results)
}

fn daily_metric_summary_from_backend_row(row: &[String]) -> Result<DailyMetricSummary> {
    let reader = RowReader::new(row);
    Ok(DailyMetricSummary {
        day_ts: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Daily metric day_ts", err))?,
        agent_type: reader
            .optional_string(1)
            .map_err(|err| storage_backend_error("Daily metric agent_type", err))?,
        total_tokens: reader
            .i64(2)
            .map_err(|err| storage_backend_error("Daily metric total_tokens", err))?,
        total_cost: reader
            .f64(3)
            .map_err(|err| storage_backend_error("Daily metric total_cost", err))?,
        event_count: reader
            .i64(4)
            .map_err(|err| storage_backend_error("Daily metric event_count", err))?,
    })
}

fn aggregate_daily_backend(
    backend: &dyn StorageBackend,
    since_ts: i64,
) -> Result<Vec<DailyMetricSummary>> {
    let rows = backend
        .query_map_typed(
            "SELECT (timestamp / 86400000) * 86400000 AS day_ts,
                    agent_type,
                    COALESCE(SUM(tokens), 0),
                    COALESCE(SUM(amount), 0.0),
                    COUNT(*)
             FROM usage_metrics
             WHERE timestamp >= ?1
             GROUP BY day_ts, agent_type
             ORDER BY day_ts DESC",
            &[ToSqlValue::Integer(since_ts)],
        )
        .map_err(|err| storage_backend_error("Failed to aggregate daily", err))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(daily_metric_summary_from_backend_row(&row)?);
    }
    Ok(results)
}

fn agent_metric_breakdown_from_backend_row(row: &[String]) -> Result<AgentMetricBreakdown> {
    let reader = RowReader::new(row);
    Ok(AgentMetricBreakdown {
        agent_type: reader
            .string(0)
            .map_err(|err| storage_backend_error("Agent metric agent_type", err))?,
        total_tokens: reader
            .i64(1)
            .map_err(|err| storage_backend_error("Agent metric total_tokens", err))?,
        total_cost: reader
            .f64(2)
            .map_err(|err| storage_backend_error("Agent metric total_cost", err))?,
        avg_tokens_per_event: reader
            .f64(3)
            .map_err(|err| storage_backend_error("Agent metric avg_tokens_per_event", err))?,
    })
}

fn aggregate_by_agent_backend(
    backend: &dyn StorageBackend,
    since_ts: i64,
) -> Result<Vec<AgentMetricBreakdown>> {
    let rows = backend
        .query_map_typed(
            "SELECT COALESCE(agent_type, 'unknown'),
                    COALESCE(SUM(tokens), 0),
                    COALESCE(SUM(amount), 0.0),
                    CASE WHEN COUNT(*) > 0 THEN CAST(COALESCE(SUM(tokens), 0) AS REAL) / COUNT(*) ELSE 0.0 END
             FROM usage_metrics
             WHERE timestamp >= ?1
             GROUP BY agent_type
             ORDER BY SUM(amount) DESC",
            &[ToSqlValue::Integer(since_ts)],
        )
        .map_err(|err| storage_backend_error("Failed to aggregate by agent", err))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(agent_metric_breakdown_from_backend_row(&row)?);
    }
    Ok(results)
}

// =============================================================================
// Notification History Operations (Synchronous)
// =============================================================================

fn record_notification_backend(
    backend: &dyn StorageBackend,
    record: &NotificationHistoryRecord,
) -> Result<i64> {
    let row = backend
        .query_row_typed(
            "INSERT INTO notification_history (
                timestamp, event_id, channel, title, body, severity,
                status, error_message, acknowledged_at, acknowledged_by,
                action_taken, retry_count, metadata, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             RETURNING id",
            &[
                ToSqlValue::Integer(record.timestamp),
                ToSqlValue::optional_i64(record.event_id),
                ToSqlValue::Text(&record.channel),
                ToSqlValue::Text(&record.title),
                ToSqlValue::Text(&record.body),
                ToSqlValue::Text(&record.severity),
                ToSqlValue::Text(record.status.as_str()),
                ToSqlValue::optional_text(record.error_message.as_deref()),
                ToSqlValue::optional_i64(record.acknowledged_at),
                ToSqlValue::optional_text(record.acknowledged_by.as_deref()),
                ToSqlValue::optional_text(record.action_taken.as_deref()),
                ToSqlValue::Integer(record.retry_count),
                ToSqlValue::optional_text(record.metadata.as_deref()),
                ToSqlValue::Integer(record.created_at),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to record notification", err))?
        .ok_or_else(|| StorageError::Database("Notification insert returned no id".to_string()))?;

    Ok(RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Notification insert id", err))?)
}

fn update_notification_status_backend(
    backend: &dyn StorageBackend,
    id: i64,
    status: NotificationStatus,
    error_message: Option<&str>,
) -> Result<()> {
    let updated = backend
        .query_row_typed(
            "UPDATE notification_history
             SET status = ?1, error_message = ?2
             WHERE id = ?3
             RETURNING 1",
            &[
                ToSqlValue::Text(status.as_str()),
                ToSqlValue::optional_text(error_message),
                ToSqlValue::Integer(id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to update notification status", err))?;
    if updated.is_none() {
        return Err(StorageError::Database(format!("Notification {id} not found")).into());
    }
    Ok(())
}

fn acknowledge_notification_backend(
    backend: &dyn StorageBackend,
    id: i64,
    acknowledged_by: &str,
    action_taken: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    let updated = backend
        .query_row_typed(
            "UPDATE notification_history
             SET acknowledged_at = ?1, acknowledged_by = ?2, action_taken = ?3
             WHERE id = ?4
             RETURNING 1",
            &[
                ToSqlValue::Integer(now),
                ToSqlValue::Text(acknowledged_by),
                ToSqlValue::optional_text(action_taken),
                ToSqlValue::Integer(id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to acknowledge notification", err))?;
    if updated.is_none() {
        return Err(StorageError::Database(format!("Notification {id} not found")).into());
    }
    Ok(())
}

fn increment_notification_retry_backend(backend: &dyn StorageBackend, id: i64) -> Result<()> {
    let updated = backend
        .query_row_typed(
            "UPDATE notification_history
             SET retry_count = retry_count + 1, status = 'pending'
             WHERE id = ?1
             RETURNING 1",
            &[ToSqlValue::Integer(id)],
        )
        .map_err(|err| storage_backend_error("Failed to increment notification retry", err))?;
    if updated.is_none() {
        return Err(StorageError::Database(format!("Notification {id} not found")).into());
    }
    Ok(())
}

fn purge_notification_history_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
) -> Result<usize> {
    let deleted = backend
        .query_map_typed(
            "DELETE FROM notification_history
             WHERE timestamp < ?1
             RETURNING 1",
            &[ToSqlValue::Integer(before_ts)],
        )
        .map_err(|err| storage_backend_error("Failed to purge notification history", err))?;
    Ok(deleted.len())
}

// =============================================================================
// Cleanup engine: count + delete sync helpers
// =============================================================================

fn count_query_row_to_usize(row: &[String], context: &str) -> Result<usize> {
    let count = RowReader::new(row)
        .i64(0)
        .map_err(|err| storage_backend_error(context, err))?;
    count_i64_to_usize(count, context)
}

fn count_i64_to_usize(count: i64, context: &str) -> Result<usize> {
    usize::try_from(count).map_err(|_| {
        StorageError::Database(format!("{context}: count out of range: {count}")).into()
    })
}

fn count_segments_before_backend(backend: &dyn StorageBackend, before_ts: i64) -> Result<usize> {
    let count = count_table_where(
        backend,
        "output_segments",
        "captured_at < ?1",
        &[ToSqlValue::Integer(before_ts)],
    )
    .map_err(|err| storage_backend_error("Failed to count segments", err))?;
    count_i64_to_usize(count, "Failed to count segments row")
}

fn count_events_before_backend(backend: &dyn StorageBackend, before_ts: i64) -> Result<usize> {
    let count = count_table_where(
        backend,
        "events",
        "detected_at < ?1",
        &[ToSqlValue::Integer(before_ts)],
    )
    .map_err(|err| storage_backend_error("Failed to count events", err))?;
    count_i64_to_usize(count, "Failed to count events row")
}

fn count_events_by_tier_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
    severities: &[String],
    event_types: &[String],
    handled: Option<bool>,
) -> Result<usize> {
    let (sql, param_values) = build_tier_query(
        "SELECT COUNT(*) FROM events",
        before_ts,
        severities,
        event_types,
        handled,
    );
    let params = sql_values_to_backend_params(&param_values);
    let row = backend
        .query_row_typed(&sql, &params)
        .map_err(|err| storage_backend_error("Failed to count events by tier", err))?
        .ok_or_else(|| {
            StorageError::Database(
                "Failed to count events by tier: query returned no row".to_string(),
            )
        })?;
    count_query_row_to_usize(&row, "Failed to count events by tier row")
}

fn count_audit_actions_before_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
) -> Result<usize> {
    let count = count_table_where(
        backend,
        "audit_actions",
        "ts < ?1",
        &[ToSqlValue::Integer(before_ts)],
    )
    .map_err(|err| storage_backend_error("Failed to count audit actions", err))?;
    count_i64_to_usize(count, "Failed to count audit actions row")
}

fn count_usage_metrics_before_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
) -> Result<usize> {
    let count = count_table_where(
        backend,
        "usage_metrics",
        "timestamp < ?1",
        &[ToSqlValue::Integer(before_ts)],
    )
    .map_err(|err| storage_backend_error("Failed to count usage metrics", err))?;
    count_i64_to_usize(count, "Failed to count usage metrics row")
}

fn count_notification_history_before_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
) -> Result<usize> {
    let count = count_table_where(
        backend,
        "notification_history",
        "timestamp < ?1",
        &[ToSqlValue::Integer(before_ts)],
    )
    .map_err(|err| storage_backend_error("Failed to count notification history", err))?;
    count_i64_to_usize(count, "Failed to count notification history row")
}

fn delete_events_before_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
    batch_size: usize,
) -> Result<usize> {
    if batch_size == 0 {
        return Ok(0);
    }

    let mut total_deleted = 0usize;
    loop {
        let deleted = backend
            .query_map_typed(
                "DELETE FROM events WHERE id IN (\
                 SELECT id FROM events WHERE detected_at < ?1 LIMIT ?2) \
                 RETURNING 1",
                &[
                    ToSqlValue::Integer(before_ts),
                    ToSqlValue::Integer(batch_size as i64),
                ],
            )
            .map_err(|err| storage_backend_error("Failed to delete events", err))?
            .len();
        total_deleted += deleted;
        if deleted < batch_size {
            break;
        }
    }
    Ok(total_deleted)
}

fn delete_events_by_tier_backend(
    backend: &dyn StorageBackend,
    before_ts: i64,
    severities: &[String],
    event_types: &[String],
    handled: Option<bool>,
    batch_size: usize,
) -> Result<usize> {
    if batch_size == 0 {
        return Ok(0);
    }

    let (inner_query, param_values) = build_tier_query(
        "SELECT id FROM events",
        before_ts,
        severities,
        event_types,
        handled,
    );
    let delete_sql =
        format!("DELETE FROM events WHERE id IN ({inner_query} LIMIT {batch_size}) RETURNING 1");
    let params = sql_values_to_backend_params(&param_values);

    let mut total_deleted = 0usize;
    loop {
        let deleted = backend
            .query_map_typed(&delete_sql, &params)
            .map_err(|err| storage_backend_error("Failed to delete events by tier", err))?
            .len();
        total_deleted += deleted;
        if deleted < batch_size {
            break;
        }
    }
    Ok(total_deleted)
}

/// Build a tier-filtered query clause with positional parameters.
fn build_tier_query(
    select_prefix: &str,
    before_ts: i64,
    severities: &[String],
    event_types: &[String],
    handled: Option<bool>,
) -> (String, Vec<SqlValue>) {
    let mut sql = format!("{select_prefix} WHERE detected_at < ?");
    let mut params: Vec<SqlValue> = vec![SqlValue::Integer(before_ts)];

    if !severities.is_empty() {
        let placeholders: Vec<String> = severities.iter().map(|_| "?".to_string()).collect();
        sql.push_str(&format!(" AND severity IN ({})", placeholders.join(",")));
        for s in severities {
            params.push(SqlValue::Text(s.clone()));
        }
    }

    if !event_types.is_empty() {
        let conditions: Vec<String> = event_types
            .iter()
            .map(|_| "event_type LIKE ?".to_string())
            .collect();
        sql.push_str(&format!(" AND ({})", conditions.join(" OR ")));
        for et in event_types {
            params.push(SqlValue::Text(format!("{et}%")));
        }
    }

    if let Some(want_handled) = handled {
        if want_handled {
            sql.push_str(" AND handled_at IS NOT NULL");
        } else {
            sql.push_str(" AND handled_at IS NULL");
        }
    }

    (sql, params)
}

fn sql_values_to_backend_params(values: &[SqlValue]) -> Vec<ToSqlValue<'_>> {
    values
        .iter()
        .map(|value| match value {
            SqlValue::Null => ToSqlValue::Null,
            SqlValue::Integer(value) => ToSqlValue::Integer(*value),
            SqlValue::Real(value) => ToSqlValue::Real(*value),
            SqlValue::Text(value) => ToSqlValue::Text(value.as_str()),
            SqlValue::Blob(value) => ToSqlValue::Blob(value.as_slice()),
        })
        .collect()
}

fn notification_history_record_from_backend_row(
    row: &[String],
) -> Result<NotificationHistoryRecord> {
    let reader = RowReader::new(row);
    let status_str = reader
        .string(7)
        .map_err(|err| storage_backend_error("Notification history status", err))?;
    let status: NotificationStatus = status_str.parse().unwrap_or(NotificationStatus::Pending);

    Ok(NotificationHistoryRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Notification history id", err))?,
        timestamp: reader
            .i64(1)
            .map_err(|err| storage_backend_error("Notification history timestamp", err))?,
        event_id: reader
            .optional_i64(2)
            .map_err(|err| storage_backend_error("Notification history event_id", err))?,
        channel: reader
            .string(3)
            .map_err(|err| storage_backend_error("Notification history channel", err))?,
        title: reader
            .string(4)
            .map_err(|err| storage_backend_error("Notification history title", err))?,
        body: reader
            .string(5)
            .map_err(|err| storage_backend_error("Notification history body", err))?,
        severity: reader
            .string(6)
            .map_err(|err| storage_backend_error("Notification history severity", err))?,
        status,
        error_message: reader
            .optional_string(8)
            .map_err(|err| storage_backend_error("Notification history error_message", err))?,
        acknowledged_at: reader
            .optional_i64(9)
            .map_err(|err| storage_backend_error("Notification history acknowledged_at", err))?,
        acknowledged_by: reader
            .optional_string(10)
            .map_err(|err| storage_backend_error("Notification history acknowledged_by", err))?,
        action_taken: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Notification history action_taken", err))?,
        retry_count: reader
            .i64(12)
            .map_err(|err| storage_backend_error("Notification history retry_count", err))?,
        metadata: reader
            .optional_string(13)
            .map_err(|err| storage_backend_error("Notification history metadata", err))?,
        created_at: reader
            .i64(14)
            .map_err(|err| storage_backend_error("Notification history created_at", err))?,
    })
}

fn build_notification_history_query(query: &NotificationHistoryQuery) -> (String, Vec<SqlValue>) {
    let mut sql = String::from(
        "SELECT id, timestamp, event_id, channel, title, body, severity,
                status, error_message, acknowledged_at, acknowledged_by,
                action_taken, retry_count, metadata, created_at
         FROM notification_history WHERE 1=1",
    );
    let mut params = Vec::new();

    if let Some(since) = query.since {
        params.push(SqlValue::Integer(since));
        sql.push_str(&format!(" AND timestamp >= ?{}", params.len()));
    }
    if let Some(until) = query.until {
        params.push(SqlValue::Integer(until));
        sql.push_str(&format!(" AND timestamp <= ?{}", params.len()));
    }
    if let Some(ref channel) = query.channel {
        params.push(SqlValue::Text(channel.clone()));
        sql.push_str(&format!(" AND channel = ?{}", params.len()));
    }
    if let Some(status) = query.status {
        params.push(SqlValue::Text(status.as_str().to_string()));
        sql.push_str(&format!(" AND status = ?{}", params.len()));
    }
    if let Some(event_id) = query.event_id {
        params.push(SqlValue::Integer(event_id));
        sql.push_str(&format!(" AND event_id = ?{}", params.len()));
    }

    sql.push_str(" ORDER BY timestamp DESC");

    let limit = query.limit.unwrap_or(100);
    params.push(SqlValue::Integer(limit as i64));
    sql.push_str(&format!(" LIMIT ?{}", params.len()));

    (sql, params)
}

fn query_notification_history_backend(
    backend: &dyn StorageBackend,
    query: &NotificationHistoryQuery,
) -> Result<Vec<NotificationHistoryRecord>> {
    let (sql, param_values) = build_notification_history_query(query);
    let params = sql_values_to_backend_params(&param_values);
    let rows = backend
        .query_map_typed(&sql, &params)
        .map_err(|err| storage_backend_error("Failed to query notification history", err))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(notification_history_record_from_backend_row(&row)?);
    }
    Ok(results)
}

fn get_notification_backend(
    backend: &dyn StorageBackend,
    id: i64,
) -> Result<NotificationHistoryRecord> {
    let row = backend
        .query_row_typed(
            "SELECT id, timestamp, event_id, channel, title, body, severity,
                status, error_message, acknowledged_at, acknowledged_by,
                action_taken, retry_count, metadata, created_at
         FROM notification_history WHERE id = ?1",
            &[ToSqlValue::Integer(id)],
        )
        .map_err(|err| storage_backend_error("Failed to get notification", err))?
        .ok_or_else(|| StorageError::Database(format!("Notification {id} not found")))?;
    notification_history_record_from_backend_row(&row)
}

// =============================================================================
// Account Operations (Synchronous)
// =============================================================================

/// Upsert an account record (insert or update by service+account_id)
fn upsert_account_backend(
    backend: &dyn StorageBackend,
    account: &crate::accounts::AccountRecord,
) -> Result<i64> {
    let row = backend
        .query_row_typed(
            "INSERT INTO accounts (
            account_id, service, name, percent_remaining, reset_at,
            tokens_used, tokens_remaining, tokens_limit,
            last_refreshed_at, last_used_at, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(service, account_id) DO UPDATE SET
            name = excluded.name,
            percent_remaining = excluded.percent_remaining,
            reset_at = excluded.reset_at,
            tokens_used = excluded.tokens_used,
            tokens_remaining = excluded.tokens_remaining,
            tokens_limit = excluded.tokens_limit,
            last_refreshed_at = excluded.last_refreshed_at,
            updated_at = excluded.updated_at
        RETURNING id",
            &[
                ToSqlValue::Text(&account.account_id),
                ToSqlValue::Text(&account.service),
                ToSqlValue::optional_text(account.name.as_deref()),
                ToSqlValue::Real(account.percent_remaining),
                ToSqlValue::optional_text(account.reset_at.as_deref()),
                ToSqlValue::optional_i64(account.tokens_used),
                ToSqlValue::optional_i64(account.tokens_remaining),
                ToSqlValue::optional_i64(account.tokens_limit),
                ToSqlValue::Integer(account.last_refreshed_at),
                ToSqlValue::optional_i64(account.last_used_at),
                ToSqlValue::Integer(account.created_at),
                ToSqlValue::Integer(account.updated_at),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to upsert account", err))?
        .ok_or_else(|| StorageError::Database("account upsert returned no id".to_string()))?;

    RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Account upsert id", err).into())
}

/// Update an account's last_used_at timestamp
fn update_account_last_used_backend(
    backend: &dyn StorageBackend,
    service: &str,
    account_id: &str,
    last_used_at: i64,
) -> Result<()> {
    let row = backend
        .query_row_typed(
            "UPDATE accounts SET last_used_at = ?1, updated_at = ?2
             WHERE service = ?3 AND account_id = ?4
             RETURNING 1",
            &[
                ToSqlValue::Integer(last_used_at),
                ToSqlValue::Integer(now_ms()),
                ToSqlValue::Text(service),
                ToSqlValue::Text(account_id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to update account last_used", err))?;

    if row.is_none() {
        return Err(
            StorageError::NotFound(format!("Account not found: {service}/{account_id}")).into(),
        );
    }
    Ok(())
}

/// Delete an account by service and account_id
fn delete_account_backend(
    backend: &dyn StorageBackend,
    service: &str,
    account_id: &str,
) -> Result<bool> {
    let row = backend
        .query_row_typed(
            "DELETE FROM accounts WHERE service = ?1 AND account_id = ?2
             RETURNING 1",
            &[ToSqlValue::Text(service), ToSqlValue::Text(account_id)],
        )
        .map_err(|err| storage_backend_error("Failed to delete account", err))?;

    Ok(row.is_some())
}

/// Get all accounts for a service (synchronous, read-only)
fn account_record_from_backend_row(row: &[String]) -> Result<crate::accounts::AccountRecord> {
    let reader = RowReader::new(row);
    Ok(crate::accounts::AccountRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Account id", err))?,
        account_id: reader
            .string(1)
            .map_err(|err| storage_backend_error("Account account_id", err))?,
        service: reader
            .string(2)
            .map_err(|err| storage_backend_error("Account service", err))?,
        name: reader
            .optional_string(3)
            .map_err(|err| storage_backend_error("Account name", err))?,
        percent_remaining: reader
            .f64(4)
            .map_err(|err| storage_backend_error("Account percent_remaining", err))?,
        reset_at: reader
            .optional_string(5)
            .map_err(|err| storage_backend_error("Account reset_at", err))?,
        tokens_used: reader
            .optional_i64(6)
            .map_err(|err| storage_backend_error("Account tokens_used", err))?,
        tokens_remaining: reader
            .optional_i64(7)
            .map_err(|err| storage_backend_error("Account tokens_remaining", err))?,
        tokens_limit: reader
            .optional_i64(8)
            .map_err(|err| storage_backend_error("Account tokens_limit", err))?,
        last_refreshed_at: reader
            .i64(9)
            .map_err(|err| storage_backend_error("Account last_refreshed_at", err))?,
        last_used_at: reader
            .optional_i64(10)
            .map_err(|err| storage_backend_error("Account last_used_at", err))?,
        created_at: reader
            .i64(11)
            .map_err(|err| storage_backend_error("Account created_at", err))?,
        updated_at: reader
            .i64(12)
            .map_err(|err| storage_backend_error("Account updated_at", err))?,
    })
}

fn get_accounts_by_service_backend(
    backend: &dyn StorageBackend,
    service: &str,
) -> Result<Vec<crate::accounts::AccountRecord>> {
    let rows = backend
        .query_map_typed(
            "SELECT id, account_id, service, name, percent_remaining, reset_at,
                    tokens_used, tokens_remaining, tokens_limit,
                    last_refreshed_at, last_used_at, created_at, updated_at
             FROM accounts
             WHERE service = ?1
             ORDER BY percent_remaining DESC, last_used_at ASC NULLS FIRST",
            &[ToSqlValue::Text(service)],
        )
        .map_err(|err| storage_backend_error("Failed to query accounts", err))?;
    rows.iter()
        .map(|row| account_record_from_backend_row(row))
        .collect()
}

fn get_account_backend(
    backend: &dyn StorageBackend,
    service: &str,
    account_id: &str,
) -> Result<Option<crate::accounts::AccountRecord>> {
    let row = backend
        .query_row_typed(
            "SELECT id, account_id, service, name, percent_remaining, reset_at,
                    tokens_used, tokens_remaining, tokens_limit,
                    last_refreshed_at, last_used_at, created_at, updated_at
             FROM accounts
             WHERE service = ?1 AND account_id = ?2",
            &[ToSqlValue::Text(service), ToSqlValue::Text(account_id)],
        )
        .map_err(|err| storage_backend_error("Failed to get account", err))?;
    row.as_deref()
        .map(account_record_from_backend_row)
        .transpose()
}

// =============================================================================
// Pane Reservation Sync Operations
// =============================================================================

/// Create a pane reservation, enforcing one-active-per-pane.
///
/// If an active, unexpired reservation already exists for the pane, returns
/// a conflict error. Expired reservations are treated as released.
fn create_reservation_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
    owner_kind: &str,
    owner_id: &str,
    reason: Option<&str>,
    ttl_ms: i64,
) -> Result<PaneReservation> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let now = now_ms();

    let existing = backend
        .query_row_typed(
            "SELECT id FROM pane_reservations
             WHERE pane_id = ?1 AND status = 'active' AND expires_at > ?2",
            &[ToSqlValue::Integer(pane_id_i64), ToSqlValue::Integer(now)],
        )
        .map_err(|err| storage_backend_error("Failed to check reservation", err))?;

    if let Some(row) = existing {
        let existing_id = RowReader::new(&row)
            .i64(0)
            .map_err(|err| storage_backend_error("Failed to parse existing reservation id", err))?;
        return Err(StorageError::ReservationConflict {
            pane_id,
            existing_id,
        }
        .into());
    }

    let ttl_ms = PaneReservationConfig::default().clamp_ttl(ttl_ms);
    let expires_at = now
        .checked_add(ttl_ms)
        .ok_or_else(|| StorageError::Database("Pane reservation expiry overflowed".to_string()))?;

    let row = backend
        .query_row_typed(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active')
             RETURNING id",
            &[
                ToSqlValue::Integer(pane_id_i64),
                ToSqlValue::Text(owner_kind),
                ToSqlValue::Text(owner_id),
                ToSqlValue::optional_text(reason),
                ToSqlValue::Integer(now),
                ToSqlValue::Integer(expires_at),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to create reservation", err))?
        .ok_or_else(|| StorageError::Database("reservation insert returned no id".to_string()))?;

    let id = RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Failed to parse reservation id", err))?;

    Ok(PaneReservation {
        id,
        pane_id,
        owner_kind: owner_kind.to_string(),
        owner_id: owner_id.to_string(),
        reason: reason.map(String::from),
        created_at: now,
        expires_at,
        released_at: None,
        status: "active".to_string(),
    })
}

fn release_reservation_backend(backend: &dyn StorageBackend, reservation_id: i64) -> Result<bool> {
    let now = now_ms();
    let updated_rows = backend
        .query_map_typed(
            "UPDATE pane_reservations SET status = 'released', released_at = ?1
             WHERE id = ?2 AND status = 'active'
             RETURNING 1",
            &[
                ToSqlValue::Integer(now),
                ToSqlValue::Integer(reservation_id),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to release reservation", err))?;

    Ok(!updated_rows.is_empty())
}

/// Get the active reservation for a pane (if any).
///
/// Only returns a reservation that is both status='active' and not expired.
fn pane_reservation_from_backend_row(row: &[String]) -> Result<PaneReservation> {
    let reader = RowReader::new(row);
    let pane_id_i64 = reader
        .i64(1)
        .map_err(|err| storage_backend_error("Pane reservation pane_id", err))?;
    let pane_id = backend_i64_to_u64(pane_id_i64, "pane_reservations.pane_id")
        .map_err(|err| storage_backend_error("Pane reservation pane_id", err))?;
    Ok(PaneReservation {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Pane reservation id", err))?,
        pane_id,
        owner_kind: reader
            .string(2)
            .map_err(|err| storage_backend_error("Pane reservation owner_kind", err))?,
        owner_id: reader
            .string(3)
            .map_err(|err| storage_backend_error("Pane reservation owner_id", err))?,
        reason: reader
            .optional_string(4)
            .map_err(|err| storage_backend_error("Pane reservation reason", err))?,
        created_at: reader
            .i64(5)
            .map_err(|err| storage_backend_error("Pane reservation created_at", err))?,
        expires_at: reader
            .i64(6)
            .map_err(|err| storage_backend_error("Pane reservation expires_at", err))?,
        released_at: reader
            .optional_i64(7)
            .map_err(|err| storage_backend_error("Pane reservation released_at", err))?,
        status: reader
            .string(8)
            .map_err(|err| storage_backend_error("Pane reservation status", err))?,
    })
}

fn get_active_reservation_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
) -> Result<Option<PaneReservation>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let now = now_ms();
    let row = backend
        .query_row_typed(
            "SELECT id, pane_id, owner_kind, owner_id, reason, created_at, expires_at, released_at, status
             FROM pane_reservations
             WHERE pane_id = ?1 AND status = 'active' AND expires_at > ?2",
            &[ToSqlValue::Integer(pane_id_i64), ToSqlValue::Integer(now)],
        )
        .map_err(|err| storage_backend_error("Failed to get reservation", err))?;
    row.as_deref()
        .map(pane_reservation_from_backend_row)
        .transpose()
}

fn list_active_reservations_backend(backend: &dyn StorageBackend) -> Result<Vec<PaneReservation>> {
    let now = now_ms();
    let rows = backend
        .query_map_typed(
            "SELECT id, pane_id, owner_kind, owner_id, reason, created_at, expires_at, released_at, status
             FROM pane_reservations
             WHERE status = 'active' AND expires_at > ?1
             ORDER BY created_at ASC",
            &[ToSqlValue::Integer(now)],
        )
        .map_err(|err| storage_backend_error("Failed to list reservations", err))?;
    rows.iter()
        .map(|row| pane_reservation_from_backend_row(row))
        .collect()
}

fn expire_stale_reservations_backend(backend: &dyn StorageBackend) -> Result<usize> {
    let now = now_ms();
    let expired_rows = backend
        .query_map_typed(
            "UPDATE pane_reservations SET status = 'released', released_at = ?1
             WHERE status = 'active' AND expires_at <= ?1
             RETURNING 1",
            &[ToSqlValue::Integer(now)],
        )
        .map_err(|err| storage_backend_error("Failed to expire reservations", err))?;

    Ok(expired_rows.len())
}

/// Insert an approval token through the storage backend.
fn insert_approval_token_backend(
    backend: &dyn StorageBackend,
    token: &ApprovalTokenRecord,
) -> Result<i64> {
    let pane_id_i64 = token
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;

    let row = backend
        .query_row_typed(
            "INSERT INTO approval_tokens (code_hash, created_at, expires_at, used_at, workspace_id,
         action_kind, pane_id, action_fingerprint, plan_hash, plan_version, risk_summary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         RETURNING id",
            &[
                ToSqlValue::Text(token.code_hash.as_str()),
                ToSqlValue::Integer(token.created_at),
                ToSqlValue::Integer(token.expires_at),
                ToSqlValue::optional_i64(token.used_at),
                ToSqlValue::Text(token.workspace_id.as_str()),
                ToSqlValue::Text(token.action_kind.as_str()),
                ToSqlValue::optional_i64(pane_id_i64),
                ToSqlValue::Text(token.action_fingerprint.as_str()),
                ToSqlValue::optional_text(token.plan_hash.as_deref()),
                ToSqlValue::optional_i64(token.plan_version.map(i64::from)),
                ToSqlValue::optional_text(token.risk_summary.as_deref()),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to insert approval token", err))?
        .ok_or_else(|| {
            StorageError::Database("approval token insert returned no id".to_string())
        })?;

    let id = RowReader::new(&row)
        .i64(0)
        .map_err(|err| storage_backend_error("Failed to parse approval token id", err))?;
    Ok(id)
}

/// Insert a prepared plan (writer-thread, backend-trait path).
///
/// br-ft-l1jgo writer-thread migration: replaces the legacy
/// `insert_prepared_plan_sync(&Connection, &PreparedPlanRecord)`
/// direct-rusqlite helper. Routes the `INSERT OR REPLACE` through the
/// trait surface using `execute_typed`. Same shape as the
/// `upsert_action_undo_backend` slice (81589276c). Called from the
/// writer-thread dispatcher inside `with_writer_backend(...)`.
fn insert_prepared_plan_backend(
    backend: &dyn StorageBackend,
    record: &PreparedPlanRecord,
) -> Result<()> {
    let pane_id_i64 = record
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;
    let requires = i64::from(record.requires_approval);
    let pane_id_value = match pane_id_i64 {
        Some(v) => ToSqlValue::Integer(v),
        None => ToSqlValue::Null,
    };
    let pane_uuid_value = match record.pane_uuid.as_deref() {
        Some(s) => ToSqlValue::Text(s),
        None => ToSqlValue::Null,
    };
    let params_json_value = match record.params_json.as_deref() {
        Some(s) => ToSqlValue::Text(s),
        None => ToSqlValue::Null,
    };
    let consumed_at_value = match record.consumed_at {
        Some(v) => ToSqlValue::Integer(v),
        None => ToSqlValue::Null,
    };
    execute_typed(
        backend,
        "INSERT OR REPLACE INTO prepared_plans
         (plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
          plan_json, requires_approval, created_at, expires_at, consumed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        &[
            ToSqlValue::Text(record.plan_id.as_str()),
            ToSqlValue::Text(record.plan_hash.as_str()),
            ToSqlValue::Text(record.workspace_id.as_str()),
            ToSqlValue::Text(record.action_kind.as_str()),
            pane_id_value,
            pane_uuid_value,
            params_json_value,
            ToSqlValue::Text(record.plan_json.as_str()),
            ToSqlValue::Integer(requires),
            ToSqlValue::Integer(record.created_at),
            ToSqlValue::Integer(record.expires_at),
            consumed_at_value,
        ],
    )
    .map_err(|err| storage_backend_error("Failed to insert prepared plan", err))?;
    Ok(())
}

/// Consume a prepared plan by plan_id through the storage backend.
fn consume_prepared_plan_backend(
    backend: &dyn StorageBackend,
    plan_id: &str,
    now_ms: i64,
) -> Result<Option<PreparedPlanRecord>> {
    let row = backend
        .query_row_typed(
            "UPDATE prepared_plans
             SET consumed_at = ?2
             WHERE plan_id = ?1
               AND consumed_at IS NULL
               AND expires_at >= ?2
             RETURNING plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
                       plan_json, requires_approval, created_at, expires_at, consumed_at",
            &[ToSqlValue::Text(plan_id), ToSqlValue::Integer(now_ms)],
        )
        .map_err(|err| storage_backend_error("Failed to consume prepared plan", err))?;

    row.as_deref()
        .map(prepared_plan_from_backend_row)
        .transpose()
}

/// Consume an approval token if it matches scope and is valid through the storage backend.
#[allow(clippy::too_many_arguments)]
fn consume_approval_token_backend(
    backend: &dyn StorageBackend,
    code_hash: &str,
    workspace_id: &str,
    action_kind: &str,
    pane_id: Option<u64>,
    action_fingerprint: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let now = now_ms();
    let pane_id_i64 = pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;

    let mut sql = String::from(
        "UPDATE approval_tokens
         SET used_at = ?5
         WHERE id = (
             SELECT id FROM approval_tokens
             WHERE code_hash = ?1
               AND workspace_id = ?2
               AND action_kind = ?3
               AND action_fingerprint = ?4
               AND used_at IS NULL
               AND expires_at >= ?5",
    );
    let mut params = vec![
        ToSqlValue::Text(code_hash),
        ToSqlValue::Text(workspace_id),
        ToSqlValue::Text(action_kind),
        ToSqlValue::Text(action_fingerprint),
        ToSqlValue::Integer(now),
    ];
    if let Some(pid) = pane_id_i64 {
        sql.push_str(" AND pane_id = ?6");
        params.push(ToSqlValue::Integer(pid));
    }
    sql.push_str(
        " LIMIT 1
         )
           AND used_at IS NULL
         RETURNING id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
                   pane_id, action_fingerprint, plan_hash, plan_version, risk_summary",
    );

    let row = backend
        .query_row_typed(&sql, &params)
        .map_err(|err| storage_backend_error("Failed to consume approval token", err))?;

    row.as_deref()
        .map(approval_token_from_backend_row)
        .transpose()
}

/// Consume an approval token by code hash only, without fingerprint validation (synchronous).
///
/// # Security note: token scope is NOT validated
///
/// This function matches tokens solely on `code_hash` + `workspace_id`. It does
/// **not** verify that the token's `action_kind`, `pane_id`, or
/// `action_fingerprint` match the action being approved. A token originally
/// issued for one action kind or pane can be consumed by a different action if
/// the caller only has the code hash.
///
/// **Callers that have the original policy context available should prefer
/// [`consume_approval_token`] (the fingerprint-validated version) instead.**
/// This code-only variant exists for the CLI `ft approve` path where the user
/// provides a short approval code and the full action context is not available
/// at consumption time.
fn consume_approval_token_by_code_backend(
    backend: &dyn StorageBackend,
    code_hash: &str,
    workspace_id: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let now = now_ms();

    let row = backend
        .query_row_typed(
            "UPDATE approval_tokens
             SET used_at = ?3
             WHERE id = (
                 SELECT id FROM approval_tokens
                 WHERE code_hash = ?1
                   AND workspace_id = ?2
                   AND used_at IS NULL
                   AND expires_at >= ?3
                 LIMIT 1
             )
             AND used_at IS NULL
             RETURNING id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
                       pane_id, action_fingerprint, plan_hash, plan_version, risk_summary",
            &[
                ToSqlValue::Text(code_hash),
                ToSqlValue::Text(workspace_id),
                ToSqlValue::Integer(now),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to consume approval token", err))?;

    if let Some(row) = row {
        let record = approval_token_from_backend_row(&row)?;
        // Warn that this token was consumed without scope validation — the
        // token's action_kind/pane_id may not match what the caller intends
        // to approve. This is expected for the CLI `ft approve` path but
        // should be auditable.
        tracing::warn!(
            token_id = record.id,
            token_action_kind = %record.action_kind,
            token_pane_id = ?record.pane_id,
            workspace_id = %workspace_id,
            "approval token consumed by code-only path without action scope \
             validation; token's action_kind/pane_id were not checked against \
             the current approval context"
        );
        return Ok(Some(record));
    }

    Ok(None)
}

// =============================================================================
// Read Operations (called from spawn_blocking)
// =============================================================================

/// Validate FTS5 query syntax by attempting a limited search.
///
/// [ft-76d9i] Use `SELECT 1 ... LIMIT 1` rather than `SELECT COUNT(*)
/// ... LIMIT 1`. The pre-fix shape was a foot-gun: SQLite's `LIMIT`
/// caps OUTPUT rows, not the work the planner does to compute the
/// aggregate, and `COUNT(*)` is one output row regardless. So the
/// pre-fix preflight scanned every matching FTS5 row before
/// `search_fts_with_snippets` then scanned the same set a SECOND
/// time for the real BM25/snippet query — doubling the read-side
/// cost for any broad term ("error", "warn", etc.) and holding the
/// read connection twice as long under load. With `SELECT 1 ...
/// LIMIT 1` the planner short-circuits on the first matching rowid
/// (or returns no-rows for an empty match set), which is what
/// callers actually want from a syntax probe.
///
/// FTS5 syntax errors surface at execution time (xFilter), not
/// prepare time, so we can't replace this with a pure prepare-only
/// check — but we can stop the probe after the first match.
fn validate_fts_query(conn: &Connection, query: &str) -> Result<()> {
    let result = conn.query_row(
        "SELECT 1 FROM output_segments_fts WHERE output_segments_fts MATCH ?1 LIMIT 1",
        [query],
        |_| Ok(()),
    );

    match result {
        // Match found on the first row — the query is syntactically valid.
        Ok(()) => Ok(()),
        // Empty match set is also syntactically valid: a well-formed query
        // simply found nothing. The pre-fix `COUNT(*)` shape returned
        // Ok(0) here; we mirror that semantics by promoting no-rows to Ok.
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(msg))) => {
            // FTS5 syntax errors have specific error codes
            Err(StorageError::FtsQueryError(format!(
                "Invalid FTS5 query syntax: {msg}. \
                 Valid syntax includes: simple words, \"phrases\", prefix*, AND/OR/NOT operators. \
                 SQLite error code: {}",
                err.extended_code
            ))
            .into())
        }
        Err(e) => Err(StorageError::FtsQueryError(format!("Query validation failed: {e}")).into()),
    }
}

/// Search using FTS5 with snippet extraction and BM25 scores
///
/// Returns structured results with:
/// - The matching segment data
/// - A snippet with highlighted matching terms (using configurable markers)
/// - Highlighted content (full segment with markers)
/// - The BM25 relevance score (lower = more relevant)
///
/// # Two-stage query path (ft-okhhj)
///
/// When `include_snippets=true` the function runs two queries:
///
/// 1. **Rank stage** — selects only `(rowid, score)` (and the captured_at /
///    id tie-break columns) ordered by BM25, with `LIMIT ?` applied.
///    No content is materialized; no snippet/highlight functions run.
/// 2. **Hydration stage** — re-queries `output_segments` and the FTS table
///    for the top-N rowids returned by stage 1. `snippet()` and `highlight()`
///    are computed only for those N rows, not for every matching candidate.
///
/// The single-query shape used to compute `snippet(...)` and `highlight(...)`
/// for every matching row before sorting + LIMIT, which materialized the
/// full highlighted content even for rows that never made the cutoff.
/// On broad MATCH queries over panes with thousands of matching segments
/// this dominated query cost; the two-stage path bounds it to N.
///
/// When `include_snippets=false` the function falls back to a single-stage
/// query that still skips the snippet/highlight functions — there's no
/// asymmetric work to split out.
#[allow(clippy::cast_sign_loss)]
fn search_fts_with_snippets(
    conn: &Connection,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<SearchResult>> {
    // Validate query syntax first for better error messages
    validate_fts_query(conn, query)?;

    let limit = options.limit.unwrap_or(100);
    let include_snippets = options.include_snippets.unwrap_or(true);
    // include_highlights defaults to whatever include_snippets is — when
    // snippets are off there's nothing to highlight; when snippets are on
    // callers historically got both unless they opt out.
    let include_highlights = options.include_highlights.unwrap_or(include_snippets);

    if !include_snippets {
        return search_fts_rank_only(conn, query, options, limit);
    }

    // Stage 1: rank only — cheap query that returns just the ordered ids
    // we'll hydrate. No content / snippet / highlight materialization.
    let ranked = search_fts_rank_stage(conn, query, options, limit)?;
    if ranked.is_empty() {
        return Ok(Vec::new());
    }

    // Stage 2: hydrate snippet/highlight for the top-N ids only.
    let max_tokens = options.snippet_max_tokens.unwrap_or(64);
    let prefix = options.highlight_prefix.as_deref().unwrap_or(">>>");
    let suffix = options.highlight_suffix.as_deref().unwrap_or("<<<");

    let hydrated = search_fts_hydrate_stage(
        conn,
        query,
        &ranked,
        prefix,
        suffix,
        max_tokens,
        include_highlights,
    )?;

    tracing::trace!(
        rank_count = ranked.len(),
        hydrate_count = hydrated.len(),
        "search_fts_with_snippets two-stage path"
    );

    // Re-stitch hydrated rows in rank order. A row can only be missing if
    // it was concurrently deleted between the two queries; drop it
    // silently rather than fail the whole search.
    let mut results = Vec::with_capacity(ranked.len());
    for (id, score) in &ranked {
        if let Some((segment, snippet, highlight)) = hydrated.get(id).cloned() {
            results.push(SearchResult {
                segment,
                snippet,
                highlight,
                score: *score,
            });
        }
    }

    Ok(results)
}

type FtsHydratedRows = std::collections::HashMap<i64, (Segment, Option<String>, Option<String>)>;

fn validate_fts_query_backend(backend: &dyn StorageBackend, query: &str) -> Result<()> {
    let result = backend.query_row_cells(
        "SELECT 1 FROM output_segments_fts WHERE output_segments_fts MATCH ?1 LIMIT 1",
        &[ToSqlValue::Text(query)],
    );

    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            let msg = err.to_string();
            if looks_like_fts_syntax_error(&msg) {
                Err(StorageError::FtsQueryError(format!(
                    "Invalid FTS5 query syntax: {msg}. \
                     Valid syntax includes: simple words, \"phrases\", prefix*, AND/OR/NOT operators."
                ))
                .into())
            } else {
                Err(StorageError::FtsQueryError(format!("Query validation failed: {msg}")).into())
            }
        }
    }
}

fn looks_like_fts_syntax_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("fts5")
        || lower.contains("syntax error")
        || lower.contains("malformed match")
        || lower.contains("unterminated")
}

/// br-ft-l1jgo: trait-backed sibling of [`search_fts_with_snippets`].
fn search_fts_with_snippets_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<SearchResult>> {
    validate_fts_query_backend(backend, query)?;

    let limit = options.limit.unwrap_or(100);
    let include_snippets = options.include_snippets.unwrap_or(true);
    let include_highlights = options.include_highlights.unwrap_or(include_snippets);

    if !include_snippets {
        return search_fts_rank_only_backend(backend, query, options, limit);
    }

    let ranked = search_fts_rank_stage_backend(backend, query, options, limit)?;
    if ranked.is_empty() {
        return Ok(Vec::new());
    }

    let max_tokens = options.snippet_max_tokens.unwrap_or(64);
    let prefix = options.highlight_prefix.as_deref().unwrap_or(">>>");
    let suffix = options.highlight_suffix.as_deref().unwrap_or("<<<");

    let hydrated = search_fts_hydrate_stage_backend(
        backend,
        query,
        &ranked,
        prefix,
        suffix,
        max_tokens,
        include_highlights,
    )?;

    tracing::trace!(
        rank_count = ranked.len(),
        hydrate_count = hydrated.len(),
        "search_fts_with_snippets backend two-stage path"
    );

    let mut results = Vec::with_capacity(ranked.len());
    for (id, score) in &ranked {
        if let Some((segment, snippet, highlight)) = hydrated.get(id).cloned() {
            results.push(SearchResult {
                segment,
                snippet,
                highlight,
                score: *score,
            });
        }
    }

    Ok(results)
}

fn append_fts_filter_backend_params(
    sql: &mut String,
    params: &mut Vec<ToSqlValue<'_>>,
    options: &SearchOptions,
) -> Result<()> {
    if let Some(pane_id) = options.pane_id {
        sql.push_str(" AND s.pane_id = ?");
        params.push(ToSqlValue::Integer(u64_to_i64(pane_id, "pane_id")?));
    }
    if let Some(since) = options.since {
        sql.push_str(" AND s.captured_at >= ?");
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = options.until {
        sql.push_str(" AND s.captured_at <= ?");
        params.push(ToSqlValue::Integer(until));
    }
    Ok(())
}

fn search_fts_rank_stage_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
    limit: usize,
) -> Result<Vec<(i64, f64)>> {
    let mut sql = String::from(
        "SELECT s.id, bm25(output_segments_fts) as score, s.captured_at
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ?1",
    );
    let mut params = vec![ToSqlValue::Text(query)];
    append_fts_filter_backend_params(&mut sql, &mut params, options)?;

    sql.push_str(" ORDER BY score ASC, s.captured_at ASC, s.id ASC LIMIT ?");
    params.push(ToSqlValue::Integer(usize_to_i64(limit, "limit")?));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| StorageError::FtsQueryError(format!("Rank query failed: {err}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let reader = CellRowReader::new(&row);
        let id = reader
            .i64(0)
            .map_err(|err| storage_backend_error("Rank row id", err))?;
        let score = reader
            .f64(1)
            .map_err(|err| storage_backend_error("Rank row score", err))?;
        out.push((id, score));
    }
    Ok(out)
}

fn search_fts_hydrate_stage_backend(
    backend: &dyn StorageBackend,
    query: &str,
    ranked: &[(i64, f64)],
    prefix: &str,
    suffix: &str,
    max_tokens: usize,
    include_highlights: bool,
) -> Result<FtsHydratedRows> {
    let placeholders = std::iter::repeat_n("?", ranked.len())
        .collect::<Vec<_>>()
        .join(",");

    let highlight_col = if include_highlights {
        "highlight(output_segments_fts, 0, ?, ?) as highlight"
    } else {
        "NULL as highlight"
    };

    let sql = format!(
        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                snippet(output_segments_fts, 0, ?, ?, '...', ?) as snippet,
                {highlight_col}
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ? AND s.id IN ({placeholders})"
    );

    let mut params = vec![
        ToSqlValue::Text(prefix),
        ToSqlValue::Text(suffix),
        ToSqlValue::Integer(usize_to_i64(max_tokens, "max_tokens")?),
    ];
    if include_highlights {
        params.push(ToSqlValue::Text(prefix));
        params.push(ToSqlValue::Text(suffix));
    }
    params.push(ToSqlValue::Text(query));
    for (id, _) in ranked {
        params.push(ToSqlValue::Integer(*id));
    }

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| StorageError::FtsQueryError(format!("Hydrate query failed: {err}")))?;

    let mut out = std::collections::HashMap::with_capacity(ranked.len());
    for row in rows {
        let segment = segment_from_backend_cells(&row)?;
        let reader = CellRowReader::new(&row);
        let snippet = reader
            .optional_string(7)
            .map_err(|err| storage_backend_error("Hydrate row snippet", err))?;
        let highlight = reader
            .optional_string(8)
            .map_err(|err| storage_backend_error("Hydrate row highlight", err))?;
        out.insert(segment.id, (segment, snippet, highlight));
    }
    Ok(out)
}

fn search_fts_rank_only_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let mut sql = String::from(
        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                NULL as snippet,
                NULL as highlight,
                bm25(output_segments_fts) as score
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ?1",
    );
    let mut params = vec![ToSqlValue::Text(query)];
    append_fts_filter_backend_params(&mut sql, &mut params, options)?;

    sql.push_str(" ORDER BY score ASC, s.captured_at ASC, s.id ASC LIMIT ?");
    params.push(ToSqlValue::Integer(usize_to_i64(limit, "limit")?));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| StorageError::FtsQueryError(format!("Query failed: {err}")))?;

    let mut results = Vec::with_capacity(rows.len());
    for row in rows {
        results.push(search_result_from_backend_cells(&row)?);
    }

    Ok(results)
}

fn search_result_from_backend_cells(row: &[SqlCell]) -> Result<SearchResult> {
    let segment = segment_from_backend_cells(row)?;
    let reader = CellRowReader::new(row);
    let snippet = reader
        .optional_string(7)
        .map_err(|err| storage_backend_error("Search row snippet", err))?;
    let highlight = reader
        .optional_string(8)
        .map_err(|err| storage_backend_error("Search row highlight", err))?;
    let score = reader
        .f64(9)
        .map_err(|err| storage_backend_error("Search row score", err))?;

    Ok(SearchResult {
        segment,
        snippet,
        highlight,
        score,
    })
}

/// Stage 1 of the two-stage FTS search path (ft-okhhj). Returns the top-N
/// `(rowid, bm25_score)` pairs in deterministic order without materializing
/// content, snippet, or highlight.
fn search_fts_rank_stage(
    conn: &Connection,
    query: &str,
    options: &SearchOptions,
    limit: usize,
) -> Result<Vec<(i64, f64)>> {
    let mut sql = String::from(
        "SELECT s.id, bm25(output_segments_fts) as score, s.captured_at
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ?1",
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.to_string())];

    if let Some(pane_id) = options.pane_id {
        sql.push_str(" AND s.pane_id = ?");
        params_vec.push(Box::new(u64_to_i64(pane_id, "pane_id")?));
    }
    if let Some(since) = options.since {
        sql.push_str(" AND s.captured_at >= ?");
        params_vec.push(Box::new(since));
    }
    if let Some(until) = options.until {
        sql.push_str(" AND s.captured_at <= ?");
        params_vec.push(Box::new(until));
    }

    sql.push_str(" ORDER BY score ASC, s.captured_at ASC, s.id ASC LIMIT ?");
    params_vec.push(Box::new(usize_to_i64(limit, "limit")?));

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::FtsQueryError(format!("Failed to prepare rank query: {e}")))?;

    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            let id: i64 = row.get(0)?;
            let score: f64 = row.get(1)?;
            Ok((id, score))
        })
        .map_err(|e| StorageError::FtsQueryError(format!("Rank query failed: {e}")))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| StorageError::Database(format!("Rank row error: {e}")))?);
    }
    Ok(out)
}

/// Stage 2 of the two-stage FTS search path (ft-okhhj). Hydrates the
/// segment row plus snippet (and optionally highlight) for the given
/// pre-ranked ids. The MATCH clause is required because `snippet()` and
/// `highlight()` are FTS5 auxiliary functions — they need MATCH context
/// to know which terms to mark.
#[allow(clippy::cast_sign_loss)]
fn search_fts_hydrate_stage(
    conn: &Connection,
    query: &str,
    ranked: &[(i64, f64)],
    prefix: &str,
    suffix: &str,
    max_tokens: usize,
    include_highlights: bool,
) -> Result<FtsHydratedRows> {
    let placeholders = std::iter::repeat_n("?", ranked.len())
        .collect::<Vec<_>>()
        .join(",");

    // Highlight column toggled at SQL build time so the FTS5 highlight()
    // function isn't invoked at all when callers opt out.
    let highlight_col = if include_highlights {
        "highlight(output_segments_fts, 0, ?, ?) as highlight"
    } else {
        "NULL as highlight"
    };

    let sql = format!(
        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                snippet(output_segments_fts, 0, ?, ?, '...', ?) as snippet,
                {highlight_col}
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ? AND s.id IN ({placeholders})"
    );

    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // Snippet args: prefix, suffix, max_tokens.
    params_vec.push(Box::new(prefix.to_string()));
    params_vec.push(Box::new(suffix.to_string()));
    params_vec.push(Box::new(usize_to_i64(max_tokens, "max_tokens")?));
    if include_highlights {
        // Highlight args: prefix, suffix.
        params_vec.push(Box::new(prefix.to_string()));
        params_vec.push(Box::new(suffix.to_string()));
    }
    // MATCH query.
    params_vec.push(Box::new(query.to_string()));
    // IN-clause ids.
    for (id, _) in ranked {
        params_vec.push(Box::new(*id));
    }

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        StorageError::FtsQueryError(format!("Failed to prepare hydrate query: {e}"))
    })?;

    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            let id: i64 = row.get(0)?;
            let segment = Segment {
                id,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            };
            let snippet: Option<String> = row.get(7)?;
            let highlight: Option<String> = row.get(8)?;
            Ok((id, segment, snippet, highlight))
        })
        .map_err(|e| StorageError::FtsQueryError(format!("Hydrate query failed: {e}")))?;

    let mut out = std::collections::HashMap::with_capacity(ranked.len());
    for row in rows {
        let (id, segment, snippet, highlight) =
            row.map_err(|e| StorageError::Database(format!("Hydrate row error: {e}")))?;
        out.insert(id, (segment, snippet, highlight));
    }
    Ok(out)
}

/// Single-stage path for callers that opt out of snippets entirely.
/// Kept separate from the two-stage path so the snippet=true case can stay
/// focused; this query still doesn't invoke snippet()/highlight() so there's
/// no asymmetric work to split.
#[allow(clippy::cast_sign_loss)]
fn search_fts_rank_only(
    conn: &Connection,
    query: &str,
    options: &SearchOptions,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let mut sql = String::from(
        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                NULL as snippet,
                NULL as highlight,
                bm25(output_segments_fts) as score
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ?1",
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.to_string())];

    if let Some(pane_id) = options.pane_id {
        sql.push_str(" AND s.pane_id = ?");
        params_vec.push(Box::new(u64_to_i64(pane_id, "pane_id")?));
    }
    if let Some(since) = options.since {
        sql.push_str(" AND s.captured_at >= ?");
        params_vec.push(Box::new(since));
    }
    if let Some(until) = options.until {
        sql.push_str(" AND s.captured_at <= ?");
        params_vec.push(Box::new(until));
    }

    sql.push_str(" ORDER BY score ASC, s.captured_at ASC, s.id ASC LIMIT ?");
    params_vec.push(Box::new(usize_to_i64(limit, "limit")?));

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::FtsQueryError(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok(SearchResult {
                segment: Segment {
                    id: row.get(0)?,
                    pane_id: {
                        let val: i64 = row.get(1)?;
                        #[allow(clippy::cast_sign_loss)]
                        {
                            val as u64
                        }
                    },
                    seq: {
                        let val: i64 = row.get(2)?;
                        #[allow(clippy::cast_sign_loss)]
                        {
                            val as u64
                        }
                    },
                    content: row.get(3)?,
                    content_len: {
                        let val: i64 = row.get(4)?;
                        i64_to_usize(val)?
                    },
                    content_hash: row.get(5)?,
                    captured_at: row.get(6)?,
                },
                snippet: row.get(7)?,
                highlight: row.get(8)?,
                score: row.get(9)?,
            })
        })
        .map_err(|e| StorageError::FtsQueryError(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn search_mode_label(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Lexical => "lexical",
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
    }
}

fn reciprocal_rank_score(rank: usize) -> f32 {
    1.0 / (rank as f32 + 1.0)
}

#[allow(clippy::cast_precision_loss)]
fn rrf_component_contribution(rank: usize, rrf_k: u32, weight: f32) -> f64 {
    if weight <= 0.0 {
        return 0.0;
    }
    f64::from(weight) / (f64::from(rrf_k) + rank as f64 + 1.0)
}

fn encode_f32_embedding_blob(vector: &[f32]) -> Result<Vec<u8>> {
    if vector.iter().any(|v| !v.is_finite()) {
        return Err(StorageError::Database(
            "Embedding vector contains non-finite values".to_string(),
        )
        .into());
    }

    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(out)
}

fn decode_f32_embedding_blob(blob: &[u8], dimension: usize) -> Result<Vec<f32>> {
    let expected_len = dimension
        .checked_mul(4)
        .ok_or_else(|| StorageError::Database("Embedding dimension overflow".to_string()))?;
    if blob.len() != expected_len {
        return Err(StorageError::Database(format!(
            "Invalid embedding byte length: expected {expected_len}, got {}",
            blob.len()
        ))
        .into());
    }

    let mut values = Vec::with_capacity(dimension);
    for chunk in blob.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(StorageError::Database(
                "Embedding row contains non-finite values".to_string(),
            )
            .into());
        }
        values.push(value);
    }
    Ok(values)
}

fn cosine_similarity_f32(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot = x.mul_add(y, dot);
        norm_a = x.mul_add(x, norm_a);
        norm_b = y.mul_add(y, norm_b);
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom <= f32::EPSILON {
        return None;
    }
    Some(dot / denom)
}

#[derive(Debug, Clone)]
struct SemanticLaneResolution {
    hits: Vec<SemanticSearchHit>,
    unavailable_reason: Option<String>,
    cache_hit: bool,
    latency_ms: u64,
    rows_scanned: usize,
    budget_state: String,
    backoff_until_ms: Option<i64>,
}

#[allow(dead_code)]
fn search_semantic_sync_with_scan_limit(
    conn: &Connection,
    embedder_id: &str,
    query_vector: &[f32],
    options: &SearchOptions,
    scan_limit_rows: Option<usize>,
) -> Result<(Vec<SemanticSearchHit>, usize)> {
    if query_vector.is_empty() {
        return Ok((Vec::new(), 0));
    }
    if query_vector.iter().any(|v| !v.is_finite()) {
        return Err(
            StorageError::Database("Query vector contains non-finite values".to_string()).into(),
        );
    }

    let dimension = usize_to_i64(query_vector.len(), "query_vector dimension")?;
    let limit = options.limit.unwrap_or(100);

    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(embedder_id.to_string()), Box::new(dimension)];
    let mut sql = String::from(
        "SELECT se.segment_id, se.vector
         FROM segment_embeddings se
         JOIN output_segments s ON s.id = se.segment_id
         WHERE se.embedder_id = ?1
           AND se.dimension = ?2",
    );

    if let Some(pane_id) = options.pane_id {
        sql.push_str(" AND s.pane_id = ?");
        params_vec.push(Box::new(u64_to_i64(pane_id, "pane_id")?));
    }
    if let Some(since) = options.since {
        sql.push_str(" AND s.captured_at >= ?");
        params_vec.push(Box::new(since));
    }
    if let Some(until) = options.until {
        sql.push_str(" AND s.captured_at <= ?");
        params_vec.push(Box::new(until));
    }

    // Stable base order before similarity sort.
    sql.push_str(" ORDER BY s.id ASC");
    if let Some(scan_limit) = scan_limit_rows {
        let bounded_scan_limit = scan_limit.max(limit).max(1);
        sql.push_str(" LIMIT ?");
        params_vec.push(Box::new(usize_to_i64(
            bounded_scan_limit,
            "semantic scan limit",
        )?));
    }

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("semantic_search prepare failed: {e}")))?;

    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            let segment_id: i64 = row.get(0)?;
            let vector_blob: Vec<u8> = row.get(1)?;
            Ok((segment_id, vector_blob))
        })
        .map_err(|e| StorageError::Database(format!("semantic_search query failed: {e}")))?;

    let mut hits = Vec::new();
    let mut rows_scanned = 0usize;
    for row in rows {
        rows_scanned = rows_scanned.saturating_add(1);
        let (segment_id, vector_blob) =
            row.map_err(|e| StorageError::Database(format!("semantic_search row error: {e}")))?;
        let candidate = decode_f32_embedding_blob(&vector_blob, query_vector.len())?;
        let Some(score) = cosine_similarity_f32(query_vector, &candidate) else {
            continue;
        };
        hits.push(SemanticSearchHit {
            segment_id,
            score: f64::from(score),
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.segment_id.cmp(&b.segment_id))
    });
    hits.truncate(limit);
    Ok((hits, rows_scanned))
}

/// br-ft-l1jgo: trait-typed sibling of [`search_semantic_sync_with_scan_limit`].
fn search_semantic_backend(
    backend: &dyn StorageBackend,
    embedder_id: &str,
    query_vector: &[f32],
    options: &SearchOptions,
) -> Result<Vec<SemanticSearchHit>> {
    let (hits, _) =
        search_semantic_backend_with_scan_limit(backend, embedder_id, query_vector, options, None)?;
    Ok(hits)
}

fn search_semantic_backend_with_scan_limit(
    backend: &dyn StorageBackend,
    embedder_id: &str,
    query_vector: &[f32],
    options: &SearchOptions,
    scan_limit_rows: Option<usize>,
) -> Result<(Vec<SemanticSearchHit>, usize)> {
    if query_vector.is_empty() {
        return Ok((Vec::new(), 0));
    }
    if query_vector.iter().any(|v| !v.is_finite()) {
        return Err(
            StorageError::Database("Query vector contains non-finite values".to_string()).into(),
        );
    }

    let dimension = usize_to_i64(query_vector.len(), "query_vector dimension")?;
    let limit = options.limit.unwrap_or(100);

    let mut params = vec![
        ToSqlValue::Text(embedder_id),
        ToSqlValue::Integer(dimension),
    ];
    let mut sql = String::from(
        "SELECT se.segment_id, se.vector
         FROM segment_embeddings se
         JOIN output_segments s ON s.id = se.segment_id
         WHERE se.embedder_id = ?1
           AND se.dimension = ?2",
    );

    if let Some(pane_id) = options.pane_id {
        sql.push_str(" AND s.pane_id = ?");
        params.push(ToSqlValue::Integer(u64_to_i64(pane_id, "pane_id")?));
    }
    if let Some(since) = options.since {
        sql.push_str(" AND s.captured_at >= ?");
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = options.until {
        sql.push_str(" AND s.captured_at <= ?");
        params.push(ToSqlValue::Integer(until));
    }

    // Stable base order before similarity sort.
    sql.push_str(" ORDER BY s.id ASC");
    if let Some(scan_limit) = scan_limit_rows {
        let bounded_scan_limit = scan_limit.max(limit).max(1);
        sql.push_str(" LIMIT ?");
        params.push(ToSqlValue::Integer(usize_to_i64(
            bounded_scan_limit,
            "semantic scan limit",
        )?));
    }

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("semantic_search query", err))?;

    let mut hits = Vec::new();
    let mut rows_scanned = 0usize;
    for row in rows {
        rows_scanned = rows_scanned.saturating_add(1);
        let reader = CellRowReader::new(&row);
        let segment_id = reader
            .i64(0)
            .map_err(|err| storage_backend_error("semantic_search row segment_id", err))?;
        let vector_blob = reader
            .blob(1)
            .map_err(|err| storage_backend_error("semantic_search row vector", err))?;
        let candidate = decode_f32_embedding_blob(vector_blob, query_vector.len())?;
        let Some(score) = cosine_similarity_f32(query_vector, &candidate) else {
            continue;
        };
        hits.push(SemanticSearchHit {
            segment_id,
            score: f64::from(score),
        });
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.segment_id.cmp(&b.segment_id))
    });
    hits.truncate(limit);
    Ok((hits, rows_scanned))
}

fn resolve_semantic_lane_backend(
    backend: &dyn StorageBackend,
    options: &SearchOptions,
    embedder_id: &str,
    query_vector: &[f32],
    requested_mode: SearchMode,
    semantic_budget_state: &Arc<Mutex<SemanticBudgetState>>,
) -> Result<SemanticLaneResolution> {
    if !matches!(requested_mode, SearchMode::Hybrid | SearchMode::Semantic) {
        return Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: None,
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state: "disabled".to_string(),
            backoff_until_ms: None,
        });
    }

    if query_vector.is_empty() {
        if let Ok(mut state) = semantic_budget_state.lock() {
            state.note_semantic_fallback_reason("semantic_query_empty");
        }
        return Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: Some("semantic_query_empty".to_string()),
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state: "invalid_query".to_string(),
            backoff_until_ms: None,
        });
    }

    if query_vector.iter().any(|v| !v.is_finite()) {
        if let Ok(mut state) = semantic_budget_state.lock() {
            state.note_semantic_fallback_reason("semantic_query_non_finite");
        }
        return Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: Some("semantic_query_non_finite".to_string()),
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state: "invalid_query".to_string(),
            backoff_until_ms: None,
        });
    }

    let now = now_ms();
    let decision = match semantic_budget_state.lock() {
        Ok(mut state) => state.begin_semantic_lane(now, options, embedder_id, query_vector),
        Err(_) => SemanticBudgetDecision::Skip {
            reason: "semantic_budget_poisoned".to_string(),
            budget_state: "error".to_string(),
            backoff_until_ms: None,
        },
    };

    match decision {
        SemanticBudgetDecision::UseCache { hits } => {
            let unavailable_reason = if hits.is_empty() {
                Some("semantic_no_hits".to_string())
            } else {
                None
            };
            if unavailable_reason.is_some()
                && let Ok(mut state) = semantic_budget_state.lock()
            {
                state.note_semantic_fallback_reason("semantic_no_hits");
            }
            Ok(SemanticLaneResolution {
                hits,
                unavailable_reason,
                cache_hit: true,
                latency_ms: 0,
                rows_scanned: 0,
                budget_state: "cache_hit".to_string(),
                backoff_until_ms: None,
            })
        }
        SemanticBudgetDecision::Skip {
            reason,
            budget_state,
            backoff_until_ms,
        } => Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: Some(reason),
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state,
            backoff_until_ms,
        }),
        SemanticBudgetDecision::Execute { key, max_scan_rows } => {
            let started = Instant::now();
            let (hits, rows_scanned) = search_semantic_backend_with_scan_limit(
                backend,
                embedder_id,
                query_vector,
                options,
                Some(max_scan_rows),
            )?;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let now_after = now_ms();
            let backoff_until_ms = match semantic_budget_state.lock() {
                Ok(mut state) => {
                    state.complete_semantic_lane(now_after, key, &hits, elapsed_ms, rows_scanned)
                }
                Err(_) => None,
            };

            let unavailable_reason = if hits.is_empty() {
                if let Ok(mut state) = semantic_budget_state.lock() {
                    state.note_semantic_fallback_reason("semantic_no_hits");
                }
                Some("semantic_no_hits".to_string())
            } else {
                None
            };

            Ok(SemanticLaneResolution {
                hits,
                unavailable_reason,
                cache_hit: false,
                latency_ms: elapsed_ms,
                rows_scanned,
                budget_state: "active".to_string(),
                backoff_until_ms,
            })
        }
    }
}

#[allow(dead_code)]
fn resolve_semantic_lane(
    conn: &Connection,
    options: &SearchOptions,
    embedder_id: &str,
    query_vector: &[f32],
    requested_mode: SearchMode,
    semantic_budget_state: &Arc<Mutex<SemanticBudgetState>>,
) -> Result<SemanticLaneResolution> {
    if !matches!(requested_mode, SearchMode::Hybrid | SearchMode::Semantic) {
        return Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: None,
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state: "disabled".to_string(),
            backoff_until_ms: None,
        });
    }

    if query_vector.is_empty() {
        if let Ok(mut state) = semantic_budget_state.lock() {
            state.note_semantic_fallback_reason("semantic_query_empty");
        }
        return Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: Some("semantic_query_empty".to_string()),
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state: "invalid_query".to_string(),
            backoff_until_ms: None,
        });
    }

    if query_vector.iter().any(|v| !v.is_finite()) {
        if let Ok(mut state) = semantic_budget_state.lock() {
            state.note_semantic_fallback_reason("semantic_query_non_finite");
        }
        return Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: Some("semantic_query_non_finite".to_string()),
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state: "invalid_query".to_string(),
            backoff_until_ms: None,
        });
    }

    let now = now_ms();
    let decision = match semantic_budget_state.lock() {
        Ok(mut state) => state.begin_semantic_lane(now, options, embedder_id, query_vector),
        Err(_) => SemanticBudgetDecision::Skip {
            reason: "semantic_budget_poisoned".to_string(),
            budget_state: "error".to_string(),
            backoff_until_ms: None,
        },
    };

    match decision {
        SemanticBudgetDecision::UseCache { hits } => {
            let unavailable_reason = if hits.is_empty() {
                Some("semantic_no_hits".to_string())
            } else {
                None
            };
            if unavailable_reason.is_some() {
                if let Ok(mut state) = semantic_budget_state.lock() {
                    state.note_semantic_fallback_reason("semantic_no_hits");
                }
            }
            Ok(SemanticLaneResolution {
                hits,
                unavailable_reason,
                cache_hit: true,
                latency_ms: 0,
                rows_scanned: 0,
                budget_state: "cache_hit".to_string(),
                backoff_until_ms: None,
            })
        }
        SemanticBudgetDecision::Skip {
            reason,
            budget_state,
            backoff_until_ms,
        } => Ok(SemanticLaneResolution {
            hits: Vec::new(),
            unavailable_reason: Some(reason),
            cache_hit: false,
            latency_ms: 0,
            rows_scanned: 0,
            budget_state,
            backoff_until_ms,
        }),
        SemanticBudgetDecision::Execute { key, max_scan_rows } => {
            let started = Instant::now();
            let (hits, rows_scanned) = search_semantic_sync_with_scan_limit(
                conn,
                embedder_id,
                query_vector,
                options,
                Some(max_scan_rows),
            )?;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let now_after = now_ms();
            let backoff_until_ms = match semantic_budget_state.lock() {
                Ok(mut state) => {
                    state.complete_semantic_lane(now_after, key, &hits, elapsed_ms, rows_scanned)
                }
                Err(_) => None,
            };

            let unavailable_reason = if hits.is_empty() {
                if let Ok(mut state) = semantic_budget_state.lock() {
                    state.note_semantic_fallback_reason("semantic_no_hits");
                }
                Some("semantic_no_hits".to_string())
            } else {
                None
            };

            Ok(SemanticLaneResolution {
                hits,
                unavailable_reason,
                cache_hit: false,
                latency_ms: elapsed_ms,
                rows_scanned,
                budget_state: "active".to_string(),
                backoff_until_ms,
            })
        }
    }
}

fn hybrid_search_with_results_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
    embedder_id: &str,
    query_vector: &[f32],
    mode: SearchMode,
    rrf_k: u32,
    lexical_weight: f32,
    semantic_weight: f32,
    fusion_backend: Option<FusionBackend>,
    semantic_budget_state: &Arc<Mutex<SemanticBudgetState>>,
) -> Result<HybridSearchBundle> {
    let top_k = options.limit.unwrap_or(100);

    let requested_mode = mode;
    let lexical_weight = if lexical_weight.is_finite() {
        lexical_weight.max(0.0)
    } else {
        1.0
    };
    let semantic_weight = if semantic_weight.is_finite() {
        semantic_weight.max(0.0)
    } else {
        1.0
    };
    let (lexical_weight, semantic_weight) = if lexical_weight == 0.0 && semantic_weight == 0.0 {
        (1.0, 1.0)
    } else {
        (lexical_weight, semantic_weight)
    };
    let fusion_backend = fusion_backend.unwrap_or_else(FusionBackend::from_env);

    let semantic_lane = resolve_semantic_lane_backend(
        backend,
        options,
        embedder_id,
        query_vector,
        requested_mode,
        semantic_budget_state,
    )?;
    let semantic_hits = semantic_lane.hits.clone();

    let effective_mode = if semantic_hits.is_empty() && matches!(requested_mode, SearchMode::Hybrid)
    {
        SearchMode::Lexical
    } else {
        requested_mode
    };
    let fallback_reason = if matches!(requested_mode, SearchMode::Hybrid)
        && matches!(effective_mode, SearchMode::Lexical)
    {
        Some(
            semantic_lane
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "semantic_unavailable".to_string()),
        )
    } else {
        None
    };

    let mut lexical_options = options.clone();
    lexical_options.limit = Some(top_k.saturating_mul(4).max(top_k));
    let lexical_results = if matches!(effective_mode, SearchMode::Semantic) {
        Vec::new()
    } else {
        search_fts_with_snippets_backend(backend, query, &lexical_options)?
    };

    let lexical_ranked: Vec<(u64, f32)> = lexical_results
        .iter()
        .enumerate()
        .filter_map(|(rank, row)| {
            u64::try_from(row.segment.id)
                .ok()
                .map(|id| (id, reciprocal_rank_score(rank)))
        })
        .collect();
    let semantic_ranked: Vec<(u64, f32)> = semantic_hits
        .iter()
        .enumerate()
        .filter_map(|(rank, hit)| {
            u64::try_from(hit.segment_id)
                .ok()
                .map(|id| (id, reciprocal_rank_score(rank)))
        })
        .collect();

    let mut fused = HybridSearchService::new()
        .with_mode(effective_mode)
        .with_rrf_k(rrf_k)
        .with_rrf_weights(lexical_weight, semantic_weight)
        .with_fusion_backend(fusion_backend)
        .fuse(&lexical_ranked, &semantic_ranked, top_k);
    // Make tie behavior deterministic despite internal HashMap aggregation.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut lexical_by_id: std::collections::HashMap<i64, (usize, SearchResult)> =
        std::collections::HashMap::with_capacity(lexical_results.len());
    for (rank, row) in lexical_results.into_iter().enumerate() {
        lexical_by_id.insert(row.segment.id, (rank, row));
    }

    let mut semantic_by_id: std::collections::HashMap<i64, (usize, f64)> =
        std::collections::HashMap::with_capacity(semantic_hits.len());
    for (rank, hit) in semantic_hits.into_iter().enumerate() {
        semantic_by_id.insert(hit.segment_id, (rank, hit.score));
    }

    let mut results = Vec::with_capacity(fused.len());
    for (fusion_rank, item) in fused.into_iter().enumerate() {
        let segment_id = match i64::try_from(item.id) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let search_result = if let Some((_, row)) = lexical_by_id.get(&segment_id) {
            row.clone()
        } else if let Some(segment) = query_segment_by_id_backend(backend, segment_id)? {
            SearchResult {
                segment,
                snippet: None,
                highlight: None,
                score: 0.0,
            }
        } else {
            continue;
        };

        let semantic_score = semantic_by_id.get(&segment_id).map(|(_, score)| *score);
        let lexical_rank = lexical_by_id.get(&segment_id).map(|(rank, _)| *rank);
        let semantic_rank = semantic_by_id.get(&segment_id).map(|(rank, _)| *rank);
        let fusion_score = f64::from(item.score);
        let (lexical_contribution, semantic_contribution) = match effective_mode {
            SearchMode::Lexical => (Some(fusion_score), None),
            SearchMode::Semantic => (None, Some(fusion_score)),
            SearchMode::Hybrid => (
                lexical_rank.map(|rank| rrf_component_contribution(rank, rrf_k, lexical_weight)),
                semantic_rank.map(|rank| rrf_component_contribution(rank, rrf_k, semantic_weight)),
            ),
        };

        results.push(HybridSearchResult {
            result: search_result,
            semantic_score,
            lexical_rank,
            semantic_rank,
            lexical_contribution,
            semantic_contribution,
            fusion_rank,
            fusion_score,
        });
    }

    Ok(HybridSearchBundle {
        mode: search_mode_label(effective_mode).to_string(),
        requested_mode: search_mode_label(requested_mode).to_string(),
        fallback_reason,
        rrf_k,
        lexical_weight,
        semantic_weight,
        fusion_backend: fusion_backend.as_str().to_string(),
        lexical_candidates: lexical_ranked.len(),
        semantic_candidates: semantic_ranked.len(),
        semantic_cache_hit: semantic_lane.cache_hit,
        semantic_latency_ms: semantic_lane.latency_ms,
        semantic_rows_scanned: semantic_lane.rows_scanned,
        semantic_budget_state: semantic_lane.budget_state,
        semantic_backoff_until_ms: semantic_lane.backoff_until_ms,
        results,
    })
}

#[allow(dead_code)]
fn hybrid_search_with_results_sync(
    conn: &Connection,
    query: &str,
    options: &SearchOptions,
    embedder_id: &str,
    query_vector: &[f32],
    mode: SearchMode,
    rrf_k: u32,
    lexical_weight: f32,
    semantic_weight: f32,
    fusion_backend: Option<FusionBackend>,
    semantic_budget_state: &Arc<Mutex<SemanticBudgetState>>,
) -> Result<HybridSearchBundle> {
    let top_k = options.limit.unwrap_or(100);

    let requested_mode = mode;
    let lexical_weight = if lexical_weight.is_finite() {
        lexical_weight.max(0.0)
    } else {
        1.0
    };
    let semantic_weight = if semantic_weight.is_finite() {
        semantic_weight.max(0.0)
    } else {
        1.0
    };
    let (lexical_weight, semantic_weight) = if lexical_weight == 0.0 && semantic_weight == 0.0 {
        (1.0, 1.0)
    } else {
        (lexical_weight, semantic_weight)
    };
    let fusion_backend = fusion_backend.unwrap_or_else(FusionBackend::from_env);

    let semantic_lane = resolve_semantic_lane(
        conn,
        options,
        embedder_id,
        query_vector,
        requested_mode,
        semantic_budget_state,
    )?;
    let semantic_hits = semantic_lane.hits.clone();

    let effective_mode = if semantic_hits.is_empty() && matches!(requested_mode, SearchMode::Hybrid)
    {
        SearchMode::Lexical
    } else {
        requested_mode
    };
    let fallback_reason = if matches!(requested_mode, SearchMode::Hybrid)
        && matches!(effective_mode, SearchMode::Lexical)
    {
        Some(
            semantic_lane
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "semantic_unavailable".to_string()),
        )
    } else {
        None
    };

    let mut lexical_options = options.clone();
    lexical_options.limit = Some(top_k.saturating_mul(4).max(top_k));
    let lexical_results = if matches!(effective_mode, SearchMode::Semantic) {
        Vec::new()
    } else {
        search_fts_with_snippets(conn, query, &lexical_options)?
    };

    let lexical_ranked: Vec<(u64, f32)> = lexical_results
        .iter()
        .enumerate()
        .filter_map(|(rank, row)| {
            u64::try_from(row.segment.id)
                .ok()
                .map(|id| (id, reciprocal_rank_score(rank)))
        })
        .collect();
    let semantic_ranked: Vec<(u64, f32)> = semantic_hits
        .iter()
        .enumerate()
        .filter_map(|(rank, hit)| {
            u64::try_from(hit.segment_id)
                .ok()
                .map(|id| (id, reciprocal_rank_score(rank)))
        })
        .collect();

    let mut fused = HybridSearchService::new()
        .with_mode(effective_mode)
        .with_rrf_k(rrf_k)
        .with_rrf_weights(lexical_weight, semantic_weight)
        .with_fusion_backend(fusion_backend)
        .fuse(&lexical_ranked, &semantic_ranked, top_k);
    // Make tie behavior deterministic despite internal HashMap aggregation.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut lexical_by_id: std::collections::HashMap<i64, (usize, SearchResult)> =
        std::collections::HashMap::with_capacity(lexical_results.len());
    for (rank, row) in lexical_results.into_iter().enumerate() {
        lexical_by_id.insert(row.segment.id, (rank, row));
    }

    let mut semantic_by_id: std::collections::HashMap<i64, (usize, f64)> =
        std::collections::HashMap::with_capacity(semantic_hits.len());
    for (rank, hit) in semantic_hits.into_iter().enumerate() {
        semantic_by_id.insert(hit.segment_id, (rank, hit.score));
    }

    let mut results = Vec::with_capacity(fused.len());
    for (fusion_rank, item) in fused.into_iter().enumerate() {
        let segment_id = match i64::try_from(item.id) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let search_result = if let Some((_, row)) = lexical_by_id.get(&segment_id) {
            row.clone()
        } else if let Some(segment) = query_segment_by_id(conn, segment_id)? {
            SearchResult {
                segment,
                snippet: None,
                highlight: None,
                score: 0.0,
            }
        } else {
            continue;
        };

        let semantic_score = semantic_by_id.get(&segment_id).map(|(_, score)| *score);
        let lexical_rank = lexical_by_id.get(&segment_id).map(|(rank, _)| *rank);
        let semantic_rank = semantic_by_id.get(&segment_id).map(|(rank, _)| *rank);
        let fusion_score = f64::from(item.score);
        let (lexical_contribution, semantic_contribution) = match effective_mode {
            SearchMode::Lexical => (Some(fusion_score), None),
            SearchMode::Semantic => (None, Some(fusion_score)),
            SearchMode::Hybrid => (
                lexical_rank.map(|rank| rrf_component_contribution(rank, rrf_k, lexical_weight)),
                semantic_rank.map(|rank| rrf_component_contribution(rank, rrf_k, semantic_weight)),
            ),
        };

        results.push(HybridSearchResult {
            result: search_result,
            semantic_score,
            lexical_rank,
            semantic_rank,
            lexical_contribution,
            semantic_contribution,
            fusion_rank,
            fusion_score,
        });
    }

    Ok(HybridSearchBundle {
        mode: search_mode_label(effective_mode).to_string(),
        requested_mode: search_mode_label(requested_mode).to_string(),
        fallback_reason,
        rrf_k,
        lexical_weight,
        semantic_weight,
        fusion_backend: fusion_backend.as_str().to_string(),
        lexical_candidates: lexical_ranked.len(),
        semantic_candidates: semantic_ranked.len(),
        semantic_cache_hit: semantic_lane.cache_hit,
        semantic_latency_ms: semantic_lane.latency_ms,
        semantic_rows_scanned: semantic_lane.rows_scanned,
        semantic_budget_state: semantic_lane.budget_state,
        semantic_backoff_until_ms: semantic_lane.backoff_until_ms,
        results,
    })
}

// =============================================================================
// Indexing Progress Tracking (wa-upg.5.2)
// =============================================================================

fn pane_indexing_stats_from_backend_cells(row: &[SqlCell]) -> Result<PaneIndexingStats> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(0)
        .and_then(|value| backend_i64_to_u64(value, "panes.pane_id"))
        .map_err(|err| storage_backend_error("FTS indexing stats pane_id", err))?;
    let segment_count = reader
        .i64(1)
        .and_then(|value| backend_i64_to_u64(value, "output_segments.count"))
        .map_err(|err| storage_backend_error("FTS indexing stats segment_count", err))?;
    let total_bytes = reader
        .i64(2)
        .and_then(|value| backend_i64_to_u64(value, "output_segments.bytes"))
        .map_err(|err| storage_backend_error("FTS indexing stats total_bytes", err))?;
    let max_seq = reader
        .optional_i64(3)
        .map_err(|err| storage_backend_error("FTS indexing stats max_seq", err))?
        .map(|value| backend_i64_to_u64(value, "output_segments.max_seq"))
        .transpose()
        .map_err(|err| storage_backend_error("FTS indexing stats max_seq", err))?;
    let last_segment_at = reader
        .optional_i64(4)
        .map_err(|err| storage_backend_error("FTS indexing stats last_segment_at", err))?;

    Ok(PaneIndexingStats {
        pane_id,
        segment_count,
        total_bytes,
        max_seq,
        last_segment_at,
        fts_row_count: segment_count,
        fts_consistent: true,
    })
}

fn get_pane_indexing_stats_backend(backend: &dyn StorageBackend) -> Result<Vec<PaneIndexingStats>> {
    let rows = backend
        .query_map_cells(
            "SELECT p.pane_id,
                    COALESCE(seg.cnt, 0),
                    COALESCE(seg.bytes, 0),
                    seg.max_seq,
                    seg.last_at
             FROM panes p
             LEFT JOIN (
                 SELECT pane_id,
                        COUNT(*) AS cnt,
                        SUM(content_len) AS bytes,
                        MAX(seq) AS max_seq,
                        MAX(captured_at) AS last_at
                 FROM output_segments
                 GROUP BY pane_id
             ) seg ON seg.pane_id = p.pane_id
             WHERE p.observed = 1
             ORDER BY p.pane_id",
            &[],
        )
        .map_err(|err| storage_backend_error("Failed to query indexing stats", err))?;

    rows.iter()
        .map(|row| pane_indexing_stats_from_backend_cells(row))
        .collect()
}

/// Run the FTS5 integrity-check command.
///
/// Returns Ok(true) if the index is consistent, Ok(false) if corruption
/// is detected.
fn check_fts_integrity_sync(conn: &Connection) -> Result<bool> {
    match conn.execute_batch(
        "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
    ) {
        Ok(()) => Ok(true),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("database disk image is malformed") || msg.contains("fts5: ") {
                Ok(false)
            } else {
                Err(StorageError::Database(format!("FTS integrity check failed: {e}")).into())
            }
        }
    }
}

/// Build an aggregate health report from per-pane stats and FTS integrity.
fn build_indexing_health_report(
    pane_stats: Vec<PaneIndexingStats>,
    fts_ok: bool,
) -> IndexingHealthReport {
    let total_segments: u64 = pane_stats.iter().map(|p| p.segment_count).sum();
    let total_bytes: u64 = pane_stats.iter().map(|p| p.total_bytes).sum();
    let total_fts_rows: u64 = pane_stats.iter().map(|p| p.fts_row_count).sum();
    let inconsistent_panes = if fts_ok {
        0
    } else {
        // If FTS integrity check fails, mark all panes as potentially inconsistent
        pane_stats.len() as u64
    };

    // Update per-pane consistency based on FTS health
    let panes: Vec<PaneIndexingStats> = if fts_ok {
        pane_stats
    } else {
        pane_stats
            .into_iter()
            .map(|mut p| {
                p.fts_consistent = false;
                p
            })
            .collect()
    };

    IndexingHealthReport {
        healthy: fts_ok && inconsistent_panes == 0,
        total_segments,
        total_bytes,
        total_fts_rows,
        inconsistent_panes,
        panes,
    }
}

// =============================================================================
// Incremental FTS Sync (wa-3g9.4)
// =============================================================================

/// Current FTS index version. Increment when FTS schema changes require rebuild.
const FTS_INDEX_VERSION: u32 = 1;
/// Sentinel version used when a rebuild was started but did not finish.
///
/// `sync_fts_on_startup()` treats any non-current version as a forced full
/// rebuild, so this leaves an explicit "must rebuild" marker instead of
/// silently trusting a partially rebuilt index.
const FTS_INDEX_REBUILD_PENDING_VERSION: u32 = 0;

/// Get the current FTS index state
fn get_fts_index_state_sync(conn: &Connection) -> Result<Option<FtsIndexState>> {
    conn.query_row(
        "SELECT index_version, last_full_rebuild_at, created_at, updated_at
         FROM fts_index_state WHERE id = 1",
        [],
        |row| {
            Ok(FtsIndexState {
                index_version: {
                    // SQLite stores `index_version` as INTEGER. A
                    // bare `as u32` silently wraps if the column is
                    // corrupted to negative or outside the u32
                    // range; clamp instead so a corrupted row can
                    // still be observed without producing a
                    // gargantuan version number that confuses
                    // downstream comparisons.
                    let v: i64 = row.get(0)?;
                    v.clamp(0, i64::from(u32::MAX)) as u32
                },
                last_full_rebuild_at: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Failed to get FTS index state: {e}")).into())
}

/// Initialize or update FTS index state
fn upsert_fts_index_state_sync(conn: &Connection, state: &FtsIndexState) -> Result<()> {
    conn.execute(
        "INSERT INTO fts_index_state (id, index_version, last_full_rebuild_at, created_at, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             index_version = excluded.index_version,
             last_full_rebuild_at = excluded.last_full_rebuild_at,
             updated_at = excluded.updated_at",
        params![
            i64::from(state.index_version),
            state.last_full_rebuild_at,
            state.created_at,
            state.updated_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert FTS index state: {e}")))?;
    Ok(())
}

/// Mark the FTS index as needing a clean full rebuild.
///
/// This is used when a delete-all + reindex cycle fails after the old index
/// contents have already been discarded. We intentionally leave the state in a
/// forced-rebuild marker rather than restoring per-pane progress against an
/// incomplete FTS table.
fn mark_fts_rebuild_pending_sync(conn: &Connection, now: i64) -> Result<()> {
    let created_at = get_fts_index_state_sync(conn)?
        .map(|state| state.created_at)
        .unwrap_or(now);
    let pending_state = FtsIndexState {
        index_version: FTS_INDEX_REBUILD_PENDING_VERSION,
        last_full_rebuild_at: None,
        created_at,
        updated_at: now,
    };
    upsert_fts_index_state_sync(conn, &pending_state)
}

/// Get FTS progress for a specific pane
fn get_fts_pane_progress_sync(conn: &Connection, pane_id: u64) -> Result<Option<FtsPaneProgress>> {
    conn.query_row(
        "SELECT pane_id, last_indexed_seq, indexed_count, last_indexed_at
         FROM fts_pane_progress WHERE pane_id = ?1",
        [pane_id as i64],
        |row| {
            Ok(FtsPaneProgress {
                pane_id: {
                    let v: i64 = row.get(0)?;
                    v as u64
                },
                last_indexed_seq: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                indexed_count: {
                    let v: i64 = row.get(2)?;
                    v as u64
                },
                last_indexed_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Failed to get FTS pane progress: {e}")).into())
}

/// Update FTS progress for a pane
fn upsert_fts_pane_progress_sync(conn: &Connection, progress: &FtsPaneProgress) -> Result<()> {
    conn.execute(
        "INSERT INTO fts_pane_progress (pane_id, last_indexed_seq, indexed_count, last_indexed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(pane_id) DO UPDATE SET
             last_indexed_seq = excluded.last_indexed_seq,
             indexed_count = excluded.indexed_count,
             last_indexed_at = excluded.last_indexed_at",
        params![
            progress.pane_id as i64,
            progress.last_indexed_seq as i64,
            progress.indexed_count as i64,
            progress.last_indexed_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert FTS pane progress: {e}")))?;
    Ok(())
}

/// Clear all FTS pane progress (used before full rebuild)
fn clear_fts_pane_progress_sync(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM fts_pane_progress", [])
        .map_err(|e| StorageError::Database(format!("Failed to clear FTS pane progress: {e}")))?;
    Ok(())
}

/// Get segments that need indexing for a pane.
///
/// When `include_from_zero` is `true`, the query is `WHERE pane_id = ?1`
/// with no seq filter — used on the very first batch of the very first
/// sync for a pane, where `last_indexed_seq = 0` is the default-zero
/// sentinel meaning "never indexed", not a claim that seq=0 has been
/// indexed. See [ft-7do6c] for the full rationale: `append_segment_backend`
/// assigns `seq = COALESCE(MAX(seq) + 1, 0)`, so the pane's first-ever
/// segment is seq=0, and a strict `seq > 0` filter would silently skip
/// it forever under deferred-FTS mode.
///
/// Otherwise the query is `WHERE pane_id = ?1 AND seq > ?2`, which is
/// correct once any segment has actually been indexed (at which point
/// `last_indexed_seq` carries the real high-water mark).
fn get_unindexed_segments_sync(
    conn: &Connection,
    pane_id: u64,
    last_indexed_seq: u64,
    limit: usize,
    include_from_zero: bool,
) -> Result<Vec<Segment>> {
    let sql = if include_from_zero {
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1
             ORDER BY seq
             LIMIT ?3"
    } else {
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1 AND seq > ?2
             ORDER BY seq
             LIMIT ?3"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare unindexed query: {e}")))?;

    let rows = stmt
        .query_map(
            params![pane_id as i64, last_indexed_seq as i64, limit as i64],
            |row| {
                Ok(Segment {
                    id: row.get(0)?,
                    pane_id: {
                        let v: i64 = row.get(1)?;
                        v as u64
                    },
                    seq: {
                        let v: i64 = row.get(2)?;
                        v as u64
                    },
                    content: row.get(3)?,
                    content_len: {
                        let v: i64 = row.get(4)?;
                        i64_to_usize(v)?
                    },
                    content_hash: row.get(5)?,
                    captured_at: row.get(6)?,
                })
            },
        )
        .map_err(|e| StorageError::Database(format!("Failed to query unindexed segments: {e}")))?;

    let mut segments = Vec::new();
    for row in rows {
        segments.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(segments)
}

/// Manually insert a segment into the FTS index (for recovery/rebuild)
fn insert_fts_entry_sync(conn: &Connection, segment: &Segment) -> Result<()> {
    conn.execute(
        "INSERT INTO output_segments_fts(rowid, content) VALUES (?1, ?2)",
        params![segment.id, &segment.content],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert FTS entry: {e}")))?;
    Ok(())
}

/// Perform incremental FTS sync for a pane
///
/// This function indexes segments that are newer than the recorded progress,
/// working in batches to avoid memory pressure and allow progress commits.
fn sync_fts_for_pane(
    conn: &Connection,
    pane_id: u64,
    config: &FtsSyncConfig,
) -> Result<(u64, u64)> {
    let now = now_ms();

    // Get current progress
    let progress = get_fts_pane_progress_sync(conn, pane_id)?;
    let last_seq = progress.as_ref().map_or(0, |p| p.last_indexed_seq);
    let mut indexed_count = progress.as_ref().map_or(0, |p| p.indexed_count);
    // [ft-7do6c] "Never indexed" sentinel: the absence of a progress
    // row is distinct from "indexed up to seq=0". Without tracking
    // this, the strict `seq > 0` filter would skip the pane's first
    // segment forever — COALESCE(MAX(seq)+1, 0) in append_segment_backend
    // assigns seq=0 to every pane's very first segment.
    let had_prior_progress = progress.is_some();

    let mut total_indexed = 0u64;
    let mut max_seq = last_seq;

    loop {
        // Get batch of unindexed segments. Only the very first batch
        // of the very first sync (no prior progress row AND nothing
        // indexed yet in this call) uses the inclusive `WHERE pane_id
        // = ?1` variant to cover seq=0. Every subsequent batch reverts
        // to the strict `seq > max_seq` variant so we don't re-index
        // rows we just handled.
        let include_from_zero = !had_prior_progress && total_indexed == 0;
        let segments = get_unindexed_segments_sync(
            conn,
            pane_id,
            max_seq,
            config.batch_size,
            include_from_zero,
        )?;
        if segments.is_empty() {
            break;
        }

        // Index each segment (respecting byte limit)
        let mut batch_bytes = 0usize;
        for segment in &segments {
            // Check byte limit (but always index at least one)
            if batch_bytes > 0 && batch_bytes + segment.content_len > config.max_batch_bytes {
                break;
            }

            insert_fts_entry_sync(conn, segment)?;
            total_indexed += 1;
            indexed_count += 1;
            max_seq = segment.seq;
            batch_bytes += segment.content_len;
        }

        // Commit progress after each batch if configured
        if config.commit_progress && total_indexed > 0 {
            let new_progress = FtsPaneProgress {
                pane_id,
                last_indexed_seq: max_seq,
                indexed_count,
                last_indexed_at: now,
            };
            upsert_fts_pane_progress_sync(conn, &new_progress)?;
        }

        // If we processed fewer segments than batch size, we're done
        if segments.len() < config.batch_size {
            break;
        }
    }

    // Final progress update
    if total_indexed > 0 && !config.commit_progress {
        let new_progress = FtsPaneProgress {
            pane_id,
            last_indexed_seq: max_seq,
            indexed_count,
            last_indexed_at: now,
        };
        upsert_fts_pane_progress_sync(conn, &new_progress)?;
    }

    Ok((total_indexed, max_seq))
}

fn panes_needing_fts_sync(conn: &Connection) -> Result<Vec<u64>> {
    let mut stmt = conn
        .prepare(
            "SELECT s.pane_id
             FROM output_segments s
             LEFT JOIN fts_pane_progress p ON p.pane_id = s.pane_id
             GROUP BY s.pane_id
             HAVING MAX(p.pane_id) IS NULL OR MAX(s.seq) > MAX(p.last_indexed_seq)
             ORDER BY s.pane_id",
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to list panes needing FTS sync: {e}"))
        })?;
    let rows = stmt
        .query_map([], |row| {
            let v: i64 = row.get(0)?;
            Ok(v as u64)
        })
        .map_err(|e| {
            StorageError::Database(format!("Failed to query panes needing FTS sync: {e}"))
        })?;

    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(ids)
}

/// Perform a full FTS rebuild with batched progress tracking
///
/// This drops the FTS index content and reindexes all segments.
///
/// **Non-atomic risk**: The delete-all + re-index loop is NOT wrapped in a
/// single SQLite transaction. If a hard database error occurs mid-rebuild, we
/// fail closed: clear progress, mark the index state as rebuild-pending, and
/// return an error rather than pretending the partial index is healthy.
fn full_fts_rebuild_sync(conn: &Connection, config: &FtsSyncConfig) -> Result<FtsSyncResult> {
    use std::time::Instant;
    let start = Instant::now();
    let now = now_ms();

    let mut warnings = Vec::new();

    // Drop all FTS content
    if let Err(e) = conn
        .execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
    {
        warnings.push(format!("FTS delete-all failed (may be empty): {e}"));
    }

    // Clear progress tracking
    clear_fts_pane_progress_sync(conn)?;

    // Get all panes
    let pane_ids: Vec<u64> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT pane_id FROM output_segments ORDER BY pane_id")
            .map_err(|e| StorageError::Database(format!("Failed to list panes: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let v: i64 = row.get(0)?;
                Ok(v as u64)
            })
            .map_err(|e| StorageError::Database(format!("Failed to query panes: {e}")))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
        }
        ids
    };

    let mut total_indexed = 0u64;
    let panes_processed = pane_ids.len() as u64;
    let mut hard_failure_panes: Vec<(u64, String)> = Vec::new();

    // Sync each pane — distinguish hard I/O errors from content parse warnings
    for pane_id in &pane_ids {
        match sync_fts_for_pane(conn, *pane_id, config) {
            Ok((indexed, _)) => total_indexed += indexed,
            Err(e) => {
                let err_msg = format!("{e}");
                // Classify: Database/IO errors are hard failures; others are warnings
                let is_hard = matches!(
                    &e,
                    crate::Error::Storage(StorageError::Database(_)) | crate::Error::Io(_)
                );
                if is_hard {
                    tracing::error!(
                        pane_id = pane_id,
                        error = %e,
                        "FTS rebuild: pane completely failed to re-index (hard I/O error)"
                    );
                    hard_failure_panes.push((*pane_id, err_msg.clone()));
                } else {
                    tracing::warn!(
                        pane_id = pane_id,
                        error = %e,
                        "FTS rebuild: pane sync produced non-fatal error"
                    );
                }
                warnings.push(format!("Pane {pane_id} sync failed: {err_msg}"));
            }
        }
    }

    // If any pane had a hard failure, the delete-all rebuild is incomplete.
    // Restoring old per-pane progress here would make a structurally valid but
    // incomplete FTS table look healthy enough to skip future rebuilds.
    if !hard_failure_panes.is_empty() {
        tracing::error!(
            failed_count = hard_failure_panes.len(),
            total_panes = pane_ids.len(),
            "FTS rebuild had hard failures; marking index as rebuild-pending"
        );
        if let Err(cleanup_err) = conn.execute_batch(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')",
        ) {
            tracing::error!(
                error = %cleanup_err,
                "Failed to clear partially rebuilt FTS contents after hard rebuild failure"
            );
        }
        clear_fts_pane_progress_sync(conn)?;
        mark_fts_rebuild_pending_sync(conn, now)?;

        let failed_panes = hard_failure_panes
            .iter()
            .map(|(pane_id, _)| pane_id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(StorageError::Database(format!(
            "FTS rebuild incomplete after hard failures in panes [{failed_panes}]; index marked for full rebuild"
        ))
        .into());
    }

    // Update index state
    let state = FtsIndexState {
        index_version: FTS_INDEX_VERSION,
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };
    upsert_fts_index_state_sync(conn, &state)?;

    let duration = start.elapsed();
    Ok(FtsSyncResult {
        segments_indexed: total_indexed,
        panes_processed,
        full_rebuild: true,
        duration_ms: duration.as_millis() as u64,
        warnings,
    })
}

/// Perform incremental FTS sync on startup
///
/// This checks the FTS index state and either:
/// 1. Does nothing if index is healthy and version matches
/// 2. Syncs only new segments if index is healthy but has gaps
/// 3. Performs a full rebuild if index is corrupt or version mismatches
pub fn sync_fts_on_startup(conn: &Connection, config: &FtsSyncConfig) -> Result<FtsSyncResult> {
    use std::time::Instant;
    let start = Instant::now();

    let mut warnings = Vec::new();

    // Check FTS integrity
    let fts_ok = check_fts_integrity_sync(conn)?;
    if !fts_ok {
        tracing::warn!("FTS index corruption detected, performing full rebuild");
        return full_fts_rebuild_sync(conn, config);
    }

    // Get current index state
    let state = get_fts_index_state_sync(conn)?;

    // Check if version mismatch (schema change)
    if let Some(ref s) = state {
        if s.index_version != FTS_INDEX_VERSION {
            tracing::info!(
                old_version = s.index_version,
                new_version = FTS_INDEX_VERSION,
                "FTS index version mismatch or rebuild-pending marker detected, performing full rebuild"
            );
            return full_fts_rebuild_sync(conn, config);
        }
    } else {
        // No state = first run after migration, initialize
        let now = now_ms();
        let new_state = FtsIndexState {
            index_version: FTS_INDEX_VERSION,
            last_full_rebuild_at: None,
            created_at: now,
            updated_at: now,
        };
        upsert_fts_index_state_sync(conn, &new_state)?;
    }

    let pane_ids = panes_needing_fts_sync(conn)?;

    let mut total_indexed = 0u64;
    let panes_processed = pane_ids.len() as u64;

    // Incremental sync each pane
    for pane_id in pane_ids {
        match sync_fts_for_pane(conn, pane_id, config) {
            Ok((indexed, _)) => total_indexed += indexed,
            Err(e) => warnings.push(format!("Pane {pane_id} incremental sync failed: {e}")),
        }
    }

    let duration = start.elapsed();
    Ok(FtsSyncResult {
        segments_indexed: total_indexed,
        panes_processed,
        full_rebuild: false,
        duration_ms: duration.as_millis() as u64,
        warnings,
    })
}

fn fts_index_state_from_backend_cells(row: &[SqlCell]) -> Result<FtsIndexState> {
    let reader = CellRowReader::new(row);
    let index_version = reader
        .i64(0)
        .map(|value| value.clamp(0, i64::from(u32::MAX)) as u32)
        .map_err(|err| storage_backend_error("FTS index state version", err))?;
    Ok(FtsIndexState {
        index_version,
        last_full_rebuild_at: reader
            .optional_i64(1)
            .map_err(|err| storage_backend_error("FTS index state last_full_rebuild_at", err))?,
        created_at: reader
            .i64(2)
            .map_err(|err| storage_backend_error("FTS index state created_at", err))?,
        updated_at: reader
            .i64(3)
            .map_err(|err| storage_backend_error("FTS index state updated_at", err))?,
    })
}

fn fts_pane_progress_from_backend_cells(row: &[SqlCell]) -> Result<FtsPaneProgress> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(0)
        .and_then(|value| backend_i64_to_u64(value, "fts_pane_progress.pane_id"))
        .map_err(|err| storage_backend_error("FTS pane progress pane_id", err))?;
    let last_indexed_seq = reader
        .i64(1)
        .and_then(|value| backend_i64_to_u64(value, "fts_pane_progress.last_indexed_seq"))
        .map_err(|err| storage_backend_error("FTS pane progress last_indexed_seq", err))?;
    let indexed_count = reader
        .i64(2)
        .and_then(|value| backend_i64_to_u64(value, "fts_pane_progress.indexed_count"))
        .map_err(|err| storage_backend_error("FTS pane progress indexed_count", err))?;
    Ok(FtsPaneProgress {
        pane_id,
        last_indexed_seq,
        indexed_count,
        last_indexed_at: reader
            .i64(3)
            .map_err(|err| storage_backend_error("FTS pane progress last_indexed_at", err))?,
    })
}

fn segment_from_backend_cells(row: &[SqlCell]) -> Result<Segment> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(1)
        .and_then(|value| backend_i64_to_u64(value, "output_segments.pane_id"))
        .map_err(|err| storage_backend_error("FTS segment pane_id", err))?;
    let seq = reader
        .i64(2)
        .and_then(|value| backend_i64_to_u64(value, "output_segments.seq"))
        .map_err(|err| storage_backend_error("FTS segment seq", err))?;
    let content_len = reader
        .i64(4)
        .and_then(|value| {
            usize::try_from(value).map_err(|_| {
                BackendError::Query(format!("output_segments.content_len out of range: {value}"))
            })
        })
        .map_err(|err| storage_backend_error("FTS segment content_len", err))?;
    Ok(Segment {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("FTS segment id", err))?,
        pane_id,
        seq,
        content: reader
            .string(3)
            .map_err(|err| storage_backend_error("FTS segment content", err))?,
        content_len,
        content_hash: reader
            .optional_string(5)
            .map_err(|err| storage_backend_error("FTS segment content_hash", err))?,
        captured_at: reader
            .i64(6)
            .map_err(|err| storage_backend_error("FTS segment captured_at", err))?,
    })
}

fn check_fts_integrity_backend(backend: &dyn StorageBackend) -> Result<bool> {
    match backend.execute_batch(
        "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
    ) {
        Ok(()) => Ok(true),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("database disk image is malformed") || msg.contains("fts5: ") {
                Ok(false)
            } else {
                Err(StorageError::Database(format!("FTS integrity check failed: {err}")).into())
            }
        }
    }
}

fn get_fts_index_state_backend(backend: &dyn StorageBackend) -> Result<Option<FtsIndexState>> {
    let row = backend
        .query_row_cells(
            "SELECT index_version, last_full_rebuild_at, created_at, updated_at
             FROM fts_index_state WHERE id = 1",
            &[],
        )
        .map_err(|err| storage_backend_error("Failed to get FTS index state", err))?;
    row.as_deref()
        .map(fts_index_state_from_backend_cells)
        .transpose()
}

fn upsert_fts_index_state_backend(
    backend: &dyn StorageBackend,
    state: &FtsIndexState,
) -> Result<()> {
    execute_typed(
        backend,
        "INSERT INTO fts_index_state (id, index_version, last_full_rebuild_at, created_at, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             index_version = excluded.index_version,
             last_full_rebuild_at = excluded.last_full_rebuild_at,
             updated_at = excluded.updated_at",
        &[
            ToSqlValue::Integer(i64::from(state.index_version)),
            ToSqlValue::optional_i64(state.last_full_rebuild_at),
            ToSqlValue::Integer(state.created_at),
            ToSqlValue::Integer(state.updated_at),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert FTS index state", err))?;
    Ok(())
}

fn mark_fts_rebuild_pending_backend(backend: &dyn StorageBackend, now: i64) -> Result<()> {
    let created_at = get_fts_index_state_backend(backend)?
        .map(|state| state.created_at)
        .unwrap_or(now);
    let pending_state = FtsIndexState {
        index_version: FTS_INDEX_REBUILD_PENDING_VERSION,
        last_full_rebuild_at: None,
        created_at,
        updated_at: now,
    };
    upsert_fts_index_state_backend(backend, &pending_state)
}

fn get_fts_pane_progress_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
) -> Result<Option<FtsPaneProgress>> {
    let pane_id = u64_to_i64(pane_id, "fts_pane_progress.pane_id")?;
    let row = backend
        .query_row_cells(
            "SELECT pane_id, last_indexed_seq, indexed_count, last_indexed_at
             FROM fts_pane_progress WHERE pane_id = ?1",
            &[ToSqlValue::Integer(pane_id)],
        )
        .map_err(|err| storage_backend_error("Failed to get FTS pane progress", err))?;
    row.as_deref()
        .map(fts_pane_progress_from_backend_cells)
        .transpose()
}

fn upsert_fts_pane_progress_backend(
    backend: &dyn StorageBackend,
    progress: &FtsPaneProgress,
) -> Result<()> {
    let pane_id = u64_to_i64(progress.pane_id, "fts_pane_progress.pane_id")?;
    let last_indexed_seq = u64_to_i64(
        progress.last_indexed_seq,
        "fts_pane_progress.last_indexed_seq",
    )?;
    let indexed_count = u64_to_i64(progress.indexed_count, "fts_pane_progress.indexed_count")?;
    execute_typed(
        backend,
        "INSERT INTO fts_pane_progress (pane_id, last_indexed_seq, indexed_count, last_indexed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(pane_id) DO UPDATE SET
             last_indexed_seq = excluded.last_indexed_seq,
             indexed_count = excluded.indexed_count,
             last_indexed_at = excluded.last_indexed_at",
        &[
            ToSqlValue::Integer(pane_id),
            ToSqlValue::Integer(last_indexed_seq),
            ToSqlValue::Integer(indexed_count),
            ToSqlValue::Integer(progress.last_indexed_at),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to upsert FTS pane progress", err))?;
    Ok(())
}

fn clear_fts_pane_progress_backend(backend: &dyn StorageBackend) -> Result<()> {
    backend
        .execute("DELETE FROM fts_pane_progress")
        .map_err(|err| storage_backend_error("Failed to clear FTS pane progress", err))?;
    Ok(())
}

fn get_unindexed_segments_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
    last_indexed_seq: u64,
    limit: usize,
    include_from_zero: bool,
) -> Result<Vec<Segment>> {
    let sql = if include_from_zero {
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1
             ORDER BY seq
             LIMIT ?3"
    } else {
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1 AND seq > ?2
             ORDER BY seq
             LIMIT ?3"
    };
    let pane_id = u64_to_i64(pane_id, "output_segments.pane_id")?;
    let last_indexed_seq = u64_to_i64(last_indexed_seq, "output_segments.seq")?;
    let limit = usize_to_i64(limit, "FTS sync batch_size")?;
    let rows = backend
        .query_map_cells(
            sql,
            &[
                ToSqlValue::Integer(pane_id),
                ToSqlValue::Integer(last_indexed_seq),
                ToSqlValue::Integer(limit),
            ],
        )
        .map_err(|err| storage_backend_error("Failed to query unindexed segments", err))?;
    rows.iter()
        .map(|row| segment_from_backend_cells(row))
        .collect()
}

fn insert_fts_entry_backend(backend: &dyn StorageBackend, segment: &Segment) -> Result<()> {
    execute_typed(
        backend,
        "INSERT INTO output_segments_fts(rowid, content) VALUES (?1, ?2)",
        &[
            ToSqlValue::Integer(segment.id),
            ToSqlValue::Text(&segment.content),
        ],
    )
    .map_err(|err| storage_backend_error("Failed to insert FTS entry", err))?;
    Ok(())
}

fn sync_fts_for_pane_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
    config: &FtsSyncConfig,
) -> Result<(u64, u64)> {
    let now = now_ms();
    let progress = get_fts_pane_progress_backend(backend, pane_id)?;
    let last_seq = progress.as_ref().map_or(0, |p| p.last_indexed_seq);
    let mut indexed_count = progress.as_ref().map_or(0, |p| p.indexed_count);
    let had_prior_progress = progress.is_some();

    let mut total_indexed = 0u64;
    let mut max_seq = last_seq;

    loop {
        let include_from_zero = !had_prior_progress && total_indexed == 0;
        let segments = get_unindexed_segments_backend(
            backend,
            pane_id,
            max_seq,
            config.batch_size,
            include_from_zero,
        )?;
        if segments.is_empty() {
            break;
        }

        let mut batch_bytes = 0usize;
        for segment in &segments {
            if batch_bytes > 0 && batch_bytes + segment.content_len > config.max_batch_bytes {
                break;
            }

            insert_fts_entry_backend(backend, segment)?;
            total_indexed = total_indexed.saturating_add(1);
            indexed_count = indexed_count.saturating_add(1);
            max_seq = segment.seq;
            batch_bytes = batch_bytes.saturating_add(segment.content_len);
        }

        if config.commit_progress && total_indexed > 0 {
            let new_progress = FtsPaneProgress {
                pane_id,
                last_indexed_seq: max_seq,
                indexed_count,
                last_indexed_at: now,
            };
            upsert_fts_pane_progress_backend(backend, &new_progress)?;
        }

        if segments.len() < config.batch_size {
            break;
        }
    }

    if total_indexed > 0 && !config.commit_progress {
        let new_progress = FtsPaneProgress {
            pane_id,
            last_indexed_seq: max_seq,
            indexed_count,
            last_indexed_at: now,
        };
        upsert_fts_pane_progress_backend(backend, &new_progress)?;
    }

    Ok((total_indexed, max_seq))
}

fn panes_needing_fts_sync_backend(backend: &dyn StorageBackend) -> Result<Vec<u64>> {
    let rows = backend
        .query_map_cells(
            "SELECT s.pane_id
             FROM output_segments s
             LEFT JOIN fts_pane_progress p ON p.pane_id = s.pane_id
             GROUP BY s.pane_id
             HAVING MAX(p.pane_id) IS NULL OR MAX(s.seq) > MAX(p.last_indexed_seq)
             ORDER BY s.pane_id",
            &[],
        )
        .map_err(|err| storage_backend_error("Failed to query panes needing FTS sync", err))?;

    rows.iter()
        .map(|row| {
            CellRowReader::new(row)
                .i64(0)
                .and_then(|value| backend_i64_to_u64(value, "output_segments.pane_id"))
                .map_err(|err| {
                    storage_backend_error("Failed to list panes needing FTS sync", err).into()
                })
        })
        .collect()
}

fn full_fts_rebuild_backend(
    backend: &dyn StorageBackend,
    config: &FtsSyncConfig,
) -> Result<FtsSyncResult> {
    use std::time::Instant;
    let start = Instant::now();
    let now = now_ms();
    let mut warnings = Vec::new();

    if let Err(err) = backend
        .execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
    {
        warnings.push(format!("FTS delete-all failed (may be empty): {err}"));
    }

    clear_fts_pane_progress_backend(backend)?;

    let pane_ids = backend
        .query_map_cells(
            "SELECT DISTINCT pane_id FROM output_segments ORDER BY pane_id",
            &[],
        )
        .map_err(|err| storage_backend_error("Failed to query panes", err))?
        .iter()
        .map(|row| {
            CellRowReader::new(row)
                .i64(0)
                .and_then(|value| backend_i64_to_u64(value, "output_segments.pane_id"))
                .map_err(|err| storage_backend_error("Failed to list panes", err).into())
        })
        .collect::<Result<Vec<_>>>()?;

    let mut total_indexed = 0u64;
    let panes_processed = pane_ids.len() as u64;
    let mut hard_failure_panes: Vec<(u64, String)> = Vec::new();

    for pane_id in &pane_ids {
        match sync_fts_for_pane_backend(backend, *pane_id, config) {
            Ok((indexed, _)) => total_indexed = total_indexed.saturating_add(indexed),
            Err(err) => {
                let err_msg = format!("{err}");
                let is_hard = matches!(
                    &err,
                    crate::Error::Storage(StorageError::Database(_)) | crate::Error::Io(_)
                );
                if is_hard {
                    tracing::error!(
                        pane_id = pane_id,
                        error = %err,
                        "FTS rebuild: pane completely failed to re-index (hard I/O error)"
                    );
                    hard_failure_panes.push((*pane_id, err_msg.clone()));
                } else {
                    tracing::warn!(
                        pane_id = pane_id,
                        error = %err,
                        "FTS rebuild: pane sync produced non-fatal error"
                    );
                }
                warnings.push(format!("Pane {pane_id} sync failed: {err_msg}"));
            }
        }
    }

    if !hard_failure_panes.is_empty() {
        tracing::error!(
            failed_count = hard_failure_panes.len(),
            total_panes = pane_ids.len(),
            "FTS rebuild had hard failures; marking index as rebuild-pending"
        );
        if let Err(cleanup_err) = backend.execute_batch(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')",
        ) {
            tracing::error!(
                error = %cleanup_err,
                "Failed to clear partially rebuilt FTS contents after hard rebuild failure"
            );
        }
        clear_fts_pane_progress_backend(backend)?;
        mark_fts_rebuild_pending_backend(backend, now)?;

        let failed_panes = hard_failure_panes
            .iter()
            .map(|(pane_id, _)| pane_id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(StorageError::Database(format!(
            "FTS rebuild incomplete after hard failures in panes [{failed_panes}]; index marked for full rebuild"
        ))
        .into());
    }

    let state = FtsIndexState {
        index_version: FTS_INDEX_VERSION,
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };
    upsert_fts_index_state_backend(backend, &state)?;

    let duration = start.elapsed();
    Ok(FtsSyncResult {
        segments_indexed: total_indexed,
        panes_processed,
        full_rebuild: true,
        duration_ms: duration.as_millis() as u64,
        warnings,
    })
}

fn sync_fts_on_startup_backend(
    backend: &dyn StorageBackend,
    config: &FtsSyncConfig,
) -> Result<FtsSyncResult> {
    use std::time::Instant;
    let start = Instant::now();
    let mut warnings = Vec::new();

    let fts_ok = check_fts_integrity_backend(backend)?;
    if !fts_ok {
        tracing::warn!("FTS index corruption detected, performing full rebuild");
        return full_fts_rebuild_backend(backend, config);
    }

    let state = get_fts_index_state_backend(backend)?;
    if let Some(ref s) = state {
        if s.index_version != FTS_INDEX_VERSION {
            tracing::info!(
                old_version = s.index_version,
                new_version = FTS_INDEX_VERSION,
                "FTS index version mismatch or rebuild-pending marker detected, performing full rebuild"
            );
            return full_fts_rebuild_backend(backend, config);
        }
    } else {
        let now = now_ms();
        let new_state = FtsIndexState {
            index_version: FTS_INDEX_VERSION,
            last_full_rebuild_at: None,
            created_at: now,
            updated_at: now,
        };
        upsert_fts_index_state_backend(backend, &new_state)?;
    }

    let pane_ids = panes_needing_fts_sync_backend(backend)?;
    let mut total_indexed = 0u64;
    let panes_processed = pane_ids.len() as u64;

    for pane_id in pane_ids {
        match sync_fts_for_pane_backend(backend, pane_id, config) {
            Ok((indexed, _)) => total_indexed = total_indexed.saturating_add(indexed),
            Err(err) => warnings.push(format!("Pane {pane_id} incremental sync failed: {err}")),
        }
    }

    let duration = start.elapsed();
    Ok(FtsSyncResult {
        segments_indexed: total_indexed,
        panes_processed,
        full_rebuild: false,
        duration_ms: duration.as_millis() as u64,
        warnings,
    })
}

const AGENT_SESSION_SELECT_COLUMNS: &str =
    "id, pane_id, agent_type, session_id, external_id, external_meta,
         started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
         cached_tokens, reasoning_tokens, model_name, estimated_cost_usd";

fn optional_f64_from_backend_cells(
    row: &CellRowReader<'_>,
    idx: usize,
    context: &str,
) -> Result<Option<f64>> {
    if row.is_null(idx) {
        return Ok(None);
    }
    row.f64(idx)
        .map(Some)
        .map_err(|err| storage_backend_error(context, err).into())
}

fn agent_session_from_backend_cells(row: &[SqlCell]) -> Result<AgentSessionRecord> {
    let row = CellRowReader::new(row);
    let external_meta_str = row
        .optional_string(5)
        .map_err(|err| storage_backend_error("agent session external_meta", err))?;
    // br-ft-4d6ic: route silent serde failure through observability counter.
    let external_meta = parse_storage_json_col::<serde_json::Value>(
        external_meta_str.as_deref(),
        "agent_sessions",
        "external_meta",
    );
    Ok(AgentSessionRecord {
        id: row
            .i64(0)
            .map_err(|err| storage_backend_error("agent session id", err))?,
        pane_id: row
            .i64(1)
            .and_then(|value| backend_i64_to_u64(value, "agent_sessions.pane_id"))
            .map_err(|err| storage_backend_error("agent session pane_id", err))?,
        agent_type: row
            .string(2)
            .map_err(|err| storage_backend_error("agent session agent_type", err))?,
        session_id: row
            .optional_string(3)
            .map_err(|err| storage_backend_error("agent session session_id", err))?,
        external_id: row
            .optional_string(4)
            .map_err(|err| storage_backend_error("agent session external_id", err))?,
        external_meta,
        started_at: row
            .i64(6)
            .map_err(|err| storage_backend_error("agent session started_at", err))?,
        ended_at: row
            .optional_i64(7)
            .map_err(|err| storage_backend_error("agent session ended_at", err))?,
        end_reason: row
            .optional_string(8)
            .map_err(|err| storage_backend_error("agent session end_reason", err))?,
        total_tokens: row
            .optional_i64(9)
            .map_err(|err| storage_backend_error("agent session total_tokens", err))?,
        input_tokens: row
            .optional_i64(10)
            .map_err(|err| storage_backend_error("agent session input_tokens", err))?,
        output_tokens: row
            .optional_i64(11)
            .map_err(|err| storage_backend_error("agent session output_tokens", err))?,
        cached_tokens: row
            .optional_i64(12)
            .map_err(|err| storage_backend_error("agent session cached_tokens", err))?,
        reasoning_tokens: row
            .optional_i64(13)
            .map_err(|err| storage_backend_error("agent session reasoning_tokens", err))?,
        model_name: row
            .optional_string(14)
            .map_err(|err| storage_backend_error("agent session model_name", err))?,
        estimated_cost_usd: optional_f64_from_backend_cells(
            &row,
            15,
            "agent session estimated_cost_usd",
        )?,
    })
}

/// Query an agent session by ID
fn query_agent_session_backend(
    backend: &dyn StorageBackend,
    session_id: i64,
) -> Result<Option<AgentSessionRecord>> {
    let row = backend
        .query_row_cells(
            &format!(
                "SELECT {AGENT_SESSION_SELECT_COLUMNS}
                 FROM agent_sessions WHERE id = ?1"
            ),
            &[ToSqlValue::Integer(session_id)],
        )
        .map_err(|err| storage_backend_error("Query agent session", err))?;
    row.as_deref()
        .map(agent_session_from_backend_cells)
        .transpose()
}

/// Query active agent sessions (ended_at IS NULL)
fn query_active_sessions_backend(backend: &dyn StorageBackend) -> Result<Vec<AgentSessionRecord>> {
    let rows = backend
        .query_map_cells(
            &format!(
                "SELECT {AGENT_SESSION_SELECT_COLUMNS}
                 FROM agent_sessions WHERE ended_at IS NULL
                 ORDER BY started_at DESC"
            ),
            &[],
        )
        .map_err(|err| storage_backend_error("Query active agent sessions", err))?;
    rows.iter()
        .map(|row| agent_session_from_backend_cells(row))
        .collect()
}

/// Query agent sessions for a specific pane
fn query_sessions_for_pane_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
) -> Result<Vec<AgentSessionRecord>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let rows = backend
        .query_map_cells(
            &format!(
                "SELECT {AGENT_SESSION_SELECT_COLUMNS}
                 FROM agent_sessions WHERE pane_id = ?1
                 ORDER BY started_at DESC"
            ),
            &[ToSqlValue::Integer(pane_id_i64)],
        )
        .map_err(|err| storage_backend_error("Query sessions for pane", err))?;
    rows.iter()
        .map(|row| agent_session_from_backend_cells(row))
        .collect()
}

fn stored_event_from_backend_cells(row: &[SqlCell]) -> Result<StoredEvent> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(1)
        .and_then(|value| backend_i64_to_u64(value, "events.pane_id"))
        .map_err(|err| storage_backend_error("Event row pane_id", err))?;
    let extracted_str = reader
        .optional_string(7)
        .map_err(|err| storage_backend_error("Event row extracted", err))?;
    // br-ft-4d6ic: route silent serde failure through observability counter.
    let extracted = parse_storage_json_col::<serde_json::Value>(
        extracted_str.as_deref(),
        "events",
        "extracted",
    );

    Ok(StoredEvent {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Event row id", err))?,
        pane_id,
        rule_id: reader
            .string(2)
            .map_err(|err| storage_backend_error("Event row rule_id", err))?,
        agent_type: reader
            .string(3)
            .map_err(|err| storage_backend_error("Event row agent_type", err))?,
        event_type: reader
            .string(4)
            .map_err(|err| storage_backend_error("Event row event_type", err))?,
        severity: reader
            .string(5)
            .map_err(|err| storage_backend_error("Event row severity", err))?,
        confidence: reader
            .f64(6)
            .map_err(|err| storage_backend_error("Event row confidence", err))?,
        extracted,
        matched_text: reader
            .optional_string(8)
            .map_err(|err| storage_backend_error("Event row matched_text", err))?,
        segment_id: reader
            .optional_i64(9)
            .map_err(|err| storage_backend_error("Event row segment_id", err))?,
        detected_at: reader
            .i64(10)
            .map_err(|err| storage_backend_error("Event row detected_at", err))?,
        dedupe_key: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Event row dedupe_key", err))?,
        handled_at: reader
            .optional_i64(12)
            .map_err(|err| storage_backend_error("Event row handled_at", err))?,
        handled_by_workflow_id: reader
            .optional_string(13)
            .map_err(|err| storage_backend_error("Event row handled_by_workflow_id", err))?,
        handled_status: reader
            .optional_string(14)
            .map_err(|err| storage_backend_error("Event row handled_status", err))?,
    })
}

fn query_unhandled_events_backend(
    backend: &dyn StorageBackend,
    limit: usize,
) -> Result<Vec<StoredEvent>> {
    let limit_i64 = usize_to_i64(limit, "limit")?;
    let rows = backend
        .query_map_cells(
            "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
             extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
             handled_by_workflow_id, handled_status
             FROM events
             WHERE handled_at IS NULL
             ORDER BY detected_at DESC
             LIMIT ?1",
            &[ToSqlValue::Integer(limit_i64)],
        )
        .map_err(|err| storage_backend_error("Query unhandled events", err))?;

    rows.iter()
        .map(|row| stored_event_from_backend_cells(row))
        .collect()
}

/// Count unhandled events per pane
fn query_unhandled_event_counts(
    backend: &dyn StorageBackend,
) -> Result<std::collections::HashMap<u64, u32>> {
    let rows = backend
        .query_map_typed(
            "SELECT pane_id, COUNT(*) as cnt
             FROM events
             WHERE handled_at IS NULL
             GROUP BY pane_id",
            &[],
        )
        .map_err(|err| storage_backend_error("Query unhandled event counts", err))?;

    let mut result = std::collections::HashMap::new();
    for row in rows {
        let reader = RowReader::new(&row);
        let pane_id = reader
            .i64(0)
            .map_err(|err| storage_backend_error("Unhandled event count pane_id", err))?;
        let count = reader
            .i64(1)
            .map_err(|err| storage_backend_error("Unhandled event count", err))?;
        let pane_id = u64::try_from(pane_id).unwrap_or(0);
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        result.insert(pane_id, count);
    }

    Ok(result)
}

fn query_last_activity_by_pane_backend(
    backend: &dyn StorageBackend,
) -> Result<std::collections::HashMap<u64, i64>> {
    let rows = backend
        .query_map_cells(
            "SELECT pane_id, MAX(captured_at) as last_activity
             FROM output_segments
             GROUP BY pane_id",
            &[],
        )
        .map_err(|err| storage_backend_error("Query last activity by pane", err))?;

    let mut result = std::collections::HashMap::new();
    for row in &rows {
        let reader = CellRowReader::new(row);
        let pane_id = reader
            .i64(0)
            .and_then(|value| backend_i64_to_u64(value, "output_segments.pane_id"))
            .map_err(|err| storage_backend_error("Last activity row pane_id", err))?;
        let last_activity = reader
            .i64(1)
            .map_err(|err| storage_backend_error("Last activity row timestamp", err))?;
        result.insert(pane_id, last_activity);
    }

    Ok(result)
}

/// Query events with optional filters.
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_events_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_events(conn: &Connection, query: &EventQuery) -> Result<Vec<StoredEvent>> {
    let mut sql = String::from(
        "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
         extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
         handled_by_workflow_id, handled_status
         FROM events WHERE 1=1",
    );

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if query.unhandled_only {
        sql.push_str(" AND handled_at IS NULL");
    }

    if let Some(pane_id) = query.pane_id {
        sql.push_str(" AND pane_id = ?");
        #[allow(clippy::cast_possible_wrap)]
        params.push(Box::new(pane_id as i64));
    }

    if let Some(ref rule_id) = query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(Box::new(rule_id.clone()));
    }

    if let Some(ref event_type) = query.event_type {
        sql.push_str(" AND event_type = ?");
        params.push(Box::new(event_type.clone()));
    }

    if let Some(ref triage_state) = query.triage_state {
        sql.push_str(" AND triage_state = ?");
        params.push(Box::new(triage_state.clone()));
    }

    if let Some(ref label) = query.label {
        sql.push_str(" AND id IN (SELECT event_id FROM event_labels WHERE label = ?)");
        params.push(Box::new(label.clone()));
    }

    if let Some(since) = query.since {
        sql.push_str(" AND detected_at >= ?");
        params.push(Box::new(since));
    }

    if let Some(until) = query.until {
        sql.push_str(" AND detected_at <= ?");
        params.push(Box::new(until));
    }

    sql.push_str(" ORDER BY detected_at DESC");

    let limit = query.limit.unwrap_or(20);
    let limit_i64 = usize_to_i64(limit, "limit")?;
    sql.push_str(" LIMIT ?");
    params.push(Box::new(limit_i64));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let extracted_str: Option<String> = row.get(7)?;
            // br-ft-4d6ic: route silent serde failure through observability counter.
            let extracted = parse_storage_json_col::<serde_json::Value>(
                extracted_str.as_deref(),
                "events",
                "extracted",
            );

            Ok(StoredEvent {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                rule_id: row.get(2)?,
                agent_type: row.get(3)?,
                event_type: row.get(4)?,
                severity: row.get(5)?,
                confidence: row.get(6)?,
                extracted,
                matched_text: row.get(8)?,
                segment_id: row.get(9)?,
                detected_at: row.get(10)?,
                dedupe_key: row.get(11)?,
                handled_at: row.get(12)?,
                handled_by_workflow_id: row.get(13)?,
                handled_status: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_events_backend(
    backend: &dyn StorageBackend,
    query: &EventQuery,
) -> Result<Vec<StoredEvent>> {
    let mut sql = String::from(
        "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
         extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
         handled_by_workflow_id, handled_status
         FROM events WHERE 1=1",
    );
    let mut params = Vec::new();

    if query.unhandled_only {
        sql.push_str(" AND handled_at IS NULL");
    }

    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(ToSqlValue::Integer(pane_id_i64));
    }

    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(ToSqlValue::Text(rule_id.as_str()));
    }

    if let Some(event_type) = &query.event_type {
        sql.push_str(" AND event_type = ?");
        params.push(ToSqlValue::Text(event_type.as_str()));
    }

    if let Some(triage_state) = &query.triage_state {
        sql.push_str(" AND triage_state = ?");
        params.push(ToSqlValue::Text(triage_state.as_str()));
    }

    if let Some(label) = &query.label {
        sql.push_str(" AND id IN (SELECT event_id FROM event_labels WHERE label = ?)");
        params.push(ToSqlValue::Text(label.as_str()));
    }

    if let Some(since) = query.since {
        sql.push_str(" AND detected_at >= ?");
        params.push(ToSqlValue::Integer(since));
    }

    if let Some(until) = query.until {
        sql.push_str(" AND detected_at <= ?");
        params.push(ToSqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY detected_at DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(20), "limit")?;
    params.push(ToSqlValue::Integer(limit_i64));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query events", err))?;

    rows.iter()
        .map(|row| stored_event_from_backend_cells(row))
        .collect()
}

/// Query events in deterministic ID order with cursor-based resume.
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_events_stream_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_events_stream(conn: &Connection, query: &EventStreamQuery) -> Result<Vec<StoredEvent>> {
    let mut sql = String::from(
        "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
         extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
         handled_by_workflow_id, handled_status
         FROM events WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(after_id) = query.after_id {
        sql.push_str(" AND id > ?");
        params.push(SqlValue::Integer(after_id));
    }
    if query.unhandled_only {
        sql.push_str(" AND handled_at IS NULL");
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(event_type) = &query.event_type {
        sql.push_str(" AND event_type = ?");
        params.push(SqlValue::Text(event_type.clone()));
    }
    if let Some(triage_state) = &query.triage_state {
        sql.push_str(" AND triage_state = ?");
        params.push(SqlValue::Text(triage_state.clone()));
    }
    if let Some(label) = &query.label {
        sql.push_str(" AND id IN (SELECT event_id FROM event_labels WHERE label = ?)");
        params.push(SqlValue::Text(label.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND detected_at >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND detected_at <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY id ASC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare stream query: {e}")))?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            let extracted_str: Option<String> = row.get(7)?;
            // br-ft-4d6ic: route silent serde failure through observability counter.
            let extracted = parse_storage_json_col::<serde_json::Value>(
                extracted_str.as_deref(),
                "events",
                "extracted",
            );

            Ok(StoredEvent {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                rule_id: row.get(2)?,
                agent_type: row.get(3)?,
                event_type: row.get(4)?,
                severity: row.get(5)?,
                confidence: row.get(6)?,
                extracted,
                matched_text: row.get(8)?,
                segment_id: row.get(9)?,
                detected_at: row.get(10)?,
                dedupe_key: row.get(11)?,
                handled_at: row.get(12)?,
                handled_by_workflow_id: row.get(13)?,
                handled_status: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Stream query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_events_stream_backend(
    backend: &dyn StorageBackend,
    query: &EventStreamQuery,
) -> Result<Vec<StoredEvent>> {
    let mut sql = String::from(
        "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
         extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
         handled_by_workflow_id, handled_status
         FROM events WHERE 1=1",
    );
    let mut params = Vec::new();

    if let Some(after_id) = query.after_id {
        sql.push_str(" AND id > ?");
        params.push(ToSqlValue::Integer(after_id));
    }
    if query.unhandled_only {
        sql.push_str(" AND handled_at IS NULL");
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(ToSqlValue::Integer(pane_id_i64));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(ToSqlValue::Text(rule_id.as_str()));
    }
    if let Some(event_type) = &query.event_type {
        sql.push_str(" AND event_type = ?");
        params.push(ToSqlValue::Text(event_type.as_str()));
    }
    if let Some(triage_state) = &query.triage_state {
        sql.push_str(" AND triage_state = ?");
        params.push(ToSqlValue::Text(triage_state.as_str()));
    }
    if let Some(label) = &query.label {
        sql.push_str(" AND id IN (SELECT event_id FROM event_labels WHERE label = ?)");
        params.push(ToSqlValue::Text(label.as_str()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND detected_at >= ?");
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND detected_at <= ?");
        params.push(ToSqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY id ASC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(ToSqlValue::Integer(limit_i64));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query events stream", err))?;

    rows.iter()
        .map(|row| stored_event_from_backend_cells(row))
        .collect()
}

// =============================================================================
// Timeline Query Implementation (wa-6sk.1)
// =============================================================================

/// Query timeline with unified event view across panes.
///
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_timeline_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_timeline(conn: &Connection, query: &TimelineQuery) -> Result<Timeline> {
    let select_sql = String::from(
        "SELECT e.id, e.pane_id, e.rule_id, e.agent_type, e.event_type, e.severity,
                e.confidence, e.detected_at, e.handled_at, e.handled_by_workflow_id,
                e.handled_status, e.matched_text,
                p.pane_uuid, p.domain, p.cwd, p.title",
    );
    let from_sql = String::from(" FROM events e JOIN panes p ON p.pane_id = e.pane_id");
    let mut where_sql = String::from(" WHERE 1=1");

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    // Time range filters
    if let Some(start) = query.start {
        where_sql.push_str(" AND e.detected_at >= ?");
        params.push(Box::new(start));
    }

    if let Some(end) = query.end {
        where_sql.push_str(" AND e.detected_at <= ?");
        params.push(Box::new(end));
    }

    // Pane filter
    if let Some(ref pane_ids) = query.pane_ids {
        if !pane_ids.is_empty() {
            let pane_id_set = PaneIdSet::from_pane_ids(pane_ids.iter().copied());
            if let Some(predicate) =
                pane_id_set.as_sql_in_clause_for_column("e.pane_id", TIMELINE_PANE_ID_INLINE_LIMIT)
            {
                where_sql.push_str(" AND ");
                where_sql.push_str(&predicate);
            } else {
                stage_timeline_pane_id_temp_table(conn, &pane_id_set)?;
                where_sql.push_str(
                    " AND EXISTS (
                        SELECT 1 FROM temp_pane_id_set staged_pane_ids
                        WHERE staged_pane_ids.pane_id = e.pane_id
                    )",
                );
            }
        }
    }

    // Severity filter
    if let Some(ref severities) = query.severities {
        if !severities.is_empty() {
            let placeholders: Vec<&str> = severities.iter().map(|_| "?").collect();
            where_sql.push_str(&format!(" AND e.severity IN ({})", placeholders.join(",")));
            for s in severities {
                params.push(Box::new(s.clone()));
            }
        }
    }

    // Event type filter
    if let Some(ref event_types) = query.event_types {
        if !event_types.is_empty() {
            let placeholders: Vec<&str> = event_types.iter().map(|_| "?").collect();
            where_sql.push_str(&format!(
                " AND e.event_type IN ({})",
                placeholders.join(",")
            ));
            for et in event_types {
                params.push(Box::new(et.clone()));
            }
        }
    }

    // Agent type filter
    if let Some(ref agent_types) = query.agent_types {
        if !agent_types.is_empty() {
            let placeholders: Vec<&str> = agent_types.iter().map(|_| "?").collect();
            where_sql.push_str(&format!(
                " AND e.agent_type IN ({})",
                placeholders.join(",")
            ));
            for at in agent_types {
                params.push(Box::new(at.clone()));
            }
        }
    }

    // Unhandled filter
    if query.unhandled_only {
        where_sql.push_str(" AND e.handled_at IS NULL");
    }

    // Count total before pagination
    let count_sql = format!("SELECT COUNT(*){from_sql}{where_sql}");

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let total_count: i64 = conn
        .query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or(0);

    let mut sql = format!("{select_sql}{from_sql}{where_sql} ORDER BY e.detected_at ASC");
    let limit_i64 = usize_to_i64(query.limit, "limit")?;
    let offset_i64 = usize_to_i64(query.offset, "offset")?;
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push(Box::new(limit_i64));
    params.push(Box::new(offset_i64));

    // Execute query
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare timeline query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let pane_id: i64 = row.get(1)?;
            let pane_id_u64 = pane_id as u64;

            Ok(TimelineEvent {
                id: row.get(0)?,
                timestamp: row.get(7)?,
                pane_info: PaneInfo {
                    pane_id: pane_id_u64,
                    pane_uuid: row.get(12)?,
                    agent_type: {
                        let at: String = row.get(3)?;
                        if at == "unknown" { None } else { Some(at) }
                    },
                    domain: row.get(13)?,
                    cwd: row.get(14)?,
                    title: row.get(15)?,
                },
                rule_id: row.get(2)?,
                event_type: row.get(4)?,
                severity: row.get(5)?,
                confidence: row.get(6)?,
                handled: {
                    let handled_at: Option<i64> = row.get(8)?;
                    handled_at.map(|ts| HandledInfo {
                        handled_at: ts,
                        workflow_id: row.get(9).ok().flatten(),
                        status: row
                            .get::<_, Option<String>>(10)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "unknown".to_string()),
                    })
                },
                correlations: Vec::new(), // Populated later if requested
                summary: row.get::<_, Option<String>>(11).ok().flatten(),
            })
        })
        .map_err(|e| StorageError::Database(format!("Timeline query failed: {e}")))?;

    let mut events = Vec::new();
    for row in rows {
        events.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    let total_count_u64 = u64::try_from(total_count).unwrap_or(0);
    Ok(assemble_timeline(query, events, total_count_u64))
}

fn assemble_timeline(
    query: &TimelineQuery,
    events: Vec<TimelineEvent>,
    total_count: u64,
) -> Timeline {
    let (start, end) = if events.is_empty() {
        let now = now_ms();
        (query.start.unwrap_or(now), query.end.unwrap_or(now))
    } else {
        (
            events.first().map_or(0, |e| e.timestamp),
            events.last().map_or(0, |e| e.timestamp),
        )
    };

    // Detect correlations if requested
    let correlations = if query.include_correlations && !events.is_empty() {
        detect_correlations(&events)
    } else {
        Vec::new()
    };

    // Attach correlation refs to events
    let mut events_with_refs = events;
    for event in &mut events_with_refs {
        event.correlations = correlations
            .iter()
            .filter(|c| c.event_ids.contains(&event.id))
            .map(|c| CorrelationRef {
                id: c.id.clone(),
                correlation_type: c.correlation_type,
            })
            .collect();
    }

    let total_count_usize = usize::try_from(total_count).unwrap_or(usize::MAX);
    let has_more = query.offset.saturating_add(events_with_refs.len()) < total_count_usize;

    Timeline {
        start,
        end,
        events: events_with_refs,
        correlations,
        total_count,
        has_more,
    }
}

fn timeline_event_from_backend_cells(row: &[SqlCell]) -> Result<TimelineEvent> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(1)
        .and_then(|value| backend_i64_to_u64(value, "timeline.pane_id"))
        .map_err(|err| storage_backend_error("Timeline row pane_id", err))?;
    let agent_type = reader
        .string(3)
        .map_err(|err| storage_backend_error("Timeline row agent_type", err))?;
    let handled_at = reader
        .optional_i64(8)
        .map_err(|err| storage_backend_error("Timeline row handled_at", err))?;
    let handled = if let Some(handled_at) = handled_at {
        let status = reader
            .optional_string(10)
            .map_err(|err| storage_backend_error("Timeline row handled_status", err))?
            .unwrap_or_else(|| "unknown".to_string());
        Some(HandledInfo {
            handled_at,
            workflow_id: reader
                .optional_string(9)
                .map_err(|err| storage_backend_error("Timeline row handled_by_workflow_id", err))?,
            status,
        })
    } else {
        None
    };

    Ok(TimelineEvent {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Timeline row id", err))?,
        timestamp: reader
            .i64(7)
            .map_err(|err| storage_backend_error("Timeline row detected_at", err))?,
        pane_info: PaneInfo {
            pane_id,
            pane_uuid: reader
                .optional_string(12)
                .map_err(|err| storage_backend_error("Timeline row pane_uuid", err))?,
            agent_type: if agent_type == "unknown" {
                None
            } else {
                Some(agent_type)
            },
            domain: reader
                .string(13)
                .map_err(|err| storage_backend_error("Timeline row domain", err))?,
            cwd: reader
                .optional_string(14)
                .map_err(|err| storage_backend_error("Timeline row cwd", err))?,
            title: reader
                .optional_string(15)
                .map_err(|err| storage_backend_error("Timeline row title", err))?,
        },
        rule_id: reader
            .string(2)
            .map_err(|err| storage_backend_error("Timeline row rule_id", err))?,
        event_type: reader
            .string(4)
            .map_err(|err| storage_backend_error("Timeline row event_type", err))?,
        severity: reader
            .string(5)
            .map_err(|err| storage_backend_error("Timeline row severity", err))?,
        confidence: reader
            .f64(6)
            .map_err(|err| storage_backend_error("Timeline row confidence", err))?,
        handled,
        correlations: Vec::new(),
        summary: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Timeline row matched_text", err))?,
    })
}

fn query_timeline_backend(backend: &dyn StorageBackend, query: &TimelineQuery) -> Result<Timeline> {
    let select_sql = String::from(
        "SELECT e.id, e.pane_id, e.rule_id, e.agent_type, e.event_type, e.severity,
                e.confidence, e.detected_at, e.handled_at, e.handled_by_workflow_id,
                e.handled_status, e.matched_text,
                p.pane_uuid, p.domain, p.cwd, p.title",
    );
    let from_sql = String::from(" FROM events e JOIN panes p ON p.pane_id = e.pane_id");
    let mut where_sql = String::from(" WHERE 1=1");
    let mut params = Vec::new();

    if let Some(start) = query.start {
        where_sql.push_str(" AND e.detected_at >= ?");
        params.push(ToSqlValue::Integer(start));
    }
    if let Some(end) = query.end {
        where_sql.push_str(" AND e.detected_at <= ?");
        params.push(ToSqlValue::Integer(end));
    }
    if let Some(ref pane_ids) = query.pane_ids {
        if !pane_ids.is_empty() {
            let pane_id_set = PaneIdSet::from_pane_ids(pane_ids.iter().copied());
            if let Some(predicate) =
                pane_id_set.as_sql_in_clause_for_column("e.pane_id", TIMELINE_PANE_ID_INLINE_LIMIT)
            {
                where_sql.push_str(" AND ");
                where_sql.push_str(&predicate);
            } else {
                stage_timeline_pane_id_temp_table_backend(backend, &pane_id_set)?;
                where_sql.push_str(
                    " AND EXISTS (
                        SELECT 1 FROM temp_pane_id_set staged_pane_ids
                        WHERE staged_pane_ids.pane_id = e.pane_id
                    )",
                );
            }
        }
    }
    if let Some(ref severities) = query.severities {
        if !severities.is_empty() {
            let placeholders: Vec<&str> = severities.iter().map(|_| "?").collect();
            where_sql.push_str(&format!(" AND e.severity IN ({})", placeholders.join(",")));
            for severity in severities {
                params.push(ToSqlValue::Text(severity.as_str()));
            }
        }
    }
    if let Some(ref event_types) = query.event_types {
        if !event_types.is_empty() {
            let placeholders: Vec<&str> = event_types.iter().map(|_| "?").collect();
            where_sql.push_str(&format!(
                " AND e.event_type IN ({})",
                placeholders.join(",")
            ));
            for event_type in event_types {
                params.push(ToSqlValue::Text(event_type.as_str()));
            }
        }
    }
    if let Some(ref agent_types) = query.agent_types {
        if !agent_types.is_empty() {
            let placeholders: Vec<&str> = agent_types.iter().map(|_| "?").collect();
            where_sql.push_str(&format!(
                " AND e.agent_type IN ({})",
                placeholders.join(",")
            ));
            for agent_type in agent_types {
                params.push(ToSqlValue::Text(agent_type.as_str()));
            }
        }
    }
    if query.unhandled_only {
        where_sql.push_str(" AND e.handled_at IS NULL");
    }

    let count_sql = format!("SELECT COUNT(*){from_sql}{where_sql}");
    let total_count = backend
        .query_row_cells(&count_sql, &params)
        .map_err(|err| storage_backend_error("Timeline count query", err))?
        .map(|row| {
            let reader = CellRowReader::new(&row);
            reader
                .i64(0)
                .and_then(|value| backend_i64_to_u64(value, "timeline.total_count"))
        })
        .transpose()
        .map_err(|err| storage_backend_error("Timeline count row", err))?
        .unwrap_or(0);

    let mut sql = format!("{select_sql}{from_sql}{where_sql} ORDER BY e.detected_at ASC");
    let limit_i64 = usize_to_i64(query.limit, "limit")?;
    let offset_i64 = usize_to_i64(query.offset, "offset")?;
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push(ToSqlValue::Integer(limit_i64));
    params.push(ToSqlValue::Integer(offset_i64));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Timeline query", err))?;
    let events = rows
        .iter()
        .map(Vec::as_slice)
        .map(timeline_event_from_backend_cells)
        .collect::<Result<Vec<_>>>()?;

    Ok(assemble_timeline(query, events, total_count))
}

fn stage_timeline_pane_id_temp_table(conn: &Connection, pane_id_set: &PaneIdSet) -> Result<()> {
    let plan = pane_id_set.temp_table_plan();
    create_and_fill_pane_id_temp_table(conn, &plan)
}

fn create_and_fill_pane_id_temp_table(conn: &Connection, plan: &PaneIdTempTablePlan) -> Result<()> {
    conn.execute_batch(plan.create_sql).map_err(|err| {
        StorageError::Database(format!("Failed to create pane id temp table: {err}"))
    })?;
    conn.execute(plan.clear_sql, []).map_err(|err| {
        StorageError::Database(format!("Failed to clear pane id temp table: {err}"))
    })?;

    let mut stmt = conn.prepare(plan.insert_sql).map_err(|err| {
        StorageError::Database(format!("Failed to prepare pane id temp insert: {err}"))
    })?;
    for &pane_id in &plan.pane_ids {
        let pane_id = u64_to_i64(pane_id, "temp_pane_id_set.pane_id")?;
        stmt.execute([pane_id])
            .map_err(|err| StorageError::Database(format!("Failed to stage pane id: {err}")))?;
    }

    Ok(())
}

fn stage_timeline_pane_id_temp_table_backend(
    backend: &dyn StorageBackend,
    pane_id_set: &PaneIdSet,
) -> Result<()> {
    let plan = pane_id_set.temp_table_plan();
    create_and_fill_pane_id_temp_table_backend(backend, &plan)
}

fn create_and_fill_pane_id_temp_table_backend(
    backend: &dyn StorageBackend,
    plan: &PaneIdTempTablePlan,
) -> Result<()> {
    backend
        .execute_batch(plan.create_sql)
        .map_err(|err| storage_backend_error("Create pane id temp table", err))?;
    backend
        .execute_batch(plan.clear_sql)
        .map_err(|err| storage_backend_error("Clear pane id temp table", err))?;

    let param_rows = plan
        .pane_ids
        .iter()
        .map(|&pane_id| {
            u64_to_i64(pane_id, "temp_pane_id_set.pane_id")
                .map(|pane_id| vec![ToSqlValue::Integer(pane_id)])
        })
        .collect::<Result<Vec<_>>>()?;

    backend
        .execute_many(plan.insert_sql, &param_rows)
        .map_err(|err| storage_backend_error("Stage pane id", err))?;

    Ok(())
}

/// Detect correlations between timeline events
fn detect_correlations(events: &[TimelineEvent]) -> Vec<Correlation> {
    let mut correlations = Vec::new();
    let mut correlation_counter = 0u64;

    // Temporal correlation: events within 10 seconds of each other
    const TEMPORAL_WINDOW_MS: i64 = 10_000;
    // Cascade correlation window: 30 seconds
    const CASCADE_WINDOW_MS: i64 = 30_000;
    // Failover correlation window: 5 minutes
    const FAILOVER_WINDOW_MS: i64 = 300_000;
    // DedupeGroup window: same rule_id across different panes within 30 seconds
    const DEDUPE_GROUP_WINDOW_MS: i64 = 30_000;

    fn rule_prefix(rule_id: &str) -> Option<&str> {
        let prefix = rule_id.split('.').next()?;
        match prefix {
            "codex" | "claude_code" | "gemini" | "wezterm" => Some(prefix),
            _ => None,
        }
    }

    fn event_agent_type(event: &TimelineEvent) -> Option<&str> {
        if let Some(prefix) = rule_prefix(&event.rule_id) {
            Some(prefix)
        } else {
            event.pane_info.agent_type.as_deref()
        }
    }

    fn is_usage_limit_event(event: &TimelineEvent) -> bool {
        event.event_type == "usage.reached"
            || event.event_type == "usage_limit"
            || event.rule_id.contains("usage.reached")
            || event.rule_id.contains("usage_limit")
    }

    fn is_session_start_event(event: &TimelineEvent) -> bool {
        event.event_type == "session.start"
            || event.event_type == "session_start"
            || event.rule_id.contains("session.start")
            || event.rule_id.contains("session_start")
    }

    fn is_recovery_event(event: &TimelineEvent) -> bool {
        event.event_type.starts_with("session.")
            || event.event_type.starts_with("session_")
            || event.rule_id.contains("session.resume")
            || event.rule_id.contains("session.start")
            || event.rule_id.contains("session_resume")
            || event.rule_id.contains("session_start")
    }

    // Find temporal clusters
    let mut i = 0;
    while i < events.len() {
        let base_event = &events[i];
        let mut cluster = vec![base_event.id];
        let mut j = i + 1;

        // Collect events within temporal window
        while j < events.len() {
            let candidate = &events[j];
            if candidate.timestamp - base_event.timestamp <= TEMPORAL_WINDOW_MS {
                // Different panes = more interesting correlation
                if candidate.pane_info.pane_id != base_event.pane_info.pane_id {
                    cluster.push(candidate.id);
                }
            } else {
                break;
            }
            j += 1;
        }

        // Only create correlation if multiple events from different panes
        if cluster.len() > 1 {
            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-temporal-{correlation_counter}"),
                event_ids: cluster,
                correlation_type: CorrelationType::Temporal,
                confidence: 0.6,
                description: "Events occurred within 10 seconds across different panes".to_string(),
            });
        }

        i += 1;
    }

    // Workflow group correlation: events handled by same workflow
    let mut workflow_groups: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();

    for event in events {
        if let Some(ref handled) = event.handled {
            if let Some(ref wf_id) = handled.workflow_id {
                workflow_groups
                    .entry(wf_id.clone())
                    .or_default()
                    .push(event.id);
            }
        }
    }

    for (wf_id, event_ids) in workflow_groups {
        if event_ids.len() > 1 {
            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-workflow-{correlation_counter}"),
                event_ids,
                correlation_type: CorrelationType::WorkflowGroup,
                confidence: 0.95,
                description: format!("Events handled by workflow {wf_id}"),
            });
        }
    }

    // Cascade correlation: error/critical in one pane followed by recovery event elsewhere
    for (i, event) in events.iter().enumerate() {
        let severity = event.severity.to_lowercase();
        if severity != "error" && severity != "critical" {
            continue;
        }

        let agent = event_agent_type(event);

        for later_event in events.iter().skip(i + 1) {
            if later_event.timestamp - event.timestamp > CASCADE_WINDOW_MS {
                break;
            }
            if later_event.pane_info.pane_id == event.pane_info.pane_id {
                continue;
            }
            if !is_recovery_event(later_event) {
                continue;
            }

            let later_agent = event_agent_type(later_event);
            if agent.is_some() && later_agent.is_some() && agent != later_agent {
                continue;
            }

            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-cascade-{correlation_counter}"),
                event_ids: vec![event.id, later_event.id],
                correlation_type: CorrelationType::Cascade,
                confidence: 0.75,
                description: "Error followed by recovery event in another pane".to_string(),
            });
            break;
        }
    }

    // DedupeGroup correlation: same rule_id firing across different panes within window
    {
        let mut rule_groups: std::collections::HashMap<&str, Vec<&TimelineEvent>> =
            std::collections::HashMap::new();
        for event in events {
            rule_groups
                .entry(event.rule_id.as_str())
                .or_default()
                .push(event);
        }
        for (rule_id, group) in &rule_groups {
            if group.len() < 2 {
                continue;
            }
            // Check if events span multiple panes and are within the window
            let pane_ids: std::collections::HashSet<u64> =
                group.iter().map(|e| e.pane_info.pane_id).collect();
            if pane_ids.len() < 2 {
                continue;
            }
            // Find clusters within the dedupe window
            let mut sorted = group.clone();
            sorted.sort_by_key(|e| e.timestamp);
            let mut cluster_start = 0;
            while cluster_start < sorted.len() {
                let base_ts = sorted[cluster_start].timestamp;
                let mut cluster_ids = vec![sorted[cluster_start].id];
                let mut cluster_panes =
                    std::collections::HashSet::from([sorted[cluster_start].pane_info.pane_id]);
                let mut j = cluster_start + 1;
                while j < sorted.len() && sorted[j].timestamp - base_ts <= DEDUPE_GROUP_WINDOW_MS {
                    cluster_ids.push(sorted[j].id);
                    cluster_panes.insert(sorted[j].pane_info.pane_id);
                    j += 1;
                }
                if cluster_ids.len() >= 2 && cluster_panes.len() >= 2 {
                    correlation_counter += 1;
                    correlations.push(Correlation {
                        id: format!("corr-dedupe-{correlation_counter}"),
                        event_ids: cluster_ids,
                        correlation_type: CorrelationType::DedupeGroup,
                        confidence: 0.7,
                        description: format!(
                            "Same rule '{}' fired across {} panes",
                            rule_id,
                            cluster_panes.len()
                        ),
                    });
                }
                cluster_start = j;
            }
        }
    }

    // Failover correlation: usage limit followed by new session in different pane
    for (i, event) in events.iter().enumerate() {
        if !is_usage_limit_event(event) {
            continue;
        }

        let agent = event_agent_type(event);

        // Look for session start in another pane within 5 minutes
        for later_event in events.iter().skip(i + 1) {
            if later_event.timestamp - event.timestamp > FAILOVER_WINDOW_MS {
                break;
            }
            if later_event.pane_info.pane_id == event.pane_info.pane_id {
                continue;
            }
            if !is_session_start_event(later_event) {
                continue;
            }

            let later_agent = event_agent_type(later_event);
            if agent.is_some() && later_agent.is_some() && agent != later_agent {
                continue;
            }

            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-failover-{correlation_counter}"),
                event_ids: vec![event.id, later_event.id],
                correlation_type: CorrelationType::Failover,
                confidence: 0.85,
                description: "Usage limit followed by new session (potential failover)".to_string(),
            });
            break;
        }
    }

    correlations
}

fn audit_action_from_backend_cells(row: &[SqlCell]) -> Result<AuditActionRecord> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .optional_i64(5)
        .and_then(|value| {
            value
                .map(|pane_id| backend_i64_to_u64(pane_id, "audit_actions.pane_id"))
                .transpose()
        })
        .map_err(|err| storage_backend_error("Audit action row pane_id", err))?;

    Ok(AuditActionRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Audit action row id", err))?,
        ts: reader
            .i64(1)
            .map_err(|err| storage_backend_error("Audit action row ts", err))?,
        actor_kind: reader
            .string(2)
            .map_err(|err| storage_backend_error("Audit action row actor_kind", err))?,
        actor_id: reader
            .optional_string(3)
            .map_err(|err| storage_backend_error("Audit action row actor_id", err))?,
        correlation_id: reader
            .optional_string(4)
            .map_err(|err| storage_backend_error("Audit action row correlation_id", err))?,
        pane_id,
        domain: reader
            .optional_string(6)
            .map_err(|err| storage_backend_error("Audit action row domain", err))?,
        action_kind: reader
            .string(7)
            .map_err(|err| storage_backend_error("Audit action row action_kind", err))?,
        policy_decision: reader
            .string(8)
            .map_err(|err| storage_backend_error("Audit action row policy_decision", err))?,
        decision_reason: reader
            .optional_string(9)
            .map_err(|err| storage_backend_error("Audit action row decision_reason", err))?,
        rule_id: reader
            .optional_string(10)
            .map_err(|err| storage_backend_error("Audit action row rule_id", err))?,
        input_summary: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Audit action row input_summary", err))?,
        verification_summary: reader
            .optional_string(12)
            .map_err(|err| storage_backend_error("Audit action row verification_summary", err))?,
        decision_context: reader
            .optional_string(13)
            .map_err(|err| storage_backend_error("Audit action row decision_context", err))?,
        result: reader
            .string(14)
            .map_err(|err| storage_backend_error("Audit action row result", err))?,
    })
}

/// Query audit actions with optional filters.
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_audit_actions_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_audit_actions(conn: &Connection, query: &AuditQuery) -> Result<Vec<AuditActionRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result
         FROM audit_actions WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(SqlValue::Text(domain.clone()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(SqlValue::Text(actor_kind.clone()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(SqlValue::Text(actor_id.clone()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(SqlValue::Text(correlation_id.clone()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(SqlValue::Text(action_kind.clone()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(SqlValue::Text(policy_decision.clone()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(SqlValue::Text(result.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY ts DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare audit query: {e}")))?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(AuditActionRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                actor_kind: row.get(2)?,
                actor_id: row.get(3)?,
                correlation_id: row.get(4)?,
                pane_id: {
                    let val: Option<i64> = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(6)?,
                action_kind: row.get(7)?,
                policy_decision: row.get(8)?,
                decision_reason: row.get(9)?,
                rule_id: row.get(10)?,
                input_summary: row.get(11)?,
                verification_summary: row.get(12)?,
                decision_context: row.get(13)?,
                result: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Audit query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_audit_actions_backend(
    backend: &dyn StorageBackend,
    query: &AuditQuery,
) -> Result<Vec<AuditActionRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result
         FROM audit_actions WHERE 1=1",
    );
    let mut params = Vec::new();

    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(ToSqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(ToSqlValue::Text(domain.as_str()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(ToSqlValue::Text(actor_kind.as_str()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(ToSqlValue::Text(actor_id.as_str()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(ToSqlValue::Text(correlation_id.as_str()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(ToSqlValue::Text(action_kind.as_str()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(ToSqlValue::Text(policy_decision.as_str()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(ToSqlValue::Text(rule_id.as_str()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(ToSqlValue::Text(result.as_str()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(ToSqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY ts DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(ToSqlValue::Integer(limit_i64));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query audit actions", err))?;

    rows.iter()
        .map(Vec::as_slice)
        .map(audit_action_from_backend_cells)
        .collect()
}

/// Query audit actions using a cursor for stable streaming.
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_audit_actions_stream_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_audit_actions_stream(
    conn: &Connection,
    query: &AuditStreamQuery,
) -> Result<AuditStreamPage> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result
         FROM audit_actions WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(cursor) = query.cursor {
        sql.push_str(" AND id > ?");
        params.push(SqlValue::Integer(cursor));
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(SqlValue::Text(domain.clone()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(SqlValue::Text(actor_kind.clone()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(SqlValue::Text(actor_id.clone()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(SqlValue::Text(correlation_id.clone()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(SqlValue::Text(action_kind.clone()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(SqlValue::Text(policy_decision.clone()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(SqlValue::Text(result.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY id ASC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    if let Some(offset) = query.offset {
        sql.push_str(" OFFSET ?");
        let offset_i64 = usize_to_i64(offset, "offset")?;
        params.push(SqlValue::Integer(offset_i64));
    }

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        StorageError::Database(format!("Failed to prepare audit stream query: {e}"))
    })?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(AuditActionRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                actor_kind: row.get(2)?,
                actor_id: row.get(3)?,
                correlation_id: row.get(4)?,
                pane_id: {
                    let val: Option<i64> = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(6)?,
                action_kind: row.get(7)?,
                policy_decision: row.get(8)?,
                decision_reason: row.get(9)?,
                rule_id: row.get(10)?,
                input_summary: row.get(11)?,
                verification_summary: row.get(12)?,
                decision_context: row.get(13)?,
                result: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Audit stream query failed: {e}")))?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    let next_cursor = records.last().map(|record| record.id);
    Ok(AuditStreamPage {
        records,
        next_cursor,
    })
}

fn query_audit_actions_stream_backend(
    backend: &dyn StorageBackend,
    query: &AuditStreamQuery,
) -> Result<AuditStreamPage> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result
         FROM audit_actions WHERE 1=1",
    );
    let mut params = Vec::new();

    if let Some(cursor) = query.cursor {
        sql.push_str(" AND id > ?");
        params.push(ToSqlValue::Integer(cursor));
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(ToSqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(ToSqlValue::Text(domain.as_str()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(ToSqlValue::Text(actor_kind.as_str()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(ToSqlValue::Text(actor_id.as_str()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(ToSqlValue::Text(correlation_id.as_str()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(ToSqlValue::Text(action_kind.as_str()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(ToSqlValue::Text(policy_decision.as_str()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(ToSqlValue::Text(rule_id.as_str()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(ToSqlValue::Text(result.as_str()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(ToSqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY id ASC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(ToSqlValue::Integer(limit_i64));

    if let Some(offset) = query.offset {
        sql.push_str(" OFFSET ?");
        let offset_i64 = usize_to_i64(offset, "offset")?;
        params.push(ToSqlValue::Integer(offset_i64));
    }

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query audit action stream", err))?;
    let records = rows
        .iter()
        .map(Vec::as_slice)
        .map(audit_action_from_backend_cells)
        .collect::<Result<Vec<_>>>()?;
    let next_cursor = records.last().map(|record| record.id);

    Ok(AuditStreamPage {
        records,
        next_cursor,
    })
}

fn action_history_from_backend_cells(row: &[SqlCell]) -> Result<ActionHistoryRecord> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .optional_i64(5)
        .and_then(|value| {
            value
                .map(|pane_id| backend_i64_to_u64(pane_id, "action_history.pane_id"))
                .transpose()
        })
        .map_err(|err| storage_backend_error("Action history row pane_id", err))?;
    let undoable = reader
        .optional_i64(15)
        .and_then(|value| {
            value
                .map(|undoable| backend_i64_to_bool(undoable, "action_history.undoable"))
                .transpose()
        })
        .map_err(|err| storage_backend_error("Action history row undoable", err))?;

    Ok(ActionHistoryRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Action history row id", err))?,
        ts: reader
            .i64(1)
            .map_err(|err| storage_backend_error("Action history row ts", err))?,
        actor_kind: reader
            .string(2)
            .map_err(|err| storage_backend_error("Action history row actor_kind", err))?,
        actor_id: reader
            .optional_string(3)
            .map_err(|err| storage_backend_error("Action history row actor_id", err))?,
        correlation_id: reader
            .optional_string(4)
            .map_err(|err| storage_backend_error("Action history row correlation_id", err))?,
        pane_id,
        domain: reader
            .optional_string(6)
            .map_err(|err| storage_backend_error("Action history row domain", err))?,
        action_kind: reader
            .string(7)
            .map_err(|err| storage_backend_error("Action history row action_kind", err))?,
        policy_decision: reader
            .string(8)
            .map_err(|err| storage_backend_error("Action history row policy_decision", err))?,
        decision_reason: reader
            .optional_string(9)
            .map_err(|err| storage_backend_error("Action history row decision_reason", err))?,
        rule_id: reader
            .optional_string(10)
            .map_err(|err| storage_backend_error("Action history row rule_id", err))?,
        input_summary: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Action history row input_summary", err))?,
        verification_summary: reader
            .optional_string(12)
            .map_err(|err| storage_backend_error("Action history row verification_summary", err))?,
        decision_context: reader
            .optional_string(13)
            .map_err(|err| storage_backend_error("Action history row decision_context", err))?,
        result: reader
            .string(14)
            .map_err(|err| storage_backend_error("Action history row result", err))?,
        undoable,
        undo_strategy: reader
            .optional_string(16)
            .map_err(|err| storage_backend_error("Action history row undo_strategy", err))?,
        undo_hint: reader
            .optional_string(17)
            .map_err(|err| storage_backend_error("Action history row undo_hint", err))?,
        undone_at: reader
            .optional_i64(18)
            .map_err(|err| storage_backend_error("Action history row undone_at", err))?,
        undone_by: reader
            .optional_string(19)
            .map_err(|err| storage_backend_error("Action history row undone_by", err))?,
        workflow_id: reader
            .optional_string(20)
            .map_err(|err| storage_backend_error("Action history row workflow_id", err))?,
        step_name: reader
            .optional_string(21)
            .map_err(|err| storage_backend_error("Action history row step_name", err))?,
    })
}

/// Query action history view with optional filters.
///
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_action_history_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_action_history(
    conn: &Connection,
    query: &ActionHistoryQuery,
) -> Result<Vec<ActionHistoryRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result, undoable, undo_strategy, undo_hint, undone_at, undone_by,
         workflow_id, step_name
         FROM action_history WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(audit_action_id) = query.audit_action_id {
        sql.push_str(" AND id = ?");
        params.push(SqlValue::Integer(audit_action_id));
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(SqlValue::Text(domain.clone()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(SqlValue::Text(actor_kind.clone()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(SqlValue::Text(actor_id.clone()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(SqlValue::Text(correlation_id.clone()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(SqlValue::Text(action_kind.clone()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(SqlValue::Text(policy_decision.clone()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(SqlValue::Text(result.clone()));
    }
    if let Some(undoable) = query.undoable {
        if undoable {
            sql.push_str(" AND undoable = 1");
        } else {
            sql.push_str(" AND (undoable = 0 OR undoable IS NULL)");
        }
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY ts DESC, id DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        StorageError::Database(format!("Failed to prepare action history query: {e}"))
    })?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(ActionHistoryRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                actor_kind: row.get(2)?,
                actor_id: row.get(3)?,
                correlation_id: row.get(4)?,
                pane_id: {
                    let val: Option<i64> = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(6)?,
                action_kind: row.get(7)?,
                policy_decision: row.get(8)?,
                decision_reason: row.get(9)?,
                rule_id: row.get(10)?,
                input_summary: row.get(11)?,
                verification_summary: row.get(12)?,
                decision_context: row.get(13)?,
                result: row.get(14)?,
                undoable: {
                    let val: Option<i64> = row.get(15)?;
                    val.map(|v| v != 0)
                },
                undo_strategy: row.get(16)?,
                undo_hint: row.get(17)?,
                undone_at: row.get(18)?,
                undone_by: row.get(19)?,
                workflow_id: row.get(20)?,
                step_name: row.get(21)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Action history query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_action_history_backend(
    backend: &dyn StorageBackend,
    query: &ActionHistoryQuery,
) -> Result<Vec<ActionHistoryRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result, undoable, undo_strategy, undo_hint, undone_at, undone_by,
         workflow_id, step_name
         FROM action_history WHERE 1=1",
    );
    let mut params = Vec::new();

    if let Some(audit_action_id) = query.audit_action_id {
        sql.push_str(" AND id = ?");
        params.push(ToSqlValue::Integer(audit_action_id));
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(ToSqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(ToSqlValue::Text(domain.as_str()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(ToSqlValue::Text(actor_kind.as_str()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(ToSqlValue::Text(actor_id.as_str()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(ToSqlValue::Text(correlation_id.as_str()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(ToSqlValue::Text(action_kind.as_str()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(ToSqlValue::Text(policy_decision.as_str()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(ToSqlValue::Text(rule_id.as_str()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(ToSqlValue::Text(result.as_str()));
    }
    if let Some(undoable) = query.undoable {
        if undoable {
            sql.push_str(" AND undoable = 1");
        } else {
            sql.push_str(" AND (undoable = 0 OR undoable IS NULL)");
        }
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(ToSqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY ts DESC, id DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(ToSqlValue::Integer(limit_i64));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query action history", err))?;

    rows.iter()
        .map(Vec::as_slice)
        .map(action_history_from_backend_cells)
        .collect()
}

/// Query maximum sequence number for a pane.
///
/// `MAX(seq)` returns SQL NULL when no rows match the WHERE clause
/// (empty pane). The string-substrate path can't distinguish NULL
/// from `Integer(0)` cleanly, so this migration uses
/// `query_row_cells` which preserves the storage-class
/// distinction. The original returned `None` for both
/// "no row" and "row with NULL MAX"; we mirror that.
fn query_max_seq_backend(backend: &dyn StorageBackend, pane_id: u64) -> Result<Option<u64>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let row = backend
        .query_row_cells(
            "SELECT MAX(seq) FROM output_segments WHERE pane_id = ?1",
            &[ToSqlValue::Integer(pane_id_i64)],
        )
        .map_err(|err| storage_backend_error("Query max seq", err))?;
    let max = row.and_then(|cells| {
        cells.into_iter().next().and_then(|cell| match cell {
            SqlCell::Integer(v) => Some(v),
            // SQL NULL (empty table or no matching pane_id) maps to None.
            SqlCell::Null => None,
            // MAX over an INTEGER column should never return REAL/TEXT/BLOB;
            // treat unexpected types as "no value".
            _ => None,
        })
    });
    #[allow(clippy::cast_sign_loss)]
    Ok(max.map(|v| v as u64))
}

fn pane_record_from_backend_cells(row: &[SqlCell]) -> Result<PaneRecord> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(0)
        .and_then(|value| backend_i64_to_u64(value, "panes.pane_id"))
        .map_err(|err| storage_backend_error("Pane row pane_id", err))?;
    let window_id = reader
        .optional_i64(3)
        .map_err(|err| storage_backend_error("Pane row window_id", err))?
        .map(|value| backend_i64_to_u64(value, "panes.window_id"))
        .transpose()
        .map_err(|err| storage_backend_error("Pane row window_id", err))?;
    let tab_id = reader
        .optional_i64(4)
        .map_err(|err| storage_backend_error("Pane row tab_id", err))?
        .map(|value| backend_i64_to_u64(value, "panes.tab_id"))
        .transpose()
        .map_err(|err| storage_backend_error("Pane row tab_id", err))?;
    let observed = reader
        .i64(10)
        .and_then(|value| backend_i64_to_bool(value, "panes.observed"))
        .map_err(|err| storage_backend_error("Pane row observed", err))?;

    Ok(PaneRecord {
        pane_id,
        pane_uuid: reader
            .optional_string(1)
            .map_err(|err| storage_backend_error("Pane row pane_uuid", err))?,
        domain: reader
            .string(2)
            .map_err(|err| storage_backend_error("Pane row domain", err))?,
        window_id,
        tab_id,
        title: reader
            .optional_string(5)
            .map_err(|err| storage_backend_error("Pane row title", err))?,
        cwd: reader
            .optional_string(6)
            .map_err(|err| storage_backend_error("Pane row cwd", err))?,
        tty_name: reader
            .optional_string(7)
            .map_err(|err| storage_backend_error("Pane row tty_name", err))?,
        first_seen_at: reader
            .i64(8)
            .map_err(|err| storage_backend_error("Pane row first_seen_at", err))?,
        last_seen_at: reader
            .i64(9)
            .map_err(|err| storage_backend_error("Pane row last_seen_at", err))?,
        observed,
        ignore_reason: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Pane row ignore_reason", err))?,
        last_decision_at: reader
            .optional_i64(12)
            .map_err(|err| storage_backend_error("Pane row last_decision_at", err))?,
    })
}

/// br-ft-l1jgo: trait-typed sibling of [`query_panes`].
fn query_panes_backend(backend: &dyn StorageBackend) -> Result<Vec<PaneRecord>> {
    let rows = backend
        .query_map_cells(
            "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
             first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
             FROM panes
             ORDER BY last_seen_at DESC",
            &[],
        )
        .map_err(|err| storage_backend_error("Query panes", err))?;

    rows.iter()
        .map(|row| pane_record_from_backend_cells(row))
        .collect()
}

/// br-ft-l1jgo: trait-typed sibling of [`query_pane`].
fn query_pane_backend(backend: &dyn StorageBackend, pane_id: u64) -> Result<Option<PaneRecord>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let row = backend
        .query_row_cells(
            "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
             first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
             FROM panes WHERE pane_id = ?1",
            &[ToSqlValue::Integer(pane_id_i64)],
        )
        .map_err(|err| storage_backend_error("Query pane", err))?;
    row.as_deref()
        .map(pane_record_from_backend_cells)
        .transpose()
}

/// Query all panes
///
/// Direct-rusqlite path. Kept as a fallback while
/// [`query_panes_backend`] migration target settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_panes(conn: &Connection) -> Result<Vec<PaneRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
             first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
             FROM panes
             ORDER BY last_seen_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], pane_record_from_row)
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Query a specific pane
///
/// Direct-rusqlite path. Kept for transitional fallback and direct
/// row-shape tests while [`query_pane_backend`] settles in
/// (br-ft-l1jgo).
#[allow(dead_code)]
fn query_pane(conn: &Connection, pane_id: u64) -> Result<Option<PaneRecord>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    conn.query_row(
        "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
         first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
         FROM panes WHERE pane_id = ?1",
        [pane_id_i64],
        pane_record_from_row,
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// br-ft-l1jgo: trait-typed sibling of [`query_segments`].
fn query_segments_backend(
    backend: &dyn StorageBackend,
    pane_id: u64,
    limit: usize,
) -> Result<Vec<Segment>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let limit_i64 = usize_to_i64(limit, "limit")?;
    let rows = backend
        .query_map_cells(
            "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1
             ORDER BY seq DESC
             LIMIT ?2",
            &[
                ToSqlValue::Integer(pane_id_i64),
                ToSqlValue::Integer(limit_i64),
            ],
        )
        .map_err(|err| storage_backend_error("Query segments", err))?;

    rows.iter()
        .map(|row| segment_from_backend_cells(row))
        .collect()
}

/// Query segments for a pane
///
/// Direct-rusqlite path. Kept for transitional fallback and direct
/// row-shape tests while [`query_segments_backend`] settles in
/// (br-ft-l1jgo).
#[allow(dead_code)]
#[allow(clippy::cast_sign_loss)]
fn query_segments(conn: &Connection, pane_id: u64, limit: usize) -> Result<Vec<Segment>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let limit_i64 = usize_to_i64(limit, "limit")?;

    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1
             ORDER BY seq DESC
             LIMIT ?2",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([pane_id_i64, limit_i64], |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_segments_from_mmap(
    base_dir: &Path,
    pane_id: u64,
    limit: usize,
) -> Result<Option<Vec<Segment>>> {
    if limit == 0 {
        return Ok(Some(Vec::new()));
    }

    let config = mmap_store::MmapStoreConfig::new(base_dir.to_path_buf());
    let mut store = mmap_store::MmapScrollbackStore::new(config).map_err(|error| {
        StorageError::Database(format!("Failed to open mmap segment mirror store: {error}"))
    })?;

    match store.ensure_pane(pane_id) {
        Ok(()) => {}
        Err(mmap_store::MmapStoreError::UnknownPane(_)) => return Ok(None),
        Err(error) => {
            return Err(StorageError::Database(format!(
                "Failed to prepare mmap pane {pane_id} for read: {error}"
            ))
            .into());
        }
    }

    let lines = match store.tail_lines(pane_id, limit) {
        Ok(lines) => lines,
        Err(mmap_store::MmapStoreError::UnknownPane(_)) => return Ok(None),
        Err(error) => {
            return Err(StorageError::Database(format!(
                "Failed to read mmap pane {pane_id} lines: {error}"
            ))
            .into());
        }
    };

    let mut segments = Vec::with_capacity(lines.len());
    for raw_line in lines {
        let segment = decode_mmap_segment_line(&raw_line)?;
        if segment.pane_id != pane_id {
            return Err(StorageError::Database(format!(
                "Mmap segment pane mismatch: expected {pane_id}, found {}",
                segment.pane_id
            ))
            .into());
        }
        segments.push(segment);
    }

    // tail_lines() returns oldest->newest within the requested window;
    // query_segments() returns newest->oldest, so align ordering.
    segments.reverse();
    Ok(Some(segments))
}

fn query_segment_by_id_backend(
    backend: &dyn StorageBackend,
    segment_id: i64,
) -> Result<Option<Segment>> {
    let row = backend
        .query_row_cells(
            "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE id = ?1",
            &[ToSqlValue::Integer(segment_id)],
        )
        .map_err(|err| storage_backend_error("query_segment_by_id", err))?;

    row.map(|cells| segment_from_backend_cells(&cells))
        .transpose()
}

#[allow(dead_code)]
#[allow(clippy::cast_sign_loss)]
fn query_segment_by_id(conn: &Connection, segment_id: i64) -> Result<Option<Segment>> {
    conn.query_row(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments
         WHERE id = ?1",
        [segment_id],
        |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("query_segment_by_id failed: {e}")).into())
}

/// Query workflow by ID
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_workflow_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
#[allow(clippy::cast_sign_loss)]
fn query_workflow(conn: &Connection, workflow_id: &str) -> Result<Option<WorkflowRecord>> {
    conn.query_row(
        "SELECT id, workflow_name, pane_id, trigger_event_id, current_step, status,
         wait_condition, context, result, error, started_at, updated_at, completed_at
         FROM workflow_executions WHERE id = ?1",
        [workflow_id],
        |row| {
            // br-ft-lqj5g: route through the parse-drop helper so a
            // schema-skewed row column bumps the observability
            // counter instead of silently turning into None.
            let wait_condition_str: Option<String> = row.get(6)?;
            let wait_condition =
                parse_workflow_execution_column(wait_condition_str.as_deref(), "wait_condition");

            let context_str: Option<String> = row.get(7)?;
            let context = parse_workflow_execution_column(context_str.as_deref(), "context");

            let result_str: Option<String> = row.get(8)?;
            let result = parse_workflow_execution_column(result_str.as_deref(), "result");

            Ok(WorkflowRecord {
                id: row.get(0)?,
                workflow_name: row.get(1)?,
                pane_id: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                trigger_event_id: row.get(3)?,
                current_step: {
                    let val: i64 = row.get(4)?;
                    usize::try_from(val).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            4,
                            "current_step".to_string(),
                            rusqlite::types::Type::Integer,
                        )
                    })?
                },
                status: row.get(5)?,
                wait_condition,
                context,
                result,
                error: row.get(9)?,
                started_at: row.get(10)?,
                updated_at: row.get(11)?,
                completed_at: row.get(12)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

fn workflow_record_from_backend_cells(row: &[SqlCell]) -> Result<WorkflowRecord> {
    let reader = CellRowReader::new(row);
    let pane_id = reader
        .i64(2)
        .and_then(|value| backend_i64_to_u64(value, "workflow_executions.pane_id"))
        .map_err(|err| storage_backend_error("Workflow row pane_id", err))?;
    let current_step = reader
        .i64(4)
        .map_err(|err| storage_backend_error("Workflow row current_step", err))?;
    let wait_condition_str = reader
        .optional_string(6)
        .map_err(|err| storage_backend_error("Workflow row wait_condition", err))?;
    let context_str = reader
        .optional_string(7)
        .map_err(|err| storage_backend_error("Workflow row context", err))?;
    let result_str = reader
        .optional_string(8)
        .map_err(|err| storage_backend_error("Workflow row result", err))?;

    Ok(WorkflowRecord {
        id: reader
            .string(0)
            .map_err(|err| storage_backend_error("Workflow row id", err))?,
        workflow_name: reader
            .string(1)
            .map_err(|err| storage_backend_error("Workflow row workflow_name", err))?,
        pane_id,
        trigger_event_id: reader
            .optional_i64(3)
            .map_err(|err| storage_backend_error("Workflow row trigger_event_id", err))?,
        current_step: i64_to_usize(current_step).map_err(|err| {
            StorageError::Database(format!("Workflow row current_step decode failed: {err}"))
        })?,
        status: reader
            .string(5)
            .map_err(|err| storage_backend_error("Workflow row status", err))?,
        wait_condition: parse_workflow_execution_column(
            wait_condition_str.as_deref(),
            "wait_condition",
        ),
        context: parse_workflow_execution_column(context_str.as_deref(), "context"),
        result: parse_workflow_execution_column(result_str.as_deref(), "result"),
        error: reader
            .optional_string(9)
            .map_err(|err| storage_backend_error("Workflow row error", err))?,
        started_at: reader
            .i64(10)
            .map_err(|err| storage_backend_error("Workflow row started_at", err))?,
        updated_at: reader
            .i64(11)
            .map_err(|err| storage_backend_error("Workflow row updated_at", err))?,
        completed_at: reader
            .optional_i64(12)
            .map_err(|err| storage_backend_error("Workflow row completed_at", err))?,
    })
}

fn query_workflow_backend(
    backend: &dyn StorageBackend,
    workflow_id: &str,
) -> Result<Option<WorkflowRecord>> {
    let row = backend
        .query_row_cells(
            "SELECT id, workflow_name, pane_id, trigger_event_id, current_step, status,
             wait_condition, context, result, error, started_at, updated_at, completed_at
             FROM workflow_executions WHERE id = ?1",
            &[ToSqlValue::Text(workflow_id)],
        )
        .map_err(|err| storage_backend_error("Query workflow", err))?;

    row.as_deref()
        .map(workflow_record_from_backend_cells)
        .transpose()
}

/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_action_plan_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_action_plan(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Option<WorkflowActionPlanRecord>> {
    conn.query_row(
        "SELECT workflow_id, plan_id, plan_hash, plan_json, created_at \
         FROM workflow_action_plans WHERE workflow_id = ?1",
        [workflow_id],
        |row| {
            Ok(WorkflowActionPlanRecord {
                workflow_id: row.get(0)?,
                plan_id: row.get(1)?,
                plan_hash: row.get(2)?,
                plan_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

fn workflow_action_plan_from_backend_cells(row: &[SqlCell]) -> Result<WorkflowActionPlanRecord> {
    let reader = CellRowReader::new(row);

    Ok(WorkflowActionPlanRecord {
        workflow_id: reader
            .string(0)
            .map_err(|err| storage_backend_error("Workflow action plan workflow_id", err))?,
        plan_id: reader
            .string(1)
            .map_err(|err| storage_backend_error("Workflow action plan plan_id", err))?,
        plan_hash: reader
            .string(2)
            .map_err(|err| storage_backend_error("Workflow action plan plan_hash", err))?,
        plan_json: reader
            .string(3)
            .map_err(|err| storage_backend_error("Workflow action plan plan_json", err))?,
        created_at: reader
            .i64(4)
            .map_err(|err| storage_backend_error("Workflow action plan created_at", err))?,
    })
}

fn query_action_plan_backend(
    backend: &dyn StorageBackend,
    workflow_id: &str,
) -> Result<Option<WorkflowActionPlanRecord>> {
    let row = backend
        .query_row_cells(
            "SELECT workflow_id, plan_id, plan_hash, plan_json, created_at \
             FROM workflow_action_plans WHERE workflow_id = ?1",
            &[ToSqlValue::Text(workflow_id)],
        )
        .map_err(|err| storage_backend_error("Query action plan", err))?;

    row.as_deref()
        .map(workflow_action_plan_from_backend_cells)
        .transpose()
}

#[cfg(test)]
fn query_prepared_plan(conn: &Connection, plan_id: &str) -> Result<Option<PreparedPlanRecord>> {
    conn.query_row(
        "SELECT plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
                plan_json, requires_approval, created_at, expires_at, consumed_at
         FROM prepared_plans
         WHERE plan_id = ?1",
        [plan_id],
        prepared_plan_from_row,
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_step_logs_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_step_logs(conn: &Connection, workflow_id: &str) -> Result<Vec<WorkflowStepLogRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind,
             result_type, result_data, policy_summary, verification_refs, error_code,
             started_at, completed_at, duration_ms
             FROM workflow_step_logs
             WHERE workflow_id = ?1
             ORDER BY step_index ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([workflow_id], |row| {
            Ok(WorkflowStepLogRecord {
                id: row.get(0)?,
                workflow_id: row.get(1)?,
                audit_action_id: row.get(2)?,
                step_index: {
                    let val: i64 = row.get(3)?;
                    i64_to_usize(val)?
                },
                step_name: row.get(4)?,
                step_id: row.get(5)?,
                step_kind: row.get(6)?,
                result_type: row.get(7)?,
                result_data: row.get(8)?,
                policy_summary: row.get(9)?,
                verification_refs: row.get(10)?,
                error_code: row.get(11)?,
                started_at: row.get(12)?,
                completed_at: row.get(13)?,
                duration_ms: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn workflow_step_log_from_backend_cells(row: &[SqlCell]) -> Result<WorkflowStepLogRecord> {
    let reader = CellRowReader::new(row);
    let step_index = reader
        .i64(3)
        .map_err(|err| storage_backend_error("Workflow step log step_index", err))?;

    Ok(WorkflowStepLogRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Workflow step log id", err))?,
        workflow_id: reader
            .string(1)
            .map_err(|err| storage_backend_error("Workflow step log workflow_id", err))?,
        audit_action_id: reader
            .optional_i64(2)
            .map_err(|err| storage_backend_error("Workflow step log audit_action_id", err))?,
        step_index: i64_to_usize(step_index).map_err(|err| {
            StorageError::Database(format!("Workflow step log step_index decode failed: {err}"))
        })?,
        step_name: reader
            .string(4)
            .map_err(|err| storage_backend_error("Workflow step log step_name", err))?,
        step_id: reader
            .optional_string(5)
            .map_err(|err| storage_backend_error("Workflow step log step_id", err))?,
        step_kind: reader
            .optional_string(6)
            .map_err(|err| storage_backend_error("Workflow step log step_kind", err))?,
        result_type: reader
            .string(7)
            .map_err(|err| storage_backend_error("Workflow step log result_type", err))?,
        result_data: reader
            .optional_string(8)
            .map_err(|err| storage_backend_error("Workflow step log result_data", err))?,
        policy_summary: reader
            .optional_string(9)
            .map_err(|err| storage_backend_error("Workflow step log policy_summary", err))?,
        verification_refs: reader
            .optional_string(10)
            .map_err(|err| storage_backend_error("Workflow step log verification_refs", err))?,
        error_code: reader
            .optional_string(11)
            .map_err(|err| storage_backend_error("Workflow step log error_code", err))?,
        started_at: reader
            .i64(12)
            .map_err(|err| storage_backend_error("Workflow step log started_at", err))?,
        completed_at: reader
            .i64(13)
            .map_err(|err| storage_backend_error("Workflow step log completed_at", err))?,
        duration_ms: reader
            .i64(14)
            .map_err(|err| storage_backend_error("Workflow step log duration_ms", err))?,
    })
}

fn query_step_logs_backend(
    backend: &dyn StorageBackend,
    workflow_id: &str,
) -> Result<Vec<WorkflowStepLogRecord>> {
    let rows = backend
        .query_map_cells(
            "SELECT id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind,
             result_type, result_data, policy_summary, verification_refs, error_code,
             started_at, completed_at, duration_ms
             FROM workflow_step_logs
             WHERE workflow_id = ?1
             ORDER BY step_index ASC",
            &[ToSqlValue::Text(workflow_id)],
        )
        .map_err(|err| storage_backend_error("Query step logs", err))?;

    rows.iter()
        .map(|row| workflow_step_log_from_backend_cells(row))
        .collect()
}

fn query_latest_step_log_backend(
    backend: &dyn StorageBackend,
    workflow_id: &str,
) -> Result<Option<WorkflowStepLogRecord>> {
    let row = backend
        .query_row_cells(
            "SELECT id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind,
             result_type, result_data, policy_summary, verification_refs, error_code,
             started_at, completed_at, duration_ms
         FROM workflow_step_logs
         WHERE workflow_id = ?1
         ORDER BY step_index DESC
         LIMIT 1",
            &[ToSqlValue::Text(workflow_id)],
        )
        .map_err(|err| storage_backend_error("Query latest step log", err))?;

    row.as_deref()
        .map(workflow_step_log_from_backend_cells)
        .transpose()
}

/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_latest_step_log_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_latest_step_log(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Option<WorkflowStepLogRecord>> {
    conn.query_row(
        "SELECT id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind,
             result_type, result_data, policy_summary, verification_refs, error_code,
             started_at, completed_at, duration_ms
         FROM workflow_step_logs
         WHERE workflow_id = ?1
         ORDER BY step_index DESC
         LIMIT 1",
        [workflow_id],
        |row| {
            Ok(WorkflowStepLogRecord {
                id: row.get(0)?,
                workflow_id: row.get(1)?,
                audit_action_id: row.get(2)?,
                step_index: {
                    let val: i64 = row.get(3)?;
                    i64_to_usize(val)?
                },
                step_name: row.get(4)?,
                step_id: row.get(5)?,
                step_kind: row.get(6)?,
                result_type: row.get(7)?,
                result_data: row.get(8)?,
                policy_summary: row.get(9)?,
                verification_refs: row.get(10)?,
                error_code: row.get(11)?,
                started_at: row.get(12)?,
                completed_at: row.get(13)?,
                duration_ms: row.get(14)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// Query incomplete workflows for resume on restart
/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_incomplete_workflows_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
#[allow(clippy::cast_sign_loss)]
fn query_incomplete_workflows(conn: &Connection) -> Result<Vec<WorkflowRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, workflow_name, pane_id, trigger_event_id, current_step, status,
             wait_condition, context, result, error, started_at, updated_at, completed_at
             FROM workflow_executions
             WHERE status IN ('running', 'waiting')
             ORDER BY started_at ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            // br-ft-lqj5g: route through the parse-drop helper so a
            // schema-skewed row column bumps the observability
            // counter instead of silently turning into None.
            let wait_condition_str: Option<String> = row.get(6)?;
            let wait_condition =
                parse_workflow_execution_column(wait_condition_str.as_deref(), "wait_condition");

            let context_str: Option<String> = row.get(7)?;
            let context = parse_workflow_execution_column(context_str.as_deref(), "context");

            let result_str: Option<String> = row.get(8)?;
            let result = parse_workflow_execution_column(result_str.as_deref(), "result");

            Ok(WorkflowRecord {
                id: row.get(0)?,
                workflow_name: row.get(1)?,
                pane_id: {
                    let val: i64 = row.get(2)?;
                    val as u64
                },
                trigger_event_id: row.get(3)?,
                current_step: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                status: row.get(5)?,
                wait_condition,
                context,
                result,
                error: row.get(9)?,
                started_at: row.get(10)?,
                updated_at: row.get(11)?,
                completed_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_incomplete_workflows_backend(backend: &dyn StorageBackend) -> Result<Vec<WorkflowRecord>> {
    let rows = backend
        .query_map_cells(
            "SELECT id, workflow_name, pane_id, trigger_event_id, current_step, status,
             wait_condition, context, result, error, started_at, updated_at, completed_at
             FROM workflow_executions
             WHERE status IN ('running', 'waiting')
             ORDER BY started_at ASC",
            &[],
        )
        .map_err(|err| storage_backend_error("Query incomplete workflows", err))?;

    rows.iter()
        .map(|row| workflow_record_from_backend_cells(row))
        .collect()
}

// =============================================================================
// Segment Scan Query Functions
// =============================================================================

/// Build a dynamic WHERE clause and params from a SegmentScanQuery.
/// `time_column` is the column name used for since/until filtering.
fn build_segment_scan_where(
    query: &SegmentScanQuery,
    time_column: &str,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(after_id) = query.after_id {
        clauses.push(format!("id > ?{}", params.len() + 1));
        params.push(Box::new(after_id));
    }
    if let Some(pane_id) = query.pane_id {
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(Box::new(u64_to_i64_unchecked(pane_id)));
    }
    if let Some(since) = query.since {
        clauses.push(format!("{time_column} >= ?{}", params.len() + 1));
        params.push(Box::new(since));
    }
    if let Some(until) = query.until {
        clauses.push(format!("{time_column} <= ?{}", params.len() + 1));
        params.push(Box::new(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    (where_clause, params)
}

fn build_segment_scan_backend_where(
    query: &SegmentScanQuery,
    time_column: &str,
) -> Result<(String, Vec<ToSqlValue<'static>>)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<ToSqlValue<'static>> = Vec::new();

    if let Some(after_id) = query.after_id {
        clauses.push(format!("id > ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(after_id));
    }
    if let Some(pane_id) = query.pane_id {
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(u64_to_i64(pane_id, "pane_id")?));
    }
    if let Some(since) = query.since {
        clauses.push(format!("{time_column} >= ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        clauses.push(format!("{time_column} <= ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    Ok((where_clause, params))
}

fn query_scan_segments_backend(
    backend: &dyn StorageBackend,
    query: &SegmentScanQuery,
) -> Result<Vec<Segment>> {
    let (where_clause, mut params) = build_segment_scan_backend_where(query, "captured_at")?;
    let limit = if query.limit == 0 { 1_000 } else { query.limit };
    let limit_i64 = usize_to_i64(limit, "limit")?;
    let sql = format!(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments{where_clause}
         ORDER BY id ASC
         LIMIT ?{}",
        params.len() + 1
    );
    params.push(ToSqlValue::Integer(limit_i64));

    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query scan segments", err))?;

    rows.iter()
        .map(|row| segment_from_backend_cells(row))
        .collect()
}

/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_scan_segments_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_scan_segments(conn: &Connection, query: &SegmentScanQuery) -> Result<Vec<Segment>> {
    let (where_clause, params) = build_segment_scan_where(query, "captured_at");
    let limit = if query.limit == 0 { 1_000 } else { query.limit };
    let sql = format!(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments{where_clause}
         ORDER BY id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as usize
                    }
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn secret_scan_report_from_backend_cells(row: &[SqlCell]) -> Result<SecretScanReportRecord> {
    let reader = CellRowReader::new(row);
    Ok(SecretScanReportRecord {
        id: reader
            .i64(0)
            .map_err(|err| storage_backend_error("Secret scan report id", err))?,
        scope_hash: reader
            .string(1)
            .map_err(|err| storage_backend_error("Secret scan report scope_hash", err))?,
        scope_json: reader
            .string(2)
            .map_err(|err| storage_backend_error("Secret scan report scope_json", err))?,
        report_version: reader
            .i64(3)
            .map_err(|err| storage_backend_error("Secret scan report version", err))?,
        last_segment_id: reader
            .optional_i64(4)
            .map_err(|err| storage_backend_error("Secret scan report last_segment_id", err))?,
        report_json: reader
            .string(5)
            .map_err(|err| storage_backend_error("Secret scan report report_json", err))?,
        created_at: reader
            .i64(6)
            .map_err(|err| storage_backend_error("Secret scan report created_at", err))?,
    })
}

fn query_latest_secret_scan_report_backend(
    backend: &dyn StorageBackend,
    scope_hash: &str,
) -> Result<Option<SecretScanReportRecord>> {
    let row = backend
        .query_row_cells(
            "SELECT id, scope_hash, scope_json, report_version, last_segment_id, \
             report_json, created_at
             FROM secret_scan_reports
             WHERE scope_hash = ?1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            &[ToSqlValue::Text(scope_hash)],
        )
        .map_err(|err| storage_backend_error("Query latest secret scan report", err))?;

    row.as_deref()
        .map(secret_scan_report_from_backend_cells)
        .transpose()
}

/// Direct-rusqlite path. Kept for transitional fallback while
/// [`query_latest_secret_scan_report_backend`] settles in (br-ft-l1jgo).
#[allow(dead_code)]
fn query_latest_secret_scan_report(
    conn: &Connection,
    scope_hash: &str,
) -> Result<Option<SecretScanReportRecord>> {
    conn.query_row(
        "SELECT id, scope_hash, scope_json, report_version, last_segment_id, \
         report_json, created_at
         FROM secret_scan_reports
         WHERE scope_hash = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        params![scope_hash],
        |row| {
            Ok(SecretScanReportRecord {
                id: row.get(0)?,
                scope_hash: row.get(1)?,
                scope_json: row.get(2)?,
                report_version: row.get(3)?,
                last_segment_id: row.get(4)?,
                report_json: row.get(5)?,
                created_at: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

// =============================================================================
// Export Query Functions  →  moved to `storage/export.rs`
// =============================================================================
//
// [ft-nsb8c / ft-dn2tu Phase 5] `build_export_where` and the five
// `query_export_*` helpers now live in `storage/export.rs`. Call
// sites in `StorageHandle::export_*` use the `export::` qualifier;
// they are not re-exported publicly because the production surface
// is the async `StorageHandle` methods, not the raw query fns.
// `u64_to_i64_unchecked` stays here — it is also used by the audit
// and stream-page where-clause builders in `storage.rs`.

/// Unchecked u64→i64 cast for query params (SQLite stores as i64).
fn u64_to_i64_unchecked(val: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        val as i64
    }
}

#[cfg(test)]
mod policy_decision_tests;

// =========================================================================
// wa-4vx.3.4: FTS Search API Tests
// =========================================================================

#[cfg(test)]
#[rustfmt::skip]
mod fts_async_flat_tests {
    use super::*;

#[test]
fn fts_search_returns_matching_segments() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert segments with different content
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "error: connection refused", 26, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "successfully connected to server", 32, now_ms + 100],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 2i64, "another error occurred here", 27, now_ms + 200],
        ).unwrap();

    // Search for "error"
    let results = search_fts_with_snippets(&conn, "error", &SearchOptions::default()).unwrap();

    assert_eq!(results.len(), 2, "Should find 2 segments with 'error'");
    assert!(results[0].segment.content.contains("error"));
    assert!(results[1].segment.content.contains("error"));
}

#[test]
fn fts_backend_search_matches_direct_results() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "backend needle one", 18, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "backend needle two", 18, now_ms + 1],
        ).unwrap();

    let options = SearchOptions::default();
    let direct = search_fts_with_snippets(&conn, "needle", &options).unwrap();
    let backend = RusqliteBackend::new(conn);
    let via_backend = search_fts_with_snippets_backend(&backend, "needle", &options).unwrap();

    assert_eq!(via_backend.len(), direct.len());
    for (backend_result, direct_result) in via_backend.iter().zip(direct.iter()) {
        assert_eq!(backend_result.segment.id, direct_result.segment.id);
        assert_eq!(backend_result.segment.pane_id, direct_result.segment.pane_id);
        assert_eq!(backend_result.segment.seq, direct_result.segment.seq);
        assert_eq!(backend_result.segment.content, direct_result.segment.content);
        assert_eq!(backend_result.snippet, direct_result.snippet);
        assert_eq!(backend_result.highlight, direct_result.highlight);
        assert!(
            (backend_result.score - direct_result.score).abs() <= f64::EPSILON,
            "backend score {} must match direct score {}",
            backend_result.score,
            direct_result.score
        );
    }
}

#[test]
fn hybrid_backend_search_matches_direct_results() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    let content_a = "hybrid needle alpha";
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, content_a, i64::try_from(content_a.len()).unwrap(), now_ms],
        ).unwrap();
    let seg_a = conn.last_insert_rowid();

    let content_b = "hybrid needle beta";
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, content_b, i64::try_from(content_b.len()).unwrap(), now_ms + 1],
        ).unwrap();
    let seg_b = conn.last_insert_rowid();

    let query_vector = [1.0_f32, 0.0];
    let vector_a = encode_f32_embedding_blob(&[1.0_f32, 0.0]).unwrap();
    let vector_b = encode_f32_embedding_blob(&[0.0_f32, 1.0]).unwrap();
    conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seg_a, "hybrid-embedder", 2i64, vector_a, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seg_b, "hybrid-embedder", 2i64, vector_b, now_ms + 1],
        ).unwrap();

    let options = SearchOptions {
        limit: Some(2),
        ..SearchOptions::default()
    };
    let direct_state = Arc::new(Mutex::new(SemanticBudgetState::new(
        SemanticBudgetConfig::default(),
    )));
    let direct = hybrid_search_with_results_sync(
        &conn,
        "needle",
        &options,
        "hybrid-embedder",
        &query_vector,
        SearchMode::Hybrid,
        60,
        0.5,
        0.5,
        Some(FusionBackend::FrankenSearchRrf),
        &direct_state,
    ).unwrap();

    let backend = RusqliteBackend::new(conn);
    let backend_state = Arc::new(Mutex::new(SemanticBudgetState::new(
        SemanticBudgetConfig::default(),
    )));
    let via_backend = hybrid_search_with_results_backend(
        &backend,
        "needle",
        &options,
        "hybrid-embedder",
        &query_vector,
        SearchMode::Hybrid,
        60,
        0.5,
        0.5,
        Some(FusionBackend::FrankenSearchRrf),
        &backend_state,
    ).unwrap();

    assert_eq!(via_backend.mode, direct.mode);
    assert_eq!(via_backend.requested_mode, direct.requested_mode);
    assert_eq!(via_backend.fallback_reason, direct.fallback_reason);
    assert_eq!(via_backend.lexical_candidates, direct.lexical_candidates);
    assert_eq!(via_backend.semantic_candidates, direct.semantic_candidates);
    assert_eq!(via_backend.semantic_rows_scanned, direct.semantic_rows_scanned);
    assert_eq!(via_backend.results.len(), direct.results.len());
    for (backend_result, direct_result) in via_backend.results.iter().zip(direct.results.iter()) {
        assert_eq!(backend_result.result.segment.id, direct_result.result.segment.id);
        assert_eq!(backend_result.result.segment.content, direct_result.result.segment.content);
        assert_eq!(backend_result.lexical_rank, direct_result.lexical_rank);
        assert_eq!(backend_result.semantic_rank, direct_result.semantic_rank);
        assert_eq!(backend_result.fusion_rank, direct_result.fusion_rank);
        assert!(
            (backend_result.fusion_score - direct_result.fusion_score).abs() <= f64::EPSILON,
            "backend fusion score {} must match direct score {}",
            backend_result.fusion_score,
            direct_result.fusion_score
        );
    }
}

#[test]
fn fts_search_returns_snippets_with_highlights() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "The important error message appears here", 40, now_ms],
        ).unwrap();

    let options = SearchOptions {
        highlight_prefix: Some("[[".to_string()),
        highlight_suffix: Some("]]".to_string()),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "error", &options).unwrap();

    assert_eq!(results.len(), 1);
    let snippet = results[0].snippet.as_ref().expect("Should have snippet");
    assert!(
        snippet.contains("[[error]]"),
        "Snippet should contain highlighted term: {snippet}"
    );
}

#[test]
fn fts_search_respects_pane_filter() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![2i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "pane one test message", 21, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![2i64, 0i64, "pane two test message", 21, now_ms],
        ).unwrap();

    let options = SearchOptions {
        pane_id: Some(1),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].segment.pane_id, 1);
}

#[test]
fn fts_search_respects_time_filter() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms + 2000, 1],
        ).unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "early test message", 18, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "middle test message", 19, now_ms + 1000],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 2i64, "late test message", 17, now_ms + 2000],
        ).unwrap();

    let options = SearchOptions {
        since: Some(now_ms + 500),
        until: Some(now_ms + 1500),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 1);
    assert!(results[0].segment.content.contains("middle"));
}

#[test]
fn fts_search_invalid_query_returns_error() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let result = validate_fts_query(&conn, "\"unclosed quote");
    assert!(result.is_err());
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("Invalid FTS5 query syntax"),
        "Error should mention FTS5 syntax: {err_msg}"
    );
}

/// [ft-76d9i] A query with zero matches must NOT be reported as an
/// invalid syntax error. The pre-fix `COUNT(*) ... LIMIT 1` shape
/// always returned exactly one row (the count = 0), so empty match
/// sets fell through the Ok branch implicitly. The fix uses
/// `SELECT 1 ... LIMIT 1`, which returns `QueryReturnedNoRows` for
/// empty match sets — pin that we promote that to `Ok(())`.
#[test]
fn ft_76d9i_validate_fts_query_accepts_empty_match_set() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    // Empty FTS index. Any well-formed query must validate successfully
    // because the syntax is fine — there are simply no rows to match.
    let result = validate_fts_query(&conn, "absolutely_nothing_matches_this_token");
    assert!(
        result.is_ok(),
        "well-formed query against empty FTS index must validate: {result:?}"
    );
}

/// [ft-76d9i] A query that DOES match must also validate. The fix
/// switches from `COUNT(*) ... LIMIT 1` (which scanned every match
/// before returning the aggregate row, doubling the read cost when
/// `search_fts_with_snippets` ran the same MATCH again) to
/// `SELECT 1 ... LIMIT 1` (which short-circuits on the first
/// matching rowid). Both shapes must report Ok for a valid +
/// matching query — pin that to prevent a future refactor from
/// regressing the happy path.
#[test]
fn ft_76d9i_validate_fts_query_accepts_well_formed_matching_query() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    let now_ms = 1_700_000_000_000i64;
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();
    for i in 0i64..5 {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, i, "needle in many haystacks", 24, now_ms + i],
        )
        .unwrap();
    }
    assert!(validate_fts_query(&conn, "needle").is_ok());
    assert!(validate_fts_query(&conn, "haystacks").is_ok());
    // FTS5 prefix and operators still work after the shape change.
    assert!(validate_fts_query(&conn, "need*").is_ok());
    assert!(validate_fts_query(&conn, "needle AND haystacks").is_ok());
}

#[test]
fn fts_search_respects_limit() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    for i in 0i64..10 {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                1i64,
                i,
                format!("test message number {i}"),
                20,
                now_ms + i * 100
            ],
        )
        .unwrap();
    }

    let options = SearchOptions {
        limit: Some(3),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 3, "Should respect limit of 3");
}

#[test]
fn fts_search_bm25_ordering() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "single error here", 17, now_ms],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 1i64, "error error error multiple errors", 33, now_ms + 100],
    )
    .unwrap();

    let results = search_fts_with_snippets(&conn, "error", &SearchOptions::default()).unwrap();

    assert_eq!(results.len(), 2);
    assert!(
        results[0].score <= results[1].score,
        "First result should have lower (better) BM25 score"
    );
}

#[test]
fn fts_search_no_snippets_option() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "test content here", 17, now_ms],
    )
    .unwrap();

    let options = SearchOptions {
        include_snippets: Some(false),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 1);
    assert!(
        results[0].snippet.is_none(),
        "Snippet should be None when disabled"
    );
}

/// ft-okhhj: opting out of `highlight()` while keeping `snippet()` must
/// return the snippet column populated and the highlight column NULL. The
/// two-stage hydrate query toggles the highlight column at SQL build time
/// so the FTS5 `highlight()` function is not invoked at all when callers
/// set `include_highlights = Some(false)`.
#[test]
fn fts_search_skips_highlight_when_disabled_but_keeps_snippet() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            1i64,
            0i64,
            "an interesting needle hides in this haystack of text",
            52,
            now_ms,
        ],
    )
    .unwrap();

    let options = SearchOptions {
        include_snippets: Some(true),
        include_highlights: Some(false),
        highlight_prefix: Some("[[".to_string()),
        highlight_suffix: Some("]]".to_string()),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "needle", &options).unwrap();

    assert_eq!(results.len(), 1);
    let snippet = results[0]
        .snippet
        .as_ref()
        .expect("snippet must be populated when include_snippets=true");
    assert!(
        snippet.contains("[[needle]]"),
        "snippet must still carry the highlight markers; got {snippet}"
    );
    assert!(
        results[0].highlight.is_none(),
        "highlight column must be NULL when include_highlights=Some(false); got {:?}",
        results[0].highlight
    );
}

/// ft-okhhj: the two-stage path must preserve the deterministic ordering
/// (BM25 score ASC, captured_at ASC, id ASC) of the legacy single-stage
/// path. Build a corpus with a clear BM25 winner plus two ties on score,
/// and assert the rank-then-hydrate path returns rows in the documented
/// order.
#[test]
fn fts_search_two_stage_preserves_ordering() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    // Two segments share the same content (same BM25 score), inserted at
    // different timestamps; tie-break must order by captured_at ASC.
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "needle word", 11, now_ms + 200],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 1i64, "needle word", 11, now_ms + 100],
    )
    .unwrap();
    // High-density "needle" segment — BM25 should rank this first
    // (more occurrences in shorter content yields a more-negative score).
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 2i64, "needle needle needle", 20, now_ms + 50],
    )
    .unwrap();

    let results = search_fts_with_snippets(&conn, "needle", &SearchOptions::default()).unwrap();
    assert_eq!(results.len(), 3);

    // Highest-density "needle needle needle" must come first.
    assert_eq!(results[0].segment.content, "needle needle needle");
    // The two tied "needle word" rows must be ordered by captured_at ASC.
    assert_eq!(results[1].segment.captured_at, now_ms + 100);
    assert_eq!(results[2].segment.captured_at, now_ms + 200);

    // Score ordering invariant: scores must be monotonically non-decreasing.
    for window in results.windows(2) {
        assert!(
            window[0].score <= window[1].score,
            "scores must be ascending: {} > {}",
            window[0].score,
            window[1].score
        );
    }
}

// =========================================================================
// wa-4vx.3.7: FTS Empty/No-Match Behavior Tests
// =========================================================================

#[test]
fn fts_search_no_match_returns_empty() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "hello world", 11, now_ms],
    )
    .unwrap();

    // Search for term that doesn't exist
    let results =
        search_fts_with_snippets(&conn, "nonexistent", &SearchOptions::default()).unwrap();

    assert!(results.is_empty(), "Should return empty vec for no matches");
}

#[test]
fn fts_search_empty_db_returns_empty() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Search on empty database (no panes, no segments)
    let results = search_fts_with_snippets(&conn, "anything", &SearchOptions::default()).unwrap();

    assert!(
        results.is_empty(),
        "Should return empty vec for empty database"
    );
}

// =========================================================================
// wa-4vx.3.7: Workflow Step Logs Tests
// =========================================================================

#[test]
fn can_insert_and_query_workflow_step_logs() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    // Insert workflow execution
    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-test-001", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    // Insert step logs through the StorageBackend trait path.
    let backend = RusqliteBackend::new(conn);
    insert_step_log_backend(
        &backend,
        "wf-test-001",
        None,
        0,
        "step_one",
        None, // step_id
        None, // step_kind
        "continue",
        Some(r#"{"output": "step 1 done"}"#),
        None, // policy_summary
        None, // verification_refs
        None, // error_code
        now_ms,
        now_ms + 100,
    )
    .unwrap();

    insert_step_log_backend(
        &backend,
        "wf-test-001",
        None,
        1,
        "step_two",
        None, // step_id
        None, // step_kind
        "done",
        Some(r#"{"output": "final"}"#),
        None, // policy_summary
        None, // verification_refs
        None, // error_code
        now_ms + 100,
        now_ms + 300,
    )
    .unwrap();

    let logs = query_step_logs_backend(&backend, "wf-test-001").unwrap();

    assert_eq!(logs.len(), 2, "Should have 2 step logs");

    // Verify ordering by step_index
    assert_eq!(logs[0].step_index, 0);
    assert_eq!(logs[0].step_name, "step_one");
    assert_eq!(logs[0].result_type, "continue");
    assert_eq!(logs[0].duration_ms, 100);

    assert_eq!(logs[1].step_index, 1);
    assert_eq!(logs[1].step_name, "step_two");
    assert_eq!(logs[1].result_type, "done");
    assert_eq!(logs[1].duration_ms, 200);

    let conn = backend.into_connection();
    let direct_logs = query_step_logs(&conn, "wf-test-001").unwrap();
    assert_eq!(direct_logs.len(), logs.len());
    assert_eq!(direct_logs[0].id, logs[0].id);
    assert_eq!(direct_logs[1].id, logs[1].id);
}

#[test]
fn query_step_logs_returns_empty_for_unknown_workflow() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let backend = RusqliteBackend::new(conn);
    let logs = query_step_logs_backend(&backend, "nonexistent-workflow").unwrap();

    assert!(
        logs.is_empty(),
        "Should return empty vec for unknown workflow"
    );

    let conn = backend.into_connection();
    let direct_logs = query_step_logs(&conn, "nonexistent-workflow").unwrap();
    assert!(
        direct_logs.is_empty(),
        "direct fallback should return empty vec for unknown workflow"
    );
}

#[test]
fn query_latest_step_log_returns_last_step() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-test-latest", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    let backend = RusqliteBackend::new(conn);
    insert_step_log_backend(
        &backend,
        "wf-test-latest",
        None,
        0,
        "step_one",
        None,
        None,
        "continue",
        None,
        None,
        None,
        None,
        now_ms,
        now_ms + 100,
    )
    .unwrap();

    insert_step_log_backend(
        &backend,
        "wf-test-latest",
        None,
        2,
        "step_three",
        None,
        None,
        "done",
        None,
        None,
        None,
        None,
        now_ms + 200,
        now_ms + 400,
    )
    .unwrap();

    insert_step_log_backend(
        &backend,
        "wf-test-latest",
        None,
        1,
        "step_two",
        None,
        None,
        "continue",
        None,
        None,
        None,
        None,
        now_ms + 100,
        now_ms + 200,
    )
    .unwrap();

    let latest = query_latest_step_log_backend(&backend, "wf-test-latest")
        .unwrap()
        .unwrap();
    assert_eq!(latest.step_index, 2);
    assert_eq!(latest.step_name, "step_three");
    assert_eq!(latest.result_type, "done");

    let conn = backend.into_connection();
    let direct_latest = query_latest_step_log(&conn, "wf-test-latest")
        .unwrap()
        .unwrap();
    assert_eq!(direct_latest.id, latest.id);
    assert_eq!(direct_latest.step_index, latest.step_index);
}

#[test]
fn query_latest_step_log_returns_none_for_unknown_workflow() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let backend = RusqliteBackend::new(conn);
    let latest = query_latest_step_log_backend(&backend, "unknown-workflow").unwrap();
    assert!(latest.is_none());

    let conn = backend.into_connection();
    let direct_latest = query_latest_step_log(&conn, "unknown-workflow").unwrap();
    assert!(direct_latest.is_none());
}

#[test]
fn workflow_step_log_result_data_is_optional() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-test-002", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    // Insert step log without result_data through the StorageBackend trait path.
    let backend = RusqliteBackend::new(conn);
    insert_step_log_backend(
        &backend,
        "wf-test-002",
        None,
        0,
        "simple_step",
        None, // step_id
        None, // step_kind
        "continue",
        None, // result_data
        None, // policy_summary
        None, // verification_refs
        None, // error_code
        now_ms,
        now_ms + 50,
    )
    .unwrap();

    let conn = backend.into_connection();
    let logs = query_step_logs(&conn, "wf-test-002").unwrap();

    assert_eq!(logs.len(), 1);
    assert!(logs[0].result_data.is_none(), "result_data should be None");
}

#[test]
fn workflow_step_log_record_serializes() {
    let log = WorkflowStepLogRecord {
        id: 1,
        workflow_id: "wf-001".to_string(),
        audit_action_id: None,
        step_index: 0,
        step_name: "init".to_string(),
        step_id: None,
        step_kind: None,
        result_type: "continue".to_string(),
        result_data: Some(r#"{"status": "ok"}"#.to_string()),
        policy_summary: None,
        verification_refs: None,
        error_code: None,
        started_at: 1_700_000_000_000,
        completed_at: 1_700_000_000_100,
        duration_ms: 100,
    };

    let json = serde_json::to_string(&log).unwrap();
    assert!(json.contains("wf-001"));
    assert!(json.contains("init"));
    assert!(json.contains("duration_ms"));
}

#[test]
fn can_insert_and_query_workflow_action_plan() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-plan-001", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    let plan = crate::plan::ActionPlan::builder("Test Plan", "workspace-1")
        .add_step(crate::plan::StepPlan::new(
            1,
            crate::plan::StepAction::SendText {
                pane_id: 1,
                text: "hello".to_string(),
                paste_mode: None,
            },
            "Send hello",
        ))
        .build();

    let backend = RusqliteBackend::new(conn);
    let record = action_plan_record_from_plan("wf-plan-001", &plan).unwrap();
    upsert_action_plan_backend(&backend, &record).unwrap();

    let fetched = query_action_plan_backend(&backend, "wf-plan-001")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.plan_id, plan.plan_id.to_string());
    assert_eq!(fetched.plan_hash, plan.compute_hash());

    let conn = backend.into_connection();
    let direct_fetched = query_action_plan(&conn, "wf-plan-001").unwrap().unwrap();
    assert_eq!(direct_fetched.plan_id, fetched.plan_id);
    assert_eq!(direct_fetched.plan_hash, fetched.plan_hash);

    let parsed: crate::plan::ActionPlan = serde_json::from_str(&fetched.plan_json).unwrap();
    assert_eq!(parsed.plan_id, plan.plan_id);
}

#[test]
fn query_action_plan_returns_none_for_unknown_workflow() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let backend = RusqliteBackend::new(conn);
    let fetched = query_action_plan_backend(&backend, "missing-workflow").unwrap();
    assert!(fetched.is_none());

    let conn = backend.into_connection();
    let direct_fetched = query_action_plan(&conn, "missing-workflow").unwrap();
    assert!(direct_fetched.is_none());
}

#[test]
fn can_insert_and_consume_prepared_plan() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    let record = PreparedPlanRecord {
        plan_id: "plan:abcd1234".to_string(),
        plan_hash: "sha256:abcd1234".to_string(),
        workspace_id: "/tmp/wa".to_string(),
        action_kind: "send_text".to_string(),
        pane_id: Some(1),
        pane_uuid: None,
        params_json: Some(r#"{"type":"send_text","pane_id":1}"#.to_string()),
        plan_json: r#"{"plan_id":"plan:abcd1234","plan_hash":"sha256:abcd1234"}"#.to_string(),
        requires_approval: false,
        created_at: now_ms,
        expires_at: now_ms + 60_000,
        consumed_at: None,
    };

    with_writer_backend(&mut conn, |backend| {
        insert_prepared_plan_backend(backend, &record)
    })
    .unwrap();
    let fetched = query_prepared_plan(&conn, "plan:abcd1234")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.plan_id, record.plan_id);
    assert_eq!(fetched.action_kind, "send_text");

    let consumed = with_writer_backend(&mut conn, |backend| {
        consume_prepared_plan_backend(backend, "plan:abcd1234", now_ms + 1)
    })
        .unwrap()
        .unwrap();
    assert!(consumed.consumed_at.is_some());

    let second = with_writer_backend(&mut conn, |backend| {
        consume_prepared_plan_backend(backend, "plan:abcd1234", now_ms + 2)
    })
    .unwrap();
    assert!(second.is_none());
}

#[test]
fn prepared_plan_query_rejects_invalid_requires_approval_flag() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
        "INSERT INTO prepared_plans (
            plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
            plan_json, requires_approval, created_at, expires_at, consumed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            "plan:bad-approval",
            "sha256:bad-approval",
            "/tmp/wa",
            "send_text",
            Option::<i64>::None,
            Option::<String>::None,
            Option::<String>::None,
            "{}",
            2i64,
            now_ms,
            now_ms + 60_000,
            Option::<i64>::None,
        ],
    )
    .unwrap();

    let err = query_prepared_plan(&conn, "plan:bad-approval")
        .expect_err("invalid requires_approval flag");
    let message = err.to_string();
    assert!(
        message.contains("prepared_plans.requires_approval"),
        "{message}"
    );
    assert!(message.contains("must be 0 or 1"), "{message}");
}

#[test]
fn prepared_plan_query_rejects_negative_pane_id() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
        "INSERT INTO prepared_plans (
            plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
            plan_json, requires_approval, created_at, expires_at, consumed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            "plan:bad-pane",
            "sha256:bad-pane",
            "/tmp/wa",
            "send_text",
            -9i64,
            Option::<String>::None,
            Option::<String>::None,
            "{}",
            0i64,
            now_ms,
            now_ms + 60_000,
            Option::<i64>::None,
        ],
    )
    .unwrap();

    let err =
        query_prepared_plan(&conn, "plan:bad-pane").expect_err("negative prepared plan pane id");
    let message = err.to_string();
    assert!(message.contains("prepared_plans.pane_id"), "{message}");
    assert!(message.contains("-9"), "{message}");
}

// =========================================================================
// wa-4vx.3.7: Async StorageHandle Tests
// =========================================================================

// [ft-upvjr / ft-3tvvt] The lighter-weight `run_async_test` was removed
// — every `#[test]` in `storage.rs` and the four sibling test files
// (storage_handle_tests, queue_depth_tests, backpressure_integration_tests,
// timeline_integration_tests) now routes through the panic-catching +
// runtime-drop-absorbing `run_storage_async_test` below. Centralizing on
// the robust helper means a TLS destructor panic during runtime drop no
// longer leaks across `#[test]` boundaries on asupersync.

#[cfg(test)]
pub(super) fn run_storage_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use crate::runtime_async::CompatRuntime;
    let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build storage test runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(future);
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(runtime);
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::runtime_async::clear_runtime_handle();
    }));
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// [ft-iq339 / ft-3tvvt] Multi-thread sibling of
/// [`run_storage_async_test`] for proptest cases that need the
/// multi-thread runtime (writer + reader concurrency under
/// proptest-driven load). Same panic-catching + runtime-drop-absorbing
/// envelope, but the body returns the future's value so the proptest
/// can `prop_assert_*` on the verification results outside the
/// async block. Replaces the ad-hoc `RuntimeBuilder::multi_thread()
/// .build().expect(...).block_on(...)` boilerplate in
/// `storage::proptest_tests`.
#[cfg(test)]
pub(super) fn run_storage_proptest_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    use crate::runtime_async::CompatRuntime;
    let runtime = crate::runtime_async::RuntimeBuilder::multi_thread()
        .build()
        .expect("failed to build storage proptest runtime");
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(future)));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(runtime);
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::runtime_async::clear_runtime_handle();
    }));
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// ft-xbnl0.2.3 Cx-first: `StorageHandle::new_with_cx` must
/// open the DB identically to `new` when given a fresh cx —
/// producing a handle that supports the same upsert/shutdown
/// lifecycle as the legacy path.
#[test]
fn storage_handle_new_with_cx_succeeds_on_fresh_cx() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_new_with_cx_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        let pane = PaneRecord {
            pane_id: 7,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("cx-first".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();
        storage.shutdown().await.unwrap();

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: `StorageHandle::new_with_cx` must
/// return a `StorageError::Database` containing "cancelled"
/// when given a pre-cancelled cx, without creating the DB file
/// or spawning the writer thread.
#[test]
fn storage_handle_new_with_precancelled_cx_fails_before_fs_work() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!(
            "wa_test_new_with_cx_cancelled_{}.db",
            std::process::id()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("pre-cancel storage open"),
        );

        let result = StorageHandle::new_with_cx(&cx, &db_path_str).await;
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("new_with_cx should fail on pre-cancelled cx"),
        };

        let msg = err.to_string();
        assert!(
            msg.contains("cancelled"),
            "error should mention cancellation: {msg}"
        );
        assert!(
            !db_path.exists(),
            "DB file should not be created when cx is pre-cancelled"
        );
    });
}

/// ft-xbnl0.2.3 Cx-first: `upsert_pane_with_cx` with a fresh
/// cx must insert the pane identically to the legacy path.
#[test]
fn storage_upsert_pane_with_cx_succeeds_on_fresh_cx() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_upsert_pane_cx_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        let pane = PaneRecord {
            pane_id: 99,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("cx-first-upsert".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // Verify the pane landed in the DB by reading it back.
        let panes = storage.get_panes().await.unwrap();
        assert!(
            panes.iter().any(|p| p.pane_id == 99),
            "upserted pane 99 should be queryable"
        );

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: `upsert_pane_with_cx` with a
/// pre-cancelled cx must return error WITHOUT enqueuing the
/// write (observable via the pane not being queryable).
#[test]
fn storage_upsert_pane_with_precancelled_cx_skips_enqueue() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!(
            "wa_test_upsert_pane_cx_cancel_{}.db",
            std::process::id()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();
        let fresh_cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&fresh_cx, &db_path_str)
            .await
            .unwrap();

        let cancelled_cx = crate::cx::for_testing();
        cancelled_cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("pre-cancel upsert test"),
        );

        let pane = PaneRecord {
            pane_id: 55,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("never-landed".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        let result = storage.upsert_pane_with_cx(&cancelled_cx, pane).await;
        let err = match result {
            Err(e) => e,
            Ok(()) => panic!("upsert_pane_with_cx should fail on pre-cancelled cx"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("cancelled"),
            "error should mention cancellation: {msg}"
        );

        let panes = storage.get_panes().await.unwrap();
        assert!(
            !panes.iter().any(|p| p.pane_id == 55),
            "pre-cancelled upsert must NOT reach the DB"
        );

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 136 event-annotation cluster —
/// 7 new storage cx-first siblings exercised end-to-end:
/// `mark_event_handled_with_cx`, `set_event_triage_state_with_cx`,
/// `set_event_note_with_cx`, `add_event_label_with_cx`,
/// `remove_event_label_with_cx`, `add_event_mute_with_cx`,
/// `get_event_identity_key_with_cx`.
#[test]
fn storage_tick136_event_annotation_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick136_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed a pane (FK constraints for events).
        let pane = PaneRecord {
            pane_id: 17,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick136".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // Seed an event.
        let event = StoredEvent {
            id: 0,
            pane_id: 17,
            rule_id: "rule-tick136".to_string(),
            agent_type: "unknown".to_string(),
            event_type: "pattern".to_string(),
            severity: "info".to_string(),
            confidence: 0.9,
            extracted: None,
            matched_text: Some("hello".to_string()),
            segment_id: None,
            detected_at: 1_700_000_000_000,
            dedupe_key: Some("ident-tick136".to_string()),
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let event_id = storage.record_event_with_cx(&cx, event).await.unwrap();
        assert!(event_id > 0);

        // 1. get_event_identity_key_with_cx
        let ident = storage
            .get_event_identity_key_with_cx(&cx, event_id)
            .await
            .unwrap();
        let ident_key = ident.expect("event should have an identity key");
        assert!(
            !ident_key.is_empty(),
            "identity key should be populated for a recorded event"
        );

        // 2. mark_event_handled_with_cx
        storage
            .mark_event_handled_with_cx(&cx, event_id, Some("wf-136".to_string()), "handled")
            .await
            .unwrap();

        // 3. set_event_triage_state_with_cx
        let updated = storage
            .set_event_triage_state_with_cx(
                &cx,
                event_id,
                Some("triaged".to_string()),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(updated, "triage state update should have touched a row");

        // 4. set_event_note_with_cx
        storage
            .set_event_note_with_cx(
                &cx,
                event_id,
                Some("note-tick136".to_string()),
                Some("tester".to_string()),
            )
            .await
            .unwrap();

        // 5. add_event_label_with_cx
        let inserted = storage
            .add_event_label_with_cx(
                &cx,
                event_id,
                "label-a".to_string(),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(inserted, "label add should insert a new row");

        // 6. remove_event_label_with_cx
        let removed = storage
            .remove_event_label_with_cx(&cx, event_id, "label-a".to_string())
            .await
            .unwrap();
        assert!(removed, "label remove should delete the row we just added");

        // 7. add_event_mute_with_cx
        let mute = EventMuteRecord {
            identity_key: "ident-tick136".to_string(),
            scope: "workspace".to_string(),
            created_at: 1_700_000_000_000,
            expires_at: None,
            created_by: Some("tester".to_string()),
            reason: Some("test-mute".to_string()),
        };
        storage.add_event_mute_with_cx(&cx, mute).await.unwrap();
        let muted = storage
            .is_event_muted_with_cx(&cx, "ident-tick136", 1_700_000_000_001)
            .await
            .unwrap();
        assert!(muted, "event should be muted after add_event_mute_with_cx");

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 149 search/semantic cluster — the
/// final 10 siblings needed to complete storage cx-first coverage
/// (140/140 pub async fns).
///
/// Siblings exercised:
/// `search_with_cx`, `search_with_options_with_cx`,
/// `search_with_results_with_cx`, `store_embedding_with_cx`,
/// `get_unembedded_segments_with_cx`, `get_embedding_with_cx`,
/// `embedding_stats_with_cx`, `store_embedding_f32_with_cx`,
/// `semantic_search_with_cx`, `hybrid_search_with_results_with_cx`.
#[test]
fn storage_tick149_search_semantic_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick149_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // FTS lexical: an empty DB just exercises the call surface and
        // must roundtrip cleanly. The underlying search_fts_with_snippets
        // returns `Ok(vec![])` on a fresh index.
        let empty_search = storage.search_with_cx(&cx, "tick149").await.unwrap();
        assert!(empty_search.is_empty());
        let empty_opts = storage
            .search_with_options_with_cx(&cx, "tick149", SearchOptions::default())
            .await
            .unwrap();
        assert!(empty_opts.is_empty());
        let empty_results = storage
            .search_with_results_with_cx(&cx, "tick149", SearchOptions::default())
            .await
            .unwrap();
        assert!(empty_results.is_empty());

        // Seed pane + two segments for FK on segment_embeddings.
        let pane = PaneRecord {
            pane_id: 73,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick149".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();
        let seg_a = storage
            .append_segment_with_cx(&cx, 73, "hello world tick149", None)
            .await
            .unwrap();
        let seg_b = storage
            .append_segment_with_cx(&cx, 73, "second segment tick149", None)
            .await
            .unwrap();

        // Semantic: seed a synthetic embedding via store_embedding_f32_with_cx
        // (composite), then exercise get / get_unembedded / stats / semantic /
        // hybrid.
        let vec_f32 = [0.1_f32, 0.2, 0.3, 0.4];
        storage
            .store_embedding_f32_with_cx(&cx, seg_a.id, "embedder-tick149", &vec_f32)
            .await
            .unwrap();

        // Also exercise store_embedding_with_cx (bytes path) on seg_b.
        let raw_bytes = encode_f32_embedding_blob(&vec_f32).unwrap();
        storage
            .store_embedding_with_cx(&cx, seg_b.id, "embedder-tick149", 4, &raw_bytes)
            .await
            .unwrap();

        // get_embedding_with_cx — retrieves the bytes we stored for seg_a.
        let retrieved = storage
            .get_embedding_with_cx(&cx, seg_a.id, "embedder-tick149")
            .await
            .unwrap();
        assert!(retrieved.is_some());
        let retrieved_bytes = retrieved.unwrap();
        assert_eq!(retrieved_bytes, raw_bytes);

        // get_unembedded_segments_with_cx — both seg_a and seg_b have
        // embeddings now, so the set under "embedder-tick149" is empty.
        // A different embedder still sees both as unembedded.
        let unembedded_same = storage
            .get_unembedded_segments_with_cx(&cx, "embedder-tick149", 10)
            .await
            .unwrap();
        assert!(unembedded_same.is_empty());
        let unembedded_other = storage
            .get_unembedded_segments_with_cx(&cx, "embedder-other", 10)
            .await
            .unwrap();
        assert_eq!(unembedded_other.len(), 2);

        // embedding_stats_with_cx — one embedder, two rows.
        let stats = storage.embedding_stats_with_cx(&cx).await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].embedder_id, "embedder-tick149");
        assert_eq!(stats[0].count, 2);

        // semantic_search_with_cx — semantic hit list against our seeded
        // vectors. Exact order is a function of cosine similarity; we just
        // assert the call roundtrips and returns at most 2 hits.
        let hits = storage
            .semantic_search_with_cx(&cx, "embedder-tick149", &vec_f32, SearchOptions::default())
            .await
            .unwrap();
        assert!(hits.len() <= 2);

        // hybrid_search_with_results_with_cx — empty FTS corpus so bundle
        // will be empty-ish, but call must roundtrip cleanly.
        let bundle = storage
            .hybrid_search_with_results_with_cx(
                &cx,
                "tick149",
                SearchOptions::default(),
                "embedder-tick149",
                &vec_f32,
                SearchMode::Hybrid,
                60,
                0.5,
                0.5,
                Some(FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        let _ = bundle;

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 148 action-undo cluster —
/// 2 new storage cx-first siblings exercised end-to-end:
/// `upsert_action_undo_with_cx` and
/// `upsert_action_undo_redacted_with_cx` (composite that applies
/// redaction then routes through `upsert_action_undo_with_cx`).
#[test]
fn storage_tick148_action_undo_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick148_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed pane + two audit actions (FK target for action_undo).
        let pane = PaneRecord {
            pane_id: 63,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick148".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        let make_audit = |tag: &str| AuditActionRecord {
            id: 0,
            ts: 1_700_000_000_000,
            actor_kind: "human".to_string(),
            actor_id: Some(tag.to_string()),
            correlation_id: None,
            pane_id: Some(63),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };

        let audit_plain = storage
            .record_audit_action_with_cx(&cx, make_audit("tick148-plain"))
            .await
            .unwrap();
        let audit_red = storage
            .record_audit_action_with_cx(&cx, make_audit("tick148-red"))
            .await
            .unwrap();
        assert!(audit_plain > 0 && audit_red > audit_plain);

        // 1. upsert_action_undo_with_cx — straight insert, no redaction.
        let plain_rec = ActionUndoRecord {
            audit_action_id: audit_plain,
            undoable: true,
            undo_strategy: "manual".to_string(),
            undo_hint: Some("revert the send".to_string()),
            undo_payload: Some("{\"op\":\"noop\"}".to_string()),
            undone_at: None,
            undone_by: None,
        };
        storage
            .upsert_action_undo_with_cx(&cx, plain_rec)
            .await
            .unwrap();
        let plain_round = storage
            .get_action_undo_with_cx(&cx, audit_plain)
            .await
            .unwrap()
            .expect("plain undo row should exist");
        assert!(plain_round.undoable);
        assert_eq!(plain_round.undo_strategy, "manual");
        assert_eq!(plain_round.undo_hint.as_deref(), Some("revert the send"));
        assert!(
            storage
                .mark_action_undone_with_cx(&cx, audit_plain, "tick148")
                .await
                .unwrap()
        );
        assert!(
            !storage
                .mark_action_undone_with_cx(&cx, audit_plain, "tick148-again")
                .await
                .unwrap()
        );
        let marked_round = storage
            .get_action_undo_with_cx(&cx, audit_plain)
            .await
            .unwrap()
            .expect("marked undo row should still exist");
        assert_eq!(marked_round.undone_by.as_deref(), Some("tick148"));
        assert!(marked_round.undone_at.is_some());
        let history_round = storage
            .get_action_history_with_cx(
                &cx,
                ActionHistoryQuery {
                    audit_action_id: Some(audit_plain),
                    limit: Some(5),
                    pane_id: Some(63),
                    domain: Some("local".to_string()),
                    actor_kind: Some("human".to_string()),
                    actor_id: Some("tick148-plain".to_string()),
                    correlation_id: None,
                    action_kind: Some("send_text".to_string()),
                    policy_decision: Some("allow".to_string()),
                    rule_id: None,
                    result: Some("success".to_string()),
                    undoable: Some(true),
                    since: Some(1_700_000_000_000),
                    until: Some(1_700_000_000_000),
                },
            )
            .await
            .unwrap();
        assert_eq!(history_round.len(), 1);
        let history_row = &history_round[0];
        assert_eq!(history_row.id, audit_plain);
        assert_eq!(history_row.actor_id.as_deref(), Some("tick148-plain"));
        assert_eq!(history_row.pane_id, Some(63));
        assert_eq!(history_row.domain.as_deref(), Some("local"));
        assert_eq!(history_row.undoable, Some(true));
        assert_eq!(history_row.undo_strategy.as_deref(), Some("manual"));
        assert_eq!(history_row.undo_hint.as_deref(), Some("revert the send"));
        assert_eq!(history_row.undone_by.as_deref(), Some("tick148"));
        assert!(history_row.undone_at.is_some());

        // 2. upsert_action_undo_redacted_with_cx — composite; hint + payload
        //    are routed through Redactor before the cx-threaded insert.
        //    The Redactor doesn't guarantee transformation of any specific
        //    string (it's a no-op on benign input), but the roundtrip must
        //    land the record and match what `redact_fields` produced.
        let template = ActionUndoRecord {
            audit_action_id: audit_red,
            undoable: true,
            undo_strategy: "manual".to_string(),
            undo_hint: Some("benign hint text".to_string()),
            undo_payload: Some("{\"k\":\"v\"}".to_string()),
            undone_at: None,
            undone_by: None,
        };
        let redactor = Redactor::new();
        let mut expected = template.clone();
        expected.redact_fields(&redactor);

        storage
            .upsert_action_undo_redacted_with_cx(&cx, template.clone())
            .await
            .unwrap();
        let red_round = storage
            .get_action_undo_with_cx(&cx, audit_red)
            .await
            .unwrap()
            .expect("redacted undo row should exist");
        assert_eq!(red_round.undo_hint, expected.undo_hint);
        assert_eq!(red_round.undo_payload, expected.undo_payload);

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 147 misc step-log/plan/audit cluster —
/// 9 storage cx-first siblings exercised end-to-end:
/// `insert_step_log_with_cx`, `get_latest_step_log_with_cx`,
/// `get_audit_actions_with_cx`, `get_audit_actions_stream_with_cx`,
/// `get_approval_token_with_cx`, `get_segments_with_cx`,
/// `get_action_plan_with_cx`, `get_prepared_plan_with_cx`,
/// `is_writable_with_cx`.
#[test]
fn storage_tick147_misc_step_log_plan_audit_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick147_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed pane for FK.
        let pane = PaneRecord {
            pane_id: 57,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick147".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // Seed workflow for step_log FK.
        let workflow = WorkflowRecord {
            id: "wf-tick147".to_string(),
            workflow_name: "demo".to_string(),
            pane_id: 57,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            completed_at: None,
        };
        storage
            .upsert_workflow_with_cx(&cx, workflow)
            .await
            .unwrap();

        // 1. insert_step_log_with_cx
        storage
            .insert_step_log_with_cx(
                &cx,
                "wf-tick147",
                None,
                0,
                "step-zero",
                Some("step0".to_string()),
                Some("send_text".to_string()),
                "continue",
                None,
                None,
                None,
                None,
                1_700_000_000_000,
                1_700_000_000_100,
            )
            .await
            .unwrap();

        // 2. get_latest_step_log_with_cx — should match what we just wrote.
        let latest = storage
            .get_latest_step_log_with_cx(&cx, "wf-tick147")
            .await
            .unwrap()
            .expect("step log just inserted");
        assert_eq!(latest.step_name, "step-zero");
        assert_eq!(latest.step_index, 0);

        let audit_id = storage
            .record_audit_action_with_cx(
                &cx,
                AuditActionRecord {
                    id: 0,
                    ts: 1_700_000_000_050,
                    actor_kind: "robot".to_string(),
                    actor_id: Some("tick147".to_string()),
                    correlation_id: Some("corr-tick147".to_string()),
                    pane_id: Some(57),
                    domain: Some("local".to_string()),
                    action_kind: "send_text".to_string(),
                    policy_decision: "allow".to_string(),
                    decision_reason: Some("ok".to_string()),
                    rule_id: Some("rule.tick147".to_string()),
                    input_summary: Some("input".to_string()),
                    verification_summary: Some("verified".to_string()),
                    decision_context: Some("{\"kind\":\"test\"}".to_string()),
                    result: "success".to_string(),
                },
            )
            .await
            .unwrap();

        // 3. get_audit_actions_with_cx — filtered query returns the seeded
        //    audit row through the cx-first backend read path.
        let audits = storage
            .get_audit_actions_with_cx(
                &cx,
                AuditQuery {
                    pane_id: Some(57),
                    limit: Some(10),
                    domain: Some("local".to_string()),
                    actor_kind: Some("robot".to_string()),
                    actor_id: Some("tick147".to_string()),
                    correlation_id: Some("corr-tick147".to_string()),
                    action_kind: Some("send_text".to_string()),
                    policy_decision: Some("allow".to_string()),
                    rule_id: Some("rule.tick147".to_string()),
                    result: Some("success".to_string()),
                    since: Some(1_700_000_000_000),
                    until: Some(1_700_000_000_100),
                },
            )
            .await
            .unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].id, audit_id);

        // 4. get_audit_actions_stream_with_cx — filtered query returns the
        //    same seeded audit row through the cursor backend read path.
        let stream = storage
            .get_audit_actions_stream_with_cx(
                &cx,
                AuditStreamQuery {
                    cursor: None,
                    limit: Some(10),
                    offset: None,
                    pane_id: Some(57),
                    domain: Some("local".to_string()),
                    actor_kind: Some("robot".to_string()),
                    actor_id: Some("tick147".to_string()),
                    correlation_id: Some("corr-tick147".to_string()),
                    action_kind: Some("send_text".to_string()),
                    policy_decision: Some("allow".to_string()),
                    rule_id: Some("rule.tick147".to_string()),
                    result: Some("success".to_string()),
                    since: Some(1_700_000_000_000),
                    until: Some(1_700_000_000_100),
                },
            )
            .await
            .unwrap();
        assert_eq!(stream.records.len(), 1);
        assert_eq!(stream.records[0].id, audit_id);
        assert_eq!(stream.next_cursor, Some(audit_id));

        // 5. get_approval_token_with_cx — no tokens, returns None.
        let token = storage
            .get_approval_token_with_cx(&cx, "nonexistent-hash")
            .await
            .unwrap();
        assert!(token.is_none());

        // 6. get_segments_with_cx — no segments captured, empty list.
        let segments = storage.get_segments_with_cx(&cx, 57, 10).await.unwrap();
        assert!(segments.is_empty());

        // 7. get_action_plan_with_cx — no plan upserted, returns None.
        let plan = storage
            .get_action_plan_with_cx(&cx, "wf-tick147")
            .await
            .unwrap();
        assert!(plan.is_none());

        // 8. get_prepared_plan_with_cx — likewise None.
        let prepared = storage
            .get_prepared_plan_with_cx(&cx, "plan-nonexistent")
            .await
            .unwrap();
        assert!(prepared.is_none());

        // 9. is_writable_with_cx — writer thread up, expect true.
        let writable = storage.is_writable_with_cx(&cx).await.unwrap();
        assert!(writable, "fresh storage should be writable");

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 146 event-reads cluster —
/// 6 storage cx-first siblings exercised end-to-end:
/// `get_unhandled_events_with_cx`, `get_events_with_cx`,
/// `get_events_stream_with_cx`,
/// `get_timeline_with_cx`, `count_unhandled_events_by_pane_with_cx`,
/// `get_last_activity_by_pane_with_cx`.
#[test]
fn storage_tick146_event_reads_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick146_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed a pane for FK.
        let pane = PaneRecord {
            pane_id: 41,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick146".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // Seed an event so the reads have material to return.
        let event = StoredEvent {
            id: 0,
            pane_id: 41,
            rule_id: "rule-tick146".to_string(),
            agent_type: "unknown".to_string(),
            event_type: "pattern".to_string(),
            severity: "info".to_string(),
            confidence: 0.9,
            extracted: None,
            matched_text: Some("tick146".to_string()),
            segment_id: None,
            detected_at: 1_700_000_000_000,
            dedupe_key: Some("ident-tick146".to_string()),
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let event_id = storage.record_event_with_cx(&cx, event).await.unwrap();
        assert!(event_id > 0);

        // 1. get_unhandled_events_with_cx
        let unhandled = storage.get_unhandled_events_with_cx(&cx, 10).await.unwrap();
        assert!(
            unhandled.iter().any(|e| e.id == event_id),
            "seeded event should show up in unhandled list"
        );

        assert!(
            storage
                .set_event_triage_state_with_cx(
                    &cx,
                    event_id,
                    Some("investigating".to_string()),
                    Some("tick146".to_string()),
                )
                .await
                .unwrap(),
            "seeded event should accept a triage state"
        );
        assert!(
            storage
                .add_event_label_with_cx(
                    &cx,
                    event_id,
                    "tick146-label".to_string(),
                    Some("tick146".to_string()),
                )
                .await
                .unwrap(),
            "seeded event should accept a label"
        );

        // 2. get_events_with_cx — filtered event query path.
        let filtered_events = storage
            .get_events_with_cx(
                &cx,
                EventQuery {
                    limit: Some(10),
                    pane_id: Some(41),
                    rule_id: Some("rule-tick146".to_string()),
                    event_type: Some("pattern".to_string()),
                    triage_state: Some("investigating".to_string()),
                    label: Some("tick146-label".to_string()),
                    unhandled_only: true,
                    since: Some(1_699_999_999_999),
                    until: Some(1_700_000_000_001),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            filtered_events.iter().filter(|e| e.id == event_id).count(),
            1,
            "filtered event query should return the seeded event exactly once"
        );

        // 3. get_events_stream_with_cx — ID-cursor ordering, no filter.
        let streamed = storage
            .get_events_stream_with_cx(
                &cx,
                EventStreamQuery {
                    after_id: None,
                    limit: Some(10),
                    pane_id: None,
                    rule_id: None,
                    event_type: None,
                    triage_state: None,
                    label: None,
                    unhandled_only: false,
                    since: None,
                    until: None,
                },
            )
            .await
            .unwrap();
        assert!(streamed.iter().any(|e| e.id == event_id));

        // 4. get_timeline_with_cx — lenient query, should include our event.
        let timeline = storage
            .get_timeline_with_cx(
                &cx,
                TimelineQuery {
                    start: None,
                    end: None,
                    pane_ids: None,
                    severities: None,
                    event_types: None,
                    agent_types: None,
                    unhandled_only: false,
                    include_correlations: false,
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        // Timeline wraps entries; we just need the call to roundtrip and
        // include at least one entry for pane 41.
        assert!(timeline.events.iter().any(|event| event.pane_info.pane_id == 41));

        // br-ft-l87np: force the large-pane-id path so query_timeline stages
        // pane ids into a temp table instead of binding a long IN-clause.
        let large_pane_filter = (0..=TIMELINE_PANE_ID_INLINE_LIMIT as u64)
            .chain(std::iter::once(41))
            .collect::<Vec<_>>();
        let large_filter_timeline = storage
            .get_timeline_with_cx(
                &cx,
                TimelineQuery {
                    start: None,
                    end: None,
                    pane_ids: Some(large_pane_filter),
                    severities: None,
                    event_types: None,
                    agent_types: None,
                    unhandled_only: false,
                    include_correlations: false,
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert!(
            large_filter_timeline
                .events
                .iter()
                .any(|event| event.pane_info.pane_id == 41),
            "large pane-id filter should include the staged pane id"
        );
        assert_eq!(
            large_filter_timeline.total_count, 1,
            "large pane-id temp-table count should keep the subquery predicate intact"
        );

        // 5. count_unhandled_events_by_pane_with_cx
        let counts = storage
            .count_unhandled_events_by_pane_with_cx(&cx)
            .await
            .unwrap();
        assert_eq!(counts.get(&41), Some(&1));

        // 6. count_events_by_tier_with_cx
        let severities = vec!["info".to_string()];
        let event_types = vec!["pattern".to_string()];
        let tier_count = storage
            .count_events_by_tier_with_cx(
                &cx,
                1_800_000_000_000,
                &severities,
                &event_types,
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(tier_count, 1);

        // 7. get_last_activity_by_pane_with_cx — no segments recorded, so
        //    the map may not include pane 41. Just assert the call succeeds.
        let _activity = storage
            .get_last_activity_by_pane_with_cx(&cx)
            .await
            .unwrap();

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 145 reservation cluster —
/// 3 new storage cx-first siblings exercised end-to-end:
/// `create_reservation_with_cx`, `release_reservation_with_cx`,
/// `expire_stale_reservations_with_cx`.
#[test]
fn storage_tick145_reservation_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick145_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed a pane for FK.
        let pane = PaneRecord {
            pane_id: 31,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick145".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // 1. create_reservation_with_cx — 60s TTL, well above the 1s floor.
        let reservation = storage
            .create_reservation_with_cx(
                &cx,
                31,
                "agent",
                "owner-tick145",
                Some("running tick145"),
                60_000,
            )
            .await
            .unwrap();
        assert!(reservation.id > 0);
        assert_eq!(reservation.pane_id, 31);

        // 2. expire_stale_reservations_with_cx — our reservation is fresh,
        //    so the call should return 0 but still roundtrip.
        let expired = storage
            .expire_stale_reservations_with_cx(&cx)
            .await
            .unwrap();
        assert_eq!(expired, 0, "fresh reservation should not be expired");

        // 3. release_reservation_with_cx — first call removes the row,
        //    second call returns false.
        let released = storage
            .release_reservation_with_cx(&cx, reservation.id)
            .await
            .unwrap();
        assert!(released);
        let second = storage
            .release_reservation_with_cx(&cx, reservation.id)
            .await
            .unwrap();
        assert!(
            !second,
            "second release should return false (reservation already gone)"
        );

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 144 account cluster —
/// 5 new storage cx-first siblings exercised end-to-end:
/// `upsert_account_with_cx`, `update_account_last_used_with_cx`,
/// `delete_account_with_cx`, `get_account_with_cx`,
/// `select_account_with_cx` (composite routing via
/// `get_accounts_by_service_with_cx`).
#[test]
fn storage_tick144_account_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick144_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. upsert_account_with_cx — two accounts so select_account has
        //    real material to pick from.
        let acct_hi = crate::accounts::AccountRecord {
            id: 0,
            account_id: "acct-hi".to_string(),
            service: "openai".to_string(),
            name: Some("primary".to_string()),
            percent_remaining: 80.0,
            reset_at: None,
            tokens_used: None,
            tokens_remaining: None,
            tokens_limit: None,
            last_refreshed_at: 1_700_000_000_000,
            last_used_at: None,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        };
        let id_hi = storage.upsert_account_with_cx(&cx, acct_hi).await.unwrap();
        assert!(id_hi > 0);

        let acct_lo = crate::accounts::AccountRecord {
            id: 0,
            account_id: "acct-lo".to_string(),
            service: "openai".to_string(),
            name: Some("secondary".to_string()),
            percent_remaining: 20.0,
            reset_at: None,
            tokens_used: None,
            tokens_remaining: None,
            tokens_limit: None,
            last_refreshed_at: 1_700_000_000_000,
            last_used_at: None,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        };
        let id_lo = storage.upsert_account_with_cx(&cx, acct_lo).await.unwrap();
        assert!(id_lo > 0 && id_lo != id_hi);

        // 2. get_account_with_cx
        let fetched = storage
            .get_account_with_cx(&cx, "openai", "acct-hi")
            .await
            .unwrap()
            .expect("acct-hi should exist");
        assert!((fetched.percent_remaining - 80.0).abs() < 1e-9);

        // 3. select_account_with_cx — composite; with the default 5% threshold
        //    and quota-ranked ordering, acct-hi (80%) should win over
        //    acct-lo (20%).
        let selection = storage
            .select_account_with_cx(
                &cx,
                "openai",
                &crate::accounts::AccountSelectionConfig::default(),
            )
            .await
            .unwrap();
        let selected = selection
            .selected
            .expect("select_account_with_cx should pick an eligible account");
        assert_eq!(selected.account_id, "acct-hi");

        // 4. update_account_last_used_with_cx
        storage
            .update_account_last_used_with_cx(&cx, "openai", "acct-hi", 1_700_000_001_000)
            .await
            .unwrap();
        let after_use = storage
            .get_account_with_cx(&cx, "openai", "acct-hi")
            .await
            .unwrap()
            .expect("acct-hi should still exist");
        assert_eq!(after_use.last_used_at, Some(1_700_000_001_000));

        // 5. delete_account_with_cx
        let deleted = storage
            .delete_account_with_cx(&cx, "openai", "acct-lo")
            .await
            .unwrap();
        assert!(deleted, "delete_account_with_cx should remove acct-lo");
        let missing = storage
            .get_account_with_cx(&cx, "openai", "acct-lo")
            .await
            .unwrap();
        assert!(missing.is_none(), "acct-lo should be gone after delete");
        let not_there = storage
            .delete_account_with_cx(&cx, "openai", "acct-lo")
            .await
            .unwrap();
        assert!(
            !not_there,
            "second delete should return false (no row affected)"
        );

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 143 mux-session/checkpoint cluster —
/// 8 storage cx-first siblings exercised end-to-end:
/// `insert_mux_session_with_cx`,
/// `insert_session_checkpoint_with_cx`,
/// `prune_session_checkpoints_with_cx`,
/// `mark_session_shutdown_clean_with_cx`,
/// `get_latest_checkpoint_hash_with_cx`,
/// `get_agent_session_with_cx`,
/// `get_active_sessions_with_cx`,
/// `get_sessions_for_pane_with_cx`.
#[test]
fn storage_tick143_mux_session_checkpoint_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick143_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. insert_mux_session_with_cx
        let session_id = "sess-tick143".to_string();
        storage
            .insert_mux_session_with_cx(
                &cx,
                session_id.clone(),
                "{\"panes\":[]}".to_string(),
                "test-ft-version".to_string(),
                Some("host-tick143".to_string()),
            )
            .await
            .unwrap();

        // 2. insert_session_checkpoint_with_cx — three checkpoints so prune
        //    below has something to remove.
        for i in 0..3 {
            let pane_states = if i == 0 {
                vec![SessionPaneStateRow {
                    pane_id: 143,
                    cwd: Some("/tmp/tick143".to_string()),
                    command: Some("ft-test-shell".to_string()),
                    env_json: Some("{\"TERM\":\"xterm-256color\"}".to_string()),
                    terminal_state_json: "{\"cursor\":[0,0]}".to_string(),
                    agent_metadata_json: Some("{\"agent\":\"tick143\"}".to_string()),
                    scrollback_checkpoint_seq: Some(7),
                    last_output_at: Some(143_000),
                }]
            } else {
                Vec::new()
            };
            let pane_count = pane_states.len();
            let total_bytes = pane_states
                .iter()
                .map(|ps| ps.terminal_state_json.len())
                .sum();
            let metadata_json = (i == 0).then(|| "{\"source\":\"tick143\"}".to_string());
            let checkpoint_id = storage
                .insert_session_checkpoint_with_cx(
                    &cx,
                    session_id.clone(),
                    "periodic".to_string(),
                    format!("state-hash-{i}"),
                    pane_count,
                    total_bytes,
                    metadata_json,
                    pane_states,
                )
                .await
                .unwrap();
            assert!(checkpoint_id > 0);
        }

        // 3. get_latest_checkpoint_hash_with_cx — should match the last one.
        let latest = storage
            .get_latest_checkpoint_hash_with_cx(&cx, session_id.clone())
            .await
            .unwrap();
        assert_eq!(latest.as_deref(), Some("state-hash-2"));

        // 4. prune_session_checkpoints_with_cx — retain 1, should remove 2.
        let pruned = storage
            .prune_session_checkpoints_with_cx(&cx, session_id.clone(), 1)
            .await
            .unwrap();
        assert_eq!(
            pruned, 2,
            "retention=1 should prune the two older checkpoints"
        );

        // Latest should still be state-hash-2 after pruning.
        let after_prune = storage
            .get_latest_checkpoint_hash_with_cx(&cx, session_id.clone())
            .await
            .unwrap();
        assert_eq!(after_prune.as_deref(), Some("state-hash-2"));

        // 5. mark_session_shutdown_clean_with_cx — doesn't return state,
        //    just has to succeed.
        storage
            .mark_session_shutdown_clean_with_cx(&cx, session_id.clone())
            .await
            .unwrap();

        // 6. AgentSession read cluster — seed one session so the backend
        //    cell decoder covers nullable text + optional float fields.
        storage
            .upsert_pane_with_cx(
                &cx,
                PaneRecord {
                    pane_id: 143,
                    pane_uuid: Some("pane-uuid-tick143".to_string()),
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: Some("tick143-agent".to_string()),
                    cwd: Some("/tmp/tick143".to_string()),
                    tty_name: None,
                    first_seen_at: 143_000,
                    last_seen_at: 143_001,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                },
            )
            .await
            .unwrap();

        let mut agent_session = AgentSessionRecord::new_start(143, "codex");
        agent_session.session_id = Some("codex-session-tick143".to_string());
        agent_session.external_id = Some(String::new());
        agent_session.external_meta = Some(serde_json::json!({"source":"tick143"}));
        agent_session.model_name = Some("gpt-test".to_string());
        agent_session.total_tokens = Some(1430);
        agent_session.estimated_cost_usd = Some(0.125);
        let agent_session_id = storage
            .upsert_agent_session_with_cx(&cx, agent_session)
            .await
            .unwrap();

        let loaded = storage
            .get_agent_session_with_cx(&cx, agent_session_id)
            .await
            .unwrap()
            .expect("seeded agent session should load");
        assert_eq!(loaded.pane_id, 143);
        assert_eq!(loaded.session_id.as_deref(), Some("codex-session-tick143"));
        assert_eq!(loaded.external_id.as_deref(), Some(""));
        assert_eq!(loaded.total_tokens, Some(1430));
        assert_eq!(loaded.estimated_cost_usd, Some(0.125));

        // 7. get_active_sessions_with_cx.
        let active = storage.get_active_sessions_with_cx(&cx).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, agent_session_id);

        // 8. get_sessions_for_pane_with_cx.
        let pane_sessions = storage
            .get_sessions_for_pane_with_cx(&cx, 143)
            .await
            .unwrap();
        assert_eq!(pane_sessions.len(), 1);
        assert_eq!(pane_sessions[0].id, agent_session_id);

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 142 token-lifecycle clusters —
/// 4 new storage cx-first siblings across two related clusters:
/// approval-token-code lookup/consume, and prepared-plan insert/consume.
/// `get_approval_token_by_code_with_cx`,
/// `consume_approval_token_by_code_with_cx`,
/// `insert_prepared_plan_with_cx`, `consume_prepared_plan_with_cx`.
#[test]
fn storage_tick142_token_lifecycle_clusters_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick142_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed pane for FK constraints on both approval_tokens and
        // prepared_plans (both reference pane_id).
        let pane = PaneRecord {
            pane_id: 9,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick142".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // ---- approval-token-code cluster ----
        let token = ApprovalTokenRecord {
            id: 0,
            code_hash: "code-hash-tick142".to_string(),
            created_at: 1_700_000_000_000,
            expires_at: 4_100_000_000_000,
            used_at: None,
            workspace_id: "ws-tick142".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(9),
            action_fingerprint: "fp-tick142".to_string(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        let token_id = storage
            .insert_approval_token_with_cx(&cx, token)
            .await
            .unwrap();
        assert!(token_id > 0);
        let active_count = storage
            .count_active_approvals_with_cx(&cx, "ws-tick142", 1_700_000_000_500)
            .await
            .unwrap();
        assert_eq!(active_count, 1);
        assert!(
            storage
                .has_active_approval_for_scope_blocking(
                    "ws-tick142",
                    "send_text",
                    Some(9),
                    "fp-tick142",
                    1_700_000_000_500,
                )
                .unwrap()
        );

        // 1. get_approval_token_by_code_with_cx — unused, should return Some.
        let fetched = storage
            .get_approval_token_by_code_with_cx(&cx, "code-hash-tick142", "ws-tick142")
            .await
            .unwrap()
            .expect("approval token should exist before consume");
        assert_eq!(fetched.workspace_id, "ws-tick142");
        assert!(fetched.used_at.is_none());
        let fetched_by_hash = storage
            .get_approval_token_with_cx(&cx, "code-hash-tick142")
            .await
            .unwrap()
            .expect("approval token should be found by hash before consume");
        assert_eq!(fetched_by_hash.id, fetched.id);

        // 2. consume_approval_token_by_code_with_cx — should return the token
        //    and mark it used. Repeat consume should now return None.
        let consumed = storage
            .consume_approval_token_by_code_with_cx(&cx, "code-hash-tick142", "ws-tick142")
            .await
            .unwrap()
            .expect("approval token should be consumable once");
        assert_eq!(consumed.code_hash, "code-hash-tick142");
        let after_consume = storage
            .consume_approval_token_by_code_with_cx(&cx, "code-hash-tick142", "ws-tick142")
            .await
            .unwrap();
        assert!(
            after_consume.is_none(),
            "approval token can only be consumed once"
        );
        let active_after_consume = storage
            .count_active_approvals_with_cx(&cx, "ws-tick142", 1_700_000_000_600)
            .await
            .unwrap();
        assert_eq!(active_after_consume, 0);
        assert!(
            !storage
                .has_active_approval_for_scope_blocking(
                    "ws-tick142",
                    "send_text",
                    Some(9),
                    "fp-tick142",
                    1_700_000_000_600,
                )
                .unwrap()
        );

        let scoped_token = ApprovalTokenRecord {
            id: 0,
            code_hash: "scoped-code-hash-tick142".to_string(),
            created_at: 1_700_000_000_000,
            expires_at: 4_100_000_000_000,
            used_at: None,
            workspace_id: "ws-tick142".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(9),
            action_fingerprint: "fp-scoped-tick142".to_string(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        let scoped_token_id = storage
            .insert_approval_token_with_cx(&cx, scoped_token)
            .await
            .unwrap();
        assert!(scoped_token_id > 0);
        let wrong_scope = storage
            .consume_approval_token_with_cx(
                &cx,
                "scoped-code-hash-tick142",
                "ws-tick142",
                "send_text",
                Some(9),
                "wrong-fingerprint",
            )
            .await
            .unwrap();
        assert!(
            wrong_scope.is_none(),
            "scope mismatch must not consume approval token"
        );
        let scoped_consumed = storage
            .consume_approval_token_with_cx(
                &cx,
                "scoped-code-hash-tick142",
                "ws-tick142",
                "send_text",
                Some(9),
                "fp-scoped-tick142",
            )
            .await
            .unwrap()
            .expect("scoped approval token should be consumable once");
        assert_eq!(scoped_consumed.id, scoped_token_id);
        assert!(scoped_consumed.used_at.is_some());
        let scoped_after = storage
            .consume_approval_token_with_cx(
                &cx,
                "scoped-code-hash-tick142",
                "ws-tick142",
                "send_text",
                Some(9),
                "fp-scoped-tick142",
            )
            .await
            .unwrap();
        assert!(
            scoped_after.is_none(),
            "scoped approval token can only be consumed once"
        );

        // ---- prepared-plan cluster ----
        let plan = PreparedPlanRecord {
            plan_id: "plan-tick142".to_string(),
            plan_hash: "hash-tick142".to_string(),
            workspace_id: "ws-tick142".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(9),
            pane_uuid: None,
            params_json: None,
            plan_json: "{}".to_string(),
            requires_approval: false,
            created_at: 1_700_000_000_000,
            expires_at: 4_100_000_000_000,
            consumed_at: None,
        };
        storage
            .insert_prepared_plan_with_cx(&cx, plan)
            .await
            .unwrap();
        let plan_fetched = storage
            .get_prepared_plan_with_cx(&cx, "plan-tick142")
            .await
            .unwrap()
            .expect("prepared plan should exist before consume");
        assert_eq!(plan_fetched.plan_hash, "hash-tick142");

        // 3. consume_prepared_plan_with_cx — first call returns the record,
        //    second returns None since the plan is now consumed.
        let plan_consumed = storage
            .consume_prepared_plan_with_cx(&cx, "plan-tick142", 1_700_000_000_500)
            .await
            .unwrap()
            .expect("prepared plan should be consumable once");
        assert_eq!(plan_consumed.plan_id, "plan-tick142");
        let plan_after = storage
            .consume_prepared_plan_with_cx(&cx, "plan-tick142", 1_700_000_000_600)
            .await
            .unwrap();
        assert!(
            plan_after.is_none(),
            "prepared plan should only be consumed once"
        );

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 141 maintenance cluster —
/// 6 new storage cx-first siblings exercised end-to-end:
/// `retention_cleanup_with_cx` (composite),
/// `aggregate_daily_metrics_with_cx`, `aggregate_by_agent_with_cx`,
/// `vacuum_with_cx`, `checkpoint_with_cx`,
/// `database_page_stats_with_cx`.
#[test]
fn storage_tick141_maintenance_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick141_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. vacuum_with_cx on an empty DB — should succeed.
        storage.vacuum_with_cx(&cx).await.unwrap();

        // 2. checkpoint_with_cx — returns a CheckpointResult.
        let _ckpt = storage.checkpoint_with_cx(&cx).await.unwrap();

        // 3. database_page_stats_with_cx — returns stats for the fresh DB.
        let _stats = storage.database_page_stats_with_cx(&cx).await.unwrap();

        // 4. aggregate_daily_metrics_with_cx — empty result on fresh DB,
        //    but the call must roundtrip cleanly.
        let daily = storage
            .aggregate_daily_metrics_with_cx(&cx, 0)
            .await
            .unwrap();
        assert!(daily.is_empty());

        // 5. aggregate_by_agent_with_cx — likewise empty on fresh DB.
        let by_agent = storage.aggregate_by_agent_with_cx(&cx, 0).await.unwrap();
        assert!(by_agent.is_empty());

        // 6. retention_cleanup_with_cx — composite that cx-threads both
        //    prune_segments_before_with_cx + record_maintenance_with_cx.
        //    On an empty DB it should delete 0 segments but still log the
        //    maintenance event.
        let deleted = storage
            .retention_cleanup_with_cx(&cx, 1_700_000_000_000)
            .await
            .unwrap();
        assert_eq!(deleted, 0, "no segments to delete on fresh DB");

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 140 FTS cluster —
/// 4 new storage cx-first siblings exercised end-to-end:
/// `get_indexing_health_with_cx`, `sync_fts_with_cx`,
/// `rebuild_fts_with_cx`, `get_fts_index_state_with_cx`.
#[test]
fn storage_tick140_fts_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick140_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. sync_fts_with_cx on a fresh DB — drives initial FTS state creation.
        let cfg = FtsSyncConfig::default();
        let sync_result = storage.sync_fts_with_cx(&cx, cfg.clone()).await.unwrap();
        let _ = sync_result; // contents depend on current index state; just asserting the call roundtrips is enough here.

        // 2. get_fts_index_state_with_cx — should be Some after sync_fts.
        let state = storage.get_fts_index_state_with_cx(&cx).await.unwrap();
        assert!(
            state.is_some(),
            "FTS index state should be populated after sync_fts_with_cx"
        );

        // 3. rebuild_fts_with_cx
        let rebuild_result = storage.rebuild_fts_with_cx(&cx, cfg).await.unwrap();
        let _ = rebuild_result;

        // 4. get_indexing_health_with_cx — returns a report even with no indexed data.
        let health = storage.get_indexing_health_with_cx(&cx).await.unwrap();
        let _ = health;

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 139 notification cluster —
/// 6 new storage cx-first siblings exercised end-to-end:
/// `record_notification_with_cx`,
/// `update_notification_status_with_cx`,
/// `acknowledge_notification_with_cx`,
/// `increment_notification_retry_with_cx`,
/// `query_notification_history_with_cx`,
/// `count_notification_history_before_with_cx`,
/// `get_notification_with_cx`.
#[test]
fn storage_tick139_notification_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick139_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. record_notification_with_cx
        let rec = NotificationHistoryRecord {
            id: 0,
            timestamp: 1_700_000_000_000,
            event_id: None,
            channel: "desktop".to_string(),
            title: "tick139 ping".to_string(),
            body: "hello".to_string(),
            severity: "info".to_string(),
            status: NotificationStatus::Pending,
            error_message: None,
            acknowledged_at: None,
            acknowledged_by: None,
            action_taken: None,
            retry_count: 0,
            metadata: None,
            created_at: 1_700_000_000_000,
        };
        let id = storage.record_notification_with_cx(&cx, rec).await.unwrap();
        assert!(id > 0);

        // 2. get_notification_with_cx — freshly recorded notification
        let fetched = storage.get_notification_with_cx(&cx, id).await.unwrap();
        assert_eq!(fetched.title, "tick139 ping");
        assert!(matches!(fetched.status, NotificationStatus::Pending));

        // 3. update_notification_status_with_cx — promote to Sent
        storage
            .update_notification_status_with_cx(&cx, id, NotificationStatus::Sent, None)
            .await
            .unwrap();
        let after_sent = storage.get_notification_with_cx(&cx, id).await.unwrap();
        assert!(matches!(after_sent.status, NotificationStatus::Sent));

        // 4. acknowledge_notification_with_cx
        storage
            .acknowledge_notification_with_cx(
                &cx,
                id,
                "operator-tick139".to_string(),
                Some("dismissed".to_string()),
            )
            .await
            .unwrap();
        let after_ack = storage.get_notification_with_cx(&cx, id).await.unwrap();
        assert_eq!(
            after_ack.acknowledged_by.as_deref(),
            Some("operator-tick139")
        );
        assert_eq!(after_ack.action_taken.as_deref(), Some("dismissed"));
        assert!(after_ack.acknowledged_at.is_some());

        // 5. increment_notification_retry_with_cx
        let before_retry = after_ack.retry_count;
        storage
            .increment_notification_retry_with_cx(&cx, id)
            .await
            .unwrap();
        let after_retry = storage.get_notification_with_cx(&cx, id).await.unwrap();
        assert_eq!(after_retry.retry_count, before_retry + 1);
        // increment_notification_retry resets status to Pending per the
        // method contract.
        assert!(matches!(after_retry.status, NotificationStatus::Pending));

        // 6. query_notification_history_with_cx — no filters, should include our row.
        let listed = storage
            .query_notification_history_with_cx(&cx, NotificationHistoryQuery::default())
            .await
            .unwrap();
        assert!(listed.iter().any(|n| n.id == id));

        // 7. count_notification_history_before_with_cx — includes the seeded row.
        let count = storage
            .count_notification_history_before_with_cx(&cx, 1_800_000_000_000)
            .await
            .unwrap();
        assert_eq!(count, 1);

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 138 pane-bookmark cluster —
/// 5 new storage cx-first siblings exercised end-to-end:
/// `insert_pane_bookmark_with_cx`, `delete_pane_bookmark_with_cx`,
/// `get_pane_bookmark_by_alias_with_cx`,
/// `list_pane_bookmarks_with_cx`,
/// `list_pane_bookmarks_by_tag_with_cx`.
#[test]
fn storage_tick138_pane_bookmark_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick138_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. insert_pane_bookmark_with_cx — two bookmarks so list/tag filter
        //    can prove ordering and selectivity.
        let rec_alpha = PaneBookmarkRecord {
            id: 0,
            pane_id: 21,
            alias: "alpha".to_string(),
            tags: Some(vec!["primary".to_string(), "tick138".to_string()]),
            description: Some("first bookmark".to_string()),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        };
        let id_alpha = storage
            .insert_pane_bookmark_with_cx(&cx, rec_alpha)
            .await
            .unwrap();
        assert!(id_alpha > 0);

        let rec_beta = PaneBookmarkRecord {
            id: 0,
            pane_id: 22,
            alias: "beta".to_string(),
            tags: Some(vec!["secondary".to_string(), "tick138".to_string()]),
            description: None,
            created_at: 1_700_000_000_100,
            updated_at: 1_700_000_000_100,
        };
        let id_beta = storage
            .insert_pane_bookmark_with_cx(&cx, rec_beta)
            .await
            .unwrap();
        assert!(id_beta > 0 && id_beta != id_alpha);

        // 2. get_pane_bookmark_by_alias_with_cx
        let fetched = storage
            .get_pane_bookmark_by_alias_with_cx(&cx, "alpha")
            .await
            .unwrap()
            .expect("alpha bookmark should exist");
        assert_eq!(fetched.pane_id, 21);
        assert_eq!(fetched.description.as_deref(), Some("first bookmark"));

        // 3. list_pane_bookmarks_with_cx — both bookmarks
        let all = storage.list_pane_bookmarks_with_cx(&cx).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|b| b.alias == "alpha"));
        assert!(all.iter().any(|b| b.alias == "beta"));

        // 4. list_pane_bookmarks_by_tag_with_cx
        let primary_only = storage
            .list_pane_bookmarks_by_tag_with_cx(&cx, "primary")
            .await
            .unwrap();
        assert_eq!(primary_only.len(), 1);
        assert_eq!(primary_only[0].alias, "alpha");

        let tick138_both = storage
            .list_pane_bookmarks_by_tag_with_cx(&cx, "tick138")
            .await
            .unwrap();
        assert_eq!(tick138_both.len(), 2);

        // 5. delete_pane_bookmark_with_cx
        let deleted = storage
            .delete_pane_bookmark_with_cx(&cx, "alpha")
            .await
            .unwrap();
        assert!(deleted, "delete_pane_bookmark_with_cx should remove alpha");
        let after = storage
            .get_pane_bookmark_by_alias_with_cx(&cx, "alpha")
            .await
            .unwrap();
        assert!(after.is_none(), "alpha should be gone after delete");

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 137 saved-search cluster —
/// 6 new storage cx-first siblings exercised end-to-end:
/// `insert_saved_search_with_cx`, `update_saved_search_run_with_cx`,
/// `update_saved_search_schedule_with_cx`,
/// `delete_saved_search_with_cx`, `get_saved_search_by_name_with_cx`,
/// `list_saved_searches_with_cx`.
#[test]
fn storage_tick137_saved_search_cluster_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick137_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // 1. insert_saved_search_with_cx
        let record = SavedSearchRecord {
            id: "ss-tick137".to_string(),
            name: "tick137-errors".to_string(),
            query: "error OR panic".to_string(),
            pane_id: None,
            limit: 50,
            since_mode: SAVED_SEARCH_SINCE_MODE_LAST_RUN.to_string(),
            since_ms: None,
            schedule_interval_ms: None,
            enabled: false,
            last_run_at: None,
            last_result_count: None,
            last_error: None,
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
        };
        storage
            .insert_saved_search_with_cx(&cx, record)
            .await
            .unwrap();

        // 2. get_saved_search_by_name_with_cx
        let fetched = storage
            .get_saved_search_by_name_with_cx(&cx, "tick137-errors")
            .await
            .unwrap()
            .expect("saved search should exist");
        assert_eq!(fetched.id, "ss-tick137");
        assert_eq!(fetched.query, "error OR panic");

        // 3. list_saved_searches_with_cx
        let listed = storage.list_saved_searches_with_cx(&cx).await.unwrap();
        assert!(
            listed.iter().any(|s| s.id == "ss-tick137"),
            "list_saved_searches_with_cx should include ss-tick137"
        );

        // 4. update_saved_search_run_with_cx
        storage
            .update_saved_search_run_with_cx(&cx, "ss-tick137", 1_700_000_001_000, Some(12), None)
            .await
            .unwrap();
        let after_run = storage
            .get_saved_search_by_name_with_cx(&cx, "tick137-errors")
            .await
            .unwrap()
            .expect("saved search should still exist after run update");
        assert_eq!(after_run.last_run_at, Some(1_700_000_001_000));
        assert_eq!(after_run.last_result_count, Some(12));

        // 5. update_saved_search_schedule_with_cx
        storage
            .update_saved_search_schedule_with_cx(&cx, "ss-tick137", true, Some(60_000))
            .await
            .unwrap();
        let after_sched = storage
            .get_saved_search_by_name_with_cx(&cx, "tick137-errors")
            .await
            .unwrap()
            .expect("saved search should still exist after schedule update");
        assert!(after_sched.enabled);
        assert_eq!(after_sched.schedule_interval_ms, Some(60_000));

        // 6. delete_saved_search_with_cx
        let deleted = storage
            .delete_saved_search_with_cx(&cx, "tick137-errors")
            .await
            .unwrap();
        assert_eq!(
            deleted, 1,
            "delete_saved_search_with_cx should remove one row"
        );
        let after_delete = storage
            .get_saved_search_by_name_with_cx(&cx, "tick137-errors")
            .await
            .unwrap();
        assert!(
            after_delete.is_none(),
            "saved search should be gone after delete_saved_search_with_cx"
        );

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 121 hot-path batch smoke test —
/// 10 more storage cx-first siblings exercised end-to-end on a
/// fresh DB with pane 1 seeded for FK constraints.
#[test]
fn storage_tick121_hot_path_siblings_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick121_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed pane 1 for FK constraints.
        storage
            .upsert_pane_with_cx(
                &cx,
                PaneRecord {
                    pane_id: 1,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: Some("tick121".to_string()),
                    cwd: None,
                    tty_name: None,
                    first_seen_at: 1_700_000_000_000,
                    last_seen_at: 1_700_000_000_000,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                },
            )
            .await
            .unwrap();

        let ts = 1_700_000_000_000_i64;
        let future_ts = 1_800_000_000_000_i64;

        // 1. mark_action_undone_with_cx — false (no such audit action).
        let undone = storage
            .mark_action_undone_with_cx(&cx, 999_999, "test")
            .await
            .unwrap();
        assert!(!undone);

        // 2. record_usage_metrics_batch_with_cx — empty batch, returns 0.
        let batch_count = storage
            .record_usage_metrics_batch_with_cx(&cx, vec![])
            .await
            .unwrap();
        assert_eq!(batch_count, 0);

        // 3. purge_usage_metrics_with_cx — 0 on empty DB.
        let purged = storage
            .purge_usage_metrics_with_cx(&cx, future_ts)
            .await
            .unwrap();
        assert_eq!(purged, 0);

        // 4. purge_notification_history_with_cx — 0 on empty DB.
        let nh_purged = storage
            .purge_notification_history_with_cx(&cx, future_ts)
            .await
            .unwrap();
        assert_eq!(nh_purged, 0);

        // 5. upsert_action_plan_with_cx — seed workflow first (FK).
        storage
            .upsert_workflow_with_cx(
                &cx,
                WorkflowRecord {
                    id: "wf-tick121".to_string(),
                    workflow_name: "demo".to_string(),
                    pane_id: 1,
                    trigger_event_id: None,
                    current_step: 0,
                    status: "running".to_string(),
                    wait_condition: None,
                    context: None,
                    result: None,
                    error: None,
                    started_at: 1_700_000_000_000,
                    updated_at: 1_700_000_000_000,
                    completed_at: None,
                },
            )
            .await
            .unwrap();
        let plan = crate::plan::ActionPlan::builder("tick121", "ws-tick121").build();
        storage
            .upsert_action_plan_with_cx(&cx, "wf-tick121", &plan)
            .await
            .unwrap();

        // 6. get_sessions_for_pane_with_cx — empty on fresh DB.
        let sessions = storage.get_sessions_for_pane_with_cx(&cx, 1).await.unwrap();
        assert!(sessions.is_empty());

        // 7. get_max_seq_with_cx — None on fresh DB (no segments).
        let max_seq = storage.get_max_seq_with_cx(&cx, 1).await.unwrap();
        assert!(max_seq.is_none());

        // 8. get_active_reservation_with_cx — None on fresh DB.
        let rsv = storage
            .get_active_reservation_with_cx(&cx, 1)
            .await
            .unwrap();
        assert!(rsv.is_none());

        // 9. list_active_reservations_with_cx — empty on fresh DB.
        let rsvs = storage.list_active_reservations_with_cx(&cx).await.unwrap();
        assert!(rsvs.is_empty());

        // 10. export_reservations_with_cx — empty on fresh DB.
        let exp_rsvs = storage
            .export_reservations_with_cx(&cx, ExportQuery::default())
            .await
            .unwrap();
        assert!(exp_rsvs.is_empty());

        let _ = ts;

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 120 hot-path batch smoke test —
/// 8 more storage cx-first siblings exercised end-to-end.
#[test]
fn storage_tick120_hot_path_siblings_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick120_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed pane 1 for FK constraints on agent_session.
        storage
            .upsert_pane_with_cx(
                &cx,
                PaneRecord {
                    pane_id: 1,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: Some("tick120".to_string()),
                    cwd: None,
                    tty_name: None,
                    first_seen_at: 1_700_000_000_000,
                    last_seen_at: 1_700_000_000_000,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                },
            )
            .await
            .unwrap();

        // 1. get_event_annotations_with_cx — None on fresh DB.
        let ann = storage
            .get_event_annotations_with_cx(&cx, 9999)
            .await
            .unwrap();
        assert!(ann.is_none());

        // 2. remove_event_mute_with_cx — false (no such mute).
        let removed = storage
            .remove_event_mute_with_cx(&cx, "no-such-key")
            .await
            .unwrap();
        assert!(!removed);

        // 3. prune_segments_before_with_cx — 0 on empty DB.
        let pruned = storage
            .prune_segments_before_with_cx(&cx, 1_800_000_000_000)
            .await
            .unwrap();
        assert_eq!(pruned, 0);

        // 4. query_usage_metrics_with_cx — empty on fresh DB.
        let metrics = storage
            .query_usage_metrics_with_cx(&cx, MetricQuery::default())
            .await
            .unwrap();
        assert!(metrics.is_empty());

        // 5. scan_segments_with_cx — empty on fresh DB.
        let segments = storage
            .scan_segments_with_cx(&cx, SegmentScanQuery::default())
            .await
            .unwrap();
        assert!(segments.is_empty());

        let seg_a = storage
            .append_segment_with_cx(&cx, 1, "tick120-scan-a", None)
            .await
            .unwrap();
        let seg_b = storage
            .append_segment_with_cx(&cx, 1, "tick120-scan-b", None)
            .await
            .unwrap();
        let scanned = storage
            .scan_segments_with_cx(
                &cx,
                SegmentScanQuery {
                    after_id: Some(0),
                    pane_id: Some(1),
                    since: None,
                    until: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(scanned.len(), 2);
        assert_eq!(scanned[0].id, seg_a.id);
        assert_eq!(scanned[1].id, seg_b.id);

        let after_first = storage
            .scan_segments_with_cx(
                &cx,
                SegmentScanQuery {
                    after_id: Some(seg_a.id),
                    pane_id: Some(1),
                    since: None,
                    until: None,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].id, seg_b.id);

        // 6. get_accounts_by_service_with_cx — empty on fresh DB.
        let accts = storage
            .get_accounts_by_service_with_cx(&cx, "codex")
            .await
            .unwrap();
        assert!(accts.is_empty());

        // 7. export_gaps_with_cx — empty on fresh DB.
        let gaps = storage
            .export_gaps_with_cx(&cx, ExportQuery::default())
            .await
            .unwrap();
        assert!(gaps.is_empty());

        // 8. upsert_agent_session_with_cx — verify id > 0.
        let session = AgentSessionRecord {
            id: 0,
            pane_id: 1,
            agent_type: "codex".to_string(),
            session_id: Some("sess-tick120".to_string()),
            external_id: None,
            external_meta: None,
            started_at: 1_700_000_000_000,
            ended_at: None,
            end_reason: None,
            total_tokens: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
            model_name: None,
            estimated_cost_usd: None,
        };
        let session_id = storage
            .upsert_agent_session_with_cx(&cx, session)
            .await
            .unwrap();
        assert!(session_id > 0);

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 119 hot-path batch smoke test —
/// 7 more storage cx-first siblings exercised end-to-end:
/// count/list/undo reads and a purge/usage-metric write.
#[test]
fn storage_tick119_hot_path_siblings_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick119_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        let ts = 1_700_000_000_000_i64;
        let future_ts = 1_800_000_000_000_i64;

        // 1. list_active_mutes_with_cx — empty on fresh DB.
        let mutes = storage.list_active_mutes_with_cx(&cx, ts).await.unwrap();
        assert!(mutes.is_empty());

        // 2. count_segments_before_with_cx — 0 on empty DB.
        let segs = storage
            .count_segments_before_with_cx(&cx, future_ts)
            .await
            .unwrap();
        assert_eq!(segs, 0);

        // 3. count_audit_actions_before_with_cx — 0.
        let audits = storage
            .count_audit_actions_before_with_cx(&cx, future_ts)
            .await
            .unwrap();
        assert_eq!(audits, 0);

        // 4. count_usage_metrics_before_with_cx — 0.
        let usage = storage
            .count_usage_metrics_before_with_cx(&cx, future_ts)
            .await
            .unwrap();
        assert_eq!(usage, 0);

        // 5. get_action_undo_with_cx — None for nonexistent.
        let undo = storage.get_action_undo_with_cx(&cx, 999_999).await.unwrap();
        assert!(undo.is_none());

        // 6. record_usage_metric_with_cx — insert + verify id > 0.
        let metric = UsageMetricRecord {
            id: 0,
            timestamp: ts,
            metric_type: MetricType::ApiCall,
            pane_id: None,
            agent_type: None,
            account_id: None,
            workflow_id: None,
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: None,
            created_at: ts,
        };
        let metric_id = storage
            .record_usage_metric_with_cx(&cx, metric)
            .await
            .unwrap();
        assert!(metric_id > 0);

        // 7. purge_audit_actions_before_with_cx — runs cleanly on
        // a DB with no audit actions, returns 0.
        let purged = storage
            .purge_audit_actions_before_with_cx(&cx, future_ts)
            .await
            .unwrap();
        assert_eq!(purged, 0);

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 118 hot-path batch smoke test —
/// 6 more storage cx-first siblings exercised end-to-end:
/// `append_segment_with_cx`, `record_gap_with_cx`,
/// `get_panes_with_cx`, `get_workflow_with_cx`,
/// `find_incomplete_workflows_with_cx`, `export_workflows_with_cx`.
#[test]
fn storage_tick118_hot_path_siblings_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick118_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed two panes — verify get_panes_with_cx returns both.
        for id in [11_u64, 22_u64] {
            storage
                .upsert_pane_with_cx(
                    &cx,
                    PaneRecord {
                        pane_id: id,
                        pane_uuid: None,
                        domain: "local".to_string(),
                        window_id: None,
                        tab_id: None,
                        title: Some(format!("tick118-{id}")),
                        cwd: None,
                        tty_name: None,
                        first_seen_at: 1_700_000_000_000,
                        last_seen_at: 1_700_000_000_000,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    },
                )
                .await
                .unwrap();
        }

        // 1. get_panes_with_cx
        let panes = storage.get_panes_with_cx(&cx).await.unwrap();
        assert_eq!(panes.len(), 2);

        // 2. append_segment_with_cx (hot write path)
        let segment = storage
            .append_segment_with_cx(
                &cx,
                11,
                "tick118-content.",
                Some("hash-tick118-a".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(segment.pane_id, 11);
        assert_eq!(segment.seq, 0);
        assert_eq!(segment.content_hash.as_deref(), Some("hash-tick118-a"));
        let second_segment = storage
            .append_segment_with_cx(&cx, 11, "tick118-content-2", None)
            .await
            .unwrap();
        assert_eq!(second_segment.pane_id, 11);
        assert_eq!(second_segment.seq, 1);
        assert!(second_segment.content_hash.is_none());

        // 3. record_gap_with_cx
        let gap = storage
            .record_gap_with_cx(&cx, 11, "tick118-gap-reason")
            .await
            .unwrap();
        // gap may be None if no prior segment sequence anomaly; just
        // asserting the call roundtrips cleanly is sufficient here.
        let _ = gap;

        // 4. upsert_workflow_with_cx + get_workflow_with_cx roundtrip
        let workflow = WorkflowRecord {
            id: "wf-tick118".to_string(),
            workflow_name: "demo".to_string(),
            pane_id: 11,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            completed_at: None,
        };
        storage
            .upsert_workflow_with_cx(&cx, workflow)
            .await
            .unwrap();
        let fetched = storage
            .get_workflow_with_cx(&cx, "wf-tick118")
            .await
            .unwrap()
            .expect("workflow should exist");
        assert_eq!(fetched.id, "wf-tick118");

        // 5. find_incomplete_workflows_with_cx — our running workflow
        // above is incomplete.
        let incomplete = storage
            .find_incomplete_workflows_with_cx(&cx)
            .await
            .unwrap();
        assert!(
            incomplete.iter().any(|w| w.id == "wf-tick118"),
            "find_incomplete_workflows_with_cx should include wf-tick118"
        );

        // 6. export_workflows_with_cx
        let exported = storage
            .export_workflows_with_cx(&cx, ExportQuery::default())
            .await
            .unwrap();
        assert!(
            exported.iter().any(|w| w.id == "wf-tick118"),
            "export_workflows_with_cx should include wf-tick118"
        );

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 117 hot-path batch smoke test —
/// 6 more storage cx-first siblings exercised end-to-end:
/// `count_events_before_with_cx`, `get_pane_with_cx`,
/// `insert_approval_token_with_cx`,
/// `record_audit_action_redacted_with_cx`,
/// `is_event_muted_with_cx`, `get_events_with_cx`.
#[test]
fn storage_tick117_hot_path_siblings_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick117_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed a pane (for get_pane + FK constraints).
        let pane = PaneRecord {
            pane_id: 7,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick117".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // 1. get_pane_with_cx
        let fetched = storage
            .get_pane_with_cx(&cx, 7)
            .await
            .unwrap()
            .expect("pane 7 should exist");
        assert_eq!(fetched.pane_id, 7);
        let blocking_fetched = storage
            .get_pane_blocking(7)
            .unwrap()
            .expect("pane 7 should exist through blocking path");
        assert_eq!(blocking_fetched.pane_id, 7);

        // 2. get_events_with_cx (empty on fresh DB)
        let events = storage
            .get_events_with_cx(&cx, EventQuery::default())
            .await
            .unwrap();
        assert!(events.is_empty());

        // 3. count_events_before_with_cx
        let count = storage
            .count_events_before_with_cx(&cx, 1_800_000_000_000)
            .await
            .unwrap();
        assert_eq!(count, 0);

        // 4. is_event_muted_with_cx
        let muted = storage
            .is_event_muted_with_cx(&cx, "no-such-key", 1_700_000_000_000)
            .await
            .unwrap();
        assert!(!muted);

        // 5. record_audit_action_redacted_with_cx
        let action = AuditActionRecord {
            id: 0,
            ts: 1_700_000_000_000,
            actor_kind: "human".to_string(),
            actor_id: Some("tick117".to_string()),
            correlation_id: None,
            pane_id: Some(7),
            domain: None,
            action_kind: "test_action".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("raw reason".to_string()),
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };
        let audit_id = storage
            .record_audit_action_redacted_with_cx(&cx, action)
            .await
            .unwrap();
        assert!(audit_id > 0);

        // 6. insert_approval_token_with_cx
        let token = ApprovalTokenRecord {
            id: 0,
            code_hash: "tok-tick117-hash".to_string(),
            created_at: 1_700_000_000_000,
            expires_at: 1_800_000_000_000,
            used_at: None,
            workspace_id: "ws-tick117".to_string(),
            action_kind: "test_action".to_string(),
            pane_id: Some(7),
            action_fingerprint: "fp-tick117".to_string(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };
        let approval_id = storage
            .insert_approval_token_with_cx(&cx, token)
            .await
            .unwrap();
        assert!(approval_id > 0);

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: tick 116 hot-path batch smoke test —
/// `upsert_workflow_with_cx`, `record_audit_action_with_cx`,
/// `get_audit_actions_with_cx`, and `get_step_logs_with_cx`
/// must each round-trip cleanly with a fresh cx.
#[test]
fn storage_tick116_hot_path_siblings_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_tick116_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        // Seed a pane for FK constraints.
        let pane = PaneRecord {
            pane_id: 42,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("tick116".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane_with_cx(&cx, pane).await.unwrap();

        // 1. upsert_workflow_with_cx
        let workflow = WorkflowRecord {
            id: "wf-tick116".to_string(),
            workflow_name: "demo".to_string(),
            pane_id: 42,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            completed_at: None,
        };
        storage
            .upsert_workflow_with_cx(&cx, workflow)
            .await
            .unwrap();

        // 2. record_audit_action_with_cx
        let action = AuditActionRecord {
            id: 0,
            ts: 1_700_000_000_000,
            actor_kind: "human".to_string(),
            actor_id: Some("tick116".to_string()),
            correlation_id: None,
            pane_id: Some(42),
            domain: None,
            action_kind: "test_action".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };
        let audit_id = storage
            .record_audit_action_with_cx(&cx, action)
            .await
            .unwrap();
        assert!(audit_id > 0);

        // 3. get_audit_actions_with_cx
        let audits = storage
            .get_audit_actions_with_cx(&cx, AuditQuery::default())
            .await
            .unwrap();
        assert!(
            audits.iter().any(|a| a.id == audit_id),
            "audit action should be queryable via cx-first path"
        );

        // 4. get_step_logs_with_cx (empty result is fine — just exercise the path)
        let steps = storage
            .get_step_logs_with_cx(&cx, "wf-tick116")
            .await
            .unwrap();
        assert!(steps.is_empty(), "fresh workflow has no step logs");

        storage.shutdown_with_cx(&cx).await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

/// ft-xbnl0.2.3 Cx-first: `shutdown_with_cx` must complete the
/// full shutdown (including writer thread join) when given a
/// fresh cx — identical to the legacy `shutdown` path.
#[test]
fn storage_shutdown_with_cx_fresh_cx_full_shutdown() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_shutdown_cx_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();
        let cx = crate::cx::for_testing();
        let storage = StorageHandle::new_with_cx(&cx, &db_path_str).await.unwrap();

        storage.shutdown_with_cx(&cx).await.unwrap();

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

#[test]
fn storage_handle_graceful_shutdown() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_shutdown_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        // Create storage handle
        let storage = StorageHandle::new(&db_path_str).await.unwrap();

        // Upsert a pane to verify it works
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        // Graceful shutdown
        storage.shutdown().await.unwrap();

        // Cleanup
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

#[test]
fn storage_handle_insert_step_log_and_query() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_steplog_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();

        // Create pane
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        // Create workflow
        let workflow = WorkflowRecord {
            id: "wf-async-001".to_string(),
            workflow_name: "async_test".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            completed_at: None,
        };
        storage.upsert_workflow(workflow).await.unwrap();

        // Insert step log via async API
        storage
            .insert_step_log(
                "wf-async-001",
                None,
                0,
                "async_step",
                None, // step_id
                None, // step_kind
                "continue",
                Some(r#"{"async": true}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                1_700_000_000_000,
                1_700_000_000_050,
            )
            .await
            .unwrap();

        // Query step logs via async API
        let logs = storage.get_step_logs("wf-async-001").await.unwrap();

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].step_name, "async_step");
        assert_eq!(logs[0].duration_ms, 50);

        storage.shutdown().await.unwrap();

        // Cleanup
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

#[test]
fn storage_handle_action_plan_roundtrip() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_plan_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();

        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        let workflow = WorkflowRecord {
            id: "wf-plan-async-001".to_string(),
            workflow_name: "async_plan_test".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            completed_at: None,
        };
        storage.upsert_workflow(workflow).await.unwrap();

        let plan = crate::plan::ActionPlan::builder("Async Plan", "workspace-async")
            .add_step(crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::SendText {
                    pane_id: 1,
                    text: "/compact".to_string(),
                    paste_mode: None,
                },
                "Send compact",
            ))
            .build();

        storage
            .upsert_action_plan("wf-plan-async-001", &plan)
            .await
            .unwrap();

        let fetched = storage
            .get_action_plan("wf-plan-async-001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.plan_id, plan.plan_id.to_string());

        storage.shutdown().await.unwrap();

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

#[test]
fn storage_handle_records_audit_action_redacted() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_audit_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        let storage = StorageHandle::new(&db_path_str).await.unwrap();

        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        let action = AuditActionRecord {
            id: 0,
            ts: 1_700_000_000_000,
            actor_kind: "robot".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some(
                "API key sk-abc123456789012345678901234567890123456789012345678901".to_string(),
            ),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };

        storage.record_audit_action_redacted(action).await.unwrap();

        let query = AuditQuery {
            pane_id: Some(1),
            limit: Some(10),
            ..Default::default()
        };
        let rows = storage.get_audit_actions(query).await.unwrap();
        assert_eq!(rows.len(), 1);

        let input = rows[0].input_summary.as_ref().unwrap();
        assert!(input.contains("[REDACTED]"));
        assert!(!input.contains("sk-abc"));
        let redactor = Redactor::new();
        for field in [
            rows[0].decision_reason.as_deref(),
            rows[0].input_summary.as_deref(),
            rows[0].verification_summary.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(!field.contains("sk-abc"));
            assert!(!redactor.contains_secrets(field));
        }

        storage.shutdown().await.unwrap();

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

#[test]
fn storage_handle_writer_queue_processes_all() {
    run_storage_async_test(async {
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_queue_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        // Create storage with small queue
        let config = StorageConfig {
            write_queue_size: 4,
            defer_fts_triggers: false,
        };
        let storage = StorageHandle::with_config(&db_path_str, config)
            .await
            .unwrap();

        // Create pane first
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        // Send many segment appends sequentially
        for i in 0..10 {
            let content = format!("segment content {i}");
            storage.append_segment(1, &content, None).await.unwrap();
        }

        // All appends should succeed
        let segments = storage.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 10, "All 10 segments should be stored");

        storage.shutdown().await.unwrap();

        // Cleanup
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
    });
}

#[test]
fn mmap_segment_line_round_trip_preserves_multiline_content() {
    let segment = Segment {
        id: 11,
        pane_id: 7,
        seq: 3,
        content: "line 1\nline 2\r\nline 3".to_string(),
        content_len: "line 1\nline 2\r\nline 3".len(),
        content_hash: Some("abc123".to_string()),
        captured_at: 1_700_000_123_456,
    };

    let encoded = encode_mmap_segment_line(&segment).expect("encode mmap segment");
    assert!(
        !encoded.contains('\n'),
        "encoded mmap line must be single-line json"
    );

    let decoded = decode_mmap_segment_line(&encoded).expect("decode mmap segment");
    assert_eq!(decoded.id, segment.id);
    assert_eq!(decoded.pane_id, segment.pane_id);
    assert_eq!(decoded.seq, segment.seq);
    assert_eq!(decoded.content, segment.content);
    assert_eq!(decoded.content_len, segment.content_len);
    assert_eq!(decoded.content_hash, segment.content_hash);
    assert_eq!(decoded.captured_at, segment.captured_at);
}

#[test]
fn get_segments_prefers_mmap_lane_and_falls_back_to_sqlite_on_decode_error() {
    run_storage_async_test(async {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("mmap_lane_storage.db");
        let db_path_str = db_path.to_string_lossy().to_string();

        let mut handle = StorageHandle::new(&db_path_str)
            .await
            .expect("create handle");
        handle
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("pane-1".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 1_700_000_000_000,
                last_seen_at: 1_700_000_000_000,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .expect("upsert pane");

        handle
            .append_segment(1, "alpha\nwith newline", None)
            .await
            .expect("append alpha");
        handle
            .append_segment(1, "beta", None)
            .await
            .expect("append beta");
        let sqlite_segments = handle
            .get_segments(1, 10)
            .await
            .expect("query sqlite segments");
        assert_eq!(sqlite_segments.len(), 2);

        let mmap_dir = temp_dir.path().join("segment_mmap_lane");
        let mut mmap_store = mmap_store::MmapScrollbackStore::new(
            mmap_store::MmapStoreConfig::new(mmap_dir.clone()),
        )
        .expect("create mmap store");
        for segment in sqlite_segments.iter().rev() {
            let line = encode_mmap_segment_line(segment).expect("encode mirror line");
            mmap_store
                .append_line(segment.pane_id, &line)
                .expect("append mirror line");
        }

        handle.mmap_mirror_dir = Some(Arc::new(mmap_dir.clone()));
        let mmap_segments = handle
            .get_segments(1, 10)
            .await
            .expect("query mmap segments");
        assert_eq!(mmap_segments.len(), sqlite_segments.len());
        for (got, expected) in mmap_segments.iter().zip(&sqlite_segments) {
            assert_eq!(got.id, expected.id);
            assert_eq!(got.pane_id, expected.pane_id);
            assert_eq!(got.seq, expected.seq);
            assert_eq!(got.content, expected.content);
            assert_eq!(got.content_len, expected.content_len);
            assert_eq!(got.content_hash, expected.content_hash);
            assert_eq!(got.captured_at, expected.captured_at);
        }

        let mut corrupted_log = std::fs::OpenOptions::new()
            .append(true)
            .open(mmap_dir.join("1.log"))
            .expect("open mmap log for corruption");
        corrupted_log
            .write_all(b"{this-is-not-json}\n")
            .expect("append invalid json line");

        let fallback_segments = handle
            .get_segments(1, 10)
            .await
            .expect("fallback query should use sqlite");
        assert_eq!(fallback_segments.len(), sqlite_segments.len());
        for (got, expected) in fallback_segments.iter().zip(&sqlite_segments) {
            assert_eq!(got.id, expected.id);
            assert_eq!(got.pane_id, expected.pane_id);
            assert_eq!(got.seq, expected.seq);
            assert_eq!(got.content, expected.content);
            assert_eq!(got.content_len, expected.content_len);
            assert_eq!(got.content_hash, expected.content_hash);
            assert_eq!(got.captured_at, expected.captured_at);
        }

        handle.shutdown().await.expect("shutdown handle");
    });
}

}

#[cfg(test)]
use fts_async_flat_tests::{run_storage_async_test, run_storage_proptest_async};

// =============================================================================
// br-ft-rvt1z: PooledReadConn telemetry tests (ft-q4udk follow-up)
// =============================================================================

#[cfg(test)]
mod pool_telemetry_tests {
    use super::{
        POOL_HITS, POOL_LOCK_POISONED, POOL_MISSES, POOL_RETURNS, PoolTelemetrySnapshot,
        PooledReadConn, pool_telemetry_snapshot, pooled_backend,
    };
    use crate::storage_backend_trait::{StorageBackend, ToSqlValue};
    use std::sync::atomic::Ordering;

    /// Reset the process-global counters so a single test owns the
    /// hit/miss arithmetic. The pool itself is process-global; sibling
    /// tests in the same crate that incidentally acquire pooled conns
    /// will perturb the deltas. This helper takes a baseline snapshot
    /// and computes deltas relative to it instead — that way two
    /// pool-telemetry tests running in parallel on the same process
    /// don't race.
    fn baseline() -> PoolTelemetrySnapshot {
        PoolTelemetrySnapshot {
            hits: POOL_HITS.load(Ordering::Relaxed),
            misses: POOL_MISSES.load(Ordering::Relaxed),
            returns: POOL_RETURNS.load(Ordering::Relaxed),
            // br-ft-ac4j0: include pool_lock_poisoned in baseline.
            pool_lock_poisoned: POOL_LOCK_POISONED.load(Ordering::Relaxed),
        }
    }

    fn delta(start: PoolTelemetrySnapshot, end: PoolTelemetrySnapshot) -> PoolTelemetrySnapshot {
        PoolTelemetrySnapshot {
            hits: end.hits.saturating_sub(start.hits),
            misses: end.misses.saturating_sub(start.misses),
            returns: end.returns.saturating_sub(start.returns),
            // br-ft-ac4j0.
            pool_lock_poisoned: end
                .pool_lock_poisoned
                .saturating_sub(start.pool_lock_poisoned),
        }
    }

    #[test]
    fn pool_snapshot_returns_relaxed_load() {
        // Smoke: snapshot is callable + the field accessors do
        // saturating math correctly.
        let snap = pool_telemetry_snapshot();
        let _ = snap.hit_rate();
        assert_eq!(snap.total_acquires(), snap.hits + snap.misses);
    }

    #[test]
    fn snapshot_hit_rate_returns_zero_when_no_acquires() {
        let s = PoolTelemetrySnapshot {
            hits: 0,
            misses: 0,
            returns: 0,
            // br-ft-ac4j0.
            pool_lock_poisoned: 0,
        };
        assert_eq!(s.hit_rate(), 0.0);
    }

    #[test]
    fn snapshot_hit_rate_computed_correctly() {
        let s = PoolTelemetrySnapshot {
            hits: 80,
            misses: 20,
            returns: 80,
            // br-ft-ac4j0.
            pool_lock_poisoned: 0,
        };
        assert!((s.hit_rate() - 0.80).abs() < 1e-9);
    }

    /// br-ft-ac4j0: a clean acquire/release cycle does not bump
    /// pool_lock_poisoned. Without this assertion the metric would
    /// be useless because every database read would inflate it.
    #[test]
    fn pool_lock_poisoned_unchanged_for_clean_acquire_release() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft-ac4j0-clean.db");
        let path_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::File::create(&db_path).unwrap();

        let before = baseline().pool_lock_poisoned;
        // Even if acquire fails (uninitialized db), the .lock() itself
        // succeeds; this exercises the lock site without poisoning.
        let _ = PooledReadConn::acquire(&path_str);
        let after = baseline().pool_lock_poisoned;

        assert_eq!(
            after, before,
            "br-ft-ac4j0: clean acquire must NOT bump pool_lock_poisoned"
        );
    }

    #[test]
    fn pool_acquire_release_cycle_records_hits() {
        // br-ft-rvt1z acceptance: N sequential acquires against the
        // same db_path should yield 1 miss + (N-1) hits, since the
        // first acquire opens fresh + every subsequent acquire pops
        // the connection the prior Drop returned to the LIFO.
        //
        // This pins the regression: if a future migration silently
        // bypasses PooledReadConn::acquire (the br-ft-l1jgo defect),
        // the hit-rate will collapse — the test asserts > 80%.
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("pool_telemetry.db");
        let db_path_str = db_path.to_string_lossy().to_string();

        // Touch the file by opening + closing once (so subsequent
        // open_read_storage_conn calls succeed against a present file).
        let _seed = rusqlite::Connection::open(&db_path).expect("seed open");
        drop(_seed);

        let start = baseline();
        const N: u64 = 20;
        for _ in 0..N {
            let conn = PooledReadConn::acquire(&db_path_str).expect("acquire");
            // Use the conn so it's not dead-stripped by an over-eager
            // optimizer + so we exercise the Deref<Target = Connection>.
            let _ = conn.is_autocommit();
            // Drop returns the conn to the pool.
        }
        let end = baseline();
        let d = delta(start, end);

        assert_eq!(
            d.hits + d.misses,
            N,
            "every acquire must increment hits or misses (got {} hits + {} misses)",
            d.hits,
            d.misses,
        );
        assert!(
            d.misses >= 1,
            "first acquire for a fresh db_path must miss (got {} misses)",
            d.misses,
        );
        // The first acquire is necessarily a miss; every subsequent
        // acquire should hit because the Drop returned the conn to
        // the LIFO. Allow up to 2 misses to absorb the rare case
        // where another pool consumer in the same process drained
        // our slot mid-loop.
        assert!(
            d.misses <= 2,
            "sequential reuse should produce at most 2 misses (got {} misses)",
            d.misses,
        );
        // br-ft-rvt1z hit-rate floor: > 80%.
        let local_hit_rate = d.hits as f64 / (d.hits + d.misses) as f64;
        assert!(
            local_hit_rate > 0.80,
            "br-ft-rvt1z acceptance: hit rate must exceed 80% on \
             sequential same-db acquires (got {:.2}%, hits={} misses={})",
            local_hit_rate * 100.0,
            d.hits,
            d.misses,
        );
        // Returns count must be at most acquires (each PooledReadConn
        // returns its conn at most once on Drop).
        assert!(
            d.returns <= N,
            "returns ({}) must be <= acquires ({}) — Drop counts at most once per acquire",
            d.returns,
            N,
        );
    }

    #[test]
    fn pooled_backend_lends_trait_object() {
        fn require_trait_object(_: &dyn StorageBackend) {}

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("pooled_backend_trait.db");
        let db_path_str = db_path.to_string_lossy().to_string();

        let seed = rusqlite::Connection::open(&db_path).expect("seed open");
        seed.execute_batch(
            "CREATE TABLE sample (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO sample (id, name) VALUES (1, 'trait-object');",
        )
        .expect("seed table");
        drop(seed);

        let name = pooled_backend(&db_path_str, |backend| {
            require_trait_object(backend);
            let row = backend
                .query_row_typed(
                    "SELECT name FROM sample WHERE id = ?1",
                    &[ToSqlValue::Integer(1)],
                )
                .map_err(|err| super::storage_backend_error("pooled backend trait test", err))?
                .expect("seed row");
            Ok(row
                .first()
                .cloned()
                .expect("seed query returned one column"))
        })
        .expect("pooled trait backend query");

        assert_eq!(name, "trait-object");
    }
}

// =============================================================================
// Database Check & Repair Tests (wa-ubb)
// =============================================================================

#[cfg(test)]
mod db_check_repair_tests;

// =============================================================================
// Incremental FTS Sync Tests (wa-3g9.4)
// =============================================================================

#[cfg(test)]
mod fts_sync_tests;

// =============================================================================
// Timeline Data Model Tests (wa-6sk.1)
// =============================================================================

#[cfg(test)]
mod timeline_tests;

// =============================================================================
// Async StorageHandle Tests (wa-4vx.3.7)
// =============================================================================

#[cfg(test)]
mod storage_handle_tests;

// =============================================================================
// Queue Depth Instrumentation Tests (wa-upg.12.2)
// =============================================================================

#[cfg(test)]
mod queue_depth_tests;

// =============================================================================
// Backpressure Integration Tests (wa-upg.12.5)
// =============================================================================

#[cfg(test)]
mod backpressure_integration_tests;

// =============================================================================
// Property-Based Tests (wa-4vx.10.5)
// =============================================================================

#[cfg(test)]
mod proptest_tests;

// =============================================================================
// Accounts DB Mirror Tests (wa-nu4.1.5.3)
// =============================================================================

#[cfg(test)]
mod accounts_db_tests;

#[cfg(test)]
mod reservation_tests;

// =============================================================================
// Timeline and Correlation Detection Tests (wa-6sk.2)
// =============================================================================

#[cfg(test)]
mod timeline_correlation_tests;

// =============================================================================
// Timeline Integration Tests (wa-6sk.5)
// =============================================================================

#[cfg(test)]
mod timeline_integration_tests;

// =============================================================================
// br-ft-dngp2: agent_profiles async wrapper integration tests
// =============================================================================

#[cfg(test)]
mod agent_profiles_handle_tests;
