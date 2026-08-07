//! Session restore engine — detect and recover from unclean shutdowns.
//!
//! This library module can detect sessions that did not shut down cleanly
//! (`shutdown_clean = 0`) and load their latest checkpoint. The production
//! `ft watch` startup path does not currently call that detector. An explicit
//! restore reconstructs the mux topology via
//! [`LayoutRestorer`] without writing banners or historical output through PTY
//! input APIs.
//!
//! # Data flow
//!
//! ```text
//! Database → SessionCandidate → RestoreDecision → LayoutRestorer → RestoreSummary
//! ```

use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::checkpoint_witness::{
    CHECKPOINT_ROLE_RESTORE_INTENT, CHECKPOINT_ROLE_RESTORE_RECEIPT, CHECKPOINT_ROLE_SNAPSHOT,
    MAX_CHECKPOINT_METADATA_BYTES, MAX_CHECKPOINT_ROLE_BYTES,
    MAX_CHECKPOINT_SESSION_ID_BYTES, MAX_CHECKPOINT_STATE_HASH_BYTES,
    MAX_CHECKPOINT_TYPE_BYTES, MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
    MAX_PERSISTED_PANE_TEXT_BYTES, MAX_SESSION_FT_VERSION_BYTES,
    MAX_SESSION_HOST_ID_BYTES, RESTORE_INTENT_WITNESS_PREFIX,
    RESTORE_RECEIPT_WITNESS_PREFIX, SNAPSHOT_WITNESS_PREFIX, PersistedPaneState,
    canonical_json_string, checkpoint_witness, persisted_checkpoint_text_bytes,
};
use crate::restore_layout::{
    LayoutRestoreInterruptionReason, LayoutRestorer, RestoreConfig, RestoreResult,
};
use crate::restore_process::{
    LaunchInterruptionReason, LaunchReport, ProcessDispositionInput, ProcessLauncher,
};
use crate::snapshot_engine::{
    SnapshotAuthorityOperation, SnapshotAuthorityWorkFailure, SnapshotError,
    run_checkpoint_authority_sync, run_checkpoint_authority_with_cx,
};
use crate::session_pane_state::{AgentMetadata, TerminalState};
use crate::session_topology::{
    MAX_SNAPSHOT_BYTES, MAX_TOPOLOGY_PANES, TopologySnapshot,
};
use crate::wezterm::WeztermHandle;

// =============================================================================
// Error type
// =============================================================================

/// Errors during session restore.
#[derive(thiserror::Error)]
pub enum RestoreError {
    #[error("database operation failed")]
    Database(String),

    #[error("invalid persisted value for {field}: {value}")]
    InvalidPersistedValue { field: &'static str, value: i64 },

    #[error("invalid or oversized persisted text for {field}")]
    InvalidPersistedText { field: &'static str },

    #[error("invalid session selector")]
    InvalidSessionSelector,

    #[error(
        "invalid session query window (limit={limit}, offset={offset}; limit must be 1..={max_limit} and offset must be <= {max_offset})"
    )]
    InvalidQueryWindow {
        limit: usize,
        offset: usize,
        max_limit: usize,
        max_offset: usize,
    },

    #[error(
        "{resource} query has {observed} rows, exceeding the non-paged limit of {limit}; use the paged query API"
    )]
    QueryResultLimit {
        resource: &'static str,
        observed: usize,
        limit: usize,
    },

    #[error("no restorable sessions found")]
    NoSessions,

    #[error(
        "{session_count} unclean session(s) require recovery but are not automatically restorable: {reason}"
    )]
    UncleanSessionsNotRestorable {
        session_count: usize,
        reason: UncleanSessionBlockReason,
    },

    #[error("detected checkpoint {checkpoint_id} changed after restore admission")]
    DetectedCheckpointChanged { checkpoint_id: i64 },

    #[error("checkpoint data is corrupt")]
    CorruptCheckpoint(String),

    #[error(
        "checkpoint {checkpoint_id} exceeds the {resource} resource limit ({observed} > {limit})"
    )]
    CheckpointResourceLimit {
        checkpoint_id: i64,
        resource: &'static str,
        observed: usize,
        limit: usize,
    },

    #[error("topology deserialization failed")]
    TopologyParse(String),

    #[error("mux operation failed")]
    Wezterm(String),

    #[error("restore interrupted during {phase}: {reason}")]
    Interrupted {
        phase: &'static str,
        reason: RestoreInterruptionReason,
    },

    #[error("restore infrastructure failed during {phase}: {failure}")]
    InfrastructureFailure {
        phase: &'static str,
        failure: RestoreInfrastructureFailure,
    },

    #[error("restore bookkeeping failed")]
    Bookkeeping(String),

    /// Stored consistency witness does not match the exact persisted
    /// checkpoint projection. This detects corruption and partial mutation;
    /// it is not authentication against a writer that can recompute SHA-256.
    #[error("state_hash mismatch on checkpoint {checkpoint_id}")]
    StateHashMismatch {
        checkpoint_id: i64,
        session_id: String,
        stored: String,
        recomputed: String,
    },

    #[error("invalid checkpoint role on checkpoint {checkpoint_id}")]
    InvalidCheckpointRole { checkpoint_id: i64, role: String },

    #[error("checkpoint {checkpoint_id} is not a restorable session snapshot")]
    CheckpointNotRestorable { checkpoint_id: i64, role: String },

    #[error("checkpoint {checkpoint_id} has no checkpoint-local topology")]
    CheckpointTopologyUnavailable { checkpoint_id: i64 },

    #[error(
        "legacy-unverified checkpoint {checkpoint_id} requires explicit manual recovery and cannot be auto-restored"
    )]
    LegacyCheckpointRequiresManualRestore { checkpoint_id: i64 },

    #[error(
        "restore attempt {intent_checkpoint_id} is unresolved (outcome={outcome_checkpoint_id:?}); reconcile it before starting another restore"
    )]
    RestoreAttemptRequiresReconciliation {
        session_id: String,
        intent_checkpoint_id: i64,
        outcome_checkpoint_id: Option<i64>,
        status: String,
    },

    #[error(
        "restore attempt {intent_checkpoint_id} stopped during {phase}: {reason} (outcome={outcome_checkpoint_id:?}); reconcile it before retrying"
    )]
    RestoreAttemptInterrupted {
        session_id: String,
        intent_checkpoint_id: i64,
        outcome_checkpoint_id: Option<i64>,
        phase: &'static str,
        reason: RestoreInterruptionReason,
    },

    #[error("restore settled partially and requires reconciliation")]
    PartialRestore { summary: Box<RestoreSummary> },

    #[error(
        "scrollback replay is unavailable because the mux API has no safe terminal-output injection channel; retry with layout-only restore"
    )]
    SafeScrollbackReplayUnavailable,
}

/// Finite, content-free reason that a restore capability stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreInterruptionReason {
    Cancelled,
    CancellationCleanupTimedOut,
    DeadlineExceeded,
    PollQuotaExhausted,
    CostQuotaExhausted,
    ContextFailure,
    ValidationFailure,
    BackendFailure,
    MuxOutcomeIndeterminate,
    IntegrityFailure,
}

/// Finite failure of the blocking infrastructure rather than the caller's
/// capability. These errors must not be mislabeled as cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreInfrastructureFailure {
    BlockingRuntimeFailure,
    CancellationWatcherTimerFailure,
}

/// Finite reason why durable unclean state cannot currently produce an
/// automatic restore candidate. `RestoreError::UncleanSessionsNotRestorable`
/// preserves the essential clean-vs-blocked distinction without carrying
/// session identifiers or persisted content into diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncleanSessionBlockReason {
    NoSnapshotCheckpoint,
    EmptyOrMissingCheckpoint,
    LegacyUnverifiedAutoRestore,
    Mixed,
}

impl std::fmt::Display for UncleanSessionBlockReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NoSnapshotCheckpoint => "no snapshot checkpoint",
            Self::EmptyOrMissingCheckpoint => "empty or missing checkpoint data",
            Self::LegacyUnverifiedAutoRestore => {
                "legacy-unverified checkpoint requires manual recovery"
            }
            Self::Mixed => "multiple recovery blockers",
        })
    }
}

impl std::fmt::Display for RestoreInfrastructureFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::BlockingRuntimeFailure => "blocking runtime failed",
            Self::CancellationWatcherTimerFailure => {
                "blocking cancellation watcher timer failed"
            }
        })
    }
}

impl std::fmt::Display for RestoreInterruptionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Cancelled => "cancelled",
            Self::CancellationCleanupTimedOut => "cancellation cleanup timed out",
            Self::DeadlineExceeded => "deadline exceeded",
            Self::PollQuotaExhausted => "poll quota exhausted",
            Self::CostQuotaExhausted => "cost quota exhausted",
            Self::ContextFailure => "capability context failed",
            Self::ValidationFailure => "restore validation failed",
            Self::BackendFailure => "restore backend failed",
            Self::MuxOutcomeIndeterminate => "mux mutation outcome is indeterminate",
            Self::IntegrityFailure => "restore integrity check failed",
        };
        formatter.write_str(label)
    }
}

impl std::fmt::Debug for RestoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(_) => formatter.write_str("RestoreError::Database"),
            Self::InvalidPersistedValue { field, value } => formatter
                .debug_struct("RestoreError::InvalidPersistedValue")
                .field("field", field)
                .field("value", value)
                .finish(),
            Self::InvalidPersistedText { field } => formatter
                .debug_struct("RestoreError::InvalidPersistedText")
                .field("field", field)
                .finish(),
            Self::InvalidSessionSelector => {
                formatter.write_str("RestoreError::InvalidSessionSelector")
            }
            Self::InvalidQueryWindow {
                limit,
                offset,
                max_limit,
                max_offset,
            } => formatter
                .debug_struct("RestoreError::InvalidQueryWindow")
                .field("limit", limit)
                .field("offset", offset)
                .field("max_limit", max_limit)
                .field("max_offset", max_offset)
                .finish(),
            Self::QueryResultLimit {
                resource,
                observed,
                limit,
            } => formatter
                .debug_struct("RestoreError::QueryResultLimit")
                .field("resource", resource)
                .field("observed", observed)
                .field("limit", limit)
                .finish(),
            Self::NoSessions => formatter.write_str("RestoreError::NoSessions"),
            Self::UncleanSessionsNotRestorable {
                session_count,
                reason,
            } => formatter
                .debug_struct("RestoreError::UncleanSessionsNotRestorable")
                .field("session_count", session_count)
                .field("reason", reason)
                .finish(),
            Self::DetectedCheckpointChanged { checkpoint_id } => formatter
                .debug_struct("RestoreError::DetectedCheckpointChanged")
                .field("checkpoint_id", checkpoint_id)
                .finish(),
            Self::CorruptCheckpoint(_) => {
                formatter.write_str("RestoreError::CorruptCheckpoint")
            }
            Self::CheckpointResourceLimit {
                checkpoint_id,
                resource,
                observed,
                limit,
            } => formatter
                .debug_struct("RestoreError::CheckpointResourceLimit")
                .field("checkpoint_id", checkpoint_id)
                .field("resource", resource)
                .field("observed", observed)
                .field("limit", limit)
                .finish(),
            Self::TopologyParse(_) => formatter.write_str("RestoreError::TopologyParse"),
            Self::Wezterm(_) => formatter.write_str("RestoreError::MuxOperation"),
            Self::Interrupted { phase, reason } => formatter
                .debug_struct("RestoreError::Interrupted")
                .field("phase", phase)
                .field("reason", reason)
                .finish(),
            Self::InfrastructureFailure { phase, failure } => formatter
                .debug_struct("RestoreError::InfrastructureFailure")
                .field("phase", phase)
                .field("failure", failure)
                .finish(),
            Self::Bookkeeping(_) => formatter.write_str("RestoreError::Bookkeeping"),
            Self::StateHashMismatch { checkpoint_id, .. } => formatter
                .debug_struct("RestoreError::StateHashMismatch")
                .field("checkpoint_id", checkpoint_id)
                .finish(),
            Self::InvalidCheckpointRole { checkpoint_id, .. } => formatter
                .debug_struct("RestoreError::InvalidCheckpointRole")
                .field("checkpoint_id", checkpoint_id)
                .finish(),
            Self::CheckpointNotRestorable { checkpoint_id, .. } => formatter
                .debug_struct("RestoreError::CheckpointNotRestorable")
                .field("checkpoint_id", checkpoint_id)
                .finish(),
            Self::CheckpointTopologyUnavailable { checkpoint_id } => formatter
                .debug_struct("RestoreError::CheckpointTopologyUnavailable")
                .field("checkpoint_id", checkpoint_id)
                .finish(),
            Self::LegacyCheckpointRequiresManualRestore { checkpoint_id } => formatter
                .debug_struct("RestoreError::LegacyCheckpointRequiresManualRestore")
                .field("checkpoint_id", checkpoint_id)
                .finish(),
            Self::RestoreAttemptRequiresReconciliation {
                intent_checkpoint_id,
                outcome_checkpoint_id,
                ..
            } => formatter
                .debug_struct("RestoreError::RestoreAttemptRequiresReconciliation")
                .field("intent_checkpoint_id", intent_checkpoint_id)
                .field("outcome_checkpoint_id", outcome_checkpoint_id)
                .finish(),
            Self::RestoreAttemptInterrupted {
                intent_checkpoint_id,
                outcome_checkpoint_id,
                phase,
                reason,
                ..
            } => formatter
                .debug_struct("RestoreError::RestoreAttemptInterrupted")
                .field("intent_checkpoint_id", intent_checkpoint_id)
                .field("outcome_checkpoint_id", outcome_checkpoint_id)
                .field("phase", phase)
                .field("reason", reason)
                .finish(),
            Self::PartialRestore { summary } => formatter
                .debug_struct("RestoreError::PartialRestore")
                .field("checkpoint_id", &summary.checkpoint_id)
                .field("intent_checkpoint_id", &summary.intent_checkpoint_id)
                .field("outcome_checkpoint_id", &summary.outcome_checkpoint_id)
                .field(
                    "restore_authority_resolved",
                    &summary.restore_authority_resolved,
                )
                .finish(),
            Self::SafeScrollbackReplayUnavailable => {
                formatter.write_str("RestoreError::SafeScrollbackReplayUnavailable")
            }
        }
    }
}

impl RestoreError {
    /// Durable partial receipt carried by a non-successful restore outcome.
    /// Callers can render exact progress without treating the `Result` as
    /// success or losing the intent/outcome IDs needed for reconciliation.
    #[must_use]
    pub fn partial_restore_summary(&self) -> Option<&RestoreSummary> {
        match self {
            Self::PartialRestore { summary } => Some(summary.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RestoreAttemptInterruption {
    phase: &'static str,
    receipt_phase: RestoreInterruptionPhase,
    reason: RestoreInterruptionReason,
}

const RESTORE_OUTCOME_EVIDENCE_VERSION: u8 = 3;
const RESTORE_OUTCOME_REASON_EVIDENCE_VERSION: u8 = 2;

const fn legacy_restore_outcome_evidence_version() -> u8 {
    1
}

/// Finite persisted phase for an interrupted restore attempt. Aliases retain
/// readability of version-1 receipts that used human-oriented labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum RestoreInterruptionPhase {
    #[serde(rename = "layout_restoration", alias = "layout restoration")]
    LayoutRestoration,
    #[serde(rename = "post_layout_checkpoint", alias = "post-layout checkpoint")]
    PostLayoutCheckpoint,
    #[serde(
        rename = "pre_process_disposition_checkpoint",
        alias = "pre-process-disposition checkpoint"
    )]
    PreProcessDispositionCheckpoint,
    #[serde(
        rename = "process_disposition_evaluation",
        alias = "process disposition evaluation"
    )]
    ProcessDispositionEvaluation,
    #[serde(
        rename = "post_process_disposition_checkpoint",
        alias = "post-process-disposition checkpoint"
    )]
    PostProcessDispositionCheckpoint,
}

fn restore_layout_interruption_reason(
    reason: LayoutRestoreInterruptionReason,
) -> RestoreInterruptionReason {
    match reason {
        LayoutRestoreInterruptionReason::Cancelled => RestoreInterruptionReason::Cancelled,
        LayoutRestoreInterruptionReason::CancellationCleanupTimedOut => {
            RestoreInterruptionReason::CancellationCleanupTimedOut
        }
        LayoutRestoreInterruptionReason::DeadlineExceeded => {
            RestoreInterruptionReason::DeadlineExceeded
        }
        LayoutRestoreInterruptionReason::PollQuotaExhausted => {
            RestoreInterruptionReason::PollQuotaExhausted
        }
        LayoutRestoreInterruptionReason::CostQuotaExhausted => {
            RestoreInterruptionReason::CostQuotaExhausted
        }
        LayoutRestoreInterruptionReason::ContextFailure => {
            RestoreInterruptionReason::ContextFailure
        }
        LayoutRestoreInterruptionReason::ValidationFailure => {
            RestoreInterruptionReason::ValidationFailure
        }
        LayoutRestoreInterruptionReason::BackendFailure => {
            RestoreInterruptionReason::BackendFailure
        }
        LayoutRestoreInterruptionReason::MuxOutcomeIndeterminate => {
            RestoreInterruptionReason::MuxOutcomeIndeterminate
        }
        LayoutRestoreInterruptionReason::IntegrityFailure => {
            RestoreInterruptionReason::IntegrityFailure
        }
    }
}

fn restore_process_interruption_reason(
    reason: LaunchInterruptionReason,
) -> RestoreInterruptionReason {
    match reason {
        LaunchInterruptionReason::Cancelled => RestoreInterruptionReason::Cancelled,
        LaunchInterruptionReason::CancellationCleanupTimedOut => {
            RestoreInterruptionReason::CancellationCleanupTimedOut
        }
        LaunchInterruptionReason::DeadlineExceeded => {
            RestoreInterruptionReason::DeadlineExceeded
        }
        LaunchInterruptionReason::PollQuotaExhausted => {
            RestoreInterruptionReason::PollQuotaExhausted
        }
        LaunchInterruptionReason::CostQuotaExhausted => {
            RestoreInterruptionReason::CostQuotaExhausted
        }
        LaunchInterruptionReason::ContextFailure => RestoreInterruptionReason::ContextFailure,
    }
}

fn restore_snapshot_interruption_reason(error: &SnapshotError) -> RestoreInterruptionReason {
    match error {
        SnapshotError::Cancelled => RestoreInterruptionReason::Cancelled,
        SnapshotError::DeadlineExceeded => RestoreInterruptionReason::DeadlineExceeded,
        SnapshotError::PollQuotaExhausted => RestoreInterruptionReason::PollQuotaExhausted,
        SnapshotError::CostBudgetExhausted => RestoreInterruptionReason::CostQuotaExhausted,
        SnapshotError::ContextFailure => RestoreInterruptionReason::ContextFailure,
        SnapshotError::IndeterminateAuthorityMutation { .. }
        | SnapshotError::AuthorityReconciliationRequired { .. }
        | SnapshotError::LockPoisoned
        | SnapshotError::LockPolledAfterCompletion => RestoreInterruptionReason::IntegrityFailure,
        SnapshotError::Topology(_)
        | SnapshotError::PaneIdentitySetMismatch { .. }
        | SnapshotError::ProjectionResourceLimit { .. }
        | SnapshotError::Serialization(_) => {
            RestoreInterruptionReason::ValidationFailure
        }
        SnapshotError::InProgress
        | SnapshotError::ShuttingDown
        | SnapshotError::SchedulerInProgress
        | SnapshotError::TriggerReceiverUnavailable
        | SnapshotError::NoPanes
        | SnapshotError::NoChanges
        | SnapshotError::PaneList(_)
        | SnapshotError::Database(_)
        | SnapshotError::AuthorityMutationInProgress { .. }
        | SnapshotError::BlockingRuntimeFailure
        | SnapshotError::ShutdownTimedOut { .. }
        | SnapshotError::ShutdownMarkFailed { .. }
        | SnapshotError::LockTimedOut { .. } => RestoreInterruptionReason::BackendFailure,
    }
}

fn restore_interruption_reason(
    cx: &crate::cx::Cx,
    error: &crate::runtime_async::ContextError,
) -> RestoreInterruptionReason {
    use crate::runtime_async::ContextErrorKind;

    match error.kind() {
        ContextErrorKind::DeadlineExceeded => RestoreInterruptionReason::DeadlineExceeded,
        ContextErrorKind::PollQuotaExhausted => RestoreInterruptionReason::PollQuotaExhausted,
        ContextErrorKind::CostQuotaExhausted => RestoreInterruptionReason::CostQuotaExhausted,
        ContextErrorKind::CancelTimeout => {
            RestoreInterruptionReason::CancellationCleanupTimedOut
        }
        ContextErrorKind::Cancelled => restore_cancel_kind_reason(
            cx.root_cancel_cause().map(|reason| reason.kind),
        ),
        _ => RestoreInterruptionReason::ContextFailure,
    }
}

fn restore_cancel_kind_reason(
    kind: Option<crate::outcome::CancelKind>,
) -> RestoreInterruptionReason {
    use crate::outcome::CancelKind;

    match kind {
        Some(CancelKind::Deadline | CancelKind::Timeout) => {
            RestoreInterruptionReason::DeadlineExceeded
        }
        Some(CancelKind::PollQuota) => RestoreInterruptionReason::PollQuotaExhausted,
        Some(CancelKind::CostBudget) => RestoreInterruptionReason::CostQuotaExhausted,
        Some(
            CancelKind::User
            | CancelKind::FailFast
            | CancelKind::RaceLost
            | CancelKind::ParentCancelled
            | CancelKind::ResourceUnavailable
            | CancelKind::Shutdown
            | CancelKind::LinkedExit,
        )
        | None => RestoreInterruptionReason::Cancelled,
    }
}

fn restore_context_error(
    phase: &'static str,
    cx: &crate::cx::Cx,
    error: &crate::runtime_async::ContextError,
) -> RestoreError {
    RestoreError::Interrupted {
        phase,
        reason: restore_interruption_reason(cx, error),
    }
}

fn restore_blocking_error(
    phase: &'static str,
    error: crate::runtime_async::SpawnBlockingWithCxError,
) -> RestoreError {
    use crate::runtime_async::SpawnBlockingWithCxError;

    match error {
        SpawnBlockingWithCxError::CancelledBeforeSpawn { kind }
        | SpawnBlockingWithCxError::CancelledMidFlight { kind } => {
            RestoreError::Interrupted {
                phase,
                reason: restore_cancel_kind_reason(kind),
            }
        }
        SpawnBlockingWithCxError::RuntimeFailure => RestoreError::InfrastructureFailure {
            phase,
            failure: RestoreInfrastructureFailure::BlockingRuntimeFailure,
        },
        SpawnBlockingWithCxError::CancellationWatcherTimerFailure => {
            RestoreError::InfrastructureFailure {
                phase,
                failure: RestoreInfrastructureFailure::CancellationWatcherTimerFailure,
            }
        }
    }
}

impl From<rusqlite::Error> for RestoreError {
    fn from(_error: rusqlite::Error) -> Self {
        RestoreError::Database("database operation failed".to_string())
    }
}

#[derive(Debug, thiserror::Error)]
enum RestoreAuthorityDbError {
    #[error("{source}")]
    RetrySafe {
        #[source]
        source: RestoreError,
    },
    #[error("restore authority commit outcome is indeterminate: {source}")]
    IndeterminateCommit {
        #[source]
        source: RestoreError,
    },
    #[error(
        "restore authority mutation failed ({source}) and rollback acknowledgement failed ({rollback})"
    )]
    IndeterminateRollback {
        source: RestoreError,
        rollback: Box<RestoreError>,
    },
}

impl SnapshotAuthorityWorkFailure for RestoreAuthorityDbError {
    fn requires_reconciliation(&self) -> bool {
        matches!(
            self,
            Self::IndeterminateCommit { .. } | Self::IndeterminateRollback { .. }
        )
    }
}

fn restore_authority_error(stage: &'static str, error: SnapshotError) -> RestoreError {
    match error {
        SnapshotError::Cancelled => RestoreError::Interrupted {
            phase: stage,
            reason: RestoreInterruptionReason::Cancelled,
        },
        SnapshotError::DeadlineExceeded => RestoreError::Interrupted {
            phase: stage,
            reason: RestoreInterruptionReason::DeadlineExceeded,
        },
        SnapshotError::PollQuotaExhausted => RestoreError::Interrupted {
            phase: stage,
            reason: RestoreInterruptionReason::PollQuotaExhausted,
        },
        SnapshotError::CostBudgetExhausted => RestoreError::Interrupted {
            phase: stage,
            reason: RestoreInterruptionReason::CostQuotaExhausted,
        },
        SnapshotError::ContextFailure => RestoreError::Interrupted {
            phase: stage,
            reason: RestoreInterruptionReason::ContextFailure,
        },
        _other => RestoreError::Bookkeeping(format!(
            "{stage}: durable restore authority operation did not settle"
        )),
    }
}

fn decode_u64(value: i64, field: &'static str) -> Result<u64, RestoreError> {
    u64::try_from(value).map_err(|_| RestoreError::InvalidPersistedValue { field, value })
}

fn decode_opt_u64(value: Option<i64>, field: &'static str) -> Result<Option<u64>, RestoreError> {
    value.map(|v| decode_u64(v, field)).transpose()
}

fn decode_usize(value: i64, field: &'static str) -> Result<usize, RestoreError> {
    usize::try_from(value).map_err(|_| RestoreError::InvalidPersistedValue { field, value })
}

fn decode_opt_usize(
    value: Option<i64>,
    field: &'static str,
) -> Result<Option<usize>, RestoreError> {
    value.map(|v| decode_usize(v, field)).transpose()
}

fn decode_bool(value: i64, field: &'static str) -> Result<bool, RestoreError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(RestoreError::InvalidPersistedValue { field, value }),
    }
}

fn validate_session_selector(session_id: &str) -> Result<(), RestoreError> {
    if session_id.is_empty() || session_id.len() > MAX_CHECKPOINT_SESSION_ID_BYTES {
        return Err(RestoreError::InvalidSessionSelector);
    }
    Ok(())
}

fn decode_required_bounded_text(
    value: Option<String>,
    field: &'static str,
) -> Result<String, RestoreError> {
    value.ok_or(RestoreError::InvalidPersistedText { field })
}

fn decode_optional_bounded_text(
    value: Option<String>,
    was_present: i64,
    field: &'static str,
) -> Result<Option<String>, RestoreError> {
    let was_present = decode_bool(was_present, field)?;
    if was_present != value.is_some() {
        return Err(RestoreError::InvalidPersistedText { field });
    }
    Ok(value)
}

// =============================================================================
// Configuration
// =============================================================================

/// Session restore behavior configuration.
///
/// This library value is constructed programmatically by explicit restore
/// callers. It is not a top-level `ft.toml` section: `[session]` is rejected
/// until the production startup path and a safe replay channel are wired.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRestoreConfig {
    /// Skip the restore prompt and always restore automatically.
    pub auto_restore: bool,
    /// Request historical scrollback replay. Enabling this currently fails
    /// closed because the mux exposes no terminal-output restoration channel.
    pub restore_scrollback: bool,
}

impl Default for SessionRestoreConfig {
    fn default() -> Self {
        Self {
            auto_restore: false,
            restore_scrollback: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct SessionRestoreConfigWire {
    auto_restore: bool,
    restore_scrollback: bool,
    /// Presence sentinel for the retired replay-size knob. Historical
    /// scrollback replay is unavailable, so silently accepting this setting
    /// would falsely imply that it constrains a live restoration path.
    #[serde(default, deserialize_with = "reject_retired_restore_max_lines")]
    restore_max_lines: (),
    /// Presence sentinel for the retired process-relaunch surface. Process
    /// identity is not captured with enough authority to resume or recreate
    /// it, so every explicit representation must fail closed.
    #[serde(default, deserialize_with = "reject_retired_session_process_relaunch")]
    process_relaunch: (),
}

impl Default for SessionRestoreConfigWire {
    fn default() -> Self {
        let defaults = SessionRestoreConfig::default();
        Self {
            auto_restore: defaults.auto_restore,
            restore_scrollback: defaults.restore_scrollback,
            restore_max_lines: (),
            process_relaunch: (),
        }
    }
}

fn reject_retired_restore_max_lines<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ignored = serde::de::IgnoredAny::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "session.restore_max_lines was removed because historical scrollback replay is unavailable; delete this setting",
    ))
}

fn reject_retired_session_process_relaunch<'de, D>(
    deserializer: D,
) -> Result<(), D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ignored = serde::de::IgnoredAny::deserialize(deserializer)?;
    Err(serde::de::Error::custom(
        "session.process_relaunch was removed because process and agent restoration is unavailable; delete this setting",
    ))
}

impl<'de> Deserialize<'de> for SessionRestoreConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let SessionRestoreConfigWire {
            auto_restore,
            restore_scrollback,
            restore_max_lines: (),
            process_relaunch: (),
        } = SessionRestoreConfigWire::deserialize(deserializer)?;
        Ok(Self {
            auto_restore,
            restore_scrollback,
        })
    }
}

// =============================================================================
// Data types
// =============================================================================

/// A session candidate for restore.
#[derive(Clone)]
pub struct SessionCandidate {
    pub session_id: String,
    pub created_at: u64,
    pub last_checkpoint_at: Option<u64>,
    /// Exact snapshot selected by bounded detection, when detection performed
    /// that selection. Restore still reloads and verifies this row before any
    /// external mux effect.
    pub selected_checkpoint_id: Option<i64>,
    pub ft_version: String,
    pub host_id: Option<String>,
}

impl std::fmt::Debug for SessionCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCandidate")
            .field("has_session_id", &!self.session_id.is_empty())
            .field("has_last_checkpoint", &self.last_checkpoint_at.is_some())
            .field("selected_checkpoint_id", &self.selected_checkpoint_id)
            .field("has_ft_version", &!self.ft_version.is_empty())
            .field("has_host_id", &self.host_id.is_some())
            .finish()
    }
}

/// Durable purpose of a checkpoint row.
///
/// Snapshot rows carry restorable topology and pane state. Restore intents and
/// outcome receipts are authority records; neither may be selected as a layout
/// restore point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointRole {
    Snapshot,
    RestoreIntent,
    RestoreReceipt,
}

impl CheckpointRole {
    fn from_db(checkpoint_id: i64, role: &str) -> Result<Self, RestoreError> {
        match role {
            CHECKPOINT_ROLE_SNAPSHOT => Ok(Self::Snapshot),
            CHECKPOINT_ROLE_RESTORE_INTENT => Ok(Self::RestoreIntent),
            CHECKPOINT_ROLE_RESTORE_RECEIPT => Ok(Self::RestoreReceipt),
            _ => Err(RestoreError::InvalidCheckpointRole {
                checkpoint_id,
                role: role.to_string(),
            }),
        }
    }

    const fn as_db_str(self) -> &'static str {
        match self {
            Self::Snapshot => CHECKPOINT_ROLE_SNAPSHOT,
            Self::RestoreIntent => CHECKPOINT_ROLE_RESTORE_INTENT,
            Self::RestoreReceipt => CHECKPOINT_ROLE_RESTORE_RECEIPT,
        }
    }
}

impl std::fmt::Display for CheckpointRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_db_str())
    }
}

/// Strength of the consistency evidence carried by a loaded row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointVerification {
    /// Versioned SHA-256 witness recomputed over the exact stored projection.
    VerifiedV2,
    /// Historical 16-hex witness or literal `restore`. It is retained for
    /// explicit manual recovery only and is never eligible for auto-restore.
    LegacyUnverified,
}

/// Loaded checkpoint with per-pane state.
#[derive(Clone)]
pub struct CheckpointData {
    pub checkpoint_id: i64,
    pub session_id: String,
    pub checkpoint_at: u64,
    pub checkpoint_type: String,
    pub checkpoint_role: CheckpointRole,
    pub verification: CheckpointVerification,
    /// Exact persisted consistency witness for this checkpoint row.
    pub state_hash: String,
    /// Exact bounded metadata bytes loaded with this row. Coverage by a
    /// recomputed v2 witness is indicated separately by `verification`.
    ///
    /// Callers must parse a purpose-specific bounded projection and must not
    /// emit this potentially sensitive payload directly.
    metadata_json: Option<String>,
    /// Topology owned by this exact checkpoint. Receipts have no topology.
    pub topology_json: Option<String>,
    /// Exact intent causally settled by this outcome receipt. Snapshot and
    /// intent rows carry no parent link; legacy receipts may also be unlinked.
    pub restore_intent_checkpoint_id: Option<i64>,
    pub pane_count: usize,
    pub total_bytes: usize,
    pub pane_states: Vec<RestoredPaneState>,
}

impl std::fmt::Debug for CheckpointData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CheckpointData")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("checkpoint_role", &self.checkpoint_role)
            .field("verification", &self.verification)
            .field("has_session_id", &!self.session_id.is_empty())
            .field("has_checkpoint_type", &!self.checkpoint_type.is_empty())
            .field("has_state_hash", &!self.state_hash.is_empty())
            .field("has_metadata", &self.metadata_json.is_some())
            .field("has_topology", &self.topology_json.is_some())
            .field(
                "restore_intent_checkpoint_id",
                &self.restore_intent_checkpoint_id,
            )
            .field("declared_pane_count", &self.pane_count)
            .field("loaded_pane_count", &self.pane_states.len())
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl CheckpointData {
    /// Return the exact bounded metadata JSON loaded with this checkpoint.
    /// Consult [`CheckpointData::verification`] before treating those bytes as
    /// covered by a recomputed v2 consistency witness; legacy rows are exposed
    /// for explicit recovery but remain unverified.
    #[must_use]
    pub fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }
}

/// Per-pane state loaded from the database.
#[derive(Clone)]
pub struct RestoredPaneState {
    pub pane_id: u64,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub terminal_state: Option<TerminalState>,
    pub agent_metadata: Option<AgentMetadata>,
    pub scrollback_checkpoint_seq: Option<u64>,
    pub last_output_at: Option<u64>,
}

impl std::fmt::Debug for RestoredPaneState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestoredPaneState")
            .field("pane_id", &self.pane_id)
            .field("has_cwd", &self.cwd.is_some())
            .field("has_command", &self.command.is_some())
            .field("has_terminal_state", &self.terminal_state.is_some())
            .field("has_agent_metadata", &self.agent_metadata.is_some())
            .field(
                "has_scrollback_checkpoint",
                &self.scrollback_checkpoint_seq.is_some(),
            )
            .field("has_last_output_at", &self.last_output_at.is_some())
            .finish()
    }
}

/// Result of a layout reconstruction and durable restore-authority settlement.
pub struct RestoreSummary {
    /// The session that was restored.
    pub session_id: String,
    /// Checkpoint that was loaded.
    pub checkpoint_id: i64,
    /// Durable intent written before the first mux mutation.
    pub intent_checkpoint_id: i64,
    /// Durable outcome receipt written after external effects settled.
    pub outcome_checkpoint_id: i64,
    /// Layout restoration result.
    pub layout_result: RestoreResult,
    /// Pane states that were loaded.
    pub pane_states: Vec<RestoredPaneState>,
    /// Process disposition report when at least one captured plan was evaluated.
    pub process_launch_report: Option<LaunchReport>,
    /// Whether the exact restore receipt was durably bound as clean authority.
    pub restore_authority_resolved: bool,
    /// Total time for the restore in milliseconds.
    pub elapsed_ms: u64,
}

impl std::fmt::Debug for RestoreSummary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestoreSummary")
            .field("has_session_id", &!self.session_id.is_empty())
            .field("checkpoint_id", &self.checkpoint_id)
            .field("intent_checkpoint_id", &self.intent_checkpoint_id)
            .field("outcome_checkpoint_id", &self.outcome_checkpoint_id)
            .field("mapped_panes", &self.layout_result.pane_id_map.len())
            .field("failed_panes", &self.layout_result.failed_panes.len())
            .field("windows_created", &self.layout_result.windows_created)
            .field("tabs_created", &self.layout_result.tabs_created)
            .field("panes_created", &self.layout_result.panes_created)
            .field("loaded_pane_count", &self.pane_states.len())
            .field(
                "has_process_disposition_report",
                &self.process_launch_report.is_some(),
            )
            .field(
                "restore_authority_resolved",
                &self.restore_authority_resolved,
            )
            .field("elapsed_ms", &self.elapsed_ms)
            .finish()
    }
}

fn duplicate_target_source_pane_ids(pane_id_map: &HashMap<u64, u64>) -> HashSet<u64> {
    let mut target_counts = HashMap::with_capacity(pane_id_map.len());
    for &target_pane_id in pane_id_map.values() {
        let count = target_counts.entry(target_pane_id).or_insert(0_usize);
        *count = count.saturating_add(1);
    }
    pane_id_map
        .iter()
        .filter(|(_, target_pane_id)| {
            target_counts
                .get(target_pane_id)
                .is_some_and(|count| *count > 1)
        })
        .map(|(&source_pane_id, _)| source_pane_id)
        .collect()
}

/// Retain only source-owned, one-to-one mappings that are not also classified
/// as failed. Raw mux results may report a mapping and a failure for the same
/// source, or map multiple sources onto one target. Neither shape is an
/// authoritative success, but both must remain representable in the separate
/// failure/anomaly evidence of a durable partial receipt.
fn authoritative_persisted_pane_mapping(
    expected_pane_ids: &HashSet<u64>,
    failed_source_pane_ids: &[u64],
    raw_pane_id_map: &HashMap<u64, u64>,
) -> HashMap<u64, u64> {
    let mut failed_source_pane_ids = failed_source_pane_ids.iter().copied().collect::<HashSet<_>>();
    failed_source_pane_ids.extend(duplicate_target_source_pane_ids(raw_pane_id_map));
    let mut ordered_expected_pane_ids = expected_pane_ids.iter().copied().collect::<Vec<_>>();
    ordered_expected_pane_ids.sort_unstable();
    let mut persisted_target_ids = HashSet::new();
    ordered_expected_pane_ids
        .into_iter()
        .filter_map(|old_id| {
            if failed_source_pane_ids.contains(&old_id) {
                return None;
            }
            let new_id = *raw_pane_id_map.get(&old_id)?;
            persisted_target_ids.insert(new_id).then_some((old_id, new_id))
        })
        .collect()
}

impl RestoreSummary {
    fn failed_expected_pane_ids_for(&self, expected: &HashSet<u64>) -> HashSet<u64> {
        let mut failed = expected
            .iter()
            .copied()
            .filter(|pane_id| !self.layout_result.pane_id_map.contains_key(pane_id))
            .collect::<HashSet<_>>();
        failed.extend(
            self.layout_result
                .failed_panes
                .iter()
                .map(|(pane_id, _)| *pane_id)
                .filter(|pane_id| expected.contains(pane_id)),
        );

        // A duplicate target means none of the colliding source mappings is an
        // authoritative success. Counting both sources as settled would report
        // impossible progress such as 2/2 even though only one target pane
        // exists. Include unexpected source keys when detecting collisions so
        // an expected source cannot appear settled by sharing their target.
        failed.extend(
            duplicate_target_source_pane_ids(&self.layout_result.pane_id_map)
                .into_iter()
                .filter(|source_pane_id| expected.contains(source_pane_id)),
        );
        failed
    }

    fn failed_expected_pane_ids(&self) -> HashSet<u64> {
        let expected = self
            .pane_states
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<HashSet<_>>();
        self.failed_expected_pane_ids_for(&expected)
    }

    /// Number of expected panes whose layout mutation settled successfully.
    pub fn layout_settled_pane_count(&self) -> usize {
        self.expected_pane_count()
            .saturating_sub(self.layout_failed_pane_count())
    }

    /// Number of unique expected panes that were either not mapped or were
    /// explicitly reported failed after mapping (for example, activation), or
    /// participates in a duplicate-target mapping collision.
    /// Unexpected backend pane IDs are reported separately as integrity
    /// anomalies and never inflate this source-pane count.
    pub fn layout_failed_pane_count(&self) -> usize {
        self.failed_expected_pane_ids().len()
    }

    /// Number of source panes expected by the selected checkpoint.
    pub fn expected_pane_count(&self) -> usize {
        self.pane_states.len()
    }

}

// =============================================================================
// Database queries
// =============================================================================

fn open_conn(db_path: &str) -> Result<Connection, RestoreError> {
    let conn = Connection::open(db_path)?;
    // [ft-rfpk6] `PRAGMA foreign_keys` is per-connection. The schema's
    // ON DELETE CASCADE chain (mux_pane_state → session_checkpoints →
    // mux_sessions) only fires when FKs are ON. Without this, any
    // future DELETE that relies on CASCADE leaks orphan rows. See
    // ft-s4myu for the matching fix on StorageHandle::with_config.
    // `journal_mode` is persistent database state and is established by the
    // storage/migration owner. Reissuing `PRAGMA journal_mode=WAL` on every
    // restore/list/read connection can require a schema lock and turn a
    // nominally read-only CLI query into a contending write-like operation.
    // Keep only connection-local safety settings here.
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// Open an observation-only connection without SQLite's default CREATE flag.
/// A typo in a CLI/list/show path must not materialize an empty database, and
/// read traffic must never acquire write authority merely to inspect restore
/// state.
fn open_query_conn(db_path: &str) -> Result<Connection, RestoreError> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

/// Run a multi-statement observation against one stable SQLite read snapshot.
/// Payload admission intentionally uses separate scalar, aggregate, and text
/// statements so oversized values never cross into Rust before their limits
/// pass; the surrounding transaction keeps those statements causally aligned.
fn with_query_snapshot<T>(
    db_path: &str,
    operation: impl FnOnce(&Connection) -> Result<T, RestoreError>,
) -> Result<T, RestoreError> {
    let mut conn = open_query_conn(db_path)?;
    let tx = conn.transaction()?;
    let result = operation(&tx)?;
    tx.commit()?;
    Ok(result)
}

type RestoreAmbiguity = (i64, Option<i64>, String);
type RestoreLifecycleAuthorityRow = (i64, Option<i64>, Option<String>, i64, Option<i64>);
type RestoreSessionAuthorityRow = (i64, Option<i64>, Option<i64>, Option<i64>);

/// Return the causal-first restore attempt that makes a new restore unsafe.
///
/// A lifecycle row is authoritative only for the same session as its intent.
/// Explicit schema-v38 intents and schema-v37 receipt-shaped intents without
/// such a lifecycle row are therefore ambiguity, not permission to retry.
fn restore_ambiguity_from_conn(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<RestoreAmbiguity>, RestoreError> {
    let unresolved = conn.query_row(
        "SELECT intent_checkpoint_id, outcome_checkpoint_id, status
         FROM (
             SELECT lifecycle.intent_checkpoint_id AS intent_checkpoint_id,
                    lifecycle.outcome_checkpoint_id AS outcome_checkpoint_id,
                    CASE lifecycle.status
                        WHEN 'intent' THEN 'intent'
                        WHEN 'outcome_complete' THEN 'outcome_complete'
                        WHEN 'reconciliation_required' THEN 'reconciliation_required'
                        ELSE 'invalid_lifecycle'
                    END AS status,
                    0 AS ambiguity_kind
             FROM restore_attempt_lifecycle AS lifecycle
             WHERE lifecycle.session_id = ?1
               AND CASE
                       WHEN typeof(lifecycle.status) = 'text'
                        AND lifecycle.status = 'resolved'
                       THEN 0
                       ELSE 1
                   END = 1

             UNION ALL

             SELECT intent.id AS intent_checkpoint_id,
                    NULL AS outcome_checkpoint_id,
                    'orphaned_intent' AS status,
                    1 AS ambiguity_kind
             FROM session_checkpoints AS intent
             WHERE intent.session_id = ?1
               AND (
                   intent.checkpoint_role = 'restore_intent'
                   OR (
                   intent.checkpoint_role = 'restore_receipt'
                       AND CASE
                           WHEN typeof(intent.metadata_json) != 'text'
                             OR length(CAST(intent.metadata_json AS BLOB)) > ?2
                           THEN 1
                           WHEN NOT json_valid(intent.metadata_json)
                           THEN 1
                           WHEN json_extract(
                               intent.metadata_json,
                               '$.restore_attempt.phase'
                           ) = 'outcome'
                           THEN 0
                           WHEN json_type(
                                   intent.metadata_json,
                                   '$.restore_attempt'
                                ) IS NULL
                            AND json_type(
                                   intent.metadata_json,
                                   '$.old_to_new'
                                ) = 'object'
                           THEN 0
                           ELSE 1
                       END = 1
                   )
               )
               AND NOT EXISTS (
                   SELECT 1
                   FROM restore_attempt_lifecycle AS lifecycle
                   WHERE lifecycle.session_id = intent.session_id
                     AND lifecycle.intent_checkpoint_id = intent.id
               )
         )
         ORDER BY intent_checkpoint_id ASC, ambiguity_kind ASC
         LIMIT 1",
        rusqlite::params![
            session_id,
            i64::try_from(MAX_CHECKPOINT_METADATA_BYTES).unwrap_or(i64::MAX),
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .map_err(RestoreError::from)?;
    // A resolved lifecycle suppresses replay only while its exact outcome
    // remains a valid verified authority chain. Historical/corrupt databases
    // can lose the outcome row with foreign keys disabled; treating that as a
    // clean absence would allow the older source snapshot to be replayed.
    let mut resolved_stmt = conn.prepare(
        "SELECT CASE
                    WHEN typeof(intent_checkpoint_id) = 'integer'
                     AND intent_checkpoint_id > 0
                    THEN intent_checkpoint_id
                END,
                CASE
                    WHEN typeof(outcome_checkpoint_id) = 'integer'
                     AND outcome_checkpoint_id > 0
                    THEN outcome_checkpoint_id
                END
         FROM restore_attempt_lifecycle
         WHERE session_id = ?1
           AND typeof(status) = 'text'
           AND status = 'resolved'
         ORDER BY intent_checkpoint_id ASC",
    )?;
    let mut resolved_rows = resolved_stmt.query([session_id])?;
    let mut invalid_resolved = None;
    while let Some(row) = resolved_rows.next()? {
        let intent_checkpoint_id = row.get::<_, Option<i64>>(0)?;
        let outcome_checkpoint_id = row.get::<_, Option<i64>>(1)?;
        let Some(intent_checkpoint_id) = intent_checkpoint_id else {
            invalid_resolved = Some((
                -1,
                outcome_checkpoint_id,
                "invalid_resolved_chain".to_string(),
            ));
            break;
        };
        let Some(outcome_checkpoint_id) = outcome_checkpoint_id else {
            invalid_resolved = Some((
                intent_checkpoint_id,
                None,
                "invalid_resolved_chain".to_string(),
            ));
            break;
        };
        if !resolved_restore_chain_row_is_valid(
            conn,
            session_id,
            intent_checkpoint_id,
            outcome_checkpoint_id,
        )? {
            invalid_resolved = Some((
                intent_checkpoint_id,
                Some(outcome_checkpoint_id),
                "invalid_resolved_chain".to_string(),
            ));
            break;
        }
    }
    Ok(match (unresolved, invalid_resolved) {
        (Some(unresolved), Some(invalid_resolved)) => {
            if unresolved.0 <= invalid_resolved.0 {
                Some(unresolved)
            } else {
                Some(invalid_resolved)
            }
        }
        (Some(unresolved), None) => Some(unresolved),
        (None, Some(invalid_resolved)) => Some(invalid_resolved),
        (None, None) => None,
    })
}

fn resolved_restore_chain_row_is_valid(
    conn: &Connection,
    session_id: &str,
    intent_checkpoint_id: i64,
    outcome_checkpoint_id: i64,
) -> Result<bool, RestoreError> {
    let session_exists = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM mux_sessions WHERE session_id = ?1
         )",
        [session_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !session_exists {
        return Ok(false);
    }
    let outcome = match load_checkpoint_by_id_from_conn(conn, outcome_checkpoint_id) {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return Ok(false),
        Err(RestoreError::Database(error)) => return Err(RestoreError::Database(error)),
        Err(_error) => return Ok(false),
    };
    match validate_restore_authority_chain(conn, session_id, &outcome, "resolved", false) {
        Ok(validated_intent_id) => Ok(validated_intent_id == intent_checkpoint_id),
        Err(RestoreError::Database(error)) => Err(RestoreError::Database(error)),
        Err(_error) => Ok(false),
    }
}

fn count_invalid_resolved_restore_chains_from_conn(
    conn: &Connection,
) -> Result<usize, RestoreError> {
    let mut stmt = conn.prepare(
        "SELECT CASE
                    WHEN typeof(intent_checkpoint_id) = 'integer'
                     AND intent_checkpoint_id > 0
                    THEN intent_checkpoint_id
                END,
                CASE
                    WHEN typeof(session_id) = 'text'
                     AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?1
                    THEN session_id
                END,
                CASE
                    WHEN typeof(outcome_checkpoint_id) = 'integer'
                     AND outcome_checkpoint_id > 0
                    THEN outcome_checkpoint_id
                END
         FROM restore_attempt_lifecycle
         WHERE typeof(status) = 'text' AND status = 'resolved'
         ORDER BY intent_checkpoint_id ASC",
    )?;
    let mut rows = stmt.query([
        i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
    ])?;
    let mut invalid = 0usize;
    while let Some(row) = rows.next()? {
        let intent_checkpoint_id = row.get::<_, Option<i64>>(0)?;
        let session_id = row.get::<_, Option<String>>(1)?;
        let outcome_checkpoint_id = row.get::<_, Option<i64>>(2)?;
        let valid = match (intent_checkpoint_id, session_id, outcome_checkpoint_id) {
            (Some(intent_checkpoint_id), Some(session_id), Some(outcome_checkpoint_id)) => {
                resolved_restore_chain_row_is_valid(
                    conn,
                    &session_id,
                    intent_checkpoint_id,
                    outcome_checkpoint_id,
                )?
            }
            _ => false,
        };
        if !valid {
            invalid = invalid.saturating_add(1);
        }
    }
    Ok(invalid)
}

/// Validate the exact clean-session authority row. V2 receipts are accepted
/// only after recomputing their complete persisted witness. Legacy rows remain
/// available for explicit manual recovery, but cannot authorize destructive
/// retention or suppress crash recovery because their persisted projection is
/// not independently verifiable.
pub(crate) fn assess_clean_authority(
    conn: &Connection,
    session_id: &str,
    shutdown_clean: i64,
    clean_checkpoint_id: Option<i64>,
) -> Result<bool, RestoreError> {
    if !decode_bool(shutdown_clean, "mux_sessions.shutdown_clean")? {
        return Ok(false);
    }
    let Some(clean_checkpoint_id) = clean_checkpoint_id else {
        return Ok(false);
    };
    if restore_ambiguity_from_conn(conn, session_id)?.is_some() {
        return Ok(false);
    }
    let latest_checkpoint_id = conn
        .query_row(
            "SELECT id
             FROM session_checkpoints
             WHERE session_id = ?1
             ORDER BY id DESC
             LIMIT 1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if latest_checkpoint_id != Some(clean_checkpoint_id) {
        return Ok(false);
    }

    let checkpoint_identity: Option<(Option<String>, Option<String>, Option<i64>)> = conn
        .query_row(
            "SELECT CASE
                        WHEN typeof(checkpoint_role) = 'text'
                         AND length(CAST(checkpoint_role AS BLOB))
                             BETWEEN 1 AND ?3
                        THEN checkpoint_role
                    END,
                    CASE
                        WHEN typeof(state_hash) = 'text'
                         AND length(CAST(state_hash AS BLOB))
                             BETWEEN 1 AND ?4
                        THEN state_hash
                    END,
                    CASE
                        WHEN typeof(checkpoint_at) = 'integer'
                         AND checkpoint_at >= 0
                        THEN checkpoint_at
                    END
             FROM session_checkpoints
             WHERE id = ?1 AND session_id = ?2",
            rusqlite::params![
                clean_checkpoint_id,
                session_id,
                i64::try_from(MAX_CHECKPOINT_ROLE_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_CHECKPOINT_STATE_HASH_BYTES).unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((
        Some(checkpoint_role_raw),
        Some(stored_state_hash),
        Some(checkpoint_at),
    )) = checkpoint_identity
    else {
        return Ok(false);
    };
    let summary_checkpoint_at = conn
        .query_row(
            "SELECT last_checkpoint_at FROM mux_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    if summary_checkpoint_at != Some(checkpoint_at) {
        return Ok(false);
    }
    let checkpoint_role = match CheckpointRole::from_db(
        clean_checkpoint_id,
        &checkpoint_role_raw,
    ) {
        Ok(role) => role,
        Err(_error) => {
            warn!(
                session_id_bytes = session_id.len(),
                checkpoint_id = clean_checkpoint_id,
                "Rejecting clean-session authority with an invalid role"
            );
            return Ok(false);
        }
    };
    let is_v2 = stored_state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX)
        || stored_state_hash.starts_with(RESTORE_INTENT_WITNESS_PREFIX)
        || stored_state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX);
    if !is_v2 {
        debug!(
            session_id_bytes = session_id.len(),
            checkpoint_id = clean_checkpoint_id,
            role = %checkpoint_role,
            "Rejecting legacy-unverified clean-session authority"
        );
        return Ok(false);
    }

    let checkpoint = match load_checkpoint_by_id_from_conn(conn, clean_checkpoint_id) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) => return Ok(false),
        Err(RestoreError::Database(error)) => return Err(RestoreError::Database(error)),
        Err(_error) => {
            warn!(
                session_id_bytes = session_id.len(),
                checkpoint_id = clean_checkpoint_id,
                "Rejecting corrupt clean-session authority"
            );
            return Ok(false);
        }
    };
    if checkpoint.session_id != session_id {
        return Ok(false);
    }
    match checkpoint.checkpoint_role {
        CheckpointRole::Snapshot => {
            if checkpoint.checkpoint_type != "shutdown" {
                return Ok(false);
            }
        }
        CheckpointRole::RestoreIntent => return Ok(false),
        CheckpointRole::RestoreReceipt => {
            if let Err(error) = validate_restore_authority_chain(
                conn,
                session_id,
                &checkpoint,
                "resolved",
                false,
            ) {
                if let RestoreError::Database(message) = error {
                    return Err(RestoreError::Database(message));
                }
                warn!(
                    session_id_bytes = session_id.len(),
                    checkpoint_id = clean_checkpoint_id,
                    "Rejecting clean restore authority with an invalid causal chain"
                );
                return Ok(false);
            }
        }
    }
    Ok(checkpoint.verification == CheckpointVerification::VerifiedV2)
}

/// Find sessions that did not shut down cleanly.
pub fn find_unclean_sessions(db_path: &str) -> Result<Vec<SessionCandidate>, RestoreError> {
    with_query_snapshot(db_path, find_unclean_sessions_from_conn)
}

fn find_unclean_sessions_from_conn(
    conn: &Connection,
) -> Result<Vec<SessionCandidate>, RestoreError> {
    let mut stmt = conn.prepare(
        "SELECT CASE
                    WHEN typeof(session.session_id) = 'text'
                     AND length(CAST(session.session_id AS BLOB)) BETWEEN 1 AND ?1
                    THEN session.session_id
                END,
                session.created_at,
                (SELECT causal.checkpoint_at
                 FROM session_checkpoints AS causal
                 WHERE causal.session_id = session.session_id
                 ORDER BY causal.id DESC
                 LIMIT 1) AS last_checkpoint_at,
                (SELECT snapshot.id
                 FROM session_checkpoints AS snapshot
                 WHERE snapshot.session_id = session.session_id
                   AND snapshot.checkpoint_role = 'snapshot'
                 ORDER BY snapshot.id DESC
                 LIMIT 1) AS selected_checkpoint_id,
                CASE
                    WHEN typeof(session.ft_version) = 'text'
                     AND length(CAST(session.ft_version AS BLOB)) BETWEEN 1 AND ?2
                    THEN session.ft_version
                END,
                CASE
                    WHEN session.host_id IS NULL THEN NULL
                    WHEN typeof(session.host_id) = 'text'
                     AND length(CAST(session.host_id AS BLOB)) <= ?3
                    THEN session.host_id
                END,
                session.host_id IS NOT NULL,
                session.shutdown_clean,
                session.clean_checkpoint_id
         FROM mux_sessions AS session
         ORDER BY COALESCE((
                      SELECT MAX(causal.id)
                      FROM session_checkpoints AS causal
                      WHERE causal.session_id = session.session_id
                  ), -1) DESC,
                  session.created_at DESC,
                  session.session_id ASC",
    )?;

    let rows = stmt.query_map(
        rusqlite::params![
            i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
            i64::try_from(MAX_SESSION_FT_VERSION_BYTES).unwrap_or(i64::MAX),
            i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
        ],
        |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (
            session_id,
            created_at,
            last_checkpoint_at,
            selected_checkpoint_id,
            ft_version,
            host_id,
            host_id_present,
            shutdown_clean,
            clean_checkpoint_id,
        ) = row?;
        let session_id = decode_required_bounded_text(
            session_id,
            "mux_sessions.session_id",
        )?;
        let ft_version = decode_required_bounded_text(
            ft_version,
            "mux_sessions.ft_version",
        )?;
        let host_id = decode_optional_bounded_text(
            host_id,
            host_id_present,
            "mux_sessions.host_id",
        )?;
        if assess_clean_authority(
            conn,
            &session_id,
            shutdown_clean,
            clean_checkpoint_id,
        )? {
            continue;
        }
        candidates.push(SessionCandidate {
            session_id,
            created_at: decode_u64(created_at, "mux_sessions.created_at")?,
            last_checkpoint_at: decode_opt_u64(
                last_checkpoint_at,
                "session_checkpoints.checkpoint_at",
            )?,
            selected_checkpoint_id,
            ft_version,
            host_id,
        });
    }

    Ok(candidates)
}

/// Load the latest checkpoint for a session, including pane states.
pub fn load_latest_checkpoint(
    db_path: &str,
    session_id: &str,
) -> Result<Option<CheckpointData>, RestoreError> {
    validate_session_selector(session_id)?;
    with_query_snapshot(db_path, |conn| {
        // Purpose is explicit in schema v36. Do not infer it from pane-row
        // presence or the overloaded `startup` checkpoint type: a genuine
        // empty snapshot is still a snapshot, while a receipt is never one.
        let checkpoint_id = conn.query_row(
            "SELECT c.id
             FROM session_checkpoints c
             WHERE c.session_id = ?1
               AND c.checkpoint_role = 'snapshot'
             ORDER BY c.id DESC
             LIMIT 1",
            [session_id],
            |row| row.get::<_, i64>(0),
        );

        match checkpoint_id {
            Ok(id) => load_checkpoint_by_id_from_conn(conn, id),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(_error) => Err(RestoreError::Database(
                "checkpoint lookup failed".to_string(),
            )),
        }
    })
}

#[derive(Debug, Clone, Copy)]
struct RestoreCandidateCheckpoint {
    checkpoint_at: u64,
    verification: CheckpointVerification,
}

/// Read only the bounded scalar admission data needed to rank a candidate.
/// Full topology/pane JSON and the v2 witness are loaded only after the
/// selected checkpoint has passed this descriptor admission.
fn restore_candidate_checkpoint_from_conn(
    conn: &Connection,
    checkpoint_id: i64,
) -> Result<Option<RestoreCandidateCheckpoint>, RestoreError> {
    let header = conn
        .query_row(
            "SELECT checkpoint_at,
                    pane_count,
                    total_bytes,
                    CASE
                        WHEN typeof(state_hash) = 'text'
                         AND length(CAST(state_hash AS BLOB)) BETWEEN 1 AND ?2
                        THEN state_hash
                    END,
                    CASE
                        WHEN topology_json IS NULL THEN NULL
                        ELSE length(CAST(topology_json AS BLOB))
                    END,
                    CASE
                        WHEN metadata_json IS NULL THEN NULL
                        ELSE length(CAST(metadata_json AS BLOB))
                    END
             FROM session_checkpoints
             WHERE id = ?1 AND checkpoint_role = 'snapshot'",
            rusqlite::params![
                checkpoint_id,
                i64::try_from(MAX_CHECKPOINT_STATE_HASH_BYTES).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        checkpoint_at_raw,
        pane_count_raw,
        total_bytes_raw,
        state_hash,
        topology_bytes_raw,
        metadata_bytes_raw,
    )) = header
    else {
        return Ok(None);
    };
    let checkpoint_at = decode_u64(checkpoint_at_raw, "session_checkpoints.checkpoint_at")?;
    let pane_count = decode_usize(pane_count_raw, "session_checkpoints.pane_count")?;
    let total_bytes = decode_usize(total_bytes_raw, "session_checkpoints.total_bytes")?;
    let state_hash = state_hash.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has an invalid or oversized state hash"
        ))
    })?;
    let topology_bytes = topology_bytes_raw
        .map(|bytes| decode_usize(bytes, "session_checkpoints.topology_json_bytes"))
        .transpose()?
        .ok_or(RestoreError::CheckpointTopologyUnavailable { checkpoint_id })?;
    let metadata_bytes = metadata_bytes_raw
        .map(|bytes| decode_usize(bytes, "session_checkpoints.metadata_json_bytes"))
        .transpose()?
        .unwrap_or(0);
    if pane_count == 0 {
        return Ok(None);
    }
    for (resource, observed, limit) in [
        ("pane_count", pane_count, MAX_TOPOLOGY_PANES),
        (
            "historical_payload_bytes",
            total_bytes,
            MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
        ),
        ("topology_json_bytes", topology_bytes, MAX_SNAPSHOT_BYTES),
        (
            "metadata_json_bytes",
            metadata_bytes,
            MAX_CHECKPOINT_METADATA_BYTES,
        ),
    ] {
        if observed > limit {
            return Err(RestoreError::CheckpointResourceLimit {
                checkpoint_id,
                resource,
                observed,
                limit,
            });
        }
    }

    let preflight_row_limit = pane_count.checked_add(1).ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} candidate row limit overflows usize"
        ))
    })?;
    let (
        stored_pane_count_raw,
        pane_text_bytes_raw,
        max_pane_text_bytes_raw,
        invalid_text_rows,
    ): (i64, i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(
                        COALESCE(length(CAST(cwd AS BLOB)), 0)
                      + COALESCE(length(CAST(command AS BLOB)), 0)
                      + COALESCE(length(CAST(env_json AS BLOB)), 0)
                      + COALESCE(length(CAST(terminal_state_json AS BLOB)), 0)
                      + COALESCE(length(CAST(agent_metadata_json AS BLOB)), 0)
                    ), 0),
                    COALESCE(MAX(
                        COALESCE(length(CAST(cwd AS BLOB)), 0)
                      + COALESCE(length(CAST(command AS BLOB)), 0)
                      + COALESCE(length(CAST(env_json AS BLOB)), 0)
                      + COALESCE(length(CAST(terminal_state_json AS BLOB)), 0)
                      + COALESCE(length(CAST(agent_metadata_json AS BLOB)), 0)
                    ), 0),
                    COALESCE(SUM(CASE
                        WHEN typeof(terminal_state_json) != 'text'
                          OR (cwd IS NOT NULL AND typeof(cwd) != 'text')
                          OR (command IS NOT NULL AND typeof(command) != 'text')
                          OR (env_json IS NOT NULL AND typeof(env_json) != 'text')
                          OR (agent_metadata_json IS NOT NULL
                              AND typeof(agent_metadata_json) != 'text')
                        THEN 1 ELSE 0 END), 0)
             FROM (
                 SELECT cwd, command, env_json, terminal_state_json,
                        agent_metadata_json
                 FROM mux_pane_state
                 WHERE checkpoint_id = ?1
                 LIMIT ?2
             ) AS bounded_pane_rows",
            rusqlite::params![
                checkpoint_id,
                i64::try_from(preflight_row_limit).unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let stored_pane_count = decode_usize(stored_pane_count_raw, "mux_pane_state.row_count")?;
    let pane_text_bytes = decode_usize(pane_text_bytes_raw, "mux_pane_state.text_bytes")?;
    let max_pane_text_bytes = decode_usize(
        max_pane_text_bytes_raw,
        "mux_pane_state.max_row_text_bytes",
    )?;
    let admitted_bytes = topology_bytes
        .checked_add(metadata_bytes)
        .and_then(|bytes| bytes.checked_add(pane_text_bytes))
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} candidate byte count overflows usize"
            ))
        })?;
    if invalid_text_rows != 0 || stored_pane_count != pane_count {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has an inadmissible pane projection"
        )));
    }
    if max_pane_text_bytes > MAX_PERSISTED_PANE_TEXT_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "pane_text_row_bytes",
            observed: max_pane_text_bytes,
            limit: MAX_PERSISTED_PANE_TEXT_BYTES,
        });
    }
    if admitted_bytes > MAX_PERSISTED_CHECKPOINT_TEXT_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "checkpoint_text_bytes",
            observed: admitted_bytes,
            limit: MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
        });
    }

    let verification = if state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX) {
        CheckpointVerification::VerifiedV2
    } else {
        CheckpointVerification::LegacyUnverified
    };
    Ok(Some(RestoreCandidateCheckpoint {
        checkpoint_at,
        verification,
    }))
}

/// Load a specific checkpoint by row ID, including pane states.
pub fn load_checkpoint_by_id(
    db_path: &str,
    checkpoint_id: i64,
) -> Result<Option<CheckpointData>, RestoreError> {
    with_query_snapshot(db_path, |conn| {
        load_checkpoint_by_id_from_conn(conn, checkpoint_id)
    })
}

/// Cx-first checkpoint lookup that keeps SQLite reads, JSON decoding, and v2
/// witness recomputation off the async worker for large checkpoints.
pub async fn load_checkpoint_by_id_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
    checkpoint_id: i64,
) -> Result<Option<CheckpointData>, RestoreError> {
    let db_path = db_path.to_string();
    crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        load_checkpoint_by_id(&db_path, checkpoint_id)
    })
    .await
    .map_err(|error| restore_blocking_error("checkpoint lookup", error))?
}

pub(crate) fn load_checkpoint_by_id_from_conn(
    conn: &Connection,
    checkpoint_id: i64,
) -> Result<Option<CheckpointData>, RestoreError> {
    let checkpoint = conn.query_row(
        "SELECT
                CASE
                    WHEN typeof(session_id) = 'text'
                     AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?2
                    THEN session_id
                END,
                checkpoint_at,
                CASE
                    WHEN typeof(checkpoint_type) = 'text'
                     AND length(CAST(checkpoint_type AS BLOB)) BETWEEN 1 AND ?3
                    THEN checkpoint_type
                END,
                CASE
                    WHEN typeof(checkpoint_role) = 'text'
                     AND length(CAST(checkpoint_role AS BLOB)) BETWEEN 1 AND ?4
                    THEN checkpoint_role
                END,
                CASE
                    WHEN typeof(state_hash) = 'text'
                     AND length(CAST(state_hash AS BLOB)) BETWEEN 1 AND ?5
                    THEN state_hash
                END,
                pane_count,
                total_bytes,
                CASE
                    WHEN metadata_json IS NULL THEN 0
                    WHEN typeof(metadata_json) = 'text' THEN 1
                    ELSE 2
                END,
                CASE
                    WHEN metadata_json IS NULL THEN 0
                    ELSE length(CAST(metadata_json AS BLOB))
                END,
                CASE
                    WHEN topology_json IS NULL THEN 0
                    WHEN typeof(topology_json) = 'text' THEN 1
                    ELSE 2
                END,
                CASE
                    WHEN topology_json IS NULL THEN 0
                    ELSE length(CAST(topology_json AS BLOB))
                END,
                restore_intent_checkpoint_id
         FROM session_checkpoints
         WHERE id = ?1",
        rusqlite::params![
            checkpoint_id,
            i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
            i64::try_from(MAX_CHECKPOINT_TYPE_BYTES).unwrap_or(i64::MAX),
            i64::try_from(MAX_CHECKPOINT_ROLE_BYTES).unwrap_or(i64::MAX),
            i64::try_from(MAX_CHECKPOINT_STATE_HASH_BYTES).unwrap_or(i64::MAX),
        ],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<i64>>(11)?,
            ))
        },
    );

    let (
        session_id,
        checkpoint_at_raw,
        checkpoint_type,
        checkpoint_role_raw,
        stored_state_hash,
        pane_count_raw,
        total_bytes_raw,
        metadata_kind,
        metadata_bytes_raw,
        topology_kind,
        topology_bytes_raw,
        restore_intent_checkpoint_id,
    ) = match checkpoint {
        Ok(c) => c,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(_error) => {
            return Err(RestoreError::Database(
                "checkpoint row lookup failed".to_string(),
            ));
        }
    };
    let session_id = session_id.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has an invalid or oversized session id"
        ))
    })?;
    let checkpoint_type = checkpoint_type.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has an invalid or oversized checkpoint type"
        ))
    })?;
    let checkpoint_role_raw = checkpoint_role_raw.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has an invalid or oversized checkpoint role"
        ))
    })?;
    let stored_state_hash = stored_state_hash.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has an invalid or oversized state hash"
        ))
    })?;
    let checkpoint_at = decode_u64(checkpoint_at_raw, "session_checkpoints.checkpoint_at")?;
    let pane_count = decode_usize(pane_count_raw, "session_checkpoints.pane_count")?;
    let total_bytes = decode_usize(total_bytes_raw, "session_checkpoints.total_bytes")?;
    if pane_count > MAX_TOPOLOGY_PANES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "pane_count",
            observed: pane_count,
            limit: MAX_TOPOLOGY_PANES,
        });
    }
    if total_bytes > MAX_PERSISTED_CHECKPOINT_TEXT_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "historical_payload_bytes",
            observed: total_bytes,
            limit: MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
        });
    }
    let metadata_bytes = decode_usize(
        metadata_bytes_raw,
        "session_checkpoints.metadata_json_bytes",
    )?;
    if !matches!(metadata_kind, 0 | 1) || (metadata_kind == 0 && metadata_bytes != 0) {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has non-text metadata"
        )));
    }
    if metadata_bytes > MAX_CHECKPOINT_METADATA_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "metadata_json_bytes",
            observed: metadata_bytes,
            limit: MAX_CHECKPOINT_METADATA_BYTES,
        });
    }
    let topology_bytes = decode_usize(
        topology_bytes_raw,
        "session_checkpoints.topology_json_bytes",
    )?;
    if !matches!(topology_kind, 0 | 1) || (topology_kind == 0 && topology_bytes != 0) {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} has non-text topology"
        )));
    }
    if topology_bytes > MAX_SNAPSHOT_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "topology_json_bytes",
            observed: topology_bytes,
            limit: MAX_SNAPSHOT_BYTES,
        });
    }
    let checkpoint_role = CheckpointRole::from_db(checkpoint_id, &checkpoint_role_raw)?;
    let expected_pane_row_count = match checkpoint_role {
        CheckpointRole::Snapshot => pane_count,
        CheckpointRole::RestoreIntent | CheckpointRole::RestoreReceipt => 0,
    };
    let preflight_row_limit = expected_pane_row_count.checked_add(1).ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} pane preflight row limit overflows usize"
        ))
    })?;

    // Preflight row count and byte sizes without returning any payload text to
    // Rust. A corrupt declaration therefore cannot become an unbounded Vec
    // capacity or cause oversized TEXT values to be materialized first.
    let (stored_pane_count_raw, pane_text_bytes_raw, max_pane_text_bytes_raw, invalid_text_rows):
        (i64, i64, i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(
                    COALESCE(length(CAST(cwd AS BLOB)), 0)
                  + COALESCE(length(CAST(command AS BLOB)), 0)
                  + COALESCE(length(CAST(env_json AS BLOB)), 0)
                  + COALESCE(length(CAST(terminal_state_json AS BLOB)), 0)
                  + COALESCE(length(CAST(agent_metadata_json AS BLOB)), 0)
                ), 0),
                COALESCE(MAX(
                    COALESCE(length(CAST(cwd AS BLOB)), 0)
                  + COALESCE(length(CAST(command AS BLOB)), 0)
                  + COALESCE(length(CAST(env_json AS BLOB)), 0)
                  + COALESCE(length(CAST(terminal_state_json AS BLOB)), 0)
                  + COALESCE(length(CAST(agent_metadata_json AS BLOB)), 0)
                ), 0),
                COALESCE(SUM(CASE
                    WHEN typeof(terminal_state_json) != 'text'
                      OR (cwd IS NOT NULL AND typeof(cwd) != 'text')
                      OR (command IS NOT NULL AND typeof(command) != 'text')
                      OR (env_json IS NOT NULL AND typeof(env_json) != 'text')
                      OR (agent_metadata_json IS NOT NULL
                          AND typeof(agent_metadata_json) != 'text')
                    THEN 1 ELSE 0 END), 0)
         FROM (
             SELECT cwd, command, env_json, terminal_state_json,
                    agent_metadata_json
             FROM mux_pane_state
             WHERE checkpoint_id = ?1
             LIMIT ?2
         ) AS bounded_pane_rows",
        rusqlite::params![
            checkpoint_id,
            i64::try_from(preflight_row_limit).unwrap_or(i64::MAX),
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let stored_pane_count = decode_usize(stored_pane_count_raw, "mux_pane_state.row_count")?;
    let pane_text_bytes = decode_usize(pane_text_bytes_raw, "mux_pane_state.text_bytes")?;
    let max_pane_text_bytes = decode_usize(
        max_pane_text_bytes_raw,
        "mux_pane_state.max_row_text_bytes",
    )?;
    if invalid_text_rows != 0 {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} stores non-text pane payload columns"
        )));
    }
    if stored_pane_count != expected_pane_row_count {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} role {} expects {expected_pane_row_count} pane rows but stores {stored_pane_count}",
            checkpoint_role.as_db_str()
        )));
    }
    if max_pane_text_bytes > MAX_PERSISTED_PANE_TEXT_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "pane_text_row_bytes",
            observed: max_pane_text_bytes,
            limit: MAX_PERSISTED_PANE_TEXT_BYTES,
        });
    }
    let admitted_checkpoint_bytes = topology_bytes
        .checked_add(metadata_bytes)
        .and_then(|bytes| bytes.checked_add(pane_text_bytes))
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} admitted text byte count overflows usize"
            ))
        })?;
    if admitted_checkpoint_bytes > MAX_PERSISTED_CHECKPOINT_TEXT_BYTES {
        return Err(RestoreError::CheckpointResourceLimit {
            checkpoint_id,
            resource: "checkpoint_text_bytes",
            observed: admitted_checkpoint_bytes,
            limit: MAX_PERSISTED_CHECKPOINT_TEXT_BYTES,
        });
    }

    // Materialize topology and metadata only after every scalar and aggregate
    // byte limit has passed. Repeat the exact bounded header identity in this
    // statement so a concurrent rewrite cannot substitute a different large
    // payload between admission and allocation. Pane rows are checked again
    // below, and the final witness recomputation rejects same-size mutation.
    let payload = conn
        .query_row(
            "SELECT metadata_json, topology_json
             FROM session_checkpoints
             WHERE id = ?1
               AND session_id = ?2
               AND checkpoint_at = ?3
               AND checkpoint_type = ?4
               AND checkpoint_role = ?5
               AND state_hash = ?6
               AND pane_count = ?7
               AND total_bytes = ?8
               AND restore_intent_checkpoint_id IS ?9
               AND CASE
                       WHEN metadata_json IS NULL THEN 0
                       WHEN typeof(metadata_json) = 'text' THEN 1
                       ELSE 2
                   END = ?10
               AND CASE
                       WHEN metadata_json IS NULL THEN 0
                       ELSE length(CAST(metadata_json AS BLOB))
                   END = ?11
               AND CASE
                       WHEN topology_json IS NULL THEN 0
                       WHEN typeof(topology_json) = 'text' THEN 1
                       ELSE 2
                   END = ?12
               AND CASE
                       WHEN topology_json IS NULL THEN 0
                       ELSE length(CAST(topology_json AS BLOB))
                   END = ?13",
            rusqlite::params![
                checkpoint_id,
                session_id.as_str(),
                checkpoint_at_raw,
                checkpoint_type.as_str(),
                checkpoint_role_raw.as_str(),
                stored_state_hash.as_str(),
                pane_count_raw,
                total_bytes_raw,
                restore_intent_checkpoint_id,
                metadata_kind,
                metadata_bytes_raw,
                topology_kind,
                topology_bytes_raw,
            ],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} changed after bounded header admission"
            ))
        })?;
    let (metadata_json, topology_json) = payload;
    if metadata_json.is_some() != (metadata_kind == 1)
        || topology_json.is_some() != (topology_kind == 1)
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} payload nullability changed after admission"
        )));
    }

    // Load at most the admitted count plus one. The WHERE predicates repeat
    // the row-local text/type admission at read time, so a concurrent rewrite
    // cannot make an oversized string cross the SQL-to-Rust boundary between
    // preflight and this statement.
    let mut stmt = conn.prepare(
        "SELECT pane_id, cwd, command, env_json, terminal_state_json,
                agent_metadata_json, scrollback_checkpoint_seq, last_output_at
         FROM mux_pane_state
         WHERE checkpoint_id = ?1
           AND typeof(terminal_state_json) = 'text'
           AND (cwd IS NULL OR typeof(cwd) = 'text')
           AND (command IS NULL OR typeof(command) = 'text')
           AND (env_json IS NULL OR typeof(env_json) = 'text')
           AND (agent_metadata_json IS NULL OR typeof(agent_metadata_json) = 'text')
           AND COALESCE(length(CAST(cwd AS BLOB)), 0)
             + COALESCE(length(CAST(command AS BLOB)), 0)
             + COALESCE(length(CAST(env_json AS BLOB)), 0)
             + COALESCE(length(CAST(terminal_state_json AS BLOB)), 0)
             + COALESCE(length(CAST(agent_metadata_json AS BLOB)), 0) <= ?2
         ORDER BY pane_id ASC, id ASC
         LIMIT ?3",
    )?;
    let row_limit = expected_pane_row_count.checked_add(1).ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} pane row limit overflows usize"
        ))
    })?;
    let mut rows = stmt.query(rusqlite::params![
        checkpoint_id,
        i64::try_from(MAX_PERSISTED_PANE_TEXT_BYTES).unwrap_or(i64::MAX),
        i64::try_from(row_limit).unwrap_or(i64::MAX),
    ])?;
    let mut persisted_panes = Vec::with_capacity(expected_pane_row_count);
    while let Some(row) = rows.next()? {
        if persisted_panes.len() == expected_pane_row_count {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} stores more than {expected_pane_row_count} pane rows"
            )));
        }
        persisted_panes.push(PersistedPaneState {
            pane_id: row.get(0)?,
            cwd: row.get(1)?,
            command: row.get(2)?,
            env_json: row.get(3)?,
            terminal_state_json: row.get(4)?,
            agent_metadata_json: row.get(5)?,
            scrollback_checkpoint_seq: row.get(6)?,
            last_output_at: row.get(7)?,
        });
    }
    let loaded_checkpoint_bytes = persisted_checkpoint_text_bytes(
        topology_json.as_deref(),
        metadata_json.as_deref(),
        &persisted_panes,
    )
    .ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} loaded text byte count overflows usize"
        ))
    })?;
    if loaded_checkpoint_bytes != admitted_checkpoint_bytes {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {checkpoint_id} changed while its bounded payload was loaded"
        )));
    }

    validate_checkpoint_structure(
        checkpoint_id,
        checkpoint_role,
        &checkpoint_type,
        pane_count,
        total_bytes,
        metadata_json.as_deref(),
        topology_json.as_deref(),
        restore_intent_checkpoint_id,
        &persisted_panes,
    )?;
    let verification = verify_checkpoint_witness(
        checkpoint_id,
        &session_id,
        checkpoint_at_raw,
        &checkpoint_type,
        checkpoint_role,
        &stored_state_hash,
        pane_count_raw,
        total_bytes_raw,
        metadata_json.as_deref(),
        topology_json.as_deref(),
        &persisted_panes,
    )?;

    let mut pane_states = Vec::with_capacity(persisted_panes.len());
    for persisted in persisted_panes {
        let pane_id = decode_u64(persisted.pane_id, "mux_pane_state.pane_id")?;
        let terminal_state = serde_json::from_str::<TerminalState>(&persisted.terminal_state_json)
            .map_err(|_error| {
                RestoreError::CorruptCheckpoint(format!(
                    "pane {pane_id} has invalid terminal_state_json"
                ))
            })?;
        let agent_metadata = match persisted.agent_metadata_json.as_deref() {
            Some(agent_json) => Some(serde_json::from_str::<AgentMetadata>(agent_json).map_err(
                |_error| {
                    RestoreError::CorruptCheckpoint(format!(
                        "pane {pane_id} has invalid agent_metadata_json"
                    ))
                },
            )?),
            None => None,
        };

        pane_states.push(RestoredPaneState {
            pane_id,
            cwd: persisted.cwd,
            command: persisted.command,
            terminal_state: Some(terminal_state),
            agent_metadata,
            scrollback_checkpoint_seq: decode_opt_u64(
                persisted.scrollback_checkpoint_seq,
                "mux_pane_state.scrollback_checkpoint_seq",
            )?,
            last_output_at: decode_opt_u64(
                persisted.last_output_at,
                "mux_pane_state.last_output_at",
            )?,
        });
    }

    Ok(Some(CheckpointData {
        checkpoint_id,
        session_id,
        checkpoint_at,
        checkpoint_type,
        checkpoint_role,
        verification,
        state_hash: stored_state_hash,
        metadata_json,
        topology_json,
        restore_intent_checkpoint_id,
        pane_count,
        total_bytes,
        pane_states,
    }))
}

#[allow(clippy::too_many_arguments)]
fn validate_checkpoint_structure(
    checkpoint_id: i64,
    role: CheckpointRole,
    checkpoint_type: &str,
    pane_count: usize,
    total_bytes: usize,
    metadata_json: Option<&str>,
    topology_json: Option<&str>,
    restore_intent_checkpoint_id: Option<i64>,
    panes: &[PersistedPaneState],
) -> Result<(), RestoreError> {
    match role {
        CheckpointRole::Snapshot => {
            if restore_intent_checkpoint_id.is_some() {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "snapshot checkpoint {checkpoint_id} unexpectedly links a restore intent"
                )));
            }
            if topology_json.is_none() {
                return Err(RestoreError::CheckpointTopologyUnavailable { checkpoint_id });
            }
            if panes.len() != pane_count {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "checkpoint {checkpoint_id} declares {pane_count} panes but stores {} pane rows",
                    panes.len()
                )));
            }
            let recomputed_total_bytes = panes.iter().try_fold(0usize, |total, pane| {
                let pane_bytes = pane
                    .terminal_state_json
                    .len()
                    .checked_add(pane.env_json.as_ref().map_or(0, String::len))
                    .and_then(|bytes| {
                        bytes.checked_add(
                            pane.agent_metadata_json.as_ref().map_or(0, String::len),
                        )
                    })
                    .ok_or_else(|| {
                        RestoreError::CorruptCheckpoint(format!(
                            "checkpoint {checkpoint_id} pane byte count overflows usize"
                        ))
                    })?;
                total.checked_add(pane_bytes).ok_or_else(|| {
                    RestoreError::CorruptCheckpoint(format!(
                        "checkpoint {checkpoint_id} total byte count overflows usize"
                    ))
                })
            })?;
            if recomputed_total_bytes != total_bytes {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "checkpoint {checkpoint_id} declares {total_bytes} payload bytes but stores {recomputed_total_bytes}"
                )));
            }
            // The loader query orders by pane_id then row id, so duplicates
            // are adjacent and need no per-checkpoint hash-set allocation.
            let mut previous_pane_id = None;
            for pane in panes {
                decode_u64(pane.pane_id, "mux_pane_state.pane_id")?;
                if previous_pane_id == Some(pane.pane_id) {
                    return Err(RestoreError::CorruptCheckpoint(format!(
                        "checkpoint {checkpoint_id} stores duplicate pane id {}",
                        pane.pane_id
                    )));
                }
                previous_pane_id = Some(pane.pane_id);
            }
        }
        CheckpointRole::RestoreIntent => {
            validate_restore_authority_row_shape(
                checkpoint_id,
                "restore intent",
                checkpoint_type,
                total_bytes,
                topology_json,
                panes,
            )?;
            let metadata = parse_restore_checkpoint_metadata(checkpoint_id, metadata_json)?;
            if pane_count != 0
                || restore_intent_checkpoint_id.is_some()
                || !metadata.old_to_new.is_empty()
            {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore intent {checkpoint_id} carries a pane count, pane mapping, or outcome link"
                )));
            }
            if !persisted_restore_intent_is_valid(&metadata) {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore intent {checkpoint_id} has invalid or incomplete intent metadata"
                )));
            }
            if !matches!(
                metadata.restore_attempt.as_ref(),
                Some(PersistedRestoreAttempt::Intent {
                    source_pane_count: Some(_),
                    ..
                })
            ) {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore intent {checkpoint_id} lacks source pane-count authority"
                )));
            }
        }
        CheckpointRole::RestoreReceipt => {
            validate_restore_authority_row_shape(
                checkpoint_id,
                "restore receipt",
                checkpoint_type,
                total_bytes,
                topology_json,
                panes,
            )?;
            let metadata = parse_restore_checkpoint_metadata(checkpoint_id, metadata_json)?;
            if metadata.old_to_new.len() != pane_count {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore receipt {checkpoint_id} declares {pane_count} mappings but metadata stores {}",
                    metadata.old_to_new.len()
                )));
            }
            match metadata.restore_attempt.as_ref() {
                None => {
                    if restore_intent_checkpoint_id.is_some() {
                        return Err(RestoreError::CorruptCheckpoint(format!(
                            "legacy restore receipt {checkpoint_id} unexpectedly links an intent"
                        )));
                    }
                }
                // Schema-v37 encoded intents as receipts. Keep them readable
                // solely so migration/reconciliation can inspect their exact
                // witness; new writers use the explicit restore_intent role.
                Some(PersistedRestoreAttempt::Intent { .. }) => {
                    if restore_intent_checkpoint_id.is_some()
                        || !metadata.old_to_new.is_empty()
                        || !persisted_restore_intent_is_valid(&metadata)
                    {
                        return Err(RestoreError::CorruptCheckpoint(format!(
                            "legacy restore intent {checkpoint_id} has an outcome link or pane mappings"
                        )));
                    }
                }
                Some(PersistedRestoreAttempt::Outcome {
                    intent_checkpoint_id,
                    ..
                }) => {
                    if restore_intent_checkpoint_id != Some(*intent_checkpoint_id) {
                        return Err(RestoreError::CorruptCheckpoint(format!(
                            "restore outcome {checkpoint_id} does not bind metadata intent {intent_checkpoint_id} through its relational link"
                        )));
                    }
                    validate_restore_outcome_metadata(checkpoint_id, &metadata)?;
                }
            }
        }
    }
    Ok(())
}

fn validate_restore_authority_row_shape(
    checkpoint_id: i64,
    label: &str,
    checkpoint_type: &str,
    total_bytes: usize,
    topology_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<(), RestoreError> {
    if checkpoint_type != "startup" {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "{label} {checkpoint_id} has checkpoint type {checkpoint_type}, expected startup"
        )));
    }
    if total_bytes != 0 {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "{label} {checkpoint_id} declares {total_bytes} payload bytes"
        )));
    }
    if topology_json.is_some() {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "{label} {checkpoint_id} unexpectedly carries topology"
        )));
    }
    if !panes.is_empty() {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "{label} {checkpoint_id} unexpectedly stores pane-state rows"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRestoreCheckpointMetadata {
    old_to_new: BTreeMap<String, u64>,
    #[serde(default)]
    restore_attempt: Option<PersistedRestoreAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum PersistedRestoreAttempt {
    Intent {
        source_checkpoint_id: i64,
        source_checkpoint_at: i64,
        source_checkpoint_role: String,
        source_state_hash: String,
        #[serde(default)]
        source_pane_count: Option<usize>,
    },
    Outcome {
        #[serde(default = "legacy_restore_outcome_evidence_version")]
        evidence_version: u8,
        intent_checkpoint_id: i64,
        intent_checkpoint_at: i64,
        intent_state_hash: String,
        source_checkpoint_id: i64,
        source_checkpoint_at: i64,
        source_checkpoint_role: String,
        source_state_hash: String,
        expected_panes: usize,
        mapped_panes: usize,
        reported_layout_failures: usize,
        #[serde(default)]
        failed_source_pane_ids: Option<Vec<u64>>,
        #[serde(default)]
        unexpected_mapping_count: Option<usize>,
        #[serde(default)]
        unexpected_failure_count: Option<usize>,
        #[serde(default)]
        duplicate_target_source_pane_ids: Option<Vec<u64>>,
        layout_complete: bool,
        scrollback_requested: bool,
        scrollback_complete: bool,
        scrollback_failures: usize,
        scrollback_skipped: usize,
        scrollback_global_error: bool,
        process_plan_evaluated: bool,
        process_plans_total: usize,
        process_plans_settled: usize,
        process_interrupted: bool,
        attempt_interrupted: bool,
        interruption_phase: Option<RestoreInterruptionPhase>,
        #[serde(default)]
        interruption_reason: Option<RestoreInterruptionReason>,
        process_failed: usize,
        process_manual: usize,
        process_skipped: usize,
    },
}

fn persisted_restore_intent_is_valid(metadata: &PersistedRestoreCheckpointMetadata) -> bool {
    matches!(
        metadata.restore_attempt.as_ref(),
        Some(PersistedRestoreAttempt::Intent {
            source_checkpoint_id,
            source_checkpoint_at,
            source_checkpoint_role,
            source_state_hash,
            ..
        }) if *source_checkpoint_id > 0
            && *source_checkpoint_at >= 0
            && source_checkpoint_role == CHECKPOINT_ROLE_SNAPSHOT
            && !source_state_hash.is_empty()
    )
}

fn parse_restore_checkpoint_metadata(
    checkpoint_id: i64,
    metadata_json: Option<&str>,
) -> Result<PersistedRestoreCheckpointMetadata, RestoreError> {
    let metadata_json = metadata_json.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "restore authority checkpoint {checkpoint_id} has NULL metadata_json"
        ))
    })?;
    let metadata: PersistedRestoreCheckpointMetadata =
        serde_json::from_str(metadata_json).map_err(|_error| {
            RestoreError::CorruptCheckpoint(format!(
                "restore authority checkpoint {checkpoint_id} has invalid metadata_json"
            ))
        })?;
    let mut old_ids = HashSet::with_capacity(metadata.old_to_new.len());
    let mut new_ids = HashSet::with_capacity(metadata.old_to_new.len());
    for (old_id, &new_id) in &metadata.old_to_new {
        let parsed_old_id = old_id.parse::<u64>().map_err(|_| {
            RestoreError::CorruptCheckpoint(format!(
                "restore authority checkpoint {checkpoint_id} has a non-u64 old pane id"
            ))
        })?;
        if parsed_old_id.to_string() != *old_id || !old_ids.insert(parsed_old_id) {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "restore authority checkpoint {checkpoint_id} contains a non-canonical or duplicate encoding of old pane {parsed_old_id}"
            )));
        }
        if !new_ids.insert(new_id) {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "restore authority checkpoint {checkpoint_id} maps more than one old pane to new pane {new_id}"
            )));
        }
    }
    Ok(metadata)
}

fn validate_restore_outcome_metadata(
    checkpoint_id: i64,
    metadata: &PersistedRestoreCheckpointMetadata,
) -> Result<(), RestoreError> {
    let Some(PersistedRestoreAttempt::Outcome {
        evidence_version,
        intent_checkpoint_id,
        intent_checkpoint_at,
        intent_state_hash,
        source_checkpoint_id,
        source_checkpoint_at,
        source_checkpoint_role,
        source_state_hash,
        expected_panes,
        mapped_panes,
        reported_layout_failures,
        failed_source_pane_ids,
        unexpected_mapping_count,
        unexpected_failure_count,
        duplicate_target_source_pane_ids,
        layout_complete,
        scrollback_requested,
        scrollback_complete,
        scrollback_failures,
        scrollback_skipped,
        scrollback_global_error,
        process_plan_evaluated,
        process_plans_total,
        process_plans_settled,
        process_interrupted,
        attempt_interrupted,
        interruption_phase,
        interruption_reason,
        process_failed,
        process_manual,
        process_skipped,
    }) = metadata.restore_attempt.as_ref()
    else {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {checkpoint_id} has no outcome metadata"
        )));
    };
    let disposition_count = process_failed
        .checked_add(*process_manual)
        .and_then(|count| count.checked_add(*process_skipped))
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "restore outcome {checkpoint_id} overflows its process disposition count"
            ))
        })?;
    let v3_source_identity_consistent = if *evidence_version == RESTORE_OUTCOME_EVIDENCE_VERSION {
        failed_source_pane_ids.as_ref().is_some_and(|failed_ids| {
            mapped_panes
                .checked_add(failed_ids.len())
                .is_some_and(|covered_count| covered_count == *expected_panes)
                && metadata.old_to_new.keys().all(|source_id| {
                    source_id
                        .parse::<u64>()
                        .is_ok_and(|source_id| failed_ids.binary_search(&source_id).is_err())
                })
        })
    } else {
        true
    };
    let process_state_consistent = if !*layout_complete {
        !*process_plan_evaluated
            && *process_plans_total == 0
            && *process_plans_settled == 0
            && disposition_count == 0
            && !*process_interrupted
    } else if *expected_panes == 0 {
        !*process_plan_evaluated
            && *process_plans_total == 0
            && *process_plans_settled == 0
            && disposition_count == 0
            && !*process_interrupted
    } else if *process_plan_evaluated {
        *process_plans_total == *expected_panes
    } else {
        // Cancellation may land after layout has settled but before process
        // disposition begins. That truthful partial receipt has no process
        // inventory, and is admissible only as an interrupted attempt.
        *attempt_interrupted
            && *process_plans_total == 0
            && *process_plans_settled == 0
            && disposition_count == 0
            && !*process_interrupted
    };
    let interruption_shape_consistent = match *evidence_version {
        1 => {
            interruption_phase.is_some() == *attempt_interrupted
                && interruption_reason.is_none()
                && failed_source_pane_ids.is_none()
                && unexpected_mapping_count.is_none()
                && unexpected_failure_count.is_none()
                && duplicate_target_source_pane_ids.is_none()
        }
        RESTORE_OUTCOME_REASON_EVIDENCE_VERSION => {
            interruption_phase.is_some() == *attempt_interrupted
                && interruption_reason.is_some() == *attempt_interrupted
                && failed_source_pane_ids.is_none()
                && unexpected_mapping_count.is_none()
                && unexpected_failure_count.is_none()
                && duplicate_target_source_pane_ids.is_none()
        }
        RESTORE_OUTCOME_EVIDENCE_VERSION => {
            let failed_ids_are_canonical = failed_source_pane_ids
                .as_ref()
                .is_some_and(|pane_ids| {
                    pane_ids.len() >= *reported_layout_failures
                        && pane_ids.len() <= *expected_panes
                        && pane_ids.windows(2).all(|pair| pair[0] < pair[1])
                });
            let duplicate_ids_are_canonical = duplicate_target_source_pane_ids
                .as_ref()
                .is_some_and(|pane_ids| {
                    pane_ids.windows(2).all(|pair| pair[0] < pair[1])
                        && failed_source_pane_ids.as_ref().is_some_and(|failed_ids| {
                            pane_ids
                                .iter()
                                .all(|pane_id| failed_ids.binary_search(pane_id).is_ok())
                        })
                });
            interruption_phase.is_some() == *attempt_interrupted
                && interruption_reason.is_some() == *attempt_interrupted
                && failed_ids_are_canonical
                && unexpected_mapping_count.is_some()
                && unexpected_failure_count.is_some()
                && duplicate_ids_are_canonical
        }
        _ => false,
    };
    let interruption_phase_consistent = match interruption_phase {
        None => !*attempt_interrupted,
        Some(RestoreInterruptionPhase::LayoutRestoration) => {
            *attempt_interrupted
                && !*layout_complete
                && !*process_plan_evaluated
                && *process_plans_total == 0
                && *process_plans_settled == 0
                && !*process_interrupted
        }
        Some(
            RestoreInterruptionPhase::PostLayoutCheckpoint
            | RestoreInterruptionPhase::PreProcessDispositionCheckpoint,
        ) => {
            *attempt_interrupted
                && !*process_plan_evaluated
                && *process_plans_total == 0
                && *process_plans_settled == 0
                && !*process_interrupted
        }
        Some(RestoreInterruptionPhase::ProcessDispositionEvaluation) => {
            *attempt_interrupted
                && *layout_complete
                && *process_plan_evaluated
                && *process_interrupted
                && *process_plans_total == *expected_panes
                && *process_plans_settled < *process_plans_total
        }
        Some(RestoreInterruptionPhase::PostProcessDispositionCheckpoint) => {
            *attempt_interrupted
                && *layout_complete
                && *process_plan_evaluated
                && !*process_interrupted
                && *process_plans_total == *expected_panes
                && *process_plans_settled == *process_plans_total
        }
    };
    // The disposition-only engine has no launched-success category: every
    // settled plan must appear exactly once as failed, manual, or skipped.
    let invalid = *intent_checkpoint_id <= 0
        || *intent_checkpoint_at < 0
        || intent_state_hash.is_empty()
        || *source_checkpoint_id <= 0
        || *source_checkpoint_at < 0
        || source_checkpoint_role != CHECKPOINT_ROLE_SNAPSHOT
        || source_state_hash.is_empty()
        || *mapped_panes != metadata.old_to_new.len()
        || *mapped_panes > *expected_panes
        || *reported_layout_failures > *expected_panes
        || (*evidence_version == RESTORE_OUTCOME_EVIDENCE_VERSION && *process_failed != 0)
        || (*layout_complete
            && (failed_source_pane_ids.as_ref().is_some_and(|ids| !ids.is_empty())
                || unexpected_mapping_count.is_some_and(|count| count != 0)
                || unexpected_failure_count.is_some_and(|count| count != 0)
                || duplicate_target_source_pane_ids
                    .as_ref()
                    .is_some_and(|ids| !ids.is_empty())))
        || (*layout_complete
            && (*mapped_panes != *expected_panes || *reported_layout_failures != 0))
        || (*scrollback_complete
            && (*scrollback_failures != 0
                || *scrollback_skipped != 0
                || *scrollback_global_error))
        || (!*scrollback_requested
            && (*scrollback_failures != 0
                || *scrollback_skipped != 0
                || *scrollback_global_error))
        || *process_plans_settled > *process_plans_total
        || disposition_count != *process_plans_settled
        || *process_plan_evaluated != (*process_plans_total > 0)
        || !process_state_consistent
        || (*process_interrupted && !*attempt_interrupted)
        || !v3_source_identity_consistent
        || !interruption_shape_consistent
        || !interruption_phase_consistent;
    if invalid {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {checkpoint_id} has internally inconsistent evidence"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_checkpoint_witness(
    checkpoint_id: i64,
    session_id: &str,
    checkpoint_at: i64,
    checkpoint_type: &str,
    role: CheckpointRole,
    stored_state_hash: &str,
    pane_count: i64,
    total_bytes: i64,
    metadata_json: Option<&str>,
    topology_json: Option<&str>,
    panes: &[PersistedPaneState],
) -> Result<CheckpointVerification, RestoreError> {
    let is_v2 = stored_state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX)
        || stored_state_hash.starts_with(RESTORE_INTENT_WITNESS_PREFIX)
        || stored_state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX);
    if is_v2 {
        let expected_prefix = match role {
            CheckpointRole::Snapshot => SNAPSHOT_WITNESS_PREFIX,
            CheckpointRole::RestoreIntent => RESTORE_INTENT_WITNESS_PREFIX,
            CheckpointRole::RestoreReceipt => RESTORE_RECEIPT_WITNESS_PREFIX,
        };
        if !stored_state_hash.starts_with(expected_prefix) {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} uses a witness prefix for the wrong checkpoint role"
            )));
        }
        let recomputed = checkpoint_witness(
            role.as_db_str(),
            session_id,
            checkpoint_id,
            checkpoint_at,
            checkpoint_type,
            pane_count,
            total_bytes,
            metadata_json,
            topology_json,
            panes,
        )
        .map_err(|_error| {
            RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} witness projection is invalid"
            ))
        })?;
        if recomputed != stored_state_hash {
            return Err(RestoreError::StateHashMismatch {
                checkpoint_id,
                session_id: session_id.to_string(),
                stored: stored_state_hash.to_string(),
                recomputed,
            });
        }
        return Ok(CheckpointVerification::VerifiedV2);
    }

    let is_legacy = match role {
        CheckpointRole::Snapshot => {
            stored_state_hash.len() == 16
                && stored_state_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        }
        CheckpointRole::RestoreIntent => false,
        CheckpointRole::RestoreReceipt => {
            stored_state_hash == "restore"
                || (stored_state_hash.len() == 16
                    && stored_state_hash
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit()))
        }
    };
    if is_legacy {
        return Ok(CheckpointVerification::LegacyUnverified);
    }

    Err(RestoreError::CorruptCheckpoint(format!(
        "checkpoint {checkpoint_id} has an unknown state_hash encoding"
    )))
}

/// Test-only exact-receipt clean transition for legacy fixture coverage.
/// Production restore bookkeeping persists an unclean receipt first and binds
/// that exact receipt only after process disposition evaluation settles.
#[cfg(test)]
fn mark_session_restored(db_path: &str, session_id: &str) -> Result<(), RestoreError> {
    let conn = open_conn(db_path)?;
    let checkpoint_id = conn
        .query_row(
            "SELECT id
             FROM session_checkpoints
             WHERE session_id = ?1
             ORDER BY id DESC
             LIMIT 1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_error| {
            RestoreError::Bookkeeping(format!(
                "cannot mark session {session_id} restored without an exact checkpoint receipt"
            ))
        })?;
    let updated = conn.execute(
        "UPDATE mux_sessions
         SET shutdown_clean = 1, clean_checkpoint_id = ?2
         WHERE session_id = ?1",
        rusqlite::params![session_id, checkpoint_id],
    )?;
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "cannot bind clean receipt to missing session {session_id}"
        )));
    }
    Ok(())
}

fn prepare_restore_mapping(
    pane_id_map: &HashMap<u64, u64>,
) -> Result<(BTreeMap<String, u64>, i64), RestoreError> {
    if pane_id_map.len() > MAX_TOPOLOGY_PANES {
        return Err(RestoreError::Bookkeeping(format!(
            "restore pane mapping exceeds the {MAX_TOPOLOGY_PANES}-pane topology limit"
        )));
    }
    let pane_count = i64::try_from(pane_id_map.len()).map_err(|_| {
        RestoreError::Bookkeeping("restore pane mapping count exceeds SQLite INTEGER".to_string())
    })?;
    let mut ordered_mapping = BTreeMap::new();
    let mut new_ids = HashSet::with_capacity(pane_id_map.len());
    for (&old_id, &new_id) in pane_id_map {
        if !new_ids.insert(new_id) {
            return Err(RestoreError::Bookkeeping(format!(
                "restore mapping assigns new pane {new_id} more than once"
            )));
        }
        ordered_mapping.insert(old_id.to_string(), new_id);
    }
    Ok((ordered_mapping, pane_count))
}

#[cfg(test)]
fn prepare_restore_receipt_metadata(
    pane_id_map: &HashMap<u64, u64>,
) -> Result<(String, i64), RestoreError> {
    let (ordered_mapping, pane_count) = prepare_restore_mapping(pane_id_map)?;
    let metadata = serde_json::json!({ "old_to_new": ordered_mapping });
    let metadata_json = canonical_json_string(&metadata).map_err(|_error| {
        RestoreError::Bookkeeping(
            "failed to canonicalize restore receipt metadata".to_string(),
        )
    })?;
    Ok((metadata_json, pane_count))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreIntentSource {
    checkpoint_id: i64,
    checkpoint_at: u64,
    checkpoint_role: CheckpointRole,
    state_hash: String,
    pane_count: usize,
}

fn prepare_restore_intent_metadata(source: &RestoreIntentSource) -> Result<String, RestoreError> {
    let metadata = serde_json::json!({
        "old_to_new": {},
        "restore_attempt": {
            "phase": "intent",
            "source_checkpoint_id": source.checkpoint_id,
            "source_checkpoint_at": source.checkpoint_at,
            "source_checkpoint_role": source.checkpoint_role.as_db_str(),
            "source_state_hash": source.state_hash.as_str(),
            "source_pane_count": source.pane_count,
        }
    });
    canonical_json_string(&metadata).map_err(|_error| {
        RestoreError::Bookkeeping(
            "failed to canonicalize restore intent metadata".to_string(),
        )
    })
}

/// Insert one restore receipt and replace its transaction-local placeholder
/// only after SQLite assigns the row ID that the v2 witness binds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreReceipt {
    checkpoint_id: i64,
    checkpoint_at: i64,
    state_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreOutcomeEvidence {
    evidence_version: u8,
    intent: RestoreReceipt,
    source: RestoreIntentSource,
    expected_panes: usize,
    mapped_panes: usize,
    reported_layout_failures: usize,
    failed_source_pane_ids: Vec<u64>,
    unexpected_mapping_count: usize,
    unexpected_failure_count: usize,
    duplicate_target_source_pane_ids: Vec<u64>,
    layout_complete: bool,
    scrollback_requested: bool,
    scrollback_complete: bool,
    scrollback_failures: usize,
    scrollback_skipped: usize,
    scrollback_global_error: bool,
    process_plan_evaluated: bool,
    process_plans_total: usize,
    process_plans_settled: usize,
    process_interrupted: bool,
    attempt_interrupted: bool,
    interruption_phase: Option<RestoreInterruptionPhase>,
    interruption_reason: Option<RestoreInterruptionReason>,
    process_failed: usize,
    process_manual: usize,
    process_skipped: usize,
}

fn restore_outcome_evidence_is_complete(evidence: &RestoreOutcomeEvidence) -> bool {
    evidence.layout_complete
        && evidence.mapped_panes == evidence.expected_panes
        && evidence.reported_layout_failures == 0
        && evidence.failed_source_pane_ids.is_empty()
        && evidence.unexpected_mapping_count == 0
        && evidence.unexpected_failure_count == 0
        && evidence.duplicate_target_source_pane_ids.is_empty()
        && evidence.scrollback_complete
        && evidence.scrollback_failures == 0
        && evidence.scrollback_skipped == 0
        && !evidence.scrollback_global_error
        && evidence.process_plan_evaluated == (evidence.expected_panes > 0)
        && evidence.process_plans_total == evidence.expected_panes
        && evidence.process_plans_settled == evidence.process_plans_total
        && !evidence.process_interrupted
        && !evidence.attempt_interrupted
        && evidence.interruption_phase.is_none()
        && evidence.interruption_reason.is_none()
        && evidence.process_failed == 0
}

fn persisted_restore_outcome_is_complete(
    metadata: &PersistedRestoreCheckpointMetadata,
) -> bool {
    matches!(
        metadata.restore_attempt.as_ref(),
        Some(PersistedRestoreAttempt::Outcome {
            evidence_version,
            expected_panes,
            mapped_panes,
            reported_layout_failures: 0,
            failed_source_pane_ids,
            unexpected_mapping_count,
            unexpected_failure_count,
            duplicate_target_source_pane_ids,
            layout_complete: true,
            scrollback_complete: true,
            scrollback_failures: 0,
            scrollback_skipped: 0,
            scrollback_global_error: false,
            process_plan_evaluated,
            process_plans_total,
            process_plans_settled,
            process_interrupted: false,
            attempt_interrupted: false,
            interruption_phase: None,
            interruption_reason: None,
            process_failed: 0,
            ..
        }) if mapped_panes == expected_panes
            && match *evidence_version {
                1 | RESTORE_OUTCOME_REASON_EVIDENCE_VERSION => {
                    failed_source_pane_ids.is_none()
                        && unexpected_mapping_count.is_none()
                        && unexpected_failure_count.is_none()
                        && duplicate_target_source_pane_ids.is_none()
                }
                RESTORE_OUTCOME_EVIDENCE_VERSION => {
                    failed_source_pane_ids.as_ref().is_some_and(Vec::is_empty)
                        && *unexpected_mapping_count == Some(0)
                        && *unexpected_failure_count == Some(0)
                        && duplicate_target_source_pane_ids
                            .as_ref()
                            .is_some_and(Vec::is_empty)
                }
                _ => false,
            }
            && *process_plan_evaluated == (*expected_panes > 0)
            && process_plans_total == expected_panes
            && process_plans_settled == process_plans_total
    )
}

fn restore_checkpoint_metadata_from_conn(
    conn: &Connection,
    checkpoint_id: i64,
    session_id: &str,
) -> Result<PersistedRestoreCheckpointMetadata, RestoreError> {
    let metadata_json = conn
        .query_row(
            "SELECT metadata_json
             FROM session_checkpoints
             WHERE id = ?1 AND session_id = ?2",
            rusqlite::params![checkpoint_id, session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "restore authority checkpoint {checkpoint_id} is missing from its session"
            ))
        })?;
    parse_restore_checkpoint_metadata(checkpoint_id, metadata_json.as_deref())
}

/// Validate the complete same-session causal chain behind a restore outcome.
///
/// While an attempt is unresolved, retention guarantees that the source row is
/// present and this function verifies its exact pane IDs. After resolution the
/// source snapshot may be pruned, so the intent's witnessed source pane count
/// remains the durable comparison point for later clean-authority assessment.
fn validate_restore_authority_chain(
    conn: &Connection,
    session_id: &str,
    outcome: &CheckpointData,
    expected_lifecycle_status: &'static str,
    require_source_row: bool,
) -> Result<i64, RestoreError> {
    if outcome.session_id != session_id
        || outcome.checkpoint_role != CheckpointRole::RestoreReceipt
        || outcome.checkpoint_type != "startup"
        || outcome.verification != CheckpointVerification::VerifiedV2
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} is not exact verified startup authority",
            outcome.checkpoint_id
        )));
    }
    let linked_intent_id = outcome.restore_intent_checkpoint_id.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} has no causal intent link",
            outcome.checkpoint_id
        ))
    })?;
    let outcome_metadata =
        restore_checkpoint_metadata_from_conn(conn, outcome.checkpoint_id, session_id)?;
    validate_restore_outcome_metadata(outcome.checkpoint_id, &outcome_metadata)?;
    if !persisted_restore_outcome_is_complete(&outcome_metadata) {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} is incomplete",
            outcome.checkpoint_id
        )));
    }
    let Some(PersistedRestoreAttempt::Outcome {
        intent_checkpoint_id: metadata_intent_id,
        intent_checkpoint_at,
        intent_state_hash,
        source_checkpoint_id: outcome_source_id,
        source_checkpoint_at: outcome_source_at,
        source_checkpoint_role: outcome_source_role,
        source_state_hash: outcome_source_hash,
        expected_panes,
        ..
    }) = outcome_metadata.restore_attempt.as_ref()
    else {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} lacks outcome authority metadata",
            outcome.checkpoint_id
        )));
    };
    if linked_intent_id != *metadata_intent_id {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} disagrees with its causal intent link",
            outcome.checkpoint_id
        )));
    }

    let intent = load_checkpoint_by_id_from_conn(conn, linked_intent_id)?.ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} has a missing causal intent",
            outcome.checkpoint_id
        ))
    })?;
    if intent.session_id != session_id
        || intent.checkpoint_role != CheckpointRole::RestoreIntent
        || intent.checkpoint_type != "startup"
        || intent.verification != CheckpointVerification::VerifiedV2
        || intent.checkpoint_at
            != decode_u64(
                *intent_checkpoint_at,
                "restore_attempt.intent_checkpoint_at",
            )?
        || intent.state_hash != *intent_state_hash
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} does not match its exact causal intent",
            outcome.checkpoint_id
        )));
    }
    let intent_metadata =
        restore_checkpoint_metadata_from_conn(conn, linked_intent_id, session_id)?;
    let Some(PersistedRestoreAttempt::Intent {
        source_checkpoint_id: intent_source_id,
        source_checkpoint_at: intent_source_at,
        source_checkpoint_role: intent_source_role,
        source_state_hash: intent_source_hash,
        source_pane_count,
    }) = intent_metadata.restore_attempt.as_ref()
    else {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore intent {linked_intent_id} lacks intent authority metadata"
        )));
    };
    let source_pane_count = (*source_pane_count).ok_or_else(|| {
        RestoreError::CorruptCheckpoint(format!(
            "restore intent {linked_intent_id} lacks source pane-count authority"
        ))
    })?;
    if !persisted_restore_intent_is_valid(&intent_metadata)
        || *intent_source_id >= linked_intent_id
        || linked_intent_id >= outcome.checkpoint_id
        || *outcome_source_id != *intent_source_id
        || *outcome_source_at != *intent_source_at
        || outcome_source_role != intent_source_role
        || outcome_source_hash != intent_source_hash
        || *expected_panes != source_pane_count
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} disagrees with its intent source authority",
            outcome.checkpoint_id
        )));
    }

    let lifecycle: Option<RestoreLifecycleAuthorityRow> = conn
        .query_row(
            "SELECT source_checkpoint_id, outcome_checkpoint_id,
                    CASE
                        WHEN typeof(status) = 'text'
                         AND status IN (
                             'intent',
                             'outcome_complete',
                             'resolved',
                             'reconciliation_required'
                         )
                        THEN status
                    END,
                    created_at, resolved_at
             FROM restore_attempt_lifecycle
             WHERE intent_checkpoint_id = ?1 AND session_id = ?2",
            rusqlite::params![linked_intent_id, session_id],
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
        .optional()?;
    let Some((
        lifecycle_source_id,
        lifecycle_outcome_id,
        Some(lifecycle_status),
        lifecycle_created_at,
        resolved_at,
    )) = lifecycle
    else {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} has no same-session lifecycle authority",
            outcome.checkpoint_id
        )));
    };
    let expected_resolved_at = expected_lifecycle_status == "resolved";
    if lifecycle_source_id != *intent_source_id
        || lifecycle_outcome_id != Some(outcome.checkpoint_id)
        || lifecycle_status != expected_lifecycle_status
        || lifecycle_created_at != *intent_checkpoint_at
        || resolved_at.is_some() != expected_resolved_at
        || resolved_at.is_some_and(|resolved_at| resolved_at < lifecycle_created_at)
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "restore outcome {} has inconsistent lifecycle authority",
            outcome.checkpoint_id
        )));
    }

    match load_checkpoint_by_id_from_conn(conn, *intent_source_id)? {
        Some(source) => {
            if source.session_id != session_id
                || source.checkpoint_role != CheckpointRole::Snapshot
                || source.checkpoint_at
                    != decode_u64(
                        *intent_source_at,
                        "restore_attempt.source_checkpoint_at",
                    )?
                || source.state_hash != *intent_source_hash
                || source.pane_count != source_pane_count
            {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore intent {linked_intent_id} does not match its exact source snapshot"
                )));
            }
            let mapped_source_ids = outcome_metadata
                .old_to_new
                .keys()
                .map(|pane_id| pane_id.parse::<u64>())
                .collect::<Result<HashSet<_>, _>>()
                .map_err(|_error| {
                    RestoreError::CorruptCheckpoint(format!(
                        "restore outcome {} has invalid source pane identifiers",
                        outcome.checkpoint_id
                    ))
                })?;
            let source_pane_ids = source
                .pane_states
                .iter()
                .map(|pane| pane.pane_id)
                .collect::<HashSet<_>>();
            if mapped_source_ids != source_pane_ids {
                return Err(RestoreError::CorruptCheckpoint(format!(
                    "restore outcome {} does not map the exact source pane set",
                    outcome.checkpoint_id
                )));
            }
        }
        None if require_source_row => {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "restore intent {linked_intent_id} lost its unresolved source snapshot"
            )));
        }
        None => {}
    }

    Ok(linked_intent_id)
}

fn prepare_restore_outcome_metadata(
    pane_id_map: &HashMap<u64, u64>,
    evidence: &RestoreOutcomeEvidence,
) -> Result<(String, i64, PersistedRestoreCheckpointMetadata), RestoreError> {
    let (old_to_new, pane_count) = prepare_restore_mapping(pane_id_map)?;
    let metadata = serde_json::json!({
        "old_to_new": old_to_new,
        "restore_attempt": {
            "phase": "outcome",
            "evidence_version": evidence.evidence_version,
            "intent_checkpoint_id": evidence.intent.checkpoint_id,
            "intent_checkpoint_at": evidence.intent.checkpoint_at,
            "intent_state_hash": evidence.intent.state_hash.as_str(),
            "source_checkpoint_id": evidence.source.checkpoint_id,
            "source_checkpoint_at": evidence.source.checkpoint_at,
            "source_checkpoint_role": evidence.source.checkpoint_role.as_db_str(),
            "source_state_hash": evidence.source.state_hash.as_str(),
            "expected_panes": evidence.expected_panes,
            "mapped_panes": evidence.mapped_panes,
            "reported_layout_failures": evidence.reported_layout_failures,
            "failed_source_pane_ids": evidence.failed_source_pane_ids,
            "unexpected_mapping_count": evidence.unexpected_mapping_count,
            "unexpected_failure_count": evidence.unexpected_failure_count,
            "duplicate_target_source_pane_ids": evidence.duplicate_target_source_pane_ids,
            "layout_complete": evidence.layout_complete,
            "scrollback_requested": evidence.scrollback_requested,
            "scrollback_complete": evidence.scrollback_complete,
            "scrollback_failures": evidence.scrollback_failures,
            "scrollback_skipped": evidence.scrollback_skipped,
            "scrollback_global_error": evidence.scrollback_global_error,
            "process_plan_evaluated": evidence.process_plan_evaluated,
            "process_plans_total": evidence.process_plans_total,
            "process_plans_settled": evidence.process_plans_settled,
            "process_interrupted": evidence.process_interrupted,
            "attempt_interrupted": evidence.attempt_interrupted,
            "interruption_phase": evidence.interruption_phase,
            "interruption_reason": evidence.interruption_reason,
            "process_failed": evidence.process_failed,
            "process_manual": evidence.process_manual,
            "process_skipped": evidence.process_skipped,
        }
    });
    let metadata_json = canonical_json_string(&metadata).map_err(|_error| {
        RestoreError::Bookkeeping(
            "failed to canonicalize restore outcome metadata".to_string(),
        )
    })?;
    let parsed = parse_restore_checkpoint_metadata(
        evidence.intent.checkpoint_id,
        Some(&metadata_json),
    )?;
    validate_restore_outcome_metadata(evidence.intent.checkpoint_id, &parsed)?;
    Ok((metadata_json, pane_count, parsed))
}

fn insert_restore_receipt(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    now_ms: i64,
    metadata_json: &str,
    pane_count: i64,
    restore_intent_checkpoint_id: Option<i64>,
) -> Result<RestoreReceipt, RestoreError> {
    insert_restore_authority_checkpoint(
        tx,
        session_id,
        now_ms,
        metadata_json,
        pane_count,
        CheckpointRole::RestoreReceipt,
        restore_intent_checkpoint_id,
    )
}

fn insert_restore_intent(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    now_ms: i64,
    metadata_json: &str,
) -> Result<RestoreReceipt, RestoreError> {
    insert_restore_authority_checkpoint(
        tx,
        session_id,
        now_ms,
        metadata_json,
        0,
        CheckpointRole::RestoreIntent,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_restore_authority_checkpoint(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    now_ms: i64,
    metadata_json: &str,
    pane_count: i64,
    role: CheckpointRole,
    restore_intent_checkpoint_id: Option<i64>,
) -> Result<RestoreReceipt, RestoreError> {
    if role == CheckpointRole::Snapshot {
        return Err(RestoreError::Bookkeeping(
            "restore authority insertion cannot create snapshot rows".to_string(),
        ));
    }
    if role == CheckpointRole::RestoreIntent && restore_intent_checkpoint_id.is_some() {
        return Err(RestoreError::Bookkeeping(
            "restore intent cannot carry an outcome parent link".to_string(),
        ));
    }
    tx.execute(
        "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
          total_bytes, metadata_json, checkpoint_role, topology_json,
          restore_intent_checkpoint_id)
         VALUES (?1, ?2, 'startup', 'pending:restore-authority', ?3, 0, ?4,
                 ?5, NULL, ?6)",
        rusqlite::params![
            session_id,
            now_ms,
            pane_count,
            metadata_json,
            role.as_db_str(),
            restore_intent_checkpoint_id,
        ],
    )?;
    let checkpoint_id = tx.last_insert_rowid();
    let state_hash = checkpoint_witness(
        role.as_db_str(),
        session_id,
        checkpoint_id,
        now_ms,
        "startup",
        pane_count,
        0,
        Some(metadata_json),
        None,
        &[],
    )
    .map_err(|_error| {
        RestoreError::Bookkeeping(format!("failed to compute {role} witness"))
    })?;
    let updated = tx.execute(
        "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
        rusqlite::params![state_hash, checkpoint_id],
    )?;
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "{role} witness update changed {updated} rows for checkpoint {checkpoint_id}"
        )));
    }
    Ok(RestoreReceipt {
        checkpoint_id,
        checkpoint_at: now_ms,
        state_hash,
    })
}

/// Record the pane ID mapping from restore in a new startup checkpoint.
#[cfg(test)]
fn save_restore_checkpoint(
    db_path: &str,
    session_id: &str,
    pane_id_map: &HashMap<u64, u64>,
) -> Result<i64, RestoreError> {
    let mut conn = open_conn(db_path)?;
    // br-ft-0n4nx: route through the shared clock-anomaly helper so
    // a pre-epoch host clock can't silently produce checkpoint_at=0
    // for every persisted record (collision on the persisted column).
    let now_ms = crate::clock_anomaly::epoch_ms_i64("ft.session_restore.clock");

    let (metadata_json, pane_count) = prepare_restore_receipt_metadata(pane_id_map)?;

    let tx = conn.transaction()?;
    let receipt = insert_restore_receipt(
        &tx,
        session_id,
        now_ms,
        &metadata_json,
        pane_count,
        None,
    )?;
    let updated = tx.execute(
        "UPDATE mux_sessions
         SET last_checkpoint_at = ?2,
             shutdown_clean = 0,
             clean_checkpoint_id = NULL
         WHERE session_id = ?1",
        rusqlite::params![session_id, now_ms],
    )?;
    if updated != 1 {
        return Err(RestoreError::Bookkeeping(format!(
            "restore receipt references missing session {session_id}"
        )));
    }
    tx.commit()?;

    Ok(receipt.checkpoint_id)
}

fn run_restore_authority_transaction<T, F>(
    conn: &Connection,
    work: F,
) -> Result<T, RestoreAuthorityDbError>
where
    F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, RestoreError>,
{
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|source| RestoreAuthorityDbError::RetrySafe {
            source: source.into(),
        })?;
    match work(&tx) {
        Ok(value) => tx
            .commit()
            .map(|()| value)
            .map_err(|source| RestoreAuthorityDbError::IndeterminateCommit {
                source: source.into(),
            }),
        Err(source) => match tx.rollback() {
            Ok(()) => Err(RestoreAuthorityDbError::RetrySafe { source }),
            Err(rollback) => Err(RestoreAuthorityDbError::IndeterminateRollback {
                source,
                rollback: Box::new(rollback.into()),
            }),
        },
    }
}

fn run_optional_restore_authority_transaction<T, F>(
    conn: &Connection,
    work: F,
) -> Result<Option<T>, RestoreAuthorityDbError>
where
    F: FnOnce(&rusqlite::Transaction<'_>) -> Result<Option<T>, RestoreError>,
{
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|source| RestoreAuthorityDbError::RetrySafe {
            source: source.into(),
        })?;
    match work(&tx) {
        Ok(Some(value)) => tx
            .commit()
            .map(|()| Some(value))
            .map_err(|source| RestoreAuthorityDbError::IndeterminateCommit {
                source: source.into(),
            }),
        Ok(None) => tx
            .rollback()
            .map(|()| None)
            .map_err(|source| RestoreAuthorityDbError::RetrySafe {
                source: source.into(),
            }),
        Err(source) => match tx.rollback() {
            Ok(()) => Err(RestoreAuthorityDbError::RetrySafe { source }),
            Err(rollback) => Err(RestoreAuthorityDbError::IndeterminateRollback {
                source,
                rollback: Box::new(rollback.into()),
            }),
        },
    }
}

/// Persist and transaction-locally verify a restore receipt. Complete evidence
/// may bind clean session authority and resolve the lifecycle in this same
/// transaction; partial evidence always settles as reconciliation-required.
fn persist_restore_receipt_settled(
    db_path: &str,
    session_id: &str,
    pane_id_map: &HashMap<u64, u64>,
    evidence: &RestoreOutcomeEvidence,
    resolve_complete: bool,
) -> Result<RestoreReceipt, RestoreAuthorityDbError> {
    let conn = open_conn(db_path).map_err(|source| RestoreAuthorityDbError::RetrySafe {
        source,
    })?;
    // br-ft-0n4nx: route through the shared clock-anomaly helper so
    // a pre-epoch host clock can't silently produce checkpoint_at=0
    // for every persisted record (collision on the persisted column).
    let now_ms = crate::clock_anomaly::epoch_ms_i64("ft.session_restore.clock")
        .max(evidence.intent.checkpoint_at);

    let (metadata_json, pane_count, prepared_metadata) =
        prepare_restore_outcome_metadata(pane_id_map, evidence)
            .map_err(|source| RestoreAuthorityDbError::RetrySafe { source })?;

    run_restore_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let persisted_intent =
            load_checkpoint_by_id_from_conn(tx, evidence.intent.checkpoint_id)?.ok_or_else(
                || {
                    RestoreError::Bookkeeping(format!(
                        "restore intent {} disappeared before outcome commit",
                        evidence.intent.checkpoint_id
                    ))
                },
            )?;
        let persisted_source =
            load_checkpoint_by_id_from_conn(tx, evidence.source.checkpoint_id)?.ok_or_else(
                || {
                    RestoreError::Bookkeeping(format!(
                        "restore source {} disappeared before outcome commit",
                        evidence.source.checkpoint_id
                    ))
                },
            )?;
        let source_pane_ids = persisted_source
            .pane_states
            .iter()
            .map(|pane| pane.pane_id)
            .collect::<HashSet<_>>();
        let failed_source_pane_ids = evidence
            .failed_source_pane_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let covered_source_pane_ids = pane_id_map
            .keys()
            .copied()
            .chain(failed_source_pane_ids.iter().copied())
            .collect::<HashSet<_>>();
        let session_authority: Option<RestoreSessionAuthorityRow> = tx
            .query_row(
                "SELECT session.shutdown_clean,
                        session.clean_checkpoint_id,
                        session.last_checkpoint_at,
                        (
                            SELECT latest.id
                            FROM session_checkpoints AS latest
                            WHERE latest.session_id = session.session_id
                            ORDER BY latest.id DESC
                            LIMIT 1
                        )
                 FROM mux_sessions AS session
                 WHERE session.session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let lifecycle: Option<(String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT CASE status
                            WHEN 'intent' THEN 'intent'
                            WHEN 'outcome_complete' THEN 'outcome_complete'
                            WHEN 'reconciliation_required' THEN 'reconciliation_required'
                            WHEN 'resolved' THEN 'resolved'
                            ELSE 'invalid_lifecycle'
                        END,
                        source_checkpoint_id,
                        outcome_checkpoint_id
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1 AND session_id = ?2",
                rusqlite::params![evidence.intent.checkpoint_id, session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if persisted_intent.session_id != session_id
            || persisted_intent.checkpoint_role != CheckpointRole::RestoreIntent
            || persisted_intent.checkpoint_at
                != decode_u64(
                    evidence.intent.checkpoint_at,
                    "session_checkpoints.checkpoint_at",
                )?
            || persisted_intent.state_hash != evidence.intent.state_hash
            || persisted_intent.verification != CheckpointVerification::VerifiedV2
            || persisted_source.session_id != session_id
            || persisted_source.checkpoint_role != CheckpointRole::Snapshot
            || persisted_source.checkpoint_at != evidence.source.checkpoint_at
            || persisted_source.state_hash != evidence.source.state_hash
            || persisted_source.pane_count != evidence.source.pane_count
            || evidence.expected_panes != source_pane_ids.len()
            || covered_source_pane_ids != source_pane_ids
            || pane_id_map
                .keys()
                .any(|pane_id| failed_source_pane_ids.contains(pane_id))
            || pane_id_map
                .keys()
                .any(|pane_id| !source_pane_ids.contains(pane_id))
            || evidence
                .failed_source_pane_ids
                .iter()
                .any(|pane_id| !source_pane_ids.contains(pane_id))
            || failed_source_pane_ids.len() != evidence.failed_source_pane_ids.len()
            || evidence
                .duplicate_target_source_pane_ids
                .iter()
                .any(|pane_id| !source_pane_ids.contains(pane_id))
            || session_authority
                != Some((
                    0,
                    None,
                    Some(evidence.intent.checkpoint_at),
                    Some(evidence.intent.checkpoint_id),
                ))
            || lifecycle
                != Some((
                    "intent".to_string(),
                    evidence.source.checkpoint_id,
                    None,
                ))
        {
            return Err(RestoreError::Bookkeeping(format!(
                "restore intent {} or its lifecycle changed before outcome commit",
                evidence.intent.checkpoint_id
            )));
        }
        let intent_link = Some(evidence.intent.checkpoint_id);
        let receipt = insert_restore_receipt(
            tx,
            session_id,
            now_ms,
            &metadata_json,
            pane_count,
            intent_link,
        )?;
        let persisted_receipt =
            load_checkpoint_by_id_from_conn(tx, receipt.checkpoint_id)?.ok_or_else(|| {
                RestoreError::Bookkeeping(format!(
                    "restore outcome {} disappeared inside its insertion transaction",
                    receipt.checkpoint_id
                ))
            })?;
        let persisted_metadata =
            restore_checkpoint_metadata_from_conn(tx, receipt.checkpoint_id, session_id)?;
        validate_restore_outcome_metadata(receipt.checkpoint_id, &persisted_metadata)?;
        if persisted_receipt.session_id != session_id
            || persisted_receipt.checkpoint_role != CheckpointRole::RestoreReceipt
            || persisted_receipt.checkpoint_type != "startup"
            || persisted_receipt.verification != CheckpointVerification::VerifiedV2
            || persisted_receipt.checkpoint_at
                != decode_u64(
                    receipt.checkpoint_at,
                    "restore_receipt.checkpoint_at",
                )?
            || persisted_receipt.state_hash != receipt.state_hash
            || persisted_receipt.restore_intent_checkpoint_id != intent_link
            || persisted_receipt.pane_count
                != decode_usize(pane_count, "restore_receipt.pane_count")?
            || persisted_receipt.total_bytes != 0
            || persisted_receipt.topology_json.is_some()
            || !persisted_receipt.pane_states.is_empty()
            || persisted_metadata != prepared_metadata
        {
            return Err(RestoreError::Bookkeeping(format!(
                "restore outcome {} failed its transaction-local reload verification",
                receipt.checkpoint_id
            )));
        }
        let evidence_complete = restore_outcome_evidence_is_complete(evidence);
        let resolve_clean = resolve_complete && evidence_complete;
        let next_status = if resolve_clean {
            "resolved"
        } else if evidence_complete {
            "outcome_complete"
        } else {
            "reconciliation_required"
        };
        let resolved_at = resolve_clean.then_some(now_ms.max(evidence.intent.checkpoint_at));
        let lifecycle_updated = tx.execute(
            "UPDATE restore_attempt_lifecycle
             SET outcome_checkpoint_id = ?2, status = ?3, resolved_at = ?6
             WHERE intent_checkpoint_id = ?1
               AND session_id = ?4
               AND source_checkpoint_id = ?5
               AND status = 'intent'
               AND outcome_checkpoint_id IS NULL
               AND resolved_at IS NULL",
            rusqlite::params![
                evidence.intent.checkpoint_id,
                receipt.checkpoint_id,
                next_status,
                session_id,
                evidence.source.checkpoint_id,
                resolved_at,
            ],
        )?;
        if lifecycle_updated != 1 {
            return Err(RestoreError::Bookkeeping(format!(
                "restore intent {} lifecycle did not accept outcome {}",
                evidence.intent.checkpoint_id, receipt.checkpoint_id
            )));
        }
        let clean_flag = i64::from(resolve_clean);
        let updated = tx.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 shutdown_clean = ?3,
                 clean_checkpoint_id = ?4
             WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                now_ms,
                clean_flag,
                resolve_clean.then_some(receipt.checkpoint_id),
            ],
        )?;
        if updated != 1 {
            return Err(RestoreError::Bookkeeping(format!(
                "restore receipt references missing session {session_id}"
            )));
        }
        let settled_session: (i64, i64, Option<i64>) = tx.query_row(
            "SELECT last_checkpoint_at, shutdown_clean, clean_checkpoint_id
             FROM mux_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let settled_lifecycle: (String, Option<i64>, Option<i64>) = tx.query_row(
            "SELECT CASE status
                        WHEN 'intent' THEN 'intent'
                        WHEN 'outcome_complete' THEN 'outcome_complete'
                        WHEN 'reconciliation_required' THEN 'reconciliation_required'
                        WHEN 'resolved' THEN 'resolved'
                        ELSE 'invalid_lifecycle'
                    END,
                    outcome_checkpoint_id,
                    resolved_at
             FROM restore_attempt_lifecycle
             WHERE intent_checkpoint_id = ?1 AND session_id = ?2",
            rusqlite::params![evidence.intent.checkpoint_id, session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if settled_session
            != (
                now_ms,
                clean_flag,
                resolve_clean.then_some(receipt.checkpoint_id),
            )
            || settled_lifecycle
                != (
                    next_status.to_string(),
                    Some(receipt.checkpoint_id),
                    resolved_at,
                )
        {
            return Err(RestoreError::Bookkeeping(format!(
                "restore outcome {} failed transaction-local authority settlement",
                receipt.checkpoint_id
            )));
        }
        Ok(receipt)
    })
}

/// Record an immutable intent before the first external mux mutation. If a
/// previous unclean restore receipt is already latest, its external outcome is
/// ambiguous; refuse a blind replay rather than duplicate panes or processes.
fn persist_restore_intent_unclean(
    db_path: &str,
    session_id: &str,
    source: &RestoreIntentSource,
) -> Result<RestoreReceipt, RestoreAuthorityDbError> {
    let conn = open_conn(db_path).map_err(|source| RestoreAuthorityDbError::RetrySafe {
        source,
    })?;
    let metadata_json = prepare_restore_intent_metadata(source)
        .map_err(|source| RestoreAuthorityDbError::RetrySafe { source })?;
    let source_checkpoint_at = i64::try_from(source.checkpoint_at).map_err(|_| {
        RestoreAuthorityDbError::RetrySafe {
            source: RestoreError::Bookkeeping(
                "restore source timestamp exceeds SQLite INTEGER".to_string(),
            ),
        }
    })?;
    let now_ms = crate::clock_anomaly::epoch_ms_i64("ft.session_restore.intent.clock")
        .max(source_checkpoint_at);

    run_restore_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let persisted_source = load_checkpoint_by_id_from_conn(tx, source.checkpoint_id)?
            .ok_or_else(|| {
                RestoreError::Bookkeeping(format!(
                    "restore source checkpoint {} disappeared before intent commit",
                    source.checkpoint_id
                ))
            })?;
        if persisted_source.session_id != session_id
            || persisted_source.checkpoint_at != source.checkpoint_at
            || persisted_source.checkpoint_role != CheckpointRole::Snapshot
            || persisted_source.checkpoint_role != source.checkpoint_role
            || persisted_source.state_hash != source.state_hash
            || persisted_source.pane_count != source.pane_count
        {
            return Err(RestoreError::Bookkeeping(format!(
                "restore source checkpoint {} changed before intent commit",
                source.checkpoint_id
            )));
        }
        let session_authority: Option<RestoreSessionAuthorityRow> = tx
            .query_row(
                "SELECT session.shutdown_clean,
                        session.clean_checkpoint_id,
                        session.last_checkpoint_at,
                        (
                            SELECT latest.id
                            FROM session_checkpoints AS latest
                            WHERE latest.session_id = session.session_id
                            ORDER BY latest.id DESC
                            LIMIT 1
                        )
                 FROM mux_sessions AS session
                 WHERE session.session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((shutdown_clean, clean_checkpoint_id, last_checkpoint_at, latest_checkpoint_id)) =
            session_authority
        else {
            return Err(RestoreError::Bookkeeping(format!(
                "restore intent references missing session {session_id}"
            )));
        };
        if shutdown_clean != 0
            || clean_checkpoint_id.is_some()
            || last_checkpoint_at != Some(source_checkpoint_at)
            || latest_checkpoint_id != Some(source.checkpoint_id)
        {
            return Err(RestoreError::Bookkeeping(format!(
                "restore source checkpoint {} no longer has exact unclean latest authority for session {session_id}",
                source.checkpoint_id
            )));
        }
        if let Some((intent_checkpoint_id, outcome_checkpoint_id, status)) =
            restore_ambiguity_from_conn(tx, session_id)?
        {
            return Err(RestoreError::RestoreAttemptRequiresReconciliation {
                session_id: session_id.to_string(),
                intent_checkpoint_id,
                outcome_checkpoint_id,
                status,
            });
        }

        let receipt = insert_restore_intent(tx, session_id, now_ms, &metadata_json)?;
        let lifecycle_inserted = tx.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 outcome_checkpoint_id, status, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, NULL, 'intent', ?4, NULL)",
            rusqlite::params![
                receipt.checkpoint_id,
                session_id,
                source.checkpoint_id,
                now_ms,
            ],
        )?;
        if lifecycle_inserted != 1 {
            return Err(RestoreError::Bookkeeping(format!(
                "restore intent {} did not create exactly one lifecycle row",
                receipt.checkpoint_id
            )));
        }
        let updated = tx.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 shutdown_clean = 0,
                 clean_checkpoint_id = NULL
             WHERE session_id = ?1
               AND shutdown_clean = 0
               AND clean_checkpoint_id IS NULL
               AND last_checkpoint_at = ?3
               AND ?4 = (
                   SELECT latest.id
                   FROM session_checkpoints AS latest
                   WHERE latest.session_id = ?1
                   ORDER BY latest.id DESC
                   LIMIT 1
               )",
            rusqlite::params![
                session_id,
                now_ms,
                source_checkpoint_at,
                receipt.checkpoint_id,
            ],
        )?;
        if updated != 1 {
            return Err(RestoreError::Bookkeeping(format!(
                "restore intent references missing session {session_id}"
            )));
        }
        Ok(receipt)
    })
}

/// Bind one immutable, deterministic-latest restore receipt as the exact clean
/// authority only after layout and process disposition evaluation have settled.
#[cfg(test)]
fn mark_restore_receipt_clean(
    db_path: &str,
    session_id: &str,
    receipt: &RestoreReceipt,
) -> Result<(), RestoreAuthorityDbError> {
    let conn = open_conn(db_path).map_err(|source| RestoreAuthorityDbError::RetrySafe {
        source,
    })?;
    let resolved_at = crate::clock_anomaly::epoch_ms_i64("ft.session_restore.resolved.clock");
    run_restore_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let persisted = load_checkpoint_by_id_from_conn(tx, receipt.checkpoint_id)?
            .ok_or_else(|| {
                RestoreError::Bookkeeping(format!(
                    "restore receipt {} disappeared before clean binding",
                    receipt.checkpoint_id
                ))
            })?;
        if persisted.session_id != session_id
            || persisted.checkpoint_at != decode_u64(
                receipt.checkpoint_at,
                "session_checkpoints.checkpoint_at",
            )?
            || persisted.checkpoint_role != CheckpointRole::RestoreReceipt
            || persisted.verification != CheckpointVerification::VerifiedV2
            || persisted.state_hash != receipt.state_hash
        {
            return Err(RestoreError::Bookkeeping(format!(
                "restore receipt {} failed exact verified-v2 clean-authority validation",
                receipt.checkpoint_id
            )));
        }
        let intent_checkpoint_id = validate_restore_authority_chain(
            tx,
            session_id,
            &persisted,
            "outcome_complete",
            true,
        )?;
        let updated = tx.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 clean_checkpoint_id = ?2
             WHERE session_id = ?1
               AND last_checkpoint_at = ?3
               AND EXISTS (
                   SELECT 1
                   FROM session_checkpoints AS exact
                   WHERE exact.id = ?2
                     AND exact.session_id = ?1
                     AND exact.checkpoint_at = ?3
                     AND exact.checkpoint_role = 'restore_receipt'
                     AND exact.restore_intent_checkpoint_id = ?5
                     AND exact.state_hash = ?4
               )
               AND ?2 = (
                   SELECT latest.id
                   FROM session_checkpoints AS latest
                   WHERE latest.session_id = ?1
                   ORDER BY latest.id DESC
                   LIMIT 1
               )",
            rusqlite::params![
                session_id,
                receipt.checkpoint_id,
                receipt.checkpoint_at,
                receipt.state_hash.as_str(),
                intent_checkpoint_id,
            ],
        )?;
        if updated != 1 {
            return Err(RestoreError::Bookkeeping(format!(
                "restore receipt {} is stale, foreign, missing, or no longer latest for session {session_id}",
                receipt.checkpoint_id
            )));
        }
        let lifecycle_updated = tx.execute(
            "UPDATE restore_attempt_lifecycle
             SET status = 'resolved',
                 resolved_at = MAX(created_at, ?3)
             WHERE intent_checkpoint_id = ?1
               AND session_id = ?4
               AND outcome_checkpoint_id = ?2
               AND status = 'outcome_complete'
               AND resolved_at IS NULL",
            rusqlite::params![
                intent_checkpoint_id,
                receipt.checkpoint_id,
                resolved_at,
                session_id,
            ],
        )?;
        if lifecycle_updated != 1 {
            return Err(RestoreError::Bookkeeping(format!(
                "restore receipt {} clean binding could not resolve lifecycle {}",
                receipt.checkpoint_id, intent_checkpoint_id
            )));
        }
        Ok(())
    })
}

#[cfg(test)]
fn complete_test_restore_outcome_evidence(pane_count: usize) -> RestoreOutcomeEvidence {
    RestoreOutcomeEvidence {
        evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
        intent: RestoreReceipt {
            checkpoint_id: 0,
            checkpoint_at: 0,
            state_hash: "rsi2:test-only-intent".to_string(),
        },
        source: RestoreIntentSource {
            checkpoint_id: 0,
            checkpoint_at: 0,
            checkpoint_role: CheckpointRole::Snapshot,
            state_hash: "test-only-source".to_string(),
            pane_count,
        },
        expected_panes: pane_count,
        mapped_panes: pane_count,
        reported_layout_failures: 0,
        failed_source_pane_ids: Vec::new(),
        unexpected_mapping_count: 0,
        unexpected_failure_count: 0,
        duplicate_target_source_pane_ids: Vec::new(),
        layout_complete: true,
        scrollback_requested: false,
        scrollback_complete: true,
        scrollback_failures: 0,
        scrollback_skipped: 0,
        scrollback_global_error: false,
        process_plan_evaluated: pane_count > 0,
        process_plans_total: pane_count,
        process_plans_settled: pane_count,
        process_interrupted: false,
        attempt_interrupted: false,
        interruption_phase: None,
        interruption_reason: None,
        process_failed: 0,
        process_manual: pane_count,
        process_skipped: 0,
    }
}

#[cfg(test)]
fn finalize_restore_for_test(
    db_path: &str,
    session_id: &str,
    pane_id_map: &HashMap<u64, u64>,
    mark_clean: bool,
) -> Result<i64, RestoreError> {
    let needs_verified_snapshot_source = load_latest_checkpoint(db_path, session_id)?.is_none_or(
        |checkpoint| {
            checkpoint.checkpoint_role != CheckpointRole::Snapshot
                || checkpoint.verification != CheckpointVerification::VerifiedV2
        },
    );
    if needs_verified_snapshot_source {
        let mut conn = open_conn(db_path)?;
        let session_exists = conn
            .query_row(
                "SELECT 1 FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if session_exists.is_none() {
            return Err(RestoreError::Bookkeeping(format!(
                "test restore source references missing session {session_id}"
            )));
        }
        let checkpoint_at = crate::clock_anomaly::epoch_ms_i64(
            "ft.session_restore.test_source.clock",
        );
        let captured_at = u64::try_from(checkpoint_at).map_err(|_| {
            RestoreError::Bookkeeping("test restore source timestamp is negative".to_string())
        })?;
        let mut old_pane_ids = pane_id_map.keys().copied().collect::<Vec<_>>();
        old_pane_ids.sort_unstable();
        let tabs = old_pane_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, pane_id)| {
                let tab_id = u64::try_from(index).map_err(|_| {
                    RestoreError::Bookkeeping(
                        "test restore source tab count exceeds u64".to_string(),
                    )
                })?;
                Ok(crate::session_topology::TabSnapshot {
                    tab_id,
                    title: None,
                    active_pane_id: Some(pane_id),
                    pane_tree: crate::session_topology::PaneNode::Leaf {
                        pane_id,
                        rows: 24,
                        cols: 80,
                        cwd: None,
                        title: None,
                        is_active: true,
                    },
                })
            })
            .collect::<Result<Vec<_>, RestoreError>>()?;
        let windows = if tabs.is_empty() {
            Vec::new()
        } else {
            vec![crate::session_topology::WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs,
                active_tab_index: Some(0),
            }]
        };
        let topology_json = TopologySnapshot {
            schema_version: 1,
            captured_at,
            workspace_id: None,
            windows,
        }
        .to_json()
        .map_err(|_error| {
            RestoreError::Bookkeeping(
                "test restore source topology serialization failed".to_string(),
            )
        })?;
        let terminal_json =
            r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
        let persisted_panes = old_pane_ids
            .iter()
            .copied()
            .map(|pane_id| {
                let pane_id = i64::try_from(pane_id).map_err(|_| {
                    RestoreError::Bookkeeping(
                        "test restore source pane id exceeds SQLite INTEGER".to_string(),
                    )
                })?;
                Ok(PersistedPaneState {
                    pane_id,
                    cwd: None,
                    command: None,
                    env_json: None,
                    terminal_state_json: terminal_json.to_string(),
                    agent_metadata_json: None,
                    scrollback_checkpoint_seq: None,
                    last_output_at: None,
                })
            })
            .collect::<Result<Vec<_>, RestoreError>>()?;
        let pane_count = i64::try_from(persisted_panes.len()).map_err(|_| {
            RestoreError::Bookkeeping(
                "test restore source pane count exceeds SQLite INTEGER".to_string(),
            )
        })?;
        let total_bytes = terminal_json
            .len()
            .checked_mul(persisted_panes.len())
            .and_then(|bytes| i64::try_from(bytes).ok())
            .ok_or_else(|| {
                RestoreError::Bookkeeping(
                    "test restore source byte count exceeds SQLite INTEGER".to_string(),
                )
            })?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 topology_json, restore_intent_checkpoint_id
             ) VALUES (?1, ?2, 'periodic', 'pending:snp2', ?3, ?4, NULL,
                       'snapshot', ?5, NULL)",
            rusqlite::params![
                session_id,
                checkpoint_at,
                pane_count,
                total_bytes,
                topology_json,
            ],
        )?;
        let checkpoint_id = tx.last_insert_rowid();
        for pane in &persisted_panes {
            tx.execute(
                "INSERT INTO mux_pane_state (
                     checkpoint_id, pane_id, cwd, command, env_json,
                     terminal_state_json, agent_metadata_json,
                     scrollback_checkpoint_seq, last_output_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    checkpoint_id,
                    pane.pane_id,
                    pane.cwd,
                    pane.command,
                    pane.env_json,
                    pane.terminal_state_json,
                    pane.agent_metadata_json,
                    pane.scrollback_checkpoint_seq,
                    pane.last_output_at,
                ],
            )?;
        }
        let state_hash = checkpoint_witness(
            CHECKPOINT_ROLE_SNAPSHOT,
            session_id,
            checkpoint_id,
            checkpoint_at,
            "periodic",
            pane_count,
            total_bytes,
            None,
            Some(&topology_json),
            &persisted_panes,
        )
        .map_err(|_error| {
            RestoreError::Bookkeeping(
                "test restore source witness computation failed".to_string(),
            )
        })?;
        tx.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            rusqlite::params![state_hash, checkpoint_id],
        )?;
        tx.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 shutdown_clean = 0,
                 clean_checkpoint_id = NULL
             WHERE session_id = ?1",
            rusqlite::params![session_id, checkpoint_at],
        )?;
        tx.commit()?;
    }
    let source_checkpoint = load_latest_checkpoint(db_path, session_id)?.ok_or_else(|| {
        RestoreError::Bookkeeping(format!(
            "test restore source for session {session_id} disappeared"
        ))
    })?;
    let source = RestoreIntentSource {
        checkpoint_id: source_checkpoint.checkpoint_id,
        checkpoint_at: source_checkpoint.checkpoint_at,
        checkpoint_role: source_checkpoint.checkpoint_role,
        state_hash: source_checkpoint.state_hash,
        pane_count: source_checkpoint.pane_count,
    };
    let intent = persist_restore_intent_unclean(db_path, session_id, &source)
        .map_err(|_error| {
            RestoreError::Bookkeeping("test restore intent did not settle".to_string())
        })?;
    let mut evidence = complete_test_restore_outcome_evidence(pane_id_map.len());
    evidence.intent = intent;
    evidence.source = source;
    let receipt = persist_restore_receipt_settled(
        db_path,
        session_id,
        pane_id_map,
        &evidence,
        mark_clean,
    )
    .map_err(|_error| {
        RestoreError::Bookkeeping("test restore outcome did not settle".to_string())
    })?;
    Ok(receipt.checkpoint_id)
}

fn validate_restore_topology(
    checkpoint_id: i64,
    pane_count: usize,
    persisted_pane_ids: &[u64],
    topology: &TopologySnapshot,
) -> Result<(), RestoreError> {
    crate::restore_layout::validate_restore_snapshot_for_admission(topology, true, true).map_err(
        |_error| {
            RestoreError::CorruptCheckpoint(format!(
                "checkpoint {checkpoint_id} topology failed layout preflight"
            ))
        },
    )?;

    let mut topology_pane_ids = topology.pane_ids();
    if topology_pane_ids.len() != pane_count {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {} declares {} panes but its topology contains {} pane leaves",
            checkpoint_id,
            pane_count,
            topology_pane_ids.len()
        )));
    }
    topology_pane_ids.sort_unstable();
    if let Some(duplicate) = topology_pane_ids
        .windows(2)
        .find(|ids| ids[0] == ids[1])
        .map(|ids| ids[0])
    {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {} topology contains duplicate pane id {duplicate}",
            checkpoint_id
        )));
    }

    let mut persisted_pane_ids = persisted_pane_ids.to_vec();
    persisted_pane_ids.sort_unstable();
    if topology_pane_ids != persisted_pane_ids {
        return Err(RestoreError::CorruptCheckpoint(format!(
            "checkpoint {} topology pane IDs do not match its persisted pane-state IDs",
            checkpoint_id
        )));
    }

    Ok(())
}

// =============================================================================
// CLI query functions
// =============================================================================

/// Session summary for CLI display.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub created_at: u64,
    pub last_checkpoint_at: Option<u64>,
    pub shutdown_clean: bool,
    pub ft_version: String,
    pub host_id: Option<String>,
    pub checkpoint_count: usize,
    pub pane_count: Option<usize>,
}

/// Default number of session summaries or checkpoint summaries returned by
/// the operator-facing paged query APIs.
pub const DEFAULT_SESSION_QUERY_LIMIT: usize = 50;
/// Hard maximum number of summaries materialized by one session query.
pub const MAX_SESSION_QUERY_LIMIT: usize = 200;
/// Hard maximum offset accepted by the bounded session query APIs.
pub const MAX_SESSION_QUERY_OFFSET: usize = 100_000;

/// One bounded page of saved-session summaries.
#[derive(Debug, Clone, Serialize)]
pub struct SessionListPage {
    pub sessions: Vec<SessionInfo>,
    pub total_sessions: usize,
    pub limit: usize,
    pub offset: usize,
    pub has_more: bool,
}

fn validate_session_query_window(limit: usize, offset: usize) -> Result<(), RestoreError> {
    if limit == 0 || limit > MAX_SESSION_QUERY_LIMIT || offset > MAX_SESSION_QUERY_OFFSET {
        return Err(RestoreError::InvalidQueryWindow {
            limit,
            offset,
            max_limit: MAX_SESSION_QUERY_LIMIT,
            max_offset: MAX_SESSION_QUERY_OFFSET,
        });
    }
    Ok(())
}

/// List all sessions with their checkpoint counts when the result fits within
/// one bounded page. Larger histories fail closed so callers cannot
/// accidentally materialize an unbounded result; use [`list_sessions_page`]
/// for explicit pagination.
pub fn list_sessions(db_path: &str) -> Result<Vec<SessionInfo>, RestoreError> {
    with_query_snapshot(db_path, list_sessions_from_conn)
}

fn list_sessions_from_conn(conn: &Connection) -> Result<Vec<SessionInfo>, RestoreError> {
    let page = list_sessions_page_from_conn(conn, MAX_SESSION_QUERY_LIMIT, 0)?;
    if page.has_more {
        return Err(RestoreError::QueryResultLimit {
            resource: "session_summaries",
            observed: page.total_sessions,
            limit: MAX_SESSION_QUERY_LIMIT,
        });
    }
    Ok(page.sessions)
}

/// Return one bounded page of saved-session summaries and exact pagination
/// metadata from a single SQLite read snapshot.
pub fn list_sessions_page(
    db_path: &str,
    limit: usize,
    offset: usize,
) -> Result<SessionListPage, RestoreError> {
    validate_session_query_window(limit, offset)?;
    with_query_snapshot(db_path, |conn| {
        list_sessions_page_from_conn(conn, limit, offset)
    })
}

fn list_sessions_page_from_conn(
    conn: &Connection,
    limit: usize,
    offset: usize,
) -> Result<SessionListPage, RestoreError> {
    validate_session_query_window(limit, offset)?;
    let total_sessions = decode_usize(
        conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))?,
        "mux_sessions.count",
    )?;
    let fetch_limit = limit.saturating_add(1);
    let mut stmt = conn.prepare(
        "SELECT CASE
                    WHEN typeof(s.session_id) = 'text'
                     AND length(CAST(s.session_id AS BLOB)) BETWEEN 1 AND ?1
                    THEN s.session_id
                END,
                s.created_at,
                (SELECT causal.checkpoint_at
                 FROM session_checkpoints AS causal
                 WHERE causal.session_id = s.session_id
                 ORDER BY causal.checkpoint_at DESC, causal.id DESC
                 LIMIT 1) AS last_checkpoint_at,
                s.shutdown_clean, s.clean_checkpoint_id,
                CASE
                    WHEN typeof(s.ft_version) = 'text'
                     AND length(CAST(s.ft_version AS BLOB)) BETWEEN 1 AND ?2
                    THEN s.ft_version
                END,
                CASE
                    WHEN s.host_id IS NULL THEN NULL
                    WHEN typeof(s.host_id) = 'text'
                     AND length(CAST(s.host_id AS BLOB)) <= ?3
                    THEN s.host_id
                END,
                s.host_id IS NOT NULL,
                (SELECT COUNT(*) FROM session_checkpoints c WHERE c.session_id = s.session_id),
                (SELECT c.pane_count FROM session_checkpoints c
                 WHERE c.session_id = s.session_id
                   AND c.checkpoint_role = 'snapshot'
                 ORDER BY c.checkpoint_at DESC, c.id DESC LIMIT 1)
         FROM mux_sessions s
         ORDER BY COALESCE((
                      SELECT causal.checkpoint_at
                      FROM session_checkpoints AS causal
                      WHERE causal.session_id = s.session_id
                      ORDER BY causal.checkpoint_at DESC, causal.id DESC
                      LIMIT 1
                  ), -1) DESC,
                  COALESCE((
                      SELECT causal.id
                      FROM session_checkpoints AS causal
                      WHERE causal.session_id = s.session_id
                      ORDER BY causal.checkpoint_at DESC, causal.id DESC
                      LIMIT 1
                  ), -1) DESC,
                  s.created_at DESC,
                  s.session_id ASC
         LIMIT ?4 OFFSET ?5",
    )?;

    let mut raw_sessions = stmt
        .query_map(
            rusqlite::params![
                i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_SESSION_FT_VERSION_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
                i64::try_from(fetch_limit).unwrap_or(i64::MAX),
                i64::try_from(offset).unwrap_or(i64::MAX),
            ],
            |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, Option<i64>>(9)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let has_more = raw_sessions.len() > limit;
    raw_sessions.truncate(limit);

    let sessions = raw_sessions
        .into_iter()
        .map(
            |(
                session_id,
                created_at,
                last_checkpoint_at,
                shutdown_clean,
                clean_checkpoint_id,
                ft_version,
                host_id,
                host_id_present,
                checkpoint_count,
                pane_count,
            )| {
                let session_id = decode_required_bounded_text(
                    session_id,
                    "mux_sessions.session_id",
                )?;
                let ft_version = decode_required_bounded_text(
                    ft_version,
                    "mux_sessions.ft_version",
                )?;
                let host_id = decode_optional_bounded_text(
                    host_id,
                    host_id_present,
                    "mux_sessions.host_id",
                )?;
                let shutdown_clean = assess_clean_authority(
                    conn,
                    &session_id,
                    shutdown_clean,
                    clean_checkpoint_id,
                )?;
                Ok(SessionInfo {
                    session_id,
                    created_at: decode_u64(created_at, "mux_sessions.created_at")?,
                    last_checkpoint_at: decode_opt_u64(
                        last_checkpoint_at,
                        "session_checkpoints.checkpoint_at",
                    )?,
                    shutdown_clean,
                    ft_version,
                    host_id,
                    checkpoint_count: decode_usize(checkpoint_count, "session_checkpoints.count")?,
                    pane_count: decode_opt_usize(pane_count, "session_checkpoints.pane_count")?,
                })
            },
        )
        .collect::<Result<Vec<_>, RestoreError>>()?;

    Ok(SessionListPage {
        sessions,
        total_sessions,
        limit,
        offset,
        has_more,
    })
}

/// Checkpoint summary for show command.
#[derive(Debug, Clone, Serialize)]
pub struct CheckpointInfo {
    pub id: i64,
    pub checkpoint_at: u64,
    pub checkpoint_type: String,
    pub checkpoint_role: CheckpointRole,
    pub pane_count: usize,
    pub total_bytes: usize,
}

/// One bounded page of checkpoint summaries for a selected session.
#[derive(Debug, Clone)]
pub struct SessionShowPage {
    pub session: SessionCandidate,
    pub checkpoints: Vec<CheckpointInfo>,
    pub total_checkpoints: usize,
    pub limit: usize,
    pub offset: usize,
    pub has_more: bool,
}

/// Show detailed session info including checkpoints when the checkpoint
/// history fits within one bounded page. Larger histories fail closed; use
/// [`show_session_page`] for explicit pagination.
pub fn show_session(
    db_path: &str,
    session_id: &str,
) -> Result<(SessionCandidate, Vec<CheckpointInfo>), RestoreError> {
    validate_session_selector(session_id)?;
    with_query_snapshot(db_path, |conn| show_session_from_conn(conn, session_id))
}

fn show_session_from_conn(
    conn: &Connection,
    session_id: &str,
) -> Result<(SessionCandidate, Vec<CheckpointInfo>), RestoreError> {
    let page = show_session_page_from_conn(
        conn,
        session_id,
        MAX_SESSION_QUERY_LIMIT,
        0,
    )?;
    if page.has_more {
        return Err(RestoreError::QueryResultLimit {
            resource: "checkpoint_summaries",
            observed: page.total_checkpoints,
            limit: MAX_SESSION_QUERY_LIMIT,
        });
    }
    Ok((page.session, page.checkpoints))
}

/// Return one bounded page of checkpoint summaries for a selected session.
pub fn show_session_page(
    db_path: &str,
    session_id: &str,
    limit: usize,
    offset: usize,
) -> Result<SessionShowPage, RestoreError> {
    validate_session_selector(session_id)?;
    validate_session_query_window(limit, offset)?;
    with_query_snapshot(db_path, |conn| {
        show_session_page_from_conn(conn, session_id, limit, offset)
    })
}

fn show_session_page_from_conn(
    conn: &Connection,
    session_id: &str,
    limit: usize,
    offset: usize,
) -> Result<SessionShowPage, RestoreError> {
    validate_session_query_window(limit, offset)?;
    // Get session
    let session = conn
        .query_row(
            "SELECT CASE
                        WHEN typeof(session.session_id) = 'text'
                         AND length(CAST(session.session_id AS BLOB)) BETWEEN 1 AND ?2
                        THEN session.session_id
                    END,
                    session.created_at,
                    (SELECT causal.checkpoint_at
                     FROM session_checkpoints AS causal
                     WHERE causal.session_id = session.session_id
                     ORDER BY causal.checkpoint_at DESC, causal.id DESC
                     LIMIT 1) AS last_checkpoint_at,
                    (SELECT snapshot.id
                     FROM session_checkpoints AS snapshot
                     WHERE snapshot.session_id = session.session_id
                       AND snapshot.checkpoint_role = 'snapshot'
                     ORDER BY snapshot.checkpoint_at DESC, snapshot.id DESC
                     LIMIT 1),
                    CASE
                        WHEN typeof(session.ft_version) = 'text'
                         AND length(CAST(session.ft_version AS BLOB)) BETWEEN 1 AND ?3
                        THEN session.ft_version
                    END,
                    CASE
                        WHEN session.host_id IS NULL THEN NULL
                        WHEN typeof(session.host_id) = 'text'
                         AND length(CAST(session.host_id AS BLOB)) <= ?4
                        THEN session.host_id
                    END,
                    session.host_id IS NOT NULL
             FROM mux_sessions AS session WHERE session.session_id = ?1",
            rusqlite::params![
                session_id,
                i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_SESSION_FT_VERSION_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_SESSION_HOST_ID_BYTES).unwrap_or(i64::MAX),
            ],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => RestoreError::NoSessions,
            _other => RestoreError::Database("session lookup failed".to_string()),
        })?;
    let session = SessionCandidate {
        session_id: decode_required_bounded_text(
            session.0,
            "mux_sessions.session_id",
        )?,
        created_at: decode_u64(session.1, "mux_sessions.created_at")?,
        last_checkpoint_at: decode_opt_u64(
            session.2,
            "session_checkpoints.checkpoint_at",
        )?,
        selected_checkpoint_id: session.3,
        ft_version: decode_required_bounded_text(
            session.4,
            "mux_sessions.ft_version",
        )?,
        host_id: decode_optional_bounded_text(
            session.5,
            session.6,
            "mux_sessions.host_id",
        )?,
    };

    let total_checkpoints = decode_usize(
        conn.query_row(
            "SELECT COUNT(*) FROM session_checkpoints WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?,
        "session_checkpoints.count",
    )?;
    let fetch_limit = limit.saturating_add(1);

    // Get checkpoints
    let mut stmt = conn.prepare(
        "SELECT id,
                checkpoint_at,
                CASE
                    WHEN typeof(checkpoint_type) = 'text'
                     AND length(CAST(checkpoint_type AS BLOB)) BETWEEN 1 AND ?2
                    THEN checkpoint_type
                END,
                CASE
                    WHEN typeof(checkpoint_role) = 'text'
                     AND length(CAST(checkpoint_role AS BLOB)) BETWEEN 1 AND ?3
                    THEN checkpoint_role
                END,
                pane_count,
                total_bytes
         FROM session_checkpoints
         WHERE session_id = ?1
         ORDER BY checkpoint_at DESC, id DESC
         LIMIT ?4 OFFSET ?5",
    )?;

    let mut raw_checkpoints = stmt
        .query_map(
            rusqlite::params![
                session_id,
                i64::try_from(MAX_CHECKPOINT_TYPE_BYTES).unwrap_or(i64::MAX),
                i64::try_from(MAX_CHECKPOINT_ROLE_BYTES).unwrap_or(i64::MAX),
                i64::try_from(fetch_limit).unwrap_or(i64::MAX),
                i64::try_from(offset).unwrap_or(i64::MAX),
            ],
            |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let has_more = raw_checkpoints.len() > limit;
    raw_checkpoints.truncate(limit);

    let checkpoints = raw_checkpoints
        .into_iter()
        .map(
            |(id, checkpoint_at, checkpoint_type, checkpoint_role, pane_count, total_bytes)| {
                let checkpoint_type = decode_required_bounded_text(
                    checkpoint_type,
                    "session_checkpoints.checkpoint_type",
                )?;
                let checkpoint_role = decode_required_bounded_text(
                    checkpoint_role,
                    "session_checkpoints.checkpoint_role",
                )?;
                Ok(CheckpointInfo {
                    id,
                    checkpoint_at: decode_u64(checkpoint_at, "session_checkpoints.checkpoint_at")?,
                    checkpoint_type,
                    checkpoint_role: CheckpointRole::from_db(id, &checkpoint_role)?,
                    pane_count: decode_usize(pane_count, "session_checkpoints.pane_count")?,
                    total_bytes: decode_usize(total_bytes, "session_checkpoints.total_bytes")?,
                })
            },
        )
        .collect::<Result<Vec<_>, RestoreError>>()?;

    Ok(SessionShowPage {
        session,
        checkpoints,
        total_checkpoints,
        limit,
        offset,
        has_more,
    })
}

/// Session health check result.
#[derive(Debug, Clone, Serialize)]
pub struct SessionDoctorReport {
    pub total_sessions: usize,
    pub unclean_sessions: usize,
    pub total_checkpoints: usize,
    pub orphaned_checkpoints: usize,
    pub orphaned_pane_states: usize,
    pub unresolved_restore_attempts: usize,
    pub invalid_resolved_restore_chains: usize,
    pub outcome_complete_restore_attempts: usize,
    pub reconciliation_required_restore_attempts: usize,
    pub orphaned_restore_intents: usize,
    pub total_data_bytes: usize,
}

/// Run health check on session data.
pub fn session_doctor(db_path: &str) -> Result<SessionDoctorReport, RestoreError> {
    with_query_snapshot(db_path, session_doctor_from_conn)
}

fn session_doctor_from_conn(conn: &Connection) -> Result<SessionDoctorReport, RestoreError> {
    let total_sessions: i64 =
        conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))?;

    let mut shutdown_stmt = conn.prepare(
        "SELECT CASE
                    WHEN typeof(session_id) = 'text'
                     AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?1
                    THEN session_id
                END,
                shutdown_clean,
                clean_checkpoint_id
         FROM mux_sessions",
    )?;
    let shutdown_rows = shutdown_stmt.query_map(
        [i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX)],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        },
    )?;
    let mut unclean_sessions = 0usize;
    for row in shutdown_rows {
        let (session_id, shutdown_clean, clean_checkpoint_id) = row?;
        let session_id = decode_required_bounded_text(
            session_id,
            "mux_sessions.session_id",
        )?;
        if !assess_clean_authority(
            conn,
            &session_id,
            shutdown_clean,
            clean_checkpoint_id,
        )? {
            unclean_sessions = unclean_sessions.saturating_add(1);
        }
    }
    let invalid_resolved_restore_chains =
        count_invalid_resolved_restore_chains_from_conn(conn)?;

    let total_checkpoints: i64 =
        conn.query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })?;

    let orphaned_checkpoints: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM session_checkpoints AS checkpoint
         WHERE NOT EXISTS (
             SELECT 1
             FROM mux_sessions AS session
             WHERE session.session_id = checkpoint.session_id
         )",
        [],
        |row| row.get(0),
    )?;

    let orphaned_pane_states: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM mux_pane_state AS pane
         WHERE NOT EXISTS (
             SELECT 1
             FROM session_checkpoints AS checkpoint
             WHERE checkpoint.id = pane.checkpoint_id
         )
         OR EXISTS (
             SELECT 1
             FROM session_checkpoints AS checkpoint
             WHERE checkpoint.id = pane.checkpoint_id
               AND NOT EXISTS (
                   SELECT 1
                   FROM mux_sessions AS session
                   WHERE session.session_id = checkpoint.session_id
               )
         )",
        [],
        |row| row.get(0),
    )?;

    let (total_data_bytes, minimum_data_bytes): (i64, Option<i64>) = conn.query_row(
        "SELECT COALESCE(SUM(total_bytes), 0), MIN(total_bytes)
         FROM session_checkpoints",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if let Some(minimum_data_bytes) = minimum_data_bytes {
        decode_usize(minimum_data_bytes, "session_checkpoints.total_bytes")?;
    }

    let (
        unresolved_restore_attempts,
        outcome_complete_restore_attempts,
        reconciliation_required_restore_attempts,
    ): (i64, i64, i64) = conn.query_row(
        "SELECT
             COUNT(*) FILTER (
                 WHERE CASE
                     WHEN typeof(status) = 'text' AND status = 'resolved' THEN 0
                     ELSE 1
                 END = 1
             ),
             COUNT(*) FILTER (WHERE status = 'outcome_complete'),
             COUNT(*) FILTER (WHERE status = 'reconciliation_required')
         FROM restore_attempt_lifecycle",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let orphaned_restore_intents: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM session_checkpoints AS intent
         WHERE (
             intent.checkpoint_role = 'restore_intent'
             OR (
                 intent.checkpoint_role = 'restore_receipt'
                 AND CASE
                     WHEN typeof(intent.metadata_json) != 'text'
                       OR length(CAST(intent.metadata_json AS BLOB)) > ?1
                     THEN 1
                     WHEN NOT json_valid(intent.metadata_json)
                     THEN 1
                     WHEN json_extract(
                         intent.metadata_json,
                         '$.restore_attempt.phase'
                     ) = 'outcome'
                     THEN 0
                     WHEN json_type(
                             intent.metadata_json,
                             '$.restore_attempt'
                          ) IS NULL
                      AND json_type(
                             intent.metadata_json,
                             '$.old_to_new'
                          ) = 'object'
                     THEN 0
                     ELSE 1
                 END = 1
             )
         )
         AND NOT EXISTS (
             SELECT 1
             FROM restore_attempt_lifecycle AS lifecycle
             WHERE lifecycle.session_id = intent.session_id
               AND lifecycle.intent_checkpoint_id = intent.id
         )",
        [i64::try_from(MAX_CHECKPOINT_METADATA_BYTES).unwrap_or(i64::MAX)],
        |row| row.get(0),
    )?;

    Ok(SessionDoctorReport {
        total_sessions: decode_usize(total_sessions, "mux_sessions.count")?,
        unclean_sessions,
        total_checkpoints: decode_usize(total_checkpoints, "session_checkpoints.count")?,
        orphaned_checkpoints: decode_usize(
            orphaned_checkpoints,
            "session_checkpoints.orphaned_count",
        )?,
        orphaned_pane_states: decode_usize(orphaned_pane_states, "mux_pane_state.orphaned_count")?,
        unresolved_restore_attempts: decode_usize(
            unresolved_restore_attempts,
            "restore_attempt_lifecycle.unresolved_count",
        )?,
        invalid_resolved_restore_chains,
        outcome_complete_restore_attempts: decode_usize(
            outcome_complete_restore_attempts,
            "restore_attempt_lifecycle.outcome_complete_count",
        )?,
        reconciliation_required_restore_attempts: decode_usize(
            reconciliation_required_restore_attempts,
            "restore_attempt_lifecycle.reconciliation_required_count",
        )?,
        orphaned_restore_intents: decode_usize(
            orphaned_restore_intents,
            "restore_attempt_lifecycle.orphaned_intent_count",
        )?,
        total_data_bytes: decode_usize(total_data_bytes, "session_checkpoints.total_bytes_sum")?,
    })
}

/// Cx-first compatibility list operation that keeps SQLite work off the async
/// worker and fails closed when the history exceeds one maximum-sized page.
pub async fn list_sessions_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
) -> Result<Vec<SessionInfo>, RestoreError> {
    let db_path = db_path.to_string();
    crate::runtime_async::spawn_blocking_with_cx(cx, move || list_sessions(&db_path))
        .await
        .map_err(|error| restore_blocking_error("session list", error))?
}

/// Cx-first bounded session-list page query.
pub async fn list_sessions_page_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
    limit: usize,
    offset: usize,
) -> Result<SessionListPage, RestoreError> {
    validate_session_query_window(limit, offset)?;
    let db_path = db_path.to_string();
    crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        list_sessions_page(&db_path, limit, offset)
    })
    .await
    .map_err(|error| restore_blocking_error("session list page", error))?
}

/// Cx-first compatibility session query that fails closed when checkpoint
/// summaries exceed one maximum-sized page.
pub async fn show_session_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
    session_id: &str,
) -> Result<(SessionCandidate, Vec<CheckpointInfo>), RestoreError> {
    validate_session_selector(session_id)?;
    let db_path = db_path.to_string();
    let session_id = session_id.to_string();
    crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        show_session(&db_path, &session_id)
    })
    .await
    .map_err(|error| restore_blocking_error("session show", error))?
}

/// Cx-first bounded checkpoint-summary page query for one session.
pub async fn show_session_page_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
    session_id: &str,
    limit: usize,
    offset: usize,
) -> Result<SessionShowPage, RestoreError> {
    validate_session_selector(session_id)?;
    validate_session_query_window(limit, offset)?;
    let db_path = db_path.to_string();
    let session_id = session_id.to_string();
    crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        show_session_page(&db_path, &session_id, limit, offset)
    })
    .await
    .map_err(|error| restore_blocking_error("session show page", error))?
}

/// Cx-first health query with bounded blocking-pool execution.
pub async fn session_doctor_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
) -> Result<SessionDoctorReport, RestoreError> {
    let db_path = db_path.to_string();
    crate::runtime_async::spawn_blocking_with_cx(cx, move || session_doctor(&db_path))
        .await
        .map_err(|error| restore_blocking_error("session doctor", error))?
}

/// Delete a session and all its checkpoints (cascading via SQL).
pub fn delete_session(db_path: &str, session_id: &str) -> Result<bool, RestoreError> {
    validate_session_selector(session_id)?;
    let db_path = Arc::new(db_path.to_string());
    let session_id = session_id.to_string();
    run_checkpoint_authority_sync(
        db_path,
        SnapshotAuthorityOperation::SessionDelete,
        move |db_path| delete_session_authoritatively(db_path, &session_id),
    )
    .map(|deleted| deleted.is_some())
    .map_err(|_error| {
        RestoreError::Bookkeeping(
            "session delete authority operation did not settle".to_string(),
        )
    })
}

/// Cx-first session deletion. The blocking transaction shares snapshot
/// authority admission and latches reconciliation if its settlement is lost.
pub async fn delete_session_with_cx(
    cx: &crate::cx::Cx,
    db_path: &str,
    session_id: &str,
) -> Result<bool, RestoreError> {
    validate_session_selector(session_id)?;
    let db_path = Arc::new(db_path.to_string());
    let session_id = session_id.to_string();
    run_checkpoint_authority_with_cx(
        cx,
        db_path,
        SnapshotAuthorityOperation::SessionDelete,
        move |db_path| delete_session_authoritatively(db_path, &session_id),
    )
    .await
    .map(|deleted| deleted.is_some())
    .map_err(|error| restore_authority_error("session delete", error))
}

fn delete_session_authoritatively(
    db_path: &str,
    session_id: &str,
) -> Result<Option<()>, RestoreAuthorityDbError> {
    let conn = open_conn(db_path).map_err(|source| RestoreAuthorityDbError::RetrySafe {
        source,
    })?;
    run_optional_restore_authority_transaction(&conn, |tx| {
        crate::session_retention::ensure_session_authority_tables_have_no_unaudited_triggers(tx)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM mux_sessions WHERE session_id = ?1
                 UNION ALL
                 SELECT 1 FROM session_checkpoints WHERE session_id = ?1
                 UNION ALL
                 SELECT 1 FROM restore_attempt_lifecycle WHERE session_id = ?1
             )",
            [session_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }

        // Keep explicit dependent deletes for historical databases that were
        // once written with foreign_keys disabled; current rows also cascade.
        tx.execute(
            "DELETE FROM mux_pane_state WHERE checkpoint_id IN
             (SELECT id FROM session_checkpoints WHERE session_id = ?1)",
            [session_id],
        )?;
        tx.execute(
            "DELETE FROM restore_attempt_lifecycle WHERE session_id = ?1",
            [session_id],
        )?;
        tx.execute(
            "DELETE FROM session_checkpoints WHERE session_id = ?1",
            [session_id],
        )?;
        tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(Some(()))
    })
}

// =============================================================================
// Session restore engine
// =============================================================================

/// The session restore engine orchestrates the full restore flow.
pub struct SessionRestorer {
    db_path: Arc<String>,
    config: SessionRestoreConfig,
}

fn record_unclean_block_reason(
    current: &mut Option<UncleanSessionBlockReason>,
    next: UncleanSessionBlockReason,
) {
    *current = Some(match *current {
        None => next,
        Some(existing) if existing == next => existing,
        Some(_) => UncleanSessionBlockReason::Mixed,
    });
}

fn ensure_detected_checkpoint_remains_nonempty(
    checkpoint: &CheckpointData,
) -> Result<(), RestoreError> {
    if checkpoint.pane_states.is_empty() {
        Err(RestoreError::DetectedCheckpointChanged {
            checkpoint_id: checkpoint.checkpoint_id,
        })
    } else {
        Ok(())
    }
}

impl SessionRestorer {
    /// Create a new session restorer.
    pub fn new(db_path: Arc<String>, config: SessionRestoreConfig) -> Self {
        Self { db_path, config }
    }

    /// Whether auto-restore is enabled (skip user prompt).
    pub fn auto_restore(&self) -> bool {
        self.config.auto_restore
    }

    /// Detect unclean sessions that can be restored.
    ///
    /// Returns the candidate with the newest bounded, non-empty snapshot
    /// descriptor. Complete JSON decoding and witness verification happen when
    /// that exact checkpoint is loaded; detection never materializes every
    /// historical pane projection.
    pub fn detect(&self) -> Result<Option<SessionCandidate>, RestoreError> {
        with_query_snapshot(&self.db_path, |conn| self.detect_from_conn(conn))
    }

    fn detect_from_conn(
        &self,
        conn: &Connection,
    ) -> Result<Option<SessionCandidate>, RestoreError> {
        let mut candidates = find_unclean_sessions_from_conn(conn)?;

        if candidates.is_empty() {
            debug!("No unclean sessions found — clean startup");
            return Ok(None);
        }

        info!(
            count = candidates.len(),
            "Detected unclean session(s) from previous run"
        );
        let unclean_session_count = candidates.len();
        candidates.sort_unstable_by(|left, right| {
            right
                .selected_checkpoint_id
                .unwrap_or(i64::MIN)
                .cmp(&left.selected_checkpoint_id.unwrap_or(i64::MIN))
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        let mut first_error: Option<RestoreError> = None;
        let mut block_reason: Option<UncleanSessionBlockReason> = None;

        for candidate in candidates {
            if let Some((intent_checkpoint_id, outcome_checkpoint_id, status)) =
                restore_ambiguity_from_conn(conn, &candidate.session_id)?
            {
                let error = RestoreError::RestoreAttemptRequiresReconciliation {
                    session_id: candidate.session_id.clone(),
                    intent_checkpoint_id,
                    outcome_checkpoint_id,
                    status,
                };
                warn!(
                    session_id_bytes = candidate.session_id.len(),
                    "Skipping session with an unresolved restore attempt"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            let Some(checkpoint_id) = candidate.selected_checkpoint_id else {
                warn!(
                    session_id_bytes = candidate.session_id.len(),
                    "Skipping unclean session with no snapshot checkpoint"
                );
                record_unclean_block_reason(
                    &mut block_reason,
                    UncleanSessionBlockReason::NoSnapshotCheckpoint,
                );
                continue;
            };
            match restore_candidate_checkpoint_from_conn(conn, checkpoint_id) {
                Ok(Some(checkpoint))
                    if self.config.auto_restore
                        && checkpoint.verification == CheckpointVerification::LegacyUnverified =>
                {
                    warn!(
                        session_id_bytes = candidate.session_id.len(),
                        checkpoint_id,
                        checkpoint_at = checkpoint.checkpoint_at,
                        "Skipping legacy-unverified checkpoint in auto-restore mode"
                    );
                    record_unclean_block_reason(
                        &mut block_reason,
                        UncleanSessionBlockReason::LegacyUnverifiedAutoRestore,
                    );
                }
                Ok(Some(checkpoint)) => {
                    info!(
                        session_id_bytes = candidate.session_id.len(),
                        checkpoint_id,
                        checkpoint_at = checkpoint.checkpoint_at,
                        ft_version_bytes = candidate.ft_version.len(),
                        "Best restore candidate identified from bounded descriptor"
                    );
                    return Ok(Some(candidate));
                }
                Ok(None) => {
                    warn!(
                        session_id_bytes = candidate.session_id.len(),
                        checkpoint_id,
                        "Skipping unclean session with empty or missing checkpoint data"
                    );
                    record_unclean_block_reason(
                        &mut block_reason,
                        UncleanSessionBlockReason::EmptyOrMissingCheckpoint,
                    );
                }
                Err(error) => {
                    warn!(
                        session_id_bytes = candidate.session_id.len(),
                        "Skipping unclean session with unreadable checkpoint data"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Err(RestoreError::UncleanSessionsNotRestorable {
            session_count: unclean_session_count,
            reason: block_reason.unwrap_or(UncleanSessionBlockReason::Mixed),
        })
    }

    /// Load checkpoint data for a session candidate.
    pub fn load_checkpoint(
        &self,
        session: &SessionCandidate,
    ) -> Result<CheckpointData, RestoreError> {
        let checkpoint = match session.selected_checkpoint_id {
            Some(checkpoint_id) => load_checkpoint_by_id(&self.db_path, checkpoint_id)?,
            None => load_latest_checkpoint(&self.db_path, &session.session_id)?,
        }
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint("no checkpoints found for session".to_string())
        })?;
        if checkpoint.session_id != session.session_id {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "selected checkpoint {} belongs to another session",
                checkpoint.checkpoint_id
            )));
        }

        info!(
            session_id_bytes = session.session_id.len(),
            checkpoint_id = checkpoint.checkpoint_id,
            checkpoint_at = checkpoint.checkpoint_at,
            checkpoint_role = %checkpoint.checkpoint_role,
            verification = ?checkpoint.verification,
            pane_count = checkpoint.pane_count,
            loaded_panes = checkpoint.pane_states.len(),
            "Loaded checkpoint for restore"
        );

        Ok(checkpoint)
    }

    /// Execute the full restore: recreate layout and record its causal mapping.
    ///
    /// The `wezterm` handle must be connected to a running WezTerm instance.
    pub async fn restore(
        &self,
        session: &SessionCandidate,
        checkpoint: &CheckpointData,
        wezterm: WeztermHandle,
    ) -> Result<RestoreSummary, RestoreError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.restore_with_cx(&cx, session, checkpoint, wezterm).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`restore`].
    ///
    /// Tick 129 upgraded this from a pre-flight delegate to a
    /// fully cx-threaded multi-step pipeline. Caller cancellation
    /// now propagates into every subsystem:
    /// - `LayoutRestorer::restore_with_cx` (tick 93)
    /// - fail-closed scrollback capability preflight before any mux effect
    /// - no PTY-input banner or historical-output injection
    /// - `ProcessLauncher::execute_cx` for audited manual dispositions
    ///
    /// Per-step `cx.checkpoint()` seams also gate each stage so a
    /// cancelled caller can bail between topology parse, layout,
    /// scrollback preflight, bookkeeping, and process disposition.
    pub async fn restore_with_cx(
        &self,
        cx: &crate::cx::Cx,
        session: &SessionCandidate,
        checkpoint: &CheckpointData,
        wezterm: WeztermHandle,
    ) -> Result<RestoreSummary, RestoreError> {
        cx.checkpoint()
            .map_err(|error| restore_context_error("restore preflight", cx, &error))?;

        // Treat the caller's checkpoint as an identifier, not as authority.
        // The row and its complete witnessed pane projection may have changed
        // between detection and admission, so reload it before parsing caller-
        // supplied topology or making any external mux call.
        let requested_checkpoint_id = checkpoint.checkpoint_id;
        let mut checkpoint = load_checkpoint_by_id_with_cx(
            cx,
            self.db_path.as_str(),
            requested_checkpoint_id,
        )
        .await?
        .ok_or_else(|| {
            RestoreError::CorruptCheckpoint(format!(
                "restore checkpoint {requested_checkpoint_id} disappeared before admission"
            ))
        })?;

        if checkpoint.session_id != session.session_id {
            return Err(RestoreError::CorruptCheckpoint(format!(
                "checkpoint {} belongs to session {}, not {}",
                checkpoint.checkpoint_id, checkpoint.session_id, session.session_id
            )));
        }
        if checkpoint.checkpoint_role != CheckpointRole::Snapshot {
            return Err(RestoreError::CheckpointNotRestorable {
                checkpoint_id: checkpoint.checkpoint_id,
                role: checkpoint.checkpoint_role.to_string(),
            });
        }
        if self.config.auto_restore
            && checkpoint.verification == CheckpointVerification::LegacyUnverified
        {
            return Err(RestoreError::LegacyCheckpointRequiresManualRestore {
                checkpoint_id: checkpoint.checkpoint_id,
            });
        }
        if self.config.restore_scrollback {
            // Historical output must never be written through `send_text`:
            // that API targets PTY input and can execute captured output as a
            // shell command. Fail before the durable intent and before any mux
            // effect until the mux exposes a dedicated render-state/output
            // restoration channel.
            return Err(RestoreError::SafeScrollbackReplayUnavailable);
        }

        let start = std::time::Instant::now();

        let topology_json = checkpoint.topology_json.take().ok_or(
            RestoreError::CheckpointTopologyUnavailable {
                checkpoint_id: checkpoint.checkpoint_id,
            },
        )?;
        let checkpoint_id = checkpoint.checkpoint_id;
        let pane_count = checkpoint.pane_count;
        let persisted_pane_ids: Vec<u64> = checkpoint
            .pane_states
            .iter()
            .map(|pane| pane.pane_id)
            .collect();
        let topology = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let topology = TopologySnapshot::from_json(&topology_json).map_err(|_error| {
                RestoreError::TopologyParse(
                    "checkpoint topology failed structural decoding".to_string(),
                )
            })?;
            validate_restore_topology(
                checkpoint_id,
                pane_count,
                &persisted_pane_ids,
                &topology,
            )?;
            Ok::<_, RestoreError>(topology)
        })
        .await
        .map_err(|error| restore_blocking_error("topology preparation", error))??;

        let total_panes = topology.pane_count();

        info!(
            session_id_bytes = session.session_id.len(),
            windows = topology.windows.len(),
            panes = total_panes,
            "Restoring session topology (cx-first)"
        );

        cx.checkpoint()
            .map_err(|error| restore_context_error("before layout", cx, &error))?;

        let intent_source = RestoreIntentSource {
            checkpoint_id: checkpoint.checkpoint_id,
            checkpoint_at: checkpoint.checkpoint_at,
            checkpoint_role: checkpoint.checkpoint_role,
            state_hash: checkpoint.state_hash.clone(),
            pane_count: checkpoint.pane_count,
        };
        let intent_db_path = Arc::clone(&self.db_path);
        let intent_session_id = session.session_id.clone();
        let intent_source_for_commit = intent_source.clone();
        let intent = run_checkpoint_authority_with_cx(
            cx,
            intent_db_path,
            SnapshotAuthorityOperation::RestoreIntentCommit,
            move |db_path| {
                persist_restore_intent_unclean(
                    db_path,
                    &intent_session_id,
                    &intent_source_for_commit,
                )
            },
        )
        .await
        .map_err(|error| restore_authority_error("restore intent commit", error))?;
        debug!(
            checkpoint_id = intent.checkpoint_id,
            source_checkpoint_id = checkpoint.checkpoint_id,
            "Persisted restore intent before external mux effects"
        );

        let restore_config = RestoreConfig {
            restore_working_dirs: true,
            restore_split_ratios: true,
            continue_on_error: true,
        };
        let restorer = LayoutRestorer::new(wezterm.clone(), restore_config);
        let layout_attempt = restorer.restore_attempt_with_cx(cx, &topology).await;
        let mut attempt_interruption = layout_attempt.interruption.map(|interruption| {
            RestoreAttemptInterruption {
                phase: interruption.phase,
                receipt_phase: RestoreInterruptionPhase::LayoutRestoration,
                reason: restore_layout_interruption_reason(interruption.reason),
            }
        });
        let layout_result = layout_attempt.result;

        info!(
            panes_created = layout_result.panes_created,
            windows_created = layout_result.windows_created,
            tabs_created = layout_result.tabs_created,
            failed = layout_result.failed_panes.len(),
            "Layout restoration complete"
        );

        for (old_id, _error) in &layout_result.failed_panes {
            warn!(old_pane_id = old_id, "Failed to restore pane");
        }
        let expected_pane_ids: HashSet<u64> = checkpoint
            .pane_states
            .iter()
            .map(|pane| pane.pane_id)
            .collect();
        let mapped_pane_ids: HashSet<u64> =
            layout_result.pane_id_map.keys().copied().collect();
        let reported_failed_expected_pane_ids = layout_result
            .failed_panes
            .iter()
            .map(|(pane_id, _error)| *pane_id)
            .filter(|pane_id| expected_pane_ids.contains(pane_id))
            .collect::<HashSet<_>>();
        let unique_target_count = layout_result
            .pane_id_map
            .values()
            .copied()
            .collect::<HashSet<_>>()
            .len();
        let duplicate_target_source_pane_id_set =
            duplicate_target_source_pane_ids(&layout_result.pane_id_map);
        let unexpected_mapping_count = layout_result
            .pane_id_map
            .keys()
            .filter(|pane_id| !expected_pane_ids.contains(pane_id))
            .count();
        let unexpected_failure_count = layout_result
            .failed_panes
            .iter()
            .map(|(pane_id, _error)| *pane_id)
            .filter(|pane_id| !expected_pane_ids.contains(pane_id))
            .collect::<HashSet<_>>()
            .len();
        let layout_complete = attempt_interruption.is_none()
            && layout_result.failed_panes.is_empty()
            && mapped_pane_ids == expected_pane_ids
            && unique_target_count == layout_result.pane_id_map.len();
        let mut failed_source_pane_ids = expected_pane_ids
            .iter()
            .filter(|pane_id| {
                !mapped_pane_ids.contains(pane_id)
                    || reported_failed_expected_pane_ids.contains(pane_id)
                    || duplicate_target_source_pane_id_set.contains(pane_id)
            })
            .copied()
            .collect::<Vec<_>>();
        failed_source_pane_ids.sort_unstable();
        let failed_expected_pane_count = failed_source_pane_ids.len();
        let mut duplicate_target_source_pane_ids = duplicate_target_source_pane_id_set
            .iter()
            .copied()
            .filter(|pane_id| expected_pane_ids.contains(pane_id))
            .collect::<Vec<_>>();
        duplicate_target_source_pane_ids.sort_unstable();
        let restored_expected_pane_count = expected_pane_ids
            .len()
            .saturating_sub(failed_expected_pane_count);
        if !layout_complete && layout_result.failed_panes.is_empty() {
            warn!(
                expected_panes = expected_pane_ids.len(),
                mapped_panes = mapped_pane_ids.len(),
                unique_target_panes = unique_target_count,
                "Layout restorer omitted, duplicated, or added pane mappings without reporting failures"
            );
        }

        if attempt_interruption.is_none()
            && let Err(error) = cx.checkpoint()
        {
            attempt_interruption = Some(RestoreAttemptInterruption {
                phase: "post-layout checkpoint",
                receipt_phase: RestoreInterruptionPhase::PostLayoutCheckpoint,
                reason: restore_interruption_reason(cx, &error),
            });
        }

        // Do not inject a restore banner through the PTY input API. Persisted
        // command/agent strings are untrusted terminal data, and even a fully
        // escaped banner would still be shell input rather than display state.
        debug!(
            panes = layout_result.pane_id_map.len(),
            "Restore layout created without unsafe PTY-input banner injection"
        );

        if attempt_interruption.is_none() {
            if let Err(error) = cx.checkpoint() {
                attempt_interruption = Some(RestoreAttemptInterruption {
                    phase: "pre-process-disposition checkpoint",
                    receipt_phase: RestoreInterruptionPhase::PreProcessDispositionCheckpoint,
                    reason: restore_interruption_reason(cx, &error),
                });
            }
        }

        let mut process_disposition_complete = layout_complete;
        let mut process_launch_report = None;
        let mut process_plans_total = 0usize;
        if attempt_interruption.is_some() {
            process_disposition_complete = false;
            warn!(
                session_id_bytes = session.session_id.len(),
                intent_checkpoint_id = intent.checkpoint_id,
                "Layout attempt stopped; skipping all further external effects and settling an unclean outcome"
            );
        } else if !layout_complete {
            warn!(
                session_id_bytes = session.session_id.len(),
                restored_panes = restored_expected_pane_count,
                failed_panes = failed_expected_pane_count,
                "Session restore incomplete; leaving source session unclean for reconciliation"
            );
        } else {
            let plans = ProcessLauncher::plan_inputs(
                &layout_result.pane_id_map,
                checkpoint.pane_states.iter().map(|state| ProcessDispositionInput {
                    pane_id: state.pane_id,
                    foreground_process_name: state
                        .command
                        .as_deref()
                        .filter(|command| !command.is_empty()),
                    shell_present: false,
                    agent_present: state.agent_metadata.is_some(),
                }),
            );
            process_plans_total = plans.len();
            if !plans.is_empty() {
                let report = ProcessLauncher::execute_cx(cx, &plans);
                // Captured process plans are audit-only manual dispositions.
                // They do not make an otherwise complete layout restore fail.
                process_disposition_complete = report.interruption().is_none()
                    && report.plans_settled() == report.plans_total();
                if report.manual_count() > 0
                    || report.interruption().is_some()
                {
                    warn!(
                        session_id_bytes = session.session_id.len(),
                        plans_total = report.plans_total(),
                        plans_settled = report.plans_settled(),
                        sampled_results = report.result_sample().len(),
                        manual = report.manual_count(),
                        skipped = report.skipped_count(),
                        interrupted = report.interruption().is_some(),
                        "Captured process dispositions require manual follow-up"
                    );
                } else {
                    info!(
                        session_id_bytes = session.session_id.len(),
                        plans_total = report.plans_total(),
                        plans_settled = report.plans_settled(),
                        sampled_results = report.result_sample().len(),
                        skipped = report.skipped_count(),
                        "Captured process disposition evaluation complete"
                    );
                }
                process_launch_report = Some(report);
            }
        }

        let process_report = process_launch_report.as_ref();
        let process_interruption = process_report
            .and_then(LaunchReport::interruption)
            .map(|interruption| RestoreAttemptInterruption {
                phase: "process disposition evaluation",
                receipt_phase: RestoreInterruptionPhase::ProcessDispositionEvaluation,
                reason: restore_process_interruption_reason(interruption.reason),
            });
        if attempt_interruption.is_none() {
            attempt_interruption = process_interruption;
        }
        if attempt_interruption.is_none() {
            if let Err(error) = cx.checkpoint() {
                attempt_interruption = Some(RestoreAttemptInterruption {
                    phase: "post-process-disposition checkpoint",
                    receipt_phase: RestoreInterruptionPhase::PostProcessDispositionCheckpoint,
                    reason: restore_interruption_reason(cx, &error),
                });
            }
        }
        let outcome_evidence = RestoreOutcomeEvidence {
            evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
            intent: intent.clone(),
            source: intent_source,
            expected_panes: expected_pane_ids.len(),
            mapped_panes: 0,
            reported_layout_failures: reported_failed_expected_pane_ids.len(),
            failed_source_pane_ids,
            unexpected_mapping_count,
            unexpected_failure_count,
            duplicate_target_source_pane_ids,
            layout_complete,
            scrollback_requested: false,
            scrollback_complete: true,
            scrollback_failures: 0,
            scrollback_skipped: 0,
            scrollback_global_error: false,
            process_plan_evaluated: process_report.is_some(),
            process_plans_total,
            process_plans_settled: process_report.map_or(0, LaunchReport::plans_settled),
            process_interrupted: process_report
                .is_some_and(|report| report.interruption().is_some()),
            attempt_interrupted: attempt_interruption.is_some(),
            interruption_phase: attempt_interruption
                .map(|interruption| interruption.receipt_phase),
            interruption_reason: attempt_interruption.map(|interruption| interruption.reason),
            // The current evaluator has only settled Manual/Skip categories;
            // retain the legacy compatibility field as an enforced
            // zero until a schema migration removes it atomically.
            process_failed: 0,
            process_manual: process_report.map_or(0, LaunchReport::manual_count),
            process_skipped: process_report.map_or(0, LaunchReport::skipped_count),
        };
        // Persist only deterministic, source-owned, one-to-one mappings. A
        // malformed backend result (unexpected source IDs or duplicate target
        // IDs) must still settle to an unclean outcome receipt rather than
        // losing durable evidence after external effects have occurred.
        let persisted_pane_id_map = authoritative_persisted_pane_mapping(
            &expected_pane_ids,
            &outcome_evidence.failed_source_pane_ids,
            &layout_result.pane_id_map,
        );
        let mut outcome_evidence = outcome_evidence;
        outcome_evidence.mapped_panes = persisted_pane_id_map.len();
        let restore_complete = restore_outcome_evidence_is_complete(&outcome_evidence)
            && process_disposition_complete;
        let db_path = Arc::clone(&self.db_path);
        let session_id = session.session_id.clone();
        let pane_id_map = persisted_pane_id_map;
        let receipt_evidence = outcome_evidence.clone();
        // Once external mux effects have started, settlement must not be
        // skipped merely because the caller canceled. Use a fresh, bounded
        // capability only for the durable outcome receipt; never use it to
        // continue layout or type additional process commands.
        let receipt_cx = crate::cx::Cx::for_request_with_budget(crate::cx::Budget::MINIMAL);
        let receipt = run_checkpoint_authority_with_cx(
            &receipt_cx,
            db_path,
            SnapshotAuthorityOperation::RestoreReceiptCommit,
            move |db_path| {
                persist_restore_receipt_settled(
                    db_path,
                    &session_id,
                    &pane_id_map,
                    &receipt_evidence,
                    restore_complete,
                )
            },
        )
        .await
        .map_err(|error| RestoreError::RestoreAttemptInterrupted {
            session_id: session.session_id.clone(),
            intent_checkpoint_id: intent.checkpoint_id,
            outcome_checkpoint_id: None,
            phase: "outcome receipt commit",
            reason: restore_snapshot_interruption_reason(&error),
        })?;
        debug!(
            checkpoint_id = receipt.checkpoint_id,
            intent_checkpoint_id = intent.checkpoint_id,
            restore_complete,
            "Persisted and transactionally settled restore outcome receipt"
        );

        if let Some(interruption) = attempt_interruption {
            return Err(RestoreError::RestoreAttemptInterrupted {
                session_id: session.session_id.clone(),
                intent_checkpoint_id: intent.checkpoint_id,
                outcome_checkpoint_id: Some(receipt.checkpoint_id),
                phase: interruption.phase,
                reason: interruption.reason,
            });
        }

        if restore_complete {
            debug!(
                checkpoint_id = receipt.checkpoint_id,
                "Restore receipt, clean authority, and lifecycle resolved atomically"
            );
        } else if layout_complete {
            warn!(
                session_id_bytes = session.session_id.len(),
                checkpoint_id = receipt.checkpoint_id,
                "Process disposition evaluation requires follow-up; leaving source session unclean"
            );
        }

        let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let restore_status = if restore_complete {
            "complete"
        } else {
            "partial"
        };

        info!(
            session_id_bytes = session.session_id.len(),
            restored = restored_expected_pane_count,
            failed = failed_expected_pane_count,
            total = total_panes,
            status = restore_status,
            elapsed_ms = elapsed,
            "Session restore finished (cx-first)"
        );

        let summary = RestoreSummary {
            session_id: session.session_id.clone(),
            checkpoint_id: checkpoint.checkpoint_id,
            intent_checkpoint_id: intent.checkpoint_id,
            outcome_checkpoint_id: receipt.checkpoint_id,
            layout_result,
            pane_states: checkpoint.pane_states,
            process_launch_report,
            restore_authority_resolved: restore_complete,
            elapsed_ms: elapsed,
        };
        if restore_complete {
            Ok(summary)
        } else {
            Err(RestoreError::PartialRestore {
                summary: Box::new(summary),
            })
        }
    }

    /// Run the full detection + restore flow.
    ///
    /// Returns `None` if no restore is needed (clean startup).
    pub async fn detect_and_restore(
        &self,
        wezterm: WeztermHandle,
    ) -> Result<Option<RestoreSummary>, RestoreError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.detect_and_restore_with_cx(&cx, wezterm).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`detect_and_restore`].
    ///
    /// Checkpoint seams at each step boundary make cancellation
    /// responsive between (1) pre-flight, (2) detect, (3)
    /// checkpoint load, (4) WezTerm reachability check, and
    /// (5) before the actual restore fires. The inner restore is
    /// routed through [`restore_with_cx`] so cancellation
    /// propagates into the expensive multi-step pipeline.
    pub async fn detect_and_restore_with_cx(
        &self,
        cx: &crate::cx::Cx,
        wezterm: WeztermHandle,
    ) -> Result<Option<RestoreSummary>, RestoreError> {
        cx.checkpoint().map_err(|error| {
            restore_context_error("detect-and-restore preflight", cx, &error)
        })?;

        let detect_db_path = Arc::clone(&self.db_path);
        let detect_config = self.config.clone();
        let detected = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            SessionRestorer::new(detect_db_path, detect_config).detect()
        })
        .await
        .map_err(|error| restore_blocking_error("restore detection", error))?;
        let session = match detected? {
            Some(s) => s,
            None => return Ok(None),
        };

        cx.checkpoint().map_err(|error| {
            restore_context_error("before detected checkpoint load", cx, &error)
        })?;

        let load_db_path = Arc::clone(&self.db_path);
        let load_config = self.config.clone();
        let session_for_load = session.clone();
        let checkpoint = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            SessionRestorer::new(load_db_path, load_config)
                .load_checkpoint(&session_for_load)
        })
        .await
        .map_err(|error| restore_blocking_error("detected checkpoint load", error))??;

        if let Err(error) = ensure_detected_checkpoint_remains_nonempty(&checkpoint) {
            warn!(
                session_id_bytes = session.session_id.len(),
                checkpoint_id = checkpoint.checkpoint_id,
                "Checkpoint became empty after detection; refusing a false clean/no-op result"
            );
            return Err(error);
        }

        cx.checkpoint().map_err(|error| {
            restore_context_error("before mux reachability check", cx, &error)
        })?;

        // ft-xbnl0.2.3 tick 129: route list_panes through cx-first.
        match wezterm.list_panes_with_cx(cx).await {
            Ok(panes) if !panes.is_empty() => {
                info!(
                    existing_panes = panes.len(),
                    "WezTerm has existing panes; restore will create new panes alongside them"
                );
            }
            Ok(_) => {
                debug!("WezTerm running with no panes — clean slate for restore");
            }
            Err(_error) => {
                return Err(RestoreError::Wezterm(
                    "cannot reach the mux service".to_string(),
                ));
            }
        }

        let summary = self
            .restore_with_cx(cx, &session, &checkpoint, wezterm)
            .await?;

        Ok(Some(summary))
    }
}

// =============================================================================
// Display helpers
// =============================================================================

/// Format a restore summary for human display.
pub fn format_restore_summary(summary: &RestoreSummary) -> String {
    const MAX_FAILURE_DETAILS: usize = 20;

    let mut out = String::new();
    let expected_pane_ids = summary
        .pane_states
        .iter()
        .map(|pane| pane.pane_id)
        .collect::<HashSet<_>>();
    let failed_expected_pane_ids = summary.failed_expected_pane_ids_for(&expected_pane_ids);
    let failed_count = failed_expected_pane_ids.len();
    let restored_count = summary
        .expected_pane_count()
        .saturating_sub(failed_count);
    let status = if summary.restore_authority_resolved {
        "layout/authority settled"
    } else {
        "layout/authority partial"
    };
    out.push_str(&format!(
        "Session {}: {} for {}/{} panes in {}ms\n",
        summary.session_id,
        status,
        restored_count,
        summary.expected_pane_count(),
        summary.elapsed_ms,
    ));
    out.push_str(
        "Process continuity, historical scrollback, and full-session continuity were not restored.\n",
    );

    if failed_count > 0 {
        out.push_str("Failed panes:\n");
        let mut smallest_failed_pane_ids = BinaryHeap::with_capacity(MAX_FAILURE_DETAILS + 1);
        for &pane_id in &failed_expected_pane_ids {
            if smallest_failed_pane_ids.len() < MAX_FAILURE_DETAILS {
                smallest_failed_pane_ids.push(pane_id);
            } else if smallest_failed_pane_ids
                .peek()
                .is_some_and(|largest| pane_id < *largest)
            {
                smallest_failed_pane_ids.pop();
                smallest_failed_pane_ids.push(pane_id);
            }
        }
        let mut failed_pane_ids = smallest_failed_pane_ids.into_vec();
        failed_pane_ids.sort_unstable();
        let explicitly_failed = summary
            .layout_result
            .failed_panes
            .iter()
            .map(|(pane_id, _error)| *pane_id)
            .filter(|pane_id| failed_pane_ids.binary_search(pane_id).is_ok())
            .collect::<HashSet<_>>();
        let duplicate_target_sources =
            duplicate_target_source_pane_ids(&summary.layout_result.pane_id_map);
        for pane_id in &failed_pane_ids {
            if explicitly_failed.contains(pane_id) {
                out.push_str(&format!(
                    "  pane {pane_id}: layout restoration reported failure\n"
                ));
            } else if duplicate_target_sources.contains(pane_id) {
                out.push_str(&format!(
                    "  pane {pane_id}: layout mapping collided on a duplicate target pane\n"
                ));
            } else {
                out.push_str(&format!(
                    "  pane {pane_id}: layout backend returned no mapping or explicit failure\n"
                ));
            }
        }
        let omitted = failed_count.saturating_sub(failed_pane_ids.len());
        if omitted > 0 {
            out.push_str(&format!("  ... {omitted} additional failed panes omitted\n"));
        }
    }

    let unexpected_mapping_count = summary
        .layout_result
        .pane_id_map
        .keys()
        .filter(|pane_id| !expected_pane_ids.contains(pane_id))
        .count();
    let unexpected_failure_count = summary
        .layout_result
        .failed_panes
        .iter()
        .map(|(pane_id, _error)| *pane_id)
        .filter(|pane_id| !expected_pane_ids.contains(pane_id))
        .collect::<HashSet<_>>()
        .len();
    let unique_target_count = summary
        .layout_result
        .pane_id_map
        .values()
        .copied()
        .collect::<HashSet<_>>()
        .len();
    let duplicate_target_count = summary
        .layout_result
        .pane_id_map
        .len()
        .saturating_sub(unique_target_count);
    if unexpected_mapping_count > 0
        || unexpected_failure_count > 0
        || duplicate_target_count > 0
    {
        out.push_str(&format!(
            "Layout integrity anomalies: {unexpected_mapping_count} unexpected mappings, {unexpected_failure_count} unexpected failures, {duplicate_target_count} duplicate targets\n"
        ));
    }

    if let Some(report) = &summary.process_launch_report {
        out.push_str(&format!(
            "Process disposition: {} of {} plans settled, {} manual, {} skipped; {} content-free sample entries retained (cap {})\n",
            report.plans_settled(),
            report.plans_total(),
            report.manual_count(),
            report.skipped_count(),
            report.result_sample().len(),
            crate::restore_process::LAUNCH_RESULT_SAMPLE_CAP,
        ));
        if report.interruption().is_some() {
            out.push_str(
                "Process disposition evaluation was interrupted before every plan settled.\n",
            );
        }
    }
    if !summary.restore_authority_resolved {
        out.push_str(&format!(
            "Source session remains unclean. Reconcile restore intent {} and outcome {} before retrying.\n",
            summary.intent_checkpoint_id, summary.outcome_checkpoint_id
        ));
    }

    out
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::session_topology::{PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot};
    use crate::wezterm::{
        MockWezterm, MoveDirection, SplitDirection, WeztermFuture, WeztermInterface,
    };
    use rusqlite::params;

    fn test_runtime_error(operation: &'static str, detail: impl Into<String>) -> crate::Error {
        crate::Error::RuntimeOperation {
            operation,
            source: crate::error::RuntimeOperationSource::Backend(detail.into()),
        }
    }

    fn summary_pane_state(pane_id: u64) -> RestoredPaneState {
        RestoredPaneState {
            pane_id,
            cwd: None,
            command: None,
            terminal_state: None,
            agent_metadata: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        }
    }

    fn persisted_outcome_with_process_counts(
        process_plans_total: usize,
        process_plans_settled: usize,
        process_failed: usize,
        process_manual: usize,
        process_skipped: usize,
    ) -> PersistedRestoreCheckpointMetadata {
        let ordinary_size = process_plans_total <= 64;
        let old_to_new = if ordinary_size {
            (0..process_plans_total)
                .map(|pane_id| {
                    (
                        pane_id.to_string(),
                        u64::try_from(pane_id)
                            .expect("small test pane id fits u64")
                            .saturating_add(100),
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        };
        PersistedRestoreCheckpointMetadata {
            old_to_new,
            restore_attempt: Some(PersistedRestoreAttempt::Outcome {
                evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
                intent_checkpoint_id: 2,
                intent_checkpoint_at: 2,
                intent_state_hash: "rsi2:test-intent".to_string(),
                source_checkpoint_id: 1,
                source_checkpoint_at: 1,
                source_checkpoint_role: CHECKPOINT_ROLE_SNAPSHOT.to_string(),
                source_state_hash: "test-source".to_string(),
                expected_panes: if ordinary_size { process_plans_total } else { 0 },
                mapped_panes: if ordinary_size { process_plans_total } else { 0 },
                reported_layout_failures: 0,
                failed_source_pane_ids: Some(Vec::new()),
                unexpected_mapping_count: Some(0),
                unexpected_failure_count: Some(0),
                duplicate_target_source_pane_ids: Some(Vec::new()),
                layout_complete: ordinary_size,
                scrollback_requested: false,
                scrollback_complete: true,
                scrollback_failures: 0,
                scrollback_skipped: 0,
                scrollback_global_error: false,
                process_plan_evaluated: process_plans_total > 0,
                process_plans_total,
                process_plans_settled,
                process_interrupted: false,
                attempt_interrupted: false,
                interruption_phase: None,
                interruption_reason: None,
                process_failed,
                process_manual,
                process_skipped,
            }),
        }
    }

    #[test]
    fn outcome_process_dispositions_exactly_cover_settled_plans() {
        let exact = persisted_outcome_with_process_counts(2, 2, 0, 2, 0);
        validate_restore_outcome_metadata(3, &exact).expect("exact dispositions are valid");

        let missing = persisted_outcome_with_process_counts(2, 2, 0, 1, 0);
        assert!(matches!(
            validate_restore_outcome_metadata(3, &missing),
            Err(RestoreError::CorruptCheckpoint(_))
        ));

        let mut empty_but_claimed_evaluated =
            persisted_outcome_with_process_counts(0, 0, 0, 0, 0);
        let Some(PersistedRestoreAttempt::Outcome {
            process_plan_evaluated,
            ..
        }) = empty_but_claimed_evaluated.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *process_plan_evaluated = true;
        assert!(matches!(
            validate_restore_outcome_metadata(3, &empty_but_claimed_evaluated),
            Err(RestoreError::CorruptCheckpoint(_))
        ));
    }

    #[test]
    fn outcome_evidence_versions_preserve_v2_and_fail_closed_for_malformed_v3() {
        let mut legacy_v2 = persisted_outcome_with_process_counts(1, 1, 0, 1, 0);
        let Some(PersistedRestoreAttempt::Outcome {
            evidence_version,
            failed_source_pane_ids,
            unexpected_mapping_count,
            unexpected_failure_count,
            duplicate_target_source_pane_ids,
            ..
        }) = legacy_v2.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *evidence_version = RESTORE_OUTCOME_REASON_EVIDENCE_VERSION;
        *failed_source_pane_ids = None;
        *unexpected_mapping_count = None;
        *unexpected_failure_count = None;
        *duplicate_target_source_pane_ids = None;
        validate_restore_outcome_metadata(3, &legacy_v2)
            .expect("pre-v3 reason-bearing evidence remains readable");

        let mut missing_v3_field = persisted_outcome_with_process_counts(1, 1, 0, 1, 0);
        let Some(PersistedRestoreAttempt::Outcome {
            failed_source_pane_ids,
            ..
        }) = missing_v3_field.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *failed_source_pane_ids = None;
        assert!(matches!(
            validate_restore_outcome_metadata(3, &missing_v3_field),
            Err(RestoreError::CorruptCheckpoint(_))
        ));

        let mut unsorted_v3 = persisted_outcome_with_process_counts(2, 0, 0, 0, 0);
        let Some(PersistedRestoreAttempt::Outcome {
            mapped_panes,
            reported_layout_failures,
            failed_source_pane_ids,
            layout_complete,
            process_plan_evaluated,
            ..
        }) = unsorted_v3.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *mapped_panes = 0;
        *reported_layout_failures = 2;
        *failed_source_pane_ids = Some(vec![1, 1]);
        *layout_complete = false;
        *process_plan_evaluated = false;
        unsorted_v3.old_to_new.clear();
        assert!(matches!(
            validate_restore_outcome_metadata(3, &unsorted_v3),
            Err(RestoreError::CorruptCheckpoint(_))
        ));

        let mut unknown_version = persisted_outcome_with_process_counts(1, 1, 0, 1, 0);
        let Some(PersistedRestoreAttempt::Outcome {
            evidence_version, ..
        }) = unknown_version.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *evidence_version = RESTORE_OUTCOME_EVIDENCE_VERSION.saturating_add(1);
        assert!(matches!(
            validate_restore_outcome_metadata(3, &unknown_version),
            Err(RestoreError::CorruptCheckpoint(_))
        ));
    }

    #[test]
    fn v3_outcome_rejects_source_identity_claimed_as_both_mapped_and_failed() {
        let mut overlap = persisted_outcome_with_process_counts(2, 2, 0, 2, 0);
        overlap.old_to_new.remove("0");
        let Some(PersistedRestoreAttempt::Outcome {
            mapped_panes,
            reported_layout_failures,
            failed_source_pane_ids,
            layout_complete,
            process_plan_evaluated,
            process_plans_total,
            process_plans_settled,
            attempt_interrupted,
            interruption_phase,
            interruption_reason,
            process_manual,
            ..
        }) = overlap.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *mapped_panes = 1;
        *reported_layout_failures = 1;
        *failed_source_pane_ids = Some(vec![1]);
        *layout_complete = false;
        *process_plan_evaluated = false;
        *process_plans_total = 0;
        *process_plans_settled = 0;
        *process_manual = 0;
        *attempt_interrupted = true;
        *interruption_phase = Some(RestoreInterruptionPhase::LayoutRestoration);
        *interruption_reason = Some(RestoreInterruptionReason::BackendFailure);

        assert!(matches!(
            validate_restore_outcome_metadata(3, &overlap),
            Err(RestoreError::CorruptCheckpoint(_))
        ));
    }

    #[test]
    fn persisted_mapping_excludes_explicit_failures_and_all_duplicate_targets() {
        let expected = HashSet::from([1_u64, 2, 3]);
        let explicit_raw = HashMap::from([(1_u64, 10_u64), (2, 20), (3, 30)]);

        let explicit_failure =
            authoritative_persisted_pane_mapping(&expected, &[1], &explicit_raw);
        assert_eq!(explicit_failure, HashMap::from([(2, 20), (3, 30)]));

        let duplicate_raw = HashMap::from([(1_u64, 10_u64), (2, 20), (3, 20)]);
        let duplicate_failures =
            authoritative_persisted_pane_mapping(&expected, &[], &duplicate_raw);
        assert_eq!(duplicate_failures, HashMap::from([(1, 10)]));
    }

    #[test]
    fn v3_outcome_missing_unmapped_source_identity_rolls_back_atomically() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v3-coverage", false);
        let source_id = insert_checkpoint(&conn, "sess-v3-coverage", 5_000, 1);
        insert_pane_state(&conn, source_id, 7, Some("/tmp"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-v3-coverage'",
            [],
        )
        .expect("source must be the exact latest session authority");
        let source_checkpoint = load_checkpoint_by_id(&db_path, source_id)
            .expect("source lookup should succeed")
            .expect("source checkpoint should exist");
        let source = RestoreIntentSource {
            checkpoint_id: source_checkpoint.checkpoint_id,
            checkpoint_at: source_checkpoint.checkpoint_at,
            checkpoint_role: source_checkpoint.checkpoint_role,
            state_hash: source_checkpoint.state_hash,
            pane_count: source_checkpoint.pane_count,
        };
        let intent = persist_restore_intent_unclean(&db_path, "sess-v3-coverage", &source)
            .expect("intent should persist");
        let mut evidence = complete_test_restore_outcome_evidence(1);
        evidence.intent = intent.clone();
        evidence.source = source;
        evidence.mapped_panes = 0;
        evidence.layout_complete = false;
        evidence.process_plan_evaluated = false;
        evidence.process_plans_total = 0;
        evidence.process_plans_settled = 0;
        evidence.process_manual = 0;
        evidence.attempt_interrupted = true;
        evidence.interruption_phase = Some(RestoreInterruptionPhase::LayoutRestoration);
        evidence.interruption_reason = Some(RestoreInterruptionReason::BackendFailure);

        let error = persist_restore_receipt_settled(
            &db_path,
            "sess-v3-coverage",
            &HashMap::new(),
            &evidence,
            true,
        )
        .expect_err("missing failure identity must roll back the outcome");
        assert!(matches!(error, RestoreAuthorityDbError::RetrySafe { .. }));

        let receipt_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-v3-coverage'
                   AND checkpoint_role = 'restore_receipt'",
                [],
                |row| row.get(0),
            )
            .expect("receipt count query should succeed");
        assert_eq!(receipt_count, 0);
        let lifecycle: (String, Option<i64>) = conn
            .query_row(
                "SELECT status, outcome_checkpoint_id
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [intent.checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("intent lifecycle should remain present");
        assert_eq!(lifecycle, ("intent".to_string(), None));
    }

    #[test]
    fn outcome_commit_rejects_an_intervening_checkpoint_and_preserves_its_authority() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-intervening-checkpoint", false);
        let source_id = insert_checkpoint(&conn, "sess-intervening-checkpoint", 5_000, 1);
        insert_pane_state(&conn, source_id, 7, Some("/source"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-intervening-checkpoint'",
            [],
        )
        .expect("source must begin as exact latest authority");
        let source_checkpoint = load_checkpoint_by_id(&db_path, source_id)
            .expect("source lookup should succeed")
            .expect("source checkpoint should exist");
        let source = RestoreIntentSource {
            checkpoint_id: source_checkpoint.checkpoint_id,
            checkpoint_at: source_checkpoint.checkpoint_at,
            checkpoint_role: source_checkpoint.checkpoint_role,
            state_hash: source_checkpoint.state_hash,
            pane_count: source_checkpoint.pane_count,
        };
        let intent =
            persist_restore_intent_unclean(&db_path, "sess-intervening-checkpoint", &source)
                .expect("intent should persist");

        let intervening_id =
            insert_checkpoint(&conn, "sess-intervening-checkpoint", 6_000, 1);
        insert_pane_state(&conn, intervening_id, 8, Some("/newer"), Some("zsh"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 6000
             WHERE session_id = 'sess-intervening-checkpoint'",
            [],
        )
        .expect("intervening checkpoint must become session authority");

        let mut evidence = complete_test_restore_outcome_evidence(1);
        evidence.intent = intent.clone();
        evidence.source = source;
        let error = persist_restore_receipt_settled(
            &db_path,
            "sess-intervening-checkpoint",
            &HashMap::from([(7_u64, 70_u64)]),
            &evidence,
            true,
        )
        .expect_err("an intervening checkpoint must fence outcome settlement");
        assert!(matches!(error, RestoreAuthorityDbError::RetrySafe { .. }));

        let session_authority: (Option<i64>, i64, Option<i64>) = conn
            .query_row(
                "SELECT last_checkpoint_at, shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'sess-intervening-checkpoint'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("session authority should remain queryable");
        assert_eq!(session_authority, (Some(6_000), 0, None));
        let latest_checkpoint_id: i64 = conn
            .query_row(
                "SELECT id FROM session_checkpoints
                 WHERE session_id = 'sess-intervening-checkpoint'
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("latest checkpoint should remain queryable");
        assert_eq!(latest_checkpoint_id, intervening_id);
        let lifecycle: (String, Option<i64>) = conn
            .query_row(
                "SELECT status, outcome_checkpoint_id
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [intent.checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("intent lifecycle should remain unresolved");
        assert_eq!(lifecycle, ("intent".to_string(), None));
    }

    #[test]
    fn restore_authority_timestamps_never_precede_their_causal_source() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-future-authority", false);
        let future_checkpoint_at = i64::MAX;
        let source_id = insert_checkpoint(
            &conn,
            "sess-future-authority",
            future_checkpoint_at,
            1,
        );
        insert_pane_state(&conn, source_id, 7, Some("/future"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = ?2 WHERE session_id = ?1",
            params!["sess-future-authority", future_checkpoint_at],
        )
        .expect("future source must become exact latest authority");
        let source_checkpoint = load_checkpoint_by_id(&db_path, source_id)
            .expect("source lookup should succeed")
            .expect("source checkpoint should exist");
        let source = RestoreIntentSource {
            checkpoint_id: source_checkpoint.checkpoint_id,
            checkpoint_at: source_checkpoint.checkpoint_at,
            checkpoint_role: source_checkpoint.checkpoint_role,
            state_hash: source_checkpoint.state_hash,
            pane_count: source_checkpoint.pane_count,
        };
        let intent = persist_restore_intent_unclean(&db_path, "sess-future-authority", &source)
            .expect("intent should preserve causal timestamp ordering");
        assert_eq!(intent.checkpoint_at, future_checkpoint_at);

        let mut evidence = complete_test_restore_outcome_evidence(1);
        evidence.intent = intent;
        evidence.source = source;
        let receipt = persist_restore_receipt_settled(
            &db_path,
            "sess-future-authority",
            &HashMap::from([(7_u64, 70_u64)]),
            &evidence,
            true,
        )
        .expect("receipt should preserve causal timestamp ordering");
        assert_eq!(receipt.checkpoint_at, future_checkpoint_at);
        let session_authority: (Option<i64>, i64, Option<i64>) = conn
            .query_row(
                "SELECT last_checkpoint_at, shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'sess-future-authority'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("resolved session authority should remain queryable");
        assert_eq!(
            session_authority,
            (Some(future_checkpoint_at), 1, Some(receipt.checkpoint_id))
        );
    }

    #[test]
    fn duplicate_target_outcome_persists_a_loadable_reconciliation_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v3-duplicate-receipt", false);
        let source_id = insert_checkpoint(&conn, "sess-v3-duplicate-receipt", 5_000, 2);
        insert_pane_state(&conn, source_id, 1, Some("/left"), Some("bash"));
        insert_pane_state(&conn, source_id, 2, Some("/right"), Some("vim"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-v3-duplicate-receipt'",
            [],
        )
        .expect("source must be the exact latest session authority");

        let source_checkpoint = load_checkpoint_by_id(&db_path, source_id)
            .expect("source lookup should succeed")
            .expect("source checkpoint should exist");
        let source = RestoreIntentSource {
            checkpoint_id: source_checkpoint.checkpoint_id,
            checkpoint_at: source_checkpoint.checkpoint_at,
            checkpoint_role: source_checkpoint.checkpoint_role,
            state_hash: source_checkpoint.state_hash,
            pane_count: source_checkpoint.pane_count,
        };
        let intent =
            persist_restore_intent_unclean(&db_path, "sess-v3-duplicate-receipt", &source)
                .expect("intent should persist before the simulated external outcome");

        let expected_pane_ids = HashSet::from([1_u64, 2]);
        let raw_pane_id_map = HashMap::from([(1_u64, 100_u64), (2, 100)]);
        let mut duplicate_source_ids = duplicate_target_source_pane_ids(&raw_pane_id_map)
            .into_iter()
            .collect::<Vec<_>>();
        duplicate_source_ids.sort_unstable();
        assert_eq!(duplicate_source_ids, vec![1, 2]);
        let persisted_pane_id_map = authoritative_persisted_pane_mapping(
            &expected_pane_ids,
            &duplicate_source_ids,
            &raw_pane_id_map,
        );
        assert!(
            persisted_pane_id_map.is_empty(),
            "neither colliding source mapping is authoritative"
        );

        let mut evidence = complete_test_restore_outcome_evidence(2);
        evidence.intent = intent.clone();
        evidence.source = source;
        evidence.mapped_panes = persisted_pane_id_map.len();
        evidence.failed_source_pane_ids = duplicate_source_ids.clone();
        evidence.duplicate_target_source_pane_ids = duplicate_source_ids.clone();
        evidence.layout_complete = false;
        evidence.process_plan_evaluated = false;
        evidence.process_plans_total = 0;
        evidence.process_plans_settled = 0;
        evidence.process_manual = 0;
        evidence.attempt_interrupted = true;
        evidence.interruption_phase = Some(RestoreInterruptionPhase::LayoutRestoration);
        evidence.interruption_reason = Some(RestoreInterruptionReason::IntegrityFailure);

        let receipt = persist_restore_receipt_settled(
            &db_path,
            "sess-v3-duplicate-receipt",
            &persisted_pane_id_map,
            &evidence,
            false,
        )
        .expect("duplicate-target evidence must settle to a durable partial receipt");
        let loaded = load_checkpoint_by_id(&db_path, receipt.checkpoint_id)
            .expect("partial receipt lookup should succeed")
            .expect("partial receipt row should exist");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::RestoreReceipt);
        assert_eq!(loaded.pane_count, 0);
        assert!(loaded.pane_states.is_empty());

        let metadata = restore_checkpoint_metadata_from_conn(
            &conn,
            receipt.checkpoint_id,
            "sess-v3-duplicate-receipt",
        )
        .expect("partial receipt metadata must validate");
        assert!(metadata.old_to_new.is_empty());
        assert!(matches!(
            metadata.restore_attempt,
            Some(PersistedRestoreAttempt::Outcome {
                evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
                mapped_panes: 0,
                reported_layout_failures: 0,
                failed_source_pane_ids: Some(ref failed_source_pane_ids),
                unexpected_mapping_count: Some(0),
                unexpected_failure_count: Some(0),
                duplicate_target_source_pane_ids: Some(ref duplicate_target_source_pane_ids),
                layout_complete: false,
                attempt_interrupted: true,
                interruption_phase: Some(RestoreInterruptionPhase::LayoutRestoration),
                interruption_reason: Some(RestoreInterruptionReason::IntegrityFailure),
                ..
            }) if failed_source_pane_ids == &[1, 2]
                && duplicate_target_source_pane_ids == &[1, 2]
        ));

        let lifecycle: (String, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT status, outcome_checkpoint_id, resolved_at
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [intent.checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("settled lifecycle should remain queryable");
        assert_eq!(
            lifecycle,
            (
                "reconciliation_required".to_string(),
                Some(receipt.checkpoint_id),
                None,
            )
        );
    }

    #[test]
    fn complete_outcome_binds_process_plan_count_to_expected_panes() {
        let mut metadata = persisted_outcome_with_process_counts(1, 1, 0, 1, 0);
        metadata.old_to_new = BTreeMap::from([
            ("1".to_string(), 101),
            ("2".to_string(), 102),
            ("3".to_string(), 103),
        ]);
        let Some(PersistedRestoreAttempt::Outcome {
            expected_panes,
            mapped_panes,
            layout_complete,
            ..
        }) = metadata.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *expected_panes = 3;
        *mapped_panes = 3;
        *layout_complete = true;

        assert!(matches!(
            validate_restore_outcome_metadata(3, &metadata),
            Err(RestoreError::CorruptCheckpoint(_))
        ));
        assert!(!persisted_restore_outcome_is_complete(&metadata));

        let Some(PersistedRestoreAttempt::Outcome {
            process_plans_total,
            process_plans_settled,
            process_manual,
            ..
        }) = metadata.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *process_plans_total = 3;
        *process_plans_settled = 3;
        *process_manual = 3;
        validate_restore_outcome_metadata(3, &metadata)
            .expect("one settled disposition per expected pane is consistent");
        assert!(persisted_restore_outcome_is_complete(&metadata));
    }

    #[test]
    fn post_layout_interruption_allows_zero_process_inventory_but_not_completion() {
        let mut metadata = persisted_outcome_with_process_counts(0, 0, 0, 0, 0);
        metadata.old_to_new = BTreeMap::from([
            ("1".to_string(), 101),
            ("2".to_string(), 102),
            ("3".to_string(), 103),
        ]);
        let Some(PersistedRestoreAttempt::Outcome {
            expected_panes,
            mapped_panes,
            attempt_interrupted,
            interruption_phase,
            interruption_reason,
            ..
        }) = metadata.restore_attempt.as_mut()
        else {
            unreachable!("test helper always constructs outcome metadata");
        };
        *expected_panes = 3;
        *mapped_panes = 3;
        *attempt_interrupted = true;
        *interruption_phase = Some(RestoreInterruptionPhase::PreProcessDispositionCheckpoint);
        *interruption_reason = Some(RestoreInterruptionReason::Cancelled);

        validate_restore_outcome_metadata(3, &metadata)
            .expect("an interrupted post-layout receipt must remain reloadable");
        assert!(!persisted_restore_outcome_is_complete(&metadata));
    }

    #[test]
    fn outcome_process_disposition_overflow_is_rejected() {
        let overflow = persisted_outcome_with_process_counts(
            usize::MAX,
            usize::MAX,
            usize::MAX,
            1,
            0,
        );
        let error = validate_restore_outcome_metadata(3, &overflow)
            .expect_err("overflowed disposition sum must fail closed");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    // ── Schema-v36 role and witness regressions ──────────────────────

    #[test]
    fn load_checkpoint_by_id_verifies_unmodified_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-ok", false);

        let mut mapping = HashMap::new();
        mapping.insert(11u64, 42u64);
        mapping.insert(22, 43);

        let cp_id =
            finalize_restore_for_test(&db_path, "sess-ok", &mapping, true)
                .expect("finalize restore");

        let loaded = load_checkpoint_by_id(&db_path, cp_id)
            .expect("load should not error")
            .expect("receipt row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::RestoreReceipt);
        assert_eq!(loaded.verification, CheckpointVerification::VerifiedV2);
        assert_eq!(loaded.pane_count, 2);
        assert!(loaded.pane_states.is_empty());
        assert!(loaded.topology_json.is_none());
    }

    #[test]
    fn restore_receipt_cannot_be_submitted_as_layout_snapshot() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-receipt-role", false);
        let checkpoint_id = finalize_restore_for_test(
            &db_path,
            "sess-receipt-role",
            &HashMap::from([(1_u64, 10_u64)]),
            false,
        )
        .unwrap();
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let (session, _) = show_session(&db_path, "sess-receipt-role").unwrap();
        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );

        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            Arc::new(MockWezterm::new()),
        ))
        .expect_err("restore receipts are bookkeeping, not topology snapshots");
        assert!(matches!(
            error,
            RestoreError::CheckpointNotRestorable {
                checkpoint_id: id,
                ..
            } if id == checkpoint_id
        ));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_tampered_v2_state_hash() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-tampered", false);

        let mut mapping = HashMap::new();
        mapping.insert(1u64, 100u64);

        let cp_id =
            finalize_restore_for_test(&db_path, "sess-tampered", &mapping, true)
                .expect("finalize restore");

        conn.execute(
            "UPDATE session_checkpoints
             SET state_hash = 'rst2:0000000000000000000000000000000000000000000000000000000000000000'
             WHERE id = ?1",
            [cp_id],
        )
        .unwrap();

        let err = load_checkpoint_by_id(&db_path, cp_id).expect_err("tampered hash must error");
        match err {
            RestoreError::StateHashMismatch {
                checkpoint_id,
                session_id,
                stored,
                ..
            } => {
                assert_eq!(checkpoint_id, cp_id);
                assert_eq!(session_id, "sess-tampered");
                assert!(stored.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
            }
            other => panic!("expected StateHashMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn corrupt_v2_clean_receipt_is_unclean_on_every_reader_surface() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-corrupt-clean", false);
        let checkpoint_id = finalize_restore_for_test(
            &db_path,
            "sess-corrupt-clean",
            &HashMap::from([(1_u64, 101_u64)]),
            true,
        )
        .expect("bind valid v2 clean receipt");
        conn.execute(
            "UPDATE session_checkpoints
             SET metadata_json = '{\"old_to_new\":{\"1\":999}}'
             WHERE id = ?1",
            [checkpoint_id],
        )
        .expect("tamper receipt projection without recomputing witness");

        let candidates = find_unclean_sessions(&db_path).expect("find unclean sessions");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "sess-corrupt-clean");

        let sessions = list_sessions(&db_path).expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0].shutdown_clean);

        let doctor = session_doctor(&db_path).expect("session doctor");
        assert_eq!(doctor.total_sessions, 1);
        assert_eq!(doctor.unclean_sessions, 1);
        assert_eq!(doctor.invalid_resolved_restore_chains, 1);

        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );
        assert!(matches!(
            restorer.detect(),
            Err(RestoreError::RestoreAttemptRequiresReconciliation {
                status,
                ..
            }) if status == "invalid_resolved_chain"
        ));
    }

    #[test]
    fn missing_resolved_restore_outcome_never_replays_its_source_snapshot() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-missing-outcome", false);
        let outcome_id = finalize_restore_for_test(
            &db_path,
            "sess-missing-outcome",
            &HashMap::from([(1_u64, 101_u64)]),
            true,
        )
        .expect("bind valid resolved restore authority");
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("disable foreign keys for historical-corruption fixture");
        conn.execute(
            "DELETE FROM session_checkpoints WHERE id = ?1",
            [outcome_id],
        )
        .expect("remove resolved outcome without repairing lifecycle");

        let doctor = session_doctor(&db_path).expect("session doctor");
        assert_eq!(doctor.invalid_resolved_restore_chains, 1);

        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );
        assert!(matches!(
            restorer.detect(),
            Err(RestoreError::RestoreAttemptRequiresReconciliation {
                outcome_checkpoint_id: Some(observed_outcome_id),
                status,
                ..
            }) if observed_outcome_id == outcome_id
                && status == "invalid_resolved_chain"
        ));
    }

    #[test]
    fn doctor_validates_every_resolved_chain_not_only_the_newest() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-multiple-resolved", false);
        let mapping = HashMap::from([(1_u64, 101_u64)]);
        let older_outcome = finalize_restore_for_test(
            &db_path,
            "sess-multiple-resolved",
            &mapping,
            true,
        )
        .expect("bind older resolved restore authority");
        let newer_outcome = finalize_restore_for_test(
            &db_path,
            "sess-multiple-resolved",
            &mapping,
            true,
        )
        .expect("bind newer resolved restore authority");
        assert!(older_outcome < newer_outcome);

        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("disable foreign keys for historical-corruption fixture");
        conn.execute(
            "DELETE FROM session_checkpoints WHERE id = ?1",
            [older_outcome],
        )
        .expect("remove only the older resolved outcome");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (?1, 3000, 'startup', 'rsi2:later-unresolved',
                       0, 0, '{}', 'restore_intent')",
            ["sess-multiple-resolved"],
        )
        .expect("insert a causally later unresolved intent");
        let later_unresolved_intent = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 outcome_checkpoint_id, status, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, NULL, 'intent', 3000, NULL)",
            params![
                later_unresolved_intent,
                "sess-multiple-resolved",
                newer_outcome,
            ],
        )
        .expect("bind the later unresolved intent");

        let doctor = session_doctor(&db_path).expect("inspect every resolved chain");
        assert_eq!(doctor.invalid_resolved_restore_chains, 1);
        assert_eq!(doctor.unresolved_restore_attempts, 1);

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        assert!(matches!(
            restorer.detect(),
            Err(RestoreError::RestoreAttemptRequiresReconciliation {
                outcome_checkpoint_id: Some(observed_outcome_id),
                status,
                ..
            }) if observed_outcome_id == older_outcome
                && status == "invalid_resolved_chain"
        ));
        let candidates = find_unclean_sessions(&db_path)
            .expect("invalid historical restore chain must revoke clean authority");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "sess-multiple-resolved");
    }

    #[test]
    fn doctor_counts_resolved_chain_whose_parent_session_is_missing() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-missing-resolved-parent", false);
        finalize_restore_for_test(
            &db_path,
            "sess-missing-resolved-parent",
            &HashMap::from([(1_u64, 101_u64)]),
            true,
        )
        .expect("bind resolved restore authority");

        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("disable foreign keys for historical-corruption fixture");
        conn.execute(
            "DELETE FROM mux_sessions WHERE session_id = 'sess-missing-resolved-parent'",
            [],
        )
        .expect("remove parent session without cascading authority rows");

        let doctor = session_doctor(&db_path).expect("inspect orphaned resolved authority");
        assert_eq!(doctor.total_sessions, 0);
        assert!(doctor.orphaned_checkpoints > 0);
        assert_eq!(doctor.invalid_resolved_restore_chains, 1);
    }

    #[test]
    fn load_checkpoint_by_id_rejects_tampered_pane_id_mapping() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-map-tamper", false);

        let mut mapping = HashMap::new();
        mapping.insert(1u64, 100u64);

        let cp_id = finalize_restore_for_test(&db_path, "sess-map-tamper", &mapping, true)
            .expect("finalize restore");

        // Swap in a different old_to_new without touching state_hash.
        conn.execute(
            "UPDATE session_checkpoints
             SET metadata_json = '{\"old_to_new\":{\"1\":999}}'
             WHERE id = ?1",
            [cp_id],
        )
        .unwrap();

        let err = load_checkpoint_by_id(&db_path, cp_id).expect_err("tampered pane map must error");
        assert!(
            matches!(err, RestoreError::StateHashMismatch { .. }),
            "expected StateHashMismatch, got: {err:?}"
        );
    }

    #[test]
    fn load_checkpoint_by_id_labels_legacy_restore_literal_unverified() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-legacy", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 0, 0, ?3, 'restore_receipt')",
            params!["sess-legacy", 1_700_000_000_000i64, "{\"old_to_new\":{}}"],
        )
        .unwrap();
        let cp_id = conn.last_insert_rowid();

        let loaded = load_checkpoint_by_id(&db_path, cp_id)
            .expect("legacy receipt remains inspectable")
            .expect("legacy receipt row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::RestoreReceipt);
        assert_eq!(
            loaded.verification,
            CheckpointVerification::LegacyUnverified
        );
    }

    #[test]
    fn load_checkpoint_by_id_rejects_missing_metadata_on_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-nometa", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 0, 0, NULL, 'restore_receipt')",
            params!["sess-nometa", 1_700_000_000_000i64],
        )
        .unwrap();
        let cp_id = conn.last_insert_rowid();

        let err = load_checkpoint_by_id(&db_path, cp_id).expect_err("null metadata must error");
        assert!(
            matches!(err, RestoreError::CorruptCheckpoint(_)),
            "expected structural corruption, got: {err:?}"
        );
    }

    #[test]
    fn load_checkpoint_by_id_rejects_duplicate_receipt_target_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-duplicate-target", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 2, 0, ?3, 'restore_receipt')",
            params![
                "sess-duplicate-target",
                1_700_000_000_000i64,
                r#"{"old_to_new":{"1":99,"2":99}}"#,
            ],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("a receipt mapping must be one-to-one");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_aliased_receipt_source_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-aliased-source", false);

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', 2, 0, ?3, 'restore_receipt')",
            params![
                "sess-aliased-source",
                1_700_000_000_000i64,
                r#"{"old_to_new":{"01":98,"1":99}}"#,
            ],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("numeric aliases must not name the same source pane twice");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn startup_snapshot_is_not_confused_with_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-capture", false);
        let cp_id = insert_checkpoint(&conn, "sess-capture", 1000, 0);
        conn.execute(
            "UPDATE session_checkpoints SET checkpoint_type = 'startup' WHERE id = ?1",
            [cp_id],
        )
        .unwrap();

        let loaded = load_checkpoint_by_id(&db_path, cp_id)
            .expect("startup snapshot must load")
            .expect("startup snapshot row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::Snapshot);
        assert_eq!(loaded.checkpoint_type, "startup");
        assert_eq!(
            loaded.verification,
            CheckpointVerification::LegacyUnverified
        );
        assert!(loaded.topology_json.is_some());
    }

    fn run_async_test<F, T>(future: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        use crate::runtime_async::CompatRuntime;

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build session_restore test runtime");
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

    struct SplitFailOnceWezterm {
        inner: MockWezterm,
        split_calls: AtomicUsize,
        reject_activation: bool,
        cancel_after_activation_rejection: bool,
    }

    impl SplitFailOnceWezterm {
        fn new() -> Self {
            Self {
                inner: MockWezterm::new(),
                split_calls: AtomicUsize::new(0),
                reject_activation: false,
                cancel_after_activation_rejection: false,
            }
        }

        fn with_activation_rejection() -> Self {
            Self {
                inner: MockWezterm::new(),
                split_calls: AtomicUsize::new(0),
                reject_activation: true,
                cancel_after_activation_rejection: false,
            }
        }

        fn with_activation_rejection_then_cancel() -> Self {
            Self {
                inner: MockWezterm::new(),
                split_calls: AtomicUsize::new(0),
                reject_activation: true,
                cancel_after_activation_rejection: true,
            }
        }
    }

    impl WeztermInterface for SplitFailOnceWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            self.inner.get_pane(pane_id)
        }

        fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
            self.inner.get_text(pane_id, escapes)
        }

        fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text(pane_id, text)
        }

        fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text_no_paste(pane_id, text)
        }

        fn send_text_with_options(
            &self,
            pane_id: u64,
            text: &str,
            no_paste: bool,
            no_newline: bool,
        ) -> WeztermFuture<'_, ()> {
            self.inner
                .send_text_with_options(pane_id, text, no_paste, no_newline)
        }

        fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_control(pane_id, control_char)
        }

        fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_c(pane_id)
        }

        fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_d(pane_id)
        }

        fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
            self.inner.spawn(cwd, domain_name)
        }

        fn spawn_targeted(
            &self,
            cwd: Option<&str>,
            domain_name: Option<&str>,
            target: crate::wezterm::SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            self.inner.spawn_targeted(cwd, domain_name, target)
        }

        fn split_pane(
            &self,
            pane_id: u64,
            direction: SplitDirection,
            cwd: Option<&str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            if self.split_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(async {
                    Err(test_runtime_error(
                        "session_restore.test.split_pane",
                        "simulated split failure",
                    ))
                });
            }

            self.inner.split_pane(pane_id, direction, cwd, percent)
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            if self.reject_activation {
                let error: crate::Error =
                    crate::error::WeztermError::PaneNotFound(pane_id).into();
                Box::pin(async move { Err(error) })
            } else {
                self.inner.activate_pane(pane_id)
            }
        }

        fn activate_pane_with_cx<'a>(
            &'a self,
            cx: &'a crate::cx::Cx,
            pane_id: u64,
        ) -> WeztermFuture<'a, ()> {
            if self.reject_activation {
                if self.cancel_after_activation_rejection {
                    cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("session restore activation rejection test"),
                    );
                }
                let error: crate::Error =
                    crate::error::WeztermError::PaneNotFound(pane_id).into();
                Box::pin(async move { Err(error) })
            } else {
                self.inner.activate_pane_with_cx(cx, pane_id)
            }
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            direction: MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            self.inner.get_pane_direction(pane_id, direction)
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.kill_pane(pane_id)
        }

        fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
            self.inner.zoom_pane(pane_id, zoom)
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            self.inner.circuit_status()
        }
    }

    struct SpawnFailSecondTabWezterm {
        inner: MockWezterm,
        spawn_calls: AtomicUsize,
    }

    impl SpawnFailSecondTabWezterm {
        fn new() -> Self {
            Self {
                inner: MockWezterm::new(),
                spawn_calls: AtomicUsize::new(0),
            }
        }
    }

    impl WeztermInterface for SpawnFailSecondTabWezterm {
        fn list_panes(&self) -> WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            self.inner.list_panes()
        }

        fn get_pane(&self, pane_id: u64) -> WeztermFuture<'_, crate::wezterm::PaneInfo> {
            self.inner.get_pane(pane_id)
        }

        fn get_text(&self, pane_id: u64, escapes: bool) -> WeztermFuture<'_, String> {
            self.inner.get_text(pane_id, escapes)
        }

        fn send_text(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text(pane_id, text)
        }

        fn send_text_no_paste(&self, pane_id: u64, text: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_text_no_paste(pane_id, text)
        }

        fn send_text_with_options(
            &self,
            pane_id: u64,
            text: &str,
            no_paste: bool,
            no_newline: bool,
        ) -> WeztermFuture<'_, ()> {
            self.inner
                .send_text_with_options(pane_id, text, no_paste, no_newline)
        }

        fn send_control(&self, pane_id: u64, control_char: &str) -> WeztermFuture<'_, ()> {
            self.inner.send_control(pane_id, control_char)
        }

        fn send_ctrl_c(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_c(pane_id)
        }

        fn send_ctrl_d(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.send_ctrl_d(pane_id)
        }

        fn spawn(&self, cwd: Option<&str>, domain_name: Option<&str>) -> WeztermFuture<'_, u64> {
            if self.spawn_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Box::pin(async {
                    Err(test_runtime_error(
                        "session_restore.test.spawn",
                        "simulated second-tab spawn failure",
                    ))
                });
            }

            self.inner.spawn(cwd, domain_name)
        }

        fn spawn_targeted(
            &self,
            cwd: Option<&str>,
            domain_name: Option<&str>,
            target: crate::wezterm::SpawnTarget,
        ) -> WeztermFuture<'_, u64> {
            if self.spawn_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Box::pin(async {
                    Err(test_runtime_error(
                        "session_restore.test.spawn_targeted",
                        "simulated second-tab spawn failure",
                    ))
                });
            }

            self.inner.spawn_targeted(cwd, domain_name, target)
        }

        fn split_pane(
            &self,
            pane_id: u64,
            direction: SplitDirection,
            cwd: Option<&str>,
            percent: Option<u8>,
        ) -> WeztermFuture<'_, u64> {
            self.inner.split_pane(pane_id, direction, cwd, percent)
        }

        fn activate_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.activate_pane(pane_id)
        }

        fn get_pane_direction(
            &self,
            pane_id: u64,
            direction: MoveDirection,
        ) -> WeztermFuture<'_, Option<u64>> {
            self.inner.get_pane_direction(pane_id, direction)
        }

        fn kill_pane(&self, pane_id: u64) -> WeztermFuture<'_, ()> {
            self.inner.kill_pane(pane_id)
        }

        fn zoom_pane(&self, pane_id: u64, zoom: bool) -> WeztermFuture<'_, ()> {
            self.inner.zoom_pane(pane_id, zoom)
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            self.inner.circuit_status()
        }
    }

    fn setup_test_db() -> (String, Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db").to_string_lossy().to_string();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();

        // Create schema tables
        conn.execute_batch(
            "CREATE TABLE mux_sessions (
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
                checkpoint_type TEXT NOT NULL
                    CHECK(checkpoint_type IN ('periodic','shutdown','startup')),
                state_hash TEXT NOT NULL,
                pane_count INTEGER NOT NULL,
                total_bytes INTEGER NOT NULL,
                metadata_json TEXT,
                checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                    CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt')),
                topology_json TEXT,
                restore_intent_checkpoint_id INTEGER
                    REFERENCES session_checkpoints(id) ON DELETE CASCADE,
                CHECK(checkpoint_role = 'restore_receipt'
                      OR restore_intent_checkpoint_id IS NULL)
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
                    (status = 'intent' AND outcome_checkpoint_id IS NULL AND resolved_at IS NULL)
                    OR (status = 'outcome_complete' AND outcome_checkpoint_id IS NOT NULL AND resolved_at IS NULL)
                    OR (status = 'reconciliation_required' AND resolved_at IS NULL)
                    OR (status = 'resolved' AND resolved_at IS NOT NULL)
                )
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

            CREATE TABLE output_segments (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                seq INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_len INTEGER NOT NULL,
                content_hash TEXT,
                captured_at INTEGER NOT NULL,
                UNIQUE(pane_id, seq)
            );

            CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
            CREATE INDEX idx_checkpoints_session_role_latest
                ON session_checkpoints(session_id, checkpoint_role, checkpoint_at DESC, id DESC);
            CREATE INDEX idx_checkpoints_session_role_causal
                ON session_checkpoints(session_id, checkpoint_role, id DESC);
            CREATE UNIQUE INDEX idx_checkpoints_restore_intent_outcome
                ON session_checkpoints(restore_intent_checkpoint_id)
                WHERE restore_intent_checkpoint_id IS NOT NULL;
            CREATE INDEX idx_restore_attempt_lifecycle_session_status
                ON restore_attempt_lifecycle(session_id, status, intent_checkpoint_id);
            CREATE UNIQUE INDEX idx_restore_attempt_lifecycle_outcome
                ON restore_attempt_lifecycle(outcome_checkpoint_id)
                WHERE outcome_checkpoint_id IS NOT NULL;
            CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
            CREATE INDEX idx_output_segments_pane_seq ON output_segments(pane_id, seq);",
        )
        .unwrap();

        (db_path, conn, dir)
    }

    fn insert_session(conn: &Connection, session_id: &str, shutdown_clean: bool) {
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, 1000i64, topology, "0.1.0", shutdown_clean as i64],
        )
        .unwrap();
        if shutdown_clean {
            insert_clean_receipt(conn, session_id, 1000);
        }
    }

    fn insert_clean_receipt(conn: &Connection, session_id: &str, checkpoint_at: i64) {
        let topology_json: String = conn
            .query_row(
                "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash,
              pane_count, total_bytes, metadata_json, checkpoint_role,
              topology_json)
             VALUES (?1, ?2, 'shutdown', 'pending:snp2', 0, 0,
                     NULL, 'snapshot', ?3)",
            params![session_id, checkpoint_at, topology_json],
        )
        .unwrap();
        let receipt_id = conn.last_insert_rowid();
        let state_hash = checkpoint_witness(
            CHECKPOINT_ROLE_SNAPSHOT,
            session_id,
            receipt_id,
            checkpoint_at,
            "shutdown",
            0,
            0,
            None,
            Some(&topology_json),
            &[],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            params![state_hash, receipt_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 last_checkpoint_at = ?2,
                 clean_checkpoint_id = ?3
             WHERE session_id = ?1",
            params![session_id, checkpoint_at, receipt_id],
        )
        .unwrap();
    }

    fn insert_checkpoint(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        pane_count: usize,
    ) -> i64 {
        let topology_json: String = conn
            .query_row(
                "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, checkpoint_role, topology_json)
             VALUES (?1, ?2, 'periodic', '0123456789abcdef', ?3, 0,
                     'snapshot', ?4)",
            params![session_id, checkpoint_at, pane_count as i64, topology_json],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_pane_state(
        conn: &Connection,
        checkpoint_id: i64,
        pane_id: u64,
        cwd: Option<&str>,
        command: Option<&str>,
    ) {
        insert_pane_state_with_scrollback(conn, checkpoint_id, pane_id, cwd, command, None, None);
    }

    fn recompute_checkpoint_total_bytes(conn: &Connection, checkpoint_id: i64) {
        conn.execute(
            "UPDATE session_checkpoints
             SET total_bytes = COALESCE((
                 SELECT SUM(
                     length(CAST(terminal_state_json AS BLOB))
                     + length(CAST(COALESCE(env_json, '') AS BLOB))
                     + length(CAST(COALESCE(agent_metadata_json, '') AS BLOB))
                 )
                 FROM mux_pane_state
                 WHERE mux_pane_state.checkpoint_id = session_checkpoints.id
             ), 0)
             WHERE id = ?1",
            [checkpoint_id],
        )
        .unwrap();
    }

    fn insert_pane_state_with_scrollback(
        conn: &Connection,
        checkpoint_id: i64,
        pane_id: u64,
        cwd: Option<&str>,
        command: Option<&str>,
        scrollback_checkpoint_seq: Option<i64>,
        last_output_at: Option<i64>,
    ) {
        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, cwd, command, terminal_state_json, scrollback_checkpoint_seq, last_output_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                checkpoint_id,
                pane_id as i64,
                cwd,
                command,
                terminal_json,
                scrollback_checkpoint_seq,
                last_output_at
            ],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(conn, checkpoint_id);
    }

    fn seal_checkpoint_v2(conn: &Connection, checkpoint_id: i64) -> String {
        let (
            session_id,
            checkpoint_at,
            checkpoint_type,
            checkpoint_role,
            pane_count,
            total_bytes,
            metadata_json,
            topology_json,
        ): (
            String,
            i64,
            String,
            String,
            i64,
            i64,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT session_id, checkpoint_at, checkpoint_type, checkpoint_role,
                        pane_count, total_bytes, metadata_json, topology_json
                 FROM session_checkpoints WHERE id = ?1",
                [checkpoint_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT pane_id, cwd, command, env_json, terminal_state_json,
                        agent_metadata_json, scrollback_checkpoint_seq, last_output_at
                 FROM mux_pane_state WHERE checkpoint_id = ?1
                 ORDER BY pane_id ASC, id ASC",
            )
            .unwrap();
        let panes = stmt
            .query_map([checkpoint_id], |row| {
                Ok(PersistedPaneState {
                    pane_id: row.get(0)?,
                    cwd: row.get(1)?,
                    command: row.get(2)?,
                    env_json: row.get(3)?,
                    terminal_state_json: row.get(4)?,
                    agent_metadata_json: row.get(5)?,
                    scrollback_checkpoint_seq: row.get(6)?,
                    last_output_at: row.get(7)?,
                })
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let state_hash = checkpoint_witness(
            &checkpoint_role,
            &session_id,
            checkpoint_id,
            checkpoint_at,
            &checkpoint_type,
            pane_count,
            total_bytes,
            metadata_json.as_deref(),
            topology_json.as_deref(),
            &panes,
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            params![state_hash, checkpoint_id],
        )
        .unwrap();
        state_hash
    }

    fn insert_output_segment(
        conn: &Connection,
        pane_id: u64,
        seq: i64,
        content: &str,
        captured_at: i64,
    ) {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                pane_id as i64,
                seq,
                content,
                content.len() as i64,
                captured_at
            ],
        )
        .unwrap();
    }

    fn set_single_pane_topology(conn: &Connection, session_id: &str, pane_id: u64, cwd: &str) {
        let topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    active_pane_id: Some(pane_id),
                    pane_tree: PaneNode::Leaf {
                        pane_id,
                        rows: 24,
                        cols: 80,
                        cwd: Some(cwd.to_string()),
                        title: None,
                        is_active: true,
                    },
                }],
                active_tab_index: Some(0),
            }],
        };

        let topology_json = topology.to_json().expect("serialize single-pane topology");
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params![session_id, topology_json],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_checkpoints
             SET topology_json = ?2
             WHERE session_id = ?1 AND checkpoint_role = 'snapshot'",
            params![session_id, topology_json],
        )
        .unwrap();
    }

    #[test]
    fn detect_no_unclean_sessions() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-abc", true);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn periodic_snapshot_cannot_authorize_clean_shutdown() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-periodic-clean", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-periodic-clean", 5000, 0);
        seal_checkpoint_v2(&conn, checkpoint_id);
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 clean_checkpoint_id = ?2,
                 last_checkpoint_at = 5000
             WHERE session_id = ?1",
            params!["sess-periodic-clean", checkpoint_id],
        )
        .unwrap();

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "sess-periodic-clean");
        let listed = list_sessions(&db_path).unwrap();
        assert!(!listed[0].shutdown_clean);
    }

    #[test]
    fn detect_unclean_session() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-crash", false);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "sess-crash");
    }

    #[test]
    fn find_unclean_sessions_orders_by_causal_checkpoint_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-old", false);
        insert_session(&conn, "sess-new", false);

        let older_causal_id = insert_checkpoint(&conn, "sess-old", 2000, 0);
        let newer_causal_id = insert_checkpoint(&conn, "sess-new", 1000, 0);
        assert!(newer_causal_id > older_causal_id);

        let candidates = find_unclean_sessions(&db_path).unwrap();
        assert_eq!(candidates[0].session_id, "sess-new");
        assert_eq!(candidates[0].last_checkpoint_at, Some(1000));
    }

    #[test]
    fn find_unclean_sessions_rejects_invalid_shutdown_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-bad-clean", 1000i64, topology, "0.1.0", 2i64],
        )
        .unwrap();

        let err = find_unclean_sessions(&db_path).expect_err("invalid shutdown_clean");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.shutdown_clean",
                value: 2
            }
        ));
    }

    #[test]
    fn load_checkpoint_returns_none_for_no_checkpoints() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-no-cp", false);

        let result = load_latest_checkpoint(&db_path, "sess-no-cp").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_checkpoint_with_pane_states() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-ok", false);
        let cp_id = insert_checkpoint(&conn, "sess-ok", 5000, 2);
        insert_pane_state(&conn, cp_id, 0, Some("/home/user"), Some("bash"));
        insert_pane_state(&conn, cp_id, 1, Some("/tmp"), Some("vim"));

        let data = load_latest_checkpoint(&db_path, "sess-ok")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, cp_id);
        assert_eq!(data.pane_states.len(), 2);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/home/user"));
        assert_eq!(data.pane_states[1].command.as_deref(), Some("vim"));
    }

    #[test]
    fn load_checkpoint_with_scrollback_metadata() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-scrollback-meta", false);
        let cp_id = insert_checkpoint(&conn, "sess-scrollback-meta", 5000, 1);
        insert_pane_state_with_scrollback(
            &conn,
            cp_id,
            5,
            Some("/tmp"),
            Some("bash"),
            Some(12),
            Some(5_100),
        );

        let data = load_checkpoint_by_id(&db_path, cp_id).unwrap().unwrap();
        let pane = &data.pane_states[0];
        assert_eq!(pane.scrollback_checkpoint_seq, Some(12));
        assert_eq!(pane.last_output_at, Some(5_100));
    }

    #[test]
    fn load_checkpoint_by_id_verifies_exact_v2_snapshot_projection() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/work"), Some("zsh"));
        conn.execute(
            "UPDATE mux_pane_state
             SET env_json = '{\"redacted_count\":0,\"vars\":{\"LANG\":\"C\"}}'
             WHERE checkpoint_id = ?1",
            [checkpoint_id],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, checkpoint_id);
        let state_hash = seal_checkpoint_v2(&conn, checkpoint_id);
        assert!(state_hash.starts_with(SNAPSHOT_WITNESS_PREFIX));

        let loaded = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect("v2 snapshot load")
            .expect("v2 snapshot row");
        assert_eq!(loaded.checkpoint_role, CheckpointRole::Snapshot);
        assert_eq!(loaded.verification, CheckpointVerification::VerifiedV2);
        assert_eq!(loaded.pane_states.len(), 1);
    }

    #[test]
    fn load_checkpoint_by_id_rejects_v2_snapshot_env_mutation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2-env", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2-env", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/work"), Some("zsh"));
        conn.execute(
            "UPDATE mux_pane_state SET env_json = '{\"vars\":{\"LANG\":\"before\"}}'
             WHERE checkpoint_id = ?1",
            [checkpoint_id],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, checkpoint_id);
        seal_checkpoint_v2(&conn, checkpoint_id);
        conn.execute(
            "UPDATE mux_pane_state SET env_json = '{\"vars\":{\"LANG\":\"change\"}}'
             WHERE checkpoint_id = ?1",
            [checkpoint_id],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("env_json participates in the exact snapshot witness");
        assert!(matches!(error, RestoreError::StateHashMismatch { .. }));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_v2_snapshot_topology_mutation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2-topology", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2-topology", 5000, 0);
        seal_checkpoint_v2(&conn, checkpoint_id);
        conn.execute(
            "UPDATE session_checkpoints
             SET topology_json = '{\"schema_version\":1,\"captured_at\":9999,\"windows\":[]}'
             WHERE id = ?1",
            [checkpoint_id],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("checkpoint-local topology participates in the exact snapshot witness");
        assert!(matches!(error, RestoreError::StateHashMismatch { .. }));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_snapshot_pane_count_mismatch() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-count", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-count", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("declared pane count must match stored rows");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_more_rows_than_the_declared_count() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-count-overflow", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-count-overflow", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);
        insert_pane_state(&conn, checkpoint_id, 8, None, None);

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("the bounded expected-plus-one preflight must observe the extra row");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_declared_pane_count_before_allocation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-count-cap", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-count-cap", 5000, 0);
        conn.execute(
            "UPDATE session_checkpoints SET pane_count = ?2 WHERE id = ?1",
            params![
                checkpoint_id,
                i64::try_from(MAX_TOPOLOGY_PANES + 1).unwrap()
            ],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("an over-limit declaration must fail before pane allocation");
        assert!(matches!(
            error,
            RestoreError::CheckpointResourceLimit {
                checkpoint_id: observed_checkpoint_id,
                resource: "pane_count",
                observed,
                limit: MAX_TOPOLOGY_PANES,
            } if observed_checkpoint_id == checkpoint_id && observed == MAX_TOPOLOGY_PANES + 1
        ));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_oversized_pane_text_before_json_decode() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-row-cap", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-row-cap", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);
        recompute_checkpoint_total_bytes(&conn, checkpoint_id);
        let sealed_hash = seal_checkpoint_v2(&conn, checkpoint_id);
        assert!(sealed_hash.starts_with(SNAPSHOT_WITNESS_PREFIX));

        conn.execute(
            "UPDATE mux_pane_state SET terminal_state_json = ?2 WHERE checkpoint_id = ?1",
            params![
                checkpoint_id,
                "x".repeat(MAX_PERSISTED_PANE_TEXT_BYTES + 1)
            ],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("a sealed pre-cap row must fail as a resource-policy refusal");
        assert!(matches!(
            error,
            RestoreError::CheckpointResourceLimit {
                checkpoint_id: observed_checkpoint_id,
                resource: "pane_text_row_bytes",
                observed,
                limit: MAX_PERSISTED_PANE_TEXT_BYTES,
            } if observed_checkpoint_id == checkpoint_id
                && observed == MAX_PERSISTED_PANE_TEXT_BYTES + 1
        ));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_oversized_metadata_before_parse() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-metadata-cap", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-metadata-cap", 5000, 0);
        conn.execute(
            "UPDATE session_checkpoints SET metadata_json = ?2 WHERE id = ?1",
            params![checkpoint_id, "m".repeat(MAX_CHECKPOINT_METADATA_BYTES + 1)],
        )
        .unwrap();

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("oversized metadata must not cross the SQL-to-Rust boundary");
        assert!(matches!(
            error,
            RestoreError::CheckpointResourceLimit {
                checkpoint_id: observed_checkpoint_id,
                resource: "metadata_json_bytes",
                observed,
                limit: MAX_CHECKPOINT_METADATA_BYTES,
            } if observed_checkpoint_id == checkpoint_id
                && observed == MAX_CHECKPOINT_METADATA_BYTES + 1
        ));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_oversized_identity_text() {
        let (db_path, conn, _dir) = setup_test_db();
        let oversized_session_id = "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1);
        insert_session(&conn, &oversized_session_id, false);
        let checkpoint_id = insert_checkpoint(&conn, &oversized_session_id, 5000, 0);

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("oversized checkpoint identity must fail bounded header admission");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_checkpoint_by_id_rejects_duplicate_snapshot_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-duplicate", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-duplicate", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/duplicate"), None);

        let error = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect_err("duplicate pane IDs make restore ambiguous");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
    }

    #[test]
    fn load_latest_checkpoint_picks_newest() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-multi", false);
        let old_cp = insert_checkpoint(&conn, "sess-multi", 1000, 1);
        let new_cp = insert_checkpoint(&conn, "sess-multi", 2000, 1);
        insert_pane_state(&conn, old_cp, 9, Some("/old"), Some("bash"));
        insert_pane_state(&conn, new_cp, 10, Some("/new"), None);

        let data = load_latest_checkpoint(&db_path, "sess-multi")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, new_cp);
        assert_eq!(data.checkpoint_at, 2000);
    }

    #[test]
    fn load_latest_checkpoint_breaks_timestamp_ties_by_row_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-tied", false);
        let older_id = insert_checkpoint(&conn, "sess-tied", 2000, 0);
        let newer_id = insert_checkpoint(&conn, "sess-tied", 2000, 0);
        assert!(newer_id > older_id);

        let loaded = load_latest_checkpoint(&db_path, "sess-tied")
            .unwrap()
            .unwrap();
        assert_eq!(loaded.checkpoint_id, newer_id);
    }

    #[test]
    fn load_latest_checkpoint_does_not_fall_back_past_missing_topology() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-topology-gap", false);
        let older_id = insert_checkpoint(&conn, "sess-topology-gap", 1000, 0);
        let newer_id = insert_checkpoint(&conn, "sess-topology-gap", 2000, 0);
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = NULL WHERE id = ?1",
            [newer_id],
        )
        .unwrap();

        let error = load_latest_checkpoint(&db_path, "sess-topology-gap")
            .expect_err("an incomplete latest snapshot must fail closed");
        assert!(matches!(
            error,
            RestoreError::CheckpointTopologyUnavailable { checkpoint_id }
                if checkpoint_id == newer_id
        ));
        assert_ne!(older_id, newer_id);
    }

    #[test]
    fn checkpoint_load_uses_row_local_topology_not_mutable_session_topology() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-topology", false);
        let first_id = insert_checkpoint(&conn, "sess-topology", 1000, 0);
        let first_topology = load_checkpoint_by_id(&db_path, first_id)
            .unwrap()
            .unwrap()
            .topology_json
            .unwrap();

        let second_topology =
            r#"{"schema_version":1,"captured_at":2000,"workspace_id":"second","windows":[]}"#;
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params!["sess-topology", second_topology],
        )
        .unwrap();
        let second_id = insert_checkpoint(&conn, "sess-topology", 2000, 0);
        conn.execute(
            "UPDATE mux_sessions SET topology_json =
             '{\"schema_version\":1,\"captured_at\":3000,\"workspace_id\":\"latest-session-only\",\"windows\":[]}'
             WHERE session_id = 'sess-topology'",
            [],
        )
        .unwrap();

        let first = load_checkpoint_by_id(&db_path, first_id)
            .unwrap()
            .unwrap();
        let second = load_checkpoint_by_id(&db_path, second_id)
            .unwrap()
            .unwrap();
        assert_eq!(first.topology_json.as_deref(), Some(first_topology.as_str()));
        assert_eq!(second.topology_json.as_deref(), Some(second_topology));
        assert_eq!(
            load_latest_checkpoint(&db_path, "sess-topology")
                .unwrap()
                .unwrap()
                .checkpoint_id,
            second_id
        );
    }

    #[test]
    fn load_latest_checkpoint_ignores_newer_restore_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-shadowed", false);
        let capture_cp = insert_checkpoint(&conn, "sess-shadowed", 1000, 1);
        insert_pane_state(&conn, capture_cp, 42, Some("/real"), Some("bash"));

        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4, 'restore_receipt')",
            params![
                "sess-shadowed",
                2000i64,
                1i64,
                r#"{"old_to_new":{"42":99}}"#,
            ],
        )
        .unwrap();
        let receipt_id = conn.last_insert_rowid();

        let data = load_latest_checkpoint(&db_path, "sess-shadowed")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, capture_cp);
        assert_eq!(data.checkpoint_at, 1000);
        assert_eq!(data.pane_states.len(), 1);
        assert_eq!(data.pane_states[0].pane_id, 42);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/real"));

        let sessions = list_sessions(&db_path).unwrap();
        assert_eq!(sessions[0].checkpoint_count, 2);
        assert_eq!(sessions[0].pane_count, Some(1));
        let (_, checkpoints) = show_session(&db_path, "sess-shadowed").unwrap();
        assert_eq!(checkpoints[0].id, receipt_id);
        assert_eq!(
            checkpoints[0].checkpoint_role,
            CheckpointRole::RestoreReceipt
        );
        assert_eq!(checkpoints[1].id, capture_cp);
        assert_eq!(checkpoints[1].checkpoint_role, CheckpointRole::Snapshot);
    }

    #[test]
    fn load_latest_checkpoint_falls_back_to_newest_empty_checkpoint_when_needed() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-empty-only", false);
        let old_cp = insert_checkpoint(&conn, "sess-empty-only", 1000, 0);
        let new_cp = insert_checkpoint(&conn, "sess-empty-only", 2000, 0);

        let data = load_latest_checkpoint(&db_path, "sess-empty-only")
            .unwrap()
            .unwrap();
        assert_eq!(data.checkpoint_id, new_cp);
        assert_eq!(data.checkpoint_at, 2000);
        assert!(data.pane_states.is_empty());

        // The older empty checkpoint should not be preferred over the newest
        // one when neither is actually restorable.
        assert_ne!(data.checkpoint_id, old_cp);
    }

    #[test]
    fn load_checkpoint_by_id_returns_none_for_missing_checkpoint() {
        let (db_path, _conn, _dir) = setup_test_db();
        let result = load_checkpoint_by_id(&db_path, 999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn load_checkpoint_by_id_can_load_non_latest_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-multi-id", false);
        let old_cp = insert_checkpoint(&conn, "sess-multi-id", 1000, 1);
        let _new_cp = insert_checkpoint(&conn, "sess-multi-id", 2000, 2);
        insert_pane_state(&conn, old_cp, 42, Some("/old"), Some("bash"));

        let data = load_checkpoint_by_id(&db_path, old_cp).unwrap().unwrap();
        assert_eq!(data.checkpoint_id, old_cp);
        assert_eq!(data.session_id, "sess-multi-id");
        assert_eq!(data.checkpoint_at, 1000);
        assert_eq!(data.pane_states.len(), 1);
        assert_eq!(data.pane_states[0].pane_id, 42);
        assert_eq!(data.pane_states[0].cwd.as_deref(), Some("/old"));
    }

    #[test]
    fn mark_session_restored_sets_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-restore", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-restore", 1_000, 0);

        mark_session_restored(&db_path, "sess-restore").unwrap();

        let (clean, clean_checkpoint_id): (bool, Option<i64>) = conn
            .query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'sess-restore'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(clean);
        assert_eq!(clean_checkpoint_id, Some(checkpoint_id));
    }

    #[test]
    fn save_restore_checkpoint_records_mapping() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-map", false);

        let mut mapping = HashMap::new();
        mapping.insert(0u64, 5u64);
        mapping.insert(1, 6);

        let cp_id = save_restore_checkpoint(&db_path, "sess-map", &mapping).unwrap();
        assert!(cp_id > 0);

        let (cp_type, cp_role, state_hash, metadata_json, topology_json): (
            String,
            String,
            String,
            String,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT checkpoint_type, checkpoint_role, state_hash, metadata_json, topology_json
                 FROM session_checkpoints WHERE id = ?1",
                [cp_id],
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
        assert_eq!(cp_type, "startup");
        assert_eq!(cp_role, CHECKPOINT_ROLE_RESTORE_RECEIPT);
        assert!(state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
        assert_eq!(metadata_json, r#"{"old_to_new":{"0":5,"1":6}}"#);
        assert!(topology_json.is_none());
    }

    #[test]
    fn save_restore_checkpoint_rejects_duplicate_target_pane_ids() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-duplicate-map", false);
        let mapping = HashMap::from([(1_u64, 99_u64), (2_u64, 99_u64)]);

        let error = save_restore_checkpoint(&db_path, "sess-duplicate-map", &mapping)
            .expect_err("receipt writer must reject a non-injective pane mapping");
        assert!(matches!(error, RestoreError::Bookkeeping(_)));
        let checkpoint_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-duplicate-map'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
    }

    #[test]
    fn restore_mapping_rejects_over_limit_cardinality_before_serialization() {
        let mapping = (0..=MAX_TOPOLOGY_PANES)
            .map(|pane_id| {
                let pane_id = u64::try_from(pane_id).expect("test pane id is representable");
                (pane_id, pane_id)
            })
            .collect::<HashMap<_, _>>();

        let error = prepare_restore_mapping(&mapping)
            .expect_err("an over-limit mapping must fail before JSON construction");
        assert!(matches!(error, RestoreError::Bookkeeping(_)));
    }

    #[test]
    fn save_restore_checkpoint_updates_session_last_checkpoint_at() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-last-cp", false);

        let checkpoint_id = save_restore_checkpoint(&db_path, "sess-last-cp", &HashMap::new())
            .expect("restore checkpoint");

        let (checkpoint_at, last_checkpoint_at): (i64, Option<i64>) = conn
            .query_row(
                "SELECT c.checkpoint_at, s.last_checkpoint_at
                 FROM session_checkpoints c
                 JOIN mux_sessions s ON s.session_id = c.session_id
                 WHERE c.id = ?1",
                [checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(last_checkpoint_at, Some(checkpoint_at));
    }

    #[test]
    fn restore_receipt_two_phase_flow_binds_exact_clean_authority() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-finalize", false);

        let mut mapping = HashMap::new();
        mapping.insert(7u64, 70u64);

        let checkpoint_id =
            finalize_restore_for_test(&db_path, "sess-finalize", &mapping, true)
                .expect("finalize restore");

        let (checkpoint_type, checkpoint_role, state_hash, metadata_json, last_checkpoint_at, shutdown_clean): (
            String,
            String,
            String,
            String,
            Option<i64>,
            bool,
        ) = conn
            .query_row(
                "SELECT c.checkpoint_type, c.checkpoint_role, c.state_hash, c.metadata_json,
                        s.last_checkpoint_at, s.shutdown_clean
                 FROM session_checkpoints c
                 JOIN mux_sessions s ON s.session_id = c.session_id
                 WHERE c.id = ?1",
                [checkpoint_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(checkpoint_type, "startup");
        assert_eq!(checkpoint_role, CHECKPOINT_ROLE_RESTORE_RECEIPT);
        assert!(state_hash.starts_with(RESTORE_RECEIPT_WITNESS_PREFIX));
        assert!(metadata_json.contains("\"old_to_new\":{\"7\":70}"));
        assert!(last_checkpoint_at.is_some());
        assert!(shutdown_clean);
    }

    #[test]
    fn restore_receipt_rejects_missing_session_and_rolls_back_receipt() {
        let (db_path, conn, _dir) = setup_test_db();

        let error = finalize_restore_for_test(&db_path, "sess-missing", &HashMap::new(), true)
            .expect_err("bookkeeping must not create a receipt without its session");
        assert!(matches!(error, RestoreError::Bookkeeping(_)));
        let checkpoint_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-missing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);
    }

    #[test]
    fn restore_clean_mark_rejects_receipt_displaced_by_newer_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-finalize-rollback", false);

        let mut mapping = HashMap::new();
        mapping.insert(8u64, 80u64);

        let receipt_checkpoint_id = finalize_restore_for_test(
            &db_path,
            "sess-finalize-rollback",
            &mapping,
            false,
        )
        .expect("persist complete unclean restore outcome");
        let persisted_receipt = load_checkpoint_by_id(&db_path, receipt_checkpoint_id)
            .expect("load outcome receipt")
            .expect("outcome receipt row");
        let receipt = RestoreReceipt {
            checkpoint_id: receipt_checkpoint_id,
            checkpoint_at: i64::try_from(persisted_receipt.checkpoint_at)
                .expect("test receipt timestamp fits SQLite INTEGER"),
            state_hash: persisted_receipt.state_hash,
        };
        let newer_checkpoint = insert_checkpoint(
            &conn,
            "sess-finalize-rollback",
            receipt.checkpoint_at.saturating_add(1),
            1,
        );
        let err = mark_restore_receipt_clean(
            &db_path,
            "sess-finalize-rollback",
            &receipt,
        )
        .expect_err("a displaced restore receipt must not authorize clean state");
        assert!(matches!(
            err,
            RestoreAuthorityDbError::RetrySafe {
                source: RestoreError::Bookkeeping(_)
            }
        ));

        let startup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-finalize-rollback' AND checkpoint_type = 'startup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            startup_count, 2,
            "the intent and outcome remain as an auditable unclean restore attempt"
        );

        let (last_checkpoint_at, shutdown_clean): (Option<i64>, bool) = conn
            .query_row(
                "SELECT last_checkpoint_at, shutdown_clean
                 FROM mux_sessions
                 WHERE session_id = 'sess-finalize-rollback'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_checkpoint_at, Some(receipt.checkpoint_at));
        assert!(!shutdown_clean);
        assert!(newer_checkpoint > receipt.checkpoint_id);
    }

    #[test]
    fn session_restorer_detect_empty_db() {
        let (db_path, _conn, _dir) = setup_test_db();
        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_restorer_detect_clean_sessions_only() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean-1", true);
        insert_session(&conn, "sess-clean-2", true);

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_restorer_reports_unclean_session_without_snapshot_as_blocked() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-unclean-no-snapshot", false);
        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());

        assert!(matches!(
            restorer.detect(),
            Err(RestoreError::UncleanSessionsNotRestorable {
                session_count: 1,
                reason: UncleanSessionBlockReason::NoSnapshotCheckpoint,
            })
        ));
    }

    #[test]
    fn session_restorer_reports_empty_snapshot_as_blocked_and_changed_after_admission() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-unclean-empty-snapshot", false);
        let checkpoint_id =
            insert_checkpoint(&conn, "sess-unclean-empty-snapshot", 5_000, 0);
        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );

        assert!(matches!(
            restorer.detect(),
            Err(RestoreError::UncleanSessionsNotRestorable {
                session_count: 1,
                reason: UncleanSessionBlockReason::EmptyOrMissingCheckpoint,
            })
        ));
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .expect("load empty checkpoint fixture")
            .expect("empty checkpoint row");
        assert!(matches!(
            ensure_detected_checkpoint_remains_nonempty(&checkpoint),
            Err(RestoreError::DetectedCheckpointChanged {
                checkpoint_id: observed,
            }) if observed == checkpoint_id
        ));
    }

    #[test]
    fn session_restorer_detect_finds_crash() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean", true);
        insert_session(&conn, "sess-crash", false);
        let cp_id = insert_checkpoint(&conn, "sess-crash", 5000, 1);
        insert_pane_state(&conn, cp_id, 7, Some("/restore"), Some("bash"));

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().session_id, "sess-crash");
    }

    #[test]
    fn session_restorer_detection_pins_the_exact_snapshot_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-pinned", false);
        let selected_id = insert_checkpoint(&conn, "sess-pinned", 5000, 1);
        insert_pane_state(&conn, selected_id, 7, Some("/selected"), Some("bash"));

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        let candidate = restorer.detect().unwrap().expect("bounded candidate");
        assert_eq!(candidate.selected_checkpoint_id, Some(selected_id));

        let newer_id = insert_checkpoint(&conn, "sess-pinned", 6000, 1);
        insert_pane_state(&conn, newer_id, 8, Some("/newer"), Some("zsh"));
        let loaded = restorer.load_checkpoint(&candidate).unwrap();
        assert_eq!(loaded.checkpoint_id, selected_id);
        assert_eq!(loaded.pane_states[0].pane_id, 7);
    }

    #[test]
    fn session_restorer_detection_does_not_decode_full_pane_json() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-descriptor-only", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-descriptor-only", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, None, None);
        conn.execute(
            "UPDATE mux_pane_state SET terminal_state_json = 'not-json'
             WHERE checkpoint_id = ?1",
            [checkpoint_id],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, checkpoint_id);

        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );
        let candidate = restorer
            .detect()
            .expect("descriptor detection must not decode pane JSON")
            .expect("non-empty bounded descriptor");
        assert_eq!(candidate.selected_checkpoint_id, Some(checkpoint_id));
        assert!(matches!(
            restorer.load_checkpoint(&candidate),
            Err(RestoreError::CorruptCheckpoint(_))
        ));
    }

    #[test]
    fn auto_restore_never_selects_legacy_unverified_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-legacy-auto", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-legacy-auto", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));

        let manual = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        assert!(manual.detect().unwrap().is_some());

        let automatic = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig {
                auto_restore: true,
                ..SessionRestoreConfig::default()
            },
        );
        assert!(matches!(
            automatic.detect(),
            Err(RestoreError::UncleanSessionsNotRestorable {
                session_count: 1,
                reason: UncleanSessionBlockReason::LegacyUnverifiedAutoRestore,
            })
        ));
    }

    #[test]
    fn auto_restore_selects_verified_v2_snapshot() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-v2-auto", false);
        let checkpoint_id = insert_checkpoint(&conn, "sess-v2-auto", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));
        seal_checkpoint_v2(&conn, checkpoint_id);

        let automatic = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig {
                auto_restore: true,
                ..SessionRestoreConfig::default()
            },
        );
        let candidate = automatic
            .detect()
            .expect("verified auto-detect")
            .expect("verified v2 snapshot should be eligible for auto-restore");
        assert_eq!(candidate.session_id, "sess-v2-auto");
    }

    #[test]
    fn auto_restore_rejects_direct_legacy_checkpoint_submission() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-direct-legacy", false);
        set_single_pane_topology(&conn, "sess-direct-legacy", 7, "/restore");
        let checkpoint_id = insert_checkpoint(&conn, "sess-direct-legacy", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));

        let automatic = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig {
                auto_restore: true,
                ..SessionRestoreConfig::default()
            },
        );
        let mut sessions = find_unclean_sessions(&db_path).unwrap();
        let session = sessions.remove(0);
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let error = run_async_test(automatic.restore(
            &session,
            &checkpoint,
            Arc::new(MockWezterm::new()),
        ))
        .expect_err("direct submission must not bypass the auto-restore legacy gate");
        assert!(matches!(
            error,
            RestoreError::LegacyCheckpointRequiresManualRestore {
                checkpoint_id: id
            } if id == checkpoint_id
        ));
    }

    #[test]
    fn restore_rejects_topology_and_pane_state_id_mismatch_before_mux_calls() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-id-mismatch", false);
        set_single_pane_topology(&conn, "sess-id-mismatch", 8, "/topology");
        let checkpoint_id = insert_checkpoint(&conn, "sess-id-mismatch", 5000, 1);
        insert_pane_state(
            &conn,
            checkpoint_id,
            7,
            Some("/pane-state"),
            Some("bash"),
        );

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let mut sessions = find_unclean_sessions(&db_path).unwrap();
        let session = sessions.remove(0);
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let wezterm = Arc::new(MockWezterm::new());
        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            wezterm.clone(),
        ))
        .expect_err("topology and pane-state IDs must agree before restore starts");
        assert!(matches!(error, RestoreError::CorruptCheckpoint(_)));
        assert!(run_async_test(wezterm.list_panes()).unwrap().is_empty());
    }

    #[test]
    fn session_restorer_detect_skips_unclean_sessions_without_checkpoints() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-empty", false);
        insert_session(&conn, "sess-restorable", false);

        conn.execute(
            "UPDATE mux_sessions SET created_at = 3000 WHERE session_id = 'sess-empty'",
            [],
        )
        .unwrap();

        let cp_id = insert_checkpoint(&conn, "sess-restorable", 2000, 1);
        insert_pane_state(&conn, cp_id, 9, Some("/good"), Some("zsh"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 2000 WHERE session_id = 'sess-restorable'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert_eq!(
            result
                .as_ref()
                .map(|candidate| candidate.session_id.as_str()),
            Some("sess-restorable")
        );
    }

    #[test]
    fn session_restorer_detect_prefers_most_recent_usable_checkpoint() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-shadowed", false);
        insert_session(&conn, "sess-usable", false);

        let shadowed_capture = insert_checkpoint(&conn, "sess-shadowed", 1000, 1);
        insert_pane_state(&conn, shadowed_capture, 1, Some("/older"), Some("bash"));
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role)
             VALUES (?1, ?2, 'startup', 'restore', ?3, 0, ?4, 'restore_receipt')",
            params![
                "sess-shadowed",
                4000i64,
                1i64,
                r#"{"old_to_new":{"1":11}}"#,
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 4000 WHERE session_id = 'sess-shadowed'",
            [],
        )
        .unwrap();

        let usable_capture = insert_checkpoint(&conn, "sess-usable", 2500, 1);
        insert_pane_state(&conn, usable_capture, 2, Some("/newer"), Some("fish"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 2500 WHERE session_id = 'sess-usable'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let result = restorer.detect().unwrap();
        assert_eq!(
            result
                .as_ref()
                .map(|candidate| candidate.session_id.as_str()),
            Some("sess-usable")
        );
    }

    #[test]
    fn restore_admission_accepts_layout_supported_extreme_split_ratios() {
        let leaf = |pane_id, is_active| PaneNode::Leaf {
            pane_id,
            rows: 24,
            cols: 80,
            cwd: None,
            title: None,
            is_active,
        };
        let topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1_000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 1,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 1,
                    title: None,
                    active_pane_id: Some(1),
                    pane_tree: PaneNode::VSplit {
                        children: vec![
                            (1.0, leaf(1, true)),
                            (2.0, leaf(2, false)),
                            (f64::MAX, leaf(3, false)),
                        ],
                    },
                }],
                active_tab_index: Some(0),
            }],
        };

        validate_restore_topology(41, 3, &[1, 2, 3], &topology)
            .expect("session admission must accept every topology supported by layout preflight");
    }

    #[test]
    fn session_restorer_restore_partial_failure_requires_explicit_reconciliation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-partial", false);

        let split_topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![TabSnapshot {
                    tab_id: 0,
                    title: None,
                    active_pane_id: Some(1),
                    pane_tree: PaneNode::HSplit {
                        children: vec![
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 1,
                                    rows: 24,
                                    cols: 80,
                                    cwd: Some("/left".to_string()),
                                    title: None,
                                    is_active: true,
                                },
                            ),
                            (
                                0.5,
                                PaneNode::Leaf {
                                    pane_id: 2,
                                    rows: 24,
                                    cols: 80,
                                    cwd: Some("/right".to_string()),
                                    title: None,
                                    is_active: false,
                                },
                            ),
                        ],
                    },
                }],
                active_tab_index: Some(0),
            }],
        };
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params![
                "sess-partial",
                split_topology.to_json().expect("serialize split topology"),
            ],
        )
        .unwrap();

        let checkpoint_id = insert_checkpoint(&conn, "sess-partial", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 1, Some("/left"), Some("bash"));
        insert_pane_state(&conn, checkpoint_id, 2, Some("/right"), Some("vim"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-partial'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(SplitFailOnceWezterm::new());
        let error = run_async_test(restorer.restore(&session, &checkpoint, wezterm))
            .expect_err("an uncertain split must return its durable interrupted receipt");
        let (intent_checkpoint_id, outcome_checkpoint_id) = match error {
            RestoreError::RestoreAttemptInterrupted {
                intent_checkpoint_id,
                outcome_checkpoint_id: Some(outcome_checkpoint_id),
                reason: RestoreInterruptionReason::BackendFailure,
                ..
            } => (intent_checkpoint_id, outcome_checkpoint_id),
            other => panic!("expected receipted partial restore, got {other:?}"),
        };

        let outcome = load_checkpoint_by_id(&db_path, outcome_checkpoint_id)
            .expect("partial receipt must load")
            .expect("partial receipt row must exist");
        assert_eq!(outcome.checkpoint_role, CheckpointRole::RestoreReceipt);
        let metadata = restore_checkpoint_metadata_from_conn(
            &conn,
            outcome_checkpoint_id,
            "sess-partial",
        )
        .expect("partial receipt metadata must validate");
        assert_eq!(metadata.old_to_new.len(), 1);
        assert!(matches!(
            metadata.restore_attempt,
            Some(PersistedRestoreAttempt::Outcome {
                evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
                failed_source_pane_ids: Some(ref failed_source_pane_ids),
                unexpected_mapping_count: Some(0),
                unexpected_failure_count: Some(0),
                duplicate_target_source_pane_ids: Some(ref duplicate_target_source_pane_ids),
                layout_complete: false,
                attempt_interrupted: true,
                interruption_phase: Some(RestoreInterruptionPhase::LayoutRestoration),
                interruption_reason: Some(RestoreInterruptionReason::BackendFailure),
                ..
            }) if failed_source_pane_ids == &[2]
                && duplicate_target_source_pane_ids.is_empty()
        ));

        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-partial'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !shutdown_clean,
            "partial restore must remain unclean until its durable attempt is reconciled"
        );

        let error = restorer
            .detect()
            .expect_err("an incomplete durable restore must not be retried implicitly");
        assert!(matches!(
            error,
            RestoreError::RestoreAttemptRequiresReconciliation {
                ref session_id,
                intent_checkpoint_id: observed_intent_checkpoint_id,
                outcome_checkpoint_id: Some(observed_outcome_checkpoint_id),
                ref status,
            } if session_id == "sess-partial"
                && observed_intent_checkpoint_id == intent_checkpoint_id
                && observed_outcome_checkpoint_id == outcome_checkpoint_id
                && status == "reconciliation_required"
        ));
    }

    #[test]
    fn mapped_activation_failure_persists_failure_only_reconciliation_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-mapped-activation-failure", false);
        set_single_pane_topology(&conn, "sess-mapped-activation-failure", 7, "/restore");
        let checkpoint_id = insert_checkpoint(&conn, "sess-mapped-activation-failure", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-mapped-activation-failure'",
            [],
        )
        .expect("source must be the exact latest session authority");

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();
        let wezterm = Arc::new(SplitFailOnceWezterm::with_activation_rejection_then_cancel());

        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            wezterm.clone(),
        ))
        .expect_err("mapped activation rejection plus cancellation must return its receipt");
        let (intent_checkpoint_id, outcome_checkpoint_id) = match error {
            RestoreError::RestoreAttemptInterrupted {
                intent_checkpoint_id,
                outcome_checkpoint_id: Some(outcome_checkpoint_id),
                phase: "post-layout checkpoint",
                reason: RestoreInterruptionReason::Cancelled,
                ..
            } => (intent_checkpoint_id, outcome_checkpoint_id),
            other => panic!("expected receipted mapped activation failure, got {other:?}"),
        };
        assert_eq!(
            run_async_test(wezterm.list_panes())
                .expect("mock pane listing should succeed")
                .len(),
            1,
            "the root-pane mapping existed before activation was rejected"
        );

        let outcome = load_checkpoint_by_id(&db_path, outcome_checkpoint_id)
            .expect("partial receipt must load")
            .expect("partial receipt row must exist");
        assert_eq!(outcome.checkpoint_role, CheckpointRole::RestoreReceipt);
        assert_eq!(outcome.pane_count, 0);
        assert!(outcome.pane_states.is_empty());
        let metadata = restore_checkpoint_metadata_from_conn(
            &conn,
            outcome_checkpoint_id,
            "sess-mapped-activation-failure",
        )
        .expect("mapped activation failure metadata must validate");
        assert!(
            metadata.old_to_new.is_empty(),
            "a source reported failed after mapping must not remain an authoritative success"
        );
        assert!(matches!(
            metadata.restore_attempt,
            Some(PersistedRestoreAttempt::Outcome {
                evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
                mapped_panes: 0,
                reported_layout_failures: 1,
                failed_source_pane_ids: Some(ref failed_source_pane_ids),
                unexpected_mapping_count: Some(0),
                unexpected_failure_count: Some(0),
                duplicate_target_source_pane_ids: Some(ref duplicate_target_source_pane_ids),
                layout_complete: false,
                attempt_interrupted: true,
                interruption_phase: Some(RestoreInterruptionPhase::PostLayoutCheckpoint),
                interruption_reason: Some(RestoreInterruptionReason::Cancelled),
                ..
            }) if failed_source_pane_ids == &[7]
                && duplicate_target_source_pane_ids.is_empty()
        ));

        let lifecycle: (String, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT status, outcome_checkpoint_id, resolved_at
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [intent_checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("settled lifecycle should remain queryable");
        assert_eq!(
            lifecycle,
            (
                "reconciliation_required".to_string(),
                Some(outcome_checkpoint_id),
                None,
            )
        );
    }

    #[test]
    fn authoritative_partial_restore_is_a_typed_error_with_durable_receipt() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-authoritative-partial", false);
        set_single_pane_topology(&conn, "sess-authoritative-partial", 7, "/restore");
        let checkpoint_id =
            insert_checkpoint(&conn, "sess-authoritative-partial", 5_000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-authoritative-partial'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();
        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            Arc::new(SplitFailOnceWezterm::with_activation_rejection()),
        ))
        .expect_err("partial authority must never be represented as Result success");

        let summary = error
            .partial_restore_summary()
            .expect("typed partial error must retain its durable receipt");
        assert!(!summary.restore_authority_resolved);
        assert_eq!(summary.checkpoint_id, checkpoint_id);
        assert!(summary.intent_checkpoint_id > checkpoint_id);
        assert!(summary.outcome_checkpoint_id > summary.intent_checkpoint_id);
        assert_eq!(summary.layout_failed_pane_count(), 1);
        let lifecycle: (String, Option<i64>) = conn
            .query_row(
                "SELECT status, outcome_checkpoint_id
                 FROM restore_attempt_lifecycle
                 WHERE intent_checkpoint_id = ?1",
                [summary.intent_checkpoint_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("partial lifecycle receipt");
        assert_eq!(
            lifecycle,
            (
                "reconciliation_required".to_string(),
                Some(summary.outcome_checkpoint_id),
            )
        );
    }

    #[test]
    fn session_restorer_root_tab_failure_requires_explicit_reconciliation() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-tab-fail", false);

        let multi_tab_topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 1000,
            workspace_id: None,
            windows: vec![WindowSnapshot {
                window_id: 0,
                title: None,
                position: None,
                size: None,
                tabs: vec![
                    TabSnapshot {
                        tab_id: 0,
                        title: None,
                        active_pane_id: Some(1),
                        pane_tree: PaneNode::Leaf {
                            pane_id: 1,
                            rows: 24,
                            cols: 80,
                            cwd: Some("/ok".to_string()),
                            title: None,
                            is_active: true,
                        },
                    },
                    TabSnapshot {
                        tab_id: 1,
                        title: None,
                        active_pane_id: Some(2),
                        pane_tree: PaneNode::Leaf {
                            pane_id: 2,
                            rows: 24,
                            cols: 80,
                            cwd: Some("/fails".to_string()),
                            title: None,
                            is_active: false,
                        },
                    },
                ],
                active_tab_index: Some(0),
            }],
        };
        conn.execute(
            "UPDATE mux_sessions SET topology_json = ?2 WHERE session_id = ?1",
            params![
                "sess-tab-fail",
                multi_tab_topology
                    .to_json()
                    .expect("serialize multi-tab topology"),
            ],
        )
        .unwrap();

        let checkpoint_id = insert_checkpoint(&conn, "sess-tab-fail", 5000, 2);
        insert_pane_state(&conn, checkpoint_id, 1, Some("/ok"), Some("bash"));
        insert_pane_state(&conn, checkpoint_id, 2, Some("/fails"), Some("python"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-tab-fail'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(SpawnFailSecondTabWezterm::new());
        let error = run_async_test(restorer.restore(&session, &checkpoint, wezterm))
            .expect_err("an uncertain second-tab spawn must return a durable partial receipt");
        let (intent_checkpoint_id, outcome_checkpoint_id) = match error {
            RestoreError::RestoreAttemptInterrupted {
                intent_checkpoint_id,
                outcome_checkpoint_id: Some(outcome_checkpoint_id),
                reason: RestoreInterruptionReason::BackendFailure,
                ..
            } => (intent_checkpoint_id, outcome_checkpoint_id),
            other => panic!("expected receipted partial restore, got {other:?}"),
        };
        let metadata = restore_checkpoint_metadata_from_conn(
            &conn,
            outcome_checkpoint_id,
            "sess-tab-fail",
        )
        .expect("partial tab receipt metadata must validate");
        assert_eq!(metadata.old_to_new.len(), 1);
        assert!(matches!(
            metadata.restore_attempt,
            Some(PersistedRestoreAttempt::Outcome {
                evidence_version: RESTORE_OUTCOME_EVIDENCE_VERSION,
                failed_source_pane_ids: Some(ref failed_source_pane_ids),
                unexpected_mapping_count: Some(0),
                unexpected_failure_count: Some(0),
                duplicate_target_source_pane_ids: Some(ref duplicate_target_source_pane_ids),
                layout_complete: false,
                attempt_interrupted: true,
                interruption_phase: Some(RestoreInterruptionPhase::LayoutRestoration),
                interruption_reason: Some(RestoreInterruptionReason::BackendFailure),
                ..
            }) if failed_source_pane_ids == &[2]
                && duplicate_target_source_pane_ids.is_empty()
        ));

        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-tab-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !shutdown_clean,
            "failed tab restore must remain unclean until its durable attempt is reconciled"
        );

        let error = restorer
            .detect()
            .expect_err("an incomplete durable restore must not be retried implicitly");
        assert!(matches!(
            error,
            RestoreError::RestoreAttemptRequiresReconciliation {
                ref session_id,
                intent_checkpoint_id: observed_intent_checkpoint_id,
                outcome_checkpoint_id: Some(observed_outcome_checkpoint_id),
                ref status,
            } if session_id == "sess-tab-fail"
                && observed_intent_checkpoint_id == intent_checkpoint_id
                && observed_outcome_checkpoint_id == outcome_checkpoint_id
                && status == "reconciliation_required"
        ));
    }

    #[test]
    fn session_restorer_successful_restore_marks_clean_and_keeps_capture_checkpoint_usable() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-success", false);
        set_single_pane_topology(&conn, "sess-success", 7, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-success", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 7, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-success'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let mut session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();
        session.selected_checkpoint_id = Some(i64::MAX);

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        assert_eq!(summary.layout_settled_pane_count(), 1);
        assert_eq!(summary.layout_failed_pane_count(), 0);
        assert_eq!(summary.checkpoint_id, checkpoint_id);
        assert_eq!(summary.layout_result.pane_id_map.len(), 1);

        let verify_conn = Connection::open(&db_path).expect("open verification db");
        let shutdown_clean: bool = verify_conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-success'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            shutdown_clean,
            "successful restore should mark the source session clean"
        );

        let startup_row: (i64, String) = verify_conn
            .query_row(
                "SELECT id, metadata_json
                 FROM session_checkpoints
                 WHERE session_id = ?1 AND checkpoint_role = 'restore_receipt'
                 ORDER BY id DESC
                 LIMIT 1",
                ["sess-success"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("startup checkpoint recorded after restore");
        assert_ne!(
            startup_row.0, checkpoint_id,
            "restore should create a distinct startup checkpoint"
        );

        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&7)
            .expect("old pane mapped to restored pane");
        let metadata: serde_json::Value =
            serde_json::from_str(&startup_row.1).expect("parse startup checkpoint metadata");
        assert_eq!(metadata["old_to_new"]["7"].as_u64(), Some(new_pane_id));

        let latest = load_latest_checkpoint(&db_path, "sess-success")
            .unwrap()
            .unwrap();
        assert_eq!(
            latest.checkpoint_id, checkpoint_id,
            "the original capture checkpoint should remain the preferred manual-restore snapshot"
        );
        assert_eq!(latest.pane_states.len(), 1);
        assert_eq!(latest.pane_states[0].pane_id, 7);

        let pane_text = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();
        assert_eq!(
            pane_text, "",
            "layout-only restore must not type a banner or command into the PTY"
        );

        assert!(
            restorer.detect().unwrap().is_none(),
            "clean session should not be re-detected after a successful restore"
        );
    }

    #[test]
    fn session_restorer_agent_is_manual_without_any_launch_configuration() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-agent-launch", false);
        set_single_pane_topology(&conn, "sess-agent-launch", 7, "/agents");

        let checkpoint_id = insert_checkpoint(&conn, "sess-agent-launch", 5000, 1);
        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"agent"}"#;
        let agent_json = r#"{"agent_type":"codex","session_id":"sess-42","state":"running"}"#;
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, cwd, command, terminal_state_json, agent_metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                checkpoint_id,
                7i64,
                "/agents",
                "codex",
                terminal_json,
                agent_json
            ],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, checkpoint_id);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-agent-launch'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        assert_eq!(summary.layout_settled_pane_count(), 1);
        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&7)
            .expect("old pane mapped to restored pane");
        let content = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();
        assert_eq!(content, "");
        let report = summary
            .process_launch_report
            .as_ref()
            .expect("agent disposition must be reported");
        assert_eq!(report.manual_count(), 1);
        assert_eq!(report.plans_total(), 1);
        assert_eq!(report.plans_settled(), 1);
    }

    #[test]
    fn session_restorer_manual_process_disposition_completes_layout_restore() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-agent-manual", false);
        set_single_pane_topology(&conn, "sess-agent-manual", 7, "/agents");
        let checkpoint_id = insert_checkpoint(&conn, "sess-agent-manual", 5000, 1);
        insert_pane_state(
            &conn,
            checkpoint_id,
            7,
            Some("/agents"),
            Some("codex"),
        );
        conn.execute(
            "UPDATE mux_pane_state
             SET agent_metadata_json = ?2
             WHERE checkpoint_id = ?1 AND pane_id = 7",
            rusqlite::params![
                checkpoint_id,
                r#"{"agent_type":"codex","session_id":"sess-42","state":"running"}"#,
            ],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, checkpoint_id);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-agent-manual'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();
        let summary = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            Arc::new(MockWezterm::new()),
        ))
        .expect("layout restore with manual follow-up");
        assert_eq!(summary.layout_failed_pane_count(), 0);
        assert!(summary.restore_authority_resolved);
        let launch_report = summary
            .process_launch_report
            .as_ref()
            .expect("manual process disposition must remain visible");
        assert_eq!(launch_report.manual_count(), 1);

        let (shutdown_clean, receipt_count): (bool, i64) = conn
            .query_row(
                "SELECT s.shutdown_clean,
                        (SELECT COUNT(*) FROM session_checkpoints c
                         WHERE c.session_id = s.session_id
                           AND c.checkpoint_role = 'restore_receipt')
                 FROM mux_sessions s WHERE s.session_id = 'sess-agent-manual'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(shutdown_clean);
        assert_eq!(receipt_count, 1, "manual disposition remains auditable");
        assert!(restorer.detect().unwrap().is_none());
    }

    #[test]
    fn session_restorer_rejects_unaudited_authority_trigger_before_mux_effects() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-bookkeeping-fail", false);
        set_single_pane_topology(&conn, "sess-bookkeeping-fail", 9, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-bookkeeping-fail", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 9, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-bookkeeping-fail'",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER abort_bookkeeping_clean
             BEFORE UPDATE OF shutdown_clean ON mux_sessions
             WHEN NEW.shutdown_clean = 1
             BEGIN
                 SELECT RAISE(ABORT, 'simulated restore bookkeeping failure');
             END;",
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let err = run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone()))
            .expect_err("restore intent admission must reject an unaudited authority trigger");
        assert!(matches!(err, RestoreError::Bookkeeping(_)));
        assert!(
            run_async_test(wezterm.list_panes()).unwrap().is_empty(),
            "trigger rejection must happen before the first mux effect"
        );

        let startup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-bookkeeping-fail' AND checkpoint_type = 'startup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            startup_count, 0,
            "guard rejection must roll back before intent or outcome persistence"
        );

        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = 'sess-bookkeeping-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !shutdown_clean,
            "rejected intent admission must leave the source session unclean"
        );
    }

    #[test]
    fn orphaned_restore_intent_blocks_retry_before_mux_effects() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-orphaned-intent", false);
        set_single_pane_topology(&conn, "sess-orphaned-intent", 17, "/restore");
        let checkpoint_id = insert_checkpoint(&conn, "sess-orphaned-intent", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 17, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-orphaned-intent'",
            [],
        )
        .unwrap();
        let checkpoint = load_checkpoint_by_id(&db_path, checkpoint_id)
            .unwrap()
            .unwrap();
        let source = RestoreIntentSource {
            checkpoint_id,
            checkpoint_at: checkpoint.checkpoint_at,
            checkpoint_role: checkpoint.checkpoint_role,
            state_hash: checkpoint.state_hash,
            pane_count: checkpoint.pane_count,
        };
        let intent = persist_restore_intent_unclean(
            &db_path,
            "sess-orphaned-intent",
            &source,
        )
        .expect("persist exact restore intent");
        conn.execute(
            "DELETE FROM restore_attempt_lifecycle WHERE intent_checkpoint_id = ?1",
            [intent.checkpoint_id],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );
        let wezterm = Arc::new(MockWezterm::new());
        let error = run_async_test(restorer.detect_and_restore(wezterm.clone()))
            .expect_err("an orphaned intent requires explicit reconciliation");
        assert!(matches!(
            error,
            RestoreError::RestoreAttemptRequiresReconciliation {
                ref session_id,
                intent_checkpoint_id,
                outcome_checkpoint_id: None,
                ref status,
            } if session_id == "sess-orphaned-intent"
                && intent_checkpoint_id == intent.checkpoint_id
                && status == "orphaned_intent"
        ));
        assert!(
            run_async_test(wezterm.list_panes()).unwrap().is_empty(),
            "orphaned-intent admission must not mutate the mux"
        );
    }

    #[test]
    fn clean_session_race_rejects_stale_restore_before_mux_effects() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean-race", false);
        set_single_pane_topology(&conn, "sess-clean-race", 23, "/restore");
        let checkpoint_id = insert_checkpoint(&conn, "sess-clean-race", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 23, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE session_checkpoints SET checkpoint_type = 'shutdown' WHERE id = ?1",
            [checkpoint_id],
        )
        .unwrap();
        seal_checkpoint_v2(&conn, checkpoint_id);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-clean-race'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig::default(),
        );
        let session = restorer.detect().unwrap().expect("initially unclean session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1, clean_checkpoint_id = ?2, last_checkpoint_at = 5000
             WHERE session_id = ?1",
            params!["sess-clean-race", checkpoint_id],
        )
        .unwrap();
        let wezterm = Arc::new(MockWezterm::new());
        let error = run_async_test(restorer.restore(
            &session,
            &checkpoint,
            wezterm.clone(),
        ))
        .expect_err("a clean-session race must invalidate stale restore admission");
        assert!(matches!(error, RestoreError::Bookkeeping(_)));
        assert!(run_async_test(wezterm.list_panes()).unwrap().is_empty());
        assert!(find_unclean_sessions(&db_path).unwrap().is_empty());
    }

    #[test]
    fn restore_reloads_checkpoint_authority_instead_of_using_mutated_caller_data() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-reload-authority", false);
        set_single_pane_topology(&conn, "sess-reload-authority", 31, "/restore");
        let checkpoint_id = insert_checkpoint(&conn, "sess-reload-authority", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 31, Some("/restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000
             WHERE session_id = 'sess-reload-authority'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path),
            SessionRestoreConfig::default(),
        );
        let session = restorer.detect().unwrap().expect("restorable session");
        let mut caller_checkpoint = restorer.load_checkpoint(&session).unwrap();
        caller_checkpoint.topology_json = Some("{caller-mutated".to_string());
        caller_checkpoint.pane_count = usize::MAX;
        caller_checkpoint.state_hash = "caller-mutated".to_string();

        let summary = run_async_test(restorer.restore(
            &session,
            &caller_checkpoint,
            Arc::new(MockWezterm::new()),
        ))
        .expect("restore must rebind to the exact persisted checkpoint row");
        assert_eq!(summary.checkpoint_id, checkpoint_id);
        assert_eq!(summary.layout_settled_pane_count(), 1);
        assert!(summary.restore_authority_resolved);
    }

    #[test]
    fn session_restorer_rejects_scrollback_before_intent_or_mux_effects() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-scrollback", false);
        set_single_pane_topology(&conn, "sess-scrollback", 1, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-scrollback", 5000, 1);
        insert_pane_state_with_scrollback(
            &conn,
            checkpoint_id,
            1,
            Some("/restore"),
            Some("bash"),
            Some(1),
            Some(5_200),
        );
        insert_output_segment(&conn, 1, 0, "first line", 5_100);
        insert_output_segment(&conn, 1, 1, "second line", 5_200);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-scrollback'",
            [],
        )
        .unwrap();

        let restorer = SessionRestorer::new(
            Arc::new(db_path.clone()),
            SessionRestoreConfig {
                restore_scrollback: true,
                ..SessionRestoreConfig::default()
            },
        );
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let error = run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone()))
            .expect_err("PTY-input scrollback replay must fail closed");
        assert!(matches!(
            error,
            RestoreError::SafeScrollbackReplayUnavailable
        ));
        assert!(
            run_async_test(wezterm.list_panes()).unwrap().is_empty(),
            "safety preflight must run before any pane is spawned"
        );
        let restore_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id = 'sess-scrollback'
                   AND checkpoint_role IN ('restore_intent', 'restore_receipt')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            restore_rows, 0,
            "safety preflight must run before a durable restore intent"
        );
    }

    #[test]
    fn session_restorer_rejects_layout_invalid_topology_before_intent_or_mux_effects() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-invalid-layout-preflight", false);
        let topology = TopologySnapshot {
            schema_version: 1,
            captured_at: 5_000,
            workspace_id: None,
            windows: vec![
                WindowSnapshot {
                    window_id: 9,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 10,
                        title: None,
                        active_pane_id: Some(1),
                        pane_tree: PaneNode::Leaf {
                            pane_id: 1,
                            rows: 24,
                            cols: 80,
                            cwd: Some("/first".to_string()),
                            title: None,
                            is_active: true,
                        },
                    }],
                    active_tab_index: Some(0),
                },
                WindowSnapshot {
                    window_id: 9,
                    title: None,
                    position: None,
                    size: None,
                    tabs: vec![TabSnapshot {
                        tab_id: 11,
                        title: None,
                        active_pane_id: Some(2),
                        pane_tree: PaneNode::Leaf {
                            pane_id: 2,
                            rows: 24,
                            cols: 80,
                            cwd: Some("/second".to_string()),
                            title: None,
                            is_active: true,
                        },
                    }],
                    active_tab_index: Some(0),
                },
            ],
        };
        let topology_json =
            serde_json::to_string(&topology).expect("serialize intentionally invalid topology");
        conn.execute(
            "UPDATE mux_sessions
             SET topology_json = ?2, last_checkpoint_at = 5000
             WHERE session_id = ?1",
            params!["sess-invalid-layout-preflight", topology_json],
        )
        .unwrap();
        let checkpoint_id =
            insert_checkpoint(&conn, "sess-invalid-layout-preflight", 5_000, 2);
        insert_pane_state(&conn, checkpoint_id, 1, Some("/first"), Some("bash"));
        insert_pane_state(&conn, checkpoint_id, 2, Some("/second"), Some("bash"));
        seal_checkpoint_v2(&conn, checkpoint_id);

        let restorer =
            SessionRestorer::new(Arc::new(db_path), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session descriptor");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();
        let wezterm = Arc::new(MockWezterm::new());
        let error = run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone()))
            .expect_err("duplicate window authority must fail before durable intent");
        assert!(matches!(
            error,
            RestoreError::CorruptCheckpoint(message)
                if message == format!(
                    "checkpoint {checkpoint_id} topology failed layout preflight"
                )
        ));
        assert!(run_async_test(wezterm.list_panes()).unwrap().is_empty());
        let restore_authority_rows: i64 = conn
            .query_row(
                "SELECT
                     (SELECT COUNT(*) FROM session_checkpoints
                      WHERE session_id = 'sess-invalid-layout-preflight'
                        AND checkpoint_role IN ('restore_intent', 'restore_receipt'))
                   + (SELECT COUNT(*) FROM restore_attempt_lifecycle
                      WHERE session_id = 'sess-invalid-layout-preflight')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restore_authority_rows, 0);
    }

    #[test]
    fn session_restorer_restore_layout_only_skips_scrollback_replay() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-layout-only", false);
        set_single_pane_topology(&conn, "sess-layout-only", 7, "/restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-layout-only", 5000, 1);
        insert_pane_state_with_scrollback(
            &conn,
            checkpoint_id,
            7,
            Some("/restore"),
            Some("bash"),
            Some(1),
            Some(5_200),
        );
        insert_output_segment(&conn, 7, 0, "first line", 5_100);
        insert_output_segment(&conn, 7, 1, "second line", 5_200);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-layout-only'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let summary =
            run_async_test(restorer.restore(&session, &checkpoint, wezterm.clone())).unwrap();

        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&7)
            .expect("pane mapping for layout-only restore");
        let content = run_async_test(wezterm.get_text(new_pane_id, false)).unwrap();

        assert_eq!(
            content, "",
            "layout-only restore must not type banners or captured output into the PTY"
        );
    }

    #[test]
    fn pane_state_parses_terminal_state_json() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-ts", false);
        let cp_id = insert_checkpoint(&conn, "sess-ts", 5000, 1);
        insert_pane_state(&conn, cp_id, 0, Some("/home"), None);

        let data = load_latest_checkpoint(&db_path, "sess-ts")
            .unwrap()
            .unwrap();
        let ts = data.pane_states[0].terminal_state.as_ref().unwrap();
        assert_eq!(ts.rows, 24);
        assert_eq!(ts.cols, 80);
        assert!(!ts.is_alt_screen);
    }

    #[test]
    fn load_latest_checkpoint_reports_corrupt_terminal_state_json() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-bad-ts", false);
        let cp_id = insert_checkpoint(&conn, "sess-bad-ts", 5000, 1);

        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            params![cp_id, 0i64, "{not-json"],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, cp_id);

        let err = load_latest_checkpoint(&db_path, "sess-bad-ts").expect_err("corrupt checkpoint");
        let is_corrupt = matches!(err, RestoreError::CorruptCheckpoint(_));
        assert!(is_corrupt, "expected corrupt checkpoint error, got {err:?}");
    }

    #[test]
    fn load_latest_checkpoint_rejects_negative_pane_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-neg-pane", false);
        let cp_id = insert_checkpoint(&conn, "sess-neg-pane", 5000, 1);

        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            params![
                cp_id,
                -1i64,
                r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"neg"}"#
            ],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, cp_id);

        let err = load_latest_checkpoint(&db_path, "sess-neg-pane").expect_err("negative pane id");
        assert!(
            matches!(
                err,
                RestoreError::InvalidPersistedValue {
                    field: "mux_pane_state.pane_id",
                    value: -1
                }
            ),
            "expected InvalidPersistedValue for negative pane_id, got {err:?}"
        );
    }

    #[test]
    fn restore_summary_counts() {
        let mut layout_result = RestoreResult {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 1,
            tabs_created: 2,
            panes_created: 3,
        };
        layout_result.pane_id_map.insert(0, 5);
        layout_result.pane_id_map.insert(1, 6);
        layout_result
            .failed_panes
            .push((2, "split failed".to_string()));

        let summary = RestoreSummary {
            session_id: "sess-test".to_string(),
            checkpoint_id: 1,
            intent_checkpoint_id: 2,
            outcome_checkpoint_id: 3,
            layout_result,
            pane_states: vec![
                summary_pane_state(0),
                summary_pane_state(1),
                summary_pane_state(2),
            ],
            process_launch_report: None,
            restore_authority_resolved: false,
            elapsed_ms: 100,
        };

        assert_eq!(summary.layout_settled_pane_count(), 2);
        assert_eq!(summary.layout_failed_pane_count(), 1);
        assert_eq!(summary.expected_pane_count(), 3);
    }

    #[test]
    fn restore_summary_treats_every_duplicate_target_source_as_failed() {
        let summary = RestoreSummary {
            session_id: "sess-duplicate-target".to_string(),
            checkpoint_id: 1,
            intent_checkpoint_id: 2,
            outcome_checkpoint_id: 3,
            layout_result: RestoreResult {
                pane_id_map: HashMap::from([(7, 70), (8, 70)]),
                failed_panes: Vec::new(),
                windows_created: 1,
                tabs_created: 1,
                panes_created: 2,
            },
            pane_states: vec![summary_pane_state(7), summary_pane_state(8)],
            process_launch_report: None,
            restore_authority_resolved: false,
            elapsed_ms: 1,
        };

        assert_eq!(summary.layout_failed_pane_count(), 2);
        assert_eq!(summary.layout_settled_pane_count(), 0);
        let formatted = format_restore_summary(&summary);
        assert!(formatted.contains("0/2 panes"));
        assert!(formatted.contains(
            "pane 7: layout mapping collided on a duplicate target pane"
        ));
        assert!(formatted.contains(
            "pane 8: layout mapping collided on a duplicate target pane"
        ));
    }

    #[test]
    fn restore_summary_debug_is_bounded_and_content_free() {
        let summary = RestoreSummary {
            session_id: "raw-session-canary".to_string(),
            checkpoint_id: 1,
            intent_checkpoint_id: 2,
            outcome_checkpoint_id: 3,
            layout_result: RestoreResult {
                pane_id_map: HashMap::from([(7, 70)]),
                failed_panes: vec![(8, "raw-layout-error-canary".to_string())],
                windows_created: 1,
                tabs_created: 1,
                panes_created: 1,
            },
            pane_states: vec![RestoredPaneState {
                pane_id: 7,
                cwd: Some("raw-cwd-canary".to_string()),
                command: Some("raw-command-canary".to_string()),
                terminal_state: None,
                agent_metadata: None,
                scrollback_checkpoint_seq: None,
                last_output_at: None,
            }],
            process_launch_report: None,
            restore_authority_resolved: false,
            elapsed_ms: 1,
        };
        let debug = format!("{summary:?}");
        for canary in ["raw-session", "raw-layout", "raw-cwd", "raw-command"] {
            assert!(!debug.contains(canary));
        }
        assert!(debug.len() < 512);
    }

    #[test]
    fn restore_summary_counts_mapped_activation_failure_once_without_backend_text() {
        let summary = RestoreSummary {
            session_id: "sess-activation-failure".to_string(),
            checkpoint_id: 1,
            intent_checkpoint_id: 2,
            outcome_checkpoint_id: 3,
            layout_result: RestoreResult {
                pane_id_map: HashMap::from([(7, 70)]),
                failed_panes: vec![
                    (7, "sensitive backend detail".to_string()),
                    (7, "duplicate sensitive detail".to_string()),
                    (999, "unexpected sensitive detail".to_string()),
                    (999, "duplicate unexpected detail".to_string()),
                ],
                windows_created: 1,
                tabs_created: 1,
                panes_created: 1,
            },
            pane_states: vec![summary_pane_state(7)],
            process_launch_report: None,
            restore_authority_resolved: false,
            elapsed_ms: 1,
        };

        assert_eq!(summary.layout_failed_pane_count(), 1);
        assert_eq!(summary.layout_settled_pane_count(), 0);
        let formatted = format_restore_summary(&summary);
        assert!(formatted.contains("pane 7: layout restoration reported failure"));
        assert!(formatted.contains("1 unexpected failures"));
        assert!(!formatted.contains("sensitive backend detail"));
    }

    #[test]
    fn restore_summary_format() {
        let mut layout_result = RestoreResult {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 1,
            tabs_created: 1,
            panes_created: 2,
        };
        layout_result.pane_id_map.insert(0, 5);
        layout_result.pane_id_map.insert(1, 6);

        let summary = RestoreSummary {
            session_id: "sess-fmt".to_string(),
            checkpoint_id: 1,
            intent_checkpoint_id: 2,
            outcome_checkpoint_id: 3,
            layout_result,
            pane_states: vec![summary_pane_state(0), summary_pane_state(1)],
            process_launch_report: None,
            restore_authority_resolved: true,
            elapsed_ms: 42,
        };

        let text = format_restore_summary(&summary);
        assert!(text.contains("sess-fmt"));
        assert!(text.contains("layout/authority settled"));
        assert!(text.contains("2/2"));
        assert!(text.contains("42ms"));
    }

    #[test]
    fn restore_summary_format_partial_restore() {
        let mut layout_result = RestoreResult {
            pane_id_map: HashMap::new(),
            failed_panes: Vec::new(),
            windows_created: 1,
            tabs_created: 1,
            panes_created: 2,
        };
        layout_result.pane_id_map.insert(0, 5);
        layout_result
            .failed_panes
            .push((1, "split failed".to_string()));

        let summary = RestoreSummary {
            session_id: "sess-partial-fmt".to_string(),
            checkpoint_id: 1,
            intent_checkpoint_id: 2,
            outcome_checkpoint_id: 3,
            layout_result,
            pane_states: vec![summary_pane_state(0), summary_pane_state(1)],
            process_launch_report: None,
            restore_authority_resolved: false,
            elapsed_ms: 42,
        };

        let text = format_restore_summary(&summary);
        assert!(text.contains("sess-partial-fmt"));
        assert!(text.contains("layout/authority partial"));
        assert!(text.contains("1/2"));
        assert!(text.contains("Failed panes:"));
    }

    #[test]
    fn pane_state_with_agent_metadata() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-agent", false);
        let cp_id = insert_checkpoint(&conn, "sess-agent", 5000, 1);

        let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
        let agent_json = r#"{"agent_type":"claude_code","session_id":"abc","state":"idle"}"#;

        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, cwd, terminal_state_json, agent_metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![cp_id, 0i64, "/home", terminal_json, agent_json],
        )
        .unwrap();
        recompute_checkpoint_total_bytes(&conn, cp_id);

        let data = load_latest_checkpoint(&db_path, "sess-agent")
            .unwrap()
            .unwrap();
        let agent = data.pane_states[0].agent_metadata.as_ref().unwrap();
        assert_eq!(agent.agent_type, "claude_code");
        assert_eq!(agent.state.as_deref(), Some("idle"));
    }

    // =========================================================================
    // CLI query function tests
    // =========================================================================

    #[test]
    fn list_sessions_empty() {
        let (db_path, _conn, _dir) = setup_test_db();
        let sessions = list_sessions(&db_path).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_sessions_with_data() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-a", true);
        insert_session(&conn, "sess-b", false);
        let cp_id = insert_checkpoint(&conn, "sess-b", 2000, 3);
        insert_pane_state(&conn, cp_id, 0, Some("/tmp"), None);

        let sessions = list_sessions(&db_path).unwrap();
        assert_eq!(sessions.len(), 2);

        // sess-b has a checkpoint so it sorts first (higher last_checkpoint_at)
        let b = sessions.iter().find(|s| s.session_id == "sess-b").unwrap();
        assert!(!b.shutdown_clean);
        assert_eq!(b.checkpoint_count, 1);
        assert_eq!(b.pane_count, Some(3));

        let a = sessions.iter().find(|s| s.session_id == "sess-a").unwrap();
        assert!(a.shutdown_clean);
        assert_eq!(a.checkpoint_count, 1);
        assert_eq!(a.pane_count, Some(0));
    }

    #[test]
    fn list_sessions_page_bounds_rows_and_reports_exact_total() {
        let (db_path, conn, _dir) = setup_test_db();
        for index in 0_i64..5 {
            let session_id = format!("sess-page-{index}");
            insert_session(&conn, &session_id, false);
            insert_checkpoint(&conn, &session_id, 1_000 + index, 1);
        }

        let first = list_sessions_page(&db_path, 2, 0).expect("first session page");
        assert_eq!(first.sessions.len(), 2);
        assert_eq!(first.total_sessions, 5);
        assert_eq!(first.limit, 2);
        assert_eq!(first.offset, 0);
        assert!(first.has_more);
        assert_eq!(first.sessions[0].session_id, "sess-page-4");
        assert_eq!(first.sessions[1].session_id, "sess-page-3");

        let last = list_sessions_page(&db_path, 2, 4).expect("last session page");
        assert_eq!(last.sessions.len(), 1);
        assert_eq!(last.sessions[0].session_id, "sess-page-0");
        assert_eq!(last.total_sessions, 5);
        assert!(!last.has_more);

        let exact = list_sessions_page(&db_path, 5, 0).expect("exact session page");
        assert_eq!(exact.sessions.len(), 5);
        assert_eq!(exact.total_sessions, 5);
        assert!(!exact.has_more);

        let empty = list_sessions_page(&db_path, 2, 5).expect("empty session page");
        assert!(empty.sessions.is_empty());
        assert_eq!(empty.total_sessions, 5);
        assert_eq!(empty.offset, 5);
        assert!(!empty.has_more);
    }

    #[test]
    fn session_query_pages_reject_unbounded_windows_before_io() {
        let dir = tempfile::tempdir().expect("create query-window test directory");
        let missing_db = dir.path().join("must-not-be-opened.db");
        let missing_db = missing_db.to_string_lossy().to_string();

        for (limit, offset) in [
            (0, 0),
            (MAX_SESSION_QUERY_LIMIT + 1, 0),
            (1, MAX_SESSION_QUERY_OFFSET + 1),
        ] {
            assert!(matches!(
                list_sessions_page(&missing_db, limit, offset),
                Err(RestoreError::InvalidQueryWindow { .. })
            ));
            assert!(matches!(
                show_session_page(&missing_db, "sess-window", limit, offset),
                Err(RestoreError::InvalidQueryWindow { .. })
            ));
        }
        assert!(
            !std::path::Path::new(&missing_db).exists(),
            "query-window admission must precede opening storage"
        );
    }

    #[test]
    fn nonpaged_session_list_fails_closed_above_materialization_limit() {
        let (db_path, conn, _dir) = setup_test_db();
        for index in 0..=MAX_SESSION_QUERY_LIMIT {
            insert_session(&conn, &format!("sess-bounded-{index:03}"), false);
        }

        let error = list_sessions(&db_path).expect_err("nonpaged list must fail closed");
        assert!(matches!(
            error,
            RestoreError::QueryResultLimit {
                resource: "session_summaries",
                observed,
                limit: MAX_SESSION_QUERY_LIMIT,
            } if observed == MAX_SESSION_QUERY_LIMIT + 1
        ));
    }

    #[test]
    fn list_and_show_session_break_checkpoint_timestamp_ties_by_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-summary-tie", false);
        let older_id = insert_checkpoint(&conn, "sess-summary-tie", 2000, 1);
        let newer_id = insert_checkpoint(&conn, "sess-summary-tie", 2000, 2);

        let sessions = list_sessions(&db_path).unwrap();
        assert_eq!(sessions[0].pane_count, Some(2));
        let (_, checkpoints) = show_session(&db_path, "sess-summary-tie").unwrap();
        assert_eq!(checkpoints[0].id, newer_id);
        assert_eq!(checkpoints[1].id, older_id);
    }

    #[test]
    fn list_and_show_session_use_checkpoint_time_before_higher_backdated_id() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-chronology-newest", false);
        insert_session(&conn, "sess-chronology-middle", false);

        let chronological_latest =
            insert_checkpoint(&conn, "sess-chronology-newest", 3_000, 3);
        let backdated_higher_id =
            insert_checkpoint(&conn, "sess-chronology-newest", 1_000, 1);
        insert_checkpoint(&conn, "sess-chronology-middle", 2_000, 2);

        let sessions = list_sessions(&db_path).expect("list sessions chronologically");
        assert_eq!(sessions[0].session_id, "sess-chronology-newest");
        assert_eq!(sessions[0].last_checkpoint_at, Some(3_000));
        assert_eq!(sessions[0].pane_count, Some(3));
        assert_eq!(sessions[1].session_id, "sess-chronology-middle");

        let (session, checkpoints) =
            show_session(&db_path, "sess-chronology-newest").expect("show session");
        assert_eq!(session.last_checkpoint_at, Some(3_000));
        assert_eq!(session.selected_checkpoint_id, Some(chronological_latest));
        assert_eq!(checkpoints[0].id, chronological_latest);
        assert_eq!(checkpoints[1].id, backdated_higher_id);
    }

    #[test]
    fn list_show_and_unclean_attribute_negative_latest_time_to_checkpoint_authority() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-negative-checkpoint-time", false);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES (?1, -1, 'periodic', 'legacy:negative-time',
                       0, 0, 'snapshot')",
            ["sess-negative-checkpoint-time"],
        )
        .expect("insert negative checkpoint-time corruption fixture");

        for error in [
            list_sessions(&db_path).expect_err("list must reject negative checkpoint time"),
            show_session(&db_path, "sess-negative-checkpoint-time")
                .expect_err("show must reject negative checkpoint time"),
            find_unclean_sessions(&db_path)
                .expect_err("unclean discovery must reject negative checkpoint time"),
        ] {
            assert!(matches!(
                error,
                RestoreError::InvalidPersistedValue {
                    field: "session_checkpoints.checkpoint_at",
                    value: -1,
                }
            ));
        }
    }

    #[test]
    fn list_sessions_rejects_negative_created_at() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-neg-created", -1i64, topology, "0.1.0", 0i64],
        )
        .unwrap();

        let err = list_sessions(&db_path).expect_err("negative created_at");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.created_at",
                value: -1
            }
        ));
    }

    #[test]
    fn list_sessions_rejects_invalid_shutdown_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-bad-clean", 1000i64, topology, "0.1.0", 2i64],
        )
        .unwrap();

        let err = list_sessions(&db_path).expect_err("invalid shutdown_clean");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.shutdown_clean",
                value: 2
            }
        ));
    }

    #[test]
    fn list_sessions_rejects_oversized_ft_version_before_materialization() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-oversized-version", false);
        conn.execute(
            "UPDATE mux_sessions SET ft_version = ?2 WHERE session_id = ?1",
            params![
                "sess-oversized-version",
                "v".repeat(MAX_SESSION_FT_VERSION_BYTES + 1)
            ],
        )
        .unwrap();

        let error = list_sessions(&db_path)
            .expect_err("oversized ft_version must not cross the SQL-to-Rust boundary");
        assert!(matches!(
            error,
            RestoreError::InvalidPersistedText {
                field: "mux_sessions.ft_version"
            }
        ));
    }

    #[test]
    fn show_session_rejects_oversized_host_id_before_materialization() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-oversized-host", false);
        conn.execute(
            "UPDATE mux_sessions SET host_id = ?2 WHERE session_id = ?1",
            params![
                "sess-oversized-host",
                "h".repeat(MAX_SESSION_HOST_ID_BYTES + 1)
            ],
        )
        .unwrap();

        let error = show_session(&db_path, "sess-oversized-host")
            .expect_err("oversized host_id must not cross the SQL-to-Rust boundary");
        assert!(matches!(
            error,
            RestoreError::InvalidPersistedText {
                field: "mux_sessions.host_id"
            }
        ));
    }

    #[test]
    fn find_unclean_sessions_rejects_oversized_host_id_before_materialization() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-oversized-host-detect", false);
        conn.execute(
            "UPDATE mux_sessions SET host_id = ?2 WHERE session_id = ?1",
            params![
                "sess-oversized-host-detect",
                "h".repeat(MAX_SESSION_HOST_ID_BYTES + 1)
            ],
        )
        .unwrap();

        let error = find_unclean_sessions(&db_path)
            .expect_err("oversized host_id must not cross detection's SQL boundary");
        assert!(matches!(
            error,
            RestoreError::InvalidPersistedText {
                field: "mux_sessions.host_id"
            }
        ));
    }

    #[test]
    fn session_doctor_rejects_oversized_session_id_before_materialization() {
        let (db_path, conn, _dir) = setup_test_db();
        let oversized_session_id = "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1);
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions
             (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, 1000, ?2, '0.1.0', 0)",
            params![oversized_session_id, topology],
        )
        .unwrap();

        let error = session_doctor(&db_path)
            .expect_err("oversized session_id must not cross the SQL-to-Rust boundary");
        assert!(matches!(
            error,
            RestoreError::InvalidPersistedText {
                field: "mux_sessions.session_id"
            }
        ));
    }

    #[test]
    fn session_doctor_rejects_invalid_shutdown_clean_flag() {
        let (db_path, conn, _dir) = setup_test_db();
        let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["sess-bad-clean", 1000i64, topology, "0.1.0", 2i64],
        )
        .unwrap();

        let err = session_doctor(&db_path).expect_err("invalid shutdown_clean");
        assert!(matches!(
            err,
            RestoreError::InvalidPersistedValue {
                field: "mux_sessions.shutdown_clean",
                value: 2
            }
        ));
    }

    #[test]
    fn show_session_not_found() {
        let (db_path, _conn, _dir) = setup_test_db();
        let result = show_session(&db_path, "nonexistent");
        assert!(matches!(result, Err(RestoreError::NoSessions)));
    }

    #[test]
    fn public_session_selectors_reject_empty_and_oversized_values_before_io() {
        let dir = tempfile::tempdir().expect("create selector test directory");
        let missing_db = dir.path().join("must-not-be-opened.db");
        let missing_db = missing_db.to_string_lossy().to_string();
        let oversized = "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1);

        for selector in ["", oversized.as_str()] {
            assert!(matches!(
                load_latest_checkpoint(&missing_db, selector),
                Err(RestoreError::InvalidSessionSelector)
            ));
            assert!(matches!(
                show_session(&missing_db, selector),
                Err(RestoreError::InvalidSessionSelector)
            ));
            assert!(matches!(
                delete_session(&missing_db, selector),
                Err(RestoreError::InvalidSessionSelector)
            ));

            let cx = crate::cx::for_request();
            assert!(matches!(
                run_async_test(show_session_with_cx(&cx, &missing_db, selector)),
                Err(RestoreError::InvalidSessionSelector)
            ));
            assert!(matches!(
                run_async_test(delete_session_with_cx(&cx, &missing_db, selector)),
                Err(RestoreError::InvalidSessionSelector)
            ));
        }
        assert!(
            !std::path::Path::new(&missing_db).exists(),
            "selector admission must precede opening or creating storage"
        );
    }

    #[test]
    fn show_session_with_checkpoints() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-show", false);
        insert_checkpoint(&conn, "sess-show", 1000, 2);
        insert_checkpoint(&conn, "sess-show", 2000, 3);

        let (session, checkpoints) = show_session(&db_path, "sess-show").unwrap();
        assert_eq!(session.session_id, "sess-show");
        assert_eq!(checkpoints.len(), 2);
        // Newest first
        assert_eq!(checkpoints[0].checkpoint_at, 2000);
        assert_eq!(checkpoints[0].pane_count, 3);
        assert_eq!(checkpoints[1].checkpoint_at, 1000);
    }

    #[test]
    fn show_session_page_bounds_checkpoints_and_reports_exact_total() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-show-page", false);
        let mut checkpoint_ids = Vec::new();
        for index in 0_i64..5 {
            checkpoint_ids.push(insert_checkpoint(
                &conn,
                "sess-show-page",
                1_000 + index,
                1,
            ));
        }

        let page = show_session_page(&db_path, "sess-show-page", 2, 1)
            .expect("checkpoint summary page");
        assert_eq!(page.session.session_id, "sess-show-page");
        assert_eq!(page.checkpoints.len(), 2);
        assert_eq!(page.total_checkpoints, 5);
        assert_eq!(page.limit, 2);
        assert_eq!(page.offset, 1);
        assert!(page.has_more);
        assert_eq!(page.checkpoints[0].id, checkpoint_ids[3]);
        assert_eq!(page.checkpoints[1].id, checkpoint_ids[2]);

        let last = show_session_page(&db_path, "sess-show-page", 2, 4)
            .expect("last checkpoint summary page");
        assert_eq!(last.checkpoints.len(), 1);
        assert_eq!(last.checkpoints[0].id, checkpoint_ids[0]);
        assert!(!last.has_more);

        let exact = show_session_page(&db_path, "sess-show-page", 5, 0)
            .expect("exact checkpoint summary page");
        assert_eq!(exact.checkpoints.len(), 5);
        assert_eq!(exact.total_checkpoints, 5);
        assert!(!exact.has_more);

        let empty = show_session_page(&db_path, "sess-show-page", 2, 5)
            .expect("empty checkpoint summary page");
        assert!(empty.checkpoints.is_empty());
        assert_eq!(empty.total_checkpoints, 5);
        assert_eq!(empty.offset, 5);
        assert!(!empty.has_more);
    }

    #[test]
    fn nonpaged_session_show_fails_closed_above_materialization_limit() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-show-bounded", false);
        for index in 0..=MAX_SESSION_QUERY_LIMIT {
            insert_checkpoint(
                &conn,
                "sess-show-bounded",
                1_000 + i64::try_from(index).unwrap_or(i64::MAX),
                1,
            );
        }

        let error = show_session(&db_path, "sess-show-bounded")
            .expect_err("nonpaged show must fail closed");
        assert!(matches!(
            error,
            RestoreError::QueryResultLimit {
                resource: "checkpoint_summaries",
                observed,
                limit: MAX_SESSION_QUERY_LIMIT,
            } if observed == MAX_SESSION_QUERY_LIMIT + 1
        ));
    }

    #[test]
    fn session_doctor_healthy() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-d", false);
        let cp_id = insert_checkpoint(&conn, "sess-d", 1000, 1);
        insert_pane_state(&conn, cp_id, 0, None, None);
        insert_clean_receipt(&conn, "sess-d", 1001);

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.total_sessions, 1);
        assert_eq!(report.unclean_sessions, 0);
        assert_eq!(report.total_checkpoints, 2);
        assert_eq!(report.orphaned_checkpoints, 0);
        assert_eq!(report.orphaned_pane_states, 0);
        assert_eq!(report.invalid_resolved_restore_chains, 0);
    }

    #[test]
    fn session_doctor_counts_corrupt_legacy_receipts_as_orphaned_intents() {
        let (db_path, conn, _dir) = setup_test_db();
        let cases = [
            ("sess-doctor-corrupt-receipt", "{".to_string()),
            (
                "sess-doctor-oversized-receipt",
                "x".repeat(MAX_CHECKPOINT_METADATA_BYTES + 1),
            ),
        ];
        for (session_id, metadata_json) in cases {
            insert_session(&conn, session_id, false);
            conn.execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, metadata_json, checkpoint_role
                 ) VALUES (?1, 1001, 'startup', 'corrupt:legacy-receipt',
                           0, 0, ?2, 'restore_receipt')",
                params![session_id, metadata_json],
            )
            .expect("insert malformed legacy receipt fixture");
        }

        let report = session_doctor(&db_path).expect("inspect malformed legacy receipts");
        assert_eq!(report.orphaned_restore_intents, 2);
    }

    #[test]
    fn cross_session_lifecycle_cannot_mask_orphaned_restore_intent() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-intent-owner", false);
        let _source_checkpoint = insert_checkpoint(&conn, "sess-intent-owner", 1000, 0);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (?1, 1100, 'startup', 'rsi2:fixture', 0, 0, '{}', 'restore_intent')",
            ["sess-intent-owner"],
        )
        .unwrap();
        let intent_checkpoint_id = conn.last_insert_rowid();

        insert_session(&conn, "sess-foreign-lifecycle", false);
        let foreign_source = insert_checkpoint(&conn, "sess-foreign-lifecycle", 1000, 0);
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 outcome_checkpoint_id, status, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, NULL, 'intent', 1100, NULL)",
            params![
                intent_checkpoint_id,
                "sess-foreign-lifecycle",
                foreign_source
            ],
        )
        .unwrap();

        // A later independently verified snapshot would otherwise satisfy all
        // clean-authority checks for the intent-owning session. The corrupt
        // cross-session lifecycle must not hide its orphaned intent.
        insert_clean_receipt(&conn, "sess-intent-owner", 1200);
        let (shutdown_clean, clean_checkpoint_id): (i64, Option<i64>) = conn
            .query_row(
                "SELECT shutdown_clean, clean_checkpoint_id
                 FROM mux_sessions WHERE session_id = 'sess-intent-owner'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            !assess_clean_authority(
                &conn,
                "sess-intent-owner",
                shutdown_clean,
                clean_checkpoint_id,
            )
            .unwrap()
        );

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.orphaned_restore_intents, 1);
        assert_eq!(report.unclean_sessions, 2);
    }

    #[test]
    fn session_doctor_detects_unclean() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-crash1", false);
        insert_session(&conn, "sess-crash2", false);
        insert_session(&conn, "sess-clean", true);

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.total_sessions, 3);
        assert_eq!(report.unclean_sessions, 2);
    }

    #[test]
    fn session_doctor_detects_orphans() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-o", true);
        let orphaned_checkpoint = insert_checkpoint(&conn, "sess-o", 1_000, 1);
        insert_pane_state(&conn, orphaned_checkpoint, 1, None, None);

        // Simulate a historical database written by a connection that did not
        // enable foreign-key enforcement.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute(
            "DELETE FROM mux_sessions WHERE session_id = 'sess-o'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (999, 0, '{}')",
            [],
        )
        .unwrap();

        let report = session_doctor(&db_path).unwrap();
        assert_eq!(report.total_sessions, 0);
        assert_eq!(report.orphaned_checkpoints, 1);
        assert_eq!(report.orphaned_pane_states, 2);
    }

    #[test]
    fn session_doctor_rejects_negative_bytes_even_when_sum_cancels_to_zero() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-doctor-negative-bytes", false);
        let negative = insert_checkpoint(&conn, "sess-doctor-negative-bytes", 1_000, 0);
        let positive = insert_checkpoint(&conn, "sess-doctor-negative-bytes", 2_000, 0);
        conn.execute(
            "UPDATE session_checkpoints
             SET total_bytes = CASE id WHEN ?1 THEN -1 WHEN ?2 THEN 1 END
             WHERE id IN (?1, ?2)",
            params![negative, positive],
        )
        .expect("construct cancellation-shaped byte corruption");

        assert!(matches!(
            session_doctor(&db_path),
            Err(RestoreError::InvalidPersistedValue {
                field: "session_checkpoints.total_bytes",
                value: -1,
            })
        ));
    }

    #[test]
    fn delete_session_cascades() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-del", false);
        let cp_id = insert_checkpoint(&conn, "sess-del", 1000, 2);
        insert_pane_state(&conn, cp_id, 0, None, None);
        insert_pane_state(&conn, cp_id, 1, None, None);

        let deleted = delete_session(&db_path, "sess-del").unwrap();
        assert!(deleted);

        // Verify cascade
        let sessions = list_sessions(&db_path).unwrap();
        assert!(sessions.is_empty());

        let cp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cp_count, 0);

        let ps_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ps_count, 0);
    }

    #[test]
    fn delete_session_nonexistent() {
        let (db_path, _conn, _dir) = setup_test_db();
        let deleted = delete_session(&db_path, "nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn delete_session_reports_cleanup_for_orphaned_checkpoint_rows() {
        let (db_path, conn, _dir) = setup_test_db();

        // Simulate a historical writer that admitted parentless rows before
        // every connection enabled foreign-key enforcement.
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "sess-orphan",
                1000i64,
                "periodic",
                "0123456789abcdef",
                1i64,
                64i64
            ],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, ?3)",
            params![checkpoint_id, 7i64, "{}"],
        )
        .unwrap();

        let deleted = delete_session(&db_path, "sess-orphan").unwrap();
        assert!(
            deleted,
            "delete_session must report success when it removed orphaned checkpoint/pane rows"
        );

        let checkpoint_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints WHERE session_id = 'sess-orphan'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_count, 0);

        let pane_state_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
                [checkpoint_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pane_state_count, 0);
    }

    #[test]
    fn delete_session_reports_cleanup_for_lifecycle_only_rows() {
        let (db_path, conn, _dir) = setup_test_db();
        conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 outcome_checkpoint_id, status, created_at, resolved_at
             ) VALUES (999, 'sess-lifecycle-only', 998, NULL, 'intent', 1000, NULL)",
            [],
        )
        .unwrap();

        assert!(delete_session(&db_path, "sess-lifecycle-only").unwrap());
        let lifecycle_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM restore_attempt_lifecycle
                 WHERE session_id = 'sess-lifecycle-only'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lifecycle_count, 0);
    }

    // ---------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // ---------------------------------------------------------------

    #[test]
    fn session_restore_config_default_values() {
        let cfg = SessionRestoreConfig::default();
        assert!(!cfg.auto_restore);
        assert!(!cfg.restore_scrollback);
    }

    #[test]
    fn session_restore_config_serde_roundtrip() {
        let cfg = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: true,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.auto_restore);
        assert!(parsed.restore_scrollback);
    }

    #[test]
    fn session_restore_config_serde_defaults_on_missing_fields() {
        let json = "{}";
        let parsed: SessionRestoreConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.auto_restore);
        assert!(!parsed.restore_scrollback);
    }

    #[test]
    fn retired_session_restore_fields_fail_closed_for_every_json_shape() {
        for (field, message) in [
            (
                "restore_max_lines",
                "session.restore_max_lines was removed",
            ),
            (
                "process_relaunch",
                "session.process_relaunch was removed",
            ),
        ] {
            for value in [
                serde_json::Value::Null,
                serde_json::json!(17),
                serde_json::json!("raw-secret-canary"),
                serde_json::json!({"argv": ["raw-secret-canary"]}),
            ] {
                let encoded = serde_json::json!({field: value}).to_string();
                let error = serde_json::from_str::<SessionRestoreConfig>(&encoded)
                    .expect_err("every explicit retired field representation must fail closed")
                    .to_string();
                assert!(error.contains(message));
                assert!(!error.contains("raw-secret-canary"));
            }
        }
    }

    #[test]
    fn session_restore_config_clone() {
        let cfg = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: false,
        };
        let c = cfg.clone();
        assert!(c.auto_restore);
        assert!(!c.restore_scrollback);
    }

    #[test]
    fn session_restore_config_debug() {
        let cfg = SessionRestoreConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("SessionRestoreConfig"));
        assert!(dbg.contains("auto_restore"));
    }

    #[test]
    fn restore_error_display_database() {
        let err = RestoreError::Database("connection refused".to_string());
        assert_eq!(err.to_string(), "database operation failed");
        assert!(!format!("{err:?}").contains("connection refused"));
    }

    #[test]
    fn restore_error_display_no_sessions() {
        let err = RestoreError::NoSessions;
        assert_eq!(err.to_string(), "no restorable sessions found");
    }

    #[test]
    fn restore_error_display_corrupt_checkpoint() {
        let err = RestoreError::CorruptCheckpoint("invalid JSON".to_string());
        assert_eq!(err.to_string(), "checkpoint data is corrupt");
        assert!(!format!("{err:?}").contains("invalid JSON"));
    }

    #[test]
    fn restore_error_display_topology_parse() {
        let err = RestoreError::TopologyParse("missing windows".to_string());
        assert_eq!(
            err.to_string(),
            "topology deserialization failed"
        );
    }

    #[test]
    fn restore_error_display_wezterm() {
        let err = RestoreError::Wezterm("not running".to_string());
        assert_eq!(err.to_string(), "mux operation failed");
    }

    #[test]
    fn restore_context_errors_preserve_finite_capability_classes() {
        use crate::runtime_async::{ContextError, ContextErrorKind};

        let cx = crate::cx::for_testing();
        for (kind, expected) in [
            (
                ContextErrorKind::DeadlineExceeded,
                RestoreInterruptionReason::DeadlineExceeded,
            ),
            (
                ContextErrorKind::PollQuotaExhausted,
                RestoreInterruptionReason::PollQuotaExhausted,
            ),
            (
                ContextErrorKind::CostQuotaExhausted,
                RestoreInterruptionReason::CostQuotaExhausted,
            ),
            (
                ContextErrorKind::CancelTimeout,
                RestoreInterruptionReason::CancellationCleanupTimedOut,
            ),
            (
                ContextErrorKind::Internal,
                RestoreInterruptionReason::ContextFailure,
            ),
        ] {
            let error = ContextError::new(kind).with_message("raw-context-detail-canary");
            let classified = restore_context_error("unit-test-phase", &cx, &error);
            assert!(matches!(
                classified,
                RestoreError::Interrupted {
                    phase: "unit-test-phase",
                    reason,
                } if reason == expected
            ));
            assert!(!format!("{classified:?}").contains("raw-context-detail-canary"));
        }

        let cancelled_cx = crate::cx::for_testing();
        cancelled_cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("raw-cancel-detail-canary"),
        );
        let cancelled = ContextError::new(ContextErrorKind::Cancelled)
            .with_message("raw-context-detail-canary");
        let classified = restore_context_error("unit-test-phase", &cancelled_cx, &cancelled);
        assert!(matches!(
            classified,
            RestoreError::Interrupted {
                reason: RestoreInterruptionReason::Cancelled,
                ..
            }
        ));
        assert!(!format!("{classified:?}").contains("raw-"));
    }

    #[test]
    fn restore_blocking_errors_distinguish_cancellation_from_infrastructure() {
        use crate::outcome::CancelKind;
        use crate::runtime_async::SpawnBlockingWithCxError;

        for (kind, expected) in [
            (Some(CancelKind::User), RestoreInterruptionReason::Cancelled),
            (
                Some(CancelKind::Deadline),
                RestoreInterruptionReason::DeadlineExceeded,
            ),
            (
                Some(CancelKind::PollQuota),
                RestoreInterruptionReason::PollQuotaExhausted,
            ),
            (
                Some(CancelKind::CostBudget),
                RestoreInterruptionReason::CostQuotaExhausted,
            ),
        ] {
            let error = restore_blocking_error(
                "blocking test",
                SpawnBlockingWithCxError::CancelledMidFlight { kind },
            );
            assert!(matches!(
                error,
                RestoreError::Interrupted {
                    phase: "blocking test",
                    reason,
                } if reason == expected
            ));
        }

        assert!(matches!(
            restore_blocking_error(
                "blocking test",
                SpawnBlockingWithCxError::RuntimeFailure,
            ),
            RestoreError::InfrastructureFailure {
                failure: RestoreInfrastructureFailure::BlockingRuntimeFailure,
                ..
            }
        ));
        assert!(matches!(
            restore_blocking_error(
                "blocking test",
                SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
            ),
            RestoreError::InfrastructureFailure {
                failure: RestoreInfrastructureFailure::CancellationWatcherTimerFailure,
                ..
            }
        ));
    }

    #[test]
    fn restore_error_debug_format() {
        let err = RestoreError::NoSessions;
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("NoSessions"));
    }

    #[test]
    fn session_candidate_clone() {
        let c = SessionCandidate {
            session_id: "sess-1".to_string(),
            created_at: 1000,
            last_checkpoint_at: Some(2000),
            selected_checkpoint_id: Some(17),
            ft_version: "0.1.0".to_string(),
            host_id: Some("host-a".to_string()),
        };
        let c2 = c.clone();
        assert_eq!(c2.session_id, "sess-1");
        assert_eq!(c2.last_checkpoint_at, Some(2000));
        assert_eq!(c2.selected_checkpoint_id, Some(17));
        assert_eq!(c2.host_id, Some("host-a".to_string()));
    }

    #[test]
    fn session_candidate_debug() {
        let c = SessionCandidate {
            session_id: "raw-session-canary".to_string(),
            created_at: 0,
            last_checkpoint_at: None,
            selected_checkpoint_id: Some(19),
            ft_version: "raw-version-canary".to_string(),
            host_id: Some("raw-host-canary".to_string()),
        };
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("SessionCandidate"));
        for canary in ["raw-session", "raw-version", "raw-host"] {
            assert!(!dbg.contains(canary));
        }
    }

    #[test]
    fn checkpoint_data_clone() {
        let d = CheckpointData {
            checkpoint_id: 42,
            session_id: "sess-x".to_string(),
            checkpoint_at: 5000,
            checkpoint_type: "periodic".to_string(),
            checkpoint_role: CheckpointRole::Snapshot,
            verification: CheckpointVerification::LegacyUnverified,
            state_hash: "0123456789abcdef".to_string(),
            metadata_json: Some(r#"{"test":true}"#.to_string()),
            topology_json: Some("{}".to_string()),
            restore_intent_checkpoint_id: None,
            pane_count: 3,
            total_bytes: 0,
            pane_states: vec![],
        };
        let d2 = d.clone();
        assert_eq!(d2.checkpoint_id, 42);
        assert_eq!(d2.pane_count, 3);
        assert_eq!(d2.metadata_json(), Some(r#"{"test":true}"#));
        assert!(d2.pane_states.is_empty());
    }

    #[test]
    fn checkpoint_data_debug_is_content_free() {
        let checkpoint = CheckpointData {
            checkpoint_id: 42,
            session_id: "raw-session-canary".to_string(),
            checkpoint_at: 5000,
            checkpoint_type: "raw-type-canary".to_string(),
            checkpoint_role: CheckpointRole::Snapshot,
            verification: CheckpointVerification::VerifiedV2,
            state_hash: "raw-hash-canary".to_string(),
            metadata_json: Some(r#"{"secret":"raw-metadata-canary"}"#.to_string()),
            topology_json: Some("raw-topology-canary".to_string()),
            restore_intent_checkpoint_id: None,
            pane_count: 1,
            total_bytes: 99,
            pane_states: vec![RestoredPaneState {
                pane_id: 7,
                cwd: Some("raw-cwd-canary".to_string()),
                command: Some("raw-command-canary".to_string()),
                terminal_state: None,
                agent_metadata: None,
                scrollback_checkpoint_seq: None,
                last_output_at: None,
            }],
        };
        let debug = format!("{checkpoint:?}");
        for canary in [
            "raw-session",
            "raw-type",
            "raw-hash",
            "raw-metadata",
            "raw-topology",
            "raw-cwd",
            "raw-command",
        ] {
            assert!(!debug.contains(canary));
        }
        assert!(debug.contains("loaded_pane_count: 1"));
    }

    #[test]
    fn restored_pane_state_clone() {
        let s = RestoredPaneState {
            pane_id: 7,
            cwd: Some("/home".to_string()),
            command: Some("zsh".to_string()),
            terminal_state: None,
            agent_metadata: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let s2 = s.clone();
        assert_eq!(s2.pane_id, 7);
        assert_eq!(s2.cwd.as_deref(), Some("/home"));
        assert_eq!(s2.command.as_deref(), Some("zsh"));
    }

    #[test]
    fn restored_pane_state_debug_is_content_free() {
        let state = RestoredPaneState {
            pane_id: 7,
            cwd: Some("raw-cwd-canary".to_string()),
            command: Some("raw-command-canary".to_string()),
            terminal_state: None,
            agent_metadata: None,
            scrollback_checkpoint_seq: None,
            last_output_at: None,
        };
        let debug = format!("{state:?}");
        assert!(debug.contains("pane_id: 7"));
        assert!(!debug.contains("raw-cwd"));
        assert!(!debug.contains("raw-command"));
    }

    #[test]
    fn session_restore_json_contract_golden() {
        #[derive(serde::Serialize)]
        struct SessionCandidateGolden<'a> {
            session_id: &'a str,
            created_at: u64,
            last_checkpoint_at: Option<u64>,
            selected_checkpoint_id: Option<i64>,
            ft_version: &'a str,
            host_id: Option<&'a str>,
        }

        #[derive(serde::Serialize)]
        struct ShowSessionGolden<'a> {
            session: SessionCandidateGolden<'a>,
            checkpoints: Vec<CheckpointInfo>,
        }

        #[derive(serde::Serialize)]
        struct SessionRestoreGolden<'a> {
            list_sessions: Vec<SessionInfo>,
            show_session: ShowSessionGolden<'a>,
            session_doctor: SessionDoctorReport,
        }

        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-clean", true);
        insert_session(&conn, "sess-restore", false);
        conn.execute(
            "UPDATE mux_sessions
             SET created_at = 1500, last_checkpoint_at = 3000, host_id = 'host-a'
             WHERE session_id = 'sess-restore'",
            [],
        )
        .unwrap();

        let older_checkpoint = insert_checkpoint(&conn, "sess-restore", 2000, 1);
        insert_pane_state(
            &conn,
            older_checkpoint,
            7,
            Some("/agents"),
            Some("codex --resume"),
        );
        conn.execute(
            "UPDATE session_checkpoints
             SET total_bytes = 2048
             WHERE id = ?1",
            [older_checkpoint],
        )
        .unwrap();

        let newer_checkpoint = insert_checkpoint(&conn, "sess-restore", 3000, 2);
        conn.execute(
            "UPDATE session_checkpoints
             SET checkpoint_type = 'startup',
                 total_bytes = 512,
                 state_hash = 'startup-hash',
                 metadata_json = '{\"old_to_new\":{\"7\":42}}'
             WHERE id = ?1",
            [newer_checkpoint],
        )
        .unwrap();

        let list_sessions = list_sessions(&db_path).expect("list sessions");
        let (session, checkpoints) = show_session(&db_path, "sess-restore").expect("show session");
        let session_doctor = session_doctor(&db_path).expect("session doctor");

        let actual = serde_json::to_string_pretty(&SessionRestoreGolden {
            list_sessions,
            show_session: ShowSessionGolden {
                session: SessionCandidateGolden {
                    session_id: &session.session_id,
                    created_at: session.created_at,
                    last_checkpoint_at: session.last_checkpoint_at,
                    selected_checkpoint_id: session.selected_checkpoint_id,
                    ft_version: &session.ft_version,
                    host_id: session.host_id.as_deref(),
                },
                checkpoints,
            },
            session_doctor,
        })
        .expect("serialize golden contract");

        let expected = r#"{
  "list_sessions": [
    {
      "session_id": "sess-restore",
      "created_at": 1500,
      "last_checkpoint_at": 3000,
      "shutdown_clean": false,
      "ft_version": "0.1.0",
      "host_id": "host-a",
      "checkpoint_count": 2,
      "pane_count": 2
    },
    {
      "session_id": "sess-clean",
      "created_at": 1000,
      "last_checkpoint_at": 1000,
      "shutdown_clean": true,
      "ft_version": "0.1.0",
      "host_id": null,
      "checkpoint_count": 1,
      "pane_count": 0
    }
  ],
  "show_session": {
    "session": {
      "session_id": "sess-restore",
      "created_at": 1500,
      "last_checkpoint_at": 3000,
      "selected_checkpoint_id": 3,
      "ft_version": "0.1.0",
      "host_id": "host-a"
    },
    "checkpoints": [
      {
        "id": 3,
        "checkpoint_at": 3000,
        "checkpoint_type": "startup",
        "checkpoint_role": "snapshot",
        "pane_count": 2,
        "total_bytes": 512
      },
      {
        "id": 2,
        "checkpoint_at": 2000,
        "checkpoint_type": "periodic",
        "checkpoint_role": "snapshot",
        "pane_count": 1,
        "total_bytes": 2048
      }
    ]
  },
  "session_doctor": {
    "total_sessions": 2,
    "unclean_sessions": 1,
    "total_checkpoints": 3,
    "orphaned_checkpoints": 0,
    "orphaned_pane_states": 0,
    "unresolved_restore_attempts": 0,
    "invalid_resolved_restore_chains": 0,
    "outcome_complete_restore_attempts": 0,
    "reconciliation_required_restore_attempts": 0,
    "orphaned_restore_intents": 0,
    "total_data_bytes": 2560
  }
}"#;

        assert_eq!(
            actual, expected,
            "session restore JSON contract drifted; review intentional changes before updating the golden"
        );
    }

    #[test]
    fn session_info_serialize() {
        let info = SessionInfo {
            session_id: "sess-1".to_string(),
            created_at: 1000,
            last_checkpoint_at: Some(2000),
            shutdown_clean: true,
            ft_version: "0.1.0".to_string(),
            host_id: None,
            checkpoint_count: 5,
            pane_count: Some(3),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["checkpoint_count"], 5);
        assert_eq!(json["pane_count"], 3);
        assert_eq!(json["shutdown_clean"], true);
    }

    #[test]
    fn checkpoint_info_serialize() {
        let info = CheckpointInfo {
            id: 99,
            checkpoint_at: 5000,
            checkpoint_type: "periodic".to_string(),
            checkpoint_role: CheckpointRole::Snapshot,
            pane_count: 4,
            total_bytes: 8192,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["id"], 99);
        assert_eq!(json["pane_count"], 4);
        assert_eq!(json["total_bytes"], 8192);
    }

    #[test]
    fn session_doctor_report_serialize() {
        let report = SessionDoctorReport {
            total_sessions: 3,
            unclean_sessions: 1,
            total_checkpoints: 10,
            orphaned_checkpoints: 1,
            orphaned_pane_states: 2,
            unresolved_restore_attempts: 1,
            invalid_resolved_restore_chains: 1,
            outcome_complete_restore_attempts: 0,
            reconciliation_required_restore_attempts: 1,
            orphaned_restore_intents: 0,
            total_data_bytes: 4096,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["total_sessions"], 3);
        assert_eq!(json["unclean_sessions"], 1);
        assert_eq!(json["orphaned_checkpoints"], 1);
        assert_eq!(json["orphaned_pane_states"], 2);
        assert_eq!(json["unresolved_restore_attempts"], 1);
        assert_eq!(json["invalid_resolved_restore_chains"], 1);
    }

    #[test]
    fn session_doctor_report_clone() {
        let report = SessionDoctorReport {
            total_sessions: 1,
            unclean_sessions: 0,
            total_checkpoints: 5,
            orphaned_checkpoints: 0,
            orphaned_pane_states: 0,
            unresolved_restore_attempts: 0,
            invalid_resolved_restore_chains: 0,
            outcome_complete_restore_attempts: 0,
            reconciliation_required_restore_attempts: 0,
            orphaned_restore_intents: 0,
            total_data_bytes: 0,
        };
        let c = report.clone();
        assert_eq!(c.total_sessions, 1);
        assert_eq!(c.total_checkpoints, 5);
    }

    #[test]
    fn session_restorer_auto_restore_default() {
        let restorer = SessionRestorer::new(
            Arc::new("/tmp/test.db".to_string()),
            SessionRestoreConfig::default(),
        );
        assert!(!restorer.auto_restore());
    }

    #[test]
    fn session_restorer_auto_restore_enabled() {
        let restorer = SessionRestorer::new(
            Arc::new("/tmp/test.db".to_string()),
            SessionRestoreConfig {
                auto_restore: true,
                ..Default::default()
            },
        );
        assert!(restorer.auto_restore());
    }

    /// ft-xbnl0.2.3 Cx-first: `restore_with_cx` must produce a
    /// RestoreSummary equivalent to `restore` on a single-pane
    /// clean-restore case. Uses the same MockWezterm harness and asserts the
    /// layout-settled count, layout-failed count, and pane map size.
    #[test]
    fn session_restorer_restore_with_cx_matches_legacy() {
        let (db_path, conn, _dir) = setup_test_db();
        insert_session(&conn, "sess-cx-success", false);
        set_single_pane_topology(&conn, "sess-cx-success", 9, "/cx-restore");

        let checkpoint_id = insert_checkpoint(&conn, "sess-cx-success", 5000, 1);
        insert_pane_state(&conn, checkpoint_id, 9, Some("/cx-restore"), Some("bash"));
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = 5000 WHERE session_id = 'sess-cx-success'",
            [],
        )
        .unwrap();

        let restorer =
            SessionRestorer::new(Arc::new(db_path.clone()), SessionRestoreConfig::default());
        let session = restorer.detect().unwrap().expect("restorable session");
        let checkpoint = restorer.load_checkpoint(&session).unwrap();

        let wezterm = Arc::new(MockWezterm::new());
        let cx = crate::cx::for_request();
        let summary =
            run_async_test(restorer.restore_with_cx(&cx, &session, &checkpoint, wezterm.clone()))
                .unwrap();

        assert_eq!(summary.layout_settled_pane_count(), 1);
        assert_eq!(summary.layout_failed_pane_count(), 0);
        assert_eq!(summary.checkpoint_id, checkpoint_id);
        assert_eq!(summary.layout_result.pane_id_map.len(), 1);
    }
}
