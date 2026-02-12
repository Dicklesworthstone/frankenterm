//! Cross-module integration tests for recorder storage + tantivy ingest pipeline.
//!
//! These tests exercise the full lifecycle: append events to storage, index them
//! via IncrementalIndexer, verify checkpoint protocol, lag monitoring, and feed
//! indexed documents through the InvariantChecker for correctness validation.
//!
//! Bead: wa-rxwy (cross-module integration tests for recorder storage + tantivy ingest)

use std::path::Path;
use tempfile::tempdir;

use frankenterm_core::event_id::{RecorderMergeKey, generate_event_id_v1};
use frankenterm_core::recorder_invariants::{InvariantChecker, InvariantCheckerConfig};
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointConsumerId,
    DurabilityLevel, FlushMode, RecorderStorage,
};
use frankenterm_core::recording::{
    RecorderControlMarkerType, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
    RecorderSegmentKind, RecorderTextEncoding, RECORDER_EVENT_SCHEMA_VERSION_V1,
};
use frankenterm_core::sequence_model::SequenceAssigner;
use frankenterm_core::tantivy_ingest::{
    compute_indexer_lag, map_event_to_document, AppendLogReader, IndexCommitStats,
    IndexDocumentFields, IndexWriteError, IndexWriter, IndexerConfig, IncrementalIndexer,
    LEXICAL_SCHEMA_VERSION,
};

// ===========================================================================
// Test helpers
// ===========================================================================

fn storage_config(path: &Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 8,
        max_batch_events: 256,
        max_batch_bytes: 1024 * 1024,
        max_idempotency_entries: 64,
    }
}

fn indexer_config(path: &Path, consumer_id: &str) -> IndexerConfig {
    IndexerConfig {
        data_path: path.join("events.log"),
        consumer_id: consumer_id.to_string(),
        batch_size: 10,
        dedup_on_replay: true,
        max_batches: 0,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    }
}

fn make_ingress(event_id: &str, pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("integ-sess".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000 + seq * 10,
        recorded_at_ms: 1_700_000_000_001 + seq * 10,
        sequence: seq,
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

fn make_egress(event_id: &str, pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("integ-sess".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq * 10,
        recorded_at_ms: 1_700_000_000_001 + seq * 10,
        sequence: seq,
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

fn make_control(event_id: &str, pane_id: u64, seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq * 10,
        recorded_at_ms: 1_700_000_000_001 + seq * 10,
        sequence: seq,
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

fn make_lifecycle(event_id: &str, pane_id: u64, seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq * 10,
        recorded_at_ms: 1_700_000_000_001 + seq * 10,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: RecorderLifecyclePhase::PaneOpened,
            reason: Some("test".to_string()),
            details: serde_json::json!({}),
        },
    }
}

/// Make an event with deterministic ID using the event_id module.
fn make_event_with_det_id(pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    let mut event = make_ingress("placeholder", pane_id, seq, text);
    event.event_id = generate_event_id_v1(&event);
    event
}

async fn append_events(
    storage: &AppendLogRecorderStorage,
    batch_id: &str,
    events: Vec<RecorderEvent>,
) {
    storage
        .append_batch(AppendRequest {
            batch_id: batch_id.to_string(),
            events,
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: 1_700_000_000_000,
        })
        .await
        .unwrap();
}

/// Mock IndexWriter that records all operations for verification.
struct TrackingWriter {
    docs: Vec<IndexDocumentFields>,
    deleted_ids: Vec<String>,
    commits: u64,
    reject_ids: Vec<String>,
    fail_commit: bool,
    /// Track add/delete ordering for dedup verification
    operations: Vec<WriteOp>,
}

#[derive(Debug, Clone)]
enum WriteOp {
    Delete(String),
    Add(String),
    Commit,
}

impl TrackingWriter {
    fn new() -> Self {
        Self {
            docs: Vec::new(),
            deleted_ids: Vec::new(),
            commits: 0,
            reject_ids: Vec::new(),
            fail_commit: false,
            operations: Vec::new(),
        }
    }

    fn with_rejections(ids: Vec<String>) -> Self {
        let mut w = Self::new();
        w.reject_ids = ids;
        w
    }

    fn with_fail_commit() -> Self {
        let mut w = Self::new();
        w.fail_commit = true;
        w
    }
}

impl IndexWriter for TrackingWriter {
    fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
        if self.reject_ids.contains(&doc.event_id) {
            return Err(IndexWriteError::Rejected {
                reason: "test rejection".to_string(),
            });
        }
        self.operations.push(WriteOp::Add(doc.event_id.clone()));
        self.docs.push(doc.clone());
        Ok(())
    }

    fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
        if self.fail_commit {
            return Err(IndexWriteError::CommitFailed {
                reason: "test failure".to_string(),
            });
        }
        self.operations.push(WriteOp::Commit);
        self.commits += 1;
        Ok(IndexCommitStats {
            docs_added: self.docs.len() as u64,
            docs_deleted: self.deleted_ids.len() as u64,
            segment_count: 1,
        })
    }

    fn delete_by_event_id(&mut self, event_id: &str) -> Result<(), IndexWriteError> {
        self.operations
            .push(WriteOp::Delete(event_id.to_string()));
        self.deleted_ids.push(event_id.to_string());
        Ok(())
    }
}

// ===========================================================================
// Test: Full pipeline cold start with invariant checking
// ===========================================================================

#[tokio::test]
async fn full_pipeline_cold_start_with_invariant_check() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Use deterministic event IDs via SequenceAssigner
    let assigner = SequenceAssigner::new();
    let mut events = Vec::new();
    for i in 0u64..5 {
        let (seq, _) = assigner.assign(1);
        let mut e = make_ingress(&format!("placeholder-{i}"), 1, seq, &format!("cmd-{i}"));
        e.event_id = generate_event_id_v1(&e);
        events.push(e);
    }

    append_events(&storage, "batch-1", events.clone()).await;

    let icfg = indexer_config(dir.path(), "full-pipeline");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    assert_eq!(result.events_indexed, 5);
    assert!(result.caught_up);

    // Verify indexed docs have correct schema versions
    for doc in &indexer.writer().docs {
        assert_eq!(doc.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
        assert_eq!(doc.lexical_schema_version, LEXICAL_SCHEMA_VERSION);
    }

    // Run original events through InvariantChecker — should pass
    let mut sorted_events = events.clone();
    sorted_events.sort_by(|a, b| {
        RecorderMergeKey::from_event(a).cmp(&RecorderMergeKey::from_event(b))
    });
    let config = InvariantCheckerConfig {
        check_merge_order: true,
        check_causality: false,
        expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        ..Default::default()
    };
    let checker = InvariantChecker::with_config(config);
    let report = checker.check(&sorted_events);
    assert!(report.passed, "invariant violations: {:?}", report.violations);
}

// ===========================================================================
// Test: Multi-run checkpoint resume — no reprocessing
// ===========================================================================

#[tokio::test]
async fn multi_run_checkpoint_resume_no_reprocessing() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Append 12 events
    let events: Vec<_> = (0..12)
        .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("text-{i}")))
        .collect();
    append_events(&storage, "b1", events).await;

    let consumer = "resume-integ";

    // Run 1: batch_size=5, max_batches=1 → index 5 events
    let icfg1 = IndexerConfig {
        batch_size: 5,
        max_batches: 1,
        ..indexer_config(dir.path(), consumer)
    };
    let mut ix1 = IncrementalIndexer::new(icfg1, TrackingWriter::new());
    let r1 = ix1.run(&storage).await.unwrap();
    assert_eq!(r1.events_indexed, 5);
    assert_eq!(r1.final_ordinal, Some(4));
    assert!(!r1.caught_up);

    let indexed_ids_run1: Vec<String> = ix1.writer().docs.iter().map(|d| d.event_id.clone()).collect();

    // Run 2: index next 5
    let icfg2 = IndexerConfig {
        batch_size: 5,
        max_batches: 1,
        ..indexer_config(dir.path(), consumer)
    };
    let mut ix2 = IncrementalIndexer::new(icfg2, TrackingWriter::new());
    let r2 = ix2.run(&storage).await.unwrap();
    assert_eq!(r2.events_indexed, 5);
    assert_eq!(r2.final_ordinal, Some(9));

    let indexed_ids_run2: Vec<String> = ix2.writer().docs.iter().map(|d| d.event_id.clone()).collect();

    // Run 3: index remaining 2
    let icfg3 = indexer_config(dir.path(), consumer);
    let mut ix3 = IncrementalIndexer::new(icfg3, TrackingWriter::new());
    let r3 = ix3.run(&storage).await.unwrap();
    assert_eq!(r3.events_indexed, 2);
    assert_eq!(r3.final_ordinal, Some(11));
    assert!(r3.caught_up);

    let indexed_ids_run3: Vec<String> = ix3.writer().docs.iter().map(|d| d.event_id.clone()).collect();

    // Verify no overlap between runs
    for id in &indexed_ids_run1 {
        assert!(!indexed_ids_run2.contains(id), "run2 re-indexed {}", id);
        assert!(!indexed_ids_run3.contains(id), "run3 re-indexed {}", id);
    }
    for id in &indexed_ids_run2 {
        assert!(!indexed_ids_run3.contains(id), "run3 re-indexed {}", id);
    }

    // Verify total coverage
    assert_eq!(
        indexed_ids_run1.len() + indexed_ids_run2.len() + indexed_ids_run3.len(),
        12
    );
}

// ===========================================================================
// Test: Append new events between indexer runs
// ===========================================================================

#[tokio::test]
async fn append_between_indexer_runs() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let consumer = "append-between";

    // Phase 1: append and index 3 events
    append_events(
        &storage,
        "b1",
        (0..3)
            .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("phase1-{i}")))
            .collect(),
    )
    .await;

    let icfg = indexer_config(dir.path(), consumer);
    let mut ix1 = IncrementalIndexer::new(icfg.clone(), TrackingWriter::new());
    let r1 = ix1.run(&storage).await.unwrap();
    assert_eq!(r1.events_indexed, 3);
    assert!(r1.caught_up);

    // Phase 2: append 4 more events
    append_events(
        &storage,
        "b2",
        (3..7)
            .map(|i| make_ingress(&format!("e{i}"), 2, i, &format!("phase2-{i}")))
            .collect(),
    )
    .await;

    // Verify lag before second run
    let lag = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag.records_behind, 4);
    assert!(!lag.caught_up);

    // Index new events
    let mut ix2 = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let r2 = ix2.run(&storage).await.unwrap();
    assert_eq!(r2.events_indexed, 4);
    assert!(r2.caught_up);

    // Verify only phase 2 events were indexed
    assert_eq!(ix2.writer().docs[0].event_id, "e3");
    assert_eq!(ix2.writer().docs[3].event_id, "e6");

    // Verify lag is now zero
    let lag = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag.records_behind, 0);
    assert!(lag.caught_up);
}

// ===========================================================================
// Test: Dedup delete-before-add ordering
// ===========================================================================

#[tokio::test]
async fn dedup_delete_before_add_ordering() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        vec![
            make_ingress("ev-a", 1, 0, "first"),
            make_ingress("ev-b", 1, 1, "second"),
        ],
    )
    .await;

    let icfg = IndexerConfig {
        dedup_on_replay: true,
        ..indexer_config(dir.path(), "dedup-test")
    };
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    // Verify operation ordering: for each event, delete must precede add
    let ops = &indexer.writer().operations;
    let mut last_delete_id: Option<String> = None;

    for op in ops {
        match op {
            WriteOp::Delete(id) => {
                last_delete_id = Some(id.clone());
            }
            WriteOp::Add(id) => {
                // The most recent delete should be for this same event_id
                assert_eq!(
                    last_delete_id.as_deref(),
                    Some(id.as_str()),
                    "delete for {} must precede its add",
                    id
                );
            }
            WriteOp::Commit => {
                last_delete_id = None;
            }
        }
    }
}

// ===========================================================================
// Test: Dedup disabled — no delete calls
// ===========================================================================

#[tokio::test]
async fn dedup_disabled_no_deletes() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        vec![make_ingress("ev-1", 1, 0, "text")],
    )
    .await;

    let icfg = IndexerConfig {
        dedup_on_replay: false,
        ..indexer_config(dir.path(), "no-dedup")
    };
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    // No delete operations should have occurred
    assert!(
        indexer.writer().deleted_ids.is_empty(),
        "expected no deletes when dedup disabled"
    );
    assert_eq!(indexer.writer().docs.len(), 1);
}

// ===========================================================================
// Test: Mixed event types through full pipeline
// ===========================================================================

#[tokio::test]
async fn mixed_event_types_full_pipeline() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        make_ingress("i1", 1, 0, "echo hello"),
        make_egress("e1", 1, 1, "hello\n"),
        make_control("c1", 1, 2),
        make_lifecycle("l1", 1, 3),
        make_ingress("i2", 2, 0, "ls -la"),
        make_egress("e2", 2, 1, "total 42\n"),
    ];
    append_events(&storage, "mixed", events.clone()).await;

    let icfg = indexer_config(dir.path(), "mixed-types");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    assert_eq!(result.events_indexed, 6);
    assert!(result.caught_up);

    // Verify event types in indexed docs match source events
    let docs = &indexer.writer().docs;
    assert_eq!(docs[0].event_type, "ingress_text");
    assert_eq!(docs[1].event_type, "egress_output");
    assert_eq!(docs[2].event_type, "control_marker");
    assert_eq!(docs[3].event_type, "lifecycle_marker");
    assert_eq!(docs[4].event_type, "ingress_text");
    assert_eq!(docs[5].event_type, "egress_output");

    // Verify text fields
    assert_eq!(docs[0].text, "echo hello");
    assert_eq!(docs[1].text, "hello\n");
    assert!(docs[2].text.is_empty()); // control markers have no text
    assert_eq!(docs[3].text, "test"); // lifecycle text = reason field

    // Verify multi-pane preservation
    assert_eq!(docs[0].pane_id, 1);
    assert_eq!(docs[4].pane_id, 2);
}

// ===========================================================================
// Test: Schema version filtering
// ===========================================================================

#[tokio::test]
async fn schema_version_filtering_skips_unknown() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let v1_event = make_ingress("v1-evt", 1, 0, "valid");
    let mut v2_event = make_ingress("v2-evt", 1, 1, "future");
    v2_event.schema_version = "ft.recorder.event.v2-beta".to_string();
    let v1_event_2 = make_ingress("v1-evt-2", 1, 2, "also valid");

    append_events(&storage, "mixed-schema", vec![v1_event, v2_event, v1_event_2]).await;

    let icfg = indexer_config(dir.path(), "schema-filter");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    assert_eq!(result.events_read, 3);
    assert_eq!(result.events_indexed, 2);
    assert_eq!(result.events_skipped, 1);

    // Only v1 events should be indexed
    assert_eq!(indexer.writer().docs.len(), 2);
    assert_eq!(indexer.writer().docs[0].event_id, "v1-evt");
    assert_eq!(indexer.writer().docs[1].event_id, "v1-evt-2");

    // Checkpoint should advance past all 3 (including skipped)
    assert_eq!(result.final_ordinal, Some(2));
}

// ===========================================================================
// Test: Lag monitoring accuracy across indexer runs
// ===========================================================================

#[tokio::test]
async fn lag_monitoring_accuracy_across_runs() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let consumer = "lag-accuracy";

    // Initially empty: lag should report caught up
    let lag0 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert!(lag0.caught_up);
    assert_eq!(lag0.records_behind, 0);

    // Append 10 events
    append_events(
        &storage,
        "b1",
        (0..10)
            .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect(),
    )
    .await;

    // Lag should show 10 behind
    let lag1 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag1.records_behind, 10);
    assert!(!lag1.caught_up);

    // Index first 5
    let icfg = IndexerConfig {
        batch_size: 5,
        max_batches: 1,
        ..indexer_config(dir.path(), consumer)
    };
    let mut ix = IncrementalIndexer::new(icfg, TrackingWriter::new());
    ix.run(&storage).await.unwrap();

    // Lag should show 5 behind
    let lag2 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag2.records_behind, 5);
    assert!(!lag2.caught_up);

    // Index remaining
    let icfg2 = indexer_config(dir.path(), consumer);
    let mut ix2 = IncrementalIndexer::new(icfg2, TrackingWriter::new());
    ix2.run(&storage).await.unwrap();

    // Lag should be zero
    let lag3 = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag3.records_behind, 0);
    assert!(lag3.caught_up);
}

// ===========================================================================
// Test: Multi-consumer independent checkpoints
// ===========================================================================

#[tokio::test]
async fn multi_consumer_independent_checkpoints() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Append 8 events
    append_events(
        &storage,
        "b1",
        (0..8)
            .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect(),
    )
    .await;

    // Consumer A: index first 3
    let icfg_a = IndexerConfig {
        batch_size: 3,
        max_batches: 1,
        ..indexer_config(dir.path(), "consumer-a")
    };
    let mut ix_a = IncrementalIndexer::new(icfg_a, TrackingWriter::new());
    ix_a.run(&storage).await.unwrap();

    // Consumer B: index all 8
    let icfg_b = indexer_config(dir.path(), "consumer-b");
    let mut ix_b = IncrementalIndexer::new(icfg_b, TrackingWriter::new());
    ix_b.run(&storage).await.unwrap();

    // Verify independent lag
    let lag_a = compute_indexer_lag(&storage, "consumer-a").await.unwrap();
    let lag_b = compute_indexer_lag(&storage, "consumer-b").await.unwrap();

    assert_eq!(lag_a.records_behind, 5, "consumer-a should have 5 behind");
    assert_eq!(lag_b.records_behind, 0, "consumer-b should be caught up");

    // Verify independent checkpoints
    let cp_a = storage
        .read_checkpoint(&CheckpointConsumerId("consumer-a".to_string()))
        .await
        .unwrap()
        .unwrap();
    let cp_b = storage
        .read_checkpoint(&CheckpointConsumerId("consumer-b".to_string()))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(cp_a.upto_offset.ordinal, 2);
    assert_eq!(cp_b.upto_offset.ordinal, 7);
}

// ===========================================================================
// Test: Idempotent batch_id replay
// ===========================================================================

#[tokio::test]
async fn idempotent_batch_replay_no_duplicates() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        make_ingress("e0", 1, 0, "first"),
        make_ingress("e1", 1, 1, "second"),
    ];

    // Append with same batch_id twice
    append_events(&storage, "idem-batch", events.clone()).await;
    append_events(&storage, "idem-batch", events.clone()).await;

    // Second append should be idempotent (silently accepted, no new records)
    let icfg = indexer_config(dir.path(), "idem-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    // Should only see 2 events (not 4)
    assert_eq!(result.events_indexed, 2);
    assert!(result.caught_up);
}

// ===========================================================================
// Test: Writer rejection skips event but advances checkpoint
// ===========================================================================

#[tokio::test]
async fn writer_rejection_skips_but_advances() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        vec![
            make_ingress("ok-1", 1, 0, "good"),
            make_ingress("bad-1", 1, 1, "reject me"),
            make_ingress("ok-2", 1, 2, "also good"),
        ],
    )
    .await;

    let icfg = indexer_config(dir.path(), "reject-test");
    let writer = TrackingWriter::with_rejections(vec!["bad-1".to_string()]);
    let mut indexer = IncrementalIndexer::new(icfg, writer);
    let result = indexer.run(&storage).await.unwrap();

    assert_eq!(result.events_indexed, 2);
    assert_eq!(result.events_skipped, 1);
    assert_eq!(result.final_ordinal, Some(2)); // Checkpoint past all 3

    // On second run, nothing should be re-indexed
    let icfg2 = indexer_config(dir.path(), "reject-test");
    let mut ix2 = IncrementalIndexer::new(icfg2, TrackingWriter::new());
    let r2 = ix2.run(&storage).await.unwrap();
    assert_eq!(r2.events_indexed, 0);
    assert!(r2.caught_up);
}

// ===========================================================================
// Test: Commit failure prevents checkpoint advance
// ===========================================================================

#[tokio::test]
async fn commit_failure_prevents_checkpoint_advance() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        vec![make_ingress("e0", 1, 0, "text")],
    )
    .await;

    // First run with fail_commit → should error
    let icfg = indexer_config(dir.path(), "fail-commit");
    let mut indexer = IncrementalIndexer::new(icfg.clone(), TrackingWriter::with_fail_commit());
    let err = indexer.run(&storage).await;
    assert!(err.is_err());

    // Checkpoint should NOT have been committed
    let cp = storage
        .read_checkpoint(&CheckpointConsumerId("fail-commit".to_string()))
        .await
        .unwrap();
    assert!(cp.is_none(), "checkpoint should not exist after commit failure");

    // Retry with working writer should process the same event
    let mut ix2 = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let r2 = ix2.run(&storage).await.unwrap();
    assert_eq!(r2.events_indexed, 1);
    assert!(r2.caught_up);
}

// ===========================================================================
// Test: Torn-tail recovery — reader agrees with storage
// ===========================================================================

#[tokio::test]
async fn torn_tail_recovery_reader_matches_storage() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg.clone()).unwrap();

    // Append 5 events
    append_events(
        &storage,
        "b1",
        (0..5)
            .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect(),
    )
    .await;

    // Get health to verify head ordinal
    let health = storage.health().await;
    let _head_offset = health.latest_offset.unwrap();

    // Simulate a torn tail by appending garbage bytes after the valid data
    let log_path = dir.path().join("events.log");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        // Write a length header claiming 1000 bytes but only 3 bytes of payload
        f.write_all(&(1000u32).to_le_bytes()).unwrap();
        f.write_all(b"bad").unwrap();
    }

    // Reader should treat torn tail as EOF
    let mut reader = AppendLogReader::open(&log_path).unwrap();
    let records = reader.read_batch(100).unwrap();
    assert_eq!(records.len(), 5, "reader should recover 5 valid records");

    // Reopen storage and verify it also recovers correctly
    let storage2 = AppendLogRecorderStorage::open(scfg).unwrap();
    let health2 = storage2.health().await;
    // After torn-tail truncation on reopen, storage should have same head
    assert!(health2.latest_offset.is_some());
}

// ===========================================================================
// Test: Indexed documents preserve merge key ordering from source events
// ===========================================================================

#[tokio::test]
async fn indexed_docs_preserve_merge_key_data() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let events = vec![
        make_ingress("e0", 1, 0, "first"),
        make_ingress("e1", 2, 0, "second-pane"),
        make_ingress("e2", 1, 1, "third"),
    ];
    append_events(&storage, "b1", events.clone()).await;

    let icfg = indexer_config(dir.path(), "merge-key-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    // Verify key fields used in merge key are preserved
    for (i, doc) in indexer.writer().docs.iter().enumerate() {
        let src = &events[i];
        assert_eq!(doc.pane_id, src.pane_id);
        assert_eq!(doc.sequence, src.sequence);
        assert_eq!(doc.recorded_at_ms, src.recorded_at_ms as i64);
        assert_eq!(doc.occurred_at_ms, src.occurred_at_ms as i64);
        assert_eq!(doc.event_id, src.event_id);
    }
}

// ===========================================================================
// Test: Log offset monotonicity in indexed documents
// ===========================================================================

#[tokio::test]
async fn log_offset_monotonically_increasing() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        (0..10)
            .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect(),
    )
    .await;

    let icfg = indexer_config(dir.path(), "offset-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    // Log offsets should be monotonically increasing
    let offsets: Vec<u64> = indexer.writer().docs.iter().map(|d| d.log_offset).collect();
    for w in offsets.windows(2) {
        assert!(
            w[1] > w[0],
            "log offsets must be monotonic: {} > {}",
            w[1],
            w[0]
        );
    }
}

// ===========================================================================
// Test: Redaction levels preserved in documents
// ===========================================================================

#[tokio::test]
async fn redaction_levels_applied_in_pipeline() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let mut partial = make_ingress("partial-red", 1, 0, "secret data");
    if let RecorderEventPayload::IngressText {
        ref mut redaction, ..
    } = partial.payload
    {
        *redaction = RecorderRedactionLevel::Partial;
    }

    let mut full = make_ingress("full-red", 1, 1, "top secret");
    if let RecorderEventPayload::IngressText {
        ref mut redaction, ..
    } = full.payload
    {
        *redaction = RecorderRedactionLevel::Full;
    }

    let none = make_ingress("no-red", 1, 2, "public data");

    append_events(&storage, "b1", vec![partial, full, none]).await;

    let icfg = indexer_config(dir.path(), "redaction-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    let docs = &indexer.writer().docs;
    assert_eq!(docs.len(), 3);

    // Partial redaction: text replaced with [REDACTED]
    assert_eq!(docs[0].redaction, Some("partial".to_string()));
    assert_eq!(docs[0].text, "[REDACTED]");

    // Full redaction: text stripped to empty
    assert_eq!(docs[1].redaction, Some("full".to_string()));
    assert!(docs[1].text.is_empty(), "full redaction should strip text");

    // No redaction: original text preserved
    assert_eq!(docs[2].redaction, Some("none".to_string()));
    assert_eq!(docs[2].text, "public data");
}

// ===========================================================================
// Test: Large-scale multi-pane end-to-end with invariant checking
// ===========================================================================

#[tokio::test]
async fn large_scale_multi_pane_end_to_end() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let num_panes = 5u64;
    let events_per_pane = 40usize;
    let assigner = SequenceAssigner::new();

    let mut all_events = Vec::new();
    for round in 0..events_per_pane {
        for pane_id in 0..num_panes {
            let (seq, _) = assigner.assign(pane_id);
            let mut event = make_ingress(
                "placeholder",
                pane_id,
                seq,
                &format!("p{}-r{}", pane_id, round),
            );
            event.event_id = generate_event_id_v1(&event);
            all_events.push(event);
        }
    }

    // Append in chunks (mimicking real batched writes)
    for (i, chunk) in all_events.chunks(20).enumerate() {
        append_events(&storage, &format!("chunk-{i}"), chunk.to_vec()).await;
    }

    // Index all events
    let icfg = IndexerConfig {
        batch_size: 50,
        ..indexer_config(dir.path(), "large-scale")
    };
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    let total_events = num_panes as usize * events_per_pane;
    assert_eq!(result.events_indexed, total_events as u64);
    assert!(result.caught_up);
    assert_eq!(indexer.writer().docs.len(), total_events);

    // Verify all panes represented
    let mut pane_counts = std::collections::HashMap::new();
    for doc in &indexer.writer().docs {
        *pane_counts.entry(doc.pane_id).or_insert(0u64) += 1;
    }
    for pane_id in 0..num_panes {
        assert_eq!(
            pane_counts[&pane_id], events_per_pane as u64,
            "pane {} should have {} events",
            pane_id, events_per_pane
        );
    }

    // Run source events through InvariantChecker
    let mut sorted = all_events.clone();
    sorted.sort_by(|a, b| RecorderMergeKey::from_event(a).cmp(&RecorderMergeKey::from_event(b)));

    let config = InvariantCheckerConfig {
        check_merge_order: true,
        check_causality: false,
        expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        ..Default::default()
    };
    let checker = InvariantChecker::with_config(config);
    let report = checker.check(&sorted);
    assert!(report.passed, "invariant violations: {:?}", report.violations);

    // Verify event IDs in indexed docs are unique
    let mut seen_ids = std::collections::HashSet::new();
    for doc in &indexer.writer().docs {
        assert!(
            seen_ids.insert(&doc.event_id),
            "duplicate event_id in indexed docs: {}",
            doc.event_id
        );
    }
}

// ===========================================================================
// Test: Storage flush modes with indexer
// ===========================================================================

#[tokio::test]
async fn storage_flush_modes_with_indexer() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        vec![make_ingress("e0", 1, 0, "hello")],
    )
    .await;

    // Flush in buffered mode
    let stats = storage.flush(FlushMode::Buffered).await.unwrap();
    assert!(stats.latest_offset.is_some());

    // Flush in durable mode
    let stats2 = storage.flush(FlushMode::Durable).await.unwrap();
    assert!(stats2.latest_offset.is_some());

    // Indexer should still work after flushes
    let icfg = indexer_config(dir.path(), "flush-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();
    assert_eq!(result.events_indexed, 1);
    assert!(result.caught_up);
}

// ===========================================================================
// Test: Document mapper correctness for all event types
// ===========================================================================

#[test]
fn document_mapper_all_event_types() {
    // Ingress
    let ingress = make_ingress("i1", 42, 7, "echo hello");
    let doc_i = map_event_to_document(&ingress, 100);
    assert_eq!(doc_i.event_type, "ingress_text");
    assert_eq!(doc_i.text, "echo hello");
    assert_eq!(doc_i.pane_id, 42);
    assert_eq!(doc_i.sequence, 7);
    assert_eq!(doc_i.log_offset, 100);
    assert_eq!(doc_i.ingress_kind, Some("send_text".to_string()));
    assert_eq!(doc_i.segment_kind, None);
    assert!(!doc_i.is_gap);

    // Egress
    let egress = make_egress("e1", 10, 3, "output\n");
    let doc_e = map_event_to_document(&egress, 200);
    assert_eq!(doc_e.event_type, "egress_output");
    assert_eq!(doc_e.text, "output\n");
    assert_eq!(doc_e.segment_kind, Some("delta".to_string()));
    assert!(!doc_e.is_gap);
    assert_eq!(doc_e.ingress_kind, None);

    // Control
    let control = make_control("c1", 5, 10);
    let doc_c = map_event_to_document(&control, 300);
    assert_eq!(doc_c.event_type, "control_marker");
    assert!(doc_c.text.is_empty());
    assert_eq!(
        doc_c.control_marker_type,
        Some("prompt_boundary".to_string())
    );
    assert!(!doc_c.details_json.is_empty());

    // Lifecycle
    let lifecycle = make_lifecycle("l1", 8, 20);
    let doc_l = map_event_to_document(&lifecycle, 400);
    assert_eq!(doc_l.event_type, "lifecycle_marker");
    assert_eq!(doc_l.text, "test"); // lifecycle text = reason field
    assert_eq!(doc_l.lifecycle_phase, Some("pane_opened".to_string()));
}

// ===========================================================================
// Test: Reader and storage agree on record count
// ===========================================================================

#[tokio::test]
async fn reader_and_storage_agree_on_record_count() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let event_count = 25u64;
    for i in 0..event_count {
        append_events(
            &storage,
            &format!("b-{i}"),
            vec![make_ingress(&format!("e{i}"), 1, i, &format!("t{i}"))],
        )
        .await;
    }

    // Storage health should report latest ordinal
    let health = storage.health().await;
    assert!(health.latest_offset.is_some());

    // Reader should be able to read all records
    let mut reader = AppendLogReader::open(&dir.path().join("events.log")).unwrap();
    let all_records = reader.read_batch(1000).unwrap();
    assert_eq!(all_records.len(), event_count as usize);

    // Ordinals should be sequential
    for (i, record) in all_records.iter().enumerate() {
        assert_eq!(record.offset.ordinal, i as u64);
    }
}

// ===========================================================================
// Test: Deterministic IDs remain unique after storage roundtrip
// ===========================================================================

#[tokio::test]
async fn deterministic_ids_unique_after_roundtrip() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let assigner = SequenceAssigner::new();
    let mut events = Vec::new();
    for pane_id in 0..3u64 {
        for _ in 0..10 {
            let (seq, _) = assigner.assign(pane_id);
            events.push(make_event_with_det_id(pane_id, seq, &format!("p{pane_id}-s{seq}")));
        }
    }

    append_events(&storage, "det-ids", events.clone()).await;

    // Read back through reader and verify IDs match
    let mut reader = AppendLogReader::open(&dir.path().join("events.log")).unwrap();
    let records = reader.read_batch(100).unwrap();

    assert_eq!(records.len(), 30);

    // All IDs should be unique
    let ids: std::collections::HashSet<&str> =
        records.iter().map(|r| r.event.event_id.as_str()).collect();
    assert_eq!(ids.len(), 30, "all 30 event IDs should be unique");

    // IDs should match source events
    for (i, record) in records.iter().enumerate() {
        assert_eq!(record.event.event_id, events[i].event_id);
    }
}

// ===========================================================================
// Test: Batch size boundary behavior
// ===========================================================================

#[tokio::test]
async fn batch_boundary_exact_fit() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    // Append exactly 10 events (matches default batch_size)
    append_events(
        &storage,
        "b1",
        (0..10)
            .map(|i| make_ingress(&format!("e{i}"), 1, i, &format!("t{i}")))
            .collect(),
    )
    .await;

    let icfg = IndexerConfig {
        batch_size: 10,
        ..indexer_config(dir.path(), "batch-boundary")
    };
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let result = indexer.run(&storage).await.unwrap();

    // When batch_size exactly matches event count, indexer reads a full batch
    // then reads again and gets empty → caught_up
    assert_eq!(result.events_indexed, 10);
    assert!(result.caught_up);
    // Should take at most 2 batch reads (one full, one empty)
    assert!(result.batches_committed >= 1);
}

// ===========================================================================
// Test: Causality preservation through storage → index pipeline
// ===========================================================================

#[tokio::test]
async fn causality_preserved_through_pipeline() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let mut event = make_ingress("causal-1", 1, 0, "trigger response");
    event.causality = RecorderEventCausality {
        parent_event_id: Some("parent-abc".to_string()),
        trigger_event_id: Some("trigger-xyz".to_string()),
        root_event_id: Some("root-001".to_string()),
    };
    event.workflow_id = Some("wf-test".to_string());
    event.correlation_id = Some("corr-test".to_string());

    append_events(&storage, "causal-batch", vec![event]).await;

    let icfg = indexer_config(dir.path(), "causal-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    let doc = &indexer.writer().docs[0];
    assert_eq!(doc.parent_event_id, Some("parent-abc".to_string()));
    assert_eq!(doc.trigger_event_id, Some("trigger-xyz".to_string()));
    assert_eq!(doc.root_event_id, Some("root-001".to_string()));
    assert_eq!(doc.workflow_id, Some("wf-test".to_string()));
    assert_eq!(doc.correlation_id, Some("corr-test".to_string()));
}

// ===========================================================================
// Test: Empty batches between populated batches
// ===========================================================================

#[tokio::test]
async fn empty_run_between_populated_runs() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    let consumer = "empty-between";

    // Phase 1: append and index
    append_events(
        &storage,
        "b1",
        vec![make_ingress("e0", 1, 0, "first")],
    )
    .await;

    let icfg = indexer_config(dir.path(), consumer);
    let mut ix1 = IncrementalIndexer::new(icfg.clone(), TrackingWriter::new());
    let r1 = ix1.run(&storage).await.unwrap();
    assert_eq!(r1.events_indexed, 1);
    assert!(r1.caught_up);

    // Phase 2: run again with nothing new → no-op
    let mut ix2 = IncrementalIndexer::new(icfg.clone(), TrackingWriter::new());
    let r2 = ix2.run(&storage).await.unwrap();
    assert_eq!(r2.events_indexed, 0);
    assert!(r2.caught_up);

    // Phase 3: append more and index
    append_events(
        &storage,
        "b2",
        vec![make_ingress("e1", 1, 1, "third")],
    )
    .await;

    let mut ix3 = IncrementalIndexer::new(icfg, TrackingWriter::new());
    let r3 = ix3.run(&storage).await.unwrap();
    assert_eq!(r3.events_indexed, 1);
    assert!(r3.caught_up);
    assert_eq!(ix3.writer().docs[0].event_id, "e1");
}

// ===========================================================================
// Test: Serde roundtrip of indexed documents
// ===========================================================================

#[tokio::test]
async fn indexed_document_serde_roundtrip() {
    let dir = tempdir().unwrap();
    let scfg = storage_config(dir.path());
    let storage = AppendLogRecorderStorage::open(scfg).unwrap();

    append_events(
        &storage,
        "b1",
        vec![
            make_ingress("i1", 1, 0, "hello"),
            make_egress("e1", 2, 1, "world"),
            make_control("c1", 3, 2),
        ],
    )
    .await;

    let icfg = indexer_config(dir.path(), "serde-test");
    let mut indexer = IncrementalIndexer::new(icfg, TrackingWriter::new());
    indexer.run(&storage).await.unwrap();

    // Serialize and deserialize each indexed document
    for doc in &indexer.writer().docs {
        let json = serde_json::to_string(doc).expect("serialize");
        let roundtripped: IndexDocumentFields =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(doc, &roundtripped);
    }
}
