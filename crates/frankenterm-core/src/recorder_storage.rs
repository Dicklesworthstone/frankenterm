//! Recorder storage abstraction with append-log and rusqlite backends.
//!
//! This module implements the `wa-oegrb.3.2` hot-path baseline:
//! - append-only batched writes with deterministic offsets
//! - bounded in-flight admission and explicit overload signaling
//! - idempotent `batch_id` handling (durable in SQLite; process-local and bounded by cache
//!   retention for AppendLog until receipt reconstruction lands)
//! - ordinary-reopen writer/checkpoint state and torn-tail recovery
//!
//! ## Write/checkpoint invariant contract
//! - `append_batch` is append-only: accepted records advance `next_offset` and `next_ordinal`
//!   monotonically and never rewrite prior bytes.
//! - `batch_id` is idempotent within retention bounds: replaying an existing `batch_id`
//!   returns the same offsets without duplicating writes. A replay that requests stronger
//!   durability upgrades the cached response after flushing the already-written batch. Reusing
//!   the ID for different ordered event content is a typed, zero-mutation conflict.
//! - `commit_checkpoint` is monotonic per consumer: lower ordinals are rejected with
//!   `CheckpointRegression`, identical ordinals are `NoopAlreadyAdvanced`, and higher ordinals
//!   are accepted as `Advanced`.
//! - checkpoint state survives ordinary reopen in AppendLog's `state.json` or Rusqlite's
//!   `recorder_checkpoints` table. AppendLog's candidate-file and parent-directory sync protocol
//!   is tracked separately; ordinary reopen is not host-power-loss proof.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt as _;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
#[cfg(any(unix, windows))]
use cap_std::fs::MetadataExt as _;
use cap_std::fs::{Dir as CapDir, Metadata as CapMetadata, OpenOptions as CapOpenOptions};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::runtime_async::Mutex;

use crate::recording::RecorderEvent;

/// Stable backend identity for recorder storage implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderBackendKind {
    /// Local append-only log backend.
    AppendLog,
    /// Recorder database implemented with the `rusqlite` crate.
    Rusqlite,
    /// Reserved for a future recorder backend implemented with FrankenSQLite.
    #[serde(rename = "frankensqlite")]
    FrankenSqlite,
}

impl RecorderBackendKind {
    /// Every stable recorder backend identity, including reserved selectors.
    pub const ALL: [Self; 3] = [Self::AppendLog, Self::Rusqlite, Self::FrankenSqlite];
}

impl std::fmt::Display for RecorderBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppendLog => write!(f, "append_log"),
            Self::Rusqlite => write!(f, "rusqlite"),
            Self::FrankenSqlite => write!(f, "frankensqlite"),
        }
    }
}

/// A recorder backend that is implemented and safe to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderBackendSelection {
    /// Local append-only log backend.
    AppendLog,
    /// Recorder database implemented with the `rusqlite` crate.
    Rusqlite,
}

impl RecorderBackendSelection {
    /// Every backend that is implemented and safe to construct.
    pub const ALL: [Self; 2] = [Self::AppendLog, Self::Rusqlite];

    /// Return the public backend identity represented by this selection.
    #[must_use]
    pub const fn backend_kind(self) -> RecorderBackendKind {
        match self {
            Self::AppendLog => RecorderBackendKind::AppendLog,
            Self::Rusqlite => RecorderBackendKind::Rusqlite,
        }
    }
}

/// Typed rejection returned when a requested recorder backend is not wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "recorder backend {requested} is unavailable: FrankenSQLite recorder integration is not wired; use append_log or rusqlite"
)]
pub struct RecorderBackendSelectionError {
    requested: RecorderBackendKind,
}

impl RecorderBackendSelectionError {
    /// Return the backend identity that was rejected.
    #[must_use]
    pub const fn requested(self) -> RecorderBackendKind {
        self.requested
    }
}

/// Resolve a public recorder selector to an implemented backend.
///
/// Callers must invoke this before performing filesystem or database work.
pub fn select_recorder_backend(
    requested: RecorderBackendKind,
) -> std::result::Result<RecorderBackendSelection, RecorderBackendSelectionError> {
    match requested {
        RecorderBackendKind::AppendLog => Ok(RecorderBackendSelection::AppendLog),
        RecorderBackendKind::Rusqlite => Ok(RecorderBackendSelection::Rusqlite),
        RecorderBackendKind::FrankenSqlite => Err(RecorderBackendSelectionError { requested }),
    }
}

/// Describes which backend source the indexer should read from.
///
/// This is the backend-neutral descriptor that `IndexerConfig` uses to
/// select the right [`RecorderEventReader`] without hard-coding a file path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecorderSourceDescriptor {
    /// Append-log file backend.
    AppendLog {
        /// Path to the append-log data file.
        data_path: PathBuf,
    },
    /// Rusqlite database backend.
    Rusqlite {
        /// Path to the SQLite database file.
        db_path: PathBuf,
    },
}

impl std::fmt::Display for RecorderSourceDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppendLog { data_path } => {
                write!(f, "append_log({})", data_path.display())
            }
            Self::Rusqlite { db_path } => {
                write!(f, "rusqlite({})", db_path.display())
            }
        }
    }
}

impl RecorderSourceDescriptor {
    /// Return the backend kind for this descriptor.
    pub fn backend_kind(&self) -> RecorderBackendKind {
        match self {
            Self::AppendLog { .. } => RecorderBackendKind::AppendLog,
            Self::Rusqlite { .. } => RecorderBackendKind::Rusqlite,
        }
    }
}

/// Durability requested by append callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityLevel {
    /// Accepted in bounded in-memory buffer only.
    Enqueued,
    /// Appended to backend write surface (buffer flushed).
    Appended,
    /// Appended and fsync'd to durable media.
    Fsync,
}

impl DurabilityLevel {
    fn rank(self) -> u8 {
        match self {
            Self::Enqueued => 0,
            Self::Appended => 1,
            Self::Fsync => 2,
        }
    }

    fn satisfies(self, required: Self) -> bool {
        self.rank() >= required.rank()
    }
}

/// Flush mode for explicit flush calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushMode {
    /// Flush buffered bytes only.
    Buffered,
    /// Flush and fsync durable state.
    Durable,
}

/// Canonical logical position in the append log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderOffset {
    /// Segment identifier (single-segment for baseline backend).
    pub segment_id: u64,
    /// Byte offset of the record start in segment.
    pub byte_offset: u64,
    /// Monotonic logical ordinal across appended records.
    pub ordinal: u64,
}

/// Append batch request.
#[derive(Debug, Clone)]
pub struct AppendRequest {
    /// Idempotency key for this batch.
    pub batch_id: String,
    /// Ordered recorder events to append.
    pub events: Vec<RecorderEvent>,
    /// Required durability level.
    pub required_durability: DurabilityLevel,
    /// Producer timestamp for diagnostics.
    pub producer_ts_ms: u64,
}

/// Append result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendResponse {
    /// Backend that committed this append.
    pub backend: RecorderBackendKind,
    /// Number of accepted events.
    pub accepted_count: usize,
    /// First committed offset in this batch.
    pub first_offset: RecorderOffset,
    /// Last committed offset in this batch.
    pub last_offset: RecorderOffset,
    /// Durability actually achieved by commit.
    pub committed_durability: DurabilityLevel,
    /// Commit timestamp.
    pub committed_at_ms: u64,
    /// True only when this response came from an existing idempotency entry.
    ///
    /// Canonical receipts persisted by a backend always store `false`; a cache
    /// hit returns a transient clone with this bit set. The serde default keeps
    /// pre-field receipts readable without misclassifying them as replays.
    #[serde(default)]
    pub was_idempotent_replay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAppendReceipt {
    request_digest_sha256: String,
    response: AppendResponse,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CachedAppendReceiptWire {
    Current(CachedAppendReceipt),
    Legacy(AppendResponse),
}

fn digest_serialized_event_payloads<'a>(payloads: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ft.recorder.append-request.v1\0");
    for payload in payloads {
        hasher.update(
            u64::try_from(payload.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(payload);
    }
    hex::encode(hasher.finalize())
}

/// Checkpoint consumer identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointConsumerId(pub String);

/// Durable consumer checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderCheckpoint {
    pub consumer: CheckpointConsumerId,
    pub upto_offset: RecorderOffset,
    pub schema_version: String,
    pub committed_at_ms: u64,
}

/// Result of checkpoint commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointCommitOutcome {
    Advanced,
    NoopAlreadyAdvanced,
    RejectedOutOfOrder,
}

/// Health snapshot for recorder storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderStorageHealth {
    pub backend: RecorderBackendKind,
    pub degraded: bool,
    /// Number of admitted append calls, including owned blocking work still settling in the
    /// built-in backends. Their checkpoint and flush calls use separate single-flight admissions
    /// and are not counted.
    pub queue_depth: usize,
    /// Configured append admission capacity. Built-in checkpoint and flush capacity is one.
    pub queue_capacity: usize,
    pub latest_offset: Option<RecorderOffset>,
    pub last_error: Option<String>,
}

/// Per-consumer lag view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderConsumerLag {
    pub consumer: CheckpointConsumerId,
    pub offsets_behind: u64,
}

/// Lag metrics for storage and checkpoint consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecorderStorageLag {
    pub latest_offset: Option<RecorderOffset>,
    pub consumers: Vec<RecorderConsumerLag>,
}

/// Flush metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushStats {
    pub backend: RecorderBackendKind,
    pub flushed_at_ms: u64,
    pub latest_offset: Option<RecorderOffset>,
}

/// Stable classification for storage errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderStorageErrorClass {
    Retryable,
    Overload,
    TerminalConfig,
    TerminalData,
    Corruption,
    DependencyUnavailable,
}

/// Typed failure reported when recorder work cannot settle through the owned
/// blocking executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderBlockingFailure {
    /// The caller was already cancelled, so no blocking work was admitted.
    CancelledBeforeStart {
        /// Structured cancellation kind, when the caller context carries one.
        kind: Option<crate::outcome::CancelKind>,
    },
    /// The caller cancelled after blocking work was admitted.
    ///
    /// A closure that already started is not preempted and may still settle the
    /// storage effect after this error reaches the caller.
    CancelledMidFlight {
        /// Structured cancellation kind, when the caller context carries one.
        kind: Option<crate::outcome::CancelKind>,
    },
    /// The blocking executor or its result-delivery path failed. The caller
    /// must reconcile storage state before assuming that no effect occurred.
    RuntimeFailure,
    /// The cancellation watcher could not classify a timer failure while the
    /// caller context was still live. Admitted work may still settle after this
    /// error reaches the caller.
    CancellationWatcherTimerFailure,
}

impl std::fmt::Display for RecorderBlockingFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CancelledBeforeStart { kind } => {
                write!(formatter, "cancelled before start (kind={kind:?})")
            }
            Self::CancelledMidFlight { kind } => {
                write!(formatter, "cancelled mid-flight (kind={kind:?})")
            }
            Self::RuntimeFailure => formatter.write_str("runtime failure"),
            Self::CancellationWatcherTimerFailure => {
                formatter.write_str("cancellation watcher timer failure")
            }
        }
    }
}

/// Storage-layer error type with stable classes.
#[derive(Debug, Error)]
pub enum RecorderStorageError {
    #[error("queue full (capacity={capacity})")]
    QueueFull { capacity: usize },

    #[error("invalid append request: {message}")]
    InvalidRequest { message: String },

    #[error(
        "checkpoint regression for consumer {consumer}: current={current_ordinal}, attempted={attempted_ordinal}"
    )]
    CheckpointRegression {
        consumer: String,
        current_ordinal: u64,
        attempted_ordinal: u64,
    },

    #[error("corrupt append-log record at offset={offset}: {reason}")]
    CorruptRecord { offset: u64, reason: String },

    #[error(
        "corrupt cached response for batch {batch_id}: expected backend {expected_backend}, found {actual_backend}"
    )]
    CorruptCachedResponse {
        batch_id: String,
        expected_backend: RecorderBackendKind,
        actual_backend: RecorderBackendKind,
    },

    #[error(
        "corrupt cached response for batch {batch_id}: persisted receipts cannot be marked as idempotent replays"
    )]
    CorruptCachedReplayReceipt { batch_id: String },

    #[error("corrupt cached response for batch {batch_id}: {reason}")]
    CorruptCachedReceiptEncoding { batch_id: String, reason: String },

    #[error("batch_id {batch_id} was reused for different ordered event content")]
    IdempotencyConflict { batch_id: String },

    #[error(
        "storage operation {operation} returned backend {actual_backend}, but the storage instance is {expected_backend}"
    )]
    BackendIdentityMismatch {
        operation: &'static str,
        expected_backend: RecorderBackendKind,
        actual_backend: RecorderBackendKind,
    },

    #[error("recorder {operation} blocking execution failed: {failure}")]
    BlockingOperation {
        operation: &'static str,
        failure: RecorderBlockingFailure,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("SQLite error: {0}")]
    Sqlite(String),

    #[error(transparent)]
    BackendSelection(#[from] RecorderBackendSelectionError),

    #[error("backend {backend} unavailable: {message}")]
    BackendUnavailable {
        backend: RecorderBackendKind,
        message: String,
    },
}

impl RecorderStorageError {
    /// Stable error-class mapping for retry and policy decisions.
    #[must_use]
    pub fn class(&self) -> RecorderStorageErrorClass {
        match self {
            Self::QueueFull { .. } => RecorderStorageErrorClass::Overload,
            Self::InvalidRequest { .. }
            | Self::CheckpointRegression { .. }
            | Self::IdempotencyConflict { .. } => RecorderStorageErrorClass::TerminalData,
            Self::CorruptRecord { .. }
            | Self::CorruptCachedResponse { .. }
            | Self::CorruptCachedReplayReceipt { .. }
            | Self::CorruptCachedReceiptEncoding { .. }
            | Self::BackendIdentityMismatch { .. } => RecorderStorageErrorClass::Corruption,
            Self::BlockingOperation {
                failure: RecorderBlockingFailure::RuntimeFailure,
                ..
            } => RecorderStorageErrorClass::DependencyUnavailable,
            Self::BlockingOperation { .. } => RecorderStorageErrorClass::Retryable,
            Self::Io(_) => RecorderStorageErrorClass::Retryable,
            Self::Json(_) => RecorderStorageErrorClass::TerminalData,
            Self::Sqlite(_) => RecorderStorageErrorClass::Retryable,
            Self::BackendSelection(_) => RecorderStorageErrorClass::TerminalConfig,
            Self::BackendUnavailable { .. } => RecorderStorageErrorClass::DependencyUnavailable,
        }
    }
}

fn recorder_blocking_error(
    operation: &'static str,
    error: crate::runtime_async::SpawnBlockingWithCxError,
) -> RecorderStorageError {
    use crate::runtime_async::SpawnBlockingWithCxError;

    let failure = match error {
        SpawnBlockingWithCxError::CancelledBeforeSpawn { kind } => {
            RecorderBlockingFailure::CancelledBeforeStart { kind }
        }
        SpawnBlockingWithCxError::CancelledMidFlight { kind } => {
            RecorderBlockingFailure::CancelledMidFlight { kind }
        }
        SpawnBlockingWithCxError::RuntimeFailure => RecorderBlockingFailure::RuntimeFailure,
        SpawnBlockingWithCxError::CancellationWatcherTimerFailure => {
            RecorderBlockingFailure::CancellationWatcherTimerFailure
        }
    };
    RecorderStorageError::BlockingOperation { operation, failure }
}

fn recorder_blocking_runtime_error(operation: &'static str) -> RecorderStorageError {
    RecorderStorageError::BlockingOperation {
        operation,
        failure: RecorderBlockingFailure::RuntimeFailure,
    }
}

fn recorder_pre_cancelled_error(
    operation: &'static str,
    cx: &crate::cx::Cx,
) -> Option<RecorderStorageError> {
    cx.checkpoint().is_err().then(|| {
        let kind = cx.root_cancel_cause().map(|reason| reason.kind);
        RecorderStorageError::BlockingOperation {
            operation,
            failure: RecorderBlockingFailure::CancelledBeforeStart { kind },
        }
    })
}

fn recorder_post_admission_cancelled_error(
    operation: &'static str,
    cx: &crate::cx::Cx,
) -> Option<RecorderStorageError> {
    cx.checkpoint().is_err().then(|| {
        let kind = cx.root_cancel_cause().map(|reason| reason.kind);
        RecorderStorageError::BlockingOperation {
            operation,
            failure: RecorderBlockingFailure::CancelledMidFlight { kind },
        }
    })
}

fn deliver_owned_blocking_result<T>(
    operation: &'static str,
    cx: &crate::cx::Cx,
    result: std::result::Result<T, RecorderStorageError>,
) -> std::result::Result<T, RecorderStorageError> {
    let value = result?;
    if let Some(error) = recorder_post_admission_cancelled_error(operation, cx) {
        return Err(error);
    }
    Ok(value)
}

/// Recorder storage boundary used by capture and indexing layers.
#[allow(async_fn_in_trait)]
pub trait RecorderStorage: Send + Sync {
    fn backend_kind(&self) -> RecorderBackendKind;
    /// Return append-log data path when available for file-backed backends.
    ///
    /// Backends without a file-based append log should return `None`.
    fn append_log_data_path(&self) -> Option<&Path> {
        None
    }

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError>;

    /// Cx-first [`Self::append_batch`] (ft-xbnl0.2.3).
    ///
    /// The default implementation rejects pre-cancelled callers before delegating. Built-in
    /// backends additionally offload owned blocking work: cancellation observed after admission
    /// returns typed non-success, while an already-started storage effect settles to its natural
    /// result for exact retry reconciliation. A successful result passes a final caller-Cx
    /// delivery gate.
    async fn append_batch_with_cx(
        &self,
        cx: &crate::cx::Cx,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("append_batch", cx) {
            return Err(error);
        }
        self.append_batch(req).await
    }

    async fn flush(&self, mode: FlushMode)
    -> std::result::Result<FlushStats, RecorderStorageError>;

    /// Cx-first [`Self::flush`] (ft-xbnl0.2.3). The default implementation rejects
    /// pre-cancelled callers before delegating. Built-in backends use the same owned-settlement
    /// and final delivery-gate contract as [`Self::append_batch_with_cx`].
    async fn flush_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("flush", cx) {
            return Err(error);
        }
        self.flush(mode).await
    }

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError>;

    /// Cx-first [`Self::read_checkpoint`] (ft-xbnl0.2.3). Default
    /// implementation checks caller cancellation before
    /// delegating to the non-cx `read_checkpoint`.
    async fn read_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("read_checkpoint", cx) {
            return Err(error);
        }
        self.read_checkpoint(consumer).await
    }

    /// Commit one checkpoint under the backend's mutation authority.
    ///
    /// The built-in backends use fail-fast single-flight admission for checkpoint writes.
    /// An overlapping commit returns [`RecorderStorageError::QueueFull`] with capacity `1`;
    /// callers that generate concurrent checkpoints must retry from their monotonic source
    /// position rather than assuming the queued write will eventually run.
    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError>;

    /// Cx-first [`Self::commit_checkpoint`] (ft-xbnl0.2.3). The default implementation rejects
    /// pre-cancelled callers before delegating. Built-in backends use the same owned-settlement
    /// and final delivery-gate contract as [`Self::append_batch_with_cx`].
    async fn commit_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("commit_checkpoint", cx) {
            return Err(error);
        }
        self.commit_checkpoint(checkpoint).await
    }

    async fn health(&self) -> RecorderStorageHealth;

    /// ft-xbnl0.2.3 Cx-first sibling of [`Self::health`]. Default
    /// implementation checks caller cancellation before
    /// delegating to the non-cx `health`. On cancellation, returns
    /// a degraded health snapshot (the health call itself has no
    /// error surface — it always returns a snapshot).
    async fn health_with_cx(&self, cx: &crate::cx::Cx) -> RecorderStorageHealth {
        if cx.checkpoint().is_err() {
            return RecorderStorageHealth {
                // Preserve the probed backend in degraded snapshots so
                // operators can distinguish a cancelled database-backend
                // probe from a healthy append-log selection.
                backend: self.backend_kind(),
                degraded: true,
                queue_depth: 0,
                queue_capacity: 0,
                latest_offset: None,
                last_error: Some("health cancelled pre-start via Cx".to_string()),
            };
        }
        self.health().await
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError>;

    /// Cx-first [`Self::lag_metrics`] (ft-xbnl0.2.3). Default
    /// implementation checks caller cancellation before
    /// delegating to the non-cx `lag_metrics`.
    async fn lag_metrics_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("lag_metrics", cx) {
            return Err(error);
        }
        self.lag_metrics().await
    }
}

/// Append-log backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppendLogStorageConfig {
    /// Path to append-only data file.
    pub data_path: PathBuf,
    /// Path to persisted writer/checkpoint state.
    ///
    /// Opening resolves existing symlinks and reserves the sibling staging
    /// path (`with_extension("tmp")`) and its `.lock` sidecar. These paths
    /// must be distinct from the data file; the sidecar remains after close.
    pub state_path: PathBuf,
    /// Maximum concurrent append calls admitted.
    pub queue_capacity: usize,
    /// Maximum events accepted in a single batch.
    pub max_batch_events: usize,
    /// Maximum serialized payload bytes accepted in a single batch.
    pub max_batch_bytes: usize,
    /// Maximum idempotency cache entries retained.
    pub max_idempotency_entries: usize,
}

impl AppendLogStorageConfig {
    /// Validate numeric limits; filesystem identity admission happens on open.
    pub fn validate(&self) -> std::result::Result<(), RecorderStorageError> {
        if self.queue_capacity == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "queue_capacity must be >= 1".to_string(),
            });
        }
        if self.max_batch_events == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "max_batch_events must be >= 1".to_string(),
            });
        }
        if self.max_batch_bytes == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "max_batch_bytes must be >= 1".to_string(),
            });
        }
        if self.max_idempotency_entries == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "max_idempotency_entries must be >= 1".to_string(),
            });
        }
        Ok(())
    }
}

impl Default for AppendLogStorageConfig {
    fn default() -> Self {
        let data_path = PathBuf::from(".ft/recorder-log/events.log");
        let state_path = PathBuf::from(".ft/recorder-log/state.json");
        Self {
            data_path,
            state_path,
            queue_capacity: 1024,
            max_batch_events: 256,
            max_batch_bytes: 256 * 1024,
            max_idempotency_entries: 4096,
        }
    }
}

/// Rusqlite recorder backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RusqliteStorageConfig {
    /// Path to the recorder SQLite database.
    pub db_path: PathBuf,
    /// Maximum concurrent append calls admitted.
    pub queue_capacity: usize,
    /// Maximum events accepted in a single batch.
    pub max_batch_events: usize,
    /// Maximum serialized payload bytes accepted in a single batch.
    pub max_batch_bytes: usize,
    /// Maximum idempotency entries retained in the batch cache table.
    pub max_idempotency_entries: usize,
}

impl RusqliteStorageConfig {
    /// Validate config for runtime safety.
    pub fn validate(&self) -> std::result::Result<(), RecorderStorageError> {
        if self.queue_capacity == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "queue_capacity must be >= 1".to_string(),
            });
        }
        if self.max_batch_events == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "max_batch_events must be >= 1".to_string(),
            });
        }
        if self.max_batch_bytes == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "max_batch_bytes must be >= 1".to_string(),
            });
        }
        if self.max_idempotency_entries == 0 {
            return Err(RecorderStorageError::InvalidRequest {
                message: "max_idempotency_entries must be >= 1".to_string(),
            });
        }
        Ok(())
    }
}

impl Default for RusqliteStorageConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from(".ft/recorder-log/events.sqlite3"),
            queue_capacity: 1024,
            max_batch_events: 256,
            max_batch_bytes: 256 * 1024,
            max_idempotency_entries: 4096,
        }
    }
}

/// Startup-time recorder storage selector/config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecorderStorageConfig {
    /// Requested backend kind for recorder writes.
    pub backend: RecorderBackendKind,
    /// Append-log backend settings.
    pub append_log: AppendLogStorageConfig,
    /// Rusqlite backend settings.
    pub rusqlite: RusqliteStorageConfig,
}

impl RecorderStorageConfig {
    /// Resolve the requested backend without performing any I/O.
    pub fn select_backend(
        &self,
    ) -> std::result::Result<RecorderBackendSelection, RecorderBackendSelectionError> {
        select_recorder_backend(self.backend)
    }
}

impl Default for RecorderStorageConfig {
    fn default() -> Self {
        Self {
            backend: RecorderBackendKind::AppendLog,
            append_log: AppendLogStorageConfig::default(),
            rusqlite: RusqliteStorageConfig::default(),
        }
    }
}

/// Runtime-selected recorder storage backend.
#[derive(Debug)]
pub enum RecorderStorageInstance {
    AppendLog(AppendLogRecorderStorage),
    Rusqlite(RusqliteRecorderStorage),
}

impl RecorderStorage for RecorderStorageInstance {
    fn backend_kind(&self) -> RecorderBackendKind {
        match self {
            Self::AppendLog(inner) => inner.backend_kind(),
            Self::Rusqlite(inner) => inner.backend_kind(),
        }
    }

    fn append_log_data_path(&self) -> Option<&Path> {
        match self {
            Self::AppendLog(inner) => inner.append_log_data_path(),
            Self::Rusqlite(inner) => inner.append_log_data_path(),
        }
    }

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.append_batch(req).await,
            Self::Rusqlite(inner) => inner.append_batch(req).await,
        }
    }

    async fn append_batch_with_cx(
        &self,
        cx: &crate::cx::Cx,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.append_batch_with_cx(cx, req).await,
            Self::Rusqlite(inner) => inner.append_batch_with_cx(cx, req).await,
        }
    }

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.flush(mode).await,
            Self::Rusqlite(inner) => inner.flush(mode).await,
        }
    }

    async fn flush_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.flush_with_cx(cx, mode).await,
            Self::Rusqlite(inner) => inner.flush_with_cx(cx, mode).await,
        }
    }

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.read_checkpoint(consumer).await,
            Self::Rusqlite(inner) => inner.read_checkpoint(consumer).await,
        }
    }

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.commit_checkpoint(checkpoint).await,
            Self::Rusqlite(inner) => inner.commit_checkpoint(checkpoint).await,
        }
    }

    async fn commit_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.commit_checkpoint_with_cx(cx, checkpoint).await,
            Self::Rusqlite(inner) => inner.commit_checkpoint_with_cx(cx, checkpoint).await,
        }
    }

    async fn health(&self) -> RecorderStorageHealth {
        match self {
            Self::AppendLog(inner) => inner.health().await,
            Self::Rusqlite(inner) => inner.health().await,
        }
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.lag_metrics().await,
            Self::Rusqlite(inner) => inner.lag_metrics().await,
        }
    }
}

/// Bootstrap recorder storage from startup selector config.
pub fn bootstrap_recorder_storage(
    config: RecorderStorageConfig,
) -> std::result::Result<RecorderStorageInstance, RecorderStorageError> {
    let selection = config.select_backend()?;
    match selection {
        RecorderBackendSelection::AppendLog => {
            tracing::info!(
                target: "recorder::bootstrap",
                backend = %RecorderBackendKind::AppendLog,
                data_path = %config.append_log.data_path.display(),
                state_path = %config.append_log.state_path.display(),
                "Bootstrapping recorder append-log backend"
            );
            let storage = AppendLogRecorderStorage::open(config.append_log)?;
            Ok(RecorderStorageInstance::AppendLog(storage))
        }
        RecorderBackendSelection::Rusqlite => {
            tracing::info!(
                target: "recorder::bootstrap",
                backend = %RecorderBackendKind::Rusqlite,
                db_path = %config.rusqlite.db_path.display(),
                "Bootstrapping recorder rusqlite backend"
            );
            let storage = RusqliteRecorderStorage::open(config.rusqlite)?;
            Ok(RecorderStorageInstance::Rusqlite(storage))
        }
    }
}

/// Baseline append-only recorder backend.
#[derive(Debug, Clone)]
pub struct AppendLogRecorderStorage {
    config: AppendLogStorageConfig,
    in_flight: Arc<AtomicUsize>,
    checkpoint_in_flight: Arc<AtomicUsize>,
    flush_in_flight: Arc<AtomicUsize>,
    inner: Arc<Mutex<AppendLogInner>>,
}

#[cfg(test)]
#[derive(Clone)]
struct RecorderBlockingTestHook {
    before: Arc<dyn Fn() + Send + Sync>,
    after: Arc<dyn Fn() + Send + Sync>,
}

#[cfg(test)]
impl std::fmt::Debug for RecorderBlockingTestHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecorderBlockingTestHook(..)")
    }
}

#[cfg(test)]
impl RecorderBlockingTestHook {
    fn run_before(&self) {
        (self.before)();
    }

    fn run_after(&self) {
        (self.after)();
    }
}

#[derive(Debug)]
struct AppendLogInner {
    writer: std::io::BufWriter<File>,
    // Field order keeps state authority alive through the writer's final flush.
    state_file: AppendLogStateFile,
    segment_id: u64,
    next_offset: u64,
    next_ordinal: u64,
    latest_record_start: Option<u64>,
    checkpoints: HashMap<String, RecorderCheckpoint>,
    idempotency_cache: HashMap<String, CachedAppendReceipt>,
    idempotency_order: VecDeque<String>,
    last_error: Option<String>,
    #[cfg(test)]
    append_test_hook: Option<RecorderBlockingTestHook>,
    #[cfg(test)]
    checkpoint_test_hook: Option<RecorderBlockingTestHook>,
    #[cfg(test)]
    flush_test_hook: Option<RecorderBlockingTestHook>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedState {
    segment_id: u64,
    next_offset: u64,
    next_ordinal: u64,
    checkpoints: HashMap<String, RecorderCheckpoint>,
}

#[derive(Debug, Clone, Copy)]
struct ScanResult {
    valid_len: u64,
    valid_records: u64,
    latest_record_start: Option<u64>,
}

impl ScanResult {
    fn matches_persisted_state(self, persisted: &PersistedState) -> bool {
        persisted.next_offset == self.valid_len && persisted.next_ordinal == self.valid_records
    }
}

fn recover_checkpoints_from_scan(
    checkpoints: HashMap<String, RecorderCheckpoint>,
    recovered_segment_id: u64,
    scan: ScanResult,
) -> HashMap<String, RecorderCheckpoint> {
    checkpoints
        .into_iter()
        .filter_map(|(consumer, mut checkpoint)| {
            let within_scanned_log = scan.valid_records > 0
                && scan.valid_len > 0
                && checkpoint.upto_offset.ordinal < scan.valid_records
                && checkpoint.upto_offset.byte_offset < scan.valid_len;
            if !within_scanned_log {
                return None;
            }
            checkpoint.upto_offset.segment_id = recovered_segment_id;
            Some((consumer, checkpoint))
        })
        .collect()
}

impl AppendLogRecorderStorage {
    /// Open or create an append-log recorder backend.
    ///
    /// On Unix, the data descriptor holds a nonblocking exclusive writer lease
    /// until this backend is dropped. Competing opens fail with a retryable I/O
    /// error before tail recovery can inspect or truncate a live writer's log.
    /// A separate staging-name lease excludes writers sharing snapshot paths.
    /// Path aliases within this configuration are rejected before recovery.
    /// This is not a security boundary for directories writable by an adversary
    /// or a global reservation of paths used in different roles by other logs.
    pub fn open(
        mut config: AppendLogStorageConfig,
    ) -> std::result::Result<Self, RecorderStorageError> {
        config.validate()?;
        config.data_path = resolve_recorder_path(&config.data_path)?;
        config.state_path = resolve_recorder_path(&config.state_path)?;
        validate_recorder_paths(&config)?;
        ensure_parent_dir(&config.data_path)?;
        ensure_parent_dir(&config.state_path)?;

        // The staging-name lease also excludes distinct logs that share state
        // or whose different state extensions produce the same staging path.
        let state_file = AppendLogStateFile::open(&config.state_path)?;
        validate_recorder_paths(&config)?;
        let (data_dir, data_name) = recorder_parent(&config.data_path)?;
        let mut options = recorder_open_options();
        options.create(true).read(true).append(true);
        let mut file = data_dir.open_with(&data_name, &options)?.into_std();
        if !file.metadata()?.is_file() {
            return Err(invalid_recorder_path("data path is not a regular file"));
        }

        // Recovery may truncate a torn tail, so acquire authority before the
        // scan, not just before appending. BufWriter retains this exact File;
        // failed initialization or final owner drop releases its lease without
        // an explicit unlock that could race with a buffered final write.
        // Unix flock is advisory and leaves independent indexer reads working.
        // Windows whole-file locks deny those reads; its writer authority needs
        // a separate protocol rather than silently breaking live observation.
        #[cfg(unix)]
        fs2::FileExt::try_lock_exclusive(&file)?;

        validate_recorder_paths(&config)?;
        let persisted = state_file.load()?;
        let scan = scan_valid_prefix(&mut file)?;
        let recovered_segment_id = 0;
        let state_matches_scan = scan.matches_persisted_state(&persisted);

        let next_offset = if state_matches_scan {
            persisted.next_offset
        } else {
            scan.valid_len
        };

        let next_ordinal = if state_matches_scan {
            persisted.next_ordinal
        } else {
            scan.valid_records
        };

        let segment_id = if state_matches_scan {
            persisted.segment_id
        } else {
            recovered_segment_id
        };

        let checkpoints = if state_matches_scan {
            persisted.checkpoints
        } else {
            recover_checkpoints_from_scan(persisted.checkpoints, recovered_segment_id, scan)
        };

        file.seek(SeekFrom::End(0))?;

        let inner = AppendLogInner {
            writer: std::io::BufWriter::new(file),
            state_file,
            segment_id,
            next_offset,
            next_ordinal,
            latest_record_start: scan.latest_record_start,
            checkpoints,
            idempotency_cache: HashMap::new(),
            idempotency_order: VecDeque::new(),
            last_error: None,
            #[cfg(test)]
            append_test_hook: None,
            #[cfg(test)]
            checkpoint_test_hook: None,
            #[cfg(test)]
            flush_test_hook: None,
        };

        Ok(Self {
            config,
            in_flight: Arc::new(AtomicUsize::new(0)),
            checkpoint_in_flight: Arc::new(AtomicUsize::new(0)),
            flush_in_flight: Arc::new(AtomicUsize::new(0)),
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    fn try_acquire_slot(&self) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
        try_acquire_owned_bounded_slot(&self.in_flight, self.config.queue_capacity)
    }

    fn try_acquire_checkpoint_slot(
        &self,
    ) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
        try_acquire_owned_bounded_slot(&self.checkpoint_in_flight, 1)
    }

    fn try_acquire_flush_slot(
        &self,
    ) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
        try_acquire_owned_bounded_slot(&self.flush_in_flight, 1)
    }

    fn persist_state(inner: &AppendLogInner) -> std::result::Result<(), RecorderStorageError> {
        let persisted = PersistedState {
            segment_id: inner.segment_id,
            next_offset: inner.next_offset,
            next_ordinal: inner.next_ordinal,
            checkpoints: inner.checkpoints.clone(),
        };
        inner.state_file.write(&persisted)
    }

    fn latest_offset(inner: &AppendLogInner) -> Option<RecorderOffset> {
        Some(RecorderOffset {
            segment_id: inner.segment_id,
            byte_offset: inner.latest_record_start?,
            ordinal: inner.next_ordinal.checked_sub(1)?,
        })
    }

    /// Return the append-log data file path.
    pub fn data_path(&self) -> &Path {
        &self.config.data_path
    }

    fn clear_last_error(inner: &mut AppendLogInner) {
        inner.last_error = None;
    }

    fn record_last_error(
        inner: &mut AppendLogInner,
        operation: &'static str,
        err: &RecorderStorageError,
    ) {
        if matches!(
            err.class(),
            RecorderStorageErrorClass::TerminalData | RecorderStorageErrorClass::TerminalConfig
        ) {
            return;
        }
        inner.last_error = Some(format!(
            "{operation} failed (class={:?}): {err}",
            err.class()
        ));
    }

    fn ensure_cached_response_durability(
        inner: &mut AppendLogInner,
        response: &mut AppendResponse,
        required_durability: DurabilityLevel,
    ) -> std::result::Result<(), RecorderStorageError> {
        if response.committed_durability.satisfies(required_durability) {
            return Ok(());
        }

        match required_durability {
            DurabilityLevel::Enqueued => {}
            DurabilityLevel::Appended => {
                inner.writer.flush()?;
                Self::persist_state(inner)?;
            }
            DurabilityLevel::Fsync => {
                inner.writer.flush()?;
                inner.writer.get_ref().sync_data()?;
                Self::persist_state(inner)?;
            }
        }

        response.committed_durability = required_durability;
        response.committed_at_ms = crate::recording::epoch_ms_now();
        Ok(())
    }

    async fn append_batch_on_blocking_thread(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        if req.batch_id.trim().is_empty() {
            return Err(RecorderStorageError::InvalidRequest {
                message: "batch_id must not be empty".to_string(),
            });
        }
        if req.events.is_empty() {
            return Err(RecorderStorageError::InvalidRequest {
                message: "events must not be empty".to_string(),
            });
        }
        if req.events.len() > self.config.max_batch_events {
            return Err(RecorderStorageError::InvalidRequest {
                message: format!(
                    "batch event count {} exceeds max {}",
                    req.events.len(),
                    self.config.max_batch_events
                ),
            });
        }

        let AppendRequest {
            batch_id,
            events,
            required_durability,
            producer_ts_ms: _producer_ts_ms,
        } = req;

        let mut encoded = Vec::with_capacity(events.len());
        let mut total_bytes = 0usize;
        for event in events {
            let payload = serde_json::to_vec(&event)?;
            if payload.len() > u32::MAX as usize {
                return Err(RecorderStorageError::InvalidRequest {
                    message: format!("record payload too large: {} bytes", payload.len()),
                });
            }
            total_bytes = total_bytes
                .checked_add(payload.len().saturating_add(4))
                .ok_or_else(|| RecorderStorageError::InvalidRequest {
                    message: "batch byte size overflow".to_string(),
                })?;
            encoded.push(payload);
        }
        if total_bytes > self.config.max_batch_bytes {
            return Err(RecorderStorageError::InvalidRequest {
                message: format!(
                    "batch bytes {} exceeds max {}",
                    total_bytes, self.config.max_batch_bytes
                ),
            });
        }
        let request_digest_sha256 =
            digest_serialized_event_payloads(encoded.iter().map(Vec::as_slice));

        // Caller cancellation controls the wait for the blocking result, not
        // settlement of an admitted append. Use an independent live context
        // for per-backend serialization so the owned closure always reaches a
        // terminal result and releases its admission guard.
        let settlement_cx = crate::cx::for_request();
        let mut inner = self
            .inner
            .lock_with_cx(&settlement_cx)
            .await
            .map_err(|_| recorder_blocking_runtime_error("append_batch"))?;

        #[cfg(test)]
        let append_test_hook = inner.append_test_hook.clone();
        #[cfg(test)]
        if let Some(hook) = &append_test_hook {
            hook.run_before();
        }

        let result = (|| -> std::result::Result<AppendResponse, RecorderStorageError> {
            if let Some(mut existing) = inner.idempotency_cache.get(&batch_id).cloned() {
                if existing.request_digest_sha256 != request_digest_sha256 {
                    return Err(RecorderStorageError::IdempotencyConflict {
                        batch_id: batch_id.clone(),
                    });
                }
                if existing.response.was_idempotent_replay {
                    return Err(RecorderStorageError::CorruptCachedReplayReceipt {
                        batch_id: batch_id.clone(),
                    });
                }
                Self::ensure_cached_response_durability(
                    &mut inner,
                    &mut existing.response,
                    required_durability,
                )?;
                inner.idempotency_cache.insert(batch_id, existing.clone());
                existing.response.was_idempotent_replay = true;
                return Ok(existing.response);
            }

            let first_offset = RecorderOffset {
                segment_id: inner.segment_id,
                byte_offset: inner.next_offset,
                ordinal: inner.next_ordinal,
            };
            let mut last_offset = first_offset.clone();

            for payload in encoded {
                let payload_len = payload.len();
                let record_start = inner.next_offset;
                let ordinal = inner.next_ordinal;
                inner
                    .writer
                    .write_all(&(payload_len as u32).to_le_bytes())?;
                inner.writer.write_all(&payload)?;
                inner.next_offset += 4 + payload_len as u64;
                inner.next_ordinal += 1;
                inner.latest_record_start = Some(record_start);
                last_offset = RecorderOffset {
                    segment_id: inner.segment_id,
                    byte_offset: record_start,
                    ordinal,
                };
            }

            let mut response = AppendResponse {
                backend: RecorderBackendKind::AppendLog,
                accepted_count: last_offset
                    .ordinal
                    .saturating_sub(first_offset.ordinal)
                    .saturating_add(1) as usize,
                first_offset,
                last_offset,
                committed_durability: DurabilityLevel::Enqueued,
                committed_at_ms: crate::recording::epoch_ms_now(),
                was_idempotent_replay: false,
            };

            // Every record is now accepted by the writer. Retain its identity
            // before any flush, sync, or state save can fail: a retry must
            // upgrade this receipt, not append the same events a second time.
            inner.idempotency_cache.insert(
                batch_id.clone(),
                CachedAppendReceipt {
                    request_digest_sha256: request_digest_sha256.clone(),
                    response: response.clone(),
                },
            );
            inner.idempotency_order.push_back(batch_id.clone());
            while inner.idempotency_cache.len() > self.config.max_idempotency_entries {
                if let Some(evict) = inner.idempotency_order.pop_front() {
                    inner.idempotency_cache.remove(&evict);
                }
            }
            Self::ensure_cached_response_durability(
                &mut inner,
                &mut response,
                required_durability,
            )?;
            inner.idempotency_cache.insert(
                batch_id,
                CachedAppendReceipt {
                    request_digest_sha256,
                    response: response.clone(),
                },
            );
            Ok(response)
        })();

        #[cfg(test)]
        if let Some(hook) = &append_test_hook {
            hook.run_after();
        }

        match result {
            Ok(response) => {
                Self::clear_last_error(&mut inner);
                Ok(response)
            }
            Err(err) => {
                Self::record_last_error(&mut inner, "append_batch", &err);
                Err(err)
            }
        }
    }

    async fn commit_checkpoint_on_blocking_thread(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        let settlement_cx = crate::cx::for_request();
        let mut inner = self
            .inner
            .lock_with_cx(&settlement_cx)
            .await
            .map_err(|_| recorder_blocking_runtime_error("commit_checkpoint"))?;

        #[cfg(test)]
        let checkpoint_test_hook = inner.checkpoint_test_hook.clone();
        #[cfg(test)]
        if let Some(hook) = &checkpoint_test_hook {
            hook.run_before();
        }

        let result = (|| -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
            let key = checkpoint.consumer.0.clone();
            let outcome = match inner.checkpoints.get(&key) {
                Some(existing) if checkpoint.upto_offset.ordinal < existing.upto_offset.ordinal => {
                    return Err(RecorderStorageError::CheckpointRegression {
                        consumer: key,
                        current_ordinal: existing.upto_offset.ordinal,
                        attempted_ordinal: checkpoint.upto_offset.ordinal,
                    });
                }
                Some(existing)
                    if checkpoint.upto_offset.ordinal == existing.upto_offset.ordinal =>
                {
                    CheckpointCommitOutcome::NoopAlreadyAdvanced
                }
                _ => CheckpointCommitOutcome::Advanced,
            };

            if outcome == CheckpointCommitOutcome::Advanced {
                // Publish in memory only after the state-file replacement
                // succeeds. Otherwise a failed save makes an equal retry
                // return NoopAlreadyAdvanced without ever saving its progress.
                let mut persisted = PersistedState {
                    segment_id: inner.segment_id,
                    next_offset: inner.next_offset,
                    next_ordinal: inner.next_ordinal,
                    checkpoints: inner.checkpoints.clone(),
                };
                persisted.checkpoints.insert(key, checkpoint);
                inner.state_file.write(&persisted)?;
                inner.checkpoints = persisted.checkpoints;
            }

            Ok(outcome)
        })();

        #[cfg(test)]
        if let Some(hook) = &checkpoint_test_hook {
            hook.run_after();
        }

        match result {
            Ok(outcome) => {
                Self::clear_last_error(&mut inner);
                Ok(outcome)
            }
            Err(err) => {
                Self::record_last_error(&mut inner, "commit_checkpoint", &err);
                Err(err)
            }
        }
    }

    async fn flush_on_blocking_thread(
        inner: Arc<Mutex<AppendLogInner>>,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        // Caller cancellation controls the wait for admission, not settlement of
        // an already-admitted storage effect. Use a live infrastructure context
        // for the serialization lock so a mid-flight caller cancellation cannot
        // strand the owned blocking closure before it reaches a terminal result.
        let settlement_cx = crate::cx::for_request();
        let mut inner = inner
            .lock_with_cx(&settlement_cx)
            .await
            .map_err(|_| recorder_blocking_runtime_error("flush"))?;

        #[cfg(test)]
        let flush_test_hook = inner.flush_test_hook.clone();
        #[cfg(test)]
        if let Some(hook) = &flush_test_hook {
            hook.run_before();
        }

        let result = (|| -> std::result::Result<FlushStats, RecorderStorageError> {
            inner.writer.flush()?;
            if mode == FlushMode::Durable {
                inner.writer.get_ref().sync_data()?;
            }
            Self::persist_state(&inner)?;
            Ok(FlushStats {
                backend: RecorderBackendKind::AppendLog,
                flushed_at_ms: crate::recording::epoch_ms_now(),
                latest_offset: Self::latest_offset(&inner),
            })
        })();

        #[cfg(test)]
        if let Some(hook) = &flush_test_hook {
            hook.run_after();
        }

        match result {
            Ok(stats) => {
                Self::clear_last_error(&mut inner);
                Ok(stats)
            }
            Err(err) => {
                Self::record_last_error(&mut inner, "flush", &err);
                Err(err)
            }
        }
    }
}

#[cfg(test)]
struct InFlightGuard<'a> {
    counter: &'a AtomicUsize,
}

#[cfg(test)]
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

struct OwnedInFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for OwnedInFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
fn try_acquire_bounded_slot(
    counter: &AtomicUsize,
    capacity: usize,
) -> std::result::Result<InFlightGuard<'_>, RecorderStorageError> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current >= capacity {
            return Err(RecorderStorageError::QueueFull { capacity });
        }

        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(InFlightGuard { counter }),
            Err(observed) => current = observed,
        }
    }
}

fn try_acquire_owned_bounded_slot(
    counter: &Arc<AtomicUsize>,
    capacity: usize,
) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current >= capacity {
            return Err(RecorderStorageError::QueueFull { capacity });
        }

        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return Ok(OwnedInFlightGuard {
                    counter: Arc::clone(counter),
                });
            }
            Err(observed) => current = observed,
        }
    }
}

impl RecorderStorage for AppendLogRecorderStorage {
    fn backend_kind(&self) -> RecorderBackendKind {
        RecorderBackendKind::AppendLog
    }

    fn append_log_data_path(&self) -> Option<&Path> {
        Some(&self.config.data_path)
    }

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        let slot = self.try_acquire_slot()?;
        let storage = self.clone();
        crate::runtime_async::spawn_blocking(move || {
            let _slot = slot;
            futures::executor::block_on(storage.append_batch_on_blocking_thread(req))
        })
        .await
        .map_err(|_| recorder_blocking_runtime_error("append_batch"))?
    }

    async fn append_batch_with_cx(
        &self,
        cx: &crate::cx::Cx,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("append_batch", cx) {
            return Err(error);
        }
        let slot = self.try_acquire_slot()?;
        let storage = self.clone();
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _slot = slot;
            futures::executor::block_on(storage.append_batch_on_blocking_thread(req))
        })
        .await
        .map_err(|error| recorder_blocking_error("append_batch", error))?;
        deliver_owned_blocking_result("append_batch", cx, result)
    }

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        let flush_slot = self.try_acquire_flush_slot()?;
        let inner = Arc::clone(&self.inner);
        crate::runtime_async::spawn_blocking(move || {
            let _flush_slot = flush_slot;
            futures::executor::block_on(Self::flush_on_blocking_thread(inner, mode))
        })
        .await
        .map_err(|_| recorder_blocking_runtime_error("flush"))?
    }

    async fn flush_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("flush", cx) {
            return Err(error);
        }
        let flush_slot = self.try_acquire_flush_slot()?;
        let inner = Arc::clone(&self.inner);
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _flush_slot = flush_slot;
            futures::executor::block_on(Self::flush_on_blocking_thread(inner, mode))
        })
        .await
        .map_err(|error| recorder_blocking_error("flush", error))?;
        deliver_owned_blocking_result("flush", cx, result)
    }

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        let inner = self.inner.lock().await;
        Ok(inner.checkpoints.get(&consumer.0).cloned())
    }

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        let slot = self.try_acquire_checkpoint_slot()?;
        let storage = self.clone();
        crate::runtime_async::spawn_blocking(move || {
            let _slot = slot;
            futures::executor::block_on(storage.commit_checkpoint_on_blocking_thread(checkpoint))
        })
        .await
        .map_err(|_| recorder_blocking_runtime_error("commit_checkpoint"))?
    }

    async fn commit_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("commit_checkpoint", cx) {
            return Err(error);
        }
        let slot = self.try_acquire_checkpoint_slot()?;
        let storage = self.clone();
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _slot = slot;
            futures::executor::block_on(storage.commit_checkpoint_on_blocking_thread(checkpoint))
        })
        .await
        .map_err(|error| recorder_blocking_error("commit_checkpoint", error))?;
        deliver_owned_blocking_result("commit_checkpoint", cx, result)
    }

    async fn health(&self) -> RecorderStorageHealth {
        let inner = self.inner.lock().await;
        RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded: inner.last_error.is_some(),
            queue_depth: self.in_flight.load(Ordering::Acquire),
            queue_capacity: self.config.queue_capacity,
            latest_offset: Self::latest_offset(&inner),
            last_error: inner.last_error.clone(),
        }
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        let inner = self.inner.lock().await;
        let latest = Self::latest_offset(&inner);
        let latest_ordinal = latest.as_ref().map_or(0, |o| o.ordinal);

        let mut consumers = Vec::with_capacity(inner.checkpoints.len());
        for checkpoint in inner.checkpoints.values() {
            consumers.push(RecorderConsumerLag {
                consumer: checkpoint.consumer.clone(),
                offsets_behind: latest_ordinal.saturating_sub(checkpoint.upto_offset.ordinal),
            });
        }
        consumers.sort_by(|a, b| a.consumer.0.cmp(&b.consumer.0));

        Ok(RecorderStorageLag {
            latest_offset: latest,
            consumers,
        })
    }
}

/// Rusqlite-backed recorder storage and event reader.
#[derive(Clone)]
pub struct RusqliteRecorderStorage {
    config: RusqliteStorageConfig,
    in_flight: Arc<AtomicUsize>,
    checkpoint_in_flight: Arc<AtomicUsize>,
    flush_in_flight: Arc<AtomicUsize>,
    inner: Arc<Mutex<RusqliteInner>>,
}

struct RusqliteInner {
    conn: Connection,
    last_error: Option<String>,
    #[cfg(test)]
    append_test_hook: Option<RecorderBlockingTestHook>,
    #[cfg(test)]
    checkpoint_test_hook: Option<RecorderBlockingTestHook>,
    #[cfg(test)]
    flush_test_hook: Option<RecorderBlockingTestHook>,
}

impl std::fmt::Debug for RusqliteRecorderStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RusqliteRecorderStorage")
            .field("db_path", &self.config.db_path)
            .field("queue_capacity", &self.config.queue_capacity)
            .field("in_flight", &self.in_flight.load(Ordering::Relaxed))
            .field(
                "checkpoint_in_flight",
                &self.checkpoint_in_flight.load(Ordering::Relaxed),
            )
            .field(
                "flush_in_flight",
                &self.flush_in_flight.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl RusqliteRecorderStorage {
    /// Open or create a rusqlite recorder backend.
    pub fn open(config: RusqliteStorageConfig) -> std::result::Result<Self, RecorderStorageError> {
        config.validate()?;
        ensure_parent_dir(&config.db_path)?;
        let conn = Connection::open(&config.db_path).map_err(sqlite_error)?;
        configure_rusqlite_durability(&conn)?;
        initialize_rusqlite_schema(&conn)?;
        Ok(Self {
            config,
            in_flight: Arc::new(AtomicUsize::new(0)),
            checkpoint_in_flight: Arc::new(AtomicUsize::new(0)),
            flush_in_flight: Arc::new(AtomicUsize::new(0)),
            inner: Arc::new(Mutex::new(RusqliteInner {
                conn,
                last_error: None,
                #[cfg(test)]
                append_test_hook: None,
                #[cfg(test)]
                checkpoint_test_hook: None,
                #[cfg(test)]
                flush_test_hook: None,
            })),
        })
    }

    /// Return a reader over this storage's event stream.
    pub fn event_reader(&self) -> RusqliteEventReader {
        RusqliteEventReader::new(self.config.db_path.clone())
    }

    fn try_acquire_slot(&self) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
        try_acquire_owned_bounded_slot(&self.in_flight, self.config.queue_capacity)
    }

    fn try_acquire_checkpoint_slot(
        &self,
    ) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
        try_acquire_owned_bounded_slot(&self.checkpoint_in_flight, 1)
    }

    fn try_acquire_flush_slot(
        &self,
    ) -> std::result::Result<OwnedInFlightGuard, RecorderStorageError> {
        try_acquire_owned_bounded_slot(&self.flush_in_flight, 1)
    }

    fn clear_last_error(inner: &mut RusqliteInner) {
        inner.last_error = None;
    }

    fn record_last_error(
        inner: &mut RusqliteInner,
        operation: &'static str,
        err: &RecorderStorageError,
    ) {
        if matches!(
            err.class(),
            RecorderStorageErrorClass::TerminalData | RecorderStorageErrorClass::TerminalConfig
        ) {
            return;
        }
        inner.last_error = Some(format!(
            "{operation} failed (class={:?}): {err}",
            err.class()
        ));
    }

    fn cached_response(
        conn: &Connection,
        batch_id: &str,
    ) -> std::result::Result<Option<CachedAppendReceipt>, RecorderStorageError> {
        let response_json = conn
            .query_row(
                "SELECT response_json FROM recorder_batches WHERE batch_id = ?1",
                params![batch_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        let receipt = response_json
            .map(|json| {
                let wire = serde_json::from_str::<CachedAppendReceiptWire>(&json).map_err(
                    |error| RecorderStorageError::CorruptCachedReceiptEncoding {
                        batch_id: batch_id.to_string(),
                        reason: format!(
                            "receipt JSON matched neither the current nor legacy schema: {error}"
                        ),
                    },
                )?;
                let (request_digest_sha256, response) = match wire {
                    CachedAppendReceiptWire::Current(receipt) => {
                        (Some(receipt.request_digest_sha256), receipt.response)
                    }
                    CachedAppendReceiptWire::Legacy(response) => (None, response),
                };
                if response.backend != RecorderBackendKind::Rusqlite {
                    return Err(RecorderStorageError::CorruptCachedResponse {
                        batch_id: batch_id.to_string(),
                        expected_backend: RecorderBackendKind::Rusqlite,
                        actual_backend: response.backend,
                    });
                }
                if response.was_idempotent_replay {
                    return Err(RecorderStorageError::CorruptCachedReplayReceipt {
                        batch_id: batch_id.to_string(),
                    });
                }
                let receipt_i64 = |value: u64, field: &str| {
                    i64::try_from(value).map_err(|_| {
                        RecorderStorageError::CorruptCachedReceiptEncoding {
                            batch_id: batch_id.to_string(),
                            reason: format!("{field} value {value} exceeds SQLite INTEGER range"),
                        }
                    })
                };
                let first_ordinal = receipt_i64(response.first_offset.ordinal, "first ordinal")?;
                let last_ordinal = receipt_i64(response.last_offset.ordinal, "last ordinal")?;
                let mut statement = conn
                    .prepare(
                        "SELECT ordinal, segment_id, byte_offset, payload_json
                         FROM recorder_events
                         WHERE batch_id = ?1 AND ordinal BETWEEN ?2 AND ?3
                         ORDER BY ordinal ASC",
                    )
                    .map_err(sqlite_error)?;
                let rows = statement
                    .query_map(params![batch_id, first_ordinal, last_ordinal], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })
                    .map_err(sqlite_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(sqlite_error)?;
                let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
                    return Err(RecorderStorageError::CorruptCachedReceiptEncoding {
                        batch_id: batch_id.to_string(),
                        reason: "receipt has no committed event rows".to_string(),
                    });
                };
                if response.accepted_count != rows.len() {
                    return Err(RecorderStorageError::CorruptCachedReceiptEncoding {
                        batch_id: batch_id.to_string(),
                        reason: format!(
                            "receipt accepted_count {} does not match {} committed events",
                            response.accepted_count,
                            rows.len()
                        ),
                    });
                }
                let expected_first = (
                    first_ordinal,
                    receipt_i64(response.first_offset.segment_id, "first segment_id")?,
                    receipt_i64(response.first_offset.byte_offset, "first byte_offset")?,
                );
                let expected_last = (
                    last_ordinal,
                    receipt_i64(response.last_offset.segment_id, "last segment_id")?,
                    receipt_i64(response.last_offset.byte_offset, "last byte_offset")?,
                );
                let actual_first = (first.0, first.1, first.2);
                let actual_last = (last.0, last.1, last.2);
                if actual_first != expected_first || actual_last != expected_last {
                    return Err(RecorderStorageError::CorruptCachedReceiptEncoding {
                        batch_id: batch_id.to_string(),
                        reason: "receipt offsets do not match the committed event range"
                            .to_string(),
                    });
                }
                for (index, row) in rows.iter().enumerate() {
                    let index = i64::try_from(index).map_err(|_| {
                        RecorderStorageError::CorruptCachedReceiptEncoding {
                            batch_id: batch_id.to_string(),
                            reason: "committed event count cannot fit in i64".to_string(),
                        }
                    })?;
                    if row.0
                        != first_ordinal.checked_add(index).ok_or_else(|| {
                            RecorderStorageError::CorruptCachedReceiptEncoding {
                                batch_id: batch_id.to_string(),
                                reason: "committed event ordinal range overflowed".to_string(),
                            }
                        })?
                    {
                        return Err(RecorderStorageError::CorruptCachedReceiptEncoding {
                            batch_id: batch_id.to_string(),
                            reason: "committed event ordinals are not contiguous".to_string(),
                        });
                    }
                }
                let stored_digest = digest_serialized_event_payloads(
                    rows.iter().map(|(_, _, _, payload)| payload.as_bytes()),
                );
                let request_digest_sha256 =
                    request_digest_sha256.unwrap_or_else(|| stored_digest.clone());
                let canonical_digest = request_digest_sha256.len() == 64
                    && request_digest_sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
                if !canonical_digest {
                    return Err(RecorderStorageError::CorruptCachedReceiptEncoding {
                        batch_id: batch_id.to_string(),
                        reason: "request digest is not 64 lowercase hexadecimal bytes".to_string(),
                    });
                }
                if request_digest_sha256 != stored_digest {
                    return Err(RecorderStorageError::CorruptCachedReceiptEncoding {
                        batch_id: batch_id.to_string(),
                        reason: "request digest does not match the committed event payloads"
                            .to_string(),
                    });
                }
                Ok(CachedAppendReceipt {
                    request_digest_sha256,
                    response,
                })
            })
            .transpose()?;
        Ok(receipt)
    }

    fn update_cached_response(
        conn: &Connection,
        batch_id: &str,
        receipt: &CachedAppendReceipt,
    ) -> std::result::Result<(), RecorderStorageError> {
        if receipt.response.was_idempotent_replay {
            return Err(RecorderStorageError::CorruptCachedReplayReceipt {
                batch_id: batch_id.to_string(),
            });
        }
        let response_json = serde_json::to_string(receipt)?;
        conn.execute(
            "UPDATE recorder_batches
             SET response_json = ?2, committed_at_ms = ?3
             WHERE batch_id = ?1",
            params![
                batch_id,
                response_json,
                u64_to_sql_i64(receipt.response.committed_at_ms, "committed_at_ms")?,
            ],
        )
        .map_err(sqlite_error)?;
        Ok(())
    }

    async fn append_batch_on_blocking_thread(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        if req.batch_id.trim().is_empty() {
            return Err(RecorderStorageError::InvalidRequest {
                message: "batch_id must not be empty".to_string(),
            });
        }
        if req.events.is_empty() {
            return Err(RecorderStorageError::InvalidRequest {
                message: "events must not be empty".to_string(),
            });
        }
        if req.events.len() > self.config.max_batch_events {
            return Err(RecorderStorageError::InvalidRequest {
                message: format!(
                    "batch event count {} exceeds max {}",
                    req.events.len(),
                    self.config.max_batch_events
                ),
            });
        }

        let AppendRequest {
            batch_id,
            events,
            required_durability,
            producer_ts_ms: _producer_ts_ms,
        } = req;

        let mut encoded = Vec::with_capacity(events.len());
        let mut total_bytes = 0usize;
        for event in events {
            let payload = serde_json::to_string(&event)?;
            total_bytes = total_bytes
                .checked_add(payload.len().saturating_add(4))
                .ok_or_else(|| RecorderStorageError::InvalidRequest {
                    message: "batch byte size overflow".to_string(),
                })?;
            encoded.push((event, payload));
        }
        if total_bytes > self.config.max_batch_bytes {
            return Err(RecorderStorageError::InvalidRequest {
                message: format!(
                    "batch bytes {} exceeds max {}",
                    total_bytes, self.config.max_batch_bytes
                ),
            });
        }
        let request_digest_sha256 =
            digest_serialized_event_payloads(encoded.iter().map(|(_, payload)| payload.as_bytes()));

        let settlement_cx = crate::cx::for_request();
        let mut inner = self
            .inner
            .lock_with_cx(&settlement_cx)
            .await
            .map_err(|_| recorder_blocking_runtime_error("append_batch"))?;

        #[cfg(test)]
        let append_test_hook = inner.append_test_hook.clone();
        #[cfg(test)]
        if let Some(hook) = &append_test_hook {
            hook.run_before();
        }

        let result = (|| -> std::result::Result<AppendResponse, RecorderStorageError> {
            if let Some(mut existing) = Self::cached_response(&inner.conn, &batch_id)? {
                if existing.request_digest_sha256 != request_digest_sha256 {
                    return Err(RecorderStorageError::IdempotencyConflict {
                        batch_id: batch_id.clone(),
                    });
                }
                if !existing
                    .response
                    .committed_durability
                    .satisfies(required_durability)
                {
                    existing.response.committed_durability = required_durability;
                    existing.response.committed_at_ms = crate::recording::epoch_ms_now();
                    Self::update_cached_response(&inner.conn, &batch_id, &existing)?;
                }
                existing.response.was_idempotent_replay = true;
                return Ok(existing.response);
            }

            let head = sqlite_head_offset(&inner.conn)?;
            let transaction = inner.conn.transaction().map_err(sqlite_error)?;
            let first_offset = head.clone();
            let mut next_ordinal = head.ordinal;
            let mut next_byte_offset = head.byte_offset;
            let mut last_offset = first_offset.clone();
            let committed_at_ms = crate::recording::epoch_ms_now();

            for (event, payload) in encoded {
                let payload_bytes = payload.len() as u64;
                let offset = RecorderOffset {
                    segment_id: 0,
                    byte_offset: next_byte_offset,
                    ordinal: next_ordinal,
                };
                transaction
                    .execute(
                        "INSERT INTO recorder_events (
                             ordinal, segment_id, byte_offset, payload_json, payload_bytes,
                             event_id, pane_id, schema_version, recorded_at_ms, batch_id, inserted_at_ms
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            u64_to_sql_i64(offset.ordinal, "ordinal")?,
                            u64_to_sql_i64(offset.segment_id, "segment_id")?,
                            u64_to_sql_i64(offset.byte_offset, "byte_offset")?,
                            payload,
                            u64_to_sql_i64(payload_bytes, "payload_bytes")?,
                            event.event_id.as_str(),
                            u64_to_sql_i64(event.pane_id, "pane_id")?,
                            event.schema_version.as_str(),
                            u64_to_sql_i64(event.recorded_at_ms, "recorded_at_ms")?,
                            batch_id.as_str(),
                            u64_to_sql_i64(committed_at_ms, "inserted_at_ms")?,
                        ],
                    )
                    .map_err(sqlite_error)?;
                last_offset = offset;
                next_ordinal = next_ordinal.saturating_add(1);
                next_byte_offset = next_byte_offset.saturating_add(payload_bytes.saturating_add(4));
            }

            let response = AppendResponse {
                backend: RecorderBackendKind::Rusqlite,
                accepted_count: last_offset
                    .ordinal
                    .saturating_sub(first_offset.ordinal)
                    .saturating_add(1) as usize,
                first_offset,
                last_offset,
                committed_durability: required_durability,
                committed_at_ms,
                was_idempotent_replay: false,
            };
            let receipt = CachedAppendReceipt {
                request_digest_sha256,
                response: response.clone(),
            };
            let response_json = serde_json::to_string(&receipt)?;
            transaction
                .execute(
                    "INSERT INTO recorder_batches (batch_id, response_json, committed_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![
                        batch_id.as_str(),
                        response_json,
                        u64_to_sql_i64(committed_at_ms, "committed_at_ms")?,
                    ],
                )
                .map_err(sqlite_error)?;
            evict_rusqlite_receipts(&transaction, self.config.max_idempotency_entries)?;
            transaction.commit().map_err(sqlite_error)?;
            Ok(response)
        })();

        #[cfg(test)]
        if let Some(hook) = &append_test_hook {
            hook.run_after();
        }

        match result {
            Ok(response) => {
                Self::clear_last_error(&mut inner);
                Ok(response)
            }
            Err(err) => {
                Self::record_last_error(&mut inner, "append_batch", &err);
                Err(err)
            }
        }
    }

    async fn commit_checkpoint_on_blocking_thread(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        let settlement_cx = crate::cx::for_request();
        let mut inner = self
            .inner
            .lock_with_cx(&settlement_cx)
            .await
            .map_err(|_| recorder_blocking_runtime_error("commit_checkpoint"))?;

        #[cfg(test)]
        let checkpoint_test_hook = inner.checkpoint_test_hook.clone();
        #[cfg(test)]
        if let Some(hook) = &checkpoint_test_hook {
            hook.run_before();
        }

        let result = (|| -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
            // The async mutex covers this instance only. Acquire SQLite writer
            // authority before reading so another connection cannot advance the
            // checkpoint between our comparison and upsert (or noop receipt).
            let transaction = inner
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(sqlite_error)?;
            let existing = read_sqlite_checkpoint(&transaction, &checkpoint.consumer)?;
            let outcome = match existing {
                Some(existing) if checkpoint.upto_offset.ordinal < existing.upto_offset.ordinal => {
                    return Err(RecorderStorageError::CheckpointRegression {
                        consumer: checkpoint.consumer.0.clone(),
                        current_ordinal: existing.upto_offset.ordinal,
                        attempted_ordinal: checkpoint.upto_offset.ordinal,
                    });
                }
                Some(existing)
                    if checkpoint.upto_offset.ordinal == existing.upto_offset.ordinal =>
                {
                    CheckpointCommitOutcome::NoopAlreadyAdvanced
                }
                _ => CheckpointCommitOutcome::Advanced,
            };

            if outcome == CheckpointCommitOutcome::Advanced {
                transaction
                    .execute(
                        "INSERT INTO recorder_checkpoints (
                             consumer, segment_id, byte_offset, ordinal,
                             schema_version, committed_at_ms
                         )
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT(consumer) DO UPDATE SET
                             segment_id = excluded.segment_id,
                             byte_offset = excluded.byte_offset,
                             ordinal = excluded.ordinal,
                             schema_version = excluded.schema_version,
                             committed_at_ms = excluded.committed_at_ms",
                        params![
                            checkpoint.consumer.0,
                            u64_to_sql_i64(checkpoint.upto_offset.segment_id, "segment_id")?,
                            u64_to_sql_i64(checkpoint.upto_offset.byte_offset, "byte_offset")?,
                            u64_to_sql_i64(checkpoint.upto_offset.ordinal, "ordinal")?,
                            checkpoint.schema_version,
                            u64_to_sql_i64(checkpoint.committed_at_ms, "committed_at_ms")?,
                        ],
                    )
                    .map_err(sqlite_error)?;
            }
            transaction.commit().map_err(sqlite_error)?;
            Ok(outcome)
        })();

        #[cfg(test)]
        if let Some(hook) = &checkpoint_test_hook {
            hook.run_after();
        }

        match result {
            Ok(outcome) => {
                Self::clear_last_error(&mut inner);
                Ok(outcome)
            }
            Err(err) => {
                Self::record_last_error(&mut inner, "commit_checkpoint", &err);
                Err(err)
            }
        }
    }

    async fn flush_on_blocking_thread(
        inner: Arc<Mutex<RusqliteInner>>,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        // An admitted SQLite flush remains owned even when the caller
        // stops waiting. Do not let caller cancellation abort the per-backend
        // serialization acquire and leave the closure with an unknown lifetime.
        let settlement_cx = crate::cx::for_request();
        let mut inner = inner
            .lock_with_cx(&settlement_cx)
            .await
            .map_err(|_| recorder_blocking_runtime_error("flush"))?;

        #[cfg(test)]
        let flush_test_hook = inner.flush_test_hook.clone();
        #[cfg(test)]
        if let Some(hook) = &flush_test_hook {
            hook.run_before();
        }

        let result = (|| -> std::result::Result<FlushStats, RecorderStorageError> {
            if mode == FlushMode::Durable {
                inner
                    .conn
                    .execute_batch("PRAGMA wal_checkpoint(FULL);")
                    .map_err(sqlite_error)?;
            }
            Ok(FlushStats {
                backend: RecorderBackendKind::Rusqlite,
                flushed_at_ms: crate::recording::epoch_ms_now(),
                latest_offset: sqlite_latest_storage_offset(&inner.conn)?,
            })
        })();

        #[cfg(test)]
        if let Some(hook) = &flush_test_hook {
            hook.run_after();
        }

        match result {
            Ok(stats) => {
                Self::clear_last_error(&mut inner);
                Ok(stats)
            }
            Err(err) => {
                Self::record_last_error(&mut inner, "flush", &err);
                Err(err)
            }
        }
    }
}

impl RecorderStorage for RusqliteRecorderStorage {
    fn backend_kind(&self) -> RecorderBackendKind {
        RecorderBackendKind::Rusqlite
    }

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        let slot = self.try_acquire_slot()?;
        let storage = self.clone();
        crate::runtime_async::spawn_blocking(move || {
            let _slot = slot;
            futures::executor::block_on(storage.append_batch_on_blocking_thread(req))
        })
        .await
        .map_err(|_| recorder_blocking_runtime_error("append_batch"))?
    }

    async fn append_batch_with_cx(
        &self,
        cx: &crate::cx::Cx,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("append_batch", cx) {
            return Err(error);
        }
        let slot = self.try_acquire_slot()?;
        let storage = self.clone();
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _slot = slot;
            futures::executor::block_on(storage.append_batch_on_blocking_thread(req))
        })
        .await
        .map_err(|error| recorder_blocking_error("append_batch", error))?;
        deliver_owned_blocking_result("append_batch", cx, result)
    }

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        let flush_slot = self.try_acquire_flush_slot()?;
        let inner = Arc::clone(&self.inner);
        crate::runtime_async::spawn_blocking(move || {
            let _flush_slot = flush_slot;
            futures::executor::block_on(Self::flush_on_blocking_thread(inner, mode))
        })
        .await
        .map_err(|_| recorder_blocking_runtime_error("flush"))?
    }

    async fn flush_with_cx(
        &self,
        cx: &crate::cx::Cx,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("flush", cx) {
            return Err(error);
        }
        let flush_slot = self.try_acquire_flush_slot()?;
        let inner = Arc::clone(&self.inner);
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _flush_slot = flush_slot;
            futures::executor::block_on(Self::flush_on_blocking_thread(inner, mode))
        })
        .await
        .map_err(|error| recorder_blocking_error("flush", error))?;
        deliver_owned_blocking_result("flush", cx, result)
    }

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        let inner = self.inner.lock().await;
        read_sqlite_checkpoint(&inner.conn, consumer)
    }

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        let slot = self.try_acquire_checkpoint_slot()?;
        let storage = self.clone();
        crate::runtime_async::spawn_blocking(move || {
            let _slot = slot;
            futures::executor::block_on(storage.commit_checkpoint_on_blocking_thread(checkpoint))
        })
        .await
        .map_err(|_| recorder_blocking_runtime_error("commit_checkpoint"))?
    }

    async fn commit_checkpoint_with_cx(
        &self,
        cx: &crate::cx::Cx,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        if let Some(error) = recorder_pre_cancelled_error("commit_checkpoint", cx) {
            return Err(error);
        }
        let slot = self.try_acquire_checkpoint_slot()?;
        let storage = self.clone();
        let result = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
            let _slot = slot;
            futures::executor::block_on(storage.commit_checkpoint_on_blocking_thread(checkpoint))
        })
        .await
        .map_err(|error| recorder_blocking_error("commit_checkpoint", error))?;
        deliver_owned_blocking_result("commit_checkpoint", cx, result)
    }

    async fn health(&self) -> RecorderStorageHealth {
        let inner = self.inner.lock().await;
        let latest_offset = match sqlite_latest_storage_offset(&inner.conn) {
            Ok(offset) => offset,
            Err(err) => {
                return RecorderStorageHealth {
                    backend: RecorderBackendKind::Rusqlite,
                    degraded: true,
                    queue_depth: self.in_flight.load(Ordering::Acquire),
                    queue_capacity: self.config.queue_capacity,
                    latest_offset: None,
                    last_error: Some(err.to_string()),
                };
            }
        };
        RecorderStorageHealth {
            backend: RecorderBackendKind::Rusqlite,
            degraded: inner.last_error.is_some(),
            queue_depth: self.in_flight.load(Ordering::Acquire),
            queue_capacity: self.config.queue_capacity,
            latest_offset,
            last_error: inner.last_error.clone(),
        }
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        let inner = self.inner.lock().await;
        let latest = sqlite_latest_storage_offset(&inner.conn)?;
        let latest_ordinal = latest.as_ref().map_or(0, |o| o.ordinal);
        let mut stmt = inner
            .conn
            .prepare(
                "SELECT consumer, segment_id, byte_offset, ordinal, schema_version, committed_at_ms
                 FROM recorder_checkpoints
                 ORDER BY consumer ASC",
            )
            .map_err(sqlite_error)?;
        let mut rows = stmt.query([]).map_err(sqlite_error)?;
        let mut consumers = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let checkpoint = sqlite_checkpoint_from_row(row)?;
            consumers.push(RecorderConsumerLag {
                consumer: checkpoint.consumer,
                offsets_behind: latest_ordinal.saturating_sub(checkpoint.upto_offset.ordinal),
            });
        }
        Ok(RecorderStorageLag {
            latest_offset: latest,
            consumers,
        })
    }
}

/// Reader over a rusqlite recorder event stream.
#[derive(Debug, Clone)]
pub struct RusqliteEventReader {
    db_path: PathBuf,
}

impl RusqliteEventReader {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open_connection(&self) -> std::result::Result<Connection, EventCursorError> {
        let conn = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|err| {
                EventCursorError::Unavailable(format!(
                    "failed to open existing rusqlite recorder DB {} read-only: {err}",
                    self.db_path.display()
                ))
            })?;
        conn.prepare(
            "SELECT ordinal, segment_id, byte_offset, payload_json, payload_bytes
             FROM recorder_events
             LIMIT 0",
        )
        .map_err(|err| {
            EventCursorError::Unavailable(format!(
                "rusqlite recorder DB {} is missing the required recorder_events schema: {err}",
                self.db_path.display()
            ))
        })?;
        Ok(conn)
    }
}

impl RecorderEventReader for RusqliteEventReader {
    fn open_cursor(
        &self,
        from: RecorderOffset,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        self.open_cursor_at_ordinal(from.ordinal)
    }

    fn open_cursor_at_ordinal(
        &self,
        target_ordinal: u64,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        let conn = self.open_connection()?;
        let start = sqlite_cursor_start(&conn, target_ordinal)?;
        Ok(Box::new(RusqliteCursor {
            conn,
            next_offset: start,
        }))
    }

    fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
        let conn = self.open_connection()?;
        sqlite_head_offset(&conn).map_err(|err| EventCursorError::Unavailable(err.to_string()))
    }
}

struct RusqliteCursor {
    conn: Connection,
    next_offset: RecorderOffset,
}

impl RecorderEventCursor for RusqliteCursor {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> std::result::Result<Vec<CursorRecord>, EventCursorError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let limit = usize_to_sql_i64(max, "cursor batch size")
            .map_err(|err| EventCursorError::Unavailable(err.to_string()))?;
        let start_ordinal = u64_to_sql_i64(self.next_offset.ordinal, "cursor ordinal")
            .map_err(|err| EventCursorError::Unavailable(err.to_string()))?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ordinal, segment_id, byte_offset, payload_json, payload_bytes
                 FROM recorder_events
                 WHERE ordinal >= ?1
                 ORDER BY ordinal ASC
                 LIMIT ?2",
            )
            .map_err(|err| EventCursorError::Io(err.to_string()))?;
        let mut rows = stmt
            .query(params![start_ordinal, limit])
            .map_err(|err| EventCursorError::Io(err.to_string()))?;
        let mut batch = Vec::with_capacity(max.min(256));
        while let Some(row) = rows
            .next()
            .map_err(|err| EventCursorError::Io(err.to_string()))?
        {
            let ordinal = sql_i64_to_u64(
                row.get::<_, i64>(0)
                    .map_err(|err| EventCursorError::Io(err.to_string()))?,
                "recorder_events.ordinal",
            )?;
            let segment_id = sql_i64_to_u64(
                row.get::<_, i64>(1)
                    .map_err(|err| EventCursorError::Io(err.to_string()))?,
                "recorder_events.segment_id",
            )?;
            let byte_offset = sql_i64_to_u64(
                row.get::<_, i64>(2)
                    .map_err(|err| EventCursorError::Io(err.to_string()))?,
                "recorder_events.byte_offset",
            )?;
            let payload_json = row
                .get::<_, String>(3)
                .map_err(|err| EventCursorError::Io(err.to_string()))?;
            let payload_bytes = sql_i64_to_u64(
                row.get::<_, i64>(4)
                    .map_err(|err| EventCursorError::Io(err.to_string()))?,
                "recorder_events.payload_bytes",
            )?;
            let offset = RecorderOffset {
                segment_id,
                byte_offset,
                ordinal,
            };
            let event = serde_json::from_str::<RecorderEvent>(&payload_json).map_err(|err| {
                EventCursorError::Corrupt {
                    offset: offset.clone(),
                    reason: err.to_string(),
                }
            })?;
            self.next_offset = RecorderOffset {
                segment_id,
                byte_offset: byte_offset.saturating_add(payload_bytes.saturating_add(4)),
                ordinal: ordinal.saturating_add(1),
            };
            batch.push(CursorRecord { event, offset });
        }
        Ok(batch)
    }

    fn current_offset(&self) -> RecorderOffset {
        self.next_offset.clone()
    }
}

fn ensure_parent_dir(path: &Path) -> std::result::Result<(), RecorderStorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn sqlite_error(err: rusqlite::Error) -> RecorderStorageError {
    RecorderStorageError::Sqlite(err.to_string())
}

fn u64_to_sql_i64(value: u64, field: &str) -> std::result::Result<i64, RecorderStorageError> {
    i64::try_from(value).map_err(|_| RecorderStorageError::InvalidRequest {
        message: format!("{field} value {value} exceeds SQLite INTEGER range"),
    })
}

fn usize_to_sql_i64(value: usize, field: &str) -> std::result::Result<i64, RecorderStorageError> {
    i64::try_from(value).map_err(|_| RecorderStorageError::InvalidRequest {
        message: format!("{field} value {value} exceeds SQLite INTEGER range"),
    })
}

fn sql_i64_to_u64(value: i64, field: &str) -> std::result::Result<u64, EventCursorError> {
    u64::try_from(value).map_err(|_| EventCursorError::Corrupt {
        offset: RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        },
        reason: format!("{field} is negative: {value}"),
    })
}

fn evict_rusqlite_receipts(
    conn: &Connection,
    max_entries: usize,
) -> std::result::Result<(), RecorderStorageError> {
    conn.execute(
        "DELETE FROM recorder_batches
         WHERE rowid IN (
             SELECT rowid FROM recorder_batches
             ORDER BY rowid ASC
             LIMIT (
                 SELECT CASE
                     WHEN COUNT(*) > ?1 THEN COUNT(*) - ?1
                     ELSE 0
                 END
                 FROM recorder_batches
             )
         )",
        params![usize_to_sql_i64(max_entries, "max_idempotency_entries")?],
    )
    .map_err(sqlite_error)?;
    Ok(())
}

/// Establish the writer policy before schema changes or append receipts.
///
/// EXTRA adds directory synchronization for DELETE-journal commits and equals
/// FULL in WAL mode. Preserve existing safe journal modes rather than migrate
/// a database (and contend with its readers) merely by opening a recorder.
/// fullfsync requests the stronger flush on platforms that support it; reading
/// this flag on Linux is not evidence of hardware power-loss durability.
/// https://www.sqlite.org/pragma.html#pragma_synchronous
fn configure_rusqlite_durability(
    conn: &Connection,
) -> std::result::Result<(), RecorderStorageError> {
    let journal_mode: String = conn
        .query_row("PRAGMA main.journal_mode", [], |row| row.get(0))
        .map_err(sqlite_error)?;
    let file_backed: bool = conn
        .query_row(
            "SELECT length(file) > 0 FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if !file_backed
        || !matches!(
            journal_mode.as_str(),
            "delete" | "truncate" | "persist" | "wal"
        )
    {
        return Err(RecorderStorageError::InvalidRequest {
            message: "SQLite recorder requires a file-backed database with a persistent journal"
                .to_string(),
        });
    }
    conn.execute_batch("PRAGMA main.synchronous = EXTRA; PRAGMA fullfsync = ON;")
        .map_err(sqlite_error)?;
    // SQLite silently ignores unknown PRAGMAs. Do not infer the policy solely
    // from execute_batch succeeding, including on alternate SQLite builds.
    let synchronous: i64 = conn
        .query_row("PRAGMA main.synchronous", [], |row| row.get(0))
        .map_err(sqlite_error)?;
    let fullfsync: i64 = conn
        .query_row("PRAGMA fullfsync", [], |row| row.get(0))
        .map_err(sqlite_error)?;
    if synchronous != 3 || fullfsync != 1 {
        return Err(RecorderStorageError::InvalidRequest {
            message: "SQLite recorder could not establish EXTRA/fullfsync durability policy"
                .to_string(),
        });
    }
    tracing::debug!(
        target: "recorder::bootstrap",
        journal_mode,
        synchronous,
        fullfsync,
        "Verified SQLite recorder connection durability policy"
    );
    Ok(())
}

fn initialize_rusqlite_schema(conn: &Connection) -> std::result::Result<(), RecorderStorageError> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS recorder_events (
            ordinal INTEGER PRIMARY KEY,
            segment_id INTEGER NOT NULL DEFAULT 0,
            byte_offset INTEGER NOT NULL,
            payload_json TEXT NOT NULL,
            payload_bytes INTEGER NOT NULL,
            event_id TEXT NOT NULL,
            pane_id INTEGER NOT NULL,
            schema_version TEXT NOT NULL,
            recorded_at_ms INTEGER NOT NULL,
            batch_id TEXT NOT NULL,
            inserted_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_recorder_events_event_id
            ON recorder_events(event_id);
        CREATE INDEX IF NOT EXISTS idx_recorder_events_pane_ordinal
            ON recorder_events(pane_id, ordinal);
        CREATE TABLE IF NOT EXISTS recorder_batches (
            batch_id TEXT PRIMARY KEY,
            response_json TEXT NOT NULL,
            committed_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS recorder_checkpoints (
            consumer TEXT PRIMARY KEY,
            segment_id INTEGER NOT NULL,
            byte_offset INTEGER NOT NULL,
            ordinal INTEGER NOT NULL,
            schema_version TEXT NOT NULL,
            committed_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_recorder_checkpoints_ordinal
            ON recorder_checkpoints(ordinal);
        ",
    )
    .map_err(sqlite_error)
}

fn sqlite_last_record(
    conn: &Connection,
) -> std::result::Result<Option<(RecorderOffset, u64)>, RecorderStorageError> {
    let row = conn
        .query_row(
            "SELECT ordinal, segment_id, byte_offset, payload_bytes
             FROM recorder_events
             ORDER BY ordinal DESC
             LIMIT 1",
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
        .optional()
        .map_err(sqlite_error)?;
    let Some((ordinal, segment_id, byte_offset, payload_bytes)) = row else {
        return Ok(None);
    };
    let ordinal = u64::try_from(ordinal).map_err(|_| RecorderStorageError::CorruptRecord {
        offset: 0,
        reason: format!("recorder_events.ordinal is negative: {ordinal}"),
    })?;
    let segment_id =
        u64::try_from(segment_id).map_err(|_| RecorderStorageError::CorruptRecord {
            offset: 0,
            reason: format!("recorder_events.segment_id is negative: {segment_id}"),
        })?;
    let byte_offset =
        u64::try_from(byte_offset).map_err(|_| RecorderStorageError::CorruptRecord {
            offset: 0,
            reason: format!("recorder_events.byte_offset is negative: {byte_offset}"),
        })?;
    let payload_bytes =
        u64::try_from(payload_bytes).map_err(|_| RecorderStorageError::CorruptRecord {
            offset: byte_offset,
            reason: format!("recorder_events.payload_bytes is negative: {payload_bytes}"),
        })?;
    Ok(Some((
        RecorderOffset {
            segment_id,
            byte_offset,
            ordinal,
        },
        payload_bytes,
    )))
}

fn sqlite_head_offset(
    conn: &Connection,
) -> std::result::Result<RecorderOffset, RecorderStorageError> {
    let Some((last, payload_bytes)) = sqlite_last_record(conn)? else {
        return Ok(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        });
    };
    Ok(RecorderOffset {
        segment_id: last.segment_id,
        byte_offset: last
            .byte_offset
            .saturating_add(payload_bytes.saturating_add(4)),
        ordinal: last.ordinal.saturating_add(1),
    })
}

fn sqlite_latest_storage_offset(
    conn: &Connection,
) -> std::result::Result<Option<RecorderOffset>, RecorderStorageError> {
    Ok(sqlite_last_record(conn)?.map(|(offset, _)| offset))
}

fn sqlite_cursor_start(
    conn: &Connection,
    target_ordinal: u64,
) -> std::result::Result<RecorderOffset, EventCursorError> {
    let target = i64::try_from(target_ordinal).map_err(|_| {
        EventCursorError::Unavailable(format!(
            "target ordinal {target_ordinal} exceeds SQLite INTEGER range"
        ))
    })?;
    let row = conn
        .query_row(
            "SELECT ordinal, segment_id, byte_offset
             FROM recorder_events
             WHERE ordinal >= ?1
             ORDER BY ordinal ASC
             LIMIT 1",
            params![target],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|err| EventCursorError::Io(err.to_string()))?;
    if let Some((ordinal, segment_id, byte_offset)) = row {
        return Ok(RecorderOffset {
            segment_id: sql_i64_to_u64(segment_id, "recorder_events.segment_id")?,
            byte_offset: sql_i64_to_u64(byte_offset, "recorder_events.byte_offset")?,
            ordinal: sql_i64_to_u64(ordinal, "recorder_events.ordinal")?,
        });
    }
    sqlite_head_offset(conn).map_err(|err| EventCursorError::Unavailable(err.to_string()))
}

fn sqlite_checkpoint_from_row(
    row: &rusqlite::Row<'_>,
) -> std::result::Result<RecorderCheckpoint, RecorderStorageError> {
    let consumer = row.get::<_, String>(0).map_err(sqlite_error)?;
    let segment_id = row.get::<_, i64>(1).map_err(sqlite_error)?;
    let byte_offset = row.get::<_, i64>(2).map_err(sqlite_error)?;
    let ordinal = row.get::<_, i64>(3).map_err(sqlite_error)?;
    let schema_version = row.get::<_, String>(4).map_err(sqlite_error)?;
    let committed_at_ms = row.get::<_, i64>(5).map_err(sqlite_error)?;
    Ok(RecorderCheckpoint {
        consumer: CheckpointConsumerId(consumer),
        upto_offset: RecorderOffset {
            segment_id: u64::try_from(segment_id).map_err(|_| {
                RecorderStorageError::CorruptRecord {
                    offset: 0,
                    reason: format!("checkpoint segment_id is negative: {segment_id}"),
                }
            })?,
            byte_offset: u64::try_from(byte_offset).map_err(|_| {
                RecorderStorageError::CorruptRecord {
                    offset: 0,
                    reason: format!("checkpoint byte_offset is negative: {byte_offset}"),
                }
            })?,
            ordinal: u64::try_from(ordinal).map_err(|_| RecorderStorageError::CorruptRecord {
                offset: 0,
                reason: format!("checkpoint ordinal is negative: {ordinal}"),
            })?,
        },
        schema_version,
        committed_at_ms: u64::try_from(committed_at_ms).map_err(|_| {
            RecorderStorageError::CorruptRecord {
                offset: 0,
                reason: format!("checkpoint committed_at_ms is negative: {committed_at_ms}"),
            }
        })?,
    })
}

fn read_sqlite_checkpoint(
    conn: &Connection,
    consumer: &CheckpointConsumerId,
) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
    let mut stmt = conn
        .prepare(
            "SELECT consumer, segment_id, byte_offset, ordinal, schema_version, committed_at_ms
             FROM recorder_checkpoints
             WHERE consumer = ?1",
        )
        .map_err(sqlite_error)?;
    let mut rows = stmt
        .query(params![consumer.0.as_str()])
        .map_err(sqlite_error)?;
    rows.next()
        .map_err(sqlite_error)?
        .map(sqlite_checkpoint_from_row)
        .transpose()
}

fn invalid_recorder_path(message: impl Into<String>) -> RecorderStorageError {
    RecorderStorageError::InvalidRequest {
        message: message.into(),
    }
}

/// Resolve existing symlinks before reducing parent components. Missing
/// descendants can be normalized without creating directories or files.
fn resolve_recorder_path(path: &Path) -> std::result::Result<PathBuf, RecorderStorageError> {
    let mut resolved = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        if component == std::path::Component::ParentDir {
            match std::fs::metadata(&resolved) {
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(invalid_recorder_path(
                        "parent traversal through a non-directory",
                    ));
                }
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
                _ => {}
            }
            resolved.pop();
            continue;
        }
        resolved.push(component.as_os_str());
        match std::fs::canonicalize(&resolved) {
            Ok(canonical) => resolved = canonical,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        return Err(invalid_recorder_path(
                            "recorder path has a dangling symlink",
                        ));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err.into()),
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(resolved)
}

fn recorder_parent(path: &Path) -> std::result::Result<(CapDir, PathBuf), RecorderStorageError> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid_recorder_path("recorder path needs a file name"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((
        CapDir::open_ambient_dir(parent, cap_std::ambient_authority())?,
        PathBuf::from(name),
    ))
}

fn recorder_open_options() -> CapOpenOptions {
    let mut options = CapOpenOptions::new();
    options.follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.nonblock(true);
    options
}

fn recorder_file_identity(
    metadata: &CapMetadata,
) -> std::result::Result<(u64, u64), RecorderStorageError> {
    #[cfg(unix)]
    {
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        metadata
            .volume_serial_number()
            .zip(metadata.file_index())
            .map(|(volume, index)| (u64::from(volume), index))
            .ok_or_else(|| invalid_recorder_path("recorder file identity is unavailable"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Err(invalid_recorder_path(
            "recorder file identity is unsupported",
        ))
    }
}

fn recorder_link_count(metadata: &CapMetadata) -> std::result::Result<u64, RecorderStorageError> {
    #[cfg(unix)]
    {
        Ok(metadata.nlink())
    }
    #[cfg(windows)]
    {
        metadata
            .number_of_links()
            .map(u64::from)
            .ok_or_else(|| invalid_recorder_path("recorder file link count is unavailable"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Err(invalid_recorder_path(
            "recorder file link count is unsupported",
        ))
    }
}

fn recorder_state_lock_path(state_path: &Path) -> PathBuf {
    let mut lock_path = state_path.with_extension("tmp").into_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

/// Read-only admission. Do this before opening the log: recovery can truncate it.
fn validate_recorder_paths(
    config: &AppendLogStorageConfig,
) -> std::result::Result<(), RecorderStorageError> {
    let paths = [
        ("data", config.data_path.clone()),
        ("state", config.state_path.clone()),
        ("staging", config.state_path.with_extension("tmp")),
        ("state lock", recorder_state_lock_path(&config.state_path)),
    ];
    let mut observed = Vec::with_capacity(paths.len());
    for (role, path) in &paths {
        let canonical = resolve_recorder_path(path)?;
        let metadata = match recorder_parent(&canonical)
            .and_then(|(dir, name)| Ok(dir.metadata(name)?))
        {
            Ok(metadata) => Some(metadata),
            Err(RecorderStorageError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                None
            }
            Err(err) => return Err(err),
        };
        if let Some(metadata) = &metadata {
            if !metadata.is_file() {
                return Err(invalid_recorder_path(format!(
                    "recorder {role} path is not a regular file"
                )));
            }
        }
        let identity = metadata.as_ref().map(recorder_file_identity).transpose()?;
        for (prior_role, prior_path, prior_identity) in &observed {
            if &canonical == prior_path || identity.is_some() && identity == *prior_identity {
                return Err(invalid_recorder_path(format!(
                    "recorder {prior_role} and {role} paths identify the same file"
                )));
            }
        }
        // Mutable state/staging names must not alias an unenumerated file in
        // another configuration. The data descriptor may have read aliases.
        if *role != "data" {
            if let Some(metadata) = &metadata {
                if recorder_link_count(metadata)? != 1 {
                    return Err(invalid_recorder_path(format!(
                        "recorder {role} file must have exactly one hard link"
                    )));
                }
            }
        }
        if matches!(*role, "staging" | "state lock") && canonical != *path {
            return Err(invalid_recorder_path(format!(
                "recorder {role} path must not be a symlink"
            )));
        }
        observed.push((*role, canonical, identity));
    }
    Ok(())
}

#[derive(Debug)]
struct AppendLogStateFile {
    directory: CapDir,
    state_name: PathBuf,
    temporary_name: PathBuf,
    lock_name: PathBuf,
    lock: File,
}

impl AppendLogStateFile {
    fn open(path: &Path) -> std::result::Result<Self, RecorderStorageError> {
        let (directory, state_name) = recorder_parent(path)?;
        let temporary_name = state_name.with_extension("tmp");
        let lock_name = recorder_state_lock_path(&state_name);
        let mut options = recorder_open_options();
        options.create(true).read(true).write(true);
        let lock = directory.open_with(&lock_name, &options)?.into_std();
        let metadata = CapMetadata::from_file(&lock)?;
        if !metadata.is_file() || recorder_link_count(&metadata)? != 1 {
            return Err(invalid_recorder_path(
                "recorder state lock must be a uniquely linked regular file",
            ));
        }
        fs2::FileExt::try_lock_exclusive(&lock)?;
        let state_file = Self {
            directory,
            state_name,
            temporary_name,
            lock_name,
            lock,
        };
        state_file.check_lock()?;
        Ok(state_file)
    }

    fn check_lock(&self) -> std::result::Result<(), RecorderStorageError> {
        let named = self.directory.symlink_metadata(&self.lock_name)?;
        let held = CapMetadata::from_file(&self.lock)?;
        if !named.is_file()
            || recorder_link_count(&named)? != 1
            || recorder_file_identity(&named)? != recorder_file_identity(&held)?
        {
            return Err(std::io::Error::other("recorder state lock identity changed").into());
        }
        Ok(())
    }

    fn load(&self) -> std::result::Result<PersistedState, RecorderStorageError> {
        self.check_lock()?;
        let mut options = recorder_open_options();
        options.read(true);
        let mut file = match self.directory.open_with(&self.state_name, &options) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PersistedState::default());
            }
            Err(err) => return Err(err.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(invalid_recorder_path(
                "recorder state path is not a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Ok(PersistedState::default());
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn write(&self, state: &PersistedState) -> std::result::Result<(), RecorderStorageError> {
        self.check_lock()?;
        let bytes = serde_json::to_vec_pretty(state)?;
        let mut options = recorder_open_options();
        // Do not truncate until the actual no-follow descriptor is validated.
        options.create(true).write(true);
        let mut file = self.directory.open_with(&self.temporary_name, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || recorder_link_count(&metadata)? != 1 {
            return Err(std::io::Error::other(
                "recorder staging file must be a uniquely linked regular file",
            )
            .into());
        }
        file.set_len(0)?;
        file.write_all(&bytes)?;
        drop(file);
        self.check_lock()?;
        self.directory
            .rename(&self.temporary_name, &self.directory, &self.state_name)?;
        Ok(())
    }
}

fn scan_valid_prefix(file: &mut File) -> std::result::Result<ScanResult, RecorderStorageError> {
    file.seek(SeekFrom::Start(0))?;
    let file_len = file.metadata()?.len();

    let mut offset = 0u64;
    let mut records = 0u64;
    let mut latest_record_start = None;
    loop {
        if offset + 4 > file_len {
            break;
        }

        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let payload_len = u32::from_le_bytes(len_buf) as u64;
        let next_offset = offset + 4 + payload_len;

        if next_offset > file_len {
            break;
        }

        file.seek(SeekFrom::Current(
            i64::try_from(payload_len).unwrap_or(i64::MAX),
        ))?;
        latest_record_start = Some(offset);
        offset = next_offset;
        records += 1;
    }

    if offset < file_len {
        file.set_len(offset)?;
        file.sync_data()?;
    }

    file.seek(SeekFrom::End(0))?;
    Ok(ScanResult {
        valid_len: offset,
        valid_records: records,
        latest_record_start,
    })
}

// ---------------------------------------------------------------------------
// RecorderEventReader / RecorderEventCursor — backend-neutral event reading
// ---------------------------------------------------------------------------

/// Error during event cursor reading.
#[derive(Debug)]
pub enum EventCursorError {
    /// I/O or backend communication failure.
    Io(String),
    /// Data corruption or deserialization failure.
    Corrupt {
        offset: RecorderOffset,
        reason: String,
    },
    /// Backend is unavailable or shutting down.
    Unavailable(String),
}

impl std::fmt::Display for EventCursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg) => write!(f, "event cursor I/O: {msg}"),
            Self::Corrupt { offset, reason } => {
                write!(
                    f,
                    "corrupt at segment={} byte={} ordinal={}: {reason}",
                    offset.segment_id, offset.byte_offset, offset.ordinal
                )
            }
            Self::Unavailable(msg) => write!(f, "event source unavailable: {msg}"),
        }
    }
}

impl std::error::Error for EventCursorError {}

/// A single record from a backend-neutral event cursor.
#[derive(Debug, Clone)]
pub struct CursorRecord {
    pub event: RecorderEvent,
    pub offset: RecorderOffset,
}

/// Backend-neutral sequential reader of recorder events.
///
/// Implementations wrap a concrete storage backend (append-log file,
/// rusqlite, etc.) and expose a cursor-based iteration interface
/// that the indexing pipeline consumes without knowing the backend.
pub trait RecorderEventReader: Send + Sync {
    /// Open a cursor positioned at `from`. Events with ordinal >= `from.ordinal`
    /// will be yielded by the cursor.
    fn open_cursor(
        &self,
        from: RecorderOffset,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError>;

    /// Open a cursor at the very beginning of the event stream.
    fn open_cursor_from_start(
        &self,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        self.open_cursor(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        })
    }

    /// Open a cursor positioned at the first event with ordinal >= `target_ordinal`.
    ///
    /// Backends may override for efficient seek. The default opens from start
    /// and scans forward, yielding the target event on the first `next_batch`.
    fn open_cursor_at_ordinal(
        &self,
        target_ordinal: u64,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        // Default: use open_cursor with a synthetic offset. Backends that
        // support ordinal-based seeking (like AppendLogEventSource) should
        // override this for efficiency.
        self.open_cursor(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: target_ordinal,
        })
    }

    /// Return the current head offset (the next offset that would be written).
    fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError>;
}

/// Sequential cursor over recorder events.
///
/// Yields batches of [`CursorRecord`] (event + offset pairs). Advancing the
/// cursor is monotonic — calling `next_batch` moves the position forward.
pub trait RecorderEventCursor: Send {
    /// Read the next batch of up to `max` records. Returns an empty vec at EOF.
    fn next_batch(
        &mut self,
        max: usize,
    ) -> std::result::Result<Vec<CursorRecord>, EventCursorError>;

    /// Current cursor position (the offset of the *next* record to be read).
    fn current_offset(&self) -> RecorderOffset;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality,
        RecorderEventPayload, RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel,
        RecorderTextEncoding,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Condvar, Mutex as StdMutex};
    use tempfile::tempdir;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build recorder_storage test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Still attempt TLS cleanup after teardown fails, but never hide a
        // runtime failure behind otherwise successful recorder assertions.
        let teardown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
        if let Err(payload) = teardown {
            std::panic::resume_unwind(payload);
        }
        if let Err(payload) = cleanup {
            std::panic::resume_unwind(payload);
        }
    }

    #[derive(Clone, Copy)]
    enum RecorderBlockingTestOperation {
        Append,
        Checkpoint,
        Flush,
    }

    async fn install_blocking_test_hook(
        storage: &RecorderStorageInstance,
        operation: RecorderBlockingTestOperation,
        hook: RecorderBlockingTestHook,
    ) {
        match storage {
            RecorderStorageInstance::AppendLog(storage) => {
                let mut inner = storage.inner.lock().await;
                match operation {
                    RecorderBlockingTestOperation::Append => inner.append_test_hook = Some(hook),
                    RecorderBlockingTestOperation::Checkpoint => {
                        inner.checkpoint_test_hook = Some(hook);
                    }
                    RecorderBlockingTestOperation::Flush => inner.flush_test_hook = Some(hook),
                }
            }
            RecorderStorageInstance::Rusqlite(storage) => {
                let mut inner = storage.inner.lock().await;
                match operation {
                    RecorderBlockingTestOperation::Append => inner.append_test_hook = Some(hook),
                    RecorderBlockingTestOperation::Checkpoint => {
                        inner.checkpoint_test_hook = Some(hook);
                    }
                    RecorderBlockingTestOperation::Flush => inner.flush_test_hook = Some(hook),
                }
            }
        }
    }

    async fn clear_blocking_test_hook(
        storage: &RecorderStorageInstance,
        operation: RecorderBlockingTestOperation,
    ) {
        match storage {
            RecorderStorageInstance::AppendLog(storage) => {
                let mut inner = storage.inner.lock().await;
                match operation {
                    RecorderBlockingTestOperation::Append => inner.append_test_hook = None,
                    RecorderBlockingTestOperation::Checkpoint => {
                        inner.checkpoint_test_hook = None;
                    }
                    RecorderBlockingTestOperation::Flush => inner.flush_test_hook = None,
                }
            }
            RecorderStorageInstance::Rusqlite(storage) => {
                let mut inner = storage.inner.lock().await;
                match operation {
                    RecorderBlockingTestOperation::Append => inner.append_test_hook = None,
                    RecorderBlockingTestOperation::Checkpoint => {
                        inner.checkpoint_test_hook = None;
                    }
                    RecorderBlockingTestOperation::Flush => inner.flush_test_hook = None,
                }
            }
        }
    }

    async fn install_flush_test_hook(
        storage: &RecorderStorageInstance,
        hook: RecorderBlockingTestHook,
    ) {
        install_blocking_test_hook(storage, RecorderBlockingTestOperation::Flush, hook).await;
    }

    async fn clear_flush_test_hook(storage: &RecorderStorageInstance) {
        clear_blocking_test_hook(storage, RecorderBlockingTestOperation::Flush).await;
    }

    async fn wait_for_test_flag(flag: &AtomicBool, description: &'static str) {
        crate::runtime_async::timeout(std::time::Duration::from_secs(5), async {
            while !flag.load(Ordering::Acquire) {
                crate::runtime_async::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("timed out waiting for {description}: {error}"));
    }

    fn blocking_admission_depth(
        storage: &RecorderStorageInstance,
        operation: RecorderBlockingTestOperation,
    ) -> usize {
        match (storage, operation) {
            (
                RecorderStorageInstance::AppendLog(storage),
                RecorderBlockingTestOperation::Append,
            ) => storage.in_flight.load(Ordering::Acquire),
            (
                RecorderStorageInstance::AppendLog(storage),
                RecorderBlockingTestOperation::Checkpoint,
            ) => storage.checkpoint_in_flight.load(Ordering::Acquire),
            (RecorderStorageInstance::AppendLog(storage), RecorderBlockingTestOperation::Flush) => {
                storage.flush_in_flight.load(Ordering::Acquire)
            }
            (RecorderStorageInstance::Rusqlite(storage), RecorderBlockingTestOperation::Append) => {
                storage.in_flight.load(Ordering::Acquire)
            }
            (
                RecorderStorageInstance::Rusqlite(storage),
                RecorderBlockingTestOperation::Checkpoint,
            ) => storage.checkpoint_in_flight.load(Ordering::Acquire),
            (RecorderStorageInstance::Rusqlite(storage), RecorderBlockingTestOperation::Flush) => {
                storage.flush_in_flight.load(Ordering::Acquire)
            }
        }
    }

    async fn wait_for_blocking_admission_release(
        storage: &RecorderStorageInstance,
        operation: RecorderBlockingTestOperation,
        description: &'static str,
    ) {
        crate::runtime_async::timeout(std::time::Duration::from_secs(5), async {
            while blocking_admission_depth(storage, operation) != 0 {
                crate::runtime_async::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("timed out waiting for {description}: {error}"));
    }

    type BlockingReleaseGate = (StdMutex<bool>, Condvar);

    fn wait_for_blocking_test_release(
        gate: &BlockingReleaseGate,
        backend: RecorderBackendKind,
        operation: &'static str,
    ) {
        let released = gate.0.lock().unwrap();
        let (released, _) = gate
            .1
            .wait_timeout_while(released, std::time::Duration::from_secs(5), |released| {
                !*released
            })
            .unwrap();
        assert!(
            *released,
            "{backend} timed out waiting for the executor-side {operation} release"
        );
    }

    fn release_blocking_test_gate(gate: &BlockingReleaseGate) {
        let mut released = gate.0.lock().unwrap();
        *released = true;
        drop(released);
        gate.1.notify_all();
    }

    fn persisted_event_count(config: &RecorderStorageConfig) -> u64 {
        match config.backend {
            RecorderBackendKind::AppendLog => {
                let mut file = File::open(&config.append_log.data_path).unwrap();
                scan_valid_prefix(&mut file).unwrap().valid_records
            }
            RecorderBackendKind::Rusqlite => {
                let connection = Connection::open(&config.rusqlite.db_path).unwrap();
                let count = connection
                    .query_row("SELECT COUNT(*) FROM recorder_events", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap();
                u64::try_from(count).unwrap()
            }
            RecorderBackendKind::FrankenSqlite => {
                panic!("frankensqlite has no recorder storage implementation")
            }
        }
    }

    fn blocking_test_checkpoint(response: &AppendResponse) -> RecorderCheckpoint {
        RecorderCheckpoint {
            consumer: CheckpointConsumerId("blocking-mutation-consumer".to_string()),
            upto_offset: response.last_offset.clone(),
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            committed_at_ms: 1_700_000_000_500,
        }
    }

    fn sample_event(event_id: &str, pane_id: u64, sequence: u64, text: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: Some("corr-1".to_string()),
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: 1_700_000_000_000 + sequence,
            recorded_at_ms: 1_700_000_000_001 + sequence,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct StubRecorderStorage {
        backend: RecorderBackendKind,
    }

    impl RecorderStorage for StubRecorderStorage {
        fn backend_kind(&self) -> RecorderBackendKind {
            self.backend
        }

        fn append_batch(
            &self,
            _req: AppendRequest,
        ) -> impl std::future::Future<Output = std::result::Result<AppendResponse, RecorderStorageError>>
        {
            std::future::ready(Err(RecorderStorageError::BackendUnavailable {
                backend: self.backend,
                message: "append_batch unused in StubRecorderStorage".to_string(),
            }))
        }

        fn flush(
            &self,
            _mode: FlushMode,
        ) -> impl std::future::Future<Output = std::result::Result<FlushStats, RecorderStorageError>>
        {
            std::future::ready(Ok(FlushStats {
                backend: self.backend,
                flushed_at_ms: 0,
                latest_offset: None,
            }))
        }

        fn read_checkpoint(
            &self,
            _consumer: &CheckpointConsumerId,
        ) -> impl std::future::Future<
            Output = std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError>,
        > {
            std::future::ready(Ok(None))
        }

        fn commit_checkpoint(
            &self,
            _checkpoint: RecorderCheckpoint,
        ) -> impl std::future::Future<
            Output = std::result::Result<CheckpointCommitOutcome, RecorderStorageError>,
        > {
            std::future::ready(Ok(CheckpointCommitOutcome::NoopAlreadyAdvanced))
        }

        fn health(&self) -> impl std::future::Future<Output = RecorderStorageHealth> {
            std::future::ready(RecorderStorageHealth {
                backend: self.backend,
                degraded: false,
                queue_depth: 0,
                queue_capacity: 1,
                latest_offset: None,
                last_error: None,
            })
        }

        fn lag_metrics(
            &self,
        ) -> impl std::future::Future<
            Output = std::result::Result<RecorderStorageLag, RecorderStorageError>,
        > {
            std::future::ready(Ok(RecorderStorageLag {
                latest_offset: None,
                consumers: Vec::new(),
            }))
        }
    }

    fn test_config(path: &Path) -> AppendLogStorageConfig {
        AppendLogStorageConfig {
            data_path: path.join("events.log"),
            state_path: path.join("state.json"),
            queue_capacity: 4,
            max_batch_events: 16,
            max_batch_bytes: 128 * 1024,
            max_idempotency_entries: 8,
        }
    }

    fn recorder_test_config(path: &Path) -> RecorderStorageConfig {
        RecorderStorageConfig {
            backend: RecorderBackendKind::AppendLog,
            append_log: test_config(path),
            rusqlite: RusqliteStorageConfig {
                db_path: path.join("recorder.sqlite3"),
                queue_capacity: 4,
                max_batch_events: 256,
                max_batch_bytes: 128 * 1024,
                max_idempotency_entries: 8,
            },
        }
    }

    #[test]
    fn bootstrap_selects_append_log_backend() {
        let dir = tempdir().unwrap();
        let config = recorder_test_config(dir.path());
        let storage = bootstrap_recorder_storage(config).unwrap();
        assert_eq!(storage.backend_kind(), RecorderBackendKind::AppendLog);
    }

    #[test]
    fn bootstrap_selects_rusqlite_backend_with_default_settings() {
        let dir = tempdir().unwrap();
        let mut config = recorder_test_config(dir.path());
        config.backend = RecorderBackendKind::Rusqlite;
        config.rusqlite = RusqliteStorageConfig {
            db_path: dir.path().join("recorder-defaults.sqlite3"),
            ..RusqliteStorageConfig::default()
        };

        let storage = bootstrap_recorder_storage(config).unwrap();
        assert_eq!(storage.backend_kind(), RecorderBackendKind::Rusqlite);
    }

    #[test]
    fn bootstrap_rusqlite_ignores_append_log_validation() {
        let dir = tempdir().unwrap();
        let mut config = recorder_test_config(dir.path());
        config.backend = RecorderBackendKind::Rusqlite;
        config.append_log.queue_capacity = 0; // invalid for append-log

        let storage = bootstrap_recorder_storage(config).unwrap();
        assert_eq!(storage.backend_kind(), RecorderBackendKind::Rusqlite);
    }

    #[test]
    fn bootstrap_rejects_frankensqlite_before_creating_a_database() {
        let dir = tempdir().unwrap();
        let untouched_parent = dir.path().join("must-not-exist");
        let db_path = untouched_parent.join("recorder.sqlite3");
        let mut config = recorder_test_config(dir.path());
        config.backend = RecorderBackendKind::FrankenSqlite;
        config.rusqlite.db_path = db_path.clone();

        let expected = config.select_backend().unwrap_err();
        let high_level_config = crate::config::Config::from_toml_unvalidated(
            r#"
[storage]
recorder_backend = "frankensqlite"
"#,
        )
        .unwrap();
        assert_eq!(
            high_level_config.recorder_backend_selection().unwrap_err(),
            expected
        );
        assert_eq!(expected.requested(), RecorderBackendKind::FrankenSqlite);
        let error = bootstrap_recorder_storage(config).unwrap_err();
        assert!(matches!(
            error,
            RecorderStorageError::BackendSelection(actual) if actual == expected
        ));
        assert!(!untouched_parent.exists());
        assert!(!db_path.exists());
    }

    #[test]
    fn bootstrap_rejection_does_not_mutate_an_existing_database_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("existing.sqlite3");
        let sentinel = b"not-a-database-and-must-remain-byte-identical";
        std::fs::write(&db_path, sentinel).unwrap();

        let mut config = recorder_test_config(dir.path());
        config.backend = RecorderBackendKind::FrankenSqlite;
        config.rusqlite.db_path = db_path.clone();

        let error = bootstrap_recorder_storage(config).unwrap_err();
        assert!(matches!(
            error,
            RecorderStorageError::BackendSelection(selection)
                if selection.requested() == RecorderBackendKind::FrankenSqlite
        ));
        let after = std::fs::read(&db_path).unwrap();
        assert_eq!(after.as_slice(), sentinel);
        assert!(!db_path.with_extension("sqlite3-wal").exists());
        assert!(!db_path.with_extension("sqlite3-shm").exists());
    }

    #[test]
    fn append_log_slot_admission_is_atomic_at_capacity_one() {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.queue_capacity = 1;
        let storage = Arc::new(AppendLogRecorderStorage::open(cfg).unwrap());
        let attempts = 32usize;
        let start = Arc::new(Barrier::new(attempts));
        let release = Arc::new(Barrier::new(attempts));
        let successes = Arc::new(AtomicUsize::new(0));

        let handles = (0..attempts)
            .map(|_| {
                let storage = Arc::clone(&storage);
                let start = Arc::clone(&start);
                let release = Arc::clone(&release);
                let successes = Arc::clone(&successes);
                std::thread::spawn(move || {
                    start.wait();
                    let guard = storage.try_acquire_slot().ok();
                    if guard.is_some() {
                        successes.fetch_add(1, Ordering::AcqRel);
                    }
                    release.wait();
                    drop(guard);
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(successes.load(Ordering::Acquire), 1);
        assert_eq!(storage.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn rusqlite_slot_admission_is_atomic_at_capacity_one() {
        let dir = tempdir().unwrap();
        let cfg = RusqliteStorageConfig {
            db_path: dir.path().join("recorder.sqlite3"),
            queue_capacity: 1,
            max_batch_events: 16,
            max_batch_bytes: 128 * 1024,
            max_idempotency_entries: 8,
        };
        let storage = Arc::new(RusqliteRecorderStorage::open(cfg).unwrap());
        let attempts = 32usize;
        let start = Arc::new(Barrier::new(attempts));
        let release = Arc::new(Barrier::new(attempts));
        let successes = Arc::new(AtomicUsize::new(0));

        let handles = (0..attempts)
            .map(|_| {
                let storage = Arc::clone(&storage);
                let start = Arc::clone(&start);
                let release = Arc::clone(&release);
                let successes = Arc::clone(&successes);
                std::thread::spawn(move || {
                    start.wait();
                    let guard = storage.try_acquire_slot().ok();
                    if guard.is_some() {
                        successes.fetch_add(1, Ordering::AcqRel);
                    }
                    release.wait();
                    drop(guard);
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(successes.load(Ordering::Acquire), 1);
        assert_eq!(storage.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn recorder_storage_config_serde_roundtrip() {
        let dir = tempdir().unwrap();
        let config = recorder_test_config(dir.path());
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"rusqlite\""));
        assert!(!json.contains("\"frankensqlite\""));
        let back: RecorderStorageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.backend, RecorderBackendKind::AppendLog);
        assert_eq!(
            back.append_log.queue_capacity,
            config.append_log.queue_capacity
        );
        assert_eq!(
            back.append_log.max_batch_events,
            config.append_log.max_batch_events
        );
        assert_eq!(back.rusqlite.db_path, config.rusqlite.db_path);
    }

    #[test]
    fn recorder_storage_config_rejects_legacy_franken_sqlite_names() {
        let value = serde_json::json!({
            "backend": "franken_sqlite",
            "append_log": AppendLogStorageConfig::default(),
            "rusqlite": RusqliteStorageConfig::default()
        });
        assert!(serde_json::from_value::<RecorderStorageConfig>(value).is_err());

        let value = serde_json::json!({
            "backend": "rusqlite",
            "append_log": AppendLogStorageConfig::default(),
            "frankensqlite": RusqliteStorageConfig::default()
        });
        assert!(serde_json::from_value::<RecorderStorageConfig>(value).is_err());
    }

    #[test]
    fn bounded_slot_admission_rejects_second_live_guard() {
        let counter = AtomicUsize::new(0);
        let first = try_acquire_bounded_slot(&counter, 1).unwrap();

        let second = try_acquire_bounded_slot(&counter, 1);
        assert!(matches!(
            second,
            Err(RecorderStorageError::QueueFull { capacity: 1 })
        ));
        assert_eq!(counter.load(Ordering::Acquire), 1);

        drop(first);
        assert_eq!(counter.load(Ordering::Acquire), 0);
        let reacquired = try_acquire_bounded_slot(&counter, 1).unwrap();
        assert_eq!(counter.load(Ordering::Acquire), 1);
        drop(reacquired);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn bounded_slot_admission_stays_at_capacity_one_under_contention() {
        let attempts = 32;
        let counter = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(attempts));
        let attempted = Arc::new(Barrier::new(attempts));
        let successes = Arc::new(AtomicUsize::new(0));
        let queue_full = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..attempts {
                let counter = Arc::clone(&counter);
                let start = Arc::clone(&start);
                let attempted = Arc::clone(&attempted);
                let successes = Arc::clone(&successes);
                let queue_full = Arc::clone(&queue_full);

                scope.spawn(move || {
                    start.wait();
                    let acquired = match try_acquire_bounded_slot(&counter, 1) {
                        Ok(guard) => {
                            successes.fetch_add(1, Ordering::AcqRel);
                            Some(guard)
                        }
                        Err(RecorderStorageError::QueueFull { capacity: 1 }) => {
                            queue_full.fetch_add(1, Ordering::AcqRel);
                            None
                        }
                        Err(err) => panic!("unexpected bounded-slot admission error: {err}"),
                    };
                    attempted.wait();
                    drop(acquired);
                });
            }
        });

        assert_eq!(successes.load(Ordering::Acquire), 1);
        assert_eq!(queue_full.load(Ordering::Acquire), attempts - 1);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn concrete_storage_slot_admission_guards_share_bounded_helper() {
        let append_dir = tempdir().unwrap();
        let mut append_config = test_config(append_dir.path());
        append_config.queue_capacity = 1;
        let append_storage = AppendLogRecorderStorage::open(append_config).unwrap();
        let append_first = append_storage.try_acquire_slot().unwrap();
        assert!(matches!(
            append_storage.try_acquire_slot(),
            Err(RecorderStorageError::QueueFull { capacity: 1 })
        ));
        drop(append_first);

        let sqlite_dir = tempdir().unwrap();
        let mut sqlite_config = recorder_test_config(sqlite_dir.path()).rusqlite;
        sqlite_config.queue_capacity = 1;
        let sqlite_storage = RusqliteRecorderStorage::open(sqlite_config).unwrap();
        let sqlite_first = sqlite_storage.try_acquire_slot().unwrap();
        assert!(matches!(
            sqlite_storage.try_acquire_slot(),
            Err(RecorderStorageError::QueueFull { capacity: 1 })
        ));
        drop(sqlite_first);
    }

    #[test]
    fn latest_offsets_identify_last_record_before_and_after_reopen() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                let mut storage = bootstrap_recorder_storage(config.clone()).unwrap();
                assert_eq!(storage.health().await.latest_offset, None);
                assert_eq!(storage.lag_metrics().await.unwrap().latest_offset, None);
                assert_eq!(
                    storage
                        .flush(FlushMode::Durable)
                        .await
                        .unwrap()
                        .latest_offset,
                    None
                );

                for batch in 0..3 {
                    let response = storage
                        .append_batch(AppendRequest {
                            batch_id: format!("latest-{batch}"),
                            events: vec![
                                sample_event(&format!("first-{batch}"), 7, batch * 2, "界面"),
                                sample_event(
                                    &format!("last-{batch}"),
                                    7,
                                    batch * 2 + 1,
                                    "longer 🚀 payload",
                                ),
                            ],
                            required_durability: DurabilityLevel::Fsync,
                            producer_ts_ms: batch,
                        })
                        .await
                        .unwrap();
                    let expected = Some(response.last_offset);
                    assert_eq!(
                        storage
                            .flush(FlushMode::Durable)
                            .await
                            .unwrap()
                            .latest_offset,
                        expected
                    );
                    assert_eq!(storage.health().await.latest_offset, expected);
                    assert_eq!(storage.lag_metrics().await.unwrap().latest_offset, expected);
                    drop(storage);

                    storage = bootstrap_recorder_storage(config.clone()).unwrap();
                    assert_eq!(
                        storage
                            .flush(FlushMode::Durable)
                            .await
                            .unwrap()
                            .latest_offset,
                        expected
                    );
                    assert_eq!(storage.health().await.latest_offset, expected);
                    assert_eq!(storage.lag_metrics().await.unwrap().latest_offset, expected);
                }
            }
        });
    }

    #[test]
    fn rusqlite_durability_policy_delete_roundtrip() {
        run_async_test(rusqlite_durability_policy_roundtrip(false));
    }

    #[test]
    fn rusqlite_durability_policy_wal_roundtrip() {
        run_async_test(rusqlite_durability_policy_roundtrip(true));
    }

    async fn rusqlite_durability_policy_roundtrip(wal: bool) {
        let dir = tempdir().unwrap();
        let config = recorder_test_config(dir.path()).rusqlite;
        let expected_mode = if wal { "wal" } else { "delete" };
        if wal {
            let seed = Connection::open(&config.db_path).unwrap();
            let mode: String = seed
                .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            seed.execute_batch("PRAGMA synchronous = NORMAL; CREATE TABLE seed (id INTEGER);")
                .unwrap();
        }

        for ordinal in 0..2 {
            let storage = RusqliteRecorderStorage::open(config.clone()).unwrap();
            {
                let inner = storage.inner.lock().await;
                let mode: String = inner
                    .conn
                    .query_row("PRAGMA main.journal_mode", [], |row| row.get(0))
                    .unwrap();
                let synchronous: i64 = inner
                    .conn
                    .query_row("PRAGMA main.synchronous", [], |row| row.get(0))
                    .unwrap();
                let fullfsync: i64 = inner
                    .conn
                    .query_row("PRAGMA fullfsync", [], |row| row.get(0))
                    .unwrap();
                eprintln!(
                    "recorder durability open: mode={mode}, reopen={ordinal}, synchronous={synchronous}, fullfsync={fullfsync}"
                );
                assert_eq!(mode, expected_mode, "opening must preserve journal mode");
                assert_eq!(
                    synchronous, 3,
                    "persistent recorder must explicitly use EXTRA"
                );
                assert_eq!(fullfsync, 1, "request fullfsync on supporting platforms");
            }
            let response = storage
                .append_batch(AppendRequest {
                    batch_id: format!("durability-{ordinal}"),
                    events: vec![sample_event(
                        &format!("durability-{ordinal}"),
                        7,
                        ordinal,
                        "payload",
                    )],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: ordinal,
                })
                .await
                .unwrap();
            assert_eq!(response.backend, RecorderBackendKind::Rusqlite);
            assert_eq!(response.accepted_count, 1);
            assert_eq!(response.committed_durability, DurabilityLevel::Fsync);
            assert_eq!(response.last_offset.ordinal, ordinal);
            let checkpoint = RecorderCheckpoint {
                consumer: CheckpointConsumerId("durability-reader".to_string()),
                upto_offset: response.last_offset.clone(),
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                committed_at_ms: ordinal,
            };
            assert_eq!(
                storage.commit_checkpoint(checkpoint).await.unwrap(),
                CheckpointCommitOutcome::Advanced
            );
            let flushed = storage.flush(FlushMode::Durable).await.unwrap();
            assert_eq!(flushed.backend, RecorderBackendKind::Rusqlite);
            assert_eq!(flushed.latest_offset, Some(response.last_offset));
            drop(storage);

            let reader = Connection::open_with_flags(
                &config.db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: i64 = reader
                .query_row("SELECT COUNT(*) FROM recorder_events", [], |row| row.get(0))
                .unwrap();
            let checkpoint: i64 = reader
                .query_row(
                    "SELECT ordinal FROM recorder_checkpoints WHERE consumer = 'durability-reader'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let integrity: String = reader
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, i64::try_from(ordinal + 1).unwrap());
            assert_eq!(checkpoint, i64::try_from(ordinal).unwrap());
            assert_eq!(integrity, "ok");
            eprintln!(
                "recorder durability reopen: mode={expected_mode}, events={count}, checkpoint={checkpoint}, integrity={integrity}"
            );
        }
    }

    #[test]
    fn rusqlite_durability_policy_rejects_volatile_public_storage() {
        let config = RusqliteStorageConfig {
            db_path: PathBuf::from(":memory:"),
            ..RusqliteStorageConfig::default()
        };
        let error = RusqliteRecorderStorage::open(config)
            .expect_err("a volatile database cannot acknowledge persistent recorder writes");
        assert!(
            matches!(error, RecorderStorageError::InvalidRequest { ref message }
            if message.contains("persistent journal")),
            "{error}"
        );
    }

    #[test]
    fn rusqlite_durability_policy_normalizes_weak_connections_without_changing_journal() {
        for mode in ["DELETE", "TRUNCATE", "PERSIST", "WAL"] {
            let dir = tempdir().unwrap();
            let conn = Connection::open(dir.path().join("policy.sqlite3")).unwrap();
            let actual: String = conn
                .query_row(&format!("PRAGMA journal_mode = {mode}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(actual, mode.to_ascii_lowercase());
            conn.execute_batch("PRAGMA synchronous = OFF; PRAGMA fullfsync = OFF;")
                .unwrap();
            configure_rusqlite_durability(&conn).unwrap();
            let settings: (String, i64, i64) = conn.query_row(
                "SELECT journal_mode, synchronous, fullfsync FROM pragma_journal_mode, pragma_synchronous, pragma_fullfsync",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(settings, (actual, 3, 1));
            initialize_rusqlite_schema(&conn).unwrap();
            eprintln!("recorder policy normalized: mode={mode}, synchronous=3, fullfsync=1");
        }
    }

    #[test]
    fn rusqlite_durability_policy_rejects_unsafe_journals_before_schema_changes() {
        for mode in ["MEMORY", "OFF"] {
            let dir = tempdir().unwrap();
            let conn = Connection::open(dir.path().join("unsafe.sqlite3")).unwrap();
            let actual: String = conn
                .query_row(&format!("PRAGMA journal_mode = {mode}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(actual, mode.to_ascii_lowercase());
            let error = configure_rusqlite_durability(&conn).unwrap_err();
            assert!(matches!(error, RecorderStorageError::InvalidRequest { .. }));
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                count, 0,
                "reject unsafe storage before recorder schema writes"
            );
            eprintln!("recorder unsafe journal rejected: mode={mode}, schema_objects={count}");
        }
        let temporary = Connection::open("").unwrap();
        assert!(matches!(
            configure_rusqlite_durability(&temporary),
            Err(RecorderStorageError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn rusqlite_checkpoint_contention_never_rewinds_delete() {
        rusqlite_checkpoint_contention_never_rewinds("DELETE");
    }

    #[test]
    fn rusqlite_checkpoint_contention_never_rewinds_wal() {
        rusqlite_checkpoint_contention_never_rewinds("WAL");
    }

    fn rusqlite_checkpoint_contention_never_rewinds(mode: &str) {
        use std::cell::RefCell;
        use std::sync::mpsc::{Receiver, Sender, channel};
        use std::time::Duration;

        // Only SQLite's real lock-contention callback releases the coordinator.
        // TLS keeps concurrent test cases independent; no callback re-enters SQL.
        thread_local! {
            static BUSY_GATE: RefCell<Option<(Sender<()>, Receiver<()>)>> = const { RefCell::new(None) };
        }
        for iteration in 0..10 {
            let dir = tempdir().unwrap();
            let config = recorder_test_config(dir.path()).rusqlite;
            let mut blocker = Connection::open(&config.db_path).unwrap();
            let actual_mode: String = blocker
                .query_row(&format!("PRAGMA journal_mode = {mode}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(actual_mode, mode.to_ascii_lowercase());
            let storage = RusqliteRecorderStorage::open(config).unwrap();
            let initial = RecorderCheckpoint {
                consumer: CheckpointConsumerId("contended-reader".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                committed_at_ms: 0,
            };
            run_async_test(async {
                storage
                    .append_batch(AppendRequest {
                        batch_id: "checkpoint-contention".to_string(),
                        events: (0..3)
                            .map(|ordinal| {
                                sample_event(&format!("event-{ordinal}"), 7, ordinal, "data")
                            })
                            .collect(),
                        required_durability: DurabilityLevel::Fsync,
                        producer_ts_ms: 0,
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    storage.commit_checkpoint(initial.clone()).await.unwrap(),
                    CheckpointCommitOutcome::Advanced
                );
            });
            let mut newer = initial.clone();
            newer.upto_offset.ordinal = 2;
            newer.committed_at_ms = 2;
            let mut delayed = initial;
            delayed.upto_offset.ordinal = 1;
            delayed.committed_at_ms = 1;

            let transaction = blocker
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            assert_eq!(transaction.execute(
                "UPDATE recorder_checkpoints SET ordinal = 2, committed_at_ms = 2 WHERE consumer = ?1",
                [&newer.consumer.0],
            ).unwrap(), 1);
            let (busy_tx, busy_rx) = channel();
            let (resume_tx, resume_rx) = channel();
            std::thread::scope(|scope| {
                let writer = scope.spawn(move || {
                    let busy_gate = Arc::new(StdMutex::new(Some((busy_tx, resume_rx))));
                    run_async_test(async {
                        {
                            let mut inner = storage.inner.lock().await;
                            inner.conn.busy_handler(Some(|_| {
                                BUSY_GATE.with(|gate| {
                                    let Some((entered, resume)) = gate.borrow_mut().take() else {
                                        return false;
                                    };
                                    entered.send(()).is_ok()
                                        && resume.recv_timeout(Duration::from_secs(10)).is_ok()
                                })
                            })).unwrap();
                            inner.checkpoint_test_hook = Some(RecorderBlockingTestHook {
                                before: {
                                    let busy_gate = Arc::clone(&busy_gate);
                                    Arc::new(move || {
                                        let pair = busy_gate
                                            .lock()
                                            .unwrap()
                                            .take()
                                            .expect("checkpoint busy gate installed once");
                                        BUSY_GATE.with(|gate| *gate.borrow_mut() = Some(pair));
                                    })
                                },
                                after: Arc::new(|| {
                                    BUSY_GATE.with(|gate| *gate.borrow_mut() = None);
                                }),
                            });
                        }
                        let result = storage.commit_checkpoint(delayed).await;
                        storage.inner.lock().await.checkpoint_test_hook = None;
                        let persisted = storage.read_checkpoint(&newer.consumer).await.unwrap();
                        let outcome = match &result {
                            Err(RecorderStorageError::CheckpointRegression { .. }) => "regression",
                            Ok(CheckpointCommitOutcome::Advanced) => "advanced",
                            _ => "unexpected",
                        };
                        eprintln!("recorder checkpoint contention: mode={mode}, iteration={iteration}, outcome={outcome}, persisted_ordinal={:?}", persisted.as_ref().map(|cp| cp.upto_offset.ordinal));
                        assert_eq!(persisted, Some(newer.clone()), "delayed writer must not overwrite newer checkpoint");
                        assert!(matches!(result, Err(RecorderStorageError::CheckpointRegression {
                            current_ordinal: 2, attempted_ordinal: 1, ..
                        })));
                        assert_eq!(storage.commit_checkpoint(newer).await.unwrap(), CheckpointCommitOutcome::NoopAlreadyAdvanced);
                        assert!(!storage.health().await.degraded);
                    });
                });
                busy_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("writer must reach actual SQLite lock contention");
                transaction.commit().unwrap();
                resume_tx.send(()).unwrap();
                writer.join().unwrap();
            });
        }
    }

    #[test]
    fn rusqlite_append_reader_seek_head_and_checkpoints() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let config = recorder_test_config(dir.path()).rusqlite;
            let storage = RusqliteRecorderStorage::open(config).unwrap();
            let events = vec![
                sample_event("sql-0", 7, 0, "zero"),
                sample_event("sql-1", 7, 1, "one"),
                sample_event("sql-2", 9, 2, "two"),
            ];

            let response = storage
                .append_batch(AppendRequest {
                    batch_id: "sqlite-batch".to_string(),
                    events: events.clone(),
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            assert_eq!(response.backend, RecorderBackendKind::Rusqlite);
            assert_eq!(response.accepted_count, 3);
            assert_eq!(response.first_offset.ordinal, 0);
            assert_eq!(response.last_offset.ordinal, 2);
            assert!(!response.was_idempotent_replay);

            let idempotent = storage
                .append_batch(AppendRequest {
                    batch_id: "sqlite-batch".to_string(),
                    events,
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            assert_eq!(idempotent.accepted_count, 3);
            assert_eq!(idempotent.last_offset.ordinal, 2);
            assert_eq!(idempotent.committed_durability, DurabilityLevel::Fsync);
            assert!(idempotent.was_idempotent_replay);

            let reader = storage.event_reader();
            let head = reader.head_offset().unwrap();
            assert_eq!(head.ordinal, 3);
            assert!(head.byte_offset > response.last_offset.byte_offset);

            let mut cursor = reader.open_cursor_at_ordinal(1).unwrap();
            assert_eq!(cursor.current_offset().ordinal, 1);
            let batch = cursor.next_batch(8).unwrap();
            assert_eq!(batch.len(), 2);
            assert_eq!(batch[0].event.event_id, "sql-1");
            assert_eq!(batch[0].offset.ordinal, 1);
            assert_eq!(batch[1].event.event_id, "sql-2");
            assert_eq!(cursor.current_offset().ordinal, 3);
            assert!(cursor.next_batch(8).unwrap().is_empty());

            let checkpoint = RecorderCheckpoint {
                consumer: CheckpointConsumerId("sqlite-indexer".to_string()),
                upto_offset: batch[0].offset.clone(),
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                committed_at_ms: crate::recording::epoch_ms_now(),
            };
            let outcome = storage.commit_checkpoint(checkpoint.clone()).await.unwrap();
            assert_eq!(outcome, CheckpointCommitOutcome::Advanced);
            let reread = storage
                .read_checkpoint(&CheckpointConsumerId("sqlite-indexer".to_string()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reread.upto_offset.ordinal, 1);

            let regression = RecorderCheckpoint {
                upto_offset: response.first_offset,
                ..checkpoint
            };
            let err = storage.commit_checkpoint(regression).await.unwrap_err();
            assert!(matches!(
                err,
                RecorderStorageError::CheckpointRegression { .. }
            ));

            let lag = storage.lag_metrics().await.unwrap();
            assert_eq!(lag.latest_offset.unwrap().ordinal, 2);
            assert_eq!(lag.consumers[0].offsets_behind, 1);
        });
    }

    #[test]
    fn rusqlite_rejects_cached_response_from_a_different_backend_without_inserting_events() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let config = recorder_test_config(dir.path()).rusqlite;
            let storage = RusqliteRecorderStorage::open(config).unwrap();
            let poisoned = AppendResponse {
                backend: RecorderBackendKind::FrankenSqlite,
                accepted_count: 1,
                first_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                last_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                committed_durability: DurabilityLevel::Appended,
                committed_at_ms: 1,
                was_idempotent_replay: false,
            };
            let poisoned = CachedAppendReceipt {
                request_digest_sha256: "0".repeat(64),
                response: poisoned,
            };
            {
                let inner = storage.inner.lock().await;
                inner
                    .conn
                    .execute(
                        "INSERT INTO recorder_batches (batch_id, response_json, committed_at_ms)
                         VALUES (?1, ?2, ?3)",
                        params![
                            "poisoned-backend",
                            serde_json::to_string(&poisoned).unwrap(),
                            1_i64,
                        ],
                    )
                    .unwrap();
            }

            let error = storage
                .append_batch(AppendRequest {
                    batch_id: "poisoned-backend".to_string(),
                    events: vec![sample_event("must-not-insert", 7, 0, "payload")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                &error,
                RecorderStorageError::CorruptCachedResponse {
                    batch_id,
                    expected_backend: RecorderBackendKind::Rusqlite,
                    actual_backend: RecorderBackendKind::FrankenSqlite,
                } if batch_id == "poisoned-backend"
            ));
            assert_eq!(error.class(), RecorderStorageErrorClass::Corruption);

            let inner = storage.inner.lock().await;
            let event_count: i64 = inner
                .conn
                .query_row("SELECT COUNT(*) FROM recorder_events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(event_count, 0);
        });
    }

    #[test]
    fn rusqlite_rejects_persisted_replay_receipt_without_inserting_events() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage =
                RusqliteRecorderStorage::open(recorder_test_config(dir.path()).rusqlite).unwrap();
            let event = sample_event("must-not-insert", 7, 0, "payload");
            let payload = serde_json::to_string(&event).unwrap();
            let poisoned = CachedAppendReceipt {
                request_digest_sha256: digest_serialized_event_payloads([payload.as_bytes()]),
                response: AppendResponse {
                    backend: RecorderBackendKind::Rusqlite,
                    accepted_count: 1,
                    first_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    },
                    last_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    },
                    committed_durability: DurabilityLevel::Appended,
                    committed_at_ms: 1,
                    was_idempotent_replay: true,
                },
            };
            {
                let inner = storage.inner.lock().await;
                inner
                    .conn
                    .execute(
                        "INSERT INTO recorder_batches (batch_id, response_json, committed_at_ms)
                         VALUES (?1, ?2, ?3)",
                        params![
                            "poisoned-replay",
                            serde_json::to_string(&poisoned).unwrap(),
                            1_i64,
                        ],
                    )
                    .unwrap();
            }

            let error = storage
                .append_batch(AppendRequest {
                    batch_id: "poisoned-replay".to_string(),
                    events: vec![event],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                RecorderStorageError::CorruptCachedReplayReceipt { ref batch_id }
                    if batch_id == "poisoned-replay"
            ));
            let inner = storage.inner.lock().await;
            let event_count: i64 = inner
                .conn
                .query_row("SELECT COUNT(*) FROM recorder_events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(event_count, 0);
        });
    }

    #[test]
    fn rusqlite_reads_legacy_bare_receipt_and_verifies_committed_content() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage =
                RusqliteRecorderStorage::open(recorder_test_config(dir.path()).rusqlite).unwrap();
            let event = sample_event("legacy-receipt-event", 7, 0, "payload");
            let first = storage
                .append_batch(AppendRequest {
                    batch_id: "legacy-receipt".to_string(),
                    events: vec![event.clone()],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            {
                let inner = storage.inner.lock().await;
                let mut legacy = serde_json::to_value(&first).unwrap();
                legacy
                    .as_object_mut()
                    .unwrap()
                    .remove("was_idempotent_replay");
                inner
                    .conn
                    .execute(
                        "UPDATE recorder_batches SET response_json = ?2 WHERE batch_id = ?1",
                        params!["legacy-receipt", serde_json::to_string(&legacy).unwrap()],
                    )
                    .unwrap();
            }

            let replay = storage
                .append_batch(AppendRequest {
                    batch_id: "legacy-receipt".to_string(),
                    events: vec![event],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            assert!(replay.was_idempotent_replay);
            assert_eq!(replay.first_offset, first.first_offset);
            assert_eq!(replay.last_offset, first.last_offset);
            let mut cursor = storage.event_reader().open_cursor_from_start().unwrap();
            assert_eq!(cursor.next_batch(8).unwrap().len(), 1);
        });
    }

    #[test]
    fn rusqlite_classifies_malformed_persisted_receipt_as_corruption() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage =
                RusqliteRecorderStorage::open(recorder_test_config(dir.path()).rusqlite).unwrap();
            let event = sample_event("malformed-receipt-event", 7, 0, "payload");
            storage
                .append_batch(AppendRequest {
                    batch_id: "malformed-receipt".to_string(),
                    events: vec![event.clone()],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            {
                let inner = storage.inner.lock().await;
                inner
                    .conn
                    .execute(
                        "UPDATE recorder_batches SET response_json = '{' WHERE batch_id = ?1",
                        params!["malformed-receipt"],
                    )
                    .unwrap();
            }

            let error = storage
                .append_batch(AppendRequest {
                    batch_id: "malformed-receipt".to_string(),
                    events: vec![event],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                RecorderStorageError::CorruptCachedReceiptEncoding { ref batch_id, .. }
                    if batch_id == "malformed-receipt"
            ));
            assert_eq!(error.class(), RecorderStorageErrorClass::Corruption);
            assert!(storage.health().await.degraded);
            let mut cursor = storage.event_reader().open_cursor_from_start().unwrap();
            assert_eq!(cursor.next_batch(8).unwrap().len(), 1);
        });
    }

    #[test]
    fn rusqlite_receipt_eviction_uses_insertion_order_when_timestamps_tie() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_rusqlite_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO recorder_batches (batch_id, response_json, committed_at_ms)
             VALUES ('z-old', '{}', 7), ('a-new', '{}', 7)",
            [],
        )
        .unwrap();

        evict_rusqlite_receipts(&conn, 1).unwrap();

        let remaining = conn
            .query_row("SELECT batch_id FROM recorder_batches", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        assert_eq!(remaining, "a-new");
    }

    #[test]
    fn rusqlite_evicted_batch_id_can_be_reused_and_replayed_without_old_rows_leaking() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let mut config = recorder_test_config(dir.path()).rusqlite;
            config.max_idempotency_entries = 1;
            let storage = RusqliteRecorderStorage::open(config).unwrap();

            let same = sample_event("same-old", 1, 0, "same-content");
            storage
                .append_batch(AppendRequest {
                    batch_id: "reuse-same".to_string(),
                    events: vec![same.clone()],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            storage
                .append_batch(AppendRequest {
                    batch_id: "evict-same".to_string(),
                    events: vec![sample_event("evict-same", 1, 1, "evict")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            let same_reused = storage
                .append_batch(AppendRequest {
                    batch_id: "reuse-same".to_string(),
                    events: vec![same.clone()],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 3,
                })
                .await
                .unwrap();
            assert!(!same_reused.was_idempotent_replay);
            let same_replay = storage
                .append_batch(AppendRequest {
                    batch_id: "reuse-same".to_string(),
                    events: vec![same],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 4,
                })
                .await
                .unwrap();
            assert!(same_replay.was_idempotent_replay);
            assert_eq!(same_replay.first_offset, same_reused.first_offset);

            storage
                .append_batch(AppendRequest {
                    batch_id: "reuse-changed".to_string(),
                    events: vec![sample_event("changed-old", 1, 3, "old-content")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 5,
                })
                .await
                .unwrap();
            storage
                .append_batch(AppendRequest {
                    batch_id: "evict-changed".to_string(),
                    events: vec![sample_event("evict-changed", 1, 4, "evict")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 6,
                })
                .await
                .unwrap();
            let changed = sample_event("changed-new", 1, 5, "new-content");
            let changed_reused = storage
                .append_batch(AppendRequest {
                    batch_id: "reuse-changed".to_string(),
                    events: vec![changed.clone()],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 7,
                })
                .await
                .unwrap();
            assert!(!changed_reused.was_idempotent_replay);
            let changed_replay = storage
                .append_batch(AppendRequest {
                    batch_id: "reuse-changed".to_string(),
                    events: vec![changed],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 8,
                })
                .await
                .unwrap();
            assert!(changed_replay.was_idempotent_replay);
            assert_eq!(changed_replay.first_offset, changed_reused.first_offset);

            let mut cursor = storage.event_reader().open_cursor_from_start().unwrap();
            assert_eq!(cursor.next_batch(16).unwrap().len(), 6);
        });
    }

    #[test]
    fn rusqlite_reader_missing_path_fails_without_creating_database() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("missing-recorder.sqlite3");
        assert!(!db_path.exists());

        let reader = RusqliteEventReader::new(db_path.clone());
        let error = reader
            .open_cursor_from_start()
            .err()
            .expect("missing source path must be rejected");

        assert!(matches!(error, EventCursorError::Unavailable(_)));
        assert!(
            !db_path.exists(),
            "read-only recorder source discovery must never create a missing database"
        );
    }

    #[test]
    fn rusqlite_reader_rejects_database_without_required_schema() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("invalid-recorder.sqlite3");
        Connection::open(&db_path).unwrap();

        let reader = RusqliteEventReader::new(db_path.clone());
        let error = reader.head_offset().unwrap_err();

        assert!(matches!(error, EventCursorError::Unavailable(_)));
        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0, "reader must not initialize missing schema");
    }

    #[test]
    fn rusqlite_reader_reports_corrupt_payload_row() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("recorder.sqlite3");
        let conn = Connection::open(&db_path).unwrap();
        initialize_rusqlite_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO recorder_events (
                 ordinal, segment_id, byte_offset, payload_json, payload_bytes,
                 event_id, pane_id, schema_version, recorded_at_ms, batch_id, inserted_at_ms
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                0_i64,
                0_i64,
                0_i64,
                "{not-json",
                9_i64,
                "bad-event",
                1_i64,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                1_i64,
                "bad-batch",
                1_i64,
            ],
        )
        .unwrap();
        drop(conn);

        let reader = RusqliteEventReader::new(db_path);
        let mut cursor = reader.open_cursor_from_start().unwrap();
        let err = cursor.next_batch(1).unwrap_err();
        assert!(matches!(
            err,
            EventCursorError::Corrupt {
                offset: RecorderOffset { ordinal: 0, .. },
                ..
            }
        ));
    }

    #[test]
    fn backend_kind_display_is_snake_case() {
        assert_eq!(
            RecorderBackendKind::AppendLog.to_string(),
            "append_log".to_string()
        );
        assert_eq!(
            RecorderBackendKind::Rusqlite.to_string(),
            "rusqlite".to_string()
        );
        assert_eq!(
            RecorderBackendKind::FrankenSqlite.to_string(),
            "frankensqlite".to_string()
        );
    }

    #[test]
    fn append_assigns_monotonic_offsets() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let r1 = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![
                        sample_event("e1", 1, 0, "first"),
                        sample_event("e2", 1, 1, "second"),
                    ],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let r2 = storage
                .append_batch(AppendRequest {
                    batch_id: "b2".to_string(),
                    events: vec![sample_event("e3", 2, 2, "third")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();

            assert_eq!(r1.first_offset.ordinal, 0);
            assert_eq!(r1.last_offset.ordinal, 1);
            assert_eq!(r2.first_offset.ordinal, 2);
            assert!(r2.first_offset.byte_offset > r1.first_offset.byte_offset);
            assert_eq!(r2.last_offset.ordinal, 2);
        });
    }

    /// The concrete Cx-first append path and legacy append path share the same
    /// owned blocking mutation body. For a live Cx they must retain one ordered
    /// offset stream and otherwise remain observationally equivalent.
    #[test]
    fn trait_append_batch_with_cx_matches_legacy() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let cx = crate::cx::for_request();

            let cx_response = storage
                .append_batch_with_cx(
                    &cx,
                    AppendRequest {
                        batch_id: "cx-b1".to_string(),
                        events: vec![sample_event("cx-e1", 1, 0, "first")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: 1,
                    },
                )
                .await
                .unwrap();

            let legacy_response = storage
                .append_batch(AppendRequest {
                    batch_id: "legacy-b1".to_string(),
                    events: vec![sample_event("legacy-e1", 1, 1, "second")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();

            assert_eq!(cx_response.first_offset.ordinal, 0);
            assert_eq!(cx_response.last_offset.ordinal, 0);
            assert_eq!(legacy_response.first_offset.ordinal, 1);
            assert_eq!(legacy_response.last_offset.ordinal, 1);
            // Byte offsets are monotonically increasing across
            // both cx and legacy append paths — proves the
            // Cx-first owned dispatch didn't accidentally re-order or skip the
            // write.
            assert!(
                legacy_response.first_offset.byte_offset > cx_response.first_offset.byte_offset,
                "legacy byte_offset must follow cx byte_offset in the same storage"
            );
        });
    }

    /// The concrete Cx-first checkpoint commit must round-trip through the
    /// existing Cx-first read boundary for a live caller context.
    #[test]
    fn trait_checkpoint_cx_variants_round_trip() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let cx = crate::cx::for_request();

            let append = storage
                .append_batch(AppendRequest {
                    batch_id: "cx-checkpoint-source".to_string(),
                    events: vec![sample_event("cx-checkpoint-event", 1, 0, "source")],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let consumer = CheckpointConsumerId("cx-test".to_string());
            let checkpoint = RecorderCheckpoint {
                consumer: consumer.clone(),
                upto_offset: append.last_offset,
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                committed_at_ms: 1_700_000_000,
            };

            let commit_outcome = storage
                .commit_checkpoint_with_cx(&cx, checkpoint.clone())
                .await
                .unwrap();
            assert_eq!(commit_outcome, CheckpointCommitOutcome::Advanced);

            let read_back = storage
                .read_checkpoint_with_cx(&cx, &consumer)
                .await
                .unwrap()
                .expect("read_checkpoint_with_cx must return the just-written checkpoint");

            assert_eq!(read_back, checkpoint);
        });
    }

    #[test]
    fn trait_health_with_cx_cancelled_preserves_backend_kind() {
        run_async_test(async {
            let storage = StubRecorderStorage {
                backend: RecorderBackendKind::Rusqlite,
            };
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancelled recorder health probe"),
            );

            let health = storage.health_with_cx(&cx).await;
            assert!(health.degraded);
            assert_eq!(health.backend, RecorderBackendKind::Rusqlite);
            assert_eq!(
                health.last_error.as_deref(),
                Some("health cancelled pre-start via Cx")
            );

            let health_json = serde_json::to_value(&health).expect("serialize recorder health");
            assert_eq!(
                health_json["backend"],
                serde_json::Value::String("rusqlite".to_string())
            );
            assert_eq!(health_json["degraded"], serde_json::Value::Bool(true));
            assert_eq!(
                health_json["last_error"],
                serde_json::Value::String("health cancelled pre-start via Cx".to_string())
            );
        });
    }

    #[test]
    fn trait_default_cx_cancellation_is_typed_and_content_free() {
        fn assert_cancelled(error: RecorderStorageError, expected_operation: &'static str) {
            let display = error.to_string();
            assert!(
                matches!(
                    error,
                    RecorderStorageError::BlockingOperation {
                        operation,
                        failure: RecorderBlockingFailure::CancelledBeforeStart {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    } if operation == expected_operation
                ),
                "unexpected default Cx cancellation for {expected_operation}: {display}"
            );
            assert!(
                !display.contains("secret cancellation reason"),
                "default Cx cancellation leaked its caller-provided reason"
            );
        }

        run_async_test(async {
            let storage = StubRecorderStorage {
                backend: RecorderBackendKind::AppendLog,
            };
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("secret cancellation reason"),
            );

            assert_cancelled(
                storage
                    .append_batch_with_cx(
                        &cx,
                        AppendRequest {
                            batch_id: "unused".to_string(),
                            events: vec![sample_event("unused", 1, 0, "unused")],
                            required_durability: DurabilityLevel::Enqueued,
                            producer_ts_ms: 1,
                        },
                    )
                    .await
                    .unwrap_err(),
                "append_batch",
            );
            assert_cancelled(
                storage
                    .flush_with_cx(&cx, FlushMode::Buffered)
                    .await
                    .unwrap_err(),
                "flush",
            );
            assert_cancelled(
                storage
                    .read_checkpoint_with_cx(&cx, &CheckpointConsumerId("unused".to_string()))
                    .await
                    .unwrap_err(),
                "read_checkpoint",
            );
            assert_cancelled(
                storage
                    .commit_checkpoint_with_cx(
                        &cx,
                        RecorderCheckpoint {
                            consumer: CheckpointConsumerId("unused".to_string()),
                            upto_offset: RecorderOffset {
                                segment_id: 0,
                                byte_offset: 0,
                                ordinal: 0,
                            },
                            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                            committed_at_ms: 1,
                        },
                    )
                    .await
                    .unwrap_err(),
                "commit_checkpoint",
            );
            assert_cancelled(
                storage.lag_metrics_with_cx(&cx).await.unwrap_err(),
                "lag_metrics",
            );
        });
    }

    #[test]
    fn duplicate_batch_id_is_idempotent() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());
            let data_path = cfg.data_path.clone();
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();
            let event = sample_event("e1", 1, 0, "one");

            let first = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![event.clone()],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let before_len = std::fs::metadata(&data_path).unwrap().len();
            let second = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![event],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            let after_len = std::fs::metadata(&data_path).unwrap().len();

            assert_eq!(first.first_offset, second.first_offset);
            assert_eq!(first.last_offset, second.last_offset);
            assert_eq!(first.accepted_count, second.accepted_count);
            assert!(!first.was_idempotent_replay);
            assert!(second.was_idempotent_replay);
            assert_eq!(before_len, after_len);
        });
    }

    #[test]
    fn conflicting_batch_id_content_is_rejected_without_mutation_by_both_backends() {
        run_async_test(async {
            let append_dir = tempdir().unwrap();
            let append_config = test_config(append_dir.path());
            let append_path = append_config.data_path.clone();
            let append = AppendLogRecorderStorage::open(append_config).unwrap();
            append
                .append_batch(AppendRequest {
                    batch_id: "content-bound".to_string(),
                    events: vec![sample_event("first", 1, 0, "one")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            let append_len = std::fs::metadata(&append_path).unwrap().len();
            let append_error = append
                .append_batch(AppendRequest {
                    batch_id: "content-bound".to_string(),
                    events: vec![sample_event("second", 1, 1, "different")],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                append_error,
                RecorderStorageError::IdempotencyConflict { ref batch_id }
                    if batch_id == "content-bound"
            ));
            assert_eq!(std::fs::metadata(&append_path).unwrap().len(), append_len);
            assert_eq!(
                append
                    .health()
                    .await
                    .latest_offset
                    .map(|offset| offset.ordinal),
                Some(0)
            );
            assert!(!append.health().await.degraded);

            let sqlite_dir = tempdir().unwrap();
            let sqlite =
                RusqliteRecorderStorage::open(recorder_test_config(sqlite_dir.path()).rusqlite)
                    .unwrap();
            sqlite
                .append_batch(AppendRequest {
                    batch_id: "content-bound".to_string(),
                    events: vec![sample_event("first", 1, 0, "one")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            let sqlite_error = sqlite
                .append_batch(AppendRequest {
                    batch_id: "content-bound".to_string(),
                    events: vec![sample_event("second", 1, 1, "different")],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                sqlite_error,
                RecorderStorageError::IdempotencyConflict { ref batch_id }
                    if batch_id == "content-bound"
            ));
            assert_eq!(
                sqlite
                    .health()
                    .await
                    .latest_offset
                    .map(|offset| offset.ordinal),
                Some(0)
            );
            let mut cursor = sqlite.event_reader().open_cursor_from_start().unwrap();
            let records = cursor.next_batch(8).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].event.event_id, "first");
            assert!(!sqlite.health().await.degraded);
        });
    }

    #[test]
    fn duplicate_batch_id_upgrades_requested_durability_without_duplication() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());
            let data_path = cfg.data_path.clone();
            let state_path = cfg.state_path.clone();
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();
            let event = sample_event("e1", 1, 0, "one");

            let first = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![event.clone()],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            assert_eq!(first.committed_durability, DurabilityLevel::Enqueued);
            assert!(
                !state_path.exists(),
                "enqueued append should not persist state before a durability upgrade"
            );

            let upgraded = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![event.clone()],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();

            assert_eq!(upgraded.first_offset, first.first_offset);
            assert_eq!(upgraded.last_offset, first.last_offset);
            assert_eq!(upgraded.accepted_count, first.accepted_count);
            assert_eq!(upgraded.committed_durability, DurabilityLevel::Fsync);
            assert!(upgraded.was_idempotent_replay);
            assert!(
                state_path.exists(),
                "durability upgrade should persist recorder state"
            );
            let upgraded_len = std::fs::metadata(&data_path).unwrap().len();

            let cached = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![event],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 3,
                })
                .await
                .unwrap();
            let cached_len = std::fs::metadata(&data_path).unwrap().len();

            assert_eq!(cached.first_offset, first.first_offset);
            assert_eq!(cached.last_offset, first.last_offset);
            assert_eq!(cached.committed_durability, DurabilityLevel::Fsync);
            assert!(cached.was_idempotent_replay);
            assert_eq!(
                upgraded_len, cached_len,
                "cached duplicate must not append another copy of the batch"
            );
        });
    }

    #[test]
    fn checkpoint_commit_is_monotonic_and_persisted() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());
            let state_path = cfg.state_path.clone();
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "one")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let cp = RecorderCheckpoint {
                consumer: CheckpointConsumerId("lexical".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                schema_version: "ft.recorder.event.v1".to_string(),
                committed_at_ms: 123,
            };

            let outcome = storage.commit_checkpoint(cp.clone()).await.unwrap();
            assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

            let read_back = storage
                .read_checkpoint(&CheckpointConsumerId("lexical".to_string()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(read_back.upto_offset.ordinal, 0);

            let regression = RecorderCheckpoint {
                consumer: CheckpointConsumerId("lexical".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                schema_version: "ft.recorder.event.v1".to_string(),
                committed_at_ms: 124,
            };
            let no_op = storage.commit_checkpoint(regression).await.unwrap();
            assert_eq!(no_op, CheckpointCommitOutcome::NoopAlreadyAdvanced);

            drop(storage);

            let reopened = AppendLogRecorderStorage::open(cfg).unwrap();
            let persisted = reopened
                .read_checkpoint(&CheckpointConsumerId("lexical".to_string()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(persisted.upto_offset.ordinal, 0);
            assert!(state_path.exists());
        });
    }

    #[test]
    fn startup_truncates_torn_tail_and_recovers_ordinal() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            let event = sample_event("e1", 1, 0, "hello");
            let payload = serde_json::to_vec(&event).unwrap();
            let valid_len = 4 + payload.len() as u64;

            {
                let mut file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&cfg.data_path)
                    .unwrap();
                file.write_all(&(payload.len() as u32).to_le_bytes())
                    .unwrap();
                file.write_all(&payload).unwrap();
                file.write_all(&(100u32).to_le_bytes()).unwrap();
                file.write_all(b"abc").unwrap();
                file.flush().unwrap();
            }

            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            let recovered_len = std::fs::metadata(&cfg.data_path).unwrap().len();
            assert_eq!(recovered_len, valid_len);

            let response = storage
                .append_batch(AppendRequest {
                    batch_id: "b2".to_string(),
                    events: vec![sample_event("e2", 1, 1, "world")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();

            assert_eq!(response.first_offset.ordinal, 1);
        });
    }

    #[test]
    fn rejects_batch_larger_than_configured_byte_limit() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let mut cfg = test_config(dir.path());
            cfg.max_batch_bytes = 32;
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();

            let err = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event(
                        "e1",
                        1,
                        0,
                        "this event payload is intentionally too long",
                    )],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap_err();

            assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
        });
    }

    // ── Config validation ────────────────────────────────────────────

    #[test]
    fn config_validate_rejects_zero_queue_capacity() {
        let cfg = AppendLogStorageConfig {
            queue_capacity: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_rejects_zero_max_batch_events() {
        let cfg = AppendLogStorageConfig {
            max_batch_events: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_rejects_zero_max_batch_bytes() {
        let cfg = AppendLogStorageConfig {
            max_batch_bytes: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_rejects_zero_idempotency_entries() {
        let cfg = AppendLogStorageConfig {
            max_idempotency_entries: 0,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_accepts_valid_config() {
        let cfg = AppendLogStorageConfig::default();
        assert!(cfg.validate().is_ok());
    }

    // ── Request validation ───────────────────────────────────────────

    #[test]
    fn rejects_empty_batch_id() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let err = storage
                .append_batch(AppendRequest {
                    batch_id: "  ".to_string(),
                    events: vec![sample_event("e1", 1, 0, "hello")],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap_err();

            assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
        });
    }

    #[test]
    fn rejects_empty_events_list() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let err = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap_err();

            assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
        });
    }

    #[test]
    fn rejects_batch_exceeding_event_count_limit() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let mut cfg = test_config(dir.path());
            cfg.max_batch_events = 2;
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();

            let err = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![
                        sample_event("e1", 1, 0, "a"),
                        sample_event("e2", 1, 1, "b"),
                        sample_event("e3", 1, 2, "c"),
                    ],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap_err();

            assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
        });
    }

    // ── Idempotency cache eviction ───────────────────────────────────

    #[test]
    fn idempotency_cache_evicts_oldest_when_full() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let mut cfg = test_config(dir.path());
            cfg.max_idempotency_entries = 3;
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();

            // Insert 4 batches (cache holds 3)
            for i in 0..4u64 {
                let _ = storage
                    .append_batch(AppendRequest {
                        batch_id: format!("b{i}"),
                        events: vec![sample_event(&format!("e{i}"), 1, i, "x")],
                        required_durability: DurabilityLevel::Enqueued,
                        producer_ts_ms: i,
                    })
                    .await
                    .unwrap();
            }

            // b0 should have been evicted — replaying it should write new data
            let data_len_before = std::fs::metadata(dir.path().join("events.log"))
                .unwrap()
                .len();

            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b0".to_string(),
                    events: vec![sample_event("e0-replay", 1, 100, "replay")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 100,
                })
                .await
                .unwrap();

            let data_len_after = std::fs::metadata(dir.path().join("events.log"))
                .unwrap()
                .len();

            // b0 was evicted from cache, so replay should write new data
            assert!(data_len_after > data_len_before);
            assert_eq!(resp.first_offset.ordinal, 4); // ordinal 4 (after 0,1,2,3)

            // b3 should still be cached — replay should be idempotent
            let data_len_before2 = std::fs::metadata(dir.path().join("events.log"))
                .unwrap()
                .len();

            let cached = storage
                .append_batch(AppendRequest {
                    batch_id: "b3".to_string(),
                    events: vec![sample_event("e3", 1, 3, "x")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 200,
                })
                .await
                .unwrap();

            let data_len_after2 = std::fs::metadata(dir.path().join("events.log"))
                .unwrap()
                .len();

            assert_eq!(data_len_before2, data_len_after2, "b3 should be cached");
            assert!(cached.was_idempotent_replay);
        });
    }

    // ── Health and lag metrics ────────────────────────────────────────

    #[test]
    fn health_reports_correct_state() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            // Initial health: no data
            let h = storage.health().await;
            assert_eq!(h.backend, RecorderBackendKind::AppendLog);
            assert!(!h.degraded);
            assert_eq!(h.queue_depth, 0);
            assert_eq!(h.queue_capacity, 4);
            assert!(h.latest_offset.is_none());
            assert!(h.last_error.is_none());

            // After append: has data
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "hi")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let h2 = storage.health().await;
            assert!(h2.latest_offset.is_some());
            assert_eq!(h2.latest_offset.unwrap().ordinal, 0);
        });
    }

    #[test]
    fn lag_metrics_track_consumer_offsets() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            // Append 5 events
            for i in 0..5u64 {
                let _ = storage
                    .append_batch(AppendRequest {
                        batch_id: format!("b{i}"),
                        events: vec![sample_event(&format!("e{i}"), 1, i, "data")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: i,
                    })
                    .await
                    .unwrap();
            }

            // Register two consumers at different positions
            let _ = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId("indexer".to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 2,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 100,
                })
                .await
                .unwrap();

            let _ = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId("search".to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 4,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 100,
                })
                .await
                .unwrap();

            let lag = storage.lag_metrics().await.unwrap();
            assert!(lag.latest_offset.is_some());
            assert_eq!(lag.latest_offset.unwrap().ordinal, 4);
            assert_eq!(lag.consumers.len(), 2);

            // Consumers sorted by name
            assert_eq!(lag.consumers[0].consumer.0, "indexer");
            assert_eq!(lag.consumers[0].offsets_behind, 2); // 4 - 2
            assert_eq!(lag.consumers[1].consumer.0, "search");
            assert_eq!(lag.consumers[1].offsets_behind, 0); // 4 - 4
        });
    }

    // ── Checkpoint regression ────────────────────────────────────────

    #[test]
    fn checkpoint_regression_returns_error() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            // Advance checkpoint to ordinal 5
            let _ = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId("cons".to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 5,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 100,
                })
                .await
                .unwrap();

            // Try to regress to ordinal 3
            let err = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId("cons".to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 3,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 200,
                })
                .await
                .unwrap_err();

            assert!(matches!(
                err,
                RecorderStorageError::CheckpointRegression { .. }
            ));
            assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
        });
    }

    #[test]
    fn health_ignores_checkpoint_regression_from_caller_data() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let consumer = CheckpointConsumerId("diag-consumer".to_string());

            let _ = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: consumer.clone(),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 5,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 100,
                })
                .await
                .unwrap();

            let err = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: consumer.clone(),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 3,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 101,
                })
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                RecorderStorageError::CheckpointRegression { .. }
            ));

            let after_rejection = storage.health().await;
            assert!(
                !after_rejection.degraded,
                "a rejected caller checkpoint must not claim the storage backend is degraded"
            );
            assert!(after_rejection.last_error.is_none());

            let _ = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer,
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 8,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 102,
                })
                .await
                .unwrap();

            let healthy = storage.health().await;
            assert!(!healthy.degraded);
            assert!(healthy.last_error.is_none());
        });
    }

    #[test]
    fn health_ignores_invalid_append_request_from_caller_data() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let mut cfg = test_config(dir.path());
            cfg.max_batch_bytes = 1200;
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();

            let err = storage
                .append_batch(AppendRequest {
                    batch_id: "oversized".to_string(),
                    events: vec![sample_event("e-big", 1, 0, &"x".repeat(5000))],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 10,
                })
                .await
                .unwrap_err();
            assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));

            let after_rejection = storage.health().await;
            assert!(
                !after_rejection.degraded,
                "a rejected caller batch must not claim the storage backend is degraded"
            );
            assert!(after_rejection.last_error.is_none());

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "small".to_string(),
                    events: vec![sample_event("e-small", 1, 1, "ok")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 11,
                })
                .await
                .unwrap();

            let healthy = storage.health().await;
            assert!(!healthy.degraded);
            assert!(healthy.last_error.is_none());
        });
    }

    // ── Read checkpoint for unknown consumer ─────────────────────────

    #[test]
    fn read_checkpoint_unknown_consumer_returns_none() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let result = storage
                .read_checkpoint(&CheckpointConsumerId("nonexistent".to_string()))
                .await
                .unwrap();

            assert!(result.is_none());
        });
    }

    // ── Flush modes ──────────────────────────────────────────────────

    #[test]
    fn flush_buffered_and_durable() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Enqueued, // not flushed yet
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            // Flush buffered
            let stats_buf = storage.flush(FlushMode::Buffered).await.unwrap();
            assert_eq!(stats_buf.backend, RecorderBackendKind::AppendLog);
            assert!(stats_buf.latest_offset.is_some());

            // Flush durable
            let stats_dur = storage.flush(FlushMode::Durable).await.unwrap();
            assert_eq!(stats_dur.backend, RecorderBackendKind::AppendLog);
        });
    }

    // ── Durability levels ────────────────────────────────────────────

    #[test]
    fn checkpoint_busy_handler_does_not_pin_the_async_executor() {
        struct BusyGate {
            entered: AtomicBool,
            released: StdMutex<bool>,
            release: Condvar,
        }

        static ACTIVE_BUSY_GATE: StdMutex<Option<Arc<BusyGate>>> = StdMutex::new(None);
        static TEST_SERIALIZATION: StdMutex<()> = StdMutex::new(());

        fn wait_for_writer_release(_: i32) -> bool {
            let gate = ACTIVE_BUSY_GATE
                .lock()
                .unwrap()
                .as_ref()
                .cloned()
                .expect("checkpoint busy handler gate must be installed");
            gate.entered.store(true, Ordering::Release);
            let released = gate.released.lock().unwrap();
            let (released, _) = gate
                .release
                .wait_timeout_while(released, std::time::Duration::from_secs(10), |released| {
                    !*released
                })
                .unwrap();
            *released
        }

        let _serial = TEST_SERIALIZATION.lock().unwrap();
        let dir = tempdir().unwrap();
        let config = recorder_test_config(dir.path()).rusqlite;
        let storage = RusqliteRecorderStorage::open(config.clone()).unwrap();
        let gate = Arc::new(BusyGate {
            entered: AtomicBool::new(false),
            released: StdMutex::new(false),
            release: Condvar::new(),
        });
        *ACTIVE_BUSY_GATE.lock().unwrap() = Some(Arc::clone(&gate));
        run_async_test(async {
            storage
                .inner
                .lock()
                .await
                .conn
                .busy_handler(Some(wait_for_writer_release))
                .unwrap();
        });

        let (writer_locked_tx, writer_locked_rx) = std::sync::mpsc::channel();
        let (writer_release_tx, writer_release_rx) = std::sync::mpsc::channel();
        let writer_released = Arc::new(AtomicBool::new(false));
        let writer_released_by_thread = Arc::clone(&writer_released);
        let writer = std::thread::spawn(move || {
            let mut connection = Connection::open(config.db_path).unwrap();
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            writer_locked_tx.send(()).unwrap();
            writer_release_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("executor sibling must release the SQLite writer");
            transaction.commit().unwrap();
            writer_released_by_thread.store(true, Ordering::Release);
        });
        writer_locked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("external SQLite writer must acquire its transaction");

        run_async_test(async {
            let checkpoint = RecorderCheckpoint {
                consumer: CheckpointConsumerId("blocking-executor-proof".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                committed_at_ms: 1,
            };
            let cx = crate::cx::for_request();
            let sibling = async {
                wait_for_test_flag(&gate.entered, "real SQLite busy-handler entry").await;
                writer_release_tx.send(()).unwrap();
                wait_for_test_flag(&writer_released, "external SQLite writer release").await;
                let mut released = gate.released.lock().unwrap();
                *released = true;
                drop(released);
                gate.release.notify_all();
            };
            let (outcome, ()) =
                futures::join!(storage.commit_checkpoint_with_cx(&cx, checkpoint), sibling);
            assert_eq!(outcome.unwrap(), CheckpointCommitOutcome::Advanced);
        });

        writer.join().unwrap();
        *ACTIVE_BUSY_GATE.lock().unwrap() = None;
    }

    #[test]
    fn mutation_with_cx_pre_cancel_is_typed_and_never_enters_blocking_body() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                config.append_log.queue_capacity = 1;
                config.rusqlite.queue_capacity = 1;
                let storage = bootstrap_recorder_storage(config.clone()).unwrap();

                let append_entered = Arc::new(AtomicBool::new(false));
                install_blocking_test_hook(
                    &storage,
                    RecorderBlockingTestOperation::Append,
                    RecorderBlockingTestHook {
                        before: {
                            let append_entered = Arc::clone(&append_entered);
                            Arc::new(move || append_entered.store(true, Ordering::Release))
                        },
                        after: Arc::new(|| {}),
                    },
                )
                .await;
                let append_cx = crate::cx::for_testing();
                append_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("recorder append pre-cancel test"),
                );
                let append_error = storage
                    .append_batch_with_cx(
                        &append_cx,
                        AppendRequest {
                            batch_id: format!("pre-cancel-append-{backend}"),
                            events: vec![sample_event("pre-cancel-event", 1, 0, "unchanged")],
                            required_durability: DurabilityLevel::Fsync,
                            producer_ts_ms: 1,
                        },
                    )
                    .await
                    .expect_err("pre-cancelled append must not report success");
                assert!(matches!(
                    append_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "append_batch",
                        failure: RecorderBlockingFailure::CancelledBeforeStart {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                assert!(
                    !append_entered.load(Ordering::Acquire),
                    "{backend} pre-cancelled append entered its blocking body"
                );
                assert_eq!(persisted_event_count(&config), 0);
                clear_blocking_test_hook(&storage, RecorderBlockingTestOperation::Append).await;

                let checkpoint_entered = Arc::new(AtomicBool::new(false));
                install_blocking_test_hook(
                    &storage,
                    RecorderBlockingTestOperation::Checkpoint,
                    RecorderBlockingTestHook {
                        before: {
                            let checkpoint_entered = Arc::clone(&checkpoint_entered);
                            Arc::new(move || checkpoint_entered.store(true, Ordering::Release))
                        },
                        after: Arc::new(|| {}),
                    },
                )
                .await;
                let checkpoint = RecorderCheckpoint {
                    consumer: CheckpointConsumerId(format!("pre-cancel-checkpoint-{backend}")),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    },
                    schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                    committed_at_ms: 1,
                };
                let checkpoint_cx = crate::cx::for_testing();
                checkpoint_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("recorder checkpoint pre-cancel test"),
                );
                let checkpoint_error = storage
                    .commit_checkpoint_with_cx(&checkpoint_cx, checkpoint.clone())
                    .await
                    .expect_err("pre-cancelled checkpoint must not report success");
                assert!(matches!(
                    checkpoint_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "commit_checkpoint",
                        failure: RecorderBlockingFailure::CancelledBeforeStart {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                assert!(
                    !checkpoint_entered.load(Ordering::Acquire),
                    "{backend} pre-cancelled checkpoint entered its blocking body"
                );
                assert_eq!(
                    storage.read_checkpoint(&checkpoint.consumer).await.unwrap(),
                    None
                );
                clear_blocking_test_hook(&storage, RecorderBlockingTestOperation::Checkpoint).await;

                // Both pre-cancelled calls must release admission immediately.
                let appended = storage
                    .append_batch(AppendRequest {
                        batch_id: format!("post-pre-cancel-append-{backend}"),
                        events: vec![sample_event("post-pre-cancel-event", 1, 0, "written")],
                        required_durability: DurabilityLevel::Fsync,
                        producer_ts_ms: 2,
                    })
                    .await
                    .unwrap();
                assert_eq!(appended.backend, backend);
                let checkpoint = blocking_test_checkpoint(&appended);
                assert_eq!(
                    storage.commit_checkpoint(checkpoint).await.unwrap(),
                    CheckpointCommitOutcome::Advanced
                );
            }
        });
    }

    #[test]
    fn mutation_with_cx_cancelled_at_completion_never_delivers_success() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                config.append_log.queue_capacity = 1;
                config.rusqlite.queue_capacity = 1;
                let storage = bootstrap_recorder_storage(config.clone()).unwrap();

                let append_cx = crate::cx::for_testing();
                install_blocking_test_hook(
                    &storage,
                    RecorderBlockingTestOperation::Append,
                    RecorderBlockingTestHook {
                        before: Arc::new(|| {}),
                        after: {
                            let append_cx = append_cx.clone();
                            Arc::new(move || {
                                append_cx.cancel_with(
                                    crate::outcome::CancelKind::User,
                                    Some("recorder append completion delivery gate test"),
                                );
                            })
                        },
                    },
                )
                .await;
                let append_request = AppendRequest {
                    batch_id: format!("completion-gate-append-{backend}"),
                    events: vec![sample_event("completion-gate-event", 11, 0, "settled")],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 1,
                };
                let append_error = storage
                    .append_batch_with_cx(&append_cx, append_request.clone())
                    .await
                    .expect_err("an append cancelled before result delivery must not succeed");
                assert!(matches!(
                    append_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "append_batch",
                        failure: RecorderBlockingFailure::CancelledMidFlight {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                clear_blocking_test_hook(&storage, RecorderBlockingTestOperation::Append).await;
                wait_for_blocking_admission_release(
                    &storage,
                    RecorderBlockingTestOperation::Append,
                    "append completion-gate admission release",
                )
                .await;
                let replay = storage.append_batch(append_request).await.unwrap();
                assert_eq!(replay.backend, backend);
                assert!(replay.was_idempotent_replay);
                assert_eq!(persisted_event_count(&config), 1);

                let checkpoint = blocking_test_checkpoint(&replay);
                let checkpoint_cx = crate::cx::for_testing();
                install_blocking_test_hook(
                    &storage,
                    RecorderBlockingTestOperation::Checkpoint,
                    RecorderBlockingTestHook {
                        before: Arc::new(|| {}),
                        after: {
                            let checkpoint_cx = checkpoint_cx.clone();
                            Arc::new(move || {
                                checkpoint_cx.cancel_with(
                                    crate::outcome::CancelKind::User,
                                    Some("recorder checkpoint completion delivery gate test"),
                                );
                            })
                        },
                    },
                )
                .await;
                let checkpoint_error = storage
                    .commit_checkpoint_with_cx(&checkpoint_cx, checkpoint.clone())
                    .await
                    .expect_err("a checkpoint cancelled before result delivery must not succeed");
                assert!(matches!(
                    checkpoint_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "commit_checkpoint",
                        failure: RecorderBlockingFailure::CancelledMidFlight {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                clear_blocking_test_hook(&storage, RecorderBlockingTestOperation::Checkpoint).await;
                wait_for_blocking_admission_release(
                    &storage,
                    RecorderBlockingTestOperation::Checkpoint,
                    "checkpoint completion-gate admission release",
                )
                .await;
                assert_eq!(
                    storage.commit_checkpoint(checkpoint.clone()).await.unwrap(),
                    CheckpointCommitOutcome::NoopAlreadyAdvanced
                );
                assert_eq!(
                    storage.read_checkpoint(&checkpoint.consumer).await.unwrap(),
                    Some(checkpoint)
                );
            }
        });
    }

    #[test]
    fn mutation_with_cx_midflight_cancel_retains_owned_settlement_and_exact_retry() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                config.append_log.queue_capacity = 1;
                config.rusqlite.queue_capacity = 1;
                let storage = Arc::new(bootstrap_recorder_storage(config.clone()).unwrap());
                let polling_thread = std::thread::current().id();

                let append_started = Arc::new(AtomicBool::new(false));
                let append_completed = Arc::new(AtomicBool::new(false));
                let append_thread = Arc::new(StdMutex::new(None));
                let append_release = Arc::new((StdMutex::new(false), Condvar::new()));
                install_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Append,
                    RecorderBlockingTestHook {
                        before: {
                            let append_started = Arc::clone(&append_started);
                            let append_thread = Arc::clone(&append_thread);
                            let append_release = Arc::clone(&append_release);
                            Arc::new(move || {
                                *append_thread.lock().unwrap() = Some(std::thread::current().id());
                                append_started.store(true, Ordering::Release);
                                wait_for_blocking_test_release(&append_release, backend, "append");
                            })
                        },
                        after: {
                            let append_completed = Arc::clone(&append_completed);
                            Arc::new(move || append_completed.store(true, Ordering::Release))
                        },
                    },
                )
                .await;
                let append_request = AppendRequest {
                    batch_id: format!("midflight-append-{backend}"),
                    events: vec![sample_event("midflight-event", 7, 0, "settled-once")],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 1,
                };
                let append_cx = crate::cx::for_testing();
                let append_task_cx = append_cx.clone();
                let append_task_storage = Arc::clone(&storage);
                let append_task_request = append_request.clone();
                let (append_result_tx, append_result_rx) = crate::runtime_async::oneshot::channel();
                let append_task = crate::runtime_async::task::spawn(async move {
                    let result = append_task_storage
                        .append_batch_with_cx(&append_task_cx, append_task_request)
                        .await;
                    let _ = append_result_tx.send(result);
                });

                wait_for_test_flag(&append_started, "blocking recorder append admission").await;
                assert_ne!(
                    *append_thread.lock().unwrap(),
                    Some(polling_thread),
                    "{backend} append body ran inline on the executor thread"
                );
                let append_pre_cancel_cx = crate::cx::for_testing();
                append_pre_cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("concurrent append pre-cancel test"),
                );
                let pre_cancel_error = storage
                    .append_batch_with_cx(&append_pre_cancel_cx, append_request.clone())
                    .await
                    .expect_err("pre-cancel must take precedence over append overload");
                assert!(matches!(
                    pre_cancel_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "append_batch",
                        failure: RecorderBlockingFailure::CancelledBeforeStart { .. }
                    }
                ));
                let append_overload = storage
                    .append_batch(AppendRequest {
                        batch_id: format!("overlapping-append-{backend}"),
                        events: vec![sample_event("overlapping-event", 7, 1, "must-not-write")],
                        required_durability: DurabilityLevel::Fsync,
                        producer_ts_ms: 2,
                    })
                    .await
                    .expect_err("admitted append must retain its configured capacity");
                assert!(matches!(
                    append_overload,
                    RecorderStorageError::QueueFull { capacity: 1 }
                ));
                append_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("recorder append mid-flight cancel test"),
                );
                let append_error = crate::runtime_async::timeout(
                    std::time::Duration::from_secs(5),
                    crate::runtime_async::oneshot_recv(append_result_rx),
                )
                .await
                .expect("mid-flight append cancellation did not settle within 5s")
                .expect("append task dropped its result sender")
                .expect_err("mid-flight append cancellation must not report success");
                assert!(matches!(
                    append_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "append_batch",
                        failure: RecorderBlockingFailure::CancelledMidFlight {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                assert!(
                    !append_completed.load(Ordering::Acquire),
                    "{backend} append cancellation waited for the held blocking body"
                );
                release_blocking_test_gate(&append_release);
                clear_blocking_test_hook(storage.as_ref(), RecorderBlockingTestOperation::Append)
                    .await;
                append_task.await.unwrap();
                assert!(append_completed.load(Ordering::Acquire));
                wait_for_blocking_admission_release(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Append,
                    "append admission release after late settlement",
                )
                .await;

                let reconciled_append = storage.append_batch(append_request).await.unwrap();
                assert_eq!(reconciled_append.backend, backend);
                assert!(reconciled_append.was_idempotent_replay);
                assert_eq!(reconciled_append.first_offset.ordinal, 0);
                assert_eq!(reconciled_append.last_offset.ordinal, 0);
                assert_eq!(persisted_event_count(&config), 1);
                assert_eq!(
                    storage.flush(FlushMode::Durable).await.unwrap().backend,
                    backend
                );

                let checkpoint = blocking_test_checkpoint(&reconciled_append);
                let checkpoint_started = Arc::new(AtomicBool::new(false));
                let checkpoint_completed = Arc::new(AtomicBool::new(false));
                let checkpoint_thread = Arc::new(StdMutex::new(None));
                let checkpoint_release = Arc::new((StdMutex::new(false), Condvar::new()));
                install_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Checkpoint,
                    RecorderBlockingTestHook {
                        before: {
                            let checkpoint_started = Arc::clone(&checkpoint_started);
                            let checkpoint_thread = Arc::clone(&checkpoint_thread);
                            let checkpoint_release = Arc::clone(&checkpoint_release);
                            Arc::new(move || {
                                *checkpoint_thread.lock().unwrap() =
                                    Some(std::thread::current().id());
                                checkpoint_started.store(true, Ordering::Release);
                                wait_for_blocking_test_release(
                                    &checkpoint_release,
                                    backend,
                                    "checkpoint",
                                );
                            })
                        },
                        after: {
                            let checkpoint_completed = Arc::clone(&checkpoint_completed);
                            Arc::new(move || checkpoint_completed.store(true, Ordering::Release))
                        },
                    },
                )
                .await;
                let checkpoint_cx = crate::cx::for_testing();
                let checkpoint_task_cx = checkpoint_cx.clone();
                let checkpoint_task_storage = Arc::clone(&storage);
                let checkpoint_task_value = checkpoint.clone();
                let (checkpoint_result_tx, checkpoint_result_rx) =
                    crate::runtime_async::oneshot::channel();
                let checkpoint_task = crate::runtime_async::task::spawn(async move {
                    let result = checkpoint_task_storage
                        .commit_checkpoint_with_cx(&checkpoint_task_cx, checkpoint_task_value)
                        .await;
                    let _ = checkpoint_result_tx.send(result);
                });

                wait_for_test_flag(
                    &checkpoint_started,
                    "blocking recorder checkpoint admission",
                )
                .await;
                assert_ne!(
                    *checkpoint_thread.lock().unwrap(),
                    Some(polling_thread),
                    "{backend} checkpoint body ran inline on the executor thread"
                );
                let checkpoint_pre_cancel_cx = crate::cx::for_testing();
                checkpoint_pre_cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("concurrent checkpoint pre-cancel test"),
                );
                let pre_cancel_error = storage
                    .commit_checkpoint_with_cx(&checkpoint_pre_cancel_cx, checkpoint.clone())
                    .await
                    .expect_err("pre-cancel must take precedence over checkpoint overload");
                assert!(matches!(
                    pre_cancel_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "commit_checkpoint",
                        failure: RecorderBlockingFailure::CancelledBeforeStart { .. }
                    }
                ));
                let checkpoint_overload = storage
                    .commit_checkpoint(checkpoint.clone())
                    .await
                    .expect_err("checkpoint single-flight must reject overlapping work");
                assert!(matches!(
                    checkpoint_overload,
                    RecorderStorageError::QueueFull { capacity: 1 }
                ));
                checkpoint_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("recorder checkpoint mid-flight cancel test"),
                );
                let checkpoint_error = crate::runtime_async::timeout(
                    std::time::Duration::from_secs(5),
                    crate::runtime_async::oneshot_recv(checkpoint_result_rx),
                )
                .await
                .expect("mid-flight checkpoint cancellation did not settle within 5s")
                .expect("checkpoint task dropped its result sender")
                .expect_err("mid-flight checkpoint cancellation must not report success");
                assert!(matches!(
                    checkpoint_error,
                    RecorderStorageError::BlockingOperation {
                        operation: "commit_checkpoint",
                        failure: RecorderBlockingFailure::CancelledMidFlight {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                assert!(
                    !checkpoint_completed.load(Ordering::Acquire),
                    "{backend} checkpoint cancellation waited for the held blocking body"
                );
                release_blocking_test_gate(&checkpoint_release);
                clear_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Checkpoint,
                )
                .await;
                checkpoint_task.await.unwrap();
                assert!(checkpoint_completed.load(Ordering::Acquire));
                wait_for_blocking_admission_release(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Checkpoint,
                    "checkpoint admission release after late settlement",
                )
                .await;

                assert_eq!(
                    storage.commit_checkpoint(checkpoint.clone()).await.unwrap(),
                    CheckpointCommitOutcome::NoopAlreadyAdvanced
                );
                assert_eq!(
                    storage.read_checkpoint(&checkpoint.consumer).await.unwrap(),
                    Some(checkpoint.clone())
                );
                drop(storage);

                let reopened = bootstrap_recorder_storage(config.clone()).unwrap();
                assert_eq!(persisted_event_count(&config), 1);
                assert_eq!(
                    reopened
                        .read_checkpoint(&checkpoint.consumer)
                        .await
                        .unwrap(),
                    Some(checkpoint)
                );
                assert_eq!(
                    reopened
                        .health()
                        .await
                        .latest_offset
                        .map(|offset| offset.ordinal),
                    Some(0)
                );
            }
        });
    }

    #[test]
    fn cross_operation_blocking_bodies_share_one_serialization_authority() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                let storage = Arc::new(bootstrap_recorder_storage(config.clone()).unwrap());
                let active_bodies = Arc::new(AtomicUsize::new(0));
                let append_started = Arc::new(AtomicBool::new(false));
                let checkpoint_started = Arc::new(AtomicBool::new(false));
                let flush_started = Arc::new(AtomicBool::new(false));
                let append_release = Arc::new((StdMutex::new(false), Condvar::new()));

                install_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Append,
                    RecorderBlockingTestHook {
                        before: {
                            let active_bodies = Arc::clone(&active_bodies);
                            let append_started = Arc::clone(&append_started);
                            let append_release = Arc::clone(&append_release);
                            Arc::new(move || {
                                assert_eq!(
                                    active_bodies.fetch_add(1, Ordering::AcqRel),
                                    0,
                                    "{backend} append overlapped another mutation body"
                                );
                                append_started.store(true, Ordering::Release);
                                wait_for_blocking_test_release(
                                    &append_release,
                                    backend,
                                    "cross-operation append",
                                );
                            })
                        },
                        after: {
                            let active_bodies = Arc::clone(&active_bodies);
                            Arc::new(move || {
                                assert_eq!(active_bodies.fetch_sub(1, Ordering::AcqRel), 1);
                            })
                        },
                    },
                )
                .await;
                install_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Checkpoint,
                    RecorderBlockingTestHook {
                        before: {
                            let active_bodies = Arc::clone(&active_bodies);
                            let checkpoint_started = Arc::clone(&checkpoint_started);
                            Arc::new(move || {
                                assert_eq!(
                                    active_bodies.fetch_add(1, Ordering::AcqRel),
                                    0,
                                    "{backend} checkpoint overlapped another mutation body"
                                );
                                checkpoint_started.store(true, Ordering::Release);
                            })
                        },
                        after: {
                            let active_bodies = Arc::clone(&active_bodies);
                            Arc::new(move || {
                                assert_eq!(active_bodies.fetch_sub(1, Ordering::AcqRel), 1);
                            })
                        },
                    },
                )
                .await;
                install_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Flush,
                    RecorderBlockingTestHook {
                        before: {
                            let active_bodies = Arc::clone(&active_bodies);
                            let flush_started = Arc::clone(&flush_started);
                            Arc::new(move || {
                                assert_eq!(
                                    active_bodies.fetch_add(1, Ordering::AcqRel),
                                    0,
                                    "{backend} flush overlapped another mutation body"
                                );
                                flush_started.store(true, Ordering::Release);
                            })
                        },
                        after: {
                            let active_bodies = Arc::clone(&active_bodies);
                            Arc::new(move || {
                                assert_eq!(active_bodies.fetch_sub(1, Ordering::AcqRel), 1);
                            })
                        },
                    },
                )
                .await;

                let append_storage = Arc::clone(&storage);
                let append_task = crate::runtime_async::task::spawn(async move {
                    append_storage
                        .append_batch(AppendRequest {
                            batch_id: format!("cross-operation-{backend}"),
                            events: vec![sample_event("cross-operation-event", 13, 0, "ordered")],
                            required_durability: DurabilityLevel::Fsync,
                            producer_ts_ms: 1,
                        })
                        .await
                });
                wait_for_test_flag(&append_started, "cross-operation append body").await;

                let checkpoint = RecorderCheckpoint {
                    consumer: CheckpointConsumerId(format!("cross-operation-{backend}")),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    },
                    schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                    committed_at_ms: 2,
                };
                let checkpoint_storage = Arc::clone(&storage);
                let checkpoint_value = checkpoint.clone();
                let checkpoint_task = crate::runtime_async::task::spawn(async move {
                    checkpoint_storage.commit_checkpoint(checkpoint_value).await
                });
                let flush_storage = Arc::clone(&storage);
                let flush_task = crate::runtime_async::task::spawn(async move {
                    flush_storage.flush(FlushMode::Durable).await
                });
                crate::runtime_async::timeout(std::time::Duration::from_secs(5), async {
                    while blocking_admission_depth(
                        storage.as_ref(),
                        RecorderBlockingTestOperation::Checkpoint,
                    ) != 1
                        || blocking_admission_depth(
                            storage.as_ref(),
                            RecorderBlockingTestOperation::Flush,
                        ) != 1
                    {
                        crate::runtime_async::yield_now().await;
                    }
                })
                .await
                .expect("checkpoint and flush did not enter their bounded admissions");
                assert!(!checkpoint_started.load(Ordering::Acquire));
                assert!(!flush_started.load(Ordering::Acquire));

                release_blocking_test_gate(&append_release);
                let (append_result, checkpoint_result, flush_result) =
                    crate::runtime_async::timeout(std::time::Duration::from_secs(5), async {
                        (append_task.await, checkpoint_task.await, flush_task.await)
                    })
                    .await
                    .expect("serialized recorder mutations did not all settle within 5s");
                let append = append_result.unwrap().unwrap();
                assert_eq!(append.backend, backend);
                assert_eq!(
                    checkpoint_result.unwrap().unwrap(),
                    CheckpointCommitOutcome::Advanced
                );
                assert_eq!(flush_result.unwrap().unwrap().backend, backend);
                assert!(checkpoint_started.load(Ordering::Acquire));
                assert!(flush_started.load(Ordering::Acquire));
                assert_eq!(active_bodies.load(Ordering::Acquire), 0);
                assert_eq!(persisted_event_count(&config), 1);
                assert_eq!(
                    storage.read_checkpoint(&checkpoint.consumer).await.unwrap(),
                    Some(checkpoint)
                );
                clear_blocking_test_hook(storage.as_ref(), RecorderBlockingTestOperation::Append)
                    .await;
                clear_blocking_test_hook(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Checkpoint,
                )
                .await;
                clear_blocking_test_hook(storage.as_ref(), RecorderBlockingTestOperation::Flush)
                    .await;
            }
        });
    }

    #[test]
    fn flush_with_cx_leaves_executor_responsive_and_reopens_for_both_backends() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                let storage = bootstrap_recorder_storage(config.clone()).unwrap();
                storage
                    .append_batch(AppendRequest {
                        batch_id: format!("blocking-flush-{backend}"),
                        events: vec![sample_event("blocking-flush-event", 1, 0, "durable")],
                        required_durability: DurabilityLevel::Enqueued,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();

                let polling_thread = std::thread::current().id();
                let blocking_thread = Arc::new(StdMutex::new(None));
                let started = Arc::new(AtomicBool::new(false));
                let completed = Arc::new(AtomicBool::new(false));
                let sibling_ran = Arc::new(AtomicBool::new(false));
                let release = Arc::new((StdMutex::new(false), Condvar::new()));
                let hook = RecorderBlockingTestHook {
                    before: {
                        let blocking_thread = Arc::clone(&blocking_thread);
                        let started = Arc::clone(&started);
                        let release = Arc::clone(&release);
                        Arc::new(move || {
                            *blocking_thread.lock().unwrap() = Some(std::thread::current().id());
                            started.store(true, Ordering::Release);
                            wait_for_blocking_test_release(&release, backend, "flush");
                        })
                    },
                    after: {
                        let completed = Arc::clone(&completed);
                        Arc::new(move || completed.store(true, Ordering::Release))
                    },
                };
                install_flush_test_hook(&storage, hook).await;

                let cx = crate::cx::for_request();
                let sibling = async {
                    wait_for_test_flag(&started, "blocking recorder flush admission").await;
                    sibling_ran.store(true, Ordering::Release);
                    release_blocking_test_gate(&release);
                };
                let (flush, ()) =
                    futures::join!(storage.flush_with_cx(&cx, FlushMode::Durable), sibling);
                let stats = flush.unwrap();

                assert_eq!(stats.backend, backend);
                assert_eq!(
                    stats.latest_offset.as_ref().map(|offset| offset.ordinal),
                    Some(0)
                );
                assert!(sibling_ran.load(Ordering::Acquire));
                assert!(completed.load(Ordering::Acquire));
                assert_ne!(
                    *blocking_thread.lock().unwrap(),
                    Some(polling_thread),
                    "{backend} flush body ran inline on the executor thread"
                );

                drop(storage);
                let reopened = bootstrap_recorder_storage(config).unwrap();
                assert_eq!(
                    reopened
                        .health()
                        .await
                        .latest_offset
                        .as_ref()
                        .map(|offset| offset.ordinal),
                    Some(0),
                    "{backend} durable flush did not survive reopen"
                );
            }
        });
    }

    #[test]
    fn flush_with_cx_pre_cancel_never_enters_blocking_body() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                let storage = bootstrap_recorder_storage(config).unwrap();
                let entered = Arc::new(AtomicBool::new(false));
                install_flush_test_hook(
                    &storage,
                    RecorderBlockingTestHook {
                        before: {
                            let entered = Arc::clone(&entered);
                            Arc::new(move || entered.store(true, Ordering::Release))
                        },
                        after: Arc::new(|| {}),
                    },
                )
                .await;

                let cx = crate::cx::for_testing();
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("recorder flush pre-cancel test"),
                );
                let error = storage
                    .flush_with_cx(&cx, FlushMode::Durable)
                    .await
                    .expect_err("pre-cancelled flush must not report success");
                assert!(matches!(
                    error,
                    RecorderStorageError::BlockingOperation {
                        operation: "flush",
                        failure: RecorderBlockingFailure::CancelledBeforeStart {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                assert!(
                    !entered.load(Ordering::Acquire),
                    "{backend} pre-cancelled flush entered the blocking body"
                );
                clear_flush_test_hook(&storage).await;
                assert_eq!(
                    storage.flush(FlushMode::Buffered).await.unwrap().backend,
                    backend,
                    "{backend} pre-cancellation leaked its exclusive flush admission"
                );
            }
        });
    }

    #[test]
    fn flush_with_cx_midflight_cancel_is_non_success_and_retry_reconciles() {
        run_async_test(async {
            for backend in [
                RecorderBackendKind::AppendLog,
                RecorderBackendKind::Rusqlite,
            ] {
                let dir = tempdir().unwrap();
                let mut config = recorder_test_config(dir.path());
                config.backend = backend;
                let storage = Arc::new(bootstrap_recorder_storage(config.clone()).unwrap());
                storage
                    .append_batch(AppendRequest {
                        batch_id: format!("cancelled-flush-{backend}"),
                        events: vec![sample_event("cancelled-flush-event", 1, 0, "settle")],
                        required_durability: DurabilityLevel::Enqueued,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();

                let started = Arc::new(AtomicBool::new(false));
                let completed = Arc::new(AtomicBool::new(false));
                let release = Arc::new((StdMutex::new(false), Condvar::new()));
                install_flush_test_hook(
                    &storage,
                    RecorderBlockingTestHook {
                        before: {
                            let started = Arc::clone(&started);
                            let release = Arc::clone(&release);
                            Arc::new(move || {
                                started.store(true, Ordering::Release);
                                wait_for_blocking_test_release(&release, backend, "flush");
                            })
                        },
                        after: {
                            let completed = Arc::clone(&completed);
                            Arc::new(move || completed.store(true, Ordering::Release))
                        },
                    },
                )
                .await;

                let cx = crate::cx::for_testing();
                let task_cx = cx.clone();
                let task_storage = Arc::clone(&storage);
                let (result_tx, result_rx) = crate::runtime_async::oneshot::channel();
                let flush_task = crate::runtime_async::task::spawn(async move {
                    let result = task_storage
                        .flush_with_cx(&task_cx, FlushMode::Durable)
                        .await;
                    let _ = result_tx.send(result);
                });

                wait_for_test_flag(&started, "mid-flight recorder flush admission").await;
                let pre_cancelled_cx = crate::cx::for_testing();
                pre_cancelled_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("concurrent recorder flush pre-cancel test"),
                );
                let pre_cancelled = storage
                    .flush_with_cx(&pre_cancelled_cx, FlushMode::Buffered)
                    .await
                    .expect_err("pre-cancellation must take precedence over overload");
                assert!(matches!(
                    pre_cancelled,
                    RecorderStorageError::BlockingOperation {
                        operation: "flush",
                        failure: RecorderBlockingFailure::CancelledBeforeStart {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                let overload = storage
                    .flush(FlushMode::Buffered)
                    .await
                    .expect_err("a second flush must not occupy another blocking worker");
                assert!(
                    matches!(overload, RecorderStorageError::QueueFull { capacity: 1 }),
                    "{backend} returned the wrong concurrent-flush admission error: {overload}"
                );
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("recorder flush mid-flight cancel test"),
                );
                let error = crate::runtime_async::timeout(
                    std::time::Duration::from_secs(5),
                    crate::runtime_async::oneshot_recv(result_rx),
                )
                .await
                .expect("mid-flight flush cancellation did not settle within 5s")
                .expect("flush task dropped its result sender")
                .expect_err("mid-flight cancellation must not report durable success");
                assert!(matches!(
                    error,
                    RecorderStorageError::BlockingOperation {
                        operation: "flush",
                        failure: RecorderBlockingFailure::CancelledMidFlight {
                            kind: Some(crate::outcome::CancelKind::User)
                        }
                    }
                ));
                assert!(
                    !completed.load(Ordering::Acquire),
                    "{backend} cancellation waited for the held flush body"
                );
                release_blocking_test_gate(&release);
                clear_flush_test_hook(storage.as_ref()).await;
                flush_task.await.unwrap();
                wait_for_blocking_admission_release(
                    storage.as_ref(),
                    RecorderBlockingTestOperation::Flush,
                    "flush admission release after late settlement",
                )
                .await;
                let reconciled = storage.flush(FlushMode::Durable).await.unwrap();
                assert_eq!(reconciled.backend, backend);
                assert!(completed.load(Ordering::Acquire));
                drop(storage);

                let reopened = bootstrap_recorder_storage(config).unwrap();
                assert_eq!(
                    reopened
                        .health()
                        .await
                        .latest_offset
                        .as_ref()
                        .map(|offset| offset.ordinal),
                    Some(0),
                    "{backend} retry did not reconcile the cancelled flush before reopen"
                );
            }
        });
    }

    #[test]
    fn enqueued_durability_does_not_fsync() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            assert_eq!(resp.committed_durability, DurabilityLevel::Enqueued);
            assert_eq!(resp.accepted_count, 1);
        });
    }

    #[test]
    fn fsync_durability_committed() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Fsync,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            assert_eq!(resp.committed_durability, DurabilityLevel::Fsync);
            // State should be persisted (fsync does persist_state)
            assert!(dir.path().join("state.json").exists());
        });
    }

    // ── Reopen persistence ───────────────────────────────────────────

    #[test]
    fn reopen_continues_ordinals() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            {
                let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
                let _ = storage
                    .append_batch(AppendRequest {
                        batch_id: "b1".to_string(),
                        events: vec![
                            sample_event("e1", 1, 0, "a"),
                            sample_event("e2", 1, 1, "b"),
                            sample_event("e3", 1, 2, "c"),
                        ],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();
            }

            // Reopen and verify ordinals continue
            let storage2 = AppendLogRecorderStorage::open(cfg).unwrap();
            let resp = storage2
                .append_batch(AppendRequest {
                    batch_id: "b2".to_string(),
                    events: vec![sample_event("e4", 1, 3, "d")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();

            assert_eq!(resp.first_offset.ordinal, 3);
        });
    }

    #[test]
    fn reopen_uses_scanned_ordinal_when_state_ordinal_is_stale() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            {
                let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
                let _ = storage
                    .append_batch(AppendRequest {
                        batch_id: "b1".to_string(),
                        events: vec![sample_event("e1", 1, 0, "a"), sample_event("e2", 1, 1, "b")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();
            }

            let state_file = AppendLogStateFile::open(&cfg.state_path).unwrap();
            let mut persisted = state_file.load().unwrap();
            let recovered_offset = persisted.next_offset;
            persisted.next_ordinal = 99;
            state_file.write(&persisted).unwrap();
            drop(state_file);

            let reopened = AppendLogRecorderStorage::open(cfg).unwrap();
            let resp = reopened
                .append_batch(AppendRequest {
                    batch_id: "b2".to_string(),
                    events: vec![sample_event("e3", 1, 2, "c")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();

            assert_eq!(resp.first_offset.byte_offset, recovered_offset);
            assert_eq!(resp.first_offset.ordinal, 2);
        });
    }

    #[test]
    fn reopen_drops_only_checkpoints_beyond_recovered_log_head() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());
            let keep_consumer = CheckpointConsumerId("keep".to_string());
            let drop_consumer = CheckpointConsumerId("drop".to_string());

            {
                let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
                let resp = storage
                    .append_batch(AppendRequest {
                        batch_id: "b1".to_string(),
                        events: vec![sample_event("e1", 1, 0, "a"), sample_event("e2", 1, 1, "b")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();

                storage
                    .commit_checkpoint(RecorderCheckpoint {
                        consumer: keep_consumer.clone(),
                        upto_offset: resp.first_offset,
                        schema_version: "v1".to_string(),
                        committed_at_ms: 10,
                    })
                    .await
                    .unwrap();
            }

            let actual_len = std::fs::metadata(&cfg.data_path).unwrap().len();
            let state_file = AppendLogStateFile::open(&cfg.state_path).unwrap();
            let mut persisted = state_file.load().unwrap();
            persisted.segment_id = 9;
            persisted.next_offset = actual_len + 32;
            persisted.next_ordinal = 3;
            persisted
                .checkpoints
                .get_mut(&keep_consumer.0)
                .unwrap()
                .upto_offset
                .segment_id = 9;
            persisted.checkpoints.insert(
                drop_consumer.0.clone(),
                RecorderCheckpoint {
                    consumer: drop_consumer.clone(),
                    upto_offset: RecorderOffset {
                        segment_id: 9,
                        byte_offset: actual_len + 16,
                        ordinal: 2,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 11,
                },
            );
            state_file.write(&persisted).unwrap();
            drop(state_file);

            let reopened = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            let keep = reopened
                .read_checkpoint(&keep_consumer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(keep.upto_offset.ordinal, 0);
            assert_eq!(keep.upto_offset.segment_id, 0);
            assert!(
                reopened
                    .read_checkpoint(&drop_consumer)
                    .await
                    .unwrap()
                    .is_none(),
                "checkpoint beyond recovered log head should be discarded"
            );

            let resp = reopened
                .append_batch(AppendRequest {
                    batch_id: "b2".to_string(),
                    events: vec![sample_event("e3", 1, 2, "c")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            assert_eq!(resp.first_offset.byte_offset, actual_len);
            assert_eq!(resp.first_offset.ordinal, 2);
            assert_eq!(resp.first_offset.segment_id, 0);
        });
    }

    // ── Error classification ─────────────────────────────────────────

    #[test]
    fn error_class_mapping() {
        let err = RecorderStorageError::QueueFull { capacity: 10 };
        assert_eq!(err.class(), RecorderStorageErrorClass::Overload);

        let err = RecorderStorageError::InvalidRequest {
            message: "bad".to_string(),
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);

        let err = RecorderStorageError::CheckpointRegression {
            consumer: "c".to_string(),
            current_ordinal: 5,
            attempted_ordinal: 3,
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);

        let err = RecorderStorageError::IdempotencyConflict {
            batch_id: "reused".to_string(),
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);

        let err = RecorderStorageError::CorruptRecord {
            offset: 100,
            reason: "bad crc".to_string(),
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::Corruption);

        let err = RecorderStorageError::CorruptCachedReceiptEncoding {
            batch_id: "corrupt".to_string(),
            reason: "invalid receipt JSON".to_string(),
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::Corruption);

        let err = RecorderStorageError::BackendSelection(
            select_recorder_backend(RecorderBackendKind::FrankenSqlite).unwrap_err(),
        );
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalConfig);

        let err = RecorderStorageError::BackendUnavailable {
            backend: RecorderBackendKind::Rusqlite,
            message: "dependency is temporarily unavailable".to_string(),
        };
        assert_eq!(
            err.class(),
            RecorderStorageErrorClass::DependencyUnavailable
        );

        let err = RecorderStorageError::BackendIdentityMismatch {
            operation: "append_batch",
            expected_backend: RecorderBackendKind::Rusqlite,
            actual_backend: RecorderBackendKind::AppendLog,
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::Corruption);
    }

    #[test]
    fn blocking_failure_mapping_is_structured_and_classified() {
        use crate::runtime_async::SpawnBlockingWithCxError;

        let cases = [
            (
                SpawnBlockingWithCxError::CancelledBeforeSpawn {
                    kind: Some(crate::outcome::CancelKind::User),
                },
                RecorderBlockingFailure::CancelledBeforeStart {
                    kind: Some(crate::outcome::CancelKind::User),
                },
                RecorderStorageErrorClass::Retryable,
            ),
            (
                SpawnBlockingWithCxError::CancelledMidFlight {
                    kind: Some(crate::outcome::CancelKind::Deadline),
                },
                RecorderBlockingFailure::CancelledMidFlight {
                    kind: Some(crate::outcome::CancelKind::Deadline),
                },
                RecorderStorageErrorClass::Retryable,
            ),
            (
                SpawnBlockingWithCxError::RuntimeFailure,
                RecorderBlockingFailure::RuntimeFailure,
                RecorderStorageErrorClass::DependencyUnavailable,
            ),
            (
                SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
                RecorderBlockingFailure::CancellationWatcherTimerFailure,
                RecorderStorageErrorClass::Retryable,
            ),
        ];

        for (source, expected_failure, expected_class) in cases {
            let error = recorder_blocking_error("flush", source);
            assert_eq!(error.class(), expected_class);
            let RecorderStorageError::BlockingOperation { operation, failure } = error else {
                panic!("blocking error mapping returned a non-blocking variant");
            };
            assert_eq!(operation, "flush");
            assert_eq!(failure, expected_failure);
            let json = serde_json::to_string(&failure).unwrap();
            assert_eq!(
                serde_json::from_str::<RecorderBlockingFailure>(&json).unwrap(),
                failure
            );
        }
    }

    #[test]
    fn owned_result_delivery_gate_rejects_success_for_cancelled_caller() {
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("secret post-admission cancellation reason"),
        );

        for operation in ["append_batch", "commit_checkpoint", "flush"] {
            let error = deliver_owned_blocking_result(operation, &cx, Ok(7_u8))
                .expect_err("cancelled caller must not receive settled success");
            let display = error.to_string();
            assert!(matches!(
                error,
                RecorderStorageError::BlockingOperation {
                    operation: actual_operation,
                    failure: RecorderBlockingFailure::CancelledMidFlight {
                        kind: Some(crate::outcome::CancelKind::User)
                    }
                } if actual_operation == operation
            ));
            assert!(
                !display.contains("secret post-admission cancellation reason"),
                "delivery gate leaked its caller-provided cancellation reason"
            );
        }

        let storage_error = deliver_owned_blocking_result::<u8>(
            "append_batch",
            &cx,
            Err(RecorderStorageError::InvalidRequest {
                message: "specific storage failure".to_string(),
            }),
        )
        .expect_err("specific storage failure must remain non-success");
        assert!(matches!(
            storage_error,
            RecorderStorageError::InvalidRequest { message }
                if message == "specific storage failure"
        ));
    }

    // ── Backend kind ─────────────────────────────────────────────────

    #[test]
    fn backend_kind_is_append_log() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            assert_eq!(storage.backend_kind(), RecorderBackendKind::AppendLog);
        });
    }

    // ── Serde roundtrips ─────────────────────────────────────────────

    #[test]
    fn recorder_offset_serde_roundtrip() {
        let offset = RecorderOffset {
            segment_id: 1,
            byte_offset: 1024,
            ordinal: 42,
        };
        let json = serde_json::to_string(&offset).unwrap();
        let back: RecorderOffset = serde_json::from_str(&json).unwrap();
        assert_eq!(back, offset);
    }

    #[test]
    fn health_serde_roundtrip() {
        let health = RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded: false,
            queue_depth: 2,
            queue_capacity: 16,
            latest_offset: Some(RecorderOffset {
                segment_id: 0,
                byte_offset: 512,
                ordinal: 10,
            }),
            last_error: None,
        };
        let json = serde_json::to_string(&health).unwrap();
        let back: RecorderStorageHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(back, health);
    }

    #[test]
    fn lag_metrics_serde_roundtrip() {
        let lag = RecorderStorageLag {
            latest_offset: Some(RecorderOffset {
                segment_id: 0,
                byte_offset: 1000,
                ordinal: 50,
            }),
            consumers: vec![RecorderConsumerLag {
                consumer: CheckpointConsumerId("idx".to_string()),
                offsets_behind: 5,
            }],
        };
        let json = serde_json::to_string(&lag).unwrap();
        let back: RecorderStorageLag = serde_json::from_str(&json).unwrap();
        assert_eq!(back, lag);
    }

    #[test]
    fn flush_stats_serde_roundtrip() {
        let stats = FlushStats {
            backend: RecorderBackendKind::Rusqlite,
            flushed_at_ms: 123456,
            latest_offset: None,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: FlushStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back, stats);
    }

    // ── Multi-batch ordering ─────────────────────────────────────────

    #[test]
    fn multi_batch_accepted_count_correct() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![
                        sample_event("e1", 1, 0, "a"),
                        sample_event("e2", 1, 1, "b"),
                        sample_event("e3", 1, 2, "c"),
                        sample_event("e4", 1, 3, "d"),
                    ],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            assert_eq!(resp.accepted_count, 4);
            assert_eq!(resp.first_offset.ordinal, 0);
            assert_eq!(resp.last_offset.ordinal, 3);
        });
    }

    // ── Open with empty data file ────────────────────────────────────

    #[test]
    fn open_with_empty_data_file() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            // Create empty data file
            std::fs::create_dir_all(cfg.data_path.parent().unwrap()).unwrap();
            std::fs::write(&cfg.data_path, []).unwrap();

            let storage = AppendLogRecorderStorage::open(cfg).unwrap();
            let h = storage.health().await;
            assert!(h.latest_offset.is_none());
        });
    }

    // ── Lag with no consumers ────────────────────────────────────────

    #[test]
    fn lag_with_no_consumers() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let lag = storage.lag_metrics().await.unwrap();
            assert!(lag.consumers.is_empty());
            assert!(lag.latest_offset.is_some());
        });
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn backend_kind_serde_roundtrip_all_variants() {
        for backend in RecorderBackendKind::ALL {
            let json = serde_json::to_string(&backend).unwrap();
            let back: RecorderBackendKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, backend);
        }
        assert_eq!(
            serde_json::to_string(&RecorderBackendKind::AppendLog).unwrap(),
            "\"append_log\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderBackendKind::Rusqlite).unwrap(),
            "\"rusqlite\""
        );
        assert_eq!(
            serde_json::to_string(&RecorderBackendKind::FrankenSqlite).unwrap(),
            "\"frankensqlite\""
        );
    }

    #[test]
    fn durability_level_serde_roundtrip_all_variants() {
        for level in [
            DurabilityLevel::Enqueued,
            DurabilityLevel::Appended,
            DurabilityLevel::Fsync,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: DurabilityLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
        // Verify snake_case rename
        assert!(
            serde_json::to_string(&DurabilityLevel::Enqueued)
                .unwrap()
                .contains("enqueued")
        );
        assert!(
            serde_json::to_string(&DurabilityLevel::Appended)
                .unwrap()
                .contains("appended")
        );
        assert!(
            serde_json::to_string(&DurabilityLevel::Fsync)
                .unwrap()
                .contains("fsync")
        );
    }

    #[test]
    fn flush_mode_serde_roundtrip_both_variants() {
        for mode in [FlushMode::Buffered, FlushMode::Durable] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: FlushMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, mode);
        }
        assert!(
            serde_json::to_string(&FlushMode::Buffered)
                .unwrap()
                .contains("buffered")
        );
        assert!(
            serde_json::to_string(&FlushMode::Durable)
                .unwrap()
                .contains("durable")
        );
    }

    #[test]
    fn checkpoint_commit_outcome_serde_roundtrip() {
        for outcome in [
            CheckpointCommitOutcome::Advanced,
            CheckpointCommitOutcome::NoopAlreadyAdvanced,
            CheckpointCommitOutcome::RejectedOutOfOrder,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: CheckpointCommitOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, outcome);
        }
    }

    #[test]
    fn error_class_serde_roundtrip_all_variants() {
        for class in [
            RecorderStorageErrorClass::Retryable,
            RecorderStorageErrorClass::Overload,
            RecorderStorageErrorClass::TerminalConfig,
            RecorderStorageErrorClass::TerminalData,
            RecorderStorageErrorClass::Corruption,
            RecorderStorageErrorClass::DependencyUnavailable,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            let back: RecorderStorageErrorClass = serde_json::from_str(&json).unwrap();
            assert_eq!(back, class);
        }
    }

    #[test]
    fn checkpoint_consumer_id_serde_roundtrip() {
        let id = CheckpointConsumerId("my-consumer-v2".to_string());
        let json = serde_json::to_string(&id).unwrap();
        let back: CheckpointConsumerId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn recorder_checkpoint_serde_roundtrip() {
        let cp = RecorderCheckpoint {
            consumer: CheckpointConsumerId("indexer".to_string()),
            upto_offset: RecorderOffset {
                segment_id: 3,
                byte_offset: 8192,
                ordinal: 100,
            },
            schema_version: "ft.recorder.event.v1".to_string(),
            committed_at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: RecorderCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cp);
    }

    #[test]
    fn append_response_serde_roundtrip() {
        let resp = AppendResponse {
            backend: RecorderBackendKind::AppendLog,
            accepted_count: 3,
            first_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            last_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 512,
                ordinal: 2,
            },
            committed_durability: DurabilityLevel::Appended,
            committed_at_ms: 1_700_000_000_000,
            was_idempotent_replay: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: AppendResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);

        let mut legacy = serde_json::to_value(&resp).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("was_idempotent_replay");
        let legacy_back: AppendResponse = serde_json::from_value(legacy).unwrap();
        assert!(!legacy_back.was_idempotent_replay);
    }

    #[test]
    fn error_display_formatting() {
        let err = RecorderStorageError::QueueFull { capacity: 128 };
        let msg = format!("{}", err);
        assert!(msg.contains("128"), "expected capacity in message: {}", msg);

        let err = RecorderStorageError::InvalidRequest {
            message: "bad batch".to_string(),
        };
        let msg = format!("{}", err);
        assert!(
            msg.contains("bad batch"),
            "expected detail in message: {}",
            msg
        );

        let err = RecorderStorageError::CheckpointRegression {
            consumer: "idx".to_string(),
            current_ordinal: 10,
            attempted_ordinal: 5,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("idx"), "expected consumer in message: {}", msg);
        assert!(
            msg.contains("10"),
            "expected current ordinal in message: {}",
            msg
        );
        assert!(
            msg.contains("5"),
            "expected attempted ordinal in message: {}",
            msg
        );

        let err = RecorderStorageError::CorruptRecord {
            offset: 42,
            reason: "truncated".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("42"), "expected offset in message: {}", msg);
        assert!(
            msg.contains("truncated"),
            "expected reason in message: {}",
            msg
        );
    }

    #[test]
    fn io_error_class_is_retryable() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        let err = RecorderStorageError::Io(io_err);
        assert_eq!(err.class(), RecorderStorageErrorClass::Retryable);
    }

    #[test]
    fn json_error_class_is_terminal_data() {
        let json_err: serde_json::Error =
            serde_json::from_str::<RecorderOffset>("bad json").unwrap_err();
        let err = RecorderStorageError::Json(json_err);
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
    }

    #[test]
    fn default_config_values() {
        let cfg = AppendLogStorageConfig::default();
        assert_eq!(cfg.queue_capacity, 1024);
        assert_eq!(cfg.max_batch_events, 256);
        assert_eq!(cfg.max_batch_bytes, 256 * 1024);
        assert_eq!(cfg.max_idempotency_entries, 4096);
        assert!(cfg.data_path.to_str().unwrap().contains("events.log"));
        assert!(cfg.state_path.to_str().unwrap().contains("state.json"));
    }

    #[test]
    fn multiple_consumers_checkpoint_advance() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            // Commit checkpoints for three consumers
            for (name, ordinal) in [("alpha", 2u64), ("beta", 5), ("gamma", 10)] {
                let outcome = storage
                    .commit_checkpoint(RecorderCheckpoint {
                        consumer: CheckpointConsumerId(name.to_string()),
                        upto_offset: RecorderOffset {
                            segment_id: 0,
                            byte_offset: 0,
                            ordinal,
                        },
                        schema_version: "v1".to_string(),
                        committed_at_ms: 100,
                    })
                    .await
                    .unwrap();
                assert_eq!(outcome, CheckpointCommitOutcome::Advanced);
            }

            // Advance beta from 5 to 8
            let outcome = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId("beta".to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 8,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 200,
                })
                .await
                .unwrap();
            assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

            // Read back and verify
            let beta = storage
                .read_checkpoint(&CheckpointConsumerId("beta".to_string()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(beta.upto_offset.ordinal, 8);

            let alpha = storage
                .read_checkpoint(&CheckpointConsumerId("alpha".to_string()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(alpha.upto_offset.ordinal, 2);
        });
    }

    #[test]
    fn lag_metrics_empty_store() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let lag = storage.lag_metrics().await.unwrap();
            assert!(lag.latest_offset.is_none());
            assert!(lag.consumers.is_empty());
        });
    }

    #[test]
    fn flush_empty_store() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let stats = storage.flush(FlushMode::Buffered).await.unwrap();
            assert_eq!(stats.backend, RecorderBackendKind::AppendLog);
            assert!(stats.latest_offset.is_none());

            let stats_dur = storage.flush(FlushMode::Durable).await.unwrap();
            assert!(stats_dur.latest_offset.is_none());
        });
    }

    #[test]
    fn single_event_accepted_count_is_one() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "only-one")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            assert_eq!(resp.accepted_count, 1);
            assert_eq!(resp.first_offset, resp.last_offset);
        });
    }

    #[test]
    fn byte_offset_monotonicity_across_batches() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let mut offsets = Vec::new();
            for i in 0..5u64 {
                let resp = storage
                    .append_batch(AppendRequest {
                        batch_id: format!("b{}", i),
                        events: vec![sample_event(&format!("e{}", i), 1, i, "payload")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: i,
                    })
                    .await
                    .unwrap();
                offsets.push(resp.first_offset.byte_offset);
            }

            // Byte offsets must be strictly increasing
            for window in offsets.windows(2) {
                assert!(
                    window[1] > window[0],
                    "byte offsets not strictly increasing: {} <= {}",
                    window[1],
                    window[0]
                );
            }
        });
    }

    #[test]
    fn reopen_preserves_checkpoints_across_restart() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            {
                let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
                // Write some data
                let _ = storage
                    .append_batch(AppendRequest {
                        batch_id: "b1".to_string(),
                        events: vec![sample_event("e1", 1, 0, "data")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();
                // Commit a checkpoint
                let _ = storage
                    .commit_checkpoint(RecorderCheckpoint {
                        consumer: CheckpointConsumerId("persist-test".to_string()),
                        upto_offset: RecorderOffset {
                            segment_id: 0,
                            byte_offset: 0,
                            ordinal: 0,
                        },
                        schema_version: "v1".to_string(),
                        committed_at_ms: 500,
                    })
                    .await
                    .unwrap();
            }

            // Reopen and verify checkpoint is still there
            let storage2 = AppendLogRecorderStorage::open(cfg).unwrap();
            let cp = storage2
                .read_checkpoint(&CheckpointConsumerId("persist-test".to_string()))
                .await
                .unwrap();
            assert!(cp.is_some());
            let cp = cp.unwrap();
            assert_eq!(cp.upto_offset.ordinal, 0);
            assert_eq!(cp.schema_version, "v1");
            assert_eq!(cp.committed_at_ms, 500);
        });
    }

    #[test]
    fn health_with_latest_offset_after_multi_event_batch() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![
                        sample_event("e1", 1, 0, "a"),
                        sample_event("e2", 1, 1, "b"),
                        sample_event("e3", 1, 2, "c"),
                    ],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let h = storage.health().await;
            let latest = h.latest_offset.unwrap();
            // latest_offset reflects the last written ordinal
            assert_eq!(latest.ordinal, 2);
            assert!(!h.degraded);
            assert_eq!(h.queue_depth, 0);
        });
    }

    #[test]
    fn appended_durability_persists_state() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());
            let state_path = cfg.state_path.clone();
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();

            // Before any append, state file should not exist (or be empty)
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            // Appended durability should have written state file
            assert!(state_path.exists());
            let bytes = std::fs::read(&state_path).unwrap();
            assert_ne!(bytes, [] as [u8; 0]);

            // Verify state content is valid JSON
            let state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(state.get("next_ordinal").is_some());
            assert_eq!(state["next_ordinal"], 1);
        });
    }

    #[test]
    fn response_backend_always_append_log() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            for i in 0..3u64 {
                let resp = storage
                    .append_batch(AppendRequest {
                        batch_id: format!("b{}", i),
                        events: vec![sample_event(&format!("e{}", i), 1, i, "data")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: i,
                    })
                    .await
                    .unwrap();
                assert_eq!(resp.backend, RecorderBackendKind::AppendLog);
            }
        });
    }

    #[test]
    fn committed_at_ms_is_nonzero() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            assert!(
                resp.committed_at_ms > 0,
                "committed_at_ms should be a valid epoch timestamp"
            );
        });
    }

    #[test]
    fn health_with_last_error_is_degraded() {
        // Construct a RecorderStorageHealth with last_error set, verify degraded semantics
        let h = RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded: true,
            queue_depth: 0,
            queue_capacity: 128,
            latest_offset: None,
            last_error: Some("disk full".to_string()),
        };
        assert!(h.degraded);
        assert_eq!(h.last_error.as_deref(), Some("disk full"));

        let h2 = RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded: false,
            queue_depth: 0,
            queue_capacity: 128,
            latest_offset: None,
            last_error: None,
        };
        assert!(!h2.degraded);
        assert!(h2.last_error.is_none());
    }

    #[test]
    fn health_serde_roundtrip_with_error() {
        let health = RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded: true,
            queue_depth: 5,
            queue_capacity: 128,
            latest_offset: Some(RecorderOffset {
                segment_id: 1,
                byte_offset: 4096,
                ordinal: 20,
            }),
            last_error: Some("disk full".to_string()),
        };
        let json = serde_json::to_string(&health).unwrap();
        let back: RecorderStorageHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(back, health);
        assert!(back.degraded);
        assert_eq!(back.last_error.as_deref(), Some("disk full"));
    }

    #[test]
    fn consumer_lag_serde_roundtrip() {
        let lag = RecorderConsumerLag {
            consumer: CheckpointConsumerId("search-indexer".to_string()),
            offsets_behind: 42,
        };
        let json = serde_json::to_string(&lag).unwrap();
        let back: RecorderConsumerLag = serde_json::from_str(&json).unwrap();
        assert_eq!(back, lag);
    }

    #[test]
    fn torn_tail_with_only_length_prefix_no_payload() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            // Write just a 4-byte length prefix with no payload following
            std::fs::create_dir_all(cfg.data_path.parent().unwrap()).unwrap();
            {
                let mut file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&cfg.data_path)
                    .unwrap();
                // Write length prefix claiming 100 bytes, but provide nothing
                file.write_all(&(100u32).to_le_bytes()).unwrap();
                file.flush().unwrap();
            }

            // Open should truncate the torn tail (incomplete record)
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            let recovered_len = std::fs::metadata(&cfg.data_path).unwrap().len();
            assert_eq!(recovered_len, 0, "torn partial record should be truncated");

            // Should be able to append starting at ordinal 0
            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "fresh")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            assert_eq!(resp.first_offset.ordinal, 0);
            assert_eq!(resp.first_offset.byte_offset, 0);
        });
    }

    #[test]
    fn segment_id_preserved_across_reopen() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());

            {
                let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
                let resp = storage
                    .append_batch(AppendRequest {
                        batch_id: "b1".to_string(),
                        events: vec![sample_event("e1", 1, 0, "seg-test")],
                        required_durability: DurabilityLevel::Appended,
                        producer_ts_ms: 1,
                    })
                    .await
                    .unwrap();
                // Default segment_id is 0
                assert_eq!(resp.first_offset.segment_id, 0);
            }

            // Reopen: segment_id from persisted state
            let storage2 = AppendLogRecorderStorage::open(cfg).unwrap();
            let resp2 = storage2
                .append_batch(AppendRequest {
                    batch_id: "b2".to_string(),
                    events: vec![sample_event("e2", 1, 1, "seg-test-2")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            assert_eq!(resp2.first_offset.segment_id, 0);
        });
    }

    #[test]
    fn flush_updates_flushed_at_ms() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let stats = storage.flush(FlushMode::Buffered).await.unwrap();
            assert!(
                stats.flushed_at_ms > 0,
                "flushed_at_ms should be a valid epoch timestamp"
            );

            let stats2 = storage.flush(FlushMode::Durable).await.unwrap();
            assert!(
                stats2.flushed_at_ms >= stats.flushed_at_ms,
                "subsequent flush timestamp should be >= previous"
            );
        });
    }

    // ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

    #[test]
    fn whitespace_only_batch_id_rejected() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let err = storage
                .append_batch(AppendRequest {
                    batch_id: "   \t  ".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap_err();
            assert!(
                matches!(err, RecorderStorageError::InvalidRequest { .. }),
                "whitespace-only batch_id should be rejected"
            );
            assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
        });
    }

    #[test]
    fn corrupt_record_error_class_is_corruption() {
        let err = RecorderStorageError::CorruptRecord {
            offset: 1024,
            reason: "bad CRC".to_string(),
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::Corruption);
        let msg = format!("{err}");
        assert!(msg.contains("1024"), "display should contain offset");
        assert!(msg.contains("bad CRC"), "display should contain reason");
    }

    #[test]
    fn queue_full_error_class_is_overload() {
        let err = RecorderStorageError::QueueFull { capacity: 64 };
        assert_eq!(err.class(), RecorderStorageErrorClass::Overload);
        let msg = format!("{err}");
        assert!(msg.contains("64"), "display should contain capacity");
    }

    #[test]
    fn checkpoint_regression_error_display() {
        let err = RecorderStorageError::CheckpointRegression {
            consumer: "indexer".to_string(),
            current_ordinal: 100,
            attempted_ordinal: 50,
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
        let msg = format!("{err}");
        assert!(msg.contains("indexer"));
        assert!(msg.contains("100"));
        assert!(msg.contains("50"));
    }

    #[test]
    fn default_config_has_expected_paths_and_limits() {
        let cfg = AppendLogStorageConfig::default();
        assert!(cfg.data_path.to_str().unwrap().contains("events.log"));
        assert!(cfg.state_path.to_str().unwrap().contains("state.json"));
        assert_eq!(cfg.queue_capacity, 1024);
        assert_eq!(cfg.max_batch_events, 256);
        assert_eq!(cfg.max_batch_bytes, 256 * 1024);
        assert_eq!(cfg.max_idempotency_entries, 4096);
        cfg.validate().expect("default config should be valid");
    }

    #[test]
    fn checkpoint_noop_when_same_ordinal() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let checkpoint = RecorderCheckpoint {
                consumer: CheckpointConsumerId("c1".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 100,
            };
            let outcome = storage.commit_checkpoint(checkpoint.clone()).await.unwrap();
            assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

            // Same ordinal → noop
            let outcome2 = storage.commit_checkpoint(checkpoint).await.unwrap();
            assert_eq!(outcome2, CheckpointCommitOutcome::NoopAlreadyAdvanced);
        });
    }

    #[test]
    fn lag_consumers_sorted_alphabetically() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            // Commit checkpoints for consumers in non-alphabetical order
            for name in &["zebra", "alpha", "middle"] {
                storage
                    .commit_checkpoint(RecorderCheckpoint {
                        consumer: CheckpointConsumerId(name.to_string()),
                        upto_offset: RecorderOffset {
                            segment_id: 0,
                            byte_offset: 0,
                            ordinal: 0,
                        },
                        schema_version: "v1".to_string(),
                        committed_at_ms: 100,
                    })
                    .await
                    .unwrap();
            }

            let lag = storage.lag_metrics().await.unwrap();
            let names: Vec<&str> = lag
                .consumers
                .iter()
                .map(|c| c.consumer.0.as_str())
                .collect();
            assert_eq!(names, vec!["alpha", "middle", "zebra"]);
        });
    }

    #[test]
    fn recorder_offset_clone_eq() {
        let offset = RecorderOffset {
            segment_id: 3,
            byte_offset: 1024,
            ordinal: 42,
        };
        let cloned = offset.clone();
        assert_eq!(offset, cloned);
    }

    #[test]
    fn error_class_serde_all_variants() {
        let variants = [
            RecorderStorageErrorClass::Retryable,
            RecorderStorageErrorClass::Overload,
            RecorderStorageErrorClass::TerminalConfig,
            RecorderStorageErrorClass::TerminalData,
            RecorderStorageErrorClass::Corruption,
            RecorderStorageErrorClass::DependencyUnavailable,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: RecorderStorageErrorClass = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn checkpoint_commit_outcome_serde_all_variants() {
        let variants = [
            CheckpointCommitOutcome::Advanced,
            CheckpointCommitOutcome::NoopAlreadyAdvanced,
            CheckpointCommitOutcome::RejectedOutOfOrder,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: CheckpointCommitOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn health_queue_depth_reflects_zero_when_idle() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
            let health = storage.health().await;
            assert_eq!(health.queue_depth, 0);
            assert!(!health.degraded);
            assert!(health.last_error.is_none());
            assert!(health.latest_offset.is_none());
        });
    }

    #[test]
    fn recorder_storage_lag_serde_roundtrip() {
        let lag = RecorderStorageLag {
            latest_offset: Some(RecorderOffset {
                segment_id: 0,
                byte_offset: 500,
                ordinal: 10,
            }),
            consumers: vec![
                RecorderConsumerLag {
                    consumer: CheckpointConsumerId("a".to_string()),
                    offsets_behind: 3,
                },
                RecorderConsumerLag {
                    consumer: CheckpointConsumerId("b".to_string()),
                    offsets_behind: 7,
                },
            ],
        };
        let json = serde_json::to_string(&lag).unwrap();
        let back: RecorderStorageLag = serde_json::from_str(&json).unwrap();
        assert_eq!(back.consumers.len(), 2);
        assert_eq!(back.consumers[0].offsets_behind, 3);
        assert_eq!(back.consumers[1].offsets_behind, 7);
        assert_eq!(back.latest_offset.unwrap().ordinal, 10);
    }

    // =========================================================================
    // RecorderEventReader / RecorderEventCursor / CursorRecord trait tests
    // =========================================================================

    #[test]
    fn cursor_record_clone_and_debug() {
        let record = CursorRecord {
            event: sample_event("cr-1", 1, 0, "test"),
            offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
        };
        let cloned = record.clone();
        assert_eq!(cloned.event.event_id, "cr-1");
        assert_eq!(cloned.offset.ordinal, 0);
        let dbg = format!("{:?}", record);
        assert!(dbg.contains("CursorRecord"));
    }

    #[test]
    fn event_cursor_error_display_io() {
        let err = EventCursorError::Io("disk full".to_string());
        let msg = err.to_string();
        assert!(msg.contains("disk full"));
        assert!(msg.contains("I/O"));
    }

    #[test]
    fn event_cursor_error_display_corrupt() {
        let err = EventCursorError::Corrupt {
            offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 42,
                ordinal: 7,
            },
            reason: "bad CRC".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("42"));
        assert!(msg.contains("bad CRC"));
    }

    #[test]
    fn event_cursor_error_display_unavailable() {
        let err = EventCursorError::Unavailable("shutting down".to_string());
        let msg = err.to_string();
        assert!(msg.contains("shutting down"));
        assert!(msg.contains("unavailable"));
    }

    #[test]
    fn event_cursor_error_is_std_error() {
        let err = EventCursorError::Io("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    /// Mock event reader for testing the trait ergonomics.
    struct MockEventReader {
        records: Vec<CursorRecord>,
    }

    impl RecorderEventReader for MockEventReader {
        fn open_cursor(
            &self,
            from: RecorderOffset,
        ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
            let start = from.ordinal as usize;
            let remaining: Vec<_> = self
                .records
                .iter()
                .filter(|r| r.offset.ordinal >= start as u64)
                .cloned()
                .collect();
            Ok(Box::new(MockCursor {
                records: remaining,
                pos: 0,
            }))
        }

        fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
            Ok(self
                .records
                .last()
                .map(|r| RecorderOffset {
                    segment_id: 0,
                    byte_offset: r.offset.byte_offset + 1,
                    ordinal: r.offset.ordinal + 1,
                })
                .unwrap_or(RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                }))
        }
    }

    struct MockCursor {
        records: Vec<CursorRecord>,
        pos: usize,
    }

    impl RecorderEventCursor for MockCursor {
        fn next_batch(
            &mut self,
            max: usize,
        ) -> std::result::Result<Vec<CursorRecord>, EventCursorError> {
            let end = (self.pos + max).min(self.records.len());
            let batch = self.records[self.pos..end].to_vec();
            self.pos = end;
            Ok(batch)
        }

        fn current_offset(&self) -> RecorderOffset {
            if self.pos < self.records.len() {
                self.records[self.pos].offset.clone()
            } else {
                self.records
                    .last()
                    .map(|r| RecorderOffset {
                        segment_id: 0,
                        byte_offset: r.offset.byte_offset + 1,
                        ordinal: r.offset.ordinal + 1,
                    })
                    .unwrap_or(RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    })
            }
        }
    }

    #[test]
    fn mock_event_reader_open_cursor_from_start() {
        let records: Vec<CursorRecord> = (0..3)
            .map(|i| CursorRecord {
                event: sample_event(&format!("mock-{i}"), 1, i, "x"),
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: i * 100,
                    ordinal: i,
                },
            })
            .collect();
        let reader = MockEventReader { records };

        let mut cursor = reader.open_cursor_from_start().unwrap();
        let batch = cursor.next_batch(10).unwrap();
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].event.event_id, "mock-0");
    }

    #[test]
    fn mock_event_reader_open_cursor_at_offset() {
        let records: Vec<CursorRecord> = (0..5)
            .map(|i| CursorRecord {
                event: sample_event(&format!("mock-{i}"), 1, i, "x"),
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: i * 100,
                    ordinal: i,
                },
            })
            .collect();
        let reader = MockEventReader { records };

        let mut cursor = reader
            .open_cursor(RecorderOffset {
                segment_id: 0,
                byte_offset: 200,
                ordinal: 2,
            })
            .unwrap();
        let batch = cursor.next_batch(10).unwrap();
        assert_eq!(batch.len(), 3); // records 2, 3, 4
        assert_eq!(batch[0].event.event_id, "mock-2");
    }

    #[test]
    fn mock_cursor_batch_limits() {
        let records: Vec<CursorRecord> = (0..5)
            .map(|i| CursorRecord {
                event: sample_event(&format!("mock-{i}"), 1, i, "x"),
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: i * 100,
                    ordinal: i,
                },
            })
            .collect();
        let reader = MockEventReader { records };
        let mut cursor = reader.open_cursor_from_start().unwrap();

        let b1 = cursor.next_batch(2).unwrap();
        assert_eq!(b1.len(), 2);

        let b2 = cursor.next_batch(2).unwrap();
        assert_eq!(b2.len(), 2);

        let b3 = cursor.next_batch(2).unwrap();
        assert_eq!(b3.len(), 1);

        let b4 = cursor.next_batch(2).unwrap();
        assert!(b4.is_empty());
    }

    #[test]
    fn mock_cursor_offset_advances() {
        let records: Vec<CursorRecord> = (0..3)
            .map(|i| CursorRecord {
                event: sample_event(&format!("mock-{i}"), 1, i, "x"),
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: i * 100,
                    ordinal: i,
                },
            })
            .collect();
        let reader = MockEventReader { records };
        let mut cursor = reader.open_cursor_from_start().unwrap();

        assert_eq!(cursor.current_offset().ordinal, 0);
        let _ = cursor.next_batch(2).unwrap();
        assert_eq!(cursor.current_offset().ordinal, 2);
        let _ = cursor.next_batch(10).unwrap();
        assert_eq!(cursor.current_offset().ordinal, 3);
    }

    #[test]
    fn mock_head_offset_empty_reader() {
        let reader = MockEventReader { records: vec![] };
        let head = reader.head_offset().unwrap();
        assert_eq!(head.ordinal, 0);
        assert_eq!(head.byte_offset, 0);
    }

    #[test]
    fn mock_head_offset_with_records() {
        let records: Vec<CursorRecord> = (0..5)
            .map(|i| CursorRecord {
                event: sample_event(&format!("mock-{i}"), 1, i, "x"),
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: i * 100,
                    ordinal: i,
                },
            })
            .collect();
        let reader = MockEventReader { records };
        let head = reader.head_offset().unwrap();
        assert_eq!(head.ordinal, 5);
        assert_eq!(head.byte_offset, 401);
    }

    // =========================================================================
    // RecorderSourceDescriptor tests
    // =========================================================================

    #[test]
    fn source_descriptor_append_log_display() {
        let desc = RecorderSourceDescriptor::AppendLog {
            data_path: PathBuf::from("/tmp/events.log"),
        };
        let display = desc.to_string();
        assert!(display.contains("append_log"));
        assert!(display.contains("/tmp/events.log"));
    }

    #[test]
    fn source_descriptor_rusqlite_display() {
        let desc = RecorderSourceDescriptor::Rusqlite {
            db_path: PathBuf::from("/tmp/recorder.db"),
        };
        let display = desc.to_string();
        assert!(display.contains("rusqlite"));
        assert!(display.contains("/tmp/recorder.db"));
    }

    #[test]
    fn source_descriptor_backend_kind() {
        let al = RecorderSourceDescriptor::AppendLog {
            data_path: PathBuf::from("x"),
        };
        assert_eq!(al.backend_kind(), RecorderBackendKind::AppendLog);

        let sqlite = RecorderSourceDescriptor::Rusqlite {
            db_path: PathBuf::from("y"),
        };
        assert_eq!(sqlite.backend_kind(), RecorderBackendKind::Rusqlite);
    }

    #[test]
    fn source_descriptor_serde_roundtrip_append_log() {
        let desc = RecorderSourceDescriptor::AppendLog {
            data_path: PathBuf::from("/data/events.log"),
        };
        let json = serde_json::to_string(&desc).unwrap();
        assert!(json.contains("append_log"));
        let back: RecorderSourceDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back, desc);
    }

    #[test]
    fn source_descriptor_serde_roundtrip_rusqlite() {
        let desc = RecorderSourceDescriptor::Rusqlite {
            db_path: PathBuf::from("/data/recorder.db"),
        };
        let json = serde_json::to_string(&desc).unwrap();
        assert!(json.contains("rusqlite"));
        let back: RecorderSourceDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back, desc);
    }

    #[test]
    fn source_descriptor_clone_and_eq() {
        let desc = RecorderSourceDescriptor::AppendLog {
            data_path: PathBuf::from("events.log"),
        };
        let cloned = desc.clone();
        assert_eq!(desc, cloned);

        let other = RecorderSourceDescriptor::Rusqlite {
            db_path: PathBuf::from("db.sqlite"),
        };
        assert_ne!(desc, other);
    }

    #[test]
    fn source_descriptor_debug() {
        let desc = RecorderSourceDescriptor::AppendLog {
            data_path: PathBuf::from("events.log"),
        };
        let dbg = format!("{:?}", desc);
        assert!(dbg.contains("AppendLog"));
    }
}
