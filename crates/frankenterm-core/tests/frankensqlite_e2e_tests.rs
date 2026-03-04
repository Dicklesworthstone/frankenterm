//! E4.F1.T2: End-to-end migration cutover and rollback scenario harness.
//!
//! Tests the full M0→M5 pipeline and rollback tiers using AppendLog source
//! and mock target storage.

use frankenterm_core::recorder_migration::{MigrationConfig, MigrationEngine, MigrationManifest};
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, AppendResponse,
    CheckpointCommitOutcome, CheckpointConsumerId, CursorRecord, DurabilityLevel, EventCursorError,
    FlushMode, FlushStats, RecorderBackendKind, RecorderCheckpoint, RecorderEventCursor,
    RecorderEventReader, RecorderOffset, RecorderStorage, RecorderStorageError,
    RecorderStorageHealth, RecorderStorageLag,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use frankenterm_core::storage::{
    MigrationForensicBackendState, MigrationForensicCaptureContext,
    MigrationForensicCorruptionDetail, MigrationForensicMigrationCheckpoint,
    MigrationRollbackClass, MigrationRollbackClassifierConfig, MigrationRollbackClassifierInput,
    MigrationRollbackExecutionState, MigrationRollbackPlaybookContext, MigrationStage,
    classify_migration_rollback_trigger, execute_migration_rollback_playbook,
};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::tempdir;

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    let runtime = frankenterm_core::runtime_compat::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_append_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 64,
        max_batch_events: 256,
        max_batch_bytes: 512 * 1024,
        max_idempotency_entries: 128,
    }
}

fn sample_event(pane_id: u64, sequence: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: format!("e2e-{sequence}"),
        pane_id,
        session_id: Some("e2e-session".to_string()),
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
            text: format!("text-{sequence}"),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

/// Populate an AppendLog source with N events and return cursor records.
async fn populate_source(storage: &AppendLogRecorderStorage, count: u64) -> Vec<CursorRecord> {
    let batch_size = 50u64;
    let mut sequence = 0u64;
    let mut all_records = Vec::new();

    while sequence < count {
        let end = (sequence + batch_size).min(count);
        let events: Vec<_> = (sequence..end)
            .map(|i| sample_event(i % 3 + 1, i))
            .collect();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: format!("populate-{sequence}"),
                events: events.clone(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        // Build CursorRecords with the correct ordinals from the response
        let first_ord = resp.first_offset.ordinal;
        for (i, event) in events.into_iter().enumerate() {
            let ordinal = first_ord + i as u64;
            all_records.push(CursorRecord {
                event,
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: ordinal * 100, // approximate
                    ordinal,
                },
            });
        }

        sequence = end;
    }
    all_records
}

/// In-memory event reader wrapping pre-loaded records.
struct MemoryReader {
    records: Vec<CursorRecord>,
}

struct MemoryCursor {
    records: Vec<CursorRecord>,
    pos: usize,
}

impl RecorderEventCursor for MemoryCursor {
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

impl RecorderEventReader for MemoryReader {
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
        Ok(Box::new(MemoryCursor {
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

// ---------------------------------------------------------------------------
// Mock target storage
// ---------------------------------------------------------------------------

struct MockTargetStorage {
    health: RecorderStorageHealth,
    appended: Mutex<Vec<AppendRequest>>,
    checkpoints: Mutex<std::collections::HashMap<String, RecorderCheckpoint>>,
    fail_append: AtomicBool,
    degraded_post_cutover: AtomicBool,
}

impl MockTargetStorage {
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
            checkpoints: Mutex::new(std::collections::HashMap::new()),
            fail_append: AtomicBool::new(false),
            degraded_post_cutover: AtomicBool::new(false),
        }
    }

    fn total_events(&self) -> usize {
        self.appended
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.events.len())
            .sum()
    }
}

impl RecorderStorage for MockTargetStorage {
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
        self.appended.lock().unwrap().push(req);
        Ok(AppendResponse {
            backend: self.health.backend,
            accepted_count: count,
            first_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            last_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: count.saturating_sub(1) as u64,
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
        self.checkpoints
            .lock()
            .unwrap()
            .insert(checkpoint.consumer.0.clone(), checkpoint);
        Ok(CheckpointCommitOutcome::Advanced)
    }

    async fn health(&self) -> RecorderStorageHealth {
        if self.degraded_post_cutover.load(Ordering::Relaxed) {
            RecorderStorageHealth {
                degraded: true,
                last_error: Some("post-cutover degradation".to_string()),
                ..self.health.clone()
            }
        } else {
            self.health.clone()
        }
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        Ok(RecorderStorageLag {
            latest_offset: None,
            consumers: vec![],
        })
    }
}

// ===========================================================================
// E2E scenarios
// ===========================================================================

#[test]
fn test_e2e_full_migration_happy_path() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 100).await;
        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig {
            export_batch_size: 25,
            import_batch_size: 30,
            consumer_id: "e2e-test".to_string(),
        });

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 100);
        assert_eq!(manifest.export_count, 100);
        assert_eq!(manifest.import_count, 100);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        assert_eq!(target.total_events(), 100);
    });
}

#[test]
fn test_e2e_m5_cutover_completes() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 10).await;
        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        let cutover = engine
            .m5_cutover(
                &target,
                &manifest,
                1708000000,
                Some(dir.path().to_string_lossy().to_string()),
            )
            .await
            .unwrap();

        assert_eq!(
            cutover.activated_backend,
            RecorderBackendKind::FrankenSqlite
        );
        assert!(cutover.target_healthy);
        assert!(cutover.source_retained_path.is_some());
    });
}

#[test]
fn test_e2e_m2_digest_mismatch_triggers_immediate_rollback() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Import,
            import_digest_mismatch: true,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::Immediate)
        );
    });
}

#[test]
fn test_e2e_m5_health_failure_triggers_postcutover_rollback() {
    run_async_test(async {
        // projection_lag_breach requires sustained_slo_windows consecutive breaches
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Activate,
            projection_lag_breach: true,
            consecutive_slo_breach_windows: 3,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::PostCutover)
        );
    });
}

#[test]
fn test_e2e_corruption_triggers_immediate_rollback() {
    run_async_test(async {
        // corrupt_import is an immediate-tier trigger (not emergency)
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Import,
            corrupt_import: true,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::Immediate)
        );
    });
}

#[test]
fn test_e2e_rollback_preserves_source_data() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let _records = populate_source(&source, 20).await;

        let health = source.health().await;
        assert!(!health.degraded);
        assert!(health.latest_offset.is_some());
        assert_eq!(health.latest_offset.unwrap().ordinal, 19);
    });
}

#[test]
fn test_e2e_immediate_rollback_playbook() {
    run_async_test(async {
        let dir = tempdir().unwrap();

        let context = MigrationRollbackPlaybookContext {
            rollback_class: MigrationRollbackClass::Immediate,
            from_stage: MigrationStage::Import,
            pre_migration_checkpoints: BTreeMap::new(),
            forensic_capture: None,
            forensics_output_dir: dir.path().to_path_buf(),
        };

        let mut state = MigrationRollbackExecutionState::default();
        let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();

        assert_eq!(report.tier, MigrationRollbackClass::Immediate);
        assert!(!report.migration_active);
        assert_eq!(report.backend_selector, RecorderBackendKind::AppendLog);
        assert!(report.target_cleared);
    });
}

#[test]
fn test_e2e_postcutover_rollback_playbook() {
    run_async_test(async {
        let dir = tempdir().unwrap();

        let context = MigrationRollbackPlaybookContext {
            rollback_class: MigrationRollbackClass::PostCutover,
            from_stage: MigrationStage::Activate,
            pre_migration_checkpoints: BTreeMap::new(),
            forensic_capture: None,
            forensics_output_dir: dir.path().to_path_buf(),
        };

        let mut state = MigrationRollbackExecutionState::default();
        let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();

        assert_eq!(report.tier, MigrationRollbackClass::PostCutover);
        assert_eq!(report.backend_selector, RecorderBackendKind::AppendLog);
        assert!(report.projection_rebuild_triggered);
    });
}

#[test]
fn test_e2e_target_write_failure_at_m2() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 10).await;
        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        target.fail_append.store(true, Ordering::Relaxed);
        let engine = MigrationEngine::new(MigrationConfig::default());

        let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        let result = engine.m2_import(&target, &exported, &mut manifest).await;
        assert!(result.is_err());
    });
}

#[test]
fn test_e2e_m5_degraded_target_reports_unhealthy() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 5).await;
        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        target.degraded_post_cutover.store(true, Ordering::Relaxed);

        let cutover = engine
            .m5_cutover(&target, &manifest, 1000, None)
            .await
            .unwrap();

        assert!(!cutover.target_healthy);
    });
}

#[test]
fn test_e2e_checkpoint_monotonicity_across_cutover() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 20).await;

        let consumer = CheckpointConsumerId("e2e-consumer".to_string());
        source
            .commit_checkpoint(RecorderCheckpoint {
                consumer: consumer.clone(),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 10,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 1000,
            })
            .await
            .unwrap();

        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        let sync_result = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();

        assert_eq!(sync_result.consumers_found, 1);
        assert_eq!(sync_result.checkpoints_migrated, 1);

        let target_cp = target.read_checkpoint(&consumer).await.unwrap().unwrap();
        assert_eq!(target_cp.upto_offset.ordinal, 10);
    });
}

#[test]
fn test_e2e_manifest_captures_per_pane_distribution() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 30).await;

        let reader = MemoryReader { records };
        let engine = MigrationEngine::new(MigrationConfig::default());
        let manifest = engine.m0_preflight(&source, &reader).await.unwrap();

        assert_eq!(manifest.per_pane_counts.len(), 3);
        assert_eq!(manifest.per_pane_counts.get(&1), Some(&10));
        assert_eq!(manifest.per_pane_counts.get(&2), Some(&10));
        assert_eq!(manifest.per_pane_counts.get(&3), Some(&10));
    });
}

#[test]
fn test_e2e_empty_source_migration() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 0).await;

        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 0);
        assert_eq!(manifest.export_count, 0);
        assert_eq!(manifest.import_count, 0);
        assert_eq!(target.total_events(), 0);
    });
}

#[test]
fn test_e2e_large_dataset_200_events() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 200).await;

        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig {
            export_batch_size: 50,
            import_batch_size: 75,
            ..Default::default()
        });

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 200);
        assert_eq!(manifest.export_count, 200);
        assert_eq!(manifest.import_count, 200);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        assert_eq!(target.total_events(), 200);
    });
}

#[test]
fn test_e2e_no_rollback_when_all_healthy() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Activate,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(!decision.should_rollback);
        assert!(decision.rollback_class.is_none());
    });
}

#[test]
fn test_e2e_cardinality_mismatch_is_immediate() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Import,
            event_cardinality_mismatch: true,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::Immediate)
        );
    });
}

#[test]
fn test_e2e_checkpoint_regression_is_immediate() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::CheckpointSync,
            checkpoint_regression: true,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::Immediate)
        );
    });
}

#[test]
fn test_e2e_data_loss_is_data_integrity_emergency() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Activate,
            confirmed_canonical_data_loss: true,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::DataIntegrityEmergency)
        );
    });
}

#[test]
fn test_e2e_full_pipeline_with_m3_and_m5() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 50).await;

        source
            .commit_checkpoint(RecorderCheckpoint {
                consumer: CheckpointConsumerId("indexer".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 25,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 1000,
            })
            .await
            .unwrap();

        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();
        assert_eq!(manifest.export_count, 50);

        let sync = engine
            .m3_checkpoint_sync(&source, &target, &manifest)
            .await
            .unwrap();
        assert_eq!(sync.checkpoints_migrated, 1);

        let cutover = engine
            .m5_cutover(
                &target,
                &manifest,
                1708000000,
                source
                    .append_log_data_path()
                    .map(|p| p.to_string_lossy().to_string()),
            )
            .await
            .unwrap();
        assert!(cutover.target_healthy);
        assert_eq!(
            cutover.activated_backend,
            RecorderBackendKind::FrankenSqlite
        );
    });
}

#[test]
fn test_e2e_source_health_still_good_after_export() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 30).await;

        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let _manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        let health = source.health().await;
        assert!(!health.degraded);
    });
}

#[test]
fn test_e2e_manifest_digest_matches_re_export() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 25).await;

        let reader = MemoryReader {
            records: records.clone(),
        };
        let engine = MigrationEngine::new(MigrationConfig::default());

        let mut m1 = MigrationManifest::default();
        engine.m1_export(&reader, &mut m1).unwrap();

        let reader2 = MemoryReader { records };
        let mut m2 = MigrationManifest::default();
        engine.m1_export(&reader2, &mut m2).unwrap();

        assert_eq!(m1.export_digest, m2.export_digest);
        assert_eq!(m1.export_count, m2.export_count);
    });
}

#[test]
fn test_e2e_rollback_execution_state_default() {
    let state = MigrationRollbackExecutionState::default();
    assert!(!state.recorder_writes_blocked());
}

#[test]
fn test_e2e_rollback_class_as_str() {
    assert_eq!(MigrationRollbackClass::Immediate.as_str(), "immediate");
    assert_eq!(MigrationRollbackClass::PostCutover.as_str(), "post_cutover");
    assert_eq!(
        MigrationRollbackClass::DataIntegrityEmergency.as_str(),
        "data_integrity_emergency"
    );
}

#[test]
fn test_e2e_single_event_migration() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 1).await;

        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.event_count, 1);
        assert_eq!(manifest.first_ordinal, 0);
        assert_eq!(manifest.last_ordinal, 0);
        assert_eq!(target.total_events(), 1);
    });
}

#[test]
fn test_e2e_suspected_corruption_triggers_emergency() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Import,
            suspected_canonical_corruption: true,
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
        assert_eq!(
            decision.rollback_class,
            Some(MigrationRollbackClass::DataIntegrityEmergency)
        );
    });
}

#[test]
fn test_e2e_repeated_write_failures_trigger_postcutover() {
    run_async_test(async {
        let input = MigrationRollbackClassifierInput {
            stage: MigrationStage::Soak,
            high_severity_write_failures: 5,
            config: MigrationRollbackClassifierConfig {
                repeated_write_failure_threshold: 3,
                ..Default::default()
            },
            ..Default::default()
        };

        let decision = classify_migration_rollback_trigger(&input);
        assert!(decision.should_rollback);
    });
}

#[test]
fn test_e2e_export_batch_size_respected() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 40).await;
        let reader = MemoryReader { records };
        let engine = MigrationEngine::new(MigrationConfig {
            export_batch_size: 10,
            ..Default::default()
        });

        let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();
        assert_eq!(exported.len(), 40);
        assert_eq!(manifest.export_count, 40);
    });
}

#[test]
fn test_e2e_import_batch_size_splits_correctly() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 15).await;
        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig {
            import_batch_size: 5,
            ..Default::default()
        });

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        assert_eq!(manifest.import_count, 15);
        // With batch size 5, 15 events → 3 batches
        assert!(target.appended.lock().unwrap().len() >= 3);
    });
}

#[test]
fn test_e2e_manifest_serde_roundtrip() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let source = AppendLogRecorderStorage::open(test_append_config(dir.path())).unwrap();
        let records = populate_source(&source, 10).await;
        let reader = MemoryReader { records };
        let target = MockTargetStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();

        let json = serde_json::to_string(&manifest).unwrap();
        let roundtripped: MigrationManifest = serde_json::from_str(&json).unwrap();

        assert_eq!(roundtripped.event_count, manifest.event_count);
        assert_eq!(roundtripped.export_digest, manifest.export_digest);
        assert_eq!(roundtripped.import_digest, manifest.import_digest);
        assert_eq!(roundtripped.per_pane_counts, manifest.per_pane_counts);
    });
}

#[test]
fn test_e2e_data_integrity_freeze_blocks_writes() {
    run_async_test(async {
        let dir = tempdir().unwrap();
        let forensic_capture = MigrationForensicCaptureContext {
            source_state: MigrationForensicBackendState {
                health: true,
                head_offset: None,
                last_checkpoint: None,
            },
            target_state: MigrationForensicBackendState {
                health: false,
                head_offset: None,
                last_checkpoint: None,
            },
            migration_checkpoint: MigrationForensicMigrationCheckpoint {
                last_completed_stage: MigrationStage::Import,
                manifest: "{}".to_string(),
            },
            corruption_detail: MigrationForensicCorruptionDetail {
                location: "target-import".to_string(),
                affected_ordinals: vec![0, 1, 2],
                detail: "e2e test corruption".to_string(),
            },
        };

        let context = MigrationRollbackPlaybookContext {
            rollback_class: MigrationRollbackClass::DataIntegrityEmergency,
            from_stage: MigrationStage::Import,
            pre_migration_checkpoints: BTreeMap::new(),
            forensic_capture: Some(forensic_capture),
            forensics_output_dir: dir.path().to_path_buf(),
        };

        let mut state = MigrationRollbackExecutionState::default();
        let report = execute_migration_rollback_playbook(&mut state, &context).unwrap();

        assert_eq!(report.tier, MigrationRollbackClass::DataIntegrityEmergency);
        assert!(state.recorder_writes_blocked());
        assert!(!report.migration_active);
    });
}
