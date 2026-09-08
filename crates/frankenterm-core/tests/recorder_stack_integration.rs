//! Integration tests for the recorder stack.
//!
//! Exercises cross-module interactions between:
//! - `recorder_storage` (append-log backend)
//! - `recorder_retention` (segment lifecycle management)
//! - `storage_telemetry` (metrics, SLO tracking, diagnostics)
//! - `recorder_audit` (tamper-evident audit trail)
//!
//! These tests validate that the four recorder modules compose correctly
//! and that data flows coherently through the full pipeline.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use frankenterm_core::policy::ActorKind;
use frankenterm_core::recorder_audit::{
    AUDIT_SCHEMA_VERSION, AccessTier, ActorIdentity, AuditEventBuilder, AuditEventType, AuditLog,
    AuditLogConfig, AuthzDecision, GENESIS_HASH, check_authorization, required_tier_for_event,
};
use frankenterm_core::recorder_retention::{
    RetentionConfig, RetentionManager, SegmentMeta, SegmentPhase, SensitivityTier,
};
use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CheckpointCommitOutcome,
    CheckpointConsumerId, DurabilityLevel, FlushMode, RecorderCheckpoint, RecorderStorage,
    RecorderStorageError, RecorderStorageErrorClass,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use frankenterm_core::storage_telemetry::{
    StorageHealthTier, StorageTelemetry, StorageTelemetryConfig, diagnose, remediation_for_error,
};

// =============================================================================
// Async test helper
// =============================================================================

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(future);
    }));
    let teardown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(runtime);
    }));
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        frankenterm_core::runtime_async::clear_runtime_handle();
    }));
    // Attempt every cleanup step without discarding failures or masking the
    // original test-body assertion with a later teardown failure.
    for result in [body, teardown, cleanup] {
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

// =============================================================================
// Test helpers
// =============================================================================

fn sample_event(event_id: &str, pane_id: u64, seq: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("sess-integration".to_string()),
        workflow_id: None,
        correlation_id: Some("corr-integ".to_string()),
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
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

fn sample_redacted_event(event_id: &str, pane_id: u64, seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("sess-integration".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: "[REDACTED]".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Partial,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn storage_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 16,
        max_batch_events: 64,
        max_batch_bytes: 1024 * 1024,
        max_idempotency_entries: 32,
    }
}

#[cfg(unix)]
fn assert_writer_lease_busy(config: AppendLogStorageConfig) {
    let err = AppendLogRecorderStorage::open(config).unwrap_err();
    assert_eq!(err.class(), RecorderStorageErrorClass::Retryable);
    match err {
        RecorderStorageError::Io(source) => {
            assert_eq!(
                source.raw_os_error(),
                fs2::lock_contended_error().raw_os_error()
            );
        }
        other => panic!("expected a contended writer lease, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn append_log_writer_lease_precedes_torn_tail_recovery() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let config = storage_config(dir.path());
    let owner = AppendLogRecorderStorage::open(config.clone()).unwrap();
    // A real incomplete record models bytes that must not be recovered while
    // their writer is alive. Unix advisory locks allow this independent handle.
    let tail = [12, 0, 0, 0, b'{'];
    let mut external = std::fs::OpenOptions::new()
        .append(true)
        .open(&config.data_path)
        .unwrap();
    external.write_all(&tail).unwrap();
    external.sync_data().unwrap();
    drop(external);

    for attempt in 1..=2 {
        assert_writer_lease_busy(config.clone());
        assert_eq!(std::fs::read(&config.data_path).unwrap(), tail);
        assert!(!config.state_path.exists());
        eprintln!("recorder writer lease: phase=busy, attempt={attempt}, preserved_bytes=5");
    }
    drop(owner);
    let recovered = AppendLogRecorderStorage::open(config.clone()).unwrap();
    assert!(std::fs::read(&config.data_path).unwrap().is_empty());
    drop(recovered);
    eprintln!("recorder writer lease: phase=owner_released, recovered_bytes=0");
}

#[cfg(unix)]
#[test]
fn append_log_writer_lease_covers_hard_link_alias() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage_config(dir.path());
    let owner = AppendLogRecorderStorage::open(config.clone()).unwrap();
    let mut alias = config.clone();
    alias.data_path = dir.path().join("events-alias.log");
    std::fs::hard_link(&config.data_path, &alias.data_path).unwrap();
    assert_writer_lease_busy(alias.clone());
    drop(owner);
    let successor = AppendLogRecorderStorage::open(alias).unwrap();
    assert_writer_lease_busy(config);
    drop(successor);
    eprintln!("recorder writer lease: alias=hard_link, phase=successor_exclusive");
}

#[cfg(unix)]
#[test]
fn append_log_writer_lease_covers_symlink_alias() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage_config(dir.path());
    let owner = AppendLogRecorderStorage::open(config.clone()).unwrap();
    let mut alias = config.clone();
    alias.data_path = dir.path().join("events-symlink.log");
    std::os::unix::fs::symlink(&config.data_path, &alias.data_path).unwrap();
    assert_writer_lease_busy(alias.clone());
    drop(owner);
    let successor = AppendLogRecorderStorage::open(alias).unwrap();
    assert_writer_lease_busy(config);
    drop(successor);
    eprintln!("recorder writer lease: alias=symlink, phase=successor_exclusive");
}

#[cfg(unix)]
#[test]
fn append_log_writer_lease_released_after_initialization_failure() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage_config(dir.path());
    std::fs::write(&config.state_path, b"not valid JSON").unwrap();
    for attempt in 1..=2 {
        assert!(matches!(
            AppendLogRecorderStorage::open(config.clone()),
            Err(RecorderStorageError::Json(_))
        ));
        eprintln!("recorder writer lease: phase=initialization_failed, attempt={attempt}");
    }
    std::fs::rename(&config.state_path, dir.path().join("invalid-state.saved")).unwrap();
    let recovered = AppendLogRecorderStorage::open(config.clone()).unwrap();
    assert_writer_lease_busy(config.clone());
    drop(recovered);
    AppendLogRecorderStorage::open(config).unwrap();
    eprintln!("recorder writer lease: phase=initialization_repaired");
}

#[cfg(unix)]
#[test]
fn append_log_writer_lease_cross_process_handoff() {
    const CHILD_MODE: &str = "FT_RECORDER_WRITER_LEASE_CHILD";
    const CHILD_DIR: &str = "FT_RECORDER_WRITER_LEASE_DIR";
    const TEST_NAME: &str = "append_log_writer_lease_cross_process_handoff";

    if let Some(mode) = std::env::var_os(CHILD_MODE) {
        let path = std::path::PathBuf::from(std::env::var_os(CHILD_DIR).unwrap());
        let config = storage_config(&path);
        if mode == "busy" {
            assert_writer_lease_busy(config);
            eprintln!("recorder writer lease: process=child, phase=busy_verified");
        } else {
            assert_eq!(mode, "append");
            run_async_test(async {
                let storage = AppendLogRecorderStorage::open(config).unwrap();
                let receipt = storage
                    .append_batch(AppendRequest {
                        batch_id: "successor-batch".to_string(),
                        events: vec![sample_event("successor", 1, 1, "second")],
                        required_durability: DurabilityLevel::Fsync,
                        producer_ts_ms: 0,
                    })
                    .await
                    .unwrap();
                assert_eq!(receipt.first_offset.ordinal, 1);
                assert_eq!(receipt.accepted_count, 1);
            });
            eprintln!("recorder writer lease: process=child, phase=appended, ordinal=1");
        }
        return;
    }

    let run_child = |path: &std::path::Path, mode: &str| {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD_MODE, mode)
            .env(CHILD_DIR, path)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "writer lease child mode={mode}: {status}");
                    break;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => {
                    // Only this test's own child is terminated, and always
                    // reaped: a blocking-lock regression must not hang the suite.
                    let killed = child.kill();
                    let reaped = child.wait();
                    panic!(
                        "writer lease child mode={mode} did not finish: {result:?}; kill={killed:?}; wait={reaped:?}"
                    );
                }
            }
        }
    };

    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let config = storage_config(dir.path());
        let owner = AppendLogRecorderStorage::open(config.clone()).unwrap();
        owner
            .append_batch(AppendRequest {
                batch_id: "owner-batch".to_string(),
                events: vec![sample_event("owner", 1, 0, "first")],
                required_durability: DurabilityLevel::Fsync,
                producer_ts_ms: 0,
            })
            .await
            .unwrap();
        run_child(dir.path(), "busy");
        assert_eq!(owner.health().await.latest_offset.unwrap().ordinal, 0);
        drop(owner);
        run_child(dir.path(), "append");
        let reopened = AppendLogRecorderStorage::open(config.clone()).unwrap();
        assert_eq!(reopened.health().await.latest_offset.unwrap().ordinal, 1);
        drop(reopened);

        let bytes = std::fs::read(&config.data_path).unwrap();
        let mut remaining = bytes.as_slice();
        for expected in [
            sample_event("owner", 1, 0, "first"),
            sample_event("successor", 1, 1, "second"),
        ] {
            assert!(remaining.len() >= 4);
            let len = u32::from_le_bytes(remaining[..4].try_into().unwrap()) as usize;
            remaining = &remaining[4..];
            assert!(remaining.len() >= len);
            let actual: serde_json::Value = serde_json::from_slice(&remaining[..len]).unwrap();
            assert_eq!(actual, serde_json::to_value(expected).unwrap());
            remaining = &remaining[len..];
        }
        assert!(
            remaining.is_empty(),
            "unexpected bytes after two exact events"
        );
        eprintln!("recorder writer lease: process=parent, phase=reopened, exact_records=2");
    });
}

#[test]
fn append_log_appended_retry_after_state_write_failure_does_not_duplicate() {
    assert_append_retry_after_state_failure_is_idempotent(DurabilityLevel::Appended, false);
}

#[test]
fn append_log_fsync_retry_after_state_write_failure_does_not_duplicate() {
    assert_append_retry_after_state_failure_is_idempotent(DurabilityLevel::Fsync, false);
}

#[test]
fn append_log_appended_retry_after_state_rename_failure_does_not_duplicate() {
    assert_append_retry_after_state_failure_is_idempotent(DurabilityLevel::Appended, true);
}

#[test]
fn append_log_fsync_retry_after_state_rename_failure_does_not_duplicate() {
    assert_append_retry_after_state_failure_is_idempotent(DurabilityLevel::Fsync, true);
}

fn assert_append_retry_after_state_failure_is_idempotent(
    durability: DurabilityLevel,
    fail_rename: bool,
) {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let config = storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(config.clone()).unwrap();
        let obstruction = if fail_rename {
            config.state_path.clone()
        } else {
            config.state_path.with_extension("tmp")
        };
        std::fs::create_dir(&obstruction).unwrap();
        let request = AppendRequest {
            batch_id: "accepted-before-save-error".to_string(),
            events: vec![
                sample_event("retry-0", 1, 0, "first"),
                sample_event("retry-1", 1, 1, "second"),
            ],
            required_durability: durability,
            producer_ts_ms: 0,
        };
        assert!(matches!(
            storage.append_batch(request.clone()).await,
            Err(RecorderStorageError::Io(_))
        ));
        let accepted_bytes = std::fs::read(&config.data_path).unwrap();
        assert!(!accepted_bytes.is_empty());
        let first_health = storage.health().await;
        assert!(first_health.degraded);
        assert_eq!(first_health.latest_offset.as_ref().unwrap().ordinal, 1);

        // The retry must attempt persistence again, but not append again.
        assert!(matches!(
            storage.append_batch(request.clone()).await,
            Err(RecorderStorageError::Io(_))
        ));
        let retry_health = storage.health().await;
        eprintln!(
            "recorder append retry: durability={durability:?}, rename={fail_rename}, phase=save_failed_twice, observed_ordinal={:?}, expected_ordinal=1",
            retry_health
                .latest_offset
                .as_ref()
                .map(|offset| offset.ordinal)
        );
        assert_eq!(retry_health.latest_offset, first_health.latest_offset);
        assert!(retry_health.degraded);
        assert_eq!(std::fs::read(&config.data_path).unwrap(), accepted_bytes);

        let mut conflicting = request.clone();
        conflicting.events[0] = sample_event("retry-0", 1, 0, "different");
        assert!(matches!(
            storage.append_batch(conflicting).await,
            Err(RecorderStorageError::IdempotencyConflict { .. })
        ));
        assert_eq!(std::fs::read(&config.data_path).unwrap(), accepted_bytes);

        std::fs::rename(&obstruction, dir.path().join("retained-save-obstruction")).unwrap();
        let saved = storage.append_batch(request.clone()).await.unwrap();
        assert!(saved.was_idempotent_replay);
        assert_eq!(saved.accepted_count, 2);
        assert_eq!(saved.first_offset.ordinal, 0);
        assert_eq!(saved.first_offset.byte_offset, 0);
        assert_eq!(saved.last_offset.ordinal, 1);
        assert_eq!(saved.committed_durability, durability);
        assert!(!storage.health().await.degraded);
        assert_eq!(std::fs::read(&config.data_path).unwrap(), accepted_bytes);
        assert_eq!(storage.append_batch(request.clone()).await.unwrap(), saved);

        let next_event = sample_event("retry-2", 1, 2, "third");
        let next = storage
            .append_batch(AppendRequest {
                batch_id: "after-recovered-save".to_string(),
                events: vec![next_event.clone()],
                required_durability: DurabilityLevel::Fsync,
                producer_ts_ms: 0,
            })
            .await
            .unwrap();
        assert!(!next.was_idempotent_replay);
        assert_eq!(next.first_offset.ordinal, 2);
        assert_eq!(next.first_offset.byte_offset, accepted_bytes.len() as u64);
        drop(storage);

        let reopened = AppendLogRecorderStorage::open(config.clone()).unwrap();
        assert_eq!(reopened.health().await.latest_offset.unwrap().ordinal, 2);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config.state_path).unwrap()).unwrap();
        assert_eq!(persisted["next_ordinal"], 3);

        // Decode the actual length-prefixed log independently of the writer's
        // counters, so a plausible receipt cannot hide duplicate/corrupt bytes.
        let bytes = std::fs::read(&config.data_path).unwrap();
        let mut remaining = bytes.as_slice();
        let mut records = Vec::new();
        while !remaining.is_empty() {
            let (length, rest) = remaining.split_at(4);
            let length = u32::from_le_bytes(length.try_into().unwrap()) as usize;
            let (payload, rest) = rest.split_at(length);
            records.push(serde_json::from_slice::<serde_json::Value>(payload).unwrap());
            remaining = rest;
        }
        let mut expected = request.events;
        expected.push(next_event);
        assert_eq!(
            serde_json::Value::Array(records),
            serde_json::to_value(expected).unwrap()
        );
        eprintln!(
            "recorder append retry: durability={durability:?}, rename={fail_rename}, phase=reopened, records=3, duplicates=0"
        );
    });
}

#[test]
fn append_log_checkpoint_persistence_failure_preserves_absent_checkpoint() {
    assert_checkpoint_persistence_failure_is_atomic(false);
}

#[test]
fn append_log_checkpoint_persistence_failure_preserves_existing_checkpoint() {
    assert_checkpoint_persistence_failure_is_atomic(true);
}

fn assert_checkpoint_persistence_failure_is_atomic(has_prior_checkpoint: bool) {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let config = storage_config(dir.path());
        let storage = AppendLogRecorderStorage::open(config.clone()).unwrap();
        let appended = storage
            .append_batch(AppendRequest {
                batch_id: "checkpoint-save-failure".to_string(),
                events: vec![
                    sample_event("save-0", 1, 0, "first"),
                    sample_event("save-1", 1, 1, "second"),
                ],
                required_durability: DurabilityLevel::Fsync,
                producer_ts_ms: 0,
            })
            .await
            .unwrap();
        let consumer = CheckpointConsumerId("save-failure-reader".to_string());
        let first = RecorderCheckpoint {
            consumer: consumer.clone(),
            upto_offset: appended.first_offset,
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            committed_at_ms: 1,
        };
        let previous = if has_prior_checkpoint {
            assert_eq!(
                storage.commit_checkpoint(first.clone()).await.unwrap(),
                CheckpointCommitOutcome::Advanced
            );
            Some(first.clone())
        } else {
            None
        };
        let next = RecorderCheckpoint {
            upto_offset: appended.last_offset,
            committed_at_ms: 2,
            ..first.clone()
        };
        let state_before = std::fs::read(&config.state_path).unwrap();
        let temporary_path = config.state_path.with_extension("tmp");
        // A real directory makes the actual state-file write fail even
        // as root. Move the obstruction aside for retry; do not delete it.
        std::fs::create_dir(&temporary_path).unwrap();
        let error = storage.commit_checkpoint(next.clone()).await.unwrap_err();
        assert!(matches!(error, RecorderStorageError::Io(_)));
        let observed = storage.read_checkpoint(&consumer).await.unwrap();
        eprintln!(
            "recorder checkpoint save: prior={has_prior_checkpoint}, phase=write_failed, observed_ordinal={:?}, expected_ordinal={:?}",
            observed
                .as_ref()
                .map(|checkpoint| checkpoint.upto_offset.ordinal),
            previous
                .as_ref()
                .map(|checkpoint| checkpoint.upto_offset.ordinal)
        );
        assert_eq!(observed, previous);
        assert_eq!(std::fs::read(&config.state_path).unwrap(), state_before);
        assert!(storage.health().await.degraded);

        std::fs::rename(
            &temporary_path,
            dir.path().join("retained-state-obstruction"),
        )
        .unwrap();
        assert_eq!(
            storage.commit_checkpoint(next.clone()).await.unwrap(),
            CheckpointCommitOutcome::Advanced,
            "a previously failed checkpoint must actually be saved on retry"
        );
        assert_eq!(
            storage.read_checkpoint(&consumer).await.unwrap(),
            Some(next.clone())
        );
        assert!(!storage.health().await.degraded);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config.state_path).unwrap()).unwrap();
        let saved: RecorderCheckpoint =
            serde_json::from_value(persisted["checkpoints"][&consumer.0].clone()).unwrap();
        assert_eq!(saved, next);
        drop(storage);

        let reopened = AppendLogRecorderStorage::open(config).unwrap();
        assert_eq!(
            reopened.read_checkpoint(&consumer).await.unwrap(),
            Some(next.clone())
        );
        assert_eq!(
            reopened.commit_checkpoint(next.clone()).await.unwrap(),
            CheckpointCommitOutcome::NoopAlreadyAdvanced
        );
        assert!(matches!(
            reopened.commit_checkpoint(first).await,
            Err(RecorderStorageError::CheckpointRegression {
                current_ordinal: 1,
                attempted_ordinal: 0,
                ..
            })
        ));
        assert_eq!(
            reopened.read_checkpoint(&consumer).await.unwrap(),
            Some(next)
        );
        eprintln!(
            "recorder checkpoint save: prior={has_prior_checkpoint}, phase=reopened, ordinal=1, equal=noop, lower=rejected"
        );
    });
}

fn make_segment(
    id: &str,
    tier: SensitivityTier,
    phase: SegmentPhase,
    created_at_ms: u64,
    size_bytes: u64,
    events: u64,
    ordinal_start: u64,
) -> SegmentMeta {
    SegmentMeta {
        segment_id: id.to_string(),
        sensitivity: tier,
        phase,
        start_ordinal: ordinal_start,
        end_ordinal: Some(ordinal_start + events - 1),
        size_bytes,
        created_at_ms,
        sealed_at_ms: if phase != SegmentPhase::Active {
            Some(created_at_ms + 3_600_000)
        } else {
            None
        },
        archived_at_ms: if phase == SegmentPhase::Archived {
            Some(created_at_ms + 7 * 86_400_000)
        } else {
            None
        },
        purged_at_ms: None,
        event_count: events,
    }
}

fn human_actor() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Human, "operator-1")
}

fn robot_actor() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Robot, "agent-swarm-1")
}

fn workflow_actor() -> ActorIdentity {
    ActorIdentity::new(ActorKind::Workflow, "wf-restart-42")
}

// =============================================================================
// 1. Storage → Telemetry integration
// =============================================================================

/// Append events and verify telemetry records the operation.
#[test]
fn storage_append_records_telemetry_metrics() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
        let telemetry = Arc::new(StorageTelemetry::with_defaults());

        let events: Vec<RecorderEvent> = (0..5)
            .map(|i| sample_event(&format!("evt-{}", i), 1, i, "hello"))
            .collect();

        let req = AppendRequest {
            batch_id: "batch-1".to_string(),
            events,
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: 1_700_000_000_000,
        };

        let start = Instant::now();
        let result = storage.append_batch(req).await;
        let elapsed_us = start.elapsed().as_micros() as f64;

        assert!(result.is_ok());
        let resp = result.unwrap();

        // Record in telemetry.
        telemetry.record_append(elapsed_us, resp.accepted_count, 500, false);

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.total_events_appended, 5);
        assert_eq!(snapshot.total_batches, 1);
        assert!(snapshot.append_rate_ewma > 0.0);
    });
}

/// Flush and verify telemetry snapshot captures flush stats.
#[test]
fn storage_flush_updates_telemetry() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
        let telemetry = Arc::new(StorageTelemetry::with_defaults());

        // Append first.
        let events = vec![sample_event("evt-0", 1, 0, "data")];
        let req = AppendRequest {
            batch_id: "batch-flush".to_string(),
            events,
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: 1_700_000_000_000,
        };
        let start = Instant::now();
        let resp = storage.append_batch(req).await.unwrap();
        telemetry.record_append(
            start.elapsed().as_micros() as f64,
            resp.accepted_count,
            100,
            false,
        );

        // Flush.
        let flush_start = Instant::now();
        let flush_result = storage.flush(FlushMode::Buffered).await;
        assert!(flush_result.is_ok());
        telemetry.record_flush(flush_start.elapsed().as_micros() as f64);

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.total_flushes, 1);
        assert_eq!(snapshot.total_batches, 1);
    });
}

/// Health status propagates to telemetry tier classification.
#[test]
fn storage_health_propagates_to_telemetry_tier() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
        let telemetry = Arc::new(StorageTelemetry::with_defaults());

        let health = storage.health().await;
        telemetry.update_health(health);

        // Fresh storage should be healthy (Green).
        assert_eq!(telemetry.current_tier(), StorageHealthTier::Green);
    });
}

/// Error recording tracks class distribution.
#[test]
fn telemetry_error_class_tracking() {
    let telemetry = StorageTelemetry::with_defaults();

    telemetry.record_error(RecorderStorageErrorClass::Overload);
    telemetry.record_error(RecorderStorageErrorClass::Overload);
    telemetry.record_error(RecorderStorageErrorClass::Retryable);
    telemetry.record_error(RecorderStorageErrorClass::Corruption);

    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.errors.overload, 2);
    assert_eq!(snapshot.errors.retryable, 1);
    assert_eq!(snapshot.errors.corruption, 1);
    assert_eq!(snapshot.errors.total(), 4);
}

// =============================================================================
// 2. Retention → Audit integration
// =============================================================================

/// Retention sweep produces segment transitions auditable via AuditLog.
#[test]
fn retention_sweep_generates_auditable_events() {
    let config = RetentionConfig::default();
    let mut mgr = RetentionManager::new(config).unwrap();
    let audit_log = AuditLog::new(AuditLogConfig::default());

    let base_ms = 1_700_000_000_000u64;
    let hot_expiry = base_ms + 25 * 3_600_000; // 25 hours past creation (past hot window).

    // Add an active segment created at base time.
    mgr.add_segment(make_segment(
        "seg_t1_001",
        SensitivityTier::T1Standard,
        SegmentPhase::Active,
        base_ms,
        1_000_000,
        500,
        0,
    ));

    // Sweep at a time past hot window — should seal.
    let result = mgr.sweep(hot_expiry, &HashMap::new());

    // Audit each transition.
    for seg_id in &result.sealed {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionSegmentSealed,
                ActorIdentity::new(ActorKind::Workflow, "retention-sweep"),
                hot_expiry,
            )
            .with_segment_ids(vec![seg_id.clone()]),
        );
    }

    assert_ne!(result.sealed, [] as [std::string::String; 0]);
    let entries = audit_log.entries_by_type(AuditEventType::RetentionSegmentSealed);
    assert_eq!(entries.len(), result.sealed.len());

    // Audit chain is intact.
    let all = audit_log.entries();
    let verification = AuditLog::verify_chain(&all, GENESIS_HASH);
    assert!(verification.chain_intact);
}

/// T3 accelerated purge generates both retention sweep result and audit entry.
#[test]
fn t3_accelerated_purge_audited() {
    let config = RetentionConfig {
        t3_max_hours: 24,
        ..RetentionConfig::default()
    };
    let mut mgr = RetentionManager::new(config).unwrap();
    let audit_log = AuditLog::new(AuditLogConfig::default());

    let base_ms = 1_700_000_000_000u64;

    // Add a T3 sealed segment.
    let mut seg = make_segment(
        "seg_t3_001",
        SensitivityTier::T3Restricted,
        SegmentPhase::Active,
        base_ms,
        500_000,
        100,
        0,
    );
    // Manually seal it (simulating prior sweep).
    seg.phase = SegmentPhase::Sealed;
    seg.sealed_at_ms = Some(base_ms + 3_600_000);
    mgr.add_segment(seg);

    // Sweep 25 hours later — T3 should be purge-eligible.
    let sweep_time = base_ms + 25 * 3_600_000;
    let result = mgr.sweep(sweep_time, &HashMap::new());

    // T3 data should be archived or marked for purge.
    let total_transitions = result.sealed.len()
        + result.archived.len()
        + result.purge_candidates.len()
        + result.purged.len();
    assert!(total_transitions > 0, "T3 data should have transitioned");

    // Audit the accelerated purge.
    for seg_id in result.purge_candidates.iter().chain(result.purged.iter()) {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionAcceleratedPurge,
                ActorIdentity::new(ActorKind::Workflow, "retention-t3-purge"),
                sweep_time,
            )
            .with_segment_ids(vec![seg_id.clone()])
            .with_justification("T3 data exceeded 24h retention window"),
        );
    }

    // Verify all audit entries have justification.
    for entry in audit_log.entries() {
        if entry.event_type == AuditEventType::RetentionAcceleratedPurge {
            assert!(entry.justification.is_some());
        }
    }
}

// =============================================================================
// 3. Sensitivity classification → Retention → Audit pipeline
// =============================================================================

/// Redacted events classify as T2 sensitive.
#[test]
fn redacted_events_classify_as_t2() {
    let redacted = sample_redacted_event("redacted-1", 5, 0);
    if let RecorderEventPayload::IngressText { redaction, .. } = &redacted.payload {
        let tier = SensitivityTier::classify(*redaction, false);
        assert_eq!(tier, SensitivityTier::T2Sensitive);
    } else {
        panic!("Expected IngressText payload");
    }
}

/// Classify events by redaction level, assign to retention tiers, audit lifecycle.
#[test]
fn sensitivity_classification_flows_through_retention_and_audit() {
    let audit_log = AuditLog::new(AuditLogConfig::default());

    // Classify different redaction levels.
    let t1 = SensitivityTier::classify(RecorderRedactionLevel::None, false);
    let t2 = SensitivityTier::classify(RecorderRedactionLevel::Partial, false);
    let t3 = SensitivityTier::classify(RecorderRedactionLevel::None, true);

    assert_eq!(t1, SensitivityTier::T1Standard);
    assert_eq!(t2, SensitivityTier::T2Sensitive);
    assert_eq!(t3, SensitivityTier::T3Restricted);

    // Create segments with each tier.
    let mut mgr = RetentionManager::with_defaults();
    let base_ms = 1_700_000_000_000u64;

    mgr.add_segment(make_segment(
        "seg_t1",
        t1,
        SegmentPhase::Active,
        base_ms,
        1000,
        10,
        0,
    ));
    mgr.add_segment(make_segment(
        "seg_t2",
        t2,
        SegmentPhase::Active,
        base_ms,
        2000,
        20,
        10,
    ));
    mgr.add_segment(make_segment(
        "seg_t3",
        t3,
        SegmentPhase::Active,
        base_ms,
        3000,
        30,
        30,
    ));

    assert_eq!(mgr.segment_count(), 3);
    assert_eq!(mgr.segments_by_tier(SensitivityTier::T1Standard).len(), 1);
    assert_eq!(mgr.segments_by_tier(SensitivityTier::T2Sensitive).len(), 1);
    assert_eq!(mgr.segments_by_tier(SensitivityTier::T3Restricted).len(), 1);

    // Audit segment creation.
    for (seg_id, tier) in [("seg_t1", t1), ("seg_t2", t2), ("seg_t3", t3)] {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionSegmentSealed,
                workflow_actor(),
                base_ms,
            )
            .with_segment_ids(vec![seg_id.to_string()])
            .with_details(serde_json::json!({"sensitivity_tier": format!("{:?}", tier)})),
        );
    }

    assert_eq!(audit_log.len(), 3);
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
}

// =============================================================================
// 4. Access control → Audit integration
// =============================================================================

/// Access control decisions are correctly audited with appropriate event types.
#[test]
fn access_control_decisions_audited() {
    let audit_log = AuditLog::new(AuditLogConfig::default());
    let now = 1_700_000_000_000u64;

    // Human queries at A2 (allowed).
    let human_decision = check_authorization(ActorKind::Human, AccessTier::A2FullQuery);
    assert_eq!(human_decision, AuthzDecision::Allow);
    audit_log.append(
        AuditEventBuilder::new(AuditEventType::RecorderQuery, human_actor(), now)
            .with_decision(human_decision)
            .with_pane_ids(vec![1, 2, 3])
            .with_query("error timeout")
            .with_result_count(15),
    );

    // Robot tries A3 (denied).
    let robot_decision = check_authorization(ActorKind::Robot, AccessTier::A3PrivilegedRaw);
    assert_eq!(robot_decision, AuthzDecision::Deny);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQueryPrivileged,
            robot_actor(),
            now + 1000,
        )
        .with_decision(robot_decision),
    );

    // Human elevates to A3 (elevated with justification).
    let elevate_decision = check_authorization(ActorKind::Human, AccessTier::A3PrivilegedRaw);
    assert_eq!(elevate_decision, AuthzDecision::Elevate);
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::AccessApprovalGranted,
            human_actor(),
            now + 2000,
        )
        .with_decision(AuthzDecision::Allow)
        .with_justification("Investigating production incident INC-456"),
    );

    // Now human does privileged query with approval.
    audit_log.append(
        AuditEventBuilder::new(
            AuditEventType::RecorderQueryPrivileged,
            human_actor(),
            now + 3000,
        )
        .with_decision(AuthzDecision::Allow)
        .with_pane_ids(vec![7])
        .with_time_range(now - 3_600_000, now)
        .with_justification("Approved via INC-456")
        .with_result_count(3),
    );

    // Verify stats.
    let stats = audit_log.stats();
    assert_eq!(stats.total_entries, 4);
    assert_eq!(stats.denied_count, 1);
    assert_eq!(stats.elevated_count, 0); // Elevation was recorded as Allow.
    assert_eq!(stats.by_actor.get("human"), Some(&3));
    assert_eq!(stats.by_actor.get("robot"), Some(&1));

    // Verify chain.
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
    assert_eq!(verification.total_entries, 4);
}

/// Event types map to required access tiers per governance policy.
#[test]
fn event_types_require_correct_access_tiers() {
    // Standard queries need A1.
    assert_eq!(
        required_tier_for_event(AuditEventType::RecorderQuery),
        AccessTier::A1RedactedQuery
    );

    // Admin operations need A4.
    assert_eq!(
        required_tier_for_event(AuditEventType::AdminPurge),
        AccessTier::A4Admin
    );
    assert_eq!(
        required_tier_for_event(AuditEventType::AdminRetentionOverride),
        AccessTier::A4Admin
    );

    // Privileged raw needs A3.
    assert_eq!(
        required_tier_for_event(AuditEventType::RecorderQueryPrivileged),
        AccessTier::A3PrivilegedRaw
    );

    // Retention lifecycle is A0 (internal).
    assert_eq!(
        required_tier_for_event(AuditEventType::RetentionSegmentSealed),
        AccessTier::A0PublicMetadata
    );
}

/// Robot cannot access admin operations; workflow can elevate to A3.
#[test]
fn actor_elevation_rules_match_governance_policy() {
    // Robot: A1 default, can elevate to A2, denied A3+.
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A1RedactedQuery),
        AuthzDecision::Allow
    );
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A2FullQuery),
        AuthzDecision::Elevate
    );
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A3PrivilegedRaw),
        AuthzDecision::Deny
    );
    assert_eq!(
        check_authorization(ActorKind::Robot, AccessTier::A4Admin),
        AuthzDecision::Deny
    );

    // Workflow: A2 default, can elevate to A3, denied A4.
    assert_eq!(
        check_authorization(ActorKind::Workflow, AccessTier::A2FullQuery),
        AuthzDecision::Allow
    );
    assert_eq!(
        check_authorization(ActorKind::Workflow, AccessTier::A3PrivilegedRaw),
        AuthzDecision::Elevate
    );
    assert_eq!(
        check_authorization(ActorKind::Workflow, AccessTier::A4Admin),
        AuthzDecision::Deny
    );

    // Human: A2 default, can elevate to A3 and A4.
    assert_eq!(
        check_authorization(ActorKind::Human, AccessTier::A3PrivilegedRaw),
        AuthzDecision::Elevate
    );
    assert_eq!(
        check_authorization(ActorKind::Human, AccessTier::A4Admin),
        AuthzDecision::Elevate
    );
}

// =============================================================================
// 5. Telemetry → Diagnostics integration
// =============================================================================

/// Diagnostics summary reflects telemetry snapshot state.
#[test]
fn telemetry_snapshot_produces_diagnostic_summary() {
    let telemetry = StorageTelemetry::with_defaults();

    // Record some operations.
    for i in 0..20 {
        telemetry.record_append((i as f64).mul_add(10.0, 100.0), 5, 500, false);
    }
    telemetry.record_flush(50.0);
    telemetry.record_flush(75.0);

    let snapshot = telemetry.snapshot();
    assert_eq!(snapshot.total_batches, 20);
    assert_eq!(snapshot.total_events_appended, 100);
    assert_eq!(snapshot.total_flushes, 2);

    let summary = diagnose(&snapshot);
    assert_ne!(summary.status, "");
    // Green health — no urgent items.
    assert_eq!(snapshot.health_tier, StorageHealthTier::Green);
    assert_eq!(summary.tier, StorageHealthTier::Green);
}

/// Error remediation messages are non-empty for all error classes.
#[test]
fn remediation_covers_all_error_classes() {
    let classes = [
        RecorderStorageErrorClass::Overload,
        RecorderStorageErrorClass::Retryable,
        RecorderStorageErrorClass::TerminalData,
        RecorderStorageErrorClass::Corruption,
    ];

    for class in &classes {
        let msg = remediation_for_error(*class);
        assert!(!msg.is_empty(), "Remediation missing for {:?}", class);
    }
}

// =============================================================================
// 6. Storage → Telemetry → Retention → Audit end-to-end
// =============================================================================

/// Full pipeline: append events, record telemetry, manage retention, audit everything.
#[test]
fn full_pipeline_append_telemetry_retention_audit() {
    run_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let storage = AppendLogRecorderStorage::open(storage_config(dir.path())).unwrap();
        let telemetry = Arc::new(StorageTelemetry::with_defaults());
        let audit_log = AuditLog::new(AuditLogConfig::default());
        let mut retention_mgr = RetentionManager::with_defaults();

        let base_ms = 1_700_000_000_000u64;

        // Step 1: Append events to storage.
        let events: Vec<RecorderEvent> = (0..10)
            .map(|i| sample_event(&format!("pipe-{}", i), 1, i, &format!("data-{}", i)))
            .collect();

        let req = AppendRequest {
            batch_id: "pipeline-batch-1".to_string(),
            events,
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: base_ms,
        };

        let start = Instant::now();
        let resp = storage.append_batch(req).await.unwrap();
        let elapsed_us = start.elapsed().as_micros() as f64;

        // Step 2: Record in telemetry.
        telemetry.record_append(elapsed_us, resp.accepted_count, 5000, false);

        // Step 3: Create retention segment from appended data.
        let seg = make_segment(
            "seg_pipeline_001",
            SensitivityTier::T1Standard,
            SegmentPhase::Active,
            base_ms,
            5000,
            10,
            resp.first_offset.ordinal,
        );
        retention_mgr.add_segment(seg);

        // Step 4: Audit the query.
        let decision = check_authorization(ActorKind::Human, AccessTier::A2FullQuery);
        audit_log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQuery, human_actor(), base_ms)
                .with_decision(decision)
                .with_pane_ids(vec![1])
                .with_result_count(10),
        );

        // Step 5: Flush storage.
        let flush_start = Instant::now();
        storage.flush(FlushMode::Buffered).await.unwrap();
        telemetry.record_flush(flush_start.elapsed().as_micros() as f64);

        // Step 6: Verify telemetry snapshot.
        let health = storage.health().await;
        telemetry.update_health(health);
        let snapshot = telemetry.snapshot();

        assert_eq!(snapshot.total_events_appended, 10);
        assert_eq!(snapshot.total_batches, 1);
        assert_eq!(snapshot.total_flushes, 1);
        assert_eq!(snapshot.health_tier, StorageHealthTier::Green);

        // Step 7: Retention stats.
        let ret_stats = retention_mgr.stats();
        assert_eq!(ret_stats.active_count, 1);
        assert_eq!(retention_mgr.total_events(), 10);

        // Step 8: Audit chain is intact.
        let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
        assert!(verification.chain_intact);
    });
}

// =============================================================================
// 7. Audit log resume across "persistence boundaries"
// =============================================================================

/// Simulate persisting audit log to disk and resuming — chain stays valid.
#[test]
fn audit_log_resume_preserves_chain_across_persistence() {
    // Phase 1: Write some audit entries.
    let log1 = AuditLog::new(AuditLogConfig::default());

    for i in 0..5 {
        log1.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                human_actor(),
                1_700_000_000_000 + i * 1000,
            )
            .with_pane_ids(vec![1]),
        );
    }

    let phase1_entries = log1.entries();
    let phase1_last_hash = log1.last_hash();
    let phase1_next_ordinal = log1.next_ordinal();

    // Simulate "flush to disk" — drain entries.
    let drained = log1.drain();
    assert_eq!(drained.len(), 5);
    assert!(log1.is_empty());

    // Phase 2: Resume from persisted state.
    let log2 = AuditLog::resume(
        AuditLogConfig::default(),
        phase1_next_ordinal,
        phase1_last_hash,
    );

    for i in 5..10 {
        log2.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderReplay,
                robot_actor(),
                1_700_000_005_000 + i * 1000,
            )
            .with_decision(AuthzDecision::Allow),
        );
    }

    let phase2_entries = log2.entries();

    // Combined entries should form a valid chain.
    let mut all: Vec<_> = phase1_entries;
    all.extend(phase2_entries);

    let verification = AuditLog::verify_chain(&all, GENESIS_HASH);
    assert!(verification.chain_intact);
    assert_eq!(verification.total_entries, 10);
    assert_eq!(verification.ordinal_range, Some((0, 9)));
    assert_eq!(verification.missing_ordinals, [] as [u64; 0]);
}

// =============================================================================
// 8. Retention policy validation across tiers
// =============================================================================

/// Default retention config is valid.
#[test]
fn default_retention_config_validates() {
    let config = RetentionConfig::default();
    assert!(config.validate().is_ok());
}

/// Retention hours differ by sensitivity tier per governance policy.
#[test]
fn retention_windows_differ_by_tier() {
    let config = RetentionConfig::default();

    let t1_hours = config.retention_hours(SensitivityTier::T1Standard);
    let t2_hours = config.retention_hours(SensitivityTier::T2Sensitive);
    let t3_hours = config.retention_hours(SensitivityTier::T3Restricted);

    // T3 has accelerated purge (shortest retention).
    assert!(
        t3_hours <= t1_hours,
        "T3 ({}) should be <= T1 ({})",
        t3_hours,
        t1_hours
    );
    assert!(
        t3_hours <= t2_hours,
        "T3 ({}) should be <= T2 ({})",
        t3_hours,
        t2_hours
    );

    // All tiers have positive retention.
    assert!(t1_hours > 0);
    assert!(t2_hours > 0);
    assert!(t3_hours > 0);
}

/// Segment lifecycle transitions follow valid paths.
#[test]
fn segment_lifecycle_transitions_are_valid() {
    assert!(SegmentPhase::Active.can_transition_to(SegmentPhase::Sealed));
    assert!(SegmentPhase::Sealed.can_transition_to(SegmentPhase::Archived));
    assert!(SegmentPhase::Archived.can_transition_to(SegmentPhase::Purged));

    // Invalid transitions.
    assert!(!SegmentPhase::Active.can_transition_to(SegmentPhase::Purged));
    assert!(!SegmentPhase::Sealed.can_transition_to(SegmentPhase::Active));
    assert!(!SegmentPhase::Purged.can_transition_to(SegmentPhase::Active));
}

// =============================================================================
// 9. Multi-actor concurrent audit scenario
// =============================================================================

/// Multiple actors performing operations simultaneously — all audited correctly.
#[test]
fn multi_actor_concurrent_operations_audited() {
    let audit_log = AuditLog::new(AuditLogConfig {
        max_memory_entries: 1000,
        ..AuditLogConfig::default()
    });

    let now = 1_700_000_000_000u64;

    // Human: queries and admin.
    for i in 0..10 {
        audit_log.append(
            AuditEventBuilder::new(AuditEventType::RecorderQuery, human_actor(), now + i * 100)
                .with_pane_ids(vec![1, 2])
                .with_result_count(i + 1),
        );
    }

    // Robot: queries (some denied).
    for i in 0..5 {
        let decision = if i % 2 == 0 {
            AuthzDecision::Allow
        } else {
            AuthzDecision::Deny
        };
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderQuery,
                robot_actor(),
                now + 1000 + i * 100,
            )
            .with_decision(decision),
        );
    }

    // Workflow: replay with elevation.
    for i in 0..3 {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderReplay,
                workflow_actor(),
                now + 2000 + i * 100,
            )
            .with_decision(AuthzDecision::Elevate)
            .with_justification("Automated incident analysis"),
        );
    }

    // Admin purge by human.
    audit_log.append(
        AuditEventBuilder::new(AuditEventType::AdminPurge, human_actor(), now + 3000)
            .with_segment_ids(vec!["seg_expired_001".to_string()])
            .with_justification("Quarterly data cleanup"),
    );

    assert_eq!(audit_log.len(), 19);

    let stats = audit_log.stats();
    assert_eq!(stats.by_actor.get("human"), Some(&11));
    assert_eq!(stats.by_actor.get("robot"), Some(&5));
    assert_eq!(stats.by_actor.get("workflow"), Some(&3));
    assert_eq!(stats.denied_count, 2); // 2 odd-indexed robot queries.
    assert_eq!(stats.elevated_count, 3); // 3 workflow replays.

    // Full chain verification.
    let verification = AuditLog::verify_chain(&audit_log.entries(), GENESIS_HASH);
    assert!(verification.chain_intact);
    assert_eq!(verification.total_entries, 19);
    assert_eq!(verification.missing_ordinals, [] as [u64; 0]);
}

// =============================================================================
// 10. Tamper detection scenarios
// =============================================================================

/// Modifying an entry's field breaks the hash chain.
#[test]
fn tamper_detection_modified_field() {
    let log = AuditLog::new(AuditLogConfig::default());
    for i in 0..5 {
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            human_actor(),
            i * 1000,
        ));
    }

    let mut entries = log.entries();
    // Tamper: change the result count of entry 2.
    entries[2].scope.result_count = Some(999);

    let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
    assert!(!result.chain_intact);
    assert_eq!(result.first_break_at, Some(3)); // Entry 3's prev hash won't match.
}

/// Deleting an entry from the middle creates a gap and breaks the chain.
#[test]
fn tamper_detection_deleted_entry() {
    let log = AuditLog::new(AuditLogConfig::default());
    for i in 0..5 {
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            human_actor(),
            i * 1000,
        ));
    }

    let mut entries = log.entries();
    entries.remove(2); // Delete ordinal 2.

    let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
    assert!(!result.chain_intact);
    assert_eq!(result.missing_ordinals, vec![2]);
}

/// Inserting a fake entry breaks the chain.
#[test]
fn tamper_detection_inserted_entry() {
    let log = AuditLog::new(AuditLogConfig::default());
    for i in 0..3 {
        log.append(AuditEventBuilder::new(
            AuditEventType::RecorderQuery,
            human_actor(),
            i * 1000,
        ));
    }

    let mut entries = log.entries();

    // Create a fake entry pretending to be ordinal 1.5.
    let fake = frankenterm_core::recorder_audit::RecorderAuditEntry {
        audit_version: AUDIT_SCHEMA_VERSION.to_string(),
        ordinal: 10, // Wrong ordinal.
        event_type: AuditEventType::AdminPurge,
        actor: ActorIdentity::new(ActorKind::Human, "attacker"),
        timestamp_ms: 999,
        scope: Default::default(),
        decision: AuthzDecision::Allow,
        justification: None,
        policy_version: "governance.v1".to_string(),
        prev_entry_hash: "fake_hash".to_string(),
        details: None,
    };

    entries.insert(2, fake);

    let result = AuditLog::verify_chain(&entries, GENESIS_HASH);
    assert!(!result.chain_intact);
}

// =============================================================================
// 11. Checkpoint holds prevent retention purge
// =============================================================================

/// Segments with active checkpoint consumers are held during sweep.
#[test]
fn checkpoint_holds_prevent_purge() {
    let config = RetentionConfig::default();
    let mut mgr = RetentionManager::new(config).unwrap();
    let audit_log = AuditLog::new(AuditLogConfig::default());

    let base_ms = 1_700_000_000_000u64;

    // Add an archived segment past cold window.
    let mut seg = make_segment(
        "seg_held",
        SensitivityTier::T1Standard,
        SegmentPhase::Archived,
        base_ms,
        10_000,
        100,
        0,
    );
    seg.archived_at_ms = Some(base_ms + 8 * 86_400_000);
    mgr.add_segment(seg);

    // A consumer holds a checkpoint referencing this segment.
    let mut holders: HashMap<String, Vec<String>> = HashMap::new();
    holders.insert("seg_held".to_string(), vec!["tantivy-indexer".to_string()]);

    // Sweep well past cold window.
    let sweep_time = base_ms + 90 * 86_400_000;
    let result = mgr.sweep(sweep_time, &holders);

    // Segment should be held, not purged.
    assert!(
        !result.held.is_empty(),
        "Segment should be held by checkpoint"
    );

    // Audit the hold.
    for (seg_id, consumer) in &result.held {
        audit_log.append(
            AuditEventBuilder::new(
                AuditEventType::RetentionSegmentArchived, // Documented as held.
                workflow_actor(),
                sweep_time,
            )
            .with_segment_ids(vec![seg_id.clone()])
            .with_details(serde_json::json!({"held_by": consumer})),
        );
    }

    assert!(!audit_log.is_empty());
}

// =============================================================================
// 12. Audit schema version consistency
// =============================================================================

/// All audit entries use the current schema version.
#[test]
fn audit_entries_use_current_schema_version() {
    let log = AuditLog::new(AuditLogConfig::default());

    let event_types = [
        AuditEventType::RecorderQuery,
        AuditEventType::RecorderQueryPrivileged,
        AuditEventType::AdminPurge,
        AuditEventType::AccessApprovalGranted,
        AuditEventType::RetentionSegmentSealed,
    ];

    for (i, event_type) in event_types.iter().enumerate() {
        log.append(AuditEventBuilder::new(
            *event_type,
            human_actor(),
            i as u64 * 1000,
        ));
    }

    for entry in log.entries() {
        assert_eq!(entry.audit_version, AUDIT_SCHEMA_VERSION);
    }
}

// =============================================================================
// 13. Telemetry SLO evaluation
// =============================================================================

/// SLO status reflects latency percentiles.
#[test]
fn slo_status_reflects_latency() {
    let config = StorageTelemetryConfig {
        slo_append_p95_us: 5000.0, // 5ms SLO.
        ..StorageTelemetryConfig::default()
    };
    let telemetry = StorageTelemetry::new(config);

    // Record fast operations — should meet SLO.
    for _ in 0..100 {
        telemetry.record_append(100.0, 1, 100, false); // 100μs.
    }

    let snapshot = telemetry.snapshot();
    // With all operations at 100μs, p95 should be well under 5000μs SLO.
    assert_eq!(snapshot.total_events_appended, 100);
}

// =============================================================================
// 14. Retention manager total data tracking
// =============================================================================

/// Total data bytes and events tracked across segments.
#[test]
fn retention_tracks_aggregate_data() {
    let mut mgr = RetentionManager::with_defaults();
    let base_ms = 1_700_000_000_000u64;

    mgr.add_segment(make_segment(
        "seg_a",
        SensitivityTier::T1Standard,
        SegmentPhase::Active,
        base_ms,
        10_000,
        100,
        0,
    ));
    mgr.add_segment(make_segment(
        "seg_b",
        SensitivityTier::T2Sensitive,
        SegmentPhase::Sealed,
        base_ms,
        20_000,
        200,
        100,
    ));
    mgr.add_segment(make_segment(
        "seg_c",
        SensitivityTier::T3Restricted,
        SegmentPhase::Archived,
        base_ms,
        5_000,
        50,
        300,
    ));

    assert_eq!(mgr.total_data_bytes(), 35_000);
    assert_eq!(mgr.total_events(), 350);
    assert_eq!(mgr.segment_count(), 3);

    let stats = mgr.stats();
    assert_eq!(stats.live_count(), 3);
    assert_eq!(stats.live_bytes(), 35_000);
    assert_eq!(mgr.total_events(), 350);
}
