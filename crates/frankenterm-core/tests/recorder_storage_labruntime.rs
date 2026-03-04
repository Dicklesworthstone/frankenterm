//! LabRuntime-ported recorder storage tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `recorder_storage.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for append-log storage,
//! checkpoint management, and idempotency cache tests.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointCommitOutcome,
    CheckpointConsumerId, DurabilityLevel, FlushMode, RecorderBackendKind, RecorderCheckpoint,
    RecorderOffset, RecorderStorage, RecorderStorageConfig, RecorderStorageError,
    RecorderStorageErrorClass,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
    RECORDER_EVENT_SCHEMA_VERSION_V1,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use tempfile::tempdir;

// ===========================================================================
// Helpers (mirrors recorder_storage.rs test helpers)
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

fn test_config(path: &Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 4,
        max_batch_events: 16,
        max_batch_bytes: 128 * 1024,
        max_idempotency_entries: 8,
    }
}

#[allow(dead_code)]
fn recorder_test_config(path: &Path) -> RecorderStorageConfig {
    RecorderStorageConfig {
        backend: RecorderBackendKind::AppendLog,
        append_log: test_config(path),
    }
}

// ===========================================================================
// Section 1: Append operations
// ===========================================================================

#[test]
fn rs_append_assigns_monotonic_offsets() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let r1 = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![
                    sample_event("e1", 1, 0, "first"),
                    sample_event("e2", 1, 1, "second"),
                ],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let r2 = storage
            .append_batch(AppendRequest {
                batch_id: "b2".to_string(),
                events: vec![sample_event("e3", 2, 2, "third")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 2,
            })
            .await
            .unwrap();

        assert_eq!(r1.first_offset.ordinal, 0);
        assert_eq!(r1.last_offset.ordinal, 1);
        assert_eq!(r2.first_offset.ordinal, 2);
        assert!(r2.first_offset.byte_offset > r1.first_offset.byte_offset);
        assert_eq!(r2.last_offset.ordinal, 2);
    });
}

#[test]
fn rs_duplicate_batch_id_is_idempotent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());
        let data_path = cfg.data_path.clone();
        let storage = AppendLogRecorderStorage::open(cfg).unwrap();

        let first = storage
            .append_batch(AppendRequest {
                batch_id: "same-batch".to_string(),
                events: vec![sample_event("e1", 1, 0, "one")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let before_len = std::fs::metadata(&data_path).unwrap().len();
        let second = storage
            .append_batch(AppendRequest {
                batch_id: "same-batch".to_string(),
                events: vec![sample_event("e2", 1, 1, "two")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 2,
            })
            .await
            .unwrap();
        let after_len = std::fs::metadata(&data_path).unwrap().len();

        assert_eq!(first, second);
        assert_eq!(before_len, after_len);
    });
}

// ===========================================================================
// Section 2: Checkpoint management
// ===========================================================================

#[test]
fn rs_checkpoint_commit_is_monotonic_and_persisted() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state_path = cfg.state_path.clone();
        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "one")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let cp = RecorderCheckpoint {
            consumer: CheckpointConsumerId("lexical".to_string()),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            schema_version: "ft.recorder.event.v1".to_string(),
            committed_at_ms: 123,
        };

        let outcome = storage.commit_checkpoint(cp.clone()).await.unwrap();
        assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

        let read_back = storage
            .read_checkpoint(&CheckpointConsumerId("lexical".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_back.upto_offset.ordinal, 0);

        let regression = RecorderCheckpoint {
            consumer: CheckpointConsumerId("lexical".to_string()),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            schema_version: "ft.recorder.event.v1".to_string(),
            committed_at_ms: 124,
        };
        let no_op = storage.commit_checkpoint(regression).await.unwrap();
        assert_eq!(no_op, CheckpointCommitOutcome::NoopAlreadyAdvanced);

        drop(storage);

        let reopened = AppendLogRecorderStorage::open(cfg).unwrap();
        let persisted = reopened
            .read_checkpoint(&CheckpointConsumerId("lexical".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.upto_offset.ordinal, 0);
        assert!(state_path.exists());
    });
}

// ===========================================================================
// Section 3: Torn tail recovery
// ===========================================================================

#[test]
fn rs_startup_truncates_torn_tail_and_recovers_ordinal() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        let event = sample_event("e1", 1, 0, "hello");
        let payload = serde_json::to_vec(&event).unwrap();
        let valid_len = 4 + payload.len() as u64;

        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&cfg.data_path)
                .unwrap();
            file.write_all(&(payload.len() as u32).to_le_bytes())
                .unwrap();
            file.write_all(&payload).unwrap();
            file.write_all(&(100u32).to_le_bytes()).unwrap();
            file.write_all(b"abc").unwrap();
            file.flush().unwrap();
        }

        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
        let recovered_len = std::fs::metadata(&cfg.data_path).unwrap().len();
        assert_eq!(recovered_len, valid_len);

        let response = storage
            .append_batch(AppendRequest {
                batch_id: "b2".to_string(),
                events: vec![sample_event("e2", 1, 1, "world")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 2,
            })
            .await
            .unwrap();

        assert_eq!(response.first_offset.ordinal, 1);
    });
}

// ===========================================================================
// Section 4: Request validation
// ===========================================================================

#[test]
fn rs_rejects_batch_larger_than_configured_byte_limit() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.max_batch_bytes = 32;
        let storage = AppendLogRecorderStorage::open(cfg).unwrap();

        let err = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event(
                    "e1",
                    1,
                    0,
                    "this event payload is intentionally too long",
                )],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap_err();

        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
    });
}

#[test]
fn rs_rejects_empty_batch_id() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let err = storage
            .append_batch(AppendRequest {
                batch_id: "  ".to_string(),
                events: vec![sample_event("e1", 1, 0, "hello")],
                required_durability: DurabilityLevel::Enqueued,
                producer_ts_ms: 1,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    });
}

#[test]
fn rs_rejects_empty_events_list() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let err = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![],
                required_durability: DurabilityLevel::Enqueued,
                producer_ts_ms: 1,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    });
}

#[test]
fn rs_rejects_batch_exceeding_event_count_limit() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.max_batch_events = 2;
        let storage = AppendLogRecorderStorage::open(cfg).unwrap();

        let err = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![
                    sample_event("e1", 1, 0, "a"),
                    sample_event("e2", 1, 1, "b"),
                    sample_event("e3", 1, 2, "c"),
                ],
                required_durability: DurabilityLevel::Enqueued,
                producer_ts_ms: 1,
            })
            .await
            .unwrap_err();

        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));
    });
}

// ===========================================================================
// Section 5: Idempotency cache
// ===========================================================================

#[test]
fn rs_idempotency_cache_evicts_oldest_when_full() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.max_idempotency_entries = 3;
        let storage = AppendLogRecorderStorage::open(cfg).unwrap();

        for i in 0..4u64 {
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: format!("b{i}"),
                    events: vec![sample_event(&format!("e{i}"), 1, i, "x")],
                    required_durability: DurabilityLevel::Enqueued,
                    producer_ts_ms: i,
                })
                .await
                .unwrap();
        }

        let data_len_before = std::fs::metadata(dir.path().join("events.log"))
            .unwrap()
            .len();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b0".to_string(),
                events: vec![sample_event("e0-replay", 1, 100, "replay")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 100,
            })
            .await
            .unwrap();

        let data_len_after = std::fs::metadata(dir.path().join("events.log"))
            .unwrap()
            .len();

        assert!(data_len_after > data_len_before);
        assert_eq!(resp.first_offset.ordinal, 4);

        let data_len_before2 = std::fs::metadata(dir.path().join("events.log"))
            .unwrap()
            .len();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b3".to_string(),
                events: vec![sample_event("e3-replay", 1, 200, "replay")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 200,
            })
            .await
            .unwrap();

        let data_len_after2 = std::fs::metadata(dir.path().join("events.log"))
            .unwrap()
            .len();

        assert_eq!(data_len_before2, data_len_after2, "b3 should be cached");
    });
}

// ===========================================================================
// Section 6: Health and lag metrics
// ===========================================================================

#[test]
fn rs_health_reports_correct_state() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let h = storage.health().await;
        assert_eq!(h.backend, RecorderBackendKind::AppendLog);
        assert!(!h.degraded);
        assert_eq!(h.queue_depth, 0);
        assert_eq!(h.queue_capacity, 4);
        assert!(h.latest_offset.is_none());
        assert!(h.last_error.is_none());

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "hi")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let h2 = storage.health().await;
        assert!(h2.latest_offset.is_some());
        assert_eq!(h2.latest_offset.unwrap().ordinal, 0);
    });
}

#[test]
fn rs_lag_metrics_track_consumer_offsets() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        for i in 0..5u64 {
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: format!("b{i}"),
                    events: vec![sample_event(&format!("e{i}"), 1, i, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: i,
                })
                .await
                .unwrap();
        }

        let _ = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: CheckpointConsumerId("indexer".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 2,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 100,
            })
            .await
            .unwrap();

        let _ = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: CheckpointConsumerId("search".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 4,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 100,
            })
            .await
            .unwrap();

        let lag = storage.lag_metrics().await.unwrap();
        assert!(lag.latest_offset.is_some());
        assert_eq!(lag.latest_offset.unwrap().ordinal, 4);
        assert_eq!(lag.consumers.len(), 2);
        assert_eq!(lag.consumers[0].consumer.0, "indexer");
        assert_eq!(lag.consumers[0].offsets_behind, 2);
        assert_eq!(lag.consumers[1].consumer.0, "search");
        assert_eq!(lag.consumers[1].offsets_behind, 0);
    });
}

// ===========================================================================
// Section 7: Checkpoint regression
// ===========================================================================

#[test]
fn rs_checkpoint_regression_returns_error() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let _ = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: CheckpointConsumerId("cons".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 5,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 100,
            })
            .await
            .unwrap();

        let err = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: CheckpointConsumerId("cons".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 3,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 200,
            })
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            RecorderStorageError::CheckpointRegression { .. }
        ));
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
    });
}

#[test]
fn rs_health_records_checkpoint_regression_diagnostic() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        let consumer = CheckpointConsumerId("diag-consumer".to_string());

        let _ = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: consumer.clone(),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 5,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 100,
            })
            .await
            .unwrap();

        let err = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: consumer.clone(),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 3,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 101,
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            RecorderStorageError::CheckpointRegression { .. }
        ));

        let degraded = storage.health().await;
        assert!(degraded.degraded);
        let diagnostic = degraded.last_error.unwrap();
        assert!(diagnostic.contains("commit_checkpoint failed"));
        assert!(diagnostic.contains("TerminalData"));

        let _ = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer,
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 8,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 102,
            })
            .await
            .unwrap();

        let healthy = storage.health().await;
        assert!(!healthy.degraded);
        assert!(healthy.last_error.is_none());
    });
}

#[test]
fn rs_health_records_append_diagnostic_and_clears_on_success() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let mut cfg = test_config(dir.path());
        cfg.max_batch_bytes = 1200;
        let storage = AppendLogRecorderStorage::open(cfg).unwrap();

        let err = storage
            .append_batch(AppendRequest {
                batch_id: "oversized".to_string(),
                events: vec![sample_event("e-big", 1, 0, &"x".repeat(5000))],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 10,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RecorderStorageError::InvalidRequest { .. }));

        let degraded = storage.health().await;
        assert!(degraded.degraded);
        let diagnostic = degraded.last_error.unwrap();
        assert!(diagnostic.contains("append_batch failed"));
        assert!(diagnostic.contains("TerminalData"));

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "small".to_string(),
                events: vec![sample_event("e-small", 1, 1, "ok")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 11,
            })
            .await
            .unwrap();

        let healthy = storage.health().await;
        assert!(!healthy.degraded);
        assert!(healthy.last_error.is_none());
    });
}

// ===========================================================================
// Section 8: Read checkpoint / Flush / Durability
// ===========================================================================

#[test]
fn rs_read_checkpoint_unknown_consumer_returns_none() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let result = storage
            .read_checkpoint(&CheckpointConsumerId("nonexistent".to_string()))
            .await
            .unwrap();

        assert!(result.is_none());
    });
}

#[test]
fn rs_flush_buffered_and_durable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Enqueued,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let stats_buf = storage.flush(FlushMode::Buffered).await.unwrap();
        assert_eq!(stats_buf.backend, RecorderBackendKind::AppendLog);
        assert!(stats_buf.latest_offset.is_some());

        let stats_dur = storage.flush(FlushMode::Durable).await.unwrap();
        assert_eq!(stats_dur.backend, RecorderBackendKind::AppendLog);
    });
}

#[test]
fn rs_enqueued_durability_does_not_fsync() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Enqueued,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(resp.committed_durability, DurabilityLevel::Enqueued);
        assert_eq!(resp.accepted_count, 1);
    });
}

#[test]
fn rs_fsync_durability_committed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Fsync,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(resp.committed_durability, DurabilityLevel::Fsync);
        assert!(dir.path().join("state.json").exists());
    });
}

// ===========================================================================
// Section 9: Reopen persistence
// ===========================================================================

#[test]
fn rs_reopen_continues_ordinals() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        {
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![
                        sample_event("e1", 1, 0, "a"),
                        sample_event("e2", 1, 1, "b"),
                        sample_event("e3", 1, 2, "c"),
                    ],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
        }

        let storage2 = AppendLogRecorderStorage::open(cfg).unwrap();
        let resp = storage2
            .append_batch(AppendRequest {
                batch_id: "b2".to_string(),
                events: vec![sample_event("e4", 1, 3, "d")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 2,
            })
            .await
            .unwrap();

        assert_eq!(resp.first_offset.ordinal, 3);
    });
}

// ===========================================================================
// Section 10: Backend kind
// ===========================================================================

#[test]
fn rs_backend_kind_is_append_log() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        assert_eq!(storage.backend_kind(), RecorderBackendKind::AppendLog);
    });
}

// ===========================================================================
// Section 11: Multi-batch ordering
// ===========================================================================

#[test]
fn rs_multi_batch_accepted_count_correct() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![
                    sample_event("e1", 1, 0, "a"),
                    sample_event("e2", 1, 1, "b"),
                    sample_event("e3", 1, 2, "c"),
                    sample_event("e4", 1, 3, "d"),
                ],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(resp.accepted_count, 4);
        assert_eq!(resp.first_offset.ordinal, 0);
        assert_eq!(resp.last_offset.ordinal, 3);
    });
}

#[test]
fn rs_open_with_empty_data_file() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        std::fs::create_dir_all(cfg.data_path.parent().unwrap()).unwrap();
        std::fs::write(&cfg.data_path, []).unwrap();

        let storage = AppendLogRecorderStorage::open(cfg).unwrap();
        let h = storage.health().await;
        assert!(h.latest_offset.is_none());
    });
}

#[test]
fn rs_lag_with_no_consumers() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let lag = storage.lag_metrics().await.unwrap();
        assert!(lag.consumers.is_empty());
        assert!(lag.latest_offset.is_some());
    });
}

// ===========================================================================
// Section 12: Extended batch tests
// ===========================================================================

#[test]
fn rs_multiple_consumers_checkpoint_advance() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        for (name, ordinal) in [("alpha", 2u64), ("beta", 5), ("gamma", 10)] {
            let outcome = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId(name.to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 100,
                })
                .await
                .unwrap();
            assert_eq!(outcome, CheckpointCommitOutcome::Advanced);
        }

        let outcome = storage
            .commit_checkpoint(RecorderCheckpoint {
                consumer: CheckpointConsumerId("beta".to_string()),
                upto_offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 8,
                },
                schema_version: "v1".to_string(),
                committed_at_ms: 200,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

        let beta = storage
            .read_checkpoint(&CheckpointConsumerId("beta".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(beta.upto_offset.ordinal, 8);

        let alpha = storage
            .read_checkpoint(&CheckpointConsumerId("alpha".to_string()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(alpha.upto_offset.ordinal, 2);
    });
}

#[test]
fn rs_lag_metrics_empty_store() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let lag = storage.lag_metrics().await.unwrap();
        assert!(lag.latest_offset.is_none());
        assert!(lag.consumers.is_empty());
    });
}

#[test]
fn rs_flush_empty_store() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let stats = storage.flush(FlushMode::Buffered).await.unwrap();
        assert_eq!(stats.backend, RecorderBackendKind::AppendLog);
        assert!(stats.latest_offset.is_none());

        let stats_dur = storage.flush(FlushMode::Durable).await.unwrap();
        assert!(stats_dur.latest_offset.is_none());
    });
}

#[test]
fn rs_single_event_accepted_count_is_one() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "only-one")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(resp.accepted_count, 1);
        assert_eq!(resp.first_offset, resp.last_offset);
    });
}

#[test]
fn rs_byte_offset_monotonicity_across_batches() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let mut offsets = Vec::new();
        for i in 0..5u64 {
            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: format!("b{}", i),
                    events: vec![sample_event(&format!("e{}", i), 1, i, "payload")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: i,
                })
                .await
                .unwrap();
            offsets.push(resp.first_offset.byte_offset);
        }

        for window in offsets.windows(2) {
            assert!(
                window[1] > window[0],
                "byte offsets not strictly increasing: {} <= {}",
                window[1],
                window[0]
            );
        }
    });
}

#[test]
fn rs_reopen_preserves_checkpoints_across_restart() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        {
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            let _ = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            let _ = storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId("persist-test".to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 500,
                })
                .await
                .unwrap();
        }

        let storage2 = AppendLogRecorderStorage::open(cfg).unwrap();
        let cp = storage2
            .read_checkpoint(&CheckpointConsumerId("persist-test".to_string()))
            .await
            .unwrap();
        assert!(cp.is_some());
        let cp = cp.unwrap();
        assert_eq!(cp.upto_offset.ordinal, 0);
        assert_eq!(cp.schema_version, "v1");
        assert_eq!(cp.committed_at_ms, 500);
    });
}

#[test]
fn rs_health_with_latest_offset_after_multi_event_batch() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![
                    sample_event("e1", 1, 0, "a"),
                    sample_event("e2", 1, 1, "b"),
                    sample_event("e3", 1, 2, "c"),
                ],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let h = storage.health().await;
        let latest = h.latest_offset.unwrap();
        assert_eq!(latest.ordinal, 2);
        assert!(!h.degraded);
        assert_eq!(h.queue_depth, 0);
    });
}

#[test]
fn rs_appended_durability_persists_state() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());
        let state_path = cfg.state_path.clone();
        let storage = AppendLogRecorderStorage::open(cfg).unwrap();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        assert!(state_path.exists());
        let bytes = std::fs::read(&state_path).unwrap();
        assert!(!bytes.is_empty());

        let state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(state.get("next_ordinal").is_some());
        assert_eq!(state["next_ordinal"], 1);
    });
}

#[test]
fn rs_response_backend_always_append_log() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        for i in 0..3u64 {
            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: format!("b{}", i),
                    events: vec![sample_event(&format!("e{}", i), 1, i, "data")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: i,
                })
                .await
                .unwrap();
            assert_eq!(resp.backend, RecorderBackendKind::AppendLog);
        }
    });
}

#[test]
fn rs_committed_at_ms_is_nonzero() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        assert!(
            resp.committed_at_ms > 0,
            "committed_at_ms should be a valid epoch timestamp"
        );
    });
}

// ===========================================================================
// Section 13: Torn tail edge cases
// ===========================================================================

#[test]
fn rs_torn_tail_with_only_length_prefix_no_payload() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        std::fs::create_dir_all(cfg.data_path.parent().unwrap()).unwrap();
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&cfg.data_path)
                .unwrap();
            file.write_all(&(100u32).to_le_bytes()).unwrap();
            file.flush().unwrap();
        }

        let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
        let recovered_len = std::fs::metadata(&cfg.data_path).unwrap().len();
        assert_eq!(recovered_len, 0, "torn partial record should be truncated");

        let resp = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "fresh")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();
        assert_eq!(resp.first_offset.ordinal, 0);
        assert_eq!(resp.first_offset.byte_offset, 0);
    });
}

#[test]
fn rs_segment_id_preserved_across_reopen() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let cfg = test_config(dir.path());

        {
            let storage = AppendLogRecorderStorage::open(cfg.clone()).unwrap();
            let resp = storage
                .append_batch(AppendRequest {
                    batch_id: "b1".to_string(),
                    events: vec![sample_event("e1", 1, 0, "seg-test")],
                    required_durability: DurabilityLevel::Appended,
                    producer_ts_ms: 1,
                })
                .await
                .unwrap();
            assert_eq!(resp.first_offset.segment_id, 0);
        }

        let storage2 = AppendLogRecorderStorage::open(cfg).unwrap();
        let resp2 = storage2
            .append_batch(AppendRequest {
                batch_id: "b2".to_string(),
                events: vec![sample_event("e2", 1, 1, "seg-test-2")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 2,
            })
            .await
            .unwrap();
        assert_eq!(resp2.first_offset.segment_id, 0);
    });
}

#[test]
fn rs_flush_updates_flushed_at_ms() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();

        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Enqueued,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let stats = storage.flush(FlushMode::Buffered).await.unwrap();
        assert!(
            stats.flushed_at_ms > 0,
            "flushed_at_ms should be a valid epoch timestamp"
        );

        let stats2 = storage.flush(FlushMode::Durable).await.unwrap();
        assert!(
            stats2.flushed_at_ms >= stats.flushed_at_ms,
            "subsequent flush timestamp should be >= previous"
        );
    });
}

// ===========================================================================
// Section 14: DarkBadger batch + checkpoint noop/sort tests
// ===========================================================================

#[test]
fn rs_whitespace_only_batch_id_rejected() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        let err = storage
            .append_batch(AppendRequest {
                batch_id: "   \t  ".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, RecorderStorageError::InvalidRequest { .. }),
            "whitespace-only batch_id should be rejected"
        );
        assert_eq!(err.class(), RecorderStorageErrorClass::TerminalData);
    });
}

#[test]
fn rs_checkpoint_noop_when_same_ordinal() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        let checkpoint = RecorderCheckpoint {
            consumer: CheckpointConsumerId("c1".to_string()),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            },
            schema_version: "v1".to_string(),
            committed_at_ms: 100,
        };
        let outcome = storage.commit_checkpoint(checkpoint.clone()).await.unwrap();
        assert_eq!(outcome, CheckpointCommitOutcome::Advanced);

        let outcome2 = storage.commit_checkpoint(checkpoint).await.unwrap();
        assert_eq!(outcome2, CheckpointCommitOutcome::NoopAlreadyAdvanced);
    });
}

#[test]
fn rs_lag_consumers_sorted_alphabetically() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        let _ = storage
            .append_batch(AppendRequest {
                batch_id: "b1".to_string(),
                events: vec![sample_event("e1", 1, 0, "data")],
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();

        for name in &["zebra", "alpha", "middle"] {
            storage
                .commit_checkpoint(RecorderCheckpoint {
                    consumer: CheckpointConsumerId(name.to_string()),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: 0,
                    },
                    schema_version: "v1".to_string(),
                    committed_at_ms: 100,
                })
                .await
                .unwrap();
        }

        let lag = storage.lag_metrics().await.unwrap();
        let names: Vec<&str> = lag
            .consumers
            .iter()
            .map(|c| c.consumer.0.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    });
}

#[test]
fn rs_health_queue_depth_reflects_zero_when_idle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
        let health = storage.health().await;
        assert_eq!(health.queue_depth, 0);
        assert!(!health.degraded);
        assert!(health.last_error.is_none());
        assert!(health.latest_offset.is_none());
    });
}
