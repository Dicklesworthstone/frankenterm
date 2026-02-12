//! Recorder storage abstraction and append-log backend.
//!
//! This module implements the `wa-oegrb.3.2` hot-path baseline:
//! - append-only batched writes with deterministic offsets
//! - bounded in-flight admission and explicit overload signaling
//! - idempotent `batch_id` handling
//! - persisted writer/checkpoint state and torn-tail recovery

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::recording::RecorderEvent;

/// Stable backend identity for recorder storage implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderBackendKind {
    /// Local append-only log backend.
    AppendLog,
    /// FrankenSQLite-backed backend (implemented in a follow-on bead).
    FrankenSqlite,
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

    #[error("checkpoint regression for consumer {consumer}: current={current_ordinal}, attempted={attempted_ordinal}")]
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
        }
    }
}

/// Recorder storage boundary used by capture and indexing layers.
#[allow(async_fn_in_trait)]
pub trait RecorderStorage: Send + Sync {
    fn backend_kind(&self) -> RecorderBackendKind;

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError>;

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError>;

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
#[derive(Debug, Clone)]
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

impl AppendLogRecorderStorage {
    /// Open or create an append-log recorder backend.
    pub fn open(config: AppendLogStorageConfig) -> std::result::Result<Self, RecorderStorageError> {
        config.validate()?;
        ensure_parent_dir(&config.data_path)?;
        ensure_parent_dir(&config.state_path)?;

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .append(true)
            .open(&config.data_path)?;

        let scan = scan_valid_prefix(&mut file)?;
        let persisted = load_persisted_state(&config.state_path)?;

        let next_offset = if persisted.next_offset == scan.valid_len {
            persisted.next_offset
        } else {
            scan.valid_len
        };

        let next_ordinal = if persisted.next_offset == scan.valid_len {
            persisted.next_ordinal
        } else {
            scan.valid_records
        };

        file.seek(SeekFrom::End(0))?;

        let inner = AppendLogInner {
            writer: std::io::BufWriter::new(file),
            segment_id: persisted.segment_id,
            next_offset,
            next_ordinal,
            checkpoints: persisted.checkpoints,
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
        inner: &mut AppendLogInner,
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

        if let Some(existing) = inner.idempotency_cache.get(&req.batch_id) {
            return Ok(existing.clone());
        }

        let mut encoded = Vec::with_capacity(req.events.len());
        let mut total_bytes = 0usize;
        for event in req.events {
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

        match req.required_durability {
            DurabilityLevel::Enqueued => {}
            DurabilityLevel::Appended => {
                inner.writer.flush()?;
                self.persist_state(&mut inner)?;
            }
            DurabilityLevel::Fsync => {
                inner.writer.flush()?;
                inner.writer.get_ref().sync_data()?;
                self.persist_state(&mut inner)?;
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
            committed_durability: req.required_durability,
            committed_at_ms: crate::recording::epoch_ms_now(),
        };

        inner
            .idempotency_cache
            .insert(req.batch_id.clone(), response.clone());
        inner.idempotency_order.push_back(req.batch_id);
        while inner.idempotency_cache.len() > self.config.max_idempotency_entries {
            if let Some(evict) = inner.idempotency_order.pop_front() {
                inner.idempotency_cache.remove(&evict);
            }
        }

        Ok(response)
    }

    async fn flush(
        &self,
        mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        let mut inner = self.inner.lock().await;
        inner.writer.flush()?;
        if mode == FlushMode::Durable {
            inner.writer.get_ref().sync_data()?;
        }
        self.persist_state(&mut inner)?;
        Ok(FlushStats {
            backend: RecorderBackendKind::AppendLog,
            flushed_at_ms: crate::recording::epoch_ms_now(),
            latest_offset: Self::latest_offset(&inner),
        })
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
        let key = checkpoint.consumer.0.clone();
        let outcome = match inner.checkpoints.get(&key) {
            Some(existing) if checkpoint.upto_offset.ordinal < existing.upto_offset.ordinal => {
                return Err(RecorderStorageError::CheckpointRegression {
                    consumer: key,
                    current_ordinal: existing.upto_offset.ordinal,
                    attempted_ordinal: checkpoint.upto_offset.ordinal,
                });
            }
            Some(existing) if checkpoint.upto_offset.ordinal == existing.upto_offset.ordinal => {
                CheckpointCommitOutcome::NoopAlreadyAdvanced
            }
            _ => CheckpointCommitOutcome::Advanced,
        };

        if outcome == CheckpointCommitOutcome::Advanced {
            inner.checkpoints.insert(key, checkpoint);
            self.persist_state(&mut inner)?;
        }

        Ok(outcome)
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

        file.seek(SeekFrom::Current(payload_len as i64))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality,
        RecorderEventPayload, RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel,
        RecorderTextEncoding,
    };
    use tempfile::tempdir;

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

    #[tokio::test]
    async fn append_assigns_monotonic_offsets() {
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
    }

    #[tokio::test]
    async fn duplicate_batch_id_is_idempotent() {
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
    }

    #[tokio::test]
    async fn checkpoint_commit_is_monotonic_and_persisted() {
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
    }

    #[tokio::test]
    async fn startup_truncates_torn_tail_and_recovers_ordinal() {
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
            file.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
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
    }

    #[tokio::test]
    async fn rejects_batch_larger_than_configured_byte_limit() {
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

    #[tokio::test]
    async fn rejects_empty_batch_id() {
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
    }

    #[tokio::test]
    async fn rejects_empty_events_list() {
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
    }

    #[tokio::test]
    async fn rejects_batch_exceeding_event_count_limit() {
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
    }

    // ── Idempotency cache eviction ───────────────────────────────────

    #[tokio::test]
    async fn idempotency_cache_evicts_oldest_when_full() {
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
    }

    // ── Health and lag metrics ────────────────────────────────────────

    #[tokio::test]
    async fn health_reports_correct_state() {
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
    }

    #[tokio::test]
    async fn lag_metrics_track_consumer_offsets() {
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
    }

    // ── Checkpoint regression ────────────────────────────────────────

    #[tokio::test]
    async fn checkpoint_regression_returns_error() {
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
    }

    // ── Read checkpoint for unknown consumer ─────────────────────────

    #[tokio::test]
    async fn read_checkpoint_unknown_consumer_returns_none() {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let result = storage
            .read_checkpoint(&CheckpointConsumerId("nonexistent".to_string()))
            .await
            .unwrap();

        assert!(result.is_none());
    }

    // ── Flush modes ──────────────────────────────────────────────────

    #[tokio::test]
    async fn flush_buffered_and_durable() {
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
    }

    // ── Durability levels ────────────────────────────────────────────

    #[tokio::test]
    async fn enqueued_durability_does_not_fsync() {
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
    }

    #[tokio::test]
    async fn fsync_durability_committed() {
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
    }

    // ── Reopen persistence ───────────────────────────────────────────

    #[tokio::test]
    async fn reopen_continues_ordinals() {
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

    #[tokio::test]
    async fn backend_kind_is_append_log() {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        assert_eq!(storage.backend_kind(), RecorderBackendKind::AppendLog);
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

    #[tokio::test]
    async fn multi_batch_accepted_count_correct() {
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
    }

    // ── Open with empty data file ────────────────────────────────────

    #[tokio::test]
    async fn open_with_empty_data_file() {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        // Create empty data file
        std::fs::create_dir_all(cfg.data_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg.data_path, &[]).unwrap();

        let storage = AppendLogRecorderStorage::open(cfg).unwrap();
        let h = storage.health().await;
        assert!(h.latest_offset.is_none());
    }

    // ── Lag with no consumers ────────────────────────────────────────

    #[tokio::test]
    async fn lag_with_no_consumers() {
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
    }
}
