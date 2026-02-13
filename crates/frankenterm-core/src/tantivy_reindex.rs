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

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::recorder_storage::{
    CheckpointConsumerId, RecorderCheckpoint, RecorderOffset, RecorderStorage,
};
use crate::recording::RECORDER_EVENT_SCHEMA_VERSION_V1;
use crate::tantivy_ingest::{
    AppendLogReader, IndexWriteError, IndexWriter, IndexerError, map_event_to_document,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default consumer ID prefix for reindex operations.
pub const REINDEX_CONSUMER_PREFIX: &str = "tantivy-reindex";

/// Default consumer ID prefix for backfill operations.
pub const BACKFILL_CONSUMER_PREFIX: &str = "tantivy-backfill";

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
    /// Path to the append-log data file.
    pub data_path: PathBuf,
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
            data_path: PathBuf::from(".ft/recorder-log/events.log"),
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
    /// Path to the append-log data file.
    pub data_path: PathBuf,
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
            data_path: PathBuf::from(".ft/recorder-log/events.log"),
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

        let mut reader = match &checkpoint {
            Some(cp) => {
                let mut r = AppendLogReader::open_at_offset(
                    &config.data_path,
                    cp.upto_offset.byte_offset,
                    cp.upto_offset.ordinal,
                )?;
                // Skip past the checkpointed record
                let _ = r.next_record()?;
                progress.current_ordinal = Some(cp.upto_offset.ordinal);
                r
            }
            None => AppendLogReader::open(&config.data_path)?,
        };

        self.index_loop(
            storage,
            &mut reader,
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

        let mut progress = ReindexProgress::new();
        let consumer_id = CheckpointConsumerId(config.consumer_id.clone());

        // For ordinal ranges, we can start scanning from the range start
        let checkpoint = storage.read_checkpoint(&consumer_id).await?;

        let mut reader = match &checkpoint {
            Some(cp) => {
                let mut r = AppendLogReader::open_at_offset(
                    &config.data_path,
                    cp.upto_offset.byte_offset,
                    cp.upto_offset.ordinal,
                )?;
                let _ = r.next_record()?;
                progress.current_ordinal = Some(cp.upto_offset.ordinal);
                r
            }
            None => {
                // For ordinal ranges, skip ahead to the start
                if let BackfillRange::OrdinalRange { start, .. } = &config.range {
                    if *start > 0 {
                        AppendLogReader::open_at_ordinal(&config.data_path, *start)?
                    } else {
                        AppendLogReader::open(&config.data_path)?
                    }
                } else {
                    AppendLogReader::open(&config.data_path)?
                }
            }
        };

        self.index_loop(
            storage,
            &mut reader,
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
        reader: &mut AppendLogReader,
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

            let batch = reader.read_batch(batch_size)?;
            if batch.is_empty() {
                progress.caught_up = true;
                break;
            }

            let is_final_batch = batch.len() < batch_size;
            let mut last_offset: Option<RecorderOffset> = None;
            let mut indexed_in_batch = 0u64;

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
                    let _ = self.writer.delete_by_event_id(&doc.event_id);
                }

                match self.writer.add_document(&doc) {
                    Ok(()) => {
                        progress.events_indexed += 1;
                        indexed_in_batch += 1;
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
                if indexed_in_batch > 0 || progress.events_filtered > 0 {
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
    /// Path to the append-log data file.
    pub data_path: PathBuf,
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
            data_path: PathBuf::from(".ft/recorder-log/events.log"),
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
    /// Scans the append log and for each event, checks that:
    /// 1. A document with the event's `event_id` exists in the index.
    /// 2. The stored `log_offset` matches the event's actual ordinal.
    pub fn check<L: IndexLookup>(
        lookup: &L,
        config: &IntegrityCheckConfig,
    ) -> Result<IntegrityReport, IndexerError> {
        let mut reader = match config.ordinal_range {
            Some((start, _)) if start > 0 => {
                AppendLogReader::open_at_ordinal(&config.data_path, start)?
            }
            _ => AppendLogReader::open(&config.data_path)?,
        };

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

            let record = match reader.next_record()? {
                Some(r) => r,
                None => break,
            };

            let ordinal = record.offset.ordinal;

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
    use std::path::Path;
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
    async fn backfill_batch_size_zero_errors() {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg).unwrap();

        let config = BackfillConfig {
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &check_config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.index_matches, 8);
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
            data_path: dir.path().join("events.log"),
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
            data_path: dir.path().join("events.log"),
            ordinal_range: Some((3, 7)),
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let report = IntegrityChecker::check(&lookup, &check_config).unwrap();
        assert!(report.is_consistent);
        assert_eq!(report.index_matches, 5);

        // Check full range — should show gaps
        let full_check = IntegrityCheckConfig {
            data_path: dir.path().join("events.log"),
            ordinal_range: None,
            max_events: 0,
            expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        };

        let full_report = IntegrityChecker::check(&lookup, &full_check).unwrap();
        assert!(!full_report.is_consistent);
        assert_eq!(full_report.missing_from_index.len(), 5); // e0-e2, e8-e9
    }
}
