//! Repeatable recovery drills for recorder storage/indexing incidents.
//!
//! Bead: wa-oegrb.7.4

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use tempfile::tempdir;

use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointConsumerId,
    DurabilityLevel, RecorderCheckpoint, RecorderStorage, RecorderStorageError,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use frankenterm_core::tantivy_ingest::{
    IncrementalIndexer, IndexCommitStats, IndexDocumentFields, IndexWriteError, IndexWriter,
    IndexerConfig, IndexerError, compute_indexer_lag,
};
use frankenterm_core::tantivy_reindex::{
    IndexLookup, IntegrityCheckConfig, IntegrityChecker, ReindexConfig, ReindexPipeline,
    ReindexableWriter,
};

fn emit_recovery_artifact(label: &str, value: serde_json::Value) {
    eprintln!("[ARTIFACT][recorder-recovery-drill] {label}={value}");
}

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

fn indexer_config(
    path: &Path,
    consumer_id: &str,
    batch_size: usize,
    max_batches: usize,
) -> IndexerConfig {
    IndexerConfig {
        data_path: path.join("events.log"),
        consumer_id: consumer_id.to_string(),
        batch_size,
        dedup_on_replay: true,
        max_batches,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    }
}

fn reindex_config(
    path: &Path,
    consumer_id: &str,
    batch_size: usize,
    max_batches: usize,
) -> ReindexConfig {
    ReindexConfig {
        data_path: path.join("events.log"),
        consumer_id: consumer_id.to_string(),
        batch_size,
        dedup_on_replay: true,
        clear_before_start: true,
        max_batches,
        expected_event_schema: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
    }
}

fn ingress_event(event_id: &str, pane_id: u64, sequence: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("recovery-drill".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RecoveryFlow,
        occurred_at_ms: 1_700_000_000_000 + sequence * 10,
        recorded_at_ms: 1_700_000_000_001 + sequence * 10,
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
            ingress_kind: RecorderIngressKind::WorkflowAction,
        },
    }
}

async fn append_events(storage: &AppendLogRecorderStorage, prefix: &str, count: u64) {
    for (chunk_index, chunk) in (0..count)
        .map(|i| ingress_event(&format!("{prefix}-{i}"), 1, i, &format!("payload-{i}")))
        .collect::<Vec<_>>()
        .chunks(4)
        .enumerate()
    {
        storage
            .append_batch(AppendRequest {
                batch_id: format!("{prefix}-batch-{chunk_index}"),
                events: chunk.to_vec(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1_700_000_000_000 + chunk_index as u64,
            })
            .await
            .unwrap();
    }
}

#[derive(Debug, Default)]
struct DrillWriter {
    docs: Vec<IndexDocumentFields>,
    deleted_ids: Vec<String>,
    commits: u64,
    fail_commit: bool,
}

impl DrillWriter {
    fn new() -> Self {
        Self::default()
    }

    fn failing_commit() -> Self {
        Self {
            fail_commit: true,
            ..Self::default()
        }
    }
}

impl IndexWriter for DrillWriter {
    fn add_document(&mut self, doc: &IndexDocumentFields) -> Result<(), IndexWriteError> {
        self.docs.push(doc.clone());
        Ok(())
    }

    fn commit(&mut self) -> Result<IndexCommitStats, IndexWriteError> {
        if self.fail_commit {
            return Err(IndexWriteError::CommitFailed {
                reason: "simulated crash before durable commit".to_string(),
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

impl ReindexableWriter for DrillWriter {
    fn clear_all(&mut self) -> Result<u64, IndexWriteError> {
        let count = self.docs.len() as u64;
        self.docs.clear();
        self.deleted_ids.clear();
        Ok(count)
    }
}

struct DrillLookup {
    offsets: HashMap<String, u64>,
    total: u64,
}

impl DrillLookup {
    fn from_docs(docs: &[IndexDocumentFields]) -> Self {
        let mut offsets = HashMap::new();
        for doc in docs {
            offsets.insert(doc.event_id.clone(), doc.log_offset);
        }
        Self {
            offsets,
            total: docs.len() as u64,
        }
    }
}

impl IndexLookup for DrillLookup {
    fn has_event_id(&self, event_id: &str) -> Result<bool, IndexWriteError> {
        Ok(self.offsets.contains_key(event_id))
    }

    fn get_log_offset(&self, event_id: &str) -> Result<Option<u64>, IndexWriteError> {
        Ok(self.offsets.get(event_id).copied())
    }

    fn count_total(&self) -> Result<u64, IndexWriteError> {
        Ok(self.total)
    }
}

#[tokio::test]
async fn recovery_drill_writer_crash_resume_replays_without_loss() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    append_events(&storage, "writer-crash", 12).await;

    let consumer = "drill-writer-crash";
    let cfg = indexer_config(dir.path(), consumer, 6, 0);

    let mut failing = IncrementalIndexer::new(cfg.clone(), DrillWriter::failing_commit());
    let err = failing.run(&storage).await.unwrap_err();
    assert!(matches!(
        err,
        IndexerError::IndexWrite(IndexWriteError::CommitFailed { .. })
    ));

    let checkpoint_after_failure = storage
        .read_checkpoint(&CheckpointConsumerId(consumer.to_string()))
        .await
        .unwrap();
    assert!(checkpoint_after_failure.is_none());

    let recovery_start = Instant::now();
    let mut recovered = IncrementalIndexer::new(cfg, DrillWriter::new());
    let result = recovered.run(&storage).await.unwrap();
    let recovery_ms = recovery_start.elapsed().as_millis() as u64;

    assert_eq!(result.events_indexed, 12);
    assert!(result.caught_up);

    let lag = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag.records_behind, 0);

    emit_recovery_artifact(
        "writer_crash_resume",
        serde_json::json!({
            "consumer": consumer,
            "events_indexed": result.events_indexed,
            "recovery_batches": result.batches_committed,
            "final_ordinal": result.final_ordinal,
            "recovery_ms": recovery_ms,
        }),
    );
}

#[tokio::test]
async fn recovery_drill_checkpoint_divergence_detected_then_resumed() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    append_events(&storage, "checkpoint-divergence", 9).await;

    let consumer = "drill-checkpoint-divergence";
    let first_cfg = indexer_config(dir.path(), consumer, 4, 1);
    let mut first = IncrementalIndexer::new(first_cfg, DrillWriter::new());
    let first_result = first.run(&storage).await.unwrap();
    assert_eq!(first_result.events_indexed, 4);
    assert_eq!(first_result.final_ordinal, Some(3));

    let cp = storage
        .read_checkpoint(&CheckpointConsumerId(consumer.to_string()))
        .await
        .unwrap()
        .unwrap();
    let regression = RecorderCheckpoint {
        consumer: cp.consumer.clone(),
        upto_offset: frankenterm_core::recorder_storage::RecorderOffset {
            ordinal: 1,
            ..cp.upto_offset.clone()
        },
        schema_version: cp.schema_version.clone(),
        committed_at_ms: cp.committed_at_ms + 1,
    };
    let divergence_err = storage.commit_checkpoint(regression).await.unwrap_err();
    assert!(matches!(
        divergence_err,
        RecorderStorageError::CheckpointRegression { .. }
    ));

    let recovery_start = Instant::now();
    let resume_cfg = indexer_config(dir.path(), consumer, 4, 0);
    let mut resumed = IncrementalIndexer::new(resume_cfg, DrillWriter::new());
    let resumed_result = resumed.run(&storage).await.unwrap();
    let recovery_ms = recovery_start.elapsed().as_millis() as u64;

    assert_eq!(resumed_result.events_indexed, 5);
    assert_eq!(resumed_result.final_ordinal, Some(8));
    assert!(resumed_result.caught_up);

    let lag = compute_indexer_lag(&storage, consumer).await.unwrap();
    assert_eq!(lag.records_behind, 0);

    emit_recovery_artifact(
        "checkpoint_divergence",
        serde_json::json!({
            "consumer": consumer,
            "divergence_detected": true,
            "recovery_events_indexed": resumed_result.events_indexed,
            "recovery_batches": resumed_result.batches_committed,
            "final_ordinal": resumed_result.final_ordinal,
            "recovery_ms": recovery_ms,
        }),
    );
}

#[tokio::test]
async fn recovery_drill_reindex_resume_integrity_consistent() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
    append_events(&storage, "reindex-drill", 16).await;

    let consumer = "drill-reindex-resume";
    let cfg_first = reindex_config(dir.path(), consumer, 5, 1);
    let mut pipeline_first = ReindexPipeline::new(DrillWriter::new());
    let first_progress = pipeline_first
        .full_reindex(&storage, &cfg_first)
        .await
        .unwrap();
    assert_eq!(first_progress.events_indexed, 5);
    assert!(!first_progress.caught_up);

    let writer_first = pipeline_first.into_writer();

    let recovery_start = Instant::now();
    let cfg_resume = reindex_config(dir.path(), consumer, 5, 0);
    let mut pipeline_resume = ReindexPipeline::new(DrillWriter::new());
    let resume_progress = pipeline_resume
        .full_reindex(&storage, &cfg_resume)
        .await
        .unwrap();
    let recovery_ms = recovery_start.elapsed().as_millis() as u64;

    assert_eq!(resume_progress.docs_cleared, 0);
    assert_eq!(resume_progress.events_indexed, 11);
    assert_eq!(resume_progress.current_ordinal, Some(15));
    assert!(resume_progress.caught_up);

    let mut combined_docs = writer_first.docs;
    combined_docs.extend(pipeline_resume.writer().docs.iter().cloned());
    assert_eq!(combined_docs.len(), 16);

    let lookup = DrillLookup::from_docs(&combined_docs);
    let integrity_cfg = IntegrityCheckConfig {
        data_path: dir.path().join("events.log"),
        ..IntegrityCheckConfig::default()
    };
    let integrity = IntegrityChecker::check(&lookup, &integrity_cfg).unwrap();
    assert!(integrity.is_consistent, "integrity report: {integrity:?}");
    assert_eq!(integrity.log_events_scanned, 16);
    assert_eq!(integrity.index_matches, 16);

    emit_recovery_artifact(
        "reindex_resume_integrity",
        serde_json::json!({
            "consumer": consumer,
            "first_events_indexed": first_progress.events_indexed,
            "resume_events_indexed": resume_progress.events_indexed,
            "resume_batches": resume_progress.batches_committed,
            "recovery_ms": recovery_ms,
            "integrity_consistent": integrity.is_consistent,
            "events_scanned": integrity.log_events_scanned,
        }),
    );
}
