//! Deterministic migration engine for recorder backends.
//!
//! Orchestrates the M0→M5 migration pipeline:
//! - **M0 Preflight**: health check and manifest observation; source
//!   quiescence remains an external prerequisite
//! - **M1 Export**: stream all events from source, compute digest
//! - **M2 Import**: write events to target, verify digest match
//! - **M3 Checkpoint sync**: migrate consumer checkpoints with monotonicity
//! - **M4 Reserved**: projection reconciliation (handled by E2.F2 reindex hooks)
//! - **M5 Readiness**: verify the imported manifest and target, then emit a
//!   durable marker stating that an external selector authority may activate
//!   the target
//!
//! This module does not own the process-wide recorder backend selector. M5
//! therefore never claims to activate a backend or complete a migration. A
//! caller with real selector authority must atomically persist/switch that
//! selector after [`MigrationEngine::m5_mark_ready`] succeeds.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::{error, info};

use tracing::{debug, warn};

use crate::recorder_storage::{
    AppendRequest, CheckpointConsumerId, CursorRecord, DurabilityLevel, EventCursorError,
    RecorderBackendKind, RecorderBackendSelectionError, RecorderCheckpoint, RecorderEventReader,
    RecorderOffset, RecorderStorage, RecorderStorageHealth, select_recorder_backend,
};
use crate::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderLifecyclePhase,
};

// ---------------------------------------------------------------------------
// Migration stage enum
// ---------------------------------------------------------------------------

/// Stages of the M0→M5 migration pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStage {
    /// Preflight: health check and manifest observation (no quiescence fence).
    M0Preflight,
    /// Export: stream events from source, compute digest.
    M1Export,
    /// Import: write events to target, verify digest.
    M2Import,
    /// Checkpoint synchronization.
    M3CheckpointSync,
    /// Reserved for future use.
    M4Reserved,
    /// Target readiness marker persisted; selector activation remains external.
    M5Readiness,
}

impl MigrationStage {
    /// Returns `true` when this stage proves a completed migration.
    ///
    /// No current stage returns `true`: M5 only establishes target readiness,
    /// and this module has no authority to persist or switch the live backend
    /// selector.
    pub fn is_complete(&self) -> bool {
        !matches!(
            self,
            Self::M0Preflight
                | Self::M1Export
                | Self::M2Import
                | Self::M3CheckpointSync
                | Self::M4Reserved
                | Self::M5Readiness
        )
    }

    /// Returns `true` if the migration can be rolled back from this stage.
    pub fn can_rollback(&self) -> bool {
        matches!(
            self,
            Self::M0Preflight
                | Self::M1Export
                | Self::M2Import
                | Self::M3CheckpointSync
                | Self::M5Readiness
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
// Migration checkpoint DTO (external orchestration)
// ---------------------------------------------------------------------------

/// Serializable state an external migration orchestrator may persist.
///
/// `MigrationEngine` itself neither stores nor consumes this DTO, so its
/// presence is not evidence of durable stage resumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationCheckpoint {
    /// Current stage of the migration.
    pub stage: MigrationStage,
    /// Manifest captured so far.
    pub manifest: MigrationManifest,
    /// Last successfully processed ordinal reported by the external owner.
    pub last_processed_ordinal: u64,
    /// Caller-reported active state; this is not a source-quiescence witness.
    pub migration_active: bool,
}

// ---------------------------------------------------------------------------
// M3 checkpoint sync result
// ---------------------------------------------------------------------------

/// Result of M3 checkpoint synchronization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSyncResult {
    /// Number of consumers discovered in source.
    pub consumers_found: usize,
    /// Number of checkpoints successfully migrated.
    pub checkpoints_migrated: usize,
    /// Number of checkpoints reset due to ordinal gaps.
    pub checkpoints_reset: usize,
    /// Consumer IDs that were reset.
    pub reset_consumers: Vec<String>,
}

// ---------------------------------------------------------------------------
// M5 readiness result
// ---------------------------------------------------------------------------

/// Proof that an M5 readiness marker was durably appended to a healthy target.
///
/// This result deliberately has no `activated` field: selector persistence and
/// activation are outside this module's authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutoverReadinessResult {
    /// Backend whose readiness marker was durably appended.
    pub target_backend: RecorderBackendKind,
    /// Migration epoch timestamp (ms).
    pub migration_epoch_ms: u64,
    /// Durable storage offset returned for the readiness marker.
    pub marker_offset: RecorderOffset,
    /// Caller-reported source path hint for external rollback coordination.
    ///
    /// The migration engine does not verify filesystem retention or ownership.
    pub source_path_hint: Option<String>,
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

/// Compute a content-bound M2 idempotency digest.
///
/// Ordinals alone are insufficient: a retry with different event bytes but
/// the same ordinal range must not collide with the first append request.
fn m2_batch_content_digest(records: &[CursorRecord]) -> Result<String, MigrationError> {
    let mut hasher = Sha256::new();
    hasher.update(b"ft.recorder.migration.m2.batch.v1\0");
    for record in records {
        hasher.update(record.offset.ordinal.to_le_bytes());
        let event_json = serde_json::to_vec(&record.event).map_err(|error| {
            MigrationError::StorageError(format!(
                "failed to serialize M2 event {} for batch identity: {error}",
                record.event.event_id
            ))
        })?;
        let event_len = u64::try_from(event_json.len()).map_err(|_| {
            MigrationError::StorageError(format!(
                "M2 event {} serialized length cannot fit in u64",
                record.event.event_id
            ))
        })?;
        hasher.update(event_len.to_le_bytes());
        hasher.update(event_json);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Derive the full-content idempotency digest for an M5 readiness marker.
///
/// `event_id.v1` remains the canonical recorder-event identity. This separate
/// domain-separated digest binds the storage retry key to every serialized
/// marker field, including fields intentionally outside the event-ID preimage.
fn readiness_marker_batch_digest(marker: &RecorderEvent) -> Result<String, MigrationError> {
    let encoded = serde_json::to_vec(marker).map_err(|error| {
        MigrationError::StorageError(format!(
            "failed to serialize readiness marker for batch identity: {error}"
        ))
    })?;
    let encoded_len = u64::try_from(encoded.len()).map_err(|_| {
        MigrationError::StorageError(
            "readiness marker serialized length cannot fit in u64".to_string(),
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(b"ft.recorder.migration.readiness.batch.v1\0");
    hasher.update(encoded_len.to_le_bytes());
    hasher.update(encoded);
    Ok(hex::encode(hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Migration errors
// ---------------------------------------------------------------------------

/// Errors that can occur during migration.
#[derive(Debug)]
pub enum MigrationError {
    /// Source storage is degraded, cannot migrate.
    SourceDegraded { last_error: Option<String> },
    /// Cursor/reader failure.
    CursorError(EventCursorError),
    /// Target storage rejected a write.
    TargetWriteError(String),
    /// Digest mismatch between export and import.
    DigestMismatch { expected: u64, actual: u64 },
    /// Event count mismatch between export and import.
    CountMismatch { expected: u64, actual: u64 },
    /// Source storage error (lag_metrics, read_checkpoint, etc.).
    StorageError(String),
    /// Target checkpoint commit was rejected.
    CheckpointCommitRejected { consumer: String, reason: String },
    /// A migration endpoint advertised a reserved or unavailable backend.
    BackendSelection(RecorderBackendSelectionError),
    /// A configured migration batch size was zero.
    InvalidBatchSize {
        stage: &'static str,
        field: &'static str,
    },
    /// A successful M2 append response violated the target contract.
    TargetAppendContractViolation { batch_id: String, reason: String },
    /// M0, M1, and M2 counts do not describe the same event set.
    ManifestCountMismatch {
        preflight_count: u64,
        export_count: u64,
        import_count: u64,
    },
    /// M1 and M2 digests do not describe the same ordinal sequence.
    ManifestDigestMismatch {
        export_digest: u64,
        import_digest: u64,
    },
    /// The readiness-marker ordinal cannot be represented.
    MarkerOrdinalOverflow { last_ordinal: u64 },
    /// Target was unhealthy before marker I/O; no marker was attempted.
    TargetDegradedBeforeMarker {
        backend: RecorderBackendKind,
        last_error: Option<String>,
    },
    /// A target health snapshot identified a different backend than the target.
    TargetIdentityMismatch {
        phase: &'static str,
        expected: RecorderBackendKind,
        actual: RecorderBackendKind,
        marker_offset: Option<RecorderOffset>,
    },
    /// A successful append response violated the one-event fsync contract.
    ReadinessMarkerContractViolation {
        marker_offset: RecorderOffset,
        reason: String,
    },
    /// Marker append succeeded, but the target degraded before readiness could
    /// be confirmed. The marker offset is retained for operator diagnosis.
    TargetDegradedAfterMarker {
        backend: RecorderBackendKind,
        marker_offset: RecorderOffset,
        last_error: Option<String>,
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
            Self::StorageError(e) => write!(f, "storage error: {e}"),
            Self::CheckpointCommitRejected { consumer, reason } => {
                write!(
                    f,
                    "checkpoint commit rejected for consumer {consumer}: {reason}"
                )
            }
            Self::BackendSelection(error) => write!(f, "backend selection error: {error}"),
            Self::InvalidBatchSize { stage, field } => {
                write!(f, "{stage} requires {field} >= 1")
            }
            Self::TargetAppendContractViolation { batch_id, reason } => write!(
                f,
                "M2 append contract violation for batch {batch_id}: {reason}"
            ),
            Self::ManifestCountMismatch {
                preflight_count,
                export_count,
                import_count,
            } => write!(
                f,
                "migration manifest count mismatch: preflight={preflight_count}, export={export_count}, import={import_count}"
            ),
            Self::ManifestDigestMismatch {
                export_digest,
                import_digest,
            } => write!(
                f,
                "migration manifest digest mismatch: export={export_digest:#x}, import={import_digest:#x}"
            ),
            Self::MarkerOrdinalOverflow { last_ordinal } => write!(
                f,
                "readiness marker ordinal overflow: last_ordinal={last_ordinal}"
            ),
            Self::TargetDegradedBeforeMarker {
                backend,
                last_error,
            } => write!(
                f,
                "target {backend} degraded before readiness marker: {last_error:?}"
            ),
            Self::TargetIdentityMismatch {
                phase,
                expected,
                actual,
                marker_offset,
            } => write!(
                f,
                "target backend identity mismatch during {phase}: expected={expected}, actual={actual}, marker_offset={marker_offset:?}"
            ),
            Self::ReadinessMarkerContractViolation {
                marker_offset,
                reason,
            } => {
                write!(
                    f,
                    "readiness marker append contract violation at ordinal {}: {reason}",
                    marker_offset.ordinal
                )
            }
            Self::TargetDegradedAfterMarker {
                backend,
                marker_offset,
                last_error,
            } => write!(
                f,
                "target {backend} degraded after readiness marker at ordinal {}: {last_error:?}",
                marker_offset.ordinal
            ),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<EventCursorError> for MigrationError {
    fn from(e: EventCursorError) -> Self {
        Self::CursorError(e)
    }
}

impl From<RecorderBackendSelectionError> for MigrationError {
    fn from(error: RecorderBackendSelectionError) -> Self {
        Self::BackendSelection(error)
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

/// Provides the implemented M0/M1/M2/M3/M5 migration stage helpers.
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

    /// Execute M0 preflight: health check and capture a manifest observation.
    ///
    /// Returns a manifest with event_count, first/last ordinal, and per-pane counts.
    /// Rejects if the source storage reports as degraded. This method does not
    /// own a source quiescence or snapshot fence; concurrent-source exclusion
    /// remains an external prerequisite.
    pub async fn m0_preflight<S: RecorderStorage>(
        &self,
        source_storage: &S,
        source_reader: &dyn RecorderEventReader,
    ) -> Result<MigrationManifest, MigrationError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.m0_preflight_with_cx(&cx, source_storage, source_reader)
            .await
    }

    /// Cx-first [`Self::m0_preflight`] (ft-xbnl0.2.3). Threads
    /// caller `&Cx` through the preflight sequence via checkpoint
    /// seams: pre-flight (before `storage.health()` touches the
    /// source), between the health check and the head offset
    /// read, and per-iteration during the cursor-scan loop that
    /// builds the manifest.
    ///
    /// The scan loop can be long on large ledgers (hundreds of
    /// thousands of events). Without per-batch cancellation the
    /// caller can't abort a scan — M0 is a no-op blocker but can
    /// still take minutes in practice. The checkpoint per batch
    /// keeps the loop responsive to operator cancel signals.
    pub async fn m0_preflight_with_cx<S: RecorderStorage>(
        &self,
        cx: &crate::cx::Cx,
        source_storage: &S,
        source_reader: &dyn RecorderEventReader,
    ) -> Result<MigrationManifest, MigrationError> {
        if self.config.export_batch_size == 0 {
            return Err(MigrationError::InvalidBatchSize {
                stage: "M0",
                field: "export_batch_size",
            });
        }
        select_recorder_backend(source_storage.backend_kind())?;

        cx.checkpoint()
            .map_err(|err| MigrationError::SourceDegraded {
                last_error: Some(format!("m0_preflight cancelled pre-start: {err}")),
            })?;

        // ft-xbnl0.2.3 tick 128: route through cx-first health
        // so caller cancellation propagates into the health probe.
        let health: RecorderStorageHealth = source_storage.health_with_cx(cx).await;
        if health.degraded {
            return Err(MigrationError::SourceDegraded {
                last_error: health.last_error,
            });
        }

        cx.checkpoint()
            .map_err(|err| MigrationError::SourceDegraded {
                last_error: Some(format!("m0_preflight cancelled after health check: {err}")),
            })?;

        let head = source_reader.head_offset()?;

        let mut cursor = source_reader.open_cursor_from_start()?;
        let mut event_count: u64 = 0;
        let mut first_ordinal: Option<u64> = None;
        let mut last_ordinal: u64 = 0;
        let mut per_pane_counts: HashMap<u64, u64> = HashMap::new();

        loop {
            cx.checkpoint().map_err(|err| {
                MigrationError::SourceDegraded {
                    last_error: Some(format!(
                        "m0_preflight cancelled mid-scan (event_count={event_count}, last_ordinal={last_ordinal}): {err}"
                    )),
                }
            })?;

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
            "preflight complete (cx)"
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
        if self.config.export_batch_size == 0 {
            return Err(MigrationError::InvalidBatchSize {
                stage: "M1",
                field: "export_batch_size",
            });
        }
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.m2_import_with_cx(&cx, target, records, manifest).await
    }

    /// Cx-first [`Self::m2_import`] (ft-xbnl0.2.3). Threads
    /// caller `&Cx` through the chunk-import loop via
    /// `cx.checkpoint()?` before each `target.append_batch` call.
    /// A pre-cancelled cx returns before any writes; a mid-import
    /// cancel leaves the target with all successfully-committed
    /// chunks intact. Re-running against a backend that still retains the
    /// content-bound batch receipts skips those chunks cleanly; this method
    /// does not persist a resume cursor itself.
    ///
    /// Digest + count verification runs AFTER the loop completes
    /// (no checkpoint there — the verification is pure CPU, and
    /// a cancellation at that point would lose the import
    /// context). If the caller cancels and the partial import
    /// exits early, the count/digest checks do NOT run, which is
    /// the right behavior: a partial import's digest won't match
    /// the full-export digest, so we'd get a spurious
    /// DigestMismatch on top of a cancellation.
    pub async fn m2_import_with_cx<T: RecorderStorage>(
        &self,
        cx: &crate::cx::Cx,
        target: &T,
        records: &[CursorRecord],
        manifest: &mut MigrationManifest,
    ) -> Result<(), MigrationError> {
        if self.config.import_batch_size == 0 {
            return Err(MigrationError::InvalidBatchSize {
                stage: "M2",
                field: "import_batch_size",
            });
        }
        let target_backend = target.backend_kind();
        select_recorder_backend(target_backend)?;

        cx.checkpoint().map_err(|err| {
            MigrationError::TargetWriteError(format!("m2_import cancelled pre-start: {err}"))
        })?;

        let mut import_digest = FNV1A_OFFSET_BASIS;
        let mut import_count: u64 = 0;

        for chunk in records.chunks(self.config.import_batch_size) {
            cx.checkpoint().map_err(|err| {
                MigrationError::TargetWriteError(format!(
                    "m2_import cancelled mid-import (import_count={import_count}): {err}"
                ))
            })?;

            let first_ord = chunk.first().map(|r| r.offset.ordinal).unwrap_or(0);
            let last_ord = chunk.last().map(|r| r.offset.ordinal).unwrap_or(0);
            let content_digest = m2_batch_content_digest(chunk)?;
            let batch_id = format!(
                "{}-{first_ord}-{last_ord}-{content_digest}",
                self.config.consumer_id
            );

            let events: Vec<_> = chunk.iter().map(|r| r.event.clone()).collect();

            let req = AppendRequest {
                batch_id: batch_id.clone(),
                events,
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 0,
            };

            // Tick 75 refactor: route through the Cx-first trait
            // sibling (tick 74) so cancellation propagates into the
            // storage write path itself, not just the between-chunk
            // seam. Backends that override
            // `append_batch_with_cx` with `timeout_with_cx`-aware
            // code will benefit; the default impl is
            // observationally equivalent to the prior
            // `append_batch` call.
            let append_response = target
                .append_batch_with_cx(cx, req)
                .await
                .map_err(|e| MigrationError::TargetWriteError(e.to_string()))?;

            if append_response.backend != target_backend {
                return Err(MigrationError::TargetAppendContractViolation {
                    batch_id,
                    reason: format!(
                        "response backend {} did not match target backend {target_backend}",
                        append_response.backend
                    ),
                });
            }
            if append_response.accepted_count != chunk.len() {
                return Err(MigrationError::TargetAppendContractViolation {
                    batch_id,
                    reason: format!(
                        "response accepted {} events, expected {}",
                        append_response.accepted_count,
                        chunk.len()
                    ),
                });
            }
            if !matches!(
                append_response.committed_durability,
                DurabilityLevel::Appended | DurabilityLevel::Fsync
            ) {
                return Err(MigrationError::TargetAppendContractViolation {
                    batch_id,
                    reason: format!(
                        "response durability {:?} did not satisfy appended",
                        append_response.committed_durability
                    ),
                });
            }
            let ordinal_span = u64::try_from(chunk.len().saturating_sub(1)).map_err(|_| {
                MigrationError::TargetAppendContractViolation {
                    batch_id: batch_id.clone(),
                    reason: "chunk ordinal span cannot fit in u64".to_string(),
                }
            })?;
            let expected_last_ordinal = append_response
                .first_offset
                .ordinal
                .checked_add(ordinal_span)
                .ok_or_else(|| MigrationError::TargetAppendContractViolation {
                    batch_id: batch_id.clone(),
                    reason: "response ordinal span overflowed u64".to_string(),
                })?;
            if append_response.last_offset.ordinal != expected_last_ordinal {
                return Err(MigrationError::TargetAppendContractViolation {
                    batch_id,
                    reason: format!(
                        "response offsets were not contiguous: first={}, last={}, expected_last={expected_last_ordinal}",
                        append_response.first_offset.ordinal, append_response.last_offset.ordinal
                    ),
                });
            }

            for record in chunk {
                import_digest = fnv1a_feed(import_digest, record.offset.ordinal);
                import_count += 1;
            }
        }

        if import_count != manifest.export_count {
            error!(
                migration_abort = true,
                stage = "M2",
                expected_count = manifest.export_count,
                actual_count = import_count,
                "count mismatch (cx)"
            );
            return Err(MigrationError::CountMismatch {
                expected: manifest.export_count,
                actual: import_count,
            });
        }

        if import_digest != manifest.export_digest {
            error!(
                migration_abort = true,
                stage = "M2",
                expected_digest = %format!("{:#x}", manifest.export_digest),
                actual_digest = %format!("{import_digest:#x}"),
                "digest mismatch (cx)"
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
            "import complete (cx)"
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // M3 — Checkpoint synchronization
    // -----------------------------------------------------------------------

    /// Execute M3 checkpoint sync: migrate consumer checkpoints from source to target.
    ///
    /// For each consumer discovered via `lag_metrics()`:
    /// 1. Read their checkpoint from source
    /// 2. Verify the checkpoint ordinal falls within the manifest's ordinal range
    /// 3. Commit the checkpoint to target
    /// 4. On ordinal gap, reset to first_ordinal (safe replay point)
    pub async fn m3_checkpoint_sync<S: RecorderStorage, T: RecorderStorage>(
        &self,
        source: &S,
        target: &T,
        manifest: &MigrationManifest,
    ) -> Result<CheckpointSyncResult, MigrationError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.m3_checkpoint_sync_with_cx(&cx, source, target, manifest)
            .await
    }

    /// Cx-first [`Self::m3_checkpoint_sync`] (ft-xbnl0.2.3).
    /// Threads caller `&Cx` through the per-consumer loop via
    /// checkpoint seams: pre-flight (before `source.lag_metrics`),
    /// per-iteration (before each consumer's read+commit pair),
    /// so caller cancellation aborts between consumers.
    ///
    /// Cancellation-safety contract: the per-iteration checkpoint
    /// fires BEFORE the `source.read_checkpoint` call for that
    /// consumer, so a mid-sync cancel leaves the target with a
    /// consistent set of fully-migrated consumers up to the abort
    /// point. Already-migrated consumers are NOT rolled back —
    /// the `commit_checkpoint` idempotency (Advanced /
    /// NoopAlreadyAdvanced outcomes) means re-running the consumer loop
    /// replays safely; this method does not persist a resume cursor itself.
    pub async fn m3_checkpoint_sync_with_cx<S: RecorderStorage, T: RecorderStorage>(
        &self,
        cx: &crate::cx::Cx,
        source: &S,
        target: &T,
        manifest: &MigrationManifest,
    ) -> Result<CheckpointSyncResult, MigrationError> {
        // Resolve both endpoints before touching either one. In particular, a
        // reserved target must fail before source lag/checkpoint reads begin.
        select_recorder_backend(source.backend_kind())?;
        select_recorder_backend(target.backend_kind())?;

        // Tick 75 refactor: use the Cx-first trait sibling (tick
        // 74) — absorbs the pre-flight checkpoint into the trait
        // default's cancellation seam.
        let lag = source
            .lag_metrics_with_cx(cx)
            .await
            .map_err(|e| MigrationError::StorageError(e.to_string()))?;

        let consumer_ids: Vec<CheckpointConsumerId> =
            lag.consumers.iter().map(|c| c.consumer.clone()).collect();

        info!(
            migration_stage = "M3",
            consumers = consumer_ids.len(),
            "checkpoint sync starting (cx)"
        );

        let mut result = CheckpointSyncResult {
            consumers_found: consumer_ids.len(),
            checkpoints_migrated: 0,
            checkpoints_reset: 0,
            reset_consumers: Vec::new(),
        };

        if consumer_ids.is_empty() {
            return Ok(result);
        }

        for consumer_id in &consumer_ids {
            // Tick 75 refactor: use the Cx-first trait sibling
            // for the per-consumer read. The iteration-context
            // error message from the previous explicit
            // `cx.checkpoint()?` is still preserved on the
            // surrounding pane — if `read_checkpoint_with_cx`
            // surfaces a cancellation, the default trait's error
            // message ("read_checkpoint cancelled pre-start")
            // combined with the `map_err` string gives enough
            // context (`StorageError(...)` wraps the cancellation
            // reason). Running counters are lost from the error
            // message but that's acceptable — operators can
            // inspect `result` state before the error for
            // progress.
            let checkpoint_opt = source
                .read_checkpoint_with_cx(cx, consumer_id)
                .await
                .map_err(|e| MigrationError::StorageError(e.to_string()))?;

            let checkpoint = match checkpoint_opt {
                Some(cp) => cp,
                None => continue,
            };

            let ordinal = checkpoint.upto_offset.ordinal;
            let in_range = ordinal >= manifest.first_ordinal && ordinal <= manifest.last_ordinal;

            let target_checkpoint = if in_range {
                debug!(
                    checkpoint_migrated = true,
                    consumer = %consumer_id.0,
                    ordinal = ordinal,
                    "migrating checkpoint as-is (cx)"
                );
                checkpoint
            } else {
                warn!(
                    checkpoint_reset = true,
                    consumer = %consumer_id.0,
                    original_ordinal = ordinal,
                    reset_ordinal = manifest.first_ordinal,
                    reason = "ordinal_gap",
                    "resetting checkpoint to safe replay point (cx)"
                );
                result.checkpoints_reset += 1;
                result.reset_consumers.push(consumer_id.0.clone());

                RecorderCheckpoint {
                    consumer: consumer_id.clone(),
                    upto_offset: RecorderOffset {
                        segment_id: 0,
                        byte_offset: 0,
                        ordinal: manifest.first_ordinal,
                    },
                    schema_version: checkpoint.schema_version,
                    committed_at_ms: checkpoint.committed_at_ms,
                }
            };

            // Tick 75 refactor: Cx-first commit via trait sibling.
            let outcome = target
                .commit_checkpoint_with_cx(cx, target_checkpoint)
                .await
                .map_err(|e| MigrationError::StorageError(e.to_string()))?;

            use crate::recorder_storage::CheckpointCommitOutcome;
            match outcome {
                CheckpointCommitOutcome::Advanced
                | CheckpointCommitOutcome::NoopAlreadyAdvanced => {
                    result.checkpoints_migrated += 1;
                }
                CheckpointCommitOutcome::RejectedOutOfOrder => {
                    return Err(MigrationError::CheckpointCommitRejected {
                        consumer: consumer_id.0.clone(),
                        reason: "rejected out of order".to_string(),
                    });
                }
            }
        }

        info!(
            migration_stage = "M3",
            migrated = result.checkpoints_migrated,
            reset = result.checkpoints_reset,
            "checkpoint sync complete (cx)"
        );

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // M5 — Readiness marker
    // -----------------------------------------------------------------------

    /// Execute M5 readiness verification and persist a durable marker.
    ///
    /// This method does **not** activate a backend selector. Before any marker
    /// I/O it verifies M0/M1/M2 count parity, M1/M2 digest parity, a checked
    /// marker ordinal, and target health/identity. After the fsync append it
    /// rechecks target health. A successful result means the target is ready
    /// for a separate selector authority to activate; it does not mean that
    /// activation occurred.
    pub async fn m5_mark_ready<T: RecorderStorage>(
        &self,
        target: &T,
        manifest: &MigrationManifest,
        epoch_ms: u64,
        source_path: Option<String>,
    ) -> Result<CutoverReadinessResult, MigrationError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.m5_mark_ready_with_cx(&cx, target, manifest, epoch_ms, source_path)
            .await
    }

    /// Cx-first [`Self::m5_mark_ready`] (ft-xbnl0.2.3).
    ///
    /// A pre-cancelled context leaves the target untouched. Cancellation after
    /// append is surfaced by `health_with_cx` as
    /// [`MigrationError::TargetDegradedAfterMarker`], which records the durable
    /// marker offset and never claims readiness or activation.
    pub async fn m5_mark_ready_with_cx<T: RecorderStorage>(
        &self,
        cx: &crate::cx::Cx,
        target: &T,
        manifest: &MigrationManifest,
        epoch_ms: u64,
        source_path: Option<String>,
    ) -> Result<CutoverReadinessResult, MigrationError> {
        let target_backend = target.backend_kind();
        select_recorder_backend(target_backend)?;

        cx.checkpoint().map_err(|err| {
            MigrationError::TargetWriteError(format!("m5_mark_ready cancelled pre-start: {err}"))
        })?;

        if manifest.event_count != manifest.export_count
            || manifest.export_count != manifest.import_count
        {
            return Err(MigrationError::ManifestCountMismatch {
                preflight_count: manifest.event_count,
                export_count: manifest.export_count,
                import_count: manifest.import_count,
            });
        }
        if manifest.export_digest != manifest.import_digest {
            return Err(MigrationError::ManifestDigestMismatch {
                export_digest: manifest.export_digest,
                import_digest: manifest.import_digest,
            });
        }

        let marker_sequence =
            manifest
                .last_ordinal
                .checked_add(1)
                .ok_or(MigrationError::MarkerOrdinalOverflow {
                    last_ordinal: manifest.last_ordinal,
                })?;

        let preflight_health = target.health_with_cx(cx).await;
        if preflight_health.backend != target_backend {
            return Err(MigrationError::TargetIdentityMismatch {
                phase: "pre-marker health check",
                expected: target_backend,
                actual: preflight_health.backend,
                marker_offset: None,
            });
        }
        if preflight_health.degraded {
            return Err(MigrationError::TargetDegradedBeforeMarker {
                backend: target_backend,
                last_error: preflight_health.last_error,
            });
        }

        let mut marker_event = RecorderEvent {
            schema_version: "ft.recorder.event.v1".to_string(),
            event_id: String::new(),
            pane_id: 0,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RecoveryFlow,
            occurred_at_ms: epoch_ms,
            recorded_at_ms: epoch_ms,
            sequence: marker_sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: RecorderLifecyclePhase::MigrationReadyForActivation,
                reason: Some("migration_target_ready_for_external_activation".to_string()),
                details: serde_json::json!({
                    "migration_type": "recorder_target_readiness",
                    "target_backend": target_backend,
                    "event_count": manifest.event_count,
                    "export_digest": format!("{:#x}", manifest.export_digest),
                    "epoch_ms": epoch_ms,
                    "selector_activated": false,
                    "selector_authority": "external_to_recorder_migration",
                }),
            },
        };
        marker_event.event_id = crate::event_id::generate_event_id_v1(&marker_event);
        let marker_batch_digest = readiness_marker_batch_digest(&marker_event)?;

        let req = AppendRequest {
            batch_id: format!("ft-recorder-migration-readiness-{marker_batch_digest}"),
            events: vec![marker_event],
            required_durability: DurabilityLevel::Fsync,
            producer_ts_ms: epoch_ms,
        };

        // Tick 75 refactor: Cx-first marker append via trait
        // sibling. A backend with internal cancellation support
        // (via `timeout_with_cx`) can short-circuit the fsync'd
        // write rather than waiting for a timeout.
        let append_response = target
            .append_batch_with_cx(cx, req)
            .await
            .map_err(|e| MigrationError::TargetWriteError(e.to_string()))?;

        if append_response.backend != target_backend {
            return Err(MigrationError::ReadinessMarkerContractViolation {
                marker_offset: append_response.last_offset.clone(),
                reason: format!(
                    "append response backend {} did not match target backend {target_backend}",
                    append_response.backend
                ),
            });
        }
        if append_response.accepted_count != 1 {
            return Err(MigrationError::ReadinessMarkerContractViolation {
                marker_offset: append_response.last_offset.clone(),
                reason: format!(
                    "append response accepted {} events instead of exactly 1",
                    append_response.accepted_count
                ),
            });
        }
        if append_response.committed_durability != DurabilityLevel::Fsync {
            return Err(MigrationError::ReadinessMarkerContractViolation {
                marker_offset: append_response.last_offset.clone(),
                reason: format!(
                    "append response durability {:?} did not satisfy fsync",
                    append_response.committed_durability
                ),
            });
        }

        // Do not insert a separate checkpoint here: health_with_cx converts a
        // cancellation into a degraded snapshot, allowing the error to retain
        // the durable marker offset instead of reporting a generic write error.
        let postflight_health = target.health_with_cx(cx).await;
        if postflight_health.backend != target_backend {
            return Err(MigrationError::TargetIdentityMismatch {
                phase: "post-marker health check",
                expected: target_backend,
                actual: postflight_health.backend,
                marker_offset: Some(append_response.last_offset.clone()),
            });
        }
        if postflight_health.degraded {
            return Err(MigrationError::TargetDegradedAfterMarker {
                backend: target_backend,
                marker_offset: append_response.last_offset,
                last_error: postflight_health.last_error,
            });
        }

        if let Some(source_path_hint) = source_path.as_deref() {
            info!(
                source_path_hint_reported = true,
                path = %source_path_hint,
                "caller reported a source path hint; filesystem retention is not verified"
            );
        }

        let result = CutoverReadinessResult {
            target_backend,
            migration_epoch_ms: epoch_ms,
            marker_offset: append_response.last_offset,
            source_path_hint: source_path,
        };

        info!(
            migration_stage = "M5",
            readiness_marker_durable = true,
            selector_activated = false,
            backend = %target_backend,
            epoch = %epoch_ms,
            marker_ordinal = result.marker_offset.ordinal,
            "target ready for external selector activation (cx)"
        );

        Ok(result)
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_m0_m2_with_cx(&cx, source_storage, source_reader, target)
            .await
    }

    /// Cx-first [`Self::run_m0_m2`] (ft-xbnl0.2.3). Composite
    /// pipeline that chains all three Cx-first M-stage methods:
    ///
    ///   1. `m0_preflight_with_cx` (tick 67) — per-batch
    ///      checkpointed scan to build the manifest.
    ///   2. `m1_export` — synchronous; no cx threading needed
    ///      because the function is CPU-only (reader cursor
    ///      traversal + FNV digest accumulation with no await
    ///      points inside the per-record loop). An extra
    ///      `cx.checkpoint()?` is placed BEFORE m1 so a cancel
    ///      between m0 and m1 aborts the pipeline cleanly.
    ///   3. `m2_import_with_cx` (tick 68) — per-chunk
    ///      checkpointed write to target.
    ///
    /// Cancellation-safety: caller cancellation is honored at
    /// every await point across the 3 stages, AND at an
    /// additional checkpoint between m0 and m1 (since m1 is
    /// synchronous and takes time on large ledgers). The
    /// documented cancellation contracts on the component
    /// methods still apply:
    ///   - m0 cancel: manifest never returned, no target writes.
    ///   - m1 cancel: m1 is sync, so cancellation only fires at
    ///     the boundary checkpoint (before or after m1).
    ///   - m2 cancel: already-committed chunks on target remain; a full rerun
    ///     can reuse them while the target retains their batch receipts.
    pub async fn run_m0_m2_with_cx<S: RecorderStorage, T: RecorderStorage>(
        &self,
        cx: &crate::cx::Cx,
        source_storage: &S,
        source_reader: &dyn RecorderEventReader,
        target: &T,
    ) -> Result<MigrationManifest, MigrationError> {
        // M0 (tick 67)
        let mut manifest = self
            .m0_preflight_with_cx(cx, source_storage, source_reader)
            .await?;

        // Boundary checkpoint: m1 is synchronous so no internal
        // cancellation seam exists; catching a cancel here
        // prevents wasted sync CPU on the export loop.
        cx.checkpoint().map_err(|err| {
            MigrationError::StorageError(format!(
                "run_m0_m2 cancelled between m0 and m1 (event_count={}): {err}",
                manifest.event_count
            ))
        })?;

        // M1 (sync)
        let records = self.m1_export(source_reader, &mut manifest)?;

        // M2 (tick 68)
        self.m2_import_with_cx(cx, target, &records, &mut manifest)
            .await?;

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
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // -----------------------------------------------------------------------
    // Async test helper
    // -----------------------------------------------------------------------

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build recorder_migration test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

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
        degrade_after_append: AtomicBool,
        health_calls: AtomicUsize,
        response_accepted_count: Mutex<Option<usize>>,
    }

    impl MockMigrationStorage {
        fn healthy() -> Self {
            Self {
                health: RecorderStorageHealth {
                    backend: RecorderBackendKind::Rusqlite,
                    degraded: false,
                    queue_depth: 0,
                    queue_capacity: 100,
                    latest_offset: None,
                    last_error: None,
                },
                appended: Mutex::new(Vec::new()),
                fail_append: AtomicBool::new(false),
                degrade_after_append: AtomicBool::new(false),
                health_calls: AtomicUsize::new(0),
                response_accepted_count: Mutex::new(None),
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
                degrade_after_append: AtomicBool::new(false),
                health_calls: AtomicUsize::new(0),
                response_accepted_count: Mutex::new(None),
            }
        }

        fn reserved() -> Self {
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
                degrade_after_append: AtomicBool::new(false),
                health_calls: AtomicUsize::new(0),
                response_accepted_count: Mutex::new(None),
            }
        }

        fn degrades_after_append() -> Self {
            let storage = Self::healthy();
            storage.degrade_after_append.store(true, Ordering::Relaxed);
            storage
        }

        fn with_response_accepted_count(accepted_count: usize) -> Self {
            let storage = Self::healthy();
            *storage.response_accepted_count.lock().unwrap() = Some(accepted_count);
            storage
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

        fn append_batch(
            &self,
            req: AppendRequest,
        ) -> impl std::future::Future<Output = std::result::Result<AppendResponse, RecorderStorageError>>
        {
            if self.fail_append.load(Ordering::Relaxed) {
                return std::future::ready(Err(RecorderStorageError::QueueFull { capacity: 0 }));
            }
            let count = req.events.len();
            let accepted_count = self
                .response_accepted_count
                .lock()
                .unwrap()
                .unwrap_or(count);
            let committed_durability = req.required_durability;
            let first_ord = 0_u64;
            let last_ord = count.saturating_sub(1) as u64;
            self.appended.lock().unwrap().push(req);
            std::future::ready(Ok(AppendResponse {
                backend: self.health.backend,
                accepted_count,
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
                committed_durability,
                committed_at_ms: 0,
                was_idempotent_replay: false,
            }))
        }

        fn flush(
            &self,
            _mode: FlushMode,
        ) -> impl std::future::Future<Output = std::result::Result<FlushStats, RecorderStorageError>>
        {
            std::future::ready(Ok(FlushStats {
                backend: self.health.backend,
                flushed_at_ms: 0,
                latest_offset: None,
            }))
        }

        fn read_checkpoint(
            &self,
            _consumer: &CheckpointConsumerId,
        ) -> impl std::future::Future<
            Output = std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError>,
        > {
            std::future::ready(Ok(None))
        }

        fn commit_checkpoint(
            &self,
            _checkpoint: RecorderCheckpoint,
        ) -> impl std::future::Future<
            Output = std::result::Result<CheckpointCommitOutcome, RecorderStorageError>,
        > {
            std::future::ready(Ok(CheckpointCommitOutcome::Advanced))
        }

        fn health(&self) -> impl std::future::Future<Output = RecorderStorageHealth> {
            self.health_calls.fetch_add(1, Ordering::Relaxed);
            let degraded_after_append = self.degrade_after_append.load(Ordering::Relaxed)
                && !self.appended.lock().unwrap().is_empty();
            std::future::ready(if degraded_after_append {
                RecorderStorageHealth {
                    degraded: true,
                    last_error: Some("degraded after readiness marker".to_string()),
                    ..self.health.clone()
                }
            } else {
                self.health.clone()
            })
        }

        fn lag_metrics(
            &self,
        ) -> impl std::future::Future<
            Output = std::result::Result<RecorderStorageLag, RecorderStorageError>,
        > {
            std::future::ready(Ok(RecorderStorageLag {
                latest_offset: None,
                consumers: vec![],
            }))
        }
    }

    // -----------------------------------------------------------------------
    // M0 tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_m0_captures_manifest_with_correct_counts() {
        run_async_test(async {
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
        run_async_test(async {
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

    /// ft-xbnl0.2.3 Cx-first: `m0_preflight_with_cx` must match
    /// the legacy `m0_preflight` for an uncancelled cx. Uses the
    /// same 5-record fixture as `test_m0_captures_manifest_with_correct_counts`
    /// to prove the manifest counts are bit-for-bit identical.
    #[test]
    fn m0_preflight_with_cx_matches_legacy() {
        run_async_test(async {
            let records = vec![
                make_cursor_record(1, 0),
                make_cursor_record(1, 1),
                make_cursor_record(2, 2),
                make_cursor_record(1, 3),
                make_cursor_record(3, 4),
            ];

            // Legacy path
            let reader_legacy = TestEventReader::new(records.clone());
            let storage_legacy = MockMigrationStorage::healthy();
            let engine_legacy = MigrationEngine::new(MigrationConfig::default());
            let legacy = engine_legacy
                .m0_preflight(&storage_legacy, &reader_legacy)
                .await
                .unwrap();

            // Cx-first path
            let reader_cx = TestEventReader::new(records);
            let storage_cx = MockMigrationStorage::healthy();
            let engine_cx = MigrationEngine::new(MigrationConfig::default());
            let cx = crate::cx::for_request();
            let cx_first = engine_cx
                .m0_preflight_with_cx(&cx, &storage_cx, &reader_cx)
                .await
                .unwrap();

            assert_eq!(legacy.event_count, cx_first.event_count);
            assert_eq!(legacy.first_ordinal, cx_first.first_ordinal);
            assert_eq!(legacy.last_ordinal, cx_first.last_ordinal);
            assert_eq!(legacy.per_pane_counts, cx_first.per_pane_counts);
            assert_eq!(legacy.last_offset, cx_first.last_offset);
            assert_eq!(cx_first.event_count, 5);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `m0_preflight_with_cx` must reject
    /// a degraded source with SourceDegraded, same as the legacy
    /// path. Proves the health-check branch short-circuits on
    /// the Cx path too.
    #[test]
    fn m0_preflight_with_cx_rejects_degraded_source() {
        run_async_test(async {
            let reader = TestEventReader::new(vec![]);
            let storage = MockMigrationStorage::degraded();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let cx = crate::cx::for_request();

            let result = engine.m0_preflight_with_cx(&cx, &storage, &reader).await;
            assert!(result.is_err());
            let msg = format!("{}", result.unwrap_err());
            assert!(
                msg.contains("degraded"),
                "Cx-first error should mention degraded: {msg}"
            );
        });
    }

    #[test]
    fn test_m0_empty_source_produces_zero_counts() {
        run_async_test(async {
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

    #[test]
    fn test_m2_imports_preserving_ordinals() {
        run_async_test(async {
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
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `m2_import_with_cx` must match the
    /// legacy `m2_import` for an uncancelled cx. Exercises the
    /// chunk-import loop with import_batch_size=2 (forces
    /// multiple chunks from 3 records) and asserts:
    ///   * same total events written to target
    ///   * same import_count and import_digest in the manifest
    ///   * import_digest matches export_digest (the full
    ///     round-trip parity check the legacy test already makes)
    #[test]
    fn m2_import_with_cx_matches_legacy() {
        run_async_test(async {
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

            let reader = TestEventReader::new(records);
            let exported = engine.m1_export(&reader, &mut manifest).unwrap();

            let target = MockMigrationStorage::healthy();
            let cx = crate::cx::for_request();
            engine
                .m2_import_with_cx(&cx, &target, &exported, &mut manifest)
                .await
                .unwrap();

            assert_eq!(target.total_events_appended(), 3);
            assert_eq!(manifest.import_count, 3);
            assert_eq!(
                manifest.import_digest, manifest.export_digest,
                "Cx-first m2_import must produce digest matching the export"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `m2_import_with_cx` must detect
    /// digest mismatches the same as the legacy path. Proves the
    /// verification branches still run on the Cx-first path.
    #[test]
    fn m2_import_with_cx_digest_mismatch_aborts() {
        run_async_test(async {
            let records = vec![make_cursor_record(1, 0), make_cursor_record(1, 1)];
            let engine = MigrationEngine::new(MigrationConfig::default());
            let mut manifest = MigrationManifest::default();

            let reader = TestEventReader::new(records);
            let exported = engine.m1_export(&reader, &mut manifest).unwrap();

            // Tamper with the digest so the Cx-first path surfaces the mismatch.
            manifest.export_digest = 0xDEADBEEF;

            let target = MockMigrationStorage::healthy();
            let cx = crate::cx::for_request();
            let result = engine
                .m2_import_with_cx(&cx, &target, &exported, &mut manifest)
                .await;
            assert!(result.is_err());
            let msg = format!("{}", result.unwrap_err());
            assert!(
                msg.contains("digest mismatch"),
                "Cx-first error should mention digest mismatch: {msg}"
            );
        });
    }

    #[test]
    fn test_m2_digest_match_passes() {
        run_async_test(async {
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
        run_async_test(async {
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
        run_async_test(async {
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

    // -----------------------------------------------------------------------
    // End-to-end M0→M2 pipeline
    // -----------------------------------------------------------------------

    #[test]
    fn test_m0_m2_pipeline_end_to_end() {
        run_async_test(async {
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

    /// ft-xbnl0.2.3 Cx-first: `run_m0_m2_with_cx` composite must
    /// produce a bit-for-bit identical manifest to the legacy
    /// `run_m0_m2` for the same inputs. Exercises the full
    /// M0 → M1 → M2 pipeline under the Cx-first path and
    /// verifies the import digest matches the export digest
    /// (round-trip parity).
    #[test]
    fn run_m0_m2_with_cx_matches_legacy() {
        run_async_test(async {
            let records = vec![
                make_cursor_record(1, 0),
                make_cursor_record(1, 1),
                make_cursor_record(2, 2),
                make_cursor_record(3, 3),
                make_cursor_record(1, 4),
            ];

            // Legacy path
            let reader_legacy = TestEventReader::new(records.clone());
            let source_legacy = MockMigrationStorage::healthy();
            let target_legacy = MockMigrationStorage::healthy();
            let engine_legacy = MigrationEngine::new(MigrationConfig {
                export_batch_size: 2,
                import_batch_size: 3,
                consumer_id: "legacy-composite".to_string(),
            });
            let m_legacy = engine_legacy
                .run_m0_m2(&source_legacy, &reader_legacy, &target_legacy)
                .await
                .unwrap();

            // Cx-first path
            let reader_cx = TestEventReader::new(records);
            let source_cx = MockMigrationStorage::healthy();
            let target_cx = MockMigrationStorage::healthy();
            let engine_cx = MigrationEngine::new(MigrationConfig {
                export_batch_size: 2,
                import_batch_size: 3,
                consumer_id: "cx-composite".to_string(),
            });
            let cx = crate::cx::for_request();
            let m_cx = engine_cx
                .run_m0_m2_with_cx(&cx, &source_cx, &reader_cx, &target_cx)
                .await
                .unwrap();

            // Manifest parity across the M0 + M1 + M2 stages.
            assert_eq!(m_legacy.event_count, m_cx.event_count);
            assert_eq!(m_legacy.first_ordinal, m_cx.first_ordinal);
            assert_eq!(m_legacy.last_ordinal, m_cx.last_ordinal);
            assert_eq!(m_legacy.export_count, m_cx.export_count);
            assert_eq!(m_legacy.import_count, m_cx.import_count);
            assert_eq!(m_legacy.export_digest, m_cx.export_digest);
            assert_eq!(m_legacy.import_digest, m_cx.import_digest);
            assert_eq!(m_legacy.per_pane_counts, m_cx.per_pane_counts);

            // Absolute invariants
            assert_eq!(m_cx.event_count, 5);
            assert_eq!(m_cx.import_digest, m_cx.export_digest);
            assert_eq!(target_cx.total_events_appended(), 5);
        });
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
        assert!(!MigrationStage::M5Readiness.is_complete());
    }

    #[test]
    fn test_migration_stage_can_rollback() {
        assert!(MigrationStage::M0Preflight.can_rollback());
        assert!(MigrationStage::M1Export.can_rollback());
        assert!(MigrationStage::M2Import.can_rollback());
        assert!(MigrationStage::M3CheckpointSync.can_rollback());
        assert!(!MigrationStage::M4Reserved.can_rollback());
        assert!(MigrationStage::M5Readiness.can_rollback());
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
        let mut manifest = MigrationManifest {
            event_count: 42,
            first_ordinal: 1,
            last_ordinal: 42,
            export_digest: 0xCAFE,
            export_count: 42,
            ..Default::default()
        };
        manifest.per_pane_counts.insert(1, 20);
        manifest.per_pane_counts.insert(2, 22);

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

    #[test]
    fn test_m0_m2_with_batch_size_one() {
        run_async_test(async {
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
        run_async_test(async {
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

    // -----------------------------------------------------------------------
    // Batch ID idempotency test
    // -----------------------------------------------------------------------

    #[test]
    fn test_m2_batch_ids_contain_ordinal_range() {
        run_async_test(async {
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

        let manifest = RuntimeBuilder::current_thread()
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

    #[test]
    fn test_m2_count_mismatch_detected() {
        run_async_test(async {
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

    #[test]
    fn test_m0_rejects_reserved_source_before_health_io() {
        run_async_test(async {
            let source = MockMigrationStorage::reserved();
            let reader = TestEventReader::new(vec![make_cursor_record(1, 0)]);
            let engine = MigrationEngine::new(MigrationConfig::default());

            let error = engine.m0_preflight(&source, &reader).await.unwrap_err();

            assert!(matches!(error, MigrationError::BackendSelection(_)));
            assert_eq!(source.health_calls.load(Ordering::Relaxed), 0);
        });
    }

    #[test]
    fn test_m2_rejects_reserved_target_before_append_io() {
        run_async_test(async {
            let target = MockMigrationStorage::reserved();
            let records = vec![make_cursor_record(1, 0)];
            let reader = TestEventReader::new(records);
            let engine = MigrationEngine::new(MigrationConfig::default());
            let mut manifest = MigrationManifest::default();
            let exported = engine.m1_export(&reader, &mut manifest).unwrap();

            let error = engine
                .m2_import(&target, &exported, &mut manifest)
                .await
                .unwrap_err();

            assert!(matches!(error, MigrationError::BackendSelection(_)));
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_zero_batch_sizes_fail_typed_before_storage_io() {
        run_async_test(async {
            let source = MockMigrationStorage::healthy();
            let reader = TestEventReader::new(vec![make_cursor_record(1, 0)]);
            let export_zero = MigrationEngine::new(MigrationConfig {
                export_batch_size: 0,
                ..MigrationConfig::default()
            });

            let m0_error = export_zero
                .m0_preflight(&source, &reader)
                .await
                .unwrap_err();
            assert!(matches!(
                m0_error,
                MigrationError::InvalidBatchSize {
                    stage: "M0",
                    field: "export_batch_size"
                }
            ));
            assert_eq!(source.health_calls.load(Ordering::Relaxed), 0);

            let mut manifest = MigrationManifest::default();
            let m1_error = export_zero.m1_export(&reader, &mut manifest).unwrap_err();
            assert!(matches!(
                m1_error,
                MigrationError::InvalidBatchSize {
                    stage: "M1",
                    field: "export_batch_size"
                }
            ));

            let target = MockMigrationStorage::healthy();
            let import_zero = MigrationEngine::new(MigrationConfig {
                import_batch_size: 0,
                ..MigrationConfig::default()
            });
            let m2_error = import_zero
                .m2_import(&target, &[make_cursor_record(1, 0)], &mut manifest)
                .await
                .unwrap_err();
            assert!(matches!(
                m2_error,
                MigrationError::InvalidBatchSize {
                    stage: "M2",
                    field: "import_batch_size"
                }
            ));
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_m2_batch_identity_is_bound_to_event_content() {
        run_async_test(async {
            let engine = MigrationEngine::new(MigrationConfig::default());
            let mut first_record = make_cursor_record(1, 7);
            let mut second_record = first_record.clone();
            first_record.event.event_id = "content-a".to_string();
            second_record.event.event_id = "content-b".to_string();

            let first_target = MockMigrationStorage::healthy();
            let second_target = MockMigrationStorage::healthy();
            let mut first_manifest = MigrationManifest {
                export_count: 1,
                export_digest: fnv1a_feed(FNV1A_OFFSET_BASIS, 7),
                ..Default::default()
            };
            let mut second_manifest = first_manifest.clone();

            engine
                .m2_import(&first_target, &[first_record], &mut first_manifest)
                .await
                .unwrap();
            engine
                .m2_import(&second_target, &[second_record], &mut second_manifest)
                .await
                .unwrap();

            let first_batches = first_target.appended.lock().unwrap();
            let second_batches = second_target.appended.lock().unwrap();
            assert_ne!(first_batches[0].batch_id, second_batches[0].batch_id);
        });
    }

    #[test]
    fn test_m2_rejects_success_response_with_wrong_accepted_count() {
        run_async_test(async {
            let target = MockMigrationStorage::with_response_accepted_count(0);
            let record = make_cursor_record(1, 0);
            let mut manifest = MigrationManifest {
                export_count: 1,
                export_digest: fnv1a_feed(FNV1A_OFFSET_BASIS, 0),
                ..Default::default()
            };
            let engine = MigrationEngine::new(MigrationConfig::default());

            let error = engine
                .m2_import(&target, &[record], &mut manifest)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                MigrationError::TargetAppendContractViolation { .. }
            ));
            assert_eq!(manifest.import_count, 0);
            assert_eq!(target.appended.lock().unwrap().len(), 1);
        });
    }

    // -----------------------------------------------------------------------
    // M3 mock storage with checkpoint support
    // -----------------------------------------------------------------------

    use crate::recorder_storage::RecorderConsumerLag;

    /// Mock storage with configurable checkpoints and lag consumers.
    struct MockCheckpointStorage {
        health: RecorderStorageHealth,
        checkpoints: Mutex<HashMap<String, RecorderCheckpoint>>,
        consumers: Vec<RecorderConsumerLag>,
        committed: Mutex<Vec<RecorderCheckpoint>>,
        reject_commit: AtomicBool,
        lag_calls: AtomicUsize,
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
                lag_calls: AtomicUsize::new(0),
            }
        }

        fn empty_target() -> Self {
            Self::new(vec![], HashMap::new())
        }

        fn reserved_target() -> Self {
            let mut target = Self::empty_target();
            target.health.backend = RecorderBackendKind::FrankenSqlite;
            target
        }
    }

    impl RecorderStorage for MockCheckpointStorage {
        fn backend_kind(&self) -> RecorderBackendKind {
            self.health.backend
        }

        fn append_batch(
            &self,
            _req: AppendRequest,
        ) -> impl std::future::Future<Output = std::result::Result<AppendResponse, RecorderStorageError>>
        {
            std::future::ready(Ok(AppendResponse {
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
                was_idempotent_replay: false,
            }))
        }

        fn flush(
            &self,
            _mode: FlushMode,
        ) -> impl std::future::Future<Output = std::result::Result<FlushStats, RecorderStorageError>>
        {
            std::future::ready(Ok(FlushStats {
                backend: self.health.backend,
                flushed_at_ms: 0,
                latest_offset: None,
            }))
        }

        fn read_checkpoint(
            &self,
            consumer: &CheckpointConsumerId,
        ) -> impl std::future::Future<
            Output = std::result::Result<Option<RecorderCheckpoint>, RecorderStorageError>,
        > {
            std::future::ready(Ok(self
                .checkpoints
                .lock()
                .unwrap()
                .get(&consumer.0)
                .cloned()))
        }

        fn commit_checkpoint(
            &self,
            checkpoint: RecorderCheckpoint,
        ) -> impl std::future::Future<
            Output = std::result::Result<CheckpointCommitOutcome, RecorderStorageError>,
        > {
            if self.reject_commit.load(Ordering::Relaxed) {
                return std::future::ready(Ok(CheckpointCommitOutcome::RejectedOutOfOrder));
            }
            self.committed.lock().unwrap().push(checkpoint);
            std::future::ready(Ok(CheckpointCommitOutcome::Advanced))
        }

        fn health(&self) -> impl std::future::Future<Output = RecorderStorageHealth> {
            std::future::ready(self.health.clone())
        }

        fn lag_metrics(
            &self,
        ) -> impl std::future::Future<
            Output = std::result::Result<RecorderStorageLag, RecorderStorageError>,
        > {
            self.lag_calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(RecorderStorageLag {
                latest_offset: None,
                consumers: self.consumers.clone(),
            }))
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

    // -----------------------------------------------------------------------
    // M3 tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_m3_rejects_reserved_target_before_source_io() {
        run_async_test(async {
            let source = MockCheckpointStorage::new(vec![], HashMap::new());
            let target = MockCheckpointStorage::reserved_target();
            let engine = MigrationEngine::new(MigrationConfig::default());

            let error = engine
                .m3_checkpoint_sync(&source, &target, &MigrationManifest::default())
                .await
                .unwrap_err();

            assert!(matches!(error, MigrationError::BackendSelection(_)));
            assert_eq!(source.lag_calls.load(Ordering::Relaxed), 0);
            assert!(target.committed.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_m3_migrates_all_consumer_checkpoints() {
        run_async_test(async {
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

    /// ft-xbnl0.2.3 Cx-first: `m3_checkpoint_sync_with_cx` must
    /// match the legacy `m3_checkpoint_sync` for an uncancelled cx.
    /// Mirrors the two-consumer topology from
    /// `test_m3_migrates_all_consumer_checkpoints` and asserts
    /// CheckpointSyncResult parity across all fields.
    #[test]
    fn m3_checkpoint_sync_with_cx_matches_legacy() {
        run_async_test(async {
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

            let cx = crate::cx::for_request();
            let result = engine
                .m3_checkpoint_sync_with_cx(&cx, &source, &target, &manifest)
                .await
                .unwrap();

            assert_eq!(result.consumers_found, 2);
            assert_eq!(result.checkpoints_migrated, 2);
            assert_eq!(result.checkpoints_reset, 0);
            assert!(result.reset_consumers.is_empty());
            assert_eq!(target.committed.lock().unwrap().len(), 2);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `m3_checkpoint_sync_with_cx` must
    /// reset an out-of-range checkpoint the same as the legacy
    /// path (same branch as `test_m3_rejects_checkpoint_referencing_missing_ordinal`).
    /// Proves the reset branch fires on the Cx path too.
    #[test]
    fn m3_checkpoint_sync_with_cx_resets_out_of_range() {
        run_async_test(async {
            let consumers = vec![make_consumer_lag("idx", 0)];
            let mut checkpoints = HashMap::new();
            // Checkpoint ordinal 100 is WAY past the manifest range
            // [5, 20], so it must be reset to first_ordinal=5.
            checkpoints.insert("idx".to_string(), make_checkpoint("idx", 100));

            let source = MockCheckpointStorage::new(consumers, checkpoints);
            let target = MockCheckpointStorage::empty_target();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let cx = crate::cx::for_request();

            let manifest = MigrationManifest {
                first_ordinal: 5,
                last_ordinal: 20,
                ..Default::default()
            };

            let result = engine
                .m3_checkpoint_sync_with_cx(&cx, &source, &target, &manifest)
                .await
                .unwrap();

            assert_eq!(result.checkpoints_reset, 1);
            assert_eq!(result.reset_consumers, vec!["idx".to_string()]);
            assert_eq!(result.checkpoints_migrated, 1);
        });
    }

    #[test]
    fn test_m3_preserves_checkpoint_monotonicity() {
        run_async_test(async {
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
        run_async_test(async {
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
        run_async_test(async {
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
        run_async_test(async {
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
        run_async_test(async {
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
        run_async_test(async {
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
    fn test_checkpoint_sync_result_serialize_roundtrip() {
        let result = CheckpointSyncResult {
            consumers_found: 3,
            checkpoints_migrated: 2,
            checkpoints_reset: 1,
            reset_consumers: vec!["stale".to_string()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: CheckpointSyncResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, restored);
    }

    #[test]
    fn test_m3_consumer_without_checkpoint_skipped() {
        run_async_test(async {
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

    // -----------------------------------------------------------------------
    // New error variant display tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_migration_error_storage_error_display() {
        let err = MigrationError::StorageError("connection refused".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("connection refused"), "msg: {msg}");
    }

    #[test]
    fn test_migration_error_checkpoint_commit_rejected_display() {
        let err = MigrationError::CheckpointCommitRejected {
            consumer: "idx".to_string(),
            reason: "out of order".to_string(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("idx"), "msg: {msg}");
        assert!(msg.contains("out of order"), "msg: {msg}");
    }

    // -----------------------------------------------------------------------
    // M5 readiness tests
    // -----------------------------------------------------------------------

    fn verified_manifest(event_count: u64, last_ordinal: u64, digest: u64) -> MigrationManifest {
        MigrationManifest {
            event_count,
            first_ordinal: 0,
            last_ordinal,
            export_digest: digest,
            export_count: event_count,
            import_digest: digest,
            import_count: event_count,
            ..Default::default()
        }
    }

    #[test]
    fn test_m5_emits_readiness_marker_without_activation_claim() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = verified_manifest(100, 99, 0xCAFE);

            let result = engine
                .m5_mark_ready(&target, &manifest, 1708000000, None)
                .await
                .unwrap();

            assert_eq!(result.target_backend, RecorderBackendKind::Rusqlite);
            assert_eq!(result.migration_epoch_ms, 1708000000);
            assert_eq!(result.marker_offset.ordinal, 0);
            assert!(result.source_path_hint.is_none());

            let appended = target.appended.lock().unwrap();
            assert_eq!(appended.len(), 1);
            assert_eq!(appended[0].events.len(), 1);

            let marker = &appended[0].events[0];
            assert_eq!(marker.event_id.len(), 64);
            assert!(
                marker
                    .event_id
                    .bytes()
                    .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) })
            );
            assert_eq!(
                marker.event_id,
                crate::event_id::generate_event_id_v1(marker)
            );
            assert_eq!(
                appended[0].batch_id,
                format!(
                    "ft-recorder-migration-readiness-{}",
                    readiness_marker_batch_digest(marker).unwrap()
                )
            );
            assert_eq!(marker.sequence, 100);
            let RecorderEventPayload::LifecycleMarker {
                lifecycle_phase,
                reason,
                details,
            } = &marker.payload
            else {
                panic!("M5 must append a lifecycle readiness marker");
            };
            assert_eq!(
                *lifecycle_phase,
                RecorderLifecyclePhase::MigrationReadyForActivation
            );
            assert_eq!(
                reason.as_deref(),
                Some("migration_target_ready_for_external_activation")
            );
            assert_eq!(details["selector_activated"], false);
        });
    }

    /// The Cx-first readiness path must produce the same proof and marker as
    /// the ambient-Cx wrapper for identical inputs.
    #[test]
    fn m5_mark_ready_with_cx_matches_wrapper() {
        run_async_test(async {
            let target_wrapper = MockMigrationStorage::healthy();
            let target_cx = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = verified_manifest(100, 99, 0xCAFE);
            let epoch_ms = 1_708_000_000_u64;

            let wrapper = engine
                .m5_mark_ready(&target_wrapper, &manifest, epoch_ms, None)
                .await
                .unwrap();

            let cx = crate::cx::for_request();
            let cx_first = engine
                .m5_mark_ready_with_cx(&cx, &target_cx, &manifest, epoch_ms, None)
                .await
                .unwrap();

            assert_eq!(wrapper, cx_first);

            let wrapper_appended = target_wrapper.appended.lock().unwrap();
            let cx_appended = target_cx.appended.lock().unwrap();
            assert_eq!(wrapper_appended.len(), cx_appended.len());
            assert_eq!(wrapper_appended.len(), 1);
            assert_eq!(
                wrapper_appended[0].events.len(),
                cx_appended[0].events.len()
            );

            let wrapper_marker = &wrapper_appended[0].events[0];
            let cx_marker = &cx_appended[0].events[0];
            assert_eq!(wrapper_marker.event_id, cx_marker.event_id);
            assert_eq!(wrapper_marker.sequence, cx_marker.sequence);
            assert_eq!(wrapper_appended[0].batch_id, cx_appended[0].batch_id);
        });
    }

    #[test]
    fn m5_readiness_identity_is_bound_to_verified_manifest_content() {
        run_async_test(async {
            let first_target = MockMigrationStorage::healthy();
            let second_target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let first_manifest = verified_manifest(100, 99, 0xCAFE);
            let second_manifest = verified_manifest(101, 100, 0xBEEF);
            let epoch_ms = 1_708_000_000_u64;

            engine
                .m5_mark_ready(&first_target, &first_manifest, epoch_ms, None)
                .await
                .unwrap();
            engine
                .m5_mark_ready(&second_target, &second_manifest, epoch_ms, None)
                .await
                .unwrap();

            let first = first_target.appended.lock().unwrap();
            let second = second_target.appended.lock().unwrap();
            assert_ne!(first[0].events[0].event_id, second[0].events[0].event_id);
            assert_ne!(first[0].batch_id, second[0].batch_id);
        });
    }

    #[test]
    fn test_m5_result_names_target_without_claiming_selector_activation() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            let result = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap();

            assert_eq!(result.target_backend, RecorderBackendKind::Rusqlite);
        });
    }

    #[test]
    fn test_m5_returns_source_path_hint_without_claiming_retention() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            let result = engine
                .m5_mark_ready(
                    &target,
                    &manifest,
                    1000,
                    Some("/data/events.log".to_string()),
                )
                .await
                .unwrap();

            assert_eq!(
                result.source_path_hint,
                Some("/data/events.log".to_string())
            );
        });
    }

    #[test]
    fn test_m5_rejects_degraded_target_before_marker_io() {
        run_async_test(async {
            let target = MockMigrationStorage::degraded();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            let error = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                MigrationError::TargetDegradedBeforeMarker { .. }
            ));
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_m5_reports_post_marker_degradation_without_readiness_result() {
        run_async_test(async {
            let target = MockMigrationStorage::degrades_after_append();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            let error = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                MigrationError::TargetDegradedAfterMarker { .. }
            ));
            assert_eq!(target.appended.lock().unwrap().len(), 1);
        });
    }

    #[test]
    fn test_m5_write_failure_propagates() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            target.fail_append.store(true, Ordering::Relaxed);
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            let result = engine.m5_mark_ready(&target, &manifest, 1000, None).await;
            assert!(result.is_err());
            let msg = format!("{}", result.unwrap_err());
            assert!(msg.contains("target write error"), "msg: {msg}");
        });
    }

    #[test]
    fn test_cutover_readiness_result_serialize_roundtrip() {
        let result = CutoverReadinessResult {
            target_backend: RecorderBackendKind::Rusqlite,
            migration_epoch_ms: 1708000000,
            marker_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 10,
                ordinal: 4,
            },
            source_path_hint: Some("/data/events.log".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: CutoverReadinessResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, restored);
    }

    #[test]
    fn test_m5_marker_batch_uses_fsync_durability() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap();

            let appended = target.appended.lock().unwrap();
            assert_eq!(appended[0].required_durability, DurabilityLevel::Fsync,);
        });
    }

    #[test]
    fn test_m5_rejects_count_mismatch_before_marker_io() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let mut manifest = verified_manifest(3, 2, 0xCAFE);
            manifest.import_count = 2;

            let error = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                MigrationError::ManifestCountMismatch { .. }
            ));
            assert_eq!(target.health_calls.load(Ordering::Relaxed), 0);
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_m5_rejects_digest_mismatch_before_marker_io() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let mut manifest = verified_manifest(3, 2, 0xCAFE);
            manifest.import_digest = 0xDEAD;

            let error = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                MigrationError::ManifestDigestMismatch { .. }
            ));
            assert_eq!(target.health_calls.load(Ordering::Relaxed), 0);
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_m5_rejects_max_ordinal_before_marker_io() {
        run_async_test(async {
            let target = MockMigrationStorage::healthy();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = verified_manifest(1, u64::MAX, 0xCAFE);

            let error = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap_err();

            assert!(matches!(
                error,
                MigrationError::MarkerOrdinalOverflow { .. }
            ));
            assert_eq!(target.health_calls.load(Ordering::Relaxed), 0);
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn test_m5_rejects_reserved_backend_before_health_or_marker_io() {
        run_async_test(async {
            let target = MockMigrationStorage::reserved();
            let engine = MigrationEngine::new(MigrationConfig::default());
            let manifest = MigrationManifest::default();

            let error = engine
                .m5_mark_ready(&target, &manifest, 1000, None)
                .await
                .unwrap_err();

            assert!(matches!(error, MigrationError::BackendSelection(_)));
            assert_eq!(target.health_calls.load(Ordering::Relaxed), 0);
            assert!(target.appended.lock().unwrap().is_empty());
        });
    }
}
