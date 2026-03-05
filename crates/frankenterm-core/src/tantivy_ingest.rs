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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        occurred_at_ms: i64::try_from(event.occurred_at_ms).unwrap_or(i64::MAX),
        recorded_at_ms: i64::try_from(event.recorded_at_ms).unwrap_or(i64::MAX),
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
    use crate::recording::RecorderEventSource::{
        OperatorAction, RecoveryFlow, RobotMode, WeztermMux, WorkflowEngine,
    };
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
    use crate::recording::RecorderIngressKind::{Paste, SendText, WorkflowAction};
    match k {
        SendText => "send_text",
        Paste => "paste",
        WorkflowAction => "workflow_action",
    }
    .to_string()
}

fn format_segment_kind(k: crate::recording::RecorderSegmentKind) -> String {
    use crate::recording::RecorderSegmentKind::{Delta, Gap, Snapshot};
    match k {
        Delta => "delta",
        Gap => "gap",
        Snapshot => "snapshot",
    }
    .to_string()
}

fn format_control_marker(t: crate::recording::RecorderControlMarkerType) -> String {
    use crate::recording::RecorderControlMarkerType::{
        ApprovalCheckpoint, PolicyDecision, PromptBoundary, Resize,
    };
    match t {
        PromptBoundary => "prompt_boundary",
        Resize => "resize",
        PolicyDecision => "policy_decision",
        ApprovalCheckpoint => "approval_checkpoint",
    }
    .to_string()
}

fn format_lifecycle_phase(p: crate::recording::RecorderLifecyclePhase) -> String {
    use crate::recording::RecorderLifecyclePhase::{
        CaptureStarted, CaptureStopped, PaneClosed, PaneOpened, ReplayFinished, ReplayStarted,
    };
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

        // Guard against corrupt files with impossibly large payload lengths
        const MAX_RECORD_PAYLOAD: u64 = 64 * 1024 * 1024; // 64 MiB
        if payload_len > MAX_RECORD_PAYLOAD {
            return Err(LogReadError::Corrupt {
                byte_offset: record_start,
                reason: format!(
                    "record payload too large: {payload_len} bytes (max {MAX_RECORD_PAYLOAD})"
                ),
            });
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
// AppendLogEventSource — RecorderEventReader backed by append-log file
// ---------------------------------------------------------------------------

/// Wraps the append-log data file to implement [`RecorderEventReader`].
///
/// This is the append-log concrete implementation of the backend-neutral
/// event reader seam. It delegates to [`AppendLogReader`] for sequential
/// record iteration.
pub struct AppendLogEventSource {
    data_path: PathBuf,
}

impl AppendLogEventSource {
    /// Create from an existing [`AppendLogRecorderStorage`].
    pub fn from_storage(storage: &crate::recorder_storage::AppendLogRecorderStorage) -> Self {
        Self {
            data_path: storage.data_path().to_path_buf(),
        }
    }

    /// Create from a raw data path.
    pub fn from_path(data_path: PathBuf) -> Self {
        Self { data_path }
    }
}

impl crate::recorder_storage::RecorderEventReader for AppendLogEventSource {
    fn open_cursor(
        &self,
        from: RecorderOffset,
    ) -> std::result::Result<
        Box<dyn crate::recorder_storage::RecorderEventCursor>,
        crate::recorder_storage::EventCursorError,
    > {
        use crate::recorder_storage::EventCursorError;
        tracing::debug!(
            source = "append_log",
            from_offset = %from.ordinal,
            from_byte = %from.byte_offset,
            "opening event cursor"
        );
        let reader = if from.byte_offset == 0 && from.ordinal == 0 {
            AppendLogReader::open(&self.data_path)
                .map_err(|e| EventCursorError::Io(e.to_string()))?
        } else {
            AppendLogReader::open_at_offset(&self.data_path, from.byte_offset, from.ordinal)
                .map_err(|e| EventCursorError::Io(e.to_string()))?
        };
        Ok(Box::new(AppendLogCursor { reader }))
    }

    fn open_cursor_at_ordinal(
        &self,
        target_ordinal: u64,
    ) -> std::result::Result<
        Box<dyn crate::recorder_storage::RecorderEventCursor>,
        crate::recorder_storage::EventCursorError,
    > {
        use crate::recorder_storage::EventCursorError;
        let reader = AppendLogReader::open_at_ordinal(&self.data_path, target_ordinal)
            .map_err(|e| EventCursorError::Io(e.to_string()))?;
        Ok(Box::new(AppendLogCursor { reader }))
    }

    fn head_offset(
        &self,
    ) -> std::result::Result<RecorderOffset, crate::recorder_storage::EventCursorError> {
        use crate::recorder_storage::EventCursorError;
        let file_len = std::fs::metadata(&self.data_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if file_len == 0 {
            return Ok(RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            });
        }
        let mut reader = AppendLogReader::open(&self.data_path)
            .map_err(|e| EventCursorError::Io(e.to_string()))?;
        loop {
            match reader.next_record() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => return Err(EventCursorError::Io(e.to_string())),
            }
        }
        Ok(RecorderOffset {
            segment_id: 0,
            byte_offset: reader.byte_offset(),
            ordinal: reader.next_ordinal(),
        })
    }
}

/// Cursor wrapping [`AppendLogReader`] for the [`RecorderEventCursor`](crate::recorder_storage::RecorderEventCursor) trait.
struct AppendLogCursor {
    reader: AppendLogReader,
}

impl crate::recorder_storage::RecorderEventCursor for AppendLogCursor {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> std::result::Result<
        Vec<crate::recorder_storage::CursorRecord>,
        crate::recorder_storage::EventCursorError,
    > {
        use crate::recorder_storage::{CursorRecord, EventCursorError};
        let records = self
            .reader
            .read_batch(max)
            .map_err(|e| EventCursorError::Io(e.to_string()))?;
        Ok(records
            .into_iter()
            .map(|r| CursorRecord {
                event: r.event,
                offset: r.offset,
            })
            .collect())
    }

    fn current_offset(&self) -> RecorderOffset {
        RecorderOffset {
            segment_id: 0,
            byte_offset: self.reader.byte_offset(),
            ordinal: self.reader.next_ordinal(),
        }
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
    /// Backend-neutral source descriptor. Replaces the former `data_path` field.
    pub source: crate::recorder_storage::RecorderSourceDescriptor,
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

impl IndexerConfig {
    /// Convenience accessor for the data path when the source is append-log.
    /// Returns `None` for non-file backends.
    pub fn data_path(&self) -> Option<&Path> {
        match &self.source {
            crate::recorder_storage::RecorderSourceDescriptor::AppendLog { data_path } => {
                Some(data_path)
            }
            crate::recorder_storage::RecorderSourceDescriptor::FrankenSqlite { .. } => None,
        }
    }

    /// Create a [`RecorderEventReader`](crate::recorder_storage::RecorderEventReader)
    /// from this config's source descriptor.
    pub fn create_event_reader(
        &self,
    ) -> Result<Box<dyn crate::recorder_storage::RecorderEventReader>, IndexerError> {
        match &self.source {
            crate::recorder_storage::RecorderSourceDescriptor::AppendLog { data_path } => {
                tracing::info!(
                    indexer_source = %self.source,
                    "creating append-log event reader"
                );
                Ok(Box::new(AppendLogEventSource::from_path(data_path.clone())))
            }
            crate::recorder_storage::RecorderSourceDescriptor::FrankenSqlite { db_path } => {
                tracing::info!(
                    indexer_source = %self.source,
                    db_path = %db_path.display(),
                    "frankensqlite event reader not yet implemented"
                );
                Err(IndexerError::Config(
                    "frankensqlite event reader not yet implemented".to_string(),
                ))
            }
        }
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            source: crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
                data_path: PathBuf::from(".ft/recorder-log/events.log"),
            },
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
        let data_path = self.config.data_path().ok_or_else(|| {
            IndexerError::Config(
                "run() requires an AppendLog source; use run_with_reader() for other backends"
                    .to_string(),
            )
        })?;
        let mut reader = match &checkpoint {
            Some(cp) => {
                // Resume from the record AFTER the last checkpointed ordinal.
                let resume_byte = cp.upto_offset.byte_offset;
                let resume_ordinal = cp.upto_offset.ordinal;
                // We need to position at the byte AFTER the checkpointed record.
                // The checkpoint stores the offset of the last processed record,
                // so we need to seek past it. We open at the checkpoint's byte
                // offset and skip one record to get past it.
                let mut r =
                    AppendLogReader::open_at_offset(data_path, resume_byte, resume_ordinal)?;
                // Skip the checkpointed record itself
                let _ = r.next_record()?;
                r
            }
            None => AppendLogReader::open(data_path)?,
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
                if self.config.dedup_on_replay
                    && self.writer.delete_by_event_id(&doc.event_id).is_err()
                {
                    // Deletion failure on a non-existent doc is fine; only
                    // propagate genuine failures on commit.
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

    /// Run the indexer using a backend-neutral [`RecorderEventReader`].
    ///
    /// This is the preferred entry point for backend-agnostic indexing.
    /// The reader provides a cursor over recorder events regardless of whether
    /// the underlying storage is an append-log file or FrankenSqlite.
    pub async fn run_with_reader<S: RecorderStorage>(
        &mut self,
        storage: &S,
        reader: &dyn crate::recorder_storage::RecorderEventReader,
    ) -> Result<IndexerRunResult, IndexerError> {
        if self.config.batch_size == 0 {
            return Err(IndexerError::Config("batch_size must be >= 1".to_string()));
        }

        let consumer_id = CheckpointConsumerId(self.config.consumer_id.clone());

        // 1. Read last checkpoint
        let checkpoint = storage.read_checkpoint(&consumer_id).await?;

        // 2. Open cursor at resume point
        let mut cursor = match &checkpoint {
            Some(cp) => {
                let mut c = reader.open_cursor(cp.upto_offset.clone()).map_err(|e| {
                    IndexerError::LogRead(LogReadError::Io(std::io::Error::other(e.to_string())))
                })?;
                // Skip the checkpointed record itself
                let _ = c.next_batch(1).map_err(|e| {
                    IndexerError::LogRead(LogReadError::Io(std::io::Error::other(e.to_string())))
                })?;
                tracing::debug!(
                    cursor = "resume",
                    from_ordinal = %cp.upto_offset.ordinal,
                    "cursor opened at checkpoint"
                );
                c
            }
            None => {
                tracing::debug!(cursor = "cold_start", "cursor opened from start");
                reader.open_cursor_from_start().map_err(|e| {
                    IndexerError::LogRead(LogReadError::Io(std::io::Error::other(e.to_string())))
                })?
            }
        };

        let mut result = IndexerRunResult {
            events_read: 0,
            events_indexed: 0,
            events_skipped: 0,
            batches_committed: 0,
            final_ordinal: checkpoint.as_ref().map(|cp| cp.upto_offset.ordinal),
            caught_up: false,
        };

        // 3. Process batches via cursor
        loop {
            if self.config.max_batches > 0
                && result.batches_committed >= self.config.max_batches as u64
            {
                break;
            }

            let batch = cursor.next_batch(self.config.batch_size).map_err(|e| {
                IndexerError::LogRead(LogReadError::Io(std::io::Error::other(e.to_string())))
            })?;
            if batch.is_empty() {
                result.caught_up = true;
                break;
            }

            let is_final_batch = batch.len() < self.config.batch_size;
            let mut last_offset: Option<RecorderOffset> = None;

            for record in &batch {
                result.events_read += 1;

                if record.event.schema_version != self.config.expected_event_schema {
                    result.events_skipped += 1;
                    last_offset = Some(record.offset.clone());
                    continue;
                }

                let doc = map_event_to_document(&record.event, record.offset.ordinal);

                if self.config.dedup_on_replay {
                    let _ = self.writer.delete_by_event_id(&doc.event_id);
                }

                match self.writer.add_document(&doc) {
                    Ok(()) => result.events_indexed += 1,
                    Err(IndexWriteError::Rejected { .. }) => result.events_skipped += 1,
                    Err(e) => return Err(e.into()),
                }

                last_offset = Some(record.offset.clone());
            }

            self.writer.commit()?;

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

            tracing::debug!(
                cursor_batch = batch.len(),
                current_offset = %cursor.current_offset().ordinal,
                "batch processed via cursor"
            );

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
        RecorderEventReader,
    };
    use crate::recording::{
        RecorderControlMarkerType, RecorderEventCausality, RecorderEventPayload,
        RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
        RecorderSegmentKind, RecorderTextEncoding,
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
            .expect("failed to build tantivy_ingest test runtime");
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
            source: crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
                data_path: path.join("events.log"),
            },
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

        fn written_docs(&self) -> &[IndexDocumentFields] {
            &self.docs
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

    #[test]
    fn reader_reads_all_events() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn reader_eof_returns_none() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            populate_log(&storage, vec![sample_event("e1", 1, 0, "only")]).await;
            drop(storage);

            let mut reader = AppendLogReader::open(&cfg.data_path).unwrap();
            let _first = reader.next_record().unwrap().unwrap();
            let second = reader.next_record().unwrap();
            assert!(second.is_none());
        });
    }

    #[test]
    fn reader_open_at_ordinal() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn reader_open_at_offset_direct() {
        run_async_test(async {
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
            let mut reader2 = AppendLogReader::open_at_offset(
                &cfg.data_path,
                offset_2.byte_offset,
                offset_2.ordinal,
            )
            .unwrap();
            let rec = reader2.next_record().unwrap().unwrap();
            assert_eq!(rec.event.event_id, "e2");
        });
    }

    #[test]
    fn reader_batch_limits() {
        run_async_test(async {
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
        });
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

    #[test]
    fn indexer_full_pipeline_cold_start() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_resumes_from_checkpoint() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let events: Vec<_> = (0..6)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            // First run: index first 3
            let icfg = IndexerConfig {
                source: crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
                    data_path: dir.path().join("events.log"),
                },
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
        });
    }

    #[test]
    fn indexer_empty_log() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_already_caught_up() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_skips_wrong_schema_version() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_dedup_deletes_before_add() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_no_dedup_when_disabled() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_rejected_docs_are_skipped() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_max_batches_limits_processing() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_commit_failure_propagates() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_batch_size_one() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn indexer_config_batch_size_zero_errors() {
        run_async_test(async {
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
        });
    }

    // =========================================================================
    // Multiple event types in one batch
    // =========================================================================

    #[test]
    fn indexer_handles_mixed_event_types() {
        run_async_test(async {
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
        });
    }

    // =========================================================================
    // Lag monitor tests
    // =========================================================================

    #[test]
    fn lag_monitor_no_events() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn lag_monitor_with_events_no_checkpoint() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn lag_monitor_partially_indexed() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn lag_monitor_fully_caught_up() {
        run_async_test(async {
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
        });
    }

    // =========================================================================
    // Incremental append-after-index test
    // =========================================================================

    #[test]
    fn indexer_picks_up_new_events_after_append() {
        run_async_test(async {
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
        });
    }

    // =========================================================================
    // Checkpoint monotonicity across multiple runs
    // =========================================================================

    #[test]
    fn checkpoint_advances_monotonically_across_runs() {
        run_async_test(async {
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
        });
    }

    // =========================================================================
    // Multi-pane indexing
    // =========================================================================

    #[test]
    fn indexer_handles_multi_pane_events() {
        run_async_test(async {
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
        });
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

    // =========================================================================
    // NEW: format helper coverage (all enum variants)
    // =========================================================================

    #[test]
    fn format_source_all_variants() {
        use crate::recording::RecorderEventSource;
        assert_eq!(
            format_source(RecorderEventSource::WeztermMux),
            "wezterm_mux"
        );
        assert_eq!(format_source(RecorderEventSource::RobotMode), "robot_mode");
        assert_eq!(
            format_source(RecorderEventSource::WorkflowEngine),
            "workflow_engine"
        );
        assert_eq!(
            format_source(RecorderEventSource::OperatorAction),
            "operator_action"
        );
        assert_eq!(
            format_source(RecorderEventSource::RecoveryFlow),
            "recovery_flow"
        );
    }

    #[test]
    fn format_ingress_kind_all_variants() {
        assert_eq!(
            format_ingress_kind(RecorderIngressKind::SendText),
            "send_text"
        );
        assert_eq!(format_ingress_kind(RecorderIngressKind::Paste), "paste");
        assert_eq!(
            format_ingress_kind(RecorderIngressKind::WorkflowAction),
            "workflow_action"
        );
    }

    #[test]
    fn format_segment_kind_all_variants() {
        assert_eq!(format_segment_kind(RecorderSegmentKind::Delta), "delta");
        assert_eq!(format_segment_kind(RecorderSegmentKind::Gap), "gap");
        assert_eq!(
            format_segment_kind(RecorderSegmentKind::Snapshot),
            "snapshot"
        );
    }

    #[test]
    fn format_control_marker_all_variants() {
        assert_eq!(
            format_control_marker(RecorderControlMarkerType::PromptBoundary),
            "prompt_boundary"
        );
        assert_eq!(
            format_control_marker(RecorderControlMarkerType::Resize),
            "resize"
        );
        assert_eq!(
            format_control_marker(RecorderControlMarkerType::PolicyDecision),
            "policy_decision"
        );
        assert_eq!(
            format_control_marker(RecorderControlMarkerType::ApprovalCheckpoint),
            "approval_checkpoint"
        );
    }

    #[test]
    fn format_lifecycle_phase_all_variants() {
        assert_eq!(
            format_lifecycle_phase(RecorderLifecyclePhase::CaptureStarted),
            "capture_started"
        );
        assert_eq!(
            format_lifecycle_phase(RecorderLifecyclePhase::CaptureStopped),
            "capture_stopped"
        );
        assert_eq!(
            format_lifecycle_phase(RecorderLifecyclePhase::PaneOpened),
            "pane_opened"
        );
        assert_eq!(
            format_lifecycle_phase(RecorderLifecyclePhase::PaneClosed),
            "pane_closed"
        );
        assert_eq!(
            format_lifecycle_phase(RecorderLifecyclePhase::ReplayStarted),
            "replay_started"
        );
        assert_eq!(
            format_lifecycle_phase(RecorderLifecyclePhase::ReplayFinished),
            "replay_finished"
        );
    }

    #[test]
    fn format_redaction_all_variants() {
        assert_eq!(format_redaction(RecorderRedactionLevel::None), "none");
        assert_eq!(format_redaction(RecorderRedactionLevel::Partial), "partial");
        assert_eq!(format_redaction(RecorderRedactionLevel::Full), "full");
    }

    #[test]
    fn redacted_text_none_preserves_content() {
        assert_eq!(
            redacted_text("secret data", RecorderRedactionLevel::None),
            "secret data"
        );
    }

    #[test]
    fn redacted_text_partial_replaces() {
        assert_eq!(
            redacted_text("secret data", RecorderRedactionLevel::Partial),
            "[REDACTED]"
        );
    }

    #[test]
    fn redacted_text_full_empties() {
        assert_eq!(
            redacted_text("secret data", RecorderRedactionLevel::Full),
            ""
        );
    }

    #[test]
    fn redacted_text_empty_input_none() {
        assert_eq!(redacted_text("", RecorderRedactionLevel::None), "");
    }

    #[test]
    fn redacted_text_empty_input_partial() {
        assert_eq!(
            redacted_text("", RecorderRedactionLevel::Partial),
            "[REDACTED]"
        );
    }

    // =========================================================================
    // NEW: Document mapper edge cases
    // =========================================================================

    #[test]
    fn map_ingress_paste_variant() {
        let mut event = sample_event("ev-paste", 1, 0, "pasted text");
        if let RecorderEventPayload::IngressText {
            ref mut ingress_kind,
            ..
        } = event.payload
        {
            *ingress_kind = RecorderIngressKind::Paste;
        }
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.ingress_kind, Some("paste".to_string()));
    }

    #[test]
    fn map_egress_snapshot_variant() {
        let mut event = egress_event("ev-snap", 1, 0, "snapshot text");
        if let RecorderEventPayload::EgressOutput {
            ref mut segment_kind,
            ..
        } = event.payload
        {
            *segment_kind = RecorderSegmentKind::Snapshot;
        }
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.segment_kind, Some("snapshot".to_string()));
    }

    #[test]
    fn map_control_resize_variant() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ctrl-resize".to_string(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::Resize,
                details: serde_json::json!({"cols": 120, "rows": 40}),
            },
        };
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.control_marker_type, Some("resize".to_string()));
        assert!(doc.details_json.contains("120"));
        assert!(doc.details_json.contains("40"));
        assert_eq!(doc.text, "");
        assert_eq!(doc.ingress_kind, None);
        assert_eq!(doc.segment_kind, None);
        assert_eq!(doc.lifecycle_phase, None);
        assert_eq!(doc.redaction, None);
    }

    #[test]
    fn map_lifecycle_no_reason() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "lc-noreason".to_string(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: RecorderLifecyclePhase::CaptureStopped,
                reason: None,
                details: serde_json::json!({}),
            },
        };
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.lifecycle_phase, Some("capture_stopped".to_string()));
        assert_eq!(doc.text, "");
    }

    #[test]
    fn map_event_text_symbols_matches_text() {
        let event = sample_event("ev-sym", 1, 0, "ls -la /tmp");
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.text, doc.text_symbols);
    }

    #[test]
    fn map_event_none_optional_fields() {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "ev-none".to_string(),
            pane_id: 0,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::OperatorAction,
            occurred_at_ms: 0,
            recorded_at_ms: 0,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: "x".to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.session_id, None);
        assert_eq!(doc.workflow_id, None);
        assert_eq!(doc.correlation_id, None);
        assert_eq!(doc.parent_event_id, None);
        assert_eq!(doc.trigger_event_id, None);
        assert_eq!(doc.root_event_id, None);
        assert_eq!(doc.pane_id, 0);
        assert_eq!(doc.source, "operator_action");
    }

    #[test]
    fn map_event_large_pane_id() {
        let event = sample_event("ev-big", u64::MAX, 0, "big pane");
        let doc = map_event_to_document(&event, u64::MAX);
        assert_eq!(doc.pane_id, u64::MAX);
        assert_eq!(doc.log_offset, u64::MAX);
    }

    #[test]
    fn map_egress_redacted_partial() {
        let mut event = egress_event("ev-egrp", 1, 0, "secret output");
        if let RecorderEventPayload::EgressOutput {
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
    fn map_egress_redacted_full() {
        let mut event = egress_event("ev-egrf", 1, 0, "secret output");
        if let RecorderEventPayload::EgressOutput {
            ref mut redaction, ..
        } = event.payload
        {
            *redaction = RecorderRedactionLevel::Full;
        }
        let doc = map_event_to_document(&event, 0);
        assert_eq!(doc.text, "");
        assert_eq!(doc.text_symbols, "");
        assert_eq!(doc.redaction, Some("full".to_string()));
    }

    // =========================================================================
    // NEW: Serde roundtrip edge cases
    // =========================================================================

    #[test]
    fn document_fields_serde_roundtrip_egress() {
        let event = egress_event("ev-eg-rt", 99, 42, "output data");
        let doc = map_event_to_document(&event, 77);
        let json = serde_json::to_string(&doc).unwrap();
        let deser: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, deser);
    }

    #[test]
    fn document_fields_serde_roundtrip_control() {
        let event = control_event("ev-ct-rt", 5, 10);
        let doc = map_event_to_document(&event, 200);
        let json = serde_json::to_string(&doc).unwrap();
        let deser: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, deser);
    }

    #[test]
    fn document_fields_serde_roundtrip_lifecycle() {
        let event = lifecycle_event("ev-lc-rt", 8, 20);
        let doc = map_event_to_document(&event, 300);
        let json = serde_json::to_string(&doc).unwrap();
        let deser: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, deser);
    }

    #[test]
    fn document_fields_serde_all_none_optionals() {
        let doc = IndexDocumentFields {
            schema_version: "v1".to_string(),
            lexical_schema_version: "lv1".to_string(),
            event_id: "eid".to_string(),
            pane_id: 0,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            source: "test".to_string(),
            event_type: "test".to_string(),
            ingress_kind: None,
            segment_kind: None,
            control_marker_type: None,
            lifecycle_phase: None,
            is_gap: false,
            redaction: None,
            occurred_at_ms: 0,
            recorded_at_ms: 0,
            sequence: 0,
            log_offset: 0,
            text: String::new(),
            text_symbols: String::new(),
            details_json: "{}".to_string(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let deser: IndexDocumentFields = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, deser);
        assert!(json.contains("\"session_id\":null"));
    }

    // =========================================================================
    // NEW: Display/Debug impls
    // =========================================================================

    #[test]
    fn log_read_error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = LogReadError::Io(io_err);
        let msg = err.to_string();
        assert!(msg.contains("log read I/O error"));
        assert!(msg.contains("file missing"));
    }

    #[test]
    fn log_read_error_corrupt_display() {
        let err = LogReadError::Corrupt {
            byte_offset: 1024,
            reason: "bad header".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("corrupt record at byte 1024"));
        assert!(msg.contains("bad header"));
    }

    #[test]
    fn log_read_error_deserialize_display() {
        let json_err = serde_json::from_str::<serde_json::Value>("{{bad}}").unwrap_err();
        let err = LogReadError::Deserialize {
            byte_offset: 256,
            source: json_err,
        };
        let msg = err.to_string();
        assert!(msg.contains("JSON error at byte 256"));
    }

    #[test]
    fn log_read_error_is_std_error() {
        let err = LogReadError::Corrupt {
            byte_offset: 0,
            reason: "test".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn log_read_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: LogReadError = io_err.into();
        assert!(matches!(err, LogReadError::Io(_)));
        assert!(err.to_string().contains("denied"));
    }

    #[test]
    fn index_write_error_transient_display() {
        let err = IndexWriteError::Transient {
            reason: "resource busy".to_string(),
        };
        assert!(err.to_string().contains("transient index error"));
        assert!(err.to_string().contains("resource busy"));
    }

    #[test]
    fn index_write_error_is_std_error() {
        let err = IndexWriteError::Rejected {
            reason: "x".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn indexer_error_from_log_read() {
        let lre = LogReadError::Corrupt {
            byte_offset: 10,
            reason: "bad".to_string(),
        };
        let err: IndexerError = lre.into();
        assert!(matches!(err, IndexerError::LogRead(_)));
        assert!(err.to_string().contains("log read"));
    }

    #[test]
    fn indexer_error_from_index_write() {
        let iwe = IndexWriteError::CommitFailed {
            reason: "disk full".to_string(),
        };
        let err: IndexerError = iwe.into();
        assert!(matches!(err, IndexerError::IndexWrite(_)));
        assert!(err.to_string().contains("index write"));
        assert!(err.to_string().contains("disk full"));
    }

    #[test]
    fn indexer_error_config_display() {
        let err = IndexerError::Config("batch_size must be >= 1".to_string());
        assert_eq!(err.to_string(), "config: batch_size must be >= 1");
    }

    #[test]
    fn indexer_error_is_std_error() {
        let err = IndexerError::Config("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    // =========================================================================
    // NEW: IndexerConfig Debug and Clone
    // =========================================================================

    #[test]
    fn indexer_config_debug_impl() {
        let cfg = IndexerConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("IndexerConfig"));
        assert!(dbg.contains("batch_size"));
    }

    #[test]
    fn indexer_config_clone() {
        let cfg = IndexerConfig {
            source: crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
                data_path: PathBuf::from("/tmp/test.log"),
            },
            consumer_id: "clone-test".to_string(),
            batch_size: 99,
            dedup_on_replay: false,
            max_batches: 5,
            expected_event_schema: "v99".to_string(),
        };
        let cloned = cfg.clone();
        assert_eq!(cloned.source, cfg.source);
        assert_eq!(cloned.consumer_id, cfg.consumer_id);
        assert_eq!(cloned.batch_size, cfg.batch_size);
        assert_eq!(cloned.dedup_on_replay, cfg.dedup_on_replay);
        assert_eq!(cloned.max_batches, cfg.max_batches);
        assert_eq!(cloned.expected_event_schema, cfg.expected_event_schema);
    }

    #[test]
    fn default_config_is_append_log() {
        let cfg = IndexerConfig::default();
        assert_eq!(
            cfg.source,
            crate::recorder_storage::RecorderSourceDescriptor::AppendLog {
                data_path: PathBuf::from(".ft/recorder-log/events.log"),
            }
        );
        assert_eq!(
            cfg.data_path(),
            Some(Path::new(".ft/recorder-log/events.log"))
        );
    }

    // =========================================================================
    // NEW: IndexCommitStats
    // =========================================================================

    #[test]
    fn index_commit_stats_clone_eq() {
        let stats = IndexCommitStats {
            docs_added: 10,
            docs_deleted: 2,
            segment_count: 3,
        };
        let cloned = stats.clone();
        assert_eq!(stats, cloned);
    }

    #[test]
    fn index_commit_stats_debug() {
        let stats = IndexCommitStats {
            docs_added: 0,
            docs_deleted: 0,
            segment_count: 1,
        };
        let dbg = format!("{:?}", stats);
        assert!(dbg.contains("IndexCommitStats"));
    }

    // =========================================================================
    // NEW: IndexerRunResult
    // =========================================================================

    #[test]
    fn indexer_run_result_clone_eq() {
        let r1 = IndexerRunResult {
            events_read: 100,
            events_indexed: 95,
            events_skipped: 5,
            batches_committed: 10,
            final_ordinal: Some(99),
            caught_up: true,
        };
        let r2 = r1.clone();
        assert_eq!(r1, r2);
    }

    #[test]
    fn indexer_run_result_debug() {
        let result = IndexerRunResult {
            events_read: 0,
            events_indexed: 0,
            events_skipped: 0,
            batches_committed: 0,
            final_ordinal: None,
            caught_up: false,
        };
        let dbg = format!("{:?}", result);
        assert!(dbg.contains("IndexerRunResult"));
        assert!(dbg.contains("caught_up"));
    }

    // =========================================================================
    // NEW: IndexerLagSnapshot
    // =========================================================================

    #[test]
    fn lag_snapshot_clone_eq() {
        let s = IndexerLagSnapshot {
            log_head_ordinal: Some(100),
            indexer_ordinal: Some(50),
            records_behind: 50,
            caught_up: false,
        };
        assert_eq!(s, s.clone());
    }

    #[test]
    fn lag_snapshot_debug() {
        let s = IndexerLagSnapshot {
            log_head_ordinal: None,
            indexer_ordinal: None,
            records_behind: 0,
            caught_up: true,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("IndexerLagSnapshot"));
    }

    // =========================================================================
    // NEW: LogRecord Debug/Clone
    // =========================================================================

    #[test]
    fn log_record_debug_clone() {
        let event = sample_event("lr-1", 1, 0, "test");
        let record = LogRecord {
            event,
            offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
        };
        let cloned = record.clone();
        assert_eq!(cloned.event.event_id, "lr-1");
        assert_eq!(cloned.offset.ordinal, 0);
        let dbg = format!("{:?}", record);
        assert!(dbg.contains("LogRecord"));
    }

    // =========================================================================
    // NEW: Append-log reader edge cases
    // =========================================================================

    #[test]
    fn reader_nonexistent_file() {
        let result = AppendLogReader::open(Path::new("/nonexistent/path/file.log"));
        assert!(result.is_err());
    }

    #[test]
    fn reader_corrupt_json_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("corrupt_json.log");

        let payload = b"this is not json";
        let mut data = Vec::new();
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(payload);
        std::fs::write(&path, &data).unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        let err = reader.next_record().unwrap_err();
        assert!(matches!(err, LogReadError::Deserialize { .. }));
        match err {
            LogReadError::Deserialize { byte_offset, .. } => assert_eq!(byte_offset, 0),
            _ => panic!("expected Deserialize"),
        }
    }

    #[test]
    fn reader_only_length_header_no_payload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("header_only.log");

        // Write a length header claiming 100 bytes but provide 0 bytes of payload
        let mut data = Vec::new();
        data.extend_from_slice(&(100u32).to_le_bytes());
        std::fs::write(&path, &data).unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        // Torn tail: length header says 100 bytes but file only has 4 bytes total
        let record = reader.next_record().unwrap();
        assert!(record.is_none());
    }

    #[test]
    fn reader_partial_length_header() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial_header.log");

        // Only 2 bytes — not enough for a 4-byte length header
        std::fs::write(&path, [0u8, 1u8]).unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        let record = reader.next_record().unwrap();
        assert!(record.is_none());
    }

    #[test]
    fn reader_batch_zero_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("zero_batch.log");

        let event = sample_event("e1", 1, 0, "hello");
        let payload = serde_json::to_vec(&event).unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&payload);
        std::fs::write(&path, &data).unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        let batch = reader.read_batch(0).unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn reader_byte_offset_and_ordinal_accessors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("accessors.log");

        let event = sample_event("e1", 1, 0, "hello");
        let payload = serde_json::to_vec(&event).unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&payload);
        std::fs::write(&path, &data).unwrap();

        let mut reader = AppendLogReader::open(&path).unwrap();
        assert_eq!(reader.byte_offset(), 0);
        assert_eq!(reader.next_ordinal(), 0);

        let _ = reader.next_record().unwrap();
        assert_eq!(reader.byte_offset(), 4 + payload.len() as u64);
        assert_eq!(reader.next_ordinal(), 1);
    }

    #[test]
    fn reader_skip_to_ordinal_beyond_eof() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            populate_log(&storage, vec![sample_event("e1", 1, 0, "only")]).await;
            drop(storage);

            let result = AppendLogReader::open_at_ordinal(&cfg.data_path, 5);
            assert!(result.is_err());
            if let Err(LogReadError::Corrupt { reason, .. }) = result {
                assert!(reason.contains("EOF before reaching ordinal"));
            } else {
                panic!("expected Corrupt error");
            }
        });
    }

    #[test]
    fn reader_multiple_next_record_past_eof() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let cfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            populate_log(&storage, vec![sample_event("e1", 1, 0, "only")]).await;
            drop(storage);

            let mut reader = AppendLogReader::open(&cfg.data_path).unwrap();
            let _ = reader.next_record().unwrap().unwrap();
            // Multiple calls past EOF should all return None safely
            assert!(reader.next_record().unwrap().is_none());
            assert!(reader.next_record().unwrap().is_none());
            assert!(reader.next_record().unwrap().is_none());
        });
    }

    // =========================================================================
    // NEW: IncrementalIndexer into_writer / writer accessors
    // =========================================================================

    #[test]
    fn indexer_writer_accessor() {
        let cfg = test_indexer_config(Path::new("/tmp"));
        let writer = MockIndexWriter::new();
        let indexer = IncrementalIndexer::new(cfg, writer);
        assert_eq!(indexer.writer().docs.len(), 0);
        assert_eq!(indexer.writer().commits, 0);
    }

    #[test]
    fn indexer_into_writer() {
        let cfg = test_indexer_config(Path::new("/tmp"));
        let mut writer = MockIndexWriter::new();
        writer.reject_event_ids = vec!["x".to_string()];
        let indexer = IncrementalIndexer::new(cfg, writer);
        let recovered = indexer.into_writer();
        assert_eq!(recovered.reject_event_ids, vec!["x".to_string()]);
    }

    // =========================================================================
    // NEW: Indexer transient error propagation
    // =========================================================================

    #[test]
    fn indexer_transient_write_error_propagates() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();
            populate_log(&storage, vec![sample_event("e1", 1, 0, "text")]).await;

            let icfg = test_indexer_config(dir.path());

            // Build a writer that returns Transient errors
            struct TransientFailWriter;
            impl IndexWriter for TransientFailWriter {
                fn add_document(
                    &mut self,
                    _doc: &IndexDocumentFields,
                ) -> Result<(), IndexWriteError> {
                    Err(IndexWriteError::Transient {
                        reason: "overloaded".to_string(),
                    })
                }
                fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
                    Ok(IndexCommitStats {
                        docs_added: 0,
                        docs_deleted: 0,
                        segment_count: 0,
                    })
                }
                fn delete_by_event_id(&mut self, _: &str) -> Result<(), IndexWriteError> {
                    Ok(())
                }
            }

            let mut indexer = IncrementalIndexer::new(icfg, TransientFailWriter);
            let err = indexer.run(&storage).await.unwrap_err();
            assert!(matches!(err, IndexerError::IndexWrite(_)));
            assert!(err.to_string().contains("overloaded"));
        });
    }

    // =========================================================================
    // NEW: All-skipped batch still advances checkpoint
    // =========================================================================

    #[test]
    fn indexer_all_events_wrong_schema_still_commits_checkpoint() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let mut ev1 = sample_event("bad-1", 1, 0, "a");
            ev1.schema_version = "ft.recorder.event.v99".to_string();
            let mut ev2 = sample_event("bad-2", 1, 1, "b");
            ev2.schema_version = "ft.recorder.event.v99".to_string();

            populate_log(&storage, vec![ev1, ev2]).await;

            let icfg = IndexerConfig {
                consumer_id: "all-skip-test".to_string(),
                ..test_indexer_config(dir.path())
            };
            let mut indexer = IncrementalIndexer::new(icfg, MockIndexWriter::new());
            let result = indexer.run(&storage).await.unwrap();

            assert_eq!(result.events_read, 2);
            assert_eq!(result.events_indexed, 0);
            assert_eq!(result.events_skipped, 2);
            assert_eq!(result.batches_committed, 1);
            assert!(result.caught_up);
            // Checkpoint should still advance
            assert!(result.final_ordinal.is_some());
        });
    }

    // =========================================================================
    // NEW: LEXICAL_INDEXER_CONSUMER constant
    // =========================================================================

    #[test]
    fn lexical_indexer_consumer_constant() {
        assert_eq!(LEXICAL_INDEXER_CONSUMER, "tantivy-lexical-v1");
    }

    // =========================================================================
    // NEW: epoch_ms_now sanity
    // =========================================================================

    #[test]
    fn epoch_ms_now_returns_reasonable_value() {
        let ms = epoch_ms_now();
        // Should be after 2024-01-01 (1_704_067_200_000) and before 2030-01-01
        assert!(ms > 1_704_067_200_000);
        assert!(ms < 1_893_456_000_000);
    }

    // =========================================================================
    // RecorderEventReader / RecorderEventCursor seam tests
    // =========================================================================

    #[test]
    fn append_log_event_source_wraps_reader_correctly() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events = vec![
                sample_event("e1", 1, 0, "hello"),
                sample_event("e2", 1, 1, "world"),
                sample_event("e3", 2, 2, "three"),
            ];
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let mut cursor = source.open_cursor_from_start().unwrap();

            let batch = cursor.next_batch(10).unwrap();
            assert_eq!(batch.len(), 3);
            assert_eq!(batch[0].event.event_id, "e1");
            assert_eq!(batch[1].event.event_id, "e2");
            assert_eq!(batch[2].event.event_id, "e3");

            // Second call returns empty (EOF)
            let batch2 = cursor.next_batch(10).unwrap();
            assert!(batch2.is_empty());
        });
    }

    #[test]
    fn event_cursor_next_batch_returns_correct_count() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events: Vec<_> = (0..5)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let mut cursor = source.open_cursor_from_start().unwrap();

            // Request batch of 2
            let b1 = cursor.next_batch(2).unwrap();
            assert_eq!(b1.len(), 2);
            assert_eq!(b1[0].event.event_id, "e0");
            assert_eq!(b1[1].event.event_id, "e1");

            // Request batch of 2 again
            let b2 = cursor.next_batch(2).unwrap();
            assert_eq!(b2.len(), 2);
            assert_eq!(b2[0].event.event_id, "e2");
            assert_eq!(b2[1].event.event_id, "e3");

            // Last batch has 1 remaining
            let b3 = cursor.next_batch(2).unwrap();
            assert_eq!(b3.len(), 1);
            assert_eq!(b3[0].event.event_id, "e4");
        });
    }

    #[test]
    fn event_cursor_offset_advances_monotonically() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events: Vec<_> = (0..4)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let mut cursor = source.open_cursor_from_start().unwrap();

            let initial = cursor.current_offset();
            assert_eq!(initial.ordinal, 0);

            let _ = cursor.next_batch(2).unwrap();
            let mid = cursor.current_offset();
            assert_eq!(mid.ordinal, 2);
            assert!(mid.byte_offset > initial.byte_offset);

            let _ = cursor.next_batch(10).unwrap();
            let end = cursor.current_offset();
            assert_eq!(end.ordinal, 4);
            assert!(end.byte_offset > mid.byte_offset);
        });
    }

    #[test]
    fn event_cursor_empty_source_returns_empty() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let _storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let source = AppendLogEventSource::from_path(scfg.data_path);
            let mut cursor = source.open_cursor_from_start().unwrap();

            let batch = cursor.next_batch(10).unwrap();
            assert!(batch.is_empty());
        });
    }

    #[test]
    fn incremental_indexer_with_injected_source() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let events = vec![
                sample_event("e1", 1, 0, "first"),
                sample_event("e2", 1, 1, "second"),
                sample_event("e3", 2, 2, "third"),
            ];
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let writer = MockIndexWriter::new();
            let icfg = test_indexer_config(dir.path());
            let mut indexer = IncrementalIndexer::new(icfg, writer);

            let result = indexer.run_with_reader(&storage, &source).await.unwrap();
            assert_eq!(result.events_read, 3);
            assert_eq!(result.events_indexed, 3);
            assert_eq!(result.events_skipped, 0);
            assert!(result.caught_up);
            assert_eq!(result.final_ordinal, Some(2));
        });
    }

    #[test]
    fn head_offset_matches_written_events() {
        run_async_test(async {
            use crate::recorder_storage::RecorderEventReader;
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let source = AppendLogEventSource::from_storage(&storage);

            // Empty log: head at 0
            let head = source.head_offset().unwrap();
            assert_eq!(head.ordinal, 0);
            assert_eq!(head.byte_offset, 0);

            // Write 3 events
            let events: Vec<_> = (0..3)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            let head = source.head_offset().unwrap();
            assert_eq!(head.ordinal, 3);
            assert!(head.byte_offset > 0);
        });
    }

    #[test]
    fn cursor_open_at_offset_skips_prior_records() {
        run_async_test(async {
            use crate::recorder_storage::RecorderEventReader;
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events: Vec<_> = (0..5)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            // Read all to find offset of record 2
            let source = AppendLogEventSource::from_storage(&storage);
            let mut cursor = source.open_cursor_from_start().unwrap();
            let all = cursor.next_batch(10).unwrap();
            let offset_2 = all[2].offset.clone();

            // Open cursor at that offset
            let mut cursor2 = source.open_cursor(offset_2).unwrap();
            let batch = cursor2.next_batch(10).unwrap();
            assert_eq!(batch.len(), 3); // records 2, 3, 4
            assert_eq!(batch[0].event.event_id, "e2");
            assert_eq!(batch[1].event.event_id, "e3");
            assert_eq!(batch[2].event.event_id, "e4");
        });
    }

    #[test]
    fn run_with_reader_matches_run_result() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let events: Vec<_> = (0..4)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            // Run via legacy path
            let icfg1 = test_indexer_config(dir.path());
            let writer1 = MockIndexWriter::new();
            let mut indexer1 = IncrementalIndexer::new(
                IndexerConfig {
                    consumer_id: "legacy-consumer".to_string(),
                    ..icfg1
                },
                writer1,
            );
            let result_legacy = indexer1.run(&storage).await.unwrap();

            // Run via reader path
            let source = AppendLogEventSource::from_storage(&storage);
            let icfg2 = test_indexer_config(dir.path());
            let writer2 = MockIndexWriter::new();
            let mut indexer2 = IncrementalIndexer::new(
                IndexerConfig {
                    consumer_id: "reader-consumer".to_string(),
                    ..icfg2
                },
                writer2,
            );
            let result_reader = indexer2.run_with_reader(&storage, &source).await.unwrap();

            // Results should match
            assert_eq!(result_legacy.events_read, result_reader.events_read);
            assert_eq!(result_legacy.events_indexed, result_reader.events_indexed);
            assert_eq!(result_legacy.events_skipped, result_reader.events_skipped);
            assert_eq!(result_legacy.caught_up, result_reader.caught_up);
            assert_eq!(result_legacy.final_ordinal, result_reader.final_ordinal);
        });
    }

    #[test]
    fn run_with_reader_resumes_from_checkpoint() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let events: Vec<_> = (0..6)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);

            // First run: process max 1 batch of 3
            let icfg = IndexerConfig {
                batch_size: 3,
                max_batches: 1,
                ..test_indexer_config(dir.path())
            };
            let writer = MockIndexWriter::new();
            let mut indexer = IncrementalIndexer::new(icfg.clone(), writer);
            let r1 = indexer.run_with_reader(&storage, &source).await.unwrap();
            assert_eq!(r1.events_read, 3);
            assert!(!r1.caught_up);

            // Second run: should pick up remaining 3
            let writer2 = MockIndexWriter::new();
            let mut indexer2 = IncrementalIndexer::new(icfg, writer2);
            let r2 = indexer2.run_with_reader(&storage, &source).await.unwrap();
            assert_eq!(r2.events_read, 3);
            assert!(r2.caught_up);
            assert_eq!(r2.final_ordinal, Some(5));
        });
    }

    #[test]
    fn event_source_from_path_works() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

            let events = vec![sample_event("e1", 1, 0, "a"), sample_event("e2", 1, 1, "b")];
            populate_log(&storage, events).await;

            // Create source from path directly, not from storage reference
            let source = AppendLogEventSource::from_path(scfg.data_path);
            let mut cursor = source.open_cursor_from_start().unwrap();
            let batch = cursor.next_batch(10).unwrap();
            assert_eq!(batch.len(), 2);
            assert_eq!(batch[0].event.event_id, "e1");
        });
    }

    // =========================================================================
    // E2.F1.T3: Schema-dedup-EOF parity validation across reader paths
    // =========================================================================

    /// Helper: run both paths and return (legacy_result, reader_result).
    async fn run_both_paths(
        storage: &AppendLogRecorderStorage,
        source: &AppendLogEventSource,
        base_config: IndexerConfig,
    ) -> (IndexerRunResult, IndexerRunResult) {
        // Legacy path
        let legacy_config = IndexerConfig {
            consumer_id: format!("{}-legacy", base_config.consumer_id),
            ..base_config.clone()
        };
        let writer1 = MockIndexWriter::new();
        let mut indexer1 = IncrementalIndexer::new(legacy_config, writer1);
        let r1 = indexer1.run(storage).await.unwrap();

        // Reader path
        let reader_config = IndexerConfig {
            consumer_id: format!("{}-reader", base_config.consumer_id),
            ..base_config
        };
        let writer2 = MockIndexWriter::new();
        let mut indexer2 = IncrementalIndexer::new(reader_config, writer2);
        let r2 = indexer2.run_with_reader(storage, source).await.unwrap();

        (r1, r2)
    }

    #[test]
    fn parity_single_event_indexing() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            populate_log(&storage, vec![sample_event("e1", 1, 0, "hello")]).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let icfg = test_indexer_config(dir.path());
            let (r1, r2) = run_both_paths(&storage, &source, icfg).await;

            assert_eq!(r1.events_read, r2.events_read);
            assert_eq!(r1.events_indexed, r2.events_indexed);
            assert_eq!(r1.events_skipped, r2.events_skipped);
            assert_eq!(r1.batches_committed, r2.batches_committed);
            assert_eq!(r1.caught_up, r2.caught_up);
            assert_eq!(r1.final_ordinal, r2.final_ordinal);
            assert_eq!(r1.events_read, 1);
        });
    }

    #[test]
    fn parity_batch_indexing_100_events() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events: Vec<_> = (0..100)
                .map(|i| sample_event(&format!("e{i}"), i % 5, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let icfg = test_indexer_config(dir.path());
            let (r1, r2) = run_both_paths(&storage, &source, icfg).await;

            assert_eq!(r1.events_read, 100);
            assert_eq!(r1.events_read, r2.events_read);
            assert_eq!(r1.events_indexed, r2.events_indexed);
            assert_eq!(r1.final_ordinal, r2.final_ordinal);
            assert_eq!(r1.caught_up, r2.caught_up);
            assert!(r1.caught_up);
        });
    }

    #[test]
    fn parity_eof_partial_batch_handling() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            // Write exactly 7 events with batch_size=3 -> 2 full + 1 partial
            let events: Vec<_> = (0..7)
                .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t-{i}")))
                .collect();
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let icfg = IndexerConfig {
                batch_size: 3,
                ..test_indexer_config(dir.path())
            };
            let (r1, r2) = run_both_paths(&storage, &source, icfg).await;

            assert_eq!(r1.events_read, 7);
            assert_eq!(r1.events_read, r2.events_read);
            assert_eq!(r1.events_indexed, r2.events_indexed);
            assert_eq!(r1.batches_committed, r2.batches_committed);
            // 3 batches: [3, 3, 1]
            assert_eq!(r1.batches_committed, 3);
            assert!(r1.caught_up);
            assert!(r2.caught_up);
        });
    }

    #[test]
    fn parity_schema_fields_match_exactly() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            // Create events with different pane IDs and sources
            let events = vec![
                sample_event("e1", 1, 0, "first output"),
                sample_event("e2", 2, 1, "second output"),
                sample_event("e3", 1, 2, "third output"),
            ];
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);

            // Legacy path: capture docs written
            let writer1 = MockIndexWriter::new();
            let icfg1 = IndexerConfig {
                consumer_id: "schema-legacy".to_string(),
                ..test_indexer_config(dir.path())
            };
            let mut indexer1 = IncrementalIndexer::new(icfg1, writer1);
            let _ = indexer1.run(&storage).await.unwrap();
            let w1 = indexer1.into_writer();
            let docs1 = w1.written_docs();

            // Reader path: capture docs written
            let writer2 = MockIndexWriter::new();
            let icfg2 = IndexerConfig {
                consumer_id: "schema-reader".to_string(),
                ..test_indexer_config(dir.path())
            };
            let mut indexer2 = IncrementalIndexer::new(icfg2, writer2);
            let _ = indexer2.run_with_reader(&storage, &source).await.unwrap();
            let w2 = indexer2.into_writer();
            let docs2 = w2.written_docs();

            assert_eq!(docs1.len(), docs2.len());
            for (d1, d2) in docs1.iter().zip(docs2.iter()) {
                assert_eq!(d1.event_id, d2.event_id);
                assert_eq!(d1.pane_id, d2.pane_id);
                assert_eq!(d1.sequence, d2.sequence);
                assert_eq!(d1.schema_version, d2.schema_version);
                assert_eq!(d1.text, d2.text);
            }
        });
    }

    #[test]
    fn parity_with_mixed_pane_ids() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events: Vec<_> = (0..20)
                .map(|i| {
                    sample_event(
                        &format!("e{i}"),
                        (i % 4) + 1,
                        i,
                        &format!("pane-{}-text-{i}", (i % 4) + 1),
                    )
                })
                .collect();
            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let icfg = test_indexer_config(dir.path());
            let (r1, r2) = run_both_paths(&storage, &source, icfg).await;

            assert_eq!(r1.events_read, 20);
            assert_eq!(r1.events_read, r2.events_read);
            assert_eq!(r1.events_indexed, r2.events_indexed);
            assert_eq!(r1.final_ordinal, r2.final_ordinal);
        });
    }

    #[test]
    fn parity_dedup_skips_identical_schema_mismatch() {
        run_async_test(async {
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            // Mix valid and invalid schema version events
            let mut events = vec![
                sample_event("e1", 1, 0, "valid"),
                sample_event("e2", 1, 1, "valid"),
            ];
            // Create event with wrong schema
            let mut bad_event = sample_event("e3", 1, 2, "bad-schema");
            bad_event.schema_version = "ft.recorder.v99".to_string();
            events.push(bad_event);
            events.push(sample_event("e4", 1, 3, "valid-again"));

            populate_log(&storage, events).await;

            let source = AppendLogEventSource::from_storage(&storage);
            let icfg = test_indexer_config(dir.path());
            let (r1, r2) = run_both_paths(&storage, &source, icfg).await;

            assert_eq!(r1.events_read, 4);
            assert_eq!(r1.events_indexed, 3); // e3 skipped
            assert_eq!(r1.events_skipped, 1);
            assert_eq!(r1.events_read, r2.events_read);
            assert_eq!(r1.events_indexed, r2.events_indexed);
            assert_eq!(r1.events_skipped, r2.events_skipped);
        });
    }

    // =========================================================================
    // Mock FrankenSqlite cursor for testing abstraction
    // =========================================================================

    #[test]
    fn mock_frankensqlite_cursor_produces_identical_results() {
        run_async_test(async {
            use crate::recorder_storage::{
                CursorRecord, EventCursorError, RecorderEventCursor, RecorderEventReader,
            };

            struct MockSqliteCursor {
                records: Vec<CursorRecord>,
                pos: usize,
            }

            impl RecorderEventCursor for MockSqliteCursor {
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
                        RecorderOffset {
                            segment_id: 0,
                            byte_offset: 999,
                            ordinal: self.records.len() as u64,
                        }
                    }
                }
            }

            struct MockSqliteReader {
                records: Vec<CursorRecord>,
            }

            impl RecorderEventReader for MockSqliteReader {
                fn open_cursor(
                    &self,
                    from: RecorderOffset,
                ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError>
                {
                    let start = from.ordinal as usize;
                    let remaining: Vec<_> = self
                        .records
                        .iter()
                        .filter(|r| r.offset.ordinal >= start as u64)
                        .cloned()
                        .collect();
                    Ok(Box::new(MockSqliteCursor {
                        records: remaining,
                        pos: 0,
                    }))
                }

                fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
                    Ok(RecorderOffset {
                        segment_id: 0,
                        byte_offset: 999,
                        ordinal: self.records.len() as u64,
                    })
                }
            }

            // Populate both real append-log and mock sqlite with same events
            let dir = tempdir().unwrap();
            let scfg = test_storage_config(dir.path());
            let storage = AppendLogRecorderStorage::open(scfg).unwrap();

            let events: Vec<_> = (0..10)
                .map(|i| sample_event(&format!("e{i}"), i % 3, i, &format!("text-{i}")))
                .collect();
            populate_log(&storage, events.clone()).await;

            // Build mock records matching what append-log reader would produce
            let mock_records: Vec<CursorRecord> = events
                .iter()
                .enumerate()
                .map(|(i, e)| CursorRecord {
                    event: e.clone(),
                    offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: i as u64 * 100, // fake offsets don't affect indexing
                        ordinal: i as u64,
                    },
                })
                .collect();

            // Run via real append-log source
            let al_source = AppendLogEventSource::from_storage(&storage);
            let icfg_al = IndexerConfig {
                consumer_id: "parity-al".to_string(),
                ..test_indexer_config(dir.path())
            };
            let writer_al = MockIndexWriter::new();
            let mut indexer_al = IncrementalIndexer::new(icfg_al, writer_al);
            let r_al = indexer_al
                .run_with_reader(&storage, &al_source)
                .await
                .unwrap();

            // Run via mock sqlite source
            let sqlite_source = MockSqliteReader {
                records: mock_records,
            };
            let icfg_sq = IndexerConfig {
                consumer_id: "parity-sqlite".to_string(),
                ..test_indexer_config(dir.path())
            };
            let writer_sq = MockIndexWriter::new();
            let mut indexer_sq = IncrementalIndexer::new(icfg_sq, writer_sq);
            let r_sq = indexer_sq
                .run_with_reader(&storage, &sqlite_source)
                .await
                .unwrap();

            // Both should produce identical results
            assert_eq!(r_al.events_read, r_sq.events_read);
            assert_eq!(r_al.events_indexed, r_sq.events_indexed);
            assert_eq!(r_al.events_skipped, r_sq.events_skipped);
            assert_eq!(r_al.caught_up, r_sq.caught_up);
        });
    }

    #[test]
    fn indexer_config_frankensqlite_descriptor() {
        let cfg = IndexerConfig {
            source: crate::recorder_storage::RecorderSourceDescriptor::FrankenSqlite {
                db_path: PathBuf::from("/data/recorder.db"),
            },
            ..IndexerConfig::default()
        };
        assert!(cfg.data_path().is_none());
        assert_eq!(
            cfg.source.backend_kind(),
            crate::recorder_storage::RecorderBackendKind::FrankenSqlite
        );
    }

    #[test]
    fn indexer_config_create_event_reader_append_log() {
        let dir = tempdir().unwrap();
        // Create the events.log file
        let scfg = test_storage_config(dir.path());
        let _storage = AppendLogRecorderStorage::open(scfg).unwrap();

        let cfg = test_indexer_config(dir.path());
        let reader = cfg.create_event_reader();
        assert!(reader.is_ok());
    }

    #[test]
    fn indexer_config_create_event_reader_frankensqlite_errors() {
        let cfg = IndexerConfig {
            source: crate::recorder_storage::RecorderSourceDescriptor::FrankenSqlite {
                db_path: PathBuf::from("/data/recorder.db"),
            },
            ..IndexerConfig::default()
        };
        let result = cfg.create_event_reader();
        assert!(result.is_err());
    }
}
