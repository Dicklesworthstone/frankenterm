//! E4.F1.T1: Contract-level unit and integration suites for recorder and projection seams.
//!
//! Tests RecorderStorage trait contracts against AppendLog backend and mock backends.
//! When FrankenSqlite is implemented, each test should be parameterized to run against both.

use frankenterm_core::recorder_migration::{
    CheckpointSyncResult, CutoverResult, MigrationConfig, MigrationEngine, MigrationManifest,
    MigrationStage,
};
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointCommitOutcome,
    CheckpointConsumerId, DurabilityLevel, FlushMode, RecorderBackendKind, RecorderCheckpoint,
    RecorderOffset, RecorderStorage, RecorderStorageError, RecorderStorageErrorClass,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 4,
        max_batch_events: 16,
        max_batch_bytes: 128 * 1024,
        max_idempotency_entries: 8,
    }
}

fn sample_event(event_id: &str, pane_id: u64, sequence: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
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

fn append_req(batch_id: &str, events: Vec<RecorderEvent>) -> AppendRequest {
    AppendRequest {
        batch_id: batch_id.to_string(),
        events,
        required_durability: DurabilityLevel::Appended,
        producer_ts_ms: 1,
    }
}

// ===========================================================================
// RecorderStorage trait contract tests (AppendLog backend)
// ===========================================================================

#[tokio::test]
async fn test_append_idempotency_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    let first = storage
        .append_batch(append_req(
            "same-batch",
            vec![sample_event("e1", 1, 0, "one")],
        ))
        .await
        .unwrap();
    assert_eq!(first.accepted_count, 1);

    let second = storage
        .append_batch(append_req(
            "same-batch",
            vec![sample_event("e2", 1, 1, "two")],
        ))
        .await
        .unwrap();
    // Idempotent: same batch_id → same result, no duplicate
    assert_eq!(second.accepted_count, 1);
    assert_eq!(second.first_offset, first.first_offset);
}

#[tokio::test]
async fn test_ordinal_monotonicity_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    let r1 = storage
        .append_batch(append_req("b1", vec![sample_event("e1", 1, 0, "first")]))
        .await
        .unwrap();

    let r2 = storage
        .append_batch(append_req("b2", vec![sample_event("e2", 1, 1, "second")]))
        .await
        .unwrap();

    assert!(
        r2.first_offset.ordinal > r1.last_offset.ordinal,
        "ordinals must be monotonically increasing: {} vs {}",
        r2.first_offset.ordinal,
        r1.last_offset.ordinal
    );
}

#[tokio::test]
async fn test_checkpoint_roundtrip_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let consumer = CheckpointConsumerId("test-consumer".to_string());

    // Initially no checkpoint
    let initial = storage.read_checkpoint(&consumer).await.unwrap();
    assert!(initial.is_none());

    // Commit a checkpoint
    let cp = RecorderCheckpoint {
        consumer: consumer.clone(),
        upto_offset: RecorderOffset {
            segment_id: 0,
            byte_offset: 100,
            ordinal: 5,
        },
        schema_version: "ft.recorder.event.v1".to_string(),
        committed_at_ms: 1000,
    };
    let outcome = storage.commit_checkpoint(cp.clone()).await.unwrap();
    assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

    // Read it back
    let read = storage.read_checkpoint(&consumer).await.unwrap();
    assert!(read.is_some());
    let read = read.unwrap();
    assert_eq!(read.upto_offset.ordinal, 5);
    assert_eq!(read.consumer, consumer);
}

#[tokio::test]
async fn test_checkpoint_regression_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let consumer = CheckpointConsumerId("regress-test".to_string());

    // Advance to ordinal 10
    let cp1 = RecorderCheckpoint {
        consumer: consumer.clone(),
        upto_offset: RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 10,
        },
        schema_version: "ft.recorder.event.v1".to_string(),
        committed_at_ms: 1000,
    };
    storage.commit_checkpoint(cp1).await.unwrap();

    // Try to go backwards to ordinal 5
    let cp2 = RecorderCheckpoint {
        consumer: consumer.clone(),
        upto_offset: RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        },
        schema_version: "ft.recorder.event.v1".to_string(),
        committed_at_ms: 2000,
    };
    // AppendLog backend returns CheckpointRegression error for backwards ordinals
    let result = storage.commit_checkpoint(cp2).await;
    assert!(result.is_err(), "checkpoint regression must be rejected");
    assert!(matches!(
        result.unwrap_err(),
        RecorderStorageError::CheckpointRegression { .. }
    ));
}

#[tokio::test]
async fn test_health_queue_depth_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    let health = storage.health().await;
    assert_eq!(health.backend, RecorderBackendKind::AppendLog);
    assert!(!health.degraded);
    assert_eq!(health.queue_depth, 0);
    assert!(health.queue_capacity > 0);
}

#[tokio::test]
async fn test_lag_metrics_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    // Append events and commit a checkpoint behind head
    storage
        .append_batch(append_req(
            "b1",
            vec![
                sample_event("e1", 1, 0, "a"),
                sample_event("e2", 1, 1, "b"),
                sample_event("e3", 1, 2, "c"),
            ],
        ))
        .await
        .unwrap();

    let consumer = CheckpointConsumerId("lag-test".to_string());
    storage
        .commit_checkpoint(RecorderCheckpoint {
            consumer: consumer.clone(),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            schema_version: "v1".to_string(),
            committed_at_ms: 1000,
        })
        .await
        .unwrap();

    let lag = storage.lag_metrics().await.unwrap();
    assert!(lag.latest_offset.is_some());
    assert!(!lag.consumers.is_empty());
    let consumer_lag = &lag.consumers[0];
    assert!(consumer_lag.offsets_behind > 0);
}

// ---------------------------------------------------------------------------
// Error class mapping tests
// ---------------------------------------------------------------------------

#[test]
fn test_error_class_retryable() {
    let err =
        RecorderStorageError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"));
    assert_eq!(err.class(), RecorderStorageErrorClass::Retryable);
}

#[test]
fn test_error_class_overload() {
    let err = RecorderStorageError::QueueFull { capacity: 10 };
    assert_eq!(err.class(), RecorderStorageErrorClass::Overload);
}

#[test]
fn test_error_class_terminal_data() {
    let err = RecorderStorageError::InvalidRequest {
        message: "bad data".to_string(),
    };
    assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
}

#[test]
fn test_error_class_corruption() {
    let err = RecorderStorageError::CorruptRecord {
        offset: 42,
        reason: "bad crc".to_string(),
    };
    assert_eq!(err.class(), RecorderStorageErrorClass::Corruption);
}

#[test]
fn test_error_class_dependency_unavailable() {
    let err = RecorderStorageError::BackendUnavailable {
        backend: RecorderBackendKind::FrankenSqlite,
        message: "not implemented".to_string(),
    };
    assert_eq!(
        err.class(),
        RecorderStorageErrorClass::DependencyUnavailable
    );
}

#[test]
fn test_error_class_checkpoint_regression_is_terminal() {
    let err = RecorderStorageError::CheckpointRegression {
        consumer: "c".to_string(),
        current_ordinal: 10,
        attempted_ordinal: 5,
    };
    assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
}

// ---------------------------------------------------------------------------
// Flush contract tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_flush_buffered_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    storage
        .append_batch(append_req(
            "flush-test",
            vec![sample_event("e1", 1, 0, "text")],
        ))
        .await
        .unwrap();

    let stats = storage.flush(FlushMode::Buffered).await.unwrap();
    assert_eq!(stats.backend, RecorderBackendKind::AppendLog);
}

#[tokio::test]
async fn test_flush_durable_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    storage
        .append_batch(append_req(
            "flush-durable",
            vec![sample_event("e1", 1, 0, "text")],
        ))
        .await
        .unwrap();

    let stats = storage.flush(FlushMode::Durable).await.unwrap();
    assert_eq!(stats.backend, RecorderBackendKind::AppendLog);
}

// ---------------------------------------------------------------------------
// Migration engine contract tests
// ---------------------------------------------------------------------------

#[test]
fn test_manifest_digest_deterministic() {
    use frankenterm_core::recorder_storage::{CursorRecord, EventCursorError, RecorderEventReader};

    struct StaticReader {
        records: Vec<CursorRecord>,
    }

    impl RecorderEventReader for StaticReader {
        fn open_cursor(
            &self,
            from: RecorderOffset,
        ) -> std::result::Result<
            Box<dyn frankenterm_core::recorder_storage::RecorderEventCursor>,
            EventCursorError,
        > {
            struct Cur {
                recs: Vec<CursorRecord>,
                pos: usize,
            }
            impl frankenterm_core::recorder_storage::RecorderEventCursor for Cur {
                fn next_batch(
                    &mut self,
                    max: usize,
                ) -> std::result::Result<Vec<CursorRecord>, EventCursorError> {
                    let end = (self.pos + max).min(self.recs.len());
                    let batch = self.recs[self.pos..end].to_vec();
                    self.pos = end;
                    Ok(batch)
                }
                fn current_offset(&self) -> RecorderOffset {
                    if self.pos < self.recs.len() {
                        self.recs[self.pos].offset.clone()
                    } else {
                        RecorderOffset {
                            segment_id: 0,
                            byte_offset: 0,
                            ordinal: 0,
                        }
                    }
                }
            }
            let recs: Vec<_> = self
                .records
                .iter()
                .filter(|r| r.offset.ordinal >= from.ordinal)
                .cloned()
                .collect();
            Ok(Box::new(Cur { recs, pos: 0 }))
        }

        fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
            Ok(RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: self.records.len() as u64,
            })
        }
    }

    let records: Vec<CursorRecord> = (0..5)
        .map(|i| CursorRecord {
            event: sample_event(&format!("e{i}"), 1, i, &format!("text-{i}")),
            offset: RecorderOffset {
                segment_id: 0,
                byte_offset: i * 100,
                ordinal: i,
            },
        })
        .collect();

    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut m1 = MigrationManifest::default();
    let r1 = StaticReader {
        records: records.clone(),
    };
    engine.m1_export(&r1, &mut m1).unwrap();

    let mut m2 = MigrationManifest::default();
    let r2 = StaticReader { records };
    engine.m1_export(&r2, &mut m2).unwrap();

    assert_eq!(m1.export_digest, m2.export_digest);
    assert_eq!(m1.export_count, m2.export_count);
}

// ---------------------------------------------------------------------------
// Multi-batch append ordering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multi_batch_preserves_order() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    for i in 0..3u64 {
        storage
            .append_batch(append_req(
                &format!("batch-{i}"),
                vec![sample_event(&format!("e{i}"), 1, i, &format!("text-{i}"))],
            ))
            .await
            .unwrap();
    }

    let health = storage.health().await;
    let offset = health.latest_offset.unwrap();
    assert_eq!(offset.ordinal, 2); // 0-indexed, 3 events
}

// ---------------------------------------------------------------------------
// Empty batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_empty_batch_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    let result = storage.append_batch(append_req("empty", vec![])).await;

    // Empty batch should either succeed with 0 count or be rejected
    match result {
        Ok(r) => assert_eq!(r.accepted_count, 0),
        Err(e) => assert!(matches!(e, RecorderStorageError::InvalidRequest { .. })),
    }
}

// ---------------------------------------------------------------------------
// Max batch size enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_max_batch_events_enforced() {
    let dir = tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.max_batch_events = 2;
    let storage = AppendLogRecorderStorage::open(cfg).unwrap();

    // Try to append 3 events with max_batch_events=2
    let events: Vec<_> = (0..3)
        .map(|i| sample_event(&format!("e{i}"), 1, i, &format!("t{i}")))
        .collect();
    let result = storage.append_batch(append_req("big", events)).await;

    // Should be rejected
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Backend kind contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_backend_kind_is_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    assert_eq!(storage.backend_kind(), RecorderBackendKind::AppendLog);
}

// ---------------------------------------------------------------------------
// Multiple consumers checkpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multiple_consumers_independent_checkpoints() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

    let c1 = CheckpointConsumerId("consumer-a".to_string());
    let c2 = CheckpointConsumerId("consumer-b".to_string());

    storage
        .commit_checkpoint(RecorderCheckpoint {
            consumer: c1.clone(),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 5,
            },
            schema_version: "v1".to_string(),
            committed_at_ms: 1000,
        })
        .await
        .unwrap();

    storage
        .commit_checkpoint(RecorderCheckpoint {
            consumer: c2.clone(),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 10,
            },
            schema_version: "v1".to_string(),
            committed_at_ms: 2000,
        })
        .await
        .unwrap();

    let cp1 = storage.read_checkpoint(&c1).await.unwrap().unwrap();
    let cp2 = storage.read_checkpoint(&c2).await.unwrap().unwrap();

    assert_eq!(cp1.upto_offset.ordinal, 5);
    assert_eq!(cp2.upto_offset.ordinal, 10);
}

// ---------------------------------------------------------------------------
// Checkpoint advance (noop if already advanced)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_checkpoint_noop_already_advanced() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let consumer = CheckpointConsumerId("noop-test".to_string());

    let cp = RecorderCheckpoint {
        consumer: consumer.clone(),
        upto_offset: RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 5,
        },
        schema_version: "v1".to_string(),
        committed_at_ms: 1000,
    };
    storage.commit_checkpoint(cp.clone()).await.unwrap();

    // Same ordinal = noop
    let outcome = storage.commit_checkpoint(cp).await.unwrap();
    assert_eq!(outcome, CheckpointCommitOutcome::NoopAlreadyAdvanced);
}

// ---------------------------------------------------------------------------
// Health with degraded source detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_not_degraded_initially() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let health = storage.health().await;
    assert!(!health.degraded);
    assert!(health.last_error.is_none());
}

// ---------------------------------------------------------------------------
// MigrationStage contract tests
// ---------------------------------------------------------------------------

#[test]
fn test_stage_m0_through_m5_progression() {
    let stages = [
        MigrationStage::M0Preflight,
        MigrationStage::M1Export,
        MigrationStage::M2Import,
        MigrationStage::M3CheckpointSync,
        MigrationStage::M4Reserved,
        MigrationStage::M5Cutover,
    ];

    // Only M5 is complete
    for (i, stage) in stages.iter().enumerate() {
        if i == 5 {
            assert!(stage.is_complete());
        } else {
            assert!(!stage.is_complete());
        }
    }

    // M0-M3 can rollback, M4-M5 cannot
    assert!(MigrationStage::M0Preflight.can_rollback());
    assert!(MigrationStage::M3CheckpointSync.can_rollback());
    assert!(!MigrationStage::M4Reserved.can_rollback());
    assert!(!MigrationStage::M5Cutover.can_rollback());
}

// ---------------------------------------------------------------------------
// RecorderOffset ordering
// ---------------------------------------------------------------------------

#[test]
fn test_recorder_offset_equality() {
    let a = RecorderOffset {
        segment_id: 0,
        byte_offset: 100,
        ordinal: 5,
    };
    let b = RecorderOffset {
        segment_id: 0,
        byte_offset: 100,
        ordinal: 5,
    };
    assert_eq!(a, b);
}

#[test]
fn test_recorder_offset_inequality() {
    let a = RecorderOffset {
        segment_id: 0,
        byte_offset: 100,
        ordinal: 5,
    };
    let b = RecorderOffset {
        segment_id: 0,
        byte_offset: 200,
        ordinal: 6,
    };
    assert_ne!(a, b);
}

// ---------------------------------------------------------------------------
// Serde roundtrip contracts
// ---------------------------------------------------------------------------

#[test]
fn test_backend_kind_serde_roundtrip() {
    for kind in [
        RecorderBackendKind::AppendLog,
        RecorderBackendKind::FrankenSqlite,
    ] {
        let json = serde_json::to_string(&kind).unwrap();
        let back: RecorderBackendKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, back);
    }
}

#[test]
fn test_migration_manifest_serde_roundtrip() {
    let manifest = MigrationManifest {
        event_count: 100,
        first_ordinal: 0,
        last_ordinal: 99,
        export_digest: 0xDEAD,
        export_count: 100,
        import_digest: 0xDEAD,
        import_count: 100,
        ..Default::default()
    };
    let json = serde_json::to_string(&manifest).unwrap();
    let back: MigrationManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(manifest, back);
}

#[test]
fn test_cutover_result_serde_roundtrip() {
    let result = CutoverResult {
        activated_backend: RecorderBackendKind::FrankenSqlite,
        migration_epoch_ms: 1708000000,
        target_healthy: true,
        source_retained_path: Some("/data/events.log".to_string()),
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: CutoverResult = serde_json::from_str(&json).unwrap();
    assert_eq!(result, back);
}

#[test]
fn test_checkpoint_sync_result_serde_roundtrip() {
    let result = CheckpointSyncResult {
        consumers_found: 3,
        checkpoints_migrated: 2,
        checkpoints_reset: 1,
        reset_consumers: vec!["stale".to_string()],
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: CheckpointSyncResult = serde_json::from_str(&json).unwrap();
    assert_eq!(result, back);
}

// ---------------------------------------------------------------------------
// Error class serde roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_error_class_serde_all_variants() {
    for class in [
        RecorderStorageErrorClass::Retryable,
        RecorderStorageErrorClass::Overload,
        RecorderStorageErrorClass::TerminalConfig,
        RecorderStorageErrorClass::TerminalData,
        RecorderStorageErrorClass::Corruption,
        RecorderStorageErrorClass::DependencyUnavailable,
    ] {
        let json = serde_json::to_string(&class).unwrap();
        let back: RecorderStorageErrorClass = serde_json::from_str(&json).unwrap();
        assert_eq!(class, back);
    }
}

// ---------------------------------------------------------------------------
// Append data path contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_append_log_data_path_present() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    assert!(storage.append_log_data_path().is_some());
}

// ---------------------------------------------------------------------------
// FrankenSqlite backend is unavailable (not yet implemented)
// ---------------------------------------------------------------------------

#[test]
fn test_frankensqlite_bootstrap_returns_unavailable() {
    use frankenterm_core::recorder_storage::{RecorderStorageConfig, bootstrap_recorder_storage};
    let dir = tempdir().unwrap();
    let config = RecorderStorageConfig {
        backend: RecorderBackendKind::FrankenSqlite,
        append_log: test_config(dir.path()),
    };
    let err = bootstrap_recorder_storage(config).unwrap_err();
    assert!(matches!(
        err,
        RecorderStorageError::BackendUnavailable { .. }
    ));
    assert_eq!(
        err.class(),
        RecorderStorageErrorClass::DependencyUnavailable
    );
}
