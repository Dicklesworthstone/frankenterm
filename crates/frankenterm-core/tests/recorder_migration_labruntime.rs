//! LabRuntime-ported recorder migration tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `recorder_migration.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for the M0→M5 migration
//! pipeline tests.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::recorder_migration::{MigrationConfig, MigrationEngine, MigrationManifest};
use frankenterm_core::recorder_storage::{
    AppendRequest, AppendResponse, CheckpointCommitOutcome, CheckpointConsumerId, CursorRecord,
    DurabilityLevel, EventCursorError, FlushMode, FlushStats, RecorderBackendKind,
    RecorderCheckpoint, RecorderConsumerLag, RecorderEventCursor, RecorderEventReader,
    RecorderOffset, RecorderStorage, RecorderStorageError, RecorderStorageHealth,
    RecorderStorageLag,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// ===========================================================================
// Test helpers: mock reader + mock storage
// ===========================================================================

fn make_event(pane_id: u64, ordinal: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: format!("evt-{ordinal}"),
        pane_id,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: ordinal * 100,
        recorded_at_ms: ordinal * 100 + 1,
        sequence: ordinal,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: format!("text-{ordinal}"),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn make_cursor_record(pane_id: u64, ordinal: u64) -> CursorRecord {
    CursorRecord {
        event: make_event(pane_id, ordinal),
        offset: RecorderOffset {
            segment_id: 0,
            byte_offset: ordinal * 100,
            ordinal,
        },
    }
}

/// In-memory event reader for tests.
struct TestEventReader {
    records: Vec<CursorRecord>,
}

impl TestEventReader {
    fn new(records: Vec<CursorRecord>) -> Self {
        Self { records }
    }
}

struct TestCursor {
    records: Vec<CursorRecord>,
    pos: usize,
}

impl RecorderEventCursor for TestCursor {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> std::result::Result<Vec<CursorRecord>, EventCursorError> {
        let end = (self.pos + max).min(self.records.len());
        let batch: Vec<_> = self.records[self.pos..end].to_vec();
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

impl RecorderEventReader for TestEventReader {
    fn open_cursor(
        &self,
        from: RecorderOffset,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        let remaining: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.offset.ordinal >= from.ordinal)
            .cloned()
            .collect();
        Ok(Box::new(TestCursor {
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

/// Mock storage that records appended batches.
struct MockMigrationStorage {
    health: RecorderStorageHealth,
    appended: Mutex<Vec<AppendRequest>>,
    fail_append: AtomicBool,
}

impl MockMigrationStorage {
    fn healthy() -> Self {
        Self {
            health: RecorderStorageHealth {
                backend: RecorderBackendKind::FrankenSqlite,
                degraded: false,
                queue_depth: 0,
                queue_capacity: 100,
                latest_offset: None,
                last_error: None,
            },
            appended: Mutex::new(Vec::new()),
            fail_append: AtomicBool::new(false),
        }
    }

    fn degraded() -> Self {
        Self {
            health: RecorderStorageHealth {
                backend: RecorderBackendKind::AppendLog,
                degraded: true,
                queue_depth: 0,
                queue_capacity: 100,
                latest_offset: None,
                last_error: Some("disk full".to_string()),
            },
            appended: Mutex::new(Vec::new()),
            fail_append: AtomicBool::new(false),
        }
    }

    fn total_events_appended(&self) -> usize {
        self.appended
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.events.len())
            .sum()
    }
}

impl RecorderStorage for MockMigrationStorage {
    fn backend_kind(&self) -> RecorderBackendKind {
        self.health.backend
    }

    async fn append_batch(
        &self,
        req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        if self.fail_append.load(Ordering::Relaxed) {
            return Err(RecorderStorageError::QueueFull { capacity: 0 });
        }
        let count = req.events.len();
        let first_ord = 0_u64;
        let last_ord = count.saturating_sub(1) as u64;
        self.appended.lock().unwrap().push(req);
        Ok(AppendResponse {
            backend: self.health.backend,
            accepted_count: count,
            first_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: first_ord,
            },
            last_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: last_ord,
            },
            committed_durability: DurabilityLevel::Appended,
            committed_at_ms: 0,
        })
    }

    async fn flush(
        &self,
        _mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        Ok(FlushStats {
            backend: self.health.backend,
            flushed_at_ms: 0,
            latest_offset: None,
        })
    }

    async fn read_checkpoint(
        &self,
        _consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        Ok(None)
    }

    async fn commit_checkpoint(
        &self,
        _checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        Ok(CheckpointCommitOutcome::Advanced)
    }

    async fn health(&self) -> RecorderStorageHealth {
        self.health.clone()
    }

    async fn lag_metrics(
        &self,
    ) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        Ok(RecorderStorageLag {
            latest_offset: None,
            consumers: vec![],
        })
    }
}

/// Mock storage with configurable checkpoints and lag consumers.
struct MockCheckpointStorage {
    health: RecorderStorageHealth,
    checkpoints: Mutex<HashMap<String, RecorderCheckpoint>>,
    consumers: Vec<RecorderConsumerLag>,
    committed: Mutex<Vec<RecorderCheckpoint>>,
    reject_commit: AtomicBool,
}

impl MockCheckpointStorage {
    fn new(
        consumers: Vec<RecorderConsumerLag>,
        checkpoints: HashMap<String, RecorderCheckpoint>,
    ) -> Self {
        Self {
            health: RecorderStorageHealth {
                backend: RecorderBackendKind::AppendLog,
                degraded: false,
                queue_depth: 0,
                queue_capacity: 100,
                latest_offset: None,
                last_error: None,
            },
            checkpoints: Mutex::new(checkpoints),
            consumers,
            committed: Mutex::new(Vec::new()),
            reject_commit: AtomicBool::new(false),
        }
    }

    fn empty_target() -> Self {
        Self::new(vec![], HashMap::new())
    }
}

impl RecorderStorage for MockCheckpointStorage {
    fn backend_kind(&self) -> RecorderBackendKind {
        self.health.backend
    }

    async fn append_batch(
        &self,
        _req: AppendRequest,
    ) -> std::result::Result<AppendResponse, RecorderStorageError> {
        Ok(AppendResponse {
            backend: self.health.backend,
            accepted_count: 0,
            first_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            last_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            committed_durability: DurabilityLevel::Appended,
            committed_at_ms: 0,
        })
    }

    async fn flush(
        &self,
        _mode: FlushMode,
    ) -> std::result::Result<FlushStats, RecorderStorageError> {
        Ok(FlushStats {
            backend: self.health.backend,
            flushed_at_ms: 0,
            latest_offset: None,
        })
    }

    async fn read_checkpoint(
        &self,
        consumer: &CheckpointConsumerId,
    ) -> std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError> {
        Ok(self.checkpoints.lock().unwrap().get(&consumer.0).cloned())
    }

    async fn commit_checkpoint(
        &self,
        checkpoint: RecorderCheckpoint,
    ) -> std::result::Result<CheckpointCommitOutcome, RecorderStorageError> {
        if self.reject_commit.load(Ordering::Relaxed) {
            return Ok(CheckpointCommitOutcome::RejectedOutOfOrder);
        }
        self.committed.lock().unwrap().push(checkpoint);
        Ok(CheckpointCommitOutcome::Advanced)
    }

    async fn health(&self) -> RecorderStorageHealth {
        self.health.clone()
    }

    async fn lag_metrics(
        &self,
    ) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        Ok(RecorderStorageLag {
            latest_offset: None,
            consumers: self.consumers.clone(),
        })
    }
}

fn make_checkpoint(consumer: &str, ordinal: u64) -> RecorderCheckpoint {
    RecorderCheckpoint {
        consumer: CheckpointConsumerId(consumer.to_string()),
        upto_offset: RecorderOffset {
            segment_id: 0,
            byte_offset: ordinal * 100,
            ordinal,
        },
        schema_version: "ft.recorder.event.v1".to_string(),
        committed_at_ms: 1000,
    }
}

fn make_consumer_lag(consumer: &str, behind: u64) -> RecorderConsumerLag {
    RecorderConsumerLag {
        consumer: CheckpointConsumerId(consumer.to_string()),
        offsets_behind: behind,
    }
}

// ===========================================================================
// Section 1: M0 preflight tests
// ===========================================================================

#[test]
fn test_m0_captures_manifest_with_correct_counts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![
            make_cursor_record(1, 0),
            make_cursor_record(1, 1),
            make_cursor_record(2, 2),
            make_cursor_record(1, 3),
            make_cursor_record(3, 4),
        ];
        let reader = TestEventReader::new(records);
        let storage = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.m0_preflight(&storage, &reader).await.unwrap();

        assert_eq!(manifest.event_count, 5);
        assert_eq!(manifest.first_ordinal, 0);
        assert_eq!(manifest.last_ordinal, 4);
        assert_eq!(manifest.per_pane_counts.get(&1), Some(&3));
        assert_eq!(manifest.per_pane_counts.get(&2), Some(&1));
        assert_eq!(manifest.per_pane_counts.get(&3), Some(&1));
        assert!(manifest.last_offset.is_some());
    });
}

#[test]
fn test_m0_rejects_degraded_source() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let reader = TestEventReader::new(vec![]);
        let storage = MockMigrationStorage::degraded();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let result = engine.m0_preflight(&storage, &reader).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("degraded"),
            "error should mention degraded: {msg}"
        );
    });
}

#[test]
fn test_m0_empty_source_produces_zero_counts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let reader = TestEventReader::new(vec![]);
        let storage = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.m0_preflight(&storage, &reader).await.unwrap();
        assert_eq!(manifest.event_count, 0);
        assert_eq!(manifest.first_ordinal, 0);
        assert_eq!(manifest.last_ordinal, 0);
        assert!(manifest.per_pane_counts.is_empty());
    });
}

// ===========================================================================
// Section 2: M2 import tests
// ===========================================================================

#[test]
fn test_m2_imports_preserving_ordinals() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![
            make_cursor_record(1, 0),
            make_cursor_record(1, 1),
            make_cursor_record(2, 2),
        ];
        let engine = MigrationEngine::new(MigrationConfig {
            import_batch_size: 2,
            ..Default::default()
        });
        let mut manifest = MigrationManifest::default();

        let reader = TestEventReader::new(records.clone());
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        let target = MockMigrationStorage::healthy();
        engine
            .m2_import(&target, &exported, &mut manifest)
            .await
            .unwrap();

        assert_eq!(target.total_events_appended(), 3);
        assert_eq!(manifest.import_count, 3);
        assert_eq!(manifest.import_digest, manifest.export_digest);
    });
}

#[test]
fn test_m2_digest_match_passes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![make_cursor_record(1, 0), make_cursor_record(1, 1)];
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let reader = TestEventReader::new(records);
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        let target = MockMigrationStorage::healthy();
        let result = engine.m2_import(&target, &exported, &mut manifest).await;
        assert!(result.is_ok());
    });
}

#[test]
fn test_m2_digest_mismatch_aborts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![make_cursor_record(1, 0), make_cursor_record(1, 1)];
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let reader = TestEventReader::new(records);
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        // Tamper with the digest
        manifest.export_digest = 0xDEADBEEF;

        let target = MockMigrationStorage::healthy();
        let result = engine.m2_import(&target, &exported, &mut manifest).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("digest mismatch"), "error: {msg}");
    });
}

#[test]
fn test_m2_target_write_failure_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![make_cursor_record(1, 0)];
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let reader = TestEventReader::new(records);
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        let target = MockMigrationStorage::healthy();
        target.fail_append.store(true, Ordering::Relaxed);

        let result = engine.m2_import(&target, &exported, &mut manifest).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("target write error"), "error: {msg}");
    });
}

#[test]
fn test_m2_batch_ids_contain_ordinal_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![
            make_cursor_record(1, 10),
            make_cursor_record(1, 11),
            make_cursor_record(1, 12),
        ];
        let engine = MigrationEngine::new(MigrationConfig {
            import_batch_size: 2,
            consumer_id: "test-mig".to_string(),
            ..Default::default()
        });
        let mut manifest = MigrationManifest::default();
        let reader = TestEventReader::new(records);
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        let target = MockMigrationStorage::healthy();
        engine
            .m2_import(&target, &exported, &mut manifest)
            .await
            .unwrap();

        let appended = target.appended.lock().unwrap();
        // batch_size=2: [10,11] then [12]
        assert_eq!(appended.len(), 2);
        assert!(appended[0].batch_id.contains("10"));
        assert!(appended[0].batch_id.contains("11"));
        assert!(appended[1].batch_id.contains("12"));
    });
}

#[test]
fn test_m2_count_mismatch_detected() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![make_cursor_record(1, 0), make_cursor_record(1, 1)];
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let reader = TestEventReader::new(records);
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        // Tamper with export_count so count verification fails
        manifest.export_count = 999;

        let target = MockMigrationStorage::healthy();
        let result = engine.m2_import(&target, &exported, &mut manifest).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("count mismatch"), "msg: {msg}");
    });
}

// ===========================================================================
// Section 3: End-to-end M0->M2 pipeline
// ===========================================================================

#[test]
fn test_m0_m2_pipeline_end_to_end() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![
            make_cursor_record(1, 0),
            make_cursor_record(1, 1),
            make_cursor_record(2, 2),
            make_cursor_record(3, 3),
            make_cursor_record(1, 4),
        ];
        let reader = TestEventReader::new(records);
        let source = MockMigrationStorage::healthy();
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig {
            export_batch_size: 2,
            import_batch_size: 3,
            consumer_id: "test-migration".to_string(),
        });

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 5);
        assert_eq!(manifest.first_ordinal, 0);
        assert_eq!(manifest.last_ordinal, 4);
        assert_eq!(manifest.export_count, 5);
        assert_eq!(manifest.import_count, 5);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        assert_eq!(target.total_events_appended(), 5);
        assert_eq!(manifest.per_pane_counts.get(&1), Some(&3));
        assert_eq!(manifest.per_pane_counts.get(&2), Some(&1));
        assert_eq!(manifest.per_pane_counts.get(&3), Some(&1));
    });
}

#[test]
fn test_m0_m2_with_batch_size_one() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let records = vec![
            make_cursor_record(1, 0),
            make_cursor_record(2, 1),
            make_cursor_record(3, 2),
        ];
        let reader = TestEventReader::new(records);
        let source = MockMigrationStorage::healthy();
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig {
            export_batch_size: 1,
            import_batch_size: 1,
            ..Default::default()
        });

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 3);
        assert_eq!(manifest.export_count, 3);
        assert_eq!(manifest.import_count, 3);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        // batch_size=1 means 3 separate append calls
        assert_eq!(target.appended.lock().unwrap().len(), 3);
    });
}

#[test]
fn test_m0_m2_pipeline_empty_source() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let reader = TestEventReader::new(vec![]);
        let source = MockMigrationStorage::healthy();
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 0);
        assert_eq!(manifest.export_count, 0);
        assert_eq!(manifest.import_count, 0);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        assert_eq!(target.total_events_appended(), 0);
    });
}

// ===========================================================================
// Section 4: M3 checkpoint sync tests
// ===========================================================================

#[test]
fn test_m3_migrates_all_consumer_checkpoints() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let consumers = vec![
            make_consumer_lag("indexer", 5),
            make_consumer_lag("auditor", 10),
        ];
        let mut checkpoints = HashMap::new();
        checkpoints.insert("indexer".to_string(), make_checkpoint("indexer", 3));
        checkpoints.insert("auditor".to_string(), make_checkpoint("auditor", 2));

        let source = MockCheckpointStorage::new(consumers, checkpoints);
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();

        assert_eq!(result.consumers_found, 2);
        assert_eq!(result.checkpoints_migrated, 2);
        assert_eq!(result.checkpoints_reset, 0);
        assert_eq!(target.committed.lock().unwrap().len(), 2);
    });
}

#[test]
fn test_m3_preserves_checkpoint_monotonicity() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let consumers = vec![make_consumer_lag("idx", 0)];
        let mut checkpoints = HashMap::new();
        checkpoints.insert("idx".to_string(), make_checkpoint("idx", 5));

        let source = MockCheckpointStorage::new(consumers, checkpoints);
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(result.checkpoints_migrated, 1);

        let committed = target.committed.lock().unwrap();
        assert_eq!(committed[0].upto_offset.ordinal, 5);
    });
}

#[test]
fn test_m3_rejects_checkpoint_referencing_missing_ordinal() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Checkpoint at ordinal 20, but manifest only goes to 10 -> reset
        let consumers = vec![make_consumer_lag("stale", 0)];
        let mut checkpoints = HashMap::new();
        checkpoints.insert("stale".to_string(), make_checkpoint("stale", 20));

        let source = MockCheckpointStorage::new(consumers, checkpoints);
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(result.checkpoints_reset, 1);
        assert_eq!(result.reset_consumers, vec!["stale"]);

        let committed = target.committed.lock().unwrap();
        // Reset to first_ordinal
        assert_eq!(committed[0].upto_offset.ordinal, 0);
    });
}

#[test]
fn test_m3_handles_zero_consumers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let source = MockCheckpointStorage::new(vec![], HashMap::new());
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest::default();

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(result.consumers_found, 0);
        assert_eq!(result.checkpoints_migrated, 0);
        assert_eq!(result.checkpoints_reset, 0);
    });
}

#[test]
fn test_m3_handles_consumer_at_head_offset() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Checkpoint at exactly last_ordinal -- should pass without reset
        let consumers = vec![make_consumer_lag("head", 0)];
        let mut checkpoints = HashMap::new();
        checkpoints.insert("head".to_string(), make_checkpoint("head", 10));

        let source = MockCheckpointStorage::new(consumers, checkpoints);
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(result.checkpoints_migrated, 1);
        assert_eq!(result.checkpoints_reset, 0);

        let committed = target.committed.lock().unwrap();
        assert_eq!(committed[0].upto_offset.ordinal, 10);
    });
}

#[test]
fn test_m3_mixed_valid_and_stale_consumers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let consumers = vec![make_consumer_lag("good", 2), make_consumer_lag("stale", 0)];
        let mut checkpoints = HashMap::new();
        checkpoints.insert("good".to_string(), make_checkpoint("good", 5));
        checkpoints.insert("stale".to_string(), make_checkpoint("stale", 100));

        let source = MockCheckpointStorage::new(consumers, checkpoints);
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(result.consumers_found, 2);
        assert_eq!(result.checkpoints_migrated, 2);
        assert_eq!(result.checkpoints_reset, 1);
        assert_eq!(result.reset_consumers, vec!["stale"]);
    });
}

#[test]
fn test_m3_target_rejects_out_of_order() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let consumers = vec![make_consumer_lag("rej", 0)];
        let mut checkpoints = HashMap::new();
        checkpoints.insert("rej".to_string(), make_checkpoint("rej", 5));

        let source = MockCheckpointStorage::new(consumers, checkpoints);
        let target = MockCheckpointStorage::empty_target();
        target.reject_commit.store(true, Ordering::Relaxed);
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine.m3_checkpoint_sync(&source, &target, &manifest).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("rejected"), "msg: {msg}");
    });
}

#[test]
fn test_m3_consumer_without_checkpoint_skipped() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Consumer appears in lag_metrics but has no checkpoint stored
        let consumers = vec![make_consumer_lag("ghost", 0)];
        let source = MockCheckpointStorage::new(consumers, HashMap::new());
        let target = MockCheckpointStorage::empty_target();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(result.consumers_found, 1);
        assert_eq!(result.checkpoints_migrated, 0);
        assert_eq!(result.checkpoints_reset, 0);
        assert!(target.committed.lock().unwrap().is_empty());
    });
}

// ===========================================================================
// Section 5: M5 cutover tests
// ===========================================================================

#[test]
fn test_m5_emits_lifecycle_marker() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = MigrationManifest {
            event_count: 100,
            first_ordinal: 0,
            last_ordinal: 99,
            export_digest: 0xCAFE,
            ..Default::default()
        };

        let result = engine
            .m5_cutover(&target, &manifest, 1708000000, None)
            .await
            .unwrap();

        assert_eq!(result.activated_backend, RecorderBackendKind::FrankenSqlite);
        assert_eq!(result.migration_epoch_ms, 1708000000);
        assert!(result.target_healthy);
        assert!(result.source_retained_path.is_none());

        // Verify one batch was appended (the marker event)
        let appended = target.appended.lock().unwrap();
        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].events.len(), 1);

        let marker = &appended[0].events[0];
        assert!(marker.event_id.contains("cutover"));
        assert_eq!(marker.sequence, 100); // last_ordinal + 1
    });
}

#[test]
fn test_m5_switches_backend_selector() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = MigrationManifest::default();

        let result = engine
            .m5_cutover(&target, &manifest, 1000, None)
            .await
            .unwrap();

        // Activation result always indicates FrankenSqlite
        assert_eq!(result.activated_backend, RecorderBackendKind::FrankenSqlite);
    });
}

#[test]
fn test_m5_preserves_source_files() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = MigrationManifest::default();

        let result = engine
            .m5_cutover(
                &target,
                &manifest,
                1000,
                Some("/data/events.log".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(
            result.source_retained_path,
            Some("/data/events.log".to_string())
        );
    });
}

#[test]
fn test_m5_verifies_target_health_post_activation() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Use a degraded target
        let target = MockMigrationStorage::degraded();
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = MigrationManifest::default();

        let result = engine
            .m5_cutover(&target, &manifest, 1000, None)
            .await
            .unwrap();

        // Degraded target reports unhealthy
        assert!(!result.target_healthy);
    });
}

#[test]
fn test_m5_write_failure_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let target = MockMigrationStorage::healthy();
        target.fail_append.store(true, Ordering::Relaxed);
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = MigrationManifest::default();

        let result = engine.m5_cutover(&target, &manifest, 1000, None).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("target write error"), "msg: {msg}");
    });
}

#[test]
fn test_m5_marker_batch_uses_fsync_durability() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = MigrationManifest::default();

        engine
            .m5_cutover(&target, &manifest, 1000, None)
            .await
            .unwrap();

        let appended = target.appended.lock().unwrap();
        assert_eq!(appended[0].required_durability, DurabilityLevel::Fsync);
    });
}
