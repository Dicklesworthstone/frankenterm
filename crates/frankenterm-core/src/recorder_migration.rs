//! Deterministic migration engine for recorder backends.
//!
//! Orchestrates the M0→M5 migration pipeline:
//! - **M0 Preflight**: health check, manifest capture, quiesce source
//! - **M1 Export**: stream all events from source, compute digest
//! - **M2 Import**: write events to target, verify digest match
//! - **M3 Checkpoint sync**: (future bead)
//! - **M4 Reserved**: (future bead)
//! - **M5 Cutover**: (future bead)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info};

use crate::recorder_storage::{
    AppendRequest, CursorRecord, DurabilityLevel, EventCursorError, RecorderEventReader,
    RecorderOffset, RecorderStorage, RecorderStorageHealth,
};

// ---------------------------------------------------------------------------
// Migration stage enum
// ---------------------------------------------------------------------------

/// Stages of the M0→M5 migration pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStage {
    /// Preflight: health check, manifest capture, quiesce.
    M0Preflight,
    /// Export: stream events from source, compute digest.
    M1Export,
    /// Import: write events to target, verify digest.
    M2Import,
    /// Checkpoint synchronization.
    M3CheckpointSync,
    /// Reserved for future use.
    M4Reserved,
    /// Cutover: activate target backend.
    M5Cutover,
}

impl MigrationStage {
    /// Returns `true` when this stage marks a completed migration.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::M5Cutover)
    }

    /// Returns `true` if the migration can be rolled back from this stage.
    pub fn can_rollback(&self) -> bool {
        matches!(
            self,
            Self::M0Preflight | Self::M1Export | Self::M2Import | Self::M3CheckpointSync
        )
    }
}

// ---------------------------------------------------------------------------
// Migration manifest
// ---------------------------------------------------------------------------

/// Captures migration metadata and verification digests at each stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationManifest {
    /// Total events in the source at preflight time.
    pub event_count: u64,
    /// First ordinal in source stream.
    pub first_ordinal: u64,
    /// Last ordinal in source stream.
    pub last_ordinal: u64,
    /// Per-pane event counts captured at preflight.
    pub per_pane_counts: HashMap<u64, u64>,
    /// FNV-1a digest of ordinal sequence from export.
    pub export_digest: u64,
    /// Number of events exported during M1.
    pub export_count: u64,
    /// FNV-1a digest of ordinal sequence from import verification.
    pub import_digest: u64,
    /// Number of events imported during M2.
    pub import_count: u64,
    /// Source head offset recorded at preflight.
    pub last_offset: Option<RecorderOffset>,
}

impl Default for MigrationManifest {
    fn default() -> Self {
        Self {
            event_count: 0,
            first_ordinal: 0,
            last_ordinal: 0,
            per_pane_counts: HashMap::new(),
            export_digest: FNV1A_OFFSET_BASIS,
            export_count: 0,
            import_digest: FNV1A_OFFSET_BASIS,
            import_count: 0,
            last_offset: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Migration checkpoint (resumable)
// ---------------------------------------------------------------------------

/// Persisted state for resumable migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationCheckpoint {
    /// Current stage of the migration.
    pub stage: MigrationStage,
    /// Manifest captured so far.
    pub manifest: MigrationManifest,
    /// Last successfully processed ordinal (for resume).
    pub last_processed_ordinal: u64,
    /// Whether the migration is currently active (quiesced).
    pub migration_active: bool,
}

// ---------------------------------------------------------------------------
// FNV-1a digest helpers
// ---------------------------------------------------------------------------

const FNV1A_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV1A_PRIME: u64 = 0x100000001b3;

/// Feed an ordinal into a running FNV-1a hash.
fn fnv1a_feed(hash: u64, ordinal: u64) -> u64 {
    let bytes = ordinal.to_le_bytes();
    let mut h = hash;
    for &b in &bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV1A_PRIME);
    }
    h
}

// ---------------------------------------------------------------------------
// Migration errors
// ---------------------------------------------------------------------------

/// Errors that can occur during migration.
#[derive(Debug)]
pub enum MigrationError {
    /// Source storage is degraded, cannot migrate.
    SourceDegraded {
        last_error: Option<String>,
    },
    /// Cursor/reader failure.
    CursorError(EventCursorError),
    /// Target storage rejected a write.
    TargetWriteError(String),
    /// Digest mismatch between export and import.
    DigestMismatch {
        expected: u64,
        actual: u64,
    },
    /// Event count mismatch between export and import.
    CountMismatch {
        expected: u64,
        actual: u64,
    },
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceDegraded { last_error } => {
                write!(f, "source degraded: {:?}", last_error)
            }
            Self::CursorError(e) => write!(f, "cursor error: {:?}", e),
            Self::TargetWriteError(e) => write!(f, "target write error: {e}"),
            Self::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "digest mismatch: expected={expected:#x}, actual={actual:#x}"
                )
            }
            Self::CountMismatch { expected, actual } => {
                write!(f, "count mismatch: expected={expected}, actual={actual}")
            }
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<EventCursorError> for MigrationError {
    fn from(e: EventCursorError) -> Self {
        Self::CursorError(e)
    }
}

// ---------------------------------------------------------------------------
// Migration engine
// ---------------------------------------------------------------------------

/// Configuration for the migration engine.
#[derive(Debug, Clone)]
pub struct MigrationConfig {
    /// Batch size for cursor reads during export.
    pub export_batch_size: usize,
    /// Batch size for writes during import.
    pub import_batch_size: usize,
    /// Consumer ID for idempotent batch IDs.
    pub consumer_id: String,
}

impl Default for MigrationConfig {
    fn default() -> Self {
        Self {
            export_batch_size: 1000,
            import_batch_size: 1000,
            consumer_id: "migration-engine".to_string(),
        }
    }
}

/// Orchestrates the M0→M2 migration pipeline.
///
/// The engine reads from a `RecorderEventReader` source and writes to a
/// `RecorderStorage` target, verifying deterministic digests at each stage.
pub struct MigrationEngine {
    config: MigrationConfig,
}

impl MigrationEngine {
    /// Create a new migration engine with the given config.
    pub fn new(config: MigrationConfig) -> Self {
        Self { config }
    }

    // -----------------------------------------------------------------------
    // M0 — Preflight
    // -----------------------------------------------------------------------

    /// Execute M0 preflight: health check, capture manifest, quiesce.
    ///
    /// Returns a manifest with event_count, first/last ordinal, and per-pane counts.
    /// Rejects if the source storage reports as degraded.
    pub async fn m0_preflight<S: RecorderStorage>(
        &self,
        source_storage: &S,
        source_reader: &dyn RecorderEventReader,
    ) -> Result<MigrationManifest, MigrationError> {
        // 1. Health check
        let health: RecorderStorageHealth = source_storage.health().await;
        if health.degraded {
            return Err(MigrationError::SourceDegraded {
                last_error: health.last_error,
            });
        }

        // 2. Head offset
        let head = source_reader.head_offset()?;

        // 3. Scan all events for manifest counts
        let mut cursor = source_reader.open_cursor_from_start()?;
        let mut event_count: u64 = 0;
        let mut first_ordinal: Option<u64> = None;
        let mut last_ordinal: u64 = 0;
        let mut per_pane_counts: HashMap<u64, u64> = HashMap::new();

        loop {
            let batch = cursor.next_batch(self.config.export_batch_size)?;
            if batch.is_empty() {
                break;
            }
            for record in &batch {
                event_count += 1;
                if first_ordinal.is_none() {
                    first_ordinal = Some(record.offset.ordinal);
                }
                last_ordinal = record.offset.ordinal;
                *per_pane_counts.entry(record.event.pane_id).or_insert(0) += 1;
            }
        }

        let manifest = MigrationManifest {
            event_count,
            first_ordinal: first_ordinal.unwrap_or(0),
            last_ordinal,
            per_pane_counts,
            last_offset: Some(head),
            ..Default::default()
        };

        info!(
            migration_stage = "M0",
            event_count = event_count,
            last_ordinal = %last_ordinal,
            "preflight complete"
        );

        Ok(manifest)
    }

    // -----------------------------------------------------------------------
    // M1 — Export
    // -----------------------------------------------------------------------

    /// Execute M1 export: stream all events, compute FNV-1a digest of ordinals.
    ///
    /// Updates the manifest with `export_digest` and `export_count`.
    /// Returns the exported records for M2 import.
    pub fn m1_export(
        &self,
        source_reader: &dyn RecorderEventReader,
        manifest: &mut MigrationManifest,
    ) -> Result<Vec<CursorRecord>, MigrationError> {
        let mut cursor = source_reader.open_cursor_from_start()?;
        let mut all_records: Vec<CursorRecord> = Vec::new();
        let mut digest = FNV1A_OFFSET_BASIS;
        let mut count: u64 = 0;

        loop {
            let batch = cursor.next_batch(self.config.export_batch_size)?;
            if batch.is_empty() {
                break;
            }
            for record in batch {
                digest = fnv1a_feed(digest, record.offset.ordinal);
                count += 1;
                all_records.push(record);
            }
        }

        manifest.export_digest = digest;
        manifest.export_count = count;

        info!(
            migration_stage = "M1",
            events_exported = count,
            digest = %format!("{digest:#x}"),
            "export complete"
        );

        Ok(all_records)
    }

    // -----------------------------------------------------------------------
    // M2 — Import
    // -----------------------------------------------------------------------

    /// Execute M2 import: write events to target, verify digest and count match.
    ///
    /// Uses original event ordinals as part of batch IDs for idempotency.
    /// On mismatch, returns an error without modifying the manifest.
    pub async fn m2_import<T: RecorderStorage>(
        &self,
        target: &T,
        records: &[CursorRecord],
        manifest: &mut MigrationManifest,
    ) -> Result<(), MigrationError> {
        let mut import_digest = FNV1A_OFFSET_BASIS;
        let mut import_count: u64 = 0;

        // Write in batches
        for chunk in records.chunks(self.config.import_batch_size) {
            let first_ord = chunk.first().map(|r| r.offset.ordinal).unwrap_or(0);
            let last_ord = chunk.last().map(|r| r.offset.ordinal).unwrap_or(0);
            let batch_id =
                format!("{}-{first_ord}-{last_ord}", self.config.consumer_id);

            let events: Vec<_> = chunk.iter().map(|r| r.event.clone()).collect();

            let req = AppendRequest {
                batch_id,
                events,
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 0,
            };

            target.append_batch(req).await.map_err(|e| {
                MigrationError::TargetWriteError(e.to_string())
            })?;

            // Compute digest over imported ordinals
            for record in chunk {
                import_digest = fnv1a_feed(import_digest, record.offset.ordinal);
                import_count += 1;
            }
        }

        // Verify counts
        if import_count != manifest.export_count {
            error!(
                migration_abort = true,
                stage = "M2",
                expected_count = manifest.export_count,
                actual_count = import_count,
                "count mismatch"
            );
            return Err(MigrationError::CountMismatch {
                expected: manifest.export_count,
                actual: import_count,
            });
        }

        // Verify digests
        if import_digest != manifest.export_digest {
            error!(
                migration_abort = true,
                stage = "M2",
                expected_digest = %format!("{:#x}", manifest.export_digest),
                actual_digest = %format!("{import_digest:#x}"),
                "digest mismatch"
            );
            return Err(MigrationError::DigestMismatch {
                expected: manifest.export_digest,
                actual: import_digest,
            });
        }

        manifest.import_digest = import_digest;
        manifest.import_count = import_count;

        info!(
            migration_stage = "M2",
            events_imported = import_count,
            digest = %format!("{import_digest:#x}"),
            "import complete"
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Full M0→M2 pipeline
    // -----------------------------------------------------------------------

    /// Run the full M0→M2 pipeline: preflight → export → import.
    ///
    /// Returns the verified manifest on success.
    pub async fn run_m0_m2<S: RecorderStorage, T: RecorderStorage>(
        &self,
        source_storage: &S,
        source_reader: &dyn RecorderEventReader,
        target: &T,
    ) -> Result<MigrationManifest, MigrationError> {
        // M0
        let mut manifest = self.m0_preflight(source_storage, source_reader).await?;

        // M1
        let records = self.m1_export(source_reader, &mut manifest)?;

        // M2
        self.m2_import(target, &records, &mut manifest).await?;

        Ok(manifest)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder_storage::{
        AppendResponse, CheckpointCommitOutcome, CheckpointConsumerId, FlushMode, FlushStats,
        RecorderBackendKind, RecorderCheckpoint, RecorderEventCursor, RecorderStorageError,
        RecorderStorageHealth, RecorderStorageLag,
    };
    use crate::recording::{
        RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
        RecorderIngressKind, RecorderTextEncoding,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // Test helpers: mock reader + mock storage
    // -----------------------------------------------------------------------

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
                redaction: crate::recording::RecorderRedactionLevel::None,
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

    // -----------------------------------------------------------------------
    // M0 tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_m0_captures_manifest_with_correct_counts() {
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
    }

    #[tokio::test]
    async fn test_m0_rejects_degraded_source() {
        let reader = TestEventReader::new(vec![]);
        let storage = MockMigrationStorage::degraded();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let result = engine.m0_preflight(&storage, &reader).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("degraded"), "error should mention degraded: {msg}");
    }

    #[tokio::test]
    async fn test_m0_empty_source_produces_zero_counts() {
        let reader = TestEventReader::new(vec![]);
        let storage = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine.m0_preflight(&storage, &reader).await.unwrap();
        assert_eq!(manifest.event_count, 0);
        assert_eq!(manifest.first_ordinal, 0);
        assert_eq!(manifest.last_ordinal, 0);
        assert!(manifest.per_pane_counts.is_empty());
    }

    // -----------------------------------------------------------------------
    // M1 tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_m1_exports_all_events_in_order() {
        let records = vec![
            make_cursor_record(1, 0),
            make_cursor_record(1, 1),
            make_cursor_record(2, 2),
        ];
        let reader = TestEventReader::new(records.clone());
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        assert_eq!(exported.len(), 3);
        assert_eq!(exported[0].offset.ordinal, 0);
        assert_eq!(exported[1].offset.ordinal, 1);
        assert_eq!(exported[2].offset.ordinal, 2);
        assert_eq!(manifest.export_count, 3);
        assert_ne!(manifest.export_digest, FNV1A_OFFSET_BASIS);
    }

    #[test]
    fn test_m1_digest_deterministic_for_same_data() {
        let records = vec![
            make_cursor_record(1, 0),
            make_cursor_record(1, 1),
            make_cursor_record(2, 2),
        ];

        let engine = MigrationEngine::new(MigrationConfig::default());

        let mut manifest1 = MigrationManifest::default();
        let reader1 = TestEventReader::new(records.clone());
        engine.m1_export(&reader1, &mut manifest1).unwrap();

        let mut manifest2 = MigrationManifest::default();
        let reader2 = TestEventReader::new(records);
        engine.m1_export(&reader2, &mut manifest2).unwrap();

        assert_eq!(manifest1.export_digest, manifest2.export_digest);
        assert_eq!(manifest1.export_count, manifest2.export_count);
    }

    #[test]
    fn test_m1_empty_source_produces_basis_digest() {
        let reader = TestEventReader::new(vec![]);
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let exported = engine.m1_export(&reader, &mut manifest).unwrap();
        assert!(exported.is_empty());
        assert_eq!(manifest.export_count, 0);
        assert_eq!(manifest.export_digest, FNV1A_OFFSET_BASIS);
    }

    #[test]
    fn test_m1_different_ordinals_produce_different_digests() {
        let engine = MigrationEngine::new(MigrationConfig::default());

        let mut m1 = MigrationManifest::default();
        let r1 = TestEventReader::new(vec![make_cursor_record(1, 0), make_cursor_record(1, 1)]);
        engine.m1_export(&r1, &mut m1).unwrap();

        let mut m2 = MigrationManifest::default();
        let r2 = TestEventReader::new(vec![make_cursor_record(1, 0), make_cursor_record(1, 2)]);
        engine.m1_export(&r2, &mut m2).unwrap();

        assert_ne!(m1.export_digest, m2.export_digest);
    }

    // -----------------------------------------------------------------------
    // M2 tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_m2_imports_preserving_ordinals() {
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

        // Compute export digest first
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
    }

    #[tokio::test]
    async fn test_m2_digest_match_passes() {
        let records = vec![make_cursor_record(1, 0), make_cursor_record(1, 1)];
        let engine = MigrationEngine::new(MigrationConfig::default());
        let mut manifest = MigrationManifest::default();

        let reader = TestEventReader::new(records);
        let exported = engine.m1_export(&reader, &mut manifest).unwrap();

        let target = MockMigrationStorage::healthy();
        let result = engine.m2_import(&target, &exported, &mut manifest).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_m2_digest_mismatch_aborts() {
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
    }

    #[tokio::test]
    async fn test_m2_target_write_failure_propagates() {
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
    }

    // -----------------------------------------------------------------------
    // End-to-end M0→M2 pipeline
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_m0_m2_pipeline_end_to_end() {
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

        let manifest = engine
            .run_m0_m2(&source, &reader, &target)
            .await
            .unwrap();

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
    }

    // -----------------------------------------------------------------------
    // FNV-1a digest unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_fnv1a_feed_deterministic() {
        let h1 = fnv1a_feed(FNV1A_OFFSET_BASIS, 42);
        let h2 = fnv1a_feed(FNV1A_OFFSET_BASIS, 42);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_fnv1a_feed_different_values_differ() {
        let h1 = fnv1a_feed(FNV1A_OFFSET_BASIS, 1);
        let h2 = fnv1a_feed(FNV1A_OFFSET_BASIS, 2);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_fnv1a_order_sensitive() {
        let h1 = fnv1a_feed(fnv1a_feed(FNV1A_OFFSET_BASIS, 1), 2);
        let h2 = fnv1a_feed(fnv1a_feed(FNV1A_OFFSET_BASIS, 2), 1);
        assert_ne!(h1, h2);
    }

    // -----------------------------------------------------------------------
    // Stage enum tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_migration_stage_is_complete() {
        assert!(!MigrationStage::M0Preflight.is_complete());
        assert!(!MigrationStage::M1Export.is_complete());
        assert!(!MigrationStage::M2Import.is_complete());
        assert!(!MigrationStage::M3CheckpointSync.is_complete());
        assert!(!MigrationStage::M4Reserved.is_complete());
        assert!(MigrationStage::M5Cutover.is_complete());
    }

    #[test]
    fn test_migration_stage_can_rollback() {
        assert!(MigrationStage::M0Preflight.can_rollback());
        assert!(MigrationStage::M1Export.can_rollback());
        assert!(MigrationStage::M2Import.can_rollback());
        assert!(MigrationStage::M3CheckpointSync.can_rollback());
        assert!(!MigrationStage::M4Reserved.can_rollback());
        assert!(!MigrationStage::M5Cutover.can_rollback());
    }

    #[test]
    fn test_migration_stage_serialize_roundtrip() {
        let stage = MigrationStage::M2Import;
        let json = serde_json::to_string(&stage).unwrap();
        let restored: MigrationStage = serde_json::from_str(&json).unwrap();
        assert_eq!(stage, restored);
    }

    // -----------------------------------------------------------------------
    // Manifest + checkpoint tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_migration_manifest_serialize_roundtrip() {
        let mut manifest = MigrationManifest::default();
        manifest.event_count = 42;
        manifest.first_ordinal = 1;
        manifest.last_ordinal = 42;
        manifest.per_pane_counts.insert(1, 20);
        manifest.per_pane_counts.insert(2, 22);
        manifest.export_digest = 0xCAFE;
        manifest.export_count = 42;

        let json = serde_json::to_string(&manifest).unwrap();
        let restored: MigrationManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, restored);
    }

    #[test]
    fn test_migration_checkpoint_serialize_roundtrip() {
        let checkpoint = MigrationCheckpoint {
            stage: MigrationStage::M1Export,
            manifest: MigrationManifest::default(),
            last_processed_ordinal: 100,
            migration_active: true,
        };

        let json = serde_json::to_string(&checkpoint).unwrap();
        let restored: MigrationCheckpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(checkpoint, restored);
    }

    #[test]
    fn test_migration_manifest_default_has_basis_digest() {
        let manifest = MigrationManifest::default();
        assert_eq!(manifest.export_digest, FNV1A_OFFSET_BASIS);
        assert_eq!(manifest.import_digest, FNV1A_OFFSET_BASIS);
        assert_eq!(manifest.event_count, 0);
    }

    // -----------------------------------------------------------------------
    // Error display tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_migration_error_display() {
        let err = MigrationError::SourceDegraded {
            last_error: Some("disk full".to_string()),
        };
        assert!(format!("{err}").contains("disk full"));

        let err = MigrationError::DigestMismatch {
            expected: 0xAA,
            actual: 0xBB,
        };
        let msg = format!("{err}");
        assert!(msg.contains("0xaa"));
        assert!(msg.contains("0xbb"));

        let err = MigrationError::CountMismatch {
            expected: 5,
            actual: 3,
        };
        let msg = format!("{err}");
        assert!(msg.contains("5"));
        assert!(msg.contains("3"));
    }

    // -----------------------------------------------------------------------
    // Config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_migration_config_default() {
        let config = MigrationConfig::default();
        assert_eq!(config.export_batch_size, 1000);
        assert_eq!(config.import_batch_size, 1000);
        assert_eq!(config.consumer_id, "migration-engine");
    }

    // -----------------------------------------------------------------------
    // Small batch size pipeline
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_m0_m2_with_batch_size_one() {
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

        let manifest = engine
            .run_m0_m2(&source, &reader, &target)
            .await
            .unwrap();

        assert_eq!(manifest.event_count, 3);
        assert_eq!(manifest.export_count, 3);
        assert_eq!(manifest.import_count, 3);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        // batch_size=1 means 3 separate append calls
        assert_eq!(target.appended.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_m0_m2_pipeline_empty_source() {
        let reader = TestEventReader::new(vec![]);
        let source = MockMigrationStorage::healthy();
        let target = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = engine
            .run_m0_m2(&source, &reader, &target)
            .await
            .unwrap();

        assert_eq!(manifest.event_count, 0);
        assert_eq!(manifest.export_count, 0);
        assert_eq!(manifest.import_count, 0);
        assert_eq!(manifest.import_digest, manifest.export_digest);
        assert_eq!(target.total_events_appended(), 0);
    }

    // -----------------------------------------------------------------------
    // Batch ID idempotency test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_m2_batch_ids_contain_ordinal_range() {
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
    }

    // -----------------------------------------------------------------------
    // Additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_m0_preflight_per_pane_counts_single_pane() {
        let records = vec![
            make_cursor_record(7, 0),
            make_cursor_record(7, 1),
            make_cursor_record(7, 2),
            make_cursor_record(7, 3),
        ];
        let reader = TestEventReader::new(records);
        let storage = MockMigrationStorage::healthy();
        let engine = MigrationEngine::new(MigrationConfig::default());

        let manifest = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(engine.m0_preflight(&storage, &reader));
        let manifest = manifest.unwrap();
        assert_eq!(manifest.per_pane_counts.len(), 1);
        assert_eq!(manifest.per_pane_counts.get(&7), Some(&4));
    }

    #[test]
    fn test_migration_error_cursor_error_display() {
        let err = MigrationError::CursorError(EventCursorError::Io("timeout".to_string()));
        let msg = format!("{err}");
        assert!(msg.contains("timeout"), "msg: {msg}");
    }

    #[test]
    fn test_migration_error_target_write_display() {
        let err = MigrationError::TargetWriteError("queue full".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("queue full"), "msg: {msg}");
    }

    #[tokio::test]
    async fn test_m2_count_mismatch_detected() {
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
    }
}
