//! Incremental Tantivy ingestion from append-log checkpoints.
//!
//! Bead: wa-oegrb.4.2
//!
//! This module implements the offset-driven incremental indexer pipeline that
//! bridges the recorder append-log with Tantivy lexical search:
//!
//! 1. Read last checkpoint for this consumer
//! 2. Scan append-log from checkpoint offset forward
//! 3. Map each [`RecorderEvent`] to [`IndexDocumentFields`]
//! 4. Write batches to an [`IndexWriter`] implementation
//! 5. Commit checkpoint after each successful batch
//!
//! The pipeline is idempotent: replaying from the same checkpoint produces
//! identical index state. Schema version: `ft.recorder.lexical.v1`.

use std::fs::File;
use std::io::{Read as IoRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::recorder_storage::{
    CheckpointConsumerId, RecorderCheckpoint, RecorderOffset, RecorderStorage, RecorderStorageError,
};
use crate::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventPayload, RecorderRedactionLevel,
};

// ---------------------------------------------------------------------------
// Schema version
// ---------------------------------------------------------------------------

/// Lexical index schema version. Bumping this triggers a full reindex.
pub const LEXICAL_SCHEMA_VERSION: &str = "ft.recorder.lexical.v1";

/// Default consumer ID used by the lexical indexer.
pub const LEXICAL_INDEXER_CONSUMER: &str = "tantivy-lexical-v1";

// ---------------------------------------------------------------------------
// IndexDocumentFields — flat representation of a Tantivy document
// ---------------------------------------------------------------------------

/// All fields for a single Tantivy document, matching the `ft.recorder.lexical.v1` schema.
///
/// Field names and semantics are 1:1 with `docs/flight-recorder/tantivy-schema-v1.md`.
/// String fields use `Option<String>` where the source event field is optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDocumentFields {
    // --- identity ---
    pub schema_version: String,
    pub lexical_schema_version: String,
    pub event_id: String,

    // --- pane/session ---
    pub pane_id: u64,
    pub session_id: Option<String>,
    pub workflow_id: Option<String>,
    pub correlation_id: Option<String>,

    // --- causality ---
    pub parent_event_id: Option<String>,
    pub trigger_event_id: Option<String>,
    pub root_event_id: Option<String>,

    // --- source/type ---
    pub source: String,
    pub event_type: String,

    // --- variant-specific (empty string if not applicable) ---
    pub ingress_kind: Option<String>,
    pub segment_kind: Option<String>,
    pub control_marker_type: Option<String>,
    pub lifecycle_phase: Option<String>,
    pub is_gap: bool,
    pub redaction: Option<String>,

    // --- timestamps/ordering ---
    pub occurred_at_ms: i64,
    pub recorded_at_ms: i64,
    pub sequence: u64,
    pub log_offset: u64,

    // --- text ---
    pub text: String,
    pub text_symbols: String,

    // --- details ---
    pub details_json: String,
}

// ---------------------------------------------------------------------------
// Document mapper
// ---------------------------------------------------------------------------

/// Maps a [`RecorderEvent`] at a given log offset to [`IndexDocumentFields`].
pub fn map_event_to_document(event: &RecorderEvent, log_offset: u64) -> IndexDocumentFields {
    let (
        event_type,
        text,
        ingress_kind,
        segment_kind,
        control_marker_type,
        lifecycle_phase,
        is_gap,
        redaction,
        details_json,
    ) = match &event.payload {
        RecorderEventPayload::IngressText {
            text,
            redaction,
            ingress_kind,
            ..
        } => (
            "ingress_text".to_string(),
            redacted_text(text, *redaction),
            Some(format_ingress_kind(*ingress_kind)),
            None,
            None,
            None,
            false,
            Some(format_redaction(*redaction)),
            "{}".to_string(),
        ),
        RecorderEventPayload::EgressOutput {
            text,
            redaction,
            segment_kind,
            is_gap,
            ..
        } => (
            "egress_output".to_string(),
            redacted_text(text, *redaction),
            None,
            Some(format_segment_kind(*segment_kind)),
            None,
            None,
            *is_gap,
            Some(format_redaction(*redaction)),
            "{}".to_string(),
        ),
        RecorderEventPayload::ControlMarker {
            control_marker_type,
            details,
        } => (
            "control_marker".to_string(),
            String::new(),
            None,
            None,
            Some(format_control_marker(*control_marker_type)),
            None,
            false,
            None,
            serde_json::to_string(details).unwrap_or_else(|_| "{}".to_string()),
        ),
        RecorderEventPayload::LifecycleMarker {
            lifecycle_phase,
            reason,
            details,
        } => {
            let reason_text = reason.as_deref().unwrap_or("");
            (
                "lifecycle_marker".to_string(),
                reason_text.to_string(),
                None,
                None,
                None,
                Some(format_lifecycle_phase(*lifecycle_phase)),
                false,
                None,
                serde_json::to_string(details).unwrap_or_else(|_| "{}".to_string()),
            )
        }
    };

    // text_symbols gets the same text content — the schema uses a different tokenizer
    // at index time (ft_terminal_symbols_v1 vs ft_terminal_text_v1).
    let text_symbols = text.clone();

    IndexDocumentFields {
        schema_version: event.schema_version.clone(),
        lexical_schema_version: LEXICAL_SCHEMA_VERSION.to_string(),
        event_id: event.event_id.clone(),
        pane_id: event.pane_id,
        session_id: event.session_id.clone(),
        workflow_id: event.workflow_id.clone(),
        correlation_id: event.correlation_id.clone(),
        parent_event_id: event.causality.parent_event_id.clone(),
        trigger_event_id: event.causality.trigger_event_id.clone(),
        root_event_id: event.causality.root_event_id.clone(),
        source: format_source(event.source),
        event_type,
        ingress_kind,
        segment_kind,
        control_marker_type,
        lifecycle_phase,
        is_gap,
        redaction,
        occurred_at_ms: event.occurred_at_ms as i64,
        recorded_at_ms: event.recorded_at_ms as i64,
        sequence: event.sequence,
        log_offset,
        text,
        text_symbols,
        details_json,
    }
}

fn redacted_text(text: &str, level: RecorderRedactionLevel) -> String {
    match level {
        RecorderRedactionLevel::None => text.to_string(),
        RecorderRedactionLevel::Partial => "[REDACTED]".to_string(),
        RecorderRedactionLevel::Full => String::new(),
    }
}

fn format_source(s: crate::recording::RecorderEventSource) -> String {
    use crate::recording::RecorderEventSource::*;
    match s {
        WeztermMux => "wezterm_mux",
        RobotMode => "robot_mode",
        WorkflowEngine => "workflow_engine",
        OperatorAction => "operator_action",
        RecoveryFlow => "recovery_flow",
    }
    .to_string()
}

fn format_ingress_kind(k: crate::recording::RecorderIngressKind) -> String {
    use crate::recording::RecorderIngressKind::*;
    match k {
        SendText => "send_text",
        Paste => "paste",
        WorkflowAction => "workflow_action",
    }
    .to_string()
}

fn format_segment_kind(k: crate::recording::RecorderSegmentKind) -> String {
    use crate::recording::RecorderSegmentKind::*;
    match k {
        Delta => "delta",
        Gap => "gap",
        Snapshot => "snapshot",
    }
    .to_string()
}

fn format_control_marker(t: crate::recording::RecorderControlMarkerType) -> String {
    use crate::recording::RecorderControlMarkerType::*;
    match t {
        PromptBoundary => "prompt_boundary",
        Resize => "resize",
        PolicyDecision => "policy_decision",
        ApprovalCheckpoint => "approval_checkpoint",
    }
    .to_string()
}

fn format_lifecycle_phase(p: crate::recording::RecorderLifecyclePhase) -> String {
    use crate::recording::RecorderLifecyclePhase::*;
    match p {
        CaptureStarted => "capture_started",
        CaptureStopped => "capture_stopped",
        PaneOpened => "pane_opened",
        PaneClosed => "pane_closed",
        ReplayStarted => "replay_started",
        ReplayFinished => "replay_finished",
    }
    .to_string()
}

fn format_redaction(r: RecorderRedactionLevel) -> String {
    match r {
        RecorderRedactionLevel::None => "none",
        RecorderRedactionLevel::Partial => "partial",
        RecorderRedactionLevel::Full => "full",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Append-log reader
// ---------------------------------------------------------------------------

/// Reads [`RecorderEvent`]s sequentially from an append-log data file.
///
/// The binary format is: `[u32 LE payload_len][JSON payload]` per record.
/// Records are numbered by monotonic ordinal starting from 0.
pub struct AppendLogReader {
    file: File,
    /// Current byte position in the data file.
    byte_offset: u64,
    /// Next ordinal to yield.
    next_ordinal: u64,
}

/// A single record read from the append log.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub event: RecorderEvent,
    pub offset: RecorderOffset,
}

/// Error during append-log reading.
#[derive(Debug)]
pub enum LogReadError {
    /// I/O error reading the log file.
    Io(std::io::Error),
    /// Corrupt or truncated record at the given byte offset.
    Corrupt { byte_offset: u64, reason: String },
    /// JSON deserialization failure.
    Deserialize {
        byte_offset: u64,
        source: serde_json::Error,
    },
}

impl std::fmt::Display for LogReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "log read I/O error: {e}"),
            Self::Corrupt {
                byte_offset,
                reason,
            } => {
                write!(f, "corrupt record at byte {byte_offset}: {reason}")
            }
            Self::Deserialize {
                byte_offset,
                source,
            } => {
                write!(f, "JSON error at byte {byte_offset}: {source}")
            }
        }
    }
}

impl std::error::Error for LogReadError {}

impl From<std::io::Error> for LogReadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl AppendLogReader {
    /// Open a reader positioned at the start of the file.
    pub fn open(data_path: &Path) -> Result<Self, LogReadError> {
        let mut file = File::open(data_path)?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            file,
            byte_offset: 0,
            next_ordinal: 0,
        })
    }

    /// Open a reader and seek forward to `start_ordinal`.
    ///
    /// This scans from the beginning, skipping records until the target ordinal
    /// is reached. Returns the reader positioned to yield `start_ordinal` next.
    pub fn open_at_ordinal(data_path: &Path, start_ordinal: u64) -> Result<Self, LogReadError> {
        let mut reader = Self::open(data_path)?;
        reader.skip_to_ordinal(start_ordinal)?;
        Ok(reader)
    }

    /// Open a reader and seek directly to `byte_offset` with known ordinal.
    ///
    /// Callers that have a trusted (byte_offset, ordinal) pair from a checkpoint
    /// can skip the sequential scan. The caller is responsible for correctness of
    /// the pair.
    pub fn open_at_offset(
        data_path: &Path,
        byte_offset: u64,
        ordinal: u64,
    ) -> Result<Self, LogReadError> {
        let mut file = File::open(data_path)?;
        file.seek(SeekFrom::Start(byte_offset))?;
        Ok(Self {
            file,
            byte_offset,
            next_ordinal: ordinal,
        })
    }

    /// Skip forward to the given ordinal by scanning records.
    fn skip_to_ordinal(&mut self, target: u64) -> Result<(), LogReadError> {
        while self.next_ordinal < target {
            let file_len = self.file.metadata()?.len();
            if self.byte_offset + 4 > file_len {
                return Err(LogReadError::Corrupt {
                    byte_offset: self.byte_offset,
                    reason: format!(
                        "EOF before reaching ordinal {target} (at ordinal {})",
                        self.next_ordinal
                    ),
                });
            }

            let mut len_buf = [0u8; 4];
            self.file.read_exact(&mut len_buf)?;
            let payload_len = u32::from_le_bytes(len_buf) as u64;
            let next_pos = self.byte_offset + 4 + payload_len;

            if next_pos > file_len {
                return Err(LogReadError::Corrupt {
                    byte_offset: self.byte_offset,
                    reason: format!(
                        "truncated record: payload_len={payload_len}, but only {} bytes remain",
                        file_len - self.byte_offset - 4
                    ),
                });
            }

            self.file.seek(SeekFrom::Start(next_pos))?;
            self.byte_offset = next_pos;
            self.next_ordinal += 1;
        }
        Ok(())
    }

    /// Read the next record, returning `None` at EOF.
    pub fn next_record(&mut self) -> Result<Option<LogRecord>, LogReadError> {
        let file_len = self.file.metadata()?.len();
        if self.byte_offset + 4 > file_len {
            return Ok(None);
        }

        let record_start = self.byte_offset;

        let mut len_buf = [0u8; 4];
        self.file.read_exact(&mut len_buf)?;
        let payload_len = u32::from_le_bytes(len_buf) as u64;

        if self.byte_offset + 4 + payload_len > file_len {
            // Torn tail — treat as EOF (don't advance past it).
            self.file.seek(SeekFrom::Start(record_start))?;
            return Ok(None);
        }

        let mut payload_buf = vec![0u8; payload_len as usize];
        self.file.read_exact(&mut payload_buf)?;

        let event: RecorderEvent =
            serde_json::from_slice(&payload_buf).map_err(|e| LogReadError::Deserialize {
                byte_offset: record_start,
                source: e,
            })?;

        let offset = RecorderOffset {
            segment_id: 0,
            byte_offset: record_start,
            ordinal: self.next_ordinal,
        };

        self.byte_offset = record_start + 4 + payload_len;
        self.next_ordinal += 1;

        Ok(Some(LogRecord { event, offset }))
    }

    /// Read up to `limit` records into a batch.
    pub fn read_batch(&mut self, limit: usize) -> Result<Vec<LogRecord>, LogReadError> {
        let mut batch = Vec::with_capacity(limit.min(256));
        for _ in 0..limit {
            match self.next_record()? {
                Some(record) => batch.push(record),
                None => break,
            }
        }
        Ok(batch)
    }

    /// Current byte offset in the file.
    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }

    /// Next ordinal that will be yielded.
    pub fn next_ordinal(&self) -> u64 {
        self.next_ordinal
    }
}

// ---------------------------------------------------------------------------
// IndexWriter trait — abstract index writing surface
// ---------------------------------------------------------------------------

/// Error from index write operations.
#[derive(Debug)]
pub enum IndexWriteError {
    /// The document was rejected (bad data, schema mismatch, etc.).
    Rejected { reason: String },
    /// Transient I/O or resource error; safe to retry.
    Transient { reason: String },
    /// Commit/flush to durable index failed.
    CommitFailed { reason: String },
}

impl std::fmt::Display for IndexWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { reason } => write!(f, "document rejected: {reason}"),
            Self::Transient { reason } => write!(f, "transient index error: {reason}"),
            Self::CommitFailed { reason } => write!(f, "index commit failed: {reason}"),
        }
    }
}

impl std::error::Error for IndexWriteError {}

/// Trait for writing documents to a search index.
///
/// Implementations wrap the actual Tantivy `IndexWriter` or a test mock.
/// The trait is intentionally synchronous since Tantivy's writer is sync.
pub trait IndexWriter: Send {
    /// Add a document to the pending write buffer.
    fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError>;

    /// Commit all pending documents to the index, making them searchable.
    fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError>;

    /// Delete all documents with the given event_id (for deduplication on replay).
    fn delete_by_event_id(&mut self, event_id: &str) -> Result<(), IndexWriteError>;
}

/// Stats returned after a successful index commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCommitStats {
    /// Number of documents added since last commit.
    pub docs_added: u64,
    /// Number of documents deleted since last commit.
    pub docs_deleted: u64,
    /// Total segments after commit.
    pub segment_count: u64,
}

// ---------------------------------------------------------------------------
// Indexer configuration
// ---------------------------------------------------------------------------

/// Configuration for the incremental indexer pipeline.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Path to the append-log data file.
    pub data_path: PathBuf,
    /// Consumer ID for checkpoint tracking (unique per index pipeline).
    pub consumer_id: String,
    /// Maximum events to read per batch before committing.
    pub batch_size: usize,
    /// Whether to delete-before-add for idempotent replay.
    pub dedup_on_replay: bool,
    /// Stop after processing this many batches (0 = unlimited, for one-shot mode).
    pub max_batches: usize,
    /// Expected recorder event schema version (reject events with mismatched version).
    pub expected_event_schema: String,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            data_path: PathBuf::from(".ft/recorder-log/events.log"),
            consumer_id: LEXICAL_INDEXER_CONSUMER.to_string(),
            batch_size: 512,
            dedup_on_replay: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// IncrementalIndexer — the main pipeline
// ---------------------------------------------------------------------------

/// Result of a single indexer run (one or more batches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerRunResult {
    /// Total events read from the log.
    pub events_read: u64,
    /// Events successfully indexed.
    pub events_indexed: u64,
    /// Events skipped (schema mismatch, rejection, etc.).
    pub events_skipped: u64,
    /// Batches processed.
    pub batches_committed: u64,
    /// Final checkpoint ordinal after the run.
    pub final_ordinal: Option<u64>,
    /// Whether the run reached the current end of the log.
    pub caught_up: bool,
}

/// Error during an indexer run.
#[derive(Debug)]
pub enum IndexerError {
    /// Failed to read from the append log.
    LogRead(LogReadError),
    /// Failed to read/commit checkpoint.
    Storage(RecorderStorageError),
    /// Index write/commit failed.
    IndexWrite(IndexWriteError),
    /// Configuration error.
    Config(String),
}

impl std::fmt::Display for IndexerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LogRead(e) => write!(f, "log read: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
            Self::IndexWrite(e) => write!(f, "index write: {e}"),
            Self::Config(msg) => write!(f, "config: {msg}"),
        }
    }
}

impl std::error::Error for IndexerError {}

impl From<LogReadError> for IndexerError {
    fn from(e: LogReadError) -> Self {
        Self::LogRead(e)
    }
}

impl From<RecorderStorageError> for IndexerError {
    fn from(e: RecorderStorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<IndexWriteError> for IndexerError {
    fn from(e: IndexWriteError) -> Self {
        Self::IndexWrite(e)
    }
}

/// Incremental indexer that bridges the append log to a search index.
///
/// Typical usage:
/// ```ignore
/// let storage = AppendLogRecorderStorage::open(config)?;
/// let writer = TantivyIndexWriter::new(...);
/// let indexer = IncrementalIndexer::new(indexer_config, writer);
/// let result = indexer.run(&storage).await?;
/// ```
pub struct IncrementalIndexer<W: IndexWriter> {
    config: IndexerConfig,
    writer: W,
}

impl<W: IndexWriter> IncrementalIndexer<W> {
    /// Create a new indexer with the given configuration and index writer.
    pub fn new(config: IndexerConfig, writer: W) -> Self {
        Self { config, writer }
    }

    /// Run the indexer: read checkpoint, scan log, index events, commit checkpoints.
    ///
    /// Processes batches until EOF or `max_batches` is reached.
    pub async fn run<S: RecorderStorage>(
        &mut self,
        storage: &S,
    ) -> Result<IndexerRunResult, IndexerError> {
        if self.config.batch_size == 0 {
            return Err(IndexerError::Config("batch_size must be >= 1".to_string()));
        }

        let consumer_id = CheckpointConsumerId(self.config.consumer_id.clone());

        // 1. Read last checkpoint
        let checkpoint = storage.read_checkpoint(&consumer_id).await?;

        // 2. Open log reader at resume point
        let mut reader = match &checkpoint {
            Some(cp) => {
                // Resume from the record AFTER the last checkpointed ordinal.
                let resume_byte = cp.upto_offset.byte_offset;
                let resume_ordinal = cp.upto_offset.ordinal;
                // We need to position at the byte AFTER the checkpointed record.
                // The checkpoint stores the offset of the last processed record,
                // so we need to seek past it. We open at the checkpoint's byte
                // offset and skip one record to get past it.
                let mut r = AppendLogReader::open_at_offset(
                    &self.config.data_path,
                    resume_byte,
                    resume_ordinal,
                )?;
                // Skip the checkpointed record itself
                let _ = r.next_record()?;
                r
            }
            None => AppendLogReader::open(&self.config.data_path)?,
        };

        let mut result = IndexerRunResult {
            events_read: 0,
            events_indexed: 0,
            events_skipped: 0,
            batches_committed: 0,
            final_ordinal: checkpoint.as_ref().map(|cp| cp.upto_offset.ordinal),
            caught_up: false,
        };

        // 3. Process batches
        loop {
            if self.config.max_batches > 0
                && result.batches_committed >= self.config.max_batches as u64
            {
                break;
            }

            let batch = reader.read_batch(self.config.batch_size)?;
            if batch.is_empty() {
                result.caught_up = true;
                break;
            }

            // If we got fewer records than requested, we've reached the end.
            let is_final_batch = batch.len() < self.config.batch_size;

            let mut last_offset: Option<RecorderOffset> = None;

            for record in &batch {
                result.events_read += 1;

                // Schema version check
                if record.event.schema_version != self.config.expected_event_schema {
                    result.events_skipped += 1;
                    last_offset = Some(record.offset.clone());
                    continue;
                }

                // Map event to document
                let doc = map_event_to_document(&record.event, record.offset.ordinal);

                // Dedup: delete existing doc with same event_id before re-adding
                if self.config.dedup_on_replay {
                    if let Err(_) = self.writer.delete_by_event_id(&doc.event_id) {
                        // Deletion failure on a non-existent doc is fine; only
                        // propagate genuine failures on commit.
                    }
                }

                match self.writer.add_document(&doc) {
                    Ok(()) => result.events_indexed += 1,
                    Err(IndexWriteError::Rejected { .. }) => result.events_skipped += 1,
                    Err(e) => return Err(e.into()),
                }

                last_offset = Some(record.offset.clone());
            }

            // Commit the index batch
            self.writer.commit()?;

            // Commit checkpoint to storage
            if let Some(offset) = last_offset {
                result.final_ordinal = Some(offset.ordinal);
                let cp = RecorderCheckpoint {
                    consumer: consumer_id.clone(),
                    upto_offset: offset,
                    schema_version: self.config.expected_event_schema.clone(),
                    committed_at_ms: epoch_ms_now(),
                };
                storage.commit_checkpoint(cp).await?;
            }

            result.batches_committed += 1;

            if is_final_batch {
                result.caught_up = true;
                break;
            }
        }

        Ok(result)
    }

    /// Access the underlying writer (e.g. for diagnostics).
    pub fn writer(&self) -> &W {
        &self.writer
    }

    /// Consume the indexer and return the writer.
    pub fn into_writer(self) -> W {
        self.writer
    }
}

// ---------------------------------------------------------------------------
// IndexerLagMonitor — observe indexing progress
// ---------------------------------------------------------------------------

/// Snapshot of indexer lag relative to the append log head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerLagSnapshot {
    /// Latest ordinal in the append log.
    pub log_head_ordinal: Option<u64>,
    /// Indexer's last checkpointed ordinal.
    pub indexer_ordinal: Option<u64>,
    /// Number of records behind.
    pub records_behind: u64,
    /// Whether the indexer has fully caught up.
    pub caught_up: bool,
}

/// Compute indexing lag from storage health and checkpoint data.
pub async fn compute_indexer_lag<S: RecorderStorage>(
    storage: &S,
    consumer_id: &str,
) -> Result<IndexerLagSnapshot, RecorderStorageError> {
    let health = storage.health().await;
    let checkpoint = storage
        .read_checkpoint(&CheckpointConsumerId(consumer_id.to_string()))
        .await?;

    let log_head = health.latest_offset.map(|o| o.ordinal);
    let indexer_ord = checkpoint.map(|cp| cp.upto_offset.ordinal);

    let records_behind = match (log_head, indexer_ord) {
        (Some(head), Some(idx)) => head.saturating_sub(idx),
        (Some(head), None) => head + 1, // never indexed
        _ => 0,
    };

    let caught_up = records_behind == 0;

    Ok(IndexerLagSnapshot {
        log_head_ordinal: log_head,
        indexer_ordinal: indexer_ord,
        records_behind,
        caught_up,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn epoch_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder_storage::{
        AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, DurabilityLevel,
    };
    use crate::recording::{
        RecorderControlMarkerType, RecorderEventCausality, RecorderEventPayload,
        RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding,
    };
    use tempfile::tempdir;

    // -- test helpers --

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

    fn egress_event(event_id: &str, pane_id: u64, sequence: u64, text: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_000 + sequence,
            recorded_at_ms: 1_700_000_000_001 + sequence,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        }
    }

    fn control_event(event_id: &str, pane_id: u64, sequence: u64) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_000 + sequence,
            recorded_at_ms: 1_700_000_000_001 + sequence,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: serde_json::json!({"cols": 80}),
            },
        }
    }

    fn lifecycle_event(event_id: &str, pane_id: u64, sequence: u64) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_000 + sequence,
            recorded_at_ms: 1_700_000_000_001 + sequence,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: Some("parent-1".to_string()),
                trigger_event_id: None,
                root_event_id: Some("root-1".to_string()),
            },
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: RecorderLifecyclePhase::PaneOpened,
                reason: Some("user action".to_string()),
                details: serde_json::json!({}),
            },
        }
    }

    fn test_storage_config(path: &Path) -> AppendLogStorageConfig {
        AppendLogStorageConfig {
            data_path: path.join("events.log"),
            state_path: path.join("state.json"),
            queue_capacity: 4,
            max_batch_events: 256,
            max_batch_bytes: 1024 * 1024,
            max_idempotency_entries: 64,
        }
    }

    fn test_indexer_config(path: &Path) -> IndexerConfig {
        IndexerConfig {
            data_path: path.join("events.log"),
            consumer_id: "test-indexer".to_string(),
            batch_size: 10,
            dedup_on_replay: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        }
    }

    /// Mock IndexWriter that records all operations.
    struct MockIndexWriter {
        docs: Vec<IndexDocumentFields>,
        deleted_ids: Vec<String>,
        commits: u64,
        reject_event_ids: Vec<String>,
        fail_commit: bool,
    }

    impl MockIndexWriter {
        fn new() -> Self {
            Self {
                docs: Vec::new(),
                deleted_ids: Vec::new(),
                commits: 0,
                reject_event_ids: Vec::new(),
                fail_commit: false,
            }
        }
    }

    impl IndexWriter for MockIndexWriter {
        fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
            if self.reject_event_ids.contains(&doc.event_id) {
                return Err(IndexWriteError::Rejected {
                    reason: "test rejection".to_string(),
                });
            }
            self.docs.push(doc.clone());
            Ok(())
        }

        fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
            if self.fail_commit {
                return Err(IndexWriteError::CommitFailed {
                    reason: "test failure".to_string(),
                });
            }
            self.commits += 1;
            Ok(IndexCommitStats {
                docs_added: self.docs.len() as u64,
                docs_deleted: self.deleted_ids.len() as u64,
                segment_count: 1,
            })
        }

        fn delete_by_event_id(&mut self, event_id: &str) -> Result<(), IndexWriteError> {
            self.deleted_ids.push(event_id.to_string());
            Ok(())
        }
    }

    async fn populate_log(storage: &AppendLogRecorderStorage, events: Vec<RecorderEvent>) {
        for (i, chunk) in events.chunks(4).enumerate() {
            storage
                .append_batch(AppendRequest {
                    batch_id: format!("b-{i}"),
                    events: chunk.to_vec(),
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1_700_000_000_000 + i as u64,
                })
                .await
                .unwrap();
        }
    }

    // =========================================================================
    // Document mapper tests
    // =========================================================================

    #[test]
    fn map_ingress_event() {
        let event = sample_event("ev-1", 42, 7, "echo hello");
        let doc = map_event_to_document(&event, 100);

        assert_eq!(doc.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
        assert_eq!(doc.lexical_schema_version, LEXICAL_SCHEMA_VERSION);
        assert_eq!(doc.event_id, "ev-1");
        assert_eq!(doc.pane_id, 42);
        assert_eq!(doc.session_id, Some("sess-1".to_string()));
        assert_eq!(doc.source, "robot_mode");
        assert_eq!(doc.event_type, "ingress_text");
        assert_eq!(doc.ingress_kind, Some("send_text".to_string()));
        assert_eq!(doc.segment_kind, None);
        assert_eq!(doc.text, "echo hello");
        assert_eq!(doc.text_symbols, "echo hello");
        assert_eq!(doc.log_offset, 100);
        assert_eq!(doc.sequence, 7);
        assert!(!doc.is_gap);
    }

    #[test]
    fn map_egress_event() {
        let event = egress_event("ev-2", 10, 3, "output line");
        let doc = map_event_to_document(&event, 200);

        assert_eq!(doc.event_type, "egress_output");
        assert_eq!(doc.segment_kind, Some("delta".to_string()));
        assert_eq!(doc.source, "wezterm_mux");
        assert_eq!(doc.text, "output line");
        assert!(!doc.is_gap);
    }

    #[test]
    fn map_egress_gap_event() {
        let mut event = egress_event("ev-gap", 10, 4, "");
        if let RecorderEventPayload::EgressOutput {
            ref mut is_gap,
            ref mut segment_kind,
            ..
        } = event.payload
        {
            *is_gap = true;
            *segment_kind = RecorderSegmentKind::Gap;
        }
        let doc = map_event_to_document(&event, 300);

        assert!(doc.is_gap);
        assert_eq!(doc.segment_kind, Some("gap".to_string()));
    }

    #[test]
    fn map_control_event() {
        let event = control_event("ev-ctrl", 5, 10);
        let doc = map_event_to_document(&event, 400);

        assert_eq!(doc.event_type, "control_marker");
        assert_eq!(doc.control_marker_type, Some("prompt_boundary".to_string()));
        assert_eq!(doc.text, "");
        assert!(doc.details_json.contains("cols"));
    }

    #[test]
    fn map_lifecycle_event() {
        let event = lifecycle_event("ev-lc", 8, 20);
        let doc = map_event_to_document(&event, 500);

        assert_eq!(doc.event_type, "lifecycle_marker");
        assert_eq!(doc.lifecycle_phase, Some("pane_opened".to_string()));
        assert_eq!(doc.parent_event_id, Some("parent-1".to_string()));
        assert_eq!(doc.root_event_id, Some("root-1".to_string()));
        assert_eq!(doc.text, "user action");
    }

    #[test]
    fn map_redacted_partial() {
        let mut event = sample_event("ev-r", 1, 0, "secret stuff");
        if let RecorderEventPayload::IngressText {
            ref mut redaction, ..
        } = event.payload
        {
            *redaction = RecorderRedactionLevel::Partial;
        }
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.text, "[REDACTED]");
        assert_eq!(doc.redaction, Some("partial".to_string()));
    }

    #[test]
    fn map_redacted_full() {
        let mut event = sample_event("ev-rf", 1, 0, "top secret");
        if let RecorderEventPayload::IngressText {
            ref mut redaction, ..
        } = event.payload
        {
            *redaction = RecorderRedactionLevel::Full;
        }
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.text, "");
        assert_eq!(doc.redaction, Some("full".to_string()));
    }

    #[test]
    fn document_fields_serialize_roundtrip() {
        let event = sample_event("ev-rt", 1, 0, "test");
        let doc = map_event_to_document(&event, 42);
        let json = serde_json::to_string(&doc).unwrap();
        let deser: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, deser);
    }

    // =========================================================================
    // Append-log reader tests
    // =========================================================================

    #[tokio::test]
    async fn reader_reads_all_events() {
        let dir = tempdir().unwrap();
        let cfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();

        let events = vec![
            sample_event("e1", 1, 0, "first"),
            sample_event("e2", 1, 1, "second"),
            sample_event("e3", 2, 2, "third"),
        ];
        populate_log(&storage, events).await;
        drop(storage);

        let mut reader = AppendLogReader::open(&cfg.data_path).unwrap();
        let batch = reader.read_batch(100).unwrap();

        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].event.event_id, "e1");
        assert_eq!(batch[0].offset.ordinal, 0);
        assert_eq!(batch[1].event.event_id, "e2");
        assert_eq!(batch[1].offset.ordinal, 1);
        assert_eq!(batch[2].event.event_id, "e3");
        assert_eq!(batch[2].offset.ordinal, 2);
    }

    #[tokio::test]
    async fn reader_eof_returns_none() {
        let dir = tempdir().unwrap();
        let cfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
        populate_log(&storage, vec![sample_event("e1", 1, 0, "only")]).await;
        drop(storage);

        let mut reader = AppendLogReader::open(&cfg.data_path).unwrap();
        let _first = reader.next_record().unwrap().unwrap();
        let second = reader.next_record().unwrap();
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn reader_open_at_ordinal() {
        let dir = tempdir().unwrap();
        let cfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;
        drop(storage);

        let mut reader = AppendLogReader::open_at_ordinal(&cfg.data_path, 3).unwrap();
        assert_eq!(reader.next_ordinal(), 3);
        let record = reader.next_record().unwrap().unwrap();
        assert_eq!(record.event.event_id, "e3");
        assert_eq!(record.offset.ordinal, 3);
    }

    #[tokio::test]
    async fn reader_open_at_offset_direct() {
        let dir = tempdir().unwrap();
        let cfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();

        let events: Vec<_> = (0..3)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;
        drop(storage);

        // First read all to discover offsets
        let mut reader = AppendLogReader::open(&cfg.data_path).unwrap();
        let all = reader.read_batch(100).unwrap();
        let offset_2 = &all[2].offset;

        // Open at the byte offset of record 2
        let mut reader2 =
            AppendLogReader::open_at_offset(&cfg.data_path, offset_2.byte_offset, offset_2.ordinal)
                .unwrap();
        let rec = reader2.next_record().unwrap().unwrap();
        assert_eq!(rec.event.event_id, "e2");
    }

    #[tokio::test]
    async fn reader_batch_limits() {
        let dir = tempdir().unwrap();
        let cfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;
        drop(storage);

        let mut reader = AppendLogReader::open(&cfg.data_path).unwrap();
        let batch1 = reader.read_batch(3).unwrap();
        assert_eq!(batch1.len(), 3);
        assert_eq!(reader.next_ordinal(), 3);

        let batch2 = reader.read_batch(3).unwrap();
        assert_eq!(batch2.len(), 3);
        assert_eq!(batch2[0].offset.ordinal, 3);
    }

    #[test]
    fn reader_empty_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.log");
        std::fs::write(&path, b"").unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        assert!(reader.next_record().unwrap().is_none());
    }

    #[test]
    fn reader_torn_tail_treated_as_eof() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("torn.log");

        // Write one valid record
        let event = sample_event("e1", 1, 0, "hello");
        let payload = serde_json::to_vec(&event).unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&payload);
        // Add a torn tail: length header but incomplete payload
        data.extend_from_slice(&(100u32).to_le_bytes());
        data.extend_from_slice(b"abc");
        std::fs::write(&path, &data).unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        let first = reader.next_record().unwrap().unwrap();
        assert_eq!(first.event.event_id, "e1");
        // Torn tail treated as EOF
        assert!(reader.next_record().unwrap().is_none());
    }

    // =========================================================================
    // IncrementalIndexer tests
    // =========================================================================

    #[tokio::test]
    async fn indexer_full_pipeline_cold_start() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events = vec![
            sample_event("e1", 1, 0, "first"),
            sample_event("e2", 1, 1, "second"),
            egress_event("e3", 2, 2, "output"),
        ];
        populate_log(&storage, events).await;

        let icfg = test_indexer_config(dir.path());
        let writer = MockIndexWriter::new();
        let mut indexer = IncrementalIndexer::new(icfg, writer);

        let result = indexer.run(&storage).await.unwrap();

        assert_eq!(result.events_read, 3);
        assert_eq!(result.events_indexed, 3);
        assert_eq!(result.events_skipped, 0);
        assert_eq!(result.batches_committed, 1);
        assert_eq!(result.final_ordinal, Some(2));
        assert!(result.caught_up);

        assert_eq!(indexer.writer().docs.len(), 3);
        assert_eq!(indexer.writer().commits, 1);
    }

    #[tokio::test]
    async fn indexer_resumes_from_checkpoint() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..6)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        // First run: index first 3
        let icfg = IndexerConfig {
            data_path: dir.path().join("events.log"),
            consumer_id: "resume-test".to_string(),
            batch_size: 3,
            dedup_on_replay: true,
            max_batches: 1,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut indexer = IncrementalIndexer::new(icfg.clone(), MockIndexWriter::new());
        let r1 = indexer.run(&storage).await.unwrap();
        assert_eq!(r1.events_indexed, 3);
        assert_eq!(r1.final_ordinal, Some(2));
        assert!(!r1.caught_up);

        // Verify checkpoint was committed
        let cp = storage
            .read_checkpoint(&CheckpointConsumerId("resume-test".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cp.upto_offset.ordinal, 2);

        // Second run: should resume at ordinal 3 (unlimited batches to confirm caught_up)
        let icfg2 = IndexerConfig {
            max_batches: 0,
            ..icfg
        };
        let mut indexer2 = IncrementalIndexer::new(icfg2, MockIndexWriter::new());
        let r2 = indexer2.run(&storage).await.unwrap();
        assert_eq!(r2.events_indexed, 3);
        assert_eq!(r2.final_ordinal, Some(5));
        assert!(r2.caught_up);

        // Writer should only have the second batch's docs
        assert_eq!(indexer2.writer().docs.len(), 3);
        assert_eq!(indexer2.writer().docs[0].event_id, "e3");
    }

    #[tokio::test]
    async fn indexer_empty_log() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let icfg = test_indexer_config(dir.path());
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());

        let result = indexer.run(&storage).await.unwrap();
        assert_eq!(result.events_read, 0);
        assert_eq!(result.events_indexed, 0);
        assert!(result.caught_up);
        assert_eq!(result.final_ordinal, None);
    }

    #[tokio::test]
    async fn indexer_already_caught_up() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e1", 1, 0, "only")]).await;

        // First run indexes everything
        let icfg = test_indexer_config(dir.path());
        let mut indexer = IncrementalIndexer::new(icfg.clone(), MockIndexWriter::new());
        let r1 = indexer.run(&storage).await.unwrap();
        assert!(r1.caught_up);

        // Second run with no new events
        let mut indexer2 = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let r2 = indexer2.run(&storage).await.unwrap();
        assert_eq!(r2.events_read, 0);
        assert!(r2.caught_up);
    }

    #[tokio::test]
    async fn indexer_skips_wrong_schema_version() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let mut bad_event = sample_event("bad-1", 1, 0, "bad");
        bad_event.schema_version = "ft.recorder.event.v99".to_string();
        let good_event = sample_event("good-1", 1, 1, "good");

        populate_log(&storage, vec![bad_event, good_event]).await;

        let icfg = test_indexer_config(dir.path());
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let result = indexer.run(&storage).await.unwrap();

        assert_eq!(result.events_read, 2);
        assert_eq!(result.events_indexed, 1);
        assert_eq!(result.events_skipped, 1);
        assert_eq!(indexer.writer().docs[0].event_id, "good-1");
    }

    #[tokio::test]
    async fn indexer_dedup_deletes_before_add() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("dup-1", 1, 0, "text"),
                sample_event("dup-2", 1, 1, "text2"),
            ],
        )
        .await;

        let icfg = IndexerConfig {
            dedup_on_replay: true,
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        indexer.run(&storage).await.unwrap();

        // Verify delete_by_event_id was called for each doc
        assert_eq!(indexer.writer().deleted_ids.len(), 2);
        assert!(indexer.writer().deleted_ids.contains(&"dup-1".to_string()));
        assert!(indexer.writer().deleted_ids.contains(&"dup-2".to_string()));
    }

    #[tokio::test]
    async fn indexer_no_dedup_when_disabled() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e1", 1, 0, "text")]).await;

        let icfg = IndexerConfig {
            dedup_on_replay: false,
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        indexer.run(&storage).await.unwrap();

        assert!(indexer.writer().deleted_ids.is_empty());
    }

    #[tokio::test]
    async fn indexer_rejected_docs_are_skipped() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("ok-1", 1, 0, "good"),
                sample_event("reject-me", 1, 1, "bad"),
                sample_event("ok-2", 1, 2, "good"),
            ],
        )
        .await;

        let icfg = test_indexer_config(dir.path());
        let mut writer = MockIndexWriter::new();
        writer.reject_event_ids = vec!["reject-me".to_string()];
        let mut indexer = IncrementalIndexer::new(icfg, writer);

        let result = indexer.run(&storage).await.unwrap();
        assert_eq!(result.events_indexed, 2);
        assert_eq!(result.events_skipped, 1);
        // Checkpoint still advances past the rejected event
        assert_eq!(result.final_ordinal, Some(2));
    }

    #[tokio::test]
    async fn indexer_max_batches_limits_processing() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..20)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let icfg = IndexerConfig {
            batch_size: 5,
            max_batches: 2,
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let result = indexer.run(&storage).await.unwrap();

        assert_eq!(result.events_indexed, 10);
        assert_eq!(result.batches_committed, 2);
        assert!(!result.caught_up);
    }

    #[tokio::test]
    async fn indexer_commit_failure_propagates() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e1", 1, 0, "text")]).await;

        let icfg = test_indexer_config(dir.path());
        let mut writer = MockIndexWriter::new();
        writer.fail_commit = true;
        let mut indexer = IncrementalIndexer::new(icfg, writer);

        let err = indexer.run(&storage).await.unwrap_err();
        assert!(matches!(err, IndexerError::IndexWrite(_)));
    }

    #[tokio::test]
    async fn indexer_batch_size_one() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![sample_event("e1", 1, 0, "a"), sample_event("e2", 1, 1, "b")],
        )
        .await;

        let icfg = IndexerConfig {
            batch_size: 1,
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let result = indexer.run(&storage).await.unwrap();

        assert_eq!(result.events_indexed, 2);
        assert_eq!(result.batches_committed, 2);
        assert_eq!(indexer.writer().commits, 2);
    }

    #[tokio::test]
    async fn indexer_config_batch_size_zero_errors() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let icfg = IndexerConfig {
            batch_size: 0,
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let err = indexer.run(&storage).await.unwrap_err();
        assert!(matches!(err, IndexerError::Config(_)));
    }

    // =========================================================================
    // Multiple event types in one batch
    // =========================================================================

    #[tokio::test]
    async fn indexer_handles_mixed_event_types() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("ingress-1", 1, 0, "echo hello"),
                egress_event("egress-1", 1, 1, "hello\n"),
                control_event("ctrl-1", 1, 2),
                lifecycle_event("lc-1", 2, 3),
            ],
        )
        .await;

        let icfg = test_indexer_config(dir.path());
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let result = indexer.run(&storage).await.unwrap();

        assert_eq!(result.events_indexed, 4);
        let docs = &indexer.writer().docs;
        assert_eq!(docs[0].event_type, "ingress_text");
        assert_eq!(docs[1].event_type, "egress_output");
        assert_eq!(docs[2].event_type, "control_marker");
        assert_eq!(docs[3].event_type, "lifecycle_marker");
    }

    // =========================================================================
    // Lag monitor tests
    // =========================================================================

    #[tokio::test]
    async fn lag_monitor_no_events() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let lag = compute_indexer_lag(&storage, "test-consumer")
            .await
            .unwrap();
        assert_eq!(lag.log_head_ordinal, None);
        assert_eq!(lag.indexer_ordinal, None);
        assert_eq!(lag.records_behind, 0);
        assert!(lag.caught_up);
    }

    #[tokio::test]
    async fn lag_monitor_with_events_no_checkpoint() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("e1", 1, 0, "a"),
                sample_event("e2", 1, 1, "b"),
                sample_event("e3", 1, 2, "c"),
            ],
        )
        .await;

        let lag = compute_indexer_lag(&storage, "test-consumer")
            .await
            .unwrap();
        assert_eq!(lag.log_head_ordinal, Some(2));
        assert_eq!(lag.indexer_ordinal, None);
        assert_eq!(lag.records_behind, 3);
        assert!(!lag.caught_up);
    }

    #[tokio::test]
    async fn lag_monitor_partially_indexed() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        // Index first 5
        let icfg = IndexerConfig {
            consumer_id: "lag-test".to_string(),
            batch_size: 5,
            max_batches: 1,
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        indexer.run(&storage).await.unwrap();

        let lag = compute_indexer_lag(&storage, "lag-test").await.unwrap();
        assert_eq!(lag.log_head_ordinal, Some(9));
        assert_eq!(lag.indexer_ordinal, Some(4));
        assert_eq!(lag.records_behind, 5);
        assert!(!lag.caught_up);
    }

    #[tokio::test]
    async fn lag_monitor_fully_caught_up() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e1", 1, 0, "a")]).await;

        let icfg = IndexerConfig {
            consumer_id: "lag-test-2".to_string(),
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        indexer.run(&storage).await.unwrap();

        let lag = compute_indexer_lag(&storage, "lag-test-2").await.unwrap();
        assert_eq!(lag.records_behind, 0);
        assert!(lag.caught_up);
    }

    // =========================================================================
    // Incremental append-after-index test
    // =========================================================================

    #[tokio::test]
    async fn indexer_picks_up_new_events_after_append() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        // Initial events
        populate_log(
            &storage,
            vec![
                sample_event("e1", 1, 0, "first"),
                sample_event("e2", 1, 1, "second"),
            ],
        )
        .await;

        // First index run
        let icfg = IndexerConfig {
            consumer_id: "incr-test".to_string(),
            ..test_indexer_config(dir.path())
        };
        let mut indexer = IncrementalIndexer::new(icfg.clone(), MockIndexWriter::new());
        let r1 = indexer.run(&storage).await.unwrap();
        assert_eq!(r1.events_indexed, 2);
        assert!(r1.caught_up);

        // Append more events
        storage
            .append_batch(AppendRequest {
                batch_id: "late-batch".to_string(),
                events: vec![
                    sample_event("e3", 2, 2, "third"),
                    sample_event("e4", 2, 3, "fourth"),
                ],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 999,
            })
            .await
            .unwrap();

        // Second index run picks up only new events
        let mut indexer2 = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let r2 = indexer2.run(&storage).await.unwrap();
        assert_eq!(r2.events_indexed, 2);
        assert_eq!(r2.final_ordinal, Some(3));
        assert!(r2.caught_up);

        assert_eq!(indexer2.writer().docs[0].event_id, "e3");
        assert_eq!(indexer2.writer().docs[1].event_id, "e4");
    }

    // =========================================================================
    // Checkpoint monotonicity across multiple runs
    // =========================================================================

    #[tokio::test]
    async fn checkpoint_advances_monotonically_across_runs() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..9)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let consumer = "mono-test";
        let icfg = IndexerConfig {
            consumer_id: consumer.to_string(),
            batch_size: 3,
            max_batches: 1,
            ..test_indexer_config(dir.path())
        };

        let mut prev_ordinal: Option<u64> = None;
        for _ in 0..3 {
            let mut indexer = IncrementalIndexer::new(icfg.clone(), MockIndexWriter::new());
            let result = indexer.run(&storage).await.unwrap();

            let current = result.final_ordinal.unwrap();
            if let Some(prev) = prev_ordinal {
                assert!(
                    current > prev,
                    "checkpoint must advance: {current} > {prev}"
                );
            }
            prev_ordinal = Some(current);
        }
        assert_eq!(prev_ordinal, Some(8));
    }

    // =========================================================================
    // Multi-pane indexing
    // =========================================================================

    #[tokio::test]
    async fn indexer_handles_multi_pane_events() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("p1-e1", 1, 0, "pane 1 first"),
                sample_event("p2-e1", 2, 0, "pane 2 first"),
                sample_event("p3-e1", 3, 0, "pane 3 first"),
                sample_event("p1-e2", 1, 1, "pane 1 second"),
            ],
        )
        .await;

        let icfg = test_indexer_config(dir.path());
        let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
        let result = indexer.run(&storage).await.unwrap();

        assert_eq!(result.events_indexed, 4);
        let pane_ids: Vec<u64> = indexer.writer().docs.iter().map(|d| d.pane_id).collect();
        assert_eq!(pane_ids, vec![1, 2, 3, 1]);
    }

    // =========================================================================
    // Causality fields preserved
    // =========================================================================

    #[test]
    fn causality_fields_preserved_in_document() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "causal-1".to_string(),
            pane_id: 1,
            session_id: Some("s1".to_string()),
            workflow_id: Some("wf1".to_string()),
            correlation_id: Some("c1".to_string()),
            source: RecorderEventSource::WorkflowEngine,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 5,
            causality: RecorderEventCausality {
                parent_event_id: Some("parent-x".to_string()),
                trigger_event_id: Some("trigger-y".to_string()),
                root_event_id: Some("root-z".to_string()),
            },
            payload: RecorderEventPayload::IngressText {
                text: "test".to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::WorkflowAction,
            },
        };

        let doc = map_event_to_document(&event, 99);
        assert_eq!(doc.parent_event_id, Some("parent-x".to_string()));
        assert_eq!(doc.trigger_event_id, Some("trigger-y".to_string()));
        assert_eq!(doc.root_event_id, Some("root-z".to_string()));
        assert_eq!(doc.workflow_id, Some("wf1".to_string()));
        assert_eq!(doc.correlation_id, Some("c1".to_string()));
        assert_eq!(doc.source, "workflow_engine");
        assert_eq!(doc.ingress_kind, Some("workflow_action".to_string()));
    }

    // =========================================================================
    // IndexWriter error types
    // =========================================================================

    #[test]
    fn index_write_error_display() {
        let e1 = IndexWriteError::Rejected {
            reason: "bad".to_string(),
        };
        assert!(e1.to_string().contains("rejected"));

        let e2 = IndexWriteError::Transient {
            reason: "busy".to_string(),
        };
        assert!(e2.to_string().contains("transient"));

        let e3 = IndexWriteError::CommitFailed {
            reason: "disk".to_string(),
        };
        assert!(e3.to_string().contains("commit"));
    }

    #[test]
    fn indexer_error_display() {
        let e1 = IndexerError::Config("bad".to_string());
        assert!(e1.to_string().contains("config"));

        let e2 = IndexerError::LogRead(LogReadError::Corrupt {
            byte_offset: 42,
            reason: "bad".to_string(),
        });
        assert!(e2.to_string().contains("log read"));
    }

    // =========================================================================
    // Default config values
    // =========================================================================

    #[test]
    fn default_indexer_config() {
        let cfg = IndexerConfig::default();
        assert_eq!(cfg.consumer_id, LEXICAL_INDEXER_CONSUMER);
        assert_eq!(cfg.batch_size, 512);
        assert!(cfg.dedup_on_replay);
        assert_eq!(cfg.max_batches, 0);
        assert_eq!(cfg.expected_event_schema, RECORDER_EVENT_SCHEMA_VERSION_V1);
    }

    #[test]
    fn lexical_schema_version_is_v1() {
        assert_eq!(LEXICAL_SCHEMA_VERSION, "ft.recorder.lexical.v1");
    }
}
