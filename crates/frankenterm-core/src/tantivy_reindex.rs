//! Deterministic reindex and backfill tooling for the Tantivy lexical index.
//!
//! Bead: wa-oegrb.4.4
//!
//! This module provides operators with reliable tools to rebuild, backfill,
//! and verify the lexical search index from the recorder source-of-truth log:
//!
//! - **Full reindex**: Wipe and rebuild the entire index from ordinal 0.
//! - **Range backfill**: Re-index a specific ordinal or time range (for
//!   schema evolution, corruption repair, or historical imports).
//! - **Integrity verification**: Compare index contents against the append
//!   log to detect missing, extra, or offset-mismatched documents.
//!
//! All operations use resumable checkpoints with a separate consumer ID
//! so they don't interfere with the live incremental indexer.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::recorder_storage::{
    CheckpointConsumerId, EventCursorError, RecorderCheckpoint, RecorderEventCursor,
    RecorderEventReader, RecorderOffset, RecorderSourceDescriptor, RecorderStorage,
};
use crate::recording::RECORDER_EVENT_SCHEMA_VERSION_V1;
use crate::tantivy_ingest::{
    AppendLogEventSource, IndexWriteError, IndexWriter, IndexerError, map_event_to_document,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default consumer ID prefix for reindex operations.
pub const REINDEX_CONSUMER_PREFIX: &str = "tantivy-reindex";

/// Default consumer ID prefix for backfill operations.
pub const BACKFILL_CONSUMER_PREFIX: &str = "tantivy-backfill";

/// Create a [`RecorderEventReader`] from a source descriptor.
fn create_event_reader(
    source: &RecorderSourceDescriptor,
) -> Result<Box<dyn RecorderEventReader>, IndexerError> {
    match source {
        RecorderSourceDescriptor::AppendLog { data_path } => {
            tracing::debug!(
                reindex_source = %source,
                "creating append-log event reader for reindex"
            );
            Ok(Box::new(AppendLogEventSource::from_path(
                data_path.clone(),
            )))
        }
        RecorderSourceDescriptor::FrankenSqlite { .. } => Err(IndexerError::Config(
            "frankensqlite event reader not yet implemented for reindex".to_string(),
        )),
    }
}

/// Convert [`EventCursorError`] to [`IndexerError`].
fn cursor_err(e: EventCursorError) -> IndexerError {
    IndexerError::LogRead(crate::tantivy_ingest::LogReadError::Io(
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
    ))
}

// ---------------------------------------------------------------------------
// Extended writer trait for reindex operations
// ---------------------------------------------------------------------------

/// Extended capabilities required for reindex (beyond base `IndexWriter`).
///
/// The `clear_all` method is needed for full rebuilds. Implementations should
/// delete every document and commit the deletion before returning.
pub trait ReindexableWriter: IndexWriter {
    /// Delete all documents from the index.
    ///
    /// Returns the number of documents deleted.
    fn clear_all(&mut self) -> Result<u64, IndexWriteError>;
}

/// Trait for looking up documents in the index (used by integrity checker).
pub trait IndexLookup: Send {
    /// Check whether a document with the given event_id exists.
    fn has_event_id(&self, event_id: &str) -> Result<bool, IndexWriteError>;

    /// Get the stored log_offset for a given event_id.
    fn get_log_offset(&self, event_id: &str) -> Result<Option<u64>, IndexWriteError>;

    /// Total number of indexed documents.
    fn count_total(&self) -> Result<u64, IndexWriteError>;
}

// ---------------------------------------------------------------------------
// Backfill range specification
// ---------------------------------------------------------------------------

/// Specifies which events to include in a backfill operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackfillRange {
    /// Events with ordinal in `[start, end]` (inclusive).
    OrdinalRange { start: u64, end: u64 },
    /// Events with `occurred_at_ms` in `[start_ms, end_ms]` (inclusive).
    TimeRange { start_ms: u64, end_ms: u64 },
    /// All events (equivalent to full reindex without clearing).
    All,
}

impl BackfillRange {
    /// Whether an event at the given ordinal and timestamp is within range.
    pub fn includes(&self, ordinal: u64, occurred_at_ms: u64) -> bool {
        match self {
            Self::OrdinalRange { start, end } => ordinal >= *start && ordinal <= *end,
            Self::TimeRange { start_ms, end_ms } => {
                occurred_at_ms >= *start_ms && occurred_at_ms <= *end_ms
            }
            Self::All => true,
        }
    }

    /// Whether we can stop scanning early (ordinal past end of range).
    pub fn past_end(&self, ordinal: u64) -> bool {
        match self {
            Self::OrdinalRange { end, .. } => ordinal > *end,
            // Time ranges can't short-circuit — events may not be time-ordered.
            Self::TimeRange { .. } => false,
            Self::All => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Reindex configuration
// ---------------------------------------------------------------------------

/// Configuration for a full reindex operation.
#[derive(Debug, Clone)]
pub struct ReindexConfig {
    /// Backend-neutral source descriptor.
    pub source: RecorderSourceDescriptor,
    /// Consumer ID for reindex checkpoint tracking.
    pub consumer_id: String,
    /// Maximum events per batch before committing.
    pub batch_size: usize,
    /// Whether to delete-before-add for idempotent replay.
    pub dedup_on_replay: bool,
    /// Expected recorder event schema version.
    pub expected_event_schema: String,
    /// Whether to clear the entire index before starting.
    pub clear_before_start: bool,
    /// Stop after this many batches (0 = unlimited).
    pub max_batches: usize,
}

impl Default for ReindexConfig {
    fn default() -> Self {
        Self {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: PathBuf::from(".ft/recorder-log/events.log"),
            },
            consumer_id: format!("{REINDEX_CONSUMER_PREFIX}-default"),
            batch_size: 1024,
            dedup_on_replay: true,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            clear_before_start: true,
            max_batches: 0,
        }
    }
}

/// Configuration for a partial backfill operation.
#[derive(Debug, Clone)]
pub struct BackfillConfig {
    /// Backend-neutral source descriptor.
    pub source: RecorderSourceDescriptor,
    /// Consumer ID for backfill checkpoint tracking.
    pub consumer_id: String,
    /// Maximum events per batch before committing.
    pub batch_size: usize,
    /// Which events to backfill.
    pub range: BackfillRange,
    /// Whether to delete-before-add for idempotent replay.
    pub dedup_on_replay: bool,
    /// Expected recorder event schema version.
    pub expected_event_schema: String,
    /// Stop after this many batches (0 = unlimited).
    pub max_batches: usize,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: PathBuf::from(".ft/recorder-log/events.log"),
            },
            consumer_id: format!("{BACKFILL_CONSUMER_PREFIX}-default"),
            batch_size: 1024,
            range: BackfillRange::All,
            dedup_on_replay: true,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Reindex progress / result
// ---------------------------------------------------------------------------

/// Progress snapshot during a reindex or backfill operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReindexProgress {
    /// Total events read from the log.
    pub events_read: u64,
    /// Events successfully indexed.
    pub events_indexed: u64,
    /// Events skipped (schema mismatch, writer rejection).
    pub events_skipped: u64,
    /// Events filtered out (outside backfill range).
    pub events_filtered: u64,
    /// Batches committed.
    pub batches_committed: u64,
    /// Current ordinal position.
    pub current_ordinal: Option<u64>,
    /// Whether the operation reached the end of the scannable range.
    pub caught_up: bool,
    /// Documents deleted during clear (full reindex only).
    pub docs_cleared: u64,
}

impl ReindexProgress {
    fn new() -> Self {
        Self {
            events_read: 0,
            events_indexed: 0,
            events_skipped: 0,
            events_filtered: 0,
            batches_committed: 0,
            current_ordinal: None,
            caught_up: false,
            docs_cleared: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// ReindexPipeline — orchestrates full reindex and backfill
// ---------------------------------------------------------------------------

/// Orchestrates full reindex and partial backfill operations.
///
/// Uses a separate checkpoint consumer ID so that operations don't conflict
/// with the live incremental indexer pipeline.
pub struct ReindexPipeline<W: IndexWriter> {
    writer: W,
}

impl<W: ReindexableWriter> ReindexPipeline<W> {
    /// Create a new pipeline with the given writer.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Perform a full reindex from ordinal 0.
    ///
    /// If `config.clear_before_start` is true, all existing documents are
    /// deleted before indexing begins. The operation is resumable: if
    /// interrupted, the next call with the same consumer ID continues from
    /// the last checkpoint.
    pub async fn full_reindex<S: RecorderStorage>(
        &mut self,
        storage: &S,
        config: &ReindexConfig,
    ) -> Result<ReindexProgress, IndexerError> {
        if config.batch_size == 0 {
            return Err(IndexerError::Config("batch_size must be >= 1".to_string()));
        }
        let event_reader = create_event_reader(&config.source)?;

        let mut progress = ReindexProgress::new();
        let consumer_id = CheckpointConsumerId(config.consumer_id.clone());

        // Optionally clear the index
        if config.clear_before_start {
            let existing_checkpoint = storage.read_checkpoint(&consumer_id).await?;
            // Only clear if we're starting fresh (no existing checkpoint)
            if existing_checkpoint.is_none() {
                let cleared = self.writer.clear_all().map_err(IndexerError::IndexWrite)?;
                progress.docs_cleared = cleared;
            }
        }

        // Read checkpoint for resume
        let checkpoint = storage.read_checkpoint(&consumer_id).await?;

        let mut cursor = match &checkpoint {
            Some(cp) => {
                let mut c = event_reader
                    .open_cursor(cp.upto_offset.clone())
                    .map_err(cursor_err)?;
                // Skip past the checkpointed record
                let _ = c.next_batch(1).map_err(cursor_err)?;
                progress.current_ordinal = Some(cp.upto_offset.ordinal);
                c
            }
            None => event_reader.open_cursor_from_start().map_err(cursor_err)?,
        };

        self.index_loop(
            storage,
            &mut *cursor,
            &consumer_id,
            &BackfillRange::All,
            config.batch_size,
            config.dedup_on_replay,
            &config.expected_event_schema,
            config.max_batches,
            &mut progress,
        )
        .await?;

        Ok(progress)
    }

    /// Access the underlying writer.
    pub fn writer(&self) -> &W {
        &self.writer
    }

    /// Consume the pipeline and return the writer.
    pub fn into_writer(self) -> W {
        self.writer
    }
}

impl<W: IndexWriter> ReindexPipeline<W> {
    /// Create a pipeline for backfill-only (no clear_all needed).
    pub fn new_for_backfill(writer: W) -> Self {
        Self { writer }
    }

    /// Backfill a specific range of events.
    ///
    /// Events outside the specified range are skipped. The operation uses
    /// its own checkpoint consumer ID for resumability.
    pub async fn backfill<S: RecorderStorage>(
        &mut self,
        storage: &S,
        config: &BackfillConfig,
    ) -> Result<ReindexProgress, IndexerError> {
        if config.batch_size == 0 {
            return Err(IndexerError::Config("batch_size must be >= 1".to_string()));
        }
        let event_reader = create_event_reader(&config.source)?;

        let mut progress = ReindexProgress::new();
        let consumer_id = CheckpointConsumerId(config.consumer_id.clone());

        let checkpoint = storage.read_checkpoint(&consumer_id).await?;

        let mut cursor = match &checkpoint {
            Some(cp) => {
                let mut c = event_reader
                    .open_cursor(cp.upto_offset.clone())
                    .map_err(cursor_err)?;
                let _ = c.next_batch(1).map_err(cursor_err)?;
                progress.current_ordinal = Some(cp.upto_offset.ordinal);
                c
            }
            None => {
                // For ordinal ranges, seek to the range start to avoid
                // wasting batches on events before the range.
                if let BackfillRange::OrdinalRange { start, .. } = &config.range {
                    if *start > 0 {
                        event_reader
                            .open_cursor_at_ordinal(*start)
                            .map_err(cursor_err)?
                    } else {
                        event_reader.open_cursor_from_start().map_err(cursor_err)?
                    }
                } else {
                    event_reader.open_cursor_from_start().map_err(cursor_err)?
                }
            }
        };

        self.index_loop(
            storage,
            &mut *cursor,
            &consumer_id,
            &config.range,
            config.batch_size,
            config.dedup_on_replay,
            &config.expected_event_schema,
            config.max_batches,
            &mut progress,
        )
        .await?;

        Ok(progress)
    }

    /// Access the writer (backfill variant).
    pub fn backfill_writer(&self) -> &W {
        &self.writer
    }

    /// Core indexing loop shared by reindex and backfill.
    #[allow(clippy::too_many_arguments)]
    async fn index_loop<S: RecorderStorage>(
        &mut self,
        storage: &S,
        cursor: &mut dyn RecorderEventCursor,
        consumer_id: &CheckpointConsumerId,
        range: &BackfillRange,
        batch_size: usize,
        dedup_on_replay: bool,
        expected_schema: &str,
        max_batches: usize,
        progress: &mut ReindexProgress,
    ) -> Result<(), IndexerError> {
        loop {
            if max_batches > 0 && progress.batches_committed >= max_batches as u64 {
                break;
            }

            let batch = cursor.next_batch(batch_size).map_err(cursor_err)?;
            if batch.is_empty() {
                progress.caught_up = true;
                break;
            }

            let is_final_batch = batch.len() < batch_size;
            let mut last_offset: Option<RecorderOffset> = None;
            let mut index_mutations_in_batch = 0u64;

            for record in &batch {
                progress.events_read += 1;
                let ordinal = record.offset.ordinal;

                // Short-circuit for ordinal ranges past the end
                if range.past_end(ordinal) {
                    progress.caught_up = true;
                    last_offset = Some(record.offset.clone());
                    // Commit what we have so far before stopping
                    break;
                }

                // Range filter
                if !range.includes(ordinal, record.event.occurred_at_ms) {
                    progress.events_filtered += 1;
                    last_offset = Some(record.offset.clone());
                    continue;
                }

                // Schema version check
                if record.event.schema_version != expected_schema {
                    progress.events_skipped += 1;
                    last_offset = Some(record.offset.clone());
                    continue;
                }

                let doc = map_event_to_document(&record.event, record.offset.ordinal);

                // Dedup
                if dedup_on_replay {
                    self.writer
                        .delete_by_event_id(&doc.event_id)
                        .map_err(IndexerError::IndexWrite)?;
                    index_mutations_in_batch += 1;
                }

                match self.writer.add_document(&doc) {
                    Ok(()) => {
                        progress.events_indexed += 1;
                        index_mutations_in_batch += 1;
                    }
                    Err(IndexWriteError::Rejected { .. }) => {
                        progress.events_skipped += 1;
                    }
                    Err(e) => return Err(e.into()),
                }

                last_offset = Some(record.offset.clone());
            }

            // Only commit if we indexed or filtered something
            if let Some(offset) = last_offset {
                // Checkpoint advancement must be coupled to committed index mutations.
                if index_mutations_in_batch > 0 {
                    self.writer.commit().map_err(IndexerError::IndexWrite)?;
                }

                progress.current_ordinal = Some(offset.ordinal);

                let cp = RecorderCheckpoint {
                    consumer: consumer_id.clone(),
                    upto_offset: offset,
                    schema_version: expected_schema.to_string(),
                    committed_at_ms: epoch_ms_now(),
                };
                storage.commit_checkpoint(cp).await?;
                progress.batches_committed += 1;
            }

            if is_final_batch || progress.caught_up {
                if !progress.caught_up {
                    progress.caught_up = true;
                }
                break;
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Integrity checker
// ---------------------------------------------------------------------------

/// Result of an integrity check comparing the append log to the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityReport {
    /// Events scanned from the append log.
    pub log_events_scanned: u64,
    /// Documents found in the index for scanned events.
    pub index_matches: u64,
    /// Event IDs present in the log but missing from the index.
    pub missing_from_index: Vec<String>,
    /// Events where the stored log_offset doesn't match the actual log position.
    pub offset_mismatches: Vec<OffsetMismatch>,
    /// Whether the index is consistent with the log for the checked range.
    pub is_consistent: bool,
    /// Total documents in the index (if available).
    pub total_index_docs: Option<u64>,
    /// Range that was checked.
    pub checked_range: CheckedRange,
}

/// A detected offset mismatch between log and index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetMismatch {
    pub event_id: String,
    pub expected_offset: u64,
    pub actual_offset: u64,
}

/// Summary of the range that was checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckedRange {
    pub start_ordinal: u64,
    pub end_ordinal: u64,
    pub events_checked: u64,
}

/// Configuration for an integrity check.
#[derive(Debug, Clone)]
pub struct IntegrityCheckConfig {
    /// Backend-neutral source descriptor.
    pub source: RecorderSourceDescriptor,
    /// Range of ordinals to check (None = all).
    pub ordinal_range: Option<(u64, u64)>,
    /// Maximum events to check (0 = unlimited).
    pub max_events: usize,
    /// Expected schema version (only check matching events).
    pub expected_event_schema: String,
}

impl Default for IntegrityCheckConfig {
    fn default() -> Self {
        Self {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: PathBuf::from(".ft/recorder-log/events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        }
    }
}

/// Verifies index integrity against the source-of-truth append log.
pub struct IntegrityChecker;

impl IntegrityChecker {
    /// Run an integrity check over the specified range.
    ///
    /// Scans the event source and for each event, checks that:
    /// 1. A document with the event's `event_id` exists in the index.
    /// 2. The stored `log_offset` matches the event's actual ordinal.
    pub fn check<L: IndexLookup>(
        lookup: &L,
        config: &IntegrityCheckConfig,
    ) -> Result<IntegrityReport, IndexerError> {
        let event_reader = create_event_reader(&config.source)?;

        let mut cursor = match config.ordinal_range {
            Some((start, _)) if start > 0 => event_reader
                .open_cursor_at_ordinal(start)
                .map_err(cursor_err)?,
            _ => event_reader.open_cursor_from_start().map_err(cursor_err)?,
        };

        let start_ordinal = config.ordinal_range.map(|(s, _)| s);
        let end_ordinal = config.ordinal_range.map(|(_, end)| end);

        let mut report = IntegrityReport {
            log_events_scanned: 0,
            index_matches: 0,
            missing_from_index: Vec::new(),
            offset_mismatches: Vec::new(),
            is_consistent: true,
            total_index_docs: None,
            checked_range: CheckedRange {
                start_ordinal: config.ordinal_range.map(|(s, _)| s).unwrap_or(0),
                end_ordinal: 0,
                events_checked: 0,
            },
        };

        // Get total index doc count if available
        report.total_index_docs = lookup.count_total().ok();

        let mut events_checked = 0u64;

        loop {
            if config.max_events > 0 && events_checked >= config.max_events as u64 {
                break;
            }

            let batch = cursor.next_batch(1).map_err(cursor_err)?;
            let record = match batch.into_iter().next() {
                Some(r) => r,
                None => break,
            };

            let ordinal = record.offset.ordinal;

            // Before start of range — skip
            if let Some(start) = start_ordinal {
                if ordinal < start {
                    continue;
                }
            }

            // Past end of range
            if let Some(end) = end_ordinal {
                if ordinal > end {
                    break;
                }
            }

            report.log_events_scanned += 1;

            // Skip events with wrong schema version
            if record.event.schema_version != config.expected_event_schema {
                continue;
            }

            events_checked += 1;
            let event_id = &record.event.event_id;

            // Check existence
            match lookup.has_event_id(event_id) {
                Ok(true) => {
                    report.index_matches += 1;

                    // Check offset consistency
                    if let Ok(Some(stored_offset)) = lookup.get_log_offset(event_id) {
                        if stored_offset != ordinal {
                            report.offset_mismatches.push(OffsetMismatch {
                                event_id: event_id.clone(),
                                expected_offset: ordinal,
                                actual_offset: stored_offset,
                            });
                            report.is_consistent = false;
                        }
                    }
                }
                Ok(false) => {
                    report.missing_from_index.push(event_id.clone());
                    report.is_consistent = false;
                }
                Err(_) => {
                    // Treat lookup errors as missing
                    report.missing_from_index.push(event_id.clone());
                    report.is_consistent = false;
                }
            }

            report.checked_range.end_ordinal = ordinal;
        }

        report.checked_range.events_checked = events_checked;

        Ok(report)
    }
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
        RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
        RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
    };
    use crate::tantivy_ingest::{IndexCommitStats, IndexDocumentFields};
    use std::collections::HashMap;
    use tempfile::tempdir;

    // -- test helpers --

    fn sample_event(event_id: &str, pane_id: u64, sequence: u64, text: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
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

    fn timed_event(
        event_id: &str,
        pane_id: u64,
        sequence: u64,
        occurred_at_ms: u64,
        text: &str,
    ) -> RecorderEvent {
        let mut e = sample_event(event_id, pane_id, sequence, text);
        e.occurred_at_ms = occurred_at_ms;
        e
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

    // -- Mock ReindexableWriter --

    struct MockReindexWriter {
        docs: Vec<IndexDocumentFields>,
        deleted_ids: Vec<String>,
        commits: u64,
        cleared: bool,
        clear_count: u64,
        reject_ids: Vec<String>,
        fail_delete_ids: Vec<String>,
        fail_commit: bool,
    }

    impl MockReindexWriter {
        fn new() -> Self {
            Self {
                docs: Vec::new(),
                deleted_ids: Vec::new(),
                commits: 0,
                cleared: false,
                clear_count: 0,
                reject_ids: Vec::new(),
                fail_delete_ids: Vec::new(),
                fail_commit: false,
            }
        }
    }

    impl IndexWriter for MockReindexWriter {
        fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
            if self.reject_ids.contains(&doc.event_id) {
                return Err(IndexWriteError::Rejected {
                    reason: "test".to_string(),
                });
            }
            self.docs.push(doc.clone());
            Ok(())
        }

        fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
            if self.fail_commit {
                return Err(IndexWriteError::CommitFailed {
                    reason: "test".to_string(),
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
            if self.fail_delete_ids.iter().any(|id| id == event_id) {
                return Err(IndexWriteError::Rejected {
                    reason: "delete-fail".to_string(),
                });
            }
            self.deleted_ids.push(event_id.to_string());
            Ok(())
        }
    }

    impl ReindexableWriter for MockReindexWriter {
        fn clear_all(&mut self) -> Result<u64, IndexWriteError> {
            let count = self.docs.len() as u64 + self.clear_count;
            self.docs.clear();
            self.deleted_ids.clear();
            self.cleared = true;
            self.clear_count = count;
            Ok(count)
        }
    }

    // -- Mock IndexLookup --

    struct MockIndexLookup {
        docs: HashMap<String, u64>, // event_id -> log_offset
        total: u64,
    }

    impl MockIndexLookup {
        fn new() -> Self {
            Self {
                docs: HashMap::new(),
                total: 0,
            }
        }

        fn from_docs(docs: &[IndexDocumentFields]) -> Self {
            let mut lookup = Self::new();
            for doc in docs {
                lookup.docs.insert(doc.event_id.clone(), doc.log_offset);
            }
            lookup.total = docs.len() as u64;
            lookup
        }
    }

    impl IndexLookup for MockIndexLookup {
        fn has_event_id(&self, event_id: &str) -> Result<bool, IndexWriteError> {
            Ok(self.docs.contains_key(event_id))
        }

        fn get_log_offset(&self, event_id: &str) -> Result<Option<u64>, IndexWriteError> {
            Ok(self.docs.get(event_id).copied())
        }

        fn count_total(&self) -> Result<u64, IndexWriteError> {
            Ok(self.total)
        }
    }

    // =========================================================================
    // BackfillRange tests
    // =========================================================================

    #[test]
    fn range_ordinal_includes() {
        let r = BackfillRange::OrdinalRange { start: 5, end: 10 };
        assert!(!r.includes(4, 0));
        assert!(r.includes(5, 0));
        assert!(r.includes(7, 0));
        assert!(r.includes(10, 0));
        assert!(!r.includes(11, 0));
    }

    #[test]
    fn range_ordinal_past_end() {
        let r = BackfillRange::OrdinalRange { start: 5, end: 10 };
        assert!(!r.past_end(10));
        assert!(r.past_end(11));
    }

    #[test]
    fn range_time_includes() {
        let r = BackfillRange::TimeRange {
            start_ms: 1000,
            end_ms: 2000,
        };
        assert!(!r.includes(0, 999));
        assert!(r.includes(0, 1000));
        assert!(r.includes(0, 1500));
        assert!(r.includes(0, 2000));
        assert!(!r.includes(0, 2001));
    }

    #[test]
    fn range_time_never_past_end() {
        let r = BackfillRange::TimeRange {
            start_ms: 1000,
            end_ms: 2000,
        };
        assert!(!r.past_end(999999));
    }

    #[test]
    fn range_all_includes_everything() {
        let r = BackfillRange::All;
        assert!(r.includes(0, 0));
        assert!(r.includes(u64::MAX, u64::MAX));
        assert!(!r.past_end(u64::MAX));
    }

    #[test]
    fn range_serialization_roundtrip() {
        let ranges = vec![
            BackfillRange::OrdinalRange { start: 0, end: 100 },
            BackfillRange::TimeRange {
                start_ms: 1000,
                end_ms: 2000,
            },
            BackfillRange::All,
        ];
        for r in ranges {
            let json = serde_json::to_string(&r).unwrap();
            let deser: BackfillRange = serde_json::from_str(&json).unwrap();
            assert_eq!(r, deser);
        }
    }

    // =========================================================================
    // Full reindex tests
    // =========================================================================

    #[tokio::test]
    async fn full_reindex_cold_start() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "reindex-test".to_string(),
            batch_size: 10,
            dedup_on_replay: true,
            clear_before_start: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();

        assert_eq!(progress.events_read, 5);
        assert_eq!(progress.events_indexed, 5);
        assert_eq!(progress.events_skipped, 0);
        assert_eq!(progress.events_filtered, 0);
        assert!(progress.caught_up);
        assert_eq!(progress.current_ordinal, Some(4));
        assert!(pipeline.writer().cleared);
    }

    #[tokio::test]
    async fn full_reindex_resumes_from_checkpoint() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "resume-reindex".to_string(),
            batch_size: 3,
            dedup_on_replay: true,
            clear_before_start: true,
            max_batches: 1,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        // First run: index 3 events
        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let p1 = pipeline.full_reindex(&storage, &config).await.unwrap();
        assert_eq!(p1.events_indexed, 3);
        assert!(!p1.caught_up);

        // Second run: should NOT clear (checkpoint exists), indexes next 3
        let mut pipeline2 = ReindexPipeline::new(MockReindexWriter::new());
        let p2 = pipeline2.full_reindex(&storage, &config).await.unwrap();
        assert_eq!(p2.events_indexed, 3);
        assert_eq!(p2.docs_cleared, 0); // no clear because checkpoint exists
        assert!(!pipeline2.writer().cleared);

        // Verify docs start from ordinal 3
        assert_eq!(pipeline2.writer().docs[0].event_id, "e3");
    }

    #[tokio::test]
    async fn full_reindex_empty_log() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "empty-reindex".to_string(),
            batch_size: 10,
            clear_before_start: true,
            ..ReindexConfig::default()
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();

        assert_eq!(progress.events_read, 0);
        assert_eq!(progress.events_indexed, 0);
        assert!(progress.caught_up);
    }

    #[tokio::test]
    async fn full_reindex_no_clear() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e1", 1, 0, "text")]).await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "no-clear".to_string(),
            batch_size: 10,
            clear_before_start: false,
            ..ReindexConfig::default()
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 1);
        assert_eq!(progress.docs_cleared, 0);
        assert!(!pipeline.writer().cleared);
    }

    #[tokio::test]
    async fn full_reindex_batch_size_zero_errors() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg).unwrap();

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            batch_size: 0,
            ..ReindexConfig::default()
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let err = pipeline.full_reindex(&storage, &config).await.unwrap_err();
        assert!(matches!(err, IndexerError::Config(_)));
    }

    // =========================================================================
    // Backfill tests — ordinal range
    // =========================================================================

    #[tokio::test]
    async fn backfill_ordinal_range() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-ord".to_string(),
            batch_size: 20,
            range: BackfillRange::OrdinalRange { start: 3, end: 6 },
            dedup_on_replay: true,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline.backfill(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 4); // ordinals 3, 4, 5, 6
        assert!(progress.caught_up);

        let event_ids: Vec<&str> = pipeline
            .backfill_writer()
            .docs
            .iter()
            .map(|d| d.event_id.as_str())
            .collect();
        assert_eq!(event_ids, vec!["e3", "e4", "e5", "e6"]);
    }

    #[tokio::test]
    async fn backfill_ordinal_range_resumes() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-resume".to_string(),
            batch_size: 2,
            range: BackfillRange::OrdinalRange { start: 2, end: 7 },
            dedup_on_replay: true,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 1,
        };

        // First run
        let mut p1 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r1 = p1.backfill(&storage, &config).await.unwrap();
        assert_eq!(r1.events_indexed, 2);
        assert!(!r1.caught_up);

        // Second run resumes
        let mut p2 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r2 = p2.backfill(&storage, &config).await.unwrap();
        assert_eq!(r2.events_indexed, 2);

        // Third run
        let mut p3 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r3 = p3.backfill(&storage, &config).await.unwrap();
        assert_eq!(r3.events_indexed, 2);

        // Fourth run — past end of range
        let config_unlimited = BackfillConfig {
            max_batches: 0,
            ..config.clone()
        };
        let mut p4 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r4 = p4.backfill(&storage, &config_unlimited).await.unwrap();
        assert!(r4.caught_up);
    }

    // =========================================================================
    // Backfill tests — time range
    // =========================================================================

    #[tokio::test]
    async fn backfill_time_range() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        // Events with specific timestamps
        let events = vec![
            timed_event("e0", 1, 0, 1000, "early"),
            timed_event("e1", 1, 1, 2000, "in-range-1"),
            timed_event("e2", 1, 2, 2500, "in-range-2"),
            timed_event("e3", 1, 3, 3000, "in-range-3"),
            timed_event("e4", 1, 4, 4000, "late"),
        ];
        populate_log(&storage, events).await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-time".to_string(),
            batch_size: 20,
            range: BackfillRange::TimeRange {
                start_ms: 2000,
                end_ms: 3000,
            },
            dedup_on_replay: false,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline.backfill(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 3);
        assert_eq!(progress.events_filtered, 2); // e0 and e4

        let ids: Vec<&str> = pipeline
            .backfill_writer()
            .docs
            .iter()
            .map(|d| d.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2", "e3"]);
    }

    #[tokio::test]
    async fn backfill_all_range() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..3)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-all".to_string(),
            batch_size: 20,
            range: BackfillRange::All,
            dedup_on_replay: false,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline.backfill(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 3);
        assert_eq!(progress.events_filtered, 0);
    }

    #[tokio::test]
    async fn backfill_schema_mismatch_skipped() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let mut bad = sample_event("bad", 1, 0, "bad");
        bad.schema_version = "ft.recorder.event.v99".to_string();
        let good = sample_event("good", 1, 1, "good");

        populate_log(&storage, vec![bad, good]).await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-schema".to_string(),
            batch_size: 20,
            range: BackfillRange::All,
            dedup_on_replay: false,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline.backfill(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 1);
        assert_eq!(progress.events_skipped, 1);
    }

    #[tokio::test]
    async fn backfill_rejected_docs_skipped() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("e1", 1, 0, "ok"),
                sample_event("e2", 1, 1, "reject"),
                sample_event("e3", 1, 2, "ok"),
            ],
        )
        .await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-reject".to_string(),
            batch_size: 20,
            range: BackfillRange::All,
            dedup_on_replay: false,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut writer = MockReindexWriter::new();
        writer.reject_ids = vec!["e2".to_string()];
        let mut pipeline = ReindexPipeline::new_for_backfill(writer);
        let progress = pipeline.backfill(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 2);
        assert_eq!(progress.events_skipped, 1);
    }

    #[tokio::test]
    async fn backfill_dedup_rejected_docs_commit_delete_mutations() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("e1", 1, 0, "reject-a"),
                sample_event("e2", 1, 1, "reject-b"),
            ],
        )
        .await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "backfill-dedup-reject".to_string(),
            batch_size: 20,
            range: BackfillRange::All,
            dedup_on_replay: true,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut writer = MockReindexWriter::new();
        writer.reject_ids = vec!["e1".to_string(), "e2".to_string()];

        let mut pipeline = ReindexPipeline::new_for_backfill(writer);
        let progress = pipeline.backfill(&storage, &config).await.unwrap();

        assert_eq!(progress.events_indexed, 0);
        assert_eq!(progress.events_skipped, 2);
        assert_eq!(pipeline.backfill_writer().deleted_ids.len(), 2);
        // Dedup deletes are index mutations and must be committed before checkpoint advance.
        assert_eq!(pipeline.backfill_writer().commits, 1);
    }

    #[tokio::test]
    async fn backfill_batch_size_zero_errors() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg).unwrap();

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            batch_size: 0,
            ..BackfillConfig::default()
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let err = pipeline.backfill(&storage, &config).await.unwrap_err();
        assert!(matches!(err, IndexerError::Config(_)));
    }

    // =========================================================================
    // Integrity checker tests
    // =========================================================================

    #[tokio::test]
    async fn integrity_check_consistent() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events.clone()).await;

        // Build a consistent index lookup
        let docs: Vec<_> = events
            .iter()
            .enumerate()
            .map(|(i, e)| map_event_to_document(e, i as u64))
            .collect();
        let lookup = MockIndexLookup::from_docs(&docs);

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.log_events_scanned, 5);
        assert_eq!(report.index_matches, 5);
        assert!(report.missing_from_index.is_empty());
        assert!(report.offset_mismatches.is_empty());
        assert_eq!(report.total_index_docs, Some(5));
    }

    #[tokio::test]
    async fn integrity_check_missing_docs() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events.clone()).await;

        // Only index 3 of 5 events
        let docs: Vec<_> = events
            .iter()
            .take(3)
            .enumerate()
            .map(|(i, e)| map_event_to_document(e, i as u64))
            .collect();
        let lookup = MockIndexLookup::from_docs(&docs);

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(!report.is_consistent);
        assert_eq!(report.missing_from_index.len(), 2);
        assert!(report.missing_from_index.contains(&"e3".to_string()));
        assert!(report.missing_from_index.contains(&"e4".to_string()));
    }

    #[tokio::test]
    async fn integrity_check_offset_mismatch() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..3)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        // Index with wrong offsets for e1
        let mut lookup = MockIndexLookup::new();
        lookup.docs.insert("e0".to_string(), 0);
        lookup.docs.insert("e1".to_string(), 999); // wrong!
        lookup.docs.insert("e2".to_string(), 2);
        lookup.total = 3;

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(!report.is_consistent);
        assert_eq!(report.offset_mismatches.len(), 1);
        assert_eq!(report.offset_mismatches[0].event_id, "e1");
        assert_eq!(report.offset_mismatches[0].expected_offset, 1);
        assert_eq!(report.offset_mismatches[0].actual_offset, 999);
    }

    #[tokio::test]
    async fn integrity_check_with_ordinal_range() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events.clone()).await;

        // Only index ordinals 3-6
        let mut lookup = MockIndexLookup::new();
        for i in 3..=6 {
            lookup.docs.insert(format!("e{i}"), i);
        }
        lookup.total = 4;

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: Some((3, 6)),
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.checked_range.events_checked, 4);
        assert_eq!(report.index_matches, 4);
    }

    #[tokio::test]
    async fn integrity_check_max_events() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events.clone()).await;

        let docs: Vec<_> = events
            .iter()
            .enumerate()
            .map(|(i, e)| map_event_to_document(e, i as u64))
            .collect();
        let lookup = MockIndexLookup::from_docs(&docs);

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 3,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert_eq!(report.checked_range.events_checked, 3);
        assert!(report.is_consistent);
    }

    #[tokio::test]
    async fn integrity_check_empty_log() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let _storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let lookup = MockIndexLookup::new();

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.log_events_scanned, 0);
    }

    #[tokio::test]
    async fn integrity_check_skips_wrong_schema() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let mut bad = sample_event("bad", 1, 0, "bad");
        bad.schema_version = "ft.recorder.event.v99".to_string();
        let good = sample_event("good", 1, 1, "good");

        populate_log(&storage, vec![bad, good]).await;

        let mut lookup = MockIndexLookup::new();
        lookup.docs.insert("good".to_string(), 1);
        lookup.total = 1;

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.log_events_scanned, 2);
        assert_eq!(report.checked_range.events_checked, 1);
        assert_eq!(report.index_matches, 1);
    }

    // =========================================================================
    // Default config tests
    // =========================================================================

    #[test]
    fn default_reindex_config() {
        let cfg = ReindexConfig::default();
        assert!(cfg.consumer_id.starts_with(REINDEX_CONSUMER_PREFIX));
        assert_eq!(cfg.batch_size, 1024);
        assert!(cfg.dedup_on_replay);
        assert!(cfg.clear_before_start);
        assert_eq!(cfg.max_batches, 0);
    }

    #[test]
    fn default_backfill_config() {
        let cfg = BackfillConfig::default();
        assert!(cfg.consumer_id.starts_with(BACKFILL_CONSUMER_PREFIX));
        assert_eq!(cfg.range, BackfillRange::All);
        assert_eq!(cfg.batch_size, 1024);
    }

    #[test]
    fn default_integrity_config() {
        let cfg = IntegrityCheckConfig::default();
        assert!(cfg.ordinal_range.is_none());
        assert_eq!(cfg.max_events, 0);
    }

    // =========================================================================
    // Progress serialization
    // =========================================================================

    #[test]
    fn progress_serialization_roundtrip() {
        let progress = ReindexProgress {
            events_read: 100,
            events_indexed: 95,
            events_skipped: 3,
            events_filtered: 2,
            batches_committed: 10,
            current_ordinal: Some(99),
            caught_up: true,
            docs_cleared: 50,
        };

        let json = serde_json::to_string(&progress).unwrap();
        let deser: ReindexProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(progress, deser);
    }

    // -----------------------------------------------------------------------
    // Batch 11 — TopazBay wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- BackfillRange edge cases ----

    #[test]
    fn range_ordinal_start_equals_end() {
        let r = BackfillRange::OrdinalRange { start: 5, end: 5 };
        assert!(!r.includes(4, 0));
        assert!(r.includes(5, 0));
        assert!(!r.includes(6, 0));
        assert!(!r.past_end(5));
        assert!(r.past_end(6));
    }

    #[test]
    fn range_time_start_equals_end() {
        let r = BackfillRange::TimeRange {
            start_ms: 1000,
            end_ms: 1000,
        };
        assert!(!r.includes(0, 999));
        assert!(r.includes(0, 1000));
        assert!(!r.includes(0, 1001));
    }

    #[test]
    fn range_ordinal_zero_to_zero() {
        let r = BackfillRange::OrdinalRange { start: 0, end: 0 };
        assert!(r.includes(0, 0));
        assert!(!r.includes(1, 0));
        assert!(r.past_end(1));
    }

    #[test]
    fn range_ordinal_max_values() {
        let r = BackfillRange::OrdinalRange {
            start: u64::MAX - 1,
            end: u64::MAX,
        };
        assert!(r.includes(u64::MAX, 0));
        assert!(!r.includes(u64::MAX - 2, 0));
    }

    #[test]
    fn range_time_zero() {
        let r = BackfillRange::TimeRange {
            start_ms: 0,
            end_ms: 0,
        };
        assert!(r.includes(0, 0));
        assert!(!r.includes(0, 1));
    }

    // ---- BackfillRange serde individual variants ----

    #[test]
    fn range_ordinal_serde() {
        let r = BackfillRange::OrdinalRange { start: 10, end: 20 };
        let json = serde_json::to_string(&r).unwrap();
        let back: BackfillRange = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(json.contains("OrdinalRange"));
    }

    #[test]
    fn range_time_serde() {
        let r = BackfillRange::TimeRange {
            start_ms: 1000,
            end_ms: 2000,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: BackfillRange = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(json.contains("TimeRange"));
    }

    #[test]
    fn range_all_serde() {
        let r = BackfillRange::All;
        let json = serde_json::to_string(&r).unwrap();
        let back: BackfillRange = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(json.contains("All"));
    }

    // ---- BackfillRange Debug/Clone ----

    #[test]
    fn range_debug() {
        let r = BackfillRange::OrdinalRange { start: 1, end: 10 };
        let debug = format!("{r:?}");
        assert!(debug.contains("OrdinalRange"));
        assert!(debug.contains("10"));
    }

    #[test]
    fn range_clone() {
        let r = BackfillRange::TimeRange {
            start_ms: 100,
            end_ms: 200,
        };
        let cloned = r.clone();
        assert_eq!(r, cloned);
    }

    // ---- ReindexProgress ----

    #[test]
    fn reindex_progress_new_all_zeros() {
        let p = ReindexProgress::new();
        assert_eq!(p.events_read, 0);
        assert_eq!(p.events_indexed, 0);
        assert_eq!(p.events_skipped, 0);
        assert_eq!(p.events_filtered, 0);
        assert_eq!(p.batches_committed, 0);
        assert_eq!(p.current_ordinal, None);
        assert!(!p.caught_up);
        assert_eq!(p.docs_cleared, 0);
    }

    #[test]
    fn reindex_progress_debug() {
        let p = ReindexProgress::new();
        let debug = format!("{p:?}");
        assert!(debug.contains("ReindexProgress"));
    }

    #[test]
    fn reindex_progress_clone() {
        let p = ReindexProgress {
            events_read: 50,
            events_indexed: 45,
            events_skipped: 3,
            events_filtered: 2,
            batches_committed: 5,
            current_ordinal: Some(49),
            caught_up: true,
            docs_cleared: 10,
        };
        let cloned = p.clone();
        assert_eq!(p, cloned);
    }

    // ---- OffsetMismatch ----

    #[test]
    fn offset_mismatch_serde_roundtrip() {
        let m = OffsetMismatch {
            event_id: "evt-42".to_string(),
            expected_offset: 42,
            actual_offset: 99,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: OffsetMismatch = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn offset_mismatch_debug_clone() {
        let m = OffsetMismatch {
            event_id: "e1".to_string(),
            expected_offset: 1,
            actual_offset: 100,
        };
        let debug = format!("{m:?}");
        assert!(debug.contains("OffsetMismatch"));
        assert!(debug.contains("e1"));
        let cloned = m.clone();
        assert_eq!(m, cloned);
    }

    // ---- CheckedRange ----

    #[test]
    fn checked_range_serde_roundtrip() {
        let cr = CheckedRange {
            start_ordinal: 0,
            end_ordinal: 999,
            events_checked: 500,
        };
        let json = serde_json::to_string(&cr).unwrap();
        let back: CheckedRange = serde_json::from_str(&json).unwrap();
        assert_eq!(cr, back);
    }

    #[test]
    fn checked_range_debug_clone() {
        let cr = CheckedRange {
            start_ordinal: 10,
            end_ordinal: 20,
            events_checked: 11,
        };
        let debug = format!("{cr:?}");
        assert!(debug.contains("CheckedRange"));
        let cloned = cr.clone();
        assert_eq!(cr, cloned);
    }

    // ---- IntegrityReport ----

    #[test]
    fn integrity_report_debug() {
        let report = IntegrityReport {
            log_events_scanned: 10,
            index_matches: 10,
            missing_from_index: vec![],
            offset_mismatches: vec![],
            is_consistent: true,
            total_index_docs: Some(10),
            checked_range: CheckedRange {
                start_ordinal: 0,
                end_ordinal: 9,
                events_checked: 10,
            },
        };
        let debug = format!("{report:?}");
        assert!(debug.contains("IntegrityReport"));
        assert!(debug.contains("is_consistent"));
    }

    #[test]
    fn integrity_report_clone() {
        let report = IntegrityReport {
            log_events_scanned: 5,
            index_matches: 3,
            missing_from_index: vec!["e1".to_string()],
            offset_mismatches: vec![],
            is_consistent: false,
            total_index_docs: None,
            checked_range: CheckedRange {
                start_ordinal: 0,
                end_ordinal: 4,
                events_checked: 5,
            },
        };
        let cloned = report.clone();
        assert_eq!(report, cloned);
    }

    // ---- Config Debug/Clone ----

    #[test]
    fn reindex_config_debug() {
        let cfg = ReindexConfig::default();
        let debug = format!("{cfg:?}");
        assert!(debug.contains("ReindexConfig"));
        assert!(debug.contains("batch_size"));
    }

    #[test]
    fn reindex_config_clone() {
        let cfg = ReindexConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cloned.batch_size, cfg.batch_size);
        assert_eq!(cloned.dedup_on_replay, cfg.dedup_on_replay);
    }

    #[test]
    fn backfill_config_debug() {
        let cfg = BackfillConfig::default();
        let debug = format!("{cfg:?}");
        assert!(debug.contains("BackfillConfig"));
    }

    #[test]
    fn backfill_config_clone() {
        let cfg = BackfillConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cloned.batch_size, cfg.batch_size);
        assert_eq!(cloned.range, cfg.range);
    }

    #[test]
    fn integrity_check_config_debug() {
        let cfg = IntegrityCheckConfig::default();
        let debug = format!("{cfg:?}");
        assert!(debug.contains("IntegrityCheckConfig"));
    }

    #[test]
    fn integrity_check_config_clone() {
        let cfg = IntegrityCheckConfig::default();
        let cloned = cfg.clone();
        assert_eq!(cloned.max_events, cfg.max_events);
        assert_eq!(cloned.ordinal_range, cfg.ordinal_range);
    }

    // ---- Constants ----

    #[test]
    fn consumer_prefixes() {
        assert_eq!(REINDEX_CONSUMER_PREFIX, "tantivy-reindex");
        assert_eq!(BACKFILL_CONSUMER_PREFIX, "tantivy-backfill");
    }

    #[test]
    fn integrity_report_serialization_roundtrip() {
        let report = IntegrityReport {
            log_events_scanned: 100,
            index_matches: 98,
            missing_from_index: vec!["e50".to_string(), "e75".to_string()],
            offset_mismatches: vec![OffsetMismatch {
                event_id: "e10".to_string(),
                expected_offset: 10,
                actual_offset: 999,
            }],
            is_consistent: false,
            total_index_docs: Some(98),
            checked_range: CheckedRange {
                start_ordinal: 0,
                end_ordinal: 99,
                events_checked: 100,
            },
        };

        let json = serde_json::to_string(&report).unwrap();
        let deser: IntegrityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, deser);
    }

    // =========================================================================
    // Integration: reindex then integrity check
    // =========================================================================

    #[tokio::test]
    async fn reindex_then_integrity_check_consistent() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..8)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        // Reindex all
        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "verify-test".to_string(),
            batch_size: 20,
            dedup_on_replay: false,
            clear_before_start: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();
        assert_eq!(progress.events_indexed, 8);

        // Build lookup from indexed docs
        let lookup = MockIndexLookup::from_docs(&pipeline.writer().docs);

        // Verify consistency
        let check_config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &check_config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.index_matches, 8);
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ──────────────────────────────────

    #[tokio::test]
    async fn reindex_dedup_calls_delete_before_add() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("e0", 1, 0, "hello"),
                sample_event("e1", 1, 1, "world"),
            ],
        )
        .await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "dedup-test".to_string(),
            batch_size: 10,
            dedup_on_replay: true,
            clear_before_start: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();
        assert_eq!(progress.events_indexed, 2);

        // With dedup_on_replay=true, delete_by_event_id should be called for each doc
        let deleted = &pipeline.writer().deleted_ids;
        assert_eq!(deleted.len(), 2);
        assert!(deleted.contains(&"e0".to_string()));
        assert!(deleted.contains(&"e1".to_string()));
    }

    #[tokio::test]
    async fn reindex_no_dedup_skips_delete() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![
                sample_event("e0", 1, 0, "hello"),
                sample_event("e1", 1, 1, "world"),
            ],
        )
        .await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "nodedup-test".to_string(),
            batch_size: 10,
            dedup_on_replay: false,
            clear_before_start: false,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();
        assert_eq!(progress.events_indexed, 2);

        // With dedup_on_replay=false, no deletes should be issued
        assert!(pipeline.writer().deleted_ids.is_empty());
    }

    #[tokio::test]
    async fn reindex_dedup_delete_failure_propagates() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e0", 1, 0, "text")]).await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "fail-delete".to_string(),
            batch_size: 10,
            dedup_on_replay: true,
            clear_before_start: false,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut writer = MockReindexWriter::new();
        writer.fail_delete_ids = vec!["e0".to_string()];
        let mut pipeline = ReindexPipeline::new(writer);

        let err = pipeline.full_reindex(&storage, &config).await.unwrap_err();
        assert!(matches!(err, IndexerError::IndexWrite(_)));
    }

    #[tokio::test]
    async fn reindex_commit_failure_propagates() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e0", 1, 0, "text")]).await;

        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "fail-commit".to_string(),
            batch_size: 10,
            dedup_on_replay: false,
            clear_before_start: false,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut writer = MockReindexWriter::new();
        writer.fail_commit = true;
        let mut pipeline = ReindexPipeline::new(writer);
        let err = pipeline.full_reindex(&storage, &config).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn backfill_commit_failure_propagates() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e0", 1, 0, "text")]).await;

        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "fail-commit-bf".to_string(),
            batch_size: 10,
            range: BackfillRange::All,
            dedup_on_replay: false,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut writer = MockReindexWriter::new();
        writer.fail_commit = true;
        let mut pipeline = ReindexPipeline::new_for_backfill(writer);
        let err = pipeline.backfill(&storage, &config).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn pipeline_writer_accessor() {
        let writer = MockReindexWriter::new();
        let pipeline = ReindexPipeline::new(writer);
        // writer() returns a reference to the inner writer
        assert!(!pipeline.writer().cleared);
        assert_eq!(pipeline.writer().docs.len(), 0);
    }

    #[tokio::test]
    async fn pipeline_into_writer_consumes() {
        let writer = MockReindexWriter::new();
        let pipeline = ReindexPipeline::new(writer);
        let recovered = pipeline.into_writer();
        assert_eq!(recovered.commits, 0);
        assert!(!recovered.cleared);
    }

    #[tokio::test]
    async fn pipeline_backfill_writer_accessor() {
        let writer = MockReindexWriter::new();
        let pipeline = ReindexPipeline::new_for_backfill(writer);
        assert_eq!(pipeline.backfill_writer().docs.len(), 0);
    }

    #[tokio::test]
    async fn reindex_multi_batch_progress_accumulates() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        // batch_size=3 means 4 batches (3+3+3+1)
        let config = ReindexConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "multi-batch".to_string(),
            batch_size: 3,
            dedup_on_replay: false,
            clear_before_start: true,
            max_batches: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let mut pipeline = ReindexPipeline::new(MockReindexWriter::new());
        let progress = pipeline.full_reindex(&storage, &config).await.unwrap();

        assert_eq!(progress.events_read, 10);
        assert_eq!(progress.events_indexed, 10);
        assert_eq!(progress.batches_committed, 4);
        assert!(progress.caught_up);
        assert_eq!(progress.current_ordinal, Some(9));
    }

    #[tokio::test]
    async fn integrity_report_with_mixed_issues() {
        // Test a report that has BOTH missing and mismatched entries
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        // Index: e0 correct, e1 wrong offset, e2 missing, e3 correct, e4 missing
        let mut lookup = MockIndexLookup::new();
        lookup.docs.insert("e0".to_string(), 0);
        lookup.docs.insert("e1".to_string(), 777); // wrong offset
        // e2 missing
        lookup.docs.insert("e3".to_string(), 3);
        // e4 missing
        lookup.total = 3;

        let config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &config).unwrap();
        assert!(!report.is_consistent);
        assert_eq!(report.index_matches, 3); // e0, e1, e3 found
        assert_eq!(report.missing_from_index.len(), 2); // e2, e4
        assert_eq!(report.offset_mismatches.len(), 1); // e1
        assert_eq!(report.offset_mismatches[0].event_id, "e1");
        assert_eq!(report.offset_mismatches[0].actual_offset, 777);
    }

    #[test]
    fn reindex_progress_equality() {
        let a = ReindexProgress {
            events_read: 10,
            events_indexed: 8,
            events_skipped: 1,
            events_filtered: 1,
            batches_committed: 2,
            current_ordinal: Some(9),
            caught_up: true,
            docs_cleared: 0,
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = ReindexProgress::new();
        assert_ne!(a, c);
    }

    #[test]
    fn integrity_report_serde_with_mismatches() {
        let report = IntegrityReport {
            log_events_scanned: 50,
            index_matches: 48,
            missing_from_index: vec!["e10".to_string()],
            offset_mismatches: vec![
                OffsetMismatch {
                    event_id: "e5".to_string(),
                    expected_offset: 5,
                    actual_offset: 500,
                },
                OffsetMismatch {
                    event_id: "e20".to_string(),
                    expected_offset: 20,
                    actual_offset: 2000,
                },
            ],
            is_consistent: false,
            total_index_docs: Some(48),
            checked_range: CheckedRange {
                start_ordinal: 0,
                end_ordinal: 49,
                events_checked: 50,
            },
        };

        let json = serde_json::to_string(&report).unwrap();
        let back: IntegrityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
        assert_eq!(back.offset_mismatches.len(), 2);
        assert_eq!(back.missing_from_index.len(), 1);
    }

    #[tokio::test]
    async fn backfill_then_integrity_check_partial() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        // Backfill ordinals 3-7 only
        let config = BackfillConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            consumer_id: "partial-verify".to_string(),
            batch_size: 20,
            range: BackfillRange::OrdinalRange { start: 3, end: 7 },
            dedup_on_replay: false,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            max_batches: 0,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline.backfill(&storage, &config).await.unwrap();
        assert_eq!(progress.events_indexed, 5);

        let lookup = MockIndexLookup::from_docs(&pipeline.backfill_writer().docs);

        // Check only the backfilled range — should be consistent
        let check_config = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: Some((3, 7)),
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &check_config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.index_matches, 5);

        // Check full range — should show gaps
        let full_check = IntegrityCheckConfig {
            source: RecorderSourceDescriptor::AppendLog {
                data_path: dir.path().join("events.log"),
            },
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let full_report = IntegrityChecker::check(&lookup, &full_check).unwrap();
        assert!(!full_report.is_consistent);
        assert_eq!(full_report.missing_from_index.len(), 5); // e0-e2, e8-e9
    }
}
