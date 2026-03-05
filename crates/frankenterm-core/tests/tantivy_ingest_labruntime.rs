//! LabRuntime-ported tantivy ingest tests for deterministic async testing.
//!
//! Ports all 46 `#[tokio::test]` functions from `tantivy_ingest.rs` to
//! asupersync-based `RuntimeFixture`, gaining seed-based reproducibility
//! for append-log reader, incremental indexer, lag monitor, event cursor,
//! and parity tests.
//!
//! Bead: ft-22x4r

#![cfg(all(feature = "asupersync-runtime", feature = "recorder-lexical"))]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointConsumerId,
    CursorRecord, DurabilityLevel, EventCursorError, RecorderEventCursor, RecorderEventReader,
    RecorderOffset, RecorderSourceDescriptor, RecorderStorage,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderLifecyclePhase, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{
    AppendLogEventSource, AppendLogReader, IncrementalIndexer, IndexCommitStats,
    IndexDocumentFields, IndexWriteError, IndexWriter, IndexerConfig, IndexerError,
    IndexerRunResult,
};
use std::path::Path;
use tempfile::tempdir;

// ===========================================================================
// Helpers (mirrors tantivy_ingest.rs test helpers)
// ===========================================================================

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
        source: RecorderSourceDescriptor::AppendLog {
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

// ===========================================================================
// Append-log reader tests
// ===========================================================================

#[test]
fn reader_reads_all_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    });
}

#[test]
fn reader_batch_limits() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// IncrementalIndexer tests
// ===========================================================================

#[test]
fn indexer_full_pipeline_cold_start() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let events: Vec<_> = (0..6)
            .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")))
            .collect();
        populate_log(&storage, events).await;

        // First run: index first 3
        let icfg = IndexerConfig {
            source: RecorderSourceDescriptor::AppendLog {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// Multiple event types in one batch
// ===========================================================================

#[test]
fn indexer_handles_mixed_event_types() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// Lag monitor tests
// ===========================================================================

#[test]
fn lag_monitor_no_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

        let lag = frankenterm_core::tantivy_ingest::compute_indexer_lag(&storage, "test-consumer")
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

        let lag = frankenterm_core::tantivy_ingest::compute_indexer_lag(&storage, "test-consumer")
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

        let lag = frankenterm_core::tantivy_ingest::compute_indexer_lag(&storage, "lag-test")
            .await
            .unwrap();
        assert_eq!(lag.log_head_ordinal, Some(9));
        assert_eq!(lag.indexer_ordinal, Some(4));
        assert_eq!(lag.records_behind, 5);
        assert!(!lag.caught_up);
    });
}

#[test]
fn lag_monitor_fully_caught_up() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

        let lag = frankenterm_core::tantivy_ingest::compute_indexer_lag(&storage, "lag-test-2")
            .await
            .unwrap();
        assert_eq!(lag.records_behind, 0);
        assert!(lag.caught_up);
    });
}

// ===========================================================================
// Incremental append-after-index test
// ===========================================================================

#[test]
fn indexer_picks_up_new_events_after_append() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// Checkpoint monotonicity across multiple runs
// ===========================================================================

#[test]
fn checkpoint_advances_monotonically_across_runs() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// Multi-pane indexing
// ===========================================================================

#[test]
fn indexer_handles_multi_pane_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// Append-log reader edge cases (async tests)
// ===========================================================================

#[test]
fn reader_skip_to_ordinal_beyond_eof() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
        populate_log(&storage, vec![sample_event("e1", 1, 0, "only")]).await;
        drop(storage);

        let result = AppendLogReader::open_at_ordinal(&cfg.data_path, 5);
        assert!(result.is_err());
        if let Err(frankenterm_core::tantivy_ingest::LogReadError::Corrupt { reason, .. }) = result
        {
            assert!(reason.contains("EOF before reaching ordinal"));
        } else {
            panic!("expected Corrupt error");
        }
    });
}

#[test]
fn reader_multiple_next_record_past_eof() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// IncrementalIndexer transient error propagation
// ===========================================================================

#[test]
fn indexer_transient_write_error_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let scfg = test_storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();
        populate_log(&storage, vec![sample_event("e1", 1, 0, "text")]).await;

        let icfg = test_indexer_config(dir.path());

        // Build a writer that returns Transient errors
        struct TransientFailWriter;
        impl IndexWriter for TransientFailWriter {
            fn add_document(&mut self, _doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
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

// ===========================================================================
// All-skipped batch still advances checkpoint
// ===========================================================================

#[test]
fn indexer_all_events_wrong_schema_still_commits_checkpoint() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// RecorderEventReader / RecorderEventCursor seam tests
// ===========================================================================

#[test]
fn append_log_event_source_wraps_reader_correctly() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// E2.F1.T3: Schema-dedup-EOF parity validation across reader paths
// ===========================================================================

#[test]
fn parity_single_event_indexing() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// ===========================================================================
// Mock FrankenSqlite cursor for testing abstraction
// ===========================================================================

#[test]
fn mock_frankensqlite_cursor_produces_identical_results() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
            ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
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
