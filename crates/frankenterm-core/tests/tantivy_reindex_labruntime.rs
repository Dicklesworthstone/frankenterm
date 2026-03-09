//! LabRuntime-ported tantivy reindex tests for deterministic async testing.
//!
//! Ports all 45 `#[tokio::test]` functions from `tantivy_reindex.rs` to
//! asupersync-based `RuntimeFixture`, gaining seed-based reproducibility
//! for full reindex, backfill, integrity check, deterministic range reindex,
//! and operator observability tests.
//!
//! Bead: ft-22x4r

#![cfg(all(feature = "asupersync-runtime", feature = "recorder-lexical"))]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CursorRecord, DurabilityLevel,
    EventCursorError, RecorderEventCursor, RecorderEventReader, RecorderOffset,
    RecorderSourceDescriptor, RecorderStorage,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{
    IndexCommitStats, IndexDocumentFields, IndexWriteError, IndexWriter, IndexerError,
    map_event_to_document,
};
use frankenterm_core::tantivy_reindex::{
    BackfillConfig, BackfillRange, IndexLookup, IntegrityCheckConfig, IntegrityChecker,
    ReindexConfig, ReindexObserver, ReindexPipeline, ReindexProgress, ReindexStats,
    ReindexableWriter,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

// ===========================================================================
// Helpers (mirrors tantivy_reindex.rs test helpers)
// ===========================================================================

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

/// Equivalent to `ReindexProgress::new()` which is private.
fn new_reindex_progress() -> ReindexProgress {
    ReindexProgress {
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

/// Equivalent to private `epoch_ms_now()`.
fn epoch_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ===========================================================================
// Mock ReindexableWriter
// ===========================================================================

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

// ===========================================================================
// Mock IndexLookup
// ===========================================================================

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

// ===========================================================================
// In-memory event reader for backend-neutral range parity tests
// ===========================================================================

struct InMemoryEventReader {
    records: Vec<CursorRecord>,
}

impl InMemoryEventReader {
    fn from_events(events: &[RecorderEvent]) -> Self {
        let records: Vec<_> = events
            .iter()
            .enumerate()
            .map(|(i, e)| CursorRecord {
                event: e.clone(),
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: i as u64 * 100,
                    ordinal: i as u64,
                },
            })
            .collect();
        Self { records }
    }
}

struct InMemoryCursor {
    records: Vec<CursorRecord>,
    pos: usize,
}

impl RecorderEventCursor for InMemoryCursor {
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
        if self.pos > 0 && self.pos <= self.records.len() {
            self.records[self.pos - 1].offset.clone()
        } else {
            RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            }
        }
    }
}

impl RecorderEventReader for InMemoryEventReader {
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
        Ok(Box::new(InMemoryCursor {
            records: remaining,
            pos: 0,
        }))
    }

    fn open_cursor_at_ordinal(
        &self,
        target_ordinal: u64,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        self.open_cursor(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: target_ordinal,
        })
    }

    fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
        Ok(self
            .records
            .last()
            .map(|r| RecorderOffset {
                segment_id: 0,
                byte_offset: r.offset.byte_offset + 100,
                ordinal: r.offset.ordinal + 1,
            })
            .unwrap_or(RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            }))
    }
}

// ===========================================================================
// Test observer for operator observability tests
// ===========================================================================

struct TestObserver {
    progress_calls: Arc<Mutex<Vec<(RecorderOffset, u64, u64)>>>,
    complete_calls: Arc<Mutex<Vec<ReindexStats>>>,
}

impl TestObserver {
    fn new() -> Self {
        Self {
            progress_calls: Arc::new(Mutex::new(Vec::new())),
            complete_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ReindexObserver for TestObserver {
    fn on_progress(&self, current: &RecorderOffset, total_estimate: u64, eta_ms: u64) {
        self.progress_calls
            .lock()
            .unwrap()
            .push((current.clone(), total_estimate, eta_ms));
    }

    fn on_complete(&self, stats: &ReindexStats) {
        self.complete_calls.lock().unwrap().push(stats.clone());
    }
}

// ===========================================================================
// Full reindex tests
// ===========================================================================

#[test]
fn full_reindex_cold_start() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn full_reindex_resumes_from_checkpoint() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn full_reindex_empty_log() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn full_reindex_no_clear() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn full_reindex_batch_size_zero_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

// ===========================================================================
// Backfill tests -- ordinal range
// ===========================================================================

#[test]
fn backfill_ordinal_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_ordinal_range_resumes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

        // Fourth run -- past end of range
        let config_unlimited = BackfillConfig {
            max_batches: 0,
            ..config.clone()
        };
        let mut p4 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r4 = p4.backfill(&storage, &config_unlimited).await.unwrap();
        assert!(r4.caught_up);
    });
}

// ===========================================================================
// Backfill tests -- time range
// ===========================================================================

#[test]
fn backfill_time_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_all_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_schema_mismatch_skipped() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_rejected_docs_skipped() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_dedup_rejected_docs_commit_delete_mutations() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_batch_size_zero_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

// ===========================================================================
// Integrity checker tests
// ===========================================================================

#[test]
fn integrity_check_consistent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_check_missing_docs() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_check_offset_mismatch() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_check_with_ordinal_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_check_max_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_check_empty_log() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_check_skips_wrong_schema() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

// ===========================================================================
// Integration: reindex then integrity check
// ===========================================================================

#[test]
fn reindex_then_integrity_check_consistent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

// ===========================================================================
// DarkBadger batch tests
// ===========================================================================

#[test]
fn reindex_dedup_calls_delete_before_add() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn reindex_no_dedup_skips_delete() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn reindex_dedup_delete_failure_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn reindex_commit_failure_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_commit_failure_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn pipeline_writer_accessor() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let writer = MockReindexWriter::new();
        let pipeline = ReindexPipeline::new(writer);
        // writer() returns a reference to the inner writer
        assert!(!pipeline.writer().cleared);
        assert_eq!(pipeline.writer().docs.len(), 0);
    });
}

#[test]
fn pipeline_into_writer_consumes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let writer = MockReindexWriter::new();
        let pipeline = ReindexPipeline::new(writer);
        let recovered = pipeline.into_writer();
        assert_eq!(recovered.commits, 0);
        assert!(!recovered.cleared);
    });
}

#[test]
fn pipeline_backfill_writer_accessor() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let writer = MockReindexWriter::new();
        let pipeline = ReindexPipeline::new_for_backfill(writer);
        assert_eq!(pipeline.backfill_writer().docs.len(), 0);
    });
}

#[test]
fn reindex_multi_batch_progress_accumulates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn integrity_report_with_mixed_issues() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn backfill_then_integrity_check_partial() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

        // Check only the backfilled range -- should be consistent
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

        // Check full range -- should show gaps
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
    });
}

// ===========================================================================
// Deterministic range reindex [from, to) -- E2.F2.T2
// ===========================================================================

#[test]
fn reindex_range_from_to_exclusive() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };

        // Range [5, 8) should index ordinals 5, 6, 7 -- NOT 4 or 8
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 8,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "range-exclusive-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 3);
        assert!(progress.caught_up);

        let ids: Vec<&str> = pipeline
            .backfill_writer()
            .docs
            .iter()
            .map(|d| d.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e5", "e6", "e7"]);
    });
}

#[test]
fn reindex_range_empty_produces_zero_documents() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e0", 1, 0, "text")]).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };

        // [5, 5) is empty
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        };
        let to = from.clone();

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "empty-range-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 0);
        assert_eq!(progress.events_read, 0);
        assert!(pipeline.backfill_writer().docs.is_empty());
    });
}

#[test]
fn reindex_range_single_event() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };

        // [3, 4) should index exactly ordinal 3
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 3,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 4,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "single-event-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 1);
        assert_eq!(pipeline.backfill_writer().docs[0].event_id, "e3");
    });
}

#[test]
fn reindex_replay_same_range_idempotent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };

        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 2,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 6,
        };

        // Run 1: index [2, 6)
        let mut p1 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r1 = p1
            .reindex_range(
                &storage,
                &source,
                from.clone(),
                to.clone(),
                "replay-r1",
                20,
                true,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        // Run 2: same range with different consumer (simulates replay)
        let mut p2 = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r2 = p2
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "replay-r2",
                20,
                true,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        // Both runs should produce identical results
        assert_eq!(r1.events_indexed, r2.events_indexed);
        assert_eq!(r1.events_indexed, 4); // ordinals 2, 3, 4, 5

        let ids1: Vec<&str> = p1
            .backfill_writer()
            .docs
            .iter()
            .map(|d| d.event_id.as_str())
            .collect();
        let ids2: Vec<&str> = p2
            .backfill_writer()
            .docs
            .iter()
            .map(|d| d.event_id.as_str())
            .collect();
        assert_eq!(ids1, ids2);
        assert_eq!(ids1, vec!["e2", "e3", "e4", "e5"]);
    });
}

#[test]
fn reindex_range_parity_across_backends() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Tests that the same range produces identical documents from
        // an AppendLog backend and an in-memory "FrankenSqlite-like" backend.
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..8)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events.clone()).await;

        // --- AppendLog backend ---
        let source_al = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 2,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        };

        let mut p_al = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let r_al = p_al
            .reindex_range(
                &storage,
                &source_al,
                from.clone(),
                to.clone(),
                "parity-al",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        // --- In-memory backend (simulating FrankenSqlite) ---
        let mem_reader = InMemoryEventReader::from_events(&events);
        let mut cursor = mem_reader.open_cursor_at_ordinal(from.ordinal).unwrap();

        // Manually run the same range using the in-memory cursor
        let mut mem_docs = Vec::new();
        let batch = cursor.next_batch(20).unwrap();
        for record in &batch {
            if record.offset.ordinal >= to.ordinal {
                break;
            }
            if record.offset.ordinal < from.ordinal {
                continue;
            }
            mem_docs.push(map_event_to_document(&record.event, record.offset.ordinal));
        }

        // Compare results
        assert_eq!(r_al.events_indexed, mem_docs.len() as u64);
        assert_eq!(r_al.events_indexed, 3); // ordinals 2, 3, 4

        let al_ids: Vec<&str> = p_al
            .backfill_writer()
            .docs
            .iter()
            .map(|d| d.event_id.as_str())
            .collect();
        let mem_ids: Vec<&str> = mem_docs.iter().map(|d| d.event_id.as_str()).collect();
        assert_eq!(al_ids, mem_ids);
        assert_eq!(al_ids, vec!["e2", "e3", "e4"]);

        // Verify document field parity
        for (al_doc, mem_doc) in p_al.backfill_writer().docs.iter().zip(mem_docs.iter()) {
            assert_eq!(al_doc.event_id, mem_doc.event_id);
            assert_eq!(al_doc.pane_id, mem_doc.pane_id);
            assert_eq!(al_doc.log_offset, mem_doc.log_offset);
            assert_eq!(al_doc.sequence, mem_doc.sequence);
        }
    });
}

#[test]
fn reindex_range_batch_size_zero_errors() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg).unwrap();

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 10,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let err = pipeline
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "zero-batch",
                0,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, IndexerError::Config(_)));
    });
}

#[test]
fn reindex_range_schema_mismatch_skipped() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let mut bad = sample_event("bad", 1, 0, "bad-schema");
        bad.schema_version = "ft.recorder.event.v99".to_string();
        let good1 = sample_event("good1", 1, 1, "ok");
        let good2 = sample_event("good2", 1, 2, "ok");

        populate_log(&storage, vec![bad, good1, good2]).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 3,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "schema-skip-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 2);
        assert_eq!(progress.events_skipped, 1);
    });
}

#[test]
fn reindex_range_reversed_bounds_empty() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // [8, 3) should produce zero documents (to < from)
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(
            &storage,
            vec![sample_event("e0", 1, 0, "t"), sample_event("e1", 1, 1, "t")],
        )
        .await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };

        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 8,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 3,
        };

        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let progress = pipeline
            .reindex_range(
                &storage,
                &source,
                from,
                to,
                "reversed-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 0);
        assert_eq!(progress.events_read, 0);
    });
}

// ===========================================================================
// Operator observability -- ReindexStats + observer callbacks -- E2.F2.T3
// ===========================================================================

#[test]
fn reindex_progress_callback_called() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..10)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 10,
        };

        let observer = TestObserver::new();
        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let (progress, _stats) = pipeline
            .reindex_range_observed(
                &storage,
                &source,
                from,
                to,
                "progress-cb-test",
                5,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                &observer,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 10);

        // on_progress should have been called at least once per batch commit
        let calls = observer.progress_calls.lock().unwrap();
        assert!(
            calls.len() >= 2,
            "expected >= 2 progress calls, got {}",
            calls.len()
        );
    });
}

#[test]
fn reindex_progress_percentage_monotonic() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..20)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 20,
        };

        let observer = TestObserver::new();
        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let _ = pipeline
            .reindex_range_observed(
                &storage,
                &source,
                from,
                to,
                "monotonic-test",
                3,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                &observer,
            )
            .await
            .unwrap();

        // Progress ordinals should be monotonically increasing
        let calls = observer.progress_calls.lock().unwrap();
        assert!(calls.len() >= 2);
        for i in 1..calls.len() {
            assert!(
                calls[i].0.ordinal >= calls[i - 1].0.ordinal,
                "ordinal {} < {} at progress call {}",
                calls[i].0.ordinal,
                calls[i - 1].0.ordinal,
                i
            );
        }
    });
}

#[test]
fn reindex_stats_accurate_event_count() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..8)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 2,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 6,
        };

        let observer = TestObserver::new();
        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let (progress, stats) = pipeline
            .reindex_range_observed(
                &storage,
                &source,
                from,
                to,
                "stats-count-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                &observer,
            )
            .await
            .unwrap();

        // Stats should match progress
        assert_eq!(stats.indexed_count, progress.events_indexed);
        assert_eq!(stats.indexed_count, 4); // ordinals 2, 3, 4, 5
        assert_eq!(stats.event_count, progress.events_read);
        assert_eq!(stats.skipped_count, 0);
        assert_eq!(stats.filtered_count, 0);
        assert!(stats.caught_up);
        // final_ordinal may be the boundary event that triggered the stop
        assert!(stats.final_ordinal.is_some());
    });
}

#[test]
fn reindex_complete_callback_with_stats() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..5)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect();
        populate_log(&storage, events).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let from = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        };
        let to = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        };

        let observer = TestObserver::new();
        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let _ = pipeline
            .reindex_range_observed(
                &storage,
                &source,
                from,
                to,
                "complete-cb-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                &observer,
            )
            .await
            .unwrap();

        // on_complete should be called exactly once
        let complete_calls = observer.complete_calls.lock().unwrap();
        assert_eq!(complete_calls.len(), 1);
        let stats = &complete_calls[0];
        assert_eq!(stats.indexed_count, 5);
        assert!(stats.caught_up);
        assert!(stats.duration_ms < 10_000); // should finish in well under 10s
    });
}

#[test]
fn reindex_complete_callback_on_empty_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        populate_log(&storage, vec![sample_event("e0", 1, 0, "text")]).await;

        let source = RecorderSourceDescriptor::AppendLog {
            data_path: dir.path().join("events.log"),
        };
        let offset = RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        };

        let observer = TestObserver::new();
        let mut pipeline = ReindexPipeline::new_for_backfill(MockReindexWriter::new());
        let (progress, stats) = pipeline
            .reindex_range_observed(
                &storage,
                &source,
                offset.clone(),
                offset,
                "empty-complete-test",
                20,
                false,
                RECORDER_EVENT_SCHEMA_VERSION_V1,
                &observer,
            )
            .await
            .unwrap();

        assert_eq!(progress.events_indexed, 0);
        // on_complete still called even for empty ranges
        let complete_calls = observer.complete_calls.lock().unwrap();
        assert_eq!(complete_calls.len(), 1);
        assert_eq!(complete_calls[0].indexed_count, 0);
        assert_eq!(stats.indexed_count, 0);
    });
}

#[test]
fn reindex_stats_from_progress_computes_throughput() {
    let mut progress = new_reindex_progress();
    progress.events_read = 1000;
    progress.events_indexed = 900;
    progress.events_skipped = 50;
    progress.events_filtered = 50;
    progress.current_ordinal = Some(999);
    progress.caught_up = true;

    // Simulate a 2-second run
    let start_ms = epoch_ms_now().saturating_sub(2000);
    let stats = ReindexStats::from_progress(&progress, start_ms);

    assert_eq!(stats.event_count, 1000);
    assert_eq!(stats.indexed_count, 900);
    assert_eq!(stats.skipped_count, 50);
    assert_eq!(stats.filtered_count, 50);
    assert!(stats.caught_up);
    assert_eq!(stats.final_ordinal, Some(999));
    assert!(stats.duration_ms >= 1900);
    assert!(stats.events_per_sec > 0.0);
}
