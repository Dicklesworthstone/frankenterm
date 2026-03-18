//! Recorder storage abstraction and append-log backend.
//!
//! This module implements the `wa-oegrb.3.2` hot-path baseline:
//! - append-only batched writes with deterministic offsets
//! - bounded in-flight admission and explicit overload signaling
//! - idempotent `batch_id` handling
//! - persisted writer/checkpoint state and torn-tail recovery
//!
//! ## Write/checkpoint invariant contract
//! - `append_batch` is append-only: accepted records advance `next_offset` and `next_ordinal`
//!   monotonically and never rewrite prior bytes.
//! - `batch_id` is idempotent within retention bounds: replaying an existing `batch_id`
//!   returns the original `AppendResponse` without duplicating writes.
//! - `commit_checkpoint` is monotonic per consumer: lower ordinals are rejected with
//!   `CheckpointRegression`, identical ordinals are `NoopAlreadyAdvanced`, and higher ordinals
//!   are accepted as `Advanced`.
//! - checkpoint state is durable in `state.json` and survives reopen.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::runtime_compat::Mutex;

use crate::recording::RecorderEvent;

/// Stable backend identity for recorder storage implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderBackendKind {
    /// Local append-only log backend.
    AppendLog,
    /// FrankenSQLite-backed backend (implemented in a follow-on bead).
    #[serde(rename = "frankensqlite", alias = "franken_sqlite")]
    FrankenSqlite,
}

impl std::fmt::Display for RecorderBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppendLog => write!(f, "append_log"),
            Self::FrankenSqlite => write!(f, "frankensqlite"),
        }
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
    /// FrankenSqlite database backend.
    FrankenSqlite {
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
            Self::FrankenSqlite { db_path } => {
                write!(f, "frankensqlite({})", db_path.display())
            }
        }
    }
}

impl RecorderSourceDescriptor {
    /// Return the backend kind for this descriptor.
    pub fn backend_kind(&self) -> RecorderBackendKind {
        match self {
            Self::AppendLog { .. } => RecorderBackendKind::AppendLog,
            Self::FrankenSqlite { .. } => RecorderBackendKind::FrankenSqlite,
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
    pub queue_depth: usize,
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

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

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
            Self::InvalidRequest { .. } | Self::CheckpointRegression { .. } => {
                RecorderStorageErrorClass::TerminalData
            }
            Self::CorruptRecord { .. } => RecorderStorageErrorClass::Corruption,
            Self::Io(_) => RecorderStorageErrorClass::Retryable,
            Self::Json(_) => RecorderStorageErrorClass::TerminalData,
            Self::BackendUnavailable { .. } => RecorderStorageErrorClass::DependencyUnavailable,
        }
    }
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

    async fn flush(&self, mode: FlushMode)
    -> std::result::Result<FlushStats, RecorderStorageError>;

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError>;

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError>;

    async fn health(&self) -> RecorderStorageHealth;

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError>;
}

/// Append-log backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppendLogStorageConfig {
    /// Path to append-only data file.
    pub data_path: PathBuf,
    /// Path to persisted writer/checkpoint state.
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

/// Startup-time recorder storage selector/config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecorderStorageConfig {
    /// Requested backend kind for recorder writes.
    pub backend: RecorderBackendKind,
    /// Append-log backend settings.
    pub append_log: AppendLogStorageConfig,
}

impl Default for RecorderStorageConfig {
    fn default() -> Self {
        Self {
            backend: RecorderBackendKind::AppendLog,
            append_log: AppendLogStorageConfig::default(),
        }
    }
}

/// Runtime-selected recorder storage backend.
#[derive(Debug)]
pub enum RecorderStorageInstance {
    AppendLog(AppendLogRecorderStorage),
}

impl RecorderStorage for RecorderStorageInstance {
    fn backend_kind(&self) -> RecorderBackendKind {
        match self {
            Self::AppendLog(inner) => inner.backend_kind(),
        }
    }

    fn append_log_data_path(&self) -> Option<&Path> {
        match self {
            Self::AppendLog(inner) => inner.append_log_data_path(),
        }
    }

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.append_batch(req).await,
        }
    }

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.flush(mode).await,
        }
    }

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.read_checkpoint(consumer).await,
        }
    }

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.commit_checkpoint(checkpoint).await,
        }
    }

    async fn health(&self) -> RecorderStorageHealth {
        match self {
            Self::AppendLog(inner) => inner.health().await,
        }
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        match self {
            Self::AppendLog(inner) => inner.lag_metrics().await,
        }
    }
}

/// Bootstrap recorder storage from startup selector config.
pub fn bootstrap_recorder_storage(
    config: RecorderStorageConfig,
) -> std::result::Result<RecorderStorageInstance, RecorderStorageError> {
    match config.backend {
        RecorderBackendKind::AppendLog => {
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
        RecorderBackendKind::FrankenSqlite => Err(RecorderStorageError::BackendUnavailable {
            backend: RecorderBackendKind::FrankenSqlite,
            message: "frankensqlite backend not yet implemented".to_string(),
        }),
    }
}

/// Baseline append-only recorder backend.
#[derive(Debug)]
pub struct AppendLogRecorderStorage {
    config: AppendLogStorageConfig,
    in_flight: AtomicUsize,
    inner: Arc<Mutex<AppendLogInner>>,
}

#[derive(Debug)]
struct AppendLogInner {
    writer: std::io::BufWriter<File>,
    segment_id: u64,
    next_offset: u64,
    next_ordinal: u64,
    checkpoints: HashMap<String, RecorderCheckpoint>,
    idempotency_cache: HashMap<String, AppendResponse>,
    idempotency_order: VecDeque<String>,
    last_error: Option<String>,
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
    pub fn open(config: AppendLogStorageConfig) -> std::result::Result<Self, RecorderStorageError> {
        config.validate()?;
        ensure_parent_dir(&config.data_path)?;
        ensure_parent_dir(&config.state_path)?;

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&config.data_path)?;

        let scan = scan_valid_prefix(&mut file)?;
        let persisted = load_persisted_state(&config.state_path)?;
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
            segment_id,
            next_offset,
            next_ordinal,
            checkpoints,
            idempotency_cache: HashMap::new(),
            idempotency_order: VecDeque::new(),
            last_error: None,
        };

        Ok(Self {
            config,
            in_flight: AtomicUsize::new(0),
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    fn try_acquire_slot(&self) -> std::result::Result<InFlightGuard<'_>, RecorderStorageError> {
        let current = self.in_flight.load(Ordering::Acquire);
        if current >= self.config.queue_capacity {
            return Err(RecorderStorageError::QueueFull {
                capacity: self.config.queue_capacity,
            });
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        Ok(InFlightGuard {
            counter: &self.in_flight,
        })
    }

    fn persist_state(
        &self,
        inner: &AppendLogInner,
    ) -> std::result::Result<(), RecorderStorageError> {
        let persisted = PersistedState {
            segment_id: inner.segment_id,
            next_offset: inner.next_offset,
            next_ordinal: inner.next_ordinal,
            checkpoints: inner.checkpoints.clone(),
        };
        write_persisted_state(&self.config.state_path, &persisted)
    }

    fn latest_offset(inner: &AppendLogInner) -> Option<RecorderOffset> {
        if inner.next_ordinal == 0 {
            return None;
        }
        Some(RecorderOffset {
            segment_id: inner.segment_id,
            byte_offset: inner.next_offset,
            ordinal: inner.next_ordinal - 1,
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
        inner.last_error = Some(format!(
            "{operation} failed (class={:?}): {err}",
            err.class()
        ));
    }
}

struct InFlightGuard<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
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
        let _slot = self.try_acquire_slot()?;

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

        let mut inner = self.inner.lock().await;
        let AppendRequest {
            batch_id,
            events,
            required_durability,
            producer_ts_ms: _producer_ts_ms,
        } = req;

        if let Some(existing) = inner.idempotency_cache.get(&batch_id).cloned() {
            Self::clear_last_error(&mut inner);
            return Ok(existing);
        }

        let result = (|| -> std::result::Result<AppendResponse, RecorderStorageError> {
            let mut encoded = Vec::with_capacity(events.len());
            let mut total_bytes = 0usize;
            for event in events {
                let payload = serde_json::to_vec(&event)?;
                total_bytes += payload.len() + 4;
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

            let first_offset = RecorderOffset {
                segment_id: inner.segment_id,
                byte_offset: inner.next_offset,
                ordinal: inner.next_ordinal,
            };

            let mut last_offset = first_offset.clone();

            for payload in encoded {
                let payload_len = payload.len();
                if payload_len > u32::MAX as usize {
                    return Err(RecorderStorageError::InvalidRequest {
                        message: format!("record payload too large: {payload_len} bytes"),
                    });
                }

                let record_start = inner.next_offset;
                let ordinal = inner.next_ordinal;
                inner
                    .writer
                    .write_all(&(payload_len as u32).to_le_bytes())?;
                inner.writer.write_all(&payload)?;
                inner.next_offset += 4 + payload_len as u64;
                inner.next_ordinal += 1;
                last_offset = RecorderOffset {
                    segment_id: inner.segment_id,
                    byte_offset: record_start,
                    ordinal,
                };
            }

            match required_durability {
                DurabilityLevel::Enqueued => {}
                DurabilityLevel::Appended => {
                    inner.writer.flush()?;
                    self.persist_state(&inner)?;
                }
                DurabilityLevel::Fsync => {
                    inner.writer.flush()?;
                    inner.writer.get_ref().sync_data()?;
                    self.persist_state(&inner)?;
                }
            }

            let response = AppendResponse {
                backend: RecorderBackendKind::AppendLog,
                accepted_count: last_offset
                    .ordinal
                    .saturating_sub(first_offset.ordinal)
                    .saturating_add(1) as usize,
                first_offset,
                last_offset,
                committed_durability: required_durability,
                committed_at_ms: crate::recording::epoch_ms_now(),
            };

            inner
                .idempotency_cache
                .insert(batch_id.clone(), response.clone());
            inner.idempotency_order.push_back(batch_id);
            while inner.idempotency_cache.len() > self.config.max_idempotency_entries {
                if let Some(evict) = inner.idempotency_order.pop_front() {
                    inner.idempotency_cache.remove(&evict);
                }
            }
            Ok(response)
        })();

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

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        let mut inner = self.inner.lock().await;
        let result = (|| -> std::result::Result<FlushStats, RecorderStorageError> {
            inner.writer.flush()?;
            if mode == FlushMode::Durable {
                inner.writer.get_ref().sync_data()?;
            }
            self.persist_state(&inner)?;
            Ok(FlushStats {
                backend: RecorderBackendKind::AppendLog,
                flushed_at_ms: crate::recording::epoch_ms_now(),
                latest_offset: Self::latest_offset(&inner),
            })
        })();

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
        let mut inner = self.inner.lock().await;
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
                inner.checkpoints.insert(key, checkpoint);
                self.persist_state(&inner)?;
            }

            Ok(outcome)
        })();

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

fn ensure_parent_dir(path: &Path) -> std::result::Result<(), RecorderStorageError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn load_persisted_state(path: &Path) -> std::result::Result<PersistedState, RecorderStorageError> {
    if !path.exists() {
        return Ok(PersistedState::default());
    }
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(PersistedState::default());
    }
    let state = serde_json::from_slice::<PersistedState>(&bytes)?;
    Ok(state)
}

fn write_persisted_state(
    path: &Path,
    state: &PersistedState,
) -> std::result::Result<(), RecorderStorageError> {
    let tmp_path = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(state)?;
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn scan_valid_prefix(file: &mut File) -> std::result::Result<ScanResult, RecorderStorageError> {
    file.seek(SeekFrom::Start(0))?;
    let file_len = file.metadata()?.len();

    let mut offset = 0u64;
    let mut records = 0u64;
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
/// FrankenSqlite, etc.) and expose a cursor-based iteration interface
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
    use tempfile::tempdir;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build recorder_storage test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
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
    fn bootstrap_frankensqlite_reports_dependency_unavailable() {
        let dir = tempdir().unwrap();
        let mut config = recorder_test_config(dir.path());
        config.backend = RecorderBackendKind::FrankenSqlite;

        let err = bootstrap_recorder_storage(config).unwrap_err();
        assert!(matches!(
            err,
            RecorderStorageError::BackendUnavailable {
                backend: RecorderBackendKind::FrankenSqlite,
                ..
            }
        ));
        assert_eq!(
            err.class(),
            RecorderStorageErrorClass::DependencyUnavailable
        );
    }

    #[test]
    fn bootstrap_frankensqlite_ignores_append_log_validation() {
        let dir = tempdir().unwrap();
        let mut config = recorder_test_config(dir.path());
        config.backend = RecorderBackendKind::FrankenSqlite;
        config.append_log.queue_capacity = 0; // invalid for append-log

        let err = bootstrap_recorder_storage(config).unwrap_err();
        assert!(matches!(
            err,
            RecorderStorageError::BackendUnavailable {
                backend: RecorderBackendKind::FrankenSqlite,
                ..
            }
        ));
    }

    #[test]
    fn recorder_storage_config_serde_roundtrip() {
        let dir = tempdir().unwrap();
        let config = recorder_test_config(dir.path());
        let json = serde_json::to_string(&config).unwrap();
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
    }

    #[test]
    fn recorder_storage_config_accepts_legacy_franken_sqlite_alias() {
        let value = serde_json::json!({
            "backend": "franken_sqlite",
            "append_log": AppendLogStorageConfig::default()
        });
        let parsed: RecorderStorageConfig = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.backend, RecorderBackendKind::FrankenSqlite);
    }

    #[test]
    fn backend_kind_display_is_snake_case() {
        assert_eq!(
            RecorderBackendKind::AppendLog.to_string(),
            "append_log".to_string()
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

    #[test]
    fn duplicate_batch_id_is_idempotent() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_config(dir.path());
            let data_path = cfg.data_path.clone();
            let storage = AppendLogRecorderStorage::open(cfg).unwrap();

            let first = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![sample_event("e1", 1, 0, "one")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();

            let before_len = std::fs::metadata(&data_path).unwrap().len();
            let second = storage
                .append_batch(AppendRequest {
                    batch_id: "same-batch".to_string(),
                    events: vec![sample_event("e2", 1, 1, "two")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 2,
                })
                .await
                .unwrap();
            let after_len = std::fs::metadata(&data_path).unwrap().len();

            assert_eq!(first, second);
            assert_eq!(before_len, after_len);
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
        let mut cfg = AppendLogStorageConfig::default();
        cfg.queue_capacity = 0;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_rejects_zero_max_batch_events() {
        let mut cfg = AppendLogStorageConfig::default();
        cfg.max_batch_events = 0;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_rejects_zero_max_batch_bytes() {
        let mut cfg = AppendLogStorageConfig::default();
        cfg.max_batch_bytes = 0;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    }

    #[test]
    fn config_validate_rejects_zero_idempotency_entries() {
        let mut cfg = AppendLogStorageConfig::default();
        cfg.max_idempotency_entries = 0;
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

            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b3".to_string(),
                    events: vec![sample_event("e3-replay", 1, 200, "replay")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 200,
                })
                .await
                .unwrap();

            let data_len_after2 = std::fs::metadata(dir.path().join("events.log"))
                .unwrap()
                .len();

            assert_eq!(data_len_before2, data_len_after2, "b3 should be cached");
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
    fn health_records_checkpoint_regression_diagnostic() {
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

            let degraded = storage.health().await;
            assert!(degraded.degraded);
            let diagnostic = degraded.last_error.unwrap();
            assert!(diagnostic.contains("commit_checkpoint failed"));
            assert!(diagnostic.contains("TerminalData"));

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
    fn health_records_append_diagnostic_and_clears_on_success() {
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

            let degraded = storage.health().await;
            assert!(degraded.degraded);
            let diagnostic = degraded.last_error.unwrap();
            assert!(diagnostic.contains("append_batch failed"));
            assert!(diagnostic.contains("TerminalData"));

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

            let mut persisted = load_persisted_state(&cfg.state_path).unwrap();
            let recovered_offset = persisted.next_offset;
            persisted.next_ordinal = 99;
            write_persisted_state(&cfg.state_path, &persisted).unwrap();

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
            let mut persisted = load_persisted_state(&cfg.state_path).unwrap();
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
            write_persisted_state(&cfg.state_path, &persisted).unwrap();

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

        let err = RecorderStorageError::CorruptRecord {
            offset: 100,
            reason: "bad crc".to_string(),
        };
        assert_eq!(err.class(), RecorderStorageErrorClass::Corruption);
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
            backend: RecorderBackendKind::FrankenSqlite,
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
    fn backend_kind_serde_roundtrip_both_variants() {
        for kind in [
            RecorderBackendKind::AppendLog,
            RecorderBackendKind::FrankenSqlite,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: RecorderBackendKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
        // Verify snake_case rename
        let json = serde_json::to_string(&RecorderBackendKind::AppendLog).unwrap();
        assert!(json.contains("append_log"));
        let json = serde_json::to_string(&RecorderBackendKind::FrankenSqlite).unwrap();
        assert!(json.contains("frankensqlite"));
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
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: AppendResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, resp);
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
            assert!(!bytes.is_empty());

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
    fn source_descriptor_frankensqlite_display() {
        let desc = RecorderSourceDescriptor::FrankenSqlite {
            db_path: PathBuf::from("/tmp/recorder.db"),
        };
        let display = desc.to_string();
        assert!(display.contains("frankensqlite"));
        assert!(display.contains("/tmp/recorder.db"));
    }

    #[test]
    fn source_descriptor_backend_kind() {
        let al = RecorderSourceDescriptor::AppendLog {
            data_path: PathBuf::from("x"),
        };
        assert_eq!(al.backend_kind(), RecorderBackendKind::AppendLog);

        let fs = RecorderSourceDescriptor::FrankenSqlite {
            db_path: PathBuf::from("y"),
        };
        assert_eq!(fs.backend_kind(), RecorderBackendKind::FrankenSqlite);
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
    fn source_descriptor_serde_roundtrip_frankensqlite() {
        let desc = RecorderSourceDescriptor::FrankenSqlite {
            db_path: PathBuf::from("/data/recorder.db"),
        };
        let json = serde_json::to_string(&desc).unwrap();
        assert!(json.contains("franken_sqlite"));
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

        let other = RecorderSourceDescriptor::FrankenSqlite {
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
