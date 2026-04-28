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
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointConsumerId,
    CursorRecord, DurabilityLevel, EventCursorError, RecorderBackendKind, RecorderCheckpoint,
    RecorderEventCursor, RecorderEventReader, RecorderOffset, RecorderStorage,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use tempfile::{TempDir, tempdir};

// ===========================================================================
// Test helpers: event reader + real append-log storage fixture
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

struct AppendLogStorageFixture {
    _dir: TempDir,
    config: AppendLogStorageConfig,
    storage: AppendLogRecorderStorage,
}

impl AppendLogStorageFixture {
    fn new() -> Self {
        Self::with_config(|_| {})
    }

    fn with_config(configure: impl FnOnce(&mut AppendLogStorageConfig)) -> Self {
        let dir = tempdir().unwrap();
        let mut config = AppendLogStorageConfig {
            data_path: dir.path().join("events.log"),
            state_path: dir.path().join("state.json"),
            queue_capacity: 4,
            max_batch_events: 16,
            max_batch_bytes: 128 * 1024,
            max_idempotency_entries: 8,
        };
        configure(&mut config);
        let storage = AppendLogRecorderStorage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }

    async fn append_records(&self, batch_id: &str, records: &[CursorRecord]) {
        if records.is_empty() {
            return;
        }

        self.storage
            .append_batch(AppendRequest {
                batch_id: batch_id.to_string(),
                events: records.iter().map(|record| record.event.clone()).collect(),
                required_durability: DurabilityLevel::Fsync,
                producer_ts_ms: 0,
            })
            .await
            .unwrap();
    }

    fn reader(&self) -> AppendLogFileReader {
        AppendLogFileReader {
            path: self.data_path(),
        }
    }

    fn data_path(&self) -> PathBuf {
        self.storage.append_log_data_path().unwrap().to_path_buf()
    }

    fn records(&self) -> Vec<CursorRecord> {
        read_append_log_records(&self.data_path(), 0).unwrap()
    }

    fn event_count(&self) -> usize {
        self.records().len()
    }
}

struct AppendLogFileReader {
    path: PathBuf,
}

struct AppendLogFileCursor {
    records: Vec<CursorRecord>,
    pos: usize,
}

impl RecorderEventCursor for AppendLogFileCursor {
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
            head_offset_for_records(&self.records)
        }
    }
}

impl RecorderEventReader for AppendLogFileReader {
    fn open_cursor(
        &self,
        from: RecorderOffset,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        Ok(Box::new(AppendLogFileCursor {
            records: read_append_log_records(&self.path, from.ordinal)?,
            pos: 0,
        }))
    }

    fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
        Ok(head_offset_for_records(&read_append_log_records(
            &self.path, 0,
        )?))
    }
}

fn read_append_log_records(
    path: &Path,
    from_ordinal: u64,
) -> std::result::Result<Vec<CursorRecord>, EventCursorError> {
    let mut file = File::open(path).map_err(|err| EventCursorError::Io(err.to_string()))?;
    let mut records = Vec::new();
    let mut byte_offset = 0_u64;
    let mut ordinal = 0_u64;

    loop {
        let mut len_buf = [0_u8; 4];
        match file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(EventCursorError::Io(err.to_string())),
        }

        let payload_len = u32::from_le_bytes(len_buf) as usize;
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|err| EventCursorError::Io(err.to_string()))?;

        let event = serde_json::from_slice::<RecorderEvent>(&payload).map_err(|err| {
            EventCursorError::Corrupt {
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset,
                    ordinal,
                },
                reason: err.to_string(),
            }
        })?;

        if ordinal >= from_ordinal {
            records.push(CursorRecord {
                event,
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset,
                    ordinal,
                },
            });
        }

        byte_offset += 4 + payload_len as u64;
        ordinal += 1;
    }

    Ok(records)
}

fn head_offset_for_records(records: &[CursorRecord]) -> RecorderOffset {
    records
        .last()
        .map(|record| RecorderOffset {
            segment_id: record.offset.segment_id,
            byte_offset: record.offset.byte_offset + 1,
            ordinal: record.offset.ordinal + 1,
        })
        .unwrap_or(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        })
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

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
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
        let source = AppendLogStorageFixture::new();
        source.append_records("seed-m0", &records).await;
        let reader = source.reader();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.m0_preflight(&source.storage, &reader).await.unwrap();

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
        let storage = AppendLogStorageFixture::with_config(|config| {
            config.max_batch_bytes = 1;
        });
        let failed_append = storage
            .storage
            .append_batch(AppendRequest {
                batch_id: "force-degraded-health".to_string(),
                events: vec![make_event(1, 0)],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 0,
            })
            .await;
        assert!(failed_append.is_err());
        assert!(storage.storage.health().await.degraded);
        let engine = MigrationEngine::new(MigrationConfig::default());

        let result = engine.m0_preflight(&storage.storage, &reader).await;
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
        let storage = AppendLogStorageFixture::new();
        let reader = storage.reader();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.m0_preflight(&storage.storage, &reader).await.unwrap();
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

        let target = AppendLogStorageFixture::new();
        engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await
            .unwrap();

        assert_eq!(target.event_count(), 3);
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

        let target = AppendLogStorageFixture::new();
        let result = engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await;
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

        let target = AppendLogStorageFixture::new();
        let result = engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await;
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

        let target = AppendLogStorageFixture::with_config(|config| {
            config.max_batch_bytes = 1;
        });

        let result = engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await;
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

        let target = AppendLogStorageFixture::new();
        engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await
            .unwrap();

        assert_eq!(target.event_count(), 3);
        let first_import_head = target.storage.health().await.latest_offset.unwrap();
        engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await
            .unwrap();
        let second_import_head = target.storage.health().await.latest_offset.unwrap();
        assert_eq!(second_import_head, first_import_head);
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

        let target = AppendLogStorageFixture::new();
        let result = engine
            .m2_import(&target.storage, &exported, &mut manifest)
            .await;
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
        let source = AppendLogStorageFixture::new();
        source
            .storage
            .commit_checkpoint(make_checkpoint("indexer", 3))
            .await
            .unwrap();
        source
            .storage
            .commit_checkpoint(make_checkpoint("auditor", 2))
            .await
            .unwrap();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
            .await
            .unwrap();

        assert_eq!(result.consumers_found, 2);
        assert_eq!(result.checkpoints_migrated, 2);
        assert_eq!(result.checkpoints_reset, 0);
        assert!(
            target
                .storage
                .read_checkpoint(&CheckpointConsumerId("indexer".to_string()))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            target
                .storage
                .read_checkpoint(&CheckpointConsumerId("auditor".to_string()))
                .await
                .unwrap()
                .is_some()
        );
    });
}

#[test]
fn test_m3_preserves_checkpoint_monotonicity() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let source = AppendLogStorageFixture::new();
        source
            .storage
            .commit_checkpoint(make_checkpoint("idx", 5))
            .await
            .unwrap();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
            .await
            .unwrap();
        assert_eq!(result.checkpoints_migrated, 1);

        let committed = target
            .storage
            .read_checkpoint(&CheckpointConsumerId("idx".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(committed.upto_offset.ordinal, 5);
    });
}

#[test]
fn test_m3_rejects_checkpoint_referencing_missing_ordinal() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        // Checkpoint at ordinal 20, but manifest only goes to 10 -> reset
        let source = AppendLogStorageFixture::new();
        source
            .storage
            .commit_checkpoint(make_checkpoint("stale", 20))
            .await
            .unwrap();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
            .await
            .unwrap();
        assert_eq!(result.checkpoints_reset, 1);
        assert_eq!(result.reset_consumers, vec!["stale"]);

        let committed = target
            .storage
            .read_checkpoint(&CheckpointConsumerId("stale".to_string()))
            .await
            .unwrap()
            .unwrap();
        // Reset to first_ordinal
        assert_eq!(committed.upto_offset.ordinal, 0);
    });
}

#[test]
fn test_m3_handles_zero_consumers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let source = AppendLogStorageFixture::new();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest::default();

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
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
        let source = AppendLogStorageFixture::new();
        source
            .storage
            .commit_checkpoint(make_checkpoint("head", 10))
            .await
            .unwrap();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
            .await
            .unwrap();
        assert_eq!(result.checkpoints_migrated, 1);
        assert_eq!(result.checkpoints_reset, 0);

        let committed = target
            .storage
            .read_checkpoint(&CheckpointConsumerId("head".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(committed.upto_offset.ordinal, 10);
    });
}

#[test]
fn test_m3_mixed_valid_and_stale_consumers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let source = AppendLogStorageFixture::new();
        source
            .storage
            .commit_checkpoint(make_checkpoint("good", 5))
            .await
            .unwrap();
        source
            .storage
            .commit_checkpoint(make_checkpoint("stale", 100))
            .await
            .unwrap();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
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
        let source = AppendLogStorageFixture::new();
        source
            .storage
            .commit_checkpoint(make_checkpoint("rej", 5))
            .await
            .unwrap();
        let target = AppendLogStorageFixture::new();
        target
            .storage
            .commit_checkpoint(make_checkpoint("rej", 10))
            .await
            .unwrap();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
            .await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("checkpoint"), "msg: {msg}");
    });
}

#[test]
fn test_m3_real_lag_omits_consumers_without_checkpoints() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let source = AppendLogStorageFixture::new();
        let target = AppendLogStorageFixture::new();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = MigrationManifest {
            first_ordinal: 0,
            last_ordinal: 10,
            ..Default::default()
        };

        let result = engine
            .m3_checkpoint_sync(&source.storage, &target.storage, &manifest)
            .await
            .unwrap();
        assert_eq!(result.consumers_found, 0);
        assert_eq!(result.checkpoints_migrated, 0);
        assert_eq!(result.checkpoints_reset, 0);
        assert!(
            target
                .storage
                .read_checkpoint(&CheckpointConsumerId("ghost".to_string()))
                .await
                .unwrap()
                .is_none()
        );
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
