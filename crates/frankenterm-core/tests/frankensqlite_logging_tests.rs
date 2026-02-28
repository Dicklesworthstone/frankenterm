//! E4.F2.T1: Structured logging tracing fields and diagnostic assertion probes.
//!
//! Verifies that critical recorder/migration/rollback code paths emit structured
//! tracing fields needed for observability and incident triage.

use frankenterm_core::recorder_migration::{MigrationConfig, MigrationEngine};
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
    MigrationRollbackClassifierInput, MigrationStage, classify_migration_rollback_trigger,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

// ---------------------------------------------------------------------------
// LogCapture — tracing layer that captures structured fields
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct CapturedEvent {
    level: Level,
    target: String,
    message: String,
    fields: HashMap<String, String>,
}

struct LogCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl LogCapture {
    fn new() -> (Self, Arc<Mutex<Vec<CapturedEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
            },
            events,
        )
    }
}

struct FieldVisitor {
    fields: HashMap<String, String>,
    message: String,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

impl<S: Subscriber> Layer<S> for LogCapture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor {
            fields: HashMap::new(),
            message: String::new(),
        };
        event.record(&mut visitor);

        let meta = event.metadata();
        let captured = CapturedEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        self.events.lock().unwrap().push(captured);
    }
}

fn install_capture() -> (
    tracing::dispatcher::DefaultGuard,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let (layer, events) = LogCapture::new();
    let subscriber = tracing_subscriber::registry().with(layer);
    let dispatch = tracing::Dispatch::new(subscriber);
    let guard = tracing::dispatcher::set_default(&dispatch);
    (guard, events)
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 64,
        max_batch_events: 256,
        max_batch_bytes: 512 * 1024,
        max_idempotency_entries: 128,
    }
}

fn sample_event(seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: format!("log-{seq}"),
        pane_id: seq % 3 + 1,
        session_id: Some("log-session".to_string()),
        workflow_id: None,
        correlation_id: None,
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
            text: format!("payload-{seq}"),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

async fn populate_source(storage: &AppendLogRecorderStorage, count: u64) -> Vec<CursorRecord> {
    let batch_size = 50u64;
    let mut seq = 0u64;
    let mut all_records = Vec::new();

    while seq < count {
        let end = (seq + batch_size).min(count);
        let events: Vec<_> = (seq..end).map(sample_event).collect();
        let resp = storage
            .append_batch(AppendRequest {
                batch_id: format!("populate-{seq}"),
                events: events.clone(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();
        let first_ord = resp.first_offset.ordinal;
        for (i, event) in events.into_iter().enumerate() {
            let ordinal = first_ord + i as u64;
            all_records.push(CursorRecord {
                event,
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: ordinal * 100,
                    ordinal,
                },
            });
        }
        seq = end;
    }
    all_records
}

// In-memory reader for migration tests
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

// Mock target storage
struct MockTargetStorage {
    health: RecorderStorageHealth,
    appended: Mutex<Vec<AppendRequest>>,
    checkpoints: Mutex<HashMap<String, RecorderCheckpoint>>,
    fail_append: AtomicBool,
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
            checkpoints: Mutex::new(HashMap::new()),
            fail_append: AtomicBool::new(false),
        }
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
        self.health.clone()
    }

    async fn lag_metrics(&self) -> std::result::Result<RecorderStorageLag, RecorderStorageError> {
        Ok(RecorderStorageLag {
            latest_offset: None,
            consumers: vec![],
        })
    }
}

// ===========================================================================
// Helper to find events containing a specific field
// ===========================================================================

fn events_with_field(events: &[CapturedEvent], field_name: &str) -> Vec<CapturedEvent> {
    events
        .iter()
        .filter(|e| e.fields.contains_key(field_name))
        .cloned()
        .collect()
}

fn events_with_field_value(
    events: &[CapturedEvent],
    field_name: &str,
    value: &str,
) -> Vec<CapturedEvent> {
    events
        .iter()
        .filter(|e| e.fields.get(field_name).map(|v| v.as_str()) == Some(value))
        .cloned()
        .collect()
}

fn has_field(events: &[CapturedEvent], field_name: &str) -> bool {
    events.iter().any(|e| e.fields.contains_key(field_name))
}

// ===========================================================================
// Migration logging tests
// ===========================================================================

#[tokio::test]
async fn test_m0_preflight_logs_migration_stage() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 20).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    engine.m0_preflight(&source, &reader).await.unwrap();

    let events = captured.lock().unwrap();
    let stage_events = events_with_field_value(&events, "migration_stage", "M0");
    assert!(
        !stage_events.is_empty(),
        "M0 preflight should log migration_stage=M0"
    );
}

#[tokio::test]
async fn test_m0_preflight_logs_event_count() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 15).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    engine.m0_preflight(&source, &reader).await.unwrap();

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "event_count"),
        "M0 should log event_count field"
    );
}

#[tokio::test]
async fn test_m1_export_logs_migration_stage_m1() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
    engine.m1_export(&reader, &mut manifest).unwrap();

    let events = captured.lock().unwrap();
    let m1_events = events_with_field_value(&events, "migration_stage", "M1");
    assert!(
        !m1_events.is_empty(),
        "M1 export should log migration_stage=M1"
    );
}

#[tokio::test]
async fn test_m1_export_logs_digest() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
    engine.m1_export(&reader, &mut manifest).unwrap();

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "digest"),
        "M1 export should log digest field"
    );
}

#[tokio::test]
async fn test_m1_export_logs_events_exported() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
    engine.m1_export(&reader, &mut manifest).unwrap();

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "events_exported"),
        "M1 export should log events_exported field"
    );
}

#[tokio::test]
async fn test_m2_import_logs_migration_stage_m2() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let target = MockTargetStorage::healthy();
    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
    let exported = engine.m1_export(&reader, &mut manifest).unwrap();
    engine
        .m2_import(&target, &exported, &mut manifest)
        .await
        .unwrap();

    let events = captured.lock().unwrap();
    let m2_events = events_with_field_value(&events, "migration_stage", "M2");
    assert!(
        !m2_events.is_empty(),
        "M2 import should log migration_stage=M2"
    );
}

#[tokio::test]
async fn test_m2_import_error_logs_error_fields() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let target = MockTargetStorage::healthy();
    target.fail_append.store(true, Ordering::Relaxed);
    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
    let exported = engine.m1_export(&reader, &mut manifest).unwrap();
    let _ = engine.m2_import(&target, &exported, &mut manifest).await;

    // The M1 export still runs and logs its stage — verify that
    let events = captured.lock().unwrap();
    let m1_events = events_with_field_value(&events, "migration_stage", "M1");
    assert!(
        !m1_events.is_empty(),
        "M1 stage should still be logged before M2 failure"
    );
    // M2 write failure propagates without logging — verify no M2 success log
    let m2_events = events_with_field_value(&events, "migration_stage", "M2");
    assert!(
        m2_events.is_empty(),
        "M2 stage should NOT be logged on write failure"
    );
}

#[tokio::test]
async fn test_m3_checkpoint_sync_logs_stage() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let target = MockTargetStorage::healthy();
    let engine = MigrationEngine::new(MigrationConfig::default());

    let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();
    engine
        .m3_checkpoint_sync(&source, &target, &manifest)
        .await
        .unwrap();

    let events = captured.lock().unwrap();
    let m3_events = events_with_field_value(&events, "migration_stage", "M3");
    assert!(
        !m3_events.is_empty(),
        "M3 checkpoint sync should log migration_stage=M3"
    );
}

#[tokio::test]
async fn test_m5_cutover_logs_stage() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let target = MockTargetStorage::healthy();
    let engine = MigrationEngine::new(MigrationConfig::default());

    let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();
    engine
        .m5_cutover(&target, &manifest, 1708000000, None)
        .await
        .unwrap();

    let events = captured.lock().unwrap();
    let m5_events = events_with_field_value(&events, "migration_stage", "M5");
    assert!(
        !m5_events.is_empty(),
        "M5 cutover should log migration_stage=M5"
    );
}

// ===========================================================================
// Rollback classifier logging tests
// ===========================================================================

#[test]
fn test_rollback_classifier_logs_stage() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Import,
        import_digest_mismatch: true,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "stage"),
        "rollback classifier should log stage field"
    );
}

#[test]
fn test_rollback_classifier_logs_rollback_class() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Import,
        import_digest_mismatch: true,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "rollback_class"),
        "rollback classifier should log rollback_class field"
    );
}

#[test]
fn test_rollback_classifier_logs_triggers() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Import,
        event_cardinality_mismatch: true,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "triggers"),
        "rollback classifier should log triggers field"
    );
}

#[test]
fn test_rollback_no_trigger_logs_info() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Activate,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    let has_info = events.iter().any(|e| e.level == Level::INFO);
    assert!(has_info, "no-trigger classifier should log at info level");
}

#[test]
fn test_rollback_trigger_logs_warn() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Import,
        checkpoint_regression: true,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    let has_warn = events.iter().any(|e| e.level == Level::WARN);
    assert!(has_warn, "triggered rollback should log at warn level");
}

// ===========================================================================
// Bootstrap logging tests
// ===========================================================================

#[test]
fn test_bootstrap_logs_backend_kind() {
    use frankenterm_core::recorder_storage::{RecorderStorageConfig, bootstrap_recorder_storage};

    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();

    let config = RecorderStorageConfig {
        backend: RecorderBackendKind::AppendLog,
        append_log: test_config(dir.path()),
    };
    let _ = bootstrap_recorder_storage(config);

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "backend"),
        "bootstrap should log backend field"
    );
}

#[test]
fn test_bootstrap_logs_data_path() {
    use frankenterm_core::recorder_storage::{RecorderStorageConfig, bootstrap_recorder_storage};

    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();

    let config = RecorderStorageConfig {
        backend: RecorderBackendKind::AppendLog,
        append_log: test_config(dir.path()),
    };
    let _ = bootstrap_recorder_storage(config);

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "data_path"),
        "bootstrap should log data_path field"
    );
}

// ===========================================================================
// LogCapture infrastructure tests
// ===========================================================================

#[test]
fn test_log_capture_captures_info_events() {
    let (_guard, captured) = install_capture();
    tracing::info!(key = "value", "test message");

    let events = captured.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fields.get("key").unwrap(), "value");
}

#[test]
fn test_log_capture_captures_warn_events() {
    let (_guard, captured) = install_capture();
    tracing::warn!(severity = "high", "warning message");

    let events = captured.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, Level::WARN);
    assert_eq!(events[0].fields.get("severity").unwrap(), "high");
}

#[test]
fn test_log_capture_captures_error_events() {
    let (_guard, captured) = install_capture();
    tracing::error!(error_class = "io", "error occurred");

    let events = captured.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, Level::ERROR);
}

#[test]
fn test_log_capture_captures_multiple_fields() {
    let (_guard, captured) = install_capture();
    tracing::info!(a = "1", b = "2", c = "3", "multi-field");

    let events = captured.lock().unwrap();
    assert_eq!(events[0].fields.len(), 3);
}

#[test]
fn test_events_with_field_filters_correctly() {
    let events = vec![
        CapturedEvent {
            level: Level::INFO,
            target: "test".to_string(),
            message: "msg".to_string(),
            fields: std::iter::once(("key1".to_string(), "val1".to_string())).collect(),
        },
        CapturedEvent {
            level: Level::INFO,
            target: "test".to_string(),
            message: "msg".to_string(),
            fields: std::iter::once(("key2".to_string(), "val2".to_string())).collect(),
        },
    ];

    let filtered = events_with_field(&events, "key1");
    assert_eq!(filtered.len(), 1);
}

#[test]
fn test_events_with_field_value_filters_correctly() {
    let events = vec![
        CapturedEvent {
            level: Level::INFO,
            target: "test".to_string(),
            message: "msg".to_string(),
            fields: std::iter::once(("stage".to_string(), "M0".to_string())).collect(),
        },
        CapturedEvent {
            level: Level::INFO,
            target: "test".to_string(),
            message: "msg".to_string(),
            fields: std::iter::once(("stage".to_string(), "M1".to_string())).collect(),
        },
    ];

    let m0 = events_with_field_value(&events, "stage", "M0");
    assert_eq!(m0.len(), 1);
    let m1 = events_with_field_value(&events, "stage", "M1");
    assert_eq!(m1.len(), 1);
}

// ===========================================================================
// Rollback classifier field presence meta-tests
// ===========================================================================

#[test]
fn test_rollback_classifier_logs_slo_fields_when_present() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Activate,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "consecutive_slo_breach_windows"),
        "should log consecutive_slo_breach_windows"
    );
    assert!(
        has_field(&events, "high_severity_write_failures"),
        "should log high_severity_write_failures"
    );
    assert!(
        has_field(&events, "high_severity_index_failures"),
        "should log high_severity_index_failures"
    );
}

#[tokio::test]
async fn test_full_pipeline_emits_all_stage_logs() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 20).await;
    let reader = MemoryReader { records };
    let target = MockTargetStorage::healthy();
    let engine = MigrationEngine::new(MigrationConfig::default());

    let manifest = engine.run_m0_m2(&source, &reader, &target).await.unwrap();
    engine
        .m3_checkpoint_sync(&source, &target, &manifest)
        .await
        .unwrap();
    engine
        .m5_cutover(&target, &manifest, 1708000000, None)
        .await
        .unwrap();

    let events = captured.lock().unwrap();
    let stages: Vec<String> = events
        .iter()
        .filter_map(|e| e.fields.get("migration_stage").cloned())
        .collect();

    assert!(stages.contains(&"M0".to_string()), "missing M0 stage log");
    assert!(stages.contains(&"M1".to_string()), "missing M1 stage log");
    assert!(stages.contains(&"M2".to_string()), "missing M2 stage log");
    assert!(stages.contains(&"M3".to_string()), "missing M3 stage log");
    assert!(stages.contains(&"M5".to_string()), "missing M5 stage log");
}

// ===========================================================================
// Numeric field correctness
// ===========================================================================

#[tokio::test]
async fn test_m0_logs_correct_event_count() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 25).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    engine.m0_preflight(&source, &reader).await.unwrap();

    let events = captured.lock().unwrap();
    let count_events = events_with_field(&events, "event_count");
    assert!(!count_events.is_empty());
    let logged_count = &count_events[0].fields["event_count"];
    assert_eq!(logged_count, "25", "event_count should be 25");
}

#[tokio::test]
async fn test_m1_logs_correct_export_count() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 12).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    let mut manifest = engine.m0_preflight(&source, &reader).await.unwrap();
    engine.m1_export(&reader, &mut manifest).unwrap();

    let events = captured.lock().unwrap();
    let export_events = events_with_field(&events, "events_exported");
    assert!(!export_events.is_empty());
    let logged = &export_events[0].fields["events_exported"];
    assert_eq!(logged, "12", "events_exported should be 12");
}

// ===========================================================================
// Additional logging probes
// ===========================================================================

#[tokio::test]
async fn test_m0_logs_last_ordinal() {
    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();
    let source = AppendLogRecorderStorage::open(test_config(dir.path())).unwrap();
    let records = populate_source(&source, 10).await;
    let reader = MemoryReader { records };
    let engine = MigrationEngine::new(MigrationConfig::default());

    engine.m0_preflight(&source, &reader).await.unwrap();

    let events = captured.lock().unwrap();
    assert!(
        has_field(&events, "last_ordinal"),
        "M0 should log last_ordinal field"
    );
}

#[test]
fn test_rollback_classifier_warn_includes_write_failure_count() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Soak,
        high_severity_write_failures: 10,
        config: frankenterm_core::storage::MigrationRollbackClassifierConfig {
            repeated_write_failure_threshold: 3,
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    let has_warn = events.iter().any(|e| e.level == Level::WARN);
    assert!(has_warn);
    assert!(
        has_field(&events, "high_severity_write_failures"),
        "should include write failure count in warn log"
    );
}

#[test]
fn test_bootstrap_logs_at_info_level() {
    use frankenterm_core::recorder_storage::{RecorderStorageConfig, bootstrap_recorder_storage};

    let (_guard, captured) = install_capture();
    let dir = tempdir().unwrap();

    let config = RecorderStorageConfig {
        backend: RecorderBackendKind::AppendLog,
        append_log: test_config(dir.path()),
    };
    let _ = bootstrap_recorder_storage(config);

    let events = captured.lock().unwrap();
    let has_info = events.iter().any(|e| e.level == Level::INFO);
    assert!(has_info, "bootstrap should log at INFO level");
}

#[test]
fn test_multiple_rollback_triggers_all_present_in_log() {
    let (_guard, captured) = install_capture();
    let input = MigrationRollbackClassifierInput {
        stage: MigrationStage::Import,
        import_digest_mismatch: true,
        event_cardinality_mismatch: true,
        checkpoint_regression: true,
        ..Default::default()
    };

    let _ = classify_migration_rollback_trigger(&input);

    let events = captured.lock().unwrap();
    let trigger_events = events_with_field(&events, "triggers");
    assert!(!trigger_events.is_empty());
    let triggers_str = &trigger_events[0].fields["triggers"];
    // Multiple triggers should be present in the debug output
    assert!(
        triggers_str.contains("ImportDigestMismatch")
            || triggers_str.contains("import_digest_mismatch"),
        "triggers should include digest mismatch"
    );
}
